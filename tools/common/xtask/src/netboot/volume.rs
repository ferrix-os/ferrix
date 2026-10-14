//! Every file on a FAT32 image `xtask` wrote, by its path: what `netboot`
//! serves is what `build` put on the image, read back rather than built a
//! second way.
//!
//! Short names only, as `crate::fat` writes them; a long-name entry is
//! skipped. Read through seeks rather than the whole image, which is
//! 512 MiB and mostly empty.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::{Error, Result};

/// The only sector size `crate::fat` writes.
const SECTOR: u64 = 512;
/// A long-name entry's attribute byte.
const LONG_NAME: u8 = 0x0F;
/// A directory's.
const DIRECTORY: u8 = 0x10;
/// The volume label's.
const VOLUME_ID: u8 = 0x08;
/// How deep a directory may be: the image's deepest is two.
const MAX_DEPTH: usize = 8;

/// An open image.
struct Volume {
    file: File,
    /// The first allocation table, whole.
    fat: Vec<u8>,
    /// Bytes in a cluster.
    cluster: u64,
    /// Where cluster 2 starts.
    data: u64,
}

/// Every file on the FAT32 image at `path`, by its slash-separated path.
pub(super) fn files(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let on = |error: Error| Error::new(format!("{}: {error}", path.display()));
    let file =
        File::open(path).map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    let mut out = BTreeMap::new();
    let (mut volume, root) = Volume::open(file).map_err(on)?;
    volume.walk(root, "", 0, &mut out).map_err(on)?;
    Ok(out)
}

/// `N` bytes of `bytes` at `at`.
fn field<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N]> {
    bytes
        .get(at..at + N)
        .and_then(|field| field.try_into().ok())
        .ok_or_else(|| Error::new("a field past the end of its sector"))
}

impl Volume {
    /// Read the boot sector and the allocation table; answer the volume
    /// and its root directory's cluster.
    fn open(mut file: File) -> Result<(Self, u32)> {
        let boot = read_at(&mut file, 0, SECTOR as usize)?;
        let [signature, mark] = field::<2>(&boot, 510)?;
        if u16::from_le_bytes(field(&boot, 11)?) != SECTOR as u16
            || (signature, mark) != (0x55, 0xAA)
            || &field::<8>(&boot, 82)? != b"FAT32   "
        {
            return Err(Error::new("not a FAT32 volume of 512-byte sectors"));
        }
        let [per_cluster] = field::<1>(&boot, 13)?;
        let reserved = u64::from(u16::from_le_bytes(field(&boot, 14)?));
        let [copies] = field::<1>(&boot, 16)?;
        let fat_sectors = u64::from(u32::from_le_bytes(field(&boot, 36)?));
        let root = u32::from_le_bytes(field(&boot, 44)?);
        let fat_bytes = usize::try_from(fat_sectors * SECTOR)
            .map_err(|_| Error::new("an allocation table larger than memory"))?;
        let fat = read_at(&mut file, reserved * SECTOR, fat_bytes)?;
        let volume = Volume {
            file,
            fat,
            cluster: u64::from(per_cluster) * SECTOR,
            data: (reserved + u64::from(copies) * fat_sectors) * SECTOR,
        };
        if volume.cluster == 0 {
            return Err(Error::new("a volume whose clusters hold no sectors"));
        }
        Ok((volume, root))
    }

    /// The chain of clusters from `first`.
    fn chain(&self, first: u32) -> Result<Vec<u32>> {
        let mut chain = Vec::new();
        let mut current = first;
        // A chain longer than the table has entries loops.
        let most = self.fat.len() / 4;
        while (2..0x0FFF_FFF8).contains(&current) {
            if chain.len() > most {
                return Err(Error::new("a cluster chain that loops"));
            }
            chain.push(current);
            let at = usize::try_from(current)
                .map_err(|_| Error::new("a cluster past memory"))?
                .checked_mul(4)
                .ok_or_else(|| Error::new("a cluster past memory"))?;
            current = u32::from_le_bytes(field(&self.fat, at)?) & 0x0FFF_FFFF;
        }
        Ok(chain)
    }

    /// The bytes of the chain from `first`, each run of consecutive
    /// clusters in one read.
    fn read_chain(&mut self, first: u32) -> Result<Vec<u8>> {
        let chain = self.chain(first)?;
        let mut out = Vec::new();
        let mut index = 0;
        while let Some(&start) = chain.get(index) {
            let mut run = 1;
            while chain.get(index + run) == Some(&(start + run as u32)) {
                run += 1;
            }
            let at = self.data + u64::from(start - 2) * self.cluster;
            let len = usize::try_from(run as u64 * self.cluster)
                .map_err(|_| Error::new("a run larger than memory"))?;
            out.extend(read_at(&mut self.file, at, len)?);
            index += run;
        }
        Ok(out)
    }

    /// Add every file under the directory at `cluster`, whose path is
    /// `prefix`, to `out`.
    fn walk(
        &mut self,
        cluster: u32,
        prefix: &str,
        depth: usize,
        out: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::new("directories nested deeper than the image's"));
        }
        let directory = self.read_chain(cluster)?;
        for slot in directory.chunks_exact(32) {
            let entry: [u8; 32] = field(slot, 0)?;
            let [first, ..] = entry;
            let [attributes] = field::<1>(&entry, 11)?;
            if first == 0 {
                break;
            }
            if first == 0xE5 || attributes == LONG_NAME || attributes & VOLUME_ID != 0 {
                continue;
            }
            let name = short_name(&field(&entry, 0)?);
            if name == "." || name == ".." {
                continue;
            }
            let high = u32::from(u16::from_le_bytes(field(&entry, 20)?));
            let low = u32::from(u16::from_le_bytes(field(&entry, 26)?));
            let start = (high << 16) | low;
            let size = usize::try_from(u32::from_le_bytes(field(&entry, 28)?))
                .map_err(|_| Error::new("a file larger than memory"))?;
            let path = format!("{prefix}{name}");
            if attributes & DIRECTORY != 0 {
                self.walk(start, &format!("{path}/"), depth + 1, out)?;
                continue;
            }
            let mut bytes = if start == 0 {
                Vec::new()
            } else {
                self.read_chain(start)?
            };
            if bytes.len() < size {
                return Err(Error::new(format!("{path} is shorter than its entry says")));
            }
            bytes.truncate(size);
            let _ = out.insert(path, bytes);
        }
        Ok(())
    }
}

/// An 8.3 entry's name as a path component: `KERNEL.ELF`, `BOOT`.
fn short_name(raw: &[u8; 11]) -> String {
    let (stem, extension) = raw.split_at(8);
    let stem = String::from_utf8_lossy(stem).trim_end().to_owned();
    let extension = String::from_utf8_lossy(extension).trim_end().to_owned();
    if extension.is_empty() {
        stem
    } else {
        format!("{stem}.{extension}")
    }
}

/// `len` bytes of `file` at `at`.
fn read_at(file: &mut File, at: u64, len: usize) -> Result<Vec<u8>> {
    let _ = file.seek(SeekFrom::Start(at))?;
    let mut bytes = vec![0_u8; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
