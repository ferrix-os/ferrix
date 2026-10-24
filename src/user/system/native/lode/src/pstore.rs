//! The loader's `FERRIX/LASTKERNEL.TXT` off the ESP (`docs/LOGGING.md` §5):
//! the disks `/proc/partitions` names, each read through its `/dev` node,
//! its GUID partition table's EFI system partition -- or the disk itself,
//! when it is a FAT32 volume with no table, as the test images are -- and
//! the file through `ferrix-fat`, read only, at most
//! [`ferrix_lode_core::lastkernel::MOST`] bytes.
//!
//! Root's alone: a raw block device opens only for root (VULNERABILITY-
//! ANALYSIS V-17), and the lode service is uid 91 by the time a disk's
//! driver has started. So `lode pstore-fetch`, root's one-shot
//! (`lode-pstore.service`), reads the file and copies it to [`COPY`], lode's
//! alone, and the service imports from the copy. Nothing is written to any
//! disk here.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_fat::{Device, Error, Volume};
use ferrix_lode_core::lastkernel;
use ferrix_partition::{ESP, parse_entries, parse_header};

use crate::sys;

/// A disk's sector, as GUID partition tables count them.
const SECTOR: u64 = 512;

/// The block a read is made of: the device takes aligned whole blocks.
const BLOCK: usize = 4096;

/// A block device, open, read in whole aligned blocks from `base` on.
struct Disk {
    /// The node, open read only.
    fd: usize,
    /// Where the volume begins on it.
    base: u64,
    /// The block last read, and its number.
    cached: Option<(u64, [u8; BLOCK])>,
}

impl Device for Disk {
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let mut at = self.base.checked_add(offset).ok_or(Error::Io)?;
        let mut done = 0;
        while done < buffer.len() {
            let number = at / BLOCK as u64;
            let within = (at % BLOCK as u64) as usize;
            if self.cached.is_none_or(|(cached, _)| cached != number) {
                let mut block = [0_u8; BLOCK];
                let start = usize::try_from(number * BLOCK as u64).map_err(|_| Error::Io)?;
                let _ = sys::lseek(self.fd, start, sys::SEEK_SET).map_err(|_| Error::Io)?;
                let got = sys::read(self.fd, &mut block).map_err(|_| Error::Io)?;
                if got != BLOCK {
                    return Err(Error::Io);
                }
                self.cached = Some((number, block));
            }
            let Some((_, block)) = &self.cached else {
                return Err(Error::Io);
            };
            let take = (BLOCK - within).min(buffer.len() - done);
            let (Some(to), Some(from)) = (
                buffer.get_mut(done..done + take),
                block.get(within..within + take),
            ) else {
                return Err(Error::Io);
            };
            to.copy_from_slice(from);
            done += take;
            at += take as u64;
        }
        Ok(())
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        sys::close(self.fd);
    }
}

/// The disks `/proc/partitions` names, with their sizes in sectors.
fn disks() -> Vec<(String, u64)> {
    let text = sys::read_small(b"/proc/partitions").unwrap_or_default();
    String::from_utf8_lossy(&text)
        .lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let [_, _, blocks, name] = words[..] else {
                return None;
            };
            let kib: u64 = blocks.parse().ok()?;
            Some((String::from(name), kib.saturating_mul(2)))
        })
        .collect()
}

/// `name`'s node, open read only, at `base`.
fn open(name: &str, base: u64) -> Option<Disk> {
    let path = alloc::format!("/dev/{name}");
    let fd = sys::openat(
        sys::AT_FDCWD,
        path.as_bytes(),
        sys::O_RDONLY | sys::O_CLOEXEC,
        0,
    )
    .ok()?;
    Some(Disk {
        fd,
        base,
        cached: None,
    })
}

