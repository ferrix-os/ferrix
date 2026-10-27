//! The stack's and the empty class's rules, over a toy class: a first-in,
//! first-out list that charges a fixed quantum.

extern crate std;

use std::collections::VecDeque;
use std::vec::Vec;

use super::{Back, Empty, Handed, Left, Over, Refusal, SchedClass, Tier};

/// A toy class: runs in arrival order, a quantum of `Config` nanoseconds.
struct Fifo;

#[derive(Default)]
struct FifoQueue {
    waiting: VecDeque<(u64, &'static str)>,
    running: Option<(u64, &'static str, u64)>,
    quantum: u64,
}

impl SchedClass<&'static str> for Fifo {
    const NAME: &'static str = "toy-fifo";
    type Queue = FifoQueue;
    type Entity = ();
    type Config = u64;

    fn new_queue(quantum: u64) -> Result<FifoQueue, Refusal> {
        if quantum == 0 {
            return Err(Refusal::Invalid);
        }
        Ok(FifoQueue {
            quantum,
            ..FifoQueue::default()
        })
    }
    fn len(q: &FifoQueue) -> usize {
        q.waiting.len() + usize::from(q.running.is_some())
    }
    fn enqueue(
        q: &mut FifoQueue,
        id: u64,
        payload: &'static str,
        (): (),
    ) -> Result<(), Back<&'static str, ()>> {
        let taken = q.waiting.iter().any(|&(other, _)| other == id)
            || q.running.is_some_and(|(other, ..)| other == id);
        if taken {
            return Err(Back {
                payload,
                entity: (),
                reason: Refusal::Duplicate(id),
            });
        }
        q.waiting.push_back((id, payload));
        Ok(())
    }
    fn remove(q: &mut FifoQueue, id: u64) -> Option<(&'static str, ())> {
        if q.running.is_some_and(|(other, ..)| other == id) {
            return q.running.take().map(|(_, payload, _)| (payload, ()));
        }
        let at = q.waiting.iter().position(|&(other, _)| other == id)?;
        q.waiting.remove(at).map(|(_, payload)| (payload, ()))
    }
    fn remove_current(q: &mut FifoQueue) -> Option<Left<&'static str, ()>> {
        q.running.take().map(|(id, payload, _)| (id, payload, ()))
    }
    fn pick(q: &mut FifoQueue) -> Option<&&'static str> {
        if q.running.is_none() {
            q.running = q.waiting.pop_front().map(|(id, payload)| (id, payload, 0));
        }
        q.running.as_ref().map(|(_, payload, _)| payload)
    }
    fn put_back(q: &mut FifoQueue) {
        if let Some((id, payload, _)) = q.running.take() {
            q.waiting.push_front((id, payload));
        }
    }
    fn current(q: &FifoQueue) -> Option<&&'static str> {
        q.running.as_ref().map(|(_, payload, _)| payload)
    }
    fn hand_over(
        q: &mut FifoQueue,
        id: u64,
        payload: &'static str,
        (): (),
        _: u64,
    ) -> Handed<&'static str, ()> {
        Self::enqueue(q, id, payload, ())?;
        let left = Self::remove_current(q);
        let _ = Self::pick(q);
        Ok(left)
    }
    fn charge(q: &mut FifoQueue, ran_ns: u64) -> bool {
        let quantum = q.quantum;
        q.running.as_mut().is_some_and(|(_, _, used)| {
            *used += ran_ns;
            *used >= quantum
        })
    }
    fn remaining_ns(q: &FifoQueue) -> Option<u64> {
        q.running.map(|(.., used)| q.quantum.saturating_sub(used))
    }
    fn steals_work(_: &FifoQueue) -> bool {
        false
    }
    fn check(q: &FifoQueue) -> Result<(), &'static str> {
        if q.quantum == 0 {
            Err("a zero quantum")
        } else {
            Ok(())
        }
    }
}

/// A second toy class that does steal work, to tell the tiers apart.
struct Lowly;

impl SchedClass<&'static str> for Lowly {
    const NAME: &'static str = "toy-lowly";
    type Queue = FifoQueue;
    type Entity = ();
    type Config = u64;
    fn new_queue(quantum: u64) -> Result<FifoQueue, Refusal> {
        Fifo::new_queue(quantum)
    }
    fn len(q: &FifoQueue) -> usize {
        Fifo::len(q)
    }
    fn enqueue(
        q: &mut FifoQueue,
        id: u64,
        p: &'static str,
        e: (),
    ) -> Result<(), Back<&'static str, ()>> {
        Fifo::enqueue(q, id, p, e)
    }
    fn remove(q: &mut FifoQueue, id: u64) -> Option<(&'static str, ())> {
        Fifo::remove(q, id)
    }
    fn remove_current(q: &mut FifoQueue) -> Option<Left<&'static str, ()>> {
        Fifo::remove_current(q)
    }
    fn pick(q: &mut FifoQueue) -> Option<&&'static str> {
        Fifo::pick(q)
    }
    fn put_back(q: &mut FifoQueue) {
        Fifo::put_back(q);
    }
    fn current(q: &FifoQueue) -> Option<&&'static str> {
        Fifo::current(q)
    }
    fn hand_over(
        q: &mut FifoQueue,
        id: u64,
        p: &'static str,
        e: (),
        s: u64,
    ) -> Handed<&'static str, ()> {
        Fifo::hand_over(q, id, p, e, s)
    }
    fn charge(q: &mut FifoQueue, ran: u64) -> bool {
        Fifo::charge(q, ran)
    }
    fn remaining_ns(q: &FifoQueue) -> Option<u64> {
        Fifo::remaining_ns(q)
    }
    fn steals_work(_: &FifoQueue) -> bool {
        true
    }
    fn check(q: &FifoQueue) -> Result<(), &'static str> {
        Fifo::check(q)
    }
}

