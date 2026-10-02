//! Tests for the mount, through the [`Inode`] trait the VFS calls.
//!
//! The images are the real `mkfs.btrfs` ones `ferrix-btrfs` tests with, and the
//! expectations are its manifest: every path resolves one component at a time,
//! every file reads back with the right size and CRC-32C, every link's target
//! does, and every directory lists exactly its children — in pieces, resuming
//! from the cursor each piece ended at, the way `getdents64` asks.

extern crate std;

use std::collections::{BTreeMap, BTreeSet};
use std::string::String;
use std::thread;

use ferrix_btrfs::crc32c;
use ferrix_vfs::tmpfs::HeapStorage;

use super::*;

/// Heap pages, as the kernel's VMO pages stand in on the host.
fn heap() -> Arc<dyn Storage> {
    Arc::new(HeapStorage::new(1 << 30))
}
use ferrix_btrfs::volume::ReadKind;

mod namespace;
mod rw;

const BLOCK: usize = 4096;
const IMAGE_SIZE: u64 = 128 * 1024 * 1024;

const IMAGES: [(&str, &[u8]); 4] = [
    (
        "none",
        include_bytes!("../../btrfs/testdata/none.img.packed"),
    ),
    (
        "zlib",
        include_bytes!("../../btrfs/testdata/zlib.img.packed"),
    ),
    ("lzo", include_bytes!("../../btrfs/testdata/lzo.img.packed")),
    (
        "zstd",
        include_bytes!("../../btrfs/testdata/zstd.img.packed"),
    ),
];

/// A packed image as a device handle: shared blocks, zeroes elsewhere.
#[derive(Clone)]
struct Image(Arc<BTreeMap<u64, [u8; BLOCK]>>);

impl Image {
    fn new(packed: &[u8]) -> Image {
        let blocks = packed
            .chunks_exact(8 + BLOCK)
            .map(|record| {
                let offset = u64::from_le_bytes(record[..8].try_into().unwrap());
                (offset, record[8..].try_into().unwrap())
            })
            .collect();
        Image(Arc::new(blocks))
    }
}

impl Device for Image {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        _kind: ReadKind,
    ) -> core::result::Result<(), BtrfsError> {
        if physical
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > IMAGE_SIZE)
        {
            return Err(BtrfsError::DeviceRead { physical });
        }
        let mut done = 0;
        while done < buf.len() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            let within = (at - base) as usize;
            let len = (BLOCK - within).min(buf.len() - done);
            let dest = &mut buf[done..done + len];
            match self.0.get(&base) {
                Some(block) => dest.copy_from_slice(&block[within..within + len]),
                None => dest.fill(0),
            }
            done += len;
        }
        Ok(())
    }
}

struct Expected {
    kind: &'static str,
    size: u64,
    crc: u32,
}

fn manifest() -> BTreeMap<Vec<u8>, Expected> {
    include_str!("../../btrfs/testdata/manifest.txt")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let fields: Vec<&'static str> = line.split(' ').collect();
            let path = fields[1]
                .as_bytes()
                .chunks(2)
                .map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect();
            let expected = Expected {
                kind: fields[0],
                size: fields[2].parse().unwrap(),
                crc: u32::from_str_radix(fields[3], 16).unwrap(),
            };
            (path, expected)
        })
        .collect()
}

fn mount(name: &str) -> Arc<Btrfs<Image>> {
    let (_, packed) = IMAGES.iter().find(|(n, _)| *n == name).unwrap();
    Btrfs::mount(Image::new(packed), 42, heap()).unwrap()
}

fn resolve(fs: &Btrfs<Image>, path: &[u8]) -> Arc<dyn Inode> {
    path.split(|&b| b == b'/').fold(fs.root(), |dir, name| {
        dir.lookup(name)
            .unwrap_or_else(|e| panic!("{} missing: {e:?}", String::from_utf8_lossy(path)))
    })
}

