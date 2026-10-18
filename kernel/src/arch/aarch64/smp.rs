//! Starting the other processors on `AArch64`.
//!
//! # Why there is assembly here
//!
//! PSCI `CPU_ON` starts a processor **with the MMU off**, at a physical
//! address, with caches off and every interrupt masked. Everything a Rust
//! function needs — a stack at a virtual address, a translation regime, a
//! program counter that means what the compiler thought it meant — has to be
//! built before the first Rust instruction can run, and the middle of building
//! it executes with the program counter as a physical address. That is the same
//! argument the loader makes for its own last twenty instructions, and it is
//! the only reason there is assembly here.
//!
//! The sequence is shorter than the loader's for one reason: the loader has to
//! turn a *live* translation regime off before installing its own, because
//! `TCR_EL1` cannot be reprogrammed underneath one. Here there is none to turn
//! off.
//!
//! # Why the regime is copied rather than recomputed
//!
//! `MAIR_EL1`, `TCR_EL1` and both translation table base registers are read off
//! the processor that is doing the starting rather than assembled from
//! constants. A second set of constants that has to agree with the loader's is
//! a second set of constants that can stop agreeing with the loader's, and the
//! failure mode is a processor that faults on its first instruction with no
//! console to say so.

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_acpi::MadtEntry;
use ferrix_bootinfo::{BootView, PAGE_SIZE};

use super::cpu;
use super::gic;
use super::trap;
use crate::smp::{Ipi, Target};

core::arch::global_asm!(
    r#"
.section .text
.globl ferrix_ap_start
.globl ferrix_ap_params
.globl ferrix_ap_end
.align 3
ferrix_ap_start:
    adr x9, ferrix_ap_params
    ldp x1, x2, [x9, #0]
    ldp x3, x4, [x9, #16]
    ldp x5, x6, [x9, #32]
    msr mair_el1, x1
    msr tcr_el1, x2
    msr ttbr0_el1, x3
    msr ttbr1_el1, x4
    isb
    tlbi vmalle1
    dsb nsh
    isb
    mrs x7, sctlr_el1
    orr x7, x7, #1
    orr x7, x7, #4
    orr x7, x7, #4096
    msr sctlr_el1, x7
    isb
    mov sp, x5
    mov x29, xzr
    mov x30, xzr
    br x6
.align 3
ferrix_ap_params:
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .quad 0
ferrix_ap_end:
"#
);

unsafe extern "C" {
    /// First instruction, and the physical address PSCI is given.
    static ferrix_ap_start: u8;
    /// Six quadwords: `MAIR_EL1`, `TCR_EL1`, `TTBR0_EL1`, `TTBR1_EL1`, the
    /// stack pointer and the entry point.
    static ferrix_ap_params: u8;
    /// One past the last byte.
    static ferrix_ap_end: u8;
}

/// Offset of `symbol` within the trampoline.
fn offset_of(symbol: &u8) -> usize {
    (symbol as *const u8 as usize).saturating_sub((&raw const ferrix_ap_start) as usize)
}

/// Where the trampoline was installed, physically.
static TRAMPOLINE: AtomicU64 = AtomicU64::new(0);

/// PSCI `CPU_ON`, in the 64-bit calling convention.
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// Tell `found` about every processor firmware describes as startable.
pub(crate) fn enumerate_cpus(view: &BootView<'_>, mut found: impl FnMut(u64)) {
    let Ok(firmware) = crate::acpi::Firmware::open(view) else {
        return;
    };
    let Ok(madt) = firmware.acpi().madt() else {
        return;
    };

    for entry in madt.entries() {
        if let MadtEntry::Gicc(cpu) = entry
            && cpu.is_enabled()
        {
            found(cpu.mpidr);
        }
    }
}

/// Copy the trampoline into the page reserved for it and fill in its
/// parameters.
///
/// # Safety
///
/// Must be called while the loader's identity map is still live, and only from
/// the CPU doing the starting.
unsafe fn install_trampoline() -> Result<u64, &'static str> {
    let phys = crate::mm::startup_page().ok_or("no page was reserved to start a processor on")?;
    let len = offset_of(&ferrix_ap_end);
    if len > PAGE_SIZE as usize {
        return Err("the startup trampoline does not fit in a page");
    }

    let at = crate::mm::direct_map(phys);
    // SAFETY: the trampoline is a run of bytes in this image's `.text`, and
    // `at` is a page the frame allocator was never given, reached through the
    // direct map. The two cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(&raw const ferrix_ap_start, at as *mut u8, len) };

    let regime = cpu::translation_regime();
    let params = at as usize + offset_of(&ferrix_ap_params);
    for (slot, value) in [
        regime.mair,
        regime.tcr,
        regime.ttbr0,
        regime.ttbr1,
        0,
        ap_entry as usize as u64,
    ]
    .into_iter()
    .enumerate()
    {
        // SAFETY: `params` is inside the page copied above, and the six slots
        // are the six quadwords the assembly reserved there.
        unsafe { core::ptr::write_unaligned((params + slot * 8) as *mut u64, value) };
    }

    if regime.ttbr0 == 0 {
        return Err("the loader's identity map is already gone, so no processor can be started");
    }

    TRAMPOLINE.store(phys, Ordering::Release);
    Ok(phys)
}

/// Start the processor whose `MPIDR_EL1` is `hardware_id`.
///
/// # Safety
///
/// `stack_top` must be the top of a stack nothing else will run on, and the
/// caller must have published the [`crate::percpu::PerCpu`] record this
/// processor is to install.
pub(crate) unsafe fn start_cpu(hardware_id: u64, stack_top: u64) -> Result<(), &'static str> {
    let phys = match TRAMPOLINE.load(Ordering::Acquire) {
        0 => {
            // SAFETY: called from `smp::start_others`, which runs before the
            // identity map is dropped and is the only caller.
            unsafe { install_trampoline()? }
        }
        installed => installed,
    };

    let at = crate::mm::direct_map(phys) as usize + offset_of(&ferrix_ap_params);
    // SAFETY: inside the trampoline's own page; the processor it is for has not
    // been started yet, so nothing is reading it.
    unsafe { core::ptr::write_unaligned((at + 4 * 8) as *mut u64, stack_top) };

    // The processor about to start has its caches off, so it reads memory
    // rather than the cache line this one just wrote. Without this the
    // parameters it loads are whatever was in RAM before.
    cpu::publish_to_uncached(crate::mm::direct_map(phys), PAGE_SIZE);

    match cpu::psci_cpu_on(PSCI_CPU_ON, hardware_id, phys) {
        0 => Ok(()),
        // PSCI's own error numbering, as signed values in x0. -4 is
        // ALREADY_ON, which for a processor nothing has started means the
        // firmware tables describe one that is not really there.
        -4 => Err("PSCI says the processor is already on"),
        -2 => Err("PSCI refused the entry point as invalid"),
        _ => Err("PSCI would not start the processor"),
    }
}

/// Where a freshly started processor arrives from the trampoline.
///
/// Entered at a virtual address, on its own stack, with the identity map still
/// live underneath it — which is the first thing it gets rid of, because
/// `TCR_EL1` is per-CPU and the processor that dropped its own identity map
/// dropped only its own.
extern "C" fn ap_entry() -> ! {
    // SAFETY: the first Rust to run on this processor, exactly once, with every
    // interrupt masked by PSCI.
    unsafe { trap::init() };
    // SAFETY: this processor reached Rust through a virtual address and its
    // stack is one, so nothing of its own is in the lower half. The trampoline
    // was, and is behind it.
    unsafe { cpu::disable_ttbr0() };

    crate::smp::ap_start()
}

/// Bring up this processor's GIC interface.
///
/// # Safety
///
/// Must be called once per processor, after its vector table is installed and
/// while interrupts are masked.
pub(crate) unsafe fn init_cpu_interrupts() -> Result<(), &'static str> {
    // SAFETY: the caller's contract.
    unsafe { gic::init_this_cpu() }
}

