//! Channels: messages of bytes and handles between two endpoints.

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::types::{
    CHANNEL_MAX_HANDLES, TICKET_ADOPT, TICKET_END, TICKET_ISSUE, TICKET_LEAVE, TicketIssuer,
};

use crate::call::{Call, Syscall};
use crate::error::{Error, decode, decode_unit};
use crate::handle::{Object, OwnedHandle, object_handle, register};

/// A handle's width in a message's handle buffer.
const HANDLE_BYTES: usize = size_of::<u32>();

/// The handle buffer one message can need, in bytes.
const HANDLE_BUFFER: usize = CHANNEL_MAX_HANDLES * HANDLE_BYTES;

object_handle!(
    /// One endpoint of a channel.
    Channel
);

/// What a read delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    /// Bytes written into the byte buffer.
    pub bytes: usize,
    /// Handles written into the handle buffer, now held by this process.
    pub handles: usize,
}

/// Who wrote a message, as the kernel stamped it at the write
/// (`docs/microkernel/K6.md`): never taken from the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Writer {
    /// The writing process's id, or zero for the kernel. An identity, not
    /// credentials: a process keeps its id across `execve`, a set-id one
    /// included, so a privilege is never cached by it across an exec.
    pub process: u64,
    /// The id of the job it was in at the write, or zero for the kernel.
    pub job: u64,
}

/// Why a read delivered nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// The next message needs more room than was offered. It stays queued.
    TooSmall {
        /// Its size in bytes.
        bytes: usize,
        /// How many handles it carries.
        handles: usize,
    },
    /// Any other failure: [`Error::ShouldWait`] when there is nothing to read,
    /// [`Error::PeerClosed`] when nothing more will come.
    Failed(Error),
}

/// `channel_create`: a channel's two endpoints.
///
/// # Errors
///
/// [`Error::NoMemory`], [`Error::NoHandles`].
pub fn create<S: Syscall>(sys: S) -> Result<(Channel<S>, Channel<S>), Error> {
    let mut out = [0_u8; 2 * HANDLE_BYTES];
    decode_unit(Call::new(nr::CHANNEL_CREATE).output(&mut out).make(sys))?;
    let [a0, a1, a2, a3, b0, b1, b2, b3] = out;
    let first = OwnedHandle::from_raw(sys, Handle(u32::from_ne_bytes([a0, a1, a2, a3])));
    let second = OwnedHandle::from_raw(sys, Handle(u32::from_ne_bytes([b0, b1, b2, b3])));
    if !first.raw().is_valid() || !second.raw().is_valid() {
        return Err(Error::Unexpected(0));
    }
    Ok((Channel::from_owned(first), Channel::from_owned(second)))
}

/// The most bytes [`Channel::write_read`] carries each way: three words.
pub const WRITE_READ_BYTES: usize = nr::CHANNEL_WRITE_READ_BYTES;

/// A message [`Channel::write_read`] received: its size, and its bytes in
/// three words as they would lie in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Words {
    /// How many of the words' bytes are the message.
    pub len: usize,
    /// The words.
    pub words: [usize; 3],
}

impl Words {
    /// The words' bytes; the first [`Words::len`] are the message.
    #[must_use]
    #[inline]
    pub fn bytes(&self) -> [u8; WRITE_READ_BYTES] {
        let mut bytes = [0_u8; WRITE_READ_BYTES];
        for (chunk, word) in bytes.chunks_exact_mut(size_of::<usize>()).zip(self.words) {
            chunk.copy_from_slice(&word.to_ne_bytes());
        }
        bytes
    }

    /// `bytes` as three words, if they fit.
    ///
    /// Word by word, whole words read as words and a short last one a byte
    /// at a time, rather than one copy of a length known only at run time:
    /// that copy is a call to `memcpy` through the GOT on every send, two
    /// more pages of the program touched (`decode`'s note). The compiler may
    /// still make the short last word's bytes a call; a message of whole
    /// words, as an echo of a word is, makes none.
    #[inline]
    pub(crate) fn of(bytes: &[u8]) -> Option<[usize; 3]> {
        if bytes.len() > WRITE_READ_BYTES {
            return None;
        }
        let mut words = [0_usize; 3];
        let mut chunks = bytes.chunks(size_of::<usize>());
        for word in &mut words {
            let Some(chunk) = chunks.next() else { break };
            *word = match <[u8; size_of::<usize>()]>::try_from(chunk) {
                Ok(whole) => usize::from_ne_bytes(whole),
                Err(_) => {
                    let mut raw = [0_u8; size_of::<usize>()];
                    for (to, from) in raw.iter_mut().zip(chunk) {
                        *to = *from;
                    }
                    usize::from_ne_bytes(raw)
                }
            };
        }
        Some(words)
    }
}

