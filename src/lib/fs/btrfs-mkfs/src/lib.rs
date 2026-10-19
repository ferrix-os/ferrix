//! `mkfs.btrfs` for one device: an empty volume, the size of its device.
//!
//! The installer formats the root partition with this (`docs/INSTALLER.md`
//! §5.3, slice I3), where it used to copy a 1 GiB volume `mkfs.btrfs` had
//! made on the build host. What it makes is what `mkfs.btrfs` makes by
//! default for one device, and what `ferrix-btrfs-write` takes:
//!
//! * `SINGLE` data, `DUP` metadata and system chunks;
//! * skinny metadata, `NO_HOLES`, extended inode references, the free-space
//!   tree, CRC-32C;
//! * the trees `mkfs.btrfs` writes: root, chunk, extent, device, the
//!   top-level subvolume, checksums, free space, data relocation and UUIDs,
//!   each a single leaf, item for item as `mkfs.btrfs` fills them (the tests
//!   compare the two).
//!
//! It differs from `mkfs.btrfs` in one way that matters: it makes one chunk
//! of each kind where the device's start is, sized as Linux sizes a first
//! chunk (a tenth of the device, at most 1 GiB of data and 256 MiB of
//! metadata), and leaves the rest of the device unallocated.
//! `ferrix-btrfs-write` makes further chunks as the volume fills, as Linux
//! does, so there is no second pass that grows the volume to its device.
//!
//! [`make`] is pure: it returns the bytes to write and where, and
//! [`Image::write_to`] writes them in the order that keeps a half-written
//! volume from looking like a whole one -- the old start of the device
//! wiped, every tree block, a flush, and only then the superblocks.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use ferrix_btrfs::chunk::{
    BLOCK_GROUP_DATA, BLOCK_GROUP_DUP, BLOCK_GROUP_METADATA, BLOCK_GROUP_SYSTEM,
    FIRST_CHUNK_TREE_OBJECTID,
};
use ferrix_btrfs::crc32c;
use ferrix_btrfs::items::{
    CHUNK_ITEM_KEY, CHUNK_TREE_OBJECTID, CSUM_TREE_OBJECTID, DEV_ITEM_KEY, DEV_ITEMS_OBJECTID,
    DEV_TREE_OBJECTID, DIR_ITEM_KEY, EXTENT_TREE_OBJECTID, FIRST_FREE_OBJECTID, FS_TREE_OBJECTID,
    INODE_ITEM_KEY, INODE_REF_KEY, ROOT_ITEM_KEY, ROOT_TREE_DIR_OBJECTID, ROOT_TREE_OBJECTID,
    name_hash,
};
use ferrix_btrfs::superblock::{IncompatFlags, MAGIC, SUPERBLOCK_OFFSETS, SUPERBLOCK_SIZE};
use ferrix_btrfs::tree::{BtrfsKey, MAX_NODE_SIZE, MIN_NODE_SIZE};
use ferrix_btrfs_write::extent::{
    BLOCK_GROUP_ITEM_KEY, DEV_EXTENT_KEY, FREE_SPACE_EXTENT_KEY, FREE_SPACE_INFO_KEY,
    METADATA_ITEM_KEY,
};
use ferrix_btrfs_write::node::{Body, LeafItem, TreeNode};

use items::{BackupRoots, Bytes, DevFields, RootFields};

mod items;

/// The smallest device this makes a volume on. `mkfs.btrfs` refuses about
/// 114 MiB with its default profiles; this keeps a round margin.
pub const MIN_SIZE: u64 = 128 * MIB;
/// `mkfs.btrfs`'s node size.
pub const DEFAULT_NODESIZE: u32 = 16 * 1024;
/// The sector size: the page size of every machine Ferrix runs on.
pub const SECTORSIZE: u32 = 4096;
/// The longest label, as `mkfs.btrfs -L` takes it.
pub const MAX_LABEL: usize = 255;

