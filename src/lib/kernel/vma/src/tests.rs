//! Tests for the virtual memory area map.
//!
//! Two things here are worth more than the rest. The first is that every test
//! that mutates the map calls [`AddressSpace::check_invariants`] afterwards, so
//! a splitting bug is caught where it happens rather than as a missing region
//! several operations later. The second is the randomised test at the bottom,
//! which drives thousands of operations against a byte-per-page shadow model:
//! that comparison is what would actually catch an off-by-one in the carving,
//! and the region count it derives from the shadow is what would catch a
//! failure to merge.

extern crate std;

use alloc::vec;

use super::*;

/// A small window, in units of pages, so a test can name addresses by hand.
const LOW: u64 = 0x1000;
/// One past the top of the window used by most tests.
const HIGH: u64 = 0x10_0000;

fn space() -> AddressSpace {
    AddressSpace::new(LOW, HIGH).unwrap()
}

fn range(start: u64, end: u64) -> PageRange {
    PageRange::new(start, end).unwrap()
}

fn file(id: u64, offset: u64) -> Backing {
    Backing::File { id, offset }
}

/// Asserts the structural invariant, naming the violation if there is one.
#[track_caller]
fn check(space: &AddressSpace) {
    assert_eq!(
        space.check_invariants(),
        Ok(()),
        "the map broke its own invariant"
    );
}

/// The regions as `(start, end)` pairs, which is what most assertions compare.
fn extents(space: &AddressSpace) -> Vec<(u64, u64)> {
    space
        .iter()
        .map(|region| (region.range.start(), region.range.end()))
        .collect()
}

/// The reported unmappings as `(start, end)` pairs.
fn reported(report: &[Unmapping]) -> Vec<(u64, u64)> {
    report
        .iter()
        .map(|item| (item.range.start(), item.range.end()))
        .collect()
}

/// Maps an anonymous region, asserting that it took.
/// Private anonymous backing for a mapping starting at `start`.
///
/// The offset is the start address, which is the convention `Backing` documents
/// for memory with no named object: it is what makes two adjacent private
/// regions contiguous, and therefore mergeable, exactly as they were when the
/// variant carried nothing at all.
fn anon(start: u64) -> Backing {
    Backing::Anonymous {
        id: 0,
        offset: start,
    }
}

#[track_caller]
fn map(space: &mut AddressSpace, start: u64, end: u64, flags: VmaFlags) {
    assert_eq!(
        space.insert(range(start, end), flags, anon(start)),
        Ok(()),
        "the fixture mapping should have been accepted"
    );
    check(space);
}

// -- Construction and range validation --------------------------------------

#[test]
fn new_rejects_unaligned_bounds() {
    assert_eq!(
        AddressSpace::new(0x800, HIGH).err(),
        Some(VmaError::Misaligned),
        "a window that does not start on a page cannot describe whole pages"
    );
    assert_eq!(
        AddressSpace::new(LOW, HIGH + 1).err(),
        Some(VmaError::Misaligned),
        "a window that does not end on a page cannot describe whole pages"
    );
}

#[test]
fn new_rejects_an_empty_window() {
    assert_eq!(
        AddressSpace::new(HIGH, HIGH).err(),
        Some(VmaError::ZeroLength),
        "an address space with no usable addresses is not usable"
    );
    assert_eq!(
        AddressSpace::new(HIGH, LOW).err(),
        Some(VmaError::ZeroLength),
        "an inverted window is empty, not enormous"
    );
}

#[test]
fn page_range_rejects_zero_length() {
    assert_eq!(
        PageRange::new(0x2000, 0x2000).err(),
        Some(VmaError::ZeroLength),
        "a mapping of no pages has nothing to fault in"
    );
    assert_eq!(
        PageRange::from_len(0x2000, 0).err(),
        Some(VmaError::ZeroLength),
        "mmap with a length of zero is refused before it reaches the map"
    );
}

#[test]
fn page_range_rejects_misalignment() {
    assert_eq!(
        PageRange::new(0x2001, 0x4000).err(),
        Some(VmaError::Misaligned),
        "an unaligned start would put half a page in two regions"
    );
    assert_eq!(
        PageRange::new(0x2000, 0x4001).err(),
        Some(VmaError::Misaligned),
        "an unaligned end would leave a partial page mapped"
    );
    assert_eq!(
        PageRange::from_len(0x2000, 0x800).err(),
        Some(VmaError::Misaligned),
        "an unaligned length is an unaligned end"
    );
}

#[test]
fn page_range_rejects_wrapping() {
    assert_eq!(
        PageRange::from_len(u64::MAX - 0xFFF, 0x2000).err(),
        Some(VmaError::Wraps),
        "a length that carries the end past the top of the address space is not a range"
    );
    assert_eq!(
        PageRange::new(0x4000, 0x2000).err(),
        Some(VmaError::ZeroLength),
        "an inverted range is how a caller's own wrapped addition arrives here"
    );
}

// -- insert, find and iteration ---------------------------------------------

#[test]
fn insert_then_find() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ_WRITE);

    let found = space
        .find(0x11_000)
        .expect("the middle page of the region is mapped");
    assert_eq!(
        found.range,
        range(0x10_000, 0x12_000),
        "find returns the region that covers it"
    );
    assert_eq!(
        found.flags,
        VmaFlags::READ_WRITE,
        "the flags survive the insertion"
    );
    assert_eq!(
        found.backing,
        anon(0x10_000),
        "the backing survives the insertion"
    );
    assert!(!found.cow, "a freshly mapped region is not copy-on-write");
}

#[test]
fn find_treats_the_end_as_exclusive() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);

    assert!(
        space.find(0x0F_FFF).is_none(),
        "the page below the region is not part of it"
    );
    assert!(
        space.find(0x10_000).is_some(),
        "the first byte of the region is part of it"
    );
    assert!(
        space.find(0x11_FFF).is_some(),
        "the last byte of the region is part of it"
    );
    assert!(
        space.find(0x12_000).is_none(),
        "the end is exclusive, or two neighbours would both claim one page"
    );
}

#[test]
fn insert_rejects_overlap() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x14_000, VmaFlags::READ);

    for (start, end) in [
        (0x10_000, 0x14_000),
        (0x0F_000, 0x11_000),
        (0x13_000, 0x18_000),
        (0x11_000, 0x12_000),
        (0x0F_000, 0x20_000),
    ] {
        assert_eq!(
            space
                .insert(range(start, end), VmaFlags::READ, anon(start))
                .err(),
            Some(VmaError::Overlap),
            "an insert touching a mapped page must be refused, not silently merged"
        );
    }
    assert_eq!(
        space.region_count(),
        1,
        "a refused insert leaves the map alone"
    );
    check(&space);
}

#[test]
fn insert_rejects_ranges_outside_the_window() {
    let mut space = space();

    assert_eq!(
        space
            .insert(range(0, 0x2000), VmaFlags::READ, anon(0))
            .err(),
        Some(VmaError::OutOfRange),
        "the guard page below the window must stay unmapped so a null dereference faults"
    );
    assert_eq!(
        space
            .insert(
                range(HIGH - 0x1000, HIGH + 0x1000),
                VmaFlags::READ,
                anon(HIGH - 0x1000)
            )
            .err(),
        Some(VmaError::OutOfRange),
        "a region may not run past the top of the window"
    );
    assert!(
        space.is_empty(),
        "neither refused insert left anything behind"
    );
}

