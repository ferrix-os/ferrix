//! Tests for the page table builder.
//!
//! The mapper is generic over the encoding, so most of what follows runs
//! against all three architectures from one body: a walk that works for x86-64
//! and not for `AArch64` is exactly the failure this crate exists to prevent,
//! and one that works on a four-level walk and not on ARMv7-A's three is the
//! same failure in a different shape. So the generic bodies place every
//! mapping relative to the start of the encoding's upper half, and count
//! tables in levels rather than as a literal. The per-architecture tests below
//! them assert the individual bits, because a descriptor with the right address
//! and the wrong permission bit translates perfectly and protects nothing.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use super::aarch64::{AArch64, MAIR_DEVICE, MAIR_EL1, MAIR_NORMAL, MAIR_NORMAL_NC};
use super::armv7a::{Armv7a, MAIR0, MAIR1};
use super::stage2::ArmStage2;
use super::vtd::VtdSecondLevel;
use super::x86_64::X86_64;
use super::*;

/// Physical memory as a sparse map, so a test can assert on descriptors
/// without owning a machine.
#[derive(Debug, Default)]
struct Memory {
    cells: BTreeMap<u64, u64>,
    next_frame: u64,
    frames: Vec<PhysAddr>,
    /// Descriptors read, for the tests that bound what a walk costs.
    reads: core::cell::Cell<u64>,
}

impl Memory {
    fn new() -> Self {
        Memory {
            cells: BTreeMap::new(),
            // Deliberately not zero: a mapper that confuses "absent" with
            // "frame 0" should fail a test rather than work by accident.
            next_frame: 0x10_0000,
            frames: Vec::new(),
            reads: core::cell::Cell::new(0),
        }
    }

    /// A root table, plus the mapper that owns it.
    fn with_root<E: Encoding>() -> (Self, Mapper<E>) {
        let mut memory = Memory::new();
        let root = memory.allocate_table().unwrap();
        (memory, Mapper::new(root))
    }

    /// Descriptors that have been written, for counting tables.
    fn tables_allocated(&self) -> usize {
        self.frames.len()
    }
}

// SAFETY: this is not real physical memory at all — it is a map, and every
// address the mapper hands back came from `allocate_table` below. Reads and
// writes cannot alias anything, and frames are unique and conceptually zeroed
// because an unwritten cell reads as zero.
unsafe impl PhysMem for Memory {
    fn read(&self, at: PhysAddr) -> u64 {
        assert_eq!(at.0 % 8, 0, "descriptor reads must be eight-byte aligned");
        self.reads.set(self.reads.get() + 1);
        self.cells.get(&at.0).copied().unwrap_or(0)
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        assert_eq!(at.0 % 8, 0, "descriptor writes must be eight-byte aligned");
        let _ = self.cells.insert(at.0, value);
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        let frame = PhysAddr(self.next_frame);
        self.next_frame += PAGE_SIZE;
        self.frames.push(frame);
        Some(frame)
    }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// The first address of the encoding's upper half: `0xFFFF_8000_0000_0000` on
/// a 48-bit geometry and `0x8000_0000` on a 32-bit one.
fn upper<E: Encoding>() -> u64 {
    E::canonical(1 << (E::VIRT_BITS - 1))
}

/// An address `offset` bytes into the upper half. Every offset the generic
/// bodies use is below 2 GiB, which is the whole of ARMv7-A's upper half.
fn high<E: Encoding>(offset: u64) -> VirtAddr {
    VirtAddr(upper::<E>() + offset)
}

/// Tables a walk from the root to a 4 KiB leaf passes through, the root
/// included: four on the 64-bit pair and three on ARMv7-A.
fn levels<E: Encoding>() -> usize {
    usize::from(Level::PAGE.depth() - E::ROOT_LEVEL.depth()) + 1
}

// ---------------------------------------------------------------------------
// Address and level arithmetic
// ---------------------------------------------------------------------------

#[test]
fn level_indices_split_a_virtual_address_the_way_the_hardware_does() {
    // 0x0000_1234_5678_9ABC, split into 9-bit fields from bit 47 down.
    let virt = VirtAddr(0x0000_1234_5678_9ABC);
    assert_eq!(Level::ROOT.index(virt), (0x1234_5678_9ABC >> 39) & 0x1FF);
    assert_eq!(
        Level::GIGABYTE.index(virt),
        (0x1234_5678_9ABC >> 30) & 0x1FF
    );
    assert_eq!(
        Level::MEGABYTE.index(virt),
        (0x1234_5678_9ABC >> 21) & 0x1FF
    );
    assert_eq!(Level::PAGE.index(virt), (0x1234_5678_9ABC >> 12) & 0x1FF);
}

#[test]
fn level_spans_are_the_familiar_page_sizes() {
    assert_eq!(Level::ROOT.span(), 512 << 30);
    assert_eq!(Level::GIGABYTE.span(), 1 << 30);
    assert_eq!(Level::MEGABYTE.span(), 2 << 20);
    assert_eq!(Level::PAGE.span(), 4096);
    assert_eq!(
        Level::PAGE.next(),
        None,
        "there is nothing below the leaves"
    );
    assert_eq!(Level::new(4), None);
}

/// The 64-bit rule, for one encoding.
fn forty_eight_bit_addresses_are_sign_extended<E: Encoding>() {
    assert!(E::is_canonical(VirtAddr(0)));
    assert!(E::is_canonical(VirtAddr(0x0000_7FFF_FFFF_FFFF)));
    assert!(E::is_canonical(VirtAddr(0xFFFF_8000_0000_0000)));
    assert!(E::is_canonical(VirtAddr(0xFFFF_FFFF_8000_0000)));
    assert!(!E::is_canonical(VirtAddr(0x0000_8000_0000_0000)));
    assert!(!E::is_canonical(VirtAddr(0x0001_0000_0000_0000)));
    assert!(!E::is_canonical(VirtAddr(0xFFFF_7FFF_FFFF_FFFF)));
    assert_eq!(E::canonical(0x0000_8000_0000_0000), 0xFFFF_8000_0000_0000);
    assert_eq!(E::canonical(0x0000_7FFF_FFFF_F000), 0x0000_7FFF_FFFF_F000);
}

#[test]
fn canonical_addresses_are_the_two_halves_and_nothing_between() {
    forty_eight_bit_addresses_are_sign_extended::<X86_64>();
    forty_eight_bit_addresses_are_sign_extended::<AArch64>();
}

#[test]
fn a_32_bit_address_is_canonical_exactly_when_it_fits() {
    assert!(Armv7a::is_canonical(VirtAddr(0)));
    assert!(Armv7a::is_canonical(VirtAddr(0x7FFF_FFFF)));
    assert!(
        Armv7a::is_canonical(VirtAddr(0x8000_0000)),
        "a 32-bit space has no hole between its halves"
    );
    assert!(Armv7a::is_canonical(VirtAddr(0xFFFF_FFFF)));
    assert!(!Armv7a::is_canonical(VirtAddr(0x1_0000_0000)));
    assert!(
        !Armv7a::is_canonical(VirtAddr(0xFFFF_FFFF_8000_0000)),
        "a sign-extended 64-bit kernel address is not a 32-bit one"
    );
    assert_eq!(
        Armv7a::canonical(0x8000_0000),
        0x8000_0000,
        "there is nothing above bit 31 to extend into"
    );
}

// ---------------------------------------------------------------------------
// The walk, run against every architecture
// ---------------------------------------------------------------------------

/// A mapping's leaf reads back with the flags it was made with, and an
/// unmapped address has none.
#[test]
fn a_leaf_reads_back_its_flags() {
    let (mut memory, mapper) = Memory::with_root::<X86_64>();
    let at = high::<X86_64>(0x7000_0000);
    let flags = MapFlags {
        write_combining: true,
        ..MapFlags::USER_DATA
    };
    mapper
        .map_range(&mut memory, at, PhysAddr(0x20_0000), PAGE_SIZE, flags)
        .unwrap();
    assert_eq!(mapper.leaf_flags(&memory, at), Some(flags));
    assert_eq!(mapper.leaf_flags(&memory, VirtAddr(at.0 + PAGE_SIZE)), None);
}

/// A 4 KiB mapping resolves, offset and all, and costs one table per level.
fn maps_a_page<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x7000_0000);
    mapper
        .map_range(
            &mut memory,
            at,
            PhysAddr(0x20_0000),
            PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    assert_eq!(
        mapper.translate(&memory, at),
        Some(PhysAddr(0x20_0000)),
        "{}: base of the page",
        E::NAME
    );
    assert_eq!(
        mapper.translate(&memory, VirtAddr(at.0 + 0xFFF)),
        Some(PhysAddr(0x20_0FFF)),
        "{}: the offset within a page must survive translation",
        E::NAME
    );
    assert_eq!(
        mapper.translate(&memory, VirtAddr(at.0 + 0x1000)),
        None,
        "{}: the next page is not mapped",
        E::NAME
    );
    assert_eq!(mapper.mapping_level(&memory, at), Some(Level::PAGE));
    assert_eq!(
        memory.tables_allocated(),
        levels::<E>(),
        "{}: the root and one table per level below it",
        E::NAME
    );
}