impl<S: Syscall> Channel<S> {
    /// `channel_write_read`: send `send`, if given, at most
    /// [`WRITE_READ_BYTES`] and no handles, then wait for the next message on
    /// this end and receive it, both ways in registers. One call where a
    /// round trip was a write, a wait and a read on each side.
    ///
    /// # Errors
    ///
    /// [`Error::TooBig`] for more than [`WRITE_READ_BYTES`] to send;
    /// [`Error::BufferTooSmall`] for a message only [`Channel::read`] can
    /// take; [`Error::PeerClosed`]; whatever a write is refused for;
    /// [`Error::Unsupported`] where the way this handle makes calls cannot
    /// read registers back ([`Syscall::call_words`]).
    #[inline]
    pub fn write_read(&self, send: Option<&[u8]>) -> Result<Words, Error> {
        let (count, words) = match send {
            Some(bytes) => (bytes.len(), Words::of(bytes).ok_or(Error::TooBig)?),
            None => (nr::WRITE_READ_NOTHING, [0; 3]),
        };
        let [w0, w1, w2] = words;
        let (value, back) = Call::new(nr::CHANNEL_WRITE_READ)
            .value(register(self.handle()))
            .value(count)
            .value(w0)
            .value(w1)
            .value(w2)
            .make_words(self.syscall())
            .ok_or(Error::Unsupported)?;
        let len = decode(value)?;
        Ok(Words { len, words: back })
    }

    /// `channel_write_read` with a count and words as given, unchecked: what
    /// a program that must send a count [`Channel::write_read`] refuses -- 25,
    /// say -- calls, to see the kernel refuse it (`ipc-equiv`'s case 12).
    ///
    /// # Errors
    ///
    /// As [`Channel::write_read`], the kernel's answer to whatever was sent.
    pub fn write_read_unchecked(&self, count: usize, words: [usize; 3]) -> Result<Words, Error> {
        let [w0, w1, w2] = words;
        let (value, back) = Call::new(nr::CHANNEL_WRITE_READ)
            .value(register(self.handle()))
            .value(count)
            .value(w0)
            .value(w1)
            .value(w2)
            .make_words(self.syscall())
            .ok_or(Error::Unsupported)?;
        let len = decode(value)?;
        Ok(Words { len, words: back })
    }

    /// `charge_ticket(ISSUE)`: a ticket for this process's own job on this
    /// end, for the server at the other end to adopt while it works on one
    /// request (`docs/microkernel/K7.md`). Its id, to send with the request.
    /// End it with [`Channel::end_ticket`] when the reply comes or the
    /// request is given up.
    ///
    /// # Errors
    ///
    /// [`Error::ShouldWait`] past `MAX_TICKETS` live on this end;
    /// [`Error::PeerClosed`]; [`Error::NoMemory`]; [`Error::AccessDenied`]
    /// without `WRITE`.
    pub fn issue_ticket(&self) -> Result<u64, Error> {
        let mut id = [0_u8; 8];
        let value = Call::new(nr::CHARGE_TICKET)
            .value(TICKET_ISSUE as usize)
            .value(register(self.handle()))
            .value(0)
            .value(0)
            .output(&mut id)
            .make(self.syscall());
        decode_unit(value).map(|()| u64::from_ne_bytes(id))
    }

