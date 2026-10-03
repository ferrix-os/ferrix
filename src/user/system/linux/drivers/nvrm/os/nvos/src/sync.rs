//! Locks, semaphores and waits over futexes (`docs/NVIDIA.md` §4.2).
//!
//! Each type is `#[repr(C)]` words and nothing else, zero meaning its
//! initial state where one is natural, so NVIDIA's kept C can embed one in a
//! structure of its own (`include/nvos.h` declares them) and RM's `os_*`
//! calls can allocate one. None tracks an owner: Linux's `struct semaphore`,
//! which RM's mutexes are, may be released by a thread other than the one
//! that took it, and so may these.
//!
//! In ring 3 nothing runs with interrupts off. RM's spinlocks are the
//! [`Mutex`] after a short spin; an interrupt handler is a thread, so a lock
//! it shares with a thread it interrupted is simply waited for.

use core::hint::spin_loop;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::futex;
use crate::libc::Timespec;

/// How many times a spinlock tries before it sleeps.
const SPINS: u32 = 100;

/// A mutex: 0 free, 1 held, 2 held with sleepers (Drepper's three states).
#[repr(C)]
#[derive(Debug, Default)]
pub struct Mutex {
    /// The state.
    state: AtomicU32,
}

impl Mutex {
    /// A free mutex.
    pub const fn new() -> Mutex {
        Mutex {
            state: AtomicU32::new(0),
        }
    }

    /// Take it, if free, without waiting.
    pub fn try_lock(&self) -> bool {
        self.state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Take it, spinning `spins` times before sleeping.
    pub fn lock_spinning(&self, spins: u32) {
        for _ in 0..spins {
            if self.try_lock() {
                return;
            }
            spin_loop();
        }
        self.lock();
    }

    /// Take it, sleeping while another holds it.
    pub fn lock(&self) {
        if self.try_lock() {
            return;
        }
        // Mark it contended, and sleep until it was free when marked.
        while self.state.swap(2, Ordering::Acquire) != 0 {
            futex::wait(&self.state, 2, None);
        }
    }

    /// Give it back, and wake one sleeper if any.
    pub fn unlock(&self) {
        if self.state.swap(0, Ordering::Release) == 2 {
            futex::wake(&self.state, 1);
        }
    }

    /// Whether anyone holds it.
    pub fn is_locked(&self) -> bool {
        self.state.load(Ordering::Relaxed) != 0
    }
}

/// A counting semaphore: Linux's `struct semaphore`.
#[repr(C)]
#[derive(Debug, Default)]
pub struct Semaphore {
    /// What is left to take.
    count: AtomicU32,
    /// How many are asleep on `count`.
    sleepers: AtomicU32,
}

impl Semaphore {
    /// A semaphore holding `count`.
    pub const fn new(count: u32) -> Semaphore {
        Semaphore {
            count: AtomicU32::new(count),
            sleepers: AtomicU32::new(0),
        }
    }

    /// Take one, if there is one, without waiting.
    pub fn try_down(&self) -> bool {
        let mut count = self.count.load(Ordering::Relaxed);
        while count > 0 {
            match self.count.compare_exchange_weak(
                count,
                count - 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => count = now,
            }
        }
        false
    }

