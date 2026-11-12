//! The page cache, as reclaim sees it (`docs/CGROUPS.md` §10, stage 13's M2).
//!
//! Every file's pages live in a [`Vmo`] made by `Vmo::new_filled`
//! (`fs/pages.rs`). This module keeps a weak list of them, and from the list
//! does the two things a cgroup's memory controller needs of the cache:
//!
//! * [`reclaim`] gives back clean pages charged to a job and the jobs beneath
//!   it, so that a charge past `memory.max` can be retried before a program is
//!   killed, and so that a job over `memory.high` comes back down to it;
//! * [`resident`] counts what a job holds in the cache, for `memory.stat`'s
//!   `file` and `shmem`.
//!
//! # What may be reclaimed
//!
//! A page of a file on a disk, that the file's source can read again and the
//! filesystem has no newer copy of. The source says which ([`Filler::
//! reclaimable`], [`Filler::reclaim_clean`]): a read-only mount every page;
//! a writable one the pages written back since they were last written, and
//! of those not one a write is copying into or a writeback is reading, which
//! it holds still while they go -- a page taken between a write's copy and
//! its dirty mark would lose the write. A file on
//! a memory filesystem (tmpfs, `memfd`) has no source and nothing to read its
//! pages back from, and there is no swap: its pages are never reclaimed. Nor
//! is an object a shared mapping may write through, whose writes mark
//! nothing.
//!
//! A page leaves the way a truncation's leaves, by [`Vmo::decommit_range`]:
//! out of the object, out of every address space that maps it, a shootdown,
//! and then the frame is released and its charge with it. A program that
//! maps the page faults it in again from the source, `pgmajfault`.
//!
//! # Whose pages
//!
//! The frame record names the job a page is charged to (`mm::frame_owner`).
//! A reclaim at job `J` takes pages whose owner is `J` or beneath it, and no
//! other: a sibling's cache is never touched. A job whose use is at or under
//! its `memory.min` is spared by the reclaim of a job above it; one at or
//! under its `memory.low` is spared unless nothing else gave enough
//! (`quota::protected`).
//!
//! The choice of page and its removal are two steps with no lock between
//! them, so a page that changed hands in between (truncated and filled again
//! for another job) is taken all the same: a clean cache page evicted a
//! little early, which costs a read and never data. A page a writable file
//! wrote to meanwhile is not taken: the source's answer, and the stillness it
//! holds while the page goes, come after the pick. Nor is any page of an
//! object a shared mapping that may write was made of meanwhile: the take
//! looks at that under the pages lock it takes under
//! ([`Vmo::take_clean_range`]).
//!
//! # Where a reclaim starts
//!
//! Where the last one stopped ([`HAND`]): the place in the list, the object
//! and the page it would have looked at next. A reclaim that always started at the first object's
//! first page took the oldest files' pages first -- the shell's, the loader's,
//! the libraries every program runs -- and at the machine's floor each fault
//! took back pages another processor was about to fault in again, and a
//! download's cold cache at the end of the list was never reached: in a 1 GiB
//! guest two compilers then built nothing, the likely cause of Endfield's
//! install8 hang (2026-10-15; no register dump of it). Going round from where
//! the last reclaim stopped takes each page once a lap, as a clock does; it
//! does not know which pages are in use, so a hot page is still taken once
//! a lap and faulted in again.
//!
//! # The dentry and inode caches
//!
//! A reclaim that gave back fewer pages than it was asked for asks the
//! filesystem for the rest in kernel heap: the dentries the cache keeps that
//! nothing else holds, charged to a job the reclaim may take from, and the
//! inodes only they held ([`install_shrinker`], `docs/CGROUPS.md` §10). That
//! heap is charged to `memory.current` as the pages are (F-37), so it is what
//! the limit needs back as much as a page is; Linux's `shrink_slab` runs
//! beside its page reclaim the same way. The filesystem is above the item,
//! and registers its shrinker at boot; before it has, there is none.
//!
//! # What `/proc/meminfo` reads
//!
//! [`totals`] counts every cached file's pages, whoever they are charged to:
//! a disk file's, a memory filesystem's, and of the first those reclaim could
//! give back now, for `Cached`, `Shmem` and `MemAvailable`. Each object keeps
//! what it gives in its [`Share`], settled as its pages lock is let go, and
//! the share moves counts kept per processor, so a read sums those and looks
//! at no object (`docs/RSS-COUNTS.md` §9).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::vmo::{PoisonTake, Vmo};
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_frame::Frame;

