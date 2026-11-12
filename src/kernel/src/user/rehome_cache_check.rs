//! The self-check of the page cache with kernel objects alone: `user::cache`
//! over `Vmo`s with a source of the check's own, in `Job`s made for it, no file
//! system and no process. Its caller is `syscall::rehome_check`, and
//! it builds in the core's crate, so that boundary itself says it names no load
//! (`docs/microkernel/B5.md`, class A).
//!
//! What a job's task reads in, the check puts in as a fault would: with the
//! running task's group set to the job, so that each frame is charged to it.
//! An object made over a file's store goes on the cache's list as it is made,
//! and reclaim's list and its hand are the real ones. It runs before any file
//! system is mounted, so nothing else is on the list.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vfs::Errno;
use ferrix_vma::VmaFlags;

use crate::mm;
use crate::object::job::Job;
use crate::object::quota::{self, Mark, Quota, Resource};
use crate::sync::SpinLock;
use crate::user::cache::{self, Asked};
use crate::user::space::{Access, AddressSpace, FileMapping, FilePlace};
use crate::user::vmo::{self, Filler, ReclaimHook, Vmo, VmoError};

/// How many cases one run makes: the boot line's count.
const CASES: u64 = 13;

/// Run them, and answer how many cases they are. `Err` names the first thing
/// that was not true.
///
/// Called by `syscall::rehome_check`.
///
/// # Errors
///
/// The first thing that was not so, by name.
// CHECKS-ONLY: syscall/rehome_check.rs
pub fn run() -> Result<u64, &'static str> {
    cases().map(|()| CASES)
}

/// One run of every case.
fn cases() -> Result<(), &'static str> {
    check_reclaim_is_scoped()?;
    check_resident()?;
    check_a_reclaim_goes_round()?;
    check_reclaim_asked()?;
    check_a_written_page_stays()?;
    check_a_mapping_made_at_the_take()?;
    check_totals()?;
    check_read_present_answers_poison()?;
    check_a_poisoned_page_is_dropped()?;
    check_a_poisoned_read()?;
    check_no_heap_reclaim_in_interrupt_service()?;
    check_a_refused_heap_allocation_reclaims()?;
    check_a_refused_reservation_growth_reclaims()
}

/// A disk file's source of the check's own: each byte is [`pattern`], one page
/// may be written and not written back, and it may be reclaimed.
#[derive(Debug)]
struct Disk {
    /// The page written and not yet written back, or `u64::MAX`.
    written: AtomicU64,
}

impl Filler for Disk {
    fn fill(&self, vmo: &Vmo, index: u64) -> Result<(), Errno> {
        self.fill_one(vmo, index)
    }

    fn fill_one(&self, vmo: &Vmo, index: u64) -> Result<(), Errno> {
        if vmo.page(index).is_some() {
            return Ok(());
        }
        let frame = mm::allocate_user_frame().ok_or(Errno::ENOMEM)?;
        let base = mm::direct_map(frame * PAGE_SIZE) as *mut u8;
        for offset in 0..PAGE_SIZE {
            let byte = base.wrapping_add(offset as usize);
            // SAFETY: (FRAME) the frame was just allocated here, is in no
            // object yet and the direct map covers it whole.
            unsafe { byte.write(pattern(index * PAGE_SIZE + offset)) };
        }
        if !vmo.insert_absent(index, frame) {
            let _ = mm::release_frame(frame);
        }
        Ok(())
    }

    fn reclaimable(&self) -> bool {
        true
    }

    fn reclaim_clean(&self, pages: &mut Vec<u64>, drop: &mut dyn FnMut(&[u64]) -> u64) -> u64 {
        let written = self.written.load(Ordering::Relaxed);
        pages.retain(|&page| page != written);
        if pages.is_empty() { 0 } else { drop(pages) }
    }

    fn is_sourced(&self, _index: u64) -> bool {
        true
    }

    fn dirty_pages(&self) -> u64 {
        u64::from(self.written.load(Ordering::Relaxed) != u64::MAX)
    }
}

/// A byte of a file's contents, by its offset.
fn pattern(at: u64) -> u8 {
    (at.wrapping_mul(31) ^ (at >> 12)) as u8
}

/// A disk file's object of `pages`, on the cache's list, each page read in as a
/// task of `job` reads it, and the source to mark a page written with.
fn cached_file(job: &Job, pages: u64) -> Result<(Arc<Vmo>, Arc<Disk>), &'static str> {
    let disk = crate::fallible::try_arc(Disk {
        written: AtomicU64::new(u64::MAX),
    })
    .map_err(|_| "no source for a cached file")?;
    let object = Vmo::new_filled(pages, Some(Arc::clone(&disk) as Arc<dyn Filler>))
        .map_err(|_| "no object for a cached file")?;
    read_in(job, &object, 0, pages)?;
    Ok((object, disk))
}

