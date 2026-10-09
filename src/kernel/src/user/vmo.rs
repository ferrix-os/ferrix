//! Pageable memory objects: pages, not mappings.
//!
//! Stage 6 of `docs/ROADMAP.md`, and `docs/ARCHITECTURE.md` §3 calls the VMO
//! unification the load-bearing idea of the object model. One page list serves
//! anonymous memory, the page cache and driver DMA buffers, so that a block
//! driver reading into a cached page is filling the object the cache already
//! holds rather than copying into a buffer and then into the cache.
//!
//! Only the anonymous kind exists here. File-backed VMOs want a page cache and
//! a filesystem, which are stage 8 and stage 11; DMA VMOs want an IOMMU
//! domain, which is stage 10. What matters now is that the *shape* is the one
//! those will extend — a sparse list of frames indexed by page number, with
//! commit-on-demand — rather than a shape they would have to unpick.
//!
//! # Why the list is sparse
//!
//! An anonymous VMO is created at its full size and almost never filled. A
//! process maps eight megabytes of stack and touches three pages of it; a
//! `rustc` invocation reserves far more than it writes. Storing a frame per
//! page up front would allocate the whole reservation at `mmap` time, which is
//! the opposite of what lazy anonymous memory means, so a page with no frame
//! is simply absent and the fault handler commits it on first touch.
//!
//! # Who maps an object, and how a page leaves it
//!
//! An object does not know where it is mapped, but it knows *who* maps it:
//! every address space that has it in its object table, under the id that
//! space knows it by, in [`Vmo::attach`]'s list. A space holding the object
//! under two ids is in the list twice. The regions are not recorded — they
//! split and shrink under `mprotect` and `munmap`, and the space's own map
//! answers which of them name the object at the moment of asking.
//!
//! That list is what makes it safe for an object to take a frame away while
//! somebody maps it. Every such change — [`Vmo::decommit_range`],
//! [`Vmo::decommit_from`], [`Vmo::replace`], [`Vmo::adopt_pages`], and the copy
//! [`Vmo::write_page`] and [`Vmo::hold`] make of a page a `fork` left shared —
//! goes in three phases, and the order is the whole point:
//!
//! 1. **Under the pages lock**, the frames come out of the list into a
//!    [`Retired`]. A held page is skipped here, before anything is taken, so
//!    it is never in what the next phase invalidates. The [`Retired`]'s room
//!    is had before the first frame comes out, so that running out of memory
//!    refuses the change rather than losing a frame (finding F-23).
//! 2. **With no VMO lock held**, every space in the list is asked to
//!    [`AddressSpace::forget_pages`]: it takes its translations of those pages
//!    down under its own lock and says which processors may still have them
//!    cached. One scoped shootdown goes to all of them together, and returns
//!    once every processor has answered.
//! 3. **Only then** are the frames released.
//!
//! A frame released before phase two ends is a frame some processor can
//! still write through, and the allocator hands it to somebody else: two
//! owners of one page, and the symptom turns up in whichever writes second.
//!
//! None of the three needs memory once phase one has its room. Phase two
//! lists the spaces to ask; with no memory for the list it asks them one at a
//! time, each with a shootdown of its own. And [`Vmo::decommit_range`], which
//! cannot refuse, takes [`CHUNK`] pages at a time from the stack when there
//! is no memory for the whole list.
//!
//! # Lock order
//!
//! An address space's lock, then an object's mapper list, then its pages. An
//! address space's lock is **never** taken while an object's lock is held, and
//! no shootdown waits while any of them is: phase two runs with all three
//! free, and a caller already holding a space's lock -- the copy-on-write fault,
//! `mremap`, `munmap`'s give-back -- does phase one under it and hands the rest
//! to [`Vmo::retire`] once it has let go, naming itself so that phase two does
//! not come back for its lock.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::fallible::AllocError;
use crate::sync::SpinLock;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_frame::Frame;
use ferrix_sched::CpuSet;

use crate::mm;
use crate::object::quota::{Charge, Resource};
use crate::smp::{self, TlbPages};
use crate::user::space::AddressSpace;

/// Why a VMO operation was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VmoError {
    /// The page index is past the end of the object.
    OutOfRange {
        /// The index asked for.
        index: u64,
        /// How many pages the object has.
        pages: u64,
    },
    /// No frame was available to commit.
    OutOfMemory,
}

/// Why [`Vmo::commit_run`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunError {
    /// A file's object: its absent pages are the file's.
    NotAnonymous,
    /// No pages, or more than one buddy block of them.
    TooLong,
    /// Past the object's end.
    OutOfRange,
    /// A page of the range is committed.
    Committed,
    /// The job is at its memory limit, or no block is free.
    OutOfMemory,
}

impl fmt::Display for VmoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmoError::OutOfRange { index, pages } => {
                write!(f, "page {index} is outside a {pages}-page object")
            }
            VmoError::OutOfMemory => f.write_str("no frame available to commit"),
        }
    }
}

/// A move refused because a page of the range is held for a device.
///
/// A type of its own rather than a [`VmoError`] variant: only
/// [`Vmo::adopt_pages`] refuses for it, and every other caller matching on a
/// `VmoError` would have to answer a case it can never see.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct HeldPage {
    /// The first held page found.
    pub(crate) index: u64,
}

/// Why [`Vmo::take_page`] left a page as it was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kept {
    /// The page is held in place for a device.
    Held,
    /// There was no memory to record the change (finding F-23).
    NoMemory,
}

/// Why [`Vmo::adopt_pages`] moved nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Unmoved {
    /// A page of the range is held in place.
    Held(HeldPage),
    /// There was no memory to list the pages or name them in the new object
    /// (finding F-23).
    NoMemory,
}

/// Where a file's pages come from before anything has read them: the page
/// cache of a filesystem on a disk.
///
/// A `read` asks the filesystem's store, which fills the object from the
/// file; a fault through a mapping reaches the object directly and would find
/// the page absent and commit zeros in its place. So an object made over a
/// source carries this, and a fault asks it first -- with no lock held, since
/// a fill may wait for the disk.
pub(crate) trait Filler: Send + Sync + fmt::Debug {
    /// Fill page `index` of `vmo`, and perhaps a run after it, if the file
    /// has them and `vmo` lacks them. A page present already is left alone.
    ///
    /// # Errors
    ///
    /// The filesystem's: `EIO` for a page it could not read, `ENOMEM` for
    /// frames.
    fn fill(&self, vmo: &Vmo, index: u64) -> Result<(), ferrix_vfs::Errno>;

    /// Fill page `index` of `vmo` alone, if the file has it and `vmo` lacks
    /// it: no run after it. For a write the kernel makes itself into one page
    /// it knows is the only one wanted, as the loader's zeroing of a
    /// segment's `.bss` tail, where a read-ahead would read pages of the
    /// program that nothing touches.
    ///
    /// # Errors
    ///
    /// As [`Filler::fill`].
    fn fill_one(&self, vmo: &Vmo, index: u64) -> Result<(), ferrix_vfs::Errno>;

    /// Whether every page this object holds can be dropped and filled again
    /// from the source, as it is: the file has no newer copy in memory. False
    /// unless a source says so (`user::cache`).
    fn reclaimable(&self) -> bool {
        false
    }

    /// Whether page `index`, if the object lacks it, is the source's to
    /// fill: it is not a hole the file was cut to, which reads as zeros.
    fn is_sourced(&self, _index: u64) -> bool {
        false
    }
}

/// What [`Vmo::commit_within`] made of a page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Commit {
    /// The page's frame.
    Frame(Frame),
    /// The page is wholly past the end of the file.
    PastEnd,
    /// The page is absent and the file's source is to fill it, which it did
    /// before the fault took the lock and reclaim has undone: the caller
    /// fills it again, and does not commit zeros in its place.
    Unfilled,
}

