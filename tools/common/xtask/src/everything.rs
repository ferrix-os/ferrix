//! `run-compositor --everything`: one boot with all of it -- the GPU, the
//! clipboard, the network, Chrome and Steam on the desktop, and `rustc`,
//! `cargo`, Valve's `steamcmd` and Claude Code in the shell.
//!
//! All of it, always: `--everything` is everything. A volume that has not
//! been fetched yet is fetched by its script before the merge ([`fetched`]),
//! and a fetch that fails stops the run. None is left out with a line saying
//! how it could have been added, which is how a desktop came up without
//! Steam or Claude Code and nothing looked wrong.
//!
//! The kernel mounts one data disk, at `/data`, and the two downloads that
//! want it are two volumes: the one `tools/common/fetch/fetch-rustc-sysroot.sh` makes
//! and the one `tools/common/fetch/fetch-chrome.sh` makes. So `--chrome` had to take
//! the rustc volume's place, and a desktop with Chrome had no compiler.
//!
//! Both scripts keep the tree they packed beside their image, and both
//! trees are Debian 13's: where they hold the same path it is the same
//! package's file, byte for byte (281 of them on 2026-09-26). So this makes
//! a third volume out of the two trees, linked rather than copied, and
//! makes it again whenever either image is newer than it. Every image it
//! makes is checked with `btrfs check` before it is used, and made again
//! from copies when an old `mkfs.btrfs` wrote the links wrongly ([`broken`]). Two files at one
//! path that differ stop it, naming the path, rather than one quietly
//! winning -- with one exception, [`newer_runtime`] and [`newer_soname`]: a library whose newer build
//! runs everything built against the older, where the newer is kept and the
//! choice is said.
//!
//! steamcmd's tree is merged in next. Its i386 glibc is under paths neither
//! of the other two uses, so it adds files and clashes with none. Claude
//! Code's (`docs/CLAUDE-CODE.md`) follows: Chrome's glibc pin again, bash and
//! ripgrep, and `claude-code/claude`, which the desktop's terminals run as
//! `claude`.
//!
//! The Steam window's tree (`docs/STEAM.md`) comes last. It is the X
//! server's (`docs/YSERVER.md`, Y7) -- trixie's libraries again, with the
//! server at `/yserver/yserver`, built by `fetch-steam-window.sh`'s own run of
//! `fetch-yserver.sh` -- with Valve's bootstrap, the client's i386 and amd64
//! libraries and `lsof` on top, so yserver's own volume is not merged beside
//! it. Where it holds libstdc++'s gdb pretty-printers of gcc 14 beside the
//! rustc volume's of gcc 16, the ones of the newer libstdc++ the volume keeps
//! are kept ([`pretty_printers`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::{Error, Result};

/// Room for what the boot writes on the volume -- Chrome's profile and
/// caches, and what a compile leaves -- the two scripts' spares together,
/// on top of what the files take.
const SPARE_MIB: u64 = 1024 + 512;

/// Room for what steamcmd writes beside itself: the update it downloads and
/// unpacks on its first run, and what a login keeps.
const STEAMCMD_SPARE_MIB: u64 = 512;

/// Room for what Claude Code keeps: its configuration, its sessions, and
/// what its tools write.
const CLAUDE_CODE_SPARE_MIB: u64 = 256;

/// Room for the X server's log and its clients' files.
const YSERVER_SPARE_MIB: u64 = 256;

/// Room for what the Steam client writes beside its bootstrap: the packages
/// it downloads (about 500 MB), the client they unpack to, and its
/// browser's cache.
const STEAM_SPARE_MIB: u64 = 4096;

