//! The calls std does not offer: `syslog(2)`, `/dev/kmsg`'s open, seek and
//! blocking switch, and the boot clock.
//!
//! This is the only `unsafe` in the program: the `libc` calls, each with
//! what makes it sound beside it.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

/// Let a write to a closed pipe end the program, as it ends util-linux's:
/// `dmesg | head` is not an error. Rust's runtime ignores `SIGPIPE`.
pub(crate) fn die_on_broken_pipe() {
    // SAFETY: setting a signal's disposition to its default, before any
    // other thread exists.
    let _ = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

/// The `syslog(2)` actions this uses.
pub(crate) mod action {
    /// `SYSLOG_ACTION_READ_ALL`.
    pub(crate) const READ_ALL: i32 = 3;
    /// `SYSLOG_ACTION_READ_CLEAR`.
    pub(crate) const READ_CLEAR: i32 = 4;
    /// `SYSLOG_ACTION_CLEAR`.
    pub(crate) const CLEAR: i32 = 5;
    /// `SYSLOG_ACTION_CONSOLE_OFF`.
    pub(crate) const CONSOLE_OFF: i32 = 6;
    /// `SYSLOG_ACTION_CONSOLE_ON`.
    pub(crate) const CONSOLE_ON: i32 = 7;
    /// `SYSLOG_ACTION_CONSOLE_LEVEL`.
    pub(crate) const CONSOLE_LEVEL: i32 = 8;
    /// `SYSLOG_ACTION_SIZE_BUFFER`.
    pub(crate) const SIZE_BUFFER: i32 = 10;
}

/// `syslog(2)` with no buffer: answers its number.
pub(crate) fn klogctl(action: i32, len: i32) -> io::Result<usize> {
    // SAFETY: a null buffer; the actions passed here read none.
    let answer = unsafe { libc::klogctl(action, core::ptr::null_mut(), len) };
    usize::try_from(answer).map_err(|_| io::Error::last_os_error())
}

/// `syslog(2)` reading into `buf`: answers how much it wrote there.
pub(crate) fn klogctl_read(action: i32, buf: &mut [u8]) -> io::Result<usize> {
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    // SAFETY: `buf` is writable for `len` bytes, all the kernel may write.
    let answer = unsafe { libc::klogctl(action, buf.as_mut_ptr().cast(), len) };
    usize::try_from(answer).map_err(|_| io::Error::last_os_error())
}

/// `/dev/kmsg`, opened for reading without blocking.
pub(crate) fn open_kmsg() -> io::Result<OwnedFd> {
    let path = CString::new("/dev/kmsg").map_err(io::Error::other)?;
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    let raw = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` was just opened and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// `lseek(fd, 0, whence)`: on `/dev/kmsg`, a move to a record.
pub(crate) fn seek(fd: &OwnedFd, whence: i32) -> io::Result<()> {
    // SAFETY: a plain call on a descriptor this owns.
    if unsafe { libc::lseek(fd.as_raw_fd(), 0, whence) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `SEEK_DATA`: on `/dev/kmsg`, the first record after the last clear.
pub(crate) const SEEK_DATA: i32 = libc::SEEK_DATA;
/// `SEEK_END`: past the last record.
pub(crate) const SEEK_END: i32 = libc::SEEK_END;

/// Make reads of `fd` wait.
pub(crate) fn set_blocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: plain calls on a descriptor this owns.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One `read` of `fd`.
pub(crate) fn read(fd: &OwnedFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is writable for its length.
    let answer = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    usize::try_from(answer).map_err(|_| io::Error::last_os_error())
}

/// The wall clock and the time since boot, which `-T` works out the boot
/// from: `/proc/uptime` for the second, the kernel's own clock its records
/// are stamped with, so no time type of the C library's is needed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clocks;

impl dmesg::show::Clock for Clocks {
    fn boot(&self) -> (i64, u32) {
        let up = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|text| dmesg::show::uptime_micros(&text))
            .unwrap_or(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_micros());
        let boot = now.saturating_sub(u128::from(up));
        (
            i64::try_from(boot / 1_000_000).unwrap_or(0),
            u32::try_from(boot % 1_000_000).unwrap_or(0),
        )
    }
}
