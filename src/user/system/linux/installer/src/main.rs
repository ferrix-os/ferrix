//! `ferrix-install`: put the live system on a disk of its own.
//!
//! The minimum `docs/INSTALLER.md` §11 calls the MVP. The target disk is
//! wiped and given a GUID partition table with two partitions:
//!
//! 1. an EFI system partition holding a byte copy of the live disk's FAT
//!    volume -- the loader, the kernel and the initramfs the machine booted;
//! 2. a root partition holding the empty btrfs volume labelled `ferrix-root`
//!    that `run` boots on (`src/lib/fs/btrfs/testdata/root.img.packed`, 1 GiB).
//!
//! The kernel does the rest at the installed disk's first boot, as it does
//! for every `run`: it finds `ferrix-root`, now on a partition, and unpacks
//! the initramfs onto it. The live medium's command line keeps `/` in
//! memory (`ferrix.root=tmpfs`); the copy on the installed disk says
//! `ferrix.root=btrfs` instead, rewritten in place in the copied volume.
//!
//! ```text
//! ferrix-install [--yes] [--from /dev/vdX] /dev/vdY
//! ```
//!
//! Without `--from` the live system is found by itself: the partition named
//! `FERRIX-LIVE` that `xtask live` writes (`docs/INSTALLER.md` §3.1), or a
//! whole disk whose first sector is the FAT32 boot sector `xtask` writes
//! (OEM name `FERRIX`), the image of `build --installer`. Either way it must
//! be the only one. A target that is the live disk, or holds the live
//! partition, is refused. Without `--yes` it asks before it erases anything.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

use ferrix_partition::{
    ESP, Guid, LINUX_FILESYSTEM, Partition, parse_entries, parse_header, usable, write_table,
};

/// The live medium's EFI system partition's name (`tools/common/xtask/src/installer.rs`).
const LIVE_NAME: &str = "FERRIX-LIVE";
/// The live session's root option, and what the installed system's says in
/// its place: the same length, so the file is rewritten in place.
const LIVE_ROOT: &[u8] = b"ferrix.root=tmpfs";
const INSTALLED_ROOT: &[u8] = b"ferrix.root=btrfs";

/// The empty root volume, as `xtask` packs it: records of a little-endian
/// `u64` offset and the 4 KiB block there; zeros elsewhere.
const ROOT_PACKED: &str = "/usr/share/ferrix/root.img.packed";
/// The root volume's size.
const ROOT_BYTES: u64 = 1 << 30;
/// A packed block.
const BLOCK: usize = 4096;
/// Bytes in a sector: virtio's unit.
const SECTOR: u64 = 512;
/// Partitions start on MiB boundaries, as every tool puts them.
const ALIGN: u64 = 2048;
/// What is wiped at each end of the disk, so no old table or superblock
/// survives outside the new partitions.
const WIPE: u64 = 1 << 20;
/// Bytes copied at a time.
const CHUNK: usize = 1 << 20;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("ferrix-install: {why}");
            ExitCode::FAILURE
        }
    }
}

/// What the command line asked.
struct Asked {
    yes: bool,
    from: Option<String>,
    target: String,
}

fn parse() -> Result<Asked, String> {
    let mut yes = false;
    let mut from = None;
    let mut target = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            "--from" => from = Some(args.next().ok_or("--from needs a disk")?),
            "--help" | "-h" => {
                return Err("usage: ferrix-install [--yes] [--from /dev/vdX] /dev/vdY".into());
            }
            _ if target.is_none() => target = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    let target = target.ok_or("name the disk to install on, as /dev/vdY")?;
    Ok(Asked { yes, from, target })
}