use crate::fallible;
use crate::mm;
use crate::object::quota::{self, Counter};
use crate::sync::SpinLock;

/// Pages chosen from one object before they are taken, bounding the list
/// reclaim builds.
const BATCH: usize = 32;

/// Every object made for a file, weakly, and the walks of it under way.
static CACHE: SpinLock<List> = SpinLock::new(List {
    entries: Vec::new(),
    walkers: 0,
    close_owed: false,
});

/// [`CACHE`]'s value.
struct List {
    /// The objects, in the order they were made.
    entries: Vec<Weak<Vmo>>,
    /// The [`reclaim`]s walking `entries` by place now ([`Walk`]). While one
    /// is, the list is never closed up, so no entry moves under it and its
    /// pass goes once round the objects the list held when it began.
    walkers: usize,
    /// A close-up [`register`] left for the last walker to make.
    close_owed: bool,
}

impl List {
    /// Drop the entries whose objects are gone: the list closed up.
    fn close_up(&mut self) {
        self.entries.retain(|entry| entry.strong_count() > 0);
        self.close_owed = false;
    }
}

/// The heap one registration holds at most: its entry, with the room a
/// growing list keeps beside it. Counted in a file's charge (F-37).
pub const ENTRY_HEAP: usize = 2 * size_of::<Weak<Vmo>>();

/// Note `vmo` as a file's object.
///
/// # Errors
///
/// [`fallible::AllocError`] when the list cannot grow.
pub(crate) fn register(vmo: &Arc<Vmo>) -> Result<(), fallible::AllocError> {
    let mut list = CACHE.lock();
    if list.entries.len() == list.entries.capacity() {
        // About to grow: drop the ones that are gone first, so that files
        // made and closed in a loop do not grow the list without end. Not
        // while a reclaim walks it by place: then the last walker closes it
        // up as it leaves ([`Walk`]'s drop), and this one grows it.
        if list.walkers == 0 {
            list.close_up();
        } else {
            list.close_owed = true;
        }
    }
    fallible::try_push(&mut list.entries, Arc::downgrade(vmo))
}

/// A [`reclaim`]'s walk of [`CACHE`] by place: while one is held the list
/// is never closed up, only pushed to at its end, so a place names the same
/// entry for the whole walk. Made under the list's lock with the count of
/// entries the walk goes round.
struct Walk {
    /// The entries when the walk began; it looks at no others.
    count: usize,
}

impl Walk {
    /// Begin a walk.
    fn begin() -> Walk {
        let mut list = CACHE.lock();
        list.walkers += 1;
        Walk {
            count: list.entries.len(),
        }
    }
}

impl Drop for Walk {
    /// End the walk; the last one makes the close-up a registration left.
    fn drop(&mut self) {
        let mut list = CACHE.lock();
        list.walkers -= 1;
        if list.walkers == 0 && list.close_owed {
            list.close_up();
        }
    }
}

/// The lists of the objects alive [`live`] has made, check builds only:
/// `L.user.240`'s check, that a read of the counts makes none.
#[cfg(ferrix_checks = "on")]
static LISTED: AtomicU64 = AtomicU64::new(0);

// CHECKS-ONLY: user/cache_kept_check.rs; home: user::cache
/// The lists of the objects alive made so far, check builds only.
#[cfg(ferrix_checks = "on")]
pub(crate) fn listed_for_check() -> u64 {
    LISTED.load(Ordering::Relaxed)
}

/// The objects alive now, or none with no memory to list them.
pub(crate) fn live() -> Vec<Arc<Vmo>> {
    #[cfg(ferrix_checks = "on")]
    let _ = LISTED.fetch_add(1, Ordering::Relaxed);
    let list = CACHE.lock();
    let Ok(mut out) = fallible::try_with_capacity(list.entries.len()) else {
        return Vec::new();
    };
    for entry in list.entries.iter() {
        if let Some(vmo) = entry.upgrade() {
            // The room was had above.
            let _ = fallible::push_within(&mut out, vmo);
        }
    }
    out
}

