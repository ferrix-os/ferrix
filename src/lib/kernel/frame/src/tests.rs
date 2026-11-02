//! Tests for the buddy allocator.
//!
//! The properties that matter are not "does one allocation work" but the
//! invariants a long-running kernel depends on: that no frame is ever handed
//! out twice, that everything freed comes back, and that a workload which frees
//! all of its memory leaves the free lists exactly as it found them. A buddy
//! allocator that fails the last one still works — it just fragments until it
//! cannot satisfy a large request, months later, on someone else's machine.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

/// A fixture covering `count` frames starting at `base`, all free.
fn arena(base: Frame, count: usize) -> (Vec<PageEntry>, Frame, usize) {
    (vec![PageEntry::RESERVED; count], base, count)
}

/// Build an allocator over a fixture and hand the whole range to it.
macro_rules! frames {
    ($entries:expr, $base:expr, $count:expr) => {{
        let mut frames = Frames::new(&mut $entries, $base);
        frames.insert_free($base, $count as u64);
        frames
    }};
}

/// A deterministic xorshift, so a failing stress test fails the same way twice.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

// ---------------------------------------------------------------------------
// Basics
// ---------------------------------------------------------------------------

#[test]
fn an_untouched_allocator_hands_nothing_out() {
    let (mut entries, base, _) = arena(0x100, 64);
    let mut frames = Frames::new(&mut entries, base);

    assert_eq!(frames.allocate_frame(), None, "no region was ever added");
    assert_eq!(frames.free_frames(), 0);
    assert_eq!(frames.managed_frames(), 0);
    assert_eq!(frames.state(0x100), Some(State::Reserved));
}

#[test]
fn a_frame_round_trips() {
    let (mut entries, base, count) = arena(0x100, 64);
    let mut frames = frames!(entries, base, count);

    assert_eq!(frames.free_frames(), 64);
    let frame = frames.allocate_frame().unwrap();
    assert_eq!(frames.state(frame), Some(State::Allocated));
    assert_eq!(frames.free_frames(), 63);
    assert_eq!(frames.entry(frame).unwrap().refcount(), 1);

    frames.deallocate(frame, 0).unwrap();
    assert_eq!(frames.free_frames(), 64);
    assert_eq!(frames.state(frame), Some(State::Free));
}

#[test]
fn a_block_covers_every_frame_it_claims() {
    let (mut entries, base, count) = arena(0x100, 64);
    let mut frames = frames!(entries, base, count);

    let block = frames.allocate(3).unwrap();
    assert_eq!(frames.free_frames(), 64 - 8);
    assert_eq!(frames.state(block), Some(State::Allocated));
    for offset in 1..8 {
        assert_eq!(
            frames.state(block + offset),
            Some(State::AllocatedTail),
            "frame {offset} of the block should be allocated, as a tail"
        );
    }
    assert_eq!(
        frames.state(block + 8),
        Some(State::Free),
        "the frame after the block should not be"
    );
}

#[test]
fn blocks_are_aligned_to_their_own_order() {
    let (mut entries, base, count) = arena(0x100, 1024);
    let mut frames = frames!(entries, base, count);

    for order in 0..=6u8 {
        let block = frames.allocate(order).unwrap();
        assert!(
            block.is_multiple_of(1u64 << order),
            "order {order} block at {block:#x} is not aligned"
        );
    }
}

#[test]
fn an_order_above_the_maximum_is_refused() {
    let (mut entries, base, count) = arena(0, 4096);
    let mut frames = frames!(entries, base, count);

    assert_eq!(frames.allocate(MAX_ORDER + 1), None);
    assert_eq!(
        frames.deallocate(0, MAX_ORDER + 1).unwrap_err(),
        FrameError::OrderTooLarge(MAX_ORDER + 1)
    );
}

// ---------------------------------------------------------------------------
// Splitting and coalescing
// ---------------------------------------------------------------------------

#[test]
fn a_region_is_added_as_the_largest_blocks_that_fit() {
    // 1024 frames from a 1024-aligned base is exactly one order-10 block.
    let (mut entries, base, count) = arena(1024, 1024);
    let frames = frames!(entries, base, count);

    let blocks = frames.free_blocks();
    assert_eq!(blocks[MAX_ORDER as usize], 1, "should be one maximal block");
    assert_eq!(
        blocks.iter().sum::<usize>(),
        1,
        "and nothing on any other list"
    );
}

