//! `cargo xtask compare`: the tiny kernel against the hybrid, from one commit
//! and by compile-time flags alone (customer, 2026-11-03; `docs/BUILD-FLAGS.md`,
//! `docs/MICROKERNEL.md` §7).
//!
//! For each profile it reports what runs in ring 0, by the rings' product lines
//! (`check-item-boundary.py --report`) and by the kernel binary's own size. Then
//! it runs §7's rows on `desktop` and on the tiny profile, alternated, `--rounds`
//! boots a side (five, G8), by `bench-rows`' and `bench-ipc`'s own methods, and
//! prints them side by side.
//!
//! A shape not finished is said to be: a profile that does not build yet gets its
//! refusal in the table, never a figure, and its row reads `unfinished`. The hybrid's
//! figures print all the same, so the command is useful from today. Nothing here
//! fails because a ratio is over a limit: that is `--fail-above`'s, on the row itself.

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result, flags};
use std::path::{Path, PathBuf};
use std::process::Command;

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "compare",
    help_at: 426,
    help: &[
        "compare       Build every profile from this commit, report ring 0's lines and the kernel's",
        "              size per profile, and run Microkernel §7's rows on desktop and fast, --rounds",
        "              boots a side (5), alternated. Unfinished shapes are marked as unfinished",
    ],
    run: |_, args| compare(args),
}];

/// The profiles reported, smallest promise last.
const PROFILES: &[&str] = &["present", "desktop", "fast", "max"];

/// The hybrid, and the shape it is compared with.
const SIDES: [&str; 2] = ["desktop", "fast"];

/// What one profile's build came to.
struct Shape {
    /// The profile.
    name: &'static str,
    /// Product lines in ring 0: the core's and the item's, and the load's where
    /// `personality=kernel` keeps it there.
    ring0_lines: Option<u64>,
    /// The kernel binary's bytes, or why there are none.
    binary: std::result::Result<u64, String>,
}

/// `bench-rows`' and `bench-ipc`' figures for one side, by row, one per boot.
#[derive(Default)]
struct Figures(Vec<(String, Vec<u64>)>);

impl Figures {
    /// Note `ns` for `row`, in first-seen order.
    fn add(&mut self, row: &str, ns: u64) {
        if let Some((_, list)) = self.0.iter_mut().find(|(name, _)| name == row) {
            list.push(ns);
        } else {
            self.0.push((row.to_owned(), vec![ns]));
        }
    }

    /// `row`'s median over the boots, if it has one.
    fn median(&self, row: &str) -> Option<u64> {
        let (_, list) = self.0.iter().find(|(name, _)| name == row)?;
        median(list)
    }
}

/// The middle figure, the lower of two.
fn median(list: &[u64]) -> Option<u64> {
    let mut sorted = list.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len().checked_sub(1)? / 2).copied()
}

/// A `boot N: row NAME p50 M ns` line's row and figure, as `bench-rows` prints it.
fn row_line(line: &str) -> Option<(&str, u64)> {
    let rest = line.split(": row ").nth(1)?;
    let (row, rest) = rest.split_once(" p50 ")?;
    let ns = rest.strip_suffix(" ns")?.trim().parse().ok()?;
    Some((row, ns))
}

/// A `bench-ipc` boot's `domain-call` p50, from its `ipc-bench` line.
fn ipc_line(line: &str) -> Option<u64> {
    let rest = line.split("ipc-bench domain-call ").nth(1)?;
    rest.split_whitespace()
        .find_map(|word| word.strip_prefix("p50="))?
        .parse()
        .ok()
}

/// The figure after `key` in `check-item-boundary.py --report`'s size lines.
fn lines_after(report: &str, key: &str) -> Option<u64> {
    let line = report.lines().find(|line| line.contains(key))?;
    line.split(':')
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Run this xtask with `words`: its output, or why it failed.
fn xtask(words: &[String]) -> std::result::Result<String, String> {
    let program = std::env::current_exe().map_err(|error| error.to_string())?;
    let out = Command::new(program)
        .args(words)
        .current_dir(paths::workspace_root())
        .output()
        .map_err(|error| error.to_string())?;
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(text)
    } else {
        let last = text.lines().rev().find(|line| !line.trim().is_empty());
        Err(last.unwrap_or("it failed and said nothing").to_owned())
    }
}