const MIB: u64 = 1 << 20;
/// Device space below this holds no chunk: the boot area and the primary
/// superblock. It is what [`Image::zero`] wipes.
const RESERVED: u64 = MIB;
/// The system chunk, as `mkfs.btrfs` makes it.
const SYSTEM_BYTES: u64 = 8 * MIB;
/// The largest first chunks, as `ferrix-btrfs-write` sizes later ones.
const METADATA_MOST: u64 = 256 * MIB;
const DATA_MOST: u64 = 1 << 30;
/// The one device's id.
pub(crate) const DEVID: u64 = 1;
/// The transaction everything is written in.
const GENERATION: u64 = 1;

const UUID_TREE_OBJECTID: u64 = 9;
const FREE_SPACE_TREE_OBJECTID: u64 = 10;
const DATA_RELOC_TREE_OBJECTID: u64 = 0u64.wrapping_sub(9);
/// The device statistics item: `BTRFS_DEV_STATS_OBJECTID`, a persistent item.
const DEV_STATS_OBJECTID: u64 = 0;
const PERSISTENT_ITEM_KEY: u8 = 249;
const DEV_STATS_BYTES: usize = 40;
const UUID_KEY_SUBVOL: u8 = 251;
/// The superblock's `WRITTEN` flag, which `mkfs.btrfs` sets.
const SUPER_FLAG_WRITTEN: u64 = 1;
/// `compat_ro`: the free-space tree exists and is valid.
const FREE_SPACE_TREE_BITS: u64 = 0b11;
/// Superblock offsets of what follows the fixed fields.
const SB_LABEL: usize = 299;
const SB_RESERVED: usize = 595;
const SB_SYS_CHUNK_ARRAY: usize = 811;
const SB_BACKUP_ROOTS: usize = 2859;

/// What the volume is to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options<'a> {
    /// The device's size in bytes; the volume takes all of it, in whole
    /// sectors.
    pub size: u64,
    /// Bytes in a tree node: a power of two from 4 KiB to 64 KiB.
    pub nodesize: u32,
    /// The label, `ferrix-root` for a root volume.
    pub label: &'a [u8],
    /// The filesystem's UUID, which every tree block carries.
    pub fsid: [u8; 16],
    /// The device's UUID.
    pub dev_uuid: [u8; 16],
    /// The chunk tree's UUID, which every tree block and device extent carry.
    pub chunk_tree_uuid: [u8; 16],
    /// The top-level subvolume's UUID.
    pub subvol_uuid: [u8; 16],
    /// Seconds since 1970, the root directories' times.
    pub now: u64,
}

impl<'a> Options<'a> {
    /// A volume of `size` bytes with `mkfs.btrfs`'s node size, its UUIDs
    /// made from `random` as version 4 UUIDs are.
    #[must_use]
    pub fn new(size: u64, label: &'a [u8], random: [[u8; 16]; 4], now: u64) -> Self {
        let [fsid, dev_uuid, chunk_tree_uuid, subvol_uuid] = random.map(uuid_v4);
        Options {
            size,
            nodesize: DEFAULT_NODESIZE,
            label,
            fsid,
            dev_uuid,
            chunk_tree_uuid,
            subvol_uuid,
            now,
        }
    }

    fn check(&self) -> Result<(), Error> {
        let nodesize = self.nodesize;
        if !nodesize.is_power_of_two()
            || !(MIN_NODE_SIZE..=MAX_NODE_SIZE).contains(&nodesize)
            || nodesize < SECTORSIZE
        {
            return Err(Error::NodeSize(nodesize));
        }
        if self.label.len() > MAX_LABEL || self.label.contains(&0) {
            return Err(Error::Label);
        }
        if self.size < MIN_SIZE {
            return Err(Error::TooSmall {
                size: self.size,
                least: MIN_SIZE,
            });
        }
        Ok(())
    }
}

