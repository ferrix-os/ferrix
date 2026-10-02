//! Logical-to-physical address translation: btrfs's defining feature.
//!
//! btrfs does not address devices. It addresses a single flat *logical* space
//! that the chunk tree carves into chunks, each of which is placed on one or
//! more devices as a set of stripes. Every address stored anywhere else in the
//! filesystem — root pointers, `blockptr`s, `disk_bytenr`s — is logical, and
//! reading any of them means translating first.
//!
//! # The bootstrap problem
//!
//! The chunk tree is itself at a logical address, so translating it needs the
//! chunk tree. btrfs breaks the circle by copying the handful of chunk items
//! that cover the chunk tree into the superblock, as the *system chunk array*.
//! The sequence is: parse the superblock, load its system chunk array into a
//! [`ChunkMap`], translate and read the chunk tree, then feed every
//! `CHUNK_ITEM` in it into the same map. From that point the map covers the
//! whole volume.
//!
//! # What is deliberately not implemented
//!
//! For SINGLE, DUP, RAID1, RAID1C3 and RAID1C4 the mapping is linear: every
//! stripe is a complete copy of the chunk, so reading from stripe 0 is correct
//! and the physical address is just `stripe.offset + (logical - chunk_start)`.
//! The other stripes are the same bytes again, for a read whose first copy
//! fails its checksum to fall back on ([`ChunkMap::map_copy`]).
//!
//! RAID0, RAID10, RAID5 and RAID6 interleave the logical range across stripes,
//! and RAID5/6 add parity rotation on top. Those are reported as
//! [`BtrfsError::UnsupportedProfile`] rather than approximated, because an
//! approximate answer here is a successful read of the wrong bytes — usually
//! one that passes its own checksum, since the block it lands on is a real
//! block belonging to something else.

use crate::items::CHUNK_ITEM_KEY;
use crate::superblock::{MAX_SECTOR_SIZE, MIN_SECTOR_SIZE};
use crate::tree::{BtrfsKey, KEY_SIZE};
use crate::{BtrfsError, array_at, is_valid_block_size, u16_at, u32_at, u64_at};

/// Chunk holds file data.
pub const BLOCK_GROUP_DATA: u64 = 1 << 0;
/// Chunk holds filesystem metadata that must be readable before the chunk tree
/// is, which is why chunks of this type appear in the system chunk array.
pub const BLOCK_GROUP_SYSTEM: u64 = 1 << 1;
/// Chunk holds tree nodes.
pub const BLOCK_GROUP_METADATA: u64 = 1 << 2;
/// Striped across stripes with no redundancy.
pub const BLOCK_GROUP_RAID0: u64 = 1 << 3;
/// Two-way mirror across two devices.
pub const BLOCK_GROUP_RAID1: u64 = 1 << 4;
/// Two copies on the *same* device, which is what a single-device filesystem
/// uses for metadata.
pub const BLOCK_GROUP_DUP: u64 = 1 << 5;
/// Striped across mirrored pairs.
pub const BLOCK_GROUP_RAID10: u64 = 1 << 6;
/// Striped with one parity stripe.
pub const BLOCK_GROUP_RAID5: u64 = 1 << 7;
/// Striped with two parity stripes.
pub const BLOCK_GROUP_RAID6: u64 = 1 << 8;
/// Three-way mirror.
pub const BLOCK_GROUP_RAID1C3: u64 = 1 << 9;
/// Four-way mirror.
pub const BLOCK_GROUP_RAID1C4: u64 = 1 << 10;

/// Every profile bit, so the profile can be separated from the DATA / SYSTEM /
/// METADATA bits that share the same word.
pub const BLOCK_GROUP_PROFILE_MASK: u64 = BLOCK_GROUP_RAID0
    | BLOCK_GROUP_RAID1
    | BLOCK_GROUP_DUP
    | BLOCK_GROUP_RAID10
    | BLOCK_GROUP_RAID5
    | BLOCK_GROUP_RAID6
    | BLOCK_GROUP_RAID1C3
    | BLOCK_GROUP_RAID1C4;

