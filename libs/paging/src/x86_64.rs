//! x86-64 page table descriptor encoding (IA-32e paging, 4-level, 4 KiB
//! granule).
//!
//! Reference: Intel SDM Volume 3A, section 4.5, "4-Level Paging and 5-Level
//! Paging".

use crate::{Encoding, Level, MapFlags, PhysAddr};

/// Descriptor is valid.
const PRESENT: u64 = 1 << 0;
/// Writes are permitted.
const WRITABLE: u64 = 1 << 1;
/// Reachable from ring 3.
const USER: u64 = 1 << 2;
/// Write-through rather than write-back caching.
const WRITE_THROUGH: u64 = 1 << 3;
/// Caching disabled.
const CACHE_DISABLE: u64 = 1 << 4;
/// Set by hardware on first access. Pre-set here so the CPU does not have to
/// take a microfault to set it on a mapping we know is about to be used.
const ACCESSED: u64 = 1 << 5;
/// Set by hardware on first write.
const DIRTY: u64 = 1 << 6;
/// At levels 1 and 2, marks a block mapping rather than a table pointer.
const PAGE_SIZE_BIT: u64 = 1 << 7;
/// Translation survives a `CR3` write. Only meaningful with `CR4.PGE`.
const GLOBAL: u64 = 1 << 8;
/// Instruction fetches fault. Requires `EFER.NXE`.
const NO_EXECUTE: u64 = 1 << 63;

/// Bits of a descriptor that hold a physical address.
///
/// Bits 12..51. Masking rather than shifting keeps the address in place, which
/// is what both the hardware and [`Encoding::address`] want.
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// x86-64 paging.
#[derive(Clone, Copy, Debug)]
pub struct X86_64;

impl Encoding for X86_64 {
    const NAME: &'static str = "x86-64";

    fn table_descriptor(table: PhysAddr, flags: MapFlags) -> u64 {
        // Permissions at intermediate levels are ANDed with the leaf's, so an
        // intermediate entry must be at least as permissive as anything below
        // it. It must NOT be blanket-permissive either: leaving USER set on a
        // kernel-only branch would make every leaf under it reachable from
        // ring 3 the moment one of them set USER.
        let mut entry = (table.0 & ADDRESS_MASK) | PRESENT | WRITABLE;
        if flags.user {
            entry |= USER;
        }
        entry
    }

    fn leaf_descriptor(frame: PhysAddr, level: Level, flags: MapFlags) -> u64 {
        let mut entry = (frame.0 & ADDRESS_MASK) | PRESENT | ACCESSED;

        if flags.write {
            entry |= WRITABLE | DIRTY;
        }
        if flags.user {
            entry |= USER;
        }
        if flags.global {
            entry |= GLOBAL;
        }
        if flags.device {
            entry |= CACHE_DISABLE | WRITE_THROUGH;
        }
        if !flags.execute {
            entry |= NO_EXECUTE;
        }
        // At level 3 bit 7 is the PAT bit, not a size bit; a 4 KiB leaf must
        // leave it clear or it selects a different memory type.
        if level != Level::PAGE {
            entry |= PAGE_SIZE_BIT;
        }
        entry
    }

    fn is_present(entry: u64) -> bool {
        entry & PRESENT != 0
    }

    fn is_leaf(entry: u64, level: Level) -> bool {
        level == Level::PAGE || entry & PAGE_SIZE_BIT != 0
    }

    fn address(entry: u64) -> PhysAddr {
        PhysAddr(entry & ADDRESS_MASK)
    }

    fn supports_block(level: Level) -> bool {
        // 1 GiB at level 1 and 2 MiB at level 2. There is no 512 GiB page.
        level == Level::GIGABYTE || level == Level::MEGABYTE
    }

    fn leaf_flags(entry: u64) -> MapFlags {
        MapFlags {
            // A present descriptor is readable; there is no read bit to
            // consult, which is why `MapFlags::read` documents itself as
            // intent rather than a switch.
            read: true,
            write: entry & WRITABLE != 0,
            execute: entry & NO_EXECUTE == 0,
            user: entry & USER != 0,
            global: entry & GLOBAL != 0,
            device: entry & CACHE_DISABLE != 0,
        }
    }
}