/// Mark 16 random bytes a version 4, variant 1 UUID.
fn uuid_v4(mut bytes: [u8; 16]) -> [u8; 16] {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

/// Why no volume was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The device is smaller than [`MIN_SIZE`].
    TooSmall {
        /// The device's size.
        size: u64,
        /// The least this makes a volume on.
        least: u64,
    },
    /// Not a node size btrfs has.
    NodeSize(u32),
    /// A label longer than [`MAX_LABEL`] or holding a NUL.
    Label,
    /// A structure did not come out the size it must be: a bug here.
    Encode(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::TooSmall { size, least } => write!(
                f,
                "{} MiB is too small for a btrfs volume; it needs {} MiB",
                size >> 20,
                least >> 20
            ),
            Error::NodeSize(size) => write!(f, "{size} is not a btrfs node size"),
            Error::Label => write!(f, "a label is at most {MAX_LABEL} bytes and holds no NUL"),
            Error::Encode(what) => write!(f, "mkfs: {what}"),
        }
    }
}

/// The volume as writes to its device, offsets from the device's start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The volume's size: the device's, in whole sectors.
    pub size: u64,
    /// Bytes to zero first, so that nothing of an old file system at the
    /// device's start survives: the boot area and the primary superblock.
    pub zero: Range<u64>,
    /// Every copy of every tree block.
    pub blocks: Vec<(u64, Vec<u8>)>,
    /// The superblock's copies, the primary first.
    pub superblocks: Vec<(u64, Vec<u8>)>,
}

/// Where [`Image::write_to`] writes: a device, or a partition of one.
pub trait Target {
    /// What a failed write or flush says.
    type Error;
    /// Write `data` at byte `at` of the volume.
    fn write_at(&mut self, at: u64, data: &[u8]) -> Result<(), Self::Error>;
    /// Make every write that has returned durable.
    fn flush(&mut self) -> Result<(), Self::Error>;
}

impl Image {
    /// Write the volume to `target`: [`Image::zero`], the tree blocks, a
    /// flush, the primary superblock, a flush, the mirrors, a flush. Until
    /// the primary is down the device holds no superblock, so a write cut
    /// short leaves no volume rather than half of one.
    pub fn write_to<T: Target>(&self, target: &mut T) -> Result<(), T::Error> {
        let wipe = usize::try_from(self.zero.end.saturating_sub(self.zero.start)).unwrap_or(0);
        target.write_at(self.zero.start, &vec![0_u8; wipe])?;
        for (at, block) in &self.blocks {
            target.write_at(*at, block)?;
        }
        target.flush()?;
        let mut superblocks = self.superblocks.iter();
        if let Some((at, primary)) = superblocks.next() {
            target.write_at(*at, primary)?;
            target.flush()?;
        }
        for (at, mirror) in superblocks {
            target.write_at(*at, mirror)?;
        }
        target.flush()
    }
}

/// One chunk of the new volume. Its logical address is its first stripe's
/// device offset, so no two chunks' logical ranges overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub(crate) logical: u64,
    pub(crate) length: u64,
    pub(crate) flags: u64,
    pub(crate) stripes: Vec<u64>,
}

impl Chunk {
    /// A chunk of `copies` stripes of `length` bytes, from device offset
    /// `at` on, which it moves past them.
    fn place(at: &mut u64, length: u64, flags: u64, copies: u64) -> Chunk {
        let stripes = (0..copies).map(|copy| *at + copy * length).collect();
        let chunk = Chunk {
            logical: *at,
            length,
            flags,
            stripes,
        };
        *at += copies * length;
        chunk
    }

    /// Every device offset the logical address `logical` is stored at.
    fn copies(&self, logical: u64) -> impl Iterator<Item = u64> + '_ {
        let within = logical.saturating_sub(self.logical);
        self.stripes.iter().map(move |stripe| stripe + within)
    }
}

/// A first chunk's size, as Linux sizes one: at most `most`, at most a tenth
/// of the device, in whole MiB.
fn chunk_bytes(size: u64, most: u64) -> u64 {
    let bytes = most.min(size / 10);
    (bytes - bytes % MIB).max(MIB)
}

/// Where everything goes.
struct Layout {
    nodesize: u32,
    size: u64,
    system: Chunk,
    metadata: Chunk,
    data: Chunk,
}

