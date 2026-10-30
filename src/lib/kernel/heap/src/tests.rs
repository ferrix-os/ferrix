//! Tests for the kernel heap.
//!
//! The fixture is a fake page supply over a map, which lets the tests assert
//! the two things that actually matter and are otherwise invisible: that no two
//! live allocations overlap, and that a heap which is emptied has the same
//! number of objects on its lists as one that was never used.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use super::*;

/// A page supply over a map.
///
/// Addresses start well above zero so that a bug which confuses the end-of-list
/// sentinel with a real address fails a test rather than working by luck.
#[derive(Debug, Default)]
struct Pages {
    links: BTreeMap<u64, u64>,
    next: u64,
    live: BTreeMap<u64, u8>,
    budget: usize,
    handed_out: usize,
    /// Stands in for the frame allocator's per-frame record.
    slab_free: BTreeMap<u64, u16>,
}

impl Pages {
    fn with_budget(pages: usize) -> Pages {
        Pages {
            links: BTreeMap::new(),
            next: 0xFFFF_8000_0010_0000,
            live: BTreeMap::new(),
            budget: pages,
            handed_out: 0,
            slab_free: BTreeMap::new(),
        }
    }

    /// Pages this supply has handed out and not had back.
    fn outstanding(&self) -> usize {
        self.handed_out
    }
}

// SAFETY: not real memory at all — a map, whose keys are the addresses this
// type invented. Nothing aliases anything, and every address handed out is
// unique because `next` only ever moves forward.
unsafe impl Backing for Pages {
    fn allocate_pages(&mut self, order: u8) -> Option<u64> {
        let count = 1usize << order;
        if self.handed_out + count > self.budget {
            return None;
        }
        let address = self.next;
        self.next += (count * PAGE_SIZE) as u64;
        self.handed_out += count;
        let _ = self.live.insert(address, order);
        Some(address)
    }

    fn deallocate_pages(&mut self, address: u64, order: u8) {
        let recorded = self.live.remove(&address);
        assert_eq!(
            recorded,
            Some(order),
            "page block at {address:#x} freed with the wrong order"
        );
        self.handed_out -= 1usize << order;
    }

    fn read_link(&self, at: u64) -> u64 {
        assert_eq!(at % 8, 0, "links must be eight-byte aligned");
        self.links.get(&at).copied().unwrap_or(0)
    }

    fn write_link(&mut self, at: u64, value: u64) {
        assert_eq!(at % 8, 0, "links must be eight-byte aligned");
        let _ = self.links.insert(at, value);
    }

    fn slab_free(&self, page: u64) -> u16 {
        self.slab_free.get(&page).copied().unwrap_or(0)
    }

    fn set_slab_free(&mut self, page: u64, objects: u16) {
        assert_eq!(
            page % PAGE_SIZE as u64,
            0,
            "a slab page must be page aligned"
        );
        let _ = self.slab_free.insert(page, objects);
    }
}

/// Assert that a set of live allocations do not overlap each other.
fn assert_disjoint(live: &[(u64, Request)]) {
    let mut spans: Vec<(u64, u64)> = live
        .iter()
        .map(|&(address, request)| {
            let size = Heap::class_of(request).map_or_else(
                || request.effective_size().next_multiple_of(PAGE_SIZE),
                |class| CLASS_SIZES[class],
            );
            (address, address + size as u64)
        })
        .collect();
    spans.sort_unstable();

    for window in spans.windows(2) {
        let [(_, first_end), (second_start, _)] = window else {
            continue;
        };
        assert!(
            first_end <= second_start,
            "allocations overlap: one ends at {first_end:#x}, the next starts at {second_start:#x}"
        );
    }
}

// ---------------------------------------------------------------------------
// Size classes
// ---------------------------------------------------------------------------

#[test]
fn a_request_lands_in_the_smallest_class_that_holds_it() {
    assert_eq!(Heap::class_of(Request::new(1, 1)), Some(0));
    assert_eq!(Heap::class_of(Request::new(8, 1)), Some(0));
    assert_eq!(Heap::class_of(Request::new(9, 1)), Some(1));
    assert_eq!(Heap::class_of(Request::new(16, 1)), Some(1));
    assert_eq!(Heap::class_of(Request::new(17, 1)), Some(2));
    assert_eq!(Heap::class_of(Request::new(2048, 1)), Some(CLASSES - 1));
    assert_eq!(
        Heap::class_of(Request::new(2049, 1)),
        None,
        "past the largest class it becomes a page allocation"
    );
}

