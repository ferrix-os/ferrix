//! A writable mount's clean tree nodes, given back to a reclaim of the whole
//! machine (`docs/RAM-RELEASE.md` §3), on the volume the write check left
//! mounted at `/mnt-rw`:
//!
//! * a wide directory made, synced, listed and every entry looked up on a
//!   fresh mount fills the volume's clean nodes; `/proc/meminfo` counts them in `SReclaimable` and
//!   `MemAvailable`;
//! * a job's `memory.max` reclaim takes none of them: they are charged to no
//!   job;
//! * a reclaim of the machine takes them, oldest first, a batch at a time;
//! * giving them all back raises the free frames, not only the heap's free
//!   bytes, and the listing read again comes off the disk the same.
//!
//! The heap's per-processor caches, where they exist, hold freed objects
//! back from their slab pages; this tree has none (`mm::drain_caches` is on
//! `feature/heap-cpu-caches`), so nothing is drained before the frames are
//! counted.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_btrfs::crc32c::crc32c_update;
use ferrix_btrfs_vfs::rw::RwBtrfs;
use ferrix_vfs::OpenFlags;

use super::{MOUNT_POINT, make};
use crate::fs;
use crate::fs::btrfs::Disk;
use crate::mm;
use crate::object::quota::{self, Quota};
use crate::user::cache;

/// The directory the check fills the cache from.
const WIDE: &[u8] = b"/mnt-rw/wide";
/// Its entries: enough leaves of index, name and inode items that giving
/// their nodes back frees whole pages.
const ENTRIES: usize = 1000;
/// The most rounds of a reclaim the check makes waiting for one to reach the
/// nodes: the dentry cache, asked first, drains within a few.
const ROUNDS: usize = 64;

/// What the check found, for the boot line.
#[derive(Debug)]
pub(super) struct Found {
    /// Bytes of clean nodes the listing left held.
    pub(super) held: u64,
    /// Of them, what a reclaim of the machine took.
    pub(super) reclaimed: u64,
    /// Bytes of clean nodes given back to count the frames.
    pub(super) given: u64,
    /// Frames free after that, less before.
    pub(super) frames: u64,
}

/// Run the check.
///
/// # Errors
///
/// The first thing that was not as `docs/RAM-RELEASE.md` §3 has it.
///
/// Verifies: L.btrfs.120, L.btrfs.121, L.user.175
pub(super) fn run(rdev: u64) -> Result<Found, &'static str> {
    fill_wide()?;
    let volume = remount(rdev)?;
    // The page cache emptied first, so what the reclaims below take of it
    // cannot stand for the nodes.
    let _ = cache::reclaim(quota::NONE, cache::totals().disk);
    let listed = list()?;
    if listed.0 != ENTRIES {
        return Err("the wide directory did not list every entry");
    }
    look_up_all()?;
    let held = volume.clean_bytes();
    if held < 64 * PAGE_SIZE {
        crate::console::println!("  reclaim  btrfs-rw: {held} bytes of clean nodes held");
        return Err("reading a wide directory left its volume few clean nodes");
    }
    counted_in_meminfo(held)?;
    let job = Quota::new(None).map_err(|_| "no quota slot for the clean-node check")?;
    let _ = cache::reclaim(job.index(), u64::MAX);
    drop(job);
    if volume.clean_bytes() != held {
        return Err("a job's reclaim took clean btrfs nodes, which no job is charged for");
    }
    let reclaimed = reclaim_reaches(&volume, held)?;
    let (given, frames) = give_all_back(&volume)?;
    if list()? != listed {
        return Err("the wide directory listed otherwise after its nodes were given back");
    }
    if volume.clean_bytes() == 0 {
        return Err("the listing after the nodes went did not read them from the disk");
    }
    Ok(Found {
        held,
        reclaimed,
        given,
        frames,
    })
}

/// Make the wide directory and sync it.
fn fill_wide() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.mkdir(&ctx, None, WIDE, 0o755)
        .map_err(|_| "the wide directory could not be made")?;
    for i in 0..ENTRIES {
        make(format!("/mnt-rw/wide/entry-{i:040}").as_bytes())?;
    }
    sync()
}

/// Sync the writable mount.
fn sync() -> Result<(), &'static str> {
    let ns = fs::namespace();
    ns.resolve(&ns.context(), None, MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved")?
        .mount
        .filesystem()
        .sync()
        .map_err(|_| "the volume could not be synced for the clean-node check")
}

/// Mount the volume on `rdev` again, so its clean nodes are only what the
/// check reads, and answer it.
fn remount(rdev: u64) -> Result<Arc<RwBtrfs<Disk>>, &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let place = ns
        .resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved")?;
    ns.unmount(&place)
        .map_err(|_| "the volume could not be unmounted for the clean-node check")?;
    let again = fs::btrfs::mount_rw(rdev)
        .map_err(|_| "the volume would not mount again for the clean-node check")?;
    let at = ns
        .resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved again")?;
    let _ = ns
        .mount(Arc::clone(&again), &at)
        .map_err(|_| "the volume could not be mounted again for the clean-node check")?;
    fs::btrfs::writable_of(&again).ok_or("a writable mount is not listed for reclaim")
}

