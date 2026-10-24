//! AMD-Vi (the AMD IOMMU): the v1 host page tables a unit walks to translate
//! a device's DMA, and, in the modules below, the device table entries, the
//! commands, the interrupt remapping entries, the event log's records and
//! the rule for which units the kernel drives (`docs/AMD-VI.md`).
//!
//! Reference: AMD I/O Virtualization Technology (IOMMU) Specification,
//! #48882, "I/O Page Tables for Host Translations"; checked against Linux's
//! `drivers/iommu/amd/io_pgtable.c` and QEMU's `fetch_pte`
//! (`hw/i386/amd_iommu.c`), which the boot row runs.
//!
//! # Three levels, 4 KiB pages only
//!
//! A domain is `Mode` 3 in its device table entry: three levels over 39
//! bits, the same reach as a VT-d domain (`TRANSLATED_BITS`), so its root
//! starts at [`Level::GIGABYTE`]. An AMD table entry is present by its `PR`
//! bit; a table entry (`PDE`) names the level of the table it points to in
//! `NextLevel` (bits 11:9), which is why this encoding answers
//! [`Encoding::table_descriptor_at`]; a 4 KiB leaf (`PTE`) has `NextLevel`
//! 0. No larger leaf is ever written ([`Encoding::supports_block`] is false
//! at every level), so the `NextLevel` 7 page-size encoding never appears.
//!
//! `IR` and `IW` are combined by AND down the walk, the device table entry's
//! included, so every table entry grants both and the leaf decides.

use crate::{Encoding, Level, MapFlags, PhysAddr, VirtAddr};

pub mod command;
pub mod event;
pub mod table;
pub mod unit;

#[cfg(test)]
mod tests;

/// Present.
const PRESENT: u64 = 1 << 0;
/// `NextLevel`'s first bit.
const NEXT_LEVEL_SHIFT: u32 = 9;
/// The device may read through this entry.
const READ: u64 = 1 << 61;
/// The device may write through this entry.
const WRITE: u64 = 1 << 62;
/// The bits that hold a physical address: 51:12.
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// AMD-Vi v1 host translation, three levels over 39 bits (`Mode` 3).
#[derive(Clone, Copy, Debug)]
pub struct AmdV1;

/// The AMD level a table at the kernel's `level` is: AMD counts from the
/// leaves up, 1 for the table of 4 KiB entries, so a table at
/// [`Level::PAGE`] is level 1 and the root of a `Mode` 3 walk, at
/// [`Level::GIGABYTE`], is level 3.
#[must_use]
pub const fn amd_level(level: Level) -> u64 {
    4 - level.depth() as u64
}

impl Encoding for AmdV1 {
    const NAME: &'static str = "AMD-Vi v1";

    const ROOT_LEVEL: Level = Level::GIGABYTE;

    const VIRT_BITS: u32 = 39;

    const PHYS_BITS: u32 = 52;

    fn is_canonical(virt: VirtAddr) -> bool {
        virt.0 >> Self::VIRT_BITS == 0
    }

    fn canonical(address: u64) -> u64 {
        address
    }

    /// Never called by the mapper, which calls
    /// [`Encoding::table_descriptor_at`]: a table entry with no level would
    /// point at a level-1 table, the most restrictive answer.
    fn table_descriptor(table: PhysAddr, flags: MapFlags) -> u64 {
        Self::table_descriptor_at(table, Level::MEGABYTE, flags)
    }

    /// The `NextLevel` written is the level of the table pointed to, the one
    /// below `level`'s ([`amd_level`]).
    fn table_descriptor_at(table: PhysAddr, level: Level, _flags: MapFlags) -> u64 {
        let next = match level.next() {
            Some(next) => amd_level(next),
            None => 0,
        };
        (table.0 & ADDRESS_MASK) | PRESENT | next << NEXT_LEVEL_SHIFT | READ | WRITE
    }

    fn leaf_descriptor(frame: PhysAddr, _level: Level, flags: MapFlags) -> u64 {
        let mut entry = (frame.0 & ADDRESS_MASK) | PRESENT;
        if flags.read {
            entry |= READ;
        }
        if flags.write {
            entry |= WRITE;
        }
        entry
    }

    fn is_present(entry: u64) -> bool {
        entry & PRESENT != 0
    }

    /// By level alone (ledger 3867, D2): only the kernel writes these
    /// tables, and it writes no leaf above [`Level::PAGE`].
    fn is_leaf(_entry: u64, level: Level) -> bool {
        level == Level::PAGE
    }

    fn address(entry: u64) -> PhysAddr {
        PhysAddr(entry & ADDRESS_MASK)
    }

    fn supports_block(_level: Level) -> bool {
        false
    }

    fn leaf_flags(entry: u64) -> MapFlags {
        MapFlags {
            read: entry & READ != 0,
            write: entry & WRITE != 0,
            execute: false,
            user: false,
            global: false,
            device: false,
            uncached: false,
            write_combining: false,
        }
    }
}
