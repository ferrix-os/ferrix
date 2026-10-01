//! `test-container`: stage 13's exit criterion as a program.
//!
//! The program is `src/tests/container/`, built for each architecture's musl
//! target and booted as init. As an unprivileged user it makes a user
//! namespace and the other seven kinds, runs a process that is pid 1 in its
//! pid namespace under a cgroup whose `memory.max` ends it with `SIGKILL`
//! when it touches four times the limit, blocks a system call with a seccomp
//! filter and has another process killed by one. It prints a line per step and
//! `container: all ok`, and exits 0; this requires every one of those lines, in
//! order, and the status.
//!
//! Then the negative control: the same program without the filter must fail on
//! the seccomp step. A check that could not fail would pass that build too.

use crate::args::Args;
use crate::paths;
use crate::sem::{boot, build_test, says, status};
use crate::{Error, Result};

/// The lines a working run prints, in order.
const STEPS: &[&str] = &[
    "container: delegation ok",
    "container: pid ok",
    "container: namespaces ok",
    "container: ids ok",
    "container: seccomp ok",
    "container: memory ok",
    "container: isolation ok",
    "container: all ok",
];

/// The line the negative control must fail with.
const NEGATIVE_FAILURE: &str = "container: FAILED a call the filter blocks was not refused EPERM";

/// The test on every architecture asked for.
pub(crate) fn run(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");

        let program = build_test("container", arch, None, false)?;
        let lines = boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| says(line, want)) {
                let failed = lines.iter().find(|line| line.contains("container: FAILED"));
                return Err(Error::new(format!(
                    "{arch}: the container test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{arch}: the container test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!(
            "  {arch}: an unprivileged user ran a pid 1 in eight new namespaces under a memory \
             limit that ended it, with a seccomp filter that blocked a call, exit 0"
        );

        let negative = build_test("container", arch, Some("negative-control"), false)?;
        let lines = boot(arch, &negative, args)?;
        let failed_there = lines.iter().any(|line| says(line, NEGATIVE_FAILURE));
        let passed_seccomp = lines.iter().any(|line| says(line, "container: seccomp ok"));
        if !failed_there || passed_seccomp || status(&lines) != Some(1) {
            return Err(Error::new(format!(
                "{arch}: the negative control should fail with `{NEGATIVE_FAILURE}` and exit 1, \
                 and did not.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!("  {arch}: the negative control failed on the seccomp step, as it must");
    }
    Ok(())
}
