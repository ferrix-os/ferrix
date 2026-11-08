//! ARMv7-A page table descriptor encoding: the Large Physical Address
//! Extension's long-descriptor format, three levels over a 4 KiB granule.
//!
//! Reference: Arm Architecture Reference Manual, ARMv7-A and ARMv7-R edition,
//! section B3.6, "Long-descriptor translation table format".
//!
//! ARMv7-A has two descriptor formats. The short one is 32 bits wide, has two
//! levels and no privileged-execute-never bit; the long one — LPAE — is the
//! format AArch64 went on to adopt nearly bit for bit. Ferrix uses the long
//! one for exactly that reason: what follows is [`super::aarch64`] with two
//! differences, and the memory attributes are the same agreement, split
//! across two registers instead of held in one.
//!
//! The differences:
//!
//! * the output address is 40 bits rather than 48;
//! * bit 54 is `XN`, execute-never at *every* privilege level, where AArch64
//!   has `UXN`, which binds EL0 only. So a mapping the kernel may execute is
//!   one with both `XN` and `PXN` clear, and a user mapping the kernel must
//!   never execute sets `PXN` and leaves `XN` to say what user mode may do.
//!
//! # The walk is three levels
//!
//! A 32-bit input address needs no level 0: the root is a level-1 table of
//! four entries, one per gibibyte, which is [`Encoding::ROOT_LEVEL`]. The
//! kernel's half is translated through `TTBR1` with `TTBCR.T1SZ = 1`, and the
//! hardware then indexes a *two*-entry table by address bit 30 alone — while
//! this crate indexes the root by bits 31:30, putting `0x8000_0000` in entry
//! 2. The two agree when `TTBR1` holds the root's address plus sixteen bytes,
//! which is what the loader writes and why a test pins it.

use crate::aarch64::{MAIR_DEVICE, MAIR_EL1, MAIR_NORMAL, MAIR_NORMAL_NC};
use crate::{Encoding, Level, MapFlags, PhysAddr, VirtAddr};

/// Descriptor is valid.
const VALID: u64 = 1 << 0;
/// At levels 1 and 2, distinguishes a table pointer from a block. At level 3
/// it is part of the page encoding and must be set.
const TABLE_OR_PAGE: u64 = 1 << 1;

/// Access flag. As on AArch64, a descriptor without it faults on first touch,
/// and Ferrix does not use access-flag faults, so every mapping sets it.
const ACCESS_FLAG: u64 = 1 << 10;
/// Not global: tagged with an ASID. Set on user mappings, clear on kernel ones.
const NOT_GLOBAL: u64 = 1 << 11;

/// Privileged execute never.
const PXN: u64 = 1 << 53;
/// Execute never, at every privilege level.
const XN: u64 = 1 << 54;

/// Inner shareable, which normal memory on a multiprocessor has to be.
const SH_INNER: u64 = 0b11 << 8;

/// Access permissions, bits 7:6 — the same simplified model AArch64 uses.
/// `AP[1]` grants PL0 access, `AP[2]` makes the mapping read-only.
const AP_PL1_RW: u64 = 0b00 << 6;
const AP_PL0_RW: u64 = 0b01 << 6;
const AP_PL1_RO: u64 = 0b10 << 6;
const AP_PL0_RO: u64 = 0b11 << 6;
/// `AP[2]`, the bit that makes any of the four read-only.
const AP_READ_ONLY: u64 = 0b10 << 6;

/// Bits of a descriptor that hold a physical address: 12..39.
const ADDRESS_MASK: u64 = 0x0000_00FF_FFFF_F000;

/// The value the loader and kernel program into `MAIR0`: attributes 0 to 3 of
/// the agreement [`MAIR_EL1`] states for AArch64.
///
/// With `TTBCR.EAE` set, the registers that were `PRRR` and `NMRR` become
/// `MAIR0` and `MAIR1`, and hold the same eight attribute bytes AArch64 keeps
/// in one 64-bit register. Deriving both halves from that one constant is what
/// stops the two architectures' idea of "device memory" drifting apart.
pub const MAIR0: u32 = MAIR_EL1 as u32;

/// The value the loader and kernel program into `MAIR1`: attributes 4 to 7.
pub const MAIR1: u32 = (MAIR_EL1 >> 32) as u32;

