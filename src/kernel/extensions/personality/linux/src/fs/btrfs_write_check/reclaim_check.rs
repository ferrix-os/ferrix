//! Stage 12's reclaim of a writable mount (`user::cache`, `docs/CGROUPS.md`
//! §10), on the volume the write check has just filled and emptied:
//!
//! * a file synced and then written again in one page loses its clean pages
//!   to a reclaim of the whole cache, keeps the written one, and reads back
//!   as it was last written; the count `/proc/meminfo` reads has the file in
//!   `Cached` and `MemAvailable`, the written page in `Dirty` and out of
//!   `MemAvailable`, and a tmpfs file in `Cached` and `Shmem`
//!   ([`reclaim_written`]);
//! * a shared mapping that may write, made and written between the source's
//!   answer and the take, keeps the take from taking anything of the file
//!   ([`reclaim_beside_a_mapping`], through `user::vmo`'s reclaim hook);
//! * a write in a job at its `memory.max`, which holds clean cache of the
//!   mount, takes that cache back and succeeds, where it answered `ENOSPC`
//!   ([`write_reclaims_at_max`]);
//! * a read in a job further past its `memory.max` than one reclaim gives
//!   back, which holds clean cache of the mount, reclaims until there is room
//!   and succeeds, where it answered `ENOMEM` ([`fill_reclaims_until_room`]);
//! * a file cut and written again gives its pages back to a reclaim once they
//!   are written back, and reads back as written, where it read zeros; and a
//!   write into part of such a page keeps the page's other bytes, where it
//!   wrote zeros over them on the disk ([`cut_then_reclaimed`]).

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vfs::tmpfs::Storage;
use ferrix_vma::VmaFlags;

use super::{MOUNT_POINT, crc32c, make, pattern, read_pattern, write_at, write_file};
use crate::fs;
use crate::fs::pages::{MAX_FILL_RUN, VmoStorage};
use crate::object::quota::{Counter, Quota, Resource};
use crate::sync::SpinLock;
use crate::user::cache::{self, Totals};
use crate::user::space::{Access, AddressSpace, FileMapping, FilePlace};
use crate::user::vmo::{self, ReclaimHook, Vmo};

/// [`PAGE_SIZE`] as a length.
const PAGE: usize = PAGE_SIZE as usize;

/// What the reclaim checks found, for the boot line.
#[derive(Debug)]
pub(super) struct Found {
    /// Clean pages of a writable mount reclaim gave back.
    pub(super) given: u64,
    /// Pages written and not written back it left.
    pub(super) dirty: u64,
    /// Pages a write at its job's `memory.max` took back.
    pub(super) stolen_at_max: u64,
    /// Pages a read past its job's `memory.max` took back.
    pub(super) stolen_by_fill: u64,
}

/// Run every reclaim check, in order.
///
/// # Errors
///
/// The first thing that was not as `docs/CGROUPS.md` §10 has it.
pub(super) fn run() -> Result<Found, &'static str> {
    let (given, dirty) = reclaim_written()?;
    reclaim_beside_a_mapping()?;
    resident_of_a_disk_file()?;
    kept_through_a_reclaim()?;
    kept_cache_counts()?;
    cut_then_reclaimed()?;
    let stolen_at_max = write_reclaims_at_max()?;
    let stolen_by_fill = fill_reclaims_until_room()?;
    Ok(Found {
        given,
        dirty,
        stolen_at_max,
        stolen_by_fill,
    })
}

/// The disk file [`resident_of_a_disk_file`] maps part of.
const RESIDENT_FILE: &[u8] = b"/mnt-rw/resident-check";

