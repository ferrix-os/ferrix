//! Charging that follows the client: tickets (`docs/microkernel/K7.md`;
//! `docs/OPAQUE-KERNEL.md` §2 point 5; the consultant's OK IF, ledger 2525).
//!
//! A server that makes memory or objects *for* a client would pay for them
//! itself (F-35, F-37), so one client could fill a shared server to its
//! limit and starve every other. A server that could name the job to charge
//! could charge any victim. A **ticket** is the third way: the kernel issues
//! it for the caller's own job on one end of a channel
//! (`CHARGE_TICKET ISSUE`), and a server thread adopts it through the other
//! end (`ADOPT`). While the ticket is live, the VMO commits and pins and the
//! new objects the adopted thread's native calls make are charged to the
//! ticket's job ([`crate::object::quota::charge_target`]). Nothing names a
//! job but the kernel.
//!
//! # What holds
//!
//! * A ticket's job and process are the issuing caller's, read by the
//!   kernel under the process's membership lock, so the two cannot come
//!   from either side of a move between jobs (L.object.207).
//! * It is honoured only through the other end of the channel it was issued
//!   on, and only while live (L.object.208): the registry is per channel and
//!   per issuing side, in [`Tickets`].
//! * It ends at `END` from either side and when either end of its channel
//!   closes, and never becomes live again; ids come from one counter and are
//!   never issued twice (L.object.210).
//! * A ticket is one native object charged to its issuer, and at most
//!   [`MAX_TICKETS`] are live per issuing end (L.object.212).
//!
//! # The adoption
//!
//! A task holds at most one adopted ticket ([`crate::sched::Task`]'s
//! `adopted`). At the entry of each native call but `CHARGE_TICKET` itself,
//! [`charging_for_call`] looks at it: a live ticket sets the task's per-call
//! target to the ticket's slot until the call returns, and an ended one is
//! dropped there, so an adoption is inert once its ticket has ended
//! (2525 (5)). The target is checked at the call's entry only, so a ticket
//! ended while an adopted call runs still pays for the rest of that one call,
//! within the client's own limits (2525 (4)).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_native_abi::types::MAX_TICKETS;

use crate::object::channel::Sender;
use crate::object::quota::{self, Charge, Resource};

/// The next ticket's id. Starts at 1, so that 0 is never an id, and is only
/// ever increased: at one issue a nanosecond it would take 584 years to
/// wrap, and an issue that would wrap it is refused instead.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One ticket: a charge the kernel lets a server make to the client that
/// issued it, for as long as it is live.
#[derive(Debug)]
pub(crate) struct Ticket {
    /// Its id, never issued twice.
    id: u64,
    /// Who issued it: the process and its job at the issue (K6's `Sender`).
    issuer: Sender,
    /// The issuer's job's quota slot at the issue, or `quota::NONE` for the
    /// tree's root. Held by [`Ticket::charge`] for as long as the ticket is.
    slot: u32,
    /// Live until ended, once and for good.
    live: AtomicBool,
    /// The ticket itself, one object charged to its issuer.
    #[expect(
        dead_code,
        reason = "AUDIT: held for its drop, which uncharges the issuer"
    )]
    charge: Charge,
}

impl Ticket {
    /// Who issued it.
    pub(crate) fn issuer(&self) -> Sender {
        self.issuer
    }

    /// The slot its charges go to.
    pub(crate) fn slot(&self) -> u32 {
        self.slot
    }

    /// Whether it is still live.
    pub(crate) fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    /// End it. Whether this call was the one that did.
    fn end(&self) -> bool {
        self.live.swap(false, Ordering::AcqRel)
    }
}

/// Why an issue was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueError {
    /// [`MAX_TICKETS`] are live on this end already.
    Full,
    /// The other end has gone.
    PeerClosed,
    /// The issuer's job is at its object limit, or no memory.
    NoMemory,
}

/// One channel's live tickets, by the side that issued them. Behind its own
/// lock (`Channel::tickets`), taken only by `CHARGE_TICKET` and by an end's
/// close: never the inbox lock, so no IPC path takes it (2525 (6)). Each
/// side holds at most [`MAX_TICKETS`], so every walk here is bounded by 64.
#[derive(Debug, Default)]
pub(crate) struct Tickets {
    /// The tickets each side issued, indexed by the side.
    issued: [Vec<Arc<Ticket>>; 2],
}

