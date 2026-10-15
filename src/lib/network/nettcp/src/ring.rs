//! The two byte queues a connection holds, and the out-of-order pieces it
//! cannot put in one yet.
//!
//! A TCP connection is two queues and a set of counters. [`ByteQueue`] is the
//! queue: bytes in at one end, bytes out at the other, with a fixed capacity
//! that is what the connection advertises as its window. It is a
//! [`VecDeque`] with a ceiling and an argument for why the ceiling is there --
//! a socket buffer that grows without bound is a machine a stranger can fill.
//!
//! [`Reassembly`] holds what arrived early. A segment that begins past
//! `RCV.NXT` cannot be given to the program, because the bytes before it have
//! not come; it is kept here, trimmed against what is already held, and drained
//! into the receive queue as soon as the gap closes.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use ferrix_kmem::{Charge, Refused, buffer_footprint};

use crate::seq::SeqNumber;

/// A bounded first-in, first-out queue of bytes.
#[derive(Debug)]
pub struct ByteQueue {
    /// The bytes themselves, oldest first.
    bytes: VecDeque<u8>,
    /// The most bytes the queue will hold.
    capacity: usize,
    /// The heap `bytes` holds, charged to the job its connection is
    /// [`ByteQueue::charge_to`] as it grows (certification finding F-37).
    heap: Charge,
    /// Whether the last write took less than there was room for because its
    /// job could not be charged for the growth.
    refused: bool,
}

impl ByteQueue {
    /// An empty queue that will hold `capacity` bytes, charged to nobody
    /// until [`ByteQueue::charge_to`] says whom.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> ByteQueue {
        ByteQueue {
            bytes: VecDeque::new(),
            capacity,
            heap: Charge::none(),
            refused: false,
        }
    }

    /// Charge the queue's growth, and what it holds now, to `owner`: the
    /// job of the program that made the connection, or of the listener it
    /// arrived on.
    ///
    /// # Errors
    ///
    /// [`Refused`] past that job's limit, with the charge as it was.
    pub fn charge_to(&mut self, owner: u32) -> Result<(), Refused> {
        self.heap = Charge::to(owner, buffer_footprint::<u8>(self.bytes.capacity()))?;
        Ok(())
    }

    /// Whether the last write stopped short because the queue could not grow.
    #[must_use]
    pub fn refused(&self) -> bool {
        self.refused
    }

    /// Room for `wanted` more bytes, grown and charged to the next power of
    /// two up to the capacity; what fits in the room there is already when
    /// the growth is refused.
    fn room_for(&mut self, wanted: usize) -> usize {
        let (len, room) = (self.bytes.len(), self.bytes.capacity());
        let needed = len.saturating_add(wanted);
        if needed <= room {
            return wanted;
        }
        let target = needed.next_power_of_two().min(self.capacity.max(needed));
        if self.heap.resize(buffer_footprint::<u8>(target)).is_err() {
            return room.saturating_sub(len);
        }
        if self.bytes.try_reserve_exact(target - len).is_err() {
            let _ = self.heap.resize(buffer_footprint::<u8>(room));
            return room.saturating_sub(len);
        }
        let _ = self
            .heap
            .resize(buffer_footprint::<u8>(self.bytes.capacity()));
        wanted
    }

    /// How many bytes are queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// How many more bytes the queue will take.
    #[must_use]
    pub fn free(&self) -> usize {
        self.capacity.saturating_sub(self.bytes.len())
    }

    /// The ceiling this queue was built with.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Change the ceiling, keeping whatever is already queued.
    ///
    /// Shrinking below what is held does not discard anything: the queue simply
    /// reports no free space until enough has been taken out.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
    }

    /// Append as much of `data` as there is room for, and say how much that
    /// was.
    ///
    /// Less than there is room for when the queue's job cannot be charged
    /// for it to grow: [`ByteQueue::refused`] then says so.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let room = self.free();
        let wanted = data.len().min(room);
        let taken = self.room_for(wanted).min(wanted);
        self.refused = taken < wanted;
        self.bytes.extend(data.iter().take(taken).copied());
        taken
    }

    /// Copy out of the front without removing, starting `offset` bytes in.
    ///
    /// Answers how many bytes were copied, which is bounded by what is queued
    /// past `offset` and by the length of `out`.
    pub fn peek(&self, offset: usize, out: &mut [u8]) -> usize {
        let mut copied = 0;
        for (slot, byte) in out.iter_mut().zip(self.bytes.iter().skip(offset)) {
            *slot = *byte;
            copied += 1;
        }
        copied
    }

    /// Remove `count` bytes from the front, or everything if fewer are held.
    pub fn discard(&mut self, count: usize) {
        let count = count.min(self.bytes.len());
        let _ = self.bytes.drain(..count);
    }

    /// Take bytes from the front into `out`, and say how many were taken.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let copied = self.peek(0, out);
        self.discard(copied);
        copied
    }

    /// Throw everything away.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

