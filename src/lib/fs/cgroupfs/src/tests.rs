//! The formats and parses, pinned against Linux's.

use alloc::vec::Vec;

use crate::Refusal;
use crate::controllers::{self, Change, Controller, Set, Standing};
use crate::files::{self, Kind};
use crate::name;
use crate::render;
use crate::write::{self, Limit, Target};

fn text(fill: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::new();
    fill(&mut out);
    out
}

#[test]
fn names_follow_mkdir() {
    assert_eq!(name::check(b"system.slice"), Ok(()));
    assert_eq!(name::check(b"getty@ttyS0.service"), Ok(()));
    assert_eq!(name::check(b""), Err(Refusal::Invalid));
    assert_eq!(name::check(b"."), Err(Refusal::Invalid));
    assert_eq!(name::check(b".."), Err(Refusal::Invalid));
    assert_eq!(name::check(b"a/b"), Err(Refusal::Invalid));
    assert_eq!(name::check(b"a\nb"), Err(Refusal::Invalid));
    assert_eq!(name::check(&[b'x'; 255]), Ok(()));
    assert_eq!(name::check(&[b'x'; 256]), Err(Refusal::TooLong));
}

#[test]
fn the_root_lacks_what_linux_keeps_off_it() {
    let root: Vec<&str> = files::of(true, Set::EMPTY).map(|file| file.name).collect();
    assert_eq!(
        root,
        [
            "cgroup.procs",
            "cgroup.threads",
            "cgroup.controllers",
            "cgroup.subtree_control",
            "cgroup.max.descendants",
            "cgroup.max.depth",
            "cgroup.stat",
        ]
    );
    assert_eq!(
        files::of(
            false,
            controllers::ALL
                .iter()
                .fold(Set::EMPTY, |set, c| set.with(*c))
        )
        .count(),
        files::FILES.len()
    );
    assert!(files::named(b"cgroup.kill", true, Set::EMPTY).is_none());
    assert_eq!(
        files::named(b"cgroup.kill", false, Set::EMPTY).map(|file| (file.kind, file.mode())),
        Some((Kind::Kill, 0o200))
    );
    assert_eq!(
        files::named(b"cgroup.events", false, Set::EMPTY).map(files::File::mode),
        Some(0o444)
    );
    assert_eq!(
        files::named(b"cgroup.procs", true, Set::EMPTY).map(files::File::mode),
        Some(0o644)
    );
}

#[test]
fn controllers_print_in_linux_order() {
    let all = controllers::ALL
        .iter()
        .fold(Set::EMPTY, |set, c| set.with(*c));
    assert_eq!(
        text(|out| controllers::render(out, all)),
        b"cpu io memory pids\n"
    );
    let two = Set::EMPTY.with(Controller::Pids).with(Controller::Memory);
    assert_eq!(text(|out| controllers::render(out, two)), b"memory pids\n");
    assert_eq!(text(|out| controllers::render(out, Set::EMPTY)), b"\n");
}

#[test]
fn subtree_control_writes_parse_as_linux_parses_them() {
    let known = Set::EMPTY.with(Controller::Memory).with(Controller::Pids);
    let memory = Set::EMPTY.with(Controller::Memory);
    let pids = Set::EMPTY.with(Controller::Pids);
    assert_eq!(
        controllers::parse_change(b"+memory -pids\n", known),
        Ok(Change {
            enable: memory,
            disable: pids,
        })
    );
    // Empty tokens between repeated spaces are skipped.
    assert_eq!(
        controllers::parse_change(b"  +memory   +pids ", known),
        Ok(Change {
            enable: memory.with(Controller::Pids),
            disable: Set::EMPTY,
        })
    );
    // The later token wins.
    assert_eq!(
        controllers::parse_change(b"+memory -memory", known),
        Ok(Change {
            enable: Set::EMPTY,
            disable: memory,
        })
    );
    assert_eq!(controllers::parse_change(b"", known), Ok(Change::default()));
    // A controller the kernel has not built is not a name it knows.
    assert_eq!(
        controllers::parse_change(b"+cpu", known),
        Err(Refusal::Invalid)
    );
    assert_eq!(
        controllers::parse_change(b"memory", known),
        Err(Refusal::Invalid)
    );
    assert_eq!(
        controllers::parse_change(b"+bogus", known),
        Err(Refusal::Invalid)
    );
    // Split on spaces only: a tab is part of the token.
    assert_eq!(
        controllers::parse_change(b"+memory\t+pids", known),
        Err(Refusal::Invalid)
    );
}

