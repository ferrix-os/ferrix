//! Tests for the btrfs on-disk format.
//!
//! Structures are built here rather than checked in as an image, so a failure
//! names the field that is wrong instead of pointing at an opaque blob. The
//! builders below write the layout independently of the parsers under test —
//! a builder that reused the parser's own offsets could not catch the parser
//! putting a field in the wrong place.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::chunk::{ChunkItem, ChunkMap, ChunkMapEntry, ChunkProfile};
use super::crc32c::{POLYNOMIAL, TABLE, crc32c};
use super::items::{DirItemIter, ExtentData, ExtentDataBody, InodeItem, InodeRefIter, name_hash};
use super::superblock::{ChecksumType, MAGIC, SUPERBLOCK_SIZE, Superblock};
use super::tree::{BtrfsKey, HEADER_SIZE, ITEM_SIZE, KEY_PTR_SIZE, Node};
use super::*;

// ---------------------------------------------------------------------------
// CRC-32C
// ---------------------------------------------------------------------------

/// Verifies: L.btrfs.1
#[test]
fn crc32c_matches_the_published_vectors() {
    // Castagnoli, the polynomial btrfs checksums with. These three appear in
    // every implementation's test suite, which is what makes them useful: an
    // implementation that gets them right is talking to the same standard as
    // `btrfs-progs`.
    assert_eq!(crc32c(b""), 0, "the empty input has checksum zero");
    assert_eq!(crc32c(b"a"), 0xC1D0_4330, "single byte vector");
    assert_eq!(crc32c(b"123456789"), 0xE306_9283, "the check value");
}

#[test]
fn crc32c_is_not_crc32() {
    // The ordinary CRC-32 of "123456789" is 0xCBF43926. Getting the two
    // confused produces a filesystem every other tool calls corrupt.
    assert_ne!(crc32c(b"123456789"), 0xCBF4_3926, "this must be Castagnoli");
}

#[test]
fn crc32c_covers_long_input() {
    let long = vec![0x5Au8; 4096];
    let whole = crc32c(&long);
    assert_ne!(whole, 0, "a page of data should not check out as zero");
    assert_ne!(
        whole,
        crc32c(&long[..4095]),
        "dropping a byte must change the checksum"
    );
}

#[test]
fn the_name_hash_differs_from_a_plain_checksum() {
    // Directory item offsets are the name's CRC-32C seeded with `!1`, not the
    // plain checksum. Using the wrong one puts every directory entry at the
    // wrong key and lookups silently find nothing.
    assert_ne!(
        name_hash(b"hello"),
        u64::from(crc32c(b"hello")),
        "the directory hash is seeded differently"
    );
    assert_eq!(name_hash(b"a"), name_hash(b"a"), "and is deterministic");
    assert_ne!(name_hash(b"a"), name_hash(b"b"));
}

// ---------------------------------------------------------------------------
// Superblock
// ---------------------------------------------------------------------------

/// Field offsets, written out here independently of the parser's own.
mod offset {
    pub(super) const CSUM: usize = 0;
    pub(super) const FSID: usize = 32;
    pub(super) const BYTENR: usize = 48;
    pub(super) const MAGIC: usize = 64;
    pub(super) const GENERATION: usize = 72;
    pub(super) const ROOT: usize = 80;
    pub(super) const CHUNK_ROOT: usize = 88;
    pub(super) const LOG_ROOT: usize = 96;
    pub(super) const TOTAL_BYTES: usize = 112;
    pub(super) const SECTORSIZE: usize = 144;
    pub(super) const NODESIZE: usize = 148;
    pub(super) const LEAFSIZE: usize = 152;
    pub(super) const SYS_CHUNK_ARRAY_SIZE: usize = 160;
    pub(super) const INCOMPAT_FLAGS: usize = 188;
    pub(super) const CSUM_TYPE: usize = 196;
    pub(super) const ROOT_LEVEL: usize = 198;
    pub(super) const CHUNK_ROOT_LEVEL: usize = 199;
    pub(super) const LOG_ROOT_LEVEL: usize = 200;
    pub(super) const LABEL: usize = 299;
    pub(super) const SYS_CHUNK_ARRAY: usize = 811;
}

/// A superblock under construction.
struct SuperblockBuilder {
    bytes: Vec<u8>,
}

impl SuperblockBuilder {
    fn new() -> SuperblockBuilder {
        let mut bytes = vec![0u8; SUPERBLOCK_SIZE];
        bytes[offset::MAGIC..offset::MAGIC + 8].copy_from_slice(&MAGIC);
        bytes[offset::FSID..offset::FSID + 16].copy_from_slice(&[0xAB; 16]);
        put_u64(&mut bytes, offset::BYTENR, 0x1_0000);
        put_u64(&mut bytes, offset::GENERATION, 42);
        put_u64(&mut bytes, offset::ROOT, 0x2000_0000);
        put_u64(&mut bytes, offset::CHUNK_ROOT, 0x1000_0000);
        put_u64(&mut bytes, offset::TOTAL_BYTES, 1 << 30);
        put_u32(&mut bytes, offset::SECTORSIZE, 4096);
        put_u32(&mut bytes, offset::NODESIZE, 16384);
        // mkfs writes the retired leafsize equal to nodesize, and Linux
        // refuses a superblock where it is anything else.
        put_u32(&mut bytes, offset::LEAFSIZE, 16384);
        put_u16(&mut bytes, offset::CSUM_TYPE, 0);
        bytes[offset::ROOT_LEVEL] = 1;
        bytes[offset::CHUNK_ROOT_LEVEL] = 0;
        let label = b"ferrix";
        bytes[offset::LABEL..offset::LABEL + label.len()].copy_from_slice(label);
        SuperblockBuilder { bytes }
    }

    /// Change the bytes at `at`, for a test that needs one field wrong.
    fn field(mut self, at: usize, value: &[u8]) -> Self {
        self.bytes[at..at + value.len()].copy_from_slice(value);
        self
    }

    fn sys_chunk_array(mut self, array: &[u8]) -> Self {
        put_u32(
            &mut self.bytes,
            offset::SYS_CHUNK_ARRAY_SIZE,
            array.len() as u32,
        );
        self.bytes[offset::SYS_CHUNK_ARRAY..offset::SYS_CHUNK_ARRAY + array.len()]
            .copy_from_slice(array);
        self
    }

    fn checksum_type(mut self, raw: u16) -> Self {
        put_u16(&mut self.bytes, offset::CSUM_TYPE, raw);
        self
    }

    fn incompat(mut self, flags: u64) -> Self {
        put_u64(&mut self.bytes, offset::INCOMPAT_FLAGS, flags);
        self
    }

    /// Seal it: the checksum covers everything after the checksum field.
    fn build(mut self) -> Vec<u8> {
        let computed = crc32c(&self.bytes[32..SUPERBLOCK_SIZE]);
        self.bytes[offset::CSUM..offset::CSUM + 4].copy_from_slice(&computed.to_le_bytes());
        self.bytes
    }

    /// Seal it with a checksum that is wrong.
    fn build_corrupt(mut self) -> Vec<u8> {
        let computed = crc32c(&self.bytes[32..SUPERBLOCK_SIZE]);
        self.bytes[offset::CSUM..offset::CSUM + 4]
            .copy_from_slice(&computed.wrapping_add(1).to_le_bytes());
        self.bytes
    }
}

fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Verifies: L.btrfs.1
#[test]
fn a_well_formed_superblock_parses() {
    let bytes = SuperblockBuilder::new().build();
    let superblock = Superblock::parse(&bytes).unwrap();

    assert_eq!(superblock.magic(), MAGIC, "magic must round trip");
    assert_eq!(superblock.generation(), 42);
    assert_eq!(
        superblock.root(),
        0x2000_0000,
        "the root tree's logical address"
    );
    assert_eq!(superblock.chunk_root(), 0x1000_0000);
    assert_eq!(superblock.sectorsize(), 4096);
    assert_eq!(superblock.nodesize(), 16384);
    assert_eq!(superblock.root_level(), 1);
    assert_eq!(superblock.chunk_root_level(), 0);
    assert_eq!(superblock.csum_type(), ChecksumType::Crc32c);
    assert_eq!(
        superblock.label(),
        Some("ferrix"),
        "the label is NUL padded"
    );
    assert_eq!(superblock.total_bytes(), 1 << 30);
}

/// Verifies: L.btrfs.1
#[test]
fn a_superblock_with_the_wrong_magic_is_refused() {
    let mut bytes = SuperblockBuilder::new().build();
    bytes[offset::MAGIC] = b'X';
    assert!(
        matches!(Superblock::parse(&bytes), Err(BtrfsError::BadMagic)),
        "without the magic this is not a btrfs superblock at all"
    );
}