/// A disk file's pages in a space's resident set: the pages of the part a
/// region maps count as the file's, not the file's other cached pages; a
/// page a private mapping has written counts as anonymous instead, and the
/// file's original no longer does.
///
/// Verifies: L.user.172
fn resident_of_a_disk_file() -> Result<(), &'static str> {
    write_file(RESIDENT_FILE, &pattern(17, 0, 8 * PAGE))?;
    sync()?;
    let object = object_of(RESIDENT_FILE)?;
    if object.committed() != 8 {
        return Err("a disk file just written does not hold its eight pages");
    }
    let space = AddressSpace::new().map_err(|_| "no address space for the resident check")?;
    let map = |flags: VmaFlags, offset: u64, pages: u64| {
        space.map_file(
            FilePlace::Anywhere(None),
            pages * PAGE_SIZE,
            flags,
            Arc::clone(&object),
            offset * PAGE_SIZE,
            FileMapping {
                file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
                may_write: false,
            },
        )
    };
    let part = map(VmaFlags::READ, 2, 4)
        .map_err(|_| "the space refused a mapping of part of a disk file")?;
    let split = space.resident();
    if split.file != 4 || split.anon != 0 || split.shmem != 0 {
        return Err(
            "four pages of an eight-page disk file mapped did not count as four file pages",
        );
    }
    let _ = space.unmap(part, 4 * PAGE_SIZE);
    let private = map(VmaFlags::READ_WRITE, 0, 1)
        .map_err(|_| "the space refused a private mapping of a disk file")?;
    let wrote = space.with_page(private, Access::WRITE, |to| {
        // SAFETY: `with_page` passes the direct-map address of the byte,
        // translated under the space's lock, which it holds while this runs;
        // the write fault gave the private mapping a copy of its own.
        unsafe { core::ptr::write(to as *mut u8, 0xa5) }
    });
    let split = space.resident();
    let _ = space.unmap(private, PAGE_SIZE);
    drop(space);
    wrote.map_err(|_| "could not write the private mapping of a disk file")?;
    if split.anon != 1 || split.file != 0 || split.shmem != 0 {
        return Err(
            "a disk page a private mapping wrote did not move from the file's count to anon",
        );
    }
    Ok(())
}

/// The disk file [`kept_through_a_reclaim`] maps.
const KEPT_FILE: &[u8] = b"/mnt-rw/kept-check";

/// A disk file's mapped pages taken by a reclaim of the whole cache, and
/// filled again by faults: a space mapping it shared and private, with a copy
/// the private mapping wrote, keeps counts equal to a recount after each
/// (`docs/RSS-COUNTS.md` §7).
///
/// Verifies: L.user.236
fn kept_through_a_reclaim() -> Result<(), &'static str> {
    use crate::user::rss_check;
    write_file(KEPT_FILE, &pattern(23, 0, 8 * PAGE))?;
    sync()?;
    let object = object_of(KEPT_FILE)?;
    let space = AddressSpace::new().map_err(|_| "no address space for the kept check")?;
    let map = |flags: VmaFlags, pages: u64| {
        space.map_file(
            FilePlace::Anywhere(None),
            pages * PAGE_SIZE,
            flags,
            Arc::clone(&object),
            0,
            FileMapping {
                file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
                may_write: false,
            },
        )
    };
    let same = |after: &'static str| {
        rss_check::agrees(&space).map_err(|(kept, counted)| {
            crate::console::println!(
                "  rss      {after}: kept file {} anon {}, recounted file {} anon {}",
                kept.file,
                kept.anon,
                counted.file,
                counted.anon
            );
            "a space mapping a disk file kept counts that differ from a recount"
        })
    };
    let shared = map(
        VmaFlags {
            shared: true,
            ..VmaFlags::READ
        },
        8,
    )
    .map_err(|_| "the space refused a shared mapping of a disk file")?;
    let private = map(VmaFlags::READ_WRITE, 2)
        .map_err(|_| "the space refused a private mapping of a disk file")?;
    let outcome = space
        .with_page(private + PAGE_SIZE, Access::WRITE, |to| {
            // SAFETY: `with_page` passes the direct-map address of the byte,
            // translated under the space's lock, which it holds while this
            // runs; the write fault gave the private mapping a copy of its own.
            unsafe { core::ptr::write(to as *mut u8, 0x5a) }
        })
        .map_err(|_| "could not write the private mapping of a disk file")
        .and_then(|()| same("a disk file mapped shared and private, one page copied"))
        .and_then(|()| {
            let held = object.committed();
            // All of the cache, as `reclaim_written` asks, so that this
            // file's turn comes.
            let _ = cache::reclaim(crate::object::quota::NONE, cache::totals().disk);
            let left = object.committed();
            if left >= held {
                return Err("a reclaim took none of a mapped disk file's clean pages");
            }
            crate::console::println!(
                "  rss      a disk file mapped shared and private: the kept counts equal a \
                 recount after a reclaim took {} of its {held} pages, and after their faults",
                held - left
            );
            same("a reclaim of the mapped file's clean pages")
        })
        .and_then(|()| {
            (0..8).try_for_each(|page| {
                space
                    .with_page(shared + page * PAGE_SIZE, Access::READ, |_| ())
                    .map_err(|_| "a reclaimed page of a disk file would not fault back in")
            })
        })
        .and_then(|()| same("the reclaimed pages faulted back in"));
    let _ = space.unmap(shared, 8 * PAGE_SIZE);
    let _ = space.unmap(private, 2 * PAGE_SIZE);
    outcome
}

