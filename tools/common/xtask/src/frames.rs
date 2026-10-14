//! `cargo xtask frames`: stage 24's frame recorder, built and read
//! (docs/GAME-PATH.md §2.4, R1).
//!
//! `tools/common/game/framerec.c` is an `LD_PRELOAD` library that records,
//! for every swap of a GLX or EGL program, the monotonic time the swap was
//! entered and returned, and for a GLX window the Present extension's
//! `CompleteNotify` -- the window's media stream counter (msc) and its time
//! (ust) -- answered for a `PresentNotifyMSC` sent right after the swap.
//! `tools/common/game/framegl.c` is its test client: a GLX window paced to the
//! server's msc as a program with vsync on is. Both are built here, by the
//! host's C compiler, against nothing but the C library: the same two
//! binaries go to Ferrix and to the Linux baseline guest.
//!
//! [`summarise`] reads a record: the frame times between swaps (median, 99th
//! and 99.9th percentile, worst), and the missed refreshes counted two ways.
//! **By msc**, the design's definition: an msc that advanced by more than
//! one between two swaps' readings misses the refreshes in between. **By
//! time**, the program's own view: the time between two swaps' returns in
//! refreshes, rounded, less one, with the refresh taken from the
//! completions (or `--fps`). On a server whose msc is a clock -- Xorg's,
//! Xvfb's, yserver's -- the two agree. Xwayland's msc is not one: it counts
//! the compositor's frame events while a presentation waits, and stands
//! still while nothing does, so a program that stalls misses no msc there;
//! only the time sees it (`xwl_present_msc_bump`, Xwayland 24.1).
//!
//! `cargo xtask frames` builds both into the build directory and prints
//! their paths and SHA-256; `cargo xtask frames --from <record>` summarises
//! a record from either system. `cargo xtask test-frames`
//! (`crate::compositor::frames`) is the gate on Ferrix.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result};

/// The recorder's source, under `tools/common/game/`.
const RECORDER: &str = "framerec.c";

/// The test client's source, under `tools/common/game/`.
const CLIENT: &str = "framegl.c";

/// The two programs [`build`] makes.
#[derive(Debug)]
pub(crate) struct Built {
    /// `libframerec.so`, the recorder.
    pub(crate) recorder: PathBuf,
    /// `framegl`, the paced test client.
    pub(crate) client: PathBuf,
}

/// Build the recorder and the test client with the host's `cc` into
/// `<build dir>/frames/`.
///
/// The recorder asks the C library for nothing newer than glibc 2.2.5's
/// symbols, as the baseline's swap counter did, so it loads into any x86-64
/// glibc program; the client runs on Debian 13's glibc, which both guests
/// carry.
///
/// # Errors
///
/// No `cc`, or a build that fails.
pub(crate) fn build() -> Result<Built> {
    let source = paths::workspace_root().join("tools/common/game");
    let out = paths::build_dir(Arch::X86_64).join("frames");
    std::fs::create_dir_all(&out)
        .map_err(|error| Error::new(format!("creating {}: {error}", out.display())))?;
    let recorder = out.join("libframerec.so");
    let client = out.join("framegl");
    let common = ["-std=gnu11", "-O2", "-Wall", "-Wextra", "-Werror"];
    compile(
        &[
            &common[..],
            &[
                "-fPIC",
                "-shared",
                "-fno-stack-protector",
                "-U_FORTIFY_SOURCE",
            ],
        ]
        .concat(),
        &source.join(RECORDER),
        &recorder,
    )?;
    compile(&common, &source.join(CLIENT), &client)?;
    Ok(Built { recorder, client })
}

/// `cc <flags> -o <out> <source> -ldl`.
fn compile(flags: &[&str], source: &Path, out: &Path) -> Result<()> {
    let built = Command::new("cc")
        .args(flags)
        .arg("-o")
        .arg(out)
        .arg(source)
        .arg("-ldl")
        .status()
        .map_err(|error| Error::new(format!("running cc: {error}")))?;
    if built.success() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "building {} failed ({built})",
            source.display()
        )))
    }
}

