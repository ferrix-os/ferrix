//! The btrfs test disk: the `none` fixture image `tools/common/gen/gen-btrfs-fixtures.py`
//! made with real `mkfs.btrfs`, unpacked into a raw image every QEMU machine
//! carries as a second `virtio-blk-pci` device, for stage 11's exit: the
//! kernel mounts it through the ring-3 driver and reads its tree back against
//! the fixture's manifest.
//!
//! The fixture is committed packed — only its non-zero 4 KiB blocks, each
//! behind its 8-byte little-endian offset — because a 128 MiB image of mostly
//! zeros has no business in a repository. Unpacking writes each block at its
//! offset into a file of the image's full size, so the file is sparse where
//! the host's filesystem allows and zeros where it does not; either way the
//! device reads what `mkfs.btrfs` wrote. The image is rewritten whenever it is
//! missing, the wrong size, or its first block differs from the fixture's, so
//! a regenerated fixture reaches the next boot without a step of its own.
//!
//! Attached read-only, after the pattern disk, so the pattern disk stays
//! `vda` and this one is `vdb`, in the order devmgr and the boot check name
//! them. Behind it comes a third: a fresh copy of the `blank` fixture, an
//! empty volume, attached writable as `vdc` for stage 12's check to write on
//! and for host `btrfs check` to read afterwards. That one is rewritten for
//! every boot, because the boot before wrote on it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::paths::{self, Arch};
use crate::{Error, Result};

/// The packed fixture, as `src/lib/fs/btrfs/format`'s tests read it.
const PACKED: &[u8] =
    include_bytes!("../../../../src/lib/fs/btrfs/format/testdata/none.img.packed");

/// The blank fixture: an empty volume with `mkfs.btrfs`'s default profiles
/// and features, which stage 12's check writes on.
const BLANK: &[u8] =
    include_bytes!("../../../../src/lib/fs/btrfs/format/testdata/blank.img.packed");

/// The same empty volume at [`ROOT_SIZE`]: the root disk starts from it.
const ROOT: &[u8] = include_bytes!("../../../../src/lib/fs/btrfs/format/testdata/root.img.packed");

/// How big the root disk is: the system is tens of megabytes, and btrfs
/// keeps two copies of its metadata.
const ROOT_SIZE: u64 = 1024 * 1024 * 1024;

/// The writable image's name under `build/<arch>/`: one an architecture, so
/// that the architectures' boots can run at once (`crate::parallel`).
const BLANK_FILE_NAME: &str = "btrfs-write.img";

/// Bytes in a packed block, and in the image's blocks.
const BLOCK: usize = 4096;

/// A packed record: the block's offset, then the block.
const RECORD: usize = 8 + BLOCK;

/// The image's size: what `gen-btrfs-fixtures.py` makes the filesystem on.
pub(crate) const IMAGE_SIZE: u64 = 128 * 1024 * 1024;

/// The image's name under `build/`.
const FILE_NAME: &str = "btrfs-test.img";

/// The image on disk, unpacked if it is not current.
///
/// # Errors
///
/// The fixture being malformed, or the file not writable.
pub(crate) fn ensure() -> Result<PathBuf> {
    let directory = paths::workspace_root().join("build");
    let path = directory.join(FILE_NAME);
    if is_current(&path) {
        return Ok(path);
    }
    std::fs::create_dir_all(&directory)?;
    let partial = directory.join(format!("{FILE_NAME}.{}.partial", std::process::id()));
    write(&partial).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        Error::new(format!("writing {}: {error}", partial.display()))
    })?;
    std::fs::rename(&partial, &path)
        .map_err(|error| Error::new(format!("renaming to {}: {error}", path.display())))?;
    Ok(path)
}

/// A fresh copy of the blank fixture, for stage 12's check on `arch` to write
/// on.
///
/// Rewritten for every boot, never reused: the boot before wrote on it, and
/// a check that starts from a volume another run left behind is checking
/// something nobody chose. Its path is also what `btrfs check` is pointed at
/// afterwards. The one exception is [`keep_blank`]: `test-powerfail`'s
/// second boot must see what the first one's crash left.
///
/// # Errors
///
/// The fixture being malformed, or the file not writable.
pub(crate) fn ensure_blank(arch: Arch) -> Result<PathBuf> {
    let directory = paths::build_dir(arch);
    let path = directory.join(BLANK_FILE_NAME);
    if KEEP_BLANK.load(Ordering::Relaxed) && path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(&directory)?;
    let partial = directory.join(format!("{BLANK_FILE_NAME}.{}.partial", std::process::id()));
    write_packed(&partial, BLANK).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        Error::new(format!("writing {}: {error}", partial.display()))
    })?;
    std::fs::rename(&partial, &path)
        .map_err(|error| Error::new(format!("renaming to {}: {error}", path.display())))?;
    Ok(path)
}

