//! Timers: one thread runs every timer's callback at its deadline
//! (`docs/NVIDIA.md` §4.1: "the 1 Hz RC timer and the nanosecond timers").
//!
//! Linux runs these callbacks from a timer interrupt's soft-IRQ; RM's
//! callbacks then enter RM, which takes its own locks. Here they run on the
//! timer thread, one at a time, with no lock of this module held, so a
//! callback may start or cancel any timer, its own included.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::futex;
use crate::libc;
use crate::sync::{Event, Mutex};
use crate::thread;
use crate::time;

/// A timer. Its fields other than the atomics are touched only under
/// [`TIMERS`]'s lock.
#[derive(Debug)]
pub struct Timer {
    /// The next timer in the list.
    next: UnsafeCell<*mut Timer>,
    /// What to call.
    callback: extern "C" fn(*mut c_void),
    /// Its argument.
    argument: *mut c_void,
    /// When it fires on the monotonic clock, 0 when disarmed.
    deadline: UnsafeCell<u64>,
    /// Whether its callback is running now.
    running: AtomicBool,
}

/// Every timer, and the timer thread's state.
struct Timers {
    /// Guards the list and every timer's deadline.
    lock: Mutex,
    /// The first timer.
    head: UnsafeCell<*mut Timer>,
    /// Signalled when a deadline moves, so the thread looks again.
    changed: Event,
    /// Signalled when a callback returns, for a cancel that waits.
    returned: Event,
    /// 0 not started, 1 starting, 2 running.
    state: AtomicU32,
    /// The timer thread's id.
    tid: AtomicU32,
}

// SAFETY: the list and deadlines are only touched under `lock`; the rest
// are atomics.
unsafe impl Sync for Timers {}

/// The timers.
static TIMERS: Timers = Timers {
    lock: Mutex::new(),
    head: UnsafeCell::new(ptr::null_mut()),
    changed: Event::new(),
    returned: Event::new(),
    state: AtomicU32::new(0),
    tid: AtomicU32::new(0),
};

/// Start the timer thread if nobody has; whether it runs.
fn start() -> bool {
    match TIMERS
        .state
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => {
            let started = thread::spawn(serve, ptr::null_mut());
            TIMERS
                .state
                .store(if started { 2 } else { 0 }, Ordering::Release);
            started
        }
        Err(1) => {
            while TIMERS.state.load(Ordering::Acquire) == 1 {
                core::hint::spin_loop();
            }
            TIMERS.state.load(Ordering::Acquire) == 2
        }
        Err(state) => state == 2,
    }
}

/// The timer thread.
extern "C" fn serve(_: *mut c_void) {
    TIMERS.tid.store(futex::tid(), Ordering::Release);
    loop {
        let seen = TIMERS.changed.now();
        let now = time::monotonic();
        let mut due: *mut Timer = ptr::null_mut();
        let mut earliest = u64::MAX;
        TIMERS.lock.lock();
        // SAFETY: under the lock.
        let mut at = unsafe { *TIMERS.head.get() };
        while !at.is_null() {
            // SAFETY: a live timer of the list, under the lock.
            let timer = unsafe { &*at };
            // SAFETY: under the lock.
            let deadline = unsafe { *timer.deadline.get() };
            if deadline != 0 {
                if deadline <= now {
                    due = at;
                    // SAFETY: under the lock: it fires now, once.
                    unsafe { *timer.deadline.get() = 0 };
                    timer.running.store(true, Ordering::Release);
                    break;
                }
                earliest = earliest.min(deadline);
            }
            // SAFETY: under the lock.
            at = unsafe { *timer.next.get() };
        }
        TIMERS.lock.unlock();
        if due.is_null() {
            let timeout = (earliest != u64::MAX).then(|| time::timespec(earliest - now));
            TIMERS.changed.wait(seen, timeout);
            continue;
        }
        // SAFETY: `running` keeps it alive: `destroy` waits for it.
        let timer = unsafe { &*due };
        (timer.callback)(timer.argument);
        timer.running.store(false, Ordering::Release);
        TIMERS.returned.signal();
    }
}

