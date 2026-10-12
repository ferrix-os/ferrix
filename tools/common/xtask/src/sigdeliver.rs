//! `test-sigdeliver`: what the kernel's `sigpaths` boot check proves at the
//! delivery decision, shown end to end by a user program.
//!
//! The program is `src/tests/sigdeliver/`, a static musl program built for each
//! architecture and booted as init, as `test-sem`'s is: a `SIGSEGV` from a real
//! fault caught on the alternate stack and recovered from, a stack overflow
//! caught there, and a pipe read interrupted by `SIGALRM` and by another
//! thread's `SIGUSR2`, restarted under `SA_RESTART` and `EINTR` without it.
//! It prints a line per step and `sigdeliver: all ok`, and exits 0; this
//! requires every line, in order, and the status.

use crate::args::Args;
use crate::paths;
use crate::sem;
use crate::{Error, Result};

/// The lines a working run prints, in order.
const STEPS: &[&str] = &[
    "sigdeliver: altstack ok",
    "sigdeliver: overflow ok",
    "sigdeliver: alarm-restart ok",
    "sigdeliver: alarm-eintr ok",
    "sigdeliver: thread-restart ok",
    "sigdeliver: thread-eintr ok",
    "sigdeliver: all ok",
];

/// The test on every architecture asked for.
///
/// # Errors
///
/// A build or boot that fails, a step missing from the output (the error
/// quotes the program's `FAILED` line, which names the step and what went
/// wrong), or a status other than 0.
pub(crate) fn test_sigdeliver(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");
        let program = sem::build_test("sigdeliver", arch, None, false)?;
        let lines = sem::boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| sem::says(line, want)) {
                let failed = lines
                    .iter()
                    .find(|line| line.contains("sigdeliver: FAILED"));
                return Err(Error::new(format!(
                    "{arch}: the signal delivery test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if sem::status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{arch}: the signal delivery test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!(
            "  {arch}: a SIGSEGV was caught on the alternate stack and recovered from, a stack \
             overflow was caught there, and a pipe read interrupted by an alarm and by another \
             thread restarted under SA_RESTART and was EINTR without it, exit 0"
        );
    }
    Ok(())
}