/// Read pages `first..end` of `object` in as a task of `job` does.
fn read_in(job: &Job, object: &Arc<Vmo>, first: u64, end: u64) -> Result<(), &'static str> {
    crate::sched::set_current_group(job.quota_index());
    let outcome = (first..end).try_for_each(|index| {
        object
            .fill_for_fault(index)
            .map_err(|_| "a cached page would not read in")
    });
    crate::sched::set_current_group(quota::NONE);
    outcome
}

/// A disk file's object of `pages`, each page read in, charged to no one.
fn source_file(pages: u64) -> Result<Arc<Vmo>, &'static str> {
    let disk = crate::fallible::try_arc(Disk {
        written: AtomicU64::new(u64::MAX),
    })
    .map_err(|_| "no source for a cached file")?;
    let object = Vmo::new_filled(pages, Some(disk as Arc<dyn Filler>))
        .map_err(|_| "no object for a cached file")?;
    (0..pages).try_for_each(|index| {
        object
            .fill_one_page(index)
            .map_err(|_| "a cached page would not read in")
    })?;
    Ok(object)
}

/// A memory file's object of `pages`, on the cache's list, written by a task of
/// `job`.
fn memory_file(job: &Job, pages: u64) -> Result<Arc<Vmo>, &'static str> {
    let object = Vmo::new_filled(pages, None).map_err(|_| "no object for a memory file")?;
    object.set_file_len(pages * PAGE_SIZE);
    crate::sched::set_current_group(job.quota_index());
    let outcome = (0..pages).try_for_each(|index| {
        object
            .commit_and_write(index, 0, &[1])
            .map_err(|_| "a memory file's page was refused")
    });
    crate::sched::set_current_group(quota::NONE);
    outcome.map(|()| object)
}

/// Whether every byte of the object's pages reads as its source has it, through
/// a mapping's fault and through the object.
fn reads_as_its_source(object: &Arc<Vmo>, pages: u64) -> Result<bool, &'static str> {
    let space = AddressSpace::new().map_err(|_| "no space to read a file back through")?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            pages * PAGE_SIZE,
            VmaFlags::READ,
            Arc::clone(object),
            0,
            FileMapping {
                file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
                may_write: false,
            },
        )
        .map_err(|_| "no mapping to read a file back through")?;
    Ok((0..pages * PAGE_SIZE).step_by(97).all(|offset| {
        space
            .with_page(at + offset, Access::READ, |byte| {
                // SAFETY: (FRAME) `with_page` passes the direct-map address of
                // the byte, translated under the space's lock, held meanwhile.
                unsafe { core::ptr::read(byte as *const u8) }
            })
            .is_ok_and(|byte| byte == pattern(offset))
    }))
}

/// A root job, one under it, a sibling, and a child of the first.
type Jobs = (Arc<Job>, Arc<Job>, Arc<Job>, Arc<Job>);

/// A root job and three jobs: one, a sibling, and a child of the first.
fn jobs() -> Result<Jobs, &'static str> {
    let root = Job::new_root().map_err(|_| "no root job for the cache check")?;
    let one = root.new_child().map_err(|_| "no job for the cache check")?;
    let sibling = root.new_child().map_err(|_| "no job for the cache check")?;
    let child = one.new_child().map_err(|_| "no job for the cache check")?;
    Ok((root, one, sibling, child))
}

/// Reclaim gives back clean pages charged at or beneath its job, never a
/// sibling's or a child's under its `memory.min`, and each reads as its source.
///
/// Verifies: L.object.109
fn check_reclaim_is_scoped() -> Result<(), &'static str> {
    let (_root, one, sibling, child) = jobs()?;
    if !child.set_mark(Mark::Min, 256 * PAGE_SIZE) {
        return Err("a job refused a memory.min");
    }
    let (mine, _) = cached_file(&one, 8)?;
    let (theirs, _) = cached_file(&sibling, 8)?;
    let (spared, _) = cached_file(&child, 8)?;
    let done = cache::reclaim(one.quota_index(), 64);
    if theirs.committed() != 8 {
        return Err("reclaim took a sibling's pages");
    }
    if spared.committed() != 8 {
        return Err("reclaim took a child's pages at or under its memory.min");
    }
    if done.stolen != 8 || mine.committed() != 0 {
        return Err("reclaim did not give back the clean pages charged to its job");
    }
    if !reads_as_its_source(&mine, 8)? {
        return Err("a page reclaim gave back did not read as its source has it");
    }
    Ok(())
}

