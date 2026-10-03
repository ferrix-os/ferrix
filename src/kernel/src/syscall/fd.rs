//! Descriptors: `openat`, `close`, `dup` and its kin, `fcntl`, `lseek`,
//! `ftruncate` and `ioctl`.
//!
//! The rules about numbers live in `ferrix_vfs::fd::FdTable` and the rules
//! about offsets in `ferrix_vfs::OpenFile`, both host-tested. What is here is
//! the part neither can do: reading a program's arguments, choosing this
//! architecture's flag bits, and holding the process's table lock for exactly
//! as long as a table operation takes.
//!
//! # The lock is never held across I/O
//!
//! Every call takes the table lock, clones or removes what it needs, and lets
//! go before touching a file. A `read` of the console waits minutes for a
//! person to type; a table lock held across it would stop every other thread
//! of the program from opening anything. And a description that `close` or
//! `dup2` displaces is dropped after the lock is released, because dropping
//! the last reference to an open file releases its dentry, and that chain is
//! unbounded.
//!
//! # `ioctl`
//!
//! Resolved here like every other call on a descriptor, and handed to
//! `crate::syscall::tty` when the descriptor is the console. Every other file
//! is `ENOTTY`.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    AT_FDCWD, F_ADD_SEALS, F_DUPFD, F_DUPFD_CLOEXEC, F_GET_SEALS, F_GETFD, F_GETFL, F_SETFD,
    F_SETFL, FD_CLOEXEC, FIOCLEX, FIONBIO, FIONCLEX, O_ACCMODE, O_APPEND, O_CLOEXEC, O_CREAT,
    O_EXCL, O_NONBLOCK, O_PATH, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY, SEEK_CUR, SEEK_DATA, SEEK_END,
    SEEK_HOLE, SEEK_SET,
};
use ferrix_vfs::fd::FdTable;
use ferrix_vfs::{Context, FileType, Location, OpenFile, OpenFlags, Resolve, Whence};

use crate::arch;
use crate::fallible::AllocError;
use crate::fs;
use crate::fs::console;
use crate::panic::{catalog, fatal};
use crate::syscall::process::Process;
use crate::syscall::tty;
use crate::syscall::uaccess::{self, UserError};
use crate::user::vmo::Vmo;

/// Linux's `PATH_MAX`, counting the terminator.
const PATH_MAX: usize = 4096;

/// A descriptor argument, which the ABI passes as a 32-bit `int`.
///
/// Narrowed first so that `-1` from a 32-bit caller and from a 64-bit one are
/// both `-1`, and so that a 64-bit caller's stale upper half is ignored, as
/// Linux ignores it.
pub(crate) fn arg(value: u64) -> i32 {
    value as u32 as i32
}

/// The description `fd` names, with the table lock already released.
pub(crate) fn file(process: &Process, fd: i32) -> Result<Arc<OpenFile>, Errno> {
    process.files().lock().get(fd).map(Arc::clone)
}

/// Where a `*at` call's relative path starts: `None` for the working
/// directory, the directory `dirfd` names otherwise.
///
/// # Errors
///
/// `EBADF` for a descriptor that names nothing, `ENOTDIR` for one that names
/// something other than a directory. An `O_PATH` descriptor on a directory is
/// accepted, which is what `O_PATH` exists for.
pub(crate) fn start_location(process: &Process, dirfd: i32) -> Result<Option<Location>, Errno> {
    if dirfd == AT_FDCWD {
        return Ok(None);
    }
    let file = file(process, dirfd)?;
    if file.kind() != FileType::Directory {
        return Err(Errno::ENOTDIR);
    }
    Ok(Some(file.location().clone()))
}

/// [`start_location`] for a particular path.
///
/// An absolute path never looks at `dirfd`, which Linux documents and programs
/// rely on: `openat(-1, "/etc/passwd", ...)` succeeds. Refusing the bad
/// descriptor first would break them.
pub(crate) fn start_for(
    process: &Process,
    dirfd: i32,
    path: &[u8],
) -> Result<Option<Location>, Errno> {
    if path.first() == Some(&b'/') {
        return Ok(None);
    }
    start_location(process, dirfd)
}

