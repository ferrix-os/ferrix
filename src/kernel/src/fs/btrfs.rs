//! btrfs, mounted from a disk a ring-3 driver serves.
//!
//! `mount -t btrfs /dev/vda /mnt` names a block node. Opening a block node is
//! `ENXIO` here, so the mount does not open it: it takes the node's `st_rdev`
//! and asks devfs's registry for the [`BlockDevice`] behind that number, the
//! one the block ring's kernel side registered when the driver said hello.
//! Everything above that is `ferrix-btrfs-vfs`, which reads the volume through
//! a [`Device`] — bytes at physical offsets — and keeps file data in the pages
//! the kernel's [`VmoStorage`] lends it. [`Disk`] is the adapter between the
//! two: whole sectors on one side, byte offsets on the other.
//!
//! # Read-only
//!
//! Stage 11 is btrfs stage A, the read path. A mount that does not ask for
//! `MS_RDONLY` is refused with `EROFS`, so no program is told it has a
//! writable btrfs until stage 12 makes one; `syscall::fsctl` judges the flag
//! and this module never sees a mount that could write.
//!
//! # Clean tree nodes are reclaimable
//!
//! A writable mount keeps the tree nodes it read and did not change, a cache
//! of the disk, in kernel heap: up to 4096 of them between operations, about
//! twice a node's size each, on every writable volume. That heap is charged
//! to no job -- neither `ferrix-btrfs-write` nor `ferrix-btrfs-vfs` makes a
//! `ferrix_kmem` charge -- so a job's `memory.max` reclaim has no claim on
//! it, and only a reclaim of the whole machine takes it ([`drop_clean`],
//! asked by the filesystem's shrinker). Every writable mount is listed here
//! for that, weakly, and asked in turn: a volume an operation holds is
//! skipped, never waited for (`ferrix_btrfs_vfs::rw`, "Clean tree nodes are
//! given back between operations"). What they hold is counted for
//! `/proc/meminfo` ([`clean_bytes`]).
//!
//! # The device outlives its registration
//!
//! The registry hands out an `Arc`, and the mount keeps it for its life. A
//! disk whose driver has died answers `EIO` to every read, promptly, and the
//! mount then answers `EIO` to every read of a file — never a hang and never
//! a panic — until it is unmounted.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::volume::{Device, ReadKind};
use ferrix_btrfs_vfs::Btrfs;
use ferrix_btrfs_vfs::rw::RwBtrfs;
use ferrix_btrfs_write::{Error as WriteError, WriteDevice};
use ferrix_vfs::{Errno, FileSystem};

use super::block::BlockDevice;
use super::devfs;
use super::pages::VmoStorage;
use crate::fallible;
use crate::sync::SpinLock;

/// Every writable mount, weakly: those [`drop_clean`] asks.
static WRITABLE: SpinLock<Vec<Weak<RwBtrfs<Disk>>>> = SpinLock::new(Vec::new());

/// A registered disk as a btrfs volume reads it: bytes at physical offsets,
/// fetched in whole sectors.
///
/// Cloned for every operation, as `ferrix-btrfs-vfs` asks of its handle; a
/// clone is one `Arc` and the sector size.
#[derive(Clone)]
pub(crate) struct Disk {
    device: Arc<dyn BlockDevice>,
    /// Bytes in a sector: a power of two, checked once.
    sector: u64,
}

impl Disk {
    /// A handle on `device`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a sector size that is not a power of two, which no sector
    /// arithmetic below could serve.
    pub(crate) fn new(device: Arc<dyn BlockDevice>) -> Result<Disk, Errno> {
        let sector = u64::from(device.sector_size());
        if sector == 0 || !sector.is_power_of_two() {
            return Err(Errno::EINVAL);
        }
        Ok(Disk { device, sector })
    }
}

impl fmt::Debug for Disk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Disk")
            .field("sector", &self.sector)
            .field("sectors", &self.device.sectors())
            .finish_non_exhaustive()
    }
}

