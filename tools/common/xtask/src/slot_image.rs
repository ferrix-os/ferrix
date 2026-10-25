//! System slot images (`docs/AB-BOOT.md` §2): the kernel, initramfs and
//! command line an image carries, in the slot format `bootctl try` writes
//! and the loader boots.
//!
//! `cargo xtask slot-image` is `build` that also writes
//! `build/<arch>/ferrix-slot.img` beside `ferrix.img`, from the same bytes:
//! so "rebuild Ferrix inside Ferrix" is `cargo xtask slot-image` and then
//! `bootctl try build/x86_64/ferrix-slot.img` (`bootctl` takes
//! `ferrix.img` itself too).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};

use ferrix_slots::slot::{HEADER_LEN, SlotHeader, VERSION_1, VERSION_2};

use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "build",
        help_at: 10,
        help: &["build         Compile the loader and kernel and write a bootable image"],
        run: build,
    },
    Task {
        name: "slot-image",
        help_at: 20,
        help: &[
            "slot-image    build, and also write build/<arch>/ferrix-slot.img: the image's kernel, initramfs and command line",
            "              as a system slot, for `bootctl try` (docs/AB-BOOT.md)",
        ],
        run: build,
    },
    Task {
        name: "signed-slot-image",
        help_at: 21,
        help: &[
            "signed-slot-image  slot-image as header version 2 (docs/SIGNING.md §1.3), its manifest block left",
            "              for the signer, and build/<arch>/ferrix-slot.payload, the 4064 bytes it signs",
        ],
        run: build,
    },
];

/// The header version of the slot image written beside every image this
/// run, or 0 for none.
static WANTED: AtomicU16 = AtomicU16::new(0);

/// `build`, and with `slot-image` the slot image beside each image (header
/// version 1), with `signed-slot-image` a version-2 one to be signed.
pub(crate) fn build(command: &str, args: &crate::args::Args) -> Result<()> {
    let version = match command {
        "slot-image" => VERSION_1,
        "signed-slot-image" => VERSION_2,
        _ => 0,
    };
    WANTED.store(version, Ordering::Relaxed);
    super::build(args)
}

/// Write the slot image for `arch` beside its `ferrix.img` when
/// `slot-image` or `signed-slot-image` asked for one ([`build`]); the command line is `cmdline`'s words then
/// `defaults`', as the loader joins the two files.
pub(crate) fn write_if_wanted(
    arch: Arch,
    kernel: &[u8],
    initramfs: &[u8],
    cmdline: Option<&str>,
    defaults: Option<&str>,
) -> Result<Option<PathBuf>> {
    let version = WANTED.load(Ordering::Relaxed);
    if version == 0 {
        return Ok(None);
    }
    let words: Vec<&str> = [cmdline, defaults]
        .into_iter()
        .flatten()
        .flat_map(str::split_whitespace)
        .collect();
    let path = paths::build_dir(arch).join("ferrix-slot.img");
    let header = write(
        &path,
        version,
        &label(arch),
        kernel,
        initramfs,
        words.join(" ").as_bytes(),
    )?;
    if version == VERSION_2 {
        // What a slot manifest signs: header bytes 0..4064, the seal's
        // input (docs/SIGNING.md §1.2), for the signer's --payload.
        let payload = paths::build_dir(arch).join("ferrix-slot.payload");
        let encoded = header.encode();
        std::fs::write(&payload, encoded.get(..HEADER_LEN - 32).unwrap_or_default())
            .map_err(|error| Error::new(format!("{}: {error}", payload.display())))?;
    }
    Ok(Some(path))
}

/// Write a slot image of these blobs at `path`, header `version`; a
/// version-2 image's manifest block is left zero for the signer.
pub(crate) fn write(
    path: &Path,
    version: u16,
    label: &str,
    kernel: &[u8],
    initramfs: &[u8],
    cmdline: &[u8],
) -> Result<SlotHeader> {
    let header = SlotHeader::lay_out(version, 0, label, kernel, initramfs, cmdline);
    let size = usize::try_from(header.needed())
        .map_err(|_| Error::new("a slot image larger than memory"))?;
    let mut bytes = vec![0_u8; size];
    for (blob, data) in header.blobs().iter().zip([kernel, initramfs, cmdline]) {
        let at = usize::try_from(blob.offset).map_err(|_| Error::new("a blob past memory"))?;
        bytes
            .get_mut(at..at + data.len())
            .ok_or_else(|| Error::new("a blob outside its slot image"))?
            .copy_from_slice(data);
    }
    let encoded = header.encode();
    bytes
        .get_mut(..encoded.len())
        .ok_or_else(|| Error::new("a slot image shorter than its header"))?
        .copy_from_slice(&encoded);
    // The image must verify as the loader checks it.
    let _ = SlotHeader::decode(&bytes, header.needed())
        .map_err(|why| Error::new(format!("the slot image does not verify: {why:?}")))?;
    std::fs::write(path, &bytes)
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    println!(
        "  slot image {} ({} KiB, `{label}`, command line `{}`)",
        path.display(),
        bytes.len() / 1024,
        String::from_utf8_lossy(cmdline)
    );
    Ok(header)
}

/// `ferrix <arch> <commit>`, or without the commit outside a checkout.
fn label(arch: Arch) -> String {
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .current_dir(paths::workspace_root())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default();
    format!("ferrix {} {commit}", arch.name()).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_image_verifies_and_holds_its_blobs() {
        let dir = std::env::temp_dir().join(format!("slot-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("slot.img");
        let _ = write(
            &path,
            VERSION_1,
            "ferrix test",
            &[1; 5000],
            &[2; 9000],
            b"ferrix.root=btrfs",
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let header = SlotHeader::decode(&bytes, bytes.len() as u64).unwrap();
        let cmdline = header.cmdline;
        let at = usize::try_from(cmdline.offset).unwrap();
        assert_eq!(&bytes[at..at + 17], b"ferrix.root=btrfs");
        assert_eq!(header.label_text(), "ferrix test");
        // Version 2: the blobs after the manifest block, which stays zero.
        let signed = write(
            &path,
            VERSION_2,
            "ferrix test",
            &[1; 5000],
            &[2; 9000],
            b"x",
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(signed.kernel.offset, 8192);
        assert!(bytes[4096..8192].iter().all(|&b| b == 0));
        assert_eq!(SlotHeader::decode(&bytes, bytes.len() as u64), Ok(signed));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
