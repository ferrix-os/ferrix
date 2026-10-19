//! `bench-rows`: Microkernel M0's Linux rows, timed (`docs/MICROKERNEL.md`
//! §7, `docs/roadmap/microkernel-m0-baselines.md`).
//!
//! Boots `src/tests/linux-rows/`, a static musl program, as init under
//! `bench-pipe`'s machine: KVM, one processor unless `--smp` says otherwise,
//! QEMU pinned to host processor 11 unless `--pin` says otherwise, the
//! gate's processor model ([`crate::qemu::x86_cpu`]). Where the rustc volume
//! is fetched it is attached at `/data`, as `test-rustc` attaches it, for
//! the page-cache row. The program prints the bare `getppid` (by the per-call
//! p50 and by `clockbench`'s mean), `fork` + `exec` + `wait` of a static
//! program, 4 KiB `mmap`/`munmap` churn, a 4 KiB page-cache miss, a signal
//! to the running thread and a futex wake between two threads, each the
//! median of five repetitions' p50s, in nanoseconds; and the speculation
//! bits CPUID shows the guest. The kernel's own `seam` boot line in the
//! same boot gives the 4 KiB block read's row, its depth-1 p50.
//!
//! `--rounds` boots that many times (default 5) and prints each row's median
//! over the boots with its range. `--fail-above <row>=<ns>` (once a row) is
//! the row's fail rule: the run fails when that median is above it, which is
//! what a negative control's injected delay must make happen.

use crate::args::Args;
use crate::ipc::{Host, PIN, host_processors};
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, initramfs, native, ports, qemu, rustc, sem, shell};
use std::path::Path;

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "bench-rows",
    help_at: 425,
    help: &[
        "bench-rows    Boot src/tests/linux-rows as init --rounds times (5) and time Microkernel M0's Linux rows:",
        "              getppid, fork+exec+wait, mmap churn, a page-cache miss, a signal, a futex wake",
    ],
    run: |_, args| bench_rows(args),
}];

/// The guest's memory in MiB, unless `--memory` says.
const MEMORY: u32 = 4096;

/// The rows the program prints, in its order.
const ROWS: &[&str] = &[
    "pagecache-miss-4K",
    "null-getppid",
    "getppid-mean",
    "mmap-munmap-4K",
    "mmap-touch-munmap-4K",
    "signal-self",
    "futex-thread-pingpong",
    "fork-exit-wait",
    "fork-exec-wait",
    // The kernel's own row, from its boot line rather than the program's:
    // the depth-1 p50 of the seam's 4 KiB reads through the block ring and
    // the ring-3 driver (`block_ring::hop_check`).
    BLOCK_ROW,
];

/// The row read from the kernel's `seam` boot line.
const BLOCK_ROW: &str = "block-read-4K";

/// Boot `--rounds` times, print every row's p50 a boot and their median,
/// and apply `--fail-above`.
///
/// # Errors
///
/// A build or boot that fails, a program that did not print `LB done`, a
/// `--fail-above` that names no row or is not `<row>=<ns>`, or a row whose
/// median is above its `--fail-above`.
pub(crate) fn bench_rows(args: &Args) -> Result<()> {
    let limits = limits(&args.fail_above, ROWS, "bench-rows")?;
    let mut args = args.clone();
    if !args.smp_given {
        args.smp = 1;
    }
    if args.pin.is_none() && host_processors() > 12 {
        args.pin = Some(PIN.to_owned());
    }
    // Room for fork+exec+wait's 5,100 children: free memory fell by about
    // 344 KiB with each, reaped or not (2026-10-13, m0b-smoke3), which ran
    // a 512 MiB guest out of memory at the 1,317th (m0b-smoke2). `LB
    // meminfo` says how much each takes.
    if !args.memory_given {
        args.memory = MEMORY;
    }
    let boots = args.rounds.unwrap_or(5);
    for arch in args.arches()? {
        if arch == Arch::X86_64 && args.data_image.is_none() {
            match rustc::volume() {
                Ok(volume) => args.data_image = Some(volume),
                Err(error) => println!("  {arch}: no page-cache row: {error}"),
            }
        }
        if arch == Arch::X86_64 {
            let accel = args.accel.as_deref().unwrap_or("kvm");
            println!(
                "  {arch}: processor model under {accel}: {}",
                qemu::x86_cpu(accel)
            );
        }
        let program = sem::build_test("linux-rows", arch, None, false)?;
        let log = paths::build_dir(arch).join("serial.log");
        let mut figures: Vec<Vec<u64>> = vec![Vec::new(); ROWS.len()];
        for boot in 1..=boots {
            let host = Host::before(args.pin.as_deref());
            let lines = boot_once(arch, &program, &args)?;
            println!("  {arch}: boot {boot}: {}", host.after());
            if let Some(error) = lines.iter().find(|line| line.contains("LB error")) {
                return Err(Error::new(format!(
                    "{arch}: linux-rows failed: `{}`.\n  Serial output is in {}",
                    error.trim(),
                    log.display()
                )));
            }
            if !lines
                .iter()
                .any(|line| line.trim_end().ends_with("LB done"))
            {
                return Err(Error::new(format!(
                    "{arch}: linux-rows did not finish.\n  Serial output is in {}",
                    log.display()
                )));
            }
            for line in lines.iter().filter(|line| {
                ["LB cpuid", "LB pagecache reads", "LB meminfo", "LB inject"]
                    .iter()
                    .any(|key| line.contains(key))
            }) {
                println!("  {arch}: boot {boot}: {}", line.trim());
            }
            for (row, list) in ROWS.iter().zip(figures.iter_mut()) {
                let p50 = if *row == BLOCK_ROW {
                    seam_p50(&lines)
                } else {
                    crate::pipe_bench::summary(&lines, row)
                };
                println!(
                    "  {arch}: boot {boot}: row {row} p50 {} ns",
                    p50.map_or_else(|| "?".to_owned(), |ns| ns.to_string())
                );
                list.extend(p50);
            }
        }
        let mut over = Vec::new();
        for (row, list) in ROWS.iter().zip(figures.iter_mut()) {
            list.sort_unstable();
            let median = list.get(list.len() / 2).copied();
            println!(
                "  {arch}: row {row} p50 {} ns, the median of {} boots ({} to {})",
                median.map_or_else(|| "?".to_owned(), |ns| ns.to_string()),
                list.len(),
                list.first().copied().unwrap_or(0),
                list.last().copied().unwrap_or(0)
            );
            if let Some(&(_, limit)) = limits.iter().find(|(name, _)| name == row) {
                match median {
                    Some(median) if median > limit => over.push(format!(
                        "row {row}: median p50 {median} ns is over its fail line {limit} ns"
                    )),
                    Some(median) => println!(
                        "  {arch}: row {row}: median p50 {median} ns is within its fail line {limit} ns"
                    ),
                    None => over.push(format!("row {row}: no figure to hold to {limit} ns")),
                }
            }
        }
        if !over.is_empty() {
            return Err(Error::new(format!("{arch}: {}", over.join("; "))));
        }
    }
    Ok(())
}

