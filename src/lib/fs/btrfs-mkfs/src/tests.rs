//! What [`make`] writes, judged by the reader, the writer and real
//! `mkfs.btrfs`'s volume.
//!
//! The oracles: `ferrix-btrfs` must open the volume and find its label and
//! root directory; `ferrix-btrfs-write` must open it, which checks every
//! block group against its chunk and its free-space items, then fill it to
//! the end of its device, making chunks as it goes, and read back what it
//! wrote after a reopen; and its trees must hold the items, item type for
//! item type, that `mkfs.btrfs` wrote into the `root` fixture. Host
//! `btrfs check` is the fourth, over the images the ignored test
//! [`btrfs_check_images`] writes (`tools/common/test/btrfs-check-writer.sh`).

extern crate std;

use std::boxed::Box;
use std::collections::BTreeMap;
use std::vec;
use std::vec::Vec;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::chunk::ChunkMapEntry;
use ferrix_btrfs::items::{S_IFREG, Timespec};
use ferrix_btrfs::superblock::{PRIMARY_OFFSET, SUPERBLOCK_SIZE, Superblock};
use ferrix_btrfs::volume::{Device, ReadKind, Volume};
use ferrix_btrfs_write::{NewInode, WriteDevice, WriteVolume};

use super::*;

const BLOCK: usize = 4096;
const ROOT_DIR: u64 = 256;
const NEW: NewInode = NewInode {
    mode: S_IFREG | 0o644,
    uid: 0,
    gid: 0,
    rdev: 0,
    now: Timespec { sec: 1, nsec: 0 },
};
/// `mkfs.btrfs -L ferrix-root` of 1 GiB with 4 KiB nodes: the volume the
/// installer used to copy.
const ROOT_FIXTURE: &[u8] = include_bytes!("../../btrfs/testdata/root.img.packed");

/// What a device was asked to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Write(u64, usize),
    Flush,
}

/// A sparse device in memory.
#[derive(Debug, Clone)]
struct MemDevice {
    size: u64,
    blocks: BTreeMap<u64, Box<[u8; BLOCK]>>,
    log: Vec<Op>,
}

impl MemDevice {
    fn new(size: u64) -> Self {
        MemDevice {
            size,
            blocks: BTreeMap::new(),
            log: Vec::new(),
        }
    }

    /// A device holding a packed fixture: records of an offset and a block.
    fn packed(packed: &[u8], size: u64) -> Self {
        let mut device = MemDevice::new(size);
        for record in packed.chunks_exact(8 + BLOCK) {
            let at = u64::from_le_bytes(record[..8].try_into().unwrap());
            device.store(at, &record[8..]);
        }
        device
    }

    fn store(&mut self, at: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let here = at + done as u64;
            let base = here - here % BLOCK as u64;
            let within = (here - base) as usize;
            let take = (BLOCK - within).min(data.len() - done);
            let block = self
                .blocks
                .entry(base)
                .or_insert_with(|| Box::new([0; BLOCK]));
            block[within..within + take].copy_from_slice(&data[done..done + take]);
            done += take;
        }
    }

    /// The whole device, for host tools.
    fn image(&self) -> Vec<u8> {
        let mut out = vec![0_u8; usize::try_from(self.size).unwrap()];
        for (&at, block) in &self.blocks {
            out[at as usize..at as usize + BLOCK].copy_from_slice(&block[..]);
        }
        out
    }
}

impl Device for MemDevice {
    fn read_at(&mut self, at: u64, buf: &mut [u8], _: ReadKind) -> Result<(), BtrfsError> {
        if at + buf.len() as u64 > self.size {
            return Err(BtrfsError::DeviceRead { physical: at });
        }
        let mut done = 0;
        while done < buf.len() {
            let here = at + done as u64;
            let base = here - here % BLOCK as u64;
            let within = (here - base) as usize;
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
    fn write_at(&mut self, at: u64, data: &[u8]) -> ferrix_btrfs_write::Result<()> {
        if at + data.len() as u64 > self.size {
            return Err(ferrix_btrfs_write::Error::DeviceWrite { physical: at });
        }
        self.store(at, data);
        self.log.push(Op::Write(at, data.len()));
        Ok(())
    }

    fn flush(&mut self) -> ferrix_btrfs_write::Result<()> {
        self.log.push(Op::Flush);
        Ok(())
    }
}

fn options(size: u64, nodesize: u32) -> Options<'static> {
    let random = [[0x11; 16], [0x22; 16], [0x33; 16], [0x44; 16]];
    Options {
        nodesize,
        ..Options::new(size, b"ferrix-root", random, 1_790_000_000)
    }
}

/// A device of `size` bytes, formatted.
fn formatted(size: u64, nodesize: u32) -> MemDevice {
    let image = make(&options(size, nodesize)).unwrap();
    let mut device = MemDevice::new(size);
    image.write_to(&mut device).unwrap();
    device
}

impl Target for MemDevice {
    type Error = ferrix_btrfs_write::Error;

