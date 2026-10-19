//! An epoch: the requests between two barriers, and the barriers that end it.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::limits::Limits;
use crate::unit::Unit;

/// The queued units of one epoch, indexed three ways, and its barriers.
///
/// Units are keyed by a number the queue hands out in submission order, which
/// is also every index's tie-breaker, so two units starting at the same sector
/// or sharing a deadline leave in the order they were queued.
///
/// * `by_start` is the elevator's order and finds a front-merge partner.
/// * `by_end` finds a back-merge partner.
/// * `by_deadline` finds the most overdue unit.
///
/// Barriers are held apart from the units, in submission order, and leave one
/// at a time after every unit has completed.
#[derive(Debug, Default)]
pub(crate) struct Epoch {
    units: BTreeMap<u64, Unit>,
    by_start: BTreeSet<(u64, u64)>,
    by_end: BTreeSet<(u64, u64)>,
    by_deadline: BTreeSet<(u64, u64)>,
    /// Commands from this epoch on the device: units, or one barrier.
    pub(crate) in_flight: usize,
    /// Barriers not yet dispatched, oldest first.
    pub(crate) barriers: VecDeque<Unit>,
}

impl Epoch {
    /// No queued unit and nothing on the device. Barriers may remain.
    pub(crate) fn is_idle(&self) -> bool {
        self.units.is_empty() && self.in_flight == 0
    }

    /// Nothing left to do at all.
    pub(crate) fn is_done(&self) -> bool {
        self.is_idle() && self.barriers.is_empty()
    }

    /// Whether a unit is queued.
    pub(crate) fn has_units(&self) -> bool {
        !self.units.is_empty()
    }

    /// Queue `unit` under `key`, joining it with the unit that ends where it
    /// starts and then with the unit that starts where the result ends.
    ///
    /// The joined unit keeps the oldest key, so its place among equals in the
    /// elevator and in the deadline order is its oldest part's.
    pub(crate) fn add(&mut self, key: u64, unit: Unit, limits: &Limits) {
        let (mut key, mut unit) = (key, unit);
        if let Some((left_key, left)) = self.take_left_partner(&unit, limits) {
            unit = left.join(unit);
            key = key.min(left_key);
        }
        if let Some((right_key, right)) = self.take_right_partner(&unit, limits) {
            unit = unit.join(right);
            key = key.min(right_key);
        }
        self.insert(key, unit);
    }

    /// Queue `unit` under `key` without looking for a partner.
    pub(crate) fn insert(&mut self, key: u64, unit: Unit) {
        let _ = self.by_start.insert((unit.sector, key));
        let _ = self.by_end.insert((unit.end(), key));
        let _ = self.by_deadline.insert((unit.deadline, key));
        let _ = self.units.insert(key, unit);
    }

    /// Remove and return the unit under `key`.
    pub(crate) fn remove(&mut self, key: u64) -> Option<Unit> {
        let unit = self.units.remove(&key)?;
        let _ = self.by_start.remove(&(unit.sector, key));
        let _ = self.by_end.remove(&(unit.end(), key));
        let _ = self.by_deadline.remove(&(unit.deadline, key));
        Some(unit)
    }

    /// Append a barrier, joining it to the last queued barrier if both are
    /// FUA writes that abut. Nothing can lie between the two: a barrier is
    /// only appended here when no unit was submitted after the last one.
    pub(crate) fn push_barrier(&mut self, unit: Unit, limits: &Limits) {
        let Some(last) = self.barriers.pop_back() else {
            self.barriers.push_back(unit);
            return;
        };
        if last.can_join(&unit, limits) {
            self.barriers.push_back(last.join(unit));
        } else if unit.can_join(&last, limits) {
            self.barriers.push_back(unit.join(last));
        } else {
            self.barriers.push_back(last);
            self.barriers.push_back(unit);
        }
    }

    /// The earliest deadline among queued units, and its unit's key.
    pub(crate) fn earliest_deadline(&self) -> Option<(u64, u64)> {
        self.by_deadline.first().copied()
    }

    /// The key of the first unit, in elevator order, starting at or after
    /// `sector`.
    pub(crate) fn first_at_or_after(&self, sector: u64) -> Option<u64> {
        self.by_start
            .range((sector, 0)..)
            .next()
            .map(|&(_, key)| key)
    }

    /// Remove and return a unit that `unit` may be appended to.
    fn take_left_partner(&mut self, unit: &Unit, limits: &Limits) -> Option<(u64, Unit)> {
        let key = self
            .by_end
            .range((unit.sector, 0)..=(unit.sector, u64::MAX))
            .map(|&(_, key)| key)
            .find(|key| {
                self.units
                    .get(key)
                    .is_some_and(|left| left.can_join(unit, limits))
            })?;
        Some((key, self.remove(key)?))
    }

    /// Remove and return a unit that may be appended to `unit`.
    fn take_right_partner(&mut self, unit: &Unit, limits: &Limits) -> Option<(u64, Unit)> {
        let end = unit.end();
        let key = self
            .by_start
            .range((end, 0)..=(end, u64::MAX))
            .map(|&(_, key)| key)
            .find(|key| {
                self.units
                    .get(key)
                    .is_some_and(|right| unit.can_join(right, limits))
            })?;
        Some((key, self.remove(key)?))
    }
}