/// `--steam-preinstall`: merge the Steam library
/// `tools/common/fetch/fetch-steam-preinstall.sh` installed on the host
/// (Teeworlds and the Steam Linux Runtime by default) into `/data/steam`,
/// so Steam in the guest finds those apps installed and neither downloads
/// nor stages them (docs/STEAM.md §7, item 7).
static STEAM_PREINSTALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set once from the command line, as `crate::fat::set_strip_kernel`.
pub(crate) fn set_steam_preinstall(on: bool) {
    STEAM_PREINSTALL.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The preinstalled library's stamp and its tree, beside each other in
/// `FERRIX_STEAM_PREINSTALL` (default `~/.local/share/ferrix/steam-preinstall`).
fn steam_preinstall() -> Result<(PathBuf, PathBuf)> {
    let directory = match std::env::var_os("FERRIX_STEAM_PREINSTALL") {
        Some(directory) => PathBuf::from(directory),
        None => crate::paths::volume_directory("steam-preinstall")?,
    };
    let stamp = directory.join("steam-preinstall.stamp");
    let tree = directory.join("tree");
    if !stamp.is_file() || !tree.join("steam/steamapps").is_dir() {
        return Err(Error::new(format!(
            "--steam-preinstall: no library in {}: tools/common/fetch/fetch-steam-preinstall.sh \
             installs it with steamcmd on this host",
            directory.display()
        )));
    }
    Ok((stamp, tree))
}

/// The volume, made or made again from the two trees when it is missing or
/// older than either of the images they were packed into.
///
/// # Errors
///
/// When a volume's fetch fails, the trees disagree about a file, or
/// `mkfs.btrfs` cannot make the image, or `btrfs check` finds the one it
/// made from copies broken too.
pub(crate) fn volume() -> Result<PathBuf> {
    if cfg!(windows) {
        return volume_in_wsl();
    }
    let (sources, spare) = sources()?;
    let directory = directory()?;
    let image = directory.join("everything.img");
    let trees: Vec<&PathBuf> = sources.iter().map(|(_, tree)| tree).collect();
    let newest = sources
        .iter()
        .map(|(image, _)| modified(image))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max();
    // Which trees the image was made from: one made before steamcmd was
    // fetched is newer than every image and would otherwise be kept.
    let stamp = directory.join("sources");
    let sources: String = trees
        .iter()
        .map(|tree| format!("{}\n", tree.display()))
        .collect();
    if image.is_file()
        && Some(modified(&image)?) >= newest
        && std::fs::read_to_string(&stamp).ok().as_deref() == Some(sources.as_str())
    {
        println!("  everything: {}", image.display());
        return Ok(image);
    }

    println!(
        "  everything: making {} from {}",
        image.display(),
        trees
            .iter()
            .map(|tree| tree.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = std::fs::remove_file(&stamp);
    let mut size = make(&directory, &trees, spare, &image, Files::Linked)?;
    if let Some(why) = broken(&image)? {
        println!(
            "  everything: btrfs check finds {} broken ({why}): this host's mkfs.btrfs writes \
             hard-linked files wrongly, as btrfs-progs 6.6 does; making it again from copies",
            image.display()
        );
        size = make(&directory, &trees, spare, &image, Files::Copied)?;
        if let Some(why) = broken(&image)? {
            let _ = std::fs::remove_file(&image);
            return Err(Error::new(format!(
                "mkfs.btrfs made {} broken, from copies too: btrfs check says {why}",
                image.display()
            )));
        }
    }
    std::fs::write(&stamp, sources)
        .map_err(|error| Error::new(format!("writing {}: {error}", stamp.display())))?;
    println!(
        "  everything: {} ({size} MiB), btrfs check clean",
        image.display()
    );
    Ok(image)
}

/// How the merged tree holds the trees' files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Files {
    /// Hard links into the trees: one file system, and no second copy.
    Linked,
    /// Copies: for an `mkfs.btrfs` that writes hard links wrongly.
    Copied,
}

/// Merge `trees` into `directory`'s `tree`, holding their files as `files`
/// says, and make `image` of it with `spare` MiB of room; give its size in
/// MiB.
fn make(
    directory: &Path,
    trees: &[&PathBuf],
    spare: u64,
    image: &Path,
    files: Files,
) -> Result<u64> {
    let tree = directory.join("tree");
    if tree.exists() {
        std::fs::remove_dir_all(&tree)
            .map_err(|error| Error::new(format!("removing {}: {error}", tree.display())))?;
    }
    create_dir(&tree)?;
    let mut bytes = 0u64;
    for from in trees {
        bytes += merge(from, &tree, files)?;
    }

    let size = bytes.div_ceil(1 << 20) + spare;
    let _ = std::fs::remove_file(image);
    let file = std::fs::File::create(image)
        .map_err(|error| Error::new(format!("creating {}: {error}", image.display())))?;
    file.set_len(size << 20)
        .map_err(|error| Error::new(format!("sizing {}: {error}", image.display())))?;
    drop(file);
    let status = Command::new("mkfs.btrfs")
        .arg("-q")
        .arg("--rootdir")
        .arg(&tree)
        .arg(image)
        .status()
        .map_err(|error| Error::new(format!("running mkfs.btrfs: {error}")))?;
    if !status.success() {
        // A half-made image would be taken as made next time.
        let _ = std::fs::remove_file(image);
        return Err(Error::new(format!(
            "mkfs.btrfs {}: {status}",
            image.display()
        )));
    }
    Ok(size)
}

/// What `btrfs check --readonly` finds wrong with `image`: its first line
/// that is not progress, or nothing when it is clean.
///
/// btrfs-progs 6.6 (Ubuntu 24.04's, so WSL's) makes an image from a tree of
/// hard links whose linked files name directories the image does not have:
/// "link count wrong", "unresolved ref dir". The guest then reads `I/O
/// error` for those directories -- `/data/steam` and `/data/home` on
/// 2026-10-01 -- and nothing said why. 6.17 makes the same tree clean.
fn broken(image: &Path) -> Result<Option<String>> {
    let output = Command::new("btrfs")
        .args(["check", "--readonly"])
        .arg(image)
        .output()
        .map_err(|error| {
            Error::new(format!(
                "running btrfs check, from btrfs-progs as mkfs.btrfs is: {error}"
            ))
        })?;
    if output.status.success() {
        return Ok(None);
    }
    let said = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    // Its first lines are progress ("Opening filesystem to check..."); the
    // first that names a fault says why.
    let lines: Vec<&str> = said
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('['))
        .collect();
    let first = lines
        .iter()
        .find(|line| {
            ["error", "wrong", "unresolved", "corrupt", "fail"]
                .iter()
                .any(|fault| line.to_ascii_lowercase().contains(fault))
        })
        .or(lines.last())
        .copied()
        .unwrap_or("no reason given")
        .to_owned();
    Ok(Some(format!("{}: {first}", output.status)))
}

/// Each volume merged, as its image and the tree beside it, in the order
/// they are merged, and the MiB of room the volume leaves for what they
/// write.
///
/// Every one is there: a volume not fetched yet is fetched here, and one
/// that cannot be stops the run. `--everything` is everything, so none is
/// left out with a line saying how it could have been added.
fn sources() -> Result<(Vec<(PathBuf, PathBuf)>, u64)> {
    let rustc = (
        fetched(crate::rustc::volume, "fetch-rustc-sysroot.sh")?,
        crate::rustc::tree()?,
    );
    let chrome = fetched(crate::chrome::volume, "fetch-chrome.sh")?;
    let chrome = beside(chrome, "fetch-chrome.sh", Path::is_dir)?;
    let steamcmd = fetched(crate::steamcmd::volume, "fetch-steamcmd.sh")?;
    let steamcmd = beside(steamcmd, "fetch-steamcmd.sh", Path::is_dir)?;
    let claude_code = fetched(crate::claude_code::volume, "fetch-claude-code.sh")?;
    let claude_code = beside(claude_code, "fetch-claude-code.sh", |tree| {
        tree.join("claude-code/claude").is_file()
    })?;
    // Steam's tree is yserver's with Valve's bootstrap and the client's
    // libraries on top, and its yserver was built by its own run of
    // fetch-yserver.sh, so it brings the X server and yserver's own volume
    // is not merged: two servers at one path would stop the merge.
    let steam = fetched(
        crate::compositor::steam_window::volume,
        "fetch-steam-window.sh",
    )?;
    let steam = beside(steam, "fetch-steam-window.sh", |tree| {
        tree.join("steam/ubuntu12_32/steam").is_file() && tree.join("yserver/yserver").is_file()
    })?;
    let spare = SPARE_MIB
        + STEAMCMD_SPARE_MIB
        + CLAUDE_CODE_SPARE_MIB
        + YSERVER_SPARE_MIB
        + STEAM_SPARE_MIB;
    let mut sources = vec![rustc, chrome, steamcmd, claude_code, steam];
    // Last, under Steam's tree: only `steam/steamapps`, which no other tree has.
    if STEAM_PREINSTALL.load(std::sync::atomic::Ordering::Relaxed) {
        sources.push(steam_preinstall()?);
    }
    Ok((sources, spare))
}

/// The image `volume` finds, after running `tools/common/fetch/<script>`
/// when it finds none. The script writes where `volume` looks: both read the
/// same `FERRIX_*_VOLUME`, and default to the same place under
/// `~/.local/share/ferrix`. A script that fails, or that leaves no image,
/// stops the run.
fn fetched(volume: fn() -> Result<PathBuf>, script: &str) -> Result<PathBuf> {
    if let Ok(image) = volume() {
        return Ok(image);
    }
    let path = crate::paths::workspace_root()
        .join("tools/common/fetch")
        .join(script);
    println!("  everything: fetching with {}", path.display());
    // On Windows the scripts run in WSL, which is where `volume` looks for
    // what they make (`crate::wsl`), and `bash` there cannot open a Windows
    // path: the script is named relative to the checkout.
    let mut command = if cfg!(windows) {
        crate::wsl::bash(
            &crate::paths::workspace_root(),
            "exec bash \"tools/common/fetch/$1\"",
            &[script],
        )
    } else {
        let mut command = Command::new("bash");
        let _ = command.arg(&path);
        command
    };
    let status = command
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|error| Error::new(format!("running {}: {error}", path.display())))?;
    if !status.success() {
        return Err(Error::new(format!(
            "{script}: {status}; --everything needs what it fetches"
        )));
    }
    volume()
}