/// `cargo xtask frames`: build, or with `--from` summarise a record.
///
/// # Errors
///
/// A build that fails, or a record that cannot be read or holds no swaps.
pub(crate) fn run(args: &Args) -> Result<()> {
    if let Some(from) = args.from.as_deref() {
        let text = std::fs::read_to_string(from)
            .map_err(|error| Error::new(format!("reading {from}: {error}")))?;
        let record = Record::parse(text.lines(), "");
        let summary = summarise(&record, args.fps.map(|fps| 1e6 / f64::from(fps)))?;
        println!("{from}:\n{summary}");
        return Ok(());
    }
    let built = build()?;
    for program in [&built.recorder, &built.client] {
        println!("{}  {}", sha256_of(program)?, program.display());
    }
    Ok(())
}

/// The SHA-256 of a file, in hex: what says two systems ran the same
/// binary.
///
/// # Errors
///
/// The file cannot be read.
pub(crate) fn sha256_of(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    Ok(crate::sha256::hex(&bytes))
}

/// One swap: its number and the monotonic nanoseconds it was entered and
/// returned at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Swap {
    /// The swap's number, from 0.
    pub(crate) index: u32,
    /// When the program called it.
    pub(crate) entry: u64,
    /// When it returned.
    pub(crate) back: u64,
}

/// What the server answered for a swap: the ust in microseconds and the
/// msc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Completion {
    /// The time of the msc, in microseconds.
    pub(crate) ust: u64,
    /// The media stream counter.
    pub(crate) msc: u64,
}

/// A record, read.
#[derive(Debug, Default)]
pub(crate) struct Record {
    /// The swaps, in order.
    pub(crate) swaps: Vec<Swap>,
    /// The recorder's own completions, by the swap they follow.
    pub(crate) after: BTreeMap<u32, Completion>,
    /// The program's own `PresentPixmap` completions.
    pub(crate) pixmaps: usize,
    /// The stalls injected, by the swap they came before, in milliseconds.
    pub(crate) stalls: BTreeMap<u32, u64>,
    /// What went wrong, as the recorder wrote it.
    pub(crate) errors: Vec<String>,
}

impl Record {
    /// Read a record from its lines. With a `prefix`, only the part of a
    /// line after it is read, and lines without it are passed over, so a
    /// guest's whole transcript can be given.
    pub(crate) fn parse<'a>(lines: impl Iterator<Item = &'a str>, prefix: &str) -> Self {
        let mut record = Self::default();
        for line in lines {
            let body = if prefix.is_empty() {
                Some(line)
            } else {
                line.split_once(prefix).map(|(_, rest)| rest)
            };
            if let Some(body) = body {
                record.add(body.trim());
            }
        }
        record
    }

    /// One line of the record.
    fn add(&mut self, line: &str) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let number = |at: usize| fields.get(at).and_then(|field| field.parse::<u64>().ok());
        let index = || number(1).and_then(|index| u32::try_from(index).ok());
        match fields.first().copied() {
            Some("s") => {
                if let (Some(index), Some(entry), Some(back)) = (index(), number(2), number(3)) {
                    self.swaps.push(Swap { index, entry, back });
                }
            }
            Some("c") => match (index(), fields.get(2).copied(), number(3), number(4)) {
                (Some(index), Some("n"), Some(ust), Some(msc)) => {
                    let _ = self.after.insert(index, Completion { ust, msc });
                }
                (Some(_), Some("p"), Some(_), Some(_)) => self.pixmaps += 1,
                _ => {}
            },
            Some("x") => {
                if let (Some(index), Some(ms)) = (index(), number(2)) {
                    let _ = self.stalls.insert(index, ms);
                }
            }
            Some("e") => self.errors.push(line.to_owned()),
            _ => {}
        }
    }
}