/// A pageable memory object.
///
/// Reference-counted, because the whole point is that more than one mapping
/// can name the same pages: two processes sharing memory, and from stage 8 a
/// mapped file and a `read` file, are the same object seen twice.
#[derive(Debug)]
pub(crate) struct Vmo {
    /// Which pages hold a frame, and which of those are held in place.
    pages: SpinLock<Pages>,
    /// The object's size in pages. Set at creation and only ever raised, by
    /// [`Vmo::grow_to`]: an index that was inside the object stays inside it.
    len: AtomicU64,
    /// How many pages from the start a fault through a file mapping may
    /// commit: those not wholly past the end of the file this object holds, as
    /// its filesystem last said. [`u64::MAX`] for an object that is not a
    /// file's. Stored before a truncation takes pages away and read under the
    /// pages lock by [`Vmo::commit_within`], which is what keeps a fault from
    /// committing a page the truncation has already passed.
    bound: AtomicU64,
    /// The address spaces that name this object, each with the id it names
    /// it by. Taken after a space's lock and before `pages`, never around
    /// either.
    mappers: SpinLock<Vec<Mapper>>,
    /// How many shared mappings may write the file this object holds: made
    /// from a file open for writing, and not after a seal refused writes.
    /// What a write seal is refused for, as Linux's `i_mmap_writable`. Raised
    /// under a space's lock before a mapping's id enters its tables, lowered
    /// under it when the id leaves.
    shared_may_write: AtomicU64,
    /// Whether a shared mapping that may write has been made since the file's
    /// owner last asked ([`Vmo::take_mapped_writes`]): its writes mark no page
    /// dirty, so the owner writes back every page it holds instead.
    mapped_written: AtomicBool,
    /// What fills an absent page of a file on a disk before a fault commits
    /// it; `None` for every other object, whose absent pages are zeros.
    filler: Option<Arc<dyn Filler>>,
    /// Shared with a device that does not snoop the caches: every mapping
    /// bypasses them, and the kernel copies nothing in or out through its own
    /// cached view. Set once, by [`Vmo::make_coherent`], and never cleared.
    coherent: AtomicBool,
    /// The kernel object it is, charged to the job that made it for as long
    /// as it exists (`object::quota`). Nothing for a file's page cache, which
    /// is the file's; its pages are charged as memory all the same.
    #[expect(
        dead_code,
        reason = "AUDIT: held for its drop, which uncharges the job"
    )]
    charge: Charge,
}

/// How an address space maps an object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sharing {
    /// Private: copy-on-write across `fork`, which gives the child an object
    /// of its own. Such an object has exactly one mapper, ever.
    Private,
    /// Shared: every mapper sees every write.
    Shared,
}

/// One address space's name for an object.
#[derive(Debug)]
struct Mapper {
    /// Whether the space maps it privately or shared.
    sharing: Sharing,
    /// The space. Weak, so that an object does not keep alive the spaces that
    /// map it — they keep it alive, through their object tables.
    space: Weak<AddressSpace>,
    /// The id the space's regions name the object by.
    object: u64,
    /// What the name holds of the heap, charged to the job that mapped the
    /// object ([`MAPPER_HEAP`], certification finding F-37).
    _charge: ferrix_kmem::Charge,
}

/// The heap one id a space names an object by holds, at its largest: its
/// entry in this list, whose room [`compact`] keeps within four times what
/// it lists, and its entries in the space's tables of objects and of files,
/// B-tree nodes being at least half full. A shared mapping of a file makes
/// no object for the object limit to count, so this is what bounds one.
const MAPPER_HEAP: usize = 4 * size_of::<Mapper>()
    + 2 * size_of::<(u64, Arc<Vmo>)>()
    + 2 * size_of::<(u64, crate::user::space::FileMapping)>();

/// Give a mapper list back most of its room once it lists under a quarter
/// of it, so that an object mapped a million times and unmapped does not
/// keep the room for a million: the charges went with the mappers. Kept as
/// it is when there is no memory for the smaller list.
fn compact(mappers: &mut Vec<Mapper>) {
    let (len, room) = (mappers.len(), mappers.capacity());
    if room <= 16 || len >= room / 4 {
        return;
    }
    let Ok(mut smaller) = crate::fallible::try_with_capacity(len.saturating_mul(2).max(4)) else {
        return;
    };
    // NOALLOC: the room for every mapper was had just above, so nothing
    // here can fail part-way and drop the mappers not yet moved.
    smaller.append(mappers);
    *mappers = smaller;
}

/// A VMO's pages, under one lock.
#[derive(Debug, Default)]
struct Pages {
    /// Page index to the frame holding it. Absent means not yet committed.
    frames: BTreeMap<u64, Frame>,
    /// Page index to how many [`Held`] guards hold it. Absent means none.
    held: BTreeMap<u64, u32>,
}

/// Frames phase one took out of an object's list, whose translations are still
/// to be invalidated.
///
/// Given to [`Vmo::retire`], by the object that made it, to finish. Dropped
/// without that, its frames are never released: a leak, and deliberately not
/// the other failure, a frame freed under a live translation.
///
/// A list rather than a map, with its room had before the first frame is
/// taken out ([`Retired::with_room`]): a frame out of an object's list and
/// not yet in this one would be lost, so recording one never allocates
/// (finding F-23).
#[derive(Debug)]
#[must_use = "the frames stay out of the allocator, and mapped, until the retirement is finished"]
pub(crate) struct Retired {
    /// Each page index taken, with the frame that held it, lowest index
    /// first.
    frames: Vec<(u64, Frame)>,
    /// Whether the frames are released once invalidated. Not for a move, whose
    /// frames live on in another object.
    release: bool,
}

impl Retired {
    /// Nothing retired yet, with room for `pages` frames.
    fn with_room(pages: usize) -> Result<Retired, AllocError> {
        Ok(Retired {
            frames: crate::fallible::try_with_capacity(pages)?,
            release: true,
        })
    }

    /// Whether another frame can be recorded without allocating.
    fn has_room(&self) -> bool {
        self.frames.len() < self.frames.capacity()
    }

    /// Record page `index`'s frame, taken out of its object, which must be
    /// above every index recorded so far. The caller made sure of the room
    /// with [`Retired::has_room`] before it took the frame out.
    fn record(&mut self, index: u64, frame: Frame) {
        // NOALLOC: every caller checks `has_room` before it takes the frame
        // out of its object's list.
        self.frames.push((index, frame));
    }

    /// Whether nothing was taken out.
    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The frame page `index` held, if it was taken out.
    pub(crate) fn frame(&self, index: u64) -> Option<Frame> {
        let at = self
            .frames
            .binary_search_by_key(&index, |&(index, _)| index)
            .ok()?;
        self.frames.get(at).map(|&(_, frame)| frame)
    }
}

/// Runs of pages phase two is told about at most, per retirement: past this,
/// the last run is stretched over the rest.
const RUNS: usize = 32;

/// The pages `frames` names, lowest first, as runs of `(first, count)` in
/// `out`; how many runs were written.
///
/// With more runs than `out` holds, the last one is stretched to cover the
/// rest, gaps and all. That asks phase two to forget pages the retirement did
/// not take, which is safe -- a translation forgotten is faulted back in --
/// and needs no memory.
fn runs_of(frames: &[(u64, Frame)], out: &mut [(u64, u64)]) -> usize {
    let mut used: usize = 0;
    let room = out.len();
    for &(index, _) in frames {
        let full = used == room;
        let last = used.checked_sub(1).and_then(|at| out.get_mut(at));
        match last {
            Some((first, count)) if first.saturating_add(*count) == index || full => {
                *count = index.saturating_add(1).saturating_sub(*first);
            }
            _ => {
                if let Some(slot) = out.get_mut(used) {
                    *slot = (index, 1);
                    used += 1;
                }
            }
        }
    }
    used
}

/// Frames a retirement that could not have its list takes out of an object
/// at a time, from the stack: [`Vmo::decommit_range`] when memory has run
/// out.
const CHUNK: usize = 32;

/// The address space phase one ran under the lock of, which phase two must
/// therefore not visit.
#[derive(Debug)]
pub(crate) struct Own<'a> {
    /// The space.
    pub(crate) space: &'a AddressSpace,
    /// Its own shootdown, still to run: the processors it read after taking
    /// its translations down, and the pages, which it has counted pending.
    /// `None` when its shootdown has already returned.
    pub(crate) shootdown: Option<(CpuSet, TlbPages)>,
}

