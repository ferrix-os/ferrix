//! Allocating a chunk: turning free device space into a new block group.
//!
//! `mkfs.btrfs` makes one small chunk of each kind — on a default volume an
//! 8 MiB data chunk — and leaves the rest of the device unallocated, so
//! writing anything sizeable means making chunks. A chunk touches five
//! places, and all five must agree for `btrfs check` to accept the volume:
//!
//! 1. a `CHUNK_ITEM` in the chunk tree, and for a system chunk a copy in the
//!    superblock's system chunk array;
//! 2. one `DEV_EXTENT` per stripe in the device tree, saying which bytes of
//!    the device the stripe occupies;
//! 3. the device's `DEV_ITEM`, in the chunk tree and embedded in the
//!    superblock, whose `bytes_used` grows by every stripe;
//! 4. a `BLOCK_GROUP_ITEM` in the extent tree;
//! 5. a `FREE_SPACE_INFO` and one `FREE_SPACE_EXTENT` covering it all.
//!
//! Recording those allocates tree blocks, and the chunk being made may be the
//! only place with room for them — it is being made because a kind ran out.
//! So the chunk joins the in-memory layout and allocator first, and the items
//! are written after, allocating from it like any other group.

use alloc::vec::Vec;

use ferrix_btrfs::chunk::{BLOCK_GROUP_PROFILE_MASK, Stripe};
use ferrix_btrfs::items::{
    CHUNK_ITEM_KEY, CHUNK_TREE_OBJECTID, DEV_ITEM_KEY, DEV_ITEMS_OBJECTID, DEV_TREE_OBJECTID,
    EXTENT_TREE_OBJECTID,
};
use ferrix_btrfs::tree::BtrfsKey;

use crate::bytes::{key_bytes, put, put_u32, put_u64};
use crate::chunks::Chunk;
use crate::commit::FREE_SPACE_TREE_OBJECTID;
use crate::extent::{
    BLOCK_GROUP_ITEM_KEY, DEV_EXTENT_KEY, FREE_SPACE_EXTENT_KEY, FREE_SPACE_INFO_KEY,
};
use crate::ranges::RangeSet;
use crate::space::{BlockGroup, Kind};
use crate::{Error, Result, WriteDevice, WriteVolume, fallible};

/// Device space below this is never allocated: it holds the boot area and
/// the primary superblock. Linux's `BTRFS_DEVICE_RANGE_RESERVED`.
const DEVICE_RESERVED: u64 = 1024 * 1024;
/// Chunk sizes are multiples of this.
const CHUNK_ALIGN: u64 = 1024 * 1024;
/// The most [`WriteVolume::tree_headroom`] keeps back for one copy of the
/// trees: a 64 MiB metadata chunk holds the extent and checksum items of
/// some 60 GiB of data.
const TREE_HEADROOM: u64 = 64 * 1024 * 1024;
/// The object id chunk items and device extents name as the chunk tree's
/// first chunk.
const FIRST_CHUNK_TREE_OBJECTID: u64 = 256;

impl<D: WriteDevice> WriteVolume<D> {
    /// The largest chunk of `kind` Linux would make on this device: 1 GiB of
    /// data, 256 MiB of metadata, 32 MiB of system, never more than a tenth
    /// of the device.
    fn chunk_size(&self, kind: Kind) -> u64 {
        let most = match kind {
            Kind::Data => 1024 * 1024 * 1024,
            Kind::Metadata => 256 * 1024 * 1024,
            Kind::System => 32 * 1024 * 1024,
        };
        let tenth = self.geometry.device_size / 10;
        let size = most.min(tenth);
        (size - size % CHUNK_ALIGN).max(CHUNK_ALIGN)
    }

    /// The profile bits new chunks of `kind` get: whatever the volume already
    /// uses for that kind, so a DUP-metadata volume stays DUP.
    fn profile_for(&self, kind: Kind) -> u64 {
        self.chunks
            .iter()
            .find(|chunk| chunk.kind() == kind.bits())
            .map_or(0, |chunk| chunk.type_bits & BLOCK_GROUP_PROFILE_MASK)
    }

    /// The unallocated stretches of the device.
    fn device_holes(&self) -> Result<RangeSet> {
        let mut holes = RangeSet::new();
        let _ = holes.try_insert(
            DEVICE_RESERVED,
            self.geometry.device_size.saturating_sub(DEVICE_RESERVED),
        )?;
        for chunk in self.chunks.iter() {
            for stripe in &chunk.stripes {
                let _ = holes.try_remove(stripe.offset, chunk.length)?;
            }
        }
        Ok(holes)
    }

    /// How many copies of each block a chunk of `kind` holds: two for DUP.
    fn copies(&self, kind: Kind) -> u64 {
        if self.profile_for(kind) == 0 { 1 } else { 2 }
    }

