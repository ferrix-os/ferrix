//! The empty filesystem at the bottom of every mount namespace.
//!
//! A namespace's root mount is this, read-only and empty, and the
//! filesystem a namespace is made with is mounted on top of it as `/`. That
//! is the shape every Linux machine has once it has booted: `/` is a mount
//! whose parent is a hidden mount under it, which `mountinfo` leaves out
//! because the reader's root cannot reach it (on a Linux 7.0 host, `/` is
//! mount 34 on parent 2, and there is no mount 2 to be read). It is what lets
//! `pivot_root` move `/` aside at all: Linux refuses a root mount with no
//! parent, and so it refuses a process whose `/` is still the initramfs.
//! Here every `/` has one from the start, in memory or on a disk
//! (`docs/NAMESPACES.md` §2.1).
//!
//! Nothing can be made in it: its mount is read-only, and its one directory
//! finds nothing and lists nothing. It is never unmounted, and nothing a
//! program reaches through `..` can climb into it past the root the program
//! was given.

use alloc::sync::Arc;
use core::any::Any;

use ferrix_linux_abi::errno::Errno;

use crate::Result;
use crate::node::{DirEntry, FileSystem, FileType, Inode, Metadata, Timespec};

/// The bottom filesystem: one empty directory.
#[derive(Debug, Default)]
pub(crate) struct Bottom;

impl FileSystem for Bottom {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(Empty)
    }

    /// Linux's name for the mount at the bottom of its tree.
    fn name(&self) -> &'static str {
        "rootfs"
    }

    /// No device: it holds nothing.
    fn device(&self) -> u64 {
        0
    }

    fn read_only(&self) -> bool {
        true
    }
}

/// [`Bottom`]'s one directory.
#[derive(Debug)]
struct Empty;

impl Inode for Empty {
    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 1,
            kind: FileType::Directory,
            permissions: 0o755,
            nlink: 2,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: 0,
            blocks: 0,
            block_size: 4096,
            atime: Timespec::default(),
            mtime: Timespec::default(),
            ctime: Timespec::default(),
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        let _ = name;
        Err(Errno::ENOENT)
    }

    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        let _ = (cursor, emit);
        Ok(())
    }
}