/// Read all of a file in 3001-byte pieces, so reads straddle extent edges.
fn read_all(file: &Arc<dyn Inode>) -> Vec<u8> {
    let mut contents = Vec::new();
    let mut piece = vec![0xAAu8; 3001];
    loop {
        let n = file.read_at(contents.len() as u64, &mut piece).unwrap();
        if n == 0 {
            return contents;
        }
        contents.extend_from_slice(&piece[..n]);
    }
}

/// List a directory five entries at a time, resuming from each piece's cursor.
fn list(dir: &Arc<dyn Inode>) -> Vec<Vec<u8>> {
    let mut names = Vec::new();
    let mut cursor = FIRST_CURSOR;
    loop {
        let mut taken = 0;
        let mut resume = None;
        dir.read_dir(cursor, &mut |entry| {
            if taken == 5 {
                return false;
            }
            names.push(entry.name.to_vec());
            resume = Some(entry.next);
            taken += 1;
            true
        })
        .unwrap();
        match resume {
            Some(next) => cursor = next,
            None => return names,
        }
    }
}

fn children(manifest: &BTreeMap<Vec<u8>, Expected>, dir: &[u8]) -> BTreeSet<Vec<u8>> {
    manifest
        .keys()
        .filter_map(|path| {
            let rest = if dir.is_empty() {
                path.as_slice()
            } else {
                path.strip_prefix(dir)?.strip_prefix(b"/")?
            };
            (!rest.contains(&b'/')).then(|| rest.to_vec())
        })
        .collect()
}

fn check_listing(dir: &Arc<dyn Inode>, path: &[u8], manifest: &BTreeMap<Vec<u8>, Expected>) {
    let names = list(dir);
    let unique: BTreeSet<Vec<u8>> = names.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        names.len(),
        "no name is listed twice in {path:?}"
    );
    assert_eq!(unique, children(manifest, path), "the listing of {path:?}");
}

fn check_image(name: &str) {
    let fs = mount(name);
    assert_eq!(fs.name(), "btrfs", "the type /proc/mounts shows");
    let manifest = manifest();
    check_listing(&fs.root(), b"", &manifest);
    for (path, expected) in &manifest {
        let inode = resolve(&fs, path);
        let meta = inode.metadata();
        let shown = String::from_utf8_lossy(path);
        match expected.kind {
            "dir" => {
                assert_eq!(meta.kind, FileType::Directory, "{name}: {shown}");
                check_listing(&inode, path, &manifest);
            }
            "file" => {
                assert_eq!(meta.kind, FileType::Regular, "{name}: {shown}");
                assert_eq!(meta.size, expected.size, "{name}: {shown} stat size");
                let contents = read_all(&inode);
                assert_eq!(contents.len() as u64, expected.size, "{name}: {shown} size");
                assert_eq!(crc32c(&contents), expected.crc, "{name}: {shown} contents");
            }
            "link" => {
                assert_eq!(meta.kind, FileType::Symlink, "{name}: {shown}");
                let target = inode.read_link().unwrap();
                assert_eq!(
                    target.len() as u64,
                    expected.size,
                    "{name}: {shown} target size"
                );
                assert_eq!(crc32c(&target), expected.crc, "{name}: {shown} target");
            }
            other => panic!("unknown manifest type {other}"),
        }
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "reads every file of a real image; plain cargo test covers it, Miri runs the small image tests"
)]
fn the_uncompressed_image_mounts_and_reads_back() {
    check_image("none");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "reads every file of a real image; plain cargo test covers it, Miri runs the small image tests"
)]
fn the_zlib_image_mounts_and_reads_back() {
    check_image("zlib");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "reads every file of a real image; plain cargo test covers it, Miri runs the small image tests"
)]
fn the_lzo_image_mounts_and_reads_back() {
    check_image("lzo");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "reads every file of a real image; plain cargo test covers it, Miri runs the small image tests"
)]
fn the_zstd_image_mounts_and_reads_back() {
    check_image("zstd");
}