/// An aligned 2 MiB range becomes one block rather than 512 pages.
fn maps_a_block<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x4000_0000);
    mapper
        .map_range(
            &mut memory,
            at,
            PhysAddr(0x40_0000),
            2 << 20,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    assert_eq!(
        mapper.mapping_level(&memory, at),
        Some(Level::MEGABYTE),
        "{}: an aligned 2 MiB range should not be 512 pages",
        E::NAME
    );
    assert_eq!(
        mapper.translate(&memory, VirtAddr(at.0 + 0x10_0000)),
        Some(PhysAddr(0x50_0000)),
        "{}: an address inside a block translates by its offset",
        E::NAME
    );
    assert_eq!(
        memory.tables_allocated(),
        levels::<E>() - 1,
        "{}: every table down to level 2, and no leaf table at all",
        E::NAME
    );
}

/// 1 GiB blocks are used only when asked for, because x86-64 may not have them.
fn gigabyte_blocks_are_opt_in<E: Encoding>() {
    let at = high::<E>(0);

    let (mut memory, mapper) = Memory::with_root::<E>();
    mapper
        .map_range(&mut memory, at, PhysAddr(0), 1 << 30, MapFlags::KERNEL_DATA)
        .unwrap();
    assert_eq!(
        mapper.mapping_level(&memory, at),
        Some(Level::MEGABYTE),
        "{}: 1 GiB pages need CPUID.80000001H:EDX.PDPE1GB, so they are opt-in",
        E::NAME
    );

    let (mut memory, mut mapper) = Memory::with_root::<E>();
    mapper.allow_gigabyte_blocks();
    mapper
        .map_range(&mut memory, at, PhysAddr(0), 1 << 30, MapFlags::KERNEL_DATA)
        .unwrap();
    assert_eq!(
        mapper.mapping_level(&memory, at),
        Some(Level::GIGABYTE),
        "{}",
        E::NAME
    );
    assert_eq!(
        memory.tables_allocated(),
        levels::<E>() - 2,
        "{}: every table down to level 1 — on ARMv7-A that is the root alone",
        E::NAME
    );
}

/// A range that starts unaligned uses pages, then blocks, then pages again —
/// and every byte of it resolves.
fn misaligned_ranges_fall_back<E: Encoding>() {
    let (mut memory, mut mapper) = Memory::with_root::<E>();
    mapper.allow_gigabyte_blocks();

    let start = high::<E>(PAGE_SIZE);
    let length = (4 << 20) + PAGE_SIZE;
    mapper
        .map_range(
            &mut memory,
            start,
            PhysAddr(PAGE_SIZE),
            length,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    assert_eq!(
        mapper.mapping_level(&memory, start),
        Some(Level::PAGE),
        "{}: an unaligned head must be paged",
        E::NAME
    );
    assert_eq!(
        mapper.mapping_level(&memory, high::<E>(0x20_0000)),
        Some(Level::MEGABYTE),
        "{}: the aligned middle should still use a block",
        E::NAME
    );

    for offset in (0..length).step_by(PAGE_SIZE as usize) {
        assert_eq!(
            mapper.translate(&memory, VirtAddr(start.0 + offset)),
            Some(PhysAddr(PAGE_SIZE + offset)),
            "{}: gap at offset {offset:#x}",
            E::NAME
        );
    }
}

/// Mapping over something already mapped fails, and changes nothing.
fn refuses_to_overwrite<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x7000_0000);
    mapper
        .map_range(
            &mut memory,
            at,
            PhysAddr(0x1000),
            PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    assert_eq!(
        mapper
            .map_range(
                &mut memory,
                at,
                PhysAddr(0x2000),
                PAGE_SIZE,
                MapFlags::KERNEL_DATA
            )
            .unwrap_err(),
        MapError::AlreadyMapped(at),
        "{}: a silent replacement is a bug that appears after the jump",
        E::NAME
    );
    assert_eq!(
        mapper.translate(&memory, at),
        Some(PhysAddr(0x1000)),
        "{}: the failed call must not have changed anything",
        E::NAME
    );
}

/// Mapping inside an existing block fails rather than corrupting the walk.
fn refuses_to_split_a_block<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let block = high::<E>(0x20_0000);
    mapper
        .map_range(
            &mut memory,
            block,
            PhysAddr(0x20_0000),
            2 << 20,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    let inside = VirtAddr(block.0 + PAGE_SIZE);
    assert_eq!(
        mapper
            .map_range(
                &mut memory,
                inside,
                PhysAddr(0x1000),
                PAGE_SIZE,
                MapFlags::KERNEL_DATA
            )
            .unwrap_err(),
        MapError::BlockInTheWay(inside),
        "{}",
        E::NAME
    );
}

/// Alignment and canonicality are checked before anything is written.
fn validates_arguments<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let flags = MapFlags::KERNEL_DATA;

    for (virt, phys, len) in [
        (0x1001u64, 0u64, PAGE_SIZE),
        (0x1000, 0x11, PAGE_SIZE),
        (0x1000, 0, 100),
    ] {
        assert_eq!(
            mapper
                .map_range(&mut memory, VirtAddr(virt), PhysAddr(phys), len, flags)
                .unwrap_err(),
            MapError::Misaligned,
            "{}: {virt:#x}/{phys:#x}/{len:#x}",
            E::NAME
        );
    }

    assert_eq!(
        mapper
            .map_range(
                &mut memory,
                VirtAddr(1 << E::VIRT_BITS),
                PhysAddr(0),
                PAGE_SIZE,
                flags
            )
            .unwrap_err(),
        MapError::NotCanonical,
        "{}: an address the walk cannot translate is an entry nothing can reach",
        E::NAME
    );
    assert_eq!(
        memory.tables_allocated(),
        1,
        "{}: nothing was written",
        E::NAME
    );
}

/// An empty range is a no-op, not an error.
fn empty_range_is_a_noop<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    mapper
        .map_range(
            &mut memory,
            VirtAddr(0x1000),
            PhysAddr(0x1000),
            0,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();
    assert_eq!(memory.tables_allocated(), 1, "{}: only the root", E::NAME);
}

/// Every generic property, for one encoding.
///
/// A walk that works for x86-64 and not for `AArch64` is exactly the failure
/// this crate exists to prevent, so every architecture runs the same bodies.
fn walk_properties<E: Encoding>() {
    maps_a_page::<E>();
    maps_a_block::<E>();
    gigabyte_blocks_are_opt_in::<E>();
    misaligned_ranges_fall_back::<E>();
    refuses_to_overwrite::<E>();
    refuses_to_split_a_block::<E>();
    validates_arguments::<E>();
    empty_range_is_a_noop::<E>();
    unmapping_reports_what_it_removed::<E>();
    unmapping_walks_through_a_hole::<E>();
    unmapping_steps_over_a_hole_whole::<E>();
    unmapping_refuses_to_cut_a_block::<E>();
    protecting_keeps_the_frame_and_changes_the_permissions::<E>();
    protecting_a_hole_is_an_error::<E>();
    the_walk_finds_every_leaf::<E>();
    the_walk_reports_canonical_addresses::<E>();
    the_walk_can_stop_early::<E>();
}

