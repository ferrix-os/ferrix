//! Single `AArch64` instructions with no spelling in Rust.
//!
//! On the assembly allow-list as "CPU primitives" (`docs/ASSEMBLY.md`): a
//! barrier, a TLB maintenance operation and a hypervisor call are not things
//! Rust has syntax for.

use core::arch::asm;

/// Wait for an interrupt.
pub(crate) fn wfi() {
    // SAFETY: `wfi` is a hint. With interrupts masked it may still return at
    // any time, which is why every caller loops.
    unsafe {
        asm!("wfi", options(nomem, nostack, preserves_flags));
    }
}

/// Mask every interrupt on this CPU: debug, `SError`, `IRQ` and `FIQ`.
pub(crate) fn disable_interrupts() {
    // SAFETY: `daifset` only sets mask bits in `PSTATE`.
    unsafe {
        asm!("msr daifset, #0xf", options(nomem, nostack));
    }
}

/// Publish page table writes and invalidate the whole TLB.
///
/// The first barrier is the one that matters and the one that is easy to leave
/// out: the table walker is a separate observer of memory, and a descriptor
/// still sitting in a store buffer is a descriptor it cannot see. Without
/// `dsb ishst` the mapping is invalidated *before* it exists, and the fault
/// that follows points at the access rather than at the omission.
pub(crate) fn flush_tlb() {
    // SAFETY: barriers and TLB maintenance have no effect other than ordering
    // and invalidation.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
}

/// PSCI `SYSTEM_OFF`, in the 32-bit calling convention.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

/// Ask the platform to power the machine off.
///
/// Returns if there is no PSCI implementation to answer, which is why the
/// caller halts afterwards rather than assuming this diverges.
///
/// The call is made *after* the boot test's success marker has been printed, on
/// purpose: `hvc` on a machine with no EL2 raises an exception, and until
/// stage 3 installs a vector table there is nothing to take it. Ordering it
/// last means the worst case is an untidy shutdown rather than a lost result.
pub(crate) fn psci_system_off() {
    // SAFETY: `hvc` with x0 = SYSTEM_OFF either powers the machine off or
    // returns an error in x0. Both are fine; the caller halts either way.
    unsafe {
        asm!(
            "hvc #0",
            in("x0") PSCI_SYSTEM_OFF,
            lateout("x0") _,
            lateout("x1") _,
            lateout("x2") _,
            lateout("x3") _,
            options(nostack),
        );
    }
}

/// PSCI `CPU_ON`, in the 64-bit calling convention.
///
/// The 64-bit variant rather than the 32-bit one because two of its three
/// arguments are addresses: the entry point, and a context identifier that has
/// to survive the round trip into the new core's `x0`.
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// Ask the platform to start the core with affinity `mpidr` at `entry`.
///
/// Returns the PSCI status: zero is success, and the negative codes are
/// `INVALID_PARAMETERS` (-2), `DENIED` (-3), `ALREADY_ON` (-4) and
/// `ON_PENDING` (-5). Handed back as it came rather than turned into a
/// `Result` here, because which of them came back is what the caller reports.
///
/// # Safety
///
/// `entry` must be the physical address of code that can run with the MMU off
/// on a freshly reset core, and `mpidr` must name a core that is not already
/// running anything. Starting a core that is already up is `ALREADY_ON` and
/// harmless; pointing a core at an address that is not a trampoline is a core
/// executing whatever happens to be in memory, invisibly.
pub(crate) unsafe fn psci_cpu_on(mpidr: u64, entry: u64, context: u64) -> i64 {
    let status: i64;
    // SAFETY: the caller guarantees the arguments. `hvc` with x0 = CPU_ON
    // either starts the core and returns, or returns an error code; it has no
    // effect on this core other than the status in x0.
    unsafe {
        asm!(
            "hvc #0",
            inlateout("x0") PSCI_CPU_ON => status,
            in("x1") mpidr,
            in("x2") entry,
            in("x3") context,
            options(nostack),
        );
    }
    status
}

