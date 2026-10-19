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
        verifies: None,
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
        member("ferrix-btrfs", "src/lib/fs/btrfs/format", &[], &[]),
        // Only a dev-dependency on the item: its own code is not linked.
        member("ferrix-driver", "src/lib/driver", &[], &["ferrix-heap"]),
        member("xtask", "tools/common/xtask", &["ferrix-elf"], &[]),
        member("ferrix-elf", "src/lib/elf", &[], &[]),
        member("ferrix-fuzzed", "src/lib/fuzzed", &[], &["proptest"]),
    ];
    let item = [
        "src/kernel/".to_owned(),
        "src/lib/fs/btrfs/format/".to_owned(),
    ];
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
            "src/lib/fs/btrfs/write",
            &["ferrix-btrfs"],
            &[],
        ),
        member("ferrix-btrfs", "src/lib/fs/btrfs/format", &[], &[]),
    ];
    let item = [
        "src/lib/fs/btrfs/format/".to_owned(),
        "src/lib/fs/btrfs/write/".to_owned(),
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
    // Host tests outside the item that verify the item's requirements
    // (ferrix-native's L.device.24, xtask's tags).
    for package in ["ferrix-native", "xtask"] {
        assert!(
            verdicts
                .get(package)
                .cloned()
                .flatten()
                .is_some_and(|why| why.starts_with("it verifies")),
            "{package}: {:?}",
            verdicts.get(package)
        );
    }
    assert_eq!(refusal(&root, &members, &item), None);
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
            assert!(unit.flake.is_none());
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

/// A scratch directory of its own.
fn scratch(name: &str) -> PathBuf {
    let path = crate::paths::target_dir()
        .join("xtask-test-cache-test")
        .join(format!("{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn a_package_whose_files_say_they_verify_a_requirement_is_never_cached() {
    let root = scratch("verifies");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    assert_eq!(verifies(&root), None);
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("tests/a.rs"),
        "/// Verifies: L.device.24, L.x.1\n#[test]\nfn a() {}\n",
    )
    .unwrap();
    let said = verifies(&root).unwrap();
    assert!(
        said.contains("L.device.24") && said.contains("L.x.1"),
        "{said}"
    );
    // Code that only names the tag still counts, its id not read.
    let mut ids = BTreeSet::new();
    tagged("let x = \"Verifies:\").skip(1);", &mut ids);
    assert_eq!(ids.into_iter().collect::<Vec<_>>(), ["(an id not read)"]);
    let mut tagged = member("ferrix-tagged", "src/lib/tagged", &[], &[]);
    tagged.verifies = Some(said);
    let verdicts = verdicts(&[tagged], &[]);
    assert!(
        verdicts["ferrix-tagged"]
            .as_deref()
            .unwrap()
            .starts_with("it verifies")
    );
    // A directory that cannot be listed counts as verifying.
    assert!(verifies(&root.join("nonexistent")).is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_dependency_the_workspace_renames_leaves_its_users_unread() {
    let manifest = "[workspace]\nmembers = [\"a\", \"b\"]\n\n[workspace.dependencies]\nferrix-x = { path = \"x\" }\nalias = { package = \"ferrix-y\", path = \"y\" }\n\n[profile.dev]\nopt-level = 1\n";
    let renamed = workspace_renames(manifest);
    assert_eq!(renamed.into_iter().collect::<Vec<_>>(), ["alias"]);
    let root = scratch("renames");
    std::fs::write(root.join("Cargo.toml"), manifest).unwrap();
    for (name, dependency) in [("a", "alias"), ("b", "ferrix-x")] {
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(
            root.join(name).join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\n\n[dependencies]\n{dependency}.workspace = true\n"
            ),
        )
        .unwrap();
    }
    let members = members(&root).unwrap();
    assert!(members[0].unread.as_deref().unwrap().contains("alias"));
    assert_eq!(members[1].unread, None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_item_the_workspace_does_not_hold_whole_caches_nothing() {
    let members = [
        member("ferrix-kernel", "src/kernel", &["ferrix-heap"], &[]),
        member("ferrix-heap", "src/lib/heap", &["ferrix-elsewhere"], &[]),
    ];
    let item = ["src/kernel/".to_owned()];
    assert!(
        links_outside(&members, &item)
            .unwrap()
            .contains("ferrix-elsewhere")
    );
    assert_eq!(links_outside(&members[..1], &[]), None);
    let root = scratch("outside");
    std::fs::create_dir_all(root.join("src/kernel/core")).unwrap();
    std::fs::write(root.join("src/kernel/Cargo.toml"), "").unwrap();
    std::fs::write(
        root.join("Cargo.lock"),
        "[[package]]\nname = \"ferrix-kernel\"\n",
    )
    .unwrap();
    let whole = [member("ferrix-kernel", "src/kernel", &[], &[])];
    assert_eq!(refusal(&root, &whole, &item), None);
    // A crate under the item that is no member.
    std::fs::write(root.join("src/kernel/core/Cargo.toml"), "").unwrap();
    assert!(
        refusal(&root, &whole, &item)
            .unwrap()
            .contains("core/Cargo.toml")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_randomizing_crate_in_the_lock_caches_nothing() {
    let lock = "version = 4\n\n[[package]]\nname = \"ferrix-a\"\nversion = \"0.1.0\"\n";
    assert_eq!(locked_randomizers(lock), None);
    let with = format!("{lock}\n[[package]]\nname = \"proptest\"\nversion = \"1.0.0\"\n");
    assert_eq!(locked_randomizers(&with), Some("proptest"));
    // The real lock holds none.
    let real = std::fs::read_to_string(crate::paths::workspace_root().join("Cargo.lock")).unwrap();
    assert_eq!(locked_randomizers(&real), None);
}

/// The consultant's condition 6, ledger 1650: only P3's units, keyed on
/// the whole item, may read it. One place in xtask's product code sets
/// `reads_item`, `step_cache::item_keyed`, which keys on the item's digest
/// in the same expression.
#[test]
fn only_the_whole_item_key_reads_the_item() {
    let source = crate::paths::workspace_root().join("tools/common/xtask/src");
    let mut pending = vec![source];
    let mut found = Vec::new();
    while let Some(here) = pending.pop() {
        for entry in std::fs::read_dir(&here).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && path.file_name().is_some_and(|name| name != "tests.rs")
            {
                let text = std::fs::read_to_string(&path).unwrap();
                let setter = format!("reads_item: {}", "true");
                for (at, _) in text.match_indices(&setter) {
                    found.push((path.clone(), text.get(..at).unwrap_or_default().to_owned()));
                }
            }
        }
    }
    assert_eq!(
        found.len(),
        1,
        "{:?}",
        found.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    let (path, before) = &found[0];
    assert!(path.ends_with("step_cache.rs"), "{}", path.display());
    let function = before.rsplit("fn ").next().unwrap_or_default();
    assert!(
        function.starts_with("item_keyed()"),
        "set in {function:.40}"
    );
    assert!(
        function.contains("lines: vec![format!(\"item {digest}\")]"),
        "{function}"
    );
}
