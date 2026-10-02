//! Tests of the write path against volumes real `mkfs.btrfs` made.
//!
//! Two oracles. The first is `ferrix-btrfs`, the reader stage 11 checked
//! against btrfs itself: whatever this crate writes must open and read back
//! there. The second is [`check`], which recomputes from the trees everything
//! a commit must have kept consistent — each tree block's extent item and
//! owner, each data extent's references, each block group's usage and free
//! space, the superblock's total — and compares. Host `btrfs check` is the
//! third, run over images the ignored test [`btrfs_check_images`] writes.

extern crate std;

use std::boxed::Box;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::vec;
use std::vec::Vec;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::chunk::ChunkMapEntry;
use ferrix_btrfs::items::{EXTENT_DATA_KEY, EXTENT_TREE_OBJECTID, ExtentData, FS_TREE_OBJECTID};
use ferrix_btrfs::tree::BtrfsKey;
use ferrix_btrfs::volume::{Device, ReadKind, Volume};

use super::*;
use crate::chunks::STRIPE_LEN;
use crate::extent::{Backref, EXTENT_ITEM_KEY, ExtentRecord, METADATA_ITEM_KEY};
use crate::node::Body;
use crate::ranges::RangeSet;
use crate::space::Kind;

mod fsops;
mod full;
mod inodes;
mod log;
mod powerfail;

const BLOCK: usize = 4096;
/// Every fixture is 128 MiB.
pub(crate) const IMAGE_SIZE: u64 = 128 * 1024 * 1024;
/// An empty volume with `mkfs.btrfs`'s default profiles and features.
pub(crate) const BLANK: &[u8] = include_bytes!("../../btrfs/testdata/blank.img.packed");
/// A populated volume with SINGLE metadata.
pub(crate) const POPULATED: &[u8] = include_bytes!("../../btrfs/testdata/none.img.packed");

/// One thing a device was asked to do, for replaying a crash.
#[derive(Debug, Clone)]
pub(crate) enum Op {
    Write(u64, Vec<u8>),
    Flush,
}

/// A device in memory, over a packed image, that remembers what it was told.
#[derive(Debug, Clone)]
pub(crate) struct MemDevice {
    blocks: BTreeMap<u64, Box<[u8; BLOCK]>>,
    pub(crate) log: Vec<Op>,
}

impl MemDevice {
    pub(crate) fn new(packed: &[u8]) -> MemDevice {
        let mut blocks = BTreeMap::new();
        for record in packed.chunks_exact(8 + BLOCK) {
            let offset = u64::from_le_bytes(record[..8].try_into().unwrap());
            let block: [u8; BLOCK] = record[8..].try_into().unwrap();
            let _ = blocks.insert(offset, Box::new(block));
        }
        MemDevice {
            blocks,
            log: Vec::new(),
        }
    }

    fn store(&mut self, physical: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            let within = (at - base) as usize;
            let take = (BLOCK - within).min(data.len() - done);
            let block = self
                .blocks
                .entry(base)
                .or_insert_with(|| Box::new([0; BLOCK]));
            block[within..within + take].copy_from_slice(&data[done..done + take]);
            done += take;
        }
    }

    /// The whole image, for handing to host tools.
    pub(crate) fn image(&self) -> Vec<u8> {
        let mut out = vec![0u8; IMAGE_SIZE as usize];
        for (&offset, block) in &self.blocks {
            out[offset as usize..offset as usize + BLOCK].copy_from_slice(&block[..]);
        }
        out
    }
}

impl Device for MemDevice {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        _kind: ReadKind,
    ) -> core::result::Result<(), BtrfsError> {
        if physical + buf.len() as u64 > IMAGE_SIZE {
            return Err(BtrfsError::DeviceRead { physical });
        }
        let mut done = 0;
        while done < buf.len() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            let within = (at - base) as usize;
            let take = (BLOCK - within).min(buf.len() - done);
            match self.blocks.get(&base) {
                Some(block) => {
                    buf[done..done + take].copy_from_slice(&block[within..within + take]);
                }
                None => buf[done..done + take].fill(0),
            }
            done += take;
        }
        Ok(())
    }
}

