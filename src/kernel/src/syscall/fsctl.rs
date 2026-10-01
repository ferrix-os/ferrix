//! The calls about a filesystem as a whole rather than one file in it:
//! `statfs`, `sync` and its kin, `truncate`, `fallocate`, `chroot`, `mount`,
//! `umount2`, `pivot_root` and the extended attributes. [`dispatch`] also
//! routes `pipe`, `pipe2`, `sendfile`, `splice` and `copy_file_range` to
//! `crate::syscall::pipe`, so that
//! stage 8's calls hang off `with_process` by one line.
//!
//! # Writing out
//!
//! Every filesystem but btrfs is memory, and has nothing for `sync`,
//! `syncfs`, `fsync` or `fdatasync` to write out: each is done the moment it
//! is asked, which is a true answer rather than a pretence. On btrfs each
//! commits: `fsync` writes the file back and commits the transaction,
//! `syncfs` the filesystem the descriptor is on, and `sync` every mount in
//! the caller's namespace and the first. What they check is what Linux checks -- a descriptor that
//! names nothing is `EBADF`, and an object with no storage to sync, a pipe
//! or a terminal, is `EINVAL`.
//!
//! # Mounting what exists
//!
//! `mount` makes a new filesystem on a directory: a `tmpfs`, a `proc` or a
//! `devtmpfs`, the three an init script mounts first, or a `btrfs` on a disk.
//! Each `proc` mount is a new procfs instance over the one kernel, so every
//! one of them shows the same processes, as on Linux; each `devtmpfs` mount
//! is a new devfs over the one device table. Mounting on a directory that is
//! already a mount's root stacks the new one on top, so `mount -t proc proc
//! /proc` over the boot's `/proc` works, and unmounting it uncovers the old
//! one.
//!
//! `btrfs` is the one type with a source: the path of a block node in `/dev`,
//! whose number names a disk a ring-3 driver registered. The source is
//! resolved last, after the target and the flags, as Linux resolves it inside
//! the filesystem's own mount; a source that is not a block node is
//! `ENOTBLK`, and a number no disk answers to is `ENXIO`. A mount that asks
//! for `MS_RDONLY` gets stage 11's reader; one that does not gets stage
//! 12's writer, which refuses with `EROFS` — the answer a program gets from
//! a read-only medium — when the disk takes no writes or the volume is one
//! it will not maintain, a snapshot or a quota-enabled volume among them.
//!
//! `sysfs` and `cgroup2` are views of the device tree and the job tree, a new
//! instance of each over the one kernel. The types that do not exist yet --
//! `devpts` and every other -- are `ENODEV`, Linux's answer for a type the
//! kernel was built without; they are added to [`filesystem_named`] as they
//! arrive.
//!
//! # Per-mount flags, enforced
//!
//! `MS_RDONLY`, `MS_NOSUID`, `MS_NODEV` and `MS_NOEXEC` are the mount's own
//! (`ferrix_vfs::MountFlags`), for every type, and each is enforced where the
//! operation it governs is decided: a change through a read-only mount is
//! `EROFS` (the VFS), a device on a `nodev` one `EACCES` (the VFS's `open`), a
//! program on a `noexec` one `EACCES` and its executable mapping `EPERM`
//! (`fs::open_program`, `memory`), and a set-id bit on a `nosuid` one is
//! ignored (`fs::open_program`). `MS_NOATIME`, `MS_NODIRATIME` and
//! `MS_RELATIME` are recorded and shown and change nothing, since no access
//! time is kept apart from the others; a new mount gets only the ones asked
//! for, not Linux's default `relatime`, because none of them is kept.
//! `/proc/mounts`, `/proc/<pid>/mountinfo` and `statfs`'s `f_flags` show them.
//!
//! `MS_REMOUNT | MS_BIND` changes them on the mount whose root the target
//! is and on no other, as Linux's `do_reconfigure_mnt`; a remount that names
//! no access-time flag keeps the old one. A plain `MS_REMOUNT` does the same
//! and also makes the filesystem itself read-only or writable -- Linux's
//! `SB_RDONLY` -- so that every bind of it refuses writes, or takes them
//! again. Before a mount or its filesystem goes read-only the filesystem is
//! written out, as `umount2` writes it, so a btrfs `/` or `/data` remounted
//! read-only at shutdown is committed and then takes no more writes (finding
//! F-53:
//! `docs/INIT.md` §8.2 said init did this, and until then the kernel refused
//! the remount). Linux refuses a read-only remount while a file is open for
//! writing (`EBUSY`); nothing here counts writers, so it is accepted, and a
//! file already open for writing goes on writing, as it would through a
//! read-only bind.
//!
//! The options string -- tmpfs's `size=`, procfs's `hidepid=` -- is not read
//! for any type, so an option is never refused and never has an effect.
//!
//! # Binds, propagation, and what stays refused
//!
//! `MS_BIND` mounts the place the source names -- a directory, a
//! subdirectory, a file or a socket -- on the target, with `MS_REC` the
//! mounts below it too (`ferrix_vfs::Namespace::bind`); the new mount has
//! the source mount's flags, whatever else `mount` was passed, as on Linux,
//! where a bind read-only takes a remount after it. `MS_PRIVATE`,
//! `MS_SLAVE` and `MS_UNBINDABLE`, with or without `MS_REC`, are accepted on
//! a mount's root and change nothing: no mount here is ever shared, so every
//! mount already is what they ask. `MS_SHARED` and `MS_MOVE` are `EINVAL`
//! (`docs/NAMESPACES.md` §1.5). `umount2` with `MNT_DETACH` takes the mounts
//! inside the target with it; without, they make it `EBUSY`.
//!
//! # Namespaces
//!
//! Each change acts on the caller's mount namespace, and a place on a mount
//! of another is `EINVAL`, as Linux's `check_mnt` has it. `pivot_root` swaps
//! the caller's root mount for another and mounts the old one where it is
//! asked, then moves every process of the namespace that was at the old root
//! to the new one. `umount2` acts on the mount on top of the place it names,
//! so that `umount2(".", MNT_DETACH)` after `pivot_root(".", ".")` takes
//! the old root stacked there (`docs/NAMESPACES.md` §2.1).
//!
//! # No extended attributes
//!
//! tmpfs keeps none. So every file answers that it has none: `getxattr` is
//! `ENODATA`, a list is empty, and setting or removing one is `EOPNOTSUPP`. The
//! path or the descriptor is resolved first, so a missing file is still
//! `ENOENT` and a closed descriptor still `EBADF`.