    /// Take one, sleeping while there is none.
    pub fn down(&self) {
        loop {
            if self.try_down() {
                return;
            }
            let _ = self.sleepers.fetch_add(1, Ordering::SeqCst);
            // The count is read again by the kernel: an `up` between the
            // failed take and this sleep leaves it non-zero, and the wait
            // returns at once.
            futex::wait(&self.count, 0, None);
            let _ = self.sleepers.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Give one back, and wake a sleeper if any.
    pub fn up(&self) {
        let _ = self.count.fetch_add(1, Ordering::Release);
        if self.sleepers.load(Ordering::SeqCst) != 0 {
            futex::wake(&self.count, 1);
        }
    }
}

/// The writer bit of [`RwLock`]'s state; the rest counts readers.
const WRITER: u32 = 1 << 31;

/// A reader-writer lock that lets a waiting writer in before new readers.
#[repr(C)]
#[derive(Debug, Default)]
pub struct RwLock {
    /// [`WRITER`] while written, else the number of readers.
    state: AtomicU32,
    /// Writers waiting, which hold new readers back.
    writers_waiting: AtomicU32,
    /// Bumped on every release; sleepers wait for it to move.
    sequence: AtomicU32,
}

impl RwLock {
    /// A free lock.
    pub const fn new() -> RwLock {
        RwLock {
            state: AtomicU32::new(0),
            writers_waiting: AtomicU32::new(0),
            sequence: AtomicU32::new(0),
        }
    }

    /// Read it, if no writer holds it or waits, without waiting.
    pub fn try_read(&self) -> bool {
        if self.writers_waiting.load(Ordering::Acquire) != 0 {
            return false;
        }
        self.try_read_ignoring_writers()
    }

    /// Read it if no writer holds it.
    fn try_read_ignoring_writers(&self) -> bool {
        let mut state = self.state.load(Ordering::Relaxed);
        while state & WRITER == 0 && state < WRITER - 1 {
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => state = now,
            }
        }
        false
    }

    /// Write it, if nobody holds it, without waiting.
    pub fn try_write(&self) -> bool {
        self.state
            .compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// Read it, sleeping while a writer holds it or waits.
    pub fn read(&self) {
        loop {
            let seen = self.sequence.load(Ordering::Acquire);
            if self.try_read() {
                return;
            }
            futex::wait(&self.sequence, seen, None);
        }
    }

    /// Write it, sleeping while anyone holds it.
    pub fn write(&self) {
        let _ = self.writers_waiting.fetch_add(1, Ordering::SeqCst);
        loop {
            let seen = self.sequence.load(Ordering::Acquire);
            if self.try_write() {
                let _ = self.writers_waiting.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            futex::wait(&self.sequence, seen, None);
        }
    }

    /// Stop reading it.
    pub fn read_unlock(&self) {
        if self.state.fetch_sub(1, Ordering::Release) == 1 {
            self.release();
        }
    }

    /// Stop writing it.
    pub fn write_unlock(&self) {
        self.state.store(0, Ordering::Release);
        self.release();
    }

    /// Wake everyone asleep on it, to try again.
    fn release(&self) {
        let _ = self.sequence.fetch_add(1, Ordering::Release);
        futex::wake(&self.sequence, u32::MAX);
    }
}

/// A completion, as Linux's `complete_all` leaves one: once done it stays
/// done, and every wait after returns at once. RM's wait queues are these.
#[repr(C)]
#[derive(Debug, Default)]
pub struct Completion {
    /// 1 once done.
    done: AtomicU32,
}

impl Completion {
    /// A completion not yet done.
    pub const fn new() -> Completion {
        Completion {
            done: AtomicU32::new(0),
        }
    }

    /// Sleep until it is done.
    pub fn wait(&self) {
        while self.done.load(Ordering::Acquire) == 0 {
            futex::wait(&self.done, 0, None);
        }
    }

    /// Mark it done and wake every waiter.
    pub fn complete_all(&self) {
        self.done.store(1, Ordering::Release);
        futex::wake(&self.done, u32::MAX);
    }

    /// Whether it is done.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire) != 0
    }
}

/// An event a sleeper waits on to move: a sequence number, bumped by each
/// signal. What changed is read under a [`Mutex`]; the sequence only says
/// to look again.
#[repr(C)]
#[derive(Debug, Default)]
pub struct Event {
    /// Bumped on every signal.
    sequence: AtomicU32,
}

impl Event {
    /// An event never signalled.
    pub const fn new() -> Event {
        Event {
            sequence: AtomicU32::new(0),
        }
    }

    /// The sequence now, to wait on after looking.
    pub fn now(&self) -> u32 {
        self.sequence.load(Ordering::Acquire)
    }

    /// Sleep until a signal after `seen`, or for at most `timeout`.
    pub(crate) fn wait(&self, seen: u32, timeout: Option<Timespec>) {
        futex::wait(&self.sequence, seen, timeout);
    }

    /// Wake every sleeper.
    pub fn signal(&self) {
        let _ = self.sequence.fetch_add(1, Ordering::Release);
        futex::wake(&self.sequence, u32::MAX);
    }
}

/// How long a spinlock spins before it sleeps.
pub const fn spinlock_spins() -> u32 {
    SPINS
}

// ---------------------------------------------------------------------------
// The C view (`include/nvos.h`), for NVIDIA's kept C.
// ---------------------------------------------------------------------------

/// Generate `extern "C"` wrappers over a type's methods, each taking a pointer.
macro_rules! c_methods {
    ($ty:ty { $( $(#[$doc:meta])* fn $name:ident => $method:ident $(-> $ret:ty)?; )* }) => {
        $(
            $(#[$doc])*
            ///
            /// # Safety
            ///
            /// `this` points to a live, initialized value of the type, which
            /// outlives the call.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $name(this: *const $ty) $(-> $ret)? {
                // SAFETY: the caller vouches for the pointer.
                let this = unsafe { &*this };
                this.$method()
            }
        )*
    };
}

c_methods!(Mutex {
    /// Take a mutex.
    fn nvos_mutex_lock => lock;
    /// Take a mutex if free.
    fn nvos_mutex_trylock => try_lock -> bool;
    /// Give a mutex back.
    fn nvos_mutex_unlock => unlock;
});

c_methods!(Semaphore {
    /// Take one from a semaphore.
    fn nvos_sema_down => down;
    /// Take one from a semaphore if there is one.
    fn nvos_sema_trydown => try_down -> bool;
    /// Give one back to a semaphore.
    fn nvos_sema_up => up;
});

c_methods!(RwLock {
    /// Read a reader-writer lock.
    fn nvos_rwlock_read => read;
    /// Write a reader-writer lock.
    fn nvos_rwlock_write => write;
    /// Stop reading a reader-writer lock.
    fn nvos_rwlock_read_unlock => read_unlock;
    /// Stop writing a reader-writer lock.
    fn nvos_rwlock_write_unlock => write_unlock;
});

c_methods!(Completion {
    /// Wait for a completion.
    fn nvos_completion_wait => wait;
    /// Complete a completion for everyone.
    fn nvos_completion_complete_all => complete_all;
});

c_methods!(Event {
    /// Signal an event.
    fn nvos_event_signal => signal;
    /// The event's sequence now.
    fn nvos_event_now => now -> u32;
});

/// Take a spinlock: a mutex after a short spin.
///
/// # Safety
///
/// `this` points to a live mutex.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_spin_lock(this: *const Mutex) {
    // SAFETY: the caller vouches for the pointer.
    let this = unsafe { &*this };
    this.lock_spinning(SPINS);
}

/// Initialize a semaphore in place with `count`.
///
/// # Safety
///
/// `this` points to writable memory for a semaphore nobody uses yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_sema_init(this: *mut Semaphore, count: u32) {
    // SAFETY: the caller vouches for the memory.
    unsafe { this.write(Semaphore::new(count)) };
}

/// Sleep until the event moves past `seen`.
///
/// # Safety
///
/// `this` points to a live event.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_event_wait(this: *const Event, seen: u32) {
    // SAFETY: the caller vouches for the pointer.
    let this = unsafe { &*this };
    this.wait(seen, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn a_mutex_excludes() {
        struct Shared {
            lock: Mutex,
            count: core::cell::UnsafeCell<u64>,
        }
        // SAFETY: `count` is only touched under `lock`.
        unsafe impl Sync for Shared {}
        let shared = Arc::new(Shared {
            lock: Mutex::new(),
            count: core::cell::UnsafeCell::new(0),
        });
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    for _ in 0..10_000 {
                        shared.lock.lock_spinning(10);
                        // SAFETY: under the lock.
                        unsafe { *shared.count.get() += 1 };
                        shared.lock.unlock();
                    }
                })
            })
            .collect();
        for thread in threads {
            assert!(thread.join().is_ok());
        }
        // SAFETY: every writer has been joined.
        assert_eq!(unsafe { *shared.count.get() }, 80_000);
    }

