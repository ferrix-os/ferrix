//! Intel VT-d second-level page table encodings: the tables a remapping
//! unit walks to translate a device's DMA, over a 4 KiB granule, three or
//! four levels deep.
//!
//! Reference: Intel Virtualization Technology for Directed I/O, Architecture
//! Specification, "Second-Level Paging Entries"; checked against QEMU's
//! `hw/i386/intel_iommu_internal.h`, which is what the boot test runs.
//!
//! A device's address is an I/O virtual address, not a processor's, and the
//! walk is the one its context entry's address width selects. Three levels
//! translate 39 bits -- `SAGAW` bit 1, the one width QEMU's unit offers at
//! `aw-bits=39`, as the boot tests run it -- and a table built for it starts at
//! [`Level::GIGABYTE`], as ARMv7-A's does: [`VtdSecondLevel`]. Four levels
//! translate 48 bits -- `SAGAW` bit 2, the only width the N150's units
//! walk, and one QEMU offers beside three at `aw-bits=48` -- and start at
//! [`Level::ROOT`]: [`VtdFourLevel`]. Both use the same entries; only the
//! depth differs. Which one a unit gets is `vtd::unit::accept`'s choice.
//! Nothing is sign extended: an I/O virtual address is a plain number.
//!
//! A descriptor says only what the device may do, read or write, so the
//! flags a processor mapping carries for its own sake — user, global,
//! execute, device memory — have no bit here, and
//! [`Encoding::leaf_flags`] reads them back as clear.
//!
//! A 2 MiB or 1 GiB leaf is legal only on a unit whose capability register
//! reports that page size, which the kernel checks before a domain's mapper
//! is allowed to build one. A 512 GiB leaf does not exist: bit 7 of a
//! four-level walk's top entry is reserved.

use crate::{Encoding, Level, MapFlags, PhysAddr, VirtAddr};

pub mod queue;
pub mod remap;
pub mod rmrr;
pub mod unit;

#[cfg(test)]
mod tests;

/// The device may read through this entry.
const READ: u64 = 1 << 0;
/// The device may write through this entry.
const WRITE: u64 = 1 << 1;
/// At levels 1 and 2, the entry maps a 1 GiB or 2 MiB page rather than
/// pointing at a table.
const SUPERPAGE: u64 = 1 << 7;

/// Bits of a descriptor that hold a physical address: 12..38. The unit's
/// host address width bounds them; QEMU's and the N150's are 39 bits, and a
/// domain never maps a frame above them (`TRANSLATED_BITS` in the kernel).
const ADDRESS_MASK: u64 = 0x0000_007F_FFFF_F000;

/// VT-d second-level translation, three levels over 39 bits.
#[derive(Clone, Copy, Debug)]
pub struct VtdSecondLevel;

/// VT-d second-level translation, four levels over 48 bits, of which a
/// domain uses the low 39: the walk a unit whose `SAGAW` offers four levels
/// is given.
#[derive(Clone, Copy, Debug)]
pub struct VtdFourLevel;

/// What a table entry holds: the table, and both permissions. What a device
/// may do is the intersection of every entry on the walk, so a table entry
/// grants both and the leaf decides.
const fn table_entry(table: PhysAddr) -> u64 {
    (table.0 & ADDRESS_MASK) | READ | WRITE
}

/// What a leaf at `level` holds.
const fn leaf_entry(frame: PhysAddr, level: Level, flags: MapFlags) -> u64 {
    let mut entry = frame.0 & ADDRESS_MASK;
    if flags.read {
        entry |= READ;
    }
    if flags.write {
        entry |= WRITE;
    }
    if level.depth() != Level::PAGE.depth() {
        entry |= SUPERPAGE;
    }
    entry
}

/// What a leaf permits, as flags: read and write, nothing else.
const fn flags_of(entry: u64) -> MapFlags {
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

/// The `Encoding` of a VT-d walk starting at `$root` over `$bits` bits.
macro_rules! second_level {
    ($walk:ty, $name:literal, $root:expr, $bits:literal) => {
        impl Encoding for $walk {
            const NAME: &'static str = $name;

            const ROOT_LEVEL: Level = $root;

            const VIRT_BITS: u32 = $bits;

            const PHYS_BITS: u32 = 39;

            fn is_canonical(virt: VirtAddr) -> bool {
                virt.0 >> Self::VIRT_BITS == 0
            }

            fn canonical(address: u64) -> u64 {
                address
            }

            fn table_descriptor(table: PhysAddr, _flags: MapFlags) -> u64 {
                table_entry(table)
            }

            fn leaf_descriptor(frame: PhysAddr, level: Level, flags: MapFlags) -> u64 {
                leaf_entry(frame, level, flags)
            }

            fn is_present(entry: u64) -> bool {
                // There is no valid bit: an entry that permits nothing is not
                // there.
                entry & (READ | WRITE) != 0
            }

            fn is_leaf(entry: u64, level: Level) -> bool {
                level == Level::PAGE || entry & SUPERPAGE != 0
            }

            fn address(entry: u64) -> PhysAddr {
                PhysAddr(entry & ADDRESS_MASK)
            }

            fn supports_block(level: Level) -> bool {
                level == Level::GIGABYTE || level == Level::MEGABYTE
            }

            fn leaf_flags(entry: u64) -> MapFlags {
                flags_of(entry)
            }
        }
    };
}

second_level!(VtdSecondLevel, "VT-d second level", Level::GIGABYTE, 39);
second_level!(
    VtdFourLevel,
    "VT-d second level, four levels",
    Level::ROOT,
    48
);
