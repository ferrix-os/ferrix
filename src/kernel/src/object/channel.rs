//! Channels: two endpoints, each reading what the other writes.
//!
//! A channel's state -- each end's queue of messages written *to* it, its
//! waiters, its port registrations and whether it is closed -- lives in one
//! `Channel` both ends share, and an [`Endpoint`] is a handle's view of one
//! side of it. An end is open exactly while its `Endpoint` is alive: while a
//! handle, a message in flight, or a call its own holder is making holds it.
//! An endpoint travelling in a message is still open, which is the right
//! answer -- it has an owner, who simply has not read it yet. The last of
//! those going marks its side closed, and the peer sees `PEER_CLOSED` from
//! that moment.
//!
//! # Nothing on one side holds the other
//!
//! An end reaches its peer's queue, waiters and registrations through the
//! channel, never through the peer's `Endpoint`, so nothing done on one end
//! -- a write, a look at its signals, a read that makes room -- holds the
//! other open. Before, each end knew its peer by a weak link it upgraded for
//! every such look, and the upgrade kept the peer alive until it was let go:
//! a write held the end it wrote to until it had woken that end's reader, so
//! a driver woken by the kernel's READY could read it and close its handle
//! while the kernel's write, preempted or on a processor the host was not
//! running, still held the driver's end. A quiesce made the instant the
//! driver's handle closed saw its channel still open and refused the device
//! as served (FX-1004). "Open" now means held by an owner, and only an
//! owner's handles hold it.
//!
//! # A write is all or nothing
//!
//! [`Endpoint::write`] takes the sender's handles out of its table only while
//! it holds the peer's queue lock and after the queue has agreed to take the
//! message. A refused write therefore moves nothing: every handle the caller
//! named is still in its table under the same number. The lock order this
//! implies — a process's handle table, then a peer's queue — is never taken
//! the other way round: a read releases its own queue before it touches the
//! reader's table.
//!
//! A side is marked closed under its queue lock, as its queue is emptied, and
//! a write looks at the mark under the same lock: a message either lands
//! before the close and is freed with the rest, or is refused as written to a
//! closed peer. None is left in a closed side's queue.
//!
//! # No cycles
//!
//! Two endpoints each queued in the other's inbox would keep each other alive
//! after every handle to both is closed, with everything they hold. So a
//! send carrying an endpoint is refused if it would close such a loop:
//! [`check_carry`] walks from what the message carries, through the endpoints
//! queued in each, looking for the end it is about to land in. The shared
//! `Channel` adds no edge: what a side's queue holds is freed when that
//! side's `Endpoint` goes, whichever end still holds the channel.

pub(crate) mod check;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering, fence};

use crate::sync::SpinLock;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES};
use ferrix_objects::message::{Limits, Message, MessageQueue, ReceiveError, SendError};
use ferrix_objects::reach::{Reach, reaches};

use super::port::{Observer, Observers, PortError, deliver, register, trigger};
use super::{Object, Transfer, dispose};
use crate::fallible::{self, AllocError};
use crate::object::quota::{Charge, Resource};
use crate::sched::{Task, WaitQueue};

/// The most messages an endpoint holds unread.
///
/// Two hundred and fifty-six: deep enough that a driver servicing a burst
/// does not see its client wait, shallow enough that a client whose driver has
/// wedged is told to wait long before it has pinned megabytes of kernel heap.
const MAX_QUEUED: usize = 256;

/// What every channel carries.
const LIMITS: Limits = Limits {
    max_bytes: CHANNEL_MAX_BYTES,
    max_handles: CHANNEL_MAX_HANDLES,
    max_queued: MAX_QUEUED,
};

/// The most bytes a message kept in an inbox's slot carries: what
/// `channel_write_read` carries in registers.
pub(crate) const SMALL_BYTES: usize = ferrix_native_abi::nr::CHANNEL_WRITE_READ_BYTES;

/// A message of at most [`SMALL_BYTES`] and no handles, held in place.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Small {
    /// The bytes, the first `len` of them meant.
    pub(crate) bytes: [u8; SMALL_BYTES],
    /// How many.
    pub(crate) len: usize,
}

impl Small {
    /// `bytes` held in place, if they fit.
    pub(crate) fn of(bytes: &[u8]) -> Option<Small> {
        let mut small = Small {
            bytes: [0; SMALL_BYTES],
            len: bytes.len(),
        };
        small.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(small)
    }

    /// The bytes meant.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }
}

/// What a side holds unread: its queue, and before it a slot for one small
/// message.
///
/// # Why a slot
///
/// A queued message is a heap allocation for its bytes and room in the
/// queue, made by the writer and freed by the reader: on a round trip
/// between two programs, two of each. A small message without handles
/// written while nothing else is queued -- a request, its answer -- goes in
/// the slot instead, and is the head of the queue for everything that reads
/// it. Nothing goes in the slot while the queue holds anything, so the slot
/// is always the oldest message: the order a reader sees is the order the
/// writers wrote.
///
/// # A buffer kept for it
///
/// A reader that takes the slot's message as a queued one -- `channel_read`,
/// or a kernel ring's control loop -- needs it in a buffer of its own. That
/// buffer is the inbox's `spare`, made when the slot is first filled and
/// kept: while the slot is full the spare holds room for [`SMALL_BYTES`], so
/// taking the message, or putting it behind one given back, never allocates
/// and never fails for memory. A write that finds no spare and cannot make
/// one queues the message instead, as `channel_write` would, and only the
/// writer hears of the memory. `channel_write_read`'s own read copies out of
/// the slot and leaves the spare where it is.
#[derive(Debug)]
struct Inbox {
    /// The slot, the head when it is full.
    small: Option<Small>,
    /// Room for the slot's message, whenever the slot is full.
    spare: Vec<u8>,
    /// Everything after it.
    queue: MessageQueue<Transfer>,
    /// The park (`docs/OPAQUE-KERNEL.md` §9.7, part 2): the one task blocked
    /// in `channel_write_read` reading this side through the fast path,
    /// whose reply a fast writer may put in its reply cell. Set only while
    /// the inbox is empty and the task blocked, under this lock; taken by a
    /// fast commit, by any general write or close, which then wakes it, and
    /// by the task as it leaves its call. Set only where the fast path is
    /// (`arch::FAST_WRITE_READ`): elsewhere nothing parks.
    parked: Option<Arc<Task>>,
}

