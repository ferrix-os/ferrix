//! btrfs, mounted: the VFS's [`FileSystem`] and [`Inode`] over `ferrix-btrfs`.
//!
//! `ferrix-btrfs` reads a volume through a [`Device`] into buffers its caller
//! supplies, and allocates nothing. The VFS wants `Arc<dyn Inode>` objects that
//! answer on their own. This crate is the part in between, and what it adds is
//! ownership, sharing, and the translation into Linux's answers.
//!
//! # Read-only
//!
//! Stage 11 is btrfs stage A. Every operation that would change the volume
//! answers `EROFS`, which is what Linux says on a read-only mount, rather than
//! the `ENOTDIR` or `EINVAL` the trait's defaults give an object of the wrong
//! kind.
//!
//! # No lock is held across I/O
//!
//! The [`Inode`] contract forbids holding a spin lock across I/O, and a spin
//! lock is all there is. So nothing that does I/O sits behind one:
//!
//! * the device is a handle, cloned for each operation, so how concurrent reads
//!   meet is the device's business — in the kernel, the block core's queue;
//! * metadata read through it is kept in a bounded cache every handle shares
//!   (`cache`), whose lock is held to look an entry up or add one, never
//!   across the read that fills it;
//! * the [`Volume`] does not change after mount and is shared freely;
//! * working memory comes from a pool whose lock is held to take a set of
//!   buffers and to give it back, never while the buffers are in use. An
//!   operation that finds the pool empty allocates a fresh set, so two reads
//!   never wait on each other's memory.
//!
//! Each pooled set keeps its decompression cache. On a read-only volume an
//! extent's bytes never change, so a cached extent stays correct whichever
//! inode reads it next.
//!
//! # One inode object per inode
//!
//! An inode object is made from its `INODE_ITEM` when a lookup first finds it,
//! and its metadata is fixed from then on, which is right for a volume nothing
//! writes. The mount keeps a weak reference to every live one, keyed by inode
//! number, so a second name for the same file — a hard link, or `..` back to
//! a directory — finds the object that exists rather than a twin with a page
//! cache of its own. The VFS's dentry cache is what keeps an object alive
//! between lookups; when the last reference goes, the object takes its map
//! entry with it.
//!
//! # File data lives in the page cache
//!
//! A regular file's bytes are read through the [`Pages`] the mount's
//! [`Storage`] lends it, made over a [`PageSource`] that reads runs of pages
//! from the volume with no lock held: in the kernel, the inode's VMO, so that
//! a mapping of the file and a `read` of it see the same pages. The source
//! fills zeros past the file's end. A page whose data fails its checksum is an
//! error and never a zeroed page: the leading pages of a run that did verify
//! are kept, and the bad one answers `EIO` on its own, at every read.
//!
//! # What is not crossed
//!
//! A directory entry naming another subvolume is left out of listings and
//! reported absent by lookup, so the two always agree; entering subvolumes is
//! btrfs stage C. Device numbers are passed through as btrfs stores them:
//! nothing in the test images is a device node, and how that value maps to
//! `st_rdev` has not yet been checked against Linux.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::ops::ControlFlow;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::chunk::ChunkMapEntry;
use ferrix_btrfs::compress::MAX_UNCOMPRESSED;
use ferrix_btrfs::compress::zstd::Workspace;
use ferrix_btrfs::fs::{ExtentBuffers, ReadBuffers, Subvolume, Target};
use ferrix_btrfs::items::{self, InodeItem};
use ferrix_btrfs::tree::MAX_NODE_SIZE;
use ferrix_btrfs::volume::{Device, Volume};
use ferrix_sync::SpinLock;
use ferrix_vfs::tmpfs::{PAGE_SIZE, PageSource, Pages, Storage};
use ferrix_vfs::{
    DirEntry, Errno, FIRST_CURSOR, FileSystem, FileType, Inode, Metadata, NewNode, Result,
    SetAttributes, StatFs, Timespec,
};

mod cache;
pub mod rw;

use cache::{Cached, NodeCache};

/// How many chunks a mounted volume may have.
///
/// Data chunks are a gibibyte each, so this is a volume of several terabytes,
/// for 160 KiB of map.
pub const MAX_CHUNKS: usize = 4096;