#[test]
fn insert_rejects_an_unusable_file_offset() {
    let mut space = space();

    assert_eq!(
        space
            .insert(range(0x10_000, 0x11_000), VmaFlags::READ, file(1, 0x800))
            .err(),
        Some(VmaError::Misaligned),
        "an unaligned file offset cannot be mapped a page at a time"
    );
    assert_eq!(
        space
            .insert(
                range(0x10_000, 0x11_000),
                VmaFlags::READ,
                file(1, u64::MAX - 0xFFF)
            )
            .err(),
        Some(VmaError::BackingOverflow),
        "an offset whose end wraps would make every later split's arithmetic a lie"
    );
    assert_eq!(
        space
            .insert(
                range(0x10_000, 0x11_000),
                VmaFlags::READ,
                Backing::Device {
                    physical: u64::MAX - 0xFFF,
                    id: 0,
                    cached: false,
                }
            )
            .err(),
        Some(VmaError::BackingOverflow),
        "the same applies to a physical base address"
    );
    assert!(space.is_empty(), "a refused backing leaves nothing behind");
}

#[test]
fn iteration_is_in_ascending_order() {
    let mut space = space();
    map(&mut space, 0x30_000, 0x31_000, VmaFlags::READ);
    map(&mut space, 0x10_000, 0x11_000, VmaFlags::READ);
    map(&mut space, 0x20_000, 0x21_000, VmaFlags::READ);

    assert_eq!(
        extents(&space),
        vec![
            (0x10_000, 0x11_000),
            (0x20_000, 0x21_000),
            (0x30_000, 0x31_000)
        ],
        "regions come back sorted whatever order they were inserted in"
    );
    assert_eq!(
        space.into_iter().count(),
        3,
        "iterating the address space by reference yields the same regions"
    );
}

// -- map_fixed ---------------------------------------------------------------

#[test]
fn map_fixed_over_nothing() {
    let mut space = space();
    let report = space.map_fixed(
        range(0x10_000, 0x12_000),
        VmaFlags::READ_WRITE,
        anon(0x10_000),
    );
    let report = report.expect("mapping into empty space succeeds");
    check(&space);

    assert!(
        report.is_empty(),
        "nothing was mapped there, so nothing was unmapped"
    );
    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x12_000)],
        "the new region is the only one"
    );
}

#[test]
fn map_fixed_over_the_exact_extent_of_a_region() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);

    let report = space
        .map_fixed(
            range(0x10_000, 0x12_000),
            VmaFlags::READ_WRITE,
            anon(0x10_000),
        )
        .expect("replacing a whole region is allowed");
    check(&space);

    assert_eq!(
        reported(&report),
        vec![(0x10_000, 0x12_000)],
        "the old region was unmapped whole"
    );
    assert_eq!(space.region_count(), 1, "one region replaced one region");
    assert_eq!(
        space.find(0x10_000).map(|region| region.flags),
        Some(VmaFlags::READ_WRITE),
        "the new flags are in place"
    );
}

#[test]
fn map_fixed_into_the_middle_splits_a_region_in_three() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x20_000, VmaFlags::READ);

    let report = space
        .map_fixed(
            range(0x14_000, 0x15_000),
            VmaFlags::READ_WRITE,
            anon(0x14_000),
        )
        .expect("mapping inside a region is allowed");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![
            (0x10_000, 0x14_000),
            (0x14_000, 0x15_000),
            (0x15_000, 0x20_000)
        ],
        "the region is cut at both edges of the new mapping"
    );
    assert_eq!(
        reported(&report),
        vec![(0x14_000, 0x15_000)],
        "only the replaced pages are reported"
    );
    assert_eq!(
        space.find(0x13_FFF).map(|region| region.flags),
        Some(VmaFlags::READ),
        "the head keeps its old flags"
    );
    assert_eq!(
        space.find(0x15_000).map(|region| region.flags),
        Some(VmaFlags::READ),
        "so does the tail"
    );
    assert_eq!(
        space.total_mapped(),
        0x10_000,
        "no pages were gained or lost"
    );
}

#[test]
fn map_fixed_over_the_front_of_a_region() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x20_000, VmaFlags::READ);

    let report = space
        .map_fixed(
            range(0x10_000, 0x12_000),
            VmaFlags::READ_WRITE,
            anon(0x10_000),
        )
        .expect("replacing the front of a region is allowed");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x12_000), (0x12_000, 0x20_000)],
        "the old region keeps only what the new one did not take"
    );
    assert_eq!(
        reported(&report),
        vec![(0x10_000, 0x12_000)],
        "the front pages were unmapped"
    );
}

#[test]
fn map_fixed_over_the_back_of_a_region() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x20_000, VmaFlags::READ);

    let report = space
        .map_fixed(
            range(0x1E_000, 0x20_000),
            VmaFlags::READ_WRITE,
            anon(0x1E_000),
        )
        .expect("replacing the back of a region is allowed");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x1E_000), (0x1E_000, 0x20_000)],
        "the old region is truncated at the new one's start"
    );
    assert_eq!(
        reported(&report),
        vec![(0x1E_000, 0x20_000)],
        "the back pages were unmapped"
    );
}

#[test]
fn map_fixed_across_whole_and_partial_regions() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);
    map(&mut space, 0x13_000, 0x15_000, VmaFlags::READ);
    map(&mut space, 0x16_000, 0x1A_000, VmaFlags::READ);

    let report = space
        .map_fixed(
            range(0x11_000, 0x18_000),
            VmaFlags::READ_WRITE,
            anon(0x11_000),
        )
        .expect("a span over several regions and two holes is allowed");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![
            (0x10_000, 0x11_000),
            (0x11_000, 0x18_000),
            (0x18_000, 0x1A_000)
        ],
        "one partial region survives at each end and the middle one is gone"
    );
    assert_eq!(
        reported(&report),
        vec![
            (0x11_000, 0x12_000),
            (0x13_000, 0x15_000),
            (0x16_000, 0x18_000)
        ],
        "the report names the mapped pages only, in ascending order, holes skipped"
    );
}

#[test]
fn map_fixed_moves_the_file_offset_of_a_truncated_front() {
    let mut space = space();
    space
        .insert(range(0x10_000, 0x14_000), VmaFlags::READ, file(7, 0x1000))
        .expect("the fixture file mapping is valid");
    check(&space);

    let report = space
        .map_fixed(
            range(0x10_000, 0x12_000),
            VmaFlags::READ_WRITE,
            anon(0x10_000),
        )
        .expect("replacing the front of a file mapping is allowed");
    check(&space);

    assert_eq!(
        space.find(0x12_000).map(|region| region.backing),
        Some(file(7, 0x3000)),
        "the surviving tail must read the file from two pages further in"
    );
    assert_eq!(
        report.first().map(|item| item.backing),
        Some(file(7, 0x1000)),
        "the unmapped front is reported at the offset it actually had"
    );
}

#[test]
fn map_fixed_rejects_ranges_outside_the_window() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);

    assert_eq!(
        space
            .map_fixed(range(0, 0x2000), VmaFlags::READ, anon(0))
            .err(),
        Some(VmaError::OutOfRange),
        "MAP_FIXED below the window is refused rather than clamped"
    );
    assert_eq!(
        space.region_count(),
        1,
        "a refused MAP_FIXED unmaps nothing"
    );
    check(&space);
}

// -- remove ------------------------------------------------------------------

#[test]
fn remove_over_nothing() {
    let mut space = space();
    let report = space
        .remove(range(0x10_000, 0x12_000))
        .expect("munmap of a hole is not an error");
    check(&space);
    assert!(
        report.is_empty(),
        "nothing was mapped, so nothing is reported"
    );
}

