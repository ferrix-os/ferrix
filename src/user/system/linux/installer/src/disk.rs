//! A disk as a file: its size, its GUID partition table, its partitions'
//! device nodes, and whole-range copies with a read-back.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};

use ferrix_partition::{Header, Partition, parse_entries, parse_header};

/// Bytes in a sector: virtio's and the SN720's unit.
pub const SECTOR: u64 = 512;
/// Bytes copied or compared at a time.
pub const CHUNK: usize = 1 << 20;

/// A disk's table: its header and its used entries, each with its index.
pub struct Table {
    pub header: Header,
    pub entries: Vec<(u32, Partition)>,
}

/// Open `path` to read.
pub fn open(path: &str) -> io::Result<File> {
    File::open(path)
}

/// Open `path` to read and write.
pub fn open_rw(path: &str) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

/// The size of the disk or file open as `file`, in bytes.
pub fn size(file: &mut File) -> io::Result<u64> {
    file.seek(SeekFrom::End(0))
}

/// The primary GUID partition table of `disk`, or `None` when it has none
/// or a damaged one.
pub fn table(disk: &mut File) -> io::Result<Option<Table>> {
    let sectors = size(disk)? / SECTOR;
    let mut sector = [0_u8; SECTOR as usize];
    read_at(disk, SECTOR, &mut sector)?;
    let Ok(header) = parse_header(&sector, sectors) else {
        return Ok(None);
    };
    let mut array = vec![0_u8; header.entries_bytes()];
    read_at(
        disk,
        header
            .entries_at
            .checked_mul(SECTOR)
            .ok_or_else(|| io::Error::other("an entry array past the end"))?,
        &mut array,
    )?;
    Ok(parse_entries(&header, &array)
        .ok()
        .map(|entries| Table { header, entries }))
}

/// The ASCII name of a partition, up to its first zero.
#[must_use]
pub fn name(partition: &Partition) -> String {
    partition
        .name
        .iter()
        .take_while(|&&unit| unit != 0)
        .map(|&unit| char::from_u32(u32::from(unit)).unwrap_or('?'))
        .collect()
}

/// The device node of partition `number` (1-based) of the whole disk
/// `disk`: `/dev/vdd4`, and `/dev/nvme0n1p4` for a disk whose name ends in a
/// digit, as Linux names them.
#[must_use]
pub fn partition_node(disk: &str, number: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{number}")
    } else {
        format!("{disk}{number}")
    }
}

/// The whole disks a running Ferrix may have: virtio's and NVMe's.
#[must_use]
pub fn whole_disks() -> Vec<String> {
    let mut found = Vec::new();
    for letter in b'a'..=b'z' {
        found.push(format!("/dev/vd{}", letter as char));
    }
    for controller in 0..4 {
        for namespace in 1..=2 {
            found.push(format!("/dev/nvme{controller}n{namespace}"));
        }
    }
    found
        .into_iter()
        .filter(|path| std::path::Path::new(path).exists())
        .collect()
}

pub fn read_at(file: &mut File, at: u64, bytes: &mut [u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(bytes)
}

pub fn write_at(file: &mut File, at: u64, bytes: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(at))?;
    file.write_all(bytes)
}

/// Copy `bytes` bytes from the start of `source` to the start of `target`,
/// then flush `target` and read every byte back to compare it with the
/// source. `progress` is told the bytes done after each MiB written and
/// each MiB compared.
///
/// # Errors
///
/// A read or write error, or a mismatch on the read-back, naming the first
/// byte that differs.
pub fn copy_verified(
    source: &mut File,
    target: &mut File,
    bytes: u64,
    progress: &mut dyn FnMut(&str, u64),
) -> io::Result<()> {
    let mut buffer = vec![0_u8; CHUNK];
    let mut done = 0;
    while done < bytes {
        let len = chunk(bytes - done);
        read_at(source, done, &mut buffer[..len])?;
        write_at(target, done, &buffer[..len])?;
        done += len as u64;
        progress("written", done);
    }
    target.sync_all()?;
    compare(source, target, bytes, progress)
}

/// Compare the first `bytes` bytes of `source` and `target`.
///
/// # Errors
///
/// A read error, or the first byte that differs.
pub fn compare(
    source: &mut File,
    target: &mut File,
    bytes: u64,
    progress: &mut dyn FnMut(&str, u64),
) -> io::Result<()> {
    let mut wanted = vec![0_u8; CHUNK];
    let mut found = vec![0_u8; CHUNK];
    let mut done = 0;
    while done < bytes {
        let len = chunk(bytes - done);
        read_at(source, done, &mut wanted[..len])?;
        read_at(target, done, &mut found[..len])?;
        if let Some(at) = wanted[..len]
            .iter()
            .zip(&found[..len])
            .position(|(a, b)| a != b)
        {
            return Err(io::Error::other(format!(
                "read back differs at byte {}",
                done + at as u64
            )));
        }
        done += len as u64;
        progress("verified", done);
    }
    Ok(())
}

fn chunk(left: u64) -> usize {
    usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK)
}
