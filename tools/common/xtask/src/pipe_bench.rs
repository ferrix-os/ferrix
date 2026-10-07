//! `bench-pipe`: Linux-ABI round trips between two processes, timed.
//!
//! Most programs on Ferrix -- everything linked against ferrousli or musl,
//! Chrome, the desktop -- talk through pipes, Unix sockets and futexes, not
//! the native channel `bench-ipc` times. This boots `src/tests/pipe-bench/`,
//! a static musl program, as init, under `bench-ipc`'s machine: KVM, one
//! processor unless `--smp` says otherwise, QEMU pinned to host processor 11
//! unless `--pin` says otherwise. It prints the system call floor and a pipe,
//! a Unix stream socket and a futex ping-pong of 8 bytes, each the median of
//! five repetitions of 20,000 round trips, in nanoseconds.
//!
//! The program is the in-tree form of the `lbench` that measured Linux's own
//! figures under the same QEMU line, so the same binary can be run as a Linux
//! guest's `/init` for the like-for-like comparison. `--rounds` boots that
//! many times (default 5) and prints each test's median over the boots.

use crate::args::Args;
use crate::ipc::{Host, PIN, host_processors};
use crate::paths::Arch;
use crate::{Error, Result, cargo, fat, initramfs, native, paths, ports, qemu, sem, shell};
use std::path::Path;

/// The tests the program runs, in its order.
const TESTS: &[&str] = &[
    "null-getppid",
    "pipe-pingpong-8B",
    "unix-stream-pingpong-8B",
    "futex-pingpong",
    // Again, the two processes born in one speculation domain: the matched
    // configuration, without the predictor barrier between them.
    "domain-null-getppid",
    "domain-pipe-pingpong-8B",
    "domain-unix-stream-pingpong-8B",
    "domain-futex-pingpong",
];

/// Boot `--rounds` times and print every test's p50 a boot, and their median.
///
/// # Errors
///
/// A build or boot that fails, or a program that did not print `LB done`.
pub(crate) fn bench_pipe(args: &Args) -> Result<()> {
    let mut args = args.clone();
    if !args.smp_given {
        args.smp = 1;
    }
    if args.pin.is_none() && host_processors() > 12 {
        args.pin = Some(PIN.to_owned());
    }
    let boots = args.rounds.unwrap_or(5);
    for arch in args.arches()? {
        let program = sem::build_test("pipe-bench", arch, None, false)?;
        let log = paths::build_dir(arch).join("serial.log");
        let mut figures: Vec<Vec<u64>> = vec![Vec::new(); TESTS.len()];
        for boot in 1..=boots {
            let host = Host::before(args.pin.as_deref());
            let lines = boot_once(arch, &program, &args)?;
            println!("  {arch}: boot {boot}: {}", host.after());
            if let Some(error) = lines.iter().find(|line| line.contains("LB error")) {
                return Err(Error::new(format!(
                    "{arch}: pipe-bench failed: `{}`.\n  Serial output is in {}",
                    error.trim(),
                    log.display()
                )));
            }
            if !lines
                .iter()
                .any(|line| line.trim_end().ends_with("LB done"))
            {
                return Err(Error::new(format!(
                    "{arch}: pipe-bench did not finish.\n  Serial output is in {}",
                    log.display()
                )));
            }
            for (test, list) in TESTS.iter().zip(figures.iter_mut()) {
                let p50 = summary(&lines, test);
                println!(
                    "  {arch}: boot {boot}: pipe-bench {test} p50 {} ns",
                    p50.map_or_else(|| "?".to_owned(), |ns| ns.to_string())
                );
                list.extend(p50);
            }
        }
        for (test, list) in TESTS.iter().zip(figures.iter_mut()) {
            list.sort_unstable();
            let median = list.get(list.len() / 2).copied();
            println!(
                "  {arch}: pipe-bench {test} p50 {} ns, the median of {} boots ({} to {})",
                median.map_or_else(|| "?".to_owned(), |ns| ns.to_string()),
                list.len(),
                list.first().copied().unwrap_or(0),
                list.last().copied().unwrap_or(0)
            );
        }
    }
    Ok(())
}

/// Where the program is carried in the initramfs as well as being init: the
/// built-in init has no file of its own, and the domain run makes a process
/// from the program's image, which it reads from here.
const CARRIED_AT: &str = "bin/pipe-bench";

/// Boot `program` as init with itself carried at [`CARRIED_AT`], and return
/// every line once init has exited.
fn boot_once(arch: Arch, program: &Path, args: &Args) -> Result<Vec<String>> {
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, program, shell::SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let image = std::fs::read(program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    let carried = vec![ports::File {
        path: CARRIED_AT.to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(image),
    }];
    let initramfs = initramfs::build(None, &natives, None, &carried)?;
    let cmdline = crate::image_cmdline(args);
    let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, cmdline.as_deref())?;
    qemu::watch_lines(arch, &image, &kernel, args, shell::EXITED)
}

/// The p50 in ns of `test`'s `LB summary` line.
fn summary(lines: &[String], test: &str) -> Option<u64> {
    let key = format!("LB summary name={test} ");
    lines.iter().find_map(|line| {
        let at = line.find(&key)?;
        line.get(at + key.len()..)?
            .split_whitespace()
            .find_map(|word| word.strip_prefix("p50_ns=")?.parse().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::summary;

    #[test]
    fn reads_the_summary_of_the_test_named_and_no_other() {
        let lines = vec![
            " 15.2 | LB summary name=pipe-pingpong-8B p50_ns=9340".to_owned(),
            " 15.3 | LB summary name=pipe-pingpong-8Bx p50_ns=1".to_owned(),
        ];
        assert_eq!(summary(&lines, "pipe-pingpong-8B"), Some(9340));
        assert_eq!(summary(&lines, "futex-pingpong"), None);
    }
}