/// What a record says, in numbers.
#[derive(Debug, PartialEq)]
pub(crate) struct Summary {
    /// Swaps recorded.
    pub(crate) swaps: usize,
    /// Swaps the recorder has the server's answer for.
    pub(crate) answered: usize,
    /// The time from the first swap's return to the last's, in seconds.
    pub(crate) seconds: f64,
    /// Frame times, the time between two swaps' returns, in milliseconds.
    pub(crate) median: f64,
    /// The 99th percentile.
    pub(crate) p99: f64,
    /// The 99.9th percentile.
    pub(crate) p999: f64,
    /// The longest frame, and the swap that ended it.
    pub(crate) worst: (f64, u32),
    /// The refresh, in microseconds, from the completions or `--fps`.
    pub(crate) refresh: Option<f64>,
    /// How far the msc moved from the first answer to the last.
    pub(crate) msc_span: u64,
    /// Missed refreshes by msc, when the server answered.
    pub(crate) by_msc: Option<u64>,
    /// Missed refreshes by time, when the refresh is known.
    pub(crate) by_time: Option<u64>,
    /// Each injected stall: the swap after it, and the refreshes it missed
    /// by msc and by time.
    pub(crate) stalls: Vec<(u32, Option<u64>, Option<u64>)>,
}

impl Summary {
    /// `misses` (by msc or by time) a minute.
    pub(crate) fn per_minute(&self, misses: Option<u64>) -> Option<f64> {
        let misses = misses?;
        (self.seconds > 0.0).then(|| misses as f64 * 60.0 / self.seconds)
    }
}

/// The part of a refresh by which [`missed_by_time`] may count a return as
/// after the refresh it came just before.
const SLACK: f64 = 0.01;

/// The refreshes missed between swaps `a` and `b` by time, at a refresh of
/// `refresh` microseconds: the refreshes that began from the one the
/// server said `a` fell in (its completion's ust) to `b`'s return, less
/// one. Anchored at the server's refresh and not at `a`'s return, so a
/// stall that is not a whole number of refreshes is not rounded up: at 75
/// Hz a 50 ms stall is 3.75 refreshes, and the frame after it still shows
/// at the fourth. A return within [`SLACK`] of a refresh counts as after
/// it: the refresh is a median of the server's steps, a microsecond off,
/// which four refreshes on would put a frame that returned just after one
/// before it. Without `a`'s completion, the time between the two returns,
/// rounded.
fn missed_by_time(a: &Swap, b: &Swap, after_a: Option<&Completion>, refresh: f64) -> u64 {
    let refreshes = match after_a {
        Some(at) => ((b.back as f64 / 1000.0 - at.ust as f64) / refresh + SLACK).floor(),
        None => (b.back.saturating_sub(a.back) as f64 / 1000.0 / refresh).round(),
    };
    (refreshes.max(0.0) as u64).saturating_sub(1)
}

/// The value at `percentile` (0 to 1) of `sorted`, by nearest rank.
fn rank(sorted: &[f64], percentile: f64) -> f64 {
    let at = (percentile * sorted.len() as f64).ceil() as usize;
    sorted
        .get(at.saturating_sub(1).min(sorted.len().saturating_sub(1)))
        .copied()
        .unwrap_or_default()
}

/// The median time between the completions of two swaps in a row whose
/// msc moved by one: the server's refresh, in microseconds.
fn refresh_of(record: &Record) -> Option<f64> {
    let mut steps: Vec<f64> = record
        .swaps
        .windows(2)
        .filter_map(|pair| {
            let (a, b) = (pair.first()?, pair.get(1)?);
            let (a, b) = (record.after.get(&a.index)?, record.after.get(&b.index)?);
            (b.msc == a.msc + 1).then(|| b.ust.saturating_sub(a.ust) as f64)
        })
        .collect();
    steps.sort_by(f64::total_cmp);
    (!steps.is_empty()).then(|| rank(&steps, 0.5))
}