impl WriteDevice for MemDevice {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> Result<()> {
        if physical + data.len() as u64 > IMAGE_SIZE {
            return Err(Error::DeviceWrite { physical });
        }
        self.store(physical, data);
        self.log.push(Op::Write(physical, data.to_vec()));
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.log.push(Op::Flush);
        Ok(())
    }
}

/// xorshift64*, so the tests do not depend on a crate for randomness.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ---------------------------------------------------------------------------
// The consistency check
// ---------------------------------------------------------------------------

/// A tree block found by walking a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Block {
    level: u8,
    owner: u64,
}

/// Walk `tree` from its root, checking the shape every read also checks and
/// the pointer keys the kernel checks, and record every block.
fn walk_tree(volume: &mut WriteVolume<MemDevice>, tree: TreeId, blocks: &mut BTreeMap<u64, Block>) {
    let root = volume.root(tree).unwrap();
    let mut stack = vec![(
        root.bytenr,
        root.level,
        root.generation,
        None::<BtrfsKey>,
        true,
    )];
    while let Some((at, level, generation, first, is_root)) = stack.pop() {
        volume
            .load(at, level, generation)
            .unwrap_or_else(|e| panic!("tree {tree}: block {at:#x}: {e}"));
        let node = volume.node(at).unwrap().clone();
        assert_eq!(
            node.owner, tree,
            "block {at:#x} is owned by the tree it is in"
        );
        assert!(
            is_root || !node.is_empty(),
            "tree {tree}: empty non-root block {at:#x}"
        );
        assert!(
            !node.overflows(volume.nodesize()),
            "block {at:#x} overflows"
        );
        if let Some(first) = first {
            assert_eq!(
                node.first_key(),
                Some(first),
                "tree {tree}: parent key of {at:#x}"
            );
        }
        assert!(
            blocks.insert(at, Block { level, owner: tree }).is_none(),
            "block {at:#x} reached twice"
        );
        if let Body::Internal(ptrs) = &node.body {
            for ptr in ptrs {
                stack.push((
                    ptr.blockptr,
                    level - 1,
                    ptr.generation,
                    Some(ptr.key),
                    false,
                ));
            }
        }
    }
}

/// Every item of a tree, in order.
pub(crate) fn items(volume: &mut WriteVolume<MemDevice>, tree: TreeId) -> Vec<(BtrfsKey, Vec<u8>)> {
    volume.range(tree, &BtrfsKey::MIN, &BtrfsKey::MAX).unwrap()
}

/// Every file extent's reference, counted, from the fs trees.
fn data_refs(
    volume: &mut WriteVolume<MemDevice>,
    trees: &[TreeId],
) -> BTreeMap<u64, BTreeMap<Backref, u64>> {
    let mut refs: BTreeMap<u64, BTreeMap<Backref, u64>> = BTreeMap::new();
    for &tree in trees {
        if tree != FS_TREE_OBJECTID && tree != 0u64.wrapping_sub(9) {
            continue;
        }
        for (key, data) in items(volume, tree) {
            if key.item_type != EXTENT_DATA_KEY {
                continue;
            }
            let extent = ExtentData::parse(&data).unwrap();
            if let Some(file) = extent.file_extent().filter(|f| !f.is_hole()) {
                let backref = Backref::Data {
                    root: tree,
                    objectid: key.objectid,
                    offset: key.offset - file.offset,
                };
                *refs
                    .entry(file.disk_bytenr)
                    .or_default()
                    .entry(backref)
                    .or_default() += 1;
            }
        }
    }
    refs
}

