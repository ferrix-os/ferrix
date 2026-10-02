//! Tests for the volume reader, against images real `mkfs.btrfs` wrote.
//!
//! The rest of the crate's tests build their structures by hand, which pins
//! every field to an offset. These pin the reader to btrfs itself: the images
//! in `testdata/` come from `tools/common/gen/gen-btrfs-fixtures.py`, and nothing in
//! them was produced by this crate.

extern crate std;

use core::ops::ControlFlow;
use std::collections::BTreeMap;
use std::vec;
use std::vec::Vec;

use super::*;
use crate::chunk::ChunkMapEntry;
use crate::items::INODE_ITEM_KEY;

const BLOCK: usize = 4096;
/// The size of every fixture but [`DUP`].
const IMAGE_SIZE: u64 = 128 * 1024 * 1024;
/// A volume with two copies of everything, data too, holding one file,
/// `data.bin`; 256 MiB, as `mkfs.btrfs --rootdir` sized it.
pub(crate) const DUP: &[u8] = include_bytes!("../../testdata/dup.img.packed");
/// The size of [`DUP`].
const DUP_SIZE: u64 = 256 * 1024 * 1024;

/// The four images, by the compression `mkfs.btrfs` was asked for.
pub(crate) const IMAGES: [(&str, &[u8]); 4] = [
    ("none", include_bytes!("../../testdata/none.img.packed")),
    ("zlib", include_bytes!("../../testdata/zlib.img.packed")),
    ("lzo", include_bytes!("../../testdata/lzo.img.packed")),
    ("zstd", include_bytes!("../../testdata/zstd.img.packed")),
];

/// A device over a packed image: the non-zero blocks, and zeros elsewhere.
pub(crate) struct PackedDevice {
    blocks: BTreeMap<u64, [u8; BLOCK]>,
    /// Where the device ends.
    size: u64,
    /// Physical offsets to fail reads at, to test error propagation.
    pub(crate) fail_at: Option<u64>,
}

impl PackedDevice {
    pub(crate) fn new(packed: &[u8]) -> PackedDevice {
        let mut blocks = BTreeMap::new();
        for record in packed.chunks_exact(8 + BLOCK) {
            let offset = u64::from_le_bytes(record[..8].try_into().unwrap());
            let previous = blocks.insert(offset, record[8..].try_into().unwrap());
            assert!(previous.is_none(), "the generator packs each block once");
        }
        // Only the DUP fixture has blocks past the others' size.
        let last = blocks.keys().next_back().copied().unwrap_or(0);
        PackedDevice {
            blocks,
            size: if last < IMAGE_SIZE {
                IMAGE_SIZE
            } else {
                DUP_SIZE
            },
            fail_at: None,
        }
    }

    /// Flip one bit of the image at `physical`.
    pub(crate) fn corrupt(&mut self, physical: u64) {
        let base = physical - physical % BLOCK as u64;
        let block = self.blocks.entry(base).or_insert([0; BLOCK]);
        block[(physical - base) as usize] ^= 0x10;
    }

    /// Overwrite the image at `physical` with `bytes`.
    pub(crate) fn write(&mut self, physical: u64, bytes: &[u8]) {
        for (i, &byte) in bytes.iter().enumerate() {
            let at = physical + i as u64;
            let base = at - at % BLOCK as u64;
            let block = self.blocks.entry(base).or_insert([0; BLOCK]);
            block[(at - base) as usize] = byte;
        }
    }

    /// Checksum the 4 KiB node at `physical` again after an edit, the way a
    /// hostile image arrives: damaged, and consistent with its checksum.
    pub(crate) fn reseal(&mut self, physical: u64) {
        let block = self
            .blocks
            .get_mut(&physical)
            .expect("a node starts a block");
        let sum = crate::crc32c::crc32c(&block[32..]);
        block[..4].copy_from_slice(&sum.to_le_bytes());
    }
}

impl Device for PackedDevice {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        _kind: ReadKind,
    ) -> Result<(), BtrfsError> {
        let end = physical.checked_add(buf.len() as u64);
        if end.is_none_or(|end| end > self.size)
            || self
                .fail_at
                .is_some_and(|bad| (physical..end.unwrap()).contains(&bad))
        {
            return Err(BtrfsError::DeviceRead { physical });
        }
        // A block at a time: a per-byte loop made every image test crawl,
        // and under Miri it never finished.
        let mut done = 0;
        while done < buf.len() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            let within = (at - base) as usize;
            let len = (BLOCK - within).min(buf.len() - done);
            let dest = &mut buf[done..done + len];
            match self.blocks.get(&base) {
                Some(block) => dest.copy_from_slice(&block[within..within + len]),
                None => dest.fill(0),
            }
            done += len;
        }
        Ok(())
    }
}

