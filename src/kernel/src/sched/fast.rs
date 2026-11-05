//! What K1's fast path asks of the scheduler, with nothing out of line and one branch
//! between it and the general path: after the exit's barrier every taken branch runs
//! cold (`docs/microkernel/PERFORMANCE.md`).
#![allow(dead_code, reason = "x86-64's fast crossings alone ask these")]

use core::sync::atomic::Ordering;

use super::{MOVES, NEED_RESCHED, RUNNING_SEEN, Task};
use crate::arch;
use crate::object::quota;

/// The running task, on a path only a running thread reaches.
///
/// # Safety
///
/// (SHARED) A thread is running here, with interrupts masked.
#[inline(always)]
pub(crate) unsafe fn task<'a>() -> &'a Task {
    // SAFETY: (SHARED) as `borrow::current_ptr`.
    let word = unsafe { arch::this_cpu_read(crate::smp::RUNNING_WORD_OFFSET) };
    // SAFETY: (SHARED) the caller's guarantee: the running task.
    unsafe { &*(word as usize as *const Task) }
}

/// The running task and this processor's record, as [`task`].
///
/// # Safety
///
/// As [`task`].
#[inline(always)]
pub(crate) unsafe fn here<'a>() -> (&'a Task, &'a crate::smp::PerCpu) {
    // SAFETY: the caller's guarantee.
    let task = unsafe { task() };
    // SAFETY: (SHARED) every processor's register holds its own record, for ever.
    let at = unsafe { arch::cpu_local() };
    // SAFETY: (SHARED) as above.
    (task, unsafe { &*(at as *const crate::smp::PerCpu) })
}

/// Its restricted-mode record, which a task in restricted mode or back from it holds.
///
/// # Safety
///
/// (CONTEXT) The running task's own, and it has one.
#[inline(always)]
pub(crate) unsafe fn state<'a>(task: &Task) -> &'a mut arch::RestrictedState {
    // SAFETY: (CONTEXT) the caller's guarantee.
    let slot = unsafe { task.restricted_slot() };
    // SAFETY: (CONTEXT) as above.
    let slot = unsafe { &mut *slot };
    // SAFETY: (CONTEXT) it has one.
    unsafe { slot.as_deref_mut().unwrap_unchecked() }
}

/// Nothing due on processor `cpu` for `task`: [`super::nothing_due_here`], and
/// [`super::work::peek`] is zero, without a branch.
#[inline(always)]
pub(crate) fn quiet(task: &Task, cpu: usize) -> bool {
    // SAFETY: (SHARED) both tables exist before any thread runs, one slot a processor.
    let resched = unsafe { NEED_RESCHED.get().unwrap_unchecked() };
    // SAFETY: as above.
    let seen = unsafe { RUNNING_SEEN.get().unwrap_unchecked() };
    // SAFETY: as above.
    let resched = unsafe { resched.get_unchecked(cpu) };
    // SAFETY: as above.
    let seen = unsafe { seen.get_unchecked(cpu) };
    (!resched.load(Ordering::Relaxed))
        & (seen.load(Ordering::Relaxed) == MOVES.load(Ordering::Acquire))
        & (task.work().load(Ordering::Acquire) == 0)
        & (!quota::bandwidth_in_use())
}

/// Which record the task's state is read from, as `set_active_user_state`.
#[inline(always)]
pub(crate) fn set_active(task: &Task, record: *mut arch::UserState) {
    task.set_active_user(record);
}

/// Its own record, as `with_own_task_record`, which a program's thread has.
///
/// # Safety
///
/// (SHARED) With interrupts masked; a thread's task.
#[inline(always)]
pub(crate) unsafe fn own_record<'a>(task: &Task) -> &'a arch::UserState {
    // SAFETY: (SHARED) the caller's guarantee.
    let record = unsafe { task.own_record() };
    // SAFETY: (SHARED) a thread's task has one.
    let record = unsafe { record.unwrap_unchecked() };
    // SAFETY: (SHARED) as above.
    unsafe { &*record }
}

/// Its mode is now `restricted`, as `set_running_restricted`.
#[inline(always)]
pub(crate) fn set_mode(task: &Task, cpu: &crate::smp::PerCpu, restricted: bool) {
    task.set_restricted(restricted);
    cpu.set_task_kind(restricted, task.is_bare());
}

/// Its restricted-mode record, if it has one.
#[inline(always)]
pub(crate) fn state_of<'a>(task: &Task) -> Option<&'a mut arch::RestrictedState> {
    // SAFETY: (CONTEXT) only the running task reaches its own record, for this call.
    let slot = unsafe { task.restricted_slot() };
    // SAFETY: (CONTEXT) as above.
    let slot = unsafe { &mut *slot };
    slot.as_deref_mut()
}
