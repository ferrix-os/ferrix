//! The kernel's spin locks.
//!
//! `ferrix_sync` provides the primitives; this module says which the kernel
//! takes where, so that the choice is made once.
//!
//! * [`SpinLock`] — for data tasks share and no interrupt handler touches:
//!   a process's tables, a channel's queue, a filesystem's state. It keeps the
//!   holding task on its processor until the guard drops, and that is not
//!   optional. A ticket lock whose holder can be switched out stalls every
//!   waiter for a round of the run queue, and waiters switched out holding
//!   tickets pass the stall on; the thousand-task check spent fifty seconds
//!   in one such convoy. **Never block while holding one** — `schedule`
//!   stops the machine if asked to switch with the count raised.
//! * [`IrqSpinLock`] — for data an interrupt handler also touches. It masks
//!   interrupts, which also keeps the holder on its processor.
//! * The run queues' own locks are plain `ferrix_sync::SpinLock`s taken with
//!   interrupts masked and handed across a context switch; `sched::queue`
//!   says how.
//! * `ferrix_sync::SleepLock` — for a critical section that may block: the
//!   namespace's rename lock, held across path walks that read a disk. It is
//!   the one lock a holder may sleep under, and it must never be taken with a
//!   [`SpinLock`] held or preemption otherwise off: a task that blocks with
//!   the count raised is FX-0503. Its waiters sleep on a `sched::WaitQueue`,
//!   which [`SchedParker`] lends to every such lock a crate below the kernel
//!   makes.

/// A spin lock whose holder is not switched out while it holds it.
pub(crate) type SpinLock<T> = ferrix_sync::PreemptSpinLock<T, crate::sched::Preempt>;

/// What the kernel lends a `ferrix_sync::SleepLock` to wait on: a wait
/// queue of its own per lock, so that a release wakes that lock's waiters and
/// nobody else's.
///
/// Before the scheduler runs, a wait on the queue spins, so a lock made at
/// boot works then too; it just cannot be contended yet.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SchedParker;

impl ferrix_sync::Parker for SchedParker {
    fn new_parking(&self) -> Option<alloc::boxed::Box<dyn ferrix_sync::Parking>> {
        let queue = crate::fallible::try_box(crate::sched::WaitQueue::new()).ok()?;
        Some(queue)
    }
}

/// A [`SpinLock`] held under the interrupt mask rather than with the full
/// preemption count ([`try_lock_masked`]).
pub(crate) struct MaskedGuard<'a, T> {
    /// The ticket lock's own guard, which releases it.
    guard: ferrix_sync::SpinLockGuard<'a, T>,
}

/// [`SpinLock::try_lock`] for a holder that keeps interrupts masked for the
/// whole hold: the native round trip's fast path (`docs/OPAQUE-KERNEL.md`
/// §9.11). The same ticket lock, so it excludes every holder as `try_lock`
/// does. The mask keeps the holder on its processor; the hold is still
/// counted as one lock on this processor's preemption word
/// (`sched::raise_masked`), so that a switch made with it held stops the
/// machine at A3 (FX-0503) as it does for any guard. What it leaves out is
/// the site record and the deferred decision on release, which a masked
/// holder could not make.
///
/// # Safety
///
/// (CONTEXT) Interrupts are masked on this processor from before the call
/// until the guard drops, and the holder does not block while holding it.
pub(crate) unsafe fn try_lock_masked<T>(lock: &SpinLock<T>) -> Option<MaskedGuard<'_, T>> {
    // SAFETY: (CONTEXT) the caller's contract is the lock's.
    let guard = unsafe { lock.try_lock_masked() }?;
    crate::sched::raise_masked();
    Some(MaskedGuard { guard })
}

impl<T> core::ops::Deref for MaskedGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> core::ops::DerefMut for MaskedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for MaskedGuard<'_, T> {
    /// Lowers the count; the field's own drop then lets the lock go. Both
    /// under the mask, so the order is not observable on this processor.
    fn drop(&mut self) {
        crate::sched::lower_masked();
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for MaskedGuard<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&*self.guard, f)
    }
}
