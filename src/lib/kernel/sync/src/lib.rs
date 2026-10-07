//! Kernel synchronisation primitives.
//!
//! # Why a ticket lock
//!
//! The obvious spin lock is a single flag swapped with a compare-exchange in a
//! loop. It is three lines and it is unfair: whichever CPU happens to win the
//! next cache-line transfer takes the lock, so a core can lose every race for
//! an unbounded time while another core reacquires the same lock in a tight
//! loop. On a kernel path that is a CPU that never makes progress, and it
//! shows up as a hang nobody can attribute to a line of code.
//!
//! [`SpinLock`] therefore hands out numbered tickets. A CPU takes the next
//! ticket, waits for the serving counter to reach it, and on release bumps the
//! counter by one. Arrival order is acquisition order, so the worst-case wait
//! is bounded by the number of CPUs ahead in the queue rather than by luck.
//!
//! # The rules these primitives do not enforce
//!
//! Nothing here can detect a deadlock at runtime — a spinning CPU looks exactly
//! like a busy one — so the discipline has to be kept by the callers:
//!
//! * **Never take a plain [`SpinLock`] from an interrupt handler.** If the
//!   handler interrupts the very CPU that holds the lock, the handler spins
//!   waiting for a release that can only happen after the handler returns.
//!   Data shared with a handler belongs in an [`IrqSpinLock`], which masks
//!   interrupts for the duration and so cannot be interrupted while held.
//! * **Never take a lock twice on one CPU.** None of these are reentrant; a
//!   second acquisition waits for a release that the same CPU owes.
//! * **Take nested locks in one global order.** Two CPUs taking the same pair
//!   in opposite orders is the classic cycle, and no primitive can see it.
//! * **Keep critical sections short and never sleep inside one.** A CPU that
//!   holds a spin lock while blocked stops every other CPU that wants it.
//! * **A holder that can be switched out convoys everyone.** A ticket lock
//!   hands itself to whoever is next in line whether or not that context is
//!   running. A holder preempted for the few instructions it holds the lock
//!   stalls every waiter until it runs again, and waiters preempted while
//!   holding *tickets* pass the stall on, each hand-off costing a full round
//!   of the run queue. That is why a kernel with preemption takes plain
//!   [`SpinLock`] only where the holder cannot be switched out, and
//!   [`PreemptSpinLock`] everywhere a task with interrupts on contends.
//!
//! # What is here
//!
//! * [`SpinLock`] — mutual exclusion, first come first served.
//! * [`PreemptSpinLock`] — the same, with the holder kept on its CPU;
//!   [`HookedPreempt`] keeps it there for a library that cannot name the
//!   kernel, through the [`PreemptHooks`] the kernel installs.
//! * [`IrqSpinLock`] — the same, with interrupts masked for the duration.
//! * [`Once`] — run an initialiser exactly once, for globals set up at boot.
//! * [`RwSpinLock`] — many readers or one writer, writer-preferring.
//! * [`SpinLockedCell`] — a global that is filled in at boot and read after.
//! * [`SleepLock`] — mutual exclusion whose waiters sleep, on a [`Parking`] the
//!   kernel lends through a [`Parker`]; the one lock a holder may block under.
//! * [`nospec::bounded`] — an index check a mispredicted branch cannot see
//!   past, for the tables in these libraries a program indexes.
//!
//! ```
//! use ferrix_sync::SpinLock;
//!
//! static COUNTER: SpinLock<u64> = SpinLock::new(0);
//!
//! *COUNTER.lock() += 1;
//! assert_eq!(*COUNTER.lock(), 1);
//! ```

#![no_std]

extern crate alloc;

pub mod nospec;

#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::fmt;
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// SpinLock
// ---------------------------------------------------------------------------

/// A mutual-exclusion lock that spins, handing the lock out in arrival order.
///
/// Acquisition is a ticket queue: [`lock`](Self::lock) takes the next ticket
/// and waits until the lock is serving it. That costs one extra counter over a
/// test-and-set lock and buys a bounded wait — at most one turn per CPU already
/// in the queue — which is what keeps a contended lock from starving a core.
///
/// This lock must not be taken from an interrupt handler; see the module
/// documentation for why, and for [`IrqSpinLock`], which can be.
///
/// ```
/// # use ferrix_sync::SpinLock;
/// let lock = SpinLock::new([0_u8; 4]);
/// lock.lock()[0] = 1;
/// assert!(lock.try_lock().is_some(), "the guard above was temporary");
/// ```
pub struct SpinLock<T: ?Sized> {
    /// The ticket the next caller will take.
    next_ticket: AtomicUsize,
    /// The ticket currently entitled to the data.
    ///
    /// The lock is free exactly when this equals `next_ticket`.
    now_serving: AtomicUsize,
    /// The protected data, reachable only through a guard.
    data: UnsafeCell<T>,
}

// SAFETY: sending the lock sends the data it owns, which is what `T: Send`
// permits. The two counters are atomics and belong to no particular thread.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

