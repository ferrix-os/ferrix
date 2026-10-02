//! The commit: settle the bookkeeping trees, write every new node, flush, and
//! only then name the new roots in the superblock.
//!
//! # Settling
//!
//! Before anything is written, four kinds of bookkeeping must agree with the
//! transaction's edits, and bringing any one into line can disturb another:
//!
//! 1. delayed refs become extent items ([`WriteVolume::run_head`]);
//! 2. each block group whose usage changed gets its item rewritten;
//! 3. each block group's free-space-tree items are made to say
//!    `free ∪ pinned` (see [`crate::space`]);
//! 4. each tree whose root moved gets its `ROOT_ITEM` rewritten.
//!
//! Every one of them edits a tree, which copies nodes, which queues refs and
//! moves roots. So the four run in a loop until a whole pass finds nothing to
//! do. It converges because a node copied once in a transaction is edited in
//! place from then on: the first pass copies the paths it needs, and later
//! passes only touch nodes the transaction already owns.
//!
//! # Writing
//!
//! Then every node the transaction owns is serialised and written to each of
//! its copies, the device is flushed, and the primary superblock is written
//! with [`WriteDevice::write_durable`], followed by the mirrors. The flush
//! before the superblock is the whole crash-safety argument: without it the
//! device may make the superblock durable before a node it names.

use alloc::vec::Vec;

use ferrix_btrfs::items::{
    CHUNK_TREE_OBJECTID, CSUM_TREE_OBJECTID, DEV_TREE_OBJECTID, EXTENT_TREE_OBJECTID,
    FS_TREE_OBJECTID, ROOT_ITEM_KEY, ROOT_TREE_OBJECTID,
};
use ferrix_btrfs::superblock::{PRIMARY_OFFSET, SUPERBLOCK_OFFSETS, SUPERBLOCK_SIZE};
use ferrix_btrfs::tree::{BtrfsKey, HEADER_SIZE};

use crate::bytes::{get_u64, put_u8, put_u32, put_u64};
use crate::extent::{
    BLOCK_FLAG_FULL_BACKREF, BLOCK_GROUP_ITEM_KEY, Backref, EXTENT_FLAG_DATA,
    EXTENT_FLAG_TREE_BLOCK, EXTENT_ITEM_KEY, ExtentRecord, FREE_SPACE_BITMAP_KEY,
    FREE_SPACE_EXTENT_KEY, FREE_SPACE_INFO_KEY, METADATA_ITEM_KEY,
};
use crate::ranges::RangeSet;
use crate::refs::Head;

use crate::volume::TreeId;
use crate::{Error, Result, Unsupported, WriteDevice, WriteVolume, fallible};

/// The free-space tree's id.
pub(crate) const FREE_SPACE_TREE_OBJECTID: u64 = 10;
/// The object id every block group item's `chunk_objectid` holds.
const FIRST_CHUNK_TREE_OBJECTID: u64 = 256;
/// Passes of the settling loop after which it is declared not converging.
const SETTLE_PASSES: usize = 64;

/// Superblock field offsets the commit writes; see `btrfs_super_block`.
mod sb {
    pub(super) const BYTENR: usize = 48;
    pub(super) const GENERATION: usize = 72;
    pub(super) const ROOT: usize = 80;
    pub(super) const CHUNK_ROOT: usize = 88;
    pub(super) const LOG_ROOT: usize = 96;
    pub(super) const LOG_ROOT_TRANSID: usize = 104;
    pub(super) const BYTES_USED: usize = 120;
    pub(super) const SYS_CHUNK_ARRAY_SIZE: usize = 160;
    pub(super) const CHUNK_ROOT_GENERATION: usize = 164;
    pub(super) const ROOT_LEVEL: usize = 198;
    pub(super) const CHUNK_ROOT_LEVEL: usize = 199;
    pub(super) const LOG_ROOT_LEVEL: usize = 200;
    pub(super) const DEV_ITEM_BYTES_USED: usize = 201 + 16;
    pub(super) const SYS_CHUNK_ARRAY: usize = 811;
    pub(super) const BACKUP_ROOTS: usize = 2859;
    pub(super) const BACKUP_SIZE: usize = 168;
    pub(super) const BACKUPS: usize = 4;
}

