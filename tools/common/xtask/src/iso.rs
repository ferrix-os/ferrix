//! A hybrid ISO, written in Rust as `fat.rs` writes FAT (`docs/INSTALLER.md`
//! §3.1, I6b): one file that boots as a CD and as a disk.
//!
//! ```text
//! 512-byte sectors  2048-byte blocks
//! 0                 0      protective MBR           \
//! 1                 0      GPT header                > the ISO's system area,
//! 2..33             0..8   GPT partition entries    /  which ISO 9660 leaves free
//! 64                16     primary volume descriptor
//! 68                17     El Torito's boot record
//! 72                18     the descriptor set's terminator
//!                   19, 20 the path tables (L and M)
//!                   21     the root directory
//!                   22     the boot catalog
//!                   23..   README.TXT and the other files, a block each at least
//! 1 MiB aligned     ...    the EFI system partition: GPT partition 1, named
//!                          FERRIX-LIVE, and El Torito's EFI boot image, and the
//!                          file EFIBOOT.IMG in the ISO's root, all one extent
//! last 33           ...    the backup GPT
//! ```
//!
//! Firmware that boots the file as a CD finds El Torito's catalog, whose one
//! entry is the EFI system partition (platform `0xEF`, no emulation); EDK2,
//! and the firmware built on it, reads a no-emulation image whose sector
//! count is 0 as reaching to the volume's end, as `xorriso` writes one larger
//! than 32 MiB. Firmware that boots it as a disk, after `dd` to a USB stick or
//! attached to a virtual machine as a disk, finds the GUID partition table,
//! whose one partition is the same bytes. Ferrix itself sees only the second
//! view, and only on a disk it drives: the installer finds the live system by
//! the partition's name, as on the `.img` (`installer.rs`).
//!
//! The MBR is the protective one the UEFI specification asks for, a single
//! `0xEE` record: no legacy BIOS boot, which Ferrix does not have. Every time
//! and identifier is a constant, so one tree writes one ISO.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use ferrix_partition::{ESP, Partition, write_table};

use crate::installer::{LIVE_DISK_GUID, LIVE_NAME, LIVE_PART_GUID, hide};
use crate::{Error, Result};

/// ISO 9660's logical block.
pub(crate) const BLOCK: u64 = 2048;
/// A disk's sector, as the GPT counts them.
const SECTOR: u64 = 512;
/// The ESP starts on a MiB, as every partitioning tool puts partitions.
const ALIGN_BLOCKS: u64 = (1 << 20) / BLOCK;
/// The fixed blocks before the first file.
const PVD: u64 = 16;
const BOOT_RECORD: u64 = 17;
const TERMINATOR: u64 = 18;
const L_PATH: u64 = 19;
const M_PATH: u64 = 20;
const ROOT: u64 = 21;
const CATALOG: u64 = 22;
const FIRST_FILE: u64 = 23;
/// The name the ESP has in the ISO's root, as `xorriso` shows its EFI image.
const ESP_FILE: &str = "EFIBOOT.IMG";
/// 2026-01-01 00:00:00 UTC, the time every record carries.
const YEAR: u8 = 126;

/// A file in the ISO 9660 root beside the ESP: a name of ISO 9660 level 2
/// (`A-Z`, `0-9`, `_`, one dot) and its bytes.
pub(crate) struct Extra {
    pub(crate) name: String,
    pub(crate) data: Vec<u8>,
}

/// Where things went, for the caller's line and the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    /// The ESP's first block.
    pub(crate) esp_block: u64,
    /// Its length in bytes.
    pub(crate) esp_bytes: u64,
    /// The whole file, in bytes.
    pub(crate) bytes: u64,
}

