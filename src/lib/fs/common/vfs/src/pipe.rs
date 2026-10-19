//! The buffer behind a pipe, and the rules that make it one.
//!
//! A pipe is a bounded byte queue with two kinds of end, and almost all of
//! what a program relies on is in what happens at the edges rather than in
//! the queue: a read of an empty pipe with a writer still open waits, and with
//! none is end of file; a write with no reader left is `EPIPE`; and a write of
//! at most [`PIPE_BUF`] bytes is never split, so lines from two processes
//! writing to one pipe do not interleave mid-line.
//!
//! This is the part of that which is a pure function of the buffer and the
//! count of ends. Waiting is not here. The kernel wraps a [`PipeBuffer`] in a
//! lock and two wait queues, and turns [`ReadOutcome::WouldBlock`] into a
//! sleep — or into `EAGAIN` under `O_NONBLOCK` — which is why every outcome
//! that would block is a value rather than a wait: the same buffer serves both
//! and the host tests can reach every edge.
//!
//! # Packets
//!
//! A pipe made with `O_DIRECT` is a packet pipe, as Linux's is since 3.4: each
//! write of up to [`PIPE_BUF`] bytes is one packet, a larger one is cut into
//! packets of [`PIPE_BUF`], and a read takes one packet at most and drops what
//! of it does not fit the reader's buffer. Linux decides it per open file, and
//! `fcntl` can change it; here the pipe is made one or the other, which is all
//! `pipe2(O_DIRECT)` asks.

use alloc::collections::VecDeque;

use ferrix_kmem::{Charge, buffer_footprint};

use crate::node::Readiness;

/// Linux's default pipe capacity: sixteen pages.
pub const PIPE_CAPACITY: usize = 65536;

/// The largest write POSIX promises is never split: one page.
pub const PIPE_BUF: usize = 4096;

/// `PIPEFS_MAGIC`, from `include/uapi/linux/magic.h`: what `fstatfs` on a pipe
/// reports as the filesystem it is on.
pub const PIPEFS_MAGIC: u64 = 0x5049_5045;

/// What a read did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Bytes were copied out. Zero only for a zero-length read.
    Read(usize),
    /// Nothing to read, and a writer could still add something.
    WouldBlock,
    /// Nothing to read, and no writer is left to add anything.
    EndOfFile,
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Bytes were queued. May be fewer than asked for a write larger than
    /// [`PIPE_BUF`]; the caller waits and writes the rest.
    Wrote(usize),
    /// Not enough room: none at all, or too little for a write that must not
    /// be split.
    WouldBlock,
    /// No reader is left. The caller reports `EPIPE`, and raises `SIGPIPE`.
    Broken,
    /// The buffer could not grow: the pipe's job is at its memory limit, or
    /// the heap is empty. Nothing was queued; the caller reports `ENOMEM`.
    NoMemory,
}

/// The queue, and how many of each end are open.
#[derive(Debug)]
pub struct PipeBuffer {
    data: VecDeque<u8>,
    capacity: usize,
    readers: usize,
    writers: usize,
    /// The length of each packet in `data`, oldest first, for a packet pipe;
    /// `None` for a byte stream. They add up to `data.len()`.
    packets: Option<VecDeque<usize>>,
    /// The heap `data` holds, charged to the job that made the pipe as the
    /// buffer grows, and given back as the pipe goes (F-37). The buffer
    /// keeps the room it grew to, and so does the charge.
    charge: Charge,
}

impl PipeBuffer {
    /// An empty pipe holding at most `capacity` bytes, with no ends open,
    /// whose buffer is charged to the running task's job as it grows. A
    /// charge of nothing is never refused, so this cannot fail.
    #[must_use]
    pub fn new(capacity: usize) -> PipeBuffer {
        PipeBuffer {
            data: VecDeque::new(),
            capacity: capacity.max(PIPE_BUF),
            readers: 0,
            writers: 0,
            packets: None,
            charge: Charge::bytes(0).unwrap_or_default(),
        }
    }

    /// An empty packet pipe: see the module documentation.
    #[must_use]
    pub fn packets(capacity: usize) -> PipeBuffer {
        PipeBuffer {
            packets: Some(VecDeque::new()),
            ..PipeBuffer::new(capacity)
        }
    }

    /// Whether this is a packet pipe.
    #[must_use]
    pub fn is_packets(&self) -> bool {
        self.packets.is_some()
    }