/// `unmap_range` clears descriptors and says which frames came out.
fn unmapping_reports_what_it_removed<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x1000_0000);

    mapper
        .map_range(
            &mut memory,
            at,
            PhysAddr(0x20_0000),
            3 * PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    let mut pages = Vec::new();
    let mut tables = Vec::new();
    let removed = mapper
        .unmap_range(&mut memory, at, 3 * PAGE_SIZE, |freed| match freed {
            Released::Page { phys, level } => pages.push((phys, level)),
            Released::Table { phys } => tables.push(phys),
        })
        .unwrap();

    assert_eq!(removed, 3);
    assert_eq!(
        pages,
        [
            (PhysAddr(0x20_0000), Level::PAGE),
            (PhysAddr(0x20_1000), Level::PAGE),
            (PhysAddr(0x20_2000), Level::PAGE),
        ]
    );
    // Every table between the root and the leaves emptied out with the last
    // page and was unlinked. Without this the arena leaks a table per region
    // it ever touches, and the leak is invisible because the page count
    // balances perfectly.
    assert_eq!(
        tables.len(),
        levels::<E>() - 1,
        "{}: emptied tables were not reclaimed",
        E::NAME
    );
    for page in 0..3 {
        assert_eq!(
            mapper.translate(&memory, VirtAddr(at.0 + page * PAGE_SIZE)),
            None,
            "the descriptor was not cleared"
        );
    }

    // And the range can be mapped again, which is the whole point: a `vmap`
    // arena that could not reuse an address would not be an allocator.
    mapper
        .map_range(
            &mut memory,
            at,
            PhysAddr(0x30_0000),
            PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();
    assert_eq!(mapper.translate(&memory, at), Some(PhysAddr(0x30_0000)));
}

/// Unmapping a range containing a hole is not an error.
///
/// A guard page is a hole by construction, and `vunmap` of a guarded
/// allocation walks straight across it.
fn unmapping_walks_through_a_hole<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x1010_0000);

    mapper
        .map_range(
            &mut memory,
            VirtAddr(at.0 + PAGE_SIZE),
            PhysAddr(0x20_0000),
            PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    let mut pages = 0;
    let removed = mapper
        .unmap_range(&mut memory, at, 3 * PAGE_SIZE, |freed| {
            if matches!(freed, Released::Page { .. }) {
                pages += 1;
            }
        })
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(pages, 1);
}

/// Unmapping a gigabyte with one page in it at the far end finds that page
/// without a walk from the root for every page of the hole before it -- a
/// quarter of a million walks, which was what a program handing back a
/// reservation it barely touched cost.
fn unmapping_steps_over_a_hole_whole<E: Encoding>() {
    const GIB: u64 = 1 << 30;
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0);
    let last = VirtAddr(at.0 + GIB - PAGE_SIZE);
    mapper
        .map_range(
            &mut memory,
            last,
            PhysAddr(0x20_0000),
            PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();
    memory.reads.set(0);
    let removed = mapper.unmap_range(&mut memory, at, GIB, |_| {}).unwrap();
    assert_eq!(removed, 1, "{}: the page at the end was missed", E::NAME);
    assert_eq!(mapper.translate(&memory, last), None);
    assert!(
        memory.reads.get() < 16 * 1024,
        "{}: {} descriptors read to unmap one page",
        E::NAME,
        memory.reads.get()
    );
}

/// Unmapping half a block is refused rather than rounded.
fn unmapping_refuses_to_cut_a_block<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x5000_0000);

    mapper
        .map_range(
            &mut memory,
            at,
            PhysAddr(0x40_0000),
            2 * 1024 * 1024,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();
    assert_eq!(mapper.mapping_level(&memory, at), Some(Level::MEGABYTE));

    assert_eq!(
        mapper
            .unmap_range(&mut memory, at, PAGE_SIZE, |_| {})
            .unwrap_err(),
        MapError::BlockInTheWay(at)
    );
    // And the block is still there: a refused call changes nothing.
    assert_eq!(mapper.translate(&memory, at), Some(PhysAddr(0x40_0000)));
}

/// `protect_range` rewrites permissions and leaves the translation alone.
fn protecting_keeps_the_frame_and_changes_the_permissions<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x1020_0000);

    mapper
        .map_range(
            &mut memory,
            at,
            PhysAddr(0x50_0000),
            2 * PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();
    mapper
        .protect_range(&mut memory, at, 2 * PAGE_SIZE, MapFlags::KERNEL_RODATA)
        .unwrap();

    assert_eq!(mapper.translate(&memory, at), Some(PhysAddr(0x50_0000)));

    let mut seen = Vec::new();
    let _ = mapper.for_each_leaf(&memory, |leaf| {
        if leaf.virt.0 >= at.0 && leaf.virt.0 < at.0 + 2 * PAGE_SIZE {
            seen.push(leaf.flags);
        }
        true
    });
    assert_eq!(seen.len(), 2);
    for flags in seen {
        assert!(!flags.write, "protect left the mapping writable");
        assert!(!flags.execute, "read-only data must not be executable");
    }
}

/// Protecting a range with nothing in it is an error rather than a no-op.
fn protecting_a_hole_is_an_error<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x1030_0000);
    assert_eq!(
        mapper
            .protect_range(&mut memory, at, PAGE_SIZE, MapFlags::KERNEL_RODATA)
            .unwrap_err(),
        MapError::AlreadyMapped(at)
    );
}

/// Every flag the encoder writes, the decoder reads back.
///
/// This is what makes the W^X sweep a measurement: the sweep decides whether a
/// mapping is writable *and* executable by reading a descriptor the loader may
/// have written, so a decoder that disagreed with the encoder would make the
/// sweep pass by being blind.
fn every_flag_survives_a_descriptor<E: Encoding>() {
    let cases = [
        MapFlags::KERNEL_CODE,
        MapFlags::KERNEL_RODATA,
        MapFlags::KERNEL_DATA,
        MapFlags::KERNEL_DEVICE,
        MapFlags::USER_CODE,
        MapFlags::USER_DATA,
    ];

    for flags in cases {
        for level in [Level::PAGE, Level::MEGABYTE, Level::GIGABYTE] {
            if level != Level::PAGE && !E::supports_block(level) {
                continue;
            }
            let entry = E::leaf_descriptor(PhysAddr(0x20_0000), level, flags);
            assert_eq!(
                E::leaf_flags(entry),
                flags,
                "{} lost a flag at level {}",
                E::NAME,
                level.depth()
            );
        }
    }
}

/// Every flag an IOMMU descriptor can hold, the decoder reads back.
///
/// It holds fewer than a processor's — no user, global or execute bit — so
/// the cases are the encoding's own rather than the kernel's mapping kinds.
fn every_dma_flag_survives_a_descriptor<E: Encoding>(cases: &[MapFlags]) {
    for &flags in cases {
        for level in [Level::PAGE, Level::MEGABYTE, Level::GIGABYTE] {
            if level != Level::PAGE && !E::supports_block(level) {
                continue;
            }
            let entry = E::leaf_descriptor(PhysAddr(0x20_0000), level, flags);
            assert_eq!(
                E::leaf_flags(entry),
                flags,
                "{} lost a flag at level {}",
                E::NAME,
                level.depth()
            );
        }
    }
}

/// A walk visits every leaf that was mapped, and nothing else.
fn the_walk_finds_every_leaf<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();

    // Three regions far enough apart to land in different branches of the
    // tree — on ARMv7-A, in both of the root entries the kernel half has — so
    // the walk has to descend more than one.
    let regions = [
        (high::<E>(0), PhysAddr(0x10_0000), 2u64),
        (high::<E>(0x4000_0000), PhysAddr(0x20_0000), 1),
        (high::<E>(0x7000_0000), PhysAddr(0x30_0000), 3),
    ];
    for (virt, phys, pages) in regions {
        mapper
            .map_range(
                &mut memory,
                virt,
                phys,
                pages * PAGE_SIZE,
                MapFlags::KERNEL_DATA,
            )
            .unwrap();
    }

    let mut found = Vec::new();
    let outcome = mapper.for_each_leaf(&memory, |leaf| {
        found.push((leaf.virt, leaf.phys));
        true
    });

    assert!(!outcome.stopped);
    assert_eq!(outcome.leaves, 6);
    assert_eq!(found.len(), 6);
    for (virt, phys, pages) in regions {
        for page in 0..pages {
            let offset = page * PAGE_SIZE;
            assert!(
                found.contains(&(VirtAddr(virt.0 + offset), PhysAddr(phys.0 + offset))),
                "{}: the walk missed {:#x}",
                E::NAME,
                virt.0 + offset
            );
        }
    }

    // Addresses come out in order, which is what makes a sweep's report
    // readable and what lets a caller merge adjacent leaves.
    let mut sorted = found.clone();
    sorted.sort_by_key(|(virt, _)| virt.0);
    assert_eq!(found, sorted);
}

/// The walk reports addresses in the form a caller compares them in.
///
/// The address is assembled from table indices. On a 48-bit geometry that is
/// the unextended form, and reporting a kernel mapping as `0x0000_FF00_...`
/// would be a correct walk of an address no caller can compare against a
/// constant; on a 32-bit one there is nothing to extend, and extending anyway
/// would be the same bug in reverse.
fn the_walk_reports_canonical_addresses<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = high::<E>(0x7000_0000);

    mapper
        .map_range(
            &mut memory,
            at,
            PhysAddr(0x10_0000),
            PAGE_SIZE,
            MapFlags::KERNEL_CODE,
        )
        .unwrap();

    let mut seen = None;
    let _ = mapper.for_each_leaf(&memory, |leaf| {
        seen = Some(leaf.virt);
        true
    });
    assert_eq!(seen, Some(at), "{}", E::NAME);
    assert!(E::is_canonical(seen.unwrap()));
}

