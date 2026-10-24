//! The device table's entries, the interrupt remapping tables' entries,
//! and what an MSI or an I/O APIC entry carries to name one
//! (`docs/AMD-VI.md` §1.3, §3).
//!
//! Reference: #48882, "Device Table Entry Format" and "Interrupt
//! Remapping Tables"; checked against QEMU's `amdvi_validate_dte` and
//! `amdvi_int_remap_legacy`, and Linux's `set_dte_entry` and
//! `irq_remapping_prepare_irte`.
//!
//! # Two halves, written apart
//!
//! A device table entry is four quadwords. Quadwords 0 and 1 are its **DMA
//! half** -- valid, translation valid, `Mode`, the page table root, `IR`,
//! `IW`, the domain ID -- and quadword 2 its **interrupt half** -- `IV`,
//! `IntCtl`, `IntTabLen` and the interrupt table's root. Quadword 3 is
//! always 0. An attach or a detach writes the DMA half alone and a
//! function's first vector the interrupt half alone ([`dma_half`],
//! [`interrupt_half`]), so neither can undo the other (ledger 3867,
//! condition 4).
//!
//! # Blocked
//!
//! Every entry starts blocked, both halves, before the unit is enabled. A
//! `V` of 0 would pass DMA through untranslated, and an `IV` of 0 would
//! pass interrupts through unremapped, so neither is ever written. On a
//! translating unit a blocked DMA half is `Mode` 3 over one kernel-held
//! empty root with `IR` and `IW` clear and domain ID 0, which no domain is
//! given: QEMU lets a `Mode` 0 entry pass everything, whatever `IR` and
//! `IW` say, so blocking by `Mode` 0 would block on silicon only (ledger
//! 3867, condition 1). On a unit that only remaps, the DMA half is `Mode`
//! 0 with `IR` and `IW` set: untranslated, as a VT-d unit with `TE` off.

/// Quadword 0: valid.
const VALID: u64 = 1 << 0;
/// Quadword 0: translation information valid.
const TRANSLATION_VALID: u64 = 1 << 1;
/// Quadword 0: the `Mode` field's first bit.
const MODE_SHIFT: u32 = 9;
/// Quadword 0: a three-level walk.
const MODE_THREE: u64 = 3 << MODE_SHIFT;
/// Quadword 0: the page table root's bits, 51:12.
const ROOT_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Quadword 0: reads allowed.
const READ: u64 = 1 << 61;
/// Quadword 0: writes allowed.
const WRITE: u64 = 1 << 62;
/// Quadword 1: the domain ID, bits 15:0.
const DOMAIN_MASK: u64 = 0xFFFF;

/// Quadword 2: interrupt information valid.
const INTERRUPTS_VALID: u64 = 1 << 0;
/// Quadword 2: `IntTabLen`, bits 4:1, the table's entries as a power of two.
const TABLE_LENGTH_SHIFT: u32 = 1;
/// Quadword 2: the interrupt table root's bits, 51:6.
const TABLE_ROOT_MASK: u64 = 0x000F_FFFF_FFFF_FFC0;
/// Quadword 2: `IntCtl`, bits 61:60; 10b remaps fixed and arbitrated
/// requests, 00b aborts them.
const REMAPPED: u64 = 2 << 60;

/// A function's interrupt remapping table: 512 entries, the largest every
/// unit takes without `NumIntRemapSup` (Linux's `MAX_IRQS_PER_TABLE_512`).
pub const TABLE_ENTRIES: u16 = 512;
/// `IntTabLen` for [`TABLE_ENTRIES`].
const TABLE_LENGTH: u64 = 9;
/// Bytes of a table of [`TABLE_ENTRIES`] four-byte entries.
pub const TABLE_BYTES: u64 = TABLE_ENTRIES as u64 * 4;

/// Interrupt remapping entry: remapping enabled.
const REMAP_ENABLED: u32 = 1 << 0;
/// Interrupt remapping entry: the destination, bits 15:8.
const DESTINATION_SHIFT: u32 = 8;
/// Interrupt remapping entry: the vector, bits 23:16.
const VECTOR_SHIFT: u32 = 16;

/// The address every remapped message is written to: the destination bits
/// are not read by the unit, which takes the destination from the entry.
pub const MESSAGE_ADDRESS: u64 = 0xFEE0_0000;

/// A device table entry, as the unit reads its four quadwords.
pub type Entry = [u64; 4];

/// Quadwords 0 and 1 of a blocked entry on a translating unit: `Mode` 3
/// over the empty table at `empty_root`, nothing allowed, domain 0.
#[must_use]
pub const fn blocked_translating(empty_root: u64) -> [u64; 2] {
    [
        VALID | TRANSLATION_VALID | MODE_THREE | (empty_root & ROOT_MASK),
        0,
    ]
}