/// `image` and the tree its script keeps beside it, which `whole` says is
/// the tree the script packed.
fn beside(image: PathBuf, script: &str, whole: fn(&Path) -> bool) -> Result<(PathBuf, PathBuf)> {
    let tree = image
        .parent()
        .map(|directory| directory.join("tree"))
        .filter(|tree| whole(tree))
        .ok_or_else(|| {
            Error::new(format!(
                "no tree beside {}: tools/common/fetch/{script} keeps one there",
                image.display()
            ))
        })?;
    Ok((image, tree))
}

/// [`volume`] on Windows, where the trees are WSL's, full of symbolic links,
/// and `mkfs.btrfs` is Linux's: xtask's Linux build makes the volume there
/// (`cargo xtask everything-volume`), and this one boots it from WSL's home.
/// `FERRIX_EVERYTHING_VOLUME` is not passed on, so the volume is always at
/// the Linux run's default place, and this looks for it there.
fn volume_in_wsl() -> Result<PathBuf> {
    crate::wsl::require_toolchain(
        "--everything makes its volume from trees of symbolic links with mkfs.btrfs",
    )?;
    let root = crate::paths::workspace_root();
    let status = crate::wsl::cargo(
        &root,
        &[
            "run",
            "--quiet",
            "--package",
            "xtask",
            "--",
            "everything-volume",
        ],
    )
    .stdin(std::process::Stdio::null())
    .status()
    .map_err(|error| Error::new(format!("could not run wsl.exe: {error}")))?;
    if !status.success() {
        return Err(Error::new(format!(
            "making the --everything volume in WSL: {status}"
        )));
    }
    let image = crate::paths::volume_directory("everything")?.join("everything.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "WSL made the --everything volume, but {} is not there",
            image.display()
        )));
    }
    Ok(image)
}