#[test]
fn every_change_is_refused_as_read_only() {
    let fs = mount("none");
    let root = fs.root();
    let file = resolve(&fs, b"big.txt");
    assert_eq!(
        root.create(b"new", NewNode::Regular, 0o644).err(),
        Some(Errno::EROFS),
        "create"
    );
    assert_eq!(root.unlink(b"big.txt"), Err(Errno::EROFS), "unlink");
    assert_eq!(root.rmdir(b"empty-dir"), Err(Errno::EROFS), "rmdir");
    assert_eq!(root.link(b"again", &file), Err(Errno::EROFS), "link");
    assert_eq!(
        root.rename(b"big.txt", &root, b"moved", true),
        Err(Errno::EROFS),
        "rename"
    );
    assert_eq!(file.write_at(0, b"x", false), Err(Errno::EROFS), "write");
    assert_eq!(file.set_len(0), Err(Errno::EROFS), "truncate");
    assert_eq!(
        file.set_attributes(&SetAttributes::default()),
        Err(Errno::EROFS),
        "chmod and friends"
    );
}

#[test]
fn the_wrong_kind_of_object_is_refused_as_linux_would() {
    let fs = mount("none");
    let file = resolve(&fs, b"big.txt");
    assert_eq!(
        fs.root().lookup(b"no-such-name").err(),
        Some(Errno::ENOENT),
        "a miss"
    );
    assert_eq!(
        file.lookup(b"x").err(),
        Some(Errno::ENOTDIR),
        "lookup in a file"
    );
    assert_eq!(
        file.read_dir(FIRST_CURSOR, &mut |_| true),
        Err(Errno::ENOTDIR),
        "list a file"
    );
    assert_eq!(
        fs.root().read_at(0, &mut [0; 8]),
        Err(Errno::EINVAL),
        "read a directory"
    );
    assert_eq!(file.read_link(), Err(Errno::EINVAL), "readlink on a file");
}

#[test]
fn a_device_without_btrfs_is_refused_with_einval() {
    let blank = Image(Arc::new(BTreeMap::new()));
    assert_eq!(
        Btrfs::mount(blank, 1, heap()).err(),
        Some(Errno::EINVAL),
        "no superblock magic"
    );
}

#[test]
fn inodes_report_the_mount_device_and_their_own_numbers() {
    let fs = mount("none");
    assert_eq!(fs.device(), 42, "the device number given at mount");
    let root = fs.root().metadata();
    assert_eq!(root.ino, 256, "btrfs's top-level directory");
    let a = resolve(&fs, b"big.txt").metadata().ino;
    let b = resolve(&fs, b"random.bin").metadata().ino;
    assert_ne!(a, b, "two files are two inodes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "reads every file of a real image; plain cargo test covers it, Miri runs the small image tests"
)]
fn concurrent_readers_of_one_compressed_file_each_get_its_bytes() {
    let fs = mount("zstd");
    let expected = read_all(&resolve(&fs, b"big.txt"));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let fs = Arc::clone(&fs);
            thread::spawn(move || read_all(&resolve(&fs, b"big.txt")))
        })
        .collect();
    for reader in readers {
        assert!(
            reader.join().unwrap() == expected,
            "every reader sees the same file"
        );
    }
}

#[test]
fn statfs_reports_btrfs_and_the_volume_size() {
    let fs = mount("none");
    let stat = fs.statfs();
    assert_eq!(stat.magic, BTRFS_SUPER_MAGIC, "the magic programs test for");
    assert_eq!(stat.block_size, 4096, "counts are in sectors");
    assert_eq!(
        stat.blocks * 4096,
        128 * 1024 * 1024,
        "the generator makes a 128 MiB image"
    );
    assert!(
        stat.blocks_free > 0 && stat.blocks_free < stat.blocks,
        "some of the volume is used and some is not"
    );
    assert_eq!(stat.name_max, 255, "btrfs's name limit");
}

