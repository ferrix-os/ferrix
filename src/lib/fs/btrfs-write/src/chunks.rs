//! The chunk layout, with every stripe: where each copy of a logical range is.
//!
//! `ferrix_btrfs::chunk::ChunkMap` keeps one stripe per chunk, which is all a
//! reader needs: every stripe of a mirrored profile is a whole copy. A writer
//! needs all of them, because a DUP chunk written through its first stripe
//! alone leaves a second copy that disagrees, and a later read that falls back
//! to it — or a scrub — finds a node from an older transaction.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::chunk::{
    BLOCK_GROUP_DUP, BLOCK_GROUP_PROFILE_MASK, BLOCK_GROUP_TYPE_MASK, ChunkItem, Stripe,
};

use ferrix_btrfs::superblock::{PRIMARY_OFFSET, SUPERBLOCK_OFFSETS};

use crate::bytes::{put, put_u16, put_u32, put_u64};
use crate::ranges::RangeSet;
use crate::{Error, Result, Unsupported, fallible};

/// One chunk: a logical range and the physical places it is stored.
///
/// Not `Clone` outside the tests: a copy allocates, and is
/// [`Chunk::try_clone`].
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct Chunk {
    /// First logical address.
    pub(crate) logical: u64,
    /// Length of the logical range, and of each stripe.
    pub(crate) length: u64,
    /// Type and profile bits.
    pub(crate) type_bits: u64,
    /// Every copy.
    pub(crate) stripes: Vec<Stripe>,
}

impl Chunk {
    /// A chunk from its item, filed at logical address `logical`.
    ///
    /// Only `SINGLE` (one stripe) and `DUP` (two, on the one device) are
    /// writable: every other profile either stripes or spans devices.
    pub(crate) fn from_item(logical: u64, item: &ChunkItem<'_>) -> Result<Chunk> {
        let profile = item.type_bits() & BLOCK_GROUP_PROFILE_MASK;
        let stripes: Vec<Stripe> = fallible::collect(item.stripes())?;
        let copies = if profile == BLOCK_GROUP_DUP { 2 } else { 1 };
        if (profile != 0 && profile != BLOCK_GROUP_DUP) || stripes.len() != copies {
            return Err(Error::Unsupported(Unsupported::Profile));
        }
        Ok(Chunk {
            logical,
            length: item.length(),
            type_bits: item.type_bits(),
            stripes,
        })
    }

    /// A copy of the chunk.
    pub(crate) fn try_clone(&self) -> Result<Chunk> {
        Ok(Chunk {
            logical: self.logical,
            length: self.length,
            type_bits: self.type_bits,
            stripes: fallible::copy(&self.stripes)?,
        })
    }

    /// Logical end, exclusive.
    pub(crate) fn end(&self) -> u64 {
        self.logical.saturating_add(self.length)
    }

    /// The type bits alone: data, metadata or system.
    pub(crate) fn kind(&self) -> u64 {
        self.type_bits & BLOCK_GROUP_TYPE_MASK
    }

    /// The logical ranges of this chunk that a superblock copy lies in, which
    /// are never to be allocated: Linux's `exclude_super_stripes`.
    ///
    /// Each superblock offset is mapped back through every stripe holding
    /// it, as `btrfs_rmap_block` does, and excluded for a whole
    /// [`STRIPE_LEN`] from there, clipped to the chunk. A chunk starting
    /// below the primary superblock also loses everything up to it. The
    /// free-space tree still lists these ranges as free — Linux never takes
    /// them out of it — so only the allocator may not hand them out: a tree
    /// block placed there is overwritten by the next commit's superblocks.
    pub(crate) fn superblock_stripes(&self) -> Result<RangeSet> {
        let mut out = RangeSet::new();
        if self.logical < PRIMARY_OFFSET {
            out.try_add(self.logical, PRIMARY_OFFSET.min(self.end()) - self.logical)?;
        }
        for &offset in &SUPERBLOCK_OFFSETS {
            for stripe in &self.stripes {
                let Some(within) = offset
                    .checked_sub(stripe.offset)
                    .filter(|&within| within < self.length)
                else {
                    continue;
                };
                // SINGLE and DUP stripes are whole copies, so the offset
                // into the stripe is the offset into the chunk.
                let at = self.logical.saturating_add(within);
                out.try_add(at, STRIPE_LEN.min(self.end() - at))?;
            }
        }
        Ok(out)
    }

