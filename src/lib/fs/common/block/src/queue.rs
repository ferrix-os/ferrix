//! The request queue: submission, dispatch, completion and requeue.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use core::fmt;

use crate::epoch::Epoch;
use crate::limits::{Config, Limits};
use crate::request::{Op, Part, Request, RequestId};
use crate::schedule;
use crate::unit::Unit;

/// Why [`Queue::submit`] refused a request. Checked in the order listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// `fua` on something other than a write.
    FuaWithoutWrite,
    /// A flush with a non-zero sector or count.
    FlushWithRange,
    /// A read, write or discard of no sectors.
    Empty,
    /// The range's end does not fit in a `u64`.
    Overflow,
    /// The range ends past the device's capacity.
    PastCapacity,
    /// More sectors than one command may carry. The caller splits it.
    TooLarge,
    /// The id belongs to a request not yet completed.
    DuplicateId,
    /// [`Config::max_requests`] requests are already outstanding.
    Full,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SubmitError::FuaWithoutWrite => "fua on a request that is not a write",
            SubmitError::FlushWithRange => "a flush carries a range",
            SubmitError::Empty => "a request of no sectors",
            SubmitError::Overflow => "the range's end overflows",
            SubmitError::PastCapacity => "the range ends past the device",
            SubmitError::TooLarge => "more sectors than one command carries",
            SubmitError::DuplicateId => "the id is already outstanding",
            SubmitError::Full => "the queue is full",
        })
    }
}

/// Why a token was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenError {
    /// The queue has not issued this token.
    NeverIssued,
    /// The token was already completed or requeued.
    AlreadyFinished,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TokenError::NeverIssued => "the token was never issued",
            TokenError::AlreadyFinished => "the token was already completed or requeued",
        })
    }
}

/// Names one dispatched command until it is completed or requeued.
///
/// Tokens are issued in increasing order and never reused, which is what lets
/// the queue tell a token completed twice from one it never issued. The raw
/// value is for carrying through a descriptor ring and back.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Token(u64);

impl Token {
    /// The token a driver handed back, as the number it was carried as.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Token(raw)
    }

    /// The number to carry through a ring.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// One command for the device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dispatch<'a> {
    /// Hand this back to [`Queue::complete`] or [`Queue::requeue`].
    pub token: Token,
    /// What the command does.
    pub op: Op,
    /// The first sector. Zero for a flush.
    pub sector: u64,
    /// The sector count, the sum of the parts'. Zero for a flush.
    pub count: u32,
    /// Whether the command must be durable when it completes.
    pub fua: bool,
    /// The requests the command performs, in submission order. Their ranges
    /// tile `sector..sector + count` exactly.
    pub parts: &'a [Part],
}

/// A finished command: the result, and every request it answers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Completion<R> {
    /// What the command did.
    pub op: Op,
    /// Its first sector.
    pub sector: u64,
    /// Its sector count.
    pub count: u32,
    /// The result the driver reported.
    pub result: R,
    parts: Vec<Part>,
}

impl<R: Copy> Completion<R> {
    /// The requests completed, in submission order.
    #[must_use]
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Each request's id with the command's result.
    pub fn requests(&self) -> impl Iterator<Item = (RequestId, R)> + '_ {
        self.parts.iter().map(|part| (part.id, self.result))
    }
}

/// A command on the device.
#[derive(Debug)]
struct InFlight {
    /// Which epoch it came from.
    epoch: u64,
    /// Whether it was that epoch's barrier.
    barrier: bool,
    unit: Unit,
}

/// One device's request queue. See the crate documentation for the rules it
/// keeps.
#[derive(Debug)]
pub struct Queue {
    limits: Limits,
    config: Config,
    /// Oldest first. Never empty: the last is the open epoch new requests
    /// join, and every other one has at least one barrier to end it.
    epochs: VecDeque<Epoch>,
    /// The number of the epoch at the front of `epochs`.
    first_epoch: u64,
    in_flight: BTreeMap<u64, InFlight>,
    /// The ids of requests submitted and not yet completed.
    outstanding: BTreeSet<RequestId>,
    /// Requests queued and not on the device.
    queued: usize,
    /// The sector after the last dispatched range: the elevator's position.
    head: u64,
    plugged: bool,
    next_token: u64,
    next_key: u64,
    next_seq: u64,
}

impl Queue {
    /// An empty, unplugged queue for a device with `limits`.
    #[must_use]
    pub fn new(limits: Limits, config: Config) -> Self {
        let mut epochs = VecDeque::new();
        epochs.push_back(Epoch::default());
        Queue {
            limits,
            config,
            epochs,
            first_epoch: 0,
            in_flight: BTreeMap::new(),
            outstanding: BTreeSet::new(),
            queued: 0,
            head: 0,
            plugged: false,
            next_token: 0,
            next_key: 0,
            next_seq: 0,
        }
    }

