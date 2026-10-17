//! Finding the control partition and the two slots in a GUID partition
//! table (UEFI 2.10 §5.3), by their Ferrix type GUIDs.
//!
//! Only the primary table is read, and nothing in it is trusted: the
//! header's signature, size, CRC-32 and own address, the entry array's
//! CRC-32, and each partition's range against the disk are all checked.
//! `ferrix-partition` does the same for the kernel and the installer with
//! an allocator; the loader has none, so this walks the array in place.

use crate::crc::crc32;
use crate::record::MIN_CONTROL_BYTES;
use crate::slot::HEADER_LEN;
use crate::{Slot, le32, le64};

/// A GUID in the table's byte order (the first three fields little-endian).
pub type Guid = [u8; 16];

/// The GUID written `a-b-c-d-e`, in the table's byte order.
#[must_use]
pub const fn guid(a: u32, b: u16, c: u16, d: u16, e: u64) -> Guid {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    let d = d.to_be_bytes();
    let e = e.to_be_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], e[2], e[3], e[4], e[5], e[6],
        e[7],
    ]
}

/// The partition type of a Ferrix system slot,
/// `5475b681-82f5-47da-a5a1-55db0e7e2bd3`.
pub const SLOT_TYPE: Guid = guid(0x5475_b681, 0x82f5, 0x47da, 0xa5a1, 0x55db_0e7e_2bd3);

/// The partition type of the Ferrix boot control record,
/// `c29aa8c5-c19c-4147-838f-01accca28be4`.
pub const CONTROL_TYPE: Guid = guid(0xc29a_a8c5, 0xc19c, 0x4147, 0x838f, 0x01ac_cca2_8be4);

/// The partition type of the Ferrix event log (`docs/POSTMORTEM.md` §4),
/// `0a0323e0-d79d-498d-a4d6-dbea6af3bab0`.
pub const EVENTS_TYPE: Guid = guid(0x0a03_23e0, 0xd79d, 0x498d, 0xa4d6, 0xdbea_6af3_bab0);

/// The header's signature.
const SIGNATURE: &[u8; 8] = b"EFI PART";
/// The most entry-array bytes accepted: 128 entries of 128 bytes is what
/// every tool writes, and four times that is generous.
pub const MAX_ARRAY_BYTES: u64 = 65536;
/// The most entries accepted.
pub const MAX_ENTRIES: u32 = 512;
/// The largest entry accepted; the specification's sizes are 128 times a
/// power of two.
pub const MAX_ENTRY_SIZE: u32 = 1024;

/// What the primary header says about the entry array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The block the array starts at.
    pub entries_lba: u64,
    /// Entries in the array.
    pub entries: u32,
    /// Bytes per entry.
    pub entry_size: u32,
    /// The array's CRC-32.
    pub entries_crc: u32,
    /// The first block a partition may use.
    pub first_usable: u64,
    /// The last block a partition may use.
    pub last_usable: u64,
    /// The disk's GUID.
    pub disk: Guid,
}

impl Header {
    /// Bytes the array takes.
    #[must_use]
    pub const fn array_bytes(&self) -> u64 {
        self.entries as u64 * self.entry_size as u64
    }
}

/// One partition the loader cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    /// Its index in the entry array, from zero.
    pub index: u32,
    /// Its unique GUID.
    pub unique: Guid,
    /// Its first block.
    pub first_lba: u64,
    /// Its last block, inclusive.
    pub last_lba: u64,
}

impl Part {
    /// Its length in bytes, with blocks of `block_size`.
    #[must_use]
    pub const fn bytes(&self, block_size: u32) -> u64 {
        (self.last_lba - self.first_lba + 1).saturating_mul(block_size as u64)
    }
}

/// The partitions A/B boot uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The control partition.
    pub control: Part,
    /// Slot A, the slot partition with the lower entry index.
    pub a: Part,
    /// Slot B.
    pub b: Part,
    /// The disk's GUID, which the loader names on the command line.
    pub disk: Guid,
}

impl Layout {
    /// The partition of `slot`.
    #[must_use]
    pub const fn slot(&self, slot: Slot) -> Part {
        match slot {
            Slot::A => self.a,
            Slot::B => self.b,
        }
    }
}

