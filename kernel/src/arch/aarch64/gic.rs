//! The Generic Interrupt Controller, version 2.
//!
//! Two register blocks, and the split matters. The **distributor** is
//! machine-wide: it decides which CPU an interrupt goes to and whether it is
//! enabled at all. The **CPU interface** is per-core: it is what the core
//! reads to find out which interrupt arrived and writes to say it is done.
//!
//! Three kinds of interrupt, distinguished only by number, which is why the
//! constants below are worth naming: 0..16 are software-generated, 16..32 are
//! *private* peripherals — each core has its own, and the architected timer is
//! one — and 32 upwards are shared peripherals.
//!
//! GICv3 replaces the CPU interface with system registers and adds a
//! redistributor per core. It is not here yet: `init` reports the version it
//! found and refuses one it cannot drive, rather than writing GICv2 layouts
//! into GICv3 registers and producing a machine that takes no interrupts for
//! reasons nothing explains.

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_acpi::{Acpi, MadtEntry};

use crate::acpi::DirectMap;
use crate::mmio::Mmio;

/// Distributor control.
const GICD_CTLR: u64 = 0x000;
/// Distributor type, with the interrupt count in its low five bits.
const GICD_TYPER: u64 = 0x004;
/// Set-enable, one bit per interrupt.
const GICD_ISENABLER: u64 = 0x100;
/// Clear-enable, one bit per interrupt.
const GICD_ICENABLER: u64 = 0x180;
/// Priority, one byte per interrupt.
const GICD_IPRIORITYR: u64 = 0x400;
/// Bytes of register window the distributor occupies.
const GICD_WINDOW: u64 = 0x1000;

/// CPU interface control.
const GICC_CTLR: u64 = 0x000;
/// Priority mask: interrupts at a numerically higher priority are not
/// delivered. Zero — the reset value — means none are.
const GICC_PMR: u64 = 0x004;
/// Interrupt acknowledge. Reading it claims the interrupt.
const GICC_IAR: u64 = 0x00C;
/// End of interrupt. The value read from `GICC_IAR` goes back here.
const GICC_EOIR: u64 = 0x010;
/// Bytes of register window the CPU interface occupies.
const GICC_WINDOW: u64 = 0x2000;

/// Enable the controller.
const CTLR_ENABLE: u32 = 1 << 0;

/// Let every priority through.
const PMR_ALL: u32 = 0xFF;

/// A middle priority, which is every interrupt's priority until something
/// needs otherwise. Stage 14 is where these stop being all the same.
const DEFAULT_PRIORITY: u8 = 0xA0;

/// Interrupt identifiers from here up are not real interrupts: 1023 is
/// "spurious", and the rest are reserved. Reading one means the controller had
/// nothing to give, and it must not be acknowledged.
const FIRST_SPECIAL_ID: u32 = 1020;

/// The identifier field of `GICC_IAR`.
const IAR_ID_MASK: u32 = 0x3FF;

/// Distributor registers.
static DISTRIBUTOR: AtomicU64 = AtomicU64::new(0);

/// This core's CPU interface registers.
static CPU_INTERFACE: AtomicU64 = AtomicU64::new(0);

/// The distributor's window.
fn distributor() -> Mmio {
    window(&DISTRIBUTOR)
}

/// The CPU interface's window.
fn cpu_interface() -> Mmio {
    window(&CPU_INTERFACE)
}

/// Turn a stored base address into a window.
fn window(slot: &AtomicU64) -> Mmio {
    match slot.load(Ordering::Relaxed) {
        0 => Mmio::unmapped(),
        base => Mmio::at(base),
    }
}

/// Where the two register blocks are, according to firmware.
struct Layout {
    /// Physical address of the distributor.
    distributor: u64,
    /// Physical address of this core's CPU interface.
    cpu_interface: u64,
    /// The architecture version firmware reports, or zero for "probe it".
    version: u8,
}

