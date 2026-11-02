//! The clean nodes: a walk keeps every node it loaded however full the cache
//! is (finding F-99), the cache comes back to its size between operations
//! oldest first, a reclaim gives up clean nodes and never a dirty one, and
//! the count of their bytes is exact.

use alloc::format;
use alloc::vec::Vec;

use ferrix_btrfs::items::{DIR_INDEX_KEY, FS_TREE_OBJECTID, S_IFDIR, S_IFIFO, Timespec};
use ferrix_btrfs::tree::BtrfsKey;

use super::{BLANK, MemDevice};
use crate::clean::CleanNodes;
use crate::node::{Body, TreeNode};
use crate::volume::{CLEAN_NODES, DROP_BATCH};
use crate::{NewInode, WriteVolume};

const ROOT: u64 = 256;
const FS: u64 = FS_TREE_OBJECTID;
const NOW: Timespec = Timespec {
    sec: 1_790_000_000,
    nsec: 7,
};
/// Entries in the wide directory: enough for its index items to fill
/// several leaves.
const ENTRIES: usize = 300;

fn new(mode: u32) -> NewInode {
    NewInode {
        mode,
        uid: 0,
        gid: 0,
        rdev: 0,
        now: NOW,
    }
}

/// The name of entry `i` of the wide directory: long, so that few index
/// items fill a leaf.
fn name(i: usize) -> Vec<u8> {
    format!("{i:048}").into_bytes()
}

/// A volume with one directory of [`ENTRIES`] FIFOs, committed, and the
/// directory's inode number.
fn wide() -> (WriteVolume<MemDevice>, u64) {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let dir = volume.create(ROOT, b"wide", &new(S_IFDIR | 0o755)).unwrap();
    for i in 0..ENTRIES {
        let _ = volume.create(dir, &name(i), &new(S_IFIFO | 0o644)).unwrap();
    }
    volume.commit().unwrap();
    (volume, dir)
}

/// The fs tree's leaves that start with an index item of `dir`, in key
/// order: each one's index. The tree must be one level of pointers over
/// its leaves.
fn index_leaves(volume: &mut WriteVolume<MemDevice>, dir: u64) -> Vec<u64> {
    let root = volume.root(FS).unwrap();
    assert_eq!(root.level, 1, "the wide directory's tree is two levels");
    volume
        .load(root.bytenr, root.level, root.generation)
        .unwrap();
    let Body::Internal(ptrs) = &volume.node(root.bytenr).unwrap().body else {
        panic!("the fs tree's root is not internal");
    };
    ptrs.iter()
        .filter(|ptr| ptr.key.objectid == dir && ptr.key.item_type == DIR_INDEX_KEY)
        .map(|ptr| ptr.key.offset)
        .collect()
}

/// Remove the entry of `dir` whose index is `index`, which must be the
/// first item of its leaf: a gap opens between that leaf and the one before.
fn unlink_index(volume: &mut WriteVolume<MemDevice>, dir: u64, index: u64) {
    let entries = volume.read_dir(dir, index).unwrap();
    let (found, _, _, gone) = entries.first().unwrap();
    assert_eq!(*found, index);
    let _ = volume.unlink(dir, gone, NOW).unwrap();
}

/// Point the fs tree's root at the leaf now starting at `index + 1` with
/// the key `index`, below the leaf's first item. Only a tree whose pointer
/// is below its leaf's first item has `prev_item` climb from slot 0 of a
/// leaf; this is one.
fn lower_pointer(volume: &mut WriteVolume<MemDevice>, dir: u64, index: u64) {
    let root = volume.cow_root(FS).unwrap();
    let Body::Internal(ptrs) = &mut volume.node_mut(root.bytenr).unwrap().body else {
        panic!("the fs tree's root is not internal");
    };
    let ptr = ptrs
        .iter_mut()
        .find(|ptr| ptr.key == BtrfsKey::new(dir, DIR_INDEX_KEY, index + 1))
        .unwrap();
    ptr.key = BtrfsKey::new(dir, DIR_INDEX_KEY, index);
}

