//! One install, as the command line and the installer's daemon both run it:
//! a target disk, the space on it, the person's answers, and a report of
//! each step as it goes (`docs/INSTALLER.md` §4.2, the live ISO's slice).
//!
//! Two kinds of space:
//!
//! * **Erase**: the whole disk, wiped at both ends and given a new GUID
//!   partition table of two partitions, the EFI system partition and the
//!   root -- what `ferrix-install /dev/vdY` has always done.
//! * **Beside**: a range of the disk's GUID partition table that holds no
//!   partition, or that one partition the person chose to give up held.
//!   The new EFI system partition and the root go there, in entries of the
//!   table that were empty, so every other partition keeps its number and
//!   its bytes: nothing outside the range and the table's own sectors is
//!   written (§4.4's rule), and the other partition's entry is cleared only
//!   when it is the one given up. A disk with a table of another kind is
//!   offered only for *Erase*.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use ferrix_install::disk;
use ferrix_partition::{ESP, LINUX_FILESYSTEM, Partition, crc32, usable, write_table};

use crate::setup::{self, Answers};
use crate::{
    ALIGN, CHUNK, INSTALLED_ROOT, LIVE_ROOT, SECTOR, WIPE, fat_in_place, installed_cmdline,
    random_guid, write_at, write_root,
};

/// The smallest root a desktop installs onto: the system unpacked with room
/// to use it.
pub const ROOT_MIN: u64 = 4 << 30;

/// Where on the target Ferrix goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    Erase,
    /// Sectors `first..=last` that no partition holds.
    Free {
        first: u64,
        last: u64,
    },
    /// The partition numbered `number` (1-based), whose contents are lost.
    Partition {
        number: u32,
    },
}

/// One install.
pub struct Plan {
    pub target: String,
    pub space: Space,
    /// The person's answers; none for the command line's bare install,
    /// whose installed system is the image's as it was.
    pub answers: Option<Answers>,
}

/// What an install says as it goes.
pub enum Event {
    Step(usize, usize, String),
    Progress(u8),
    Log(String),
}

/// The bytes a target needs: the live volume's ESP and the smallest root,
/// on MiB boundaries, and a MiB of table at each end.
pub fn needed(esp_bytes: u64) -> u64 {
    esp_bytes.next_multiple_of(ALIGN * SECTOR) + ROOT_MIN + 2 * (ALIGN * SECTOR)
}