/// The trees in the metadata chunk, in the order their leaves are placed.
const METADATA_TREES: [u64; 8] = [
    ROOT_TREE_OBJECTID,
    EXTENT_TREE_OBJECTID,
    DEV_TREE_OBJECTID,
    FS_TREE_OBJECTID,
    CSUM_TREE_OBJECTID,
    FREE_SPACE_TREE_OBJECTID,
    DATA_RELOC_TREE_OBJECTID,
    UUID_TREE_OBJECTID,
];

impl Layout {
    fn new(size: u64, nodesize: u32) -> Result<Layout, Error> {
        let mut at = RESERVED;
        let system = Chunk::place(
            &mut at,
            SYSTEM_BYTES,
            BLOCK_GROUP_SYSTEM | BLOCK_GROUP_DUP,
            2,
        );
        let metadata = Chunk::place(
            &mut at,
            chunk_bytes(size, METADATA_MOST),
            BLOCK_GROUP_METADATA | BLOCK_GROUP_DUP,
            2,
        );
        let data = Chunk::place(&mut at, chunk_bytes(size, DATA_MOST), BLOCK_GROUP_DATA, 1);
        if at > size {
            return Err(Error::TooSmall {
                size,
                least: MIN_SIZE.max(at),
            });
        }
        Ok(Layout {
            nodesize,
            size,
            system,
            metadata,
            data,
        })
    }

    fn chunks(&self) -> [&Chunk; 3] {
        [&self.system, &self.metadata, &self.data]
    }

    /// The chunk tree's leaf: the system chunk's first node.
    const fn chunk_root(&self) -> u64 {
        self.system.logical
    }

    /// Where `tree`'s leaf is, if it is one in the metadata chunk.
    fn root_of(&self, tree: u64) -> u64 {
        let index = METADATA_TREES.iter().position(|&t| t == tree).unwrap_or(0) as u64;
        self.metadata.logical + index * u64::from(self.nodesize)
    }

    /// Bytes of tree blocks in each chunk.
    fn used(&self, chunk: &Chunk) -> u64 {
        let nodes = if chunk.flags & BLOCK_GROUP_SYSTEM != 0 {
            1
        } else if chunk.flags & BLOCK_GROUP_METADATA != 0 {
            METADATA_TREES.len() as u64
        } else {
            0
        };
        nodes * u64::from(self.nodesize)
    }

    /// Bytes of tree blocks in the volume: the superblock's `bytes_used`.
    fn bytes_used(&self) -> u64 {
        self.chunks().iter().map(|chunk| self.used(chunk)).sum()
    }

    /// Bytes of the device the chunks take: the device item's `bytes_used`.
    fn device_used(&self) -> u64 {
        self.chunks()
            .iter()
            .map(|chunk| chunk.length * chunk.stripes.len() as u64)
            .sum()
    }

    fn dev(&self, options: &Options<'_>) -> DevFields {
        DevFields {
            total_bytes: self.size,
            bytes_used: self.device_used(),
            sectorsize: SECTORSIZE,
            uuid: options.dev_uuid,
            fsid: options.fsid,
        }
    }

    fn root_fields(&self, tree: u64) -> RootFields {
        RootFields {
            bytenr: self.root_of(tree),
            generation: GENERATION,
            nodesize: self.nodesize,
            root_dirid: 0,
            uuid: [0; 16],
            time: 0,
        }
    }
}