#[test]
fn a_misaligned_region_is_split_into_aligned_blocks() {
    // Frames 1..8: one at 1, one at 2..4, one at 4..8 — the only decomposition
    // into naturally aligned blocks.
    let (mut entries, base, count) = arena(1, 7);
    let frames = frames!(entries, base, count);

    let blocks = frames.free_blocks();
    assert_eq!(blocks[0], 1, "frame 1 alone");
    assert_eq!(blocks[1], 1, "frames 2-3");
    assert_eq!(blocks[2], 1, "frames 4-7");
    assert_eq!(frames.free_frames(), 7);
}

#[test]
fn a_split_block_is_single_frames_that_merge_back_as_they_are_released() {
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);
    let before = frames.free_blocks();

    let block = frames.allocate(3).unwrap();
    assert_eq!(
        frames.release(block + 1),
        Err(FrameError::InsideBlock(block + 1))
    );
    frames.split(block, 3).unwrap();
    assert_eq!(
        frames.free_frames(),
        56,
        "splitting takes and frees nothing"
    );
    for frame in block..block + 8 {
        assert_eq!(frames.state(frame), Some(State::Allocated));
        assert_eq!(frames.entry(frame).unwrap().refcount(), 1);
    }

    // Released out of order, as a VMO gives pages back.
    for frame in [
        block + 5,
        block,
        block + 7,
        block + 2,
        block + 1,
        block + 6,
        block + 3,
    ] {
        assert_eq!(frames.release(frame), Ok(Released::Freed));
    }
    assert_ne!(frames.free_blocks(), before, "one frame still out");
    assert_eq!(frames.release(block + 4), Ok(Released::Freed));
    assert_eq!(frames.free_frames(), 64);
    assert_eq!(frames.free_blocks(), before, "the block re-formed");
}

#[test]
fn only_the_head_of_an_unshared_block_of_that_order_is_split() {
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);

    let block = frames.allocate(2).unwrap();
    assert_eq!(
        frames.split(block + 1, 2),
        Err(FrameError::InsideBlock(block + 1))
    );
    assert_eq!(frames.split(block, 3), Err(FrameError::WrongOrder(block)));
    assert_eq!(
        frames.split(block, MAX_ORDER + 1),
        Err(FrameError::OrderTooLarge(MAX_ORDER + 1))
    );
    let single = frames.allocate_frame().unwrap();
    assert_eq!(frames.share(single), Ok(2));
    assert_eq!(
        frames.split(single, 0),
        Err(FrameError::StillShared(single))
    );
    let free = frames.free_frames();
    frames.deallocate(block, 2).unwrap();
    assert_eq!(
        frames.free_frames(),
        free + 4,
        "a refused split left the block whole"
    );
}

#[test]
fn freeing_both_halves_merges_them() {
    let (mut entries, base, count) = arena(0, 1024);
    let mut frames = frames!(entries, base, count);

    let low = frames.allocate(2).unwrap();
    let high = frames.allocate(2).unwrap();
    assert_eq!(high, low + 4, "the two halves of one order-3 block");

    frames.deallocate(low, 2).unwrap();
    let before = frames.free_blocks();
    assert_eq!(before[2], 1, "one order-2 block waiting for its buddy");

    frames.deallocate(high, 2).unwrap();
    let after = frames.free_blocks();
    assert_eq!(after[2], 0, "the pair should have merged");
    assert!(after[3] > 0 || after[MAX_ORDER as usize] > 0);
}

#[test]
fn freeing_everything_restores_the_original_free_lists() {
    // The invariant that matters over a long run: an allocator that does not
    // return to its starting shape fragments a little on every cycle, and the
    // failure appears months later as a large allocation that cannot be met.
    let (mut entries, base, count) = arena(0, 4096);
    let mut frames = frames!(entries, base, count);

    let original = frames.free_blocks();
    let original_free = frames.free_frames();

    let mut taken = Vec::new();
    for order in [0u8, 3, 1, 5, 2, 0, 4, 7, 6, 2] {
        let block = frames.allocate(order).unwrap();
        taken.push((block, order));
    }
    assert_ne!(frames.free_blocks(), original, "the test did nothing");

    for (block, order) in taken {
        frames.deallocate(block, order).unwrap();
    }

    assert_eq!(frames.free_frames(), original_free);
    assert_eq!(
        frames.free_blocks(),
        original,
        "every block should have merged back to where it started"
    );
}

#[test]
fn splitting_a_large_block_leaves_the_remainder_available() {
    // One order-10 block, and a single frame taken out of it: the other 1023
    // frames must still be reachable, as one block per order on the way down.
    let (mut entries, base, count) = arena(0, 1024);
    let mut frames = frames!(entries, base, count);

    let frame = frames.allocate_frame().unwrap();
    assert_eq!(frame, 0);

    let blocks = frames.free_blocks();
    for order in 0..MAX_ORDER {
        assert_eq!(
            blocks[order as usize], 1,
            "order {order} should hold exactly one block after the split"
        );
    }
    assert_eq!(frames.free_frames(), 1023);
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn freeing_something_never_allocated_is_refused() {
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);

    assert_eq!(
        frames.deallocate(0, 0).unwrap_err(),
        FrameError::NotAllocated(0),
        "frame 0 is on a free list, not out with a caller"
    );
}

