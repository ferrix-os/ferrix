//! A system slot's contents (`docs/AB-BOOT.md` §2) as files: loading the
//! kernel, initramfs and command line from an image, writing them into a
//! slot partition, and verifying a slot the way the loader does.
//!
//! The format is `ferrix-slots`'; this is its file I/O.

use std::fs::File;
use std::path::Path;

use ferrix_slots::slot::{HEADER_LEN, MAGIC, MAX_CMDLINE, SlotHeader, VERSION_1};

use crate::disk;
use crate::fat::Fat;

/// What a slot holds.
pub struct Image {
    pub kernel: Vec<u8>,
    pub initrd: Vec<u8>,
    pub cmdline: Vec<u8>,
    /// Where it came from, said in a few words.
    pub from: String,
}

impl Image {
    /// The header for these blobs.
    #[must_use]
    pub fn header(&self, generation: u64, label: &str) -> SlotHeader {
        SlotHeader::lay_out(
            VERSION_1,
            generation,
            label,
            &self.kernel,
            &self.initrd,
            &self.cmdline,
        )
    }
}

/// Load an image from `path`, which is one of:
///
/// * a slot image (`cargo xtask slot-image`), verified as the loader would;
/// * a FAT32 volume as `xtask` writes it (`build/<arch>/ferrix.img`), whose
///   `FERRIX/KERNEL.ELF`, `INITRD.IMG`, `CMDLINE.TXT` and `DEFAULTS.TXT`
///   are taken, the command line being the two files' words in that order,
///   as the loader joins them;
/// * a directory holding those files.
///
/// # Errors
///
/// What is missing or wrong.
pub fn load(path: &str) -> Result<Image, String> {
    let on = |e: std::io::Error| format!("{path}: {e}");
    if Path::new(path).is_dir() {
        let read = |name: &str| std::fs::read(Path::new(path).join(name));
        let kernel = read("KERNEL.ELF").map_err(|e| format!("{path}/KERNEL.ELF: {e}"))?;
        let initrd = read("INITRD.IMG").unwrap_or_default();
        let cmdline = join(read("CMDLINE.TXT").ok(), read("DEFAULTS.TXT").ok());
        return checked(Image {
            kernel,
            initrd,
            cmdline,
            from: format!("the directory {path}"),
        });
    }
    let mut file = disk::open(path).map_err(on)?;
    let mut head = vec![0_u8; HEADER_LEN];
    let size = disk::size(&mut file).map_err(on)?;
    if size < HEADER_LEN as u64 {
        return Err(format!("{path} is too short to be an image"));
    }
    disk::read_at(&mut file, 0, &mut head).map_err(on)?;
    if head[..8] == MAGIC {
        let header = SlotHeader::decode(&head, size)
            .map_err(|why| format!("{path}: the slot header does not verify: {why:?}"))?;
        let mut blobs = Vec::new();
        for (name, blob) in ["kernel", "initramfs", "command line"]
            .iter()
            .zip(header.blobs())
        {
            let mut bytes = vec![0_u8; usize::try_from(blob.length).map_err(|e| e.to_string())?];
            disk::read_at(&mut file, blob.offset, &mut bytes).map_err(on)?;
            if !blob.matches(&bytes) {
                return Err(format!(
                    "{path}: its {name} does not match its header's SHA-256"
                ));
            }
            blobs.push(bytes);
        }
        let cmdline = blobs.pop().unwrap_or_default();
        let initrd = blobs.pop().unwrap_or_default();
        let kernel = blobs.pop().unwrap_or_default();
        return checked(Image {
            kernel,
            initrd,
            cmdline,
            from: format!("the slot image {path} ({})", header.label_text()),
        });
    }
    if head.get(3..11) == Some(b"FERRIX  ") && head.get(82..90) == Some(b"FAT32   ") {
        let fat = Fat::read(&mut file, 0).map_err(on)?;
        let mut take = |name: &[u8; 11]| -> Result<Option<Vec<u8>>, String> {
            match fat
                .open_path(&mut file, &[b"FERRIX     ", name])
                .map_err(on)?
            {
                Some(entry) => fat.read_file(&mut file, &entry).map(Some).map_err(on),
                None => Ok(None),
            }
        };
        let kernel =
            take(b"KERNEL  ELF")?.ok_or_else(|| format!("{path} has no FERRIX/KERNEL.ELF"))?;
        let initrd = take(b"INITRD  IMG")?.unwrap_or_default();
        let cmdline = join(take(b"CMDLINE TXT")?, take(b"DEFAULTSTXT")?);
        return checked(Image {
            kernel,
            initrd,
            cmdline,
            from: format!("the FAT image {path}"),
        });
    }
    Err(format!(
        "{path} is not a slot image, an xtask FAT image or a directory with KERNEL.ELF"
    ))
}

