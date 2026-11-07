//! The deadline tier's reservations: one body, for the native call and the boot check alike
//! (`docs/microkernel/K9.md`, `docs/SCHED-CLASSES.md` §9g; ledger 7011 S.4, 7088, 7763).
//! Every way in answers [`Refusal::Unschedulable`] in a stack without a deadline tier
//! ([`super::stack::DEADLINE`]), whose tasks' cells hold nothing.
//!
//! A reservation names one processor, the first with room, the caller's own first. The thread
//! is pinned there, so balance, steal, push and wake placement cannot move it, and
//! `set_affinity` answers `Reserved`. Death gives the share back when the task is freed. A
//! child starts on the fair class. A processor taken out of service moves its reserved
//! threads with the rest: they run on the fair class wherever they land, and their share
//! stays counted where it was until they end or give it back, which is stated rather than
//! re-admitted.
//!
//! What this does not bound: a thread throttled while it holds a kernel sleep lock, or
//! linuxd's arena lock, holds up everyone behind it until its refill, with no inheritance;
//! and a reserved thread that waits on `btrfsd` or a driver waits on the fair class's time
//! (7088 E.4). No timing claim is made of any of it.

use alloc::sync::Arc;

use ferrix_sched_class::CpuSet;
use ferrix_sched_class::deadline::{NONE, ReserveCell};
pub use ferrix_sched_class::deadline::{Refusal, Reservation};
use ferrix_sched_fair::StackTiers;

use super::stack::{DEADLINE, Stack};
use super::{Task, affinity, current, queue_of, this_cpu};
use crate::arch;
use ferrix_sync::IrqControl;

/// Whether `task` holds a reservation.
pub(crate) fn holds(task: &Task) -> bool {
    DEADLINE && task.reserve.on() != NONE
}

/// Have `cpu`'s queue admit `id`, masked.
fn admit_on(cpu: usize, id: u64, asked: Reservation) -> Result<(), Refusal> {
    let Some(lock) = queue_of(cpu) else {
        return Err(Refusal::Unschedulable);
    };
    let saved = <arch::Irq as IrqControl>::disable();
    let answer = <Stack as StackTiers<_>>::admit(&mut lock.lock().stack, id, asked);
    <arch::Irq as IrqControl>::restore(saved);
    answer
}

/// The running thread takes `asked`.
///
/// # Errors
///
/// [`Refusal`], with nothing changed: `Unschedulable` when no processor it may run on has room.
pub fn take(asked: Reservation) -> Result<(), Refusal> {
    let _ = asked.share_ppm()?;
    if !DEADLINE {
        return Err(Refusal::Unschedulable);
    }
    let task = current().ok_or(Refusal::Malformed)?;
    if holds(&task) {
        return Err(Refusal::Already);
    }
    let here = this_cpu().ok_or(Refusal::Malformed)?;
    let mask = affinity::mask_for(&task);
    let mut last = Refusal::Unschedulable;
    let others = (0..crate::smp::count()).filter(|cpu| *cpu != here);
    for cpu in core::iter::once(here).chain(others) {
        if !mask.contains(cpu) {
            continue;
        }
        match admit_on(cpu, task.id, asked) {
            Ok(()) => return pinned_to(&task, cpu, mask),
            Err(why @ (Refusal::Malformed | Refusal::Already)) => return Err(why),
            Err(why) => last = why,
        }
    }
    Err(last)
}

/// Pin the thread `cpu` admitted, and let the classes choose again: at that decision its
/// processor moves it into the deadline tier (`CpuQueue::requeue_if_moved`), so it is never a
/// waiting entity while it runs (container review 30, B1; ledger 7763).
fn pinned_to(task: &Arc<Task>, cpu: usize, mask: CpuSet) -> Result<(), Refusal> {
    task.reserve.keep_mask(Some(mask));
    if affinity::set_mask(task, &CpuSet::of(cpu)).is_err() {
        let _ = withdraw(task, cpu);
        task.reserve.keep_mask(None);
        return Err(Refusal::Unschedulable);
    }
    let _ = task.reserve.swap_on(cpu);
    super::yield_now();
    Ok(())
}

/// Take `task`'s reservation off `cpu`, it running or queued there.
fn withdraw(task: &Task, cpu: usize) -> bool {
    let Some(lock) = queue_of(cpu) else {
        return false;
    };
    let saved = <arch::Irq as IrqControl>::disable();
    let done = <Stack as StackTiers<_>>::withdraw(&mut lock.lock().stack, task.id);
    <arch::Irq as IrqControl>::restore(saved);
    done
}

/// The running thread gives its reservation back: the fair class's again, with its mask.
///
/// # Errors
///
/// [`Refusal::Already`] for a thread that holds none.
pub fn give_back() -> Result<(), Refusal> {
    let task = current().ok_or(Refusal::Malformed)?;
    let cpu = task.reserve.swap_on(NONE);
    if cpu == NONE {
        return Err(Refusal::Already);
    }
    let _ = withdraw(&task, cpu);
    let before = task.reserve.take_mask();
    if let Some(mask) = before {
        let _ = affinity::set_mask(&task, &mask);
    }
    // Off the processor once, and it is the fair class's: the decision moves it (DG.7).
    super::yield_now();
    Ok(())
}

/// The share admitted on `cpu`, and its throttles and refills so far.
#[cfg(ferrix_checks = "on")]
#[must_use]
pub(crate) fn state_of(cpu: usize) -> Option<(u64, u64, u64)> {
    let lock = queue_of(cpu)?;
    let saved = <arch::Irq as IrqControl>::disable();
    let (used, (throttles, refills)) = {
        let queue = lock.lock();
        let (used, throttles, refills) = <Stack as StackTiers<_>>::reserve_state(&queue.stack);
        (used, (throttles, refills))
    };
    <arch::Irq as IrqControl>::restore(saved);
    Some((used, throttles, refills))
}

/// Whether the running thread holds one.
#[must_use]
pub fn holds_own() -> bool {
    current().is_some_and(|task| holds(&task))
}

/// A task being freed gives its share back, wherever it is held: it may have died before it
/// ever moved to the processor that admitted it (ledger 7088 E.3).
pub(crate) fn forget(task: &Task) {
    let cpu = task.reserve.swap_on(NONE);
    if cpu == NONE {
        return;
    }
    let _ = withdraw(task, cpu);
}
