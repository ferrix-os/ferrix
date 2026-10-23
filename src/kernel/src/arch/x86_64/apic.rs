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

use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub(super) mod check;

use ferrix_acpi::{Acpi, IsaInterrupt, Madt, MadtEntry};
use ferrix_sync::IrqControl;

use super::clock;
use super::trap::IRQ_BASE;
use crate::discovery::acpi::DirectMap;
use crate::mmio::Mmio;

/// Where the local APIC's registers are when the MADT does not say.
const LAPIC_DEFAULT_BASE: u64 = 0xFEE0_0000;
/// Bytes of register window a local APIC occupies.
const LAPIC_WINDOW: u64 = 0x400;

/// Local APIC identifier. In xAPIC mode it is the register's top byte.
const LAPIC_ID: u64 = 0x020;
/// Task priority: which interrupts this CPU is willing to take.
const LAPIC_TPR: u64 = 0x080;
/// End of interrupt.
const LAPIC_EOI: u64 = 0x0B0;
/// Spurious interrupt vector, and the software enable bit.
const LAPIC_SVR: u64 = 0x0F0;
/// Interrupt command, low half. Writing it sends.
const LAPIC_ICR_LOW: u64 = 0x300;
/// Interrupt command, high half: the destination.
const LAPIC_ICR_HIGH: u64 = 0x310;
/// Local vector table entry for the timer.
const LAPIC_LVT_TIMER: u64 = 0x320;

/// Interrupt command: delivery mode INIT, which resets the destination into
/// waiting for a start-up IPI.
const ICR_INIT: u32 = 0b101 << 8;
/// Interrupt command: delivery mode start-up. The vector is a page number.
const ICR_STARTUP: u32 = 0b110 << 8;
/// Interrupt command: delivery mode NMI. The vector field is ignored; the
/// processor enters through vector 2.
const ICR_NMI: u32 = 0b100 << 8;
/// Interrupt command: level assert, required for everything except the INIT
/// de-assert that only processors older than the Pentium 4 ever needed.
const ICR_ASSERT: u32 = 1 << 14;
/// Interrupt command: the last one has not been accepted yet.
const ICR_PENDING: u32 = 1 << 12;
/// Interrupt command: destination shorthand "every processor but this one".
const ICR_ALL_BUT_SELF: u32 = 0b11 << 18;

/// The vector inter-processor interrupts arrive on: just below the timer's,
/// and like it outside the range handed to I/O APIC inputs.
pub(crate) const IPI_VECTOR: u64 = 0xFD;

/// The interrupt number the generic layer knows an IPI by.
pub(crate) const fn ipi_irq() -> u32 {
    (IPI_VECTOR - IRQ_BASE) as u32
}
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
/// A redirection entry's low word: the input is asserted low.
const IOAPIC_ENTRY_ACTIVE_LOW: u32 = 1 << 13;
/// A redirection entry's low word: the input is level triggered.
const IOAPIC_ENTRY_LEVEL: u32 = 1 << 15;
/// Where a redirection entry's high word keeps the destination APIC identifier.
const IOAPIC_DESTINATION_SHIFT: u32 = 24;
/// Bytes of register window an I/O APIC occupies.
const IOAPIC_WINDOW_BYTES: u64 = 0x20;

/// `IA32_APIC_BASE`: where the local APIC is, and its mode.
const IA32_APIC_BASE: u32 = 0x1B;
/// `IA32_APIC_BASE`: the local APIC is enabled.
const APIC_BASE_ENABLE: u64 = 1 << 11;
/// `IA32_APIC_BASE`: x2APIC mode, in which the MMIO window does not exist.
const APIC_BASE_X2APIC: u64 = 1 << 10;
/// `IA32_ARCH_CAPABILITIES`, which CPUID leaf 7 enumerates.
const IA32_ARCH_CAPABILITIES: u32 = 0x10A;
/// `IA32_ARCH_CAPABILITIES`: `IA32_XAPIC_DISABLE_STATUS` exists.
const ARCH_CAPABILITIES_XAPIC_DISABLE_STATUS: u64 = 1 << 21;
/// `IA32_XAPIC_DISABLE_STATUS`.
const IA32_XAPIC_DISABLE_STATUS: u32 = 0xBD;
/// `IA32_XAPIC_DISABLE_STATUS`: legacy xAPIC is disabled; the platform
/// locks the local APIC in x2APIC mode.
const LEGACY_XAPIC_DISABLED: u64 = 1 << 0;

