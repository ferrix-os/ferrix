//! The buffer behind one direction of a socket, and the rules that make it one.
//!
//! A connected `AF_UNIX` socket is two of these, one each way, and like a pipe
//! almost everything a program relies on happens at the edges rather than in
//! the queue. What a pipe does not have is the two things this adds:
//!
//! * **Records.** A `SOCK_SEQPACKET` or `SOCK_DGRAM` socket delivers each send
//!   as one receive, whole or cut short but never run together with the next,
//!   and a receive into a buffer too small for the record discards the rest
//!   while still able to say how long it was, which is what `MSG_TRUNC`
//!   reports. A `SOCK_STREAM` socket has no boundaries, and this buffer joins
//!   consecutive writes into one segment so a stream of one-byte writes is not
//!   a segment per byte.
//! * **Ancillary data.** An `SCM_RIGHTS` message rides with the bytes it was
//!   sent with, and belongs to their first byte. Linux treats the socket buffer
//!   that carries one as a boundary even in a stream, and so does this: a read
//!   runs on through plain bytes into bytes that brought descriptors, takes
//!   them, and never runs on past the end of those bytes. So each receive
//!   hands back at most one set, with the bytes it came with and whatever
//!   plain bytes were before them. A caller can name ancillary data a read
//!   that has taken bytes stops before instead
//!   ([`SocketBuffer::stopping_before`]), which the kernel does for
//!   credentials.
//!
//! The ancillary type is a parameter, `A`, because what it holds -- open
//! files, credentials -- is the kernel's, and this crate stays a pure function
//! of bytes that host tests can reach every edge of. That has one consequence
//! the interface is shaped around: dropping an `A` may mean closing a file,
//! and the kernel holds this buffer's lock. So an `A` is never dropped here.
//! [`SocketBuffer::write`] takes it only when it attaches it, and leaves it with
//! the caller otherwise; [`SocketBuffer::read`] returns it;
//! [`SocketBuffer::close_reader`] and [`SocketBuffer::drain`] hand back every
//! one still queued. The caller drops them after unlocking.
//!
//! As in [`crate::pipe`], waiting is not here: every outcome that would block
//! is a value, and the kernel turns it into a sleep or `EAGAIN`. Unlike a
//! pipe's there is no `poll` answer here, because a socket's is not a fact
//! about one buffer: it combines this direction's buffer with the other's, and
//! with a connection state -- listening, connecting, shut down -- that only
//! the kernel has.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use ferrix_kmem::{Charge, buffer_footprint};
use ferrix_linux_abi::socket::SOCKET_BUFFER_MIN;

/// Whether a buffer keeps the boundaries between writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `SOCK_STREAM`: bytes, with a boundary only where ancillary data is.
    Stream,
    /// `SOCK_SEQPACKET` and `SOCK_DGRAM`: every write is one record, and
    /// every read takes one.
    Record,
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Bytes were queued, and the ancillary data with them if there was any.
    /// A stream may take fewer bytes than offered; the caller waits and
    /// writes the rest. Zero only for an empty stream write, which queues
    /// nothing and leaves any ancillary data with the caller.
    Wrote(usize),
    /// Not enough room: none at all for a stream, or too little for the
    /// whole record.
    WouldBlock,
    /// No reader is left. The caller reports `EPIPE`, and raises `SIGPIPE`
    /// unless told not to.
    Broken,
    /// A record larger than the buffer could ever hold: `EMSGSIZE`, rather
    /// than a wait nothing could end.
    TooBig,
    /// The queue could not grow: the writer's job is at its memory limit, or
    /// the heap is empty. Nothing was queued, and the ancillary data is left
    /// with the caller, which reports `ENOMEM`.
    NoMemory,
}

/// What a read did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome<A> {
    /// Bytes were copied out.
    Read {
        /// Bytes copied. Zero for a zero-length read, and for a zero-length
        /// record.
        bytes: usize,
        /// For a record, its whole length, which is more than `bytes` when
        /// it was cut short: what `MSG_TRUNC` asks for. For a stream, `bytes`.
        full: usize,
        /// The ancillary data that came with the bytes read, taken off the
        /// queue: at most one segment's, and the read ended with that
        /// segment's bytes. Always `None` for a peek, which takes nothing.
        ancillary: Option<A>,
    },
    /// Nothing to read, and a writer could still add something.
    WouldBlock,
    /// Nothing to read, and no writer is left to add anything.
    EndOfFile,
}

