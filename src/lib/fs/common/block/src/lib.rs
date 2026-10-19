//! The block core: request queues, merging, write barriers and the I/O
//! scheduler that sit between filesystems and block drivers.
//!
//! Stage 11 of `docs/ROADMAP.md`. `docs/ARCHITECTURE.md` §8 puts the block
//! core in the kernel, and §7 puts the drivers below it in user processes that
//! share a descriptor ring with it. This crate is the part of that which
//! decides *what goes to the device next*: which requests become one command,
//! in what order commands leave, and what a filesystem may rely on about that
//! order. None of it needs the machine, so it lives here, where `cargo test`,
//! Miri and a fuzzer reach it.
//!
//! # Sector ranges and ids, not buffers
//!
//! A [`Request`] names a range of logical sectors and carries an id the caller
//! chose. It does not carry memory. The caller keeps the map from a
//! [`RequestId`] to the pages it reads into or writes from, and when a
//! [`Dispatch`] names several parts the caller builds the scatter list from
//! its own map. That keeps this crate out of DMA entirely — which pages a
//! device may address, and through which IOMMU domain, is stage 10's ring's
//! business — and it keeps every structure here a small, copyable record that
//! Miri and a fuzzer can churn through quickly.
//!
//! # Requests are validated once, on the way in
//!
//! [`Queue::submit`] checks a request against the device's [`Limits`] and
//! returns a [`SubmitError`] rather than queueing something no device could
//! perform: an empty read, write or discard, a range past the end of the disk
//! or one whose end overflows, a flush that carries a range, `fua` on anything
//! but a write, an id already in use, or more sectors than one command may
//! carry. That last one is refused rather than split. Splitting would make one
//! request's result the combination of several completions, and the caller —
//! a filesystem that already thinks in extents — is better placed to cut its
//! own I/O than this crate is to reassemble outcomes. Everything downstream of
//! `submit` may therefore assume a range is non-empty, in bounds and fits in a
//! command, which is what makes the rest total without re-checking.
//!
//! # Merging
//!
//! A disk is fastest when it is asked for one large contiguous range rather
//! than many small ones, and a filesystem naturally produces the small ones —
//! one block at a time, as the page cache fills. So a request joins a queued,
//! not yet dispatched unit of the same [`Op`] whose range ends exactly where
//! it starts (a *back merge*) or starts exactly where it ends (a *front
//! merge*); and if that closes a gap, the two units on either side join too.
//! A merge is refused when it would exceed [`Limits::max_sectors`] or
//! [`Limits::max_parts`], and a unit that was [requeued](Queue::requeue) never
//! merges again. Discards merge like writes. A flush has no range and never
//! merges. A merged unit keeps its constituent [`Part`]s in submission order,
//! each with its own sub-range, so completion can answer every caller.
//!
//! Only exact contiguity merges. Overlapping requests are left separate:
//! merging them would need the queue to know which bytes win, and that is the
//! data this crate deliberately does not hold.
//!
//! # Barriers
//!
//! A flush, and a write with `fua`, is a *barrier*. The rule, which stage 12's
//! crash consistency is built on and which the tests and the fuzz target name
//! as such:
//!
//! > **Barrier order.** Every request submitted before a barrier *completes*
//! > before the barrier is dispatched, and every request submitted after it is
//! > dispatched only once the barrier has *completed*. No merge joins requests
//! > on both sides of one.
//!
//! btrfs commits a transaction by writing new trees, flushing, and only then
//! writing the superblock that points at them. If the superblock could reach
//! the disk before the trees it names, a power cut between the two leaves a
//! superblock pointing at garbage, and every other property of a copy-on-write
//! filesystem is moot. A barrier is how the filesystem says "the order around
//! this point matters"; everywhere else the queue is free to reorder.
//!
//! The rule waits for *completion* on both sides, which is stronger than
//! ordering dispatch. A barrier that was merely dispatched after earlier work
//! could still land first on a device that runs several commands at once, and
//! a later write overlapping a FUA write that is still in flight could be
//! overwritten by it. Waiting costs one flush's latency per barrier, and a
//! filesystem committing a transaction waits for exactly that anyway.
//!
//! The requests between two barriers form an *epoch*. Only the oldest epoch's
//! work is ever on the device, which is what makes the invariant cheap to
//! state and to check. Consecutive barriers with nothing between them stay
//! serial, one on the device at a time; two contiguous FUA writes with nothing
//! between them may merge, since no request lies between them for the merge
//! to cross.
//!
//! # Scheduling within an epoch: a deadline elevator
//!
//! Inside an epoch the queue is free to choose, and it chooses as Linux's
//! `mq-deadline` does. Normally units leave in ascending sector order from
//! where the last dispatch ended, wrapping once to the lowest sector when
//! nothing lies ahead — a one-way elevator, which bounds seek distance on a
//! spinning disk and costs nothing on a solid-state one. But the elevator on
//! its own starves: a stream of writes just ahead of the head can hold a read
//! at the far end indefinitely. So every part is given a deadline when it is
//! submitted, and a unit whose earliest deadline has passed leaves before any
//! unit whose deadline has not, the most overdue first. Reads, and writes a
//! caller is waiting on (`sync`), get [`Config::read_expiry`]; asynchronous
//! writes and discards get the longer [`Config::write_expiry`]. Time is
//! whatever tick the caller counts and passes in as `now`, so nothing here
//! reads a clock.
//!
//! Per-cgroup bandwidth is stage 13's. It belongs in one function — the one
//! that picks the next unit of an epoch — and that function's documentation
//! says what it will become.
//!
//! # Plugging
//!
//! A caller about to submit a burst calls [`Queue::plug`], so that the burst
//! merges before any of it is dispatched, and [`Queue::unplug`] when it is
//! done. While plugged, [`Queue::dispatch`] returns nothing unless the queued
//! requests reach [`Config::plug_threshold`], so a caller that forgets to
//! unplug delays the device rather than stalling it.
//!
//! # Dispatch, completion and requeue
//!
//! [`Queue::dispatch`] returns the next [`Dispatch`] — an op, a range, a
//! [`Token`] and the parts — or nothing, when the queue is empty, plugged, at
//! [`Limits::queue_depth`], or when barrier order holds the next unit back.
//! [`Queue::complete`] takes the token and the caller's result and returns a
//! [`Completion`] naming every constituent request with that result. A driver
//! whose merged command failed can instead [`Queue::requeue`] it: the parts
//! go back as separate units that never merge again, keep their original
//! deadlines (so they are overdue and go first), and the next failure names
//! the one request whose sector is bad. A token completed twice, or never
//! issued, is a [`TokenError`].
//!
//! # Totality
//!
//! Nothing here panics, indexes, or loops without a bound. Every sector
//! calculation is checked, every lookup is a `get`, and every error is a value.
//! The `model` module — compiled for the tests and, with the `model` feature,
//! for `src/tests/fuzz/` — holds the queue to an independent statement of the rules
//! above over operation sequences nobody chose.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod epoch;
mod limits;
mod queue;
mod request;
mod schedule;
mod throttle;
mod unit;

#[cfg(any(test, feature = "model"))]
pub mod model;

#[cfg(test)]
mod tests;

pub use limits::{Config, Limits, LimitsError, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE};
pub use queue::{Completion, Dispatch, Queue, SubmitError, Token, TokenError};
pub use request::{Flags, Op, Part, Request, RequestId};
pub use throttle::{Limiter, Throttle};
