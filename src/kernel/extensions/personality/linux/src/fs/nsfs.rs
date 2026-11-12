//! nsfs: where a namespace is a file, so that a program can keep one in a
//! descriptor, hand it on and join it with `setns` (`docs/NAMESPACES.md` §12).
//!
//! Linux's `nsfs`. Following a magic link of `/proc/<pid>/ns` -- `open`, or
//! `openat` of it -- leads to an inode here, one per namespace and kind; the
//! inode holds the namespace strongly, so a descriptor keeps it alive after
//! every process in it has gone. Its number is the namespace's own
//! (`/proc/<pid>/ns/<kind>` reads `<kind>:[N]`, and `fstat` of the open file
//! says `st_ino` N), so two opens of one namespace are one `st_ino` and one
//! `st_dev`. Nothing is named in a tree: the location is detached, as a pipe's
//! is, and `readlink("/proc/self/fd/N")` reads the name it was opened with.
//!
//! The file has no `read` or `write`. Its requests are [`NS_GET_USERNS`],
//! [`NS_GET_PARENT`], [`NS_GET_NSTYPE`] and [`NS_GET_OWNER_UID`], whose
//! numbers are `_IO(0xb7, 1..=4)` of `linux/nsfs.h`.
//!
//! What a descriptor costs: the namespace at its making, charged to the job
//! that made it, and the inode, the detached mount and the open file, charged
//! to the job that opens (F-37).

use alloc::sync::Arc;
use core::any::Any;

use ferrix_kmem::Charge;
use ferrix_sync::Once;
use ferrix_vfs::path::NAME_MAX;
use ferrix_vfs::{
    Errno, FileSystem, FileType, Inode, Location, Metadata, Namespace, OpenFile, OpenFlags, StatFs,
    Timespec,
};

use crate::fs;
use crate::syscall::credentials;
use crate::syscall::namespace::{CLONE_NEWNS, CLONE_NEWUSER};
use crate::syscall::nsproxy::{CLONE_NEWCGROUP, CLONE_NEWIPC, CLONE_NEWUTS, CgroupNamespace};
use crate::syscall::process::Process;
use crate::syscall::sem::IpcNamespace;
use crate::syscall::system::UtsNamespace;
use crate::syscall::uaccess;
use crate::syscall::userns::{self, Kind as IdKind, UserNamespace};

/// `NSFS_MAGIC`.
pub(crate) const NSFS_MAGIC: u64 = 0x6e73_6673;

/// `NS_GET_USERNS`: a descriptor for the user namespace that owns this one.
pub(crate) const NS_GET_USERNS: u32 = 0xb701;
/// `NS_GET_PARENT`: a descriptor for the parent of a hierarchical namespace.
pub(crate) const NS_GET_PARENT: u32 = 0xb702;
/// `NS_GET_NSTYPE`: the `CLONE_NEW*` flag of its kind.
pub(crate) const NS_GET_NSTYPE: u32 = 0xb703;
/// `NS_GET_OWNER_UID`: the owner of a user namespace.
pub(crate) const NS_GET_OWNER_UID: u32 = 0xb704;

/// The block size `stat` reports: a page.
const BLOCK_SIZE: u32 = 4096;

/// The five kinds of namespace there are to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `mnt`.
    Mount,
    /// `user`.
    User,
    /// `uts`.
    Uts,
    /// `ipc`.
    Ipc,
    /// `cgroup`.
    Cgroup,
}

/// A namespace, of any kind, held.
#[derive(Debug, Clone)]
pub(crate) enum Handle {
    /// A mount namespace.
    Mount(Arc<Namespace>),
    /// A user namespace.
    User(Arc<UserNamespace>),
    /// A UTS namespace.
    Uts(Arc<UtsNamespace>),
    /// An IPC namespace.
    Ipc(Arc<IpcNamespace>),
    /// A cgroup namespace.
    Cgroup(Arc<CgroupNamespace>),
}

