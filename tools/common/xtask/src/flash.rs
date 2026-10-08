//! Putting the loader and the kernel onto a board's boot partition.
//!
//! # Why this copies files instead of writing an image
//!
//! `cargo xtask build` produces a FAT filesystem with no partition table,
//! which is exactly right for QEMU — firmware there is handed the whole thing
//! as a disk — and exactly wrong for this board. An STM32MP157 boots from ST's
//! own chain: the ROM loads TF-A from a partition it finds by name, TF-A loads
//! OP-TEE and U-Boot, and only then is there anything that can read a
//! filesystem. Writing our image over the card would remove all of it, and the
//! board would stop booting entirely rather than boot the wrong kernel.
//!
//! So the card keeps its vendor layout, and this copies the loader, the kernel
//! and the initramfs onto the FAT partition U-Boot already looks at — the same
//! paths the image contains, because firmware looks for them in the same places.

use std::path::{Path, PathBuf};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result};

/// Where the loader goes, less the architecture's file name.
const BOOT_DIRECTORY: &str = "EFI/BOOT";

/// Where the kernel goes, as the loader looks for it.
const KERNEL_PATH: &str = "FERRIX/KERNEL.ELF";

/// Where the initramfs goes, beside the kernel.
const INITRD_PATH: &str = "FERRIX/INITRD.IMG";

/// The kernel as the card gets it, and as an image does under
/// `--strip-kernel` (`fat::set_strip_kernel`): without its debug information
/// or its symbol table, which the loader never reads and the kernel never looks at
/// -- neither is in a loaded segment, and a panic prints addresses, which are
/// resolved on the host against the ELF `build` leaves beside the image,
/// which keeps all of it (`src/lib/platform/elf/src/symbols.rs` is `xtask`'s reader).
///
/// It matters for room and for time. A debug kernel is some 70 MB, the
/// board's `bootfs` is 128 MiB, and with the compositor's initramfs beside it
/// the two no longer fit; stripped, the kernel is a tenth of that. And the
/// loader reads the card at some 16 MB/s under U-Boot: the desktop's kernel
/// was 10.5 MB without its debug information and 8.7 MB without its symbols
/// too (2026-09-24), a tenth of a second of every boot. The rust toolchain's
/// own `llvm-objcopy` (the `llvm-tools` component `rust-toolchain.toml` asks
/// for) does it; without one the whole kernel is copied and a line says so.
///
/// Except that a kernel which keeps its relocations beside the sections they
/// patch -- the ARMv7-A kernel, linked with `--emit-relocs` so the loader can
/// move it (KASLR) -- loses only its debug information: `--strip-all` would
/// take the relocations with the symbol table they index, and the kernel,
/// built to move, would refuse to start at its link address. That keeps
/// about 1 MB of relocations and 2 MB of symbols on the card.
pub(crate) fn stripped_kernel(kernel: &Path) -> PathBuf {
    let stripped = kernel.with_extension("stripped.elf");
    let Some(objcopy) = llvm_objcopy() else {
        println!("    llvm-objcopy not found; copying the kernel with its debug information");
        return kernel.to_path_buf();
    };
    let how = if keeps_relocations(kernel) {
        "--strip-debug"
    } else {
        "--strip-all"
    };
    let status = std::process::Command::new(&objcopy)
        .arg(how)
        .arg(kernel)
        .arg(&stripped)
        .status();
    match status {
        Ok(status) if status.success() => stripped,
        _ => {
            println!(
                "    {} could not strip the kernel; copying it whole",
                objcopy.display()
            );
            kernel.to_path_buf()
        }
    }
}

/// Whether `kernel` is a fixed-address image that keeps the relocations the
/// loader moves it with, which a full strip would remove.
fn keeps_relocations(kernel: &Path) -> bool {
    std::fs::read(kernel).is_ok_and(|bytes| {
        ferrix_elf::Elf::parse(&bytes)
            .is_ok_and(|elf| elf.header().elf_type == ferrix_elf::ET_EXEC && elf.is_relocatable())
    })
}