/// `~/.local/share/ferrix/everything`, or `FERRIX_EVERYTHING_VOLUME`.
///
/// Beside the two volumes by default, which is what lets the tree be hard
/// links into theirs: one file system, and no second copy of 1.6 GiB.
fn directory() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("FERRIX_EVERYTHING_VOLUME") {
        return Ok(PathBuf::from(directory));
    }
    crate::paths::volume_directory("everything")
}

/// Put everything under `from` into `into`, and say how many bytes of files
/// that added: a file hard-linked (copied where it cannot be), a symbolic
/// link made again with its target. A path already there from the other
/// tree must be the same file or the same link.
fn merge(from: &Path, into: &Path, files: Files) -> Result<u64> {
    let mut bytes = 0u64;
    let entries = std::fs::read_dir(from)
        .map_err(|error| Error::new(format!("reading {}: {error}", from.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| Error::new(format!("reading {}: {error}", from.display())))?;
        let source = entry.path();
        let target = into.join(entry.file_name());
        let kind = std::fs::symlink_metadata(&source)
            .map_err(|error| Error::new(format!("{}: {error}", source.display())))?;
        if kind.is_symlink() {
            let link = std::fs::read_link(&source)
                .map_err(|error| Error::new(format!("{}: {error}", source.display())))?;
            if std::fs::symlink_metadata(&target).is_ok() {
                let there = std::fs::read_link(&target).ok();
                if there.as_ref() == Some(&link) {
                    continue;
                }
                if is_alternative(&target) {
                    say_kept(&target, "the earlier volume's, one provider of the command");
                    continue;
                }
                match there
                    .as_deref()
                    .and_then(|there| newer_soname(&target, there, &link))
                {
                    Some(Keep::There) => {
                        say_kept(&target, "the earlier volume's, the newer runtime");
                        continue;
                    }
                    Some(Keep::Here) => {
                        say_kept(&target, "the later volume's, the newer runtime");
                        remove(&target)?;
                    }
                    None => return Err(clash(&target)),
                }
            }
            symlink(&link, &target)?;
        } else if kind.is_dir() {
            match std::fs::symlink_metadata(&target) {
                Ok(there) if !there.is_dir() => return Err(clash(&target)),
                Ok(_) => {}
                Err(_) => create_dir(&target)?,
            }
            bytes += merge(&source, &target, files)?;
        } else {
            if let Ok(there) = std::fs::symlink_metadata(&target) {
                if !there.is_file() {
                    return Err(clash(&target));
                }
                if same_contents(&source, &target)? {
                    continue;
                }
                if pretty_printers(&target) {
                    say_kept(&target, "the first volume's, beside the newer libstdc++");
                    continue;
                }
                match newer_runtime(&target, &target, &source)? {
                    Some(Keep::There) => {
                        say_kept(&target, "the earlier volume's, the newer runtime");
                        continue;
                    }
                    Some(Keep::Here) => {
                        say_kept(&target, "the later volume's, the newer runtime");
                        // Its bytes were counted when the other copy came.
                        remove(&target)?;
                        link_or_copy(&source, &target, files)?;
                        continue;
                    }
                    None => return Err(clash(&target)),
                }
            }
            link_or_copy(&source, &target, files)?;
            // Rounded up to a block, as the file system will store it.
            bytes += kind.len().div_ceil(4096) * 4096;
        }
    }
    Ok(bytes)
}

/// A symbolic link at `at` reading `link`.
#[cfg(unix)]
fn symlink(link: &Path, at: &Path) -> Result<()> {
    std::os::unix::fs::symlink(link, at)
        .map_err(|error| Error::new(format!("{}: {error}", at.display())))
}

/// A symbolic link, which only a Unix host makes: on Windows, [`volume`]
/// has WSL make the volume, and never merges here.
#[cfg(not(unix))]
fn symlink(_link: &Path, at: &Path) -> Result<()> {
    Err(Error::new(format!(
        "{}: --everything makes its volume on a Unix host",
        at.display()
    )))
}

/// Remove the copy a newer one replaces.
fn remove(path: &Path) -> Result<()> {
    std::fs::remove_file(path).map_err(|error| Error::new(format!("{}: {error}", path.display())))
}

/// Hard-link `source` at `target`, or copy it where a link cannot be made
/// or `files` says copies.
fn link_or_copy(source: &Path, target: &Path, files: Files) -> Result<()> {
    if files == Files::Copied || std::fs::hard_link(source, target).is_err() {
        let _ = std::fs::copy(source, target)
            .map_err(|error| Error::new(format!("{}: {error}", target.display())))?;
    }
    Ok(())
}

/// Which of two copies at one path the volume keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    /// The one already there: an earlier volume's, the rustc volume's first.
    There,
    /// The one arriving: a later volume's, Chrome's or yserver's.
    Here,
}