    /// `charge_ticket(ADOPT)`: this thread's native calls charge what they
    /// make to the job of ticket `id`, which the other end issued, until
    /// [`Channel::leave_ticket`] or the ticket's end. Its issuer, as the
    /// kernel recorded it.
    ///
    /// # Errors
    ///
    /// [`Error::BadState`] for an id the other end did not issue or that has
    /// ended; [`Error::AccessDenied`] without `READ`.
    pub fn adopt_ticket(&self, id: u64) -> Result<TicketIssuer, Error> {
        let mut issuer = [0_u8; 16];
        let value = Call::new(nr::CHARGE_TICKET)
            .value(TICKET_ADOPT as usize)
            .value(register(self.handle()))
            .value(low(id))
            .value(high(id))
            .output(&mut issuer)
            .make(self.syscall());
        decode_unit(value)?;
        let word = |at: usize| {
            let mut raw = [0_u8; 8];
            if let Some(from) = issuer.get(at..at + 8) {
                raw.copy_from_slice(from);
            }
            u64::from_ne_bytes(raw)
        };
        Ok(TicketIssuer {
            process: word(0),
            job: word(8),
        })
    }

    /// `charge_ticket(LEAVE)`: this thread charges its own job again.
    ///
    /// # Errors
    ///
    /// None the kernel answers; [`Error`] only if the call itself is
    /// unsupported.
    pub fn leave_ticket(&self) -> Result<(), Error> {
        let value = Call::new(nr::CHARGE_TICKET)
            .value(TICKET_LEAVE as usize)
            .value(register(self.handle()))
            .value(0)
            .value(0)
            .value(0)
            .make(self.syscall());
        decode_unit(value)
    }

    /// `charge_ticket(END)`: end ticket `id`, from either end: the client
    /// when the reply came or it gave up, the server as it replies.
    ///
    /// # Errors
    ///
    /// [`Error::BadState`] for an id with no live ticket on this channel.
    pub fn end_ticket(&self, id: u64) -> Result<(), Error> {
        let value = Call::new(nr::CHARGE_TICKET)
            .value(TICKET_END as usize)
            .value(register(self.handle()))
            .value(low(id))
            .value(high(id))
            .value(0)
            .make(self.syscall());
        decode_unit(value)
    }

    /// `channel_write` of bytes alone.
    ///
    /// # Errors
    ///
    /// [`Error::PeerClosed`]; [`Error::ShouldWait`] when the peer's queue is
    /// full; [`Error::TooBig`]; [`Error::AccessDenied`] without `WRITE`.
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error> {
        let value = Call::new(nr::CHANNEL_WRITE)
            .value(register(self.handle()))
            .input(bytes)
            .value(bytes.len())
            .value(0)
            .value(0)
            .make(self.syscall());
        decode_unit(value)
    }

    /// `channel_write` of bytes and handles.
    ///
    /// The handles leave this process only if the write succeeds, so a failure
    /// gives every one of them back.
    ///
    /// # Errors
    ///
    /// As [`Channel::write`], and [`Error::AccessDenied`] for a handle without
    /// `TRANSFER`, [`Error::InvalidArgs`] for this endpoint itself or a cycle.
    pub fn write_with<const N: usize>(
        &self,
        bytes: &[u8],
        handles: [OwnedHandle<S>; N],
    ) -> Result<(), (Error, [OwnedHandle<S>; N])> {
        let mut buffer = [0_u8; HANDLE_BUFFER];
        let Some(values) = N
            .checked_mul(HANDLE_BYTES)
            .and_then(|len| buffer.get_mut(..len))
        else {
            return Err((Error::TooBig, handles));
        };
        for (slot, handle) in values.chunks_exact_mut(HANDLE_BYTES).zip(&handles) {
            slot.copy_from_slice(&handle.raw().0.to_ne_bytes());
        }
        let value = Call::new(nr::CHANNEL_WRITE)
            .value(register(self.handle()))
            .input(bytes)
            .value(bytes.len())
            .input(values)
            .value(N)
            .make(self.syscall());
        match decode_unit(value) {
            Ok(()) => {
                for handle in handles {
                    let _ = handle.into_raw();
                }
                Ok(())
            }
            Err(error) => Err((error, handles)),
        }
    }

