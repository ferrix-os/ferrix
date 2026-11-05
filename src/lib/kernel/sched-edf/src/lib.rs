//! `schedule=edf`: partitioned earliest-deadline-first with a hard constant-bandwidth
//! server, above the fair class (`docs/BUILD-FLAGS.md`, ledger 7011 S.4).
//!
//! A pure crate: a function of the numbers it is handed, with no clock of its own, no
//! unsafe and no allocation after construction. [`OverFair`] answers every method of
//! [`RunQueue`] the kernel calls, so one type holds every door.
//!
//! # A reservation
//!
//! A thread is admitted to one processor with a [`Reservation`]: `runtime` in every
//! `period`, due by the period's end: an implicit deadline, since the sum of `runtime /
//! period` that admission tests does not suffice for a shorter one (ledger 7088 E.1). The
//! sum is at most [`LIMIT_PPM`] parts per million, which leaves the fair class its share,
//! with at most [`MOST`] on a processor.
//!
//! # The server
//!
//! Running charges the budget. At zero the thread is **throttled**, hard: it does not run
//! again until its period ends, when its budget refills and its deadline moves on a period.
//! A thread that wakes with less time left than its remaining budget's bandwidth asks for
//! is given a fresh budget and deadline, Linux's rule, so that sleeping cannot store up
//! bandwidth.
//!
//! No timing claim is made here: this is the scheduling rule, and [`OverFair`]'s pick is a
//! scan of [`MOST`] entries.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use ferrix_sched::{Config, EntityState, EntityView, Left, Refused, RunQueue, SchedError, Slot};

/// The most reservations on one processor: what bounds a pick's scan.
pub const MOST: usize = 32;
/// The most of a processor reservations may ask for, in parts per million.
pub const LIMIT_PPM: u64 = 950_000;
/// The least budget a reservation may have: Linux's 1,024 ns would be shorter than a switch.
pub const LEAST_RUNTIME_NS: u64 = 100_000;
/// The shortest period, which bounds the refill timer's rate.
pub const LEAST_PERIOD_NS: u64 = 1_000_000;
/// The longest period, which keeps the arithmetic in range.
pub const MOST_PERIOD_NS: u64 = 10_000_000_000;

/// What a thread asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    /// Its budget in each period.
    pub runtime_ns: u64,
    /// When each period's work is due, from the period's start.
    pub deadline_ns: u64,
    /// The period.
    pub period_ns: u64,
}

/// Why a thread was not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not `LEAST_RUNTIME_NS <= runtime <= period`, `deadline == period`, and the period
    /// within `LEAST_PERIOD_NS` to `MOST_PERIOD_NS`.
    Malformed,
    /// With it the processor's reservations would pass [`LIMIT_PPM`]: an unschedulable set.
    Unschedulable,
    /// The processor already holds [`MOST`].
    Full,
    /// It is already admitted.
    Already,
}

impl Reservation {
    /// Its share of a processor, in parts per million, rounded up, or why it has none.
    ///
    /// # Errors
    ///
    /// [`Refusal::Malformed`].
    pub const fn share_ppm(self) -> Result<u64, Refusal> {
        if self.runtime_ns < LEAST_RUNTIME_NS
            || self.runtime_ns > self.period_ns
            || self.deadline_ns != self.period_ns
            || self.period_ns < LEAST_PERIOD_NS
            || self.period_ns > MOST_PERIOD_NS
        {
            return Err(Refusal::Malformed);
        }
        // At most 10^10 * 10^6, well inside 64 bits.
        Ok((self.runtime_ns * 1_000_000).div_ceil(self.period_ns))
    }
}

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
    /// On this queue, and not running: what the fair class would hold of it.
    ready: Option<(T, EntityState, Slot<T>)>,
    /// Running here, the same three.
    running: Option<(T, EntityState, Slot<T>)>,
    /// Given back while it runs: the fair class's at its next way off the processor. Never
    /// put in the fair class's tree while it runs, where a steal would find it.
    leaving: bool,
}

/// One processor's classes: its reservations, and the fair class beneath them.
#[derive(Debug)]
pub struct OverFair<T> {
    fair: RunQueue<T>,
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

impl<T> OverFair<T> {
    /// An empty pair of classes.
    ///
    /// # Errors
    ///
    /// As [`RunQueue::new`].
    pub fn new(config: Config) -> Result<OverFair<T>, SchedError> {
        Ok(OverFair {
            fair: RunQueue::new(config)?,
            entries: core::array::from_fn(|_| None),
            used_ppm: 0,
            now: 0,
            running: None,
            throttles: 0,
            refills: 0,
        })
    }

