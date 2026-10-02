//! btrfs mounted read-write, over `ferrix-btrfs-write`.
//!
//! The read-only mount beside this one ([`crate::Btrfs`]) reads a volume that
//! cannot change, which is what lets it read with no lock held. A writable
//! mount cannot: every read must see the transaction the writes are going
//! into — a file created a moment ago is not on the disk yet — and two edits
//! to one tree cannot run at once. So this mount keeps the whole volume
//! behind one lock, `ferrix_sync::SleepLock`, whose waiters sleep rather than
//! spin, because an operation under it does I/O.
//!
//! # What holds what
//!
//! The lock order is: the volume, then an inode's own locks. It is never the
//! other way round, and one rule keeps it honest: **nothing calls into the
//! page cache while holding the volume lock, except to read a page that is
//! certainly present.** A page cache miss is filled from the volume, which
//! takes the lock; a writeback reads only pages something wrote, which are
//! present by construction, so it cannot miss.
//!
//! # Writes become extents at the commit, not at the write
//!
//! A `write` goes into the page cache and is remembered as a dirty page. The
//! commit — `fsync`, `sync`, unmount, or enough dirty bytes — turns each run
//! of dirty pages into extents with their checksums. That is what makes an
//! append of a byte at a time cost one extent rather than one per byte, and
//! it is why a program that does not `fsync` can lose its last writes, as on
//! any filesystem.
//!
//! A write through a shared mapping marks no page: the program writes the
//! page directly. So the store says which pages such a mapping may have
//! written (`Pages::mapped_writes`) -- every page it holds, once a mapping
//! that may write has been made -- and a writeback writes those as well.
//!
//! # What is written stays
//!
//! The dirty pages and the length a write gave are in the inode object, and
//! nothing else keeps that object: the VFS holds a bounded number of names,
//! and a file closed and not looked up again loses its last reference. So
//! the mount holds every object with dirty pages itself, from the write that
//! dirtied the first until the writeback, as Linux's list of dirty inodes
//! does. Without that, a build that closes an object file and opens it
//! again to archive it read back whatever the last commit had written. A
//! file handed out for mapping is held too, since the mapping may write
//! without a word to the mount, until a writeback finds no mapping left that
//! may: a linker writes its output through one.
//!
//! # Room is taken at the write
//!
//! Since a write's extents are made at the commit, a volume that has run
//! out would only find out there, after `write` had said yes. So a write
//! takes its room when it dirties a page: the mount keeps the bytes its dirty
//! pages will need (`reserved`) against what the volume can still take
//! (`room`), and a write past that answers `ENOSPC` -- or the part that fits
//! -- as on Linux, whose btrfs reserves data space in `write` the same way.
//! The writeback then measures each run before it changes anything, so a
//! volume that is full after all answers `ENOSPC` with the pages kept dirty,
//! rather than half-writing a file into the transaction.
//!
//! Room a deletion frees comes back only once the commit is down. So a sync
//! whose writeback ran out goes once more when its commit made room, and a
//! write that finds too little after an unlink, a rename or a truncation
//! commits and measures again before it answers `ENOSPC`.
//!
//! The trees are measured the same way, by the write path: an operation is
//! refused with `ENOSPC` before its first edit unless the trees have room
//! for its worst case and for the commit (`WriteVolume::admit`).
//!
//! # After an aborted transaction
//!
//! An edit that fails half-way -- a disk error, a volume that contradicts
//! itself -- aborts the transaction, which nothing may commit. The next use
//! of the volume reloads it from its last commit and makes the mount
//! read-only, says so through the `notice` the mount was given, and goes on
//! answering reads; Linux turns a btrfs mount read-only on an abort too.
//! What the aborted transaction held is lost, as on Linux.
//!
//! # Deleting what is still open
//!
//! Unlinking a file that something still holds leaves the inode with an
//! orphan item, as btrfs does. When the last reference to the inode object
//! goes, its number joins a list the next operation drains, which deletes it
//! for good. A crash before that leaves the orphan item, and the next mount
//! cleans it up.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::items::Timespec as BtrfsTime;
use ferrix_btrfs_write::fs::NewInode;
use ferrix_btrfs_write::{Error as WriteError, Unsupported, WriteDevice, WriteVolume};
use ferrix_sync::{Parker, SleepLock, SleepLockGuard, SpinLock};
use ferrix_vfs::tmpfs::{PAGE_SIZE, PageSource, Pages, Storage};
use ferrix_vfs::{
    Clock, DirEntry, Errno, FIRST_CURSOR, FileSystem, FileType, Inode, Metadata, NewNode, Result,
    SetAttributes, StatFs, Timespec,
};

use crate::{BTRFS_SUPER_MAGIC, NAME_MAX};

/// Pages a writeback turns into extents at a time: 256, a mebibyte, which
/// bounds the buffer it copies them through. More would be fewer, larger
/// extents; a kernel heap of a few megabytes is what says no.
const WRITEBACK_PAGES: usize = 256;

/// Dirty bytes after which a write commits of its own accord, so that a
/// program writing forever does not hold an unbounded transaction. The same
/// holds the tree nodes a transaction has changed: a program making and
/// removing names in a loop dirties them without writing a page, and held
/// them all in memory until the commit interval ran out.
const COMMIT_THRESHOLD: u64 = 32 * 1024 * 1024;

/// What a writable mount needs of its device.
pub trait WriteHandle: WriteDevice + Send + Sync + 'static {}

impl<T: WriteDevice + Send + Sync + 'static> WriteHandle for T {}