#[test]
fn alignment_larger_than_the_size_forces_a_bigger_class() {
    // The classes are the only alignment guarantee, so an eight-byte object
    // needing 64-byte alignment has to come from the 64-byte class.
    assert_eq!(
        Heap::class_of(Request::new(8, 64)),
        Heap::class_of(Request::new(64, 1))
    );
    assert_eq!(Request::new(8, 64).effective_size(), 64);
    assert_eq!(
        Request::new(0, 1).effective_size(),
        1,
        "zero rounds up to one"
    );
}

#[test]
fn an_alignment_that_is_not_a_power_of_two_is_refused() {
    let mut pages = Pages::with_budget(16);
    let mut heap = Heap::new();
    assert_eq!(
        heap.allocate(&mut pages, Request::new(16, 3)).unwrap_err(),
        HeapError::BadAlignment(3)
    );
}

// ---------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------

#[test]
fn an_allocation_round_trips() {
    let mut pages = Pages::with_budget(16);
    let mut heap = Heap::new();

    let request = Request::new(32, 8);
    let address = heap.allocate(&mut pages, request).unwrap();
    assert_eq!(heap.allocated_bytes(), 32);
    assert_eq!(
        heap.slab_pages(),
        1,
        "one page carved for the 32-byte class"
    );

    heap.deallocate(&mut pages, address, request);
    assert_eq!(heap.allocated_bytes(), 0);
}

#[test]
fn every_object_from_a_class_is_aligned_to_it() {
    let mut pages = Pages::with_budget(64);
    let mut heap = Heap::new();

    for &size in &CLASS_SIZES {
        let request = Request::new(size, size);
        let address = heap.allocate(&mut pages, request).unwrap();
        assert!(
            address.is_multiple_of(size as u64),
            "a {size}-byte object landed at {address:#x}, which is not {size}-aligned"
        );
    }
}

#[test]
fn one_page_yields_exactly_as_many_objects_as_it_holds() {
    let mut pages = Pages::with_budget(2);
    let mut heap = Heap::new();

    let request = Request::new(256, 8);
    let mut taken = Vec::new();
    // The first page gives 4096/256 = 16 objects.
    for _ in 0..16 {
        taken.push(heap.allocate(&mut pages, request).unwrap());
        assert_eq!(heap.slab_pages(), 1, "should not have needed a second page");
    }

    // The seventeenth forces a refill.
    let _ = heap.allocate(&mut pages, request).unwrap();
    assert_eq!(heap.slab_pages(), 2);

    let live: Vec<(u64, Request)> = taken.iter().map(|&address| (address, request)).collect();
    assert_disjoint(&live);
}

#[test]
fn objects_come_out_of_a_fresh_page_in_ascending_order() {
    // Not a correctness requirement, but a heap dump that reads in address
    // order is worth a great deal the first time one has to be read.
    let mut pages = Pages::with_budget(4);
    let mut heap = Heap::new();

    let request = Request::new(512, 8);
    let first = heap.allocate(&mut pages, request).unwrap();
    let second = heap.allocate(&mut pages, request).unwrap();
    let third = heap.allocate(&mut pages, request).unwrap();

    assert_eq!(second, first + 512);
    assert_eq!(third, second + 512);
}

#[test]
fn a_large_request_bypasses_the_classes() {
    let mut pages = Pages::with_budget(64);
    let mut heap = Heap::new();

    let request = Request::new(9000, 8);
    let address = heap.allocate(&mut pages, request).unwrap();

    assert_eq!(heap.slab_pages(), 0, "no size class was involved");
    assert_eq!(heap.large_pages(), 4, "9000 bytes rounds to four pages");
    assert!(address.is_multiple_of(PAGE_SIZE as u64));

    heap.deallocate(&mut pages, address, request);
    assert_eq!(heap.large_pages(), 0);
    assert_eq!(heap.allocated_bytes(), 0);
    assert_eq!(
        pages.handed_out, 0,
        "large pages must go back to the supply"
    );
}

