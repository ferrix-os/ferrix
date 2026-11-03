//! The class's rules, on the host (container review 29 (not on record; ledger 5684), R2).

extern crate std;

use std::vec::Vec;

use ferrix_sched_class::{LOAD_UNIT, RealtimeClass, Refusal, SchedClass};

use super::{FifoQueue, FifoRr, FifoSlot, PERIOD_NS, Params, Policy, RR_QUANTUM_NS, RUNTIME_NS};

fn queue() -> FifoQueue<u64> {
    <FifoRr as SchedClass<u64>>::new_queue(()).unwrap()
}

fn fifo(priority: u8) -> (Params, FifoSlot<u64>) {
    (
        Params {
            policy: Policy::Fifo,
            priority,
        },
        FifoSlot::new().unwrap(),
    )
}

fn rr(priority: u8) -> (Params, FifoSlot<u64>) {
    (
        Params {
            policy: Policy::Rr,
            priority,
        },
        FifoSlot::new().unwrap(),
    )
}

/// Pick and take off every entity in turn, in the order the class runs them.
fn drain(q: &mut FifoQueue<u64>) -> Vec<u64> {
    let mut order = Vec::new();
    while let Some(&id) = FifoRr::pick(q) {
        order.push(id);
        let _ = FifoRr::remove_current(q);
    }
    order
}

#[test]
fn higher_priorities_first_and_arrival_within_one() {
    let mut q = queue();
    for (id, priority) in [(1, 10), (2, 50), (3, 10), (4, 99), (5, 50)] {
        FifoRr::enqueue(&mut q, id, id, fifo(priority)).unwrap();
    }
    assert!(FifoRr::check(&q).is_ok());
    assert_eq!(drain(&mut q), [4, 2, 5, 1, 3]);
}

#[test]
fn a_bad_priority_or_a_duplicate_is_handed_back() {
    let mut q = queue();
    assert!(
        matches!(FifoRr::enqueue(&mut q, 1, 1, fifo(0)), Err(back) if back.reason == Refusal::Invalid)
    );
    assert!(
        matches!(FifoRr::enqueue(&mut q, 1, 1, fifo(100)), Err(back) if back.reason == Refusal::Invalid)
    );
    FifoRr::enqueue(&mut q, 1, 1, fifo(5)).unwrap();
    assert!(matches!(FifoRr::enqueue(&mut q, 1, 7, fifo(5)),
        Err(back) if back.reason == Refusal::Duplicate(1) && back.payload == 7));
    let _ = FifoRr::pick(&mut q);
    assert!(
        matches!(FifoRr::enqueue(&mut q, 1, 7, fifo(5)), Err(back) if back.reason == Refusal::Duplicate(1))
    );
}

#[test]
fn a_higher_arrival_preempts_and_the_preempted_keeps_its_head() {
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, fifo(10)).unwrap();
    FifoRr::enqueue(&mut q, 2, 2, fifo(10)).unwrap();
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    assert!(!FifoRr::should_preempt(&q));
    FifoRr::enqueue(&mut q, 3, 3, fifo(20)).unwrap();
    assert!(FifoRr::should_preempt(&q));
    assert!(FifoRr::charge(&mut q, 1_000));
    assert_eq!(FifoRr::pick(&mut q), Some(&3));
    let _ = FifoRr::remove_current(&mut q);
    // 1 was preempted, not finished: it runs before 2, which arrived later.
    assert_eq!(drain(&mut q), [1, 2]);
}

#[test]
fn put_back_keeps_the_head_of_the_priority() {
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, fifo(10)).unwrap();
    FifoRr::enqueue(&mut q, 2, 2, fifo(10)).unwrap();
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    FifoRr::put_back(&mut q);
    assert_eq!(FifoRr::current(&q), None);
    assert_eq!(drain(&mut q), [1, 2]);
}

#[test]
fn rr_rotates_at_its_quantum_and_fifo_does_not() {
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, rr(30)).unwrap();
    FifoRr::enqueue(&mut q, 2, 2, rr(30)).unwrap();
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    assert_eq!(FifoRr::decision_in_ns(&q), Some(RR_QUANTUM_NS));
    assert!(!FifoRr::charge(&mut q, RR_QUANTUM_NS / 2));
    assert!(!FifoRr::should_preempt(&q));
    assert!(FifoRr::charge(&mut q, RR_QUANTUM_NS / 2));
    assert!(FifoRr::should_preempt(&q));
    assert_eq!(FifoRr::pick(&mut q), Some(&2));
    assert!(FifoRr::check(&q).is_ok());
    // Alone at its priority, an RR entity keeps running past its quantum.
    let mut alone = queue();
    FifoRr::enqueue(&mut alone, 1, 1, rr(30)).unwrap();
    let _ = FifoRr::pick(&mut alone);
    assert!(!FifoRr::charge(&mut alone, 2 * RR_QUANTUM_NS));
    // FIFO keeps the processor however long it runs.
    let mut f = queue();
    FifoRr::enqueue(&mut f, 1, 1, fifo(30)).unwrap();
    FifoRr::enqueue(&mut f, 2, 2, fifo(30)).unwrap();
    let _ = FifoRr::pick(&mut f);
    assert!(!FifoRr::charge(&mut f, 3 * RR_QUANTUM_NS));
    assert_eq!(FifoRr::pick(&mut f), Some(&1));
}