#[test]
fn the_mount_shows_the_default_subvolume_not_the_top_level_tree() {
    let packed = include_bytes!("../../btrfs/testdata/default-subvol.img.packed");
    let fs = Btrfs::mount(Image::new(packed), 42, heap()).unwrap();
    assert_eq!(
        read_all(&resolve(&fs, b"marker")),
        b"in the default subvolume\n"
    );
    assert_eq!(
        read_all(&resolve(&fs, b"nested/file")),
        b"nested in the default subvolume\n"
    );
    assert!(
        matches!(fs.root().lookup(b"top-level-only"), Err(Errno::ENOENT)),
        "Linux mounts the default subvolume, so the top-level tree's file is not visible"
    );
}

/// An [`Image`] that counts the metadata and data reads it serves.
#[derive(Clone)]
struct Counting {
    image: Image,
    metadata: Arc<core::sync::atomic::AtomicU64>,
    data: Arc<core::sync::atomic::AtomicU64>,
}

impl Counting {
    fn new(packed: &[u8]) -> Counting {
        Counting {
            image: Image::new(packed),
            metadata: Arc::default(),
            data: Arc::default(),
        }
    }

    /// Metadata reads and data reads served so far.
    fn counts(&self) -> (u64, u64) {
        let ordering = core::sync::atomic::Ordering::SeqCst;
        (self.metadata.load(ordering), self.data.load(ordering))
    }
}

impl Device for Counting {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        kind: ReadKind,
    ) -> core::result::Result<(), BtrfsError> {
        let counter = match kind {
            ReadKind::Metadata => &self.metadata,
            ReadKind::Data => &self.data,
        };
        let _ = counter.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        self.image.read_at(physical, buf, kind)
    }
}

/// Resolve `path` from the root of a mount over any device.
fn resolve_in<D: BlockHandle>(fs: &Btrfs<D>, path: &[u8]) -> Arc<dyn Inode> {
    path.split(|&b| b == b'/').fold(fs.root(), |dir, name| {
        dir.lookup(name)
            .unwrap_or_else(|e| panic!("{} missing: {e:?}", String::from_utf8_lossy(path)))
    })
}

#[test]
fn a_second_walk_to_a_file_reads_no_metadata_from_the_device() {
    let device = Counting::new(IMAGES[0].1);
    let fs = Btrfs::mount(device.clone(), 42, heap()).unwrap();
    let path = b"dir03/sub0/file07.txt";
    let _ = resolve_in(&fs, path).metadata();
    let (metadata, _) = device.counts();
    assert!(metadata > 0, "the first walk reads nodes from the device");
    let _ = resolve_in(&fs, path).metadata();
    assert_eq!(
        device.counts().0,
        metadata,
        "every node the second walk needs comes from the cache"
    );
}

#[test]
fn file_data_is_read_from_the_device_once_and_then_from_the_page_cache() {
    let device = Counting::new(IMAGES[0].1);
    let fs = Btrfs::mount(device.clone(), 42, heap()).unwrap();
    let file = resolve_in(&fs, b"random.bin");
    let start = device.counts().1;
    let first = read_all(&file);
    let once = device.counts().1 - start;
    let second = read_all(&file);
    let twice = device.counts().1 - start - once;
    assert!(
        once > 0,
        "an uncompressed file's data comes from the device"
    );
    assert_eq!(twice, 0, "and never again: the page cache holds it");
    assert_eq!(first, second, "both reads return the same bytes");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "reads every file of a real image; plain cargo test covers it"
)]
fn a_cache_too_small_for_one_walk_still_reads_every_file_back() {
    let fs = Btrfs::mount_with(Image::new(IMAGES[0].1), 42, heap(), 2).unwrap();
    for (path, expected) in manifest() {
        if expected.kind != "file" {
            continue;
        }
        let contents = read_all(&resolve_in(&fs, &path));
        let shown = String::from_utf8_lossy(&path);
        assert_eq!(contents.len() as u64, expected.size, "{shown} size");
        assert_eq!(crc32c(&contents), expected.crc, "{shown} contents");
    }
}