/// Make the volume `options` describes.
pub fn make(options: &Options<'_>) -> Result<Image, Error> {
    options.check()?;
    let size = options.size - options.size % u64::from(SECTORSIZE);
    let layout = Layout::new(size, options.nodesize)?;
    let mut blocks = Vec::new();
    let mut buf = vec![0_u8; options.nodesize as usize];
    for (tree, at, items) in leaves(options, &layout) {
        let mut items = items;
        items.sort_by_key(|item| item.key);
        let node = TreeNode {
            bytenr: at,
            generation: GENERATION,
            owner: tree,
            level: 0,
            body: Body::Leaf(items),
        };
        if node.overflows(options.nodesize) {
            return Err(Error::Encode("a tree does not fit one leaf"));
        }
        node.write(&mut buf, &options.fsid, &options.chunk_tree_uuid)
            .ok_or(Error::Encode("a leaf does not encode"))?;
        let chunk = if tree == CHUNK_TREE_OBJECTID {
            &layout.system
        } else {
            &layout.metadata
        };
        for physical in chunk.copies(at) {
            blocks.push((physical, buf.clone()));
        }
    }
    let mut superblocks = Vec::new();
    for offset in SUPERBLOCK_OFFSETS {
        if offset + SUPERBLOCK_SIZE as u64 <= size {
            superblocks.push((offset, superblock(options, &layout, offset)?));
        }
    }
    Ok(Image {
        size,
        zero: 0..RESERVED,
        blocks,
        superblocks,
    })
}

fn item(key: BtrfsKey, data: Vec<u8>) -> LeafItem {
    LeafItem { key, data }
}

/// Every tree's one leaf: its id, where it is, and its items.
fn leaves(options: &Options<'_>, layout: &Layout) -> Vec<(u64, u64, Vec<LeafItem>)> {
    let root = |tree| layout.root_of(tree);
    vec![
        (
            CHUNK_TREE_OBJECTID,
            layout.chunk_root(),
            chunk_tree(options, layout),
        ),
        (
            ROOT_TREE_OBJECTID,
            root(ROOT_TREE_OBJECTID),
            root_tree(options, layout),
        ),
        (
            EXTENT_TREE_OBJECTID,
            root(EXTENT_TREE_OBJECTID),
            extent_tree(layout),
        ),
        (
            DEV_TREE_OBJECTID,
            root(DEV_TREE_OBJECTID),
            dev_tree(options, layout),
        ),
        (
            FS_TREE_OBJECTID,
            root(FS_TREE_OBJECTID),
            subvolume_tree(options),
        ),
        (CSUM_TREE_OBJECTID, root(CSUM_TREE_OBJECTID), Vec::new()),
        (
            FREE_SPACE_TREE_OBJECTID,
            root(FREE_SPACE_TREE_OBJECTID),
            free_space_tree(layout),
        ),
        (
            DATA_RELOC_TREE_OBJECTID,
            root(DATA_RELOC_TREE_OBJECTID),
            subvolume_tree(options),
        ),
        (
            UUID_TREE_OBJECTID,
            root(UUID_TREE_OBJECTID),
            uuid_tree(options),
        ),
    ]
}

/// The device item and every chunk.
fn chunk_tree(options: &Options<'_>, layout: &Layout) -> Vec<LeafItem> {
    let mut items = vec![item(
        BtrfsKey::new(DEV_ITEMS_OBJECTID, DEV_ITEM_KEY, DEVID),
        items::dev_item(&layout.dev(options)),
    )];
    for chunk in layout.chunks() {
        items.push(item(
            BtrfsKey::new(FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY, chunk.logical),
            items::chunk_item(chunk, SECTORSIZE, &options.dev_uuid),
        ));
    }
    items
}

/// A root item for every tree but the root and chunk trees, and the root
/// tree's directory naming the top-level subvolume `default`, as
/// `mkfs.btrfs` writes it.
fn root_tree(options: &Options<'_>, layout: &Layout) -> Vec<LeafItem> {
    let mut items = Vec::new();
    for tree in METADATA_TREES
        .into_iter()
        .filter(|&t| t != ROOT_TREE_OBJECTID)
    {
        let mut fields = layout.root_fields(tree);
        if tree == FS_TREE_OBJECTID || tree == DATA_RELOC_TREE_OBJECTID {
            fields.root_dirid = FIRST_FREE_OBJECTID;
        }
        if tree == FS_TREE_OBJECTID {
            fields.uuid = options.subvol_uuid;
            fields.time = options.now;
        }
        items.push(item(
            BtrfsKey::new(tree, ROOT_ITEM_KEY, 0),
            items::root_item(&fields),
        ));
    }
    let dir = ROOT_TREE_DIR_OBJECTID;
    let name: &[u8] = b"default";
    items.push(item(
        BtrfsKey::new(FS_TREE_OBJECTID, INODE_REF_KEY, dir),
        items::inode_ref(0, name),
    ));
    items.push(item(
        BtrfsKey::new(dir, INODE_ITEM_KEY, 0),
        items::dir_inode(GENERATION, options.nodesize, options.now),
    ));
    items.push(item(
        BtrfsKey::new(dir, INODE_REF_KEY, dir),
        items::inode_ref(0, b".."),
    ));
    items.push(item(
        BtrfsKey::new(dir, DIR_ITEM_KEY, name_hash(name)),
        items::dir_item_for_tree(FS_TREE_OBJECTID, GENERATION, name),
    ));
    items
}