/// `resident` counts the cached pages charged to a job and beneath it, a disk
/// file's apart from a memory filesystem's.
///
/// Verifies: L.object.111
fn check_resident() -> Result<(), &'static str> {
    let (_root, one, sibling, child) = jobs()?;
    let (_mine, _) = cached_file(&one, 8)?;
    let (_beneath, _) = cached_file(&child, 2)?;
    let (_theirs, _) = cached_file(&sibling, 16)?;
    let _memory = memory_file(&one, 3)?;
    if cache::resident(one.quota_index()) != (10, 3) {
        return Err("resident did not count a job's cached pages, a memory file's apart");
    }
    Ok(())
}

/// A reclaim begins where the last stopped, so a page is not taken again before
/// the pages after it have been looked at.
///
/// Verifies: L.object.191
fn check_a_reclaim_goes_round() -> Result<(), &'static str> {
    let (_root, one, _sibling, _child) = jobs()?;
    let (first, _) = cached_file(&one, 16)?;
    let (second, _) = cached_file(&one, 16)?;
    let scope = one.quota_index();
    let done = cache::reclaim(scope, 4);
    // The hand stands wherever the last reclaim left it: one of the two.
    let file = if first.committed() == 12 {
        &first
    } else {
        &second
    };
    if done.stolen != 4 || file.committed() != 12 || (0..4).any(|i| file.page(i).is_some()) {
        return Err("a first reclaim of 4 pages did not take the first 4 pages of one file");
    }
    // Hot: read in again. The hand has gone past them.
    read_in(&one, file, 0, 4)?;
    let done = cache::reclaim(scope, 4);
    if done.stolen != 4 || (0..4).any(|index| file.page(index).is_none()) {
        return Err("a reclaim took a page again before the pages after it had been looked at");
    }
    if (4..8).any(|index| file.page(index).is_some()) {
        return Err("a reclaim did not go on from where the last one stopped");
    }
    Ok(())
}

/// `reclaim_asked` in rounds of at most 32 pages, `Short` after 16 empty rounds,
/// `Interrupted` between rounds, and at no scope `Short` for any count but zero.
///
/// Verifies: L.object.183
fn check_reclaim_asked() -> Result<(), &'static str> {
    let (_root, one, sibling, _child) = jobs()?;
    let scope = one.quota_index();
    let (file, _) = cached_file(&one, 64)?;
    let memory = memory_file(&one, 4)?;
    let (theirs, _) = cached_file(&sibling, 8)?;
    let never = || false;
    if !matches!(cache::reclaim_asked(scope, 0, &never), Asked::Given) || file.committed() != 64 {
        return Err("a reclaim asked for nothing did not answer Given taking nothing");
    }
    let rounds = AtomicU64::new(0);
    let counted = || {
        let _ = rounds.fetch_add(1, Ordering::Relaxed);
        false
    };
    if !matches!(cache::reclaim_asked(scope, 33, &counted), Asked::Given)
        || file.committed() > 64 - 33
    {
        return Err("a reclaim asked for 33 pages did not give them back");
    }
    if rounds.load(Ordering::Relaxed) != 2 {
        return Err("a reclaim asked for 33 pages was not two rounds of at most 32");
    }
    if !matches!(cache::reclaim_asked(scope, 1 << 18, &never), Asked::Short) {
        return Err("a reclaim asked for more than there is did not answer Short");
    }
    if file.committed() != 0 {
        return Err("a reclaim asked for more than there is left a file's clean page");
    }
    if memory.committed() != 4 {
        return Err("a reclaim asked for more than there is took a memory file's page");
    }
    if theirs.committed() != 8 {
        return Err("a reclaim asked for pages took a sibling's");
    }
    if !reads_as_its_source(&file, 64)? {
        return Err("a page reclaim_asked took did not read as its source has it");
    }
    let held = file.committed();
    if !matches!(
        cache::reclaim_asked(scope, 32, &|| true),
        Asked::Interrupted
    ) || file.committed() != held
    {
        return Err("a reclaim interrupted before its first round did not stop taking nothing");
    }
    if !matches!(cache::reclaim_asked(quota::NONE, 1, &never), Asked::Short) {
        return Err("a reclaim asked of the whole machine did not answer Short");
    }
    if !matches!(cache::reclaim_asked(quota::NONE, 0, &never), Asked::Given) {
        return Err("a reclaim asked of the whole machine for nothing did not answer Given");
    }
    Ok(())
}

/// Whether [`check_shrinker`] answers: never once the check has ended.
static SHRINKER_ARMED: AtomicBool = AtomicBool::new(false);
/// What the shrinker gives back while armed, in bytes.
static SHRINKER_GIVES: AtomicU64 = AtomicU64::new(0);
/// What it was last asked for, in bytes.
static SHRINKER_ASKED: AtomicU64 = AtomicU64::new(0);
/// How many times it has been asked.
static SHRINKER_CALLS: AtomicU64 = AtomicU64::new(0);
/// The slot in the scope it asks about.
static SHRINKER_IN_SCOPE: AtomicU32 = AtomicU32::new(0);
/// The slot out of the scope it asks about.
static SHRINKER_OUT: AtomicU32 = AtomicU32::new(0);
/// Whether the last ask named the slots as it should.
static SHRINKER_NAMED: AtomicBool = AtomicBool::new(false);

