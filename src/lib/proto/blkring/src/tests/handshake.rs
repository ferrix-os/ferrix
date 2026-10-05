//! No lost wake-up: every interleaving of the want-bell handshake, stepped on
//! real threads one memory access at a time.
//!
//! Each side runs its real method on its own thread over a [`Lockstep`]
//! memory, which lets exactly one access through at a time and chooses whose
//! by a schedule. The producer's publish is two accesses (write the tail, read
//! the want-bell flag), so an interleaving is fully described by the two
//! global steps at which the producer goes; the tests enumerate every such
//! pair for a consumer of up to [`LIMIT`] accesses, which covers every
//! interleaving of the operations below. A deliberately wrong producer, which
//! reads before it writes, must lose a wake-up in at least one of them — the
//! evidence that the stepping reaches the interleavings that matter.

use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::vec;
use std::vec::Vec;

use super::support::{device, read};
use crate::RingMemory;
use crate::bell::Wait;
use crate::driver::DriverSide;
use crate::kernel::{KernelSide, Slot};
use crate::layout::{RingLayout, Status, header};

/// Global steps enumerated. The longest consumer below takes under 24 accesses.
const LIMIT: u32 = if cfg!(miri) { 8 } else { 26 };

/// The producer's side number; the consumer is the other.
const PRODUCER: usize = 0;

struct State {
    bytes: Vec<u8>,
    /// Global steps at which the producer is preferred.
    producer_steps: u64,
    steps: u32,
    done: [bool; 2],
    /// Before and after a race, anyone may go.
    free: bool,
}

impl State {
    fn whose_turn(&self) -> usize {
        let producer = self.steps < 64 && (self.producer_steps >> self.steps) & 1 == 1;
        let wanted = if producer { PRODUCER } else { 1 - PRODUCER };
        if self.done[wanted] {
            1 - wanted
        } else {
            wanted
        }
    }
}

struct Lockstep {
    state: Mutex<State>,
    turn: Condvar,
}

impl Lockstep {
    fn new(len: u64) -> Self {
        Self {
            state: Mutex::new(State {
                bytes: vec![0; usize::try_from(len).expect("small")],
                producer_steps: 0,
                steps: 0,
                done: [false; 2],
                free: true,
            }),
            turn: Condvar::new(),
        }
    }

    fn handle(&self, side: usize) -> Step<'_> {
        Step { lock: self, side }
    }

    fn with<R>(&self, body: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let result = body(&mut state);
        self.turn.notify_all();
        result
    }

    fn access<R>(&self, side: usize, body: impl FnOnce(&mut Vec<u8>) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while !state.free && state.whose_turn() != side {
            state = self
                .turn
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let result = body(&mut state.bytes);
        state.steps += 1;
        self.turn.notify_all();
        result
    }
}

/// Marks a side finished even if it panics, so the other cannot wait forever.
struct Finished<'a>(&'a Lockstep, usize);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        let side = self.1;
        self.0.with(|state| state.done[side] = true);
    }
}

struct Step<'a> {
    lock: &'a Lockstep,
    side: usize,
}

impl RingMemory for Step<'_> {
    fn read_u8(&self, offset: usize) -> u8 {
        self.lock.access(self.side, |bytes| bytes[offset])
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.lock.access(self.side, |bytes| bytes[offset] = value);
    }

    fn barrier(&self) {}

    fn read_u16(&self, offset: usize) -> u16 {
        self.lock.access(self.side, |bytes| {
            u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("two bytes"))
        })
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        self.lock.access(self.side, |bytes| {
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        });
    }

    fn read_u32(&self, offset: usize) -> u32 {
        self.lock.access(self.side, |bytes| {
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
        })
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.lock.access(self.side, |bytes| {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        });
    }
}

/// Run `produce` and `consume` against each other, the producer going at the
/// steps set in `producer_steps`.
fn race<P: Send, C: Send, RP: Send, RC: Send>(
    lock: &Lockstep,
    producer_steps: u64,
    producer: &mut P,
    produce: impl FnOnce(&mut P) -> RP + Send,
    consumer: &mut C,
    consume: impl FnOnce(&mut C) -> RC + Send,
) -> (RP, RC) {
    lock.with(|state| {
        state.producer_steps = producer_steps;
        state.steps = 0;
        state.done = [false; 2];
        state.free = false;
    });
    let result = thread::scope(|scope| {
        let left = scope.spawn(|| {
            let _finished = Finished(lock, PRODUCER);
            produce(producer)
        });
        let right = scope.spawn(|| {
            let _finished = Finished(lock, 1 - PRODUCER);
            consume(consumer)
        });
        (
            left.join().expect("the producer panicked"),
            right.join().expect("the consumer panicked"),
        )
    });
    lock.with(|state| state.free = true);
    result
}

/// Every placement of the producer's two accesses among `LIMIT` steps.
fn schedules() -> impl Iterator<Item = u64> {
    (0..LIMIT).flat_map(|first| (first + 1..LIMIT).map(move |second| (1 << first) | (1 << second)))
}

fn layout() -> RingLayout {
    RingLayout::standard(4).expect("valid")
}