#[test]
fn freeing_the_same_block_twice_is_refused() {
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);

    let frame = frames.allocate(1).unwrap();
    frames.deallocate(frame, 1).unwrap();
    assert!(
        frames.deallocate(frame, 1).is_err(),
        "a double free must not corrupt the free lists"
    );
    assert_eq!(frames.free_frames(), 64, "and must not inflate the count");
}

#[test]
fn freeing_an_out_of_range_or_misaligned_block_is_refused() {
    let (mut entries, base, count) = arena(0x1000, 64);
    let mut frames = frames!(entries, base, count);

    assert_eq!(
        frames.deallocate(0, 0).unwrap_err(),
        FrameError::OutOfRange(0)
    );
    assert_eq!(
        frames.deallocate(0x9999, 0).unwrap_err(),
        FrameError::OutOfRange(0x9999)
    );

    let block = frames.allocate(2).unwrap();
    assert_eq!(
        frames.deallocate(block + 1, 2).unwrap_err(),
        FrameError::Misaligned(block + 1),
        "an order-2 block does not start one frame in"
    );
}

#[test]
fn a_hole_in_the_middle_is_never_handed_out() {
    // Two regions with a gap: the classic shape of a real memory map, where the
    // gap is firmware, MMIO, or the kernel's own image.
    let mut entries = vec![PageEntry::RESERVED; 64];
    let mut frames = Frames::new(&mut entries, 0);
    frames.insert_free(0, 16);
    frames.insert_free(48, 16);

    assert_eq!(frames.free_frames(), 32);
    assert_eq!(frames.managed_frames(), 32);
    for hole in 16..48 {
        assert_eq!(
            frames.state(hole),
            Some(State::Reserved),
            "frame {hole} is in the hole"
        );
    }

    // Drain it, and check nothing from the hole appears.
    let mut seen = Vec::new();
    while let Some(frame) = frames.allocate_frame() {
        assert!(
            !(16..48).contains(&frame),
            "frame {frame} came from the hole"
        );
        seen.push(frame);
    }
    assert_eq!(seen.len(), 32);
}

#[test]
fn blocks_never_straddle_a_hole() {
    // A region ending at 12 must not produce a block that runs past it, even
    // though 8..16 would be a properly aligned order-3 block.
    let mut entries = vec![PageEntry::RESERVED; 32];
    let mut frames = Frames::new(&mut entries, 0);
    frames.insert_free(0, 12);

    assert_eq!(frames.free_frames(), 12);
    let blocks = frames.free_blocks();
    assert_eq!(blocks[3], 1, "0-7");
    assert_eq!(blocks[2], 1, "8-11");
    assert_eq!(blocks.iter().sum::<usize>(), 2);
}

// ---------------------------------------------------------------------------
// Allocating below a limit
// ---------------------------------------------------------------------------

#[test]
fn a_block_below_a_limit_lies_wholly_below_it() {
    // One order-8 block at 0x100. The request is for the four frames at its
    // bottom, which is where splitting leaves the block handed out.
    let (mut entries, base, count) = arena(0x100, 0x100);
    let mut frames = frames!(entries, base, count);

    let block = frames.allocate_below(2, 0x110).unwrap();
    assert!(block + 4 <= 0x110, "block {block:#x} runs past the limit");
    assert_eq!(frames.free_frames(), 0x100 - 4);
    assert_eq!(frames.state(block), Some(State::Allocated));
    for offset in 1..4 {
        assert_eq!(frames.state(block + offset), Some(State::AllocatedTail));
    }
}

#[test]
fn a_low_block_is_found_behind_higher_ones() {
    // Free lists are not sorted: each insert pushes at the head, so the list
    // for order 0 here runs 17, 9, 0 and the one low frame is at its tail.
    // Only a search finds it, and taking it from there must leave the rest of
    // the list intact.
    let mut entries = vec![PageEntry::RESERVED; 64];
    let mut frames = Frames::new(&mut entries, 0);
    frames.insert_free(0, 1);
    frames.insert_free(9, 1);
    frames.insert_free(17, 1);

    assert_eq!(frames.allocate_below(0, 1), Some(0));
    assert_eq!(frames.allocate_below(0, 1), None, "there was only one");
    assert_eq!(frames.free_frames(), 2);
    assert_eq!(frames.allocate_frame(), Some(17), "the head is untouched");
    assert_eq!(frames.allocate_frame(), Some(9), "and so is the link to it");
    assert_eq!(frames.allocate_frame(), None);
}

