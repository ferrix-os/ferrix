//! The fixture class's rules, on the host.

use ferrix_sched_class::{Refusal, SchedClass};

use super::{CAPACITY, Fixture, Queue};

fn queue() -> Queue<u32> {
    <Fixture as SchedClass<u32>>::new_queue(10).unwrap()
}

#[test]
fn it_runs_in_arrival_order_and_refuses_a_duplicate() {
    let mut q = queue();
    for id in 1..=3 {
        Fixture::enqueue(&mut q, id, id as u32 * 10, ()).unwrap();
    }
    assert!(matches!(
        Fixture::enqueue(&mut q, 2, 99, ()),
        Err(back) if back.reason == Refusal::Duplicate(2) && back.payload == 99
    ));
    let mut order = [0; 3];
    for slot in &mut order {
        *slot = *Fixture::pick(&mut q).unwrap();
        let _ = Fixture::remove_current(&mut q);
    }
    assert_eq!(order, [10, 20, 30]);
    assert_eq!(Fixture::len(&q), 0);
    assert!(Fixture::check(&q).is_ok());
}

#[test]
fn a_zero_quantum_is_refused_and_a_full_queue_refuses() {
    assert!(matches!(
        <Fixture as SchedClass<u32>>::new_queue(0),
        Err(Refusal::Invalid)
    ));
    let mut q = queue();
    for id in 0..CAPACITY as u64 {
        Fixture::enqueue(&mut q, id, 0, ()).unwrap();
    }
    assert!(matches!(
        Fixture::enqueue(&mut q, 99, 7, ()),
        Err(back) if back.reason == Refusal::Unschedulable && back.payload == 7
    ));
}

#[test]
fn the_quantum_is_charged_and_put_back_keeps_order() {
    let mut q = queue();
    Fixture::enqueue(&mut q, 1, 1, ()).unwrap();
    Fixture::enqueue(&mut q, 2, 2, ()).unwrap();
    assert_eq!(Fixture::pick(&mut q), Some(&1));
    assert!(!Fixture::charge(&mut q, 4));
    assert_eq!(Fixture::remaining_ns(&q), Some(6));
    assert!(Fixture::charge(&mut q, 6));
    Fixture::put_back(&mut q);
    assert_eq!(Fixture::current(&q), None);
    assert_eq!(Fixture::pick(&mut q), Some(&1));
    assert_eq!(Fixture::remove(&mut q, 2), Some((2, ())));
    assert!(Fixture::check(&q).is_ok());
}

#[test]
fn hand_over_queues_the_arrival_and_lets_the_running_one_go() {
    let mut q = queue();
    Fixture::enqueue(&mut q, 1, 1, ()).unwrap();
    let _ = Fixture::pick(&mut q);
    let left = Fixture::hand_over(&mut q, 2, 2, (), 0).ok().flatten();
    assert_eq!(left, Some((1, 1, ())));
    assert_eq!(Fixture::current(&q), Some(&2));
    assert_eq!(<Fixture as SchedClass<u32>>::NAME, "fixture");
    assert!(!Fixture::steals_work(&q));
}
