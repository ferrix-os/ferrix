//! `SystemCallFilter=`, `SystemCallErrorNumber=` and
//! `SystemCallArchitectures=` compiled to a classic BPF program for
//! `seccomp(2)` (`docs/INIT.md` §4.5, L13c).
//!
//! The program is one block per ABI the kernel serves the service through,
//! entered by the entry's `AUDIT_ARCH_*` token, which a filter must check
//! first (`docs/SECCOMP.md` SR1):
//!
//! ```text
//!       ld  [4]                      arch
//!       jeq AUDIT_ARCH_<abi>, 0, 1   per ABI
//!       ja  block_<abi>
//!       ret KILL_PROCESS             an architecture with no block
//! block ld  [0]                      nr
//!       jge 0x40000000, 0, 1         x86-64 only: an x32 number
//!       ret <x32>
//!       jge 0x1000, 0, 2             Ferrix's native range,
//!       jgt 0x1fff, 1, 0             when it differs from the default
//!       ret <native>
//!       jeq <nr>, 0, 1               per call whose action is not the
//!       ret <action>                 default
//!       ret <default>
//! ```
//!
//! An ABI `SystemCallArchitectures=` leaves out is one `ret KILL_PROCESS`.
//! The set of calls is worked out per ABI from the unit's rules in order,
//! each group expanded from systemd's own listing (`tables.rs`, generated),
//! so a later `~` word takes away what an earlier word added, as systemd's
//! merge does. A name the ABI's table lacks is left out of that ABI's block,
//! as libseccomp leaves it.

// Generated, and kept as the generator writes it.
#[rustfmt::skip]
mod tables;

#[cfg(test)]
mod tests;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::kind::{FilterAction, Sandbox};

/// `SECCOMP_RET_KILL_PROCESS`.
pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
/// `SECCOMP_RET_ERRNO`, with the errno in the low 16 bits.
pub const RET_ERRNO: u32 = 0x0005_0000;
/// `SECCOMP_RET_ALLOW`.
pub const RET_ALLOW: u32 = 0x7fff_0000;

/// `BPF_LD | BPF_W | BPF_ABS`.
const LD_W_ABS: u16 = 0x20;
/// `BPF_JMP | BPF_JA`.
const JA: u16 = 0x05;
/// `BPF_JMP | BPF_JEQ | BPF_K`.
const JEQ: u16 = 0x15;
/// `BPF_JMP | BPF_JGT | BPF_K`.
const JGT: u16 = 0x25;
/// `BPF_JMP | BPF_JGE | BPF_K`.
const JGE: u16 = 0x35;
/// `BPF_RET | BPF_K`.
const RET: u16 = 0x06;

/// `offsetof(struct seccomp_data, nr)`.
const NR: u32 = 0;
/// `offsetof(struct seccomp_data, arch)`.
const ARCH: u32 = 4;

/// The bit an x32 call's number carries on x86-64's entry.
const X32_BIT: u32 = 0x4000_0000;
/// Ferrix's native calls (`docs/ARCHITECTURE.md` §2), which pass through a
/// filter like any other number (`docs/SECCOMP.md` SR2).
const NATIVE: (u32, u32) = (0x1000, 0x1fff);

/// Most instructions in one filter: Linux's `BPF_MAXINSNS`.
pub const MAX_INSNS: usize = 4096;

/// The groups init knows beyond systemd's: `@ferrix-native`, Ferrix's
/// native calls, and systemd's `@known`, every call the ABI's table has.
const EXTRA_GROUPS: [&str; 2] = ["ferrix-native", "known"];

/// One `struct sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    /// The opcode.
    pub code: u16,
    /// Jump this far on a true test.
    pub jt: u8,
    /// Jump this far on a false one.
    pub jf: u8,
    /// The constant.
    pub k: u32,
}

const fn insn(code: u16, jt: u8, jf: u8, k: u32) -> Insn {
    Insn { code, jt, jf, k }
}