impl Inbox {
    /// Nothing held.
    fn new() -> Inbox {
        Inbox {
            small: None,
            spare: Vec::new(),
            queue: MessageQueue::new(LIMITS),
            parked: None,
        }
    }

    /// The parked reader, taken off its record, for a general writer or
    /// close to wake once it has let the lock go: the general path's one
    /// new test (part 2). Always `None` where nothing parks.
    fn take_parked(&mut self) -> Option<Arc<Task>> {
        self.parked.take()
    }

    /// Whether nothing is waiting.
    fn is_empty(&self) -> bool {
        self.small.is_none() && self.queue.is_empty()
    }

    /// Whether a write would be refused as full. The slot does not count: it
    /// is used only while the queue is empty.
    fn is_full(&self) -> bool {
        self.queue.is_full()
    }

    /// Whether a message of this shape could ever be sent.
    fn accepts(&self, bytes: usize, handles: usize) -> bool {
        self.queue.accepts(bytes, handles)
    }

    /// Hold `bytes` in the slot, if they fit, nothing is waiting, and the
    /// spare has room for them or can be made to.
    fn put_small(&mut self, bytes: &[u8]) -> bool {
        if !self.is_empty() {
            return false;
        }
        let Some(small) = Small::of(bytes) else {
            return false;
        };
        if self.spare.capacity() < SMALL_BYTES {
            let Ok(spare) = fallible::try_with_capacity(SMALL_BYTES) else {
                return false;
            };
            self.spare = spare;
        }
        self.small = Some(small);
        true
    }

    /// The slot's message `small` in the spare, now a buffer of its own; the
    /// spare is made again by the next write the slot takes.
    fn take_spare(&mut self, small: &Small) -> Vec<u8> {
        let mut bytes = core::mem::take(&mut self.spare);
        bytes.clear();
        // NOALLOC: the spare holds room for `SMALL_BYTES` whenever the slot
        // is full (`put_small`), and a slot message is at most that.
        bytes.extend_from_slice(small.as_bytes());
        bytes
    }

    /// The waiting messages' handles' carriers, oldest first: the queue's,
    /// since the slot's message carries none.
    fn iter(&self) -> impl Iterator<Item = &ChannelMessage> + '_ {
        self.queue.iter()
    }

    /// Whether the oldest message waiting carries endpoints and fits, as
    /// [`Endpoint::read`] asks before it takes it.
    fn head_needs_topology(&self, byte_capacity: usize, handle_capacity: usize) -> bool {
        self.small.is_none()
            && self.queue.iter().next().is_some_and(|head| {
                head.bytes.len() <= byte_capacity
                    && head.handles.len() <= handle_capacity
                    && carries_endpoints(head)
            })
    }

    /// The oldest message, if it fits, as a queued one.
    fn pop_fitting(
        &mut self,
        byte_capacity: usize,
        handle_capacity: usize,
    ) -> Result<ChannelMessage, ReceiveError> {
        if let Some(small) = self.small {
            if small.len > byte_capacity {
                return Err(ReceiveError::TooSmall {
                    bytes: small.len,
                    handles: 0,
                });
            }
            let bytes = self.take_spare(&small);
            self.small = None;
            return Ok(Message {
                bytes,
                handles: Vec::new(),
            });
        }
        self.queue.pop_fitting(byte_capacity, handle_capacity)
    }

    /// The oldest message, if it is small and carries no handles, held in
    /// place. A queued one is taken out of the queue and its buffer freed.
    fn pop_small(&mut self) -> Result<Small, ReceiveError> {
        if let Some(small) = self.small.take() {
            return Ok(small);
        }
        let (bytes, handles) = self.queue.peek_sizes().ok_or(ReceiveError::Empty)?;
        if bytes > SMALL_BYTES || handles != 0 {
            return Err(ReceiveError::TooSmall { bytes, handles });
        }
        let message = self.queue.pop_fitting(SMALL_BYTES, 0)?;
        Small::of(&message.bytes).ok_or(ReceiveError::TooSmall { bytes, handles })
    }

    /// Make room for one more queued message.
    fn reserve(&mut self) -> Result<(), SendError> {
        // FALLIBLE: `MessageQueue::reserve` grows the queue with
        // `try_reserve_deque` and answers `NoMemory`.
        self.queue.reserve()
    }

    /// Queue a message after everything waiting.
    fn push(&mut self, message: ChannelMessage) -> Result<(), (SendError, ChannelMessage)> {
        // FALLIBLE: `MessageQueue::push` reserves with `try_reserve_deque`
        // and gives the message back with `NoMemory`.
        self.queue.push(message)
    }

    /// Put a message taken by [`Inbox::pop_fitting`] back at the head.
    ///
    /// A writer may have filled the slot since, the queue being empty once
    /// the message was out; that message is younger, so it moves into the
    /// queue behind the one coming back.
    fn unpop(&mut self, message: ChannelMessage) -> Result<(), ChannelMessage> {
        if let Some(small) = self.small {
            let younger = Message {
                bytes: self.take_spare(&small),
                handles: Vec::new(),
            };
            if let Err(younger) = self.queue.unpop(younger) {
                // Still the slot's, with its room back.
                self.spare = younger.bytes;
                return Err(message);
            }
            self.small = None;
        }
        self.queue.unpop(message)
    }

    /// Take everything, as a side closes. The slot's message holds nothing
    /// to free, and its spare goes now.
    fn drain(&mut self) -> VecDeque<ChannelMessage> {
        self.small = None;
        self.spare = Vec::new();
        self.queue.drain()
    }
}