/// What every inode of one writable mount shares.
struct Shared<D> {
    /// The volume, and with it the running transaction.
    volume: SleepLock<WriteVolume<D>>,
    dev_no: u64,
    storage: Arc<dyn Storage>,
    clock: Arc<dyn Clock>,
    /// Every live inode object, by inode number.
    nodes: SpinLock<BTreeMap<u64, Weak<Node<D>>>>,
    /// Every inode object with pages not yet written back, held so that
    /// they outlive every other reference: see "What is written stays". An
    /// object's entry is made and taken away under its own `dirty` lock.
    held: SpinLock<BTreeMap<u64, Arc<Node<D>>>>,
    /// Inodes with no names left whose last reference has gone.
    evictable: SpinLock<Vec<u64>>,
    /// Bytes written into the page cache since the last commit.
    pending: SpinLock<u64>,
    /// Whether anything has changed the shape of the tree — a name made,
    /// moved or removed — since the last commit. A log carries one inode's
    /// items and no names, so an `fsync` with this set commits instead.
    structural: SpinLock<bool>,
    /// Bytes the dirty pages of every inode will take when written back: a
    /// page each. See "Room is taken at the write".
    reserved: SpinLock<u64>,
    /// What the volume could still take the last time it was looked at:
    /// `WriteVolume::data_room`, kept here so a write need not wait for the
    /// volume's lock to measure itself against it.
    room: SpinLock<u64>,
    /// Set once the volume was reloaded read-only after an aborted
    /// transaction.
    read_only: SpinLock<bool>,
    /// Set when something may have freed data since the last commit -- an
    /// unlink, a rename over a file, a truncation, an eviction -- whose room
    /// comes back only at the commit.
    freed: SpinLock<bool>,
    /// Set once reloading an aborted volume failed: it stays aborted, every
    /// answer `EIO`, and is not reread at every use.
    unreadable: SpinLock<bool>,
    /// Where the mount says that it went read-only, and why.
    notice: fn(fmt::Arguments<'_>),
}

impl<D> fmt::Debug for Shared<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("dev_no", &self.dev_no)
            .finish_non_exhaustive()
    }
}

/// A btrfs volume mounted read-write.
pub struct RwBtrfs<D> {
    shared: Arc<Shared<D>>,
    root: Arc<Node<D>>,
}

impl<D> fmt::Debug for RwBtrfs<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwBtrfs")
            .field("shared", &self.shared)
            .finish_non_exhaustive()
    }
}

impl<D: WriteHandle> RwBtrfs<D> {
    /// Mount the volume on `device` for writing.
    ///
    /// `EROFS` when the volume is one this writer will not change — a
    /// snapshot, quotas, an unreplayed log — with the reason in the error the
    /// write path gives; `EINVAL` when it is not a volume at all, and `EIO`
    /// when it cannot be read.
    ///
    /// `notice` is told, once, if an aborted transaction turns the mount
    /// read-only: see "After an aborted transaction".
    pub fn mount(
        device: D,
        dev_no: u64,
        storage: Arc<dyn Storage>,
        clock: Arc<dyn Clock>,
        parker: &dyn Parker,
        notice: fn(fmt::Arguments<'_>),
    ) -> Result<Arc<RwBtrfs<D>>> {
        let volume = WriteVolume::open(device).map_err(mount_errno)?;
        let room = volume.data_room();
        let shared = Arc::new(Shared {
            volume: SleepLock::new(volume, parker),
            dev_no,
            storage,
            clock,
            nodes: SpinLock::new(BTreeMap::new()),
            held: SpinLock::new(BTreeMap::new()),
            evictable: SpinLock::new(Vec::new()),
            pending: SpinLock::new(0),
            structural: SpinLock::new(false),
            reserved: SpinLock::new(0),
            room: SpinLock::new(room),
            read_only: SpinLock::new(false),
            freed: SpinLock::new(false),
            unreadable: SpinLock::new(false),
            notice,
        });
        // The open may have evicted orphans left by a crash; commit that
        // before anything else changes, so a second crash has less to redo.
        shared.with(WriteVolume::commit)?;
        let root = Node::get(&shared, ferrix_btrfs::items::FIRST_FREE_OBJECTID)?;
        if root.kind != FileType::Directory {
            return Err(Errno::EIO);
        }
        Ok(Arc::new(RwBtrfs { shared, root }))
    }

    /// Write everything back and commit: what `sync` and unmounting do.
    pub fn sync(&self) -> Result<()> {
        self.shared.sync_all()
    }
}

impl<D: WriteHandle> Shared<D> {
    /// The volume, under its lock. One whose transaction an earlier edit
    /// aborted is reloaded read-only from its last commit first: see "After
    /// an aborted transaction".
    fn lock(&self) -> SleepLockGuard<'_, WriteVolume<D>> {
        let mut volume = self.volume.lock();
        if let Some(cause) = volume.aborted()
            && !*self.unreadable.lock()
        {
            match volume.reload_read_only() {
                Ok(()) => {
                    *self.read_only.lock() = true;
                    (self.notice)(format_args!(
                        "a transaction was aborted ({cause}); read-only from here, \
                         at the last commit"
                    ));
                }
                // Left aborted: every answer stays EIO, said once.
                Err(error) => {
                    *self.unreadable.lock() = true;
                    (self.notice)(format_args!(
                        "a transaction was aborted ({cause}), and the last commit \
                         cannot be read again ({error}); every answer is EIO"
                    ));
                }
            }
            self.note_room(&volume);
        }
        volume
    }

    /// Remember what the volume can still take, after something used or
    /// freed room.
    fn note_room(&self, volume: &WriteVolume<D>) {
        *self.room.lock() = if volume.is_read_only() {
            0
        } else {
            volume.data_room()
        };
    }