/// The first of the x2APIC MSRs: register `offset` of the xAPIC window is
/// MSR `X2APIC_MSR_BASE + (offset >> 4)` in x2APIC mode.
const X2APIC_MSR_BASE: u32 = 0x800;
/// x2APIC: the interrupt command register, one 64-bit MSR with the
/// destination in bits 63:32.
const X2APIC_ICR: u32 = 0x830;

/// Whether this kernel drives every local APIC in x2APIC mode, through its
/// MSRs, rather than in xAPIC mode through the MMIO window. Decided once, by
/// the boot processor in [`init`], before any processor uses its local APIC;
/// every application processor follows it ([`enter_mode`]).
static X2APIC: AtomicBool = AtomicBool::new(false);

/// Whether the local APICs run in x2APIC mode.
pub(crate) fn x2apic() -> bool {
    X2APIC.load(Ordering::Relaxed)
}

/// The command-line option that chooses the mode: `x2apic` to run in x2APIC
/// mode wherever the processor has it, `xapic` to stay in xAPIC mode unless
/// the platform locks x2APIC. Without it, x2APIC mode is taken only where
/// firmware locked it or left it on.
pub(crate) const OPTION: &str = "ferrix.apic";

/// Why the boot processor chose x2APIC mode, for its line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Why {
    /// `IA32_XAPIC_DISABLE_STATUS` says legacy xAPIC is disabled.
    Locked,
    /// Firmware handed over the boot processor in x2APIC mode.
    LeftOn,
    /// `ferrix.apic=x2apic`.
    Asked,
}

/// Choose the mode, on the boot processor, before its local APIC is used.
fn choose_mode(option: Option<&str>) -> Result<Option<Why>, &'static str> {
    let offered = core::arch::x86_64::__cpuid(1).ecx & 1 << 21 != 0;
    // SAFETY: (SYSREG) `IA32_APIC_BASE` exists on every processor with a
    // local APIC, which every x86-64 processor has.
    let base = unsafe { super::cpu::read_msr(IA32_APIC_BASE) };
    let why = if xapic_locked() {
        Some(Why::Locked)
    } else if option == Some("xapic") {
        None
    } else if base & APIC_BASE_X2APIC != 0 && base & APIC_BASE_ENABLE != 0 {
        Some(Why::LeftOn)
    } else if option == Some("x2apic") && offered {
        Some(Why::Asked)
    } else {
        None
    };
    if why.is_some() && !offered {
        return Err("the local APIC is in x2APIC mode but CPUID does not offer x2APIC");
    }
    Ok(why)
}

/// Put this processor's local APIC in the mode [`X2APIC`] says, before it
/// is used: on the boot processor from [`init`], on every application
/// processor from `secondary_start`.
///
/// # Errors
///
/// As [`leave_x2apic`], in xAPIC mode.
pub(crate) fn enter_mode() -> Result<(), &'static str> {
    if !x2apic() {
        return leave_x2apic();
    }
    let _ = MODE_CHECKED.fetch_add(1, Ordering::Relaxed);
    // SAFETY: (SYSREG) `IA32_APIC_BASE` exists on every processor with a
    // local APIC, which every x86-64 processor has.
    let base = unsafe { super::cpu::read_msr(IA32_APIC_BASE) };
    let both = APIC_BASE_ENABLE | APIC_BASE_X2APIC;
    if base & both == both {
        return Ok(());
    }
    if base & APIC_BASE_ENABLE == 0 {
        // SAFETY: (SYSREG) disabled to xAPIC, a transition the SDM allows,
        // on this processor's own register with its base unchanged; nothing
        // uses the local APIC before the next write.
        unsafe { super::cpu::write_msr(IA32_APIC_BASE, base | APIC_BASE_ENABLE) };
    }
    // SAFETY: (SYSREG) xAPIC to x2APIC, a transition the SDM allows, on this
    // processor's own register; the boot processor saw CPUID offer x2APIC,
    // and every processor of a platform is of one kind.
    unsafe { super::cpu::write_msr(IA32_APIC_BASE, base | both) };
    Ok(())
}