/// What [`drop_poisoned`] did with a frame that holds poison
/// (`docs/MACHINE-CHECK-PAGECACHE.md` §3.1, `L.object.259`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dropped {
    /// A clean page of a file on a disk: out of the cache and of every
    /// space that mapped it; the next touch reads it from the disk again.
    Clean,
    /// A cached page that has no other copy -- written and not written
    /// back, a memory filesystem's, written through a shared mapping, or
    /// held -- kept where it is.
    NotClean,
    /// The frame could not be retired (the set full, or not the
    /// allocator's): nothing taken, since a taken page's frame would go back
    /// to the allocator.
    NotRetired,
    /// No cached page holds the frame.
    NotCache,
}

/// Retire `frame`, then drop the cached page it holds if that page is
/// clean, finding it by a walk of the list as reclaim walks it: the walker
/// count held, no spin lock across an object, each object's map scanned a
/// chunk at a time ([`Vmo::index_of`]). Says what came of it in the kernel
/// log. Run only after a machine check, with no spin lock held: it may wait
/// for a shootdown.
pub(crate) fn drop_poisoned(frame: Frame) -> Dropped {
    if !retire(frame) {
        return Dropped::NotRetired;
    }
    let walk = Walk::begin();
    let mut found = None;
    for at in 0..walk.count {
        let Some(vmo) = object_at(at) else {
            continue;
        };
        if let Some(index) = vmo.index_of(frame) {
            found = Some((vmo, index));
            break;
        }
        // An object that went meanwhile is let go of here, with no lock held.
        drop(vmo);
    }
    drop(walk);
    let dropped = match found {
        Some((vmo, index)) => take(&vmo, index, frame),
        None => Dropped::NotCache,
    };
    say(frame, dropped);
    dropped
}

/// [`drop_poisoned`] for a frame whose object and page the caller knows: a
/// read that consumed it (`L.object.261`).
pub(crate) fn drop_poisoned_at(vmo: &Vmo, index: u64, frame: Frame) -> Dropped {
    if !retire(frame) {
        return Dropped::NotRetired;
    }
    let dropped = take(vmo, index, frame);
    say(frame, dropped);
    dropped
}

impl Dropped {
    /// The outcome's name, for the poison check raised above the core.
    const fn name(self) -> &'static str {
        match self {
            Dropped::Clean => "Clean",
            Dropped::NotClean => "NotClean",
            Dropped::NotRetired => "NotRetired",
            Dropped::NotCache => "NotCache",
        }
    }
}

// CHECKS-ONLY: fs/btrfs_write_check/poison_check.rs; home: user::cache
/// [`drop_poisoned`] for stage 12's poison check, its outcome by name.
pub fn drop_poisoned_for_check(frame: Frame) -> &'static str {
    drop_poisoned(frame).name()
}

// CHECKS-ONLY: fs/btrfs_write_check/poison_check.rs; home: user::cache
/// [`drop_poisoned_at`] for stage 12's poison check, its outcome by name.
pub fn drop_poisoned_at_for_check(vmo: &Vmo, index: u64, frame: Frame) -> &'static str {
    drop_poisoned_at(vmo, index, frame).name()
}

/// What a read of page `index` of `vmo` does when its copy consumed poison
/// at `frame` (`L.object.261`, `docs/MACHINE-CHECK-PAGECACHE.md` §3.2):
/// [`drop_poisoned_at`], then `true` -- go round the fill loop, filling the
/// page again from the source -- for a clean page dropped or a page that no
/// longer holds the frame, and `false` -- fail `EIO` -- for one that is not
/// clean or a frame that could not be retired. With no lock held.
pub fn poisoned_read(vmo: &Vmo, index: u64, frame: Frame) -> bool {
    match drop_poisoned_at(vmo, index, frame) {
        Dropped::Clean | Dropped::NotCache => true,
        Dropped::NotClean | Dropped::NotRetired => false,
    }
}