impl<D: WriteDevice> WriteVolume<D> {
    /// Make the transaction durable.
    ///
    /// On success the volume on disk is the transaction's, and a new one is
    /// open. On failure nothing the last commit wrote has been touched — the
    /// old superblock still names the old trees — and the transaction is
    /// aborted; [`WriteVolume::abort`] rereads the committed state.
    ///
    /// Running out of memory anywhere in it aborts the transaction; none of
    /// it can happen after the primary superblock is written, because
    /// everything the commit needs past that point -- the superblock it
    /// keeps, the space it gives back -- is made before. (A mirror's write
    /// can still fail after it, as a device error.)
    pub fn commit(&mut self) -> Result<()> {
        self.guarded_with(None, |volume| {
            if !volume.is_dirty() && !volume.chunks_changed && !volume.has_log() {
                return Ok(());
            }
            // A commit puts everything a log held into the trees themselves,
            // so the log goes with it and the superblock names none.
            volume.drop_log()?;
            // The root tree's root must be this transaction's: the superblock
            // records one generation for both.
            let _ = volume.cow_root(ROOT_TREE_OBJECTID)?;
            volume.settle()?;
            volume.write_nodes()?;
            volume.device.flush()?;
            // The last allocations of the commit: what unpinning leaves, and
            // the superblocks. Past the primary's write nothing allocates.
            let unpinned = volume.space.prepare_unpin()?;
            let primary = volume.write_superblocks()?;
            volume.finish_commit(primary, unpinned);
            Ok(())
        })
    }

    /// Run the bookkeeping loop until a pass changes nothing.
    fn settle(&mut self) -> Result<()> {
        for _ in 0..SETTLE_PASSES {
            let mut changed = false;
            while let Some((bytenr, head)) = self.refs.pop() {
                self.run_head(bytenr, &head)?;
                changed = true;
            }
            changed |= self.write_block_group_items()?;
            changed |= self.sync_free_space_tree()?;
            changed |= self.write_root_items()?;
            if !changed && self.refs.is_empty() {
                return Ok(());
            }
        }
        Err(Error::Inconsistent("commit bookkeeping did not converge"))
    }

    /// Every extent-tree item under the extent at `bytenr`: its extent item
    /// and any keyed references.
    pub(crate) fn extent_items(&mut self, bytenr: u64) -> Result<Vec<(BtrfsKey, Vec<u8>)>> {
        let from = BtrfsKey::new(bytenr, EXTENT_ITEM_KEY, 0);
        let to = BtrfsKey::new(bytenr, crate::extent::SHARED_DATA_REF_KEY, u64::MAX);
        self.range(EXTENT_TREE_OBJECTID, &from, &to)
    }

    /// Apply one head's reference changes to the extent tree.
    pub(crate) fn run_head(&mut self, bytenr: u64, head: &Head) -> Result<()> {
        let flags = if head.level.is_some() {
            EXTENT_FLAG_TREE_BLOCK
        } else {
            EXTENT_FLAG_DATA
        };
        let existing = self.extent_items(bytenr)?;
        let had_item = !existing.is_empty();
        let mut record = ExtentRecord::new(self.transid, flags);
        if let Some((main, rest)) = existing.split_first() {
            let (parsed, stored) = ExtentRecord::parse(&main.0, &main.1)?;
            record = parsed;
            for (key, data) in rest {
                record.add_keyed(key, data)?;
            }
            record.check_total(stored)?;
            check_main_key(&main.0, head, record.flags)?;
        } else if !self.space.allocated_now(bytenr, head.num_bytes) {
            if head.deltas.is_empty() {
                return Ok(());
            }
            return Err(Error::Inconsistent("reference to an extent with no item"));
        }
        if record.is_tree_block()
            && (record.flags & BLOCK_FLAG_FULL_BACKREF != 0
                || record
                    .refs
                    .keys()
                    .any(|r| matches!(r, Backref::SharedBlock { .. })))
        {
            return Err(Error::Unsupported(Unsupported::SharedBlock));
        }
        for (backref, delta) in &head.deltas {
            record.apply(*backref, *delta)?;
        }
        // Made before anything is deleted, so running out of memory for
        // them changes nothing.
        let key = match head.level {
            Some(level) => BtrfsKey::new(bytenr, METADATA_ITEM_KEY, u64::from(level)),
            None => BtrfsKey::new(bytenr, EXTENT_ITEM_KEY, head.num_bytes),
        };
        let items = if record.total() == 0 {
            Vec::new()
        } else {
            record.to_items(&key, self.max_inline_extent())?
        };
        for (key, _) in &existing {
            self.delete(EXTENT_TREE_OBJECTID, key)?;
        }
        if record.total() == 0 {
            if had_item {
                self.space.account(bytenr, head.num_bytes, false)?;
            }
            // Data written and dropped within one transaction had its sums
            // inserted too, so they go whether or not an item ever existed.
            if head.level.is_none() {
                self.delete_csums(bytenr, head.num_bytes)?;
            }
            return self.space.release(bytenr, head.num_bytes);
        }
        for (item_key, data) in items {
            // FALLIBLE: the tree's own insert, which reports running out of memory.
            self.insert(EXTENT_TREE_OBJECTID, item_key, data)?;
        }
        if !had_item {
            self.space.account(bytenr, head.num_bytes, true)?;
        }
        Ok(())
    }