/// Processors that looked at their local APIC's mode before using it
/// ([`enter_mode`]).
static MODE_CHECKED: AtomicU64 = AtomicU64::new(0);
/// Of those, the ones found in x2APIC mode and switched to xAPIC.
static SWITCHED_TO_XAPIC: AtomicU64 = AtomicU64::new(0);

/// Make sure this processor's local APIC is in xAPIC mode before its MMIO
/// window is used, which is all this kernel drives (`docs/NVIDIA.md` §12.3,
/// condition G6). Firmware may have left any processor in x2APIC mode, and
/// INIT does not take one out of it, so the boot processor ([`init`]) and
/// every application processor (`smp.rs`, `secondary_start`) ask, each of
/// its own MSR.
///
/// From x2APIC mode the SDM's path is taken: disabled (`EN` = 0, `EXTD` = 0),
/// then xAPIC (`EN` = 1). Counted, for stage 4's line.
///
/// # Errors
///
/// The platform locks the local APIC in x2APIC mode
/// (`IA32_XAPIC_DISABLE_STATUS`): the caller stops the boot by name, the
/// customer's decision 4.
pub(crate) fn leave_x2apic() -> Result<(), &'static str> {
    let _ = MODE_CHECKED.fetch_add(1, Ordering::Relaxed);
    // SAFETY: (SYSREG) `IA32_APIC_BASE` exists on every processor with a
    // local APIC, which every x86-64 processor has.
    let base = unsafe { super::cpu::read_msr(IA32_APIC_BASE) };
    if base & APIC_BASE_X2APIC == 0 {
        return Ok(());
    }
    if xapic_locked() {
        return Err(
            "the platform locks the local APIC in x2APIC mode, which this kernel does \
                    not drive yet",
        );
    }
    let disabled = base & !(APIC_BASE_ENABLE | APIC_BASE_X2APIC);
    // SAFETY: (SYSREG) x2APIC to disabled, the first of the two transitions
    // the SDM allows from x2APIC mode, on this processor's own register,
    // with its base address unchanged. Nothing on this processor uses its
    // local APIC until the second: interrupts are masked here.
    unsafe { super::cpu::write_msr(IA32_APIC_BASE, disabled) };
    // SAFETY: (SYSREG) disabled to xAPIC, the second transition, on the same
    // register with the same base.
    unsafe { super::cpu::write_msr(IA32_APIC_BASE, disabled | APIC_BASE_ENABLE) };
    let _ = SWITCHED_TO_XAPIC.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Whether the platform has disabled legacy xAPIC mode: enumerated by
/// `IA32_ARCH_CAPABILITIES`, itself enumerated by CPUID leaf 7's `EDX`
/// bit 29.
fn xapic_locked() -> bool {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    if __cpuid(0).eax < 7 || __cpuid_count(7, 0).edx & 1 << 29 == 0 {
        return false;
    }
    // SAFETY: (SYSREG) CPUID says `IA32_ARCH_CAPABILITIES` exists.
    let capabilities = unsafe { super::cpu::read_msr(IA32_ARCH_CAPABILITIES) };
    if capabilities & ARCH_CAPABILITIES_XAPIC_DISABLE_STATUS == 0 {
        return false;
    }
    // SAFETY: (SYSREG) `IA32_ARCH_CAPABILITIES` says
    // `IA32_XAPIC_DISABLE_STATUS` exists.
    unsafe { super::cpu::read_msr(IA32_XAPIC_DISABLE_STATUS) & LEGACY_XAPIC_DISABLED != 0 }
}

