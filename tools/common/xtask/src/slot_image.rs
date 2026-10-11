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
use std::sync::atomic::{AtomicBool, Ordering};

use ferrix_slots::slot::SlotHeader;

use crate::paths::{self, Arch};
use crate::{Error, Result};

/// Whether images written this run get a slot image beside them.
static WANTED: AtomicBool = AtomicBool::new(false);

/// Have every image this run writes get a slot image beside it.
pub(crate) fn set_wanted(wanted: bool) {
    WANTED.store(wanted, Ordering::Relaxed);
}

/// Write the slot image for `arch` beside its `ferrix.img` when
/// [`set_wanted`] asked for one; the command line is `cmdline`'s words then
/// `defaults`', as the loader joins the two files.
pub(crate) fn write_if_wanted(
    arch: Arch,
    kernel: &[u8],
    initramfs: &[u8],
    cmdline: Option<&str>,
    defaults: Option<&str>,
) -> Result<Option<PathBuf>> {
    if !WANTED.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let words: Vec<&str> = [cmdline, defaults]
        .into_iter()
        .flatten()
        .flat_map(str::split_whitespace)
        .collect();
    let path = paths::build_dir(arch).join("ferrix-slot.img");
    write(
        &path,
        &label(arch),
        kernel,
        initramfs,
        words.join(" ").as_bytes(),
    )?;
    Ok(Some(path))
}

/// Write a slot image of these blobs at `path`.
pub(crate) fn write(
    path: &Path,
    label: &str,
    kernel: &[u8],
    initramfs: &[u8],
    cmdline: &[u8],
) -> Result<()> {
    let header = SlotHeader::lay_out(0, label, kernel, initramfs, cmdline);
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
    Ok(())
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
        write(
            &path,
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
