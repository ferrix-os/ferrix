//! MEASUREMENT ONLY (os07-prof; never lands): the registers `crate::prof`
//! stamps and keys with on ARMv7-A.

use core::arch::asm;

/// `PMCCNTR`, after an `isb`, as `board-bench.h`'s `bb_cycles` reads it.
#[inline(always)]
pub(crate) fn cycles() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading the cycle counter has no side effects; the
    // `isb` keeps it after the instructions before it, as the bench's own
    // read does. Only run where a window is open, which `start_cycle_counter`
    // made sure counts.
    unsafe {
        asm!(
            "isb",
            "mrc p15, 0, {}, c9, c13, 0",
            out(reg) value,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// `MPIDR`.
#[inline(always)]
pub(crate) fn mpidr() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading an identification register has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c0, 5", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `TTBR0`'s low word: the installed user root.
pub(crate) fn root() -> u32 {
    super::cpu::read_ttbr0() as u32
}

/// `PMCR`.
pub(crate) fn pmcr() -> u32 {
    super::cpu::read_pmcr()
}

/// Make this processor's cycle counter count: `PMCR.E` and
/// `PMCNTENSET.C`, leaving its value, its divider and the event counters
/// as `ipc-bench`'s own start left them.
pub(crate) fn start_cycle_counter() {
    // SAFETY: (SYSREG) the PMU is not used by the kernel; enabling the
    // counters changes nothing but what they count. Only `E` is set in
    // `PMCR` (no reset bits), and only `C` in `PMCNTENSET`.
    unsafe {
        asm!(
            "mrc p15, 0, {scratch}, c9, c12, 0",
            "orr {scratch}, {scratch}, #1",
            "mcr p15, 0, {scratch}, c9, c12, 0",
            "mcr p15, 0, {c}, c9, c12, 1",
            "isb",
            scratch = out(reg) _,
            c = in(reg) 1_u32 << 31,
            options(nostack, preserves_flags),
        );
    }
}