    /// Device space no chunk holds yet, in the whole mebibytes a chunk can
    /// be made of: measured when the chunks change, by
    /// [`Self::measure_unallocated`], because measuring allocates and every
    /// operation's admission asks.
    const fn unallocated(&self) -> u64 {
        self.unallocated
    }

    /// Measure what [`Self::unallocated`] answers from the chunks.
    pub(crate) fn measure_unallocated(&self) -> Result<u64> {
        Ok(self.device_holes()?.iter().fold(0u64, |sum, (_, len)| {
            sum.saturating_add(len - len % CHUNK_ALIGN)
        }))
    }

    /// Unallocated device space data chunks leave alone, so the trees can
    /// still grow on a volume full of data: one metadata chunk of at most
    /// [`TREE_HEADROOM`], in every copy. Without it the data took the last
    /// of the device, and the next edit that needed a tree node found none
    /// half-way through, which aborts the transaction; Linux keeps its
    /// global block reserve for the same reason.
    fn tree_headroom(&self) -> u64 {
        let size = self.chunk_size(Kind::Metadata).min(TREE_HEADROOM);
        size.saturating_mul(self.copies(Kind::Metadata))
    }

    /// Bytes of file data the volume can still take: the room left in its
    /// data block groups, and what new data chunks may still be made of.
    /// What `statfs` reports as available, and what a write is measured
    /// against before it changes anything.
    #[must_use]
    pub fn data_room(&self) -> u64 {
        let spare = self.unallocated().saturating_sub(self.tree_headroom());
        let chunks = (spare - spare % CHUNK_ALIGN) / self.copies(Kind::Data);
        self.space
            .free_bytes(Kind::Data)
            .saturating_add(chunks - chunks % CHUNK_ALIGN)
    }

    /// Bytes of tree nodes the volume can still take: free in its metadata
    /// groups, and what new metadata chunks may be made of, in every copy.
    pub(crate) fn meta_room(&self) -> u64 {
        let chunks = self.unallocated() / self.copies(Kind::Metadata);
        self.space
            .free_bytes(Kind::Metadata)
            .saturating_add(chunks - chunks % CHUNK_ALIGN)
    }

    /// Where a chunk of `kind` would go, as large as the device allows up to
    /// [`Self::chunk_size`], changing nothing. A data chunk is kept out of
    /// [`Self::tree_headroom`]. [`Error::NoSpace`] if there is no room.
    pub(crate) fn place_chunk(&self, kind: Kind) -> Result<Chunk> {
        let profile = self.profile_for(kind);
        let copies = self.copies(kind);
        let holes = self.device_holes()?;
        let mut size = self.chunk_size(kind);
        if kind == Kind::Data {
            let spare = self.unallocated().saturating_sub(self.tree_headroom());
            size = size.min(spare / copies);
            size -= size % CHUNK_ALIGN;
        }
        // Every copy needs a stripe of the same size: sized to the largest
        // hole, the first took the room the second needed, and a DUP chunk
        // could not be made on a device with room for one. So the size
        // starts at what the holes hold for each copy and halves until all
        // the copies fit.
        size = size.min(self.unallocated() / copies);
        size -= size % CHUNK_ALIGN;
        let stripes = loop {
            if size == 0 {
                return Err(Error::NoSpace);
            }
            if let Some(stripes) = try_place_stripes(&holes, size, copies)? {
                break stripes;
            }
            size /= 2;
            size -= size % CHUNK_ALIGN;
        };
        Ok(Chunk {
            logical: self.chunks.next_logical().next_multiple_of(CHUNK_ALIGN),
            length: size,
            type_bits: kind.bits() | profile,
            stripes: fallible::collect(stripes.iter().map(|&offset| Stripe {
                devid: self.geometry.devid,
                offset,
                dev_uuid: self.geometry.dev_uuid,
            }))?,
        })
    }

    /// What [`Self::add_chunk`] puts in memory for `chunk`: its block group,
    /// and a copy of it for the layout. Made before anything changes, so
    /// running out of memory for them aborts nothing.
    pub(crate) fn chunk_parts(&self, chunk: &Chunk) -> Result<(BlockGroup, Chunk)> {
        // The free-space item below covers the whole chunk, as Linux's
        // `add_block_group_free_space` writes it, but a stripe placed over
        // the 64 MiB or 256 GiB superblock must still not be allocated there.
        let mut free = RangeSet::new();
        let _ = free.try_insert(chunk.logical, chunk.length)?;
        let group = BlockGroup::new(
            chunk.logical,
            chunk.length,
            chunk.type_bits,
            0,
            free,
            chunk.superblock_stripes()?,
        )?;
        Ok((group, chunk.try_clone()?))
    }

    /// Make `chunk`, which [`Self::place_chunk`] placed, a block group, and
    /// record it everywhere; `group` and `copy` are its
    /// [`Self::chunk_parts`].
    pub(crate) fn add_chunk(
        &mut self,
        chunk: &Chunk,
        group: BlockGroup,
        copy: Chunk,
    ) -> Result<()> {
        // FALLIBLE: the allocator's own insert, which reports running out of memory.
        self.space.insert(group)?;
        // FALLIBLE: the layout's own insert.
        self.chunks.insert(copy)?;
        self.unallocated = self.measure_unallocated()?;
        self.chunks_changed = true;
        self.record_chunk(chunk)
    }

