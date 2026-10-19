//! A reference model of the queue's rules, and a harness that holds a real
//! [`Queue`] to it over an operation sequence decoded from bytes.
//!
//! The unit tests feed it bytes from a seeded generator; the `block_queue`
//! fuzz target feeds it a fuzzer's. Both call [`run`], so a sequence that
//! breaks a rule in one reproduces in the other.
//!
//! # What the model is
//!
//! Not a second queue. It keeps no units, no epochs and no elevator — only
//! the requests outstanding, in submission order, and which command each is
//! on. Every rule is stated over that flat list, the way the crate
//! documentation states it, so the model agrees with the queue only if the
//! queue's structures really implement the rules rather than merely agreeing
//! with themselves.
//!
//! # The rules checked
//!
//! * **Validation.** `submit` refuses exactly the requests the model says it
//!   must, with the same error.
//! * **Exactly once.** Every accepted request completes once, in the
//!   completion of the command that carried it, with that command's result;
//!   after the final drain nothing is outstanding.
//! * **Shape.** A dispatched unit's parts are outstanding requests not already
//!   on the device, of the unit's op and FUA-ness, listed in submission order,
//!   whose ranges tile the unit's range exactly. A flush is alone and empty.
//! * **Limits.** No unit exceeds the sector or part limit or the capacity, and
//!   no dispatch exceeds the queue depth.
//! * **Barrier order.** A barrier leaves only when everything submitted before
//!   it has completed and nothing else is on the device; ordinary work leaves
//!   only when every earlier barrier has completed and no later one has
//!   started.
//! * **Expiry.** When a request eligible in the oldest epoch is overdue, the
//!   unit dispatched is at least as overdue as it.
//! * **Liveness.** `dispatch` returns nothing only when plugging, the depth or
//!   barrier order justifies it.
//! * **Tokens.** A finished token is refused as finished, one never issued as
//!   never issued, and neither disturbs the queue.
//!
//! The elevator's order is not checked here: which unit is lowest from the
//! head depends on how requests merged, which the model deliberately does not
//! reconstruct. The unit tests pin it down instead.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::ops::Bound;

use crate::{Config, Limits, Op, Part, Queue, Request, RequestId, SubmitError, Token, TokenError};

/// A rule the queue broke, and the step of the sequence at which it did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Violation {
    /// The rule, in words.
    pub rule: &'static str,
    /// The number of operations applied before it, counting from one.
    pub step: usize,
}

/// What a sequence exercised, so a test can require it reached the paths it
/// meant to.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Stats {
    /// Requests accepted.
    pub submitted: usize,
    /// Requests refused.
    pub rejected: usize,
    /// Commands dispatched.
    pub dispatched: usize,
    /// Commands dispatched carrying more than one request.
    pub merged: usize,
    /// Barrier commands dispatched.
    pub barriers: usize,
    /// Commands put back.
    pub requeued: usize,
    /// Requests completed.
    pub completed: usize,
    /// Stale or invented tokens refused.
    pub refused_tokens: usize,
    /// Commands dispatched ahead of the elevator because they were overdue.
    pub expired: usize,
}

/// The fuzzer's or generator's bytes, handed out one at a time. Exhausted
/// input reads as zeros.
#[derive(Debug)]
struct Bytes<'a> {
    rest: &'a [u8],
}

impl Bytes<'_> {
    fn byte(&mut self) -> u8 {
        match self.rest.split_first() {
            Some((&first, tail)) => {
                self.rest = tail;
                first
            }
            None => 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }
}

/// Decode a device and a tuning from the input, apply the operations the rest
/// of it describes, then unplug and drain. Returns what was exercised, or the
/// first rule broken.
///
/// Limits and tuning are kept small — a capacity of a few hundred sectors,
/// commands of up to sixteen, depths of up to four, expiries of a few ticks —
/// so that requests collide, merge, overrun limits and expire constantly.
pub fn run(data: &[u8]) -> Result<Stats, Violation> {
    let mut bytes = Bytes { rest: data };
    let mut harness = Harness::new(&mut bytes)?;
    let mut steps = 0_usize;
    while !bytes.is_empty() {
        steps = steps.saturating_add(1);
        harness.step = steps;
        harness.apply(&mut bytes)?;
    }
    harness.drain()?;
    Ok(harness.stats)
}