/// A path out of the program's memory.
///
/// # Errors
///
/// `ENAMETOOLONG` for a path with no terminator within `PATH_MAX`, `EFAULT`
/// for one the program cannot read.
pub(crate) fn user_path(process: &Process, at: u64) -> Result<Vec<u8>, Errno> {
    let mut path = Vec::new();
    match uaccess::copy_cstr_from_user(process.space(), at, PATH_MAX, &mut path) {
        Ok(()) => Ok(path),
        // The copy stops at the limit with every byte read, and at a fault
        // with fewer: the length is what tells the two apart.
        Err(UserError::Fault) if path.len() >= PATH_MAX => Err(Errno::ENAMETOOLONG),
        Err(_) => Err(Errno::EFAULT),
    }
}

/// Descriptors 0, 1 and 2 for a new process, all naming one open description
/// of the console.
///
/// One description rather than three opens, which is how Linux's first
/// process gets them and what a program can observe: `fcntl(0, F_SETFL,
/// O_NONBLOCK)` changes descriptor 1 too.
///
/// Charged to no job (`quota::charging_nobody`): a job at its memory limit
/// must not be able to make this fail. The table is charged to the
/// process's job the first time it grows.
///
/// # Errors
///
/// [`AllocError`] when there is no memory for the table, which
/// `process_create` answers with `NO_MEMORY` (F-23): it used to stop the
/// kernel, on an item path. The injection policy the checks fail
/// allocations with is asked first, since the table's own growth does not
/// ask it. The console's open description is still made by `OpenFile::new`,
/// whose `Arc` cannot report a refusal (MEMORY-AND-TIMING §1.3). A console
/// that cannot be opened, or a new table with no room for three, is a kernel
/// bug and stops it (`CONSOLE_DESCRIPTORS`).
pub(crate) fn standard_streams() -> Result<FdTable<Arc<OpenFile>>, AllocError> {
    ferrix_fallible::check()?;
    match crate::object::quota::charging_nobody(console_table) {
        Ok(table) => Ok(table),
        Err(Errno::ENOMEM) => Err(AllocError),
        Err(errno) => fatal!(
            catalog::CONSOLE_DESCRIPTORS,
            "a new process could not be given the console: errno {}",
            errno.0
        ),
    }
}

/// See [`standard_streams`].
fn console_table() -> Result<FdTable<Arc<OpenFile>>, Errno> {
    let console = console::open_console()?;
    let mut table = FdTable::new();
    for _ in 0..3 {
        let _ = table.insert(Arc::clone(&console), false)?;
    }
    Ok(table)
}

/// A descriptor number as a return value.
fn number(fd: i32) -> Result<usize, Errno> {
    usize::try_from(fd).map_err(|_| Errno::EBADF)
}

/// Decode `open`'s flag word with this architecture's bits, into what the VFS
/// is asked for and whether the descriptor is close-on-exec.
///
/// `O_DIRECT` and `O_LARGEFILE` are recognised and ignored: nothing here has a
/// page cache to bypass, and every offset is 64 bits. They are in the table so
/// that their bits are never mistaken for the flags that share them on another
/// architecture. Unknown bits are ignored, as `open` has always ignored them.
pub(crate) fn decode_open_flags(raw: u32) -> (OpenFlags, bool) {
    let bits = arch::OPEN_FLAGS;
    let set = |flag: u32| raw & flag != 0;
    let cloexec = set(O_CLOEXEC);
    if set(O_PATH) {
        // `O_PATH` keeps only these three; every other flag is ignored.
        let flags = OpenFlags {
            path: true,
            directory: set(bits.directory),
            nofollow: set(bits.nofollow),
            ..OpenFlags::default()
        };
        return (flags, cloexec);
    }
    let (read, write) = match raw & O_ACCMODE {
        O_RDONLY => (true, false),
        O_WRONLY => (false, true),
        O_RDWR => (true, true),
        // Mode 3: neither, which Linux allows for a descriptor only `ioctl`
        // will use.
        _ => (false, false),
    };
    let flags = OpenFlags {
        read,
        write,
        create: set(O_CREAT),
        exclusive: set(O_EXCL),
        truncate: set(O_TRUNC),
        append: set(O_APPEND),
        directory: set(bits.directory),
        nofollow: set(bits.nofollow),
        path: false,
        nonblock: set(O_NONBLOCK),
    };
    (flags, cloexec)
}