/// The clean cache holding [`CLEAN_NODES`] nodes, the fs tree's root among
/// them and none of its leaves: the next load is the one that, before the
/// fix, cleared the cache and the root with it under the walk.
fn full_around_the_root(volume: &mut WriteVolume<MemDevice>) {
    volume.clean = CleanNodes::new();
    let root = volume.root(FS).unwrap();
    volume
        .load(root.bytenr, root.level, root.generation)
        .unwrap();
    let mut filler = u64::MAX - 0xffff;
    while volume.clean.len() < CLEAN_NODES {
        volume
            .clean
            .insert(filler, TreeNode::empty_leaf(filler, 1, 1))
            .unwrap();
        filler -= 0x1_0000;
    }
}

/// The wide volume with two gaps between leaves of the directory's index:
/// the first after a leaf whose last item is `before`, followed by the
/// leaf starting at `after`; the second with the pointer lowered as
/// [`lower_pointer`] does. Answers the directory, the first gap's index
/// and the second's.
fn gapped() -> (WriteVolume<MemDevice>, u64, u64, u64) {
    let (mut volume, dir) = wide();
    let leaves = index_leaves(&mut volume, dir);
    assert!(leaves.len() >= 4, "the index fills {} leaves", leaves.len());
    let (first, second) = (leaves[1], leaves[2]);
    unlink_index(&mut volume, dir, first);
    unlink_index(&mut volume, dir, second);
    lower_pointer(&mut volume, dir, second);
    volume.commit().unwrap();
    (volume, dir, first, second)
}

/// `next_item` from a key in the gap between two leaves climbs to the next
/// leaf, with the cache full when it loads the first.
///
/// Verifies: L.btrfs.119
#[test]
fn next_item_crosses_a_leaf_with_the_cache_full() {
    let (mut volume, dir, gap, _) = gapped();
    full_around_the_root(&mut volume);
    let found = volume
        .next_item(FS, &BtrfsKey::new(dir, DIR_INDEX_KEY, gap))
        .unwrap()
        .map(|(key, _)| key);
    assert_eq!(found, Some(BtrfsKey::new(dir, DIR_INDEX_KEY, gap + 1)));
}

/// `prev_item` from slot 0 of a leaf climbs to the leaf before, with the
/// cache full when it loads the first.
///
/// Verifies: L.btrfs.119
#[test]
fn prev_item_crosses_a_leaf_with_the_cache_full() {
    let (mut volume, dir, _, gap) = gapped();
    full_around_the_root(&mut volume);
    let found = volume
        .prev_item(FS, &BtrfsKey::new(dir, DIR_INDEX_KEY, gap))
        .unwrap()
        .map(|(key, _)| key);
    assert_eq!(found, Some(BtrfsKey::new(dir, DIR_INDEX_KEY, gap - 1)));
}

/// `read_dir` from the gap answers every entry after it, with the cache
/// full when it loads the first leaf: it once answered none.
///
/// Verifies: L.btrfs.119
#[test]
fn read_dir_answers_every_entry_with_the_cache_full() {
    let (mut volume, dir, gap, second) = gapped();
    let want: Vec<u64> = volume
        .read_dir(dir, 0)
        .unwrap()
        .iter()
        .map(|entry| entry.0)
        .filter(|&index| index > gap)
        .collect();
    assert_eq!(want.len(), ENTRIES - usize::try_from(gap).unwrap());
    assert!(!want.contains(&second));
    full_around_the_root(&mut volume);
    let got: Vec<u64> = volume
        .read_dir(dir, gap)
        .unwrap()
        .iter()
        .map(|entry| entry.0)
        .collect();
    assert_eq!(got, want);
}