/// The depth-1 p50 of the kernel's `seam` boot line, in ns: "seam  a 4 KiB
/// read through the ring: depth 1 mean 62.4 p50 62.7 p99 83.8 us, ...".
fn seam_p50(lines: &[String]) -> Option<u64> {
    lines.iter().find_map(|line| {
        let at = line.find("read through the ring: depth 1 ")?;
        let mut words = line.get(at..)?.split_whitespace();
        let _ = words.position(|word| word == "p50")?;
        let us: f64 = words.next()?.parse().ok()?;
        // A boot line's microseconds to whole nanoseconds; a figure that is
        // not finite and positive is no figure.
        (us.is_finite() && us > 0.0).then(|| (us * 1000.0).round() as u64)
    })
}

/// `--fail-above`'s `<row>=<number>` pairs, each naming one of `rows`, the
/// rows of `command`.
fn limits(given: &[String], rows: &[&str], command: &str) -> Result<Vec<(String, u64)>> {
    given
        .iter()
        .map(|pair| {
            let (row, ns) = pair.split_once('=').ok_or_else(|| {
                Error::new(format!("--fail-above wants <row>=<number>, not `{pair}`"))
            })?;
            if !rows.contains(&row) {
                return Err(Error::new(format!(
                    "--fail-above names `{row}`, which is no row of {command} ({})",
                    rows.join(", ")
                )));
            }
            let ns = ns.parse().map_err(|_| {
                Error::new(format!(
                    "--fail-above {row} wants a whole number, got `{ns}`"
                ))
            })?;
            Ok((row.to_owned(), ns))
        })
        .collect()
}

/// One wall-clock row of a gate whose script stamps `/proc/uptime` as
/// `rows-time <label> <seconds>`: the time from the stamp `from` to the stamp
/// `to`, divided by the `runs` of the timed work between them, in ms.
#[derive(Debug)]
pub(crate) struct Span {
    /// The row's name, which `--fail-above` takes.
    pub(crate) row: &'static str,
    /// The stamp it starts at.
    pub(crate) from: &'static str,
    /// The stamp it ends at.
    pub(crate) to: &'static str,
    /// How many runs of the work lie between the two.
    pub(crate) runs: u64,
}

/// The names of `spans`' rows.
fn span_rows(spans: &[Span]) -> Vec<&'static str> {
    spans.iter().map(|span| span.row).collect()
}

/// Refuse a `--fail-above` that names none of `spans`' rows, before a gate's
/// boot rather than after it.
///
/// # Errors
///
/// A pair that is not `<row>=<ms>` or names no row of `command`.
pub(crate) fn check_wall_limits(given: &[String], spans: &[Span], command: &str) -> Result<()> {
    let _ = limits(given, &span_rows(spans), command)?;
    Ok(())
}

