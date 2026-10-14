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
//!   ([`write_reclaims_at_max`]).

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vfs::tmpfs::Storage;
use ferrix_vma::VmaFlags;

use super::{MOUNT_POINT, crc32c, make, pattern, read_pattern, write_at, write_file};
use crate::fs;
use crate::fs::pages::VmoStorage;
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
}

/// Run every reclaim check, in order.
///
/// # Errors
///
/// The first thing that was not as `docs/CGROUPS.md` §10 has it.
pub(super) fn run() -> Result<Found, &'static str> {
    let (given, dirty) = reclaim_written()?;
    reclaim_beside_a_mapping()?;
    let stolen_at_max = write_reclaims_at_max()?;
    Ok(Found {
        given,
        dirty,
        stolen_at_max,
    })
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

    let given = cache::reclaim(crate::object::quota::NONE, before.disk).stolen;
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
/// source's answer and before the take, keeps the take from taking: such a
/// write marks no page dirty, so the take looks again under the pages lock
/// (`Vmo::take_clean_range`). The interleaving is forced through
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
    job.set_limit(Resource::Memory, held.saturating_add(HEADROOM));
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
