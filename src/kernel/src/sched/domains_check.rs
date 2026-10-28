//! The scheduling domains' self-checks (`docs/SCHED-CLASSES.md` §9f, V15),
//! with kernel tasks: the run-time switch, the partition, admission and
//! dissolution as the scheduler carries them out.
//!
//! * **The refusals** (D-3, V9): no `SoftRt` domain where the tier is not
//!   built; none of no processors, nor of more than all but two; none on a
//!   machine of fewer than four; a second refused while one exists.
//! * **The partition** (D-1, D-2, H.SCHED.21): [`SPINNERS`] kernel tasks of
//!   the boot domain spin, unpinned, across every processor; a `SoftRt`
//!   domain of all but two processors is made under them, and afterwards
//!   none of them runs on its processors.
//! * **Admission**: two of them admitted run only on the domain's
//!   processors, and run there; the rest still never do.
//! * **Dissolution** (D-4): dissolved, its processors are the boot
//!   domain's again, and the boot domain's tasks run on them.
//!
//! A spinner records the processor it runs on with interrupts masked, so
//! that a record is never made on one processor and written after a move
//! to another. Every pick and every move under the partition is also the
//! scheduler's own check (FX-0538), so a placement that put a task outside
//! its domain stops the machine rather than reaching these counts.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_sched_class::CpuSet;
use ferrix_sched_fair::NICE_0_WEIGHT;
use ferrix_sync::IrqControl;

use crate::arch;
use crate::object::job::Job;

use super::domains::{self, Refused};
use super::task::Task;

/// How many spinners: twice the processors of a four-processor machine, so
/// that the boot domain's two left are crowded and an idle processor would
/// steal at once if it could.
const SPINNERS: usize = 8;

/// How long each look at where the spinners ran lasts.
const LOOK_NANOS: u64 = 100_000_000;

/// How long to give a task to move at its processor's next decision.
const SETTLE_NANOS: u64 = 20_000_000;

/// How long a spinner may take to see [`STOP`] and exit.
const GONE_NANOS: u64 = 5_000_000_000;

/// Told to the spinners when the checks are done.
static STOP: AtomicBool = AtomicBool::new(false);

/// How many spinners have started.
static STARTED: AtomicUsize = AtomicUsize::new(0);

/// The processors each spinner has run on since the last [`clear`], a bit
/// each (the first 64).
static SEEN: [AtomicU64; SPINNERS] = [const { AtomicU64::new(0) }; SPINNERS];

/// Spin until [`STOP`], recording where.
fn spinner(index: usize) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    while !STOP.load(Ordering::Acquire) {
        // Masked, so that the processor read is the one the record is made
        // on: a spinner moved between the two would blame the wrong one.
        let saved = <arch::Irq as IrqControl>::disable();
        let bit = super::this_cpu()
            .and_then(|cpu| u32::try_from(cpu).ok())
            .and_then(|cpu| 1_u64.checked_shl(cpu))
            .unwrap_or(0);
        if let Some(seen) = SEEN.get(index) {
            let _ = seen.fetch_or(bit, Ordering::AcqRel);
        }
        <arch::Irq as IrqControl>::restore(saved);
        core::hint::spin_loop();
    }
}

/// Forget where the spinners ran.
fn clear() {
    for seen in &SEEN {
        seen.store(0, Ordering::Release);
    }
}

/// Where spinners `range` ran since the last [`clear`], together.
fn seen(range: core::ops::Range<usize>) -> u64 {
    SEEN.get(range)
        .unwrap_or(&[])
        .iter()
        .fold(0, |all, seen| all | seen.load(Ordering::Acquire))
}

/// `cpus` as a mask of the first 64.
fn mask(cpus: &CpuSet) -> u64 {
    cpus.iter()
        .filter_map(|cpu| u32::try_from(cpu).ok())
        .filter_map(|cpu| 1_u64.checked_shl(cpu))
        .fold(0, |all, bit| all | bit)
}

