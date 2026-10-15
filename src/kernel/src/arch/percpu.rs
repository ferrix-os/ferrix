//! A word of this processor's own record, read and changed so that finding
//! the record and touching the word are one step a migration cannot come
//! between: for the preemption count every kernel spin lock raises
//! (`sched::preempt`, docs/OPAQUE-KERNEL.md §9.8 2b).
//!
//! * **x86-64: one instruction.** A `mov` from, or an `xadd` without `lock`
//!   to, `gs:[offset]` (`x86_64::cpu::read_gs_at`, `gs_xadd`). The processor
//!   resolves `GS` and makes the access as one instruction, so an interrupt is
//!   taken before it or after it. No `lock`, because only this processor
//!   writes the word.
//!
//!   Every path that can reach the word runs on the kernel's `GS` base
//!   (condition 2 of the consultant's 2026-10-02 review). The ordinary
//!   interrupt and exception stub decides `swapgs` from the saved `CS`, which
//!   is right for everything the kernel can hold off: every ring-0 stretch on
//!   the program's `GS` (the `SYSCALL` trampoline's two ends, the way out to
//!   a program and its resume, the trap stub's own return, and
//!   `load_gs_index`'s pair of `swapgs`) runs with interrupts masked and holds
//!   no instruction that can fault. The paranoid entries -- the NMI, `#DB`,
//!   `#MC` and `#DF` -- decide from `GS_BASE` itself before any Rust runs, so
//!   one taken inside the `SYSCALL` stub before its `swapgs`, or between
//!   `load_gs_index`'s two, swaps to the kernel's base; and their handlers
//!   take no lock that raises the count in any case: the NMI and a kernel
//!   `#DB` count into atomics and return, `#MC` and `#DF` never return, and a
//!   ring-3 `#DB` moves to the task's stack and the ordinary path.
//! * **AArch64 and ARMv7-A: masked.** The record's address comes from
//!   `TPIDR_EL1` or `TPIDRPRW` and the access is a load and a store, so the
//!   three are made with every asynchronous exception masked, through
//!   [`super::Irq`]: `daifset #0xf` (debug, `SError`, `IRQ`, `FIQ`) and
//!   `cpsid aif` (asynchronous abort, `IRQ`, `FIQ`). No pseudo-NMI is
//!   configured on AArch64: the GIC's priority mask is only ever written fully
//!   open and no interrupt is given NMI priority, so nothing the mask leaves
//!   open runs code that could take a lock. The span touches only the record,
//!   mapped since boot, and raises no synchronous exception.
//!
//! On ARMv7-A the word is two `u32` halves, low first, each loaded and stored
//! whole, so that another processor reading either reads a number this one
//! wrote, never half of one.

#[cfg(not(target_arch = "x86_64"))]
use core::sync::atomic::{Ordering, compiler_fence};

#[cfg(not(target_arch = "x86_64"))]
use ferrix_sync::IrqControl;

/// Add `delta` to the word `offset` bytes into this processor's record and
/// return what it held, atomically against interrupts and migration.
///
/// # Safety
///
/// (SHARED) This processor's record must be installed, and `offset` must be
/// that of an aligned `u64` in it (two `u32` halves, low first, on a 32-bit
/// machine) that no other processor writes.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn this_cpu_add(offset: usize, delta: u64) -> u64 {
    // SAFETY: (SHARED) the caller's contract is `gs_xadd`'s.
    unsafe { super::x86_64::gs_xadd(offset, delta) }
}

/// The word `offset` bytes into this processor's record, read atomically
/// against interrupts and migration.
///
/// # Safety
///
/// (SHARED) As [`this_cpu_add`], for a read.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) unsafe fn this_cpu_read(offset: usize) -> u64 {
    // SAFETY: (SHARED) the caller's contract is `read_gs_at`'s.
    unsafe { super::x86_64::read_gs_at(offset) }
}

/// [`this_cpu_add`] on Arm: the record's address, the load and the store
/// with every exception that can take a lock masked.
///
/// # Safety
///
/// (SHARED) As on x86-64.
#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe fn this_cpu_add(offset: usize, delta: u64) -> u64 {
    let saved = <super::Irq as IrqControl>::disable();
    // The mask is an `asm!` the compiler may move memory accesses across; the
    // fences keep the access inside it.
    compiler_fence(Ordering::SeqCst);
    // SAFETY: (SHARED) the record is installed, by the caller's contract.
    let at = unsafe { super::cpu_local() } as usize + offset;
    // SAFETY: (SHARED) `at` is the aligned word the caller names, in a record
    // that lives for the life of the system.
    let word = unsafe { Word::at(at) };
    let old = word.load();
    word.store(old.wrapping_add(delta));
    compiler_fence(Ordering::SeqCst);
    <super::Irq as IrqControl>::restore(saved);
    old
}

/// [`this_cpu_read`] on Arm, masked as [`this_cpu_add`] is.
///
/// # Safety
///
/// (SHARED) As on x86-64.
#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe fn this_cpu_read(offset: usize) -> u64 {
    let saved = <super::Irq as IrqControl>::disable();
    compiler_fence(Ordering::SeqCst);
    // SAFETY: (SHARED) the record is installed, by the caller's contract.
    let at = unsafe { super::cpu_local() } as usize + offset;
    // SAFETY: (SHARED) as in `this_cpu_add`.
    let value = unsafe { Word::at(at) }.load();
    compiler_fence(Ordering::SeqCst);
    <super::Irq as IrqControl>::restore(saved);
    value
}

/// The word, as the record holds it: one `u64` on AArch64, two `u32` halves
/// on ARMv7-A.
#[cfg(not(target_arch = "x86_64"))]
struct Word {
    /// The whole word.
    #[cfg(target_pointer_width = "64")]
    whole: &'static core::sync::atomic::AtomicU64,
    /// Its halves, low first.
    #[cfg(not(target_pointer_width = "64"))]
    halves: &'static [core::sync::atomic::AtomicU32; 2],
}

#[cfg(not(target_arch = "x86_64"))]
impl Word {
    /// The word at `at`.
    ///
    /// # Safety
    ///
    /// (SHARED) `at` must be the address of an aligned word of a record that
    /// lives for the life of the system.
    unsafe fn at(at: usize) -> Self {
        // SAFETY: (SHARED) the caller's contract; an atomic has the layout of
        // the integer the record declares there.
        unsafe {
            Self {
                #[cfg(target_pointer_width = "64")]
                whole: &*(at as *const core::sync::atomic::AtomicU64),
                #[cfg(not(target_pointer_width = "64"))]
                halves: &*(at as *const [core::sync::atomic::AtomicU32; 2]),
            }
        }
    }

    /// Load it. `Relaxed`: only this processor writes it, under the mask.
    fn load(&self) -> u64 {
        #[cfg(target_pointer_width = "64")]
        {
            self.whole.load(Ordering::Relaxed)
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            let [low, high] = self.halves;
            u64::from(low.load(Ordering::Relaxed)) | (u64::from(high.load(Ordering::Relaxed)) << 32)
        }
    }

    /// Store it, each half whole on ARMv7-A.
    fn store(&self, value: u64) {
        #[cfg(target_pointer_width = "64")]
        {
            self.whole.store(value, Ordering::Relaxed);
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            let [low, high] = self.halves;
            low.store(value as u32, Ordering::Relaxed);
            high.store((value >> 32) as u32, Ordering::Relaxed);
        }
    }
}
