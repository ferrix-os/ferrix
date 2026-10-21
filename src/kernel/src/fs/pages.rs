//! File contents as VMO pages.
//!
//! tmpfs keeps a regular file's bytes in whatever [`Pages`] it is given, and
//! the kernel gives it a VMO. That is the unification `docs/ARCHITECTURE.md`
//! §3 calls load-bearing: the object a `read` copies out of is the object a
//! future `mmap` of the same file maps, so a page is never held twice and a
//! write through one is seen through the other.
//!
//! The same store is the page cache of a filesystem on a disk. Made over a
//! [`PageSource`] by [`Storage::allocate_with`], it fills a page from the file
//! the first time a read reaches it, as `src/lib/fs/vfs`'s `HeapPages` does and its
//! host tests pin: runs of at most [`MAX_FILL_RUN`] missing pages, frames
//! allocated and zeroed before the source is called with no lock held, only
//! the pages still absent kept, and a fill that fails or claims more than it
//! was asked for keeping nothing.
//!
//! A fault through a mapping of the file fills the same way. It reaches the
//! VMO rather than this store, so the VMO carries the source too, as its
//! `Filler`, and the address space asks it for the page before it takes its
//! own lock; a page it did not fill would be committed as zeros. Until that
//! was so, a program run from btrfs mapped its libraries' unread pages as
//! zeros, and glibc's linker failed on the first one it read.
//!
//! # Sized for the largest file, paid for by the page
//!
//! Each file's VMO is created at [`MAX_FILE_SIZE`], which costs nothing: the
//! page list is sparse and a page with no frame is simply absent. The file's
//! real length is the filesystem's to keep, not the object's.
//!
//! # Who serialises what
//!
//! A read copies each page under the VMO's own lock, so a truncation cannot
//! release a frame part-way through the copy: a filesystem on a disk holds no
//! lock of its own across a read, because the read may wait for the disk.
//! A write commits and copies each page under that lock too
//! ([`Vmo::commit_and_write`]), and a truncation zeroes the page its cut
//! falls in under it ([`Vmo::zero_from`]): reclaim takes a writable mount's
//! clean pages, so the lock, and not the filesystem, is what keeps a frame
//! the object's while it is written. What the filesystem serialises is the
//! file's bytes: tmpfs holds the inode's lock across a write and a
//! truncation, and btrfs holds the pages a write touches still against
//! reclaim from the copy to their dirty mark, so that no written page is
//! dropped before it is marked. A file mapping reaches the same VMO without
//! either, and the release path takes the care a copy-on-write fault already
//! takes.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_frame::Frame;
use ferrix_kmem::{Charge, arc_footprint, boxed_footprint};
use ferrix_vfs::tmpfs::{PageSource, Pages, Storage};
use ferrix_vfs::{Errno, Result};

use crate::mm;
use crate::object::quota::{self, Counter};
use crate::sync::SpinLock;
use crate::user::vmo::{Filler, Vmo, VmoError};

/// The largest a tmpfs file may grow: one tebibyte.
///
/// A policy, not a limit of the structure. It keeps every file size and offset
/// comfortably inside what both `off_t` and a page index can say on all three
/// architectures.
pub(crate) const MAX_FILE_SIZE: u64 = 1 << 40;

/// The most pages a store asks its source for in one call: the run `src/lib/fs/vfs`'s
/// `HeapPages` asks for, so that a read and a fault ask a filesystem the same
/// way. A compressed btrfs extent is at most 128 KiB, which is 32 pages.
pub(crate) const MAX_FILL_RUN: usize = 32;

/// How many times a read fills a page that reclaim took again before it reads
/// the page from the source directly.
const REFILLS: usize = 8;

/// [`PAGE_SIZE`] as a length.
const PAGE_BYTES: usize = PAGE_SIZE as usize;

/// Hands a filesystem a VMO per file: empty for tmpfs, over a [`PageSource`]
/// for a filesystem whose files live on a disk.
#[derive(Debug)]
pub(crate) struct VmoStorage;

impl Storage for VmoStorage {
    fn allocate(&self) -> Result<Box<dyn Pages>> {
        Ok(Box::new(VmoPages::new(None)?))
    }

    fn allocate_with(&self, source: Arc<dyn PageSource>) -> Result<Box<dyn Pages>> {
        Ok(Box::new(VmoPages::new(Some(source))?))
    }

    fn max_file_size(&self) -> u64 {
        MAX_FILE_SIZE
    }