/// Quadwords 0 and 1 of every entry on a unit that only remaps: no
/// translation, every access allowed.
#[must_use]
pub const fn untranslated() -> [u64; 2] {
    [VALID | TRANSLATION_VALID | READ | WRITE, 0]
}

/// Quadwords 0 and 1 of an attached entry: `Mode` 3 over `root`, reads and
/// writes allowed down to the leaves, in domain `domain`.
#[must_use]
pub const fn attached(root: u64, domain: u16) -> [u64; 2] {
    [
        VALID | TRANSLATION_VALID | MODE_THREE | (root & ROOT_MASK) | READ | WRITE,
        domain as u64 & DOMAIN_MASK,
    ]
}

/// Quadword 2 of a blocked entry: interrupts valid, every fixed and
/// arbitrated request aborted, every pass bit clear (NMI, INIT, `ExtInt` and
/// both LINTs aborted too).
pub const BLOCKED_INTERRUPTS: u64 = INTERRUPTS_VALID;

/// Quadword 2 of an entry whose requests are remapped through the table
/// at `table`, of [`TABLE_ENTRIES`].
#[must_use]
pub const fn remapped(table: u64) -> u64 {
    INTERRUPTS_VALID | TABLE_LENGTH << TABLE_LENGTH_SHIFT | (table & TABLE_ROOT_MASK) | REMAPPED
}

/// A whole blocked entry, both halves: what every entry is written as
/// before the unit is enabled.
#[must_use]
pub const fn blocked(dma: [u64; 2]) -> Entry {
    [dma[0], dma[1], BLOCKED_INTERRUPTS, 0]
}

/// The quadwords of an entry that hold its DMA half.
#[must_use]
pub const fn dma_half(entry: &Entry) -> [u64; 2] {
    [entry[0], entry[1]]
}

/// The quadword of an entry that holds its interrupt half.
#[must_use]
pub const fn interrupt_half(entry: &Entry) -> u64 {
    entry[2]
}

/// Whether the DMA half lets the device reach anything: valid, and either
/// untranslated with an access allowed or translating with one allowed.
#[must_use]
pub const fn dma_allowed(dma: [u64; 2]) -> bool {
    dma[0] & VALID == 0 || dma[0] & (READ | WRITE) != 0
}

/// Whether the interrupt half lets any request through unremapped or
/// remapped: `IV` clear, or `IntCtl` other than abort, or a pass bit set.
#[must_use]
pub const fn interrupts_allowed(interrupts: u64) -> bool {
    interrupts != BLOCKED_INTERRUPTS
}

/// The destination an entry aimed at local APIC `apic_id` holds: the ID
/// itself, in eight bits. `None` above 255, which the 32-bit format cannot
/// name.
///
/// **x2APIC work must move to the 128-bit format (`GAEn`, `XTEn`) and
/// widen this**, as VT-d's `remap::destination` says for `EIME`.
#[must_use]
pub const fn destination(apic_id: u32) -> Option<u8> {
    if apic_id > 0xFF {
        None
    } else {
        Some(apic_id as u8)
    }
}

/// A 32-bit interrupt remapping entry: remapped, its faults reported
/// (`SupIOPF` 0), fixed delivery, physical destination `destination`, not
/// guest mode, `vector`.
#[must_use]
pub const fn interrupt_entry(vector: u8, destination: u8) -> u32 {
    REMAP_ENABLED | (destination as u32) << DESTINATION_SHIFT | (vector as u32) << VECTOR_SHIFT
}

/// Whether a 32-bit entry is in use.
#[must_use]
pub const fn interrupt_entry_used(entry: u32) -> bool {
    entry & REMAP_ENABLED != 0
}

/// The data of a message naming entry `index` of its function's table:
/// the index in bits 10:0, fixed delivery in 10:8 since an index is below
/// 256 here. Its address is [`MESSAGE_ADDRESS`].
#[must_use]
pub const fn message_data(index: u16) -> u32 {
    index as u32 & 0x7FF
}

/// An I/O APIC redirection entry whose request names entry `index` of the
/// I/O APIC's table: compatibility form, the index in the vector field,
/// fixed delivery, physical destination 0 (the unit reads the entry's).
/// An index past 255 cannot be carried, so it answers `None`.
#[must_use]
pub const fn redirection_entry(
    index: u16,
    level: bool,
    active_low: bool,
    masked: bool,
) -> Option<u64> {
    if index > 0xFF {
        return None;
    }
    let mut entry = index as u64;
    if active_low {
        entry |= 1 << 13;
    }
    if level {
        entry |= 1 << 15;
    }
    if masked {
        entry |= 1 << 16;
    }
    Some(entry)
}
