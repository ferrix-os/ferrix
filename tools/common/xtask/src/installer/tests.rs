//! The live medium's layout, read back with the partition reader the kernel
//! publishes partitions with.

use std::io::Cursor;

use ferrix_partition::{ESP, Partition, parse_entries, parse_header};

use super::{LIVE_FIRST, LIVE_NAME, SECTOR, lay_out};
use crate::fat::Fat32;

/// A small FAT32 volume with a file in it, as `xtask` writes its images.
fn volume() -> Vec<u8> {
    let mut fs = Fat32::new(64 << 20).unwrap();
    fs.add_file("FERRIX/CMDLINE.TXT", b"ferrix.root=tmpfs")
        .unwrap();
    fs.finish()
}

fn sector(bytes: &[u8], lba: u64) -> &[u8] {
    let at = usize::try_from(lba * SECTOR).unwrap();
    &bytes[at..at + SECTOR as usize]
}

/// The medium has a valid GUID partition table at both ends whose one
/// partition is the EFI system partition named `FERRIX-LIVE`, over exactly
/// the volume; the volume's bytes are there unchanged but for the hidden
/// sectors, which now say where it starts.
#[test]
fn the_live_medium_holds_the_volume_in_its_one_partition() {
    let volume = volume();
    let mut medium = Cursor::new(Vec::new());
    let sectors = lay_out(&mut Cursor::new(&volume), volume.len() as u64, &mut medium).unwrap();
    let medium = medium.into_inner();
    assert_eq!(medium.len() as u64, sectors * SECTOR, "the medium's length");
    assert_eq!(sectors % 2048, 0, "the medium ends on a MiB");

    // The protective MBR.
    assert_eq!(&sector(&medium, 0)[510..512], &[0x55, 0xAA]);
    assert_eq!(sector(&medium, 0)[446 + 4], 0xEE);

    // The primary table, read as the kernel and `ferrix-install` read it.
    let volume_sectors = volume.len() as u64 / SECTOR;
    let header = parse_header(sector(&medium, 1), sectors).unwrap();
    assert_eq!(header.alternate, sectors - 1, "the backup header's place");
    let at = usize::try_from(header.entries_at * SECTOR).unwrap();
    let array = &medium[at..at + header.entries_bytes()];
    let entries = parse_entries(&header, array).unwrap();
    assert_eq!(entries.len(), 1, "one partition");
    let (index, partition) = entries[0];
    assert_eq!(index, 0);
    assert_eq!(partition.type_guid, ESP);
    assert_eq!(partition.first, LIVE_FIRST);
    assert_eq!(partition.sectors(), volume_sectors);
    let named = Partition::new(ESP, partition.unique, 0, 0, LIVE_NAME);
    assert_eq!(partition.name, named.name, "the partition's name");

    // The backup at the end: its header, and the same entries just before it.
    let backup = sector(&medium, sectors - 1);
    assert_eq!(&backup[..8], b"EFI PART", "the backup header");
    let backup_at = u64::from_le_bytes(backup[72..80].try_into().unwrap());
    let backup_at = usize::try_from(backup_at * SECTOR).unwrap();
    assert_eq!(
        &medium[backup_at..backup_at + header.entries_bytes()],
        array,
        "the backup entries"
    );

    let start = usize::try_from(LIVE_FIRST * SECTOR).unwrap();
    let placed = &medium[start..start + volume.len()];
    let hidden = u32::try_from(LIVE_FIRST).unwrap().to_le_bytes();
    assert_eq!(&placed[3..11], b"FERRIX  ", "the boot sector");
    assert_eq!(&placed[28..32], &hidden, "the boot sector's hidden sectors");
    assert_eq!(&placed[6 * 512 + 28..6 * 512 + 32], &hidden, "the backup's");
    let mut expected = volume.clone();
    expected[28..32].copy_from_slice(&hidden);
    expected[6 * 512 + 28..6 * 512 + 32].copy_from_slice(&hidden);
    assert!(
        placed == expected.as_slice(),
        "the volume's other bytes changed"
    );
}

/// A volume that is not whole sectors is refused, not cut.
#[test]
fn a_volume_of_part_sectors_is_refused() {
    let volume = vec![1_u8; 1000];
    let mut medium = Cursor::new(Vec::new());
    assert!(lay_out(&mut Cursor::new(&volume), 1000, &mut medium).is_err());
}