/// The check's shrinker: nothing once the check has ended.
fn check_shrinker(takes: &dyn Fn(u32) -> bool, want: u64) -> u64 {
    if !SHRINKER_ARMED.load(Ordering::SeqCst) {
        return 0;
    }
    let inside = SHRINKER_IN_SCOPE.load(Ordering::SeqCst);
    let outside = SHRINKER_OUT.load(Ordering::SeqCst);
    SHRINKER_NAMED.store(takes(inside) && !takes(outside), Ordering::SeqCst);
    SHRINKER_ASKED.store(want, Ordering::SeqCst);
    let _ = SHRINKER_CALLS.fetch_add(1, Ordering::SeqCst);
    SHRINKER_GIVES.load(Ordering::SeqCst)
}

// CHECKS-ONLY: syscall/rehome_check.rs
/// A reclaim short of its pages asks the shrinker for the rest in bytes, naming
/// only the jobs at the scope or beneath it, and answers its bytes apart; heap
/// alone answers a reclaim asked for by count.
///
/// Before the load registers the file system's shrinker, which replaces the
/// check's, since a second registration replaces the first and there is no way
/// back from one: `syscall::rehome_check::stage_shrinker`, ahead of `register_load`.
/// No job can read pages in before the scheduler runs, so the cases with pages
/// are at no scope, which takes any page, and hold nothing of anyone's: nothing
/// else is on the list yet.
///
/// Verifies: L.object.186, L.object.183
///
/// # Errors
///
/// The first thing that was not so, by name.
pub fn check_the_shrinker() -> Result<u64, &'static str> {
    let parent = Quota::new(None).map_err(|_| "no quota slot for the shrinker's case")?;
    let scope = Quota::new(Some(&parent)).map_err(|_| "no quota slot for the shrinker's case")?;
    let inside = Quota::new(Some(&scope)).map_err(|_| "no quota slot for the shrinker's case")?;
    let sibling = Quota::new(Some(&parent)).map_err(|_| "no quota slot for the shrinker's case")?;
    SHRINKER_IN_SCOPE.store(inside.index(), Ordering::SeqCst);
    SHRINKER_OUT.store(sibling.index(), Ordering::SeqCst);
    SHRINKER_GIVES.store(777, Ordering::SeqCst);
    // Using more than its `memory.min` of zero: spared otherwise.
    quota::charge(inside.index(), Resource::Memory, PAGE_SIZE)
        .map_err(|_| "the shrinker's case's memory would not charge")?;
    cache::install_shrinker(check_shrinker);
    SHRINKER_ARMED.store(true, Ordering::SeqCst);
    let outcome = (|| {
        let short = cache::reclaim(scope.index(), 3);
        if SHRINKER_CALLS.load(Ordering::SeqCst) == 0 {
            return Err("a reclaim short of its pages did not ask the shrinker");
        }
        if SHRINKER_ASKED.load(Ordering::SeqCst) != 3 * PAGE_SIZE {
            return Err("a reclaim did not ask the shrinker for the rest in bytes");
        }
        if !SHRINKER_NAMED.load(Ordering::SeqCst) {
            return Err("a reclaim named the shrinker a job outside its scope, or not one in it");
        }
        if short.stolen != 0 || short.bytes() != 777 {
            return Err("a reclaim did not answer the shrinker's bytes apart from the pages");
        }
        let file = source_file(4)?;
        let calls = SHRINKER_CALLS.load(Ordering::SeqCst);
        let enough = cache::reclaim(quota::NONE, 2);
        if enough.stolen != 2 || SHRINKER_CALLS.load(Ordering::SeqCst) != calls {
            return Err("a reclaim that found all its pages asked the shrinker");
        }
        let part = cache::reclaim(quota::NONE, 5);
        if part.stolen != 2 || file.committed() != 0 {
            return Err("a reclaim did not give back the pages it found before the shrinker");
        }
        if SHRINKER_ASKED.load(Ordering::SeqCst) != 3 * PAGE_SIZE {
            return Err("a reclaim that found some pages did not ask the shrinker for the rest");
        }
        SHRINKER_GIVES.store(8192, Ordering::SeqCst);
        if !matches!(
            cache::reclaim_asked(scope.index(), 2, &|| false),
            Asked::Given
        ) {
            return Err("a reclaim asked for 2 pages was not given them by the heap");
        }
        SHRINKER_GIVES.store(0, Ordering::SeqCst);
        if !matches!(
            cache::reclaim_asked(scope.index(), 2, &|| false),
            Asked::Short
        ) {
            return Err("a reclaim asked for 2 pages, given nothing in 16 rounds, was not Short");
        }
        Ok(3)
    })();
    SHRINKER_ARMED.store(false, Ordering::SeqCst);
    quota::uncharge(inside.index(), Resource::Memory, PAGE_SIZE);
    outcome
}