    /// Linux's `BTRFS_MAX_EXTENT_ITEM_SIZE`: a sixteenth of a leaf.
    fn max_inline_extent(&self) -> usize {
        (self.nodesize() as usize).saturating_sub(HEADER_SIZE) >> 4
    }

    /// Rewrite the item of every block group whose usage changed. Whether
    /// anything was written.
    fn write_block_group_items(&mut self) -> Result<bool> {
        let dirty: Vec<(u64, u64, u64, u64)> = fallible::collect(
            self.space
                .groups()
                .filter(|group| group.item_dirty)
                .map(|group| (group.start, group.length, group.used, group.flags)),
        )?;
        for group in self.space.groups_mut() {
            group.item_dirty = false;
        }
        for &(start, length, used, flags) in &dirty {
            let mut data = fallible::zeroed(24)?;
            put_u64(&mut data, 0, used).ok_or(Error::ItemTooLarge)?;
            put_u64(&mut data, 8, FIRST_CHUNK_TREE_OBJECTID).ok_or(Error::ItemTooLarge)?;
            put_u64(&mut data, 16, flags).ok_or(Error::ItemTooLarge)?;
            let key = BtrfsKey::new(start, BLOCK_GROUP_ITEM_KEY, length);
            self.update(EXTENT_TREE_OBJECTID, key, data)?;
        }
        Ok(!dirty.is_empty())
    }

    /// Make the free-space tree say what each block group will have free once
    /// this transaction commits. Whether anything was written.
    fn sync_free_space_tree(&mut self) -> Result<bool> {
        let mut work: Vec<(u64, u64, RangeSet, RangeSet, bool)> = Vec::new();
        for group in self.space.groups() {
            let new = group.committed_free()?;
            if group.on_disk != new || group.bitmaps {
                let old = group.on_disk.try_clone()?;
                fallible::push(
                    &mut work,
                    (group.start, group.length, old, new, group.bitmaps),
                )?;
            }
        }
        let changed = !work.is_empty();
        for (start, length, old, new, bitmaps) in work {
            self.write_group_free_space(start, length, &old, &new, bitmaps)?;
            if let Some(group) = self.space.groups_mut().find(|group| group.start == start) {
                group.on_disk = new;
                group.bitmaps = false;
            }
        }
        Ok(changed)
    }