/// A visitor that returns `false` stops the whole recursion, not one table.
fn the_walk_can_stop_early<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    mapper
        .map_range(
            &mut memory,
            high::<E>(0x1000_0000),
            PhysAddr(0x10_0000),
            8 * PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    let mut count = 0;
    let outcome = mapper.for_each_leaf(&memory, |_| {
        count += 1;
        count < 3
    });
    assert!(outcome.stopped);
    assert_eq!(outcome.leaves, 3);
    assert_eq!(count, 3);
}

#[test]
fn the_walk_behaves_the_same_on_x86_64() {
    walk_properties::<X86_64>();
    every_flag_survives_a_descriptor::<X86_64>();
}

#[test]
fn the_walk_behaves_the_same_on_aarch64() {
    walk_properties::<AArch64>();
    every_flag_survives_a_descriptor::<AArch64>();
}

#[test]
fn the_walk_behaves_the_same_on_armv7a() {
    walk_properties::<Armv7a>();
    every_flag_survives_a_descriptor::<Armv7a>();
}

/// A range whose start is canonical and whose end is not is refused whole,
/// before a table is allocated. The walk ignores the address bits above its
/// top level, so without the check the page past the boundary would land in
/// a root slot of the other half and map an address nobody asked for.
fn a_range_may_not_leave_the_lower_half<E: Encoding>() {
    let boundary = 1u64 << (E::VIRT_BITS - 1);
    let last_low_page = boundary - PAGE_SIZE;
    let first_high = E::canonical(boundary);

    for (len, what) in [
        (2 * PAGE_SIZE, "into the gap between the halves"),
        (
            first_high - last_low_page + PAGE_SIZE,
            "across the gap into the upper half",
        ),
    ] {
        let (mut memory, mapper) = Memory::with_root::<E>();
        assert_eq!(
            mapper
                .map_range(
                    &mut memory,
                    VirtAddr(last_low_page),
                    PhysAddr(0x1000),
                    len,
                    MapFlags::KERNEL_DATA,
                )
                .unwrap_err(),
            MapError::NotCanonical,
            "{}: a range running {what}",
            E::NAME
        );
        assert_eq!(
            memory.tables_allocated(),
            1,
            "{}: nothing was written for a range running {what}",
            E::NAME
        );
    }
}

#[test]
fn a_range_may_not_leave_the_lower_half_on_x86_64() {
    a_range_may_not_leave_the_lower_half::<X86_64>();
}

#[test]
fn a_range_may_not_leave_the_lower_half_on_aarch64() {
    a_range_may_not_leave_the_lower_half::<AArch64>();
}

#[test]
fn a_32_bit_range_may_not_run_past_four_gibibytes() {
    let (mut memory, mapper) = Memory::with_root::<Armv7a>();
    assert_eq!(
        mapper
            .map_range(
                &mut memory,
                VirtAddr(0xFFFF_F000),
                PhysAddr(0x1000),
                2 * PAGE_SIZE,
                MapFlags::KERNEL_DATA,
            )
            .unwrap_err(),
        MapError::NotCanonical,
        "the second page would be slot 4 of a four-slot root"
    );
    assert_eq!(memory.tables_allocated(), 1, "nothing was written");

    // The halves of a 32-bit space touch, so crossing between them is fine.
    mapper
        .map_range(
            &mut memory,
            VirtAddr(0x7FFF_F000),
            PhysAddr(0x1000),
            2 * PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();
}

#[test]
fn running_out_of_frames_is_reported_rather_than_ignored() {
    /// A memory that hands out exactly one frame: the root.
    #[derive(Debug, Default)]
    struct Stingy {
        cells: BTreeMap<u64, u64>,
        given: usize,
    }

    // SAFETY: a map, as above; nothing here is a real address.
    unsafe impl PhysMem for Stingy {
        fn read(&self, at: PhysAddr) -> u64 {
            self.cells.get(&at.0).copied().unwrap_or(0)
        }
        fn write(&mut self, at: PhysAddr, value: u64) {
            let _ = self.cells.insert(at.0, value);
        }
        fn allocate_table(&mut self) -> Option<PhysAddr> {
            self.given += 1;
            (self.given <= 1).then(|| PhysAddr(0x1000 * self.given as u64))
        }
    }

    let mut memory = Stingy::default();
    let root = memory.allocate_table().unwrap();
    let mapper: Mapper<X86_64> = Mapper::new(root);

    assert_eq!(
        mapper
            .map_range(
                &mut memory,
                VirtAddr(0xFFFF_FFFF_8000_0000),
                PhysAddr(0x1000),
                PAGE_SIZE,
                MapFlags::KERNEL_DATA
            )
            .unwrap_err(),
        MapError::OutOfMemory
    );
}

/// A map that runs out of frames part-way leaves empty tables behind, and a
/// prune over the range gives exactly those back, deepest first, leaving
/// what is mapped beside them alone.
fn pruning_gives_back_what_a_failed_map_left<E: Encoding>() {
    /// A memory that refuses tables once `left` reaches zero.
    #[derive(Debug, Default)]
    struct Rationed {
        inner: Memory,
        left: usize,
    }

    // SAFETY: a map, as `Memory` is; nothing here is a real address.
    unsafe impl PhysMem for Rationed {
        fn read(&self, at: PhysAddr) -> u64 {
            self.inner.read(at)
        }
        fn write(&mut self, at: PhysAddr, value: u64) {
            self.inner.write(at, value);
        }
        fn allocate_table(&mut self) -> Option<PhysAddr> {
            self.left = self.left.checked_sub(1)?;
            self.inner.allocate_table()
        }
    }

    let (inner, mapper) = Memory::with_root::<E>();
    let below = levels::<E>() - 1;
    let mut memory = Rationed { inner, left: below };
    // Every hole the prune meets before `kept` scans `kept`'s tables for an
    // entry, so the cost grows with the square of its slot numbers: at
    // 0x7000_0000 it is slot 384 of its level-2 table and six minutes per
    // encoding under Miri. At 2 MiB it is slot 1, with a hole still before
    // it for the prune to look past.
    let kept = high::<E>(if cfg!(miri) { 0x20_0000 } else { 0x7000_0000 });
    mapper
        .map_range(
            &mut memory,
            kept,
            PhysAddr(0x20_0000),
            PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        )
        .unwrap();

    // A second page in another slot of the root, so that it needs a table of
    // its own at every level below it, with one fewer to make them from.
    let far = VirtAddr(kept.0 ^ (1 << (E::VIRT_BITS - 2)));
    memory.left = below.saturating_sub(1);
    assert_eq!(
        mapper.map_range(
            &mut memory,
            far,
            PhysAddr(0x30_0000),
            PAGE_SIZE,
            MapFlags::KERNEL_DATA
        ),
        Err(MapError::OutOfMemory),
        "{}",
        E::NAME
    );
    let made: Vec<PhysAddr> = memory
        .inner
        .frames
        .iter()
        .rev()
        .take(below - 1)
        .copied()
        .collect();

    let mut given = Vec::new();
    mapper
        .prune_range(
            &mut memory,
            VirtAddr(upper::<E>()),
            1 << (E::VIRT_BITS - 1),
            |freed| given.push(freed),
        )
        .unwrap();
    let tables: Vec<PhysAddr> = given
        .iter()
        .map(|freed| match freed {
            Released::Table { phys } => *phys,
            Released::Page { .. } => panic!("{}: a prune released a page", E::NAME),
        })
        .collect();
    assert_eq!(
        tables,
        made,
        "{}: the failed map's tables, deepest first",
        E::NAME
    );
    assert_eq!(
        mapper.translate(&memory, kept),
        Some(PhysAddr(0x20_0000)),
        "{}",
        E::NAME
    );
    assert_eq!(mapper.translate(&memory, far), None, "{}", E::NAME);
}

#[test]
fn pruning_gives_back_what_a_failed_map_left_on_x86_64() {
    pruning_gives_back_what_a_failed_map_left::<X86_64>();
}

#[test]
fn pruning_gives_back_what_a_failed_map_left_on_aarch64() {
    pruning_gives_back_what_a_failed_map_left::<AArch64>();
}

#[test]
fn pruning_gives_back_what_a_failed_map_left_on_armv7a() {
    pruning_gives_back_what_a_failed_map_left::<Armv7a>();
}

// ---------------------------------------------------------------------------
// x86-64 descriptor bits
// ---------------------------------------------------------------------------

mod x86_bits {
    use super::*;

    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    const USER: u64 = 1 << 2;
    const WRITE_THROUGH: u64 = 1 << 3;
    const CACHE_DISABLE: u64 = 1 << 4;
    const PAGE_SIZE_BIT: u64 = 1 << 7;
    const GLOBAL: u64 = 1 << 8;
    const NO_EXECUTE: u64 = 1 << 63;

    #[test]
    fn kernel_code_is_executable_and_never_writable() {
        let entry =
            X86_64::leaf_descriptor(PhysAddr(0x20_0000), Level::PAGE, MapFlags::KERNEL_CODE);
        assert_ne!(entry & PRESENT, 0);
        assert_eq!(entry & WRITABLE, 0, "kernel text must not be writable");
        assert_eq!(entry & NO_EXECUTE, 0, "kernel text must be executable");
        assert_eq!(
            entry & USER,
            0,
            "kernel text must not be reachable from ring 3"
        );
        assert_ne!(entry & GLOBAL, 0);
    }

    #[test]
    fn kernel_data_is_writable_and_never_executable() {
        let entry =
            X86_64::leaf_descriptor(PhysAddr(0x20_0000), Level::PAGE, MapFlags::KERNEL_DATA);
        assert_ne!(entry & WRITABLE, 0);
        assert_ne!(
            entry & NO_EXECUTE,
            0,
            "W^X: writable memory must not execute"
        );
    }

    #[test]
    fn user_mappings_set_the_user_bit_at_every_level() {
        let leaf = X86_64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::USER_CODE);
        assert_ne!(leaf & USER, 0);

        // The one that is easy to forget: permissions are ANDed down the walk,
        // so an intermediate entry without USER makes the leaf unreachable.
        let table = X86_64::table_descriptor(PhysAddr(0x2000), MapFlags::USER_CODE);
        assert_ne!(
            table & USER,
            0,
            "user pages need USER on every level above them"
        );

        let kernel_table = X86_64::table_descriptor(PhysAddr(0x2000), MapFlags::KERNEL_DATA);
        assert_eq!(
            kernel_table & USER,
            0,
            "a kernel branch must not be blanket-permissive"
        );
    }

    #[test]
    fn the_size_bit_marks_blocks_and_only_blocks() {
        let flags = MapFlags::KERNEL_DATA;
        assert_ne!(
            X86_64::leaf_descriptor(PhysAddr(0), Level::GIGABYTE, flags) & PAGE_SIZE_BIT,
            0
        );
        assert_ne!(
            X86_64::leaf_descriptor(PhysAddr(0), Level::MEGABYTE, flags) & PAGE_SIZE_BIT,
            0
        );
        assert_eq!(
            X86_64::leaf_descriptor(PhysAddr(0), Level::PAGE, flags) & PAGE_SIZE_BIT,
            0,
            "at level 3 bit 7 is the PAT bit and selects a memory type"
        );
    }

    /// Write-combining selects PAT entry 1 -- write-through alone -- which
    /// the kernel programs write-combining, reads back as itself, and loses
    /// to `device`, whose entry 3 is uncacheable.
    ///
    /// Verifies: `L.x86_64.127`
    #[test]
    fn write_combining_selects_pat_entry_one_and_reads_back() {
        let flags = MapFlags {
            write_combining: true,
            ..MapFlags::USER_DATA
        };
        let entry = X86_64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, flags);
        assert_eq!(entry & (WRITE_THROUGH | CACHE_DISABLE), WRITE_THROUGH);
        assert_eq!(entry & PAGE_SIZE_BIT, 0, "the PAT bit stays clear");
        assert_eq!(X86_64::leaf_flags(entry), flags);
        // Entry 1 of the table the kernel programs is write-combining (0x01);
        // entries 0 and 3 stay write-back (0x06) and uncacheable (0x00).
        assert_eq!(X86_PAT & 0xFF, 0x06);
        assert_eq!((X86_PAT >> 8) & 0xFF, 0x01);
        assert_eq!((X86_PAT >> 24) & 0xFF, 0x00);

        let device = MapFlags {
            write_combining: true,
            ..MapFlags::KERNEL_DEVICE
        };
        let entry = X86_64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, device);
        assert_eq!(
            entry & (WRITE_THROUGH | CACHE_DISABLE),
            WRITE_THROUGH | CACHE_DISABLE,
            "device wins over write-combining"
        );
        assert!(!X86_64::leaf_flags(entry).write_combining);
    }

    #[test]
    fn device_mappings_disable_caching() {
        let entry =
            X86_64::leaf_descriptor(PhysAddr(0xFEC0_0000), Level::PAGE, MapFlags::KERNEL_DEVICE);
        assert_ne!(entry & CACHE_DISABLE, 0);
        assert_ne!(entry & NO_EXECUTE, 0);
    }

    #[test]
    fn the_address_survives_a_round_trip() {
        let frame = PhysAddr(0x0000_000F_FFFF_F000);
        let entry = X86_64::leaf_descriptor(frame, Level::PAGE, MapFlags::KERNEL_DATA);
        assert_eq!(X86_64::address(entry), frame);
        assert_eq!(
            X86_64::address(X86_64::table_descriptor(frame, MapFlags::KERNEL_DATA)),
            frame
        );
    }
}