use alloc::sync::Arc;
use core::mem::size_of;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    ARM_STATFS64_UNPACKED_SIZE, AT_FDCWD, AT_SYMLINK_NOFOLLOW, FALLOC_FL_KEEP_SIZE, MNT_DETACH,
    MNT_EXPIRE, MNT_FORCE, MS_BIND, MS_MGC_MSK, MS_MGC_VAL, MS_MOVE, MS_NOATIME, MS_NODEV,
    MS_NODIRATIME, MS_NOEXEC, MS_NOSUID, MS_PRIVATE, MS_RDONLY, MS_REC, MS_RELATIME, MS_REMOUNT,
    MS_SHARED, MS_SILENT, MS_SLAVE, MS_STRICTATIME, MS_UNBINDABLE, UMOUNT_NOFOLLOW,
};
use ferrix_vfs::access::{MAY_EXEC, MAY_WRITE};
use ferrix_vfs::statfs::StatfsLayout;
use ferrix_vfs::{FileSystem, FileType, Inode, Location, MountFlags, Namespace, SetAttributes};

use crate::fs;
use crate::fs::devfs::Devfs;
use crate::fs::procfs::Procfs;
use crate::syscall::namespace;
use crate::syscall::path::{self, Target};
use crate::syscall::process::Process;
use crate::syscall::{fd, pipe, registry, uaccess};
use crate::trap::Abi;

/// The propagation changes, one of which a `mount` call names alone.
const PROPAGATION_FLAGS: u32 = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;

/// The access-time flags, which a remount naming none of them keeps.
const ATIME_FLAGS: u32 = MS_NOATIME | MS_NODIRATIME | MS_RELATIME | MS_STRICTATIME;

/// A mount's own flags from `mount`'s, as Linux's `path_mount` separates
/// them: `MS_STRICTATIME` is the absence of the other two access-time ones.
fn mount_flags(flags: u32) -> MountFlags {
    [
        (MS_RDONLY, MountFlags::READ_ONLY),
        (MS_NOSUID, MountFlags::NOSUID),
        (MS_NODEV, MountFlags::NODEV),
        (MS_NOEXEC, MountFlags::NOEXEC),
        (MS_NOATIME, MountFlags::NOATIME),
        (MS_NODIRATIME, MountFlags::NODIRATIME),
        (MS_RELATIME, MountFlags::RELATIME),
    ]
    .into_iter()
    .filter(|&(bit, _)| flags & bit != 0)
    .fold(MountFlags::NONE, |set, (_, flag)| set.union(flag))
    .without(if flags & MS_STRICTATIME != 0 {
        MountFlags::RELATIME.union(MountFlags::NOATIME)
    } else {
        MountFlags::NONE
    })
}