    /// How many bytes the next read takes out of the pipe: the next packet
    /// for a packet pipe, all of it for a byte stream.
    #[must_use]
    pub fn next_read(&self) -> usize {
        match &self.packets {
            Some(packets) => packets.front().copied().unwrap_or(0),
            None => self.data.len(),
        }
    }

    /// The heap the buffer holds and is charged for, in bytes.
    #[must_use]
    pub fn charged(&self) -> u64 {
        self.charge.charged()
    }

    /// Make room for `count` more bytes, charged, before taking them from
    /// somewhere that cannot have them back: what `splice` from another pipe
    /// does. False, with nothing changed, if the room cannot be had.
    pub fn reserve(&mut self, count: usize) -> bool {
        self.make_room(count)
    }

    /// Make room for `count` more bytes, charged: the next power of two
    /// that holds them, up to the capacity, so a pipe of short writes grows
    /// a few times and not at every one. False, with nothing changed, when
    /// the charge or the allocation is refused.
    fn make_room(&mut self, count: usize) -> bool {
        let len = self.data.len();
        let needed = len.saturating_add(count);
        if needed <= self.data.capacity() {
            return true;
        }
        let target = needed.next_power_of_two().min(self.capacity.max(needed));
        let before = buffer_footprint::<u8>(self.data.capacity());
        if self.charge.resize(buffer_footprint::<u8>(target)).is_err() {
            return false;
        }
        if self.data.try_reserve_exact(target - len).is_err() {
            let _ = self.charge.resize(before);
            return false;
        }
        // The deque may round its room up; the charge follows what it holds,
        // and keeps what it had if the job has no more to give.
        let _ = self
            .charge
            .resize(buffer_footprint::<u8>(self.data.capacity()));
        true
    }

    /// A read end was opened.
    pub fn open_reader(&mut self) {
        self.readers = self.readers.saturating_add(1);
    }

    /// A read end was closed.
    pub fn close_reader(&mut self) {
        self.readers = self.readers.saturating_sub(1);
    }

    /// A write end was opened.
    pub fn open_writer(&mut self) {
        self.writers = self.writers.saturating_add(1);
    }

    /// A write end was closed.
    pub fn close_writer(&mut self) {
        self.writers = self.writers.saturating_sub(1);
    }

    /// Open read ends.
    #[must_use]
    pub fn readers(&self) -> usize {
        self.readers
    }

    /// Open write ends.
    #[must_use]
    pub fn writers(&self) -> usize {
        self.writers
    }

