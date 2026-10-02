//! The btrfs write path: copy-on-write trees, allocation, and commit.
//!
//! `ferrix-btrfs` reads a volume and allocates nothing. This crate changes one,
//! and it is the part of the filesystem where a mistake is not a failed read
//! but a volume that no longer mounts, so its design is built around one rule
//! and three consequences of it.
//!
//! # The rule: nothing the last commit can reach is ever overwritten
//!
//! A btrfs volume is a set of trees whose roots the superblock names. Changing
//! a tree never edits a node in place: the node is copied to a newly allocated
//! block, edited there, and every node above it up to the root is copied too,
//! so the edit produces a new root and leaves the old tree whole. Only when
//! every new block is on the disk, and a flush has made it durable, is the
//! superblock rewritten to name the new roots. A crash at any instant before
//! that write leaves the old superblock naming the old, intact trees; after
//! it, the new ones. There is no instant in which the volume is half-changed,
//! which is what makes a journal unnecessary.
//!
//! The consequences are where the work is:
//!
//! 1. **A block freed in a transaction is not reusable until it commits.** The
//!    old copy of a node is still part of the committed tree until the new
//!    superblock is down, so a freed range is *pinned* ([`space`]) and only
//!    returns to the allocator after the commit. A block allocated and freed
//!    within one transaction was never committed and returns at once.
//! 2. **Allocation is itself recorded in trees.** The extent tree holds an
//!    item for every allocated extent, with a back-reference to each owner,
//!    and the free-space tree the complement. Recording an allocation changes
//!    those trees, which allocates blocks for their new nodes, which must be
//!    recorded... Reference changes are therefore queued as *delayed refs*
//!    ([`refs`]) while a transaction runs, and the commit applies them in a
//!    loop that stops when a pass changes nothing: after the first pass every
//!    node those trees touch is already a copy of this transaction's and is
//!    edited in place, so the loop converges in a few passes.
//! 3. **Order is the only durability tool.** The device promises nothing about
//!    the order of writes between two flushes. So a commit writes every new
//!    node (and file data was written before it), flushes, and only then
//!    writes the superblock, with [`WriteDevice::write_durable`] for the
//!    primary copy. Nothing else in the crate depends on write order.
//!
//! # What a writable volume must be
//!
//! One device, `SINGLE` or `DUP` chunks, CRC-32C, skinny metadata, and the
//! free-space tree — which is what `mkfs.btrfs` has made by default for years
//! — and no subvolume but the top-level one, no quota tree, and no unreplayed
//! log. Anything else opens read-only through `ferrix-btrfs` and is refused
//! here with [`Error::Unsupported`] saying why: a snapshot shares tree blocks
//! between roots, and copying a shared block needs reference bookkeeping on
//! its children this crate does not do, so writing one would corrupt the
//! volume rather than fail.
//!
//! # Totality
//!
//! As in `ferrix-btrfs`: no `unsafe`, no indexing, no panics. A node read from
//! the disk is parsed and checked by `ferrix-btrfs` before this crate edits it,
//! and a node this crate writes is built from typed items, never patched.
//!
//! Nor does running out of memory stop the machine: every allocation goes
//! through the `fallible` module and fails as [`Error::OutOfMemory`], which
//! aborts the transaction like any other failure part-way through an edit,
//! and changes nothing before the first one.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use core::fmt;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::volume::Device;

mod bytes;
mod chunks;
mod commit;
mod csum;
pub mod extent;
mod fallible;
pub mod fs;
mod grow;
pub mod log;
pub mod node;
mod open;
pub mod ranges;
pub mod refs;
pub mod space;
mod tree;
mod volume;

pub use fs::NewInode;
pub use volume::{TreeId, WriteVolume};

/// A device a volume can be written to, as well as read from.
///
/// Writes may reach the medium in any order and may be cached by it; the only
/// ordering this crate relies on is that everything written before a
/// [`WriteDevice::flush`] is durable when the flush returns.
pub trait WriteDevice: Device {
    /// Write `data` at physical byte offset `physical`. Durable only after a
    /// later [`WriteDevice::flush`] returns.
    fn write_at(&mut self, physical: u64, data: &[u8]) -> Result<()>;

    /// Make every write that has returned durable.
    fn flush(&mut self) -> Result<()>;

    /// Write `data` and make it durable before returning: a write with the
    /// block layer's FUA flag. The default writes and then flushes, which is
    /// correct and costs a whole-cache flush; a device with FUA does better.
    fn write_durable(&mut self, physical: u64, data: &[u8]) -> Result<()> {
        self.write_at(physical, data)?;
        self.flush()
    }
}