impl Vmo {
    /// An anonymous object of `pages` pages, with nothing committed.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when memory has run out.
    pub(crate) fn new_anonymous(pages: u64) -> Result<Arc<Vmo>, AllocError> {
        // An object the running task's program made, which its job's
        // object limit counts.
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| AllocError)?;
        crate::fallible::try_arc(Vmo::unfilled(pages, None, charge))
    }

    /// An object of `pages` pages holding a file's contents, whose absent
    /// pages `filler` fills before a fault commits them.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when memory has run out.
    pub(crate) fn new_filled(
        pages: u64,
        filler: Option<Arc<dyn Filler>>,
    ) -> Result<Arc<Vmo>, AllocError> {
        let vmo = crate::fallible::try_arc(Vmo::unfilled(
            pages,
            filler,
            Charge::none(Resource::Objects),
        ))?;
        crate::user::cache::register(&vmo)?;
        Ok(vmo)
    }

    /// Whether this is plain anonymous memory: not a file's object, whose
    /// pages are the file's, on a disk or in memory. What a chardev driver
    /// may hand a program as a dmabuf (`interfaces::chardev::dmabuf`; the
    /// certification consultant's ledger 316, (2)). The same test as
    /// [`Vmo::commit_run`]'s.
    pub(crate) fn is_anonymous(&self) -> bool {
        self.filler.is_none() && self.bound.load(Ordering::SeqCst) == u64::MAX
    }

    /// Whether this is the object of a file on a disk.
    pub(crate) fn is_disk_file(&self) -> bool {
        self.filler.is_some()
    }

    /// Whether reclaim may drop this object's pages: its source can read
    /// them again and has no newer copy, and no shared mapping may write
    /// them without a mark (`user::cache`).
    pub(crate) fn reclaimable(&self) -> bool {
        self.filler
            .as_ref()
            .is_some_and(|filler| filler.reclaimable())
            && !self.writably_mapped()
            && !self.mapped_written.load(Ordering::SeqCst)
    }

    /// Note, in `out`, the index of each committed page from `from` whose
    /// frame `pick` accepts and that is not held, until `room` are noted or
    /// [`CHUNK`]-many pages were looked at; `pick` sees each page looked at.
    /// `out` has room for `room` already. The index to go on from, or
    /// `None` at the end of the object.
    pub(crate) fn pick_pages(
        &self,
        from: u64,
        room: usize,
        out: &mut Vec<u64>,
        pick: &mut dyn FnMut(Frame) -> bool,
    ) -> Option<u64> {
        let state = self.pages.lock();
        for (looked, (&index, &frame)) in state.frames.range(from..).enumerate() {
            if out.len() >= room || looked >= CHUNK {
                return Some(index);
            }
            if !state.held.contains_key(&index)
                && pick(frame)
                && crate::fallible::push_within(out, index).is_err()
            {
                return Some(index);
            }
        }
        None
    }

    /// Call `visit` with the frame of each committed page.
    pub(crate) fn for_each_page(&self, visit: &mut dyn FnMut(Frame)) {
        for &frame in self.pages.lock().frames.values() {
            visit(frame);
        }
    }

    /// The object [`Vmo::new_filled`] allocates.
    fn unfilled(pages: u64, filler: Option<Arc<dyn Filler>>, charge: Charge) -> Vmo {
        Vmo {
            pages: SpinLock::new(Pages::default()),
            len: AtomicU64::new(pages),
            bound: AtomicU64::new(u64::MAX),
            mappers: SpinLock::new(Vec::new()),
            shared_may_write: AtomicU64::new(0),
            mapped_written: AtomicBool::new(false),
            filler,
            coherent: AtomicBool::new(false),
            charge,
        }
    }

    /// Fill page `index` from the file before a fault commits it, if this
    /// object is a file's on a disk. Must be called with no lock held.
    ///
    /// # Errors
    ///
    /// As [`Filler::fill`].
    pub(crate) fn fill_for_fault(&self, index: u64) -> Result<(), ferrix_vfs::Errno> {
        match &self.filler {
            Some(filler) => filler.fill(self, index),
            None => Ok(()),
        }
    }

    /// [`Vmo::fill_for_fault`] for page `index` alone, with no read-ahead
    /// ([`Filler::fill_one`]). Must be called with no lock held.
    ///
    /// # Errors
    ///
    /// As [`Filler::fill_one`].
    pub(crate) fn fill_one_page(&self, index: u64) -> Result<(), ferrix_vfs::Errno> {
        match &self.filler {
            Some(filler) => filler.fill_one(self, index),
            None => Ok(()),
        }
    }

    /// Whether the object's mappings bypass the caches.
    pub(crate) fn is_coherent(&self) -> bool {
        self.coherent.load(Ordering::Acquire)
    }

    /// Make every mapping of this object from now on bypass the caches, for
    /// a device that does not snoop them (`vmo_pin`'s `PIN_COHERENT`).
    ///
    /// Refused, answering `false`, while any address space maps it: its
    /// translations are cached ones, and a page reached both ways is a page
    /// whose dirty lines may land on what the device wrote. Taken under the
    /// mappers' lock, so a mapping attached after this sees the flag, since
    /// every mapping attaches before a fault can reach it.
    pub(crate) fn make_coherent(&self) -> bool {
        let mappers = self.mappers.lock();
        if !mappers.is_empty() {
            return false;
        }
        self.coherent.store(true, Ordering::Release);
        true
    }

    /// The object's size in pages.
    pub(crate) fn len_pages(&self) -> u64 {
        self.len.load(Ordering::Relaxed)
    }

    /// The object's size in bytes.
    pub(crate) fn len_bytes(&self) -> u64 {
        self.len_pages() * PAGE_SIZE
    }

    /// How many pages currently hold a frame.
    ///
    /// The difference between this and [`Vmo::len_pages`] is the reservation
    /// that has been promised and not yet paid for, which is what overcommit
    /// means and what the exit criterion measures.
    pub(crate) fn committed(&self) -> usize {
        self.pages.lock().frames.len()
    }

    /// The frame holding page `index`, if it has been committed.
    pub(crate) fn page(&self, index: u64) -> Option<Frame> {
        self.pages.lock().frames.get(&index).copied()
    }

    /// Record that `space` names this object as `object`.
    ///
    /// Called wherever an id goes into an address space's object table, with
    /// that space's lock held — which is the order: a space's lock, then this
    /// list. From here on a page this object takes away is forgotten in that
    /// space before its frame is released. A space that has gone since it was
    /// recorded is pruned whenever the list is next taken.
    ///
    /// # Panics
    ///
    /// If this would give an object backing a private region a second
    /// mapper, or a private mapper to an object already mapped: an invariant
    /// agreed with stage 9's `vmo_map`, and one `mremap`'s move of a private
    /// region's pages relies on to tell nobody else. `fork` gives a child its
    /// own object for every private region, so nothing in the kernel does
    /// this, and a caller that did would be silently wrong about which spaces
    /// see a page. Always on, not a debug assertion, for that reason.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory to list the mapper; nothing is
    /// recorded.
    pub(crate) fn attach(
        &self,
        space: Weak<AddressSpace>,
        object: u64,
        sharing: Sharing,
    ) -> Result<(), AllocError> {
        let charge = ferrix_kmem::Charge::bytes(MAPPER_HEAP).map_err(|_| AllocError)?;
        let mut mappers = self.mappers.lock();
        mappers.retain(|mapper| mapper.space.strong_count() > 0);
        compact(&mut mappers);
        let joinable = mappers
            .iter()
            .all(|mapper| mapper.sharing == Sharing::Shared)
            && (sharing == Sharing::Shared || mappers.is_empty());
        if !joinable {
            crate::panic::fatal!(
                crate::panic::catalog::PRIVATE_OBJECT_SHARED,
                "a VMO backing a private region would have more than one mapper"
            );
        }
        crate::fallible::try_push(
            &mut mappers,
            Mapper {
                sharing,
                space,
                object,
                _charge: charge,
            },
        )
    }

    /// Record that the address space at `space` no longer names this object
    /// as `object`.
    ///
    /// Called wherever an id leaves an address space's object table, and for
    /// every object as a space is dropped. Takes the mapper list only, and
    /// compares addresses rather than following one, so a space in the middle
    /// of being dropped can name itself.
    pub(crate) fn detach(&self, space: *const AddressSpace, object: u64) {
        let mut mappers = self.mappers.lock();
        mappers.retain(|mapper| {
            mapper.space.strong_count() > 0
                && !(core::ptr::eq(mapper.space.as_ptr(), space) && mapper.object == object)
        });
        compact(&mut mappers);
    }

    /// How many address spaces name this object, counting a space once per
    /// id it names it by.
    pub(crate) fn mapper_count(&self) -> usize {
        self.mappers
            .lock()
            .iter()
            .filter(|mapper| mapper.space.strong_count() > 0)
            .count()
    }

    /// A second object naming every page this one holds, for `fork`.
    ///
    /// The pages are *shared*, not copied: each committed frame gains a
    /// reference and both objects name it. What makes that safe is that the
    /// regions mapping either object are marked copy-on-write at the same
    /// moment, so the first write through either one copies the page and
    /// replaces it in *its own* object, leaving the other's untouched.
    ///
    /// Which is the reason fork clones the object rather than sharing one
    /// `Arc`. Two address spaces holding one `Arc<Vmo>` cannot diverge —
    /// [`Vmo::replace`] would swap the page for *both* of them, and a child's
    /// first write would be visible to its parent, which is precisely what
    /// `MAP_PRIVATE` promises will not happen. Sharing the `Arc` is right for
    /// `MAP_SHARED`, and [`crate::user::space::AddressSpace::fork`] is where
    /// the two cases are told apart.
    ///
    /// A page [`Vmo::hold`] holds is copied into the new object instead:
    /// sharing it would make the next write through either side a
    /// copy-on-write fault, and one that swapped the held frame out of this
    /// object would leave a device writing to a page nobody reads.
    ///
    /// The new object is mapped by nobody yet; the space that takes it
    /// attaches itself.
    ///
    /// # Errors
    ///
    /// [`VmoError::OutOfMemory`] if the allocator refuses a reference on a
    /// page — which means the frame is not allocated or its count would wrap,
    /// both kernel bugs rather than conditions to recover from — or has no
    /// frame to copy a held page into. Nothing is shared when that happens:
    /// the references and copies taken so far are given back, so a failed
    /// fork costs nothing.
    pub(crate) fn fork(&self) -> Result<Arc<Vmo>, VmoError> {
        // A second object, the forking program's to pay for, as a page cache
        // object's fork -- a private file mapping's shadow -- is too.
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| VmoError::OutOfMemory)?;
        let pages = self.pages.lock();

        let mut frames = BTreeMap::new();
        for (&index, &frame) in &pages.frames {
            let given = if pages.held.contains_key(&index) {
                mm::allocate_user_frame().inspect(|&copy| mm::copy_frame(copy, frame))
            } else {
                mm::share_frame(frame).map(|_| frame)
            };
            // Recorded before it is counted as taken, so that the unwind below
            // gives back exactly what the map holds.
            let recorded = given.and_then(|given| {
                crate::fallible::insert(&mut frames, index, given)
                    .map_err(|_| {
                        let _ = mm::release_frame(given);
                    })
                    .ok()
            });
            if recorded.is_none() {
                // Unwind, or the pages this got through would be held by an
                // object that is never built and never dropped.
                for &taken in frames.values() {
                    let _ = mm::release_frame(taken);
                }
                return Err(VmoError::OutOfMemory);
            }
        }

        // An object that cannot be allocated drops its frames as it goes,
        // which gives back every reference and copy taken above.
        crate::fallible::try_arc(Vmo {
            pages: SpinLock::new(Pages {
                frames,
                held: BTreeMap::new(),
            }),
            len: AtomicU64::new(self.len_pages()),
            bound: AtomicU64::new(self.bound.load(Ordering::SeqCst)),
            mappers: SpinLock::new(Vec::new()),
            shared_may_write: AtomicU64::new(0),
            mapped_written: AtomicBool::new(false),
            // A page neither side has is still the file's.
            filler: self.filler.clone(),
            // Forked for a private mapping, which a coherent object never
            // has: `vmo_map` maps it shared. The copy is the process's own.
            coherent: AtomicBool::new(false),
            charge,
        })
        .map_err(|_| VmoError::OutOfMemory)
    }

    /// The frame holding page `index`, allocating and zeroing one if this is
    /// the first touch.
    ///
    /// Zeroed rather than merely allocated: a fresh anonymous page that still
    /// held the last owner's data is an information leak, and from here the
    /// last owner is another process. [`crate::mm::zero_frame`] is the single
    /// path every such hand-off goes through.
    ///
    /// # Errors
    ///
    /// [`VmoError::OutOfRange`] past the end of the object, and
    /// [`VmoError::OutOfMemory`] when the allocator has nothing left.
    pub(crate) fn commit(&self, index: u64) -> Result<Frame, VmoError> {
        let len = self.len_pages();
        if index >= len {
            return Err(VmoError::OutOfRange { index, pages: len });
        }

        let mut pages = self.pages.lock();
        if let Some(&frame) = pages.frames.get(&index) {
            return Ok(frame);
        }

        // Room to record the page before the frame is taken, so a refusal
        // leaves nothing to give back.
        let held = crate::fallible::reserve().map_err(|_| VmoError::OutOfMemory)?;
        let frame = mm::allocate_user_frame().ok_or(VmoError::OutOfMemory)?;
        mm::zero_frame(frame);
        let _ = crate::fallible::insert_held(&held, &mut pages.frames, index, frame);
        Ok(frame)
    }

    /// Put `frame`, filled by the caller, in page `index` if that page is
    /// inside the object and holds nothing yet. Whether it went in.
    ///
    /// What a page cache filled from a disk does once the fill returns: the
    /// fill ran with no lock held, so a write or a racing fill may have put a
    /// page there meanwhile, and that one wins. A frame that did not go in is
    /// still the caller's, to release. Nobody can have mapped a page that was
    /// absent, so nothing is taken down.
    pub(crate) fn insert_absent(&self, index: u64, frame: Frame) -> bool {
        if index >= self.len_pages() {
            return false;
        }
        let mut pages = self.pages.lock();
        if pages.frames.contains_key(&index) {
            return false;
        }
        // With no memory to record it, it did not go in, and stays the
        // caller's, as when another fill won.
        crate::fallible::insert(&mut pages.frames, index, frame).is_ok()
    }

    /// The file this object holds is now `len` bytes long: a fault through a
    /// mapping of it may commit no page wholly past that.
    ///
    /// The filesystem calls it under its inode lock, after a write or grow
    /// extends the file, and *before* it takes a truncated file's pages away,
    /// so a fault racing the truncation sees the new end first.
    pub(crate) fn set_file_len(&self, len: u64) {
        self.bound.store(len.div_ceil(PAGE_SIZE), Ordering::SeqCst);
    }

    /// Whether page `index` is wholly past the end of the file this object
    /// holds, as its filesystem last said; never for an object that is not a
    /// file's.
    ///
    /// For a private file mapping, whose fault refuses such a page before it
    /// looks at the file or at the shadow its copies live in. Read without the
    /// pages lock: the fault holds its space's lock from this check through
    /// its mapping, and a truncation stores the new end before it takes the
    /// pages away and then takes the lock of every space that maps them.
    pub(crate) fn past_file_end(&self, index: u64) -> bool {
        index >= self.bound.load(Ordering::SeqCst)
    }

    /// One more shared mapping that may write the file. Before the mapping
    /// reads the file's seals, so a seal stored before this is seen there,
    /// and one stored after it sees this.
    pub(crate) fn raise_shared_may_write(&self) {
        let _ = self.shared_may_write.fetch_add(1, Ordering::SeqCst);
        self.mapped_written.store(true, Ordering::SeqCst);
    }

    /// The pages a shared mapping may have written since the last call:
    /// every page this object holds, since such writes mark none, or none if
    /// no mapping that may write has been made. The mark stays while such a
    /// mapping does.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for the list; the mark is left
    /// as it was, so the next call reports the pages instead.
    pub(crate) fn take_mapped_writes(&self) -> Result<Vec<u64>, AllocError> {
        let still = self.writably_mapped();
        let was = self.mapped_written.swap(still, Ordering::SeqCst);
        if !was && !still {
            return Ok(Vec::new());
        }
        let written = crate::fallible::try_collect(self.pages.lock().frames.keys().copied());
        if written.is_err() && was {
            self.mapped_written.store(true, Ordering::SeqCst);
        }
        written
    }

    /// One fewer, as such a mapping's id leaves a space's tables.
    pub(crate) fn lower_shared_may_write(&self) {
        let _ = self.shared_may_write.fetch_sub(1, Ordering::SeqCst);
    }

    /// Whether a shared mapping may write the file: what refuses a write seal.
    pub(crate) fn writably_mapped(&self) -> bool {
        self.shared_may_write.load(Ordering::SeqCst) > 0
    }

    /// [`Vmo::commit`] for a fault through a file mapping: `None`, committing
    /// nothing, for a page wholly past the end of the file, which is the
    /// fault Linux answers with `SIGBUS`.
    ///
    /// The end is read under the pages lock. A truncation stores its new end
    /// before it takes that lock to take pages away, so either this sees the
    /// new end and refuses, or it commits first and the truncation's
    /// retirement then takes the page back out of every space that maps it,
    /// the faulting one included, once that space's lock is free.
    ///
    /// # Errors
    ///
    /// As [`Vmo::commit`].
    pub(crate) fn commit_within(&self, index: u64) -> Result<Commit, VmoError> {
        let len = self.len_pages();
        if index >= len {
            return Err(VmoError::OutOfRange { index, pages: len });
        }

        let mut pages = self.pages.lock();
        if index >= self.bound.load(Ordering::SeqCst) {
            return Ok(Commit::PastEnd);
        }
        if let Some(&frame) = pages.frames.get(&index) {
            return Ok(Commit::Frame(frame));
        }
        if self.sourced(index) {
            return Ok(Commit::Unfilled);
        }

        let held = crate::fallible::reserve().map_err(|_| VmoError::OutOfMemory)?;
        let frame = mm::allocate_user_frame().ok_or(VmoError::OutOfMemory)?;
        mm::zero_frame(frame);
        let _ = crate::fallible::insert_held(&held, &mut pages.frames, index, frame);
        Ok(Commit::Frame(frame))
    }

    /// Whether page `index`, if absent, is its file's source to fill.
    pub(crate) fn sourced(&self, index: u64) -> bool {
        self.filler
            .as_ref()
            .is_some_and(|filler| filler.is_sourced(index))
    }

    /// [`Vmo::read_page`], but `false`, reading nothing, for a page the
    /// object lacks: the caller decides whether that is a hole or a page to
    /// fill again.
    ///
    /// # Errors
    ///
    /// As [`Vmo::read_page`].
    pub(crate) fn read_present(
        &self,
        index: u64,
        offset: usize,
        out: &mut [u8],
    ) -> Result<bool, VmoError> {
        self.check_span(index, offset, out.len())?;
        let pages = self.pages.lock();
        let Some(&frame) = pages.frames.get(&index) else {
            return Ok(false);
        };
        let at = mm::direct_map(frame * PAGE_SIZE) as usize + offset;
        // SAFETY: (FRAME) as in `read_page`: the object holds a reference on
        // `frame` while its lock is held, `check_span` kept the range inside
        // the page, and the direct map covers all of RAM.
        let source = unsafe { core::slice::from_raw_parts(at as *const u8, out.len()) };
        out.copy_from_slice(source);
        Ok(true)
    }

    /// Copy `out.len()` bytes out of page `index`, starting `offset` into it.
    ///
    /// A page never committed reads as zeros and stays uncommitted: reading a
    /// reservation must not pay for it. The copy is made under the object's
    /// lock, which is what keeps the frame from being decommitted and freed
    /// half-way through; the caller copies onwards to user memory after it is
    /// released.
    ///
    /// # Errors
    ///
    /// [`VmoError::OutOfRange`] past the end of the object or of the page.
    pub(crate) fn read_page(
        &self,
        index: u64,
        offset: usize,
        out: &mut [u8],
    ) -> Result<(), VmoError> {
        self.check_span(index, offset, out.len())?;
        let pages = self.pages.lock();
        match pages.frames.get(&index) {
            None => out.fill(0),
            Some(&frame) => {
                let at = mm::direct_map(frame * PAGE_SIZE) as usize + offset;
                // SAFETY: (FRAME) the object holds a reference on `frame` for as long
                // as its lock is held, `check_span` kept `offset + out.len()`
                // inside the page, and the direct map covers all of RAM.
                let source = unsafe { core::slice::from_raw_parts(at as *const u8, out.len()) };
                out.copy_from_slice(source);
            }
        }
        Ok(())
    }

    /// Copy `data` into page `index`, starting `offset` into it, committing
    /// the page first if this is its first touch.
    ///
    /// A page this object shares with another — one `fork` left in both — is
    /// copied before it is written, so that a write through this object is
    /// never visible through the other. A held page never is: it is written
    /// where the device reads it. The shared frame the copy displaced leaves
    /// in three phases, as a decommitted one does, so that a mapping of this
    /// object faults the copy in rather than keeping the original.
    ///
    /// Must not be called holding a spin lock: the displacement may wait for
    /// a shootdown.
    ///
    /// # Errors
    ///
    /// [`VmoError::OutOfRange`], or [`VmoError::OutOfMemory`].
    pub(crate) fn write_page(
        &self,
        index: u64,
        offset: usize,
        data: &[u8],
    ) -> Result<(), VmoError> {
        self.check_span(index, offset, data.len())?;
        // A page some other holder shares is copied, and its original
        // displaced into a list that must have room before the original
        // leaves this one (finding F-23). The room is had only when a copy
        // is due, which is rare: the lock is let go for it and the page
        // looked at again.
        let mut room = None;
        let retired = loop {
            let mut pages = self.pages.lock();
            let (frame, retired) = match pages.frames.get(&index).copied() {
                Some(frame)
                    if mm::frame_references(frame) <= 1 || pages.held.contains_key(&index) =>
                {
                    (frame, None)
                }
                Some(_) => {
                    let Some(mut retired) = room.take() else {
                        drop(pages);
                        room = Some(Retired::with_room(1).map_err(|_| VmoError::OutOfMemory)?);
                        continue;
                    };
                    let (frame, shared) = pages.exclusive(index)?;
                    if let Some(shared) = shared {
                        retired.record(index, shared);
                    }
                    (frame, Some(retired))
                }
                None => (pages.exclusive(index)?.0, None),
            };
            let at = mm::direct_map(frame * PAGE_SIZE) as usize + offset;
            // SAFETY: (FRAME) the frame is this object's alone (copied above if it was
            // not) and held under its lock; `check_span` kept the write inside
            // the page; the direct map is writable for all of RAM.
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), at as *mut u8, data.len()) };
            break retired;
        };
        if let Some(retired) = retired.filter(|retired| !retired.is_empty()) {
            self.retire(retired, None);
        }
        Ok(())
    }

    /// Refuse a byte range that leaves page `index` or the object.
    fn check_span(&self, index: u64, offset: usize, len: usize) -> Result<(), VmoError> {
        let end = offset.checked_add(len);
        let pages = self.len_pages();
        if index >= pages || end.is_none_or(|end| end as u64 > PAGE_SIZE) {
            return Err(VmoError::OutOfRange { index, pages });
        }
        Ok(())
    }

    /// Replace the frame holding page `index`, giving back the reference the
    /// object held on the old one.
    ///
    /// What a copy-on-write fault does once it has copied: the object now
    /// names the private copy, and the shared original is one holder lighter.
    /// Returns the frame that was displaced, once every mapping of it is gone
    /// and its reference given back.
    ///
    /// A held page is not replaced, and `None` comes back with `frame` still
    /// the caller's; so it does when memory has run out. No fault reaches here for one — [`Vmo::hold`] leaves each
    /// page it holds unshared, and [`Vmo::fork`] copies rather than shares it
    /// — so this is the refusal that keeps a device's page from being freed
    /// under it if that ever stops being true.
    ///
    /// Must not be called holding a spin lock; a caller holding an address
    /// space's lock uses [`Vmo::take_page`] and [`Vmo::retire`] instead.
    pub(crate) fn replace(&self, index: u64, frame: Frame) -> Option<Frame> {
        let retired = self.take_page(index, frame).ok()?;
        let old = retired.frame(index);
        self.retire(retired, None);
        old
    }

    /// Phase one of [`Vmo::replace`]: put `frame` in page `index`, and take
    /// out the frame that was there.
    ///
    /// May be called holding an address space's lock, and nothing else.
    ///
    /// # Errors
    ///
    /// [`Kept`], with `frame` still the caller's and the page unchanged: the
    /// page is held, or there is no memory to record the change in.
    pub(crate) fn take_page(&self, index: u64, frame: Frame) -> Result<Retired, Kept> {
        let mut retired = Retired::with_room(1).map_err(|_| Kept::NoMemory)?;
        let mut pages = self.pages.lock();
        if pages.held.contains_key(&index) {
            return Err(Kept::Held);
        }
        match pages.frames.get_mut(&index) {
            Some(slot) => retired.record(index, core::mem::replace(slot, frame)),
            None => {
                let _ = crate::fallible::insert(&mut pages.frames, index, frame)
                    .map_err(|_| Kept::NoMemory)?;
            }
        }
        Ok(retired)
    }

    /// Give back `pages` pages from `first`, and report how many held a frame.
    ///
    /// What `munmap` of part of a mapping does to the object behind it. The
    /// pages are gone rather than merely unmapped: a process that unmaps half
    /// its heap expects the memory back, and an object that kept them until
    /// the rest of the mapping went would hold them for as long as the
    /// process lived.
    ///
    /// Only the caller knows whether that is right — a page of a *shared*
    /// object is not one unmapper's to take away — so this does as it is told
    /// and [`crate::user::space::AddressSpace`] decides. A held page stays,
    /// mapped wherever it was, and is not counted. Every other page is
    /// forgotten by every space that maps it before its frame goes back.
    ///
    /// Must not be called holding a spin lock.
    pub(crate) fn decommit_range(&self, first: u64, pages: u64) -> usize {
        match self.take_range(first, pages) {
            Ok(retired) => {
                let given = retired.frames.len();
                self.retire(retired, None);
                given
            }
            Err(AllocError) => self.decommit_in_chunks(first, first.saturating_add(pages)),
        }
    }

    /// [`Vmo::decommit_range`] over `first..end` with no memory for the list
    /// of what it takes: [`CHUNK`] pages at a time, each chunk through all
    /// three phases before the next is taken. Slower -- a shootdown a chunk
    /// -- and the same in the end.
    fn decommit_in_chunks(&self, first: u64, end: u64) -> usize {
        let mut given = 0;
        let mut from = Some(first);
        while let Some(start) = from {
            let mut chunk = [(0, 0); CHUNK];
            let (taken, next) = self.pages.lock().take_into(start, end, &mut chunk);
            let frames = chunk.get(..taken).unwrap_or(&[]);
            given += frames.len();
            let mut runs = [(0, 0); RUNS];
            let count = runs_of(frames, &mut runs);
            self.retire_runs(frames, true, runs.get(..count).unwrap_or(&[]), None, false);
            from = next;
        }
        given
    }

    /// Commit `pages` pages from `first` with one run of fresh, zeroed
    /// frames, each charged to the running task's job: what a contiguous pin
    /// (`PIN_CONTIGUOUS`, `docs/NVIDIA.md` §4.3) holds. Every page of the
    /// range must be absent, and they all go in under the pages lock at
    /// once, or none does, so nobody can map one of them before the run is
    /// whole. The run's first frame; [`Vmo::give_back_run`] undoes it.
    ///
    /// Must not be called holding a spin lock.
    ///
    /// # Errors
    ///
    /// [`RunError::NotAnonymous`] for a file's object, whose absent pages are
    /// the file's and not zeros; [`RunError::TooLong`] for none or more than
    /// one buddy block (`1 << MAX_ORDER` pages); [`RunError::OutOfRange`] past
    /// the object's end; [`RunError::Committed`] when a page of the range is
    /// committed, before or while the run is made; and
    /// [`RunError::OutOfMemory`] when the job is at its limit or there is no
    /// free block. Nothing is committed or charged then.
    pub(crate) fn commit_run(&self, first: u64, pages: u64) -> Result<Frame, RunError> {
        if self.filler.is_some() || self.bound.load(Ordering::SeqCst) != u64::MAX {
            return Err(RunError::NotAnonymous);
        }
        if pages == 0 || pages > 1 << ferrix_frame::MAX_ORDER {
            return Err(RunError::TooLong);
        }
        let end = first
            .checked_add(pages)
            .filter(|&end| end <= self.len_pages())
            .ok_or(RunError::OutOfRange)?;
        if self.pages.lock().frames.range(first..end).next().is_some() {
            return Err(RunError::Committed);
        }
        let order = (0..=ferrix_frame::MAX_ORDER)
            .find(|&order| 1u64 << order >= pages)
            .ok_or(RunError::TooLong)?;
        let block = mm::allocate_user_run(order).ok_or(RunError::OutOfMemory)?;
        // The block's frames past the range go back at once.
        for frame in block + pages..block + (1u64 << order) {
            let _ = mm::release_frame(frame);
        }
        for frame in block..block + pages {
            mm::zero_frame(frame);
        }
        let inserted = self.pages.lock().insert_run(first, block, pages);
        if inserted.is_err() {
            for frame in block..block + pages {
                let _ = mm::release_frame(frame);
            }
        }
        inserted
    }

    /// Undo [`Vmo::commit_run`] of `pages` frames from `block` at `first`:
    /// take out each page that still holds the frame the run put there, and
    /// is not held, forget it in every space that maps it, and release it,
    /// which takes its charge back. A page anything else filled since stays.
    /// Needs no memory: [`CHUNK`] pages at a time on the stack.
    ///
    /// Must not be called holding a spin lock.
    pub(crate) fn give_back_run(&self, first: u64, block: Frame, pages: u64) {
        let mut page = 0;
        while page < pages {
            let mut chunk = [(0, 0); CHUNK];
            let (taken, next) =
                self.pages
                    .lock()
                    .take_ours(first, block, (page, pages), &mut chunk);
            page = next;
            let frames = chunk.get(..taken).unwrap_or(&[]);
            let mut runs = [(0, 0); RUNS];
            let count = runs_of(frames, &mut runs);
            self.retire_runs(frames, true, runs.get(..count).unwrap_or(&[]), None, false);
        }
    }

    /// Whether any page of `first..first + pages` is held in place for a
    /// device: what [`Vmo::decommit_range`] skipped, and a range that must not
    /// be handed to anyone else.
    pub(crate) fn holds_any(&self, first: u64, pages: u64) -> bool {
        let end = first.saturating_add(pages);
        self.pages.lock().held.range(first..end).next().is_some()
    }

    /// Phase one of [`Vmo::decommit_range`]: take every committed page in
    /// `first..first + pages` that is not held out of the list.
    ///
    /// May be called holding an address space's lock, and nothing else.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory to list what would be taken;
    /// nothing is. The list's room is had first, so that a frame once out of
    /// the object is never lost (finding F-23).
    pub(crate) fn take_range(&self, first: u64, pages: u64) -> Result<Retired, AllocError> {
        let end = first.saturating_add(pages);
        let mut state = self.pages.lock();
        // Held pages are skipped before anything is taken: a held page is
        // never among what the invalidation is told about.
        let count = state
            .frames
            .range(first..end)
            .filter(|&(index, _)| !state.held.contains_key(index))
            .count();
        let mut frames = crate::fallible::try_filled((0, 0), count)?;
        let (taken, _) = state.take_into(first, end, &mut frames);
        frames.truncate(taken);
        Ok(Retired {
            frames,
            release: true,
        })
    }

    /// Give back every page from `first` to the end, and report how many held
    /// a frame.
    ///
    /// What truncating a file does. Not [`Vmo::decommit_range`] over the rest
    /// of the object, because a file's object is sized for the largest file
    /// the filesystem allows -- hundreds of millions of pages -- and a loop
    /// over indices would visit every one of them to find the handful that are
    /// committed. A walk of the sparse list from `first` visits only those. A
    /// held page stays, and is not counted.
    ///
    /// Must not be called holding a spin lock. An object nobody maps — every
    /// file's today — costs no more than the split: phase two has nobody to
    /// ask.
    pub(crate) fn decommit_from(&self, first: u64) -> usize {
        self.decommit_range(first, u64::MAX - first)
    }

    /// Phases two and three: have every address space that maps this object
    /// forget the pages `retired` took, shoot down once on every processor
    /// any of them names, wait for every answer, and only then release the
    /// frames.
    ///
    /// `own` is the space a caller already did phase one under the lock of:
    /// it is not visited, its own shootdown (if it has one still to run) is
    /// folded into this one, and its pending count is dropped once that has
    /// returned.
    ///
    /// Must be called holding no spin lock at all — no space's, no object's.
    /// Phase two takes every mapping space's lock in turn, and the shootdown
    /// waits for other processors.
    pub(crate) fn retire(&self, retired: Retired, own: Option<Own<'_>>) {
        let mut runs = [(0, 0); RUNS];
        let count = runs_of(&retired.frames, &mut runs);
        let Retired { frames, release } = retired;
        self.retire_runs(
            &frames,
            release,
            runs.get(..count).unwrap_or(&[]),
            own,
            false,
        );
    }

    /// Take down every mapping of this object's pages from page `first` on,
    /// whether or not the object holds them, and give back what a private file
    /// mapping copied of them, once no processor can reach it.
    ///
    /// What truncating a file does besides giving back the file's own pages
    /// ([`Vmo::decommit_from`]). A private mapping of the file keeps the pages
    /// it wrote in a shadow object; on Linux a truncation drops those copies
    /// too, and a file grown back shows zeros through the mapping, not the old
    /// copy. And a copy the mapping made of a hole names an index no page of
    /// the file's own does, so no run of the file's pages would reach it. So
    /// the forget runs over everything from `first`, and each space hands back
    /// its shadow's pages there, released after this call's own shootdown:
    /// their translations are among the addresses it takes down, and nothing
    /// else maps a shadow.
    ///
    /// Only a truncation takes copies. A retirement of the file's own pages for
    /// any other reason leaves a private mapping's copies alone, as reclaim
    /// would on Linux.
    ///
    /// The run from `first` to the end is walked through each object's sparse
    /// page map, never page by page, so its length costs nothing.
    ///
    /// Must not be called holding a spin lock.
    pub(crate) fn cut_mappings(&self, first: u64) {
        let runs = [(first, u64::MAX - first)];
        self.retire_runs(&[], false, &runs, None, true);
    }

    /// [`Vmo::retire`] of `frames` over `runs`, which cover at least the
    /// pages `frames` names; the frames are released after if `release`. With
    /// `cut`, as [`Vmo::cut_mappings`].
    fn retire_runs(
        &self,
        frames: &[(u64, Frame)],
        release: bool,
        runs: &[(u64, u64)],
        own: Option<Own<'_>>,
        cut: bool,
    ) {
        let except = own.as_ref().map(|own| own.space);
        let from = cut.then_some(self);
        if runs.is_empty() {
            own_shootdown(own);
        } else {
            match self.mapped_by(except) {
                Ok(visits) => forget_together(visits, runs, own, from),
                // No memory for the list of spaces to ask: ask them one at a
                // time instead, each with a shootdown of its own.
                Err(AllocError) => {
                    own_shootdown(own);
                    self.forget_one_at_a_time(except, runs, from);
                }
            }
        }
        if release {
            for &(_, frame) in frames {
                let _ = mm::release_frame(frame);
            }
        }
    }

    /// Every live address space that maps this object, but `except`, with the
    /// id it maps it by. Spaces that have gone are pruned on the way.
    ///
    /// The list is copied and its lock let go before any space is visited, and
    /// the references come back to be dropped by the caller with no lock held.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for the list.
    fn mapped_by(&self, except: Option<&AddressSpace>) -> Result<Vec<Visit>, AllocError> {
        let mut mappers = self.mappers.lock();
        mappers.retain(|mapper| mapper.space.strong_count() > 0);
        let mut visits = crate::fallible::try_with_capacity(mappers.len())?;
        let found = mappers
            .iter()
            .filter(|mapper| {
                except.is_none_or(|space| !core::ptr::eq(mapper.space.as_ptr(), space))
            })
            .filter_map(|mapper| Some((mapper.space.upgrade()?, mapper.object)));
        for (space, object) in found {
            // NOALLOC: room for every mapper was had above.
            visits.push(Visit {
                space,
                object,
                forgotten: false,
                copies: None,
            });
        }
        Ok(visits)
    }

    /// Phase two without a list: visit each space that maps this object, but
    /// `except`, one at a time, and run each one's shootdown before the next.
    ///
    /// The mapper list may change between visits, since its lock is let go
    /// for each, so the walk goes in order of a key that does not move -- the
    /// space's address and the id -- and each step takes the least key past
    /// the last one visited. A mapper there all along is visited exactly once;
    /// one attached meanwhile cannot reach the frames, which were out of the
    /// object before it came.
    fn forget_one_at_a_time(
        &self,
        except: Option<&AddressSpace>,
        runs: &[(u64, u64)],
        from: Option<&Vmo>,
    ) {
        let mut after = None;
        while let Some((space, object)) = self.next_mapper(except, after) {
            after = Some((Arc::as_ptr(&space).addr(), object));
            let mut pages = TlbPages::new();
            if let Some((cpus, copies)) = space.forget_runs(object, runs, &mut pages, from) {
                smp::flush_tlb_pages(&cpus, &mut pages);
                space.flushed();
                if let Some(copies) = copies {
                    copies.finish();
                }
            }
        }
    }

    /// The live mapper, but `except`, with the least key past `after`: see
    /// [`Vmo::forget_one_at_a_time`].
    fn next_mapper(
        &self,
        except: Option<&AddressSpace>,
        after: Option<(usize, u64)>,
    ) -> Option<(Arc<AddressSpace>, u64)> {
        let mappers = self.mappers.lock();
        mappers
            .iter()
            .filter(|mapper| {
                except.is_none_or(|space| !core::ptr::eq(mapper.space.as_ptr(), space))
            })
            .map(|mapper| ((mapper.space.as_ptr().addr(), mapper.object), mapper))
            .filter(|&(key, _)| after.is_none_or(|after| key > after))
            .filter(|(_, mapper)| mapper.space.strong_count() > 0)
            .min_by_key(|&(key, _)| key)
            .and_then(|(_, mapper)| Some((mapper.space.upgrade()?, mapper.object)))
    }
}