impl Device for Disk {
    /// Read `buf.len()` bytes at `physical`.
    ///
    /// A read that starts and ends on a sector boundary goes straight into
    /// `buf`, which is every read btrfs makes on a disk whose sectors are no
    /// larger than its own: nodes and extents sit on sector boundaries. One
    /// that does not is read into a bounce buffer of whole sectors and copied
    /// out, so the trait's contract holds on any sector size. Any failure,
    /// including a read past the end of the disk, is `DeviceRead` at the
    /// offset asked for; the volume reader turns that into `EIO`. No memory
    /// for the bounce buffer is `OutOfMemory`, which it turns into `ENOMEM`.
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        _kind: ReadKind,
    ) -> Result<(), BtrfsError> {
        let failed = BtrfsError::DeviceRead { physical };
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        let end = physical.checked_add(len).ok_or(failed)?;
        let first = physical / self.sector;
        if physical.is_multiple_of(self.sector) && len.is_multiple_of(self.sector) {
            return self.device.read(first, buf).map_err(|_| failed);
        }
        let sectors = end.div_ceil(self.sector).saturating_sub(first);
        let bytes = usize::try_from(sectors.saturating_mul(self.sector)).map_err(|_| failed)?;
        let mut bounce = fallible::try_filled(0u8, bytes).map_err(|_| BtrfsError::OutOfMemory)?;
        self.device.read(first, &mut bounce).map_err(|_| failed)?;
        let skip = usize::try_from(physical - first * self.sector).map_err(|_| failed)?;
        let taken = bounce.get(skip..skip + buf.len()).ok_or(failed)?;
        buf.copy_from_slice(taken);
        Ok(())
    }
}

impl WriteDevice for Disk {
    /// Write `data` at `physical`.
    ///
    /// The write path writes whole nodes, whole sectors of data and whole
    /// superblocks, so on a disk whose sectors are no larger than the
    /// volume's this is always sector-aligned. One that is not is done as a
    /// read, a patch and a write of the sectors it touches, which keeps the
    /// trait's contract on any sector size; nothing in a commit takes that
    /// path.
    fn write_at(&mut self, physical: u64, data: &[u8]) -> Result<(), WriteError> {
        let failed = WriteError::DeviceWrite { physical };
        if data.is_empty() {
            return Ok(());
        }
        let len = data.len() as u64;
        let end = physical.checked_add(len).ok_or(failed)?;
        let first = physical / self.sector;
        if physical.is_multiple_of(self.sector) && len.is_multiple_of(self.sector) {
            return self.device.write(first, data).map_err(|_| failed);
        }
        let sectors = end.div_ceil(self.sector).saturating_sub(first);
        let bytes = usize::try_from(sectors.saturating_mul(self.sector)).map_err(|_| failed)?;
        let mut bounce = fallible::try_filled(0u8, bytes).map_err(|_| WriteError::OutOfMemory)?;
        self.device.read(first, &mut bounce).map_err(|_| failed)?;
        let skip = usize::try_from(physical - first * self.sector).map_err(|_| failed)?;
        let patch = bounce.get_mut(skip..skip + data.len()).ok_or(failed)?;
        patch.copy_from_slice(data);
        self.device.write(first, &bounce).map_err(|_| failed)
    }

    /// Everything written is durable when this returns: the driver's flush,
    /// which the commit puts between its nodes and its superblock.
    fn flush(&mut self) -> Result<(), WriteError> {
        self.device.flush().map_err(|_| WriteError::Flush)
    }
}

/// Mount the btrfs volume on the registered disk numbered `rdev`, read-only.
///
/// The mount reports `rdev` as every inode's `st_dev`, as Linux reports the
/// block device a disk filesystem is on, and reads file data through the
/// kernel's VMO pages.
///
/// # Errors
///
/// `ENXIO` for a number no registered disk has, `EINVAL` for a disk that
/// holds no btrfs volume this reader reads (a log tree, an unknown feature,
/// a checksum type other than CRC-32C), and `EIO` for one that cannot be
/// read.
pub(crate) fn mount(rdev: u64) -> Result<Arc<dyn FileSystem>, Errno> {
    let device = devfs::block_device(rdev).ok_or(Errno::ENXIO)?;
    let disk = Disk::new(device)?;
    let volume = Btrfs::mount(disk, rdev, Arc::new(VmoStorage))?;
    Ok(volume as Arc<dyn FileSystem>)
}

