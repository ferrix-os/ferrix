//! Host tests: every format against lines a real Linux printed.
//!
//! The fixtures were copied from `/proc` on a Linux 7.0 x86-64 machine by a
//! script and are literals here, so these tests read nothing from the host
//! they run on. A fixture that says `derived` is not a copy: no 32-bit Linux
//! was at hand, and the expected bytes follow the rule `fs/proc/task_mmu.c`
//! states, which the 64-bit copies confirm.

use alloc::vec::Vec;

use crate::filesystems::{self, Filesystem};
use crate::kstat::{self, CpuTimes, Kstat};
use crate::loadavg::{self, Averages, FIXED_1, Loadavg};
use crate::maps::{self, Mapping, Width};
use crate::meminfo::{self, Meminfo};
use crate::mounts::{self, Mount};
use crate::partitions::{self, Partition};
use crate::stat::{self, Stat};
use crate::status::{self, State, Status};
use crate::sysctl;

fn rendered(render: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::new();
    render(&mut out);
    out
}

fn show(bytes: &[u8]) -> alloc::string::String {
    alloc::string::String::from_utf8_lossy(bytes).into_owned()
}

/// (line as printed, the region it describes)
const HOST_MAPS: &[(&[u8], Mapping<'static>)] = &[
    // text
    (
        b"00422000-007a8000 r-xp 00022000 fc:00 2884149                            /usr/bin/python3.14",
        Mapping {
            start: 0x00422000,
            end: 0x007a8000,
            read: true,
            write: false,
            execute: true,
            shared: false,
            offset: 0x00022000,
            major: 0xfc,
            minor: 0x00,
            inode: 2884149,
            name: Some(b"/usr/bin/python3.14"),
        },
    ),
    // shared
    (
        b"7c87fc8da000-7c87fc8e1000 r--s 00000000 fc:00 2900624                    /usr/lib/x86_64-linux-gnu/gconv/gconv-modules.cache",
        Mapping {
            start: 0x7c87fc8da000,
            end: 0x7c87fc8e1000,
            read: true,
            write: false,
            execute: false,
            shared: true,
            offset: 0x00000000,
            major: 0xfc,
            minor: 0x00,
            inode: 2900624,
            name: Some(b"/usr/lib/x86_64-linux-gnu/gconv/gconv-modules.cache"),
        },
    ),
    // anonymous
    (
        b"00b22000-00b94000 rw-p 00000000 00:00 0 ",
        Mapping {
            start: 0x00b22000,
            end: 0x00b94000,
            read: true,
            write: true,
            execute: false,
            shared: false,
            offset: 0x00000000,
            major: 0x00,
            minor: 0x00,
            inode: 0,
            name: None,
        },
    ),
    // heap
    (
        b"03874000-03a78000 rw-p 00000000 00:00 0                                  [heap]",
        Mapping {
            start: 0x03874000,
            end: 0x03a78000,
            read: true,
            write: true,
            execute: false,
            shared: false,
            offset: 0x00000000,
            major: 0x00,
            minor: 0x00,
            inode: 0,
            name: Some(b"[heap]"),
        },
    ),
    // stack
    (
        b"7fff81e31000-7fff81e53000 rw-p 00000000 00:00 0                          [stack]",
        Mapping {
            start: 0x7fff81e31000,
            end: 0x7fff81e53000,
            read: true,
            write: true,
            execute: false,
            shared: false,
            offset: 0x00000000,
            major: 0x00,
            minor: 0x00,
            inode: 0,
            name: Some(b"[stack]"),
        },
    ),
];

#[test]
fn a_maps_line_is_byte_for_byte_what_linux_printed() {
    for (line, mapping) in HOST_MAPS {
        let out = rendered(|out| maps::render(out, mapping, Width::Bits64));
        let mut expected = line.to_vec();
        expected.push(b'\n');
        assert_eq!(show(&out), show(&expected), "rendering {mapping:?}");
    }
}

#[test]
fn a_maps_line_reads_back_as_the_region_it_describes() {
    for (line, mapping) in HOST_MAPS {
        assert_eq!(
            maps::parse(line).as_ref(),
            Some(mapping),
            "parsing {}",
            show(line)
        );
    }
}

#[test]
fn a_short_address_on_a_64_bit_kernel_pads_to_the_same_column() {
    let mapping = Mapping {
        start: 0x40_0000,
        end: 0x42_2000,
        read: true,
        write: false,
        execute: false,
        shared: false,
        offset: 0,
        major: 0xfc,
        minor: 0,
        inode: 2_884_149,
        name: Some(b"/usr/bin/python3.14"),
    };
    let out = rendered(|out| maps::render(out, &mapping, Width::Bits64));
    assert_eq!(
        show(&out),
        "00400000-00422000 r--p 00000000 fc:00 2884149                            /usr/bin/python3.14\n",
        "copied from the host, where this binary is not position-independent"
    );
}

#[test]
fn a_32_bit_kernel_puts_the_name_at_byte_49() {
    // Derived: 25 + 4 * 6 - 1 = 48 bytes of padding, then a space.
    let mapping = Mapping {
        start: 0x0001_0000,
        end: 0x000a_1000,
        read: true,
        write: false,
        execute: true,
        shared: false,
        offset: 0,
        major: 0,
        minor: 0,
        inode: 0,
        name: Some(b"[stack]"),
    };
    let out = rendered(|out| maps::render(out, &mapping, Width::Bits32));
    assert_eq!(
        show(&out),
        "00010000-000a1000 r-xp 00000000 00:00 0          [stack]\n",
        "the 32-bit column"
    );
    assert_eq!(
        out.iter().position(|&b| b == b'['),
        Some(49),
        "the name column"
    );
}

#[test]
fn a_newline_in_a_mapped_file_name_is_escaped_and_nothing_else_is() {
    let mapping = Mapping {
        name: Some(b"/tmp/a b\nc"),
        ..HOST_MAPS[0].1
    };
    let out = rendered(|out| maps::render(out, &mapping, Width::Bits64));
    assert!(out.ends_with(b" /tmp/a b\\012c\n"), "{}", show(&out));
    assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 1, "one line");
}

