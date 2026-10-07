//! The live installer (`docs/INSTALLER.md`): `ferrix-install` in the image,
//! the live medium, and `test-install`, which boots the medium, installs
//! from it and boots what it wrote.
//!
//! `build --installer` makes the MVP's live image: the usual image, a bare
//! FAT volume, whose initramfs also carries `/sbin/ferrix-install` and the
//! empty root volume it writes. `live` makes the live medium of §3.1: the
//! same files on a GUID partition table, in an EFI system partition named
//! `FERRIX-LIVE`, with `ferrix.root=tmpfs` on its command line so that the
//! live session keeps `/` in memory whatever disks the machine has. Either,
//! attached to a virtual machine as a virtio disk, boots as any image does;
//! `ferrix-install /dev/vdX` then puts it on another disk.
//!
//! `test-install`, x86-64:
//!
//! 1. The live medium is built at `build/x86_64/install-live.img`.
//! 2. It boots alone, as a virtio disk, through OVMF: the kernel must publish
//!    its partition, `vdd1`, say it was given `ferrix.root=tmpfs` from the
//!    partition's `CMDLINE.TXT`, and reach the boot marker.
//! 3. A boot whose shell runs `ferrix-install` has the medium as `vdd` and a
//!    blank 2 GiB disk as `vde`. The installer must refuse `/dev/vdd`, the
//!    disk the live system is on, then find the live system on `vdd1` by
//!    itself and install on `/dev/vde`.
//! 4. A second boot starts from that disk alone, as a virtio disk, through
//!    OVMF: the kernel must publish its partitions, find `ferrix-root` on
//!    `vdd2`, install the system on it and reach the boot marker. That needs
//!    the installer to have turned the medium's `ferrix.root=tmpfs` into
//!    `ferrix.root=btrfs` on the installed disk's EFI system partition.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ferrix_partition::{ESP, Guid, Partition, write_table};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, fat, initramfs, native, ports, qemu, zinc};

/// Where the installer goes in the initramfs.
const PROGRAM_PATH: &str = "sbin/ferrix-install";
/// Where the empty root volume goes.
const ROOT_PATH: &str = "usr/share/ferrix/root.img.packed";
/// The empty root volume, packed as `src/lib/fs/btrfs/testdata` keeps it.
const ROOT_PACKED: &[u8] = include_bytes!("../../../../src/lib/fs/btrfs/testdata/root.img.packed");
/// The target disk's size.
const TARGET_BYTES: u64 = 2 << 30;

/// The live medium's EFI system partition's name, by which `ferrix-install`
/// knows the live system's disk (`docs/INSTALLER.md` §3.1).
const LIVE_NAME: &str = "FERRIX-LIVE";
/// The live session's command line: `/` in memory, so that booting the
/// medium never installs onto, or mounts, a disk the machine already has.
/// `ferrix-install` rewrites it on the disk it installs (§4.2 step 5).
const LIVE_CMDLINE: &str = "ferrix.root=tmpfs";
/// Where the live medium's partition starts: 1 MiB, as every partitioning
/// tool aligns the first one.
const LIVE_FIRST: u64 = 2048;
/// Bytes in a sector of the medium.
const SECTOR: u64 = 512;
/// The medium's disk and partition GUIDs. Fixed, so that two builds of one
/// tree write the same image, as release media of one version all carry
/// the same ones; an installed disk gets random ones.
const LIVE_DISK_GUID: Guid =
    Guid::from_fields(0x7a3d_6f52, 0x2c1e, 0x4d0b, 0x9e41, 0x4652_5249_584c);
const LIVE_PART_GUID: Guid =
    Guid::from_fields(0x7a3d_6f52, 0x2c1e, 0x4d0b, 0x9e42, 0x4652_5249_584c);

/// What the install boot's shell runs: first an install onto the disk the
/// live system is on, which must be refused, then the real one, with the
/// live system found by the installer itself.
const SCRIPT: &str = "/sbin/ferrix-install --yes /dev/vdd\n\
echo \"install: on the live disk, exited $?\"\n\
/sbin/ferrix-install --yes /dev/vde\n\
echo \"install: exited $?\"\n\
exit 0\n";

/// What the live medium's boot must print: its partition, published.
const PUBLISHED: &str = "disks    vdd1 is a partition";
/// What it must print of its command line: the loader read the medium's
/// `CMDLINE.TXT` from the partition. Spelled out, not built from
/// [`LIVE_CMDLINE`], so that a medium written with another line fails.
const KEPT_IN_MEMORY: &str = "cmdline  ferrix.root=tmpfs  (from /FERRIX/CMDLINE.TXT)";
/// What the installer must say when asked to install on the live disk.
const REFUSED: &str = "ferrix-install: /dev/vdd holds /dev/vdd1, the live system";
/// What the installer must say it found by itself.
const FOUND: &str = "ferrix-install: the live system is on /dev/vdd1";
/// What the install boot must print.
const DONE: &str = "ferrix-install: done.";
/// What the installed disk's boot must print.
const ROOTED: &str = "/ is btrfs on vdd2; the system was installed on it";