/// A page written and not written back stays, and reads back as last written:
/// reclaim takes only what the source hands it.
///
/// Verifies: L.object.188
fn check_a_written_page_stays() -> Result<(), &'static str> {
    const WRITTEN: &[u8] = b"written and not yet written back";
    let (_root, one, _sibling, _child) = jobs()?;
    let (file, disk) = cached_file(&one, 8)?;
    file.write_page(3, 40, WRITTEN)
        .map_err(|_| "a page of the cached file would not write")?;
    disk.written.store(3, Ordering::Relaxed);
    // A source whose count moves tells its object (`Filler::dirty_pages`).
    file.dirty_changed();
    let done = cache::reclaim(one.quota_index(), 64);
    if done.stolen != 7 || file.page(3).is_none() {
        return Err("reclaim took a page written and not yet written back");
    }
    let mut out = [0_u8; WRITTEN.len()];
    file.read_page(3, 40, &mut out)
        .map_err(|_| "the written page would not read")?;
    if out != *WRITTEN {
        return Err("a page written and kept did not read back as last written");
    }
    Ok(())
}

/// What [`make_a_mapping`] works on.
static AT_THE_TAKE: SpinLock<Option<AtTheTake>> = SpinLock::new(None);
/// The file and the space the mapping is made in.
type AtTheTake = (Arc<Vmo>, Arc<AddressSpace>);
/// Set once the hook has written through its mapping.
static WROTE_AT_THE_TAKE: AtomicBool = AtomicBool::new(false);

/// Between the source's answer and the take: a shared mapping that may write is
/// made and written.
fn make_a_mapping(_object: &Vmo) {
    let Some((object, space)) = AT_THE_TAKE.lock().clone() else {
        return;
    };
    let Ok(at) = space.map_file(
        FilePlace::Anywhere(None),
        PAGE_SIZE,
        VmaFlags {
            shared: true,
            ..VmaFlags::READ_WRITE
        },
        object,
        0,
        FileMapping {
            file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
            may_write: true,
        },
    ) else {
        return;
    };
    let wrote = space.with_page(at, Access::WRITE, |byte| {
        // SAFETY: (FRAME) `with_page` passes the direct-map address of the byte,
        // translated under the space's lock, held meanwhile.
        unsafe { core::ptr::write(byte as *mut u8, 0xC3) }
    });
    WROTE_AT_THE_TAKE.store(wrote.is_ok(), Ordering::SeqCst);
}

/// The hook of [`check_a_mapping_made_at_the_take`].
static TAKE_HOOK: ReclaimHook = ReclaimHook {
    armed_by: "the core's cache check",
    run: make_a_mapping,
};

/// A shared mapping that may write, made and written between the source's
/// answer and the take: the object is no longer reclaimable at the take.
///
/// Verifies: L.object.188
fn check_a_mapping_made_at_the_take() -> Result<(), &'static str> {
    let (_root, one, _sibling, _child) = jobs()?;
    let (file, _) = cached_file(&one, 1)?;
    let space = AddressSpace::new().map_err(|_| "no space for the mapping at the take")?;
    *AT_THE_TAKE.lock() = Some((Arc::clone(&file), Arc::clone(&space)));
    WROTE_AT_THE_TAKE.store(false, Ordering::SeqCst);
    vmo::arm_reclaim_hook(&TAKE_HOOK);
    let done = cache::reclaim(one.quota_index(), 1);
    vmo::disarm_reclaim_hook();
    let held = AT_THE_TAKE.lock().take();
    if !WROTE_AT_THE_TAKE.load(Ordering::SeqCst) {
        return Err("the mapping made at the take did not write");
    }
    if done.stolen != 0 || file.page(0).is_none() {
        return Err("reclaim took a page a shared mapping wrote between the answer and the take");
    }
    let mut out = [0_u8];
    file.read_page(0, 0, &mut out)
        .map_err(|_| "the page written at the take would not read")?;
    if out != [0xC3] {
        return Err("a page written at the take did not read back as written");
    }
    drop(held);
    Ok(())
}

