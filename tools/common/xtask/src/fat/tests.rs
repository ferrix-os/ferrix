//! Tests for the FAT32 writer.
//!
//! The writer's failure mode is not a crash — it is firmware that reads the
//! image, finds nothing bootable, and drops to a shell with no message. So the
//! tests read the image back through an independent walker written against the
//! specification rather than against the writer, and assert on the bytes that
//! come out.

use super::*;

/// A read-only FAT32 walker, deliberately written from the on-disk layout
/// rather than by reusing the writer's helpers: a reader that shares the
/// writer's idea of where a cluster lives cannot catch the writer putting it in
/// the wrong place.
struct Reader<'a> {
    bytes: &'a [u8],
    sectors_per_cluster: u32,
    reserved: u32,
    num_fats: u32,
    fat_sectors: u32,
    root_cluster: u32,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        let u16_at =
            |offset: usize| u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
        let u32_at =
            |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());

        assert_eq!(u16_at(11), 512, "bytes per sector");
        assert_eq!(bytes[510], 0x55, "boot signature");
        assert_eq!(bytes[511], 0xAA, "boot signature");
        assert_eq!(&bytes[82..90], b"FAT32   ", "filesystem type string");

        Reader {
            bytes,
            sectors_per_cluster: u32::from(bytes[13]),
            reserved: u32::from(u16_at(14)),
            num_fats: u32::from(bytes[16]),
            fat_sectors: u32_at(36),
            root_cluster: u32_at(44),
        }
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR
    }

    fn cluster_at(&self, cluster: u32) -> &'a [u8] {
        let first_data = self.reserved + self.num_fats * self.fat_sectors;
        let sector = first_data + (cluster - 2) * self.sectors_per_cluster;
        let base = sector as usize * SECTOR;
        &self.bytes[base..base + self.cluster_bytes()]
    }

    fn next(&self, cluster: u32) -> Option<u32> {
        let offset = self.reserved as usize * SECTOR + cluster as usize * 4;
        let value =
            u32::from_le_bytes(self.bytes[offset..offset + 4].try_into().unwrap()) & 0x0FFF_FFFF;
        (2..0x0FFF_FFF8).contains(&value).then_some(value)
    }

    /// Every 32-byte directory entry in the chain starting at `cluster`.
    fn entries(&self, cluster: u32) -> Vec<[u8; 32]> {
        let mut out = Vec::new();
        let mut current = Some(cluster);
        while let Some(this) = current {
            let data = self.cluster_at(this);
            for slot in data.chunks_exact(32) {
                if slot[0] == 0x00 {
                    return out;
                }
                if slot[0] == 0xE5 {
                    continue;
                }
                out.push(slot.try_into().unwrap());
            }
            current = self.next(this);
        }
        out
    }

    fn find(&self, directory: u32, name: &str) -> Option<(u32, u32, u8)> {
        let wanted = short_name(name).unwrap();
        self.entries(directory).into_iter().find_map(|entry| {
            // Skip the volume label, whose name field can equal a real one.
            (entry[11] & ATTR_VOLUME_ID == 0 && entry[0..11] == wanted).then(|| {
                let high = u16::from_le_bytes(entry[20..22].try_into().unwrap());
                let low = u16::from_le_bytes(entry[26..28].try_into().unwrap());
                let size = u32::from_le_bytes(entry[28..32].try_into().unwrap());
                ((u32::from(high) << 16) | u32::from(low), size, entry[11])
            })
        })
    }

    /// Read a whole file by path.
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        let mut directory = self.root_cluster;
        let mut components = path.split('/').peekable();

        while let Some(name) = components.next() {
            let (cluster, size, attr) = self.find(directory, name)?;
            if components.peek().is_some() {
                assert_ne!(attr & ATTR_DIRECTORY, 0, "{name} should be a directory");
                directory = cluster;
                continue;
            }

            let mut out = Vec::new();
            let mut current = (cluster != 0).then_some(cluster);
            while let Some(this) = current {
                out.extend_from_slice(self.cluster_at(this));
                current = self.next(this);
            }
            out.truncate(size as usize);
            return Some(out);
        }
        None
    }
}

fn small_image() -> Fat32 {
    // The smallest image the format allows, so the geometry search is exercised
    // rather than trivially satisfied.
    Fat32::new(IMAGE_BYTES).unwrap()
}

#[test]
fn geometry_produces_a_real_fat32() {
    let (spc, fat_sectors, clusters) = geometry((IMAGE_BYTES / SECTOR) as u32).unwrap();
    assert!(spc.is_power_of_two(), "cluster size must be a power of two");
    assert!(
        clusters >= MIN_FAT32_CLUSTERS,
        "{clusters} clusters would be read as FAT16, not FAT32"
    );
    // The table must be big enough for every cluster it has to describe.
    assert!((fat_sectors as usize * SECTOR) >= (clusters as usize + 2) * 4);
}

#[test]
fn geometry_refuses_a_disk_too_small_for_fat32() {
    assert!(
        geometry(1024).is_err(),
        "a 512 KiB disk cannot hold 65525 clusters"
    );
}