/// Compare the extent tree with the tree blocks and file extents found,
/// returning every extent by start and the data extents alone.
fn check_extent_tree(
    volume: &mut WriteVolume<MemDevice>,
    blocks: &BTreeMap<u64, Block>,
    mut data_refs: BTreeMap<u64, BTreeMap<Backref, u64>>,
) -> (BTreeMap<u64, u64>, RangeSet) {
    let nodesize = u64::from(volume.nodesize());
    let mut extents = BTreeMap::new();
    let mut data_extents = RangeSet::new();
    let mut seen_blocks = BTreeSet::new();
    let all = items(volume, EXTENT_TREE_OBJECTID);
    let mut index = 0;
    while index < all.len() {
        let (key, data) = &all[index];
        index += 1;
        if key.item_type != EXTENT_ITEM_KEY && key.item_type != METADATA_ITEM_KEY {
            continue;
        }
        let (mut record, stored) = ExtentRecord::parse(key, data).unwrap();
        while index < all.len()
            && all[index].0.objectid == key.objectid
            && all[index].0.item_type < 192
        {
            record.add_keyed(&all[index].0, &all[index].1).unwrap();
            index += 1;
        }
        record.check_total(stored).unwrap();
        let len = if key.item_type == METADATA_ITEM_KEY {
            nodesize
        } else {
            key.offset
        };
        let overlaps = extents
            .range(..key.objectid + len)
            .next_back()
            .is_some_and(|(&s, &l): (&u64, &u64)| s + l > key.objectid);
        assert!(!overlaps, "extent {:#x} overlaps another", key.objectid);
        let _ = extents.insert(key.objectid, len);
        if record.is_tree_block() {
            let block = blocks
                .get(&key.objectid)
                .unwrap_or_else(|| panic!("extent item for unreachable block {:#x}", key.objectid));
            assert_eq!(
                key.offset,
                u64::from(block.level),
                "level of block {:#x}",
                key.objectid
            );
            let want: BTreeMap<Backref, u64> = [(Backref::Tree { root: block.owner }, 1)].into();
            assert_eq!(record.refs, want, "references of block {:#x}", key.objectid);
            let _ = seen_blocks.insert(key.objectid);
        } else {
            let _ = data_extents.insert(key.objectid, len);
            let want = data_refs.remove(&key.objectid).unwrap_or_default();
            assert_eq!(
                record.refs, want,
                "references of data extent {:#x}",
                key.objectid
            );
        }
    }
    let missing: Vec<_> = blocks.keys().filter(|b| !seen_blocks.contains(b)).collect();
    assert!(
        missing.is_empty(),
        "tree blocks without extent items: {missing:x?}"
    );
    assert!(
        data_refs.is_empty(),
        "file extents without extent items: {:x?}",
        data_refs.keys()
    );
    (extents, data_extents)
}

/// Each block group's usage, and its free space as the complement of its
/// extents less its superblock stripes; and the superblock's total.
fn check_groups(volume: &WriteVolume<MemDevice>, extents: &BTreeMap<u64, u64>) {
    let mut used_total = 0;
    for group in volume.space.groups() {
        let mut free = RangeSet::new();
        let _ = free.insert(group.start, group.length);
        let mut used = 0;
        for (&start, &len) in extents.range(group.start..group.end()) {
            assert!(
                free.remove(start, len),
                "extent {start:#x} crosses its group's end"
            );
            used += len;
        }
        assert_eq!(group.used, used, "usage of block group {:#x}", group.start);
        // The superblock stripes nothing uses are free on disk but never
        // allocatable.
        let mut allocatable = free.clone();
        let reserved = allocatable.extract(&group.excluded);
        assert_eq!(
            group.free, allocatable,
            "free space of block group {:#x}",
            group.start
        );
        assert_eq!(
            group.reserved, reserved,
            "superblock stripes of block group {:#x}",
            group.start
        );
        used_total += used;
    }
    assert_eq!(volume.space.used(), used_total);
    let bytes_used = bytes::get_u64(&volume.superblock, 120).unwrap();
    assert_eq!(bytes_used, used_total, "superblock bytes_used");
}

/// Check everything a commit must keep consistent; see the module
/// documentation. Panics with what disagreed.
pub(crate) fn check(device: &MemDevice) {
    // The reader opens it.
    let mut reader = device.clone();
    let mut node = vec![0u8; 65536];
    let storage = vec![ChunkMapEntry::EMPTY; 256];
    let _ =
        Volume::open(&mut reader, storage, &mut node).expect("the reader opens what was written");
    // The writer opens it: that checks block group items and free-space
    // counts against the free-space tree.
    let mut volume =
        WriteVolume::open_committed(device.clone()).expect("the writer reopens what it wrote");
    let mut blocks = BTreeMap::new();
    let trees: Vec<TreeId> = volume.roots.keys().copied().collect();
    for tree in &trees {
        walk_tree(&mut volume, *tree, &mut blocks);
    }
    let refs = data_refs(&mut volume, &trees);
    let (extents, data_extents) = check_extent_tree(&mut volume, &blocks, refs);
    check_groups(&volume, &extents);
    inodes::check_files(&mut volume, &data_extents);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn the_fixtures_open_for_writing_and_check_clean() {
    for packed in [BLANK, POPULATED] {
        let device = MemDevice::new(packed);
        check(&device);
    }
}

#[test]
fn a_commit_with_nothing_to_do_writes_nothing() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    volume.commit().unwrap();
    assert!(volume.device.log.is_empty());
    assert_eq!(volume.generation(), volume.transid() - 1);
}