/// An ABI a call can come through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Abi {
    /// x86-64's `syscall`.
    X86_64,
    /// x86-64's `int $0x80`, with i386's numbers.
    I386,
    /// AArch64's `svc`.
    Aarch64,
    /// ARMv7-A's `svc`, EABI.
    Arm,
}

impl Abi {
    /// Its `AUDIT_ARCH_*` token (`docs/SECCOMP.md` §3.2).
    pub const fn audit_arch(self) -> u32 {
        match self {
            Abi::X86_64 => 0xC000_003E,
            Abi::I386 => 0x4000_0003,
            Abi::Aarch64 => 0xC000_00B7,
            Abi::Arm => 0x4000_0028,
        }
    }

    /// Its name in `SystemCallArchitectures=`.
    pub const fn name(self) -> &'static str {
        match self {
            Abi::X86_64 => "x86-64",
            Abi::I386 => "x86",
            Abi::Aarch64 => "arm64",
            Abi::Arm => "arm",
        }
    }

    fn table(self) -> &'static [(&'static str, u32)] {
        match self {
            Abi::X86_64 => tables::X86_64,
            Abi::I386 => tables::I386,
            Abi::Aarch64 => tables::AARCH64,
            Abi::Arm => tables::ARM,
        }
    }

    /// The number `name` has here.
    pub fn number(self, name: &str) -> Option<u32> {
        let table = self.table();
        table
            .binary_search_by(|(known, _)| (*known).cmp(name))
            .ok()
            .and_then(|at| table.get(at))
            .map(|&(_, number)| number)
    }
}

/// Every ABI, in the order blocks are laid out.
pub const ABIS: [Abi; 4] = [Abi::X86_64, Abi::I386, Abi::Aarch64, Abi::Arm];

/// Whether any ABI's table names the call `name`.
pub fn is_call(name: &str) -> bool {
    ABIS.iter().any(|abi| abi.number(name).is_some())
}

/// Whether `name` (without the `@`) is a group.
pub fn is_group(name: &str) -> bool {
    EXTRA_GROUPS.contains(&name) || group(name).is_some()
}

fn group(name: &str) -> Option<&'static [&'static str]> {
    tables::GROUPS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|&(_, members)| members)
}

/// An errno's number by its name (`EPERM`).
pub fn errno(name: &str) -> Option<u32> {
    tables::ERRNO
        .iter()
        .find(|(known, _)| *known == name)
        .map(|&(_, number)| number)
}

/// What a name in a rule stands for on one ABI.
#[derive(Debug, Default)]
struct Expanded {
    calls: Vec<u32>,
    native: bool,
}

/// `name` (a call, or `@group`) as numbers on `abi`, groups expanded
/// through the groups they name.
fn expand(name: &str, abi: Abi, out: &mut Expanded, depth: u8) {
    let Some(group_name) = name.strip_prefix('@') else {
        out.calls.extend(abi.number(name));
        return;
    };
    match group_name {
        "ferrix-native" => out.native = true,
        "known" => out
            .calls
            .extend(abi.table().iter().map(|&(_, number)| number)),
        _ => {
            // systemd's groups nest two deep at most; the bound only keeps
            // a bad table from looping.
            if depth > 8 {
                return;
            }
            for member in group(group_name).unwrap_or_default() {
                expand(member, abi, out, depth + 1);
            }
        }
    }
}

/// A [`FilterAction`] as a `SECCOMP_RET_*` value.
const fn ret(action: FilterAction) -> u32 {
    match action {
        FilterAction::Kill => RET_KILL_PROCESS,
        FilterAction::Errno(errno) => RET_ERRNO | (errno & 0xffff),
    }
}

/// Which ABIs `SystemCallArchitectures=` allows, of those `served`; every
/// one when it is unset. `native` is the first served ABI.
fn allowed_abis(names: Option<&[String]>, served: &[Abi]) -> Vec<Abi> {
    let Some(names) = names else {
        return served.to_vec();
    };
    served
        .iter()
        .enumerate()
        .filter(|&(at, abi)| {
            names
                .iter()
                .any(|name| name == abi.name() || (name == "native" && at == 0))
        })
        .map(|(_, &abi)| abi)
        .collect()
}