    #[test]
    fn a_mutex_is_released_by_another_thread() {
        let lock = Arc::new(Mutex::new());
        lock.lock();
        let other = Arc::clone(&lock);
        assert!(thread::spawn(move || other.unlock()).join().is_ok());
        assert!(lock.try_lock());
    }

    #[test]
    fn a_semaphore_counts() {
        let sema = Semaphore::new(2);
        assert!(sema.try_down());
        assert!(sema.try_down());
        assert!(!sema.try_down());
        sema.up();
        assert!(sema.try_down());
    }

    #[test]
    fn a_semaphore_wakes_a_sleeper() {
        let sema = Arc::new(Semaphore::new(0));
        let other = Arc::clone(&sema);
        let sleeper = thread::spawn(move || other.down());
        thread::sleep(std::time::Duration::from_millis(20));
        sema.up();
        assert!(sleeper.join().is_ok());
    }

    #[test]
    fn readers_share_and_writers_exclude() {
        let lock = RwLock::new();
        assert!(lock.try_read());
        assert!(lock.try_read());
        assert!(!lock.try_write());
        lock.read_unlock();
        lock.read_unlock();
        assert!(lock.try_write());
        assert!(!lock.try_read());
        lock.write_unlock();
        assert!(lock.try_read());
        lock.read_unlock();
    }

    #[test]
    fn a_writer_gets_in_past_readers() {
        let lock = Arc::new(RwLock::new());
        lock.read();
        let other = Arc::clone(&lock);
        let writer = thread::spawn(move || {
            other.write();
            other.write_unlock();
        });
        thread::sleep(std::time::Duration::from_millis(20));
        lock.read_unlock();
        assert!(writer.join().is_ok());
    }

    #[test]
    fn a_completion_stays_done() {
        let done = Arc::new(Completion::new());
        let other = Arc::clone(&done);
        let waiter = thread::spawn(move || other.wait());
        done.complete_all();
        assert!(waiter.join().is_ok());
        done.wait();
        assert!(done.is_done());
    }
}
