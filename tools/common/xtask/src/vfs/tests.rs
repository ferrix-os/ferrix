use super::*;

fn owned(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

/// Every command the kernel runs, the criterion's and then the applets.
fn every_command() -> impl Iterator<Item = &'static Command> {
    COMMANDS.iter().chain(APPLETS).chain(SHELL).chain(UTILITIES)
}

/// The log a kernel that passes every command would write.
fn passing_log() -> Vec<String> {
    let mut log = owned(&["FERRIX-BOOT-OK stages 1-12"]);
    for (index, command) in every_command().enumerate() {
        log.push(format!("  init     command {index}: {}", command.argv[0]));
        match command.expect {
            Expect::Lines(lines) => log.extend(owned(lines)),
            Expect::Shaped(shapes) => log.extend(
                shapes
                    .iter()
                    .map(|shape| shape.replace('#', "1").replace('*', "x")),
            ),
            Expect::Nothing => {}
            Expect::ProcListing => {
                log.extend(owned(&[
                    "/proc:", "1", "self", "", "/proc/1:", "maps", "exe",
                ]));
            }
            Expect::Maps => log.extend(owned(&[
                "00400000-00401000 r--p 00000000 00:00 0",
                "7fffffdde000-7fffffdff000 rw-p 00000000 00:00 0          [stack]",
            ])),
        }
        log.push(format!(
            "  init     command {index} exited with {}",
            command.status
        ));
    }
    log.push("  init     every command has run".to_owned());
    log
}

#[test]
fn the_list_encodes_as_nul_terminated_arguments_and_empty_ones_between_commands() {
    let commands = [
        Command {
            argv: &["ls", "-R"],
            status: 0,
            expect: Expect::Lines(&[]),
        },
        Command {
            argv: &["sh", "-c", "echo a\necho b"],
            status: 0,
            expect: Expect::Lines(&[]),
        },
    ];
    assert_eq!(
        encode(&commands).unwrap(),
        b"ls\0-R\0\0sh\0-c\0echo a\necho b\0\0"
    );
    assert!(encode(COMMANDS).unwrap().ends_with(b"\0\0"));
    assert!(encode(APPLETS).unwrap().ends_with(b"\0\0"));
}

#[test]
fn the_list_refuses_what_its_encoding_cannot_carry() {
    for argv in [&[][..], &["sh", ""][..], &["sh", "a\0b"][..]] {
        let command = Command {
            argv,
            status: 0,
            expect: Expect::Lines(&[]),
        };
        assert!(encode(&[command]).is_err(), "{argv:?} must be refused");
    }
}

#[test]
fn every_program_is_a_name_the_initramfs_links() {
    for command in every_command() {
        let name = command.argv[0];
        assert!(
            crate::initramfs::APPLETS.contains(&name)
                || crate::initramfs::ZINC_NAMES.contains(&name),
            "{name} is not linked in /bin"
        );
    }
}

#[test]
fn no_script_exits_zero_and_no_expectation_is_empty() {
    for command in every_command() {
        if command.argv[0] == "sh" {
            assert_ne!(command.status, 0, "a dead shell reports 0");
        }
        if let Expect::Lines(lines) | Expect::Shaped(lines) = command.expect {
            assert!(!lines.is_empty(), "{:?} expects no lines", command.argv);
        }
    }
}

#[test]
fn shapes_stand_hashes_for_numbers_and_stars_for_any_text() {
    assert!(shaped("#: /tmp", "60: /tmp"));
    assert!(!shaped("#: /tmp", ": /tmp"), "a number has a digit");
    assert!(!shaped("#: /tmp", "6x: /tmp"));
    assert!(
        !shaped("#: /tmp", "60: /tmp/vfs"),
        "a shape matches the whole line"
    );
    assert!(shaped("kernel.pid_max = #", "kernel.pid_max = 32768"));
    assert!(!shaped("kernel.pid_max = #", "kernel.pid_max = "));
    assert!(shaped("* all *", "00:00:05     all   18.26    0.00"));
    assert!(!shaped("* all *", "00:00:05     CPU    %usr"));
    assert!(
        shaped("Load average:*", "Load average:"),
        "a star can be empty"
    );
    assert!(shaped("plain", "plain"));
    assert!(!shaped("plain", "plainer"));
}