// ---------------------------------------------------------------------------
// AArch64 descriptor bits
// ---------------------------------------------------------------------------

mod aarch64_bits {
    use super::*;

    const VALID: u64 = 1 << 0;
    const TABLE_OR_PAGE: u64 = 1 << 1;
    const ACCESS_FLAG: u64 = 1 << 10;
    const NOT_GLOBAL: u64 = 1 << 11;
    const PXN: u64 = 1 << 53;
    const UXN: u64 = 1 << 54;
    const SH_INNER: u64 = 0b11 << 8;

    fn attr_index(entry: u64) -> u64 {
        (entry >> 2) & 0b111
    }

    fn access_permission(entry: u64) -> u64 {
        (entry >> 6) & 0b11
    }

    #[test]
    fn every_mapping_sets_the_access_flag() {
        // Without it the first touch takes an access-flag fault, and Ferrix has
        // no handler for one because it does not use them for page aging.
        for flags in [
            MapFlags::KERNEL_CODE,
            MapFlags::USER_DATA,
            MapFlags::KERNEL_DEVICE,
        ] {
            let entry = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, flags);
            assert_ne!(entry & ACCESS_FLAG, 0, "{flags:?}");
        }
    }

    #[test]
    fn bit_one_means_the_opposite_at_a_leaf_and_at_a_block() {
        let flags = MapFlags::KERNEL_DATA;
        assert_ne!(
            AArch64::leaf_descriptor(PhysAddr(0), Level::PAGE, flags) & TABLE_OR_PAGE,
            0,
            "a level-3 page sets bit 1; 0b01 there is reserved"
        );
        assert_eq!(
            AArch64::leaf_descriptor(PhysAddr(0), Level::MEGABYTE, flags) & TABLE_OR_PAGE,
            0,
            "a block leaves bit 1 clear — the exact inverse of the page case"
        );
        assert_ne!(
            AArch64::table_descriptor(PhysAddr(0x1000), flags) & TABLE_OR_PAGE,
            0
        );
    }

    #[test]
    fn access_permissions_encode_the_four_combinations() {
        let at = PhysAddr(0x1000);
        assert_eq!(
            access_permission(AArch64::leaf_descriptor(
                at,
                Level::PAGE,
                MapFlags::KERNEL_DATA
            )),
            0b00,
            "EL1 read-write, no EL0 access"
        );
        assert_eq!(
            access_permission(AArch64::leaf_descriptor(
                at,
                Level::PAGE,
                MapFlags::KERNEL_CODE
            )),
            0b10,
            "EL1 read-only"
        );
        assert_eq!(
            access_permission(AArch64::leaf_descriptor(
                at,
                Level::PAGE,
                MapFlags::USER_DATA
            )),
            0b01,
            "EL0 and EL1 read-write"
        );
        assert_eq!(
            access_permission(AArch64::leaf_descriptor(
                at,
                Level::PAGE,
                MapFlags::USER_CODE
            )),
            0b11,
            "EL0 and EL1 read-only"
        );
    }

    #[test]
    fn neither_privilege_level_may_execute_what_it_does_not_own() {
        let kernel = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::KERNEL_CODE);
        assert_eq!(kernel & PXN, 0, "the kernel may execute its own text");
        assert_ne!(kernel & UXN, 0, "user must never execute kernel text");

        let user = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::USER_CODE);
        assert_eq!(user & UXN, 0, "user may execute its own text");
        assert_ne!(
            user & PXN,
            0,
            "the kernel must never execute user text — this is what x86-64 calls SMEP"
        );

        let data = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::KERNEL_DATA);
        assert_ne!(data & PXN, 0);
        assert_ne!(data & UXN, 0);
    }

    #[test]
    fn normal_memory_is_inner_shareable_and_device_memory_is_not_cached() {
        let normal = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::KERNEL_DATA);
        assert_eq!(attr_index(normal), MAIR_NORMAL);
        assert_eq!(
            normal & SH_INNER,
            SH_INNER,
            "normal memory must be inner shareable or SMP coherency is not guaranteed"
        );

        let device =
            AArch64::leaf_descriptor(PhysAddr(0xFE00_0000), Level::PAGE, MapFlags::KERNEL_DEVICE);
        assert_eq!(attr_index(device), MAIR_DEVICE);
    }

    #[test]
    fn kernel_mappings_are_global_and_user_mappings_are_not() {
        let kernel = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::KERNEL_DATA);
        assert_eq!(
            kernel & NOT_GLOBAL,
            0,
            "kernel mappings survive an ASID change"
        );

        let user = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::USER_DATA);
        assert_ne!(
            user & NOT_GLOBAL,
            0,
            "user mappings are tagged with an ASID"
        );
    }

    #[test]
    fn the_address_survives_a_round_trip() {
        let frame = PhysAddr(0x0000_FFFF_FFFF_F000);
        let entry = AArch64::leaf_descriptor(frame, Level::PAGE, MapFlags::KERNEL_DATA);
        assert_eq!(AArch64::address(entry), frame);
        assert_ne!(entry & VALID, 0);
    }

    #[test]
    fn the_mair_value_matches_the_indices_the_encoder_uses() {
        // attr N occupies bits 8N..8N+7 of MAIR_EL1.
        let attr = |index: u64| (MAIR_EL1 >> (8 * index)) & 0xFF;
        assert_eq!(attr(MAIR_NORMAL), 0xFF, "normal write-back cacheable");
        assert_eq!(attr(MAIR_DEVICE), 0x00, "device-nGnRnE");
        assert_eq!(attr(MAIR_NORMAL_NC), 0x44, "normal non-cacheable");
    }

    /// Memory a device that does not snoop shares with a program: normal,
    /// so unaligned access and gathering stay legal, but past the caches.
    #[test]
    fn uncached_memory_is_normal_non_cacheable_and_reads_back() {
        let flags = MapFlags {
            uncached: true,
            write_combining: false,
            ..MapFlags::USER_DATA
        };
        let entry = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, flags);
        assert_eq!(attr_index(entry), MAIR_NORMAL_NC);
        assert_eq!(entry & SH_INNER, SH_INNER);
        assert_eq!(AArch64::leaf_flags(entry), flags);

        let device = MapFlags {
            uncached: true,
            write_combining: false,
            ..MapFlags::KERNEL_DEVICE
        };
        let entry = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, device);
        assert_eq!(attr_index(entry), MAIR_DEVICE, "device wins over uncached");
    }

    /// Write-combining on AArch64 is normal non-cacheable memory, which may
    /// gather stores.
    #[test]
    fn write_combining_is_normal_non_cacheable() {
        let flags = MapFlags {
            write_combining: true,
            ..MapFlags::USER_DATA
        };
        let entry = AArch64::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, flags);
        assert_eq!(attr_index(entry), MAIR_NORMAL_NC);
    }
}

