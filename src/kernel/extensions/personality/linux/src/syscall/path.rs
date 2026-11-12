//! The calls that take a path: making, removing, renaming and linking names,
//! reading a link, the working directory, and changing what `stat` reports.
//!
//! Stage 8 of `docs/ROADMAP.md`. Each call here is a thin layer over one
//! [`Namespace`](ferrix_vfs::Namespace) operation, and deliberately so. The
//! namespace is the half of the VFS that host tests, Miri and a fuzzer have
//! walked; what is left for this module is the part only the kernel can do —
//! copy a path out of a program, work out where a relative one starts, decode
//! a flag word — and no decision about what a path *means*.
//!
//! # Where a path starts
//!
//! A `*at` call names its starting directory with a descriptor, or with
//! `AT_FDCWD` for the working directory. The descriptor is consulted only for
//! a relative path. An absolute path ignores it entirely, even a closed one,
//! which is what Linux does and what a program that passes a stale `dirfd`
//! alongside an absolute path depends on without knowing it.
//!
//! # No lock is held across a walk
//!
//! The working directory and the descriptor table are each behind a lock on
//! the process. Both are *copied out* — a [`Context`] and an `Arc` are cheap
//! to clone — and the lock is released before the namespace is asked
//! anything, so a walk into a slow filesystem cannot hold up a thread that
//! only wanted to `dup`.
//!
//! # `umask`
//!
//! Applied here, to `mkdirat` and `mknodat`, and by `openat` when it creates.
//! Linux keeps it in `fs_struct` beside the root and the working directory.
//! It is a field of its own on the process rather than part of the
//! [`Context`], because the namespace never reads it: a mode is masked before
//! the namespace sees it.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_EMPTY_PATH, AT_FDCWD, AT_HANDLE_CONNECTABLE, AT_HANDLE_FID, AT_HANDLE_MNT_ID_UNIQUE,
    AT_REMOVEDIR, AT_SYMLINK_FOLLOW, AT_SYMLINK_NOFOLLOW, RENAME_EXCHANGE, RENAME_NOREPLACE,
    RENAME_WHITEOUT, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFMT, S_IFREG, S_IFSOCK, UTIME_NOW,
    UTIME_OMIT,
};
use ferrix_vfs::access::{Access, MAY_EXEC};
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{
    Context, FileType, Location, Namespace, NewNode, OpenFile, RenameMode, SetAttributes, Stat,
    Timespec,
};

use crate::fs;
use crate::fs::inotify;
use crate::syscall::credentials;
use crate::syscall::fd::{self, arg as int, file as open_file, user_path};
use crate::syscall::process::Process;
use crate::syscall::time::TimeWidth;
use crate::syscall::uaccess;
use crate::syscall::{SyscallArgs, stat};
use crate::trap::Abi;

/// Answer `call` if it is one of this module's or [`stat`]'s.
///
/// `None` means "not mine", as it does for the stateless table in `mod.rs`,
/// which is what lets the dispatcher hand every path call over in one line.
pub(crate) fn dispatch(
    call: Syscall,
    args: &SyscallArgs,
    process: &Process,
) -> Option<Result<usize, Errno>> {
    let a = &args.args;
    describe(call, a, process, args.abi)
        .or_else(|| change(call, a, process))
        .or_else(|| attributes(call, a, process, args.abi))
}

/// The calls that report on a name without changing anything. `abi` picks
/// the `stat` family's record.
fn describe(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let answer = match call {
        Syscall::Stat | Syscall::Stat64 => stat::sys_fstatat(process, AT_FDCWD, a[0], a[1], 0, abi),
        Syscall::Lstat | Syscall::Lstat64 => {
            stat::sys_fstatat(process, AT_FDCWD, a[0], a[1], AT_SYMLINK_NOFOLLOW, abi)
        }
        Syscall::Fstat | Syscall::Fstat64 => stat::sys_fstat(process, int(a[0]), a[1], abi),
        Syscall::Newfstatat | Syscall::Fstatat64 => {
            stat::sys_fstatat(process, int(a[0]), a[1], a[2], word(a[3]), abi)
        }
        Syscall::Statx => stat::sys_statx(process, int(a[0]), a[1], word(a[2]), word(a[3]), a[4]),
        Syscall::Getdents64 => stat::sys_getdents64(process, int(a[0]), a[1], a[2]),
        Syscall::Access => stat::sys_faccessat(process, AT_FDCWD, a[0], word(a[1]), 0),
        Syscall::Faccessat => stat::sys_faccessat(process, int(a[0]), a[1], word(a[2]), 0),
        Syscall::Faccessat2 => {
            stat::sys_faccessat(process, int(a[0]), a[1], word(a[2]), word(a[3]))
        }
        Syscall::Readlink => sys_readlinkat(process, AT_FDCWD, a[0], a[1], a[2]),
        Syscall::Readlinkat => sys_readlinkat(process, int(a[0]), a[1], a[2], a[3]),
        Syscall::InotifyAddWatch => sys_inotify_add_watch(process, int(a[0]), a[1], word(a[2])),
        Syscall::NameToHandleAt => sys_name_to_handle_at(process, int(a[0]), a[1], word(a[4])),
        Syscall::Getcwd => sys_getcwd(process, a[0], a[1]),
        _ => return None,
    };
    Some(answer)
}