/// Processors that checked their local APIC's mode, and of those the ones
/// switched from x2APIC to xAPIC.
pub(crate) fn modes_checked() -> (u64, u64) {
    (
        MODE_CHECKED.load(Ordering::Relaxed),
        SWITCHED_TO_XAPIC.load(Ordering::Relaxed),
    )
}

/// Virtual address of this CPU's local APIC registers.
static LAPIC: AtomicU64 = AtomicU64::new(0);

/// How fast the local APIC timer counts, after the divider.
static TIMER_HZ: AtomicU64 = AtomicU64::new(0);

/// The local APIC's register window, in xAPIC mode. Only [`read_reg`],
/// [`write_reg`], [`id`] and [`issue`] use it.
fn lapic() -> Mmio {
    let base = LAPIC.load(Ordering::Relaxed);
    if base == 0 {
        return Mmio::unmapped();
    }
    Mmio::at(base)
}

/// The x2APIC MSR that holds xAPIC register `offset`.
fn x2apic_msr(offset: u64) -> u32 {
    X2APIC_MSR_BASE + (offset >> 4) as u32
}

/// Read local APIC register `offset` (its xAPIC window offset), in whichever
/// mode the local APICs run: every read of a local APIC register goes
/// through here, [`id`] or [`issue`].
fn read_reg(offset: u64) -> u32 {
    if x2apic() {
        // SAFETY: (SYSREG) the local APIC is in x2APIC mode, where every
        // register the kernel names has its MSR.
        return unsafe { super::cpu::read_msr(x2apic_msr(offset)) } as u32;
    }
    lapic().read32(offset)
}

/// Write local APIC register `offset`, as [`read_reg`] reads it.
fn write_reg(offset: u64, value: u32) {
    if x2apic() {
        // SAFETY: (SYSREG) as in `read_reg`; the values written are the
        // xAPIC ones, which x2APIC mode takes unchanged for these registers.
        unsafe { super::cpu::write_msr(x2apic_msr(offset), u64::from(value)) };
        return;
    }
    lapic().write32(offset, value);
}

/// Bring up the local APIC, calibrate its timer, and quiesce every I/O APIC.
///
/// # Safety
///
/// (DEVICE) Must be called once, on the boot CPU, after the interrupt descriptor table
/// is loaded and while interrupts are still masked: this leaves the local APIC
/// enabled, and an interrupt arriving before there is a gate for it is a fault
/// with no handler.
pub(crate) unsafe fn init(
    acpi: &Acpi<'_, DirectMap>,
    option: Option<&str>,
) -> Result<(), &'static str> {
    let madt = acpi.madt().map_err(|_| "the machine has no MADT")?;
    // Before the window is mapped, let alone used: it does not exist in
    // x2APIC mode.
    let why = choose_mode(option)?;
    X2APIC.store(why.is_some(), Ordering::Relaxed);
    enter_mode()?;
    crate::console::println!(
        "  apic     {}",
        match why {
            Some(Why::Locked) => "x2APIC mode (firmware locked it)",
            Some(Why::LeftOn) => "x2APIC mode (firmware left it on)",
            Some(Why::Asked) => "x2APIC mode (ferrix.apic=x2apic asked for it)",
            None => "xAPIC mode",
        }
    );

    if why.is_none() {
        let phys = match madt.local_apic_address() {
            0 => LAPIC_DEFAULT_BASE,
            address => address,
        };
        let base = crate::vmap::map_device(phys, LAPIC_WINDOW)
            .map_err(|_| "could not map the local APIC")?;
        LAPIC.store(base, Ordering::Relaxed);
    }

    // Accept every priority. Firmware may have left this high enough to
    // silently discard the timer.
    write_reg(LAPIC_TPR, 0);
    write_reg(LAPIC_SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);

    calibrate()?;
    quiesce_io_apics(&madt)?;
    Ok(())
}

