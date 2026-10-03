//! One open of a chardev node: a file the driver knows by its identity.
//!
//! Opening sends OPEN and waits for the driver's answer; each ioctl sends
//! IOCTL with the command and argument as the program passed them; the last
//! close queues RELEASE in the slot the open reserved. Nothing else is
//! forwarded: read and write answer `EINVAL`, poll is never ready, and
//! mmap finds no pages and answers `ENODEV` (`docs/NVIDIA.md` §4.4, N11).

use alloc::sync::Arc;
use core::any::Any;

use ferrix_chardevctl::message::Op;
use ferrix_chardevctl::node::{MAJOR, MODE};
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileType, Inode, Metadata, Readiness, Result as VfsResult, Timespec};

use crate::syscall::process::{self, Process};

use super::Control;

/// Where the nodes' inode numbers start, beside devfs's own.
pub(crate) const INO_BASE: u64 = 1 << 43;

/// A node's metadata, whether or not it is published.
pub(crate) fn metadata(minor: u16) -> Metadata {
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    Metadata {
        ino: INO_BASE.saturating_add(u64::from(minor)),
        kind: FileType::CharDevice,
        permissions: MODE,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        rdev: makedev(MAJOR, u32::from(minor)),
        blocks: 0,
        block_size: 4096,
        atime: zero,
        mtime: zero,
        ctime: zero,
    }
}

/// One open file.
pub(crate) struct ChardevFile {
    control: Arc<Control>,
    file: u64,
    minor: u16,
}

impl ChardevFile {
    /// Open minor `minor`: OPEN to its driver, and the file once it agrees.
    ///
    /// # Errors
    ///
    /// `ENXIO` for a node nobody serves, `ENODEV` once its driver is gone,
    /// or what the driver or the wait answered.
    pub(crate) fn open(minor: u16) -> VfsResult<Arc<Self>> {
        let control = super::published(minor).ok_or(Errno::ENXIO)?;
        let client = process::current().ok_or(Errno::ENXIO)?;
        super::hold_release(&control)?;
        let file = super::next_file(&control);
        if let Err(errno) = super::call(&control, &client, Op::Open, file, minor, 0, 0) {
            // A refused open leaves nothing at the driver; an abandoned one
            // may have opened it before the program gave up. Either way its
            // release follows the open, in the slot held for it, and the
            // driver ignores the release of a file it does not have.
            super::queue_release(&control, file, minor);
            return Err(errno);
        }
        match crate::fallible::try_arc(Self {
            control: Arc::clone(&control),
            file,
            minor,
        }) {
            Ok(opened) => Ok(opened),
            Err(_) => {
                super::queue_release(&control, file, minor);
                Err(Errno::ENOMEM)
            }
        }
    }
}

impl Drop for ChardevFile {
    fn drop(&mut self) {
        super::queue_release(&self.control, self.file, self.minor);
    }
}

impl core::fmt::Debug for ChardevFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ChardevFile")
            .field("file", &self.file)
            .field("minor", &self.minor)
            .finish()
    }
}

impl Inode for ChardevFile {
    fn metadata(&self) -> Metadata {
        metadata(self.minor)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn splices_out(&self) -> bool {
        false
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn read_stream(&self, _buf: &mut [u8], _nonblock: bool) -> VfsResult<usize> {
        Err(Errno::EINVAL)
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(Errno::EINVAL)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8], _append: bool) -> VfsResult<(usize, u64)> {
        Err(Errno::EINVAL)
    }

    fn poll(&self) -> Readiness {
        let gone = self.control.is_gone();
        Readiness {
            readable: false,
            writable: false,
            hangup: gone,
            error: gone,
            priority: false,
        }
    }
}

/// The chardev file behind an open file, if it is one.
pub(crate) fn of(io: &Arc<dyn Inode>) -> Option<Arc<ChardevFile>> {
    Arc::clone(io).into_any().downcast::<ChardevFile>().ok()
}

/// An ioctl on an open chardev file: IOCTL to its driver, its answer back.
///
/// # Errors
///
/// `ENODEV` once the driver is gone, `EBUSY`, `EINTR`, or the errno the
/// driver answered.
pub(crate) fn ioctl(file: &ChardevFile, request: u32, arg: u64) -> Result<usize, Errno> {
    if file.control.is_gone() {
        return Err(Errno::ENODEV);
    }
    let client: Arc<Process> = process::current().ok_or(Errno::ENODEV)?;
    super::call(&file.control, &client, Op::Ioctl, file.file, file.minor, request, arg)
}
