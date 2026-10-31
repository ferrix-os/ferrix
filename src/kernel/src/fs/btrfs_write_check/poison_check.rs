//! Stage 12's check of poison in the page cache
//! (`docs/MACHINE-CHECK-PAGECACHE.md` §6), on the writable volume the reclaim
//! checks have just used. QEMU's memory never fails a load, so each case names
//! a frame poisoned itself, through `cache::drop_poisoned` or the read path's
//! check-only injection:
//!
//! * a clean page of a synced file: retired, gone from the object and from a
//!   mapping of it, and read again with the disk's bytes;
//! * a page written and not written back: kept, `NotClean`;
//! * a held page: kept, `NotClean`;
//! * a frame no cached page holds: `NotCache`; one the allocator does not
//!   cover: `NotRetired`, nothing taken;
//! * a page asked for with a frame it no longer holds: `NotCache`, the page
//!   kept;
//! * a read whose copy is answered as poisoned: a clean page refilled and the
//!   read answered with the disk's bytes; a dirty page's read fails.
//!
//! Every frame the check retires is forgotten after.

use alloc::sync::Arc;
use core::any::Any;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vma::VmaFlags;

use super::{crc32c, pattern, read_pattern, write_at, write_file};
use crate::fs;
use crate::mm;
use crate::user::cache;
use crate::user::space::{Access, AddressSpace, FileMapping, FilePlace};
use crate::user::vmo::{self, Vmo};

/// [`PAGE_SIZE`] as a length.
const PAGE: usize = PAGE_SIZE as usize;
/// The file the clean cases use.
const CLEAN_FILE: &[u8] = b"/mnt-rw/poison-clean";
/// The file the dirty cases use.
const DIRTY_FILE: &[u8] = b"/mnt-rw/poison-dirty";
/// Their pages.
const PAGES: usize = 8;

/// What the check did, for the boot line.
#[derive(Debug)]
pub(super) struct Found {
    /// Clean pages dropped and read again.
    pub(super) dropped: u64,
    /// Pages kept because they were not clean.
    pub(super) kept: u64,
}

/// Run every case; forget the retirements after, whatever came of them.
///
/// # Errors
///
/// The first case that was not as the design has it.
///
/// Verifies: `H.FAIL.11`
/// Verifies: `L.object.259`
/// Verifies: `L.object.260`
/// Verifies: `L.object.261`
pub(super) fn run() -> Result<Found, &'static str> {
    let before = mm::retired_for_check();
    mm::hold_table_for_check();
    cache::quiet_for_check(true);
    let outcome = cases();
    cache::quiet_for_check(false);
    let retired = mm::retired_for_check().saturating_sub(before);
    mm::forget_retired_for_check(retired);
    outcome
}