/// `openat`, and `open` with `AT_FDCWD`.
pub(crate) fn sys_openat(
    process: &Process,
    dirfd: i32,
    path: u64,
    raw_flags: u32,
    mode: u32,
) -> Result<usize, Errno> {
    let path = user_path(process, path)?;
    let start = start_for(process, dirfd, &path)?;
    // A copy of the context rather than the lock: the walk calls into
    // filesystems, and `chdir` on another thread must not wait for it. It
    // carries the caller's identity, which the walk and the open check.
    let context = crate::syscall::path::context(process);
    open_with(
        process,
        &context,
        start.as_ref(),
        &path,
        raw_flags,
        mode,
        Resolve::default(),
    )
}

/// `openat2`'s `RESOLVE_NO_XDEV`, `RESOLVE_NO_MAGICLINKS`,
/// `RESOLVE_NO_SYMLINKS`, `RESOLVE_BENEATH`, `RESOLVE_IN_ROOT` and
/// `RESOLVE_CACHED`, from `include/uapi/linux/openat2.h`.
const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_IN_ROOT: u64 = 0x10;
const RESOLVE_CACHED: u64 = 0x20;

/// The first `struct open_how`, `OPEN_HOW_SIZE_VER0`: `flags`, `mode` and
/// `resolve`, a `u64` each.
const OPEN_HOW_SIZE: usize = 24;

/// `openat2(dirfd, path, how, size)`: `openat` with its flags in a `struct
/// open_how` and a walk restricted as `how.resolve` says.
///
/// bubblewrap from 0.12 opens every place it binds from and onto this way,
/// with `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`, and Debian builds it with
/// no fallback (`docs/NAMESPACES.md`, N3). `RESOLVE_IN_ROOT` and
/// `RESOLVE_BENEATH` walk with `dirfd` as the root; `RESOLVE_CACHED` asks to
/// be refused `EAGAIN` rather than wait for a lookup, and every walk here
/// runs to its end, which is the answer a caller that retries without it
/// would get.
///
/// # Errors
///
/// Linux's, in `copy_struct_from_user` and `build_open_how`'s order:
/// `EINVAL` for a `size` below the first version, `E2BIG` above a page or
/// with a nonzero byte past what is known; `EINVAL` for flags above 32 bits,
/// an unknown resolve flag, `RESOLVE_BENEATH` with `RESOLVE_IN_ROOT`, or a
/// mode without `O_CREAT` or one outside `07777`; `EAGAIN` for
/// `RESOLVE_CACHED` with `O_CREAT` or `O_TRUNC`; then `openat`'s own, and
/// `ELOOP` or `EXDEV` where the walk is refused a step.
pub(crate) fn sys_openat2(
    process: &Process,
    dirfd: i32,
    path: u64,
    how: u64,
    size: u64,
) -> Result<usize, Errno> {
    if size < OPEN_HOW_SIZE as u64 {
        return Err(Errno::EINVAL);
    }
    if size > ferrix_bootinfo::PAGE_SIZE {
        return Err(Errno::E2BIG);
    }
    let size = usize::try_from(size).map_err(|_| Errno::E2BIG)?;
    let mut raw = [0_u8; OPEN_HOW_SIZE];
    uaccess::copy_from_user(process.space(), how, &mut raw).map_err(|_| Errno::EFAULT)?;
    if size > OPEN_HOW_SIZE {
        let mut rest = alloc::vec![0_u8; size - OPEN_HOW_SIZE];
        let past = how.checked_add(OPEN_HOW_SIZE as u64).ok_or(Errno::EFAULT)?;
        uaccess::copy_from_user(process.space(), past, &mut rest).map_err(|_| Errno::EFAULT)?;
        if rest.iter().any(|&byte| byte != 0) {
            return Err(Errno::E2BIG);
        }
    }
    let word = |at: usize| {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(raw.get(at..at + 8).unwrap_or(&[0; 8]));
        u64::from_ne_bytes(bytes)
    };
    let (flags, mode, resolve) = (word(0), word(8), word(16));
    let raw_flags = u32::try_from(flags).map_err(|_| Errno::EINVAL)?;
    let known = RESOLVE_NO_XDEV
        | RESOLVE_NO_MAGICLINKS
        | RESOLVE_NO_SYMLINKS
        | RESOLVE_BENEATH
        | RESOLVE_IN_ROOT
        | RESOLVE_CACHED;
    if resolve & !known != 0
        || resolve & (RESOLVE_BENEATH | RESOLVE_IN_ROOT) == RESOLVE_BENEATH | RESOLVE_IN_ROOT
        || mode & !0o7777 != 0
        || (mode != 0 && raw_flags & O_CREAT == 0)
    {
        return Err(Errno::EINVAL);
    }
    if resolve & RESOLVE_CACHED != 0 && raw_flags & (O_CREAT | O_TRUNC) != 0 {
        return Err(Errno::EAGAIN);
    }
    let path = user_path(process, path)?;
    let mut context = crate::syscall::path::context(process);
    let rooted = resolve & (RESOLVE_BENEATH | RESOLVE_IN_ROOT) != 0;
    let start = if rooted {
        // The walk's root is `dirfd`'s directory, absolute paths included.
        let dir = start_location(process, dirfd)?.unwrap_or_else(|| context.cwd.clone());
        context.root = dir.clone();
        Some(dir)
    } else {
        start_for(process, dirfd, &path)?
    };
    let resolve = Resolve {
        no_xdev: resolve & RESOLVE_NO_XDEV != 0,
        no_magic_links: resolve & RESOLVE_NO_MAGICLINKS != 0,
        no_symlinks: resolve & RESOLVE_NO_SYMLINKS != 0,
        beneath: resolve & RESOLVE_BENEATH != 0,
    };
    // `mode` fits: it was checked against 07777 above.
    open_with(
        process,
        &context,
        start.as_ref(),
        &path,
        raw_flags,
        mode as u32,
        resolve,
    )
}