/// The filter `sandbox` asks for, for a service the kernel serves through
/// `served` (the native ABI first), or `None` when it asks for none.
/// `native_calls` is whether the unit `Uses=` or `Offers=` a directory name,
/// whose calls an allow-list then allows (§4.5). Fails when the program
/// would be longer than the kernel takes.
pub fn compile(
    sandbox: &Sandbox,
    served: &[Abi],
    native_calls: bool,
) -> Result<Option<Vec<Insn>>, String> {
    let filter = sandbox.system_call_filter.as_ref();
    let architectures = sandbox.system_call_architectures.as_deref();
    if filter.is_none() && architectures.is_none() {
        return Ok(None);
    }
    let allowed = allowed_abis(architectures, served);
    let allow_list = filter.is_some_and(|filter| filter.allow_list);
    let error = ret(sandbox
        .system_call_error_number
        .unwrap_or(FilterAction::Kill));
    let default = if allow_list { error } else { RET_ALLOW };

    let mut blocks: Vec<Vec<Insn>> = Vec::new();
    for &abi in served {
        if !allowed.contains(&abi) {
            blocks.push(alloc::vec![insn(RET, 0, 0, RET_KILL_PROCESS)]);
            continue;
        }
        // The calls whose action is not the default, and the native range's.
        let mut listed: BTreeMap<u32, u32> = BTreeMap::new();
        let mut native = allow_list && native_calls;
        let mut native_action = RET_ALLOW;
        for rule in filter
            .map(|filter| filter.rules.as_slice())
            .unwrap_or_default()
        {
            let mut expanded = Expanded::default();
            expand(&rule.name, abi, &mut expanded, 0);
            let action = if allow_list {
                RET_ALLOW
            } else {
                rule.action.map_or(error, ret)
            };
            for number in expanded.calls {
                if rule.add {
                    let _ = listed.insert(number, action);
                } else {
                    let _ = listed.remove(&number);
                }
            }
            if expanded.native {
                native = rule.add;
                native_action = action;
            }
        }
        let mut block = alloc::vec![insn(LD_W_ABS, 0, 0, NR)];
        if abi == Abi::X86_64 {
            let x32 = if architectures.is_some() {
                RET_KILL_PROCESS
            } else {
                default
            };
            block.push(insn(JGE, 0, 1, X32_BIT));
            block.push(insn(RET, 0, 0, x32));
        }
        let native_ret = if native { native_action } else { default };
        if native_ret != default {
            block.push(insn(JGE, 0, 2, NATIVE.0));
            block.push(insn(JGT, 1, 0, NATIVE.1));
            block.push(insn(RET, 0, 0, native_ret));
        }
        for (&number, &action) in &listed {
            if action != default {
                block.push(insn(JEQ, 0, 1, number));
                block.push(insn(RET, 0, 0, action));
            }
        }
        block.push(insn(RET, 0, 0, default));
        blocks.push(block);
    }

    let header = 1 + 2 * served.len() + 1;
    let mut program = alloc::vec![insn(LD_W_ABS, 0, 0, ARCH)];
    let mut start = header;
    for (abi, block) in served.iter().zip(&blocks) {
        let here = program.len();
        program.push(insn(JEQ, 0, 1, abi.audit_arch()));
        // `ja` counts from the instruction after it.
        let distance = u32::try_from(start - (here + 2)).map_err(|_| String::from("too long"))?;
        program.push(insn(JA, 0, 0, distance));
        start += block.len();
    }
    program.push(insn(RET, 0, 0, RET_KILL_PROCESS));
    for block in blocks {
        program.extend(block);
    }
    if program.len() > MAX_INSNS {
        return Err(format!(
            "the filter is {} instructions, more than the kernel's {MAX_INSNS}",
            program.len()
        ));
    }
    Ok(Some(program))
}
