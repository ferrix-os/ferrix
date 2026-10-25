//! `ferrix-install`: put the live system on a disk of its own.
//!
//! The minimum `docs/INSTALLER.md` §11 calls the MVP. The target disk is
//! wiped and given a GUID partition table with two partitions:
//!
//! 1. an EFI system partition holding a byte copy of the live disk's FAT
//!    volume -- the loader, the kernel and the initramfs the machine booted;
//! 2. a root partition, the rest of the disk, holding an empty btrfs volume
//!    labelled `ferrix-root` made to its size (`ferrix-btrfs-mkfs`).
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
//! (OEM name `FERRIX`), the image of `build --installer`, taken only when no
//! disk has a live partition. Either way it must be the only one. A target that is the live disk, or holds the live
//! partition, is refused. Without `--yes` it asks before it erases anything.

use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::ExitCode;

use ferrix_install::fat::Fat;

mod engine;
mod fat_in_place;
mod serve;
mod setup;
mod slots;
use ferrix_partition::{
    ESP, Guid, Partition, parse_entries, parse_header,
};

/// The live medium's EFI system partition's name (`tools/common/xtask/src/installer.rs`).
pub(crate) const LIVE_NAME: &str = "FERRIX-LIVE";
/// The live session's root option, and what the installed system's says in
/// its place: the same length, so the file is rewritten in place.
const LIVE_ROOT: &[u8] = b"ferrix.root=tmpfs";
const INSTALLED_ROOT: &[u8] = b"ferrix.root=btrfs";
/// The live session's own word, which starts the installer's daemon and
/// its window; blanked on the installed disk, the same length.
const LIVE_WORD: &[u8] = b"ferrix.live";

/// The root volume's label, which is how the kernel finds it.
const ROOT_LABEL: &[u8] = b"ferrix-root";
/// The smallest root partition: the system unpacked, with room to work.
const ROOT_BYTES: u64 = 1 << 30;
/// Bytes in a sector: virtio's unit.
pub(crate) const SECTOR: u64 = 512;
/// Partitions start on MiB boundaries, as every tool puts them.
pub(crate) const ALIGN: u64 = 2048;
/// What is wiped at each end of the disk, so no old table or superblock
/// survives outside the new partitions.
pub(crate) const WIPE: u64 = 1 << 20;
/// Bytes copied at a time.
pub(crate) const CHUNK: usize = 1 << 20;