type Stack = Over<Fifo, Lowly>;
type Up = Tier<(), ()>;

fn stack() -> <Stack as SchedClass<&'static str>>::Queue {
    Stack::new_queue((10, 20)).unwrap()
}

#[test]
fn the_upper_class_runs_first_and_the_lower_one_waits_its_turn() {
    let mut q = stack();
    Stack::enqueue(&mut q, 1, "low", Up::Lower(())).unwrap();
    assert_eq!(Stack::pick(&mut q), Some(&"low"));
    assert!(
        Stack::steals_work(&q),
        "a stack holding only its lower class answers as the lower class"
    );
    Stack::enqueue(&mut q, 2, "high", Up::Upper(())).unwrap();
    // Something of the upper class waiting makes a decision due.
    assert!(Stack::charge(&mut q, 1));
    assert_eq!(Stack::pick(&mut q), Some(&"high"));
    assert!(!Stack::steals_work(&q));
    Stack::check(&q).unwrap();
    // The lower class's entity went back, not away.
    assert_eq!(Stack::len(&q), 2);
    let (id, payload, tier) = Stack::remove_current(&mut q).unwrap();
    assert_eq!((id, payload), (2, "high"));
    assert!(matches!(tier, Tier::Upper(())));
    assert_eq!(Stack::pick(&mut q), Some(&"low"));
    Stack::check(&q).unwrap();
}

#[test]
fn charge_and_remaining_follow_the_running_tier() {
    let mut q = stack();
    Stack::enqueue(&mut q, 1, "low", Up::Lower(())).unwrap();
    let _ = Stack::pick(&mut q);
    assert_eq!(Stack::remaining_ns(&q), Some(20));
    assert!(!Stack::charge(&mut q, 15));
    assert!(Stack::charge(&mut q, 5));
    Stack::enqueue(&mut q, 2, "high", Up::Upper(())).unwrap();
    let _ = Stack::pick(&mut q);
    assert_eq!(Stack::remaining_ns(&q), Some(10));
}

#[test]
fn a_refused_entity_comes_back_with_its_tier() {
    let mut q = stack();
    Stack::enqueue(&mut q, 1, "one", Up::Upper(())).unwrap();
    let back = Stack::enqueue(&mut q, 1, "again", Up::Upper(())).unwrap_err();
    assert_eq!(back.payload, "again");
    assert_eq!(back.reason, Refusal::Duplicate(1));
    assert!(matches!(back.entity, Tier::Upper(())));
}

#[test]
fn hand_over_is_the_general_sequence_across_tiers() {
    // Within one tier the class's own step; across tiers, enqueue, take the
    // running one off and pick: the arriving upper entity runs.
    let mut q = stack();
    Stack::enqueue(&mut q, 1, "low", Up::Lower(())).unwrap();
    let _ = Stack::pick(&mut q);
    let left = Stack::hand_over(&mut q, 2, "high", Up::Upper(()), 0)
        .unwrap()
        .unwrap();
    assert_eq!((left.0, left.1), (1, "low"));
    assert_eq!(Stack::current(&q), Some(&"high"));
    let left = Stack::hand_over(&mut q, 3, "high2", Up::Upper(()), 0)
        .unwrap()
        .unwrap();
    assert_eq!((left.0, left.1), (2, "high"));
    assert_eq!(Stack::current(&q), Some(&"high2"));
    Stack::check(&q).unwrap();
}

#[test]
fn removal_finds_either_tier() {
    let mut q = stack();
    Stack::enqueue(&mut q, 1, "low", Up::Lower(())).unwrap();
    Stack::enqueue(&mut q, 2, "high", Up::Upper(())).unwrap();
    assert!(matches!(
        Stack::remove(&mut q, 1),
        Some(("low", Tier::Lower(())))
    ));
    assert!(matches!(
        Stack::remove(&mut q, 2),
        Some(("high", Tier::Upper(())))
    ));
    assert!(Stack::remove(&mut q, 3).is_none());
    assert_eq!(Stack::len(&q), 0);
}

#[test]
fn a_stack_over_the_empty_class_is_its_upper_class() {
    type Alone = Over<Fifo, Empty>;
    let mut q = Alone::new_queue((5, ())).unwrap();
    let mut order = Vec::new();
    for (id, name) in [(1, "a"), (2, "b")] {
        Alone::enqueue(&mut q, id, name, Tier::Upper(())).unwrap();
    }
    while let Some(&name) = Alone::pick(&mut q) {
        order.push(name);
        let _ = Alone::remove_current(&mut q);
    }
    assert_eq!(order, ["a", "b"]);
    assert_eq!(<Empty as SchedClass<&'static str>>::NAME, "empty");
    assert!(!Alone::steals_work(&q));
}

#[test]
fn a_bad_configuration_is_refused_by_the_class_it_is_for() {
    assert!(matches!(Stack::new_queue((0, 1)), Err(Refusal::Invalid)));
    assert!(matches!(Stack::new_queue((1, 0)), Err(Refusal::Invalid)));
}

#[test]
fn the_stack_check_finds_a_tier_running_out_of_turn() {
    let mut q = stack();
    Stack::enqueue(&mut q, 1, "high", Up::Upper(())).unwrap();
    let _ = Stack::pick(&mut q);
    Stack::check(&q).unwrap();
    // The lower tier made to run behind the stack's back.
    Stack::enqueue(&mut q, 2, "low", Up::Lower(())).unwrap();
    let _ = Lowly::pick(&mut q.lower);
    assert!(Stack::check(&q).is_err());
}
