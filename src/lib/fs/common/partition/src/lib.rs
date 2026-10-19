//! GUID partition tables (UEFI 2.10 §5.3): read one off a disk, and write a
//! new one.
//!
//! The kernel reads a disk's table to publish its partitions (`vda1`,
//! `vda2`), and the installer writes one (`docs/INSTALLER.md` §5.2). Both
//! are pure functions of bytes here, so the host tests reach them.
//!
//! Reading trusts nothing: a header whose signature, size, CRC-32 or ranges
//! are wrong is refused, and so is an entry array whose CRC-32 does not
//! match or whose partitions leave the usable range. Only the primary table
//! is read; a disk whose primary is damaged has no partitions here, which
//! is the safe answer for a kernel looking for its root.
//!
//! Writing makes the whole table at once: the protective MBR, the primary
//! header and entry array, and the backup array and header at the end of
//! the disk, as [`write_table`]'s list of sectors to write.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

#[cfg(test)]
mod tests;

/// The header's signature, `"EFI PART"`.
const SIGNATURE: &[u8; 8] = b"EFI PART";
/// Revision 1.0.
const REVISION: u32 = 0x0001_0000;
/// Bytes of the header that its CRC covers.
const HEADER_SIZE: u32 = 92;
/// Entries a table written here has, as every tool writes.
const ENTRIES: u32 = 128;
/// Bytes per entry.
const ENTRY_SIZE: u32 = 128;
/// Characters in an entry's name.
const NAME_UNITS: usize = 36;

/// A GUID as the table stores it: the first three fields little-endian.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// The GUID written `a-b-c-d-e`, the way specifications print it.
    #[must_use]
    pub const fn from_fields(a: u32, b: u16, c: u16, d: u16, e: u64) -> Guid {
        let a = a.to_le_bytes();
        let b = b.to_le_bytes();
        let c = c.to_le_bytes();
        let d = d.to_be_bytes();
        let e = e.to_be_bytes();
        Guid([
            a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], e[2], e[3], e[4], e[5],
            e[6], e[7],
        ])
    }

    /// A random (version 4) GUID from sixteen random bytes.
    #[must_use]
    pub const fn random(mut bytes: [u8; 16]) -> Guid {
        bytes[7] = (bytes[7] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        Guid(bytes)
    }

    /// Whether it is all zeros: an unused entry's type.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 16]
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let g = &self.0;
        write!(
            f,
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-",
            g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6], g[8], g[9]
        )?;
        g[10..].iter().try_for_each(|byte| write!(f, "{byte:02X}"))
    }
}

/// The EFI system partition's type.
pub const ESP: Guid = Guid::from_fields(0xC12A_7328, 0xF81F, 0x11D2, 0xBA4B, 0x00A0_C93E_C93B);
/// Linux's generic filesystem data type, which Ferrix's root partition takes.
pub const LINUX_FILESYSTEM: Guid =
    Guid::from_fields(0x0FC6_3DAF, 0x8483, 0x4772, 0x8E79, 0x3D69_D847_7DE4);

/// One partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// What it holds.
    pub type_guid: Guid,
    /// Which one it is, unique on every disk.
    pub unique: Guid,
    /// Its first sector.
    pub first: u64,
    /// Its last sector, inclusive, as the table stores it.
    pub last: u64,
    /// Its name, UTF-16, zero-padded.
    pub name: [u16; NAME_UNITS],
}

impl Partition {
    /// A partition from `first` to `last` inclusive, named `name` (ASCII,
    /// cut at 36 characters).
    #[must_use]
    pub fn new(type_guid: Guid, unique: Guid, first: u64, last: u64, name: &str) -> Partition {
        let mut units = [0_u16; NAME_UNITS];
        for (unit, byte) in units.iter_mut().zip(name.bytes()) {
            *unit = u16::from(byte);
        }
        Partition {
            type_guid,
            unique,
            first,
            last,
            name: units,
        }
    }