/// The interrupt number an inter-processor interrupt arrives as.
///
/// Software-generated interrupts are identifiers 0 to 15, and an identifier is
/// all the GIC carries — so these *are* the interrupt numbers, with none of
/// x86-64's translation from a vector.
pub(crate) const fn ipi_irq(ipi: Ipi) -> u32 {
    match ipi {
        Ipi::Reschedule => 0,
        Ipi::TlbFlush => 1,
        Ipi::Quiesce => 2,
    }
}

/// Let this CPU take `ipi`.
pub(crate) fn unmask_ipi(ipi: Ipi) {
    gic::enable(ipi_irq(ipi));
}

/// Send `ipi` to `target`.
pub(crate) fn send_ipi(target: Target, ipi: Ipi) {
    gic::send_software_interrupt(target, ipi_irq(ipi));
}

/// Where this CPU's per-CPU record is, or zero before one is installed.
///
/// `TPIDR_EL1` is the architecture's own answer to this question: a register
/// reserved for the kernel at EL1 with no other defined meaning, which resets
/// to zero — so "no record yet" needs no sentinel and no placeholder.
pub(crate) fn current_percpu() -> usize {
    cpu::read_tpidr() as usize
}

/// Point `TPIDR_EL1` at this CPU's record.
///
/// # Safety
///
/// `address` must be a `PerCpu` that lives for the rest of the system.
pub(crate) unsafe fn install_percpu(address: usize) {
    // SAFETY: the caller guarantees the record. `TPIDR_EL1` has no
    // architectural meaning beyond holding what the kernel put there.
    unsafe { cpu::write_tpidr(address as u64) };
}