/// Every live cached file's pages, whoever they are charged to: a disk file's, a
/// memory file's, those written, and those reclaim may take.
///
/// Verifies: L.object.189
fn check_totals() -> Result<(), &'static str> {
    let (_root, one, sibling, _child) = jobs()?;
    let before = cache::totals();
    let (file, disk) = cached_file(&one, 8)?;
    let _memory = memory_file(&sibling, 3)?;
    disk.written.store(2, Ordering::Relaxed);
    // A source whose count moves tells its object (`Filler::dirty_pages`).
    file.dirty_changed();
    let after = cache::totals();
    let _ = file;
    if after.disk != before.disk + 8 {
        return Err("totals did not count a disk file's pages");
    }
    if after.memory != before.memory + 3 {
        return Err("totals did not count a memory file's pages");
    }
    if after.dirty != before.dirty + 1 {
        return Err("totals did not count a page written and not written back");
    }
    if after.reclaimable != before.reclaimable + 7 {
        return Err("totals did not take a reclaimable object's dirty pages from what may go");
    }
    if after.cached() != before.cached() + 11 {
        return Err("totals' Cached is not both kinds of file");
    }
    if after.available(5) != before.available(5) + 7 {
        return Err("totals' MemAvailable is not what is free and what reclaim may take");
    }
    Ok(())
}

/// Run `work` with retirements quiet, and forget those it made.
fn retiring(work: impl FnOnce() -> Result<(), &'static str>) -> Result<(), &'static str> {
    let before = mm::retired_for_check();
    mm::hold_table_for_check();
    cache::quiet_for_check(true);
    let outcome = work();
    cache::quiet_for_check(false);
    mm::forget_retired_for_check(mm::retired_for_check().saturating_sub(before));
    outcome
}

/// A copy a machine check stopped answers `Poisoned(frame)` after checking the
/// page against the frame it read.
///
/// Verifies: L.object.260
fn check_read_present_answers_poison() -> Result<(), &'static str> {
    let (_root, one, _sibling, _child) = jobs()?;
    let (file, _) = cached_file(&one, 2)?;
    let frame = vmo::frame_for_check(&file, 1).ok_or("a page read in is not in the object")?;
    vmo::inject_read_poison(frame);
    let mut out = [0_u8; 8];
    if file.read_present(1, 0, &mut out) != Err(VmoError::Poisoned(frame)) {
        return Err("a read whose copy a machine check stopped did not answer Poisoned");
    }
    if file.read_present(0, 0, &mut out) != Ok(true) || out[0] != pattern(0) {
        return Err("a read of another page was stopped by poison, or read wrong");
    }
    Ok(())
}

/// The frame retired first, the page found by a walk of the list, taken only
/// through reclaim's test, with what came of it answered.
///
/// Verifies: L.object.259
fn check_a_poisoned_page_is_dropped() -> Result<(), &'static str> {
    let (_root, one, _sibling, _child) = jobs()?;
    let (file, disk) = cached_file(&one, 8)?;
    retiring(|| {
        // A clean page, mapped: out of the object and the mapping, frame retired.
        let space = AddressSpace::new().map_err(|_| "no space for the poison case")?;
        let at = space
            .map_file(
                FilePlace::Anywhere(None),
                8 * PAGE_SIZE,
                VmaFlags::READ,
                Arc::clone(&file),
                0,
                FileMapping {
                    file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
                    may_write: false,
                },
            )
            .map_err(|_| "no mapping for the poison case")?;
        space
            .with_page(at + 2 * PAGE_SIZE, Access::READ, |_| ())
            .map_err(|_| "the poison case's page would not fault in")?;
        let frame = vmo::frame_for_check(&file, 2).ok_or("page 2 is not in the object")?;
        if cache::drop_poisoned_for_check(frame) != "Clean" {
            return Err("a clean cached page's poison did not answer Clean");
        }
        if !mm::is_retired(frame) {
            return Err("a page dropped for poison left its frame not retired");
        }
        if file.page(2).is_some() {
            return Err("a page dropped for poison is still in the object");
        }
        let mapped = space
            .with_present_page(at + 2 * PAGE_SIZE, Access::READ, |_| ())
            .ok()
            .flatten()
            .is_some();
        if mapped {
            return Err("a page dropped for poison is still mapped");
        }

        // A frame nothing holds, one that cannot be retired.
        let loose = mm::allocate_frames(0).ok_or("no frame for the poison case")?;
        let not_cache = cache::drop_poisoned_for_check(loose);
        mm::deallocate_frames(loose, 0);
        if not_cache != "NotCache" {
            return Err("poison in a frame no cached page holds did not answer NotCache");
        }
        if cache::drop_poisoned_for_check(u64::MAX / PAGE_SIZE) != "NotRetired" {
            return Err("poison in a frame that cannot be retired did not answer NotRetired");
        }

        // A page asked for with a frame it does not hold: kept.
        let held = vmo::frame_for_check(&file, 3).ok_or("page 3 is not in the object")?;
        let other = mm::allocate_frames(0).ok_or("no frame for the poison case")?;
        let gone = cache::drop_poisoned_at_for_check(&file, 3, other);
        mm::deallocate_frames(other, 0);
        if gone != "NotCache" || vmo::frame_for_check(&file, 3) != Some(held) {
            return Err("a page asked for with a frame it does not hold was taken for poison");
        }

        // A held page and a written one: kept.
        let hold = file
            .hold(4, 1)
            .map_err(|_| "the poison case could not hold a page")?;
        let frame = vmo::frame_for_check(&file, 4).ok_or("page 4 is not in the object")?;
        let kept = cache::drop_poisoned_for_check(frame);
        drop(hold);
        if kept != "NotClean" || file.page(4) != Some(frame) {
            return Err("a held page was dropped for poison");
        }
        let frame = vmo::frame_for_check(&file, 5).ok_or("page 5 is not in the object")?;
        disk.written.store(5, Ordering::Relaxed);
        // A source whose count moves tells its object (`Filler::dirty_pages`).
        file.dirty_changed();
        if cache::drop_poisoned_for_check(frame) != "NotClean" || file.page(5) != Some(frame) {
            return Err("a page written and not written back was dropped for poison");
        }
        Ok(())
    })
}