    /// How many sectors it has.
    #[must_use]
    pub fn sectors(&self) -> u64 {
        self.last.saturating_sub(self.first).saturating_add(1)
    }
}

/// What a table's header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The disk's GUID.
    pub disk: Guid,
    /// Where the other header is.
    pub alternate: u64,
    /// The first sector a partition may use.
    pub first_usable: u64,
    /// The last one, inclusive.
    pub last_usable: u64,
    /// Where the entry array starts.
    pub entries_at: u64,
    /// How many entries it has.
    pub entries: u32,
    /// Bytes per entry.
    pub entry_size: u32,
    /// The array's CRC-32.
    pub entries_crc: u32,
}

impl Header {
    /// Bytes of the entry array.
    #[must_use]
    pub fn entries_bytes(&self) -> usize {
        self.entries as usize * self.entry_size as usize
    }
}

/// Why a table was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No `"EFI PART"` where the header should be.
    NoTable,
    /// A field the specification fixes is wrong, or a range is impossible.
    Malformed,
    /// The header's CRC-32 does not match it.
    HeaderChecksum,
    /// The entry array's CRC-32 does not match it.
    EntriesChecksum,
    /// A partition that overlaps another or leaves the usable range.
    BadPartition,
    /// A disk too small for the table asked for.
    TooSmall,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::NoTable => "no GUID partition table",
            Error::Malformed => "a malformed GUID partition table header",
            Error::HeaderChecksum => "a GUID partition table header whose checksum is wrong",
            Error::EntriesChecksum => "a partition entry array whose checksum is wrong",
            Error::BadPartition => "a partition outside the usable sectors or over another",
            Error::TooSmall => "a disk too small for the table",
        })
    }
}

/// Read a little-endian field.
fn le(bytes: &[u8], at: usize, width: usize) -> Option<u64> {
    let mut word = [0_u8; 8];
    word.get_mut(..width)?
        .copy_from_slice(bytes.get(at..at.checked_add(width)?)?);
    Some(u64::from_le_bytes(word))
}

/// A GUID at `at`.
fn guid(bytes: &[u8], at: usize) -> Option<Guid> {
    let mut out = [0_u8; 16];
    out.copy_from_slice(bytes.get(at..at.checked_add(16)?)?);
    Some(Guid(out))
}

/// The primary header, from the sector at LBA 1 of a disk of `sectors`
/// sectors.
///
/// # Errors
///
/// [`Error::NoTable`], [`Error::Malformed`] or [`Error::HeaderChecksum`].
pub fn parse_header(sector: &[u8], sectors: u64) -> Result<Header, Error> {
    if sector.get(..8) != Some(SIGNATURE.as_slice()) {
        return Err(Error::NoTable);
    }
    let field = |at, width| le(sector, at, width).ok_or(Error::Malformed);
    let size = usize::try_from(field(12, 4)?).map_err(|_| Error::Malformed)?;
    if size < HEADER_SIZE as usize || size > sector.len() {
        return Err(Error::Malformed);
    }
    let mut covered = sector.get(..size).ok_or(Error::Malformed)?.to_vec();
    covered.get_mut(16..20).ok_or(Error::Malformed)?.fill(0);
    if crc32(&covered) != u32::try_from(field(16, 4)?).map_err(|_| Error::Malformed)? {
        return Err(Error::HeaderChecksum);
    }
    let header = Header {
        disk: guid(sector, 56).ok_or(Error::Malformed)?,
        alternate: field(32, 8)?,
        first_usable: field(40, 8)?,
        last_usable: field(48, 8)?,
        entries_at: field(72, 8)?,
        entries: u32::try_from(field(80, 4)?).map_err(|_| Error::Malformed)?,
        entry_size: u32::try_from(field(84, 4)?).map_err(|_| Error::Malformed)?,
        entries_crc: u32::try_from(field(88, 4)?).map_err(|_| Error::Malformed)?,
    };
    let sane = field(24, 8)? == 1
        && header.first_usable <= header.last_usable
        && header.last_usable < sectors
        && header.entries_at >= 2
        && header.entries_at < header.first_usable
        && header.entry_size >= ENTRY_SIZE
        && header.entry_size.is_power_of_two()
        && header.entries <= 1024;
    if !sane {
        return Err(Error::Malformed);
    }
    Ok(header)
}

