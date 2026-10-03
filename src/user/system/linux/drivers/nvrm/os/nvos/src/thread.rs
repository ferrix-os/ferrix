//! The threads this layer starts -- work queues, the timer thread and the
//! interrupt thread -- and what RM asks about the thread it runs on.
//!
//! On Linux these are kernel threads, and an interrupt handler runs in hard
//! interrupt context. Here each is a thread of nvrm's, started with the C
//! library's `pthread_create`, and RM's questions are answered from their
//! ids: `os_get_current_process_flags` says "kernel thread" on one of them,
//! and `os_is_isr` says yes on the interrupt thread while it is inside
//! `rm_isr`. No thread-local storage is needed, which a `no_std` Rust
//! library could not declare on a stable compiler.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::futex;
use crate::libc;

/// The most threads this layer starts and remembers.
const THREADS: usize = 32;

/// The ids of the threads this layer started; 0 is a free slot.
static OURS: [AtomicU32; THREADS] = [const { AtomicU32::new(0) }; THREADS];

/// The interrupt thread's id, 0 before it starts.
static ISR_THREAD: AtomicU32 = AtomicU32::new(0);

/// Whether the interrupt thread is inside the top half.
static IN_ISR: AtomicBool = AtomicBool::new(false);

/// What a new thread runs, and what it is given.
struct Start {
    /// The function.
    run: extern "C" fn(*mut c_void),
    /// Its argument.
    argument: *mut c_void,
}

/// Every thread this layer starts begins here: it records itself, then runs.
extern "C" fn trampoline(start: *mut c_void) -> *mut c_void {
    let start = start.cast::<Start>();
    // SAFETY: `spawn` passes a `Start` it allocated and gave up.
    let Start { run, argument } = unsafe { start.read() };
    // SAFETY: allocated by `spawn` with `malloc`, read above, used no more.
    unsafe { libc::free(start.cast()) };
    remember(futex::tid());
    run(argument);
    core::ptr::null_mut()
}

/// Record `tid` as one of ours.
fn remember(tid: u32) {
    for slot in &OURS {
        if slot
            .compare_exchange(0, tid, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}

/// Start a detached thread that runs `run(argument)`. Returns whether it
/// started.
pub(crate) fn spawn(run: extern "C" fn(*mut c_void), argument: *mut c_void) -> bool {
    // SAFETY: a fresh block for one `Start`; malloc's alignment suffices.
    let start = unsafe { libc::malloc(size_of::<Start>()) }.cast::<Start>();
    if start.is_null() {
        return false;
    }
    // SAFETY: `start` is a fresh, aligned block for one `Start`.
    unsafe { start.write(Start { run, argument }) };
    let mut thread = 0_u64;
    // SAFETY: `thread` is writable and `trampoline` takes over `start`.
    let started = unsafe {
        libc::pthread_create(&raw mut thread, core::ptr::null(), trampoline, start.cast())
    };
    if started != 0 {
        // SAFETY: the thread did not start, so `start` is still ours.
        unsafe { libc::free(start.cast()) };
        return false;
    }
    // SAFETY: `thread` names the thread just started, joined by nobody.
    let _ = unsafe { libc::pthread_detach(thread) };
    true
}

/// Whether the calling thread is one this layer started.
pub(crate) fn is_ours() -> bool {
    let tid = futex::tid();
    OURS.iter().any(|slot| slot.load(Ordering::Acquire) == tid)
}

/// Whether the calling thread is in RM's interrupt handler.
pub(crate) fn in_isr() -> bool {
    IN_ISR.load(Ordering::Acquire) && ISR_THREAD.load(Ordering::Acquire) == futex::tid()
}

/// Say the calling thread is the interrupt thread, entering (`true`) or
/// leaving (`false`) RM's top half. nvrm's interrupt thread calls it around
/// `rm_isr`.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_isr_enter_leave(entering: bool) {
    if entering {
        ISR_THREAD.store(futex::tid(), Ordering::Release);
    }
    IN_ISR.store(entering, Ordering::Release);
}
