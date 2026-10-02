//! Opening a volume for writing: everything the writer needs in memory.
//!
//! The reader bootstraps from the superblock to the default subvolume. The
//! writer needs more, and needs it to be consistent before it may change a
//! byte: the chunk layout with every stripe, the root of every tree, each
//! block group's usage, and what the free-space tree says is free in each.
//! Every one of those is checked against the others as it is loaded — a
//! block group with no chunk, a free-space count that disagrees with its
//! items — because a writer that trusts a wrong free map hands out space
//! something already uses.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::chunk::{ChunkItem, FIRST_CHUNK_TREE_OBJECTID};
use ferrix_btrfs::items::{
    CHUNK_ITEM_KEY, CHUNK_TREE_OBJECTID, DEV_ITEM_KEY, DEV_ITEMS_OBJECTID, DevItem,
    EXTENT_TREE_OBJECTID, FIRST_FREE_OBJECTID, LAST_FREE_OBJECTID, ROOT_ITEM_KEY,
    ROOT_TREE_OBJECTID, RootItem,
};
use ferrix_btrfs::superblock::{PRIMARY_OFFSET, SUPERBLOCK_SIZE, Superblock};
use ferrix_btrfs::tree::{BtrfsKey, NodeHeader};
use ferrix_btrfs::volume::{Device, ReadKind};

use crate::bytes::{get_u32, get_u64};
use crate::chunks::{Chunk, Chunks};
use crate::commit::FREE_SPACE_TREE_OBJECTID;
use crate::extent::{
    BLOCK_GROUP_ITEM_KEY, FREE_SPACE_BITMAP_KEY, FREE_SPACE_EXTENT_KEY, FREE_SPACE_INFO_KEY,
};
use crate::ranges::RangeSet;
use crate::refs::DelayedRefs;
use crate::space::{BlockGroup, Space};
use crate::volume::{Geometry, Root};
use crate::{Error, Result, Unsupported, WriteDevice, WriteVolume, fallible};

/// `compat_ro` bit: the free-space tree exists.
const FREE_SPACE_TREE: u64 = 1 << 0;
/// `compat_ro` bit: the free-space tree is complete and may be trusted.
const FREE_SPACE_TREE_VALID: u64 = 1 << 1;
/// `compat_ro` bit: fs-verity items may exist; they are file items this
/// writer never touches, so a volume with them stays writable.
const VERITY: u64 = 1 << 2;
/// The quota tree's id.
const QUOTA_TREE_OBJECTID: u64 = 8;
/// Object id of log tree roots in the root tree.
const TREE_LOG_OBJECTID: u64 = 0u64.wrapping_sub(6);
/// Object id of orphan items in the root tree: subvolumes being deleted.
const ORPHAN_OBJECTID: u64 = 0u64.wrapping_sub(5);
/// `ROOT_REF` and `ROOT_BACKREF`: links between subvolumes.
const ROOT_BACKREF_KEY: u8 = 144;
const ROOT_REF_KEY: u8 = 156;
/// Free-space info flag: the group is recorded as bitmaps.
const USING_BITMAPS: u32 = 1 << 0;

impl<D: WriteDevice> WriteVolume<D> {
    /// Open the volume on `device` for writing.
    ///
    /// [`Error::Unsupported`] names the reason a volume this writer does not
    /// maintain is refused; such a volume can still be read with
    /// `ferrix-btrfs`.
    pub fn open(device: D) -> Result<Self> {
        let mut volume = Self::open_committed(device)?;
        // Files unlinked while open when the volume was last in use: nothing
        // can hold them open now, so they go, as Linux's orphan cleanup at
        // mount. The deletion is part of the first transaction.
        for ino in volume.orphans()? {
            volume.evict(ino)?;
        }
        volume.admitting = true;
        Ok(volume)
    }