/// One run of bytes that arrived before the bytes in front of it.
#[derive(Debug)]
struct Hole {
    /// The sequence number of the run's first byte.
    start: SeqNumber,
    /// The bytes themselves.
    bytes: Vec<u8>,
}

impl Hole {
    /// One past the run's last byte.
    fn end(&self) -> SeqNumber {
        self.start.advance(self.bytes.len() as u32)
    }
}

/// Segments held because the bytes in front of them have not arrived.
///
/// The list is kept sorted by sequence number and non-overlapping, so draining
/// it is a walk from the front and the SACK blocks it reports are the list
/// itself.
#[derive(Debug)]
pub struct Reassembly {
    /// The runs, in sequence order, none of them touching or overlapping.
    holes: Vec<Hole>,
    /// The most bytes the whole list will hold, each run's bookkeeping
    /// counted with its bytes.
    capacity: usize,
    /// How many bytes it holds now.
    held: usize,
    /// What it holds of the heap, charged to its connection's job
    /// (certification finding F-37).
    heap: Charge,
}

/// What a run costs beside its bytes: its entry in the list and the
/// smallest allocation its bytes take. Counted against the capacity, so a
/// peer sending one-byte runs with gaps between them fills the list at the
/// same heap as one sending whole segments.
pub(crate) const RUN_COST: usize = size_of::<Hole>() + 8;

/// The most runs the list holds, whatever its capacity. Each insert rebuilds
/// the list (`coalesce`), under the network stack's lock and with an
/// allocation that cannot fail gracefully, so its length is bounded here
/// rather than by the capacity: a 1 MiB receive buffer would otherwise let a
/// peer's gapped one-byte segments make some 25,000 runs and an 800 KB
/// rebuild per segment. 1,024 is below the 1,598 one-byte runs a 64 KiB
/// buffer allowed (certification consultant, 2026-10-15), and far above
/// what a path that reorders or loses segments makes.
pub const MAX_RUNS: usize = 1_024;

impl Reassembly {
    /// An empty list that will hold `capacity` bytes across all its runs.
    #[must_use]
    pub const fn with_capacity(capacity: usize) -> Reassembly {
        Reassembly {
            holes: Vec::new(),
            capacity,
            held: 0,
            heap: Charge::none(),
        }
    }

    /// Charge what it holds from now on to `owner`.
    ///
    /// # Errors
    ///
    /// [`Refused`] past that job's limit.
    pub fn charge_to(&mut self, owner: u32) -> Result<(), Refused> {
        self.heap = Charge::to(owner, self.cost(self.holes.len()))?;
        Ok(())
    }

    /// What `runs` runs and the bytes held cost against the capacity.
    fn cost(&self, runs: usize) -> usize {
        self.held.saturating_add(runs.saturating_mul(RUN_COST))
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.holes.is_empty()
    }

    /// How many bytes are held.
    #[must_use]
    pub const fn held(&self) -> usize {
        self.held
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.holes.clear();
        self.held = 0;
        self.heap.shrink(usize::MAX);
    }

