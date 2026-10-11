//! A reader of the FAT32 volumes `xtask` writes: 512-byte sectors and 8.3
//! names, which is all `xtask`'s FAT writer makes. `docs/INSTALLER.md`
//! §5.4's FAT library takes its place.
//!
//! The installer rewrites the copied volume's command line with it, and
//! `bootctl` reads the kernel, initramfs and command line out of an image
//! for a system slot.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use crate::disk::SECTOR;

/// A FAT32 volume's geometry, from its boot sector.
pub struct Fat {
    /// Where the volume starts on the disk, in bytes.
    at: u64,
    sectors_per_cluster: u64,
    /// Where cluster 2 starts, in bytes from `at`.
    data: u64,
    /// The root directory's first cluster.
    pub root: u32,
    /// The first allocation table, read once: following a chain through
    /// the disk a cluster at a time is a read per cluster.
    entries: Vec<u8>,
}

/// A directory entry found: its first cluster and size.
pub struct Entry {
    pub cluster: u32,
    pub size: u32,
}

impl Fat {
    /// Clusters followed in one chain before it is taken for a loop.
    const LONGEST: usize = 1 << 16;

    pub fn read(disk: &mut File, at: u64) -> io::Result<Fat> {
        let mut sector = [0_u8; 512];
        disk.seek(SeekFrom::Start(at))?;
        disk.read_exact(&mut sector)?;
        let u16_at = |i: usize| u64::from(u16::from_le_bytes([sector[i], sector[i + 1]]));
        let u32_at =
            |i: usize| u32::from_le_bytes([sector[i], sector[i + 1], sector[i + 2], sector[i + 3]]);
        let sectors_per_cluster = u64::from(sector[13]);
        if u16_at(11) != SECTOR || sectors_per_cluster == 0 || &sector[82..90] != b"FAT32   " {
            return Err(io::Error::other("not a FAT32 volume of 512-byte sectors"));
        }
        let reserved = u16_at(14);
        let tables = u64::from(sector[16]);
        let table_sectors = u64::from(u32_at(36));
        let mut entries =
            vec![0_u8; usize::try_from(table_sectors * SECTOR).map_err(io::Error::other)?];
        disk.seek(SeekFrom::Start(at + reserved * SECTOR))?;
        disk.read_exact(&mut entries)?;
        Ok(Fat {
            at,
            sectors_per_cluster,
            data: (reserved + tables * table_sectors) * SECTOR,
            root: u32_at(44),
            entries,
        })
    }

    pub fn cluster_bytes(&self) -> usize {
        usize::try_from(self.sectors_per_cluster * SECTOR).unwrap_or(usize::MAX)
    }

    pub fn cluster_offset(&self, cluster: u32) -> io::Result<u64> {
        let index = u64::from(cluster)
            .checked_sub(2)
            .ok_or_else(|| io::Error::other("a cluster number below 2"))?;
        Ok(self.at + self.data + index * self.sectors_per_cluster * SECTOR)
    }

    /// The cluster after `cluster` in its chain, or `None` at its end.
    pub fn next(&self, disk: &mut File, cluster: u32) -> io::Result<Option<u32>> {
        let _ = disk;
        let at = usize::try_from(cluster).map_err(io::Error::other)? * 4;
        let entry = self
            .entries
            .get(at..at + 4)
            .ok_or_else(|| io::Error::other("a cluster past the allocation table"))?;
        let value = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]) & 0x0FFF_FFFF;
        Ok((2..0x0FFF_FFF8).contains(&value).then_some(value))
    }

    /// The entry named `name` (8.3, space-padded) in the directory starting
    /// at `directory`, skipping the volume label and long-name entries.
    pub fn find(
        &self,
        disk: &mut File,
        directory: u32,
        name: &[u8; 11],
    ) -> io::Result<Option<Entry>> {
        let mut cluster = directory;
        let mut bytes = vec![0_u8; self.cluster_bytes()];
        for _ in 0..Self::LONGEST {
            disk.seek(SeekFrom::Start(self.cluster_offset(cluster)?))?;
            disk.read_exact(&mut bytes)?;
            for entry in bytes.chunks_exact(32) {
                match entry[0] {
                    0x00 => return Ok(None),
                    0xE5 => continue,
                    _ => {}
                }
                if entry[11] & 0x08 != 0 || &entry[..11] != name {
                    continue;
                }
                let high = u32::from(u16::from_le_bytes([entry[20], entry[21]]));
                let low = u32::from(u16::from_le_bytes([entry[26], entry[27]]));
                return Ok(Some(Entry {
                    cluster: (high << 16) | low,
                    size: u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]),
                }));
            }
            match self.next(disk, cluster)? {
                Some(next) => cluster = next,
                None => return Ok(None),
            }
        }
        Err(io::Error::other("a directory's cluster chain does not end"))
    }

    /// The whole of the file `entry`, following its cluster chain.
    ///
    /// # Errors
    ///
    /// A read error, or a chain shorter than the file.
    pub fn read_file(&self, disk: &mut File, entry: &Entry) -> io::Result<Vec<u8>> {
        let size = usize::try_from(entry.size).map_err(io::Error::other)?;
        let mut out = vec![0_u8; size];
        let mut done = 0;
        let mut cluster = Some(entry.cluster);
        let cluster_bytes = self.cluster_bytes();
        while done < size {
            let first =
                cluster.ok_or_else(|| io::Error::other("a file's cluster chain ends early"))?;
            // A run of consecutive clusters is one read: xtask writes each
            // file contiguously, so a file is usually one run.
            let mut run = 1;
            let mut next = self.next(disk, first)?;
            while next == Some(first + run) && done + run as usize * cluster_bytes < size {
                run += 1;
                next = self.next(disk, first + run - 1)?;
            }
            let take = (size - done).min(run as usize * cluster_bytes);
            disk.seek(SeekFrom::Start(self.cluster_offset(first)?))?;
            disk.read_exact(&mut out[done..done + take])?;
            done += take;
            cluster = next;
        }
        Ok(out)
    }

    /// The file at `path`, 8.3 names from the root, if it is there.
    ///
    /// # Errors
    ///
    /// A read error.
    pub fn open_path(&self, disk: &mut File, path: &[&[u8; 11]]) -> io::Result<Option<Entry>> {
        let mut directory = self.root;
        let mut found = None;
        for name in path {
            let Some(entry) = self.find(disk, directory, name)? else {
                return Ok(None);
            };
            directory = entry.cluster;
            found = Some(entry);
        }
        Ok(found)
    }
}