/// Ledger 5684's B1: a decision before the quantum's end restarts nothing.
/// Two `SCHED_RR` tasks of one priority, 60 ms then a decision, twelve times:
/// with a quantum restarted at each pick, the peer never ran in 720 ms of a
/// 100 ms quantum.
#[test]
fn an_early_decision_does_not_restart_the_quantum() {
    const STEP_NS: u64 = 60_000_000;
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, rr(30)).unwrap();
    FifoRr::enqueue(&mut q, 2, 2, rr(30)).unwrap();
    let mut ran = Vec::new();
    ran.push(*FifoRr::pick(&mut q).unwrap());
    for _ in 0..12 {
        let _ = FifoRr::charge(&mut q, STEP_NS);
        ran.push(*FifoRr::pick(&mut q).unwrap());
    }
    // Two steps of 60 ms each spend a quantum, then the other's turn.
    assert_eq!(ran, [1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2, 1]);
    assert!(FifoRr::check(&q).is_ok());
}

/// sched(7): a preempted thread "will complete the unexpired portion", and
/// the head of its priority is still its own.
#[test]
fn a_preempted_rr_entity_completes_its_quantum() {
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, rr(30)).unwrap();
    FifoRr::enqueue(&mut q, 2, 2, rr(30)).unwrap();
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    assert!(!FifoRr::charge(&mut q, 60_000_000));
    FifoRr::enqueue(&mut q, 3, 3, rr(50)).unwrap();
    assert!(FifoRr::should_preempt(&q));
    assert_eq!(FifoRr::pick(&mut q), Some(&3));
    let _ = FifoRr::remove_current(&mut q);
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    assert_eq!(FifoRr::remaining_ns(&q), Some(40_000_000));
    assert!(FifoRr::charge(&mut q, 40_000_000));
    assert_eq!(FifoRr::pick(&mut q), Some(&2));
    // Put back by a tier above, the same.
    let mut b = queue();
    FifoRr::enqueue(&mut b, 1, 1, rr(30)).unwrap();
    let _ = FifoRr::pick(&mut b);
    let _ = FifoRr::charge(&mut b, 60_000_000);
    FifoRr::put_back(&mut b);
    let _ = FifoRr::pick(&mut b);
    assert_eq!(FifoRr::remaining_ns(&b), Some(40_000_000));
}

/// `yield_task_rt` requeues and leaves `time_slice` alone; so does a block,
/// and the node carries it.
#[test]
fn a_yield_and_a_block_keep_what_is_left_of_the_quantum() {
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, rr(30)).unwrap();
    FifoRr::enqueue(&mut q, 2, 2, rr(30)).unwrap();
    let _ = FifoRr::pick(&mut q);
    let _ = FifoRr::charge(&mut q, 60_000_000);
    FifoRr::yield_current(&mut q);
    assert_eq!(FifoRr::current(&q), Some(&2));
    assert_eq!(FifoRr::remaining_ns(&q), Some(RR_QUANTUM_NS));
    assert!(FifoRr::charge(&mut q, RR_QUANTUM_NS));
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    assert_eq!(FifoRr::remaining_ns(&q), Some(40_000_000));
    let (id, payload, entity) = FifoRr::remove_current(&mut q).unwrap();
    FifoRr::enqueue(&mut q, id, payload, entity).unwrap();
    // 2 has the tail's turn first, then 1 with its 40 ms.
    assert_eq!(FifoRr::pick(&mut q), Some(&2));
    let _ = FifoRr::remove_current(&mut q);
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    assert_eq!(FifoRr::remaining_ns(&q), Some(40_000_000));
}

/// `task_tick_rt` refills at the quantum's end alone or not, so a peer
/// arriving after that waits out what is left of the new quantum.
#[test]
fn a_quantum_spent_alone_is_refilled() {
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, rr(30)).unwrap();
    let _ = FifoRr::pick(&mut q);
    assert!(!FifoRr::charge(&mut q, RR_QUANTUM_NS));
    assert_eq!(FifoRr::remaining_ns(&q), Some(RR_QUANTUM_NS));
    FifoRr::enqueue(&mut q, 2, 2, rr(30)).unwrap();
    assert!(!FifoRr::should_preempt(&q));
    assert!(!FifoRr::charge(&mut q, 60_000_000));
    assert!(FifoRr::charge(&mut q, 40_000_000));
    assert_eq!(FifoRr::pick(&mut q), Some(&2));
}