// ---------------------------------------------------------------------------
// ARMv7-A descriptor bits
// ---------------------------------------------------------------------------

mod armv7a_bits {
    use super::*;

    const VALID: u64 = 1 << 0;
    const TABLE_OR_PAGE: u64 = 1 << 1;
    const ACCESS_FLAG: u64 = 1 << 10;
    const PXN: u64 = 1 << 53;
    const XN: u64 = 1 << 54;

    #[test]
    fn the_walk_starts_at_level_one_over_thirty_two_bits() {
        assert_eq!(Armv7a::ROOT_LEVEL, Level::GIGABYTE);
        assert_eq!(Armv7a::VIRT_BITS, 32);
        assert_eq!(levels::<Armv7a>(), 3, "root, level 2, level 3");
    }

    /// The kernel half lives in root entries 2 and 3, which is why the loader
    /// points `TTBR1` at the root plus sixteen bytes.
    ///
    /// `TTBCR.T1SZ = 1` makes the `TTBR1` table two entries long, indexed by
    /// address bit 30 alone. The mapper indexes the same root by bits 31:30.
    /// This test is what stops the two disagreeing silently: if the mapper
    /// ever put `0x8000_0000` anywhere but byte 16, the kernel would run on a
    /// table the hardware reads from the wrong place.
    #[test]
    fn the_kernel_half_is_entries_two_and_three_of_the_root() {
        let (mut memory, mapper) = Memory::with_root::<Armv7a>();
        for at in [0x8000_0000u64, 0xC000_0000] {
            mapper
                .map_range(
                    &mut memory,
                    VirtAddr(at),
                    PhysAddr(0x4000_0000),
                    PAGE_SIZE,
                    MapFlags::KERNEL_DATA,
                )
                .unwrap();
        }

        let root = mapper.root().0;
        assert_eq!(memory.read(PhysAddr(root)), 0, "nothing in the user half");
        assert_eq!(memory.read(PhysAddr(root + 8)), 0);
        for slot in [root + 16, root + 24] {
            let entry = memory.read(PhysAddr(slot));
            assert_ne!(entry & VALID, 0, "no descriptor at {:#x}", slot - root);
            assert_ne!(entry & TABLE_OR_PAGE, 0, "a table, not a block");
        }
        for slot in 4..ENTRIES as u64 {
            assert_eq!(
                memory.read(PhysAddr(root + slot * 8)),
                0,
                "a 32-bit root has four entries; slot {slot} is above 4 GiB"
            );
        }
    }

    #[test]
    fn execute_never_binds_both_privilege_levels() {
        let at = PhysAddr(0x4000_0000);
        let code = Armv7a::leaf_descriptor(at, Level::PAGE, MapFlags::KERNEL_CODE);
        assert_eq!(
            code & (XN | PXN),
            0,
            "XN binds PL1 too, so kernel text must clear both"
        );

        let data = Armv7a::leaf_descriptor(at, Level::PAGE, MapFlags::KERNEL_DATA);
        assert_ne!(data & XN, 0, "W^X: writable memory must not execute");
        assert_ne!(data & PXN, 0);

        let user = Armv7a::leaf_descriptor(at, Level::PAGE, MapFlags::USER_CODE);
        assert_eq!(user & XN, 0, "user may execute its own text");
        assert_ne!(user & PXN, 0, "the kernel must never execute user text");

        let user_data = Armv7a::leaf_descriptor(at, Level::PAGE, MapFlags::USER_DATA);
        assert_ne!(user_data & XN, 0);
    }

    /// Everything but the execute bits is AArch64's encoding, bit for bit.
    ///
    /// Which is the claim the whole port rests on for page tables: the same
    /// access permissions, the same attribute indices, the same shareability
    /// and access flag. If this ever fails, one of the two encoders has
    /// drifted and the other is the reference.
    #[test]
    fn everything_but_execute_is_aarch64s_encoding() {
        let execute_bits = !(XN | PXN);
        let frame = PhysAddr(0x3F_C000_0000);
        for flags in [
            MapFlags::KERNEL_CODE,
            MapFlags::KERNEL_RODATA,
            MapFlags::KERNEL_DATA,
            MapFlags::KERNEL_DEVICE,
            MapFlags::USER_CODE,
            MapFlags::USER_DATA,
        ] {
            for level in [Level::PAGE, Level::MEGABYTE, Level::GIGABYTE] {
                assert_eq!(
                    Armv7a::leaf_descriptor(frame, level, flags) & execute_bits,
                    AArch64::leaf_descriptor(frame, level, flags) & execute_bits,
                    "{flags:?} at level {}",
                    level.depth()
                );
            }
        }
    }

    #[test]
    fn the_address_is_forty_bits() {
        let frame = PhysAddr(0x0000_00FF_FFFF_F000);
        let entry = Armv7a::leaf_descriptor(frame, Level::PAGE, MapFlags::KERNEL_DATA);
        assert_eq!(Armv7a::address(entry), frame);

        // Bits 40..47 hold an address on AArch64 and are reserved here. A frame
        // up there is one this CPU cannot reach, and the encoder must not
        // smuggle it into bits the MMU reads as something else.
        let beyond = Armv7a::leaf_descriptor(
            PhysAddr(0x0000_FF00_0000_1000),
            Level::PAGE,
            MapFlags::KERNEL_DATA,
        );
        assert_eq!(beyond & 0x0000_FF00_0000_0000, 0);
        assert_eq!(Armv7a::address(beyond), PhysAddr(0x1000));
    }