/// The words of a command line and a defaults file, in that order.
fn join(cmdline: Option<Vec<u8>>, defaults: Option<Vec<u8>>) -> Vec<u8> {
    let words: Vec<String> = [cmdline, defaults]
        .into_iter()
        .flatten()
        .flat_map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    words.join(" ").into_bytes()
}

fn checked(image: Image) -> Result<Image, String> {
    if image.kernel.is_empty() {
        return Err(format!("{}: the kernel is empty", image.from));
    }
    if image.cmdline.len() as u64 > MAX_CMDLINE || std::str::from_utf8(&image.cmdline).is_err() {
        return Err(format!(
            "{}: the command line is not UTF-8 of at most {MAX_CMDLINE} bytes",
            image.from
        ));
    }
    Ok(image)
}

/// Write `image` into the slot partition that starts `base` bytes into the
/// file `slot` (0 for a partition's own node) and has `bytes` bytes:
/// the old header zeroed first, so a write cut short leaves a slot that
/// does not verify rather than an old header over new blobs; then the
/// blobs; then the header; then a flush. Returns the header written.
///
/// # Errors
///
/// An image larger than the slot, or a write error.
pub fn write(
    slot: &mut File,
    base: u64,
    bytes: u64,
    image: &Image,
    generation: u64,
    label: &str,
) -> Result<SlotHeader, String> {
    let header = image.header(generation, label);
    if header.needed() > bytes {
        return Err(format!(
            "the image needs {} MiB; the slot has {} MiB",
            header.needed() >> 20,
            bytes >> 20
        ));
    }
    let e = |e: std::io::Error| e.to_string();
    disk::write_at(slot, base, &[0_u8; HEADER_LEN]).map_err(e)?;
    slot.sync_all().map_err(e)?;
    for (blob, data) in header
        .blobs()
        .iter()
        .zip([&image.kernel, &image.initrd, &image.cmdline])
    {
        disk::write_at(slot, base + blob.offset, data).map_err(e)?;
    }
    slot.sync_all().map_err(e)?;
    disk::write_at(slot, base, &header.encode()).map_err(e)?;
    slot.sync_all().map_err(e)?;
    Ok(header)
}

/// Verify the slot partition `base` bytes into `slot`, of `bytes` bytes, as the
/// loader does: its header, then each blob's SHA-256 over the bytes read.
///
/// # Errors
///
/// Which check failed.
pub fn verify(slot: &mut File, base: u64, bytes: u64) -> Result<SlotHeader, String> {
    let mut head = vec![0_u8; HEADER_LEN];
    disk::read_at(slot, base, &mut head).map_err(|e| e.to_string())?;
    let header = SlotHeader::decode(&head, bytes).map_err(|why| format!("its header: {why:?}"))?;
    for (name, blob) in ["kernel", "initramfs", "command line"]
        .iter()
        .zip(header.blobs())
    {
        let mut data = vec![0_u8; usize::try_from(blob.length).map_err(|e| e.to_string())?];
        disk::read_at(slot, base + blob.offset, &mut data).map_err(|e| e.to_string())?;
        if !blob.matches(&data) {
            return Err(format!("its {name} does not match the header's SHA-256"));
        }
    }
    Ok(header)
}