/// Open `packed` with generous storage, handing both to `test`.
fn with_volume(
    packed: &[u8],
    test: impl FnOnce(&mut PackedDevice, &Volume<&mut [ChunkMapEntry; 16]>, &mut [u8]),
) {
    let mut device = PackedDevice::new(packed);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let mut node = vec![0u8; 65536];
    let volume = Volume::open(&mut device, &mut chunks, &mut node).unwrap();
    test(&mut device, &volume, &mut node);
}

/// Every key in `root`, in the order the walk visits them.
fn all_keys(
    device: &mut PackedDevice,
    volume: &Volume<&mut [ChunkMapEntry; 16]>,
    root: TreeRoot,
    node: &mut [u8],
) -> Vec<BtrfsKey> {
    let mut keys = Vec::new();
    let done = volume
        .walk(device, root, BtrfsKey::MIN, node, |_, item| {
            keys.push(item.key);
            Ok(ControlFlow::<()>::Continue(()))
        })
        .unwrap();
    assert_eq!(done, None, "a walk that never breaks runs off the end");
    keys
}

/// How many paths the manifest lists.
fn manifest_entries() -> usize {
    include_str!("../../testdata/manifest.txt")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .count()
}

#[test]
#[cfg_attr(
    miri,
    ignore = "walks all four real images key by key; plain cargo test covers it"
)]
fn every_image_mounts_and_finds_its_fs_tree() {
    for (name, packed) in IMAGES {
        with_volume(packed, |_, volume, _| {
            assert_eq!(
                volume.nodesize(),
                4096,
                "{name}: the generator asks for 4 KiB nodes"
            );
            assert_eq!(volume.sectorsize(), 4096, "{name}: sector size");
            assert_eq!(
                volume.root_dir(),
                256,
                "{name}: the top-level subvolume's root directory"
            );
            assert_eq!(
                volume.fs_tree().level,
                1,
                "{name}: the generator checks the fs tree is more than a leaf"
            );
            assert!(
                volume.chunks().len() >= 3,
                "{name}: system, metadata and data chunks"
            );
        });
    }
}

/// Verifies: L.btrfs.6
#[test]
#[cfg_attr(
    miri,
    ignore = "walks all four real images key by key; plain cargo test covers it"
)]
fn a_walk_visits_every_key_once_in_order() {
    for (name, packed) in IMAGES {
        with_volume(packed, |device, volume, node| {
            let keys = all_keys(device, volume, volume.fs_tree(), node);
            assert!(
                keys.windows(2).all(|pair| pair[0] < pair[1]),
                "{name}: keys must strictly ascend across leaf boundaries"
            );
            let inodes = keys
                .iter()
                .filter(|key| key.item_type == INODE_ITEM_KEY)
                .count();
            // No hard links in the fixture, so every manifest path is its own
            // inode; the root directory is the one inode no path names.
            assert_eq!(
                inodes,
                manifest_entries() + 1,
                "{name}: one inode per path, plus the root directory"
            );
        });
    }
}

/// Verifies: L.btrfs.6
#[test]
fn a_walk_stops_when_the_visitor_breaks() {
    with_volume(IMAGES[0].1, |device, volume, node| {
        let mut seen = 0;
        let found = volume
            .walk(device, volume.fs_tree(), BtrfsKey::MIN, node, |_, item| {
                seen += 1;
                Ok(if seen == 5 {
                    ControlFlow::Break(item.key)
                } else {
                    ControlFlow::Continue(())
                })
            })
            .unwrap();
        assert!(found.is_some(), "the break value comes back");
        assert_eq!(seen, 5, "nothing is visited after the break");
    });
}