impl Handle {
    /// The namespace of `kind` that `process` is in.
    pub(crate) fn of(process: &Process, kind: Kind) -> Handle {
        match kind {
            Kind::Mount => Handle::Mount(fs::namespace_of(&process.fs_context().lock())),
            Kind::User => Handle::User(process.with_credentials(|held| Arc::clone(&held.user_ns))),
            Kind::Uts => Handle::Uts(process.nsproxy().uts),
            Kind::Ipc => Handle::Ipc(process.nsproxy().ipc),
            Kind::Cgroup => Handle::Cgroup(process.nsproxy().cgroup),
        }
    }

    /// What `/proc/<pid>/ns/<kind>` and `st_ino` call it.
    pub(crate) fn id(&self) -> u64 {
        match self {
            Handle::Mount(ns) => ns.id(),
            Handle::User(ns) => ns.id(),
            Handle::Uts(ns) => ns.id(),
            Handle::Ipc(ns) => ns.id(),
            Handle::Cgroup(ns) => ns.id(),
        }
    }

    /// Its kind's `CLONE_NEW*` flag.
    pub(crate) fn nstype(&self) -> u64 {
        match self {
            Handle::Mount(_) => CLONE_NEWNS,
            Handle::User(_) => CLONE_NEWUSER,
            Handle::Uts(_) => CLONE_NEWUTS,
            Handle::Ipc(_) => CLONE_NEWIPC,
            Handle::Cgroup(_) => CLONE_NEWCGROUP,
        }
    }

    /// Its kind's name in `/proc/<pid>/ns`.
    fn label(&self) -> &'static str {
        match self {
            Handle::Mount(_) => "mnt",
            Handle::User(_) => "user",
            Handle::Uts(_) => "uts",
            Handle::Ipc(_) => "ipc",
            Handle::Cgroup(_) => "cgroup",
        }
    }

    /// The user namespace that owns it: for a user namespace, its parent, and
    /// `None` for the first. Linux's `ns->ops->owner`.
    pub(crate) fn owner(&self) -> Option<Arc<UserNamespace>> {
        match self {
            Handle::Mount(ns) => Some(fs::owner_of(ns)),
            Handle::User(ns) => ns.parent().map(Arc::clone),
            Handle::Uts(ns) => Some(Arc::clone(ns.owner())),
            Handle::Ipc(ns) => Some(Arc::clone(ns.owner())),
            Handle::Cgroup(ns) => Some(Arc::clone(ns.owner())),
        }
    }
}

/// The filesystem.
#[derive(Debug)]
struct NsFs {
    device: u64,
    root: Arc<dyn Inode>,
}

/// The one nsfs, made on first use.
fn nsfs() -> &'static Arc<NsFs> {
    static NSFS: Once<Arc<NsFs>> = Once::new();
    NSFS.call_once(|| {
        Arc::new(NsFs {
            device: fs::anonymous_device(),
            root: Arc::new(NsRoot),
        })
    })
}

impl FileSystem for NsFs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::clone(&self.root)
    }

    fn name(&self) -> &'static str {
        "nsfs"
    }

    fn device(&self) -> u64 {
        self.device
    }

    fn statfs(&self) -> StatFs {
        StatFs {
            magic: NSFS_MAGIC,
            block_size: u64::from(BLOCK_SIZE),
            name_max: NAME_MAX as u64,
            ..StatFs::default()
        }
    }
}

/// What `stat` reports for a namespace file: a regular file, mode 0444, root's,
/// numbered by the namespace.
fn metadata(ino: u64, kind: FileType) -> Metadata {
    Metadata {
        ino,
        kind,
        permissions: if kind == FileType::Directory {
            0o555
        } else {
            0o444
        },
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        rdev: 0,
        blocks: 0,
        block_size: BLOCK_SIZE,
        atime: Timespec::default(),
        mtime: Timespec::default(),
        ctime: Timespec::default(),
    }
}

/// The filesystem's root, which nothing can reach.
#[derive(Debug)]
struct NsRoot;