/// Open `path` from `start` in `context`, resolving as `resolve` says, and
/// install it: what `openat` and `openat2` share once their arguments are
/// read.
fn open_with(
    process: &Process,
    context: &Context,
    start: Option<&Location>,
    path: &[u8],
    raw_flags: u32,
    mode: u32,
    resolve: Resolve,
) -> Result<usize, Errno> {
    let (flags, cloexec) = decode_open_flags(raw_flags);
    // The descriptor before the path, as Linux takes it. An open refused for
    // `EMFILE` only after it had created its file would leave the file
    // behind, and the program's retry with `O_EXCL` would be `EEXIST`.
    let reserved = process.files().lock().reserve(cloexec)?;
    // Whether this open makes the file, for inotify's `IN_CREATE`: asked
    // only while something is watched.
    let creating = flags.create
        && fs::inotify::watching()
        && fs::namespace()
            .resolve(context, start, path, !flags.nofollow)
            .is_err();
    let opened = fs::namespace()
        .open_resolving(
            context,
            start,
            path,
            &flags,
            mode & 0o7777 & !process.umask(),
            resolve,
        )
        // A read-only `/proc/sys` value opened for writing is refused, as is
        // a sysfs attribute opened for what it cannot do, a named pipe opens
        // as a pipe end, and a device node as the device its number names;
        // a socket or anonymous file reached through `/proc/<pid>/fd` is
        // refused; everything else is returned as it is.
        .and_then(fs::procfs::refuse_write_open)
        .and_then(fs::procfs::refuse_reopen)
        .and_then(fs::sysfs::refuse_open)
        .and_then(fs::pipe::attach_fifo)
        .and_then(fs::devfs::attach_device);
    let file = match opened {
        Ok(file) => file,
        Err(errno) => {
            process.files().lock().release(reserved);
            return Err(errno);
        }
    };
    if creating {
        fs::inotify::node_event(file.location(), fs::inotify::IN_CREATE);
    }
    fs::inotify::opened(&file);
    // The guard is a temporary of this statement: a file handed back by a
    // failed fill is dropped with the lock released, as the module requires.
    let filled = process.files().lock().fill(reserved, file);
    number(filled.map_err(|_unfilled| Errno::EBADF)?)
}

/// A descriptor on `file` has ended: closed, displaced by `dup2` or `dup3`,
/// closed on exec, or closed by its process's exit.
///
/// **Every path that ends a descriptor calls this**, with the table's lock let
/// go and before the file is dropped, so that what an ending means is said here
/// once rather than known at each of those paths -- `close_range` and a
/// descriptor passed over a socket will be more of them. Today it means the
/// classic record locks `process` holds on the file go, as Linux's
/// `locks_remove_posix` takes them on every close, whichever descriptor set
/// them.
///
/// The last one to end also reports the file's close to inotify: the
/// caller's hold is then the only one left.
pub(crate) fn closed(process: &Process, file: &Arc<OpenFile>) {
    crate::syscall::flock::closed(process, file);
    if Arc::strong_count(file) == 1 {
        fs::inotify::closed(file);
    }
}

