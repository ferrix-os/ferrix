//! The file-level half of the consistency check: what `btrfs check` compares
//! inside an fs tree, recomputed and compared the same way.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use ferrix_btrfs::items::{
    CSUM_TREE_OBJECTID, DIR_INDEX_KEY, DIR_ITEM_KEY, DirItemIter, EXTENT_CSUM_KEY, EXTENT_DATA_KEY,
    ExtentData, ExtentDataBody, FS_TREE_OBJECTID, INODE_EXTREF_KEY, INODE_ITEM_KEY,
    INODE_NODATASUM, INODE_REF_KEY, InodeExtrefIter, InodeItem, InodeRefIter, name_hash,
};

use super::{MemDevice, items};
use crate::WriteVolume;
use crate::fs::{ORPHAN_ITEM_KEY, ORPHAN_OBJECTID};
use crate::ranges::RangeSet;

#[derive(Debug, Default)]
struct Found {
    item: Option<InodeItem>,
    links: u32,
    dir_names: u64,
    index_names: u64,
    extent_bytes: u64,
}

/// What the fs tree says about each inode, which inodes are orphans, and
/// which data sectors need a checksum.
struct Survey {
    inodes: BTreeMap<u64, Found>,
    orphans: BTreeSet<u64>,
    need_sums: RangeSet,
}

/// Count the records of a back-reference item, failing on a bad one.
fn records<T>(iter: impl Iterator<Item = Result<T, ferrix_btrfs::BtrfsError>>) -> u32 {
    iter.fold(0, |n, record| {
        let _ = record.unwrap();
        n + 1
    })
}

/// Add one file extent to what its inode holds and what needs sums.
fn survey_extent(
    found: &mut Found,
    need_sums: &mut RangeSet,
    key: &ferrix_btrfs::tree::BtrfsKey,
    payload: &[u8],
    sectorsize: u32,
) {
    let sector = u64::from(sectorsize);
    let extent = ExtentData::parse_item(key, payload, sectorsize).unwrap();
    found.extent_bytes += match extent.body {
        ExtentDataBody::Inline(_) => extent.ram_bytes,
        ExtentDataBody::Regular(f) | ExtentDataBody::Prealloc(f) if f.disk_bytenr != 0 => {
            f.num_bytes
        }
        _ => 0,
    };
    if let ExtentDataBody::Regular(f) = extent.body
        && f.disk_bytenr != 0
    {
        let (at, len) = if extent.is_uncompressed() {
            (f.disk_bytenr + f.offset, f.num_bytes)
        } else {
            (f.disk_bytenr, f.disk_num_bytes)
        };
        // Pieces of one extent overlap in what they need.
        for s in (at..at + len).step_by(sector as usize) {
            let _ = need_sums.insert(s, sector);
        }
    }
}

fn survey(volume: &mut WriteVolume<MemDevice>) -> Survey {
    let sectorsize = volume.sectorsize();
    let mut out = Survey {
        inodes: BTreeMap::new(),
        orphans: BTreeSet::new(),
        need_sums: RangeSet::new(),
    };
    for (key, payload) in items(volume, FS_TREE_OBJECTID) {
        if key.objectid == ORPHAN_OBJECTID && key.item_type == ORPHAN_ITEM_KEY {
            let _ = out.orphans.insert(key.offset);
            continue;
        }
        // The tree-edit tests file scratch items under this type; they are
        // not files.
        if key.item_type == 250 {
            continue;
        }
        let found = out.inodes.entry(key.objectid).or_default();
        match key.item_type {
            INODE_ITEM_KEY => found.item = Some(InodeItem::parse(&payload).unwrap()),
            INODE_REF_KEY => found.links += records(InodeRefIter::new(&payload)),
            INODE_EXTREF_KEY => found.links += records(InodeExtrefIter::new(&payload)),
            DIR_ITEM_KEY => {
                for entry in DirItemIter::new(&payload, DIR_ITEM_KEY) {
                    let entry = entry.unwrap();
                    assert_eq!(
                        name_hash(entry.name),
                        key.offset,
                        "dir item {key:?} filed under its hash"
                    );
                    found.dir_names += entry.name.len() as u64;
                }
            }
            DIR_INDEX_KEY => {
                let entry = DirItemIter::new(&payload, DIR_INDEX_KEY)
                    .next()
                    .unwrap()
                    .unwrap();
                found.index_names += entry.name.len() as u64;
            }
            EXTENT_DATA_KEY => survey_extent(found, &mut out.need_sums, &key, &payload, sectorsize),
            _ => {}
        }
    }
    out
}

/// Check every inode of the fs tree, and the checksum tree against the data
/// extents: `data` holds every data extent the extent tree records.
pub(super) fn check_files(volume: &mut WriteVolume<MemDevice>, data: &RangeSet) {
    let Survey {
        inodes,
        orphans,
        need_sums,
    } = survey(volume);
    for (&ino, found) in &inodes {
        if ino < 256 {
            continue;
        }
        let item = found
            .item
            .unwrap_or_else(|| panic!("inode {ino}: items but no INODE_ITEM"));
        // No fixture has NODATASUM files; the checksum rules below would
        // have to learn about them.
        assert_eq!(item.flags & INODE_NODATASUM, 0, "inode {ino} is NODATASUM");
        if item.is_dir() {
            assert_eq!(
                found.dir_names, found.index_names,
                "dir {ino}: DIR_ITEM and DIR_INDEX names"
            );
            assert_eq!(item.size, 2 * found.index_names, "dir {ino}: isize");
            assert!(item.nlink <= 1, "dir {ino}: nlink {}", item.nlink);
        } else {
            assert_eq!(
                found.dir_names + found.index_names,
                0,
                "file {ino} holds entries"
            );
            if item.nlink > 0 {
                assert_eq!(item.nbytes, found.extent_bytes, "inode {ino}: nbytes");
            }
            assert_eq!(
                item.nlink, found.links,
                "inode {ino}: nlink against its back-references"
            );
        }
        assert_eq!(
            item.nlink == 0,
            orphans.contains(&ino),
            "inode {ino}: orphan item iff no links"
        );
    }
    check_sums(volume, &need_sums, data);
}

/// Every sector of every regular extent has a checksum, and no checksum lies
/// outside a data extent.
fn check_sums(volume: &mut WriteVolume<MemDevice>, need_sums: &RangeSet, data: &RangeSet) {
    let sector = u64::from(volume.sectorsize());
    let mut sums = RangeSet::new();
    for (key, payload) in items(volume, CSUM_TREE_OBJECTID) {
        assert_eq!(key.item_type, EXTENT_CSUM_KEY);
        let len = payload.len() as u64 / 4 * sector;
        assert!(
            sums.insert(key.offset, len),
            "checksum items overlap at {:#x}",
            key.offset
        );
    }
    for (at, len) in sums.iter() {
        assert!(
            data.contains(at, len),
            "checksums at {at:#x} outside every data extent"
        );
    }
    let missing: Vec<(u64, u64)> = need_sums
        .iter()
        .filter(|&(at, len)| !sums.contains(at, len))
        .collect();
    assert!(missing.is_empty(), "data without checksums: {missing:x?}");
}