/// The calls that change which names exist, or where the process stands.
fn change(call: Syscall, a: &[u64; 6], process: &Process) -> Option<Result<usize, Errno>> {
    let answer = match call {
        Syscall::Mkdir => sys_mkdirat(process, AT_FDCWD, a[0], word(a[1])),
        Syscall::Mkdirat => sys_mkdirat(process, int(a[0]), a[1], word(a[2])),
        Syscall::Mknod => sys_mknodat(process, AT_FDCWD, a[0], word(a[1]), word(a[2])),
        Syscall::Mknodat => sys_mknodat(process, int(a[0]), a[1], word(a[2]), word(a[3])),
        Syscall::Unlink => sys_unlinkat(process, AT_FDCWD, a[0], 0),
        Syscall::Rmdir => sys_unlinkat(process, AT_FDCWD, a[0], AT_REMOVEDIR),
        Syscall::Unlinkat => sys_unlinkat(process, int(a[0]), a[1], word(a[2])),
        Syscall::Rename => sys_renameat2(process, (AT_FDCWD, a[0]), (AT_FDCWD, a[1]), 0),
        Syscall::Renameat => sys_renameat2(process, (int(a[0]), a[1]), (int(a[2]), a[3]), 0),
        Syscall::Renameat2 => {
            sys_renameat2(process, (int(a[0]), a[1]), (int(a[2]), a[3]), word(a[4]))
        }
        Syscall::Symlink => sys_symlinkat(process, a[0], AT_FDCWD, a[1]),
        Syscall::Symlinkat => sys_symlinkat(process, a[0], int(a[1]), a[2]),
        Syscall::Link => sys_linkat(process, (AT_FDCWD, a[0]), (AT_FDCWD, a[1]), 0),
        Syscall::Linkat => sys_linkat(process, (int(a[0]), a[1]), (int(a[2]), a[3]), word(a[4])),
        Syscall::Chdir => sys_chdir(process, a[0]),
        Syscall::Fchdir => sys_fchdir(process, int(a[0])),
        _ => return None,
    };
    Some(answer)
}

/// The calls that change what `stat` reports, and the mask on new modes.
/// `abi` lays out `utimensat`'s `timespec`s.
fn attributes(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let answer = match call {
        Syscall::Umask => usize::try_from(process.set_umask(word(a[0]))).map_err(|_| Errno::EINVAL),
        Syscall::Chmod => sys_fchmodat(process, AT_FDCWD, a[0], word(a[1])),
        Syscall::Fchmodat => sys_fchmodat(process, int(a[0]), a[1], word(a[2])),
        Syscall::Fchmod => sys_fchmod(process, int(a[0]), word(a[1])),
        Syscall::Chown => sys_fchownat(process, AT_FDCWD, a[0], (a[1], a[2]), 0),
        Syscall::Lchown => sys_fchownat(process, AT_FDCWD, a[0], (a[1], a[2]), AT_SYMLINK_NOFOLLOW),
        Syscall::Fchownat => sys_fchownat(process, int(a[0]), a[1], (a[2], a[3]), word(a[4])),
        Syscall::Fchown => sys_fchown(process, int(a[0]), (a[1], a[2])),
        Syscall::Utimensat => {
            let width = TimeWidth::Native.in_abi(abi);
            sys_utimensat(process, int(a[0]), a[1], a[2], word(a[3]), width)
        }
        Syscall::UtimensatTime64 => {
            let width = TimeWidth::Wide.in_abi(abi);
            sys_utimensat(process, int(a[0]), a[1], a[2], word(a[3]), width)
        }
        _ => return None,
    };
    Some(answer)
}

/// A flag or mode word, which is 32 bits in the ABI whatever the register.
fn word(value: u64) -> u32 {
    value as u32
}

// ---------------------------------------------------------------------------
// Names, and where they start
// ---------------------------------------------------------------------------