    /// Replace one block group's free-space items with `new`'s runs.
    ///
    /// Extent items are changed by difference: runs only in `old` go, runs
    /// only in `new` come. A group kept as bitmaps is converted to extents on
    /// its first change, which btrfs reads as well, rather than kept in a
    /// format this writer would have to edit bit by bit.
    fn write_group_free_space(
        &mut self,
        start: u64,
        length: u64,
        old: &RangeSet,
        new: &RangeSet,
        bitmaps: bool,
    ) -> Result<()> {
        let tree = FREE_SPACE_TREE_OBJECTID;
        let end = start.saturating_add(length);
        if bitmaps {
            let from = BtrfsKey::new(start, FREE_SPACE_EXTENT_KEY, 0);
            let to = BtrfsKey::new(end.saturating_sub(1), FREE_SPACE_BITMAP_KEY, u64::MAX);
            for (key, _) in self.range(tree, &from, &to)? {
                self.delete(tree, &key)?;
            }
            for (run, len) in new.iter() {
                // FALLIBLE: the tree's own insert.
                self.insert(
                    tree,
                    BtrfsKey::new(run, FREE_SPACE_EXTENT_KEY, len),
                    Vec::new(),
                )?;
            }
        } else {
            for (run, len) in runs_only_in(old, new) {
                self.delete(tree, &BtrfsKey::new(run, FREE_SPACE_EXTENT_KEY, len))?;
            }
            for (run, len) in runs_only_in(new, old) {
                // FALLIBLE: the tree's own insert.
                self.insert(
                    tree,
                    BtrfsKey::new(run, FREE_SPACE_EXTENT_KEY, len),
                    Vec::new(),
                )?;
            }
        }
        let count = u32::try_from(new.runs()).map_err(|_| Error::ItemTooLarge)?;
        let mut info = fallible::zeroed(8)?;
        put_u32(&mut info, 0, count).ok_or(Error::ItemTooLarge)?;
        put_u32(&mut info, 4, 0).ok_or(Error::ItemTooLarge)?;
        self.update(
            tree,
            BtrfsKey::new(start, FREE_SPACE_INFO_KEY, length),
            info,
        )
    }

    /// Point each moved tree's `ROOT_ITEM` at its new root. Whether anything
    /// was written.
    fn write_root_items(&mut self) -> Result<bool> {
        let stale: Vec<TreeId> = fallible::collect(
            self.stale_roots
                .iter()
                .copied()
                .filter(|&tree| tree != ROOT_TREE_OBJECTID && tree != CHUNK_TREE_OBJECTID),
        )?;
        for &tree in &stale {
            let _ = self.stale_roots.remove(&tree);
            let root = self.root(tree)?;
            let key = BtrfsKey::new(tree, ROOT_ITEM_KEY, 0);
            let mut item = self
                .get(ROOT_TREE_OBJECTID, &key)?
                .ok_or(Error::Inconsistent("tree without a root item"))?;
            let bad = Error::Inconsistent("root item too short");
            put_u64(&mut item, 160, root.generation).ok_or(bad)?;
            put_u64(&mut item, 176, root.bytenr).ok_or(bad)?;
            put_u8(&mut item, 238, root.level).ok_or(bad)?;
            // `generation_v2` equal to `generation` is how Linux tells an item
            // written by a kernel that knows the fields after it.
            if item.len() >= 247 {
                put_u64(&mut item, 239, root.generation).ok_or(bad)?;
            }
            self.update(ROOT_TREE_OBJECTID, key, item)?;
        }
        Ok(!stale.is_empty())
    }

    /// Serialise every node the transaction owns and write each copy.
    pub(crate) fn write_nodes(&mut self) -> Result<()> {
        let size = self.nodesize() as usize;
        let mut buf = fallible::zeroed(size)?;
        let addresses: Vec<u64> = fallible::collect(self.dirty.keys().copied())?;
        for logical in addresses {
            let node = self.node(logical)?;
            node.write(
                &mut buf,
                &self.geometry.fsid,
                &self.geometry.chunk_tree_uuid,
            )
            .ok_or(Error::Inconsistent("node does not fit its block"))?;
            for physical in self.chunks.copies(logical, u64::from(self.nodesize()))? {
                self.device.write_at(physical, &buf)?;
            }
        }
        Ok(())
    }