    /// Write the items describing a new chunk into the four trees.
    fn record_chunk(&mut self, chunk: &Chunk) -> Result<()> {
        let bad = Error::Inconsistent("chunk item does not encode");
        let item = chunk.item_bytes(self.sectorsize())?;
        // FALLIBLE: the tree's own insert, which reports running out of memory.
        self.insert(
            CHUNK_TREE_OBJECTID,
            BtrfsKey::new(FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY, chunk.logical),
            item,
        )?;
        for stripe in &chunk.stripes {
            let mut extent = fallible::zeroed(48)?;
            put_u64(&mut extent, 0, CHUNK_TREE_OBJECTID).ok_or(bad)?;
            put_u64(&mut extent, 8, FIRST_CHUNK_TREE_OBJECTID).ok_or(bad)?;
            put_u64(&mut extent, 16, chunk.logical).ok_or(bad)?;
            put_u64(&mut extent, 24, chunk.length).ok_or(bad)?;
            put(&mut extent, 32, &self.geometry.chunk_tree_uuid).ok_or(bad)?;
            let key = BtrfsKey::new(stripe.devid, DEV_EXTENT_KEY, stripe.offset);
            // FALLIBLE: the tree's own insert.
            self.insert(DEV_TREE_OBJECTID, key, extent)?;
        }
        let dev_key = BtrfsKey::new(DEV_ITEMS_OBJECTID, DEV_ITEM_KEY, self.geometry.devid);
        let mut dev_item = self
            .get(CHUNK_TREE_OBJECTID, &dev_key)?
            .ok_or(Error::Inconsistent("device has no DEV_ITEM"))?;
        put_u64(&mut dev_item, 16, self.device_bytes_used()).ok_or(bad)?;
        self.update(CHUNK_TREE_OBJECTID, dev_key, dev_item)?;
        let mut group = fallible::zeroed(24)?;
        put_u64(&mut group, 0, 0).ok_or(bad)?;
        put_u64(&mut group, 8, FIRST_CHUNK_TREE_OBJECTID).ok_or(bad)?;
        put_u64(&mut group, 16, chunk.type_bits).ok_or(bad)?;
        let key = BtrfsKey::new(chunk.logical, BLOCK_GROUP_ITEM_KEY, chunk.length);
        // FALLIBLE: the tree's own insert.
        self.insert(EXTENT_TREE_OBJECTID, key, group)?;
        let mut info = fallible::zeroed(8)?;
        put_u32(&mut info, 0, 1).ok_or(bad)?;
        let tree = FREE_SPACE_TREE_OBJECTID;
        // FALLIBLE: the tree's own insert.
        self.insert(
            tree,
            BtrfsKey::new(chunk.logical, FREE_SPACE_INFO_KEY, chunk.length),
            info,
        )?;
        // FALLIBLE: the tree's own insert.
        self.insert(
            tree,
            BtrfsKey::new(chunk.logical, FREE_SPACE_EXTENT_KEY, chunk.length),
            Vec::new(),
        )
    }

    /// The superblock's system chunk array: the key and item of every system
    /// chunk, back to back.
    pub(crate) fn system_chunk_array(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for chunk in self
            .chunks
            .iter()
            .filter(|chunk| chunk.kind() == Kind::System.bits())
        {
            let key = BtrfsKey::new(FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY, chunk.logical);
            fallible::extend_from_slice(&mut out, &key_bytes(&key))?;
            fallible::extend_from_slice(&mut out, &chunk.item_bytes(self.sectorsize())?)?;
        }
        if out.len() > ferrix_btrfs::superblock::SYS_CHUNK_ARRAY_SIZE {
            return Err(Error::NoSpace);
        }
        Ok(out)
    }
}

/// Where `copies` stripes of `size` bytes go in `holes`, each at the first
/// aligned place left; `None` if they do not all fit.
pub(crate) fn try_place_stripes(
    holes: &RangeSet,
    size: u64,
    copies: u64,
) -> Result<Option<Vec<u64>>> {
    let mut holes = holes.try_clone()?;
    let mut stripes = Vec::new();
    while (stripes.len() as u64) < copies {
        let Some((at, _)) = holes.first_prefix(size, size, CHUNK_ALIGN, 0) else {
            return Ok(None);
        };
        let _ = holes.try_remove(at, size)?;
        fallible::push(&mut stripes, at)?;
    }
    Ok(Some(stripes))
}

/// [`try_place_stripes`], for the tests.
#[cfg(test)]
pub(crate) fn place_stripes(holes: &RangeSet, size: u64, copies: u64) -> Option<Vec<u64>> {
    try_place_stripes(holes, size, copies).expect("host memory")
}
