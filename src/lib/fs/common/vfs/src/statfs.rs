//! Packing a `statfs` answer in the layout a program reads it in.
//!
//! [`crate::FileSystem::statfs`] says what a filesystem is in numbers. What a
//! program is handed is one of three structures, and which one is a fact about
//! the call and the word size rather than about the filesystem:
//!
//! * `statfs` and `fstatfs` on a 64-bit architecture fill the generic
//!   `struct statfs`, every field a word: [`StatfsLayout::Wide`].
//! * The same calls on ARMv7-A fill the same structure with 32-bit words:
//!   [`StatfsLayout::Narrow`]. Nothing wider than 32 bits fits, so a count that
//!   does not is `EOVERFLOW`, as Linux's `do_statfs_native` answers.
//! * `statfs64` and `fstatfs64`, which only ARMv7-A has, fill `struct
//!   statfs64`, packed: [`StatfsLayout::Packed64`].
//!
//! # Encoded field by field
//!
//! The same way `struct stat` is encoded in the kernel's `syscall::stat`, so
//! the two cannot disagree about method: each record is built as the
//! `src/lib/proto/linux-abi` structure, so the compiler checks every field's width, and
//! written out one field at a time at its `offset_of!`. That needs no view of
//! a structure as bytes and no idea of padding -- a byte no field names is
//! zero, which is what Linux writes. It lives here rather than in the kernel
//! because it is a pure function of bytes, and so gets host tests that decode
//! each answer at the header's offsets.

use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{ArmStatfs, ArmStatfs64, ST_VALID, Statfs};

use crate::Result;
use crate::node::StatFs;

/// Which structure a `statfs` answer is packed into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatfsLayout {
    /// The generic `struct statfs` on a 64-bit architecture: 120 bytes.
    Wide,
    /// The generic `struct statfs` with 32-bit words, ARMv7-A's: 64 bytes.
    Narrow,
    /// ARMv7-A's packed `struct statfs64`: 84 bytes.
    Packed64,
}

impl StatfsLayout {
    /// The layout plain `statfs` and `fstatfs` answer in, for a build whose
    /// native word is `word_bytes` wide.
    ///
    /// The word size decides it because the header does: `__statfs_word` is
    /// `__kernel_long_t` on a 64-bit build and `__u32` on a 32-bit one, and no
    /// architecture this kernel runs on overrides the structure.
    #[must_use]
    pub const fn native(word_bytes: usize) -> StatfsLayout {
        if word_bytes == 8 {
            StatfsLayout::Wide
        } else {
            StatfsLayout::Narrow
        }
    }

    /// How many bytes a record in this layout is.
    #[must_use]
    pub const fn size(self) -> usize {
        match self {
            StatfsLayout::Wide => size_of::<Statfs>(),
            StatfsLayout::Narrow => size_of::<ArmStatfs>(),
            StatfsLayout::Packed64 => size_of::<ArmStatfs64>(),
        }
    }

    /// `stat` as this layout's bytes.
    ///
    /// `f_frsize` is the block size and `f_flags` is `ST_VALID`, as Linux
    /// fills them for a filesystem that gives neither: nothing here has a
    /// fragment size of its own. The system calls use
    /// [`StatfsLayout::encode_with_flags`], with the mount's flags.
    ///
    /// # Errors
    ///
    /// `EOVERFLOW` for a value the layout's field is too narrow for.
    pub fn encode(self, stat: &StatFs) -> Result<Vec<u8>> {
        self.encode_with_flags(stat, ST_VALID)
    }

    /// As [`StatfsLayout::encode`], with `f_flags` as `flags` says: what a
    /// mount's own flags make it (`MountFlags::statfs_flags`), as Linux's
    /// `flags_by_mnt` does.
    ///
    /// # Errors
    ///
    /// As [`StatfsLayout::encode`].
    pub fn encode_with_flags(self, stat: &StatFs, flags: u64) -> Result<Vec<u8>> {
        match self {
            StatfsLayout::Wide => Ok(wide(stat, flags)),
            StatfsLayout::Narrow => narrow(stat, flags),
            StatfsLayout::Packed64 => packed64(stat, flags),
        }
    }
}