/// Run `plan`, copying the live system from `live` (a disk or a partition
/// holding its FAT volume), saying each step to `report`.
pub fn install(plan: &Plan, live: &str, report: &mut dyn FnMut(Event)) -> Result<(), String> {
    if let Some(answers) = &plan.answers {
        answers.check()?;
    }
    let mut source = File::open(live).map_err(|e| format!("{live}: {e}"))?;
    let esp_bytes = crate::fat_volume_bytes(&mut source).map_err(|e| format!("{live}: {e}"))?;
    let mut disk = disk::open_rw(&plan.target).map_err(|e| format!("{}: {e}", plan.target))?;
    let size = disk::size(&mut disk).map_err(|e| format!("{}: {e}", plan.target))?;
    let sectors = size / SECTOR;
    let esp_sectors = esp_bytes.div_ceil(SECTOR).next_multiple_of(ALIGN);

    // Where the two partitions go, and the table they go into.
    let (first, last, table) = match plan.space {
        Space::Erase => {
            let (_, last_usable) = usable(sectors, SECTOR as u32).map_err(|e| e.to_string())?;
            (ALIGN, (last_usable + 1) / ALIGN * ALIGN - 1, None)
        }
        Space::Free { first, last } => {
            let table = existing(&mut disk, &plan.target)?;
            check_free(&table, first, last)?;
            (first, last, Some(table))
        }
        Space::Partition { number } => {
            let mut table = existing(&mut disk, &plan.target)?;
            let at = table
                .entries
                .iter()
                .position(|(index, _)| index + 1 == number)
                .ok_or_else(|| format!("{} has no partition {number}", plan.target))?;
            let (_, given_up) = table.entries.remove(at);
            (given_up.first, given_up.last, Some(table))
        }
    };
    let esp_first = first.next_multiple_of(ALIGN);
    let esp_last = esp_first + esp_sectors - 1;
    let root_first = esp_last + 1;
    let root_last = (last + 1) / ALIGN * ALIGN - 1;
    // The command line's bare install, a test image's, keeps its 1 GiB
    // floor; a person's desktop needs room.
    let root_min = if plan.answers.is_some() {
        ROOT_MIN
    } else {
        crate::ROOT_BYTES
    };
    if root_last <= root_first || (root_last - root_first + 1) * SECTOR < root_min {
        return Err(format!(
            "the space chosen on {} is {} MiB; Ferrix needs {} MiB",
            plan.target,
            ((last + 1).saturating_sub(first) * SECTOR) >> 20,
            needed(esp_bytes) >> 20
        ));
    }
    let esp = Partition::new(
        ESP,
        random_guid()?,
        esp_first,
        esp_last,
        "EFI system partition",
    );
    let root = Partition::new(
        LINUX_FILESYSTEM,
        random_guid()?,
        root_first,
        root_last,
        "ferrix-root",
    );
    let total = 7;
    let mut step = 0;
    let mut next = |report: &mut dyn FnMut(Event), what: &str| {
        step += 1;
        report(Event::Step(step, total, what.to_owned()));
        report(Event::Log(format!("ferrix-install: {what}")));
    };

    next(report, "Checking the disk");
    if crate::mounted(&plan.target)? {
        return Err(format!("{} has something mounted from it", plan.target));
    }
    let hash = match &plan.answers {
        Some(answers) => {
            report(Event::Log("ferrix-install: hashing the password".into()));
            Some(setup::hash(&answers.password)?)
        }
        None => None,
    };
    report(Event::Progress(2));

    next(report, "Preparing the disk");
    if table.is_none() {
        let zeros = vec![0_u8; WIPE as usize];
        write_at(&mut disk, 0, &zeros).map_err(|e| format!("wiping: {e}"))?;
        write_at(&mut disk, size - WIPE, &zeros).map_err(|e| format!("wiping: {e}"))?;
    }
    report(Event::Progress(4));

    next(report, "Copying the system");
    copy(&mut source, &mut disk, esp_bytes, esp_first, report)
        .map_err(|e| format!("copying the system: {e}"))?;

    next(report, "Setting up the boot files");
    if installed_cmdline(&mut disk, esp_first * SECTOR).map_err(|e| e.to_string())? {
        report(Event::Log(format!(
            "ferrix-install: FERRIX/CMDLINE.TXT says {} instead of {}",
            String::from_utf8_lossy(INSTALLED_ROOT),
            String::from_utf8_lossy(LIVE_ROOT)
        )));
    }
    if let (Some(answers), Some(hash)) = (&plan.answers, &hash) {
        let bytes = answers.file(hash)?;
        let there = fat_in_place::rewrite(&mut disk, esp_first * SECTOR, setup::SETUP, &bytes)
            .map_err(|e| format!("FERRIX/SETUP.TXT: {e}"))?;
        if !there {
            return Err("the live system carries no FERRIX/SETUP.TXT to fill in".into());
        }
        report(Event::Log(format!(
            "ferrix-install: the answers are in FERRIX/SETUP.TXT, for {}",
            answers.user
        )));
    }
    report(Event::Progress(82));

    next(report, "Making the root file system");
    write_root(
        &mut disk,
        root_first * SECTOR,
        (root_last - root_first + 1) * SECTOR,
    )
    .map_err(|e| format!("making the root volume: {e}"))?;
    report(Event::Progress(90));

    next(report, "Writing the partition table");
    let sectors_written = match table {
        None => write_table(sectors, SECTOR as u32, random_guid()?, &[esp, root])
            .map_err(|e| e.to_string())?,
        Some(table) => patched(&table, &[esp, root])?,
    };
    for (lba, bytes) in sectors_written {
        write_at(&mut disk, lba * SECTOR, &bytes).map_err(|e| format!("the table: {e}"))?;
    }
    report(Event::Progress(96));

    next(report, "Finishing");
    disk.sync_all().map_err(|e| format!("flushing: {e}"))?;
    report(Event::Progress(100));
    report(Event::Log(format!(
        "ferrix-install: done. Remove the live disk and start the machine from {}.",
        plan.target
    )));
    Ok(())
}

/// The target's GUID partition table, which a *Beside* install needs.
fn existing(disk: &mut File, target: &str) -> Result<disk::Table, String> {
    disk::table(disk)
        .map_err(|e| format!("{target}: {e}"))?
        .ok_or_else(|| format!("{target} has no GUID partition table; only erasing it is offered"))
}

