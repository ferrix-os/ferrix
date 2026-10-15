//! The N150 box (`docs/N150.md`): a whole-disk image for its NVMe drive, and
//! the QEMU boot that runs that image as the box would.
//!
//! * `n150-install` writes `build/x86_64/n150-disk.img` (or `--disk`), a
//!   regular file the size of the box's disk (`--disk-bytes`, 512 GB by
//!   default), sparse: a GUID partition table whose first partition is an
//!   EFI system partition holding the image `build` makes (or `--image`),
//!   with the loader at the firmware's fallback path, and whose second is
//!   the rest of the disk, a btrfs volume labelled `ferrix-root` made here by
//!   btrfs-progs at the partition's full size, with a program tree
//!   (`--tree`, by default the Proton launcher volume's) at `/data`. The
//!   kernel finds the root by its label on any partition and installs the
//!   system on it at the first boot, as on every disk; `/data` is a plain
//!   directory of the root, so the programs that expect their volume there
//!   find it, and the game's download shares the volume's space. Getting the
//!   file onto the box's drive is the customer's step (`docs/N150.md` §4);
//!   this tool never opens a device.
//! * `test-n150` writes the same disk, through the same function
//!   ([`make_disk`]), with a small fixture tree, and boots it under OVMF as
//!   the machine's one boot disk: an NVMe drive on a root port, first in the
//!   boot order, with no AHCI disk and q35's standard VGA as the only
//!   display, the firmware's GOP framebuffer the box will have. The system's
//!   shell reads the fixture's marker from `/data`, writes a file beside it
//!   and syncs; a second power cycle must find the file. Then the host runs
//!   `btrfs check` over the root partition.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use ferrix_partition::{ESP, Guid, LINUX_FILESYSTEM, Partition, usable, write_table};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{
    Error, Result, btrfs_check, cargo, fat, initramfs, native, qemu, sha256, uutils, zinc,
};

#[cfg(test)]
mod tests;

/// Bytes in a sector of the disk.
const SECTOR: u64 = 512;
/// Sectors in a MiB: partitions start and end on MiB boundaries.
const MIB_SECTORS: u64 = (1 << 20) / SECTOR;
/// The usual size of a 512 GB NVMe drive: 1,000,215,216 sectors, the IDEMA
/// size every vendor sells as "512 GB". `--disk-bytes` gives the real one,
/// `blockdev --getsize64` on the box.
const DEFAULT_DISK_BYTES: u64 = 1_000_215_216 * SECTOR;
/// The label the kernel finds its root by (`src/kernel/src/fs/root_disk.rs`).
const ROOT_LABEL: &str = "ferrix-root";
/// The features asked of `mkfs.btrfs`, named so the log and the doc can say
/// them: btrfs-progs 6.x's defaults, which every volume Ferrix has mounted
/// writable was made with.
const MKFS_FEATURES: &str = "no-holes,free-space-tree";
/// The partitions' names.
const ESP_NAME: &str = "ferrix-esp";
const ROOT_NAME: &str = "ferrix-root";

/// `test-n150`'s disk: small, sparse, and the same layout.
const TEST_DISK_BYTES: u64 = 4 << 30;
/// Where the fixture's marker is, under the tree (so at `/data/` on the box).
const MARKER_FILE: &str = "n150/marker";
/// What the marker holds.
const MARKER_TEXT: &str = "ferrix-n150-fixture";
/// `test-n150`'s negative control (c): when true, the disk is written
/// again before the second power cycle, which must then find nothing kept.
const REWRITE_BEFORE_SECOND_BOOT: bool = false;
/// How long each boot may take, unless `--timeout` says: the first installs
/// the system on the root through a ring-3 NVMe driver.
const BOOT_SECONDS: u64 = 600;

/// The shell script each `test-n150` boot runs as the system's first
/// program, on the btrfs root.
const SCRIPT: &str = "read token < /data/n150/marker\n\
echo \"n150: marker $token\"\n\
if read kept < /data/n150/kept; then echo \"n150: found $kept\"; else echo \"n150: nothing kept yet\"; fi\n\
echo n150-kept-by-the-first-boot > /data/n150/kept\n\
sync\n\
echo \"n150: kept written and synced\"\n\
exit 0\n";

