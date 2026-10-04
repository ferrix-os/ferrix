//! The compiler's host tests: every program is verified and run by the
//! kernel's own interpreter (`src/lib/kernel/seccomp`), on each ABI's
//! numbers, and the tables are held to `ferrix-linux-abi`'s.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_seccomp::{Insn as Kernel, SeccompData, run, verify};

use super::*;
use crate::kind::{Config, FilterAction, Sandbox};
use crate::source::{Entry, Layer, Source};

/// The sandbox a `[Service]` section with `keys` asks for.
fn sandbox(keys: &str) -> Sandbox {
    let mut source = Source::new();
    let text = alloc::format!("[Service]\nExecStart=/a\n{keys}");
    assert!(
        source
            .add(Layer::Image, "t.service", Entry::File(text.into_bytes()))
            .is_ok()
    );
    let unit = source.load("t.service").unwrap();
    assert!(unit.warnings.is_empty(), "{:?}", unit.warnings.list());
    match unit.config {
        Config::Service(service) => service.sandbox,
        other => panic!("{other:?}"),
    }
}

/// A filter, compiled for `served`, verified by the kernel's verifier.
struct Filter(ferrix_seccomp::Program);

impl Filter {
    fn new(keys: &str, served: &[Abi], native_calls: bool) -> Filter {
        let program = compile(&sandbox(keys), served, native_calls)
            .unwrap()
            .unwrap();
        let raw: Vec<Kernel> = program
            .iter()
            .map(|i| Kernel::new(i.code, i.jt, i.jf, i.k))
            .collect();
        Filter(verify(&raw).unwrap())
    }

    /// What a call numbered `nr` through `arch` is answered.
    fn raw(&self, arch: u32, nr: u32) -> u32 {
        let data = SeccompData {
            nr: nr as i32,
            arch,
            instruction_pointer: 0x40_1000,
            args: [0; 6],
        };
        run(&self.0, &data)
    }

    /// What the call `name` through `abi` is answered.
    fn call(&self, abi: Abi, name: &str) -> u32 {
        let nr = abi
            .number(name)
            .unwrap_or_else(|| panic!("{name} on {abi:?}"));
        self.raw(abi.audit_arch(), nr)
    }
}

const X86: [Abi; 2] = [Abi::X86_64, Abi::I386];
const EPERM: u32 = RET_ERRNO | 1;
const EACCES: u32 = RET_ERRNO | 13;

#[test]
fn nothing_asked_is_no_filter() {
    assert_eq!(compile(&Sandbox::default(), &X86, false), Ok(None));
    assert_eq!(
        compile(&sandbox("NoNewPrivileges=yes\n"), &X86, false),
        Ok(None)
    );
}

#[test]
fn a_deny_list_kills_its_calls_on_every_abi_and_allows_the_rest() {
    let f = Filter::new("SystemCallFilter=~mkdir mkdirat\n", &X86, false);
    for abi in X86 {
        assert_eq!(f.call(abi, "mkdir"), RET_KILL_PROCESS, "{abi:?}");
        assert_eq!(f.call(abi, "mkdirat"), RET_KILL_PROCESS, "{abi:?}");
        assert_eq!(f.call(abi, "read"), RET_ALLOW, "{abi:?}");
        assert_eq!(f.call(abi, "openat"), RET_ALLOW, "{abi:?}");
    }
    // i386's mkdir is 39, which is x86-64's getpid: the arch decides.
    assert_eq!(f.raw(Abi::X86_64.audit_arch(), 39), RET_ALLOW);
    assert_eq!(f.raw(Abi::I386.audit_arch(), 39), RET_KILL_PROCESS);
    // An entry the filter has no block for.
    assert_eq!(f.raw(Abi::Aarch64.audit_arch(), 34), RET_KILL_PROCESS);
    assert_eq!(f.raw(0x1234_5678, 0), RET_KILL_PROCESS);
}