/// Verifies: L.btrfs.1, H.STORE.2
#[test]
fn a_superblock_with_a_bad_checksum_is_refused() {
    let bytes = SuperblockBuilder::new().build_corrupt();
    assert!(
        matches!(
            Superblock::parse(&bytes),
            Err(BtrfsError::BadChecksum { .. })
        ),
        "a superblock that does not check out must not be trusted"
    );
}

/// Verifies: L.btrfs.1
#[test]
fn a_truncated_superblock_is_refused() {
    let bytes = SuperblockBuilder::new().build();
    for length in [0, 1, 64, 810, SUPERBLOCK_SIZE - 1] {
        assert!(
            Superblock::parse(&bytes[..length]).is_err(),
            "a {length}-byte superblock must not parse"
        );
    }
}

/// Verifies: L.btrfs.1
#[test]
fn a_superblock_whose_sizes_disagree_is_refused() {
    let sizes = |sector: u32, node: u32, leaf: u32| {
        SuperblockBuilder::new()
            .field(offset::SECTORSIZE, &sector.to_le_bytes())
            .field(offset::NODESIZE, &node.to_le_bytes())
            .field(offset::LEAFSIZE, &leaf.to_le_bytes())
            .build()
    };
    for (bytes, expected, what) in [
        (
            sizes(65536, 4096, 8192),
            BtrfsError::BadNodeSize(4096),
            "nodes smaller than a sector, with a leafsize disagreeing too",
        ),
        (
            sizes(16384, 8192, 8192),
            BtrfsError::BadNodeSize(8192),
            "nodes smaller than a sector",
        ),
        (
            sizes(4096, 16384, 8192),
            BtrfsError::BadNodeSize(8192),
            "a leafsize other than the nodesize",
        ),
        (
            sizes(4096, 16384, 0),
            BtrfsError::BadNodeSize(0),
            "a zero leafsize",
        ),
    ] {
        assert_eq!(Superblock::parse(&bytes).unwrap_err(), expected, "{what}");
    }
    for (sector, node) in [(4096, 4096), (65536, 65536), (512, 4096)] {
        assert!(
            Superblock::parse(&sizes(sector, node, node)).is_ok(),
            "sectorsize {sector} with nodesize {node} is a volume mkfs could make"
        );
    }
}

/// Verifies: L.btrfs.1
#[test]
fn a_superblock_naming_an_impossible_root_is_refused() {
    let with = |at: usize, value: &[u8]| SuperblockBuilder::new().field(at, value).build();
    for (bytes, logical, what) in [
        (
            with(offset::ROOT_LEVEL, &[8]),
            0x2000_0000,
            "a root level of 8",
        ),
        (
            with(offset::CHUNK_ROOT_LEVEL, &[8]),
            0x1000_0000,
            "a chunk root level of 8",
        ),
        (
            with(offset::LOG_ROOT_LEVEL, &[0xFF]),
            0,
            "a log root level of 255",
        ),
        (
            with(offset::ROOT, &0x2000_0200u64.to_le_bytes()),
            0x2000_0200,
            "a root between sectors",
        ),
        (
            with(offset::CHUNK_ROOT, &0x1000_0001u64.to_le_bytes()),
            0x1000_0001,
            "a chunk root between sectors",
        ),
        (
            with(offset::LOG_ROOT, &0x3000_0800u64.to_le_bytes()),
            0x3000_0800,
            "a log root between sectors",
        ),
    ] {
        assert_eq!(
            Superblock::parse(&bytes).unwrap_err(),
            BtrfsError::BadTree { logical },
            "{what}"
        );
    }

    let deepest = SuperblockBuilder::new()
        .field(offset::ROOT_LEVEL, &[7])
        .field(offset::CHUNK_ROOT_LEVEL, &[7])
        .field(offset::LOG_ROOT_LEVEL, &[7])
        .field(offset::LOG_ROOT, &0x3000_1000u64.to_le_bytes())
        .build();
    assert!(
        Superblock::parse(&deepest).is_ok(),
        "level 7 roots on sector boundaries are the deepest btrfs builds"
    );
}

/// Verifies: L.btrfs.1
#[test]
fn an_unsupported_checksum_type_is_reported() {
    // 1, 2 and 3 are xxhash, sha256 and blake2. They are real, and this reader
    // does not implement them; saying so beats verifying nothing.
    let bytes = SuperblockBuilder::new().checksum_type(2).build();
    match Superblock::parse(&bytes) {
        Err(BtrfsError::UnsupportedChecksum(2)) => {}
        other => panic!("expected an unsupported-checksum error, got {other:?}"),
    }
}

#[test]
fn incompatible_feature_bits_are_decoded() {
    const SKINNY_METADATA: u64 = 256;
    const NO_HOLES: u64 = 512;
    let bytes = SuperblockBuilder::new()
        .incompat(SKINNY_METADATA | NO_HOLES)
        .build();
    let superblock = Superblock::parse(&bytes).unwrap();

    assert!(superblock.incompat_flags().skinny_metadata());
    assert!(superblock.incompat_flags().no_holes());
    assert!(!superblock.incompat_flags().extended_iref());
    assert_eq!(
        superblock.incompat_flags().unknown(),
        0,
        "every bit set here is one this reader understands"
    );
}

#[test]
fn an_unknown_feature_bit_is_reported_rather_than_ignored() {
    let bytes = SuperblockBuilder::new().incompat(1 << 40).build();
    let superblock = Superblock::parse(&bytes).unwrap();
    assert_ne!(
        superblock.incompat_flags().unknown(),
        0,
        "a filesystem using a feature we do not implement must be recognisable as such"
    );
}

// ---------------------------------------------------------------------------
// Chunks: the logical-to-physical mapping
// ---------------------------------------------------------------------------

/// Build a `CHUNK_ITEM` payload.
fn chunk_item(length: u64, type_bits: u64, stripes: &[(u64, u64)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&2u64.to_le_bytes()); // owner
    bytes.extend_from_slice(&65536u64.to_le_bytes()); // stripe_len
    bytes.extend_from_slice(&type_bits.to_le_bytes());
    bytes.extend_from_slice(&4096u32.to_le_bytes()); // io_align
    bytes.extend_from_slice(&4096u32.to_le_bytes()); // io_width
    bytes.extend_from_slice(&4096u32.to_le_bytes()); // sector_size
    bytes.extend_from_slice(&(stripes.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes()); // sub_stripes
    for &(devid, offset) in stripes {
        bytes.extend_from_slice(&devid.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 16]);
    }
    bytes
}

/// Chunk type bits.
const CHUNK_DATA: u64 = 1;
const CHUNK_SYSTEM: u64 = 2;
const CHUNK_RAID1: u64 = 16;
const CHUNK_DUP: u64 = 32;
const CHUNK_RAID0: u64 = 8;

/// Verifies: L.btrfs.3
#[test]
fn a_single_chunk_maps_logical_to_physical() {
    let item_bytes = chunk_item(1 << 20, CHUNK_DATA, &[(1, 0x400_0000)]);
    let item = ChunkItem::parse(&item_bytes).unwrap();

    let mut storage = [ChunkMapEntry::default(); 4];
    let mut map = ChunkMap::new(&mut storage);
    map.insert(0x100_0000, &item).unwrap();

    assert_eq!(
        map.logical_to_physical(0x100_0000),
        Some((1, 0x400_0000)),
        "the first byte of the chunk"
    );
    assert_eq!(
        map.logical_to_physical(0x100_1000),
        Some((1, 0x400_1000)),
        "an address inside translates by its offset"
    );
    assert_eq!(
        map.logical_to_physical(0x100_0000 + (1 << 20) - 1),
        Some((1, 0x400_0000 + (1 << 20) - 1)),
        "the last byte"
    );
    assert_eq!(
        map.logical_to_physical(0x100_0000 + (1 << 20)),
        None,
        "one past the end is outside the chunk"
    );
    assert_eq!(
        map.logical_to_physical(0),
        None,
        "an address in no chunk maps nowhere"
    );
}