/// One outstanding request as the model knows it.
#[derive(Clone, Copy, Debug)]
struct Tracked {
    request: Request,
    deadline: u64,
    /// The command it is on, if any.
    token: Option<u64>,
}

/// One command on the device as the model knows it.
#[derive(Clone, Debug)]
struct Flight {
    op: Op,
    sector: u64,
    count: u32,
    /// Its requests' sequence numbers, ascending.
    seqs: Vec<u64>,
}

/// A dispatch, copied out of the queue's borrow.
#[derive(Clone, Debug)]
struct Seen {
    token: u64,
    op: Op,
    sector: u64,
    count: u32,
    fua: bool,
    parts: Vec<Part>,
}

#[derive(Debug)]
struct Harness {
    queue: Queue,
    limits: Limits,
    config: Config,
    /// Outstanding requests by sequence number.
    live: BTreeMap<u64, Tracked>,
    ids: BTreeMap<RequestId, u64>,
    flights: BTreeMap<u64, Flight>,
    /// Sequence numbers of barrier requests on the device or completed.
    started_barriers: BTreeSet<u64>,
    finished_token: Option<u64>,
    plugged: bool,
    now: u64,
    next_seq: u64,
    next_id: u64,
    /// The range of the last accepted request, for aiming the next one at it.
    last: (u64, u64),
    step: usize,
    stats: Stats,
}

impl Harness {
    fn new(bytes: &mut Bytes<'_>) -> Result<Self, Violation> {
        let limits = Limits::new(
            512 << (bytes.byte() % 4),
            16 + u64::from(bytes.byte()),
            1 + u32::from(bytes.byte() % 16),
            1 + u32::from(bytes.byte() % 8),
            1 + u32::from(bytes.byte() % 4),
        )
        .map_err(|_| Violation {
            rule: "limits the harness built were refused",
            step: 0,
        })?;
        let config = Config {
            read_expiry: u64::from(bytes.byte() % 16),
            write_expiry: u64::from(bytes.byte() % 64),
            plug_threshold: usize::from(bytes.byte() % 8),
            max_requests: 1 + usize::from(bytes.byte() % 32),
        };
        Ok(Harness {
            queue: Queue::new(limits, config),
            limits,
            config,
            live: BTreeMap::new(),
            ids: BTreeMap::new(),
            flights: BTreeMap::new(),
            started_barriers: BTreeSet::new(),
            finished_token: None,
            plugged: false,
            now: 0,
            next_seq: 0,
            next_id: 0,
            last: (0, 0),
            step: 0,
            stats: Stats::default(),
        })
    }

    const fn fail<T>(&self, rule: &'static str) -> Result<T, Violation> {
        Err(Violation {
            rule,
            step: self.step,
        })
    }

    fn ensure(&self, holds: bool, rule: &'static str) -> Result<(), Violation> {
        if holds { Ok(()) } else { self.fail(rule) }
    }

    fn apply(&mut self, bytes: &mut Bytes<'_>) -> Result<(), Violation> {
        match bytes.byte() % 16 {
            0..=5 => self.submit(bytes)?,
            6..=8 => {
                let _ = self.dispatch()?;
            }
            9 | 10 => self.complete_nth(bytes.byte())?,
            11 => self.requeue_nth(bytes.byte())?,
            12 => self.stale(bytes.byte())?,
            13 => {
                self.queue.plug();
                self.plugged = true;
            }
            14 => {
                self.queue.unplug();
                self.plugged = false;
            }
            _ => self.now = self.now.saturating_add(u64::from(bytes.byte() % 32)),
        }
        self.check_counts()
    }

    fn check_counts(&self) -> Result<(), Violation> {
        self.ensure(
            self.queue.outstanding() == self.live.len(),
            "outstanding requests disagree with the model",
        )?;
        self.ensure(
            self.queue.in_flight() == self.flights.len(),
            "commands on the device disagree with the model",
        )?;
        self.ensure(
            self.queue.queued() == self.queued(),
            "queued requests disagree with the model",
        )?;
        self.ensure(
            self.queue.is_plugged() == self.plugged,
            "plugging disagrees with the model",
        )
    }