    /// The new superblock, for the copy at `offset`.
    fn build_superblock(&self, offset: u64) -> Result<Vec<u8>> {
        let bad = Error::Inconsistent("superblock field out of range");
        let mut out = fallible::copy(&self.superblock)?;
        let root = self.root(ROOT_TREE_OBJECTID)?;
        let chunk = self.root(CHUNK_TREE_OBJECTID)?;
        put_u64(&mut out, sb::BYTENR, offset).ok_or(bad)?;
        put_u64(&mut out, sb::GENERATION, self.transid).ok_or(bad)?;
        put_u64(&mut out, sb::ROOT, root.bytenr).ok_or(bad)?;
        put_u8(&mut out, sb::ROOT_LEVEL, root.level).ok_or(bad)?;
        put_u64(&mut out, sb::CHUNK_ROOT, chunk.bytenr).ok_or(bad)?;
        put_u8(&mut out, sb::CHUNK_ROOT_LEVEL, chunk.level).ok_or(bad)?;
        put_u64(&mut out, sb::CHUNK_ROOT_GENERATION, chunk.generation).ok_or(bad)?;
        put_u64(&mut out, sb::LOG_ROOT, 0).ok_or(bad)?;
        put_u64(&mut out, sb::LOG_ROOT_TRANSID, 0).ok_or(bad)?;
        put_u8(&mut out, sb::LOG_ROOT_LEVEL, 0).ok_or(bad)?;
        put_u64(&mut out, sb::BYTES_USED, self.space.used()).ok_or(bad)?;
        put_u64(&mut out, sb::DEV_ITEM_BYTES_USED, self.device_bytes_used()).ok_or(bad)?;
        if self.chunks_changed {
            let array = self.system_chunk_array()?;
            let len = u32::try_from(array.len()).map_err(|_| bad)?;
            put_u32(&mut out, sb::SYS_CHUNK_ARRAY_SIZE, len).ok_or(bad)?;
            let area = out
                .get_mut(sb::SYS_CHUNK_ARRAY..sb::SYS_CHUNK_ARRAY + 2048)
                .ok_or(bad)?;
            area.fill(0);
            crate::bytes::put(area, 0, &array).ok_or(bad)?;
        }
        self.write_backup_root(&mut out)?;
        sealed(out)
    }

    /// Record this commit's roots in the next of the superblock's four backup
    /// slots, as Linux's `backup_super_roots`: the slot after the one naming
    /// the last commit.
    fn write_backup_root(&self, out: &mut [u8]) -> Result<()> {
        let bad = Error::Inconsistent("backup root out of range");
        let slot_at = |slot: usize| sb::BACKUP_ROOTS + slot * sb::BACKUP_SIZE;
        let last =
            (0..sb::BACKUPS).find(|&slot| get_u64(out, slot_at(slot) + 8) == Some(self.committed));
        let slot = last.map_or(0, |slot| (slot + 1) % sb::BACKUPS);
        let at = slot_at(slot);
        let backup = out.get_mut(at..at + sb::BACKUP_SIZE).ok_or(bad)?;
        backup.fill(0);
        let trees = [
            (0, 144, ROOT_TREE_OBJECTID),
            (16, 145, CHUNK_TREE_OBJECTID),
            (32, 146, EXTENT_TREE_OBJECTID),
            (48, 147, FS_TREE_OBJECTID),
            (64, 148, DEV_TREE_OBJECTID),
            (80, 149, CSUM_TREE_OBJECTID),
        ];
        for (field, level_field, tree) in trees {
            let root = self.root(tree)?;
            put_u64(backup, field, root.bytenr).ok_or(bad)?;
            put_u64(backup, field + 8, root.generation).ok_or(bad)?;
            put_u8(backup, level_field, root.level).ok_or(bad)?;
        }
        let total = get_u64(&self.superblock, 112).ok_or(bad)?;
        put_u64(backup, 96, total).ok_or(bad)?;
        put_u64(backup, 104, self.space.used()).ok_or(bad)?;
        put_u64(backup, 112, 1).ok_or(bad)?;
        Ok(())
    }

