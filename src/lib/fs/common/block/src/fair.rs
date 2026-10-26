//! Proportional shares of a disk: which waiting request goes next.
//!
//! `io.weight` divides a disk among cgroups by weight while they contend for
//! it, and `io.latency` serves first a cgroup that is missing its latency
//! target. [`Fair`] is the decision for one disk, as start-time fair queueing
//! makes it: each request is given a virtual start, the later of the disk's
//! virtual clock and the finish of its group's last request, and a virtual
//! finish, its start and its cost, which the caller has already divided by
//! the group's weight. One request is in flight at a time; the next is the
//! urgent one with the least finish if any is urgent, else the one with the
//! least finish, ties going to the one queued first. A group that keeps a
//! request waiting therefore gets the disk in proportion to its weight, and
//! a group that comes back after a pause starts at the disk's clock, not
//! where it left off, so it cannot claim the time it was away.
//!
//! A request is made by a task that waits for it, so a group often has one
//! request at a time, and the next only once the last is done. Served as it
//! comes, the disk would then go to whichever other group is waiting the
//! moment a request ends, and alternate between groups whatever their
//! weights. So, as BFQ idles, the disk may be held for the group just served
//! ([`Fair::done`]): until the hold ends, only that group's next request, or
//! an urgent one, may go. The caller holds it when that group would win
//! anyway -- its next finish below every waiting one, or it is urgent -- and
//! for a short while, so that a group that has stopped costs the disk no more
//! than that.
//!
//! Urgency is not starvation: after [`URGENT_RUN`] urgent requests in a row
//! while another waited, the waiting one with the least finish that is not
//! urgent goes next, hold or not, as
//! Linux's `blk-iolatency` cuts a sibling to one request in flight, never to
//! none.
//!
//! The caller keeps each group's last finish ([`Ticket::finish`]) and passes
//! it back with the group's next request; this keeps no per-group state.
//!
//! # Costs and the clock
//!
//! A request's cost is [`weighted_cost`]: its bytes scaled by the weights on
//! the way up its group's tree, rounded up at each level so that it is never
//! below the true value, and [`COST_CEILING`] once it passes it, never less
//! afterwards, so that a deep or light group never gains by its depth. The
//! virtual times are 128 bits: at the ceiling, 2^40, a disk serves 2^88
//! requests before its clock could saturate.

use alloc::collections::TryReserveError;
use alloc::vec::Vec;

/// The most a request costs, in virtual units: 2^40, a terabyte at the
/// default weight.
pub const COST_CEILING: u64 = 1 << 40;

/// The default weight, against which a weight scales a cost.
pub const WEIGHT_DEFAULT: u32 = 100;

/// Fraction bits of [`weighted_cost`]'s fixed point.
const FRACTION: u32 = 32;

/// What a request of `bytes` costs in a group whose weights are `weights`,
/// from its own up: its bytes, at least 1, times, at each level,
/// [`WEIGHT_DEFAULT`] over that weight taken as 1 to 10,000. Kept in fixed
/// point with 32 fraction bits and rounded up at each level, so it is never
/// below the true value; once a level's product passes [`COST_CEILING`] the
/// cost is the ceiling, whatever the levels above would divide it by. At least
/// 1.
#[must_use]
pub fn weighted_cost(bytes: u64, weights: impl IntoIterator<Item = u32>) -> u64 {
    let ceiling = u128::from(COST_CEILING) << FRACTION;
    let mut cost = u128::from(bytes.max(1)) << FRACTION;
    for weight in weights {
        let weight = u128::from(weight.clamp(1, 10_000));
        // At most 2^96 (the first level's bytes, shifted) times 100, below
        // 2^103: no overflow.
        cost = (cost * u128::from(WEIGHT_DEFAULT)).div_ceil(weight);
        if cost >= ceiling {
            return COST_CEILING;
        }
    }
    u64::try_from(cost.div_ceil(1 << FRACTION))
        .unwrap_or(COST_CEILING)
        .clamp(1, COST_CEILING)
}

/// Urgent requests that may go in a row while one that is not waits.
pub const URGENT_RUN: u32 = 7;

/// A request waiting for the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    /// Its number, in the order requests were queued.
    pub id: u64,
    /// The group it is of, as the caller names groups.
    pub group: u64,
    /// Its virtual start.
    pub start: u128,
    /// Its virtual finish: what its group's next request starts from.
    pub finish: u128,
    /// Whether it is served before any request that is not.
    pub urgent: bool,
}

