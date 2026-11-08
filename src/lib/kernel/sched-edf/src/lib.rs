//! `sched_deadline=edf_cbs`: partitioned earliest-deadline-first with a hard constant-bandwidth
//! server, the class stack's deadline tier (`docs/SCHED-CLASSES.md` §9g; ledger 7011 S.4, 7088,
//! 7763).
//!
//! A pure crate: a function of the numbers it is handed, with no clock of its own, no unsafe and
//! no allocation after its queue is made. [`Edf`] is a [`SchedClass`] and a [`DeadlineClass`];
//! the arms file stacks it above the fair class as `Over<Edf, Eevdf>`.
//!
//! # A reservation
//!
//! A thread is admitted to one processor with a [`Reservation`]: `runtime` in every `period`,
//! due by the period's end: an implicit deadline, since the sum of `runtime / period` that
//! admission tests does not suffice for a shorter one (ledger 7088 E.1). The sum is at most
//! [`LIMIT_PPM`] parts per million, which leaves the tier below its share, with at most [`MOST`]
//! on a processor. Its entry in the processor's fixed table is the task's node in this tier:
//! made with the queue, so admission is where [`Refusal::Full`] is answered and no scheduler
//! path allocates. The entity it is queued with, [`Admitted`], carries nothing: the task's fair
//! parts stay with the task while this tier holds it.
//!
//! # The server
//!
//! Running charges the budget. At zero the thread is **throttled**, hard: it does not run again
//! until its period ends, when its budget refills and its deadline moves on a period, whether
//! or not anything below waits ([`SchedClass::ready`] ignores that). A thread that wakes with
//! less time left than its remaining budget's bandwidth asks for is given a fresh budget and
//! deadline, Linux's rule, so that sleeping cannot store up bandwidth.
//!
//! # Time
//!
//! Absolute (ledger 7763 DG.4): [`DeadlineClass::note_time`] before a charge, before a pick and
//! before each enqueue; [`SchedClass::charge`] alone charges the budget and
//! [`SchedClass::elapse`] does nothing.
//!
//! No timing claim is made here: this is the scheduling rule, and the pick is a scan of
//! [`MOST`] entries.

#![no_std]
#![forbid(unsafe_code)]

use ferrix_sched_class::deadline::{DeadlineClass, DeadlineKind, UpperClass};
pub use ferrix_sched_class::deadline::{
    LEAST_PERIOD_NS, LEAST_RUNTIME_NS, LIMIT_PPM, MOST, MOST_PERIOD_NS, Refusal, Reservation,
};
use ferrix_sched_class::{Back, Handed, LOAD_UNIT, Left, SchedClass};

/// The entity of an admitted task: it goes to its admission's entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admitted;

/// A reservation on this processor.
#[derive(Debug)]
struct Entry<T> {
    id: u64,
    reservation: Reservation,
    share_ppm: u64,
    /// What is left of this period's budget.
    left_ns: u64,
    /// When this period's work is due.
    deadline_at: u64,
    /// Out of budget, until `refill_at`.
    throttled: bool,
    /// When a throttled thread's budget comes back.
    refill_at: u64,
    /// On this queue, and not running.
    ready: Option<T>,
    /// Running here.
    running: Option<T>,
    /// Given back: its share is returned, and it goes once no queue holds it here. Never
    /// throttled, and still counted toward [`MOST`] until it goes (ledger 7763 DG.7).
    leaving: bool,
}

/// One processor's reservations.
#[derive(Debug)]
pub struct EdfQueue<T> {
    entries: [Option<Entry<T>>; MOST],
    /// The sum of the entries' shares.
    used_ppm: u64,
    /// The latest time the kernel gave.
    now: u64,
    /// Which entry runs, if one does.
    running: Option<usize>,
    /// How many throttles and refills there have been, for the boot check's line.
    throttles: u64,
    refills: u64,
}

/// The class.
#[derive(Debug)]
pub struct Edf;