    /// Admit thread `id`.
    ///
    /// # Errors
    ///
    /// [`Refusal`], with nothing changed.
    pub fn admit(&mut self, id: u64, reservation: Reservation) -> Result<(), Refusal> {
        let share_ppm = reservation.share_ppm()?;
        if self.entry_of(id).is_some() {
            return Err(Refusal::Already);
        }
        if self.used_ppm.saturating_add(share_ppm) > LIMIT_PPM {
            return Err(Refusal::Unschedulable);
        }
        let Some(free) = self.entries.iter_mut().find(|entry| entry.is_none()) else {
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
        self.used_ppm += share_ppm;
        Ok(())
    }

    /// Take thread `id`'s reservation away. Answers false if it holds none, or is on this
    /// queue: it must leave the queue first, and so comes back to the fair class.
    pub fn discharge(&mut self, id: u64) -> bool {
        let Some(at) = self.entry_of(id) else {
            return false;
        };
        let Some(entry) = self.entries.get_mut(at) else {
            return false;
        };
        let held = entry
            .as_ref()
            .is_some_and(|entry| entry.ready.is_some() || entry.running.is_some());
        if held {
            return false;
        }
        if let Some(gone) = entry.take() {
            self.used_ppm = self.used_ppm.saturating_sub(gone.share_ppm);
        }
        true
    }

    /// Take thread `id`'s reservation away wherever it stands on this queue: ready, it moves
    /// to the fair class now; running, its share comes back now and it moves at its next way
    /// off the processor. Answers whether it held one.
    pub fn withdraw(&mut self, id: u64) -> bool {
        let Some(at) = self.entry_of(id) else {
            return false;
        };
        let Some(entry) = self.entries.get_mut(at).and_then(Option::as_mut) else {
            return false;
        };
        if entry.leaving {
            return false;
        }
        self.used_ppm = self.used_ppm.saturating_sub(entry.share_ppm);
        entry.share_ppm = 0;
        entry.throttled = false;
        entry.leaving = true;
        if entry.running.is_none() {
            self.settle_leaving();
        }
        true
    }

    /// Every entry given back and not running now goes: its thread, if queued, to the fair class.
    fn settle_leaving(&mut self) {
        for slot in &mut self.entries {
            if !slot
                .as_ref()
                .is_some_and(|entry| entry.leaving && entry.running.is_none())
            {
                continue;
            }
            let Some(mut gone) = slot.take() else {
                continue;
            };
            if let Some((payload, state, parts)) = gone.ready.take() {
                // Only a duplicate or a zero weight is refused, and it held neither there.
                drop(self.fair.enqueue(gone.id, payload, state, parts));
            }
        }
    }

    /// A thread just admitted here leaves the fair class for its entry, running or queued,
    /// and the classes choose again: otherwise it would stay fair until it next slept.
    pub fn adopt(&mut self, id: u64) {
        let parts = if self.fair.current_id() == Some(id) {
            self.fair.remove_curr()
        } else {
            self.fair
                .remove(id)
                .map(|(payload, state, slot)| (id, payload, state, slot))
        };
        if let Some((id, payload, state, slot)) = parts {
            drop(self.enqueue(id, payload, state, slot));
        }
    }

    /// Whether thread `id` holds a reservation here.
    #[must_use]
    pub fn is_reserved(&self, id: u64) -> bool {
        self.entry_of(id).is_some()
    }

    /// Whether a reservation with budget is waiting to run.
    #[must_use]
    pub fn any_ready(&self) -> bool {
        self.entries
            .iter()
            .flatten()
            .any(|entry| entry.ready.is_some() && !entry.throttled)
    }

    /// Whether a reservation is running here.
    #[must_use]
    pub const fn runs_reserved(&self) -> bool {
        self.running.is_some()
    }

    /// The sum of the shares, in parts per million.
    #[must_use]
    pub const fn used_ppm(&self) -> u64 {
        self.used_ppm
    }

    /// How many times a reservation ran out of budget and came back.
    #[must_use]
    pub const fn throttles_and_refills(&self) -> (u64, u64) {
        (self.throttles, self.refills)
    }

    fn entry_of(&self, id: u64) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|entry| entry.id == id))
    }

    fn running_entry(&self) -> Option<&Entry<T>> {
        self.running
            .and_then(|at| self.entries.get(at))
            .and_then(Option::as_ref)
    }

    fn running_entry_mut(&mut self) -> Option<&mut Entry<T>> {
        self.running
            .and_then(|at| self.entries.get_mut(at))
            .and_then(Option::as_mut)
    }

    /// Tell the class the time: every budget whose period ended by `now` comes back.
    pub fn note_time(&mut self, now: u64) {
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

    /// As [`RunQueue::enqueue`]: a reserved thread goes to its entry, under the wake rule.
    ///
    /// # Errors
    ///
    /// As [`RunQueue::enqueue`].
    pub fn enqueue(
        &mut self,
        id: u64,
        payload: T,
        state: EntityState,
        slot: Slot<T>,
    ) -> Result<(), Refused<T>> {
        let now = self.now;
        let at = self.entry_of(id);
        let Some(entry) = at
            .and_then(|at| self.entries.get_mut(at))
            .and_then(Option::as_mut)
        else {
            return self.fair.enqueue(id, payload, state, slot);
        };
        if !entry.throttled {
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
        entry.ready = Some((payload, state, slot));
        Ok(())
    }

    /// As [`RunQueue::pick_next`], which puts what runs back first: the earliest deadline
    /// with budget, else the fair class.
    pub fn pick_next(&mut self) -> Option<&T> {
        self.note_time(self.now);
        if let Some(entry) = self.running_entry_mut() {
            entry.ready = entry.running.take();
        }
        self.running = None;
        self.settle_leaving();
        let earliest = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(at, entry)| Some((at, entry.as_ref()?)))
            .filter(|(_, entry)| entry.ready.is_some() && !entry.throttled)
            .min_by_key(|(_, entry)| entry.deadline_at)
            .map(|(at, _)| at);
        let Some(entry) = earliest
            .and_then(|at| self.entries.get_mut(at))
            .and_then(Option::as_mut)
        else {
            return self.fair.pick_next();
        };
        entry.running = entry.ready.take();
        self.running = earliest;
        self.fair.put_curr_back();
        self.current()
    }

    /// As [`RunQueue::hand_over`], the same long way when a reservation is here at all.
    ///
    /// # Errors
    ///
    /// As [`RunQueue::hand_over`].
    pub fn hand_over(
        &mut self,
        id: u64,
        payload: T,
        state: EntityState,
        slot: Slot<T>,
        slice_after: u64,
    ) -> Result<Option<Left<T>>, Refused<T>> {
        if self.running.is_none() && !self.any_ready() && self.entry_of(id).is_none() {
            return self.fair.hand_over(id, payload, state, slot, slice_after);
        }
        self.enqueue(id, payload, state, slot)?;
        let _ = self.fair.set_slice_ns(slice_after);
        let left = self.remove_curr();
        let _ = self.pick_next();
        Ok(left)
    }

    /// As [`RunQueue::update_curr`]: a running reservation's budget is charged.
    pub fn update_curr(&mut self, delta_ns: u64) -> bool {
        let Some(entry) = self.running_entry_mut() else {
            return self.fair.update_curr(delta_ns);
        };
        let ended = entry.left_ns <= delta_ns;
        entry.left_ns = entry.left_ns.saturating_sub(delta_ns);
        if ended && !entry.throttled && !entry.leaving {
            entry.throttled = true;
            self.throttles += 1;
        }
        ended
    }

    /// As [`RunQueue::yield_curr`]: a reservation gives up its budget until its next period.
    pub fn yield_curr(&mut self) {
        let Some(entry) = self.running_entry_mut() else {
            self.fair.yield_curr();
            return;
        };
        entry.left_ns = 0;
        if !entry.throttled {
            entry.throttled = true;
            self.throttles += 1;
        }
    }

    /// As [`RunQueue::decision_in_ns`]: also a running budget's end, and the next refill.
    #[must_use]
    pub fn decision_in_ns(&self) -> Option<u64> {
        let refill = self
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.throttled)
            .map(|entry| entry.refill_at.saturating_sub(self.now))
            .min();
        let here = match self.running_entry() {
            Some(entry) => Some(entry.left_ns),
            None => self.fair.decision_in_ns(),
        };
        match (refill, here) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// As [`RunQueue::should_preempt`]: a reservation out of budget gives way, and any with
    /// budget and an earlier deadline takes the processor.
    #[must_use]
    pub fn should_preempt(&self) -> bool {
        let earliest = self
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.ready.is_some() && !entry.throttled)
            .map(|entry| entry.deadline_at)
            .min();
        match self.running_entry() {
            Some(running) => {
                running.throttled || earliest.is_some_and(|at| at < running.deadline_at)
            }
            None => earliest.is_some() || self.fair.should_preempt(),
        }
    }

    /// As [`RunQueue::remaining_ns`].
    #[must_use]
    pub fn remaining_ns(&self) -> Option<u64> {
        match self.running_entry() {
            Some(entry) => Some(entry.left_ns),
            None => self.fair.remaining_ns(),
        }
    }

    /// As [`RunQueue::remove_curr`].
    pub fn remove_curr(&mut self) -> Option<Left<T>> {
        let Some(at) = self.running else {
            return self.fair.remove_curr();
        };
        self.running = None;
        let entry = self.entries.get_mut(at).and_then(Option::as_mut)?;
        let id = entry.id;
        let left = entry
            .running
            .take()
            .map(|(payload, state, slot)| (id, payload, state, slot));
        self.settle_leaving();
        left
    }

    /// As [`RunQueue::remove`]: a reservation that is taken off this queue stays admitted
    /// here, and until it comes back is the fair class's wherever it goes.
    pub fn remove(&mut self, id: u64) -> Option<(T, EntityState, Slot<T>)> {
        if let Some(entry) = self
            .entry_of(id)
            .and_then(|at| self.entries.get_mut(at))
            .and_then(Option::as_mut)
        {
            return entry.ready.take();
        }
        self.fair.remove(id)
    }

    /// As [`RunQueue::current`].
    #[must_use]
    pub fn current(&self) -> Option<&T> {
        match self.running_entry() {
            Some(entry) => entry.running.as_ref().map(|(payload, _, _)| payload),
            None => self.fair.current(),
        }
    }

    /// As [`RunQueue::current_id`].
    #[must_use]
    pub fn current_id(&self) -> Option<u64> {
        match self.running_entry() {
            Some(entry) => Some(entry.id),
            None => self.fair.current_id(),
        }
    }

    /// As [`RunQueue::len`], the reservations queued here counted.
    #[must_use]
    pub fn len(&self) -> usize {
        let held = self
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.ready.is_some() || entry.running.is_some())
            .count();
        self.fair.len() + held
    }

    /// As [`RunQueue::is_empty`], a throttled reservation counted: it is on the queue.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// As [`RunQueue::check_invariants`], and the shares' sum.
    ///
    /// # Errors
    ///
    /// The invariant that failed.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        self.fair.check_invariants()?;
        let shares: u64 = self
            .entries
            .iter()
            .flatten()
            .map(|entry| entry.share_ppm)
            .sum();
        if shares != self.used_ppm {
            return Err("edf: the shares' sum is not the entries'");
        }
        if self.used_ppm > LIMIT_PPM {
            return Err("edf: more admitted than the limit");
        }
        if self.running.is_some() && self.fair.current().is_some() {
            return Err("edf: both classes hold a running task");
        }
        Ok(())
    }

    /// The fair class's own answers, which a reservation is no part of.
    #[must_use]
    pub fn avg_vruntime(&self) -> u64 {
        self.fair.avg_vruntime()
    }
    /// As [`RunQueue::for_each`], the fair class's alone.
    pub fn for_each(&self, visit: impl FnMut(EntityView<'_, T>)) {
        self.fair.for_each(visit);
    }
    /// As [`RunQueue::set_weight`].
    ///
    /// # Errors
    ///
    /// As [`RunQueue::set_weight`].
    pub fn set_weight(&mut self, id: u64, weight: u32) -> Result<bool, SchedError> {
        self.fair.set_weight(id, weight)
    }
    /// As [`RunQueue::get`].
    #[must_use]
    pub fn get(&self, id: u64) -> Option<EntityView<'_, T>> {
        self.fair.get(id)
    }
    /// As [`RunQueue::slice_ns`].
    #[must_use]
    pub fn slice_ns(&self) -> u64 {
        self.fair.slice_ns()
    }
    /// As [`RunQueue::set_request`].
    pub fn set_request(&mut self, id: u64, request_ns: u64) -> bool {
        self.fair.set_request(id, request_ns)
    }
    /// As [`RunQueue::load_weight`].
    #[must_use]
    pub fn load_weight(&self) -> u64 {
        self.fair.load_weight()
    }
    /// As [`RunQueue::latest_where`], the fair class's alone: nothing moves a reservation.
    #[must_use]
    pub fn latest_where(&self, movable: impl Fn(&T) -> bool) -> Option<u64> {
        self.fair.latest_where(movable)
    }
    /// As [`RunQueue::set_slice_ns`].
    ///
    /// # Errors
    ///
    /// As [`RunQueue::set_slice_ns`].
    pub fn set_slice_ns(&mut self, slice_ns: u64) -> Result<(), SchedError> {
        self.fair.set_slice_ns(slice_ns)
    }
    /// As [`RunQueue::config`].
    #[must_use]
    pub fn config(&self) -> Config {
        self.fair.config()
    }
    /// As [`RunQueue::queued`], the reservations ready here counted.
    #[must_use]
    pub fn queued(&self) -> usize {
        let ready = self
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.ready.is_some())
            .count();
        self.fair.queued() + ready
    }
    /// As [`RunQueue::level`].
    pub fn level(&mut self) {
        self.fair.level();
    }
}

#[cfg(test)]
mod tests;