/// Write the blank fixture back over the `vdc` [`ensure_blank`] attached, in
/// the file QEMU holds open: for a gate that resets one QEMU, as a warm
/// reset does, so the next boot's stage 12 check starts from the fixture as
/// every boot's does (`test-log-pstore`). Only while the guest is paused.
///
/// # Errors
///
/// The fixture being malformed, or the disk not writable.
pub(crate) fn rewrite_blank_in_place(arch: Arch) -> Result<()> {
    use std::io::Write as _;
    let directory = paths::build_dir(arch);
    let path = directory.join(BLANK_FILE_NAME);
    let partial = directory.join(format!("{BLANK_FILE_NAME}.{}.again", std::process::id()));
    write_packed(&partial, BLANK)
        .map_err(|error| Error::new(format!("writing {}: {error}", partial.display())))?;
    let bytes = std::fs::read(&partial);
    let _ = std::fs::remove_file(&partial);
    let bytes =
        bytes.map_err(|error| Error::new(format!("reading {}: {error}", partial.display())))?;
    let mut disk = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|error| Error::new(format!("opening {}: {error}", path.display())))?;
    disk.write_all(&bytes)
        .and_then(|()| disk.set_len(bytes.len() as u64))
        .and_then(|()| disk.sync_all())
        .map_err(|error| Error::new(format!("rewriting {}: {error}", path.display())))
}

/// A fresh copy of the blank fixture at `build/<arch>/<name>`, apart from the one
/// [`ensure_blank`] attaches as `vdc`: a volume for a test to attach at
/// `/data` and read back on a later boot (`init_file`'s `reboot(2)` check).
///
/// # Errors
///
/// The fixture being malformed, or the file not writable.
pub(crate) fn blank_copy(arch: Arch, name: &str) -> Result<PathBuf> {
    let directory = paths::build_dir(arch);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(name);
    write_packed(&path, BLANK)
        .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))?;
    Ok(path)
}

/// A fresh copy of the home fixture under `build/<arch>/` as `name`, for a
/// test boot that keeps files across two boots on it (`test-shell`'s K7).
///
/// # Errors
///
/// The file not writable.
pub(crate) fn home_copy(arch: Arch, name: &str) -> Result<PathBuf> {
    let directory = paths::build_dir(arch);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(name);
    write_packed_sized(&path, HOME, HOME_SIZE)
        .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))?;
    Ok(path)
}

/// The packed home fixture: an empty volume labelled `ferrix-home`, which
/// the kernel mounts at `/home`.
const HOME: &[u8] = include_bytes!("../../../../src/lib/fs/btrfs/format/testdata/home.img.packed");

/// The home image's size: what `mkfs.btrfs` made the fixture at.
const HOME_SIZE: u64 = 8 * 1024 * 1024 * 1024;

/// The home image's name under `build/`: `/home` in an interactive boot.
const HOME_FILE_NAME: &str = "home.img";

/// The root image's name under `build/`: `/` in an interactive boot.
const ROOT_FILE_NAME: &str = "root.img";

/// The root disk: `build/root.img`, made from the `root` fixture if it is not
/// there or `reset` asks, and otherwise left exactly as the last boot left
/// it. The kernel installs the system on it at the first boot. Answers its
/// path and whether it was made new.
///
/// # Errors
///
/// The file not writable.
pub(crate) fn ensure_root(reset: bool) -> Result<(PathBuf, bool)> {
    let directory = paths::workspace_root().join("build");
    let path = directory.join(ROOT_FILE_NAME);
    if !reset && path.is_file() {
        return Ok((path, false));
    }
    std::fs::create_dir_all(&directory)?;
    let partial = directory.join(format!("{ROOT_FILE_NAME}.{}.partial", std::process::id()));
    write_packed_sized(&partial, ROOT, ROOT_SIZE).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        Error::new(format!("writing {}: {error}", partial.display()))
    })?;
    std::fs::rename(&partial, &path)
        .map_err(|error| Error::new(format!("renaming to {}: {error}", path.display())))?;
    Ok((path, true))
}

/// The home disk: `build/home.img`, made from the `home` fixture if it is
/// not there or `reset` asks (`--reset-flash`), and otherwise left exactly as
/// the last boot left it -- `--reset-root` does not touch it. Answers its
/// path and whether it was made new.
///
/// # Errors
///
/// The file not writable.
pub(crate) fn ensure_home(reset: bool) -> Result<(PathBuf, bool)> {
    let directory = paths::workspace_root().join("build");
    let path = directory.join(HOME_FILE_NAME);
    if !reset && path.is_file() {
        return Ok((path, false));
    }
    std::fs::create_dir_all(&directory)?;
    let partial = directory.join(format!("{HOME_FILE_NAME}.{}.partial", std::process::id()));
    write_packed_sized(&partial, HOME, HOME_SIZE).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        Error::new(format!("writing {}: {error}", partial.display()))
    })?;
    std::fs::rename(&partial, &path)
        .map_err(|error| Error::new(format!("renaming to {}: {error}", path.display())))?;
    Ok((path, true))
}