/// This core's affinity, as the MADT and PSCI both name it.
///
/// The reserved bits above the affinity fields are masked off: `MPIDR_EL1`
/// carries `MT`, `U` and `RES1` in bits the MADT's copy does not, and
/// comparing the raw registers would match nothing.
pub(crate) fn read_mpidr() -> u64 {
    let mpidr: u64;
    // SAFETY: reading `MPIDR_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack, preserves_flags));
    }
    mpidr & AFFINITY_MASK
}

/// Affinity levels 0 to 3 of `MPIDR_EL1`, and nothing else.
const AFFINITY_MASK: u64 = 0x0000_00FF_00FF_FFFF;

/// The memory attribute indirection register this core is using.
pub(crate) fn read_mair() -> u64 {
    let value: u64;
    // SAFETY: reading `MAIR_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, mair_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// The translation control register this core is using.
pub(crate) fn read_tcr() -> u64 {
    let value: u64;
    // SAFETY: reading `TCR_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, tcr_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Clean the data cache over a physical range, to the point of coherency.
///
/// Needed exactly once, and for a reason that is easy to miss: the boot CPU
/// writes the trampoline with its caches on, and the core it is writing it for
/// starts with its caches *off*. A line still dirty in this core's cache is
/// invisible to that one, which reads RAM directly — so it executes whatever
/// was in those bytes before, with the MMU off and no vector table.
///
/// # Safety
///
/// `at` must be a mapped, readable virtual address and `len` must stay inside
/// the same mapping. The instruction takes a virtual address even though what
/// it operates on is a physical line.
pub(crate) unsafe fn clean_dcache(at: u64, len: u64) {
    if len == 0 {
        return;
    }

    let cache_type: u64;
    // SAFETY: `CTR_EL0` is readable at EL1 and has no side effects.
    unsafe {
        asm!("mrs {}, ctr_el0", out(reg) cache_type, options(nomem, nostack, preserves_flags));
    }
    // `DminLine` is log2 of the line size in *words*, so the size in bytes is
    // four shifted by it.
    let line = 4u64 << ((cache_type >> 16) & 0xF);

    let mut address = at & !(line - 1);
    let end = at.saturating_add(len);
    while address < end {
        // SAFETY: `dc cvac` cleans one line by virtual address, which the
        // caller guarantees is mapped, and has no effect other than on the
        // cache.
        unsafe {
            asm!("dc cvac, {}", in(reg) address, options(nostack, preserves_flags));
        }
        address += line;
    }

    // SAFETY: barriers, ordering the maintenance above against the `CPU_ON`
    // that follows it.
    unsafe {
        asm!("dsb sy", "isb", options(nostack, preserves_flags));
    }
}

/// Install the exception vector table.
///
/// # Safety
///
/// `table` must be the address of a sixteen-entry vector table aligned to 2048
/// bytes, every entry of which is real code. Until this is called the CPU is
/// still using firmware's table, which stopped existing at
/// `exit_boot_services` — so the window between the hand-off and this call is
/// one where any fault is unrecoverable.
pub(crate) unsafe fn write_vbar(table: u64) {
    // SAFETY: the caller guarantees the table's contents and alignment. The
    // `isb` is what makes the write take effect before the next instruction is
    // fetched.
    unsafe {
        asm!(
            "msr vbar_el1, {}",
            "isb",
            in(reg) table,
            options(nostack, preserves_flags),
        );
    }
}

/// Unmask `IRQ` on this CPU, leaving `FIQ`, `SError` and debug masked.
///
/// Only `IRQ`: Ferrix routes nothing to `FIQ` — it is conventionally reserved
/// for a secure world the kernel does not own — and an `SError` is an
/// asynchronous abort that stays masked until there is something that could
/// act on one.
pub(crate) fn enable_interrupts() {
    // SAFETY: `daifclr` only clears mask bits in `PSTATE`. The vector table is
    // installed long before anything calls this.
    unsafe {
        asm!("msr daifclr, #0x2", options(nomem, nostack));
    }
}

/// The interrupt mask bits of `PSTATE`.
///
/// Read for the same reason x86-64 reads `RFLAGS`: a lock that masks
/// interrupts has to put back the state it found, not unconditionally unmask.
pub(crate) fn read_daif() -> u64 {
    let daif: u64;
    // SAFETY: reading `DAIF` has no side effects.
    unsafe {
        asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags));
    }
    daif
}