/// Where `name`'s EFI system partitions begin, by its table; empty for a
/// disk with none or no table.
fn esps(name: &str, sectors: u64) -> Vec<u64> {
    let Some(mut disk) = open(name, 0) else {
        return Vec::new();
    };
    let mut sector = [0_u8; SECTOR as usize];
    if disk.read_at(SECTOR, &mut sector).is_err() {
        return Vec::new();
    }
    let Ok(header) = parse_header(&sector, sectors) else {
        return Vec::new();
    };
    let length = header.entries_bytes();
    if length > 64 * 1024 {
        return Vec::new();
    }
    let mut array = alloc::vec![0_u8; length];
    if disk
        .read_at(header.entries_at.saturating_mul(SECTOR), &mut array)
        .is_err()
    {
        return Vec::new();
    }
    parse_entries(&header, &array)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, partition)| partition.type_guid == ESP)
        .map(|(_, partition)| partition.first.saturating_mul(SECTOR))
        .collect()
}

/// The file's bytes off the first volume that has it, or why there are
/// none: `Err` with `None` when no volume has it, which is a boot with
/// nothing to import (a cold boot, or `ferrix.pstore=off`).
pub(crate) fn read() -> Result<Vec<u8>, Option<String>> {
    let mut why = None;
    for (name, sectors) in disks() {
        let mut places = esps(&name, sectors);
        if places.is_empty() {
            // No table: a FAT32 volume on the whole disk, or nothing.
            places.push(0);
        }
        for base in places {
            let Some(disk) = open(&name, base) else {
                continue;
            };
            let Ok(mut volume) = Volume::open(disk) else {
                continue;
            };
            match volume.read_file(lastkernel::PATH, lastkernel::MOST) {
                Ok(bytes) => return Ok(bytes),
                Err(Error::NotFound | Error::NotAFile) => {}
                Err(other) => why = Some(alloc::format!("/dev/{name}: {other:?}")),
            }
        }
    }
    Err(why)
}

/// Where `lode pstore-fetch` leaves the file for the service: lode's, 0600.
pub(crate) const COPY: &[u8] = b"/run/log/lastkernel.txt";

/// How long `lode pstore-fetch` waits for a disk that has the file.
const WAIT_MS: u64 = 120_000;

/// `lode pstore-fetch`: [`read`], waiting up to 2 min for the disk's
/// driver, and the file copied to [`COPY`] for the service; the exit status.
pub(crate) fn fetch() -> i32 {
    let mut waited = 0;
    let bytes = loop {
        match read() {
            Ok(bytes) => break bytes,
            Err(Some(why)) => {
                crate::cli::complain(&alloc::format!("lode pstore-fetch: {why}"));
                return 1;
            }
            Err(None) if waited >= WAIT_MS => {
                crate::cli::out(
                    alloc::format!(
                        "lode pstore-fetch: no disk holds {}: nothing to import\n",
                        lastkernel::PATH
                    )
                    .as_bytes(),
                );
                return 0;
            }
            Err(None) => {
                sys::sleep_ms(2000);
                waited += 2000;
            }
        }
    };
    let fresh = b"/run/log/lastkernel.txt.new";
    let written = sys::openat(
        sys::AT_FDCWD,
        fresh,
        sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC,
        0o600,
    )
    .and_then(|fd| {
        let done = sys::fchown(fd, crate::serve::UID, crate::serve::UID)
            .and_then(|()| sys::fchmod(fd, 0o600))
            .and_then(|()| sys::write_all(fd, &bytes));
        sys::close(fd);
        done
    })
    .and_then(|()| sys::rename(fresh, COPY));
    match written {
        Ok(()) => {
            crate::cli::out(
                alloc::format!(
                    "lode pstore-fetch: {} bytes of {} copied for lode\n",
                    bytes.len(),
                    lastkernel::PATH
                )
                .as_bytes(),
            );
            0
        }
        Err(error) => {
            crate::cli::complain(&alloc::format!(
                "lode pstore-fetch: /run/log/lastkernel.txt: errno {}",
                error.0
            ));
            1
        }
    }
}