/// The file [`kept_cache_counts`] writes.
const COUNTED_FILE: &[u8] = b"/mnt-rw/cache-counted";

/// Pages it is written with: more than a writeback slice's 256, so that a
/// sync takes a slice before the rest.
const COUNTED_PAGES: usize = 260;

/// The page cache's kept counts on a writable mount, after each change its
/// dirty set and page states see: a write's marks, a sync's slice and full
/// writeback, a page written again, a shared mapping's write, a truncation's
/// cut and the sync after it -- each followed by the file's share against
/// the walk's figures for it and the machine's sum against the walk
/// (`docs/RSS-COUNTS.md` §9.6; the failed returns of a writeback are the
/// host test `every_change_of_a_dirty_set_tells_its_store_the_count_after_it`'s).
///
/// Verifies: L.user.239
fn kept_cache_counts() -> Result<(), &'static str> {
    use crate::user::cache_kept_check::agrees;
    let mut compared = 0;
    let mut check = |object: &Arc<Vmo>, after: &'static str| {
        agrees(&[object])
            .map(|(made, _)| compared += made)
            .inspect_err(|problem| {
                crate::console::println!("  pc-kept  after {after}: {problem}");
            })
    };
    write_file(COUNTED_FILE, &pattern(31, 0, COUNTED_PAGES * PAGE))?;
    let object = object_of(COUNTED_FILE)?;
    check(&object, "a write")?;
    sync()?;
    check(&object, "a sync")?;
    write_at(COUNTED_FILE, 3 * PAGE_SIZE, &pattern(32, 0, PAGE))?;
    check(&object, "a page written again")?;
    let space = AddressSpace::new().map_err(|_| "no address space for the counts check")?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            4 * PAGE_SIZE,
            VmaFlags {
                shared: true,
                ..VmaFlags::READ_WRITE
            },
            Arc::clone(&object),
            0,
            FileMapping {
                file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
                may_write: true,
            },
        )
        .map_err(|_| "the space refused a shared mapping for the counts check")?;
    space
        .with_page(at + PAGE_SIZE, Access::WRITE, |to| {
            // SAFETY: `with_page` passes the direct-map address of the byte,
            // translated under the space's lock, which it holds while this
            // runs; the write fault made the page writable through the mapping.
            unsafe { core::ptr::write(to as *mut u8, 0x3c) }
        })
        .map_err(|_| "could not write the shared mapping for the counts check")?;
    check(&object, "a shared mapping's write")?;
    let _ = space.unmap(at, 4 * PAGE_SIZE);
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.resolve(&ctx, None, COUNTED_FILE, true)
        .map_err(|_| "the counted file is gone")?
        .inode()
        .map_err(|_| "the counted file has no inode")?
        .set_len(2 * PAGE_SIZE)
        .map_err(|_| "the counted file would not truncate")?;
    check(&object, "a truncation")?;
    sync()?;
    check(&object, "the sync after it")?;
    crate::console::println!(
        "  pc-kept  a writable mount's file written, synced in slices, written again, written \
         through a shared mapping and truncated: its share and the machine's sum equal the walk \
         {compared} times over"
    );
    Ok(())
}

/// Cut the file at `path` to `len` bytes, as `ftruncate` and `O_TRUNC` do.
fn cut(path: &[u8], len: u64) -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let at = ns
        .resolve(&ctx, None, path, true)
        .map_err(|_| "a file to cut is gone")?;
    ns.truncate(&at, len).map_err(|_| "a file could not be cut")
}

/// Give back every clean page of the cache, and require that the file at
/// `path` then holds `left` pages.
fn reclaim_to(path: &[u8], left: usize, short: &'static str) -> Result<(), &'static str> {
    #[allow(
        clippy::disallowed_methods,
        reason = "a raised check: `Reclaimed::stolen` is made pub for the checks alone"
    )]
    let _ = cache::reclaim(crate::object::quota::NONE, cache::totals().disk).stolen();
    if object_of(path)?.committed() == left {
        Ok(())
    } else {
        Err(short)
    }
}

