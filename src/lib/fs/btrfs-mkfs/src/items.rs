//! The items of a new volume, encoded field by field in on-disk order.
//!
//! Every structure here is written front to back by [`Bytes`], so a field's
//! offset is the sum of the fields before it, as in the kernel's `ctree.h`;
//! the sizes each encoder must come to are checked by the tests against what
//! `ferrix-btrfs` parses.

use alloc::vec::Vec;

use ferrix_btrfs::tree::BtrfsKey;

/// Size of a superblock backup root.
const BACKUP_ROOT_SIZE: usize = 168;

/// `S_IFDIR | 0755`.
const DIR_MODE: u32 = 0o040_755;
/// A directory entry's type byte for a directory: `BTRFS_FT_DIR`.
const FT_DIR: u8 = 2;
/// Every chunk's stripe length and I/O alignment, as `mkfs.btrfs` writes them.
const STRIPE_LEN: u64 = ferrix_btrfs::chunk::STRIPE_LEN;
/// A chunk item's owner: the extent tree.
const CHUNK_OWNER: u64 = ferrix_btrfs::items::EXTENT_TREE_OBJECTID;
/// The tree the device extents name as their chunk's.
const CHUNK_TREE: u64 = ferrix_btrfs::items::CHUNK_TREE_OBJECTID;
/// The object id of every chunk item.
const FIRST_CHUNK_TREE: u64 = ferrix_btrfs::chunk::FIRST_CHUNK_TREE_OBJECTID;
/// Extent item flag: the extent is a tree block.
const TREE_BLOCK: u64 = ferrix_btrfs_write::extent::EXTENT_FLAG_TREE_BLOCK;
/// Inline reference type: a tree block owned by a tree.
const TREE_BLOCK_REF: u8 = ferrix_btrfs_write::extent::TREE_BLOCK_REF_KEY;

/// A structure being written front to back.
pub(crate) struct Bytes(Vec<u8>);

impl Bytes {
    pub(crate) const fn new() -> Self {
        Bytes(Vec::new())
    }

    pub(crate) fn u8(mut self, value: u8) -> Self {
        self.0.push(value);
        self
    }

    pub(crate) fn u16(self, value: u16) -> Self {
        self.raw(&value.to_le_bytes())
    }

    pub(crate) fn u32(self, value: u32) -> Self {
        self.raw(&value.to_le_bytes())
    }

    pub(crate) fn u64(self, value: u64) -> Self {
        self.raw(&value.to_le_bytes())
    }