/// The floor's clock is real time: a budget spent is refilled by the time
/// passing while nothing ran (`CpuQueue::account_with`'s `elapse`), not left
/// to wait for the class's next charges (ledger 5684's C1).
#[test]
fn time_passing_with_nothing_running_refills_the_budget() {
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, fifo(30)).unwrap();
    let _ = FifoRr::pick(&mut q);
    assert!(FifoRr::charge(&mut q, RUNTIME_NS));
    let (id, payload, entity) = FifoRr::remove_current(&mut q).unwrap();
    FifoRr::elapse(&mut q, PERIOD_NS);
    FifoRr::enqueue(&mut q, id, payload, entity).unwrap();
    assert!(FifoRr::ready(&q, true));
    let _ = FifoRr::pick(&mut q);
    assert_eq!(FifoRr::remaining_ns(&q), Some(RUNTIME_NS));
}

#[test]
fn a_yield_goes_to_the_tail_of_its_priority() {
    let mut q = queue();
    for id in 1..=3 {
        FifoRr::enqueue(&mut q, id, id, fifo(40)).unwrap();
    }
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    FifoRr::yield_current(&mut q);
    assert_eq!(FifoRr::current(&q), Some(&2));
    assert_eq!(FifoRr::pick(&mut q), Some(&2));
    let _ = FifoRr::remove_current(&mut q);
    assert_eq!(drain(&mut q), [3, 1]);
}

#[test]
fn the_running_and_a_waiting_entity_are_removed() {
    let mut q = queue();
    for id in 1..=4 {
        FifoRr::enqueue(&mut q, id, id, fifo(60)).unwrap();
    }
    assert_eq!(FifoRr::pick(&mut q), Some(&1));
    let (payload, (params, _)) = FifoRr::remove(&mut q, 1).unwrap();
    assert_eq!((payload, params.priority), (1, 60));
    assert_eq!(FifoRr::current(&q), None);
    assert_eq!(
        FifoRr::remove(&mut q, 3).map(|(payload, _)| payload),
        Some(3)
    );
    assert!(FifoRr::remove(&mut q, 3).is_none());
    assert_eq!(FifoRr::len(&q), 2);
    assert_eq!(FifoRr::load_weight(&q), 2 * LOAD_UNIT);
    assert_eq!(drain(&mut q), [2, 4]);
}

#[test]
fn the_tree_stays_ordered_and_balanced() {
    let mut q = queue();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut queued = Vec::new();
    for round in 0..600_u64 {
        let roll = next();
        if roll % 3 == 0 && !queued.is_empty() {
            let at = (roll / 3) as usize % queued.len();
            let id = queued.swap_remove(at);
            assert!(FifoRr::remove(&mut q, id).is_some());
        } else {
            let priority = (roll % 99) as u8 + 1;
            FifoRr::enqueue(&mut q, round, round, fifo(priority)).unwrap();
            queued.push(round);
        }
        if roll % 11 == 0 {
            let _ = FifoRr::pick(&mut q);
        }
        assert!(FifoRr::check(&q).is_ok());
        assert_eq!(FifoRr::len(&q), queued.len());
    }
}

#[test]
fn the_floor_throttles_and_the_period_refills() {
    let mut q = queue();
    FifoRr::enqueue(&mut q, 1, 1, fifo(99)).unwrap();
    let _ = FifoRr::pick(&mut q);
    assert!(FifoRr::ready(&q, true));
    assert!(!FifoRr::charge(&mut q, RUNTIME_NS - 1));
    assert!(FifoRr::charge(&mut q, 1), "a spent budget is no decision");
    assert!(
        !FifoRr::ready(&q, true),
        "throttled, with something below waiting"
    );
    assert!(
        FifoRr::ready(&q, false),
        "the floor bound with nothing below"
    );
    assert_eq!(FifoRr::decision_in_ns(&q), Some(PERIOD_NS - RUNTIME_NS));
    // The lower tier runs the rest of the period; then the budget is back.
    FifoRr::elapse(&mut q, PERIOD_NS - RUNTIME_NS - 1);
    assert!(!FifoRr::ready(&q, true));
    FifoRr::elapse(&mut q, 1);
    assert!(FifoRr::ready(&q, true));
    assert!(FifoRr::check(&q).is_ok());
}

#[test]
fn a_waiting_entity_is_found_by_its_payload_and_the_running_one_never() {
    let mut q = queue();
    for (id, priority) in [(1, 10), (2, 50), (3, 10), (4, 99)] {
        FifoRr::enqueue(&mut q, id, id * 10, fifo(priority)).unwrap();
    }
    assert_eq!(FifoRr::pick(&mut q), Some(&40));
    // The running entity's payload is 40: never answered.
    assert_eq!(FifoRr::waiting_where(&q, |payload| *payload == 40), None);
    for id in [1, 2, 3] {
        assert_eq!(
            FifoRr::waiting_where(&q, |payload| *payload == id * 10),
            Some(id),
            "waiting entity {id}"
        );
    }
    assert_eq!(FifoRr::waiting_where(&q, |payload| *payload > 100), None);
    let found = FifoRr::waiting_where(&q, |payload| *payload % 20 == 10);
    assert!(matches!(found, Some(1 | 3)), "one of the two that match");
    assert!(FifoRr::check(&q).is_ok(), "and the walk changed nothing");
}