#[test]
fn a_request_larger_than_one_block_is_refused() {
    let mut pages = Pages::with_budget(1 << 14);
    let mut heap = Heap::new();

    // MAX_ORDER is 10, so 1024 pages is the largest block.
    let ok = Request::new(1024 * PAGE_SIZE, 8);
    let address = heap.allocate(&mut pages, ok).unwrap();
    heap.deallocate(&mut pages, address, ok);

    let too_big = Request::new(1025 * PAGE_SIZE, 8);
    assert_eq!(
        heap.allocate(&mut pages, too_big).unwrap_err(),
        HeapError::TooLarge(1025 * PAGE_SIZE)
    );
}

#[test]
fn exhaustion_is_reported_rather_than_wrapping() {
    let mut pages = Pages::with_budget(1);
    let mut heap = Heap::new();

    let request = Request::new(2048, 8);
    // One page holds two objects.
    let _ = heap.allocate(&mut pages, request).unwrap();
    let _ = heap.allocate(&mut pages, request).unwrap();
    assert_eq!(
        heap.allocate(&mut pages, request).unwrap_err(),
        HeapError::OutOfMemory,
        "the page supply is empty and there is nothing on the free list"
    );
}

// ---------------------------------------------------------------------------
// The invariants
// ---------------------------------------------------------------------------

#[test]
fn freeing_returns_the_object_to_its_own_class() {
    let mut pages = Pages::with_budget(8);
    let mut heap = Heap::new();

    let request = Request::new(64, 8);
    let first = heap.allocate(&mut pages, request).unwrap();
    heap.deallocate(&mut pages, first, request);

    let again = heap.allocate(&mut pages, request).unwrap();
    assert_eq!(again, first, "the freed object should be reused");
    assert_eq!(heap.slab_pages(), 1, "and no new page taken");
}

#[test]
fn a_class_that_is_emptied_and_refilled_conserves_its_objects() {
    let mut pages = Pages::with_budget(4);
    let mut heap = Heap::new();
    let request = Request::new(128, 8);

    let before = heap.free_objects(&pages);
    let mut taken = Vec::new();
    for _ in 0..32 {
        taken.push(heap.allocate(&mut pages, request).unwrap());
    }
    // One page of 128-byte objects is exactly 32, so the class is empty again.
    assert_eq!(heap.free_objects(&pages)[4], 0);

    for address in taken {
        heap.deallocate(&mut pages, address, request);
    }
    assert_eq!(
        heap.free_objects(&pages)[4],
        32,
        "every object should be back on the list"
    );
    assert_eq!(heap.allocated_bytes(), 0);
    assert_eq!(before[4], 0, "and the class started empty");
}

#[test]
fn a_mixed_workload_never_overlaps_two_live_allocations() {
    // The property that actually matters. A heap that hands the same bytes to
    // two callers does not fail where it happened; it fails in whichever
    // subsystem writes second.
    let mut pages = Pages::with_budget(256);
    let mut heap = Heap::new();

    let sizes = [1usize, 7, 8, 9, 33, 100, 255, 256, 700, 2048, 5000];
    let mut live: Vec<(u64, Request)> = Vec::new();
    let mut seed = 0x1234_5678u64;

    for step in 0..2000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let roll = (seed >> 33) as usize;

        if live.is_empty() || roll % 100 < 60 {
            let request = Request::new(sizes[roll % sizes.len()], 8);
            let Ok(address) = heap.allocate(&mut pages, request) else {
                continue;
            };
            live.push((address, request));
        } else {
            let victim = roll % live.len();
            let (address, request) = live.swap_remove(victim);
            heap.deallocate(&mut pages, address, request);
        }

        if step % 50 == 0 {
            assert_disjoint(&live);
        }
    }

    assert_disjoint(&live);
    for (address, request) in live {
        heap.deallocate(&mut pages, address, request);
    }
    assert_eq!(heap.allocated_bytes(), 0, "everything was given back");
}

#[test]
fn a_new_heap_holds_nothing() {
    let pages = Pages::with_budget(0);
    let heap = Heap::new();

    assert_eq!(heap.allocated_bytes(), 0);
    assert_eq!(heap.slab_pages(), 0);
    assert_eq!(heap.large_pages(), 0);
    assert_eq!(heap.free_objects(&pages), [0; CLASSES]);
}

