//! The wake row, checked (`docs/OPAQUE-KERNEL.md` §9.8, 2c case 10).
//!
//! A wake reads its target's state only under the run-queue lock of the
//! processor that owns the target: step 4's fast path will read its task's
//! [`END`](super::END) under that lock and block in the same hold, with no
//! fence, and is safe only because a waker cannot decide "runnable, nothing
//! to do" without that lock. The general path does not show it: its waits
//! order a kill against their last look with a fence pair, so an early exit
//! in the wake would lose none of their wakes. So this does what the fast
//! path will do, with a check-only blocker:
//!
//! 1. The blocker takes its home queue's lock, reads its `END` bit, says it
//!    is ready, and waits with the lock held until the poster on another
//!    processor is at its wake -- [`super::notify`]'s hook says so, between
//!    the post and the wake -- and a little longer, so that the poster's wake
//!    is under way. Then it marks itself blocked, lets the lock go, and
//!    switches out.
//! 2. The poster posts `END` to it through [`super::notify`].
//!
//! With the row kept, the poster's wake waits for the lock, finds the blocker
//! blocked or about to switch out, and makes it runnable: the blocker comes
//! back within the bound. A wake that read the state before the lock would
//! have found it runnable, done nothing, and left it blocked for good.
//!
//! The hook follows §9.7 condition 11's rules: set only here, in stage 9,
//! disarmed before this returns whatever it found, and required disarmed
//! after stage 9 ([`super::hook_armed_by`], FX-0908).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::{IrqControl, SpinLock};

use super::super::{Task, task};
use crate::arch;

/// How long the blocker waits, holding its lock, for the poster to reach its
/// wake.
const POSTER_BOUND_NANOS: u64 = 50_000_000;
/// How much longer it holds the lock once the poster is there, so that the
/// poster's wake has begun: waiting for the lock, or past a read it made
/// without it.
const SETTLE_NANOS: u64 = 1_000_000;
/// How soon the blocker must be back once it has blocked.
const WOKEN_BOUND_NANOS: u64 = 100_000_000;
/// How long the ready flag is waited for.
const READY_BOUND_NANOS: u64 = 2_000_000_000;
/// How many tries a run makes before a poster that never reached its wake
/// fails it: a busy host can keep a virtual processor from running for longer
/// than the blocker may hold its lock.
const TRIES: usize = 3;

/// The blocker has its lock and has read its bit.
static READY: AtomicBool = AtomicBool::new(false);
/// The poster is between its post and its wake: set by the hook.
static AT_WAKE: AtomicBool = AtomicBool::new(false);
/// The poster reached its wake while the blocker held its lock.
static REACHED: AtomicBool = AtomicBool::new(false);
/// The blocker read `END` already set as it took its lock.
static END_EARLY: AtomicBool = AtomicBool::new(false);
/// The blocker has come back from its block.
static WOKEN: AtomicBool = AtomicBool::new(false);
/// The blocker, for the poster.
static TARGET: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);

/// What [`super::notify`] calls between its post and its wake while a check
/// has armed the hook.
pub(super) fn before_wake(task: &Task) {
    if super::armed_by() == Some(super::HOOK_WAKE_ROW)
        && super::HOOK_TARGET.load(Ordering::Acquire) == task.id
    {
        AT_WAKE.store(true, Ordering::Release);
    }
}

/// What the check found, for its boot line: the tries it took.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// The try that reached a decision, from one.
    pub tries: usize,
}

/// The wake row's check. `Ok(None)` on a machine with one processor, where
/// there is no other processor for the poster.
///
/// # Errors
///
/// The blocker left blocked past the bound, its bit seen before the post, a
/// poster that never reached its wake in [`TRIES`] tries, or a task that
/// could not be made or did not end.
/// Verifies: `L.sched.32`, `L.sched.34`
pub fn run() -> Result<Option<Report>, &'static str> {
    let count = crate::smp::count();
    if count < 2 {
        return Ok(None);
    }
    let here = super::super::this_cpu().unwrap_or(0);
    let home = (here + 1) % count;
    let elsewhere = if count > 2 { (here + 2) % count } else { here };
    for tries in 1..=TRIES {
        let outcome = one_try(home, elsewhere);
        super::disarm();
        drop(TARGET.lock().take());
        if outcome? {
            return Ok(Some(Report { tries }));
        }
    }
    Err("the wake row: the poster never reached its wake while the blocker held its lock")
}

