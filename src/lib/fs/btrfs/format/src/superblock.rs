//! The btrfs superblock.
//!
//! The superblock is the only structure on a btrfs volume with a fixed
//! location, which is what makes it the entry point: everything else is found
//! by following a logical address out of it, and logical addresses mean nothing
//! until the system chunk array it carries has been loaded.
//!
//! It is written at three offsets, so a volume survives losing the head of the
//! disk. All copies are updated on every commit, so the highest `generation`
//! among the valid ones is the current filesystem.

use crate::chunk::SysChunkArray;
use crate::tree::{MAX_LEVEL, MAX_NODE_SIZE, MIN_NODE_SIZE};
use crate::{
    BtrfsError, array_at, is_valid_block_size, slice_at, truncated, u8_at, u16_at, u32_at, u64_at,
    verify_crc32c,
};

/// The primary superblock, at 64 KiB.
///
/// Deliberately past the first 64 KiB so that a partition table, a boot sector
/// or another filesystem's superblock can be overwritten without destroying
/// this one, and so that btrfs can be created on a disk that already has one.
pub const PRIMARY_OFFSET: u64 = 0x1_0000;

/// The first mirror, at 64 MiB.
pub const MIRROR1_OFFSET: u64 = 0x400_0000;

/// The second mirror, at 256 GiB.
pub const MIRROR2_OFFSET: u64 = 0x40_0000_0000;

/// All three superblock offsets, in the order a reader should try them.
pub const SUPERBLOCK_OFFSETS: [u64; 3] = [PRIMARY_OFFSET, MIRROR1_OFFSET, MIRROR2_OFFSET];

/// Bytes a superblock occupies, and the extent of its checksum.
pub const SUPERBLOCK_SIZE: usize = 4096;

/// The eight bytes at offset 64 that identify a btrfs superblock.
pub const MAGIC: [u8; 8] = *b"_BHRfS_M";

/// Bytes reserved for the system chunk array at the end of the superblock.
pub const SYS_CHUNK_ARRAY_SIZE: usize = 2048;

/// Offset of the system chunk array within the superblock, `0x32b`.
const SYS_CHUNK_ARRAY_OFFSET: usize = 811;

/// Offset of the embedded `DEV_ITEM` describing the device this copy was read
/// from.
const DEV_ITEM_OFFSET: usize = 201;

/// Offset of the 256-byte label.
const LABEL_OFFSET: usize = 299;

/// Length of the label field, including its terminating NUL.
const LABEL_SIZE: usize = 256;

/// Smallest sector size btrfs supports.
pub const MIN_SECTOR_SIZE: u32 = 512;

/// Largest sector size btrfs supports.
pub const MAX_SECTOR_SIZE: u32 = 65536;

// ---------------------------------------------------------------------------
// Checksum algorithms
// ---------------------------------------------------------------------------

/// The algorithm named by the superblock's `csum_type`.
///
/// Only [`ChecksumType::Crc32c`] is implemented here; the others are recognised
/// so that a volume using one is refused with a message that names it rather
/// than being reported as corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumType {
    /// `0`: CRC-32C, the original and still the default.
    Crc32c,
    /// `1`: xxhash64, truncated into the 32-byte field.
    XxHash64,
    /// `2`: SHA-256.
    Sha256,
    /// `3`: `BLAKE2b`, truncated to 256 bits.
    Blake2b,
    /// Anything else, carrying the raw value.
    Unknown(u16),
}

impl ChecksumType {
    /// Classify a raw `csum_type`.
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        match raw {
            0 => ChecksumType::Crc32c,
            1 => ChecksumType::XxHash64,
            2 => ChecksumType::Sha256,
            3 => ChecksumType::Blake2b,
            other => ChecksumType::Unknown(other),
        }
    }

    /// Whether this crate can verify checksums of this type.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, ChecksumType::Crc32c)
    }
}

// ---------------------------------------------------------------------------
// Incompatible feature flags
// ---------------------------------------------------------------------------

/// The `incompat_flags` word, decoded.
///
/// A bit set here that the reader does not understand means the on-disk layout
/// has changed in a way that makes the volume unreadable, not merely
/// unwriteable — hence [`IncompatFlags::unknown`], which is the check a mount
/// path must actually perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncompatFlags(u64);