/// The calls this module answers, or `None` for one it does not.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let fd = fd::arg(a[0]);
    let answer = match call {
        // i386's `compat_statfs64` is the same packed record, but Linux takes
        // only its own size from a 32-bit x86 program: the unpacked 88 bytes
        // are an ARM entry's allowance for musl's ARM `struct statfs`.
        Syscall::Statfs64 | Syscall::Fstatfs64
            if abi == Abi::Compat && a[1] != StatfsLayout::Packed64.size() as u64 =>
        {
            Err(Errno::EINVAL)
        }
        Syscall::Pipe => pipe::sys_pipe2(process, a[0], 0),
        Syscall::Pipe2 => pipe::sys_pipe2(process, a[0], super::linux::truncate(a[1])),
        Syscall::Sendfile | Syscall::Sendfile64 => {
            pipe::sys_sendfile(process, fd, fd::arg(a[1]), a[2], a[3])
        }
        Syscall::Splice => {
            let flags = super::linux::truncate(a[5]);
            pipe::sys_splice(process, fd, a[1], fd::arg(a[2]), a[3], a[4], flags)
        }
        Syscall::CopyFileRange => {
            let flags = super::linux::truncate(a[5]);
            pipe::sys_copy_file_range(process, fd, a[1], fd::arg(a[2]), a[3], a[4], flags)
        }
        Syscall::Statfs => sys_statfs(process, a[0], a[1]),
        Syscall::Fstatfs => sys_fstatfs(process, fd, a[1]),
        Syscall::Statfs64 => sys_statfs64(process, a[0], a[1], a[2]),
        Syscall::Fstatfs64 => sys_fstatfs64(process, fd, a[1], a[2]),
        Syscall::Sync => sys_sync(process),
        Syscall::Syncfs => sys_syncfs(process, fd),
        Syscall::Fsync => sys_fsync(process, fd, false),
        Syscall::Fdatasync => sys_fsync(process, fd, true),
        Syscall::Readahead => sys_readahead(process, fd, readahead_count(a)),
        Syscall::Truncate => sys_truncate(process, a[0], super::linux::native_signed(a[1])),
        Syscall::Truncate64 => sys_truncate(process, a[0], super::linux::wide(a, 1)),
        // `fallocate(fd, mode, offset, len)`: on ARMv7-A the offset is in the
        // register pair from 2 and the length in the pair from 4, which
        // `wide` reaches from slot 3. QEMU's linux-user reads the same six
        // registers, and busybox loads r4 and r5 before the call.
        Syscall::Fallocate => sys_fallocate(
            process,
            fd,
            super::linux::truncate(a[1]),
            super::linux::wide(a, 2),
            super::linux::wide(a, 3),
        ),
        Syscall::Chroot => sys_chroot(process, a[0]),
        Syscall::Mount => sys_mount(process, a[0], a[1], a[2], super::linux::truncate(a[3])),
        Syscall::Umount2 => sys_umount2(process, a[0], super::linux::truncate(a[1])),
        Syscall::PivotRoot => sys_pivot_root(process, a[0], a[1]),
        Syscall::Getxattr | Syscall::Listxattr | Syscall::Setxattr | Syscall::Removexattr => {
            xattr_at(process, a[0], 0, call)
        }
        Syscall::Lgetxattr | Syscall::Llistxattr | Syscall::Lsetxattr | Syscall::Lremovexattr => {
            xattr_at(process, a[0], AT_SYMLINK_NOFOLLOW, call)
        }
        Syscall::Fgetxattr | Syscall::Flistxattr | Syscall::Fsetxattr | Syscall::Fremovexattr => {
            xattr_of(process, fd, call)
        }
        _ => return None,
    };
    Some(answer)
}

// ---------------------------------------------------------------------------
// statfs
// ---------------------------------------------------------------------------

/// The layout plain `statfs` and `fstatfs` fill, which the word size decides.
fn native_layout() -> StatfsLayout {
    StatfsLayout::native(size_of::<usize>())
}

