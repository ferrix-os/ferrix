//! What a filesystem asks of a disk, and what the queue remembers of it.

/// A caller-chosen name for one request.
///
/// The queue returns it on completion and never interprets it. It must be
/// unique among the requests that have been submitted and not yet completed;
/// once completed, the caller may use it again.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RequestId(pub u64);

/// What a request does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Read sectors into the caller's memory.
    Read,
    /// Write sectors from the caller's memory.
    Write,
    /// Make everything written so far durable. Carries no range, and is a
    /// barrier.
    Flush,
    /// Tell the device the sectors' contents are no longer needed.
    Discard,
}

/// How a request wants to be treated.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Flags {
    /// A caller is waiting on this request, so it takes the short expiry that
    /// reads have rather than the long one of background writes.
    pub sync: bool,
    /// Force unit access: the data is durable when the write completes. Only
    /// meaningful on a write, and it makes the write a barrier.
    pub fua: bool,
}

/// One request, as a filesystem submits it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    /// The caller's name for it.
    pub id: RequestId,
    /// What it does.
    pub op: Op,
    /// The first logical sector. Zero for a flush.
    pub sector: u64,
    /// The number of logical sectors. Zero for a flush, and for nothing else.
    pub count: u32,
    /// How it wants to be treated.
    pub flags: Flags,
}

impl Request {
    /// A read of `count` sectors from `sector`.
    #[must_use]
    pub const fn read(id: u64, sector: u64, count: u32) -> Self {
        Self::new(id, Op::Read, sector, count)
    }

    /// An asynchronous write of `count` sectors from `sector`.
    #[must_use]
    pub const fn write(id: u64, sector: u64, count: u32) -> Self {
        Self::new(id, Op::Write, sector, count)
    }

    /// A discard of `count` sectors from `sector`.
    #[must_use]
    pub const fn discard(id: u64, sector: u64, count: u32) -> Self {
        Self::new(id, Op::Discard, sector, count)
    }

    /// A flush.
    #[must_use]
    pub const fn flush(id: u64) -> Self {
        Self::new(id, Op::Flush, 0, 0)
    }

    /// The same request with `sync` set.
    #[must_use]
    pub const fn sync(mut self) -> Self {
        self.flags.sync = true;
        self
    }

    /// The same request with `fua` set.
    #[must_use]
    pub const fn fua(mut self) -> Self {
        self.flags.fua = true;
        self
    }

    const fn new(id: u64, op: Op, sector: u64, count: u32) -> Self {
        Request {
            id: RequestId(id),
            op,
            sector,
            count,
            flags: Flags {
                sync: false,
                fua: false,
            },
        }
    }

    /// Whether this request is a barrier: a flush, or a write with `fua`.
    #[must_use]
    pub const fn is_barrier(&self) -> bool {
        matches!(self.op, Op::Flush) || (matches!(self.op, Op::Write) && self.flags.fua)
    }
}

/// One request inside a unit: its own sub-range, and when it arrived.
///
/// Parts are what a [`Dispatch`](crate::Dispatch) and a
/// [`Completion`](crate::Completion) list, in submission order, so a caller
/// can find each request's memory and each request's caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Part {
    /// The caller's name for the request.
    pub id: RequestId,
    /// The request's first sector.
    pub sector: u64,
    /// The request's sector count.
    pub count: u32,
    /// The request's flags.
    pub flags: Flags,
    /// The tick at which it was submitted.
    pub arrival: u64,
    /// The tick by which it should have been dispatched.
    pub deadline: u64,
    /// Submission order across the whole queue; unique.
    pub(crate) seq: u64,
}
