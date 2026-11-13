//! The token against the recording account.

use std::sync::Arc;
use std::vec::Vec;

use std::collections::VecDeque;

use crate::testing::Job;
use crate::{
    Charge, NOBODY, Refused, arc_footprint, boxed_footprint, buffer_footprint, current, footprint,
    reserve, reserve_deque,
};

#[test]
fn footprint_is_the_heaps_class_or_its_pages() {
    assert_eq!(footprint(0, 1), 16);
    assert_eq!(footprint(1, 1), 16);
    assert_eq!(footprint(9, 8), 16);
    assert_eq!(footprint(100, 8), 128);
    assert_eq!(footprint(2048, 8), 2048);
    assert_eq!(footprint(2049, 8), 4096);
    assert_eq!(footprint(4097, 8), 8192);
    assert_eq!(footprint(3 * 4096, 8), 4 * 4096);
    assert_eq!(
        footprint(4, 64),
        64,
        "an alignment above the size picks the class"
    );
    assert_eq!(boxed_footprint::<[u8; 24]>(), 32);
    assert_eq!(arc_footprint::<[u64; 2]>(), 32, "two counts and the value");
    assert_eq!(
        buffer_footprint::<u8>(0),
        0,
        "an empty buffer allocates nothing"
    );
    assert_eq!(buffer_footprint::<u8>(65536), 65536);
    assert_eq!(buffer_footprint::<u64>(usize::MAX), usize::MAX);
}

#[test]
fn a_charge_goes_to_the_running_job_and_comes_back_on_drop() {
    let job = Job::enter(1000);
    assert_eq!(current(), job.id());
    let charge = Charge::bytes(400).unwrap();
    assert_eq!(charge.owner(), job.id());
    assert_eq!((job.used(), job.holds()), (400, 1));
    let second = Charge::bytes(600).unwrap();
    assert_eq!(
        Charge::bytes(1),
        Err(Refused),
        "the limit refuses one byte more"
    );
    assert_eq!((job.used(), job.refused(), job.holds()), (1000, 1, 2));
    drop(charge);
    drop(second);
    assert_eq!((job.used(), job.holds()), (0, 0));
}

#[test]
fn growth_is_charged_to_the_same_job_whoever_runs() {
    let maker = Job::enter(100);
    let mut charge = Charge::bytes(10).unwrap();
    let other = Job::enter(u64::MAX);
    charge.grow(80).unwrap();
    assert_eq!(
        charge.grow(20),
        Err(Refused),
        "the maker's limit, not the runner's"
    );
    assert_eq!((maker.used(), other.used()), (90, 0));
    assert_eq!(
        charge.charged(),
        90,
        "a refused growth leaves the charge as it was"
    );
    charge.shrink(50);
    assert_eq!(maker.used(), 40);
    charge.resize(100).unwrap();
    assert_eq!(maker.used(), 100);
    charge.resize(1).unwrap();
    assert_eq!(maker.used(), 1);
    charge.shrink(1000);
    assert_eq!((maker.used(), charge.charged()), (0, 0), "never below zero");
    drop(charge);
    assert_eq!(maker.holds(), 0);
    drop(other);
}

#[test]
fn a_buffer_is_charged_for_its_room_and_refused_with_nothing_changed() {
    let job = Job::enter(1024);
    let mut vec: Vec<u64> = Vec::new();
    let mut charge = Charge::bytes(0).unwrap();
    for value in 0..100 {
        reserve(&mut vec, 1, &mut charge).unwrap();
        vec.push(value);
        assert_eq!(
            charge.charged(),
            buffer_footprint::<u64>(vec.capacity()) as u64
        );
    }
    assert_eq!((vec.len(), vec.capacity()), (100, 128));
    assert_eq!(job.used(), 1024);
    assert_eq!(
        reserve(&mut vec, 29, &mut charge),
        Err(Refused),
        "256 u64s is past the limit"
    );
    assert_eq!(
        (vec.capacity(), charge.charged()),
        (128, 1024),
        "nothing changed"
    );
    reserve(&mut vec, 28, &mut charge).unwrap();
    assert_eq!(vec.capacity(), 128, "room enough is no growth");

    let mut deque: VecDeque<u8> = VecDeque::new();
    let mut queued = Charge::bytes(0).unwrap();
    job.set_limit(u64::MAX);
    reserve_deque(&mut deque, 3, &mut queued).unwrap();
    assert_eq!(
        (deque.capacity(), queued.charged()),
        (4, 16),
        "at least four, the smallest class, 16 bytes"
    );
    drop((vec, charge, deque, queued));
    assert_eq!((job.used(), job.holds()), (0, 0));
}

#[test]
fn nothing_is_never_refused() {
    let job = Job::enter(10);
    let full = Charge::bytes(10).unwrap();
    job.set_limit(5);
    let mut empty = Charge::bytes(0).unwrap();
    assert_eq!(
        (empty.owner(), job.holds()),
        (job.id(), 2),
        "an empty charge still names its job"
    );
    assert_eq!(empty.grow(0), Ok(()));
    assert_eq!(empty.grow(1), Err(Refused));
    drop((full, empty));
    assert_eq!((job.used(), job.holds()), (0, 0));
}

#[test]
fn outside_any_job_nothing_is_charged_or_refused() {
    let charge = Charge::bytes(usize::MAX >> 1).unwrap();
    assert_eq!(charge.owner(), NOBODY);
    let none = Charge::none();
    assert_eq!((none.owner(), none.charged()), (NOBODY, 0));
}

#[test]
fn a_charge_outlives_its_job_and_goes_back_to_it() {
    let job = Job::enter(1 << 20);
    let charges: Vec<Charge> = (0..10)
        .map(|_| Charge::arc::<[u8; 100]>().unwrap())
        .collect();
    let owner = job.id();
    let used = job.used();
    assert_eq!(used, 10 * 128);
    // Handed to another thread, as a file sent over a socket is handed to
    // another process: the charge stays the maker's.
    let handed = Arc::new(charges);
    let elsewhere = Arc::clone(&handed);
    std::thread::spawn(move || {
        let receiver = Job::enter(0);
        assert!(elsewhere.iter().all(|charge| charge.owner() == owner));
        drop(elsewhere);
        assert_eq!(receiver.used(), 0);
    })
    .join()
    .unwrap();
    assert_eq!(job.used(), used);
    drop(handed);
    assert_eq!((job.used(), job.holds()), (0, 0));
}