// SAFETY: sharing the lock lets another thread reach the data, but only after
// winning the ticket queue, so at most one thread holds a reference at a time
// and that reference moves between threads. That is exactly `T: Send`; `T` need
// not be `Sync`, because two threads never hold the data simultaneously.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates a lock in the unlocked state.
    ///
    /// This is `const`, so a lock can be a `static` without a boot-time
    /// initialiser — which is the point, since the globals it replaces are
    /// reached before any allocator exists.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            next_ticket: AtomicUsize::new(0),
            now_serving: AtomicUsize::new(0),
            data: UnsafeCell::new(value),
        }
    }

    /// Consumes the lock and returns the protected value.
    ///
    /// Taking the lock by value proves no guard is outstanding, so this needs
    /// no synchronisation at all.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Takes the lock, spinning until this caller's ticket comes up.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        self.acquire();
        SpinLockGuard {
            lock: self,
            not_send: PhantomData,
        }
    }

    /// Waits for this caller's turn, leaving the release to the caller.
    ///
    /// Split out from [`lock`](Self::lock) because [`IrqSpinLock`] needs the
    /// acquisition without a guard whose drop would release at the wrong point
    /// in its own drop order.
    fn acquire(&self) {
        // Relaxed: taking a ticket publishes nothing and reads nothing that the
        // acquire load below does not already order. All that is required of
        // the increment is that no two callers get the same number.
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        // Acquire: this is the load that decides the lock is ours, so it has to
        // pair with the releasing store of the previous holder and make that
        // holder's writes to the data visible before any of ours.
        while self.now_serving.load(Ordering::Acquire) != ticket {
            spin_loop();
        }
    }

    /// Takes the lock if it is free right now, and gives up otherwise.
    ///
    /// A failure means the lock was contended at some instant during the call,
    /// which is the only honest thing a non-blocking attempt can report.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.try_acquire().then(|| SpinLockGuard {
            lock: self,
            not_send: PhantomData,
        })
    }

    /// One attempt at the lock, leaving the release to the caller.
    ///
    /// The guardless counterpart of [`try_lock`](Self::try_lock), for the same
    /// reason [`acquire`](Self::acquire) exists.
    fn try_acquire(&self) -> bool {
        // Relaxed: this load only proposes a ticket number. Claiming it is the
        // compare-exchange below, and that is the operation carrying ordering.
        let serving = self.now_serving.load(Ordering::Relaxed);
        // The exchange succeeds only if nobody holds or is queued for the lock,
        // because the ticket counter runs ahead of the serving counter exactly
        // when someone does. Acquire on success for the reason given in
        // `acquire`; Relaxed on failure, since a failed attempt reads no
        // protected data. A concurrent release between the load and the
        // exchange makes this report contention that has just ended, which is
        // a permitted answer for a non-blocking attempt.
        self.next_ticket
            .compare_exchange(
                serving,
                serving.wrapping_add(1),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Reports whether the lock was held at some instant during the call.
    ///
    /// It is a statistic, not a decision: by the time a caller acts on `false`
    /// another CPU may hold the lock. Use it for diagnostics and assertions,
    /// and [`try_lock`](Self::try_lock) to act.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        // Relaxed on both: the answer is already stale when it is returned, so
        // ordering it against anything would buy nothing.
        self.next_ticket.load(Ordering::Relaxed) != self.now_serving.load(Ordering::Relaxed)
    }

    /// Borrows the protected data directly.
    ///
    /// Safe, and free: `&mut self` is itself proof that no other reference to
    /// the lock exists, so no other CPU can be inside it.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    /// Takes the lock and hands out the data with no guard to release it.
    ///
    /// **For one caller: a context switch.** A scheduler takes its run
    /// queue's lock, chooses what runs next, switches stacks, and the
    /// *incoming* context releases the lock — which is what keeps the
    /// outgoing task from being picked up by another CPU before its registers
    /// have been saved. A guard cannot express that, because the release
    /// would happen on the way out of a scope the switch never comes back to.
    /// Linux hands its `rq` lock across `context_switch` for the same reason
    /// and in the same way.
    ///
    /// Everything else should use [`lock`](Self::lock).
    ///
    /// `clippy::mut_from_ref` is the right lint to fire here, and this is the
    /// one place that answers it rather than obeys it: exclusivity does not
    /// come from the `&self`, it comes from the ticket queue having served
    /// this caller and nobody else. That is the same guarantee [`lock`](Self::lock) rests
    /// on; it simply cannot be spelled in the signature, because the guard
    /// that would carry it is exactly what a context switch cannot use.
    ///
    /// # Safety
    ///
    /// The lock must be released exactly once, with
    /// [`force_unlock`](Self::force_unlock), by whichever context ends up
    /// owning it; and the returned reference must not be used after that, nor
    /// alongside any other reference into the same lock.
    #[must_use = "the lock stays held until force_unlock"]
    #[allow(clippy::mut_from_ref, reason = "the ticket queue makes it exclusive")]
    pub unsafe fn lock_manually(&self) -> &mut T {
        self.acquire();
        // SAFETY: the ticket queue serves one caller at a time and this one
        // has just been served, so no other reference to the data exists; the
        // caller's contract carries that forward to whoever releases it.
        unsafe { &mut *self.data.get() }
    }

    /// [`lock_manually`](Self::lock_manually) if the lock is free right now,
    /// and `None`, having changed nothing, otherwise.
    ///
    /// For the same one caller, a context switch, where it cannot wait: the
    /// direct switch of a channel round trip takes a second processor's run
    /// queue only if nobody holds it (OPAQUE-KERNEL.md §9.7), and goes the
    /// general way when somebody does. A failed attempt takes no ticket, so it
    /// leaves nothing behind for the holder or the next in line.
    ///
    /// `clippy::mut_from_ref` is exempted for the reason given on
    /// [`lock_manually`](Self::lock_manually).
    ///
    /// # Safety
    ///
    /// As [`lock_manually`](Self::lock_manually), when it returns `Some`.
    #[must_use = "the lock stays held until force_unlock"]
    #[allow(clippy::mut_from_ref, reason = "the ticket queue makes it exclusive")]
    pub unsafe fn try_lock_manually(&self) -> Option<&mut T> {
        if !self.try_acquire() {
            return None;
        }
        // SAFETY: the attempt above was served, so as in `lock_manually` no
        // other reference to the data exists; the caller's contract carries
        // that forward to whoever releases it.
        Some(unsafe { &mut *self.data.get() })
    }

    /// Reaches the data of a lock this CPU already holds.
    ///
    /// The other half of [`lock_manually`](Self::lock_manually): the context
    /// that a switch handed the lock to did not take it, so it has no
    /// reference and needs one to finish what the previous context started.
    ///
    /// `clippy::mut_from_ref` is exempted for the reason given on
    /// [`lock_manually`](Self::lock_manually): holding the lock, not holding
    /// the borrow, is what makes the reference exclusive.
    ///
    /// # Safety
    ///
    /// The caller's CPU must hold this lock, and the reference must not
    /// outlive that: no other reference into the lock may exist, and it must
    /// not be used after [`force_unlock`](Self::force_unlock).
    #[must_use = "the lock is not released by reading the data"]
    #[allow(clippy::mut_from_ref, reason = "holding the lock makes it exclusive")]
    pub unsafe fn locked_data(&self) -> &mut T {
        // SAFETY: the caller guarantees this CPU holds the lock, which is
        // what makes the reference exclusive.
        unsafe { &mut *self.data.get() }
    }

    /// Releases a lock taken with [`lock_manually`](Self::lock_manually).
    ///
    /// # Safety
    ///
    /// The caller must own the lock — having taken it manually, or having
    /// been handed it by a context switch — and must not touch the protected
    /// data afterwards.
    pub unsafe fn force_unlock(&self) {
        // SAFETY: the caller guarantees ownership, which is `release`'s
        // contract.
        unsafe { self.release() };
    }

    /// Releases the lock without consuming a guard.
    ///
    /// # Safety
    ///
    /// The caller must be the current holder of this lock and must not touch
    /// the protected data afterwards. Releasing a lock this CPU does not hold
    /// hands the data to a CPU that is still inside its critical section.
    unsafe fn release(&self) {
        // Relaxed: only the holder writes this counter, and we are it, so no
        // other CPU can have changed it since we were served.
        let serving = self.now_serving.load(Ordering::Relaxed);
        // Release: everything written under the lock has to be visible to the
        // next holder, whose acquire load of this counter pairs with this
        // store. `wrapping_add` because the ticket counter wraps too, and the
        // lock only ever compares the two for equality.
        self.now_serving
            .store(serving.wrapping_add(1), Ordering::Release);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLock<T> {
    /// Formats the lock without ever waiting for it.
    ///
    /// A `Debug` that took the lock would deadlock the moment anything
    /// formatted a structure whose lock the same CPU already held — including a
    /// panic handler dumping the state it locked in order to read.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => f.debug_struct("SpinLock").field("data", &&*guard).finish(),
            None => f.write_str("SpinLock { data: <locked> }"),
        }
    }
}

impl<T: Default> Default for SpinLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Proof that its holder is inside a [`SpinLock`]'s critical section.
///
/// The lock is released when this is dropped, so the guard's lifetime *is* the
/// critical section: keep it short, and never hold it across a sleep.
pub struct SpinLockGuard<'a, T: ?Sized> {
    /// The lock to release on drop, and the data to hand out until then.
    lock: &'a SpinLock<T>,
    /// Makes the guard `!Send`.
    ///
    /// Releasing on a CPU other than the one that acquired would be legal for
    /// this lock in isolation, but it breaks every caller that reasons about
    /// per-CPU state across a critical section, and it is what
    /// [`IrqSpinLockGuard`] has to forbid outright.
    not_send: PhantomData<*const ()>,
}

// SAFETY: a shared reference to a guard reaches the data only through `Deref`,
// so sharing one across threads shares `&T`, which is what `T: Sync` allows.
unsafe impl<T: ?Sized + Sync> Sync for SpinLockGuard<'_, T> {}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard exists only while its thread holds the lock, and
        // the lock serves one ticket at a time, so no other reference to the
        // data can exist for the life of this borrow.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, with `&mut self` additionally proving this is
        // the only borrow taken through the guard itself.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: an acquisition handed this guard to this thread, it is being
        // consumed here so no further access can happen through it, and a guard
        // is dropped exactly once.
        unsafe { self.lock.release() };
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLockGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for SpinLockGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

// ---------------------------------------------------------------------------
// PreemptSpinLock
// ---------------------------------------------------------------------------

/// How to keep the running task on its CPU for a while.
///
/// This crate cannot see the scheduler, so [`PreemptSpinLock`] is
/// parameterised over this trait and the kernel supplies the two halves: a
/// per-CPU count that `disable` raises and `enable` lowers, which the
/// scheduler's interrupt-exit hook reads before switching a task out.
///
/// Calls nest: each `disable` is matched by one `enable`, and the task stays
/// where it is until the last one.
///
/// # Safety
///
/// `disable` must actually keep the calling context from being switched out
/// until the matching `enable`, and both must be safe to call from any
/// context a lock may be taken in — before the scheduler exists, with
/// interrupts masked, inside a handler. An implementation that only pretends
/// leaves every [`PreemptSpinLock`] open to the convoy the type exists to
/// prevent.
pub unsafe trait PreemptControl {
    /// Keep the running task on this CPU until the matching `enable`.
    ///
    /// `#[track_caller]`, and so are the lock's `lock` and `try_lock`, so an
    /// implementation that records `Location::caller()` records the line
    /// that took the lock -- which is what a report about a holder that
    /// went to sleep needs to name.
    #[track_caller]
    fn disable();
    /// Let it be switched out again, once every `disable` is matched.
    fn enable();
}