    #[test]
    fn blocks_are_allowed_at_levels_one_and_two() {
        assert!(Armv7a::supports_block(Level::GIGABYTE));
        assert!(Armv7a::supports_block(Level::MEGABYTE));

        let block = Armv7a::leaf_descriptor(
            PhysAddr(0x4000_0000),
            Level::GIGABYTE,
            MapFlags::KERNEL_DATA,
        );
        assert_eq!(block & TABLE_OR_PAGE, 0, "0b01 is a block at level 1");
        assert!(Armv7a::is_leaf(block, Level::GIGABYTE));
        assert_ne!(block & ACCESS_FLAG, 0);
    }

    #[test]
    fn the_mair_halves_are_aarch64s_value_split_in_two() {
        assert_eq!((u64::from(MAIR1) << 32) | u64::from(MAIR0), MAIR_EL1);
        let attr = |index: u64| (u64::from(MAIR0) >> (8 * index)) & 0xFF;
        assert_eq!(attr(MAIR_NORMAL), 0xFF, "normal write-back cacheable");
        assert_eq!(
            attr(MAIR_DEVICE),
            0x00,
            "strongly ordered — ARMv7's name for device-nGnRnE"
        );
        assert_eq!(attr(MAIR_NORMAL_NC), 0x44, "normal non-cacheable");
    }

    /// The STM32MP1's USB host does not snoop, so the descriptors its driver
    /// shares with it are mapped past the caches, and the walk says so.
    #[test]
    fn uncached_memory_is_normal_non_cacheable_and_reads_back() {
        let flags = MapFlags {
            uncached: true,
            write_combining: false,
            ..MapFlags::USER_DATA
        };
        let entry = Armv7a::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, flags);
        assert_eq!((entry >> 2) & 0b111, MAIR_NORMAL_NC);
        assert_eq!(Armv7a::leaf_flags(entry), flags);
        let cached = Armv7a::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::USER_DATA);
        assert_eq!((cached >> 2) & 0b111, MAIR_NORMAL);
    }
}

#[test]
fn a_physical_address_wider_than_a_descriptor_is_refused() {
    let (mut memory, mapper) = Memory::with_root::<Armv7a>();
    let virt = VirtAddr(0x8000_0000);
    let top = 1_u64 << 40;
    assert_eq!(
        mapper.map_range(
            &mut memory,
            virt,
            PhysAddr(top),
            PAGE_SIZE,
            MapFlags::KERNEL_DEVICE
        ),
        Err(MapError::PhysicalOutOfRange),
        "1 TiB is beyond LPAE"
    );
    assert_eq!(
        mapper.map_range(
            &mut memory,
            virt,
            PhysAddr(top - PAGE_SIZE),
            2 * PAGE_SIZE,
            MapFlags::KERNEL_DEVICE
        ),
        Err(MapError::PhysicalOutOfRange),
        "one page past"
    );
    assert!(
        mapper
            .map_range(
                &mut memory,
                virt,
                PhysAddr(top - PAGE_SIZE),
                PAGE_SIZE,
                MapFlags::KERNEL_DEVICE
            )
            .is_ok(),
        "the last page LPAE holds"
    );
}

// ---------------------------------------------------------------------------
// IOMMU tables
// ---------------------------------------------------------------------------

/// What an `SMMUv3` domain maps an MSI doorbell with: device memory a device
/// may write.
const DOORBELL: MapFlags = MapFlags {
    device: true,
    uncached: false,
    write_combining: false,
    ..MapFlags::DMA
};

#[test]
fn the_walk_behaves_the_same_through_vt_d() {
    walk_properties::<VtdSecondLevel>();
    every_dma_flag_survives_a_descriptor::<VtdSecondLevel>(&[
        MapFlags::DMA,
        MapFlags::DMA_READ_ONLY,
    ]);
}

#[test]
fn the_walk_behaves_the_same_through_an_smmu_stage_2() {
    walk_properties::<ArmStage2>();
    every_dma_flag_survives_a_descriptor::<ArmStage2>(&[
        MapFlags::DMA,
        MapFlags::DMA_READ_ONLY,
        DOORBELL,
    ]);
}