    /// Take room for `pages` more dirty pages, or for as many of them as
    /// fit. Returns how many it took.
    fn reserve(&self, pages: u64) -> u64 {
        let room = *self.room.lock();
        let mut reserved = self.reserved.lock();
        let fit = room.saturating_sub(*reserved) / PAGE_SIZE;
        let took = pages.min(fit);
        *reserved = reserved.saturating_add(took.saturating_mul(PAGE_SIZE));
        took
    }

    /// Take room for `pages` dirty pages whether it is there or not: pages
    /// that are dirty already, as a writeback that failed puts back.
    fn reserve_anyway(&self, pages: u64) {
        let mut reserved = self.reserved.lock();
        *reserved = reserved.saturating_add(pages.saturating_mul(PAGE_SIZE));
    }

    /// Give back the room of `pages` dirty pages that are no longer dirty.
    fn release(&self, pages: u64) {
        let mut reserved = self.reserved.lock();
        *reserved = reserved.saturating_sub(pages.saturating_mul(PAGE_SIZE));
    }

    /// Run `op` with the volume, translating its error. Evictions queued by
    /// dropped inodes are done first, so they never pile up.
    fn with<T>(
        &self,
        op: impl FnOnce(&mut WriteVolume<D>) -> core::result::Result<T, WriteError>,
    ) -> Result<T> {
        let mut volume = self.lock();
        self.evict_queued(&mut volume)?;
        op(&mut volume).map_err(errno)
    }

    /// Delete the inodes queued by dropped objects with no names left.
    fn evict_queued(&self, volume: &mut WriteVolume<D>) -> Result<()> {
        // A read-only volume deletes nothing; its orphans go at the next
        // writable mount, as a crash's do.
        if volume.is_read_only() {
            return Ok(());
        }
        let queued = core::mem::take(&mut *self.evictable.lock());
        if !queued.is_empty() {
            *self.freed.lock() = true;
        }
        for ino in queued {
            // An inode that gained a name again is no orphan; the write path
            // says so by refusing to evict it.
            if volume
                .inode(ino)
                .is_ok_and(|item| item.is_some_and(|item| item.nlink == 0))
            {
                volume.evict(ino).map_err(errno)?;
            }
        }
        Ok(())
    }

    fn now(&self) -> BtrfsTime {
        let now = self.clock.now();
        BtrfsTime {
            sec: now.tv_sec.cast_unsigned(),
            nsec: u32::try_from(now.tv_nsec).unwrap_or(0),
        }
    }

    /// Write back every dirty page of every live inode, then commit.
    ///
    /// An inode the volume has no room for keeps its pages dirty and the
    /// rest are written back and committed all the same, so what fit is
    /// kept; the answer is then `ENOSPC`.
    fn sync_all(&self) -> Result<()> {
        let live: Vec<Arc<Node<D>>> = self
            .nodes
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        let mut volume = self.lock();
        // Nothing can be written back or committed any more; the pages stay
        // in the cache, and saying so at every sync would tell no more than
        // the notice did.
        if volume.is_read_only() {
            return Ok(());
        }
        // Files deleted since go first, so the commit frees their room.
        self.evict_queued(&mut volume)?;
        // What a deletion freed is free only once the commit is down, after
        // the writeback that wanted it; so a writeback that ran out, when
        // that commit made room, goes once more.
        let mut full = false;
        for pass in 0..2 {
            full = false;
            for node in &live {
                match node.write_back(&mut volume) {
                    Ok(()) => {}
                    Err(Errno::ENOSPC) => full = true,
                    Err(error) => return Err(error),
                }
            }
            let before = volume.data_room();
            *self.pending.lock() = 0;
            *self.structural.lock() = false;
            let committed = volume.commit().map_err(errno);
            self.note_room(&volume);
            committed?;
            *self.freed.lock() = false;
            if !full || pass == 1 || volume.data_room() <= before {
                break;
            }
        }
        if full { Err(Errno::ENOSPC) } else { Ok(()) }
    }

    /// Note that the shape of the tree changed; see `structural`.
    fn shape_changed(&self) {
        *self.structural.lock() = true;
    }

    /// Commit first if the running transaction holds more tree nodes than
    /// [`COMMIT_THRESHOLD`]: before a change to the tree's shape, so what a
    /// loop of them holds in memory stays within the threshold and one more.
    fn commit_if_heavy(&self) -> Result<()> {
        let heavy = self.lock().dirty_bytes() >= COMMIT_THRESHOLD;
        if heavy { self.sync_all() } else { Ok(()) }
    }
}

/// One file, directory or link of a writable mount.
struct Node<D> {
    shared: Arc<Shared<D>>,
    /// This object, for the mount to hold while it has dirty pages.
    me: Weak<Node<D>>,
    ino: u64,
    kind: FileType,
    /// What `stat` answers, kept in step with every change made through this
    /// object.
    meta: SpinLock<Metadata>,
    /// The page cache, made at first use.
    pages: SpinLock<Option<Arc<dyn Pages>>>,
    /// Pages written and not yet turned into extents.
    dirty: SpinLock<BTreeSet<u64>>,
}