#[test]
fn the_maps_parser_refuses_what_the_renderer_cannot_produce() {
    let refused: [&[u8]; 7] = [
        b"",
        b"00400000-00422000 r--p 00000000 fc:00 2884149",
        b"00400000-00422000 r--q 00000000 fc:00 2884149 ",
        b"0040000-00422000 r--p 00000000 fc:00 2884149 ",
        b"00400000-00422000 r--p 00000000 fc:0 2884149 ",
        b"00400000-00422000 r--p 00000000 FC:00 2884149 ",
        b"00400000-00422000 r--p 00000000 fc:00 +2884149 ",
    ];
    for line in refused {
        assert_eq!(maps::parse(line), None, "{}", show(line));
    }
}

#[test]
fn meminfo_is_byte_for_byte_what_linux_printed() {
    let info = Meminfo {
        total: 62103444,
        free: 768736,
        available: 34850072,
        buffers: 2613488,
        cached: 37462144,
        swap_cached: 538760,
        swap_total: 8388604,
        swap_free: 4308344,
        slab: 3343592,
    };
    let out = rendered(|out| meminfo::render(out, &info));
    assert_eq!(show(&out), show(b"MemTotal:       62103444 kB\nMemFree:          768736 kB\nMemAvailable:   34850072 kB\nBuffers:         2613488 kB\nCached:         37462144 kB\nSwapCached:       538760 kB\nSwapTotal:       8388604 kB\nSwapFree:        4308344 kB\nSlab:            3343592 kB\n"), "meminfo");
}

#[test]
fn a_meminfo_value_wider_than_eight_pushes_the_unit_along() {
    let out = rendered(|out| meminfo::line(out, "VmallocTotal", 34359738367));
    assert_eq!(
        show(&out),
        show(b"VmallocTotal:   34359738367 kB\n"),
        "a wide value"
    );
}

#[test]
fn status_is_byte_for_byte_what_linux_printed() {
    let status = Status {
        name: b"python3",
        umask: 0o0002,
        state: State::Running,
        tgid: 457743,
        pid: 457743,
        ppid: 457739,
        uid: [1000; 4],
        gid: [1000; 4],
        fd_size: 64,
        vm_size: 20428,
        vm_locked: 0,
        vm_data: 6724,
        vm_stack: 136,
        threads: 1,
        no_new_privs: false,
        seccomp: 0,
        seccomp_filters: 0,
        cpus: 24,
    };
    let out = rendered(|out| status::render(out, &status));
    // Groups is copied from pid 1, which runs as root with no supplementary
    // groups — and still ends the line with the space `array.c` apologises
    // for.
    assert_eq!(show(&out), show(b"Name:\tpython3\nUmask:\t0002\nState:\tR (running)\nTgid:\t457743\nNgid:\t0\nPid:\t457743\nPPid:\t457739\nTracerPid:\t0\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\nFDSize:\t64\nGroups:\t \nNStgid:\t457743\nNSpid:\t457743\nVmSize:\t   20428 kB\nVmLck:\t       0 kB\nVmData:\t    6724 kB\nVmStk:\t     136 kB\nThreads:\t1\nNoNewPrivs:\t0\nSeccomp:\t0\nSeccomp_filters:\t0\nCpus_allowed:\tffffff\nCpus_allowed_list:\t0-23\n"), "status");
}

