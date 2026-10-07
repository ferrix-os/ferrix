//! ARMv7-A, in ARM state.
//!
//! The kernel's entry is `system_call` in `src/kernel/src/arch/armv7a/trap.rs`:
//! `svc #0`, the EABI's convention — the number in R7, the arguments in R0 to
//! R5, and the result back in R0. Every other register comes back as it went.
//! R7 is free to carry the number because the targets are built in ARM state,
//! where the frame pointer is R11; in Thumb state it would be R7.

use core::arch::{asm, naked_asm};

use ferrix_linux_abi::nr::arm::{CLOCK_GETTIME, EXIT_GROUP, UNLINKAT};

use crate::linux::CLOCK_MONOTONIC;

/// The numbers of the Linux calls `crate::linux` makes.
///
/// A number is this architecture's, so the table it comes from is named here
/// and nowhere else; `crate::linux` spells every call the same on all three.
pub(crate) mod nr {
    pub(crate) use ferrix_linux_abi::nr::arm::{
        ACCEPT4, BIND, CLOSE, FCNTL, GETSOCKOPT, LISTEN, READ, REBOOT, SETSOCKOPT, SOCKET, WRITE,
    };
}

/// This architecture's whole table, for a program that makes a call
/// `crate::linux` has no function for (`crate::linux::call`).
pub use ferrix_linux_abi::nr::arm as numbers;

/// `AT_FDCWD`: start from the current directory, which an absolute path then
/// ignores.
const AT_FDCWD: usize = (-100_isize) as usize;
use ferrix_native::Raw;

/// The process's first instruction.
///
/// `ferrix_enter_user` starts a program with `rfeia`, which leaves the frame
/// pointer and link register as whatever it put in them rather than a caller's
/// frame. They are zeroed so a frame-pointer walk ends here, the stack pointer
/// is brought to the multiple of 8 the AAPCS requires at a call, and
/// `ferrix_rt_start` is called. The bootstrap handle's register, R0, is
/// already the first argument.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) extern "C" fn _start() -> ! {
    naked_asm!(
        "mov r11, #0",
        "mov lr, #0",
        "bic sp, sp, #7",
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
/// This table has both `unlink` and `unlinkat`; `unlinkat` is taken because it
/// is the one the newer tables kept, so the two architectures that have it
/// answer alike. Which of the two spells it is the architecture's, so the
/// choice is made here rather than in a caller.
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
/// A `timespec` is two words of the architecture's width, and this
/// architecture's word is 32 bits: `CLOCK_GETTIME` is the kernel's
/// `TimeWidth::Native`, so it writes two `i32`s here where the 64-bit
/// architectures get two `i64`s. Asking for eight bytes more than the kernel
/// writes would be reading this frame's own uninitialised memory, and asking
/// for eight fewer would be letting it write past the array, which is why the
/// width lives with the architecture rather than with the caller.
///
/// Seconds since boot, which is what this clock counts, do not come near a
/// 32-bit overflow in a boot.
pub(crate) fn monotonic_nanos(nanos: &mut u64) -> usize {
    let mut when = [0_i32; 2];
    let at = when.as_mut_ptr().addr();
    // SAFETY: `when` is this frame's own, two 32-bit words as this
    // architecture's `timespec` is, and the kernel only writes that much.
    let result = unsafe { linux(CLOCK_GETTIME, [CLOCK_MONOTONIC, at, 0, 0, 0, 0]) };
    if result == 0 {
        let [seconds, rest] = when;
        *nanos = u64::from(seconds.unsigned_abs())
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::from(rest.unsigned_abs()));
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
    // The kernel preserves every register but R0, and uses no user stack.
    unsafe {
        asm!(
            "svc #0",
            in("r7") number,
            inlateout("r0") a0 => result,
            inlateout("r1") a1 => w0,
            inlateout("r2") a2 => w1,
            inlateout("r3") a3 => w2,
            in("r4") a4,
            in("r5") a5,
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

/// The processor's free-running counter: the virtual counter, `CNTVCT`,
/// the one the kernel's clock counts. The kernel lets PL0 read it on every
/// processor (`CNTKCTL.PL0VCTEN`, as Linux sets it on ARM). The `isb` before
/// it for AArch64's reason: no out-of-order core (Cortex-A15, A17) may take
/// the read before the instructions ahead of it retire.
pub(crate) fn counter() -> Option<u64> {
    let (low, high): (u32, u32);
    // SAFETY: a barrier and a 64-bit read of a coprocessor register the
    // kernel lets PL0 read; no memory or other register changes.
    unsafe {
        asm!(
            "isb",
            "mrrc p15, 1, {low}, {high}, c14",
            low = out(reg) low,
            high = out(reg) high,
            options(nostack, preserves_flags),
        );
    }
    Some(u64::from(high) << 32 | u64::from(low))
}

/// `exit_group(status)`.
pub(crate) fn exit(status: i32) -> ! {
    // SAFETY: `exit_group` takes no pointer and does not return: the process
    // ends in the kernel, with nothing of this one left to run.
    unsafe {
        asm!(
            "svc #0",
            in("r7") EXIT_GROUP,
            in("r0") status.cast_unsigned() as usize,
            options(noreturn, nostack),
        );
    }
}
