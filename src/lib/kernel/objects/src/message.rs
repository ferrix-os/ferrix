//! One direction of a channel: a bounded queue of messages.
//!
//! A channel is two of these, one per endpoint, each holding what the *other*
//! endpoint wrote. This is the queue and its rules; the kernel adds the lock,
//! the peer-closed state, and the wake-ups.
//!
//! # Bounded, so a writer can be told to wait
//!
//! An unbounded queue lets a driver that stops reading make its peer allocate
//! kernel memory until something else fails. With a bound, the write that
//! would exceed it is refused with [`SendError::Full`] — `SHOULD_WAIT` to the
//! program — and the writer waits for `WRITABLE`, which is backpressure rather
//! than an out-of-memory condition somewhere unrelated.
//!
//! # A read that does not fit takes nothing
//!
//! If the next message is larger than the buffers offered, it stays at the
//! head of the queue and the sizes it needs are reported. The alternative —
//! truncating, or discarding — loses handles, and a lost handle is a leaked
//! object the program can never close.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// A message: bytes, objects travelling with them, and who sent it.
///
/// `S` is the sender's identity as the queue's owner records it: the
/// kernel's channels stamp it at the write (`docs/microkernel/K6.md`), and
/// the queue only carries it. Every message is made with one, so none is
/// queued without it, and no operation here changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message<H, S = ()> {
    /// The data.
    pub bytes: Vec<u8>,
    /// The objects, in the order the sender listed their handles.
    pub handles: Vec<H>,
    /// Who wrote it, as the writer's side recorded it.
    pub sender: S,
}

/// How much a queue carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes in one message.
    pub max_bytes: usize,
    /// The most handles in one message.
    pub max_handles: usize,
    /// The most messages waiting at once.
    pub max_queued: usize,
}

/// Why a message was not queued. The message comes back with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// Larger than any message this queue carries. Waiting will not help.
    TooBig,
    /// The queue is full. Waiting for the reader will.
    Full,
    /// There was no memory to queue it.
    NoMemory,
}

/// Why nothing was received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveError {
    /// The queue is empty.
    Empty,
    /// The next message needs more room than was offered, and is still
    /// queued.
    TooSmall {
        /// Its size in bytes.
        bytes: usize,
        /// How many handles it carries.
        handles: usize,
    },
}

/// A bounded queue of messages.
#[derive(Debug)]
pub struct MessageQueue<H, S = ()> {
    /// Oldest first.
    queue: VecDeque<Message<H, S>>,
    /// What it will accept.
    limits: Limits,
}

impl<H, S> MessageQueue<H, S> {
    /// An empty queue.
    #[must_use]
    pub fn new(limits: Limits) -> MessageQueue<H, S> {
        MessageQueue {
            queue: VecDeque::new(),
            limits,
        }
    }

    /// How many messages are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether nothing is waiting: the reader's `READABLE` is the negation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Whether a write would be refused as full: the writer's `WRITABLE` is
    /// the negation.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.queue.len() >= self.limits.max_queued
    }

    /// Whether a message of this shape could ever be sent. Asked before
    /// anything is taken from the sender, so a message that can never fit
    /// costs the sender nothing.
    #[must_use]
    pub fn accepts(&self, bytes: usize, handles: usize) -> bool {
        bytes <= self.limits.max_bytes && handles <= self.limits.max_handles
    }

    /// Make room for one more message, so that the [`MessageQueue::push`]
    /// after it cannot be refused for memory. Asked before anything is taken
    /// from the sender, as [`MessageQueue::accepts`] is, so a message there
    /// is no memory for costs the sender nothing.
    ///
    /// # Errors
    ///
    /// [`SendError::NoMemory`].
    pub fn reserve(&mut self) -> Result<(), SendError> {
        ferrix_fallible::try_reserve_deque(&mut self.queue, 1).map_err(|_| SendError::NoMemory)
    }

    /// Queue a message.
    ///
    /// # Errors
    ///
    /// The reason, and the message back untouched.
    pub fn push(&mut self, message: Message<H, S>) -> Result<(), (SendError, Message<H, S>)> {
        if !self.accepts(message.bytes.len(), message.handles.len()) {
            return Err((SendError::TooBig, message));
        }
        if self.is_full() {
            return Err((SendError::Full, message));
        }
        if ferrix_fallible::try_reserve_deque(&mut self.queue, 1).is_err() {
            return Err((SendError::NoMemory, message));
        }
        self.queue.push_back(message);
        Ok(())
    }

    /// Every waiting message, oldest first, without taking any.
    ///
    /// For the kernel's cycle check, which has to see which objects a queue
    /// is holding.
    pub fn iter(&self) -> impl Iterator<Item = &Message<H, S>> + '_ {
        self.queue.iter()
    }

    /// The size of the next message, without taking it.
    #[must_use]
    pub fn peek_sizes(&self) -> Option<(usize, usize)> {
        self.queue
            .front()
            .map(|message| (message.bytes.len(), message.handles.len()))
    }

    /// Take the next message, if it fits in `byte_capacity` bytes and
    /// `handle_capacity` handles.
    ///
    /// # Errors
    ///
    /// [`ReceiveError::Empty`], or [`ReceiveError::TooSmall`] with the message
    /// left where it was.
    pub fn pop_fitting(
        &mut self,
        byte_capacity: usize,
        handle_capacity: usize,
    ) -> Result<Message<H, S>, ReceiveError> {
        let (bytes, handles) = self.peek_sizes().ok_or(ReceiveError::Empty)?;
        if bytes > byte_capacity || handles > handle_capacity {
            return Err(ReceiveError::TooSmall { bytes, handles });
        }
        self.queue.pop_front().ok_or(ReceiveError::Empty)
    }

    /// Put a message back at the head, as if it had never been taken.
    ///
    /// For the kernel's read path, which takes a message and then may fail to
    /// deliver it — the reader's handle table filled up, or its buffer turned
    /// out not to be writable. Putting it back at the *head* is what keeps
    /// message order intact across that failure. Ignores the bound: the slot
    /// was this message's a moment ago.
    ///
    /// # Errors
    ///
    /// The message back, when the queue had to grow to take it and there was
    /// no memory: a writer took its slot meanwhile.
    pub fn unpop(&mut self, message: Message<H, S>) -> Result<(), Message<H, S>> {
        // A write may have taken the slot the message came out of, so the
        // queue can need to grow to take it back.
        if ferrix_fallible::try_reserve_deque(&mut self.queue, 1).is_err() {
            return Err(message);
        }
        self.queue.push_front(message);
        Ok(())
    }

    /// Take everything, for when the reading endpoint closes and no one will
    /// ever read these. The caller drops them, and the objects with them,
    /// outside whatever lock guards the queue.
    pub fn drain(&mut self) -> VecDeque<Message<H, S>> {
        // Moved out whole rather than collected: this runs as an endpoint
        // closes, where there is nobody to tell that memory ran out.
        core::mem::take(&mut self.queue)
    }
}