    /// The scheduler's: a task waiting for a tmpfs file's I/O lock behind a
    /// write that commits frames, a read throttled at `memory.high` or a
    /// truncation's shootdown sleeps, as every kernel `SleepLock` does.
    fn parker(&self) -> &dyn ferrix_sync::Parker {
        &crate::sync::SchedParker
    }

    /// Half of memory in total, which is Linux's default `size=` for a tmpfs
    /// mounted without one, and whatever the allocator has free up to that.
    ///
    /// Free is the machine's rather than this filesystem's own remainder,
    /// because every tmpfs draws on the one allocator and nothing charges a
    /// page to the instance that committed it: `df` sees what a write could
    /// actually get. Nothing enforces the half yet -- a write fails when
    /// frames run out, not at the size reported -- which is the part Linux's
    /// `size=` adds and this does not.
    fn capacity(&self) -> (u64, u64) {
        let total = mm::managed_frames() / 2;
        (total, mm::free_frames().min(total))
    }
}

/// One file's contents.
#[derive(Debug)]
pub(crate) struct VmoPages {
    vmo: Arc<Vmo>,
    /// Where a page the VMO does not hold comes from; `None` for tmpfs, whose
    /// missing pages are zeros. The VMO holds it too, as its [`Filler`], so
    /// that a fault through a mapping fills a page the way a read does.
    fill: Option<Arc<Fill>>,
    /// The heap the store holds -- itself, the object, the filler -- charged
    /// to the job that made the file, or first opened it from a disk
    /// (F-37). The object's pages are frames, charged to whoever commits
    /// each.
    _charge: Charge,
}