#[test]
fn a_threads_status_names_its_process_and_itself() {
    // The same process's second thread, as `task/457750/status` under it
    // reads: its own id in `Pid` and `NSpid`, the process's in `Tgid` and
    // `NStgid`, and the process's thread count.
    let status = Status {
        name: b"python3",
        umask: 0o0002,
        state: State::Sleeping,
        tgid: 457743,
        pid: 457750,
        ppid: 457739,
        uid: [1000; 4],
        gid: [1000; 4],
        fd_size: 64,
        vm_size: 20428,
        vm_locked: 0,
        vm_data: 6724,
        vm_stack: 136,
        threads: 2,
        no_new_privs: true,
        seccomp: 2,
        seccomp_filters: 3,
        cpus: 24,
    };
    let out = rendered(|out| status::render(out, &status));
    assert_eq!(show(&out), show(b"Name:\tpython3\nUmask:\t0002\nState:\tS (sleeping)\nTgid:\t457743\nNgid:\t0\nPid:\t457750\nPPid:\t457739\nTracerPid:\t0\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\nFDSize:\t64\nGroups:\t \nNStgid:\t457743\nNSpid:\t457750\nVmSize:\t   20428 kB\nVmLck:\t       0 kB\nVmData:\t    6724 kB\nVmStk:\t     136 kB\nThreads:\t2\nNoNewPrivs:\t1\nSeccomp:\t2\nSeccomp_filters:\t3\nCpus_allowed:\tffffff\nCpus_allowed_list:\t0-23\n"), "status");
}

#[test]
fn a_status_name_escapes_newline_and_backslash_only() {
    let status = Status {
        name: b"a\\b\nc\td",
        umask: 0o22,
        state: State::Sleeping,
        tgid: 1,
        pid: 1,
        ppid: 0,
        uid: [0; 4],
        gid: [0; 4],
        fd_size: 64,
        vm_size: 0,
        vm_locked: 0,
        vm_data: 0,
        vm_stack: 0,
        threads: 1,
        no_new_privs: false,
        seccomp: 0,
        seccomp_filters: 0,
        cpus: 1,
    };
    let out = rendered(|out| status::render(out, &status));
    assert!(
        out.starts_with(b"Name:\ta\\\\b\\nc\td\nUmask:\t0022\nState:\tS (sleeping)\n"),
        "{}",
        show(&out)
    );
}

#[test]
fn cpu_masks_group_in_32_bit_chunks_and_lists_are_ranges() {
    let cases: [(u32, &str, &str); 6] = [
        (1, "1", "0"),
        (2, "3", "0-1"),
        (4, "f", "0-3"),
        (24, "ffffff", "0-23"),
        (32, "ffffffff", "0-31"),
        (36, "f,ffffffff", "0-35"),
    ];
    for (cpus, mask, list) in cases {
        assert_eq!(
            show(&rendered(|out| status::cpu_mask(out, cpus))),
            mask,
            "mask of {cpus}"
        );
        assert_eq!(
            show(&rendered(|out| status::cpu_list(out, cpus))),
            list,
            "list of {cpus}"
        );
    }
}

fn a_stat() -> Stat<'static> {
    Stat {
        pid: 7,
        comm: b"cat",
        state: State::Running,
        ppid: 1,
        pgrp: 7,
        session: 7,
        tty_nr: 0,
        tpgid: -1,
        flags: 0,
        utime: 3,
        stime: 4,
        priority: 20,
        nice: 0,
        threads: 1,
        start_time: 460_536,
        vsize: 16_637_952,
        rss: 1824,
        rss_limit: u64::MAX,
        start_code: 0x40_0000,
        end_code: 0x42_2000,
        start_stack: 0x7fff_f000,
        pending: 0,
        blocked: 1,
        ignored: 2,
        caught: 4,
        exit_signal: 17,
        processor: 3,
        start_brk: 0x50_0000,
        arg_start: 11,
        arg_end: 12,
        env_start: 13,
        env_end: 14,
    }
}