/// Verifies: L.btrfs.3
#[test]
fn mirrored_profiles_read_from_the_first_stripe() {
    // DUP and RAID1 both keep a whole copy per stripe, so a *read* can be
    // satisfied from stripe zero. RAID0 cannot: its data is interleaved, and
    // pretending otherwise would return the wrong bytes rather than an error.
    for (bits, mirrored) in [(CHUNK_DUP, true), (CHUNK_RAID1, true), (CHUNK_RAID0, false)] {
        let item_bytes = chunk_item(1 << 20, CHUNK_DATA | bits, &[(1, 0x1000), (2, 0x9000)]);
        let item = ChunkItem::parse(&item_bytes).unwrap();
        assert_eq!(
            item.profile().is_mirrored(),
            mirrored,
            "profile {bits:#x} mirroring"
        );

        let mut storage = [ChunkMapEntry::default(); 2];
        let mut map = ChunkMap::new(&mut storage);
        map.insert(0, &item).unwrap();

        if mirrored {
            assert_eq!(
                map.logical_to_physical(0),
                Some((1, 0x1000)),
                "a mirrored chunk reads from stripe zero"
            );
        } else {
            // Recording the chunk is fine -- knowing a RAID0 chunk exists is
            // useful. What must not happen is *translating* through it, since
            // its data is interleaved across stripes and stripe zero holds only
            // part of it. Refusing at translation rather than at insertion
            // keeps the map a description of the filesystem rather than of what
            // this reader happens to support.
            assert!(
                map.logical_to_physical(0).is_none(),
                "an interleaved profile must not translate"
            );
            assert!(
                matches!(map.map(0), Err(BtrfsError::UnsupportedProfile(_))),
                "and must say why rather than merely answering nothing"
            );
        }
    }
}

/// Verifies: L.btrfs.3
#[test]
fn the_profile_of_an_unsupported_layout_is_reported() {
    assert!(!ChunkProfile::from_type(CHUNK_RAID0).is_supported());
    assert!(ChunkProfile::from_type(CHUNK_DATA).is_supported());
    assert!(ChunkProfile::from_type(CHUNK_DATA | CHUNK_DUP).is_supported());
}

/// Verifies: L.btrfs.3
#[test]
fn a_chunk_item_with_no_stripes_is_refused() {
    let item_bytes = chunk_item(1 << 20, CHUNK_DATA, &[]);
    assert!(
        ChunkItem::parse(&item_bytes).is_err(),
        "a chunk with no stripes describes no storage at all"
    );
}

/// Verifies: L.btrfs.3
#[test]
fn a_truncated_chunk_item_is_refused() {
    let item_bytes = chunk_item(1 << 20, CHUNK_DATA, &[(1, 0x1000)]);
    for length in [0, 16, 47, item_bytes.len() - 1] {
        assert!(
            ChunkItem::parse(&item_bytes[..length]).is_err(),
            "a {length}-byte chunk item must not parse"
        );
    }
}

/// Verifies: L.btrfs.3
#[test]
fn the_system_chunk_array_bootstraps_the_map() {
    // This array is the only way to read the chunk tree, because finding the
    // chunk tree needs a mapping and the mapping lives in the chunk tree.
    let item_bytes = chunk_item(1 << 20, CHUNK_SYSTEM, &[(1, 0x400_0000)]);

    let mut array = Vec::new();
    array.extend_from_slice(&256u64.to_le_bytes()); // key objectid: FIRST_CHUNK_TREE
    array.push(228); // key type: CHUNK_ITEM
    array.extend_from_slice(&0x100_0000u64.to_le_bytes()); // key offset: logical
    array.extend_from_slice(&item_bytes);

    let bytes = SuperblockBuilder::new().sys_chunk_array(&array).build();
    let superblock = Superblock::parse(&bytes).unwrap();

    let mut storage = [ChunkMapEntry::default(); 8];
    let mut map = ChunkMap::new(&mut storage);
    let loaded = map
        .load_sys_chunk_array(superblock.sys_chunk_array_bytes())
        .unwrap();

    assert_eq!(loaded, 1, "one chunk in the array");
    assert_eq!(map.logical_to_physical(0x100_0000), Some((1, 0x400_0000)));
}

/// Verifies: L.btrfs.3
#[test]
fn a_full_chunk_map_reports_rather_than_overruns() {
    let item_bytes = chunk_item(1 << 20, CHUNK_DATA, &[(1, 0)]);
    let item = ChunkItem::parse(&item_bytes).unwrap();

    let mut storage = [ChunkMapEntry::default(); 2];
    let mut map = ChunkMap::new(&mut storage);
    map.insert(0, &item).unwrap();
    map.insert(1 << 20, &item).unwrap();
    assert!(
        map.insert(2 << 20, &item).is_err(),
        "a third chunk does not fit and must be refused"
    );
    assert_eq!(map.len(), 2);
}

/// Verifies: L.btrfs.3
#[test]
fn a_chunk_overlapping_one_already_mapped_is_refused() {
    // A covers 16 MiB from 0x100_0000. Before this check, B inside it was
    // inserted beside it and shadowed it: 0x180_0000, past B's end but well
    // inside A, stopped mapping at all.
    let a = chunk_item(16 << 20, CHUNK_DATA, &[(1, 0x400_0000)]);
    let inside = chunk_item(1 << 20, CHUNK_DATA, &[(1, 0x900_0000)]);
    let straddling = chunk_item(0x90_0000, CHUNK_DATA, &[(1, 0xA00_0000)]);
    let same_start = chunk_item(1 << 20, CHUNK_DATA, &[(1, 0x400_0000)]);

    let mut storage = [ChunkMapEntry::default(); 8];
    let mut map = ChunkMap::new(&mut storage);
    map.insert(0x100_0000, &ChunkItem::parse(&a).unwrap())
        .unwrap();

    for (logical, bytes, what) in [
        (0x140_0000, &inside, "a chunk inside another"),
        (0x80_0000, &straddling, "a chunk running into the next one"),
        (
            0x100_0000,
            &same_start,
            "a different chunk at the same start",
        ),
    ] {
        assert_eq!(
            map.insert(logical, &ChunkItem::parse(bytes).unwrap()),
            Err(BtrfsError::BadChunk),
            "{what} is refused"
        );
    }
    assert_eq!(map.len(), 1, "nothing refused was kept");
    assert_eq!(
        map.logical_to_physical(0x180_0000),
        Some((1, 0x480_0000)),
        "the first chunk still maps all of its range"
    );
}

/// Verifies: L.btrfs.3
#[test]
fn adjacent_and_repeated_chunks_are_accepted() {
    // The boundary cases the overlap check must not catch: a chunk starting
    // exactly where the previous one ends, one ending exactly where the next
    // begins, and the system chunk array repeating a chunk-tree item.
    let item = chunk_item(1 << 20, CHUNK_DATA, &[(1, 0x400_0000)]);
    let other = chunk_item(1 << 20, CHUNK_DATA, &[(1, 0x500_0000)]);
    let below = chunk_item(1 << 20, CHUNK_DATA, &[(1, 0x600_0000)]);
    let item = ChunkItem::parse(&item).unwrap();

    let mut storage = [ChunkMapEntry::default(); 8];
    let mut map = ChunkMap::new(&mut storage);
    map.insert(0x100_0000, &item).unwrap();
    map.insert(0x110_0000, &ChunkItem::parse(&other).unwrap())
        .unwrap();
    map.insert(0x0F0_0000, &ChunkItem::parse(&below).unwrap())
        .unwrap();
    map.insert(0x100_0000, &item)
        .expect("an identical chunk may arrive twice");

    assert_eq!(map.len(), 3, "the repeat is not a second entry");
    assert_eq!(map.logical_to_physical(0x10F_FFFF), Some((1, 0x40F_FFFF)));
    assert_eq!(map.logical_to_physical(0x110_0000), Some((1, 0x500_0000)));
    assert_eq!(map.logical_to_physical(0x0FF_FFFF), Some((1, 0x60F_FFFF)));
}

/// Chunk item field offsets, written out independently of the parser's.
mod chunk_offset {
    pub(super) const LENGTH: usize = 0;
    pub(super) const STRIPE_LEN: usize = 16;
    pub(super) const TYPE: usize = 24;
    pub(super) const SECTOR_SIZE: usize = 40;
    pub(super) const SUB_STRIPES: usize = 46;
}

/// More chunk type bits, for the profiles the fixtures above do not use.
const CHUNK_METADATA: u64 = 4;
const CHUNK_RAID10: u64 = 64;
const CHUNK_RAID5: u64 = 128;
const CHUNK_RAID6: u64 = 256;
const CHUNK_RAID1C3: u64 = 512;
const CHUNK_RAID1C4: u64 = 1024;

/// `count` stripes on distinct devices.
fn stripes(count: u64) -> Vec<(u64, u64)> {
    (1..=count).map(|devid| (devid, devid << 24)).collect()
}

