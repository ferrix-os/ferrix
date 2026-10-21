//! The device a filesystem lives on, as every on-disk filesystem reads and
//! writes it (`docs/FS-COMMON.md`).
//!
//! Byte-addressed reads with a hint of what they are for, writes, a flush
//! and a durable write: nothing in them is one format's. btrfs's reader and
//! write path take their device through these traits, and ext4, NTFS or FAT
//! take theirs the same way. Each filesystem turns a [`DeviceError`] into
//! its own error where it calls the device, keeping the address it asked
//! for.
//!
//! This crate is item code: `ferrix-btrfs` and `ferrix-btrfs-write` stand
//! on it. Its traits and its error change only under review; what another
//! filesystem needs and btrfs does not -- a size query, discard, a partition
//! offset -- goes into an extension trait in a crate of its own.

#![no_std]
#![forbid(unsafe_code)]

/// What a read through a [`Device`] is for, so a device that caches can keep
/// only what is worth keeping.
///
/// Tree nodes are read again by every lookup that descends past them; file data
/// is read once, and belongs in a page cache above the filesystem. A device
/// that caches should keep only [`ReadKind::Metadata`], so a node cache and the
/// page cache never hold the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadKind {
    /// The superblock or a tree node.
    Metadata,
    /// File data: an extent's bytes, compressed or not.
    Data,
}

/// Why a device did not do what it was asked.
///
/// Not `non_exhaustive`: a new variant must break every filesystem's
/// mapping of it rather than fall into a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    /// A read failed or ran past the end of the device, at this offset.
    Read {
        /// Where the read began.
        at: u64,
    },
    /// A write failed or ran past the end of the device, at this offset.
    Write {
        /// Where the write began.
        at: u64,
    },
    /// The device could not make its writes durable.
    Flush,
    /// The device could not get the memory the operation needed. It says
    /// nothing about the medium: a filesystem passes it on as it is, and
    /// never retries another copy for it or reports it as damage.
    OutOfMemory,
}

/// Byte-addressed access to the one device a volume lives on.
///
/// A filesystem reads whole nodes and whole extents, so an implementation
/// over a block cache can serve every call from cached sectors.
pub trait Device {
    /// Fill `buf` with the device's bytes starting at `physical`.
    ///
    /// A short device or a failed read is an error, typically
    /// [`DeviceError::Read`]; a partially filled buffer must never be
    /// reported as success. A read that could not get the memory it needed
    /// is [`DeviceError::OutOfMemory`], which every caller passes on as it is:
    /// it says nothing about the volume, so it is never retried from another
    /// copy or reported as damage.
    ///
    /// `kind` says what the bytes are for; see [`ReadKind`].
    fn read_at(&mut self, physical: u64, buf: &mut [u8], kind: ReadKind)
    -> Result<(), DeviceError>;
}

/// A device a volume can be written to, as well as read from.
///
/// Writes may reach the medium in any order and may be cached by it; the only
/// ordering a filesystem may rely on is that everything written before a
/// [`WriteDevice::flush`] is durable when the flush returns.
pub trait WriteDevice: Device {
    /// Write `data` at physical byte offset `physical`. Durable only after a
    /// later [`WriteDevice::flush`] returns.
    fn write_at(&mut self, physical: u64, data: &[u8]) -> Result<(), DeviceError>;

    /// Make every write that has returned durable.
    fn flush(&mut self) -> Result<(), DeviceError>;

    /// Write `data` and make it durable before returning: a write with the
    /// block layer's FUA flag. The default writes and then flushes, which is
    /// correct and costs a whole-cache flush; a device with FUA does better.
    fn write_durable(&mut self, physical: u64, data: &[u8]) -> Result<(), DeviceError> {
        self.write_at(physical, data)?;
        self.flush()
    }
}
