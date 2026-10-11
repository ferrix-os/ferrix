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

/// Ferrix's own partition types (`docs/AB-BOOT.md` §2, §3), `ferrix-slots`'.
/// The customer's disk carries Linux's data type on its slots until these
/// are written there (the product owner's write phase).
pub const SLOT_TYPE: Guid = Guid(ferrix_slots::gpt::SLOT_TYPE);
pub const CONTROL_TYPE: Guid = Guid(ferrix_slots::gpt::CONTROL_TYPE);
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
    /// The disk's GUID, which the loader names (`ferrix.slot.disk=`).
    pub guid: Guid,
    /// The layout as `ferrix-slots` reads it, the loader's reading: the
    /// control record is written through it.
    pub slots_layout: ferrix_slots::gpt::Layout,
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
    // The loader's own reading of the same table must find the same three.
    let slots_layout = loaders_reading(table)
        .map_err(|why| format!("{disk}: ferrix-slots refuses its table: {why:?}"))?;
    let agrees = |mine: &Part, theirs: &ferrix_slots::gpt::Part| {
        mine.entry.first == theirs.first_lba && mine.entry.last == theirs.last_lba
    };
    if !agrees(control, &slots_layout.control)
        || !agrees(a, &slots_layout.a)
        || !agrees(b, &slots_layout.b)
    {
        return Err(format!(
            "{disk}: the loader's reading of its table disagrees with this one"
        ));
    }
    Ok(Some(AbDisk {
        disk: disk.to_owned(),
        guid: table.header.disk,
        slots_layout,
        esp,
        control: control.clone(),
        slots: [a.clone(), b.clone()],
        root,
    }))
}

/// `ferrix-slots`' reading of `table`: the loader's.
fn loaders_reading(
    table: &Table,
) -> Result<ferrix_slots::gpt::Layout, ferrix_slots::gpt::GptError> {
    let block = SECTOR as u32;
    let header = ferrix_slots::gpt::parse_header(&table.raw_header, table.sectors, block)?;
    ferrix_slots::gpt::find(&header, &table.raw_array, table.sectors, block)
}

/// Whether `table` has any partition of Ferrix's boot control type.
fn has_control(table: &Table) -> bool {
    table
        .entries
        .iter()
        .any(|(_, entry)| entry.type_guid == CONTROL_TYPE)
}

/// The one disk with the A/B layout (`docs/AB-BOOT.md` §5; the
/// consultant's S1): of `disks`, each a path and its table, exactly one
/// may carry a partition of Ferrix's boot control type, and it must be the
/// one the loader names by its GUID (`booted`) when it named one, and the
/// one `named` when a disk is named. A disk named that has no control
/// partition is refused with what is wrong with it.
///
/// # Errors
///
/// None, more than one, not the disk asked for, or a wrong layout.
pub fn choose(
    disks: &[(String, Table)],
    named: Option<&str>,
    booted: Option<Guid>,
) -> Result<AbDisk, String> {
    let carrying: Vec<&(String, Table)> = disks.iter().filter(|(_, t)| has_control(t)).collect();
    if let Some(name) = named
        && !carrying.iter().any(|(path, _)| path == name)
    {
        return Err(match disks.iter().find(|(path, _)| path == name) {
            Some((path, table)) => match find(path, table)? {
                Some(_) => unreachable_layout(path),
                None => format!("{path} has no partition of Ferrix's boot control type"),
            },
            None => format!("{name} has no GUID partition table"),
        });
    }
    let [(path, table)] = carrying.as_slice() else {
        return Err(if carrying.is_empty() {
            "no disk has a partition of Ferrix's boot control type".to_owned()
        } else {
            format!(
                "more than one disk has a partition of Ferrix's boot control type ({}); \
                 bootctl acts only when there is one",
                carrying
                    .iter()
                    .map(|(path, _)| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    };
    if let Some(guid) = booted
        && table.header.disk != guid
    {
        return Err(format!(
            "{path} is disk {}, but this system booted from disk {guid}",
            table.header.disk
        ));
    }
    find(path, table)?.ok_or_else(|| unreachable_layout(path))
}

fn unreachable_layout(path: &str) -> String {
    format!("{path}: its table changed while it was read")
}

/// [`choose`] over every whole disk this system has.
///
/// # Errors
///
/// As [`choose`], or a disk that cannot be read.
pub fn locate(named: Option<&str>, booted: Option<Guid>) -> Result<AbDisk, String> {
    let mut disks = Vec::new();
    for path in disk::whole_disks() {
        let Ok(mut file) = disk::open(&path) else {
            continue;
        };
        if let Some(table) = disk::table(&mut file).map_err(|e| format!("{path}: {e}"))? {
            disks.push((path, table));
        }
    }
    choose(&disks, named, booted)
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

    /// The customer's layout, in MiB, scaled down, on disk `disk`.
    fn table_of(slot_type: Guid, disk: u8) -> Table {
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
        let written = write_table(sectors, 512, Guid([disk; 16]), &parts).unwrap();
        let header = parse_header(&written[1].1, sectors).unwrap();
        let entries = parse_entries(&header, &written[2].1).unwrap();
        Table {
            header,
            entries,
            raw_header: written[1].1.clone(),
            raw_array: written[2].1.clone(),
            sectors,
        }
    }

    fn table(slot_type: Guid) -> Table {
        table_of(slot_type, 9)
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
    fn one_disk_only_and_the_one_the_loader_names() {
        let ab = |disk| {
            (
                "/dev/vd".to_owned() + disk,
                table_of(SLOT_TYPE, disk.as_bytes()[0]),
            )
        };
        let linux = ("/dev/vdf".to_owned(), table(LINUX_FILESYSTEM));
        // One carrying disk beside the SN720 as it is: that one.
        let one = vec![ab("d"), linux];
        let found = choose(&one, None, None).unwrap();
        assert_eq!(found.disk, "/dev/vdd");
        assert_eq!(found.guid, Guid([b'd'; 16]));
        // The loader named it: still that one; named another: refused.
        assert!(choose(&one, None, Some(Guid([b'd'; 16]))).is_ok());
        let why = choose(&one, None, Some(Guid([b'e'; 16]))).unwrap_err();
        assert!(why.contains("booted from disk"), "{why}");
        // The SN720 as it is, named: refused, saying why.
        let why = choose(&one, Some("/dev/vdf"), None).unwrap_err();
        assert!(why.contains("not Ferrix's boot control type"), "{why}");
        // Two carrying disks: refused, whichever is named or booted.
        let two = vec![ab("d"), ab("e")];
        for (named, booted) in [
            (None, None),
            (Some("/dev/vdd"), None),
            (None, Some(Guid([b'd'; 16]))),
        ] {
            let why = choose(&two, named, booted).unwrap_err();
            assert!(why.contains("more than one disk"), "{why}");
        }
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