    /// Change the ceiling on what may be held.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
    }

    /// How many runs it holds.
    #[must_use]
    pub fn runs(&self) -> usize {
        self.holes.len()
    }

    /// Remember `bytes`, which begin at `start`.
    ///
    /// Anything already held, and anything past the capacity or
    /// [`MAX_RUNS`], is dropped rather than kept twice: a duplicate is not
    /// information, and a peer that sends nothing but holes must not be able
    /// to grow this list without end.
    pub fn insert(&mut self, start: SeqNumber, bytes: &[u8]) {
        if bytes.is_empty() || self.holes.len() >= MAX_RUNS {
            return;
        }
        let room = self
            .capacity
            .saturating_sub(self.cost(self.holes.len() + 1));
        let taken = bytes.len().min(room);
        if taken == 0 {
            return;
        }
        let before = self.heap.charged();
        if self
            .heap
            .resize(self.cost(self.holes.len() + 1).saturating_add(taken))
            .is_err()
            || self.holes.try_reserve(1).is_err()
        {
            // Dropped, as a run past the capacity is: the peer sends it again.
            let _ = self
                .heap
                .resize(usize::try_from(before).unwrap_or(usize::MAX));
            return;
        }
        let hole = Hole {
            start,
            bytes: bytes.iter().take(taken).copied().collect(),
        };
        self.held += taken;
        let at = self
            .holes
            .iter()
            .position(|existing| hole.start.precedes(existing.start))
            .unwrap_or(self.holes.len());
        self.holes.insert(at, hole);
        self.coalesce();
        self.settle();
    }

    /// Bring the charge down to what is held, after runs merged or left.
    fn settle(&mut self) {
        let cost = self.cost(self.holes.len());
        let _ = self.heap.resize(cost);
    }

    /// Merge runs that touch or overlap, keeping the earlier copy of any byte
    /// held twice.
    fn coalesce(&mut self) {
        let mut merged: Vec<Hole> = Vec::with_capacity(self.holes.len());
        for hole in self.holes.drain(..) {
            match merged.last_mut() {
                Some(previous) if hole.start.precedes_or_equals(previous.end()) => {
                    let overlap = previous.end().distance_from(hole.start) as usize;
                    let extra = hole.bytes.len().saturating_sub(overlap);
                    previous
                        .bytes
                        .extend(hole.bytes.iter().skip(overlap).copied());
                    self.held -= hole.bytes.len() - extra;
                }
                _ => merged.push(hole),
            }
        }
        self.holes = merged;
    }

    /// Take the run that begins exactly at `next`, if there is one.
    ///
    /// The caller has just consumed everything before `next`, so this is how
    /// the gap closing turns held bytes into deliverable ones.
    pub fn take_contiguous(&mut self, next: SeqNumber) -> Option<Vec<u8>> {
        let first = self.holes.first()?;
        if first.start != next {
            return None;
        }
        let hole = self.holes.remove(0);
        self.held -= hole.bytes.len();
        self.settle();
        Some(hole.bytes)
    }

    /// Drop everything before `next`, and trim a run that straddles it.
    pub fn trim_before(&mut self, next: SeqNumber) {
        for hole in &mut self.holes {
            if hole.start.follows_or_equals(next) {
                continue;
            }
            let drop = next.distance_from(hole.start) as usize;
            let drop = drop.min(hole.bytes.len());
            let _ = hole.bytes.drain(..drop);
            hole.start = next;
            self.held -= drop;
        }
        self.holes.retain(|hole| !hole.bytes.is_empty());
        self.settle();
    }

    /// The runs held, newest-first, as the sequence ranges a SACK option
    /// reports.
    pub fn blocks(&self, out: &mut [(SeqNumber, SeqNumber)]) -> usize {
        let mut count = 0;
        for (slot, hole) in out.iter_mut().zip(self.holes.iter()) {
            *slot = (hole.start, hole.end());
            count += 1;
        }
        count
    }
}