/// The most queued endpoints one send's cycle check may walk.
///
/// A program can nest endpoints as deep as memory allows, and the walk runs
/// under a lock every endpoint-carrying send waits on. A thousand and
/// twenty-four is far past anything a driver's control plane builds, and a
/// send that reaches it is refused as too big rather than allowed to hold
/// that lock for as long as the program likes.
const MAX_WALK: usize = 1024;

/// A message as it sits in a queue.
pub(crate) type ChannelMessage = Message<Transfer>;

/// Why a write did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteFailure<E> {
    /// Nobody holds the other end.
    PeerClosed,
    /// Larger than a channel carries.
    TooBig,
    /// The peer's queue is full.
    Full,
    /// Taking the handles out of the sender's table was refused.
    Take(E),
    /// There was no memory to queue it.
    NoMemory,
}

/// Why a read found nothing to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// Nothing is queued, and nobody holds the other end to write more.
    PeerClosed,
    /// Nothing is queued yet.
    Empty,
    /// The next message carries channel endpoints, and may only be taken
    /// holding [`super::TOPOLOGY`]. Still queued.
    NeedsTopology,
    /// The next message needs more room, and is still queued.
    TooSmall {
        /// Its size in bytes.
        bytes: usize,
        /// How many handles it carries.
        handles: usize,
    },
}

/// Which of a channel's two sides an [`Endpoint`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The first end [`Endpoint::pair`] returns.
    First,
    /// The second.
    Second,
}

impl Side {
    /// The other side.
    fn other(self) -> Side {
        match self {
            Side::First => Side::Second,
            Side::Second => Side::First,
        }
    }
}

/// One side of a channel: what is written to it, and who waits on it.
#[derive(Debug)]
struct Half {
    /// Messages written by the other side, waiting for this one to read them.
    inbox: SpinLock<Inbox>,
    /// Woken when this side's signals may have changed: a message arrived, the
    /// other side's queue gained room, or the other side closed.
    waiters: WaitQueue,
    /// Port registrations waiting on this side's signals. Taken only inside
    /// `inbox`'s lock, which is what serialises a registration against the
    /// change it waits for.
    observers: SpinLock<Observers>,
    /// Whether a port registration has ever been made on this side: set
    /// under `inbox`'s lock with the registration and never cleared, for the
    /// fast path's T10 to read under that lock instead of taking
    /// `observers`'s. A side once observed declines the fast path for good.
    observed: AtomicBool,
    /// Whether this side's [`Endpoint`] has gone. Set once, under `inbox`'s
    /// lock as the queue is emptied, and never cleared.
    closed: AtomicBool,
    /// What a reader waiting on this side waits for, in one word:
    /// [`NONEMPTY`] and [`PEER_CLOSED`]. Written only under `inbox`'s lock,
    /// and read without it by `channel_write_read`'s wait
    /// ([`Endpoint::readable_or_closed`]).
    state: AtomicU8,
}

/// A side's [`Half::state`]: its inbox holds a message. Set and cleared
/// under the inbox lock by every operation that changes whether the inbox
/// is empty, so that under the lock it is set exactly when the inbox is not
/// empty.
const NONEMPTY: u8 = 1 << 0;
/// A side's [`Half::state`]: the other side has gone. Set once, by the
/// closer under this side's inbox lock, after the other side's `closed` and
/// before this side's waiters are woken; never cleared.
const PEER_CLOSED: u8 = 1 << 1;

impl Half {
    /// An open side with nothing queued.
    fn new() -> Half {
        Half {
            inbox: SpinLock::new(Inbox::new()),
            waiters: WaitQueue::new(),
            observers: SpinLock::new(Observers::new()),
            observed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            state: AtomicU8::new(0),
        }
    }

    /// Bring [`NONEMPTY`] into line with `inbox`, this side's, whose lock the
    /// caller holds: after every operation that may have changed whether it
    /// is empty.
    fn note(&self, inbox: &Inbox) {
        let word = self.state.load(Ordering::Relaxed);
        let fresh = if inbox.is_empty() {
            word & !NONEMPTY
        } else {
            word | NONEMPTY
        };
        if fresh != word {
            self.state.store(fresh, Ordering::Release);
        }
    }

    /// Mark the other side gone in this side's word, holding this side's
    /// inbox lock.
    fn note_peer_closed(&self) {
        let _ = self.state.fetch_or(PEER_CLOSED, Ordering::Release);
    }

    /// This side's word, read without the lock.
    fn word(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// Whether this side's `Endpoint` has gone.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// A channel: both sides, held by both ends.
///
/// Freed when both ends have gone, and holding nothing by then: each side's
/// queue was emptied, and its registrations let go, as its end closed.
#[derive(Debug)]
struct Channel {
    /// The first end's side.
    first: Half,
    /// The second end's.
    second: Half,
    /// Its two ends, charged as two kernel objects to the job that made them
    /// for as long as either exists, wherever it went: an end parked in
    /// another channel's queue is counted as one in a handle table is
    /// (`object::quota`).
    #[expect(
        dead_code,
        reason = "AUDIT: held for its drop, which uncharges the job"
    )]
    charge: Charge,
}