    /// The device's limits.
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The queue's tuning.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Requests submitted and not yet completed, queued or on the device.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }

    /// Requests queued and not on the device.
    #[must_use]
    pub const fn queued(&self) -> usize {
        self.queued
    }

    /// Commands on the device.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether nothing is outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outstanding.is_empty()
    }

    /// Whether the queue is plugged.
    #[must_use]
    pub const fn is_plugged(&self) -> bool {
        self.plugged
    }

    /// Hold dispatch back so a burst of submissions can merge.
    ///
    /// Plugs do not nest: one `unplug` releases however many `plug`s came
    /// before it, as a task's plug does in Linux.
    pub fn plug(&mut self) {
        self.plugged = true;
    }

    /// Release a plug.
    pub fn unplug(&mut self) {
        self.plugged = false;
    }

    /// The parts of the command `token` names, while it is on the device.
    #[must_use]
    pub fn parts(&self, token: Token) -> Option<&[Part]> {
        self.in_flight
            .get(&token.0)
            .map(|flight| flight.unit.parts.as_slice())
    }

    /// Queue `request`, submitted at tick `now`, merging it where it may.
    pub fn submit(&mut self, now: u64, request: Request) -> Result<(), SubmitError> {
        self.check(&request)?;
        let part = Part {
            id: request.id,
            sector: request.sector,
            count: request.count,
            flags: request.flags,
            arrival: now,
            deadline: now.saturating_add(self.config.expiry(request.op, request.flags)),
            seq: self.next_seq,
        };
        // Sequence numbers and keys are never reused within 2^64 submissions,
        // which no device lives to see; wrapping keeps the counter total.
        self.next_seq = self.next_seq.wrapping_add(1);
        let _ = self.outstanding.insert(request.id);
        self.queued = self.queued.saturating_add(1);
        let unit = Unit::single(request.op, part, false);
        if request.is_barrier() {
            self.push_barrier(unit);
        } else {
            self.push_unit(unit);
        }
        Ok(())
    }

    /// The next command for the device at tick `now`, or `None`.
    ///
    /// `None` when nothing is queued; when plugged below the threshold; when
    /// [`Limits::queue_depth`] commands are on the device; or when barrier
    /// order holds everything queued back — the oldest epoch's units are all
    /// on the device and its barrier must wait for them, or a barrier is on
    /// the device and the next epoch must wait for it.
    pub fn dispatch(&mut self, now: u64) -> Option<Dispatch<'_>> {
        if self.plugged && self.queued < self.config.plug_threshold {
            return None;
        }
        let depth = usize::try_from(self.limits.queue_depth()).unwrap_or(usize::MAX);
        if self.in_flight.len() >= depth {
            return None;
        }
        let token = self.next_token;
        let next_token = token.checked_add(1)?;
        let (unit, barrier) = self.take_next(now)?;
        self.next_token = next_token;
        self.queued = self.queued.saturating_sub(unit.parts.len());
        if !matches!(unit.op, Op::Flush) {
            self.head = unit.end();
        }
        let flight = InFlight {
            epoch: self.first_epoch,
            barrier,
            unit,
        };
        let flight = match self.in_flight.entry(token) {
            alloc::collections::btree_map::Entry::Vacant(slot) => slot.insert(flight),
            alloc::collections::btree_map::Entry::Occupied(mut slot) => {
                // Tokens only increase, so this arm is never taken; replacing
                // keeps the function total without a panic.
                let _ = slot.insert(flight);
                slot.into_mut()
            }
        };
        Some(Dispatch {
            token: Token(token),
            op: flight.unit.op,
            sector: flight.unit.sector,
            count: flight.unit.count,
            fua: flight.unit.fua,
            parts: &flight.unit.parts,
        })
    }

    /// Finish the command `token` names with `result`, answering every
    /// request it carried.
    pub fn complete<R: Copy>(
        &mut self,
        token: Token,
        result: R,
    ) -> Result<Completion<R>, TokenError> {
        let flight = self.take_flight(token)?;
        for part in &flight.unit.parts {
            let _ = self.outstanding.remove(&part.id);
        }
        self.retire();
        let unit = flight.unit;
        Ok(Completion {
            op: unit.op,
            sector: unit.sector,
            count: unit.count,
            result,
            parts: unit.parts,
        })
    }

    /// Put the command `token` names back, one unit per request, never to be
    /// merged again. Returns how many requests went back.
    ///
    /// For a driver whose merged command failed: retried separately, each
    /// request's own result says whether its own sectors are bad. The parts
    /// keep their deadlines, so they are likely overdue and leave first, and
    /// they stay in their epoch, so barrier order is unchanged.
    pub fn requeue(&mut self, token: Token) -> Result<usize, TokenError> {
        let flight = self.take_flight(token)?;
        let count = flight.unit.parts.len();
        self.queued = self.queued.saturating_add(count);
        let op = flight.unit.op;
        let index = usize::try_from(flight.epoch.saturating_sub(self.first_epoch)).unwrap_or(0);
        let mut keys = self.next_key;
        let Some(epoch) = self.epochs.get_mut(index) else {
            return Ok(count);
        };
        if flight.barrier {
            for part in flight.unit.parts.into_iter().rev() {
                epoch.barriers.push_front(Unit::single(op, part, true));
            }
        } else {
            for part in flight.unit.parts {
                epoch.insert(keys, Unit::single(op, part, true));
                keys = keys.wrapping_add(1);
            }
        }
        self.next_key = keys;
        Ok(count)
    }

    fn check(&self, request: &Request) -> Result<(), SubmitError> {
        if request.flags.fua && request.op != Op::Write {
            return Err(SubmitError::FuaWithoutWrite);
        }
        if request.op == Op::Flush {
            if request.sector != 0 || request.count != 0 {
                return Err(SubmitError::FlushWithRange);
            }
        } else {
            self.check_range(request)?;
        }
        if self.outstanding.contains(&request.id) {
            return Err(SubmitError::DuplicateId);
        }
        if self.outstanding.len() >= self.config.max_requests {
            return Err(SubmitError::Full);
        }
        Ok(())
    }

    fn check_range(&self, request: &Request) -> Result<(), SubmitError> {
        if request.count == 0 {
            return Err(SubmitError::Empty);
        }
        let end = request
            .sector
            .checked_add(u64::from(request.count))
            .ok_or(SubmitError::Overflow)?;
        if end > self.limits.capacity() {
            return Err(SubmitError::PastCapacity);
        }
        if request.count > self.limits.max_sectors() {
            return Err(SubmitError::TooLarge);
        }
        Ok(())
    }

    /// Queue a non-barrier in the open epoch.
    fn push_unit(&mut self, unit: Unit) {
        let key = self.next_key;
        self.next_key = self.next_key.wrapping_add(1);
        if self.epochs.is_empty() {
            self.epochs.push_back(Epoch::default());
        }
        if let Some(open) = self.epochs.back_mut() {
            open.add(key, unit, &self.limits);
        }
    }

    /// Queue a barrier after everything submitted so far.
    ///
    /// If nothing was submitted since the last barrier, the open epoch is
    /// empty and the barrier follows the last one in that barrier's epoch;
    /// otherwise it closes the open epoch and a new one opens.
    fn push_barrier(&mut self, unit: Unit) {
        let open_is_idle = self.epochs.back().is_none_or(Epoch::is_idle);
        let closed = self.epochs.len().checked_sub(2);
        if open_is_idle
            && let Some(index) = closed
            && let Some(epoch) = self.epochs.get_mut(index)
        {
            epoch.push_barrier(unit, &self.limits);
            return;
        }
        if let Some(open) = self.epochs.back_mut() {
            open.push_barrier(unit, &self.limits);
        }
        self.epochs.push_back(Epoch::default());
    }

    /// Take the next unit of the oldest epoch, or its next barrier once
    /// nothing of the epoch is queued or on the device.
    fn take_next(&mut self, now: u64) -> Option<(Unit, bool)> {
        let head = self.head;
        let epoch = self.epochs.front_mut()?;
        let taken = if epoch.has_units() {
            let key = schedule::pick(epoch, head, now)?;
            (epoch.remove(key)?, false)
        } else if epoch.in_flight == 0 {
            (epoch.barriers.pop_front()?, true)
        } else {
            return None;
        };
        epoch.in_flight = epoch.in_flight.saturating_add(1);
        Some(taken)
    }

    fn take_flight(&mut self, token: Token) -> Result<InFlight, TokenError> {
        let Some(flight) = self.in_flight.remove(&token.0) else {
            return Err(if token.0 < self.next_token {
                TokenError::AlreadyFinished
            } else {
                TokenError::NeverIssued
            });
        };
        let index = usize::try_from(flight.epoch.saturating_sub(self.first_epoch)).unwrap_or(0);
        if let Some(epoch) = self.epochs.get_mut(index) {
            epoch.in_flight = epoch.in_flight.saturating_sub(1);
        }
        Ok(flight)
    }

    /// Drop finished epochs from the front, keeping the open one.
    fn retire(&mut self) {
        while self.epochs.len() > 1 && self.epochs.front().is_some_and(Epoch::is_done) {
            let _ = self.epochs.pop_front();
            self.first_epoch = self.first_epoch.wrapping_add(1);
        }
    }
}