    fn queued(&self) -> usize {
        self.live.values().filter(|t| t.token.is_none()).count()
    }

    // -- Submission ---------------------------------------------------------

    fn submit(&mut self, bytes: &mut Bytes<'_>) -> Result<(), Violation> {
        let request = self.request(bytes);
        let expected = self.expect(&request);
        let actual = self.queue.submit(self.now, request);
        self.ensure(
            actual == expected,
            "submit's verdict disagrees with the model",
        )?;
        if actual.is_err() {
            self.stats.rejected = self.stats.rejected.saturating_add(1);
            return Ok(());
        }
        let expiry = if request.op == Op::Read || request.flags.sync {
            self.config.read_expiry
        } else {
            self.config.write_expiry
        };
        let tracked = Tracked {
            request,
            deadline: self.now.saturating_add(expiry),
            token: None,
        };
        let _ = self.live.insert(self.next_seq, tracked);
        let _ = self.ids.insert(request.id, self.next_seq);
        self.next_seq = self.next_seq.saturating_add(1);
        self.last = (
            request.sector,
            request.sector.saturating_add(u64::from(request.count)),
        );
        self.stats.submitted = self.stats.submitted.saturating_add(1);
        Ok(())
    }

    /// A request aimed, most of the time, to abut the last one accepted, and
    /// now and then malformed.
    fn request(&mut self, bytes: &mut Bytes<'_>) -> Request {
        let (shape, place, size, spot) = (bytes.byte(), bytes.byte(), bytes.byte(), bytes.byte());
        let op = match shape & 3 {
            0 => Op::Read,
            1 => Op::Write,
            2 => Op::Flush,
            _ => Op::Discard,
        };
        let odd = shape & 0x10 != 0;
        let count = u32::from(size % 9);
        let sector = match place % 4 {
            0 => self.last.1,
            1 => self.last.0.saturating_sub(u64::from(count)),
            2 => u64::from(spot),
            _ if odd => u64::MAX - u64::from(spot % 4),
            _ => u64::from(spot),
        };
        let (sector, count) = if op == Op::Flush && !odd {
            (0, 0)
        } else {
            (sector, count)
        };
        let id = self.pick_id(shape & 0x20 != 0, spot);
        let mut request = Request {
            id,
            op,
            sector,
            count,
            flags: crate::Flags::default(),
        };
        request.flags.sync = shape & 4 != 0;
        request.flags.fua = shape & 8 != 0 && (op == Op::Write || odd);
        request
    }

    /// A fresh id, or when asked and possible, one already outstanding.
    fn pick_id(&mut self, reuse: bool, spot: u8) -> RequestId {
        if reuse && !self.ids.is_empty() {
            let index = usize::from(spot) % self.ids.len();
            if let Some(&id) = self.ids.keys().nth(index) {
                return id;
            }
        }
        self.next_id = self.next_id.saturating_add(1);
        RequestId(self.next_id)
    }

    /// The verdict the rules require for `request`.
    fn expect(&self, request: &Request) -> Result<(), SubmitError> {
        if request.flags.fua && request.op != Op::Write {
            return Err(SubmitError::FuaWithoutWrite);
        }
        if request.op == Op::Flush && (request.sector != 0 || request.count != 0) {
            return Err(SubmitError::FlushWithRange);
        }
        if request.op != Op::Flush {
            self.expect_range(request)?;
        }
        if self.ids.contains_key(&request.id) {
            return Err(SubmitError::DuplicateId);
        }
        if self.live.len() >= self.config.max_requests {
            return Err(SubmitError::Full);
        }
        Ok(())
    }

    fn expect_range(&self, request: &Request) -> Result<(), SubmitError> {
        if request.count == 0 {
            return Err(SubmitError::Empty);
        }
        let Some(end) = request.sector.checked_add(u64::from(request.count)) else {
            return Err(SubmitError::Overflow);
        };
        if end > self.limits.capacity() {
            return Err(SubmitError::PastCapacity);
        }
        if request.count > self.limits.max_sectors() {
            return Err(SubmitError::TooLarge);
        }
        Ok(())
    }