#[test]
fn remove_the_exact_extent_of_a_region() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);

    let report = space
        .remove(range(0x10_000, 0x12_000))
        .expect("the range is inside the window");
    check(&space);

    assert!(space.is_empty(), "the region is gone");
    assert_eq!(
        reported(&report),
        vec![(0x10_000, 0x12_000)],
        "and is reported whole"
    );
    assert_eq!(space.total_mapped(), 0, "nothing is mapped any more");
}

#[test]
fn remove_from_the_middle_splits_a_region_in_two() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x20_000, VmaFlags::READ);

    let report = space
        .remove(range(0x14_000, 0x15_000))
        .expect("the range is inside the window");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x14_000), (0x15_000, 0x20_000)],
        "a hole in the middle leaves a region on each side"
    );
    assert_eq!(
        reported(&report),
        vec![(0x14_000, 0x15_000)],
        "only the hole is reported"
    );
    assert_eq!(
        space.total_mapped(),
        0x0F_000,
        "exactly one page stopped being mapped"
    );
}

#[test]
fn remove_the_front_of_a_region() {
    let mut space = space();
    space
        .insert(range(0x10_000, 0x14_000), VmaFlags::READ, file(3, 0))
        .expect("the fixture file mapping is valid");
    check(&space);

    let report = space
        .remove(range(0x10_000, 0x12_000))
        .expect("the range is inside the window");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![(0x12_000, 0x14_000)],
        "the region starts where the hole ends"
    );
    assert_eq!(
        space.find(0x12_000).map(|region| region.backing),
        Some(file(3, 0x2000)),
        "and reads the file from where the removed pages left off"
    );
    assert_eq!(
        reported(&report),
        vec![(0x10_000, 0x12_000)],
        "the removed front is reported"
    );
}

#[test]
fn remove_the_back_of_a_region() {
    let mut space = space();
    space
        .insert(range(0x10_000, 0x14_000), VmaFlags::READ, file(3, 0))
        .expect("the fixture file mapping is valid");
    check(&space);

    let report = space
        .remove(range(0x12_000, 0x14_000))
        .expect("the range is inside the window");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x12_000)],
        "the region ends where the hole starts"
    );
    assert_eq!(
        space.find(0x10_000).map(|region| region.backing),
        Some(file(3, 0)),
        "truncating the back does not move the offset"
    );
    assert_eq!(
        report.first().map(|item| item.backing),
        Some(file(3, 0x2000)),
        "but the removed back is reported at its own offset, for writeback"
    );
}

#[test]
fn remove_across_whole_and_partial_regions() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);
    map(&mut space, 0x13_000, 0x15_000, VmaFlags::READ);
    map(&mut space, 0x16_000, 0x1A_000, VmaFlags::READ);

    let report = space
        .remove(range(0x11_000, 0x18_000))
        .expect("the range is inside the window");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x11_000), (0x18_000, 0x1A_000)],
        "one partial region survives at each end"
    );
    assert_eq!(
        reported(&report),
        vec![
            (0x11_000, 0x12_000),
            (0x13_000, 0x15_000),
            (0x16_000, 0x18_000)
        ],
        "every mapped piece inside the range is reported, in ascending order"
    );
    assert_eq!(space.total_mapped(), 0x3000, "three pages are left mapped");
}

#[test]
fn remove_skips_holes() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x11_000, VmaFlags::READ);
    map(&mut space, 0x12_000, 0x13_000, VmaFlags::READ);

    let report = space
        .remove(range(0x0F_000, 0x20_000))
        .expect("unmapping a partly mapped range is not an error");
    check(&space);

    assert!(space.is_empty(), "both regions are gone");
    assert_eq!(
        reported(&report),
        vec![(0x10_000, 0x11_000), (0x12_000, 0x13_000)],
        "the hole between them is not reported as unmapped"
    );
}

#[test]
fn remove_rejects_ranges_outside_the_window() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);

    assert_eq!(
        space.remove(range(HIGH - 0x1000, HIGH + 0x1000)).err(),
        Some(VmaError::OutOfRange),
        "an unmap that leaves the window is refused whole"
    );
    assert_eq!(space.region_count(), 1, "a refused unmap removes nothing");
    check(&space);
}

// -- protect -----------------------------------------------------------------

#[test]
fn protect_splits_at_both_edges_and_merges_back() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x20_000, VmaFlags::READ);
    let before = space.region_count();

    assert_eq!(
        space.protect(range(0x14_000, 0x15_000), VmaFlags::READ_WRITE),
        Ok(()),
        "mprotect of a mapped page succeeds"
    );
    check(&space);
    assert_eq!(
        extents(&space),
        vec![
            (0x10_000, 0x14_000),
            (0x14_000, 0x15_000),
            (0x15_000, 0x20_000)
        ],
        "the region is cut at both edges of the protected range"
    );

    assert_eq!(
        space.protect(range(0x14_000, 0x15_000), VmaFlags::READ),
        Ok(()),
        "putting the protection back succeeds"
    );
    check(&space);
    assert_eq!(
        space.region_count(),
        before,
        "the region count returns to what it was; without this a protect cycle leaks regions"
    );
    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x20_000)],
        "and the map is one region again"
    );
}

#[test]
fn repeated_protect_cycles_do_not_leak_regions() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x20_000, VmaFlags::READ);

    for page in 0..8u64 {
        let start = 0x10_000 + page * PAGE_SIZE;
        let target = range(start, start + PAGE_SIZE);
        assert_eq!(
            space.protect(target, VmaFlags::READ_WRITE),
            Ok(()),
            "protect succeeds"
        );
        check(&space);
        assert_eq!(
            space.protect(target, VmaFlags::READ),
            Ok(()),
            "and so does restoring it"
        );
        check(&space);
    }

    assert_eq!(
        space.region_count(),
        1,
        "eight protect cycles leave one region, not seventeen"
    );
}

#[test]
fn protect_a_whole_region() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x20_000, VmaFlags::READ);

    assert_eq!(
        space.protect(range(0x10_000, 0x20_000), VmaFlags::READ_EXECUTE),
        Ok(()),
        "protecting exactly one region succeeds"
    );
    check(&space);

    assert_eq!(space.region_count(), 1, "no split was needed");
    assert_eq!(
        space.find(0x18_000).map(|region| region.flags),
        Some(VmaFlags::READ_EXECUTE),
        "and the flags changed"
    );
}

#[test]
fn protect_spanning_several_regions_merges_them() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);
    map(&mut space, 0x12_000, 0x14_000, VmaFlags::READ_EXECUTE);
    map(&mut space, 0x14_000, 0x16_000, VmaFlags::READ_WRITE);

    assert_eq!(
        space.protect(range(0x10_000, 0x16_000), VmaFlags::READ),
        Ok(()),
        "protecting a run of adjacent regions succeeds"
    );
    check(&space);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x16_000)],
        "three regions with one protection between them are one region"
    );
}

#[test]
fn protect_rejects_a_range_with_a_hole() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x11_000, VmaFlags::READ);
    map(&mut space, 0x12_000, 0x13_000, VmaFlags::READ);

    assert_eq!(
        space
            .protect(range(0x10_000, 0x13_000), VmaFlags::READ_WRITE)
            .err(),
        Some(VmaError::NotMapped),
        "mprotect over an unmapped page fails as a whole, as it does in Linux"
    );
    check(&space);
    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x11_000), (0x12_000, 0x13_000)],
        "and nothing was split on the way to discovering the hole"
    );
    assert_eq!(
        space.find(0x10_000).map(|region| region.flags),
        Some(VmaFlags::READ),
        "nor were any flags changed"
    );
}