impl Channel {
    /// The side `side` names.
    fn half(&self, side: Side) -> &Half {
        match side {
            Side::First => &self.first,
            Side::Second => &self.second,
        }
    }
}

/// One end of a channel.
#[derive(Debug)]
pub(crate) struct Endpoint {
    /// The channel, shared with the other end.
    channel: Arc<Channel>,
    /// Which side of it this end is.
    side: Side,
}

impl Endpoint {
    /// A new channel's two ends.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the channel or either end could not be allocated
    /// (finding F-23).
    pub(crate) fn pair() -> Result<(Arc<Endpoint>, Arc<Endpoint>), AllocError> {
        let charge = Charge::running(Resource::Objects, 2).map_err(|_| AllocError)?;
        let channel = fallible::try_arc(Channel {
            first: Half::new(),
            second: Half::new(),
            charge,
        })?;
        let first = fallible::try_arc(Endpoint {
            channel: Arc::clone(&channel),
            side: Side::First,
        })?;
        let second = fallible::try_arc(Endpoint {
            channel,
            side: Side::Second,
        })?;
        Ok((first, second))
    }

    /// This end's side.
    fn own(&self) -> &Half {
        self.channel.half(self.side)
    }

    /// The other end's side, reached without holding the other end.
    fn peer(&self) -> &Half {
        self.channel.half(self.side.other())
    }

    /// Queue a message for the peer, taking its handles with `take` only once
    /// the peer's queue has room for it.
    ///
    /// `take` runs under the peer's queue lock. It is the caller's handle
    /// table removing the handles, and it must not take any lock a queue
    /// holder could be waiting for.
    ///
    /// # Errors
    ///
    /// [`WriteFailure`]; whichever it is, `take` either did not run or failed
    /// and removed nothing.
    pub(crate) fn write<E>(
        &self,
        bytes: Vec<u8>,
        handle_count: usize,
        take: impl FnOnce() -> Result<Vec<Transfer>, E>,
    ) -> Result<(), WriteFailure<E>> {
        let peer = self.peer();
        let (refused, fired, parked) = {
            let mut inbox = peer.inbox.lock();
            // Under the lock the close empties the queue under, so nothing
            // lands in a queue nobody will read.
            if peer.is_closed() {
                return Err(WriteFailure::PeerClosed);
            }
            if !inbox.accepts(bytes.len(), handle_count) {
                return Err(WriteFailure::TooBig);
            }
            if inbox.is_full() {
                return Err(WriteFailure::Full);
            }
            // FALLIBLE: the queue's room for the message, made before the
            // handles leave the sender, whose write refused for memory then
            // costs it nothing: the handles leave only if the write succeeds.
            inbox.reserve().map_err(|_| WriteFailure::NoMemory)?;
            let handles = take().map_err(WriteFailure::Take)?;
            // NOALLOC: the room `reserve` made above.
            let refused = inbox.push(Message { bytes, handles }).err();
            peer.note(&inbox);
            let fired = refused.is_none() && trigger(&mut peer.observers.lock(), Signals::READABLE);
            // A parked reader is woken whatever became of the write: one
            // refused (which the checks above make unreachable) wakes it
            // for nothing, and it parks again.
            let parked = inbox.take_parked();
            (refused, fired, parked)
        };
        // Size and room checked, and the room made, above under the same
        // lock, so this is unreachable; if it ever were reached, the handles
        // are already out of the sender's table and the only safe thing left
        // is to free them, after the lock.
        match refused {
            None => {
                // After the queue lock is gone: a woken reader goes straight
                // for it, and a port's wake-up takes locks of its own. The
                // reader may close its end before this returns, and it is
                // closed when it does: this holds the channel, not that end.
                if fired {
                    deliver(|| peer.observers.lock().next_fired());
                }
                // The word's store before the wake's reads of the waiters'
                // states, against a waiter listed and not yet blocked: see
                // [`Endpoint::readable_or_closed`].
                fence(Ordering::SeqCst);
                // Onto this processor where it is free: a writer usually
                // waits for the answer next, and a reader woken elsewhere is
                // an interrupt to a processor that is likely halted. See
                // `sched::Wake::Sync`.
                wake_parked(parked, &peer.waiters, crate::sched::Wake::Sync);
                peer.waiters.wake_all_with(crate::sched::Wake::Sync);
                Ok(())
            }
            Some((why, message)) => {
                dispose(message.handles.into_iter().map(|(object, _)| object));
                Err(match why {
                    SendError::TooBig => WriteFailure::TooBig,
                    SendError::Full => WriteFailure::Full,
                    SendError::NoMemory => WriteFailure::NoMemory,
                })
            }
        }
    }

    /// Whether the other end's queue has room for one more message. Only
    /// meaningful to an end nobody else writes from, which the answer then
    /// stays true for until it writes: a reader only makes room.
    pub(crate) fn peer_has_room(&self) -> bool {
        let peer = self.peer();
        !peer.is_closed() && !peer.inbox.lock().is_full()
    }

