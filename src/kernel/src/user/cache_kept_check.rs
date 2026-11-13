//! The walk the kept page-cache counts are held to, and their core self-check
//! (`L.user.239` to `L.user.241`, `docs/RSS-COUNTS.md` §9.6): check code
//! only, in no ring.
//!
//! [`walked_totals`] is what `user::cache::totals` counted for every reader
//! until the counts were kept, its body moved here unchanged but for the
//! dirty count's name: each live cache object's pages, a disk file's dirty
//! ones at most its pages, and of a reclaimable object's pages those not
//! dirty. It reads the cache's list and each object's pages and nothing of
//! the kept counts, and `/proc/meminfo` never falls back to it.
//!
//! [`run`] makes cache objects over sources of its own -- a memory
//! filesystem's, a reclaimable disk file's whose dirty count moves and tells,
//! a disk file's reclaim may not take, one dirty before its object is made --
//! changes their pages and page states, and after each change holds every
//! object's share to the walk's figures for it, and the machine's sum to the
//! walk on a quiet reading.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_errno::Errno;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use crate::mm;
use crate::user::cache::{self, Totals};
use crate::user::vmo::{self, Filler, Vmo};

/// What the page cache holds now, by a walk of every live object: the
/// checks' oracle for [`cache::totals`].
pub(crate) fn walked_totals() -> Totals {
    let mut out = Totals::default();
    for vmo in cache::live() {
        let pages = vmo.committed() as u64;
        if vmo.is_disk_file() {
            let dirty = vmo::dirty_pages_for_check(&vmo).min(pages);
            out.disk += pages;
            out.dirty += dirty;
            if vmo.reclaimable() {
                out.reclaimable += pages - dirty;
            }
        } else {
            out.memory += pages;
        }
    }
    out
}

// CHECKS-ONLY: stages_check.rs
/// Objects made and dropped at once, the list a read must not walk.
pub const MANY: usize = 4_096;

/// Readings of the machine's sum tried before a check gives up on finding
/// one quiet: two walks that agree around the kept read.
const QUIET_TRIES: u32 = 64;

// CHECKS-ONLY: stages_check.rs
/// Reads of [`cache::totals`] timed, with few objects and with [`MANY`].
pub const READS: u32 = 200;

// CHECKS-ONLY: stages_check.rs
/// What [`run`] found.
#[derive(Debug, Default)]
pub struct KeptCounted {
    /// Comparisons of a share or a sum with the walk, all equal.
    pub compared: u32,
    /// Readings that were not quiet and were tried again.
    pub retried: u32,
    /// Nanoseconds of [`READS`] reads with the check's few objects.
    pub few_ns: u64,
    /// Nanoseconds of [`READS`] reads with [`MANY`] more live.
    pub many_ns: u64,
    /// Whether `L.user.241`'s processor case ran: not with one processor.
    pub on_another: bool,
}

/// A disk file's source of the check's own: zeroed pages, a dirty count the
/// check sets and tells of, and reclaim's answer fixed at its making.
#[derive(Debug)]
struct Source {
    /// Pages written and not written back, as the check says.
    dirty: AtomicU64,
    /// Whether reclaim may take its clean pages.
    reclaimable: bool,
}

impl Filler for Source {
    fn fill(&self, vmo: &Vmo, index: u64) -> Result<(), Errno> {
        self.fill_one(vmo, index)
    }

    fn fill_one(&self, vmo: &Vmo, index: u64) -> Result<(), Errno> {
        if vmo.page(index).is_some() {
            return Ok(());
        }
        let frame = mm::allocate_user_frame().ok_or(Errno::ENOMEM)?;
        // SAFETY: (FRAME) the frame was just allocated here, is in no object
        // yet and the direct map covers it whole.
        unsafe {
            (mm::direct_map(frame * PAGE_SIZE) as *mut u8).write_bytes(0, PAGE_SIZE as usize);
        };
        if !vmo.insert_absent(index, frame) {
            let _ = mm::release_frame(frame);
        }
        Ok(())
    }

    fn reclaimable(&self) -> bool {
        self.reclaimable
    }

    fn is_sourced(&self, _index: u64) -> bool {
        true
    }

    fn dirty_pages(&self) -> u64 {
        self.dirty.load(Ordering::Relaxed)
    }
}

/// A disk file's object of `pages` pages over a new [`Source`], none
/// committed.
fn disk_object(
    pages: u64,
    reclaimable: bool,
    dirty: u64,
) -> Result<(Arc<Vmo>, Arc<Source>), &'static str> {
    let source = crate::fallible::try_arc(Source {
        dirty: AtomicU64::new(dirty),
        reclaimable,
    })
    .map_err(|_| "no source for a check object")?;
    let object = Vmo::new_filled(pages, Some(Arc::clone(&source) as Arc<dyn Filler>))
        .map_err(|_| "no check object")?;
    Ok((object, source))
}