/// A read that consumed poison goes round the fill loop only after a clean page
/// is dropped, and fails after one that is not.
///
/// Verifies: L.object.261
fn check_a_poisoned_read() -> Result<(), &'static str> {
    let (_root, one, _sibling, _child) = jobs()?;
    let (file, disk) = cached_file(&one, 4)?;
    retiring(|| {
        let frame = vmo::frame_for_check(&file, 1).ok_or("page 1 is not in the object")?;
        if !cache::poisoned_read(&file, 1, frame) {
            return Err("a clean page's poisoned read did not go round the fill again");
        }
        if file.page(1).is_some() || !mm::is_retired(frame) {
            return Err("a clean page's poisoned read did not drop the page and retire its frame");
        }
        read_in(&one, &file, 1, 2)?;
        let mut out = [0_u8];
        if file.read_page(1, 5, &mut out).is_err() || out != [pattern(PAGE_SIZE + 5)] {
            return Err("a clean page's poisoned read was not refilled with the source's bytes");
        }
        let frame = vmo::frame_for_check(&file, 2).ok_or("page 2 is not in the object")?;
        disk.written.store(2, Ordering::Relaxed);
        // A source whose count moves tells its object (`Filler::dirty_pages`).
        file.dirty_changed();
        if cache::poisoned_read(&file, 2, frame) {
            return Err("a written page's poisoned read went round the fill again");
        }
        if file.page(2) != Some(frame) {
            return Err("a written page's poisoned read dropped the page");
        }
        Ok(())
    })
}

/// Room for the blocks [`hold_every_free_frame`] takes of `free` frames:
/// one entry a largest block, and as many again of each smaller order and
/// for the frames other processors free meanwhile. Sized by blocks, not
/// frames: a list of every free frame was 64 MiB of heap on a 16 GiB
/// machine, more than the heap's largest block, and the check stopped the
/// boot on its own list (CI's "rustc on Ferrix", 2026-11-08).
fn held_room(free: u64) -> usize {
    let largest = free >> ferrix_frame::MAX_ORDER;
    let spare = 1024 * (u64::from(ferrix_frame::MAX_ORDER) + 1);
    usize::try_from(largest.saturating_mul(2).saturating_add(spare)).unwrap_or(usize::MAX / 32)
}

/// Take every free frame the allocator will give, largest blocks first, into
/// `held` until it is full: each as `(frame, order)`, given back by
/// [`mm::deallocate_frames`] at its order.
fn hold_every_free_frame(held: &mut Vec<(u64, u8)>) {
    for order in (0..=ferrix_frame::MAX_ORDER).rev() {
        while held.len() < held.capacity() {
            match mm::allocate_frames(order) {
                Some(frame) => held.push((frame, order)),
                None => break,
            }
        }
    }
}

/// The kernel heap's reclaim and kill are never begun in interrupt service
/// (L.mm.136, L.mm.138): `mm::heap_may_reclaim`, which both ask first, says
/// yes in this task, with interrupts on and nothing held, and no for the same
/// task marked as servicing an interrupt ([`crate::irq::serviced`]), where
/// only its interrupt test can say so. `check-stack-depth` leaves the kill
/// out of what a timer's interrupt reaches on this guard (`INTERRUPT_CANNOT`,
/// F-107); this is the check that fires when its interrupt test goes.
///
/// Verifies: L.mm.136, L.mm.138
fn check_no_heap_reclaim_in_interrupt_service() -> Result<(), &'static str> {
    if !mm::heap_may_reclaim() {
        return Err("a task with interrupts on and nothing held may not reclaim for the heap");
    }
    if crate::irq::serviced(mm::heap_may_reclaim) {
        return Err("the kernel heap may reclaim or kill inside interrupt service");
    }
    Ok(())
}