#[test]
fn kstrtoint_takes_base_zero() {
    assert_eq!(write::kstrtoint(b"42"), Ok(42));
    assert_eq!(write::kstrtoint(b"+42"), Ok(42));
    assert_eq!(write::kstrtoint(b"-42"), Ok(-42));
    assert_eq!(write::kstrtoint(b"0"), Ok(0));
    assert_eq!(write::kstrtoint(b"0x1F"), Ok(31));
    assert_eq!(write::kstrtoint(b"017"), Ok(15));
    assert_eq!(write::kstrtoint(b"2147483647"), Ok(i32::MAX));
    assert_eq!(write::kstrtoint(b"-2147483648"), Ok(i32::MIN));
    assert_eq!(write::kstrtoint(b"2147483648"), Err(Refusal::Range));
    assert_eq!(
        write::kstrtoint(b"99999999999999999999999"),
        Err(Refusal::Range)
    );
    assert_eq!(write::kstrtoint(b""), Err(Refusal::Invalid));
    assert_eq!(write::kstrtoint(b"08"), Err(Refusal::Invalid));
    assert_eq!(write::kstrtoint(b"0x"), Err(Refusal::Invalid));
    assert_eq!(write::kstrtoint(b"1 2"), Err(Refusal::Invalid));
    assert_eq!(write::kstrtoint(b"+-1"), Err(Refusal::Invalid));
}

#[test]
fn procs_writes_name_one_process() {
    assert_eq!(write::parse_procs(b"1234\n"), Ok(Target::Pid(1234)));
    assert_eq!(write::parse_procs(b"  7  "), Ok(Target::Pid(7)));
    assert_eq!(write::parse_procs(b"0"), Ok(Target::Writer));
    assert_eq!(write::parse_procs(b"-1"), Err(Refusal::Invalid));
    // Out of range is EINVAL here, not ERANGE: cgroup_procs_write_start
    // answers every kstrtoint failure the same.
    assert_eq!(write::parse_procs(b"4294967296"), Err(Refusal::Invalid));
    assert_eq!(write::parse_procs(b"1 2"), Err(Refusal::Invalid));
    assert_eq!(write::parse_procs(b""), Err(Refusal::Invalid));
}

#[test]
fn kill_takes_one_and_nothing_else() {
    assert_eq!(write::parse_kill(b"1\n"), Ok(()));
    assert_eq!(write::parse_kill(b"0"), Err(Refusal::Range));
    assert_eq!(write::parse_kill(b"2"), Err(Refusal::Range));
    assert_eq!(write::parse_kill(b"yes"), Err(Refusal::Invalid));
}

#[test]
fn limits_take_max_or_a_count() {
    assert_eq!(write::parse_limit(b"max\n"), Ok(Limit::Max));
    assert_eq!(write::parse_limit(b"3"), Ok(Limit::At(3)));
    assert_eq!(write::parse_limit(b"2147483647"), Ok(Limit::Max));
    assert_eq!(write::parse_limit(b"-1"), Err(Refusal::Range));
    assert_eq!(write::parse_limit(b"MAX"), Err(Refusal::Invalid));
    assert_eq!(text(|out| render::limit(out, Limit::Max)), b"max\n");
    assert_eq!(text(|out| render::limit(out, Limit::At(3))), b"3\n");
    assert!(Limit::At(3).allows(3));
    assert!(!Limit::At(3).allows(4));
    assert!(Limit::Max.allows(u32::MAX));
}

#[test]
fn type_takes_only_threaded_and_ferrix_has_none() {
    assert_eq!(write::parse_type(b"threaded\n"), Err(Refusal::NotSupported));
    assert_eq!(write::parse_type(b"domain"), Err(Refusal::Invalid));
    assert_eq!(write::parse_freeze(b"1"), Ok(true));
    assert_eq!(write::parse_freeze(b"0\n"), Ok(false));
    assert_eq!(write::parse_freeze(b"2"), Err(Refusal::Range));
}

#[test]
fn reads_print_as_linux_prints_them() {
    assert_eq!(text(|out| render::ids(out, &[1, 42, 300])), b"1\n42\n300\n");
    assert_eq!(text(|out| render::ids(out, &[])), b"");
    assert_eq!(
        text(|out| render::events(out, true, false)),
        b"populated 1\nfrozen 0\n"
    );
    assert_eq!(
        text(|out| render::stat(out, 5)),
        b"nr_descendants 5\nnr_dying_descendants 0\n"
    );
    assert_eq!(text(|out| render::proc_cgroup(out, [])), b"0::/\n");
    assert_eq!(
        text(|out| render::proc_cgroup(out, [&b"system.slice"[..], b"sshd.service"])),
        b"0::/system.slice/sshd.service\n"
    );
}

#[test]
fn strip_is_the_kernels_strstrip() {
    assert_eq!(write::strip(b" \t\n\x0b\x0c\rx y\r\n"), b"x y");
    assert_eq!(write::strip(b"   "), b"");
    assert_eq!(write::strip(b""), b"");
}