/// Where a disk's two partitions are, in sectors, inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    /// The disk's size in sectors.
    pub(crate) sectors: u64,
    /// The EFI system partition: from 1 MiB, the image volume's size.
    pub(crate) esp: (u64, u64),
    /// The root: from the next MiB to the last whole MiB before the backup
    /// table.
    pub(crate) root: (u64, u64),
}

impl Layout {
    /// The root partition's offset and length in bytes.
    pub(crate) fn root_bytes(&self) -> (u64, u64) {
        (
            self.root.0 * SECTOR,
            (self.root.1 + 1 - self.root.0) * SECTOR,
        )
    }
}

/// The layout of a disk of `disk_bytes` whose ESP holds a volume of
/// `esp_bytes`. Refuses a disk that leaves the root under 1 GiB.
pub(crate) fn layout(disk_bytes: u64, esp_bytes: u64) -> Result<Layout> {
    if !disk_bytes.is_multiple_of(SECTOR) || esp_bytes == 0 || !esp_bytes.is_multiple_of(SECTOR) {
        return Err(Error::new(format!(
            "n150: a disk of {disk_bytes} bytes with an ESP of {esp_bytes}: not whole sectors"
        )));
    }
    let sectors = disk_bytes / SECTOR;
    let (_, last_usable) = usable(sectors, SECTOR as u32)
        .map_err(|error| Error::new(format!("n150: a disk of {disk_bytes} bytes: {error}")))?;
    let esp = (MIB_SECTORS, MIB_SECTORS + esp_bytes / SECTOR - 1);
    let root_first = (esp.1 + 1).next_multiple_of(MIB_SECTORS);
    let root_last = ((last_usable + 1) / MIB_SECTORS * MIB_SECTORS).saturating_sub(1);
    if root_last < root_first || (root_last + 1 - root_first) < (1 << 30) / SECTOR {
        return Err(Error::new(format!(
            "n150: a disk of {disk_bytes} bytes leaves the root under 1 GiB"
        )));
    }
    Ok(Layout {
        sectors,
        esp,
        root: (root_first, root_last),
    })
}

/// `n150-install`, `test-n150`, `test-n150-sata` and `test-n150-desktop`;
/// any other name is refused, since main.rs hands over every `test-n150*`.
pub(crate) fn run(command: &str, args: &Args) -> Result<()> {
    if args.arches()?.iter().any(|&arch| arch != Arch::X86_64) {
        return Err(Error::new(format!(
            "{command} is x86-64 only: the N150 box is an x86-64 PC (docs/N150.md)"
        )));
    }
    match command {
        "test-n150" => test(args, false),
        "test-n150-sata" => test(args, true),
        "test-n150-desktop" => test_desktop(args),
        "n150-install" => install(args),
        _ => Err(Error::new(format!("unknown command {command}"))),
    }
}

/// The tree put at `/data`: `--tree`, or the Proton launcher volume's.
fn launcher_tree(args: &Args) -> Result<PathBuf> {
    let tree = match &args.n150.tree {
        Some(tree) => tree.clone(),
        None => paths::volume_directory("endfield-launcher-proton")?.join("tree"),
    };
    if !tree.is_dir() {
        return Err(Error::new(format!(
            "n150: no tree at {}: `tools/common/fetch/fetch-proton.sh`, then \
             `FERRIX_ENDFIELD_WINE=proton tools/common/fetch/fetch-endfield-launcher.sh` make it \
             (docs/WINE.md §10), or name one with --tree",
            tree.display()
        )));
    }
    Ok(tree)
}

/// `n150-install`: see the module documentation.
fn install(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    let tree = launcher_tree(args)?;
    let image = match &args.n150.image {
        Some(image) => image.clone(),
        None => crate::compositor::n150_box::image(args)?.0,
    };
    let disk = args
        .n150
        .disk
        .clone()
        .unwrap_or_else(|| paths::build_dir(arch).join("n150-disk.img"));
    let bytes = args.n150.disk_bytes.unwrap_or(DEFAULT_DISK_BYTES);
    let layout = make_disk(&disk, &image, Some(&tree), bytes)?;
    let (_, root_len) = layout.root_bytes();
    println!(
        "  n150: {} written: {} bytes, the ESP from {}, `{ROOT_LABEL}` {} GiB with {} at /data",
        disk.display(),
        bytes,
        image.display(),
        root_len >> 30,
        tree.display()
    );
    println!(
        "  n150: to the box (docs/N150.md §4; it ERASES the box's whole disk -- pick the disk by \
         its model and serial):\n    \
         box$ ssh <you>@<this host> 'tar -cSf - -C {} {}' | tar -xSOf - | \
         sudo dd of=/dev/disk/by-id/<the drive's id> bs=4M oflag=direct status=progress",
        disk.parent()
            .map_or_else(|| ".".into(), |p| p.display().to_string()),
        disk.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    );
    Ok(())
}