/// One blocker on `home` and one poster on `elsewhere`: `true` once a
/// decision was reached, `false` when the poster came too late to test the
/// row.
fn one_try(home: usize, elsewhere: usize) -> Result<bool, &'static str> {
    for flag in [&READY, &AT_WAKE, &REACHED, &END_EARLY, &WOKEN] {
        flag.store(false, Ordering::Release);
    }
    // The poster first: a spawn may take every run queue's lock in turn,
    // and one made while the blocker holds its own would wait the hold out.
    let poster = super::super::spawn_on(
        "wake-row-poster",
        post_end,
        0,
        NICE_0_WEIGHT,
        elsewhere,
        CpuSet::of(elsewhere),
    )?;
    let blocker = super::super::spawn_on(
        "wake-row-blocker",
        block_under_the_lock,
        0,
        NICE_0_WEIGHT,
        home,
        CpuSet::of(home),
    )?;
    super::arm(super::HOOK_WAKE_ROW, &blocker);
    *TARGET.lock() = Some(Arc::clone(&blocker));

    let deadline = crate::timer::now_nanos().saturating_add(READY_BOUND_NANOS);
    while !READY.load(Ordering::Acquire) {
        if crate::timer::now_nanos() >= deadline {
            return Err("the wake row: the blocker never took its lock");
        }
        super::super::sleep_for(100_000);
    }
    // Back within the bound once it blocked; the bound runs from when it
    // could first have blocked.
    let deadline = crate::timer::now_nanos()
        .saturating_add(POSTER_BOUND_NANOS + SETTLE_NANOS + WOKEN_BOUND_NANOS);
    while !WOKEN.load(Ordering::Acquire) && crate::timer::now_nanos() < deadline {
        super::super::sleep_for(1_000_000);
    }
    let woken = WOKEN.load(Ordering::Acquire);
    if !woken {
        // Woken for real, so that it ends: this wake comes after the block.
        super::super::wake(&blocker);
    }
    super::super::wait_until_gone(&blocker, super::super::REAPER_PATIENCE_NANOS)?;
    super::super::wait_until_gone(&poster, super::super::REAPER_PATIENCE_NANOS)?;
    if END_EARLY.load(Ordering::Acquire) {
        return Err("the wake row: the blocker read END before anything posted it");
    }
    if !REACHED.load(Ordering::Acquire) {
        return Ok(false);
    }
    if !woken {
        return Err(
            "the wake row: a wake made while its target held its own run-queue lock left it \
             blocked: the wake read the state without the lock",
        );
    }
    Ok(true)
}

/// The blocker: what step 4's fast path will do, in one hold of its home
/// queue's lock.
fn block_under_the_lock(_: usize) {
    let Some(me) = super::super::current() else {
        return;
    };
    let saved = <arch::Irq as IrqControl>::disable();
    if let Some(lock) = super::super::this_cpu().and_then(super::super::queue_of) {
        let queue = lock.lock();
        END_EARLY.store(super::has_end(&me), Ordering::Release);
        READY.store(true, Ordering::Release);
        let until = crate::timer::now_nanos().saturating_add(POSTER_BOUND_NANOS);
        while !AT_WAKE.load(Ordering::Acquire) && crate::timer::now_nanos() < until {
            core::hint::spin_loop();
        }
        if AT_WAKE.load(Ordering::Acquire) {
            REACHED.store(true, Ordering::Release);
            let settle = crate::timer::now_nanos().saturating_add(SETTLE_NANOS);
            while crate::timer::now_nanos() < settle {
                core::hint::spin_loop();
            }
        }
        me.set_state(task::BLOCKED);
        drop(queue);
    }
    <arch::Irq as IrqControl>::restore(saved);
    drop(me);
    super::super::block();
    WOKEN.store(true, Ordering::Release);
}

/// The poster: once the blocker is ready, post `END` to it the one way a
/// waker posts.
fn post_end(_: usize) {
    let deadline = crate::timer::now_nanos().saturating_add(READY_BOUND_NANOS);
    loop {
        let target = if READY.load(Ordering::Acquire) {
            TARGET.lock().clone()
        } else {
            None
        };
        if let Some(target) = target {
            super::notify(&target, super::END);
            return;
        }
        if crate::timer::now_nanos() >= deadline {
            return;
        }
        core::hint::spin_loop();
    }
}