/// A [`SpinLock`] whose holder cannot be switched out while it holds it.
///
/// The lock for data that tasks share and no interrupt handler touches: a
/// process's tables, a channel's queue, a filesystem's state. It masks nothing
/// — an interrupt still arrives and is still handled — it only keeps the
/// holding task on its CPU until the guard drops, so the lock is held for
/// exactly the instructions it covers and nobody waits on a task that is not
/// running.
///
/// **Never block while holding one.** Sleeping with the count raised would
/// keep whatever ran next from being preempted, and the count would be
/// lowered on whatever CPU the sleeper woke on; the kernel's scheduler
/// treats a switch with the count raised as a bug and stops.
pub struct PreemptSpinLock<T: ?Sized, P: PreemptControl> {
    /// Names the preemption-control implementation without storing anything.
    control: PhantomData<fn() -> P>,
    /// The ticket lock underneath.
    inner: SpinLock<T>,
}

impl<T, P: PreemptControl> PreemptSpinLock<T, P> {
    /// Creates a lock in the unlocked state.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            control: PhantomData,
            inner: SpinLock::new(value),
        }
    }

    /// Consumes the lock and returns the protected value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T: ?Sized, P: PreemptControl> PreemptSpinLock<T, P> {
    /// Keeps this task on its CPU and then takes the lock, in that order.
    ///
    /// The order matters for the waiters too: a task spinning for its ticket
    /// with preemption on could be switched out holding the ticket, which is
    /// the second half of the convoy.
    #[must_use = "the task stays on its CPU until the guard is dropped"]
    #[track_caller]
    pub fn lock(&self) -> PreemptSpinLockGuard<'_, T, P> {
        P::disable();
        self.inner.acquire();
        PreemptSpinLockGuard {
            lock: self,
            not_send: PhantomData,
        }
    }

    /// Takes the lock if it is free right now, and gives up otherwise,
    /// leaving preemption as it found it.
    #[must_use = "the task stays on its CPU until the guard is dropped"]
    #[track_caller]
    pub fn try_lock(&self) -> Option<PreemptSpinLockGuard<'_, T, P>> {
        P::disable();
        if self.inner.try_acquire() {
            Some(PreemptSpinLockGuard {
                lock: self,
                not_send: PhantomData,
            })
        } else {
            P::enable();
            None
        }
    }

    /// [`try_lock`](Self::try_lock) for a caller that masks interrupts for
    /// the whole hold, which keeps it on its CPU without the count: the same
    /// ticket lock, so it excludes every other holder exactly as `try_lock`
    /// does, with no `disable` and no `enable`. For the native channel round
    /// trip's fast path (OPAQUE-KERNEL.md §9.7), which takes its locks only
    /// by `try_lock`, with interrupts masked from the system call's entry.
    ///
    /// # Safety
    ///
    /// Interrupts are masked on this CPU from before the call until the guard
    /// drops, and the caller neither blocks nor switches while it holds the
    /// guard: what the count would otherwise have promised the lock. A masked
    /// hold records no site for a report of a holder that switched; a caller
    /// that wants the switch checked counts the hold itself, as the kernel's
    /// `sync::try_lock_masked` does.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub unsafe fn try_lock_masked(&self) -> Option<SpinLockGuard<'_, T>> {
        self.inner.try_lock()
    }

    /// Reports whether the lock was held at some instant during the call.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    /// Borrows the protected data directly, with no lock.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}

impl<T: ?Sized + fmt::Debug, P: PreemptControl> fmt::Debug for PreemptSpinLock<T, P> {
    /// Formats the lock without waiting for it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner.try_lock() {
            Some(guard) => f
                .debug_struct("PreemptSpinLock")
                .field("data", &&*guard)
                .finish(),
            None => f.write_str("PreemptSpinLock { data: <locked> }"),
        }
    }
}

impl<T: Default, P: PreemptControl> Default for PreemptSpinLock<T, P> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Proof that its holder is inside a [`PreemptSpinLock`]'s critical section,
/// on a CPU it will not be switched out of.
///
/// Dropping it releases the lock and *then* lets the task be preempted, so
/// that a switch cannot land between the two with the lock still held.
pub struct PreemptSpinLockGuard<'a, T: ?Sized, P: PreemptControl> {
    /// The lock to release on drop, and the data to hand out until then.
    lock: &'a PreemptSpinLock<T, P>,
    /// Makes the guard `!Send`: the count is per-CPU and is lowered where it
    /// was raised.
    not_send: PhantomData<*const ()>,
}

// SAFETY: a shared reference to a guard reaches the data only through `Deref`,
// so sharing one across threads shares `&T`, which is what `T: Sync` allows.
unsafe impl<T: ?Sized + Sync, P: PreemptControl> Sync for PreemptSpinLockGuard<'_, T, P> {}

impl<T: ?Sized, P: PreemptControl> Deref for PreemptSpinLockGuard<'_, T, P> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard exists only while its task holds the inner lock,
        // which serves one ticket at a time, so no other reference to the data
        // can exist for the life of this borrow.
        unsafe { &*self.lock.inner.data.get() }
    }
}

impl<T: ?Sized, P: PreemptControl> DerefMut for PreemptSpinLockGuard<'_, T, P> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, with `&mut self` additionally proving this is
        // the only borrow taken through the guard itself.
        unsafe { &mut *self.lock.inner.data.get() }
    }
}

impl<T: ?Sized, P: PreemptControl> Drop for PreemptSpinLockGuard<'_, T, P> {
    fn drop(&mut self) {
        // SAFETY: an acquisition handed this guard to this task, it is being
        // consumed here so no further access can happen through it, and a guard
        // is dropped exactly once.
        unsafe { self.lock.inner.release() };
        // Only now, with the lock free, may the task be switched out.
        P::enable();
    }
}

impl<T: ?Sized + fmt::Debug, P: PreemptControl> fmt::Debug for PreemptSpinLockGuard<'_, T, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display, P: PreemptControl> fmt::Display for PreemptSpinLockGuard<'_, T, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

// ---------------------------------------------------------------------------
// HookedPreempt
// ---------------------------------------------------------------------------

/// The kernel's two halves of [`PreemptControl`], as plain functions, for the
/// libraries that cannot name the kernel's type.
///
/// `disable` is handed the line that took the lock, which
/// `#[track_caller]` cannot carry through a function pointer by itself.
#[derive(Debug)]
pub struct PreemptHooks {
    /// [`PreemptControl::disable`], told where it was called from.
    pub disable: fn(&'static core::panic::Location<'static>),
    /// [`PreemptControl::enable`].
    pub enable: fn(),
}

/// The hooks [`install_preempt_hooks`] installed; null until then.
static PREEMPT_HOOKS: AtomicPtr<PreemptHooks> = AtomicPtr::new(core::ptr::null_mut());

/// Make every [`HookedPreempt`] lock keep its holder on its CPU through
/// `hooks`.
///
/// Returns `false`, and changes nothing, if hooks were installed before.
///
/// # Safety
///
/// `hooks` must keep the promises of [`PreemptControl`]'s two methods. It
/// must be installed before any context that takes a [`HookedPreempt`] lock
/// can be switched out, and while no such guard is alive: a guard taken
/// before would be released through an `enable` no `disable` matched.
pub unsafe fn install_preempt_hooks(hooks: &'static PreemptHooks) -> bool {
    // Release: pairs with the Acquire load in `preempt_hooks`, so a lock that
    // sees the pointer sees the functions behind it.
    PREEMPT_HOOKS
        .compare_exchange(
            core::ptr::null_mut(),
            core::ptr::from_ref(hooks).cast_mut(),
            Ordering::Release,
            Ordering::Relaxed,
        )
        .is_ok()
}

/// The installed hooks, if any.
fn preempt_hooks() -> Option<&'static PreemptHooks> {
    // SAFETY: the pointer is null or came from the `&'static PreemptHooks`
    // `install_preempt_hooks` was given, and it is never changed after.
    unsafe { PREEMPT_HOOKS.load(Ordering::Acquire).as_ref() }
}

/// A [`PreemptControl`] for the libraries below the kernel: whatever the
/// kernel installed with [`install_preempt_hooks`], and nothing before that.
///
/// A library's own locks are `PreemptSpinLock<T, HookedPreempt>`, and the
/// kernel makes them keep their holders on their CPUs exactly as its own
/// locks do. On the host, where nothing is installed, they are plain ticket
/// locks.
#[derive(Debug, Clone, Copy, Default)]
pub struct HookedPreempt;