#[test]
fn stat_has_every_field_in_its_place() {
    let out = show(&rendered(|out| stat::render(out, &a_stat())));
    assert_eq!(
        out,
        "7 (cat) R 1 7 7 0 -1 0 0 0 0 0 3 4 0 0 20 0 1 0 460536 16637952 1824 \
         18446744073709551615 4194304 4333568 2147479552 0 0 0 1 2 4 0 0 0 17 3 0 0 0 0 0 0 0 \
         5242880 11 12 13 14 0\n",
        "stat"
    );
    // As many fields as this machine's Linux printed for itself.
    let after_comm = out.rsplit_once(") ").map(|(_, rest)| rest).unwrap_or("");
    assert_eq!(after_comm.split_whitespace().count() + 2, 52, "field count");
}

#[test]
fn a_parenthesis_in_the_command_name_is_why_readers_find_the_last_one() {
    let odd = Stat {
        comm: b"a) b",
        ..a_stat()
    };
    let out = show(&rendered(|out| stat::render(out, &odd)));
    assert!(out.starts_with("7 (a) b) R 1 "), "{out}");
}

#[test]
fn a_mounts_line_is_what_linux_printed_and_escapes_what_would_split_it() {
    let host = Mount {
        source: b"proc",
        point: b"/proc",
        fstype: b"proc",
        options: b"rw,nosuid,nodev,noexec,relatime",
    };
    let out = rendered(|out| mounts::render(out, &host));
    assert_eq!(
        show(&out),
        show(b"proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n"),
        "the host's /proc"
    );

    let awkward = Mount {
        source: b"tmpfs",
        point: b"/mnt/a b\\c\td\ne",
        fstype: b"tmpfs",
        options: b"rw",
    };
    let out = rendered(|out| mounts::render(out, &awkward));
    assert_eq!(
        show(&out),
        "tmpfs /mnt/a\\040b\\134c\\011d\\012e tmpfs rw 0 0\n",
        "escapes"
    );
}

#[test]
fn a_devtmpfs_mounts_line_is_what_linux_printed() {
    // The host's `/dev`, mounted by its initramfs with the source `udev`.
    let host = Mount {
        source: b"udev",
        point: b"/dev",
        fstype: b"devtmpfs",
        options: b"rw,nosuid,relatime,size=27067912k,nr_inodes=6766978,mode=755,inode64",
    };
    let out = rendered(|out| mounts::render(out, &host));
    assert_eq!(
        show(&out),
        show(
            b"udev /dev devtmpfs rw,nosuid,relatime,size=27067912k,nr_inodes=6766978,mode=755,inode64 0 0\n"
        ),
        "the host's /dev"
    );

    // What `mount -t devtmpfs devtmpfs /tmp/d` shows here: the source is the
    // type's name, and the options are the ones the kernel enforces.
    let here = Mount {
        source: b"devtmpfs",
        point: b"/tmp/d",
        fstype: b"devtmpfs",
        options: b"rw",
    };
    let out = rendered(|out| mounts::render(out, &here));
    assert_eq!(show(&out), "devtmpfs /tmp/d devtmpfs rw 0 0\n", "here");
}

#[test]
fn mountinfo_lines_are_what_linux_printed() {
    // Two lines of the host's own `/proc/self/mountinfo` (Linux 7.0), less
    // the `shared:` tags, which no mount here ever has.
    let proc = mounts::MountInfo {
        id: 28,
        parent: 34,
        device: 25,
        root: b"/",
        point: b"/proc",
        options: b"rw,nosuid,nodev,noexec,relatime",
        fstype: b"proc",
        source: b"proc",
        super_options: b"rw",
    };
    let out = rendered(|out| mounts::render_info(out, &proc));
    assert_eq!(
        show(&out),
        show(b"28 34 0:25 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n"),
        "the host's /proc"
    );
    let dev = mounts::MountInfo {
        id: 29,
        parent: 34,
        device: 7,
        root: b"/",
        point: b"/dev",
        options: b"rw,nosuid,relatime",
        fstype: b"devtmpfs",
        source: b"udev",
        super_options: b"rw,size=27073364k,nr_inodes=6768341,mode=755,inode64",
    };
    let out = rendered(|out| mounts::render_info(out, &dev));
    assert_eq!(
        show(&out),
        show(
            b"29 34 0:7 / /dev rw,nosuid,relatime - devtmpfs udev rw,size=27073364k,nr_inodes=6768341,mode=755,inode64\n"
        ),
        "the host's /dev"
    );

    // A large device number splits as `makedev` packs it, and a bind's root
    // and a point with a space in it are escaped.
    let awkward = mounts::MountInfo {
        id: 7,
        parent: 1,
        // `makedev(74565, 490565)`, as glibc packs it.
        device: ((74_565_u64 & 0xfff) << 8)
            | ((74_565_u64 & !0xfff) << 32)
            | (490_565_u64 & 0xff)
            | ((490_565_u64 & !0xff) << 12),
        root: b"/etc dir",
        point: b"/new root/etc",
        options: b"ro,nosuid,nodev",
        fstype: b"tmpfs",
        source: b"tmpfs",
        super_options: b"rw",
    };
    let out = rendered(|out| mounts::render_info(out, &awkward));
    assert_eq!(
        show(&out),
        "7 1 74565:490565 /etc\\040dir /new\\040root/etc ro,nosuid,nodev - tmpfs tmpfs rw\n",
        "escapes and a wide device"
    );
}

