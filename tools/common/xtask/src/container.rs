//! `test-container`: stage 13's exit criterion as a program.
//!
//! The program is `src/tests/container/`, built for each architecture's musl
//! target and booted as init. As uid 1000, with no capability, it makes a user
//! namespace and six other kinds and runs a process that is pid 1 in its pid
//! namespace, in a cgroup whose `memory.max` makes the kernel reclaim the
//! clean pages of a file that process read and then end it with `SIGKILL`,
//! and not its parent in the same cgroup; that process blocks a system call
//! with a seccomp filter. The program prints a line per clause and
//! `container: all ok`, and exits 0; this requires every one of those lines,
//! in order, and the status.
//!
//! Each judged line has a negative control, a one-line sabotage of the
//! program run through the gate pool's `control --expect-line`, that makes
//! this fail on that line (`docs/CGROUPS.md` §15).

use crate::args::Args;
use crate::paths;
use crate::sem::{boot, build_test, says, status};
use crate::{Error, Result};

/// The lines a working run prints, in order: one a clause of the criterion,
/// between the cgroup's delegation and init's own look afterwards.
const STEPS: &[&str] = &[
    "container: delegation ok",
    "container: unprivileged ok",
    "container: pid ok",
    "container: namespaces ok",
    "container: ids ok",
    "container: seccomp ok",
    "container: reclaim ok",
    "container: oom ok",
    "container: isolation ok",
    "container: all ok",
];

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
            "  {arch}: uid 1000 with no capability made a user namespace and ran a pid 1 in seven \
             new namespaces, whose cgroup's memory.max reclaimed its file pages and then OOM-killed \
             it and not its parent, under a seccomp filter that blocked a call; exit 0"
        );
    }
    Ok(())
}
