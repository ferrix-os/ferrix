//! `/dev/watchdog`: Linux's watchdog device over the kernel's watchdog
//! (`crate::watchdog`, `docs/POSTMORTEM.md` §6.11).
//!
//! Character device 10:130, root's, mode 0600, present only under
//! `ferrix.watchdog=on`. The Linux numbers live here, in the load; the core's
//! `watchdog` holds the deadline and knows none of them.
//! Taken from the S4c branch's file (`s4-lockup`, 2026-10-31), over the
//! core's interface instead of its `userwatch`.
//!
//! * An open arms it, one at a time (`EBUSY` for a second), and only for a
//!   privileged caller (`EPERM`).
//! * A write of any length feeds it. A write that holds a `V` lets the last
//!   release of this open disarm it (the magic close); a later write without
//!   one takes that back, as Linux's `watchdog_write` does.
//! * The last release of the open file -- not each `close` of a copy -- ends
//!   the open: disarmed after the magic close, armed and counting otherwise.
//! * `ioctl`: `WDIOC_GETSUPPORT`, `GETSTATUS`, `GETBOOTSTATUS`, `KEEPALIVE`,
//!   `SETTIMEOUT`, `GETTIMEOUT`, `GETTIMELEFT`, `SETOPTIONS`; anything else
//!   is `ENOTTY` (`linux/watchdog.h`).

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_linux_abi::errno::Errno as LinuxErrno;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileType, Inode, Metadata, Result, Timespec};

use crate::syscall::process::{self, Process};
use crate::syscall::{credentials, uaccess};
use crate::watchdog;

/// Its device number: Linux's `WATCHDOG_MINOR` of the misc major.
pub(crate) const MAJOR: u32 = 10;
pub(crate) const MINOR: u32 = 130;

/// The requests (`linux/watchdog.h`, magic `'W'`), the same on every
/// architecture Ferrix runs on.
const WDIOC_GETSUPPORT: u32 = 0x8028_5700;
const WDIOC_GETSTATUS: u32 = 0x8004_5701;
const WDIOC_GETBOOTSTATUS: u32 = 0x8004_5702;
const WDIOC_SETOPTIONS: u32 = 0x8004_5704;
const WDIOC_KEEPALIVE: u32 = 0x8004_5705;
const WDIOC_SETTIMEOUT: u32 = 0xC004_5706;
const WDIOC_GETTIMEOUT: u32 = 0x8004_5707;
const WDIOC_GETTIMELEFT: u32 = 0x8004_570A;

/// `watchdog_info.options`: what this one does.
const WDIOF_SETTIMEOUT: u32 = 0x0080;
const WDIOF_MAGICCLOSE: u32 = 0x0100;
const WDIOF_KEEPALIVEPING: u32 = 0x8000;

/// `WDIOC_SETOPTIONS`'s words.
const WDIOS_DISABLECARD: u32 = 0x0001;
const WDIOS_ENABLECARD: u32 = 0x0002;

/// What `WDIOC_GETSUPPORT` names it.
const IDENTITY: &[u8] = b"Ferrix watchdog";

/// One open of `/dev/watchdog`.
#[derive(Debug)]
pub(crate) struct WatchdogFile {
    /// The process that opened it, for the disarm's line.
    pid: u32,
    /// Whether the last write held a `V`: the magic close.
    magic: AtomicBool,
    /// Whether this open armed the watchdog: false for one refused
    /// `EBUSY`, whose drop must not end the open that holds it.
    arms: AtomicBool,
}

impl WatchdogFile {
    /// Arm the watchdog for the caller.
    ///
    /// # Errors
    ///
    /// `ENXIO` without `ferrix.watchdog=on`; `EPERM` for an unprivileged
    /// caller; `EBUSY` while another open holds it; `ENOMEM` for the object,
    /// which leaves it unarmed.
    pub(crate) fn open() -> Result<Arc<WatchdogFile>> {
        crate::syscall::diag_exit::install_hang_dump();
        if !watchdog::served() {
            return Err(Errno::ENXIO);
        }
        let caller = process::current();
        if let Some(caller) = caller.as_ref() {
            credentials::require_privilege(caller)?;
        }
        let pid = caller.as_ref().map_or(0, |caller| caller.pid());
        let file = crate::fallible::try_arc(WatchdogFile {
            pid,
            magic: AtomicBool::new(false),
            arms: AtomicBool::new(false),
        })
        .map_err(|_| Errno::ENOMEM)?;
        let name = caller
            .as_ref()
            .map(|caller| caller.comm())
            .unwrap_or_default();
        watchdog::open(pid, &name).map_err(|_| Errno::EBUSY)?;
        file.arms.store(true, Ordering::Release);
        Ok(file)
    }
}