/// The mount namespace the process is in: the one its `mount`, `umount2` and
/// `pivot_root` change.
pub(crate) fn mount_namespace(process: &Process) -> Arc<Namespace> {
    fs::namespace_of(&process.fs_context().lock())
}

/// The process's root and working directory, copied out of their lock, and
/// the identity its permission checks are made as: its filesystem ids.
pub(crate) fn context(process: &Process) -> Context {
    let mut ctx = process.fs_context().lock().clone();
    ctx.who = process.with_credentials(|credentials| Access {
        uid: credentials.user.filesystem,
        gid: credentials.group.filesystem,
        groups: credentials.groups.clone(),
    });
    ctx
}

/// Who a process's new pipes, sockets and memfds belong to: its filesystem
/// user and group ids, as Linux gives them.
pub(crate) fn creator_ids(process: &Process) -> (u32, u32) {
    process.with_credentials(|ids| (ids.user.filesystem, ids.group.filesystem))
}

/// A path a call is about to act on, and what it is relative to.
struct Named {
    ctx: Context,
    start: Option<Location>,
    path: Vec<u8>,
}

impl Named {
    /// Where a relative path starts, or `None` for the working directory.
    fn start(&self) -> Option<&Location> {
        self.start.as_ref()
    }
}

/// Copy a path in and find where it starts.
fn named_at(process: &Process, dirfd: i32, at: u64) -> Result<Named, Errno> {
    named(process, dirfd, user_path(process, at)?)
}

/// Find where `path` starts.
///
/// An empty path is refused here, before the descriptor is looked at, so that
/// an empty path with a bad descriptor is `ENOENT` as on Linux and not
/// `EBADF`.
fn named(process: &Process, dirfd: i32, path: Vec<u8>) -> Result<Named, Errno> {
    named_in(process, context(process), dirfd, path)
}

/// [`named`], walked with `ctx` rather than the process's own context.
fn named_in(process: &Process, ctx: Context, dirfd: i32, path: Vec<u8>) -> Result<Named, Errno> {
    if path.is_empty() {
        return Err(Errno::ENOENT);
    }
    let start = fd::start_for(process, dirfd, &path)?;
    Ok(Named { ctx, start, path })
}

/// What a call that describes or changes a file is about.
#[derive(Debug)]
pub(crate) enum Target {
    /// An open file, named by descriptor.
    Open(Arc<OpenFile>),
    /// A place in the tree, reached by path.
    At(Location),
}

impl Target {
    /// Where it is in the tree.
    pub(crate) fn location(&self) -> &Location {
        match self {
            Target::Open(file) => file.location(),
            Target::At(at) => at,
        }
    }

    /// What `stat` reports about it.
    pub(crate) fn stat(&self) -> Result<Stat, Errno> {
        match self {
            Target::Open(file) => Ok(Stat {
                dev: file.location().mount.filesystem().device(),
                metadata: file.inode().metadata(),
            }),
            Target::At(at) => fs::namespace().stat(at),
        }
    }
}

/// Resolve a `*at` call's descriptor, path and `AT_*` flags.
///
/// `AT_EMPTY_PATH` with an empty path names the descriptor itself, or the
/// working directory for `AT_FDCWD`. A null path counts as empty when the flag
/// is given, as it has since Linux 6.11, which spares a program the copy.
/// `AT_SYMLINK_NOFOLLOW` decides whether a link in last position is followed;
/// every other flag is the caller's to have checked.
pub(crate) fn target(process: &Process, dirfd: i32, at: u64, flags: u32) -> Result<Target, Errno> {
    target_in(process, context(process), dirfd, at, flags)
}

/// [`target`], walked as `ctx` says rather than as the process's own
/// context: `faccessat` walks as the real ids.
pub(crate) fn target_in(
    process: &Process,
    ctx: Context,
    dirfd: i32,
    at: u64,
    flags: u32,
) -> Result<Target, Errno> {
    let empty_allowed = flags & AT_EMPTY_PATH != 0;
    let path = if empty_allowed && at == 0 {
        Vec::new()
    } else {
        user_path(process, at)?
    };
    if path.is_empty() && empty_allowed {
        if dirfd == AT_FDCWD {
            return Ok(Target::At(ctx.cwd));
        }
        return open_file(process, dirfd).map(Target::Open);
    }
    let named = named_in(process, ctx, dirfd, path)?;
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    fs::namespace()
        .resolve(&named.ctx, named.start(), &named.path, follow)
        .map(Target::At)
}

// ---------------------------------------------------------------------------
// Making and removing names
// ---------------------------------------------------------------------------