#[test]
fn the_node_cache_is_bounded_and_gives_a_hit_entry_a_second_chance() {
    let cache = NodeCache::new(2);
    cache.insert(1, &[1; 4]);
    cache.insert(2, &[2; 4]);
    assert!(cache.get(1, 4).is_some(), "entry 1 is hit");
    cache.insert(3, &[3; 4]);
    assert_eq!(cache.stats().2, 2, "never more entries than the capacity");
    assert!(
        cache.get(2, 4).is_none(),
        "entry 2, never hit, was the one evicted"
    );
    assert_eq!(
        cache.get(1, 4).as_deref(),
        Some(&[1u8; 4][..]),
        "entry 1 kept its bytes on its second chance"
    );
    assert!(cache.get(3, 4).is_some(), "the new entry is held");
    assert!(cache.get(1, 8).is_none(), "a read of another length misses");
}

// -- One inode object per inode, and the page cache over the source ---------

#[test]
fn two_walks_to_one_file_share_one_inode_object() {
    let fs = mount("none");
    let path = b"dir03/sub0/file07.txt";
    let first = resolve(&fs, path);
    let second = resolve(&fs, path);
    assert!(
        Arc::ptr_eq(&first, &second),
        "a second lookup finds the object that exists, not a twin"
    );
    assert_eq!(
        fs.shared.nodes.lock().len(),
        2,
        "the root and the file are alive; the directories walked through are not"
    );
}

#[test]
fn a_dropped_inode_object_takes_its_map_entry_with_it() {
    let fs = mount("none");
    let file = resolve(&fs, b"random.bin");
    let ino = file.metadata().ino;
    assert!(fs.shared.nodes.lock().contains_key(&ino));
    drop(file);
    assert!(
        !fs.shared.nodes.lock().contains_key(&ino),
        "the last reference removes the entry"
    );
    assert_eq!(fs.shared.nodes.lock().len(), 1, "only the root is left");
    let again = resolve(&fs, b"random.bin");
    assert_eq!(
        again.metadata().ino,
        ino,
        "and a new lookup builds it again"
    );
}

/// An image whose data reads are logged as `(physical, len)`, and in which
/// one byte can be corrupted, so a test can find a file's sector and break it.
#[derive(Clone)]
struct Breakable {
    image: Image,
    reads: Arc<SpinLock<Vec<(u64, usize)>>>,
    broken: Arc<SpinLock<Option<u64>>>,
}

impl Breakable {
    fn new(packed: &[u8]) -> Breakable {
        Breakable {
            image: Image::new(packed),
            reads: Arc::new(SpinLock::new(Vec::new())),
            broken: Arc::new(SpinLock::new(None)),
        }
    }
}

impl Device for Breakable {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        kind: ReadKind,
    ) -> core::result::Result<(), BtrfsError> {
        self.image.read_at(physical, buf, kind)?;
        if kind == ReadKind::Data {
            self.reads.lock().push((physical, buf.len()));
        }
        if let Some(at) = *self.broken.lock()
            && at >= physical
            && at < physical + buf.len() as u64
        {
            buf[(at - physical) as usize] ^= 0x10;
        }
        Ok(())
    }
}