#[test]
fn a_block_straddling_the_limit_is_not_taken() {
    // 0..64 is one order-6 block. Its lowest four frames end at 4, which is
    // past a limit of 3, so an order-2 request cannot be met from it even
    // though the block *starts* below the limit.
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);

    assert_eq!(frames.allocate_below(2, 3), None);
    assert_eq!(frames.free_frames(), 64, "a refusal changes nothing");
    assert_eq!(frames.free_blocks()[6], 1, "not even the free lists");
}

#[test]
fn nothing_below_the_limit_is_reported_rather_than_wrapping() {
    let (mut entries, base, count) = arena(0x1000, 64);
    let mut frames = frames!(entries, base, count);

    assert_eq!(
        frames.allocate_below(0, 0x1000),
        None,
        "every frame is at or above the limit"
    );
    assert_eq!(
        frames.allocate_below(MAX_ORDER + 1, u64::MAX),
        None,
        "nor is an order above the largest one granted"
    );
    assert_eq!(frames.free_frames(), 64);
}

#[test]
fn a_block_taken_below_a_limit_frees_like_any_other() {
    let (mut entries, base, count) = arena(0, 1024);
    let mut frames = frames!(entries, base, count);
    let original = frames.free_blocks();

    let low = frames.allocate_below(0, 0x100).unwrap();
    assert!(low < 0x100);
    frames.deallocate(low, 0).unwrap();
    assert_eq!(
        frames.free_blocks(),
        original,
        "it should merge back to where it started"
    );
}

// ---------------------------------------------------------------------------
// The one that would actually catch a bug
// ---------------------------------------------------------------------------

#[test]
fn a_long_random_workload_never_hands_out_the_same_frame_twice() {
    const FRAMES: usize = 4096;
    let (mut entries, base, count) = arena(0, FRAMES);
    let mut frames = frames!(entries, base, count);

    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let mut live: Vec<(Frame, u8)> = Vec::new();
    // One byte per frame: which live allocation owns it, or 0 for nobody.
    let mut owner = vec![0u32; FRAMES];
    let mut next_id = 1u32;

    // Miri interprets every step, and the books are balanced over the whole
    // live list after each one: at twenty thousand steps this one test was
    // 97% of the crate's nine-minute Miri run. Two thousand still takes the
    // arena through thousands of splits and merges, which is where an aliasing
    // mistake in the free lists would show; the full count stays on the host,
    // where it costs a fraction of a second.
    let steps = if cfg!(miri) { 2_000 } else { 20_000 };
    for step in 0..steps {
        let allocating = live.is_empty() || rng.below(100) < 55;

        if allocating {
            let order = (rng.below(6)) as u8;
            let Some(block) = frames.allocate(order) else {
                continue;
            };
            let id = next_id;
            next_id += 1;

            for offset in 0..(1u64 << order) {
                let slot = &mut owner[(block + offset) as usize];
                assert_eq!(
                    *slot,
                    0,
                    "step {step}: frame {} was already owned by {}",
                    block + offset,
                    *slot
                );
                *slot = id;
                let expected = if offset == 0 {
                    State::Allocated
                } else {
                    State::AllocatedTail
                };
                assert_eq!(frames.state(block + offset), Some(expected));
            }
            live.push((block, order));
        } else {
            let victim = rng.below(live.len() as u64) as usize;
            let (block, order) = live.swap_remove(victim);
            for offset in 0..(1u64 << order) {
                owner[(block + offset) as usize] = 0;
            }
            frames.deallocate(block, order).unwrap();
        }

        // The books must balance at every step: frames out with callers plus
        // frames on the free lists is the whole arena.
        let held: u64 = live.iter().map(|&(_, order)| 1u64 << order).sum();
        assert_eq!(
            held + frames.free_frames(),
            FRAMES as u64,
            "step {step}: {held} held + {} free",
            frames.free_frames()
        );
    }

    // And after giving everything back, the arena is whole again.
    for (block, order) in live {
        frames.deallocate(block, order).unwrap();
    }
    assert_eq!(frames.free_frames(), FRAMES as u64);
    assert_eq!(
        frames.free_blocks()[MAX_ORDER as usize],
        FRAMES / (1 << MAX_ORDER),
        "the arena should have merged back into maximal blocks"
    );
}

// ---------------------------------------------------------------------------
// Runs longer than a block
// ---------------------------------------------------------------------------

const BLOCK: u64 = 1 << MAX_ORDER;

