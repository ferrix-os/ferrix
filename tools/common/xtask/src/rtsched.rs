//! `test-rtsched`: `SCHED_FIFO` and `SCHED_RR` as a Linux program asks for
//! them (`docs/SCHED-CLASSES.md` §9e, R12).
//!
//! The program is `src/tests/rtsched/`, built for each architecture's musl
//! target and booted as init on the run's kernel profile. Where the profile
//! builds the realtime tier (`sched_realtime = fifo_rr`, `desktop`), it moves
//! itself to `SCHED_FIFO` and back as `chrt` does, makes every refusal Linux
//! makes, is refused as uid 1000 by the `CAP_SYS_NICE` and `RLIMIT_RTPRIO`
//! rule, and forks and starts a thread under `SCHED_FIFO` with and without
//! reset-on-fork; then the negative control, built to expect a reset-on-fork
//! child to keep its parent's policy, must fail on that step. Where the
//! profile does not (`present`), the program built with `present` requires
//! every real-time policy to be `EINVAL`.

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::sem::{boot, build_test, says, status};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-rtsched",
    help_at: 552,
    help: &[
        "test-rtsched  Boot rtsched-test as init and require SCHED_FIFO and SCHED_RR's calls, errnos, permission rule and",
        "              reset-on-fork on a profile with the realtime tier (desktop), or every real-time policy EINVAL without it",
    ],
    run: |_, args| test_rtsched(args),
}];

/// The lines a working run prints on a profile with the tier, in order.
const STEPS: &[&str] = &[
    "rtsched: encoding ok",
    "rtsched: chrt ok",
    "rtsched: errors ok",
    "rtsched: permission ok",
    "rtsched: inherit ok",
    "rtsched: all ok",
];

/// The lines a working run prints on a profile without it.
const ABSENT_STEPS: &[&str] = &[
    "rtsched: encoding ok",
    "rtsched: absent ok",
    "rtsched: all ok",
];

/// The start of the line the negative control must fail with.
const NEGATIVE_FAILURE: &str = "rtsched: FAILED inherit: a reset-on-fork child did not start";

/// Require `steps`, in order, and status 0 in `lines`.
fn require(arch: Arch, lines: &[String], steps: &[&str]) -> Result<()> {
    let log = paths::build_dir(arch).join("serial.log");
    let mut remaining = lines.iter();
    for want in steps {
        if !remaining.any(|line| says(line, want)) {
            let failed = lines.iter().find(|line| line.contains("rtsched: FAILED"));
            return Err(Error::new(format!(
                "{arch}: the real-time test did not print `{want}` in order{}.\n  \
                 Serial output is in {}",
                failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                log.display()
            )));
        }
    }
    if status(lines) != Some(0) {
        return Err(Error::new(format!(
            "{arch}: the real-time test did not exit with 0.\n  Serial output is in {}",
            log.display()
        )));
    }
    Ok(())
}

/// The test on every architecture asked for.
///
/// Verifies: L.sched.112, L.sched.113, L.sched.116
fn test_rtsched(args: &Args) -> Result<()> {
    let built = crate::flags::this_run()?.value("sched_realtime") == Some("fifo_rr");
    for arch in args.arches()? {
        if !built {
            let program = build_test("rtsched", arch, Some("present"), false)?;
            require(arch, &boot(arch, &program, args)?, ABSENT_STEPS)?;
            println!("  {arch}: no realtime tier: SCHED_FIFO and SCHED_RR are EINVAL, exit 0");
            continue;
        }
        let program = build_test("rtsched", arch, None, false)?;
        require(arch, &boot(arch, &program, args)?, STEPS)?;
        println!(
            "  {arch}: SCHED_FIFO and SCHED_RR taken and read back, Linux's refusals, the \
             permission rule, inherited by a fork and a thread, reset on fork, exit 0"
        );
        let negative = build_test("rtsched", arch, Some("negative-control"), false)?;
        let lines = boot(arch, &negative, args)?;
        let failed_there = lines.iter().any(|line| line.contains(NEGATIVE_FAILURE));
        let passed = lines.iter().any(|line| says(line, "rtsched: inherit ok"));
        if !failed_there || passed || status(&lines) != Some(1) {
            return Err(Error::new(format!(
                "{arch}: the negative control should fail with `{NEGATIVE_FAILURE}` and exit 1, \
                 and did not.\n  Serial output is in {}",
                paths::build_dir(arch).join("serial.log").display()
            )));
        }
        println!("  {arch}: the negative control failed on the inherit step, as it must");
    }
    Ok(())
}
