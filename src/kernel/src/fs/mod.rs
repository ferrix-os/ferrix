//! The kernel's filesystem: one namespace, and what `src/lib/fs/vfs` needs from a
//! machine.
//!
//! Stage 8 of `docs/ROADMAP.md`. The VFS itself — dentries, mounts, the path
//! walk, tmpfs — is `src/lib/fs/vfs`, host-tested and fuzzed. What only the kernel
//! can supply is here: memory for file contents, a clock, device numbers, the
//! archive the loader handed over, and the one namespace every process
//! resolves paths in until stage 13 gives them namespaces of their own.
//!
//! # The root, and where it comes from
//!
//! A tmpfs, with the initramfs unpacked into it. That is Linux's answer to the
//! cycle `docs/ARCHITECTURE.md` §7 describes — the root filesystem needs a
//! block driver that lives on the root filesystem — and it is Ferrix's for
//! the same reason. `/tmp` is a tmpfs of its own on top, so that what a
//! program writes there is one mount that can later be bounded or discarded
//! without touching what the archive put in place.
//!
//! # Why the namespace cannot be missing
//!
//! [`namespace`] is total: if nothing has built the root yet, the first caller
//! gets an empty tmpfs, and [`init`] unpacks into that same one. A system call
//! that raced boot, or a self-check run in an unusual order, finds an empty
//! tree and gets `ENOENT` — a real answer — rather than a kernel that has to
//! decide what to do about a filesystem that does not exist.

pub(crate) mod anon;
pub(crate) mod bind_check;
pub(crate) mod block;
pub(crate) mod btrfs;
pub(crate) mod btrfs_check;
pub(crate) mod btrfs_powerfail;
pub(crate) mod btrfs_write_check;
pub(crate) mod cgroupfs;
pub(crate) mod check;
pub(crate) mod console;
pub(crate) mod data_disk;
pub(crate) mod devfs;
pub(crate) mod disk_file;
pub(crate) mod epoll;
pub(crate) mod epoll_check;
pub(crate) mod eventfd;
pub(crate) mod eventfd_check;
pub(crate) mod exec_check;
pub(crate) mod inotify;
pub(crate) mod kmem_check;
pub(crate) mod memfd_check;
pub(crate) mod mmap_check;
pub(crate) mod mount_check;
pub(crate) mod namespace_check;
pub(crate) mod netns_check;
pub(crate) mod nsfs;
mod pages;
pub(crate) mod partitions;
pub(crate) mod pidfd;
pub(crate) mod pidns_check;
pub(crate) mod pipe;
pub(crate) mod portfd;
pub(crate) mod procfs;
pub(crate) mod pty;
pub(crate) mod root_disk;
pub(crate) mod seam;
pub(crate) mod signalfd;
pub(crate) mod signalfd_check;
pub(crate) mod smallns_check;
pub(crate) mod socket;
pub(crate) mod sockname;
pub(crate) mod sysfs;
pub(crate) mod terminal;
pub(crate) mod timerfd;
pub(crate) mod timerfd_check;
pub(crate) mod userns_check;
pub(crate) mod wake;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_bootinfo::BootView;
use ferrix_sync::Once;
use ferrix_vfs::access::MAY_EXEC;
use ferrix_vfs::initramfs::{self, UnpackError, Unpacked, makedev};
use ferrix_vfs::tmpfs::Tmpfs;
use ferrix_vfs::{
    Clock, Context, Errno, FileType, Location, Namespace, OpenFile, OpenFlags, SetAttributes,
    Timespec,
};

use crate::hooks::Full;
use crate::mm;
use crate::power::Flush;
use crate::syscall::program::ProgramFile;
use crate::syscall::time;
use crate::vmap;

/// The first mount namespace: the kernel's own, and every process's until
/// an `unshare` or `clone` with `CLONE_NEWNS` gives it a copy
/// (`docs/NAMESPACES.md` §2.1).
static NAMESPACE: Once<Arc<Namespace>> = Once::new();

/// `/`'s commit before the machine stops: the root disk's last half-minute,
/// which its committer has not reached yet.
static ROOT_FLUSH: Flush = Flush {
    mount: "/",
    commit: root_disk::sync,
};

/// `/data`'s, which has no committer of its own.
static DATA_FLUSH: Flush = Flush {
    mount: "/data",
    commit: data_disk::sync,
};