// ---------------------------------------------------------------------------
// Returning slab pages
// ---------------------------------------------------------------------------

/// A class that empties gives its pages back, keeping one.
///
/// The count is the whole point: before the per-page free count existed, this
/// heap grew monotonically and a burst of small allocations was memory the
/// system never saw again.
#[test]
fn an_emptied_class_returns_its_pages_but_keeps_one() {
    let mut pages = Pages::with_budget(64);
    let mut heap = Heap::new();
    let request = Request::new(64, 8);
    let per_page = PAGE_SIZE / 64;

    // Four pages' worth, so there is something to give back.
    let mut live = Vec::new();
    for _ in 0..per_page * 4 {
        live.push(heap.allocate(&mut pages, request).unwrap());
    }
    assert_eq!(heap.slab_pages(), 4);
    assert_eq!(pages.outstanding(), 4);

    for address in live.drain(..) {
        heap.deallocate(&mut pages, address, request);
    }

    assert_eq!(
        heap.slab_pages(),
        1,
        "an emptied class should keep exactly one page"
    );
    assert_eq!(heap.slab_pages_returned(), 3);
    assert_eq!(pages.outstanding(), 1, "the pages did not reach the supply");
    assert_eq!(heap.allocated_bytes(), 0);
}

/// The page that is kept still serves allocations.
///
/// A page returned to the supply while its objects were still linked would
/// leave the class's free list pointing into memory the supply has given to
/// somebody else — which is not a leak but a corruption, and the reason
/// `unlink_page` exists.
#[test]
fn the_kept_page_still_works_after_the_others_go_back() {
    let mut pages = Pages::with_budget(64);
    let mut heap = Heap::new();
    let request = Request::new(32, 8);
    let per_page = PAGE_SIZE / 32;

    let mut live: Vec<u64> = (0..per_page * 3)
        .map(|_| heap.allocate(&mut pages, request).unwrap())
        .collect();
    for address in live.drain(..) {
        heap.deallocate(&mut pages, address, request);
    }
    assert_eq!(heap.slab_pages(), 1);

    // Exactly one page's worth must come out of the kept page, with no new
    // page taken and no two allocations overlapping.
    let taken = pages.outstanding();
    let refilled: Vec<(u64, Request)> = (0..per_page)
        .map(|_| (heap.allocate(&mut pages, request).unwrap(), request))
        .collect();
    assert_eq!(
        pages.outstanding(),
        taken,
        "the kept page did not serve the allocations"
    );
    assert_disjoint(&refilled);

    for (address, request) in refilled {
        heap.deallocate(&mut pages, address, request);
    }
    assert_eq!(heap.slab_pages(), 1);
}

/// Churning a class does not grow it.
///
/// The shape of a real workload: allocate a burst, free it, repeat. Before
/// pages were returned this rose without bound.
#[test]
fn churn_does_not_grow_the_heap() {
    let mut pages = Pages::with_budget(256);
    let mut heap = Heap::new();
    let request = Request::new(128, 8);
    let burst = PAGE_SIZE / 128 * 5;

    let mut high_water = 0;
    for _ in 0..8 {
        let live: Vec<u64> = (0..burst)
            .map(|_| heap.allocate(&mut pages, request).unwrap())
            .collect();
        high_water = high_water.max(pages.outstanding());
        for address in live {
            heap.deallocate(&mut pages, address, request);
        }
        assert_eq!(heap.slab_pages(), 1, "the class kept more than one page");
    }
    assert_eq!(high_water, 5, "the burst should need exactly five pages");
    assert_eq!(pages.outstanding(), 1);
}