/// The file at `path` reads as `want`.
fn reads_as(path: &[u8], want: &[u8]) -> bool {
    read_pattern(path).is_ok_and(|read| read == (want.len(), crc32c(want)))
}

/// A file cut and written again is the volume's to fill once the volume
/// holds the cut (`Pages::source_cut`): its pages, written back, go to a
/// reclaim like any other clean page and read back as written.
///
/// Before, the store forgot the source from the cut on for as long as the
/// inode's object lived. A page written past the cut, written back and
/// reclaimed read as zeros until the object was made again, though the disk
/// held it: Steam's `steam.pid`, written by `O_TRUNC` and five bytes, read
/// five zero bytes once Endfield's load had reclaimed the cache, and a second
/// `steam` then found no running client to hand its address to (N150 box,
/// 2026-11-13). A write into part of such a page took a zeroed frame for it
/// and wrote the zeros back over the file's bytes.
///
/// Three files: one cut to nothing and written short, as a pid file is, then
/// appended to after the reclaim; one cut inside its second page and grown
/// past the cut, with a gap; and one cut to nothing whose first write leaves
/// a page between unwritten.
///
/// Verifies: H.QUOTA.12
fn cut_then_reclaimed() -> Result<(), &'static str> {
    const PID: &[u8] = b"/mnt-rw/reclaim-cut-pid";
    const GROWN: &[u8] = b"/mnt-rw/reclaim-cut-grown";
    const SPARSE: &[u8] = b"/mnt-rw/reclaim-cut-sparse";
    const CUT_AT: usize = 5000;
    const GROWN_FROM: usize = 6000;

    // An old file of three pages on the disk, cut to nothing and written
    // again, as `echo $$ > steam.pid` does.
    write_file(PID, &pattern(31, 0, 3 * PAGE))?;
    sync()?;
    cut(PID, 0)?;
    write_at(PID, 0, b"1840\n")?;
    sync()?;
    reclaim_to(
        PID,
        0,
        "a page written after a cut and written back stayed through a reclaim",
    )?;
    if !reads_as(PID, b"1840\n") {
        return Err("a file cut, written, written back and reclaimed read otherwise than written");
    }
    // The read filled the page again; without it, a write into part of the
    // page must take the page's other bytes from the volume.
    reclaim_to(
        PID,
        0,
        "a page filled again after a cut stayed through a reclaim",
    )?;
    write_at(PID, 5, b"abc")?;
    sync()?;
    reclaim_to(
        PID,
        0,
        "a page appended to after a cut stayed through a reclaim",
    )?;
    if !reads_as(PID, b"1840\nabc") {
        return Err("an append to a reclaimed page of a cut file lost the bytes before it");
    }

    // Cut inside the second page and grown past the cut, leaving a gap: the
    // gap reads zeros, never the old bytes, and what was written reads back.
    write_file(GROWN, &pattern(37, 0, 3 * PAGE))?;
    sync()?;
    cut(GROWN, CUT_AT as u64)?;
    write_at(GROWN, GROWN_FROM as u64, &pattern(41, GROWN_FROM, 3 * PAGE))?;
    sync()?;
    reclaim_to(
        GROWN,
        0,
        "pages written past a cut inside a page stayed through a reclaim",
    )?;
    let mut want = pattern(37, 0, CUT_AT);
    want.resize(GROWN_FROM, 0);
    want.extend_from_slice(&pattern(41, GROWN_FROM, 3 * PAGE));
    if !reads_as(GROWN, &want) {
        return Err(
            "a file cut inside a page and grown read otherwise than written after a reclaim",
        );
    }

    // Cut to nothing and written from the third page on: the two pages
    // before it are a hole, which reads zeros from the volume as it did from
    // the cut.
    write_file(SPARSE, &pattern(43, 0, 4 * PAGE))?;
    sync()?;
    cut(SPARSE, 0)?;
    write_at(SPARSE, 2 * PAGE_SIZE, &pattern(47, 0, PAGE))?;
    sync()?;
    reclaim_to(
        SPARSE,
        0,
        "a page written past a hole a cut left stayed through a reclaim",
    )?;
    let mut want = alloc::vec![0_u8; 2 * PAGE];
    want.extend_from_slice(&pattern(47, 0, PAGE));
    if !reads_as(SPARSE, &want) {
        return Err("a hole a cut left read otherwise than zeros after a reclaim");
    }
    Ok(())
}

/// Sync the writable mount.
fn sync() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved")?
        .mount
        .filesystem()
        .sync()
        .map_err(|_| "the volume could not be synced around a reclaim")
}

