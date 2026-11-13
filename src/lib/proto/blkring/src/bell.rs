//! Doorbells: what to ring, and what counts as having rung.
//!
//! Both doorbells are `PACKET_USER` port packets on a port the receiver made
//! and the sender holds with `WRITE` only. Neither side here queues one; a
//! side's `publish` returns the [`Doorbell`] to ring, and the glue rings it
//! with `port_queue` in the driver or `Port::queue_user` in the kernel.
//!
//! Ports are bounded (1024 user packets), so a queue can be refused. A bell is
//! a hint and any number of them counts as one, so a refusal for being full
//! means a bell is already waiting: the call *rang*. [`rung`] and
//! [`port_queue_rung`] turn that into [`Rung::Full`], so neither side's glue
//! treats it as an error.

use ferrix_errno::Errno;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{PACKET_USER, PortPacket};

/// The port key of the kernel's bell on the driver's port: new submissions.
///
/// Keys 1 and 2 are the ring's; a driver chooses its interrupt and control
/// channel keys outside them.
pub const BELL_SUBMIT: u64 = 1;

/// The port key of the driver's bell on the kernel's completion port: new
/// completions.
pub const BELL_COMPLETE: u64 = 2;

/// The want-bell value that asks the producer to ring.
pub const WANT_BELL: u32 = 1;

/// A bell to ring: the producer published, and the consumer asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use = "a doorbell nobody rings is a lost wake-up"]
pub struct Doorbell {
    key: u64,
    tail: u32,
}

impl Doorbell {
    pub(crate) const fn new(key: u64, tail: u32) -> Self {
        Self { key, tail }
    }

    /// [`BELL_SUBMIT`] or [`BELL_COMPLETE`].
    #[must_use]
    pub const fn key(&self) -> u64 {
        self.key
    }

    /// The tail published, carried as a hint.
    #[must_use]
    pub const fn tail(&self) -> u32 {
        self.tail
    }

    /// The packet to queue: this bell's key, `PACKET_USER`, and the tail in
    /// the first data word.
    #[must_use]
    pub const fn packet(&self) -> PortPacket {
        PortPacket {
            key: self.key,
            kind: PACKET_USER,
            signals: 0,
            data: [self.tail as u64, 0],
        }
    }
}

/// A bell that counts as rung.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rung {
    /// The packet was queued.
    Queued,
    /// The port was full, so a bell is already waiting. Rung, not failed.
    Full,
}

/// Classify the result of queueing a bell: success and full are both rung, any
/// other failure is passed back.
///
/// `is_full` recognises the queue's own "full" error, which is
/// `PortError::Full` in the kernel.
///
/// # Errors
///
/// `result`'s error, if `is_full` does not recognise it.
pub fn rung<E>(result: Result<(), E>, is_full: impl FnOnce(&E) -> bool) -> Result<Rung, E> {
    match result {
        Ok(()) => Ok(Rung::Queued),
        Err(error) if is_full(&error) => Ok(Rung::Full),
        Err(error) => Err(error),
    }
}

/// [`rung`] for a driver's `port_queue`, which reports a full port as
/// `SHOULD_WAIT`.
///
/// # Errors
///
/// Any error other than `SHOULD_WAIT`, such as a closed or wrong handle.
pub fn port_queue_rung(result: Result<(), Errno>) -> Result<Rung, Errno> {
    rung(result, |error| *error == status::SHOULD_WAIT)
}

/// What a consumer about to sleep should do instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// Nothing is pending and the producer will ring: wait on the port.
    Sleep,
    /// This many entries arrived; the want-bell flag is cleared again. Process
    /// them instead of sleeping.
    Pending(u32),
}