/// One address space phase two asks, and what came of asking.
struct Visit {
    /// The space.
    space: Arc<AddressSpace>,
    /// The id it maps the object by.
    object: u64,
    /// Whether it took translations down, so that its shootdown is counted
    /// pending until [`AddressSpace::flushed`].
    forgotten: bool,
    /// What a cut took out of its shadow, to be released after the
    /// shootdown.
    copies: Option<ShadowCopies>,
}

/// Phase two with the list: ask every space in `visits`, then one shootdown
/// for all of them and `own`'s, then release what cuts took.
fn forget_together(
    mut visits: Vec<Visit>,
    runs: &[(u64, u64)],
    mut own: Option<Own<'_>>,
    from: Option<&Vmo>,
) {
    let mut cpus = CpuSet::empty();
    let mut pages = TlbPages::new();
    if let Some((own_cpus, own_pages)) = own.as_mut().and_then(|own| own.shootdown.as_mut()) {
        smp::add_cpus(&mut cpus, own_cpus);
        pages.add_all(own_pages);
    }
    for visit in &mut visits {
        if let Some((theirs, copies)) =
            visit
                .space
                .forget_runs(visit.object, runs, &mut pages, from)
        {
            smp::add_cpus(&mut cpus, &theirs);
            visit.forgotten = true;
            visit.copies = copies;
        }
    }

    smp::flush_tlb_pages(&cpus, &mut pages);

    for visit in &visits {
        if visit.forgotten {
            visit.space.flushed();
        }
    }
    if let Some(own) = own
        && own.shootdown.is_some()
    {
        own.space.flushed();
    }
    // What a cut took out of private mappings' shadows. Their translations
    // were among the addresses the shootdown above reached, so after it
    // nothing reaches them. The spaces are dropped here with no lock held:
    // the last reference to a space that exited meanwhile may be one of
    // these, and dropping it detaches it from this object's list.
    for visit in visits {
        if let Some(copies) = visit.copies {
            copies.finish();
        }
    }
}