/// `close`.
pub(crate) fn sys_close(process: &Process, fd: i32) -> Result<usize, Errno> {
    // The guard is a temporary of this statement, so the description is
    // dropped below with the lock already released.
    let file = process.files().lock().remove(fd)?;
    closed(process, &file);
    drop(file);
    let _ = fs::socket::collect_cycles();
    Ok(0)
}

/// `dup`.
pub(crate) fn sys_dup(process: &Process, fd: i32) -> Result<usize, Errno> {
    let mut files = process.files().lock();
    let file = Arc::clone(files.get(fd)?);
    number(files.insert(file, false)?)
}

/// `dup2`: as `dup3` with no flags, except that duplicating a descriptor onto
/// itself is a check that it is open rather than an error.
pub(crate) fn sys_dup2(process: &Process, old: i32, new: i32) -> Result<usize, Errno> {
    if old == new {
        let _ = file(process, old)?;
        return number(new);
    }
    replace(process, old, new, false)
}

/// `dup3`.
pub(crate) fn sys_dup3(process: &Process, old: i32, new: i32, flags: u32) -> Result<usize, Errno> {
    if flags & !O_CLOEXEC != 0 || old == new {
        return Err(Errno::EINVAL);
    }
    replace(process, old, new, flags & O_CLOEXEC != 0)
}

/// Make `new` name what `old` does, in one hold of the table lock, so that a
/// `close(old)` on another thread cannot land between the lookup and the
/// install.
fn replace(process: &Process, old: i32, new: i32, cloexec: bool) -> Result<usize, Errno> {
    let displaced = {
        let mut files = process.files().lock();
        let file = Arc::clone(files.get(old)?);
        files.install(new, file, cloexec)?
    };
    // Released outside the lock: this may be the last reference.
    if let Some(file) = &displaced {
        closed(process, file);
    }
    drop(displaced);
    number(new)
}

