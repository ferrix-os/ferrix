//! The rules the kernel's objects rely on, checked without a kernel.

extern crate std;

use alloc::vec;
use alloc::vec::Vec;

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::{Requested, Rights};

use crate::message::{Limits, Message, MessageQueue, ReceiveError, SendError};
use crate::reach::{Reach, reaches};
use crate::table::{HandleTable, MAX_SLOTS, TableError};

/// Enough closes of one slot to use up every generation it has.
const GENERATIONS: usize = 4095;

/// A table with room for `limit`, holding numbered objects.
fn table(limit: usize) -> HandleTable<u32> {
    HandleTable::new(limit)
}

#[test]
fn handles_are_non_zero_and_distinct() {
    let mut t = table(16);
    let handles: Vec<Handle> = (0..16).map(|i| t.insert(i, Rights::ALL).unwrap()).collect();
    for (i, a) in handles.iter().enumerate() {
        assert!(a.is_valid(), "handle {i} is zero");
        assert!(
            !handles.iter().skip(i + 1).any(|b| a == b),
            "{a:?} issued twice"
        );
        assert_eq!(t.get(*a), Ok((&(i as u32), Rights::ALL)), "handle {i}");
    }
    assert_eq!(t.get(Handle::INVALID), Err(TableError::BadHandle), "zero");
}

/// Verifies: H.OBJ.4
#[test]
fn a_closed_handle_stays_stale_when_its_slot_is_reused() {
    let mut t = table(1);
    let first = t.insert(1, Rights::ALL).unwrap();
    assert_eq!(t.remove(first), Ok((1, Rights::ALL)), "close");
    let second = t.insert(2, Rights::ALL).unwrap();
    assert_ne!(first, second, "the reused slot must issue a new value");
    assert_eq!(t.get(first), Err(TableError::BadHandle), "the old value");
    assert_eq!(t.remove(first), Err(TableError::BadHandle), "double close");
    assert_eq!(t.get(second), Ok((&2, Rights::ALL)), "the new one works");
}

/// Verifies: H.OBJ.15
#[test]
fn a_slot_whose_generations_run_out_is_retired_rather_than_wrapped() {
    let mut t = table(1);
    let mut issued = Vec::with_capacity(GENERATIONS + 1);
    for i in 0..GENERATIONS {
        let handle = t.insert(i as u32, Rights::NONE).unwrap();
        issued.push(handle);
        let _ = t.remove(handle).unwrap();
    }
    // Every generation of slot 0 has been used. The next handle is elsewhere.
    let next = t.insert(0, Rights::NONE).unwrap();
    assert_eq!(next.0 >> 12, 1, "slot 0 should be retired: {next:?}");
    for old in issued {
        assert_ne!(old, next, "{old:?} was reissued");
        assert_eq!(t.get(old), Err(TableError::BadHandle), "{old:?} resolves");
    }
}

#[test]
fn the_limit_is_enforced_and_the_object_comes_back() {
    let mut t = table(2);
    let _ = t.insert(1, Rights::ALL).unwrap();
    let _ = t.insert(2, Rights::ALL).unwrap();
    assert_eq!(t.insert(3, Rights::ALL), Err(3), "the object back");
    assert_eq!(t.len(), 2, "unchanged");
    assert_eq!(t.room(), 0, "full");
}

#[test]
fn rights_are_checked_on_use() {
    let mut t = table(4);
    let h = t.insert(7, Rights::READ).unwrap();
    assert_eq!(t.get_with(h, Rights::READ), Ok(&7), "held");
    assert_eq!(
        t.get_with(h, Rights::READ | Rights::WRITE),
        Err(TableError::AccessDenied),
        "WRITE is not held"
    );
}