// SAFETY: once hooks are installed, both methods are the kernel's, which
// `install_preempt_hooks`'s caller promised keep `PreemptControl`'s promises.
// Before that, its caller promised nothing taking one of these locks can be
// switched out, so doing nothing keeps the task where it is.
unsafe impl PreemptControl for HookedPreempt {
    #[track_caller]
    fn disable() {
        if let Some(hooks) = preempt_hooks() {
            (hooks.disable)(core::panic::Location::caller());
        }
    }

    fn enable() {
        if let Some(hooks) = preempt_hooks() {
            (hooks.enable)();
        }
    }
}

// ---------------------------------------------------------------------------
// IrqSpinLock
// ---------------------------------------------------------------------------

/// How to mask and restore interrupts on this CPU.
///
/// This crate is architecture-neutral, so it cannot write `cli`/`sti` or
/// `msr daifset` itself. [`IrqSpinLock`] is parameterised over this trait
/// instead, and each architecture supplies the two instructions.
///
/// The state is an opaque `usize` — on x86-64 the saved `RFLAGS`, on AArch64
/// the saved `DAIF` — so that nesting works: restoring puts back whatever was
/// there rather than unconditionally enabling interrupts, which would enable
/// them in the middle of an outer critical section that had masked them.
///
/// # Safety
///
/// `disable` must actually prevent interrupt delivery on the current CPU, and
/// `restore` must put the state back exactly as `disable` found it. An
/// implementation that only pretends to mask leaves every [`IrqSpinLock`] open
/// to the self-deadlock the type exists to prevent, and one that enables
/// interrupts a caller had masked corrupts the caller's critical section.
pub unsafe trait IrqControl {
    /// Mask interrupts, returning the previous state.
    fn disable() -> usize;
    /// Restore the state a previous `disable` returned.
    fn restore(state: usize);
}

/// A [`SpinLock`] that masks interrupts on the holding CPU for the duration.
///
/// This is the lock for data an interrupt handler also touches. Taking a plain
/// [`SpinLock`] in a handler that interrupted the holder *on the same CPU* is a
/// guaranteed deadlock: the handler spins for a release that cannot happen
/// until the handler returns. Masking interrupts before taking the lock removes
/// the interruption, so the handler cannot run while the lock is held.
///
/// Masking is per-CPU, so a handler on *another* CPU can still contend for the
/// lock. That is fine — it waits, and the holder is not blocked on it — which
/// is why the critical section still has to be short.
pub struct IrqSpinLock<T: ?Sized, C: IrqControl> {
    /// Names the interrupt-control implementation without storing anything.
    ///
    /// `fn() -> C` rather than `C` so that the lock's `Send` and `Sync` do not
    /// depend on a zero-sized marker type's auto traits.
    control: PhantomData<fn() -> C>,
    /// The ticket lock underneath; masking adds no mutual exclusion of its own.
    inner: SpinLock<T>,
}

impl<T, C: IrqControl> IrqSpinLock<T, C> {
    /// Creates a lock in the unlocked state, with interrupts untouched.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            control: PhantomData,
            inner: SpinLock::new(value),
        }
    }

    /// Consumes the lock and returns the protected value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T: ?Sized, C: IrqControl> IrqSpinLock<T, C> {
    /// Masks interrupts and then takes the lock, in that order.
    ///
    /// Masking first is the whole point: between taking the lock and masking,
    /// an interrupt could arrive on this CPU and its handler could ask for the
    /// same lock, which is the deadlock this type prevents.
    #[must_use = "interrupts stay masked until the guard is dropped"]
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T, C> {
        let irq_state = C::disable();
        self.inner.acquire();
        IrqSpinLockGuard {
            lock: self,
            irq_state,
            not_send: PhantomData,
        }
    }

    /// Masks interrupts and takes the lock if it is free, restoring the
    /// interrupt state and giving up if it is not.
    ///
    /// A failed attempt leaves interrupts exactly as it found them, so a
    /// caller that loops on `try_lock` does not accumulate masking.
    #[must_use = "interrupts stay masked until the guard is dropped"]
    pub fn try_lock(&self) -> Option<IrqSpinLockGuard<'_, T, C>> {
        let irq_state = C::disable();
        if self.inner.try_acquire() {
            Some(IrqSpinLockGuard {
                lock: self,
                irq_state,
                not_send: PhantomData,
            })
        } else {
            C::restore(irq_state);
            None
        }
    }

    /// Reports whether the lock was held at some instant during the call.
    ///
    /// Interrupts are not touched: this reads two counters and decides nothing.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }

    /// Borrows the protected data directly, with no lock and no masking.
    ///
    /// `&mut self` proves no other reference to the lock exists, so no
    /// interrupt handler can be holding it either.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}

impl<T: ?Sized + fmt::Debug, C: IrqControl> fmt::Debug for IrqSpinLock<T, C> {
    /// Formats the lock without waiting for it, and without masking anything.
    ///
    /// Formatting must not change interrupt state: a `Debug` that masked and
    /// restored would be a side effect in the middle of someone else's
    /// critical section.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner.try_lock() {
            Some(guard) => f
                .debug_struct("IrqSpinLock")
                .field("data", &&*guard)
                .finish(),
            None => f.write_str("IrqSpinLock { data: <locked> }"),
        }
    }
}

impl<T: Default, C: IrqControl> Default for IrqSpinLock<T, C> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Proof that its holder is inside an [`IrqSpinLock`]'s critical section, with
/// interrupts masked on this CPU.
///
/// Dropping it releases the lock and *then* restores the interrupt state. The
/// order is not cosmetic: restoring first would open a window in which an
/// interrupt arrives on this CPU while the lock is still held, and a handler
/// that wants the same lock would spin on a release that can only happen after
/// it returns — the deadlock this type exists to prevent.
pub struct IrqSpinLockGuard<'a, T: ?Sized, C: IrqControl> {
    /// The lock to release on drop, and the data to hand out until then.
    lock: &'a IrqSpinLock<T, C>,
    /// Whatever [`IrqControl::disable`] found, to be handed back on drop.
    irq_state: usize,
    /// Makes the guard `!Send`.
    ///
    /// Interrupt masking is per-CPU, so the restore has to happen on the CPU
    /// that masked. A guard that could cross threads would restore the wrong
    /// CPU's state and leave this one masked forever.
    not_send: PhantomData<*const ()>,
}

// SAFETY: a shared reference to a guard reaches the data only through `Deref`,
// so sharing one across threads shares `&T`, which is what `T: Sync` allows.
// The interrupt state is a plain integer read only by the owning thread's drop.
unsafe impl<T: ?Sized + Sync, C: IrqControl> Sync for IrqSpinLockGuard<'_, T, C> {}

impl<T: ?Sized, C: IrqControl> Deref for IrqSpinLockGuard<'_, T, C> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard exists only while its CPU holds the inner lock,
        // which serves one ticket at a time, so no other reference to the data
        // can exist for the life of this borrow.
        unsafe { &*self.lock.inner.data.get() }
    }
}

impl<T: ?Sized, C: IrqControl> DerefMut for IrqSpinLockGuard<'_, T, C> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, with `&mut self` additionally proving this is
        // the only borrow taken through the guard itself.
        unsafe { &mut *self.lock.inner.data.get() }
    }
}

impl<T: ?Sized, C: IrqControl> Drop for IrqSpinLockGuard<'_, T, C> {
    fn drop(&mut self) {
        // SAFETY: an acquisition handed this guard to this CPU, it is being
        // consumed here so no further access can happen through it, and a guard
        // is dropped exactly once.
        unsafe { self.lock.inner.release() };
        // Only now, with the lock free, is it safe to let interrupts in again.
        C::restore(self.irq_state);
    }
}

impl<T: ?Sized + fmt::Debug, C: IrqControl> fmt::Debug for IrqSpinLockGuard<'_, T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display, C: IrqControl> fmt::Display for IrqSpinLockGuard<'_, T, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

// ---------------------------------------------------------------------------
// Once
// ---------------------------------------------------------------------------

/// Nobody has run the initialiser yet.
const ONCE_INCOMPLETE: u32 = 0;
/// A CPU is inside the initialiser; the value is not yet readable.
const ONCE_RUNNING: u32 = 1;
/// The value is initialised and readable by everyone, forever.
const ONCE_COMPLETE: u32 = 2;

