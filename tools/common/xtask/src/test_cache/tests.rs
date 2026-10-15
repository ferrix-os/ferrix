use super::*;

/// A member named `name` at `directory` linking `linked` and naming `dev`
/// besides.
fn member(name: &str, directory: &str, linked: &[&str], dev: &[&str]) -> Member {
    let linked: BTreeSet<String> = linked.iter().map(|name| (*name).to_owned()).collect();
    let mut named = linked.clone();
    named.extend(dev.iter().map(|name| (*name).to_owned()));
    Member {
        name: name.to_owned(),
        directory: directory.to_owned(),
        linked,
        named,
        unread: None,
    }
}

#[test]
fn a_manifest_s_dependencies_are_read_by_kind() {
    let (linked, named, unread) = dependencies(
        r#"[package]
name = "a"

[dependencies]
ferrix-b.workspace = true
local = { package = "ferrix-c", path = "../package-c" }
plain = { path = "../package-d" } # a comment

[target.'cfg(unix)'.dependencies]
ferrix-e.workspace = true

[build-dependencies]
ferrix-f = "1"

[dev-dependencies]
ferrix-heap.workspace = true

[lints]
workspace = true
"#,
    );
    let names = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>();
    assert_eq!(
        names(&linked),
        ["ferrix-b", "ferrix-c", "ferrix-e", "ferrix-f", "plain"]
    );
    assert_eq!(
        names(&named),
        [
            "ferrix-b",
            "ferrix-c",
            "ferrix-e",
            "ferrix-f",
            "ferrix-heap",
            "plain"
        ]
    );
    assert_eq!(unread, None);
}

#[test]
fn a_manifest_shape_this_does_not_read_fails_closed() {
    for text in [
        "[dependencies.ferrix-b]\nworkspace = true\n",
        "[dependencies]\nferrix-b = { workspace = true,\n  features = [\"x\"] }\n",
        "[dependencies]\nnothing here\n",
    ] {
        let (_, _, unread) = dependencies(text);
        assert!(unread.is_some(), "{text}");
    }
}

#[test]
fn the_item_and_everything_it_links_are_never_cached() {
    let members = [
        member(
            "ferrix-kernel",
            "src/kernel",
            &["ferrix-heap", "ferrix-sync"],
            &[],
        ),
        member("ferrix-heap", "src/lib/heap", &["ferrix-frame"], &[]),
        member("ferrix-frame", "src/lib/frame", &[], &[]),
        member("ferrix-sync", "src/lib/sync", &[], &[]),
        member("ferrix-btrfs", "src/lib/fs/btrfs", &[], &[]),
        // Only a dev-dependency on the item: its own code is not linked.
        member("ferrix-driver", "src/lib/driver", &[], &["ferrix-heap"]),
        member("xtask", "tools/common/xtask", &["ferrix-elf"], &[]),
        member("ferrix-elf", "src/lib/elf", &[], &[]),
        member("ferrix-fuzzed", "src/lib/fuzzed", &[], &["proptest"]),
    ];
    let item = ["src/kernel/".to_owned(), "src/lib/fs/btrfs/".to_owned()];
    let verdicts = verdicts(&members, &item);
    let never = |name: &str| verdicts.get(name).cloned().flatten();
    assert!(
        never("ferrix-kernel")
            .unwrap()
            .contains("the item's (src/kernel/)")
    );
    assert!(never("ferrix-btrfs").unwrap().contains("the item's"));
    assert!(
        never("ferrix-heap")
            .unwrap()
            .contains("linked by the item's ferrix-kernel")
    );
    assert!(
        never("ferrix-frame")
            .unwrap()
            .contains("linked by the item's ferrix-heap")
    );
    assert!(never("ferrix-sync").is_some());
    assert!(never("ferrix-fuzzed").unwrap().contains("proptest"));
    assert_eq!(never("ferrix-driver"), None);
    assert_eq!(never("xtask"), None);
    assert_eq!(never("ferrix-elf"), None);
}

#[test]
fn an_item_crate_another_links_says_it_is_the_item_s_own() {
    let members = [
        member(
            "ferrix-btrfs-write",
            "src/lib/fs/btrfs-write",
            &["ferrix-btrfs"],
            &[],
        ),
        member("ferrix-btrfs", "src/lib/fs/btrfs", &[], &[]),
    ];
    let item = [
        "src/lib/fs/btrfs/".to_owned(),
        "src/lib/fs/btrfs-write/".to_owned(),
    ];
    for (name, verdict) in verdicts(&members, &item) {
        assert!(verdict.unwrap().starts_with("the item's ("), "{name}");
    }
}

#[test]
fn a_member_whose_manifest_was_not_read_whole_is_never_cached() {
    let mut odd = member("ferrix-odd", "src/lib/odd", &[], &[]);
    odd.unread = Some("the table [dependencies.x]".to_owned());
    let verdicts = verdicts(&[odd], &[]);
    assert!(
        verdicts["ferrix-odd"]
            .as_deref()
            .unwrap()
            .contains("not read whole")
    );
}

#[test]
fn the_real_workspace_keeps_the_item_s_test_binaries_running() {
    let root = crate::paths::workspace_root();
    let members = members(&root).unwrap();
    let item = step_cache::item_paths(&std::fs::canonicalize(&root).unwrap()).unwrap();
    let verdicts = verdicts(&members, &item);
    // The item's, its allow-list's and a library only the kernel links.
    for package in [
        "ferrix-btrfs",
        "ferrix-btrfs-write",
        "ferrix-fallible",
        "ferrix-sync",
        "ferrix-heap",
        "ferrix-kernel",
    ] {
        assert!(
            verdicts.get(package).cloned().flatten().is_some(),
            "{package} would be cached"
        );
    }
    assert_eq!(verdicts.get("xtask").cloned().flatten(), None);
    for member in &members {
        assert!(verdicts.contains_key(&member.name), "{}", member.name);
    }
}