impl<T> EdfQueue<T> {
    fn entry_of(&self, id: u64) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|entry| entry.id == id))
    }

    fn entry_mut(&mut self, at: usize) -> Option<&mut Entry<T>> {
        self.entries.get_mut(at).and_then(Option::as_mut)
    }

    fn running_entry(&self) -> Option<&Entry<T>> {
        self.running
            .and_then(|at| self.entries.get(at))
            .and_then(Option::as_ref)
    }

    fn running_entry_mut(&mut self) -> Option<&mut Entry<T>> {
        self.running.and_then(|at| self.entry_mut(at))
    }

    /// Every entry given back that no queue holds here any more goes.
    fn settle_leaving(&mut self) {
        for slot in &mut self.entries {
            if slot.as_ref().is_some_and(|entry| {
                entry.leaving && entry.running.is_none() && entry.ready.is_none()
            }) {
                *slot = None;
            }
        }
    }

    /// The earliest deadline ready with budget.
    fn earliest(&self) -> Option<(usize, u64)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(at, entry)| Some((at, entry.as_ref()?)))
            .filter(|(_, entry)| entry.ready.is_some() && !entry.throttled)
            .min_by_key(|(_, entry)| entry.deadline_at)
            .map(|(at, entry)| (at, entry.deadline_at))
    }

    /// Every budget whose period ended by `now` comes back.
    fn note_time(&mut self, now: u64) {
        self.now = self.now.max(now);
        let now = self.now;
        let mut refilled = 0;
        for entry in self.entries.iter_mut().flatten() {
            if entry.throttled && entry.refill_at <= now {
                let Reservation {
                    runtime_ns,
                    deadline_ns,
                    period_ns,
                } = entry.reservation;
                entry.throttled = false;
                entry.left_ns = runtime_ns;
                // Late by more than a period (a stopped processor, a long way in), it starts
                // from now: a stall is not paid back with budget the set was not admitted for.
                let at = if entry.refill_at.saturating_add(period_ns) <= now {
                    now
                } else {
                    entry.refill_at
                };
                entry.deadline_at = at.saturating_add(deadline_ns);
                entry.refill_at = at.saturating_add(period_ns);
                refilled += 1;
            }
        }
        self.refills += refilled;
    }
}

impl<T> SchedClass<T> for Edf {
    const NAME: &'static str = "edf_cbs";
    type Queue = EdfQueue<T>;
    type Entity = Admitted;
    type Config = ();

    fn new_queue((): ()) -> Result<EdfQueue<T>, ferrix_sched_class::Refusal> {
        Ok(EdfQueue {
            entries: core::array::from_fn(|_| None),
            used_ppm: 0,
            now: 0,
            running: None,
            throttles: 0,
            refills: 0,
        })
    }