/// Make a disarmed timer that will call `callback(argument)`; null without
/// memory or without its thread.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_timer_create(
    callback: extern "C" fn(*mut c_void),
    argument: *mut c_void,
) -> *mut Timer {
    if !start() {
        return ptr::null_mut();
    }
    // SAFETY: a fresh block for one timer; malloc's alignment suffices.
    let timer = unsafe { libc::malloc(size_of::<Timer>()) }.cast::<Timer>();
    if timer.is_null() {
        return timer;
    }
    TIMERS.lock.lock();
    // SAFETY: a fresh, aligned block, linked in under the lock.
    unsafe {
        timer.write(Timer {
            next: UnsafeCell::new(*TIMERS.head.get()),
            callback,
            argument,
            deadline: UnsafeCell::new(0),
            running: AtomicBool::new(false),
        });
    }
    // SAFETY: under the lock.
    unsafe { *TIMERS.head.get() = timer };
    TIMERS.lock.unlock();
    timer
}

/// Arm `timer` to fire `nanoseconds` from now, replacing any deadline.
///
/// # Safety
///
/// `timer` is one [`nvos_timer_create`] made and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_timer_start(timer: *mut Timer, nanoseconds: u64) {
    let deadline = time::monotonic().saturating_add(nanoseconds).max(1);
    TIMERS.lock.lock();
    // SAFETY: the caller vouches for the timer; under the lock.
    unsafe { *(*timer).deadline.get() = deadline };
    TIMERS.lock.unlock();
    TIMERS.changed.signal();
}

/// Disarm `timer`, and wait for its callback if it is running on another
/// thread than the caller's. Returns whether it was armed.
///
/// # Safety
///
/// `timer` is one [`nvos_timer_create`] made and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_timer_cancel(timer: *mut Timer) -> bool {
    TIMERS.lock.lock();
    // SAFETY: the caller vouches for the timer; under the lock.
    let armed = unsafe { core::mem::replace(&mut *(*timer).deadline.get(), 0) } != 0;
    TIMERS.lock.unlock();
    if TIMERS.tid.load(Ordering::Acquire) != futex::tid() {
        loop {
            let seen = TIMERS.returned.now();
            // SAFETY: as above.
            if !unsafe { &*timer }.running.load(Ordering::Acquire) {
                break;
            }
            TIMERS.returned.wait(seen, None);
        }
    }
    armed
}

/// Cancel `timer`, waiting for its callback, and free it.
///
/// # Safety
///
/// `timer` is one [`nvos_timer_create`] made and not yet destroyed, and is
/// not destroyed from its own callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_timer_destroy(timer: *mut Timer) {
    // SAFETY: the caller vouches for the timer.
    let _ = unsafe { nvos_timer_cancel(timer) };
    TIMERS.lock.lock();
    // SAFETY: under the lock: unlink it.
    let mut link = TIMERS.head.get();
    // SAFETY: each link is the head or a live timer's `next`, under the lock.
    while !unsafe { *link }.is_null() {
        // SAFETY: as above.
        let at = unsafe { *link };
        if at == timer {
            // SAFETY: as above.
            unsafe { *link = *(*at).next.get() };
            break;
        }
        // SAFETY: as above.
        link = unsafe { (*at).next.get() };
    }
    TIMERS.lock.unlock();
    // SAFETY: unlinked and not running, so nothing else can reach it.
    unsafe { libc::free(timer.cast()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static FIRED: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn fire(_: *mut c_void) {
        let _ = FIRED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn a_timer_fires_once_and_a_cancelled_one_never() {
        let timer = nvos_timer_create(fire, ptr::null_mut());
        let other = nvos_timer_create(fire, ptr::null_mut());
        assert!(!timer.is_null() && !other.is_null());
        // SAFETY: both were made above and are destroyed below.
        unsafe {
            nvos_timer_start(timer, 5_000_000);
            nvos_timer_start(other, 50_000_000);
            assert!(nvos_timer_cancel(other));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(FIRED.load(Ordering::SeqCst), 1);
        // SAFETY: as above.
        unsafe {
            nvos_timer_destroy(timer);
            nvos_timer_destroy(other);
        }
    }
}
