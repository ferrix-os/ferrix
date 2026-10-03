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

/// Wake up to `count` threads sleeping on `word`.
pub(crate) fn wake(word: &AtomicU32, count: u32) {
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

/// The calling thread's id.
pub(crate) fn tid() -> u32 {
    // SAFETY: `gettid` takes no arguments and cannot fail.
    let result = unsafe { call6(SYS_GETTID, [0; 6]) };
    u32::try_from(result).unwrap_or(0)
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
