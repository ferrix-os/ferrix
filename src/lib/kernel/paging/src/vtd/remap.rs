//! VT-d interrupt remapping: the interrupt remapping table's entries, the
//! remappable-format messages and I/O APIC entries that name them, the
//! fault-recording register's reason, and the rule for when a machine's
//! interrupts are isolated.
//!
//! Reference: Intel Virtualization Technology for Directed I/O, Architecture
//! Specification, "Interrupt Remapping" (5.1) and the fault reasons of
//! 7.1; checked against QEMU's `hw/i386/intel_iommu.c` (`vtd_irte_get`,
//! `vtd_interrupt_remap_msi`) and `hw/intc/ioapic.c`, which the boot tests
//! run.
//!
//! # An entry is written while not present, and edited by one store
//!
//! The kernel writes an entry's high quadword (the source-ID check) first
//! and its low quadword, with `P`, last, while the entry is not present,
//! and invalidates it in the unit's interrupt entry cache before the
//! message that names it is programmed. Vectors are minted once and kept
//! for the life of the machine, so an entry is never freed. The one edit
//! of a present entry is core quarantine's retarget ([`retargeted`],
//! `docs/CORE-QUARANTINE.md` §3.3): the low quadword alone, its
//! destination changed and nothing else, in one aligned 64-bit store, so
//! the unit reads the old entry or the new one with the same source check
//! (the IRTE's layout, VT-d 9.10; the interrupt entry cache invalidation
//! that follows, and its wait, 6.5.2.7). **Any other change to a present
//! entry must clear it first** -- mask the device, clear `P`, invalidate
//! and wait, then rewrite as above -- or write it whole with a 128-bit
//! `cmpxchg16b`; never two plain stores to a present entry.
//!
//! # Posted interrupts are never used
//!
//! `IM` is always 0: an entry remaps, it never posts, whatever the unit's
//! posted-interrupt capability says.

/// One interrupt remapping table entry: its low quadword, then its high
/// one, as the unit reads them.
pub type Entry = [u64; 2];

/// Entries in the kernel's table: one 4 KiB frame of 16-byte entries.
pub const TABLE_ENTRIES: u16 = 256;

/// IRTA's size field for [`TABLE_ENTRIES`]: `2^(S+1)` entries.
const TABLE_SIZE_FIELD: u64 = 7;

/// Low quadword: present.
const PRESENT: u64 = 1 << 0;
/// Low quadword: trigger mode, level when set.
const TRIGGER_LEVEL: u64 = 1 << 4;
/// Low quadword: the vector, bits 23:16.
const VECTOR_SHIFT: u32 = 16;
/// Low quadword: the destination, bits 63:32.
const DESTINATION_SHIFT: u32 = 32;

/// High quadword: source-ID qualifier 00 (all 16 bits compared) and
/// source validation type 01 (verify the requester against `SID`).
const VERIFY_SOURCE: u64 = 0b01 << 18;

/// The first fault reason of interrupt remapping (VT-d 7.1, table 25): a
/// reason from here to [`LAST_INTERRUPT_REASON`] is an interrupt request's,
/// below it a DMA request's.
pub const FIRST_INTERRUPT_REASON: u8 = 0x20;
/// The last interrupt-remapping fault reason the kernel decodes: 0x26, a
/// source ID that failed its entry's check.
pub const LAST_INTERRUPT_REASON: u8 = 0x26;
/// Fault reason 0x22: the entry the request named is not present.
pub const REASON_NOT_PRESENT: u8 = 0x22;
/// Fault reason 0x25: a compatibility-format request, while compatibility
/// format is blocked.
pub const REASON_COMPATIBILITY: u8 = 0x25;
/// Fault reason 0x26: the requester's source ID did not match its entry's.
pub const REASON_SOURCE: u8 = 0x26;

/// What an entry delivers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Remap {
    /// The vector, fixed delivery, to one processor.
    pub vector: u8,
    /// The destination field, from [`destination`]: never built otherwise.
    pub destination: u32,
    /// Whether the request is level-triggered: an I/O APIC input wired so.
    /// A message is always edge.
    pub level: bool,
    /// The source ID the request must arrive with: a PCI function's
    /// requester ID, or an I/O APIC's from the DMAR's scope for it.
    pub source: u16,
}

/// The destination field of an entry aimed at the local APIC `apic_id`, in
/// the xAPIC format `IRTA.EIME` = 0 selects: the ID in bits 15:8. `None` for
/// an ID above 255, which that format cannot name.
///
/// The one place a destination is encoded. **The x2APIC work must set
/// `IRTA.EIME` = 1 and widen this to the full 32 bits.**
#[must_use]
pub const fn destination(apic_id: u32) -> Option<u32> {
    if apic_id > 0xFF {
        None
    } else {
        Some(apic_id << 8)
    }
}