/// Read the distributor and CPU interface addresses out of the MADT.
fn layout(acpi: &Acpi<'_, DirectMap>) -> Result<Layout, &'static str> {
    let madt = acpi.madt().map_err(|_| "the machine has no MADT")?;

    let mut distributor = 0;
    let mut cpu_interface = 0;
    let mut version = 0;

    for entry in madt.entries() {
        match entry {
            MadtEntry::Gicd(gicd) => {
                distributor = gicd.physical_base_address;
                version = gicd.gic_version;
            }
            // The first enabled processor is this one: nothing else has been
            // started. Stage 4 reads the rest of them.
            MadtEntry::Gicc(gicc) if cpu_interface == 0 && gicc.is_enabled() => {
                cpu_interface = gicc.physical_base_address;
            }
            _ => {}
        }
    }

    if distributor == 0 {
        return Err("the MADT describes no GIC distributor");
    }
    Ok(Layout {
        distributor,
        cpu_interface,
        version,
    })
}

/// Bring up the controller.
///
/// # Safety
///
/// Must be called once, on the boot CPU, after the vector table is installed
/// and while interrupts are masked: it leaves the controller able to deliver.
pub(crate) unsafe fn init(acpi: &Acpi<'_, DirectMap>) -> Result<u8, &'static str> {
    let layout = layout(acpi)?;

    // Version 0 means firmware declined to say and the OS should probe. On a
    // machine that also gave a CPU interface address, that is a GICv2 layout:
    // GICv3 has no CPU interface to give an address for.
    let version = match layout.version {
        0 if layout.cpu_interface != 0 => 2,
        other => other,
    };
    if version != 2 {
        return Err("this GIC is not a GICv2, and GICv3 support is not written yet");
    }
    if layout.cpu_interface == 0 {
        return Err("the MADT describes a GICv2 with no CPU interface");
    }

    let gicd = crate::vmap::map_device(layout.distributor, GICD_WINDOW)
        .map_err(|_| "could not map the GIC distributor")?;
    let gicc = crate::vmap::map_device(layout.cpu_interface, GICC_WINDOW)
        .map_err(|_| "could not map the GIC CPU interface")?;
    DISTRIBUTOR.store(gicd, Ordering::Relaxed);
    CPU_INTERFACE.store(gicc, Ordering::Relaxed);

    configure(Mmio::at(gicd), Mmio::at(gicc));
    Ok(version)
}

/// Put the controller into a known state: everything off, then enabled.
fn configure(gicd: Mmio, gicc: Mmio) {
    // `GICD_TYPER`'s low five bits give the line count as N, meaning
    // 32 * (N + 1) identifiers.
    let lines = (u64::from(gicd.read32(GICD_TYPER) & 0b1_1111) + 1) * 32;

    gicd.write32(GICD_CTLR, 0);

    // Disable every shared peripheral and give it a defined priority. What
    // firmware left enabled is firmware's business; from here it is ours, and
    // an interrupt nothing has registered for would otherwise arrive as soon
    // as the CPU unmasks.
    let words = lines.div_ceil(32);
    for word in 0..words {
        gicd.write32(GICD_ICENABLER + word * 4, u32::MAX);
    }
    for line in 0..lines {
        gicd.write32(
            GICD_IPRIORITYR + (line & !3),
            u32::from_ne_bytes([DEFAULT_PRIORITY; 4]),
        );
    }

    gicd.write32(GICD_CTLR, CTLR_ENABLE);

    // The priority mask resets to zero, which blocks everything. This is the
    // single most common reason a freshly written GIC driver delivers no
    // interrupts at all.
    gicc.write32(GICC_PMR, PMR_ALL);
    gicc.write32(GICC_CTLR, CTLR_ENABLE);
}

/// Let interrupt `id` through to this core.
pub(crate) fn enable(id: u32) {
    let gicd = distributor();
    let word = u64::from(id / 32) * 4;
    let bit = 1u32 << (id % 32);

    // Priority is per interrupt and one byte wide, so this is a read-modify-
    // write of the word holding it rather than a plain store.
    let register = GICD_IPRIORITYR + u64::from(id & !3);
    let mut priorities = gicd.read32(register).to_ne_bytes();
    if let Some(slot) = priorities.get_mut((id % 4) as usize) {
        *slot = DEFAULT_PRIORITY;
    }
    gicd.write32(register, u32::from_ne_bytes(priorities));

    gicd.write32(GICD_ISENABLER + word, bit);
}

/// Claim the interrupt that arrived, or `None` if there was none.
///
/// The value returned by the controller is kept whole for [`complete`]: the
/// CPU identifier in its upper bits has to go back exactly as it came, and
/// masking it off here is a bug that only shows on a multiprocessor.
pub(crate) fn claim() -> Option<(u32, u32)> {
    let acknowledgement = cpu_interface().read32(GICC_IAR);
    let id = acknowledgement & IAR_ID_MASK;
    if id >= FIRST_SPECIAL_ID {
        return None;
    }
    Some((id, acknowledgement))
}

/// Tell the controller the interrupt has been handled.
pub(crate) fn complete(acknowledgement: u32) {
    cpu_interface().write32(GICC_EOIR, acknowledgement);
}