    /// Take the next message, if it fits.
    ///
    /// A message carrying channel endpoints is taken only when the caller
    /// holds [`super::TOPOLOGY`], and the caller keeps holding it until the
    /// message is delivered or put back with [`Endpoint::unread`]. Putting one
    /// back re-adds edges to the graph the cycle check walks, and doing that
    /// while a send is walking is how two processes could build the cycle
    /// the check exists to refuse. A caller without the lock gets
    /// [`ReadError::NeedsTopology`], takes it, and asks again.
    ///
    /// # Errors
    ///
    /// [`ReadError`]. A message too large stays queued.
    pub(crate) fn read(
        &self,
        byte_capacity: usize,
        handle_capacity: usize,
        topology_held: bool,
    ) -> Result<ChannelMessage, ReadError> {
        // Looked at before the queue, not after it is found empty: a peer
        // writes its last message and then closes, so a peer seen closed
        // here has nothing left to land, and an empty queue below is the
        // end. Asked after, a peer that wrote and closed between the two
        // was reported closed with its last message still queued -- the
        // net ring's REFUSED lost that way, FX-1151 under WHPX. A close
        // that comes after this look reads as `Empty`, and the caller's
        // wait for `READABLE | PEER_CLOSED` returns at once to read again.
        let peer_closed = self.peer_closed();
        let (taken, was_full) = {
            let mut inbox = self.own().inbox.lock();
            let was_full = inbox.is_full();
            // Decided under the same lock as the pop, so the message looked at
            // is the message taken.
            let needs_topology =
                !topology_held && inbox.head_needs_topology(byte_capacity, handle_capacity);
            if needs_topology {
                return Err(ReadError::NeedsTopology);
            }
            let taken = inbox.pop_fitting(byte_capacity, handle_capacity);
            self.own().note(&inbox);
            (taken, was_full)
        };
        // A reader that makes room in a full queue is what a blocked writer
        // is waiting for; a closed peer has nobody waiting.
        if was_full && taken.is_ok() {
            self.peer().waiters.wake_all();
        }
        match taken {
            Ok(message) => Ok(message),
            Err(ReceiveError::TooSmall { bytes, handles }) => {
                Err(ReadError::TooSmall { bytes, handles })
            }
            Err(ReceiveError::Empty) if peer_closed => Err(ReadError::PeerClosed),
            Err(ReceiveError::Empty) => Err(ReadError::Empty),
        }
    }

    /// Send `bytes`, at most [`SMALL_BYTES`] and no handles, held in the
    /// peer's slot when nothing is waiting there and queued otherwise, and
    /// wake the reader onto this processor where it is free.
    ///
    /// `channel_write_read`'s write: the round trip's half that allocates
    /// nothing when the other side keeps up.
    ///
    /// # Errors
    ///
    /// [`WriteFailure`], never `Take`.
    pub(crate) fn write_small(&self, bytes: &[u8]) -> Result<(), WriteFailure<()>> {
        let peer = self.peer();
        let (fired, parked) = {
            let mut inbox = peer.inbox.lock();
            // As `write`: under the lock the close empties the queue under.
            if peer.is_closed() {
                return Err(WriteFailure::PeerClosed);
            }
            if bytes.len() > SMALL_BYTES || !inbox.accepts(bytes.len(), 0) {
                return Err(WriteFailure::TooBig);
            }
            if inbox.is_full() {
                return Err(WriteFailure::Full);
            }
            if !inbox.put_small(bytes) {
                // FALLIBLE: the queue's room, as in `write`.
                inbox.reserve().map_err(|_| WriteFailure::NoMemory)?;
                let mut queued =
                    fallible::try_filled(0_u8, bytes.len()).map_err(|_| WriteFailure::NoMemory)?;
                queued.copy_from_slice(bytes);
                let message = Message {
                    bytes: queued,
                    handles: Vec::new(),
                };
                // NOALLOC: the room `reserve` made above.
                inbox.push(message).map_err(|_| WriteFailure::NoMemory)?;
            }
            peer.note(&inbox);
            let fired = trigger(&mut peer.observers.lock(), Signals::READABLE);
            (fired, inbox.take_parked())
        };
        // As `write`, after the lock, with its fence.
        if fired {
            deliver(|| peer.observers.lock().next_fired());
        }
        crate::prof::stamp(crate::prof::Point::Written);
        fence(Ordering::SeqCst);
        wake_parked(parked, &peer.waiters, crate::sched::Wake::Sync);
        peer.waiters.wake_all_with(crate::sched::Wake::Sync);
        crate::prof::stamp(crate::prof::Point::Woken);
        Ok(())
    }

    /// Take the next message if it is small and carries no handles, held in
    /// place: `channel_write_read`'s read.
    ///
    /// # Errors
    ///
    /// [`ReadError::Empty`], [`ReadError::PeerClosed`], or
    /// [`ReadError::TooSmall`] for a message only `channel_read` can take,
    /// left where it was.
    pub(crate) fn read_small(&self) -> Result<Small, ReadError> {
        // Before the queue, for the reason `read` gives.
        let peer_closed = self.peer_closed();
        let (taken, was_full) = {
            let mut inbox = self.own().inbox.lock();
            let was_full = inbox.is_full();
            let taken = inbox.pop_small();
            self.own().note(&inbox);
            (taken, was_full)
        };
        if was_full && taken.is_ok() {
            self.peer().waiters.wake_all();
        }
        match taken {
            Ok(small) => Ok(small),
            Err(ReceiveError::TooSmall { bytes, handles }) => {
                Err(ReadError::TooSmall { bytes, handles })
            }
            Err(ReceiveError::Empty) if peer_closed => Err(ReadError::PeerClosed),
            Err(ReceiveError::Empty) => Err(ReadError::Empty),
        }
    }

