//! A FAT32 writer, so building a bootable image needs no external tools.
//!
//! UEFI boots from a FAT filesystem, which normally means `mkfs.vfat` and
//! `mtools` — two more things to install, absent on Windows, and a different
//! version on every CI runner. A few hundred lines of Rust removes all of that
//! and makes the image byte-for-byte reproducible, because every timestamp and
//! volume ID below is a constant rather than a clock reading.
//!
//! Only what a boot image needs: 8.3 names (every path we write fits, so there
//! are no long-name entries), no fragmentation concerns, and write-once
//! semantics.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::paths::{self, Arch};
use crate::{Error, Result};

/// Bytes per sector. UEFI firmware assumes 512 on removable media.
const SECTOR: usize = 512;
/// Sectors before the first file allocation table.
const RESERVED_SECTORS: u32 = 32;
/// Two, as every FAT filesystem has had since MS-DOS.
const NUM_FATS: u32 = 2;
/// The first cluster of the root directory. FAT32 fixes this at 2 in practice.
const ROOT_CLUSTER: u32 = 2;
/// End-of-chain marker written into the last cluster of every file.
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;
/// FAT32 is only FAT32 if it has at least this many clusters; below it, the
/// format is FAT16 and firmware will read it as such.
const MIN_FAT32_CLUSTERS: u32 = 65_525;

/// Size of the image we build. Comfortably larger than a kernel, a loader and
/// an initramfs, and small enough to write in a moment.
///
/// 64 MiB held until 2026-09-16, when the unoptimised x86-64 kernel reached
/// 57.5 MiB and, with busybox's initramfs of 6.7 MiB and the loader beside it,
/// no longer fitted: `run --init` failed with this file's "image is full".
/// Doubled rather than nudged, because the kernel grows with every landing.
///
/// 128 MiB held until 2026-09-20, when the image began carrying oh-my-zsh --
/// 12 MiB of themes and plugins, since a shell's configuration is no use if
/// only part of it is there -- and the same failure came back. Doubled again,
/// for the same reason: this is a file on a disk, not memory the guest holds.
///
/// 256 MiB held until 2026-10-03, when the release x86-64 kernel reached
/// 94 MB and `run-compositor --everything` failed the same way again.
/// Doubled once more; `--strip-kernel` remains the way to keep it small.
const IMAGE_BYTES: usize = 512 * 1024 * 1024;

/// A frozen timestamp — 2026-01-01 00:00:00 — so that two builds of the same
/// inputs produce the same image.
const FIXED_TIME: u16 = 0;
const FIXED_DATE: u16 = (46 << 9) | (1 << 5) | 1;

/// Directory-entry attribute: subdirectory.
const ATTR_DIRECTORY: u8 = 0x10;
/// Directory-entry attribute: regular file.
const ATTR_ARCHIVE: u8 = 0x20;
/// Directory-entry attribute: the volume label in the root directory.
const ATTR_VOLUME_ID: u8 = 0x08;

/// A FAT32 filesystem being built in memory.
#[derive(Debug)]
pub(crate) struct Fat32 {
    bytes: Vec<u8>,
    sectors_per_cluster: u32,
    fat_sectors: u32,
    cluster_count: u32,
    next_free: u32,
}

impl Fat32 {
    /// Create an empty filesystem of `size` bytes with a root directory.
    pub(crate) fn new(size: usize) -> Result<Self> {
        let total_sectors = u32::try_from(size / SECTOR)
            .map_err(|_| Error::new("image size does not fit a FAT32 sector count"))?;

        let (sectors_per_cluster, fat_sectors, cluster_count) = geometry(total_sectors)?;

        let mut fs = Fat32 {
            bytes: vec![0u8; size],
            sectors_per_cluster,
            fat_sectors,
            cluster_count,
            next_free: ROOT_CLUSTER + 1,
        };

        fs.write_boot_sector(total_sectors);
        fs.write_fs_info();

        // FAT[0] is the media descriptor, FAT[1] the end-of-chain marker; both
        // are conventions rather than allocations.
        fs.set_fat(0, 0x0FFF_FFF8);
        fs.set_fat(1, END_OF_CHAIN);
        fs.set_fat(ROOT_CLUSTER, END_OF_CHAIN);

        fs.add_volume_label(ROOT_CLUSTER, "FERRIX")?;
        Ok(fs)
    }

