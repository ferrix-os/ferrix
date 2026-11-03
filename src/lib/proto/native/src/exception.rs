//! Exception channels (`docs/microkernel/K4.md`): a supervisor attached to a
//! process or a job, which hears the faults of the processes beneath it.

use ferrix_native_abi::nr;

use crate::call::{Call, Syscall};
use crate::channel::Channel;
use crate::error::{Error, decode_handle, decode_unit};
use crate::handle::{Object, OwnedHandle, object_handle, register};

pub use ferrix_native_abi::exception::{
    HAND_ON, KILL, RESUME, Registers, Report, STATE_READ, STATE_WRITE, Words,
};

object_handle!(
    /// One faulting thread, held in the kernel until its supervisor decides:
    /// the handle a report carries.
    Exception
);

impl<S: Syscall> Exception<S> {
    /// `exception_state` with `STATE_READ`: the faulting thread's registers,
    /// as bytes of [`Registers`] (`buffer` must be exactly that long).
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `READ`; [`Error::InvalidArgs`] for a
    /// wrong length; [`Error::BadState`] once decided; [`Error::TimedOut`]
    /// once its wait expired.
    pub fn read_state(&self, buffer: &mut [u8]) -> Result<(), Error> {
        let len = buffer.len();
        decode_unit(
            Call::new(nr::EXCEPTION_STATE)
                .value(register(self.handle()))
                .value(STATE_READ as usize)
                .output(buffer)
                .value(len)
                .make(self.syscall()),
        )
    }

    /// `exception_state` with `STATE_WRITE`: replace the faulting thread's
    /// registers with `buffer`, bytes of [`Registers`].
    ///
    /// # Errors
    ///
    /// As [`Exception::read_state`], with `WRITE`; [`Error::InvalidArgs`]
    /// too for registers a program may not hold, which change nothing.
    pub fn write_state(&self, buffer: &[u8]) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::EXCEPTION_STATE)
                .value(register(self.handle()))
                .value(STATE_WRITE as usize)
                .input(buffer)
                .value(buffer.len())
                .make(self.syscall()),
        )
    }

    /// `exception_resolve`: [`RESUME`], [`KILL`] or [`HAND_ON`].
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MANAGE`; [`Error::InvalidArgs`] for
    /// another verdict; [`Error::BadState`] once decided;
    /// [`Error::TimedOut`] once its wait expired.
    pub fn resolve(&self, verdict: u64) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::EXCEPTION_RESOLVE)
                .value(register(self.handle()))
                .value(usize::try_from(verdict).unwrap_or(usize::MAX))
                .make(self.syscall()),
        )
    }
}

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
