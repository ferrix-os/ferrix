//! The log tree, and the crash it exists for.
//!
//! Each test writes a file, logs it, and then throws the volume away without
//! committing — which is what a power cut does — and opens the bytes again.
//! What the log promised must be there, the trees must be consistent, and
//! nothing the log left behind may still be lying about.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_btrfs::items::{FS_TREE_OBJECTID, S_IFREG, Timespec};

use super::{BLANK, MemDevice, Op, check, items};
use crate::log::TREE_LOG_OBJECTID;
use crate::{NewInode, WriteVolume};

const ROOT: u64 = 256;
const NOW: Timespec = Timespec {
    sec: 1_790_000_000,
    nsec: 5,
};

/// `len` bytes that are this file's and no other's.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|at| (at as u8).wrapping_mul(31) ^ seed)
        .collect()
}

/// A volume with one file in it, committed, and the file's inode number.
fn with_a_file(data: &[u8]) -> (WriteVolume<MemDevice>, u64) {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let new = NewInode {
        mode: S_IFREG | 0o644,
        uid: 0,
        gid: 0,
        rdev: 0,
        now: NOW,
    };
    let ino = volume.create(ROOT, b"file", &new).unwrap();
    volume.write_file(ino, 0, data, data.len() as u64).unwrap();
    volume.commit().unwrap();
    (volume, ino)
}

/// Read a whole file back.
fn read_all(volume: &mut WriteVolume<MemDevice>, ino: u64) -> Vec<u8> {
    let size = volume.inode(ino).unwrap().unwrap().size as usize;
    let mut out = vec![0u8; size];
    let read = volume.read_file(ino, 0, &mut out).unwrap();
    out.truncate(read);
    out
}

/// Verifies: L.btrfs.15
#[test]
fn what_a_log_promised_survives_a_crash() {
    let first = pattern(40_000, 1);
    let (mut volume, ino) = with_a_file(&first);
    // A second write, logged but never committed: the transaction holding it
    // is lost, and only the log says where its bytes are.
    let second = pattern(60_000, 2);
    volume
        .write_file(ino, 0, &second, second.len() as u64)
        .unwrap();
    volume.log_inode(ino).unwrap();
    volume.commit_log().unwrap();
    let crashed = volume.into_device();

    // The mount after the crash: the log is replayed, and what it said is
    // what the file holds.
    let mut after = WriteVolume::open(crashed).unwrap();
    assert!(!after.has_log(), "the replay cleared the log");
    assert_eq!(
        read_all(&mut after, ino),
        second,
        "the logged write is there"
    );
    check(&after.device);
}

/// Verifies: L.btrfs.15
#[test]
fn a_commit_after_a_log_leaves_no_log_behind() {
    let (mut volume, ino) = with_a_file(&pattern(9000, 3));
    let data = pattern(12_000, 4);
    volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
    volume.log_inode(ino).unwrap();
    volume.commit_log().unwrap();
    assert!(volume.has_log());
    volume.commit().unwrap();
    assert!(!volume.has_log(), "the commit dropped the log");
    // The superblock the commit wrote names no log, so a mount replays
    // nothing and the file is simply there.
    let mut after = WriteVolume::open(volume.into_device()).unwrap();
    assert_eq!(read_all(&mut after, ino), data);
    check(&after.device);
}

#[test]
fn a_log_costs_less_than_a_commit() {
    let (mut volume, ino) = with_a_file(&pattern(5000, 5));
    let data = pattern(5000, 6);
    volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
    volume.device.log.clear();
    volume.log_inode(ino).unwrap();
    volume.commit_log().unwrap();
    let logged = volume.device.log.len();

    let (mut volume, ino) = with_a_file(&pattern(5000, 5));
    let data = pattern(5000, 6);
    volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
    volume.device.log.clear();
    volume.commit().unwrap();
    let committed = volume.device.log.len();
    assert!(
        logged < committed,
        "a log commit wrote {logged} things and a commit {committed}"
    );
}