    // -- Dispatch -----------------------------------------------------------

    /// Dispatch once and check what came out. Returns whether anything did.
    fn dispatch(&mut self) -> Result<bool, Violation> {
        let eligible = self.may_dispatch();
        let seen = self.queue.dispatch(self.now).map(|d| Seen {
            token: d.token.raw(),
            op: d.op,
            sector: d.sector,
            count: d.count,
            fua: d.fua,
            parts: d.parts.to_vec(),
        });
        let Some(seen) = seen else {
            self.ensure(
                !eligible,
                "dispatch returned nothing while work was eligible",
            )?;
            return Ok(false);
        };
        self.ensure(
            eligible,
            "dispatch returned a unit while nothing was eligible",
        )?;
        let found = self.known_parts(&seen)?;
        self.check_shape(&seen, &found)?;
        self.check_limits(&seen)?;
        self.check_barrier_order(&seen, &found)?;
        self.check_expiry(&seen, &found)?;
        self.start(&seen, &found);
        Ok(true)
    }

    /// Whether plugging, the depth and barrier order leave anything to send.
    fn may_dispatch(&self) -> bool {
        if self.plugged && self.queued() < self.config.plug_threshold {
            return false;
        }
        let depth = usize::try_from(self.limits.queue_depth()).unwrap_or(usize::MAX);
        if self.flights.len() >= depth {
            return false;
        }
        let first_barrier = self.live.iter().find(|(_, t)| t.request.is_barrier());
        let Some((&barrier_seq, barrier)) = first_barrier else {
            return self.live.values().any(|t| t.token.is_none());
        };
        let before = self.live.range(..barrier_seq);
        let ordinary_ready = before.clone().any(|(_, t)| t.token.is_none());
        let barrier_ready = barrier.token.is_none() && before.clone().next().is_none();
        ordinary_ready || barrier_ready
    }

    /// The dispatched parts' model entries, by ascending sequence number.
    fn known_parts(&self, seen: &Seen) -> Result<Vec<(u64, Tracked)>, Violation> {
        let mut found: Vec<(u64, Tracked)> = Vec::new();
        for part in &seen.parts {
            let Some(&seq) = self.ids.get(&part.id) else {
                return self.fail("a dispatched part is not an outstanding request");
            };
            let Some(tracked) = self.live.get(&seq) else {
                return self.fail("a dispatched part is not an outstanding request");
            };
            let r = &tracked.request;
            self.ensure(
                tracked.token.is_none(),
                "a request was dispatched twice at once",
            )?;
            self.ensure(
                r.sector == part.sector && r.count == part.count && r.flags == part.flags,
                "a part does not describe its request",
            )?;
            self.ensure(r.op == seen.op, "a unit mixes ops")?;
            self.ensure(r.flags.fua == seen.fua, "a merge mixed FUA and non-FUA")?;
            self.ensure(
                found.last().is_none_or(|&(prev, _)| prev < seq),
                "parts are not in submission order",
            )?;
            found.push((seq, *tracked));
        }
        self.ensure(!found.is_empty(), "a unit has no parts")?;
        Ok(found)
    }

    fn check_shape(&self, seen: &Seen, found: &[(u64, Tracked)]) -> Result<(), Violation> {
        if seen.op == Op::Flush {
            return self.ensure(
                found.len() == 1 && seen.sector == 0 && seen.count == 0,
                "a flush merged or carried a range",
            );
        }
        let mut ranges: Vec<(u64, u32)> = found
            .iter()
            .map(|(_, t)| (t.request.sector, t.request.count))
            .collect();
        ranges.sort_unstable();
        let mut at = seen.sector;
        for (sector, count) in ranges {
            self.ensure(sector == at, "a unit's parts do not tile its range")?;
            at = sector.saturating_add(u64::from(count));
        }
        self.ensure(
            at == seen.sector.saturating_add(u64::from(seen.count)),
            "a unit's count is not the sum of its parts'",
        )
    }

