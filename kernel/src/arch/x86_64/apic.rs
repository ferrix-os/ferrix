//! The local APIC and the I/O APIC.
//!
//! The local APIC is per-CPU and is how a timer interrupt and, from stage 4,
//! an inter-processor interrupt reach this core. The I/O APIC is per-machine
//! and is how a *device* interrupt does. Stage 3 needs the first and only
//! quiesces the second: nothing is wired to a device line until stage 10, and
//! an I/O APIC left as firmware set it up is an I/O APIC that may still be
//! delivering the PIT's interrupt to a vector the kernel has other plans for.
//!
//! # Why the timer is calibrated rather than read
//!
//! The local APIC timer counts at the core crystal's frequency divided by
//! whatever the divide register says, and on most machines nothing reports
//! that frequency. `CPUID` leaf 0x15 does on recent Intel parts and nowhere
//! else. So it is measured against the counter `super::clock` brought up a
//! moment earlier, which is the same thing every other kernel does.

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_acpi::{Acpi, MadtEntry};

use super::clock;
use super::trap::IRQ_BASE;
use crate::acpi::DirectMap;
use crate::mmio::Mmio;

/// Where the local APIC's registers are when the MADT does not say.
const LAPIC_DEFAULT_BASE: u64 = 0xFEE0_0000;
/// Bytes of register window a local APIC occupies.
const LAPIC_WINDOW: u64 = 0x400;

/// Task priority: which interrupts this CPU is willing to take.
const LAPIC_TPR: u64 = 0x080;
/// End of interrupt.
const LAPIC_EOI: u64 = 0x0B0;
/// Spurious interrupt vector, and the software enable bit.
const LAPIC_SVR: u64 = 0x0F0;
/// Local vector table entry for the timer.
const LAPIC_LVT_TIMER: u64 = 0x320;
/// Timer initial count. Writing it starts the timer.
const LAPIC_TIMER_ICR: u64 = 0x380;
/// Timer current count, counting down towards zero.
const LAPIC_TIMER_CCR: u64 = 0x390;
/// Timer divide configuration.
const LAPIC_TIMER_DCR: u64 = 0x3E0;

/// Spurious vector register: the local APIC is enabled.
const SVR_ENABLE: u32 = 1 << 8;
/// Local vector table: this entry is masked.
const LVT_MASKED: u32 = 1 << 16;
/// Divide configuration: divide the core crystal by sixteen.
///
/// Sixteen rather than one so a 32-bit initial count spans a longer interval:
/// at a 1 `GHz` crystal, divide-by-one wraps in four seconds and
/// divide-by-sixteen in sixty-eight.
const DCR_DIVIDE_BY_16: u32 = 0b0011;

/// The vector the spurious interrupt arrives on.
///
/// The architecture requires the low four bits to be set on some old parts,
/// and by convention it is the top of the vector space. It must never be
/// acknowledged — writing an end-of-interrupt for it would retire an
/// interrupt that was never delivered.
pub(crate) const SPURIOUS_VECTOR: u64 = 0xFF;

/// The vector the local APIC timer arrives on.
///
/// Just below the spurious vector, and deliberately far from the bottom of
/// the range: vectors from [`IRQ_BASE`] upwards are handed out to I/O APIC
/// inputs by global system interrupt number, and the timer is not one of
/// those.
pub(crate) const TIMER_VECTOR: u64 = 0xFE;

/// The interrupt number the generic layer knows the timer by.
pub(crate) const fn timer_irq() -> u32 {
    (TIMER_VECTOR - IRQ_BASE) as u32
}

/// I/O APIC: the register selector.
const IOAPIC_SELECT: u64 = 0x00;
/// I/O APIC: the data window the selector addresses.
const IOAPIC_WINDOW: u64 = 0x10;
/// I/O APIC register: version, with the highest input in bits 16..24.
const IOAPIC_VERSION_REGISTER: u32 = 0x01;
/// I/O APIC register: the first half of the first redirection entry.
const IOAPIC_REDIRECTION_BASE: u32 = 0x10;
/// A redirection entry's low word: the input is masked.
const IOAPIC_ENTRY_MASKED: u32 = 1 << 16;
/// Bytes of register window an I/O APIC occupies.
const IOAPIC_WINDOW_BYTES: u64 = 0x20;

/// Virtual address of this CPU's local APIC registers.
static LAPIC: AtomicU64 = AtomicU64::new(0);

/// How fast the local APIC timer counts, after the divider.
static TIMER_HZ: AtomicU64 = AtomicU64::new(0);

/// The local APIC's register window.
fn lapic() -> Mmio {
    let base = LAPIC.load(Ordering::Relaxed);
    if base == 0 {
        return Mmio::unmapped();
    }
    Mmio::at(base)
}