/// Write the disk image at `out`: the GUID partition table, the ESP holding
/// the FAT volume `esp_volume`, and the root volume, made by `mkfs.btrfs`
/// at the partition's size with `tree` (when given) at `/data`. The one
/// function both `n150-install` and `test-n150` write their disks with.
///
/// `out` must be a regular file or not exist: this never opens a device.
pub(crate) fn make_disk(
    out: &Path,
    esp_volume: &Path,
    tree: Option<&Path>,
    disk_bytes: u64,
) -> Result<Layout> {
    refuse_devices(out)?;
    let esp_bytes = std::fs::metadata(esp_volume)
        .map_err(|error| Error::new(format!("{}: {error}", esp_volume.display())))?
        .len();
    let layout = layout(disk_bytes, esp_bytes)?;
    let (root_at, root_len) = layout.root_bytes();

    // The root volume, at its partition's size, in a file of its own.
    let staging = sibling(out, "staging");
    let root = sibling(out, "root");
    remove_own(&staging)?;
    remove_own(&root)?;
    if let Some(tree) = tree {
        stage(tree, &staging)?;
    }
    File::create(&root)
        .and_then(|file| file.set_len(root_len))
        .map_err(|error| Error::new(format!("{}: {error}", root.display())))?;
    let made = mkfs(&root, tree.map(|_| staging.as_path()));
    remove_own(&staging)?;
    made?;
    let total = total_bytes(&root)?;
    if total != root_len {
        return Err(Error::new(format!(
            "n150: mkfs.btrfs made a volume of {total} bytes for a partition of {root_len}"
        )));
    }
    btrfs_check::Checker::required()?.run(&root, Arch::X86_64)?;

    // The disk: the root volume moved to its partition, then the ESP and
    // the tables around it.
    place_root(&root, out, root_at)?;
    let mut disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(out)
        .map_err(|error| Error::new(format!("{}: {error}", out.display())))?;
    disk.set_len(disk_bytes)?;
    write_esp(esp_volume, &mut disk, layout.esp.0)?;
    let partitions = [
        Partition::new(ESP, guid(b"esp"), layout.esp.0, layout.esp.1, ESP_NAME),
        Partition::new(
            LINUX_FILESYSTEM,
            guid(b"root"),
            layout.root.0,
            layout.root.1,
            ROOT_NAME,
        ),
    ];
    let table = write_table(layout.sectors, SECTOR as u32, guid(b"disk"), &partitions)
        .map_err(|error| Error::new(format!("n150: the partition table: {error}")))?;
    for (lba, sector) in table {
        let _ = disk.seek(SeekFrom::Start(lba * SECTOR))?;
        disk.write_all(&sector)?;
    }
    disk.sync_all()?;
    Ok(layout)
}

/// Refuse anything at `out` but a regular file, and any path under `/dev`.
fn refuse_devices(out: &Path) -> Result<()> {
    if out.starts_with("/dev") {
        return Err(Error::new(format!(
            "n150: {} is under /dev: this writes an image file, never a device (docs/N150.md §4)",
            out.display()
        )));
    }
    match std::fs::symlink_metadata(out) {
        #[allow(
            clippy::filetype_is_file,
            reason = "everything but a regular file is refused: devices, directories, links"
        )]
        Ok(meta) if !meta.file_type().is_file() => Err(Error::new(format!(
            "n150: {} exists and is not a regular file: this writes an image file, never a \
             device (docs/N150.md §4)",
            out.display()
        ))),
        _ => Ok(()),
    }
}

/// A working path beside `out`, named after it.
fn sibling(out: &Path, what: &str) -> PathBuf {
    let name = out.file_name().map_or_else(
        || "n150-disk.img".into(),
        |n| n.to_string_lossy().into_owned(),
    );
    out.with_file_name(format!("{name}.{what}"))
}