#[test]
fn nothing_allows_blank_lines_and_no_more() {
    assert_eq!(nothing(&[]), Ok(()));
    assert_eq!(nothing(&owned(&["", "  "])), Ok(()));
    assert!(nothing(&owned(&["", "major minor  #blocks  name"])).is_err());
}

#[test]
fn a_one_line_script_is_named_by_its_text() {
    let short = Command {
        argv: &["sh", "-c", "pwdx $$; exit 3"],
        status: 3,
        expect: Expect::Nothing,
    };
    assert_eq!(name(&short), "sh -c 'pwdx $$; exit 3'");
    let long = Command {
        argv: &[
            "sh",
            "-c",
            "echo 0123456789 0123456789 0123456789 0123456789 0123456789 0123456789 \
             0123456789 0123456789; exit 3",
        ],
        status: 3,
        expect: Expect::Nothing,
    };
    let named = name(&long);
    assert!(named.ends_with(" 012345678...'"), "{named}");
    assert_eq!(named.len(), "sh -c ''...".len() + NAMED_SCRIPT);
    assert_eq!(
        name(&COMMANDS[2]),
        "sh -c",
        "a script of many lines is not quoted"
    );
    assert_eq!(name(&COMMANDS[0]), "ls -R");
}

/// What the applets printed on `x86_64`, from `cargo xtask test-vfs`: the
/// output the shapes were written against.
const APPLETS_ON_X86_64: &[(usize, &[&str])] = &[
    (3, &["60: /tmp", "/"]),
    (
        4,
        &["sysctl: error setting key 'kernel.ostype': Permission denied"],
    ),
    (5, &["kernel.ostype = Ferrix"]),
    (6, &["kernel.pid_max = 32768"]),
    (
        7,
        &[
            "kernel.hostname = applets",
            "applets",
            "kernel.hostname = ferrix",
            "hostname: restored",
        ],
    ),
    (8, &["140000"]),
    (
        9,
        &[
            "major minor  #blocks  name",
            "",
            " 254        0      65536 vda",
            " 254       16     131072 vdb",
        ],
    ),
    (
        10,
        &[
            "Disk /dev/vda: 64 MB, 67108864 bytes, 131072 sectors",
            "8 cylinders, 255 heads, 63 sectors/track",
            "Units: sectors of 1 * 512 = 512 bytes",
            "",
            "Disk /dev/vda doesn't contain a valid partition table",
            "Disk /dev/vdb: 128 MB, 134217728 bytes, 262144 sectors",
            "16 cylinders, 255 heads, 63 sectors/track",
            "Units: sectors of 1 * 512 = 512 bytes",
            "",
            "Disk /dev/vdb doesn't contain a valid partition table",
            "Disk /dev/vdc: 128 MB, 134217728 bytes, 262144 sectors",
            "16 cylinders, 255 heads, 63 sectors/track",
            "Units: sectors of 1 * 512 = 512 bytes",
            "",
            "Disk /dev/vdc doesn't contain a valid partition table",
        ],
    ),
    (
        11,
        &[
            "Mem: 3964K used, 507124K free, 0K shrd, 0K buff, 0K cached",
            "CPU:   2% usr   0% sys   0% nic  97% idle   0% io   0% irq   0% sirq",
            "Load average: ",
            "  PID  PPID USER     STAT   VSZ %VSZ CPU %CPU COMMAND",
            "   74     1 root     R    10260   2%   2   0% {busybox} top -b -n1",
            "",
        ],
    ),
    (
        12,
        &[
            "Linux 6.1.0-ferrix (ferrix)\t01/01/70\t_x86_64_\t(4 CPU)",
            "",
            "00:00:05     CPU    %usr   %nice    %sys %iowait    %irq   %soft  %steal  \
             %guest   %idle",
            "00:00:05     all   18.26    0.00    0.00    0.00    0.00    0.00    0.00    \
             0.00   81.74",
        ],
    ),
    (
        13,
        &[
            "Linux 6.1.0-ferrix (ferrix) \t01/01/70 \t_x86_64_\t(4 CPU)",
            "",
            "avg-cpu:  %user   %nice %system %iowait  %steal   %idle",
            "          18.27    0.00    0.00    0.00    0.00   81.73",
            "",
        ],
    ),
    (14, &["0"]),
    (15, &[" 00 00 00 00 00 00 00 00"]),
    (16, &["cat: can't open '/tmp/x': No such device or address"]),
    (
        17,
        &[
            "uid=1000(ferrix) gid=1000(ferrix) groups=1000(ferrix)",
            "cat: /tmp/dac-private: Permission denied",
            "owned by 1000 1000",
            "rm: cannot remove '/tmp/dac-private': Permission denied",
            "chmod: Operation not permitted (os error 1)",
            "ls: cannot open directory '/tmp/dac-closed': Permission denied",
            "zsh:7: permission denied: /tmp/dac-noexec",
            "proc self owned by 1000 1000",
            "listed its own descriptors",
            "Uid:\t1000\t1000\t1000\t1000",
            "set-user-id gives uid 1000 euid 0",
            "hostname: failed to set hostname: Permission denied",
            "zsh:kill:13: kill 1 failed: operation not permitted",
            "mknod: Operation not permitted (os error 1)",
            "login shell -zsh in /home/ferrix as 1000",
            "root still reads: secret",
        ],
    ),
    (
        18,
        &[
            "delegated to 1000",
            "made by 1000",
            "moved itself in",
            "0::/deleg/work",
            "move out of the subtree refused",
            "move to the root refused",
            "0::/deleg/work",
            "root took its shell back",
        ],
    ),
    (
        19,
        &[
            "cpu memory pids",
            "10",
            "67108864",
            "200",
            "a fork refused within pids.max",
            "pids.events counted it",
            "removed",
        ],
    ),
    (
        20,
        &[
            "4+0 records in",
            "4+0 records out",
            "4096 bytes (4.0KB) copied, 0.000531 seconds, 7.4MB/s",
            "4+0 records in",
            "4+0 records out",
            "4096 bytes (4.0KB) copied, 0.000499 seconds, 7.8MB/s",
            "4+0 records in",
            "4+0 records out",
            "4096 bytes (4.0KB) copied, 0.000507 seconds, 7.7MB/s",
        ],
    ),
    (
        21,
        &[
            "file creation refused within memory.max",
            "memory.current within memory.max",
            "memory.stat counts kernel memory",
            "memory.events counted the refusal",
            "memory.current back to 0",
            "removed",
        ],
    ),
    (22, &["killed by SIGKILL", "oom_kill 1", "removed"]),
    (
        23,
        &[
            "running cgroup used the processor",
            "cgroup.events frozen 1",
            "frozen cgroup used no processor",
            "thawed cgroup ran again",
            "frozen cgroup killed",
            "removed",
        ],
    ),
    (
        24,
        &[
            "max 100000",
            "20000 100000",
            "held to about a fifth of a processor",
            "cpu.stat counted the throttling",
            "free again",
            "removed",
        ],
    ),
];