/// Write the hybrid ISO at `out` from the FAT volume at `volume` and the
/// files `extras`, with the volume identifier `label`.
pub(crate) fn write(volume: &Path, extras: &[Extra], label: &str, out: &Path) -> Result<Layout> {
    let on = |path: &Path| {
        let path = path.display().to_string();
        move |error: std::io::Error| Error::new(format!("{path}: {error}"))
    };
    let mut source = File::open(volume).map_err(on(volume))?;
    let esp_bytes = source.metadata().map_err(on(volume))?.len();
    let mut target = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .map_err(on(out))?;
    let layout = lay_out(&mut source, esp_bytes, extras, label, &mut target)?;
    target.sync_all().map_err(on(out))?;
    Ok(layout)
}

/// [`write()`] onto any seekable target: the tests write into memory.
pub(crate) fn lay_out<R: Read, W: Write + Seek>(
    volume: &mut R,
    esp_bytes: u64,
    extras: &[Extra],
    label: &str,
    out: &mut W,
) -> Result<Layout> {
    if esp_bytes == 0 || !esp_bytes.is_multiple_of(BLOCK) {
        return Err(Error::new(format!(
            "the EFI system partition is {esp_bytes} bytes, not whole 2048-byte blocks"
        )));
    }
    for extra in extras {
        check_name(&extra.name)?;
    }
    let mut extras: Vec<&Extra> = extras.iter().collect();
    extras.sort_by(|a, b| a.name.cmp(&b.name));

    // Where each file goes.
    let mut next = FIRST_FILE;
    let mut placed = Vec::new();
    for extra in &extras {
        placed.push((next, extra));
        next += blocks(extra.data.len() as u64).max(1);
    }
    let esp_block = next.next_multiple_of(ALIGN_BLOCKS);
    let esp_blocks = esp_bytes / BLOCK;
    let end_sector = (esp_block + esp_blocks) * (BLOCK / SECTOR);
    // The partition, then the backup table's 33 sectors, rounded up to a MiB.
    let sectors = (end_sector + 33).next_multiple_of((1 << 20) / SECTOR);
    let volume_blocks = sectors / (BLOCK / SECTOR);

    // The root directory: `.`, `..`, then the files and the ESP by name.
    let mut entries: Vec<(String, u64, u64)> = placed
        .iter()
        .map(|(at, extra)| (extra.name.clone(), *at, extra.data.len() as u64))
        .collect();
    entries.push((ESP_FILE.to_owned(), esp_block, esp_bytes));
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut root = Vec::new();
    root.extend(record(&[0], ROOT, BLOCK, true));
    root.extend(record(&[1], ROOT, BLOCK, true));
    for (name, at, len) in &entries {
        root.extend(record(format!("{name};1").as_bytes(), *at, *len, false));
    }
    if root.len() as u64 > BLOCK {
        return Err(Error::new("the ISO's root directory outgrew its one block"));
    }

    let mut put = |block: u64, bytes: &[u8]| -> Result<()> {
        let _ = out.seek(SeekFrom::Start(block * BLOCK))?;
        out.write_all(bytes)?;
        Ok(())
    };
    put(PVD, &primary(label, volume_blocks))?;
    put(BOOT_RECORD, &boot_record())?;
    put(TERMINATOR, &terminator())?;
    put(L_PATH, &path_table(false))?;
    put(M_PATH, &path_table(true))?;
    put(ROOT, &root)?;
    put(CATALOG, &catalog(esp_block))?;
    for (at, extra) in &placed {
        put(*at, &extra.data)?;
    }

    // The ESP, with its boot sector told where it starts on the disk, and
    // with sectors of zeros left unwritten so the file stays sparse.
    let first = esp_block * (BLOCK / SECTOR);
    let mut buffer = vec![0_u8; 1 << 20];
    let mut done = 0_u64;
    while done < esp_bytes {
        let len = usize::try_from((esp_bytes - done).min(buffer.len() as u64))
            .map_err(|_| Error::new("a chunk larger than memory"))?;
        let piece = buffer
            .get_mut(..len)
            .ok_or_else(|| Error::new("a chunk larger than its buffer"))?;
        volume.read_exact(piece)?;
        if done == 0 {
            hide(piece, first)?;
        }
        if piece.iter().any(|&byte| byte != 0) {
            let _ = out.seek(SeekFrom::Start(esp_block * BLOCK + done))?;
            out.write_all(piece)?;
        }
        done += len as u64;
    }

    // The partition table last: its backup header is the file's last
    // sector, which gives the file its length.
    let partition = Partition::new(
        ESP,
        LIVE_PART_GUID,
        first,
        first + esp_bytes / SECTOR - 1,
        LIVE_NAME,
    );
    let table = write_table(sectors, SECTOR as u32, LIVE_DISK_GUID, &[partition])
        .map_err(|error| Error::new(format!("the ISO's partition table: {error}")))?;
    for (lba, sector) in table {
        let _ = out.seek(SeekFrom::Start(lba * SECTOR))?;
        out.write_all(&sector)?;
    }
    Ok(Layout {
        esp_block,
        esp_bytes,
        bytes: sectors * SECTOR,
    })
}