/// The cases, in order.
fn cases() -> Result<Found, &'static str> {
    let mut found = Found {
        dropped: 0,
        kept: 0,
    };
    write_file(CLEAN_FILE, &pattern(41, 0, PAGES * PAGE))?;
    write_file(DIRTY_FILE, &pattern(43, 0, PAGES * PAGE))?;
    sync()?;
    // Both read whole, so every page is in the cache.
    let clean_want = (PAGES * PAGE, crc32c(&pattern(41, 0, PAGES * PAGE)));
    if read_pattern(CLEAN_FILE)? != clean_want {
        return Err("a synced file did not read back before the poison check");
    }
    let clean = object_of(CLEAN_FILE)?;

    // A clean page, mapped by a space of the check's own: dropped from the
    // object and the mapping, the frame retired, read again from the disk.
    let space = AddressSpace::new().map_err(|_| "no space for the poison check")?;
    let at = space
        .map_file(
            FilePlace::Anywhere(None),
            (PAGES * PAGE) as u64,
            VmaFlags::READ,
            clean.clone(),
            0,
            FileMapping {
                file: Arc::new(()) as Arc<dyn Any + Send + Sync>,
                may_write: false,
            },
        )
        .map_err(|_| "the poison check could not map its file")?;
    space
        .with_page(at + 2 * PAGE_SIZE, Access::READ, |_| ())
        .map_err(|_| "the poison check's mapping would not fault its page in")?;
    let frame =
        vmo::frame_for_check(&clean, 2).ok_or("a page read and mapped is not in the cache")?;
    let mapped = |space: &AddressSpace| {
        space
            .with_present_page(at + 2 * PAGE_SIZE, Access::READ, |_| ())
            .ok()
            .flatten()
            .is_some()
    };
    if !mapped(&space) {
        return Err("the poison check's mapping does not map the page it faulted in");
    }
    if cache::drop_poisoned_for_check(frame) != "Clean" {
        return Err("a clean cached page was not dropped");
    }
    if !mm::is_retired(frame) || vmo::frame_for_check(&clean, 2) == Some(frame) {
        return Err("a dropped page's frame was not retired, or is still in the object");
    }
    if mapped(&space) {
        return Err("a dropped page is still mapped");
    }
    if read_pattern(CLEAN_FILE)? != clean_want {
        return Err("a dropped page did not read again with the disk's bytes");
    }
    found.dropped += 1;
    drop(space);

    // A frame the cache does not hold, and one the allocator does not cover.
    let loose = mm::allocate_frames(0).ok_or("no frame for the poison check")?;
    let not_cache = cache::drop_poisoned_for_check(loose);
    mm::deallocate_frames(loose, 0);
    if not_cache != "NotCache" {
        return Err("a frame no cached page holds was not answered NotCache");
    }
    if cache::drop_poisoned_for_check(u64::MAX / PAGE_SIZE) != "NotRetired" {
        return Err("a frame that could not be retired was not answered NotRetired");
    }

    // A page asked for with a frame it does not hold: kept.
    let held = vmo::frame_for_check(&clean, 3).ok_or("page 3 is not in the cache")?;
    let other = mm::allocate_frames(0).ok_or("no frame for the poison check")?;
    let gone = cache::drop_poisoned_at_for_check(&clean, 3, other);
    mm::deallocate_frames(other, 0);
    if gone != "NotCache" || vmo::frame_for_check(&clean, 3) != Some(held) {
        return Err("a page asked for with a frame it does not hold was taken");
    }

    // A held page: kept.
    let hold = clean
        .hold(4, 1)
        .map_err(|_| "the poison check could not hold a page")?;
    let frame = vmo::frame_for_check(&clean, 4).ok_or("page 4 is not in the cache")?;
    let kept = cache::drop_poisoned_for_check(frame);
    drop(hold);
    if kept != "NotClean" || vmo::frame_for_check(&clean, 4) != Some(frame) {
        return Err("a held page was dropped");
    }
    found.kept += 1;

    read_cases(&clean, clean_want, &mut found)?;
    // Written back now, so the volume is as the next check expects.
    sync()?;
    Ok(found)
}

/// The read path's cases: a clean page whose read is answered as poisoned is
/// refilled with the disk's bytes, its frame retired and the page dropped; a
/// page written and not written back is kept, and its read fails.
fn read_cases(
    clean: &Arc<Vmo>,
    clean_want: (usize, u32),
    found: &mut Found,
) -> Result<(), &'static str> {
    // A read whose copy is answered as poisoned: refilled, the disk's bytes.
    let frame = vmo::frame_for_check(clean, 5).ok_or("page 5 is not in the cache")?;
    vmo::inject_read_poison(frame);
    if read_pattern(CLEAN_FILE).ok() != Some(clean_want) {
        return Err("a read that consumed poison in a clean page did not refill it");
    }
    if !mm::is_retired(frame) || vmo::frame_for_check(clean, 5) == Some(frame) {
        return Err("a read that consumed poison did not retire and drop the page");
    }
    found.dropped += 1;

    // A page written and not written back: kept, and its read fails.
    write_at(DIRTY_FILE, PAGE as u64, &pattern(47, 0, PAGE))?;
    let dirty = object_of(DIRTY_FILE)?;
    let frame = vmo::frame_for_check(&dirty, 1).ok_or("a written page is not in the cache")?;
    if cache::drop_poisoned_for_check(frame) != "NotClean"
        || vmo::frame_for_check(&dirty, 1) != Some(frame)
    {
        return Err("a page written and not written back was dropped");
    }
    found.kept += 1;
    vmo::inject_read_poison(frame);
    if read_pattern(DIRTY_FILE).is_ok() {
        return Err("a read that consumed poison in a dirty page did not fail");
    }
    Ok(())
}

/// Sync the writable volume.
fn sync() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.resolve(&ctx, None, super::MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved")?
        .mount
        .filesystem()
        .sync()
        .map_err(|_| "the volume could not be synced for the poison check")
}

/// The page cache's object of the file at `path`.
fn object_of(path: &[u8]) -> Result<Arc<Vmo>, &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.resolve(&ctx, None, path, true)
        .map_err(|_| "a file the poison check uses is gone")?
        .inode()
        .map_err(|_| "a file the poison check uses has no inode")?
        .mapping()
        .and_then(|object| object.downcast::<Vmo>().ok())
        .ok_or("a file of a writable mount has no object")
}
