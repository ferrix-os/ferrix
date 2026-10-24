//! `test-stall`: woken sleepers against a ring-3 load that saturates every
//! processor (finding F-95, `docs/PREEMPTIBLE-WAITS.md`).
//!
//! Boots `src/tests/stall` as init: canaries that sleep 10 ms, one pinned
//! to each processor and one free, beside a load process started and killed
//! phase by phase (`src/tests/stall/src/main.rs` says what each phase does).
//! The program prints each phase's worst wake lateness and the slowest
//! `/proc` read of its load; this row compares each with the bound in
//! [`BOUNDS`], the one place the bounds are written.
//!
//! **Reporting, not asserted.** No phase is asserted yet: F-95 is open, and
//! develop keeps woken tasks off every processor for 280 ms to 190 s in the
//! longer phases. A phase is asserted from the landing that brings it under
//! its bound (`Bound::asserted`); until then its line says "reporting, not
//! asserted", and the row verifies nothing.
//!
//! **The noise rule.** A shared host can keep a virtual processor from
//! running for a hundred milliseconds and more, which no kernel can answer
//! for. So the program runs a quiet phase, with no load, before the load
//! phases and after them. A run in which either quiet phase breaks its bound
//! measured the host and is void: it is reported as void, never as passed,
//! and run once more. Two void runs fail the row.

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, paths, sem};

use crate::task::Task;

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-stall",
    help_at: 575,
    help: &[
        "test-stall    Boot stall-test as init: 10 ms sleepers on every processor beside ring-3 loads that",
        "              saturate them (CPU, futex, faults, Wine's heaps, a convoy, a 1 GiB unmap and fork)",
    ],
    run: |_, args| test_stall(args),
}];

/// One phase's bound.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bound {
    /// The phase's name, as the program prints it.
    pub(crate) phase: &'static str,
    /// The most any canary may wake late in it, in milliseconds.
    pub(crate) wake_ms: u64,
    /// The most the slowest `/proc` read of its load may take, in
    /// milliseconds; `None` for a phase with no load.
    pub(crate) proc_ms: Option<u64>,
    /// Whether the row fails when the phase is over its bound. A quiet
    /// phase is never asserted: over its bound, it makes the run void.
    pub(crate) asserted: bool,
}

/// Each phase's bound: one slice, plus the longest hold a ring-3 program can
/// ask the kernel for, plus room for a shared host's noise. Linux 7.0 in the
/// same QEMU shape measured 3-98 ms in every phase.
pub(crate) const BOUNDS: &[Bound] = &[
    quiet("quiet"),
    load("hog", 100),
    load("futex", 100),
    load("fault", 100),
    load("wine", 100),
    load("convoy", 100),
    load("bigunmap", 150),
    load("forkbig", 150),
    quiet("quiet2"),
];

/// A quiet phase's bound: over it, the run measured the host.
const fn quiet(phase: &'static str) -> Bound {
    Bound {
        phase,
        wake_ms: 100,
        proc_ms: None,
        asserted: false,
    }
}

/// A load phase's bound, not yet asserted.
const fn load(phase: &'static str, wake_ms: u64) -> Bound {
    Bound {
        phase,
        wake_ms,
        proc_ms: Some(wake_ms),
        asserted: false,
    }
}

/// The machine the row asks for unless told otherwise: four processors, so
/// that one process's threads can occupy every one, and memory for the
/// program's 1 GiB regions.
const PROCESSORS: u32 = 4;
/// Memory, in MiB.
const MEMORY: u32 = 2048;
/// Seconds the boot may take: nine 4 s phases, and a kernel that stalls.
const TIMEOUT: u64 = 400;
/// Runs the row makes before two void ones fail it.
const RUNS: usize = 2;

/// What one run found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Every asserted phase within its bound.
    Within,
    /// A quiet phase over its bound: the host was measured.
    Void(String),
    /// An asserted phase over its bound.
    Over(String),
}

/// Boot the program on every architecture asked for and compare each phase
/// with its bound.
///
/// # Errors
///
/// A build or boot that fails, a phase missing from the output, an asserted
/// phase over its bound, two void runs, or a status other than 0.
pub(crate) fn test_stall(args: &Args) -> Result<()> {
    let mut args = args.clone();
    if !args.smp_given {
        args.smp = PROCESSORS;
    }
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    for arch in args.arches()? {
        let program = sem::build_test("stall", arch, None, false)?;
        let mut voids = Vec::new();
        for run in 1..=RUNS {
            match run_once(arch, &program, &args)? {
                Verdict::Within => {
                    println!("  {arch}: stall run {run}: every asserted phase within its bound");
                    voids.clear();
                    break;
                }
                Verdict::Over(why) => {
                    return Err(Error::new(format!(
                        "{arch}: woken tasks were kept off the processors past their bound: \
                         {why}.\n  Serial output is in {}",
                        paths::build_dir(arch).join("serial.log").display()
                    )));
                }
                Verdict::Void(why) => {
                    println!("  {arch}: stall run {run}: VOID, the host was measured: {why}");
                    voids.push(why);
                }
            }
        }
        if voids.len() == RUNS {
            return Err(Error::new(format!(
                "{arch}: {RUNS} stall runs were void, every one's quiet phase late: {}",
                voids.join("; ")
            )));
        }
    }
    Ok(())
}

