//! AArch64.
//!
//! The kernel's entry is `system_call` in `src/kernel/src/arch/aarch64/trap.rs`:
//! `svc #0`, the number in X8, the arguments in X0 to X5, and the result back
//! in X0. Every other register comes back as it went.

use core::arch::{asm, naked_asm};

use ferrix_linux_abi::nr::aarch64::{CLOCK_GETTIME, EXIT_GROUP, UNLINKAT};

use crate::linux::CLOCK_MONOTONIC;

/// The numbers of the Linux calls `crate::linux` makes.
///
/// A number is this architecture's, so the table it comes from is named here
/// and nowhere else; `crate::linux` spells every call the same on all three.
pub(crate) mod nr {
    pub(crate) use ferrix_linux_abi::nr::aarch64::{
        ACCEPT4, BIND, CLOSE, FCNTL, GETSOCKOPT, LISTEN, READ, REBOOT, SOCKET, WRITE,
    };
}

/// This architecture's whole table, for a program that makes a call
/// `crate::linux` has no function for (`crate::linux::call`).
pub use ferrix_linux_abi::nr::aarch64 as numbers;

/// `AT_FDCWD`: start from the current directory, which an absolute path then
/// ignores.
const AT_FDCWD: usize = (-100_isize) as usize;
use ferrix_native::Raw;

/// The process's first instruction.
///
/// `ferrix_enter_user` starts a program with `eret`, which leaves the frame
/// pointer and link register as whatever it put in them rather than a caller's
/// frame. They are zeroed so a frame-pointer walk ends here, the stack pointer
/// is brought to a multiple of 16 in case a starter did not, and
/// `ferrix_rt_start` is called. The bootstrap handle's register, X0, is already
/// the first argument.
// SAFETY: (ENTRY) the body is the whole function and the process's first
// instructions: nothing calls it, and it never returns.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) extern "C" fn _start() -> ! {
    naked_asm!(
        "mov x29, #0",
        "mov x30, #0",
        "mov x9, sp",
        "bic x9, x9, #15",
        "mov sp, x9",
        "bl ferrix_rt_start",
        "udf #0",
    )
}

/// Make the call `raw` describes.
pub(crate) fn call(raw: &Raw<'_>) -> usize {
    trap(raw.number(), raw.args())
}

/// Make the Linux call `number` with `args`.
///
/// The same instruction and the same registers as [`call`]: the kernel picks
/// the ABI by the number's range and by nothing else (`dispatch` in
/// `src/kernel/src/syscall/mod.rs`), so a native program issues a Linux call
/// exactly as it issues one of its own. `exit` below has done this since this
/// file was written; `docs/CLIPBOARD.md` §5 is what made it worth naming.
///
/// # Safety
///
/// The caller is answerable for every pointer in `args`: each must be valid
/// for whatever the named call does with it, for the whole of the call. The
/// kernel checks that a pointer is the caller's before it touches it, so a
/// wrong one is refused rather than followed -- but a pointer to the wrong
/// *owned* memory is memory this program may see changed under it.
pub(crate) unsafe fn linux(number: usize, args: [usize; 6]) -> usize {
    trap(number, args)
}

/// Take the NUL-terminated path at `at` out of the filesystem.
///
/// This table has no `unlink` at all, only `unlinkat`, which is the same call
/// with a directory in front of it. Which of the two spells it is the
/// architecture's, so the choice is made here rather than in a caller.
///
/// # Safety
///
/// `at` is the address of a NUL-terminated path this program owns, valid for
/// the whole of the call; the kernel only reads it.
pub(crate) unsafe fn unlink(at: usize) -> usize {
    // SAFETY: the caller's promise about `at`, forwarded unchanged.
    unsafe { linux(UNLINKAT, [AT_FDCWD, at, 0, 0, 0, 0]) }
}

/// Read `CLOCK_MONOTONIC` into `nanos`, and answer the call's own result.
///
/// A `timespec` is two words of the architecture's width, so its size is the
/// architecture's and the caller is given nanoseconds instead. `nanos` is left
/// alone unless the call succeeded, since the kernel wrote nothing then.
pub(crate) fn monotonic_nanos(nanos: &mut u64) -> usize {
    let mut when = [0_i64; 2];
    let at = when.as_mut_ptr().addr();
    // SAFETY: `when` is this frame's own, two 64-bit words as this
    // architecture's `timespec` is, and the kernel only writes that much.
    let result = unsafe { linux(CLOCK_GETTIME, [CLOCK_MONOTONIC, at, 0, 0, 0, 0]) };
    if result == 0 {
        let [seconds, rest] = when;
        *nanos = seconds
            .unsigned_abs()
            .saturating_mul(1_000_000_000)
            .saturating_add(rest.unsigned_abs());
    }
    result
}

/// The trap itself: a number, six argument registers, and the result.
///
/// One block for both ABIs, because it is one instruction and one register
/// assignment -- which is the whole of what `asm!` is here, and what
/// `tools/common/data/asm-allowlist.json` admits.
fn trap(number: usize, args: [usize; 6]) -> usize {
    trap_words(number, args).0
}

/// [`trap`], and the second, third and fourth argument registers as the call
/// left them: where `channel_write_read` hands back the words of the message
/// it received. Every other call leaves them as they went in.
pub(crate) fn trap_words(number: usize, args: [usize; 6]) -> (usize, [usize; 3]) {
    let [a0, a1, a2, a3, a4, a5] = args;
    let result;
    let (w0, w1, w2);
    // SAFETY: `raw` was built by `src/lib/proto/native`, which puts in a pointer
    // argument only the address of a slice `raw` borrows — shared if the
    // kernel reads it, exclusive if it writes — and `raw` outlives this trap.
    // Every native call either touches only that memory or adds mappings where
    // nothing is mapped, so no memory Rust believes it owns changes under it.
    // The kernel preserves every register but X0, and uses no user stack.
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") a0 => result,
            inlateout("x1") a1 => w0,
            inlateout("x2") a2 => w1,
            inlateout("x3") a3 => w2,
            in("x4") a4,
            in("x5") a5,
            options(nostack),
        );
    }
    (result, [w0, w1, w2])
}

/// `dsb sy`: complete every access before this, to memory of any type and
/// to device registers, before any after it, as every observer sees them --
/// a device's DMA included, which `dmb ish` does not reach.
pub(crate) fn device_barrier() {
    // SAFETY: a barrier: no memory or register changes.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) }
}

/// The processor's free-running counter: the virtual counter, `CNTVCT_EL0`,
/// the one the kernel's clock counts. The kernel lets EL0 read it on every
/// processor (`CNTKCTL_EL1.EL0VCTEN`, which Linux sets for its vDSO too).
/// No `isb` before it, unlike the kernel's read: the read may be taken a few
/// instructions early, which does not matter to a driver timing a device in
/// tens of microseconds, and the file's assembly budget is spent.
pub(crate) fn counter() -> Option<u64> {
    let count: u64;
    // SAFETY: a read of a system register the kernel lets EL0 read; no
    // memory or other register changes.
    unsafe {
        asm!(
            "mrs {}, cntvct_el0",
            out(reg) count,
            options(nomem, nostack, preserves_flags),
        );
    }
    Some(count)
}

/// `exit_group(status)`.
pub(crate) fn exit(status: i32) -> ! {
    // SAFETY: `exit_group` takes no pointer and does not return: the process
    // ends in the kernel, with nothing of this one left to run.
    unsafe {
        asm!(
            "svc #0",
            in("x8") EXIT_GROUP,
            in("x0") status.cast_unsigned() as usize,
            options(noreturn, nostack),
        );
    }
}
