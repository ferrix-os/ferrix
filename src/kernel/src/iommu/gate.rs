//! Waiting on a unit with interrupts on.
//!
//! A VT-d invalidation and an `SMMUv3` command are finished when the unit says
//! so: at once under QEMU, in microseconds on hardware, and only after a
//! unit's patience on one that is busy or broken. Every unpin from a
//! translated domain waits for one, and virtio-blk pins and unpins on its
//! request path, so the wait must not mask interrupts, as it did inside the
//! `IrqSpinLock`s it was first written under.
//!
//! * A [`Gate`] is what an operation holds across its wait instead: one
//!   domain's pins and unpins, or one unit's commands, one at a time. A task
//!   waiting to enter sleeps on a wait queue.
//! * [`poll`] is the wait itself: it looks at the unit, and between looks
//!   gives up the processor.
//!
//! Both need a context that may block: a task, on a processor taking
//! interrupts, with nothing holding preemption off. Anywhere else — before
//! the scheduler, or under a spin lock — they spin, as the locks did, and the
//! deadline still ends them. So a spinning waiter cannot hang on a holder
//! switched out on its own processor: its deadline answers an error, and the
//! caller keeps its pages rather than freeing what a unit may still reach.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::sched::{self, WaitQueue};
use crate::timer;

/// How long an operation waits to enter a gate: longer than a holder's
/// longest stay, two commands at a unit's full patience each.
const ENTRY_PATIENCE_NANOS: u64 = 1_000_000_000;

/// Looks at a unit before a waiter that may block starts giving up its
/// processor between them.
const SPINS: u32 = 64;

/// Waits on a unit made in a context that may block.
static WAITS_WITH_INTERRUPTS_ON: AtomicU64 = AtomicU64::new(0);

/// One operation at a time, waited for by sleeping.
#[derive(Debug)]
pub(crate) struct Gate {
    /// Whether an operation is inside.
    held: AtomicBool,
    /// Tasks waiting to enter.
    waiters: WaitQueue,
}

/// Inside a [`Gate`] until dropped.
#[derive(Debug)]
#[must_use = "the gate is left as soon as this drops"]
pub(crate) struct Entered<'a> {
    /// The gate entered.
    gate: &'a Gate,
}

impl Gate {
    /// A gate nobody is inside.
    pub(crate) const fn new() -> Gate {
        Gate {
            held: AtomicBool::new(false),
            waiters: WaitQueue::new(),
        }
    }

    /// Enter, once whoever is inside has left.
    ///
    /// # Errors
    ///
    /// Whoever was inside stayed past [`ENTRY_PATIENCE_NANOS`].
    pub(crate) fn enter(&self) -> Result<Entered<'_>, &'static str> {
        self.enter_within(ENTRY_PATIENCE_NANOS)
    }

    /// [`Gate::enter`], giving up after `patience` nanoseconds rather than
    /// the unit's: for `iommu/check.rs`, which proves a held gate refuses the
    /// next entry and has no reason to spend a second of every boot on it.
    ///
    /// # Errors
    ///
    /// Whoever was inside stayed past `patience`.
    pub(crate) fn enter_within(&self, patience: u64) -> Result<Entered<'_>, &'static str> {
        let deadline = timer::now_nanos().saturating_add(patience);
        let take = || {
            self.held
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        };
        let entered = if sched::may_block() {
            self.waiters.wait_until_deadline(take, deadline)
        } else {
            look(take, deadline, false)
        };
        if entered {
            Ok(Entered { gate: self })
        } else {
            Err("another operation on the unit never finished")
        }
    }
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.gate.held.store(false, Ordering::Release);
        self.gate.waiters.wake_all();
    }
}

/// Look at `ready` until it answers true or `deadline` passes, and answer
/// which. Between looks, a context that may block gives up its processor.
pub(crate) fn poll(ready: impl FnMut() -> bool, deadline: u64) -> bool {
    let blocking = sched::may_block();
    if blocking {
        let _ = WAITS_WITH_INTERRUPTS_ON.fetch_add(1, Ordering::Relaxed);
    }
    look(ready, deadline, blocking)
}

/// How many waits on a unit were made in a context that may block.
pub fn waits_with_interrupts_on() -> u64 {
    WAITS_WITH_INTERRUPTS_ON.load(Ordering::Relaxed)
}

/// [`poll`]'s loop, yielding between looks only when `blocking`.
fn look(mut ready: impl FnMut() -> bool, deadline: u64, blocking: bool) -> bool {
    let mut looks = 0_u32;
    loop {
        if ready() {
            return true;
        }
        if timer::now_nanos() > deadline {
            return ready();
        }
        looks = looks.saturating_add(1);
        if blocking && looks > SPINS {
            sched::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }
}