/// Run them, and what ran, for the boot log.
///
/// # Errors
///
/// The first case that failed, as a sentence.
///
/// Verifies: H.SCHED.23, L.sched.122, L.sched.127
pub(super) fn run() -> Result<&'static str, &'static str> {
    if !super::realtime_built() {
        if domains::create_soft(1) != Err(Refused::Unavailable) {
            return Err("a SoftRt domain was not refused as unavailable without the tier");
        }
        return Ok("no tier above fair: a SoftRt domain refused as unavailable");
    }
    let online = super::online();
    if online < 4 {
        if domains::create_soft(1) != Err(Refused::TooFew) {
            return Err("a SoftRt domain was not refused on fewer than four processors");
        }
        return Ok("fewer than four processors: a SoftRt domain refused");
    }
    refusals(online)?;
    STOP.store(false, Ordering::Release);
    STARTED.store(0, Ordering::Release);
    let mut spinners = Vec::new();
    for index in 0..SPINNERS {
        let task = super::spawn("domain-spinner", spinner, index, NICE_0_WEIGHT)?;
        crate::fallible::try_push(&mut spinners, task).map_err(|_| "no memory for a spinner")?;
    }
    let outcome = started().and_then(|()| partition(&spinners, online));
    STOP.store(true, Ordering::Release);
    domains::dissolve();
    for task in &spinners {
        super::wait_until_gone(task, GONE_NANOS)?;
    }
    outcome?;
    if domains::moving() != 0 || domains::moving_lost() != 0 {
        return Err("a task taken off its queue for its domain was never placed");
    }
    Ok(
        "SoftRt refusals, the partition under a running boot domain, admission and \
         dissolution",
    )
}

/// D-3's refusals, before any domain exists (V9).
///
/// Verifies: H.SCHED.23, L.sched.127
fn refusals(online: usize) -> Result<(), &'static str> {
    if domains::create_soft(0) != Err(Refused::Count) {
        return Err("a SoftRt domain of no processors was not refused");
    }
    if domains::create_soft(online - 1) != Err(Refused::Count) {
        return Err("a SoftRt domain leaving the boot domain one processor was not refused");
    }
    if domains::soft_exists() {
        return Err("a refused SoftRt domain was made anyway");
    }
    Ok(())
}

/// Wait until every spinner runs.
fn started() -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(GONE_NANOS);
    while STARTED.load(Ordering::Acquire) < SPINNERS {
        if crate::timer::now_nanos() >= deadline {
            return Err("the domain check's spinners never started");
        }
        super::sleep_for(1_000_000);
    }
    Ok(())
}

/// A job of no processes admitted (V10): it and a job made beneath it are
/// in the domain. Answers the one beneath, for the dissolution's look.
///
/// Verifies: L.sched.124
fn a_job_admitted() -> Result<Arc<Job>, &'static str> {
    let job = crate::object::job::root()
        .new_child()
        .map_err(|_| "no job to admit")?;
    if domains::admit(&job) != Ok(0) {
        return Err("a job of no processes was refused admission");
    }
    let beneath = job
        .new_child()
        .map_err(|_| "no job beneath the admitted one")?;
    if job.sched_slot() != domains::SOFT || beneath.sched_slot() != domains::SOFT {
        return Err("an admitted job, or a job made beneath it, is not in the SoftRt domain");
    }
    Ok(beneath)
}

/// The partition, admission and dissolution, with `spinners` spinning.
///
/// Verifies: H.SCHED.21, H.SCHED.22, L.sched.118, L.sched.119, L.sched.120
/// Verifies: L.sched.121, L.sched.123, L.sched.124, L.sched.125, L.sched.126
fn partition(spinners: &[Arc<Task>], online: usize) -> Result<(), &'static str> {
    let soft = domains::create_soft(online - 2)
        .map_err(|_| "a SoftRt domain of all but two processors was refused")?;
    let soft = mask(&soft);
    if domains::create_soft(1) != Err(Refused::Exists) {
        return Err("a second SoftRt domain was not refused");
    }
    clear();
    super::sleep_for(LOOK_NANOS);
    if seen(0..SPINNERS) & soft != 0 {
        return Err("a task of the boot domain ran on a SoftRt domain's processor");
    }
    let admitted = spinners.get(..2).ok_or("no spinners to admit")?;
    if domains::admit_tasks(admitted) != Ok(2) {
        return Err("two kernel tasks were refused admission");
    }
    let job = a_job_admitted()?;
    super::sleep_for(SETTLE_NANOS);
    clear();
    super::sleep_for(LOOK_NANOS);
    let inside = seen(0..2);
    if inside == 0 || inside & !soft != 0 {
        return Err("an admitted task did not run, or ran outside its domain's processors");
    }
    if seen(2..SPINNERS) & soft != 0 {
        return Err(
            "a task of the boot domain ran on a SoftRt domain's processor after an admission",
        );
    }
    domains::dissolve();
    if domains::soft_exists() {
        return Err("a dissolved SoftRt domain still exists");
    }
    if job.sched_slot() != domains::BOOT {
        return Err("a job admitted to a dissolved SoftRt domain is not in the boot domain");
    }
    super::sleep_for(SETTLE_NANOS);
    clear();
    super::sleep_for(LOOK_NANOS);
    if seen(0..SPINNERS) & soft == 0 {
        return Err(
            "a dissolved SoftRt domain's processors were not given back to the boot domain",
        );
    }
    Ok(())
}