/// Verifies: L.btrfs.3
#[test]
fn a_chunk_item_btrfs_would_not_write_is_refused() {
    let single = chunk_item(1 << 20, CHUNK_DATA, &stripes(1));
    let patched = |at: usize, value: &[u8]| {
        let mut bytes = single.clone();
        bytes[at..at + value.len()].copy_from_slice(value);
        bytes
    };
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (
            patched(chunk_offset::STRIPE_LEN, &32768u64.to_le_bytes()),
            "a stripe length other than 64 KiB",
        ),
        (
            patched(chunk_offset::LENGTH, &0x10_0001u64.to_le_bytes()),
            "a length that is not whole sectors",
        ),
        (
            patched(
                chunk_offset::LENGTH,
                &(u64::from(u32::MAX) << 16).to_le_bytes(),
            ),
            "a length the stripe arithmetic cannot hold",
        ),
        (
            patched(chunk_offset::SECTOR_SIZE, &0u32.to_le_bytes()),
            "a zero sector size",
        ),
        (
            patched(chunk_offset::SECTOR_SIZE, &1000u32.to_le_bytes()),
            "a sector size that is not a power of two",
        ),
        (
            patched(chunk_offset::TYPE, &(CHUNK_DATA | 1 << 11).to_le_bytes()),
            "a type bit btrfs does not define",
        ),
        (
            patched(chunk_offset::TYPE, &0u64.to_le_bytes()),
            "no type bit at all",
        ),
        (
            patched(
                chunk_offset::TYPE,
                &(CHUNK_SYSTEM | CHUNK_DATA).to_le_bytes(),
            ),
            "a system chunk that also holds data",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_DUP | CHUNK_RAID1, &stripes(2)),
            "two profile bits",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA, &stripes(2)),
            "SINGLE with two stripes",
        ),
        (
            chunk_item(1 << 20, CHUNK_METADATA | CHUNK_DUP, &stripes(1)),
            "DUP with one stripe",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID1, &stripes(3)),
            "RAID1 with three stripes",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID1C3, &stripes(2)),
            "RAID1C3 with two stripes",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID1C4, &stripes(3)),
            "RAID1C4 with three stripes",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID10, &stripes(4)),
            "RAID10 without sub_stripes of two",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID5, &stripes(1)),
            "RAID5 with only its parity stripe",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID6, &stripes(2)),
            "RAID6 with only its parity stripes",
        ),
    ];
    for (bytes, what) in cases {
        assert_eq!(
            ChunkItem::parse(&bytes).map(|_| ()),
            Err(BtrfsError::BadChunk),
            "{what} is refused"
        );
    }
}

/// Verifies: L.btrfs.3
#[test]
fn every_layout_btrfs_writes_still_parses() {
    // The boundary of each rule above: the exact stripe counts, the longest
    // length the arithmetic holds, and a length of one sector.
    let mut raid10 = chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID10, &stripes(4));
    put_u16(&mut raid10, chunk_offset::SUB_STRIPES, 2);
    let longest = (u64::from(u32::MAX) << 16) - 4096;
    let cases = [
        (chunk_item(4096, CHUNK_DATA, &stripes(1)), "one sector"),
        (
            chunk_item(longest, CHUNK_DATA, &stripes(1)),
            "the longest chunk",
        ),
        (
            chunk_item(1 << 20, CHUNK_METADATA | CHUNK_DUP, &stripes(2)),
            "DUP metadata",
        ),
        (
            chunk_item(1 << 20, CHUNK_SYSTEM | CHUNK_DUP, &stripes(2)),
            "DUP system",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_METADATA, &stripes(1)),
            "mixed data and metadata",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID1C3, &stripes(3)),
            "RAID1C3",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID1C4, &stripes(4)),
            "RAID1C4",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID0, &stripes(1)),
            "RAID0 on one device",
        ),
        (raid10, "RAID10"),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID5, &stripes(2)),
            "RAID5",
        ),
        (
            chunk_item(1 << 20, CHUNK_DATA | CHUNK_RAID6, &stripes(3)),
            "RAID6",
        ),
    ];
    for (bytes, what) in cases {
        assert!(ChunkItem::parse(&bytes).is_ok(), "{what} parses");
    }
}

/// Verifies: L.btrfs.3
#[test]
fn a_chunk_must_agree_with_the_volume_sector_size() {
    let bytes = chunk_item(1 << 20, CHUNK_DATA, &stripes(1));
    let item = ChunkItem::parse(&bytes).unwrap();
    assert_eq!(
        item.check_sectorsize(0x100_0000, 4096),
        Ok(()),
        "a chunk on the sector grid, recording the volume's sector size"
    );
    assert_eq!(
        item.check_sectorsize(0x100_0200, 4096),
        Err(BtrfsError::BadChunk),
        "a chunk starting between sectors"
    );
    assert_eq!(
        item.check_sectorsize(0x100_0000, 8192),
        Err(BtrfsError::BadChunk),
        "a chunk recording a sector size the volume does not use"
    );
}

/// One system chunk array entry: a key and a chunk item.
fn sys_array_entry(key: BtrfsKey, item: &[u8]) -> Vec<u8> {
    let mut entry = Vec::new();
    entry.extend_from_slice(&key.objectid.to_le_bytes());
    entry.push(key.item_type);
    entry.extend_from_slice(&key.offset.to_le_bytes());
    entry.extend_from_slice(item);
    entry
}

/// Verifies: L.btrfs.3
#[test]
fn the_system_chunk_array_holds_only_system_chunk_items() {
    let system = chunk_item(1 << 20, CHUNK_SYSTEM, &stripes(1));
    let data = chunk_item(1 << 20, CHUNK_DATA, &stripes(1));
    let chunk_key = BtrfsKey::new(256, 228, 0x100_0000);
    for (array, what) in [
        (
            sys_array_entry(BtrfsKey::new(0, 0, 0), &system),
            "an entry keyed (0, 0, 0)",
        ),
        (
            sys_array_entry(BtrfsKey::new(256, 1, 0x100_0000), &system),
            "an entry keyed as some other item",
        ),
        (sys_array_entry(chunk_key, &data), "a data chunk"),
    ] {
        let mut storage = [ChunkMapEntry::default(); 4];
        let mut map = ChunkMap::new(&mut storage);
        assert_eq!(
            map.load_sys_chunk_array(&array),
            Err(BtrfsError::BadChunk),
            "{what} is refused"
        );
        assert!(map.is_empty(), "{what} maps nothing");
    }

    let mut storage = [ChunkMapEntry::default(); 4];
    let mut map = ChunkMap::new(&mut storage);
    assert_eq!(
        map.load_sys_chunk_array(&sys_array_entry(chunk_key, &system)),
        Ok(1),
        "a system chunk keyed as a chunk item loads"
    );
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

#[test]
fn keys_order_by_objectid_then_type_then_offset() {
    // The order is the whole basis of every lookup, and comparing the fields in
    // the wrong order still produces a total order — one that silently finds
    // the wrong item. Each pair below would compare the other way round under
    // some other field order.
    assert!(
        BtrfsKey::new(1, 200, 999) < BtrfsKey::new(2, 1, 0),
        "objectid dominates type and offset"
    );
    assert!(
        BtrfsKey::new(5, 1, 999) < BtrfsKey::new(5, 2, 0),
        "within one objectid, type dominates offset"
    );
    assert!(
        BtrfsKey::new(5, 2, 1) < BtrfsKey::new(5, 2, 2),
        "offset breaks the tie"
    );
    assert_eq!(BtrfsKey::new(7, 3, 9), BtrfsKey::new(7, 3, 9));
}

// ---------------------------------------------------------------------------
// B-tree nodes
// ---------------------------------------------------------------------------

/// Node size used by the fixtures. The smallest btrfs allows.
const NODE_SIZE: usize = 4096;

/// Build a leaf node holding `items`, each a key and its data.
///
/// Item headers grow forwards from the end of the node header while the data
/// grows *backwards* from the end of the node — a layout worth writing out by
/// hand here, because a parser that gets it the other way round still reads
/// plausible-looking bytes.
fn leaf(items: &[(BtrfsKey, Vec<u8>)], logical: u64) -> Vec<u8> {
    let mut bytes = vec![0u8; NODE_SIZE];
    put_u64(&mut bytes, 48, logical); // bytenr
    put_u64(&mut bytes, 80, 1); // generation
    put_u64(&mut bytes, 88, 5); // owner: FS_TREE
    put_u32(&mut bytes, 96, items.len() as u32); // nritems
    bytes[100] = 0; // level: a leaf

    let mut data_end = NODE_SIZE - HEADER_SIZE;
    for (slot, (key, data)) in items.iter().enumerate() {
        let item_at = HEADER_SIZE + slot * ITEM_SIZE;
        put_u64(&mut bytes, item_at, key.objectid);
        bytes[item_at + 8] = key.item_type;
        put_u64(&mut bytes, item_at + 9, key.offset);

        data_end -= data.len();
        put_u32(&mut bytes, item_at + 17, data_end as u32);
        put_u32(&mut bytes, item_at + 21, data.len() as u32);

        let absolute = HEADER_SIZE + data_end;
        bytes[absolute..absolute + data.len()].copy_from_slice(data);
    }

    let computed = crc32c(&bytes[32..]);
    bytes[0..4].copy_from_slice(&computed.to_le_bytes());
    bytes
}

/// Build an internal node holding key pointers.
fn internal(pointers: &[(BtrfsKey, u64)], logical: u64, level: u8) -> Vec<u8> {
    let mut bytes = vec![0u8; NODE_SIZE];
    put_u64(&mut bytes, 48, logical);
    put_u64(&mut bytes, 80, 1);
    put_u64(&mut bytes, 88, 5);
    put_u32(&mut bytes, 96, pointers.len() as u32);
    bytes[100] = level;

    for (slot, (key, blockptr)) in pointers.iter().enumerate() {
        let at = HEADER_SIZE + slot * KEY_PTR_SIZE;
        put_u64(&mut bytes, at, key.objectid);
        bytes[at + 8] = key.item_type;
        put_u64(&mut bytes, at + 9, key.offset);
        put_u64(&mut bytes, at + 17, *blockptr);
        put_u64(&mut bytes, at + 25, 1); // generation
    }

    let computed = crc32c(&bytes[32..]);
    bytes[0..4].copy_from_slice(&computed.to_le_bytes());
    bytes
}

fn sample_leaf() -> Vec<u8> {
    leaf(
        &[
            (BtrfsKey::new(256, 1, 0), vec![0xAA; 32]),
            (BtrfsKey::new(256, 12, 256), vec![0xBB; 17]),
            (BtrfsKey::new(257, 1, 0), vec![0xCC; 8]),
        ],
        0x4000,
    )
}

#[test]
fn a_leaf_yields_its_items_in_order() {
    let bytes = sample_leaf();
    let node = Node::parse(&bytes, 0x4000).unwrap();

    assert!(node.is_leaf(), "level zero is a leaf");
    assert_eq!(node.nritems(), 3);

    let items: Vec<_> = node.items().collect();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].key, BtrfsKey::new(256, 1, 0));
    assert_eq!(items[0].data, &[0xAA; 32][..], "the first item's payload");
    assert_eq!(items[1].key, BtrfsKey::new(256, 12, 256));
    assert_eq!(items[1].data.len(), 17);
    assert_eq!(items[2].data, &[0xCC; 8][..]);
}

