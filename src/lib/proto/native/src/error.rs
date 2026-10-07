//! What a failed call means, decoded from the return register.
//!
//! A native call fails the way a Linux one does: `-errno` in `-4095..=-1`.
//! `src/lib/proto/native-abi`'s `status` module names which `errno` each native failure
//! travels as, and [`Error`] turns it back into the name, so a caller matches
//! on [`Error::PeerClosed`] rather than on `EPIPE`.

use ferrix_linux_abi::errno::{Errno, MAX_ERRNO};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::status;

/// Why a native call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// [`status::BAD_HANDLE`].
    BadHandle,
    /// [`status::WRONG_TYPE`].
    WrongType,
    /// [`status::ACCESS_DENIED`].
    AccessDenied,
    /// [`status::PEER_CLOSED`].
    PeerClosed,
    /// [`status::SHOULD_WAIT`].
    ShouldWait,
    /// [`status::BUFFER_TOO_SMALL`].
    BufferTooSmall,
    /// [`status::TOO_BIG`].
    TooBig,
    /// [`status::TIMED_OUT`].
    TimedOut,
    /// [`status::NO_MEMORY`].
    NoMemory,
    /// [`status::NO_HANDLES`].
    NoHandles,
    /// [`status::INVALID_ARGS`].
    InvalidArgs,
    /// [`status::FAULT`].
    Fault,
    /// [`status::ALREADY_BOUND`].
    AlreadyBound,
    /// [`status::BAD_STATE`].
    BadState,
    /// `ENOSYS`: this kernel has no such call. A gap in the table, or a call
    /// whose number is decided and whose handler is not built — which is what
    /// every wrapper in [`crate::pending`] answers today.
    Unsupported,
    /// `ESRCH`: the caller is not a process, or a pid names no live one
    /// ([`status::NO_PROCESS`]).
    NoProcess,
    /// [`status::NOT_CHILD`]: a pid names a process that is not the caller's
    /// child.
    NotChild,
    /// `EINTR`: the process is being killed, and the call gave up waiting.
    Interrupted,
    /// [`status::QUARANTINE_FULL`]: a device's DMA quarantine holds all it
    /// may, so no pin for it is taken until a driver of it is accepted.
    QuarantineFull,
    /// [`status::LIMIT_REACHED`]: a pin would take its device's live pins
    /// past the device's pin budget. Nothing was pinned.
    LimitReached,
    /// Some other `errno`, which no native call is documented to return.
    Other(Errno),
    /// A success value this call cannot produce: a handle of zero, or one
    /// wider than 32 bits. Never truncated, for the reason
    /// `Handle::from_register` gives.
    Unexpected(usize),
}

impl Error {
    /// The name for `errno`.
    #[must_use]
    pub const fn from_errno(errno: Errno) -> Error {
        match errno {
            status::BAD_HANDLE => Error::BadHandle,
            status::WRONG_TYPE => Error::WrongType,
            status::ACCESS_DENIED => Error::AccessDenied,
            status::PEER_CLOSED => Error::PeerClosed,
            status::SHOULD_WAIT => Error::ShouldWait,
            status::BUFFER_TOO_SMALL => Error::BufferTooSmall,
            status::TOO_BIG => Error::TooBig,
            status::TIMED_OUT => Error::TimedOut,
            status::NO_MEMORY => Error::NoMemory,
            status::NO_HANDLES => Error::NoHandles,
            status::INVALID_ARGS => Error::InvalidArgs,
            status::FAULT => Error::Fault,
            status::ALREADY_BOUND => Error::AlreadyBound,
            status::BAD_STATE => Error::BadState,
            Errno::ENOSYS => Error::Unsupported,
            Errno::ESRCH => Error::NoProcess,
            status::NOT_CHILD => Error::NotChild,
            Errno::EINTR => Error::Interrupted,
            status::QUARANTINE_FULL => Error::QuarantineFull,
            status::LIMIT_REACHED => Error::LimitReached,
            other => Error::Other(other),
        }
    }

    /// The `errno` this failure travelled as, or `None` for
    /// [`Error::Unexpected`], which was a success value.
    #[must_use]
    pub const fn errno(self) -> Option<Errno> {
        Some(match self {
            Error::BadHandle => status::BAD_HANDLE,
            Error::WrongType => status::WRONG_TYPE,
            Error::AccessDenied => status::ACCESS_DENIED,
            Error::PeerClosed => status::PEER_CLOSED,
            Error::ShouldWait => status::SHOULD_WAIT,
            Error::BufferTooSmall => status::BUFFER_TOO_SMALL,
            Error::TooBig => status::TOO_BIG,
            Error::TimedOut => status::TIMED_OUT,
            Error::NoMemory => status::NO_MEMORY,
            Error::NoHandles => status::NO_HANDLES,
            Error::InvalidArgs => status::INVALID_ARGS,
            Error::Fault => status::FAULT,
            Error::AlreadyBound => status::ALREADY_BOUND,
            Error::BadState => status::BAD_STATE,
            Error::Unsupported => Errno::ENOSYS,
            Error::NoProcess => Errno::ESRCH,
            Error::NotChild => status::NOT_CHILD,
            Error::Interrupted => Errno::EINTR,
            Error::QuarantineFull => status::QUARANTINE_FULL,
            Error::LimitReached => status::LIMIT_REACHED,
            Error::Other(errno) => errno,
            Error::Unexpected(_) => return None,
        })
    }
}

/// A return register, as success or failure.
///
/// # Errors
///
/// The [`Error`] for a value in `-4095..=-1`.
///
/// Inlined, with the failure's decoding kept out of line: every call's
/// return goes through this, and a call to it from the caller's code reaches
/// a page of the runtime's text the caller's own code may not be on. After
/// an address-space switch each such page is a TLB miss
/// (`docs/OPAQUE-KERNEL.md` §9.10, the user TLB refill).
#[inline]
pub fn decode(value: usize) -> Result<usize, Error> {
    let signed = value.cast_signed();
    if signed < 0 && signed >= -(MAX_ERRNO as isize) {
        Err(failure(signed))
    } else {
        Ok(value)
    }
}

/// [`decode`]'s failure: the [`Error`] for `signed`, in `-4095..=-1`.
#[cold]
#[inline(never)]
fn failure(signed: isize) -> Error {
    let number = u16::try_from(signed.unsigned_abs()).unwrap_or(MAX_ERRNO);
    Error::from_errno(Errno(number))
}

/// A return register that carries a new handle.
///
/// # Errors
///
/// As [`decode`], and [`Error::Unexpected`] for a success that is not a
/// handle.
#[inline]
pub fn decode_handle(value: usize) -> Result<Handle, Error> {
    let value = decode(value)?;
    match u32::try_from(value) {
        Ok(raw) if raw != 0 => Ok(Handle(raw)),
        _ => Err(Error::Unexpected(value)),
    }
}

/// A return register that carries nothing but success or failure.
///
/// # Errors
///
/// As [`decode`].
#[inline]
pub fn decode_unit(value: usize) -> Result<(), Error> {
    decode(value).map(|_| ())
}
