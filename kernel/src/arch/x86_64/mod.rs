//! The x86-64 end of the kernel.

mod apic;
mod clock;
pub(crate) mod console;
mod cpu;
mod gdt;
mod trap;

use ferrix_bootinfo::BootView;

use crate::early::{EarlyError, EarlyMemory};
use crate::irq::Report;

/// Name for log lines.
pub(crate) const NAME: &str = "x86_64";

/// The page table descriptor layout this machine uses.
pub(crate) type PageEncoding = ferrix_paging::x86_64::X86_64;

/// Bring up the early console.
///
/// Nothing to map: the 16550 is behind I/O ports, which are a separate address
/// space with no page tables of their own. The `AArch64` counterpart has to map
/// an `MMIO` window first, which is why this takes an argument it ignores.
pub(crate) fn init_console(_memory: &mut EarlyMemory) -> Result<(), EarlyError> {
    console::init();
    Ok(())
}

pub(crate) use trap::{TrapFrame, advance_past_breakpoint, breakpoint, classify, report_trap};

/// Install the descriptor tables and the trap handlers.
///
/// Until this runs the kernel is executing on firmware's tables: a fault would
/// enter a handler that stopped existing at `exit_boot_services`, which is a
/// triple fault and a silent reset.
///
/// # Safety
///
/// Must be called exactly once, on the boot CPU, before interrupts are enabled.
pub(crate) unsafe fn init_traps() {
    // SAFETY: called once from `kmain`, before anything can fault deliberately.
    unsafe { gdt::init() };
    // SAFETY: after `gdt::init`, whose kernel code selector every gate names.
    unsafe { trap::init() };
}

/// Invalidate the whole TLB.
pub(crate) fn flush_tlb() {
    cpu::reload_cr3();
}

/// Root of the loader's identity map, while it still exists.
///
/// Always `None` on x86-64: there is only one root table, and the identity map
/// is the lower half of it. A sweep that walks the kernel's tables has
/// therefore already seen it — which is what makes the W^X check on this
/// architecture find the identity map without being told where it is.
pub(crate) const fn identity_root(_view: &BootView<'_>) -> Option<u64> {
    None
}

/// The first root-table slot belonging to the upper half.
///
/// A 48-bit address space has 512 top-level slots, and the upper half starts
/// at slot 256. Everything below it is the identity map the loader built and
/// nothing else: the direct map begins at slot 256, the `vmap` area at 510 and
/// the kernel image at 511.
const UPPER_HALF_SLOT: usize = 256;

/// Drop the loader's identity map by clearing the lower half of the root
/// table.
///
/// The frames those tables occupied are *not* given back. They are part of the
/// loader's page table pool, which the memory map reports as
/// `MemKind::PageTables` and which the kernel is still running on — the upper
/// half's tables came out of the same pool. Reclaiming the lower half's share
/// would mean tracking which frame of the pool belongs to which half, and the
/// pool is a few dozen frames.
///
/// # Safety
///
/// Nothing may still be executing or reading through the lower half. The
/// kernel runs entirely in the upper half from its first instruction.
pub(crate) unsafe fn drop_identity_map(_view: &BootView<'_>) {
    crate::mm::clear_root_slots(0..UPPER_HALF_SLOT);
}

/// Stop the machine, and QEMU with it.
pub(crate) fn shutdown() -> ! {
    cpu::debug_exit();
    halt()
}

/// Stop this CPU permanently.
pub(crate) fn halt() -> ! {
    cpu::disable_interrupts();
    loop {
        cpu::hlt();
    }
}

/// Bring up the local APIC, the counter and the timer.
///
/// Order matters twice over. The counter has to exist before the local APIC
/// timer can be calibrated against it, and the interrupt descriptor table has
/// to exist before the local APIC is enabled — an interrupt delivered to a
/// vector with no gate is a fault the CPU cannot report.
///
/// # Safety
///
/// Must be called exactly once, on the boot CPU, after [`init_traps`] and
/// while interrupts are still masked.
pub(crate) unsafe fn init_interrupts(view: &BootView<'_>) -> Result<Report, &'static str> {
    let firmware =
        crate::acpi::Firmware::open(view).map_err(|_| "the machine has no readable ACPI tables")?;
    let acpi = firmware.acpi();

    let counter = clock::init(&acpi)?;
    // SAFETY: called once from `kmain`, on the boot CPU, after `init_traps`
    // filled the IDT and with interrupts masked.
    unsafe { apic::init(&acpi)? };

    Ok(Report {
        counter,
        counter_hz: clock::counter_hz(),
        controller: "APIC",
        timer: "local APIC timer",
        timer_hz: apic::timer_hz(),
    })
}

/// Unmask interrupts on this CPU.
pub(crate) fn enable_interrupts() {
    cpu::enable_interrupts();
}

/// `RFLAGS.IF` — interrupts are unmasked.
const RFLAGS_INTERRUPT: u64 = 1 << 9;

/// How `ferrix_sync`'s interrupt-masking lock masks interrupts here.
#[derive(Debug)]
pub(crate) struct Irq;

// SAFETY: `disable` masks interrupts on this CPU with `cli` and reports
// whether they were unmasked beforehand; `restore` unmasks only if they were,
// so nesting two critical sections leaves the inner one unable to unmask
// halfway out of the outer one. Neither touches any other state.
unsafe impl ferrix_sync::IrqControl for Irq {
    fn disable() -> usize {
        let was_enabled = cpu::read_rflags() & RFLAGS_INTERRUPT != 0;
        cpu::disable_interrupts();
        usize::from(was_enabled)
    }

    fn restore(state: usize) {
        if state != 0 {
            cpu::enable_interrupts();
        }
    }
}

/// Wait until an interrupt arrives.
pub(crate) fn wait_for_interrupt() {
    cpu::hlt();
}

/// The free-running counter.
pub(crate) fn counter_now() -> u64 {
    clock::counter_now()
}

/// How fast it counts.
pub(crate) fn counter_hz() -> u64 {
    clock::counter_hz()
}

/// Fire the timer interrupt once, `nanos` from now.
pub(crate) fn timer_arm(nanos: u64) {
    apic::arm(nanos);
}

/// Stop the timer.
pub(crate) fn timer_disarm() {
    apic::disarm();
}

/// The interrupt number the timer arrives on.
pub(crate) fn timer_irq() -> u32 {
    apic::timer_irq()
}

/// Dispatch the interrupt that arrived and retire it at the controller.
///
/// x86-64 puts the vector in the frame, so there is nothing to claim: the
/// work here is deciding what *not* to acknowledge. The spurious vector is
/// raised by the local APIC when an interrupt is withdrawn between being
/// signalled and being taken, and it is the one vector that must never be
/// given an end-of-interrupt — doing so retires a different interrupt that
/// was genuinely in service.
pub(crate) fn service_interrupts(frame: &mut TrapFrame, handle: fn(u32)) {
    if frame.vector == apic::SPURIOUS_VECTOR {
        return;
    }
    handle((frame.vector - trap::IRQ_BASE) as u32);
    apic::end_of_interrupt();
}