    /// The finished image.
    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    /// Write `data` to `path`, creating parent directories as needed.
    ///
    /// `path` is slash-separated and each component must fit an 8.3 name; the
    /// boot image's paths (`EFI/BOOT/BOOTX64.EFI`) all do, and a name that does
    /// not is a mistake worth failing on rather than silently truncating.
    pub(crate) fn add_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let mut components = path.split('/').filter(|part| !part.is_empty());
        let Some(mut name) = components.next() else {
            return Err(Error::new("empty path"));
        };

        let mut directory = ROOT_CLUSTER;
        for next in components {
            directory = self.ensure_directory(directory, name)?;
            name = next;
        }

        let first = if data.is_empty() {
            0
        } else {
            self.write_chain(data)?
        };
        let size = u32::try_from(data.len())
            .map_err(|_| Error::new(format!("{path} is larger than FAT32 allows")))?;
        self.add_entry(directory, name, ATTR_ARCHIVE, first, size)
    }

    // -- geometry ----------------------------------------------------------

    /// Byte offset of the first sector of `cluster`.
    fn cluster_offset(&self, cluster: u32) -> usize {
        let first_data_sector = RESERVED_SECTORS + NUM_FATS * self.fat_sectors;
        let sector = first_data_sector + (cluster - ROOT_CLUSTER) * self.sectors_per_cluster;
        sector as usize * SECTOR
    }

    /// Bytes in one cluster.
    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * SECTOR
    }

    // -- the allocation tables ---------------------------------------------

    /// Set the FAT entry for `cluster`, in every copy of the table.
    fn set_fat(&mut self, cluster: u32, value: u32) {
        for copy in 0..NUM_FATS {
            let base = (RESERVED_SECTORS + copy * self.fat_sectors) as usize * SECTOR;
            let offset = base + cluster as usize * 4;
            if let Some(slot) = self.bytes.get_mut(offset..offset + 4) {
                slot.copy_from_slice(&(value & 0x0FFF_FFFF).to_le_bytes());
            }
        }
    }

    /// Take the next free cluster, or fail if the image is full.
    fn allocate(&mut self) -> Result<u32> {
        let cluster = self.next_free;
        if cluster >= self.cluster_count + ROOT_CLUSTER {
            return Err(Error::new(
                "FAT32 image is full; raise IMAGE_BYTES in tools/common/xtask/src/fat.rs",
            ));
        }
        self.next_free += 1;
        self.set_fat(cluster, END_OF_CHAIN);
        Ok(cluster)
    }

    /// Copy `data` into a fresh cluster chain and return its first cluster.
    fn write_chain(&mut self, data: &[u8]) -> Result<u32> {
        let cluster_bytes = self.cluster_bytes();
        let mut first = 0u32;
        let mut previous = 0u32;

        for chunk in data.chunks(cluster_bytes) {
            let cluster = self.allocate()?;
            if first == 0 {
                first = cluster;
            } else {
                self.set_fat(previous, cluster);
            }
            previous = cluster;

            let offset = self.cluster_offset(cluster);
            let Some(slot) = self.bytes.get_mut(offset..offset + chunk.len()) else {
                return Err(Error::new("cluster lies outside the image"));
            };
            slot.copy_from_slice(chunk);
        }

        Ok(first)
    }

    // -- directories -------------------------------------------------------

    /// Find `name` under `parent`, creating it as a directory if absent.
    fn ensure_directory(&mut self, parent: u32, name: &str) -> Result<u32> {
        if let Some(existing) = self.find_entry(parent, name) {
            return Ok(existing);
        }

        let cluster = self.allocate()?;
        let offset = self.cluster_offset(cluster);
        let bytes = self.cluster_bytes();
        if let Some(slot) = self.bytes.get_mut(offset..offset + bytes) {
            slot.fill(0);
        }

        // `.` and `..`, which every FAT subdirectory begins with. A `..` whose
        // parent is the root is written as cluster 0, not as 2.
        self.add_entry(cluster, ".", ATTR_DIRECTORY, cluster, 0)?;
        let parent_link = if parent == ROOT_CLUSTER { 0 } else { parent };
        self.add_entry(cluster, "..", ATTR_DIRECTORY, parent_link, 0)?;

        self.add_entry(parent, name, ATTR_DIRECTORY, cluster, 0)?;
        Ok(cluster)
    }

    /// The first cluster of `name` under `directory`, if it is there.
    fn find_entry(&self, directory: u32, name: &str) -> Option<u32> {
        let wanted = short_name(name).ok()?;
        let mut cluster = directory;

        loop {
            let base = self.cluster_offset(cluster);
            for slot in (0..self.cluster_bytes()).step_by(32) {
                let entry = self.bytes.get(base + slot..base + slot + 32)?;
                match entry.first()? {
                    0x00 => return None, // end of directory
                    0xE5 => continue,    // deleted
                    _ => {}
                }
                // The volume label is an entry whose name field is the label,
                // and `FERRIX` as a label is byte-identical to `FERRIX` as a
                // directory name. Matching it returns first-cluster 0, which is
                // not a cluster at all. Skipping label and long-name entries is
                // what every real FAT reader does, and for this reason.
                let attr = *entry.get(11)?;
                if attr & ATTR_VOLUME_ID != 0 {
                    continue;
                }
                if entry.get(0..11)? == wanted {
                    let high = u16::from_le_bytes(entry.get(20..22)?.try_into().ok()?);
                    let low = u16::from_le_bytes(entry.get(26..28)?.try_into().ok()?);
                    return Some((u32::from(high) << 16) | u32::from(low));
                }
            }
            cluster = self.next_in_chain(cluster)?;
        }
    }

    /// Follow one link of a cluster chain, or `None` at the end.
    fn next_in_chain(&self, cluster: u32) -> Option<u32> {
        let offset = RESERVED_SECTORS as usize * SECTOR + cluster as usize * 4;
        let value =
            u32::from_le_bytes(self.bytes.get(offset..offset + 4)?.try_into().ok()?) & 0x0FFF_FFFF;
        (value < 0x0FFF_FFF8).then_some(value)
    }

    /// Append a directory entry to `directory`, extending its chain if full.
    fn add_entry(
        &mut self,
        directory: u32,
        name: &str,
        attr: u8,
        first_cluster: u32,
        size: u32,
    ) -> Result<()> {
        let entry = directory_entry(&short_name(name)?, attr, first_cluster, size);
        let offset = self.free_slot(directory)?;
        let Some(slot) = self.bytes.get_mut(offset..offset + 32) else {
            return Err(Error::new("directory entry lies outside the image"));
        };
        slot.copy_from_slice(&entry);
        Ok(())
    }

    /// Byte offset of the next unused 32-byte slot in `directory`.
    fn free_slot(&mut self, directory: u32) -> Result<usize> {
        let mut cluster = directory;
        loop {
            let base = self.cluster_offset(cluster);
            for slot in (0..self.cluster_bytes()).step_by(32) {
                match self.bytes.get(base + slot) {
                    Some(0x00 | 0xE5) => return Ok(base + slot),
                    Some(_) => {}
                    None => return Err(Error::new("directory lies outside the image")),
                }
            }
            match self.next_in_chain(cluster) {
                Some(next) => cluster = next,
                None => {
                    let extension = self.allocate()?;
                    self.set_fat(cluster, extension);
                    let offset = self.cluster_offset(extension);
                    let bytes = self.cluster_bytes();
                    if let Some(area) = self.bytes.get_mut(offset..offset + bytes) {
                        area.fill(0);
                    }
                    cluster = extension;
                }
            }
        }
    }

    /// Write the volume label, which lives in the root directory as an entry
    /// with no contents.
    fn add_volume_label(&mut self, directory: u32, label: &str) -> Result<()> {
        let mut name = [b' '; 11];
        for (slot, byte) in name.iter_mut().zip(label.bytes()) {
            *slot = byte.to_ascii_uppercase();
        }
        let entry = directory_entry(&name, ATTR_VOLUME_ID, 0, 0);
        let offset = self.free_slot(directory)?;
        let Some(slot) = self.bytes.get_mut(offset..offset + 32) else {
            return Err(Error::new("root directory lies outside the image"));
        };
        slot.copy_from_slice(&entry);
        Ok(())
    }

    // -- the reserved region ----------------------------------------------

    fn write_boot_sector(&mut self, total_sectors: u32) {
        let mut sector = [0u8; SECTOR];

        // A jump over the BPB. No boot code follows it — this disk is booted by
        // UEFI, which reads the filesystem rather than executing sector 0 — but
        // firmware checks the first byte is a plausible jump.
        sector[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        sector[3..11].copy_from_slice(b"FERRIX  ");

        sector[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        sector[13] = self.sectors_per_cluster as u8;
        sector[14..16].copy_from_slice(&(RESERVED_SECTORS as u16).to_le_bytes());
        sector[16] = NUM_FATS as u8;
        // Root entry count and the 16-bit sector counts are zero on FAT32;
        // that is how a driver tells the two apart.
        sector[21] = 0xF8; // fixed disk
        sector[24..26].copy_from_slice(&32u16.to_le_bytes()); // sectors per track
        sector[26..28].copy_from_slice(&8u16.to_le_bytes()); // heads
        sector[32..36].copy_from_slice(&total_sectors.to_le_bytes());
        sector[36..40].copy_from_slice(&self.fat_sectors.to_le_bytes());
        sector[44..48].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
        sector[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
        sector[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
        sector[64] = 0x80; // drive number
        sector[66] = 0x29; // extended boot signature
        sector[67..71].copy_from_slice(&0xFE22_1000u32.to_le_bytes()); // volume id
        sector[71..82].copy_from_slice(b"FERRIX     ");
        sector[82..90].copy_from_slice(b"FAT32   ");
        sector[510] = 0x55;
        sector[511] = 0xAA;

        if let Some(slot) = self.bytes.get_mut(0..SECTOR) {
            slot.copy_from_slice(&sector);
        }
        // The backup at sector 6, which firmware falls back to.
        if let Some(slot) = self.bytes.get_mut(6 * SECTOR..7 * SECTOR) {
            slot.copy_from_slice(&sector);
        }
    }

    fn write_fs_info(&mut self) {
        let mut sector = [0u8; SECTOR];
        sector[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        sector[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        // Free count and next-free hint are advisory; 0xFFFFFFFF means unknown,
        // which is honest and saves keeping them correct as files are added.
        sector[488..492].copy_from_slice(&u32::MAX.to_le_bytes());
        sector[492..496].copy_from_slice(&u32::MAX.to_le_bytes());
        sector[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());

        if let Some(slot) = self.bytes.get_mut(SECTOR..2 * SECTOR) {
            slot.copy_from_slice(&sector);
        }
    }
}

/// Choose cluster size and FAT size for a disk of `total_sectors`.
///
/// The two depend on each other: a bigger allocation table leaves fewer data
/// clusters, and fewer clusters need a smaller table. Iterating "recompute the
/// table from the cluster count" does *not* converge — it oscillates between
/// two adjacent sizes forever, which is how this was first written and why
/// every test failed at once.
///
/// So it works downwards instead. Start from the most clusters that could
/// possibly fit, ask what tables they would need, and if that does not fit,
/// give back exactly the shortfall. Each step strictly decreases the cluster
/// count, so it terminates, and it lands on the largest count that fits.
fn geometry(total_sectors: u32) -> Result<(u32, u32, u32)> {
    for sectors_per_cluster in [1u32, 2, 4, 8, 16, 32, 64] {
        let Some(available) = total_sectors.checked_sub(RESERVED_SECTORS) else {
            continue;
        };

        let mut clusters = available / sectors_per_cluster;
        while clusters > 0 {
            // Two reserved entries at the head of every FAT.
            let fat_sectors = ((clusters + 2) * 4).div_ceil(SECTOR as u32);
            let needed = NUM_FATS * fat_sectors + clusters * sectors_per_cluster;

            let Some(shortfall) = needed.checked_sub(available) else {
                if clusters >= MIN_FAT32_CLUSTERS {
                    return Ok((sectors_per_cluster, fat_sectors, clusters));
                }
                // A larger cluster gives fewer clusters, never more, so no
                // later cluster size can reach the minimum either.
                break;
            };
            if shortfall == 0 {
                if clusters >= MIN_FAT32_CLUSTERS {
                    return Ok((sectors_per_cluster, fat_sectors, clusters));
                }
                break;
            }
            clusters -= shortfall.div_ceil(sectors_per_cluster).max(1);
        }
    }

    Err(Error::new(format!(
        "cannot lay out a FAT32 filesystem in {total_sectors} sectors; \
         FAT32 needs at least {MIN_FAT32_CLUSTERS} clusters"
    )))
}

/// Convert `name` to the padded 8.3 form a directory entry stores.
fn short_name(name: &str) -> Result<[u8; 11]> {
    let mut out = [b' '; 11];

    // `.` and `..` are stored literally, left-aligned.
    if name == "." || name == ".." {
        for (slot, byte) in out.iter_mut().zip(name.bytes()) {
            *slot = byte;
        }
        return Ok(out);
    }

    let (stem, extension) = name.split_once('.').unwrap_or((name, ""));
    if stem.is_empty() || stem.len() > 8 || extension.len() > 3 {
        return Err(Error::new(format!(
            "`{name}` is not an 8.3 name; the boot image only writes short names"
        )));
    }

    for (slot, byte) in out.iter_mut().zip(stem.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
    for (slot, byte) in out.iter_mut().skip(8).zip(extension.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
    Ok(out)
}

/// Lay out one 32-byte directory entry.
fn directory_entry(name: &[u8; 11], attr: u8, first_cluster: u32, size: u32) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(name);
    entry[11] = attr;
    entry[14..16].copy_from_slice(&FIXED_TIME.to_le_bytes()); // creation
    entry[16..18].copy_from_slice(&FIXED_DATE.to_le_bytes());
    entry[18..20].copy_from_slice(&FIXED_DATE.to_le_bytes()); // last access
    entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    entry[22..24].copy_from_slice(&FIXED_TIME.to_le_bytes()); // last write
    entry[24..26].copy_from_slice(&FIXED_DATE.to_le_bytes());
    entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

/// Where an image's own command-line options go, and `flash` puts them on a
/// card: the loader reads the file after `CMDLINE.TXT`.
pub(crate) const DEFAULTS_PATH: &str = "FERRIX/DEFAULTS.TXT";

/// Whether this run's images carry the stripped kernel: `--strip-kernel`.
static STRIP_KERNEL: AtomicBool = AtomicBool::new(false);

/// Say whether every image this run writes carries the kernel as `flash`
/// strips it: `main` calls it once, with what `--strip-kernel` said.
///
/// Off by default, because an image is otherwise the bytes that were built.
/// On, the loader's copy of the file is some 6 MB rather than 70 to 130 MB --
/// it reads the whole ELF into memory before placing its segments -- which
/// is what lets `--memory` go below about 256 MiB at all. The ELF with
/// everything in it stays beside the image for resolving a panic's addresses.
pub(crate) fn set_strip_kernel(strip: bool) {
    STRIP_KERNEL.store(strip, Ordering::Relaxed);
}

/// Assemble the bootable image for `arch` from a built loader and kernel, with
/// the initramfs every image carries: the tree's native programs, and no
/// `--init` program.
pub(crate) fn write_image(
    arch: Arch,
    loader: &Path,
    kernel: &crate::cargo::Kernel,
    natives: &[crate::native::Built],
    cmdline: Option<&str>,
) -> Result<PathBuf> {
    write_image_with(
        arch,
        loader,
        kernel,
        &crate::initramfs::build(None, natives, None, &[])?,
        cmdline,
    )
}

/// [`write_image`], carrying `initramfs` instead: the same archive with a
/// program and its applet links added, for `test-vfs` and for `build` or `run`
/// given one.
pub(crate) fn write_image_with(
    arch: Arch,
    loader: &Path,
    kernel: &crate::cargo::Kernel,
    initramfs: &[u8],
    cmdline: Option<&str>,
) -> Result<PathBuf> {
    write_image_carrying(arch, loader, kernel, initramfs, cmdline, None)
}

/// [`write_image_with`], and the image's own options in `FERRIX/DEFAULTS.TXT`
/// when there are any: the file `flash` writes beside a card owner's
/// `CMDLINE.TXT`, which the loader appends after it (`src/boot/common/uefi/src/main.rs`).
///
/// The initramfs is `initramfs` with `kernel`'s init inputs added, which is
/// where pid 1's program, script and commands are carried.
pub(crate) fn write_image_carrying(
    arch: Arch,
    loader: &Path,
    kernel: &crate::cargo::Kernel,
    initramfs: &[u8],
    cmdline: Option<&str>,
    defaults: Option<&str>,
) -> Result<PathBuf> {
    let mut fs = Fat32::new(IMAGE_BYTES)?;
    let carried = kernel.initramfs(initramfs)?;
    let initramfs = carried.as_slice();
    let kernel: &Path = kernel;

    let loader_bytes = std::fs::read(loader)
        .map_err(|error| Error::new(format!("reading {}: {error}", loader.display())))?;
    let stripped;
    let kernel = if STRIP_KERNEL.load(Ordering::Relaxed) {
        stripped = crate::flash::stripped_kernel(kernel);
        stripped.as_path()
    } else {
        kernel
    };
    let kernel_bytes = std::fs::read(kernel)
        .map_err(|error| Error::new(format!("reading {}: {error}", kernel.display())))?;

    // The removable-media path, so the image boots with no firmware boot entry.
    fs.add_file(
        &format!("EFI/BOOT/{}", arch.removable_boot_name()),
        &loader_bytes,
    )?;
    fs.add_file("FERRIX/KERNEL.ELF", &kernel_bytes)?;
    // Beside the kernel, where the loader looks for it. Every image carries
    // one, because stage 8's self-check reads its files back through the VFS.
    fs.add_file("FERRIX/INITRD.IMG", initramfs)?;
    // A kernel command line, when the caller has one: the loader reads it
    // from beside the kernel and hands it over in the boot info.
    if let Some(cmdline) = cmdline {
        fs.add_file("FERRIX/CMDLINE.TXT", cmdline.as_bytes())?;
    }
    if let Some(defaults) = defaults {
        fs.add_file(DEFAULTS_PATH, defaults.as_bytes())?;
    }

    let directory = paths::build_dir(arch);
    std::fs::create_dir_all(&directory)?;
    let image = directory.join("ferrix.img");
    std::fs::write(&image, fs.finish())?;

    println!(
        "  image {} ({} KiB loader, {} KiB kernel, {} KiB initramfs)",
        image.display(),
        loader_bytes.len() / 1024,
        kernel_bytes.len() / 1024,
        initramfs.len().div_ceil(1024)
    );
    Ok(image)
}

#[cfg(test)]
mod tests;