#[test]
fn protect_rejects_a_range_outside_the_window() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x11_000, VmaFlags::READ);

    assert_eq!(
        space
            .protect(range(HIGH - 0x1000, HIGH + 0x1000), VmaFlags::READ_WRITE)
            .err(),
        Some(VmaError::OutOfRange),
        "a protect that leaves the window is refused"
    );
    check(&space);
}

#[test]
fn protect_keeps_the_copy_on_write_marking() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x20_000, VmaFlags::READ_WRITE);
    let _child = space.clone_for_fork().unwrap();
    check(&space);

    assert_eq!(
        space.protect(range(0x14_000, 0x15_000), VmaFlags::READ),
        Ok(()),
        "a child may re-protect its inherited heap"
    );
    check(&space);

    for region in space.iter() {
        assert!(
            region.cow,
            "every piece stays copy-on-write, or a write would escape into the parent"
        );
    }
}

// -- merging -----------------------------------------------------------------

#[test]
fn adjacent_anonymous_regions_with_equal_flags_merge() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ_WRITE);
    map(&mut space, 0x12_000, 0x14_000, VmaFlags::READ_WRITE);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x14_000)],
        "anonymous memory has no identity, so two equal neighbours are one region"
    );
}

#[test]
fn a_region_merges_with_the_neighbour_below_as_well() {
    let mut space = space();
    map(&mut space, 0x12_000, 0x14_000, VmaFlags::READ_WRITE);
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ_WRITE);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x14_000)],
        "merging looks in both directions"
    );
}

#[test]
fn a_region_merges_with_both_neighbours_at_once() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ_WRITE);
    map(&mut space, 0x14_000, 0x16_000, VmaFlags::READ_WRITE);
    map(&mut space, 0x12_000, 0x14_000, VmaFlags::READ_WRITE);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x16_000)],
        "filling a gap between two identical regions leaves one region"
    );
}

#[test]
fn regions_with_different_flags_do_not_merge() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ_WRITE);
    map(&mut space, 0x12_000, 0x14_000, VmaFlags::READ);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x12_000), (0x12_000, 0x14_000)],
        "merging these would grant write access to pages that were mapped read-only"
    );
}

#[test]
fn regions_that_are_not_adjacent_do_not_merge() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ_WRITE);
    map(&mut space, 0x13_000, 0x15_000, VmaFlags::READ_WRITE);

    assert_eq!(
        space.region_count(),
        2,
        "a hole between two regions keeps them apart"
    );
}

#[test]
fn file_regions_merge_only_when_the_offsets_are_contiguous() {
    let mut space = space();
    space
        .insert(range(0x10_000, 0x12_000), VmaFlags::READ, file(1, 0x1000))
        .expect("valid");
    space
        .insert(range(0x12_000, 0x14_000), VmaFlags::READ, file(1, 0x3000))
        .expect("valid");
    check(&space);

    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x14_000)],
        "the second region starts exactly where the first one's two pages end"
    );
}

#[test]
fn file_regions_with_a_gap_in_the_offsets_do_not_merge() {
    let mut space = space();
    space
        .insert(range(0x10_000, 0x12_000), VmaFlags::READ, file(1, 0x1000))
        .expect("valid");
    space
        .insert(range(0x12_000, 0x14_000), VmaFlags::READ, file(1, 0x9000))
        .expect("valid");
    check(&space);

    assert_eq!(
        space.region_count(),
        2,
        "merging these would make the second half of the region read the wrong part of the file"
    );
}

#[test]
fn file_regions_of_different_files_do_not_merge() {
    let mut space = space();
    space
        .insert(range(0x10_000, 0x12_000), VmaFlags::READ, file(1, 0x1000))
        .expect("valid");
    space
        .insert(range(0x12_000, 0x14_000), VmaFlags::READ, file(2, 0x3000))
        .expect("valid");
    check(&space);

    assert_eq!(
        space.region_count(),
        2,
        "contiguous offsets into different files are not contiguous"
    );
}

#[test]
fn anonymous_and_file_regions_do_not_merge() {
    let mut space = space();
    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);
    space
        .insert(range(0x12_000, 0x14_000), VmaFlags::READ, file(1, 0))
        .expect("valid");
    check(&space);

    assert_eq!(
        space.region_count(),
        2,
        "a file mapping is not a continuation of zero-filled memory"
    );
}

#[test]
fn device_regions_merge_only_when_the_physical_pages_are_contiguous() {
    let mut space = space();
    let base = Backing::Device {
        physical: 0x8000_0000,
        id: 0,
        cached: false,
    };
    let next = Backing::Device {
        physical: 0x8000_2000,
        id: 0,
        cached: false,
    };
    let far = Backing::Device {
        physical: 0x9000_0000,
        id: 0,
        cached: false,
    };

    space
        .insert(range(0x10_000, 0x12_000), VmaFlags::READ_WRITE, base)
        .expect("valid");
    space
        .insert(range(0x12_000, 0x14_000), VmaFlags::READ_WRITE, next)
        .expect("valid");
    check(&space);
    assert_eq!(
        extents(&space),
        vec![(0x10_000, 0x14_000)],
        "the register windows are adjacent"
    );

    space
        .insert(range(0x14_000, 0x16_000), VmaFlags::READ_WRITE, far)
        .expect("valid");
    check(&space);
    assert_eq!(
        space.region_count(),
        2,
        "a different device is a different region"
    );
}

/// Two windows of one BAR may lie side by side in it and in the address
/// space, and still be two blobs: what keeps each is its own, and so is how
/// it is cached. Neither merges with the other, or unmapping one would take
/// the other's keeper with it.
#[test]
fn device_windows_merge_only_with_their_own_keeper_and_caching() {
    let window = |physical, id, cached| Backing::Device {
        physical,
        id,
        cached,
    };
    for (next, merges, why) in [
        (window(0x8000_2000, 7, true), true, "the same window"),
        (window(0x8000_2000, 8, true), false, "another keeper"),
        (window(0x8000_2000, 7, false), false, "another caching"),
        (
            window(0x8000_2000, 0, true),
            false,
            "registers beside a window",
        ),
    ] {
        let mut space = space();
        space
            .insert(
                range(0x10_000, 0x12_000),
                VmaFlags::READ_WRITE,
                window(0x8000_0000, 7, true),
            )
            .expect("valid");
        space
            .insert(range(0x12_000, 0x14_000), VmaFlags::READ_WRITE, next)
            .expect("valid");
        check(&space);
        assert_eq!(space.region_count() == 1, merges, "{why}");
    }
}

#[test]
fn a_split_file_region_merges_back_together() {
    let mut space = space();
    space
        .insert(range(0x10_000, 0x20_000), VmaFlags::READ, file(9, 0x4000))
        .expect("valid");
    check(&space);

    assert_eq!(
        space.protect(range(0x14_000, 0x15_000), VmaFlags::READ_WRITE),
        Ok(()),
        "protect"
    );
    check(&space);
    assert_eq!(
        space.region_count(),
        3,
        "the file region was cut at both edges"
    );

    assert_eq!(
        space.protect(range(0x14_000, 0x15_000), VmaFlags::READ),
        Ok(()),
        "restore"
    );
    check(&space);
    assert_eq!(
        space.region_count(),
        1,
        "the offsets the split produced line up again, so the pieces merge"
    );
    assert_eq!(
        space.find(0x10_000).map(|region| region.backing),
        Some(file(9, 0x4000)),
        "and the merged region keeps the original offset"
    );
}