/// How a file on a disk fills its object: shared by the store, for reads and
/// partial writes, and by the object, for faults.
#[derive(Debug)]
struct Fill {
    source: Arc<dyn PageSource>,
    /// Bytes from here on are not the source's: a source knows the file as it
    /// was, not as it has been cut. Starts past every offset;
    /// [`Pages::discard_from`] lowers it and nothing raises it, so a file cut
    /// and grown again reads zeros past the cut. A fill is kept under this
    /// lock, which a truncation takes to lower it before it takes pages away,
    /// so a fill that raced the truncation is either cut back here or taken
    /// away there.
    sourced_below: SpinLock<u64>,
    /// Where the last read began and ended, in bytes: a read that begins
    /// where it ended, in a later page than it began, continues a
    /// sequential stream and reads ahead (see `VmoPages::read`).
    last_read: (AtomicU64, AtomicU64),
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

/// The direct-map address of byte `within` of `frame`.
fn byte_of(frame: Frame, within: usize) -> u64 {
    mm::direct_map(frame * PAGE_SIZE) + within as u64
}

/// What a program is told when the object refuses: a page past the largest
/// file is `EFBIG`, and memory running out under a write is what Linux's tmpfs
/// says, `ENOSPC`.
fn refused(error: VmoError) -> Errno {
    match error {
        VmoError::OutOfRange { .. } => Errno::EFBIG,
        VmoError::OutOfMemory => Errno::ENOSPC,
    }
}

/// A frame for a page of the cache, reclaiming when there is none: first
/// inside the job whose `memory.max` refused it, then anywhere if the machine
/// itself is out (`user::cache`), tried again while a reclaim gives some
/// back, at most [`RECLAIMS`] times, as a write is ([`write_reclaiming`]).
/// One reclaim was not enough: at the machine's floor, with clean cache to
/// spare, the frames it gave back went to another processor's allocation
/// before the retry, and the read that wanted the page answered `ENOMEM` --
/// which Wine turns into `ERROR_GEN_FAILURE`, and which stopped Endfield's
/// launcher's 7-Zip half-way through 68 GB (2026-10-15). Called with no lock
/// held, as a fill is.
fn allocate_reclaiming() -> Option<Frame> {
    let mut tries = 0;
    loop {
        if let Some(frame) = mm::allocate_user_frame() {
            return Some(frame);
        }
        if tries == RECLAIMS {
            return None;
        }
        tries += 1;
        let group = crate::sched::running_group();
        let scope =
            quota::at_limit(group, quota::Resource::Memory, PAGE_SIZE).unwrap_or(quota::NONE);
        if crate::user::cache::reclaim(scope, reclaim_want(scope)).stolen == 0 {
            // Nothing to give back; another task may still have let a frame
            // go since the first try.
            return mm::allocate_user_frame();
        }
    }
}

/// Write `bytes` into page `index` of `vmo` at `within`, committing the page
/// first if it is absent ([`Vmo::commit_and_write`]), and reclaiming when
/// there is no frame for it as a fill does ([`allocate_reclaiming`]): clean
/// cache inside the job whose `memory.max` refused it, or anywhere when the
/// machine is out, tried again while a reclaim gives some back, at most
/// [`RECLAIMS`] times. Without it a write found the machine full of
/// clean cache and answered `ENOSPC` (docs/WINE.md §10). Called with no lock
/// held, as a write into a store is.
///
/// # Errors
///
/// As [`refused`]: `ENOSPC` when nothing more could be given back.
fn write_reclaiming(vmo: &Vmo, index: u64, within: usize, bytes: &[u8]) -> Result<()> {
    let mut tries = 0;
    loop {
        match vmo.commit_and_write(index, within, bytes) {
            Ok(()) => return Ok(()),
            Err(VmoError::OutOfMemory) if tries < RECLAIMS => {
                tries += 1;
                let group = crate::sched::running_group();
                let scope = quota::at_limit(group, quota::Resource::Memory, PAGE_SIZE)
                    .unwrap_or(quota::NONE);
                if crate::user::cache::reclaim(scope, reclaim_want(scope)).stolen == 0 {
                    return Err(Errno::ENOSPC);
                }
            }
            Err(error) => return Err(refused(error)),
        }
    }
}

/// How many pages one reclaim for a frame asks for: a fill's run, and, when
/// the machine itself is out (`scope` is [`quota::NONE`]), the frames the
/// free count lacks of the watermark besides.
///
/// A program's frame is refused while fewer than the watermark are free
/// (`mm::watermark`), and the kernel's own allocations go on below it. At the
/// machine's floor the free count sat under the watermark by more than a
/// run, so each of the eight reclaims of a run gave pages back and each retry
/// still found no frame: a write answered `ENOSPC` and a fill `ENOMEM` with
/// hundreds of megabytes of clean cache left, and Endfield's 7-Zip was told
/// "disk full" with 400 GB free on the N150 box (2026-10-17,
/// `docs/BTRFS-PRESSURE.md`). Asking for the shortfall too lifts the free
/// count over the watermark, so the retry has a frame unless another task
/// took the run on top of it.
fn reclaim_want(scope: u32) -> u64 {
    let run = MAX_FILL_RUN as u64;
    if scope != quota::NONE {
        return run;
    }
    // The floor a program's frame is held to: the watermark beyond the
    // reservations' outstanding promise (`L.mm.82`, `docs/microkernel/K12.md`).
    let short = mm::program_floor()
        .saturating_add(1)
        .saturating_sub(mm::free_frames());
    run.saturating_add(short)
}

/// How many reclaims one page of a write, or one frame of a fill, tries
/// before it answers `ENOSPC` (a write) or `ENOMEM` (a fill): each gives back
/// up to a fill's run, or, when the machine itself is out, a run and what the
/// free count lacks of the watermark ([`reclaim_want`]); another task may
/// take the room between a reclaim and the retry.
const RECLAIMS: u32 = 8;

/// Give back frames no page kept.
fn release_all(frames: impl IntoIterator<Item = Frame>) {
    for frame in frames {
        let _ = mm::release_frame(frame);
    }
}

impl VmoPages {
    fn new(source: Option<Arc<dyn PageSource>>) -> Result<VmoPages> {
        let filled = if source.is_some() {
            arc_footprint::<Fill>()
        } else {
            0
        };
        let charge = Charge::bytes(
            boxed_footprint::<VmoPages>()
                .saturating_add(arc_footprint::<Vmo>())
                .saturating_add(crate::user::cache::ENTRY_HEAP)
                .saturating_add(filled),
        )
        .map_err(|_| Errno::ENOMEM)?;
        let fill = source.map(|source| {
            Arc::new(Fill {
                source,
                sourced_below: SpinLock::new(u64::MAX),
                last_read: (AtomicU64::new(u64::MAX), AtomicU64::new(u64::MAX)),
            })
        });
        let filler = fill.clone().map(|fill| fill as Arc<dyn Filler>);
        Ok(VmoPages {
            vmo: Vmo::new_filled(MAX_FILE_SIZE / PAGE_SIZE, filler).map_err(|_| Errno::ENOMEM)?,
            fill,
            _charge: charge,
        })
    }