/// Why a volume cannot be opened for writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unsupported {
    /// More than one device.
    MultipleDevices,
    /// A chunk profile other than `SINGLE` or `DUP`.
    Profile,
    /// No free-space tree, or one not marked valid: the free space would have
    /// to be rebuilt from the extent tree, and the v1 cache kept in step.
    NoFreeSpaceTree,
    /// Metadata extent items in the old, non-skinny layout.
    NotSkinny,
    /// Holes recorded as extents, which every write would have to keep up;
    /// `mkfs.btrfs` has made volumes with `NO_HOLES` since 5.15.
    NoHoles,
    /// Data and metadata sharing block groups.
    MixedGroups,
    /// A read-only-compatible feature this writer does not maintain, carrying
    /// its bits: the block-group tree, or one from the future.
    CompatRo(u64),
    /// A subvolume or snapshot besides the top-level tree: its tree blocks may
    /// be shared.
    Subvolumes,
    /// Quotas are enabled, and every change would have to be accounted.
    Quotas,
    /// A log tree waits to be replayed.
    Log,
    /// A tree block has a shared back-reference, or a node carries the
    /// relocation flag: something this writer cannot copy correctly.
    SharedBlock,
}

/// Everything that can go wrong writing a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The volume could not be read, or what was read is damaged.
    Volume(BtrfsError),
    /// The device refused a write at this physical offset.
    DeviceWrite {
        /// Where the failed write began.
        physical: u64,
    },
    /// The device could not flush its cache.
    Flush,
    /// No block group has room for the allocation.
    NoSpace,
    /// The volume uses something this writer does not maintain.
    Unsupported(Unsupported),
    /// The volume contradicts itself in a way only a write discovers: an
    /// extent without its item, a reference count going below zero, a key
    /// that should be unique found twice. Named by what was found.
    Inconsistent(&'static str),
    /// An item with this key already exists.
    Exists,
    /// No item with this key exists.
    NotFound,
    /// An item too large for any leaf of this volume.
    ItemTooLarge,
    /// A name longer than 255 bytes, or a symlink target longer than a leaf
    /// holds.
    NameTooLong,
    /// An empty name, `.` or `..`, or one containing `/` or NUL.
    InvalidName,
    /// A name was to be added to something that is not a directory.
    NotDir,
    /// A directory was named where a file belongs.
    IsDir,
    /// A directory with entries in it was to be removed or replaced.
    NotEmpty,
    /// An inode has as many names as btrfs can record.
    TooManyLinks,
    /// An earlier operation failed half-way through this transaction, so what
    /// it holds in memory can no longer be trusted. Nothing of it reached the
    /// disk; [`WriteVolume::abort`] discards it.
    Aborted,
    /// The volume was reloaded from its last commit after an aborted
    /// transaction, and takes no more changes; see
    /// [`WriteVolume::reload_read_only`].
    ReadOnly,
    /// Memory ran out. Before an operation's first change nothing happened;
    /// after it, the transaction is aborted like any other half-done one.
    OutOfMemory,
}

impl From<BtrfsError> for Error {
    fn from(error: BtrfsError) -> Self {
        match error {
            // A device that ran out of memory says nothing about the volume.
            BtrfsError::OutOfMemory => Error::OutOfMemory,
            error => Error::Volume(error),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Volume(error) => write!(f, "{error}"),
            Error::DeviceWrite { physical } => write!(f, "device write at {physical:#x} failed"),
            Error::Flush => f.write_str("device flush failed"),
            Error::NoSpace => f.write_str("no space left in any block group"),
            Error::Unsupported(what) => write!(f, "volume cannot be written: {what:?}"),
            Error::Inconsistent(what) => write!(f, "volume is inconsistent: {what}"),
            Error::Exists => f.write_str("item already exists"),
            Error::NotFound => f.write_str("no such item"),
            Error::ItemTooLarge => f.write_str("item does not fit in a leaf"),
            Error::NameTooLong => f.write_str("name too long"),
            Error::InvalidName => f.write_str("not a valid name"),
            Error::NotDir => f.write_str("not a directory"),
            Error::IsDir => f.write_str("is a directory"),
            Error::NotEmpty => f.write_str("directory is not empty"),
            Error::TooManyLinks => f.write_str("too many links"),
            Error::Aborted => f.write_str("transaction aborted by an earlier failure"),
            Error::ReadOnly => f.write_str("read-only since a transaction was aborted"),
            Error::OutOfMemory => f.write_str("out of memory"),
        }
    }
}

/// The crate's result.
pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests;
