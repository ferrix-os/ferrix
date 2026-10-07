//! The running task, borrowed from the processor's record (OPAQUE-KERNEL.md
//! §9.8, 2a; the consultant's OK IF of 2026-10-02, condition 1).
//!
//! [`super::current`] used to take the run queue's lock with interrupts
//! masked and clone the queue's `Arc`: an interrupt save and restore, the
//! ticket's locked add, and the `Arc`'s locked increment and decrement, about
//! eight times a side of a round trip. Each processor's record now holds a
//! copy of `Arc::as_ptr` of its queue's `current` ([`RunningSlot`]), and
//! [`with_current`] lends the task it names to a closure, by reference.
//!
//! # The invariant
//!
//! Whenever interrupts are on, the record's pointer is the task the run
//! queue's `current` holds an `Arc` to. Both are written only by this
//! processor, with interrupts masked and its queue lock held, together, by
//! [`super::set_current`]: at the boot task's adoption, at an idle task's
//! installation, and at every switch in `choose_next`. Between that write and
//! the stack switch interrupts stay masked, so nothing reads the record while
//! it already names the incoming task and the outgoing one still runs.
//!
//! # Why a borrow cannot outlive its task
//!
//! A borrow is used only by code running as the task: the closure runs in
//! the task's own context, and the reference cannot leave it, because the
//! closure takes `&'a Task` for every `'a` and its result cannot name one. So
//! the question is whether a task can be freed while its own code runs.
//!
//! 1. While a task runs, its processor's run queue holds an `Arc` to it as
//!    `current`.
//! 2. A task stops running only at a switch, and when it is switched to again
//!    it is `current` once more. A borrow held across a block inside the
//!    closure is not used while the task is not running, and is valid again
//!    when it is (the consultant's Q1).
//! 3. A task is freed only when its last `Arc` goes. For a dead task that is
//!    the reaper's, and the reaper takes a task only from `ZOMBIES`, where
//!    `finish_switch` files it after the switch away from it for the last
//!    time. After that switch none of its code runs.
//! 4. A task that migrates holds a reference to the task, not to a
//!    processor's slot, so the move does not change what it names.
//!
//! An interrupt handler that borrows runs as the interrupted task, which is
//! `current` by (1).
//!
//! # How it is read
//!
//! Through `arch::this_cpu_read`: on x86-64 one `mov` from `GS:offset`, on
//! Arm the record's address and the field read with interrupts masked, so that
//! a task moved between finding the record and reading it never reads another
//! processor's task.
//!
//! # The audit
//!
//! From boot to the end of stage 5 the invariant is also checked where it is
//! relied on, at points apart from the writes: once the boot task is adopted,
//! as each secondary's idle task first runs, in the incoming context of every
//! switch, and at every interrupt exit. Each compares the borrow's pointer
//! with the queue's `current`, read under the lock, counts the audit by site,
//! and stops the machine on a mismatch, naming the site. Stage 5's check
//! requires every site audited and ends the audit ([`end_audit`]).

use alloc::sync::Arc;
#[cfg(not(target_pointer_width = "64"))]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::Task;
use crate::arch;

/// One processor's copy of `Arc::as_ptr` of its queue's `current`, or zero.
/// A field of [`crate::smp::PerCpu`]; only this module and
/// [`super::set_current`] touch it.
#[derive(Debug)]
pub(crate) struct RunningSlot {
    /// The pointer, as a number.
    #[cfg(target_pointer_width = "64")]
    word: AtomicU64,
    /// The same as two halves, low first, the shape `arch::this_cpu_read`
    /// reads on a 32-bit machine; the high half is always zero.
    #[cfg(not(target_pointer_width = "64"))]
    word: [AtomicU32; 2],
}

impl RunningSlot {
    /// Where [`Self::word`] is in the slot, for `smp::RUNNING_WORD_OFFSET`.
    pub(crate) const WORD_OFFSET: usize = core::mem::offset_of!(Self, word);

    /// Naming nothing.
    pub(crate) const fn new() -> Self {
        Self {
            #[cfg(target_pointer_width = "64")]
            word: AtomicU64::new(0),
            #[cfg(not(target_pointer_width = "64"))]
            word: [AtomicU32::new(0), AtomicU32::new(0)],
        }
    }

    /// Name `task`. Only by this processor, with interrupts masked and its
    /// queue lock held, beside the queue's own `current`. `Relaxed`: only
    /// this processor reads it, and in program order after this.
    pub(super) fn set(&self, task: &Arc<Task>) {
        let pointer = Arc::as_ptr(task) as usize;
        #[cfg(target_pointer_width = "64")]
        {
            self.word.store(pointer as u64, Ordering::Relaxed);
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            let [low, _] = &self.word;
            low.store(pointer as u32, Ordering::Relaxed);
        }
    }
}

