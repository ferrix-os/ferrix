//! The lent firmware framebuffer's write-combining, x86-64's side
//! (`docs/DISPLAY.md` §8, under `ferrix.efifb`).
//!
//! Three things the rest of the kernel asks of the processor: one entry of
//! its page attribute table read back, the memory type its MTRRs give a
//! physical range, and a store fence that empties its write-combining
//! buffers. Every register here is read, never written: the PAT is
//! programmed at each processor's bring-up (`cpu::program_pat`) and the
//! MTRRs are firmware's.
//!
//! The MSR numbers are Linux's (`arch/x86/include/asm/msr-index.h`:
//! `MSR_MTRRcap` 0xFE, `MSR_MTRRdefType` 0x2FF, `MSR_IA32_CR_PAT` 0x277)
//! and QEMU's (`target/i386/cpu.h`: `MSR_MTRRphysBase(n)` 0x200 + 2n,
//! `MSR_MTRRphysMask(n)` 0x201 + 2n); the type encodings are Linux's
//! `uapi/asm/mtrr.h` (`MTRR_TYPE_UNCACHABLE` 0, `WRCOMB` 1, `WRTHROUGH` 4,
//! `WRPROT` 5, `WRBACK` 6), which the PAT's entries share.

use core::arch::asm;
use core::arch::x86_64::__cpuid;

use super::cpu;

/// `IA32_MTRRCAP`: the variable ranges' count in bits 7:0.
const MTRR_CAP: u32 = 0xFE;
/// `IA32_MTRR_DEF_TYPE`: the default type in bits 7:0, enable in bit 11.
const MTRR_DEF_TYPE: u32 = 0x2FF;
/// `IA32_MTRR_PHYSBASE0`; range `n`'s base is at `+ 2n`, its mask one after.
const MTRR_PHYS_BASE0: u32 = 0x200;
/// The variable ranges read: processors have 8 to 10, and one with more
/// than this gets no answer rather than a partial one.
const MAX_VARIABLE: usize = 32;

/// This processor's page attribute table, as it reads now: a byte a
/// memory type for each of the eight entries a leaf may select.
pub(crate) fn pat() -> Option<u64> {
    // SAFETY: (SYSREG) every processor this kernel runs on has the PAT: its
    // bring-up programmed it (`cpu::program_pat`), and stage 9 stops the boot
    // where one did not.
    Some(unsafe { cpu::read_msr(cpu::IA32_PAT) })
}

/// The memory type this processor's MTRRs give every page of
/// `phys..phys + len`, when it is one type for all of them
/// ([`ferrix_paging::mtrr::range_type`]); `None` when the processor has no
/// MTRRs, more than [`MAX_VARIABLE`] variable ranges, or the range has no
/// one type.
///
/// For a boot line only: a page the PAT maps write-combining is
/// write-combining under every MTRR type, so nothing is decided by this.
pub(crate) fn mtrr_type(phys: u64, len: u64) -> Option<u8> {
    // CPUID.(EAX=1):EDX.MTRR is bit 12.
    if __cpuid(1).edx & (1 << 12) == 0 {
        return None;
    }
    // SAFETY: (SYSREG) CPUID reported the MTRRs, so the register exists.
    let cap = unsafe { cpu::read_msr(MTRR_CAP) };
    // SAFETY: (SYSREG) as above.
    let default = unsafe { cpu::read_msr(MTRR_DEF_TYPE) };
    let count = usize::try_from(cap & 0xFF).ok()?;
    if count > MAX_VARIABLE {
        return None;
    }
    let mut ranges = [(0_u64, 0_u64); MAX_VARIABLE];
    for (range, slot) in (0_u32..).zip(ranges.iter_mut().take(count)) {
        let at = MTRR_PHYS_BASE0 + 2 * range;
        // SAFETY: (SYSREG) `range` is below `MTRRcap.VCNT`, so its base exists.
        let base = unsafe { cpu::read_msr(at) };
        // SAFETY: (SYSREG) and its mask.
        let mask = unsafe { cpu::read_msr(at + 1) };
        *slot = (base, mask);
    }
    ferrix_paging::mtrr::range_type(default, ranges.get(..count)?, phys, len)
}

/// Make every write-combining store this processor has made globally
/// visible before any store after: `sfence`. What the panic screen ends
/// its drawing with on a framebuffer mapped write-combining, whose stores
/// may otherwise wait in a fill buffer.
pub(crate) fn drain_write_combining() {
    // SAFETY: (SYSREG) a fence: it changes no register or memory. Not `nomem`, so the
    // compiler keeps the drawing's stores before it.
    unsafe { asm!("sfence", options(nostack, preserves_flags)) };
}
