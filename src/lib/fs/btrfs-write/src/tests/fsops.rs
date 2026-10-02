//! File operations, as a VFS would make them, checked three ways: read back
//! through the stage 11 reader, the consistency check, and — for the images
//! [`images`] returns — host `btrfs check`.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_btrfs::chunk::ChunkMapEntry;
use ferrix_btrfs::compress::MAX_UNCOMPRESSED;
use ferrix_btrfs::compress::zstd::Workspace;
use ferrix_btrfs::fs::{ReadBuffers, Target};
use ferrix_btrfs::items::{
    FS_TREE_OBJECTID, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, Timespec,
};
use ferrix_btrfs::tree::BtrfsKey;
use ferrix_btrfs::volume::Volume;

use super::{BLANK, MemDevice, POPULATED, Rng, check};
use crate::{NewInode, WriteVolume};

/// The zstd fixture: the populated tree with compressed extents.
const ZSTD: &[u8] = include_bytes!("../../../btrfs/testdata/zstd.img.packed");

const ROOT: u64 = 256;
const NOW: Timespec = Timespec {
    sec: 1_790_000_000,
    nsec: 5,
};

fn new(mode: u32) -> NewInode {
    NewInode {
        mode,
        uid: 1000,
        gid: 100,
        rdev: 0,
        now: NOW,
    }
}

/// Bytes nobody would write by accident, different for each `seed`.
fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next() as u8).collect()
}

/// Write `bytes` at any `offset` the way a page cache writes back: whole
/// sectors, read first where the write covers only part of one.
fn write_bytes(volume: &mut WriteVolume<MemDevice>, ino: u64, offset: u64, bytes: &[u8]) {
    let sector = u64::from(volume.sectorsize());
    let size = volume.inode(ino).unwrap().unwrap().size;
    let end = offset + bytes.len() as u64;
    let first = offset - offset % sector;
    let new_size = size.max(end);
    let last = end
        .next_multiple_of(sector)
        .min(new_size.next_multiple_of(sector));
    let mut buf = vec![0u8; (last - first) as usize];
    let _ = volume.read_file(ino, first, &mut buf).unwrap();
    buf[(offset - first) as usize..(end - first) as usize].copy_from_slice(bytes);
    let keep = (new_size.min(last) - first) as usize;
    volume
        .write_file(ino, first, &buf[..keep], new_size)
        .unwrap();
}

/// What a path should hold after a scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expect {
    File(Vec<u8>),
    Dir,
    Link(Vec<u8>),
    Gone,
}

/// Read `path` back through `ferrix-btrfs`, which knows nothing of this
/// crate, and compare with what the scenario says it should be.
fn read_back(device: &MemDevice, expected: &BTreeMap<&'static str, Expect>) {
    let mut device = device.clone();
    let mut node = vec![0u8; 65536];
    let volume = Volume::open(&mut device, vec![ChunkMapEntry::EMPTY; 256], &mut node).unwrap();
    let subvolume = volume.default_subvolume();
    let (mut a, mut b, mut c, mut d) = (
        vec![0u8; MAX_UNCOMPRESSED],
        vec![0u8; MAX_UNCOMPRESSED],
        vec![0u8; Workspace::SIZE],
        vec![0u8; 65536],
    );
    let mut buffers = ReadBuffers::new((&mut a[..], &mut b[..], &mut c[..], &mut d[..])).unwrap();
    for (path, want) in expected {
        let mut ino = subvolume.root_dir();
        let mut found = true;
        for part in path.split('/') {
            match subvolume
                .lookup(&mut device, ino, part.as_bytes(), &mut node)
                .unwrap()
            {
                Some(entry) => match entry.target {
                    Target::Inode(next) => ino = next,
                    Target::Subvolume(_) => panic!("{path}: crosses a subvolume"),
                },
                None => {
                    found = false;
                    break;
                }
            }
        }
        if *want == Expect::Gone {
            assert!(!found, "{path} should be gone");
            continue;
        }
        assert!(found, "{path} should exist");
        let inode = subvolume
            .inode(&mut device, ino, &mut node)
            .unwrap()
            .unwrap();
        let got = match want {
            Expect::Dir => {
                assert!(inode.is_dir(), "{path} is a directory");
                continue;
            }
            Expect::File(_) => {
                assert!(inode.is_file(), "{path} is a file");
                let mut out = vec![0u8; inode.size as usize];
                let n = subvolume
                    .read(&mut device, ino, 0, &mut out, &mut node, &mut buffers)
                    .unwrap();
                out.truncate(n);
                Expect::File(out)
            }
            Expect::Link(_) => {
                assert!(inode.is_symlink(), "{path} is a symlink");
                let mut out = vec![0u8; inode.size as usize];
                let n = subvolume
                    .read(&mut device, ino, 0, &mut out, &mut node, &mut buffers)
                    .unwrap();
                out.truncate(n);
                Expect::Link(out)
            }
            Expect::Gone => continue,
        };
        assert!(got == *want, "{path}: contents differ");
    }
}