/// Commands Debian lets several packages provide, whose link a volume's
/// script makes to whichever provider its tree has: `awk` is `gawk` in the
/// rustc volume's and `mawk` in the Steam window volume's. Either runs what
/// asks for `awk`, so the first volume's link is kept.
const ALTERNATIVES: &[&str] = &["usr/bin/awk"];

/// Whether `target`, a path in the merged tree, is one of [`ALTERNATIVES`].
fn is_alternative(target: &Path) -> bool {
    ALTERNATIVES
        .iter()
        .any(|path| target.ends_with(Path::new(path)))
}

/// Say which copy of a file the volume took, and why, in one line.
fn say_kept(path: &Path, whose: &str) {
    println!(
        "  everything: {}: two volumes differ; kept {whose}",
        path.display()
    );
}

/// Whether `path` is one of libstdc++'s gdb pretty-printers
/// (`usr/share/gcc/python/`), Python that gdb loads for the libstdc++ it
/// debugs, and nothing runs otherwise. Two volumes' libstdc++6 packages of
/// different gcc each ship theirs, and the ones already there came with the
/// rustc volume, merged first, whose libstdc++ is the newer that
/// [`newer_runtime`] keeps.
fn pretty_printers(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|path| path.contains("/usr/share/gcc/python/"))
}