#[test]
fn threaded_controllers_are_cpu_and_pids() {
    let all = controllers::ALL
        .iter()
        .fold(Set::EMPTY, |set, c| set.with(*c));
    assert_eq!(
        all.domain(),
        Set::EMPTY.with(Controller::Io).with(Controller::Memory)
    );
    assert!(Controller::Cpu.threaded() && Controller::Pids.threaded());
}

#[test]
fn no_internal_processes_when_enabling_a_domain_controller() {
    let memory = Set::EMPTY.with(Controller::Memory);
    let pids = Set::EMPTY.with(Controller::Pids);
    let busy = Standing {
        has_tasks: true,
        ..Standing::default()
    };
    // A cgroup with processes may not enable memory for its children...
    assert_eq!(controllers::vet_enable(memory, busy), Err(Refusal::Busy));
    // ...nor may one with processes and memory already on enable pids.
    let with_memory = Standing {
        subtree_control: memory,
        ..busy
    };
    assert_eq!(
        controllers::vet_enable(pids, with_memory),
        Err(Refusal::Busy)
    );
    // A threaded controller may be enabled beside processes while the cgroup
    // could still be a threaded root: no domain controller on, no populated
    // child.
    assert_eq!(controllers::vet_enable(pids, busy), Ok(()));
    let populated_child = Standing {
        populated_children: true,
        ..busy
    };
    assert_eq!(
        controllers::vet_enable(pids, populated_child),
        Err(Refusal::Busy)
    );
    // Nothing new to enable is never refused, and neither is the root.
    assert_eq!(controllers::vet_enable(Set::EMPTY, busy), Ok(()));
    let root = Standing { root: true, ..busy };
    assert_eq!(controllers::vet_enable(memory, root), Ok(()));
    // Without processes of its own, anything may be enabled.
    assert_eq!(controllers::vet_enable(memory, Standing::default()), Ok(()));
}

#[test]
fn no_internal_processes_when_moving_in() {
    let memory = Set::EMPTY.with(Controller::Memory);
    let pids = Set::EMPTY.with(Controller::Pids);
    let enabling = |subtree_control| Standing {
        subtree_control,
        ..Standing::default()
    };
    // Not into a cgroup that enables a domain controller for its children.
    assert_eq!(
        controllers::vet_destination(enabling(memory)),
        Err(Refusal::Busy)
    );
    // Into one enabling only threaded ones while it could be a threaded
    // root, but not once it has a populated child.
    assert_eq!(controllers::vet_destination(enabling(pids)), Ok(()));
    let populated = Standing {
        populated_children: true,
        ..enabling(pids)
    };
    assert_eq!(controllers::vet_destination(populated), Err(Refusal::Busy));
    // The root takes processes whatever it enables, and a cgroup enabling
    // nothing takes them whatever else holds.
    let root = Standing {
        root: true,
        ..enabling(memory)
    };
    assert_eq!(controllers::vet_destination(root), Ok(()));
    let plain = Standing {
        populated_children: true,
        has_tasks: true,
        ..Standing::default()
    };
    assert_eq!(controllers::vet_destination(plain), Ok(()));
}

#[test]
fn a_controllers_files_are_there_only_where_it_is_enabled() {
    let pids = Set::EMPTY.with(Controller::Pids);
    let names = |root, enabled| {
        files::of(root, enabled)
            .map(|file| file.name)
            .collect::<Vec<_>>()
    };
    assert!(
        !names(false, Set::EMPTY)
            .iter()
            .any(|name| name.starts_with("pids."))
    );
    let with_pids = names(false, pids);
    for name in ["pids.max", "pids.current", "pids.events"] {
        assert!(with_pids.contains(&name), "{name} is missing");
    }
    assert!(!with_pids.iter().any(|name| name.starts_with("memory.")));
    // The root has none, whatever its children enable.
    let all = controllers::ALL
        .iter()
        .fold(Set::EMPTY, |set, c| set.with(*c));
    assert!(
        !names(true, all)
            .iter()
            .any(|name| name.contains("max") && !name.starts_with("cgroup."))
    );
    assert_eq!(
        files::named(b"memory.max", false, all).map(|file| (file.kind, file.mode())),
        Some((Kind::MemoryMax, 0o644))
    );
    assert_eq!(
        files::named(b"memory.current", false, all).map(files::File::mode),
        Some(0o444)
    );
    for (name, kind) in [
        ("memory.min", Kind::MemoryMin),
        ("memory.low", Kind::MemoryLow),
        ("memory.high", Kind::MemoryHigh),
    ] {
        assert_eq!(
            files::named(name.as_bytes(), false, all).map(|file| (file.kind, file.mode())),
            Some((kind, 0o644)),
            "{name}"
        );
    }
    assert_eq!(files::named(b"cpu.weight", false, pids), None);
}

