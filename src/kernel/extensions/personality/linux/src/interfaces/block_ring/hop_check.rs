//! The seam measured, 1: what the hop to ring 3 costs (`docs/BACKLOG.md`,
//! `docs/OPAQUE-KERNEL.md` S0).
//!
//! After the driver check has read the pattern disk back, this reads it again
//! [`READS`] times, one request at a time and then from [`DEPTH`] kernel
//! tasks at once, and times each read from the call to its answer: the
//! kernel's queue, the ring, the doorbells, the scheduler, the driver and the
//! device. The driver times the device alone -- from handing it a request to
//! draining its completion -- in each completion's `device_ticks`, where the
//! processor's counter is one ring 3 can read and the kernel's clock counts
//! (`arch::ring3_reads_counter`). The difference is the seam: everything
//! between the kernel and the device that a driver in ring 0 would not pay.
//!
//! It prints one line, and asserts nothing about the numbers: they are a
//! measurement, and a slow host is not a failure. What it does require is
//! that every read answers, and answers the bytes `xtask` wrote, and that
//! where ring 3 may read the counter the driver timed its depth-1 requests.
//!
//! The depth-1 reads are traced as well (`sched::trip`), after one untimed
//! read that names the ring's ports to the trace, and [`super::trip_check`]
//! prints where their time went on two more lines.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::trip_check::Trace;
use crate::fs::block::BlockDevice;
use crate::sync::SpinLock;
use crate::{arch, sched, timer};

/// Reads at each depth.
const READS: usize = 1024;
/// The deeper queue: as many readers at once.
const DEPTH: usize = 32;
/// Sectors per read: one 4 KiB page, the page cache's unit.
const SECTORS: usize = 8;
/// Bytes per sector.
const SECTOR_SIZE: usize = 512;
/// How far apart reads land, in sectors, so that consecutive ones are not
/// the same page: a prime, walked modulo the disk.
const STRIDE: u64 = 4099;
/// How long to wait for the deeper run's readers.
const PATIENCE_NANOS: u64 = 60_000_000_000;

/// Device ticks and completions the driver timed, summed over every block
/// ring: `block_ring` adds to these as completions arrive.
pub(crate) static DEVICE_TICKS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// See [`DEVICE_TICKS`].
pub(crate) static DEVICE_TIMED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// One run's numbers, in nanoseconds.
#[derive(Debug, Default)]
struct Run {
    mean: u64,
    p50: u64,
    p99: u64,
    /// The driver's mean device time, if it measured one the kernel can read.
    device: Option<u64>,
}

/// Measure, and print the line. A read that fails or comes back wrong is the
/// check's failure.
///
/// # Errors
///
/// What did not hold, as a sentence.
pub(crate) fn run(disk: &Arc<dyn BlockDevice>) -> Result<(), &'static str> {
    let sectors = disk.sectors();
    if sectors < (SECTORS as u64) * 2 {
        return Ok(());
    }
    let (one, trace) = depth_one(disk.as_ref(), sectors)?;
    let deep = depth_many(disk, sectors)?;
    crate::console::println!(
        "  seam     a 4 KiB read through the ring: depth 1 mean {} p50 {} p99 {} us, {}; \
         depth {DEPTH} mean {} p50 {} p99 {} us, {}",
        micros(one.mean),
        micros(one.p50),
        micros(one.p99),
        device_part(&one),
        micros(deep.mean),
        micros(deep.p50),
        micros(deep.p99),
        device_part(&deep),
    );
    trace.print(READS)
}

/// What the device took, and the rest, which is the seam's.
fn device_part(run: &Run) -> alloc::string::String {
    match run.device {
        Some(device) => alloc::format!(
            "the driver's own submit-to-drain {} us of it (the device, and the interrupt's \
             way up to the driver) and the rest {} us",
            micros(device),
            micros(run.mean.saturating_sub(device))
        ),
        None => alloc::string::String::from("the driver's share not measured here"),
    }
}

/// Nanoseconds as microseconds with one decimal.
fn micros(nanos: u64) -> alloc::string::String {
    alloc::format!("{}.{}", nanos / 1000, (nanos % 1000) / 100)
}

/// Where read `index` lands: page-aligned, spread over the disk.
fn sector_of(index: usize, sectors: u64) -> u64 {
    let pages = sectors / SECTORS as u64;
    (index as u64).wrapping_mul(STRIDE) % pages.max(1) * SECTORS as u64
}

/// Time one read of `sector`, in nanoseconds, requiring it to answer, with
/// its trip when it was traced whole, or else the stamp it did not reach.
fn timed_read(
    disk: &dyn BlockDevice,
    sector: u64,
) -> Result<(u64, Result<sched::trip::Trip, usize>), &'static str> {
    let mut page = [0_u8; SECTORS * SECTOR_SIZE];
    let started = timer::now_nanos();
    sched::trip::issued();
    disk.read(sector, &mut page)
        .map_err(|_| "a timed read through the ring failed")?;
    let trip = sched::trip::done();
    Ok((timer::now_nanos().saturating_sub(started), trip))
}