    /// The reservations queued here, running or not, a throttled one counted: it is on the
    /// queue.
    fn len(queue: &EdfQueue<T>) -> usize {
        queue
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.ready.is_some() || entry.running.is_some())
            .count()
    }

    /// A reserved thread goes to its entry, under the wake rule. One with no entry here is
    /// refused [`ferrix_sched_class::Refusal::NoSuchTier`]: the kernel builds [`Admitted`] only
    /// for a task admitted on this processor.
    fn enqueue(
        queue: &mut EdfQueue<T>,
        id: u64,
        payload: T,
        entity: Admitted,
    ) -> Result<(), Back<T, Admitted>> {
        let now = queue.now;
        let Some(entry) = queue.entry_of(id).and_then(|at| queue.entry_mut(at)) else {
            return Err(Back {
                payload,
                entity,
                reason: ferrix_sched_class::Refusal::NoSuchTier,
            });
        };
        if entry.ready.is_some() || entry.running.is_some() {
            return Err(Back {
                payload,
                entity,
                reason: ferrix_sched_class::Refusal::Duplicate(id),
            });
        }
        if !entry.throttled && !entry.leaving {
            let Reservation {
                runtime_ns,
                deadline_ns,
                period_ns,
            } = entry.reservation;
            // Linux's dl_entity_overflow: left / (deadline - now) > runtime / period.
            let overflow = now >= entry.deadline_at
                || u128::from(entry.left_ns) * u128::from(period_ns)
                    > u128::from(entry.deadline_at - now) * u128::from(runtime_ns);
            if overflow {
                entry.left_ns = runtime_ns;
                entry.deadline_at = now.saturating_add(deadline_ns);
                entry.refill_at = now.saturating_add(period_ns);
            }
        }
        entry.ready = Some(payload);
        Ok(())
    }

    /// Queued or running: it stays admitted here, and is the tier below's wherever it goes until
    /// it comes back here; a given-back entry goes with it.
    fn remove(queue: &mut EdfQueue<T>, id: u64) -> Option<(T, Admitted)> {
        let at = queue.entry_of(id)?;
        let runs = queue.running == Some(at);
        let entry = queue.entry_mut(at)?;
        let payload = if runs {
            entry.running.take()
        } else {
            entry.ready.take()
        }?;
        if runs {
            queue.running = None;
        }
        queue.settle_leaving();
        Some((payload, Admitted))
    }

    fn remove_current(queue: &mut EdfQueue<T>) -> Option<Left<T, Admitted>> {
        let at = queue.running.take()?;
        let entry = queue.entry_mut(at)?;
        let id = entry.id;
        let left = entry.running.take().map(|payload| (id, payload, Admitted));
        queue.settle_leaving();
        left
    }

    /// The earliest deadline with budget, the running one competing again.
    fn pick(queue: &mut EdfQueue<T>) -> Option<&T> {
        Self::put_back(queue);
        let (at, _) = queue.earliest()?;
        let entry = queue.entry_mut(at)?;
        entry.running = entry.ready.take();
        queue.running = Some(at);
        queue.running_entry()?.running.as_ref()
    }

    fn put_back(queue: &mut EdfQueue<T>) {
        if let Some(entry) = queue.running_entry_mut() {
            entry.ready = entry.running.take();
        }
        queue.running = None;
    }

    fn current(queue: &EdfQueue<T>) -> Option<&T> {
        queue.running_entry()?.running.as_ref()
    }

    /// Never fused: the general sequence, so that the direct switch never passes over a
    /// reservation (ledger 7763 DG.7; 7088 E.5).
    fn hand_over(
        queue: &mut EdfQueue<T>,
        id: u64,
        payload: T,
        entity: Admitted,
        _: u64,
    ) -> Handed<T, Admitted> {
        Self::enqueue(queue, id, payload, entity)?;
        let left = Self::remove_current(queue);
        let _ = Self::pick(queue);
        Ok(left)
    }

    /// A running reservation's budget is charged; at zero it is throttled. Whether a decision
    /// is due.
    fn charge(queue: &mut EdfQueue<T>, ran_ns: u64) -> bool {
        let Some(entry) = queue.running_entry_mut() else {
            return false;
        };
        let ended = entry.left_ns <= ran_ns;
        entry.left_ns = entry.left_ns.saturating_sub(ran_ns);
        if ended && !entry.throttled && !entry.leaving {
            entry.throttled = true;
            queue.throttles += 1;
        }
        ended
    }

    fn remaining_ns(queue: &EdfQueue<T>) -> Option<u64> {
        queue.running_entry().map(|entry| entry.left_ns)
    }

    /// A reservation out of budget gives way, and any with budget and an earlier deadline takes
    /// the processor.
    fn should_preempt(queue: &EdfQueue<T>) -> bool {
        let earliest = queue.earliest().map(|(_, at)| at);
        match queue.running_entry() {
            Some(running) => {
                (running.throttled && !running.leaving)
                    || earliest.is_some_and(|at| at < running.deadline_at)
            }
            None => earliest.is_some(),
        }
    }

    /// A running budget's end, and the next refill.
    fn decision_in_ns(queue: &EdfQueue<T>) -> Option<u64> {
        let refill = queue
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.throttled)
            .map(|entry| entry.refill_at.saturating_sub(queue.now))
            .min();
        let here = queue.running_entry().map(|entry| entry.left_ns);
        match (refill, here) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn load_weight(queue: &EdfQueue<T>) -> u64 {
        LOAD_UNIT * Self::len(queue) as u64
    }

    /// A reservation gives up its budget until its next period.
    fn yield_current(queue: &mut EdfQueue<T>) {
        let Some(entry) = queue.running_entry_mut() else {
            return;
        };
        entry.left_ns = 0;
        if !entry.throttled && !entry.leaving {
            entry.throttled = true;
            queue.throttles += 1;
        }
    }

    /// A reservation with budget, waiting or running: whatever waits below, since the budget
    /// is hard.
    fn ready(queue: &EdfQueue<T>, _: bool) -> bool {
        queue.earliest().is_some() || queue.running_entry().is_some_and(|entry| !entry.throttled)
    }

    /// Nothing: this class's time is absolute (DG.4).
    fn elapse(_: &mut EdfQueue<T>, _: u64) {}

    /// Never: a reservation is its processor's.
    fn steals_work(_: &EdfQueue<T>) -> bool {
        false
    }
    // Never: only FifoRr says it is dormant (ledger 8057, SD.1).
    fn dormant(_: &EdfQueue<T>) -> bool {
        false
    }

    fn check(queue: &EdfQueue<T>) -> Result<(), &'static str> {
        let shares: u64 = queue
            .entries
            .iter()
            .flatten()
            .map(|entry| entry.share_ppm)
            .sum();
        if shares != queue.used_ppm {
            return Err("edf: the shares' sum is not the entries'");
        }
        if queue.used_ppm > LIMIT_PPM {
            return Err("edf: more admitted than the limit");
        }
        if let Some(at) = queue.running
            && queue
                .entries
                .get(at)
                .and_then(Option::as_ref)
                .is_none_or(|entry| entry.running.is_none())
        {
            return Err("edf: the running entry holds no task");
        }
        if queue
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.leaving && (entry.throttled || entry.share_ppm != 0))
        {
            return Err("edf: an entry given back is throttled or still holds a share");
        }
        Ok(())
    }
}

