//! linuxd's locks (docs/microkernel/M5-LIFT.md §3): the same interface the kernel's
//! `SpinLock` has, built for ring 3, where a holder can be preempted and no count
//! stops it. Not a ticket lock: a waiter holding a ticket and switched out would
//! stall everyone behind it. A waiter spins briefly, then gives up the processor.

use core::cell::UnsafeCell;
use core::fmt;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// How long a waiter spins before it yields.
const SPINS: u32 = 256;

pub struct SpinLock<T: ?Sized> {
    held: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: `held` admits one holder at a time, as a mutex does.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> SpinLock<T> {
        SpinLock {
            held: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> SpinLock<T> {
    #[track_caller]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let mut spins = 0_u32;
        loop {
            if !self.held.swap(true, Ordering::Acquire) {
                crate::sched::lock_taken();
                return SpinLockGuard { lock: self };
            }
            while self.held.load(Ordering::Relaxed) {
                spins = spins.wrapping_add(1);
                if spins % SPINS == 0 {
                    crate::sys::thread_yield();
                }
                core::hint::spin_loop();
            }
        }
    }
    #[track_caller]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        (!self.held.swap(true, Ordering::Acquire)).then(|| {
            crate::sched::lock_taken();
            SpinLockGuard { lock: self }
        })
    }
    /// # Safety
    /// As the kernel's; there is no mask here to keep.
    pub unsafe fn try_lock_masked(&self) -> Option<SpinLockGuard<'_, T>> {
        self.try_lock()
    }
    pub fn is_locked(&self) -> bool {
        self.held.load(Ordering::Relaxed)
    }
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => f.debug_struct("SpinLock").field("data", &&*guard).finish(),
            None => f.write_str("SpinLock(locked)"),
        }
    }
}

impl<T: Default> Default for SpinLock<T> {
    fn default() -> Self {
        SpinLock::new(T::default())
    }
}

#[must_use = "the lock is held until the guard is dropped"]
pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard is the one holder.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard is the one holder.
        unsafe { &mut *self.lock.data.get() }
    }
}
impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
        crate::sched::lock_given_back();
    }
}
impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLockGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// Where the kernel's wait queues park a sleeping lock's waiters.
#[derive(Debug, Clone, Copy, Default)]
pub struct SchedParker;
impl ferrix_sync::Parker for SchedParker {
    fn new_parking(&self) -> Option<alloc::boxed::Box<dyn ferrix_sync::Parking>> {
        let queue = crate::fallible::try_box(crate::sched::WaitQueue::new()).ok()?;
        Some(queue)
    }
}

/// # Safety
/// As the kernel's; there is no mask here to keep.
pub unsafe fn try_lock_masked<T>(lock: &SpinLock<T>) -> Option<SpinLockGuard<'_, T>> {
    lock.try_lock()
}