/// A block group item for every chunk, and a metadata item for every tree
/// block.
fn extent_tree(layout: &Layout) -> Vec<LeafItem> {
    let mut items: Vec<LeafItem> = layout
        .chunks()
        .iter()
        .map(|chunk| {
            item(
                BtrfsKey::new(chunk.logical, BLOCK_GROUP_ITEM_KEY, chunk.length),
                items::block_group(layout.used(chunk), chunk.flags),
            )
        })
        .collect();
    let blocks = core::iter::once((layout.chunk_root(), CHUNK_TREE_OBJECTID)).chain(
        METADATA_TREES
            .iter()
            .map(|&tree| (layout.root_of(tree), tree)),
    );
    for (at, tree) in blocks {
        items.push(item(
            BtrfsKey::new(at, METADATA_ITEM_KEY, 0),
            items::tree_block_extent(GENERATION, tree),
        ));
    }
    items
}

/// The device's statistics and a device extent for every stripe.
fn dev_tree(options: &Options<'_>, layout: &Layout) -> Vec<LeafItem> {
    let mut items = vec![item(
        BtrfsKey::new(DEV_STATS_OBJECTID, PERSISTENT_ITEM_KEY, DEVID),
        vec![0; DEV_STATS_BYTES],
    )];
    for chunk in layout.chunks() {
        for &stripe in &chunk.stripes {
            items.push(item(
                BtrfsKey::new(DEVID, DEV_EXTENT_KEY, stripe),
                items::dev_extent(chunk.logical, chunk.length, &options.chunk_tree_uuid),
            ));
        }
    }
    items
}

/// An empty subvolume: its root directory, inode 256, its own parent.
fn subvolume_tree(options: &Options<'_>) -> Vec<LeafItem> {
    let dir = FIRST_FREE_OBJECTID;
    vec![
        item(
            BtrfsKey::new(dir, INODE_ITEM_KEY, 0),
            items::dir_inode(GENERATION, options.nodesize, options.now),
        ),
        item(
            BtrfsKey::new(dir, INODE_REF_KEY, dir),
            items::inode_ref(0, b".."),
        ),
    ]
}

/// Each block group's free space: everything after its tree blocks, which
/// start it. The superblock mirrors are listed free, as Linux lists them.
fn free_space_tree(layout: &Layout) -> Vec<LeafItem> {
    let mut items = Vec::new();
    for chunk in layout.chunks() {
        let used = layout.used(chunk);
        let free = chunk.length.saturating_sub(used);
        let extents = u32::from(free > 0);
        items.push(item(
            BtrfsKey::new(chunk.logical, FREE_SPACE_INFO_KEY, chunk.length),
            items::free_space_info(extents),
        ));
        if free > 0 {
            items.push(item(
                BtrfsKey::new(chunk.logical + used, FREE_SPACE_EXTENT_KEY, free),
                Vec::new(),
            ));
        }
    }
    items
}

/// The top-level subvolume's UUID, as `mkfs.btrfs` records it.
fn uuid_tree(options: &Options<'_>) -> Vec<LeafItem> {
    let (high, low) = options.subvol_uuid.split_at(8);
    let half = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
    vec![item(
        BtrfsKey::new(half(high), UUID_KEY_SUBVOL, half(low)),
        FS_TREE_OBJECTID.to_le_bytes().to_vec(),
    )]
}

