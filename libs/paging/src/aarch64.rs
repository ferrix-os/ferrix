//! AArch64 page table descriptor encoding (VMSAv8-64, 4-level, 4 KiB granule).
//!
//! Reference: Arm Architecture Reference Manual for A-profile, section D8,
//! "The `AArch64` Virtual Memory System Architecture".
//!
//! Unlike x86-64, the cacheability of a mapping is not spelled out in the
//! descriptor: the descriptor carries a three-bit *index* into `MAIR_EL1`, and
//! what that index means is whatever the kernel programmed. [`MAIR_EL1`] below
//! is that agreement, and the loader and kernel must both use it — a table
//! built against one `MAIR` and installed alongside another maps device
//! registers as cacheable memory, which fails in ways that look like hardware
//! faults.

use crate::{Encoding, Level, MapFlags, PhysAddr};

/// Descriptor is valid.
const VALID: u64 = 1 << 0;
/// At levels 0-2, distinguishes a table pointer from a block. At level 3 it is
/// part of the page encoding and must be set.
const TABLE_OR_PAGE: u64 = 1 << 1;

/// Access flag. Hardware faults on a descriptor without it, and Ferrix does not
/// use access-flag faults for aging, so every mapping sets it.
const ACCESS_FLAG: u64 = 1 << 10;
/// Not global: the translation is tagged with an ASID and does not survive an
/// address space switch. Set on user mappings, clear on kernel ones.
const NOT_GLOBAL: u64 = 1 << 11;

/// Privileged execute never.
const PXN: u64 = 1 << 53;
/// Unprivileged execute never.
const UXN: u64 = 1 << 54;

/// Inner shareable. Required for normal memory on an SMP system, or the
/// hardware is not obliged to keep other cores' caches coherent with it.
const SH_INNER: u64 = 0b11 << 8;

/// Access permissions, bits 7:6. `AP[1]` grants EL0 access, `AP[2]` makes it
/// read-only.
const AP_EL1_RW: u64 = 0b00 << 6;
const AP_EL0_RW: u64 = 0b01 << 6;
const AP_EL1_RO: u64 = 0b10 << 6;
const AP_EL0_RO: u64 = 0b11 << 6;

/// `MAIR_EL1` index of normal write-back cacheable memory.
pub const MAIR_NORMAL: u64 = 0;
/// `MAIR_EL1` index of device-nGnRnE memory, the strongest ordering.
pub const MAIR_DEVICE: u64 = 1;
/// `MAIR_EL1` index of normal non-cacheable memory.
pub const MAIR_NORMAL_NC: u64 = 2;

/// The value the loader and kernel must program into `MAIR_EL1`.
///
/// * attr 0 = `0xFF`: normal, inner and outer write-back non-transient,
///   read-allocate and write-allocate.
/// * attr 1 = `0x00`: device-nGnRnE — non-gathering, non-reordering, no early
///   write acknowledgement. What a control register needs.
/// * attr 2 = `0x44`: normal non-cacheable, for buffers shared with a device
///   that is not coherent.
pub const MAIR_EL1: u64 = 0x00_00_00_00_00_44_00_FF;

/// Bits of a descriptor that hold a physical address: 12..47.
const ADDRESS_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Place a `MAIR_EL1` index into a descriptor's `AttrIndx` field, bits 4:2.
const fn attr_index(index: u64) -> u64 {
    (index & 0b111) << 2
}

/// `AArch64` paging.
#[derive(Clone, Copy, Debug)]
pub struct AArch64;

impl Encoding for AArch64 {
    const NAME: &'static str = "AArch64";

    fn table_descriptor(table: PhysAddr, _flags: MapFlags) -> u64 {
        // A table descriptor carries its own permission fields (APTable,
        // UXNTable, PXNTable, NSTable) in bits 59..63, which further *restrict*
        // everything below. Left at zero they restrict nothing, so unlike
        // x86-64 there is no need to propagate the leaf's permissions upwards.
        (table.0 & ADDRESS_MASK) | VALID | TABLE_OR_PAGE
    }

    fn leaf_descriptor(frame: PhysAddr, level: Level, flags: MapFlags) -> u64 {
        let mut entry = (frame.0 & ADDRESS_MASK) | VALID | ACCESS_FLAG;

        // A level-3 descriptor is a page and sets bit 1; a block at level 1 or
        // 2 leaves it clear. This is the exact inverse of the x86-64 rule and
        // is a favourite way to produce a table that translates to nowhere.
        if level == Level::PAGE {
            entry |= TABLE_OR_PAGE;
        }

        entry |= match (flags.user, flags.write) {
            (false, true) => AP_EL1_RW,
            (false, false) => AP_EL1_RO,
            (true, true) => AP_EL0_RW,
            (true, false) => AP_EL0_RO,
        };

        if flags.device {
            entry |= attr_index(MAIR_DEVICE);
            // Device memory is outer shareable by definition of its type; the
            // shareability field is ignored, so leave it clear.
        } else {
            entry |= attr_index(MAIR_NORMAL) | SH_INNER;
        }

        if !flags.global {
            entry |= NOT_GLOBAL;
        }

        // Execute permission is per privilege level, and the safe default is
        // that neither may execute what it does not own. A kernel mapping is
        // never executable from EL0, and a user mapping is never executable
        // from EL1 — the latter is what stops the kernel being tricked into
        // running user code, which x86-64 spells SMEP.
        if flags.user {
            entry |= PXN;
            if !flags.execute {
                entry |= UXN;
            }
        } else {
            entry |= UXN;
            if !flags.execute {
                entry |= PXN;
            }
        }

        entry
    }

    fn is_present(entry: u64) -> bool {
        entry & VALID != 0
    }

    fn is_leaf(entry: u64, level: Level) -> bool {
        if level == Level::PAGE {
            // At level 3 only bits 0b11 are a valid page; 0b01 is reserved.
            return true;
        }
        entry & TABLE_OR_PAGE == 0
    }

    fn address(entry: u64) -> PhysAddr {
        PhysAddr(entry & ADDRESS_MASK)
    }

    fn supports_block(level: Level) -> bool {
        // 1 GiB at level 1 and 2 MiB at level 2, as on x86-64. A level-0 block
        // needs FEAT_LPA2, which we do not require.
        level == Level::GIGABYTE || level == Level::MEGABYTE
    }

    fn leaf_flags(entry: u64) -> MapFlags {
        let access = entry & (0b11 << 6);
        let user = access == AP_EL0_RW || access == AP_EL0_RO;
        let write = access == AP_EL1_RW || access == AP_EL0_RW;

        // Execute permission is asked of the privilege level the mapping is
        // for, which is the same asymmetry the encoder writes: a user mapping
        // is executable when UXN is clear, a kernel one when PXN is.
        let execute = if user {
            entry & UXN == 0
        } else {
            entry & PXN == 0
        };

        MapFlags {
            read: true,
            write,
            execute,
            user,
            global: entry & NOT_GLOBAL == 0,
            device: (entry >> 2) & 0b111 == MAIR_DEVICE,
        }
    }
}
