//! The seam measured, 1, split: where a depth-1 read's time goes, hop by hop,
//! and what the path did on the way (`docs/OPAQUE-KERNEL.md`, S0).
//!
//! [`super::hop_check`] traces its depth-1 reads through `sched::trip`, which
//! stamps each point a read passes. This gathers the stamps of every whole
//! trip and prints two lines under the `seam` line:
//!
//! - `seam-trip`: each hop's median and 99th percentile, the whole trip's,
//!   what the hops' medians add up to, how often a task was woken on another
//!   processor than the one that woke it, and how often a hop was not taken;
//! - `seam-count`: what the path did per read -- switches, switch barriers,
//!   user roots written and taken off, interprocessor interrupts, device
//!   interrupts, and the sleeps on the three queues a trip waits on, ended by
//!   a wake or by the recheck timer.
//!
//! Like the `seam` line it asserts nothing about the numbers. It does require
//! that the trace saw at least one whole trip: a stamp point moved or lost
//! would otherwise print a line of zeros that reads as a free trip.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::sched::trip::{self, Count, NAMES, STAMPS, Stamp, Traced, Trip};
use crate::{arch, sched, timer};

/// Hops between consecutive stamps.
const HOPS: usize = STAMPS - 1;

/// Each wake on the path: the stamp of the waker, then the woken's.
const WAKES: [(Stamp, Stamp); 5] = [
    (Stamp::Queued, Stamp::RingRunning),
    (Stamp::Bell, Stamp::DriverBell),
    (Stamp::Irq, Stamp::DriverIrq),
    (Stamp::Posted, Stamp::RingAgain),
    (Stamp::Answered, Stamp::Reader),
];

/// Switches and switch barriers on every processor so far.
fn machine_counts() -> (u64, u64) {
    (sched::summary().switches, arch::switch_barriers())
}

/// The traced trips of one run.
#[derive(Debug)]
pub(super) struct Trace {
    /// Each hop's time, in counter ticks, one sample per whole trip.
    hops: [Vec<u64>; HOPS],
    /// The whole trip's.
    whole: Vec<u64>,
    /// Trips that were not whole, by the stamp each did not reach.
    partial: [u32; STAMPS],
    /// Per wake in [`WAKES`]: trips where it happened, and where the woken
    /// ran on another processor than the waker.
    woken: [(u32, u32); WAKES.len()],
    /// Per stamp: trips that passed it over.
    skipped: [u32; STAMPS],
    /// Switches and switch barriers when counting started, then the
    /// difference when it stopped.
    machine: (u64, u64),
    /// What `sched::trip` counted.
    counted: trip::Counted,
}