/// `words` as owned strings.
fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| (*word).to_owned()).collect()
}

/// The ring-0 lines of `name`, from the report's lines of each ring.
fn ring0(report: &str, name: &str) -> Option<u64> {
    let item = lines_after(report, "certified item (core+item)")?;
    let load = lines_after(report, "uncertified load")?;
    let profile = flags::resolve_unbuilt(Some(name), &[]).ok()?;
    if profile.value("personality") == Some("kernel") {
        Some(item + load)
    } else {
        Some(item)
    }
}

/// The kernel binary a build's output names, its `sha256 ... (PATH)` line.
fn binary_of(output: &str) -> Option<PathBuf> {
    let line = output
        .lines()
        .rev()
        .find(|line| line.contains("kernel sha256"))?;
    let path = line.split('(').nth(1)?.trim_end().trim_end_matches(')');
    Some(Path::new(path).to_path_buf())
}

/// Build `name`, and read what it came to.
fn shape(name: &'static str, report: &str) -> Shape {
    let ring0_lines = ring0(report, name);
    let build = xtask(&words(&[
        "build",
        "--arch",
        "x86_64",
        "--release",
        "--kernel-profile",
        name,
    ]));
    let binary = build.and_then(|output| {
        let path = binary_of(&output).ok_or("the build named no kernel binary")?;
        std::fs::metadata(&path)
            .map(|meta| meta.len())
            .map_err(|error| format!("{}: {error}", path.display()))
    });
    Shape {
        name,
        ring0_lines,
        binary,
    }
}

/// One boot of `side`: its rows into `figures`, or why it did not run.
fn one_round(side: &str, figures: &mut Figures, args: &Args) -> std::result::Result<(), String> {
    let mut rows = words(&[
        "bench-rows",
        "--release",
        "--accel",
        "kvm",
        "--arch",
        "x86_64",
        "--rounds",
        "1",
        "--kernel-profile",
    ]);
    rows.push(side.to_owned());
    let mut ipc = words(&[
        "bench-ipc",
        "--release",
        "--accel",
        "kvm",
        "--smp",
        "1",
        "--arch",
        "x86_64",
        "--rounds",
        "1",
        "--kernel-profile",
    ]);
    ipc.push(side.to_owned());
    if let Some(init) = &args.init {
        ipc.push("--init".to_owned());
        ipc.push(init.clone());
    }
    let output = xtask(&rows)?;
    for (row, ns) in output.lines().filter_map(row_line) {
        figures.add(row, ns);
    }
    let output = if args.init.is_some() {
        xtask(&ipc)?
    } else {
        println!(
            "compare: {side}: round-trip-domain-call not run: bench-ipc needs --init <busybox>"
        );
        String::new()
    };
    for ns in output.lines().filter_map(ipc_line) {
        figures.add("round-trip-domain-call", ns);
    }
    Ok(())
}