/// Run `own`'s shootdown, if it still has one to run, on its own.
fn own_shootdown(own: Option<Own<'_>>) {
    if let Some(mut own) = own
        && let Some((cpus, pages)) = own.shootdown.as_mut()
    {
        smp::flush_tlb_pages(cpus, pages);
        own.space.flushed();
    }
}

/// What a cut took out of a private mapping's shadow, for
/// [`AddressSpace::forget_runs`]'s caller to give back after its shootdown.
#[derive(Debug)]
pub(crate) struct ShadowCopies {
    /// The shadow.
    pub(crate) shadow: Arc<Vmo>,
    /// Its pages from `first` on, taken out -- or `None` if there was no
    /// memory to list them, and they are decommitted after instead.
    pub(crate) taken: Option<Retired>,
    /// The first page the cut takes.
    pub(crate) first: u64,
}

impl ShadowCopies {
    /// Give the copies back, once no processor can reach them.
    fn finish(self) {
        match self.taken {
            Some(taken) => {
                for (_, frame) in taken.frames {
                    let _ = mm::release_frame(frame);
                }
            }
            // Through all three phases of the shadow's own: its one mapper
            // took its translations down already, so this only releases.
            None => {
                let _ = self.shadow.decommit_from(self.first);
            }
        }
    }
}