/// Whether the poison lines are held back: while stage 12's poison check
/// names frames poisoned itself, so that a boot with the checks does not
/// announce retirements of frames nothing found bad.
static QUIET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// CHECKS-ONLY: fs/btrfs_write_check/poison_check.rs; home: user::cache
/// Hold back, or let out again, the lines [`drop_poisoned`] says.
pub fn quiet_for_check(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

/// Retire `frame` and say so: whether it is retired now (`L.object.259`).
fn retire(frame: Frame) -> bool {
    let outcome = mm::retire_frame(frame);
    let (retired, said) = match outcome {
        mm::Retired::Claimed | mm::Retired::Marked => (true, "retired"),
        mm::Retired::Already => (true, "already retired"),
        mm::Retired::Full | mm::Retired::NotCovered => (false, "not retired"),
    };
    if !QUIET.load(Ordering::Relaxed) {
        crate::println!("mce: frame {:#x} {said}", frame * PAGE_SIZE);
    }
    retired
}

/// Take page `index` of `vmo`, which holds the retired `frame`.
fn take(vmo: &Vmo, index: u64, frame: Frame) -> Dropped {
    match vmo.take_poisoned(index, frame) {
        PoisonTake::Taken => Dropped::Clean,
        PoisonTake::Kept => Dropped::NotClean,
        PoisonTake::Gone => Dropped::NotCache,
    }
}

/// The kernel log's line for a dropped or kept cached page.
fn say(frame: Frame, dropped: Dropped) {
    let said = match dropped {
        Dropped::Clean => "was a clean cached page, dropped",
        Dropped::NotClean => "was a cached page that is not clean, kept",
        Dropped::NotRetired | Dropped::NotCache => return,
    };
    if !QUIET.load(Ordering::Relaxed) {
        crate::println!("mce: frame {:#x} {said}", frame * PAGE_SIZE);
    }
}

/// What [`reclaim`] did.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Reclaimed {
    /// Pages looked at.
    pub(crate) scanned: u64,
    /// Pages given back.
    pub(crate) stolen: u64,
    /// Bytes of kernel heap given back by the shrinker ([`install_shrinker`]).
    pub heap: u64,
}

impl Reclaimed {
    /// Pages given back, for the reclaim checks.
    #[doc(hidden)]
    #[must_use]
    pub fn stolen(&self) -> u64 {
        self.stolen
    }

    /// Whether it gave anything back.
    #[must_use]
    pub fn gave(&self) -> bool {
        self.stolen > 0 || self.heap > 0
    }

    /// What it gave back, in bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.stolen
            .saturating_mul(PAGE_SIZE)
            .saturating_add(self.heap)
    }
}

/// What gives back the kernel heap of the dentry and inode caches: asked
/// which jobs' charges it may take (a test that takes no lock) and how many
/// bytes, it answers how many it gave back.
pub type Shrinker = fn(&dyn Fn(u32) -> bool, u64) -> u64;

/// The filesystem's [`Shrinker`], once it has registered one.
static SHRINKER: SpinLock<Option<Shrinker>> = SpinLock::new(None);

/// Register the filesystem's [`Shrinker`], which every [`reclaim`] that
/// falls short of its pages calls afterwards; a second replaces the first.
pub fn install_shrinker(shrinker: Shrinker) {
    *SHRINKER.lock() = Some(shrinker);
}

/// Ask the [`Shrinker`] for `want` bytes charged to `scope` or beneath it,
/// sparing `memory.min` as the pages' second pass does. Called with no lock
/// held: the shrinker drops what it gives back.
fn shrink(scope: u32, want: u64) -> u64 {
    let shrinker = *SHRINKER.lock();
    shrinker.map_or(0, |shrinker| {
        shrinker(&|owner| takes(owner, scope, false), want)
    })
}

/// Where the next [`reclaim`] starts looking: the place in [`CACHE`] the last
/// one stopped at, the object it found there, and the page it would have
/// looked at next. See "Where a reclaim starts".
static HAND: SpinLock<Hand> = SpinLock::new(Hand {
    at: 0,
    object: None,
    page: 0,
});

/// [`HAND`]'s value.
struct Hand {
    /// The place in [`CACHE`].
    at: usize,
    /// The object that was there, to tell whether it still is: the list is
    /// closed up between walks ([`register`]), and a place can then hold
    /// another object, whose pages are looked at from its first.
    object: Option<Weak<Vmo>>,
    /// The page to go on from in that object.
    page: u64,
}

/// The object at place `at` in [`CACHE`], if it is still alive: one entry
/// upgraded under the list's lock, never the whole list.
fn object_at(at: usize) -> Option<Arc<Vmo>> {
    CACHE.lock().entries.get(at).and_then(Weak::upgrade)
}