/// Refuse a range that is not free space of `table`'s.
fn check_free(table: &disk::Table, first: u64, last: u64) -> Result<(), String> {
    if first > last || first < table.header.first_usable || last > table.header.last_usable {
        return Err("the space chosen is not inside the disk's usable sectors".into());
    }
    if let Some((index, _)) = table
        .entries
        .iter()
        .find(|(_, partition)| partition.first <= last && first <= partition.last)
    {
        return Err(format!("the space chosen overlaps partition {}", index + 1));
    }
    Ok(())
}

/// The ranges of `table` that no partition holds, on MiB boundaries, at
/// least a MiB each.
pub fn free_ranges(table: &disk::Table) -> Vec<(u64, u64)> {
    let mut used: Vec<(u64, u64)> = table
        .entries
        .iter()
        .map(|(_, partition)| (partition.first, partition.last))
        .collect();
    used.sort_unstable();
    let mut ranges = Vec::new();
    let mut at = table.header.first_usable;
    for (first, last) in used.into_iter().chain([(table.header.last_usable + 1, 0)]) {
        let start = at.next_multiple_of(ALIGN);
        if first > start {
            let end = first - 1;
            if end + 1 - start >= ALIGN {
                ranges.push((start, end));
            }
        }
        at = at.max(last + 1);
    }
    ranges
}