/// The running task's pointer, or null before the records are kept or
/// where none runs yet.
fn current_ptr() -> *const Task {
    if !super::preempt::counting() {
        return core::ptr::null();
    }
    // SAFETY: (SHARED) the records are kept only once every processor has
    // installed its own (`preempt::start_counting`), and the offset is the
    // slot's, which only its own processor writes.
    let word = unsafe { arch::this_cpu_read(crate::smp::RUNNING_WORD_OFFSET) };
    word as usize as *const Task
}

/// Whether a task runs here: the borrow's question without the borrow.
pub(super) fn running() -> bool {
    !current_ptr().is_null()
}

/// Lend the running task to `lend`, or answer `None` before one runs.
///
/// The reference is good for the closure's whole run, a block inside it
/// included: see the module.
#[inline(always)]
pub(crate) fn with_current<R>(lend: impl FnOnce(&Task) -> R) -> Option<R> {
    let pointer = current_ptr();
    // SAFETY: (SHARED) a non-null pointer is `Arc::as_ptr` of the task this
    // processor's queue holds as `current`, which is the task running this
    // code, and a task is not freed while its own code runs (the module's
    // argument). The reference does not outlive the closure.
    let task = unsafe { pointer.as_ref() }?;
    Some(lend(task))
}

/// The running task as an `Arc` of its own, made without the run queue's
/// lock: the queue's `Arc` is held for as long as this code runs, so a
/// strong count raised from the borrow is a count on a live allocation.
pub(super) fn current_arc() -> Option<Arc<Task>> {
    let pointer = current_ptr();
    if pointer.is_null() {
        return None;
    }
    // SAFETY: (SHARED) `pointer` came from `Arc::as_ptr` of the queue's
    // `current`, which holds a strong count for as long as this task runs.
    unsafe { Arc::increment_strong_count(pointer) };
    // SAFETY: (SHARED) the count raised above is the one this `Arc` owns.
    Some(unsafe { Arc::from_raw(pointer) })
}

/// Where an audit looks.
#[derive(Clone, Copy)]
pub(super) enum Site {
    /// Just after the boot task is adopted.
    Boot = 0,
    /// As a secondary's idle task first runs.
    Idle = 1,
    /// In the incoming context of a switch.
    Switch = 2,
    /// At an interrupt's exit.
    Interrupt = 3,
    /// From a task of stage 5's check, at its resumption.
    Task = 4,
}

/// How many places [`Site`] names.
const SITES: usize = 5;

/// Whether the audit runs: from boot to the end of stage 5.
static AUDITING: AtomicBool = AtomicBool::new(true);

/// Audits made, by site.
static AUDITS: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];

/// Mismatches seen, by site.
static MISMATCHES: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];

/// Whether the audit runs.
pub(super) fn auditing() -> bool {
    AUDITING.load(Ordering::Relaxed)
}

/// Stop auditing. Once, at the end of stage 5's check.
pub(super) fn end_audit() {
    AUDITING.store(false, Ordering::Release);
}

/// Compare the borrow's pointer with `queued`, the queue's `current` read
/// under its lock by the caller with interrupts masked, and count the audit
/// under `site`. A mismatch stops the machine at once (FX-0502), naming the
/// site: the invariant is what every reader of the running task now trusts,
/// so the next thing to go wrong after one is a hang somewhere else, which
/// is what the switch's negative control showed before it was made fatal.
pub(super) fn audit(site: Site, queued: Option<&Arc<Task>>) {
    let index = site as usize;
    let borrowed = current_ptr();
    let expected = queued.map_or(core::ptr::null(), Arc::as_ptr);
    if let Some(count) = AUDITS.get(index) {
        let _ = count.fetch_add(1, Ordering::Relaxed);
    }
    if borrowed != expected {
        if let Some(count) = MISMATCHES.get(index) {
            let _ = count.fetch_add(1, Ordering::Relaxed);
        }
        crate::panic::fatal!(
            crate::panic::catalog::STAGE5_SCHEDULER,
            "the processor record did not name its run queue's current task {}",
            site.name()
        );
    }
}

impl Site {
    /// Where, for the message.
    const fn name(self) -> &'static str {
        match self {
            Self::Boot => "once the boot task was adopted",
            Self::Idle => "as an idle task first ran",
            Self::Switch => "after a switch, in the incoming context",
            Self::Interrupt => "at an interrupt's exit",
            Self::Task => "as a task of the borrow check resumed",
        }
    }
}

/// Audits made and mismatches seen at `site`.
pub(super) fn audited(site: Site) -> (u64, u64) {
    let index = site as usize;
    (
        AUDITS
            .get(index)
            .map_or(0, |count| count.load(Ordering::Relaxed)),
        MISMATCHES
            .get(index)
            .map_or(0, |count| count.load(Ordering::Relaxed)),
    )
}