/// Verifies: H.OBJ.3
#[test]
fn duplicate_needs_the_right_and_cannot_add_rights() {
    let mut t = table(4);
    let plain = t.insert(1, Rights::READ).unwrap();
    assert_eq!(
        t.duplicate(plain, Requested::Same),
        Err(TableError::AccessDenied),
        "no DUPLICATE"
    );

    let h = t.insert(2, Rights::DUPLICATE | Rights::READ).unwrap();
    let copy = t.duplicate(h, Requested::Exactly(Rights::READ)).unwrap();
    assert_eq!(t.get(copy), Ok((&2, Rights::READ)), "a narrower copy");
    assert_eq!(
        t.duplicate(h, Requested::Exactly(Rights::WRITE)),
        Err(TableError::AccessDenied),
        "cannot add WRITE"
    );
    assert_eq!(
        t.get(h),
        Ok((&2, Rights::DUPLICATE | Rights::READ)),
        "original"
    );
}

/// Verifies: H.OBJ.14
#[test]
fn replace_closes_the_original_and_cannot_add_rights() {
    let mut t = table(4);
    let h = t.insert(5, Rights::READ | Rights::WRITE).unwrap();
    assert_eq!(
        t.replace(h, Requested::Exactly(Rights::MAP)),
        Err(TableError::AccessDenied),
        "cannot add MAP"
    );
    assert_eq!(
        t.get(h),
        Ok((&5, Rights::READ | Rights::WRITE)),
        "unchanged"
    );

    let narrower = t.replace(h, Requested::Exactly(Rights::READ)).unwrap();
    assert_eq!(
        t.get(h),
        Err(TableError::BadHandle),
        "the original is closed"
    );
    assert_eq!(t.get(narrower), Ok((&5, Rights::READ)), "the replacement");
    assert_eq!(t.len(), 1, "still one handle");
}

#[test]
fn replace_on_a_used_up_slot_moves_to_another() {
    let mut t = table(1);
    for i in 0..GENERATIONS - 1 {
        let handle = t.insert(i as u32, Rights::NONE).unwrap();
        let _ = t.remove(handle).unwrap();
    }
    let last = t.insert(9, Rights::READ).unwrap();
    assert_eq!(last.0 & 0xFFF, 0xFFF, "at the final generation: {last:?}");
    let moved = t.replace(last, Requested::Same).unwrap();
    assert_eq!(moved.0 >> 12, 1, "moved to slot 1: {moved:?}");
    assert_eq!(t.get(last), Err(TableError::BadHandle), "the original");
    assert_eq!(t.get(moved), Ok((&9, Rights::READ)), "the replacement");
}

#[test]
fn take_many_is_all_or_nothing() {
    let mut t = table(8);
    let a = t.insert(1, Rights::TRANSFER).unwrap();
    let b = t.insert(2, Rights::TRANSFER).unwrap();
    let locked = t.insert(3, Rights::READ).unwrap();
    let before: Vec<_> = t.handles().collect();

    assert_eq!(
        t.take_many(&[a, Handle(0xDEAD_B001), b], Rights::TRANSFER),
        Err(TableError::BadHandle),
        "a bad handle in the middle"
    );
    assert_eq!(
        t.take_many(&[a, locked], Rights::TRANSFER),
        Err(TableError::AccessDenied),
        "a handle without TRANSFER"
    );
    assert_eq!(
        t.take_many(&[a, b, a], Rights::TRANSFER),
        Err(TableError::Repeated),
        "the same handle twice"
    );
    assert_eq!(t.handles().collect::<Vec<_>>(), before, "nothing moved");

    let taken = t.take_many(&[b, a], Rights::TRANSFER).unwrap();
    assert_eq!(
        taken,
        vec![(2, Rights::TRANSFER), (1, Rights::TRANSFER)],
        "in the order listed"
    );
    assert_eq!(t.len(), 1, "only the locked one is left");
}

#[test]
fn insert_many_is_all_or_nothing() {
    let mut t = table(3);
    let _ = t.insert(0, Rights::ALL).unwrap();
    let three = vec![(1, Rights::READ), (2, Rights::READ), (3, Rights::READ)];
    assert_eq!(
        t.insert_many(three.clone()),
        Err((TableError::Full, three)),
        "no room for three"
    );
    assert_eq!(t.len(), 1, "nothing inserted");

    let handles = t
        .insert_many(vec![(1, Rights::READ), (2, Rights::WRITE)])
        .unwrap();
    assert_eq!(t.get(handles[0]), Ok((&1, Rights::READ)), "first");
    assert_eq!(t.get(handles[1]), Ok((&2, Rights::WRITE)), "second");
}