/// A cell that runs an initialiser exactly once, whoever gets there first.
///
/// This is what a kernel global that needs a non-`const` setup becomes: the
/// serial port that has to probe its divisor, the ACPI tables that have to be
/// parsed, the heap that has to be told where memory is. The alternative — a
/// `static mut` written during boot and read forever after — is exactly the
/// pattern that stops being sound when a second CPU starts.
///
/// A caller arriving while the initialiser is running waits for it and then
/// sees the finished value. It does not get `None`, and it does not run a
/// second initialiser, because either would hand out a second instance of
/// something the kernel has exactly one of.
///
/// # Panicking initialisers
///
/// If the initialiser panics, the cell stays in its running state and every
/// later caller spins forever. The kernel builds with `panic = "abort"`, so
/// there is no unwinding and no such state to recover from; a host test that
/// wants to check panic behaviour should not use this type.
///
/// ```
/// # use ferrix_sync::Once;
/// static CONFIG: Once<u32> = Once::new();
///
/// assert!(CONFIG.get().is_none(), "nothing has initialised it yet");
/// assert_eq!(*CONFIG.call_once(|| 7), 7, "the first caller's value wins");
/// assert_eq!(*CONFIG.call_once(|| 9), 7, "and the second one does not run");
/// ```
pub struct Once<T> {
    /// Which of the three states the cell is in.
    ///
    /// `AtomicU32` rather than `AtomicUsize` because every architecture this
    /// kernel targets has 32-bit atomics, including the 32-bit early boot
    /// environments where a 64-bit atomic would need a lock.
    state: AtomicU32,
    /// The value, valid only once the state is complete.
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: the value can be initialised on one thread and dropped on another
// (whoever owns the cell at the end), so it has to be `Send`. Nothing else
// about the cell is thread-affine.
unsafe impl<T: Send> Send for Once<T> {}

// SAFETY: every caller that observes the complete state gets `&T`, so the value
// is shared across threads and must be `Sync`; and the initialising thread
// hands its value to those threads, so it must be `Send`. The state machine
// guarantees exactly one thread ever writes the slot, and only before the
// release store that lets anyone read it.
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    /// Creates an empty cell.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(ONCE_INCOMPLETE),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Returns the value, running `init` first if nobody else has.
    ///
    /// Exactly one call to `init` ever happens for a given cell, and every
    /// caller — including the ones that arrived while it was running — returns
    /// a reference to that one value.
    pub fn call_once<F: FnOnce() -> T>(&self, init: F) -> &T {
        // Acquire: the fast path reads the value, so it has to pair with the
        // release store made by the initialising thread. Without it a CPU could
        // see the complete state and a half-written value.
        if self.state.load(Ordering::Acquire) == ONCE_COMPLETE {
            // SAFETY: the load above observed the complete state through an
            // acquire, so the value is initialised and visible here, and
            // nothing ever writes the slot again.
            return unsafe { self.value_unchecked() };
        }
        self.call_once_slow(init)
    }

    /// The contended path: claim the initialiser, or wait for whoever did.
    ///
    /// Split out of [`call_once`](Self::call_once) so that the common case —
    /// an already-initialised global — stays small enough to inline.
    fn call_once_slow<F: FnOnce() -> T>(&self, init: F) -> &T {
        loop {
            // Acquire on both outcomes: on success because this thread is about
            // to write the slot and must not have its write reordered before
            // the claim, and on failure because the observed state may be
            // complete, in which case this is the load that publishes the
            // value to us.
            let claim = self.state.compare_exchange_weak(
                ONCE_INCOMPLETE,
                ONCE_RUNNING,
                Ordering::Acquire,
                Ordering::Acquire,
            );
            match claim {
                Ok(_) => {
                    self.initialise(init);
                    break;
                }
                Err(ONCE_COMPLETE) => break,
                // Either the state is running, in which case waiting is the
                // whole contract, or the weak exchange failed spuriously.
                Err(_) => spin_loop(),
            }
        }
        // Acquire: for the waiter that broke out on `Err(ONCE_COMPLETE)` the
        // ordering came from the failing exchange, but the initialiser's own
        // release store is what this has to pair with in general.
        while self.state.load(Ordering::Acquire) != ONCE_COMPLETE {
            spin_loop();
        }
        // SAFETY: the loop above only exits on the complete state observed
        // through an acquire load, so the value is initialised and visible.
        unsafe { self.value_unchecked() }
    }

    /// Runs the initialiser and publishes its value.
    ///
    /// Only ever called by the thread whose exchange moved the state from
    /// incomplete to running.
    fn initialise<F: FnOnce() -> T>(&self, init: F) {
        let value = init();
        // SAFETY: this thread claimed the running state, so it is the only one
        // that may touch the slot; every other thread waits for the complete
        // state before reading it.
        let slot = unsafe { &mut *self.value.get() };
        // The `&mut T` this returns is not needed: readers reach the value
        // through the cell, once the store below has published it.
        let _initialised = slot.write(value);
        // Release: this is the store that makes the value readable, so it has
        // to be ordered after the write above, and it pairs with every acquire
        // load of the state elsewhere in this type.
        self.state.store(ONCE_COMPLETE, Ordering::Release);
    }
}

impl<T> Once<T> {
    /// Returns the value if it has been initialised, without waiting.
    ///
    /// A caller that arrives while the initialiser is running gets `None`
    /// rather than blocking, which is the difference between this and
    /// [`call_once`](Self::call_once): it is for code that has something else
    /// to do, such as an early panic handler asking whether the console exists
    /// yet.
    #[must_use]
    pub fn get(&self) -> Option<&T> {
        // Acquire: this decides whether the value is readable, so it pairs with
        // the initialiser's release store.
        (self.state.load(Ordering::Acquire) == ONCE_COMPLETE).then(||
            // SAFETY: the acquire load above observed the complete state, which
            // the initialiser publishes with a release store *after* writing
            // the value -- so the value is initialised and can no longer change.
            unsafe { self.value_unchecked() })
    }

    /// Reports whether the initialiser has finished.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        // Acquire rather than Relaxed: callers use this to decide that a later
        // `get` will succeed, so the same pairing has to hold here.
        self.state.load(Ordering::Acquire) == ONCE_COMPLETE
    }

    /// Borrows the value directly, if there is one.
    ///
    /// `&mut self` proves no other CPU can be inside the cell, so no atomics
    /// are needed to answer.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        (*self.state.get_mut() == ONCE_COMPLETE).then(||
            // SAFETY: the state says the initialiser finished, and `&mut self`
            // proves nothing else can be inside the cell, so the value is
            // initialised and exclusively ours.
            unsafe { self.value.get_mut().assume_init_mut() })
    }

    /// Returns a reference to the value.
    ///
    /// # Safety
    ///
    /// The caller must have observed the complete state through an acquire
    /// load, which is what makes the initialising thread's write both finished
    /// and visible on this CPU.
    unsafe fn value_unchecked(&self) -> &T {
        // SAFETY: the caller guarantees the initialiser has published the
        // value, and nothing writes the slot after that, so a shared reference
        // into the cell cannot alias a mutable one.
        let slot = unsafe { &*self.value.get() };
        // SAFETY: the same guarantee, restated for the `MaybeUninit`: a
        // complete state means this slot was written.
        unsafe { slot.assume_init_ref() }
    }
}

impl<T> Drop for Once<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == ONCE_COMPLETE {
            // SAFETY: the state says the slot was initialised, and `&mut self`
            // in a destructor proves this is the last reference to it, so the
            // value is dropped exactly once.
            unsafe { self.value.get_mut().assume_init_drop() };
        }
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for Once<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.get() {
            Some(value) => f.debug_struct("Once").field("value", value).finish(),
            None => f.write_str("Once { value: <uninitialised> }"),
        }
    }
}

// ---------------------------------------------------------------------------
// RwSpinLock
// ---------------------------------------------------------------------------

/// The top bit of the state word, set while a writer holds the lock.
///
/// It is the top bit so that the reader count can use every other bit and still
/// be a plain increment.
const RW_WRITER: usize = 1 << (usize::BITS - 1);

/// The bits of the state word holding the number of readers inside the lock.
const RW_READERS: usize = !RW_WRITER;