// -- find_free ---------------------------------------------------------------

#[test]
fn find_free_places_top_down() {
    let mut space = space();

    assert_eq!(
        space.find_free(0x1000, PAGE_SIZE, None),
        Some(HIGH - 0x1000),
        "the first mapping goes at the very top of the window"
    );

    map(&mut space, HIGH - 0x1000, HIGH, VmaFlags::READ);
    assert_eq!(
        space.find_free(0x2000, PAGE_SIZE, None),
        Some(HIGH - 0x3000),
        "the next one goes just below it, not at the bottom of the window"
    );
}

#[test]
fn find_free_skips_a_gap_that_is_too_small() {
    let mut space = space();
    map(&mut space, HIGH - 0x1000, HIGH, VmaFlags::READ);
    map(&mut space, HIGH - 0x4000, HIGH - 0x3000, VmaFlags::READ);

    assert_eq!(
        space.find_free(0x3000, PAGE_SIZE, None),
        Some(HIGH - 0x7000),
        "the two-page gap between the regions cannot hold three pages"
    );
}

#[test]
fn find_free_honours_an_alignment_larger_than_a_page() {
    let space = AddressSpace::new(0x1000, 0x100_0000).expect("a sixteen megabyte window");

    assert_eq!(
        space.find_free(0x1000, 0x20_0000, None),
        Some(0xE0_0000),
        "a two megabyte alignment rounds the top-down candidate down to a huge page boundary"
    );
}

#[test]
fn find_free_uses_a_hint_that_fits() {
    let space = space();

    assert_eq!(
        space.find_free(0x2000, PAGE_SIZE, Some(0x20_000)),
        Some(0x20_000),
        "a free hint is honoured rather than overridden by the top-down search"
    );
}

#[test]
fn find_free_rounds_a_hint_down_to_the_alignment() {
    let space = space();

    assert_eq!(
        space.find_free(0x1000, PAGE_SIZE, Some(0x20_800)),
        Some(0x20_000),
        "mmap's address argument is advisory, so an unaligned hint is rounded, not refused"
    );
}

#[test]
fn find_free_ignores_a_hint_that_does_not_fit() {
    let mut space = space();
    map(&mut space, 0x20_000, 0x21_000, VmaFlags::READ);

    assert_eq!(
        space.find_free(0x1000, PAGE_SIZE, Some(0x20_000)),
        Some(HIGH - 0x1000),
        "an occupied hint falls back to the top-down search rather than failing"
    );
    assert_eq!(
        space.find_free(0x1000, PAGE_SIZE, Some(HIGH + 0x1000)),
        Some(HIGH - 0x1000),
        "and so does a hint outside the window"
    );
    assert_eq!(
        space.find_free(0x1000, PAGE_SIZE, Some(0)),
        Some(HIGH - 0x1000),
        "including one below it, which would otherwise map over the null guard page"
    );
}

#[test]
fn find_free_returns_none_when_the_window_is_full() {
    let mut space = AddressSpace::new(0x1000, 0x3000).expect("a two page window");
    map(&mut space, 0x1000, 0x3000, VmaFlags::READ);

    assert_eq!(
        space.find_free(0x1000, PAGE_SIZE, None),
        None,
        "there is nowhere left to put a page"
    );
    assert_eq!(
        space.find_free(0x1000, PAGE_SIZE, Some(0x1000)),
        None,
        "and a hint cannot conjure space that does not exist"
    );
}

#[test]
fn find_free_returns_none_for_a_request_that_cannot_fit() {
    let space = space();

    assert_eq!(
        space.find_free(HIGH, PAGE_SIZE, None),
        None,
        "a length larger than the whole window never fits"
    );
}

#[test]
fn find_free_rejects_lengths_and_alignments_that_are_not_mappable() {
    let space = space();

    assert_eq!(
        space.find_free(0, PAGE_SIZE, None),
        None,
        "a zero length is not a mapping"
    );
    assert_eq!(
        space.find_free(0x800, PAGE_SIZE, None),
        None,
        "nor is half a page"
    );
    assert_eq!(
        space.find_free(0x1000, 3, None),
        None,
        "an alignment that is not a power of two"
    );
    assert_eq!(
        space.find_free(0x1000, 0, None),
        None,
        "an alignment of zero"
    );
    assert_eq!(
        space.find_free(0x1000, 0x800, None),
        None,
        "an alignment finer than a page"
    );
}

// -- clone_for_fork ----------------------------------------------------------

#[test]
fn clone_for_fork_marks_private_writable_regions_on_both_sides() {
    let mut parent = space();
    map(&mut parent, 0x10_000, 0x12_000, VmaFlags::READ_WRITE);

    let child = parent.clone_for_fork().unwrap();
    check(&parent);
    check(&child);

    assert!(
        parent.find(0x10_000).is_some_and(|region| region.cow),
        "the parent must fault on its own next write, or the child would see it"
    );
    assert!(
        child.find(0x10_000).is_some_and(|region| region.cow),
        "and so must the child"
    );
    assert_eq!(
        extents(&child),
        extents(&parent),
        "the child starts as an exact copy of the parent"
    );
}

#[test]
fn clone_for_fork_leaves_shared_and_read_only_regions_alone() {
    let mut parent = space();
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    map(&mut parent, 0x10_000, 0x11_000, shared);
    map(&mut parent, 0x12_000, 0x13_000, VmaFlags::READ);

    let child = parent.clone_for_fork().unwrap();
    check(&parent);
    check(&child);

    for space in [&parent, &child] {
        assert!(
            space.find(0x10_000).is_some_and(|region| !region.cow),
            "MAP_SHARED means the two processes are supposed to see each other's writes"
        );
        assert!(
            space.find(0x12_000).is_some_and(|region| !region.cow),
            "a read-only region can never be written, so it can never need copying"
        );
    }
}

#[test]
fn clone_for_fork_leaves_device_registers_alone() {
    let mut parent = space();
    parent
        .insert(
            range(0x10_000, 0x11_000),
            VmaFlags::READ_WRITE,
            Backing::Device {
                physical: 0x8000_0000,
                id: 0,
                cached: false,
            },
        )
        .expect("valid");
    check(&parent);

    let child = parent.clone_for_fork().unwrap();
    check(&child);

    assert!(
        child.find(0x10_000).is_some_and(|region| !region.cow),
        "device registers have one physical home, so copying them on write is meaningless"
    );
}

#[test]
fn clone_for_fork_merges_regions_that_marking_made_identical() {
    let mut parent = space();
    map(&mut parent, 0x10_000, 0x12_000, VmaFlags::READ_WRITE);
    let _first_child = parent.clone_for_fork().unwrap();
    check(&parent);

    map(&mut parent, 0x12_000, 0x14_000, VmaFlags::READ_WRITE);
    check(&parent);
    assert_eq!(
        extents(&parent),
        vec![(0x10_000, 0x14_000)],
        "a fresh mapping continuing a marked one joins it: the mark is only ever safe to keep"
    );
    assert!(
        parent.find(0x13_000).is_some_and(|region| region.cow),
        "the joined region keeps the mark, and the fresh half's first write takes its page in \
         place, since nobody else holds it"
    );

    let second_child = parent.clone_for_fork().unwrap();
    check(&parent);
    check(&second_child);
    assert_eq!(
        extents(&parent),
        vec![(0x10_000, 0x14_000)],
        "a second fork leaves one region"
    );
}