#[test]
fn clear_gives_everything_back_and_leaves_every_value_stale() {
    let mut t = table(4);
    let handles: Vec<Handle> = (0..3).map(|i| t.insert(i, Rights::ALL).unwrap()).collect();
    let mut objects = t.clear();
    objects.sort_unstable();
    assert_eq!(objects, vec![0, 1, 2], "every object");
    assert!(t.is_empty(), "empty");
    for handle in handles {
        assert_eq!(t.get(handle), Err(TableError::BadHandle), "{handle:?}");
    }
    assert!(t.insert(9, Rights::ALL).is_ok(), "still usable");
}

/// Small limits, so the bounds are reachable in a test.
const LIMITS: Limits = Limits {
    max_bytes: 8,
    max_handles: 2,
    max_queued: 2,
};

/// A message of `bytes` bytes carrying `handles` numbered objects.
fn message(bytes: usize, handles: u32) -> Message<u32> {
    Message {
        bytes: vec![0xA5; bytes],
        handles: (0..handles).collect(),
        sender: (),
    }
}

#[test]
fn messages_come_out_in_order() {
    let mut q = MessageQueue::new(LIMITS);
    q.push(message(1, 0)).unwrap();
    q.push(message(2, 1)).unwrap();
    assert_eq!(q.pop_fitting(8, 2).unwrap().bytes.len(), 1, "first");
    assert_eq!(q.pop_fitting(8, 2).unwrap().bytes.len(), 2, "second");
    assert_eq!(q.pop_fitting(8, 2), Err(ReceiveError::Empty), "empty");
}

#[test]
fn a_refused_message_comes_back_whole() {
    let mut q = MessageQueue::new(LIMITS);
    let big = message(9, 0);
    assert_eq!(q.push(big.clone()), Err((SendError::TooBig, big)), "bytes");
    let many = message(0, 3);
    assert_eq!(
        q.push(many.clone()),
        Err((SendError::TooBig, many)),
        "handles"
    );

    q.push(message(0, 0)).unwrap();
    q.push(message(0, 0)).unwrap();
    assert!(q.is_full(), "full at two");
    let third = message(1, 1);
    assert_eq!(q.push(third.clone()), Err((SendError::Full, third)), "full");
}

#[test]
fn a_read_that_does_not_fit_takes_nothing() {
    let mut q = MessageQueue::new(LIMITS);
    q.push(message(6, 2)).unwrap();
    assert_eq!(
        q.pop_fitting(5, 2),
        Err(ReceiveError::TooSmall {
            bytes: 6,
            handles: 2
        }),
        "too few bytes"
    );
    assert_eq!(
        q.pop_fitting(6, 1),
        Err(ReceiveError::TooSmall {
            bytes: 6,
            handles: 2
        }),
        "too few handles"
    );
    assert_eq!(q.len(), 1, "still queued");
    assert_eq!(q.pop_fitting(6, 2), Ok(message(6, 2)), "exactly enough");
}

#[test]
fn unpop_restores_the_head() {
    let mut q = MessageQueue::new(LIMITS);
    q.push(message(1, 0)).unwrap();
    q.push(message(2, 0)).unwrap();
    let head = q.pop_fitting(8, 2).unwrap();
    q.unpop(head).unwrap();
    assert_eq!(q.peek_sizes(), Some((1, 0)), "the first is first again");
    assert_eq!(q.drain().len(), 2, "both");
    assert!(q.is_empty(), "drained");
}