/// `mkdirat` and `mkdir`.
fn sys_mkdirat(process: &Process, dirfd: i32, at: u64, mode: u32) -> Result<usize, Errno> {
    let named = named_at(process, dirfd, at)?;
    // Linux's `vfs_mkdir` keeps the sticky bit and drops the set-id bits.
    let permissions = mode & 0o1777 & !process.umask();
    let watched = watched_name(&named);
    fs::namespace().mkdir(&named.ctx, named.start(), &named.path, permissions)?;
    made(watched, true);
    Ok(0)
}

/// The directory `named`'s last component is in, and that component: where
/// an inotify event about the name goes. `None` while nothing is watched,
/// and for a path whose last component is no name (`/`, `.`, `..`).
///
/// Looked up again beside the call it reports on, which is racy against a
/// rename of the directory in between; Linux reports from inside the call.
fn watched_name(named: &Named) -> Option<(Location, Vec<u8>)> {
    if !inotify::watching() {
        return None;
    }
    let (dir, name) = ferrix_vfs::path::split_last(&named.path);
    if name.is_empty() || name == b"." || name == b".." || name == b"/" {
        return None;
    }
    let dir: &[u8] = if dir.is_empty() { b"." } else { dir };
    let at = fs::namespace()
        .resolve(&named.ctx, named.start(), dir, true)
        .ok()?;
    Some((at, name.to_vec()))
}

/// `IN_CREATE` for a name a call made in the directory [`watched_name`]
/// found.
fn made(watched: Option<(Location, Vec<u8>)>, is_dir: bool) {
    if let Some((dir, name)) = watched {
        inotify::dir_event(&dir, &name, inotify::IN_CREATE, 0, is_dir);
    }
}

/// `mknodat` and `mknod`.
///
/// Regular files, named pipes, socket names, and character and block device
/// nodes, which only root may make, for `CAP_MKNOD`. A device
/// node records the number `dev` gives; what opening it reaches is devfs's to
/// say, by that number (`fs::devfs::attach_device`). A directory is `EPERM`,
/// as Linux answers: `mkdir` makes those. The mode's kind is checked before the
/// path is looked at, as Linux does.
fn sys_mknodat(
    process: &Process,
    dirfd: i32,
    at: u64,
    mode: u32,
    dev: u32,
) -> Result<usize, Errno> {
    let node = match mode & S_IFMT {
        0 | S_IFREG => NewNode::Regular,
        S_IFIFO => NewNode::Fifo,
        S_IFSOCK => NewNode::Socket,
        S_IFCHR => NewNode::Device {
            kind: FileType::CharDevice,
            rdev: decode_dev(dev),
        },
        S_IFBLK => NewNode::Device {
            kind: FileType::BlockDevice,
            rdev: decode_dev(dev),
        },
        S_IFDIR => return Err(Errno::EPERM),
        _ => return Err(Errno::EINVAL),
    };
    let named = named_at(process, dirfd, at)?;
    if matches!(node, NewNode::Device { .. }) {
        credentials::require_privilege(process)?;
    }
    let permissions = mode & 0o7777 & !process.umask();
    let watched = watched_name(&named);
    fs::namespace().mknod(&named.ctx, named.start(), &named.path, node, permissions)?;
    made(watched, false);
    Ok(0)
}

/// The device number a program passed to `mknod`, as a 64-bit `dev_t`.
///
/// Linux's `sys_mknodat` takes `dev` as an `unsigned int` on every
/// architecture, so only the register's low 32 bits count, and decodes them
/// with `new_decode_dev` from `include/linux/kdev_t.h`: the major is bits 8 to
/// 19, the minor the low byte with bits 20 to 31 above it. That is the layout
/// `makedev` writes for every number Linux can hold -- a 12-bit major and a
/// 20-bit minor -- so the node reports back through `st_rdev` what the program
/// passed in.
fn decode_dev(dev: u32) -> u64 {
    let major = (dev & 0xfff00) >> 8;
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    makedev(major, minor)
}