/// Verifies: L.btrfs.6
#[test]
#[cfg_attr(
    miri,
    ignore = "walks all four real images key by key; plain cargo test covers it"
)]
fn the_last_key_at_or_before_is_found_across_leaf_boundaries() {
    for (name, packed) in IMAGES {
        with_volume(packed, |device, volume, node| {
            let root = volume.fs_tree();
            let keys = all_keys(device, volume, root, node);
            for pair in keys.windows(2) {
                let (before, after) = (pair[0], pair[1]);
                assert_eq!(
                    volume
                        .last_at_or_before(device, root, &after, node)
                        .unwrap(),
                    Some(after),
                    "{name}: an exact key finds itself"
                );
                if let Some(just_below) = predecessor(&after).filter(|k| *k != before) {
                    assert_eq!(
                        volume
                            .last_at_or_before(device, root, &just_below, node)
                            .unwrap(),
                        Some(before),
                        "{name}: a key in the gap finds the item before the gap"
                    );
                }
            }
            let first = keys[0];
            if let Some(below_all) = predecessor(&first) {
                assert_eq!(
                    volume
                        .last_at_or_before(device, root, &below_all, node)
                        .unwrap(),
                    None,
                    "{name}: nothing sorts before the first key"
                );
            }
        });
    }
}

#[test]
fn a_node_buffer_smaller_than_a_node_is_refused() {
    let mut device = PackedDevice::new(IMAGES[0].1);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let mut node = vec![0u8; 1024];
    assert!(
        matches!(
            Volume::open(&mut device, &mut chunks, &mut node),
            Err(BtrfsError::Truncated { .. })
        ),
        "a 1 KiB buffer cannot hold a 4 KiB node"
    );
}

/// Verifies: L.btrfs.1, H.STORE.2
#[test]
fn a_corrupt_superblock_is_refused() {
    let mut device = PackedDevice::new(IMAGES[0].1);
    device.corrupt(PRIMARY_OFFSET + 0x100);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let mut node = vec![0u8; 65536];
    assert!(
        matches!(
            Volume::open(&mut device, &mut chunks, &mut node),
            Err(BtrfsError::BadChecksum { .. })
        ),
        "a flipped bit in the superblock fails its checksum"
    );
}

/// Verifies: L.btrfs.10, H.STORE.2
#[test]
fn a_corrupt_tree_node_is_refused_not_misread() {
    with_volume(IMAGES[0].1, |device, volume, node| {
        let root = volume.fs_tree();
        let (_, physical) = volume.chunks().map(root.bytenr).unwrap();
        device.corrupt(physical + 200);
        assert!(
            matches!(
                volume.seek(device, root, &BtrfsKey::MIN, node),
                Err(BtrfsError::BadChecksum { .. })
            ),
            "the fs tree's top node no longer checks out"
        );
    });
}

/// Verifies: L.btrfs.10
#[test]
fn a_device_error_propagates() {
    with_volume(IMAGES[0].1, |device, volume, node| {
        let root = volume.fs_tree();
        let (_, physical) = volume.chunks().map(root.bytenr).unwrap();
        device.fail_at = Some(physical);
        assert_eq!(
            volume.seek(device, root, &BtrfsKey::MIN, node).unwrap_err(),
            BtrfsError::DeviceRead { physical },
            "a failed read is reported as such, not as corruption"
        );
    });
}

/// Verifies: L.btrfs.10, H.STORE.2
#[test]
fn a_pointer_to_the_wrong_generation_is_refused() {
    with_volume(IMAGES[0].1, |device, volume, node| {
        let mut stale = volume.fs_tree();
        stale.generation += 1;
        assert_eq!(
            volume
                .seek(device, stale, &BtrfsKey::MIN, node)
                .unwrap_err(),
            BtrfsError::BadTree {
                logical: stale.bytenr
            },
            "a node from another transaction is not the one the pointer meant"
        );
    });
}

/// Verifies: L.btrfs.10, H.STORE.2
#[test]
fn a_dup_node_whose_first_copy_fails_is_read_from_the_second() {
    let mut device = PackedDevice::new(DUP);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let mut node = vec![0u8; 65536];
    let volume = Volume::open(&mut device, &mut chunks, &mut node).unwrap();
    let root = volume.fs_tree();
    assert_eq!(volume.copies(root.bytenr), 2, "DUP metadata has two copies");
    let expected = all_keys(&mut device, &volume, root, &mut node);
    let (_, first) = volume.chunks().map_copy(root.bytenr, 0).unwrap();
    let (_, second) = volume.chunks().map_copy(root.bytenr, 1).unwrap();
    assert_ne!(first, second);
    // What a volume Ferrix wrote suffered: the first copy of an fs tree leaf
    // overwritten by a superblock mirror.
    let mut superblock = [0u8; SUPERBLOCK_SIZE];
    device
        .read_at(PRIMARY_OFFSET, &mut superblock, ReadKind::Metadata)
        .unwrap();
    device.write(first, &superblock);
    assert_eq!(
        all_keys(&mut device, &volume, root, &mut node),
        expected,
        "the second copy serves the read"
    );
    device.corrupt(second + 200);
    assert_eq!(
        volume
            .seek(&mut device, root, &BtrfsKey::MIN, &mut node)
            .unwrap_err(),
        BtrfsError::BadTree {
            logical: root.bytenr
        },
        "with both copies bad the read fails, with the first copy's error"
    );
}

