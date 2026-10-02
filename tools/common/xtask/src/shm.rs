//! `test-shm`: System V shared memory as Steam's web helper uses it.
//!
//! The program is `src/tests/shm/`, built for each architecture's musl target
//! -- with `--i686`, x86-64's is 32-bit x86, whose musl reaches shared memory
//! through `ipc` (117), as the 32-bit Steam client's glibc does, and which
//! also calls the direct numbers 395 to 398 -- and booted as init. It runs
//! Chromium's MIT-SHM sequence between a web helper of uid 1000 and a root
//! X server: an 8 MiB segment of mode 0600 made, attached in both, written
//! in one and read in the other, removed while both have it attached, and
//! gone with the last detach. Then a stranger is refused root's segment, a
//! fork shares and counts an attach, and the attach address rules hold. It
//! prints a line per step and `shm: all ok`, and exits 0; this requires
//! every one of those lines, in order, and the status.
//!
//! Then the negative control: the same program with the X server's last
//! `shmdt` left out must fail on the "gone" check. A removal check that
//! could not fail would pass that build too.

use crate::args::Args;
use crate::paths;
use crate::sem::{boot, build_test, says, status};
use crate::{Error, Result};

/// The lines every run prints, in order.
const STEPS: &[&str] = &[
    "shm: info ok",
    "shm: chromium ok",
    "shm: permission ok",
    "shm: fork ok",
    "shm: address ok",
];

/// The line a 32-bit x86 run prints after them.
const DIRECT: &str = "shm: direct ok";

/// The last line.
const ALL: &str = "shm: all ok";

/// The start of the line the negative control must fail with.
const NEGATIVE_FAILURE: &str = "shm: FAILED chromium: the segment outlived its last detach";

/// The test on every architecture asked for.
pub(crate) fn test_shm(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");

        let program = build_test("shm", arch, None, args.i686)?;
        let lines = boot(arch, &program, args)?;
        let mut wanted: Vec<&str> = STEPS.to_vec();
        if args.i686 {
            wanted.push(DIRECT);
        }
        wanted.push(ALL);
        let mut remaining = lines.iter();
        for want in wanted {
            if !remaining.any(|line| says(line, want)) {
                let failed = lines
                    .iter()
                    .find(|line| line.contains("shm: FAILED") || line.contains("shm: the "));
                return Err(Error::new(format!(
                    "{arch}: the shared memory test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{arch}: the shared memory test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!(
            "  {arch}: an 8 MiB segment made by uid 1000, attached by root, read across the \
             processes, removed while attached and gone with its last detach; a stranger \
             refused, a fork counted, the attach address rules kept{}, exit 0",
            if args.i686 {
                "; ipc(117) and 395-398 both"
            } else {
                ""
            }
        );

        let negative = build_test("shm", arch, Some("negative-control"), args.i686)?;
        let lines = boot(arch, &negative, args)?;
        let failed_there = lines.iter().any(|line| line.contains(NEGATIVE_FAILURE));
        let passed_it = lines.iter().any(|line| says(line, "shm: chromium ok"));
        if !failed_there || passed_it || status(&lines) != Some(1) {
            return Err(Error::new(format!(
                "{arch}: the negative control should fail with `{NEGATIVE_FAILURE}` and exit 1, \
                 and did not.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!("  {arch}: the negative control failed on the removal step, as it must");
    }
    Ok(())
}