/// The command.
fn compare(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    let root = paths::workspace_root();
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()?;
    println!(
        "compare: from one commit, {} on {arch}, by compile-time flags alone",
        String::from_utf8_lossy(&commit.stdout).trim()
    );
    let report = Command::new("python3")
        .arg("tools/common/check/check-item-boundary.py")
        .arg("--report")
        .current_dir(&root)
        .output()?;
    let report = String::from_utf8_lossy(&report.stdout).into_owned();
    println!(
        "\n{:<9} {:>14} {:>14}  shape",
        "profile", "ring-0 lines", "kernel bytes"
    );
    for name in PROFILES {
        let shape = shape(name, &report);
        let lines = shape
            .ring0_lines
            .map_or_else(|| "?".to_owned(), |lines| lines.to_string());
        match &shape.binary {
            Ok(bytes) => println!("{:<9} {lines:>14} {bytes:>14}  built", shape.name),
            Err(why) => println!(
                "{:<9} {lines:>14} {:>14}  unfinished: {why}",
                shape.name, "-"
            ),
        }
    }

    let rounds = args.rounds.unwrap_or(5);
    let mut sides = [Figures::default(), Figures::default()];
    let mut refused: [Option<String>; 2] = [None, None];
    for round in 1..=rounds {
        for (at, side) in SIDES.iter().enumerate() {
            if refused.get(at).is_some_and(Option::is_some) {
                continue;
            }
            println!("compare: round {round} of {rounds}, {side}");
            let (Some(figures), Some(slot)) = (sides.get_mut(at), refused.get_mut(at)) else {
                continue;
            };
            if let Err(why) = one_round(side, figures, args) {
                *slot = Some(why);
            }
        }
    }
    let [hybrid, tiny] = &sides;
    println!(
        "\n{:<26} {:>12} {:>12} {:>7}",
        "row (ns, median)", SIDES[0], SIDES[1], "ratio"
    );
    for (row, _) in &hybrid.0 {
        let a = hybrid.median(row);
        let b = tiny.median(row);
        let show =
            |value: Option<u64>| value.map_or_else(|| "unfinished".to_owned(), |ns| ns.to_string());
        let ratio = match (a, b) {
            (Some(a), Some(b)) if a != 0 => format!("{:.3}", b as f64 / a as f64),
            _ => "-".to_owned(),
        };
        println!("{row:<26} {:>12} {:>12} {ratio:>7}", show(a), show(b));
    }
    for (side, why) in SIDES.iter().zip(&refused) {
        if let Some(why) = why {
            println!("compare: {side} is unfinished: {why}");
        }
    }
    if hybrid.0.is_empty() {
        return Err(Error::new(
            "compare: the hybrid's rows did not run, so nothing was compared",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Figures, binary_of, ipc_line, lines_after, median, row_line};

    #[test]
    fn a_median_is_the_middle_figure() {
        assert_eq!(median(&[3, 1, 2]), Some(2));
        assert_eq!(median(&[4, 1, 2, 3]), Some(2));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn a_rows_line_is_read() {
        assert_eq!(
            row_line("  x86_64: boot 1: row null-getppid p50 357 ns"),
            Some(("null-getppid", 357))
        );
        assert_eq!(
            row_line("  x86_64: boot 1: row null-getppid p50 ? ns"),
            None
        );
        assert_eq!(row_line("  a line about nothing"), None);
    }

    #[test]
    fn a_round_trips_line_is_read() {
        assert_eq!(
            ipc_line("  x86_64: ipc-bench domain-call n=20000 min=749 p50=978 p90=1108"),
            Some(978)
        );
        assert_eq!(ipc_line("  x86_64: ipc-bench call n=1"), None);
    }

    #[test]
    fn the_reports_sizes_are_read() {
        let report = "  certified item (core+item): 111843 lines of product code\n  \
                      uncertified load:           82175 lines\n";
        assert_eq!(
            lines_after(report, "certified item (core+item)"),
            Some(111_843)
        );
        assert_eq!(lines_after(report, "uncertified load"), Some(82_175));
        assert_eq!(lines_after(report, "missing"), None);
    }

    #[test]
    fn a_builds_binary_is_read() {
        let output = "built x\n  x86_64 kernel sha256 ab12 (/t/ferrix-kernel)\n";
        assert_eq!(
            binary_of(output).as_deref(),
            Some(std::path::Path::new("/t/ferrix-kernel"))
        );
    }

    #[test]
    fn figures_keep_a_median_per_row() {
        let mut figures = Figures::default();
        figures.add("a", 3);
        figures.add("a", 1);
        figures.add("a", 2);
        assert_eq!(figures.median("a"), Some(2));
        assert_eq!(figures.median("b"), None);
    }
}