/// Put what `target`'s filesystem says into the program's buffer.
fn write_statfs(
    process: &Process,
    buf: u64,
    target: &Target,
    layout: StatfsLayout,
) -> Result<usize, Errno> {
    let at = target.location();
    let flags = at.mount.flags();
    let flags = if at.mount.filesystem_read_only() {
        flags.union(MountFlags::READ_ONLY)
    } else {
        flags
    };
    let record = layout.encode_with_flags(&fs::namespace().statfs(at), flags.statfs_flags())?;
    uaccess::copy_to_user(process.space(), buf, &record).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `statfs64`'s size argument: the kernel's packed structure, or musl's
/// unpacked one, which Linux's ARM entry code takes as the same thing. Checked
/// before the path is looked at, as on Linux; an i386 program's is narrowed
/// to the packed size alone in [`dispatch`].
fn statfs64_size(size: u64) -> Result<(), Errno> {
    let size = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    if size == StatfsLayout::Packed64.size() || size == ARM_STATFS64_UNPACKED_SIZE {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

/// `statfs`: the filesystem a path is on, following a final link.
pub(crate) fn sys_statfs(process: &Process, at: u64, buf: u64) -> Result<usize, Errno> {
    let target = path::target(process, AT_FDCWD, at, 0)?;
    write_statfs(process, buf, &target, native_layout())
}

/// `fstatfs`: the filesystem an open file is on. An `O_PATH` descriptor is
/// enough, as on Linux.
pub(crate) fn sys_fstatfs(process: &Process, fd: i32, buf: u64) -> Result<usize, Errno> {
    let target = Target::Open(fd::file(process, fd)?);
    write_statfs(process, buf, &target, native_layout())
}

/// `statfs64`, ARMv7-A's and i386's, into the packed `struct statfs64`.
pub(crate) fn sys_statfs64(
    process: &Process,
    at: u64,
    size: u64,
    buf: u64,
) -> Result<usize, Errno> {
    statfs64_size(size)?;
    let target = path::target(process, AT_FDCWD, at, 0)?;
    write_statfs(process, buf, &target, StatfsLayout::Packed64)
}

/// `fstatfs64`, ARMv7-A's and i386's.
pub(crate) fn sys_fstatfs64(
    process: &Process,
    fd: i32,
    size: u64,
    buf: u64,
) -> Result<usize, Errno> {
    statfs64_size(size)?;
    let target = Target::Open(fd::file(process, fd)?);
    write_statfs(process, buf, &target, StatfsLayout::Packed64)
}

// ---------------------------------------------------------------------------
// Syncing, and lengths
// ---------------------------------------------------------------------------

/// An open file a call that acts through a descriptor may use: `O_PATH` names
/// a file without opening it, and is `EBADF` to these as to `read`.
fn usable(process: &Process, fd: i32) -> Result<Arc<ferrix_vfs::OpenFile>, Errno> {
    let file = fd::file(process, fd)?;
    if file.is_path() {
        return Err(Errno::EBADF);
    }
    Ok(file)
}

/// `syncfs`: write out the filesystem the descriptor's file is on. On a
/// filesystem that keeps nothing back — everything in memory — that is
/// nothing; on btrfs it is a transaction commit.
fn sys_syncfs(process: &Process, fd: i32) -> Result<usize, Errno> {
    let file = fd::file(process, fd)?;
    file.location().mount.filesystem().sync()?;
    Ok(0)
}

/// `sync`: write out every filesystem in the caller's namespace and the
/// first, and answer nothing, as Linux does — `sync(2)` has no error to give.
/// Linux writes out every filesystem of the kernel; the two namespaces hold
/// every one a process can have written to but one mounted only in a third,
/// which that namespace's own `sync` or unmount writes.
fn sys_sync(process: &Process) -> Result<usize, Errno> {
    let own = path::mount_namespace(process);
    let first = fs::first_namespace();
    let mut mounts = own.mounts();
    if !Arc::ptr_eq(&own, first) {
        mounts.extend(first.mounts());
    }
    for (index, mount) in mounts.iter().enumerate() {
        if mounts
            .iter()
            .take(index)
            .all(|earlier| !earlier.shares_filesystem(mount))
        {
            let _ = mount.filesystem().sync();
        }
    }
    Ok(0)
}

/// `fsync` and `fdatasync`: write this file out and wait for it. `EINVAL` for
/// an object with no storage to sync -- a pipe, a terminal -- as Linux
/// answers for a file with no `fsync` operation.
pub(crate) fn sys_fsync(process: &Process, fd: i32, data_only: bool) -> Result<usize, Errno> {
    let file = usable(process, fd)?;
    match file.kind() {
        FileType::Regular | FileType::Directory => {
            file.location().inode()?.fsync(data_only)?;
            Ok(0)
        }
        // A disk's descriptor flushes the disk (`fs::disk_file`).
        FileType::BlockDevice if fs::disk_file::of(file.io()).is_some() => {
            file.io().fsync(data_only)?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `readahead`: done as soon as it is asked, because there is no page cache
/// to fill -- every file is in memory already.
///
/// What is still checked is what Linux's `ksys_readahead` checks, in its
/// order: a descriptor not open for reading, `O_PATH` included, is `EBADF`;
/// anything but a regular file is `EINVAL`; and a count too large to be a
/// `loff_t` is `EINVAL`, from `generic_fadvise`. The offset is never looked
/// at, as Linux does not look at it either before the page cache does.
pub(crate) fn sys_readahead(process: &Process, fd: i32, count: u64) -> Result<usize, Errno> {
    let file = fd::file(process, fd)?;
    if !file.readable() {
        return Err(Errno::EBADF);
    }
    if file.kind() != FileType::Regular || i64::try_from(count).is_err() {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// `readahead`'s count register.
///
/// `readahead(fd, offset, count)` puts a 64-bit `loff_t` second. One register
/// on a 64-bit architecture, so the count is the third. On ARMv7-A the EABI
/// starts the offset at the next even register, r2 and r3, which leaves r1
/// empty and the 32-bit count in r4 -- what `regpairs_aligned` makes QEMU's
/// linux-user read, and what `super::linux::wide` assumes for the offset.
fn readahead_count(a: &[u64; 6]) -> u64 {
    if size_of::<usize>() == 8 {
        a[2]
    } else {
        a[4] & 0xFFFF_FFFF
    }
}

/// `truncate` and `truncate64`: set a file's length by path, following a
/// final link. A negative length is refused before the path is read.
pub(crate) fn sys_truncate(process: &Process, at: u64, length: i64) -> Result<usize, Errno> {
    let length = u64::try_from(length).map_err(|_| Errno::EINVAL)?;
    let target = path::target(process, AT_FDCWD, at, 0)?;
    let metadata = target.stat()?.metadata;
    if metadata.kind == FileType::Regular {
        // `vfs_truncate`: a read-only mount is `EROFS` before permission.
        target.location().require_writable()?;
        path::context(process).who.require(&metadata, MAY_WRITE)?;
    }
    fs::namespace().truncate(target.location(), length)?;
    fs::inotify::node_event(target.location(), fs::inotify::IN_MODIFY);
    Ok(0)
}

/// `fallocate`, in the order Linux's `vfs_fallocate` checks.
///
/// Mode 0 grows the file to cover the range if it does not already, and never
/// shrinks it. `FALLOC_FL_KEEP_SIZE` is accepted and does nothing: what it
/// asks is that later writes into the range cannot fail for space, and pages
/// here are committed when first written, so there is nothing to reserve --
/// the same is true of the range mode 0 grows over, which reads as zeros and
/// costs nothing until written. Every other mode punches, zeroes, collapses or
/// inserts ranges, which tmpfs cannot, and is `EOPNOTSUPP`.
pub(crate) fn sys_fallocate(
    process: &Process,
    fd: i32,
    mode: u32,
    offset: i64,
    len: i64,
) -> Result<usize, Errno> {
    let file = usable(process, fd)?;
    if offset < 0 || len <= 0 {
        return Err(Errno::EINVAL);
    }
    if mode & !FALLOC_FL_KEEP_SIZE != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if !file.writable() {
        return Err(Errno::EBADF);
    }
    match file.kind() {
        FileType::Regular => {}
        FileType::Fifo => return Err(Errno::ESPIPE),
        FileType::Directory => return Err(Errno::EISDIR),
        _ => return Err(Errno::ENODEV),
    }
    let end = offset
        .checked_add(len)
        .and_then(|end| u64::try_from(end).ok())
        .ok_or(Errno::EFBIG)?;
    if mode & FALLOC_FL_KEEP_SIZE == 0 {
        file.grow_to(end)?;
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// The tree: chroot, mount, umount2
// ---------------------------------------------------------------------------

/// `chroot`: make a directory the process's `/`.
///
/// The new root must be a directory the caller may search, as Linux's
/// `path_permission` requires, and the caller root, for `CAP_SYS_CHROOT`. The
/// working directory stays where it was, as on Linux.
pub(crate) fn sys_chroot(process: &Process, at: u64) -> Result<usize, Errno> {
    let place = path::target(process, AT_FDCWD, at, 0)?.location().clone();
    let metadata = fs::namespace().stat(&place)?.metadata;
    if metadata.kind != FileType::Directory {
        return Err(Errno::ENOTDIR);
    }
    path::context(process).who.require(&metadata, MAY_EXEC)?;
    // `CAP_SYS_CHROOT` in the caller's namespace (in the first, root).
    if !process.with_credentials(|held| held.holds(crate::syscall::userns::CAP_SYS_CHROOT)) {
        return Err(Errno::EPERM);
    }
    process.fs_context().lock().root = place;
    Ok(0)
}

/// The filesystem a `mount` type names, new: on nothing for the memory
/// filesystems, on the disk `source` names for btrfs.
///
/// The names are the ones Linux registers, and the ones `/proc/filesystems`
/// lists. `devpts` joins this match when it exists; until then it falls to
/// `ENODEV` with every name Linux would not know either.
/// `cgroup2` mounts read-only as well as writable, as on Linux, where a
/// read-only mount is how a container is shown the tree it may not change.
fn filesystem_named(
    process: &Process,
    name: &[u8],
    source: u64,
    read_only: bool,
) -> Result<Arc<dyn FileSystem>, Errno> {
    // A read-only memory filesystem is a writable one on a read-only mount:
    // the flag is the mount's, and the VFS refuses the writes.
    match name {
        b"tmpfs" => Ok(fs::new_tmpfs()?),
        // Of the pid namespace the mounter is in (`docs/PIDNS.md` §6).
        b"proc" => Ok(Arc::new(Procfs::new_in(
            process
                .numbers()
                .map(|numbers| Arc::clone(numbers.namespace())),
        ))),
        b"devtmpfs" => Ok(Arc::new(Devfs::new())),
        b"cgroup2" => Ok(Arc::new(fs::cgroupfs::Cgroupfs::rooted_at(Arc::clone(
            process.nsproxy().cgroup.root(),
        )))),
        // Read-only as well as writable, as on Linux, where a container is
        // shown `/sys` read-only; nothing in it takes a write but `bind` and
        // `unbind`, which a read-only mount refuses.
        b"sysfs" => Ok(Arc::new(fs::sysfs::Sysfs::new())),
        b"btrfs" => {
            if source == 0 {
                return Err(Errno::EINVAL);
            }
            let node = path::target(process, AT_FDCWD, source, 0)?;
            let meta = node.location().inode()?.metadata();
            if meta.kind != FileType::BlockDevice {
                return Err(Errno::ENOTBLK);
            }
            if read_only {
                fs::btrfs::mount(meta.rdev)
            } else {
                fs::btrfs::mount_rw(meta.rdev)
            }
        }
        _ => Err(Errno::ENODEV),
    }
}

/// `mount(source, target, type, flags, data)`: a new filesystem on a
/// directory, or with `MS_REMOUNT` new flags for the mount whose root the
/// target is. The options string means nothing to any filesystem here and is
/// not read, and the source only to btrfs; see the module documentation.
///
/// In Linux's order (`path_mount`): the type is copied in before the target
/// is looked up; then privilege is asked, the flags the table cannot act on
/// are refused, a remount is done, and for a new mount the type is
/// looked for last, with the source resolved inside it.
pub(crate) fn sys_mount(
    process: &Process,
    source: u64,
    target: u64,
    kind: u64,
    flags: u32,
) -> Result<usize, Errno> {
    let kind = if kind == 0 {
        None
    } else {
        Some(fd::user_path(process, kind)?)
    };
    let place = path::target(process, AT_FDCWD, target, 0)?
        .location()
        .clone();
    // The magic number programs from before Linux 2.4 put in the top half,
    // which Linux still strips.
    let flags = if flags & MS_MGC_MSK == MS_MGC_VAL {
        flags & !MS_MGC_MSK
    } else {
        flags
    };
    // `path_mount`'s order: a remount (with `MS_BIND`, of the one mount),
    // then a bind, then a propagation change, then a move, then a new
    // mount. Each ignores the flags that name the ones after it.
    // `may_mount`: `CAP_SYS_ADMIN` over the namespace's owner, asked before
    // any flag is judged (M1).
    namespace::may_mount(process)?;
    // From a user namespace that is not the first, a new mount is `tmpfs`
    // alone and always `nosuid,nodev`: no filesystem parser sees an image an
    // unprivileged user chose (M2).
    let confined = !process.with_credentials(|held| held.user_ns.is_first());
    // The caller's own namespace: a mount of another is `EINVAL` to each
    // of its changes, Linux's `check_mnt`.
    let mounts = path::mount_namespace(process);
    let remount = flags & MS_REMOUNT != 0;
    let bind = !remount && flags & MS_BIND != 0;
    let propagation = !remount && !bind && flags & PROPAGATION_FLAGS != 0;
    if !remount && !bind && !propagation && flags & MS_MOVE != 0 {
        return Err(Errno::EINVAL);
    }
    // `flags_to_propagation_type`: exactly one type, and never shared.
    if propagation {
        let kind = flags & !(MS_REC | MS_SILENT);
        if !kind.is_power_of_two() || kind == MS_SHARED {
            return Err(Errno::EINVAL);
        }
    }
    if remount {
        // A plain remount changes the filesystem for every mount of it:
        // only for a caller privileged over the filesystem's owner (N5).
        if flags & MS_BIND == 0 {
            namespace::may_remount_filesystem(process, &place.mount)?;
        }
        return remount_at(&mounts, &place, flags);
    }
    if bind {
        // `do_loopback`: no source, or an empty one, is `EINVAL`.
        if source == 0 || fd::user_path(process, source)?.is_empty() {
            return Err(Errno::EINVAL);
        }
        let from = path::target(process, AT_FDCWD, source, 0)?
            .location()
            .clone();
        let _ = mounts.bind(&from, &place, flags & MS_REC != 0)?;
        return Ok(0);
    }
    if propagation {
        // Nothing is ever shared, so nothing changes; Linux's
        // `do_change_type` still wants a mount's root in this tree.
        if !place.is_mount_root() || !mounts.owns(&place.mount) {
            return Err(Errno::EINVAL);
        }
        return Ok(0);
    }
    let read_only = flags & MS_RDONLY != 0;
    let kind = kind.ok_or(Errno::EINVAL)?;
    let mut wanted = mount_flags(flags);
    if confined {
        if kind != b"tmpfs" {
            return Err(Errno::EPERM);
        }
        wanted = wanted.union(MountFlags::NOSUID).union(MountFlags::NODEV);
    }
    let filesystem = filesystem_named(process, &kind, source, read_only)?;
    if kind == b"tmpfs" {
        // Linux's tmpfs root is `1777` and the mounter's: a user's own tmpfs
        // is one it can make a directory in, which bubblewrap does for its
        // new root. `mode=`, `uid=` and `gid=` are not read.
        let (uid, gid) =
            process.with_credentials(|held| (held.user.filesystem, held.group.filesystem));
        filesystem.root().set_attributes(&SetAttributes {
            permissions: Some(0o1777),
            uid: Some(uid),
            gid: Some(gid),
            ..SetAttributes::default()
        })?;
    }
    // Owned by the caller's user namespace when it is not the first: the
    // filesystem's owner is who may remount it as a whole.
    let owner = confined.then(|| {
        process.with_credentials(|held| {
            Arc::clone(&held.user_ns) as Arc<dyn core::any::Any + Send + Sync>
        })
    });
    let _ = mounts.mount_owned(filesystem, &place, wanted, owner)?;
    Ok(0)
}

/// `MS_REMOUNT`, with or without `MS_BIND`: the mount whose root `place` is
/// takes the flags `flags` names, keeping its access-time flags if `flags`
/// names none of them (Linux's `path_mount`). Without `MS_BIND` the
/// filesystem itself turns read-only or writable too, for every bind of it.
///
/// A mount going read-only has its filesystem written out first, as
/// `umount2` writes it: a write-out that fails leaves the mount as it was
/// and answers the error, so nothing is lost silently. `EINVAL` for a place
/// that is not a mount's root, before anything is written.
fn remount_at(mounts: &Namespace, place: &Location, flags: u32) -> Result<usize, Errno> {
    if !place.is_mount_root() || !mounts.owns(&place.mount) {
        return Err(Errno::EINVAL);
    }
    let mut wanted = mount_flags(flags);
    if flags & ATIME_FLAGS == 0 {
        let old = place.mount.flags();
        let kept_atime = old.without(old.without(MountFlags::ATIME));
        wanted = wanted.union(kept_atime);
    }
    let going_read_only = wanted.contains(MountFlags::READ_ONLY)
        && (!place.mount.read_only() || !place.mount.filesystem_read_only());
    if going_read_only {
        place.mount.filesystem().sync()?;
    }
    if flags & MS_BIND != 0 {
        mounts.remount(place, wanted)?;
    } else {
        mounts.remount_filesystem(place, wanted)?;
    }
    Ok(0)
}

/// `umount2`: unmount the mount whose root `target` names.
///
/// Every unmount here is lazy, as `MNT_DETACH` is: the mount leaves the tree
/// at once and goes when the last open file on it closes, so a busy mount is
/// not `EBUSY`. What `MNT_DETACH` adds is the mounts inside it: they leave
/// with it, where without it they make it `EBUSY`. `MNT_EXPIRE` marks a mount for a later call to remove if
/// nobody used it in between, and there is no use count to decide that by,
/// so it is `EINVAL`.
///
/// The filesystem is written out first, as Linux's unmount does before it
/// lets a superblock go. It was not: a btrfs volume unmounted without a
/// `sync` came back without what was written since its last commit -- files,
/// directories, renames, a 40 MiB file cut to 32 MiB (ferrix-ea's black-box
/// pass, 2026-09-26). A write-out that fails keeps the mount, with the
/// error, so nothing is lost silently; `MNT_FORCE` unmounts it anyway.
pub(crate) fn sys_umount2(process: &Process, target: u64, flags: u32) -> Result<usize, Errno> {
    if flags & !(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW) != 0
        || flags & MNT_EXPIRE != 0
    {
        return Err(Errno::EINVAL);
    }
    // `may_mount`, before the target is looked up, as `ksys_umount` does.
    namespace::may_mount(process)?;
    let follow = if flags & UMOUNT_NOFOLLOW != 0 {
        AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    let mounts = path::mount_namespace(process);
    // The mount on top of the place named: what `.` names after
    // `pivot_root(".", ".")` stacked the old root on the new one, which a
    // walk of `.` does not cross (Linux's `LOOKUP_MOUNTPOINT`).
    let named = path::target(process, AT_FDCWD, target, follow)?
        .location()
        .clone();
    let place = mounts.descend_mounts(named);
    let detach = flags & MNT_DETACH != 0;
    // With `MNT_DETACH` every filesystem mounted inside goes too, so each is
    // written out, once however many binds of it there are; a mount made
    // inside after this and before the unmount is not, as a write after
    // the one sync is not.
    let going = if detach {
        mounts.subtree(&place)?
    } else {
        alloc::vec![Arc::clone(&place.mount)]
    };
    for (index, mount) in going.iter().enumerate() {
        let first_of_its_filesystem = going
            .iter()
            .take(index)
            .all(|earlier| !earlier.shares_filesystem(mount));
        if first_of_its_filesystem
            && let Err(error) = mount.filesystem().sync()
            && flags & MNT_FORCE == 0
        {
            return Err(error);
        }
    }
    mounts.unmount_with(&place, detach)?;
    Ok(0)
}

/// `pivot_root(new_root, put_old)`: the mount whose root `new_root` is
/// becomes the caller's `/`, the old root is mounted on `put_old`, and every
/// process of the namespace whose root or working directory was the old root
/// is moved to the new one (Linux's `chroot_fs_refs`), all in the caller's
/// namespace (`docs/NAMESPACES.md` §2.1). The checks are Linux's, in its
/// order; see [`Namespace::pivot_root`].
///
/// # Errors
///
/// `EPERM` without privilege, before either path is looked up, as
/// `may_mount` is asked first; then the walks' own, and the pivot's.
fn sys_pivot_root(process: &Process, new_root: u64, put_old: u64) -> Result<usize, Errno> {
    namespace::may_mount(process)?;
    let new_root = path::target(process, AT_FDCWD, new_root, 0)?
        .location()
        .clone();
    let put_old = path::target(process, AT_FDCWD, put_old, 0)?
        .location()
        .clone();
    let ctx = path::context(process);
    let mounts = fs::namespace_of(&ctx);
    mounts.pivot_root(&ctx.root, &new_root, &put_old)?;
    chroot_fs_refs(&mounts, &ctx.root, &new_root)?;
    Ok(0)
}

/// Move every process of `mounts` whose root or working directory is `old` to
/// `new`: `pivot_root`'s last step. Each fs context is locked alone, after
/// the namespace's locks are released (`docs/NAMESPACES.md` §6), and what it
/// held is dropped after its lock. A context shared through `CLONE_FS` is met
/// once per process and changed the first time.
///
/// # Errors
///
/// `ENOMEM` when there is no memory to list the processes: the pivot has
/// happened, as Linux's has when its own walk of the tasks cannot be undone.
fn chroot_fs_refs(mounts: &Namespace, old: &Location, new: &Location) -> Result<(), Errno> {
    for other in registry::live()? {
        let displaced = {
            let mut context = other.fs_context().lock();
            if !fs::is_in(&context, mounts) {
                continue;
            }
            let root = context
                .root
                .same(old)
                .then(|| core::mem::replace(&mut context.root, new.clone()));
            let cwd = context
                .cwd
                .same(old)
                .then(|| core::mem::replace(&mut context.cwd, new.clone()));
            (root, cwd)
        };
        drop(displaced);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Extended attributes
// ---------------------------------------------------------------------------

/// What a file with no extended attributes answers each of the calls.
fn no_attributes(call: Syscall) -> Result<usize, Errno> {
    match call {
        // An empty list, which is a length of zero.
        Syscall::Listxattr | Syscall::Llistxattr | Syscall::Flistxattr => Ok(0),
        Syscall::Getxattr | Syscall::Lgetxattr | Syscall::Fgetxattr => Err(Errno::ENODATA),
        _ => Err(Errno::EOPNOTSUPP),
    }
}

/// Whether `call` changes attributes rather than reading them: what a
/// read-only mount refuses with `EROFS` first, as Linux's `mnt_want_write`
/// comes before the filesystem is asked.
fn changes(call: Syscall) -> bool {
    matches!(
        call,
        Syscall::Setxattr
            | Syscall::Lsetxattr
            | Syscall::Fsetxattr
            | Syscall::Removexattr
            | Syscall::Lremovexattr
            | Syscall::Fremovexattr
    )
}

/// The path forms, resolved first so a missing file is `ENOENT`.
fn xattr_at(process: &Process, at: u64, flags: u32, call: Syscall) -> Result<usize, Errno> {
    let target = path::target(process, AT_FDCWD, at, flags)?;
    if changes(call) {
        target.location().require_writable()?;
    }
    no_attributes(call)
}

/// The descriptor forms, checked first so a closed descriptor is `EBADF`.
fn xattr_of(process: &Process, fd: i32, call: Syscall) -> Result<usize, Errno> {
    let file = usable(process, fd)?;
    if changes(call) {
        file.location().require_writable()?;
    }
    no_attributes(call)
}