/// Commit pages `first..end` of a disk object as a fill does.
fn fill(object: &Vmo, first: u64, end: u64) -> Result<(), &'static str> {
    (first..end).try_for_each(|index| {
        object
            .fill_one_page(index)
            .map_err(|_| "a check object's page would not fill")
    })
}

/// A memory filesystem's object of `pages` pages, each written.
fn memory_object(pages: u64) -> Result<Arc<Vmo>, &'static str> {
    let object = Vmo::new_filled(pages, None).map_err(|_| "no memory check object")?;
    object.set_file_len(pages * PAGE_SIZE);
    (0..pages).try_for_each(|index| {
        object
            .commit_and_write(index, 0, &[1])
            .map_err(|_| "a memory check object's page was refused")
    })?;
    Ok(object)
}

// CHECKS-ONLY: fs/btrfs_write_check/reclaim_check.rs
/// Each of `objects`' shares equals the walk's figures for it alone, and
/// the machine's sum equals the walk on a quiet reading. Answers the
/// comparisons made and the readings tried again.
///
/// # Errors
///
/// Which disagreed.
pub fn agrees(objects: &[&Arc<Vmo>]) -> Result<(u32, u32), &'static str> {
    let mut compared = 0;
    for object in objects {
        let (share, walked) = vmo::share_and_walked_for_check(object);
        if share != Some(walked) {
            return Err("an object's share is not the walk's figures for it");
        }
        compared += 1;
    }
    for retried in 0..QUIET_TRIES {
        let before = walked_totals();
        let kept = cache::totals();
        let after = walked_totals();
        if before == after && kept == before {
            return Ok((compared + 1, retried));
        }
        crate::sched::sleep_for(1_000_000);
    }
    Err("the machine's kept cache counts are not the walk on any quiet reading")
}

/// [`agrees`], noted in `checked`.
fn agree(objects: &[&Arc<Vmo>], checked: &mut KeptCounted) -> Result<(), &'static str> {
    let (compared, retried) = agrees(objects)?;
    checked.compared += compared;
    checked.retried += retried;
    Ok(())
}

/// The machine's sum equals the walk on a reading two agreeing walks
/// bracket, tried [`QUIET_TRIES`] times before it fails.
fn quiet_sum(checked: &mut KeptCounted) -> Result<(), &'static str> {
    agree(&[], checked)
}

// CHECKS-ONLY: stages_check.rs
/// [`run`], stopping the boot (FX-0978) on what disagreed.
pub fn run_or_stop() -> KeptCounted {
    match run() {
        Ok(counted) => counted,
        Err(problem) => crate::panic::fatal!(
            crate::panic::catalog::CACHE_COUNT_BROKEN,
            "the kept page-cache counts' core self-check failed: {problem}"
        ),
    }
}

/// The core self-check of the kept page-cache counts, on every profile:
/// each change a cache object's pages, page states and source's dirty count
/// can make, followed by every share against the walk's figures for it and
/// the machine's sum against the walk; a read with 4,096 objects more; and
/// a settle on processor 1 against its own record.
///
/// Verifies: L.user.239
/// Verifies: L.user.240
/// Verifies: L.user.241
///
/// # Errors
///
/// What disagreed, or what could not be set up.
fn run() -> Result<KeptCounted, &'static str> {
    let mut checked = KeptCounted::default();
    shares_follow_changes(&mut checked)?;
    a_read_walks_nothing(&mut checked)?;
    checked.on_another = a_settle_adds_here()?;
    Ok(checked)
}

/// Every kind of change moves the share it should, and the sum with it.
///
/// Verifies: L.user.239
fn shares_follow_changes(checked: &mut KeptCounted) -> Result<(), &'static str> {
    // A memory filesystem's pages, written; then some decommitted.
    let memory = memory_object(4)?;
    // A reclaimable disk file's, then its source's dirty count moving.
    let (disk, source) = disk_object(6, true, 0)?;
    // One reclaim may not take.
    let (kept, _kept_source) = disk_object(3, false, 0)?;
    // One dirty before its object was made.
    let (early, _early_source) = disk_object(2, true, 1)?;
    fill(&disk, 0, 6)?;
    fill(&kept, 0, 3)?;
    fill(&early, 0, 2)?;
    let all = [&memory, &disk, &kept, &early];
    agree(&all, checked)?;
    if vmo::share_for_check(&early) != Some((2, 1)) {
        return Err("a source dirty before its object was made is not counted");
    }

    source.dirty.store(2, Ordering::Relaxed);
    disk.dirty_changed();
    agree(&all, checked)?;

    // Page states: Dirty, then awaiting clean, then Clean again.
    if !disk.mark_dirty(4).map_err(|_| "no room for a page state")? {
        return Err("a committed page could not be marked Dirty");
    }
    if !disk.mark_dirty(5).map_err(|_| "no room for a page state")? {
        return Err("a committed page could not be marked Dirty");
    }
    agree(&all, checked)?;
    let begun = disk
        .begin_mapped_writes()
        .map_err(|_| "no room to begin a write-back")?;
    if begun.len() != 2 {
        return Err("the write-back did not take the two Dirty pages");
    }
    agree(&all, checked)?;
    disk.end_mapped_writes(None);
    agree(&all, checked)?;

    // The cap: the filesystem's count and a page state over one page each,
    // more dirty marks than pages.
    source.dirty.store(6, Ordering::Relaxed);
    disk.dirty_changed();
    if !disk.mark_dirty(0).map_err(|_| "no room for a page state")? {
        return Err("a committed page could not be marked Dirty");
    }
    agree(&all, checked)?;
    if vmo::share_for_check(&disk) != Some((6, 6)) {
        return Err("an object's dirty pages are not capped at its pages");
    }

    // Pages leave: a decommit of each kind.
    let _ = memory.decommit_range(1, 2);
    let _ = disk.decommit_range(0, 3);
    let _ = kept.decommit_range(2, 1);
    agree(&all, checked)?;

    // Objects dropped holding pages give everything back.
    drop(memory);
    drop(disk);
    drop(kept);
    drop(early);
    quiet_sum(checked)
}

