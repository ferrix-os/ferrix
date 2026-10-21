//! Making a new volume, whatever its filesystem (`docs/FS-COMMON.md`).
//!
//! A filesystem's mkfs is pure: it turns what the volume is to be into an
//! [`Image`], the bytes to write and where, and [`Image::write_to`] writes
//! them in the one order that keeps a half-written volume from looking like
//! a whole one: the old start of the device wiped, every structure but the
//! one that makes the volume a volume, a flush, and only then that one (a
//! btrfs superblock, an ext4 superblock, a FAT boot sector), primary first.
//!
//! [`Mkfs`] is what each filesystem's mkfs implements: `ferrix-btrfs-mkfs`
//! today, a FAT or ext4 one later.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use ferrix_fs_device::{DeviceError, WriteDevice};

/// A filesystem's mkfs.
pub trait Mkfs {
    /// What the volume is to be: its size, label, identifiers.
    type Options<'a>;
    /// Why no volume was made.
    type Error: fmt::Display;

    /// Make the volume `options` describe, as writes.
    fn make(options: &Self::Options<'_>) -> Result<Image, Self::Error>;
}

/// A new volume as writes to its device, offsets from the device's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The volume's size: the device's, in whole sectors.
    pub size: u64,
    /// Bytes to zero first, so that nothing of an old file system at the
    /// device's start survives.
    pub zero: Range<u64>,
    /// Every structure but the ones that make the device a volume.
    pub blocks: Vec<(u64, Vec<u8>)>,
    /// What makes the device a volume -- the superblock's copies -- the
    /// primary first, written only once everything else is durable.
    pub superblocks: Vec<(u64, Vec<u8>)>,
}

impl Image {
    /// Write the volume to `device`: [`Image::zero`], the blocks, a flush,
    /// the primary superblock, a flush, the other copies, a flush. Until the
    /// primary is down the device holds no superblock, so a write cut short
    /// leaves no volume rather than half of one.
    pub fn write_to<D: WriteDevice + ?Sized>(&self, device: &mut D) -> Result<(), DeviceError> {
        let wipe = usize::try_from(self.zero.end.saturating_sub(self.zero.start)).unwrap_or(0);
        device.write_at(self.zero.start, &vec![0_u8; wipe])?;
        for (at, block) in &self.blocks {
            device.write_at(*at, block)?;
        }
        device.flush()?;
        let mut superblocks = self.superblocks.iter();
        if let Some((at, primary)) = superblocks.next() {
            device.write_at(*at, primary)?;
            device.flush()?;
        }
        for (at, mirror) in superblocks {
            device.write_at(*at, mirror)?;
        }
        device.flush()
    }
}
