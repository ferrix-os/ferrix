//! A disk's GUID partitions, published as disks of their own: `vda1`,
//! `vda2`, numbered as Linux numbers them, one minor above the disk's.
//!
//! The installer (`docs/INSTALLER.md` §5.2) writes an EFI system partition
//! and a root partition, and the next boot has to find the root on the
//! second. [`scan`] reads every whole disk's primary table once, when the
//! root is looked for, and registers each partition as a [`BlockDevice`]
//! that reads and writes its range of the disk. A disk whose table is
//! missing or damaged has no partitions, which is what a kernel looking for
//! its root should see. A table written later is seen by the next boot;
//! `BLKRRPART` does not read it again yet.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_partition::{parse_entries, parse_header};
use ferrix_vfs::Errno;

use crate::console::println;
use crate::fs::block::BlockDevice;
use crate::fs::devfs::{self, BlockRegistration};
use crate::sync::SpinLock;

/// Partitions a disk may have published: Linux gives a virtio disk sixteen
/// minors, the disk's and fifteen more.
const MOST: u32 = 15;

/// The registrations, kept for as long as the machine runs.
static PUBLISHED: SpinLock<Vec<BlockRegistration>> = SpinLock::new(Vec::new());

/// One partition: its disk and its range of sectors.
#[derive(Debug)]
struct Partition {
    /// The whole disk.
    disk: Arc<dyn BlockDevice>,
    /// Its first sector on the disk.
    start: u64,
    /// How many sectors it has.
    sectors: u64,
}

impl Partition {
    /// The disk's sector for `sector` of `bytes`, if the run is inside.
    fn on_disk(&self, sector: u64, bytes: usize) -> Result<u64, Errno> {
        let size = u64::from(self.disk.sector_size()).max(1);
        let count = (bytes as u64).div_ceil(size);
        match sector.checked_add(count) {
            Some(end) if end <= self.sectors => Ok(self.start + sector),
            _ => Err(Errno::EIO),
        }
    }
}

impl BlockDevice for Partition {
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        self.disk.read(self.on_disk(sector, buf.len())?, buf)
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn sector_size(&self) -> u32 {
        self.disk.sector_size()
    }

    fn read_only(&self) -> bool {
        self.disk.read_only()
    }

    fn offline(&self) -> bool {
        self.disk.offline()
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), Errno> {
        self.disk.write(self.on_disk(sector, buf.len())?, buf)
    }

    fn flush(&self) -> Result<(), Errno> {
        self.disk.flush()
    }
}

/// Publish every whole disk's partitions, once; a later call does nothing.
pub(crate) fn scan() {
    if !PUBLISHED.lock().is_empty() {
        return;
    }
    for disk in devfs::disks() {
        // A whole disk's minor is a multiple of sixteen, and its name ends
        // in a letter.
        if disk.minor % (MOST + 1) != 0 || disk.name.last().is_none_or(u8::is_ascii_digit) {
            continue;
        }
        let Ok(found) = partitions(disk.device.as_ref()) else {
            continue;
        };
        for (index, start, sectors) in found {
            let number = index + 1;
            let mut name = disk.name.clone();
            name.extend_from_slice(alloc::format!("{number}").as_bytes());
            let partition = Arc::new(Partition {
                disk: Arc::clone(&disk.device),
                start,
                sectors,
            });
            match devfs::register_block(&name, disk.major, disk.minor + number, partition) {
                Ok(registration) => {
                    println!(
                        "  disks    {} is a partition of {} sectors",
                        core::str::from_utf8(&name).unwrap_or("?"),
                        sectors
                    );
                    PUBLISHED.lock().push(registration);
                }
                Err(_) => println!(
                    "  disks    {} could not be published",
                    core::str::from_utf8(&name).unwrap_or("?")
                ),
            }
        }
    }
}

/// A disk's partitions: each one's index in the table, first sector and
/// size, for the first [`MOST`] entries.
fn partitions(disk: &dyn BlockDevice) -> Result<Vec<(u32, u64, u64)>, Errno> {
    let size = disk.sector_size() as usize;
    if size < 512 {
        return Err(Errno::EINVAL);
    }
    let mut sector = vec![0_u8; size];
    disk.read(1, &mut sector)?;
    let header = parse_header(&sector, disk.sectors()).map_err(|_| Errno::EINVAL)?;
    let mut array = vec![0_u8; header.entries_bytes().next_multiple_of(size)];
    disk.read(header.entries_at, &mut array)?;
    let entries = parse_entries(&header, &array).map_err(|_| Errno::EINVAL)?;
    Ok(entries
        .into_iter()
        .filter(|(index, _)| *index < MOST)
        .map(|(index, partition)| (index, partition.first, partition.sectors()))
        .collect())
}