#[test]
fn filesystems_lines_are_what_linux_printed() {
    // Three lines of the host's `/proc/filesystems`, in its order, and one
    // for a filesystem that needs a block device.
    let types = [
        Filesystem {
            name: b"tmpfs",
            nodev: true,
        },
        Filesystem {
            name: b"proc",
            nodev: true,
        },
        Filesystem {
            name: b"devtmpfs",
            nodev: true,
        },
        Filesystem {
            name: b"ext4",
            nodev: false,
        },
    ];
    let out = rendered(|out| {
        for filesystem in &types {
            filesystems::render(out, filesystem);
        }
    });
    assert_eq!(
        show(&out),
        show(b"nodev\ttmpfs\nnodev\tproc\nnodev\tdevtmpfs\n\text4\n"),
        "the host's /proc/filesystems"
    );
}

/// A host's `/proc/stat` with `cpu2` to `cpu23` left out, and the interrupt
/// counts after the thirty-eighth: every line kept is as printed, and the
/// `intr` line is what Linux prints for a machine with that many interrupts.
const HOST_KSTAT: &str = "cpu  9076855 270647 1436233 94650326 308715 0 31227 0 169401 437\n\
cpu0 425646 12091 77968 3850347 14446 0 12100 0 11352 6\n\
cpu1 657376 15596 82034 3625601 13139 0 3049 0 5940 3\n\
intr 1540466352 127 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 1 3291 1 1 1 1 0 0 0 1\n\
ctxt 3405985925\n\
btime 1789246016\n\
processes 2384717\n\
procs_running 6\n\
procs_blocked 0\n\
softirq 256823522 47856483 13436684 9923 12532756 236740 0 353061 83875241 911 98521723\n";

/// The host's per-interrupt counts, as numbers.
fn host_interrupts() -> Vec<u64> {
    HOST_KSTAT
        .lines()
        .find_map(|line| line.strip_prefix("intr "))
        .unwrap_or("")
        .split(' ')
        .skip(1)
        .map(|word| word.parse().unwrap_or(u64::MAX))
        .collect()
}

fn host_cpus() -> [(u32, CpuTimes); 2] {
    [
        (
            0,
            CpuTimes::from_fields([425646, 12091, 77968, 3850347, 14446, 0, 12100, 0, 11352, 6]),
        ),
        (
            1,
            CpuTimes::from_fields([657376, 15596, 82034, 3625601, 13139, 0, 3049, 0, 5940, 3]),
        ),
    ]
}

fn a_host_kstat<'a>(cpus: &'a [(u32, CpuTimes)], interrupts: &'a [u64]) -> Kstat<'a> {
    Kstat {
        total: CpuTimes::from_fields([
            9076855, 270647, 1436233, 94650326, 308715, 0, 31227, 0, 169401, 437,
        ]),
        cpus,
        interrupts: 1540466352,
        per_interrupt: interrupts,
        context_switches: 3405985925,
        boot_time: 1789246016,
        processes: 2384717,
        running: 6,
        blocked: 0,
        softirqs: 256823522,
        per_softirq: [
            47856483, 13436684, 9923, 12532756, 236740, 0, 353061, 83875241, 911, 98521723,
        ],
    }
}

#[test]
fn kstat_is_byte_for_byte_what_linux_printed() {
    let cpus = host_cpus();
    let interrupts = host_interrupts();
    assert_eq!(interrupts.len(), 38, "the fixture's interrupt counts");
    let out = rendered(|out| kstat::render(out, &a_host_kstat(&cpus, &interrupts)));
    assert_eq!(show(&out), HOST_KSTAT);
}

