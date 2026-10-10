//! Futexes, the thread id, and the native calls: the system calls this layer
//! makes by the instruction rather than through the C library.
//!
//! A Ferrix process may make Linux and native calls alike; the kernel picks
//! the table by the number, 0x1000 to 0x1FFF being native
//! (`docs/ARCHITECTURE.md` §2). Both use Linux's convention: `syscall`, the
//! number in RAX, arguments in RDI, RSI, RDX, R10, R8 and R9, the result in
//! RAX, an error as `-errno`. On the host a native number is `ENOSYS`, which
//! is how the host build finds it has no device.

use core::arch::asm;
use core::sync::atomic::AtomicU32;

use ferrix_native::{Raw, Syscall};

use crate::libc::Timespec;

/// Linux's `futex`.
const SYS_FUTEX: usize = 202;
/// Linux's `gettid`.
const SYS_GETTID: usize = 186;
/// `FUTEX_WAIT_PRIVATE`.
const FUTEX_WAIT_PRIVATE: usize = 128;
/// `FUTEX_WAKE_PRIVATE`.
const FUTEX_WAKE_PRIVATE: usize = 129;

/// A system call with up to six arguments; the result register.
///
/// # Safety
///
/// The call must be one whose pointer arguments name memory valid for what
/// the call does with it.
pub(crate) unsafe fn call6(number: usize, a: [usize; 6]) -> usize {
    let mut result = number;
    // SAFETY: the caller vouches for the call; the instruction clobbers RCX
    // and R11, and Ferrix's native calls may also write RSI, RDX and R10
    // back, so all three are declared clobbered.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") result,
            in("rdi") a[0],
            inlateout("rsi") a[1] => _,
            inlateout("rdx") a[2] => _,
            inlateout("r10") a[3] => _,
            in("r8") a[4],
            in("r9") a[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Sleep while `word` holds `expected`, for at most `timeout` if given.
/// Returns early on a wake, a signal, or a changed word, so callers loop.
pub(crate) fn wait(word: &AtomicU32, expected: u32, timeout: Option<Timespec>) {
    let timespec = timeout
        .as_ref()
        .map_or(core::ptr::null(), |t| t as *const Timespec);
    // SAFETY: the word is a live `AtomicU32` the kernel only reads, and the
    // timeout, when given, is a `Timespec` on this frame.
    let _ = unsafe {
        call6(
            SYS_FUTEX,
            [
                word.as_ptr() as usize,
                FUTEX_WAIT_PRIVATE,
                expected as usize,
                timespec as usize,
                0,
                0,
            ],
        )
    };
}

/// Wake up to `count` threads sleeping on `word`; `u32::MAX` is all of
/// them.
///
/// The kernel reads the count as an `int`, Linux's and Ferrix's alike, and
/// wakes ONE sleeper for a count of zero or less. `u32::MAX` is -1 there:
/// until 2026-10-10 "wake everyone" woke one. A reader-writer lock given
/// back with a writer and two readers asleep woke one of them, a reader the
/// waiting writer holds back went to sleep again, and the lock stayed free
/// with every thread that wanted it asleep -- RM's API lock, at a session's
/// start and at a client's end. So the count is capped at `i32::MAX`.
pub(crate) fn wake(word: &AtomicU32, count: u32) {
    #[cfg(test)]
    WAKES.with(|wakes| wakes.set(wakes.get() + 1));
    let count = count.min(i32::MAX as u32);
    // SAFETY: the kernel only uses the word's address as a key.
    let _ = unsafe {
        call6(
            SYS_FUTEX,
            [
                word.as_ptr() as usize,
                FUTEX_WAKE_PRIVATE,
                count as usize,
                0,
                0,
                0,
            ],
        )
    };
}

#[cfg(test)]
std::thread_local! {
    /// How many times the thread has called [`wake`].
    static WAKES: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

/// How many wakes the calling thread has made: for a test that a path
/// makes none.
#[cfg(test)]
pub(crate) fn wakes_by_this_thread() -> u32 {
    WAKES.with(core::cell::Cell::get)
}

/// The time-stamp counter: a clock for what may be a little stale.
#[cfg(not(test))]
pub(crate) fn ticks() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: `rdtsc` reads the counter into EAX and EDX and touches
    // nothing else.
    unsafe {
        asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// The calling thread's id, from the kernel.
fn tid_asked() -> u32 {
    // SAFETY: `gettid` takes no arguments and cannot fail.
    let result = unsafe { call6(SYS_GETTID, [0; 6]) };
    u32::try_from(result).unwrap_or(0)
}

/// The thread pointer: `%fs:0`, which holds its own address on x86-64 in
/// ferrousli as in every C library (`ferrousli/src/thread.rs`). One value a
/// live thread.
#[cfg(not(test))]
fn thread_pointer() -> usize {
    let pointer: usize;
    // SAFETY: reads one word at `%fs:0`, which the C library set before any
    // code of this layer ran on the thread; nothing is written.
    unsafe {
        asm!("mov {}, fs:[0]", out(reg) pointer, options(nostack, readonly, preserves_flags));
    }
    pointer
}

/// The ids kept: a thread pointer and its thread's id, by the pointer.
#[cfg(not(test))]
static TIDS: [(core::sync::atomic::AtomicUsize, AtomicU32); 64] =
    [const { (core::sync::atomic::AtomicUsize::new(0), AtomicU32::new(0)) }; 64];

/// The processor each thread last asked the kernel about, with the thread
/// pointer and the time-stamp counter then: slots as [`TIDS`]'.
#[cfg(not(test))]
static CPUS: [(
    core::sync::atomic::AtomicUsize,
    AtomicU32,
    core::sync::atomic::AtomicU64,
); 64] = [const {
    (
        core::sync::atomic::AtomicUsize::new(0),
        AtomicU32::new(0),
        core::sync::atomic::AtomicU64::new(0),
    )
}; 64];

/// How long an answer about the processor is kept, in counter ticks: under
/// half a millisecond on any processor that counts at 8 GHz or less.
#[cfg(not(test))]
const CPU_KEPT_TICKS: u64 = 1 << 21;

/// The processor the calling thread is on, or was a moment ago, `ask`
/// being how the kernel is asked.
///
/// A thread may be moved the instant after it asks, on Linux as here, so
/// every answer is "a moment ago" and RM treats it so. `sched_getcpu` is a
/// system call and RM asks three times a call; the answer is kept, by the
/// thread pointer, for [`CPU_KEPT_TICKS`]. A slot another thread wrote in
/// between is only asked again.
#[cfg(not(test))]
pub(crate) fn cpu(ask: fn() -> u32) -> u32 {
    use core::sync::atomic::Ordering;
    let pointer = thread_pointer();
    let index = ((pointer >> 6) ^ (pointer >> 12) ^ (pointer >> 20)) % CPUS.len();
    let Some((key, cpu, when)) = CPUS.get(index) else {
        return ask();
    };
    let now = ticks();
    if key.load(Ordering::Acquire) == pointer {
        let kept = cpu.load(Ordering::Acquire);
        let since = now.wrapping_sub(when.load(Ordering::Acquire));
        if since < CPU_KEPT_TICKS && key.load(Ordering::Acquire) == pointer {
            return kept;
        }
    }
    let asked = ask();
    key.store(0, Ordering::Release);
    cpu.store(asked, Ordering::Release);
    when.store(now, Ordering::Release);
    key.store(pointer, Ordering::Release);
    asked
}

/// See the other [`cpu`]: the host's tests ask every time.
#[cfg(test)]
pub(crate) fn cpu(ask: fn() -> u32) -> u32 {
    ask()
}

/// What [`slot`] answers if its index were ever out of range.
#[cfg(not(test))]
static SPARE: (core::sync::atomic::AtomicUsize, AtomicU32) =
    (core::sync::atomic::AtomicUsize::new(0), AtomicU32::new(0));

/// The slot `pointer`'s id is kept in. Thread control blocks are far more
/// than 64 bytes apart, and pages apart when each has a stack of its own.
#[cfg(not(test))]
fn slot(pointer: usize) -> &'static (core::sync::atomic::AtomicUsize, AtomicU32) {
    let index = ((pointer >> 6) ^ (pointer >> 12) ^ (pointer >> 20)) % TIDS.len();
    // The remainder is below the length, so the spare is never used.
    TIDS.get(index).unwrap_or(&SPARE)
}

/// The calling thread's id.
///
/// `gettid` is a system call, and this layer asks on every copy of a
/// client's memory, every `os_get_current_thread` and every lock taken
/// while the lock table watches: several a request. So the id is kept by
/// the thread pointer, in a table a thread reads and writes without a lock:
/// the id is stored before the pointer that vouches for it, and a reader
/// that finds its own pointer reads an id only its own thread, or a thread
/// that had the same control block before it, stored. That second case is
/// why a thread this layer starts stores its id before anything else
/// ([`tid_begin`]): a control block reused from a thread that ended never
/// answers with that thread's id. Two threads whose pointers share a slot
/// overwrite each other and ask the kernel again, which is only slower.
#[cfg(not(test))]
pub(crate) fn tid() -> u32 {
    use core::sync::atomic::Ordering;
    let pointer = thread_pointer();
    let (key, id) = slot(pointer);
    if key.load(Ordering::Acquire) == pointer {
        let kept = id.load(Ordering::Acquire);
        // Another thread may have taken the slot between the two loads:
        // only an id read with the pointer still in place is this thread's.
        if kept != 0 && key.load(Ordering::Acquire) == pointer {
            return kept;
        }
    }
    tid_begin()
}

/// Ask the kernel for the calling thread's id and keep it: what a thread
/// does first, and what [`tid`] does when it has none kept.
#[cfg(not(test))]
pub(crate) fn tid_begin() -> u32 {
    use core::sync::atomic::Ordering;
    let pointer = thread_pointer();
    let asked = tid_asked();
    let (key, id) = slot(pointer);
    key.store(0, Ordering::Release);
    id.store(asked, Ordering::Release);
    key.store(pointer, Ordering::Release);
    asked
}

/// The calling thread's id. The host's tests run on threads this layer did
/// not start, whose control blocks are reused, so they ask every time.
#[cfg(test)]
pub(crate) fn tid() -> u32 {
    tid_asked()
}

/// See [`tid`].
#[cfg(test)]
pub(crate) fn tid_begin() -> u32 {
    tid_asked()
}

/// Ferrix's native calls, by the instruction: what `ferrix-native`'s
/// wrappers trap through.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Kernel;

impl Syscall for Kernel {
    fn call(self, raw: Raw<'_>) -> usize {
        // SAFETY: a `Raw` names only memory it borrows, which lives across
        // the call (`ferrix_native::call`, where a `Raw` is built).
        unsafe { call6(raw.number(), raw.args()) }
    }
}