/// The initramfs as the card gets it: every program in it without its debug
/// information or symbol table, for [`stripped_kernel`]'s reasons.
///
/// The drivers and `devmgr` are built in the tree's `dev` profile, with
/// debug information, and were 13 MB of the desktop's archive that stripped
/// come to 0.5 MB; the compositor's programs keep symbols that are
/// another 2.9 MB (2026-09-24). A fault in one of them prints its `pc`, which
/// resolves against the program the build left on the host, as a kernel
/// panic's addresses do. Only the card's copy is stripped: images, and the
/// archive every test boots, stay the bytes they were built as.
fn card_initramfs(arch: Arch, archive: &[u8]) -> Result<Vec<u8>> {
    let Some(objcopy) = llvm_objcopy() else {
        println!("    llvm-objcopy not found; the initramfs's programs keep their symbols");
        return Ok(archive.to_vec());
    };
    let scratch = paths::build_dir(arch).join("card-programs");
    std::fs::create_dir_all(&scratch)
        .map_err(|error| Error::new(format!("creating {}: {error}", scratch.display())))?;
    let (mut programs, mut before, mut after) = (0usize, 0usize, 0usize);
    let stripped = crate::initramfs::with_files_changed(archive, |name, data| {
        // MEASUREMENT ONLY (os07/ferrix-posix): the board bench's programs go
        // as B5 built them, and the busybox as Alpine built it: the bytes
        // Linux's card carries.
        if !data.starts_with(b"\x7fELF")
            || name.contains("opt/posixbench/")
            || name.trim_start_matches("./") == "bin/busybox"
        {
            return Ok(None);
        }
        let whole = scratch.join("whole");
        let less = scratch.join("stripped");
        std::fs::write(&whole, data)
            .map_err(|error| Error::new(format!("writing {}: {error}", whole.display())))?;
        let status = std::process::Command::new(&objcopy)
            .arg("--strip-all")
            .arg(&whole)
            .arg(&less)
            .status();
        if !status.is_ok_and(|status| status.success()) {
            println!(
                "    {} could not strip /{name}; it goes whole",
                objcopy.display()
            );
            return Ok(None);
        }
        let bytes = std::fs::read(&less)
            .map_err(|error| Error::new(format!("reading {}: {error}", less.display())))?;
        programs += 1;
        before += data.len();
        after += bytes.len();
        Ok(Some(bytes))
    })?;
    println!(
        "    {programs} programs in the initramfs stripped, {} KiB to {} KiB",
        before / 1024,
        after / 1024
    );
    Ok(stripped)
}

/// The toolchain's `llvm-objcopy`: in `lib/rustlib/<host>/bin` of the
/// sysroot `rustc` reports, as rustup installs `llvm-tools`.
fn llvm_objcopy() -> Option<PathBuf> {
    let rustc = |arg: &str| {
        std::process::Command::new("rustc")
            .arg(arg)
            .current_dir(paths::workspace_root())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let sysroot = rustc("--print=sysroot")?;
    let version = rustc("-vV")?;
    let host = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))?
        .trim()
        .to_owned();
    let name = if cfg!(windows) {
        "llvm-objcopy.exe"
    } else {
        "llvm-objcopy"
    };
    let path = Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(host)
        .join("bin")
        .join(name);
    path.is_file().then_some(path)
}

/// What `flash` puts on a card.
pub(crate) struct BoardFiles {
    /// The loader, which goes to `EFI/BOOT` under the architecture's name.
    pub(crate) loader: PathBuf,
    /// The kernel as built, with its debug information: [`stripped_kernel`]
    /// strips the copy the card gets, and a panic's addresses are resolved
    /// against this one.
    pub(crate) kernel: crate::cargo::Kernel,
    /// The archive the loader hands over, byte for byte what an image
    /// carries: [`card_initramfs`] strips the programs in the card's copy.
    pub(crate) initramfs: Vec<u8>,
    /// The image's own command-line options, for `FERRIX/DEFAULTS.TXT`, or
    /// none: `flash --compositor`'s `ferrix.checks=skip`.
    pub(crate) defaults: Option<&'static str>,
}