/// Bring up the local APIC, calibrate its timer, and quiesce every I/O APIC.
///
/// # Safety
///
/// Must be called once, on the boot CPU, after the interrupt descriptor table
/// is loaded and while interrupts are still masked: this leaves the local APIC
/// enabled, and an interrupt arriving before there is a gate for it is a fault
/// with no handler.
pub(crate) unsafe fn init(acpi: &Acpi<'_, DirectMap>) -> Result<(), &'static str> {
    let madt = acpi.madt().map_err(|_| "the machine has no MADT")?;

    let phys = match madt.local_apic_address() {
        0 => LAPIC_DEFAULT_BASE,
        address => address,
    };
    let base =
        crate::vmap::map_device(phys, LAPIC_WINDOW).map_err(|_| "could not map the local APIC")?;
    LAPIC.store(base, Ordering::Relaxed);

    let regs = Mmio::at(base);
    // Accept every priority. Firmware may have left this high enough to
    // silently discard the timer.
    regs.write32(LAPIC_TPR, 0);
    regs.write32(LAPIC_SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);

    calibrate(regs)?;
    quiesce_io_apics(&madt)?;
    Ok(())
}

/// Measure the timer against the counter, and leave it stopped.
fn calibrate(regs: Mmio) -> Result<(), &'static str> {
    /// Microseconds to measure over. Long enough that the counter's own
    /// resolution is noise, short enough not to stretch the boot.
    const WINDOW_MICROS: u64 = 10_000;

    let reference_hz = clock::counter_hz();
    if reference_hz == 0 {
        return Err("the local APIC timer has no counter to calibrate against");
    }
    let window_ticks = reference_hz * WINDOW_MICROS / 1_000_000;

    regs.write32(LAPIC_TIMER_DCR, DCR_DIVIDE_BY_16);
    // Masked: this is a measurement, and a timer interrupt arriving in the
    // middle of it would be delivered before there is a handler.
    regs.write32(LAPIC_LVT_TIMER, TIMER_VECTOR as u32 | LVT_MASKED);
    regs.write32(LAPIC_TIMER_ICR, u32::MAX);

    let started = clock::counter_now();
    while clock::counter_now().wrapping_sub(started) < window_ticks {}
    let remaining = regs.read32(LAPIC_TIMER_CCR);

    regs.write32(LAPIC_TIMER_ICR, 0);

    let counted = u64::from(u32::MAX - remaining);
    if counted == 0 {
        return Err("the local APIC timer did not count");
    }
    TIMER_HZ.store(counted * 1_000_000 / WINDOW_MICROS, Ordering::Relaxed);
    Ok(())
}

/// Mask every input on every I/O APIC firmware described.
///
/// Not "configure": there is nothing to route yet. What this prevents is a
/// line firmware left unmasked — the PIT's, classically — arriving at a vector
/// chosen by whoever wrote the firmware.
fn quiesce_io_apics(madt: &ferrix_acpi::Madt<'_>) -> Result<(), &'static str> {
    for entry in madt.entries() {
        let MadtEntry::IoApic(io_apic) = entry else {
            continue;
        };
        let base = crate::vmap::map_device(u64::from(io_apic.address), IOAPIC_WINDOW_BYTES)
            .map_err(|_| "could not map an I/O APIC")?;
        mask_all_inputs(Mmio::at(base));
    }
    Ok(())
}

/// Read one of an I/O APIC's indirect registers.
fn io_apic_read(regs: Mmio, register: u32) -> u32 {
    regs.write32(IOAPIC_SELECT, register);
    regs.read32(IOAPIC_WINDOW)
}

/// Write one of them.
fn io_apic_write(regs: Mmio, register: u32, value: u32) {
    regs.write32(IOAPIC_SELECT, register);
    regs.write32(IOAPIC_WINDOW, value);
}

/// Mask every redirection entry this I/O APIC has.
fn mask_all_inputs(regs: Mmio) {
    let inputs = ((io_apic_read(regs, IOAPIC_VERSION_REGISTER) >> 16) & 0xFF).saturating_add(1);
    for input in 0..inputs {
        let low = IOAPIC_REDIRECTION_BASE.saturating_add(input.saturating_mul(2));
        io_apic_write(regs, low, IOAPIC_ENTRY_MASKED);
    }
}

/// Retire the interrupt currently being serviced.
pub(crate) fn end_of_interrupt() {
    lapic().write32(LAPIC_EOI, 0);
}

/// Fire the timer interrupt once, `nanos` from now.
pub(crate) fn arm(nanos: u64) {
    let hz = TIMER_HZ.load(Ordering::Relaxed);
    if hz == 0 {
        return;
    }
    let ticks = u128::from(nanos) * u128::from(hz) / 1_000_000_000;
    // At least one tick: a zero initial count stops the timer rather than
    // firing immediately, which would be an interrupt that never arrives.
    let count = u32::try_from(ticks.max(1)).unwrap_or(u32::MAX);

    let regs = lapic();
    regs.write32(LAPIC_LVT_TIMER, TIMER_VECTOR as u32);
    regs.write32(LAPIC_TIMER_ICR, count);
}

/// Stop the timer.
pub(crate) fn disarm() {
    let regs = lapic();
    regs.write32(LAPIC_TIMER_ICR, 0);
    regs.write32(LAPIC_LVT_TIMER, TIMER_VECTOR as u32 | LVT_MASKED);
}

/// How fast the timer counts, for the boot log.
pub(crate) fn timer_hz() -> u64 {
    TIMER_HZ.load(Ordering::Relaxed)
}