    /// `channel_read`: the next message, into `bytes` and `handles`.
    ///
    /// The handles written are this process's from then on; adopt each one
    /// with [`OwnedHandle::from_raw`] so it is closed. At most
    /// `CHANNEL_MAX_HANDLES` of `handles` are offered, which is all a message
    /// can carry.
    ///
    /// # Errors
    ///
    /// [`ReadError::TooSmall`], or [`ReadError::Failed`].
    pub fn read(&self, bytes: &mut [u8], handles: &mut [Handle]) -> Result<Received, ReadError> {
        let mut values = [0_u8; HANDLE_BUFFER];
        let capacity = handles.len().min(CHANNEL_MAX_HANDLES);
        let byte_capacity = bytes.len();
        let mut actual = [0_u8; 8];
        let value = Call::new(nr::CHANNEL_READ)
            .value(register(self.handle()))
            .output(bytes)
            .value(byte_capacity)
            .output(
                values
                    .get_mut(..capacity * HANDLE_BYTES)
                    .unwrap_or_default(),
            )
            .value(capacity)
            .output(&mut actual)
            .make(self.syscall());
        let [b0, b1, b2, b3, h0, h1, h2, h3] = actual;
        let message_bytes = u32::from_ne_bytes([b0, b1, b2, b3]) as usize;
        let message_handles = u32::from_ne_bytes([h0, h1, h2, h3]) as usize;
        match decode_unit(value) {
            Ok(()) => {}
            Err(Error::BufferTooSmall) => {
                return Err(ReadError::TooSmall {
                    bytes: message_bytes,
                    handles: message_handles,
                });
            }
            Err(error) => return Err(ReadError::Failed(error)),
        }
        let received = message_handles.min(capacity);
        for (slot, word) in handles
            .iter_mut()
            .zip(values.chunks_exact(HANDLE_BYTES))
            .take(received)
        {
            if let Ok(word) = <[u8; HANDLE_BYTES]>::try_from(word) {
                *slot = Handle(u32::from_ne_bytes(word));
            }
        }
        Ok(Received {
            bytes: message_bytes,
            handles: received,
        })
    }

    /// `channel_read_attested`: [`Channel::read`], and who wrote the message.
    ///
    /// # Errors
    ///
    /// As [`Channel::read`].
    pub fn read_attested(
        &self,
        bytes: &mut [u8],
        handles: &mut [Handle],
    ) -> Result<(Received, Writer), ReadError> {
        let mut values = [0_u8; HANDLE_BUFFER];
        let capacity = handles.len().min(CHANNEL_MAX_HANDLES);
        let byte_capacity = bytes.len();
        let mut actual = [0_u8; 24];
        let value = Call::new(nr::CHANNEL_READ_ATTESTED)
            .value(register(self.handle()))
            .output(bytes)
            .value(byte_capacity)
            .output(
                values
                    .get_mut(..capacity * HANDLE_BYTES)
                    .unwrap_or_default(),
            )
            .value(capacity)
            .output(&mut actual)
            .make(self.syscall());
        let word = |at: usize| {
            let mut raw = [0_u8; 8];
            if let Some(from) = actual.get(at..at + 8) {
                raw.copy_from_slice(from);
            }
            u64::from_ne_bytes(raw)
        };
        let sizes = word(0).to_ne_bytes();
        let [b0, b1, b2, b3, h0, h1, h2, h3] = sizes;
        let message_bytes = u32::from_ne_bytes([b0, b1, b2, b3]) as usize;
        let message_handles = u32::from_ne_bytes([h0, h1, h2, h3]) as usize;
        match decode_unit(value) {
            Ok(()) => {}
            Err(Error::BufferTooSmall) => {
                return Err(ReadError::TooSmall {
                    bytes: message_bytes,
                    handles: message_handles,
                });
            }
            Err(error) => return Err(ReadError::Failed(error)),
        }
        let received = message_handles.min(capacity);
        for (slot, word) in handles
            .iter_mut()
            .zip(values.chunks_exact(HANDLE_BYTES))
            .take(received)
        {
            if let Ok(word) = <[u8; HANDLE_BYTES]>::try_from(word) {
                *slot = Handle(u32::from_ne_bytes(word));
            }
        }
        Ok((
            Received {
                bytes: message_bytes,
                handles: received,
            },
            Writer {
                process: word(8),
                job: word(16),
            },
        ))
    }
}

/// A ticket id's low 32 bits, as `charge_ticket` takes them.
fn low(id: u64) -> usize {
    (id & u64::from(u32::MAX)) as usize
}

/// Its high 32 bits.
fn high(id: u64) -> usize {
    (id >> 32) as usize
}