impl<D> fmt::Debug for Node<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("ino", &self.ino)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl<D: WriteHandle> Node<D> {
    /// The inode object for `ino`: the one alive already, or one built from
    /// its `INODE_ITEM`.
    fn get(shared: &Arc<Shared<D>>, ino: u64) -> Result<Arc<Node<D>>> {
        if let Some(alive) = shared.nodes.lock().get(&ino).and_then(Weak::upgrade) {
            return Ok(alive);
        }
        let item = shared.with(|volume| volume.inode(ino))?.ok_or(Errno::EIO)?;
        let sector = shared.with(|volume| Ok(volume.sectorsize()))?;
        let meta = crate::metadata(ino, &item, sector)?;
        let built = Arc::new_cyclic(|me| Node {
            shared: Arc::clone(shared),
            me: Weak::clone(me),
            ino,
            kind: meta.kind,
            meta: SpinLock::new(meta),
            pages: SpinLock::new(None),
            dirty: SpinLock::new(BTreeSet::new()),
        });
        let mut nodes = shared.nodes.lock();
        if let Some(alive) = nodes.get(&ino).and_then(Weak::upgrade) {
            return Ok(alive);
        }
        let _ = nodes.insert(ino, Arc::downgrade(&built));
        Ok(built)
    }

    /// This file's page cache, made over the volume at the first call.
    fn pages(&self) -> Result<Arc<dyn Pages>> {
        if let Some(pages) = self.pages.lock().as_ref() {
            return Ok(Arc::clone(pages));
        }
        let source: Arc<dyn PageSource> = Arc::new(Source {
            shared: Arc::clone(&self.shared),
            ino: self.ino,
        });
        let made: Arc<dyn Pages> = Arc::from(self.shared.storage.allocate_with(source)?);
        made.resize(self.meta.lock().size);
        let mut slot = self.pages.lock();
        Ok(Arc::clone(slot.get_or_insert(made)))
    }

    fn require_dir(&self) -> Result<()> {
        if self.kind == FileType::Directory {
            Ok(())
        } else {
            Err(Errno::ENOTDIR)
        }
    }

    /// Have the mount hold this object until its next writeback. Called under
    /// this object's `dirty` lock, as the writeback that lets go takes it.
    fn hold(&self) {
        if let Some(me) = self.me.upgrade() {
            // Whatever this replaces is this object, and not its last
            // reference; dropped after the map's lock is let go.
            let replaced = self.shared.held.lock().insert(self.ino, me);
            drop(replaced);
        }
    }

    /// Refresh what `stat` answers from the volume.
    ///
    /// What the volume holds is behind what this object does while pages
    /// wait to be written back — the file's length lives in memory until
    /// then, as Linux's `i_size` does — so the length and the times this
    /// object knows win over the ones on the disk until the writeback makes
    /// them agree.
    fn refresh(&self, volume: &mut WriteVolume<D>) -> Result<()> {
        let item = volume.inode(self.ino).map_err(errno)?.ok_or(Errno::EIO)?;
        let sector = volume.sectorsize();
        let mut fresh = crate::metadata(self.ino, &item, sector)?;
        let mut meta = self.meta.lock();
        if !self.dirty.lock().is_empty() {
            fresh.size = fresh.size.max(meta.size);
            fresh.mtime = meta.mtime;
            fresh.ctime = meta.ctime;
        }
        *meta = fresh;
        Ok(())
    }

    /// Turn this inode's dirty pages into extents. The volume's lock is held,
    /// so only pages that are certainly present are read — which every dirty
    /// page is, because something wrote it.
    fn write_back(&self, volume: &mut WriteVolume<D>) -> Result<()> {
        let store = self.pages.lock().clone();
        let (dirty, held) = {
            let mut dirty = self.dirty.lock();
            // Every page dirty now took its room; the ones taken out below
            // give it back, and the ones put back take it again.
            self.shared.release(dirty.len() as u64);
            // What a shared mapping may have written, which marked nothing:
            // pages the store holds, so reading them takes no fill.
            let (written, mapped) = store
                .as_ref()
                .map_or((Vec::new(), false), |pages| pages.mapped_writes());
            dirty.extend(written);
            // Written back from here on, so the mount lets go, unless the
            // file is mapped or about to be; a write after this marks its
            // pages under the same lock and holds it again.
            let held = if mapped {
                None
            } else {
                self.shared.held.lock().remove(&self.ino)
            };
            (core::mem::take(&mut *dirty), held)
        };
        // Never the last reference: whoever called this holds one.
        drop(held);
        if dirty.is_empty() {
            return Ok(());
        }
        let Some(pages) = self.pages.lock().clone() else {
            return Ok(());
        };
        let size = self.meta.lock().size;
        let written = self.write_runs(volume, &pages, &dirty, size);
        self.shared.note_room(volume);
        if let Err((error, first)) = written {
            // The pages from the run that failed on stay dirty, to be written
            // when there is room, or to go with the file.
            let mut still = self.dirty.lock();
            if still.is_empty() {
                self.hold();
            }
            let before = still.len();
            still.extend(dirty.range(first..));
            self.shared
                .reserve_anyway(still.len().saturating_sub(before) as u64);
            return Err(error);
        }
        self.refresh(volume)
    }

    /// Write the runs of `dirty` into the volume. A failure answers with the
    /// first page of the run that failed, which nothing of was written.
    fn write_runs(
        &self,
        volume: &mut WriteVolume<D>,
        pages: &Arc<dyn Pages>,
        dirty: &BTreeSet<u64>,
        size: u64,
    ) -> core::result::Result<(), (Errno, u64)> {
        let page = usize::try_from(PAGE_SIZE).map_err(|_| (Errno::EIO, 0))?;
        for (first, count) in runs(dirty, WRITEBACK_PAGES) {
            let offset = first.checked_mul(PAGE_SIZE).ok_or((Errno::EIO, first))?;
            if offset >= size {
                continue;
            }
            let len = count.saturating_mul(page);
            let mut buf = vec![0u8; len];
            pages
                .read(offset, &mut buf)
                .map_err(|error| (error, first))?;
            // The last page of the file is written up to its size; the bytes
            // after it belong to nothing and the write path zero-fills them.
            let end = offset.saturating_add(len as u64).min(size);
            let keep =
                usize::try_from(end.saturating_sub(offset)).map_err(|_| (Errno::EIO, first))?;
            buf.truncate(keep);
            volume
                .write_file(self.ino, offset, &buf, size)
                .map_err(|error| (errno(error), first))?;
        }
        Ok(())
    }