/// The page cache's object of the file at `path`: what a mapping maps.
fn object_of(path: &[u8]) -> Result<Arc<Vmo>, &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.resolve(&ctx, None, path, true)
        .map_err(|_| "a file reclaim is checked on is gone")?
        .inode()
        .map_err(|_| "a file reclaim is checked on has no inode")?
        .mapping()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a file of a writable mount has no object to map")
}

/// The count a reclaim may take, as `MemAvailable` reads it, must hold no
/// dirty page: at most the disk pages less the dirty ones.
fn available_holds_no_dirty_page(totals: &Totals) -> Result<(), &'static str> {
    if totals.reclaimable > totals.disk.saturating_sub(totals.dirty) {
        return Err("MemAvailable counts a page written and not written back");
    }
    Ok(())
}

/// A writable mount's clean pages are reclaim's to take and its dirty ones
/// are not: a file written and synced, one page of it written again and not
/// synced, the whole cache reclaimed, and then every page must read as it
/// was last written -- the one written again included, which reclaim would
/// have refilled from the disk's older copy. `/proc/meminfo`'s figures come
/// from the same count (`user::cache::totals`): the synced file's pages in
/// `Cached` and in `MemAvailable`, the page written again in `Dirty` and out
/// of `MemAvailable`, and a tmpfs file's pages in `Cached` and `Shmem`.
/// Answers the pages reclaim gave back and the dirty pages it left.
///
/// Verifies: L.object.188, L.object.189
fn reclaim_written() -> Result<(u64, u64), &'static str> {
    const FILE: &[u8] = b"/mnt-rw/reclaim";
    const PAGES: usize = 64;
    const SHMEM_PAGES: u64 = 4;
    write_file(FILE, &pattern(23, 0, PAGES * PAGE))?;
    sync()?;
    let clean = cache::totals();
    if clean.disk < PAGES as u64 {
        return Err("Cached does not count a written file's pages");
    }
    if clean.available(0) < PAGES as u64 {
        return Err("MemAvailable does not count a written-back file's pages");
    }
    available_holds_no_dirty_page(&clean)?;
    // Page 1 written again, and not written back.
    write_at(FILE, PAGE as u64, &pattern(29, 0, PAGE))?;
    let before = cache::totals();
    if before.dirty == 0 {
        return Err("Dirty does not count a page written and not written back");
    }
    available_holds_no_dirty_page(&before)?;

    // A memory filesystem's pages are in Cached and Shmem.
    let memory = VmoStorage
        .allocate()
        .map_err(|_| "no tmpfs store for the meminfo check")?;
    (0..SHMEM_PAGES)
        .try_for_each(|index| memory.write(index * PAGE_SIZE, &[7; PAGE]))
        .map_err(|_| "a tmpfs file would not take its pages")?;
    let with_memory = cache::totals();
    drop(memory);
    if with_memory.memory < before.memory.saturating_add(SHMEM_PAGES) {
        return Err("Shmem does not count a memory filesystem's pages");
    }
    if with_memory.cached() < before.cached().saturating_add(SHMEM_PAGES) {
        return Err("Cached does not count a memory filesystem's pages");
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "a raised check: `Reclaimed::stolen` is made pub for the checks alone"
    )]
    let given = cache::reclaim(crate::object::quota::NONE, before.disk).stolen();
    let after = cache::totals();
    if given < (PAGES - 1) as u64 || after.disk > before.disk.saturating_sub(PAGES as u64 - 1) {
        return Err("reclaim gave back no clean page of a writable mount");
    }
    if after.dirty < before.dirty {
        return Err("reclaim gave back a page written and not written back");
    }
    let mut want = pattern(23, 0, PAGES * PAGE);
    want.get_mut(PAGE..2 * PAGE)
        .ok_or("the page written again is past the file")?
        .copy_from_slice(&pattern(29, 0, PAGE));
    if read_pattern(FILE)? != (want.len(), crc32c(&want)) {
        return Err(
            "a page of a writable mount read back otherwise than last written after reclaim",
        );
    }
    sync()?;
    Ok((given, before.dirty))
}