/// `unlinkat`, `unlink` and `rmdir`.
fn sys_unlinkat(process: &Process, dirfd: i32, at: u64, flags: u32) -> Result<usize, Errno> {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(Errno::EINVAL);
    }
    let named = named_at(process, dirfd, at)?;
    let ns = fs::namespace();
    let watched = watched_name(&named).and_then(|(dir, name)| {
        let node = ns
            .resolve(&named.ctx, named.start(), &named.path, false)
            .ok()?;
        let last = ns.stat(&node).ok()?.metadata.nlink <= 1;
        Some((dir, name, inotify::key_of(&node)?, last))
    });
    if flags & AT_REMOVEDIR != 0 {
        ns.rmdir(&named.ctx, named.start(), &named.path)?;
    } else {
        ns.unlink(&named.ctx, named.start(), &named.path)?;
    }
    if let Some((dir, name, (key, is_dir), last)) = watched {
        inotify::dir_event(&dir, &name, inotify::IN_DELETE, 0, is_dir);
        // Its last name gone, the node is; another name left, its count of
        // them changed.
        let what = if is_dir || last {
            inotify::IN_DELETE_SELF
        } else {
            inotify::IN_ATTRIB
        };
        inotify::self_event(key, what, is_dir);
    }
    Ok(0)
}

/// `renameat2`, `renameat` and `rename`.
///
/// `RENAME_NOREPLACE` is supported. `RENAME_EXCHANGE` and `RENAME_WHITEOUT`
/// are `EINVAL`, which is what Linux answers on a filesystem that cannot do
/// them, and every caller of either already handles it: `mv --exchange`
/// reports it, and overlayfs does not mount.
fn sys_renameat2(
    process: &Process,
    old: (i32, u64),
    new: (i32, u64),
    flags: u32,
) -> Result<usize, Errno> {
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT) != 0
        || flags & (RENAME_EXCHANGE | RENAME_WHITEOUT) != 0
    {
        return Err(Errno::EINVAL);
    }
    let mode = if flags & RENAME_NOREPLACE != 0 {
        RenameMode::NoReplace
    } else {
        RenameMode::Replace
    };
    let from = named_at(process, old.0, old.1)?;
    let to = named_at(process, new.0, new.1)?;
    let ns = fs::namespace();
    let watched = watched_name(&from)
        .zip(watched_name(&to))
        .and_then(|(source, dest)| {
            let moving = ns
                .resolve(&from.ctx, from.start(), &from.path, false)
                .ok()?;
            let replaced = ns
                .resolve(&to.ctx, to.start(), &to.path, false)
                .ok()
                .and_then(|there| inotify::key_of(&there));
            Some((source, dest, inotify::key_of(&moving)?, replaced))
        });
    ns.rename(
        &from.ctx,
        (from.start(), &from.path),
        (to.start(), &to.path),
        mode,
    )?;
    if let Some(((from_dir, from_name), (to_dir, to_name), (key, is_dir), replaced)) = watched {
        let cookie = inotify::next_cookie();
        inotify::dir_event(
            &from_dir,
            &from_name,
            inotify::IN_MOVED_FROM,
            cookie,
            is_dir,
        );
        inotify::dir_event(&to_dir, &to_name, inotify::IN_MOVED_TO, cookie, is_dir);
        inotify::self_event(key, inotify::IN_MOVE_SELF, is_dir);
        if let Some((gone, gone_dir)) = replaced.filter(|(gone, _)| *gone != key) {
            inotify::self_event(gone, inotify::IN_DELETE_SELF, gone_dir);
        }
    }
    Ok(0)
}

/// `symlinkat` and `symlink`: make `at` a link to `target`.
///
/// The target is copied with the same bound as a path, but it is not one yet:
/// it is stored as given and means something only when a walk reaches it.
fn sys_symlinkat(process: &Process, target: u64, dirfd: i32, at: u64) -> Result<usize, Errno> {
    let target = user_path(process, target)?;
    let named = named_at(process, dirfd, at)?;
    let watched = watched_name(&named);
    fs::namespace().symlink(&named.ctx, named.start(), &named.path, &target)?;
    made(watched, false);
    Ok(0)
}

/// `linkat` and `link`.
///
/// `AT_EMPTY_PATH` is accepted and an empty old path is then `ENOENT`, which
/// is Linux's answer to a caller without `CAP_DAC_READ_SEARCH`. Root has that
/// capability, but linking an open file back into the tree needs a namespace
/// operation on a location rather than a path, which nothing has asked for.
fn sys_linkat(
    process: &Process,
    old: (i32, u64),
    new: (i32, u64),
    flags: u32,
) -> Result<usize, Errno> {
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let from = named_at(process, old.0, old.1)?;
    let to = named_at(process, new.0, new.1)?;
    let follow = flags & AT_SYMLINK_FOLLOW != 0;
    let watched = watched_name(&to);
    fs::namespace().link(
        &from.ctx,
        (from.start(), &from.path),
        follow,
        (to.start(), &to.path),
    )?;
    if watched.is_some() {
        // One more name for the node: its link count changed.
        if let Ok(node) = fs::namespace().resolve(&from.ctx, from.start(), &from.path, follow) {
            inotify::node_event(&node, inotify::IN_ATTRIB);
        }
    }
    made(watched, false);
    Ok(0)
}