/// A key in a range of object ids nothing in the fixtures uses, of a type
/// nothing reads, so the model test owns every item it touches.
fn scratch_key(rng: &mut Rng) -> BtrfsKey {
    BtrfsKey::new(100_000 + rng.below(2000), 250, rng.below(4))
}

/// Random inserts, updates and deletes in the fs tree, checked against a
/// model after every step, with commits and a full consistency check along
/// the way, and a reopen at the end that must find the same items.
fn edit_against_model(packed: &[u8], seed: u64, steps: usize) {
    let mut volume = WriteVolume::open(MemDevice::new(packed)).unwrap();
    let tree = FS_TREE_OBJECTID;
    let mut model: BTreeMap<BtrfsKey, Vec<u8>> = items(&mut volume, tree).into_iter().collect();
    let mut rng = Rng::new(seed);
    for step in 0..steps {
        let key = scratch_key(&mut rng);
        let data: Vec<u8> = (0..rng.below(300))
            .map(|i| (i as u8) ^ (step as u8))
            .collect();
        match rng.below(4) {
            0 | 1 => {
                let result = volume.insert(tree, key, data.clone());
                match model.entry(key) {
                    // `Exists` is an answer, and aborts nothing.
                    Entry::Occupied(_) => assert_eq!(result, Err(Error::Exists)),
                    Entry::Vacant(slot) => {
                        result.unwrap();
                        let _ = slot.insert(data);
                    }
                }
            }
            2 => {
                let target = model
                    .range(key..)
                    .map(|(k, _)| *k)
                    .find(|k| k.item_type == 250);
                if let Some(target) = target {
                    volume.update(tree, target, data.clone()).unwrap();
                    let _ = model.insert(target, data);
                }
            }
            _ => {
                let target = model
                    .range(key..)
                    .map(|(k, _)| *k)
                    .find(|k| k.item_type == 250);
                if let Some(target) = target {
                    volume.delete(tree, &target).unwrap();
                    let _ = model.remove(&target);
                }
            }
        }
        if step % 500 == 499 {
            volume.commit().unwrap();
            check(&volume.device);
        }
    }
    let found: BTreeMap<_, _> = items(&mut volume, tree).into_iter().collect();
    assert_eq!(found, model, "the tree holds what the model does");
    volume.commit().unwrap();
    check(&volume.device);
    let mut reopened = WriteVolume::open(volume.into_device()).unwrap();
    let found: BTreeMap<_, _> = items(&mut reopened, tree).into_iter().collect();
    assert_eq!(found, model, "the committed tree holds what the model does");
}

/// Verifies: L.btrfs.17, H.STORE.5
#[test]
fn random_edits_on_the_blank_volume_match_a_model() {
    edit_against_model(BLANK, 1, 3000);
}

/// Verifies: L.btrfs.17, H.STORE.5
#[test]
fn random_edits_on_a_populated_volume_match_a_model() {
    edit_against_model(POPULATED, 2, 3000);
}

/// Verifies: L.btrfs.17
#[test]
fn deleting_everything_added_shrinks_the_tree_back() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let tree = FS_TREE_OBJECTID;
    let before = items(&mut volume, tree);
    let keys: Vec<BtrfsKey> = (0..2000)
        .map(|i| BtrfsKey::new(100_000 + i, 250, 0))
        .collect();
    for key in &keys {
        volume.insert(tree, *key, vec![7; 100]).unwrap();
    }
    volume.commit().unwrap();
    assert!(
        volume.root(tree).unwrap().level >= 2,
        "2000 items make a tree of three levels"
    );
    check(&volume.device);
    for key in &keys {
        volume.delete(tree, key).unwrap();
    }
    volume.commit().unwrap();
    check(&volume.device);
    assert_eq!(volume.root(tree).unwrap().level, 0);
    assert_eq!(items(&mut volume, tree), before);
}

