//! Whether the kernel drives an AMD-Vi unit, and how, from its EFR and its
//! IVHD alone (`docs/AMD-VI.md` §1.2); and the unit's registers.
//!
//! Reference: #48882, "MMIO Registers"; read against QEMU 10.2's unit
//! (`amdvi_extended_feature_register`) and nazuna's (EFR
//! 0x246577efa2254afa, from its IVHD 11h).
//!
//! # The order of the decision
//!
//! `HATDis` (an IVHD 11h or 40h attribute: no host address translation) is
//! decided first, and a unit with it remaps interrupts only; its `HATS` is
//! not read, because QEMU's `dma-translation=off` unit reports the
//! reserved 11b there (ledger 3867, condition 2). A unit that translates
//! must report a `HATS` other than 11b: every other value allows the three
//! levels a domain uses.

/// Bytes of the unit's registers the kernel maps: every register it uses
/// is below 0x2028.
pub const WINDOW: u64 = 0x4000;
/// Bytes of the unit's registers kept from every driver's apertures:
/// Linux's `MMIO_REG_END_OFFSET`, the performance counters' pages included.
pub const KEPT: u64 = 0x8_0000;

/// Device table base.
pub const DEVICE_TABLE: u64 = 0x0000;
/// Command buffer base.
pub const COMMAND_BASE: u64 = 0x0008;
/// Event log base.
pub const EVENT_BASE: u64 = 0x0010;
/// Control.
pub const CONTROL: u64 = 0x0018;
/// Exclusion range base.
pub const EXCLUSION_BASE: u64 = 0x0020;
/// Exclusion range limit.
pub const EXCLUSION_LIMIT: u64 = 0x0028;
/// Extended features.
pub const EXTENDED_FEATURES: u64 = 0x0030;
/// Command buffer head.
pub const COMMAND_HEAD: u64 = 0x2000;
/// Command buffer tail.
pub const COMMAND_TAIL: u64 = 0x2008;
/// Event log head.
pub const EVENT_HEAD: u64 = 0x2010;
/// Event log tail.
pub const EVENT_TAIL: u64 = 0x2018;
/// Status.
pub const STATUS: u64 = 0x2020;

/// Control: the unit is on.
pub const CONTROL_ENABLE: u64 = 1 << 0;
/// Control: the event log is on.
pub const CONTROL_EVENT_LOG: u64 = 1 << 2;
/// Control: `InvTimeOut`, bits 7:5; 1 is 1 ms.
pub const CONTROL_TIMEOUT_1MS: u64 = 1 << 5;
/// Control: the unit's own memory accesses snoop.
pub const CONTROL_COHERENT: u64 = 1 << 10;
/// Control: the command buffer is on.
pub const CONTROL_COMMANDS: u64 = 1 << 12;
/// Control: what the kernel turns on, and nothing else: no event or
/// completion interrupt, no PPR, no guest translation, no GA, no x2APIC.
pub const CONTROL_STANDING: u64 =
    CONTROL_ENABLE | CONTROL_EVENT_LOG | CONTROL_TIMEOUT_1MS | CONTROL_COHERENT | CONTROL_COMMANDS;
/// Control: the bits a unit firmware left on shows.
pub const CONTROL_FIRMWARE: u64 = CONTROL_ENABLE | CONTROL_EVENT_LOG | CONTROL_COMMANDS;

/// Status: the event log overflowed.
pub const STATUS_EVENT_OVERFLOW: u64 = 1 << 0;
/// Status: the event log is running.
pub const STATUS_EVENT_RUNNING: u64 = 1 << 3;
/// Status: the command buffer is running.
pub const STATUS_COMMANDS_RUNNING: u64 = 1 << 4;

/// EFR: `INVALIDATE_IOMMU_ALL` supported.
const EFR_IA: u64 = 1 << 6;
/// EFR: `HATS`, bits 11:10.
const EFR_HATS_SHIFT: u32 = 10;
/// `HATS`' reserved value.
const HATS_RESERVED: u64 = 3;

/// What the kernel drives a unit with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Accepted {
    /// Whether its domains translate: false for a unit that only remaps.
    pub translates: bool,
    /// Whether it takes `INVALIDATE_IOMMU_ALL`.
    pub invalidate_all: bool,
}

/// Whether the kernel drives the unit whose EFR is `efr` and whose IVHD
/// says `translation_disabled` (`HATDis`), and how.
///
/// # Errors
///
/// Why not, as a sentence.
pub const fn accept(efr: u64, translation_disabled: bool) -> Result<Accepted, &'static str> {
    let invalidate_all = efr & EFR_IA != 0;
    if translation_disabled {
        return Ok(Accepted {
            translates: false,
            invalidate_all,
        });
    }
    if (efr >> EFR_HATS_SHIFT) & 0x3 == HATS_RESERVED {
        return Err("its EFR reports no host translation level (HATS 11b)");
    }
    Ok(Accepted {
        translates: true,
        invalidate_all,
    })
}

/// Whether a unit firmware left on must be stopped first: its control
/// register shows the unit, its log or its buffer on.
#[must_use]
pub const fn left_on(control: u64) -> bool {
    control & CONTROL_FIRMWARE != 0
}

/// The word that turns `ferrix.amdvi` off, as the command line gives it.
pub const OFF_OPTION: &str = "ferrix.amdvi";

/// Whether the command line's `ferrix.amdvi` value leaves every AMD-Vi
/// unit alone: `off` exactly; `on`, no option, and anything else do not
/// (L.iommu.98).
#[must_use]
pub fn switched_off(value: Option<&str>) -> bool {
    value == Some("off")
}