#[test]
fn a_run_is_maximal_blocks_back_to_back_that_split_and_free_one_by_one() {
    let (mut entries, base, count) = arena(0, 4 * BLOCK as usize);
    let mut frames = frames!(entries, base, count);
    let before = frames.free_blocks();

    let run = frames.allocate_run(3).expect("three of four blocks");
    assert_eq!(frames.free_frames(), BLOCK, "one block left");
    for k in 0..3 {
        let block = run + k * BLOCK;
        assert_eq!(frames.state(block), Some(State::Allocated));
        assert_eq!(frames.entry(block).unwrap().order, MAX_ORDER);
    }
    // As the display core does: each block split, the tail past what it
    // needs given back at once, the rest page by page later.
    let needed = 2 * BLOCK + 10;
    for k in 0..3 {
        frames.split(run + k * BLOCK, MAX_ORDER).unwrap();
    }
    for frame in run + needed..run + 3 * BLOCK {
        assert_eq!(frames.release(frame), Ok(Released::Freed));
    }
    assert_eq!(frames.free_frames(), 4 * BLOCK - needed);
    for frame in (run..run + needed).rev() {
        assert_eq!(frames.release(frame), Ok(Released::Freed));
    }
    assert_eq!(frames.free_blocks(), before, "everything merged back");
}

#[test]
fn a_run_needs_every_block_free_and_adjacent() {
    let (mut entries, base, count) = arena(0, 4 * BLOCK as usize);
    let mut frames = frames!(entries, base, count);
    // One frame taken out of the second block leaves runs of one (the
    // first) and two (the third and fourth).
    assert_eq!(frames.claim(BLOCK + 7), Some(BLOCK + 7));
    assert_eq!(frames.allocate_run(3), None, "no three in a row");
    let free = frames.free_frames();
    assert_eq!(frames.allocate_run(2), Some(2 * BLOCK), "the two after it");
    assert_eq!(frames.free_frames(), free - 2 * BLOCK);
    assert_eq!(frames.allocate_run(2), None, "none left");
    assert_eq!(frames.allocate_run(0), None, "nothing is not a run");
    assert_eq!(frames.allocate_run(1), Some(0), "a run of one is a block");
}

#[test]
fn exhaustion_is_reported_rather_than_wrapping() {
    let (mut entries, base, count) = arena(0, 16);
    let mut frames = frames!(entries, base, count);

    let mut taken = Vec::new();
    while let Some(frame) = frames.allocate_frame() {
        taken.push(frame);
    }
    assert_eq!(taken.len(), 16);
    assert_eq!(frames.free_frames(), 0);
    assert_eq!(frames.allocate_frame(), None);
    assert_eq!(frames.allocate(4), None, "nor a block");

    for frame in taken {
        frames.deallocate(frame, 0).unwrap();
    }
    assert_eq!(frames.free_frames(), 16);
}

#[test]
fn entries_needed_matches_the_range_it_describes() {
    assert_eq!(Frames::entries_needed(0, 1024), 1024);
    assert_eq!(Frames::entries_needed(0x100, 0x200), 0x100);
    assert_eq!(
        Frames::entries_needed(10, 10),
        0,
        "an empty range needs none"
    );
    assert_eq!(Frames::entries_needed(20, 10), 0, "nor does a reversed one");
}

#[test]
fn the_covered_range_is_reported_honestly() {
    let (mut entries, base, count) = arena(0x1000, 256);
    let frames = frames!(entries, base, count);

    assert_eq!(frames.base(), 0x1000);
    assert_eq!(frames.end(), 0x1100);
    assert_eq!(frames.state(0x0FFF), None, "below the range");
    assert_eq!(frames.state(0x1100), None, "one past the range");
    assert!(frames.state(0x10FF).is_some());
}

#[test]
fn zero_is_a_valid_state() {
    // The kernel builds its side array by zeroing raw physical memory and then
    // forming a `&mut [PageEntry]` over it. An enum holding a discriminant it
    // does not define is undefined behaviour, so zero has to be one this type
    // defines -- and it is, because `Reserved` is explicitly 0.
    //
    // Asserted rather than transmuted: this crate is `forbid(unsafe_code)`, and
    // a test that reached for `mem::zeroed` to check a claim about zeroed memory
    // would be the one place the rule got bent.
    assert_eq!(State::Reserved as u8, 0);
    assert_eq!(State::Free as u8, 1);
    assert_eq!(State::FreeTail as u8, 2);
    assert_eq!(State::Allocated as u8, 3);
    assert_eq!(State::AllocatedTail as u8, 4);
    assert_eq!(
        size_of::<State>(),
        1,
        "repr(u8), so the array size is predictable"
    );

    // And the array `Frames::new` produces is reserved throughout, whatever the
    // memory held before -- which is the property the kernel actually relies on.
    let mut entries = vec![PageEntry::RESERVED; 8];
    let frames = Frames::new(&mut entries, 0);
    for frame in 0..8 {
        assert_eq!(frames.state(frame), Some(State::Reserved));
    }
    assert_eq!(frames.free_frames(), 0);
}