impl Vmo {
    /// Make the object at least `pages` pages long.
    ///
    /// What `mremap` growing a shared mapping does to the object behind it,
    /// as Linux grows the shmem file behind `MAP_SHARED | MAP_ANONYMOUS`. The
    /// new pages are uncommitted, so they read as zeros and cost nothing until
    /// touched. Never shrinks: another mapping of the object may still reach
    /// the pages a shorter length would put out of range.
    pub(crate) fn grow_to(&self, pages: u64) {
        let _ = self.len.fetch_max(pages, Ordering::Relaxed);
    }

    /// Move `count` pages of `from`, starting at page `first`, into this
    /// object at pages `0..count`, and hand back what `from` must still have
    /// forgotten.
    ///
    /// Moved, not copied or shared: each frame leaves `from`'s list and joins
    /// this one with the reference it already had, so no page is allocated,
    /// copied or released. What `mremap` does with a private mapping it
    /// resizes, so that the region can be given an object of its own length
    /// without the cost of its contents. A frame a `fork` child still shares
    /// keeps its count above one, and the moved region keeps its
    /// copy-on-write marking, so the next write copies it as it would have.
    ///
    /// The [`Retired`] names the pages that left `from`, which every other
    /// space mapping `from` must forget — not for this object's sake but for
    /// its own: when this object later releases one of them, it asks only the
    /// spaces that map *it*. It releases nothing; give it to `from`'s
    /// [`Vmo::retire`] once the caller's lock is gone.
    ///
    /// Pages past this object's end stay in `from`. `from` must not be this
    /// object: the two locks are taken one after the other, never together.
    ///
    /// # Errors
    ///
    /// [`Unmoved::Held`], moving nothing, if any page of the range is held. A
    /// device reaches a held page at its frame, and a mapping moved to a new
    /// object would fault in a different frame at the same place: the program
    /// and the device would silently stop sharing the page. Leaving the held
    /// page behind in `from` is exactly that split, so the whole move is
    /// refused rather than part of it, as a held page's `replace` is.
    pub(crate) fn adopt_pages(
        &self,
        from: &Vmo,
        first: u64,
        count: u64,
    ) -> Result<Retired, Unmoved> {
        let count = count.min(self.len_pages());
        let end = first.saturating_add(count);
        let moved = {
            let source = from.pages.lock();
            if let Some((&index, _)) = source.held.range(first..end).next() {
                return Err(Unmoved::Held(HeldPage { index }));
            }
            let listed = source.frames.range(first..end).count();
            let mut moved =
                crate::fallible::try_filled((0, 0), listed).map_err(|_| Unmoved::NoMemory)?;
            for (slot, (&index, &frame)) in moved.iter_mut().zip(source.frames.range(first..end)) {
                *slot = (index, frame);
            }
            moved
        };
        // Named here as well as in `from` until `release_moved`: nothing but
        // the caller, under its space's lock, can reach either object's list
        // meanwhile, and this one is emptied again if the move goes no
        // further.
        let mut pages = self.pages.lock();
        for &(index, frame) in &moved {
            if crate::fallible::insert(&mut pages.frames, index - first, frame).is_err() {
                pages.frames.clear();
                return Err(Unmoved::NoMemory);
            }
        }
        Ok(Retired {
            frames: moved,
            release: false,
        })
    }