/// A private region read-only at a fork is not marked, and a later
/// `mprotect` making it writable must mark it: its frames may be the other
/// space's too, and without the mark the first write lands in them. glibc's
/// RELRO page on AArch64, holding the stack guard, is one.
#[test]
fn protect_making_a_private_region_writable_marks_it_copy_on_write() {
    let mut parent = space();
    map(&mut parent, 0x10_000, 0x12_000, VmaFlags::READ);
    let mut child = parent.clone_for_fork().unwrap();
    assert!(
        child.find(0x10_000).is_some_and(|region| !region.cow),
        "read-only at the fork, so not marked by it"
    );
    child
        .protect(range(0x10_000, 0x11_000), VmaFlags::READ_WRITE)
        .unwrap();
    check(&child);
    assert!(
        child.find(0x10_000).is_some_and(|region| region.cow),
        "made writable, so its first write copies a frame the parent still holds"
    );
    child
        .protect(range(0x10_000, 0x11_000), VmaFlags::READ)
        .unwrap();
    check(&child);
    assert_eq!(
        extents(&child),
        vec![(0x10_000, 0x12_000)],
        "and back to read-only it merges with its neighbour again"
    );
}

// -- accounting --------------------------------------------------------------

#[test]
fn total_mapped_and_region_count_follow_the_map() {
    let mut space = space();
    assert_eq!(
        space.total_mapped(),
        0,
        "an empty address space maps nothing"
    );
    assert_eq!(space.region_count(), 0, "and has no regions");
    assert!(space.is_empty(), "and says so");

    map(&mut space, 0x10_000, 0x12_000, VmaFlags::READ);
    map(&mut space, 0x14_000, 0x18_000, VmaFlags::READ_WRITE);
    assert_eq!(
        space.total_mapped(),
        0x6000,
        "two regions of two and four pages"
    );
    assert_eq!(
        space.region_count(),
        2,
        "which do not touch, so they stay apart"
    );

    let _report = space
        .remove(range(0x11_000, 0x15_000))
        .expect("inside the window");
    check(&space);
    assert_eq!(
        space.total_mapped(),
        0x4000,
        "one page from each region survives the unmap"
    );
}

// -- randomised stress -------------------------------------------------------

/// A deterministic xorshift64, so a failure reproduces exactly.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Four distinct flag sets, so the shadow model's byte code determines the
/// flags and two neighbours merge exactly when their codes are equal.
fn flags_for(code: u8) -> VmaFlags {
    VmaFlags {
        read: true,
        write: code & 1 != 0,
        execute: code & 2 != 0,
        ..VmaFlags::NONE
    }
}

/// Checks that a report is ascending, stays inside the requested range, and
/// returns the number of bytes it claims were unmapped.
#[track_caller]
fn reported_bytes(report: &[Unmapping], requested: PageRange) -> u64 {
    let mut previous_end = requested.start();
    let mut total = 0;
    for item in report {
        assert!(
            item.range.start() >= previous_end,
            "reported unmappings must be ascending and disjoint"
        );
        assert!(
            item.range.end() <= requested.end(),
            "a reported unmapping must lie inside the requested range"
        );
        previous_end = item.range.end();
        total += item.range.bytes();
    }
    total
}

#[test]
fn random_operations_keep_the_invariants_and_match_a_shadow_model() {
    const PAGES: u64 = 48;
    let top = LOW + PAGES * PAGE_SIZE;

    let mut space = AddressSpace::new(LOW, top).expect("a forty-eight page window");
    let mut shadow = vec![0u8; PAGES as usize];
    let mut rng = Xorshift(0x2545_F491_4F6C_DD1D);

    for _ in 0..4000 {
        let a = rng.below(PAGES);
        let b = rng.below(PAGES);
        let (first, last) = if a <= b { (a, b + 1) } else { (b, a + 1) };
        let target = range(LOW + first * PAGE_SIZE, LOW + last * PAGE_SIZE);
        let pages = &mut shadow[first as usize..last as usize];
        let code = (rng.below(4) + 1) as u8;
        let mapped_before = pages.iter().filter(|byte| **byte != 0).count() as u64;

        match rng.below(4) {
            0 => {
                let report = space
                    .map_fixed(target, flags_for(code), anon(target.start()))
                    .expect("the range is aligned and inside the window");
                assert_eq!(
                    reported_bytes(&report, target),
                    mapped_before * PAGE_SIZE,
                    "MAP_FIXED must report exactly the pages that were mapped before it"
                );
                pages.fill(code);
            }
            1 => {
                let report = space
                    .remove(target)
                    .expect("the range is aligned and inside the window");
                assert_eq!(
                    reported_bytes(&report, target),
                    mapped_before * PAGE_SIZE,
                    "munmap must report exactly the pages it took away"
                );
                pages.fill(0);
            }
            2 => {
                let fully_mapped = mapped_before == pages.len() as u64;
                let outcome = space.protect(target, flags_for(code));
                if fully_mapped {
                    assert_eq!(outcome, Ok(()), "mprotect over mapped pages succeeds");
                    pages.fill(code);
                } else {
                    assert_eq!(
                        outcome.err(),
                        Some(VmaError::NotMapped),
                        "mprotect over a hole fails and changes nothing"
                    );
                }
            }
            _ => {
                let outcome = space.insert(target, flags_for(code), anon(target.start()));
                if mapped_before == 0 {
                    assert_eq!(outcome, Ok(()), "an insert into free space succeeds");
                    pages.fill(code);
                } else {
                    assert_eq!(
                        outcome.err(),
                        Some(VmaError::Overlap),
                        "an insert over a mapped page is refused"
                    );
                }
            }
        }

        check(&space);
    }

    // Compare the map against the shadow page by page, which is the check that
    // catches an off-by-one in the splitting rather than a broken invariant.
    let mut mapped_pages = 0;
    let mut runs = 0;
    let mut previous = 0u8;
    for (index, code) in shadow.iter().copied().enumerate() {
        let address = LOW + index as u64 * PAGE_SIZE;
        let found = space.find(address);
        if code == 0 {
            assert!(found.is_none(), "page {index} should be unmapped");
        } else {
            let region = found.expect("page should be mapped");
            assert_eq!(
                region.flags,
                flags_for(code),
                "page {index} should have the flags it was last given"
            );
            mapped_pages += 1;
            if code != previous {
                runs += 1;
            }
        }
        previous = code;
    }

    assert_eq!(
        space.total_mapped(),
        mapped_pages * PAGE_SIZE,
        "the map should account for exactly the pages the shadow says are mapped"
    );
    assert_eq!(
        space.region_count(),
        runs,
        "one region per run of equal flags: more would mean a merge was missed"
    );
}

// ---------------------------------------------------------------------------
// Named anonymous objects
// ---------------------------------------------------------------------------
//
// Anonymous memory grew an identity so that `MAP_SHARED | MAP_ANONYMOUS`, a
// futex in such memory, and reclaim asking what a page belongs to all have an
// object to name. These pin the consequences: an identity that is ignored
// would let two unrelated shared objects merge into one region, and the pages
// of the second would silently answer to the first.

#[test]
fn two_different_anonymous_objects_do_not_merge() {
    let mut space = space();
    let flags = VmaFlags::READ_WRITE;

    space
        .insert(
            range(0x10_000, 0x11_000),
            flags,
            Backing::Anonymous { id: 7, offset: 0 },
        )
        .expect("the first range is free");
    space
        .insert(
            range(0x11_000, 0x12_000),
            flags,
            Backing::Anonymous { id: 9, offset: 0 },
        )
        .expect("the second range is free");

    assert_eq!(
        space.region_count(),
        2,
        "adjacent regions naming different objects are not one mapping"
    );
}