    /// The part of `data`, to be written at `at`, the volume has room for,
    /// with the room taken: a page for each page of it that is not dirty
    /// already. Returns that part and the pages taken; `ENOSPC` if not a
    /// byte fits.
    fn take_room<'d>(&self, at: u64, data: &'d [u8]) -> Result<(&'d [u8], u64)> {
        if data.is_empty() {
            return Ok((data, 0));
        }
        let end = at.saturating_add(data.len() as u64);
        let first = at / PAGE_SIZE;
        let last = end.saturating_sub(1) / PAGE_SIZE;
        let clean: Vec<u64> = {
            let dirty = self.dirty.lock();
            (first..=last)
                .filter(|page| !dirty.contains(page))
                .collect()
        };
        let mut took = self.shared.reserve(clean.len() as u64);
        // Short of room, with data deleted since the last commit: that room
        // comes back at the commit, so commit and measure again, rather than
        // answer ENOSPC to the write after an `rm`.
        if took < clean.len() as u64 && core::mem::take(&mut *self.shared.freed.lock()) {
            match self.shared.sync_all() {
                Ok(()) | Err(Errno::ENOSPC) => {}
                Err(error) => {
                    self.shared.release(took);
                    return Err(error);
                }
            }
            took = took.saturating_add(self.shared.reserve(clean.len() as u64 - took));
        }
        let Some(&short) = clean.get(usize::try_from(took).unwrap_or(usize::MAX)) else {
            return Ok((data, took));
        };
        // Up to the first clean page there was no room for.
        let fits = short.saturating_mul(PAGE_SIZE).saturating_sub(at);
        let fits = usize::try_from(fits).unwrap_or(0).min(data.len());
        if fits == 0 {
            self.shared.release(took);
            return Err(Errno::ENOSPC);
        }
        Ok((data.get(..fits).unwrap_or(data), took))
    }

    /// Commit if the page cache holds more than a transaction should.
    ///
    /// A volume that turned out full is not this write's failure: its bytes
    /// took their room and are in the cache, and whichever pages did not fit
    /// stay dirty for the next `sync` to report.
    fn maybe_commit(&self, written: usize) -> Result<()> {
        let over = {
            let mut pending = self.shared.pending.lock();
            *pending = pending.saturating_add(written as u64);
            *pending >= COMMIT_THRESHOLD
        };
        match if over { self.shared.sync_all() } else { Ok(()) } {
            Err(Errno::ENOSPC) => Ok(()),
            other => other,
        }
    }

    /// Apply `edit` to the volume with this inode's dirty pages written back
    /// first, so what the edit sees is what the file holds.
    fn with_written_back<T>(
        &self,
        edit: impl FnOnce(&mut WriteVolume<D>) -> core::result::Result<T, WriteError>,
    ) -> Result<T> {
        let mut volume = self.shared.lock();
        self.write_back(&mut volume)?;
        let out = edit(&mut volume).map_err(errno)?;
        self.refresh(&mut volume)?;
        Ok(out)
    }
}

impl<D> Drop for Node<D> {
    /// Take this object's entry out of the mount's map, and remember an inode
    /// with no names for the next operation to delete.
    fn drop(&mut self) {
        let orphaned = self.meta.lock().nlink == 0;
        let mut nodes = self.shared.nodes.lock();
        let mine = nodes
            .get(&self.ino)
            .is_some_and(|weak| core::ptr::addr_eq(weak.as_ptr(), self));
        if mine {
            let _ = nodes.remove(&self.ino);
            if orphaned {
                self.shared.evictable.lock().push(self.ino);
            }
        }
    }
}

/// A file's pages, filled from the volume.
struct Source<D> {
    shared: Arc<Shared<D>>,
    ino: u64,
}

impl<D> fmt::Debug for Source<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("ino", &self.ino)
            .finish_non_exhaustive()
    }
}

impl<D: WriteHandle> PageSource for Source<D> {
    fn reads_disk(&self) -> bool {
        true
    }

    fn fill_range(&self, first: u64, pages: &mut [&mut [u8]]) -> Result<usize> {
        if pages.is_empty() {
            return Err(Errno::EINVAL);
        }
        let offset = first.checked_mul(PAGE_SIZE).ok_or(Errno::EIO)?;
        let page = usize::try_from(PAGE_SIZE).map_err(|_| Errno::EIO)?;
        let mut run = vec![0u8; page.saturating_mul(pages.len())];
        let ino = self.ino;
        let read = self
            .shared
            .with(|volume| volume.read_file(ino, offset, &mut run));
        match read {
            Ok(_) => {
                for (target, filled) in pages.iter_mut().zip(run.chunks_exact(page)) {
                    target.copy_from_slice(filled);
                }
                Ok(pages.len())
            }
            // As the read-only mount: a run that fails is retried a page at a
            // time, so the pages before the bad one are kept and the bad one
            // answers for itself.
            Err(error) if pages.len() == 1 => Err(error),
            Err(_) => {
                let mut filled = 0;
                for (index, target) in pages.iter_mut().enumerate() {
                    let at = offset.saturating_add(PAGE_SIZE.saturating_mul(index as u64));
                    match self.shared.with(|volume| volume.read_file(ino, at, target)) {
                        Ok(_) => filled += 1,
                        Err(error) if filled == 0 => return Err(error),
                        Err(_) => break,
                    }
                }
                Ok(filled)
            }
        }
    }
}