#[test]
fn a_damaged_sector_fails_its_own_page_and_the_pages_before_it_are_kept() {
    // Find where the file's data lives: the first data read of a fresh mount.
    let device = Breakable::new(IMAGES[0].1);
    let fs = Btrfs::mount(device.clone(), 42, heap()).unwrap();
    let file = resolve_in(&fs, b"random.bin");
    let mut pages = vec![0u8; 3 * BLOCK];
    assert_eq!(file.read_at(0, &mut pages), Ok(3 * BLOCK));
    let page = pages[..BLOCK].to_vec();
    // The volume reads a regular extent's sectors in one read, so the file's
    // second sector is one block into the first data read.
    let second = {
        let reads = device.reads.lock();
        let (physical, len) = reads[0];
        assert!(len >= 3 * BLOCK, "the run read several sectors: {reads:?}");
        physical + BLOCK as u64
    };
    drop(file);
    drop(fs);

    // Break that sector, and mount afresh.
    let device = Breakable::new(IMAGES[0].1);
    *device.broken.lock() = Some(second + 100);
    let fs = Btrfs::mount(device.clone(), 42, heap()).unwrap();
    let file = resolve_in(&fs, b"random.bin");
    let mut first = vec![0u8; BLOCK];
    assert_eq!(
        file.read_at(0, &mut first),
        Ok(BLOCK),
        "the page before the damage reads: the run failed, the page verified"
    );
    assert_eq!(first, page, "with the bytes the undamaged image has");
    let mut second = vec![0u8; BLOCK];
    assert_eq!(
        file.read_at(BLOCK as u64, &mut second),
        Err(Errno::EIO),
        "the damaged page is an error, never zeros"
    );
    assert_eq!(
        file.read_at(BLOCK as u64 + 50, &mut second[..10]),
        Err(Errno::EIO),
        "and stays one on the next read"
    );
    let reads_before = device.reads.lock().len();
    assert_eq!(file.read_at(0, &mut first), Ok(BLOCK));
    assert_eq!(
        device.reads.lock().len(),
        reads_before,
        "the good page is served from the cache"
    );
}

#[test]
fn a_read_past_the_end_is_empty_and_the_last_page_is_zero_padded() {
    let fs = mount("none");
    let manifest = manifest();
    let (path, expected) = manifest
        .iter()
        .find(|(_, e)| e.kind == "file" && e.size % BLOCK as u64 != 0 && e.size > 0)
        .expect("a file whose size is not a whole number of pages");
    let file = resolve(&fs, path);
    let mut buf = vec![0xAAu8; 16];
    assert_eq!(file.read_at(expected.size, &mut buf), Ok(0), "at the end");
    assert_eq!(
        file.read_at(expected.size + 100, &mut buf),
        Ok(0),
        "past it"
    );
    let tail = expected.size % BLOCK as u64;
    let mut last = vec![0xAAu8; BLOCK];
    let got = file.read_at(expected.size - tail, &mut last).unwrap();
    assert_eq!(got as u64, tail, "a read is clamped to the file's size");
}

/// A device that runs out of memory on every read while `starved` is set,
/// as one reading through a buffer it must allocate does.
#[derive(Clone)]
struct Starved {
    image: Image,
    starved: Arc<std::sync::atomic::AtomicBool>,
}

impl Device for Starved {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        kind: ReadKind,
    ) -> core::result::Result<(), BtrfsError> {
        if self.starved.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(BtrfsError::OutOfMemory);
        }
        self.image.read_at(physical, buf, kind)
    }
}

#[test]
fn a_device_out_of_memory_is_enomem_not_eio() {
    let (_, packed) = IMAGES[0];
    let starved = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let device = Starved {
        image: Image::new(packed),
        starved: Arc::clone(&starved),
    };
    assert_eq!(
        Btrfs::mount(device.clone(), 42, heap()).err(),
        Some(Errno::ENOMEM),
        "a mount that could not read for memory is not a bad volume"
    );
    starved.store(false, std::sync::atomic::Ordering::Relaxed);
    let fs = Btrfs::mount(device, 42, heap()).unwrap();
    let file = fs.root().lookup(b"big.txt").unwrap();
    starved.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut piece = vec![0u8; 3001];
    assert_eq!(file.read_at(0, &mut piece).err(), Some(Errno::ENOMEM));
    starved.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        read_all(&file).len(),
        140_000,
        "and it reads once there is memory"
    );
}