    fn check_limits(&self, seen: &Seen) -> Result<(), Violation> {
        self.ensure(
            seen.count <= self.limits.max_sectors(),
            "a unit exceeds the sector limit",
        )?;
        let parts = u64::try_from(seen.parts.len()).unwrap_or(u64::MAX);
        self.ensure(
            parts <= u64::from(self.limits.max_parts()),
            "a unit exceeds the part limit",
        )?;
        let end = seen.sector.checked_add(u64::from(seen.count));
        self.ensure(
            end.is_some_and(|end| end <= self.limits.capacity()),
            "a unit runs past the device",
        )
    }

    fn check_barrier_order(&self, seen: &Seen, found: &[(u64, Tracked)]) -> Result<(), Violation> {
        let barrier = seen.op == Op::Flush || (seen.op == Op::Write && seen.fua);
        self.ensure(
            found.iter().all(|(_, t)| t.request.is_barrier() == barrier),
            "a unit mixes barriers and ordinary requests",
        )?;
        let (Some(&(first, _)), Some(&(last, _))) = (found.first(), found.last()) else {
            return Ok(());
        };
        let later = (Bound::Excluded(first), Bound::Unbounded);
        self.ensure(
            self.started_barriers.range(later).next().is_none(),
            "barrier order: a request left after a later barrier started",
        )?;
        if barrier {
            let is_part = |seq: &u64| found.binary_search_by_key(seq, |&(s, _)| s).is_ok();
            self.ensure(
                self.live.range(..last).all(|(seq, _)| is_part(seq)),
                "barrier order: a barrier left before everything ahead of it completed",
            )?;
            self.ensure(
                self.flights.is_empty(),
                "barrier order: a barrier shared the device",
            )
        } else {
            self.ensure(
                !self.live.range(..last).any(|(_, t)| t.request.is_barrier()),
                "barrier order: a request left before an earlier barrier completed",
            )
        }
    }

    fn check_expiry(&mut self, seen: &Seen, found: &[(u64, Tracked)]) -> Result<(), Violation> {
        if seen.op == Op::Flush || seen.fua {
            return Ok(());
        }
        let deadline = found
            .iter()
            .map(|(_, t)| t.deadline)
            .min()
            .unwrap_or(u64::MAX);
        let horizon = self
            .live
            .iter()
            .find(|(_, t)| t.request.is_barrier())
            .map_or(u64::MAX, |(&seq, _)| seq);
        let is_part = |seq: &u64| found.binary_search_by_key(seq, |&(s, _)| s).is_ok();
        let rest = self
            .live
            .range(..horizon)
            .filter(|(seq, t)| t.token.is_none() && !is_part(seq))
            .map(|(_, t)| t.deadline)
            .min();
        if deadline <= self.now {
            self.stats.expired = self.stats.expired.saturating_add(1);
        }
        match rest {
            Some(rest) if rest <= self.now => self.ensure(
                deadline <= rest,
                "expiry: an overdue unit was passed over by a less overdue one",
            ),
            _ => Ok(()),
        }
    }

    fn start(&mut self, seen: &Seen, found: &[(u64, Tracked)]) {
        let seqs: Vec<u64> = found.iter().map(|&(seq, _)| seq).collect();
        let barrier = found.iter().any(|(_, t)| t.request.is_barrier());
        for seq in &seqs {
            if let Some(tracked) = self.live.get_mut(seq) {
                tracked.token = Some(seen.token);
            }
            if barrier {
                let _ = self.started_barriers.insert(*seq);
            }
        }
        let flight = Flight {
            op: seen.op,
            sector: seen.sector,
            count: seen.count,
            seqs,
        };
        let _ = self.flights.insert(seen.token, flight);
        let stats = &mut self.stats;
        stats.dispatched = stats.dispatched.saturating_add(1);
        if seen.parts.len() > 1 {
            stats.merged = stats.merged.saturating_add(1);
        }
        if barrier {
            stats.barriers = stats.barriers.saturating_add(1);
        }
    }

    // -- Completion ---------------------------------------------------------