#[test]
fn item_data_really_does_grow_backwards() {
    // The second item's data must sit at a *lower* offset than the first's,
    // because the data area fills from the end of the node towards the middle.
    let bytes = sample_leaf();
    let node = Node::parse(&bytes, 0x4000).unwrap();
    let items: Vec<_> = node.items().collect();

    assert!(
        items[1].offset < items[0].offset,
        "later items are stored at lower offsets: {} then {}",
        items[0].offset,
        items[1].offset
    );
}

#[test]
fn an_internal_node_yields_its_key_pointers() {
    let bytes = internal(
        &[
            (BtrfsKey::new(256, 1, 0), 0x8000),
            (BtrfsKey::new(300, 1, 0), 0xC000),
        ],
        0x4000,
        1,
    );
    let node = Node::parse(&bytes, 0x4000).unwrap();

    assert!(!node.is_leaf(), "level one is not a leaf");
    let pointers: Vec<_> = node.key_ptrs().collect();
    assert_eq!(pointers.len(), 2);
    assert_eq!(pointers[0].blockptr, 0x8000, "where the child lives");
    assert_eq!(pointers[1].key, BtrfsKey::new(300, 1, 0));
}

/// Verifies: L.btrfs.2
#[test]
fn searching_finds_a_key_and_reports_where_an_absent_one_would_go() {
    let bytes = sample_leaf();
    let node = Node::parse(&bytes, 0x4000).unwrap();

    assert_eq!(node.search(&BtrfsKey::new(256, 1, 0)), Ok(0));
    assert_eq!(node.search(&BtrfsKey::new(257, 1, 0)), Ok(2));
    assert_eq!(
        node.search(&BtrfsKey::new(256, 5, 0)),
        Err(1),
        "an absent key reports the slot it would occupy"
    );
    assert_eq!(
        node.search(&BtrfsKey::new(1, 1, 0)),
        Err(0),
        "before everything"
    );
    assert_eq!(
        node.search(&BtrfsKey::new(9999, 1, 0)),
        Err(3),
        "after everything"
    );
}

/// Verifies: L.btrfs.2
#[test]
fn a_node_read_from_the_wrong_address_is_refused() {
    // The header records where the node was written. If it disagrees with where
    // it was read from, the chunk mapping sent the read to the wrong place —
    // and the bytes will parse perfectly, which is precisely the danger.
    let bytes = sample_leaf();
    match Node::parse(&bytes, 0x8000) {
        Err(BtrfsError::WrongAddress { expected, found }) => {
            assert_eq!(expected, 0x8000);
            assert_eq!(found, 0x4000);
        }
        other => panic!("expected a wrong-address error, got {other:?}"),
    }
}

/// Verifies: L.btrfs.2, H.STORE.2
#[test]
fn a_node_with_a_bad_checksum_is_refused() {
    let mut bytes = sample_leaf();
    bytes[HEADER_SIZE + 4] ^= 0xFF;
    assert!(
        matches!(
            Node::parse(&bytes, 0x4000),
            Err(BtrfsError::BadChecksum { .. })
        ),
        "a node that does not check out must not be trusted"
    );
}

/// Verifies: L.btrfs.2
#[test]
fn a_node_claiming_more_items_than_it_can_hold_is_refused() {
    let mut bytes = sample_leaf();
    put_u32(&mut bytes, 96, 100_000);
    let computed = crc32c(&bytes[32..]);
    bytes[0..4].copy_from_slice(&computed.to_le_bytes());

    assert!(
        matches!(
            Node::parse(&bytes, 0x4000),
            Err(BtrfsError::TooManyItems { .. })
        ),
        "nritems is attacker-controlled and must be checked against the node size"
    );
}

/// Verifies: L.btrfs.2
#[test]
fn an_item_running_past_the_node_is_refused() {
    let mut bytes = sample_leaf();
    // Inflate the first item's size so its data would run off the end.
    put_u32(&mut bytes, HEADER_SIZE + 21, 60_000);
    let computed = crc32c(&bytes[32..]);
    bytes[0..4].copy_from_slice(&computed.to_le_bytes());

    assert!(
        matches!(
            Node::parse(&bytes, 0x4000),
            Err(BtrfsError::ItemOutOfBounds { .. })
        ),
        "an item extent outside the node must be caught before it is read"
    );
}

/// Verifies: L.btrfs.2
#[test]
fn items_out_of_order_are_refused() {
    let bytes = leaf(
        &[
            (BtrfsKey::new(500, 1, 0), vec![0xAA; 8]),
            (BtrfsKey::new(100, 1, 0), vec![0xBB; 8]),
        ],
        0x4000,
    );
    assert!(
        matches!(
            Node::parse(&bytes, 0x4000),
            Err(BtrfsError::ItemsOutOfOrder { .. })
        ),
        "the binary search assumes order; an unordered node would silently mislead it"
    );
}

/// Rewrite leaf item `slot`'s payload offset and size, and checksum the node
/// again so only the layout is wrong.
fn move_payload(bytes: &mut [u8], slot: usize, offset: u32, size: u32) {
    let at = HEADER_SIZE + slot * ITEM_SIZE;
    put_u32(bytes, at + 17, offset);
    put_u32(bytes, at + 21, size);
    let computed = crc32c(&bytes[32..]);
    bytes[0..4].copy_from_slice(&computed.to_le_bytes());
}

