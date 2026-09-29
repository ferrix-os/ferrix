//! `run-compositor --everything`: one boot with all of it -- the GPU, the
//! clipboard, the network, Chrome on the desktop and `rustc`, `cargo` and,
//! once `scripts/fetch/fetch-steamcmd.sh` has run, Valve's `steamcmd` in the
//! shell.
//!
//! The kernel mounts one data disk, at `/data`, and the two downloads that
//! want it are two volumes: the one `scripts/fetch/fetch-rustc-sysroot.sh` makes
//! and the one `scripts/fetch/fetch-chrome.sh` makes. So `--chrome` had to take
//! the rustc volume's place, and a desktop with Chrome had no compiler.
//!
//! Both scripts keep the tree they packed beside their image, and both
//! trees are Debian 13's: where they hold the same path it is the same
//! package's file, byte for byte (281 of them on 2026-09-26). So this makes
//! a third volume out of the two trees, linked rather than copied, and
//! makes it again whenever either image is newer than it. Two files at one
//! path that differ stop it, naming the path, rather than one quietly
//! winning -- with one exception, [`newer_runtime`] and [`newer_soname`]: a library whose newer build
//! runs everything built against the older, where the newer is kept and the
//! choice is said.
//!
//! steamcmd's tree, when it has been fetched, is merged in next. Its i386
//! glibc is under paths neither of the other two uses, so it adds files and
//! clashes with none. yserver's, the X server's (`docs/YSERVER.md`, Y7), comes
//! last when `scripts/fetch/fetch-yserver.sh` has made it: trixie's libraries
//! again, with the server at `/yserver/yserver`. Where it holds libstdc++'s
//! gdb pretty-printers of gcc 14 beside the rustc volume's of gcc 16, the ones
//! of the newer libstdc++ the volume keeps are kept ([`pretty_printers`]).
//!
//! The Steam window's tree (`docs/STEAM.md`), when
//! `scripts/fetch/fetch-steam-window.sh` has made it, comes last in yserver's
//! place: it is yserver's tree with Valve's bootstrap, the client's i386 and
//! amd64 libraries and `lsof` on top, and its own build of the server.

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

/// Room for the X server's log and its clients' files.
const YSERVER_SPARE_MIB: u64 = 256;

/// Room for what the Steam client writes beside its bootstrap: the packages
/// it downloads (about 500 MB), the client they unpack to, and its
/// browser's cache.
const STEAM_SPARE_MIB: u64 = 4096;

/// The volume, made or made again from the two trees when it is missing or
/// older than either of the images they were packed into.
///
/// # Errors
///
/// When either volume has not been fetched, the trees disagree about a
/// file, or `mkfs.btrfs` cannot make the image.
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
    let tree = directory.join("tree");
    if tree.exists() {
        std::fs::remove_dir_all(&tree)
            .map_err(|error| Error::new(format!("removing {}: {error}", tree.display())))?;
    }
    create_dir(&tree)?;
    let mut bytes = 0u64;
    for from in trees {
        bytes += merge(from, &tree)?;
    }

    let size = bytes.div_ceil(1 << 20) + spare;
    let _ = std::fs::remove_file(&stamp);
    let _ = std::fs::remove_file(&image);
    let file = std::fs::File::create(&image)
        .map_err(|error| Error::new(format!("creating {}: {error}", image.display())))?;
    file.set_len(size << 20)
        .map_err(|error| Error::new(format!("sizing {}: {error}", image.display())))?;
    drop(file);
    let status = Command::new("mkfs.btrfs")
        .arg("-q")
        .arg("--rootdir")
        .arg(&tree)
        .arg(&image)
        .status()
        .map_err(|error| Error::new(format!("running mkfs.btrfs: {error}")))?;
    if !status.success() {
        // A half-made image would be taken as made next time.
        let _ = std::fs::remove_file(&image);
        return Err(Error::new(format!(
            "mkfs.btrfs {}: {status}",
            image.display()
        )));
    }
    std::fs::write(&stamp, sources)
        .map_err(|error| Error::new(format!("writing {}: {error}", stamp.display())))?;
    println!("  everything: {} ({size} MiB)", image.display());
    Ok(image)
}