/// Why a disk has no A/B layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GptError {
    /// The header is not a GPT header, or is damaged.
    Header,
    /// The entry array's CRC-32 does not match, or it is too large.
    Entries,
    /// A partition's range is backwards or past the disk.
    Range,
    /// A control or slot partition overlaps another entry.
    Overlap,
    /// No control partition: the disk is not set up for slots.
    NoControl,
    /// More than one control partition.
    ManyControls,
    /// Not exactly two slot partitions.
    SlotCount,
    /// The control partition or a slot is too small.
    TooSmall,
}

/// Read the primary header from block 1's bytes, on a disk of `blocks`
/// blocks of `block_size` bytes.
pub fn parse_header(sector: &[u8], blocks: u64, block_size: u32) -> Result<Header, GptError> {
    if sector.get(..8) != Some(&SIGNATURE[..]) {
        return Err(GptError::Header);
    }
    let size = le32(sector, 12).ok_or(GptError::Header)? as usize;
    if !(92..=512).contains(&size) || size > sector.len() || size > block_size as usize {
        return Err(GptError::Header);
    }
    let stored = le32(sector, 16).ok_or(GptError::Header)?;
    let mut copy = [0u8; 512];
    let region = copy.get_mut(..size).ok_or(GptError::Header)?;
    region.copy_from_slice(sector.get(..size).ok_or(GptError::Header)?);
    if let Some(crc) = region.get_mut(16..20) {
        crc.fill(0);
    }
    if crc32(region) != stored || le64(sector, 24) != Some(1) {
        return Err(GptError::Header);
    }
    let header = Header {
        entries_lba: le64(sector, 72).ok_or(GptError::Header)?,
        entries: le32(sector, 80).ok_or(GptError::Header)?,
        entry_size: le32(sector, 84).ok_or(GptError::Header)?,
        entries_crc: le32(sector, 88).ok_or(GptError::Header)?,
        first_usable: le64(sector, 40).ok_or(GptError::Header)?,
        last_usable: le64(sector, 48).ok_or(GptError::Header)?,
        disk: sector
            .get(56..72)
            .and_then(|raw| raw.try_into().ok())
            .ok_or(GptError::Header)?,
    };
    if header.entry_size < 128
        || header.entry_size > MAX_ENTRY_SIZE
        || !(header.entry_size / 128).is_power_of_two()
        || !header.entry_size.is_multiple_of(128)
        || header.entries == 0
        || header.entries > MAX_ENTRIES
        || header.array_bytes() > MAX_ARRAY_BYTES
        || header.entries_lba < 2
        || header.entries_lba >= blocks
        || header.last_usable >= blocks
        || header.first_usable < 2
        || header.first_usable > header.last_usable
    {
        return Err(GptError::Header);
    }
    Ok(header)
}

/// Find the control partition and the two slots in the entry array.
///
/// `blocks` and `block_size` describe the disk; every partition of a Ferrix
/// type must lie inside it, and the control partition and the slots must
/// be large enough for their formats.
pub fn find(
    header: &Header,
    array: &[u8],
    blocks: u64,
    block_size: u32,
) -> Result<Layout, GptError> {
    let array = usize::try_from(header.array_bytes())
        .ok()
        .and_then(|len| array.get(..len))
        .ok_or(GptError::Entries)?;
    if crc32(array) != header.entries_crc {
        return Err(GptError::Entries);
    }
    let entries = || array.chunks_exact(header.entry_size as usize).enumerate();
    let mut control = None;
    let mut slots: [Option<Part>; 2] = [None, None];
    let mut slot_count = 0u32;
    for (index, entry) in entries() {
        let Some((kind, part)) = entry_part(index, entry)? else {
            continue;
        };
        if part.first_lba == 0 || part.first_lba > part.last_lba || part.last_lba >= blocks {
            return Err(GptError::Range);
        }
        if kind != SLOT_TYPE && kind != CONTROL_TYPE {
            continue;
        }
        // A Ferrix partition only inside the usable blocks: a control
        // partition over the GPT's own sectors would put the loader's one
        // write on the table (the consultant's G1).
        if part.first_lba < header.first_usable || part.last_lba > header.last_usable {
            return Err(GptError::Range);
        }
        // Hostile input: a Ferrix partition overlapping any other entry
        // would let a write to it land in someone else's.
        for (other_index, other) in entries() {
            if other_index == index {
                continue;
            }
            if let Some((_, other)) = entry_part(other_index, other)?
                && part.first_lba <= other.last_lba
                && other.first_lba <= part.last_lba
            {
                return Err(GptError::Overlap);
            }
        }
        if kind == CONTROL_TYPE {
            if control.replace(part).is_some() {
                return Err(GptError::ManyControls);
            }
        } else {
            if let Some(place) = slots.get_mut(slot_count as usize) {
                *place = Some(part);
            }
            slot_count += 1;
        }
    }
    let control = control.ok_or(GptError::NoControl)?;
    let (Some(a), Some(b), 2) = (slots[0], slots[1], slot_count) else {
        return Err(GptError::SlotCount);
    };
    if control.bytes(block_size) < MIN_CONTROL_BYTES
        || a.bytes(block_size) < 2 * HEADER_LEN as u64
        || b.bytes(block_size) < 2 * HEADER_LEN as u64
    {
        return Err(GptError::TooSmall);
    }
    Ok(Layout {
        control,
        a,
        b,
        disk: header.disk,
    })
}

