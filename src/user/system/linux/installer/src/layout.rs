//! The A/B layout (`docs/DEV-ON-FERRIX.md` §4.4, D8): which partitions of a
//! disk are Ferrix's EFI system partition, control record, two system slots
//! and root, and the checks that keep every other partition unwritten.
//!
//! A partition is writable here only by its GPT *type*: Ferrix's slot type
//! or its control-record type ([`SLOT_TYPE`], [`CONTROL_TYPE`]); any other
//! type is refused whatever it is named, so a host's live swap partition
//! beside Ferrix's can never be written (the certification consultant's
//! ruling, 2026-10-11). The names the customer's disk was given on
//! 2026-10-11 (`ferrix-esp`, `ferrix-bootctl`, `ferrix-slot-a`,
//! `ferrix-slot-b`, `ferrix-root`) tell slot A from slot B. On top of the
//! type, [`check_writable`] looks at what a partition holds and refuses one
//! holding swap, btrfs, NTFS, ext4 or a partition table.

use std::fs::File;
use std::io;

use ferrix_partition::{ESP, Guid, LINUX_FILESYSTEM, Partition};

use crate::disk::{self, SECTOR, Table};

/// Ferrix's own partition types, proposed 2026-10-11 (the dev-on-ferrix
/// handover). The customer's disk carries Linux's data type on its slots
/// until these are written there (the product owner's write phase).
pub const SLOT_TYPE: Guid =
    Guid::from_fields(0x5475_b681, 0x82f5, 0x47da, 0xa5a1, 0x55db_0e7e_2bd3);
pub const CONTROL_TYPE: Guid =
    Guid::from_fields(0xc29a_a8c5, 0xc19c, 0x4147, 0x838f, 0x01ac_cca2_8be4);
/// Linux swap, which a disk shared with a Linux host may hold beside Ferrix.
pub const LINUX_SWAP: Guid =
    Guid::from_fields(0x0657_fd6d, 0xa4ab, 0x43c4, 0x84e5, 0x0933_c84b_4f4f);

pub const ESP_NAME: &str = "ferrix-esp";
pub const CONTROL_NAME: &str = "ferrix-bootctl";
pub const SLOT_NAMES: [&str; 2] = ["ferrix-slot-a", "ferrix-slot-b"];
pub const ROOT_NAME: &str = "ferrix-root";

/// The control record's partition holds two copies, at 0 and at 64 KiB:
/// it must be at least 128 KiB (`docs/AB-BOOT.md` §3).
pub const CONTROL_BYTES: u64 = 128 << 10;

pub use ferrix_slots::Slot;

/// One partition of the layout: its number (1-based, as its node is
/// named), its entry, and its node.
#[derive(Debug, Clone)]
pub struct Part {
    pub number: u32,
    pub entry: Partition,
    pub node: String,
}

impl Part {
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.entry.sectors() * SECTOR
    }
}

/// A disk with the A/B layout.
#[derive(Debug, Clone)]
pub struct AbDisk {
    pub disk: String,
    pub esp: Option<Part>,
    pub control: Part,
    pub slots: [Part; 2],
    pub root: Option<Part>,
}

impl AbDisk {
    #[must_use]
    pub fn slot(&self, slot: Slot) -> &Part {
        &self.slots[usize::from(slot.code())]
    }
}

/// The A/B layout on `disk`, whose table is `table` (`docs/AB-BOOT.md` §2,
/// §3): exactly one partition of [`CONTROL_TYPE`] and exactly two of
/// [`SLOT_TYPE`], A the one with the lower entry index. `Ok(None)` when it
/// has neither type and no partition named [`CONTROL_NAME`]; an error when
/// the layout is half there -- including the customer's disk as partitioned
/// on 2026-10-11, whose slots are named but of Linux's data type.
///
/// # Errors
///
/// What is wrong with the layout.
pub fn find(disk: &str, table: &Table) -> Result<Option<AbDisk>, String> {
    let part = |index: u32, entry: &Partition| Part {
        number: index + 1,
        entry: *entry,
        node: disk::partition_node(disk, index + 1),
    };
    let of_type = |kind: Guid| -> Vec<Part> {
        let mut found: Vec<Part> = table
            .entries
            .iter()
            .filter(|(_, entry)| entry.type_guid == kind)
            .map(|(index, entry)| part(*index, entry))
            .collect();
        found.sort_by_key(|p| p.number);
        found
    };
    let named = |wanted: &str| {
        table
            .entries
            .iter()
            .find(|(_, entry)| disk::name(entry) == wanted)
            .map(|(index, entry)| part(*index, entry))
    };
    let controls = of_type(CONTROL_TYPE);
    let slots = of_type(SLOT_TYPE);
    if controls.is_empty() && slots.is_empty() {
        return match named(CONTROL_NAME) {
            None => Ok(None),
            Some(control) => Err(format!(
                "{} is named {CONTROL_NAME} but its type is {}, not Ferrix's boot control \
                 type {CONTROL_TYPE}; bootctl writes only partitions of Ferrix's two types",
                control.node, control.entry.type_guid
            )),
        };
    }
    let [control] = controls.as_slice() else {
        return Err(format!(
            "{disk} has {} partitions of Ferrix's boot control type; it needs one",
            controls.len()
        ));
    };
    let [a, b] = slots.as_slice() else {
        return Err(format!(
            "{disk} has {} partitions of Ferrix's system slot type; it needs two",
            slots.len()
        ));
    };
    if control.bytes() < CONTROL_BYTES {
        return Err(format!(
            "{} is {} bytes; the control record needs {CONTROL_BYTES}",
            control.node,
            control.bytes()
        ));
    }
    let esp = named(ESP_NAME)
        .or_else(|| of_type(ESP).into_iter().next())
        .filter(|esp| esp.entry.type_guid == ESP);
    let root = named(ROOT_NAME).filter(|root| root.entry.type_guid == LINUX_FILESYSTEM);
    Ok(Some(AbDisk {
        disk: disk.to_owned(),
        esp,
        control: control.clone(),
        slots: [a.clone(), b.clone()],
        root,
    }))
}