#[test]
fn kstat_reads_back_as_what_it_was_given() {
    let cpus = host_cpus();
    let parsed = kstat::parse(HOST_KSTAT.as_bytes());
    let parsed = parsed.as_ref();
    assert_eq!(parsed.map(|p| p.cpus.as_slice()), Some(cpus.as_slice()));
    assert_eq!(parsed.map(|p| p.total.idle), Some(94650326));
    assert_eq!(parsed.map(|p| p.interrupts), Some(1540466352));
    assert_eq!(parsed.map(|p| p.context_switches), Some(3405985925));
    assert_eq!(parsed.map(|p| p.boot_time), Some(1789246016));
    assert_eq!(parsed.map(|p| p.processes), Some(2384717));
    assert_eq!(parsed.map(|p| p.running), Some(6));
    assert_eq!(parsed.map(|p| p.blocked), Some(0));

    // With no per-interrupt counts, which is what a kernel that keeps none
    // prints: the total alone, and still a file that reads back.
    let bare = Kstat {
        per_interrupt: &[],
        per_softirq: [0; kstat::SOFTIRQS],
        softirqs: 0,
        ..a_host_kstat(&cpus, &[])
    };
    let out = show(&rendered(|out| kstat::render(out, &bare)));
    assert!(
        out.contains("\nintr 1540466352\nctxt 3405985925\n"),
        "{out}"
    );
    assert!(out.ends_with("\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n"), "{out}");
    assert_eq!(
        kstat::parse(out.as_bytes()).map(|p| p.interrupts),
        Some(1540466352)
    );
}

#[test]
fn the_kstat_parser_refuses_what_the_renderer_cannot_produce() {
    let refused = [
        (
            "one space after cpu",
            HOST_KSTAT.replacen("cpu  ", "cpu ", 1),
        ),
        (
            "nine values",
            HOST_KSTAT.replacen(" 11352 6\n", " 11352\n", 1),
        ),
        (
            "eleven values",
            HOST_KSTAT.replacen(" 11352 6\n", " 11352 6 7\n", 1),
        ),
        ("a repeated label", HOST_KSTAT.replacen("ctxt", "btime", 1)),
        ("an unknown label", HOST_KSTAT.replacen("ctxt", "cxtt", 1)),
        (
            "no softirq line",
            HOST_KSTAT.replacen("softirq", "procs_blocked", 1),
        ),
        (
            "a word for a number",
            HOST_KSTAT.replacen("processes 2384717", "processes many", 1),
        ),
        (
            "a counter with two",
            HOST_KSTAT.replacen("ctxt 3405985925", "ctxt 3405 985925", 1),
        ),
        ("no total", HOST_KSTAT.replacen("cpu  ", "cpu9 ", 1)),
    ];
    for (why, text) in refused {
        assert_ne!(text, HOST_KSTAT, "{why}: the fixture did not change");
        assert_eq!(kstat::parse(text.as_bytes()), None, "{why}");
    }
}

#[test]
fn partitions_is_the_header_and_a_row_per_device_as_linux_printed() {
    let host = [
        Partition {
            major: 7,
            minor: 0,
            blocks: 4,
            name: b"loop0",
        },
        Partition {
            major: 7,
            minor: 1,
            blocks: 68452,
            name: b"loop1",
        },
    ];
    let out = rendered(|out| partitions::render(out, &host));
    assert_eq!(
        show(&out),
        show(
            b"major minor  #blocks  name\n\n   7        0          4 loop0\n   7        1      68452 loop1\n"
        ),
        "the host's /proc/partitions"
    );
}

#[test]
fn partitions_with_one_device_is_the_header_the_blank_line_and_its_row() {
    let host = [Partition {
        major: 7,
        minor: 0,
        blocks: 4,
        name: b"loop0",
    }];
    let out = rendered(|out| partitions::render(out, &host));
    assert_eq!(
        show(&out),
        "major minor  #blocks  name\n\n   7        0          4 loop0\n"
    );
}

#[test]
fn partitions_with_no_devices_is_empty() {
    let out = rendered(|out| partitions::render(out, &[]));
    assert_eq!(show(&out), "");
}

#[test]
fn a_sysctl_value_is_what_linux_printed() {
    let cases: [(Vec<u8>, &[u8]); 5] = [
        (rendered(|out| sysctl::string(out, b"Linux")), b"Linux\n"),
        (
            rendered(|out| sysctl::string(out, b"7.0.0-29-generic")),
            b"7.0.0-29-generic\n",
        ),
        (rendered(|out| sysctl::string(out, b"(none)")), b"(none)\n"),
        (rendered(|out| sysctl::number(out, 4_194_304)), b"4194304\n"),
        (
            rendered(|out| sysctl::number(out, i64::MAX as u64)),
            b"9223372036854775807\n",
        ),
    ];
    for (out, host) in cases {
        assert_eq!(show(&out), show(host));
    }
}

