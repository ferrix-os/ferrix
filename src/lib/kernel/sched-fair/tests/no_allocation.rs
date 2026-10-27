//! Scheduling with lent slots allocates nothing (the kernel's finding F-23).
//!
//! An integration test, so that it can install a global allocator that counts:
//! the crate itself forbids `unsafe`, and implementing `GlobalAlloc` is.

#![allow(missing_docs, reason = "a test crate")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use ferrix_fallible as _;
use ferrix_sched_class as _;
use ferrix_sched_fair::{Config, EntityState, NICE_0_WEIGHT, RunQueue, Slot, Timeline};

std::thread_local! {
    /// Allocations this thread has made while counting.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    /// Whether this thread is counting.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

/// The system allocator, counting this thread's allocations on request.
struct Counting;

// SAFETY: every allocation is `System`'s, passed through unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
        // SAFETY: forwarded with the caller's layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `System.alloc` with `layout`.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// How many allocations `body` made on this thread.
fn allocations_in(body: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|count| count.set(0));
    COUNTING.with(|on| on.set(true));
    body();
    COUNTING.with(|on| on.set(false));
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn scheduling_with_lent_slots_allocates_nothing() {
    let mut queue: RunQueue<u64> = RunQueue::new(Config {
        slice_ns: 3_000_000,
    })
    .unwrap();
    let mut spare: Vec<Slot<u64>> = (0..32).map(|_| Slot::new().unwrap()).collect();
    let mut away: Vec<(u64, EntityState)> = Vec::with_capacity(64);
    let mut timeline: Timeline<u64> = Timeline::new();
    let mut seed = 0x5eed_u64;
    let made = allocations_in(|| {
        for id in 1..=32 {
            if let Some(slot) = spare.pop() {
                queue
                    .enqueue(id, id, EntityState::new(NICE_0_WEIGHT), slot)
                    .unwrap();
            }
        }
        for round in 0..2_000_u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let _ = queue.pick_next();
            let _ = queue.update_curr(seed % 3_000_000);
            if round % 3 == 0
                && let Some((id, _, state, slot)) = queue.remove_curr()
            {
                timeline.insert(round + 5, id, id, slot);
                away.push((id, state));
            }
            while let Some((id, payload, slot)) = timeline.pop_due(round) {
                let position = away.iter().position(|(at, _)| *at == id);
                if let Some((_, state)) = position.map(|at| away.swap_remove(at)) {
                    queue.enqueue(id, payload, state, slot).unwrap();
                }
            }
            if round % 50 == 0 {
                queue.level();
            }
        }
    });
    assert_eq!(made, 0, "a scheduling decision allocated");
    assert_eq!(queue.check_invariants(), Ok(()));
    assert!(timeline.check().is_ok());
}

#[test]
fn a_timeline_gives_entities_back_in_order_and_by_name() {
    let mut timeline: Timeline<&str> = Timeline::new();
    timeline.insert(30, 3, "three", Slot::new().unwrap());
    timeline.insert(10, 1, "one", Slot::new().unwrap());
    timeline.insert(20, 2, "two", Slot::new().unwrap());
    assert_eq!(timeline.first_due(), Some(10));
    assert!(timeline.pop_due(9).is_none(), "nothing is due before ten");
    let (id, payload, _) = timeline.pop_due(10).unwrap();
    assert_eq!((id, payload), (1, "one"));
    let (payload, _) = timeline.remove(3).unwrap();
    assert_eq!(payload, "three");
    assert_eq!(timeline.len(), 1);
    assert_eq!(timeline.check(), Ok(1));
}