/// Bytes in a `CHUNK_ITEM` before its stripe array.
pub const CHUNK_HEADER_SIZE: usize = 48;

/// Bytes in one stripe record: a device id, a device offset and a device UUID.
pub const STRIPE_SIZE: usize = 32;

/// What a chunk holds, as opposed to how it is replicated.
pub const BLOCK_GROUP_TYPE_MASK: u64 = BLOCK_GROUP_DATA | BLOCK_GROUP_SYSTEM | BLOCK_GROUP_METADATA;

/// The only `stripe_len` btrfs writes: Linux's `BTRFS_STRIPE_LEN`, 64 KiB.
pub const STRIPE_LEN: u64 = 64 * 1024;

/// The longest chunk Linux accepts: one whose stripe count still fits the
/// `u32` its striping arithmetic keeps it in.
pub const MAX_CHUNK_LENGTH: u64 = (u32::MAX as u64) * STRIPE_LEN;

/// Object id every chunk item's key carries. Not a real object; btrfs reuses
/// the "first free" number as a constant marker so chunk items sort together.
pub const FIRST_CHUNK_TREE_OBJECTID: u64 = 256;

/// A chunk's replication profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkProfile {
    /// No profile bit set: one stripe, no redundancy.
    Single,
    /// Two copies on one device.
    Dup,
    /// Two-way mirror.
    Raid1,
    /// Three-way mirror.
    Raid1c3,
    /// Four-way mirror.
    Raid1c4,
    /// Striped, no redundancy.
    Raid0,
    /// Striped mirrors.
    Raid10,
    /// Single parity.
    Raid5,
    /// Double parity.
    Raid6,
    /// More than one profile bit set, or a bit from a future format. Carries
    /// the profile bits found.
    Unknown(u64),
}

impl ChunkProfile {
    /// Classify the profile bits of a chunk's `type_` word.
    ///
    /// More than one profile bit set is not a profile btrfs can produce, so it
    /// is reported as [`ChunkProfile::Unknown`] rather than resolved by
    /// precedence — silently picking one of two contradictory layouts is how a
    /// corrupt chunk item turns into a wrong read.
    #[must_use]
    pub const fn from_type(type_: u64) -> Self {
        let profile = type_ & BLOCK_GROUP_PROFILE_MASK;
        match profile {
            0 => ChunkProfile::Single,
            BLOCK_GROUP_DUP => ChunkProfile::Dup,
            BLOCK_GROUP_RAID1 => ChunkProfile::Raid1,
            BLOCK_GROUP_RAID1C3 => ChunkProfile::Raid1c3,
            BLOCK_GROUP_RAID1C4 => ChunkProfile::Raid1c4,
            BLOCK_GROUP_RAID0 => ChunkProfile::Raid0,
            BLOCK_GROUP_RAID10 => ChunkProfile::Raid10,
            BLOCK_GROUP_RAID5 => ChunkProfile::Raid5,
            BLOCK_GROUP_RAID6 => ChunkProfile::Raid6,
            other => ChunkProfile::Unknown(other),
        }
    }

    /// Whether every stripe of this profile is a whole copy of the chunk, which
    /// is exactly the condition under which stripe 0 can be read directly.
    #[must_use]
    pub const fn is_mirrored(self) -> bool {
        matches!(
            self,
            ChunkProfile::Single
                | ChunkProfile::Dup
                | ChunkProfile::Raid1
                | ChunkProfile::Raid1c3
                | ChunkProfile::Raid1c4
        )
    }

    /// Whether this crate will translate addresses in a chunk of this profile.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        self.is_mirrored()
    }
}

/// One stripe: where a copy of the chunk lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stripe {
    /// Which device holds this copy.
    pub devid: u64,
    /// Byte offset of the copy within that device. This *is* a physical
    /// address, and the only one in the format.
    pub offset: u64,
    /// UUID of the device, so `devid` can be confirmed against the device
    /// actually present rather than trusted.
    pub dev_uuid: [u8; 16],
}

