//! A disk opened as a file: what `open("/dev/vda")` reads and writes.
//!
//! Until the installer (`docs/INSTALLER.md` §5.1) a block node's descriptor
//! was `ENXIO`, and only a mount reached a disk, through the registry's
//! [`BlockDevice`]. A program that partitions and formats a disk needs the
//! bytes themselves, as Linux gives them: `read`, `write`, `pread`, `pwrite`,
//! `lseek` with `SEEK_END` at the disk's size, `fsync` as a flush, and the
//! `BLK*` requests `blockdev`, `sfdisk` and `mkfs` ask. This is that open
//! file, and [`ioctl`] is those requests.
//!
//! # Bytes, not sectors
//!
//! A program may read or write any range, as it may on Linux, where the page
//! cache hides the sector. Here there is no cache: an aligned run of whole
//! sectors goes to the device in one request, and a sector the range covers
//! only in part is read, patched and written back -- a read-modify-write
//! per open, which two programs writing parts of one sector at once can
//! interleave. Linux's cache serialises them; nothing that partitions or
//! formats a disk writes half-sectors from two places, so this does not.
//!
//! # Past the end
//!
//! A read at or past the end is end of file, and one that runs over it is
//! short. A write that starts at or past the end is `ENOSPC` and one that runs
//! over it is short, as Linux's `blkdev_write_iter` answers.
//!
//! # Not coherent with a mount
//!
//! A mounted btrfs reads through the same device but caches what it read, so
//! bytes written here under a mount are not seen by it, and bytes it commits
//! overwrite them. Linux is the same without `O_EXCL`; the installer never
//! writes a disk that has anything mounted from it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_linux_abi::types::{
    BLKALIGNOFF, BLKBSZGET, BLKFLSBUF, BLKGETSIZE, BLKGETSIZE64, BLKIOMIN, BLKIOOPT, BLKPBSZGET,
    BLKROGET, BLKROTATIONAL, BLKRRPART, BLKSSZGET,
};
use ferrix_vfs::{Errno, FileType, Inode, Metadata, Result, Timespec};

use crate::fs::block::{self, BlockDevice};
use crate::fs::devfs;
use crate::syscall::process::Process;
use crate::syscall::uaccess;

/// The block size `stat` and `BLKBSZGET` report: the page, as Linux's
/// default soft block size for a disk whose sectors are no larger.
const BLOCK_SIZE: u32 = 4096;

/// The largest sector this reads a piece of: a sector patched in a
/// read-modify-write is staged in memory, and no disk here has one larger.
const LARGEST_SECTOR: u32 = 4096;

/// An open disk.
#[derive(Debug)]
pub(crate) struct DiskFile {
    /// The disk, kept for as long as the descriptor is open: a driver that
    /// dies under it makes every read `EIO`, as [`BlockDevice`] promises.
    device: Arc<dyn BlockDevice>,
    /// Its number, for what `metadata` reports.
    rdev: u64,
}

impl DiskFile {
    /// Open the registered disk numbered `rdev`, for writing if `write`.
    ///
    /// # Errors
    ///
    /// `ENXIO` for a number no disk has, and `EACCES` for writing a disk
    /// that refuses writes, as Linux's `bdev_permission` answers.
    pub(crate) fn open(rdev: u64, write: bool) -> Result<Arc<dyn Inode>> {
        let device = devfs::block_device(rdev).ok_or(Errno::ENXIO)?;
        if write && device.read_only() {
            return Err(Errno::EACCES);
        }
        let sector = device.sector_size();
        if sector == 0 || sector > LARGEST_SECTOR || !sector.is_power_of_two() {
            return Err(Errno::ENXIO);
        }
        Ok(Arc::new(DiskFile { device, rdev }))
    }

    /// The disk's size in bytes.
    fn size(&self) -> u64 {
        block::size_in_bytes(self.device.as_ref())
    }

    /// Bytes in a sector, checked on open to be a power of two no larger
    /// than [`LARGEST_SECTOR`].
    fn sector(&self) -> u64 {
        u64::from(self.device.sector_size())
    }

    /// A buffer of one sector, for a read-modify-write.
    fn staging(&self) -> Result<Vec<u8>> {
        let len = usize::try_from(self.sector()).map_err(|_| Errno::EINVAL)?;
        let mut staged = Vec::new();
        staged.try_reserve_exact(len).map_err(|_| Errno::ENOMEM)?;
        staged.resize(len, 0);
        Ok(staged)
    }

