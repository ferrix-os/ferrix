//! Host tests over images built here, by hand, with what each case plants.

use alloc::vec;
use alloc::vec::Vec;

use super::{Error, Volume};

/// The test volume's geometry: 512-byte sectors, one a cluster, 32 reserved,
/// one table of 8 sectors (1024 entries), 400 data clusters.
const RESERVED: usize = 32;
const FAT_SECTORS: usize = 8;
const CLUSTERS: usize = 400;

/// An image with `FERRIX/LASTKERNEL.TXT` holding `text` (long name with its
/// short alias `LASTKE~1.TXT`), and `checksum_off` to plant a long name whose
/// pieces carry the wrong checksum.
fn image(text: &[u8], checksum_off: bool) -> Vec<u8> {
    let total = RESERVED + FAT_SECTORS + CLUSTERS;
    let mut disk = vec![0_u8; total * 512];
    {
        let boot = &mut disk[..512];
        boot[11..13].copy_from_slice(&512_u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
        boot[16] = 1;
        boot[32..36].copy_from_slice(&(total as u32).to_le_bytes());
        boot[36..40].copy_from_slice(&(FAT_SECTORS as u32).to_le_bytes());
        boot[44..48].copy_from_slice(&2_u32.to_le_bytes());
        boot[510] = 0x55;
        boot[511] = 0xAA;
    }
    let data = (RESERVED + FAT_SECTORS) * 512;
    let set = |disk: &mut Vec<u8>, cluster: usize, value: u32| {
        let at = RESERVED * 512 + cluster * 4;
        disk[at..at + 4].copy_from_slice(&value.to_le_bytes());
    };
    // Root (2) and FERRIX (3) one cluster each; the file from 4 on.
    set(&mut disk, 2, 0x0FFF_FFFF);
    set(&mut disk, 3, 0x0FFF_FFFF);
    let needed = text.len().div_ceil(512).max(1);
    for index in 0..needed {
        let cluster = 4 + index;
        let next = if index + 1 == needed {
            0x0FFF_FFFF
        } else {
            (cluster + 1) as u32
        };
        set(&mut disk, cluster, next);
    }
    let entry = |name: &[u8; 11], attributes: u8, cluster: u32, size: u32| {
        let mut raw = [0_u8; 32];
        raw[..11].copy_from_slice(name);
        raw[11] = attributes;
        raw[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        raw[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        raw[28..32].copy_from_slice(&size.to_le_bytes());
        raw
    };
    let root = data;
    disk[root..root + 32].copy_from_slice(&entry(b"FERRIX     ", 0x10, 3, 0));
    let short = *b"LASTKE~1TXT";
    let mut sum = super::checksum(&short);
    if checksum_off {
        sum = sum.wrapping_add(1);
    }
    let long = "LASTKERNEL.TXT";
    let units: Vec<u16> = long.encode_utf16().chain([0]).collect();
    let pieces = units.len().div_ceil(13);
    let mut at = data + 512;
    for piece in (0..pieces).rev() {
        let mut raw = [0xFF_u8; 32];
        raw[0] = (piece as u8 + 1) | if piece + 1 == pieces { 0x40 } else { 0 };
        raw[11] = 0x0F;
        raw[12] = 0;
        raw[13] = sum;
        raw[26] = 0;
        raw[27] = 0;
        for (index, offset) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
            .into_iter()
            .enumerate()
        {
            if let Some(unit) = units.get(piece * 13 + index) {
                raw[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        disk[at..at + 32].copy_from_slice(&raw);
        at += 32;
    }
    disk[at..at + 32].copy_from_slice(&entry(&short, 0x20, 4, text.len() as u32));
    let file = data + 2 * 512;
    disk[file..file + text.len()].copy_from_slice(text);
    disk
}

fn text() -> Vec<u8> {
    (0..1300_u32).map(|n| b'a' + (n % 26) as u8).collect()
}

#[test]
fn a_long_named_file_reads_back_whole_whatever_its_case() {
    let disk = image(&text(), false);
    let mut volume = Volume::open(disk.as_slice()).unwrap();
    assert_eq!(
        volume.read_file("FERRIX/LASTKERNEL.TXT", 1 << 20).unwrap(),
        text()
    );
    assert_eq!(
        volume.read_file("ferrix/lastkernel.txt", 1 << 20).unwrap(),
        text()
    );
    assert_eq!(
        volume.read_file("FERRIX/LASTKERNEL.PRV", 1 << 20),
        Err(Error::NotFound)
    );
    assert_eq!(volume.read_file("FERRIX", 1 << 20), Err(Error::NotAFile));
    assert_eq!(
        volume.read_file("FERRIX/LASTKERNEL.TXT", 100),
        Err(Error::TooLarge)
    );
}

#[test]
fn a_long_name_with_the_wrong_checksum_is_its_short_name() {
    let disk = image(&text(), true);
    let mut volume = Volume::open(disk.as_slice()).unwrap();
    assert_eq!(
        volume.read_file("FERRIX/LASTKERNEL.TXT", 1 << 20),
        Err(Error::NotFound)
    );
    assert_eq!(
        volume.read_file("FERRIX/LASTKE~1.TXT", 1 << 20).unwrap(),
        text()
    );
}

#[test]
fn a_chain_that_loops_or_leaves_the_volume_is_refused() {
    let cluster = |disk: &mut Vec<u8>, at: usize, value: u32| {
        let offset = RESERVED * 512 + at * 4;
        disk[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    };
    let mut looped = image(&text(), false);
    cluster(&mut looped, 6, 4);
    // The file's size stops the read before the loop would: a directory has
    // no size, so the loop is planted in FERRIX's own chain.
    cluster(&mut looped, 3, 3);
    let mut volume = Volume::open(looped.as_slice()).unwrap();
    assert_eq!(
        volume.read_file("FERRIX/LASTKERNEL.TXT", 1 << 20),
        Err(Error::Corrupt("a chain that loops"))
    );
    let mut outside = image(&text(), false);
    cluster(&mut outside, 4, 5000);
    let mut volume = Volume::open(outside.as_slice()).unwrap();
    assert_eq!(
        volume.read_file("FERRIX/LASTKERNEL.TXT", 1 << 20),
        Err(Error::Corrupt("a cluster outside the volume"))
    );
    let mut free = image(&text(), false);
    cluster(&mut free, 5, 0);
    let mut volume = Volume::open(free.as_slice()).unwrap();
    assert_eq!(
        volume.read_file("FERRIX/LASTKERNEL.TXT", 1 << 20),
        Err(Error::Corrupt("a free cluster in a chain"))
    );
    let mut short = image(&text(), false);
    cluster(&mut short, 5, 0x0FFF_FFFF);
    let mut volume = Volume::open(short.as_slice()).unwrap();
    assert_eq!(
        volume.read_file("FERRIX/LASTKERNEL.TXT", 1 << 20),
        Err(Error::Corrupt("a chain shorter than its file"))
    );
}

#[test]
fn a_boot_sector_that_is_not_fat32s_is_refused() {
    let good = image(b"x", false);
    let mut unsigned = good.clone();
    unsigned[510] = 0;
    assert!(matches!(
        Volume::open(unsigned.as_slice()),
        Err(Error::NotFat32(_))
    ));
    let mut fat16 = good.clone();
    fat16[17] = 0x00;
    fat16[18] = 0x02;
    assert!(matches!(
        Volume::open(fat16.as_slice()),
        Err(Error::NotFat32(_))
    ));
    let mut odd = good.clone();
    odd[13] = 3;
    assert!(matches!(
        Volume::open(odd.as_slice()),
        Err(Error::NotFat32(_))
    ));
    let mut huge = good.clone();
    huge[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        Volume::open(huge.as_slice()),
        Err(Error::NotFat32(_))
    ));
    let mut root = good;
    root[44..48].copy_from_slice(&9999_u32.to_le_bytes());
    assert!(matches!(
        Volume::open(root.as_slice()),
        Err(Error::Corrupt(_))
    ));
    // Too short to hold a boot sector at all.
    assert_eq!(Volume::open(&[0_u8; 100][..]).err(), Some(Error::Io));
}