/// Restore the interrupt mask bits `read_daif` returned.
///
/// # Safety
///
/// `daif` must be a value a previous [`read_daif`] returned. Writing an
/// arbitrary value unmasks exceptions the caller may not be ready for — an
/// `SError` in particular, which Ferrix keeps masked until there is something
/// that could act on one.
pub(crate) unsafe fn write_daif(daif: u64) {
    // SAFETY: the caller guarantees the value came from `read_daif`.
    unsafe {
        asm!("msr daif, {}", in(reg) daif, options(nomem, nostack));
    }
}

/// `TCR_EL1.EPD0` — translations through `TTBR0_EL1` fault instead of walking.
const TCR_EPD0: u64 = 1 << 7;

/// Stop the CPU translating the lower half of the address space at all.
///
/// This is how `AArch64` drops the loader's identity map. Unlike x86-64, where
/// the identity map is a set of entries in the same table as everything else
/// and is dropped by clearing them, here it is a whole second translation
/// regime with its own base register — so the way to drop it is to switch the
/// regime off.
///
/// `TTBR0_EL1` is zeroed as well as disabled. With `EPD0` set the register is
/// not consulted, so this changes no behaviour; it is here so that a later
/// change that clears `EPD0` — stage 6, giving the lower half to a user
/// process — cannot accidentally resurrect the loader's tables.
///
/// # Safety
///
/// Nothing may still be executing or reading through the lower half. The
/// kernel runs entirely in the upper half from its first instruction, so this
/// holds once the boot stack and the hand-off are being reached through the
/// direct map, which they are.
pub(crate) unsafe fn disable_ttbr0() {
    // SAFETY: the caller guarantees nothing needs the lower half. The `isb`
    // makes both writes take effect before the next instruction is fetched;
    // without it the CPU is permitted to walk the old tables for a while yet.
    unsafe {
        asm!(
            "mrs {scratch}, tcr_el1",
            "orr {scratch}, {scratch}, {epd0}",
            "msr tcr_el1, {scratch}",
            "msr ttbr0_el1, xzr",
            "isb",
            scratch = out(reg) _,
            epd0 = in(reg) TCR_EPD0,
            options(nostack, preserves_flags),
        );
    }
}

/// The frequency of the architected counter, in hertz.
///
/// Firmware programs this register at reset and it is read-only thereafter, so
/// a zero here means firmware did not do its job rather than that the counter
/// is stopped.
pub(crate) fn read_cntfrq() -> u64 {
    let frequency: u64;
    // SAFETY: reading `CNTFRQ_EL0` has no side effects.
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) frequency, options(nomem, nostack, preserves_flags));
    }
    frequency
}

/// The virtual counter.
///
/// The `isb` is not optional: the counter read is otherwise free to be
/// satisfied out of order with respect to the instructions around it, and a
/// timing loop built on that measures something other than what it thinks.
pub(crate) fn read_cntvct() -> u64 {
    let count: u64;
    // SAFETY: a barrier and a system register read, neither of which changes
    // any state.
    unsafe {
        asm!(
            "isb",
            "mrs {}, cntvct_el0",
            out(reg) count,
            options(nostack, preserves_flags),
        );
    }
    count
}

/// Set the instant the virtual timer fires at.
pub(crate) fn write_cntv_cval(instant: u64) {
    // SAFETY: `CNTV_CVAL_EL0` is a comparator. Writing it changes when the
    // timer's output asserts and nothing else.
    unsafe {
        asm!("msr cntv_cval_el0, {}", in(reg) instant, options(nomem, nostack, preserves_flags));
    }
}

/// Enable or mask the virtual timer.
pub(crate) fn write_cntv_ctl(control: u64) {
    // SAFETY: `CNTV_CTL_EL0` holds the timer's enable and mask bits. The `isb`
    // makes the write take effect before the next instruction, so a caller
    // that disarms and then returns cannot take one more interrupt.
    unsafe {
        asm!(
            "msr cntv_ctl_el0, {}",
            "isb",
            in(reg) control,
            options(nomem, nostack, preserves_flags),
        );
    }
}