    /// The `CHUNK_ITEM` payload for this chunk, as the kernel writes one for
    /// a single-device volume.
    pub(crate) fn item_bytes(&self, sectorsize: u32) -> Result<Vec<u8>> {
        let bad = Error::Inconsistent("chunk item does not encode");
        let len = 32usize
            .checked_mul(self.stripes.len())
            .and_then(|stripes| stripes.checked_add(48))
            .ok_or(bad)?;
        let mut out = fallible::zeroed(len)?;
        self.encode_item(&mut out, sectorsize).ok_or(bad)?;
        Ok(out)
    }

    /// Lay the `CHUNK_ITEM` out in `out`, which is exactly its size.
    fn encode_item(&self, out: &mut [u8], sectorsize: u32) -> Option<()> {
        let num = u16::try_from(self.stripes.len()).ok()?;
        put_u64(out, 0, self.length)?;
        // The owner is always the extent tree's id, for historical reasons.
        put_u64(out, 8, ferrix_btrfs::items::EXTENT_TREE_OBJECTID)?;
        put_u64(out, 16, STRIPE_LEN)?;
        put_u64(out, 24, self.type_bits)?;
        put_u32(out, 32, STRIPE_LEN as u32)?;
        put_u32(out, 36, STRIPE_LEN as u32)?;
        put_u32(out, 40, sectorsize)?;
        put_u16(out, 44, num)?;
        put_u16(out, 46, 1)?;
        for (index, stripe) in self.stripes.iter().enumerate() {
            let at = 48usize.checked_add(index.checked_mul(32)?)?;
            put_u64(out, at, stripe.devid)?;
            put_u64(out, at.checked_add(8)?, stripe.offset)?;
            put(out, at.checked_add(16)?, &stripe.dev_uuid)?;
        }
        Some(())
    }
}

/// The stripe length every chunk this crate makes records: 64 KiB, as Linux.
pub(crate) const STRIPE_LEN: u64 = 64 * 1024;

/// Every chunk of the volume, by logical address.
#[derive(Debug, Default)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct Chunks {
    by_logical: BTreeMap<u64, Chunk>,
}

impl Chunks {
    /// Add a chunk. The same chunk twice — the system chunk array repeats the
    /// chunk tree — is accepted once; a different chunk overlapping one
    /// already present is damage.
    pub(crate) fn insert(&mut self, chunk: Chunk) -> Result<()> {
        if let Some(existing) = self.by_logical.get(&chunk.logical) {
            return if *existing == chunk {
                Ok(())
            } else {
                Err(Error::Volume(BtrfsError::BadChunk))
            };
        }
        let before = self.by_logical.range(..chunk.logical).next_back();
        let after = self.by_logical.range(chunk.logical..).next();
        if before.is_some_and(|(_, prev)| prev.end() > chunk.logical)
            || after.is_some_and(|(&start, _)| start < chunk.end())
        {
            return Err(Error::Volume(BtrfsError::BadChunk));
        }
        let _ = fallible::insert(&mut self.by_logical, chunk.logical, chunk)?;
        Ok(())
    }

    /// The chunk holding `logical`.
    pub(crate) fn lookup(&self, logical: u64) -> Option<&Chunk> {
        let (_, chunk) = self.by_logical.range(..=logical).next_back()?;
        (logical < chunk.end()).then_some(chunk)
    }

    /// Every physical offset holding `[logical, logical + len)`, first stripe
    /// first. The range must lie inside one chunk.
    pub(crate) fn copies(&self, logical: u64, len: u64) -> Result<Vec<u64>> {
        let chunk = self
            .lookup(logical)
            .ok_or(Error::Volume(BtrfsError::NotMapped(logical)))?;
        let within = logical - chunk.logical;
        if logical.checked_add(len).is_none_or(|end| end > chunk.end()) {
            return Err(Error::Volume(BtrfsError::NotMapped(chunk.end())));
        }
        fallible::collect_ok(chunk.stripes.iter().map(|stripe| {
            stripe
                .offset
                .checked_add(within)
                .ok_or(Error::Volume(BtrfsError::NotMapped(logical)))
        }))
    }

    /// Every chunk, in logical order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Chunk> {
        self.by_logical.values()
    }

    /// Where the next chunk's logical range starts: after the last one, as
    /// Linux's `find_next_chunk`.
    pub(crate) fn next_logical(&self) -> u64 {
        self.by_logical.values().next_back().map_or(0, Chunk::end)
    }
}