/// Look up a path with the writer.
fn resolve(volume: &mut WriteVolume<MemDevice>, path: &str) -> u64 {
    path.split('/').fold(ROOT, |dir, part| {
        volume.lookup(dir, part.as_bytes()).unwrap().unwrap().0
    })
}

/// A tree of every kind of object, on the blank volume: nested directories,
/// files around every size boundary — empty, inline, the inline limit, one
/// sector, many sectors, and one larger than the 8 MiB data chunk `mkfs`
/// made, so chunks are allocated — a symlink, a hard link, a FIFO and a
/// device node.
fn build(expected: &mut BTreeMap<&'static str, Expect>) -> WriteVolume<MemDevice> {
    let mut volume = WriteVolume::open(MemDevice::new(BLANK)).unwrap();
    let a = volume.create(ROOT, b"a", &new(S_IFDIR | 0o755)).unwrap();
    let b = volume.create(a, b"b", &new(S_IFDIR | 0o700)).unwrap();
    let _c = volume.create(b, b"c", &new(S_IFDIR | 0o755)).unwrap();
    let _ = expected.insert("a", Expect::Dir);
    let _ = expected.insert("a/b", Expect::Dir);
    let _ = expected.insert("a/b/c", Expect::Dir);
    let sizes: [(&'static str, usize); 8] = [
        ("empty", 0),
        ("small", 100),
        ("edge", 2048),
        ("mid", 3000),
        ("page", 4096),
        ("a/b/c/deep", 70_000),
        ("big", 1_000_000),
        ("huge", 20 * 1024 * 1024),
    ];
    for (index, (path, size)) in sizes.iter().enumerate() {
        let (dir, name) = match path.rsplit_once('/') {
            Some((dir, name)) => (resolve(&mut volume, dir), name),
            None => (ROOT, *path),
        };
        let ino = volume
            .create(dir, name.as_bytes(), &new(S_IFREG | 0o644))
            .unwrap();
        let data = pattern(*size, index as u64 + 10);
        if !data.is_empty() {
            volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
        }
        let _ = expected.insert(path, Expect::File(data));
    }
    let link = volume.create(ROOT, b"link", &new(S_IFLNK | 0o777)).unwrap();
    volume.set_symlink(link, b"a/b/c/deep").unwrap();
    let _ = expected.insert("link", Expect::Link(b"a/b/c/deep".to_vec()));
    let small = resolve(&mut volume, "small");
    volume.link(a, b"hard", small, NOW).unwrap();
    let _ = expected.insert("a/hard", expected["small"].clone());
    let _ = volume.create(ROOT, b"fifo", &new(S_IFIFO | 0o600)).unwrap();
    let dev = NewInode {
        rdev: (4 << 20) | 64,
        ..new(S_IFCHR | 0o620)
    };
    let _ = volume.create(ROOT, b"tty", &dev).unwrap();
    volume.commit().unwrap();
    volume
}

/// Edits on the built tree: overwrites that split extents, truncation down
/// and up, a sparse write, unlinks — of a hard link, of a file's last name,
/// of an empty directory — and renames within and across directories, one
/// replacing an existing name.
fn edit(volume: &mut WriteVolume<MemDevice>, expected: &mut BTreeMap<&'static str, Expect>) {
    let file = |expected: &BTreeMap<&'static str, Expect>, path: &str| match &expected[path] {
        Expect::File(data) => data.clone(),
        _ => panic!("{path} is not a file"),
    };
    // Overwrite the middle of a multi-extent file: its extent is cut in
    // three, the middle replaced.
    let big = resolve(volume, "big");
    let mut data = file(expected, "big");
    let patch = pattern(10_000, 99);
    data[8192..18_192].copy_from_slice(&patch);
    write_bytes(volume, big, 8192, &patch);
    let _ = expected.insert("big", Expect::File(data));
    // Truncate down, then grow with a write far past the end: a hole.
    let deep = resolve(volume, "a/b/c/deep");
    let mut data = file(expected, "a/b/c/deep");
    data.truncate(5000);
    volume.truncate(deep, 5000).unwrap();
    let tail = pattern(3000, 7);
    write_bytes(volume, deep, 100_000, &tail);
    data.resize(100_000, 0);
    data.extend_from_slice(&tail);
    let _ = expected.insert("a/b/c/deep", Expect::File(data));
    // Shrink an inline file, and grow one past the inline limit.
    let edge = resolve(volume, "edge");
    let mut data = file(expected, "edge");
    data.truncate(10);
    volume.truncate(edge, 10).unwrap();
    let _ = expected.insert("edge", Expect::File(data));
    let small = resolve(volume, "small");
    let mut data = file(expected, "small");
    let more = pattern(9000, 3);
    write_bytes(volume, small, 100, &more);
    data.extend_from_slice(&more);
    let _ = expected.insert("small", Expect::File(data.clone()));
    let _ = expected.insert("a/hard", Expect::File(data));
    // One name of two goes; then a file's only name, which frees its data.
    let _ = volume.unlink(ROOT, b"small", NOW).unwrap();
    let _ = expected.insert("small", Expect::Gone);
    let page = volume.unlink(ROOT, b"page", NOW).unwrap();
    volume.evict(page).unwrap();
    let _ = expected.insert("page", Expect::Gone);
    // Renames: within a directory, across directories, and over a name.
    let a = resolve(volume, "a");
    volume.rename(ROOT, b"mid", a, b"mid2", NOW).unwrap();
    let _ = expected.insert("a/mid2", expected["mid"].clone());
    let _ = expected.insert("mid", Expect::Gone);
    let victim = resolve(volume, "a/hard");
    volume.rename(ROOT, b"huge", a, b"hard", NOW).unwrap();
    volume.evict(victim).unwrap();
    let _ = expected.insert("a/hard", expected["huge"].clone());
    let _ = expected.insert("huge", Expect::Gone);
    let b = resolve(volume, "a/b");
    volume.rename(b, b"c", ROOT, b"moved-c", NOW).unwrap();
    let _ = expected.insert("moved-c", Expect::Dir);
    let _ = expected.insert("moved-c/deep", expected["a/b/c/deep"].clone());
    let _ = expected.insert("a/b/c", Expect::Gone);
    let _ = expected.remove("a/b/c/deep");
    // An empty directory goes.
    let gone = volume.unlink(a, b"b", NOW).unwrap();
    volume.evict(gone).unwrap();
    let _ = expected.insert("a/b", Expect::Gone);
    volume.commit().unwrap();
}