/// A log of the applets alone, numbered after the criterion's commands, each
/// printing `output` and exiting with its expected status.
fn applets_log(outputs: &[(usize, &[&str])]) -> Vec<String> {
    let mut log = owned(&["FERRIX-BOOT-OK stages 1-12"]);
    for (index, output) in outputs {
        let command = &APPLETS[index - COMMANDS.len()];
        log.push(format!("  init     command {index}: {}", command.argv[0]));
        log.extend(owned(output));
        log.push(format!(
            "  init     command {index} exited with {}",
            command.status
        ));
    }
    log
}

#[test]
fn the_applets_pass_on_what_busybox_printed_on_ferrix() {
    assert_eq!(APPLETS_ON_X86_64.len(), APPLETS.len());
    let log = applets_log(APPLETS_ON_X86_64);
    let passed = judge(APPLETS, COMMANDS.len(), &log).unwrap();
    assert_eq!(passed.len(), APPLETS.len());
    assert!(passed[0].starts_with("command 3 (sh -c 'cd /tmp && pwdx"));
}

#[test]
fn a_failing_applet_is_named_by_its_number_with_its_output() {
    let mut outputs = APPLETS_ON_X86_64.to_vec();
    // `/proc/stat` gone: top prints its memory and no CPU line.
    outputs[8].1 = &["Mem: 3964K used, 507124K free, 0K shrd, 0K buff, 0K cached"];
    // A character node that opens as nothing, rather than as null.
    outputs[11].1 = &["4"];
    let failed = judge(APPLETS, COMMANDS.len(), &applets_log(&outputs)).unwrap_err();
    assert_eq!(failed.len(), 2, "{failed:#?}");
    assert!(failed[0].starts_with("command 11 (top -b)"), "{failed:#?}");
    assert!(failed[0].contains("CPU: *% usr *% idle*"), "{failed:#?}");
    assert!(failed[0].contains("507124K free"), "the output is shown");
    assert!(
        failed[1].starts_with("command 14 (sh -c 'mknod /tmp/n"),
        "{failed:#?}"
    );
    assert!(failed[1].contains("[\"4\"]"), "{failed:#?}");
}