impl<D: WriteHandle> FileSystem for RwBtrfs<D> {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::clone(&self.root) as Arc<dyn Inode>
    }

    fn name(&self) -> &'static str {
        "btrfs"
    }

    fn device(&self) -> u64 {
        self.shared.dev_no
    }

    fn sync(&self) -> Result<()> {
        self.shared.sync_all()
    }

    fn statfs(&self) -> StatFs {
        let volume = self.shared.lock();
        let block_size = u64::from(volume.sectorsize());
        let (total, _) = volume.capacity();
        // What a write can still take: the volume's room less what dirty
        // pages have taken of it. Not the device less what is used, which
        // counted every DUP tree block once and the device space no chunk
        // can be made of.
        drop(volume);
        let room = self
            .shared
            .room
            .lock()
            .saturating_sub(*self.shared.reserved.lock());
        let free = room / block_size.max(1);
        StatFs {
            magic: BTRFS_SUPER_MAGIC,
            block_size,
            blocks: total / block_size.max(1),
            blocks_free: free,
            blocks_available: free,
            files: 0,
            files_free: 0,
            name_max: NAME_MAX,
        }
    }
}

impl<D: WriteHandle> Inode for Node<D> {
    fn metadata(&self) -> Metadata {
        *self.meta.lock()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn set_attributes(&self, change: &SetAttributes) -> Result<()> {
        let now = self.shared.now();
        let ino = self.ino;
        let changed = *change;
        self.shared.with(|volume| {
            let mut item = volume.inode(ino)?.ok_or(WriteError::NotFound)?;
            if let Some(permissions) = changed.permissions {
                item.mode = (item.mode & !0o7777) | (permissions & 0o7777);
            }
            if let Some(uid) = changed.uid {
                item.uid = uid;
            }
            if let Some(gid) = changed.gid {
                item.gid = gid;
            }
            if let Some(atime) = changed.atime {
                item.atime = time_in(atime);
            }
            if let Some(mtime) = changed.mtime {
                item.mtime = time_in(mtime);
            }
            item.ctime = now;
            volume.write_inode(ino, &item)
        })?;
        let mut volume = self.shared.lock();
        self.refresh(&mut volume)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if self.kind != FileType::Regular {
            return Err(Errno::EINVAL);
        }
        let size = self.meta.lock().size;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let len = usize::try_from(size - offset).map_or(buf.len(), |rest| rest.min(buf.len()));
        let out = buf.get_mut(..len).unwrap_or_default();
        self.pages()?.read(offset, out)?;
        Ok(len)
    }

    /// Into the page cache, with the volume's lock not held: filling a page
    /// the write covers only part of takes it.
    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64)> {
        if self.kind != FileType::Regular {
            return Err(Errno::EINVAL);
        }
        let pages = self.pages()?;
        let at = if append {
            self.meta.lock().size
        } else {
            offset
        };
        if *self.shared.read_only.lock() {
            return Err(Errno::EROFS);
        }
        let end = at.checked_add(data.len() as u64).ok_or(Errno::EFBIG)?;
        if end > self.shared.storage.max_file_size() {
            return Err(Errno::EFBIG);
        }
        let (data, took) = self.take_room(at, data)?;
        let end = at.saturating_add(data.len() as u64);
        pages.write(at, data)?;
        {
            let mut meta = self.meta.lock();
            meta.size = meta.size.max(end);
            meta.mtime = self.shared.clock.now();
            meta.ctime = meta.mtime;
            pages.resize(meta.size);
        }
        if !data.is_empty() {
            let first = at / PAGE_SIZE;
            let last = end.saturating_sub(1) / PAGE_SIZE;
            let mut dirty = self.dirty.lock();
            if dirty.is_empty() {
                self.hold();
            }
            let before = dirty.len();
            for page in first..=last {
                let _ = dirty.insert(page);
            }
            // The room taken was for the pages that were clean when it was
            // measured; what changed since -- another write dirtied one, a
            // writeback cleaned one -- is settled here, under the lock that
            // makes the pages dirty.
            let added = (dirty.len() - before) as u64;
            if added > took {
                self.shared.reserve_anyway(added - took);
            } else {
                self.shared.release(took - added);
            }
        }
        self.maybe_commit(data.len())?;
        // The offset just past the write, which a `write` moves the file's
        // position to: the kernel copies a large write in pieces, and a start
        // offset here wrote every piece over the first.
        Ok((data.len(), end))
    }

    fn set_len(&self, len: u64) -> Result<()> {
        if self.kind != FileType::Regular {
            return Err(Errno::EINVAL);
        }
        // Before the cache is cut: a read-only mount keeps what it holds.
        if *self.shared.read_only.lock() {
            return Err(Errno::EROFS);
        }
        *self.shared.freed.lock() = true;
        let pages = self.pages()?;
        // Cut the cache first, so a page past the new end cannot be written
        // back over what the truncation removed.
        pages.resize(len);
        pages.discard_from(len);
        {
            let mut dirty = self.dirty.lock();
            let before = dirty.len();
            dirty.retain(|&page| page * PAGE_SIZE < len);
            self.shared.release((before - dirty.len()) as u64);
        }
        let ino = self.ino;
        let now = self.shared.now();
        self.with_written_back(|volume| {
            volume.truncate(ino, len)?;
            let mut item = volume.inode(ino)?.ok_or(WriteError::NotFound)?;
            item.mtime = now;
            item.ctime = now;
            volume.write_inode(ino, &item)
        })
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        self.require_dir()?;
        let dir = self.ino;
        let found = self.shared.with(|volume| volume.lookup(dir, name))?;
        let (ino, _) = found.ok_or(Errno::ENOENT)?;
        Ok(Node::get(&self.shared, ino)? as Arc<dyn Inode>)
    }