/// `inotify_add_watch`: watch what `at` names, from the working directory,
/// following a final link unless the mask says not to.
///
/// # Errors
///
/// `EBADF` for a closed descriptor and `EINVAL` for one that is not an
/// inotify instance; the walk's errors; `EACCES` without read permission on
/// the node, as Linux asks; and `inotify::add_watch`'s.
fn sys_inotify_add_watch(process: &Process, fd: i32, at: u64, mask: u32) -> Result<usize, Errno> {
    let file = open_file(process, fd)?;
    let instance = inotify::of(&file).ok_or(Errno::EINVAL)?;
    let named = named_at(process, AT_FDCWD, at)?;
    let node = fs::namespace().resolve(
        &named.ctx,
        named.start(),
        &named.path,
        inotify::follows(mask),
    )?;
    named
        .ctx
        .who
        .require(&node.inode()?.metadata(), ferrix_vfs::access::MAY_READ)?;
    let wd = inotify::add_watch(&instance, &node, mask)?;
    usize::try_from(wd).map_err(|_| Errno::EINVAL)
}

/// `name_to_handle_at`: a file handle for what `at` names.
///
/// No filesystem here encodes one, so a name that resolves is
/// `EOPNOTSUPP`, which is Linux's answer for a filesystem without export
/// operations, and it comes, as there, after the flags are checked and the
/// walk is made but before the caller's handle is read. The walk still
/// matters: a name that does not exist is `ENOENT`, not `EOPNOTSUPP`.
///
/// `EOPNOTSUPP` is also the one failure libudev takes quietly. It asks this
/// of `/dev` to learn whether devtmpfs is there, and any other errno is
/// printed on every call; Steam's client asks over and over.
fn sys_name_to_handle_at(
    process: &Process,
    dirfd: i32,
    at: u64,
    flags: u32,
) -> Result<usize, Errno> {
    let known = AT_SYMLINK_FOLLOW
        | AT_EMPTY_PATH
        | AT_HANDLE_FID
        | AT_HANDLE_MNT_ID_UNIQUE
        | AT_HANDLE_CONNECTABLE;
    if flags & !known != 0 {
        return Err(Errno::EINVAL);
    }
    // A connectable handle needs a parent to connect to, which neither a
    // handle only for comparing nor a descriptor named by itself promises.
    if flags & AT_HANDLE_CONNECTABLE != 0 && flags & (AT_HANDLE_FID | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    // A final link is followed only when asked, the other way round from
    // `target`'s flag.
    let mut walk = flags & AT_EMPTY_PATH;
    if flags & AT_SYMLINK_FOLLOW == 0 {
        walk |= AT_SYMLINK_NOFOLLOW;
    }
    let _named = target(process, dirfd, at, walk)?;
    Err(Errno::EOPNOTSUPP)
}

/// `readlinkat` and `readlink`.
///
/// The target is not NUL-terminated and is cut silently to the buffer: the
/// return value is how much was written, and a caller that wants to know
/// whether that was all of it retries with a larger buffer. A size of zero or
/// less is `EINVAL`, checked first.
///
/// An empty path reads the link a descriptor was opened on with
/// `O_PATH | O_NOFOLLOW`; for anything else it is `ENOENT`, where a non-empty
/// path naming a non-link is `EINVAL`. Both are Linux's.
fn sys_readlinkat(
    process: &Process,
    dirfd: i32,
    at: u64,
    buf: u64,
    size: u64,
) -> Result<usize, Errno> {
    let size = usize::try_from(int(size))
        .ok()
        .filter(|&size| size > 0)
        .ok_or(Errno::EINVAL)?;
    let path = user_path(process, at)?;
    let target = if path.is_empty() {
        link_of_descriptor(process, dirfd)?
    } else {
        let named = named(process, dirfd, path)?;
        fs::namespace().read_link(&named.ctx, named.start(), &named.path)?
    };
    let written = target.get(..size.min(target.len())).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), buf, written).map_err(|_| Errno::EFAULT)?;
    Ok(written.len())
}

/// The target of the symbolic link a descriptor is open on.
fn link_of_descriptor(process: &Process, dirfd: i32) -> Result<Vec<u8>, Errno> {
    if dirfd == AT_FDCWD {
        return Err(Errno::ENOENT);
    }
    let file = open_file(process, dirfd)?;
    if file.kind() != FileType::Symlink {
        return Err(Errno::ENOENT);
    }
    file.inode().read_link()
}