/// The GCC runtime, by file-name prefix, and the prefix of the symbol
/// versions each defines. GCC keeps every one backward compatible: the
/// rustc volume takes gcc 16's from sid for gcc 15, the Chrome volume
/// trixie's gcc 14, and a program built against the older runs on the newer.
const RUNTIMES: &[(&str, &str)] = &[
    ("libgcc_s.so", "GCC_"),
    ("libatomic.so", "LIBATOMIC_"),
    ("libstdc++.so", "GLIBCXX_"),
];

/// For a GCC runtime the two volumes hold different builds of, which to keep:
/// the one whose newest symbol version is later, and on a tie the rustc
/// volume's, whose runtime is sid's and the newer build. `None` for any other
/// file, which stays a clash.
fn newer_runtime(path: &Path, there: &Path, here: &Path) -> Result<Option<Keep>> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let Some(&(_, prefix)) = RUNTIMES.iter().find(|(start, _)| name.starts_with(start)) else {
        return Ok(None);
    };
    let read = |path: &Path| {
        std::fs::read(path).map_err(|error| Error::new(format!("{}: {error}", path.display())))
    };
    let (there, here) = (
        newest_version(&read(there)?, prefix),
        newest_version(&read(here)?, prefix),
    );
    Ok(match (there, here) {
        (Some(there), Some(here)) if here > there => Some(Keep::Here),
        (Some(_), Some(_)) => Some(Keep::There),
        // Not the library its name says: a clash after all.
        _ => None,
    })
}

/// The latest `<prefix>N.N…` version a library's string table names, as
/// numbers to compare. The names of the versions it defines are in its
/// `.dynstr` beside the ones it needs, and only its own carry its prefix.
fn newest_version(bytes: &[u8], prefix: &str) -> Option<Vec<u32>> {
    let prefix = prefix.as_bytes();
    let mut newest: Option<Vec<u32>> = None;
    let mut at = 0;
    while let Some(found) = bytes.get(at..).and_then(|rest| {
        rest.windows(prefix.len())
            .position(|window| window == prefix)
    }) {
        let start = at + found + prefix.len();
        let end = bytes
            .get(start..)
            .and_then(|rest| {
                rest.iter()
                    .position(|&byte| !(byte.is_ascii_digit() || byte == b'.'))
            })
            .map_or(bytes.len(), |length| start + length);
        let version: Option<Vec<u32>> =
            std::str::from_utf8(bytes.get(start..end).unwrap_or_default())
                .ok()
                .filter(|text| !text.is_empty())
                .and_then(|text| text.split('.').map(|part| part.parse().ok()).collect());
        if let Some(version) = version
            && newest.as_ref().is_none_or(|newest| &version > newest)
        {
            newest = Some(version);
        }
        at = end.max(start);
    }
    newest
}