/// Place a `MAIR` index into a descriptor's `AttrIndx` field, bits 4:2.
const fn attr_index(index: u64) -> u64 {
    (index & 0b111) << 2
}

/// ARMv7-A paging with the Large Physical Address Extension.
#[derive(Clone, Copy, Debug)]
pub struct Armv7a;

impl Encoding for Armv7a {
    const PHYS_BITS: u32 = 40;

    const NAME: &'static str = "ARMv7-A";

    const ROOT_LEVEL: Level = Level::GIGABYTE;

    const VIRT_BITS: u32 = 32;

    fn is_canonical(virt: VirtAddr) -> bool {
        virt.0 >> 32 == 0
    }

    fn canonical(address: u64) -> u64 {
        // Nothing to extend: the upper half of a 32-bit space is simply the
        // addresses with bit 31 set, and they are compared as such.
        address
    }

    fn table_descriptor(table: PhysAddr, _flags: MapFlags) -> u64 {
        // As on AArch64, the table descriptor's own permission fields only
        // ever restrict, and left at zero restrict nothing.
        (table.0 & ADDRESS_MASK) | VALID | TABLE_OR_PAGE
    }

    fn leaf_descriptor(frame: PhysAddr, level: Level, flags: MapFlags) -> u64 {
        let mut entry = (frame.0 & ADDRESS_MASK) | VALID | ACCESS_FLAG;

        // Bit 1 set at level 3 is a page; clear at levels 1 and 2 is a block.
        if level == Level::PAGE {
            entry |= TABLE_OR_PAGE;
        }

        entry |= match (flags.user, flags.write) {
            (false, true) => AP_PL1_RW,
            (false, false) => AP_PL1_RO,
            (true, true) => AP_PL0_RW,
            (true, false) => AP_PL0_RO,
        };

        if flags.device {
            entry |= attr_index(MAIR_DEVICE);
        } else if flags.uncached || flags.write_combining {
            entry |= attr_index(MAIR_NORMAL_NC) | SH_INNER;
        } else {
            entry |= attr_index(MAIR_NORMAL) | SH_INNER;
        }

        if !flags.global {
            entry |= NOT_GLOBAL;
        }

        // The kernel never executes a user page, which is what x86-64 calls
        // SMEP. Kernel text needs nothing: user mode cannot reach a mapping
        // whose access permissions are PL1-only, execution included.
        if flags.user {
            entry |= PXN;
        }
        if !flags.execute {
            entry |= XN | PXN;
        }

        entry
    }

    fn is_present(entry: u64) -> bool {
        entry & VALID != 0
    }

    fn is_leaf(entry: u64, level: Level) -> bool {
        level == Level::PAGE || entry & TABLE_OR_PAGE == 0
    }

    fn address(entry: u64) -> PhysAddr {
        PhysAddr(entry & ADDRESS_MASK)
    }

    fn supports_block(level: Level) -> bool {
        // 1 GiB at level 1 and 2 MiB at level 2. There is no level 0 to
        // refuse.
        level == Level::GIGABYTE || level == Level::MEGABYTE
    }

    fn write_protected(entry: u64, _level: Level) -> u64 {
        // `AP[2]`, as on AArch64: read-only, `AP[1]` as it was.
        entry | AP_READ_ONLY
    }

    fn write_allowed(entry: u64, _level: Level) -> u64 {
        entry & !AP_READ_ONLY
    }

    fn leaf_flags(entry: u64) -> MapFlags {
        let access = entry & (0b11 << 6);
        let user = access == AP_PL0_RW || access == AP_PL0_RO;
        let write = access == AP_PL1_RW || access == AP_PL0_RW;

        // Asked of the privilege level the mapping is for, as the encoder
        // wrote it: user code needs `XN` clear, kernel code both bits clear.
        let execute = if user {
            entry & XN == 0
        } else {
            entry & (XN | PXN) == 0
        };

        MapFlags {
            read: true,
            write,
            execute,
            user,
            global: entry & NOT_GLOBAL == 0,
            device: (entry >> 2) & 0b111 == MAIR_DEVICE,
            uncached: (entry >> 2) & 0b111 == MAIR_NORMAL_NC,
            write_combining: false,
        }
    }
}