/// `bytes` into `out` from `at`: a field of a fixed-size structure, which
/// always fits; one that would not is left out rather than panicking.
pub(crate) fn set(out: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(field) = out.get_mut(at..at + bytes.len()) {
        field.copy_from_slice(bytes);
    }
}

/// Blocks `bytes` takes.
fn blocks(bytes: u64) -> u64 {
    bytes.div_ceil(BLOCK)
}

/// A level 2 name: d-characters with one dot, at most 30 in all.
fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 30
        && name.matches('.').count() == 1
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        && name != ESP_FILE;
    if ok {
        Ok(())
    } else {
        Err(Error::new(format!(
            "{name:?} is not an ISO 9660 level 2 file name (A-Z, 0-9, _, one dot)"
        )))
    }
}

/// `value` as both-endian 32 bits: little, then big.
fn both32(value: u32) -> [u8; 8] {
    let mut out = [0_u8; 8];
    out[..4].copy_from_slice(&value.to_le_bytes());
    out[4..].copy_from_slice(&value.to_be_bytes());
    out
}

/// `value` as both-endian 16 bits.
fn both16(value: u16) -> [u8; 4] {
    let mut out = [0_u8; 4];
    out[..2].copy_from_slice(&value.to_le_bytes());
    out[2..].copy_from_slice(&value.to_be_bytes());
    out
}

/// A directory record for `name` (`\0` is `.`, `\x01` is `..`).
fn record(name: &[u8], extent: u64, len: u64, directory: bool) -> Vec<u8> {
    let mut out = vec![0_u8; 33];
    set(
        &mut out,
        2,
        &both32(u32::try_from(extent).unwrap_or(u32::MAX)),
    );
    set(
        &mut out,
        10,
        &both32(u32::try_from(len).unwrap_or(u32::MAX)),
    );
    set(&mut out, 18, &[YEAR, 1, 1, 0, 0, 0, 0]);
    set(&mut out, 25, &[if directory { 2 } else { 0 }]);
    set(&mut out, 28, &both16(1));
    set(&mut out, 32, &[u8::try_from(name.len()).unwrap_or(u8::MAX)]);
    out.extend_from_slice(name);
    if out.len() % 2 == 1 {
        out.push(0);
    }
    let len = u8::try_from(out.len()).unwrap_or(u8::MAX);
    set(&mut out, 0, &[len]);
    out
}

/// `text` in a field of `width`, padded with spaces.
fn field(text: &str, width: usize) -> Vec<u8> {
    let mut out: Vec<u8> = text.bytes().take(width).collect();
    out.resize(width, b' ');
    out
}

/// A 17-byte descriptor date: 2026-01-01 00:00:00.00, UTC.
fn long_date() -> [u8; 17] {
    let mut out = [0_u8; 17];
    out[..16].copy_from_slice(b"2026010100000000");
    out
}