/// For a library's soname link the two volumes point at different builds of,
/// which to keep: the later version, which is how `ldconfig` would have
/// chosen. Only a link named `lib*.so.N` whose two targets are that name
/// followed by a version; `None` for any other link, which stays a clash.
fn newer_soname(path: &Path, there: &Path, here: &Path) -> Option<Keep> {
    let name = path.file_name()?.to_str()?;
    let major = name.get(name.find(".so.")? + 4..)?;
    if !name.starts_with("lib")
        || major.is_empty()
        || !major.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let version = |target: &Path| -> Option<Vec<u32>> {
        let target = target.to_str()?;
        if target.contains('/') {
            return None;
        }
        let rest = target.strip_prefix(name)?.strip_prefix('.')?;
        rest.split('.').map(|part| part.parse().ok()).collect()
    };
    let (there, here) = (version(there)?, version(here)?);
    Some(if here > there {
        Keep::Here
    } else {
        Keep::There
    })
}

/// Whether two files hold the same bytes.
fn same_contents(left: &Path, right: &Path) -> Result<bool> {
    let read = |path: &Path| {
        std::fs::read(path).map_err(|error| Error::new(format!("{}: {error}", path.display())))
    };
    Ok(read(left)? == read(right)?)
}

/// The error for a path the two trees disagree about.
fn clash(path: &Path) -> Error {
    Error::new(format!(
        "{}: two of the volumes hold different files here; fetch them again so they are \
         from the same Debian",
        path.display()
    ))
}

/// Make one directory.
fn create_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .map_err(|error| Error::new(format!("creating {}: {error}", path.display())))
}