/// A supply with nowhere to keep the count still works.
///
/// [`Backing::slab_free`] documents that returning zero is allowed, and what
/// it costs: pages are never returned. A heap that corrupted itself instead
/// would make that documented option a trap.
#[test]
fn a_backing_that_cannot_count_is_merely_wasteful() {
    #[derive(Debug, Default)]
    struct Forgetful {
        links: BTreeMap<u64, u64>,
        next: u64,
        outstanding: usize,
    }

    // SAFETY: a map, as `Pages`; no address here is real memory.
    unsafe impl Backing for Forgetful {
        fn allocate_pages(&mut self, order: u8) -> Option<u64> {
            if self.next == 0 {
                self.next = 0xFFFF_8000_0010_0000;
            }
            let address = self.next;
            self.next += ((1usize << order) * PAGE_SIZE) as u64;
            self.outstanding += 1 << order;
            Some(address)
        }
        fn deallocate_pages(&mut self, _address: u64, order: u8) {
            self.outstanding -= 1 << order;
        }
        fn read_link(&self, at: u64) -> u64 {
            self.links.get(&at).copied().unwrap_or(0)
        }
        fn write_link(&mut self, at: u64, value: u64) {
            let _ = self.links.insert(at, value);
        }
        fn slab_free(&self, _page: u64) -> u16 {
            0
        }
        fn set_slab_free(&mut self, _page: u64, _objects: u16) {}
    }

    let mut pages = Forgetful::default();
    let mut heap = Heap::new();
    let request = Request::new(16, 8);
    let live: Vec<u64> = (0..PAGE_SIZE / 16 * 3)
        .map(|_| heap.allocate(&mut pages, request).unwrap())
        .collect();
    for address in live {
        heap.deallocate(&mut pages, address, request);
    }

    assert_eq!(
        heap.slab_pages(),
        3,
        "nothing can be returned without a count"
    );
    assert_eq!(heap.slab_pages_returned(), 0);
    assert_eq!(
        heap.allocated_bytes(),
        0,
        "the objects are still accounted for"
    );
    // And the heap is still usable, which is the part that matters.
    let address = heap.allocate(&mut pages, request).unwrap();
    heap.deallocate(&mut pages, address, request);
}

// ---------------------------------------------------------------------------
// The reserve
// ---------------------------------------------------------------------------

#[test]
fn a_filled_reserve_holds_depth_objects_of_every_class() {
    let mut pages = Pages::with_budget(256);
    let mut heap = Heap::new();
    let mut reserve = Reserve::new();

    assert!(!reserve.is_full(1));
    reserve.fill(&mut heap, &mut pages, 4).unwrap();
    assert!(reserve.is_full(4));
    assert!(!reserve.is_full(5));
    assert_eq!(reserve.counts(), [4; CLASSES]);
    let held: usize = CLASS_SIZES.iter().map(|size| size * 4).sum();
    assert_eq!(
        heap.allocated_bytes(),
        held,
        "held objects are allocated, as far as the heap knows"
    );

    // A second fill to the same depth takes nothing.
    let before = heap.allocated_bytes();
    reserve.fill(&mut heap, &mut pages, 4).unwrap();
    assert_eq!(heap.allocated_bytes(), before);
}

#[test]
fn a_taken_object_is_freed_as_if_the_heap_had_handed_it_out() {
    let mut pages = Pages::with_budget(256);
    let mut heap = Heap::new();
    let mut reserve = Reserve::new();
    reserve.fill(&mut heap, &mut pages, 2).unwrap();
    let free_before = heap.free_objects(&pages);

    // A 24-byte, 8-aligned request is the 32-byte class's.
    let request = Request::new(24, 8);
    let object = reserve.take(&pages, request).unwrap();
    assert_eq!(object % 32, 0);
    assert_eq!(reserve.counts()[2], 1);
    heap.deallocate(&mut pages, object, request);

    let free_after = heap.free_objects(&pages);
    assert_eq!(
        free_after[2],
        free_before[2] + 1,
        "back on its own class's list"
    );
    let allocated = heap.allocated_bytes();
    let next = heap.allocate(&mut pages, request).unwrap();
    assert_eq!(next, object, "and handed out again from there");
    assert_eq!(heap.allocated_bytes(), allocated + 32);
}

#[test]
fn a_reserve_hands_out_distinct_objects_until_it_is_empty() {
    let mut pages = Pages::with_budget(256);
    let mut heap = Heap::new();
    let mut reserve = Reserve::new();
    reserve.fill(&mut heap, &mut pages, 3).unwrap();

    let mut live = Vec::new();
    for &size in &CLASS_SIZES {
        let request = Request::new(size, 1);
        for _ in 0..3 {
            live.push((reserve.take(&pages, request).unwrap(), request));
        }
        assert_eq!(
            reserve.take(&pages, request),
            None,
            "three held, three taken"
        );
    }
    // The heap's own allocations do not collide with what the reserve holds.
    for &size in &CLASS_SIZES {
        let request = Request::new(size, size);
        live.push((heap.allocate(&mut pages, request).unwrap(), request));
    }
    assert_disjoint(&live);
    assert!(reserve.is_full(0));
    assert!(!reserve.is_full(1));
}