/// Copy a freshly built loader, kernel and initramfs onto the card, and the
/// image's own options beside them.
pub(crate) fn run(arch: Arch, files: &BoardFiles, args: &Args) -> Result<()> {
    let BoardFiles {
        loader,
        kernel,
        initramfs,
        defaults,
    } = files;
    // A staging directory is written exactly as a card would be, and none of
    // a card's checks apply to it: it is not a mounted filesystem, and the
    // only thing it can be mistaken for is itself. The builds run on one
    // machine and the board hangs off another; the second copies the
    // directory onto the card.
    let staged = args.stage.as_deref().map(PathBuf::from);
    let target = match (staged.as_ref(), args.to.as_deref()) {
        (Some(stage), _) => {
            std::fs::create_dir_all(stage)
                .map_err(|error| Error::new(format!("creating {}: {error}", stage.display())))?;
            stage.clone()
        }
        (None, Some(given)) => verify(Path::new(given))?,
        (None, None) => discover()?,
    };

    println!(
        "  {} {}",
        if staged.is_some() {
            "staging in"
        } else {
            "flashing to"
        },
        target.display()
    );

    let boot_name = arch.removable_boot_name();
    let loader_target = target.join(BOOT_DIRECTORY).join(boot_name);
    let kernel_target = target.join(KERNEL_PATH);

    for path in [&loader_target, &kernel_target] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Error::new(format!("creating {}: {error}", parent.display())))?;
        }
    }

    copy(loader, &loader_target)?;
    copy(&stripped_kernel(kernel), &kernel_target)?;
    write_defaults(&target, *defaults)?;
    // The same archive an image carries, pid 1's inputs included, so a board
    // unpacks what QEMU does, less the programs' symbols.
    let initramfs = card_initramfs(arch, &kernel.initramfs(initramfs)?)?;
    let initramfs_target = target.join(INITRD_PATH);
    std::fs::write(&initramfs_target, &initramfs)
        .map_err(|error| Error::new(format!("writing {}: {error}", initramfs_target.display())))?;
    flush(&initramfs_target)?;
    println!(
        "    {} ({} KiB)",
        initramfs_target.display(),
        initramfs.len() / 1024
    );

    // A card pulled from the slot with dirty pages still in the page cache is
    // a card with a truncated kernel on it, and the symptom is a loader that
    // rejects the image for reasons that have nothing to do with the build.
    // Each file was flushed as it was written; this is the rest, the
    // directories and the FAT itself, where the host has a way to ask.
    if staged.is_some() {
        println!("  staged; copy the directory's contents onto the card's boot partition");
        return Ok(());
    }
    sync();
    if cfg!(windows) {
        sync_volume(&target);
    }

    println!("  flashed; the card is safe to remove");
    Ok(())
}

/// Write the image's own options to `FERRIX/DEFAULTS.TXT`, or take away a
/// previous image's.
///
/// Rewritten by every `flash`, because the file belongs to the image and not
/// to the card: a self-check image flashed over a desktop's must not boot
/// with the desktop's `ferrix.checks=skip`. `CMDLINE.TXT`, which is the card
/// owner's, is never touched, and wins over this file where both give a key.
fn write_defaults(target: &Path, defaults: Option<&str>) -> Result<()> {
    let path = target.join(crate::fat::DEFAULTS_PATH);
    match defaults {
        Some(text) => {
            std::fs::write(&path, text)
                .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))?;
            flush(&path)?;
            println!("    {} ({})", path.display(), text.trim());
        }
        None if path.exists() => {
            std::fs::remove_file(&path)
                .map_err(|error| Error::new(format!("deleting {}: {error}", path.display())))?;
            println!("    {} deleted: this image has no defaults", path.display());
        }
        None => {}
    }
    Ok(())
}

/// Copy one file, reporting what it was.
fn copy(from: &Path, to: &Path) -> Result<()> {
    let bytes = std::fs::copy(from, to)
        .map_err(|error| Error::new(format!("writing {}: {error}", to.display())))?;
    flush(to)?;
    println!("    {} ({} KiB)", to.display(), bytes / 1024);
    Ok(())
}

/// Ask the host to put one written file on the device before returning:
/// `fsync` on Linux, `FlushFileBuffers` on Windows, through the one call `std`
/// has for both.
fn flush(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| Error::new(format!("flushing {}: {error}", path.display())))
}

/// Flush what is left of the card's metadata, so the card can be pulled.
///
/// `sync` where there is one. Windows has none, and flushes a volume's cache
/// with `Write-VolumeCache`, which wants the drive letter.
fn sync() {
    if cfg!(windows) {
        return;
    }
    if let Some(sync) = paths::which("sync") {
        let _ = std::process::Command::new(sync).status();
    }
}

/// Windows' half of [`sync`], given the drive the files went to.
fn sync_volume(target: &Path) {
    let Some(letter) = drive_letter(target) else {
        return;
    };
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("Write-VolumeCache -DriveLetter {letter}"),
        ])
        .stdin(std::process::Stdio::null())
        .status();
}