/// The installer and its root volume, as files for the initramfs, or `None`
/// on an architecture the installer is not built for.
pub(crate) fn files(arch: Arch) -> Result<Option<Vec<ports::File>>> {
    let Some(target) = zinc::target(arch) else {
        println!("  ferrix-install is not built for {} yet", arch.name());
        return Ok(None);
    };
    println!("  building ferrix-install for {target}");
    let target_dir = paths::target_dir().join("installer");
    let program = target_dir
        .join(target)
        .join("release")
        .join("ferrix-install");
    crate::builds::Build::cargo(
        format!("cargo build (ferrix-install) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/installer"),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .output(&program)
    .run()?;
    let bytes = std::fs::read(&program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    Ok(Some(vec![
        ports::File {
            path: PROGRAM_PATH.to_owned(),
            mode: 0o755,
            content: ports::Content::Bytes(bytes),
        },
        ports::File {
            path: ROOT_PATH.to_owned(),
            mode: 0o644,
            content: ports::Content::Bytes(ROOT_PACKED.to_vec()),
        },
    ]))
}

/// `live`: write `build/<arch>/ferrix-live.img`, the live medium, for each
/// architecture asked (x86-64 and AArch64, §3.2).
pub(crate) fn live(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if arch == Arch::Armv7a {
            return Err(Error::new(
                "live: ARMv7-A boards keep `flash`; the live medium is for x86-64 and AArch64 \
                 (docs/INSTALLER.md §3.2)",
            ));
        }
        let carried = files(arch)?
            .ok_or_else(|| Error::new(format!("ferrix-install is not built for {arch}")))?;
        let natives = native::build(arch, args.release)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let kernel = cargo::build_kernel(arch, args.release)?;
        let medium = paths::build_dir(arch).join("ferrix-live.img");
        write_medium(arch, &loader, &kernel, &initramfs, &medium)?;
    }
    Ok(())
}

/// Write the live medium for `arch` at `path`.
fn write_medium(
    arch: Arch,
    loader: &Path,
    kernel: &cargo::Kernel,
    initramfs: &[u8],
    path: &Path,
) -> Result<()> {
    let volume_path = fat::write_image_with(arch, loader, kernel, initramfs, Some(LIVE_CMDLINE))?;
    let on = |error: std::io::Error| Error::new(format!("{}: {error}", path.display()));
    let mut volume = File::open(&volume_path)
        .map_err(|error| Error::new(format!("{}: {error}", volume_path.display())))?;
    let bytes = volume
        .metadata()
        .map_err(|error| Error::new(format!("{}: {error}", volume_path.display())))?
        .len();
    let mut medium = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(on)?;
    let sectors = lay_out(&mut volume, bytes, &mut medium)?;
    medium.sync_all().map_err(on)?;
    println!(
        "  live medium {} ({} MiB: a GUID partition table, and the EFI system partition \
         {LIVE_NAME} at sector {LIVE_FIRST} holding the image with `{LIVE_CMDLINE}`)",
        path.display(),
        (sectors * SECTOR) >> 20
    );
    Ok(())
}

/// Lay the FAT volume `volume`, of `bytes` bytes, out as the live medium on
/// `medium`: a GUID partition table whose one partition, the EFI system
/// partition [`LIVE_NAME`], holds the volume from sector [`LIVE_FIRST`].
/// Returns the medium's size in sectors.
///
/// Sectors of the volume that are all zeros are not written, so a medium in
/// a file stays as sparse as the volume was.
fn lay_out<R: Read, W: Write + Seek>(volume: &mut R, bytes: u64, medium: &mut W) -> Result<u64> {
    if bytes == 0 || !bytes.is_multiple_of(SECTOR) {
        return Err(Error::new(format!(
            "the live volume is {bytes} bytes, not whole sectors"
        )));
    }
    let last = LIVE_FIRST + bytes / SECTOR - 1;
    // The partition, then the backup table's 33 sectors, rounded up to a MiB.
    let sectors = (last + 1 + 33).next_multiple_of(LIVE_FIRST);
    let partition = Partition::new(ESP, LIVE_PART_GUID, LIVE_FIRST, last, LIVE_NAME);
    let table = write_table(sectors, SECTOR as u32, LIVE_DISK_GUID, &[partition])
        .map_err(|error| Error::new(format!("the live medium's partition table: {error}")))?;

    let mut buffer = vec![0_u8; 1 << 20];
    let mut done = 0_u64;
    while done < bytes {
        let len = usize::try_from((bytes - done).min(buffer.len() as u64))
            .map_err(|_| Error::new("a chunk larger than memory"))?;
        let piece = buffer
            .get_mut(..len)
            .ok_or_else(|| Error::new("a chunk larger than its buffer"))?;
        volume.read_exact(piece)?;
        if done == 0 {
            hide(piece, LIVE_FIRST)?;
        }
        if piece.iter().any(|&byte| byte != 0) {
            let _ = medium.seek(SeekFrom::Start(LIVE_FIRST * SECTOR + done))?;
            medium.write_all(piece)?;
        }
        done += len as u64;
    }
    // The tables last: the backup header is the medium's last sector, so
    // writing it gives the medium its full length.
    for (lba, sector) in table {
        let _ = medium.seek(SeekFrom::Start(lba * SECTOR))?;
        medium.write_all(&sector)?;
    }
    Ok(sectors)
}

/// Tell a FAT32 volume's boot sector, and its backup, where the volume
/// starts on its disk: the BPB's hidden sectors, which `xtask` writes as 0
/// for an image that is a volume alone.
fn hide(volume: &mut [u8], first: u64) -> Result<()> {
    let hidden = u32::try_from(first)
        .map_err(|_| Error::new("a partition past FAT32's hidden-sector field"))?
        .to_le_bytes();
    let backup = volume
        .get(50..52)
        .and_then(|field| field.try_into().ok())
        .map(|field| usize::from(u16::from_le_bytes(field)) * SECTOR as usize)
        .ok_or_else(|| Error::new("the live volume has no boot sector"))?;
    for at in [0, backup] {
        volume
            .get_mut(at + 28..at + 32)
            .ok_or_else(|| Error::new("the live volume's backup boot sector is missing"))?
            .copy_from_slice(&hidden);
    }
    Ok(())
}

/// `test-install`: see the module documentation.
pub(crate) fn test_install(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-install runs on x86-64 only for now: the installed disk boots as a \
             virtio-blk-pci disk through OVMF",
        ));
    }
    let carried = files(arch)?.ok_or_else(|| Error::new("ferrix-install is not built"))?;
    let natives = native::build(arch, args.release)?;
    let loader = cargo::build_loader(arch, args.release)?;
    let initramfs = initramfs::build(None, &natives, None, &carried)?;

    // The live medium, as a person would boot it.
    let kernel = cargo::build_kernel(arch, args.release)?;
    let build = paths::build_dir(arch);
    let live = build.join("install-live.img");
    write_medium(arch, &loader, &kernel, &initramfs, &live)?;
    let mut booted = args.clone();
    booted.boot_virtio = true;
    let lines = qemu::test_boot_lines(arch, &live, &kernel, &booted)?;
    for (wanted, problem) in [
        (
            PUBLISHED,
            "the live medium booted, but the kernel did not publish its partition",
        ),
        (
            KEPT_IN_MEMORY,
            "the live medium booted without its command line's tmpfs root",
        ),
    ] {
        if !lines.iter().any(|line| line.contains(wanted)) {
            return Err(Error::new(format!("{arch}: {problem} (no `{wanted}`)")));
        }
    }
    println!("  {arch}: the live medium booted from its partition table, / in memory");

    let target = build.join("install-target.img");
    blank(&target)?;

    // The install: the same system, with a shell running the installer.
    let zinc = zinc::built(arch)?.ok_or_else(|| Error::new("zinc is not built for x86_64"))?;
    let scripted = cargo::build_kernel_with_init(arch, args.release, &zinc, SCRIPT)?;
    let image = fat::write_image_with(arch, &loader, &scripted, &initramfs, None)?;
    let mut install = args.clone();
    install.install_disks = Some((live, target.clone()));
    let lines = qemu::watch_lines(arch, &image, &scripted, &install, crate::shell::EXITED)?;
    // In this order, so that each failure names its own cause: without the
    // live system found, neither the refusal nor the install happens.
    for (wanted, problem) in [
        (
            FOUND,
            "the installer did not find the live system on its partition",
        ),
        (
            REFUSED,
            "the installer did not refuse the disk the live system is on",
        ),
        (DONE, "the installer did not finish"),
    ] {
        if !lines.iter().any(|line| line.contains(wanted)) {
            return Err(Error::new(format!(
                "{arch}: {problem} (no `{wanted}`); its lines are in the serial log"
            )));
        }
    }
    println!("  {arch}: installed on {}", target.display());

    // The installed disk, on its own.
    let mut installed = args.clone();
    installed.boot_virtio = true;
    let lines = qemu::test_boot_lines(arch, &target, &kernel, &installed)?;
    if !lines.iter().any(|line| line.contains(ROOTED)) {
        return Err(Error::new(format!(
            "{arch}: the installed disk booted, but did not put / on its root partition \
             (no `{ROOTED}`)"
        )));
    }
    println!("  {arch}: the installed disk booted with / on its root partition");
    Ok(())
}

/// A fresh sparse blank disk at `path`.
fn blank(path: &PathBuf) -> Result<()> {
    let file =
        File::create(path).map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    file.set_len(TARGET_BYTES)
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))
}

#[cfg(test)]
mod tests;
