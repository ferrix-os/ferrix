//! `ferrix-install --slots`: the A/B layout of `docs/AB-BOOT.md`
//! (`docs/DEV-ON-FERRIX.md` §4.4, D8).
//!
//! On a disk with no partition table and nothing recognisable on it, it
//! writes the layout the customer's SN720 was given on 2026-10-11, in this
//! order: an optional swap partition (`--swap-mib`, none by default), the
//! EFI system partition `ferrix-esp` (`--esp-mib`, 512), the boot control
//! partition `ferrix-bootctl` (1 MiB), the slots `ferrix-slot-a` and
//! `ferrix-slot-b` (`--slot-mib`, 4096 each), and the root `ferrix-root`, the
//! rest.
//!
//! On a disk that already has the layout -- one boot control partition and
//! two system slots, by type -- it writes no partition table and touches no
//! partition but Ferrix's own: the ESP named `ferrix-esp`, the control
//! partition, the two slots, and the root named `ferrix-root`. Any other
//! disk with a table is refused: this mode erases nothing it did not make,
//! since a disk shared with another system (the SN720's first partition is
//! the host's live swap) must keep the rest.
//!
//! What it writes:
//!
//! * the ESP: a byte copy of the live volume (the loader, and the kernel,
//!   initramfs and command line as the loader's rescue path, §4 step 7),
//!   with `ferrix.root=btrfs`;
//! * slot A: the live system's kernel, initramfs and command line, with
//!   `ferrix.root=btrfs`, verified after writing as the loader verifies it;
//! * slot B: its header zeroed, so it holds nothing that verifies;
//! * the control record: both copies zeroed, then the first record, A good;
//! * the root: the empty `ferrix-root` volume, as the whole-disk install does.

use std::fs::File;

use ferrix_install::disk::{self, SECTOR};
use ferrix_install::layout::{
    self, AbDisk, CONTROL_NAME, CONTROL_TYPE, ESP_NAME, LINUX_SWAP, Part, ROOT_NAME, SLOT_NAMES,
    SLOT_TYPE,
};
use ferrix_install::record::Blocks;
use ferrix_install::{record, slotio};
use ferrix_partition::{
    ESP, LINUX_FILESYSTEM, Partition, parse_entries, parse_header, usable, write_table,
};
use ferrix_slots::Slot;
use ferrix_slots::record::{COPY_OFFSETS, Record};
use ferrix_slots::slot::HEADER_LEN;

use super::{
    ALIGN, Asked, INSTALLED_ROOT, LIVE_ROOT, ROOT_BYTES, WIPE, confirm, copy_esp,
    installed_cmdline, random_guid, step, write_at, write_root,
};

/// The sizes of a new layout, in MiB.
#[derive(Clone, Copy, Debug)]
pub struct Sizes {
    pub swap_mib: u64,
    pub esp_mib: u64,
    pub slot_mib: u64,
}

impl Default for Sizes {
    /// The customer's SN720, less its swap, which is the host's.
    fn default() -> Sizes {
        Sizes {
            swap_mib: 0,
            esp_mib: 512,
            slot_mib: 4096,
        }
    }
}

/// The control partition's size.
const CONTROL_MIB: u64 = 1;
const MIB_SECTORS: u64 = (1 << 20) / SECTOR;

