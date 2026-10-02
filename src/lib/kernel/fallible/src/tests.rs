//! Tests for fallible construction, over a global allocator that records what
//! it is asked for and can be told to refuse.
//!
//! Two kinds of claim are checked here. The first is this crate's own: that
//! every constructor reports a refusal as [`AllocError`] and leaves its
//! container as it was. The second is a claim about the pinned standard
//! library that the kernel's reserve rests on -- that `Arc::new` makes exactly
//! one allocation of [`arc_layout`], that a `BTreeMap` insert makes no more
//! than [`btree_insert_nodes`] allocations of at most [`btree_node_bound`]
//! bytes, and that removal never allocates. Those are not promises the
//! standard library makes, so they are measured, and a toolchain bump that
//! broke one would fail here.
//!
//! The allocator's state is per thread, so the test harness's parallel
//! threads do not see each other's refusals or records.

extern crate std;

use core::alloc::GlobalAlloc;
use core::cell::{Cell, RefCell};
use std::alloc::System;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::vec;

use super::*;

/// Most allocations one test records.
const LOG: usize = 64;

std::thread_local! {
    /// Refuse the allocation this many allocations from now: zero refuses the
    /// next one. `None` refuses nothing.
    static REFUSE_IN: Cell<Option<usize>> = const { Cell::new(None) };
    /// Whether this thread's allocations are being recorded.
    static RECORDING: Cell<bool> = const { Cell::new(false) };
    /// What was recorded: size and alignment.
    static RECORDED: RefCell<[(usize, usize); LOG]> = const { RefCell::new([(0, 0); LOG]) };
    /// How many allocations were recorded, which may exceed [`LOG`].
    static COUNT: Cell<usize> = const { Cell::new(0) };
    /// What the injection policy answers on this thread.
    static INJECT: Cell<bool> = const { Cell::new(false) };
}

/// The system allocator, recording and refusing on request.
struct Recording;

// SAFETY: every allocation is `System`'s, passed through unchanged, or null.
unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let refuse = REFUSE_IN
            .try_with(|slot| match slot.get() {
                Some(0) => {
                    slot.set(None);
                    true
                }
                Some(n) => {
                    slot.set(Some(n - 1));
                    false
                }
                None => false,
            })
            .unwrap_or(false);
        if refuse {
            return core::ptr::null_mut();
        }
        if RECORDING.try_with(Cell::get).unwrap_or(false) {
            note(layout);
        }
        // SAFETY: forwarded with the caller's layout, whose contract the
        // caller has already met.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `System.alloc` with `layout`, above.
        unsafe { System.dealloc(pointer, layout) }
    }
}

