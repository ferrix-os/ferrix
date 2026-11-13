//! `test-release-chain`: the case that decides how deep a process's release
//! recurses (`check-stack-depth`'s recursion row, ledger 9275 K3).
//!
//! `Process::release` reaches other processes through the signals it sends:
//! its parent's exit signal and, for a pid namespace's init, `SIGKILL` to
//! every member. A fatal signal ends its target, which is released at once,
//! on the sender's stack, when no thread of it is live. The walk cannot
//! bound that, so this boots `src/tests/releasechain` as init: a chain of
//! processes each killed by its child's exit signal, and pid namespaces
//! nested to their limit with the first init killed. The kernel says each
//! new deepest nesting of releases on one stack (`release: N releases ran
//! one inside another ...`); the row reads the deepest and holds it to
//! [`MOST_NESTED`]. Before releases were owed to the outermost one, a
//! target's own exit racing its killer's look at its live count nested them,
//! two deep in one run of four on x86-64 and ARMv7-A, with no bound in the
//! code.

use crate::args::Args;
use crate::{Error, Result, paths, sem};

/// The lines the program prints when every step passes.
const STEPS: &[&str] = &[
    "releasechain: chain ok",
    "releasechain: pidns ok",
    "releasechain: all ok",
];

/// What the kernel says at a new deepest nesting of releases.
const NESTED: &str = "release: ";

/// The most releases that may run one inside another on one stack: one, as
/// a release reached from another is owed to the outermost one on its thread
/// and run after it (`Process::release`).
const MOST_NESTED: u32 = 1;

/// The deepest nesting the kernel said, and the bytes it took, if any.
fn deepest(lines: &[String]) -> Option<(u32, u64)> {
    lines
        .iter()
        .filter_map(|line| {
            let at = line.find(NESTED)?;
            let mut words = line.get(at + NESTED.len()..)?.split_whitespace();
            let depth = words.next()?.parse().ok()?;
            let bytes = words.find_map(|word| word.parse().ok())?;
            Some((depth, bytes))
        })
        .max()
}

/// `test-release-chain`.
///
/// # Errors
///
/// A build or boot that fails, a step missing from the output, a status
/// other than 0, or releases nested deeper than [`MOST_NESTED`].
pub(crate) fn test_release_chain(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");
        let program = sem::build_test("releasechain", arch, None, false)?;
        let lines = sem::boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| sem::says(line, want)) {
                let failed = lines
                    .iter()
                    .find(|line| line.contains("releasechain: FAILED"));
                return Err(Error::new(format!(
                    "{arch}: the release chain test did not print `{want}` in order{}.\n  Serial \
                     output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if sem::status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{arch}: the release chain test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        let said = match deepest(&lines) {
            Some((depth, bytes)) if depth > MOST_NESTED => {
                return Err(Error::new(format!(
                    "{arch}: {depth} releases ran one inside another on one stack, {bytes} bytes \
                     below the first; at most {MOST_NESTED} may.\n  Serial output is in {}",
                    log.display()
                )));
            }
            Some((depth, bytes)) => {
                format!("{depth} releases at most one inside another, {bytes} bytes")
            }
            None => "no release ran inside another".to_owned(),
        };
        println!(
            "  {arch}: a chain of processes each killed by its child's exit signal, and pid \
             namespaces nested to their limit with the first init killed, all gone; {said}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::deepest;

    #[test]
    fn reads_the_deepest_nesting_said() {
        let lines = vec![
            " 3.1 | release: 2 releases ran one inside another on one stack, the last 900 bytes below the first".to_owned(),
            " 3.2 | release: 3 releases ran one inside another on one stack, the last 1800 bytes below the first".to_owned(),
            " 3.3 | releasechain: chain ok".to_owned(),
        ];
        assert_eq!(deepest(&lines), Some((3, 1800)));
        assert_eq!(deepest(&lines[2..]), None);
    }
}