/// The superblock's copy at device offset `offset`.
fn superblock(options: &Options<'_>, layout: &Layout, offset: u64) -> Result<Vec<u8>, Error> {
    let bad = Error::Encode("the superblock does not encode");
    let mut incompat = IncompatFlags::MIXED_BACKREF
        | IncompatFlags::EXTENDED_IREF
        | IncompatFlags::SKINNY_METADATA
        | IncompatFlags::NO_HOLES;
    if options.nodesize > SECTORSIZE {
        incompat |= IncompatFlags::BIG_METADATA;
    }
    let system = &layout.system;
    let sys_chunk_array = Bytes::new()
        .key(&BtrfsKey::new(
            FIRST_CHUNK_TREE_OBJECTID,
            CHUNK_ITEM_KEY,
            system.logical,
        ))
        .raw(&items::chunk_item(system, SECTORSIZE, &options.dev_uuid))
        .done();
    let array_len = u32::try_from(sys_chunk_array.len()).map_err(|_| bad)?;
    let roots = BackupRoots {
        root: layout.root_of(ROOT_TREE_OBJECTID),
        chunk: layout.chunk_root(),
        extent: layout.root_of(EXTENT_TREE_OBJECTID),
        fs: layout.root_of(FS_TREE_OBJECTID),
        dev: layout.root_of(DEV_TREE_OBJECTID),
        csum: layout.root_of(CSUM_TREE_OBJECTID),
        total_bytes: layout.size,
        bytes_used: layout.bytes_used(),
        generation: GENERATION,
    };
    let out = Bytes::new()
        .raw(&[0; ferrix_btrfs::CSUM_SIZE])
        .raw(&options.fsid)
        .u64(offset)
        .u64(SUPER_FLAG_WRITTEN)
        .raw(&MAGIC)
        .u64(GENERATION)
        .u64(roots.root)
        .u64(roots.chunk)
        .u64(0) // log_root
        .u64(0) // log_root_transid
        .u64(layout.size)
        .u64(roots.bytes_used)
        .u64(ROOT_TREE_DIR_OBJECTID)
        .u64(1) // num_devices
        .u32(SECTORSIZE)
        .u32(options.nodesize)
        .u32(options.nodesize) // leafsize
        .u32(SECTORSIZE) // stripesize
        .u32(array_len)
        .u64(GENERATION) // chunk_root_generation
        .u64(0) // compat_flags
        .u64(FREE_SPACE_TREE_BITS)
        .u64(incompat)
        .u16(0) // csum_type: CRC-32C
        .u8(0) // root_level
        .u8(0) // chunk_root_level
        .u8(0) // log_root_level
        .raw(&items::dev_item(&layout.dev(options)))
        .pad_to(SB_LABEL)
        .ok_or(bad)?
        .raw(options.label)
        .pad_to(SB_LABEL + 256)
        .ok_or(bad)?
        .u64(0) // cache_generation: no v1 space cache
        .u64(0) // uuid_tree_generation: Linux checks the tree at mount
        .pad_to(SB_RESERVED)
        .ok_or(bad)?
        .pad_to(SB_SYS_CHUNK_ARRAY)
        .ok_or(bad)?
        .raw(&sys_chunk_array)
        .pad_to(SB_BACKUP_ROOTS)
        .ok_or(bad)?
        .raw(&items::backup_root(&roots).ok_or(bad)?)
        .pad_to(SUPERBLOCK_SIZE)
        .ok_or(bad)?;
    let mut out = out.done();
    if out.len() != SUPERBLOCK_SIZE {
        return Err(bad);
    }
    let sum = crc32c(out.get(ferrix_btrfs::CSUM_SIZE..).ok_or(bad)?);
    out.get_mut(..4)
        .ok_or(bad)?
        .copy_from_slice(&sum.to_le_bytes());
    Ok(out)
}

#[cfg(test)]
mod tests;