/// Measure the timer against the counter, and leave it stopped.
fn calibrate() -> Result<(), &'static str> {
    /// Microseconds to measure over. Long enough that the counter's own
    /// resolution is noise, short enough not to stretch the boot.
    const WINDOW_MICROS: u64 = 10_000;

    let reference_hz = clock::counter_hz();
    if reference_hz == 0 {
        return Err("the local APIC timer has no counter to calibrate against");
    }
    let window_ticks = reference_hz * WINDOW_MICROS / 1_000_000;

    write_reg(LAPIC_TIMER_DCR, DCR_DIVIDE_BY_16);
    // Masked: this is a measurement, and a timer interrupt arriving in the
    // middle of it would be delivered before there is a handler.
    write_reg(LAPIC_LVT_TIMER, TIMER_VECTOR as u32 | LVT_MASKED);
    write_reg(LAPIC_TIMER_ICR, u32::MAX);

    let started = clock::counter_now();
    // Subtracted in the counter's width, which `clock` decides: a 32-bit
    // counter subtracted in 64 bits across its wrap reads as an interval of
    // centuries, and the measurement would end at once with a tiny count.
    while clock::ticks_between(started, clock::counter_now()) < window_ticks {}
    let remaining = read_reg(LAPIC_TIMER_CCR);

    write_reg(LAPIC_TIMER_ICR, 0);

    let counted = u64::from(u32::MAX - remaining);
    if counted == 0 {
        return Err("the local APIC timer did not count");
    }
    TIMER_HZ.store(counted * 1_000_000 / WINDOW_MICROS, Ordering::Relaxed);
    Ok(())
}