/// How many idle buffer sets a mount keeps. Each is about 0.4 MiB, so the pool
/// covers a few concurrent readers without holding memory for many.
const POOLED: usize = 4;

/// The longest symlink target Linux will store: `PATH_MAX` less its NUL.
pub(crate) const MAX_LINK: u64 = 4095;

/// `f_type` for btrfs, which programs compare against to learn what they are
/// running on.
pub const BTRFS_SUPER_MAGIC: u64 = 0x9123_683E;

/// The longest name a btrfs directory entry may have.
pub(crate) const NAME_MAX: u64 = 255;

/// What a mount reads through: a [`Device`] handle that can be cloned for
/// each operation and shared across threads.
pub trait BlockHandle: Device + Clone + Send + Sync + 'static {}

impl<T: Device + Clone + Send + Sync + 'static> BlockHandle for T {}

// ---------------------------------------------------------------------------
// Working memory
// ---------------------------------------------------------------------------

/// The extent buffers one set owns.
struct Owned {
    compressed: Box<[u8]>,
    plain: Box<[u8]>,
    zstd: Box<[u8]>,
    /// The checksum tree's node buffer, apart from the operation's own.
    csum_node: Box<[u8]>,
}

impl ExtentBuffers for Owned {
    fn parts(&mut self) -> (&mut [u8], &mut [u8], &mut [u8], &mut [u8]) {
        (
            &mut self.compressed,
            &mut self.plain,
            &mut self.zstd,
            &mut self.csum_node,
        )
    }
}

impl fmt::Debug for Owned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Owned").finish_non_exhaustive()
    }
}

/// Everything one operation reads into: a node buffer and the extent buffers.
#[derive(Debug)]
struct Scratch {
    node: Box<[u8]>,
    read: ReadBuffers<Owned>,
}

impl Scratch {
    fn new() -> Result<Scratch> {
        let owned = Owned {
            compressed: zeroed(MAX_UNCOMPRESSED),
            plain: zeroed(MAX_UNCOMPRESSED),
            zstd: zeroed(Workspace::SIZE),
            csum_node: zeroed(MAX_NODE_SIZE as usize),
        };
        Ok(Scratch {
            node: zeroed(MAX_NODE_SIZE as usize),
            read: ReadBuffers::new(owned).map_err(errno)?,
        })
    }
}

/// A heap buffer of `len` zeroes, built on the heap rather than moved there.
fn zeroed(len: usize) -> Box<[u8]> {
    vec![0u8; len].into_boxed_slice()
}

// ---------------------------------------------------------------------------
// The mount
// ---------------------------------------------------------------------------

/// What every inode of one mount shares.
struct Shared<D> {
    volume: Volume<Box<[ChunkMapEntry]>>,
    device: Cached<D>,
    dev_no: u64,
    pool: SpinLock<Vec<Scratch>>,
    /// Where a regular file's page cache comes from.
    storage: Arc<dyn Storage>,
    /// Every live inode object, by inode number; see the crate documentation.
    /// Held to look one up or to record one, never across a read.
    nodes: SpinLock<BTreeMap<u64, Weak<Node<D>>>>,
}

impl<D: BlockHandle> Shared<D> {
    /// Run `op` with a device handle and a buffer set, and translate its error.
    fn with<R>(
        &self,
        op: impl FnOnce(
            &Subvolume<'_, Box<[ChunkMapEntry]>>,
            &mut Cached<D>,
            &mut Scratch,
        ) -> core::result::Result<R, BtrfsError>,
    ) -> Result<R> {
        let taken = self.pool.lock().pop();
        let mut scratch = match taken {
            Some(scratch) => scratch,
            None => Scratch::new()?,
        };
        let mut device = self.device.clone();
        let result = op(&self.volume.default_subvolume(), &mut device, &mut scratch);
        let mut pool = self.pool.lock();
        if pool.len() < POOLED {
            pool.push(scratch);
        }
        drop(pool);
        result.map_err(errno)
    }
}