/// Give back up to `want` clean cache pages charged to slot `scope` or
/// beneath it, or, for [`quota::NONE`], to anyone (the machine out of
/// frames), and, if that is fewer than `want`, the rest in the heap of the
/// dentry and inode caches ([`install_shrinker`]). Counts `pgscan` and
/// `pgsteal` in `scope` and above.
///
/// It starts where the last reclaim stopped ([`HAND`]) and goes round the
/// list from there, so that a page is not taken again before the rest of
/// the cache has been looked at. It takes the list's lock for one entry at a
/// time, so that what one reclaim costs follows what it is asked for and
/// not how many files are cached: a copy of the whole list each time, the
/// way it was done before, cost a reclaim for every 32 pages an allocation
/// and an upgrade per cached file, under the list's lock. Each pass still
/// goes once round the objects the list held when the reclaim began, as the
/// copy did: the reclaim holds a [`Walk`], under which the list is never
/// closed up, only pushed to at its end, so no entry moves to another place
/// and none is skipped or looked at twice (the consultant, ledger line 2336
/// (a)). An object made meanwhile is past the walk's count, for the next.
///
/// Must be called with no spin lock held, as [`Vmo::decommit_range`] must.
///
/// The running task is marked inside a reclaim for its length, so that a
/// kernel-heap charge refused on the way is not answered by a reclaim of its
/// own (`quota::KernelHeap`, fl-096); one begun all the same stops the
/// machine (FX-0973). Not in interrupt context, where the task is not the
/// one reclaiming.
pub fn reclaim(scope: u32, want: u64) -> Reclaimed {
    let marked = !crate::irq::in_interrupt();
    if marked && crate::sched::with_current(|task| task.swap_reclaiming(true)) == Some(true) {
        crate::panic::fatal!(
            crate::panic::catalog::RECLAIM_IN_RECLAIM,
            "a reclaim began inside a reclaim of the same task"
        );
    }
    let mut done = reclaim_pages(scope, want);
    if done.stolen < want {
        done.heap = shrink(scope, (want - done.stolen).saturating_mul(PAGE_SIZE));
    }
    if marked {
        let _ = crate::sched::with_current(|task| task.swap_reclaiming(false));
    }
    done
}

/// [`reclaim`]'s pages: the walk of the list.
fn reclaim_pages(scope: u32, want: u64) -> Reclaimed {
    let mut done = Reclaimed::default();
    let walk = Walk::begin();
    let count = walk.count;
    if count == 0 {
        return done;
    }
    let (start, from) = hand_in(count);
    let mut stopped = None;
    // Whose reservation the reclaim may take drawn frames of: the running
    // task's own, never one in interrupt context (`L.user.145`).
    let requester = if crate::irq::in_interrupt() {
        mm::NO_RESERVATION
    } else {
        quota::reservation_of(crate::sched::running_group())
    };
    // Once sparing what `memory.low` protects, then without; each pass once
    // round the list from the hand, the hand's own object last again from its
    // first page.
    'passes: for low in [true, false] {
        for step in 0..=count {
            if done.stolen >= want {
                break 'passes;
            }
            let at = start.wrapping_add(step) % count;
            let Some(vmo) = object_at(at) else {
                continue;
            };
            let first = if step == 0 { from } else { 0 };
            let (more, next) = take_from(&vmo, scope, want - done.stolen, low, first, requester);
            done.scanned += more.scanned;
            done.stolen += more.stolen;
            stopped = Some(match next {
                Some(page) => (at, Some(Arc::downgrade(&vmo)), page),
                None => ((at + 1) % count, None, 0),
            });
            // The last reference to an object that went meanwhile is let go
            // of here, with no lock held, as its drop must be.
            drop(vmo);
        }
    }
    if let Some((at, object, page)) = stopped {
        set_hand(Hand { at, object, page });
    }
    drop(walk);
    if scope != quota::NONE {
        quota::count(scope, Counter::Scanned, done.scanned);
        quota::count(scope, Counter::Stolen, done.stolen);
    }
    done
}

/// Whether a page charged to `owner` is one a reclaim at `scope` may take.
fn takes(owner: u32, scope: u32, low: bool) -> bool {
    if scope == quota::NONE {
        return true;
    }
    owner != quota::NONE && quota::within(owner, scope) && !quota::protected(owner, scope, low)
}