/// Derived: the rule `_proc_do_string` states, for a write from offset 0.
#[test]
fn a_string_write_stops_at_a_newline_or_nul_and_is_cut_at_the_limit() {
    assert_eq!(sysctl::stored(b"name\n", 64), b"name");
    assert_eq!(sysctl::stored(b"name", 64), b"name");
    assert_eq!(sysctl::stored(b"one\ntwo\n", 64), b"one");
    assert_eq!(sysctl::stored(b"a\0b", 64), b"a");
    assert_eq!(sysctl::stored(b"\n", 64), b"");
    let long = [b'x'; 70];
    assert_eq!(sysctl::stored(&long, 64), &long[..64]);
}

// ---------------------------------------------------------------------------
// /proc/net
//
// Every expected line below was taken from a running Linux and pasted here.
// ---------------------------------------------------------------------------

/// The text a renderer wrote, as a string.
fn net_text(body: impl FnOnce(&mut Vec<u8>)) -> alloc::string::String {
    show(&rendered(body))
}

#[test]
fn proc_net_dev_is_the_two_headers_and_a_row_per_interface() {
    use crate::net::{Device, DeviceCounters, dev};
    let text = net_text(|out| {
        dev(
            out,
            &[Device {
                name: b"lo",
                counters: DeviceCounters {
                    received_bytes: 42_232_032,
                    received: 230_764,
                    sent_bytes: 42_232_032,
                    sent: 230_764,
                    ..DeviceCounters::default()
                },
            }],
        );
    });
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("Inter-|   Receive                                                |  Transmit")
    );
    assert_eq!(
        lines.next(),
        Some(
            " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed"
        )
    );
    assert_eq!(
        lines.next(),
        Some(
            "    lo: 42232032  230764    0    0    0     0          0         0 42232032  230764    0    0    0     0       0          0"
        )
    );
    assert_eq!(lines.next(), None);
}

#[test]
fn proc_net_route_prints_its_addresses_in_host_order() {
    use crate::net::{Route, route};
    let text = net_text(|out| {
        route(
            out,
            &[Route {
                interface: b"eno2",
                destination: [0, 0, 0, 0],
                gateway: [192, 168, 8, 1],
                flags: 0x0003,
                metric: 100,
                mask: [0, 0, 0, 0],
                mtu: 0,
            }],
        );
    });
    let mut lines = text.lines();
    let header = lines.next().expect("a header");
    assert!(header.starts_with("Iface\tDestination\tGateway \tFlags"));
    assert_eq!(header.len(), 127, "every line is padded to 127 characters");
    let row = lines.next().expect("a row");
    assert!(
        row.starts_with("eno2\t00000000\t0108A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0"),
        "the row reads {row:?}"
    );
    assert_eq!(row.len(), 127);
}

#[test]
fn a_gateway_is_the_network_order_bytes_read_as_a_host_order_number() {
    use crate::net::hex_v4;
    // 192.168.8.1 is 0x0108A8C0 in these files, which is the reverse of what
    // it looks like it should be, and is what every reader expects.
    assert_eq!(hex_v4([192, 168, 8, 1]), 0x0108_A8C0);
    assert_eq!(hex_v4([127, 0, 0, 1]), 0x0100_007F);
    assert_eq!(hex_v4([0, 0, 0, 0]), 0);
    assert_eq!(hex_v4([255, 255, 255, 0]), 0x00FF_FFFF);
}

#[test]
fn proc_net_tcp_is_the_line_linux_prints() {
    use crate::net::{Endpoint, Socket, tcp};
    let text = net_text(|out| {
        tcp(
            out,
            &[Socket {
                slot: 0,
                local: Endpoint::V4([192, 168, 122, 1], 53),
                remote: Endpoint::V4([0, 0, 0, 0], 0),
                state: 10,
                transmit_queue: 0,
                receive_queue: 0,
                uid: 0,
                inode: 12_183,
            }],
        );
    });
    let mut lines = text.lines();
    let header = lines.next().expect("a header");
    assert!(header.starts_with("  sl  local_address rem_address   st tx_queue rx_queue"));
    assert_eq!(header.len(), 149, "tcp pads to Linux's TMPSZ - 1");
    let row = lines.next().expect("a row");
    assert_eq!(
        row.trim_end(),
        "   0: 017AA8C0:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12183 1 0000000000000000 100 0 0 10 0"
    );
    assert_eq!(row.len(), 149);
}

