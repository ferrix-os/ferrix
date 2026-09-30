//! tmpfs: a filesystem that is only memory.
//!
//! The root filesystem at boot, `/tmp`, and what an initramfs is unpacked
//! into. It is the first filesystem Ferrix has, and it is written against the
//! same [`Inode`] contract btrfs will be.
//!
//! # Where file contents live
//!
//! Not in a `Vec<u8>`. A regular file's bytes are held by a [`Pages`] object
//! the kernel supplies, which in the kernel is a VMO: a sparse list of frames
//! committed on first write. That is what `docs/ARCHITECTURE.md` means by the
//! page cache being unified with VMOs, and it is what will let `mmap` of a
//! tmpfs file map the file's own pages rather than a copy. A byte vector would
//! have worked today and been unpicked the day `MAP_SHARED` met a file.
//!
//! [`HeapStorage`] is the same interface over heap pages, for the host tests
//! and the fuzzer.
//!
//! The same [`Pages`] is the page cache of a filesystem on a disk: a store
//! made over a [`PageSource`] fills a page from the file the first time it is
//! needed. tmpfs itself never asks for one, because it has nowhere else for a
//! page to come from.
//!
//! # Locking
//!
//! One spin lock per inode, holding everything about it. An operation that
//! needs more than one — `link`, `unlink`, `rmdir`, `rename` — takes them in
//! ascending inode number, after looking the names up under the directory
//! lock alone and then re-checking them once everything is held. That order
//! is the only rule, and it is what keeps `rename` of a directory over a
//! sibling from deadlocking against `rmdir` inside it.
//!
//! Outside all of them is one rename lock per instance, Linux's
//! `s_vfs_rename_mutex`. Only a rename moves a directory, so while it is held
//! every directory's parent stays put, and a rename can climb from its
//! destination to the root — one inode lock at a time, before it takes the
//! ones it needs — to refuse moving a directory into its own subtree. The VFS
//! checks that too, on dentries. This check is on the filesystem's own
//! structure, so a stale dentry or a second namespace cannot get a directory
//! loop past it, and a loop is not an error that can be undone: the
//! directories in it are unreachable and never freed.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kmem::{Charge, arc_footprint, footprint};
use ferrix_linux_abi::errno::Errno;

use crate::Result;
use crate::SpinLock;
use crate::node::{
    Clock, DirEntry, FIRST_CURSOR, FileSystem, FileType, Inode, Metadata, NewNode, SetAttributes,
    StatFs, Timespec,
};
use crate::path::PATH_MAX;

/// The block size tmpfs reports.
pub const BLOCK_SIZE: u32 = 4096;

/// The size of a page in every [`Pages`] store, and of each page a
/// [`PageSource`] fills.
pub const PAGE_SIZE: u64 = 4096;

/// [`PAGE_SIZE`] as a length.
const PAGE_BYTES: usize = PAGE_SIZE as usize;

/// The most pages [`HeapPages`] asks its source for in one call.
pub(crate) const MAX_FILL_RUN: usize = 32;

/// `TMPFS_MAGIC`, from `include/uapi/linux/magic.h`.
pub const TMPFS_MAGIC: u64 = 0x0102_1994;

/// What Linux's tmpfs counts each directory entry as, for a directory's size.
const DIRENT_SIZE: u64 = 20;

/// A regular file's contents: a sparse array of bytes, and the file's page
/// cache.
///
/// The file's length is tmpfs's to keep; this only holds bytes. Reading a
/// range nothing was written to yields zeros, or, for a store made over a
/// [`PageSource`], what the source fills it with.
pub trait Pages: Send + Sync + fmt::Debug {
    /// Fill `buf` from `offset`.
    ///
    /// A store over a source asks it for the pages it does not hold, and so
    /// may block: whoever calls this must hold no spin lock across it.
    ///
    /// # Errors
    ///
    /// `EIO` if the store cannot produce a page it holds, and whatever the
    /// source answers for one it does not.
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Store `data` at `offset`.
    ///
    /// A store over a source fills a page from it before a write that covers
    /// only part of the page, so the page's other bytes stay the file's.
    ///
    /// # Errors
    ///
    /// `ENOSPC` or `ENOMEM` if memory ran out, and what the source answers,
    /// with nothing stored past the point of failure being relied on.
    fn write(&self, offset: u64, data: &[u8]) -> Result<()>;

    /// Forget everything from `offset` on, so that it reads as zeros if the
    /// file grows again: whole pages are released and the tail of a partial
    /// one is cleared. For a store over a source, the source's bytes from
    /// `offset` on are forgotten too.
    fn discard_from(&self, offset: u64);

    /// Memory actually committed, in bytes: the pages held, and not what a
    /// source could fill.
    fn committed_bytes(&self) -> u64;

    /// The file is now `len` bytes long.
    ///
    /// Called under the inode's lock whenever the length changes: after a
    /// write or a grow has extended the file, and before
    /// [`Pages::discard_from`] when it is cut. A store whose pages can be
    /// mapped bounds a mapping's faults by it, so a page wholly past the end
    /// is refused rather than committed, as Linux answers such a fault with
    /// `SIGBUS`. Lowered before the discard, a fault racing a truncation sees
    /// the new length before the pages go. The default ignores it, which is
    /// right for a store nothing maps.
    fn resize(&self, len: u64) {
        let _ = len;
    }

    /// The object a mapping of the file maps: in the kernel, the file's VMO.
    ///
    /// `None`, the default, for a store nothing can map, which the heap store
    /// is.
    fn object(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }

    /// What shared mappings may have written, which marked no page -- every
    /// page the store holds, once a mapping that may write has been made
    /// since this was last asked -- and whether [`Pages::object`] is still
    /// mapped, or about to be, so that the file must stay in memory. A store
    /// over a disk writes the pages back as it does what `write` stored. The
    /// default, for a store nothing maps, is no page and no.
    fn mapped_writes(&self) -> (Vec<u64>, bool) {
        (Vec::new(), false)
    }
}

