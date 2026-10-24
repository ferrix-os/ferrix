//! The commands the kernel puts in an AMD-Vi unit's command buffer, and the
//! buffer's and the event log's register encodings (`docs/AMD-VI.md` §1.5).
//!
//! Reference: #48882, "Commands"; checked against QEMU's `amdvi_cmdbuf_exec`
//! and its per-command checks of reserved fields, and Linux's
//! `build_completion_wait`, `build_inv_dte`, `build_inv_iommu_pages`,
//! `build_inv_irt` and `build_inv_all`.

/// One command: two quadwords, its opcode in bits 63:60 of the first.
pub type Command = [u64; 2];

/// Bytes of a command, and of an event log record.
pub const ENTRY_BYTES: u64 = 16;
/// Commands in the buffer, and records in the log: 256, one 4 KiB frame.
pub const RING_ENTRIES: u32 = 256;
/// `ComLen` and `EventLen` for [`RING_ENTRIES`]: a power of two.
const RING_LENGTH: u64 = 8;

/// The opcode's first bit.
const OPCODE_SHIFT: u32 = 60;
/// `COMPLETION_WAIT`.
const COMPLETION_WAIT: u64 = 0x01;
/// `INVALIDATE_DEVTAB_ENTRY`.
const INVALIDATE_DEVICE_ENTRY: u64 = 0x02;
/// `INVALIDATE_IOMMU_PAGES`.
const INVALIDATE_PAGES: u64 = 0x03;
/// `INVALIDATE_INTERRUPT_TABLE`.
const INVALIDATE_INTERRUPT_TABLE: u64 = 0x05;
/// `INVALIDATE_IOMMU_ALL`.
const INVALIDATE_ALL: u64 = 0x08;

/// `COMPLETION_WAIT`: store the data at the address.
const STORE: u64 = 1 << 0;
/// The store address's bits, 51:3.
const STORE_MASK: u64 = 0x000F_FFFF_FFFF_FFF8;
/// `INVALIDATE_IOMMU_PAGES`: the address names a range, here every page.
const SIZE: u64 = 1 << 0;
/// `INVALIDATE_IOMMU_PAGES`: table entries too, not only leaves.
const PDE: u64 = 1 << 1;
/// The address that, with [`SIZE`], names every page of a domain.
const EVERY_PAGE: u64 = 0x7FFF_FFFF_FFFF_F000;

/// How a completion wait completes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Completion {
    /// It stores `data` at `address`, which the kernel waits for.
    Store {
        /// Physical address of an eight-byte-aligned word.
        address: u64,
        /// The sequence number stored.
        data: u64,
    },
    /// It stores nothing: check R7's way of making a wait fail.
    Unwritten,
}

/// `COMPLETION_WAIT`.
#[must_use]
pub const fn completion_wait(completion: Completion) -> Command {
    match completion {
        Completion::Store { address, data } => [
            COMPLETION_WAIT << OPCODE_SHIFT | (address & STORE_MASK) | STORE,
            data,
        ],
        Completion::Unwritten => [COMPLETION_WAIT << OPCODE_SHIFT, 0],
    }
}

/// `INVALIDATE_DEVTAB_ENTRY` for `devid`.
#[must_use]
pub const fn invalidate_device_entry(devid: u16) -> Command {
    [INVALIDATE_DEVICE_ENTRY << OPCODE_SHIFT | devid as u64, 0]
}

/// `INVALIDATE_IOMMU_PAGES` for every page of `domain`, table entries
/// included.
#[must_use]
pub const fn invalidate_domain(domain: u16) -> Command {
    [
        INVALIDATE_PAGES << OPCODE_SHIFT | (domain as u64) << 32,
        EVERY_PAGE | PDE | SIZE,
    ]
}

/// `INVALIDATE_INTERRUPT_TABLE` for `devid`.
#[must_use]
pub const fn invalidate_interrupt_table(devid: u16) -> Command {
    [INVALIDATE_INTERRUPT_TABLE << OPCODE_SHIFT | devid as u64, 0]
}

/// `INVALIDATE_IOMMU_ALL`.
#[must_use]
pub const fn invalidate_all() -> Command {
    [INVALIDATE_ALL << OPCODE_SHIFT, 0]
}

/// A command whose opcode no unit defines (0): what stage 10's check plants
/// to see the unit refuse one.
#[must_use]
pub const fn illegal() -> Command {
    [0, 0]
}

/// What the command buffer's or the event log's base register holds for a
/// ring of [`RING_ENTRIES`] at the 4 KiB frame `frame`.
#[must_use]
pub const fn ring_base(frame: u64) -> u64 {
    (frame & 0x000F_FFFF_FFFF_F000) | RING_LENGTH << 56
}

/// What the device table base register holds for a table of 65,536
/// entries (2 MiB) at `table`: `Size` 511, in 4 KiB units less one.
#[must_use]
pub const fn device_table_base(table: u64) -> u64 {
    (table & 0x000F_FFFF_FFFF_F000) | 511
}

/// A head or tail register's value for `slot`: its byte offset, bits 18:4.
#[must_use]
pub const fn slot_register(slot: u32) -> u64 {
    (slot as u64 * ENTRY_BYTES) & 0x7_FFF0
}

/// The slot a head or tail register names.
#[must_use]
pub const fn slot_of(register: u64) -> u32 {
    ((register & 0x7_FFF0) / ENTRY_BYTES) as u32 % RING_ENTRIES
}

/// Free slots between `head`, the next the unit reads, and `tail`, the next
/// the kernel writes: one is always left empty, so a full ring is not read
/// as an empty one.
#[must_use]
pub const fn free_slots(head: u32, tail: u32) -> u32 {
    (head + RING_ENTRIES - tail - 1) % RING_ENTRIES
}