impl Trace {
    /// Room for `reads` trips, taken before the run so the run allocates
    /// nothing.
    pub(super) fn new(reads: usize) -> Result<Trace, &'static str> {
        let mut hops: [Vec<u64>; HOPS] = core::array::from_fn(|_| Vec::new());
        let mut whole = Vec::new();
        for samples in hops.iter_mut().chain(core::iter::once(&mut whole)) {
            samples
                .try_reserve_exact(reads)
                .map_err(|_| "no memory for the seam's trace")?;
        }
        Ok(Trace {
            hops,
            whole,
            partial: [0; STAMPS],
            woken: [(0, 0); WAKES.len()],
            skipped: [0; STAMPS],
            machine: (0, 0),
            counted: trip::Counted::default(),
        })
    }

    /// Counting starts: the trace's own counts are zeroed, and the
    /// machine's read.
    pub(super) fn start(&mut self) {
        trip::arm();
        self.machine = machine_counts();
    }

    /// Counting stops.
    pub(super) fn stop(&mut self) {
        self.counted = trip::counted();
        let (switches, barriers) = machine_counts();
        self.machine = (
            switches.saturating_sub(self.machine.0),
            barriers.saturating_sub(self.machine.1),
        );
    }

    /// Take one read's trip, if it was whole, or count the stamp it did not
    /// reach.
    pub(super) fn add(&mut self, trip: Result<Trip, usize>) {
        let trip = match trip {
            Ok(trip) => trip,
            Err(waiting) => {
                if let Some(partial) = self.partial.get_mut(waiting) {
                    *partial += 1;
                }
                return;
            }
        };
        for (hop, samples) in self.hops.iter_mut().enumerate() {
            let from = trip.at.get(hop).copied().unwrap_or(0);
            let to = trip.at.get(hop + 1).copied().unwrap_or(from);
            samples.push(to.saturating_sub(from));
        }
        let first = trip.at.first().copied().unwrap_or(0);
        let last = trip.at.last().copied().unwrap_or(first);
        self.whole.push(last.saturating_sub(first));
        for (stamp, skipped) in self.skipped.iter_mut().enumerate() {
            if trip.skipped & (1 << stamp) != 0 {
                *skipped += 1;
            }
        }
        for ((waker, woken), counts) in WAKES.iter().zip(self.woken.iter_mut()) {
            if trip.skipped & (1 << (*woken as usize)) != 0 {
                continue;
            }
            counts.0 += 1;
            if trip.cpu.get(*waker as usize) != trip.cpu.get(*woken as usize) {
                counts.1 += 1;
            }
        }
    }

    /// Print both lines for a run of `reads`.
    ///
    /// # Errors
    ///
    /// When no trip was whole.
    pub(super) fn print(mut self, reads: usize) -> Result<(), &'static str> {
        let traced = self.whole.len();
        if traced == 0 {
            return Err("the seam's trace saw no whole trip through the ring");
        }
        let hz = timer::counter_hz();
        let mut line = String::new();
        let _ = write!(
            line,
            "  seam-trip depth 1: {traced} of {reads} reads traced; p50/p99 us per hop:"
        );
        let mut medians = 0;
        for (hop, samples) in self.hops.iter_mut().enumerate() {
            let (p50, p99) = percentiles(samples);
            medians += p50;
            let from = NAMES.get(hop).copied().unwrap_or("?");
            let to = NAMES.get(hop + 1).copied().unwrap_or("?");
            let _ = write!(line, " {from}>{to} {}/{}", micros(p50, hz), micros(p99, hz));
        }
        let (p50, p99) = percentiles(&mut self.whole);
        let share = medians.saturating_mul(100) / p50.max(1);
        let _ = write!(
            line,
            "; whole p50 {} p99 {} us, the hops' medians add to {} us ({share}%); \
             woken on another processor:",
            micros(p50, hz),
            micros(p99, hz),
            micros(medians, hz),
        );
        for ((_, woken), (happened, elsewhere)) in WAKES.iter().zip(self.woken) {
            let name = NAMES.get(*woken as usize).copied().unwrap_or("?");
            let _ = write!(line, " {name} {elsewhere}/{happened}");
        }
        let _ = write!(line, "; hops not taken:");
        let mut none = true;
        for (stamp, skipped) in self.skipped.iter().enumerate() {
            if *skipped != 0 {
                none = false;
                let name = NAMES.get(stamp).copied().unwrap_or("?");
                let _ = write!(line, " {name} {skipped}");
            }
        }
        if none {
            let _ = write!(line, " none");
        }
        let partial: u32 = self.partial.iter().sum();
        if partial != 0 {
            let _ = write!(line, "; {partial} trips not whole, stopped short of:");
            for (stamp, count) in self.partial.iter().enumerate() {
                if *count != 0 {
                    let name = NAMES.get(stamp).copied().unwrap_or("?");
                    let _ = write!(line, " {name} {count}");
                }
            }
        }
        crate::console::println!("{line}");
        self.print_counts(reads);
        Ok(())
    }

    /// The `seam-count` line.
    fn print_counts(&self, reads: usize) {
        let count = |what: Count| {
            per_read(
                self.counted.counts.get(what as usize).copied().unwrap_or(0),
                reads,
            )
        };
        let slept = |which: Traced| {
            let [woken, rechecked] = self
                .counted
                .slept
                .get(which as usize)
                .copied()
                .unwrap_or([0; 2]);
            alloc::format!("{}/{}", per_read(woken, reads), per_read(rechecked, reads))
        };
        crate::console::println!(
            "  seam-count per depth-1 read: switches {}, switch barriers (IBPB) {}, user roots \
             written {} and taken off {}, IPIs sent {}, device interrupts {}; sleeps ended by a \
             wake/by the recheck: reader {}, ring port {}, driver port {}",
            per_read(self.machine.0, reads),
            per_read(self.machine.1, reads),
            count(Count::RootInstall),
            count(Count::RootUninstall),
            count(Count::Ipi),
            count(Count::DeviceInterrupt),
            slept(Traced::Reader),
            slept(Traced::RingPort),
            slept(Traced::DriverPort),
        );
    }
}

/// The median and the 99th percentile of `samples`, sorting them.
fn percentiles(samples: &mut [u64]) -> (u64, u64) {
    samples.sort_unstable();
    let count = samples.len();
    let at = |fraction: usize| samples.get(count * fraction / 100).copied().unwrap_or(0);
    (at(50), at(99))
}

/// Counter ticks as microseconds with one decimal.
fn micros(ticks: u64, hz: u64) -> String {
    let nanos = u128::from(ticks) * 1_000_000_000 / u128::from(hz.max(1));
    let nanos = u64::try_from(nanos).unwrap_or(u64::MAX);
    alloc::format!("{}.{}", nanos / 1000, (nanos % 1000) / 100)
}

/// `total` over `reads`, with two decimals.
fn per_read(total: u64, reads: usize) -> String {
    let hundredths = total.saturating_mul(100) / u64::try_from(reads.max(1)).unwrap_or(1);
    alloc::format!("{}.{:02}", hundredths / 100, hundredths % 100)
}