/// One disk's queue of requests waiting their turn.
#[derive(Debug, Default)]
pub struct Fair {
    /// The virtual start of the request served last.
    clock: u128,
    /// Whether a request is in flight.
    busy: bool,
    /// The requests waiting.
    waiting: Vec<Ticket>,
    /// The next ticket's number.
    next_id: u64,
    /// The group the disk is held for, and until when.
    hold: Option<(u64, u64)>,
    /// Urgent requests that went in a row while one that was not waited.
    urgent_run: u32,
}

impl Fair {
    /// An idle disk with nothing waiting.
    #[must_use]
    pub const fn new() -> Fair {
        Fair {
            clock: 0,
            busy: false,
            waiting: Vec::new(),
            next_id: 0,
            hold: None,
            urgent_run: 0,
        }
    }

    /// Queue a request of `group`, whose last request finished at `last`,
    /// costing `cost` virtual units.
    ///
    /// # Errors
    ///
    /// When there is no memory to queue it; the caller then goes without a
    /// turn.
    pub fn enqueue(
        &mut self,
        group: u64,
        last: u128,
        cost: u64,
        urgent: bool,
    ) -> Result<Ticket, TryReserveError> {
        self.waiting.try_reserve(1)?;
        let start = self.clock.max(last);
        let ticket = Ticket {
            id: self.next_id,
            group,
            start,
            finish: start.saturating_add(u128::from(cost.clamp(1, COST_CEILING))),
            urgent,
        };
        self.next_id = self.next_id.wrapping_add(1);
        self.waiting.push(ticket);
        Ok(ticket)
    }

    /// The ticket that goes next at `now`, if any may: an urgent one with
    /// the least finish, else the one with the least finish, and while the
    /// disk is held, only one of the held group's or an urgent one. After
    /// [`URGENT_RUN`] urgent ones in a row, the least finish of those that are
    /// not urgent.
    #[must_use]
    pub fn next(&self, now: u64) -> Option<u64> {
        if self.urgent_run >= URGENT_RUN {
            return self
                .waiting
                .iter()
                .filter(|ticket| !ticket.urgent)
                .min_by_key(|ticket| (ticket.finish, ticket.id))
                .map(|ticket| ticket.id);
        }
        let held = self.held(now);
        self.waiting
            .iter()
            .filter(|ticket| held.is_none_or(|group| ticket.group == group || ticket.urgent))
            .min_by_key(|ticket| (!ticket.urgent, ticket.finish, ticket.id))
            .map(|ticket| ticket.id)
    }

    /// The group the disk is held for at `now`, if it is.
    fn held(&self, now: u64) -> Option<u64> {
        self.hold
            .filter(|&(_, until)| now < until)
            .map(|(group, _)| group)
    }

    /// When the hold at `now` ends, if there is one: a waiter it keeps out
    /// looks again then.
    #[must_use]
    pub fn held_until(&self, now: u64) -> Option<u64> {
        self.hold
            .filter(|&(_, until)| now < until)
            .map(|(_, until)| until)
    }

    /// Whether ticket `id` may go at `now`: nothing in flight, and it is
    /// next.
    #[must_use]
    pub fn may_go(&self, id: u64, now: u64) -> bool {
        !self.busy && self.next(now) == Some(id)
    }

    /// The least finish of the requests waiting, if any wait.
    #[must_use]
    pub fn least_finish(&self) -> Option<u128> {
        self.waiting.iter().map(|ticket| ticket.finish).min()
    }

    /// Ticket `id` goes: it leaves the queue, the disk is in flight, any
    /// hold ends, and the clock is its start. Whether it was waiting.
    pub fn go(&mut self, id: u64) -> bool {
        let Some(at) = self.waiting.iter().position(|ticket| ticket.id == id) else {
            return false;
        };
        let ticket = self.waiting.swap_remove(at);
        let passed = self.waiting.iter().any(|other| !other.urgent);
        self.urgent_run = if ticket.urgent && passed {
            self.urgent_run.saturating_add(1)
        } else {
            0
        };
        self.busy = true;
        self.hold = None;
        self.clock = self.clock.max(ticket.start);
        true
    }

    /// The request in flight is done; with `hold`, the disk is held for that
    /// group until that instant.
    pub fn done(&mut self, hold: Option<(u64, u64)>) {
        self.busy = false;
        self.hold = hold;
    }

    /// How many requests wait.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }
}
