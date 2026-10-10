//! One queue pair's indices: where the next command goes, how full the
//! submission queue is as the controller last reported it, and where the
//! next completion is expected (NVMe Base Specification 2.0, sections 3.3.1
//! and 3.3.3.2).
//!
//! A submission queue of `n` entries is full when its tail is one behind
//! its head, so it carries `n - 1` commands. A completion entry is new when
//! its phase bit equals the phase the host expects, which starts at one and
//! flips each time the head wraps.

/// The indices of a submission queue and its completion queue, both of
/// `entries` entries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Indices {
    entries: u16,
    sq_tail: u16,
    sq_head: u16,
    cq_head: u16,
    phase: bool,
}

impl Indices {
    /// A pair of `entries` entries, at least two, as a controller's queues
    /// are just after they were made.
    #[must_use]
    pub const fn new(entries: u16) -> Self {
        Indices {
            entries: if entries < 2 { 2 } else { entries },
            sq_tail: 0,
            sq_head: 0,
            cq_head: 0,
            phase: true,
        }
    }

    /// Entries in each queue.
    #[must_use]
    pub const fn entries(&self) -> u16 {
        self.entries
    }

    /// Whether the submission queue has no room.
    #[must_use]
    pub const fn sq_full(&self) -> bool {
        (self.sq_tail + 1) % self.entries == self.sq_head
    }

    /// Take the submission queue's next entry: its index, and the tail to
    /// write to the doorbell once the entry is written. `None` when full.
    pub fn push(&mut self) -> Option<(u16, u16)> {
        if self.sq_full() {
            return None;
        }
        let at = self.sq_tail;
        self.sq_tail = (self.sq_tail + 1) % self.entries;
        Some((at, self.sq_tail))
    }

    /// The completion queue's entry to look at next.
    #[must_use]
    pub const fn cq_head(&self) -> u16 {
        self.cq_head
    }

    /// Whether an entry whose phase bit is `phase` is a new completion.
    #[must_use]
    pub const fn is_new(&self, phase: bool) -> bool {
        phase == self.phase
    }

    /// A completion at the head was taken; it reported the controller's
    /// submission queue head as `sq_head`. The head to write to the
    /// doorbell.
    pub fn pop(&mut self, sq_head: u16) -> u16 {
        if sq_head < self.entries {
            self.sq_head = sq_head;
        }
        self.cq_head += 1;
        if self.cq_head == self.entries {
            self.cq_head = 0;
            self.phase = !self.phase;
        }
        self.cq_head
    }
}