/// Where a file's pages come from when the cache does not have them.
///
/// The page cache is the inode's [`Pages`], on tmpfs and btrfs alike, and a
/// file-backed `mmap` and a `read` share its pages. A store made by
/// [`Storage::allocate_with`] asks its source for a page the first time
/// something reads it, or writes part of it, and keeps what it is given.
///
/// # Ranges, not pages
///
/// A compressed extent decompresses whole, so a source asked for one page of
/// it has done the work for all of its pages. [`PageSource::fill_range`]
/// lets it hand them all over, and lets it stop early at an extent boundary.
///
/// # Reclaim
///
/// Nothing evicts a cached page yet. When eviction arrives, a frame
/// allocation made inside a fill must not wait on writeback through the same
/// queue the fill is waiting on, or a fill that needs memory waits on itself.
pub trait PageSource: Send + Sync + fmt::Debug {
    /// Fill `pages` — each exactly [`PAGE_SIZE`] bytes — with the file's pages
    /// starting at page `first`. Returns how many leading pages were filled,
    /// at least 1 on `Ok`; may fill fewer than asked (btrfs stops at an extent
    /// boundary, since a compressed extent decompresses whole). Bytes past the
    /// file's end are zeros. Called with no spin lock held; may block on I/O.
    /// A checksum failure is `Err(EIO)`, never a zeroed page.
    ///
    /// # Errors
    ///
    /// `EIO` for pages that cannot be read or do not verify.
    fn fill_range(&self, first: u64, pages: &mut [&mut [u8]]) -> Result<usize>;

    /// One page. Provided over [`PageSource::fill_range`].
    ///
    /// # Errors
    ///
    /// As `fill_range`, and `EIO` for a source that says it filled anything
    /// but the one page.
    fn fill(&self, index: u64, page: &mut [u8]) -> Result<()> {
        match self.fill_range(index, &mut [page])? {
            1 => Ok(()),
            _ => Err(Errno::EIO),
        }
    }

    /// Whether a fill reads a disk: what the kernel counts as a page brought
    /// across the seam to a ring-3 driver, apart from a source that makes its
    /// pages up (a self-check's). `false` unless a source says so.
    fn reads_disk(&self) -> bool {
        false
    }

    /// Whether the pages this source filled can be dropped from memory and
    /// filled again, as they were: the source is the file's only copy and
    /// nothing in memory is newer. A read-only mount says so; a source whose
    /// file can be written, whose newer bytes live in the page cache until a
    /// writeback, does not. `false` unless a source says so; reclaim takes
    /// nothing from a file it cannot rely on (`docs/CGROUPS.md` §10).
    fn reclaimable(&self) -> bool {
        false
    }
}

/// Where new files get their [`Pages`].
pub trait Storage: Send + Sync + fmt::Debug {
    /// A store for a new, empty file.
    ///
    /// # Errors
    ///
    /// `ENOMEM`.
    fn allocate(&self) -> Result<Box<dyn Pages>>;

    /// A store for a file whose pages come from `source` until something
    /// writes them.
    ///
    /// # Errors
    ///
    /// `ENODEV` by default: a store that cannot yet fill from a source.
    /// `ENOMEM` from one that can.
    fn allocate_with(&self, source: Arc<dyn PageSource>) -> Result<Box<dyn Pages>> {
        let _ = source;
        Err(Errno::ENODEV)
    }

    /// The largest a file may grow, which is `EFBIG` past.
    fn max_file_size(&self) -> u64;

    /// Pages in total and pages free, for `statfs`. Unknown by default, which
    /// `df` shows as a filesystem of no size rather than an invented one.
    fn capacity(&self) -> (u64, u64) {
        (0, 0)
    }
}

/// [`Storage`] on the heap.
#[derive(Debug, Clone, Copy)]
pub struct HeapStorage {
    max_file_size: u64,
}

impl HeapStorage {
    /// Heap storage whose files may grow to `max_file_size` bytes.
    #[must_use]
    pub const fn new(max_file_size: u64) -> HeapStorage {
        HeapStorage { max_file_size }
    }
}

impl Storage for HeapStorage {
    fn allocate(&self) -> Result<Box<dyn Pages>> {
        Ok(Box::new(HeapPages::default()))
    }

    fn allocate_with(&self, source: Arc<dyn PageSource>) -> Result<Box<dyn Pages>> {
        Ok(Box::new(HeapPages::with_source(source)))
    }

    fn max_file_size(&self) -> u64 {
        self.max_file_size
    }
}

/// [`Pages`] on the heap, a page at a time, and optionally over a
/// [`PageSource`].
///
/// # Filling
///
/// A read that reaches a page it does not hold asks the source for a run of
/// consecutive missing pages from that one, at most 32 and no further than the
/// read goes. The buffers are allocated first and the source
/// is called with this store's lock released; afterwards only the pages still
/// missing are kept, so a page that a racing fill or a write put in meanwhile
/// wins. A fill that fails, claims no page, or claims more than it was asked
/// for keeps nothing and is `EIO` (or the source's own error).
///
/// The store's own lock is never held across the source, but a caller that
/// holds a spin lock across [`Pages::read`] still breaks the source's promise.
/// tmpfs does hold its inode lock across it, which is why tmpfs never asks for
/// a store over a source.
///
/// # Truncation
///
/// A source knows the file as it was, not as it has been cut. So the store
/// keeps a bound, `sourced_below`: the source is asked only for pages that
/// start below it, and bytes of a filled page at or past it are cleared before
/// the page is kept. It starts past every offset, and [`Pages::discard_from`]
/// lowers it and nothing raises it, so a file truncated and grown again reads
/// zeros past the cut, whenever the page is next filled. The bound is checked
/// when a fill is kept, under the lock, so a fill that raced a truncation is
/// cut back too.
#[derive(Debug, Default)]
pub struct HeapPages {
    heap: SpinLock<Heap>,
    source: Option<Arc<dyn PageSource>>,
}

#[derive(Debug)]
struct Heap {
    pages: BTreeMap<u64, Box<[u8]>>,
    /// Bytes from here on are not the source's; see [`HeapPages`].
    sourced_below: u64,
}

impl Default for Heap {
    fn default() -> Heap {
        Heap {
            pages: BTreeMap::new(),
            sourced_below: u64::MAX,
        }
    }
}