/// `fcntl` and `fcntl64`, which differ only in the record-lock commands, and
/// those are answered by `crate::syscall::flock` before this is reached.
///
/// The descriptor is looked up before the command is: an unknown command on a
/// closed descriptor is `EBADF`, as on Linux.
pub(crate) fn sys_fcntl(process: &Process, fd: i32, cmd: u32, arg: u64) -> Result<usize, Errno> {
    let file = file(process, fd)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            // At or above the table's limit is `EINVAL`, which the table says.
            // A copy goes in, so that the one a refused insert drops under the
            // lock is never the last: `fd` may have been closed meanwhile.
            let min = i32::try_from(arg).map_err(|_| Errno::EINVAL)?;
            let new = process.files().lock().insert_from(
                min,
                Arc::clone(&file),
                cmd == F_DUPFD_CLOEXEC,
            )?;
            number(new)
        }
        F_GETFD => {
            let cloexec = process.files().lock().cloexec(fd)?;
            Ok(if cloexec { FD_CLOEXEC as usize } else { 0 })
        }
        F_SETFD => {
            let cloexec = arg & u64::from(FD_CLOEXEC) != 0;
            process.files().lock().set_cloexec(fd, cloexec)?;
            Ok(0)
        }
        F_GETFL => Ok(status_word(&file) as usize),
        _ if file.is_path() => Err(Errno::EBADF),
        F_SETFL => {
            // Only these two may change after the open; Linux ignores the
            // access mode and the creation flags here rather than refusing.
            let mut status = file.status();
            status.append = arg & u64::from(O_APPEND) != 0;
            status.nonblock = arg & u64::from(O_NONBLOCK) != 0;
            file.set_status(status);
            Ok(0)
        }
        F_GET_SEALS => file.inode().seals().map(|seals| seals as usize),
        F_ADD_SEALS => {
            // `memfd_add_seals`' order: a file not open for writing is `EPERM`
            // before anything about the seals or the filesystem is asked.
            if !file.writable() {
                return Err(Errno::EPERM);
            }
            let seals = u32::try_from(arg).map_err(|_| Errno::EINVAL)?;
            let object = file
                .inode()
                .mapping()
                .and_then(|object| object.downcast::<Vmo>().ok());
            let writably_mapped = || object.as_ref().is_some_and(|vmo| vmo.writably_mapped());
            file.inode().add_seals(seals, &writably_mapped)?;
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// What `F_GETFL` reports: the access mode and the status flags.
///
/// busybox's `printf` builtin asks this of descriptor 1 before it writes and
/// prints nothing if the call fails, so the console's descriptors must answer
/// it -- with `O_RDWR`, which is how they were opened.
fn status_word(file: &OpenFile) -> u32 {
    if file.is_path() {
        return O_PATH;
    }
    let mode = match (file.readable(), file.writable()) {
        (true, true) => O_RDWR,
        (false, true) => O_WRONLY,
        (true, false) => O_RDONLY,
        (false, false) => O_ACCMODE,
    };
    let status = file.status();
    let append = if status.append { O_APPEND } else { 0 };
    let nonblock = if status.nonblock { O_NONBLOCK } else { 0 };
    mode | append | nonblock
}

/// `SEEK_*` as the VFS names it.
fn whence(raw: u32) -> Result<Whence, Errno> {
    match raw {
        SEEK_SET => Ok(Whence::Set),
        SEEK_CUR => Ok(Whence::Current),
        SEEK_END => Ok(Whence::End),
        SEEK_DATA => Ok(Whence::Data),
        SEEK_HOLE => Ok(Whence::Hole),
        _ => Err(Errno::EINVAL),
    }
}

/// `lseek`.
///
/// The offset is a native word: 64 bits on the 64-bit architectures, a signed
/// 32-bit `off_t` on ARMv7-A, whose wide seeks go through `_llseek`. A result
/// the return register cannot carry as a non-negative value is `EOVERFLOW`,
/// after the position has moved -- which is Linux's order too.
pub(crate) fn sys_lseek(process: &Process, fd: i32, offset: i64, raw: u32) -> Result<usize, Errno> {
    let file = file(process, fd)?;
    let at = file.seek(offset, whence(raw)?)?;
    isize::try_from(at)
        .ok()
        .and_then(|at| usize::try_from(at).ok())
        .ok_or(Errno::EOVERFLOW)
}

/// `_llseek`, ARMv7-A's 64-bit seek: the offset in two words, the result
/// through a pointer, because a 32-bit return register cannot hold it.
pub(crate) fn sys_llseek(
    process: &Process,
    fd: i32,
    high: u64,
    low: u64,
    result: u64,
    raw: u32,
) -> Result<usize, Errno> {
    let file = file(process, fd)?;
    let offset = (u64::from(high as u32) << 32 | u64::from(low as u32)) as i64;
    let at = file.seek(offset, whence(raw)?)?;
    uaccess::copy_to_user(process.space(), result, &at.to_le_bytes()).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `ftruncate` and `ftruncate64`.
///
/// A negative length is refused before the descriptor is looked at, as on
/// Linux.
pub(crate) fn sys_ftruncate(process: &Process, fd: i32, length: i64) -> Result<usize, Errno> {
    let length = u64::try_from(length).map_err(|_| Errno::EINVAL)?;
    let file = file(process, fd)?;
    file.set_len(length)?;
    fs::inotify::node_event(file.location(), fs::inotify::IN_MODIFY);
    Ok(0)
}

/// `ioctl(fd, FIONBIO, &on)`: `O_NONBLOCK` set when the `int` at `arg` is
/// not zero and cleared when it is, as `ioctl_fionbio` does.
///
/// # Errors
///
/// `EFAULT` for an unreadable `arg`.
fn fionbio(process: &Process, file: &OpenFile, arg: u64) -> Result<usize, Errno> {
    let on = uaccess::get_u32(process.space(), arg)?;
    let mut status = file.status();
    status.nonblock = on != 0;
    file.set_status(status);
    Ok(0)
}

/// `ioctl`: `EBADF` for a closed descriptor, `FIONBIO`, `FIOCLEX` and
/// `FIONCLEX` for any file, the
/// terminal requests for the console, and `ENOTTY` for every other file,
/// which is what Linux answers for a descriptor that is not a terminal. See
/// `crate::syscall::tty`.
pub(crate) fn sys_ioctl(
    process: &Process,
    fd: i32,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    let file = file(process, fd)?;
    // Answered before the file is asked, as `do_vfs_ioctl` answers them, so a
    // pipe, a socket and a terminal all take them. `FIOCLEX` and `FIONCLEX`
    // read no argument: they change the descriptor, as `F_SETFD` does.
    match request {
        FIONBIO => return fionbio(process, &file, arg),
        FIOCLEX => return sys_fcntl(process, fd, F_SETFD, u64::from(FD_CLOEXEC)),
        FIONCLEX => return sys_fcntl(process, fd, F_SETFD, 0),
        _ => {}
    }
    // A namespace file (`fs/nsfs.rs`) answers its four requests and no other.
    if let Some(namespace) = fs::nsfs::of(&file) {
        return fs::nsfs::ioctl(process, &namespace, request, arg);
    }
    // By what reads and writes reach, not by what `fstat` reports: `/dev/tty`
    // is a devfs node of its own that opens the console, and busybox's shell
    // asks its job-control questions through it.
    if Arc::ptr_eq(file.io(), &console::console_inode()) {
        return tty::ioctl(process, &file, request, arg);
    }
    // An open card (`docs/DISPLAY.md` §2.3), by its per-open object.
    if let Some(card) = crate::interfaces::display::drm::of(file.io()) {
        return crate::interfaces::display::drm::ioctl(process, &card, request, arg);
    }
    // An open render node (`docs/GPU.md` §3.3), by its per-open object. A
    // card and a render node are different files with different ioctls, so
    // neither answers the other's.
    if let Some(node) = crate::interfaces::render::node::of(file.io()) {
        return crate::interfaces::render::node::ioctl(process, &node, request, arg);
    }
    // An open input device (`docs/INPUT.md` §3.3), by its per-open object.
    if let Some(device) = crate::interfaces::input::evdev::of(file.io()) {
        return crate::interfaces::input::evdev::ioctl(process, &device, request, arg);
    }
    // A sound card's playback and control nodes (`docs/AUDIO.md` §3.4). A
    // drain and a write wait unless the descriptor is non-blocking, which
    // an ioctl knows only from the file's status.
    if let Some(pcm) = crate::interfaces::audio::pcm::pcm_of(file.io()) {
        let nonblock = file.status().nonblock;
        return crate::interfaces::audio::pcm::pcm_ioctl(process, &pcm, request, arg, nonblock);
    }
    if let Some(control) = crate::interfaces::audio::pcm::control_of(file.io()) {
        return crate::interfaces::audio::pcm::control_ioctl(process, &control, request, arg);
    }
    // A node a ring-3 driver serves through the chardev core
    // (`docs/NVIDIA.md` §4.4): forwarded to it undecoded.
    if let Some(chardev) = crate::interfaces::chardev::file::of(file.io()) {
        return crate::interfaces::chardev::file::ioctl(&chardev, request, arg);
    }
    // An open disk (`docs/INSTALLER.md` §5.1): its geometry and a flush.
    if let Some(disk) = fs::disk_file::of(file.io()) {
        return fs::disk_file::ioctl(process, &disk, request, arg);
    }
    // A pseudoterminal, by which end of it the descriptor holds.
    if let Some(master) = fs::pty::master_of(file.io()) {
        return tty::master_ioctl(process, &master, request, arg);
    }
    if let Some(slave) = fs::pty::slave_of(file.io()) {
        return tty::slave_ioctl(process, &slave, request, arg);
    }
    // An inotify instance answers what is queued to read.
    if let Some(instance) = fs::inotify::of(&file) {
        return match request {
            ferrix_linux_abi::types::FIONREAD => {
                let queued = u32::try_from(instance.queued_bytes()).unwrap_or(u32::MAX);
                uaccess::copy_to_user(process.space(), arg, &queued.to_ne_bytes())
                    .map_err(|_| Errno::EFAULT)?;
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        };
    }
    // The two socket requests, which ask a socket what is queued each way.
    let answered = if let Some(socket) = fs::socket::of(&file) {
        socket.ioctl(process, request, arg)
    } else if let Some(socket) = crate::net::socket::of(&file) {
        socket.ioctl(process, request, arg)
    } else if let Some(socket) = crate::net::packet::of(&file) {
        socket.ioctl(process, request, arg)
    } else {
        return Err(Errno::ENOTTY);
    };
    match answered {
        // Every socket answers the interface requests, whatever its family:
        // Linux's `sock_ioctl` passes what the family did not know to
        // `dev_ioctl`, and a program relies on that. musl's
        // `if_nametoindex` -- which is how `ip` turns a name into an index,
        // and which POSIX.1-2024 specifies -- asks over an `AF_UNIX` socket.
        Err(Errno::ENOTTY) => crate::net::ifreq::ioctl(process, request, arg),
        answer => answer,
    }
}
