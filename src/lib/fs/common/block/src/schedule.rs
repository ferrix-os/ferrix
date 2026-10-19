//! Choosing the next unit of an epoch: deadline first, then the elevator.
//!
//! This is the only function in the crate that chooses among units the
//! barrier rule leaves free. Barrier order has already been settled by the
//! time it runs: the queue only calls it on the oldest epoch, and only
//! reaches for that epoch's barrier once it returns nothing.
//!
//! # Where stage 13 plugs in
//!
//! Per-cgroup bandwidth belongs here and nowhere else. Each [`Part`] will
//! carry the group that submitted it, and a group that has spent its budget
//! for the current period has its units skipped by both rules below, the way
//! a throttled group's I/O waits in Linux's `blk-throttle`. The epochs, the
//! merging and the completion path do not change; neither does barrier order,
//! which a throttled group must not be able to violate by holding back a
//! request older than someone else's flush — so a throttled unit in the
//! oldest epoch still blocks that epoch's barrier, and the throttle is lifted
//! for it rather than for the barrier.
//!
//! [`Part`]: crate::Part

use crate::epoch::Epoch;

/// The key of the unit in `epoch` to dispatch next, or `None` if the epoch
/// holds no queued unit.
///
/// A unit whose deadline is at or before `now` wins, the most overdue first,
/// ties going to the older unit. Otherwise the unit with the lowest start
/// sector at or after `head` wins, and if none starts there, the lowest start
/// sector of all: a one-way elevator that wraps once.
pub(crate) fn pick(epoch: &Epoch, head: u64, now: u64) -> Option<u64> {
    if let Some((deadline, key)) = epoch.earliest_deadline()
        && deadline <= now
    {
        return Some(key);
    }
    epoch
        .first_at_or_after(head)
        .or_else(|| epoch.first_at_or_after(0))
}