/// Verifies: L.btrfs.15
#[test]
fn a_log_commit_flushes_before_its_superblock() {
    let (mut volume, ino) = with_a_file(&pattern(5000, 7));
    let data = pattern(7000, 8);
    volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
    volume.device.log.clear();
    volume.log_inode(ino).unwrap();
    volume.commit_log().unwrap();
    let log = &volume.device.log;
    let superblock = log
        .iter()
        .position(
            |op| matches!(op, Op::Write(at, _) if *at == ferrix_btrfs::superblock::PRIMARY_OFFSET),
        )
        .expect("a log commit writes the primary superblock");
    assert!(
        matches!(log.get(superblock.wrapping_sub(1)), Some(Op::Flush)),
        "the blocks the log names are durable before the superblock names them"
    );
}

/// Verifies: L.btrfs.15
#[test]
fn a_crash_between_the_log_and_its_superblock_leaves_the_last_commit() {
    let first = pattern(20_000, 9);
    let (mut volume, ino) = with_a_file(&first);
    let base = volume.device.clone();
    let second = pattern(30_000, 10);
    volume
        .write_file(ino, 0, &second, second.len() as u64)
        .unwrap();
    volume.log_inode(ino).unwrap();
    volume.commit_log().unwrap();
    // Replay every write except the superblock: the log is on the disk, but
    // nothing points at it.
    let mut crashed = base;
    for op in &volume.device.log {
        if let Op::Write(at, data) = op
            && *at != ferrix_btrfs::superblock::PRIMARY_OFFSET
        {
            crashed.store(*at, data);
        }
    }
    let mut after = WriteVolume::open(crashed).unwrap();
    assert_eq!(read_all(&mut after, ino), first, "the last commit, whole");
    check(&after.device);
}

/// Verifies: L.btrfs.15
#[test]
fn logging_the_same_inode_twice_keeps_the_later_of_the_two() {
    let (mut volume, ino) = with_a_file(&pattern(4000, 11));
    let middle = pattern(8000, 12);
    volume
        .write_file(ino, 0, &middle, middle.len() as u64)
        .unwrap();
    volume.log_inode(ino).unwrap();
    let last = pattern(3000, 13);
    volume.truncate(ino, 0).unwrap();
    volume.write_file(ino, 0, &last, last.len() as u64).unwrap();
    volume.log_inode(ino).unwrap();
    volume.commit_log().unwrap();
    let mut after = WriteVolume::open(volume.into_device()).unwrap();
    assert_eq!(read_all(&mut after, ino), last);
    check(&after.device);
}

/// Verifies: L.btrfs.15
#[test]
fn an_extent_cut_in_two_replays_as_one_extent_twice_referred_to() {
    let whole = pattern(60_000, 15);
    let (mut volume, ino) = with_a_file(&whole);
    // An overwrite in the middle cuts the extent into three pieces, two of
    // which name the original extent at different offsets into it.
    let patch = pattern(8192, 16);
    let mut expected = whole;
    expected
        .get_mut(16_384..24_576)
        .unwrap()
        .copy_from_slice(&patch);
    volume
        .write_file(ino, 16_384, &patch, expected.len() as u64)
        .unwrap();
    volume.log_inode(ino).unwrap();
    volume.commit_log().unwrap();
    let mut after = WriteVolume::open(volume.into_device()).unwrap();
    assert_eq!(read_all(&mut after, ino), expected);
    check(&after.device);
}

#[test]
fn the_log_tree_is_a_tree_of_its_own() {
    let (mut volume, ino) = with_a_file(&pattern(6000, 14));
    volume.log_inode(ino).unwrap();
    // Everything logged is about the one inode, and it is not in the fs tree
    // under the log's id.
    let logged = items(&mut volume, TREE_LOG_OBJECTID);
    assert!(!logged.is_empty(), "the log holds the inode's items");
    assert!(
        logged.iter().all(|(key, _)| key.objectid == ino
            || key.objectid == ferrix_btrfs::items::EXTENT_CSUM_OBJECTID),
        "the log holds nothing but that inode and its checksums"
    );
    assert!(
        items(&mut volume, FS_TREE_OBJECTID)
            .iter()
            .all(|(key, _)| key.objectid != TREE_LOG_OBJECTID),
        "the log is not in the fs tree"
    );
}