#[test]
fn one_anonymous_object_merges_where_its_offsets_line_up() {
    let mut space = space();
    let flags = VmaFlags::READ_WRITE;

    space
        .insert(
            range(0x10_000, 0x11_000),
            flags,
            Backing::Anonymous { id: 7, offset: 0 },
        )
        .expect("the first range is free");
    space
        .insert(
            range(0x11_000, 0x12_000),
            flags,
            Backing::Anonymous {
                id: 7,
                offset: 0x1000,
            },
        )
        .expect("the second range is free");

    assert_eq!(
        space.region_count(),
        1,
        "one object mapped continuously is one region"
    );
}

#[test]
fn one_anonymous_object_mapped_out_of_order_does_not_merge() {
    let mut space = space();
    let flags = VmaFlags::READ_WRITE;

    // The same object, adjacent in address space, but the second region is
    // *earlier* in the object. Merging these would make the second page read
    // from the wrong part of it -- the file case's bug, reached by anonymous
    // memory now that it has parts to get wrong.
    space
        .insert(
            range(0x10_000, 0x11_000),
            flags,
            Backing::Anonymous {
                id: 7,
                offset: 0x4000,
            },
        )
        .expect("the first range is free");
    space
        .insert(
            range(0x11_000, 0x12_000),
            flags,
            Backing::Anonymous { id: 7, offset: 0 },
        )
        .expect("the second range is free");

    assert_eq!(
        space.region_count(),
        2,
        "discontinuous offsets are two regions"
    );
}

#[test]
fn splitting_a_named_anonymous_region_advances_its_offset() {
    let mut space = space();
    space
        .insert(
            range(0x10_000, 0x14_000),
            VmaFlags::READ_WRITE,
            Backing::Anonymous { id: 7, offset: 0 },
        )
        .expect("the range is free");

    // Take the first page away; the tail must now start one page into the
    // object, or everything mapped after it answers from the wrong offset.
    let _ = space
        .remove(range(0x10_000, 0x11_000))
        .expect("the range is mapped");

    let tail = space.find(0x11_000).expect("the tail is still mapped");
    assert_eq!(
        tail.backing,
        Backing::Anonymous {
            id: 7,
            offset: 0x1000
        },
        "truncating the front of a region moves its offset with it"
    );
}

#[test]
fn an_anonymous_offset_that_would_overflow_is_refused() {
    let mut space = space();
    // The same rule the file case has: a split advances the offset, so
    // `offset + len` has to fit before any split can be the thing that wraps.
    assert!(
        space
            .insert(
                range(0x10_000, 0x12_000),
                VmaFlags::READ,
                Backing::Anonymous {
                    id: 7,
                    offset: u64::MAX - 0x1000 + 1,
                },
            )
            .is_err(),
        "an offset whose end leaves the object's 64-bit space is refused"
    );
}

#[test]
fn a_region_put_back_down_keeps_its_copy_on_write_marking() {
    let mut space = space();
    let moved = Vma {
        range: range(0x10_000, 0x12_000),
        flags: VmaFlags::READ_WRITE,
        backing: Backing::Anonymous { id: 7, offset: 0 },
        cow: true,
    };
    space.insert_region(moved).expect("the range is free");
    check(&space);

    // `mremap` of a region a fork child still shares: losing the marking here
    // would let the next write land in a page the child can read.
    assert_eq!(
        space.find(0x11_000).copied(),
        Some(moved),
        "the region went down exactly as described, copy-on-write included"
    );

    let overlapping = Vma {
        range: range(0x11_000, 0x13_000),
        ..moved
    };
    assert!(
        matches!(space.insert_region(overlapping), Err(VmaError::Overlap)),
        "a region put down over another is refused, as insert refuses it"
    );
    check(&space);
    assert_eq!(space.region_count(), 1, "the refusal changed nothing");
}