/// A page of kernel heap refused with every free frame held, and clean cache
/// to give back, is answered by a reclaim and served (L.mm.136): the heap's
/// count of refusals a reclaim let in grows. Another processor's allocation
/// may refill the headroom (L.mm.137) between the hold and the request, and
/// the request then finds room with no refusal: so up to three tries, of
/// which one must show the reclaim, and a request not served at all fails
/// at once.
///
/// Verifies: L.mm.136
fn check_a_refused_heap_allocation_reclaims() -> Result<(), &'static str> {
    const PAGES: u64 = 512;
    let file = source_file(PAGES)?;
    let outcome = (|| {
        for _ in 0..3 {
            let mut held: Vec<(u64, u8)> = Vec::new();
            held.try_reserve_exact(held_room(mm::free_frames()))
                .map_err(|_| "no room for the heap-reclaim check's list of frames")?;
            let before = mm::heap_reclaimed();
            let _ = mm::drain_caches();
            hold_every_free_frame(&mut held);
            // A page of heap: a block of its own, which no cached object serves.
            let mut page: Vec<u8> = Vec::new();
            let served = page.try_reserve_exact(PAGE_SIZE as usize).is_ok();
            let after = mm::heap_reclaimed();
            drop(page);
            for (frame, order) in held.drain(..) {
                mm::deallocate_frames(frame, order);
            }
            if !served {
                return Err(
                    "a page of heap refused at the floor, with clean cache to give back, \
                     was not served after a reclaim",
                );
            }
            if after > before {
                return Ok(());
            }
        }
        Err("a page of heap with every free frame held never reached the reclaim in three tries")
    })();
    drop(file);
    outcome
}

/// Frames the reservation growth's check asks for.
const GROWTH: u64 = 16;

/// Take frames, largest blocks first, into `held` until the free count is
/// one under `least`: each as `(frame, order)`, given back by
/// [`mm::deallocate_frames`] at its order. A block is taken only while the
/// count stays at `least - 1` or over, so the last are single frames.
fn hold_free_frames_down_to(held: &mut Vec<(u64, u8)>, least: u64) {
    for order in (0..=ferrix_frame::MAX_ORDER).rev() {
        let size = 1_u64 << order;
        while held.len() < held.capacity() && mm::free_frames() >= least.saturating_add(size - 1) {
            match mm::allocate_frames(order) {
                Some(frame) => held.push((frame, order)),
                None => break,
            }
        }
    }
}

/// A reservation's growth refused for want of free frames, with clean cache
/// to give back, is answered by a reclaim and admitted (L.mm.146): the free
/// count is held one under what a growth of [`GROWTH`] frames needs (the
/// watermark, the outstanding promise and the growth), so that the resize
/// alone is refused, and `mm::reservation_grow` must reclaim the check's
/// own cached file to admit it; its count of growths a reclaim let in
/// grows. Another processor may free frames between the hold and the
/// growth, which is then admitted with no refusal: so up to three tries, of
/// which one must show the reclaim, and a growth refused at all fails at
/// once. A writable btrfs mount grows its reservation so before every
/// operation (`docs/BTRFS-PRESSURE.md` W1-c).
///
/// Its negative control: `reservation_grow` without its reclaims (the
/// growth stays refused).
///
/// Verifies: L.mm.146
fn check_a_refused_reservation_growth_reclaims() -> Result<(), &'static str> {
    const PAGES: u64 = 512;
    let file = source_file(PAGES)?;
    let entry = mm::reservation_bind().ok_or("no reservation was free for the growth's check")?;
    let outcome = (|| {
        for _ in 0..3 {
            let mut held: Vec<(u64, u8)> = Vec::new();
            held.try_reserve_exact(held_room(mm::free_frames()))
                .map_err(|_| "no room for the growth check's list of frames")?;
            let before = mm::reservation_reclaimed();
            let _ = mm::drain_caches();
            let least = mm::watermark()
                .saturating_add(mm::outstanding_promise())
                .saturating_add(GROWTH);
            hold_free_frames_down_to(&mut held, least);
            let short = mm::reservation_resize(entry, GROWTH).is_err();
            let grown = short && mm::reservation_grow(entry, GROWTH).is_ok();
            let after = mm::reservation_reclaimed();
            // Give the promise back before the frames: a shrink is always
            // admitted (L.mm.76).
            let _ = mm::reservation_resize(entry, 0);
            for (frame, order) in held.drain(..) {
                mm::deallocate_frames(frame, order);
            }
            if short && !grown {
                return Err(
                    "a reservation's growth refused at the floor, with clean cache to give \
                     back, was not admitted after a reclaim",
                );
            }
            if short && after > before {
                return Ok(());
            }
        }
        Err(
            "a reservation's growth with the free count held under its need never reached the \
             reclaim in three tries",
        )
    })();
    mm::reservation_unbind(entry);
    drop(file);
    outcome
}