/// One boot of the program, its lines printed against [`BOUNDS`].
fn run_once(arch: Arch, program: &std::path::Path, args: &Args) -> Result<Verdict> {
    let log = paths::build_dir(arch).join("serial.log");
    let lines = sem::boot(arch, program, args)?;
    if let Some(failed) = lines.iter().find(|line| line.contains("stall: FAILED")) {
        return Err(Error::new(format!(
            "{arch}: the stall test failed: `{}`.\n  Serial output is in {}",
            failed.trim(),
            log.display()
        )));
    }
    if !lines.iter().any(|line| sem::says(line, "stall: done")) || sem::status(&lines) != Some(0) {
        return Err(Error::new(format!(
            "{arch}: the stall test did not finish with status 0.\n  Serial output is in {}",
            log.display()
        )));
    }
    let mut figures = Vec::new();
    for bound in BOUNDS {
        let Some(wake) = figure(&lines, bound.phase, "worst") else {
            return Err(Error::new(format!(
                "{arch}: the stall test printed no worst for phase {}.\n  Serial output is in {}",
                bound.phase,
                log.display()
            )));
        };
        let proc_read = figure(&lines, bound.phase, "proc");
        let mode = if bound.asserted {
            "asserted"
        } else if bound.proc_ms.is_none() {
            "the host's floor"
        } else {
            "reporting, not asserted"
        };
        println!(
            "  {arch}: stall {}: worst wake {wake:.2} ms late (bound {} ms){}, {mode}",
            bound.phase,
            bound.wake_ms,
            proc_read.map_or(String::new(), |ms| format!(
                ", slowest /proc read {ms:.2} ms"
            )),
        );
        figures.push((*bound, wake, proc_read));
    }
    Ok(verdict(&figures))
}

/// The run's verdict from each phase's bound, worst wake and slowest
/// `/proc` read.
pub(crate) fn verdict(figures: &[(Bound, f64, Option<f64>)]) -> Verdict {
    let late = |bound: &Bound, wake: f64, proc_read: Option<f64>| {
        let mut why = Vec::new();
        if wake > bound.wake_ms as f64 {
            why.push(format!(
                "{} woke {wake:.2} ms > {} ms",
                bound.phase, bound.wake_ms
            ));
        }
        if let (Some(read), Some(limit)) = (proc_read, bound.proc_ms)
            && read > limit as f64
        {
            why.push(format!("{} /proc {read:.2} ms > {limit} ms", bound.phase));
        }
        why
    };
    let void: Vec<String> = figures
        .iter()
        .filter(|(bound, ..)| bound.proc_ms.is_none())
        .flat_map(|(bound, wake, read)| late(bound, *wake, *read))
        .collect();
    if !void.is_empty() {
        return Verdict::Void(void.join(", "));
    }
    let over: Vec<String> = figures
        .iter()
        .filter(|(bound, ..)| bound.asserted)
        .flat_map(|(bound, wake, read)| late(bound, *wake, *read))
        .collect();
    if over.is_empty() {
        Verdict::Within
    } else {
        Verdict::Over(over.join(", "))
    }
}

/// Phase `phase`'s figure `what` (`worst` or `proc`) in ms, from its line.
pub(crate) fn figure(lines: &[String], phase: &str, what: &str) -> Option<f64> {
    let key = format!("stall: {phase} {what} ");
    lines.iter().find_map(|line| {
        let at = line.find(&key)?;
        line.get(at + key.len()..)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::{BOUNDS, Bound, Verdict, figure, verdict};

    #[test]
    fn reads_the_figure_of_the_phase_named_and_no_other() {
        let lines = vec![
            " 15.2 | stall: fault cpu0 wakes 390 p50 0.02 p99 3.10 max 11.50 ms".to_owned(),
            " 15.3 | stall: fault worst 11.50 ms".to_owned(),
            " 15.3 | stall: fault proc 2.25 ms".to_owned(),
            " 15.4 | stall: forkbig worst 190655.50 ms".to_owned(),
        ];
        assert_eq!(figure(&lines, "fault", "worst"), Some(11.5));
        assert_eq!(figure(&lines, "fault", "proc"), Some(2.25));
        assert_eq!(figure(&lines, "forkbig", "worst"), Some(190_655.5));
        assert_eq!(figure(&lines, "hog", "worst"), None);
    }

    #[test]
    fn every_phase_of_the_program_has_a_bound() {
        let program = include_str!("../../../../src/tests/stall/src/main.rs");
        for bound in BOUNDS {
            assert!(
                program.contains(&format!("\"{}\"", bound.phase)),
                "{} is not a phase",
                bound.phase
            );
        }
    }

    fn bound(phase: &'static str, asserted: bool, quiet: bool) -> Bound {
        Bound {
            phase,
            wake_ms: 100,
            proc_ms: (!quiet).then_some(100),
            asserted,
        }
    }

    #[test]
    fn a_late_quiet_phase_voids_the_run_and_a_late_asserted_phase_fails_it() {
        let quiet = bound("quiet", false, true);
        let asserted = bound("fault", true, false);
        let reported = bound("forkbig", false, false);
        assert_eq!(
            verdict(&[
                (quiet, 5.0, None),
                (asserted, 50.0, Some(3.0)),
                (reported, 9e5, None)
            ]),
            Verdict::Within,
            "a phase only reported never fails the run"
        );
        assert!(matches!(
            verdict(&[(quiet, 150.0, None), (asserted, 500.0, None)]),
            Verdict::Void(_)
        ));
        assert!(matches!(
            verdict(&[(quiet, 5.0, None), (asserted, 500.0, None)]),
            Verdict::Over(_)
        ));
        assert!(matches!(
            verdict(&[(quiet, 5.0, None), (asserted, 5.0, Some(800.0))]),
            Verdict::Over(_)
        ));
    }

    #[test]
    fn nothing_is_asserted_while_f95_is_open() {
        assert!(BOUNDS.iter().all(|bound| !bound.asserted));
    }
}
