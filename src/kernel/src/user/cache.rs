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
//! reclaimable`]): a read-only mount always, a writable one never yet, since
//! its dirty pages are in its inodes and not in the object, and a page taken
//! between a write's copy and its dirty mark would lose the write. A file on
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
//! little early, which costs a read and never data.

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

pub(crate) static LOST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
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
        let mut runs = picked.iter().copied().peekable();
        while let Some(first) = runs.next() {
            let mut length = 1;
            while runs.next_if_eq(&(first + length)).is_some() {
                length += 1;
            }
            done.stolen += vmo.decommit_range(first, length) as u64;
        }
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