    /// Whether a message is waiting on this end, or nothing more will come:
    /// what `channel_write_read`'s wait waits for. One load of this side's
    /// word, without the inbox lock (`docs/OPAQUE-KERNEL.md` §9.8, 2e).
    ///
    /// # Why no wake is lost
    ///
    /// A writer sets [`NONEMPTY`] under the inbox lock and a closing peer
    /// sets [`PEER_CLOSED`] under it; each then wakes this end's waiters,
    /// which drains them under the wait queue's lock and reads each one's
    /// state. The waiter lists itself under the wait queue's lock, stores
    /// `BLOCKED`, makes a `SeqCst` fence, and reads the word.
    /// - *The waker's drain after the waiter's listing*, and the waiter
    ///   already `BLOCKED` as the waker reads its state: woken.
    /// - *The drain before the listing*: the word's store came before the
    ///   waker's release of the wait queue's lock, which came before the
    ///   waiter's acquire of it, so the waiter's read sees the word.
    /// - *The drain after the listing, with the waiter not yet `BLOCKED`*
    ///   (the consultant's condition 5): the waker found it runnable and did
    ///   nothing, so the waiter's read must see the word. Each side is a
    ///   store then a load -- the word then the state, `BLOCKED` then the
    ///   word -- with a `SeqCst` fence between, the waker's before its wake
    ///   (`write`, `write_small`, `unread`, the close), the waiter's in
    ///   `WaitQueue::wait_sliced`. Of the two fences one comes first, and
    ///   the side whose fence comes second sees the other's store: the waker
    ///   finds the waiter `BLOCKED`, or the waiter finds the word.
    ///
    /// The model of all three is `src/tests/loom` (`cargo xtask loom`).
    pub(crate) fn readable_or_closed(&self) -> bool {
        self.own().word() != 0
    }

    /// Put back a message [`Endpoint::read`] took and the caller could not
    /// deliver, at the head of the queue.
    ///
    /// Holding [`super::TOPOLOGY`] if the message carries endpoints, as
    /// [`Endpoint::read`] required when it was taken.
    ///
    /// Wakes this end's waiters, because the message is readable again and a
    /// second reader may have gone to sleep while it was out.
    ///
    /// # Errors
    ///
    /// The message back, when a writer took its slot meanwhile and the queue
    /// could not grow to take it again. The caller disposes of what it
    /// carries once it holds no lock; its call was failing already.
    pub(crate) fn unread(&self, message: ChannelMessage) -> Result<(), ChannelMessage> {
        let parked = {
            let mut inbox = self.own().inbox.lock();
            let put_back = inbox.unpop(message);
            self.own().note(&inbox);
            put_back?;
            inbox.take_parked()
        };
        // As a write's: the word before the wake's reads of the states.
        fence(Ordering::SeqCst);
        wake_parked(parked, &self.own().waiters, crate::sched::Wake::Home);
        self.own().waiters.wake_all();
        Ok(())
    }

    /// Whether nobody holds the other end.
    pub(crate) fn peer_closed(&self) -> bool {
        self.peer().is_closed()
    }

    /// Queue a packet with `observer` the next time a message is readable on
    /// this end or its peer closes, or at once if either already holds.
    ///
    /// `READABLE` and `PEER_CLOSED` only. `WRITABLE` depends on the peer's
    /// queue, and taking that lock while holding this end's would take the
    /// two in the opposite order from the peer registering the other way
    /// round; the caller refuses it before reaching here.
    ///
    /// # Errors
    ///
    /// [`PortError::Full`] when this end already holds
    /// [`super::port::MAX_OBSERVERS`] registrations.
    pub(crate) fn observe(&self, observer: Observer) -> Result<(), PortError> {
        let own = self.own();
        let inbox = own.inbox.lock();
        let mut asserted = Signals::NONE;
        if !inbox.is_empty() {
            asserted = asserted | Signals::READABLE;
        }
        if self.peer_closed() {
            asserted = asserted | Signals::PEER_CLOSED;
        }
        if observer.wants(asserted) {
            drop(inbox);
            observer.fire(asserted);
            return Ok(());
        }
        own.observed.store(true, Ordering::Release);
        let registered = register(&mut own.observers.lock(), observer);
        drop(inbox);
        registered
    }

    /// Whether a reader waits on this end: listed on its queue, or parked
    /// by the fast path's receive half (`docs/OPAQUE-KERNEL.md` §9.7). For
    /// the checks, which wake a reader once it waits.
    pub(crate) fn reader_waiting(&self) -> bool {
        if self.own().inbox.lock().parked.is_some() {
            return true;
        }
        self.own().waiters.listed() != 0
    }

    /// The queue woken when this end's signals may have changed.
    pub(crate) fn waiters(&self) -> &WaitQueue {
        &self.own().waiters
    }

    /// What a waiter on this end would see now.
    pub(crate) fn signals(&self) -> Signals {
        let mut signals = Signals::NONE;
        if !self.own().inbox.lock().is_empty() {
            signals = signals | Signals::READABLE;
        }
        let peer = self.peer();
        if peer.is_closed() {
            signals | Signals::PEER_CLOSED
        } else if !peer.inbox.lock().is_full() {
            signals | Signals::WRITABLE
        } else {
            signals
        }
    }

    /// The identity the cycle walk knows this end by: its side's address,
    /// which the peer can name too without holding it.
    fn identity(&self) -> usize {
        core::ptr::from_ref(self.own()) as usize
    }
}

/// Wake the parked reader a general write or close took off its record, if
/// there was one, after the inbox lock is let go: as `how`, which is what the
/// same write's wait queue is woken with.
///
/// A park a wake ends is a wait on `queue`'s end ended by a wake, and is
/// counted there as one (`WaitQueue::waits_ended_by_a_wake`).
fn wake_parked(parked: Option<Arc<Task>>, queue: &WaitQueue, how: crate::sched::Wake) {
    if let Some(task) = parked {
        queue.note_ended_by_a_wake();
        crate::sched::wake_with(&task, how);
    }
}