#[test]
fn an_ipv6_socket_prints_four_words_of_eight_digits() {
    use crate::net::{Endpoint, Socket, tcp};
    let loopback = [0_u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    let text = net_text(|out| {
        tcp(
            out,
            &[Socket {
                slot: 3,
                local: Endpoint::V6(loopback, 0x1F90),
                remote: Endpoint::V6([0; 16], 0),
                state: 1,
                transmit_queue: 0,
                receive_queue: 0,
                uid: 0,
                inode: 1,
            }],
        );
    });
    let row = text.lines().nth(1).expect("a row");
    assert!(
        row.starts_with("   3: 00000000000000000000000001000000:1F90 "),
        "the row reads {row:?}"
    );
}

#[test]
fn proc_net_udp_has_a_wider_slot_column_than_tcp() {
    use crate::net::{Endpoint, Socket, udp};
    let text = net_text(|out| {
        udp(
            out,
            &[Socket {
                slot: 4_616,
                local: Endpoint::V4([0, 0, 0, 0], 0x14E9),
                remote: Endpoint::V4([0, 0, 0, 0], 0),
                state: 7,
                transmit_queue: 0,
                receive_queue: 0,
                uid: 119,
                inode: 18_207,
            }],
        );
    });
    let row = text.lines().nth(1).expect("a row");
    assert!(
        row.starts_with(
            " 4616: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000   119        0 18207 2 "
        ),
        "the row reads {row:?}"
    );
}

#[test]
fn proc_net_arp_is_the_line_arp_reads() {
    use crate::net::{Neighbour, arp};
    let text = net_text(|out| {
        arp(
            out,
            &[Neighbour {
                address: [172, 18, 0, 4],
                flags: 2,
                hardware: [0xA2, 0xCE, 0xDE, 0xA5, 0xF8, 0x63],
                interface: b"br-d4b21d68bf63",
            }],
        );
    });
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("IP address       HW type     Flags       HW address            Mask     Device")
    );
    assert_eq!(
        lines.next(),
        Some(
            "172.18.0.4       0x1         0x2         a2:ce:de:a5:f8:63     *        br-d4b21d68bf63"
        )
    );
}

#[test]
fn a_file_with_no_rows_is_its_header_and_nothing_else() {
    use crate::net::{Device, Neighbour, Route, Socket, arp, dev, route, tcp, udp};
    let no_devices: &[Device<'_>] = &[];
    let no_routes: &[Route<'_>] = &[];
    let no_sockets: &[Socket] = &[];
    let no_neighbours: &[Neighbour<'_>] = &[];
    assert_eq!(net_text(|out| dev(out, no_devices)).lines().count(), 2);
    assert_eq!(net_text(|out| route(out, no_routes)).lines().count(), 1);
    assert_eq!(net_text(|out| tcp(out, no_sockets)).lines().count(), 1);
    assert_eq!(net_text(|out| udp(out, no_sockets)).lines().count(), 1);
    assert_eq!(net_text(|out| arp(out, no_neighbours)).lines().count(), 1);
}

/// `fs/proc/loadavg.c`'s line, derived: the averages are the ones btop showed
/// for example on 2026-09-27, and the fixed-point loads are the ones that
/// print as them.
#[test]
fn loadavg_is_linuxs_line() {
    let line = b"46.49 41.04 37.76 3/2104 3474930\n";
    // 46.49 is 95211/2048 once `hundredths` adds its rounding.
    let loads = [95_211, 84_050, 77_332];
    let got = rendered(|out| {
        loadavg::render(
            out,
            &Loadavg {
                loads,
                running: 3,
                total: 2104,
                last: 3_474_930,
            },
        );
    });
    assert_eq!(show(&got), show(line));
}

/// One task always runnable is a load of one, the one-minute average
/// reaching `1 - 1/e` after a minute; an idle machine decays to nothing.
#[test]
fn loadavg_folds_as_linux_does() {
    let mut averages = Averages::START;
    let minute = 60 * 1_000_000_000;
    averages.advance(minute, 1);
    assert_eq!(loadavg::hundredths(averages.loads[0]), (0, 63));
    averages.advance(3600 * 1_000_000_000, 1);
    assert!(
        averages
            .loads
            .iter()
            .all(|&load| loadavg::hundredths(load) == (1, 0))
    );
    // Nothing is due before the next five seconds: no fold, no change.
    let before = averages;
    averages.advance(averages.due - 1, 9);
    assert_eq!(averages, before);
    averages.advance(3 * 3600 * 1_000_000_000, 0);
    assert_eq!(averages.loads, [0; 3]);
    assert_eq!(loadavg::hundredths(FIXED_1 * 3 / 2), (1, 50));
}