fn run() -> Result<(), String> {
    let asked = parse()?;
    let live = match asked.from {
        Some(from) => from,
        None => {
            let found = find_live()?;
            println!("ferrix-install: the live system is on {found}");
            found
        }
    };
    if live == asked.target {
        return Err(format!("{live} is the disk this system booted from"));
    }
    if holds(&asked.target, &live) {
        return Err(format!("{} holds {live}, the live system", asked.target));
    }
    if mounted(&asked.target)? {
        return Err(format!("{} has something mounted from it", asked.target));
    }
    let mut source = File::open(&live).map_err(|e| format!("{live}: {e}"))?;
    let esp_bytes = fat_volume_bytes(&mut source).map_err(|e| format!("{live}: {e}"))?;
    let mut disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&asked.target)
        .map_err(|e| format!("{}: {e}", asked.target))?;
    let size = disk
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("{}: {e}", asked.target))?;
    let sectors = size / SECTOR;
    let (_, last_usable) = usable(sectors, SECTOR as u32).map_err(|e| e.to_string())?;
    let esp_first = ALIGN;
    let esp_last = esp_first + esp_bytes.div_ceil(SECTOR).next_multiple_of(ALIGN) - 1;
    let root_first = esp_last + 1;
    let root_last = (last_usable + 1) / ALIGN * ALIGN - 1;
    if root_last < root_first || (root_last - root_first + 1) * SECTOR < ROOT_BYTES {
        return Err(format!(
            "{} is {} MiB; it needs {} MiB",
            asked.target,
            size >> 20,
            (root_first * SECTOR + ROOT_BYTES + WIPE) >> 20
        ));
    }
    println!(
        "ferrix-install: {} ({} MiB) will be erased: EFI system partition of {} MiB from {live}, \
         root of {} MiB",
        asked.target,
        size >> 20,
        ((esp_last - esp_first + 1) * SECTOR) >> 20,
        ((root_last - root_first + 1) * SECTOR) >> 20
    );
    if !asked.yes && !confirm()? {
        return Err("nothing was written".into());
    }

    let partitions = [
        Partition::new(
            ESP,
            random_guid()?,
            esp_first,
            esp_last,
            "EFI system partition",
        ),
        Partition::new(
            LINUX_FILESYSTEM,
            random_guid()?,
            root_first,
            root_last,
            "ferrix-root",
        ),
    ];
    let table = write_table(sectors, SECTOR as u32, random_guid()?, &partitions)
        .map_err(|e| e.to_string())?;

    step("wiping the old table", || {
        let zeros = vec![0_u8; WIPE as usize];
        write_at(&mut disk, 0, &zeros)?;
        write_at(&mut disk, size - WIPE, &zeros)
    })?;
    step("copying the EFI system partition", || {
        copy_esp(&mut source, &mut disk, esp_bytes, esp_first)
    })?;
    step("setting the installed system's command line", || {
        if installed_cmdline(&mut disk, esp_first * SECTOR)? {
            println!(
                "ferrix-install: FERRIX/CMDLINE.TXT on {} says {} instead of {}",
                asked.target,
                String::from_utf8_lossy(INSTALLED_ROOT),
                String::from_utf8_lossy(LIVE_ROOT)
            );
        }
        Ok(())
    })?;
    step("writing the root volume", || {
        write_root(&mut disk, root_first * SECTOR)
    })?;
    step("writing the partition table", || {
        table
            .iter()
            .try_for_each(|(lba, bytes)| write_at(&mut disk, lba * SECTOR, bytes))
    })?;
    step("flushing", || disk.sync_all())?;
    println!(
        "ferrix-install: done. Remove the live disk and start the machine from {}.",
        asked.target
    );
    Ok(())
}

/// Run a step, saying what it is and what went wrong.
fn step(what: &str, work: impl FnOnce() -> io::Result<()>) -> Result<(), String> {
    println!("ferrix-install: {what}");
    work().map_err(|e| format!("{what}: {e}"))
}