/// Verifies: L.btrfs.2
#[test]
fn leaf_payloads_that_are_not_packed_back_to_back_are_refused() {
    // `sample_leaf` packs 32, 17 and 8 bytes down from the end of the node:
    // item 0 at 3963..3995, item 1 at 3946..3963, item 2 at 3938..3946, all
    // measured from the end of the header.
    let end = (NODE_SIZE - HEADER_SIZE) as u32;
    for (slot, offset, size, what) in [
        (1, end - 32, 17, "item 1 sharing item 0's bytes"),
        (1, end - 32 - 18, 17, "a one-byte gap between items 0 and 1"),
        (0, end - 33, 32, "item 0 ending a byte short of the node"),
        (
            2,
            end - 32 - 17 - 4,
            8,
            "item 2 overlapping item 1 by four bytes",
        ),
    ] {
        let mut bytes = sample_leaf();
        move_payload(&mut bytes, slot, offset, size);
        assert_eq!(
            Node::parse(&bytes, 0x4000).unwrap_err(),
            BtrfsError::ItemOutOfBounds { slot: slot as u32 },
            "{what} is refused"
        );
    }
}

/// Verifies: L.btrfs.2
#[test]
fn a_packed_leaf_parses_up_to_its_boundaries() {
    // An empty payload between two others takes no bytes, and a single item
    // may fill the node exactly up to its own descriptor.
    let bytes = leaf(
        &[
            (BtrfsKey::new(256, 1, 0), vec![0xAA; 16]),
            (BtrfsKey::new(256, 2, 0), Vec::new()),
            (BtrfsKey::new(256, 3, 0), vec![0xCC; 4]),
        ],
        0x4000,
    );
    let node = Node::parse(&bytes, 0x4000).unwrap();
    assert_eq!(node.item(1).unwrap().data, &[] as &[u8]);
    assert_eq!(node.item(2).unwrap().data, &[0xCC; 4]);

    let room = NODE_SIZE - HEADER_SIZE - ITEM_SIZE;
    let full = leaf(&[(BtrfsKey::new(256, 1, 0), vec![0xDD; room])], 0x4000);
    assert!(
        Node::parse(&full, 0x4000).is_ok(),
        "a payload reaching exactly to the descriptor array"
    );
    let mut over = full;
    move_payload(&mut over, 0, (ITEM_SIZE - 1) as u32, (room + 1) as u32);
    assert_eq!(
        Node::parse(&over, 0x4000).unwrap_err(),
        BtrfsError::ItemOutOfBounds { slot: 0 },
        "one byte further overlaps the descriptor"
    );
}

/// Verifies: L.btrfs.2
#[test]
fn a_node_deeper_than_any_tree_is_refused() {
    // Level 7 is the root of the deepest tree btrfs builds; 8 is no node at
    // all, and a standalone parse must say so without a walker's help.
    let pointers = [(BtrfsKey::new(256, 1, 0), 0x8000)];
    let deepest = internal(&pointers, 0x4000, 7);
    assert!(
        Node::parse(&deepest, 0x4000).is_ok(),
        "level 7 is the deepest a tree goes"
    );
    for level in [8, 0xFF] {
        let bytes = internal(&pointers, 0x4000, level);
        assert_eq!(
            Node::parse_unchecked_address(&bytes).unwrap_err(),
            BtrfsError::BadTree { logical: 0x4000 },
            "level {level} is deeper than btrfs allows"
        );
    }
}

/// Verifies: L.btrfs.2
#[test]
fn a_truncated_node_is_refused() {
    let bytes = sample_leaf();
    for length in [0, 50, HEADER_SIZE - 1, HEADER_SIZE + 10] {
        assert!(
            Node::parse(&bytes[..length], 0x4000).is_err(),
            "a {length}-byte node must not parse"
        );
    }
}

// ---------------------------------------------------------------------------
// Item payloads
// ---------------------------------------------------------------------------

/// Verifies: L.btrfs.4
#[test]
fn an_inode_item_is_decoded() {
    let mut bytes = vec![0u8; 160];
    put_u64(&mut bytes, 0, 7); // generation
    put_u64(&mut bytes, 16, 4096); // size
    put_u64(&mut bytes, 24, 8192); // nbytes
    put_u32(&mut bytes, 40, 1); // nlink
    put_u32(&mut bytes, 44, 1000); // uid
    put_u32(&mut bytes, 48, 1000); // gid
    put_u32(&mut bytes, 52, 0o100_644); // mode: a regular file

    let inode = InodeItem::parse(&bytes).unwrap();
    assert_eq!(inode.size, 4096, "the file's length");
    assert_eq!(inode.uid, 1000);
    assert!(inode.is_file(), "mode 0o100644 is a regular file");
    assert!(!inode.is_dir());
    assert!(!inode.is_symlink());
}

/// Verifies: L.btrfs.4
#[test]
fn a_truncated_inode_item_is_refused() {
    let bytes = vec![0u8; 100];
    assert!(
        InodeItem::parse(&bytes).is_err(),
        "an inode item is 160 bytes; a short one must not be read past"
    );
}

/// Verifies: L.btrfs.4
#[test]
fn several_directory_entries_pack_into_one_item() {
    // Names whose hashes collide land in the same item, so a reader that stops
    // after the first entry loses files — and only for directories unlucky
    // enough to collide, which is the worst kind of bug to find later.
    let mut bytes = Vec::new();
    for (name, inode) in [(&b"one"[..], 257u64), (&b"two"[..], 258)] {
        bytes.extend_from_slice(&inode.to_le_bytes()); // location objectid
        bytes.push(1); // location type: INODE_ITEM
        bytes.extend_from_slice(&0u64.to_le_bytes()); // location offset
        bytes.extend_from_slice(&1u64.to_le_bytes()); // transid
        bytes.extend_from_slice(&0u16.to_le_bytes()); // data_len
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.push(1); // type: regular file
        bytes.extend_from_slice(name);
    }

    let entries: Vec<_> = DirItemIter::new(&bytes, 84).map(Result::unwrap).collect();
    assert_eq!(entries.len(), 2, "both entries must be found");
    assert_eq!(entries[0].name, b"one");
    assert_eq!(entries[0].location.objectid, 257);
    assert_eq!(entries[1].name, b"two");
    assert_eq!(entries[1].location.objectid, 258);
}

/// Verifies: L.btrfs.4
#[test]
fn a_directory_item_with_a_name_past_the_end_stops_cleanly() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&257u64.to_le_bytes());
    bytes.push(1);
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&9999u16.to_le_bytes()); // a name far longer than what follows
    bytes.push(1);
    bytes.extend_from_slice(b"short");

    let entries: Vec<_> = DirItemIter::new(&bytes, 84).collect();
    assert!(
        entries.iter().all(Result::is_err) || entries.is_empty(),
        "an entry whose name runs past the item must be refused, not read past"
    );
}

/// Directory item key types, written out independently of the parser's.
const XATTR_ITEM: u8 = 24;
const DIR_ITEM: u8 = 84;
const DIR_INDEX: u8 = 96;
/// Directory entry types.
const FT_REG_FILE: u8 = 1;
const FT_XATTR: u8 = 8;

/// One directory entry or attribute record, naming inode 257.
fn dir_record(name: &[u8], data: &[u8], kind: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&257u64.to_le_bytes()); // location objectid
    bytes.push(1); // location type: INODE_ITEM
    bytes.extend_from_slice(&0u64.to_le_bytes()); // location offset
    bytes.extend_from_slice(&1u64.to_le_bytes()); // transid
    bytes.extend_from_slice(&(data.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(data);
    bytes
}

/// Whether every record in `payload` parses as `item_type`.
fn dir_records_parse(payload: &[u8], item_type: u8) -> bool {
    DirItemIter::new(payload, item_type).all(|entry| entry.is_ok())
}

/// Verifies: L.btrfs.4
#[test]
fn a_directory_entry_btrfs_would_not_write_is_refused() {
    let longest = [b'n'; 255];
    let too_long = [b'n'; 256];
    for (payload, item_type, what) in [
        (
            dir_record(&too_long, b"", FT_REG_FILE),
            DIR_ITEM,
            "a 256-byte name",
        ),
        (
            dir_record(&too_long, b"", FT_REG_FILE),
            DIR_INDEX,
            "a 256-byte name in the index",
        ),
        (
            dir_record(b"file", b"data", FT_REG_FILE),
            DIR_ITEM,
            "a directory entry carrying data",
        ),
        (
            dir_record(b"file", b"data", FT_REG_FILE),
            DIR_INDEX,
            "an index entry carrying data",
        ),
        (
            dir_record(b"user.a", b"v", FT_REG_FILE),
            XATTR_ITEM,
            "an attribute key holding a file entry",
        ),
        (
            dir_record(b"file", b"", FT_XATTR),
            DIR_ITEM,
            "a directory key holding an attribute",
        ),
        (dir_record(b"file", b"", 0), DIR_ITEM, "entry type 0"),
        (dir_record(b"file", b"", 9), DIR_ITEM, "entry type 9"),
        (
            dir_record(&too_long, b"v", FT_XATTR),
            XATTR_ITEM,
            "a 256-byte attribute name",
        ),
    ] {
        assert!(!dir_records_parse(&payload, item_type), "{what} is refused");
    }

    for (payload, item_type, what) in [
        (
            dir_record(&longest, b"", FT_REG_FILE),
            DIR_ITEM,
            "a 255-byte name",
        ),
        (
            dir_record(&longest, b"", 7),
            DIR_INDEX,
            "a 255-byte symlink name in the index",
        ),
        (
            dir_record(&longest, &[0xAA; 300], FT_XATTR),
            XATTR_ITEM,
            "a 255-byte attribute name with a value",
        ),
    ] {
        assert!(dir_records_parse(&payload, item_type), "{what} parses");
    }
}

/// One `INODE_REF` record.
fn inode_ref(name: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&2u64.to_le_bytes()); // index
    bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
    bytes.extend_from_slice(name);
    bytes
}

