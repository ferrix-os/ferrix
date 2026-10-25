//! The soft signer's command line, as CI runs it (`docs/SIGNING.md` §6.5):
//! the first signing-ci run failed in the command line's own parse.

#![expect(
    clippy::unwrap_used,
    reason = "AUDIT: a test: a signer that does not start fails it"
)]

use std::process::Command;

use ferrix_sha256 as _;
use ferrix_sigverify as _;
use ferrix_softsign as _;

fn signer(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ferrix-softsign"))
        .args(args)
        .env_remove("FERRIX_CI_SIGNING_KEY")
        .output()
        .unwrap()
}

/// `ci-range` prints a run attempt's leaves, and refuses a fifth attempt.
#[test]
fn ci_range_answers_as_the_workflow_asks() {
    let out = signer(&["ci-range", "--run-number", "1", "--run-attempt", "1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "64 80");
    let out = signer(&["ci-range", "--run-number", "1", "--run-attempt", "5"]);
    assert!(!out.status.success());
}

/// `ci-sign` with every flag the workflow passes, but no key, stops at the
/// key: everything before it parses.
#[test]
fn ci_sign_parses_the_workflows_flags_and_wants_the_key() {
    let out = signer(&[
        "ci-sign",
        "--run-number",
        "3",
        "--run-attempt",
        "1",
        "--prior-logs",
        "/dev/null",
        "--log",
        "/nonexistent/signing-ci.log",
        "--anchors",
        "../../data/signing-anchors.toml",
        "--workflow",
        ".github/workflows/signing-ci.yml",
        "--kind",
        "slot",
        "--anchor-id",
        "36864",
        "--profile",
        "4",
        "--arch",
        "1",
        "--family",
        "257",
        "--security-version",
        "1",
        "--label",
        "ferrix ci 3",
        "--payload",
        "/dev/null",
        "--out",
        "/nonexistent/m",
    ]);
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("FERRIX_CI_SIGNING_KEY is not set"), "{said}");
}
