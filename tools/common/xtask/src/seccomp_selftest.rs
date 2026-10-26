//! `test-seccomp-selftest`: Linux's own seccomp selftest on Ferrix
//! (`docs/SECCOMP.md` §8.2).
//!
//! `tools/testing/selftests/seccomp/seccomp_bpf.c` from Linux v6.12, built
//! static against musl for each architecture by
//! `tools/common/fetch/fetch-seccomp-selftest.sh`, booted as init. Its harness
//! prints a TAP line per test -- `ok N name`, `not ok N name`, either with
//! `# SKIP` -- and `# Totals:` at the end.
//!
//! `tools/common/data/seccomp-selftest/expected.txt` names every test that
//! does not pass, with what it gets and why. The gate fails on a test that
//! fails or skips and is not listed, or not as listed, and on a listed test
//! that now passes, so the list stays the truth: it shrinks as Ferrix grows,
//! and nothing slips onto it unseen. A run that stops before its totals fails
//! too.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::args::Args;
use crate::paths;
use crate::sem::status;
use crate::{Error, Result};

/// Seconds the run may take unless `--timeout` says: the harness gives each
/// of its tests 30 before it kills it, and the tests that wait on `ptrace`,
/// which Ferrix has not, take those 30.
const TIMEOUT: u64 = 3600;

/// What a test got.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Pass,
    Fail,
    Skip,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// Where the fetch script put the builds: `$FERRIX_SECCOMP_SELFTEST`, or
/// `~/.local/share/ferrix/seccomp-selftest`.
fn selftest_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("FERRIX_SECCOMP_SELFTEST") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| Error::new("neither HOME nor USERPROFILE is set"))?;
    Ok(PathBuf::from(home).join(".local/share/ferrix/seccomp-selftest"))
}

/// The expected list: `<arch|all> <fail|skip> <test>  # why`, a line each.
fn expected(arch: &str) -> Result<BTreeMap<String, Outcome>> {
    let path = paths::workspace_root().join("tools/common/data/seccomp-selftest/expected.txt");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    let mut list = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        let body = line.split('#').next().unwrap_or_default().trim();
        if body.is_empty() {
            continue;
        }
        let words: Vec<&str> = body.split_whitespace().collect();
        let [on, outcome, test] = words[..] else {
            return Err(Error::new(format!(
                "{}:{}: want `<arch|all> <fail|skip> <test>  # why`",
                path.display(),
                number + 1
            )));
        };
        if !line.contains('#') {
            return Err(Error::new(format!(
                "{}:{}: `{test}` says nothing of why",
                path.display(),
                number + 1
            )));
        }
        let outcome = match outcome {
            "fail" => Outcome::Fail,
            "skip" => Outcome::Skip,
            other => {
                return Err(Error::new(format!(
                    "{}:{}: `{other}` is neither fail nor skip",
                    path.display(),
                    number + 1
                )));
            }
        };
        if on == "all" || on == arch {
            let _ = list.insert(test.to_owned(), outcome);
        }
    }
    Ok(list)
}

/// The TAP lines' outcomes, by test name, and whether the totals came.
fn results(lines: &[String]) -> (BTreeMap<String, Outcome>, bool) {
    let mut got = BTreeMap::new();
    let mut totals = false;
    for line in lines {
        totals |= line.contains("# Totals:");
        let (outcome, rest) = if let Some((_, rest)) = line.split_once("not ok ") {
            (Outcome::Fail, rest)
        } else if let Some((_, rest)) = line.split_once("ok ") {
            (Outcome::Pass, rest)
        } else {
            continue;
        };
        let mut words = rest.split_whitespace();
        if words.next().is_none_or(|n| n.parse::<u32>().is_err()) {
            continue;
        }
        let Some(name) = words.next() else { continue };
        let outcome = if rest.contains("# SKIP") {
            Outcome::Skip
        } else {
            outcome
        };
        let _ = got.insert(name.to_owned(), outcome);
    }
    (got, totals)
}

/// The selftest on every architecture asked for.
pub(crate) fn run(args: &Args) -> Result<()> {
    let mut args = args.clone();
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    let args = &args;
    let dir = selftest_dir()?;
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");
        let program = dir.join(arch.to_string()).join("seccomp_bpf");
        if !program.is_file() {
            return Err(Error::new(format!(
                "{} is not built: run tools/common/fetch/fetch-seccomp-selftest.sh",
                program.display()
            )));
        }
        let lines = crate::seccomp::boot(arch, &program, args)?;
        let (got, totals) = results(&lines);
        if !totals || got.is_empty() || status(&lines).is_none() {
            return Err(Error::new(format!(
                "{arch}: the selftest stopped before its totals.\n  Serial output is in {}",
                log.display()
            )));
        }
        let want = expected(&arch.to_string())?;
        let mut wrong = Vec::new();
        for (test, outcome) in &got {
            let listed = want.get(test).copied().unwrap_or(Outcome::Pass);
            if listed != *outcome {
                wrong.push(format!(
                    "    {test}: {} where the list says {}",
                    outcome.word(),
                    listed.word()
                ));
            }
        }
        for test in want.keys().filter(|test| !got.contains_key(*test)) {
            wrong.push(format!("    {test}: listed, and the run has no such test"));
        }
        if !wrong.is_empty() {
            return Err(Error::new(format!(
                "{arch}: the selftest does not match tools/common/data/seccomp-selftest/expected.txt:\n{}\n  \
                 Serial output is in {}",
                wrong.join("\n"),
                log.display()
            )));
        }
        let count = |o| got.values().filter(|v| **v == o).count();
        println!(
            "  {arch}: {} of {} pass; {} fail and {} skip, each as listed",
            count(Outcome::Pass),
            got.len(),
            count(Outcome::Fail),
            count(Outcome::Skip)
        );
    }
    Ok(())
}