/// Add `layout` to this thread's record.
fn note(layout: Layout) {
    let index = COUNT.try_with(|count| count.replace(count.get() + 1));
    let Ok(index) = index else { return };
    let _ = RECORDED.try_with(|log| {
        if let Some(slot) = log.borrow_mut().get_mut(index) {
            *slot = (layout.size(), layout.align());
        }
    });
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

/// Refuse the `n`th allocation from now, counting from zero.
fn refuse_in(n: usize) {
    REFUSE_IN.with(|slot| slot.set(Some(n)));
}

/// Stop refusing.
fn refuse_none() {
    REFUSE_IN.with(|slot| slot.set(None));
}

/// Run `body` and return what it allocated, as (size, alignment) pairs.
fn record<R>(body: impl FnOnce() -> R) -> (R, Vec<(usize, usize)>) {
    COUNT.with(|count| count.set(0));
    RECORDING.with(|on| on.set(true));
    let out = body();
    RECORDING.with(|on| on.set(false));
    let count = COUNT.with(Cell::get);
    assert!(
        count <= LOG,
        "{count} allocations is more than one test records"
    );
    let log = RECORDED.with(|log| log.borrow().get(..count).map(<[_]>::to_vec));
    (out, log.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// This crate's constructors
// ---------------------------------------------------------------------------

#[test]
fn try_box_holds_its_value_and_reports_a_refusal() {
    let boxed = try_box([7u64; 4]).unwrap();
    assert_eq!(*boxed, [7; 4]);

    refuse_in(0);
    assert_eq!(try_box([7u64; 4]), Err(AllocError));
    refuse_none();
}

#[test]
fn try_box_of_a_zero_sized_type_allocates_nothing() {
    refuse_in(0);
    let (boxed, log) = record(|| try_box(()));
    refuse_none();
    assert!(boxed.is_ok());
    assert!(log.is_empty());
}

#[test]
fn try_box_allocates_exactly_what_box_new_does() {
    let (_, ours) = record(|| try_box(0u128));
    let (_, theirs) = record(|| Box::new(0u128));
    assert_eq!(ours, theirs);
}

#[test]
fn a_dropped_try_box_frees_its_value() {
    let shared = Arc::new(1u8);
    let boxed = try_box(Arc::clone(&shared)).unwrap();
    assert_eq!(Arc::strong_count(&shared), 2);
    drop(boxed);
    assert_eq!(Arc::strong_count(&shared), 1);
}

#[test]
fn boxed_slices_and_strs_are_exact_and_allocated_once() {
    let (slice, log) = record(|| try_boxed_slice(&[1u32, 2, 3]));
    assert_eq!(&*slice.unwrap(), &[1, 2, 3]);
    assert_eq!(log, vec![(12, 4)]);

    let (filled, log) = record(|| try_boxed_filled(7u16, 5));
    assert_eq!(&*filled.unwrap(), &[7, 7, 7, 7, 7]);
    assert_eq!(log, vec![(10, 2)]);

    let (text, log) = record(|| try_boxed_str("ferrix"));
    assert_eq!(&*text.unwrap(), "ferrix");
    assert_eq!(log, vec![(6, 1)]);

    refuse_in(0);
    assert_eq!(try_boxed_slice(&[1u8]), Err(AllocError));
    refuse_in(0);
    assert_eq!(try_boxed_str("x"), Err(AllocError));
    refuse_in(0);
    assert_eq!(try_boxed_filled(0u8, 3), Err(AllocError));
    refuse_none();
}

#[test]
fn a_refused_push_leaves_the_vector_as_it_was() {
    let mut vec: Vec<u32> = Vec::new();
    try_push(&mut vec, 1).unwrap();
    vec.shrink_to_fit();
    refuse_in(0);
    assert_eq!(try_push(&mut vec, 2), Err(AllocError));
    refuse_none();
    assert_eq!(vec, [1]);
    try_push(&mut vec, 2).unwrap();
    assert_eq!(vec, [1, 2]);
}

#[test]
fn push_within_never_allocates() {
    let mut vec = try_with_capacity::<u8>(2).unwrap();
    let room = vec.capacity();
    let (refused, log) = record(|| {
        for n in 0..room {
            assert_eq!(push_within(&mut vec, u8::try_from(n).unwrap()), Ok(()));
        }
        push_within(&mut vec, 0xFF)
    });
    assert_eq!(refused, Err(0xFF));
    assert!(log.is_empty());
    assert_eq!(vec.len(), room);
}

#[test]
fn slices_extend_convert_and_fill() {
    let mut vec = try_to_vec(&[1u8, 2]).unwrap();
    try_extend_from_slice(&mut vec, &[3, 4]).unwrap();
    assert_eq!(vec, [1, 2, 3, 4]);

    vec.shrink_to_fit();
    refuse_in(0);
    assert_eq!(try_extend_from_slice(&mut vec, &[5]), Err(AllocError));
    refuse_none();
    assert_eq!(vec, [1, 2, 3, 4]);

    assert_eq!(try_filled(9u16, 3).unwrap(), [9, 9, 9]);
    refuse_in(0);
    assert_eq!(try_filled(9u16, 3), Err(AllocError));
    refuse_in(0);
    assert_eq!(try_to_vec(&[1u8]), Err(AllocError));
    refuse_none();

    try_resize(&mut vec, 6, 0).unwrap();
    assert_eq!(vec, [1, 2, 3, 4, 0, 0]);
    try_resize(&mut vec, 2, 0).unwrap();
    assert_eq!(vec, [1, 2]);

    try_insert(&mut vec, 1, 7).unwrap();
    try_insert(&mut vec, 99, 8).unwrap();
    assert_eq!(vec, [1, 7, 2, 8]);
}

#[test]
fn collect_and_extend_reserve_once_for_a_sized_iterator() {
    let (collected, log) = record(|| try_collect(0u32..10));
    assert_eq!(collected.unwrap(), (0..10).collect::<Vec<_>>());
    assert_eq!(
        log.len(),
        1,
        "one reservation for an iterator that knows its length"
    );

    // An iterator that does not say how long it is grows one step at a time.
    let unsized_iter = (0u32..100).filter(|n| n % 3 == 0);
    let collected = try_collect(unsized_iter).unwrap();
    assert_eq!(collected.len(), 34);

    refuse_in(0);
    assert_eq!(try_collect(0u8..4), Err(AllocError));
    refuse_none();

    let mut into = try_to_vec(&[1u8]).unwrap();
    let mut from = try_to_vec(&[2u8, 3]).unwrap();
    try_append(&mut into, &mut from).unwrap();
    assert_eq!(into, [1, 2, 3]);
    assert!(from.is_empty());
}

#[test]
fn deques_push_at_both_ends_and_report_a_refusal() {
    let mut queue = try_deque_with_capacity::<u8>(1).unwrap();
    try_push_back(&mut queue, 2).unwrap();
    try_push_front(&mut queue, 1).unwrap();
    assert_eq!(queue, [1, 2]);
    queue.shrink_to_fit();
    refuse_in(0);
    assert_eq!(try_push_back(&mut queue, 3), Err(AllocError));
    refuse_in(0);
    assert_eq!(try_push_front(&mut queue, 0), Err(AllocError));
    refuse_none();
    assert_eq!(queue, [1, 2]);
}

#[test]
fn strings_copy_append_and_format() {
    let mut text = try_string("fer").unwrap();
    try_push_str(&mut text, "rix").unwrap();
    assert_eq!(text, "ferrix");

    let formatted = try_format(format_args!("{text} {} {:#x}", 3, 255)).unwrap();
    assert_eq!(formatted, "ferrix 3 0xff");

    text.shrink_to_fit();
    refuse_in(0);
    assert_eq!(try_push_str(&mut text, "!"), Err(AllocError));
    refuse_none();
    assert_eq!(text, "ferrix");

    // The first piece fits the first reservation; the long second one needs
    // another, and that is the one refused.
    let long = "a piece longer than the first reservation";
    refuse_in(1);
    let refused = try_format(format_args!("{} and {long}", "a"));
    refuse_none();
    assert_eq!(refused, Err(AllocError));
}

#[test]
fn an_armed_injector_fails_every_constructor_without_allocating() {
    // The policy answers per thread, so arming it globally does not make the
    // harness's other threads fail.
    fn policy() -> bool {
        INJECT.with(Cell::get)
    }
    let _ = set_injector(policy);
    assert!(
        !set_injector(policy),
        "only the first injector is installed"
    );
    arm(true);
    INJECT.with(|on| on.set(true));

    let mut vec: Vec<u8> = Vec::new();
    let mut queue: VecDeque<u8> = VecDeque::new();
    let mut text = String::new();
    let (results, log) = record(|| {
        [
            try_box(1u8).err(),
            try_boxed_slice(&[1u8]).err(),
            try_boxed_filled(1u8, 1).err(),
            try_boxed_str("x").err(),
            try_push(&mut vec, 1).err(),
            try_reserve(&mut vec, 1).err(),
            try_with_capacity::<u8>(1).err(),
            try_to_vec(&[1u8]).err(),
            try_collect(0u8..1).err(),
            try_push_back(&mut queue, 1).err(),
            try_push_str(&mut text, "x").err(),
            try_string("x").err(),
            try_format(format_args!("{}", 1)).err(),
            check().err(),
        ]
    });
    INJECT.with(|on| on.set(false));
    assert!(
        results.iter().all(|result| *result == Some(AllocError)),
        "{results:?}"
    );
    assert!(
        log.is_empty(),
        "an injected failure allocates nothing: {log:?}"
    );

    // Room reserved ahead is not an allocation, so it is not failed: what a
    // caller set aside stays usable while allocations fail.
    INJECT.with(|on| on.set(false));
    let mut roomy: Vec<u8> = Vec::with_capacity(4);
    let mut deque: VecDeque<u8> = VecDeque::with_capacity(4);
    INJECT.with(|on| on.set(true));
    let reserved = (
        try_reserve(&mut roomy, 4),
        try_push(&mut roomy, 1),
        try_reserve_deque(&mut deque, 4),
        try_reserve(&mut roomy, 4).err(),
    );
    INJECT.with(|on| on.set(false));
    assert_eq!(reserved, (Ok(()), Ok(()), Ok(()), Some(AllocError)));

    // Disarmed, or asked on a thread whose policy says no, nothing fails.
    assert!(try_box(1u8).is_ok());
    arm(false);
    INJECT.with(|on| on.set(true));
    assert!(try_box(1u8).is_ok());
    INJECT.with(|on| on.set(false));
}

// ---------------------------------------------------------------------------
// The standard library, as the kernel's reserve relies on it
// ---------------------------------------------------------------------------

/// Assert that `Arc::new(value)` makes exactly one allocation, of
/// `arc_layout::<T>()`, and that `Arc::new_cyclic` makes the same one.
fn arc_allocates_arc_layout<T>(make: impl Fn() -> T) {
    let expected = arc_layout::<T>().map(|layout| (layout.size(), layout.align()));
    // The value is made outside the recording: a `String` payload has an
    // allocation of its own.
    let value = make();
    let (arc, log) = record(|| Arc::new(value));
    drop(arc);
    assert_eq!(log.first().copied(), expected);
    assert_eq!(log.len(), 1, "Arc::new allocates once: {log:?}");

    let value = make();
    let (arc, log) = record(|| Arc::new_cyclic(move |_| value));
    drop(arc);
    assert_eq!(log.first().copied(), expected);
    assert_eq!(log.len(), 1, "Arc::new_cyclic allocates once: {log:?}");
}

#[test]
fn arc_new_allocates_exactly_arc_layout() {
    #[derive(Clone, Copy)]
    #[repr(align(64))]
    struct Aligned(#[expect(dead_code, reason = "only its layout is measured")] u8);

    arc_allocates_arc_layout(|| 1u8);
    arc_allocates_arc_layout(|| 1u64);
    arc_allocates_arc_layout(|| 1u128);
    arc_allocates_arc_layout(|| ());
    arc_allocates_arc_layout(|| [0u8; 3]);
    arc_allocates_arc_layout(|| [0u64; 600]);
    arc_allocates_arc_layout(|| Aligned(1));
    arc_allocates_arc_layout(|| String::from("payload"));
}

/// The most internal levels a map of `len` entries can have: every non-root
/// leaf holds at least five keys and every non-root internal node at least
/// six children, so height `h` needs at least `10 * 6^(h - 1)` entries.
fn max_height(len: usize) -> usize {
    let mut height = 0;
    let mut needed = 10;
    while needed <= len {
        height += 1;
        needed *= 6;
    }
    height
}

/// Insert `keys` one at a time into a fresh map, checking each insert's
/// allocations against the bounds, and then that removing everything
/// allocates nothing.
fn map_inserts_within_bounds<K: Ord + Copy, V: Copy>(keys: &[K], value: V) {
    let bound = btree_node_bound(
        size_of::<K>(),
        size_of::<V>(),
        align_of::<K>().max(align_of::<V>()),
    );
    let mut map = BTreeMap::new();
    let mut most = 0;
    for (index, key) in keys.iter().enumerate() {
        let (_, log) = record(|| map.insert(*key, value));
        let allowed = btree_insert_nodes(max_height(index + 1));
        assert!(
            log.len() <= allowed,
            "insert {index} made {} allocations",
            log.len()
        );
        for (size, _) in &log {
            assert!(
                *size <= bound,
                "a {size}-byte node against a bound of {bound}"
            );
        }
        most = most.max(log.len());
    }
    assert!(most >= 2, "the keys were too few to split anything");

    let (_, log) = record(|| {
        let _ = map.pop_first();
        map.retain(|_, _| true);
        if let Some(key) = keys.get(keys.len() / 2) {
            let _ = map.remove(key);
        }
        while map.pop_last().is_some() {}
    });
    assert!(log.is_empty(), "removal allocated: {log:?}");
}

/// `count` keys in an order that splits nodes all over the tree rather than
/// only at the right edge.
fn scattered(count: u64) -> Vec<u64> {
    (0..count)
        .map(|n| n.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17))
        .collect()
}

#[test]
fn btree_nodes_fit_their_bound_and_their_count() {
    let count = if cfg!(miri) { 300 } else { 5_000 };
    let ascending: Vec<u64> = (0..count).collect();
    map_inserts_within_bounds(&ascending, 0u64);
    map_inserts_within_bounds(&scattered(count), 0u64);
    map_inserts_within_bounds(&scattered(count), [0u8; 40]);
    map_inserts_within_bounds(&scattered(count), 0u128);
    map_inserts_within_bounds(&scattered(count), ());
    let small: Vec<u16> = (0..u16::try_from(count).unwrap()).rev().collect();
    map_inserts_within_bounds(&small, 0u32);
}

#[test]
fn a_set_insert_is_a_map_insert() {
    let mut set = BTreeSet::new();
    let bound = btree_node_bound(8, 0, 8);
    for (index, key) in scattered(if cfg!(miri) { 200 } else { 2_000 })
        .into_iter()
        .enumerate()
    {
        let (_, log) = record(|| set.insert(key));
        assert!(log.len() <= btree_insert_nodes(max_height(index + 1)));
        assert!(log.iter().all(|(size, _)| *size <= bound));
    }
}

std::thread_local! {
    /// Whether [`test_section`] refuses, as a reserve that cannot be filled.
    static SECTION_REFUSES: Cell<bool> = const { Cell::new(false) };
    /// The node bound [`test_section`] was last asked about.
    static SECTION_NODE: Cell<usize> = const { Cell::new(0) };
    /// Whether [`test_section`] is running an operation now.
    static IN_SECTION: Cell<bool> = const { Cell::new(false) };
}

/// A host's section, as the kernel's: refuse before running anything, or run
/// the operation inside.
fn test_section(node: usize, run: &mut dyn FnMut()) -> Result<(), AllocError> {
    SECTION_NODE.with(|seen| seen.set(node));
    if SECTION_REFUSES.with(Cell::get) {
        return Err(AllocError);
    }
    IN_SECTION.with(|inside| inside.set(true));
    run();
    IN_SECTION.with(|inside| inside.set(false));
    Ok(())
}

#[test]
fn a_library_map_insert_runs_in_the_hosts_section_or_not_at_all() {
    let _ = set_section(test_section);
    assert!(
        !set_section(test_section),
        "only the first section is installed"
    );
    let mut map = BTreeMap::new();
    assert_eq!(try_map_insert(&mut map, 1u64, 2u32), Ok(None));
    assert_eq!(SECTION_NODE.with(Cell::get), map_node_bound::<u64, u32>());
    assert_eq!(in_section(8, || IN_SECTION.with(Cell::get)), Ok(true));

    SECTION_REFUSES.with(|refuses| refuses.set(true));
    assert_eq!(try_map_insert(&mut map, 3, 4), Err(AllocError));
    assert_eq!(try_map_entry(&mut map, 3, || 4).err(), Some(AllocError));
    let mut set = BTreeSet::new();
    assert_eq!(try_set_insert(&mut set, 1u64), Err(AllocError));
    assert!(set.is_empty());
    assert_eq!(map.len(), 1, "a refused insert leaves the map as it was");
    SECTION_REFUSES.with(|refuses| refuses.set(false));

    *try_map_entry(&mut map, 5, || 6).unwrap() += 1;
    assert_eq!(map.get(&5), Some(&7));
    assert_eq!(try_set_insert(&mut set, 1), Ok(true));

    // The injection policy is asked first, and the section never entered.
    fn policy() -> bool {
        INJECT.with(Cell::get)
    }
    let _ = set_injector(policy);
    arm(true);
    INJECT.with(|on| on.set(true));
    SECTION_NODE.with(|seen| seen.set(0));
    let injected = try_map_insert(&mut map, 9, 9);
    INJECT.with(|on| on.set(false));
    assert_eq!(injected, Err(AllocError));
    assert_eq!(
        SECTION_NODE.with(Cell::get),
        0,
        "the section was not entered"
    );
    assert_eq!(map.len(), 2);
}