/// Remove `path` (a file or a directory this module made), if it is there.
fn remove_own(path: &Path) -> Result<()> {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(_) => Ok(()),
    };
    removed.map_err(|error| Error::new(format!("removing {}: {error}", path.display())))
}

/// Make `staging/data` the tree: hard links when the two are on one file
/// system, which costs no space, and a copy when they are not.
fn stage(tree: &Path, staging: &Path) -> Result<()> {
    std::fs::create_dir_all(staging)?;
    let data = staging.join("data");
    let linked = Command::new("cp")
        .arg("-al")
        .arg(tree)
        .arg(&data)
        .status()
        .is_ok_and(|status| status.success());
    if linked {
        return Ok(());
    }
    remove_own(&data)?;
    let copied = Command::new("cp")
        .arg("-a")
        .arg(tree)
        .arg(&data)
        .status()
        .map_err(|error| Error::new(format!("running cp: {error}")))?;
    if !copied.success() {
        return Err(Error::new(format!(
            "n150: could not copy {} to {}",
            tree.display(),
            data.display()
        )));
    }
    Ok(())
}

/// `mkfs.btrfs` over `file`, with `rootdir` as the volume's root when given.
fn mkfs(file: &Path, rootdir: Option<&Path>) -> Result<()> {
    let version = Command::new("mkfs.btrfs")
        .arg("--version")
        .output()
        .map_err(|error| {
            Error::new(format!(
                "running mkfs.btrfs: {error}; install btrfs-progs (docs/N150.md §4)"
            ))
        })?;
    println!(
        "  n150: {} -L {ROOT_LABEL} -O {MKFS_FEATURES} -d single -m dup{}",
        String::from_utf8_lossy(&version.stdout)
            .lines()
            .next()
            .unwrap_or("mkfs.btrfs"),
        if rootdir.is_some() {
            " --rootdir <tree at /data>"
        } else {
            ""
        }
    );
    let mut command = Command::new("mkfs.btrfs");
    let _ = command.args([
        "-f",
        "-q",
        "-L",
        ROOT_LABEL,
        "-O",
        MKFS_FEATURES,
        "-d",
        "single",
        "-m",
        "dup",
    ]);
    if let Some(rootdir) = rootdir {
        let _ = command.arg("--rootdir").arg(rootdir);
    }
    let output = command
        .arg(file)
        .output()
        .map_err(|error| Error::new(format!("running mkfs.btrfs: {error}")))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "n150: mkfs.btrfs failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// The volume's size as its superblock says it.
fn total_bytes(file: &Path) -> Result<u64> {
    let output = Command::new("btrfs")
        .args(["inspect-internal", "dump-super"])
        .arg(file)
        .output()
        .map_err(|error| Error::new(format!("running btrfs inspect-internal: {error}")))?;
    parse_total_bytes(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        Error::new(format!(
            "n150: btrfs inspect-internal dump-super {} gave no total_bytes",
            file.display()
        ))
    })
}

/// `total_bytes` from `dump-super`'s output: the superblock's own line, not
/// `dev_item.total_bytes`.
pub(crate) fn parse_total_bytes(said: &str) -> Option<u64> {
    said.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("total_bytes"))
            .then(|| words.next()?.parse().ok())
            .flatten()
    })
}

/// Make `out` the root volume with `at` bytes inserted before it.
///
/// Inserting a range at the front of the file (`fallocate --insert-range`,
/// ext4 and XFS) moves no data, so a volume holding gigabytes of programs is
/// not written twice. Where the file system cannot, the volume is copied to
/// its place, sparse.
fn place_root(root: &Path, out: &Path, at: u64) -> Result<()> {
    let inserted = Command::new("fallocate")
        .args(["--insert-range", "--offset", "0", "--length"])
        .arg(at.to_string())
        .arg(root)
        .output()
        .is_ok_and(|output| output.status.success());
    if inserted {
        std::fs::rename(root, out)
            .map_err(|error| Error::new(format!("{}: {error}", out.display())))?;
        return Ok(());
    }
    println!("  n150: this file system cannot insert a range; copying the root volume");
    let mut from = File::open(root)?;
    let mut to = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .map_err(|error| Error::new(format!("{}: {error}", out.display())))?;
    copy_sparse(&mut from, &mut to, at)?;
    drop(from);
    remove_own(root)
}