/// A lock allowing many readers or one writer, preferring the writer.
///
/// For the tables the kernel reads constantly and writes almost never — the
/// mount table, the interrupt vector table, the module list — a mutual
/// exclusion lock would serialise every lookup against every other CPU's
/// lookup for no reason. This lets them all in at once and only excludes them
/// while something is being changed.
///
/// # Why writer-preferring
///
/// Once a writer is waiting, new readers are turned away and queue behind it,
/// so the writer gets in as soon as the readers already inside have left. A
/// reader-preferring lock has no such bound: on a table read by every CPU on
/// every syscall, a continuous stream of overlapping readers means the count
/// never reaches zero and the writer waits forever. The cost is the mirror
/// image — a stream of writers delays readers — which is the right trade for
/// data that is written rarely by construction.
///
/// # Not reentrant, in either direction
///
/// Taking a second read lock while holding one deadlocks if a writer arrived in
/// between: the outer reader waits for the inner one, which waits for the
/// writer, which waits for the outer one.
///
/// ```
/// # use ferrix_sync::RwSpinLock;
/// let table = RwSpinLock::new(0_u32);
/// {
///     let first = table.read();
///     let second = table.read();
///     assert_eq!(*first + *second, 0, "both readers are inside at once");
/// }
/// *table.write() = 5;
/// assert_eq!(*table.read(), 5, "the writer's change is visible to readers");
/// ```
pub struct RwSpinLock<T: ?Sized> {
    /// The writer bit and the reader count, in one word so that a reader
    /// entering and a writer entering contend for a single atomic.
    state: AtomicUsize,
    /// How many writers are waiting to get in.
    ///
    /// Separate from `state` because readers only need to look at it, never to
    /// change it, and a writer registering here must not disturb the reader
    /// count it is waiting to see drain.
    writers_waiting: AtomicUsize,
    /// The protected data, reachable only through a guard.
    data: UnsafeCell<T>,
}

// SAFETY: sending the lock sends the data it owns, which is what `T: Send`
// permits.
unsafe impl<T: ?Sized + Send> Send for RwSpinLock<T> {}

// SAFETY: `T: Send` because a writer's `&mut T` moves between threads, and
// `T: Sync` — unlike [`SpinLock`] — because read guards hand `&T` to several
// threads simultaneously, which is exactly what `Sync` licenses.
unsafe impl<T: ?Sized + Send + Sync> Sync for RwSpinLock<T> {}

impl<T> RwSpinLock<T> {
    /// Creates a lock in the unlocked state.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(0),
            writers_waiting: AtomicUsize::new(0),
            data: UnsafeCell::new(value),
        }
    }

    /// Consumes the lock and returns the protected value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> RwSpinLock<T> {
    /// Takes a read lock, waiting for any writer that holds or is waiting.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn read(&self) -> RwSpinLockReadGuard<'_, T> {
        loop {
            if self.try_enter_read() {
                return RwSpinLockReadGuard {
                    lock: self,
                    not_send: PhantomData,
                };
            }
            spin_loop();
        }
    }

    /// Takes a read lock if one is free right now.
    ///
    /// Fails while a writer holds the lock and also while one is merely
    /// waiting, because letting this reader in would be the reader preference
    /// the type does not have.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn try_read(&self) -> Option<RwSpinLockReadGuard<'_, T>> {
        self.try_enter_read().then(|| RwSpinLockReadGuard {
            lock: self,
            not_send: PhantomData,
        })
    }

    /// One attempt to join the readers inside the lock.
    fn try_enter_read(&self) -> bool {
        // Relaxed: this is the writer-preference check, and it is advisory. A
        // writer that registers just after this load simply catches the next
        // reader instead of this one, which costs it one extra turn and no
        // correctness.
        if self.writers_waiting.load(Ordering::Relaxed) != 0 {
            return false;
        }
        // Relaxed: the state read here is only a proposal for the exchange
        // below, which is what carries the ordering.
        let state = self.state.load(Ordering::Relaxed);
        // A writer inside means no reader may enter; a saturated reader count
        // means the next increment would overflow into the writer bit and
        // silently hand this reader a writer's exclusion.
        if state & RW_WRITER != 0 || state & RW_READERS == RW_READERS {
            return false;
        }
        // Acquire on success: this is the operation that lets this CPU read the
        // data, so it pairs with the release of the writer that left. Relaxed
        // on failure, which reads nothing.
        self.state
            .compare_exchange(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }
}

impl<T: ?Sized> RwSpinLock<T> {
    /// Takes the write lock, waiting for every reader and writer inside to
    /// leave, and turning away readers that arrive meanwhile.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn write(&self) -> RwSpinLockWriteGuard<'_, T> {
        // Relaxed: this counter is a hint to readers, not a handoff of data.
        // What it has to be is visible soon, and any atomic increment is.
        let _waiting = self.writers_waiting.fetch_add(1, Ordering::Relaxed);
        while !self.try_enter_write() {
            spin_loop();
        }
        // Deregister only after the lock is held: while this writer waits, the
        // count is what keeps new readers out, and while it holds the lock the
        // writer bit does that job instead.
        let _waited = self.writers_waiting.fetch_sub(1, Ordering::Relaxed);
        RwSpinLockWriteGuard {
            lock: self,
            not_send: PhantomData,
        }
    }

    /// Takes the write lock if the lock is completely free right now.
    ///
    /// This does not register as a waiting writer: a caller that is willing to
    /// give up should not leave readers queueing behind a writer that has gone.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn try_write(&self) -> Option<RwSpinLockWriteGuard<'_, T>> {
        self.try_enter_write().then(|| RwSpinLockWriteGuard {
            lock: self,
            not_send: PhantomData,
        })
    }

    /// One attempt to claim the lock exclusively.
    fn try_enter_write(&self) -> bool {
        // Only a state of exactly zero — no writer, no readers — may be
        // claimed. Acquire on success: this is the operation after which the
        // writer reads and modifies the data, so it pairs with the release of
        // every reader and writer that has left. Relaxed on failure.
        self.state
            .compare_exchange(0, RW_WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Reports whether a writer held the lock at some instant during the call.
    #[must_use]
    pub fn is_write_locked(&self) -> bool {
        // Relaxed: a diagnostic, stale the moment it is returned.
        self.state.load(Ordering::Relaxed) & RW_WRITER != 0
    }

    /// Returns how many readers were inside the lock at some instant during
    /// the call, which is zero while a writer holds it.
    #[must_use]
    pub fn reader_count(&self) -> usize {
        // Relaxed, for the same reason as `is_write_locked`.
        let state = self.state.load(Ordering::Relaxed);
        if state & RW_WRITER != 0 {
            0
        } else {
            state & RW_READERS
        }
    }

    /// Borrows the protected data directly.
    ///
    /// `&mut self` proves no reader and no writer can be inside.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwSpinLock<T> {
    /// Formats the lock without waiting, for the reason given on
    /// [`SpinLock`]'s `Debug`.
    ///
    /// It takes a *read* lock, which succeeds even while other readers are
    /// inside, so formatting a shared table does not have to wait for them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_read() {
            Some(guard) => f
                .debug_struct("RwSpinLock")
                .field("data", &&*guard)
                .finish(),
            None => f.write_str("RwSpinLock { data: <locked> }"),
        }
    }
}

impl<T: Default> Default for RwSpinLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Proof that its holder is one of the readers inside an [`RwSpinLock`].
pub struct RwSpinLockReadGuard<'a, T: ?Sized> {
    /// The lock to leave on drop, and the data to hand out until then.
    lock: &'a RwSpinLock<T>,
    /// Makes the guard `!Send`, for the reason given on [`SpinLockGuard`].
    not_send: PhantomData<*const ()>,
}

// SAFETY: sharing a read guard shares `&T`, which is what `T: Sync` allows.
unsafe impl<T: ?Sized + Sync> Sync for RwSpinLockReadGuard<'_, T> {}

impl<T: ?Sized> Deref for RwSpinLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard exists only while its thread is counted among the
        // readers, and the writer bit cannot be set while the count is
        // non-zero, so no `&mut T` to the data can exist.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for RwSpinLockReadGuard<'_, T> {
    fn drop(&mut self) {
        // Release: a reader publishes nothing, but a writer waiting for the
        // count to reach zero acquires on the exchange that sees it, and that
        // pairing is what orders this reader's reads before the writer's
        // modifications.
        let _remaining = self.lock.state.fetch_sub(1, Ordering::Release);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwSpinLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for RwSpinLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Proof that its holder is the one writer inside an [`RwSpinLock`].
pub struct RwSpinLockWriteGuard<'a, T: ?Sized> {
    /// The lock to release on drop, and the data to hand out until then.
    lock: &'a RwSpinLock<T>,
    /// Makes the guard `!Send`, for the reason given on [`SpinLockGuard`].
    not_send: PhantomData<*const ()>,
}

// SAFETY: a shared reference to a write guard reaches the data only through
// `Deref`, so sharing one across threads shares `&T`, which `T: Sync` allows.
unsafe impl<T: ?Sized + Sync> Sync for RwSpinLockWriteGuard<'_, T> {}