/// When a file was last written.
fn modified(path: &Path) -> Result<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    /// A tree under the target directory's scratch, with each `(path, bytes)`
    /// as a file and each `(path, target)` as a symbolic link.
    fn tree(name: &str, files: &[(&str, &[u8])], links: &[(&str, &str)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ferrix-everything-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, bytes) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        for (path, target) in links {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, path).unwrap();
        }
        root
    }

    const LIB: &str = "usr/lib/x86_64-linux-gnu";

    /// Linked, a merged file is the tree's own, two names for one file;
    /// copied, it is a file of its own, which is what an `mkfs.btrfs` that
    /// writes hard links wrongly is given.
    #[test]
    fn copied_files_have_one_name() {
        use std::os::unix::fs::MetadataExt;
        let path = format!("{LIB}/libz.so.1");
        let from = tree("copied-from", &[(&path, b"zlib")], &[]);
        for (files, names) in [(Files::Linked, 2), (Files::Copied, 1)] {
            let into = tree(&format!("copied-into-{files:?}"), &[], &[]);
            create_dir(&into).unwrap();
            let _ = merge(&from, &into, files).unwrap();
            let merged = std::fs::metadata(into.join(&path)).unwrap();
            assert_eq!(merged.nlink(), names, "{files:?}");
            assert_eq!(std::fs::read(into.join(&path)).unwrap(), b"zlib");
            let _ = std::fs::remove_dir_all(&into);
        }
        let _ = std::fs::remove_dir_all(&from);
    }

    #[test]
    fn a_newer_version_is_read_from_the_string_table() {
        let table = b"\0GLIBC_2.2.5\0GCC_3.0\0GCC_14.0.0\0GCC_4.2.0\0";
        assert_eq!(newest_version(table, "GCC_"), Some(vec![14, 0, 0]));
        assert_eq!(newest_version(table, "GLIBCXX_"), None);
    }

    /// The rule the volumes met on 2026-09-26: gcc 16's runtime from sid in
    /// the rustc volume, gcc 14's from trixie in the Chrome volume, both
    /// topping out at `GCC_14.0.0`, and wayland 1.24 against 1.23.
    #[test]
    fn differing_runtimes_keep_the_newer_and_say_so() {
        let libgcc = format!("{LIB}/libgcc_s.so.1");
        let libstdcxx = format!("{LIB}/libstdc++.so.6.0.33");
        let wayland = format!("{LIB}/libwayland-server.so.0");
        let rustc = tree(
            "runtime-rustc",
            &[
                (&libgcc, b"sid\0GCC_14.0.0\0"),
                (&libstdcxx, b"old\0GLIBCXX_3.4.33\0"),
            ],
            &[(&wayland, "libwayland-server.so.0.24.0")],
        );
        let chrome = tree(
            "runtime-chrome",
            &[
                (&libgcc, b"trixie\0GCC_14.0.0\0"),
                (&libstdcxx, b"new\0GLIBCXX_3.4.34\0"),
            ],
            &[(&wayland, "libwayland-server.so.0.23.1")],
        );
        let into = tree("runtime-into", &[], &[]);
        create_dir(&into).unwrap();
        let _ = merge(&rustc, &into, Files::Linked).unwrap();
        let _ = merge(&chrome, &into, Files::Linked).unwrap();
        // A tie goes to the rustc volume's; a later version wins either way.
        assert!(
            std::fs::read(into.join(&libgcc))
                .unwrap()
                .starts_with(b"sid")
        );
        assert!(
            std::fs::read(into.join(&libstdcxx))
                .unwrap()
                .starts_with(b"new")
        );
        assert_eq!(
            std::fs::read_link(into.join(&wayland)).unwrap(),
            PathBuf::from("libwayland-server.so.0.24.0")
        );
        for root in [rustc, chrome, into] {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    /// yserver's tree, on 2026-09-28: gcc 14's pretty-printers where the
    /// rustc volume has gcc 16's.
    #[test]
    fn libstdcxx_pretty_printers_keep_the_first_volumes() {
        let printers = "usr/share/gcc/python/libstdcxx/v6/printers.py";
        let rustc = tree("printers-rustc", &[(printers, b"gcc 16")], &[]);
        let yserver = tree("printers-yserver", &[(printers, b"gcc 14")], &[]);
        let into = tree("printers-into", &[], &[]);
        create_dir(&into).unwrap();
        let _ = merge(&rustc, &into, Files::Linked).unwrap();
        let _ = merge(&yserver, &into, Files::Linked).unwrap();
        assert_eq!(std::fs::read(into.join(printers)).unwrap(), b"gcc 16");
        for root in [rustc, yserver, into] {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    /// The negative control: any other library that differs still stops the
    /// merge, and so does a soname link to somewhere that is not a version.
    #[test]
    fn any_other_difference_is_still_a_clash() {
        let libz = format!("{LIB}/libz.so.1.3.1");
        let rustc_z = tree("clash-file-rustc", &[(&libz, b"one")], &[]);
        let chrome_z = tree("clash-file-chrome", &[(&libz, b"two")], &[]);
        let into_z = tree("clash-file-into", &[], &[]);
        create_dir(&into_z).unwrap();
        let _ = merge(&rustc_z, &into_z, Files::Linked).unwrap();
        let error = merge(&chrome_z, &into_z, Files::Linked)
            .unwrap_err()
            .to_string();
        assert!(error.contains("libz.so.1.3.1"), "{error}");

        let odd = format!("{LIB}/libfoo.so.1");
        let rustc = tree("clash-link-rustc", &[], &[(&odd, "libfoo.so.1.2")]);
        let chrome = tree(
            "clash-link-chrome",
            &[],
            &[(&odd, "../elsewhere/libfoo.so.1.3")],
        );
        let into = tree("clash-link-into", &[], &[]);
        create_dir(&into).unwrap();
        let _ = merge(&rustc, &into, Files::Linked).unwrap();
        let error = merge(&chrome, &into, Files::Linked)
            .unwrap_err()
            .to_string();
        assert!(error.contains("libfoo.so.1"), "{error}");
        // A GCC runtime whose name says it is one but that names no version.
        let libgcc = format!("{LIB}/libgcc_s.so.1");
        let rustc_gcc = tree("clash-gcc-rustc", &[(&libgcc, b"no versions")], &[]);
        let chrome_gcc = tree("clash-gcc-chrome", &[(&libgcc, b"none here")], &[]);
        let into_gcc = tree("clash-gcc-into", &[], &[]);
        create_dir(&into_gcc).unwrap();
        let _ = merge(&rustc_gcc, &into_gcc, Files::Linked).unwrap();
        assert!(merge(&chrome_gcc, &into_gcc, Files::Linked).is_err());
        for root in [
            rustc, chrome, into, rustc_gcc, chrome_gcc, into_gcc, rustc_z, chrome_z, into_z,
        ] {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
