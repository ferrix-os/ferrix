//! The calls that describe a file: the `stat` family, `statx`, `getdents64`
//! and `access`.
//!
//! # Three `struct stat`s
//!
//! x86-64 kept the `struct stat` it grew: 144 bytes, with `st_nlink` a full
//! word ahead of `st_mode`. AArch64 uses the generic one, 128 bytes with the
//! two the other way round. ARMv7-A answers only through `struct stat64`,
//! 104 bytes with the inode number in it twice and 32-bit times, because its
//! plain `struct stat` cannot carry a 64-bit size. A program handed the wrong
//! one reads its file size out of padding, and nothing about what it then does
//! points back here.
//!
//! Which one applies is a fact about the architecture, so it is asked of the
//! facade ([`arch::STAT_LAYOUT`]) rather than decided with a `cfg`. All three
//! encoders are compiled on every architecture: a mistake in the ARMv7-A one
//! is then a build failure on an x86-64 machine rather than a surprise on a
//! board.
//!
//! # Encoded field by field
//!
//! Each record is built as the `src/lib/proto/linux-abi` structure, so the compiler
//! checks every field's width, and then written out one field at a time at
//! its `offset_of!`. That needs no `unsafe` view of a structure as bytes and
//! does not depend on anyone's idea of padding: a byte no field names is
//! zero, which is what Linux writes too.
//!
//! # `fstat` describes the open file, not its name
//!
//! A descriptor's name may since have been unlinked, or renamed over by
//! something else. `fstat` reports the inode that was opened, and the device
//! of the mount it was opened through, which is why a [`Target::Open`] is
//! described from the open file rather than by walking to its name again.

use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    self, AT_EACCESS, AT_EMPTY_PATH, AT_NO_AUTOMOUNT, AT_STATX_SYNC_TYPE, AT_SYMLINK_NOFOLLOW,
    R_OK, STATX_BASIC_STATS, STATX_MNT_ID, STATX_RESERVED, Statx, StatxTimestamp, W_OK, X_OK,
};
use ferrix_vfs::dirent::DirentWriter;
use ferrix_vfs::{Access, Stat, Timespec};

use crate::arch::{self, StatLayout};
use crate::syscall::fd;
use crate::syscall::path::{self, Target};
use crate::syscall::process::Process;
use crate::syscall::uaccess;
use crate::syscall::userns;
use crate::trap::Abi;

/// The encoding half of [`arch::StatLayout`], whose variants are declared in
/// the architecture facade because choosing one is an architecture's business
/// and laying out the bytes is this module's.
///
/// A trait rather than an inherent `impl` since the kernel split: the enum is
/// the core's and this module the load's, and a crate may add inherent
/// methods only to its own types (`docs/KERNEL-CRATES.md`).
pub(crate) trait StatEncoding {
    /// How many bytes a record in this layout is.
    fn size(self) -> usize;

    /// `stat` as this layout's bytes.
    fn encode(self, stat: &Stat) -> Vec<u8>;
}

impl StatEncoding for StatLayout {
    fn size(self) -> usize {
        match self {
            StatLayout::Legacy => size_of::<types::x86_64::Stat>(),
            StatLayout::Generic => size_of::<types::aarch64::Stat>(),
            StatLayout::Stat64 => size_of::<types::arm::Stat64>(),
        }
    }

    fn encode(self, stat: &Stat) -> Vec<u8> {
        match self {
            StatLayout::Legacy => legacy(stat),
            StatLayout::Generic => generic(stat),
            StatLayout::Stat64 => stat64(stat),
        }
    }
}

/// The most `getdents64` packs in one call.
///
/// A program may pass a buffer of any size, and the kernel buffer the entries
/// are packed into first is allocated to match; this bounds it. A short
/// answer is always legal, and every reader loops until it gets zero.
const DIRENT_BUFFER: u64 = 1 << 16;

// ---------------------------------------------------------------------------
// The calls
// ---------------------------------------------------------------------------

/// `newfstatat` and `fstatat64`; and, with the descriptor and flags fixed by
/// the dispatcher, `stat`, `lstat`, `stat64` and `lstat64`. `abi` picks the
/// record: an i386 program's is always its `struct stat64`.
pub(crate) fn sys_fstatat(
    process: &Process,
    dirfd: i32,
    path: u64,
    buf: u64,
    flags: u32,
    abi: Abi,
) -> Result<usize, Errno> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let target = path::target(process, dirfd, path, flags)?;
    write_stat(process, buf, &target.stat()?, abi)
}

