//! Exception channels (`docs/microkernel/K4.md`): a supervisor attached to a
//! process or a job, which hears the faults of the processes beneath it.

use ferrix_native_abi::nr;

use crate::call::{Call, Syscall};
use crate::channel::Channel;
use crate::error::{Error, decode_handle};
use crate::handle::{Object, OwnedHandle, register};

/// `exception_channel_create`: attach an exception channel to `task`, a
/// process or a job, and answer its read end. Every report on it waits for
/// at most `timeout_ms` for an answer.
///
/// # Errors
///
/// [`Error::AccessDenied`] without `SUPERVISE`; [`Error::InvalidArgs`] for a
/// timeout outside `1..=EXCEPTION_TIMEOUT_MAX_MS`; [`Error::AlreadyBound`]
/// while another channel is attached; [`Error::BadState`] for a process
/// that has ended; [`Error::WrongType`] for anything but a process or a job;
/// [`Error::NoMemory`], [`Error::NoHandles`].
pub fn attach<S: Syscall, T: Object<S>>(task: &T, timeout_ms: u32) -> Result<Channel<S>, Error> {
    let value = Call::new(nr::EXCEPTION_CHANNEL_CREATE)
        .value(register(task.handle()))
        .value(timeout_ms as usize)
        .value(0)
        .make(task.syscall());
    let handle = decode_handle(value)?;
    Ok(Channel::from_owned(OwnedHandle::from_raw(
        task.syscall(),
        handle,
    )))
}