fn main() -> ExitCode {
    let first = std::env::args().nth(1);
    let result = match first.as_deref() {
        // The live session's daemon, and the installed system's first-boot
        // setup (`serve.rs`, `setup.rs`).
        Some("--serve") => serve::serve(),
        Some("--apply") => setup::apply(),
        _ => run(),
    };
    match result {
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
    /// `--answers FILE`: the person's answers, `key=value` lines as the
    /// daemon takes them, for the installed system's first boot.
    answers: Option<String>,
    from: Option<String>,
    target: String,
    /// `--slots`: the A/B layout (`slots.rs`), with its sizes.
    slots: Option<slots::Sizes>,
}

fn parse() -> Result<Asked, String> {
    let mut yes = false;
    let mut answers = None;
    let mut from = None;
    let mut target = None;
    let mut slots: Option<slots::Sizes> = None;
    let mut args = std::env::args().skip(1);
    let mib = |args: &mut dyn Iterator<Item = String>, what: &str| -> Result<u64, String> {
        args.next()
            .and_then(|n| n.parse().ok())
            .ok_or(format!("{what} needs a size in MiB"))
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--slots" => slots = Some(slots.unwrap_or_default()),
            "--swap-mib" => slots.get_or_insert_default().swap_mib = mib(&mut args, &arg)?,
            "--esp-mib" => slots.get_or_insert_default().esp_mib = mib(&mut args, &arg)?,
            "--slot-mib" => slots.get_or_insert_default().slot_mib = mib(&mut args, &arg)?,
            "--yes" | "-y" => yes = true,
            "--answers" => answers = Some(args.next().ok_or("--answers needs a file")?),
            "--from" => from = Some(args.next().ok_or("--from needs a disk")?),
            "--help" | "-h" => {
                return Err(
                    "usage: ferrix-install [--yes] [--from /dev/vdX] [--slots [--swap-mib N] \
                            [--esp-mib N] [--slot-mib N]] /dev/vdY"
                        .into(),
                );
            }
            _ if target.is_none() => target = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    let target = target.ok_or("name the disk to install on, as /dev/vdY")?;
    Ok(Asked {
        yes,
        answers,
        from,
        target,
        slots,
    })
}

fn run() -> Result<(), String> {
    let asked = parse()?;
    let live = match asked.from.clone() {
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
    if let Some(sizes) = asked.slots {
        return slots::install(&asked, &sizes, &live, &mut source, esp_bytes);
    }
    drop(source);
    let answers = match &asked.answers {
        Some(path) => Some(answers_file(path)?),
        None => None,
    };
    let plan = engine::Plan {
        target: asked.target.clone(),
        space: engine::Space::Erase,
        answers,
    };
    println!(
        "ferrix-install: {} ({} MiB) will be erased; the system comes from {live}",
        asked.target,
        File::open(&asked.target)
            .and_then(|mut disk| disk.seek(SeekFrom::End(0)))
            .map_or(0, |size| size >> 20)
    );
    if !asked.yes && !confirm()? {
        return Err("nothing was written".into());
    }
    engine::install(&plan, &live, &mut |event| match event {
        engine::Event::Log(text) => println!("{text}"),
        engine::Event::Step(..) | engine::Event::Progress(_) => {}
    })
}

/// An answers file: the `key=value` lines the daemon takes.
fn answers_file(path: &str) -> Result<setup::Answers, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut answers = setup::Answers::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let slot = match key.trim() {
            "user" => &mut answers.user,
            "fullname" => &mut answers.fullname,
            "password" => &mut answers.password,
            "hostname" => &mut answers.hostname,
            "keyboard" => &mut answers.keyboard,
            "variant" => &mut answers.variant,
            "timezone" => &mut answers.timezone,
            "language" => &mut answers.language,
            _ => continue,
        };
        value.clone_into(slot);
    }
    answers.check()?;
    Ok(answers)
}

/// The disk the live system is on: the whole disk of [`find_live`]'s
/// partition, or the disk itself for a bare image.
pub(crate) fn live_disk() -> Option<String> {
    let live = find_live().ok()?;
    let disk = live.trim_end_matches(|c: char| c.is_ascii_digit());
    Some(if Path::new(disk).exists() { disk.to_owned() } else { live })
}

/// Run a step, saying what it is and what went wrong.
fn step(what: &str, work: impl FnOnce() -> io::Result<()>) -> Result<(), String> {
    println!("ferrix-install: {what}");
    work().map_err(|e| format!("{what}: {e}"))
}

/// The live system's FAT volume: on each `vd` disk, the partition named
/// [`LIVE_NAME`] that holds `xtask`'s boot sector; or, when no disk has
/// one, a whole disk that starts with that boot sector itself (the MVP's
/// image). Exactly one must be found.
///
/// A live medium's partition comes first because a bare image is also what
/// a machine may have booted from without being a medium to install: since
/// the AHCI driver serves q35's SATA disk, `test-install`'s scripted boot
/// image is one beside the medium it installs from.
pub(crate) fn find_live() -> Result<String, String> {
    let mut partitions = Vec::new();
    let mut whole = Vec::new();
    for letter in b'a'..=b'z' {
        let path = format!("/dev/vd{}", letter as char);
        let Ok(mut disk) = File::open(&path) else {
            continue;
        };
        if is_xtask_volume(&mut disk, 0) {
            whole.push(path);
        } else if let Some(number) = live_partition(&mut disk) {
            partitions.push(format!("{path}{number}"));
        }
    }
    let found = if partitions.is_empty() {
        whole
    } else {
        if !whole.is_empty() {
            println!(
                "ferrix-install: {} not taken for the live system: a live medium's partition is",
                whole.join(", ")
            );
        }
        partitions
    };
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
pub(crate) fn installed_cmdline(disk: &mut File, at: u64) -> io::Result<bool> {
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
    if let Some(at) = text
        .windows(LIVE_WORD.len())
        .position(|window| window == LIVE_WORD)
    {
        text[at..at + LIVE_WORD.len()].fill(b' ');
    }
    write_at(disk, offset, &text)?;
    Ok(true)
}

/// Whether anything is mounted from `disk` or one of its partitions.
pub(crate) fn mounted(disk: &str) -> Result<bool, String> {
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
pub(crate) fn fat_volume_bytes(disk: &mut File) -> io::Result<u64> {
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

/// Make an empty btrfs volume labelled [`ROOT_LABEL`] of `bytes` at byte
/// `at` of `disk`: the whole root partition (`ferrix-btrfs-mkfs`). The
/// kernel unpacks the system onto it at the installed disk's first boot.
pub(crate) fn write_root(disk: &mut File, at: u64, bytes: u64) -> io::Result<()> {
    let mut random = [[0_u8; 16]; 4];
    let mut urandom = File::open("/dev/urandom")?;
    for uuid in &mut random {
        urandom.read_exact(uuid)?;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let options = ferrix_btrfs_mkfs::Options::new(bytes, ROOT_LABEL, random, now);
    let image = ferrix_btrfs_mkfs::make(&options).map_err(|e| io::Error::other(e.to_string()))?;
    println!(
        "ferrix-install: a btrfs volume of {} MiB labelled {}",
        image.size >> 20,
        String::from_utf8_lossy(ROOT_LABEL)
    );
    image.write_to(&mut Volume { disk, at })
}

/// A volume at byte `at` of a disk, for `ferrix-btrfs-mkfs` to write.
struct Volume<'a> {
    disk: &'a mut File,
    at: u64,
}

impl ferrix_btrfs_mkfs::Target for Volume<'_> {
    type Error = io::Error;

    fn write_at(&mut self, at: u64, data: &[u8]) -> io::Result<()> {
        write_at(self.disk, self.at + at, data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.disk.sync_data()
    }
}

pub(crate) fn write_at(disk: &mut File, at: u64, bytes: &[u8]) -> io::Result<()> {
    disk.seek(SeekFrom::Start(at))?;
    disk.write_all(bytes)
}

pub(crate) fn random_guid() -> Result<Guid, String> {
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