#[test]
fn a_sender_travels_with_its_message_unchanged() {
    // K6 (`docs/microkernel/K6.md`): the queue carries each message's
    // stamp through push, a refused push, pop, put-back and drain.
    let stamped = |bytes: u8, sender: (u64, u64)| Message {
        bytes: vec![bytes],
        handles: Vec::<u32>::new(),
        sender,
    };
    let mut q = MessageQueue::new(LIMITS);
    q.push(stamped(1, (7, 3))).unwrap();
    q.push(stamped(2, (8, 4))).unwrap();
    let refused = stamped(3, (9, 5));
    assert_eq!(q.push(refused.clone()), Err((SendError::Full, refused)));
    let head = q.pop_fitting(8, 2).unwrap();
    assert_eq!(head.sender, (7, 3), "popped with its own");
    q.unpop(head).unwrap();
    let senders: Vec<(u64, u64)> = q.drain().into_iter().map(|m| m.sender).collect();
    assert_eq!(
        senders,
        vec![(7, 3), (8, 4)],
        "put back and drained with theirs"
    );
}

#[test]
fn every_handle_value_is_positive_in_a_32_bit_return_register() {
    let largest = ((MAX_SLOTS - 1) << 12) | 0xFFF;
    assert!(
        largest < 1 << 31,
        "{largest:#x} reads as negative in a 32-bit register"
    );
}

/// Walk a graph given as adjacency lists.
fn walk(graph: &[&[usize]], start: &[usize], target: usize, limit: usize) -> Reach {
    reaches(
        start.to_vec(),
        |&node| node,
        target,
        |&node| Some(graph.get(node).map_or_else(Vec::new, |next| next.to_vec())),
        limit,
    )
}

#[test]
fn a_walk_whose_children_cannot_be_listed_stops_without_an_answer() {
    let answer = reaches(vec![0usize], |&node| node, 9, |_| None, 8);
    assert_eq!(answer, Reach::NoMemory);
}

#[test]
fn a_target_among_the_starting_nodes_is_found() {
    assert_eq!(walk(&[&[], &[]], &[0, 1], 1, 8), Reach::Found, "one step");
}

#[test]
fn a_target_several_edges_away_is_found() {
    let graph: &[&[usize]] = &[&[1], &[2], &[3], &[]];
    assert_eq!(walk(graph, &[0], 3, 8), Reach::Found, "0 -> 1 -> 2 -> 3");
}

#[test]
fn an_unreachable_target_is_clear() {
    let graph: &[&[usize]] = &[&[1], &[], &[0]];
    assert_eq!(
        walk(graph, &[0], 2, 8),
        Reach::Clear,
        "2 points at 0, not back"
    );
    assert_eq!(
        walk(graph, &[], 0, 8),
        Reach::Clear,
        "nothing to start from"
    );
}

#[test]
fn a_loop_without_the_target_ends() {
    let graph: &[&[usize]] = &[&[1], &[0], &[]];
    assert_eq!(
        walk(graph, &[0], 2, 8),
        Reach::Clear,
        "0 and 1 point at each other"
    );
}

#[test]
fn each_node_counts_once_against_the_limit() {
    // A diamond: 3 is reached along two paths, and there are four nodes.
    let graph: &[&[usize]] = &[&[1, 2], &[3], &[3], &[]];
    assert_eq!(walk(graph, &[0], 9, 4), Reach::Clear, "exactly the limit");
    assert_eq!(walk(graph, &[0], 9, 3), Reach::TooFar, "one past the limit");
}

#[test]
fn a_node_reached_twice_is_expanded_once() {
    let graph: &[&[usize]] = &[&[1, 2], &[3], &[3], &[]];
    let mut expanded = Vec::new();
    let answer = reaches(
        vec![0],
        |&node| node,
        9,
        |&node| {
            expanded.push(node);
            Some(graph.get(node).map_or_else(Vec::new, |next| next.to_vec()))
        },
        8,
    );
    expanded.sort_unstable();
    assert_eq!(answer, Reach::Clear, "the target is not in the graph");
    assert_eq!(expanded, vec![0, 1, 2, 3], "3 is expanded once");
}

