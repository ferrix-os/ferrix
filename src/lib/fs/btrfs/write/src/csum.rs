//! The checksum tree: one CRC-32C per data sector, in runs.
//!
//! An `EXTENT_CSUM` item is keyed by the logical address its first sector is
//! at and holds four bytes per sector, with no header. Runs may be split and
//! joined freely — btrfs only requires that no two overlap — so this writer
//! keeps it simple: new data gets items of its own, and freeing an extent
//! cuts every item it overlaps down to the parts outside it.
//!
//! `btrfs check` compares the two directions: a checksum with no data extent
//! under it is an error, and so is a checksummed file's extent with a sector
//! the tree has no checksum for. So the sums of an extent must go exactly
//! when its last reference does.

use ferrix_btrfs::items::{CSUM_TREE_OBJECTID, EXTENT_CSUM_KEY, EXTENT_CSUM_OBJECTID};
use ferrix_btrfs::tree::{BtrfsKey, HEADER_SIZE, ITEM_SIZE};

use crate::{Error, Result, WriteDevice, WriteVolume, fallible};

/// Bytes per checksum.
const CSUM_SIZE: u64 = 4;

/// The key of the checksum item starting at `logical`.
const fn csum_key(logical: u64) -> BtrfsKey {
    BtrfsKey::new(EXTENT_CSUM_OBJECTID, EXTENT_CSUM_KEY, logical)
}

/// Delete the sums of `[start, start + len)`, keeping the parts of any item
/// that lie outside it.
pub(crate) fn delete_range<D: WriteDevice>(
    volume: &mut WriteVolume<D>,
    start: u64,
    len: u64,
) -> Result<()> {
    let sector = u64::from(volume.sectorsize());
    let end = start
        .checked_add(len)
        .ok_or(Error::Inconsistent("checksum range wraps"))?;
    let first = volume
        .prev_item(CSUM_TREE_OBJECTID, &csum_key(start))?
        .filter(|(key, _)| key.objectid == EXTENT_CSUM_OBJECTID && key.item_type == EXTENT_CSUM_KEY)
        .map_or(csum_key(start), |(key, _)| key);
    let last = csum_key(end.saturating_sub(1));
    for (key, data) in volume.range(CSUM_TREE_OBJECTID, &first, &last)? {
        let sums = (data.len() as u64) / CSUM_SIZE;
        let item_end = key.offset.saturating_add(sums.saturating_mul(sector));
        if item_end <= start || key.offset >= end {
            continue;
        }
        // The parts kept are copied before the item goes, so running out
        // of memory for them changes nothing.
        let head = if key.offset < start {
            let keep = ((start - key.offset) / sector).saturating_mul(CSUM_SIZE);
            let head = data
                .get(..usize::try_from(keep).map_err(|_| Error::ItemTooLarge)?)
                .unwrap_or_default();
            Some(fallible::copy(head)?)
        } else {
            None
        };
        let tail = if item_end > end {
            let skip = ((end - key.offset) / sector).saturating_mul(CSUM_SIZE);
            let tail = data
                .get(usize::try_from(skip).map_err(|_| Error::ItemTooLarge)?..)
                .unwrap_or_default();
            Some(fallible::copy(tail)?)
        } else {
            None
        };
        volume.delete(CSUM_TREE_OBJECTID, &key)?;
        if let Some(head) = head {
            // FALLIBLE: the tree's own insert, which reports running out of memory.
            volume.insert(CSUM_TREE_OBJECTID, key, head)?;
        }
        if let Some(tail) = tail {
            // FALLIBLE: the tree's own insert.
            volume.insert(CSUM_TREE_OBJECTID, csum_key(end), tail)?;
        }
    }
    Ok(())
}

/// Insert the sums of the sectors starting at `start`, one per entry of
/// `sums`, in items no larger than a leaf holds comfortably.
///
/// The range must hold no sums already: it is newly allocated data, and the
/// sums of whatever was there before went when that was freed.
pub(crate) fn insert_sums<D: WriteDevice>(
    volume: &mut WriteVolume<D>,
    start: u64,
    sums: &[u32],
) -> Result<()> {
    let sector = u64::from(volume.sectorsize());
    // Linux's `MAX_CSUM_ITEMS`: what fits in a leaf beside two descriptors,
    // less one.
    let per_item = (volume.nodesize() as usize)
        .saturating_sub(HEADER_SIZE)
        .saturating_sub(2 * ITEM_SIZE)
        / CSUM_SIZE as usize;
    let per_item = per_item.saturating_sub(1).max(1);
    let mut at = start;
    for piece in sums.chunks(per_item) {
        let mut data = fallible::with_capacity(piece.len().saturating_mul(CSUM_SIZE as usize))?;
        for sum in piece {
            fallible::extend_from_slice(&mut data, &sum.to_le_bytes())?;
        }
        // FALLIBLE: the tree's own insert.
        volume.insert(CSUM_TREE_OBJECTID, csum_key(at), data)?;
        at = at.saturating_add((piece.len() as u64).saturating_mul(sector));
    }
    Ok(())
}
