//! The write path's calls on its device, each failure turned into the
//! write path's [`Error`] in one place.
//!
//! The device is `ferrix-fs-device`'s, which every on-disk filesystem
//! shares and which knows nothing of btrfs. What it says went wrong, it
//! says without knowing what the write path asked: so the mapping is by
//! operation, with the address the write path asked for, and every arm is
//! named, so that a new kind of device failure breaks the build here rather
//! than falling into a wildcard.

use ferrix_btrfs::BtrfsError;
use ferrix_fs_device::{Device, DeviceError, ReadKind, WriteDevice};

use crate::{Error, Result};

/// What the write path asked of the device when it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// A read at this physical offset.
    Read(u64),
    /// A write at this physical offset, durable or not.
    Write(u64),
    /// A flush.
    Flush,
}

/// The write path's error for a device's failure at what was `asked`:
/// [`Error::OutOfMemory`] passed on as it is; a failed read
/// [`BtrfsError::DeviceRead`] at the address asked; a failed write
/// [`Error::DeviceWrite`] there, or [`Error::Flush`] when it was the flush
/// of a durable write; a failed flush [`Error::Flush`].
#[must_use]
pub const fn device_error(asked: Asked, error: DeviceError) -> Error {
    match (asked, error) {
        (_, DeviceError::OutOfMemory) => Error::OutOfMemory,
        (
            Asked::Read(physical),
            DeviceError::Read { .. } | DeviceError::Write { .. } | DeviceError::Flush,
        ) => Error::Volume(BtrfsError::DeviceRead { physical }),
        (Asked::Write(physical), DeviceError::Read { .. } | DeviceError::Write { .. }) => {
            Error::DeviceWrite { physical }
        }
        (Asked::Write(_) | Asked::Flush, DeviceError::Flush)
        | (Asked::Flush, DeviceError::Read { .. } | DeviceError::Write { .. }) => Error::Flush,
    }
}

/// Read `buf` at `physical`.
pub(crate) fn read<D: Device + ?Sized>(
    device: &mut D,
    physical: u64,
    buf: &mut [u8],
    kind: ReadKind,
) -> Result<()> {
    device
        .read_at(physical, buf, kind)
        .map_err(|error| device_error(Asked::Read(physical), error))
}

/// Write `data` at `physical`.
pub(crate) fn write<D: WriteDevice + ?Sized>(
    device: &mut D,
    physical: u64,
    data: &[u8],
) -> Result<()> {
    device
        .write_at(physical, data)
        .map_err(|error| device_error(Asked::Write(physical), error))
}

/// Write `data` at `physical` and make it durable before returning.
pub(crate) fn write_durable<D: WriteDevice + ?Sized>(
    device: &mut D,
    physical: u64,
    data: &[u8],
) -> Result<()> {
    device
        .write_durable(physical, data)
        .map_err(|error| device_error(Asked::Write(physical), error))
}

/// Make every write that has returned durable.
pub(crate) fn flush<D: WriteDevice + ?Sized>(device: &mut D) -> Result<()> {
    device
        .flush()
        .map_err(|error| device_error(Asked::Flush, error))
}