/// The page-sized piece of `[offset, offset + len)` that begins `done` bytes
/// in: its page index, the offset within the page, and its length.
fn piece(offset: u64, done: usize, len: usize) -> Result<(u64, usize, usize)> {
    let at = offset.checked_add(done as u64).ok_or(Errno::EFBIG)?;
    let within = usize::try_from(at % PAGE_SIZE).map_err(|_| Errno::EIO)?;
    Ok((
        at / PAGE_SIZE,
        within,
        (PAGE_BYTES - within).min(len - done),
    ))
}

fn zeroed_page() -> Box<[u8]> {
    alloc::vec![0_u8; PAGE_BYTES].into_boxed_slice()
}

impl Heap {
    /// Whether page `index`, if not held, would be the source's to fill.
    fn sourced(&self, index: u64) -> bool {
        index
            .checked_mul(PAGE_SIZE)
            .is_some_and(|start| start < self.sourced_below)
    }

    /// How many pages to ask for from `first`, a page that is missing and
    /// sourced: the run of such pages up to `last`, at most [`MAX_FILL_RUN`].
    fn run(&self, first: u64, last: u64) -> usize {
        let mut count = 1;
        while count < MAX_FILL_RUN {
            let Some(index) = first.checked_add(count as u64) else {
                break;
            };
            if index > last || self.pages.contains_key(&index) || !self.sourced(index) {
                break;
            }
            count += 1;
        }
        count
    }

    /// Copy what is held into `buf` from `done` on, advancing `done`, until a
    /// page the source should fill: then the run to ask for.
    fn read_held(
        &self,
        offset: u64,
        buf: &mut [u8],
        done: &mut usize,
        sourced: bool,
    ) -> Result<Option<(u64, usize)>> {
        let len = buf.len();
        while *done < len {
            let (index, within, take) = piece(offset, *done, len)?;
            let out = buf.get_mut(*done..*done + take).ok_or(Errno::EIO)?;
            match self.pages.get(&index) {
                Some(page) => {
                    out.copy_from_slice(page.get(within..within + take).ok_or(Errno::EIO)?);
                }
                None if sourced && self.sourced(index) => {
                    let (last, _, _) = piece(offset, len - 1, len)?;
                    return Ok(Some((index, self.run(index, last))));
                }
                None => out.fill(0),
            }
            *done += take;
        }
        Ok(None)
    }

    /// Store `data` from `done` on, advancing `done`, until a page that is
    /// missing, sourced and only partly written: then that page's index.
    fn write_held(
        &mut self,
        offset: u64,
        data: &[u8],
        done: &mut usize,
        sourced: bool,
    ) -> Result<Option<u64>> {
        let len = data.len();
        while *done < len {
            let (index, within, take) = piece(offset, *done, len)?;
            if !self.pages.contains_key(&index) {
                if sourced && take < PAGE_BYTES && self.sourced(index) {
                    return Ok(Some(index));
                }
                let _ = self.pages.insert(index, zeroed_page());
            }
            let page = self.pages.get_mut(&index).ok_or(Errno::EIO)?;
            let slot = page.get_mut(within..within + take).ok_or(Errno::EIO)?;
            slot.copy_from_slice(data.get(*done..*done + take).ok_or(Errno::EIO)?);
            *done += take;
        }
        Ok(None)
    }

    /// Keep each page a fill from `first` returned that is still missing and
    /// still sourced, with its bytes at or past the bound cleared.
    fn keep_filled(&mut self, first: u64, filled: Vec<Box<[u8]>>) {
        for (index, mut page) in (first..).zip(filled) {
            if self.pages.contains_key(&index) || !self.sourced(index) {
                continue;
            }
            let start = index.saturating_mul(PAGE_SIZE);
            if let Ok(valid) = usize::try_from(self.sourced_below - start)
                && let Some(past) = page.get_mut(valid..)
            {
                past.fill(0);
            }
            let _ = self.pages.insert(index, page);
        }
    }
}

impl HeapPages {
    /// An empty store whose pages come from `source` until written.
    #[must_use]
    pub fn with_source(source: Arc<dyn PageSource>) -> HeapPages {
        HeapPages {
            heap: SpinLock::new(Heap::default()),
            source: Some(source),
        }
    }

    /// Ask the source for `count` pages from `first`, into buffers allocated
    /// before the call, with no lock held. Answers the pages it filled.
    fn fetch(&self, first: u64, count: usize) -> Result<Vec<Box<[u8]>>> {
        let source = self.source.as_deref().ok_or(Errno::EIO)?;
        let mut filled: Vec<Box<[u8]>> = (0..count).map(|_| zeroed_page()).collect();
        let got = {
            let mut pages: Vec<&mut [u8]> = filled.iter_mut().map(|page| &mut **page).collect();
            source.fill_range(first, &mut pages)?
        };
        if got == 0 || got > count {
            return Err(Errno::EIO);
        }
        filled.truncate(got);
        Ok(filled)
    }
}

impl Pages for HeapPages {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let sourced = self.source.is_some();
        let mut done = 0;
        loop {
            let missing = self
                .heap
                .lock()
                .read_held(offset, buf, &mut done, sourced)?;
            let Some((first, count)) = missing else {
                return Ok(());
            };
            let filled = self.fetch(first, count)?;
            self.heap.lock().keep_filled(first, filled);
        }
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        let sourced = self.source.is_some();
        let mut done = 0;
        loop {
            let missing = self
                .heap
                .lock()
                .write_held(offset, data, &mut done, sourced)?;
            let Some(index) = missing else {
                return Ok(());
            };
            let filled = self.fetch(index, 1)?;
            self.heap.lock().keep_filled(index, filled);
        }
    }

    fn discard_from(&self, offset: u64) {
        let released = {
            let mut heap = self.heap.lock();
            heap.sourced_below = heap.sourced_below.min(offset);
            let released = heap.pages.split_off(&offset.div_ceil(PAGE_SIZE));
            let within = (offset % PAGE_SIZE) as usize;
            if within != 0
                && let Some(page) = heap.pages.get_mut(&(offset / PAGE_SIZE))
                && let Some(tail) = page.get_mut(within..)
            {
                tail.fill(0);
            }
            released
        };
        drop(released);
    }

    fn committed_bytes(&self) -> u64 {
        (self.heap.lock().pages.len() as u64).saturating_mul(PAGE_SIZE)
    }
}