/// Allocation failure, injected through `ferrix_fallible`: every change that
/// would have to grow the map refuses with `NoMemory` and changes nothing.
#[test]
fn a_change_that_cannot_grow_the_map_changes_nothing() {
    std::thread_local! {
        static FAIL: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
    fn policy() -> bool {
        FAIL.with(core::cell::Cell::get)
    }
    let _ = ferrix_fallible::set_injector(policy);
    ferrix_fallible::arm(true);

    let mut space = space();
    space
        .insert(
            range(0x2000, 0xA000),
            VmaFlags::READ_WRITE,
            Backing::Anonymous { id: 1, offset: 0 },
        )
        .unwrap();
    // No spare room, so that every change below has to grow the list: room
    // already there is not an allocation, and injection does not fail it.
    space.regions.shrink_to_fit();
    let before = space.clone();

    FAIL.with(|fail| fail.set(true));
    let refused = [
        space
            .insert(
                range(0xB000, 0xC000),
                VmaFlags::READ_WRITE,
                Backing::Anonymous { id: 2, offset: 0 },
            )
            .err(),
        space.remove(range(0x4000, 0x5000)).err(),
        space
            .map_fixed(
                range(0x4000, 0x5000),
                VmaFlags::READ_EXECUTE,
                Backing::Anonymous { id: 3, offset: 0 },
            )
            .err(),
        space
            .protect(range(0x3000, 0x4000), VmaFlags::READ_EXECUTE)
            .err(),
        space.clone_for_fork().err(),
    ];
    FAIL.with(|fail| fail.set(false));

    assert!(
        refused
            .iter()
            .all(|error| *error == Some(VmaError::NoMemory)),
        "{refused:?}"
    );
    assert_eq!(space, before, "a refused change left the map as it was");
    space.check_invariants().unwrap();
}

#[test]
fn a_quiet_removal_matches_a_reported_one_and_needs_no_memory_once_reserved() {
    let mut reported = space();
    reported
        .insert(
            range(0x2000, 0xA000),
            VmaFlags::READ_WRITE,
            Backing::Anonymous { id: 1, offset: 0 },
        )
        .unwrap();
    let mut quiet = reported.clone();
    let _ = reported.remove(range(0x4000, 0x5000)).unwrap();
    quiet.reserve(1).unwrap();
    quiet.remove_quietly(range(0x4000, 0x5000)).unwrap();
    assert_eq!(quiet, reported);
    check(&quiet);
}

/// Splitting one mapping into a region a page by `mprotect` is charged to
/// the job that made the space, region by region, and refused at its limit
/// with the map as it was (certification finding F-37); `fork`'s copy is
/// charged too, and everything comes back as the maps go.
#[test]
fn regions_are_charged_to_the_spaces_job() {
    let job = ferrix_kmem::testing::Job::enter(8 * 1024);
    let mut space = AddressSpace::new(0x1000, 0x1_0000_0000).unwrap();
    map(
        &mut space,
        0x10_0000,
        0x10_0000 + 4096 * PAGE_SIZE,
        VmaFlags::READ_WRITE,
    );
    let mut split = 0;
    let refused = loop {
        let at = 0x10_0000 + (2 * split + 1) * PAGE_SIZE;
        let before = space.clone();
        match space.protect(range(at, at + PAGE_SIZE), VmaFlags::READ) {
            Ok(()) => split += 1,
            Err(error) => {
                assert_eq!(space, before, "a refused mprotect changed the map");
                break error;
            }
        }
    };
    assert_eq!(refused, VmaError::NoMemory);
    assert_eq!(space.charged(), job.used());
    assert!(job.used() <= 8 * 1024 && split > 16, "{split} splits");
    let child = space.clone_for_fork().unwrap_err();
    assert_eq!(child, VmaError::NoMemory, "the copy is charged too");
    job.set_limit(u64::MAX);
    let child = space.clone_for_fork().unwrap();
    assert_eq!(job.used(), space.charged() + child.charged());
    drop((space, child));
    assert_eq!((job.used(), job.holds()), (0, 0));
}

// -- Moving the ceiling ------------------------------------------------------

/// `execve` of a 32-bit program lowers the ceiling of the space it empties,
/// and of a 64-bit one raises it again: the search and a fixed mapping both
/// stop there, and a ceiling that would leave a region outside is refused
/// with nothing changed.
#[test]
fn the_ceiling_moves_and_bounds_what_is_placed() {
    let mut space = space();
    assert_eq!(space.set_high(0x8_0000), Ok(()));
    assert_eq!(space.high(), 0x8_0000);
    assert_eq!(space.find_free(0x1000, 0x1000, None), Some(0x7_f000));
    assert_eq!(
        space.insert(
            range(0x8_0000, 0x8_1000),
            VmaFlags::READ_WRITE,
            anon(0x8_0000)
        ),
        Err(VmaError::OutOfRange)
    );

    map(&mut space, 0x7_0000, 0x7_2000, VmaFlags::READ_WRITE);
    assert_eq!(space.set_high(0x7_1000), Err(VmaError::OutOfRange));
    assert_eq!(space.high(), 0x8_0000, "a refused ceiling changes nothing");
    assert_eq!(space.set_high(0x7_2000), Ok(()));
    assert_eq!(space.set_high(HIGH), Ok(()));
    assert_eq!(space.find_free(0x1000, 0x1000, None), Some(HIGH - 0x1000));

    assert_eq!(space.set_high(0x7_2800), Err(VmaError::Misaligned));
    assert_eq!(space.set_high(LOW), Err(VmaError::ZeroLength));
    check(&space);
}

// -- Fault windows --------------------------------------------------------
//
// A window region is one region for its whole life (`Backing::Window`): no
// operation splits, merges, moves or marks it, and the ones that would are
// refused with nothing changed.

/// A shared read-write window region's flags.
const WINDOW_FLAGS: VmaFlags = VmaFlags {
    shared: true,
    ..VmaFlags::READ_WRITE
};

fn window(id: u64) -> Backing {
    Backing::Window { id }
}

/// A space with a window at `0x1_0000..0x1_4000` between two anonymous
/// regions that touch it on both sides.
fn space_with_window() -> AddressSpace {
    let mut space = space();
    map(&mut space, 0xc000, 0x1_0000, VmaFlags::READ_WRITE);
    assert_eq!(
        space.insert(range(0x1_0000, 0x1_4000), WINDOW_FLAGS, window(9)),
        Ok(())
    );
    map(&mut space, 0x1_4000, 0x1_8000, VmaFlags::READ_WRITE);
    check(&space);
    space
}

#[test]
fn a_window_is_never_merged_with_its_neighbours() {
    let neighboured = space_with_window();
    assert_eq!(
        extents(&neighboured),
        vec![
            (0xc000, 0x1_0000),
            (0x1_0000, 0x1_4000),
            (0x1_4000, 0x1_8000)
        ]
    );
    // Not even with a second window of the same object right after it.
    let mut twins = space();
    twins
        .insert(range(0x1_0000, 0x1_2000), WINDOW_FLAGS, window(9))
        .unwrap();
    twins
        .insert(range(0x1_2000, 0x1_4000), WINDOW_FLAGS, window(9))
        .unwrap();
    check(&twins);
    assert_eq!(twins.region_count(), 2);
}

#[test]
fn a_window_must_be_shared_and_never_executable() {
    let mut space = space();
    let private = VmaFlags::READ_WRITE;
    let executable = VmaFlags {
        execute: true,
        ..WINDOW_FLAGS
    };
    assert_eq!(
        space.insert(range(0x1_0000, 0x1_4000), private, window(1)),
        Err(VmaError::Window)
    );
    assert_eq!(
        space.insert(range(0x1_0000, 0x1_4000), executable, window(1)),
        Err(VmaError::Window)
    );
    assert_eq!(
        space
            .map_fixed(range(0x1_0000, 0x1_4000), executable, window(1))
            .map(|_| ()),
        Err(VmaError::Window)
    );
    assert_eq!(
        space.insert_region(Vma {
            range: range(0x1_0000, 0x1_4000),
            flags: WINDOW_FLAGS,
            backing: window(1),
            cow: true,
        }),
        Err(VmaError::Window)
    );
    assert!(space.is_empty());
}

#[test]
fn part_of_a_window_cannot_be_unmapped() {
    let mut space = space_with_window();
    let before = space.clone();
    for (start, end) in [
        (0x1_0000, 0x1_1000), // its first page
        (0x1_3000, 0x1_4000), // its last page
        (0x1_1000, 0x1_3000), // its middle
        (0xc000, 0x1_2000),   // its neighbour and its front
        (0x1_2000, 0x1_8000), // its back and its neighbour
    ] {
        assert_eq!(
            space.remove(range(start, end)).map(|_| ()),
            Err(VmaError::Window)
        );
        assert_eq!(
            space.remove_quietly(range(start, end)),
            Err(VmaError::Window)
        );
        assert_eq!(
            space
                .map_fixed(range(start, end), VmaFlags::READ_WRITE, anon(start))
                .map(|_| ()),
            Err(VmaError::Window)
        );
        assert_eq!(space, before, "a refused change changed the map");
    }
}

#[test]
fn a_whole_window_unmaps_as_one_region() {
    let mut space = space_with_window();
    let removed = space.remove(range(0xe000, 0x1_6000)).unwrap();
    check(&space);
    assert_eq!(
        reported(&removed),
        vec![
            (0xe000, 0x1_0000),
            (0x1_0000, 0x1_4000),
            (0x1_4000, 0x1_6000)
        ]
    );
    assert_eq!(removed.get(1).map(|gone| gone.backing), Some(window(9)));
    assert_eq!(
        extents(&space),
        vec![(0xc000, 0xe000), (0x1_6000, 0x1_8000)]
    );

    // And a fixed mapping over exactly the window replaces it.
    let mut space = space_with_window();
    let replaced = space
        .map_fixed(
            range(0x1_0000, 0x1_4000),
            VmaFlags::READ_WRITE,
            anon(0x1_0000),
        )
        .unwrap();
    check(&space);
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced.first().map(|gone| gone.backing), Some(window(9)));
    assert_eq!(
        space.find(0x1_2000).map(|vma| vma.backing.is_window()),
        Some(false)
    );
}

#[test]
fn no_protection_change_touches_a_window() {
    let mut space = space_with_window();
    let before = space.clone();
    for (start, end) in [
        (0x1_0000, 0x1_4000),
        (0x1_1000, 0x1_2000),
        (0xc000, 0x1_8000),
    ] {
        assert_eq!(
            space.protect(range(start, end), VmaFlags::READ),
            Err(VmaError::Window)
        );
        assert_eq!(space, before);
    }
    // Its neighbours still change.
    space
        .protect(range(0xc000, 0x1_0000), VmaFlags::READ)
        .unwrap();
    check(&space);
}

#[test]
fn fork_shares_a_window_unmarked() {
    let mut space = space_with_window();
    let child = space.clone_for_fork().unwrap();
    check(&space);
    check(&child);
    for side in [&space, &child] {
        let region = side.find(0x1_2000).unwrap();
        assert_eq!(region.backing, window(9));
        assert_eq!(region.range, range(0x1_0000, 0x1_4000));
        assert!(!region.cow, "a window was marked copy-on-write");
    }
}