    /// Load the committed state and nothing more: no orphan cleanup, so the
    /// transaction starts empty. What a checker wants.
    pub(crate) fn open_committed(mut device: D) -> Result<Self> {
        let mut block = fallible::zeroed(SUPERBLOCK_SIZE)?;
        device.read_at(PRIMARY_OFFSET, &mut block, ReadKind::Metadata)?;
        let sb = Superblock::parse_at(&block, PRIMARY_OFFSET)?;
        check_writable(&sb)?;
        let dev = DevItem::parse(sb.dev_item())?;
        let mut chunks = Chunks::default();
        for entry in sb.sys_chunk_array() {
            let (key, item) = entry?;
            // FALLIBLE: the layout's own insert, which reports running out of memory.
            chunks.insert(Chunk::from_item(key.offset, &item)?)?;
        }
        let mut volume = WriteVolume {
            device,
            geometry: Geometry {
                nodesize: sb.nodesize(),
                sectorsize: sb.sectorsize(),
                fsid: sb.fsid(),
                chunk_tree_uuid: [0; 16],
                devid: dev.devid,
                dev_uuid: dev.uuid,
                device_size: dev.total_bytes,
            },
            chunks,
            unallocated: 0,
            superblock: fallible::copy(&block)?,
            committed: sb.generation(),
            transid: sb.generation().saturating_add(1),
            dirty: BTreeMap::new(),
            clean: BTreeMap::new(),
            roots: BTreeMap::new(),
            stale_roots: BTreeSet::new(),
            refs: DelayedRefs::default(),
            space: Space::default(),
            chunks_changed: false,
            growing: false,
            aborted: false,
            abort_cause: None,
            read_only: false,
            depth: 0,
            admitting: false,
            edits: 0,
        };
        let _ = fallible::insert(
            &mut volume.roots,
            CHUNK_TREE_OBJECTID,
            Root {
                bytenr: sb.chunk_root(),
                level: sb.chunk_root_level(),
                generation: sb.chunk_root_generation(),
            },
        )?;
        let _ = fallible::insert(
            &mut volume.roots,
            ROOT_TREE_OBJECTID,
            Root {
                bytenr: sb.root(),
                level: sb.root_level(),
                generation: sb.generation(),
            },
        )?;
        volume.geometry.chunk_tree_uuid = volume.read_chunk_tree_uuid(sb.chunk_root())?;
        volume.load_chunks()?;
        volume.unallocated = volume.measure_unallocated()?;
        volume.load_roots()?;
        volume.load_block_groups()?;
        // A log the last mount left behind holds items that are not in the
        // trees, and names extents the committed trees think are free. It is
        // replayed here, before anything can allocate over them.
        let log = sb.log_root();
        if log != 0 {
            volume.replay_log(log, sb.log_root_level(), sb.log_root_transid())?;
        }
        Ok(volume)
    }

    /// Throw the running transaction away and reload the committed state.
    ///
    /// Nothing the transaction did reached the disk in a form the superblock
    /// names — nodes it wrote went to space the committed trees do not use —
    /// so the last commit is intact and reopening from it is complete.
    pub fn abort(self) -> Result<Self> {
        Self::open(self.device)
    }