    /// Write the primary superblock durably, then every mirror the device is
    /// large enough to hold. Returns the primary, which is the committed
    /// superblock from now on: kept rather than built again, so nothing
    /// after the commit point needs memory for it.
    fn write_superblocks(&mut self) -> Result<Vec<u8>> {
        let size = SUPERBLOCK_SIZE as u64;
        let mut primary = None;
        let mirrors = SUPERBLOCK_OFFSETS.iter().skip(1);
        // Every mirror is built before the primary is written, so the last
        // allocation of the commit comes before its commit point.
        let mut built = Vec::new();
        for &offset in mirrors {
            if offset.saturating_add(size) <= self.geometry.device_size {
                fallible::push(&mut built, (offset, self.build_superblock(offset)?))?;
            }
        }
        if PRIMARY_OFFSET.saturating_add(size) <= self.geometry.device_size {
            let block = self.build_superblock(PRIMARY_OFFSET)?;
            self.device.write_durable(PRIMARY_OFFSET, &block)?;
            primary = Some(block);
        }
        for (offset, block) in built {
            self.device.write_at(offset, &block)?;
        }
        primary.ok_or(Error::Inconsistent("device too small for a superblock"))
    }

    /// The transaction is on disk: its nodes are committed ones now, pinned
    /// space is free, and the next transaction starts. `primary` is the
    /// superblock the commit wrote, and `unpinned` the free space
    /// [`crate::space::Space::prepare_unpin`] made before it. Allocates
    /// nothing that can fail the commit: a node the clean cache has no room
    /// for is read again when next wanted.
    fn finish_commit(&mut self, primary: Vec<u8>, unpinned: crate::space::Unpinned) {
        self.superblock = primary;
        self.committed = self.transid;
        self.transid = self.transid.saturating_add(1);
        let written = core::mem::take(&mut self.dirty);
        if self.clean.len().saturating_add(written.len()) > 4096 {
            self.clean = written;
        } else {
            for (logical, node) in written {
                if fallible::insert(&mut self.clean, logical, node).is_err() {
                    break;
                }
            }
        }
        self.space.unpin(unpinned);
        self.chunks_changed = false;
    }

    /// Delete the checksums of the data extent at `[bytenr, bytenr + len)`,
    /// which is being freed. Items that straddle an end are cut, not dropped.
    pub(crate) fn delete_csums(&mut self, bytenr: u64, len: u64) -> Result<()> {
        crate::csum::delete_range(self, bytenr, len)
    }

    /// The device's `bytes_used`: every chunk's stripes.
    pub(crate) fn device_bytes_used(&self) -> u64 {
        self.chunks
            .iter()
            .map(|chunk| chunk.length.saturating_mul(chunk.stripes.len() as u64))
            .fold(0u64, u64::saturating_add)
    }
}

/// Check that an existing extent item is the kind the head expects: a tree
/// block's at its level, or data's with its length.
fn check_main_key(key: &BtrfsKey, head: &Head, flags: u64) -> Result<()> {
    let ok = match head.level {
        Some(level) => {
            flags & EXTENT_FLAG_TREE_BLOCK != 0
                && ((key.item_type == METADATA_ITEM_KEY && key.offset == u64::from(level))
                    || key.item_type == EXTENT_ITEM_KEY)
        }
        None => {
            flags & EXTENT_FLAG_DATA != 0
                && key.item_type == EXTENT_ITEM_KEY
                && key.offset == head.num_bytes
        }
    };
    if ok {
        Ok(())
    } else {
        Err(Error::Inconsistent("extent item of the wrong kind"))
    }
}

/// A superblock with its checksum over what it now holds: the last field
/// written to any copy of it, and what makes the rest of it believed.
pub(crate) fn sealed(mut block: Vec<u8>) -> Result<Vec<u8>> {
    let bad = Error::Inconsistent("superblock field out of range");
    let sum = ferrix_btrfs::crc32c(block.get(ferrix_btrfs::CSUM_SIZE..).ok_or(bad)?);
    block.get_mut(..ferrix_btrfs::CSUM_SIZE).ok_or(bad)?.fill(0);
    put_u32(&mut block, 0, sum).ok_or(bad)?;
    Ok(block)
}

/// The runs of `a` that are not runs of `b`, both in canonical form.
fn runs_only_in<'a>(a: &'a RangeSet, b: &'a RangeSet) -> impl Iterator<Item = (u64, u64)> + 'a {
    a.iter().filter(|&(start, len)| !b.has_run(start, len))
}