/// Why [`find_one`] found no partition to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The entry array is damaged, or the partition's range is not inside
    /// the usable blocks, or it overlaps another entry.
    Table(GptError),
    /// No partition of the type.
    Missing,
    /// More than one: which to write is not the loader's to guess.
    Many,
}

/// The one partition of type `kind` in the entry array, checked as [`find`]
/// checks a Ferrix partition: the array's CRC-32, a range inside the disk
/// and the usable blocks, and no overlap with any other entry.
pub fn find_one(header: &Header, array: &[u8], blocks: u64, kind: Guid) -> Result<Part, Lookup> {
    let array = usize::try_from(header.array_bytes())
        .ok()
        .and_then(|len| array.get(..len))
        .ok_or(Lookup::Table(GptError::Entries))?;
    if crc32(array) != header.entries_crc {
        return Err(Lookup::Table(GptError::Entries));
    }
    let entries = || array.chunks_exact(header.entry_size as usize).enumerate();
    let mut found = None;
    for (index, entry) in entries() {
        let Some((this, part)) = entry_part(index, entry).map_err(Lookup::Table)? else {
            continue;
        };
        if this != kind {
            continue;
        }
        if part.first_lba == 0
            || part.first_lba > part.last_lba
            || part.last_lba >= blocks
            || part.first_lba < header.first_usable
            || part.last_lba > header.last_usable
        {
            return Err(Lookup::Table(GptError::Range));
        }
        for (other_index, other) in entries() {
            if other_index == index {
                continue;
            }
            if let Some((_, other)) = entry_part(other_index, other).map_err(Lookup::Table)?
                && part.first_lba <= other.last_lba
                && other.first_lba <= part.last_lba
            {
                return Err(Lookup::Table(GptError::Overlap));
            }
        }
        if found.replace(part).is_some() {
            return Err(Lookup::Many);
        }
    }
    found.ok_or(Lookup::Missing)
}

/// An entry's type and partition, or `None` for an unused entry (type zero).
fn entry_part(index: usize, entry: &[u8]) -> Result<Option<(Guid, Part)>, GptError> {
    let kind: Guid = entry
        .get(..16)
        .and_then(|raw| raw.try_into().ok())
        .ok_or(GptError::Entries)?;
    if kind == [0; 16] {
        return Ok(None);
    }
    let part = Part {
        index: u32::try_from(index).map_err(|_| GptError::Entries)?,
        unique: entry
            .get(16..32)
            .and_then(|raw| raw.try_into().ok())
            .ok_or(GptError::Entries)?,
        first_lba: le64(entry, 32).ok_or(GptError::Entries)?,
        last_lba: le64(entry, 40).ok_or(GptError::Entries)?,
    };
    Ok(Some((kind, part)))
}

/// A GUID in its usual text form, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`,
/// lowercase, from the table's byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuidText(pub Guid);

/// A GUID as [`GuidText`] prints it, `8-4-4-4-12` hex digits, either case.
#[must_use]
pub fn parse_guid(text: &str) -> Option<Guid> {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    let order = [3usize, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut guid = [0u8; 16];
    let mut at = 0;
    for (place, &index) in order.iter().enumerate() {
        if matches!(place, 4 | 6 | 8 | 10) {
            if bytes.get(at) != Some(&b'-') {
                return None;
            }
            at += 1;
        }
        let pair = text.get(at..at + 2)?;
        *guid.get_mut(index)? = u8::from_str_radix(pair, 16).ok()?;
        at += 2;
    }
    Some(guid)
}

impl core::fmt::Display for GuidText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let g = &self.0;
        let order = [3usize, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
        for (at, &index) in order.iter().enumerate() {
            if matches!(at, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{:02x}", g.get(index).copied().unwrap_or(0))?;
        }
        Ok(())
    }
}