impl<T> DeadlineClass<T> for Edf {
    fn admitted() -> Admitted {
        Admitted
    }

    fn admit(queue: &mut EdfQueue<T>, id: u64, reservation: Reservation) -> Result<(), Refusal> {
        let share_ppm = reservation.share_ppm()?;
        if queue.entry_of(id).is_some() {
            return Err(Refusal::Already);
        }
        if queue.used_ppm.saturating_add(share_ppm) > LIMIT_PPM {
            return Err(Refusal::Unschedulable);
        }
        let Some(free) = queue.entries.iter_mut().find(|entry| entry.is_none()) else {
            return Err(Refusal::Full);
        };
        *free = Some(Entry {
            id,
            reservation,
            share_ppm,
            left_ns: 0,
            deadline_at: 0,
            throttled: false,
            refill_at: 0,
            ready: None,
            running: None,
            leaving: false,
        });
        queue.used_ppm += share_ppm;
        Ok(())
    }

    fn withdraw(queue: &mut EdfQueue<T>, id: u64) -> bool {
        let Some(at) = queue.entry_of(id) else {
            return false;
        };
        let Some(entry) = queue.entry_mut(at) else {
            return false;
        };
        if entry.leaving {
            return false;
        }
        let share = entry.share_ppm;
        entry.share_ppm = 0;
        entry.throttled = false;
        entry.leaving = true;
        queue.used_ppm = queue.used_ppm.saturating_sub(share);
        queue.settle_leaving();
        true
    }

    fn note_time(queue: &mut EdfQueue<T>, now: u64) {
        queue.note_time(now);
    }

    fn is_reserved(queue: &EdfQueue<T>, id: u64) -> bool {
        queue.entry_of(id).is_some()
    }

    fn any_ready(queue: &EdfQueue<T>) -> bool {
        queue.earliest().is_some()
    }

    fn runs_reserved(queue: &EdfQueue<T>) -> bool {
        queue.running.is_some()
    }

    fn state(queue: &EdfQueue<T>) -> (u64, u64, u64) {
        (queue.used_ppm, queue.throttles, queue.refills)
    }

    fn waiting_where(queue: &EdfQueue<T>, matches: impl Fn(&T) -> bool) -> Option<u64> {
        queue
            .entries
            .iter()
            .flatten()
            .find(|entry| entry.ready.as_ref().is_some_and(&matches))
            .map(|entry| entry.id)
    }
}

/// A deadline tier, to the stack's one impl over its upper tier (ledger 7763 DG.6).
impl<T> UpperClass<T> for Edf {
    type Kind = DeadlineKind;
}

#[cfg(test)]
mod tests;