/// An encoding's name and its canonical-address test.
type CanonicalCheck = (&'static str, fn(VirtAddr) -> bool);

#[test]
fn an_io_virtual_address_is_canonical_exactly_below_bit_39() {
    let checks: [CanonicalCheck; 2] = [
        (VtdSecondLevel::NAME, VtdSecondLevel::is_canonical),
        (ArmStage2::NAME, ArmStage2::is_canonical),
    ];
    for (name, check) in checks {
        assert!(check(VirtAddr(0)), "{name}: zero");
        assert!(check(VirtAddr((1 << 39) - 1)), "{name}: the last address");
        assert!(!check(VirtAddr(1 << 39)), "{name}: one past it");
        assert!(
            !check(VirtAddr(0xFFFF_FFC0_0000_0000)),
            "{name}: nothing is sign extended"
        );
    }
    assert_eq!(VtdSecondLevel::canonical(1 << 38), 1 << 38);
    assert_eq!(ArmStage2::canonical(1 << 38), 1 << 38);
}

#[test]
fn a_mapper_held_to_pages_never_builds_a_block() {
    let (mut memory, mut mapper) = Memory::with_root::<VtdSecondLevel>();
    mapper.pages_only();
    mapper
        .map_range(
            &mut memory,
            VirtAddr(0x20_0000),
            PhysAddr(0x20_0000),
            0x20_0000,
            MapFlags::DMA,
        )
        .unwrap();
    assert_eq!(
        mapper.mapping_level(&memory, VirtAddr(0x20_0000)),
        Some(Level::PAGE),
        "a unit without 2 MiB pages never gets one"
    );
    assert_eq!(
        mapper.translate(&memory, VirtAddr(0x3F_FFFF)),
        Some(PhysAddr(0x3F_FFFF)),
        "and every page of the range is still mapped"
    );
}

mod vtd_bits {
    use super::*;

    #[test]
    fn a_leaf_holds_read_write_and_the_superpage_bit_and_nothing_else() {
        let page =
            VtdSecondLevel::leaf_descriptor(PhysAddr(0x7F_FFFF_F000), Level::PAGE, MapFlags::DMA);
        assert_eq!(page, 0x7F_FFFF_F000 | 0b11, "R is bit 0, W bit 1");
        let block = VtdSecondLevel::leaf_descriptor(
            PhysAddr(0x20_0000),
            Level::MEGABYTE,
            MapFlags::DMA_READ_ONLY,
        );
        assert_eq!(block, 0x20_0000 | 1 | 1 << 7, "read only, superpage");
        assert!(VtdSecondLevel::is_leaf(block, Level::MEGABYTE));
        assert!(!VtdSecondLevel::is_leaf(0x1000 | 0b11, Level::MEGABYTE));
    }

    #[test]
    fn a_table_grants_both_and_the_leaf_decides() {
        assert_eq!(
            VtdSecondLevel::table_descriptor(PhysAddr(0x1000), MapFlags::DMA_READ_ONLY),
            0x1000 | 0b11
        );
    }

    #[test]
    fn an_entry_that_permits_nothing_is_absent() {
        assert!(!VtdSecondLevel::is_present(0x1000));
        assert!(VtdSecondLevel::is_present(0x1000 | 1));
    }

    #[test]
    fn the_address_is_bits_12_to_38() {
        assert_eq!(VtdSecondLevel::address(u64::MAX), PhysAddr(0x7F_FFFF_F000));
    }
}

mod stage2_bits {
    use super::*;

    #[test]
    fn a_page_is_valid_accessed_shareable_normal_memory_nobody_executes() {
        let page = ArmStage2::leaf_descriptor(PhysAddr(0x4000_0000), Level::PAGE, MapFlags::DMA);
        assert_eq!(
            page,
            0x4000_0000 | 0b11 | 0b1111 << 2 | 0b11 << 6 | 0b11 << 8 | 1 << 10 | 1 << 54,
            "valid page, MemAttr normal, S2AP read-write, SH inner, AF, XN"
        );
    }

    #[test]
    fn access_permissions_are_read_and_write_outright() {
        let read_only =
            ArmStage2::leaf_descriptor(PhysAddr(0x1000), Level::PAGE, MapFlags::DMA_READ_ONLY);
        assert_eq!(read_only & (0b11 << 6), 0b01 << 6, "S2AP[0] only");
    }

    #[test]
    fn a_doorbell_is_device_memory() {
        let doorbell = ArmStage2::leaf_descriptor(PhysAddr(0x0802_0000), Level::PAGE, DOORBELL);
        assert_eq!(doorbell & (0b1111 << 2), 0b0001 << 2, "device-nGnRE");
        assert_eq!(doorbell & (0b11 << 8), 0, "not marked shareable");
    }

    #[test]
    fn bit_one_means_the_opposite_at_a_leaf_and_at_a_block() {
        let block = ArmStage2::leaf_descriptor(
            PhysAddr(0x20_0000),
            Level::MEGABYTE,
            MapFlags::DMA_READ_ONLY,
        );
        assert_eq!(block & 0b11, 0b01, "a block clears bit 1");
        assert!(ArmStage2::is_leaf(block, Level::MEGABYTE));
        let table = ArmStage2::table_descriptor(PhysAddr(0x1000), MapFlags::DMA);
        assert_eq!(table, 0x1000 | 0b11, "a table sets it");
        assert!(!ArmStage2::is_leaf(table, Level::MEGABYTE));
    }

    #[test]
    fn the_output_address_is_forty_bits() {
        assert_eq!(ArmStage2::address(u64::MAX), PhysAddr(0xFF_FFFF_F000));
    }
}

mod coherence_bits {
    use super::*;
    use crate::coherence::{Clean, Unpublished, Walked};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// What happened to memory, in order.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        /// Eight bytes written at an address, with a value.
        Write(u64, u64),
        /// A range cleaned to memory.
        Clean(u64, u64),
        /// A table frame handed out.
        Table(u64),
    }

    type Log = Rc<RefCell<Vec<Event>>>;

    /// [`Memory`], logging each write and table.
    struct Logged {
        memory: Memory,
        log: Log,
    }

    // SAFETY: every access is `Memory`'s, whose argument this inherits.
    unsafe impl PhysMem for Logged {
        fn read(&self, at: PhysAddr) -> u64 {
            self.memory.read(at)
        }

        fn write(&mut self, at: PhysAddr, value: u64) {
            self.log.borrow_mut().push(Event::Write(at.0, value));
            self.memory.write(at, value);
        }

        fn allocate_table(&mut self) -> Option<PhysAddr> {
            let table = self.memory.allocate_table()?;
            self.log.borrow_mut().push(Event::Table(table.0));
            Some(table)
        }
    }

    /// A clean that logs the range.
    struct Recorded(Log);

    impl Clean for Recorded {
        fn clean(&mut self, at: PhysAddr, len: u64) {
            self.0.borrow_mut().push(Event::Clean(at.0, len));
        }
    }

    /// A VT-d tree with its root, and the log.
    fn tree() -> (Logged, Mapper<VtdSecondLevel>, Log) {
        let log: Log = Rc::default();
        let (memory, mapper) = Memory::with_root::<VtdSecondLevel>();
        (
            Logged {
                memory,
                log: log.clone(),
            },
            mapper,
            log,
        )
    }

    /// Whether a clean after position `from` in `log` covers the eight
    /// bytes at `at`.
    fn cleaned_after(log: &[Event], from: usize, at: u64) -> bool {
        log[from..].iter().any(|event| {
            matches!(*event, Event::Clean(start, len) if start <= at && at + 8 <= start + len)
        })
    }

    /// On a walker that does not snoop, every entry the mapper writes is
    /// cleaned after it is written, and every fresh table is cleaned whole
    /// before the entry that links it is written.
    ///
    /// Verifies: `L.iommu.56`
    /// Verifies: `L.iommu.57`
    #[test]
    fn every_write_is_cleaned_and_every_table_before_its_link() {
        let (mut memory, mapper, log) = tree();
        let mut writes = Unpublished::new(false);
        let mut walked = Walked::new(&mut memory, &mut writes, Recorded(log.clone()));
        mapper
            .map_range(
                &mut walked,
                VirtAddr(0x40_0000_0000 - 0x20_0000),
                PhysAddr(0x8000_0000),
                PAGE_SIZE,
                MapFlags::DMA,
            )
            .unwrap();
        assert!(
            !writes.is_published(),
            "the leaf is noted and not yet cleaned"
        );
        writes.publish(&mut Recorded(log.clone()));
        assert!(writes.is_published());
        let log = log.borrow().clone();
        let mut tables = 0;
        for (index, event) in log.iter().enumerate() {
            match *event {
                Event::Write(at, value) => {
                    assert!(
                        cleaned_after(&log, index, at),
                        "the write at {at:#x} was cleaned"
                    );
                    if VtdSecondLevel::is_present(value)
                        && let Some(table) =
                            log[..index].iter().find_map(|earlier| match *earlier {
                                Event::Table(table)
                                    if table == VtdSecondLevel::address(value).0 =>
                                {
                                    Some(table)
                                }
                                _ => None,
                            })
                    {
                        let linked_at = index;
                        let cleaned = log[..linked_at].contains(&Event::Clean(table, PAGE_SIZE));
                        assert!(
                            cleaned,
                            "the table at {table:#x} was cleaned before its link"
                        );
                        tables += 1;
                    }
                }
                Event::Clean(..) | Event::Table(..) => {}
            }
        }
        assert_eq!(tables, 2, "two tables below the root, each linked once");
        assert_eq!(writes.counts(), (3, 2), "three entries and two tables");
    }

    /// An unmap's writes, the leaf and the links to the tables it empties,
    /// are noted and cleaned the same way.
    ///
    /// Verifies: `L.iommu.56`
    #[test]
    fn an_unmap_cleans_what_it_clears() {
        let (mut memory, mapper, log) = tree();
        let virt = VirtAddr(0x1000_0000);
        mapper
            .map_range(
                &mut memory,
                virt,
                PhysAddr(0x8000_0000),
                PAGE_SIZE,
                MapFlags::DMA,
            )
            .unwrap();
        log.borrow_mut().clear();
        let mut writes = Unpublished::new(false);
        let mut walked = Walked::new(&mut memory, &mut writes, Recorded(log.clone()));
        let _ = mapper
            .unmap_range(&mut walked, virt, PAGE_SIZE, |_| {})
            .unwrap();
        assert!(!writes.is_published());
        writes.publish(&mut Recorded(log.clone()));
        let log = log.borrow().clone();
        let written: Vec<u64> = log
            .iter()
            .filter_map(|event| match *event {
                Event::Write(at, 0) => Some(at),
                _ => None,
            })
            .collect();
        assert!(!written.is_empty(), "the leaf was cleared");
        for (index, event) in log.iter().enumerate() {
            if let Event::Write(at, _) = *event {
                assert!(
                    cleaned_after(&log, index, at),
                    "the clear at {at:#x} was cleaned"
                );
            }
        }
    }

    /// On a walker that snoops, nothing is noted and nothing cleaned, and
    /// the record always reads as published.
    ///
    /// Verifies: `L.iommu.56`
    #[test]
    fn a_coherent_walker_cleans_nothing() {
        let (mut memory, mapper, log) = tree();
        let mut writes = Unpublished::new(true);
        let mut walked = Walked::new(&mut memory, &mut writes, Recorded(log.clone()));
        mapper
            .map_range(
                &mut walked,
                VirtAddr(0),
                PhysAddr(0x8000_0000),
                PAGE_SIZE,
                MapFlags::DMA,
            )
            .unwrap();
        assert!(writes.is_published());
        assert!(writes.is_coherent());
        assert_eq!(writes.counts(), (0, 0));
        assert!(
            !log.borrow()
                .iter()
                .any(|event| matches!(event, Event::Clean(..))),
            "nothing cleaned"
        );
    }

    /// A write noted and never published is what the check catches: the
    /// record says so until it is cleaned, and a record past its room cleans
    /// early rather than forgetting a write.
    ///
    /// Verifies: `L.iommu.56`
    #[test]
    fn an_unpublished_write_is_seen_and_a_full_record_cleans_early() {
        let log: Log = Rc::default();
        let mut clean = Recorded(log.clone());
        let mut writes = Unpublished::new(false);
        writes.wrote(PhysAddr(0x1000), 16, &mut clean);
        assert!(!writes.is_published(), "a write not cleaned is seen");
        writes.publish(&mut clean);
        assert!(writes.is_published());
        for index in 0..20 {
            writes.wrote(PhysAddr(0x2000 + index * 8), 8, &mut clean);
        }
        writes.fresh_table(PhysAddr(0x9000), &mut clean);
        assert!(writes.is_published(), "a fresh table publishes with it");
        writes.publish(&mut clean);
        let log = log.borrow();
        for index in 0..20 {
            assert!(
                log.contains(&Event::Clean(0x2000 + index * 8, 8)),
                "write {index} was cleaned"
            );
        }
        assert!(log.contains(&Event::Clean(0x9000, PAGE_SIZE)));
        assert_eq!(writes.counts(), (21, 1));
    }
}