impl Inode for NsRoot {
    fn metadata(&self) -> Metadata {
        metadata(1, FileType::Directory)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// One namespace as a file.
#[derive(Debug)]
struct NsInode {
    /// The namespace, held for as long as the file is.
    handle: Handle,
    /// The kernel heap this is, charged to the job that opened it.
    _charge: Charge,
}

impl Inode for NsInode {
    fn metadata(&self) -> Metadata {
        metadata(self.handle.id(), FileType::Regular)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// The location that following a `/proc/<pid>/ns` link leads to: `handle` as a
/// file, named `<kind>:[N]`.
///
/// # Errors
///
/// `ENOMEM` past the job's memory.
pub(crate) fn location(handle: Handle) -> Result<Location, Errno> {
    let charge = Charge::arc::<NsInode>().map_err(|_| Errno::ENOMEM)?;
    let name = alloc::format!("{}:[{}]", handle.label(), handle.id());
    let inode = crate::fallible::try_arc(NsInode {
        handle,
        _charge: charge,
    })
    .map_err(|_| Errno::ENOMEM)?;
    let parker = Arc::clone(fs::namespace().parker());
    Location::detached(
        Arc::clone(nsfs()) as Arc<dyn FileSystem>,
        inode,
        name.as_bytes(),
        parker,
    )
}

/// The namespace `file` is, if it is an nsfs file.
pub(crate) fn of(file: &OpenFile) -> Option<Handle> {
    Arc::clone(file.inode())
        .into_any()
        .downcast::<NsInode>()
        .ok()
        .map(|inode| inode.handle.clone())
}

/// An open file on `handle`, read-only.
fn open(handle: Handle) -> Result<Arc<OpenFile>, Errno> {
    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    OpenFile::new(location(handle)?, &flags)
}

/// Whether the calling process may open the namespaces of `target`, as a link
/// of `/proc/<target>/ns` leads: Linux asks `ptrace_may_access` of it, with the
/// filesystem ids, and so does [`credentials::may_access`].
pub(crate) fn may_open(target: &Process) -> bool {
    userns::acting().is_none_or(|caller| credentials::may_access(&caller, target, false))
}

/// A request to an nsfs file: Linux's `ns_ioctl`.
///
/// # Errors
///
/// `ENOTTY` for any other request; `EPERM` for an owner or parent that is not
/// in the caller's user namespace or beneath it, and for the first user
/// namespace's; `EINVAL` for `NS_GET_PARENT` of a namespace with no hierarchy
/// and `NS_GET_OWNER_UID` of one that is not a user namespace; `EFAULT`;
/// `EMFILE`.
pub(crate) fn ioctl(
    process: &Process,
    handle: &Handle,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    match request {
        NS_GET_USERNS => related(process, handle.owner()),
        NS_GET_PARENT => match handle {
            Handle::User(ns) => related(process, ns.parent().map(Arc::clone)),
            _ => Err(Errno::EINVAL),
        },
        NS_GET_NSTYPE => Ok(handle.nstype() as usize),
        NS_GET_OWNER_UID => {
            let Handle::User(ns) = handle else {
                return Err(Errno::EINVAL);
            };
            let shown = process.with_credentials(|held| {
                userns::from_kid_munged(&held.user_ns, IdKind::User, ns.owner_uid())
            });
            uaccess::copy_to_user(process.space(), arg, &shown.to_ne_bytes())
                .map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// A descriptor for the user namespace `owner`, if the caller's own is it or
/// an ancestor of it: a program learns of a namespace only from inside the one
/// that can see it (`ns_get_owner` of Linux).
fn related(process: &Process, owner: Option<Arc<UserNamespace>>) -> Result<usize, Errno> {
    let own = process.with_credentials(|held| Arc::clone(&held.user_ns));
    let mut at = owner.clone();
    loop {
        match at {
            None => return Err(Errno::EPERM),
            Some(ref found) if found.same(&own) => break,
            Some(found) => at = found.parent().map(Arc::clone),
        }
    }
    let Some(owner) = owner else {
        return Err(Errno::EPERM);
    };
    let file = open(Handle::User(owner))?;
    let fd = process.files().lock().insert(file, true)?;
    usize::try_from(fd).map_err(|_| Errno::EMFILE)
}