    /// Page `index` is absent and its source's to fill.
    fn wants_fill(&self, index: u64) -> bool {
        self.fill
            .as_ref()
            .is_some_and(|fill| fill.wants_fill(&self.vmo, index))
    }

    /// Fill the run of absent pages from `index`, no further than `last`.
    ///
    /// # Errors
    ///
    /// As [`Fill::fill_run`]; `EIO` when there is no source.
    fn fill_from(&self, index: u64, last: u64) -> Result<()> {
        let fill = self.fill.as_deref().ok_or(Errno::EIO)?;
        fill.fill_run(&self.vmo, index, fill.run(&self.vmo, index, last))
    }
}

impl Filler for Fill {
    fn reclaimable(&self) -> bool {
        self.source.reclaimable()
    }

    /// The source's own answer: a writable file keeps the pages it wrote.
    fn reclaim_clean(&self, pages: &mut Vec<u64>, drop: &mut dyn FnMut(&[u64]) -> u64) -> u64 {
        self.source.reclaim_clean(pages, drop)
    }

    fn dirty_pages(&self) -> u64 {
        self.source.dirty_pages()
    }

    fn is_sourced(&self, index: u64) -> bool {
        self.sourced(index)
    }

    /// Read ahead as far as a read asks at most, [`MAX_FILL_RUN`] pages: a
    /// program's libraries are faulted in a page at a time, and a disk read
    /// per page is what would make that slow.
    fn fill(&self, vmo: &Vmo, index: u64) -> Result<()> {
        if !self.wants_fill(vmo, index) {
            super::seam::served(1);
            return Ok(());
        }
        let last = index.saturating_add(MAX_FILL_RUN as u64 - 1);
        self.fill_run(vmo, index, self.run(vmo, index, last))?;
        // A fault that had to read the disk: Linux's `pgmajfault`.
        if self.source.reads_disk() {
            quota::count(crate::sched::running_group(), Counter::MajorFaults, 1);
        }
        Ok(())
    }

    /// The one page, and no run after it.
    fn fill_one(&self, vmo: &Vmo, index: u64) -> Result<()> {
        if !self.wants_fill(vmo, index) {
            super::seam::served(1);
            return Ok(());
        }
        self.fill_run(vmo, index, 1)
    }
}

impl Fill {
    /// Whether page `index`, if the VMO does not hold it, is the source's to
    /// fill.
    fn sourced(&self, index: u64) -> bool {
        index
            .checked_mul(PAGE_SIZE)
            .is_some_and(|start| start < *self.sourced_below.lock())
    }

    /// Page `index` is absent from `vmo` and the source's to fill.
    fn wants_fill(&self, vmo: &Vmo, index: u64) -> bool {
        vmo.page(index).is_none() && self.sourced(index)
    }

    /// How many pages to ask for from `first`, which wants filling: the run of
    /// such pages no further than `last`, at most [`MAX_FILL_RUN`].
    fn run(&self, vmo: &Vmo, first: u64, last: u64) -> usize {
        let mut count = 1;
        while count < MAX_FILL_RUN {
            match first.checked_add(count as u64) {
                Some(index) if index <= last && self.wants_fill(vmo, index) => count += 1,
                _ => break,
            }
        }
        count
    }