    /// Finish [`Vmo::adopt_pages`] on the object the pages came from: take
    /// them out of its list, now that the new object names them. Allocates
    /// nothing.
    pub(crate) fn release_moved(&self, moved: &Retired) {
        let mut pages = self.pages.lock();
        for (index, _) in &moved.frames {
            let _ = pages.frames.remove(index);
        }
    }

    /// Undo [`Vmo::adopt_pages`] on the object the pages went to, for an
    /// `mremap` that fails after it: forget them, without releasing them,
    /// since the object they came from still names every one.
    pub(crate) fn disown_pages(&self) {
        let named = core::mem::take(&mut self.pages.lock().frames);
        drop(named);
    }

    /// Hold `pages` pages from `first` in place, for a device to be given
    /// their addresses.
    ///
    /// Each page is committed, and copied first if a `fork` left it shared,
    /// so that the frame a device reads and writes is the one this object
    /// names and no one else's. While a [`Held`] for a page lives, the page
    /// keeps that frame: decommitting skips it, a copy-on-write replace is
    /// refused, a `fork` copies it rather than sharing it, and `mremap`
    /// refuses to move it. Holds nest; a page goes back to ordinary when the
    /// last one is dropped.
    ///
    /// A copied page's shared original leaves in three phases, so a mapping
    /// that still reached the original faults the held copy in: the program
    /// and the device see one page.
    ///
    /// The guard keeps the object alive, and with it every frame it holds, so
    /// forgetting a guard — what a caller does when a device may still hold
    /// the addresses — keeps the frames out of the allocator for good.
    ///
    /// Must not be called holding a spin lock.
    ///
    /// # Errors
    ///
    /// [`VmoError::OutOfRange`] for no pages or a range past the end, and
    /// [`VmoError::OutOfMemory`] when a frame cannot be had. Pages committed
    /// before a failure stay committed, as [`Vmo::commit`] would leave them,
    /// and none is held.
    pub(crate) fn hold(self: &Arc<Self>, first: u64, pages: u64) -> Result<Held, VmoError> {
        let len = self.len_pages();
        let end = first
            .checked_add(pages)
            .filter(|&end| pages > 0 && end <= len)
            .ok_or(VmoError::OutOfRange {
                index: first,
                pages: len,
            })?;
        let count = usize::try_from(pages).map_err(|_| VmoError::OutOfMemory)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(count)
            .map_err(|_| VmoError::OutOfMemory)?;

        // Room for every original a copy could displace, before any is.
        let mut displaced = Retired::with_room(count).map_err(|_| VmoError::OutOfMemory)?;
        let outcome = {
            let mut state = self.pages.lock();
            if (first..end).any(|index| state.held.get(&index) == Some(&u32::MAX)) {
                return Err(VmoError::OutOfMemory);
            }
            let outcome = (first..end).try_for_each(|index| {
                // NOALLOC: `frames` was given room for every page above.
                frames.push(state.unshared(index, &mut displaced)?);
                Ok(())
            });
            outcome.and_then(|()| state.count_held(first, end))
        };
        // Whatever was copied before a failure is copied all the same, and
        // its original has to leave properly either way.
        if !displaced.is_empty() {
            self.retire(displaced, None);
        }
        outcome?;

        Ok(Held {
            vmo: Arc::clone(self),
            first,
            frames,
        })
    }
}