#[test]
fn a_driver_going_to_sleep_never_misses_a_submission() {
    for schedule in schedules() {
        let lock = Lockstep::new(layout().ring_bytes());
        let mut slots = [Slot::EMPTY; 4];
        let mut driver = DriverSide::new(lock.handle(1), layout().ring_bytes(), layout(), device())
            .expect("fits");
        let mut kernel =
            KernelSide::attach(lock.handle(0), layout().ring_bytes(), device(), &mut slots)
                .expect("fresh");
        kernel.submit(read(1)).expect("room");
        let (bell, wait) = race(
            &lock,
            schedule,
            &mut kernel,
            KernelSide::publish,
            &mut driver,
            DriverSide::prepare_to_sleep,
        );
        let wait = wait.expect("an honest kernel");
        assert!(
            bell.is_some() || wait != Wait::Sleep,
            "{schedule:#b}: the driver slept and the kernel did not ring"
        );
    }
}

#[test]
fn a_driver_waking_up_never_misses_a_submission() {
    for schedule in schedules() {
        let lock = Lockstep::new(layout().ring_bytes());
        let mut slots = [Slot::EMPTY; 4];
        let mut driver = DriverSide::new(lock.handle(1), layout().ring_bytes(), layout(), device())
            .expect("fits");
        let mut kernel =
            KernelSide::attach(lock.handle(0), layout().ring_bytes(), device(), &mut slots)
                .expect("fresh");
        assert_eq!(driver.prepare_to_sleep(), Ok(Wait::Sleep), "asleep");
        kernel.submit(read(1)).expect("room");
        let (bell, (got, wait)) = race(
            &lock,
            schedule,
            &mut kernel,
            KernelSide::publish,
            &mut driver,
            |d| {
                d.woke();
                let mut got = false;
                while let Ok(Some(_)) = d.consume() {
                    got = true;
                }
                (got, d.prepare_to_sleep())
            },
        );
        let wait = wait.expect("an honest kernel");
        assert!(
            got || bell.is_some() || wait != Wait::Sleep,
            "{schedule:#b}: the driver woke, drained, slept, and missed it"
        );
    }
}

#[test]
fn a_kernel_going_to_sleep_never_misses_a_completion() {
    for schedule in schedules() {
        let lock = Lockstep::new(layout().ring_bytes());
        let mut slots = [Slot::EMPTY; 4];
        let mut driver = DriverSide::new(lock.handle(0), layout().ring_bytes(), layout(), device())
            .expect("fits");
        let mut kernel =
            KernelSide::attach(lock.handle(1), layout().ring_bytes(), device(), &mut slots)
                .expect("fresh");
        kernel.submit(read(1)).expect("room");
        let _ = kernel.publish();
        assert!(matches!(driver.consume(), Ok(Some(_))), "taken");
        driver.complete(1, Status::Ok, 512).expect("held");
        let (bell, wait) = race(
            &lock,
            schedule,
            &mut driver,
            DriverSide::publish,
            &mut kernel,
            KernelSide::prepare_to_sleep,
        );
        let wait = wait.expect("an honest driver");
        assert!(
            bell.is_some() || wait != Wait::Sleep,
            "{schedule:#b}: the kernel slept and the driver did not ring"
        );
    }
}

#[test]
fn a_kernel_waking_up_never_misses_a_completion() {
    for schedule in schedules() {
        let lock = Lockstep::new(layout().ring_bytes());
        let mut slots = [Slot::EMPTY; 4];
        let mut driver = DriverSide::new(lock.handle(0), layout().ring_bytes(), layout(), device())
            .expect("fits");
        let mut kernel =
            KernelSide::attach(lock.handle(1), layout().ring_bytes(), device(), &mut slots)
                .expect("fresh");
        kernel.submit(read(1)).expect("room");
        let _ = kernel.publish();
        assert!(matches!(driver.consume(), Ok(Some(_))), "taken");
        assert_eq!(kernel.prepare_to_sleep(), Ok(Wait::Sleep), "asleep");
        driver.complete(1, Status::Ok, 512).expect("held");
        let (bell, (got, wait)) = race(
            &lock,
            schedule,
            &mut driver,
            DriverSide::publish,
            &mut kernel,
            |k| {
                k.woke();
                let mut got = false;
                while let Ok(Some(_)) = k.poll() {
                    got = true;
                }
                (got, k.prepare_to_sleep())
            },
        );
        let wait = wait.expect("an honest driver");
        assert!(
            got || bell.is_some() || wait != Wait::Sleep,
            "{schedule:#b}: the kernel woke, drained, slept, and missed it"
        );
    }
}

#[test]
fn the_stepping_catches_a_producer_that_reads_before_it_writes() {
    let mut lost = false;
    for schedule in schedules() {
        let lock = Lockstep::new(layout().ring_bytes());
        let mut driver = DriverSide::new(lock.handle(1), layout().ring_bytes(), layout(), device())
            .expect("fits");
        let mut wrong = lock.handle(PRODUCER);
        let produce = |memory: &mut Step<'_>| {
            let asked = memory.read_u32(header::SUB_WANT_BELL) == 1;
            memory.write_u32(header::SUB_TAIL, 1);
            asked
        };
        let (rang, wait) = race(&lock, schedule, &mut wrong, produce, &mut driver, |d| {
            d.prepare_to_sleep()
        });
        lost |= !rang && wait == Ok(Wait::Sleep);
    }
    assert!(
        lost,
        "no interleaving lost the wake-up, so the stepping does not reach them"
    );
}
