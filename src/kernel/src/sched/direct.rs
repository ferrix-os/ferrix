//! The direct switch of step 4's fast path for `channel_write_read`
//! (`docs/OPAQUE-KERNEL.md` §9.7, part 1).
//!
//! # What it is
//!
//! A program that answers another's message and waits for the next one, on a
//! processor with nothing else to run, hands the processor to the program it
//! answered without the general path's wake, wait queue and pick. The pick
//! could choose nothing else: the fair class holds only the caller, and the
//! woken task is asleep at home here. So the switch skips the pick and keeps
//! everything else, by calling the general path's own functions:
//! [`CpuQueue::hand_over`] is the wake's `insert` and `choose_next`'s
//! `account`, `detach_current` and `pick_next`, and `switch_chosen` is
//! `choose_next`'s tail.
//!
//! # Who calls it, and under what
//!
//! Only the fast path (`object::channel::Endpoint::send_direct`), with
//! interrupts masked from the `SYSCALL` entry, holding the two halves' inbox
//! locks. [`begin`] takes this processor's run-queue lock by `try_lock`: a
//! lock that is held is a failed test, and the general path runs. Nothing
//! here waits. [`Direct::hand_over`] commits; [`Direct::switch`] switches and
//! returns when the caller runs again, possibly much later.
//!
//! # The tests and the asserts
//!
//! T11 to T13 are tests, counted when they decline ([`count`]). A1 and A4
//! follow from them and from the park's invariants, and stop the machine
//! when they do not hold: a test that cannot fail cannot have a control
//! that fires (part 2, *Asserted, not tested*). A3 is FX-0503's own check,
//! made where `schedule_from` makes it.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_sync::SpinLock;

use super::queue::CpuQueue;
use super::task::{BLOCKED, RUNNABLE, Task};
use super::{finish_switch, queue_of, switch_chosen, this_cpu, work};
use crate::arch;

/// What the fast path counts: trips taken, parks made, and each test's
/// declines (`docs/OPAQUE-KERNEL.md` §9.7, part 6). Kernel statistics no
/// program can read; the boot prints them when the shell exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Count {
    /// A full trip: the message delivered to a parked reader, the caller
    /// parked, and the processor handed over.
    Trip,
    /// A receive half alone: the caller parked through the general block.
    Park,
    /// T2: the call may be filtered.
    T2,
    /// T3: more than 24 bytes, or a receive-only call (served by the
    /// receive half on the general path).
    T3,
    /// T4 and T5, and the handle table's lock held: the lookup did not give
    /// an endpoint with READ and WRITE.
    T4,
    /// A half's inbox lock held.
    Halves,
    /// T6: the caller's inbox holds a message.
    T6,
    /// T7: a task is already parked on the caller's half.
    T7,
    /// T8: the caller's peer has closed.
    T8,
    /// T9: nobody is parked on the peer's half.
    T9,
    /// T10: the peer's half has observers or waiters.
    T10,
    /// This processor's run queue lock held.
    Queue,
    /// T11: the parked task's home is another processor, or it does not
    /// hold both its slots (asleep elsewhere's state 3).
    T11,
    /// T12: something waits in the fair class, or a sleeper is due.
    T12,
    /// T13: the caller or the peer has `END` posted.
    T13,
}

/// How many [`Count`]s there are.
const COUNTS: usize = Count::T13 as usize + 1;

/// The processors counted apart; any further ones share the last row.
const COUNTED_PROCESSORS: usize = 64;

/// The counts, one row per processor so that each is written by one
/// processor alone, with interrupts masked: a load and a store, no locked
/// operation on the trip.
static COUNTED: [[AtomicU64; COUNTS]; COUNTED_PROCESSORS] =
    [const { [const { AtomicU64::new(0) }; COUNTS] }; COUNTED_PROCESSORS];

/// Count one `what` on this processor. With interrupts masked.
pub(crate) fn count(what: Count) {
    let row = this_cpu().map_or(0, |cpu| cpu.min(COUNTED_PROCESSORS - 1));
    if let Some(cell) = COUNTED.get(row).and_then(|row| row.get(what as usize)) {
        cell.store(
            cell.load(Ordering::Relaxed).wrapping_add(1),
            Ordering::Relaxed,
        );
    }
}

