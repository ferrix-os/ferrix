//! What a handle permits.
//!
//! Rights live on the *handle*, not on the object. Two handles to one channel
//! can differ — a driver is given a VMO it may map but not write through, or a
//! job it may watch but not kill — and a handle's rights can only ever be
//! reduced: [`Requested::resolve`] refuses anything that is not a subset of
//! what is held. That monotonicity is the whole security argument of a
//! capability system, so it is decided in one function here rather than at
//! each call site in the kernel.

use core::ops::BitOr;

/// A set of rights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct Rights(
    /// The flag word. Only the bits in [`Rights::ALL`] are defined.
    pub u32,
);

impl Rights {
    /// Nothing at all. A handle with no rights can still be closed.
    pub const NONE: Rights = Rights(0);
    /// A second handle to the same object may be made from this one.
    pub const DUPLICATE: Rights = Rights(1 << 0);
    /// The handle may be sent through a channel to another process.
    pub const TRANSFER: Rights = Rights(1 << 1);
    /// Data may be read: a channel's messages, a VMO's pages, a port's packets.
    pub const READ: Rights = Rights(1 << 2);
    /// Data may be written: a channel's messages, a VMO's pages, a port's
    /// user packets.
    pub const WRITE: Rights = Rights(1 << 3);
    /// The object's memory may be mapped into an address space.
    pub const MAP: Rights = Rights(1 << 4);
    /// The object's signals may be waited on, directly or through a port.
    pub const WAIT: Rights = Rights(1 << 5);
    /// The object may be controlled: a job killed, an interrupt bound or
    /// acknowledged.
    pub const MANAGE: Rights = Rights(1 << 6);
    /// A job's own limits may be set: its memory, its tasks and its processor
    /// weight (`job_set_limit`).
    ///
    /// Apart from [`Rights::MANAGE`] because the two are held by different
    /// people. Whoever is delegated a cgroup may kill and fill its job, but
    /// its limits are its delegator's, as on Linux, where the delegatee owns
    /// `cgroup.procs` and root still owns `memory.max`. A job made with
    /// `job_create` carries it, so a limit can be set on what one makes.
    pub const SET_LIMIT: Rights = Rights(1 << 7);

    /// Every defined right.
    pub const ALL: Rights = Rights(0xFF);