/// `stat` with its owner and group as `process`'s user namespace names them,
/// or the overflow id for ones it does not (rule U9).
fn seen_by(process: &Process, stat: &Stat) -> Stat {
    let mut seen = *stat;
    process.with_credentials(|held| {
        seen.metadata.uid =
            userns::from_kid_munged(&held.user_ns, userns::Kind::User, stat.metadata.uid);
        seen.metadata.gid =
            userns::from_kid_munged(&held.user_ns, userns::Kind::Group, stat.metadata.gid);
    });
    seen
}

/// `fstat` and `fstat64`.
pub(crate) fn sys_fstat(process: &Process, fd: i32, buf: u64, abi: Abi) -> Result<usize, Errno> {
    let target = Target::Open(fd::file(process, fd)?);
    write_stat(process, buf, &target.stat()?, abi)
}

/// Put `stat` into the program's buffer in this architecture's layout, or
/// in i386's `struct stat64` for an i386 program, which is not one of the
/// architecture's own and so is chosen here rather than in the facade.
fn write_stat(process: &Process, buf: u64, stat: &Stat, abi: Abi) -> Result<usize, Errno> {
    let stat = &seen_by(process, stat);
    let record = match abi {
        Abi::Compat => i386_stat64(stat),
        Abi::Native => arch::STAT_LAYOUT.encode(stat),
    };
    uaccess::copy_to_user(process.space(), buf, &record).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `statx`.
///
/// Every basic field is filled whatever `mask` asked for, which the call
/// permits and Linux does: the mask is what the caller needs, and what came
/// back is in `stx_mask`. The mount identifier comes too, because it is the
/// only way a program can tell two mounts of one filesystem apart.
pub(crate) fn sys_statx(
    process: &Process,
    dirfd: i32,
    path: u64,
    flags: u32,
    mask: u32,
    buf: u64,
) -> Result<usize, Errno> {
    let known = AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | AT_STATX_SYNC_TYPE;
    if flags & !known != 0
        || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE
        || mask & STATX_RESERVED != 0
    {
        return Err(Errno::EINVAL);
    }
    let target = path::target(process, dirfd, path, flags)?;
    let record = statx_record(
        &seen_by(process, &target.stat()?),
        target.location().mount.id(),
    );
    uaccess::copy_to_user(process.space(), buf, &record).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `getdents64`.
///
/// The directory is read with its cursor locked, and a copy to the program
/// can fault a page in, so the entries are packed into a kernel buffer first
/// and copied out once the read is over. An entry the buffer has no room for
/// is not consumed, so the next call begins with it.
///
/// If not even the first entry fits, the answer is `EINVAL` rather than zero:
/// zero is end of directory, and a program told that with entries still unread
/// would stop.
pub(crate) fn sys_getdents64(
    process: &Process,
    fd: i32,
    dirp: u64,
    count: u64,
) -> Result<usize, Errno> {
    let file = fd::file(process, fd)?;
    // `unsigned int` in the ABI, whatever the register holds above it.
    let count = u64::from(count as u32).min(DIRENT_BUFFER);
    let mut buf = vec![0_u8; usize::try_from(count).map_err(|_| Errno::EINVAL)?];

    let mut refused = false;
    let mut writer = DirentWriter::new(&mut buf);
    file.read_dir(&mut |entry| {
        let fits = writer.push(entry.ino, entry.next, entry.kind.dirent_type(), entry.name);
        refused |= !fits;
        fits
    })?;
    let used = writer.used();
    if used == 0 && refused {
        return Err(Errno::EINVAL);
    }

    let packed = buf.get(..used).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), dirp, packed).map_err(|_| Errno::EFAULT)?;
    Ok(used)
}