/// The used entries of `array`, the entry array `header` describes, with
/// each one's index in it.
///
/// # Errors
///
/// [`Error::EntriesChecksum`], or [`Error::BadPartition`] for one outside
/// the usable sectors, backwards, or over another.
pub fn parse_entries(header: &Header, array: &[u8]) -> Result<Vec<(u32, Partition)>, Error> {
    let array = array
        .get(..header.entries_bytes())
        .ok_or(Error::Malformed)?;
    if crc32(array) != header.entries_crc {
        return Err(Error::EntriesChecksum);
    }
    let mut used: Vec<(u32, Partition)> = Vec::new();
    for (index, entry) in array.chunks_exact(header.entry_size as usize).enumerate() {
        let type_guid = guid(entry, 0).ok_or(Error::Malformed)?;
        if type_guid.is_zero() {
            continue;
        }
        let mut name = [0_u16; NAME_UNITS];
        for (at, unit) in name.iter_mut().enumerate() {
            *unit = u16::try_from(le(entry, 56 + 2 * at, 2).ok_or(Error::Malformed)?)
                .map_err(|_| Error::Malformed)?;
        }
        let partition = Partition {
            type_guid,
            unique: guid(entry, 16).ok_or(Error::Malformed)?,
            first: le(entry, 32, 8).ok_or(Error::Malformed)?,
            last: le(entry, 40, 8).ok_or(Error::Malformed)?,
            name,
        };
        let inside = partition.first <= partition.last
            && partition.first >= header.first_usable
            && partition.last <= header.last_usable;
        let overlaps = used
            .iter()
            .any(|(_, other)| partition.first <= other.last && other.first <= partition.last);
        if !inside || overlaps {
            return Err(Error::BadPartition);
        }
        used.push((
            u32::try_from(index).map_err(|_| Error::Malformed)?,
            partition,
        ));
    }
    Ok(used)
}

/// Sectors the entry array takes, for sectors of `sector_size` bytes.
fn array_sectors(sector_size: u32) -> u64 {
    u64::from(ENTRIES * ENTRY_SIZE).div_ceil(u64::from(sector_size))
}

/// The first and last usable sector of a new table on a disk of `sectors`
/// sectors of `sector_size` bytes.
///
/// # Errors
///
/// [`Error::TooSmall`] for a disk with no room between the two tables.
pub fn usable(sectors: u64, sector_size: u32) -> Result<(u64, u64), Error> {
    let array = array_sectors(sector_size);
    let first = 2 + array;
    let last = sectors.checked_sub(2 + array).ok_or(Error::TooSmall)?;
    if last < first {
        return Err(Error::TooSmall);
    }
    Ok((first, last))
}