// ---------------------------------------------------------------------------
// Reference counts: the copy-on-write primitive
// ---------------------------------------------------------------------------
//
// Stage 6 shares a frame between a parent and a child at `fork` and separates
// them again at the first write fault. What matters here is not that a counter
// counts, but that the two failure modes are impossible: a frame freed while
// somebody still maps it, and a frame nobody frees because both sides thought
// the other would.

#[test]
fn a_fresh_allocation_has_exactly_one_reference() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let frame = frames.allocate_frame().unwrap();
    assert_eq!(frames.entry(frame).unwrap().refcount(), 1);
}

#[test]
fn sharing_raises_the_count_and_releasing_lowers_it() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let frame = frames.allocate_frame().unwrap();
    assert_eq!(frames.share(frame), Ok(2));
    assert_eq!(frames.share(frame), Ok(3));
    assert_eq!(frames.release(frame), Ok(Released::Shared(2)));
    assert_eq!(frames.release(frame), Ok(Released::Shared(1)));
    assert_eq!(frames.entry(frame).unwrap().refcount(), 1);
}

#[test]
fn a_shared_frame_survives_a_release_and_stays_allocated() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let frame = frames.allocate_frame().unwrap();
    let free_after_allocating = frames.free_frames();
    assert_eq!(frames.share(frame), Ok(2));

    // The child exits. The parent still has the page mapped, so the frame must
    // not go back to the allocator -- and must not be counted free, or the next
    // allocation hands out a frame the parent is reading.
    assert_eq!(frames.release(frame), Ok(Released::Shared(1)));
    assert_eq!(frames.state(frame), Some(State::Allocated));
    assert_eq!(frames.free_frames(), free_after_allocating);
}

#[test]
fn the_last_release_returns_the_frame() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let before = frames.free_frames();
    let frame = frames.allocate_frame().unwrap();
    assert_eq!(frames.share(frame), Ok(2));

    assert_eq!(frames.release(frame), Ok(Released::Shared(1)));
    assert_eq!(frames.release(frame), Ok(Released::Freed));
    assert_eq!(frames.state(frame), Some(State::Free));
    assert_eq!(
        frames.free_frames(),
        before,
        "every frame is back once the last reference goes"
    );
}

#[test]
fn a_shared_frame_cannot_be_deallocated_directly() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let frame = frames.allocate_frame().unwrap();
    assert_eq!(frames.share(frame), Ok(2));

    // This is the bug the guard exists for: one side of a fork calling the
    // allocator directly instead of `release`.
    assert_eq!(
        frames.deallocate(frame, 0),
        Err(FrameError::StillShared(frame))
    );
    assert_eq!(frames.state(frame), Some(State::Allocated));
    assert_eq!(frames.entry(frame).unwrap().refcount(), 2);
}

#[test]
fn an_unshared_frame_deallocates_as_it_always_did() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    // The guard must not disturb the kernel's own allocations, which never
    // share and are freed with `deallocate` throughout the tree.
    let frame = frames.allocate(3).unwrap();
    assert_eq!(frames.deallocate(frame, 3), Ok(()));
    assert_eq!(frames.state(frame), Some(State::Free));
}

#[test]
fn a_free_frame_can_be_neither_shared_nor_released() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let frame = frames.allocate_frame().unwrap();
    frames.deallocate(frame, 0).unwrap();

    assert_eq!(frames.share(frame), Err(FrameError::NotAllocated(frame)));
    assert_eq!(frames.release(frame), Err(FrameError::NotAllocated(frame)));
}

#[test]
fn a_frame_outside_the_arena_is_refused_by_both() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let outside = 0x400;
    assert_eq!(frames.share(outside), Err(FrameError::OutOfRange(outside)));
    assert_eq!(
        frames.release(outside),
        Err(FrameError::OutOfRange(outside))
    );
}

#[test]
fn a_saturated_count_is_refused_rather_than_wrapped() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let frame = frames.allocate_frame().unwrap();
    // Reaching u32::MAX a share at a time is not a test anybody can run, so the
    // count is put there directly. In-crate access, which is why this lives
    // beside the allocator rather than in `src/tests/`.
    frames.entry_mut(frame).unwrap().refcount = u32::MAX;

    assert_eq!(
        frames.share(frame),
        Err(FrameError::TooManyReferences(frame))
    );
    assert_eq!(
        frames.entry(frame).unwrap().refcount(),
        u32::MAX,
        "a refused share leaves the count alone"
    );
}