/// Verifies: L.btrfs.10
#[test]
fn a_volume_whose_root_tree_has_one_bad_copy_still_opens() {
    let mut device = PackedDevice::new(DUP);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let mut node = vec![0u8; 65536];
    let volume = Volume::open(&mut device, &mut chunks, &mut node).unwrap();
    let (_, first) = volume
        .chunks()
        .map_copy(volume.root_tree.bytenr, 0)
        .unwrap();
    let (_, second) = volume
        .chunks()
        .map_copy(volume.root_tree.bytenr, 1)
        .unwrap();
    let fs_tree = volume.fs_tree();
    device.corrupt(first + 300);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let reopened = Volume::open(&mut device, &mut chunks, &mut node).unwrap();
    assert_eq!(reopened.fs_tree(), fs_tree);
    device.corrupt(second + 300);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    assert!(
        matches!(
            Volume::open(&mut device, &mut chunks, &mut node),
            Err(BtrfsError::BadChecksum { .. })
        ),
        "with both copies bad the volume does not open"
    );
}

/// Verifies: L.btrfs.10
#[test]
fn a_tree_deeper_than_btrfs_allows_is_refused() {
    with_volume(IMAGES[0].1, |device, volume, node| {
        let mut deep = volume.fs_tree();
        deep.level = MAX_LEVEL + 1;
        assert!(
            matches!(
                volume.seek(device, deep, &BtrfsKey::MIN, node),
                Err(BtrfsError::BadTree { .. })
            ),
            "the level is checked before anything is read"
        );
    });
}

/// Verifies: L.btrfs.6
#[test]
fn predecessor_steps_through_every_field() {
    assert_eq!(
        predecessor(&BtrfsKey::new(5, 1, 7)),
        Some(BtrfsKey::new(5, 1, 6)),
        "offset first"
    );
    assert_eq!(
        predecessor(&BtrfsKey::new(5, 1, 0)),
        Some(BtrfsKey::new(5, 0, u64::MAX)),
        "then type"
    );
    assert_eq!(
        predecessor(&BtrfsKey::new(5, 0, 0)),
        Some(BtrfsKey::new(4, u8::MAX, u64::MAX)),
        "then object id"
    );
    assert_eq!(
        predecessor(&BtrfsKey::MIN),
        None,
        "nothing is below the minimum"
    );
}

/// A small image whose default subvolume is not the top-level tree, from
/// `mkfs.btrfs -u default:sub`. The top level holds `top-level-only`; the
/// subvolume holds `marker` and `nested/file`.
const DEFAULT_SUBVOL: &[u8] = include_bytes!("../../testdata/default-subvol.img.packed");

/// Physical offset of the `default` entry in the root tree the superblock
/// points at.
///
/// Found in the live leaf only, reached by seeking its key: the image also
/// holds older copies of that leaf, left behind by copy-on-write, and an edit
/// to one of those changes nothing.
fn default_entry(device: &mut PackedDevice) -> u64 {
    use crate::items::{DIR_ITEM_HEADER_SIZE, ROOT_TREE_DIR_OBJECTID};
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let mut node = vec![0u8; 65536];
    let volume = Volume::open(device, &mut chunks, &mut node).unwrap();
    let key = BtrfsKey::new(ROOT_TREE_DIR_OBJECTID, DIR_ITEM_KEY, name_hash(b"default"));
    let leaf = volume
        .seek(device, volume.root_tree, &key, &mut node)
        .unwrap();
    let (_, physical) = volume.chunks().map(leaf.node.header().bytenr).unwrap();
    let block = &device.blocks[&physical];
    // `..=`: a leaf fills from its end, so the first item's name can end on
    // the block's last byte, which is where this image's does.
    let found: Vec<u64> = (DIR_ITEM_HEADER_SIZE..=BLOCK - 7)
        .map(|at| at - DIR_ITEM_HEADER_SIZE)
        .filter(|&start| {
            &block[start + DIR_ITEM_HEADER_SIZE..start + DIR_ITEM_HEADER_SIZE + 7] == b"default"
                && block[start + 27..start + 29] == 7u16.to_le_bytes()
                && block[start + 8] == ROOT_ITEM_KEY
        })
        .map(|start| physical + start as u64)
        .collect();
    assert_eq!(found.len(), 1, "the live leaf holds one default entry");
    found[0]
}

