//! FAT32, read only: one file read off a volume by its path, as `lode`
//! reads the loader's `FERRIX/LASTKERNEL.TXT` off the ESP (`docs/LOGGING.md`
//! §5, `docs/POSTMORTEM.md` §3.5).
//!
//! The volume is untrusted bytes: another operating system, firmware or a
//! damaged disk may have written them. So every step is bounded and checked
//! before it is taken:
//!
//! * the boot sector must describe FAT32 (no fixed root directory, a FAT32
//!   table size), with sector and cluster sizes of the allowed powers of two,
//!   and a table and data area inside the volume;
//! * a cluster number is used only inside the data area; a chain ends at an
//!   end mark, and one that is longer than the volume has clusters, or than
//!   its file needs, is a loop and an error; a free or bad cluster inside a
//!   chain is an error;
//! * a directory is read for at most [`DIRECTORY_MAX`] bytes, and a file only
//!   up to the caller's limit;
//! * a long name is taken only when its pieces come in order and their
//!   checksum is the short name's.
//!
//! Nothing is ever written: the reader has no write path.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The most a directory is read for: 65,536 entries.
pub const DIRECTORY_MAX: usize = 65_536 * 32;

/// Why a read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The device could not be read.
    Io,
    /// The boot sector does not describe a FAT32 volume.
    NotFat32(&'static str),
    /// The volume's structures are damaged: a cluster outside the volume, a
    /// loop, a free or bad cluster in a chain.
    Corrupt(&'static str),
    /// No such file or directory.
    NotFound,
    /// A path's component is a file, or the file a directory.
    NotAFile,
    /// The file is larger than the caller allowed.
    TooLarge,
}

/// Where the bytes come from: the volume's first byte at offset 0.
pub trait Device {
    /// Fill `buffer` from `offset`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the bytes cannot be read.
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), Error>;
}

impl Device for &[u8] {
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let start = usize::try_from(offset).map_err(|_| Error::Io)?;
        let end = start.checked_add(buffer.len()).ok_or(Error::Io)?;
        buffer.copy_from_slice(self.get(start..end).ok_or(Error::Io)?);
        Ok(())
    }
}

/// End-of-chain marks are this and above.
const END: u32 = 0x0FFF_FFF8;
/// The bad-cluster mark.
const BAD: u32 = 0x0FFF_FFF7;
/// The bits of a table entry that are its value.
const MASK: u32 = 0x0FFF_FFFF;

/// A directory entry's attributes.
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0F;

/// One FAT32 volume, its boot sector read and checked.
#[derive(Debug)]
pub struct Volume<D> {
    /// The bytes.
    device: D,
    /// Where the first table begins.
    fat_start: u64,
    /// Where cluster 2 begins.
    data_start: u64,
    /// A cluster's bytes.
    cluster_bytes: u32,
    /// How many clusters the data area holds.
    clusters: u32,
    /// The root directory's first cluster.
    root: u32,
}

/// One directory entry found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its name: the long name when it has a valid one, else `NAME.EXT`.
    pub name: String,
    /// Whether it is a directory.
    pub directory: bool,
    /// Its first cluster, 0 for none.
    pub cluster: u32,
    /// Its size, for a file.
    pub size: u32,
}

/// A little-endian `u16` at `at`.
fn u16_at(bytes: &[u8], at: usize) -> u16 {
    bytes
        .get(at..at + 2)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u16::from_le_bytes)
}

/// A little-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