/// Summarise `record`. `refresh`, in microseconds, stands in for the one
/// the completions give.
///
/// # Errors
///
/// A record with fewer than two swaps.
pub(crate) fn summarise(record: &Record, refresh: Option<f64>) -> Result<Summary> {
    let (Some(first), Some(last)) = (record.swaps.first(), record.swaps.last()) else {
        return Err(Error::new("the record holds no swaps"));
    };
    if record.swaps.len() < 2 {
        return Err(Error::new("the record holds one swap"));
    }
    let refresh = refresh.or_else(|| refresh_of(record));
    let mut times = Vec::new();
    let mut worst = (0.0, first.index);
    let (mut by_msc, mut by_time) = (0, 0);
    let mut stalls = Vec::new();
    for pair in record.swaps.windows(2) {
        let (Some(a), Some(b)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let apart = b.back.saturating_sub(a.back);
        let ms = apart as f64 / 1e6;
        times.push(ms);
        if ms > worst.0 {
            worst = (ms, b.index);
        }
        let msc = match (record.after.get(&a.index), record.after.get(&b.index)) {
            (Some(a), Some(b)) => Some(b.msc.saturating_sub(a.msc).saturating_sub(1)),
            _ => None,
        };
        let time = refresh.map(|refresh| missed_by_time(a, b, record.after.get(&a.index), refresh));
        by_msc += msc.unwrap_or_default();
        by_time += time.unwrap_or_default();
        if record.stalls.contains_key(&b.index) {
            stalls.push((b.index, msc, time));
        }
    }
    times.sort_by(f64::total_cmp);
    let answered: Vec<&Completion> = record.after.values().collect();
    let msc_span = match (answered.first(), answered.last()) {
        (Some(a), Some(b)) => b.msc.saturating_sub(a.msc),
        _ => 0,
    };
    Ok(Summary {
        swaps: record.swaps.len(),
        answered: answered.len(),
        seconds: last.back.saturating_sub(first.back) as f64 / 1e9,
        median: rank(&times, 0.5),
        p99: rank(&times, 0.99),
        p999: rank(&times, 0.999),
        worst,
        refresh,
        msc_span,
        by_msc: (!answered.is_empty()).then_some(by_msc),
        by_time: refresh.map(|_| by_time),
        stalls,
    })
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |v| v.to_string());
        let rate = |value: Option<u64>| {
            self.per_minute(value)
                .map_or_else(|| "-".to_owned(), |rate| format!("{rate:.1}"))
        };
        let mut stalls = String::new();
        for (swap, msc, time) in &self.stalls {
            let _ = write!(stalls, " {swap}:{}/{}", count(*msc), count(*time));
        }
        writeln!(
            f,
            "  swaps {} in {:.1} s, {} answered by the server, msc moved {}",
            self.swaps, self.seconds, self.answered, self.msc_span
        )?;
        writeln!(
            f,
            "  frame time median {:.2} ms, 99th {:.2} ms, 99.9th {:.2} ms, worst {:.2} ms (swap {})",
            self.median, self.p99, self.p999, self.worst.0, self.worst.1
        )?;
        writeln!(
            f,
            "  refresh {}; missed refreshes {} by msc ({} a minute), {} by time ({} a minute)",
            self.refresh.map_or_else(
                || "unknown".to_owned(),
                |us| format!("{:.3} ms", us / 1000.0)
            ),
            count(self.by_msc),
            rate(self.by_msc),
            count(self.by_time),
            rate(self.by_time)
        )?;
        write!(
            f,
            "  stalls injected {}, missed by msc/time:{}",
            self.stalls.len(),
            if stalls.is_empty() { " none" } else { &stalls }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record of a paced client on a server whose msc is a clock of
    /// `period` nanoseconds: each frame is drawn when the refresh it waited
    /// for begins, swapped 2 ms later -- `stall` nanoseconds later still
    /// before each swap in `stalled` -- and answered with the refresh the
    /// swap fell in; then the client waits for the next one.
    fn simulate(period: u64, count: u32, stalled: &[u32], stall: u64) -> String {
        let start = 1_000_000_000_u64;
        let mut text = String::from("framerec 1 pid 7\n");
        let mut waited = 100_u64;
        for index in 0..count {
            let mut back = start + waited * period + 2_000_000;
            if stalled.contains(&index) {
                back += stall;
                let _ = writeln!(text, "x {index} {}", stall / 1_000_000);
            }
            let msc = (back - start) / period;
            let _ = writeln!(text, "s {index} {} {back}", back - 1_000_000);
            let _ = writeln!(text, "c {index} n {} {msc}", (start + msc * period) / 1000);
            waited = msc + 1;
        }
        text
    }

    /// A record at 60 Hz with a stall of `stall` refreshes before each swap
    /// in `stalled`.
    fn record(count: u32, stalled: &[u32], stall: u64) -> String {
        simulate(16_666_667, count, stalled, stall * 16_666_667)
    }

    /// At 75 Hz a 50 ms stall is 3.75 refreshes: the frame after it shows
    /// at the fourth, three missed, by msc and by time alike.
    #[test]
    fn a_stall_of_a_fraction_of_refreshes_is_not_rounded_up() {
        let text = simulate(13_334_000, 3000, &[750, 1500, 2250], 50_000_000);
        let summary = summarise(&Record::parse(text.lines(), ""), None).unwrap();
        assert_eq!(
            summary.stalls,
            vec![
                (750, Some(3), Some(3)),
                (1500, Some(3), Some(3)),
                (2250, Some(3), Some(3))
            ]
        );
        assert_eq!(summary.by_msc, Some(9));
        assert_eq!(summary.by_time, Some(9));
    }

    /// A steady record misses nothing; its frame time is the refresh.
    #[test]
    fn a_steady_record_misses_nothing() {
        let text = record(600, &[], 0);
        let summary = summarise(&Record::parse(text.lines(), ""), None).unwrap();
        assert_eq!(summary.swaps, 600);
        assert_eq!(summary.answered, 600);
        assert_eq!(summary.by_msc, Some(0));
        assert_eq!(summary.by_time, Some(0));
        assert!((summary.median - 16.667).abs() < 0.01);
        assert!((summary.refresh.unwrap() - 16_667.0).abs() < 1.0);
        assert_eq!(summary.msc_span, 599);
    }

    /// Three stalls of three refreshes each are counted as three misses
    /// each, both ways, at the swaps they came before.
    #[test]
    fn each_stall_is_three_misses() {
        let text = record(2000, &[600, 1200, 1800], 3);
        let summary = summarise(&Record::parse(text.lines(), ""), None).unwrap();
        assert_eq!(summary.by_msc, Some(9));
        assert_eq!(summary.by_time, Some(9));
        assert_eq!(
            summary.stalls,
            vec![
                (600, Some(3), Some(3)),
                (1200, Some(3), Some(3)),
                (1800, Some(3), Some(3))
            ]
        );
        assert!((summary.worst.0 - 4.0 * 16.667).abs() < 0.01);
        assert_eq!(summary.worst.1, 600);
    }

    /// An msc that stands still through a stall, as Xwayland's does,
    /// misses nothing by msc; the time still sees the stall.
    #[test]
    fn a_frozen_msc_is_seen_by_time_only() {
        let text = record(1000, &[500], 3);
        let frozen: String = text
            .lines()
            .map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                match fields.as_slice() {
                    ["c", index, "n", ust, _] => {
                        let index: u64 = index.parse().unwrap();
                        format!("c {index} n {ust} {}\n", 100 + index)
                    }
                    _ => format!("{line}\n"),
                }
            })
            .collect();
        let summary = summarise(&Record::parse(frozen.lines(), ""), None).unwrap();
        assert_eq!(summary.by_msc, Some(0));
        assert_eq!(summary.by_time, Some(3));
    }

    /// A transcript's lines are read after the prefix, and others passed
    /// over; errors are kept.
    #[test]
    fn a_transcript_is_read_after_its_prefix() {
        let lines = [
            "  3.10 | frames: stall: rec: framerec 1 pid 9",
            "  3.11 | frames: stall: rec: s 0 10 20",
            "  3.12 | hyprix: something else",
            "  3.13 | frames: stall: rec: c 0 n 5 7",
            "  3.14 | frames: stall: rec: e 4 0",
            "  3.15 | frames: plain: rec: s 9 9 9",
        ];
        let record = Record::parse(lines.into_iter(), "frames: stall: rec: ");
        assert_eq!(
            record.swaps,
            vec![Swap {
                index: 0,
                entry: 10,
                back: 20
            }]
        );
        assert_eq!(record.after.len(), 1);
        assert_eq!(record.errors, vec!["e 4 0".to_owned()]);
        assert!(summarise(&record, None).is_err(), "one swap");
    }

    /// Percentiles by nearest rank.
    #[test]
    fn percentiles_are_by_nearest_rank() {
        let values: Vec<f64> = (1..=1000).map(f64::from).collect();
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(near(rank(&values, 0.5), 500.0));
        assert!(near(rank(&values, 0.99), 990.0));
        assert!(near(rank(&values, 0.999), 999.0));
        assert!(near(rank(&[], 0.5), 0.0));
    }
}