impl<D> fmt::Debug for Shared<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("dev_no", &self.dev_no)
            .field("nodesize", &self.volume.nodesize())
            .finish_non_exhaustive()
    }
}

/// A mounted btrfs volume.
pub struct Btrfs<D> {
    shared: Arc<Shared<D>>,
    root: Arc<Node<D>>,
}

impl<D: BlockHandle> Btrfs<D> {
    /// Mount the volume on `device`, reporting `dev_no` as every inode's
    /// device, with file data kept in pages from `storage`.
    ///
    /// `EINVAL` when the device does not hold a btrfs volume this reader will
    /// read, and `EIO` when it does but cannot be read.
    pub fn mount(device: D, dev_no: u64, storage: Arc<dyn Storage>) -> Result<Arc<Btrfs<D>>> {
        Self::mount_with(device, dev_no, storage, cache::ENTRIES)
    }

    /// [`Btrfs::mount`] with a metadata cache of `entries` reads, so a test can
    /// make one small enough that every walk evicts.
    fn mount_with(
        device: D,
        dev_no: u64,
        storage: Arc<dyn Storage>,
        entries: usize,
    ) -> Result<Arc<Btrfs<D>>> {
        let device = Cached::new(device, Arc::new(NodeCache::new(entries)));
        let mut reader = device.clone();
        let chunks = vec![ChunkMapEntry::EMPTY; MAX_CHUNKS].into_boxed_slice();
        let mut scratch = Scratch::new()?;
        let volume = Volume::open(&mut reader, chunks, &mut scratch.node).map_err(mount_errno)?;
        let shared = Arc::new(Shared {
            volume,
            device,
            dev_no,
            pool: SpinLock::new(vec![scratch]),
            storage,
            nodes: SpinLock::new(BTreeMap::new()),
        });
        let root = Node::get(&shared, shared.volume.root_dir())?;
        if root.meta.kind != FileType::Directory {
            return Err(Errno::EIO);
        }
        Ok(Arc::new(Btrfs { shared, root }))
    }
}

impl<D> fmt::Debug for Btrfs<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Btrfs")
            .field("shared", &self.shared)
            .finish_non_exhaustive()
    }
}

impl<D: BlockHandle> FileSystem for Btrfs<D> {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::clone(&self.root) as Arc<dyn Inode>
    }

    fn name(&self) -> &'static str {
        "btrfs"
    }

    fn device(&self) -> u64 {
        self.shared.dev_no
    }

    /// The reader never writes: stage 11's mount, which only `MS_RDONLY`
    /// reaches.
    fn read_only(&self) -> bool {
        true
    }

    /// Sizes from the superblock, in sectors.
    ///
    /// Linux derives free space from each block group's space info, which
    /// would mean walking the extent tree at every `statfs`; total less used,
    /// as the superblock records them, is an approximation of that figure,
    /// and on a read-only mount nothing can spend it. btrfs has no inode
    /// table, so, as on Linux, the inode counts are zero.
    fn statfs(&self) -> StatFs {
        let volume = &self.shared.volume;
        let block_size = u64::from(volume.sectorsize());
        let free = volume.total_bytes().saturating_sub(volume.bytes_used()) / block_size;
        StatFs {
            magic: BTRFS_SUPER_MAGIC,
            block_size,
            blocks: volume.total_bytes() / block_size,
            blocks_free: free,
            blocks_available: free,
            files: 0,
            files_free: 0,
            name_max: NAME_MAX,
        }
    }
}

// ---------------------------------------------------------------------------
// Inodes
// ---------------------------------------------------------------------------

/// One file, directory or link of a mounted volume.
struct Node<D> {
    shared: Arc<Shared<D>>,
    meta: Metadata,
    /// A regular file's page cache, made at its first read; see the crate
    /// documentation. The lock is held to look or to set, never across a read.
    pages: SpinLock<Option<Arc<dyn Pages>>>,
}

