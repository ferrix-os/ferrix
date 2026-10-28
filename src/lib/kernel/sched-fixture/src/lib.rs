//! A scheduling class no build flag selects (docs/SCHED-CLASSES.md §9b; the
//! consultant's design review of step 3b, ledger line 22, G1).
//!
//! The selection crate depends on it and no file names it, so rustc never
//! loads it and no kernel links it: test-removable's class-crate half shows
//! that on every architecture and buildable profile, from the release ELF
//! and a non-LTO link's map. It is the standing negative case, kept after
//! step 4 until a ledger line retires it. A first-in, first-out list of at
//! most [`CAPACITY`] entities, charging a fixed quantum, without
//! preemption: `pick` keeps the running entity rather than letting it
//! compete with the waiting ones, as a class that schedules would (ledger
//! line 23, C3). No flag value names it, so it is never a scheduler a
//! profile can choose.

#![no_std]
#![forbid(unsafe_code)]

use ferrix_sched_class::{Back, Handed, Left, Refusal, SchedClass};

/// The most entities one queue holds.
pub const CAPACITY: usize = 16;

/// The class.
#[derive(Debug)]
pub struct Fixture;

/// One CPU's queue: waiting entities in arrival order, the running one and
/// what it has used of its quantum.
#[derive(Debug)]
pub struct Queue<T> {
    waiting: [Option<(u64, T)>; CAPACITY],
    running: Option<(u64, T, u64)>,
    quantum: u64,
}

impl<T> Queue<T> {
    fn waiting_len(&self) -> usize {
        self.waiting.iter().filter(|slot| slot.is_some()).count()
    }

    fn holds(&self, id: u64) -> bool {
        self.running
            .as_ref()
            .is_some_and(|(other, ..)| *other == id)
            || self.waiting.iter().flatten().any(|(other, _)| *other == id)
    }

    /// Take the oldest waiting entity, keeping the rest in order.
    fn pop_front(&mut self) -> Option<(u64, T)> {
        let first = self.waiting[0].take();
        self.waiting.rotate_left(1);
        first
    }

    /// Put an entity before every waiting one; false when full.
    fn push_front(&mut self, entry: (u64, T)) -> bool {
        if self.waiting[CAPACITY - 1].is_some() {
            return false;
        }
        self.waiting.rotate_right(1);
        self.waiting[0] = Some(entry);
        true
    }
}

impl<T> SchedClass<T> for Fixture {
    const NAME: &'static str = "fixture";
    type Queue = Queue<T>;
    type Entity = ();
    type Config = u64;

    fn new_queue(quantum: u64) -> Result<Queue<T>, Refusal> {
        if quantum == 0 {
            return Err(Refusal::Invalid);
        }
        Ok(Queue {
            waiting: core::array::from_fn(|_| None),
            running: None,
            quantum,
        })
    }

    fn len(queue: &Queue<T>) -> usize {
        queue.waiting_len() + usize::from(queue.running.is_some())
    }

    fn enqueue(queue: &mut Queue<T>, id: u64, payload: T, (): ()) -> Result<(), Back<T, ()>> {
        let reason = if queue.holds(id) {
            Refusal::Duplicate(id)
        } else if let Some(slot) = queue.waiting.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some((id, payload));
            return Ok(());
        } else {
            // Full: admitting it would break the queue's bound.
            Refusal::Unschedulable
        };
        Err(Back {
            payload,
            entity: (),
            reason,
        })
    }

    fn remove(queue: &mut Queue<T>, id: u64) -> Option<(T, ())> {
        if queue
            .running
            .as_ref()
            .is_some_and(|(other, ..)| *other == id)
        {
            return queue.running.take().map(|(_, payload, _)| (payload, ()));
        }
        let at = queue
            .waiting
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|(other, _)| *other == id))?;
        let rest = queue.waiting.get_mut(at..)?;
        let (_, payload) = rest.first_mut()?.take()?;
        rest.rotate_left(1);
        Some((payload, ()))
    }

    fn remove_current(queue: &mut Queue<T>) -> Option<Left<T, ()>> {
        queue
            .running
            .take()
            .map(|(id, payload, _)| (id, payload, ()))
    }

    fn pick(queue: &mut Queue<T>) -> Option<&T> {
        if queue.running.is_none() {
            queue.running = queue.pop_front().map(|(id, payload)| (id, payload, 0));
        }
        queue.running.as_ref().map(|(_, payload, _)| payload)
    }

    fn put_back(queue: &mut Queue<T>) {
        if let Some((id, payload, used)) = queue.running.take() {
            if queue.waiting[CAPACITY - 1].is_some() {
                // Full: nowhere to put it before the others, so it keeps
                // the processor.
                queue.running = Some((id, payload, used));
            } else {
                let _ = queue.push_front((id, payload));
            }
        }
    }

    fn current(queue: &Queue<T>) -> Option<&T> {
        queue.running.as_ref().map(|(_, payload, _)| payload)
    }

    fn hand_over(queue: &mut Queue<T>, id: u64, payload: T, (): (), _: u64) -> Handed<T, ()> {
        Self::enqueue(queue, id, payload, ())?;
        let left = Self::remove_current(queue);
        let _ = Self::pick(queue);
        Ok(left)
    }

    fn charge(queue: &mut Queue<T>, ran_ns: u64) -> bool {
        let quantum = queue.quantum;
        queue.running.as_mut().is_some_and(|(_, _, used)| {
            *used = used.saturating_add(ran_ns);
            *used >= quantum
        })
    }

    fn remaining_ns(queue: &Queue<T>) -> Option<u64> {
        queue
            .running
            .as_ref()
            .map(|(.., used)| queue.quantum.saturating_sub(*used))
    }

    fn should_preempt(queue: &Queue<T>) -> bool {
        queue.waiting_len() > 0
            && queue
                .running
                .as_ref()
                .is_none_or(|(.., used)| *used >= queue.quantum)
    }

    fn decision_in_ns(queue: &Queue<T>) -> Option<u64> {
        if queue.waiting_len() == 0 {
            return None;
        }
        Some(
            queue
                .running
                .as_ref()
                .map_or(0, |(.., used)| queue.quantum.saturating_sub(*used)),
        )
    }

    fn load_weight(queue: &Queue<T>) -> u64 {
        (Self::len(queue) as u64).saturating_mul(ferrix_sched_class::LOAD_UNIT)
    }

    fn yield_current(queue: &mut Queue<T>) {
        if let Some((id, payload, used)) = queue.running.take() {
            if let Some(slot) = queue.waiting.iter_mut().find(|slot| slot.is_none()) {
                *slot = Some((id, payload));
            } else {
                // Full: nowhere to wait, so it keeps the processor.
                queue.running = Some((id, payload, used));
            }
        }
    }

    fn ready(queue: &Queue<T>, _: bool) -> bool {
        Self::len(queue) > 0
    }

    fn elapse(_: &mut Queue<T>, _: u64) {}

    fn steals_work(_: &Queue<T>) -> bool {
        false
    }

    #[inline(never)]
    fn check(queue: &Queue<T>) -> Result<(), &'static str> {
        if queue.quantum == 0 {
            return Err("a fixture queue with a zero quantum");
        }
        let mut seen_gap = false;
        for slot in &queue.waiting {
            match (slot.is_some(), seen_gap) {
                (true, true) => {
                    return Err("a fixture queue with a gap among its waiting entities");
                }
                (false, _) => seen_gap = true,
                (true, false) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