/// Check that `path` is somewhere it is safe to write a boot loader.
///
/// # Why this is not simply a copy
///
/// The argument is a path the caller typed, and the failure mode of getting it
/// wrong is not a failed build: it is files appearing in the wrong place on a
/// running system, possibly as root. So the destination has to be a mount
/// point — not merely a directory — and the filesystem mounted there has to be
/// a FAT, which is the only thing UEFI firmware reads and therefore the only
/// thing this could sensibly be. A typo that lands on an ordinary directory,
/// on the system root, or on the developer's home fails all three.
fn verify(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return Err(Error::new(format!(
            "{} is not a directory.\n  \
             Point --to at the card's mounted boot partition, not at a device node: \
             flashing writes files onto a filesystem, it does not image a disk.",
            path.display()
        )));
    }

    let canonical = plain(
        path.canonicalize()
            .map_err(|error| Error::new(format!("resolving {}: {error}", path.display())))?,
    );

    let mounts = fat_mount_points();
    if mounts.all.is_empty() && !cfg!(windows) {
        // No /proc/mounts to read: not Linux, or something unusual. Say so
        // rather than silently dropping the check that makes this safe.
        return Err(Error::new(
            "cannot read /proc/mounts, so the destination cannot be checked.\n  \
             Copy the two files by hand; `cargo xtask build --arch armv7a` says where \
             they are.",
        ));
    }

    // Refused by name, not merely left out of discovery. `/boot/efi` is a
    // mounted FAT and passes every other check here, so without this the one
    // destination that can stop this computer booting is the one destination
    // `--to` accepts without complaint. On Windows the system partition has no
    // path of its own, so it is refused by what Windows says it is.
    if starts_with_any(&canonical, &["/boot", "/efi"]) || mounts.own.contains(&canonical) {
        return Err(Error::new(format!(
            "{} is this machine's own EFI system partition, not a board's.\n  \
             Writing a boot loader there could stop this computer booting. If \
             you really mean it, copy the files by hand.",
            canonical.display()
        )));
    }

    if !mounts.all.iter().any(|mount| mount == &canonical) {
        return Err(Error::new(format!(
            "{} is not a mounted FAT filesystem.\n  \
             It must be the card's boot partition itself, mounted{}. Currently mounted \
             FAT filesystems:\n    {}",
            canonical.display(),
            if cfg!(windows) {
                " and given as its drive, like E:\\"
            } else {
                ""
            },
            describe(&mounts.all)
        )));
    }

    Ok(canonical)
}

/// The one mounted FAT filesystem, if there is exactly one.
///
/// Refuses to choose, for the same reason the serial port does: on a machine
/// with an EFI system partition mounted — which is most of them — there is
/// more than one FAT filesystem, and one of them is the one this computer
/// boots from. Writing a loader into that is a bad afternoon.
fn discover() -> Result<PathBuf> {
    let FatMounts { all, own } = fat_mount_points();
    let mounts: Vec<PathBuf> = all
        .into_iter()
        // /boot/efi is this machine's own, and never the answer.
        .filter(|mount| !starts_with_any(mount, &["/boot", "/efi"]) && !own.contains(mount))
        .collect();

    match mounts.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(Error::new(
            "no removable FAT filesystem is mounted.\n  \
             Insert the card and let the desktop mount it, or mount it by hand, then \
             pass --to <path>.",
        )),
        several => Err(Error::new(format!(
            "several FAT filesystems are mounted; say which with --to:\n    {}",
            describe(several)
        ))),
    }
}

/// The mounted FAT filesystems, and which of them belong to this machine.
struct FatMounts {
    /// Every one, by the path it is mounted at.
    all: Vec<PathBuf>,
    /// Those this computer boots from, where the host says so. Linux's are
    /// recognised by where they are mounted instead; see [`starts_with_any`].
    own: Vec<PathBuf>,
}

/// Every mounted FAT filesystem: from `/proc/mounts`, or on Windows from the
/// volumes that have a drive letter.
fn fat_mount_points() -> FatMounts {
    if cfg!(windows) {
        return windows_fat_volumes();
    }
    let Ok(text) = std::fs::read_to_string("/proc/mounts") else {
        return FatMounts {
            all: Vec::new(),
            own: Vec::new(),
        };
    };
    let all = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _device = fields.next()?;
            let mount = fields.next()?;
            let kind = fields.next()?;
            // `vfat` is what Linux calls every FAT it mounts; `msdos` is the
            // older driver, still selectable.
            (kind == "vfat" || kind == "msdos").then(|| PathBuf::from(unescape(mount)))
        })
        .collect();
    FatMounts {
        all,
        own: Vec::new(),
    }
}

/// The GPT type of an EFI system partition.
const ESP_GPT_TYPE: &str = "{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}";