    fn nth_flight(&self, choice: u8) -> Option<u64> {
        let count = self.flights.len();
        if count == 0 {
            return None;
        }
        self.flights
            .keys()
            .nth(usize::from(choice) % count)
            .copied()
    }

    fn complete_nth(&mut self, choice: u8) -> Result<(), Violation> {
        match self.nth_flight(choice) {
            Some(token) => self.complete(token),
            None => Ok(()),
        }
    }

    fn complete(&mut self, token: u64) -> Result<(), Violation> {
        let result = token.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let Ok(done) = self.queue.complete(Token::from_raw(token), result) else {
            return self.fail("completing a command on the device was refused");
        };
        let Some(flight) = self.flights.remove(&token) else {
            return self.fail("the model lost a command");
        };
        self.ensure(
            done.op == flight.op && done.sector == flight.sector && done.count == flight.count,
            "a completion describes a different command",
        )?;
        self.ensure(
            done.parts().len() == flight.seqs.len(),
            "a completion answers a different number of requests",
        )?;
        self.ensure(
            done.requests().all(|(_, r)| r == result),
            "a request completed with another command's result",
        )?;
        for (part, seq) in done.parts().iter().zip(&flight.seqs) {
            let Some(tracked) = self.live.remove(seq) else {
                return self.fail("a request completed twice");
            };
            self.ensure(
                tracked.request.id == part.id,
                "a completion names a request its command did not carry",
            )?;
            let _ = self.ids.remove(&part.id);
        }
        self.finished_token = Some(token);
        self.stats.completed = self.stats.completed.saturating_add(flight.seqs.len());
        Ok(())
    }

    fn requeue_nth(&mut self, choice: u8) -> Result<(), Violation> {
        let Some(token) = self.nth_flight(choice) else {
            return Ok(());
        };
        let Ok(count) = self.queue.requeue(Token::from_raw(token)) else {
            return self.fail("requeueing a command on the device was refused");
        };
        let Some(flight) = self.flights.remove(&token) else {
            return self.fail("the model lost a command");
        };
        self.ensure(
            count == flight.seqs.len(),
            "requeue put back a different number",
        )?;
        for seq in &flight.seqs {
            if let Some(tracked) = self.live.get_mut(seq) {
                tracked.token = None;
            }
            let _ = self.started_barriers.remove(seq);
        }
        self.finished_token = Some(token);
        self.stats.requeued = self.stats.requeued.saturating_add(1);
        Ok(())
    }

    /// Hand back a finished or an invented token, by completion or requeue.
    fn stale(&mut self, choice: u8) -> Result<(), Violation> {
        let (token, expected) = match self.finished_token {
            Some(token) if choice & 1 == 0 => (token, TokenError::AlreadyFinished),
            _ => (u64::MAX - u64::from(choice), TokenError::NeverIssued),
        };
        let verdict = if choice & 2 == 0 {
            self.queue
                .complete(Token::from_raw(token), 0_u8)
                .map(|_| ())
        } else {
            self.queue.requeue(Token::from_raw(token)).map(|_| ())
        };
        self.ensure(
            verdict == Err(expected),
            "a stale token was not refused as such",
        )?;
        self.stats.refused_tokens = self.stats.refused_tokens.saturating_add(1);
        Ok(())
    }

    // -- The end ------------------------------------------------------------

    /// Unplug, then dispatch everything eligible and complete everything on
    /// the device, until nothing is outstanding. Each round completes at least
    /// one request if liveness holds, so the rounds are bounded by the number
    /// outstanding.
    fn drain(&mut self) -> Result<(), Violation> {
        self.step = self.step.saturating_add(1);
        self.queue.unplug();
        self.plugged = false;
        let rounds = self.live.len().saturating_add(2);
        for _ in 0..rounds {
            if self.live.is_empty() {
                return self.check_counts();
            }
            for _ in 0..=self.live.len() {
                if !self.dispatch()? {
                    break;
                }
            }
            let tokens: Vec<u64> = self.flights.keys().copied().collect();
            for token in tokens {
                self.complete(token)?;
            }
            self.check_counts()?;
        }
        self.ensure(self.live.is_empty(), "the queue never finished its work")
    }
}