/// A read takes no list of the cache however many objects it holds, and its
/// time is reported against a read with few.
///
/// Verifies: L.user.240
fn a_read_walks_nothing(checked: &mut KeptCounted) -> Result<(), &'static str> {
    let time = || {
        let start = crate::timer::now_nanos();
        for _ in 0..READS {
            let _ = core::hint::black_box(cache::totals());
        }
        crate::timer::now_nanos().saturating_sub(start)
    };
    checked.few_ns = time();
    let mut many: Vec<Arc<Vmo>> =
        crate::fallible::try_with_capacity(MANY).map_err(|_| "no room to list the objects")?;
    let before = walked_totals();
    for _ in 0..MANY {
        let object = memory_object(1)?;
        // NOALLOC: room for MANY.
        let _ = crate::fallible::push_within(&mut many, object);
    }
    #[cfg(ferrix_checks = "on")]
    let listed = cache::listed_for_check();
    checked.many_ns = time();
    #[cfg(ferrix_checks = "on")]
    if cache::listed_for_check() != listed {
        return Err("a read of the kept cache counts walked the cache's list");
    }
    quiet_sum(checked)?;
    let with = cache::totals();
    if with.memory < before.memory + MANY as u64 {
        return Err("the objects made did not count");
    }
    drop(many);
    quiet_sum(checked)
}

/// Pages committed on processor 1 move processor 1's record by exactly as
/// many, preemption held off around the change. `Ok(false)` with one
/// processor, where it does not run.
///
/// Verifies: L.user.241
fn a_settle_adds_here() -> Result<bool, &'static str> {
    let online = crate::smp::topology().map_or(1, crate::smp::Topology::online);
    if online < 2 {
        return Ok(false);
    }
    let mut on = CpuSet::empty();
    on.insert(1)
        .map_err(|_| "the cache check names a processor out of range")?;
    DONE.store(false, Ordering::SeqCst);
    MOVED.store(u64::MAX, Ordering::SeqCst);
    let task = crate::sched::spawn_on("cache-count-here", settle_here, 0, NICE_0_WEIGHT, 1, on)?;
    let deadline = crate::timer::now_nanos().saturating_add(5_000_000_000);
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err("the cache check's task on processor 1 never finished");
        }
        crate::sched::sleep_for(1_000_000);
    }
    if !DONE.load(Ordering::SeqCst) {
        return Err("the cache check's task on processor 1 could not run its case");
    }
    if MOVED.load(Ordering::SeqCst) != HERE_PAGES {
        return Err("pages committed on processor 1 did not move its own record by as many");
    }
    Ok(true)
}

/// Pages [`settle_here`] commits.
const HERE_PAGES: u64 = 8;

/// Whether [`settle_here`] ran its case.
static DONE: AtomicBool = AtomicBool::new(false);

/// How far [`settle_here`]'s processor's disk count moved.
static MOVED: AtomicU64 = AtomicU64::new(u64::MAX);

/// On processor 1: commit [`HERE_PAGES`] frames had beforehand to a disk
/// object with preemption held off, and note how far this processor's own
/// disk count moved.
fn settle_here(_argument: usize) {
    if crate::smp::this_cpu().map(|cpu| cpu.logical) != Some(1) {
        return;
    }
    let Ok((object, _source)) = disk_object(HERE_PAGES, true, 0) else {
        return;
    };
    let mut frames = [0; HERE_PAGES as usize];
    for frame in &mut frames {
        let Some(had) = mm::allocate_user_frame() else {
            return;
        };
        *frame = had;
    }
    crate::sched::preempt_disable();
    let before = cache::here_for_check();
    let mut all_in = true;
    for (index, &frame) in (0..).zip(&frames) {
        if !object.insert_absent(index, frame) {
            all_in = false;
            let _ = mm::release_frame(frame);
        }
    }
    let after = cache::here_for_check();
    crate::sched::preempt_enable();
    if all_in {
        MOVED.store(after[0].wrapping_sub(before[0]), Ordering::SeqCst);
        DONE.store(true, Ordering::SeqCst);
    }
    drop(object);
}