/// The entry for `remap`: present, faults recorded (`FPD` = 0), physical
/// destination (`DM` = 0), no redirection hint, fixed delivery mode, remapped
/// rather than posted (`IM` = 0), and the requester checked against `SID`
/// by all sixteen bits.
#[must_use]
pub const fn entry(remap: Remap) -> Entry {
    let trigger = if remap.level { TRIGGER_LEVEL } else { 0 };
    [
        PRESENT
            | trigger
            | (remap.vector as u64) << VECTOR_SHIFT
            | (remap.destination as u64) << DESTINATION_SHIFT,
        remap.source as u64 | VERIFY_SOURCE,
    ]
}

/// The low quadword `low` of a present entry aimed at `destination` (from
/// [`destination`]) instead, everything else kept: a retarget writes only
/// this quadword, in one aligned store, so the unit reads either the old
/// destination or the new one with the same source check
/// (`docs/CORE-QUARANTINE.md` §3.3).
#[must_use]
pub const fn retargeted(low: u64, destination: u32) -> u64 {
    (low & !((u32::MAX as u64) << DESTINATION_SHIFT)) | (destination as u64) << DESTINATION_SHIFT
}

/// Whether the low quadword of an entry says it is present.
#[must_use]
pub const fn is_present(low: u64) -> bool {
    low & PRESENT != 0
}

/// What `IRTA` holds for a table of [`TABLE_ENTRIES`] at the 4 KiB frame
/// `table`: its address, `EIME` = 0 (xAPIC destinations) and the size.
#[must_use]
pub const fn table_address(table: u64) -> u64 {
    (table & !0xFFF) | TABLE_SIZE_FIELD
}

/// The address of a remappable-format message naming entry `handle`, with
/// no subhandle (`SHV` = 0); its data is 0. Below 4 GiB, as a 32-bit MSI
/// capability needs.
#[must_use]
pub const fn message_address(handle: u16) -> u64 {
    let low = (handle & 0x7FFF) as u64;
    let high = (handle >> 15) as u64;
    0xFEE0_0000 | low << 5 | 1 << 4 | high << 2
}

/// An I/O APIC redirection entry in remappable format naming entry
/// `handle`: format bit 48 set, handle bits 14:0 in 63:49, handle bit 15
/// in bit 11, bits 10:8 clear. Its vector is the entry's own, because an
/// I/O APIC matches an end-of-interrupt to its entries by vector, and its
/// trigger is the entry's.
#[must_use]
pub const fn redirection_entry(
    handle: u16,
    vector: u8,
    level: bool,
    active_low: bool,
    masked: bool,
) -> u64 {
    let low_handle = (handle & 0x7FFF) as u64;
    let high_handle = (handle >> 15) as u64;
    let mut entry = 1 << 48 | low_handle << 49 | high_handle << 11 | vector as u64;
    if active_low {
        entry |= 1 << 13;
    }
    if level {
        entry |= 1 << 15;
    }
    if masked {
        entry |= 1 << 16;
    }
    entry
}

/// Whether a fault record's reason is an interrupt request's.
#[must_use]
pub const fn is_interrupt_reason(reason: u8) -> bool {
    reason >= FIRST_INTERRUPT_REASON && reason <= LAST_INTERRUPT_REASON
}

/// A fault record's reason: bits 7:0 of its dword at offset 12.
#[must_use]
pub const fn fault_reason(flags: u32) -> u8 {
    (flags & 0xFF) as u8
}

/// The interrupt index an interrupt fault record names: bits 63:48 of its
/// low quadword (`FI`).
#[must_use]
pub const fn fault_index(low: u64) -> u16 {
    (low >> 48) as u16
}

/// What one DMAR unit says for the isolation rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnitState {
    /// `GSTS.IRES` read 1.
    pub remapping: bool,
    /// `GSTS.CFIS` read 0 after `IRE`.
    pub compatibility_blocked: bool,
}

/// Whether a machine's interrupts are isolated (`docs/NVIDIA.md` §12.3,
/// condition G4): every unit its DMAR lists -- refused ones included, which
/// block nothing -- remaps with compatibility format blocked, there is at
/// least one, and no PCI function is placed behind no unit.
#[must_use]
pub fn interrupts_isolated(units: &[UnitState], bypassing: usize) -> bool {
    !units.is_empty()
        && bypassing == 0
        && units
            .iter()
            .all(|unit| unit.remapping && unit.compatibility_blocked)
}