impl<T: ?Sized> Deref for RwSpinLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard exists only while its thread owns the writer bit,
        // and no reader can be counted while that bit is set, so no other
        // reference to the data exists.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for RwSpinLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, with `&mut self` additionally proving this is
        // the only borrow taken through the guard itself.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for RwSpinLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        // A plain store rather than clearing the bit: no reader can have joined
        // while the writer bit was set, so the whole word is known to be
        // exactly `RW_WRITER` and zero is the correct successor.
        //
        // Release: everything this writer changed has to be visible to the next
        // reader or writer, each of which acquires on the exchange that sees
        // this zero.
        self.lock.state.store(0, Ordering::Release);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwSpinLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for RwSpinLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

// ---------------------------------------------------------------------------
// SpinLockedCell
// ---------------------------------------------------------------------------

/// A global that is empty until boot fills it in, and locked thereafter.
///
/// This is the shape of most of the kernel's globals: nothing at reset, a value
/// installed once while the machine comes up, and mutation afterwards that is
/// rare but real — the frame allocator's free lists, the scheduler's run queue,
/// the console driver. [`Once`] is the wrong tool for them because it hands out
/// only `&T`, and a bare [`SpinLock`] is awkward because there is nothing
/// sensible to construct it with at compile time.
///
/// The difference from `SpinLock<Option<T>>`, which is what this is, is that
/// the accessors say what the states mean: absent because boot has not reached
/// it yet, rather than absent as a value in its own right.
///
/// The lock is a plain [`SpinLock`], so the module's rule applies: do not reach
/// a cell from an interrupt handler, and do not call back into the same cell
/// from inside [`with_mut`](Self::with_mut).
///
/// ```
/// # use ferrix_sync::SpinLockedCell;
/// static CONSOLE: SpinLockedCell<u32> = SpinLockedCell::new();
///
/// assert!(!CONSOLE.is_set(), "nothing is installed before boot runs");
/// assert!(CONSOLE.set(0x3f8).is_none(), "installing returns no old value");
/// assert_eq!(CONSOLE.with(|port| *port), Some(0x3f8), "and it reads back");
/// ```
#[derive(Debug, Default)]
pub struct SpinLockedCell<T> {
    /// The value once something has installed one.
    inner: SpinLock<Option<T>>,
}

impl<T> SpinLockedCell<T> {
    /// Creates an empty cell.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(None),
        }
    }

    /// Creates a cell that already holds a value.
    #[must_use]
    pub const fn with_value(value: T) -> Self {
        Self {
            inner: SpinLock::new(Some(value)),
        }
    }

    /// Installs a value, returning whatever was there before.
    ///
    /// Returning the old value rather than refusing keeps the "installed twice"
    /// decision with the caller: boot code can assert the result was `None`,
    /// and a driver that legitimately re-registers can ignore it.
    pub fn set(&self, value: T) -> Option<T> {
        self.inner.lock().replace(value)
    }

    /// Removes the value, returning it.
    pub fn take(&self) -> Option<T> {
        self.inner.lock().take()
    }

    /// Reports whether a value was installed at some instant during the call.
    ///
    /// Answered with [`try_lock`](SpinLock::try_lock), so it never waits; a
    /// cell that is locked at that instant reads as not set, which is the safe
    /// answer for the diagnostic this is.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.inner.try_lock().is_some_and(|value| value.is_some())
    }

    /// Runs `f` on the value, if there is one, holding the lock throughout.
    ///
    /// A closure rather than a returned reference, because the lock has to
    /// outlive the borrow and a guard cannot be handed out with an `Option`
    /// projection without pinning the caller to this crate's guard type.
    pub fn with<R, F: FnOnce(&T) -> R>(&self, f: F) -> Option<R> {
        self.inner.lock().as_ref().map(f)
    }

    /// Runs `f` on the value mutably, if there is one, holding the lock.
    pub fn with_mut<R, F: FnOnce(&mut T) -> R>(&self, f: F) -> Option<R> {
        self.inner.lock().as_mut().map(f)
    }

    /// Takes the lock and hands out the `Option` itself.
    ///
    /// For the caller that needs to look and then install without another CPU
    /// slipping in between.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn lock(&self) -> SpinLockGuard<'_, Option<T>> {
        self.inner.lock()
    }

    /// Borrows the value directly, if there is one.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.inner.get_mut().as_mut()
    }
}

// ---------------------------------------------------------------------------
// SleepLock
// ---------------------------------------------------------------------------

/// Where a caller waits for a [`SleepLock`] somebody else holds, and how the
/// holder tells it to look again.
///
/// This crate cannot see the scheduler, so the lock that sleeps is built over
/// this trait and the kernel lends the implementation: a wait queue, whose
/// `park_until` blocks the running task and whose `unpark_all` wakes every
/// task on it. The kernel's is the only implementation that sleeps.
/// [`SpinParker`] lends one that spins, for host tests and for a kernel whose
/// scheduler is not running yet.
///
/// # The contract
///
/// * `park_until(ready)` returns once `ready` has answered `true`. It may also
///   return before that — a spurious wake, a periodic recheck — because the
///   lock loops, and an early return costs one more look and nothing else.
/// * `ready` runs with the lock not held, and on every wake, spurious ones
///   included; it reads one atomic and nothing else, so an implementation
///   may call it from wherever it checks its condition, including under a
///   lock of its own.
/// * An implementation looks at `ready` after it has made itself findable by
///   `unpark_all`, so that an unpark arriving between the caller's last look
///   and its sleep is not lost. The kernel's wait queue is built around
///   exactly that order. The lock's side of the same argument is that its
///   release stores the flag with `SeqCst` before it calls `unpark_all`, and
///   `ready` loads it with `SeqCst`: a Dekker between the flag and the
///   parking's own list, so that neither side's look can move above its own
///   publication, whatever the parking's list is guarded by.
/// * `may_park` is called on every acquisition, contended or not, before the
///   lock is tried. The kernel's implementation checks there that the caller
///   is somewhere a task may block at all — preemption on, interrupts on —
///   because the rule is only otherwise enforced when the lock is contended,
///   and a holder that takes one under a spin lock would pass every quiet
///   boot and fail the first busy one.
pub trait Parking: Send + Sync {
    /// Block the caller until `ready` answers `true`, or until something
    /// else wakes it; the caller looks again either way.
    fn park_until(&self, ready: &mut dyn FnMut() -> bool);
    /// Wake everything parked here, so that each looks again.
    fn unpark_all(&self);
    /// Called before every attempt at the lock: the place to check that the
    /// caller may block, as the kernel's implementation does. The default
    /// checks nothing, which is right wherever a thread may always block.
    fn may_park(&self) {}
}

/// Hands out a [`Parking`] to each [`SleepLock`] made with it.
///
/// One per lock rather than one shared, so a release wakes the waiters of
/// that lock and no other's.
pub trait Parker: Send + Sync + fmt::Debug {
    /// A fresh parking with nobody on it, or `None` when there is no memory
    /// for one, in which case the lock's waiters spin: see
    /// [`SleepLock::new`].
    fn new_parking(&self) -> Option<Box<dyn Parking>>;
}

/// The [`Parker`] whose waiters spin.
///
/// For host tests, where a spinning thread is one the OS preempts, and for a
/// kernel before its scheduler runs, when there is nothing to switch to. Not
/// for a running kernel: a task spinning for a sleeping lock's holder keeps
/// its processor while the holder waits for one.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpinParker;

impl Parker for SpinParker {
    fn new_parking(&self) -> Option<Box<dyn Parking>> {
        // A zero-sized parking: `Box::new` of it allocates nothing.
        Some(Box::new(SpinParking))
    }
}

/// [`SpinParker`]'s parking: `park_until` spins on `ready`, and there is
/// nobody asleep for `unpark_all` to wake.
struct SpinParking;

impl Parking for SpinParking {
    fn park_until(&self, ready: &mut dyn FnMut() -> bool) {
        while !ready() {
            spin_loop();
        }
    }

    fn unpark_all(&self) {}
}

