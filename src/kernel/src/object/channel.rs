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

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::SpinLock;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES};
use ferrix_objects::message::{Limits, Message, MessageQueue, ReceiveError, SendError};
use ferrix_objects::reach::{Reach, reaches};

use super::port::{Observer, Observers, PortError, deliver, register, trigger};
use super::{Object, Transfer, dispose};
use crate::fallible::{self, AllocError};
use crate::object::quota::{Charge, Resource};
use crate::sched::WaitQueue;

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
    inbox: SpinLock<MessageQueue<Transfer>>,
    /// Woken when this side's signals may have changed: a message arrived, the
    /// other side's queue gained room, or the other side closed.
    waiters: WaitQueue,
    /// Port registrations waiting on this side's signals. Taken only inside
    /// `inbox`'s lock, which is what serialises a registration against the
    /// change it waits for.
    observers: SpinLock<Observers>,
    /// Whether this side's [`Endpoint`] has gone. Set once, under `inbox`'s
    /// lock as the queue is emptied, and never cleared.
    closed: AtomicBool,
}

impl Half {
    /// An open side with nothing queued.
    fn new() -> Half {
        Half {
            inbox: SpinLock::new(MessageQueue::new(LIMITS)),
            waiters: WaitQueue::new(),
            observers: SpinLock::new(Observers::new()),
            closed: AtomicBool::new(false),
        }
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
        let (refused, fired) = {
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
            let fired = refused.is_none() && trigger(&mut peer.observers.lock(), Signals::READABLE);
            (refused, fired)
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
                peer.waiters.wake_all();
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
            let needs_topology = !topology_held
                && inbox.iter().next().is_some_and(|head| {
                    head.bytes.len() <= byte_capacity
                        && head.handles.len() <= handle_capacity
                        && carries_endpoints(head)
                });
            if needs_topology {
                return Err(ReadError::NeedsTopology);
            }
            (inbox.pop_fitting(byte_capacity, handle_capacity), was_full)
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
        self.own().inbox.lock().unpop(message)?;
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
        let registered = register(&mut own.observers.lock(), observer);
        drop(inbox);
        registered
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
            | Object::WindowServer(_)
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
        let fired = {
            let _inbox = peer.inbox.lock();
            trigger(&mut peer.observers.lock(), Signals::PEER_CLOSED)
        };
        if fired {
            deliver(|| peer.observers.lock().next_fired());
        }
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
            inbox.drain()
        };
        let registrations = core::mem::take(&mut *own.observers.lock());
        drop(registrations);
        messages
    }
}
