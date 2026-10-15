use super::*;

/// A scratch directory of its own.
fn scratch(name: &str) -> PathBuf {
    let path = crate::paths::target_dir()
        .join("xtask-test-runs-test")
        .join(format!("{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Strings.
fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

#[test]
fn cargo_s_running_lines_name_their_binaries() {
    assert_eq!(
        running_binary("     Running unittests src/lib.rs (/t/debug/deps/a-1)"),
        Some("/t/debug/deps/a-1".to_owned())
    );
    assert_eq!(
        running_binary("     Running tests/x (y).rs (/t/debug/deps/x-2)"),
        Some("/t/debug/deps/x-2".to_owned())
    );
    assert_eq!(running_binary("   Compiling a v0.1.0 (/src/a)"), None);
    assert_eq!(running_binary("test a ... ok"), None);
}

/// A record of binary `path` that ended with `status`.
fn ended(path: &str, status: &str) -> Ended {
    Ended {
        command: words(&[path]),
        status: status.to_owned(),
        output: Vec::new(),
        seconds: "0.0".to_owned(),
        failure: None,
    }
}

#[test]
fn cargo_s_lines_and_the_records_must_agree() {
    let running = words(&["/a", "/b", "/b"]);
    assert!(
        disagreements(
            &running,
            &[ended("/b", "0"), ended("/a", "0"), ended("/b", "0")]
        )
        .is_empty()
    );
    // A binary cargo ran that the runner did not record, and one recorded
    // twice that cargo ran once.
    let missing = disagreements(&running, &[ended("/a", "0"), ended("/b", "0")]);
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(missing[0].contains("/b"), "{missing:?}");
    let extra = disagreements(&words(&["/a"]), &[ended("/a", "0"), ended("/a", "0")]);
    assert_eq!(extra.len(), 1, "{extra:?}");
    assert_eq!(disagreements(&[], &[ended("/c", "0")]).len(), 1);
}

/// A run directory holding binary 0, a copy of `/bin/true`, with `status`
/// and its digests recorded; or no status at all.
fn recorded(name: &str, status: Option<&str>) -> (PathBuf, PathBuf) {
    let directory = scratch(name);
    let binary = directory.join("binary");
    let _ = std::fs::copy("/bin/true", &binary).unwrap();
    let command = words(&[binary.to_str().unwrap()]);
    std::fs::write(directory.join("0.cmd"), command.join("\0")).unwrap();
    std::fs::write(
        directory.join("0.digests"),
        digests(&command, &environment_binaries()).unwrap(),
    )
    .unwrap();
    if let Some(status) = status {
        std::fs::write(directory.join("0.status"), status).unwrap();
    }
    (directory, binary)
}

#[test]
fn every_way_a_binary_can_end_badly_is_a_failure() {
    let (directory, _) = recorded("passed", Some("0"));
    assert_eq!(wait_for(&directory, 0).failure, None);
    for status in [
        "1",
        "101",
        "a signal (signal: 9 (SIGKILL))",
        "could not start: x",
        "no slot: x",
    ] {
        let (directory, _) = recorded("failed", Some(status));
        assert!(wait_for(&directory, 0).failure.is_some(), "{status}");
    }
    // No status, and no waiter left to write one.
    let (directory, _) = recorded("silent", None);
    let one = wait_for(&directory, 0);
    assert!(one.failure.unwrap_or_default().contains("never reported"));
    // A waiter whose process is gone.
    let (directory, _) = recorded("gone", None);
    std::fs::write(directory.join("0.waiting"), "999999999").unwrap();
    assert!(wait_for(&directory, 0).failure.is_some());
    // Its binary rewritten while it ran.
    let (directory, binary) = recorded("rewritten", Some("0"));
    std::fs::write(&binary, b"another build").unwrap();
    assert!(
        wait_for(&directory, 0)
            .failure
            .unwrap_or_default()
            .contains("changed")
    );
    std::fs::remove_dir_all(
        crate::paths::target_dir()
            .join("xtask-test-runs-test")
            .join(format!("{}-rewritten", std::process::id())),
    )
    .unwrap();
}

#[test]
fn a_binary_gets_cargo_s_environment_directory_and_arguments() {
    let directory = scratch("environment");
    let dump = directory.join("dump");
    let script = format!(
        "env -0 > '{0}.env'; pwd > '{0}.pwd'; printf '%s\\n' \"$@\" > '{0}.args'",
        dump.display()
    );
    let status = run(&words(&["sh", "-c", &script, "sh", "one", "two words"]), "");
    assert_eq!(status, "0");
    let mut child: Vec<String> = std::fs::read_to_string(format!("{}.env", dump.display()))
        .unwrap()
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .filter(|entry| {
            !["PWD=", "SHLVL=", "_=", "OLDPWD="]
                .iter()
                .any(|shell| entry.starts_with(shell))
        })
        .collect();
    let mut parent: Vec<String> = std::env::vars_os()
        .filter(|(key, _)| key != VARIABLE && key != OWNER)
        .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
        .filter(|entry| {
            !["PWD=", "SHLVL=", "_=", "OLDPWD="]
                .iter()
                .any(|shell| entry.starts_with(shell))
        })
        .collect();
    child.sort();
    parent.sort();
    assert_eq!(child, parent);
    assert_eq!(
        std::fs::read_to_string(format!("{}.pwd", dump.display()))
            .unwrap()
            .trim(),
        std::env::current_dir().unwrap().to_string_lossy()
    );
    assert_eq!(
        std::fs::read_to_string(format!("{}.args", dump.display())).unwrap(),
        "one\ntwo words\n"
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_exit_code_and_a_signal_are_reported() {
    assert_eq!(run(&words(&["sh", "-c", "exit 3"]), ""), "3");
    assert!(run(&words(&["sh", "-c", "kill -KILL $$"]), "").starts_with("a signal"));
    assert!(run(&words(&["/nonexistent/binary"]), "").starts_with("could not start"));
}

#[test]
fn a_binary_whose_check_is_gone_is_killed() {
    let status = run(&words(&["sleep", "30"]), "999999999");
    assert!(status.starts_with("killed"), "{status}");
}