/// A `CHUNK_ITEM` borrowing the bytes it was parsed from.
///
/// The item is variable length: a 48-byte header followed by `num_stripes`
/// 32-byte stripe records.
#[derive(Debug, Clone, Copy)]
pub struct ChunkItem<'a> {
    bytes: &'a [u8],
}

impl<'a> ChunkItem<'a> {
    /// Parse a chunk item from the front of `bytes`.
    ///
    /// Rejects a chunk with no stripes, a zero length or stripe length, or a
    /// `num_stripes` that its own bytes cannot back — each of which would
    /// otherwise turn into a division by zero or a read of adjacent memory
    /// interpreted as a device offset.
    ///
    /// Beyond that it applies every part of Linux's `btrfs_check_chunk_valid`
    /// that the item can answer on its own: see `check_type` and
    /// `check_geometry` below. The parts that need the volume's
    /// `sectorsize` are [`ChunkItem::check_sectorsize`], which a mount calls.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, BtrfsError> {
        let item = ChunkItem { bytes };
        let num_stripes = u16_at(bytes, 44).ok_or(BtrfsError::BadChunk)?;
        if num_stripes == 0 {
            return Err(BtrfsError::BadChunk);
        }
        let total = CHUNK_HEADER_SIZE
            .checked_add(
                usize::from(num_stripes)
                    .checked_mul(STRIPE_SIZE)
                    .ok_or(BtrfsError::BadChunk)?,
            )
            .ok_or(BtrfsError::BadChunk)?;
        if bytes.len() < total {
            return Err(BtrfsError::BadChunk);
        }
        if item.length() == 0 || item.stripe_len() == 0 {
            return Err(BtrfsError::BadChunk);
        }
        item.check_type()?;
        item.check_geometry()?;
        Ok(item)
    }

    /// Refuse type bits btrfs does not write.
    ///
    /// As `btrfs_check_chunk_valid` does: no bit outside the type and profile
    /// masks, at most one profile bit, at least one type bit, and a SYSTEM
    /// chunk holding nothing else. An unknown bit may be a feature that changes
    /// how the chunk is laid out, and two profile bits describe two layouts at
    /// once; reading through either would be a guess.
    fn check_type(&self) -> Result<(), BtrfsError> {
        let bits = self.type_bits();
        let kinds = bits & BLOCK_GROUP_TYPE_MASK;
        let usable = bits & !(BLOCK_GROUP_TYPE_MASK | BLOCK_GROUP_PROFILE_MASK) == 0
            && (bits & BLOCK_GROUP_PROFILE_MASK).count_ones() <= 1
            && kinds != 0
            && (kinds & BLOCK_GROUP_SYSTEM == 0 || kinds == BLOCK_GROUP_SYSTEM);
        if usable {
            Ok(())
        } else {
            Err(BtrfsError::BadChunk)
        }
    }

    /// Refuse a stripe layout the profile could not have produced.
    ///
    /// The stripe counts are Linux's `valid_stripe_count` and the
    /// `ncopies`/`nparity` checks before it: SINGLE is one stripe, DUP and
    /// RAID1 two, RAID1C3 and RAID1C4 three and four, RAID10 pairs of
    /// mirrors, RAID5 and RAID6 at least one and two stripes beyond their
    /// parity. The rest is `stripe_len` being the fixed 64 KiB, a length that
    /// the striping arithmetic can hold, and a length in whole sectors. This
    /// matters even for the profiles read through stripe 0: a SINGLE chunk
    /// claiming three stripes, or a DUP chunk one, is not the chunk mkfs wrote,
    /// and its stripe 0 is not to be trusted either.
    fn check_geometry(&self) -> Result<(), BtrfsError> {
        let stripes = self.num_stripes();
        let count_fits = match self.profile() {
            ChunkProfile::Single => stripes == 1,
            ChunkProfile::Dup | ChunkProfile::Raid1 => stripes == 2,
            ChunkProfile::Raid1c3 => stripes == 3,
            ChunkProfile::Raid1c4 => stripes == 4,
            ChunkProfile::Raid0 => stripes >= 1,
            ChunkProfile::Raid10 => stripes >= 2 && self.sub_stripes() == 2,
            ChunkProfile::Raid5 => stripes >= 2,
            ChunkProfile::Raid6 => stripes >= 3,
            ChunkProfile::Unknown(_) => false,
        };
        // The item's own sector size, which a mount then requires to be the
        // volume's; in whole sectors of it, the length is in whole sectors.
        let sector = self.sector_size();
        let whole_sectors = is_valid_block_size(sector, MIN_SECTOR_SIZE, MAX_SECTOR_SIZE)
            && self.length().checked_rem(u64::from(sector)) == Some(0);
        if count_fits
            && whole_sectors
            && self.stripe_len() == STRIPE_LEN
            && self.length() < MAX_CHUNK_LENGTH
        {
            Ok(())
        } else {
            Err(BtrfsError::BadChunk)
        }
    }

    /// Check the chunk against the volume it was found in.
    ///
    /// The rest of `btrfs_check_chunk_valid`, which needs the superblock's
    /// `sectorsize`: the chunk records the same sector size, and starts on a
    /// sector boundary. [`ChunkItem::parse`] cannot know either, so a mount
    /// calls this for every chunk before inserting it. A chunk off the sector
    /// grid maps every block in it to a physical offset off the grid too —
    /// into the middle of whatever block really lives there.
    pub fn check_sectorsize(&self, logical: u64, sectorsize: u32) -> Result<(), BtrfsError> {
        let aligned = logical.checked_rem(u64::from(sectorsize)) == Some(0);
        if aligned && self.sector_size() == sectorsize {
            Ok(())
        } else {
            Err(BtrfsError::BadChunk)
        }
    }

    /// Bytes of the logical address space this chunk covers.
    #[must_use]
    pub fn length(&self) -> u64 {
        u64_at(self.bytes, 0).unwrap_or(0)
    }

    /// The tree that owns the chunk, always the extent tree in practice.
    #[must_use]
    pub fn owner(&self) -> u64 {
        u64_at(self.bytes, 8).unwrap_or(0)
    }

    /// Bytes written to one stripe before moving to the next, for the striped
    /// profiles.
    #[must_use]
    pub fn stripe_len(&self) -> u64 {
        u64_at(self.bytes, 16).unwrap_or(0)
    }

    /// The DATA / SYSTEM / METADATA bits and the profile bits, in one word.
    #[must_use]
    pub fn type_bits(&self) -> u64 {
        u64_at(self.bytes, 24).unwrap_or(0)
    }

    /// The replication profile.
    #[must_use]
    pub fn profile(&self) -> ChunkProfile {
        ChunkProfile::from_type(self.type_bits())
    }

    /// Preferred I/O alignment.
    #[must_use]
    pub fn io_align(&self) -> u32 {
        u32_at(self.bytes, 32).unwrap_or(0)
    }

    /// Preferred I/O width.
    #[must_use]
    pub fn io_width(&self) -> u32 {
        u32_at(self.bytes, 36).unwrap_or(0)
    }

    /// Minimum I/O size for the devices under this chunk.
    #[must_use]
    pub fn sector_size(&self) -> u32 {
        u32_at(self.bytes, 40).unwrap_or(0)
    }

    /// How many stripe records follow the header.
    #[must_use]
    pub fn num_stripes(&self) -> u16 {
        u16_at(self.bytes, 44).unwrap_or(0)
    }

    /// For RAID10, how many stripes form each mirrored group.
    #[must_use]
    pub fn sub_stripes(&self) -> u16 {
        u16_at(self.bytes, 46).unwrap_or(0)
    }

    /// Total on-disk size of this item, header plus stripes. A caller walking
    /// the system chunk array needs it to find the next entry.
    #[must_use]
    pub fn total_size(&self) -> usize {
        CHUNK_HEADER_SIZE
            .saturating_add(usize::from(self.num_stripes()).saturating_mul(STRIPE_SIZE))
    }

    /// The stripe at `index`, or `None` past `num_stripes`.
    #[must_use]
    pub fn stripe(&self, index: u16) -> Option<Stripe> {
        if index >= self.num_stripes() {
            return None;
        }
        let at = CHUNK_HEADER_SIZE.checked_add(usize::from(index).checked_mul(STRIPE_SIZE)?)?;
        Some(Stripe {
            devid: u64_at(self.bytes, at)?,
            offset: u64_at(self.bytes, at.checked_add(8)?)?,
            dev_uuid: array_at::<16>(self.bytes, at.checked_add(16)?)?,
        })
    }

    /// Iterate the stripes.
    #[must_use]
    pub const fn stripes(&self) -> StripeIter<'a> {
        StripeIter {
            item: ChunkItem { bytes: self.bytes },
            index: 0,
        }
    }
}