/// Register what the certified item reaches the filesystem through, which it
/// does without naming it: the commits power makes before the machine stops,
/// `/` first, how `devmgr` reads its program and drivers, and the native
/// call that finds a job by its cgroupfs directory.
///
/// Called once from `main.rs`, before anything is mounted to commit and
/// before `devmgr` is started. A disk that is never mounted commits nothing,
/// so both flushes are registered on every machine.
///
/// # Errors
///
/// [`Full`] when power's list of flushes is, or the native call already has
/// a handler.
pub(crate) fn install() -> Result<(), Full> {
    crate::power::register_flush(&ROOT_FLUSH)?;
    crate::power::register_flush(&DATA_FLUSH)?;
    crate::discovery::devmgr::register_reader(read_from_root);
    cgroupfs::install()?;
    portfd::install()
}

/// Read the file at `path` in the root the initramfs was unpacked into.
fn read_from_root(path: &[u8]) -> Result<Vec<u8>, Errno> {
    read_file(&namespace().context(), None, path)
}

/// The next anonymous device minor, for filesystems with no device behind
/// them. Linux gives these major 0; so does Ferrix.
static NEXT_ANONYMOUS: AtomicU32 = AtomicU32::new(1);

/// Nanoseconds in a second.
const NANOS: u64 = 1_000_000_000;

/// The first namespace, built empty on first use if [`init`] has not run
/// yet: the one the kernel's own walks, `root_disk`, `devmgr` and the boot
/// checks use. A walk may go through it whatever namespace its context is
/// in, since crossing a mount point looks in the mount's own table; a change
/// to the tree goes through [`namespace_of`].
pub(crate) fn namespace() -> &'static Namespace {
    first_namespace()
}

/// The first namespace, shared.
pub(crate) fn first_namespace() -> &'static Arc<Namespace> {
    NAMESPACE.call_once(|| {
        Arc::new(Namespace::new(
            kernel_tmpfs(),
            Arc::new(crate::sync::SchedParker),
        ))
    })
}

/// The mount namespace `ctx` names: the one its mounts, unmounts and
/// `pivot_root` act on, and whose mounts `/proc/<pid>/mounts` lists.
pub(crate) fn namespace_of(ctx: &Context) -> Arc<Namespace> {
    ctx.ns
        .clone()
        .unwrap_or_else(|| Arc::clone(first_namespace()))
}

/// Whether `ctx` is in `ns`.
pub(crate) fn is_in(ctx: &Context, ns: &Namespace) -> bool {
    match &ctx.ns {
        Some(own) => core::ptr::eq(Arc::as_ptr(own), ns),
        None => core::ptr::eq(Arc::as_ptr(first_namespace()), ns),
    }
}

/// The user namespace that owns mount namespace `ns`: the one that was
/// current when it was copied, or the first for the kernel's own
/// (`docs/NAMESPACES.md` §12).
pub(crate) fn owner_of(ns: &Namespace) -> Arc<crate::syscall::userns::UserNamespace> {
    ns.owner()
        .and_then(|owner| {
            owner
                .downcast::<crate::syscall::userns::UserNamespace>()
                .ok()
        })
        .unwrap_or_else(|| Arc::clone(crate::syscall::userns::first()))
}

/// The initramfs as the loader handed it over, kept for the root disk to
/// install from.
static ARCHIVE: Once<&'static [u8]> = Once::new();

/// The initramfs archive, if the boot had one and [`init`] has run.
pub(crate) fn initramfs_archive() -> Option<&'static [u8]> {
    ARCHIVE.get().copied()
}

/// A device number no other filesystem has: `st_dev` for an in-memory one.
pub(crate) fn anonymous_device() -> u64 {
    makedev(0, NEXT_ANONYMOUS.fetch_add(1, Ordering::Relaxed))
}

/// The clock timestamps are taken from.
///
/// `CLOCK_REALTIME`: firmware's time at boot and the counter since, or the
/// counter since boot read as time since 1970 on a machine whose firmware
/// has no clock. The same clock `clock_gettime` answers, so a file written
/// now is dated now to every program that compares the two. It read the
/// counter alone once, and kept doing so after the real-time clock learned
/// firmware's time: every new file was dated 1970, older than any file a
/// tar archive unpacked, and automake's "newly created file is older than
/// distributed files" stopped curl's `configure` on Ferrix.
pub(crate) fn clock() -> Arc<dyn Clock> {
    Arc::new(RealtimeClock)
}

/// A new, empty tmpfs whose file contents are VMO pages, charged to the
/// running task's job: what `mount -t tmpfs` makes.
///
/// # Errors
///
/// `ENOMEM` past the job's memory limit.
pub(crate) fn new_tmpfs() -> Result<Arc<Tmpfs>, Errno> {
    Tmpfs::new(
        anonymous_device(),
        clock(),
        Arc::new(pages::VmoStorage),
        0o755,
    )
}