/// The root image a test boots: `build/<arch>/root-test.img`, made fresh from
/// the `root` fixture for every boot, apart from `run`'s, which keeps what a
/// person put on it. One an architecture, so that the architectures' tests
/// can run at once.
///
/// # Errors
///
/// The file not writable.
pub(crate) fn ensure_test_root(arch: Arch) -> Result<PathBuf> {
    let directory = paths::build_dir(arch);
    std::fs::create_dir_all(&directory)?;
    let path = test_root_path(arch);
    let partial = directory.join(format!(
        "{TEST_ROOT_FILE_NAME}.{}.partial",
        std::process::id()
    ));
    write_packed_sized(&partial, ROOT, ROOT_SIZE).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        Error::new(format!("writing {}: {error}", partial.display()))
    })?;
    std::fs::rename(&partial, &path)
        .map_err(|error| Error::new(format!("renaming to {}: {error}", path.display())))?;
    Ok(path)
}

/// Where [`ensure_test_root`] puts `arch`'s, without writing it.
pub(crate) fn test_root_path(arch: Arch) -> PathBuf {
    paths::build_dir(arch).join(TEST_ROOT_FILE_NAME)
}

/// The test root's name under `build/<arch>/`.
const TEST_ROOT_FILE_NAME: &str = "root-test.img";

/// Whether the next boots attach the writable image as the last one left it.
static KEEP_BLANK: AtomicBool = AtomicBool::new(false);

/// Attach the writable image as the last boot left it (`true`), or fresh
/// from the fixture (`false`, the default), from now on.
pub(crate) fn keep_blank(keep: bool) {
    KEEP_BLANK.store(keep, Ordering::Relaxed);
}

/// Where `arch`'s writable image is, without writing it.
pub(crate) fn blank_path(arch: Arch) -> PathBuf {
    paths::build_dir(arch).join(BLANK_FILE_NAME)
}

/// The writable image, in one line, for a command that attaches it.
pub(crate) fn describe_blank() -> String {
    format!(
        "a fresh empty volume from src/lib/fs/btrfs/format/testdata/blank.img.packed, {} non-zero blocks of \
         {BLOCK} bytes in a {} MiB image made by mkfs.btrfs with its default profiles",
        BLANK.len() / RECORD,
        IMAGE_SIZE / (1024 * 1024),
    )
}

/// The layout, in one line, for the output of a command that attaches the
/// disk.
pub(crate) fn describe() -> String {
    format!(
        "the none fixture from src/lib/fs/btrfs/format/testdata, {} non-zero blocks of {BLOCK} bytes in a \
         {} MiB image made by mkfs.btrfs",
        PACKED.len() / RECORD,
        IMAGE_SIZE / (1024 * 1024),
    )
}

/// The packed records, as `(offset, block)`.
fn records() -> impl Iterator<Item = (u64, &'static [u8])> {
    PACKED.chunks_exact(RECORD).map(|record| {
        let (offset, block) = record.split_at(8);
        let offset = u64::from_le_bytes(offset.try_into().unwrap_or([0; 8]));
        (offset, block)
    })
}

/// Write the image: full size, each packed block at its offset.
fn write(path: &Path) -> std::io::Result<()> {
    write_packed(path, PACKED)
}

/// Write `packed`'s blocks into an image of the fixture's full size.
fn write_packed(path: &Path, packed: &[u8]) -> std::io::Result<()> {
    write_packed_sized(path, packed, IMAGE_SIZE)
}

/// [`write_packed`], into an image of `size` bytes.
fn write_packed_sized(path: &Path, packed: &[u8], size: u64) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    file.set_len(size)?;
    for record in packed.chunks_exact(RECORD) {
        let (offset, block) = record.split_at(8);
        let offset = u64::from_le_bytes(offset.try_into().unwrap_or([0; 8]));
        let _ = file.seek(SeekFrom::Start(offset))?;
        file.write_all(block)?;
    }
    file.sync_all()
}

/// Whether the image on disk is the fixture: the right size, and its first
/// packed block where the fixture puts it.
fn is_current(path: &Path) -> bool {
    let check = || -> std::io::Result<bool> {
        let mut file = File::open(path)?;
        if file.metadata()?.len() != IMAGE_SIZE {
            return Ok(false);
        }
        let Some((offset, block)) = records().next() else {
            return Ok(false);
        };
        let _ = file.seek(SeekFrom::Start(offset))?;
        let mut held = vec![0; BLOCK];
        file.read_exact(&mut held)?;
        Ok(held == block)
    };
    check().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_is_whole_records_inside_the_image() {
        assert_eq!(PACKED.len() % RECORD, 0, "whole records");
        assert!(records().count() > 0, "some blocks");
        for (offset, block) in records() {
            assert_eq!(offset % BLOCK as u64, 0, "block-aligned offset {offset:#x}");
            assert!(offset + BLOCK as u64 <= IMAGE_SIZE, "inside the image");
            assert_eq!(block.len(), BLOCK);
        }
    }

    #[test]
    fn the_superblock_is_where_btrfs_keeps_it() {
        // btrfs's primary superblock is at 64 KiB and starts with its magic
        // at offset 64: `_BHRfS_M`.
        let super_block = records()
            .find(|(offset, _)| *offset == 0x10000)
            .expect("the fixture has a superblock block");
        assert_eq!(&super_block.1[64..72], b"_BHRfS_M");
    }
}