/// One write's bytes, or several coalesced writes', and what came with them.
#[derive(Debug)]
struct Segment<A> {
    bytes: VecDeque<u8>,
    ancillary: Option<A>,
    /// The heap the segment holds, charged to the job of the writer that
    /// made it, growth included, and given back as it is read (F-37): its
    /// bytes, and [`SLOTS_A_SEGMENT`] places in the queue.
    heap: Charge,
}

/// The places in its buffer's queue each segment is charged for: the queue
/// grows by doubling and is given back once under a quarter full
/// ([`SocketBuffer::compact`]), so it never has more than four places a
/// segment beyond the [`QUEUE_FLOOR`] it keeps.
const SLOTS_A_SEGMENT: usize = 4;

/// The places a queue keeps however few segments it holds, charged with
/// the socket rather than a segment.
pub const QUEUE_FLOOR: usize = 4;

impl<A> Segment<A> {
    /// A segment holding a copy of `data`, charged to the running task's
    /// job; `None` if the charge or the allocation is refused.
    fn new(data: &[u8], ancillary: Option<A>) -> Option<Segment<A>> {
        let heap = Charge::bytes(Segment::<A>::footprint(data.len())).ok()?;
        let mut bytes = VecDeque::new();
        bytes.try_reserve_exact(data.len()).ok()?;
        bytes.extend(data.iter().copied());
        let mut segment = Segment {
            bytes,
            ancillary,
            heap,
        };
        let _ = segment
            .heap
            .resize(Segment::<A>::footprint(segment.bytes.capacity()));
        Some(segment)
    }

    /// What a segment whose bytes have room for `room` is charged.
    fn footprint(room: usize) -> usize {
        buffer_footprint::<u8>(room)
            .saturating_add(SLOTS_A_SEGMENT.saturating_mul(size_of::<Segment<A>>()))
    }

    /// Add `data` to the end, growing to the next power of two that holds
    /// it, charged to the segment's job. False, with nothing added, when
    /// the charge or the allocation is refused.
    fn append(&mut self, data: &[u8]) -> bool {
        let len = self.bytes.len();
        let needed = len.saturating_add(data.len());
        if needed > self.bytes.capacity() {
            let target = needed.next_power_of_two();
            let before = Segment::<A>::footprint(self.bytes.capacity());
            if self.heap.resize(Segment::<A>::footprint(target)).is_err() {
                return false;
            }
            if self.bytes.try_reserve_exact(target - len).is_err() {
                let _ = self.heap.resize(before);
                return false;
            }
            let _ = self
                .heap
                .resize(Segment::<A>::footprint(self.bytes.capacity()));
        }
        self.bytes.extend(data.iter().copied());
        true
    }

    /// What the segment counts against the capacity: its bytes, or one for
    /// an empty record.
    ///
    /// Linux charges every datagram its bookkeeping as well as its payload,
    /// so a program sending empty records does run out of room. Charging
    /// payload alone would let it queue them without end. One byte keeps
    /// the count finite while leaving what [`SocketBuffer::queued`] reports
    /// payload only.
    fn charge(&self) -> usize {
        self.bytes.len().max(1)
    }
}

/// The queue of one direction of a socket, and whether each end is open.
#[derive(Debug)]
pub struct SocketBuffer<A> {
    kind: Kind,
    segments: VecDeque<Segment<A>>,
    capacity: usize,
    /// Payload bytes in `segments`.
    queued: usize,
    /// `Segment::charge` summed over `segments`.
    charged: usize,
    writer_closed: bool,
    reader_closed: bool,
    /// After a stream read that took ancillary data: how many bytes of the
    /// segment it came with that read left, which [`SocketBuffer::read_on`]
    /// may still take before it must stop. `None` after a read that took
    /// none.
    boundary: Option<usize>,
    /// Whether a stream read that has already taken bytes stops before a
    /// segment whose ancillary data this picks, rather than running on into
    /// it. Picks none unless [`SocketBuffer::stopping_before`] says.
    stops_before: fn(&A) -> bool,
}

