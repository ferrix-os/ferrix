//! One open of a chardev node: a file the driver knows by its identity.
//!
//! Opening sends OPEN and waits for the driver's answer; each ioctl sends
//! IOCTL with the command and argument as the program passed them; the last
//! close queues RELEASE in the slot the open reserved; an mmap sends MMAP
//! ([`mmap`]). Nothing else is forwarded: read and write answer `EINVAL`,
//! and poll is never ready (`docs/NVIDIA.md` §4.4, N11).

use alloc::sync::Arc;
use core::any::Any;

use ferrix_chardevctl::message::Op;
use ferrix_chardevctl::node::{MAJOR, MODE, RENDER_MINOR};
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileType, Inode, Metadata, Readiness, Result as VfsResult, Timespec};

use crate::syscall::process::{self, Process};

use super::{Answer, ApertureKeeper, Control, MapReply};

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

/// What `stat` says of `renderD<index>` when a chardev driver serves it:
/// the render core's major and the number it lent, mode 0666 as the
/// driver's other nodes (N3b).
pub(crate) fn render_metadata(index: u32) -> Metadata {
    Metadata {
        ino: (1u64 << 41).saturating_add(u64::from(index)),
        rdev: makedev(crate::interfaces::display::DRM_MAJOR, index),
        ..metadata(RENDER_MINOR)
    }
}

/// One open file.
pub(crate) struct ChardevFile {
    control: Arc<Control>,
    file: u64,
    minor: u16,
    /// For an open of `renderD<N>`: N.
    render: Option<u32>,
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
        Self::open_on(control, minor, None)
    }

    /// Open `renderD<index>`, which its driver knows as
    /// [`RENDER_MINOR`].
    ///
    /// # Errors
    ///
    /// As [`ChardevFile::open`].
    pub(crate) fn open_render(index: u32) -> VfsResult<Arc<Self>> {
        let control = super::render_control(index).ok_or(Errno::ENXIO)?;
        Self::open_on(control, RENDER_MINOR, Some(index))
    }

    fn open_on(control: Arc<Control>, minor: u16, render: Option<u32>) -> VfsResult<Arc<Self>> {
        let client = process::current().ok_or(Errno::ENXIO)?;
        super::hold_release(&control)?;
        let file = super::next_file(&control);
        if let Err(errno) = super::call(
            &control,
            &client,
            super::Ask {
                op: Op::Open,
                file,
                minor,
                cmd: 0,
                arg: 0,
                pages: 0,
            },
        ) {
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
            render,
        }) {
            Ok(opened) => Ok(opened),
            Err(_) => {
                super::queue_release(&control, file, minor);
                Err(Errno::ENOMEM)
            }
        }
    }
}

impl ChardevFile {
    /// The control serving it.
    pub(crate) fn control(&self) -> &Arc<Control> {
        &self.control
    }

    /// Its identity on its control.
    pub(crate) fn identity(&self) -> u64 {
        self.file
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
        self.render.map_or_else(|| metadata(self.minor), render_metadata)
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
    match super::call(
        &file.control,
        &client,
        super::Ask {
            op: Op::Ioctl,
            file: file.file,
            minor: file.minor,
            cmd: request,
            arg,
            pages: 0,
        },
    )? {
        Answer::Value(value) => Ok(value),
        Answer::Map(_) => Err(Errno::EIO),
    }
}

/// What an mmap of a chardev file is to map, and, for an aperture, the
/// keeper its region holds.
pub(crate) struct Mapped {
    /// The driver's checked answer.
    pub(crate) reply: MapReply,
    /// For an aperture: holds the device's claim while the region lives.
    pub(crate) keeper: Option<ApertureKeeper>,
}

/// `mmap` of an open chardev file: MMAP to its driver, refused first when
/// private, executable or empty (M4), and its answer once it is in. The
/// caller maps it, taking the program's address-space lock only now, so the
/// driver's copies into the same program never wait on it (M5).
///
/// # Errors
///
/// `EINVAL` for a private or empty mapping, `EPERM` for an executable one,
/// what the driver answered, `EIO` for an answer that is not a mapping, and
/// `ENODEV` once the driver is gone.
pub(crate) fn mmap(
    file: &ChardevFile,
    len: u64,
    prot: u32,
    flags: u32,
    offset: u64,
) -> Result<Mapped, Errno> {
    const PROT_EXEC: u32 = 4;
    const MAP_SHARED: u32 = 1;
    if flags & MAP_SHARED == 0 || len == 0 {
        return Err(Errno::EINVAL);
    }
    if prot & PROT_EXEC != 0 {
        return Err(Errno::EPERM);
    }
    let pages = u32::try_from(len.div_ceil(4096)).map_err(|_| Errno::EINVAL)?;
    if file.control.is_gone() {
        return Err(Errno::ENODEV);
    }
    let client: Arc<Process> = process::current().ok_or(Errno::ENODEV)?;
    let cmd = (prot & 0xff) | ((flags & 0xff_ffff) << 8);
    let answer = super::call(
        &file.control,
        &client,
        super::Ask {
            op: Op::Mmap,
            file: file.file,
            minor: file.minor,
            cmd,
            arg: offset,
            pages,
        },
    )?;
    let Answer::Map(reply) = answer else {
        return Err(Errno::EIO);
    };
    let keeper = match reply {
        MapReply::Aperture { .. } => Some(super::aperture_keeper(&file.control)?),
        MapReply::Vmo { .. } => None,
    };
    Ok(Mapped { reply, keeper })
}