/// Verifies: L.btrfs.4
#[test]
fn an_inode_ref_name_is_between_one_and_255_bytes() {
    let parses = |payload: &[u8]| InodeRefIter::new(payload).all(|entry| entry.is_ok());
    assert!(parses(&inode_ref(&[b'r'; 255])), "a 255-byte name parses");
    assert!(parses(&inode_ref(b"r")), "a one-byte name parses");
    assert!(
        !parses(&inode_ref(&[b'r'; 256])),
        "a 256-byte name is refused"
    );
    assert!(!parses(&inode_ref(b"")), "an empty name is refused");
}

/// Verifies: L.btrfs.4
#[test]
fn extent_data_is_decoded_in_all_three_forms() {
    // Inline: the data follows the twenty-one byte header.
    let mut inline = vec![0u8; 21];
    put_u64(&mut inline, 8, 5); // ram_bytes
    inline[20] = 0; // type: inline
    inline.extend_from_slice(b"hello");
    let extent = ExtentData::parse(&inline).unwrap();
    assert!(matches!(extent.body, ExtentDataBody::Inline(b"hello")));
    assert_eq!(extent.inline_data(), Some(&b"hello"[..]));
    assert_eq!(
        extent.file_extent(),
        None,
        "an inline extent occupies no blocks"
    );

    // Regular: a reference to blocks elsewhere.
    let mut regular = vec![0u8; 53];
    put_u64(&mut regular, 8, 4096); // ram_bytes
    regular[20] = 1; // type: regular
    put_u64(&mut regular, 21, 0x50_0000); // disk_bytenr
    put_u64(&mut regular, 29, 4096); // disk_num_bytes
    put_u64(&mut regular, 37, 0); // offset
    put_u64(&mut regular, 45, 4096); // num_bytes
    let extent = ExtentData::parse(&regular).unwrap();
    let file_extent = extent.file_extent().unwrap();
    assert_eq!(file_extent.disk_bytenr, 0x50_0000);
    assert_eq!(file_extent.num_bytes, 4096);
    assert!(!file_extent.is_hole(), "a real extent is not a hole");
    assert_eq!(file_extent.start(), Some(0x50_0000));

    // A hole: disk_bytenr zero means the range reads as zeroes.
    let mut hole = regular.clone();
    put_u64(&mut hole, 21, 0);
    let extent = ExtentData::parse(&hole).unwrap();
    assert!(
        extent.file_extent().unwrap().is_hole(),
        "disk_bytenr zero is a hole, not block zero"
    );
}

/// Verifies: L.btrfs.4
#[test]
fn a_truncated_extent_item_is_refused() {
    let bytes = vec![0u8; 10];
    assert!(
        ExtentData::parse(&bytes).is_err(),
        "an extent header is 21 bytes"
    );
}

/// A regular (`kind` 1) or preallocated (2) extent item.
fn file_extent(
    kind: u8,
    compression: u8,
    ram: u64,
    disk: (u64, u64),
    offset: u64,
    num: u64,
) -> Vec<u8> {
    let mut bytes = vec![0u8; 53];
    put_u64(&mut bytes, 0, 9); // generation
    put_u64(&mut bytes, 8, ram);
    bytes[16] = compression;
    bytes[20] = kind;
    put_u64(&mut bytes, 21, disk.0); // disk_bytenr
    put_u64(&mut bytes, 29, disk.1); // disk_num_bytes
    put_u64(&mut bytes, 37, offset);
    put_u64(&mut bytes, 45, num);
    bytes
}

/// An inline extent item holding `data`.
fn inline_extent(compression: u8, ram: u64, data: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 21];
    put_u64(&mut bytes, 8, ram);
    bytes[16] = compression;
    bytes[20] = 0;
    bytes.extend_from_slice(data);
    bytes
}

/// Check an extent item filed at file offset `at`, on a 4 KiB-sector volume.
fn check_extent(at: u64, bytes: &[u8]) -> Result<(), BtrfsError> {
    ExtentData::parse_item(&BtrfsKey::new(257, 108, at), bytes, 4096).map(|_| ())
}

/// Verifies: L.btrfs.4
#[test]
fn an_extent_btrfs_would_not_write_is_refused() {
    const REG: u8 = 1;
    const PREALLOC: u8 = 2;
    const DISK: u64 = 0x50_0000;
    const TOP: u64 = 0xFFFF_FFFF_FFFF_F000;
    let bad = Err(BtrfsError::BadItem { item_type: 108 });
    let mut encrypted = file_extent(REG, 0, 4096, (DISK, 4096), 0, 4096);
    encrypted[17] = 1;
    let mut encoded = file_extent(REG, 0, 4096, (DISK, 4096), 0, 4096);
    put_u16(&mut encoded, 18, 1);
    let mut long = file_extent(REG, 0, 4096, (DISK, 4096), 0, 4096);
    long.push(0);

    let cases: Vec<(u64, Vec<u8>, &str)> = vec![
        (
            0,
            file_extent(REG, 0, 8192, (DISK, 8192), 4096, 8192),
            "a reference running past its extent into the next",
        ),
        (
            0,
            file_extent(REG, 1, 8192, (DISK, 4096), 4096, 8192),
            "a compressed reference running past what the extent expands to",
        ),
        (
            0,
            file_extent(PREALLOC, 0, 4096, (DISK, 4096), 0, 8192),
            "a preallocated reference running past its extent",
        ),
        (
            0,
            file_extent(REG, 0, 4096, (DISK, 4096), 0, 0),
            "no bytes at all",
        ),
        (
            0,
            file_extent(REG, 0, 4096, (DISK, 4096), 0, 4095),
            "num_bytes off the sector grid",
        ),
        (
            0,
            file_extent(REG, 0, 4096, (DISK + 1, 4096), 0, 4096),
            "disk_bytenr off the sector grid",
        ),
        (
            0,
            file_extent(REG, 0, 4097, (DISK, 4096), 0, 4096),
            "ram_bytes off the sector grid",
        ),
        (
            0,
            file_extent(REG, 0, 4096, (DISK, 4096), TOP, 4096),
            "offset and num_bytes overflowing",
        ),
        (
            TOP,
            file_extent(REG, 0, 1 << 20, (0, 0), 0, 4096),
            "an extent whose end overflows the file offset",
        ),
        (
            4097,
            file_extent(REG, 0, 4096, (DISK, 4096), 0, 4096),
            "a file offset off the sector grid",
        ),
        (0, encrypted, "an encrypted extent"),
        (0, encoded, "another encoding"),
        (0, long, "a regular extent item longer than 53 bytes"),
        (
            4096,
            inline_extent(0, 5, b"hello"),
            "an inline extent past offset 0",
        ),
        (
            0,
            inline_extent(0, 6, b"hello"),
            "an inline extent shorter than its ram_bytes",
        ),
    ];
    for (at, bytes, what) in cases {
        assert_eq!(check_extent(at, &bytes), bad, "{what} is refused");
    }
    assert_eq!(
        check_extent(0, &file_extent(REG, 4, 4096, (DISK, 4096), 0, 4096)),
        Err(BtrfsError::UnsupportedCompression(4)),
        "a compression type Linux does not define"
    );
}