impl<D: BlockHandle> Node<D> {
    /// The inode object for `ino`: the one alive already, or one built from
    /// its `INODE_ITEM` and recorded.
    ///
    /// The map's lock is not held across the load, which reads the volume; so
    /// two first lookups can both build one, and the second to record it
    /// keeps the first's and drops its own, whose `Drop` then finds another
    /// object's entry in the map and leaves it.
    fn get(shared: &Arc<Shared<D>>, ino: u64) -> Result<Arc<Node<D>>> {
        if let Some(alive) = shared.nodes.lock().get(&ino).and_then(Weak::upgrade) {
            return Ok(alive);
        }
        let built = Node::load(shared, ino)?;
        let mut nodes = shared.nodes.lock();
        if let Some(alive) = nodes.get(&ino).and_then(Weak::upgrade) {
            return Ok(alive);
        }
        let _ = nodes.insert(ino, Arc::downgrade(&built));
        Ok(built)
    }

    /// Build the inode object for `ino` from its `INODE_ITEM`.
    fn load(shared: &Arc<Shared<D>>, ino: u64) -> Result<Arc<Node<D>>> {
        let item = shared
            .with(|sub, device, scratch| sub.inode(device, ino, &mut scratch.node))?
            // A name that leads to no inode is a damaged tree, not a miss.
            .ok_or(Errno::EIO)?;
        let meta = metadata(ino, &item, shared.volume.sectorsize())?;
        Ok(Arc::new(Node {
            shared: Arc::clone(shared),
            meta,
            pages: SpinLock::new(None),
        }))
    }

    /// This regular file's page cache, made over a source at the first call.
    ///
    /// The store is made with the lock released, and set only if still
    /// absent, so two first readers cannot leave two caches; the loser's
    /// store is dropped unread.
    fn pages(&self) -> Result<Arc<dyn Pages>> {
        if let Some(pages) = self.pages.lock().as_ref() {
            return Ok(Arc::clone(pages));
        }
        let source: Arc<dyn PageSource> = Arc::new(FileSource {
            shared: Arc::clone(&self.shared),
            ino: self.meta.ino,
            size: self.meta.size,
        });
        let made: Arc<dyn Pages> = Arc::from(self.shared.storage.allocate_with(source)?);
        let mut slot = self.pages.lock();
        Ok(Arc::clone(slot.get_or_insert(made)))
    }

    fn require_dir(&self) -> Result<()> {
        if self.meta.kind == FileType::Directory {
            Ok(())
        } else {
            Err(Errno::ENOTDIR)
        }
    }

    /// The answer to any change asked of a directory: the name would be
    /// valid, but the volume is read-only.
    fn read_only<T>(&self) -> Result<T> {
        self.require_dir()?;
        Err(Errno::EROFS)
    }
}

impl<D> Drop for Node<D> {
    /// Take this object's entry out of the mount's map — and only this
    /// object's: a lookup that lost the race in [`Node::get`] drops a twin
    /// whose entry was never recorded, and must not remove the winner's.
    fn drop(&mut self) {
        let mut nodes = self.shared.nodes.lock();
        let mine = nodes
            .get(&self.meta.ino)
            .is_some_and(|weak| core::ptr::addr_eq(weak.as_ptr(), self));
        if mine {
            let _ = nodes.remove(&self.meta.ino);
        }
    }
}

impl<D> fmt::Debug for Node<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("ino", &self.meta.ino)
            .field("kind", &self.meta.kind)
            .finish_non_exhaustive()
    }
}

/// A regular file as a [`PageSource`]: what fills its page cache.
struct FileSource<D> {
    shared: Arc<Shared<D>>,
    ino: u64,
    /// The file's size when its inode was read, which on a read-only volume
    /// is its size for good.
    size: u64,
}

impl<D> fmt::Debug for FileSource<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSource")
            .field("ino", &self.ino)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl<D: BlockHandle> FileSource<D> {
    /// Fill `out` with the file's bytes from `offset`, zeros past its end.
    fn read_run(&self, offset: u64, out: &mut [u8]) -> Result<()> {
        let ino = self.ino;
        let got = self.shared.with(|sub, device, scratch| {
            sub.read(
                device,
                ino,
                offset,
                out,
                &mut scratch.node,
                &mut scratch.read,
            )
        })?;
        out.get_mut(got..).unwrap_or_default().fill(0);
        Ok(())
    }
}