impl Tickets {
    /// Issue a ticket on side `side` for `issuer`, charged to `slot`.
    pub(crate) fn issue(
        &mut self,
        side: usize,
        issuer: Sender,
        slot: u32,
    ) -> Result<u64, IssueError> {
        let list = self.issued.get_mut(side).ok_or(IssueError::NoMemory)?;
        if list.len() >= MAX_TICKETS {
            return Err(IssueError::Full);
        }
        list.try_reserve(1).map_err(|_| IssueError::NoMemory)?;
        let charge = Charge::to(slot, Resource::Objects, 1)
            .map_err(|quota::Exceeded| IssueError::NoMemory)?;
        let id = NEXT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| IssueError::NoMemory)?;
        let ticket = crate::fallible::try_arc(Ticket {
            id,
            issuer,
            slot,
            live: AtomicBool::new(true),
            charge,
        })
        .map_err(|_| IssueError::NoMemory)?;
        // NOALLOC: the room `try_reserve` made above.
        list.push(ticket);
        Ok(id)
    }

    /// The live ticket `id` that side `issuer_side` issued.
    pub(crate) fn find(&self, issuer_side: usize, id: u64) -> Option<Arc<Ticket>> {
        let issued = self.issued.get(issuer_side)?;
        issued
            .iter()
            .find(|ticket| ticket.id == id && ticket.is_live())
            .map(Arc::clone)
    }

    /// End the live ticket `id`, issued by either side, and hand it back to
    /// be dropped once the lock is let go.
    pub(crate) fn end(&mut self, id: u64) -> Option<Arc<Ticket>> {
        for list in &mut self.issued {
            if let Some(at) = list.iter().position(|ticket| ticket.id == id) {
                let ticket = list.swap_remove(at);
                let _ = ticket.end();
                return Some(ticket);
            }
        }
        None
    }

    /// End every ticket, both sides': an end of the channel closed. Handed
    /// back to be dropped once the lock is let go.
    pub(crate) fn end_all(&mut self) -> [Vec<Arc<Ticket>>; 2] {
        let ended = core::mem::take(&mut self.issued);
        for ticket in ended.iter().flatten() {
            let _ = ticket.end();
        }
        ended
    }
}

/// What [`charging_for_call`] found for the running task.
enum Target {
    /// No adoption, or one whose ticket ended (and which it dropped).
    Own,
    /// A live ticket: charge its slot for this call.
    Ticket(u32),
}

/// Run one native call `answer` with the running task's adoption applied:
/// its charges of VMO commits and new objects go to the adopted ticket's job
/// while the ticket is live at the call's entry, and to the task's own
/// otherwise. An adoption whose ticket has ended is dropped here.
pub(crate) fn charging_for_call<R>(answer: impl FnOnce() -> R) -> R {
    let target = crate::sched::with_current(|task| {
        if !task.has_adoption() {
            return (Target::Own, None);
        }
        let adopted = task.adopted_ticket();
        match adopted {
            Some(ticket) if ticket.is_live() => (Target::Ticket(ticket.slot()), None),
            // Ended: inert, and let go of here, outside the task's lock.
            _ => (Target::Own, task.take_adoption()),
        }
    });
    let (target, dropped) = target.unwrap_or((Target::Own, None));
    drop(dropped);
    match target {
        Target::Own => answer(),
        Target::Ticket(slot) => {
            let _ = crate::sched::with_current(|task| task.set_charge_target(slot));
            let answered = answer();
            let _ = crate::sched::with_current(|task| task.set_charge_target(quota::NO_TARGET));
            answered
        }
    }
}

/// The running task adopts `ticket`, in place of whatever it had.
pub(crate) fn adopt(ticket: Arc<Ticket>) {
    let old = crate::sched::with_current(|task| task.replace_adoption(Some(ticket)));
    drop(old);
}

/// The running task gives up its adoption, if it has one: `LEAVE`, `exec`
/// and a task's exit.
pub(crate) fn leave() {
    // One load for the task that adopted nothing, as every task's exit and
    // every `exec` ask: no lock is taken where there is nothing to give up.
    let old = crate::sched::with_current(|task| {
        if task.has_adoption() {
            task.take_adoption()
        } else {
            None
        }
    });
    drop(old);
}