#[test]
fn a_binary_s_unit_is_named_by_its_package_place_and_stem() {
    assert_eq!(
        unit_name("xtask", Path::new("/t/debug/deps/xtask-696b1b03d83f1054")),
        "test:xtask:deps/xtask"
    );
    assert_eq!(
        unit_name(
            "ferrix-argon2",
            Path::new("/t/debug/examples/timing-7c807c24e957b5d2")
        ),
        "test:ferrix-argon2:examples/timing"
    );
    assert_eq!(
        unit_name("a", Path::new("/t/debug/deps/no-hash")),
        "test:a:deps/no-hash"
    );
}

/// The plan's text with one package, `package`, at `place`.
fn plan_text(verdict: &str) -> String {
    let shared = "xtask aaaa\nhost cccc\norigin from the test\n";
    format!("{MAGIC}\n{shared}store /nonexistent/store\n{verdict}\n")
}

#[test]
fn a_binary_the_plan_does_not_know_or_places_elsewhere_is_never_cached() {
    // This test binary's own environment: cargo names xtask and its
    // directory.
    let package = std::env::var("CARGO_PKG_NAME").unwrap();
    let binary = std::env::current_exe().unwrap();
    let binary = binary.to_string_lossy().into_owned();
    let digests = format!("{} {binary}\n", sha256::hex(b"stand-in"));
    let cwd = std::env::current_dir().unwrap();
    let command = vec![binary];
    let why = |text: &str| match unit(text, &command, &cwd, &digests) {
        Err(why) => why,
        Ok(way) => panic!("{way:?}"),
    };
    assert!(why("garbage").contains("does not parse"));
    assert!(why(&plan_text("cache someone-else src/x")).contains("does not know"));
    assert!(
        why(&plan_text(&format!("never {package} the item's (x/)"))).contains("the item's (x/)")
    );
    assert!(why(&plan_text(&format!("cache {package} src/elsewhere"))).contains("the plan at"));
    match unit(
        &plan_text(&format!("cache {package} tools/common/xtask")),
        &command,
        &cwd,
        &digests,
    ) {
        Ok(Way::Unit(unit)) => {
            assert_eq!(unit.name, format!("test:{package}:deps/xtask"));
            assert!(unit.directory.starts_with("/nonexistent/store"));
            assert!(unit.flake.is_some());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_key_holds_the_binary_its_programs_arguments_and_directory() {
    let context = Context::from_shared("xtask aaaa\nhost cccc\norigin from the test\n").unwrap();
    let binary = std::env::current_exe().unwrap();
    let path = binary.to_string_lossy().into_owned();
    let cwd = context.root().to_path_buf();
    let digests = format!("1111 {path}\n2222 {path}\n");
    let key = |command: &[&str], cwd: &Path, digests: &str| {
        let command: Vec<String> = command.iter().map(|word| (*word).to_owned()).collect();
        key_lines(&context, &command, cwd, digests).unwrap().0
    };
    let base = key(&[&path], &cwd, &digests);
    assert_eq!(base.len(), 4, "{base:?}");
    assert!(base[0].starts_with("test-binary 1111 "));
    assert!(base[1].starts_with("test-program 2222 "));
    assert_eq!(base[3], "test-directory ./");
    // Each part changed changes the key.
    assert_ne!(key(&[&path, "--exact"], &cwd, &digests), base);
    assert_ne!(key(&[&path], &cwd.join("src"), &digests), base);
    assert_ne!(
        key(&[&path], &cwd, &format!("1112 {path}\n2222 {path}\n")),
        base
    );
    assert_ne!(
        key(&[&path], &cwd, &format!("1111 {path}\n2223 {path}\n")),
        base
    );
    assert_ne!(key(&[&path], &cwd, &format!("1111 {path}\n")), base);
    let files = key_lines(&context, std::slice::from_ref(&path), &cwd, &digests)
        .unwrap()
        .1;
    let real = std::fs::canonicalize(&binary).unwrap();
    assert_eq!(files, [real.clone(), real]);
    assert!(key_lines(&context, &[path], &cwd, "").is_err());
}

#[test]
fn a_unit_whose_traced_run_was_not_recorded_is_not_traced_again_for_a_day() {
    let directory = crate::paths::target_dir()
        .join("xtask-test-cache-test")
        .join(format!("{}-memo", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let context = Context::from_shared("xtask aaaa\nhost cccc\norigin from the test\n").unwrap();
    let unit = |binary: &str| Unit {
        name: "test:a:deps/a".to_owned(),
        directory: directory.clone(),
        context: context.clone(),
        flake: None,
        binary: binary.to_owned(),
    };
    assert_eq!(unit("1111").unrecordable(), None);
    unit("1111").remember("ran, not recorded: it read the certified item (src/kernel/)");
    let why = Some("it read the certified item (src/kernel/)".to_owned());
    assert_eq!(unit("1111").unrecordable(), why);
    // Another binary of the unit too, within the day.
    assert_eq!(unit("2222").unrecordable(), why);
    // A day later it is traced again.
    let old = std::time::SystemTime::now() - UNRECORDABLE_FOR - std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(directory.join(UNRECORDABLE))
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(unit("2222").unrecordable(), None);
    // A recorded run forgets the memo.
    unit("1111").remember("ran, not recorded: x");
    assert!(unit("1111").unrecordable().is_some());
    unit("1111").remember("ran, recorded");
    assert_eq!(unit("1111").unrecordable(), None);
    std::fs::remove_dir_all(&directory).unwrap();
}
