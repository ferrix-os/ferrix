//! `test-procfs`: `/proc` as the Steam client's helpers read it.
//!
//! The program is `tests/procfs/`, built for each architecture's musl target
//! -- with `--i686`, x86-64's is 32-bit x86, as the Steam client is -- and
//! booted as init. For a TCP socket, a pair of `AF_UNIX` ones, an eventfd, an
//! epoll set, a pipe and a memfd it requires `stat` through
//! `/proc/self/fd/<n>` to be the descriptor's `fstat`, `lstat` the link, and
//! the link to open again only where Linux lets it; and `/proc/net/tcp`'s
//! row for the listening port to carry the socket's inode number, which is
//! how `lsof -i` finds the process that Steam asks it for. Then it lists
//! `/proc` recursively and requires every inode number to fit 32 bits and
//! be a name's own, and every entry's offset to fit a 32-bit `off_t`. It
//! prints a line per step and `procfs: all ok`, and exits 0; this requires
//! every one of those lines, in order, and the status.
//!
//! Then three negative controls: the program built to `lstat` the links
//! where it means `stat` must fail on the first link step, built to hold
//! inode numbers to 16 bits must fail on the inode step, and built to hold
//! offsets to 16 bits must fail there at `/proc/1`. A check that could not
//! fail would pass those builds too.

use crate::args::Args;
use crate::paths;
use crate::sem::{boot, build_test, says, status};
use crate::{Error, Result};

/// The lines a working run prints, in order.
const STEPS: &[&str] = &[
    "procfs: tcp ok",
    "procfs: unix ok",
    "procfs: eventfd ok",
    "procfs: epoll ok",
    "procfs: pipe ok",
    "procfs: memfd ok",
    "procfs: inodes ok",
    "procfs: all ok",
];

/// Each negative control: the feature it is built with, the start of the line
/// it must fail with, and the step line it must not print.
const NEGATIVE: &[(&str, &str, &str)] = &[
    (
        "negative-fd",
        "procfs: FAILED tcp: stat through /proc/self/fd/",
        "procfs: tcp ok",
    ),
    (
        "negative-ino",
        "procfs: FAILED inodes: getdents64 lists /proc/",
        "procfs: inodes ok",
    ),
    (
        "negative-off",
        "procfs: FAILED inodes: getdents64 lists /proc/1 with offset",
        "procfs: inodes ok",
    ),
];

/// The test on every architecture asked for.
pub(crate) fn test_procfs(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");

        let program = build_test("procfs", arch, None, args.i686)?;
        let lines = boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| says(line, want)) {
                let failed = lines.iter().find(|line| line.contains("procfs: FAILED"));
                return Err(Error::new(format!(
                    "{arch}: the /proc test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{arch}: the /proc test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        let counted = lines
            .iter()
            .find(|line| line.contains("names under /proc"))
            .map_or("", |line| line.trim());
        println!(
            "  {arch}: /proc/self/fd links stat as fstat for sockets, anonymous files, a pipe and \
             a memfd, /proc/net/tcp's inode matches, every /proc inode fits 32 bits: {counted}"
        );

        for (feature, failure, passed) in NEGATIVE {
            let negative = build_test("procfs", arch, Some(feature), args.i686)?;
            let lines = boot(arch, &negative, args)?;
            let failed_there = lines.iter().any(|line| line.contains(failure));
            let passed_it = lines.iter().any(|line| says(line, passed));
            if !failed_there || passed_it || status(&lines) != Some(1) {
                return Err(Error::new(format!(
                    "{arch}: the {feature} control should fail with `{failure}` and exit 1, \
                     and did not.\n  Serial output is in {}",
                    log.display()
                )));
            }
            println!("  {arch}: the {feature} control failed where it must");
        }
    }
    Ok(())
}
