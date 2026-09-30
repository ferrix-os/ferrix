//! A network namespace as a file, for `IFLA_NET_NS_FD` and `setns`
//! (`docs/NETNS.md` section 8, "what stays as it is").
//!
//! Following `/proc/<pid>/ns/net` leads to an inode here, one per opening,
//! that holds the namespace strongly: a descriptor keeps it alive after every
//! process in it has gone, as Linux's nsfs does. The branch that owns `setns`
//! (`stage13-smallns`, `fs/nsfs.rs`) has the general version of this, with
//! every kind of namespace as one `Handle`; when they meet, this file is a
//! `Handle::Net` arm there and goes. Until then it is the smallest thing that
//! makes a descriptor of a network namespace, and [`of`] is the one question
//! `IFLA_NET_NS_FD` and a future `setns(CLONE_NEWNET)` ask of it.

use alloc::sync::Arc;
use core::any::Any;

use ferrix_kmem::Charge;
use ferrix_sync::Once;
use ferrix_vfs::path::NAME_MAX;
use ferrix_vfs::{
    Errno, FileSystem, FileType, Inode, Location, Metadata, OpenFile, StatFs, Timespec,
};

use super::NetNamespace;
use crate::fs;

/// `NSFS_MAGIC`.
const NSFS_MAGIC: u64 = 0x6e73_6673;

/// The block size `stat` reports: a page.
const BLOCK_SIZE: u32 = 4096;

/// The filesystem.
#[derive(Debug)]
struct NsFs {
    device: u64,
    root: Arc<dyn Inode>,
}

/// The one filesystem, made on first use.
fn filesystem() -> &'static Arc<NsFs> {
    static NSFS: Once<Arc<NsFs>> = Once::new();
    NSFS.call_once(|| {
        Arc::new(NsFs {
            device: fs::anonymous_device(),
            root: Arc::new(Root),
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

/// What `stat` reports for a namespace file: mode 0444, root's, numbered by
/// the namespace.
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
struct Root;

impl Inode for Root {
    fn metadata(&self) -> Metadata {
        metadata(1, FileType::Directory)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// One network namespace as a file.
#[derive(Debug)]
struct NetInode {
    /// The namespace, held for as long as the file is.
    namespace: Arc<NetNamespace>,
    /// The kernel heap this is, charged to the job that opened it.
    _charge: Charge,
}

impl Inode for NetInode {
    fn metadata(&self) -> Metadata {
        metadata(self.namespace.id(), FileType::Regular)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// The location that following `/proc/<pid>/ns/net` leads to: `namespace` as
/// a file, named `net:[N]`.
///
/// # Errors
///
/// `ENOMEM` past the job's memory.
pub(crate) fn location(namespace: Arc<NetNamespace>) -> Result<Location, Errno> {
    let charge = Charge::arc::<NetInode>().map_err(|_| Errno::ENOMEM)?;
    let name = alloc::format!("net:[{}]", namespace.id());
    let inode = crate::fallible::try_arc(NetInode {
        namespace,
        _charge: charge,
    })
    .map_err(|_| Errno::ENOMEM)?;
    let parker = Arc::clone(fs::namespace().parker());
    Location::detached(
        Arc::clone(filesystem()) as Arc<dyn FileSystem>,
        inode,
        name.as_bytes(),
        parker,
    )
}

/// The network namespace `file` is, if it is one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<NetNamespace>> {
    Arc::clone(file.inode())
        .into_any()
        .downcast::<NetInode>()
        .ok()
        .map(|inode| Arc::clone(&inode.namespace))
}

/// Whether the caller may open the network namespace of `target`, as a link
/// of `/proc/<target>/ns` leads: Linux asks `ptrace_may_access` of it. The
/// caller's real, effective and saved ids must all be the target's effective
/// ones as kernel ids -- the same person -- or the caller must be privileged.
pub(crate) fn may_open(target: &crate::syscall::process::Process) -> bool {
    let Some(caller) = crate::syscall::userns::acting() else {
        return true;
    };
    if core::ptr::eq(Arc::as_ptr(&caller), target) {
        return true;
    }
    let (uid, gid) = target.with_credentials(|held| (held.user.effective, held.group.effective));
    caller.with_credentials(|held| {
        held.privileged()
            || (held.user.real == uid
                && held.user.effective == uid
                && held.user.saved == uid
                && held.group.real == gid
                && held.group.effective == gid
                && held.group.saved == gid)
    })
}