impl Pages {
    /// The frame page `index` can be held at: its own if nobody else holds a
    /// reference to it, or an exclusive copy otherwise, with the shared
    /// original put in `displaced` to be retired.
    fn unshared(&mut self, index: u64, displaced: &mut Retired) -> Result<Frame, VmoError> {
        if let Some(frame) = self
            .frames
            .get(&index)
            .copied()
            .filter(|&frame| mm::frame_references(frame) <= 1)
        {
            return Ok(frame);
        }
        // The displaced original needs room in the list before it leaves this
        // one (finding F-23); `hold` made room for every page.
        if !displaced.has_room() {
            return Err(VmoError::OutOfMemory);
        }
        let (frame, shared) = self.exclusive(index)?;
        if let Some(shared) = shared {
            displaced.record(index, shared);
        }
        Ok(frame)
    }

    /// Count one more hold on every page of `first..end`. On running out of
    /// memory part-way, the counts already raised go back down, and nothing
    /// is held.
    fn count_held(&mut self, first: u64, end: u64) -> Result<(), VmoError> {
        for index in first..end {
            if let Some(count) = self.held.get_mut(&index) {
                *count += 1;
            } else if crate::fallible::insert(&mut self.held, index, 1).is_err() {
                self.uncount_held(first, index);
                return Err(VmoError::OutOfMemory);
            }
        }
        Ok(())
    }

    /// Undo [`Pages::count_held`] for `first..end`.
    fn uncount_held(&mut self, first: u64, end: u64) {
        for index in first..end {
            match self.held.get_mut(&index) {
                Some(1) => {
                    let _ = self.held.remove(&index);
                }
                Some(count) => *count -= 1,
                None => {}
            }
        }
    }

    /// The frame page `index` can be written through without anyone else
    /// seeing it: a zeroed one if the page has none, or a copy of the one a
    /// `fork` left it sharing — which is taken out and handed back second,
    /// still referenced, for the caller to retire.
    fn exclusive(&mut self, index: u64) -> Result<(Frame, Option<Frame>), VmoError> {
        // The section first, so that the frame is not had and then lost.
        let held = crate::fallible::reserve().map_err(|_| VmoError::OutOfMemory)?;
        let fresh = mm::allocate_user_frame().ok_or(VmoError::OutOfMemory)?;
        match crate::fallible::insert_held(&held, &mut self.frames, index, fresh) {
            Some(shared) => {
                mm::copy_frame(fresh, shared);
                Ok((fresh, Some(shared)))
            }
            None => {
                mm::zero_frame(fresh);
                Ok((fresh, None))
            }
        }
    }

    /// [`Vmo::commit_run`]'s insert, under the lock: every page of
    /// `first..first + pages` absent, then each given its frame of the run
    /// from `block`, or none of them if one cannot be recorded.
    fn insert_run(&mut self, first: u64, block: Frame, pages: u64) -> Result<Frame, RunError> {
        let end = first.saturating_add(pages);
        if self.frames.range(first..end).next().is_some() {
            return Err(RunError::Committed);
        }
        let done = (0..pages)
            .take_while(|&page| {
                crate::fallible::insert(&mut self.frames, first + page, block + page).is_ok()
            })
            .count() as u64;
        if done == pages {
            return Ok(block);
        }
        // Under the same lock nobody has seen them: out again.
        for page in 0..done {
            let _ = self.frames.remove(&(first + page));
        }
        Err(RunError::OutOfMemory)
    }

    /// [`Vmo::give_back_run`]'s take, under the lock: from page `from` of the
    /// run's `pages`, each page that still holds the run's frame and is not
    /// held, into `out` until it is full. How many were taken, and the page
    /// to go on from.
    fn take_ours(
        &mut self,
        first: u64,
        block: Frame,
        (from, pages): (u64, u64),
        out: &mut [(u64, Frame)],
    ) -> (usize, u64) {
        let mut taken = 0;
        let mut page = from;
        while page < pages && taken < out.len() {
            let index = first + page;
            let ours =
                self.frames.get(&index) == Some(&(block + page)) && !self.held.contains_key(&index);
            let frame = if ours {
                self.frames.remove(&index)
            } else {
                None
            };
            if let (Some(frame), Some(slot)) = (frame, out.get_mut(taken)) {
                *slot = (index, frame);
                taken += 1;
            }
            page += 1;
        }
        (taken, page)
    }

    /// Take the committed pages of `first..end` that are not held out of
    /// this list, lowest first, into `out` until it is full: how many were
    /// taken, and the page to go on from if any are left. Allocates nothing:
    /// a map's `remove` never does.
    fn take_into(
        &mut self,
        first: u64,
        end: u64,
        out: &mut [(u64, Frame)],
    ) -> (usize, Option<u64>) {
        let mut taken = 0;
        let mut from = first;
        while from < end {
            let Some(index) = self
                .frames
                .range(from..end)
                .map(|(&index, _)| index)
                .find(|index| !self.held.contains_key(index))
            else {
                return (taken, None);
            };
            let Some(slot) = out.get_mut(taken) else {
                return (taken, Some(index));
            };
            if let Some(frame) = self.frames.remove(&index) {
                *slot = (index, frame);
                taken += 1;
            }
            from = index.saturating_add(1);
        }
        (taken, None)
    }
}

/// Pages of a VMO held in place, from [`Vmo::hold`]. Dropping it lets them go.
#[derive(Debug)]
pub(crate) struct Held {
    /// The object, kept alive with its frames.
    vmo: Arc<Vmo>,
    /// The first page held.
    first: u64,
    /// The frame holding each page, in page order.
    frames: Vec<Frame>,
}

impl Held {
    /// The frame holding each page, in page order.
    pub(crate) fn frames(&self) -> &[Frame] {
        &self.frames
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let end = self.first.saturating_add(self.frames.len() as u64);
        self.vmo.pages.lock().uncount_held(self.first, end);
    }
}

impl Drop for Vmo {
    /// Give every committed page back.
    ///
    /// Through [`crate::mm::release_frame`] rather than the allocator's
    /// `deallocate`, because a page this object shares with another is not
    /// this object's alone to free — the allocator refuses that, and the
    /// refusal is the point.
    ///
    /// No shootdown: an object is dropped when the last space naming it lets
    /// it go, and every space takes its translations of an object down, and
    /// waits for their shootdown, before it does.
    fn drop(&mut self) {
        for frame in self.pages.get_mut().frames.values() {
            let _ = mm::release_frame(*frame);
        }
    }
}