/// Mask every input on every I/O APIC firmware described.
///
/// Not "configure": the inputs the kernel uses are routed one at a time later,
/// by [`IoApicInput::route`]. What this prevents is a line firmware left
/// unmasked — the PIT's, classically — arriving at a vector chosen by whoever
/// wrote the firmware.
fn quiesce_io_apics(madt: &Madt<'_>) -> Result<(), &'static str> {
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

/// One input of one I/O APIC, and how its line is signalled.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IoApicInput {
    /// The I/O APIC's identifier in the MADT, which the DMAR's scope for it
    /// names as its enumeration ID.
    id: u8,
    /// The I/O APIC's register window, mapped.
    regs: Mmio,
    /// Which redirection entry: the GSI less the I/O APIC's base.
    input: u32,
    /// The entry's polarity and trigger bits.
    signalling: u32,
}

impl IoApicInput {
    /// The I/O APIC input ISA interrupt `irq` arrives on, as the MADT
    /// describes it.
    ///
    /// # Errors
    ///
    /// No I/O APIC covers the interrupt's GSI, or its window cannot be mapped.
    pub(crate) fn for_isa(madt: &Madt<'_>, irq: u8) -> Result<Self, &'static str> {
        let IsaInterrupt {
            gsi,
            active_low,
            level,
        } = madt.isa_interrupt(irq);
        // The I/O APIC with the highest base at or below the GSI; whether the
        // GSI is inside it is its own version register's to say.
        let io_apic = madt
            .entries()
            .filter_map(|entry| match entry {
                MadtEntry::IoApic(io_apic) if io_apic.gsi_base <= gsi => Some(io_apic),
                _ => None,
            })
            .max_by_key(|io_apic| io_apic.gsi_base)
            .ok_or("no I/O APIC covers the interrupt")?;
        let base = crate::vmap::map_device(u64::from(io_apic.address), IOAPIC_WINDOW_BYTES)
            .map_err(|_| "could not map an I/O APIC")?;
        let regs = Mmio::at(base);
        let input = gsi - io_apic.gsi_base;
        if input >= input_count(regs) {
            return Err("no I/O APIC covers the interrupt");
        }
        let signalling = if active_low {
            IOAPIC_ENTRY_ACTIVE_LOW
        } else {
            0
        } | if level { IOAPIC_ENTRY_LEVEL } else { 0 };
        Ok(IoApicInput {
            id: io_apic.id,
            regs,
            input,
            signalling,
        })
    }

    /// Deliver this input to `vector` on this processor, fixed and physical.
    ///
    /// The high word first and the low word last, so an unmasked entry is
    /// never live with a stale destination.
    pub(crate) fn route(self, vector: u64, masked: bool) {
        let low_register = IOAPIC_REDIRECTION_BASE + self.input * 2;
        let destination = id();
        if destination > 0xFF {
            // Without interrupt remapping a redirection entry names eight
            // bits of destination: the input stays masked.
            crate::console::println!(
                "  apic     I/O APIC input {} not routed: APIC ID {destination} is above 255, \
                 which needs interrupt remapping",
                self.input
            );
            io_apic_write(self.regs, low_register, IOAPIC_ENTRY_MASKED);
            return;
        }
        io_apic_write(
            self.regs,
            low_register + 1,
            destination << IOAPIC_DESTINATION_SHIFT,
        );
        let mask = if masked { IOAPIC_ENTRY_MASKED } else { 0 };
        io_apic_write(
            self.regs,
            low_register,
            vector as u32 | self.signalling | mask,
        );
    }
}

impl IoApicInput {
    /// The I/O APIC's MADT identifier.
    pub(crate) fn io_apic_id(self) -> u8 {
        self.id
    }

    /// Whether the line is level-triggered.
    pub(crate) fn level(self) -> bool {
        self.signalling & IOAPIC_ENTRY_LEVEL != 0
    }

    /// Whether the line is asserted low.
    pub(crate) fn active_low(self) -> bool {
        self.signalling & IOAPIC_ENTRY_ACTIVE_LOW != 0
    }

    /// The redirection entry as it is now: its high word in bits 63:32.
    pub(crate) fn redirection(self) -> u64 {
        let low_register = IOAPIC_REDIRECTION_BASE + self.input * 2;
        u64::from(io_apic_read(self.regs, low_register))
            | u64::from(io_apic_read(self.regs, low_register + 1)) << 32
    }

    /// Mask the input, leaving the rest of its entry as it is.
    pub(crate) fn mask(self) {
        let low_register = IOAPIC_REDIRECTION_BASE + self.input * 2;
        let low = io_apic_read(self.regs, low_register);
        io_apic_write(self.regs, low_register, low | IOAPIC_ENTRY_MASKED);
    }

    /// The vector and the destination APIC ID the entry is routed to now,
    /// in compatibility format.
    pub(crate) fn routed(self) -> (u8, u32) {
        let entry = self.redirection();
        ((entry & 0xFF) as u8, (entry >> 56) as u32)
    }

    /// Write the whole redirection entry `entry`, high word first and the
    /// low word -- which holds the mask bit -- last, so an unmasked entry is
    /// never live half written.
    pub(crate) fn write(self, entry: u64) {
        let low_register = IOAPIC_REDIRECTION_BASE + self.input * 2;
        io_apic_write(self.regs, low_register + 1, (entry >> 32) as u32);
        io_apic_write(self.regs, low_register, entry as u32);
    }
}

/// How many redirection entries an I/O APIC has.
fn input_count(regs: Mmio) -> u32 {
    ((io_apic_read(regs, IOAPIC_VERSION_REGISTER) >> 16) & 0xFF).saturating_add(1)
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
    for input in 0..input_count(regs) {
        let low = IOAPIC_REDIRECTION_BASE.saturating_add(input.saturating_mul(2));
        io_apic_write(regs, low, IOAPIC_ENTRY_MASKED);
    }
}

/// Bring up this processor's local APIC, on a processor other than the one
/// that ran [`init`].
///
/// Every processor has its own, at the same address: the window `init` mapped
/// reaches whichever local APIC belongs to the processor reading it. The timer
/// is left masked, with the divider `init` calibrated against — the rate it
/// measured holds for every processor sharing the crystal, which on every
/// machine this runs on is all of them.
pub(crate) fn init_this_cpu() {
    write_reg(LAPIC_TPR, 0);
    write_reg(LAPIC_SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);
    write_reg(LAPIC_TIMER_DCR, DCR_DIVIDE_BY_16);
    write_reg(LAPIC_LVT_TIMER, TIMER_VECTOR as u32 | LVT_MASKED);
    if let Some(flag) = lvt_unmasked() {
        flag.store(false, Ordering::Relaxed);
    }
}

/// Reset processor `apic_id` into waiting for a start-up IPI.
pub(crate) fn send_init(apic_id: u32) -> Result<(), &'static str> {
    send(apic_id, ICR_INIT | ICR_ASSERT)
}

/// Start processor `apic_id` in real mode at the beginning of page `page`.
pub(crate) fn send_startup(apic_id: u32, page: u8) -> Result<(), &'static str> {
    send(apic_id, ICR_STARTUP | ICR_ASSERT | u32::from(page))
}