#[test]
fn a_shared_frame_is_reallocatable_only_after_the_last_reference() {
    let (mut entries, base, count) = arena(0x200, 4);
    let mut frames = frames!(entries, base, count);

    // Take everything, share one, then drain the allocator dry.
    let shared = frames.allocate_frame().unwrap();
    assert_eq!(frames.share(shared), Ok(2));
    while frames.allocate_frame().is_some() {}
    assert_eq!(frames.free_frames(), 0);

    assert_eq!(frames.release(shared), Ok(Released::Shared(1)));
    assert_eq!(
        frames.allocate_frame(),
        None,
        "a frame with a reference left is not available"
    );

    assert_eq!(frames.release(shared), Ok(Released::Freed));
    assert_eq!(frames.allocate_frame(), Some(shared));
}

// ---------------------------------------------------------------------------
// Counts belong to single-frame heads
// ---------------------------------------------------------------------------
//
// A count lives on the head of an allocation, and `release` frees at order 0.
// So the only frame a count can describe honestly is the head of an order-0
// block. Before these were refused, a tail's count was whatever the last owner
// left and nothing read it, and releasing the head of a larger block freed one
// frame of it.

#[test]
fn a_tail_frame_can_be_neither_shared_nor_released() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let block = frames.allocate(3).unwrap();
    let tail = block + 1;
    assert_eq!(frames.share(tail), Err(FrameError::InsideBlock(tail)));
    assert_eq!(frames.release(tail), Err(FrameError::InsideBlock(tail)));
    assert_eq!(frames.entry(block).unwrap().refcount(), 1);
    assert_eq!(frames.state(tail), Some(State::AllocatedTail));
}

#[test]
fn the_head_of_a_larger_block_can_be_neither_shared_nor_released() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let block = frames.allocate(3).unwrap();
    let free = frames.free_frames();

    // Released at order 0, this frees one frame of eight and leaves seven
    // marked allocated with nobody holding them.
    assert_eq!(frames.release(block), Err(FrameError::WrongOrder(block)));
    assert_eq!(frames.share(block), Err(FrameError::WrongOrder(block)));
    assert_eq!(frames.free_frames(), free, "nothing came back");
    assert_eq!(frames.state(block), Some(State::Allocated));
    assert_eq!(frames.deallocate(block, 3), Ok(()));
}

#[test]
fn a_block_freed_at_the_wrong_order_is_refused() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let block = frames.allocate(3).unwrap();
    let free = frames.free_frames();

    // Smaller: would leak the rest of the block.
    assert_eq!(
        frames.deallocate(block, 0),
        Err(FrameError::WrongOrder(block))
    );
    // Larger: would free frames the block never had.
    assert_eq!(
        frames.deallocate(block, 4),
        Err(FrameError::WrongOrder(block))
    );
    assert_eq!(frames.free_frames(), free);
    assert_eq!(frames.deallocate(block, 3), Ok(()));
    assert_eq!(frames.free_frames(), 32);
}

#[test]
fn a_tail_cannot_be_freed_as_a_block_of_its_own() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    let block = frames.allocate(3).unwrap();
    let free = frames.free_frames();

    // Aligned to its own order, so only the state can tell it is a tail.
    assert_eq!(
        frames.deallocate(block + 4, 2),
        Err(FrameError::InsideBlock(block + 4))
    );
    assert_eq!(frames.free_frames(), free);
    assert_eq!(frames.state(block + 4), Some(State::AllocatedTail));
    // The block is intact, and still frees whole through its head.
    assert_eq!(frames.deallocate(block, 3), Ok(()));
    assert_eq!(frames.free_frames(), 32);
}

#[test]
fn a_freed_frame_is_claimed_back_exactly_even_after_it_merged() {
    let (mut entries, base, count) = arena(0x200, 32);
    let mut frames = frames!(entries, base, count);

    // Freed into a block of 32, so the plain allocator would hand out the
    // block's head, not this.
    let wanted = base + 13;
    assert_eq!(frames.claim(wanted), Some(wanted));
    assert_eq!(frames.state(wanted), Some(State::Allocated));
    assert_eq!(frames.free_frames(), 31);
    // Claimed once; a second claim of an allocated frame is refused.
    assert_eq!(frames.claim(wanted), None);

    // Everything else is still there, exactly once: all 31 come out, none of
    // them the claimed frame.
    let mut seen = Vec::new();
    while let Some(frame) = frames.allocate(0) {
        assert_ne!(frame, wanted);
        assert!(!seen.contains(&frame));
        seen.push(frame);
    }
    assert_eq!(seen.len(), 31);

    // And it frees and merges like any frame.
    for frame in seen {
        assert_eq!(frames.deallocate(frame, 0), Ok(()));
    }
    assert_eq!(frames.deallocate(wanted, 0), Ok(()));
    assert_eq!(frames.free_frames(), 32);
    assert_eq!(frames.claim(base + 64), None);
}