impl<A> SocketBuffer<A> {
    /// An empty buffer of `kind` holding at most `capacity` bytes, both ends
    /// open.
    ///
    /// The capacity is raised to [`SOCKET_BUFFER_MIN`] if it is less, as
    /// [`crate::pipe::PipeBuffer::new`] raises its own, so any record of a
    /// page or less can be accepted.
    #[must_use]
    pub fn new(kind: Kind, capacity: usize) -> SocketBuffer<A> {
        SocketBuffer {
            kind,
            segments: VecDeque::new(),
            capacity: capacity.max(SOCKET_BUFFER_MIN),
            queued: 0,
            charged: 0,
            writer_closed: false,
            reader_closed: false,
            boundary: None,
            stops_before: |_| false,
        }
    }

    /// What the queue holds of the heap beyond its segments' charges: its
    /// [`QUEUE_FLOOR`] places, for whoever makes the buffer to be charged.
    #[must_use]
    pub fn floor_footprint() -> usize {
        buffer_footprint::<Segment<A>>(QUEUE_FLOOR)
    }

    /// The heap the queued segments hold and are charged for, in bytes.
    #[must_use]
    pub fn charged_heap(&self) -> u64 {
        self.segments
            .iter()
            .map(|segment| segment.heap.charged())
            .fold(0, u64::saturating_add)
    }

    /// Make room in the queue for one more segment: twice the places, at
    /// least four. The segment that takes it is charged for its share.
    fn room_for_segment(&mut self) -> bool {
        let len = self.segments.len();
        if len < self.segments.capacity() {
            return true;
        }
        let target = len.saturating_add(1).next_power_of_two().max(4);
        self.segments.try_reserve_exact(target - len).is_ok()
    }

    /// Give the queue back most of its places once it is under a quarter
    /// full, so that what a burst of segments grew it to is not kept after
    /// their charges have gone. Kept as it is when there is no memory for a
    /// smaller one.
    fn compact(&mut self) {
        let (len, room) = (self.segments.len(), self.segments.capacity());
        if room <= QUEUE_FLOOR || len >= room / 4 {
            return;
        }
        let mut smaller = VecDeque::new();
        if smaller
            .try_reserve_exact(len.saturating_mul(2).max(QUEUE_FLOOR / 2))
            .is_err()
        {
            return;
        }
        // Room was had just above: moving the segments allocates nothing,
        // and so cannot stop part-way.
        smaller.extend(self.segments.drain(..));
        self.segments = smaller;
    }

    /// The same buffer, with a stream read that has already taken bytes
    /// stopping before bytes whose ancillary data `rule` picks, where it
    /// would otherwise run on into them and take it.
    ///
    /// Linux glues the bytes of one writer into a read and stops before a
    /// different writer's when the reader asked for credentials. The kernel
    /// stamps only some bytes with them, so it cannot tell two writers apart
    /// by their stamps; it stops before every stamp instead, which never
    /// hands one writer's bytes out under another's credentials.
    #[must_use]
    pub fn stopping_before(self, rule: fn(&A) -> bool) -> SocketBuffer<A> {
        SocketBuffer {
            stops_before: rule,
            ..self
        }
    }

    /// Whether the buffer keeps record boundaries.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The most bytes the buffer holds.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Change the capacity: `SO_RCVBUF`. Raised to [`SOCKET_BUFFER_MIN`] if
    /// less.
    ///
    /// Shrinking below what is queued discards nothing. Writes find no room
    /// until reads have brought the queue back under the new size.
    pub fn set_capacity(&mut self, bytes: usize) {
        self.capacity = bytes.max(SOCKET_BUFFER_MIN);
    }