/// Interrupt every processor but this one on [`IPI_VECTOR`].
///
/// One write, with the destination shorthand doing the addressing — which is
/// also why this needs no list of who is online: a processor still waiting
/// for its start-up IPI ignores a fixed interrupt, and one with interrupts
/// masked takes it when it unmasks.
///
/// No fence before it in xAPIC mode: the register is uncached memory, and x86
/// does not let an uncached store pass the ordinary stores before it. In
/// x2APIC mode the register is an MSR, and [`issue`] fences.
pub(crate) fn send_ipi_to_others() -> Result<(), &'static str> {
    issue(None, ICR_ALL_BUT_SELF | ICR_ASSERT | IPI_VECTOR as u32)
}

/// Interrupt processor `apic_id` alone on [`IPI_VECTOR`]: fixed delivery,
/// physical destination.
///
/// What a scoped TLB shootdown sends, so that a processor that holds none of
/// an address space's translations is not woken to say so. This processor
/// may name itself; the interrupt is taken when it next unmasks.
pub(crate) fn send_ipi_to(apic_id: u32) -> Result<(), &'static str> {
    send(apic_id, ICR_ASSERT | IPI_VECTOR as u32)
}

/// Raise a non-maskable interrupt on this processor, through its own local
/// APIC: addressed to its own identifier, since the "self" shorthand is
/// defined for fixed delivery only.
///
/// For the boot check that an NMI taken in the kernel is survived. The
/// interrupt may arrive before this returns -- `cli` does not hold it off.
pub fn send_nmi_to_self() -> Result<(), &'static str> {
    send(id(), ICR_NMI | ICR_ASSERT)
}

/// Raise a non-maskable interrupt on processor `apic_id`.
///
/// For the boot check that an NMI taken in a program is survived, whatever
/// `GS` base the program holds (`paranoid::check`).
pub fn send_nmi_to(apic_id: u32) -> Result<(), &'static str> {
    send(apic_id, ICR_NMI | ICR_ASSERT)
}

/// Send `command` to the local APIC `apic_id` and wait for it to be accepted.
fn send(apic_id: u32, command: u32) -> Result<(), &'static str> {
    if x2apic() {
        return issue(Some(apic_id), command);
    }
    if apic_id > 0xFF {
        return Err("an APIC ID above 255 needs x2APIC mode, which this boot does not run in");
    }
    issue(Some(apic_id << 24), command)
}