/// Iterator over a chunk's stripes, produced by [`ChunkItem::stripes`].
#[derive(Debug, Clone, Copy)]
pub struct StripeIter<'a> {
    item: ChunkItem<'a>,
    index: u16,
}

impl Iterator for StripeIter<'_> {
    type Item = Stripe;

    fn next(&mut self) -> Option<Stripe> {
        let stripe = self.item.stripe(self.index)?;
        self.index = self.index.checked_add(1)?;
        Some(stripe)
    }
}

// ---------------------------------------------------------------------------
// The system chunk array
// ---------------------------------------------------------------------------

/// Iterator over the `(key, CHUNK_ITEM)` pairs packed into the superblock's
/// system chunk array.
///
/// The array has no count and no terminator: entries are packed back to back
/// and the array ends where `sys_chunk_array_size` says it does. Each entry's
/// length depends on the `num_stripes` inside it, so the only way to find entry
/// *n* is to have parsed entries `0..n`. The iterator therefore stops at the
/// first malformed entry, after reporting it, rather than trying to resynchronise.
#[derive(Debug, Clone, Copy)]
pub struct SysChunkArray<'a> {
    bytes: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> SysChunkArray<'a> {
    /// Iterate the entries in `bytes`, which must already be trimmed to
    /// `sys_chunk_array_size`.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        SysChunkArray {
            bytes,
            at: 0,
            done: false,
        }
    }

    /// Parse the entry at the cursor without advancing it.
    ///
    /// Linux's `btrfs_read_sys_array` refuses an entry whose key is not a
    /// `CHUNK_ITEM` and a chunk without the SYSTEM bit, and so does this. The
    /// array has nothing else in it by construction: a key of another type
    /// means the cursor is not where an entry starts, and a non-system chunk
    /// here claims a place in the bootstrap map that the chunk tree, not the
    /// superblock, is meant to fill.
    fn entry(&self) -> Result<(BtrfsKey, ChunkItem<'a>, usize), BtrfsError> {
        let key = BtrfsKey::parse(self.bytes, self.at).ok_or(BtrfsError::BadChunk)?;
        if key.item_type != CHUNK_ITEM_KEY {
            return Err(BtrfsError::BadChunk);
        }
        let body_at = self.at.checked_add(KEY_SIZE).ok_or(BtrfsError::BadChunk)?;
        let body = self.bytes.get(body_at..).ok_or(BtrfsError::BadChunk)?;
        let item = ChunkItem::parse(body)?;
        if item.type_bits() & BLOCK_GROUP_SYSTEM == 0 {
            return Err(BtrfsError::BadChunk);
        }
        let next = body_at
            .checked_add(item.total_size())
            .ok_or(BtrfsError::BadChunk)?;
        Ok((key, item, next))
    }
}

