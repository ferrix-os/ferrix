//! The anonymous inode filesystem: where an object that is a file only
//! because a program holds a descriptor to it lives -- an epoll set, and next
//! an eventfd.
//!
//! Linux's `anon_inodefs`. Such an object has no name in any tree, so its
//! location is detached, as a pipe's is on pipefs; `/proc/self/fd` shows the
//! name it is opened with, `anon_inode:[eventpoll]`, and `fstatfs` reports
//! `ANON_INODE_FS_MAGIC`. Linux gives every such object the one inode, so
//! `fstat` reports one inode number for all of them, a regular file's type and
//! mode 0600 owned by root, which is what it reports since 6.15.

use alloc::sync::Arc;
use core::any::Any;

use ferrix_sync::Once;
use ferrix_vfs::path::NAME_MAX;
use ferrix_vfs::{
    Errno, FileSystem, FileType, Inode, Location, Metadata, OpenFile, OpenFlags, StatFs, Timespec,
};

use crate::fs;

/// `ANON_INODE_FS_MAGIC`.
pub(crate) const ANON_INODE_FS_MAGIC: u64 = 0x0904_1934;

/// The one inode number every anonymous object reports.
const ANONYMOUS_INO: u64 = 2;

/// The block size `stat` reports: a page.
const BLOCK_SIZE: u32 = 4096;

/// The filesystem.
#[derive(Debug)]
struct AnonFs {
    device: u64,
    root: Arc<dyn Inode>,
}

/// The one `anon_inodefs`.
static ANON_FS: Once<Arc<AnonFs>> = Once::new();

/// The one `anon_inodefs`, made on first use.
fn anon_fs() -> &'static Arc<AnonFs> {
    ANON_FS.call_once(|| {
        Arc::new(AnonFs {
            device: fs::anonymous_device(),
            root: Arc::new(AnonRoot),
        })
    })
}

impl FileSystem for AnonFs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::clone(&self.root)
    }

    fn name(&self) -> &'static str {
        "anon_inodefs"
    }

    fn device(&self) -> u64 {
        self.device
    }

    fn statfs(&self) -> StatFs {
        StatFs {
            magic: ANON_INODE_FS_MAGIC,
            block_size: u64::from(BLOCK_SIZE),
            name_max: NAME_MAX as u64,
            ..StatFs::default()
        }
    }
}

/// The filesystem's root, which nothing can reach.
#[derive(Debug)]
struct AnonRoot;

impl Inode for AnonRoot {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 1,
            kind: FileType::Directory,
            permissions: 0o700,
            ..metadata()
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// What `stat` reports for every anonymous object.
pub(crate) fn metadata() -> Metadata {
    Metadata {
        ino: ANONYMOUS_INO,
        kind: FileType::Regular,
        permissions: 0o600,
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

/// Whether `file` is one of these objects: an eventfd, a timerfd, a
/// signalfd, an epoll, a sync file or an exported GPU buffer, none of which
/// Linux can splice from.
pub(crate) fn holds(file: &OpenFile) -> bool {
    file.location().mount.filesystem().name() == anon_fs().name()
}

/// An open file on `inode`, read and write, named `name` for `/proc/self/fd`.
///
/// # Errors
///
/// Whatever [`OpenFile::new`] refuses, which for an anonymous object is
/// nothing.
pub(crate) fn open(
    inode: Arc<dyn Inode>,
    name: &[u8],
    nonblock: bool,
) -> Result<Arc<OpenFile>, Errno> {
    open_mode(inode, name, nonblock, true)
}

/// An open file on `inode`, readable, and writable only if `write`.
///
/// # Errors
///
/// As [`open`].
pub(crate) fn open_mode(
    inode: Arc<dyn Inode>,
    name: &[u8],
    nonblock: bool,
    write: bool,
) -> Result<Arc<OpenFile>, Errno> {
    let flags = OpenFlags {
        read: true,
        write,
        nonblock,
        ..OpenFlags::default()
    };
    let anon: Arc<AnonFs> = Arc::clone(anon_fs());
    let parker = Arc::clone(fs::namespace().parker());
    OpenFile::new(Location::detached(anon, inode, name, parker)?, &flags)
}