/// `table` with `added` in its first empty entries, every other entry as it
/// was read: the sectors to write, both headers and both entry arrays.
fn patched(table: &disk::Table, added: &[Partition]) -> Result<Vec<(u64, Vec<u8>)>, String> {
    let header = &table.header;
    let size = header.entry_size as usize;
    let mut array = table.raw_array.clone();
    // The entry given up, if any, is the one the table read had and
    // `entries` no longer lists: cleared.
    for (index, chunk) in array.chunks_exact_mut(size).enumerate() {
        let used = chunk
            .get(..16)
            .is_some_and(|guid| guid.iter().any(|&b| b != 0));
        let kept = table.entries.iter().any(|(at, _)| *at as usize == index);
        if used && !kept {
            chunk.fill(0);
        }
    }
    let mut free = array
        .chunks_exact(size)
        .enumerate()
        .filter(|(_, chunk)| chunk.iter().take(16).all(|&b| b == 0))
        .map(|(index, _)| index)
        .collect::<Vec<_>>()
        .into_iter();
    for partition in added {
        let index = free
            .next()
            .ok_or("the disk's partition table has no empty entry left")?;
        let entry = &mut array[index * size..(index + 1) * size];
        entry.fill(0);
        entry[..16].copy_from_slice(&partition.type_guid.0);
        entry[16..32].copy_from_slice(&partition.unique.0);
        entry[32..40].copy_from_slice(&partition.first.to_le_bytes());
        entry[40..48].copy_from_slice(&partition.last.to_le_bytes());
        for (at, unit) in partition.name.iter().enumerate() {
            entry[56 + 2 * at..58 + 2 * at].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let entries_crc = crc32(&array);
    let last = table.sectors - 1;
    let array_sectors = (array.len() as u64).div_ceil(SECTOR);
    let mut primary = table.raw_header.clone();
    let header_size = u32::from_le_bytes([primary[12], primary[13], primary[14], primary[15]]);
    let header_size = usize::try_from(header_size).map_err(|e| e.to_string())?;
    let finish = |sector: &mut Vec<u8>, mine: u64, alternate: u64, entries_at: u64| {
        sector[24..32].copy_from_slice(&mine.to_le_bytes());
        sector[32..40].copy_from_slice(&alternate.to_le_bytes());
        sector[72..80].copy_from_slice(&entries_at.to_le_bytes());
        sector[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        sector[16..20].fill(0);
        let crc = crc32(&sector[..header_size]);
        sector[16..20].copy_from_slice(&crc.to_le_bytes());
    };
    let mut backup = primary.clone();
    finish(&mut primary, 1, last, header.entries_at);
    finish(&mut backup, last, 1, last - array_sectors);
    let mut padded = array;
    padded.resize(
        usize::try_from(array_sectors * SECTOR).map_err(|e| e.to_string())?,
        0,
    );
    Ok(vec![
        (1, primary),
        (header.entries_at, padded.clone()),
        (last - array_sectors, padded),
        (last, backup),
    ])
}

/// Copy the live FAT volume onto the target's ESP at sector `first`, with
/// its boot sectors told where it now starts, reporting progress from 4 to
/// 80.
fn copy(
    source: &mut File,
    disk: &mut File,
    bytes: u64,
    first: u64,
    report: &mut dyn FnMut(Event),
) -> io::Result<()> {
    let mut buffer = vec![0_u8; CHUNK];
    let mut done = 0;
    let mut said = 0;
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
        let percent = 4 + u8::try_from(done * 76 / bytes).unwrap_or(76);
        if percent != said {
            said = percent;
            report(Event::Progress(percent));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{free_ranges, patched};
    use ferrix_install::disk::Table;
    use ferrix_partition::{
        ESP, Guid, LINUX_FILESYSTEM, Partition, parse_entries, parse_header, write_table,
    };

    /// A 64 GiB disk's table with partitions 1 and 3 used and 2 empty.
    fn table() -> Table {
        let sectors = 64_u64 << 21;
        let parts = [
            Partition::new(ESP, Guid([1; 16]), 2048, 206_847, "EFI"),
            Partition::new(LINUX_FILESYSTEM, Guid([2; 16]), 206_848, 20_000_000, "gone"),
            Partition::new(
                LINUX_FILESYSTEM,
                Guid([3; 16]),
                40_000_000,
                60_000_000,
                "other",
            ),
        ];
        let written = write_table(sectors, 512, Guid([9; 16]), &parts).expect("a table");
        let mut array = written[2].1.clone();
        // Entry 2 (index 1) emptied, so the third keeps its number 3.
        array[128..256].fill(0);
        let crc = ferrix_partition::crc32(&array[..128 * 128]);
        let mut header = written[1].1.clone();
        header[88..92].copy_from_slice(&crc.to_le_bytes());
        header[16..20].fill(0);
        let hcrc = ferrix_partition::crc32(&header[..92]);
        header[16..20].copy_from_slice(&hcrc.to_le_bytes());
        let parsed = parse_header(&header, sectors).expect("header");
        let entries = parse_entries(&parsed, &array).expect("entries");
        Table {
            header: parsed,
            entries,
            raw_header: header,
            raw_array: array,
            sectors,
        }
    }

    #[test]
    fn free_space_is_every_gap_on_mib_boundaries() {
        let table = table();
        let ranges = free_ranges(&table);
        assert_eq!(ranges[0], (206_848, 39_999_999));
        assert_eq!(ranges[1].0, 60_000_256);
        assert!(ranges.iter().all(|(first, _)| first % 2048 == 0));
    }

    #[test]
    fn new_partitions_take_empty_entries_and_others_keep_their_numbers() {
        let table = table();
        let added = [
            Partition::new(
                ESP,
                Guid([4; 16]),
                206_848,
                1_255_423,
                "EFI system partition",
            ),
            Partition::new(
                LINUX_FILESYSTEM,
                Guid([5; 16]),
                1_255_424,
                39_999_487,
                "ferrix-root",
            ),
        ];
        let sectors = patched(&table, &added).expect("patched");
        let header = &sectors[0].1;
        let parsed = parse_header(header, table.sectors).expect("a valid header");
        let entries = parse_entries(&parsed, &sectors[1].1).expect("valid entries");
        let numbers: Vec<(u32, String)> = entries
            .iter()
            .map(|(index, p)| (index + 1, ferrix_install::disk::name(p)))
            .collect();
        assert_eq!(
            numbers,
            vec![
                (1, "EFI".to_owned()),
                (2, "EFI system partition".to_owned()),
                (3, "other".to_owned()),
                (4, "ferrix-root".to_owned()),
            ]
        );
        // The backup header names the backup array and is valid too.
        let backup = &sectors[3].1;
        let mut covered = backup[..92].to_vec();
        covered[16..20].fill(0);
        let crc = u32::from_le_bytes(backup[16..20].try_into().expect("four"));
        assert_eq!(ferrix_partition::crc32(&covered), crc);
        assert_eq!(
            u64::from_le_bytes(backup[24..32].try_into().expect("8")),
            sectors[3].0
        );
        assert_eq!(
            u64::from_le_bytes(backup[72..80].try_into().expect("8")),
            sectors[2].0
        );
        assert_eq!(backup[88..92], header[88..92]);
    }

    #[test]
    fn a_partition_given_up_is_cleared_from_the_table() {
        let mut table = table();
        let at = table.entries.iter().position(|(i, _)| *i == 2).expect("3");
        let _ = table.entries.remove(at);
        let sectors = patched(&table, &[]).expect("patched");
        let parsed = parse_header(&sectors[0].1, table.sectors).expect("header");
        let entries = parse_entries(&parsed, &sectors[1].1).expect("entries");
        assert_eq!(entries.len(), 1);
    }
}
