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
    for offset in 0..8 {
        assert_eq!(
            frames.state(block + offset),
            Some(State::Allocated),
            "frame {offset} of the block should be allocated"
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

    for step in 0..20_000 {
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
                assert_eq!(frames.state(block + offset), Some(State::Allocated));
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

#[test]
fn a_low_allocation_comes_from_below_the_limit() {
    let (mut entries, base, count) = arena(0, 1024);
    let mut frames = frames!(entries, base, count);

    // Drain everything below frame 16, then ask for a page below it anyway.
    let low: Vec<Frame> = (0..16).map(|_| frames.allocate_below(0, 16).unwrap()).collect();
    for frame in &low {
        assert!(*frame < 16, "{frame} is not below the limit it was asked for");
    }
    assert_eq!(
        frames.allocate_below(0, 16),
        None,
        "low memory runs out, and that is an answer rather than a shortage"
    );

    // The rest of the arena is untouched: a failed low allocation must not have
    // eaten high memory looking for it.
    assert_eq!(frames.free_frames(), 1024 - 16);
    assert!(frames.allocate_frame().unwrap() >= 16);
}

#[test]
fn a_low_allocation_prefers_the_lowest_block_it_splits() {
    let (mut entries, base, count) = arena(0, 1024);
    let mut frames = frames!(entries, base, count);

    // Nothing is on a small list yet, so this splits an order-10 block. The
    // half that is kept must be the low one, or the split has moved the
    // allocation *up* while claiming to look down.
    assert_eq!(frames.allocate_below(0, 1024), Some(0));
}

#[test]
fn a_low_allocation_refuses_a_block_that_straddles_the_limit() {
    let (mut entries, base, count) = arena(0, 16);
    let mut frames = frames!(entries, base, count);

    // One order-4 block covering 0..16. A limit of 8 cannot be satisfied out of
    // it as an order-4 allocation, but can as an order-0 one, because the split
    // keeps the low half.
    assert_eq!(frames.allocate_below(4, 8), None, "the whole block is too high");
    assert_eq!(frames.allocate_below(0, 8), Some(0));
}

#[test]
fn a_low_allocation_is_a_normal_allocation_to_free() {
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);
    let before = frames.free_blocks();

    let frame = frames.allocate_below(2, 64).unwrap();
    frames.deallocate(frame, 2).unwrap();

    assert_eq!(
        frames.free_blocks(),
        before,
        "a low allocation and its free must coalesce back to where they started"
    );
    assert_eq!(frames.free_frames(), 64);
}

#[test]
fn a_low_allocation_beyond_the_largest_order_is_refused() {
    let (mut entries, base, count) = arena(0, 64);
    let mut frames = frames!(entries, base, count);
    assert_eq!(frames.allocate_below(MAX_ORDER + 1, 64), None);
}