/// Where the first superblock mirror, at 64 MiB on the device, lies in
/// [`BLANK`]: 27 MiB into the DUP metadata chunk at logical 29 MiB, whose
/// first stripe starts at 37 MiB. `mkfs.btrfs` lays out every larger volume
/// the same way, and on one of them a writer that allocated here lost an fs
/// tree leaf to the next commit's superblock.
const MIRROR_IN_METADATA: u64 = 58_720_256;

/// Every copy of every block reachable from the committed trees, compared:
/// a node that a superblock write landed on differs from its other copy.
fn assert_copies_agree(volume: &mut WriteVolume<MemDevice>) -> BTreeMap<u64, Block> {
    let mut blocks = BTreeMap::new();
    let trees: Vec<TreeId> = volume.roots.keys().copied().collect();
    for tree in &trees {
        walk_tree(volume, *tree, &mut blocks);
    }
    let size = volume.nodesize() as usize;
    for &at in blocks.keys() {
        let copies = volume.chunks.copies(at, size as u64).unwrap();
        let mut first = vec![0u8; size];
        let mut other = vec![0u8; size];
        for (index, &physical) in copies.iter().enumerate() {
            let buf = if index == 0 { &mut first } else { &mut other };
            volume
                .device
                .read_at(physical, buf, ReadKind::Metadata)
                .unwrap();
            if index > 0 {
                assert!(first == other, "copy {index} of block {at:#x} differs");
            }
        }
    }
    blocks
}

/// [`BLANK`] after enough small commits to carry the metadata allocator past
/// [`MIRROR_IN_METADATA`] — each commit copies its paths to blocks further
/// on — with every block's copies compared after each.
pub(crate) fn past_the_mirror() -> MemDevice {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let stripe = STRIPE_LEN;
    let group = volume.space.group_of(MIRROR_IN_METADATA).unwrap();
    assert!(group.reserved.contains(MIRROR_IN_METADATA, stripe));
    assert!(!group.free.overlaps(MIRROR_IN_METADATA, stripe));
    let mut past = false;
    let mut added = 0;
    for i in 0..5000 {
        added = i + 1;
        volume
            .insert(
                FS_TREE_OBJECTID,
                BtrfsKey::new(100_000 + i, 250, 0),
                vec![3; 200],
            )
            .unwrap();
        volume.commit().unwrap();
        let blocks = assert_copies_agree(&mut volume);
        assert!(
            !blocks
                .keys()
                .any(|&at| at + u64::from(volume.nodesize()) > MIRROR_IN_METADATA
                    && at < MIRROR_IN_METADATA + stripe),
            "a tree block was allocated over the superblock mirror"
        );
        if blocks.keys().any(|&at| at > MIRROR_IN_METADATA + stripe) {
            past = true;
            break;
        }
    }
    assert!(past, "the allocator reached the mirror");
    // The scratch items go again, so host `btrfs check` finds no item
    // without an inode.
    for i in 0..added {
        volume
            .delete(FS_TREE_OBJECTID, &BtrfsKey::new(100_000 + i, 250, 0))
            .unwrap();
    }
    volume.commit().unwrap();
    let _ = assert_copies_agree(&mut volume);
    volume.into_device()
}

/// Verifies: L.btrfs.16, H.STORE.5
#[test]
fn no_tree_block_is_allocated_over_a_superblock_mirror() {
    let device = past_the_mirror();
    check(&device);
    // The free-space tree still lists the mirror's stripe as free, as Linux
    // writes it.
    let volume = WriteVolume::open_committed(device).unwrap();
    let group = volume.space.group_of(MIRROR_IN_METADATA).unwrap();
    assert!(group.on_disk.contains(MIRROR_IN_METADATA, STRIPE_LEN));
}

