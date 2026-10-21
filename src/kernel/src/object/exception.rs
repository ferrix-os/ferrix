//! Exception channels: a supervisor attached to a process or a job
//! (`docs/microkernel/K4.md`).
//!
//! A supervisor holds the read end of an ordinary channel; the kernel keeps
//! the other end, the only one that writes, in the [`Supervisor`] slot of
//! the process or job it was attached to. So everything a supervisor reads
//! from it is the kernel's, as K6's attested sender is.
//!
//! # What is here so far
//!
//! The slot, and attaching and detaching (§2.2). The chain a fault walks,
//! the wait and the verdicts (§2.3-§2.6) come with the second landing:
//! until then an attached channel is told nothing, and a fault takes the
//! path it always took.
//!
//! # Detaching
//!
//! Closing every handle to the read end. The slot notices the next time it
//! is asked -- an attach, and with the chain a fault -- and lets the
//! kernel's end go. A process's slot is also emptied when it ends
//! ([`Supervisor::close`]), so its supervisor sees `PEER_CLOSED` then.
//!
//! # Locks
//!
//! Each slot's lock is taken alone, in task context, and nothing is taken
//! under it. A kernel end the slot lets go of is taken out under the lock
//! and handed back to the caller, which disposes of it with no lock held
//! (`object::dispose`): an end's close takes its peer's locks and wakes its
//! waiters (`channel`'s `Drop`).

use alloc::sync::Arc;

use crate::object::Object;
use crate::object::channel::Endpoint;
use crate::sync::SpinLock;

/// An attached channel: the kernel's writing end, and how long a report on
/// it waits for an answer.
#[derive(Debug)]
struct Attached {
    /// The end the kernel writes reports to; its peer is the supervisor's.
    end: Arc<Endpoint>,
    /// How long one report waits for an answer, in milliseconds.
    #[expect(
        dead_code,
        reason = "read by the fault chain, K4's second landing (docs/microkernel/K4.md §11)"
    )]
    timeout_ms: u64,
}

/// Why an attach was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachRefused {
    /// A channel whose read end is open is attached already.
    Bound,
    /// The process has ended, and its slot was closed for good.
    Closed,
}

/// The supervisor slot of a process or a job: empty, or one attached
/// channel.
#[derive(Debug)]
pub(crate) struct Supervisor {
    /// The attached channel, if any. A channel whose read end has closed
    /// stays here until the slot is next asked.
    slot: SpinLock<Slot>,
}

/// What a slot holds.
#[derive(Debug)]
enum Slot {
    /// Nothing attached: an attach may come.
    Open(Option<Attached>),
    /// Its process has ended: nothing is ever attached again.
    Closed,
}

impl Supervisor {
    /// An empty slot.
    pub(crate) const fn new() -> Supervisor {
        Supervisor {
            slot: SpinLock::new(Slot::Open(None)),
        }
    }

    /// Attach `end`, the kernel's end of a fresh channel, with `timeout_ms`.
    ///
    /// Refused while a channel whose read end is open is attached, and once
    /// the slot is closed; one whose read end has closed is replaced.
    /// Answers the kernel end that was let go, if any, for the caller to
    /// dispose of with no lock held.
    ///
    /// # Errors
    ///
    /// [`AttachRefused`], with `end` handed back.
    pub(crate) fn attach(
        &self,
        end: Arc<Endpoint>,
        timeout_ms: u64,
    ) -> Result<Option<Object>, (AttachRefused, Arc<Endpoint>)> {
        let mut slot = self.slot.lock();
        let Slot::Open(attached) = &mut *slot else {
            return Err((AttachRefused::Closed, end));
        };
        if attached
            .as_ref()
            .is_some_and(|attached| !attached.end.peer_closed())
        {
            return Err((AttachRefused::Bound, end));
        }
        let gone = attached.replace(Attached { end, timeout_ms });
        drop(slot);
        Ok(gone.map(|attached| Object::Channel(attached.end)))
    }

    /// Whether a channel whose read end is open is attached.
    pub(crate) fn is_live(&self) -> bool {
        match &*self.slot.lock() {
            Slot::Open(Some(attached)) => !attached.end.peer_closed(),
            Slot::Open(None) | Slot::Closed => false,
        }
    }

    /// Let go of the attached channel if its read end has closed, and
    /// answer its kernel end for the caller to dispose of with no lock held:
    /// what an attach whose read end could not be placed does.
    pub(crate) fn let_go_if_dead(&self) -> Option<Object> {
        let mut slot = self.slot.lock();
        let Slot::Open(attached) = &mut *slot else {
            return None;
        };
        if attached
            .as_ref()
            .is_some_and(|attached| attached.end.peer_closed())
        {
            attached
                .take()
                .map(|attached| Object::Channel(attached.end))
        } else {
            None
        }
    }

    /// Close the slot for good: what a process that has ended does, so its
    /// supervisor sees `PEER_CLOSED` then, and an attach after it is
    /// refused. Answers the kernel end for the caller to dispose of with no
    /// lock held.
    pub(crate) fn close(&self) -> Option<Object> {
        match core::mem::replace(&mut *self.slot.lock(), Slot::Closed) {
            Slot::Open(attached) => attached.map(|attached| Object::Channel(attached.end)),
            Slot::Closed => None,
        }
    }
}