/// A new, empty tmpfs the kernel keeps for every job, charged to nobody: the
/// namespace's root, `/tmp` and `/dev/shm` as boot mounts them, the memfd
/// filesystem. Its files are charged to their makers.
pub(crate) fn kernel_tmpfs() -> Arc<Tmpfs> {
    Tmpfs::for_kernel(
        anonymous_device(),
        clock(),
        Arc::new(pages::VmoStorage),
        0o755,
    )
}

/// The most [`read_file`] will read: 64 MiB.
///
/// The whole file is built in kernel memory, and a caller naming a file large
/// enough to exhaust it in one call should get `EFBIG` rather than take the
/// kernel's memory with it. Programs are not read through here: `execve`
/// maps them from their files ([`open_program`]), whatever their size.
const READ_FILE_LIMIT: u64 = 64 * 1024 * 1024;

/// Read a whole regular file, resolving `path` from `start` -- or from the
/// context's working directory -- and following symbolic links.
///
/// What the kernel reads whole from a path: a native program `devmgr`
/// starts, a manifest, and the files the boot checks compare. One function
/// rather than a loop in each, so they cannot disagree about what a path
/// names or which files may be read whole.
///
/// A file that shrinks while it is read comes back as the bytes that were
/// there; one that grows comes back at the size it had when it was opened.
///
/// # Errors
///
/// What the path walk refuses; `EISDIR` for a directory and `EACCES` for any
/// other file that is not a regular one, which is what `execve` reports;
/// `EFBIG` past [`READ_FILE_LIMIT`]; `ENOMEM` if memory cannot hold it.
pub(crate) fn read_file(
    ctx: &Context,
    start: Option<&Location>,
    path: &[u8],
) -> Result<Vec<u8>, Errno> {
    let at = namespace().resolve(ctx, start, path, true)?;
    let buffer = read_location(at)?;
    let mut contents = Vec::new();
    contents
        .try_reserve_exact(buffer.len())
        .map_err(|_| Errno::ENOMEM)?;
    contents.extend_from_slice(&buffer);
    Ok(contents)
}

/// Read a whole regular file `name` beneath the directory `dir`, following
/// no symbolic link on the way from `dir` to it: `openat2`'s
/// `RESOLVE_NO_SYMLINKS`, which Ferrix has no call for.
///
/// The read for any name a user or a command line had a hand in: anything
/// the kernel reads whole from a name it did not choose itself should read
/// it through here, beneath the one directory it may come from, rather than
/// through [`read_file`], which follows links anywhere.
///
/// What the display core reads `drm.edid_firmware`'s file with. The name has
/// already been refused if it is absolute or climbs through `..`
/// (`ferrix_displayctl::edid::confined`), so a walk one component at a time
/// that never follows a link cannot leave `dir`: a link planted under
/// `/lib/firmware` that leads to `/etc/shadow` is `ELOOP`, and what every
/// program that opens the card can read stays a file that is there. `dir`
/// itself is resolved as any path is.
///
/// # Errors
///
/// `ELOOP` for a symbolic link anywhere after `dir`; `EINVAL` for an empty
/// or absolute name or a `..` component; otherwise [`read_file`]'s.
pub(crate) fn read_file_beneath(ctx: &Context, dir: &[u8], name: &[u8]) -> Result<Vec<u8>, Errno> {
    if name.first() == Some(&b'/') {
        return Err(Errno::EINVAL);
    }
    let ns = namespace();
    let mut at = ns.resolve(ctx, None, dir, true)?;
    let mut any = false;
    for component in name
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty())
    {
        if component == b".." {
            return Err(Errno::EINVAL);
        }
        at = ns.resolve(ctx, Some(&at), component, false)?;
        if ns.stat(&at)?.metadata.kind == FileType::Symlink {
            return Err(Errno::ELOOP);
        }
        any = true;
    }
    if !any {
        return Err(Errno::EINVAL);
    }
    let buffer = read_location(at)?;
    let mut contents = Vec::new();
    contents
        .try_reserve_exact(buffer.len())
        .map_err(|_| Errno::ENOMEM)?;
    contents.extend_from_slice(&buffer);
    Ok(contents)
}