impl IncompatFlags {
    /// Extent backrefs record the tree they came from rather than a full path.
    /// Set on every filesystem made this decade.
    pub const MIXED_BACKREF: u64 = 1 << 0;
    /// The default subvolume is something other than the top-level tree.
    pub const DEFAULT_SUBVOL: u64 = 1 << 1;
    /// Data and metadata share block groups, used on very small volumes.
    pub const MIXED_GROUPS: u64 = 1 << 2;
    /// Some extent somewhere is LZO-compressed.
    pub const COMPRESS_LZO: u64 = 1 << 3;
    /// Some extent somewhere is zstd-compressed.
    pub const COMPRESS_ZSTD: u64 = 1 << 4;
    /// Metadata blocks may be larger than one page, which is why `nodesize`
    /// must be read from the superblock rather than assumed.
    pub const BIG_METADATA: u64 = 1 << 5;
    /// Inode backrefs may use the extended form, `INODE_EXTREF`.
    pub const EXTENDED_IREF: u64 = 1 << 6;
    /// The volume contains RAID5 or RAID6 chunks.
    pub const RAID56: u64 = 1 << 7;
    /// Metadata extent items omit the redundant key fields, changing the layout
    /// of the extent tree.
    pub const SKINNY_METADATA: u64 = 1 << 8;
    /// Holes in a file are implied by missing `EXTENT_DATA` items instead of
    /// being recorded explicitly, so a read path must handle a gap between
    /// extents rather than treating it as corruption.
    pub const NO_HOLES: u64 = 1 << 9;
    /// The volume contains three- or four-way mirrored chunks.
    pub const RAID1C34: u64 = 1 << 11;

    /// Every bit this crate recognises.
    pub const KNOWN: u64 = Self::MIXED_BACKREF
        | Self::DEFAULT_SUBVOL
        | Self::MIXED_GROUPS
        | Self::COMPRESS_LZO
        | Self::COMPRESS_ZSTD
        | Self::BIG_METADATA
        | Self::EXTENDED_IREF
        | Self::RAID56
        | Self::SKINNY_METADATA
        | Self::NO_HOLES
        | Self::RAID1C34;

    /// Wrap a raw flags word.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        IncompatFlags(bits)
    }

    /// The raw flags word.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether all of `mask` is set.
    #[must_use]
    pub const fn contains(self, mask: u64) -> bool {
        self.0 & mask == mask
    }

    /// Bits set that this crate does not know about. Non-zero means the volume
    /// must not be read, because an unknown incompatible feature can have
    /// changed the meaning of any structure.
    #[must_use]
    pub const fn unknown(self) -> u64 {
        self.0 & !Self::KNOWN
    }

    /// Whether extent items use the skinny layout.
    #[must_use]
    pub const fn skinny_metadata(self) -> bool {
        self.contains(Self::SKINNY_METADATA)
    }

    /// Whether file holes are implied rather than recorded.
    #[must_use]
    pub const fn no_holes(self) -> bool {
        self.contains(Self::NO_HOLES)
    }

    /// Whether inode backrefs may use the extended form.
    #[must_use]
    pub const fn extended_iref(self) -> bool {
        self.contains(Self::EXTENDED_IREF)
    }
}

// ---------------------------------------------------------------------------
// Superblock offsets
// ---------------------------------------------------------------------------

/// Iterator over the superblock offsets that fit on a device of a given size.
///
/// A mirror only exists if the whole 4 KiB copy fits, so a 32 MiB volume has
/// exactly one superblock and reading at 64 MiB would either fault or return
/// somebody else's data.
#[derive(Debug, Clone, Copy)]
pub struct SuperblockOffsets {
    device_size: u64,
    next: usize,
}

impl SuperblockOffsets {
    /// The offsets valid on a device of `device_size` bytes.
    #[must_use]
    pub const fn new(device_size: u64) -> Self {
        SuperblockOffsets {
            device_size,
            next: 0,
        }
    }
}

impl Iterator for SuperblockOffsets {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        loop {
            let offset = *SUPERBLOCK_OFFSETS.get(self.next)?;
            self.next = self.next.checked_add(1)?;
            if offset
                .checked_add(SUPERBLOCK_SIZE as u64)
                .is_some_and(|e| e <= self.device_size)
            {
                return Some(offset);
            }
        }
    }
}