/// Copy all of `from` to `to` from byte `at`, leaving runs of zeros
/// unwritten.
fn copy_sparse<R: Read, W: Write + Seek>(from: &mut R, to: &mut W, at: u64) -> Result<()> {
    let mut buffer = vec![0_u8; 1 << 20];
    let mut done = 0_u64;
    loop {
        let read = from.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        let piece = buffer.get(..read).unwrap_or_default();
        if piece.iter().any(|&byte| byte != 0) {
            let _ = to.seek(SeekFrom::Start(at + done))?;
            to.write_all(piece)?;
        }
        done += read as u64;
    }
}

/// Write the FAT volume at `volume` into the ESP from sector `first`, its
/// boot sectors told where it starts.
fn write_esp(volume: &Path, disk: &mut File, first: u64) -> Result<()> {
    let mut from =
        File::open(volume).map_err(|error| Error::new(format!("{}: {error}", volume.display())))?;
    let mut head = vec![0_u8; 1 << 20];
    let read = from.read(&mut head)?;
    head.truncate(read);
    crate::installer::hide(&mut head, first)?;
    let _ = disk.seek(SeekFrom::Start(first * SECTOR))?;
    disk.write_all(&head)?;
    copy_sparse(&mut from, disk, first * SECTOR + read as u64)
}

/// A random GUID, from the time, the process and `what`: each disk written
/// gets its own, as an installed disk does.
fn guid(what: &[u8]) -> Guid {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut seed = now.to_le_bytes().to_vec();
    seed.extend(std::process::id().to_le_bytes());
    seed.extend(what);
    let digest = sha256::digest(&seed);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(digest.get(..16).unwrap_or(&[0; 16]));
    Guid::random(bytes)
}

/// `test-n150`, and with `sata` `test-n150-sata`, whose disk is a SATA
/// drive on q35's AHCI controller: see the module documentation.
fn test(args: &Args, sata: bool) -> Result<()> {
    let arch = Arch::X86_64;
    let build = paths::build_dir(arch);
    let natives = native::build(arch, args.release)?;
    let loader = cargo::build_loader(arch, args.release)?;
    let utilities = uutils::carried(arch)?;
    let initramfs = initramfs::build_with_utilities(None, &natives, None, &utilities, &[])?;
    let zinc = zinc::built(arch)?.ok_or_else(|| Error::new("zinc is not built for x86_64"))?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &zinc, SCRIPT)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;

    let fixture = build.join("n150-fixture");
    remove_own(&fixture)?;
    let marker = fixture.join(MARKER_FILE);
    if let Some(directory) = marker.parent() {
        std::fs::create_dir_all(directory)?;
    }
    std::fs::write(&marker, format!("{MARKER_TEXT}\n"))?;
    let disk = build.join("n150-test-disk.img");
    let layout = make_disk(&disk, &image, Some(&fixture), TEST_DISK_BYTES)?;
    println!(
        "  {arch}: the box's disk written: ESP sectors {:?}, root sectors {:?}",
        layout.esp, layout.root
    );

    let mut booted = args.clone();
    booted.boot_nvme = !sata;
    booted.boot_sata = sata;
    if !booted.timeout_given {
        booted.timeout = BOOT_SECONDS;
    }
    let boot = || qemu::watch_lines(arch, &disk, &kernel, &booted, crate::shell::EXITED);
    check_first(&boot()?, sata)?;
    println!(
        "  {arch}: booted from the {} drive alone; / on its root partition, /data read and written",
        if sata { "SATA" } else { "NVMe" }
    );
    if REWRITE_BEFORE_SECOND_BOOT {
        let _ = make_disk(&disk, &image, Some(&fixture), TEST_DISK_BYTES)?;
    }
    check_second(&boot()?)?;
    println!("  {arch}: after a power cycle, what the first boot wrote was there");

    // The root partition, as btrfs-progs sees it after two boots of Ferrix.
    let copy = sibling(&disk, "root-after");
    let (at, len) = layout.root_bytes();
    {
        let mut from = File::open(&disk)?;
        let _ = from.seek(SeekFrom::Start(at))?;
        let to = File::create(&copy)?;
        to.set_len(len)?;
        let mut to = to;
        copy_sparse(&mut from.take(len), &mut to, 0)?;
    }
    let checked = btrfs_check::Checker::required()?.run(&copy, arch);
    remove_own(&copy)?;
    checked?;
    remove_own(&fixture)?;
    println!(
        "  {arch}: test-n150{} passed",
        if sata { "-sata" } else { "" }
    );
    Ok(())
}