    fn create(&self, name: &[u8], node: NewNode<'_>, permissions: u32) -> Result<Arc<dyn Inode>> {
        self.require_dir()?;
        self.shared.commit_if_heavy()?;
        self.shared.shape_changed();
        let dir = self.ino;
        let now = self.shared.now();
        let (kind, rdev, target) = match node {
            NewNode::Directory => (FileType::Directory, 0, None),
            NewNode::Regular => (FileType::Regular, 0, None),
            NewNode::Symlink(target) => (FileType::Symlink, 0, Some(target.to_vec())),
            NewNode::Fifo => (FileType::Fifo, 0, None),
            NewNode::Socket => (FileType::Socket, 0, None),
            NewNode::Device { kind, rdev } => (kind, rdev, None),
        };
        let mode = kind.mode_bits() | (permissions & 0o7777);
        let ino = self.shared.with(|volume| {
            let new = NewInode {
                mode,
                uid: 0,
                gid: 0,
                rdev,
                now,
            };
            let ino = volume.create(dir, name, &new)?;
            if let Some(target) = &target {
                volume.set_symlink(ino, target)?;
            }
            Ok(ino)
        })?;
        let mut volume = self.shared.lock();
        self.refresh(&mut volume)?;
        drop(volume);
        Ok(Node::get(&self.shared, ino)? as Arc<dyn Inode>)
    }

    fn link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<()> {
        self.require_dir()?;
        self.shared.commit_if_heavy()?;
        self.shared.shape_changed();
        let target = target
            .clone()
            .into_any()
            .downcast::<Node<D>>()
            .map_err(|_| Errno::EXDEV)?;
        if target.kind == FileType::Directory {
            return Err(Errno::EPERM);
        }
        let (dir, ino, now) = (self.ino, target.ino, self.shared.now());
        self.shared
            .with(|volume| volume.link(dir, name, ino, now))?;
        let mut volume = self.shared.lock();
        self.refresh(&mut volume)?;
        target.refresh(&mut volume)
    }

    fn unlink(&self, name: &[u8]) -> Result<()> {
        self.remove(name, false)
    }

    fn rmdir(&self, name: &[u8]) -> Result<()> {
        self.remove(name, true)
    }

    fn rename(
        &self,
        old: &[u8],
        new_parent: &Arc<dyn Inode>,
        new: &[u8],
        replace: bool,
    ) -> Result<()> {
        self.require_dir()?;
        self.shared.commit_if_heavy()?;
        self.shared.shape_changed();
        // A file the rename replaces may be freed.
        *self.shared.freed.lock() = true;
        let parent = new_parent
            .clone()
            .into_any()
            .downcast::<Node<D>>()
            .map_err(|_| Errno::EXDEV)?;
        let (from, to, now) = (self.ino, parent.ino, self.shared.now());
        let moved = self.shared.with(|volume| {
            let (_, kind) = volume.lookup(from, old)?.ok_or(WriteError::NotFound)?;
            if let Some((victim, victim_kind)) = volume.lookup(to, new)? {
                if !replace {
                    return Err(WriteError::Exists);
                }
                check_replaceable(volume, kind, victim, victim_kind)?;
            }
            let moved = volume.lookup(from, old)?.map(|(ino, _)| ino);
            volume.rename(from, old, to, new, now)?;
            Ok(moved)
        })?;
        let mut volume = self.shared.lock();
        self.refresh(&mut volume)?;
        parent.refresh(&mut volume)?;
        // Whatever was replaced, and whatever moved, may be open somewhere.
        let live: Vec<Arc<Node<D>>> = self
            .shared
            .nodes
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for node in live.iter().filter(|node| Some(node.ino) == moved) {
            node.refresh(&mut volume)?;
        }
        Ok(())
    }

    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        self.require_dir()?;
        let dir = self.ino;
        let from = cursor.max(FIRST_CURSOR);
        let entries = self.shared.with(|volume| volume.read_dir(dir, from))?;
        for (index, ino, kind, name) in entries {
            let Some(kind) = crate::entry_kind(kind) else {
                return Err(Errno::EIO);
            };
            let listed = DirEntry {
                ino,
                kind,
                name: &name,
                next: index.saturating_add(1),
            };
            if !emit(listed) {
                break;
            }
        }
        Ok(())
    }

    fn read_link(&self) -> Result<Vec<u8>> {
        if self.kind != FileType::Symlink {
            return Err(Errno::EINVAL);
        }
        let size = self.meta.lock().size;
        if size == 0 || size > crate::MAX_LINK {
            return Err(Errno::EIO);
        }
        let len = usize::try_from(size).map_err(|_| Errno::EIO)?;
        let mut target = vec![0u8; len];
        let ino = self.ino;
        let read = self
            .shared
            .with(|volume| volume.read_file(ino, 0, &mut target))?;
        if read == len {
            Ok(target)
        } else {
            Err(Errno::EIO)
        }
    }

    /// Write this file back, and make where its bytes are durable.
    ///
    /// The bytes themselves reached the disk as they were written back; what
    /// `fsync` must add is a durable record of where they are. While nothing
    /// has changed the shape of the tree since the last commit, that record
    /// is one log entry for this inode and a superblock, which is far less
    /// than a commit. Otherwise the shape must be committed, because a log
    /// carries no names; Linux falls back the same way.
    fn fsync(&self, data_only: bool) -> Result<()> {
        let _ = data_only;
        let mut volume = self.shared.lock();
        self.write_back(&mut volume)?;
        if *self.shared.structural.lock() {
            *self.shared.pending.lock() = 0;
            *self.shared.structural.lock() = false;
            return volume.commit().map_err(errno);
        }
        volume.log_inode(self.ino).map_err(errno)?;
        volume.commit_log().map_err(errno)
    }

    /// Held from here, since a mapping may write without a word to the mount;
    /// the writeback that finds the object mapped by no one lets go. The
    /// object is taken before the hold, so a writeback that comes between
    /// sees it out and keeps the file.
    fn mapping(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        let object = self.pages().ok().and_then(|pages| pages.object());
        if object.is_some() {
            let _dirty = self.dirty.lock();
            self.hold();
        }
        object
    }
}