/// Write an interrupt command, with its destination if it has one, and wait
/// for the local APIC to accept it.
fn issue(destination: Option<u32>, command: u32) -> Result<(), &'static str> {
    crate::sched::trip::count(crate::sched::trip::Count::Ipi);
    if x2apic() {
        // One 64-bit write sends, with the whole x2APIC ID as destination,
        // and there is no delivery status to wait on. `WRMSR` to an x2APIC
        // register is not serializing, so the stores before it -- a
        // shootdown's request, say -- are fenced to be seen first (SDM
        // 11.12.3: `MFENCE; LFENCE`).
        core::sync::atomic::fence(Ordering::SeqCst);
        // SAFETY: (SYSREG) SSE2, which `LFENCE` needs, is part of x86-64.
        unsafe { core::arch::x86_64::_mm_lfence() };
        let value = u64::from(destination.unwrap_or(0)) << 32 | u64::from(command);
        // SAFETY: (SYSREG) x2APIC mode: the interrupt command register's
        // MSR, written with a command this module built.
        unsafe { super::cpu::write_msr(X2APIC_ICR, value) };
        return Ok(());
    }
    // Masked, because a command can be two writes: an interrupt handler that
    // sent one of its own between them would send it to this destination.
    let saved = <super::Irq as IrqControl>::disable();
    if let Some(high) = destination {
        write_reg(LAPIC_ICR_HIGH, high);
    }
    write_reg(LAPIC_ICR_LOW, command);
    let mut accepted = false;
    for _ in 0..1_000_000 {
        if read_reg(LAPIC_ICR_LOW) & ICR_PENDING == 0 {
            accepted = true;
            break;
        }
        spin_loop();
    }
    <super::Irq as IrqControl>::restore(saved);

    if accepted {
        Ok(())
    } else {
        Err("the local APIC never accepted an inter-processor interrupt")
    }
}

/// This processor's local APIC identifier.
///
/// Read from the local APIC rather than from `CPUID`: it is the number an
/// inter-processor interrupt is addressed to, and the register is the
/// authority on that, where `CPUID` leaf 1 reports what it was at reset.
///
/// Zero before [`init`] has mapped the local APIC, which is also a valid
/// identifier — so callers must not ask until it has.
pub(crate) fn id() -> u32 {
    if x2apic() {
        // x2APIC mode: the ID register holds the whole 32-bit x2APIC ID.
        return read_reg(LAPIC_ID);
    }
    read_reg(LAPIC_ID) >> 24
}

/// Retire the interrupt currently being serviced.
pub(crate) fn end_of_interrupt() {
    // Zero in either mode: x2APIC mode faults on anything else.
    write_reg(LAPIC_EOI, 0);
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

    // The vector is unmasked once and left so: rewriting it at every arm was
    // a second exit under a hypervisor for a register that had not changed.
    // Unknown (no per-CPU record yet) is written, as before.
    let unmasked = lvt_unmasked();
    if !unmasked.is_some_and(|flag| flag.swap(true, Ordering::Relaxed)) {
        write_reg(LAPIC_LVT_TIMER, TIMER_VECTOR as u32);
    }
    write_reg(LAPIC_TIMER_ICR, count);
}

/// Stop the timer.
pub(crate) fn disarm() {
    write_reg(LAPIC_TIMER_ICR, 0);
    write_reg(LAPIC_LVT_TIMER, TIMER_VECTOR as u32 | LVT_MASKED);
    if let Some(flag) = lvt_unmasked() {
        flag.store(false, Ordering::Relaxed);
    }
}

/// Whether each processor's timer vector is unmasked, as [`arm`] left it.
static LVT_UNMASKED: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

/// This processor's flag in [`LVT_UNMASKED`], once processors have records.
fn lvt_unmasked() -> Option<&'static AtomicBool> {
    crate::smp::this_cpu().and_then(|cpu| LVT_UNMASKED.get(cpu.logical))
}

/// How fast the timer counts, for the boot log.
pub(crate) fn timer_hz() -> u64 {
    TIMER_HZ.load(Ordering::Relaxed)
}
