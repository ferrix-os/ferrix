//! Round trips, refusals, and agreement with `sgdisk` where the host has it.

extern crate std;

use alloc::vec;
use alloc::vec::Vec;
use std::process::Command;

use super::*;

const SECTORS: u64 = 64 * 2048;

fn two_partitions() -> Vec<Partition> {
    vec![
        Partition::new(ESP, Guid::random([1; 16]), 2048, 2048 * 17 - 1, "EFI"),
        Partition::new(
            LINUX_FILESYSTEM,
            Guid::random([2; 16]),
            2048 * 17,
            SECTORS - 2048,
            "ferrix-root",
        ),
    ]
}

/// The disk a table writes, as bytes.
fn disk_with(writes: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut disk = vec![0_u8; (SECTORS * 512) as usize];
    for (lba, bytes) in writes {
        let at = (*lba * 512) as usize;
        disk[at..at + bytes.len()].copy_from_slice(bytes);
    }
    disk
}

#[test]
fn guids_print_as_specifications_write_them() {
    assert_eq!(
        std::format!("{ESP}"),
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
    );
    assert_eq!(
        std::format!("{LINUX_FILESYSTEM}"),
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
    );
}

#[test]
fn crc32_is_iso_hdlc() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn a_written_table_reads_back() {
    let disk_guid = Guid::random([9; 16]);
    let writes = write_table(SECTORS, 512, disk_guid, &two_partitions()).expect("a table");
    let disk = disk_with(&writes);
    let header = parse_header(&disk[512..1024], SECTORS).expect("a header");
    assert_eq!(header.disk, disk_guid);
    assert_eq!(header.first_usable, 34);
    assert_eq!(header.last_usable, SECTORS - 34);
    let at = (header.entries_at * 512) as usize;
    let parts = parse_entries(&header, &disk[at..at + header.entries_bytes()]).expect("entries");
    let want: Vec<(u32, Partition)> = two_partitions()
        .into_iter()
        .zip(0..)
        .map(|(p, i)| (i, p))
        .collect();
    assert_eq!(parts, want);
    // The backup header names the primary and is valid in its own right.
    let backup = ((SECTORS - 1) * 512) as usize;
    assert_eq!(&disk[backup..backup + 8], b"EFI PART");
    assert_eq!(&disk[510..512], &[0x55, 0xAA]);
    assert_eq!(disk[450], 0xEE);
}

#[test]
fn damage_is_refused() {
    let writes =
        write_table(SECTORS, 512, Guid::random([9; 16]), &two_partitions()).expect("a table");
    let mut disk = disk_with(&writes);
    disk[600] ^= 1;
    assert_eq!(
        parse_header(&disk[512..1024], SECTORS),
        Err(Error::HeaderChecksum)
    );
    let mut disk = disk_with(&writes);
    disk[1024 + 40] ^= 1;
    let header = parse_header(&disk[512..1024], SECTORS).expect("a header");
    assert_eq!(
        parse_entries(&header, &disk[1024..1024 + header.entries_bytes()]),
        Err(Error::EntriesChecksum)
    );
    assert_eq!(parse_header(&[0; 512], SECTORS), Err(Error::NoTable));
    // A header that says the disk is larger than it is.
    let disk = disk_with(&writes);
    assert_eq!(
        parse_header(&disk[512..1024], SECTORS / 2),
        Err(Error::Malformed)
    );
}

#[test]
fn bad_partitions_are_refused_on_write() {
    let mut parts = two_partitions();
    parts[1].first = parts[0].last;
    assert_eq!(
        write_table(SECTORS, 512, Guid::default(), &parts),
        Err(Error::BadPartition)
    );
    let mut parts = two_partitions();
    parts[1].last = SECTORS;
    assert_eq!(
        write_table(SECTORS, 512, Guid::default(), &parts),
        Err(Error::BadPartition)
    );
    assert_eq!(
        write_table(40, 512, Guid::default(), &[]),
        Err(Error::TooSmall)
    );
}

#[test]
fn sgdisk_agrees_where_the_host_has_it() {
    let Ok(output) = Command::new("sgdisk").arg("--version").output() else {
        return;
    };
    assert!(output.status.success());
    let writes =
        write_table(SECTORS, 512, Guid::random([9; 16]), &two_partitions()).expect("a table");
    let path =
        std::env::temp_dir().join(std::format!("ferrix-partition-{}.img", std::process::id()));
    std::fs::write(&path, disk_with(&writes)).expect("the image");
    let verify = Command::new("sgdisk")
        .arg("-v")
        .arg(&path)
        .output()
        .expect("sgdisk");
    let printed = Command::new("sgdisk")
        .arg("-p")
        .arg(&path)
        .output()
        .expect("sgdisk");
    let _ = std::fs::remove_file(&path);
    let verify = std::string::String::from_utf8_lossy(&verify.stdout).into_owned();
    let printed = std::string::String::from_utf8_lossy(&printed.stdout).into_owned();
    assert!(verify.contains("No problems found"), "{verify}");
    assert!(
        printed.contains("EF00") && printed.contains("8300"),
        "{printed}"
    );
    assert!(printed.contains("ferrix-root"), "{printed}");
}
