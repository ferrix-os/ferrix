//! The `AArch64` end of the loader.
//!
//! Harder than its x86-64 counterpart for one reason: firmware hands over with
//! the MMU already **on**, and the architecture does not permit reprogramming
//! `TCR_EL1` underneath a live translation regime. So the sequence is to clean
//! what we wrote out of the caches, turn the MMU off, install our own regime,
//! and turn it back on — which works only because UEFI identity maps the
//! loader, so the program counter means the same thing either way.
//!
//! This is the same thing Linux's EFI stub does, and for the same reason.

use core::arch::asm;

use ferrix_bootinfo::Arch;
use ferrix_paging::aarch64::MAIR_EL1;

use super::Handoff;

/// The architecture the kernel is told it booted on.
pub(crate) const ARCH: Arch = Arch::AArch64;

/// The `e_machine` a kernel for this architecture must carry.
pub(crate) const ELF_MACHINE: u16 = ferrix_elf::EM_AARCH64;

/// The ELF class a kernel for this architecture must be.
pub(crate) const ELF_CLASS: ferrix_elf::Class = ferrix_elf::Class::Elf64;

/// `TCR_EL1` with 48-bit addressing and a 4 KiB granule in both halves.
///
/// Assembled here rather than inline so each field can be named. The one that
/// catches people is `TG1`: the granule encoding for the upper half is *not*
/// the same as for the lower one — `0b10` is 4 KiB for `TG1` and `0b00` is
/// 4 KiB for `TG0`.
const fn tcr_el1(intermediate_physical_size: u64) -> u64 {
    const T0SZ: u64 = 64 - 48;
    const T1SZ: u64 = 64 - 48;
    const RGN_WRITE_BACK: u64 = 0b01;
    const SHAREABILITY_INNER: u64 = 0b11;
    const TG0_4K: u64 = 0b00;
    const TG1_4K: u64 = 0b10;

    T0SZ | (RGN_WRITE_BACK << 8)
        | (RGN_WRITE_BACK << 10)
        | (SHAREABILITY_INNER << 12)
        | (TG0_4K << 14)
        | (T1SZ << 16)
        | (RGN_WRITE_BACK << 24)
        | (RGN_WRITE_BACK << 26)
        | (SHAREABILITY_INNER << 28)
        | (TG1_4K << 30)
        | (intermediate_physical_size << 32)
}

/// `SCTLR_EL1.M`, the MMU enable.
const SCTLR_MMU: u64 = 1 << 0;
/// `SCTLR_EL1.C`, the data cache enable.
const SCTLR_DCACHE: u64 = 1 << 2;
/// `SCTLR_EL1.I`, the instruction cache enable.
const SCTLR_ICACHE: u64 = 1 << 12;
/// `SCTLR_EL1.WXN`: every writable page execute-never. Cleared, because the
/// identity map is both, and the instruction after the MMU comes back on is
/// fetched through it. Firmware may leave it set, and then that instruction
/// faults with no vectors installed, which is a silent hang. The kernel's own
/// W^X sweep is what enforces the rule after.
const SCTLR_WXN: u64 = 1 << 19;

/// Check the loader is somewhere it can install a translation regime.
///
/// Unlike x86-64 there are no feature bits to enable — the execute-never bits
/// are always live and the memory attributes are programmed as part of the
/// switch. What there is instead is an assumption to verify: `enter_kernel`
/// writes `TTBR0_EL1`, `TTBR1_EL1` and `SCTLR_EL1`, which only means anything
/// at EL1. Firmware on QEMU's `virt` machine hands off there. If a platform
/// ever hands off at EL2 this has to grow an `eret` down into EL1, and finding
/// that out from a message beats finding it out from a fault.
pub(crate) fn prepare_cpu() -> Result<(), &'static str> {
    match current_exception_level() {
        1 => Ok(()),
        2 => Err("firmware handed off at EL2; the loader only supports EL1"),
        _ => Err("firmware handed off at an unexpected exception level"),
    }
}

/// Clean and invalidate the data cache over a physical range.
///
/// Necessary because the loader writes page tables with the caches on and then
/// turns them off. Anything still sitting dirty in a cache at that moment is
/// invisible afterwards, and a page table read as whatever was in RAM before it
/// translates to somewhere arbitrary.
pub(crate) fn clean_dcache(start: u64, len: u64) {
    if len == 0 {
        return;
    }

    let cache_type: u64;
    // SAFETY: CTR_EL0 is readable at EL1 and has no side effects.
    unsafe {
        asm!("mrs {}, ctr_el0", out(reg) cache_type, options(nomem, nostack, preserves_flags));
    }
    // DminLine is log2 of the line size in *words*, so the size in bytes is
    // four shifted by it.
    let line = 4u64 << ((cache_type >> 16) & 0xF);

    let mut at = start & !(line - 1);
    let end = start.saturating_add(len);
    while at < end {
        // SAFETY: `dc civac` cleans and invalidates one line by virtual
        // address; under UEFI's identity map this address is mapped, and the
        // instruction has no effect other than on the cache.
        unsafe {
            asm!("dc civac, {}", in(reg) at, options(nostack, preserves_flags));
        }
        at += line;
    }

    // SAFETY: a barrier, ordering the maintenance above against everything
    // after it.
    unsafe {
        asm!("dsb sy", "isb", options(nostack, preserves_flags));
    }
}