/// The file [`reclaim_beside_a_mapping`] maps.
const MAPPED_FILE: &[u8] = b"/mnt-rw/reclaim-mapped";
/// Its pages.
const MAPPED_PAGES: usize = 16;
/// The page the mapping writes, and where in it.
const MAPPED_PAGE: u64 = 3;
const MAPPED_AT: u64 = 5;
/// What it writes there: no byte of the file's pattern needs to differ, as
/// the check reads the file's own byte first and writes its complement.
static MAPPED_BYTE: SpinLock<u8> = SpinLock::new(0);

/// The object the hook acts on, the space it made its mapping in and where,
/// and whether it ran.
static TARGET: SpinLock<Option<Arc<Vmo>>> = SpinLock::new(None);
static MAPPING: SpinLock<Option<(Arc<AddressSpace>, u64)>> = SpinLock::new(None);
static HOOK_RAN: AtomicBool = AtomicBool::new(false);

/// The hook: on the target object, once, map the file shared and writable
/// in a space of its own and write a byte through the mapping -- between the
/// source's answer and the take, with the source holding the pages still.
static HOOK: ReclaimHook = ReclaimHook {
    armed_by: "stage 12's reclaim beside a shared mapping",
    run: map_and_write,
};

/// [`HOOK`]'s body. Failures leave `HOOK_RAN` clear, which the check reports.
fn map_and_write(vmo: &Vmo) {
    let target = TARGET.lock().clone();
    let Some(target) = target else {
        return;
    };
    if !core::ptr::eq(Arc::as_ptr(&target), vmo) || HOOK_RAN.load(Ordering::SeqCst) {
        return;
    }
    let Ok(space) = AddressSpace::new() else {
        return;
    };
    let len = (MAPPED_PAGES * PAGE) as u64;
    let Ok(at) = space.map_file(
        FilePlace::Anywhere(None),
        len,
        VmaFlags {
            shared: true,
            ..VmaFlags::READ_WRITE
        },
        target,
        0,
        FileMapping {
            file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
            may_write: true,
        },
    ) else {
        return;
    };
    let byte = *MAPPED_BYTE.lock();
    let wrote = space.with_page(
        at + MAPPED_PAGE * PAGE_SIZE + MAPPED_AT,
        Access::WRITE,
        |to| {
            // SAFETY: `with_page` passes the direct-map address of the byte,
            // translated under the space's lock, which it holds while this
            // runs; the write fault made the page writable through the mapping.
            unsafe { core::ptr::write(to as *mut u8, byte) }
        },
    );
    *MAPPING.lock() = Some((space, at));
    if wrote.is_ok() {
        HOOK_RAN.store(true, Ordering::SeqCst);
    }
}

/// A shared mapping that may write the file, made and written after the
/// source's answer and before the take, keeps its page from being taken: the
/// write marks the page Dirty, and the take looks at each page under the pages
/// lock (`Vmo::take_clean_range`, K3b). The interleaving is forced through
/// `user::vmo`'s reclaim hook, as stage 9's domain check forces its cases 11
/// and 12; this shows the look holds there, not that the window opens
/// unforced.
///
/// Verifies: L.object.188
fn reclaim_beside_a_mapping() -> Result<(), &'static str> {
    write_file(MAPPED_FILE, &pattern(31, 0, MAPPED_PAGES * PAGE))?;
    sync()?;
    let at = (MAPPED_PAGE * PAGE_SIZE + MAPPED_AT) as usize;
    let original = pattern(31, at, 1).first().copied().unwrap_or(0);
    *MAPPED_BYTE.lock() = !original;
    *TARGET.lock() = Some(object_of(MAPPED_FILE)?);
    HOOK_RAN.store(false, Ordering::SeqCst);
    vmo::arm_reclaim_hook(&HOOK);
    let all = cache::totals().disk;
    let _ = cache::reclaim(crate::object::quota::NONE, all);
    vmo::disarm_reclaim_hook();
    let _ = TARGET.lock().take();
    let mapping = MAPPING.lock().take();
    let ran = HOOK_RAN.load(Ordering::SeqCst);
    let mut read = [0_u8; 1];
    let outcome = if !ran {
        Err("reclaim never reached the mapped file's take, or the hook could not write")
    } else {
        read_at(MAPPED_FILE, at as u64, &mut read)
    };
    if let Some((space, at)) = mapping {
        let _ = space.unmap(at, (MAPPED_PAGES * PAGE) as u64);
        drop(space);
    }
    outcome?;
    if read != [!original] {
        return Err(
            "reclaim gave back a page a shared mapping made during the reclaim had written",
        );
    }
    // What the mapping wrote goes to the disk, and the file reads it back.
    sync()?;
    let mut want = pattern(31, 0, MAPPED_PAGES * PAGE);
    if let Some(byte) = want.get_mut(at) {
        *byte = !original;
    }
    if read_pattern(MAPPED_FILE)? != (want.len(), crc32c(&want)) {
        return Err("a file written through a shared mapping read back otherwise");
    }
    Ok(())
}