#[test]
fn the_criterion_is_judged_apart_from_the_applets() {
    let log = passing_log();
    assert_eq!(judge(COMMANDS, 0, &log).unwrap().len(), COMMANDS.len());
    assert_eq!(
        judge(APPLETS, COMMANDS.len(), &log).unwrap().len(),
        APPLETS.len()
    );
    // An applet that never ends leaves the criterion passing.
    let last = COMMANDS.len() + APPLETS.len() - 1;
    let mut log = log;
    log.retain(|line| !line.contains(&format!("command {last} exited")));
    assert!(judge(COMMANDS, 0, &log).is_ok());
    let failed = judge(APPLETS, COMMANDS.len(), &log).unwrap_err();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].contains("never exited"), "{failed:#?}");
}

#[test]
fn kernel_lines_about_commands_are_read() {
    assert_eq!(
        marker("  init     command 3: sh -c <200 bytes>"),
        Some((3, Marker::Started))
    );
    assert_eq!(
        marker("  init     command 12 exited with -1"),
        Some((12, Marker::Ended(Ending::Exited(-1))))
    );
    assert_eq!(
        marker("  init     command 0 could not be started: Load(NotElf)"),
        Some((
            0,
            Marker::Ended(Ending::NotStarted("Load(NotElf)".to_owned()))
        ))
    );
    assert_eq!(marker("  init     every command has run"), None);
    assert_eq!(marker("  init     command x: ls"), None);
    assert_eq!(marker("tmpfs: one"), None);
}

#[test]
fn a_log_splits_into_each_commands_output_and_unanswered_calls() {
    let log = owned(&[
        "FERRIX-BOOT-OK stages 1-12",
        "  init     /bin/busybox is 857 KiB, running 2 commands",
        "  init     command 0: ls -R /proc",
        "  syscall  Getdents64 (number 217) answered ENOSYS",
        "/proc:",
        "",
        "  init     command 0 exited with 0\r",
        "  init     command 1: cat /proc/self/maps",
        "  init     command 1 could not be started: Start(\"no stack\")",
        "stray output after every command",
    ]);
    let ran = split(&log);
    assert_eq!(ran.len(), 2);
    assert_eq!(ran[&0].output, owned(&["/proc:", ""]));
    assert_eq!(
        ran[&0].unanswered,
        owned(&["syscall  Getdents64 (number 217) answered ENOSYS"])
    );
    assert_eq!(ran[&0].ending, Some(Ending::Exited(0)));
    assert!(ran[&1].output.is_empty());
    assert!(matches!(ran[&1].ending, Some(Ending::NotStarted(_))));
}

#[test]
fn a_listing_reads_the_same_in_columns_and_in_colour() {
    let plain = owned(&["/proc:", "1", "self", "", "/proc/1:", "maps"]);
    let columns = owned(&[
        "/proc:",
        "\u{1b}[1;34m1\u{1b}[0m     \u{1b}[1;36mself\u{1b}[0m",
        "",
        "/proc/1:",
        "maps",
    ]);
    assert_eq!(listing(&plain), listing(&columns));
    assert_eq!(listing(&plain)["/proc"], owned(&["1", "self"]));
}

