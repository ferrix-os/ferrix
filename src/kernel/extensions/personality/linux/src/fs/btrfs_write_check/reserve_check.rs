//! A writable mount's memory reservation, its lifecycle (W1-b,
//! `docs/BTRFS-PRESSURE.md` W1, ledger 8337 W1.7): bound as the volume is
//! mounted, promising nothing between operations, unbound once the volume is
//! dropped; and a mount that finds no entry free made all the same, without
//! one.
//!
//! And its draw (W1-c): the mount's edits and commits run inside it, so
//! frames are drawn from it, none of their allocations of at most a page
//! runs past what each was admitted for, and nothing stays promised once
//! they are done ([`drawn`]).
//!
//! Its negative controls: the mount's handle not unbinding in its drop, which
//! leaves the entry bound after the unmount ([`unmounted`]); and the
//! writer's operations run outside their reservation, which draws nothing
//! ([`drawn`]).

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_vfs::{FileSystem, NewNode};

use crate::fs;
use crate::mm;

/// What the check's mount makes: a directory and a file in it.
const DIRECTORY: &[u8] = b"reserve-check";
const FILE: &[u8] = b"written";

/// The whole check, on the disk numbered `rdev` while nothing else mounts
/// it: mounted writable, a directory and a file of 64 KiB made, synced,
/// removed and synced again, every step's reservation as [`mounted`],
/// [`drawn`] and [`unmounted`] require; then mounted with none free
/// ([`mount_with_none_free`]). Answers the frames the edits left drawn.
///
/// # Errors
///
/// What did not hold, as a sentence.
pub(super) fn run(rdev: u64) -> Result<u64, &'static str> {
    let promise = mm::outstanding_promise();
    let volume = fs::btrfs::mount_rw(rdev)
        .map_err(|_| "reserve-check: the disk would not mount writable")?;
    let (entry, overruns) = mounted(&volume, promise)?;
    let root = volume.root();
    let dir = root
        .create(DIRECTORY, NewNode::Directory, 0o755)
        .map_err(|_| "reserve-check: the directory could not be made")?;
    let file = dir
        .create(FILE, NewNode::Regular, 0o644)
        .map_err(|_| "reserve-check: the file could not be made")?;
    let bytes = crate::fallible::try_filled(0x5a_u8, 64 * 1024)
        .map_err(|_| "reserve-check: no room for the file's bytes")?;
    let _ = file
        .write_at(0, &bytes, false)
        .map_err(|_| "reserve-check: the file could not be written")?;
    volume
        .sync()
        .map_err(|_| "reserve-check: the volume could not be synced")?;
    let left = drawn(entry, overruns, promise)?;
    drop(file);
    dir.unlink(FILE)
        .map_err(|_| "reserve-check: the file could not be removed")?;
    root.rmdir(DIRECTORY)
        .map_err(|_| "reserve-check: the directory could not be removed")?;
    volume
        .sync()
        .map_err(|_| "reserve-check: the volume could not be synced again")?;
    if mm::heap_scope_overruns(entry) != overruns {
        return Err("reserve-check: a removal ran past its memory reservation");
    }
    if mm::outstanding_promise() != promise {
        return Err("reserve-check: the reservation stayed promised after the removals");
    }
    drop((dir, root, volume));
    unmounted(entry, promise)?;
    drop(mount_with_none_free(rdev)?);
    Ok(left)
}

/// The writable mount `filesystem`, just made, with `before` the outstanding
/// promise before it: bound to a reservation of its own, which promises
/// nothing now that no operation runs. Answers the entry and its overruns
/// so far, the count since boot: an entry's earlier owner may have left some
/// (the stage 9 check's scope makes one on purpose).
///
/// # Errors
///
/// What did not hold, as a sentence.
///
/// Verifies: L.object.225
pub(super) fn mounted(
    filesystem: &Arc<dyn FileSystem>,
    before: u64,
) -> Result<(mm::Reservation, u64), &'static str> {
    let entry = fs::btrfs::reservation_of(filesystem)
        .ok_or("a writable mount bound no memory reservation with entries free")?;
    if mm::reservation_resize(entry, 0).is_err() {
        return Err("a writable mount's reservation was not bound");
    }
    if mm::outstanding_promise() != before {
        return Err("a writable mount's reservation promised frames between operations");
    }
    Ok((entry, mm::heap_scope_overruns(entry)))
}

/// After the mount that drew from `entry` was unmounted and every hold of
/// it let go: the entry unbound, and the promise `before` again.
///
/// # Errors
///
/// What did not hold, as a sentence.
pub(super) fn unmounted(entry: mm::Reservation, before: u64) -> Result<(), &'static str> {
    // A shrink is refused only for an entry not bound (`L.mm.76`).
    if mm::reservation_resize(entry, 0).is_ok() {
        return Err("an unmounted volume's memory reservation stayed bound");
    }
    if mm::outstanding_promise() != before {
        return Err("an unmounted volume's memory reservation left a promise");
    }
    Ok(())
}

/// Mount the disk numbered `rdev` writable with every free reservation
/// bound by the check, as a program making reserved jobs could bind them:
/// the mount is made all the same, without one, and the entries are given
/// back at once (W1.7). For that moment every free entry is the check's,
/// so a reserved job made meanwhile by another task is refused one, as it
/// would be with them all bound by jobs.
///
/// # Errors
///
/// What did not hold, as a sentence.
pub(super) fn mount_with_none_free(rdev: u64) -> Result<Arc<dyn FileSystem>, &'static str> {
    let mut taken = Vec::new();
    taken
        .try_reserve(64)
        .map_err(|_| "no room to hold the reservations the check binds")?;
    while let Some(entry) = mm::reservation_bind() {
        if taken.len() == taken.capacity() {
            mm::reservation_unbind(entry);
            break;
        }
        taken.push(entry);
    }
    let mounted = fs::btrfs::mount_rw(rdev);
    for &entry in &taken {
        mm::reservation_unbind(entry);
    }
    let filesystem =
        mounted.map_err(|_| "a writable mount with no reservation free was refused")?;
    if fs::btrfs::reservation_of(&filesystem).is_some() {
        return Err("a writable mount found a reservation with none free");
    }
    Ok(filesystem)
}

/// After the mount drawing from `entry` built a tree and synced it, with
/// `before` the promise before the mount and `overruns` the entry's count
/// at the mount: its edits and commits drew frames
/// from the reservation, none of their allocations of at most a page ran
/// past it (`mm::heap_scope_overruns`, W1.5), and it promises nothing now.
/// Answers the frames drawn.
///
/// # Errors
///
/// What did not hold, as a sentence.
///
/// L.btrfs.125 in the kernel. Its `Verifies` is the item crate's host test
/// (`every_kind_of_edit_runs_within_its_admission`): a check in the load
/// ring is no home for an item requirement (`docs/MICROKERNEL.md` 6.2).
pub(super) fn drawn(
    entry: mm::Reservation,
    overruns: u64,
    before: u64,
) -> Result<u64, &'static str> {
    let (_, drawn) = mm::reservation_usage(entry);
    if drawn == 0 {
        return Err("a writable mount's edits drew nothing from its memory reservation");
    }
    if mm::heap_scope_overruns(entry) != overruns {
        return Err("a writable mount's edit ran past its memory reservation");
    }
    if mm::outstanding_promise() != before {
        return Err("a writable mount's reservation stayed promised after its operations");
    }
    Ok(drawn)
}