#[test]
fn a_words_errno_and_the_error_number_key_are_its_action() {
    let f = Filter::new("SystemCallFilter=~mkdir:EPERM mkdirat:EPERM\n", &X86, false);
    assert_eq!(f.call(Abi::X86_64, "mkdir"), EPERM);
    assert_eq!(f.call(Abi::I386, "mkdirat"), EPERM);
    let f = Filter::new(
        "SystemCallFilter=~mkdir mkdirat rmdir:kill\nSystemCallErrorNumber=EACCES\n",
        &X86,
        false,
    );
    assert_eq!(f.call(Abi::X86_64, "mkdir"), EACCES);
    assert_eq!(f.call(Abi::X86_64, "rmdir"), RET_KILL_PROCESS);
    assert_eq!(f.call(Abi::X86_64, "read"), RET_ALLOW);
}

#[test]
fn an_allow_list_allows_its_groups_and_kills_the_rest() {
    let f = Filter::new("SystemCallFilter=@system-service\n", &[Abi::Aarch64], false);
    for name in [
        "read",
        "write",
        "mkdirat",
        "execve",
        "exit_group",
        "clone",
        "openat",
    ] {
        assert_eq!(f.call(Abi::Aarch64, name), RET_ALLOW, "{name}");
    }
    for name in [
        "sethostname",
        "reboot",
        "mount",
        "init_module",
        "kexec_load",
    ] {
        assert_eq!(f.call(Abi::Aarch64, name), RET_KILL_PROCESS, "{name}");
    }
    // A later deny takes from the set; the error number is the default.
    let f = Filter::new(
        "SystemCallFilter=@system-service\nSystemCallFilter=~mkdirat\nSystemCallErrorNumber=EPERM\n",
        &[Abi::Arm],
        false,
    );
    assert_eq!(f.call(Abi::Arm, "mkdirat"), EPERM);
    assert_eq!(f.call(Abi::Arm, "sethostname"), EPERM);
    assert_eq!(f.call(Abi::Arm, "read"), RET_ALLOW);
    // ARM's private set_tls is in @default, as systemd lists it.
    assert_eq!(f.call(Abi::Arm, "set_tls"), RET_ALLOW);
}

#[test]
fn the_native_range_follows_the_directory_keys_and_its_group() {
    let process_bootstrap = 0x1033;
    let arch = Abi::X86_64.audit_arch();
    let f = Filter::new("SystemCallFilter=@basic-io\n", &X86, false);
    assert_eq!(f.raw(arch, process_bootstrap), RET_KILL_PROCESS);
    let f = Filter::new("SystemCallFilter=@basic-io\n", &X86, true);
    assert_eq!(f.raw(arch, process_bootstrap), RET_ALLOW);
    assert_eq!(f.raw(arch, 0x2000), RET_KILL_PROCESS);
    let f = Filter::new("SystemCallFilter=@basic-io @ferrix-native\n", &X86, false);
    assert_eq!(f.raw(arch, 0x1000), RET_ALLOW);
    assert_eq!(f.raw(arch, 0x1fff), RET_ALLOW);
    let f = Filter::new("SystemCallFilter=~@ferrix-native:EPERM\n", &X86, false);
    assert_eq!(f.raw(arch, process_bootstrap), EPERM);
    assert_eq!(f.raw(arch, 0), RET_ALLOW);
}

#[test]
fn architectures_kill_every_other_abi_and_x32() {
    let f = Filter::new("SystemCallArchitectures=native\n", &X86, false);
    assert_eq!(f.call(Abi::X86_64, "read"), RET_ALLOW);
    assert_eq!(f.call(Abi::I386, "read"), RET_KILL_PROCESS);
    assert_eq!(
        f.raw(Abi::X86_64.audit_arch(), 0x4000_0000),
        RET_KILL_PROCESS
    );
    let f = Filter::new("SystemCallArchitectures=x86-64 x86\n", &X86, false);
    assert_eq!(f.call(Abi::I386, "read"), RET_ALLOW);
    // Without the key an x32 number gets the default: a deny-list allows
    // it, and the kernel answers ENOSYS, since it dispatches no x32 call.
    let f = Filter::new("SystemCallFilter=~reboot\n", &X86, false);
    assert_eq!(f.raw(Abi::X86_64.audit_arch(), 0x4000_0000), RET_ALLOW);
    let f = Filter::new("SystemCallFilter=read\n", &X86, false);
    assert_eq!(
        f.raw(Abi::X86_64.audit_arch(), 0x4000_0000),
        RET_KILL_PROCESS
    );
}

