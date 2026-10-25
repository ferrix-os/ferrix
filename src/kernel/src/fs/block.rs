//! What the kernel needs from a disk: whole sectors, read and written on
//! request, and a flush that says when they are durable.
//!
//! Stage 11 mounts a btrfs volume from a virtio-blk disk served by a ring-3
//! driver, and `mount(2)` names that disk by a node in `/dev`. A mount does
//! not open the node: it resolves the node's `st_rdev` through the registry
//! in [`crate::fs::devfs`] and reads through the device that returns. A
//! program that opens the node reads and writes the same device through
//! [`crate::fs::disk_file`].
//! This trait is all that registry knows about a disk, which is what lets it
//! exist before stage 11's concrete kernel disk does.
//!
//! # Lifetime
//!
//! The registry hands out an `Arc`, and a mount keeps it. So a disk can
//! outlive its registration — its driver died, devmgr removed it, a reset is
//! pending — and a device in that state has to keep answering: every read
//! fails with `EIO`, promptly, and nothing panics.

use core::fmt;

use ferrix_vfs::Errno;

/// A disk: a run of equal-sized sectors that can be read.
pub(crate) trait BlockDevice: Send + Sync + fmt::Debug {
    /// Read whole sectors from `sector` into `buf`, whose length is a whole
    /// number of sectors. Called with no spin lock held; may sleep on the
    /// device. Past the end, or a device failure, is an error. A device that
    /// has gone away (driver dead, unregistered, reset pending) returns EIO:
    /// it never blocks forever and never panics — a mount may outlive the
    /// registration by holding the Arc.
    ///
    /// A read may be any number of sectors. The implementation splits it into
    /// as many requests as its device needs, so a caller never learns the
    /// device's largest request. A `buf` that is empty, or whose length is not
    /// a whole number of sectors, is `EINVAL`, never a panic.
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno>;

    /// How many sectors the disk has.
    ///
    /// This and the other two geometry answers are fixed for the device's
    /// life. They keep their last values after the device has gone away and
    /// never panic, because the registry calls them with its lock released,
    /// possibly after the driver has died.
    fn sectors(&self) -> u64;

    /// How many bytes a sector is. Fixed for the device's life, as
    /// [`BlockDevice::sectors`] says.
    fn sector_size(&self) -> u32;

    /// Whether the disk refuses writes. Fixed for the device's life, as
    /// [`BlockDevice::sectors`] says.
    fn read_only(&self) -> bool;

    /// Whether the disk was taken offline, its every request answered EIO
    /// at once: a block ring's disk whose driver went silent
    /// (`interfaces::block_ring`, "A disk that stops answering"). `false`
    /// for a disk that cannot be.
    fn offline(&self) -> bool {
        false
    }

    /// Write whole sectors from `buf`, whose length is a whole number of
    /// sectors, at `sector`. Called with no spin lock held; may sleep.
    ///
    /// The bytes are on the device when this returns, but need not be
    /// durable: only [`BlockDevice::flush`] promises that. A device that has
    /// gone away answers `EIO`, as a read does.
    ///
    /// The default is `EROFS`, which is what a disk that cannot write should
    /// answer; a caller must check [`BlockDevice::read_only`] first if it
    /// wants a clearer refusal.
    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), Errno> {
        let _ = (sector, buf);
        Err(Errno::EROFS)
    }

    /// Make every write that has completed durable, and do not return until
    /// it is. This is the ordering a filesystem's commit rests on.
    ///
    /// The default is `Ok`, which is honest for a device that takes no
    /// writes: there is nothing of its caller's on it to lose.
    fn flush(&self) -> Result<(), Errno> {
        Ok(())
    }
}

/// The disk's size in bytes, saturating rather than wrapping for a size no
/// `u64` holds.
pub(crate) fn size_in_bytes(device: &dyn BlockDevice) -> u64 {
    device
        .sectors()
        .saturating_mul(u64::from(device.sector_size()))
}