/// `test-n150-desktop`'s disk: room for the launcher's tree (about 5 GB)
/// and the system, sparse.
const DESKTOP_TEST_DISK_BYTES: u64 = 16 << 30;
/// Megabytes of guest RAM for the desktop boot, unless `--memory` says.
const DESKTOP_MEMORY: u32 = 4096;
/// Distinct colours the screen must show: a desktop, not a cleared screen.
const DESKTOP_COLOURS: usize = 8;

/// `test-n150-desktop`: the disk `n150-install` writes by default -- the
/// box's desktop in its EFI system partition, the launcher's tree (or
/// `--tree`) at `/data` -- at a smaller size, booted under OVMF from the
/// NVMe drive alone, with a VGA as the only display (the GOP framebuffer
/// the box will have), a network, and a USB keyboard and mouse on an xHCI
/// controller. Required: `/` on the root partition
/// with the system installed, the firmware framebuffer lent to efifb,
/// hyprix on it with a keyboard in its seat, [`crate::compositor::n150_box`]'s script reporting the
/// video host blocked, the launcher on `/data`, a lease and yserver up; and
/// a screendump of the VGA that is a desktop, kept at
/// `build/x86_64/n150-desktop.ppm`.
fn test_desktop(args: &Args) -> Result<()> {
    use crate::compositor::n150_box::READY;
    use crate::display::{Qmp, free_port, parse_ppm};

    let arch = Arch::X86_64;
    let build = paths::build_dir(arch);
    let tree = launcher_tree(args)?;
    let (image, kernel) = crate::compositor::n150_box::image(args)?;
    let disk = build.join("n150-desktop-test.img");
    let layout = make_disk(&disk, &image, Some(&tree), DESKTOP_TEST_DISK_BYTES)?;
    println!(
        "  {arch}: the box's desktop disk written: ESP sectors {:?}, root sectors {:?}",
        layout.esp, layout.root
    );
    let port = free_port()?;
    let mut booted = args.clone();
    booted.boot_nvme = true;
    booted.firmware_screen = true;
    booted.net = true;
    booted.qmp_port = Some(port);
    // The box's keyboard and mouse are USB, on its xHCI: test-usb's bus,
    // with the image's own ESP volume as the stick nothing reads.
    booted.usb = Some(image);
    if !booted.memory_given {
        booted.memory = DESKTOP_MEMORY;
    }
    if !booted.timeout_given {
        booted.timeout = BOOT_SECONDS;
    }
    let dump = build.join("n150-desktop.ppm");
    let mut colours = 0;
    let lines = qemu::watch_then(arch, &disk, &kernel, &booted, READY, |watching| {
        let mut qmp = Qmp::connect(
            port,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
        )?;
        // A moment for hyprix to draw what the script's last lines changed.
        std::thread::sleep(std::time::Duration::from_secs(3));
        qmp.screendump(Some(qemu::FIRMWARE_SCREEN_ID), &dump)?;
        let bytes = std::fs::read(&dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
        colours = distinct_colours(&parse_ppm(&bytes)?.pixels);
        watching.stop_when_done();
        Ok(())
    });
    // A boot that never reached the script's last line is judged on what it
    // did print, so that the failure names what was missing.
    let lines = match lines {
        Ok(lines) => lines,
        Err(error) => {
            if let Ok(text) = std::fs::read_to_string(build.join("serial.log")) {
                let said: Vec<String> = text.lines().map(str::to_owned).collect();
                check_desktop(&said)?;
            }
            return Err(error);
        }
    };
    check_desktop(&lines)?;
    if colours < DESKTOP_COLOURS {
        return Err(Error::new(format!(
            "{arch}: the screen showed {colours} colours, not a desktop: {}",
            dump.display()
        )));
    }
    remove_own(&disk)?;
    println!(
        "  {arch}: the box's desktop on the firmware framebuffer, {colours} colours on the screen \
         ({}); test-n150-desktop passed",
        dump.display()
    );
    Ok(())
}

/// How many distinct colours an RGB picture holds, counted up to 4096.
pub(crate) fn distinct_colours(pixels: &[u8]) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    let (rgb, _) = pixels.as_chunks::<3>();
    for pixel in rgb {
        let _ = seen.insert(*pixel);
        if seen.len() >= 4096 {
            break;
        }
    }
    seen.len()
}