/// The device's ticks and timed completions so far.
fn device_now() -> (u64, u64) {
    (
        DEVICE_TICKS.load(Ordering::Relaxed),
        DEVICE_TIMED.load(Ordering::Relaxed),
    )
}

/// The mean device time between two readings of [`device_now`], when the
/// driver's counter is the kernel's clock's.
fn device_mean(before: (u64, u64), after: (u64, u64)) -> Option<u64> {
    let ticks = after.0.saturating_sub(before.0);
    let timed = after.1.saturating_sub(before.1);
    let hz = timer::counter_hz();
    if timed == 0 || hz == 0 || !arch::ring3_reads_counter() {
        return None;
    }
    let nanos = u128::from(ticks) * 1_000_000_000 / u128::from(hz) / u128::from(timed);
    u64::try_from(nanos).ok()
}

/// Summarise `samples`, sorting them.
fn summary(mut samples: Vec<u64>, device: Option<u64>) -> Run {
    samples.sort_unstable();
    let count = samples.len().max(1);
    let mean = samples.iter().sum::<u64>() / count as u64;
    let at = |fraction: usize| samples.get(count * fraction / 100).copied().unwrap_or(0);
    Run {
        mean,
        p50: at(50),
        p99: at(99),
        device,
    }
}

/// [`READS`] reads, one at a time, traced.
fn depth_one(disk: &dyn BlockDevice, sectors: u64) -> Result<(Run, Trace), &'static str> {
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(READS)
        .map_err(|_| "no memory for the seam's samples")?;
    let mut trace = Trace::new(READS)?;
    sched::trip::arm();
    let device = traced_reads(disk, sectors, &mut samples, &mut trace);
    sched::trip::disarm();
    Ok((summary(samples, device?), trace))
}

/// The depth-1 reads, after an untimed one that names the ring's ports to
/// the trace: the driver's mean device time over them.
fn traced_reads(
    disk: &dyn BlockDevice,
    sectors: u64,
    samples: &mut Vec<u64>,
    trace: &mut Trace,
) -> Result<Option<u64>, &'static str> {
    let _ = timed_read(disk, sector_of(0, sectors))?;
    trace.start();
    let before = device_now();
    for index in 0..READS {
        let (nanos, trip) = timed_read(disk, sector_of(index, sectors))?;
        samples.push(nanos);
        trace.add(trip);
    }
    let after = device_now();
    trace.stop();
    // The driver served READS requests: where ring 3 may read the counter it
    // timed them, and none timed means its read of the counter failed.
    if arch::ring3_reads_counter() && after.1 == before.1 {
        return Err("the driver timed none of its requests, though ring 3 may read the counter");
    }
    Ok(device_mean(before, after))
}

/// The disk the deeper run's readers share.
static DISK: SpinLock<Option<Arc<dyn BlockDevice>>> = SpinLock::new(None);
/// The deeper run's samples.
static SAMPLES: SpinLock<Vec<u64>> = SpinLock::new(Vec::new());
/// Readers finished, and readers that failed.
static DONE: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicUsize = AtomicUsize::new(0);
/// The disk's size, for the readers.
static SECTORS_OF: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// [`READS`] reads from [`DEPTH`] tasks at once.
fn depth_many(disk: &Arc<dyn BlockDevice>, sectors: u64) -> Result<Run, &'static str> {
    SAMPLES
        .lock()
        .try_reserve_exact(READS)
        .map_err(|_| "no memory for the seam's samples")?;
    *DISK.lock() = Some(Arc::clone(disk));
    SECTORS_OF.store(sectors, Ordering::Relaxed);
    DONE.store(0, Ordering::Relaxed);
    FAILED.store(0, Ordering::Relaxed);
    let before = device_now();
    let mut started = 0;
    for reader in 0..DEPTH {
        if sched::spawn(
            "seam reader",
            reader_task,
            reader,
            ferrix_sched::NICE_0_WEIGHT,
        )
        .is_ok()
        {
            started += 1;
        }
    }
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while DONE.load(Ordering::Acquire) < started {
        if timer::now_nanos() > deadline {
            return Err("the seam's readers did not finish");
        }
        sched::sleep_for(1_000_000);
    }
    let after = device_now();
    *DISK.lock() = None;
    if FAILED.load(Ordering::Relaxed) != 0 || started == 0 {
        return Err("a read from the seam's deeper run failed");
    }
    let samples = core::mem::take(&mut *SAMPLES.lock());
    Ok(summary(samples, device_mean(before, after)))
}

/// One of the deeper run's readers: its share of the reads, each timed.
fn reader_task(reader: usize) {
    let disk = DISK.lock().clone();
    let sectors = SECTORS_OF.load(Ordering::Relaxed);
    if let Some(disk) = disk {
        for index in (reader..READS).step_by(DEPTH) {
            match timed_read(disk.as_ref(), sector_of(index, sectors)) {
                Ok((nanos, _)) => {
                    let mut samples = SAMPLES.lock();
                    if samples.len() < samples.capacity() {
                        samples.push(nanos);
                    }
                }
                Err(_) => {
                    let _ = FAILED.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
    let _ = DONE.fetch_add(1, Ordering::Release);
}
