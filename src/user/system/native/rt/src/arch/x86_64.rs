//! x86-64.
//!
//! The kernel's entry is `ferrix_syscall_stub` in
//! `src/kernel/src/arch/x86_64/syscall.rs`: `SYSCALL`, the number in RAX, the
//! arguments in RDI, RSI, RDX, R10, R8 and R9 — R10, not RCX, because the
//! instruction overwrites RCX with the return address and R11 with the flags —
//! and the result back in RAX. Every other register comes back as it went.

use core::arch::{asm, naked_asm};

use ferrix_linux_abi::nr::x86_64::{CLOCK_GETTIME, EXIT_GROUP, UNLINK};
use ferrix_native::Raw;

use crate::linux::CLOCK_MONOTONIC;

/// The numbers of the Linux calls `crate::linux` makes.
///
/// A number is this architecture's, so the table it comes from is named here
/// and nowhere else; `crate::linux` spells every call the same on all three.
pub(crate) mod nr {
    pub(crate) use ferrix_linux_abi::nr::x86_64::{
        ACCEPT4, BIND, CLOSE, FCNTL, GETSOCKOPT, LISTEN, READ, REBOOT, SOCKET, WRITE,
    };
}

/// This architecture's whole table, for a program that makes a call
/// `crate::linux` has no function for (`crate::linux::call`).
pub use ferrix_linux_abi::nr::x86_64 as numbers;

/// The process's first instruction.
///
/// `ferrix_enter_user` starts a program with `SYSRET` and the stack pointer a
/// multiple of 16. A function expects to be entered by a `call`, eight bytes
/// below that; so the stack is aligned and `ferrix_rt_start` is called, which
/// pushes the return address the ABI wants. RBP is zeroed so a frame-pointer
/// walk ends here. The bootstrap handle's register, RDI, is already the first
/// argument.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) extern "C" fn _start() -> ! {
    naked_asm!(
        "xor ebp, ebp",
        "and rsp, -16",
        "call ferrix_rt_start",
        "ud2",
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
/// x86-64's table still has `unlink` itself, where the architectures built on
/// the newer one have only `unlinkat` and must be given a directory to start
/// from. Which of the two spells it is the architecture's, so the choice is
/// made here rather than in a caller.
///
/// # Safety
///
/// `at` is the address of a NUL-terminated path this program owns, valid for
/// the whole of the call; the kernel only reads it.
pub(crate) unsafe fn unlink(at: usize) -> usize {
    // SAFETY: the caller's promise about `at`, forwarded unchanged.
    unsafe { linux(UNLINK, [at, 0, 0, 0, 0, 0]) }
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
    // The vector registers: `channel_write_read`, `object_wait_one` and
    // `port_wait` destroy every register the System V ABI makes caller-saved,
    // as a function call does (`ferrix_native_abi::nr`, the vector-state
    // contract; `docs/OPAQUE-KERNEL.md` §9.8, 3a). `clobber_abi("sysv64")`
    // declares exactly that, so the compiler keeps no live value in an `XMM`
    // or `YMM` register, an x87 register or a mask register across the trap;
    // the explicit operands override it for the registers that carry
    // arguments and results. It is declared for every call through this one
    // block rather than for the three in a block of their own: the
    // assembly's budget has no room for a second, and a call that keeps the
    // registers loses nothing but the compiler's choice to keep a value there.
    //
    // SAFETY: `raw` was built by `src/lib/proto/native`, which puts in a pointer
    // argument only the address of a slice `raw` borrows — shared if the
    // kernel reads it, exclusive if it writes — and `raw` outlives this trap.
    // Every native call either touches only that memory or adds mappings where
    // nothing is mapped, so no memory Rust believes it owns changes under it.
    // The instruction clobbers RCX and R11, declared, and no stack.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a0,
            inlateout("rsi") a1 => w0,
            inlateout("rdx") a2 => w1,
            inlateout("r10") a3 => w2,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            clobber_abi("sysv64"),
            options(nostack),
        );
    }
    (result, [w0, w1, w2])
}

/// The processor's time-stamp counter, which ring 3 may read: the TSC, the
/// counter the kernel's clock counts when it is not the HPET.
///
/// Fenced on both sides with `lfence`, so that the read is neither made
/// before the work before it has finished nor overtaken by the work after
/// it: `rdtsc` alone is not serializing, and a timed span read without the
/// fences comes out short by however much the processor overlapped
/// (`bench-ipc`, `docs/OPAQUE-KERNEL.md` §9.5 step 0). `rdtscp` would order
/// the read after earlier work too, but the gate's CPU model does not offer
/// it.
pub(crate) fn counter() -> Option<u64> {
    let (low, high): (u32, u32);
    // SAFETY: `lfence` and `rdtsc` touch no memory and no flags; `rdtsc`
    // writes edx:eax, and nothing clears CR4.TSD, so it does not fault in
    // ring 3.
    unsafe {
        asm!("lfence; rdtsc; lfence", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
    }
    Some(u64::from(high) << 32 | u64::from(low))
}

/// Complete every access before this before any after it: x86-64's devices
/// snoop, so a fence is all a device needs.
pub(crate) fn device_barrier() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// `exit_group(status)`.
pub(crate) fn exit(status: i32) -> ! {
    // SAFETY: `exit_group` takes no pointer and does not return: the process
    // ends in the kernel, with nothing of this one left to run.
    unsafe {
        asm!(
            "syscall",
            in("rax") EXIT_GROUP,
            in("rdi") status.cast_unsigned() as usize,
            options(noreturn, nostack),
        );
    }
}