/// Verifies: L.btrfs.17
#[test]
fn a_built_tree_reads_back_and_checks_clean() {
    let mut expected = BTreeMap::new();
    let volume = build(&mut expected);
    check(&volume.device);
    read_back(&volume.device, &expected);
}

/// Verifies: L.btrfs.17
#[test]
fn edits_read_back_and_check_clean() {
    let mut expected = BTreeMap::new();
    let mut volume = build(&mut expected);
    edit(&mut volume, &mut expected);
    check(&volume.device);
    read_back(&volume.device, &expected);
    // The same, reopened: nothing depended on what was in memory.
    let reopened = WriteVolume::open(volume.into_device()).unwrap();
    read_back(&reopened.device, &expected);
}

#[test]
fn an_orphan_survives_a_commit_and_goes_at_the_next_open() {
    let mut expected = BTreeMap::new();
    let mut volume = build(&mut expected);
    let big = volume.unlink(ROOT, b"big", NOW).unwrap();
    volume.commit().unwrap();
    check(&volume.device);
    assert_eq!(volume.orphans().unwrap(), [big]);
    let mut reopened = WriteVolume::open(volume.into_device()).unwrap();
    assert!(
        reopened.orphans().unwrap().is_empty(),
        "the open evicted it"
    );
    reopened.commit().unwrap();
    check(&reopened.device);
    assert!(reopened.inode(big).unwrap().is_none());
}