// ---------------------------------------------------------------------------
// The working directory
// ---------------------------------------------------------------------------

/// `chdir`.
fn sys_chdir(process: &Process, at: u64) -> Result<usize, Errno> {
    let named = named_at(process, AT_FDCWD, at)?;
    let place = fs::namespace().resolve(&named.ctx, named.start(), &named.path, true)?;
    set_cwd(process, &named.ctx.who, place)
}

/// `fchdir`.
fn sys_fchdir(process: &Process, fd: i32) -> Result<usize, Errno> {
    let file = open_file(process, fd)?;
    set_cwd(process, &context(process).who, file.location().clone())
}

/// Make `place` the working directory, if it is a directory `who` may
/// search.
fn set_cwd(process: &Process, who: &Access, place: Location) -> Result<usize, Errno> {
    let metadata = place.inode()?.metadata();
    if metadata.kind != FileType::Directory {
        return Err(Errno::ENOTDIR);
    }
    who.require(&metadata, MAY_EXEC)?;
    let old = core::mem::replace(&mut process.fs_context().lock().cwd, place);
    // Dropped with the lock released: the last reference to a location may
    // release a chain of parent dentries, and that chain is unbounded.
    drop(old);
    Ok(0)
}

/// `getcwd`.
///
/// Returns the length *including* the terminator, which is the system call's
/// convention and not the C library's: musl turns it into a pointer. A buffer
/// too small is `ERANGE`, and a working directory that has been removed is
/// `ENOENT`, because the path it used to have now names nothing, or something
/// else.
pub(crate) fn sys_getcwd(process: &Process, buf: u64, size: u64) -> Result<usize, Errno> {
    let ctx = context(process);
    if ctx.cwd.dentry.is_unhashed() {
        return Err(Errno::ENOENT);
    }
    let mut path = fs::namespace().path_of(&ctx.cwd, &ctx.root);
    path.push(0);
    if u64::try_from(path.len()).map_or(true, |len| len > size) {
        return Err(Errno::ERANGE);
    }
    uaccess::copy_to_user(process.space(), buf, &path).map_err(|_| Errno::EFAULT)?;
    Ok(path.len())
}

// ---------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------

/// Apply `change` to what `target` names, as the caller may make it: the
/// rules `chmod` and `chown` follow, from `Access::check_change`.
fn set(process: &Process, target: &Target, change: &SetAttributes) -> Result<usize, Errno> {
    let metadata = target.stat()?.metadata;
    // `chmod_common` and its kin: a read-only mount is `EROFS` before the
    // caller's right to the change is asked.
    target.location().require_writable()?;
    let change = context(process).who.check_change(&metadata, change)?;
    fs::namespace().set_attributes(target.location(), &change)?;
    inotify::node_event(target.location(), inotify::IN_ATTRIB);
    Ok(0)
}

/// The open file a descriptor names, for a call that may not act through an
/// `O_PATH` descriptor: `fchmod` and `fchown` are `EBADF` on one.
fn open_for_change(process: &Process, fd: i32) -> Result<Target, Errno> {
    let file = open_file(process, fd)?;
    if file.is_path() {
        return Err(Errno::EBADF);
    }
    Ok(Target::Open(file))
}

/// `chmod`'s change: the permission and set-id bits, and nothing of the kind.
fn permissions(mode: u32) -> SetAttributes {
    SetAttributes {
        permissions: Some(mode & 0o7777),
        ..SetAttributes::default()
    }
}

/// `fchmodat` and `chmod`. Always follows a link in last position: the
/// system call has no flags argument, whatever the C library's has.
fn sys_fchmodat(process: &Process, dirfd: i32, at: u64, mode: u32) -> Result<usize, Errno> {
    set(process, &target(process, dirfd, at, 0)?, &permissions(mode))
}

/// `fchmod`.
fn sys_fchmod(process: &Process, fd: i32, mode: u32) -> Result<usize, Errno> {
    set(process, &open_for_change(process, fd)?, &permissions(mode))
}

/// `chown`'s change. An identifier of `-1` leaves that one alone.
///
/// The ids are as the caller's user namespace names them: one it does not map
/// is `EINVAL`, so an unmapped id can never be written into a file's owner
/// (rule U9).
fn owner(process: &Process, (uid, gid): (u64, u64)) -> Result<SetAttributes, Errno> {
    let id = |value: u64| Some(word(value)).filter(|&id| id != u32::MAX);
    let uid = id(uid)
        .map(|id| credentials::kernel_uid(process, id))
        .transpose()?;
    let gid = id(gid)
        .map(|id| credentials::kernel_gid(process, id))
        .transpose()?;
    Ok(SetAttributes {
        uid,
        gid,
        ..SetAttributes::default()
    })
}