impl<'a> Iterator for SysChunkArray<'a> {
    type Item = Result<(BtrfsKey, ChunkItem<'a>), BtrfsError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.at >= self.bytes.len() {
            return None;
        }
        // NOALLOC: the iterator's own `entry` parser; this crate has no `alloc`.
        match self.entry() {
            Ok((key, item, next)) => {
                self.at = next;
                Some(Ok((key, item)))
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The chunk map
// ---------------------------------------------------------------------------

/// The most whole copies a profile keeps: RAID1C4's four.
pub const MAX_COPIES: usize = 4;

/// One chunk reduced to what reading needs: a logical range and where each
/// copy of it lives.
///
/// Every profile this crate translates mirrors the whole chunk into every
/// stripe, so stripe 0 is where a read goes and the others are alternatives
/// to fall back on after a checksum failure, as Linux retries the next
/// mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkMapEntry {
    /// First logical address the chunk covers.
    pub logical: u64,
    /// How much of the logical space it covers.
    pub length: u64,
    /// The chunk's type and profile bits, kept so an unsupported profile is
    /// reported at lookup rather than silently dropped at insert.
    pub type_bits: u64,
    /// Device holding stripe 0.
    pub devid: u64,
    /// Physical offset of stripe 0 on that device.
    pub physical: u64,
    /// How many whole copies there are to read: the stripe count of a
    /// mirrored profile, at most [`MAX_COPIES`]; one otherwise.
    pub copies: u8,
    /// Device and physical offset of stripes 1 up to `copies`; stripe 0 is
    /// `devid` and `physical`.
    pub mirrors: [(u64, u64); MAX_COPIES - 1],
}

impl ChunkMapEntry {
    /// A zeroed entry, for initialising a caller's storage array.
    pub const EMPTY: ChunkMapEntry = ChunkMapEntry {
        logical: 0,
        length: 0,
        type_bits: 0,
        devid: 0,
        physical: 0,
        copies: 0,
        mirrors: [(0, 0); MAX_COPIES - 1],
    };
}

impl Default for ChunkMapEntry {
    fn default() -> Self {
        ChunkMapEntry::EMPTY
    }
}

/// Storage for a [`ChunkMap`]: anything that is a slice of entries.
///
/// A borrowed array before the heap exists, and a boxed slice once a mount has
/// to own its map — a mount cannot borrow from itself, and `unsafe` is not
/// available here to pretend otherwise.
pub trait ChunkStorage: AsRef<[ChunkMapEntry]> + AsMut<[ChunkMapEntry]> {}

impl<T: AsRef<[ChunkMapEntry]> + AsMut<[ChunkMapEntry]> + ?Sized> ChunkStorage for T {}

/// The accumulated logical-to-physical map.
///
/// Never allocates, because it has to work before any allocator does — the
/// first thing it maps is the chunk tree, and the heap on this system may be
/// set up from a filesystem this crate is reading. A caller supplies storage:
///
/// ```
/// # use ferrix_btrfs::chunk::{ChunkMap, ChunkMapEntry};
/// let mut storage = [ChunkMapEntry::EMPTY; 64];
/// let map = ChunkMap::new(&mut storage);
/// assert!(map.is_empty(), "a fresh map holds no chunks");
/// ```
///
/// Entries are kept sorted by logical address so lookup is a binary search.
/// Inserting a chunk identical to one already present changes nothing, which
/// is what makes it safe to load the system chunk array and then the chunk
/// tree, whose contents repeat; a chunk that overlaps a different one is
/// refused.
#[derive(Debug)]
pub struct ChunkMap<S> {
    entries: S,
    len: usize,
}

impl<S: ChunkStorage> ChunkMap<S> {
    /// Create an empty map over caller-supplied storage.
    #[must_use]
    pub fn new(storage: S) -> Self {
        ChunkMap {
            entries: storage,
            len: 0,
        }
    }

    /// How many chunks the map holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the map holds no chunks.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many chunks the storage can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.entries.as_ref().len()
    }

    /// The chunks, in ascending logical order.
    #[must_use]
    pub fn entries(&self) -> &[ChunkMapEntry] {
        self.entries.as_ref().get(..self.len).unwrap_or(&[])
    }

    /// Add a chunk covering logical address `logical`.
    ///
    /// `logical` comes from the *key* of the chunk item, not from the item
    /// body: a `CHUNK_ITEM` records its length and its stripes but not where it
    /// starts, because the key already says.
    pub fn insert(&mut self, logical: u64, item: &ChunkItem<'_>) -> Result<(), BtrfsError> {
        let stripe = item.stripe(0).ok_or(BtrfsError::BadChunk)?;
        // A chunk whose logical range wraps would make every containment test
        // below meaningless, so it is refused rather than clamped.
        if logical.checked_add(item.length()).is_none() {
            return Err(BtrfsError::BadChunk);
        }
        let mut entry = ChunkMapEntry {
            logical,
            length: item.length(),
            type_bits: item.type_bits(),
            devid: stripe.devid,
            physical: stripe.offset,
            copies: 1,
            mirrors: [(0, 0); MAX_COPIES - 1],
        };
        // A SINGLE chunk has one copy whatever its stripe count claims.
        let profile = ChunkProfile::from_type(item.type_bits());
        if profile.is_mirrored() && profile != ChunkProfile::Single {
            for (slot, mirror) in entry.mirrors.iter_mut().zip(item.stripes().skip(1)) {
                *slot = (mirror.devid, mirror.offset);
                entry.copies = entry.copies.saturating_add(1);
            }
        }
        match self.position(logical) {
            // The system chunk array is a copy of chunk tree items, so the
            // same chunk legitimately arrives twice. The same *start* with a
            // different length or placement is two descriptions of one range,
            // and neither can be believed over the other.
            Ok(at) if self.entries().get(at) == Some(&entry) => Ok(()),
            Ok(_) => Err(BtrfsError::BadChunk),
            Err(at) => {
                self.check_neighbours(at, &entry)?;
                self.insert_at(at, entry)
            }
        }
    }

    /// Refuse `entry` if it overlaps either chunk it would be inserted
    /// between.
    ///
    /// Logical ranges are disjoint by construction in btrfs: a chunk is
    /// allocated from free logical space. One that overlaps another would let
    /// [`ChunkMap::lookup`] answer with whichever sorts nearer, so part of the
    /// earlier chunk would silently map through the later one's stripe — or,
    /// past the later one's end, not map at all. The entries are kept sorted
    /// and disjoint, so the neighbours on either side are the only candidates.
    fn check_neighbours(&self, at: usize, entry: &ChunkMapEntry) -> Result<(), BtrfsError> {
        let end = entry
            .logical
            .checked_add(entry.length)
            .ok_or(BtrfsError::BadChunk)?;
        let before = at.checked_sub(1).and_then(|i| self.entries().get(i));
        if before.is_some_and(|prev| prev.logical.saturating_add(prev.length) > entry.logical) {
            return Err(BtrfsError::BadChunk);
        }
        if self
            .entries()
            .get(at)
            .is_some_and(|next| next.logical < end)
        {
            return Err(BtrfsError::BadChunk);
        }
        Ok(())
    }

    /// Shift the tail up by one and drop `entry` into the hole.
    fn insert_at(&mut self, at: usize, entry: ChunkMapEntry) -> Result<(), BtrfsError> {
        let end = self.len.checked_add(1).ok_or(BtrfsError::ChunkMapFull)?;
        if end > self.capacity() || at > self.len {
            return Err(BtrfsError::ChunkMapFull);
        }
        let tail = self
            .entries
            .as_mut()
            .get_mut(..end)
            .ok_or(BtrfsError::ChunkMapFull)?;
        tail.copy_within(at..self.len, at.saturating_add(1));
        let slot = tail.get_mut(at).ok_or(BtrfsError::ChunkMapFull)?;
        *slot = entry;
        self.len = end;
        Ok(())
    }

    /// Binary search for the entry starting exactly at `logical`, or the index
    /// one would be inserted at.
    fn position(&self, logical: u64) -> Result<usize, usize> {
        self.entries()
            .binary_search_by(|entry| entry.logical.cmp(&logical))
    }

    /// The chunk covering `logical`, if any.
    #[must_use]
    pub fn lookup(&self, logical: u64) -> Option<&ChunkMapEntry> {
        let index = match self.position(logical) {
            Ok(index) => index,
            Err(0) => return None,
            Err(index) => index.checked_sub(1)?,
        };
        let entry = self.entries().get(index)?;
        // Chunks need not be contiguous: a balance can leave a gap in the
        // logical space, and an address in a gap belongs to no chunk at all.
        let end = entry.logical.checked_add(entry.length)?;
        (logical < end).then_some(entry)
    }

    /// Translate `logical`, reporting why if it cannot be done.
    ///
    /// Returns the device id and the byte offset within that device.
    pub fn map(&self, logical: u64) -> Result<(u64, u64), BtrfsError> {
        self.map_copy(logical, 0)
    }

    /// How many copies of `logical` a read can choose from: Linux's
    /// `btrfs_num_copies`. Zero for an address no chunk holds.
    #[must_use]
    pub fn copies(&self, logical: u64) -> usize {
        self.lookup(logical)
            .map_or(0, |entry| usize::from(entry.copies.max(1)))
    }

    /// Translate `logical` to where copy `copy` of it lives, counting from
    /// stripe 0 as [`ChunkMap::map`] does. A copy past
    /// [`ChunkMap::copies`] is not mapped.
    pub fn map_copy(&self, logical: u64, copy: usize) -> Result<(u64, u64), BtrfsError> {
        let entry = self.lookup(logical).ok_or(BtrfsError::NotMapped(logical))?;
        let profile = ChunkProfile::from_type(entry.type_bits);
        if !profile.is_supported() {
            return Err(BtrfsError::UnsupportedProfile(
                entry.type_bits & BLOCK_GROUP_PROFILE_MASK,
            ));
        }
        // Every stripe of a mirrored profile is a whole copy, so the offset
        // within the chunk is the offset within the stripe.
        let within = logical
            .checked_sub(entry.logical)
            .ok_or(BtrfsError::NotMapped(logical))?;
        let (devid, start) = match copy.checked_sub(1) {
            None => (entry.devid, entry.physical),
            Some(_) if copy >= usize::from(entry.copies.max(1)) => {
                return Err(BtrfsError::NotMapped(logical));
            }
            Some(mirror) => entry
                .mirrors
                .get(mirror)
                .copied()
                .ok_or(BtrfsError::NotMapped(logical))?,
        };
        let physical = start
            .checked_add(within)
            .ok_or(BtrfsError::NotMapped(logical))?;
        Ok((devid, physical))
    }

    /// Translate `logical` to a device id and a physical byte offset.
    ///
    /// `None` covers both "no chunk holds this address" and "the chunk holding
    /// it uses a profile this crate will not translate"; [`ChunkMap::map`]
    /// distinguishes them.
    #[must_use]
    pub fn logical_to_physical(&self, logical: u64) -> Option<(u64, u64)> {
        self.map(logical).ok()
    }

    /// How many contiguous bytes from `logical` stay inside one chunk.
    ///
    /// A read longer than this must be split, because the next byte is on a
    /// different device or nowhere at all.
    #[must_use]
    pub fn contiguous_len(&self, logical: u64) -> Option<u64> {
        let entry = self.lookup(logical)?;
        let end = entry.logical.checked_add(entry.length)?;
        end.checked_sub(logical)
    }

    /// The profile of the chunk covering `logical`.
    #[must_use]
    pub fn profile_at(&self, logical: u64) -> Option<ChunkProfile> {
        self.lookup(logical)
            .map(|entry| ChunkProfile::from_type(entry.type_bits))
    }

    /// Load every chunk in a superblock's system chunk array.
    ///
    /// Returns how many chunks were added. Stops at the first malformed entry
    /// and reports it: the array is packed, so a bad entry means every byte
    /// after it is at an unknown offset.
    pub fn load_sys_chunk_array(&mut self, array: &[u8]) -> Result<usize, BtrfsError> {
        let mut count = 0usize;
        for entry in SysChunkArray::new(array) {
            let (key, item) = entry?;
            // NOALLOC: `ChunkMap::insert` fills caller-supplied storage; this crate has no `alloc`.
            self.insert(key.offset, &item)?;
            count = count.saturating_add(1);
        }
        Ok(count)
    }
}