#[test]
fn a_fill_that_runs_out_keeps_what_it_got() {
    // One page: enough for the first class's four objects, not for more.
    let mut pages = Pages::with_budget(1);
    let mut heap = Heap::new();
    let mut reserve = Reserve::new();
    assert_eq!(
        reserve.fill(&mut heap, &mut pages, 4),
        Err(HeapError::OutOfMemory)
    );
    assert_eq!(reserve.counts()[0], 4);
    assert!(!reserve.is_full(4));

    // With more pages the same fill finishes the job without redoing it.
    pages.budget = 256;
    reserve.fill(&mut heap, &mut pages, 4).unwrap();
    assert!(reserve.is_full(4));
}

#[test]
fn a_large_block_is_held_by_order_and_taken_by_order() {
    let mut pages = Pages::with_budget(64);
    let mut heap = Heap::new();
    let mut reserve = Reserve::new();

    // 5000 bytes is two pages: order one.
    let request = Request::new(5000, 8);
    assert_eq!(reserve.take(&pages, request), None);
    assert!(!reserve.holds_large());
    reserve.hold_large(&mut heap, &mut pages, request).unwrap();
    assert!(reserve.holds_large());
    assert_eq!(pages.outstanding(), 2);
    reserve.hold_large(&mut heap, &mut pages, request).unwrap();
    assert_eq!(pages.outstanding(), 2, "one block per order is enough");

    // A request of another order is not served by it.
    assert_eq!(reserve.take(&pages, Request::new(9000, 8)), None);
    let block = reserve.take(&pages, request).unwrap();
    assert!(!reserve.holds_large(), "taken, nothing is held");
    assert_eq!(reserve.take(&pages, request), None);
    heap.deallocate(&mut pages, block, request);
    assert_eq!(
        pages.outstanding(),
        0,
        "freed back to the page supply as its order"
    );
    assert_eq!(heap.large_pages(), 0);

    // A request a class serves needs nothing held.
    reserve
        .hold_large(&mut heap, &mut pages, Request::new(64, 8))
        .unwrap();
    assert_eq!(pages.outstanding(), 0);

    // Past the orders a reserve holds, it says so.
    let huge = Request::new(PAGE_SIZE << RESERVE_LARGE_ORDERS, 8);
    assert!(matches!(
        reserve.hold_large(&mut heap, &mut pages, huge),
        Err(HeapError::TooLarge(_))
    ));
}

#[test]
fn a_released_reserve_leaves_the_heap_as_it_found_it() {
    let mut pages = Pages::with_budget(256);
    let mut heap = Heap::new();
    let mut reserve = Reserve::new();
    reserve.fill(&mut heap, &mut pages, 5).unwrap();
    reserve
        .hold_large(&mut heap, &mut pages, Request::new(3 * PAGE_SIZE, 8))
        .unwrap();
    reserve.release(&mut heap, &mut pages);
    assert_eq!(reserve.counts(), [0; CLASSES]);
    assert_eq!(heap.allocated_bytes(), 0);
    assert_eq!(heap.large_pages(), 0);
}

#[test]
fn releasing_the_large_blocks_keeps_the_small_ones() {
    let mut pages = Pages::with_budget(256);
    let mut heap = Heap::new();
    let mut reserve = Reserve::new();
    reserve.fill(&mut heap, &mut pages, 2).unwrap();
    reserve
        .hold_large(&mut heap, &mut pages, Request::new(2 * PAGE_SIZE, 8))
        .unwrap();
    assert_eq!(heap.large_pages(), 2);
    reserve.release_large(&mut heap, &mut pages);
    assert_eq!(heap.large_pages(), 0);
    assert!(reserve.is_full(2));
}

#[test]
fn a_misaligned_request_is_not_served_from_the_reserve() {
    let mut pages = Pages::with_budget(64);
    let mut heap = Heap::new();
    let mut reserve = Reserve::new();
    reserve.fill(&mut heap, &mut pages, 1).unwrap();
    assert_eq!(reserve.take(&pages, Request::new(16, 3)), None);
}