/// A mutual-exclusion lock whose waiters sleep rather than spin.
///
/// For a critical section that may block: a rename whose path walks read a
/// directory off a disk, a file read that waits for its pages. A spin lock
/// held across such a wait stalls every other CPU that wants it for as long as
/// the disk takes, and a [`PreemptSpinLock`]'s holder is forbidden to block at
/// all. This lock hands its waiters to the [`Parking`] its [`Parker`] lent, so
/// they sleep until the holder lets go, and the holder may sleep for as long
/// as its work takes.
///
/// It is not a ticket lock: after a release, whoever's exchange wins next takes
/// the lock, and a waiter woken from its sleep competes with a caller arriving
/// just then. Linux's `mutex` is the same. The waits it serialises are long
/// and rare — one rename at a time per namespace — where a spin lock's
/// arrival-order guarantee bought nothing and its spinning cost a processor.
///
/// **Rules.** Never take one from an interrupt handler. Never take one while
/// holding any spin lock, or anywhere else preemption is off: the kernel's
/// scheduler stops the machine if a task blocks with the count raised
/// (FX-0503), and the uncontended case merely gets away with it. The guard is
/// `!Send`, as a spin lock's is.
///
/// ```
/// use ferrix_sync::{SleepLock, SpinParker};
///
/// let lock = SleepLock::new(0_u32, &SpinParker);
/// *lock.lock() += 1;
/// assert_eq!(*lock.lock(), 1);
/// ```
pub struct SleepLock<T: ?Sized> {
    /// Whether somebody holds the lock.
    locked: AtomicBool,
    /// How many callers are between announcing that they will park and
    /// coming back from the parking. A release that finds none skips the
    /// parking altogether, which for a lock taken on every `read` is the
    /// difference between one atomic and a masked-interrupt lock per call.
    waiters: AtomicUsize,
    /// Where the waiters sleep, and how a release wakes them.
    parking: Box<dyn Parking>,
    /// The protected data, reachable only through a guard.
    data: UnsafeCell<T>,
}

// SAFETY: as for `SpinLock`: sending the lock sends the data it owns, which
// `T: Send` permits; the flag is an atomic and the parking is `Send + Sync` by
// its bound.
unsafe impl<T: ?Sized + Send> Send for SleepLock<T> {}

// SAFETY: as for `SpinLock`: a shared lock hands the data to one thread at a
// time, the one whose exchange won, so sharing it asks only `T: Send`.
unsafe impl<T: ?Sized + Send> Sync for SleepLock<T> {}

impl<T> SleepLock<T> {
    /// A lock in the unlocked state, whose waiters sleep where `parker` says.
    ///
    /// Not `const`, because the parking is lent at run time. A lock that has
    /// to be a `static` is a spin lock; a kernel that makes one of these
    /// before its scheduler runs gives it a parker that spins.
    ///
    /// Never fails. A parker with no memory for a parking answers `None`, and
    /// the lock then parks its waiters by spinning, which `SpinParking`
    /// does without allocating: slower under contention, never wrong, and
    /// better than a lock that could not be made at all when memory has run
    /// out (the kernel's finding F-23).
    #[must_use]
    pub fn new(value: T, parker: &dyn Parker) -> Self {
        let parking = parker
            .new_parking()
            // A zero-sized parking: `Box::new` of it allocates nothing.
            .unwrap_or_else(|| Box::new(SpinParking));
        Self {
            locked: AtomicBool::new(false),
            waiters: AtomicUsize::new(0),
            parking,
            data: UnsafeCell::new(value),
        }
    }

    /// Consumes the lock and returns the protected value.
    ///
    /// Taking the lock by value proves no guard is outstanding.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> SleepLock<T> {
    /// Takes the lock, sleeping while somebody else holds it.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn lock(&self) -> SleepLockGuard<'_, T> {
        self.parking.may_park();
        while !self.try_acquire() {
            // The waiter's half of two Dekkers with `release`, both on the
            // same rule: publish first, look second, sequentially consistent
            // on both sides, so that neither side's look can be reordered
            // above its own publication and one of the two always sees the
            // other. First the count: this increment is the publication the
            // releaser's look at `waiters` pairs with, so a release that
            // finds the count zero happened before this increment, and the
            // look at the flag below then finds the lock free. Then the
            // parking's own list: the parking publishes the waiter (on its
            // list, marked blocked) before the closure's last look, and the
            // releaser stores the flag before it looks at the list, so the
            // waiter sees the lock free or the releaser sees the waiter. That
            // one holds even without the flag's ordering: a wake that missed
            // the waiter ran its critical section on the list's lock before
            // the waiter's push, so the flag store happens-before the last
            // look through that lock's own release and acquire.
            let _ = self.waiters.fetch_add(1, Ordering::SeqCst);
            self.parking
                .park_until(&mut || !self.locked.load(Ordering::SeqCst));
            let _ = self.waiters.fetch_sub(1, Ordering::SeqCst);
        }
        SleepLockGuard {
            lock: self,
            not_send: PhantomData,
        }
    }

    /// Takes the lock if it is free right now, and gives up otherwise.
    ///
    /// Asks the parking's `may_park` first all the same: the rule about
    /// where a sleeping lock may be taken does not depend on whether this
    /// attempt would have slept.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub fn try_lock(&self) -> Option<SleepLockGuard<'_, T>> {
        self.parking.may_park();
        self.try_acquire().then(|| SleepLockGuard {
            lock: self,
            not_send: PhantomData,
        })
    }

    /// One attempt at the lock, leaving the release to the caller.
    fn try_acquire(&self) -> bool {
        // Acquire on success: this is the exchange that makes the lock ours,
        // so it pairs with the previous holder's releasing store and makes
        // that holder's writes to the data visible before ours. Relaxed on
        // failure: a failed attempt reads no protected data.
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Reports whether the lock was held at some instant during the call.
    ///
    /// A statistic, not a decision, as [`SpinLock::is_locked`] is.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        // Relaxed: the answer is stale by the time it is returned.
        self.locked.load(Ordering::Relaxed)
    }

    /// Borrows the protected data directly.
    ///
    /// Safe, and free: `&mut self` is itself proof that no guard exists.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    /// Releases the lock without consuming a guard.
    ///
    /// # Safety
    ///
    /// The caller must be the current holder of this lock and must not touch
    /// the protected data afterwards.
    unsafe fn release(&self) {
        // SeqCst: it is a release, so that everything written under the lock
        // is visible to the next holder, whose acquiring exchange pairs with
        // it; and it is the releaser's half of the Dekker with `lock`. This
        // store has to be ordered before `unpark_all`'s look at the parking's
        // list, and the waiter's load of this flag after its publication on
        // that list, or a release could slip between a waiter's last look and
        // its sleep. Sequential consistency on both sides is what forbids
        // that, and it costs nothing measurable on a lock taken to sleep.
        self.locked.store(false, Ordering::SeqCst);
        // After the store, so that a waiter woken here finds the lock free;
        // and only if somebody announced a wait, which is the other half of
        // the Dekker on `waiters`: a waiter that incremented after this look
        // loads the flag after this store and does not sleep. An uncontended
        // release, the common case for a lock taken on every `read`, never
        // touches the parking at all.
        if self.waiters.load(Ordering::SeqCst) != 0 {
            self.parking.unpark_all();
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SleepLock<T> {
    /// Formats the lock without ever waiting for it, as [`SpinLock`] does.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => f.debug_struct("SleepLock").field("data", &&*guard).finish(),
            None => f.write_str("SleepLock { data: <locked> }"),
        }
    }
}

/// Proof that its holder is inside a [`SleepLock`]'s critical section.
///
/// The lock is released when this is dropped. Unlike a spin lock's guard it
/// may be held across a sleep; it is `!Send` for the reason
/// [`SpinLockGuard`] gives.
pub struct SleepLockGuard<'a, T: ?Sized> {
    /// The lock to release on drop, and the data to hand out until then.
    lock: &'a SleepLock<T>,
    /// Makes the guard `!Send`.
    not_send: PhantomData<*const ()>,
}

// SAFETY: a shared reference to a guard reaches the data only through `Deref`,
// so sharing one across threads shares `&T`, which is what `T: Sync` allows.
unsafe impl<T: ?Sized + Sync> Sync for SleepLockGuard<'_, T> {}

impl<T: ?Sized> Deref for SleepLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard exists only while its thread holds the lock, and
        // the lock admits one holder at a time, so no other reference to the
        // data can exist for the life of this borrow.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SleepLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, with `&mut self` additionally proving this
        // is the only borrow taken through the guard itself.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SleepLockGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: an acquisition handed this guard to this thread, it is
        // being consumed here so no further access can happen through it,
        // and a guard is dropped exactly once.
        unsafe { self.lock.release() };
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SleepLockGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for SleepLockGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}