/// Read `out.len()` bytes of the file at `path` from `offset`.
fn read_at(path: &[u8], offset: u64, out: &mut [u8]) -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let inode = ns
        .resolve(&ctx, None, path, true)
        .map_err(|_| "a file read back is gone")?
        .inode()
        .map_err(|_| "a file read back has no inode")?;
    match inode.read_at(offset, out) {
        Ok(read) if read == out.len() => Ok(()),
        _ => Err("a file could not be read back"),
    }
}

/// A write on the writable mount, in a job at its `memory.max` that holds
/// clean cache of that mount, takes that cache back and succeeds: its page
/// commit, refused for want of room in the job, reclaims inside the job, as
/// a fill's does, before it answers `ENOSPC` (`fs::pages`'s
/// `write_reclaiming`). Without that, the write answered `ENOSPC` with the
/// machine full of clean cache (`docs/WINE.md` §10). Answers the pages the
/// job's reclaim gave back.
///
/// Verifies: L.object.190
fn write_reclaims_at_max() -> Result<u64, &'static str> {
    const CLEAN: &[u8] = b"/mnt-rw/reclaim-clean";
    const WRITTEN: &[u8] = b"/mnt-rw/reclaim-written";
    const CLEAN_PAGES: usize = 32;
    const WRITTEN_PAGES: usize = 16;
    /// Room above what the job holds once it has read the clean file: for
    /// the kernel heap the write takes before its first page.
    const HEADROOM: u64 = 4 * PAGE_SIZE;
    let clean_pattern = pattern(41, 0, CLEAN_PAGES * PAGE);
    let written_pattern = pattern(43, 0, WRITTEN_PAGES * PAGE);
    write_file(CLEAN, &clean_pattern)?;
    make(WRITTEN)?;
    sync()?;
    // The clean file's pages out of the cache, so the job's read fills them
    // again, charged to the job.
    let _ = cache::reclaim(crate::object::quota::NONE, cache::totals().disk);
    let job = Quota::new(None).map_err(|_| "no quota slot for the write-at-max check")?;
    let own = crate::sched::running_group();
    crate::sched::set_current_group(job.index());
    let read = read_pattern(CLEAN);
    let held = job.usage(Resource::Memory).used;
    let _ = job.set_limit(Resource::Memory, held.saturating_add(HEADROOM));
    let refused = job.usage(Resource::Memory).refused;
    let stolen = job.counted(Counter::Stolen);
    let wrote = match read {
        Ok(_) => write_at(WRITTEN, 0, &written_pattern).map_err(|_| ()),
        Err(_) => Ok(()),
    };
    crate::sched::set_current_group(own);
    let refused = job.usage(Resource::Memory).refused.saturating_sub(refused);
    let stolen = job.counted(Counter::Stolen).saturating_sub(stolen);
    let outcome = read
        .map_err(|_| "a job could not read a file of the writable mount")
        .and_then(|_| {
            check_write_at_max(
                wrote,
                held,
                refused,
                stolen,
                (CLEAN, &clean_pattern),
                (WRITTEN, &written_pattern),
            )
        });
    // The job's pages go back before its slot: written back, then reclaimed.
    let synced = sync();
    let _ = cache::reclaim(job.index(), u64::MAX);
    drop(job);
    outcome?;
    synced?;
    Ok(stolen)
}

/// [`write_reclaims_at_max`]'s verdict on what it saw.
fn check_write_at_max(
    wrote: Result<(), ()>,
    held: u64,
    refused: u64,
    stolen: u64,
    clean: (&[u8], &[u8]),
    written: (&[u8], &[u8]),
) -> Result<(), &'static str> {
    if held < 16 * PAGE_SIZE {
        return Err("a job reading a file on the writable mount was charged none of its cache");
    }
    if wrote.is_err() {
        return Err(
            "a write at its job's memory.max answered ENOSPC with the job's clean cache of the \
             mount to give back",
        );
    }
    if refused == 0 {
        return Err("a write past its job's memory.max was never refused: the check missed it");
    }
    if stolen == 0 {
        return Err("a write refused at its job's memory.max took back no clean page");
    }
    for (path, bytes) in [clean, written] {
        if read_pattern(path)? != (bytes.len(), crc32c(bytes)) {
            return Err("a file read back otherwise after a write reclaimed at memory.max");
        }
    }
    Ok(())
}