impl<D: BlockHandle> PageSource for FileSource<D> {
    fn reads_disk(&self) -> bool {
        true
    }

    /// One read for the whole run, which is how a compressed extent is read
    /// anyway. If that read fails — a sector's checksum, the disk — the run
    /// is read again a page at a time, and the pages before the first failure
    /// are what is filled: cached, as they verified, while the failing page
    /// is asked for again on its own and answers the error itself.
    fn fill_range(&self, first: u64, pages: &mut [&mut [u8]]) -> Result<usize> {
        if pages.is_empty() {
            return Err(Errno::EINVAL);
        }
        let offset = first.checked_mul(PAGE_SIZE).ok_or(Errno::EIO)?;
        if offset >= self.size {
            for page in pages.iter_mut() {
                page.fill(0);
            }
            return Ok(pages.len());
        }
        let page = usize::try_from(PAGE_SIZE).map_err(|_| Errno::EIO)?;
        let mut run = vec![0u8; page.saturating_mul(pages.len())];
        if self.read_run(offset, &mut run).is_ok() {
            for (target, filled) in pages.iter_mut().zip(run.chunks_exact(page)) {
                target.copy_from_slice(filled);
            }
            return Ok(pages.len());
        }
        let mut filled = 0;
        for (i, target) in pages.iter_mut().enumerate() {
            let at = offset.saturating_add(PAGE_SIZE.saturating_mul(i as u64));
            match self.read_run(at, target) {
                Ok(()) => filled += 1,
                Err(error) if filled == 0 => return Err(error),
                Err(_) => break,
            }
        }
        Ok(filled)
    }
}

impl<D: BlockHandle> Inode for Node<D> {
    fn metadata(&self) -> Metadata {
        self.meta
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn set_attributes(&self, change: &SetAttributes) -> Result<()> {
        let _ = change;
        Err(Errno::EROFS)
    }

    /// Through the page cache, with no lock of this object held: the
    /// [`Pages`] may block on the volume, and so may a direct read.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if self.meta.kind != FileType::Regular {
            return Err(Errno::EINVAL);
        }
        let size = self.meta.size;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let len = usize::try_from(size - offset).map_or(buf.len(), |rest| rest.min(buf.len()));
        let out = buf.get_mut(..len).unwrap_or_default();
        self.pages()?.read(offset, out)?;
        Ok(len)
    }

    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64)> {
        let _ = (offset, data, append);
        Err(Errno::EROFS)
    }

    fn set_len(&self, len: u64) -> Result<()> {
        let _ = len;
        Err(Errno::EROFS)
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        self.require_dir()?;
        let dir = self.meta.ino;
        let found = self
            .shared
            .with(|sub, device, scratch| sub.lookup(device, dir, name, &mut scratch.node))?;
        match found.map(|entry| entry.target) {
            Some(Target::Inode(ino)) => Ok(Node::get(&self.shared, ino)? as Arc<dyn Inode>),
            Some(Target::Subvolume(_)) | None => Err(Errno::ENOENT),
        }
    }

    fn create(&self, name: &[u8], node: NewNode<'_>, permissions: u32) -> Result<Arc<dyn Inode>> {
        let _ = (name, node, permissions);
        self.read_only()
    }

    fn link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<()> {
        let _ = (name, target);
        self.read_only()
    }

    fn unlink(&self, name: &[u8]) -> Result<()> {
        let _ = name;
        self.read_only()
    }

    fn rmdir(&self, name: &[u8]) -> Result<()> {
        let _ = name;
        self.read_only()
    }

    fn rename(
        &self,
        old: &[u8],
        new_parent: &Arc<dyn Inode>,
        new: &[u8],
        replace: bool,
    ) -> Result<()> {
        let _ = (old, new_parent, new, replace);
        self.read_only()
    }

    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        self.require_dir()?;
        let dir = self.meta.ino;
        let mut damaged = false;
        self.shared.with(|sub, device, scratch| {
            let from = cursor.max(FIRST_CURSOR);
            sub.read_dir(device, dir, from, &mut scratch.node, |entry| {
                let Target::Inode(ino) = entry.target else {
                    return ControlFlow::Continue(());
                };
                let Some(kind) = entry_kind(entry.kind) else {
                    damaged = true;
                    return ControlFlow::Break(());
                };
                let listed = DirEntry {
                    ino,
                    kind,
                    name: entry.name,
                    next: entry.index.saturating_add(1),
                };
                if emit(listed) {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                }
            })
        })?;
        if damaged { Err(Errno::EIO) } else { Ok(()) }
    }

    fn read_link(&self) -> Result<Vec<u8>> {
        if self.meta.kind != FileType::Symlink {
            return Err(Errno::EINVAL);
        }
        if self.meta.size == 0 || self.meta.size > MAX_LINK {
            return Err(Errno::EIO);
        }
        let len = usize::try_from(self.meta.size).map_err(|_| Errno::EIO)?;
        let mut target = vec![0u8; len];
        let ino = self.meta.ino;
        let read = self.shared.with(|sub, device, scratch| {
            sub.read(
                device,
                ino,
                0,
                &mut target,
                &mut scratch.node,
                &mut scratch.read,
            )
        })?;
        if read == len {
            Ok(target)
        } else {
            Err(Errno::EIO)
        }
    }
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// What `stat` reports for an `INODE_ITEM`.
pub(crate) fn metadata(ino: u64, item: &InodeItem, block_size: u32) -> Result<Metadata> {
    let kind = FileType::from_mode(item.mode).ok_or(Errno::EIO)?;
    Ok(Metadata {
        ino,
        kind,
        permissions: item.mode & 0o7777,
        nlink: item.nlink,
        uid: item.uid,
        gid: item.gid,
        size: item.size,
        rdev: item.rdev,
        blocks: item.nbytes / 512,
        block_size,
        atime: time(item.atime),
        mtime: time(item.mtime),
        ctime: time(item.ctime),
    })
}