impl Drop for WatchdogFile {
    /// The open's last release: disarmed after the magic close, armed and
    /// counting otherwise.
    fn drop(&mut self) {
        if *self.arms.get_mut() {
            watchdog::release(self.pid, *self.magic.get_mut());
        }
    }
}

/// The open `/dev/watchdog` an open file reads and writes through, if it is
/// one.
pub(crate) fn of(io: &Arc<dyn Inode>) -> Option<Arc<WatchdogFile>> {
    Arc::clone(io).into_any().downcast::<WatchdogFile>().ok()
}

/// `ioctl` on an open `/dev/watchdog`.
///
/// # Errors
///
/// `EFAULT` for an argument outside the caller's memory; `EINVAL` for a
/// timeout out of range or an option word with neither bit; `ENOTTY` for
/// any other request.
pub(crate) fn ioctl(
    process: &Process,
    file: &WatchdogFile,
    request: u32,
    arg: u64,
) -> core::result::Result<usize, LinuxErrno> {
    match request {
        WDIOC_GETSUPPORT => {
            let mut info = [0_u8; 40];
            let options = WDIOF_SETTIMEOUT | WDIOF_MAGICCLOSE | WDIOF_KEEPALIVEPING;
            if let Some(to) = info.get_mut(..4) {
                to.copy_from_slice(&options.to_ne_bytes());
            }
            if let Some(to) = info.get_mut(8..8 + IDENTITY.len()) {
                to.copy_from_slice(IDENTITY);
            }
            put(process, arg, &info)
        }
        WDIOC_GETSTATUS => put_int(process, arg, 0),
        // No hardware timer stands behind it, so none reset the boot before.
        WDIOC_GETBOOTSTATUS => put_int(process, arg, 0),
        WDIOC_KEEPALIVE => {
            let _ = watchdog::feed();
            Ok(0)
        }
        WDIOC_SETTIMEOUT => {
            let seconds = get_int(process, arg)?;
            let set = watchdog::set_timeout(seconds).ok_or(LinuxErrno::EINVAL)?;
            put_int(process, arg, set)
        }
        WDIOC_GETTIMEOUT => put_int(process, arg, watchdog::timeout()),
        WDIOC_GETTIMELEFT => {
            let left = watchdog::time_left().unwrap_or(0);
            put_int(process, arg, u32::try_from(left).unwrap_or(u32::MAX))
        }
        WDIOC_SETOPTIONS => {
            let options = get_int(process, arg)?;
            if options & WDIOS_DISABLECARD != 0 {
                watchdog::disable(file.pid);
                Ok(0)
            } else if options & WDIOS_ENABLECARD != 0 {
                watchdog::enable();
                Ok(0)
            } else {
                Err(LinuxErrno::EINVAL)
            }
        }
        _ => Err(LinuxErrno::ENOTTY),
    }
}

/// Read a request's `int` argument.
fn get_int(process: &Process, at: u64) -> core::result::Result<u32, LinuxErrno> {
    let mut bytes = [0_u8; 4];
    uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| LinuxErrno::EFAULT)?;
    Ok(u32::from_ne_bytes(bytes))
}

/// Copy an `int` answer to the caller.
fn put_int(process: &Process, at: u64, value: u32) -> core::result::Result<usize, LinuxErrno> {
    put(process, at, &value.to_ne_bytes())
}

/// Copy an answer to the caller.
fn put(process: &Process, at: u64, bytes: &[u8]) -> core::result::Result<usize, LinuxErrno> {
    uaccess::copy_to_user(process.space(), at, bytes).map_err(|_| LinuxErrno::EFAULT)?;
    Ok(0)
}

impl Inode for WatchdogFile {
    fn metadata(&self) -> Metadata {
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        Metadata {
            ino: 0,
            kind: FileType::CharDevice,
            permissions: 0o600,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: makedev(MAJOR, MINOR),
            blocks: 0,
            block_size: 4096,
            atime: zero,
            mtime: zero,
            ctime: zero,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        Err(Errno::EINVAL)
    }

    fn write_at(&self, _offset: u64, data: &[u8], _append: bool) -> Result<(usize, u64)> {
        self.write_stream(data, false).map(|count| (count, 0))
    }

    fn write_stream(&self, data: &[u8], _nonblock: bool) -> Result<usize> {
        if !data.is_empty() {
            self.magic.store(data.contains(&b'V'), Ordering::Release);
            let _ = watchdog::feed();
        }
        Ok(data.len())
    }

    fn splices_out(&self) -> bool {
        false
    }
}