#[test]
fn a_closed_table_refuses_every_insertion() {
    let mut t = table(8);
    let _ = t.insert(1, Rights::ALL).unwrap();
    let _ = t.insert(2, Rights::DUPLICATE).unwrap();
    let mut objects: Vec<_> = t.close().collect();
    objects.sort_unstable();
    assert_eq!(objects, vec![1, 2], "every object back");
    assert!(t.is_closed(), "closed");
    assert_eq!(t.room(), 0, "no room");
    assert_eq!(
        t.insert(3, Rights::ALL),
        Err(3),
        "insert refused, object back"
    );
    assert_eq!(
        t.insert_many(vec![(4, Rights::ALL)]),
        Err((TableError::Full, vec![(4, Rights::ALL)])),
        "a batch refused whole"
    );
    assert!(t.is_empty(), "still empty");
}

#[test]
fn duplicate_children_are_admitted_once() {
    let mut expanded = 0;
    let answer = reaches(
        vec![0_usize],
        |&node| node,
        99,
        |&node| {
            expanded += 1;
            Some(if node == 0 {
                vec![1; 10_000]
            } else {
                Vec::new()
            })
        },
        2,
    );
    assert_eq!(
        answer,
        Reach::Clear,
        "two distinct nodes fit a limit of two"
    );
    assert_eq!(
        expanded, 2,
        "a node offered ten thousand times is expanded once"
    );
}

#[test]
fn a_reserved_table_takes_that_many_without_growing() {
    let mut t = table(64);
    t.reserve(10).unwrap();
    let (_, slots, free) = t.capacities();
    let handles: Vec<Handle> = (0..10).map(|n| t.insert(n, Rights::ALL).unwrap()).collect();
    assert_eq!(t.capacities(), (10, slots, free), "nothing grew");
    for handle in handles {
        let _ = t.remove(handle).unwrap();
    }
    let (len, _, free_after) = t.capacities();
    assert_eq!(free_after, free, "closing every handle grew nothing either");
    assert!(free_after >= len, "every slot fits on the free list");
}

#[test]
fn a_close_hands_back_every_object_in_slot_order_and_keeps_nothing() {
    let mut t = table(8);
    let first = t.insert(1, Rights::ALL).unwrap();
    let _ = t.insert(2, Rights::ALL).unwrap();
    let _ = t.insert(3, Rights::ALL).unwrap();
    let _ = t.remove(first).unwrap();
    assert_eq!(t.close().collect::<Vec<_>>(), vec![2, 3]);
    assert!(t.is_empty());
    assert_eq!(t.get(first), Err(TableError::BadHandle));
}

#[test]
fn a_drained_queue_keeps_its_order() {
    let mut q = MessageQueue::new(LIMITS);
    for n in 0..2u8 {
        q.push(Message {
            bytes: vec![n],
            handles: Vec::<u32>::new(),
            sender: (),
        })
        .unwrap();
    }
    let drained: Vec<u8> = q.drain().into_iter().map(|m| m.bytes[0]).collect();
    assert_eq!(drained, vec![0, 1]);
    assert!(q.is_empty());
}

/// Allocation failure, injected through `ferrix_fallible`: a queue refuses
/// room with `NoMemory` when it cannot grow, and once it has room, a push is
/// not refused however the allocator answers -- the write asks before it
/// takes the sender's handles, and the push after cannot lose them.
#[test]
fn room_made_first_cannot_be_refused_for_memory() {
    std::thread_local! {
        static FAIL: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
    fn policy() -> bool {
        FAIL.with(core::cell::Cell::get)
    }
    let _ = ferrix_fallible::set_injector(policy);
    ferrix_fallible::arm(true);

    // A new queue has no room, so its first message needs an allocation.
    let mut q: MessageQueue<u32> = MessageQueue::new(LIMITS);
    FAIL.with(|fail| fail.set(true));
    assert_eq!(q.reserve(), Err(SendError::NoMemory), "no room to be had");
    assert!(q.is_empty(), "the refusal queued nothing");

    FAIL.with(|fail| fail.set(false));
    q.reserve().unwrap();
    FAIL.with(|fail| fail.set(true));
    assert_eq!(q.push(message(1, 2)), Ok(()), "the room made is used");
    FAIL.with(|fail| fail.set(false));
    assert_eq!(q.pop_fitting(8, 2), Ok(message(1, 2)), "handles and all");
}