/// Open a program: the file, with its headers read and nothing else, and the
/// absolute path of the file, symbolic links resolved.
///
/// Not [`read_file`]: a program is mapped from its file rather than read
/// whole (`syscall/load.rs`), so it may be any size, and costs its headers to
/// start.
///
/// The path is what `/proc/<pid>/exe` reports, and glibc's static startup
/// reads it back and asserts it is absolute. Taken from the location that was
/// opened rather than by resolving the string again, so the two cannot name
/// different files if the tree changes in between.
///
/// # Errors
///
/// What the path walk refuses; `EISDIR` for a directory and `EACCES` for any
/// other file that is not a regular one, and for a regular file the context
/// may not execute; and whatever opening it or reading its headers refuses.
pub(crate) fn open_program(
    ctx: &Context,
    start: Option<&Location>,
    path: &[u8],
) -> Result<(ProgramFile, Vec<u8>, SetIds), Errno> {
    let ns = namespace();
    let at = ns.resolve(ctx, start, path, true)?;
    let metadata = ns.stat(&at)?.metadata;
    match metadata.kind {
        FileType::Regular => ctx.who.require(&metadata, MAY_EXEC)?,
        FileType::Directory => return Err(Errno::EISDIR),
        _ => return Err(Errno::EACCES),
    }
    // Linux's `do_open_execat`: nothing on a `noexec` mount runs -- the
    // program, its `#!` interpreter, or its dynamic linker, all of which
    // come through here.
    if at.mount.no_exec() {
        return Err(Errno::EACCES);
    }
    let exe = ns.path_of(&at, &ctx.root);
    let set_ids = set_ids_on(&at, &metadata);
    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let file = ProgramFile::open(OpenFile::new(at, &flags)?)?;
    Ok((file, exe, set_ids))
}

/// The ids a program takes on when it runs: its owner where the file is
/// set-user-id, its group where it is set-group-id.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SetIds {
    /// The effective user id the program starts with, if any.
    pub(crate) uid: Option<u32>,
    /// The effective group id, if any.
    pub(crate) gid: Option<u32>,
}

impl SetIds {
    /// What an image from no file carries: nothing.
    pub(crate) const NONE: SetIds = SetIds {
        uid: None,
        gid: None,
    };
}

/// [`SetIds`] from a file's mode, as `bprm_fill_uid` reads it.
///
/// The set-group-id bit counts only where the group may execute; without that
/// it means mandatory locking, which is not a privilege to take on.
pub(crate) fn set_ids_of(metadata: &ferrix_vfs::Metadata) -> SetIds {
    SetIds {
        uid: (metadata.permissions & 0o4000 != 0).then_some(metadata.uid),
        gid: (metadata.permissions & 0o2010 == 0o2010).then_some(metadata.gid),
    }
}

/// [`set_ids_of`] for the file at `at`: nothing on a `nosuid` mount, as
/// Linux's `mnt_may_suid` has it.
pub(crate) fn set_ids_on(at: &Location, metadata: &ferrix_vfs::Metadata) -> SetIds {
    if at.mount.no_set_id() {
        SetIds::NONE
    } else {
        set_ids_of(metadata)
    }
}

/// The whole of the regular file at `at`: the half of [`read_file`] after the
/// walk, with every one of its refusals.
fn read_location(at: Location) -> Result<vmap::Buffer, Errno> {
    let metadata = namespace().stat(&at)?.metadata;
    match metadata.kind {
        FileType::Regular => {}
        FileType::Directory => return Err(Errno::EISDIR),
        _ => return Err(Errno::EACCES),
    }
    if metadata.size > READ_FILE_LIMIT {
        return Err(Errno::EFBIG);
    }
    let len = usize::try_from(metadata.size).map_err(|_| Errno::EFBIG)?;

    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let file = OpenFile::new(at, &flags)?;
    let mut contents = vmap::Buffer::zeroed(len).map_err(|_| Errno::ENOMEM)?;
    let mut done = 0;
    while done < len {
        let slot = contents.get_mut(done..).ok_or(Errno::EIO)?;
        let count = file.read(slot)?;
        if count == 0 {
            break;
        }
        done += count;
    }
    contents.truncate(done);
    Ok(contents)
}

/// See [`clock`].
#[derive(Debug)]
struct RealtimeClock;

impl Clock for RealtimeClock {
    fn now(&self) -> Timespec {
        let nanos = time::realtime_nanos();
        Timespec {
            tv_sec: i64::try_from(nanos / NANOS).unwrap_or(i64::MAX),
            tv_nsec: i64::try_from(nanos % NANOS).unwrap_or(0),
        }
    }
}

/// What building the root found.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// The archive's size, if the loader handed one over.
    pub(crate) initramfs_bytes: Option<u64>,
    /// What unpacking it made.
    pub(crate) unpacked: Option<Unpacked>,
}