#[test]
fn a_proc_listing_must_reach_self_and_the_programs_maps() {
    assert_eq!(
        proc_listing(&owned(&[
            "/proc:", "7", "self", "", "/proc/7:", "exe", "maps"
        ])),
        Ok(())
    );
    assert_eq!(
        proc_listing(&owned(&["/proc:", "self", "", "/proc/self:", "maps"])),
        Ok(())
    );
    // What refusing `getdents64` looks like: headings, and nothing under them.
    assert!(proc_listing(&owned(&["/proc:"])).is_err());
    assert!(proc_listing(&owned(&["/proc:", "1", "", "/proc/1:", "maps"])).is_err());
    assert!(proc_listing(&owned(&["/proc:", "self", "cpuinfo"])).is_err());
    assert!(
        proc_listing(&owned(&["/proc:", "self", "", "/proc/sys:", "maps"])).is_err(),
        "a directory that is not a process's does not count"
    );
}

#[test]
fn maps_lines_parse_as_linux_writes_them() {
    let line = maps_line(
        "00400000-00401000 r-xp 00001000 fc:00 9044019                            \
         /bin/busy box",
    )
    .unwrap();
    assert_eq!(
        line,
        MapsLine {
            start: 0x40_0000,
            end: 0x40_1000,
            perms: "r-xp".to_owned(),
            offset: 0x1000,
            dev: (0xfc, 0),
            inode: 9_044_019,
            path: Some("/bin/busy box".to_owned()),
        }
    );
    assert_eq!(
        maps_line("7ffd22ede000-7ffd22eff000 rw-p 00000000 00:00 0 [stack]")
            .unwrap()
            .path
            .as_deref(),
        Some("[stack]")
    );
    let anonymous = maps_line("004fd000-004ff000 rw-p 00000000 00:00 0 ").unwrap();
    assert_eq!(anonymous.path, None);
    assert_eq!(
        maps_line("ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0 [vsyscall]")
            .unwrap()
            .perms,
        "--xp"
    );
}

#[test]
fn malformed_maps_lines_say_what_is_wrong() {
    for line in [
        "",
        "00400000 r--p 00000000 00:00 0",
        "00401000-00400000 r--p 00000000 00:00 0",
        "00400000-00401000 rw-- 00000000 00:00 0",
        "00400000-00401000 r--p 0000zz00 00:00 0",
        "00400000-00401000 r--p 00000000 0000 0",
        "00400000-00401000 r--p 00000000 00:00",
        "00400000-00401000 r--p 00000000 00:00 -1",
        "-00401000 r--p 00000000 00:00 0",
    ] {
        assert!(maps_line(line).is_err(), "`{line}` must be refused");
    }
}

#[test]
fn maps_must_ascend_without_overlap_and_not_be_empty() {
    let ordered = owned(&[
        "00400000-00401000 r--p 00000000 00:00 0",
        "00401000-00402000 r-xp 00000000 00:00 0",
    ]);
    assert_eq!(maps(&ordered), Ok(()));
    let overlapping = owned(&[
        "00400000-00402000 r--p 00000000 00:00 0",
        "00401000-00403000 r-xp 00000000 00:00 0",
    ]);
    assert!(maps(&overlapping).is_err());
    assert!(maps(&[]).is_err());
    assert!(maps(&owned(&["FERRIX says hello"])).is_err());
}

#[test]
fn a_passing_log_passes() {
    let passed = judge(COMMANDS, 0, &passing_log()).unwrap();
    assert_eq!(passed.len(), COMMANDS.len());
}

#[test]
fn every_failure_is_reported_with_the_calls_that_went_unanswered() {
    let mut log = passing_log();
    // Command 1's maps line, garbled; command 2's status, wrong, with a
    // refused call reported while it ran.
    let maps = log.iter().position(|l| l.starts_with("00400000-")).unwrap();
    log[maps] = "not a maps line".to_owned();
    let exit = log
        .iter()
        .position(|l| l == "  init     command 2 exited with 8")
        .unwrap();
    log[exit] = "  init     command 2 exited with 1".to_owned();
    log.insert(
        exit,
        "  syscall  Dup2 (number 33) answered ENOSYS".to_owned(),
    );

    let failed = judge(COMMANDS, 0, &log).unwrap_err();
    assert_eq!(failed.len(), 2, "{failed:#?}");
    assert!(failed[0].starts_with("command 1 (cat /proc/self/maps)"));
    assert!(failed[1].starts_with("command 2 (sh -c)"));
    assert!(failed[1].contains("exited with 1, not 8"));
    assert!(failed[1].contains("Dup2 (number 33)"));
}

