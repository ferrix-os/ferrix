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
//! # What `/proc/meminfo` reads
//!
//! [`totals`] counts every cached file's pages, whoever they are charged to:
//! a disk file's, a memory filesystem's, and of the first those reclaim could
//! give back now, for `Cached`, `Shmem` and `MemAvailable`. A count of each
//! object's pages, not a walk of them, so a program polling the file costs
//! the list and no more.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use super::vmo::Vmo;
use crate::fallible;
use crate::mm;
use crate::object::quota::{self, Counter};
use crate::sync::SpinLock;

/// Pages chosen from one object before they are taken, bounding the list
/// reclaim builds.
const BATCH: usize = 32;

/// Every object made for a file, weakly.
static CACHE: SpinLock<Vec<Weak<Vmo>>> = SpinLock::new(Vec::new());

/// The heap one registration holds at most: its entry, with the room a
/// growing list keeps beside it. Counted in a file's charge (F-37).
pub(crate) const ENTRY_HEAP: usize = 2 * size_of::<Weak<Vmo>>();

/// Note `vmo` as a file's object.
///
/// # Errors
///
/// [`fallible::AllocError`] when the list cannot grow.
pub(crate) fn register(vmo: &Arc<Vmo>) -> Result<(), fallible::AllocError> {
    let mut list = CACHE.lock();
    if list.len() == list.capacity() {
        // About to grow: drop the ones that are gone first, so that files
        // made and closed in a loop do not grow the list without end.
        list.retain(|entry| entry.strong_count() > 0);
    }
    fallible::try_push(&mut list, Arc::downgrade(vmo))
}

/// The objects alive now, or none with no memory to list them.
fn live() -> Vec<Arc<Vmo>> {
    let list = CACHE.lock();
    let Ok(mut out) = fallible::try_with_capacity(list.len()) else {
        return Vec::new();
    };
    for entry in list.iter() {
        if let Some(vmo) = entry.upgrade() {
            // The room was had above.
            let _ = fallible::push_within(&mut out, vmo);
        }
    }
    out
}

/// What [`reclaim`] did.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Reclaimed {
    /// Pages looked at.
    pub(crate) scanned: u64,
    /// Pages given back.
    pub(crate) stolen: u64,
}

/// Give back up to `want` clean cache pages charged to slot `scope` or
/// beneath it, or, for [`quota::NONE`], to anyone (the machine out of
/// frames). Counts `pgscan` and `pgsteal` in `scope` and above.
///
/// Must be called with no spin lock held, as [`Vmo::decommit_range`] must.
pub(crate) fn reclaim(scope: u32, want: u64) -> Reclaimed {
    let mut done = Reclaimed::default();
    let objects = live();
    // Once sparing what `memory.low` protects, then without.
    for low in [true, false] {
        for vmo in &objects {
            if done.stolen >= want {
                break;
            }
            let more = take_from(vmo, scope, want - done.stolen, low);
            done.scanned += more.scanned;
            done.stolen += more.stolen;
        }
        if done.stolen >= want {
            break;
        }
    }
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

/// [`reclaim`] in one object.
fn take_from(vmo: &Arc<Vmo>, scope: u32, want: u64, low: bool) -> Reclaimed {
    let mut done = Reclaimed::default();
    if !vmo.reclaimable() {
        return done;
    }
    let mut from = 0;
    while done.stolen < want {
        let Ok(mut picked) = fallible::try_with_capacity::<u64>(BATCH) else {
            break;
        };
        let room = usize::try_from(want - done.stolen)
            .unwrap_or(BATCH)
            .min(BATCH);
        let next = vmo.pick_pages(from, room, &mut picked, &mut |frame| {
            done.scanned += 1;
            takes(mm::frame_owner(frame), scope, low)
        });
        // Only the pages the source still holds clean, while it holds them
        // still: a writable file's written pages stay.
        done.stolen += vmo.reclaim_clean(&mut picked);
        match next {
            Some(index) => from = index,
            None => break,
        }
    }
    done
}

/// What slot `scope` and the jobs beneath it hold in files' pages, in pages:
/// on a disk, and on a memory filesystem.
pub(crate) fn resident(scope: u32) -> (u64, u64) {
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
pub(crate) struct Totals {
    /// Pages of files on a disk.
    pub(crate) disk: u64,
    /// Pages of files on a memory filesystem: tmpfs, the initramfs, `memfd`.
    pub(crate) memory: u64,
    /// Of the disk pages, those written and not yet written back: a
    /// writable file's, which reclaim leaves.
    pub(crate) dirty: u64,
    /// Of the disk pages, those reclaim may take now: every page of an
    /// object [`Vmo::reclaimable`] says so of, less its dirty ones. A page a
    /// write or a writeback holds still for the moment is among them.
    pub(crate) reclaimable: u64,
}

impl Totals {
    /// `/proc/meminfo`'s `Cached`: every cached file's pages, a disk file's
    /// and a memory filesystem's, as Linux counts them.
    pub(crate) fn cached(&self) -> u64 {
        self.disk.saturating_add(self.memory)
    }

    /// `/proc/meminfo`'s `MemAvailable`, with `free` frames free: those and
    /// the pages reclaim may take now, never a dirty one or a memory
    /// filesystem's.
    pub(crate) fn available(&self, free: u64) -> u64 {
        free.saturating_add(self.reclaimable)
    }
}

/// What the page cache holds now, for `/proc/meminfo`: each live object's
/// page count, which it keeps, rather than a walk of its pages.
pub(crate) fn totals() -> Totals {
    let mut out = Totals::default();
    for vmo in live() {
        let pages = vmo.committed() as u64;
        if vmo.is_disk_file() {
            let dirty = vmo.dirty_pages().min(pages);
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