/// Every count, summed over the processors, in [`Count`]'s order.
pub(crate) fn counts() -> [u64; COUNTS] {
    let mut sums = [0_u64; COUNTS];
    for row in &COUNTED {
        for (sum, cell) in sums.iter_mut().zip(row) {
            *sum = sum.wrapping_add(cell.load(Ordering::Relaxed));
        }
    }
    sums
}

/// A direct switch begun: this processor's run-queue lock held, T11 to T13
/// passed. Dropped without [`Direct::switch`], it lets the lock go, having
/// changed nothing.
pub(crate) struct Direct {
    /// This processor's run queue, whose lock is held.
    lock: &'static SpinLock<CpuQueue>,
    /// This processor.
    cpu: usize,
    /// The one clock reading of this direction.
    now: u64,
    /// The pick [`Direct::hand_over`] made, the peer.
    next: Option<Arc<Task>>,
}

/// Take this processor's run-queue lock if it is free, and make the tests
/// that need it for a switch from `caller`, the running task, to `peer`:
/// T11, T12 and T13. With interrupts masked.
///
/// # Errors
///
/// The [`Count`] of the test that declined, with the lock let go and
/// nothing changed.
pub(crate) fn begin(caller: &Task, peer: &Task) -> Result<Direct, Count> {
    let cpu = this_cpu().ok_or(Count::Queue)?;
    let lock = queue_of(cpu).ok_or(Count::Queue)?;
    // SAFETY: (SHARED) released by `Direct`'s drop when the switch is not
    // made, and otherwise by the context switched to, in `finish_switch`, as
    // `choose_next`'s is.
    let queue = unsafe { lock.try_lock_manually() }.ok_or(Count::Queue)?;
    let mut direct = Direct {
        lock,
        cpu,
        now: 0,
        next: None,
    };
    // T11: the parked task's home is this processor, and it holds both its
    // slots. A parked task files no deadline, but one woken early from an
    // earlier sleep and moved may have left its sleep slot in a sleeper set
    // elsewhere (state 3 of `wake_with`), which only that set gives back.
    // Its run slot is held exactly while no queue holds it, which A1 asserts
    // (`is_queued`, set and cleared with the slot under the queue's lock).
    if peer.cpu() != cpu || !peer.holds_sleep_slot() {
        return Err(Count::T11);
    }
    // T12: nothing waits in the fair class, and no sleeper is due, so that
    // the pick could only choose the peer and `wake_sleepers` would take
    // nothing. Declined rather than woken here, so that a decline leaves the
    // queue as it was.
    let now = crate::timer::now_nanos();
    if queue.waiting() != 0 || queue.sleeper_due(now) {
        return Err(Count::T12);
    }
    // T13, the last look, under the lock every poster's wake takes before it
    // reads the state (`sched::work`'s wake row): an end posted before this
    // is seen here, and one after finds the caller parked and wakes it.
    work::fast_path_hook(caller);
    if work::has_end(caller) || work::has_end(peer) {
        return Err(Count::T13);
    }
    direct.now = now;
    Ok(direct)
}

impl Direct {
    /// The queue whose lock this holds.
    fn queue(&mut self) -> &mut CpuQueue {
        // SAFETY: (SHARED) `begin` took this lock and nothing has let it go:
        // this is the only reference into it.
        unsafe { self.lock.locked_data() }
    }