/// What every inode of one tmpfs instance shares.
#[derive(Debug)]
struct Shared {
    device: u64,
    clock: Arc<dyn Clock>,
    storage: Arc<dyn Storage>,
    next_ino: AtomicU64,
    /// Held across every rename; see the module documentation.
    renames: SpinLock<()>,
}

/// One tmpfs instance.
#[derive(Debug)]
pub struct Tmpfs {
    shared: Arc<Shared>,
    root: Arc<Node>,
}

/// `F_SEAL_SEAL`: no more seals may be added.
pub const SEAL_SEAL: u32 = 0x0001;
/// `F_SEAL_SHRINK`: the file may not shrink.
pub const SEAL_SHRINK: u32 = 0x0002;
/// `F_SEAL_GROW`: the file may not grow.
pub const SEAL_GROW: u32 = 0x0004;
/// `F_SEAL_WRITE`: the contents may not change.
pub const SEAL_WRITE: u32 = 0x0008;
/// `F_SEAL_FUTURE_WRITE`: no new write may start.
pub const SEAL_FUTURE_WRITE: u32 = 0x0010;
/// Every seal tmpfs knows.
const SEALS_KNOWN: u32 = SEAL_SEAL | SEAL_SHRINK | SEAL_GROW | SEAL_WRITE | SEAL_FUTURE_WRITE;

impl Tmpfs {
    /// A regular file on this tmpfs that no directory names: what
    /// `memfd_create` makes. With `sealable`, it starts with no seals and may
    /// be sealed; without, it carries [`SEAL_SEAL`], as every other file does.
    ///
    /// # Errors
    ///
    /// What the storage refuses when it allocates the file's pages.
    pub fn new_unlinked_file(&self, permissions: u32, sealable: bool) -> Result<Arc<dyn Inode>> {
        let now = self.shared.clock.now();
        let body = Body::File {
            pages: self.shared.storage.allocate()?,
            len: 0,
        };
        let node = Node::new(&self.shared, body, permissions, now)?;
        {
            let mut state = node.state.lock();
            state.nlink = 0;
            if sealable {
                state.seals = 0;
            }
        }
        Ok(node as Arc<dyn Inode>)
    }

    /// An empty tmpfs whose root directory has `permissions`, charged to
    /// the running task's job -- the instance and its root -- as a mount
    /// makes one.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    pub fn new(
        device: u64,
        clock: Arc<dyn Clock>,
        storage: Arc<dyn Storage>,
        permissions: u32,
    ) -> Result<Arc<Tmpfs>> {
        let charge = crate::charge(
            arc_footprint::<Node>()
                .saturating_add(arc_footprint::<Shared>())
                .saturating_add(arc_footprint::<Tmpfs>()),
        )?;
        let shared = Arc::new(Shared {
            device,
            clock,
            storage,
            next_ino: AtomicU64::new(1),
            renames: SpinLock::new(()),
        });
        let now = shared.clock.now();
        let root = Node::with_charge(
            &shared,
            Body::Dir(Dir::new(Weak::new())),
            permissions,
            now,
            charge,
        );
        Ok(Arc::new(Tmpfs { shared, root }))
    }

    /// An empty tmpfs as [`Tmpfs::new`] makes one, charged to nobody: for an
    /// instance the kernel keeps for every job -- the namespace's root,
    /// `/tmp` and `/dev/shm` as boot mounts them, the one every memfd lives
    /// on -- whoever happens to cause it first. Its files are charged to
    /// their makers as any tmpfs's are.
    #[must_use]
    pub fn for_kernel(
        device: u64,
        clock: Arc<dyn Clock>,
        storage: Arc<dyn Storage>,
        permissions: u32,
    ) -> Arc<Tmpfs> {
        let shared = Arc::new(Shared {
            device,
            clock,
            storage,
            next_ino: AtomicU64::new(1),
            renames: SpinLock::new(()),
        });
        let now = shared.clock.now();
        let root = Node::with_charge(
            &shared,
            Body::Dir(Dir::new(Weak::new())),
            permissions,
            now,
            Charge::none(),
        );
        Arc::new(Tmpfs { shared, root })
    }
}

impl FileSystem for Tmpfs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::clone(&self.root) as Arc<dyn Inode>
    }

    fn name(&self) -> &'static str {
        "tmpfs"
    }

    fn device(&self) -> u64 {
        self.shared.device
    }

    fn statfs(&self) -> StatFs {
        let (blocks, free) = self.shared.storage.capacity();
        let files = self
            .shared
            .next_ino
            .load(Ordering::Relaxed)
            .saturating_sub(1);
        StatFs {
            magic: TMPFS_MAGIC,
            block_size: u64::from(BLOCK_SIZE),
            blocks,
            blocks_free: free,
            blocks_available: free,
            files,
            files_free: 0,
            name_max: crate::path::NAME_MAX as u64,
        }
    }
}

/// One tmpfs inode.
pub struct Node {
    ino: u64,
    /// Itself, for a directory created in it to name as its parent.
    me: Weak<Node>,
    shared: Arc<Shared>,
    /// A plain ticket lock, unlike the crate's others, whose holder may be
    /// switched out: a shrinking `set_len` cuts the file's mappings under it,
    /// which shoots down other processors' TLBs, and that is not asked with a
    /// preemption-disabling lock held. It is held because it is what
    /// serialises the cut against `write_at`, which `discard_from` relies on.
    /// Keeping the holder here needs that serialisation from a lock a
    /// shrink may sleep under instead.
    state: ferrix_sync::SpinLock<State>,
    /// The kernel heap the inode holds -- itself, and a symbolic link's
    /// target -- charged to the job that made it for as long as it exists,
    /// linked or open (F-37). Its pages are charged as they are written, to
    /// the writer's job, as frames; its names are charged by [`Entry`].
    _charge: Charge,
}