/// What the desktop boot must print.
pub(crate) fn check_desktop(lines: &[String]) -> Result<()> {
    require(
        lines,
        &[("NVMe 1.", "the NVMe driver did not bring the drive up")],
    )?;
    on_root_partition(lines)?;
    require(
        lines,
        &[
            (
                "the system was installed on it",
                "the first boot did not install the system on the root",
            ),
            (
                "lent to a driver",
                "the kernel did not lend the firmware framebuffer (ferrix.efifb)",
            ),
            ("efifb: ", "efifb did not start on the firmware framebuffer"),
            ("hyprix: card0", "hyprix found no card to draw on"),
            (
                "n150-desktop: the launcher's video host",
                "the launcher's video host is not blocked in /etc/hosts",
            ),
            (
                "n150-desktop: gryphlink is on /data",
                "the launcher's start is not on /data",
            ),
            (
                "n150-desktop: a lease for eth0",
                "the wired interface got no lease",
            ),
            ("n150-desktop: yserver is up on :0", "yserver did not start"),
        ],
    )?;
    // The keyboard in hyprix's own seat, not only the kernel's input line.
    if !lines
        .iter()
        .any(|line| line.contains("hyprix: seat") && line.contains("USB Keyboard"))
    {
        return Err(Error::new(
            "x86_64: hyprix's seat has no USB keyboard (no `hyprix: seat` line naming one); \
             its lines are in the serial log",
        ));
    }
    Ok(())
}

/// What a line list must hold: each `(text, problem)`.
fn require(lines: &[String], wanted: &[(&str, &str)]) -> Result<()> {
    for (text, problem) in wanted {
        if !lines.iter().any(|line| line.contains(text)) {
            return Err(Error::new(format!(
                "x86_64: {problem} (no `{text}`); its lines are in the serial log"
            )));
        }
    }
    Ok(())
}

/// The disk `/` was put on, from the root's line.
pub(crate) fn root_disk(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|line| {
        let (_, after) = line.split_once("/ is btrfs on ")?;
        let name: String = after
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        (!name.is_empty()).then_some(name)
    })
}

/// `/` must be on a partition numbered 2, the root of the box's layout.
fn on_root_partition(lines: &[String]) -> Result<()> {
    match root_disk(lines) {
        Some(name) if name.ends_with('2') => Ok(()),
        Some(name) => Err(Error::new(format!(
            "x86_64: / is btrfs on {name}, not the box's root partition (its second)"
        ))),
        None => Err(Error::new(
            "x86_64: the box's disk booted, but / is not on its root partition \
             (no `/ is btrfs on`); its lines are in the serial log",
        )),
    }
}

/// What the first boot must print.
pub(crate) fn check_first(lines: &[String], sata: bool) -> Result<()> {
    let up = if sata {
        (
            "serial ferrix-n150:",
            "the AHCI driver did not bring the drive up",
        )
    } else {
        ("NVMe 1.", "the NVMe driver did not bring the drive up")
    };
    require(lines, &[up])?;
    on_root_partition(lines)?;
    require(
        lines,
        &[
            (
                "the system was installed on it",
                "the first boot did not install the system on the root",
            ),
            (
                "n150: marker ferrix-n150-fixture",
                "the tree's marker was not read from /data",
            ),
            (
                "n150: nothing kept yet",
                "the first boot found a file no boot had written",
            ),
            (
                "n150: kept written and synced",
                "the first boot did not write to /data",
            ),
        ],
    )
}

/// What the second boot must print.
pub(crate) fn check_second(lines: &[String]) -> Result<()> {
    on_root_partition(lines)?;
    require(
        lines,
        &[
            (
                "as the last boot left it",
                "the second boot did not find the root the first left",
            ),
            (
                "n150: found n150-kept-by-the-first-boot",
                "the second boot did not find what the first wrote to /data",
            ),
        ],
    )
}