#[test]
fn round_trips_a_file_through_nested_directories() {
    let mut fs = small_image();
    let payload: Vec<u8> = (0..5000u32).map(|byte| byte as u8).collect();
    fs.add_file("EFI/BOOT/BOOTX64.EFI", &payload).unwrap();

    let image = fs.finish();
    let reader = Reader::new(&image);
    assert_eq!(
        reader.read("EFI/BOOT/BOOTX64.EFI").as_deref(),
        Some(&payload[..])
    );
}

#[test]
fn round_trips_a_file_larger_than_one_cluster() {
    let mut fs = small_image();
    let cluster = fs.cluster_bytes();
    // Deliberately not a multiple of the cluster size, so the tail is checked.
    let payload: Vec<u8> = (0..cluster * 3 + 17)
        .map(|byte| (byte % 251) as u8)
        .collect();
    fs.add_file("FERRIX/KERNEL.ELF", &payload).unwrap();

    let image = fs.finish();
    let reader = Reader::new(&image);
    let read = reader.read("FERRIX/KERNEL.ELF").unwrap();
    assert_eq!(
        read.len(),
        payload.len(),
        "size must survive the round trip"
    );
    assert_eq!(read, payload);
}

#[test]
fn two_files_in_one_directory_do_not_overwrite_each_other() {
    let mut fs = small_image();
    fs.add_file("EFI/BOOT/BOOTX64.EFI", b"loader").unwrap();
    fs.add_file("EFI/BOOT/BOOTAA64.EFI", b"other loader")
        .unwrap();
    fs.add_file("FERRIX/KERNEL.ELF", b"kernel").unwrap();

    let image = fs.finish();
    let reader = Reader::new(&image);
    assert_eq!(
        reader.read("EFI/BOOT/BOOTX64.EFI").as_deref(),
        Some(&b"loader"[..])
    );
    assert_eq!(
        reader.read("EFI/BOOT/BOOTAA64.EFI").as_deref(),
        Some(&b"other loader"[..])
    );
    assert_eq!(
        reader.read("FERRIX/KERNEL.ELF").as_deref(),
        Some(&b"kernel"[..])
    );
}

#[test]
fn a_shared_parent_directory_is_created_once() {
    let mut fs = small_image();
    fs.add_file("EFI/BOOT/A.EFI", b"a").unwrap();
    fs.add_file("EFI/BOOT/B.EFI", b"b").unwrap();

    let image = fs.finish();
    let reader = Reader::new(&image);
    let roots = reader.entries(reader.root_cluster);
    let efi_entries = roots
        .iter()
        .filter(|entry| entry[0..11] == short_name("EFI").unwrap())
        .count();
    assert_eq!(efi_entries, 1, "EFI/ should not be created twice");
}

#[test]
fn subdirectories_carry_dot_and_dotdot() {
    let mut fs = small_image();
    fs.add_file("EFI/BOOT/BOOTX64.EFI", b"loader").unwrap();

    let image = fs.finish();
    let reader = Reader::new(&image);
    let (efi, _, attr) = reader.find(reader.root_cluster, "EFI").unwrap();
    assert_ne!(attr & ATTR_DIRECTORY, 0);

    let entries = reader.entries(efi);
    assert_eq!(&entries[0][0..11], b".          ");
    assert_eq!(&entries[1][0..11], b"..         ");

    // `..` from a top-level directory points at the root, which FAT spells 0.
    let high = u16::from_le_bytes(entries[1][20..22].try_into().unwrap());
    let low = u16::from_le_bytes(entries[1][26..28].try_into().unwrap());
    assert_eq!((u32::from(high) << 16) | u32::from(low), 0);

    // `..` from a nested directory points at its real parent.
    let (boot, _, _) = reader.find(efi, "BOOT").unwrap();
    let nested = reader.entries(boot);
    let high = u16::from_le_bytes(nested[1][20..22].try_into().unwrap());
    let low = u16::from_le_bytes(nested[1][26..28].try_into().unwrap());
    assert_eq!((u32::from(high) << 16) | u32::from(low), efi);
}

#[test]
fn an_empty_file_occupies_no_cluster() {
    let mut fs = small_image();
    fs.add_file("EMPTY.TXT", b"").unwrap();

    let image = fs.finish();
    let reader = Reader::new(&image);
    let (cluster, size, _) = reader.find(reader.root_cluster, "EMPTY.TXT").unwrap();
    assert_eq!(cluster, 0, "an empty file has no first cluster");
    assert_eq!(size, 0);
    assert_eq!(reader.read("EMPTY.TXT").as_deref(), Some(&b""[..]));
}

#[test]
fn short_names_are_padded_and_upper_cased() {
    assert_eq!(&short_name("efi").unwrap(), b"EFI        ");
    assert_eq!(&short_name("BOOTX64.EFI").unwrap(), b"BOOTX64 EFI");
    assert_eq!(&short_name("kernel.elf").unwrap(), b"KERNEL  ELF");
    assert_eq!(&short_name(".").unwrap(), b".          ");
    assert_eq!(&short_name("..").unwrap(), b"..         ");
}