/// The live system's FAT volume: on each `vd` disk, the partition named
/// [`LIVE_NAME`] that holds `xtask`'s boot sector, or the whole disk when it
/// starts with that boot sector itself. Exactly one must be found.
fn find_live() -> Result<String, String> {
    let mut found = Vec::new();
    for letter in b'a'..=b'z' {
        let path = format!("/dev/vd{}", letter as char);
        let Ok(mut disk) = File::open(&path) else {
            continue;
        };
        if is_xtask_volume(&mut disk, 0) {
            found.push(path);
        } else if let Some(number) = live_partition(&mut disk) {
            found.push(format!("{path}{number}"));
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err("no live disk found; name it with --from".into()),
        _ => Err(format!(
            "more than one live system: {}; name one with --from",
            found.join(", ")
        )),
    }
}

/// Whether the sector at `lba` is the FAT32 boot sector `xtask` writes.
fn is_xtask_volume(disk: &mut File, lba: u64) -> bool {
    let mut sector = [0_u8; 512];
    disk.seek(SeekFrom::Start(lba * SECTOR)).is_ok()
        && disk.read_exact(&mut sector).is_ok()
        && &sector[3..11] == b"FERRIX  "
        && &sector[82..90] == b"FAT32   "
}

/// The number of `disk`'s partition named [`LIVE_NAME`], an EFI system
/// partition holding `xtask`'s FAT volume, as the kernel numbers it.
fn live_partition(disk: &mut File) -> Option<u32> {
    let sectors = disk.seek(SeekFrom::End(0)).ok()? / SECTOR;
    let mut sector = [0_u8; 512];
    disk.seek(SeekFrom::Start(SECTOR)).ok()?;
    disk.read_exact(&mut sector).ok()?;
    let header = parse_header(&sector, sectors).ok()?;
    let mut array = vec![0_u8; header.entries_bytes()];
    disk.seek(SeekFrom::Start(header.entries_at.checked_mul(SECTOR)?))
        .ok()?;
    disk.read_exact(&mut array).ok()?;
    let named = Partition::new(ESP, Guid([0; 16]), 0, 0, LIVE_NAME).name;
    let (index, partition) = parse_entries(&header, &array)
        .ok()?
        .into_iter()
        .find(|(_, partition)| partition.type_guid == ESP && partition.name == named)?;
    is_xtask_volume(disk, partition.first).then_some(index + 1)
}

/// Whether `target` is the whole disk `live`, a partition, is on: `vdd`
/// holds `vdd1`.
fn holds(target: &str, live: &str) -> bool {
    live.strip_prefix(target)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Rewrite `FERRIX/CMDLINE.TXT` in the FAT32 volume at byte `at` of `disk`
/// so that the installed system keeps `/` on its disk: [`LIVE_ROOT`] becomes
/// [`INSTALLED_ROOT`]. Whether there was one to rewrite: the MVP's image
/// carries no command line, and needs none.
///
/// A reader of `xtask`'s volumes only: 512-byte sectors and 8.3 names, which
/// is all `xtask`'s FAT writer makes. `docs/INSTALLER.md` §5.4's FAT library
/// takes its place.
fn installed_cmdline(disk: &mut File, at: u64) -> io::Result<bool> {
    let volume = Fat::read(disk, at)?;
    let Some(directory) = volume.find(disk, volume.root, b"FERRIX     ")? else {
        return Ok(false);
    };
    let Some(file) = volume.find(disk, directory.cluster, b"CMDLINE TXT")? else {
        return Ok(false);
    };
    let size = usize::try_from(file.size).map_err(io::Error::other)?;
    if size > volume.cluster_bytes() {
        return Err(io::Error::other("CMDLINE.TXT is larger than a cluster"));
    }
    let mut text = vec![0_u8; size];
    let offset = volume.cluster_offset(file.cluster)?;
    disk.seek(SeekFrom::Start(offset))?;
    disk.read_exact(&mut text)?;
    let Some(position) = text
        .windows(LIVE_ROOT.len())
        .position(|window| window == LIVE_ROOT)
    else {
        return Ok(false);
    };
    text[position..position + INSTALLED_ROOT.len()].copy_from_slice(INSTALLED_ROOT);
    write_at(disk, offset, &text)?;
    Ok(true)
}

/// A FAT32 volume's geometry, from its boot sector.
struct Fat {
    /// Where the volume starts on the disk, in bytes.
    at: u64,
    sectors_per_cluster: u64,
    /// Where the first allocation table starts, in bytes from `at`.
    table: u64,
    /// Where cluster 2 starts, in bytes from `at`.
    data: u64,
    /// The root directory's first cluster.
    root: u32,
}

/// A directory entry found: its first cluster and size.
struct Entry {
    cluster: u32,
    size: u32,
}

impl Fat {
    /// Clusters followed in one chain before it is taken for a loop.
    const LONGEST: usize = 1 << 16;

    fn read(disk: &mut File, at: u64) -> io::Result<Fat> {
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
        Ok(Fat {
            at,
            sectors_per_cluster,
            table: reserved * SECTOR,
            data: (reserved + tables * table_sectors) * SECTOR,
            root: u32_at(44),
        })
    }

    fn cluster_bytes(&self) -> usize {
        usize::try_from(self.sectors_per_cluster * SECTOR).unwrap_or(usize::MAX)
    }

    fn cluster_offset(&self, cluster: u32) -> io::Result<u64> {
        let index = u64::from(cluster)
            .checked_sub(2)
            .ok_or_else(|| io::Error::other("a cluster number below 2"))?;
        Ok(self.at + self.data + index * self.sectors_per_cluster * SECTOR)
    }

    /// The cluster after `cluster` in its chain, or `None` at its end.
    fn next(&self, disk: &mut File, cluster: u32) -> io::Result<Option<u32>> {
        let mut entry = [0_u8; 4];
        disk.seek(SeekFrom::Start(
            self.at + self.table + u64::from(cluster) * 4,
        ))?;
        disk.read_exact(&mut entry)?;
        let value = u32::from_le_bytes(entry) & 0x0FFF_FFFF;
        Ok((2..0x0FFF_FFF8).contains(&value).then_some(value))
    }

    /// The entry named `name` (8.3, space-padded) in the directory starting
    /// at `directory`, skipping the volume label and long-name entries.
    fn find(&self, disk: &mut File, directory: u32, name: &[u8; 11]) -> io::Result<Option<Entry>> {
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
}

/// Whether anything is mounted from `disk` or one of its partitions.
fn mounted(disk: &str) -> Result<bool, String> {
    let mounts =
        std::fs::read_to_string("/proc/mounts").map_err(|e| format!("/proc/mounts: {e}"))?;
    Ok(mounts.lines().any(|line| {
        line.split_whitespace().next().is_some_and(|source| {
            source
                .strip_prefix(disk)
                .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()))
        })
    }))
}

/// The FAT volume's size, from its boot sector.
fn fat_volume_bytes(disk: &mut File) -> io::Result<u64> {
    let mut sector = [0_u8; 512];
    disk.seek(SeekFrom::Start(0))?;
    disk.read_exact(&mut sector)?;
    let bytes_per_sector = u64::from(u16::from_le_bytes([sector[11], sector[12]]));
    let small = u64::from(u16::from_le_bytes([sector[19], sector[20]]));
    let large = u64::from(u32::from_le_bytes([
        sector[32], sector[33], sector[34], sector[35],
    ]));
    let count = if small != 0 { small } else { large };
    if bytes_per_sector != SECTOR || count == 0 {
        return Err(io::Error::other("not a FAT volume of 512-byte sectors"));
    }
    Ok(count * bytes_per_sector)
}

/// Copy the live FAT volume to the ESP, telling its boot sectors where it
/// now starts (the BPB's hidden sectors).
fn copy_esp(source: &mut File, disk: &mut File, bytes: u64, first: u64) -> io::Result<()> {
    let mut buffer = vec![0_u8; CHUNK];
    let mut done = 0;
    source.seek(SeekFrom::Start(0))?;
    while done < bytes {
        let len = usize::try_from((bytes - done).min(CHUNK as u64)).map_err(io::Error::other)?;
        let piece = &mut buffer[..len];
        source.read_exact(piece)?;
        if done == 0 {
            let hidden = u32::try_from(first)
                .map_err(io::Error::other)?
                .to_le_bytes();
            piece[28..32].copy_from_slice(&hidden);
            let backup = usize::from(u16::from_le_bytes([piece[50], piece[51]])) * 512;
            if backup != 0 && backup + 32 <= piece.len() {
                piece[backup + 28..backup + 32].copy_from_slice(&hidden);
            }
        }
        write_at(disk, first * SECTOR + done, piece)?;
        done += len as u64;
    }
    Ok(())
}

/// Write the packed root volume at `at`, zeroing its first and last MiB
/// first so nothing old is taken for part of it.
fn write_root(disk: &mut File, at: u64) -> io::Result<()> {
    let packed = std::fs::read(ROOT_PACKED)?;
    let zeros = vec![0_u8; WIPE as usize];
    write_at(disk, at, &zeros)?;
    write_at(disk, at + ROOT_BYTES - WIPE, &zeros)?;
    for record in packed.chunks_exact(8 + BLOCK) {
        let (offset, block) = record.split_at(8);
        let offset = u64::from_le_bytes(offset.try_into().map_err(io::Error::other)?);
        if offset + BLOCK as u64 > ROOT_BYTES {
            return Err(io::Error::other(
                "the packed root volume is larger than it says",
            ));
        }
        write_at(disk, at + offset, block)?;
    }
    Ok(())
}

fn write_at(disk: &mut File, at: u64, bytes: &[u8]) -> io::Result<()> {
    disk.seek(SeekFrom::Start(at))?;
    disk.write_all(bytes)
}

fn random_guid() -> Result<Guid, String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(Guid::random(bytes))
}

fn confirm() -> Result<bool, String> {
    print!("Erase it and install Ferrix? Type yes: ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(line.trim() == "yes")
}