/// Whether a superblock copy at `offset` fits entirely on a device of
/// `device_size` bytes.
#[must_use]
pub fn mirror_fits(offset: u64, device_size: u64) -> bool {
    offset
        .checked_add(SUPERBLOCK_SIZE as u64)
        .is_some_and(|end| end <= device_size)
}

// ---------------------------------------------------------------------------
// The superblock itself
// ---------------------------------------------------------------------------

/// A validated superblock borrowing the 4 KiB it was read from.
///
/// [`Superblock::parse`] has already checked the magic, the checksum type, the
/// checksum itself, the block sizes and how they relate, and the levels and
/// alignment of the tree roots, so every accessor is a plain field read.
#[derive(Debug, Clone, Copy)]
pub struct Superblock<'a> {
    bytes: &'a [u8],
}

impl<'a> Superblock<'a> {
    /// Parse and validate a superblock.
    ///
    /// `bytes` may be longer than 4 KiB — a whole sector read is fine — and
    /// only the first [`SUPERBLOCK_SIZE`] bytes are used, because that is
    /// exactly the span the checksum covers.
    ///
    /// The order of checks matters: magic first, so a non-btrfs device is
    /// reported as such rather than as a checksum failure; then the checksum
    /// type, so an unimplemented algorithm is not verified as if it were
    /// CRC-32C and reported as corruption.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, BtrfsError> {
        let block = bytes
            .get(..SUPERBLOCK_SIZE)
            .ok_or_else(|| truncated(SUPERBLOCK_SIZE, bytes.len()))?;
        let sb = Superblock { bytes: block };

        if sb.magic() != MAGIC {
            return Err(BtrfsError::BadMagic);
        }
        let csum_type = sb.csum_type();
        if !csum_type.is_supported() {
            return Err(BtrfsError::UnsupportedChecksum(sb.csum_type_raw()));
        }
        verify_crc32c(block)?;