    /// Throw the running transaction away, as [`WriteVolume::abort`] does,
    /// in place, and take no more changes: what a mount does after a
    /// transaction aborted, as Linux turns the volume read-only. Reads go
    /// on against the last commit; every edit answers [`Error::ReadOnly`].
    ///
    /// # Errors
    ///
    /// The committed state could not be read; the volume is left as it was.
    #[expect(
        clippy::unneeded_field_pattern,
        reason = "every field named, so one added later cannot be left \
                  holding the aborted transaction's state"
    )]
    pub fn reload_read_only(&mut self) -> Result<()> {
        let WriteVolume {
            device: _,
            geometry,
            chunks,
            unallocated,
            superblock,
            committed,
            transid,
            dirty,
            clean,
            roots,
            stale_roots,
            refs,
            space,
            chunks_changed,
            growing,
            aborted,
            abort_cause,
            read_only: _,
            edits,
            depth,
            admitting: _,
        } = WriteVolume::open_committed(Borrowed(&mut self.device))?;
        // A log the reload replayed, whose replay itself failed: the
        // volume stays as it was, aborted.
        if aborted {
            return Err(abort_cause.unwrap_or(Error::Aborted));
        }
        self.geometry = geometry;
        self.chunks = chunks;
        self.unallocated = unallocated;
        self.superblock = superblock;
        self.committed = committed;
        self.transid = transid;
        self.dirty = dirty;
        self.clean = clean;
        self.roots = roots;
        self.stale_roots = stale_roots;
        self.refs = refs;
        self.space = space;
        self.chunks_changed = chunks_changed;
        self.growing = growing;
        self.aborted = aborted;
        self.abort_cause = abort_cause;
        self.read_only = true;
        self.edits = edits;
        self.depth = depth;
        Ok(())
    }

    fn read_chunk_tree_uuid(&mut self, chunk_root: u64) -> Result<[u8; 16]> {
        let size = self.geometry.nodesize as usize;
        let mut buf = fallible::zeroed(size)?;
        let copies = self
            .chunks
            .copies(chunk_root, u64::from(self.geometry.nodesize))?;
        let first = copies
            .first()
            .copied()
            .ok_or(Error::Inconsistent("chunk root has no copy"))?;
        self.device.read_at(first, &mut buf, ReadKind::Metadata)?;
        Ok(NodeHeader::parse(&buf)?.chunk_tree_uuid)
    }

    /// Every chunk in the chunk tree, with all its stripes.
    fn load_chunks(&mut self) -> Result<()> {
        let from = BtrfsKey::new(FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY, 0);
        let to = BtrfsKey::new(FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY, u64::MAX);
        for (key, data) in self.range(CHUNK_TREE_OBJECTID, &from, &to)? {
            let item = ChunkItem::parse(&data)?;
            item.check_sectorsize(key.offset, self.geometry.sectorsize)?;
            // FALLIBLE: the layout's own insert.
            self.chunks.insert(Chunk::from_item(key.offset, &item)?)?;
        }
        let dev_key = BtrfsKey::new(DEV_ITEMS_OBJECTID, DEV_ITEM_KEY, self.geometry.devid);
        let dev = self
            .get(CHUNK_TREE_OBJECTID, &dev_key)?
            .ok_or(Error::Inconsistent("device has no DEV_ITEM"))?;
        let dev = DevItem::parse(&dev)?;
        if dev.uuid != self.geometry.dev_uuid || dev.total_bytes != self.geometry.device_size {
            return Err(Error::Inconsistent(
                "DEV_ITEM disagrees with the superblock",
            ));
        }
        Ok(())
    }

    /// The root of every tree the root tree lists, refusing a volume with
    /// anything this writer cannot keep consistent.
    fn load_roots(&mut self) -> Result<()> {
        let items = self.range(ROOT_TREE_OBJECTID, &BtrfsKey::MIN, &BtrfsKey::MAX)?;
        for (key, data) in items {
            let id = key.objectid;
            match key.item_type {
                ROOT_ITEM_KEY if id == QUOTA_TREE_OBJECTID => {
                    return Err(Error::Unsupported(Unsupported::Quotas));
                }
                ROOT_ITEM_KEY if id == TREE_LOG_OBJECTID => {
                    return Err(Error::Unsupported(Unsupported::Log));
                }
                ROOT_ITEM_KEY
                    if (FIRST_FREE_OBJECTID..=LAST_FREE_OBJECTID).contains(&id)
                        || key.offset != 0 =>
                {
                    return Err(Error::Unsupported(Unsupported::Subvolumes));
                }
                ROOT_ITEM_KEY => {
                    let item = RootItem::parse(&data)?;
                    let root = Root {
                        bytenr: item.bytenr,
                        level: item.level,
                        generation: item.generation,
                    };
                    let _ = fallible::insert(&mut self.roots, id, root)?;
                }
                ROOT_REF_KEY | ROOT_BACKREF_KEY => {
                    return Err(Error::Unsupported(Unsupported::Subvolumes));
                }
                _ if id == ORPHAN_OBJECTID => {
                    return Err(Error::Unsupported(Unsupported::Subvolumes));
                }
                _ => {}
            }
        }
        for tree in [
            EXTENT_TREE_OBJECTID,
            FREE_SPACE_TREE_OBJECTID,
            ferrix_btrfs::items::CSUM_TREE_OBJECTID,
            ferrix_btrfs::items::DEV_TREE_OBJECTID,
        ] {
            let _ = self.root(tree)?;
        }
        Ok(())
    }

    /// A block group for every chunk, with its usage and free space.
    fn load_block_groups(&mut self) -> Result<()> {
        let chunks: Vec<(u64, u64, u64, RangeSet)> =
            fallible::collect_ok(self.chunks.iter().map(|chunk| {
                let stripes = chunk.superblock_stripes()?;
                Ok((chunk.logical, chunk.length, chunk.type_bits, stripes))
            }))?;
        for (start, length, type_bits, superblocks) in chunks {
            let key = BtrfsKey::new(start, BLOCK_GROUP_ITEM_KEY, length);
            let item = self
                .get(EXTENT_TREE_OBJECTID, &key)?
                .ok_or(Error::Inconsistent("chunk without a block group item"))?;
            let bad = Error::Inconsistent("malformed block group item");
            let used = get_u64(&item, 0).ok_or(bad)?;
            let flags = get_u64(&item, 16).ok_or(bad)?;
            if flags != type_bits || used > length {
                return Err(bad);
            }
            // The free-space tree lists the superblock stripes as free, so
            // they are taken out here, as Linux's `add_new_free_space` skips
            // what `exclude_super_stripes` excluded.
            let (free, bitmaps) = self.load_free_space(start, length)?;
            let mut group = BlockGroup::new(start, length, flags, used, free, superblocks)?;
            group.bitmaps = bitmaps;
            // FALLIBLE: the allocator's own insert, which reports running out of memory.
            self.space.insert(group)?;
        }
        Ok(())
    }

    /// What the free-space tree records as free in the group at `start`, and
    /// whether it records it as bitmaps.
    fn load_free_space(&mut self, start: u64, length: u64) -> Result<(RangeSet, bool)> {
        let bad = Error::Inconsistent("free-space tree disagrees with itself");
        let tree = FREE_SPACE_TREE_OBJECTID;
        let info = self
            .get(tree, &BtrfsKey::new(start, FREE_SPACE_INFO_KEY, length))?
            .ok_or(Error::Unsupported(Unsupported::NoFreeSpaceTree))?;
        let count = get_u32(&info, 0).ok_or(bad)?;
        let bitmaps = get_u32(&info, 4).ok_or(bad)? & USING_BITMAPS != 0;
        let end = start.checked_add(length).ok_or(bad)?;
        let from = BtrfsKey::new(start, FREE_SPACE_EXTENT_KEY, 0);
        let to = BtrfsKey::new(end.saturating_sub(1), FREE_SPACE_BITMAP_KEY, u64::MAX);
        let mut free = RangeSet::new();
        let mut extents = 0u32;
        for (key, data) in self.range(tree, &from, &to)? {
            let run_end = key.objectid.checked_add(key.offset).ok_or(bad)?;
            if run_end > end {
                return Err(bad);
            }
            match key.item_type {
                FREE_SPACE_EXTENT_KEY if !bitmaps => {
                    if !free.try_insert(key.objectid, key.offset)? {
                        return Err(bad);
                    }
                    extents = extents.saturating_add(1);
                }
                FREE_SPACE_BITMAP_KEY if bitmaps => self.add_bitmap(&mut free, &key, &data)?,
                _ => return Err(bad),
            }
        }
        // A bitmap group's count is of the runs its bits describe.
        let runs = if bitmaps {
            u32::try_from(free.runs()).map_err(|_| bad)?
        } else {
            extents
        };
        if runs != count {
            return Err(bad);
        }
        Ok((free, bitmaps))
    }

    /// Add the free sectors a `FREE_SPACE_BITMAP` item marks.
    fn add_bitmap(&self, free: &mut RangeSet, key: &BtrfsKey, data: &[u8]) -> Result<()> {
        let bad = Error::Inconsistent("malformed free-space bitmap");
        let sector = u64::from(self.geometry.sectorsize);
        let bits = key.offset / sector;
        if bits.div_ceil(8) != data.len() as u64 {
            return Err(bad);
        }
        for bit in 0..bits {
            let byte = data
                .get(usize::try_from(bit / 8).map_err(|_| bad)?)
                .copied()
                .ok_or(bad)?;
            if byte & (1 << (bit % 8)) != 0 {
                let at = key
                    .objectid
                    .checked_add(bit.saturating_mul(sector))
                    .ok_or(bad)?;
                if !free.try_insert(at, sector)? {
                    return Err(bad);
                }
            }
        }
        Ok(())
    }
}