    /// What a new channel endpoint carries.
    pub const CHANNEL: Rights = Rights(
        Rights::DUPLICATE.0
            | Rights::TRANSFER.0
            | Rights::READ.0
            | Rights::WRITE.0
            | Rights::WAIT.0,
    );
    /// What a new port carries.
    pub const PORT: Rights = Rights::CHANNEL;
    /// What a new VMO carries.
    pub const VMO: Rights = Rights(
        Rights::DUPLICATE.0 | Rights::TRANSFER.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0,
    );
    /// What a new job carries.
    pub const JOB: Rights = Rights(
        Rights::DUPLICATE.0
            | Rights::TRANSFER.0
            | Rights::WAIT.0
            | Rights::MANAGE.0
            | Rights::SET_LIMIT.0,
    );
    /// What a new interrupt carries.
    ///
    /// No [`Rights::DUPLICATE`]. An interrupt is acknowledged by whoever
    /// serviced it, and two holders racing to acknowledge one line is a lost
    /// or doubled interrupt. It can still be *moved*, which is how `devmgr`
    /// hands it to a driver.
    pub const INTERRUPT: Rights = Rights(Rights::TRANSFER.0 | Rights::WAIT.0 | Rights::MANAGE.0);
    /// What a handle to a device node carries.
    ///
    /// [`Rights::MANAGE`] to mint the device's interrupts and I/O mappings,
    /// and [`Rights::TRANSFER`] so `devmgr` can hand it to a driver. No
    /// [`Rights::DUPLICATE`], for the reason [`Rights::INTERRUPT`] has none: a
    /// device has one driver.
    pub const DEVICE: Rights = Rights(Rights::TRANSFER.0 | Rights::MANAGE.0);
    /// What a new I/O mapping carries.
    ///
    /// No [`Rights::DUPLICATE`], for the reason [`Rights::INTERRUPT`] has
    /// none: a device's registers have one driver.
    pub const IO_MAPPING: Rights = Rights(Rights::TRANSFER.0 | Rights::MAP.0);
    /// What a new pin carries.
    ///
    /// [`Rights::READ`] to ask for its device addresses, and nothing else. No
    /// [`Rights::TRANSFER`]: a pin is the driver's promise that its device may
    /// reach those pages, and it is kept, or given back by closing it, where it
    /// was made.
    pub const PIN: Rights = Rights(Rights::READ.0);
    /// What a handle to a fault window's server carries: [`Rights::MANAGE`]
    /// for `window_insert`, `window_revoke` and `window_answer`, and nothing
    /// else. No [`Rights::DUPLICATE`] and no [`Rights::TRANSFER`]: the handle
    /// is the server's identity, so there is one, and its close is the
    /// server's death, which fails every fault waiting on it.
    pub const WINDOW_SERVER: Rights = Rights(Rights::MANAGE.0);
    /// What a handle to a process carries.
    ///
    /// [`Rights::WAIT`] to hear that it has ended, directly or through a port,
    /// and [`Rights::MANAGE`] for the calls that act on it, which come with
    /// native process creation.
    ///
    /// None of those calls reaches inside the process: no call reads or
    /// writes its memory, takes or copies its handles, or gives it anything
    /// once it has started (`process_start` is `BAD_STATE` then). So a handle
    /// to a process the kernel started with authority of its own -- `devmgr`
    /// under `ferrix.devmgr=init` -- hands its holder none of that authority.
    pub const PROCESS: Rights =
        Rights(Rights::DUPLICATE.0 | Rights::TRANSFER.0 | Rights::WAIT.0 | Rights::MANAGE.0);

    /// Whether every right in `other` is also in `self`.
    #[must_use]
    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    /// The rights in both.
    #[must_use]
    pub const fn intersection(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }

    /// Whether no bit outside [`Rights::ALL`] is set.
    #[must_use]
    pub const fn is_known(self) -> bool {
        self.0 & !Rights::ALL.0 == 0
    }
}

impl BitOr for Rights {
    type Output = Rights;

    fn bitor(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }
}

/// The register value that asks for a duplicate with the rights already held.
///
/// Bit 31, outside [`Rights::ALL`] and never a right, so it cannot be confused
/// with a request for some particular set.
pub const SAME_RIGHTS: u32 = 1 << 31;

/// What a `handle_duplicate` or `handle_replace` caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requested {
    /// Whatever the original holds.
    Same,
    /// Exactly these, which must be a subset of what the original holds.
    Exactly(Rights),
}

impl Requested {
    /// Read a request from an argument register.
    ///
    /// `None` for any bit that is not a defined right, rather than ignoring
    /// it. A program asking to keep a right this kernel does not know about
    /// believes it will hold something it will not; a newer program on an
    /// older kernel should find that out here, as `EINVAL`, not later as a
    /// refusal it cannot explain.
    #[must_use]
    pub const fn from_register(value: u64) -> Option<Requested> {
        if value == SAME_RIGHTS as u64 {
            return Some(Requested::Same);
        }
        if value > u32::MAX as u64 {
            return None;
        }
        let rights = Rights(value as u32);
        if rights.is_known() {
            Some(Requested::Exactly(rights))
        } else {
            None
        }
    }

    /// The rights the new handle gets, given what the original `held`.
    ///
    /// `None` if the request would add a right. This is the one place the
    /// rule "rights only ever shrink" is decided.
    #[must_use]
    pub const fn resolve(self, held: Rights) -> Option<Rights> {
        match self {
            Requested::Same => Some(held),
            Requested::Exactly(wanted) => {
                if held.contains(wanted) {
                    Some(wanted)
                } else {
                    None
                }
            }
        }
    }
}