/// The stamp `label` the script printed, in centiseconds (`/proc/uptime`'s
/// step), from the line the shell ran rather than the one it echoed: the
/// echo has `$t` where the value goes, which parses as no number.
fn stamp(lines: &[String], label: &str) -> Option<u64> {
    lines.iter().find_map(|line| {
        let at = line.find("rows-time ")?;
        let mut words = line.get(at..)?.split_whitespace().skip(1);
        if words.next()? != label {
            return None;
        }
        let (seconds, hundredths) = words.next()?.split_once('.')?;
        let seconds: u64 = seconds.parse().ok()?;
        let hundredths: u64 = hundredths.parse().ok()?;
        (hundredths < 100).then(|| seconds.saturating_mul(100).saturating_add(hundredths))
    })
}

/// Print every row of `spans` from `lines`, in ms, and hold each to its
/// `--fail-above` (in ms).
///
/// # Errors
///
/// A row whose stamps are missing while it has a fail line, or whose figure
/// is above its fail line.
pub(crate) fn wall_rows(
    arch: Arch,
    lines: &[String],
    spans: &[Span],
    given: &[String],
    command: &str,
) -> Result<()> {
    let limits = limits(given, &span_rows(spans), command)?;
    let mut over = Vec::new();
    for span in spans {
        let figure = stamp(lines, span.from)
            .zip(stamp(lines, span.to))
            .map(|(from, to)| to.saturating_sub(from).saturating_mul(10) / span.runs.max(1));
        match figure {
            Some(ms) => println!(
                "  {arch}: row {} {ms} ms ({} to {}, {} run(s), /proc/uptime's 10 ms steps)",
                span.row, span.from, span.to, span.runs
            ),
            None => println!(
                "  {arch}: row {}: no stamps {} and {}",
                span.row, span.from, span.to
            ),
        }
        if let Some(&(_, limit)) = limits.iter().find(|(name, _)| name == span.row) {
            match figure {
                Some(ms) if ms > limit => over.push(format!(
                    "row {}: {ms} ms is over its fail line {limit} ms",
                    span.row
                )),
                Some(ms) => println!(
                    "  {arch}: row {}: {ms} ms is within its fail line {limit} ms",
                    span.row
                ),
                None => over.push(format!("row {}: no figure to hold to {limit} ms", span.row)),
            }
        }
    }
    if over.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!("{arch}: {}", over.join("; "))))
    }
}

/// Boot `program` as init with `rows-true` carried at `/bin/rows-true`, and
/// return every line once init has exited.
fn boot_once(arch: Arch, program: &Path, args: &Args) -> Result<Vec<String>> {
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, program, shell::SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let read = |path: &Path| {
        std::fs::read(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
    };
    let carried = vec![ports::File {
        path: "bin/rows-true".to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(read(&program.with_file_name("rows-true"))?),
    }];
    let initramfs = initramfs::build(None, &natives, None, &carried)?;
    let cmdline = crate::image_cmdline(args);
    let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, cmdline.as_deref())?;
    qemu::watch_lines(arch, &image, &kernel, args, shell::EXITED)
}

#[cfg(test)]
mod tests {
    use super::limits;

    #[test]
    fn the_seam_line_gives_its_depth_one_p50() {
        let line = "   18.55 |   seam     a 4 KiB read through the ring: depth 1 mean 62.4 p50 \
                    62.7 p99 83.8 us, the driver's own; depth 32 mean 723.6 p50 266.0 p99"
            .to_owned();
        assert_eq!(super::seam_p50(&[line]), Some(62_700));
        assert_eq!(super::seam_p50(&["seam".to_owned()]), None);
    }

    #[test]
    fn a_fail_line_names_a_row_and_a_number() {
        let rows = super::ROWS;
        let parsed = limits(&["null-getppid=393".to_owned()], rows, "bench-rows");
        assert_eq!(parsed.ok(), Some(vec![("null-getppid".to_owned(), 393)]));
        assert!(limits(&["no-such-row=1".to_owned()], rows, "bench-rows").is_err());
        assert!(limits(&["null-getppid".to_owned()], rows, "bench-rows").is_err());
        assert!(limits(&["null-getppid=fast".to_owned()], rows, "bench-rows").is_err());
    }

    #[test]
    fn a_wall_row_is_read_from_the_stamps_the_shell_ran() {
        let lines: Vec<String> = [
            "$ echo \"rows-time warm $t\"",
            "rows-time warm 12.34",
            "rows-time warmed 13.04",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        assert_eq!(super::stamp(&lines, "warm"), Some(1234));
        assert_eq!(super::stamp(&lines, "warmed"), Some(1304));
        assert_eq!(super::stamp(&lines, "cold"), None);
        let spans = [super::Span {
            row: "warm-rustc-hello",
            from: "warm",
            to: "warmed",
            runs: 5,
        }];
        let arch = crate::paths::Arch::X86_64;
        let within = ["warm-rustc-hello=140".to_owned()];
        assert!(super::wall_rows(arch, &lines, &spans, &within, "test-rustc").is_ok());
        let over = ["warm-rustc-hello=139".to_owned()];
        assert!(super::wall_rows(arch, &lines, &spans, &over, "test-rustc").is_err());
        assert!(super::check_wall_limits(&["cold=1".to_owned()], &spans, "test-rustc").is_err());
    }
}