/// A read in a job past its `memory.max` by more than one reclaim gives back,
/// which holds clean cache of the writable mount, reclaims inside the job
/// until its fill has room, and succeeds (`fs::pages`'s
/// `allocate_reclaiming`, at most eight reclaims of a fill's run). With one
/// reclaim and one retry the read answered `ENOMEM`: what one reclaim gives
/// back is not always there for the retry -- at the machine's floor another
/// processor took it, which stopped Endfield's launcher's 7-Zip half-way
/// through its 68 GB (`docs/WINE.md` §10), and here the job is still past its
/// limit after it. Answers the pages the job's reclaim gave back.
fn fill_reclaims_until_room() -> Result<u64, &'static str> {
    const CLEAN: &[u8] = b"/mnt-rw/reclaim-held";
    const READ: &[u8] = b"/mnt-rw/reclaim-read";
    const CLEAN_PAGES: usize = 128;
    const READ_PAGES: usize = 4;
    /// How far past `memory.max` the job is put: two fills' runs, so that
    /// a read finds room only after its third reclaim.
    const OVER: u64 = 2 * MAX_FILL_RUN as u64 * PAGE_SIZE;
    let clean_pattern = pattern(47, 0, CLEAN_PAGES * PAGE);
    let read_pattern_bytes = pattern(53, 0, READ_PAGES * PAGE);
    write_file(CLEAN, &clean_pattern)?;
    write_file(READ, &read_pattern_bytes)?;
    sync()?;
    // Every page of both out of the cache, so the job's reads fill them,
    // charged to the job.
    let _ = cache::reclaim(crate::object::quota::NONE, cache::totals().disk);
    // The file read past the limit is looked up first, outside the job: its
    // object and the heap behind it are not what is checked.
    let ns = fs::namespace();
    let ctx = ns.context();
    let target = ns
        .resolve(&ctx, None, READ, true)
        .map_err(|_| "the file read past memory.max is gone")?
        .inode()
        .map_err(|_| "the file read past memory.max has no inode")?;
    let job = Quota::new(None).map_err(|_| "no quota slot for the fill-past-max check")?;
    let own = crate::sched::running_group();
    crate::sched::set_current_group(job.index());
    let held = read_pattern(CLEAN);
    let used = job.usage(Resource::Memory).used;
    let _ = job.set_limit(Resource::Memory, used.saturating_sub(OVER));
    let stolen = job.counted(Counter::Stolen);
    let mut out = alloc::vec![0_u8; READ_PAGES * PAGE];
    let read = match held {
        Ok(_) => target.read_at(0, &mut out).map_err(|_| ()),
        Err(_) => Ok(0),
    };
    crate::sched::set_current_group(own);
    let stolen = job.counted(Counter::Stolen).saturating_sub(stolen);
    let outcome = held
        .map_err(|_| "a job could not read a file of the writable mount")
        .and_then(|_| check_fill_past_max(read, used, stolen, &out, &read_pattern_bytes));
    let _ = cache::reclaim(job.index(), u64::MAX);
    drop(job);
    outcome?;
    Ok(stolen)
}

/// [`fill_reclaims_until_room`]'s verdict on what it saw.
fn check_fill_past_max(
    read: Result<usize, ()>,
    used: u64,
    stolen: u64,
    out: &[u8],
    want: &[u8],
) -> Result<(), &'static str> {
    if used < 2 * MAX_FILL_RUN as u64 * PAGE_SIZE + 16 * PAGE_SIZE {
        return Err(
            "a job reading a file on the writable mount was charged too little of its cache",
        );
    }
    match read {
        Err(()) => {
            return Err(
                "a read past its job's memory.max answered ENOMEM with the job's clean cache of \
                 the mount to give back",
            );
        }
        Ok(len) if len != out.len() => {
            return Err("a read past its job's memory.max came up short");
        }
        Ok(_) => {}
    }
    if stolen <= 2 * MAX_FILL_RUN as u64 {
        return Err("a read past its job's memory.max found room without reclaiming past it");
    }
    if crc32c(out) != crc32c(want) {
        return Err("a file read past its job's memory.max read back otherwise");
    }
    Ok(())
}