#[test]
fn the_largest_filter_fits() {
    let all = [Abi::X86_64, Abi::I386, Abi::Aarch64, Abi::Arm];
    let f = Filter::new("SystemCallFilter=~@known\n", &all, false);
    assert_eq!(f.call(Abi::I386, "write"), RET_KILL_PROCESS);
    assert!(f.0.len() < MAX_INSNS, "{}", f.0.len());
}

#[test]
fn parsing_checks_names_against_the_tables() {
    let mut source = Source::new();
    let text = "[Service]\nExecStart=/a\nSystemCallFilter=~frobnicate @known mount:ENOTANERRNO\n\
                SystemCallErrorNumber=EWOULDBLOCK\nSystemCallArchitectures=native vax\n";
    assert!(
        source
            .add(
                Layer::Image,
                "t.service",
                Entry::File(text.as_bytes().to_vec())
            )
            .is_ok()
    );
    let unit = source.load("t.service").unwrap();
    let warnings: Vec<String> = unit
        .warnings
        .list()
        .iter()
        .map(|w| w.message.clone())
        .collect();
    assert_eq!(
        warnings,
        [
            "Failed to parse system call, ignoring: frobnicate",
            "Failed to parse error number, ignoring: mount:ENOTANERRNO",
            "Failed to parse architecture, ignoring: vax",
        ]
    );
    let Config::Service(service) = unit.config else {
        panic!()
    };
    assert_eq!(
        service.sandbox.system_call_error_number,
        Some(FilterAction::Errno(11))
    );
    assert!(service.sandbox.filters());
}

/// `ferrix-linux-abi`'s numbers, as its source writes them: `pub const
/// NAME: usize = N;` inside `pub mod <abi> {`.
fn linux_abi(module: &str) -> Vec<(String, u32)> {
    let text = include_str!("../../../../proto/linux-abi/src/nr.rs");
    let header = alloc::format!("pub mod {module} {{");
    let body = text.split_once(header.as_str()).unwrap().1;
    body.split_once("\n}\n")
        .unwrap()
        .0
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("pub const ")?;
            let (name, value) = rest.split_once(": usize = ")?;
            let value = value.trim_end_matches(';');
            let number = if let Some(hex) = value.strip_prefix("0x") {
                u32::from_str_radix(&hex.replace('_', ""), 16).ok()?
            } else {
                value.replace('_', "").parse().ok()?
            };
            Some((name.to_ascii_lowercase(), number))
        })
        .collect()
}

#[test]
fn the_tables_agree_with_the_kernels_own_numbers() {
    for (module, abi) in [
        ("x86_64", Abi::X86_64),
        ("i386", Abi::I386),
        ("aarch64", Abi::Aarch64),
        ("arm", Abi::Arm),
    ] {
        let theirs = linux_abi(module);
        let mut agreed = 0;
        for (name, number) in &theirs {
            if let Some(ours) = abi.number(name) {
                assert_eq!(ours, *number, "{name} on {abi:?}");
                agreed += 1;
            }
        }
        assert!(
            agreed > 100,
            "{module}: only {agreed} of {} agreed",
            theirs.len()
        );
    }
}

#[test]
fn every_group_member_is_a_call_or_a_group() {
    for (name, members) in tables::GROUPS {
        for member in *members {
            match member.strip_prefix('@') {
                Some(group) => assert!(is_group(group), "{name}: {member}"),
                None => assert!(!member.is_empty()),
            }
        }
    }
    assert!(is_group("system-service") && is_group("known") && is_group("ferrix-native"));
    assert!(!is_group("frobnicate"));
}