#[test]
fn a_command_that_never_ends_or_never_starts_fails() {
    let mut log = passing_log();
    let last = COMMANDS.len() - 1;
    log.retain(|line| {
        !line.contains(&format!("command {last} exited")) && !line.contains("command 0")
    });
    let failed = judge(COMMANDS, 0, &log).unwrap_err();
    assert!(failed[0].contains("never started"), "{failed:#?}");
    assert!(
        failed.last().unwrap().contains("never exited"),
        "{failed:#?}"
    );
}

#[test]
fn an_unreadable_program_is_the_whole_report() {
    let log = owned(&["  init     /bin/busybox could not be read: errno 2"]);
    assert_eq!(
        judge(COMMANDS, 0, &log).unwrap_err(),
        owned(&["init     /bin/busybox could not be read: errno 2"])
    );
}

/// What the permissions row printed on `aarch64`, whose image carries busybox
/// and no uutils, from `cargo xtask test-vfs --arch aarch64`.
const PERMISSIONS_ON_AARCH64: &[&str] = &[
    "uid=1000(ferrix) gid=1000(ferrix) groups=1000(ferrix)",
    "cat: can't open '/tmp/dac-private': Permission denied",
    "owned by 1000 1000",
    "rm: can't remove '/tmp/dac-private': Operation not permitted",
    "chmod: /tmp/dac-private: Operation not permitted",
    "ls: can't open '/tmp/dac-closed': Permission denied",
    "zsh:7: permission denied: /tmp/dac-noexec",
    "proc self owned by 1000 1000",
    "listed its own descriptors",
    "Uid:\t1000\t1000\t1000\t1000",
    "set-user-id gives uid 1000 euid 0",
    "hostname: sethostname: Operation not permitted",
    "zsh:kill:13: kill 1 failed: operation not permitted",
    "mknod: /tmp/dac-null: Operation not permitted",
    "login shell -zsh in /home/ferrix as 1000",
    "root still reads: secret",
];

/// The applets' outputs with the permissions row's replaced by `permissions`.
fn with_permissions<'a>(permissions: &'a [&'a str]) -> Vec<(usize, &'a [&'a str])> {
    APPLETS_ON_X86_64
        .iter()
        .map(|&(index, output)| {
            if APPLETS[index - COMMANDS.len()].argv == PERMISSIONS {
                (index, permissions)
            } else {
                (index, output)
            }
        })
        .collect()
}

#[test]
fn the_permissions_row_is_judged_in_the_words_of_the_utilities_the_image_carries() {
    let busybox = with_permissions(PERMISSIONS_ON_AARCH64);
    let uutils = APPLETS_ON_X86_64.to_vec();
    assert_eq!(
        judge(
            &applets(Utilities::Busybox),
            COMMANDS.len(),
            &applets_log(&busybox)
        )
        .unwrap()
        .len(),
        APPLETS.len()
    );
    assert_eq!(
        judge(
            &applets(Utilities::Uutils),
            COMMANDS.len(),
            &applets_log(&uutils)
        )
        .unwrap()
        .len(),
        APPLETS.len()
    );
    // Each image's words fail the other's expectation: the check is not a
    // loose one that either would pass.
    let failed = judge(
        &applets(Utilities::Uutils),
        COMMANDS.len(),
        &applets_log(&busybox),
    )
    .unwrap_err();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].starts_with("command 17 (sh -c)"), "{failed:?}");
    let failed = judge(
        &applets(Utilities::Busybox),
        COMMANDS.len(),
        &applets_log(&uutils),
    )
    .unwrap_err();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].starts_with("command 17 (sh -c)"), "{failed:?}");
}

#[test]
fn an_image_carries_uutils_when_it_carries_coreutils() {
    let coreutils = [("coreutils", Vec::new()), ("find", Vec::new())];
    assert_eq!(Utilities::carried(&coreutils), Utilities::Uutils);
    assert_eq!(Utilities::carried(&coreutils[1..]), Utilities::Busybox);
    assert_eq!(Utilities::carried(&[]), Utilities::Busybox);
    let uutils = commands(Utilities::Uutils);
    let busybox = commands(Utilities::Busybox);
    assert_eq!(uutils.len(), busybox.len() + UTILITIES.len());
    assert_eq!(
        busybox.len(),
        COMMANDS.len() + APPLETS.len() + SHELL.len() + SEAM.len()
    );
}