/// Verifies: L.btrfs.4
#[test]
fn every_extent_btrfs_writes_still_parses() {
    const DISK: u64 = 0x50_0000;
    let cases: [(u64, Vec<u8>, &str); 7] = [
        (
            4096,
            file_extent(1, 0, 8192, (DISK, 8192), 4096, 4096),
            "the tail of an extent, exactly to its end",
        ),
        (
            0,
            file_extent(1, 3, 8192, (DISK, 4096), 4096, 4096),
            "the tail of a compressed extent",
        ),
        (
            8192,
            file_extent(1, 0, 1 << 20, (0, 0), 0, 1 << 20),
            "a hole, which names no extent",
        ),
        (
            0,
            file_extent(2, 0, 8192, (DISK, 8192), 0, 8192),
            "a preallocated extent",
        ),
        (
            0xFFFF_FFFF_FFFF_E000,
            file_extent(1, 0, 4096, (0, 0), 0, 4096),
            "an extent ending at the last representable sector",
        ),
        (0, inline_extent(0, 5, b"hello"), "an inline extent"),
        (
            0,
            inline_extent(1, 100, b"deflate"),
            "a compressed inline extent, shorter than it expands",
        ),
    ];
    for (at, bytes, what) in cases {
        assert_eq!(check_extent(at, &bytes), Ok(()), "{what} parses");
    }
}

/// Verifies: L.btrfs.4
#[test]
fn an_extent_ends_where_linux_says_it_does() {
    let key = |at| BtrfsKey::new(257, 108, at);
    let inline = inline_extent(0, 5, b"hello");
    let inline = ExtentData::parse(&inline).unwrap();
    assert_eq!(
        inline.end(&key(0), 4096),
        Some(4096),
        "an inline extent owns the rest of its sector"
    );
    let regular = file_extent(1, 0, 8192, (0x50_0000, 8192), 0, 4096);
    let regular = ExtentData::parse(&regular).unwrap();
    assert_eq!(regular.end(&key(8192), 4096), Some(12288));
    let huge = inline_extent(1, u64::MAX, b"x");
    assert_eq!(
        ExtentData::parse(&huge).unwrap().end(&key(0), 4096),
        None,
        "an end past u64::MAX is no end"
    );
}

// ---------------------------------------------------------------------------
// The one that matters most
// ---------------------------------------------------------------------------

/// Verifies: L.btrfs.1, L.btrfs.2, H.STORE.1
#[test]
fn parsing_never_panics_on_corrupt_input() {
    // A disk can be corrupt, and from stage 11 this runs in ring 0 with nothing
    // above it. Every accessor must answer or error for any bytes at all.
    let superblock = SuperblockBuilder::new().build();
    let node = sample_leaf();

    for (name, good) in [("superblock", &superblock), ("node", &node)] {
        for index in 0..good.len().min(1024) {
            for patch in [0x00u8, 0x01, 0x7F, 0xFF] {
                let mut bytes = good.clone();
                bytes[index] = patch;

                if name == "superblock" {
                    poke_superblock(&bytes);
                } else {
                    poke_node(&bytes);
                }
            }
        }
    }
}

/// Call every superblock accessor, requiring only that none of them panics.
fn poke_superblock(bytes: &[u8]) {
    let Ok(parsed) = Superblock::parse(bytes) else {
        return;
    };
    let _ = parsed.label();
    let _ = parsed.sectorsize();
    let _ = parsed.nodesize();
    let _ = parsed.incompat_flags().unknown();

    let mut storage = [ChunkMapEntry::default(); 4];
    let mut map = ChunkMap::new(&mut storage);
    let _ = map.load_sys_chunk_array(parsed.sys_chunk_array_bytes());
}

/// Call every node accessor, and every item parser on every item's payload.
fn poke_node(bytes: &[u8]) {
    let Ok(parsed) = Node::parse_unchecked_address(bytes) else {
        return;
    };
    for item in parsed.items() {
        poke_item(item.data);
    }
    for pointer in parsed.key_ptrs() {
        let _ = pointer.blockptr;
    }
    let _ = parsed.search(&BtrfsKey::new(256, 1, 0));
}

/// Every item payload parser, on bytes that may be anything at all.
fn poke_item(data: &[u8]) {
    let _ = InodeItem::parse(data);
    let _ = ExtentData::parse(data);
    for entry in DirItemIter::new(data, 84) {
        let _ = entry.map(|found| found.name.len());
    }
}

/// Verifies: L.btrfs.1, L.btrfs.2
#[test]
fn parsing_never_panics_on_arbitrary_short_input() {
    for length in 0..300usize {
        let bytes = vec![0xFFu8; length];
        let _ = Superblock::parse(&bytes);
        let _ = Node::parse_unchecked_address(&bytes);
        let _ = ChunkItem::parse(&bytes);
        let _ = InodeItem::parse(&bytes);
        let _ = ExtentData::parse(&bytes);
        for entry in DirItemIter::new(&bytes, 84) {
            let _ = entry.map(|found| found.name.len());
        }
    }
}

#[test]
fn the_lookup_table_matches_the_polynomial() {
    // The 256-entry table is written out as literals, so nothing in the build
    // proves it corresponds to the polynomial it claims to. This recomputes
    // every entry the slow way -- eight conditional shifts per byte, straight
    // from the definition -- and compares. A single mistyped digit in the table
    // would produce checksums that are wrong only for some inputs, which is
    // the hardest kind of wrong to notice.
    for byte in 0u32..256 {
        let mut expected = byte;
        for _ in 0..8 {
            expected = if expected & 1 == 1 {
                (expected >> 1) ^ POLYNOMIAL
            } else {
                expected >> 1
            };
        }
        assert_eq!(
            TABLE[byte as usize], expected,
            "table entry {byte} does not match the Castagnoli polynomial"
        );
    }
}

/// One `INODE_EXTREF` record.
fn inode_extref(parent: u64, index: u64, name: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&parent.to_le_bytes());
    bytes.extend_from_slice(&index.to_le_bytes());
    bytes.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(name);
    bytes
}

#[test]
fn the_extref_hash_is_the_key_offset_mkfs_btrfs_files_extrefs_under() {
    // From `btrfs inspect-internal dump-tree` of an image mkfs.btrfs 6.17.1
    // made from a directory holding 300 hard links to one file, which forces
    // INODE_EXTREF: `key (258 INODE_EXTREF 16484832)` names `link-160-` and 200
    // zeros in directory 257, and `key (258 INODE_EXTREF 37574397)` names
    // `link-178-` and 200 zeros there.
    let name = |prefix: &[u8]| {
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(&[b'0'; 200]);
        bytes
    };
    assert_eq!(items::extref_hash(257, &name(b"link-160-")), 16_484_832);
    assert_eq!(items::extref_hash(257, &name(b"link-178-")), 37_574_397);
}

/// Verifies: L.btrfs.4
#[test]
fn inode_extref_records_parse_and_ones_linux_refuses_are_refused() {
    use crate::items::{
        FIRST_FREE_OBJECTID, INODE_EXTREF_KEY, InodeExtrefIter, LAST_FREE_OBJECTID,
    };
    let mut two = inode_extref(257, 7, b"first");
    two.extend(inode_extref(300, 8, b"second"));
    let records: Vec<_> = InodeExtrefIter::new(&two).map(Result::unwrap).collect();
    assert_eq!(records.len(), 2, "records follow one another");
    assert_eq!(
        (records[0].parent, records[0].index, records[0].name),
        (257, 7, &b"first"[..])
    );
    assert_eq!(
        (records[1].parent, records[1].index, records[1].name),
        (300, 8, &b"second"[..])
    );

    let bad = BtrfsError::BadItem {
        item_type: INODE_EXTREF_KEY,
    };
    let refused = |payload: &[u8]| InodeExtrefIter::new(payload).any(|record| record == Err(bad));
    assert!(
        !refused(&inode_extref(257, 1, &[b'x'; 255])),
        "a 255-byte name parses"
    );
    assert!(
        refused(&inode_extref(257, 1, b"")),
        "an empty name is refused"
    );
    assert!(
        refused(&inode_extref(257, 1, &[b'x'; 256])),
        "so is one over 255 bytes"
    );
    assert!(
        refused(&inode_extref(FIRST_FREE_OBJECTID - 1, 1, b"x")),
        "a parent below the first free id is no directory a file is in"
    );
    assert!(
        refused(&inode_extref(LAST_FREE_OBJECTID + 1, 1, b"x")),
        "nor is one above the last"
    );
    let whole = inode_extref(257, 1, b"name");
    assert!(
        refused(&whole[..whole.len() - 1]),
        "a name cut short is refused"
    );
    assert!(refused(&whole[..17]), "and so is a header cut short");
}