    /// The commit's scheduler half (part 2, *The commit*, steps 2 and 4):
    /// `peer` made runnable, joined to this queue while `caller` still runs,
    /// `block_caller` run (the caller set blocked and parked), and the pick.
    ///
    /// # Asserted
    ///
    /// A4: neither task is this processor's idle task. A1: the peer is
    /// asleep at home -- blocked, neither running here nor queued; `begin`
    /// tested that it holds both its slots -- and T11 and T12 leave no way
    /// for a parked task to be otherwise. Each stops the machine with its
    /// code.
    ///
    pub(crate) fn hand_over(
        &mut self,
        caller: &Task,
        peer: Arc<Task>,
        block_caller: impl FnOnce(Arc<Task>),
    ) {
        let now = self.now;
        let queue = self.queue();
        let idle = queue
            .idle
            .as_ref()
            .is_some_and(|idle| core::ptr::eq(&**idle, caller) || Arc::ptr_eq(idle, &peer));
        if idle {
            crate::panic::fatal!(
                crate::panic::catalog::FAST_PATH_IDLE_TASK,
                "the direct switch was asked to switch from or to the idle task (A4)"
            );
        }
        let running = queue
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &peer));
        // Its slots were tested under this lock hold (T11, `begin`), and
        // only a holder of a queue's lock moves them.
        if running || peer.is_queued() || peer.state() != BLOCKED {
            crate::panic::fatal!(
                crate::panic::catalog::FAST_PATH_NOT_ASLEEP,
                "a parked task about to be handed a reply was running or queued (A1): task {}",
                peer.id
            );
        }
        // A parked task files no deadline, but whatever sleep it once meant
        // is over, as the wake at home ends it. It is in no sleeper set to
        // be taken out of: it holds its sleep slot (T11), which a set keeps
        // while it holds the task.
        let _ = peer.take_sleep_deadline();
        // Asleep, as A1 has just asserted, and every waker needs this lock.
        peer.set_state_from(BLOCKED, RUNNABLE);
        let id = peer.id;
        let pointer = Arc::as_ptr(&peer);
        let next = queue.hand_over(peer, now, block_caller);
        if !next
            .as_ref()
            .is_some_and(|next| core::ptr::eq(Arc::as_ptr(next), pointer))
        {
            crate::panic::fatal!(
                crate::panic::catalog::FAST_PATH_NOT_ASLEEP,
                "the direct switch's pick was not the parked task it handed over to (A1): task {}",
                id
            );
        }
        if queue.stats.measuring {
            queue.note_pick(now);
        }
        self.next = next;
    }

    /// Switch to the task [`Direct::hand_over`] picked, and return when the
    /// caller runs again: `switch_chosen`, the switch, and `finish_switch`,
    /// as `pick_and_switch` makes them. A3, FX-0503's check, first.
    pub(crate) fn switch(mut self) {
        super::require_preemption_on(self.cpu);
        let (cpu, now) = (self.cpu, self.now);
        let next = self.next.take();
        let lock = self.lock;
        let previous = self.queue().current.take();
        let (Some(previous), Some(next)) = (previous, next) else {
            crate::panic::fatal!(
                crate::panic::catalog::FAST_PATH_NOT_ASLEEP,
                "the direct switch had no task to switch from or to"
            );
        };
        let Some((save, resume)) = switch_chosen(lock, cpu, previous, next, now) else {
            crate::panic::fatal!(
                crate::panic::catalog::FAST_PATH_NOT_ASLEEP,
                "the direct switch's queue lost the tasks it was given"
            );
        };
        // The lock goes with the switch, to the context switched to.
        core::mem::forget(self);
        // SAFETY: (CONTEXT) as `pick_and_switch`'s: `save` is this context's
        // own slot and `resume` a stack pointer this module saved, and this
        // processor holds the run queue's lock until `finish_switch`.
        unsafe { arch::switch_to(save, resume) };
        finish_switch();
    }
}

impl Drop for Direct {
    /// A declined switch lets the lock go, having changed nothing.
    fn drop(&mut self) {
        // SAFETY: (SHARED) taken by `begin` and not handed to another context:
        // `switch` forgets `self` before it switches.
        unsafe { self.lock.force_unlock() };
    }
}

/// The receive half alone's block (part 2): the caller set itself blocked
/// and parked under its half's lock; after a `SeqCst` fence -- paired with
/// the one `work::wake_posted` makes after a poster's bit -- the last look
/// at `END`, then the general block. Answers whether it blocked; on `END`
/// it is set runnable again and does not. With interrupts on, as
/// `wait_trusting`'s block is made.
pub(crate) fn block_parked(task: &Task) -> bool {
    core::sync::atomic::fence(Ordering::SeqCst);
    if work::has_end(task) {
        task.set_state(RUNNABLE);
        return false;
    }
    super::schedule();
    true
}

/// Set `task` blocked: the park's record is set only beside a blocked task,
/// under its half's lock.
pub(crate) fn set_blocked(task: &Task) {
    task.set_state(BLOCKED);
}

/// [`set_blocked`] for the direct switch's caller, which runs here under the
/// run-queue lock with interrupts masked: see `Task::set_state_from`.
pub(crate) fn set_running_blocked(task: &Task) {
    task.set_state_from(RUNNABLE, BLOCKED);
}