/// The largest physical address size this CPU implements, as a `TCR_EL1.IPS`
/// encoding.
fn intermediate_physical_size() -> u64 {
    let features: u64;
    // SAFETY: ID_AA64MMFR0_EL1 is readable at EL1 and has no side effects.
    unsafe {
        asm!(
            "mrs {}, id_aa64mmfr0_el1",
            out(reg) features,
            options(nomem, nostack, preserves_flags),
        );
    }
    // PARange is the low four bits. Encoding 5 is 48 bits, which is as much as
    // a four-level 4 KiB walk can address; 6 (52 bits) needs FEAT_LPA and a
    // different descriptor layout, so cap rather than believe it.
    (features & 0xF).min(5)
}

/// No random number instruction: `RNDR` is `FEAT_RNG`, an ARMv8.5 feature
/// the reference Cortex-A72 does not have, so KASLR falls back from
/// firmware's `EFI_RNG_PROTOCOL` straight to [`counter`].
pub(crate) const fn cpu_random() -> Option<u64> {
    None
}

/// The virtual count of the generic timer, which every `ARMv8` core has: the
/// loader's last resort for KASLR, and a poor one, since how long firmware
/// took to get here can be guessed.
pub(crate) fn counter() -> u64 {
    let count: u64;
    // SAFETY: CNTVCT_EL0 is readable at EL1 and has no side effects.
    unsafe {
        asm!("mrs {}, cntvct_el0", out(reg) count, options(nomem, nostack, preserves_flags));
    }
    count
}

/// The exception level the loader is running at.
fn current_exception_level() -> u64 {
    let level: u64;
    // SAFETY: CurrentEL is readable at every exception level.
    unsafe {
        asm!("mrs {}, CurrentEL", out(reg) level, options(nomem, nostack, preserves_flags));
    }
    (level >> 2) & 0b11
}

/// The switch's instructions, for copying to a trampoline page: none here,
/// because a 64-bit layout never plans one.
pub(crate) const fn switch_code() -> Option<&'static [u8]> {
    None
}

/// Install the loader's translation regime and jump to the kernel.
///
/// # Safety
///
/// Boot services must already have been exited; the loader must be running at
/// EL1 and identity mapped, because the middle of this sequence executes with
/// the MMU off; and everything `handoff` points at must already have been
/// cleaned out of the data cache with [`clean_dcache`].
pub(crate) unsafe fn enter_kernel(handoff: Handoff) -> ! {
    let tcr = tcr_el1(intermediate_physical_size());

    // Every operand is bound to a named register rather than left to the
    // allocator. `options(noreturn)` forbids outputs, so there is no way to ask
    // for a scratch register — binding one as an input nobody reads is how you
    // get one the compiler agrees is yours.
    //
    // SAFETY: the caller's contract is exactly the set of conditions that make
    // this sequence sound. Interrupts are masked first, because firmware's
    // handlers stopped existing at `exit_boot_services` and its timer has not.
    unsafe {
        asm!(
            "msr daifset, #0xf",
            "dsb sy",
            "isb",

            // Turn the MMU and both caches off. From here until the MMU is back
            // on the program counter is a physical address, which is fine only
            // because UEFI identity mapped us.
            "mrs x7, sctlr_el1",
            "bic x7, x7, #{mmu}",
            "bic x7, x7, #{dcache}",
            "bic x7, x7, #{icache}",
            "msr sctlr_el1, x7",
            "isb",

            // Install our own translation regime.
            "msr mair_el1, x1",
            "msr tcr_el1, x2",
            "msr ttbr0_el1, x3",
            "msr ttbr1_el1, x4",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",

            // And back on.
            "mrs x7, sctlr_el1",
            "orr x7, x7, #{mmu}",
            "orr x7, x7, #{dcache}",
            "orr x7, x7, #{icache}",
            "bic x7, x7, #{wxn}",
            "msr sctlr_el1, x7",
            "isb",

            "mov sp, x5",
            // Null frame and link registers terminate any backtrace the kernel
            // walks, rather than letting it wander into the loader's dead frames.
            "mov x29, xzr",
            "mov x30, xzr",
            "br x6",

            mmu = const SCTLR_MMU,
            dcache = const SCTLR_DCACHE,
            icache = const SCTLR_ICACHE,
            wxn = const SCTLR_WXN,

            // The AAPCS64 argument register: the kernel entry takes the boot
            // info pointer as its only argument.
            in("x0") handoff.boot_info,
            in("x1") MAIR_EL1,
            in("x2") tcr,
            in("x3") handoff.identity_table,
            in("x4") handoff.root_table,
            in("x5") handoff.stack_top,
            in("x6") handoff.entry,
            // Claimed so the scratch above cannot collide with anything live.
            in("x7") 0u64,
            options(noreturn),
        );
    }
}

/// The control registers the breadcrumbs line prints: x86-64's alone, so
/// none here.
pub(crate) const fn control_registers() -> Option<(u64, u64, u64)> {
    None
}