/// A new table on a disk of `sectors` sectors of `sector_size` bytes, as
/// the sectors to write: each a first sector and the bytes from it.
///
/// # Errors
///
/// [`Error::TooSmall`], [`Error::Malformed`] for more than 128 partitions or
/// a sector size under 512, and [`Error::BadPartition`] for a partition
/// outside the usable sectors or over another.
pub fn write_table(
    sectors: u64,
    sector_size: u32,
    disk: Guid,
    partitions: &[Partition],
) -> Result<Vec<(u64, Vec<u8>)>, Error> {
    if sector_size < 512 || !sector_size.is_power_of_two() || partitions.len() > ENTRIES as usize {
        return Err(Error::Malformed);
    }
    let (first_usable, last_usable) = usable(sectors, sector_size)?;
    let size = sector_size as usize;
    let mut array = vec![0_u8; (ENTRIES * ENTRY_SIZE) as usize];
    for (at, (partition, entry)) in partitions
        .iter()
        .zip(array.chunks_exact_mut(ENTRY_SIZE as usize))
        .enumerate()
    {
        let overlaps = partitions.iter().enumerate().any(|(other_at, other)| {
            other_at != at && partition.first <= other.last && other.first <= partition.last
        });
        if partition.first > partition.last
            || partition.first < first_usable
            || partition.last > last_usable
            || partition.type_guid.is_zero()
            || overlaps
        {
            return Err(Error::BadPartition);
        }
        put(entry, 0, &partition.type_guid.0)?;
        put(entry, 16, &partition.unique.0)?;
        put(entry, 32, &partition.first.to_le_bytes())?;
        put(entry, 40, &partition.last.to_le_bytes())?;
        for (at, unit) in partition.name.iter().enumerate() {
            put(entry, 56 + 2 * at, &unit.to_le_bytes())?;
        }
    }
    let entries_crc = crc32(&array);
    let array_sectors = array_sectors(sector_size);
    let last = sectors - 1;
    let header = |mine: u64, alternate: u64, entries_at: u64| {
        let mut sector = vec![0_u8; size];
        put(&mut sector, 0, SIGNATURE)?;
        put(&mut sector, 8, &REVISION.to_le_bytes())?;
        put(&mut sector, 12, &HEADER_SIZE.to_le_bytes())?;
        put(&mut sector, 24, &mine.to_le_bytes())?;
        put(&mut sector, 32, &alternate.to_le_bytes())?;
        put(&mut sector, 40, &first_usable.to_le_bytes())?;
        put(&mut sector, 48, &last_usable.to_le_bytes())?;
        put(&mut sector, 56, &disk.0)?;
        put(&mut sector, 72, &entries_at.to_le_bytes())?;
        put(&mut sector, 80, &ENTRIES.to_le_bytes())?;
        put(&mut sector, 84, &ENTRY_SIZE.to_le_bytes())?;
        put(&mut sector, 88, &entries_crc.to_le_bytes())?;
        let crc = crc32(sector.get(..HEADER_SIZE as usize).ok_or(Error::Malformed)?);
        put(&mut sector, 16, &crc.to_le_bytes())?;
        Ok::<_, Error>(sector)
    };
    let mut padded = array;
    padded.resize(
        usize::try_from(array_sectors).map_err(|_| Error::Malformed)? * size,
        0,
    );
    Ok(vec![
        (0, protective_mbr(sectors, size)?),
        (1, header(1, last, 2)?),
        (2, padded.clone()),
        (last - array_sectors, padded),
        (last, header(last, 1, last - array_sectors)?),
    ])
}

/// The protective MBR: one partition of type `0xEE` over the whole disk
/// after sector 0, so that a tool that knows only MBR leaves it alone.
fn protective_mbr(sectors: u64, size: usize) -> Result<Vec<u8>, Error> {
    let mut sector = vec![0_u8; size];
    let length = u32::try_from(sectors - 1).unwrap_or(u32::MAX);
    let entry = [
        0x00, 0x00, 0x02, 0x00, 0xEE, 0xFF, 0xFF, 0xFF, 0x01, 0x00, 0x00, 0x00,
    ];
    put(&mut sector, 446, &entry)?;
    put(&mut sector, 458, &length.to_le_bytes())?;
    put(&mut sector, 510, &[0x55, 0xAA])?;
    Ok(sector)
}

/// Copy `bytes` into `buffer` at `at`.
fn put(buffer: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), Error> {
    buffer
        .get_mut(at..at + bytes.len())
        .ok_or(Error::Malformed)?
        .copy_from_slice(bytes);
    Ok(())
}

/// CRC-32 as the table uses it: ISO-HDLC, reflected, polynomial
/// `0xEDB88320`, all ones in and out.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