/// The primary volume descriptor.
fn primary(label: &str, volume_blocks: u64) -> Vec<u8> {
    let mut out = vec![0_u8; BLOCK as usize];
    set(&mut out, 0, &[1]);
    set(&mut out, 1, b"CD001");
    set(&mut out, 6, &[1]);
    set(&mut out, 8, &field("", 32));
    set(&mut out, 40, &field(label, 32));
    set(
        &mut out,
        80,
        &both32(u32::try_from(volume_blocks).unwrap_or(u32::MAX)),
    );
    set(&mut out, 120, &both16(1));
    set(&mut out, 124, &both16(1));
    set(&mut out, 128, &both16(BLOCK as u16));
    set(&mut out, 132, &both32(10));
    set(&mut out, 140, &(L_PATH as u32).to_le_bytes());
    set(&mut out, 148, &(M_PATH as u32).to_be_bytes());
    let root = record(&[0], ROOT, BLOCK, true);
    set(&mut out, 156, &root);
    set(&mut out, 190, &field("", 128));
    set(&mut out, 318, &field("FERRIX", 128));
    set(&mut out, 446, &field("", 128));
    set(&mut out, 574, &field("CARGO XTASK ISO", 128));
    set(&mut out, 702, &field("LICENSE.TXT", 37));
    set(&mut out, 739, &field("", 37));
    set(&mut out, 776, &field("", 37));
    set(&mut out, 813, &long_date());
    set(&mut out, 830, &long_date());
    // Neither expires nor takes effect later: all zeros, as ISO 9660 says.
    set(&mut out, 847, &[b'0'; 17]);
    set(&mut out, 863, &[0]);
    set(&mut out, 864, &[b'0'; 17]);
    set(&mut out, 880, &[0]);
    set(&mut out, 881, &[1]);
    out
}

/// El Torito's boot record volume descriptor, pointing at the catalog.
fn boot_record() -> Vec<u8> {
    let mut out = vec![0_u8; BLOCK as usize];
    set(&mut out, 1, b"CD001");
    set(&mut out, 6, &[1]);
    let id = b"EL TORITO SPECIFICATION";
    set(&mut out, 7, id);
    set(&mut out, 0x47, &(CATALOG as u32).to_le_bytes());
    out
}

/// The volume descriptor set terminator.
fn terminator() -> Vec<u8> {
    let mut out = vec![0_u8; BLOCK as usize];
    set(&mut out, 0, &[255]);
    set(&mut out, 1, b"CD001");
    set(&mut out, 6, &[1]);
    out
}

/// The path table, one entry: the root.
fn path_table(big_endian: bool) -> Vec<u8> {
    let mut out = vec![1_u8, 0];
    let root = ROOT as u32;
    out.extend(if big_endian {
        root.to_be_bytes()
    } else {
        root.to_le_bytes()
    });
    out.extend(if big_endian {
        1_u16.to_be_bytes()
    } else {
        1_u16.to_le_bytes()
    });
    out.extend([0, 0]);
    out
}

/// The boot catalog: a validation entry for platform `0xEF`, EFI, and the
/// default entry, the ESP, bootable without emulation, its sector count 0
/// (`xorriso`'s for an image past 32 MiB: the image reaches the end).
fn catalog(esp_block: u64) -> Vec<u8> {
    let mut out = vec![0_u8; BLOCK as usize];
    set(&mut out, 0, &[1]);
    set(&mut out, 1, &[0xEF]);
    set(&mut out, 30, &[0x55]);
    set(&mut out, 31, &[0xAA]);
    let sum = out
        .get(..32)
        .unwrap_or_default()
        .chunks_exact(2)
        .fold(0_u16, |sum, word| {
            let pair = [
                word.first().copied().unwrap_or(0),
                word.get(1).copied().unwrap_or(0),
            ];
            sum.wrapping_add(u16::from_le_bytes(pair))
        });
    set(&mut out, 28, &0_u16.wrapping_sub(sum).to_le_bytes());
    set(&mut out, 32, &[0x88]);
    set(
        &mut out,
        40,
        &u32::try_from(esp_block).unwrap_or(u32::MAX).to_le_bytes(),
    );
    out
}

#[cfg(test)]
mod tests;