    fn write_at(&mut self, at: u64, data: &[u8]) -> ferrix_btrfs_write::Result<()> {
        WriteDevice::write_at(self, at, data)
    }

    fn flush(&mut self) -> ferrix_btrfs_write::Result<()> {
        WriteDevice::flush(self)
    }
}

/// Every item of `tree`.
fn items_of(volume: &mut WriteVolume<MemDevice>, tree: u64) -> Vec<(BtrfsKey, Vec<u8>)> {
    volume.range(tree, &BtrfsKey::MIN, &BtrfsKey::MAX).unwrap()
}

const SIZES: [u64; 4] = [MIN_SIZE, 1 << 30, 300 << 30, (1 << 40) + 12345];

#[test]
fn the_reader_opens_the_volume_and_finds_its_label() {
    for size in SIZES {
        for nodesize in [4096, DEFAULT_NODESIZE, 65536] {
            let mut device = formatted(size, nodesize);
            let mut block = vec![0_u8; SUPERBLOCK_SIZE];
            device
                .read_at(PRIMARY_OFFSET, &mut block, ReadKind::Metadata)
                .unwrap();
            let sb = Superblock::parse_at(&block, PRIMARY_OFFSET).unwrap();
            assert_eq!(sb.label(), Some("ferrix-root"));
            assert_eq!(sb.total_bytes(), size - size % 4096);
            assert_eq!(sb.nodesize(), nodesize);
            let mut node = vec![0_u8; 65536];
            let storage = vec![ChunkMapEntry::EMPTY; 256];
            let _ = Volume::open(&mut device, storage, &mut node)
                .unwrap_or_else(|e| panic!("{size} bytes, {nodesize}-byte nodes: {e}"));
        }
    }
}

#[test]
fn every_superblock_copy_the_device_holds_is_written_and_names_its_place() {
    for (size, copies) in [(MIN_SIZE, 2), (1 << 30, 2), (300 << 30, 3)] {
        let image = make(&options(size, DEFAULT_NODESIZE)).unwrap();
        assert_eq!(image.superblocks.len(), copies, "{size}");
        for (at, block) in &image.superblocks {
            let sb = Superblock::parse_at(block, *at).unwrap();
            assert_eq!(sb.bytenr(), *at);
        }
        assert_eq!(image.superblocks[0].0, PRIMARY_OFFSET);
    }
}

/// Verifies the write order [`Image::write_to`] promises.
#[test]
fn no_superblock_is_written_before_the_trees_are_flushed() {
    let device = formatted(1 << 30, DEFAULT_NODESIZE);
    let log = &device.log;
    assert_eq!(log[0], Op::Write(0, 1 << 20), "the start is wiped first");
    let is_super = |op: &Op| matches!(op, Op::Write(at, _) if SUPERBLOCK_OFFSETS.contains(at));
    let primary = log.iter().position(is_super).unwrap();
    assert_eq!(log[primary], Op::Write(PRIMARY_OFFSET, SUPERBLOCK_SIZE));
    assert_eq!(log[primary - 1], Op::Flush, "the trees are flushed first");
    assert_eq!(log[primary + 1], Op::Flush, "the primary is durable first");
    assert!(log[1..primary - 1].iter().all(|op| !is_super(op)));
    assert!(
        log[primary + 2..]
            .iter()
            .all(|op| is_super(op) || *op == Op::Flush)
    );
    assert_eq!(log.last(), Some(&Op::Flush));
}

#[test]
fn the_writer_opens_the_volume_and_fills_its_whole_device() {
    for nodesize in [4096, DEFAULT_NODESIZE] {
        let size = MIN_SIZE;
        let mut volume = WriteVolume::open(formatted(size, nodesize)).unwrap();
        let first_data_chunk = chunk_bytes(size, DATA_MOST);
        // The room the writer counts includes the device no chunk holds yet,
        // less the headroom it keeps for the trees.
        let room = volume.data_room();
        assert!(
            room > 4 * first_data_chunk,
            "{nodesize}: the writer counts {room} bytes of room past a first data chunk of \
             {first_data_chunk}"
        );
        let piece = vec![0x5a_u8; 1 << 20];
        let mut written = 0_u64;
        let mut files = Vec::new();
        while volume.data_room() >= piece.len() as u64 {
            let name = std::format!("file{}", files.len());
            let ino = volume.create(ROOT_DIR, name.as_bytes(), &NEW).unwrap();
            let mut data = piece.clone();
            data[..8].copy_from_slice(&written.to_le_bytes());
            volume.write_file(ino, 0, &data, data.len() as u64).unwrap();
            written += data.len() as u64;
            files.push((ino, data));
            if files.len() % 8 == 0 {
                volume.commit().unwrap();
            }
        }
        volume.commit().unwrap();
        assert!(
            written + piece.len() as u64 >= room,
            "{nodesize}: {written} bytes went into a {size}-byte volume with {room} bytes of room"
        );
        assert!(
            volume.data_room() < piece.len() as u64,
            "{nodesize}: the volume is full"
        );
        let mut reopened = WriteVolume::open(volume.into_device()).unwrap();
        for (ino, data) in files.iter().step_by(7) {
            let mut back = vec![0_u8; data.len()];
            assert_eq!(reopened.read_file(*ino, 0, &mut back).unwrap(), data.len());
            assert!(back == *data, "{nodesize}: inode {ino} reads back");
        }
    }
}

/// The trees' items, by type and size, with the addresses and UUIDs that
/// differ between two volumes left out.
fn shape(volume: &mut WriteVolume<MemDevice>, tree: u64) -> Vec<(u64, u8, usize)> {
    items_of(volume, tree)
        .into_iter()
        .map(|(key, data)| {
            let objectid = match tree {
                ROOT_TREE_OBJECTID | FS_TREE_OBJECTID | DATA_RELOC_TREE_OBJECTID => key.objectid,
                _ => 0,
            };
            (objectid, key.item_type, data.len())
        })
        .collect()
}

/// `mkfs.btrfs` relocates its first chunks once and so writes its trees in
/// later generations; what is compared is what the trees hold.
#[test]
fn the_trees_hold_what_mkfs_btrfs_writes() {
    let mut ours = WriteVolume::open(formatted(1 << 30, 4096)).unwrap();
    let mut theirs = WriteVolume::open(MemDevice::packed(ROOT_FIXTURE, 1 << 30)).unwrap();
    for tree in [
        ROOT_TREE_OBJECTID,
        FS_TREE_OBJECTID,
        DATA_RELOC_TREE_OBJECTID,
        CSUM_TREE_OBJECTID,
        UUID_TREE_OBJECTID,
        DEV_TREE_OBJECTID,
    ] {
        assert_eq!(
            shape(&mut ours, tree),
            shape(&mut theirs, tree),
            "tree {tree}"
        );
    }
    // `mkfs.btrfs`'s DATA chunk sits below its SYSTEM one; the kinds and
    // profiles are what must agree.
    let kinds = |volume: &mut WriteVolume<MemDevice>| {
        let mut kinds: Vec<u64> = items_of(volume, CHUNK_TREE_OBJECTID)
            .iter()
            .filter(|(key, _)| key.item_type == CHUNK_ITEM_KEY)
            .map(|(_, data)| u64::from_le_bytes(data[24..32].try_into().unwrap()))
            .collect();
        kinds.sort_unstable();
        kinds
    };
    assert_eq!(kinds(&mut ours), kinds(&mut theirs));
    // The subvolume trees agree byte for byte but for their times and
    // generations.
    for tree in [FS_TREE_OBJECTID, DATA_RELOC_TREE_OBJECTID] {
        let normalise = |items: Vec<(BtrfsKey, Vec<u8>)>| -> Vec<(BtrfsKey, Vec<u8>)> {
            items
                .into_iter()
                .map(|(key, mut data)| {
                    if key.item_type == INODE_ITEM_KEY {
                        data[..8].fill(0);
                        data[112..].fill(0);
                    }
                    (key, data)
                })
                .collect()
        };
        assert_eq!(
            normalise(items_of(&mut ours, tree)),
            normalise(items_of(&mut theirs, tree)),
            "tree {tree}"
        );
    }
}

#[test]
fn the_superblock_says_what_mkfs_btrfs_says() {
    let read = |device: &mut MemDevice| {
        let mut block = vec![0_u8; SUPERBLOCK_SIZE];
        device
            .read_at(PRIMARY_OFFSET, &mut block, ReadKind::Metadata)
            .unwrap();
        block
    };
    let ours = read(&mut formatted(1 << 30, 4096));
    let theirs = read(&mut MemDevice::packed(ROOT_FIXTURE, 1 << 30));
    let (ours, theirs) = (
        Superblock::parse_at(&ours, PRIMARY_OFFSET).unwrap(),
        Superblock::parse_at(&theirs, PRIMARY_OFFSET).unwrap(),
    );
    assert_eq!(ours.incompat_flags().bits(), theirs.incompat_flags().bits());
    assert_eq!(ours.compat_ro_flags(), theirs.compat_ro_flags());
    assert_eq!(ours.compat_flags(), theirs.compat_flags());
    assert_eq!(ours.flags(), theirs.flags());
    assert_eq!(ours.label(), theirs.label());
    assert_eq!(ours.total_bytes(), theirs.total_bytes());
    assert_eq!(ours.root_dir_objectid(), theirs.root_dir_objectid());
    assert_eq!(ours.sectorsize(), theirs.sectorsize());
    assert_eq!(ours.stripesize(), theirs.stripesize());
    assert_eq!(ours.sys_chunk_array_size(), theirs.sys_chunk_array_size());
    assert_eq!(ours.bytes_used(), theirs.bytes_used(), "nine leaves each");
    // 16 KiB nodes take BIG_METADATA, as `mkfs.btrfs` sets it.
    let big = make(&options(1 << 30, DEFAULT_NODESIZE)).unwrap();
    let sb = Superblock::parse_at(&big.superblocks[0].1, PRIMARY_OFFSET).unwrap();
    assert!(sb.incompat_flags().contains(IncompatFlags::BIG_METADATA));
}

#[test]
fn a_device_too_small_a_node_size_or_a_label_is_refused() {
    let too_small = make(&options(MIN_SIZE - 1, DEFAULT_NODESIZE));
    assert!(matches!(too_small, Err(Error::TooSmall { .. })));
    for nodesize in [0, 2048, 12288, 131_072] {
        assert_eq!(
            make(&options(1 << 30, nodesize)),
            Err(Error::NodeSize(nodesize))
        );
    }
    let long = [b'x'; MAX_LABEL + 1];
    let mut asked = options(1 << 30, DEFAULT_NODESIZE);
    asked.label = &long;
    assert_eq!(make(&asked), Err(Error::Label));
    asked.label = b"a\0b";
    assert_eq!(make(&asked), Err(Error::Label));
    asked.label = &long[..MAX_LABEL];
    assert!(make(&asked).is_ok());
}

#[test]
fn the_uuids_are_version_4() {
    let made = Options::new(1 << 30, b"", [[0xff; 16]; 4], 0);
    for uuid in [
        made.fsid,
        made.dev_uuid,
        made.chunk_tree_uuid,
        made.subvol_uuid,
    ] {
        assert_eq!(uuid[6] >> 4, 4);
        assert_eq!(uuid[8] >> 6, 0b10);
    }
}

/// Write fresh volumes, and the same filled, to `FERRIX_BTRFS_OUT` for host
/// `btrfs check`; see `tools/common/test/btrfs-check-writer.sh`.
#[test]
#[ignore = "writes images for host btrfs check; run by tools/common/test/btrfs-check-writer.sh"]
fn btrfs_check_images() {
    let out = std::env::var("FERRIX_BTRFS_OUT").expect("FERRIX_BTRFS_OUT names a directory");
    for nodesize in [4096, DEFAULT_NODESIZE] {
        let device = formatted(MIN_SIZE, nodesize);
        std::fs::write(std::format!("{out}/mkfs-{nodesize}.img"), device.image()).unwrap();
        let mut volume = WriteVolume::open(device).unwrap();
        let piece = vec![0xa5_u8; 1 << 20];
        let mut n = 0;
        while volume.data_room() >= piece.len() as u64 {
            let ino = volume
                .create(ROOT_DIR, std::format!("f{n}").as_bytes(), &NEW)
                .unwrap();
            volume
                .write_file(ino, 0, &piece, piece.len() as u64)
                .unwrap();
            n += 1;
        }
        volume.commit().unwrap();
        let image = volume.into_device().image();
        std::fs::write(std::format!("{out}/mkfs-{nodesize}-filled.img"), image).unwrap();
    }
}