impl Endpoint {
    /// The fast path's send half and the direct switch
    /// (`docs/OPAQUE-KERNEL.md` §9.7, part 2): deliver `len` bytes in
    /// `words`, the bytes past `len` already zero, into the reply cell of the
    /// task parked on the peer's side; park `caller`, the running task, on
    /// this side; and hand the processor to the peer. Returns when `caller`
    /// runs again, which is when another commit hands it a reply or a general
    /// wake ends its park; it then takes its reply, or finds none and carries
    /// on as the general path does.
    ///
    /// With interrupts masked. Every lock is taken by `try_lock`, a held one
    /// a decline: the two halves by side, first before second, whichever end
    /// the caller holds; under the peer's inbox its observers and its wait
    /// queue; then this processor's run queue (`sched::direct::begin`).
    ///
    /// # Errors
    ///
    /// The test that declined (T6 to T13, or a lock held), with nothing
    /// changed: the general path runs the call from its start.
    ///
    /// # Safety
    ///
    /// (CONTEXT) Interrupts are masked on this processor from before the call
    /// until the switch: the halves' locks are held under that mask, counted
    /// for A3 but without the site record or the deferred decision
    /// (`sync::try_lock_masked`).
    pub(crate) unsafe fn send_direct(
        &self,
        caller: &Task,
        len: usize,
        words: [u64; 3],
    ) -> Result<(), crate::sched::direct::Count> {
        use crate::sched::direct::{self, Count};
        let (own, peer) = (self.own(), self.peer());
        let (first, second) = match self.side {
            Side::First => (own, peer),
            Side::Second => (peer, own),
        };
        // SAFETY: (CONTEXT) interrupts are masked from the system call's
        // entry until the switch, both guards drop before `switch`, and
        // nothing under them blocks: every lock below is a `try_lock`.
        let first = unsafe { crate::sync::try_lock_masked(&first.inbox) }.ok_or(Count::Halves)?;
        // SAFETY: (CONTEXT) as for `first`.
        let second = unsafe { crate::sync::try_lock_masked(&second.inbox) }.ok_or(Count::Halves)?;
        let (mut own_inbox, mut peer_inbox) = match self.side {
            Side::First => (first, second),
            Side::Second => (second, first),
        };
        let reader = sendable(own, &own_inbox, peer, &peer_inbox)?;
        let mut switch = direct::begin(caller, reader)?;
        // The commit, which cannot fail from here.
        let Some(reader) = peer_inbox.parked.take() else {
            return Err(Count::T9);
        };
        reader.fill_reply(len, words);
        switch.hand_over(caller, reader, |parked| {
            direct::set_running_blocked(&parked);
            own_inbox.parked = Some(parked);
        });
        drop(peer_inbox);
        drop(own_inbox);
        direct::count(Count::Trip);
        switch.switch();
        Ok(())
    }

    /// The receive half alone (`docs/OPAQUE-KERNEL.md` §9.7, part 2): park
    /// `task`, the running task, on this side and set it blocked, if the
    /// inbox is empty, nothing is parked here and the peer has not closed
    /// (T6 to T8, under this side's lock, the order a writer and a close
    /// read it in). Answers whether it parked; the caller then makes the
    /// last look and blocks (`sched::direct::block_parked`).
    pub(crate) fn park(&self, task: &Arc<Task>) -> bool {
        let own = self.own();
        let mut inbox = own.inbox.lock();
        if !inbox.is_empty() || inbox.parked.is_some() || own.word() & PEER_CLOSED != 0 {
            return false;
        }
        inbox.parked = Some(Arc::clone(task));
        crate::sched::direct::set_blocked(task);
        crate::sched::direct::count(crate::sched::direct::Count::Park);
        true
    }

    /// Clear this side's record if it still names `task`: a parked task
    /// leaving its park by any way but a commit, which cleared it already.
    pub(crate) fn unpark(&self, task: &Task) {
        let taken = {
            let mut inbox = self.own().inbox.lock();
            if inbox
                .parked
                .as_ref()
                .is_some_and(|parked| core::ptr::eq(Arc::as_ptr(parked), task))
            {
                inbox.parked.take()
            } else {
                None
            }
        };
        drop(taken);
    }
}

/// The send half's tests under the two halves' locks
/// (`docs/OPAQUE-KERNEL.md` §9.7, part 2): the caller could park on its own
/// side -- its inbox empty (T6), nothing parked there (T7), its peer open
/// (T8) -- and a reader is parked on the peer's side (T9) with nobody else
/// to be told of the message (T10). Answers the reader. A record beside a
/// message or on a closed end stops the machine (A2, FX-0531).
fn sendable<'a>(
    own: &Half,
    own_inbox: &Inbox,
    peer: &Half,
    peer_inbox: &'a Inbox,
) -> Result<&'a Arc<Task>, crate::sched::direct::Count> {
    use crate::sched::direct::Count;
    if !own_inbox.is_empty() {
        return Err(Count::T6);
    }
    if own_inbox.parked.is_some() {
        return Err(Count::T7);
    }
    if own.word() & PEER_CLOSED != 0 {
        return Err(Count::T8);
    }
    let Some(reader) = peer_inbox.parked.as_ref() else {
        return Err(Count::T9);
    };
    // A2: a record is set only beside an empty inbox, and every writer that
    // fills one takes it; the parked reader's call holds its end open.
    if !peer_inbox.is_empty() || peer.is_closed() {
        crate::panic::fatal!(
            crate::panic::catalog::FAST_PATH_PARK_BROKEN,
            "a reader was parked beside a message or on a closed end (A2): task {}",
            reader.id
        );
    }
    // T10, read under the peer's inbox lock: no port registration was ever
    // made on its side (set under that lock) and nobody is listed on its
    // queue.
    if peer.observed.load(Ordering::Acquire) || peer.waiters.listed_now() != 0 {
        return Err(Count::T10);
    }
    Ok(reader)
}

