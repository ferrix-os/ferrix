use super::*;

/// A scratch directory of its own, standing in for a worktree.
fn scratch(name: &str) -> PathBuf {
    // Not under /tmp, which the cache treats as a unit's scratch.
    let path = paths::target_dir()
        .join("xtask-step-cache-test")
        .join(format!("{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// A context rooted at `root`, with no item but `root/item/`, the target
/// directory `root/target`, and a home of its own.
fn context(root: &Path) -> Context {
    Context {
        root: root.to_path_buf(),
        target: root.join("target"),
        home: root.join("home"),
        cargo_home: root.join("home/.cargo"),
        rustup_home: root.join("home/.rustup"),
        temporary: PathBuf::from("/tmp"),
        prelude: vec![
            "xtask aaaa".to_owned(),
            "env bbbb".to_owned(),
            "host cccc".to_owned(),
        ],
        origin: "from the test".to_owned(),
        item: vec!["item/".to_owned()],
        xtask: root.join("target/debug/xtask"),
        unit_target: root.join("target/check-unit"),
        runtime: None,
    }
}

/// A trace directory holding `traces`, one file per pid, each line given
/// a time stamp as `strace -ttt` writes one (`1.<n>`) unless it has one.
fn traces(root: &Path, traces: &[(u32, &str)]) -> PathBuf {
    let directory = root.join("traces");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    for (pid, text) in traces {
        let stamped: String = text
            .lines()
            .enumerate()
            .map(|(index, line)| {
                if stamp(line).0 == 0 {
                    format!("1.{:06} {line}\n", 500_000 + index)
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        std::fs::write(directory.join(format!("t.{pid}")), stamped).unwrap();
    }
    directory
}

/// [`traces`], with the lines as given.
fn unstamped_traces(root: &Path, text: &str) -> PathBuf {
    let directory = root.join("traces");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("t.100"), text).unwrap();
    directory
}

#[test]
fn a_call_without_a_time_stamp_is_refused() {
    let root = scratch("unstamped");
    let line = "access(\"/etc/hostname\", R_OK) = 0\n";
    let directory = unstamped_traces(&root, line);
    let traced = read_traces(&directory, &root, &context(&root)).unwrap();
    assert!(traced.refused.unwrap_or_default().contains("time stamp"));
    let directory = unstamped_traces(&root, &format!("1.000001 {line}"));
    let traced = read_traces(&directory, &root, &context(&root)).unwrap();
    assert_eq!(traced.refused, None);
    std::fs::remove_dir_all(root).unwrap();
}

/// A record of the unit traced as `trace`, made at `root`.
fn record_of(root: &Path, trace: &str) -> String {
    let directory = traces(root, &[(100, trace)]);
    record("unit", &directory, &context(root)).unwrap()
}

#[test]
fn a_line_parses_into_its_call_result_and_descriptor_path() {
    let call =
        parse_line(r#"openat(AT_FDCWD</w>, "src/a.rs", O_RDONLY|O_CLOEXEC) = 3</w/src/a.rs>"#)
            .unwrap()
            .unwrap();
    assert_eq!(call.name, "openat");
    assert_eq!(call.returned, 3);
    assert_eq!(call.returned_path.as_deref(), Some("/w/src/a.rs"));
    let failed = parse_line(
        r#"newfstatat(AT_FDCWD</w>, "nope", 0x7ffd, 0) = -1 ENOENT (No such file or directory)"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(failed.returned, -1);
    assert_eq!(failed.error.as_deref(), Some("ENOENT"));
    assert_eq!(
        parse_line("exit_group(0)                           = ?").unwrap(),
        None
    );
    let padded = parse_line(
        r#"access("/etc/ld.so.preload", R_OK)      = -1 ENOENT (No such file or directory)"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(padded.error.as_deref(), Some("ENOENT"));
    assert_eq!(parse_line("+++ exited with 0 +++").unwrap(), None);
    assert!(parse_line("garbage").is_err());
}

#[test]
fn strings_are_unescaped_and_know_their_directory() {
    let found = strings(r#"3</a b>, "x\"y\\z\101\x42", O_RDONLY"#).unwrap();
    assert_eq!(
        found,
        vec![("x\"y\\zAB".to_owned(), Some("/a b".to_owned()))]
    );
    // argv's strings are not paths.
    let found = strings(r#""/bin/sh", ["sh", "-c", "x"], 0x1 /* 3 vars */"#).unwrap();
    assert_eq!(found, vec![("/bin/sh".to_owned(), None)]);
    // A string cut short is kept, marked, and refused only as a path.
    let cut = strings(r#""cut short"..., 0"#).unwrap();
    assert!(cut[0].0.starts_with('\0'), "{cut:?}");
}

#[test]
fn a_child_starts_in_its_parents_directory_and_relative_paths_resolve() {
    let root = scratch("cwd");
    std::fs::write(root.join("a"), b"a").unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/b"), b"b").unwrap();
    let parent = "chdir(\"sub\") = 0\nclone(child_stack=NULL, flags=SIGCHLD) = 101\naccess(\"../a\", R_OK) = 0\n";
    let child = "access(\"b\", R_OK) = 0\n";
    let directory = traces(&root, &[(100, parent), (101, child)]);
    let traced = read_traces(&directory, &root, &context(&root)).unwrap();
    assert_eq!(traced.refused, None);
    let paths: Vec<&Path> = traced.inputs.iter().map(input_path).collect();
    assert!(paths.contains(&root.join("sub/b").as_path()), "{paths:?}");
    assert!(
        paths.contains(&root.join("sub/../a").as_path()),
        "{paths:?}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn what_may_not_be_recorded_is_refused() {
    let root = scratch("refused");
    std::fs::create_dir_all(root.join("item")).unwrap();
    std::fs::write(root.join("item/k.rs"), b"k").unwrap();
    let cases = [
        (
            "socket(AF_INET, SOCK_STREAM, IPPROTO_IP) = 3</socket>\n",
            "socket",
        ),
        (
            &*format!(
                "openat(AT_FDCWD<{0}>, \"out\", O_WRONLY|O_CREAT, 0666) = 3<{0}/out>\n",
                root.display()
            ),
            "wrote",
        ),
        (
            &*format!(
                "openat(AT_FDCWD<{0}>, \"item/k.rs\", O_RDONLY) = 3<{0}/item/k.rs>\n",
                root.display()
            ),
            "certified item",
        ),
        (
            "openat(AT_FDCWD</>, \"/tmp/not-mine\", O_RDONLY) = 3</tmp/not-mine>\n",
            "had not made",
        ),
        ("mount(\"a\", \"b\", NULL, 0, NULL) = 0\n", "does not know"),
    ];
    for (trace, why) in cases {
        let directory = traces(&root, &[(100, trace)]);
        let traced = read_traces(&directory, &root, &context(&root)).unwrap();
        let refused = traced.refused.unwrap_or_default();
        assert!(refused.contains(why), "{trace}: {refused}");
    }
    // A pipe reopened through /proc (cargo's jobserver) is no file.
    let pipe = "openat(AT_FDCWD</>, \"/proc/self/fd/5\", O_WRONLY) = 6<pipe:[123]>\n";
    let directory = traces(&root, &[(100, pipe)]);
    let traced = read_traces(&directory, &root, &context(&root)).unwrap();
    assert_eq!(traced.refused, None);
    assert!(traced.inputs.is_empty());
    // A temporary file the run made itself, in another process, may be read.
    let made = "openat(AT_FDCWD</>, \"/tmp/mine\", O_RDONLY) = 3</tmp/mine>\n";
    let maker =
        "openat(AT_FDCWD</>, \"/tmp/mine\", O_WRONLY|O_CREAT|O_EXCL, 0600) = 3</tmp/mine>\n";
    let directory = traces(
        &root,
        &[
            (100, &format!("clone(flags=SIGCHLD) = 101\n{made}")),
            (101, maker),
        ],
    );
    let traced = read_traces(&directory, &root, &context(&root)).unwrap();
    assert_eq!(traced.refused, None);
    std::fs::remove_dir_all(root).unwrap();
}

/// A unit that read `f`, listed `d`, found `gone` absent and looked at
/// `sized`'s size.
fn sample_trace(root: &Path) -> String {
    format!(
        "openat(AT_FDCWD<{r}>, \"f\", O_RDONLY) = 3<{r}/f>\n\
         openat(AT_FDCWD<{r}>, \"d\", O_RDONLY|O_DIRECTORY) = 3<{r}/d>\n\
         newfstatat(AT_FDCWD<{r}>, \"gone\", 0x1, 0) = -1 ENOENT (No such file or directory)\n\
         statx(AT_FDCWD<{r}>, \"sized\", AT_STATX_SYNC_AS_STAT, STATX_ALL, {{}}) = 0\n",
        r = root.display()
    )
}

/// A worktree the sample trace reads.
fn sample_root(name: &str) -> PathBuf {
    let root = scratch(name);
    std::fs::write(root.join("f"), b"one").unwrap();
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d/x"), b"").unwrap();
    std::fs::write(root.join("sized"), b"12345").unwrap();
    root
}

#[test]
fn an_unchanged_tree_holds_and_paths_are_kept_relative() {
    let root = sample_root("holds");
    let text = record_of(&root, &sample_trace(&root));
    assert!(text.contains("F ") && text.contains(" ./f\n"), "{text}");
    assert_eq!(
        holds("unit", &text, &context(&root)),
        Some(("from the test".to_owned(), 4))
    );
    // The same tree elsewhere -- another slot's worktree -- holds too.
    let other = sample_root("holds-elsewhere");
    assert!(holds("unit", &text, &context(&other)).is_some());
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(other).unwrap();
}

/// Each change to an input that must make the record a miss.
#[test]
fn every_kind_of_change_is_a_miss() {
    type Change = fn(&Path, &mut Context);
    let changes: [(&str, Change); 9] = [
        ("content", |root, _| {
            std::fs::write(root.join("f"), b"two").unwrap();
        }),
        ("a new file in a listed directory", |root, _| {
            std::fs::write(root.join("d/y"), b"").unwrap();
        }),
        ("an absent path appearing", |root, _| {
            std::fs::write(root.join("gone"), b"").unwrap();
        }),
        ("a looked-up file's bytes, its size kept", |root, _| {
            std::fs::write(root.join("sized"), b"54321").unwrap();
        }),
        ("a probed file's size", |root, _| {
            std::fs::write(root.join("sized"), b"123456").unwrap();
        }),
        ("an unreadable input", |root, _| {
            std::fs::remove_file(root.join("f")).unwrap();
        }),
        ("the environment", |_, context| {
            context.prelude[1] = "env dddd".to_owned();
        }),
        ("the xtask binary", |_, context| {
            context.prelude[0] = "xtask eeee".to_owned();
        }),
        ("the kernel or the processor", |_, context| {
            context.prelude[2] = "host ffff".to_owned();
        }),
    ];
    let mut still_held = Vec::new();
    for (what, change) in changes {
        let root = sample_root("miss");
        let text = record_of(&root, &sample_trace(&root));
        let mut here = context(&root);
        assert!(holds("unit", &text, &here).is_some(), "{what}: held before");
        change(&root, &mut here);
        if holds("unit", &text, &here).is_some() {
            still_held.push(what);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(still_held.is_empty(), "still held after: {still_held:?}");
}

#[test]
fn an_excluded_entry_in_a_listed_directory_changes_nothing() {
    let root = sample_root("listed-excluded");
    let text = record_of(&root, &sample_trace(&root));
    let mut here = context(&root);
    // `d` holds an excluded root's entry, cargo's lock beside its registry.
    here.cargo_home = root.join("d");
    let text_here = {
        let directory = traces(&root, &[(100, &sample_trace(&root))]);
        record("unit", &directory, &here).unwrap()
    };
    std::fs::write(root.join("d/.package-cache"), b"").unwrap();
    assert!(holds("unit", &text_here, &here).is_some());
    // And the same entry, not excluded, is a change.
    assert_eq!(holds("unit", &text, &context(&root)), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_toolchains_version_is_part_of_the_record() {
    let root = sample_root("toolchain");
    let toolchain = root.join("home/.rustup/toolchains/test-x86_64");
    std::fs::create_dir_all(toolchain.join("bin")).unwrap();
    let rustc = toolchain.join("bin/rustc");
    // Written beside and renamed into place, so no thread of this test
    // process holds it open for writing when it is run (ETXTBSY).
    let install = |text: &str| {
        let beside = toolchain.join("bin/.rustc");
        std::fs::write(&beside, text).unwrap();
        let _ = Command::new("chmod")
            .arg("+x")
            .arg(&beside)
            .status()
            .unwrap();
        std::fs::rename(&beside, &rustc).unwrap();
    };
    install("#!/bin/sh\necho \"rustc 1.0.0 (abc 2026-01-01)\"\n");
    let trace = format!(
        "{}execve(\"{}\", [\"rustc\"], 0x1 /* 1 vars */) = 0\n",
        sample_trace(&root),
        rustc.display()
    );
    let text = record_of(&root, &trace);
    assert!(text.contains("\ntoolchain "), "{text}");
    assert!(holds("unit", &text, &context(&root)).is_some());
    install("#!/bin/sh\necho \"rustc 1.0.1 (def 2026-02-01)\"\n");
    assert_eq!(holds("unit", &text, &context(&root)), None);
    // And the components installed.
    install("#!/bin/sh\necho \"rustc 1.0.0 (abc 2026-01-01)\"\n");
    assert!(holds("unit", &text, &context(&root)).is_some());
    std::fs::create_dir_all(toolchain.join("lib/rustlib")).unwrap();
    std::fs::write(toolchain.join("lib/rustlib/components"), "clippy\n").unwrap();
    assert_eq!(holds("unit", &text, &context(&root)), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_record_that_does_not_parse_or_names_another_store_is_a_miss() {
    let root = sample_root("garbled");
    let text = record_of(&root, &sample_trace(&root));
    let here = context(&root);
    assert_eq!(
        holds("unit", &text.replace(MAGIC, "ferrix-step-cache 0"), &here),
        None
    );
    assert_eq!(holds("unit", &format!("{text}Q what\n"), &here), None);
    assert_eq!(
        holds(
            "unit",
            &text.replace("excluded /proc", "excluded /nowhere"),
            &here
        ),
        None
    );
    assert_eq!(holds("unit", "", &here), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_item_paths_come_from_the_classification() {
    let item = item_paths(&paths::workspace_root()).unwrap();
    for expected in [
        "src/kernel/",
        "src/boot/",
        "src/lib/fs/btrfs/",
        "src/lib/fs/btrfs-write/",
        "src/lib/kernel/fallible/",
        "src/lib/kernel/sync/",
    ] {
        assert!(
            item.iter().any(|path| path == expected),
            "{expected} in {item:?}"
        );
    }
    // btrfs's VFS glue is load, not item.
    assert!(
        !item.iter().any(|path| path == "src/lib/fs/btrfs-vfs/"),
        "{item:?}"
    );
}

#[test]
fn an_elf_and_a_script_name_their_interpreters() {
    let root = scratch("interp");
    let script = root.join("s");
    std::fs::write(&script, b"#!/bin/sh -e\necho\n").unwrap();
    assert_eq!(interpreters(&script), vec![PathBuf::from("/bin/sh")]);
    if let Some(interpreter) = elf_interpreter(&std::fs::read("/bin/sh").unwrap()) {
        assert!(
            interpreter.to_string_lossy().contains("ld-linux"),
            "{interpreter:?}"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_date_is_the_civil_one() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(20_376), (2025, 10, 15));
}

/// Whether the trace `text` (one process, pid 100) is refused, and why.
fn refused(root: &Path, text: &str) -> Option<String> {
    let directory = traces(root, &[(100, text)]);
    read_traces(&directory, root, &context(root))
        .unwrap()
        .refused
}

#[test]
fn a_socket_the_run_did_not_bind_is_refused() {
    let root = scratch("sockets");
    let connect = "connect(5<UNIX:[1]>, {sa_family=AF_UNIX, sun_path=\"/run/daemon\"}, 110) = 0\n";
    assert!(
        refused(&root, connect)
            .unwrap_or_default()
            .contains("did not bind")
    );
    let abstract_name = "connect(5<UNIX:[1]>, {sa_family=AF_UNIX, sun_path=@\"x\"}, 4) = 0\n";
    assert!(
        refused(&root, abstract_name)
            .unwrap_or_default()
            .contains("did not bind")
    );
    let bound = "bind(4<UNIX:[2]>, {sa_family=AF_UNIX, sun_path=\"/tmp/mine.sock\"}, 110) = 0\n\
                 connect(5<UNIX:[1]>, {sa_family=AF_UNIX, sun_path=\"/tmp/mine.sock\"}, 110) = 0\n";
    assert_eq!(refused(&root, bound), None);
    let renamed = "1.1 bind(4<UNIX:[2]>, {sa_family=AF_UNIX, sun_path=\"/tmp/s/w.1.binding\"}, 56) = 0\n\
                   1.2 rename(\"/tmp/s/w.1.binding\", \"/tmp/s/w\") = 0\n\
                   1.3 connect(5<UNIX:[1]>, {sa_family=AF_UNIX, sun_path=\"/tmp/s/w\"}, 40) = 0\n";
    assert_eq!(refused(&root, renamed), None);
    let failed = "connect(5<UNIX:[1]>, {sa_family=AF_UNIX, sun_path=\"/run/nscd/socket\"}, 110) = -1 ENOENT (No such file or directory)\n";
    assert_eq!(refused(&root, failed), None);
    assert!(
        refused(&root, "io_uring_setup(8, 0x7ffd) = 3\n")
            .unwrap_or_default()
            .contains("io_uring_setup")
    );
    assert!(
        refused(&root, "pidfd_getfd(3, 4, 0) = 5\n")
            .unwrap_or_default()
            .contains("pidfd_getfd")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_temporary_file_counts_as_made_only_when_its_contents_are_fresh() {
    let root = scratch("fresh");
    // Opened to write, not created anew: what it held before is read.
    let kept = "1.000001 openat(AT_FDCWD</>, \"/tmp/old\", O_RDWR|O_CREAT, 0600) = 3</tmp/old>\n\
                1.000002 openat(AT_FDCWD</>, \"/tmp/old\", O_RDONLY) = 3</tmp/old>\n";
    assert!(
        refused(&root, kept)
            .unwrap_or_default()
            .contains("had not made")
    );
    for fresh in [
        "O_WRONLY|O_CREAT|O_TRUNC",
        "O_RDWR|O_CREAT|O_EXCL",
        "O_RDWR|O_TMPFILE",
    ] {
        let trace = format!(
            "1.000001 openat(AT_FDCWD</>, \"/tmp/new\", {fresh}, 0600) = 3</tmp/new>\n\
             1.000002 openat(AT_FDCWD</>, \"/tmp/new\", O_RDONLY) = 3</tmp/new>\n"
        );
        assert_eq!(refused(&root, &trace), None, "{fresh}");
    }
    // Read before it was made, in another process.
    let reader = "1.000001 openat(AT_FDCWD</>, \"/tmp/late\", O_RDONLY) = 3</tmp/late>\n";
    let maker = "1.000009 openat(AT_FDCWD</>, \"/tmp/late\", O_WRONLY|O_CREAT|O_TRUNC, 0600) = 3</tmp/late>\n";
    let directory = traces(
        &root,
        &[
            (
                100,
                &format!("0.000001 clone(flags=SIGCHLD) = 101\n{reader}"),
            ),
            (101, maker),
        ],
    );
    let traced = read_traces(&directory, &root, &context(&root)).unwrap();
    assert!(traced.refused.unwrap_or_default().contains("had not made"));
    // Under a directory the run made, a file is fresh.
    let made_dir = "1.000001 mkdir(\"/tmp/d\", 0700) = 0\n\
                    1.000002 openat(AT_FDCWD</>, \"/tmp/d/f\", O_RDONLY) = 3</tmp/d/f>\n";
    assert_eq!(refused(&root, made_dir), None);
    // /dev/shm is a temporary directory, not an excluded one.
    let shm = "openat(AT_FDCWD</>, \"/dev/shm/x\", O_RDONLY) = 3</dev/shm/x>\n";
    assert!(
        refused(&root, shm)
            .unwrap_or_default()
            .contains("had not made")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn another_steps_outputs_are_refused_and_the_units_own_are_not_inputs() {
    let root = scratch("outputs");
    let r = root.display();
    let other = format!(
        "openat(AT_FDCWD<{r}>, \"target/debug/libx.rlib\", O_RDONLY) = 3<{r}/target/debug/libx.rlib>\n"
    );
    assert!(
        refused(&root, &other)
            .unwrap_or_default()
            .contains("another step's output")
    );
    let own = format!(
        "openat(AT_FDCWD<{r}>, \"target/check-unit/debug/liby.rlib\", O_RDONLY) = 3<{r}/target/check-unit/debug/liby.rlib>\n"
    );
    let directory = traces(&root, &[(100, &own)]);
    let traced = read_traces(&directory, &root, &context(&root)).unwrap();
    assert_eq!(traced.refused, None);
    assert!(traced.inputs.is_empty());
    let made = format!(
        "1.1 openat(AT_FDCWD<{r}>, \"target/debug/z\", O_WRONLY|O_CREAT|O_TRUNC, 0644) = 3<{r}/target/debug/z>\n\
         1.2 openat(AT_FDCWD<{r}>, \"target/debug/z\", O_RDONLY) = 3<{r}/target/debug/z>\n"
    );
    assert_eq!(refused(&root, &made), None);
    let xtask = format!("execve(\"{r}/target/debug/xtask\", [\"xtask\"], 0x1 /* 1 vars */) = 0\n");
    assert_eq!(refused(&root, &xtask), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_path_with_dotdot_is_judged_where_it_leads() {
    let root = scratch("dotdot");
    std::fs::create_dir_all(root.join("item")).unwrap();
    std::fs::write(root.join("item/k.rs"), b"k").unwrap();
    let escape = "access(\"/tmp/../ITEM\", R_OK) = 0\n"
        .replace("/ITEM", &root.join("item/k.rs").display().to_string());
    assert!(
        refused(&root, &escape)
            .unwrap_or_default()
            .contains("certified item"),
        "{escape}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_record_of_another_unit_is_a_miss() {
    let root = sample_root("other-unit");
    let text = record_of(&root, &sample_trace(&root));
    assert!(holds("unit", &text, &context(&root)).is_some());
    assert_eq!(holds("another", &text, &context(&root)), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_item_list_fails_closed_when_a_crate_has_no_path() {
    let root = scratch("item-list");
    std::fs::create_dir_all(root.join("tools/common/data")).unwrap();
    std::fs::write(
        root.join("tools/common/data/certification-item.json"),
        "{\n  \"crates\": {\n    \"members\": {\n      \"a\": {\n        \"path\": \"src/lib/a\",\n        \"ring\": \"item\"\n      }\n    }\n  }\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("Cargo.toml"), "").unwrap();
    assert!(item_paths(&root).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn gate_sh_s_settings_are_not_part_of_the_environment() {
    // Each gate run has a tag of its own; a record must hold across runs.
    for ignored in [
        "FERRIX_GATE_TAG",
        "GATE_SLOT",
        "GATE_STEP_CACHE",
        "GATE_PRIORITY",
    ] {
        assert!(environment_ignored(ignored), "{ignored}");
    }
    for kept in [
        "RUSTFLAGS",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "FERRIX_INIT",
        "PATH",
        "GATE_SOMETHING_NEW",
    ] {
        assert!(!environment_ignored(kept), "{kept}");
    }
}