/// Why the root could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InitError {
    /// The loader described an archive the direct map does not cover.
    OutsideDirectMap,
    /// The archive did not unpack.
    Unpack(UnpackError),
    /// `/tmp` could not be made or mounted.
    Tmp(Errno),
    /// A kernel filesystem could not be mounted on the directory named.
    Mount(&'static str, Errno),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitError::OutsideDirectMap => f.write_str("the initramfs is outside the direct map"),
            InitError::Unpack(why) => write!(f, "the initramfs did not unpack: {why}"),
            InitError::Tmp(errno) => write!(f, "/tmp could not be mounted: errno {}", errno.0),
            InitError::Mount(at, errno) => {
                write!(f, "{at} could not be mounted: errno {}", errno.0)
            }
        }
    }
}

/// Build the root: unpack the initramfs, if there is one, and mount `/tmp`
/// and the kernel's own filesystems.
///
/// Called once, from `kmain`, after the frame allocator and before anything
/// that opens a file.
///
/// # Errors
///
/// [`InitError`].
pub(crate) fn init(view: &BootView<'_>) -> Result<Report, InitError> {
    let ns = namespace();
    let ctx = ns.context();

    let mut report = Report {
        initramfs_bytes: None,
        unpacked: None,
    };
    if let Some((phys, len)) = view.initrd() {
        let archive = initrd(view, phys, len)?;
        let _ = ARCHIVE.call_once(|| archive);
        report.initramfs_bytes = Some(len);
        report.unpacked = Some(initramfs::unpack(ns, &ctx, archive).map_err(InitError::Unpack)?);
    }

    match ns.mkdir(&ctx, None, b"/tmp", 0o1777) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(errno) => return Err(InitError::Tmp(errno)),
    }
    let tmp = ns
        .resolve(&ctx, None, b"/tmp", true)
        .map_err(InitError::Tmp)?;
    let _ = ns.mount(kernel_tmpfs(), &tmp).map_err(InitError::Tmp)?;
    let mounted = ns
        .resolve(&ctx, None, b"/tmp", true)
        .map_err(InitError::Tmp)?;
    // Sticky and writable by everyone, as every Unix `/tmp` is: a program
    // that checks the mode before trusting the directory is right to.
    let sticky = SetAttributes {
        permissions: Some(0o1777),
        ..SetAttributes::default()
    };
    ns.set_attributes(&mounted, &sticky)
        .map_err(InitError::Tmp)?;

    devfs::mount().map_err(|errno| InitError::Mount("/dev", errno))?;
    procfs::mount().map_err(|errno| InitError::Mount("/proc", errno))?;
    sysfs::mount().map_err(|errno| InitError::Mount("/sys", errno))?;

    // `/dev/shm`, where POSIX shared memory and named semaphores live. devfs
    // carries the directory and nothing else; the files are in a tmpfs mounted
    // over it, as a Linux init script mounts one there. It is mounted here
    // rather than left to an init program because there is not always one: a
    // static busybox, a compositor started as init, and the kernel's own
    // checks all expect the directory to work.
    let shm = ns
        .resolve(&ctx, None, b"/dev/shm", true)
        .map_err(|errno| InitError::Mount("/dev/shm", errno))?;
    let _ = ns
        .mount(kernel_tmpfs(), &shm)
        .map_err(|errno| InitError::Mount("/dev/shm", errno))?;
    let mounted = ns
        .resolve(&ctx, None, b"/dev/shm", true)
        .map_err(|errno| InitError::Mount("/dev/shm", errno))?;
    ns.set_attributes(&mounted, &sticky)
        .map_err(|errno| InitError::Mount("/dev/shm", errno))?;
    Ok(report)
}

/// The archive, as bytes the kernel can read.
fn initrd(view: &BootView<'_>, phys: u64, len: u64) -> Result<&'static [u8], InitError> {
    let info = view.raw();
    let end = phys.checked_add(len).ok_or(InitError::OutsideDirectMap)?;
    let map_end = info
        .physmap_phys
        .checked_add(info.physmap_len)
        .ok_or(InitError::OutsideDirectMap)?;
    if phys < info.physmap_phys || end > map_end {
        return Err(InitError::OutsideDirectMap);
    }
    let len = usize::try_from(len).map_err(|_| InitError::OutsideDirectMap)?;
    // SAFETY: the loader read the archive into memory the map reports as
    // `Initrd`, which nothing reclaims and nothing writes after the hand-off,
    // and the check above puts all of it inside the direct map. So the bytes
    // stay valid and unaliased by a writer for the life of the system.
    Ok(unsafe { core::slice::from_raw_parts(mm::direct_map(phys) as *const u8, len) })
}