/// Whether sending `carried` through `writer` would close a cycle.
///
/// The message lands in the writer's peer's inbox, so it closes a cycle
/// exactly when that peer is reachable from something it carries, following
/// each endpoint into the endpoints queued in its own inbox. The peer itself
/// among `carried` is the one-step case.
///
/// Call it holding [`super::TOPOLOGY`], and make the send before releasing
/// it: the answer is only about a graph nothing else is adding edges to.
///
/// [`Reach::NoMemory`] when the walk ran out of memory, which the caller
/// refuses as it does a walk too long.
pub(crate) fn check_carry(writer: &Endpoint, carried: Vec<Arc<Endpoint>>) -> Reach {
    // A closed peer is no cycle, and the write itself will say it is closed.
    if writer.peer_closed() {
        return Reach::Clear;
    }
    reaches(
        carried,
        identity,
        core::ptr::from_ref(writer.peer()) as usize,
        queued_endpoints,
        MAX_WALK,
    )
}

/// An endpoint's identity, which is how the walk tells endpoints apart: see
/// [`Endpoint::identity`]. Stable for as long as the walk holds the `Arc`,
/// which it does until it returns.
fn identity(endpoint: &Arc<Endpoint>) -> usize {
    endpoint.identity()
}

/// The endpoints queued, unread, in `endpoint`'s inbox: the edges the cycle
/// walk follows.
///
/// The match is exhaustive on purpose. An object kind added later that can
/// hold other objects has to be followed here, or it is a way round the check,
/// and the compiler is what asks the question.
fn queued_endpoints(endpoint: &Arc<Endpoint>) -> Option<Vec<Arc<Endpoint>>> {
    let inbox = endpoint.own().inbox.lock();
    // Identities seen, sorted: a vector rather than a set, so that growing it
    // can be refused.
    let mut distinct: Vec<usize> = Vec::new();
    let mut queued_ends = Vec::new();
    let carried = inbox
        .iter()
        .flat_map(|message| message.handles.iter())
        .filter_map(|(object, _)| match object {
            Object::Channel(queued) => Some(queued),
            Object::Vmo(_)
            | Object::Job(_)
            | Object::Device(_)
            | Object::Interrupt(_)
            | Object::IoMapping(_)
            | Object::Pin(_)
            // A process handle holds how the process ended, not its table.
            | Object::Process(_)
            | Object::Port(_)
            | Object::Starter
            | Object::Audit => None,
        });
    for queued in carried {
        // Once each, and no more than the walk could use: an inbox can hold
        // two hundred and fifty-six messages of sixty-four handles, and
        // cloning sixteen thousand references under the topology lock to
        // find the walk was too far anyway would be the cost the bound is
        // there to prevent.
        if queued_ends.len() > MAX_WALK {
            break;
        }
        let identity = queued.identity();
        if let Err(at) = distinct.binary_search(&identity) {
            fallible::try_insert_at(&mut distinct, at, identity).ok()?;
            fallible::try_push(&mut queued_ends, Arc::clone(queued)).ok()?;
        }
    }
    Some(queued_ends)
}

/// Whether a message carries a channel endpoint.
fn carries_endpoints(message: &ChannelMessage) -> bool {
    message
        .handles
        .iter()
        .any(|(object, _)| matches!(object, Object::Channel(_)))
}

impl Drop for Endpoint {
    /// Close this side, tell the peer it is alone, and free what was queued
    /// and never read, one level at a time.
    fn drop(&mut self) {
        let unread = self.take_unread();
        let peer = self.peer();
        // Under the survivor's inbox lock, the lock its registrations are
        // made under, so one made a moment ago is found here.
        let (fired, parked) = {
            let mut inbox = peer.inbox.lock();
            // After this side's `closed` (`take_unread`), and before the
            // wake, under the survivor's inbox lock: the word's invariant.
            peer.note_peer_closed();
            let fired = trigger(&mut peer.observers.lock(), Signals::PEER_CLOSED);
            (fired, inbox.take_parked())
        };
        if fired {
            deliver(|| peer.observers.lock().next_fired());
        }
        // As a write's: the word before the wake's reads of the states.
        fence(Ordering::SeqCst);
        wake_parked(parked, &peer.waiters, crate::sched::Wake::Home);
        peer.waiters.wake_all();
        dispose(
            unread
                .into_iter()
                .flat_map(|message| message.handles.into_iter().map(|(object, _)| object)),
        );
    }
}

impl Endpoint {
    /// Close this side and take out what the messages queued for it and
    /// never read carry: what closing it has to free, and what [`dispose`]
    /// queues rather than drop inside the close.
    ///
    /// Closed from here on, though the `Endpoint` is not dropped yet: the
    /// peer sees `PEER_CLOSED`, and a write to this side is refused rather
    /// than left in a queue nobody reads. Its registrations go too; the ports
    /// they name are held weakly, so letting them go frees no object.
    ///
    /// The queue moves out whole, to be taken apart as it is walked: this
    /// runs as an end closes, where nothing can be told memory ran out.
    pub(super) fn take_unread(&self) -> VecDeque<ChannelMessage> {
        let own = self.own();
        let messages = {
            let mut inbox = own.inbox.lock();
            own.closed.store(true, Ordering::Release);
            let messages = inbox.drain();
            own.note(&inbox);
            messages
        };
        let registrations = core::mem::take(&mut *own.observers.lock());
        drop(registrations);
        messages
    }
}
