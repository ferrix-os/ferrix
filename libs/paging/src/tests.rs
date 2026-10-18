//! Tests for the page table builder.
//!
//! The mapper is generic over the encoding, so most of what follows runs
//! against both architectures from one body: a walk that works for x86-64 and
//! not for `AArch64` is exactly the failure this crate exists to prevent. The
//! per-architecture tests below it assert the individual bits, because a
//! descriptor with the right address and the wrong permission bit translates
//! perfectly and protects nothing.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use super::aarch64::{AArch64, MAIR_DEVICE, MAIR_EL1, MAIR_NORMAL, MAIR_NORMAL_NC};
use super::x86_64::X86_64;
use super::*;

/// Physical memory as a sparse map, so a test can assert on descriptors
/// without owning a machine.
#[derive(Debug, Default)]
struct Memory {
    cells: BTreeMap<u64, u64>,
    next_frame: u64,
    frames: Vec<PhysAddr>,
}

impl Memory {
    fn new() -> Self {
        Memory {
            cells: BTreeMap::new(),
            // Deliberately not zero: a mapper that confuses "absent" with
            // "frame 0" should fail a test rather than work by accident.
            next_frame: 0x10_0000,
            frames: Vec::new(),
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

#[test]
fn canonical_addresses_are_the_two_halves_and_nothing_between() {
    assert!(VirtAddr(0).is_canonical());
    assert!(VirtAddr(0x0000_7FFF_FFFF_FFFF).is_canonical());
    assert!(VirtAddr(0xFFFF_8000_0000_0000).is_canonical());
    assert!(VirtAddr(0xFFFF_FFFF_8000_0000).is_canonical());
    assert!(!VirtAddr(0x0000_8000_0000_0000).is_canonical());
    assert!(!VirtAddr(0x0001_0000_0000_0000).is_canonical());
    assert!(!VirtAddr(0xFFFF_7FFF_FFFF_FFFF).is_canonical());
}

// ---------------------------------------------------------------------------
// The walk, run against both architectures
// ---------------------------------------------------------------------------

/// A 4 KiB mapping resolves, offset and all, and costs one table per level.
fn maps_a_page<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = VirtAddr(0xFFFF_FFFF_8000_0000);
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
        4,
        "{}: root, level 1, level 2, level 3",
        E::NAME
    );
}

/// An aligned 2 MiB range becomes one block rather than 512 pages.
fn maps_a_block<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = VirtAddr(0xFFFF_8000_4000_0000);
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
        3,
        "{}: root and two levels, with no leaf table at all",
        E::NAME
    );
}

/// 1 GiB blocks are used only when asked for, because x86-64 may not have them.
fn gigabyte_blocks_are_opt_in<E: Encoding>() {
    let at = VirtAddr(0xFFFF_8000_0000_0000);

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
        2,
        "{}: root and level 1",
        E::NAME
    );
}

/// A range that starts unaligned uses pages, then blocks, then pages again —
/// and every byte of it resolves.
fn misaligned_ranges_fall_back<E: Encoding>() {
    let (mut memory, mut mapper) = Memory::with_root::<E>();
    mapper.allow_gigabyte_blocks();

    let start = VirtAddr(0xFFFF_8000_0000_0000 + PAGE_SIZE);
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
        mapper.mapping_level(&memory, VirtAddr(0xFFFF_8000_0020_0000)),
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
    let at = VirtAddr(0xFFFF_FFFF_8000_0000);
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
    let block = VirtAddr(0xFFFF_8000_0020_0000);
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
                VirtAddr(0x0000_8000_0000_0000),
                PhysAddr(0),
                PAGE_SIZE,
                flags
            )
            .unwrap_err(),
        MapError::NotCanonical,
        "{}: a non-canonical address is an entry nothing can reach",
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
/// this crate exists to prevent, so both architectures run the same bodies.
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
    unmapping_refuses_to_cut_a_block::<E>();
    protecting_keeps_the_frame_and_changes_the_permissions::<E>();
    protecting_a_hole_is_an_error::<E>();
    every_flag_survives_a_descriptor::<E>();
    the_walk_finds_every_leaf::<E>();
    the_walk_reports_canonical_addresses::<E>();
    the_walk_can_stop_early::<E>();
}

/// `unmap_range` clears descriptors and says which frames came out.
fn unmapping_reports_what_it_removed<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = VirtAddr(0xFFFF_FF00_0000_0000);

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
    // The three tables between the root and the leaves emptied out with the
    // last page and were unlinked. Without this the arena leaks a table per
    // region it ever touches, and the leak is invisible because the page count
    // balances perfectly.
    assert_eq!(tables.len(), 3, "emptied tables were not reclaimed");
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
    let at = VirtAddr(0xFFFF_FF00_0010_0000);

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

/// Unmapping half a block is refused rather than rounded.
fn unmapping_refuses_to_cut_a_block<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = VirtAddr(0xFFFF_FF00_4000_0000);

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
    let at = VirtAddr(0xFFFF_FF00_0020_0000);

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
    let at = VirtAddr(0xFFFF_FF00_0030_0000);
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

/// A walk visits every leaf that was mapped, and nothing else.
fn the_walk_finds_every_leaf<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();

    // Three regions far enough apart to land in different top-level entries,
    // so the walk has to descend more than one branch.
    let regions = [
        (VirtAddr(0xFFFF_FF00_0000_0000), PhysAddr(0x10_0000), 2u64),
        (VirtAddr(0xFFFF_FF80_0000_0000), PhysAddr(0x20_0000), 1),
        (VirtAddr(0xFFFF_FFFF_8000_0000), PhysAddr(0x30_0000), 3),
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
                "the walk missed {:#x}",
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

/// The walk rebuilds the sign extension a virtual address carries.
///
/// The address is assembled from table indices, which produces the 48-bit
/// form. Reporting a kernel mapping as `0x0000_FF00_...` would be a correct
/// walk of an address no caller can compare against a constant.
fn the_walk_reports_canonical_addresses<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    let at = VirtAddr(0xFFFF_FFFF_8000_0000);

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
    assert_eq!(seen, Some(at));
    assert!(seen.unwrap().is_canonical());
}

/// A visitor that returns `false` stops the whole recursion, not one table.
fn the_walk_can_stop_early<E: Encoding>() {
    let (mut memory, mapper) = Memory::with_root::<E>();
    mapper
        .map_range(
            &mut memory,
            VirtAddr(0xFFFF_FF00_0000_0000),
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
}

#[test]
fn the_walk_behaves_the_same_on_aarch64() {
    walk_properties::<AArch64>();
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

// ---------------------------------------------------------------------------
// x86-64 descriptor bits
// ---------------------------------------------------------------------------

mod x86_bits {
    use super::*;

    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    const USER: u64 = 1 << 2;
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
}
