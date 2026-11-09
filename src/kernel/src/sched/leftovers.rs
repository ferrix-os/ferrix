//! What a task's last reference gives back when it is dropped where sleeping is
//! forbidden (F-104, `L.sched.171`, `L.sched.172`).
//!
//! An `Arc<Task>` can outlive the reaper's: a timer's entry on another processor's
//! sleeper set, the waker's own copy in `WaitQueue::wake_all_with`. Its last drop then
//! runs where that holder runs, an interrupt or under a run queue's lock, and a task's
//! fields go a long way: its process, its job, a channel it held, a pin, a domain's
//! detach, which waits on a gate. So [`Task`]'s drop, where [`super::may_block`] does
//! not hold, moves them here, into a node the task was made with, and allocates
//! nothing. What stays is bounded and takes no lock that sleeps: `Drop for Task` says
//! which, and `check-stack-depth` prints its depth.
//!
//! No wake from the drop itself, which may hold the queue's lock a wake takes. Nor an
//! early timer: a one-shot fired ahead of its decision on an idle processor lost that
//! processor's next deadline, in a third of AArch64 `--smp 2` boots. [`nudge`] wakes the
//! reaper from the three places every such drop is followed by, with no lock held: the
//! end of any interrupt, anywhere in the machine (`trap::dispatch`); the end of a
//! switch (`switch_from`), for a drop under a queue's lock; and the idle loop's look
//! before it halts, for a drop on the way there. So the bound is the machine's next
//! interrupt, switch or halt, whichever is first: a lone spinning task on a tickless
//! processor with no interrupt anywhere would hold it longer, and holds nothing else up.
//! A lock-free list, so that a push is safe under anything.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use super::wait::WaitQueue;
use crate::arch;
use crate::object::process::Process;
use crate::user::space::AddressSpace;

/// A task's fields that may not be dropped where sleeping is forbidden.
#[derive(Debug)]
pub(crate) struct Leftovers {
    /// The next on [`WAITING`].
    next: *mut Leftovers,
    /// See `Task::address_space`.
    pub(super) address_space: Option<Arc<AddressSpace>>,
    /// See `Task::core`.
    pub(super) core: Option<Arc<Process>>,
    /// See `Task::thread`.
    pub(super) thread: Option<Arc<dyn super::task::UserThread>>,
    /// See `Task::user`.
    pub(super) user: Option<Box<core::cell::UnsafeCell<arch::UserState>>>,
    /// See `Task::restricted_state`.
    pub(super) restricted: Option<Box<arch::RestrictedState>>,
}

impl Leftovers {
    /// An empty node.
    pub(super) const fn new() -> Leftovers {
        Leftovers {
            next: core::ptr::null_mut(),
            address_space: None,
            core: None,
            thread: None,
            user: None,
            restricted: None,
        }
    }
}

// SAFETY: (SHARED) a node is reached by one holder at a time: the task whose drop fills
// it, then the list, then the reaper. Its cells are not read through until they are dropped.
unsafe impl Send for Leftovers {}

/// Nodes waiting for the reaper, newest first.
static WAITING: AtomicPtr<Leftovers> = AtomicPtr::new(core::ptr::null_mut());

/// Where the reaper waits.
static REAPER: WaitQueue = WaitQueue::new();

/// How many tasks' last references were dropped where sleeping is forbidden.
static DEFERRED: AtomicU64 = AtomicU64::new(0);

/// How many of those the reaper has finished.
static FINISHED: AtomicU64 = AtomicU64::new(0);

/// Hand `node` to the reaper. Allocates nothing, and takes no lock.
pub(super) fn hand_over(node: Box<Leftovers>) {
    let _ = DEFERRED.fetch_add(1, Ordering::Relaxed);
    let raw = Box::into_raw(node);
    let mut head = WAITING.load(Ordering::Acquire);
    loop {
        // SAFETY: (KMEM) `raw` is not on the list yet, so it is still only ours.
        unsafe { (*raw).next = head };
        match WAITING.compare_exchange_weak(head, raw, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(now) => head = now,
        }
    }
}

/// Whether the reaper has anything to do: a drop's leftovers, or a purge of
/// the large-request area that a free which could not block asked for
/// (`mm::large`, `docs/ROBUSTNESS.md` §2.6, R6.2).
fn wanted() -> bool {
    !WAITING.load(Ordering::Acquire).is_null() || crate::mm::large::purge_wanted()
}

/// Wake the reaper if a drop has left it anything, or a purge is wanted. Two
/// loads when not. With no lock held; see the module's head.
pub(crate) fn nudge() {
    if wanted() {
        REAPER.wake_all();
    }
}

/// The reaper: a task, so that what it drops may sleep.
pub(super) fn reaper(_: usize) {
    loop {
        let _ = REAPER.wait_trusting(wanted);
        // The purge first: it frees and allocates nothing a leftover's drop
        // needs, and a drop that blocks would hold it up.
        if crate::mm::large::purge_wanted() {
            let _ = crate::mm::large::purge();
        }
        let head = WAITING.swap(core::ptr::null_mut(), Ordering::AcqRel);
        let mut node = head;
        while !node.is_null() {
            // SAFETY: (KMEM) the swap took the whole list, and only `hand_over`
            // publishes, each node from `Box::into_raw`.
            let this = unsafe { Box::from_raw(node) };
            node = this.next;
            drop(this);
            let _ = FINISHED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Whether every drop handed over so far is finished.
pub(super) fn settled() -> bool {
    let (deferred, finished) = counts();
    deferred == finished
}

/// Drops deferred so far, and how many of them are finished.
pub(crate) fn counts() -> (u64, u64) {
    (
        DEFERRED.load(Ordering::Relaxed),
        FINISHED.load(Ordering::Relaxed),
    )
}