/// The default subvolume of the image after `patch` edits its live `default`
/// entry and the leaf is checksummed again.
fn open_with_default_entry(patch: impl FnOnce(&mut PackedDevice, u64)) -> Result<u64, BtrfsError> {
    let mut device = PackedDevice::new(DEFAULT_SUBVOL);
    let entry = default_entry(&mut device);
    patch(&mut device, entry);
    device.reseal(entry - entry % BLOCK as u64);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let mut node = vec![0u8; 65536];
    Volume::open(&mut device, &mut chunks, &mut node).map(|volume| volume.subvolume_id())
}

#[test]
fn a_volume_made_without_a_default_mounts_the_top_level_tree() {
    with_volume(IMAGES[0].1, |_, volume, _| {
        assert_eq!(
            volume.subvolume_id(),
            FS_TREE_OBJECTID,
            "mkfs.btrfs points the default entry at the top-level tree"
        );
    });
}

#[test]
fn the_subvolume_the_root_tree_names_is_the_one_mounted() {
    with_volume(DEFAULT_SUBVOL, |device, volume, node| {
        assert_eq!(
            volume.subvolume_id(),
            FIRST_FREE_OBJECTID,
            "mkfs.btrfs gives its first subvolume the first free id"
        );
        let sub = volume.default_subvolume();
        let marker = sub.lookup(device, sub.root_dir(), b"marker", node).unwrap();
        assert!(
            matches!(marker.map(|entry| entry.target), Some(Target::Inode(_))),
            "the subvolume's own file is at its root: {marker:?}"
        );
        assert_eq!(
            sub.lookup(device, sub.root_dir(), b"top-level-only", node)
                .unwrap(),
            None,
            "the top-level tree's file is not in the default subvolume"
        );
    });
}

#[test]
fn without_a_default_entry_the_top_level_tree_is_mounted() {
    let mut device = PackedDevice::new(DEFAULT_SUBVOL);
    // Point the superblock at a root tree directory that does not exist, so
    // no `default` entry is found, and checksum the superblock again.
    device.write(PRIMARY_OFFSET + 128, &1006u64.to_le_bytes());
    device.reseal(PRIMARY_OFFSET);
    let mut chunks = [ChunkMapEntry::EMPTY; 16];
    let mut node = vec![0u8; 65536];
    let volume = Volume::open(&mut device, &mut chunks, &mut node).unwrap();
    assert_eq!(
        volume.subvolume_id(),
        FS_TREE_OBJECTID,
        "as Linux does, a volume with no default entry mounts the top level"
    );
    let sub = volume.default_subvolume();
    assert!(
        sub.lookup(&mut device, sub.root_dir(), b"top-level-only", &mut node)
            .unwrap()
            .is_some(),
        "the top-level tree's file is at the root"
    );
}

#[test]
fn a_default_entry_that_names_no_subvolume_is_refused() {
    assert_eq!(
        open_with_default_entry(|_, _| {}),
        Ok(FIRST_FREE_OBJECTID),
        "resealed but unedited, the entry still names the subvolume"
    );
    assert_eq!(
        open_with_default_entry(|device, entry| device.write(entry + 8, &[INODE_ITEM_KEY])),
        Err(BtrfsError::BadItem {
            item_type: DIR_ITEM_KEY
        }),
        "a default entry naming an inode rather than a subvolume root is damage"
    );
    assert_eq!(
        open_with_default_entry(|device, entry| {
            device.write(entry, &crate::items::ROOT_TREE_OBJECTID.to_le_bytes());
        }),
        Err(BtrfsError::BadItem {
            item_type: DIR_ITEM_KEY
        }),
        "a default entry naming the root tree, which is no fs tree, is damage"
    );
}