        let sector = sb.sectorsize();
        if !is_valid_block_size(sector, MIN_SECTOR_SIZE, MAX_SECTOR_SIZE) {
            return Err(BtrfsError::BadSectorSize(sector));
        }
        // As `btrfs_validate_super`: a node is at least a sector. Every read in
        // this crate assumes a node is whole sectors, and a node smaller than a
        // sector is a unit the allocator never hands out.
        let node = sb.nodesize();
        if !is_valid_block_size(node, MIN_NODE_SIZE, MAX_NODE_SIZE) || node < sector {
            return Err(BtrfsError::BadNodeSize(node));
        }
        // The retired `leafsize` still has to equal `nodesize`; Linux refuses
        // a superblock where it does not. A disagreement means one of the two
        // size fields is damaged, and nothing says which.
        let leaf = sb.leafsize_or_unused();
        if leaf != node {
            return Err(BtrfsError::BadNodeSize(leaf));
        }
        sb.check_roots()?;
        let array_size = sb.sys_chunk_array_size();
        if array_size as usize > SYS_CHUNK_ARRAY_SIZE {
            return Err(BtrfsError::SysChunkArrayTooLarge(array_size));
        }
        Ok(sb)
    }

    /// Refuse a tree root no node could be at.
    ///
    /// `btrfs_validate_super` refuses a root, chunk root or log root level of
    /// eight or more — deeper than any tree — and an address for any of them
    /// that is not on a sector boundary, where no node starts. The walker would
    /// refuse both on the first read; refusing them here keeps a superblock
    /// that could never mount from parsing as if it were one. Reported as
    /// [`BtrfsError::BadTree`] at the address the superblock names.
    fn check_roots(&self) -> Result<(), BtrfsError> {
        let sector = u64::from(self.sectorsize());
        let roots = [
            (self.root(), self.root_level()),
            (self.chunk_root(), self.chunk_root_level()),
            (self.log_root(), self.log_root_level()),
        ];
        for (logical, level) in roots {
            if level > MAX_LEVEL || logical.checked_rem(sector) != Some(0) {
                return Err(BtrfsError::BadTree { logical });
            }
        }
        Ok(())
    }

    /// Parse a superblock and confirm it records the offset it was read from.
    ///
    /// A mirror carries its own offset in `bytenr`, so this is what tells a
    /// mirror at 64 MiB apart from a stale primary that was copied there.
    pub fn parse_at(bytes: &'a [u8], offset: u64) -> Result<Self, BtrfsError> {
        let sb = Self::parse(bytes)?;
        if sb.bytenr() != offset {
            return Err(BtrfsError::WrongAddress {
                expected: offset,
                found: sb.bytenr(),
            });
        }
        Ok(sb)
    }

    /// Read a `u64` field, defaulting to zero. Every offset used below is a
    /// constant inside the 4 KiB block, so the fallback is unreachable and
    /// exists only to keep the accessor total.
    fn field64(&self, at: usize) -> u64 {
        u64_at(self.bytes, at).unwrap_or(0)
    }

    /// Read a `u32` field, defaulting to zero, for the same reason.
    fn field32(&self, at: usize) -> u32 {
        u32_at(self.bytes, at).unwrap_or(0)
    }

    /// The 4 KiB the superblock was parsed from.
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The stored checksum field, all 32 bytes of it.
    #[must_use]
    pub fn csum(&self) -> [u8; 32] {
        array_at::<32>(self.bytes, 0).unwrap_or([0; 32])
    }

    /// The filesystem UUID, shared by every device in the volume.
    #[must_use]
    pub fn fsid(&self) -> [u8; 16] {
        array_at::<16>(self.bytes, 32).unwrap_or([0; 16])
    }

    /// The device offset this copy of the superblock lives at.
    #[must_use]
    pub fn bytenr(&self) -> u64 {
        self.field64(48)
    }

    /// Superblock flags, including the `CHANGING_FSID` markers.
    #[must_use]
    pub fn flags(&self) -> u64 {
        self.field64(56)
    }

    /// The eight magic bytes, which [`Superblock::parse`] has already checked.
    #[must_use]
    pub fn magic(&self) -> [u8; 8] {
        array_at::<8>(self.bytes, 64).unwrap_or([0; 8])
    }

    /// The transaction this superblock was written by. The largest generation
    /// among the valid copies is the current filesystem.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.field64(72)
    }

    /// *Logical* address of the root tree, which holds every subvolume's
    /// `ROOT_ITEM`. Unreachable until the chunk tree is loaded.
    #[must_use]
    pub fn root(&self) -> u64 {
        self.field64(80)
    }

    /// *Logical* address of the chunk tree. Reachable using only the system
    /// chunk array, which is the whole point of that array existing.
    #[must_use]
    pub fn chunk_root(&self) -> u64 {
        self.field64(88)
    }

    /// *Logical* address of the log tree, or zero when the log is empty. A
    /// non-zero value on a read-only mount means the volume was not cleanly
    /// unmounted and some recent writes are only in the log.
    #[must_use]
    pub fn log_root(&self) -> u64 {
        self.field64(96)
    }

    /// Transaction id of the log tree.
    #[must_use]
    pub fn log_root_transid(&self) -> u64 {
        self.field64(104)
    }

    /// Total size of the volume across all devices.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.field64(112)
    }

    /// Bytes currently allocated to chunks.
    #[must_use]
    pub fn bytes_used(&self) -> u64 {
        self.field64(120)
    }

    /// Object id of the root tree's directory, normally
    /// [`crate::items::ROOT_TREE_DIR_OBJECTID`]: where the entry naming the
    /// default subvolume is filed.
    #[must_use]
    pub fn root_dir_objectid(&self) -> u64 {
        self.field64(128)
    }

    /// How many devices the volume spans.
    #[must_use]
    pub fn num_devices(&self) -> u64 {
        self.field64(136)
    }

    /// The unit data is addressed in. Not the device's physical sector size,
    /// and on a volume made on a 64 KiB-page machine not the reader's page size
    /// either.
    #[must_use]
    pub fn sectorsize(&self) -> u32 {
        self.field32(144)
    }

    /// The size of every tree node, and therefore the size of every metadata
    /// read. Since `BIG_METADATA` this is usually 16 KiB rather than 4 KiB.
    #[must_use]
    pub fn nodesize(&self) -> u32 {
        self.field32(148)
    }

    /// Historic `leafsize`, which btrfs required to equal `nodesize` and no
    /// longer uses. Exposed only so a caller can notice a mismatch.
    #[must_use]
    pub fn leafsize_or_unused(&self) -> u32 {
        self.field32(152)
    }

    /// The stripe unit used when allocating chunks.
    #[must_use]
    pub fn stripesize(&self) -> u32 {
        self.field32(156)
    }

    /// Bytes of the 2048-byte system chunk array that are in use.
    #[must_use]
    pub fn sys_chunk_array_size(&self) -> u32 {
        self.field32(160)
    }

    /// Transaction that last wrote the chunk tree.
    #[must_use]
    pub fn chunk_root_generation(&self) -> u64 {
        self.field64(164)
    }

    /// Features an old reader may ignore entirely.
    #[must_use]
    pub fn compat_flags(&self) -> u64 {
        self.field64(172)
    }

    /// Features an old reader may ignore only if it mounts read-only.
    #[must_use]
    pub fn compat_ro_flags(&self) -> u64 {
        self.field64(180)
    }

    /// Features that change the on-disk layout; see [`IncompatFlags`].
    #[must_use]
    pub fn incompat_flags(&self) -> IncompatFlags {
        IncompatFlags::from_bits(self.field64(188))
    }

    /// The raw `csum_type` word.
    #[must_use]
    pub fn csum_type_raw(&self) -> u16 {
        u16_at(self.bytes, 196).unwrap_or(u16::MAX)
    }

    /// The checksum algorithm this volume uses.
    #[must_use]
    pub fn csum_type(&self) -> ChecksumType {
        ChecksumType::from_raw(self.csum_type_raw())
    }

    /// Height of the root tree, so a caller knows whether `root` points at a
    /// leaf or at an internal node.
    #[must_use]
    pub fn root_level(&self) -> u8 {
        u8_at(self.bytes, 198).unwrap_or(0)
    }

    /// Height of the chunk tree.
    #[must_use]
    pub fn chunk_root_level(&self) -> u8 {
        u8_at(self.bytes, 199).unwrap_or(0)
    }

    /// Height of the log tree.
    #[must_use]
    pub fn log_root_level(&self) -> u8 {
        u8_at(self.bytes, 200).unwrap_or(0)
    }

    /// The embedded 98-byte `DEV_ITEM` for the device this copy came from.
    /// Parse it with [`crate::items::DevItem::parse`].
    #[must_use]
    pub fn dev_item(&self) -> &'a [u8] {
        slice_at(self.bytes, DEV_ITEM_OFFSET, 98).unwrap_or(&[])
    }

    /// The volume label as text, or `None` if it is not valid UTF-8.
    ///
    /// The field is a fixed 256 bytes NUL-padded, so the label ends at the
    /// first NUL; an unterminated field is taken whole rather than refused.
    #[must_use]
    pub fn label(&self) -> Option<&'a str> {
        let raw = slice_at(self.bytes, LABEL_OFFSET, LABEL_SIZE)?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        core::str::from_utf8(raw.get(..end)?).ok()
    }

    /// Generation of the free-space cache.
    #[must_use]
    pub fn cache_generation(&self) -> u64 {
        self.field64(555)
    }

    /// Generation of the UUID tree.
    #[must_use]
    pub fn uuid_tree_generation(&self) -> u64 {
        self.field64(563)
    }

    /// The metadata UUID, which stays fixed while `fsid` is being changed.
    #[must_use]
    pub fn metadata_uuid(&self) -> [u8; 16] {
        array_at::<16>(self.bytes, 571).unwrap_or([0; 16])
    }

    /// The in-use portion of the system chunk array, as raw bytes.
    ///
    /// This is the bootstrap map: enough `(key, CHUNK_ITEM)` pairs to translate
    /// the logical addresses of the chunk tree itself, and nothing more.
    #[must_use]
    pub fn sys_chunk_array_bytes(&self) -> &'a [u8] {
        let len = (self.sys_chunk_array_size() as usize).min(SYS_CHUNK_ARRAY_SIZE);
        slice_at(self.bytes, SYS_CHUNK_ARRAY_OFFSET, len).unwrap_or(&[])
    }

    /// Iterate the `(key, chunk)` pairs of the system chunk array.
    #[must_use]
    pub fn sys_chunk_array(&self) -> SysChunkArray<'a> {
        SysChunkArray::new(self.sys_chunk_array_bytes())
    }
}