/// Between operations the cache comes back to [`CLEAN_NODES`]: the oldest
/// go, the newest stay, and the running transaction's nodes are untouched.
///
/// Verifies: L.btrfs.119
#[test]
fn trimming_drops_the_oldest_and_no_dirty_node() {
    let (mut volume, dir) = wide();
    let _ = volume.create(dir, b"dirty", &new(S_IFIFO | 0o644)).unwrap();
    let dirty: Vec<u64> = volume.dirty.keys().copied().collect();
    assert!(!dirty.is_empty());
    let mut filler = u64::MAX - 0xffff;
    let mut fillers = Vec::new();
    for _ in 0..CLEAN_NODES + 10 {
        volume
            .clean
            .insert(filler, TreeNode::empty_leaf(filler, 1, 1))
            .unwrap();
        fillers.push(filler);
        filler -= 0x1_0000;
    }
    volume.trim_clean();
    assert_eq!(volume.clean.len(), CLEAN_NODES);
    // Every node loaded before the fillers went first, then the oldest
    // fillers: the newest CLEAN_NODES fillers are what is left.
    let kept = fillers.split_off(10);
    assert!(fillers.iter().all(|at| volume.clean.get(at).is_none()));
    assert!(kept.iter().all(|at| volume.clean.get(at).is_some()));
    assert_eq!(volume.dirty.keys().copied().collect::<Vec<_>>(), dirty);
    assert_eq!(volume.clean_bytes(), volume.clean.recount());
}

/// A reclaim's drop gives up at most [`DROP_BATCH`] clean nodes a call,
/// counts their bytes exactly, and leaves every node of the running
/// transaction: what was made before it is there after the commit.
///
/// Verifies: L.btrfs.119, L.btrfs.121
#[test]
fn dropping_clean_nodes_leaves_the_transaction() {
    let (mut volume, dir) = wide();
    let _ = volume.read_dir(dir, 0).unwrap();
    let made = volume.create(dir, b"kept", &new(S_IFIFO | 0o644)).unwrap();
    let dirty = volume.dirty.len();
    assert!(dirty > 0);
    assert!(volume.clean.len() > DROP_BATCH);
    loop {
        let (nodes, bytes) = (volume.clean.len(), volume.clean_bytes());
        let given = volume.drop_clean(u64::MAX);
        assert_eq!(volume.clean_bytes(), bytes - given);
        assert_eq!(volume.clean_bytes(), volume.clean.recount());
        assert!(nodes - volume.clean.len() <= DROP_BATCH);
        assert_eq!(volume.dirty.len(), dirty, "a dirty node was dropped");
        if given == 0 {
            break;
        }
    }
    assert_eq!(volume.clean.len(), 0);
    assert_eq!(volume.clean_bytes(), 0);
    volume.commit().unwrap();
    let mut volume = WriteVolume::open(volume.into_device()).unwrap();
    assert_eq!(
        volume.lookup(dir, b"kept").unwrap().map(|found| found.0),
        Some(made)
    );
}

/// A drop asked for a few bytes gives one node and stops.
///
/// Verifies: L.btrfs.121
#[test]
fn dropping_stops_at_what_was_asked() {
    let (mut volume, _) = wide();
    let before = volume.clean.len();
    let given = volume.drop_clean(1);
    assert!(given > 0);
    assert_eq!(volume.clean.len(), before - 1);
}

/// The count follows loads, copies and commits: after a commit, a read of
/// the whole directory and an edit, it is the sum over the nodes held.
///
/// Verifies: L.btrfs.121
#[test]
fn the_clean_count_follows_every_change() {
    let (mut volume, dir) = wide();
    assert_eq!(volume.clean_bytes(), volume.clean.recount());
    volume.clean = CleanNodes::new();
    let _ = volume.read_dir(dir, 0).unwrap();
    assert!(volume.clean_bytes() > 0);
    assert_eq!(volume.clean_bytes(), volume.clean.recount());
    let _ = volume.unlink(dir, &name(7), NOW).unwrap();
    assert_eq!(volume.clean_bytes(), volume.clean.recount());
    volume.commit().unwrap();
    assert_eq!(volume.clean_bytes(), volume.clean.recount());
}