impl<D: WriteHandle> Node<D> {
    /// `unlink` and `rmdir`, which differ only in what they accept.
    fn remove(&self, name: &[u8], directory: bool) -> Result<()> {
        self.require_dir()?;
        self.shared.commit_if_heavy()?;
        self.shared.shape_changed();
        *self.shared.freed.lock() = true;
        let (dir, now) = (self.ino, self.shared.now());
        let gone = self.shared.with(|volume| {
            let (ino, kind) = volume.lookup(dir, name)?.ok_or(WriteError::NotFound)?;
            let is_dir = kind == ferrix_btrfs::items::FT_DIR;
            if is_dir != directory {
                return Err(if directory {
                    WriteError::NotDir
                } else {
                    WriteError::IsDir
                });
            }
            if is_dir && !volume.dir_is_empty(ino)? {
                return Err(WriteError::NotEmpty);
            }
            let _ = volume.unlink(dir, name, now)?;
            Ok(ino)
        })?;
        let mut volume = self.shared.lock();
        self.refresh(&mut volume)?;
        let live = self.shared.nodes.lock().get(&gone).and_then(Weak::upgrade);
        match live {
            // Something still holds it: it keeps its orphan item until the
            // last reference goes.
            Some(node) => node.refresh(&mut volume),
            None => volume.evict(gone).map_err(errno),
        }
    }
}

/// Whether `victim` may be replaced by a rename of something of `kind`.
fn check_replaceable<D: WriteDevice>(
    volume: &mut WriteVolume<D>,
    kind: u8,
    victim: u64,
    victim_kind: u8,
) -> core::result::Result<(), WriteError> {
    let moving_dir = kind == ferrix_btrfs::items::FT_DIR;
    let victim_dir = victim_kind == ferrix_btrfs::items::FT_DIR;
    if moving_dir && !victim_dir {
        return Err(WriteError::NotDir);
    }
    if !moving_dir && victim_dir {
        return Err(WriteError::IsDir);
    }
    if victim_dir && !volume.dir_is_empty(victim)? {
        return Err(WriteError::NotEmpty);
    }
    Ok(())
}

/// The runs of consecutive pages in `dirty`, as `(first, count)`, none longer
/// than `most` pages.
///
/// The cap is what keeps a writeback's buffer bounded: a file written whole
/// is one run of every page it has, and a kernel with a few megabytes of heap
/// cannot hold a copy of it.
fn runs(dirty: &BTreeSet<u64>, most: usize) -> Vec<(u64, usize)> {
    let mut out: Vec<(u64, usize)> = Vec::new();
    for &page in dirty {
        match out.last_mut() {
            Some((first, count))
                if first.saturating_add(*count as u64) == page && *count < most.max(1) =>
            {
                *count += 1;
            }
            _ => out.push((page, 1)),
        }
    }
    out
}

/// A VFS timestamp as btrfs stores it.
fn time_in(time: Timespec) -> BtrfsTime {
    BtrfsTime {
        sec: time.tv_sec.cast_unsigned(),
        nsec: u32::try_from(time.tv_nsec).unwrap_or(0),
    }
}

/// What an operation on a mounted writable volume reports.
fn errno(error: WriteError) -> Errno {
    match error {
        WriteError::Exists => Errno::EEXIST,
        WriteError::NotFound => Errno::ENOENT,
        WriteError::NotEmpty => Errno::ENOTEMPTY,
        WriteError::NotDir => Errno::ENOTDIR,
        WriteError::IsDir => Errno::EISDIR,
        WriteError::NameTooLong => Errno::ENAMETOOLONG,
        WriteError::InvalidName => Errno::EINVAL,
        WriteError::TooManyLinks => Errno::EMLINK,
        WriteError::NoSpace => Errno::ENOSPC,
        WriteError::ItemTooLarge => Errno::ENAMETOOLONG,
        WriteError::Unsupported(_) | WriteError::ReadOnly => Errno::EROFS,
        WriteError::OutOfMemory | WriteError::Volume(BtrfsError::OutOfMemory) => Errno::ENOMEM,
        // Damage, a failed write, or a transaction that gave up: the volume
        // is no longer to be trusted, and every answer is EIO.
        _ => Errno::EIO,
    }
}

/// The error a writable mount reports for a volume it will not take.
fn mount_errno(error: WriteError) -> Errno {
    match error {
        WriteError::Unsupported(
            Unsupported::Subvolumes
            | Unsupported::Quotas
            | Unsupported::Log
            | Unsupported::NoFreeSpaceTree
            | Unsupported::NotSkinny
            | Unsupported::NoHoles
            | Unsupported::MixedGroups
            | Unsupported::CompatRo(_)
            | Unsupported::SharedBlock,
        ) => Errno::EROFS,
        WriteError::Unsupported(_) => Errno::EINVAL,
        WriteError::Volume(error) => crate::mount_errno(error),
        WriteError::OutOfMemory => Errno::ENOMEM,
        _ => Errno::EIO,
    }
}