/// Whether a reclaim for `requester` may take `frame` as far as the
/// reservations go (`L.user.145`, `docs/microkernel/K12.md` §2.7): a frame
/// drawn from a reservation only by a reclaim that draws from the same one.
/// Another's would give the requester nothing: the frame goes back under the
/// promise, where no program's floor reaches.
pub(crate) fn spares(frame: u64, requester: mm::Reservation) -> bool {
    let drawn = mm::drawn_from(frame);
    drawn == mm::NO_RESERVATION || drawn == requester
}

/// Where in a list of `count` the hand points, and the page to start at
/// there: the hand's object's next page while it is still in its place, the
/// first page of whatever is there otherwise.
fn hand_in(count: usize) -> (usize, u64) {
    let (at, object, page) = {
        let hand = HAND.lock();
        (hand.at, hand.object.as_ref().map(Weak::as_ptr), hand.page)
    };
    let at = at % count;
    let same = object.is_some_and(|object| {
        CACHE
            .lock()
            .entries
            .get(at)
            .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), object))
    });
    (at, if same { page } else { 0 })
}

/// Move the hand. The one it replaces is dropped after the lock: a `Weak`
/// that was the last reference to its allocation frees it.
fn set_hand(hand: Hand) {
    let old = core::mem::replace(&mut *HAND.lock(), hand);
    drop(old);
}

/// [`reclaim`] in one object, from page `from`: what it did, and the page
/// to go on from, or `None` once it looked past the object's last page.
fn take_from(
    vmo: &Arc<Vmo>,
    scope: u32,
    want: u64,
    low: bool,
    from: u64,
    requester: mm::Reservation,
) -> (Reclaimed, Option<u64>) {
    let mut done = Reclaimed::default();
    if !vmo.reclaimable() {
        return (done, None);
    }
    let mut from = from;
    while done.stolen < want {
        let Ok(mut picked) = fallible::try_with_capacity::<u64>(BATCH) else {
            break;
        };
        let room = usize::try_from(want - done.stolen)
            .unwrap_or(BATCH)
            .min(BATCH);
        let next = vmo.pick_pages(from, room, &mut picked, &mut |frame| {
            done.scanned += 1;
            takes(mm::frame_owner(frame), scope, low) && spares(frame, requester)
        });
        // Only the pages the source still holds clean, while it holds them
        // still: a writable file's written pages stay.
        done.stolen += vmo.reclaim_clean(&mut picked);
        match next {
            Some(index) => from = index,
            None => return (done, None),
        }
    }
    (done, Some(from))
}

/// The most pages one round of [`reclaim_asked`] asks for. Linux's 32,
/// `SWAP_CLUSTER_MAX`, is the least a round of its `memory_reclaim` asks;
/// here it is the most, so that each round is one bounded [`reclaim`]
/// (`docs/certification/MEMORY-AND-TIMING.md`).
const ASKED_BATCH: u64 = 32;
/// Rounds of [`reclaim_asked`] that may give back nothing, in all, before it
/// is [`Asked::Short`]: Linux's `MAX_RECLAIM_RETRIES`, which its
/// `memory_reclaim` counts down and never resets.
const ASKED_RETRIES: u32 = 16;

/// How a [`reclaim_asked`] ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked {
    /// It gave back at least what it was asked for.
    Given,
    /// More than [`ASKED_RETRIES`] rounds, in all, gave back nothing, short of
    /// what it was asked for: `EAGAIN` to a `memory.reclaim` write.
    Short,
    /// `interrupted` said so between rounds: `EINTR`.
    Interrupted,
}