/// [`POPULATED`] grown by data chunks until one has its stripe over the
/// superblock mirror at 64 MiB, committed. Returns the device and that
/// chunk's logical address of the mirror.
pub(crate) fn grown_over_the_mirror() -> (MemDevice, u64) {
    let mirror = ferrix_btrfs::superblock::SUPERBLOCK_OFFSETS[1];
    let mut volume = WriteVolume::open(MemDevice::new(POPULATED)).unwrap();
    let covering = |volume: &WriteVolume<MemDevice>| {
        volume.chunks.iter().find_map(|chunk| {
            let stripe = chunk.stripes.first()?;
            let within = mirror.checked_sub(stripe.offset)?;
            (within < chunk.length).then_some(chunk.logical + within)
        })
    };
    while covering(&volume).is_none() {
        volume.grow(Kind::Data).unwrap();
    }
    let logical = covering(&volume).unwrap();
    let group = volume.space.group_of(logical).unwrap();
    assert!(group.reserved.contains(logical, STRIPE_LEN));
    assert!(!group.free.overlaps(logical, STRIPE_LEN));
    assert!(group.on_disk.contains(logical, STRIPE_LEN));
    volume.commit().unwrap();
    (volume.into_device(), logical)
}

/// Verifies: L.btrfs.16, H.STORE.5
#[test]
fn a_new_chunk_over_a_superblock_mirror_keeps_the_mirror_out_of_allocation() {
    let (device, logical) = grown_over_the_mirror();
    check(&device);
    let mut volume = WriteVolume::open(device).unwrap();
    let group = volume.space.group_of(logical).unwrap();
    assert!(group.reserved.contains(logical, STRIPE_LEN));
    // Every byte of data space, handed out: none of it over the mirror.
    while let Ok((at, len)) = volume.space.alloc_data(1 << 20, 4096, 4096) {
        assert!(
            at + len <= logical || at >= logical + STRIPE_LEN,
            "data allocated over the superblock mirror at {at:#x}"
        );
    }
}

/// Verifies: L.btrfs.13, L.btrfs.14, H.STORE.3
#[test]
fn a_crash_before_the_superblock_leaves_the_last_commit() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let base = volume.device.clone();
    for i in 0..500 {
        volume
            .insert(
                FS_TREE_OBJECTID,
                BtrfsKey::new(100_000 + i, 250, 0),
                vec![1; 50],
            )
            .unwrap();
    }
    volume.commit().unwrap();
    // Replay every write the commit made except the superblocks, in order.
    let mut crashed = base.clone();
    for op in &volume.device.log {
        if let Op::Write(at, data) = op
            && !ferrix_btrfs::superblock::SUPERBLOCK_OFFSETS.contains(at)
        {
            crashed.store(*at, data);
        }
    }
    check(&crashed);
    let mut reopened = WriteVolume::open(crashed).unwrap();
    assert_eq!(
        items(&mut reopened, FS_TREE_OBJECTID),
        items(&mut WriteVolume::open(base).unwrap(), FS_TREE_OBJECTID)
    );
}

/// Verifies: L.btrfs.13, H.STORE.3
#[test]
fn the_commit_flushes_before_the_superblock() {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    volume
        .insert(
            FS_TREE_OBJECTID,
            BtrfsKey::new(100_000, 250, 0),
            vec![1; 50],
        )
        .unwrap();
    volume.commit().unwrap();
    let log = &volume.device.log;
    let primary = log
        .iter()
        .position(
            |op| matches!(op, Op::Write(at, _) if *at == ferrix_btrfs::superblock::PRIMARY_OFFSET),
        )
        .unwrap();
    assert!(
        matches!(log[primary - 1], Op::Flush),
        "a flush comes right before the primary superblock"
    );
    assert!(
        matches!(log[primary + 1], Op::Flush),
        "and the primary is made durable before the mirror"
    );
    let nodes_after = log[primary..].iter().any(|op| matches!(op, Op::Write(at, _) if !ferrix_btrfs::superblock::SUPERBLOCK_OFFSETS.contains(at)));
    assert!(!nodes_after, "no node is written after the superblock");
}

/// Write the images the other tests produce to `FERRIX_BTRFS_OUT` for host
/// `btrfs check`; see `tools/common/test/btrfs-check-writer.sh`.
#[test]
#[ignore = "writes images for host btrfs check; run by tools/common/test/btrfs-check-writer.sh"]
fn btrfs_check_images() {
    let out = std::env::var("FERRIX_BTRFS_OUT").expect("FERRIX_BTRFS_OUT names a directory");
    for (name, device) in fsops::images() {
        std::fs::write(std::format!("{out}/{name}.img"), device.image()).unwrap();
    }
}