impl Drop for Node {
    /// Let go of what a directory holds without a frame per level.
    ///
    /// A directory holds each child's node, so dropping the last hold on one
    /// dropped its children inside its own drop, and theirs inside those: a
    /// chain 200 deep, torn down at `umount` or at power-off, ran off the end
    /// of the kernel's stack and double-faulted (ferrix-ea, 2026-09-26). Here
    /// a child this directory held last gives up its own entries before it
    /// goes, and those wait on a list, so nothing is dropped with children
    /// still in it. The list grows fallibly; a directory whose entries find
    /// no room there is dropped as before, which only a heap already
    /// exhausted brings about.
    fn drop(&mut self) {
        let Body::Dir(dir) = &mut self.state.get_mut().body else {
            return;
        };
        let mut next = core::mem::take(&mut dir.by_cursor);
        let mut waiting: Vec<BTreeMap<u64, Entry>> = Vec::new();
        loop {
            while let Some((_, entry)) = next.pop_first() {
                let Entry { node, .. } = entry;
                // `None` when someone else still holds it: nothing drops here.
                let Some(mut child) = Arc::into_inner(node) else {
                    continue;
                };
                let Body::Dir(dir) = &mut child.state.get_mut().body else {
                    continue;
                };
                let entries = core::mem::take(&mut dir.by_cursor);
                if !entries.is_empty() && waiting.try_reserve(1).is_ok() {
                    waiting.push(entries);
                }
                // `child` goes here with nothing left in it; entries that
                // found no room on the list went just before it, as before.
            }
            match waiting.pop() {
                Some(entries) => next = entries,
                None => return,
            }
        }
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("tmpfs::Node")
            .field("ino", &self.ino)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct State {
    /// `F_SEAL_*` bits. [`SEAL_SEAL`] for every file but a memfd made to
    /// allow sealing, as on Linux's shmem.
    seals: u32,
    permissions: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    body: Body,
}

impl State {
    fn touch(&mut self, now: Timespec) {
        self.mtime = now;
        self.ctime = now;
    }

    fn dir(&mut self) -> Result<&mut Dir> {
        match &mut self.body {
            Body::Dir(dir) => Ok(dir),
            _ => Err(Errno::ENOTDIR),
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.body, Body::Dir(_))
    }

    /// The directory holding this one; `None` for the root, for a removed
    /// directory, and for anything that is not a directory.
    fn parent(&self) -> Option<Arc<Node>> {
        match &self.body {
            Body::Dir(dir) => dir.parent.upgrade(),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Body {
    File { pages: Box<dyn Pages>, len: u64 },
    Dir(Dir),
    Symlink(Box<[u8]>),
    Special { kind: FileType, rdev: u64 },
}

/// A directory's names.
///
/// Indexed twice: by name for lookups, and by a cursor handed out in creation
/// order for `getdents64`. A cursor is never reused, so a program reading a
/// directory while another creates and removes names in it sees each
/// surviving entry exactly once — which a position-counting cursor cannot
/// promise, and which `rm -rf` depends on.
#[derive(Debug)]
struct Dir {
    by_name: BTreeMap<Box<[u8]>, u64>,
    by_cursor: BTreeMap<u64, Entry>,
    next_cursor: u64,
    /// Removed: nothing may be created in it again.
    dead: bool,
    /// The directory holding it. Weak, because the parent holds this one
    /// through its entry; changed only by a rename, under the rename lock.
    parent: Weak<Node>,
}

#[derive(Debug)]
struct Entry {
    name: Box<[u8]>,
    ino: u64,
    kind: FileType,
    node: Arc<Node>,
    /// The name's heap, charged to the job that made the name (F-37).
    _charge: Charge,
}

impl Entry {
    /// Charge the running task's job for a name `name` in a directory: the
    /// entry, the name twice (both maps key on it), and the other map's
    /// key. Made before anything is changed, so a refusal changes nothing.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    fn charge(name: &[u8]) -> Result<Charge> {
        let bytes = footprint(name.len(), 1)
            .saturating_mul(2)
            .saturating_add(size_of::<(u64, Entry)>())
            .saturating_add(size_of::<(Box<[u8]>, u64)>());
        crate::charge(bytes)
    }
}

impl Dir {
    fn new(parent: Weak<Node>) -> Dir {
        Dir {
            by_name: BTreeMap::new(),
            by_cursor: BTreeMap::new(),
            next_cursor: FIRST_CURSOR,
            dead: false,
            parent,
        }
    }

    fn get(&self, name: &[u8]) -> Option<&Entry> {
        self.by_name
            .get(name)
            .and_then(|cursor| self.by_cursor.get(cursor))
    }

    fn insert(
        &mut self,
        name: &[u8],
        node: Arc<Node>,
        kind: FileType,
        charge: Charge,
    ) -> Result<()> {
        let cursor = self.next_cursor;
        self.next_cursor = cursor.checked_add(1).ok_or(Errno::ENOSPC)?;
        let entry = Entry {
            name: Box::from(name),
            ino: node.ino,
            kind,
            node,
            _charge: charge,
        };
        let _ = self.by_name.insert(Box::from(name), cursor);
        let _ = self.by_cursor.insert(cursor, entry);
        Ok(())
    }

    fn remove(&mut self, name: &[u8]) -> Option<Entry> {
        let cursor = self.by_name.remove(name)?;
        self.by_cursor.remove(&cursor)
    }

    fn len(&self) -> usize {
        self.by_name.len()
    }

    fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Several inode locks, taken in ascending inode number.
struct Locked<'a> {
    guards: Vec<(u64, ferrix_sync::SpinLockGuard<'a, State>)>,
}

impl<'a> Locked<'a> {
    fn new(nodes: &[&'a Node]) -> Locked<'a> {
        let mut sorted: Vec<&'a Node> = nodes.to_vec();
        sorted.sort_by_key(|node| node.ino);
        sorted.dedup_by_key(|node| node.ino);
        Locked {
            guards: sorted
                .into_iter()
                .map(|node| (node.ino, node.state.lock()))
                .collect(),
        }
    }

    fn state(&mut self, ino: u64) -> Result<&mut State> {
        self.guards
            .iter_mut()
            .find(|(held, _)| *held == ino)
            .map(|(_, guard)| &mut **guard)
            .ok_or(Errno::EIO)
    }
}

impl Node {
    /// A new inode, charged to the running task's job.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past the job's memory limit.
    fn new(shared: &Arc<Shared>, body: Body, permissions: u32, now: Timespec) -> Result<Arc<Node>> {
        let extra = match &body {
            Body::Symlink(target) => footprint(target.len(), 1),
            _ => 0,
        };
        let charge = crate::charge(arc_footprint::<Node>().saturating_add(extra))?;
        Ok(Node::with_charge(shared, body, permissions, now, charge))
    }

    fn with_charge(
        shared: &Arc<Shared>,
        body: Body,
        permissions: u32,
        now: Timespec,
        charge: Charge,
    ) -> Arc<Node> {
        let nlink = if matches!(body, Body::Dir(_)) { 2 } else { 1 };
        Arc::new_cyclic(|me| Node {
            ino: shared.next_ino.fetch_add(1, Ordering::Relaxed),
            me: Weak::clone(me),
            shared: Arc::clone(shared),
            state: ferrix_sync::SpinLock::new(State {
                seals: SEAL_SEAL,
                permissions: permissions & 0o7777,
                uid: 0,
                gid: 0,
                nlink,
                atime: now,
                mtime: now,
                ctime: now,
                body,
            }),
            _charge: charge,
        })
    }

    fn now(&self) -> Timespec {
        self.shared.clock.now()
    }

    /// The child node called `name`.
    fn child(&self, name: &[u8]) -> Result<Arc<Node>> {
        let mut state = self.state.lock();
        let dir = state.dir()?;
        dir.get(name)
            .map(|entry| Arc::clone(&entry.node))
            .ok_or(Errno::ENOENT)
    }

    /// `other`, if it is an inode of this same tmpfs instance.
    fn ours(&self, other: &Arc<dyn Inode>) -> Result<Arc<Node>> {
        let node = Arc::clone(other)
            .into_any()
            .downcast::<Node>()
            .map_err(|_| Errno::EXDEV)?;
        if Arc::ptr_eq(&node.shared, &self.shared) {
            Ok(node)
        } else {
            Err(Errno::EXDEV)
        }
    }

    /// Whether `name` in this directory still names `ino`, and its kind.
    fn still_names(state: &mut State, name: &[u8], ino: Option<u64>) -> Result<Option<FileType>> {
        let dir = state.dir()?;
        let entry = dir.get(name);
        if entry.map(|entry| entry.ino) == ino {
            Ok(Some(entry.map_or(FileType::Regular, |entry| entry.kind)))
        } else {
            Ok(None)
        }
    }

    fn body_for(&self, node: NewNode<'_>) -> Result<Body> {
        Ok(match node {
            NewNode::Regular => Body::File {
                pages: self.shared.storage.allocate()?,
                len: 0,
            },
            NewNode::Directory => Body::Dir(Dir::new(Weak::clone(&self.me))),
            NewNode::Symlink(target) => {
                if target.len() >= PATH_MAX {
                    return Err(Errno::ENAMETOOLONG);
                }
                Body::Symlink(Box::from(target))
            }
            NewNode::Device { kind, rdev } => {
                if !matches!(kind, FileType::CharDevice | FileType::BlockDevice) {
                    return Err(Errno::EINVAL);
                }
                Body::Special { kind, rdev }
            }
            NewNode::Fifo => Body::Special {
                kind: FileType::Fifo,
                rdev: 0,
            },
            NewNode::Socket => Body::Special {
                kind: FileType::Socket,
                rdev: 0,
            },
        })
    }

    fn unlink_once(&self, name: &[u8]) -> Result<bool> {
        let child = self.child(name)?;
        let mut locked = Locked::new(&[self, &child]);
        let now = self.now();
        {
            let state = locked.state(self.ino)?;
            let Some(kind) = Node::still_names(state, name, Some(child.ino))? else {
                return Ok(false);
            };
            if kind == FileType::Directory {
                return Err(Errno::EISDIR);
            }
            let _ = state.dir()?.remove(name);
            state.touch(now);
        }
        let victim = locked.state(child.ino)?;
        victim.nlink = victim.nlink.saturating_sub(1);
        victim.ctime = now;
        Ok(true)
    }

    fn rmdir_once(&self, name: &[u8]) -> Result<bool> {
        let child = self.child(name)?;
        if child.ino == self.ino {
            return Err(Errno::EINVAL);
        }
        let mut locked = Locked::new(&[self, &child]);
        let now = self.now();
        if Node::still_names(locked.state(self.ino)?, name, Some(child.ino))?.is_none() {
            return Ok(false);
        }
        {
            let victim = locked.state(child.ino)?;
            let Body::Dir(dir) = &mut victim.body else {
                return Err(Errno::ENOTDIR);
            };
            if !dir.is_empty() {
                return Err(Errno::ENOTEMPTY);
            }
            dir.dead = true;
            victim.nlink = 0;
            victim.ctime = now;
        }
        let state = locked.state(self.ino)?;
        let _ = state.dir()?.remove(name);
        state.nlink = state.nlink.saturating_sub(1);
        state.touch(now);
        Ok(true)
    }

    fn rename_once(
        &self,
        old: &[u8],
        new_parent: &Arc<Node>,
        new: &[u8],
        replace: bool,
    ) -> Result<bool> {
        let source = self.child(old)?;
        let victim = match new_parent.child(new) {
            Ok(victim) => Some(victim),
            Err(Errno::ENOENT) => None,
            Err(other) => return Err(other),
        };
        if victim
            .as_ref()
            .is_some_and(|victim| victim.ino == source.ino)
        {
            return Ok(true);
        }
        if victim.is_some() && !replace {
            return Err(Errno::EEXIST);
        }
        // Before any inode lock is taken, because the climb takes each one in
        // turn; valid until the move because the caller holds the rename lock.
        if Node::lies_within(new_parent, &source) {
            return Err(Errno::EINVAL);
        }

        // Charged before anything moves: a refusal must not lose the name
        // it would have taken out of the old directory.
        let charge = Entry::charge(new)?;
        let mut nodes: Vec<&Node> = alloc::vec![self, new_parent, &source];
        if let Some(victim) = &victim {
            nodes.push(victim);
        }
        let mut locked = Locked::new(&nodes);
        let now = self.now();

        let Some(kind) = Node::still_names(locked.state(self.ino)?, old, Some(source.ino))? else {
            return Ok(false);
        };
        let victim_ino = victim.as_ref().map(|victim| victim.ino);
        {
            let target_dir = locked.state(new_parent.ino)?;
            if target_dir.dir()?.dead {
                return Err(Errno::ENOENT);
            }
            if Node::still_names(target_dir, new, victim_ino)?.is_none() {
                return Ok(false);
            }
        }
        let moving_dir = kind == FileType::Directory;
        let victim_dir = match victim_ino {
            Some(ino) => {
                let state = locked.state(ino)?;
                match (&state.body, moving_dir) {
                    (Body::Dir(dir), true) if !dir.is_empty() => return Err(Errno::ENOTEMPTY),
                    (Body::Dir(_), false) => return Err(Errno::EISDIR),
                    (Body::Dir(_), true) => true,
                    (_, true) => return Err(Errno::ENOTDIR),
                    (_, false) => false,
                }
            }
            None => false,
        };
        let crossing = self.ino != new_parent.ino;

        let entry = {
            let state = locked.state(self.ino)?;
            let entry = state.dir()?.remove(old).ok_or(Errno::EIO)?;
            if moving_dir && crossing {
                state.nlink = state.nlink.saturating_sub(1);
            }
            state.touch(now);
            entry
        };
        {
            let state = locked.state(new_parent.ino)?;
            let dir = state.dir()?;
            if victim_ino.is_some() {
                let _ = dir.remove(new);
            }
            dir.insert(new, entry.node, entry.kind, charge)?;
            if victim_dir {
                state.nlink = state.nlink.saturating_sub(1);
            }
            if moving_dir && crossing {
                state.nlink = state.nlink.saturating_add(1);
            }
            state.touch(now);
        }
        if let Some(ino) = victim_ino {
            let state = locked.state(ino)?;
            if let Body::Dir(dir) = &mut state.body {
                dir.dead = true;
                state.nlink = 0;
            } else {
                state.nlink = state.nlink.saturating_sub(1);
            }
            state.ctime = now;
        }
        let moved = locked.state(source.ino)?;
        moved.ctime = now;
        if let Body::Dir(dir) = &mut moved.body {
            dir.parent = Arc::downgrade(new_parent);
        }
        Ok(true)
    }

    /// Whether `node` is `ancestor` or inside it.
    ///
    /// Takes each directory's lock alone on the way up, so the caller must
    /// hold none, and must hold the rename lock for the answer to last.
    fn lies_within(node: &Arc<Node>, ancestor: &Node) -> bool {
        let mut at = Some(Arc::clone(node));
        while let Some(here) = at {
            if here.ino == ancestor.ino {
                return true;
            }
            at = here.state.lock().parent();
        }
        false
    }
}

impl Inode for Node {
    fn metadata(&self) -> Metadata {
        let state = self.state.lock();
        let (kind, size, blocks, rdev) = match &state.body {
            Body::File { pages, len } => {
                (FileType::Regular, *len, pages.committed_bytes() / 512, 0)
            }
            Body::Dir(dir) => (
                FileType::Directory,
                (dir.len() as u64)
                    .saturating_add(2)
                    .saturating_mul(DIRENT_SIZE),
                0,
                0,
            ),
            Body::Symlink(target) => (FileType::Symlink, target.len() as u64, 0, 0),
            Body::Special { kind, rdev } => (*kind, 0, 0, *rdev),
        };
        Metadata {
            ino: self.ino,
            kind,
            permissions: state.permissions,
            nlink: state.nlink,
            uid: state.uid,
            gid: state.gid,
            size,
            rdev,
            blocks,
            block_size: BLOCK_SIZE,
            atime: state.atime,
            mtime: state.mtime,
            ctime: state.ctime,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn set_attributes(&self, change: &SetAttributes) -> Result<()> {
        let now = self.now();
        let mut state = self.state.lock();
        if let Some(permissions) = change.permissions {
            state.permissions = permissions & 0o7777;
        }
        if let Some(uid) = change.uid {
            state.uid = uid;
        }
        if let Some(gid) = change.gid {
            state.gid = gid;
        }
        if let Some(atime) = change.atime {
            state.atime = atime;
        }
        if let Some(mtime) = change.mtime {
            state.mtime = mtime;
        }
        state.ctime = now;
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let state = self.state.lock();
        match &state.body {
            Body::File { pages, len } => {
                if offset >= *len {
                    return Ok(0);
                }
                let available = usize::try_from(*len - offset).unwrap_or(usize::MAX);
                let take = buf.len().min(available);
                pages.read(offset, buf.get_mut(..take).ok_or(Errno::EIO)?)?;
                Ok(take)
            }
            Body::Dir(_) => Err(Errno::EISDIR),
            _ => Err(Errno::EINVAL),
        }
    }

    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64)> {
        let now = self.now();
        let max = self.shared.storage.max_file_size();
        let mut state = self.state.lock();
        let state_seals = state.seals;
        let end = match &mut state.body {
            Body::File { pages, len } => {
                let start = if append { *len } else { offset };
                if data.is_empty() {
                    return Ok((0, start));
                }
                let end = start.checked_add(data.len() as u64).ok_or(Errno::EFBIG)?;
                if end > max {
                    return Err(Errno::EFBIG);
                }
                // `shmem_write_begin`'s order: a write seal refuses any write,
                // a grow seal one that would extend the file.
                if state_seals & (SEAL_WRITE | SEAL_FUTURE_WRITE) != 0
                    || (state_seals & SEAL_GROW != 0 && end > *len)
                {
                    return Err(Errno::EPERM);
                }
                pages.write(start, data)?;
                if end > *len {
                    *len = end;
                    pages.resize(end);
                }
                end
            }
            Body::Dir(_) => return Err(Errno::EISDIR),
            _ => return Err(Errno::EINVAL),
        };
        state.touch(now);
        Ok((data.len(), end))
    }

    fn set_len(&self, new_len: u64) -> Result<()> {
        let now = self.now();
        if new_len > self.shared.storage.max_file_size() {
            return Err(Errno::EFBIG);
        }
        let mut state = self.state.lock();
        let seals = state.seals;
        match &mut state.body {
            Body::File { len, .. }
                if (new_len < *len && seals & SEAL_SHRINK != 0)
                    || (new_len > *len && seals & SEAL_GROW != 0) =>
            {
                return Err(Errno::EPERM);
            }
            Body::File { pages, len } => {
                // The store hears the new length first, so a mapping's fault
                // past the cut is refused before the cut pages go.
                pages.resize(new_len);
                if new_len < *len {
                    pages.discard_from(new_len);
                }
                *len = new_len;
            }
            Body::Dir(_) => return Err(Errno::EISDIR),
            _ => return Err(Errno::EINVAL),
        }
        state.touch(now);
        Ok(())
    }

    fn grow_to(&self, new_len: u64) -> Result<()> {
        let now = self.now();
        if new_len > self.shared.storage.max_file_size() {
            return Err(Errno::EFBIG);
        }
        let mut state = self.state.lock();
        let seals = state.seals;
        match &mut state.body {
            // Decided under the lock every write takes, so a writer that
            // extends the file in the meantime is never cut back.
            Body::File { len, .. } if *len >= new_len => return Ok(()),
            Body::File { .. } if seals & SEAL_GROW != 0 => return Err(Errno::EPERM),
            // Nothing to clear: a shrink zeroes what it cuts off, so the bytes
            // this uncovers already read as zeros.
            Body::File { pages, len } => {
                *len = new_len;
                pages.resize(new_len);
            }
            Body::Dir(_) => return Err(Errno::EISDIR),
            _ => return Err(Errno::EINVAL),
        }
        state.touch(now);
        Ok(())
    }

    fn mapping(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        match &self.state.lock().body {
            Body::File { pages, .. } => pages.object(),
            _ => None,
        }
    }

    fn seals(&self) -> Result<u32> {
        let state = self.state.lock();
        match &state.body {
            Body::File { .. } => Ok(state.seals),
            _ => Err(Errno::EINVAL),
        }
    }

    fn add_seals(&self, seals: u32, writably_mapped: &dyn Fn() -> bool) -> Result<()> {
        if seals & !SEALS_KNOWN != 0 {
            return Err(Errno::EINVAL);
        }
        // Stored first and looked at second, and both under the node's lock,
        // as `mmap` counts itself first and then takes this lock to read the
        // seals. The lock orders the two sides whatever the processor's store
        // buffer does: if `mmap` takes it after this releases it, it sees the
        // seal; if it took it before, its count was raised before its
        // acquisition, which this acquisition follows, so the look below sees
        // the count. A write seal and a shared mapping that may write the file
        // therefore never both stand.
        let mut state = self.state.lock();
        if !matches!(state.body, Body::File { .. }) {
            return Err(Errno::EINVAL);
        }
        if state.seals & SEAL_SEAL != 0 {
            return Err(Errno::EPERM);
        }
        let added = seals & !state.seals;
        state.seals |= seals;
        if added & SEAL_WRITE != 0 && writably_mapped() {
            state.seals &= !added;
            return Err(Errno::EBUSY);
        }
        Ok(())
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        let mut state = self.state.lock();
        let dir = state.dir()?;
        if dir.dead {
            return Err(Errno::ENOENT);
        }
        dir.get(name)
            .map(|entry| Arc::clone(&entry.node) as Arc<dyn Inode>)
            .ok_or(Errno::ENOENT)
    }

    fn create(&self, name: &[u8], node: NewNode<'_>, permissions: u32) -> Result<Arc<dyn Inode>> {
        let now = self.now();
        let kind = node.kind();
        let child = Node::new(&self.shared, self.body_for(node)?, permissions, now)?;
        let charge = Entry::charge(name)?;
        let mut state = self.state.lock();
        {
            let dir = state.dir()?;
            if dir.dead {
                return Err(Errno::ENOENT);
            }
            if dir.get(name).is_some() {
                return Err(Errno::EEXIST);
            }
            dir.insert(name, Arc::clone(&child), kind, charge)?;
        }
        if kind == FileType::Directory {
            state.nlink = state.nlink.saturating_add(1);
        }
        state.touch(now);
        Ok(child)
    }

    fn link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<()> {
        let target = self.ours(target)?;
        if target.ino == self.ino {
            return Err(Errno::EPERM);
        }
        let charge = Entry::charge(name)?;
        let mut locked = Locked::new(&[self, &target]);
        let now = self.now();
        let kind = {
            let state = locked.state(target.ino)?;
            if state.is_dir() {
                return Err(Errno::EPERM);
            }
            if state.nlink == 0 {
                return Err(Errno::ENOENT);
            }
            match &state.body {
                Body::File { .. } => FileType::Regular,
                Body::Symlink(_) => FileType::Symlink,
                Body::Special { kind, .. } => *kind,
                Body::Dir(_) => FileType::Directory,
            }
        };
        {
            let state = locked.state(self.ino)?;
            let dir = state.dir()?;
            if dir.dead {
                return Err(Errno::ENOENT);
            }
            if dir.get(name).is_some() {
                return Err(Errno::EEXIST);
            }
            dir.insert(name, Arc::clone(&target), kind, charge)?;
            state.touch(now);
        }
        let state = locked.state(target.ino)?;
        state.nlink = state.nlink.saturating_add(1);
        state.ctime = now;
        Ok(())
    }

    fn unlink(&self, name: &[u8]) -> Result<()> {
        while !self.unlink_once(name)? {}
        Ok(())
    }

    fn rmdir(&self, name: &[u8]) -> Result<()> {
        while !self.rmdir_once(name)? {}
        Ok(())
    }

    fn rename(
        &self,
        old: &[u8],
        new_parent: &Arc<dyn Inode>,
        new: &[u8],
        replace: bool,
    ) -> Result<()> {
        let new_parent = self.ours(new_parent)?;
        let _serialised = self.shared.renames.lock();
        while !self.rename_once(old, &new_parent, new, replace)? {}
        Ok(())
    }

    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        let mut state = self.state.lock();
        let dir = state.dir()?;
        for (&at, entry) in dir.by_cursor.range(cursor.max(FIRST_CURSOR)..) {
            let accepted = emit(DirEntry {
                ino: entry.ino,
                kind: entry.kind,
                name: &entry.name,
                next: at.saturating_add(1),
            });
            if !accepted {
                break;
            }
        }
        Ok(())
    }

    fn read_link(&self) -> Result<Vec<u8>> {
        match &self.state.lock().body {
            Body::Symlink(target) => Ok(target.to_vec()),
            _ => Err(Errno::EINVAL),
        }
    }
}