/// Give back `want` pages charged to slot `scope` or beneath it, because
/// someone asked (`memory.reclaim`, Linux's `memory_reclaim`): rounds of
/// [`reclaim`] of at most [`ASKED_BATCH`] pages, until they have given
/// `want`, or more than [`ASKED_RETRIES`] rounds in all gave back
/// nothing, or `interrupted` answers yes before a round. What was given back
/// before it stopped stays given. A scope of [`quota::NONE`] holds nothing
/// that may be asked for: it would be the whole machine, sparing nobody's
/// `memory.min`.
///
/// Must be called with no spin lock held, as [`reclaim`] must.
pub fn reclaim_asked(scope: u32, want: u64, interrupted: &dyn Fn() -> bool) -> Asked {
    if scope == quota::NONE {
        return if want == 0 {
            Asked::Given
        } else {
            Asked::Short
        };
    }
    // In bytes, since the dentry and inode caches give back heap.
    let want = want.saturating_mul(PAGE_SIZE);
    let mut given = 0_u64;
    let mut retries = ASKED_RETRIES;
    while given < want {
        if interrupted() {
            return Asked::Interrupted;
        }
        let round = (want - given).div_ceil(PAGE_SIZE).min(ASKED_BATCH);
        let done = reclaim(scope, round);
        if !done.gave() {
            if retries == 0 {
                return Asked::Short;
            }
            retries -= 1;
        }
        given = given.saturating_add(done.bytes());
    }
    Asked::Given
}

/// What slot `scope` and the jobs beneath it hold in files' pages, in pages:
/// on a disk, and on a memory filesystem.
pub fn resident(scope: u32) -> (u64, u64) {
    let (mut file, mut memory) = (0, 0);
    for vmo in live() {
        let disk = vmo.is_disk_file();
        vmo.for_each_page(&mut |frame| {
            let owner = mm::frame_owner(frame);
            if owner != quota::NONE && quota::within(owner, scope) {
                if disk {
                    file += 1;
                } else {
                    memory += 1;
                }
            }
        });
    }
    (file, memory)
}

/// Every cached file's pages, whoever they are charged to ([`totals`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Totals {
    /// Pages of files on a disk.
    pub disk: u64,
    /// Pages of files on a memory filesystem: tmpfs, the initramfs, `memfd`.
    pub memory: u64,
    /// Of the disk pages, those written and not yet written back: a
    /// writable file's, which reclaim leaves.
    pub dirty: u64,
    /// Of the disk pages, those reclaim may take now: every page of an
    /// object [`Vmo::reclaimable`] says so of, less its dirty ones. A page a
    /// write or a writeback holds still for the moment is among them.
    pub reclaimable: u64,
}

impl Totals {
    /// `/proc/meminfo`'s `Cached`: every cached file's pages, a disk file's
    /// and a memory filesystem's, as Linux counts them.
    pub fn cached(&self) -> u64 {
        self.disk.saturating_add(self.memory)
    }

    /// `/proc/meminfo`'s `MemAvailable`, with `free` frames free: those and
    /// the pages reclaim may take now, never a dirty one or a memory
    /// filesystem's.
    pub fn available(&self, free: u64) -> u64 {
        free.saturating_add(self.reclaimable)
    }
}

/// Which of a set of counts ([`EARLY`], `PerCpu::cache_counts`) each
/// figure is.
const DISK: usize = 0;
/// See [`DISK`].
const MEMORY: usize = 1;
/// See [`DISK`].
const DIRTY: usize = 2;
/// See [`DISK`].
const RECLAIMABLE: usize = 3;

/// The counts settles moved before this processor's record could be found
/// (`L.user.241`).
static EARLY: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The counts a settle running here adds to: this processor's record, or
/// [`EARLY`] before there is one to find.
fn here() -> &'static [AtomicU64; 4] {
    crate::smp::this_cpu().map_or(&EARLY, |cpu| &cpu.cache_counts)
}

/// What one cache object gives the machine's counts now (`L.user.239`,
/// `docs/RSS-COUNTS.md` §9.4): every object [`Vmo::new_filled`] makes holds
/// one, in its pages, and no other object does. Settled at every release of
/// the object's pages lock, under it, and given back whole as the object
/// goes.
#[derive(Debug)]
pub(crate) struct Share {
    /// A disk file's object, rather than a memory filesystem's.
    disk: bool,
    /// Whether reclaim may take its clean pages: its source's answer when
    /// the object was made, which no source changes.
    reclaimable: bool,
    /// The source's count of pages written and not written back, as last
    /// read under the pages lock.
    fs: u64,
    /// The pages it gives now.
    pages: u64,
    /// The dirty pages it gives now: none for a memory filesystem's.
    dirty: u64,
}