/// Each volume merged, as its image and the tree beside it, in the order
/// they are merged, and the MiB of room the volume leaves for what they
/// write.
fn sources() -> Result<(Vec<(PathBuf, PathBuf)>, u64)> {
    let rustc = (crate::rustc::volume()?, crate::rustc::tree()?);
    let chrome_image = crate::chrome::volume()?;
    let chrome_tree = chrome_image
        .parent()
        .map(|directory| directory.join("tree"))
        .filter(|tree| tree.is_dir())
        .ok_or_else(|| {
            Error::new(format!(
                "no tree beside {}: scripts/fetch/fetch-chrome.sh keeps one there",
                chrome_image.display()
            ))
        })?;
    let mut sources = vec![rustc, (chrome_image, chrome_tree)];
    let mut spare = SPARE_MIB;
    if let Some(steamcmd) = steamcmd()? {
        sources.push(steamcmd);
        spare += STEAMCMD_SPARE_MIB;
    }
    // Steam's tree is yserver's with Valve's bootstrap and the client's
    // libraries on top, and its yserver was built by its own run of
    // fetch-yserver.sh: two servers at one path would stop the merge.
    if let Some(steam) = steam()? {
        sources.push(steam);
        spare += YSERVER_SPARE_MIB + STEAM_SPARE_MIB;
    } else if let Some(yserver) = yserver()? {
        sources.push(yserver);
        spare += YSERVER_SPARE_MIB;
    }
    Ok((sources, spare))
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

/// steamcmd's image and the tree beside it, or `None` when it has not been
/// fetched: the desktop is whole without it, and says how to add it.
fn steamcmd() -> Result<Option<(PathBuf, PathBuf)>> {
    let Ok(image) = crate::steamcmd::volume() else {
        println!(
            "  everything: no steamcmd in the terminal; scripts/fetch/fetch-steamcmd.sh adds it"
        );
        return Ok(None);
    };
    let tree = image
        .parent()
        .map(|directory| directory.join("tree"))
        .filter(|tree| tree.is_dir())
        .ok_or_else(|| {
            Error::new(format!(
                "no tree beside {}: scripts/fetch/fetch-steamcmd.sh keeps one there",
                image.display()
            ))
        })?;
    Ok(Some((image, tree)))
}

/// yserver's image and the tree beside it, or `None` when it has not been
/// made: the desktop is whole without an X server, and says how to add one.
pub(crate) fn yserver() -> Result<Option<(PathBuf, PathBuf)>> {
    let Ok(image) = crate::yserver::volume() else {
        println!("  everything: no X server; scripts/fetch/fetch-yserver.sh adds yserver");
        return Ok(None);
    };
    let tree = image
        .parent()
        .map(|directory| directory.join("tree"))
        .filter(|tree| tree.join("yserver/yserver").is_file())
        .ok_or_else(|| {
            Error::new(format!(
                "no tree with yserver beside {}: scripts/fetch/fetch-yserver.sh keeps one there",
                image.display()
            ))
        })?;
    Ok(Some((image, tree)))
}

/// The Steam window volume's image and the tree beside it
/// (`scripts/fetch/fetch-steam-window.sh`, `docs/STEAM.md`), or `None` when
/// it has not been made: the desktop is whole without Steam, and says how to
/// add it.
pub(crate) fn steam() -> Result<Option<(PathBuf, PathBuf)>> {
    let Ok(image) = crate::compositor::steam_window::volume() else {
        println!("  everything: no Steam; scripts/fetch/fetch-steam-window.sh adds the client");
        return Ok(None);
    };
    let tree = image
        .parent()
        .map(|directory| directory.join("tree"))
        .filter(|tree| tree.join("steam/ubuntu12_32/steam").is_file())
        .ok_or_else(|| {
            Error::new(format!(
                "no tree with Steam's bootstrap beside {}: \
                 scripts/fetch/fetch-steam-window.sh keeps one there",
                image.display()
            ))
        })?;
    Ok(Some((image, tree)))
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
fn merge(from: &Path, into: &Path) -> Result<u64> {
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
            bytes += merge(&source, &target)?;
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
                        link_or_copy(&source, &target)?;
                        continue;
                    }
                    None => return Err(clash(&target)),
                }
            }
            link_or_copy(&source, &target)?;
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

/// Hard-link `source` at `target`, or copy it where a link cannot be made.
fn link_or_copy(source: &Path, target: &Path) -> Result<()> {
    if std::fs::hard_link(source, target).is_err() {
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
        let _ = merge(&rustc, &into).unwrap();
        let _ = merge(&chrome, &into).unwrap();
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
        let _ = merge(&rustc, &into).unwrap();
        let _ = merge(&yserver, &into).unwrap();
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
        let _ = merge(&rustc_z, &into_z).unwrap();
        let error = merge(&chrome_z, &into_z).unwrap_err().to_string();
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
        let _ = merge(&rustc, &into).unwrap();
        let error = merge(&chrome, &into).unwrap_err().to_string();
        assert!(error.contains("libfoo.so.1"), "{error}");
        // A GCC runtime whose name says it is one but that names no version.
        let libgcc = format!("{LIB}/libgcc_s.so.1");
        let rustc_gcc = tree("clash-gcc-rustc", &[(&libgcc, b"no versions")], &[]);
        let chrome_gcc = tree("clash-gcc-chrome", &[(&libgcc, b"none here")], &[]);
        let into_gcc = tree("clash-gcc-into", &[], &[]);
        create_dir(&into_gcc).unwrap();
        let _ = merge(&rustc_gcc, &into_gcc).unwrap();
        assert!(merge(&chrome_gcc, &into_gcc).is_err());
        for root in [
            rustc, chrome, into, rustc_gcc, chrome_gcc, into_gcc, rustc_z, chrome_z, into_z,
        ] {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