    /// Fill `count` pages from `first` from the source, and keep each one the
    /// VMO still lacks and the file still has.
    ///
    /// The frames are allocated and zeroed first, and the source called with
    /// no lock held, since it may wait for a disk. A source that fails, or
    /// claims no page or more than it was asked for, keeps nothing.
    ///
    /// # Errors
    ///
    /// `ENOMEM` for frames, `EIO` for a source that lied, and the source's own
    /// error, which for a page that does not verify is `EIO`.
    fn fill_run(&self, vmo: &Vmo, first: u64, count: usize) -> Result<()> {
        let source = self.source.as_ref();
        let mut frames: Vec<Frame> = Vec::new();
        frames.try_reserve_exact(count).map_err(|_| Errno::ENOMEM)?;
        for _ in 0..count {
            let Some(frame) = allocate_reclaiming() else {
                // Room for part of the run is a fill of part of it, which a
                // source may always answer (`PageSource::fill_range`): a job
                // limited below a run's size reads on in pieces.
                if frames.is_empty() {
                    return Err(Errno::ENOMEM);
                }
                break;
            };
            // Zeroed, so a source that writes less than a page hands no
            // earlier owner's bytes to whoever reads it.
            mm::zero_frame(frame);
            frames.push(frame);
        }
        let count = frames.len();

        let filled = {
            let mut pages: Vec<&mut [u8]> = Vec::new();
            if pages.try_reserve_exact(count).is_err() {
                release_all(frames);
                return Err(Errno::ENOMEM);
            }
            for &frame in &frames {
                // SAFETY: each frame was just allocated here and is in no
                // object yet, so these are the only references to its bytes;
                // the direct map covers all of RAM, and each slice is exactly
                // one page. They end with this block, before the frames are
                // inserted or released.
                pages.push(unsafe {
                    core::slice::from_raw_parts_mut(byte_of(frame, 0) as *mut u8, PAGE_BYTES)
                });
            }
            source.fill_range(first, &mut pages)
        };
        let got = match filled {
            Ok(got) if (1..=count).contains(&got) => {
                super::seam::filled(got as u64, source.reads_disk());
                got
            }
            Ok(_) => {
                release_all(frames);
                return Err(Errno::EIO);
            }
            Err(error) => {
                release_all(frames);
                return Err(error);
            }
        };

        let bound = self.sourced_below.lock();
        for (index, frame) in (first..).zip(frames) {
            let start = index.saturating_mul(PAGE_SIZE);
            let keep = index < first.saturating_add(got as u64) && start < *bound;
            if keep
                && let Ok(valid) = usize::try_from(*bound - start)
                && valid < PAGE_BYTES
            {
                // SAFETY: as above, the frame is still this call's alone.
                unsafe {
                    core::ptr::write_bytes(byte_of(frame, valid) as *mut u8, 0, PAGE_BYTES - valid);
                }
            }
            if !(keep && vmo.insert_absent(index, frame)) {
                let _ = mm::release_frame(frame);
            }
        }
        Ok(())
    }
}

impl VmoPages {
    /// `out.len()` bytes of page `index` from `within`, filling the page
    /// first if it is the source's to fill. A fill runs to page `ahead`, or,
    /// if that run fails, to page `own`, the read's own last page: a run
    /// read ahead into a page that does not read must not fail the pages
    /// before it, which the read asked for and which read well.
    ///
    /// Under the object's lock a page a truncation took away since the fill
    /// reads as the zeros the file now has there. A page that reclaim took
    /// since the fill (`user::cache`) is no hole but the source's, and is
    /// filled again; after [`REFILLS`] of those it is read from the source
    /// directly, which nothing can race.
    fn read_piece(
        &self,
        index: u64,
        within: usize,
        out: &mut [u8],
        (ahead, own): (u64, u64),
    ) -> Result<()> {
        for _ in 0..REFILLS {
            if self.wants_fill(index) {
                match self.fill_from(index, ahead) {
                    Ok(()) => {}
                    Err(_) if own < ahead => self.fill_from(index, own)?,
                    Err(error) => return Err(error),
                }
            } else {
                super::seam::served(1);
            }
            if self
                .vmo
                .read_present(index, within, out)
                .map_err(|_| Errno::EIO)?
            {
                return Ok(());
            }
            if !self.vmo.sourced(index) {
                out.fill(0);
                return Ok(());
            }
        }
        let fill = self.fill.as_deref().ok_or(Errno::EIO)?;
        let mut page = Vec::new();
        page.try_reserve_exact(PAGE_BYTES)
            .map_err(|_| Errno::ENOMEM)?;
        page.resize(PAGE_BYTES, 0_u8);
        fill.source.fill(index, &mut page)?;
        let from = page.get(within..within + out.len()).ok_or(Errno::EIO)?;
        out.copy_from_slice(from);
        Ok(())
    }
}

impl VmoPages {
    /// The last page a read of `len` bytes at `offset` fills to: its own
    /// last page, or on a sequential stream (see the doc of `read` below) at
    /// least [`MAX_FILL_RUN`] pages from its first, never past the file's
    /// end. Notes the read as the stream's last.
    fn read_ahead_to(&self, offset: u64, len: usize) -> Result<u64> {
        let (first, _, _) = piece(offset, 0, len)?;
        let (last, _, _) = piece(offset, len - 1, len)?;
        let Some(fill) = self.fill.as_deref() else {
            return Ok(last);
        };
        let (began, ended) = &fill.last_read;
        let was_begun = began.swap(offset, Ordering::Relaxed);
        let was_ended = ended.swap(offset.saturating_add(len as u64), Ordering::Relaxed);
        if offset != was_ended || first <= was_begun / PAGE_SIZE {
            return Ok(last);
        }
        let mut ahead = last.max(first.saturating_add(MAX_FILL_RUN as u64 - 1));
        // Never past the end of the file, whose pages nobody would read.
        while ahead > last && self.vmo.past_file_end(ahead) {
            ahead -= 1;
        }
        Ok(ahead)
    }
}

impl Pages for VmoPages {
    /// A fill reads to the end of the read, and on a sequential stream at
    /// least [`MAX_FILL_RUN`] pages ahead, as a fault does: the kernel hands
    /// a filesystem a large `read` a page at a time, and filling only the
    /// page asked for made each page of a file read one disk request (a
    /// 4 KiB round trip to the disk's driver for each page 7-Zip read of its
    /// archive, docs/WINE.md §12). A read is sequential when it begins where
    /// the last one ended, in a later page than the last one began: a
    /// program's loader reading its headers out of the first page reads no
    /// further than it asks.
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let len = buf.len();
        let (ahead, own) = if len == 0 {
            (0, 0)
        } else {
            let (own, _, _) = piece(offset, len - 1, len)?;
            (self.read_ahead_to(offset, len)?, own)
        };
        let mut done = 0;
        while done < len {
            let (index, within, take) = piece(offset, done, len)?;
            let out = buf.get_mut(done..done + take).ok_or(Errno::EIO)?;
            self.read_piece(index, within, out, (ahead, own))?;
            done += take;
        }
        // The pages are in the object and copied out, so a job over its
        // `memory.high` can give them back; not before, or a fill would be
        // taken again as it was made.
        crate::object::oom::throttle();
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        let len = data.len();
        let mut done = 0;
        while done < len {
            let (index, within, take) = piece(offset, done, len)?;
            // A page this write covers only partly keeps the file's other
            // bytes, so it comes from the source first.
            if take < PAGE_BYTES && self.wants_fill(index) {
                self.fill_from(index, index)?;
            }
            let bytes = data.get(done..done + take).ok_or(Errno::EIO)?;
            // Committed and copied under the object's lock: nothing gives
            // the frame back during the copy, reclaim included, whatever the
            // filesystem holds still. A frame committed fresh was zeroed, so
            // the bytes around the ones written are zeros rather than an old
            // owner's data.
            write_reclaiming(&self.vmo, index, within, bytes)?;
            done += take;
        }
        Ok(())
    }

