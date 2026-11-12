//! `signalfd4` and `signalfd`: the calls around [`crate::fs::signalfd`].
//!
//! Linux's `do_signalfd4`, in its order: a mask size other than the kernel's
//! eight bytes is `EINVAL`, an unreadable mask `EFAULT`, a flag other than
//! `SFD_CLOEXEC` and `SFD_NONBLOCK` `EINVAL`. `SIGKILL` and `SIGSTOP` are
//! taken out of the mask. A descriptor of -1 makes a new signalfd; any other
//! must be one, whose mask is replaced -- `EBADF` for a descriptor that names
//! nothing, `EINVAL` for one that is not a signalfd -- and the flags, which
//! only a new descriptor could take, are then ignored, as on Linux.

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{SFD_CLOEXEC, SFD_NONBLOCK};

use crate::fs::signalfd;
use crate::syscall::fd;
use crate::syscall::process::Process;
use crate::syscall::signal::SIGSET_SIZE;
use crate::syscall::uaccess;

/// `signalfd4`: a signalfd reading the signals in the set at `mask`, or the
/// signalfd `fd` names made to read them instead.
///
/// # Errors
///
/// As the module says; `EMFILE` for a full table.
pub(crate) fn sys_signalfd4(
    process: &Process,
    descriptor: i32,
    mask: u64,
    size: u64,
    flags: u32,
) -> Result<usize, Errno> {
    if size != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let mut bytes = [0_u8; 8];
    uaccess::copy_from_user(process.space(), mask, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mask = u64::from_le_bytes(bytes);
    if flags & !(SFD_CLOEXEC | SFD_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    if descriptor != -1 {
        let file = fd::file(process, descriptor).map_err(|_| Errno::EBADF)?;
        let existing = signalfd::of(&file).ok_or(Errno::EINVAL)?;
        existing.set_mask(process, mask);
        return usize::try_from(descriptor).map_err(|_| Errno::EBADF);
    }
    let file = signalfd::create(process, mask, flags & SFD_NONBLOCK != 0)?;
    let fd = process
        .files()
        .lock()
        .insert(file, flags & SFD_CLOEXEC != 0)?;
    usize::try_from(fd).map_err(|_| Errno::EMFILE)
}

/// `signalfd`, x86-64's and ARMv7-A's older call: `signalfd4` with no flags.
///
/// # Errors
///
/// As [`sys_signalfd4`].
pub(crate) fn sys_signalfd(
    process: &Process,
    descriptor: i32,
    mask: u64,
    size: u64,
) -> Result<usize, Errno> {
    sys_signalfd4(process, descriptor, mask, size, 0)
}