    /// Read `buf.len()` bytes at `offset`, which the caller has kept inside
    /// the disk.
    fn read_inside(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let sector = self.sector();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let number = at / sector;
            let skip = usize::try_from(at % sector).map_err(|_| Errno::EINVAL)?;
            let rest = buf.get_mut(done..).ok_or(Errno::EINVAL)?;
            let whole = whole_sectors(rest.len(), sector);
            if skip == 0 && whole > 0 {
                let run = rest.get_mut(..whole).ok_or(Errno::EINVAL)?;
                self.device.read(number, run)?;
                done += whole;
                continue;
            }
            let mut staged = self.staging()?;
            self.device.read(number, &mut staged)?;
            let count = rest.len().min(staged.len() - skip);
            let piece = staged.get(skip..skip + count).ok_or(Errno::EINVAL)?;
            rest.get_mut(..count)
                .ok_or(Errno::EINVAL)?
                .copy_from_slice(piece);
            done += count;
        }
        Ok(())
    }

    /// Write `data` at `offset`, which the caller has kept inside the disk.
    fn write_inside(&self, offset: u64, data: &[u8]) -> Result<()> {
        let sector = self.sector();
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let number = at / sector;
            let skip = usize::try_from(at % sector).map_err(|_| Errno::EINVAL)?;
            let rest = data.get(done..).ok_or(Errno::EINVAL)?;
            let whole = whole_sectors(rest.len(), sector);
            if skip == 0 && whole > 0 {
                let run = rest.get(..whole).ok_or(Errno::EINVAL)?;
                self.device.write(number, run)?;
                done += whole;
                continue;
            }
            let mut staged = self.staging()?;
            self.device.read(number, &mut staged)?;
            let count = rest.len().min(staged.len() - skip);
            staged
                .get_mut(skip..skip + count)
                .ok_or(Errno::EINVAL)?
                .copy_from_slice(rest.get(..count).ok_or(Errno::EINVAL)?);
            self.device.write(number, &staged)?;
            done += count;
        }
        Ok(())
    }
}

/// How many bytes of `len` are whole sectors of `sector` bytes.
fn whole_sectors(len: usize, sector: u64) -> usize {
    let sector = usize::try_from(sector).unwrap_or(usize::MAX);
    len - len % sector
}

impl Inode for DiskFile {
    /// Only its size and number are read: `fstat` reports the node that was
    /// opened, and `lseek(SEEK_END)` asks this for the end.
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0,
            kind: FileType::BlockDevice,
            permissions: 0o660,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: self.size(),
            rdev: self.rdev,
            blocks: 0,
            block_size: BLOCK_SIZE,
            atime: Timespec::default(),
            mtime: Timespec::default(),
            ctime: Timespec::default(),
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let size = self.size();
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let count = usize::try_from(size - offset).map_or(buf.len(), |left| left.min(buf.len()));
        self.read_inside(offset, buf.get_mut(..count).ok_or(Errno::EINVAL)?)?;
        Ok(count)
    }

    /// `append` is ignored, as it is for a Linux disk: the end of a disk is
    /// not somewhere a write can go.
    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64)> {
        let _ = append;
        if data.is_empty() {
            return Ok((0, offset));
        }
        let size = self.size();
        if offset >= size {
            return Err(Errno::ENOSPC);
        }
        let count = usize::try_from(size - offset).map_or(data.len(), |left| left.min(data.len()));
        self.write_inside(offset, data.get(..count).ok_or(Errno::EINVAL)?)?;
        Ok((count, offset + count as u64))
    }

    /// A flush of the disk: every write that has returned is durable once
    /// this does. `fdatasync` is the same, a disk having no metadata apart
    /// from its bytes.
    fn fsync(&self, data_only: bool) -> Result<()> {
        let _ = data_only;
        self.device.flush()
    }
}

/// The open disk behind `io`, if it is one.
pub(crate) fn of(io: &Arc<dyn Inode>) -> Option<Arc<DiskFile>> {
    Arc::clone(io).into_any().downcast::<DiskFile>().ok()
}

/// `ioctl` on an open disk: the `BLK*` requests of `linux/fs.h` that tell a
/// program the disk's geometry, and a flush. Everything else is `ENOTTY`.
///
/// # Errors
///
/// `EFAULT` for an answer that cannot be written; `EFBIG` for a sector count
/// an `unsigned long` does not hold; `EINVAL` for `BLKRRPART`, since no
/// partition table is read yet -- Linux's answer for a disk that cannot
/// have partitions.
pub(crate) fn ioctl(process: &Process, disk: &DiskFile, request: u32, arg: u64) -> Result<usize> {
    let sector = disk.device.sector_size();
    match request {
        BLKGETSIZE64 => put(process, arg, &disk.size().to_ne_bytes()),
        BLKGETSIZE => {
            let sectors = usize::try_from(disk.size() / 512).map_err(|_| Errno::EFBIG)?;
            put(process, arg, &sectors.to_ne_bytes())
        }
        BLKSSZGET => put(process, arg, &sector.to_ne_bytes()),
        BLKPBSZGET | BLKIOMIN => put(process, arg, &sector.to_ne_bytes()),
        BLKBSZGET => put(process, arg, &BLOCK_SIZE.max(sector).to_ne_bytes()),
        BLKIOOPT | BLKALIGNOFF => put(process, arg, &0_u32.to_ne_bytes()),
        BLKROGET => put(
            process,
            arg,
            &i32::from(disk.device.read_only()).to_ne_bytes(),
        ),
        BLKROTATIONAL => put(process, arg, &0_u16.to_ne_bytes()),
        BLKFLSBUF => disk.device.flush().map(|()| 0),
        BLKRRPART => Err(Errno::EINVAL),
        _ => Err(Errno::ENOTTY),
    }
}

/// Write `bytes` to the caller's `arg`, answering 0.
fn put(process: &Process, arg: u64, bytes: &[u8]) -> Result<usize> {
    uaccess::copy_to_user(process.space(), arg, bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}