/// Refuse a volume this writer would damage.
fn check_writable(sb: &Superblock<'_>) -> Result<()> {
    use ferrix_btrfs::superblock::IncompatFlags;
    let incompat = sb.incompat_flags();
    if incompat.unknown() != 0 {
        return Err(Error::Volume(BtrfsError::UnsupportedFeature(
            incompat.unknown(),
        )));
    }
    if sb.num_devices() != 1 {
        return Err(Error::Unsupported(Unsupported::MultipleDevices));
    }
    if !incompat.skinny_metadata() {
        return Err(Error::Unsupported(Unsupported::NotSkinny));
    }
    if !incompat.no_holes() {
        return Err(Error::Unsupported(Unsupported::NoHoles));
    }
    if incompat.contains(IncompatFlags::MIXED_GROUPS) {
        return Err(Error::Unsupported(Unsupported::MixedGroups));
    }
    if incompat.contains(IncompatFlags::RAID56) || incompat.contains(IncompatFlags::RAID1C34) {
        return Err(Error::Unsupported(Unsupported::Profile));
    }
    let compat_ro = sb.compat_ro_flags();
    let fst = FREE_SPACE_TREE | FREE_SPACE_TREE_VALID;
    if compat_ro & fst != fst {
        return Err(Error::Unsupported(Unsupported::NoFreeSpaceTree));
    }
    let other = compat_ro & !(fst | VERITY);
    if other != 0 {
        return Err(Error::Unsupported(Unsupported::CompatRo(other)));
    }
    Ok(())
}

/// The device of a volume being reloaded in place, lent to the open that
/// rereads it.
struct Borrowed<'a, D>(&'a mut D);

impl<D: WriteDevice> Device for Borrowed<'_, D> {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        kind: ReadKind,
    ) -> core::result::Result<(), BtrfsError> {
        self.0.read_at(physical, buf, kind)
    }
}

impl<D: WriteDevice> WriteDevice for Borrowed<'_, D> {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> Result<()> {
        self.0.write_at(physical, data)
    }

    fn flush(&mut self) -> Result<()> {
        self.0.flush()
    }

    fn write_durable(&mut self, physical: u64, data: &[u8]) -> Result<()> {
        self.0.write_durable(physical, data)
    }
}