/// Look every entry of the wide directory up by name: its name items and
/// its inode item read.
fn look_up_all() -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    for i in 0..ENTRIES {
        let _ = ns
            .resolve(
                &ctx,
                None,
                format!("/mnt-rw/wide/entry-{i:040}").as_bytes(),
                true,
            )
            .and_then(|at| at.inode())
            .map_err(|_| "an entry of the wide directory could not be looked up")?
            .metadata();
    }
    Ok(())
}

/// The wide directory's entries, as a listing reads them: how many, and the
/// CRC-32C of their names and inode numbers in order.
fn list() -> Result<(usize, u32), &'static str> {
    let ns = fs::namespace();
    let flags = OpenFlags {
        read: true,
        directory: true,
        ..OpenFlags::default()
    };
    let dir = ns
        .open(&ns.context(), None, WIDE, &flags, 0)
        .map_err(|_| "the wide directory would not open")?;
    let mut count = 0usize;
    let mut register = !0u32;
    dir.read_dir(&mut |entry| {
        if entry.name != b"." && entry.name != b".." {
            count += 1;
            register = crc32c_update(register, entry.name);
            register = crc32c_update(register, &entry.ino.to_le_bytes());
        }
        true
    })
    .map_err(|_| "the wide directory could not be listed")?;
    Ok((count, !register))
}

/// `/proc/meminfo` counts the writable mounts' clean nodes in
/// `SReclaimable`, and `MemAvailable` holds them above what is free and the
/// clean page cache: with `held` bytes held, the two differ by about that.
fn counted_in_meminfo(held: u64) -> Result<(), &'static str> {
    let text = read_meminfo()?;
    let nodes = fs::btrfs::clean_bytes() / 1024;
    let free = mm::free_frames().saturating_mul(PAGE_SIZE / 1024);
    let pages = cache::totals().reclaimable.saturating_mul(PAGE_SIZE / 1024);
    let reclaimable = field(&text, "SReclaimable")?;
    let available = field(&text, "MemAvailable")?;
    if reclaimable != nodes {
        return Err("SReclaimable is not the clean btrfs nodes' count");
    }
    // What MemAvailable holds besides the free frames and the clean pages,
    // which move a little between the read and the count above: the nodes,
    // within half of them.
    let beyond = available.abs_diff(free.saturating_add(pages));
    if beyond.abs_diff(nodes) > held / 1024 / 2 {
        return Err("MemAvailable does not count the clean btrfs nodes");
    }
    Ok(())
}

/// `/proc/meminfo`, read whole.
fn read_meminfo() -> Result<Vec<u8>, &'static str> {
    let ns = fs::namespace();
    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let file = ns
        .open(&ns.context(), None, b"/proc/meminfo", &flags, 0)
        .map_err(|_| "/proc/meminfo would not open")?;
    let mut out = Vec::new();
    let mut piece = [0u8; 256];
    loop {
        let read = file
            .read(&mut piece)
            .map_err(|_| "/proc/meminfo could not be read")?;
        match piece.get(..read) {
            Some(got) if !got.is_empty() => out.extend_from_slice(got),
            _ => return Ok(out),
        }
    }
}

/// The value of `label`'s line in `/proc/meminfo`, in KiB.
fn field(text: &[u8], label: &str) -> Result<u64, &'static str> {
    let text = core::str::from_utf8(text).map_err(|_| "/proc/meminfo is not text")?;
    text.lines()
        .find_map(|line| {
            let (name, rest) = line.split_once(':')?;
            (name == label).then(|| rest.trim().trim_end_matches(" kB").parse().ok())?
        })
        .ok_or("/proc/meminfo lacks a line the check reads")
}

/// A reclaim of the machine, asked for more than every cached page, reaches
/// the nodes once the dentry cache before them has given what it had.
/// Answers the bytes of nodes it took.
fn reclaim_reaches(volume: &RwBtrfs<Disk>, held: u64) -> Result<u64, &'static str> {
    for _ in 0..ROUNDS {
        let want = cache::totals()
            .disk
            .saturating_add(held.div_ceil(PAGE_SIZE))
            .saturating_add(1);
        let _ = cache::reclaim(quota::NONE, want);
        let left = volume.clean_bytes();
        if left < held {
            return Ok(held - left);
        }
    }
    Err("a reclaim of the machine took no clean btrfs node")
}

/// Give every clean node of `volume` back, a batch at a time, and answer
/// the bytes given and the frames that came free.
fn give_all_back(volume: &RwBtrfs<Disk>) -> Result<(u64, u64), &'static str> {
    let before = mm::free_frames();
    let mut given = 0u64;
    loop {
        let more = volume.drop_clean(u64::MAX);
        if more == 0 {
            break;
        }
        given = given.saturating_add(more);
    }
    let frames = mm::free_frames().saturating_sub(before);
    if volume.clean_bytes() != 0 {
        return Err("giving back every clean node left some counted");
    }
    if given == 0 {
        return Err("a volume holding clean nodes gave none back");
    }
    // Whole pages: a node's item list and most of its items are its own,
    // made together when it was read.
    if frames.saturating_mul(PAGE_SIZE) < given / 4 {
        return Err("clean btrfs nodes given back freed heap but few frames");
    }
    Ok((given, frames))
}