/// The label of the btrfs volume on the disk numbered `rdev`, or `None` for
/// a disk that is not one, cannot be read, or has none: the primary
/// superblock's name field, read off the disk without mounting it, which is
/// what `root=LABEL=` looks at on Linux.
pub(crate) fn label(rdev: u64) -> Option<Vec<u8>> {
    /// The primary superblock, and its magic and label within it.
    const SUPERBLOCK_SECTOR: u64 = 0x1_0000 / 512;
    const MAGIC: core::ops::Range<usize> = 0x40..0x48;
    const LABEL: core::ops::Range<usize> = 0x12b..0x22b;
    let device = devfs::block_device(rdev)?;
    let mut block = alloc::vec![0u8; 4096];
    device.read(SUPERBLOCK_SECTOR, &mut block).ok()?;
    if block.get(MAGIC)? != b"_BHRfS_M" {
        return None;
    }
    let field = block.get(LABEL)?;
    let len = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    let name = field.get(..len)?;
    (!name.is_empty()).then(|| name.to_vec())
}

/// Mount the btrfs volume on the registered disk numbered `rdev` for writing.
///
/// Stage 12's mount. The volume must be one `ferrix-btrfs-write` maintains —
/// a single device, no subvolume but the top-level one, no quotas, no
/// unreplayed log — and the disk must take writes; everything else is
/// `EROFS`, and mounting read-only still works for all of them.
///
/// # Errors
///
/// `ENXIO` for a number no registered disk has, `EROFS` for a disk or a
/// volume that cannot be written, `EINVAL` for a disk that holds no btrfs
/// volume, and `EIO` for one that cannot be read.
pub(crate) fn mount_rw(rdev: u64) -> Result<Arc<dyn FileSystem>, Errno> {
    let device = devfs::block_device(rdev).ok_or(Errno::ENXIO)?;
    if device.read_only() {
        return Err(Errno::EROFS);
    }
    let disk = Disk::new(device)?;
    let volume = RwBtrfs::mount(
        disk,
        rdev,
        Arc::new(VmoStorage),
        super::clock(),
        &crate::sync::SchedParker,
        |why| crate::console::println!("  btrfs    {why}"),
    )?;
    list_writable(&volume)?;
    Ok(volume as Arc<dyn FileSystem>)
}

/// List `volume` for [`drop_clean`] and [`clean_bytes`], dropping the
/// entries of mounts that are gone first.
///
/// # Errors
///
/// `ENOMEM` when the list cannot grow: the mount is refused rather than
/// left with a cache no reclaim reaches.
fn list_writable(volume: &Arc<RwBtrfs<Disk>>) -> Result<(), Errno> {
    let mut listed = WRITABLE.lock();
    listed.retain(|entry| entry.strong_count() > 0);
    fallible::try_push(&mut listed, Arc::downgrade(volume)).map_err(|_| Errno::ENOMEM)
}

/// The writable mounts alive now, or none with no memory to list them. The
/// list's lock is let go before any is asked anything.
fn writable() -> Vec<Arc<RwBtrfs<Disk>>> {
    let listed = WRITABLE.lock();
    let Ok(mut out) = fallible::try_with_capacity(listed.len()) else {
        return Vec::new();
    };
    for entry in listed.iter() {
        if let Some(volume) = entry.upgrade() {
            // The room was had above.
            let _ = fallible::push_within(&mut out, volume);
        }
    }
    out
}

/// Give back up to `want` bytes of the writable mounts' clean tree nodes,
/// oldest first: a batch of at most 64 nodes from each mount no operation
/// holds, until `want` is met. Answers the bytes given. For a reclaim of the
/// whole machine only; see "Clean tree nodes are reclaimable".
///
/// Must be called where a task may sleep and with no lock held, as the
/// reclaim that calls it is: a volume's lock is a sleeping lock, taken here
/// only if it is free.
pub(crate) fn drop_clean(want: u64) -> u64 {
    let mut given = 0u64;
    for volume in writable() {
        if given >= want {
            break;
        }
        given = given.saturating_add(volume.drop_clean(want - given));
    }
    given
}

/// The writable mount `filesystem` is, if it is listed: for the checks,
/// which ask one volume what it holds.
pub(crate) fn writable_of(filesystem: &Arc<dyn FileSystem>) -> Option<Arc<RwBtrfs<Disk>>> {
    writable()
        .into_iter()
        .find(|volume| core::ptr::addr_eq(Arc::as_ptr(volume), Arc::as_ptr(filesystem)))
}

/// What the writable mounts' clean tree nodes hold of the heap, for
/// `/proc/meminfo`: each volume's exact count as of its lock's last release.
pub(crate) fn clean_bytes() -> u64 {
    writable()
        .iter()
        .map(|volume| volume.clean_bytes())
        .fold(0, u64::saturating_add)
}