#[test]
fn a_frame_is_charged_to_nobody_until_it_is_told_otherwise() {
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);
    let frame = frames.allocate(0).unwrap();
    assert_eq!(frames.owner(frame), NO_OWNER);
    frames.set_owner(frame, 7).unwrap();
    assert_eq!(frames.owner(frame), 7);
    // A second reference keeps it, and the last one's release frees it.
    assert_eq!(frames.share(frame), Ok(2));
    assert_eq!(frames.release(frame), Ok(Released::Shared(1)));
    assert_eq!(frames.owner(frame), 7);
    assert_eq!(frames.release(frame), Ok(Released::Freed));
    assert_eq!(frames.owner(frame), NO_OWNER);
    // Allocated again, it is nobody's.
    let again = frames.claim(frame).unwrap();
    assert_eq!(frames.owner(again), NO_OWNER);
}

#[test]
fn only_an_allocated_single_frame_takes_an_owner() {
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);
    let block = frames.allocate(2).unwrap();
    assert_eq!(
        frames.set_owner(block, 1),
        Err(FrameError::WrongOrder(block))
    );
    assert_eq!(
        frames.set_owner(block + 1, 1),
        Err(FrameError::InsideBlock(block + 1))
    );
    assert_eq!(frames.owner(block), NO_OWNER);
    let free = block + 4;
    assert!(frames.set_owner(free, 1).is_err());
}

#[test]
fn no_frame_comes_out_of_a_split_or_a_claim_with_an_owner() {
    // Every frame's link is dirtied by free lists first, so a stale link read
    // as an owner would show.
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);
    let singles: Vec<Frame> = (0..16).map(|_| frames.allocate(0).unwrap()).collect();
    for &frame in singles.iter().rev() {
        frames.deallocate(frame, 0).unwrap();
    }
    let block = frames.allocate(3).unwrap();
    frames.split(block, 3).unwrap();
    for frame in block..block + 8 {
        assert_eq!(
            frames.owner(frame),
            NO_OWNER,
            "frame {frame} of a split block"
        );
    }
    for frame in block..block + 8 {
        let _ = frames.release(frame).unwrap();
    }
    for frame in [5, 9, 30] {
        let claimed = frames.claim(frame).unwrap();
        assert_eq!(frames.owner(claimed), NO_OWNER, "claimed frame {frame}");
    }
}

/// The scrubber's claim rule (`docs/RAM-SCRUBBER.md` §3.1): the free block
/// holding a frame is found by its head and order, and taking it is allowed
/// only while another free block of that order or larger exists.
///
/// Verifies: L.mm.101
#[test]
fn a_claim_leaves_another_block_of_its_order() {
    // 64 frames at 0x100: one order-6 block.
    let (mut entries, base, count) = arena(0x100, 64);
    let mut frames = frames!(entries, base, count);
    assert_eq!(frames.free_block_of(0x123), Some((0x100, 6)));
    // The only block of order 6: no other one.
    assert!(!frames.another_free_block(0x100, 6));
    // Take one frame: the block splits, 32 + 16 + ... + 1 left.
    let taken = frames.allocate_frame().unwrap();
    assert_eq!(frames.free_block_of(taken), None);
    let (head, order) = frames.free_block_of(0x120).unwrap();
    assert_eq!(order, 5);
    // The order-5 block is the only one of order 5 or more.
    assert!(!frames.another_free_block(head, order));
    // The order-0 buddy of the taken frame: smaller blocks exist above it.
    let (small, small_order) = frames.free_block_of(taken ^ 1).unwrap();
    assert_eq!(small_order, 0);
    assert!(frames.another_free_block(small, small_order));
    // Two blocks of one order: either may go, the other stays.
    let (mut two, base2, count2) = arena(0x200, 32);
    let mut pair = Frames::new(&mut two, base2);
    pair.insert_free(0x200, 16);
    pair.insert_free(0x210, 16);
    assert_eq!(pair.free_block_of(0x205), Some((0x200, 4)));
    assert!(pair.another_free_block(0x200, 4));
    assert!(pair.another_free_block(0x210, 4));
    let _ = count2;
}