/// A btrfs timestamp as Linux reports it. The kernel reads the seconds as
/// signed, so the bits are reinterpreted rather than range-checked.
fn time(stamp: items::Timespec) -> Timespec {
    Timespec {
        tv_sec: i64::from_ne_bytes(stamp.sec.to_ne_bytes()),
        tv_nsec: i64::from(stamp.nsec.min(999_999_999)),
    }
}

/// The file type a directory entry records.
pub(crate) fn entry_kind(kind: u8) -> Option<FileType> {
    match kind {
        items::FT_REG_FILE => Some(FileType::Regular),
        items::FT_DIR => Some(FileType::Directory),
        items::FT_SYMLINK => Some(FileType::Symlink),
        items::FT_CHRDEV => Some(FileType::CharDevice),
        items::FT_BLKDEV => Some(FileType::BlockDevice),
        items::FT_FIFO => Some(FileType::Fifo),
        items::FT_SOCK => Some(FileType::Socket),
        _ => None,
    }
}

/// The error an operation on a mounted volume reports. Anything wrong with the
/// bytes, or with reading them, is `EIO`: by now the volume was accepted, so a
/// bad node is damage, not a question of what the device holds. A device that
/// ran out of memory is `ENOMEM`, which says nothing about the volume.
const fn errno(error: BtrfsError) -> Errno {
    match error {
        BtrfsError::OutOfMemory => Errno::ENOMEM,
        _ => Errno::EIO,
    }
}

/// The error a mount reports: `EINVAL` for a device that is not a volume this
/// reader accepts, as Linux's `mount` does, `ENOMEM` when memory ran out, and
/// `EIO` for one that is but cannot be read.
pub(crate) const fn mount_errno(error: BtrfsError) -> Errno {
    match error {
        BtrfsError::BadMagic
        | BtrfsError::UnsupportedChecksum(_)
        | BtrfsError::UnsupportedFeature(_)
        | BtrfsError::MultipleDevices(_)
        | BtrfsError::UnreplayedLog
        | BtrfsError::UnsupportedProfile(_)
        | BtrfsError::BadSectorSize(_)
        | BtrfsError::BadNodeSize(_) => Errno::EINVAL,
        BtrfsError::OutOfMemory => Errno::ENOMEM,
        _ => Errno::EIO,
    }
}

#[cfg(test)]
mod tests;