    /// Bytes waiting to be read: `FIONREAD`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn free(&self) -> usize {
        self.capacity.saturating_sub(self.data.len())
    }

    /// Bytes a write could queue now: what `splice` into the pipe may take
    /// from its source without taking more than it can put down.
    #[must_use]
    pub fn room(&self) -> usize {
        self.free()
    }

    /// Whether [`PipeBuffer::read`] into a non-empty buffer would not answer
    /// [`ReadOutcome::WouldBlock`]: the condition a blocked reader waits for.
    #[must_use]
    pub fn can_read(&self) -> bool {
        !self.data.is_empty() || self.writers == 0
    }

    /// Whether [`PipeBuffer::write`] of `len` bytes would not answer
    /// [`WriteOutcome::WouldBlock`]: the condition a blocked writer waits for.
    ///
    /// The same rule as the write itself -- a write of at most [`PIPE_BUF`]
    /// needs room for all of it, a larger one room for anything -- so a writer
    /// woken by this is never woken to find it still cannot write.
    #[must_use]
    pub fn can_write(&self, len: usize) -> bool {
        if self.readers == 0 || len == 0 {
            return true;
        }
        if self.packets.is_some() {
            return self.free() >= len.min(PIPE_BUF);
        }
        if len <= PIPE_BUF {
            self.free() >= len
        } else {
            self.free() > 0
        }
    }

    /// Take up to `buf.len()` bytes; from a packet pipe, the next packet, of
    /// which what does not fit `buf` is dropped.
    ///
    /// What is in the pipe is delivered even after the last writer has gone;
    /// end of file comes only once it is drained.
    pub fn read(&mut self, buf: &mut [u8]) -> ReadOutcome {
        if buf.is_empty() {
            return ReadOutcome::Read(0);
        }
        if self.data.is_empty() {
            return if self.writers == 0 {
                ReadOutcome::EndOfFile
            } else {
                ReadOutcome::WouldBlock
            };
        }
        let taken = match self.packets.as_mut().map(VecDeque::pop_front) {
            Some(packet) => packet.unwrap_or(self.data.len()),
            None => self.data.len(),
        };
        let count = buf.len().min(taken);
        // A slice at a time rather than a byte at a time: the ring is at most
        // two runs, and each is one copy.
        let (front, back) = self.data.as_slices();
        let mut rest = buf.get_mut(..count).unwrap_or_default();
        for run in [front, back] {
            let take = run.len().min(rest.len());
            let (to, after) = rest.split_at_mut(take);
            to.copy_from_slice(run.get(..take).unwrap_or_default());
            rest = after;
        }
        // A packet's bytes past what the reader could take go with it; a
        // stream keeps them for the next read.
        let gone = if self.packets.is_some() { taken } else { count };
        drop(self.data.drain(..gone.min(self.data.len())));
        ReadOutcome::Read(count)
    }

    /// Put `bytes` back at the front, to be read next: the start of what
    /// [`PipeBuffer::read`] took and the reader could not be given, because
    /// the memory it was to be copied to was bad. Linux copies straight from
    /// the pipe's pages into the reader's and consumes only what arrived, so
    /// a read into a bad buffer leaves the pipe as it was; this is the same
    /// done after the fact.
    ///
    /// Taken whatever room is left: the bytes were in the pipe a moment ago,
    /// and a writer that filled the room since has only pushed the pipe past
    /// its capacity until they are read, as `free` saturates at none.
    pub fn unread(&mut self, bytes: &[u8]) {
        // The room is there unless a writer took it meanwhile; then it grows
        // uncharged, by at most one read, rather than lose the bytes.
        let _ = self.make_room(bytes.len());
        for &byte in bytes.iter().rev() {
            self.data.push_front(byte);
        }
        // Back as the packet it was read from, less what did not fit.
        if let Some(packets) = self.packets.as_mut()
            && !bytes.is_empty()
        {
            packets.push_front(bytes.len());
        }
    }

    /// Queue as much of `data` as the rules allow.
    ///
    /// A write of at most [`PIPE_BUF`] bytes goes in whole or not at all. A
    /// larger one takes whatever room there is, and the caller comes back for
    /// the rest — which is exactly the case in which POSIX lets writers
    /// interleave. A packet pipe takes one packet: all of a write of at most
    /// [`PIPE_BUF`] bytes, or the first [`PIPE_BUF`] of a larger one, whole or
    /// not at all.
    pub fn write(&mut self, data: &[u8]) -> WriteOutcome {
        if self.readers == 0 {
            return WriteOutcome::Broken;
        }
        if data.is_empty() {
            return WriteOutcome::Wrote(0);
        }
        let free = self.free();
        let count = if self.packets.is_some() {
            let packet = data.len().min(PIPE_BUF);
            if free < packet {
                return WriteOutcome::WouldBlock;
            }
            packet
        } else if data.len() <= PIPE_BUF {
            if free < data.len() {
                return WriteOutcome::WouldBlock;
            }
            data.len()
        } else {
            if free == 0 {
                return WriteOutcome::WouldBlock;
            }
            data.len().min(free)
        };
        if !self.make_room(count) {
            return WriteOutcome::NoMemory;
        }
        self.data.extend(data.get(..count).unwrap_or_default());
        if let Some(packets) = self.packets.as_mut() {
            packets.push_back(count);
        }
        WriteOutcome::Wrote(count)
    }

    /// What `poll` reports for a read end.
    ///
    /// Readable when a read would not block — including at end of file, which
    /// is the one way a program polling a pipe learns the writer is gone — and
    /// hung up once no writer is left.
    #[must_use]
    pub fn read_readiness(&self) -> Readiness {
        Readiness {
            readable: !self.data.is_empty() || self.writers == 0,
            writable: false,
            hangup: self.writers == 0,
            error: false,
            priority: false,
        }
    }

    /// What `poll` reports for a write end.
    ///
    /// Writable when a write of [`PIPE_BUF`] bytes would go in whole, and in
    /// error once no reader is left — so a poll loop wakes to discover
    /// `EPIPE` rather than waiting for room nobody will make.
    #[must_use]
    pub fn write_readiness(&self) -> Readiness {
        Readiness {
            readable: false,
            writable: self.readers == 0 || self.free() >= PIPE_BUF,
            hangup: false,
            error: self.readers == 0,
            priority: false,
        }
    }
}