/// Find the one disk with the A/B layout, or the one named.
///
/// # Errors
///
/// None found, more than one, or a layout that is wrong.
pub fn locate(named: Option<&str>) -> Result<AbDisk, String> {
    let candidates = match named {
        Some(disk) => vec![disk.to_owned()],
        None => disk::whole_disks(),
    };
    let mut found = Vec::new();
    for path in candidates {
        let Ok(mut file) = disk::open(&path) else {
            continue;
        };
        let table = disk::table(&mut file).map_err(|e| format!("{path}: {e}"))?;
        if let Some(table) = table
            && let Some(ab) = find(&path, &table)?
        {
            found.push(ab);
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(match named {
            Some(disk) => format!("{disk} has no partition named {CONTROL_NAME}"),
            None => format!("no disk has a partition named {CONTROL_NAME}; name one with --disk"),
        }),
        _ => Err(format!(
            "more than one disk has the A/B layout: {}; name one with --disk",
            found
                .iter()
                .map(|ab| ab.disk.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// What a partition's first 68 KiB say it holds, by the file systems'
/// magic numbers; `None` for a FAT volume or nothing recognised.
#[must_use]
pub fn foreign(head: &[u8]) -> Option<&'static str> {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    if at(4086, b"SWAPSPACE2") || at(4086, b"SWAP-SPACE") {
        Some("swap")
    } else if at(0x10040, b"_BHRfS_M") {
        Some("btrfs")
    } else if at(3, b"NTFS    ") {
        Some("NTFS")
    } else if at(1080, &[0x53, 0xEF]) {
        Some("ext2/3/4")
    } else if at(512, b"EFI PART") {
        Some("a partition table")
    } else {
        None
    }
}

/// Bytes of a partition's head [`foreign`] reads.
pub const HEAD_BYTES: usize = 0x10040 + 8;

/// Refuse to write `part` as a system slot unless it holds nothing another
/// system would miss: [`foreign`] finds nothing in its head.
///
/// # Errors
///
/// What it holds, or a read error.
pub fn check_writable(part: &Part, file: &mut File) -> Result<(), String> {
    if part.entry.type_guid != SLOT_TYPE && part.entry.type_guid != CONTROL_TYPE {
        return Err(format!(
            "{} is of type {}, not a Ferrix slot or control record",
            part.node, part.entry.type_guid
        ));
    }
    let mut head = vec![0_u8; HEAD_BYTES];
    disk::read_at(file, 0, &mut head).map_err(|e: io::Error| format!("{}: {e}", part.node))?;
    match foreign(&head) {
        Some(what) => Err(format!(
            "{} holds {what}; a slot is written only when it is empty or holds a Ferrix image",
            part.node
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrix_partition::{parse_entries, parse_header, write_table};

    /// The customer's layout, in MiB, scaled down.
    fn table(slot_type: Guid) -> Table {
        let mib = 2048;
        let control_type = if slot_type == SLOT_TYPE {
            CONTROL_TYPE
        } else {
            slot_type
        };
        let parts = [
            (LINUX_SWAP, 1, 8, "nazuna-swap"),
            (ESP, 9, 8, ESP_NAME),
            (control_type, 17, 1, CONTROL_NAME),
            (slot_type, 18, 4, SLOT_NAMES[0]),
            (slot_type, 22, 4, SLOT_NAMES[1]),
            (LINUX_FILESYSTEM, 26, 8, ROOT_NAME),
        ]
        .map(|(kind, at, len, name)| {
            Partition::new(
                kind,
                Guid([at as u8; 16]),
                at * mib,
                (at + len) * mib - 1,
                name,
            )
        });
        let sectors = 36 * mib;
        let written = write_table(sectors, 512, Guid([9; 16]), &parts).unwrap();
        let header = parse_header(&written[1].1, sectors).unwrap();
        let entries = parse_entries(&header, &written[2].1).unwrap();
        Table { header, entries }
    }

    #[test]
    fn finds_the_layout_by_name_and_type() {
        let ab = find("/dev/vdd", &table(SLOT_TYPE)).unwrap().unwrap();
        assert_eq!(ab.control.node, "/dev/vdd3");
        assert_eq!(ab.slot(Slot::A).node, "/dev/vdd4");
        assert_eq!(ab.slot(Slot::B).node, "/dev/vdd5");
        assert_eq!(ab.root.unwrap().node, "/dev/vdd6");
        let nvme = find("/dev/nvme0n1", &table(SLOT_TYPE)).unwrap().unwrap();
        assert_eq!(nvme.slot(Slot::B).node, "/dev/nvme0n1p5");
    }

    #[test]
    fn a_slot_of_linux_type_is_refused() {
        // The customer's disk as partitioned: named, but Linux's data type.
        let why = find("/dev/nvme0n1", &table(LINUX_FILESYSTEM)).unwrap_err();
        assert!(why.contains("not Ferrix's boot control type"), "{why}");
    }

    #[test]
    fn foreign_contents_are_named() {
        let mut head = vec![0_u8; HEAD_BYTES];
        assert_eq!(foreign(&head), None);
        head[4086..4096].copy_from_slice(b"SWAPSPACE2");
        assert_eq!(foreign(&head), Some("swap"));
        let mut head = vec![0_u8; HEAD_BYTES];
        head[0x10040..0x10048].copy_from_slice(b"_BHRfS_M");
        assert_eq!(foreign(&head), Some("btrfs"));
    }
}