/// Serialise the named fields of a `src/lib/proto/linux-abi` structure.
///
/// Each field is converted with its own type's `to_le_bytes`, so its width is
/// the structure's and never a guess made here. A field left out of the list
/// is zero in the output: the fsid, which no filesystem here has, and the
/// spare words.
macro_rules! encode {
    ($value:expr, $layout:ty, [$($field:ident),+ $(,)?]) => {{
        let value: $layout = $value;
        let mut bytes = vec![0_u8; size_of::<$layout>()];
        $(
            put(&mut bytes, offset_of!($layout, $field), &{ value.$field }.to_le_bytes());
        )+
        bytes
    }};
}

/// Write `field` at `at`.
///
/// Every offset comes from `offset_of!` on the structure the buffer was sized
/// for, so the range is always inside it.
fn put(bytes: &mut [u8], at: usize, field: &[u8]) {
    if let Some(slot) = bytes.get_mut(at..at.saturating_add(field.len())) {
        slot.copy_from_slice(field);
    }
}

/// A count as a signed word, which is how the 64-bit layout carries it.
fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// A value as a 32-bit field, or `EOVERFLOW`.
fn narrowed(value: u64) -> Result<u32> {
    u32::try_from(value).map_err(|_| Errno::EOVERFLOW)
}

/// An inode count as a 32-bit field. Linux lets `-1` through, meaning "no
/// limit", because all ones is all ones at either width.
fn narrowed_count(value: u64) -> Result<u32> {
    if value == u64::MAX {
        return Ok(u32::MAX);
    }
    narrowed(value)
}

/// The 64-bit `struct statfs`.
fn wide(stat: &StatFs, flags: u64) -> Vec<u8> {
    encode!(
        Statfs {
            f_type: signed(stat.magic),
            f_bsize: signed(stat.block_size),
            f_blocks: signed(stat.blocks),
            f_bfree: signed(stat.blocks_free),
            f_bavail: signed(stat.blocks_available),
            f_files: signed(stat.files),
            f_ffree: signed(stat.files_free),
            f_namelen: signed(stat.name_max),
            f_frsize: signed(stat.block_size),
            f_flags: signed(flags),
            ..Statfs::default()
        },
        Statfs,
        [
            f_type, f_bsize, f_blocks, f_bfree, f_bavail, f_files, f_ffree, f_namelen, f_frsize,
            f_flags,
        ]
    )
}

/// ARMv7-A's `struct statfs`.
fn narrow(stat: &StatFs, flags: u64) -> Result<Vec<u8>> {
    Ok(encode!(
        ArmStatfs {
            f_type: narrowed(stat.magic)?,
            f_bsize: narrowed(stat.block_size)?,
            f_blocks: narrowed(stat.blocks)?,
            f_bfree: narrowed(stat.blocks_free)?,
            f_bavail: narrowed(stat.blocks_available)?,
            f_files: narrowed_count(stat.files)?,
            f_ffree: narrowed_count(stat.files_free)?,
            f_namelen: narrowed(stat.name_max)?,
            f_frsize: narrowed(stat.block_size)?,
            f_flags: narrowed(flags)?,
            ..ArmStatfs::default()
        },
        ArmStatfs,
        [
            f_type, f_bsize, f_blocks, f_bfree, f_bavail, f_files, f_ffree, f_namelen, f_frsize,
            f_flags,
        ]
    ))
}

/// ARMv7-A's packed `struct statfs64`. Only the type and the two sizes can
/// overflow, which is the check Linux's `do_statfs64` makes.
fn packed64(stat: &StatFs, flags: u64) -> Result<Vec<u8>> {
    Ok(encode!(
        ArmStatfs64 {
            f_type: narrowed(stat.magic)?,
            f_bsize: narrowed(stat.block_size)?,
            f_blocks: stat.blocks,
            f_bfree: stat.blocks_free,
            f_bavail: stat.blocks_available,
            f_files: stat.files,
            f_ffree: stat.files_free,
            f_namelen: narrowed(stat.name_max)?,
            f_frsize: narrowed(stat.block_size)?,
            f_flags: narrowed(flags)?,
            ..ArmStatfs64::default()
        },
        ArmStatfs64,
        [
            f_type, f_bsize, f_blocks, f_bfree, f_bavail, f_files, f_ffree, f_namelen, f_frsize,
            f_flags,
        ]
    ))
}