/// Install onto `asked.target` in the A/B layout from the live volume
/// `live`, open as `source`, of `esp_bytes` bytes.
pub fn install(
    asked: &Asked,
    sizes: &Sizes,
    live: &str,
    source: &mut File,
    esp_bytes: u64,
) -> Result<(), String> {
    let target = asked.target.as_str();
    let on = |e: std::io::Error| format!("{target}: {e}");
    let mut whole = disk::open_rw(target).map_err(on)?;
    let size = disk::size(&mut whole).map_err(on)?;
    let sectors = size / SECTOR;
    let block = disk::logical_block_size(target)?;
    if u64::from(block) != SECTOR {
        return Err(format!(
            "{target} has {block}-byte logical blocks; --slots writes only 512-byte ones for now"
        ));
    }

    let (ab, table) = match disk::table(&mut whole).map_err(on)? {
        Some(table) => match layout::find(target, &table)? {
            Some(ab) => (ab, None),
            None => {
                return Err(format!(
                    "{target} has a partition table without Ferrix's A/B layout; --slots erases \
                     nothing it did not make (use a blank disk, or one with the layout)"
                ));
            }
        },
        None => {
            let mut head = vec![0_u8; layout::HEAD_BYTES];
            disk::read_at(&mut whole, 0, &mut head).map_err(on)?;
            if let Some(what) = layout::foreign(&head) {
                return Err(format!(
                    "{target} holds {what}; --slots writes only a blank disk"
                ));
            }
            let partitions = new_layout(sectors, sizes, target)?;
            let table = write_table(sectors, SECTOR as u32, random_guid()?, &partitions)
                .map_err(|e| e.to_string())?;
            let ab = planned(target, sectors, &table)?;
            (ab, Some(table))
        }
    };
    let esp = ab.esp.clone().ok_or(format!(
        "{target} has no EFI system partition named {ESP_NAME}"
    ))?;
    let root = ab.root.clone().ok_or(format!(
        "{target} has no partition named {ROOT_NAME} of Linux's data type"
    ))?;
    if esp.bytes() < esp_bytes {
        return Err(format!(
            "{} is {} MiB; the live volume is {} MiB",
            esp.node,
            esp.bytes() >> 20,
            esp_bytes >> 20
        ));
    }
    if root.bytes() < ROOT_BYTES {
        return Err(format!(
            "{} is smaller than the {} MiB root volume",
            root.node,
            ROOT_BYTES >> 20
        ));
    }
    let mut image = slotio::load(live)?;
    image.cmdline = installed_words(&image.cmdline);
    let needed = image.header(1, "").needed();
    if needed > ab.slots[0].bytes() {
        return Err(format!(
            "the live system needs {} MiB; slot a is {} MiB",
            needed >> 20,
            ab.slots[0].bytes() >> 20
        ));
    }
    // Every partition written is checked first, on the disk as it is now.
    if table.is_none() {
        for part in ab.slots.iter().chain([&ab.control]) {
            check_at(&mut whole, part)?;
        }
        let mut head = vec![0_u8; layout::HEAD_BYTES];
        disk::read_at(&mut whole, root.entry.first * SECTOR, &mut head).map_err(on)?;
        if let Some(what) = layout::foreign(&head).filter(|&what| what != "btrfs") {
            return Err(format!("{} holds {what}, not a root volume", root.node));
        }
    }

    println!(
        "ferrix-install: {target} ({} MiB) {}: {} ESP {} MiB, control {}, slot a {} and b {} \
         ({} MiB each), root {} MiB; nothing else is written",
        size >> 20,
        if table.is_some() {
            "will be given the A/B layout"
        } else {
            "has the A/B layout and keeps its partition table"
        },
        esp.node,
        esp.bytes() >> 20,
        ab.control.node,
        ab.slots[0].node,
        ab.slots[1].node,
        ab.slots[0].bytes() >> 20,
        root.bytes() >> 20
    );
    if !asked.yes && !confirm()? {
        return Err("nothing was written".into());
    }

    if table.is_some() {
        step("wiping the ends of the blank disk", || {
            let zeros = vec![0_u8; WIPE as usize];
            write_at(&mut whole, 0, &zeros)?;
            write_at(&mut whole, size - WIPE, &zeros)
        })?;
    }
    step("copying the EFI system partition", || {
        copy_esp(source, &mut whole, esp_bytes, esp.entry.first)
    })?;
    step("setting the ESP's command line", || {
        installed_cmdline(&mut whole, esp.entry.first * SECTOR).map(|_| ())
    })?;
    let slot_a = &ab.slots[0];
    println!(
        "ferrix-install: writing slot a ({}): kernel {} KiB, initramfs {} KiB, `{}`",
        slot_a.node,
        image.kernel.len() >> 10,
        image.initrd.len() >> 10,
        String::from_utf8_lossy(&image.cmdline)
    );
    let base = slot_a.entry.first * SECTOR;
    let written = slotio::write(
        &mut whole,
        base,
        slot_a.bytes(),
        &image,
        1,
        "ferrix installed",
    )?;
    let back = slotio::verify(&mut whole, base, slot_a.bytes())
        .map_err(|why| format!("slot a does not verify after writing: {why}"))?;
    if back != written {
        return Err("slot a verifies but is not what was written".into());
    }
    println!("ferrix-install: slot a verified");
    step("emptying slot b", || {
        write_at(
            &mut whole,
            ab.slots[1].entry.first * SECTOR,
            &[0_u8; HEADER_LEN],
        )
    })?;
    let control = ab.control.entry.first * SECTOR;
    step("clearing the control record", || {
        for at in COPY_OFFSETS {
            write_at(&mut whole, control + at, &[0_u8; SECTOR as usize])?;
        }
        whole.sync_all()
    })?;
    let part = ab.slots_layout.control;
    let mut blocks = Blocks {
        file: &mut whole,
        block: SECTOR as u32,
    };
    let blank = record::read(&mut blocks, &part)?;
    let first = record::write(&mut blocks, &part, &blank, Some(Record::initial(Slot::A)))?;
    println!(
        "ferrix-install: the control record says slot a is good (record {})",
        first.sequence
    );
    step("making the root volume", || {
        write_root(&mut whole, root.entry.first * SECTOR, root.bytes())
    })?;
    if let Some(table) = table {
        step("writing the partition table", || {
            table
                .iter()
                .try_for_each(|(lba, bytes)| write_at(&mut whole, lba * SECTOR, bytes))
        })?;
    }
    step("flushing", || whole.sync_all())?;
    println!("ferrix-install: done. Remove the live disk and start the machine from {target}.");
    Ok(())
}

