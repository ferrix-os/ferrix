//! The self-check of a wait's deadline (L.sched.92): what every bounded
//! wait in the kernel rests on, among them the crash hold's 2 s
//! (`docs/LOGGING.md` §21.7).
//!
//! Two cases, each run in a task of its own so that a wait which never ends
//! is found by the checker's own sleep, not by another wait queue's
//! deadline:
//!
//! * **Passed**: a wait whose condition stays false returns false once the
//!   clock has passed its deadline, 20 ms out, and within a second after it.
//! * **Woken**: a wait whose condition a waker makes true and then wakes the
//!   queue returns true at once, long before its ten-second deadline.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched_fair::NICE_0_WEIGHT;

use super::WaitQueue;

/// How far out the passed case's deadline is.
const SHORT_NANOS: u64 = 20_000_000;

/// How far out the woken case's deadline is.
const LONG_NANOS: u64 = 10_000_000_000;

/// How late a wait may end after its deadline, or after its wake.
const MARGIN_NANOS: u64 = 1_000_000_000;

/// The queue the cases wait on.
static QUEUE: WaitQueue = WaitQueue::new();

/// The woken case's condition.
static READY: AtomicBool = AtomicBool::new(false);

/// What a case's waiter found: 0 nothing yet, 1 false, 2 true; and when it
/// returned.
static ANSWER: AtomicU64 = AtomicU64::new(0);
static RETURNED_AT: AtomicU64 = AtomicU64::new(0);

/// The waiter: one wait to `argument` nanoseconds from now.
fn waiter(argument: usize) {
    let deadline = crate::timer::now_nanos().saturating_add(argument as u64);
    let answer = QUEUE.wait_until_deadline(|| READY.load(Ordering::Acquire), deadline);
    RETURNED_AT.store(crate::timer::now_nanos(), Ordering::Release);
    ANSWER.store(if answer { 2 } else { 1 }, Ordering::Release);
}

/// Start a waiter for `nanos`, and sleep until it answers or `patience`
/// passes; its answer and when it returned.
fn one(nanos: u64, patience: u64, wake_after: Option<u64>) -> Result<(u64, u64), &'static str> {
    ANSWER.store(0, Ordering::Release);
    let started = crate::timer::now_nanos();
    let task = super::spawn("deadline-wait", waiter, nanos as usize, NICE_0_WEIGHT)?;
    if let Some(after) = wake_after {
        super::sleep_for(after);
        READY.store(true, Ordering::Release);
        QUEUE.wake_all();
    }
    let give_up = started.saturating_add(patience);
    while ANSWER.load(Ordering::Acquire) == 0 {
        if crate::timer::now_nanos() > give_up {
            return Err(
                "a wait did not return by its deadline: wait_until_deadline \
                        waits past the deadline it was given",
            );
        }
        super::sleep_for(1_000_000);
    }
    super::wait_until_gone(&task, super::REAPER_PATIENCE_NANOS)?;
    Ok((
        ANSWER.load(Ordering::Acquire),
        RETURNED_AT.load(Ordering::Acquire).saturating_sub(started),
    ))
}

/// Run the two cases.
///
/// # Errors
///
/// The sentence of the first case that failed.
/// Verifies: `L.sched.92`
pub(super) fn run() -> Result<(), &'static str> {
    READY.store(false, Ordering::Release);
    let (answer, took) = one(SHORT_NANOS, SHORT_NANOS + MARGIN_NANOS, None)?;
    if answer != 1 || took < SHORT_NANOS {
        return Err("a wait whose condition stayed false did not return false at its deadline");
    }
    let (answer, took) = one(LONG_NANOS, MARGIN_NANOS * 2, Some(5_000_000))?;
    READY.store(false, Ordering::Release);
    if answer != 2 || took > MARGIN_NANOS {
        return Err("a wait woken with its condition true did not return true at once");
    }
    Ok(())
}