    fn discard_from(&self, offset: u64) {
        // The bound first, under the lock a fill is kept under, then the
        // pages: a fill that kept a page past the cut before this is taken
        // away below, and one after it keeps nothing past the cut.
        if let Some(fill) = &self.fill {
            let mut bound = fill.sourced_below.lock();
            *bound = (*bound).min(offset);
        }
        let _ = self.vmo.decommit_from(offset.div_ceil(PAGE_SIZE));
        // A private mapping's copies of pages past the cut go too, and so does
        // what one shows of a hole it wrote, which no page taken above names.
        self.vmo.cut_mappings(offset.div_ceil(PAGE_SIZE));
        let Ok(within) = usize::try_from(offset % PAGE_SIZE) else {
            return;
        };
        if within == 0 {
            return;
        }
        // Under the object's lock, so reclaim cannot give the frame back
        // while it is zeroed.
        let _ = self.vmo.zero_from(offset / PAGE_SIZE, within);
    }

    fn committed_bytes(&self) -> u64 {
        (self.vmo.committed() as u64).saturating_mul(PAGE_SIZE)
    }

    /// Bound a mapping's faults by the file's new length, before any page a
    /// cut discards goes.
    fn resize(&self, len: u64) {
        self.vmo.set_file_len(len);
    }

    /// The VMO itself, which a mapping of the file maps.
    fn object(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(Arc::clone(&self.vmo) as Arc<dyn Any + Send + Sync>)
    }

    /// What the VMO says shared mappings may have written, and whether any
    /// other reference to it is out: a mapping's, or the one `mmap` holds
    /// between asking for the object and counting its mapping in.
    fn mapped_writes(&self) -> (Vec<u64>, bool) {
        // With no memory for the list, none this time: the object keeps its
        // mark, and the next call reports them.
        let written = self.vmo.take_mapped_writes().unwrap_or_default();
        (written, Arc::strong_count(&self.vmo) > 1)
    }
}