/// Overwrite part of every compressed extent of `big.txt`, which splits
/// them, and delete the incompressible file, on the zstd fixture.
fn edit_compressed() -> (MemDevice, BTreeMap<&'static str, Expect>) {
    let mut volume = WriteVolume::open(MemDevice::new(ZSTD)).unwrap();
    let mut expected = BTreeMap::new();
    let big = resolve(&mut volume, "big.txt");
    let size = volume.inode(big).unwrap().unwrap().size as usize;
    let mut original = vec![0u8; size];
    let mut device = volume.device.clone();
    let mut node = vec![0u8; 65536];
    let reader = Volume::open(&mut device, vec![ChunkMapEntry::EMPTY; 256], &mut node).unwrap();
    let (mut a, mut b, mut c, mut d) = (
        vec![0u8; MAX_UNCOMPRESSED],
        vec![0u8; MAX_UNCOMPRESSED],
        vec![0u8; Workspace::SIZE],
        vec![0u8; 65536],
    );
    let mut buffers = ReadBuffers::new((&mut a[..], &mut b[..], &mut c[..], &mut d[..])).unwrap();
    let n = reader
        .default_subvolume()
        .read(&mut device, big, 0, &mut original, &mut node, &mut buffers)
        .unwrap();
    assert_eq!(n, size);
    let mut data = original.clone();
    for at in [4096usize, 131_072] {
        let patch = pattern(4096, at as u64);
        data[at..at + 4096].copy_from_slice(&patch);
        volume
            .write_file(big, at as u64, &patch, size as u64)
            .unwrap();
    }
    let _ = expected.insert("big.txt", Expect::File(data));
    let random = volume.unlink(ROOT, b"random.bin", NOW).unwrap();
    volume.evict(random).unwrap();
    let _ = expected.insert("random.bin", Expect::Gone);
    volume.commit().unwrap();
    (volume.into_device(), expected)
}

#[test]
fn compressed_extents_split_by_overwrites_read_back() {
    let (device, expected) = edit_compressed();
    check(&device);
    read_back(&device, &expected);
}

/// Many small files created and removed on the populated fixture, so its
/// fs tree grows and shrinks around what `mkfs.btrfs` wrote.
fn churn_populated() -> MemDevice {
    let mut volume = WriteVolume::open(MemDevice::new(POPULATED)).unwrap();
    let dir = volume
        .create(ROOT, b"churn", &new(S_IFDIR | 0o755))
        .unwrap();
    let mut rng = Rng::new(5);
    let mut live = Vec::new();
    for round in 0..600u32 {
        if rng.below(3) != 0 || live.is_empty() {
            let name = alloc::format!("f{round}");
            let ino = volume
                .create(dir, name.as_bytes(), &new(S_IFREG | 0o644))
                .unwrap();
            let data = pattern(rng.below(9000) as usize, u64::from(round));
            if !data.is_empty() {
                volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
            }
            live.push(name);
        } else {
            let name = live.swap_remove(rng.below(live.len() as u64) as usize);
            let ino = volume.unlink(dir, name.as_bytes(), NOW).unwrap();
            volume.evict(ino).unwrap();
        }
        if round % 150 == 149 {
            volume.commit().unwrap();
        }
    }
    volume.commit().unwrap();
    volume.into_device()
}

/// Verifies: L.btrfs.17
#[test]
fn churn_on_a_populated_volume_checks_clean() {
    check(&churn_populated());
}

/// Fill the fs tree of `packed` with scratch items across several commits,
/// then delete them all: the files are as `mkfs.btrfs` left them, and every
/// tree block, extent item and free-space item has been rewritten.
fn churn_items(packed: &[u8]) -> MemDevice {
    let mut volume = WriteVolume::open(MemDevice::new(packed)).unwrap();
    let keys: Vec<BtrfsKey> = (0..2000)
        .map(|i| BtrfsKey::new(100_000 + i, 250, 0))
        .collect();
    for chunk in keys.chunks(400) {
        for key in chunk {
            volume
                .insert(FS_TREE_OBJECTID, *key, vec![0x5a; 120])
                .unwrap();
        }
        volume.commit().unwrap();
    }
    for key in &keys {
        volume.delete(FS_TREE_OBJECTID, key).unwrap();
    }
    volume.commit().unwrap();
    volume.into_device()
}

/// Every image to check, by name.
pub(crate) fn images() -> Vec<(&'static str, MemDevice)> {
    let mut expected = BTreeMap::new();
    let built = build(&mut expected);
    let built_device = built.device.clone();
    let mut edited = built;
    edit(&mut edited, &mut expected);
    let mut orphaned = build(&mut BTreeMap::new());
    let _ = orphaned.unlink(ROOT, b"big", NOW).unwrap();
    orphaned.commit().unwrap();
    vec![
        ("churn-blank", churn_items(BLANK)),
        ("churn-populated", churn_items(POPULATED)),
        ("built", built_device),
        ("edited", edited.into_device()),
        ("orphan", orphaned.into_device()),
        ("compressed", edit_compressed().0),
        ("churn-files", churn_populated()),
        ("past-the-mirror", super::past_the_mirror()),
        ("grown-over-the-mirror", super::grown_over_the_mirror().0),
    ]
}