#[test]
fn pids_max_reads_as_linux_reads_it() {
    assert_eq!(write::parse_pids_max(b"max\n"), Ok(None));
    assert_eq!(write::parse_pids_max(b"10\n"), Ok(Some(10)));
    assert_eq!(write::parse_pids_max(b"0"), Ok(Some(0)));
    assert_eq!(write::parse_pids_max(b"-0"), Ok(Some(0)));
    assert_eq!(write::parse_pids_max(b"0x10"), Ok(Some(16)));
    assert_eq!(write::parse_pids_max(b"4194304"), Ok(Some(4_194_304)));
    assert_eq!(
        write::parse_pids_max(b"4194305"),
        Err(Refusal::Invalid),
        "PIDS_MAX"
    );
    assert_eq!(write::parse_pids_max(b"-1"), Err(Refusal::Invalid));
    assert_eq!(write::parse_pids_max(b"ten"), Err(Refusal::Invalid));
    assert_eq!(write::parse_pids_max(b""), Err(Refusal::Invalid));
    assert_eq!(
        write::parse_pids_max(b"99999999999999999999999"),
        Err(Refusal::Range)
    );
}

#[test]
fn memory_max_reads_sizes_as_memparse_does() {
    assert_eq!(write::parse_memory_max(b"max"), Ok(None));
    assert_eq!(write::parse_memory_max(b"4096\n"), Ok(Some(4096)));
    assert_eq!(write::parse_memory_max(b"64K"), Ok(Some(64 << 10)));
    assert_eq!(write::parse_memory_max(b"64k"), Ok(Some(64 << 10)));
    assert_eq!(write::parse_memory_max(b"16M"), Ok(Some(16 << 20)));
    assert_eq!(write::parse_memory_max(b"1G"), Ok(Some(1 << 30)));
    assert_eq!(write::parse_memory_max(b"0x1000"), Ok(Some(4096)));
    assert_eq!(write::parse_memory_max(b"0"), Ok(Some(0)));
    assert_eq!(write::parse_memory_max(b"0K"), Ok(Some(0)));
    assert_eq!(write::parse_memory_max(b"16MB"), Err(Refusal::Invalid));
    assert_eq!(write::parse_memory_max(b"M"), Err(Refusal::Invalid));
    assert_eq!(write::parse_memory_max(b"-1"), Err(Refusal::Invalid));
    assert_eq!(write::parse_memory_max(b"0x"), Err(Refusal::Invalid));
    assert_eq!(
        write::parse_memory_max(b"20E"),
        Ok(Some(u64::MAX)),
        "saturates"
    );
}

#[test]
fn cpu_weight_takes_one_to_ten_thousand() {
    assert_eq!(write::parse_weight(b"100\n"), Ok(100));
    assert_eq!(write::parse_weight(b"1"), Ok(1));
    assert_eq!(write::parse_weight(b"10000"), Ok(10_000));
    assert_eq!(write::parse_weight(b"0"), Err(Refusal::Range));
    assert_eq!(write::parse_weight(b"10001"), Err(Refusal::Range));
    assert_eq!(write::parse_weight(b"-5"), Err(Refusal::Invalid));
    assert_eq!(write::parse_weight(b"heavy"), Err(Refusal::Invalid));
}

#[test]
fn the_controller_files_print_as_linux_prints_them() {
    let rendered = |write: fn(&mut Vec<u8>)| text(write);
    assert_eq!(rendered(|out| render::max(out, None)), b"max\n");
    assert_eq!(rendered(|out| render::max(out, Some(16))), b"16\n");
    assert_eq!(rendered(|out| render::number(out, 4096)), b"4096\n");
    assert_eq!(rendered(|out| render::pids_events(out, 3)), b"max 3\n");
    assert_eq!(
        rendered(|out| render::memory_stat(
            out,
            render::MemoryStat {
                file: 4096,
                kernel: 8192,
                shmem: 12288,
                pgscan: 5,
                pgsteal: 4,
                pgfault: 99,
                pgmajfault: 7,
            }
        )),
        b"file 4096\nkernel 8192\nshmem 12288\npgscan 5\npgsteal 4\npgfault 99\npgmajfault 7\n"
    );
    assert_eq!(
        rendered(|out| render::memory_events(
            out,
            render::MemoryEvents {
                low: 0,
                high: 6,
                max: 2,
                oom: 3,
                oom_kill: 1,
            }
        )),
        b"low 0\nhigh 6\nmax 2\noom 3\noom_kill 1\noom_group_kill 0\n"
    );
}