impl Share {
    /// An object's share, giving nothing yet: `disk` for a disk file's,
    /// whether its source says it is `reclaimable`, and the source's dirty
    /// count now.
    pub(crate) const fn new(disk: bool, reclaimable: bool, fs: u64) -> Share {
        Share {
            disk,
            reclaimable,
            fs,
            pages: 0,
            dirty: 0,
        }
    }

    /// Whether it was made reclaimable: the drop's check, check builds only.
    #[cfg(ferrix_checks = "on")]
    pub(crate) const fn reclaimable(&self) -> bool {
        self.reclaimable
    }

    /// What it gives now, as (pages, dirty).
    pub(crate) const fn given(&self) -> (u64, u64) {
        (self.pages, self.dirty)
    }

    /// The source's dirty count is `fs` now; the next settle gives it.
    pub(crate) fn read_fs(&mut self, fs: u64) {
        self.fs = fs;
    }

    /// Give what an object of `pages` pages, `states` of them in a page
    /// state, gives: the walk's figures for it ([`Totals`]), the dirty ones
    /// at most its pages. Under the object's pages lock, so on the processor
    /// whose counts it moves.
    #[inline(always)]
    pub(crate) fn settle(&mut self, pages: u64, states: u64) {
        let dirty = if self.disk {
            self.fs.saturating_add(states).min(pages)
        } else {
            0
        };
        if pages != self.pages || dirty != self.dirty {
            self.give(pages, dirty);
        }
    }

    /// Give everything back: the object goes.
    pub(crate) fn take_back(&mut self) {
        self.give(0, 0);
    }

    /// Move the counts from what it gave to `pages` and `dirty`.
    #[inline(never)]
    fn give(&mut self, pages: u64, dirty: u64) {
        let counts = here();
        let more = pages.wrapping_sub(self.pages);
        let dirtier = dirty.wrapping_sub(self.dirty);
        let add = |figure: usize, by: u64| {
            if let Some(count) = counts.get(figure)
                && by != 0
            {
                let _ = count.fetch_add(by, Ordering::Relaxed);
            }
        };
        if self.disk {
            add(DISK, more);
            add(DIRTY, dirtier);
            if self.reclaimable {
                add(RECLAIMABLE, more.wrapping_sub(dirtier));
            }
        } else {
            add(MEMORY, more);
        }
        self.pages = pages;
        self.dirty = dirty;
    }
}

/// What the page cache holds now, for `/proc/meminfo` (`L.object.189`,
/// `L.user.239`, `L.user.240`): the counts every object's [`Share`] moved,
/// summed over the early counts and every processor's record. No cache
/// object, list or lock: O(processors). Exact whenever no change is under
/// way, as the walk it replaced was; a read racing changes on two
/// processors may see one and not the other, and a figure is never below
/// zero.
pub fn totals() -> Totals {
    totals_except(None)
}

/// [`totals`], leaving out processor `leave_out`'s record, for the negative
/// control of the check that requires the sum exact.
pub(crate) fn totals_except(leave_out: Option<usize>) -> Totals {
    let mut sum = [0_u64; 4];
    let add = |sum: &mut [u64; 4], counts: &[AtomicU64; 4]| {
        for (total, count) in sum.iter_mut().zip(counts) {
            *total = total.wrapping_add(count.load(Ordering::Relaxed));
        }
    };
    add(&mut sum, &EARLY);
    let cpus = crate::smp::topology().map_or(&[][..], crate::smp::Topology::cpus);
    for (cpu, record) in cpus.iter().enumerate() {
        if Some(cpu) != leave_out {
            add(&mut sum, &record.cache_counts);
        }
    }
    // Two's complement: a sum caught mid-change can be below zero.
    let figure = |at: usize| {
        sum.get(at)
            .map_or(0, |&total| u64::try_from(total as i64).unwrap_or(0))
    };
    Totals {
        disk: figure(DISK),
        memory: figure(MEMORY),
        dirty: figure(DIRTY),
        reclaimable: figure(RECLAIMABLE),
    }
}

// CHECKS-ONLY: user/cache_kept_check.rs; home: user::cache
/// This processor's own counts, for the check that a settle adds to the
/// record of the processor it runs on (`L.user.241`).
pub(crate) fn here_for_check() -> [u64; 4] {
    let counts = here();
    let mut out = [0; 4];
    for (value, count) in out.iter_mut().zip(counts) {
        *value = count.load(Ordering::Relaxed);
    }
    out
}
