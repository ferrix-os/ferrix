//! `eventfd2` and `eventfd`: the calls around [`crate::fs::eventfd`].

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE};

use crate::fs::eventfd;
use crate::syscall::process::Process;

/// `eventfd2`: a counter holding `initial`.
///
/// # Errors
///
/// `EINVAL` for a flag other than `EFD_CLOEXEC`, `EFD_NONBLOCK` and
/// `EFD_SEMAPHORE`; `EMFILE` for a full table.
pub(crate) fn sys_eventfd2(process: &Process, initial: u32, flags: u32) -> Result<usize, Errno> {
    if flags & !(EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE) != 0 {
        return Err(Errno::EINVAL);
    }
    let file = eventfd::create(
        initial,
        flags & EFD_SEMAPHORE != 0,
        flags & EFD_NONBLOCK != 0,
    )?;
    let fd = process
        .files()
        .lock()
        .insert(file, flags & EFD_CLOEXEC != 0)?;
    usize::try_from(fd).map_err(|_| Errno::EMFILE)
}

/// `eventfd`, x86-64's and ARMv7-A's older call: `eventfd2` with no flags.
///
/// # Errors
///
/// As [`sys_eventfd2`].
pub(crate) fn sys_eventfd(process: &Process, initial: u32) -> Result<usize, Errno> {
    sys_eventfd2(process, initial, 0)
}
