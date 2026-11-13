//! The native failures, and the `errno` each one travels as.
//!
//! Each name is what the failure *means* to a native caller; the value is
//! whichever Linux error number reads closest, so that musl's `strerror`
//! prints something a person can act on. The one requirement beyond that is
//! that no two share a number — a caller that cannot tell "the peer has gone"
//! from "try again" cannot write a correct loop — and the tests hold it.

use ferrix_errno::Errno;

/// The handle names nothing in this process: never issued, or closed.
pub const BAD_HANDLE: Errno = Errno::EBADF;

/// The handle is real but names the wrong kind of object for this call.
pub const WRONG_TYPE: Errno = Errno::EOPNOTSUPP;

/// The handle does not carry a right this call needs.
pub const ACCESS_DENIED: Errno = Errno::EACCES;

/// The other end of a channel is closed: nothing written will be read.
pub const PEER_CLOSED: Errno = Errno::EPIPE;

/// Nothing to read, or no room to write, right now. Wait for the matching
/// signal and try again.
pub const SHOULD_WAIT: Errno = Errno::EAGAIN;

/// The next message is larger than the buffers offered for it. It is left
/// queued, and the sizes it needs are reported.
pub const BUFFER_TOO_SMALL: Errno = Errno::ENOBUFS;

/// A message is larger than any channel will carry, in bytes or in handles.
pub const TOO_BIG: Errno = Errno::EMSGSIZE;

/// A deadline passed before the thing waited for happened.
pub const TIMED_OUT: Errno = Errno::ETIMEDOUT;

/// The kernel could not allocate what the call needed.
pub const NO_MEMORY: Errno = Errno::ENOMEM;

/// The process's handle table is full.
pub const NO_HANDLES: Errno = Errno::EMFILE;

/// An argument is outside what the call accepts.
pub const INVALID_ARGS: Errno = Errno::EINVAL;

/// A pointer argument did not name memory the caller may use.
pub const FAULT: Errno = Errno::EFAULT;

/// The object is already in the state this call would put it in: an
/// interrupt already bound to a port.
pub const ALREADY_BOUND: Errno = Errno::EBUSY;

/// The object is in a state the call cannot act on: a job that has been
/// killed takes no new processes or children.
pub const BAD_STATE: Errno = Errno::EIDRM;

/// A process id names no live process.
pub const NO_PROCESS: Errno = Errno::ESRCH;

/// A process id names a live process that is not the caller's child.
pub const NOT_CHILD: Errno = Errno::ECHILD;

/// A device's DMA quarantine holds all it may: the pins of drivers that died
/// before one of their successors reset the device (`object::pin` in the
/// kernel). A pin for it is refused until a driver of the device is accepted.
pub const QUARANTINE_FULL: Errno = Errno::EDQUOT;

/// A pin would take the pages its device's live pins hold past the device's
/// pin budget, which `devmgr` sets (`docs/NVIDIA.md` §12.2). Nothing was
/// pinned; a driver treats it as it treats `NO_MEMORY`.
pub const LIMIT_REACHED: Errno = Errno::ENOSPC;
/// A function's bind whose domain does not translate, with no opt-out
/// (`docs/microkernel/K8.md`, K8.3): not a matter of rights.
pub const UNTRANSLATED: Errno = Errno::ENXIO;

/// Every name above, for the tests that hold them distinct.
pub const ALL: [Errno; 19] = [
    BAD_HANDLE,
    WRONG_TYPE,
    ACCESS_DENIED,
    PEER_CLOSED,
    SHOULD_WAIT,
    BUFFER_TOO_SMALL,
    TOO_BIG,
    TIMED_OUT,
    NO_MEMORY,
    NO_HANDLES,
    INVALID_ARGS,
    FAULT,
    ALREADY_BOUND,
    BAD_STATE,
    NO_PROCESS,
    NOT_CHILD,
    QUARANTINE_FULL,
    LIMIT_REACHED,
    UNTRANSLATED,
];

/// The call has no meaning on this architecture: restricted mode's and shared
/// processes' calls on ARMv7-A (`docs/microkernel/K1.md` §8).
pub const NOT_SUPPORTED: Errno = Errno::ENOSYS;

/// A kick ended the wait of a native call made from restricted mode
/// (`docs/microkernel/K1.md` §2.4).
pub const INTERRUPTED: Errno = Errno::EINTR;