#[test]
fn a_name_that_is_not_eight_dot_three_is_refused() {
    assert!(short_name("TOOLONGNAME.EFI").is_err());
    assert!(short_name("NAME.LONG").is_err());
    assert!(short_name(".hidden").is_err(), "an empty stem is not 8.3");
    assert!(
        Fat32::new(IMAGE_BYTES)
            .unwrap()
            .add_file("EFI/BOOT/VERYLONGNAME.EFI", b"x")
            .is_err(),
        "a bad name must fail loudly rather than be truncated"
    );
}

#[test]
fn the_volume_label_is_in_the_root_directory() {
    let image = small_image().finish();
    let reader = Reader::new(&image);
    let labels: Vec<_> = reader
        .entries(reader.root_cluster)
        .into_iter()
        .filter(|entry| entry[11] & ATTR_VOLUME_ID != 0)
        .collect();
    assert_eq!(labels.len(), 1);
    assert_eq!(&labels[0][0..11], b"FERRIX     ");
}

#[test]
fn both_copies_of_the_allocation_table_agree() {
    let mut fs = small_image();
    let payload: Vec<u8> = (0..20_000u32).map(|byte| byte as u8).collect();
    fs.add_file("FERRIX/KERNEL.ELF", &payload).unwrap();

    let fat_sectors = fs.fat_sectors as usize * SECTOR;
    let image = fs.finish();
    let first = RESERVED_SECTORS as usize * SECTOR;
    let second = first + fat_sectors;

    assert_eq!(
        image[first..first + fat_sectors],
        image[second..second + fat_sectors],
        "a mirror that disagrees is a filesystem a repair tool will 'fix' wrongly"
    );
}

#[test]
fn the_backup_boot_sector_matches_the_primary() {
    let image = small_image().finish();
    assert_eq!(image[0..SECTOR], image[6 * SECTOR..7 * SECTOR]);
}

#[test]
fn the_image_is_reproducible() {
    let build = || {
        let mut fs = Fat32::new(IMAGE_BYTES).unwrap();
        fs.add_file("EFI/BOOT/BOOTX64.EFI", b"loader").unwrap();
        fs.add_file("FERRIX/KERNEL.ELF", b"kernel").unwrap();
        fs.finish()
    };
    assert_eq!(
        build(),
        build(),
        "no clock or random value may reach the image"
    );
}

#[test]
fn a_directory_named_like_the_volume_label_is_still_found() {
    // Regression: `FERRIX` as a directory name is byte-identical to `FERRIX` as
    // a volume label, and the lookup matched the label first -- returning its
    // first-cluster field, which is 0 and not a cluster at all. Every write
    // under FERRIX/ then computed a negative cluster offset.
    let mut fs = small_image();
    fs.add_file("FERRIX/KERNEL.ELF", b"kernel").unwrap();
    fs.add_file("FERRIX/INITRD.IMG", b"initrd").unwrap();

    let image = fs.finish();
    let reader = Reader::new(&image);
    let (cluster, _, attr) = reader.find(reader.root_cluster, "FERRIX").unwrap();
    assert!(cluster >= 2, "a directory must start at a real cluster");
    assert_ne!(attr & ATTR_DIRECTORY, 0);
    assert_eq!(
        reader.read("FERRIX/KERNEL.ELF").as_deref(),
        Some(&b"kernel"[..])
    );
    assert_eq!(
        reader.read("FERRIX/INITRD.IMG").as_deref(),
        Some(&b"initrd"[..])
    );
}

#[test]
fn the_clusters_a_driver_counts_all_have_fat_entries() {
    // A driver takes the cluster count from the boot sector: the sectors
    // after the reserved ones and the tables, by the cluster size. Every one
    // of those clusters (numbered from 2) needs an entry in the table, or
    // fsck.fat and mtools refuse the volume. 512 MiB is the N150 box's ESP,
    // which had 226 clusters too many.
    for bytes in [IMAGE_BYTES, 64 << 20, 66 << 20, 100 << 20, 512 << 20] {
        let image = Fat32::new(bytes).unwrap().finish();
        let u16_at = |i: usize| u32::from(u16::from_le_bytes([image[i], image[i + 1]]));
        let u32_at =
            |i: usize| u32::from_le_bytes([image[i], image[i + 1], image[i + 2], image[i + 3]]);
        let (spc, reserved, fats) = (u32::from(image[13]), u16_at(14), u32::from(image[16]));
        let (total, fat_sectors) = (u32_at(32), u32_at(36));
        assert!(
            total as usize <= bytes / SECTOR,
            "{bytes}: the volume is larger than its image"
        );
        let clusters = (total - reserved - fats * fat_sectors) / spc;
        let entries = fat_sectors * (SECTOR as u32 / 4);
        assert!(
            clusters + 2 <= entries,
            "{bytes}: {clusters} clusters but only {entries} FAT entries"
        );
        assert!(
            clusters >= MIN_FAT32_CLUSTERS,
            "{bytes}: {clusters} clusters is not FAT32"
        );
    }
}