/// `faccessat`, `faccessat2` and `access`.
///
/// Checked as the real user and group ids, as Linux's `access_override_creds`
/// arranges, unless `AT_EACCESS` asks for the ones the process acts as; the
/// walk to the file is made as the same identity. Root passes every read and
/// write check, and execute only with an execute bit somewhere in the mode or
/// on a directory, because a file nobody may execute is data and `execve`
/// will refuse it.
pub(crate) fn sys_faccessat(
    process: &Process,
    dirfd: i32,
    path: u64,
    mode: u32,
    flags: u32,
) -> Result<usize, Errno> {
    if mode & !(R_OK | W_OK | X_OK) != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let mut ctx = path::context(process);
    if flags & AT_EACCESS == 0 {
        ctx.who = process.with_credentials(|credentials| Access {
            uid: credentials.user.real,
            gid: credentials.group.real,
            groups: credentials.groups.clone(),
        });
    }
    let who = ctx.who.clone();
    let target = path::target_in(process, ctx, dirfd, path, flags)?;
    who.require(&target.stat()?.metadata, mode)?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// The encoders
// ---------------------------------------------------------------------------

/// Serialise the named fields of a `src/lib/proto/linux-abi` structure.
///
/// Each field is converted with its own type's `to_le_bytes`, so its width is
/// the structure's and never a guess made here. Every field a caller leaves
/// out of the list is zero in the output, which is the point for padding and
/// a mistake for anything else, so the lists below name every non-padding
/// field.
macro_rules! encode {
    ($value:expr, $layout:ty, [$($($field:ident).+),+ $(,)?]) => {{
        let value: $layout = $value;
        let mut bytes = vec![0_u8; size_of::<$layout>()];
        $(
            put(&mut bytes, offset_of!($layout, $($field).+), &value.$($field).+.to_le_bytes());
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

/// A size, which the 64-bit layouts carry signed.
fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Nanoseconds, which the generic layout carries unsigned.
fn nanos(time: Timespec) -> u64 {
    u64::try_from(time.tv_nsec).unwrap_or(0)
}

/// x86-64's `struct stat`.
fn legacy(stat: &Stat) -> Vec<u8> {
    let m = &stat.metadata;
    encode!(
        types::x86_64::Stat {
            st_dev: stat.dev,
            st_ino: m.ino,
            st_nlink: u64::from(m.nlink),
            st_mode: m.mode(),
            st_uid: m.uid,
            st_gid: m.gid,
            st_rdev: m.rdev,
            st_size: signed(m.size),
            st_blksize: i64::from(m.block_size),
            st_blocks: signed(m.blocks),
            st_atime: m.atime.tv_sec,
            st_atime_nsec: m.atime.tv_nsec,
            st_mtime: m.mtime.tv_sec,
            st_mtime_nsec: m.mtime.tv_nsec,
            st_ctime: m.ctime.tv_sec,
            st_ctime_nsec: m.ctime.tv_nsec,
            ..Default::default()
        },
        types::x86_64::Stat,
        [
            st_dev,
            st_ino,
            st_nlink,
            st_mode,
            st_uid,
            st_gid,
            st_rdev,
            st_size,
            st_blksize,
            st_blocks,
            st_atime,
            st_atime_nsec,
            st_mtime,
            st_mtime_nsec,
            st_ctime,
            st_ctime_nsec,
        ]
    )
}

/// The generic `struct stat`.
fn generic(stat: &Stat) -> Vec<u8> {
    let m = &stat.metadata;
    encode!(
        types::aarch64::Stat {
            st_dev: stat.dev,
            st_ino: m.ino,
            st_mode: m.mode(),
            st_nlink: m.nlink,
            st_uid: m.uid,
            st_gid: m.gid,
            st_rdev: m.rdev,
            st_size: signed(m.size),
            st_blksize: i32::try_from(m.block_size).unwrap_or(i32::MAX),
            st_blocks: signed(m.blocks),
            st_atime: m.atime.tv_sec,
            st_atime_nsec: nanos(m.atime),
            st_mtime: m.mtime.tv_sec,
            st_mtime_nsec: nanos(m.mtime),
            st_ctime: m.ctime.tv_sec,
            st_ctime_nsec: nanos(m.ctime),
            ..Default::default()
        },
        types::aarch64::Stat,
        [
            st_dev,
            st_ino,
            st_mode,
            st_nlink,
            st_uid,
            st_gid,
            st_rdev,
            st_size,
            st_blksize,
            st_blocks,
            st_atime,
            st_atime_nsec,
            st_mtime,
            st_mtime_nsec,
            st_ctime,
            st_ctime_nsec,
        ]
    )
}

/// i386's `struct stat64`: ARMv7-A's fields at i386's offsets
/// ([`types::i386::Stat64`]), truncated the same way.
fn i386_stat64(stat: &Stat) -> Vec<u8> {
    let m = &stat.metadata;
    encode!(
        types::i386::Stat64 {
            st_dev: stat.dev,
            __st_ino: m.ino as u32,
            st_mode: m.mode(),
            st_nlink: m.nlink,
            st_uid: m.uid,
            st_gid: m.gid,
            st_rdev: m.rdev,
            st_size: signed(m.size),
            st_blksize: m.block_size,
            st_blocks: m.blocks,
            st_atime: m.atime.tv_sec as u32,
            st_atime_nsec: nanos(m.atime) as u32,
            st_mtime: m.mtime.tv_sec as u32,
            st_mtime_nsec: nanos(m.mtime) as u32,
            st_ctime: m.ctime.tv_sec as u32,
            st_ctime_nsec: nanos(m.ctime) as u32,
            st_ino: m.ino,
            ..Default::default()
        },
        types::i386::Stat64,
        [
            st_dev,
            __st_ino,
            st_mode,
            st_nlink,
            st_uid,
            st_gid,
            st_rdev,
            st_size,
            st_blksize,
            st_blocks,
            st_atime,
            st_atime_nsec,
            st_mtime,
            st_mtime_nsec,
            st_ctime,
            st_ctime_nsec,
            st_ino,
        ]
    )
}

/// ARMv7-A's `struct stat64`.
///
/// Its times and its first inode field are 32 bits, and Linux's
/// `cp_new_stat64` fills them by plain assignment, so they are truncated the
/// same way here rather than refused: a program that wants the whole of either
/// asks `statx`, and one that does not would rather have a wrong date than no
/// `ls`.
fn stat64(stat: &Stat) -> Vec<u8> {
    let m = &stat.metadata;
    encode!(
        types::arm::Stat64 {
            st_dev: stat.dev,
            __st_ino: m.ino as u32,
            st_mode: m.mode(),
            st_nlink: m.nlink,
            st_uid: m.uid,
            st_gid: m.gid,
            st_rdev: m.rdev,
            st_size: signed(m.size),
            st_blksize: m.block_size,
            st_blocks: m.blocks,
            st_atime: m.atime.tv_sec as u32,
            st_atime_nsec: nanos(m.atime) as u32,
            st_mtime: m.mtime.tv_sec as u32,
            st_mtime_nsec: nanos(m.mtime) as u32,
            st_ctime: m.ctime.tv_sec as u32,
            st_ctime_nsec: nanos(m.ctime) as u32,
            st_ino: m.ino,
            ..Default::default()
        },
        types::arm::Stat64,
        [
            st_dev,
            __st_ino,
            st_mode,
            st_nlink,
            st_uid,
            st_gid,
            st_rdev,
            st_size,
            st_blksize,
            st_blocks,
            st_atime,
            st_atime_nsec,
            st_mtime,
            st_mtime_nsec,
            st_ctime,
            st_ctime_nsec,
            st_ino,
        ]
    )
}

/// A device number's major half, from Linux's `new_decode_dev`.
pub(crate) fn major(dev: u64) -> u32 {
    ((dev & 0xfff00) >> 8) as u32
}

/// A device number's minor half, from Linux's `new_decode_dev`: the low byte,
/// and the bits above the major.
pub(crate) fn minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & 0xfff00)) as u32
}

/// A `statx` timestamp.
fn timestamp(time: Timespec) -> StatxTimestamp {
    StatxTimestamp {
        tv_sec: time.tv_sec,
        tv_nsec: nanos(time) as u32,
        __reserved: 0,
    }
}

/// `struct statx`, which is the same on every architecture.
fn statx_record(stat: &Stat, mount: u64) -> Vec<u8> {
    let m = &stat.metadata;
    encode!(
        Statx {
            stx_mask: STATX_BASIC_STATS | STATX_MNT_ID,
            stx_blksize: m.block_size,
            stx_nlink: m.nlink,
            stx_uid: m.uid,
            stx_gid: m.gid,
            stx_mode: u16::try_from(m.mode()).unwrap_or(0),
            stx_ino: m.ino,
            stx_size: m.size,
            stx_blocks: m.blocks,
            stx_atime: timestamp(m.atime),
            stx_ctime: timestamp(m.ctime),
            stx_mtime: timestamp(m.mtime),
            stx_rdev_major: major(m.rdev),
            stx_rdev_minor: minor(m.rdev),
            stx_dev_major: major(stat.dev),
            stx_dev_minor: minor(stat.dev),
            stx_mnt_id: mount,
            ..Default::default()
        },
        Statx,
        [
            stx_mask,
            stx_blksize,
            stx_nlink,
            stx_uid,
            stx_gid,
            stx_mode,
            stx_ino,
            stx_size,
            stx_blocks,
            stx_atime.tv_sec,
            stx_atime.tv_nsec,
            stx_ctime.tv_sec,
            stx_ctime.tv_nsec,
            stx_mtime.tv_sec,
            stx_mtime.tv_nsec,
            stx_rdev_major,
            stx_rdev_minor,
            stx_dev_major,
            stx_dev_minor,
            stx_mnt_id,
        ]
    )
}
