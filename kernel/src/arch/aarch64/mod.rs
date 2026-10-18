//! The `AArch64` end of the kernel.

pub(crate) mod console;
mod cpu;
mod gic;
mod timer;
mod trap;

use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::BootView;

use crate::early::{EarlyError, EarlyMemory};
use crate::irq::Report;

/// Name for log lines.
pub(crate) const NAME: &str = "aarch64";

/// The page table descriptor layout this machine uses.
pub(crate) type PageEncoding = ferrix_paging::aarch64::AArch64;

/// Bring up the early console.
///
/// Unlike x86-64, this has real work to do: the PL011 is `MMIO` and has to be
/// mapped as device memory before a single byte can go out.
pub(crate) fn init_console(memory: &mut EarlyMemory) -> Result<(), EarlyError> {
    console::init(memory)
}

pub(crate) use trap::{TrapFrame, advance_past_breakpoint, breakpoint, classify, report_trap};

/// Install the exception vector table.
///
/// Until this runs the CPU is still pointing at firmware's vectors, which
/// stopped existing at `exit_boot_services`: any fault before it is a jump into
/// reclaimed memory.
///
/// # Safety
///
/// Must be called exactly once, on the boot CPU, before interrupts are
/// unmasked.
pub(crate) unsafe fn init_traps() {
    // SAFETY: called once from `kmain`, before anything faults deliberately.
    unsafe { trap::init() };
}

/// Publish page table writes and invalidate the whole TLB.
pub(crate) fn flush_tlb() {
    cpu::flush_tlb();
}

/// Root of the loader's identity map, while it still exists.
///
/// `Some` on `AArch64` because the identity map is a second translation regime
/// with its own root, which nothing walking the kernel's tables would ever
/// see. The W^X sweep asks for this so that it sweeps the whole of what the
/// hardware can translate rather than the half the kernel happens to own.
pub(crate) fn identity_root(view: &BootView<'_>) -> Option<u64> {
    let ttbr0 = view.raw().ttbr0_phys;
    // SAFETY-free: this is a read of a `u64` the loader filled in, and zero is
    // how the loader says "the identity map is in the kernel's own table".
    (ttbr0 != 0 && !IDENTITY_DROPPED.load(Ordering::Relaxed)).then_some(ttbr0)
}

/// Set once [`drop_identity_map`] has run.
static IDENTITY_DROPPED: AtomicBool = AtomicBool::new(false);

/// Drop the loader's identity map.
///
/// # Safety
///
/// Nothing may still be executing or reading through the lower half of the
/// address space. See [`cpu::disable_ttbr0`].
pub(crate) unsafe fn drop_identity_map(_view: &BootView<'_>) {
    // SAFETY: the caller guarantees the lower half is unused, and the kernel
    // has been running entirely in the upper half since its first instruction.
    unsafe { cpu::disable_ttbr0() };
    IDENTITY_DROPPED.store(true, Ordering::Relaxed);
}

/// Stop the machine.
pub(crate) fn shutdown() -> ! {
    cpu::psci_system_off();
    halt()
}

/// Stop this CPU permanently.
pub(crate) fn halt() -> ! {
    cpu::disable_interrupts();
    loop {
        cpu::wfi();
    }
}

/// Bring up the interrupt controller and the generic timer.
///
/// # Safety
///
/// Must be called exactly once, on the boot CPU, after [`init_traps`] and
/// while interrupts are masked.
pub(crate) unsafe fn init_interrupts(view: &BootView<'_>) -> Result<Report, &'static str> {
    let firmware =
        crate::acpi::Firmware::open(view).map_err(|_| "the machine has no readable ACPI tables")?;
    let acpi = firmware.acpi();

    // SAFETY: called once from `kmain`, on the boot CPU, after the vector
    // table is installed and with interrupts masked.
    let version = unsafe { gic::init(&acpi)? };
    timer::init(&acpi)?;

    // The timer is a private peripheral interrupt, so enabling it is a
    // distributor operation like any other — it is only *private* in that
    // each core has its own copy of the number.
    gic::enable(timer::irq());

    Ok(Report {
        counter: "generic timer",
        counter_hz: timer::counter_hz(),
        controller: match version {
            2 => "GICv2",
            _ => "GIC",
        },
        timer: "virtual timer",
        timer_hz: timer::counter_hz(),
    })
}

/// Unmask `IRQ` on this CPU.
pub(crate) fn enable_interrupts() {
    cpu::enable_interrupts();
}

/// How `ferrix_sync`'s interrupt-masking lock masks interrupts here.
#[derive(Debug)]
pub(crate) struct Irq;

// SAFETY: `disable` masks every interrupt on this CPU and returns the `DAIF`
// value it found; `restore` puts exactly that value back, so nesting two
// critical sections cannot unmask halfway out of the outer one. `DAIF` is
// four bits wide and fits a `usize` on every target this kernel builds for.
unsafe impl ferrix_sync::IrqControl for Irq {
    fn disable() -> usize {
        let previous = cpu::read_daif();
        cpu::disable_interrupts();
        previous as usize
    }

    fn restore(state: usize) {
        // SAFETY: `state` is a `DAIF` value this CPU's `disable` returned a
        // moment ago, which is exactly `write_daif`'s contract.
        unsafe { cpu::write_daif(state as u64) };
    }
}

/// Wait until an interrupt arrives.
pub(crate) fn wait_for_interrupt() {
    cpu::wfi();
}

/// The free-running counter.
pub(crate) fn counter_now() -> u64 {
    timer::counter_now()
}

/// How fast it counts.
pub(crate) fn counter_hz() -> u64 {
    timer::counter_hz()
}

/// Fire the timer interrupt once, `nanos` from now.
pub(crate) fn timer_arm(nanos: u64) {
    timer::arm(nanos);
}

/// Stop the timer.
pub(crate) fn timer_disarm() {
    timer::disarm();
}

/// The interrupt number the timer arrives on.
pub(crate) fn timer_irq() -> u32 {
    timer::irq()
}

/// Claim every pending interrupt, dispatch it, and retire it.
///
/// A loop rather than a single claim: the exception is taken once however many
/// interrupts are pending, so returning after one would leave the rest
/// asserted and take the exception again immediately. Reading `GICC_IAR` until
/// it reports a special identifier is how the controller says it has no more.
pub(crate) fn service_interrupts(_frame: &mut TrapFrame, handle: fn(u32)) {
    while let Some((id, acknowledgement)) = gic::claim() {
        handle(id);
        gic::complete(acknowledgement);
    }
}