    /// Payload bytes queued: `SIOCOUTQ` for the writer, and `FIONREAD` for
    /// a stream or a `SOCK_SEQPACKET` reader, which Linux answers with the
    /// whole queue.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queued
    }

    /// The length of the record the next read takes, or `None` if nothing is
    /// queued: `FIONREAD` for a `SOCK_DGRAM` reader. A stream has no records,
    /// so for one this is the whole queue.
    #[must_use]
    pub fn next_record(&self) -> Option<usize> {
        match self.kind {
            Kind::Record => self.segments.front().map(|record| record.bytes.len()),
            Kind::Stream => (!self.segments.is_empty()).then_some(self.queued),
        }
    }

    fn free(&self) -> usize {
        self.capacity.saturating_sub(self.charged)
    }

    /// Whether [`SocketBuffer::read`] would not answer
    /// [`ReadOutcome::WouldBlock`]: the condition a blocked reader waits for.
    ///
    /// True with something queued -- an empty record included -- or with the
    /// writer gone, when the read is end of file.
    #[must_use]
    pub fn can_read(&self) -> bool {
        !self.segments.is_empty() || self.writer_closed
    }

    /// Whether [`SocketBuffer::write`] of `len` bytes would not answer
    /// [`WriteOutcome::WouldBlock`]: the condition a blocked writer waits for.
    ///
    /// The write's own rules, so a writer woken by this never finds it still
    /// cannot write: a stream needs room for one byte, a record room for all
    /// of it. With the reader gone, or a record too big to ever fit, it is
    /// true, because the write then answers [`WriteOutcome::Broken`] or
    /// [`WriteOutcome::TooBig`] rather than waiting for room nobody will make.
    #[must_use]
    pub fn can_write(&self, len: usize) -> bool {
        if self.reader_closed {
            return true;
        }
        match self.kind {
            Kind::Stream => len == 0 || self.free() > 0,
            Kind::Record => len > self.capacity || self.free() >= len.max(1),
        }
    }

    /// Queue as much of `data` as the rules allow, with `ancillary`
    /// attached to its first byte.
    ///
    /// The ancillary data is taken out of `ancillary` only when it is queued,
    /// which is only when at least one byte of `data` is -- or, for a record,
    /// when the record is, however short. On any other outcome it is left
    /// where it was, so a writer that waits for room retries with it, and one
    /// that gives up drops it after releasing the lock.
    ///
    /// A stream takes whatever room there is. Bytes without ancillary data
    /// join the last segment if that has none either; bytes with it start a
    /// segment of their own, which is the boundary a read stops at. A record
    /// goes in whole or not at all.
    pub fn write(&mut self, data: &[u8], ancillary: &mut Option<A>) -> WriteOutcome {
        match self.kind {
            Kind::Stream => self.write_stream(data, ancillary),
            Kind::Record => self.write_record(data, ancillary),
        }
    }

    fn write_stream(&mut self, data: &[u8], ancillary: &mut Option<A>) -> WriteOutcome {
        if self.reader_closed {
            return WriteOutcome::Broken;
        }
        if data.is_empty() {
            return WriteOutcome::Wrote(0);
        }
        let count = data.len().min(self.free());
        if count == 0 {
            return WriteOutcome::WouldBlock;
        }
        let accepted = data.get(..count).unwrap_or_default();
        match self.segments.back_mut() {
            Some(last) if ancillary.is_none() && last.ancillary.is_none() => {
                if !last.append(accepted) {
                    return WriteOutcome::NoMemory;
                }
            }
            _ => {
                if !self.room_for_segment() {
                    return WriteOutcome::NoMemory;
                }
                let Some(segment) = Segment::new(accepted, None) else {
                    return WriteOutcome::NoMemory;
                };
                // Room was had just above: this does not grow the queue.
                self.segments.push_back(Segment {
                    ancillary: ancillary.take(),
                    ..segment
                });
            }
        }
        self.queued = self.queued.saturating_add(count);
        self.charged = self.charged.saturating_add(count);
        WriteOutcome::Wrote(count)
    }

    fn write_record(&mut self, data: &[u8], ancillary: &mut Option<A>) -> WriteOutcome {
        // Linux checks the size before it looks for the peer, so an
        // oversized record is `EMSGSIZE` even to a closed reader.
        if data.len() > self.capacity {
            return WriteOutcome::TooBig;
        }
        if self.reader_closed {
            return WriteOutcome::Broken;
        }
        if self.free() < data.len().max(1) {
            return WriteOutcome::WouldBlock;
        }
        if !self.room_for_segment() {
            return WriteOutcome::NoMemory;
        }
        let Some(record) = Segment::new(data, None) else {
            return WriteOutcome::NoMemory;
        };
        self.charged = self.charged.saturating_add(record.charge());
        self.queued = self.queued.saturating_add(data.len());
        self.segments.push_back(Segment {
            ancillary: ancillary.take(),
            ..record
        });
        WriteOutcome::Wrote(data.len())
    }

    /// Take bytes into `out`, or with `peek` copy them and take nothing.
    ///
    /// A stream fills `out` from as many segments as the ancillary
    /// boundaries allow. A record read takes exactly one record, whatever
    /// the size of `out`: what does not fit is discarded, and
    /// [`ReadOutcome::Read::full`] says how much that was. A peek copies the
    /// same bytes the read would have and returns no ancillary data, so the
    /// read that follows still gets it.
    ///
    /// What is queued is delivered even after the writer has gone; end of
    /// file comes only once it is drained. A zero-length read of a stream
    /// with something queued copies nothing and takes nothing, ancillary data
    /// included; of an empty one it waits, as Linux's does.
    pub fn read(&mut self, out: &mut [u8], peek: bool) -> ReadOutcome<A> {
        if self.segments.is_empty() {
            return if self.writer_closed {
                ReadOutcome::EndOfFile
            } else {
                ReadOutcome::WouldBlock
            };
        }
        match (self.kind, peek) {
            (Kind::Stream, true) => self.peek_stream(out),
            (Kind::Stream, false) => self.read_stream(out),
            (Kind::Record, true) => self.peek_record(out),
            (Kind::Record, false) => self.read_record(out),
        }
    }

    fn peek_stream(&self, out: &mut [u8]) -> ReadOutcome<A> {
        let mut copied = 0;
        for segment in &self.segments {
            if copied == out.len() || (copied > 0 && self.stops_at(segment)) {
                break;
            }
            copied += copy_out(&segment.bytes, out.get_mut(copied..).unwrap_or_default());
            if segment.ancillary.is_some() {
                break;
            }
        }
        ReadOutcome::Read {
            bytes: copied,
            full: copied,
            ancillary: None,
        }
    }

    fn read_stream(&mut self, out: &mut [u8]) -> ReadOutcome<A> {
        self.boundary = None;
        let (copied, ancillary) = self.take_stream(out, false);
        ReadOutcome::Read {
            bytes: copied,
            full: copied,
            ancillary,
        }
    }

    /// Whether a read that has taken bytes stops before `segment`.
    fn stops_at(&self, segment: &Segment<A>) -> bool {
        segment.ancillary.as_ref().is_some_and(self.stops_before)
    }

    /// Take bytes off the front of a stream into `out` as one Linux read
    /// goes on through a socket's buffers: through plain bytes, into bytes
    /// that brought ancillary data, taking it, and no further than their
    /// end, whose distance it leaves in `boundary`. Once `going_on` -- bytes
    /// taken, by this call or the read it continues -- it stops before a
    /// segment [`SocketBuffer::stopping_before`] picks.
    ///
    /// Linux's `unix_stream_read_generic` breaks after the buffer whose
    /// descriptors it detached. Measured on a 7.0 host, a socketpair holding
    /// 100 plain bytes, 100 sent with a descriptor and 100 plain reads as
    /// 200 and then 100, by `read`, by `recvmsg` with a control buffer --
    /// the descriptor installed -- and without one -- `MSG_CTRUNC`, the file
    /// closed -- and by `MSG_PEEK`. This used to stop before the descriptor's
    /// bytes, reading 100, 100 and 100.
    fn take_stream(&mut self, out: &mut [u8], mut going_on: bool) -> (usize, Option<A>) {
        let stops_before = self.stops_before;
        let mut copied = 0;
        let mut ancillary = None;
        while let Some(segment) = self.segments.front_mut() {
            let carries = segment.ancillary.is_some();
            if copied == out.len()
                || (going_on && segment.ancillary.as_ref().is_some_and(stops_before))
            {
                break;
            }
            let space = out.get_mut(copied..).unwrap_or_default();
            // A stream never keeps an empty segment and `space` is not
            // empty, so this takes at least the first byte, and with it
            // whatever came with that byte.
            let count = space.len().min(segment.bytes.len());
            for (slot, byte) in space.iter_mut().zip(segment.bytes.drain(..count)) {
                *slot = byte;
            }
            if carries {
                ancillary = segment.ancillary.take();
            }
            copied += count;
            going_on = true;
            let left = segment.bytes.len();
            if left == 0 {
                let _ = self.segments.pop_front();
                self.compact();
            }
            if carries {
                self.boundary = Some(left);
                break;
            }
        }
        self.queued = self.queued.saturating_sub(copied);
        self.charged = self.charged.saturating_sub(copied);
        (copied, ancillary)
    }

    /// Go on with a stream read that has already taken bytes, as one Linux
    /// read goes on through a socket's buffers: what one
    /// [`SocketBuffer::read`] into a larger buffer would still have taken,
    /// and never anything a read must wait for. Answers how many bytes it
    /// took, zero where the read ends -- nothing queued, a boundary, or a
    /// buffer of records, which one read never runs on through -- and the
    /// ancillary data it took, which the caller drops after unlocking: this
    /// goes on only a `read`, which has nowhere to put it, and Linux's closes
    /// what it brings.
    ///
    /// The boundaries are the read's: it runs on into bytes that brought
    /// ancillary data, taking it, and stops at their end; after a read that
    /// took some it takes only the rest of the bytes that brought it. So a
    /// read of 65536 bytes from a socket holding 8000 bytes sent with a
    /// descriptor and then 100 more takes the 8000, as Linux's did on a 7.0
    /// host, however many pieces the caller reads it in.
    ///
    /// Only the last read's boundary is remembered, so a second reader
    /// taking bytes between a read and this one can move where it stops;
    /// two readers of one stream interleave its bytes anyway.
    pub fn read_on(&mut self, out: &mut [u8]) -> (usize, Option<A>) {
        if self.kind != Kind::Stream {
            return (0, None);
        }
        let before = self.boundary;
        let limit = before.map_or(out.len(), |left| left.min(out.len()));
        let (copied, ancillary) = self.take_stream(out.get_mut(..limit).unwrap_or_default(), true);
        if ancillary.is_none() {
            self.boundary = before.map(|left| left.saturating_sub(copied));
        }
        (copied, ancillary)
    }

    fn peek_record(&self, out: &mut [u8]) -> ReadOutcome<A> {
        match self.segments.front() {
            Some(record) => ReadOutcome::Read {
                bytes: copy_out(&record.bytes, out),
                full: record.bytes.len(),
                ancillary: None,
            },
            None => ReadOutcome::WouldBlock,
        }
    }

    fn read_record(&mut self, out: &mut [u8]) -> ReadOutcome<A> {
        let Some(record) = self.segments.pop_front() else {
            return ReadOutcome::WouldBlock;
        };
        self.compact();
        self.queued = self.queued.saturating_sub(record.bytes.len());
        self.charged = self.charged.saturating_sub(record.charge());
        ReadOutcome::Read {
            bytes: copy_out(&record.bytes, out),
            full: record.bytes.len(),
            ancillary: record.ancillary,
        }
    }

    /// No more data will arrive: the writing end shut down or went away.
    /// Once what is queued has been read, reads are end of file.
    pub fn close_writer(&mut self) {
        self.writer_closed = true;
    }

    /// Nobody will read: the reading end shut down or went away. Writes are
    /// [`WriteOutcome::Broken`] from now on, and what is queued will never be
    /// delivered, so it is emptied here and its ancillary data handed back,
    /// in the order it was written, for the caller to drop outside its lock.
    #[must_use = "the ancillary data must be dropped after releasing the buffer's lock"]
    pub fn close_reader(&mut self) -> Vec<A> {
        self.reader_closed = true;
        self.drain()
    }

    /// Whether [`SocketBuffer::close_writer`] has been called.
    #[must_use]
    pub fn writer_closed(&self) -> bool {
        self.writer_closed
    }

    /// Whether [`SocketBuffer::close_reader`] has been called.
    #[must_use]
    pub fn reader_closed(&self) -> bool {
        self.reader_closed
    }

    /// Empty the buffer, handing back every queued ancillary value in the
    /// order it was written.
    ///
    /// For the kernel tearing a socket down, or breaking a cycle of sockets
    /// that hold each other's descriptors in flight, which it must do without
    /// dropping those descriptors while it holds this buffer's lock.
    #[must_use = "the ancillary data must be dropped after releasing the buffer's lock"]
    pub fn drain(&mut self) -> Vec<A> {
        self.queued = 0;
        self.charged = 0;
        self.boundary = None;
        // Taken whole, so the queue's places go with its segments.
        core::mem::take(&mut self.segments)
            .into_iter()
            .filter_map(|segment| segment.ancillary)
            .collect()
    }
}

impl<A> SocketBuffer<A> {
    /// Every piece of ancillary data still queued, oldest first, left where it
    /// is: for the kernel's pass over sockets in flight, which looks at what
    /// each queue holds without taking anything out of it.
    pub fn ancillary(&self) -> impl Iterator<Item = &A> {
        self.segments
            .iter()
            .filter_map(|segment| segment.ancillary.as_ref())
    }
}

/// Copy the front of `from` into `to`, as much as fits, and say how much that
/// was.
fn copy_out(from: &VecDeque<u8>, to: &mut [u8]) -> usize {
    let count = to.len().min(from.len());
    for (slot, byte) in to.iter_mut().zip(from) {
        *slot = *byte;
    }
    count
}