/// `fchownat`, `chown` and `lchown`.
///
/// Only a privileged caller may give a file away; its owner may change its
/// group to one of the owner's own groups.
fn sys_fchownat(
    process: &Process,
    dirfd: i32,
    at: u64,
    ids: (u64, u64),
    flags: u32,
) -> Result<usize, Errno> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    set(
        process,
        &target(process, dirfd, at, flags)?,
        &owner(process, ids)?,
    )
}

/// `fchown`.
fn sys_fchown(process: &Process, fd: i32, ids: (u64, u64)) -> Result<usize, Errno> {
    set(
        process,
        &open_for_change(process, fd)?,
        &owner(process, ids)?,
    )
}

/// `utimensat` and `utimensat_time64`.
///
/// A null `times` sets both to now. Otherwise each `tv_nsec` may be
/// `UTIME_NOW`, `UTIME_OMIT` or a real nanosecond count, and both omitted
/// returns success *before the path is looked at*, which Linux's source says
/// in so many words. A null path is `futimens`: the descriptor itself, with
/// no flags.
fn sys_utimensat(
    process: &Process,
    dirfd: i32,
    at: u64,
    times: u64,
    flags: u32,
    width: TimeWidth,
) -> Result<usize, Errno> {
    let ([atime, mtime], given) = read_times(process, times, width)?;
    if atime.is_none() && mtime.is_none() {
        return Ok(0);
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::EINVAL);
    }
    let target = if at == 0 {
        if dirfd == AT_FDCWD {
            return Err(Errno::EFAULT);
        }
        if flags != 0 {
            return Err(Errno::EINVAL);
        }
        Target::Open(open_file(process, dirfd)?)
    } else {
        target(process, dirfd, at, flags)?
    };
    context(process)
        .who
        .may_set_times(&target.stat()?.metadata, given)?;
    let change = SetAttributes {
        atime,
        mtime,
        ..SetAttributes::default()
    };
    set(process, &target, &change)
}

/// The two times `utimensat` was given, `None` for one to leave alone, and
/// whether either was a value rather than `UTIME_NOW` -- which decides
/// whether setting them needs ownership or only write permission.
///
/// Two `timespec`s of two fields each, and the fields are `long`s for the
/// native call — four bytes on ARMv7-A — and 64 bits for the `time64` one.
/// The width is read from the pointer size rather than from the architecture,
/// because that is the whole of the difference.
fn read_times(
    process: &Process,
    at: u64,
    width: TimeWidth,
) -> Result<([Option<Timespec>; 2], bool), Errno> {
    let now = fs::clock().now();
    if at == 0 {
        return Ok(([Some(now), Some(now)], false));
    }
    let field = width.field_bytes();
    let mut bytes = [0_u8; 32];
    let raw = bytes.get_mut(..field * 4).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, raw).map_err(|_| Errno::EFAULT)?;

    let mut times = [None, None];
    let mut given = false;
    for (index, slot) in times.iter_mut().enumerate() {
        let tv_sec = signed_field(raw, index * 2 * field, field)?;
        let tv_nsec = signed_field(raw, (index * 2 + 1) * field, field)?;
        // A 64-bit `tv_nsec` from a 32-bit program keeps its low half, as
        // `get_timespec64` does: the rest is padding a libc need not write.
        let tv_nsec = if field == 8 && width.long_is_32() {
            i64::from(tv_nsec as i32)
        } else {
            tv_nsec
        };
        *slot = match tv_nsec {
            UTIME_NOW => Some(now),
            UTIME_OMIT => None,
            0..=999_999_999 => {
                given = true;
                Some(Timespec { tv_sec, tv_nsec })
            }
            _ => return Err(Errno::EINVAL),
        };
    }
    Ok((times, given))
}

/// A signed little-endian field of four or eight bytes, sign-extended.
fn signed_field(bytes: &[u8], at: usize, width: usize) -> Result<i64, Errno> {
    let raw = bytes
        .get(at..at.saturating_add(width))
        .ok_or(Errno::EINVAL)?;
    match *raw {
        [a, b, c, d] => Ok(i64::from(i32::from_le_bytes([a, b, c, d]))),
        [a, b, c, d, e, f, g, h] => Ok(i64::from_le_bytes([a, b, c, d, e, f, g, h])),
        _ => Err(Errno::EINVAL),
    }
}