impl<D: Device> Volume<D> {
    /// The volume on `device`, its boot sector checked.
    ///
    /// # Errors
    ///
    /// [`Error::NotFat32`] for a boot sector that is not FAT32's, or one whose
    /// areas do not fit; [`Error::Io`].
    pub fn open(mut device: D) -> Result<Volume<D>, Error> {
        let mut sector = [0_u8; 512];
        device.read_at(0, &mut sector)?;
        if sector.get(510..512) != Some(&[0x55, 0xAA][..]) {
            return Err(Error::NotFat32("no boot signature"));
        }
        let bytes_per_sector = u32::from(u16_at(&sector, 11));
        let per_cluster = u32::from(*sector.get(13).unwrap_or(&0));
        let reserved = u32::from(u16_at(&sector, 14));
        let fats = u32::from(*sector.get(16).unwrap_or(&0));
        let root_entries = u16_at(&sector, 17);
        let total16 = u32::from(u16_at(&sector, 19));
        let fat16_size = u16_at(&sector, 22);
        let total32 = u32_at(&sector, 32);
        let fat_size = u32_at(&sector, 36);
        let root = u32_at(&sector, 44);
        if ![512, 1024, 2048, 4096].contains(&bytes_per_sector) {
            return Err(Error::NotFat32("a sector size FAT does not allow"));
        }
        if per_cluster == 0 || !per_cluster.is_power_of_two() || per_cluster > 128 {
            return Err(Error::NotFat32("a cluster size FAT does not allow"));
        }
        if root_entries != 0 || fat16_size != 0 || fat_size == 0 {
            return Err(Error::NotFat32("a FAT12 or FAT16 volume"));
        }
        if reserved == 0 || !(1..=2).contains(&fats) {
            return Err(Error::NotFat32(
                "no reserved sectors, or a table count FAT does not allow",
            ));
        }
        let total = if total16 != 0 { total16 } else { total32 };
        let meta = u64::from(reserved) + u64::from(fats) * u64::from(fat_size);
        let data_sectors = u64::from(total)
            .checked_sub(meta)
            .filter(|sectors| *sectors > 0)
            .ok_or(Error::NotFat32("tables larger than the volume"))?;
        let clusters = u32::try_from(data_sectors / u64::from(per_cluster))
            .map_err(|_| Error::NotFat32("more clusters than FAT32 numbers"))?
            .min(MASK - 1);
        // The table must hold an entry for every cluster.
        if u64::from(fat_size) * u64::from(bytes_per_sector) / 4 < u64::from(clusters) + 2 {
            return Err(Error::NotFat32("a table too small for its clusters"));
        }
        let volume = Volume {
            device,
            fat_start: u64::from(reserved) * u64::from(bytes_per_sector),
            data_start: meta * u64::from(bytes_per_sector),
            cluster_bytes: bytes_per_sector * per_cluster,
            clusters,
            root,
        };
        let _ = volume.check_cluster(root)?;
        Ok(volume)
    }

    /// `cluster`, if it names one of the data area's.
    fn check_cluster(&self, cluster: u32) -> Result<u32, Error> {
        if cluster >= 2 && cluster - 2 < self.clusters {
            Ok(cluster)
        } else {
            Err(Error::Corrupt("a cluster outside the volume"))
        }
    }

    /// The cluster after `cluster` in its chain, `None` at the end.
    fn next(&mut self, cluster: u32) -> Result<Option<u32>, Error> {
        let mut entry = [0_u8; 4];
        self.device
            .read_at(self.fat_start + u64::from(cluster) * 4, &mut entry)?;
        match u32::from_le_bytes(entry) & MASK {
            value if value >= END => Ok(None),
            BAD => Err(Error::Corrupt("a bad cluster in a chain")),
            0 => Err(Error::Corrupt("a free cluster in a chain")),
            value => self.check_cluster(value).map(Some),
        }
    }

    /// The bytes of the chain from `first`, at most `most` bytes, and at most
    /// as many clusters as the volume has.
    fn chain(&mut self, first: u32, most: usize) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let mut cluster = Some(self.check_cluster(first)?);
        let cluster_bytes = self.cluster_bytes as usize;
        let mut steps = 0_u32;
        while let Some(at) = cluster {
            if out.len() >= most {
                break;
            }
            steps += 1;
            if steps > self.clusters {
                return Err(Error::Corrupt("a chain that loops"));
            }
            let start = out.len();
            out.resize(start + cluster_bytes, 0);
            let offset = self.data_start + u64::from(at - 2) * u64::from(self.cluster_bytes);
            self.device
                .read_at(offset, out.get_mut(start..).unwrap_or_default())?;
            cluster = self.next(at)?;
        }
        Ok(out)
    }

    /// The entries of the directory whose first cluster is `cluster`.
    ///
    /// # Errors
    ///
    /// As the chain's reads.
    pub fn list(&mut self, cluster: u32) -> Result<Vec<Entry>, Error> {
        let bytes = self.chain(cluster, DIRECTORY_MAX)?;
        Ok(entries(&bytes))
    }

    /// The root directory's entries.
    ///
    /// # Errors
    ///
    /// As [`Volume::list`].
    pub fn root(&mut self) -> Result<Vec<Entry>, Error> {
        let root = self.root;
        self.list(root)
    }

    /// The entry at `path`, its components separated by `/`, matched without
    /// regard to ASCII case.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotAFile`] for a file used as a directory,
    /// and the reads' errors.
    pub fn find(&mut self, path: &str) -> Result<Entry, Error> {
        let mut directory = self.root;
        let mut found = None;
        for component in path.split('/').filter(|part| !part.is_empty()) {
            if let Some(Entry {
                directory: false, ..
            }) = found
            {
                return Err(Error::NotAFile);
            }
            let entry = self
                .list(directory)?
                .into_iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(component))
                .ok_or(Error::NotFound)?;
            directory = entry.cluster;
            found = Some(entry);
        }
        found.ok_or(Error::NotFound)
    }

    /// The bytes of the file at `path`, if it has at most `most`.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past `most`; [`Error::NotAFile`] for a directory;
    /// [`Volume::find`]'s; [`Error::Corrupt`] for a chain shorter than the size.
    pub fn read_file(&mut self, path: &str, most: usize) -> Result<Vec<u8>, Error> {
        let entry = self.find(path)?;
        if entry.directory {
            return Err(Error::NotAFile);
        }
        let size = entry.size as usize;
        if size > most {
            return Err(Error::TooLarge);
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut bytes = self.chain(entry.cluster, size)?;
        if bytes.len() < size {
            return Err(Error::Corrupt("a chain shorter than its file"));
        }
        bytes.truncate(size);
        Ok(bytes)
    }
}