/// Refuse to write `part` of the whole disk `whole` unless it is of a
/// Ferrix type and holds nothing another system would miss.
fn check_at(whole: &mut File, part: &Part) -> Result<(), String> {
    if part.entry.type_guid != SLOT_TYPE && part.entry.type_guid != CONTROL_TYPE {
        return Err(format!("{} is not of a Ferrix type", part.node));
    }
    let mut head = vec![0_u8; layout::HEAD_BYTES];
    disk::read_at(whole, part.entry.first * SECTOR, &mut head).map_err(|e| e.to_string())?;
    match layout::foreign(&head) {
        Some(what) => Err(format!("{} holds {what}; nothing was written", part.node)),
        None => Ok(()),
    }
}

/// The live command line with `/` on the disk: `ferrix.root=tmpfs` made
/// `ferrix.root=btrfs`, added when there is no `ferrix.root=` at all.
fn installed_words(cmdline: &[u8]) -> Vec<u8> {
    let live = String::from_utf8_lossy(LIVE_ROOT).into_owned();
    let installed = String::from_utf8_lossy(INSTALLED_ROOT).into_owned();
    let mut words: Vec<String> = String::from_utf8_lossy(cmdline)
        .split_whitespace()
        .map(|w| {
            if w == live {
                installed.clone()
            } else {
                w.to_owned()
            }
        })
        .collect();
    if !words.iter().any(|w| w.starts_with("ferrix.root=")) {
        words.push(installed);
    }
    words.join(" ").into_bytes()
}

/// The partitions of a new layout on a disk of `sectors` sectors.
fn new_layout(sectors: u64, sizes: &Sizes, target: &str) -> Result<Vec<Partition>, String> {
    let (_, last_usable) = usable(sectors, SECTOR as u32).map_err(|e| e.to_string())?;
    let mut at = ALIGN;
    let mut out = Vec::new();
    let mut add = |kind, mib: u64, name: &str| -> Result<(), String> {
        let first = at;
        let last = first + mib * MIB_SECTORS - 1;
        at = last + 1;
        out.push(Partition::new(kind, random_guid()?, first, last, name));
        Ok(())
    };
    if sizes.swap_mib > 0 {
        add(LINUX_SWAP, sizes.swap_mib, "swap")?;
    }
    add(ESP, sizes.esp_mib, ESP_NAME)?;
    add(CONTROL_TYPE, CONTROL_MIB, CONTROL_NAME)?;
    add(SLOT_TYPE, sizes.slot_mib, SLOT_NAMES[0])?;
    add(SLOT_TYPE, sizes.slot_mib, SLOT_NAMES[1])?;
    let root_first = at;
    let root_last = (last_usable + 1) / ALIGN * ALIGN - 1;
    if root_last < root_first || (root_last - root_first + 1) * SECTOR < ROOT_BYTES {
        return Err(format!(
            "{target} is {} MiB; the layout needs {} MiB",
            (sectors * SECTOR) >> 20,
            (root_first * SECTOR + ROOT_BYTES + WIPE) >> 20
        ));
    }
    out.push(Partition::new(
        LINUX_FILESYSTEM,
        random_guid()?,
        root_first,
        root_last,
        ROOT_NAME,
    ));
    Ok(out)
}

/// The layout just planned, as `layout::find` will see it once `table` is
/// written to the disk of `sectors` sectors at `target`.
fn planned(target: &str, sectors: u64, table: &[(u64, Vec<u8>)]) -> Result<AbDisk, String> {
    let at = |lba: u64| {
        table
            .iter()
            .find(|(first, _)| *first == lba)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| format!("the new table has no sector {lba}"))
    };
    let raw_header = at(1)?;
    let raw_array = at(2)?;
    let header = parse_header(&raw_header, sectors).map_err(|e| e.to_string())?;
    let entries = parse_entries(&header, &raw_array).map_err(|e| e.to_string())?;
    let table = disk::Table {
        header,
        entries,
        raw_header,
        raw_array,
        sectors,
    };
    layout::find(target, &table)?
        .ok_or_else(|| "the new layout has no control partition".to_owned())
}