/// Windows' FAT volumes with a drive letter, as `E:\`.
///
/// Through PowerShell's storage cmdlets, one line per partition with a letter
/// and a FAT filesystem: the letter, then `1` if Windows boots from it or it
/// is an EFI system partition, else `0`. A card in a reader, or a board's
/// U-Boot `ums` disk, is an ordinary partition with a letter; this machine's
/// own ESP normally has no letter at all, and is refused if someone gave it
/// one.
fn windows_fat_volumes() -> FatMounts {
    let script = format!(
        "Get-Partition | Where-Object DriveLetter | ForEach-Object {{ \
         $v = $_ | Get-Volume; \
         if ($v.FileSystemType -in 'FAT', 'FAT32') {{ \
         $own = $_.IsSystem -or $_.IsBoot -or $_.GptType -eq '{ESP_GPT_TYPE}'; \
         '{{0}} {{1}}' -f $_.DriveLetter, [int]$own }} }}"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(std::process::Stdio::null())
        .output();
    let mut mounts = FatMounts {
        all: Vec::new(),
        own: Vec::new(),
    };
    let Ok(output) = output else {
        return mounts;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [letter, own] = fields.as_slice() else {
            continue;
        };
        let Some(letter) = letter.chars().next().filter(char::is_ascii_alphabetic) else {
            continue;
        };
        let root = PathBuf::from(format!("{letter}:\\"));
        if *own == "1" {
            mounts.own.push(root.clone());
        }
        mounts.all.push(root);
    }
    mounts
}

/// `path` without the `\\?\` prefix Windows' `canonicalize` adds to a local
/// drive's path, so that it compares equal to the `E:\` a volume listing
/// gives. Unchanged anywhere else.
fn plain(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

/// The drive letter of a Windows path like `E:\`.
fn drive_letter(path: &Path) -> Option<char> {
    let text = path.to_str()?;
    let mut chars = text.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':')).then_some(letter)
}

/// Undo the octal escaping `/proc/mounts` applies to spaces and tabs.
///
/// Removable media are mounted under a label the user chose, and labels with
/// spaces in them are ordinary. Without this, such a card never matches and
/// the error says it is not mounted when it plainly is.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(byte as char);
                let _ = chars.nth(2);
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// Whether `path` is at or under any of `prefixes`.
fn starts_with_any(path: &Path, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| path.starts_with(prefix))
}

/// Mount points, one per line, for an error message.
fn describe(mounts: &[PathBuf]) -> String {
    if mounts.is_empty() {
        return "(none)".to_owned();
    }
    mounts
        .iter()
        .map(|mount| mount.display().to_string())
        .collect::<Vec<_>>()
        .join("\n    ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_the_octal_that_proc_mounts_uses() {
        assert_eq!(
            unescape("/media/johndoe/NO\\040NAME"),
            "/media/johndoe/NO NAME"
        );
        assert_eq!(unescape("/media/plain"), "/media/plain");
        // A trailing backslash is not an escape and must not be eaten.
        assert_eq!(unescape("/media/odd\\"), "/media/odd\\");
    }

    #[test]
    fn the_machines_own_efi_partition_is_refused_by_name_too() {
        // Discovery skipping it is not enough: `--to /boot/efi` names it
        // explicitly, and that is the request that has to be refused.
        let refused = verify(Path::new("/boot/efi"));
        if Path::new("/boot/efi").is_dir() {
            let message = refused.expect_err("must refuse this machine's ESP").message;
            assert!(
                message.contains("own EFI system partition"),
                "refused for the wrong reason: {message}"
            );
        }
    }

    #[test]
    fn a_windows_drive_path_loses_its_verbatim_prefix_and_keeps_its_letter() {
        assert_eq!(plain(PathBuf::from(r"\\?\E:\")), PathBuf::from(r"E:\"));
        // A share is not a drive, and is left alone.
        assert_eq!(
            plain(PathBuf::from(r"\\?\UNC\host\share")),
            PathBuf::from(r"\\?\UNC\host\share")
        );
        assert_eq!(
            plain(PathBuf::from("/media/bootfs")),
            PathBuf::from("/media/bootfs")
        );
        assert_eq!(drive_letter(Path::new(r"E:\")), Some('E'));
        assert_eq!(drive_letter(Path::new("/media/bootfs")), None);
    }

    #[test]
    fn the_machines_own_efi_partition_is_never_a_candidate() {
        assert!(starts_with_any(Path::new("/boot/efi"), &["/boot", "/efi"]));
        assert!(!starts_with_any(
            Path::new("/media/johndoe/bootfs"),
            &["/boot", "/efi"]
        ));
    }
}