/// The checksum of an 8.3 name, which each of its long name's pieces holds.
fn checksum(short: &[u8]) -> u8 {
    short
        .iter()
        .take(11)
        .fold(0_u8, |sum, byte| sum.rotate_right(1).wrapping_add(*byte))
}

/// An 8.3 name as `NAME.EXT`, its padding dropped.
fn short_name(raw: &[u8]) -> String {
    let part = |bytes: &[u8]| -> String {
        bytes
            .iter()
            .map(|&byte| char::from(byte))
            .collect::<String>()
            .trim_end()
            .into()
    };
    let base = part(raw.get(..8).unwrap_or_default());
    let extension = part(raw.get(8..11).unwrap_or_default());
    if extension.is_empty() {
        base
    } else {
        alloc::format!("{base}.{extension}")
    }
}

/// The entries of a directory's bytes, to its end mark.
fn entries(bytes: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    // The long name being gathered: its pieces' UTF-16 units by order, the
    // order expected next, and the checksum they carry.
    let mut long: Vec<[u16; 13]> = Vec::new();
    let mut expected = 0_u8;
    let mut sum = 0_u8;
    for raw in bytes.chunks_exact(32) {
        let first = raw.first().copied().unwrap_or(0);
        if first == 0 {
            break;
        }
        if first == 0xE5 {
            long.clear();
            continue;
        }
        let attributes = raw.get(11).copied().unwrap_or(0);
        if attributes & 0x3F == ATTR_LONG_NAME {
            let order = first & 0x1F;
            if first & 0x40 != 0 {
                long.clear();
                expected = order;
                sum = raw.get(13).copied().unwrap_or(0);
            }
            if order == 0 || order != expected || raw.get(13).copied() != Some(sum) {
                long.clear();
                expected = 0;
                continue;
            }
            let mut units = [0_u16; 13];
            for (index, at) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
                .into_iter()
                .enumerate()
            {
                if let Some(unit) = units.get_mut(index) {
                    *unit = u16_at(raw, at);
                }
            }
            long.push(units);
            expected -= 1;
            continue;
        }
        let short = raw.get(..11).unwrap_or_default();
        let name = if !long.is_empty() && expected == 0 && checksum(short) == sum {
            let units: Vec<u16> = long
                .iter()
                .rev()
                .flatten()
                .copied()
                .take_while(|unit| *unit != 0 && *unit != 0xFFFF)
                .collect();
            char::decode_utf16(units)
                .map(|unit| unit.unwrap_or('?'))
                .collect()
        } else {
            short_name(short)
        };
        long.clear();
        expected = 0;
        if attributes & ATTR_VOLUME != 0 || name == "." || name == ".." {
            continue;
        }
        let cluster = (u32::from(u16_at(raw, 20)) << 16) | u32::from(u16_at(raw, 26));
        out.push(Entry {
            name,
            directory: attributes & ATTR_DIRECTORY != 0,
            cluster,
            size: u32_at(raw, 28),
        });
    }
    out
}

#[cfg(test)]
mod tests;