    pub(crate) fn raw(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    pub(crate) fn key(self, key: &BtrfsKey) -> Self {
        self.u64(key.objectid).u8(key.item_type).u64(key.offset)
    }

    /// A `btrfs_timespec`: seconds and nanoseconds.
    pub(crate) fn time(self, seconds: u64) -> Self {
        self.u64(seconds).u32(0)
    }

    /// Zeros up to offset `at`; `None` if the structure is already past it.
    pub(crate) fn pad_to(mut self, at: usize) -> Option<Self> {
        let _ = at.checked_sub(self.0.len())?;
        self.0.resize(at, 0);
        Some(self)
    }

    pub(crate) fn done(self) -> Vec<u8> {
        self.0
    }
}

/// A directory's inode item, as `mkfs.btrfs` writes every tree's root
/// directory: one link, no size, a node's worth of bytes, times `now`.
pub(crate) fn dir_inode(generation: u64, nodesize: u32, now: u64) -> Vec<u8> {
    dir_inode_bytes(generation, 0, 0, nodesize, now).done()
}

fn dir_inode_bytes(generation: u64, transid: u64, size: u64, nodesize: u32, now: u64) -> Bytes {
    Bytes::new()
        .u64(generation)
        .u64(transid)
        .u64(size)
        .u64(u64::from(nodesize))
        .u64(0) // block group
        .u32(1) // nlink
        .u32(0) // uid
        .u32(0) // gid
        .u32(DIR_MODE)
        .u64(0) // rdev
        .u64(0) // flags
        .u64(0) // sequence
        .raw(&[0; 32]) // reserved
        .time(now) // atime
        .time(now) // ctime
        .time(now) // mtime
        .time(now) // otime
}

/// An inode reference: the entry `name` with `index` in directory `key.offset`.
pub(crate) fn inode_ref(index: u64, name: &[u8]) -> Vec<u8> {
    let len = u16::try_from(name.len()).unwrap_or(u16::MAX);
    Bytes::new().u64(index).u16(len).raw(name).done()
}

/// A directory item naming the subvolume tree `tree` as `name`.
pub(crate) fn dir_item_for_tree(tree: u64, transid: u64, name: &[u8]) -> Vec<u8> {
    let location = BtrfsKey::new(tree, ferrix_btrfs::items::ROOT_ITEM_KEY, u64::MAX);
    let len = u16::try_from(name.len()).unwrap_or(u16::MAX);
    Bytes::new()
        .key(&location)
        .u64(transid)
        .u16(0) // data_len
        .u16(len)
        .u8(FT_DIR)
        .raw(name)
        .done()
}

/// What a root item says about the tree it names.
pub(crate) struct RootFields {
    pub(crate) bytenr: u64,
    pub(crate) generation: u64,
    pub(crate) nodesize: u32,
    /// The root directory's inode: 256 in a subvolume tree, 0 elsewhere.
    pub(crate) root_dirid: u64,
    /// A subvolume's UUID; zeros elsewhere.
    pub(crate) uuid: [u8; 16],
    /// A subvolume's creation time; 0 elsewhere.
    pub(crate) time: u64,
}

/// A root item, with the embedded inode `mkfs.btrfs` gives every root.
pub(crate) fn root_item(root: &RootFields) -> Vec<u8> {
    dir_inode_bytes(1, 0, 3, root.nodesize, 0)
        .u64(root.generation)
        .u64(root.root_dirid)
        .u64(root.bytenr)
        .u64(0) // byte_limit
        .u64(u64::from(root.nodesize)) // bytes_used
        .u64(0) // last_snapshot
        .u64(0) // flags
        .u32(1) // refs
        .key(&BtrfsKey::new(0, 0, 0)) // drop_progress
        .u8(0) // drop_level
        .u8(0) // level
        .u64(root.generation) // generation_v2
        .raw(&root.uuid)
        .raw(&[0; 16]) // parent_uuid
        .raw(&[0; 16]) // received_uuid
        .u64(0) // ctransid
        .u64(0) // otransid
        .u64(0) // stransid
        .u64(0) // rtransid
        .time(root.time) // ctime
        .time(root.time) // otime
        .time(0) // stime
        .time(0) // rtime
        .raw(&[0; 64]) // reserved
        .done()
}

/// The one device's item, embedded in the superblock and in the chunk tree.
pub(crate) struct DevFields {
    pub(crate) total_bytes: u64,
    pub(crate) bytes_used: u64,
    pub(crate) sectorsize: u32,
    pub(crate) uuid: [u8; 16],
    pub(crate) fsid: [u8; 16],
}

pub(crate) fn dev_item(dev: &DevFields) -> Vec<u8> {
    Bytes::new()
        .u64(crate::DEVID)
        .u64(dev.total_bytes)
        .u64(dev.bytes_used)
        .u32(dev.sectorsize) // io_align
        .u32(dev.sectorsize) // io_width
        .u32(dev.sectorsize) // sector_size
        .u64(0) // type
        .u64(0) // generation
        .u64(0) // start_offset
        .u32(0) // dev_group
        .u8(0) // seek_speed
        .u8(0) // bandwidth
        .raw(&dev.uuid)
        .raw(&dev.fsid)
        .done()
}

/// A chunk item: its length, type and every stripe's device offset.
pub(crate) fn chunk_item(chunk: &crate::Chunk, sectorsize: u32, dev_uuid: &[u8; 16]) -> Vec<u8> {
    let stripes = u16::try_from(chunk.stripes.len()).unwrap_or(u16::MAX);
    let align = u32::try_from(STRIPE_LEN).unwrap_or(u32::MAX);
    let mut out = Bytes::new()
        .u64(chunk.length)
        .u64(CHUNK_OWNER)
        .u64(STRIPE_LEN)
        .u64(chunk.flags)
        .u32(align) // io_align
        .u32(align) // io_width
        .u32(sectorsize)
        .u16(stripes)
        .u16(1); // sub_stripes
    for &offset in &chunk.stripes {
        out = out.u64(crate::DEVID).u64(offset).raw(dev_uuid);
    }
    out.done()
}

/// A device extent: the stripe of the chunk at `logical` this device holds.
pub(crate) fn dev_extent(logical: u64, length: u64, chunk_tree_uuid: &[u8; 16]) -> Vec<u8> {
    Bytes::new()
        .u64(CHUNK_TREE)
        .u64(FIRST_CHUNK_TREE)
        .u64(logical)
        .u64(length)
        .raw(chunk_tree_uuid)
        .done()
}

/// A block group item.
pub(crate) fn block_group(used: u64, flags: u64) -> Vec<u8> {
    Bytes::new()
        .u64(used)
        .u64(FIRST_CHUNK_TREE)
        .u64(flags)
        .done()
}

/// A skinny metadata item for a tree block of `tree`, with its one inline
/// reference.
pub(crate) fn tree_block_extent(generation: u64, tree: u64) -> Vec<u8> {
    Bytes::new()
        .u64(1) // refs
        .u64(generation)
        .u64(TREE_BLOCK)
        .u8(TREE_BLOCK_REF)
        .u64(tree)
        .done()
}

/// A block group's free-space info: how many free extents follow it.
pub(crate) fn free_space_info(extents: u32) -> Vec<u8> {
    Bytes::new().u32(extents).u32(0).done()
}

/// Where one tree's root is, for the superblock's backup slots.
#[derive(Clone, Copy)]
pub(crate) struct BackupRoots {
    pub(crate) root: u64,
    pub(crate) chunk: u64,
    pub(crate) extent: u64,
    pub(crate) fs: u64,
    pub(crate) dev: u64,
    pub(crate) csum: u64,
    pub(crate) total_bytes: u64,
    pub(crate) bytes_used: u64,
    pub(crate) generation: u64,
}

/// One backup slot naming every root at `generation`, all of them leaves.
pub(crate) fn backup_root(roots: &BackupRoots) -> Option<Vec<u8>> {
    let generation = roots.generation;
    let out = Bytes::new()
        .u64(roots.root)
        .u64(generation)
        .u64(roots.chunk)
        .u64(generation)
        .u64(roots.extent)
        .u64(generation)
        .u64(roots.fs)
        .u64(generation)
        .u64(roots.dev)
        .u64(generation)
        .u64(roots.csum)
        .u64(generation)
        .u64(roots.total_bytes)
        .u64(roots.bytes_used)
        .u64(1) // num_devices
        .pad_to(BACKUP_ROOT_SIZE)?;
    Some(out.done())
}
