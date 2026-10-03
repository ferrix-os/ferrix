//! The libraries ported onto ferrousli that apps build against: built by
//! the scripts in `src/user/system/linux/ferrousli/tools/ports/`, installed
//! under `~/.local/share/ferrix/ports/ferrousli` (or `$FERRIX_PORTS`).
//!
//! The programs once ported here -- curl, git, sshdt, foot, vkgears, ALSA's
//! library and utilities, btop -- are apps (`docs/APPS.md`) since
//! 2026-10-01: each script is its app's `build.sh`, and an image carries
//! them as packages. What is left is `cargo xtask ports`, which builds the
//! libraries, and the reading of a built tree into an image's files
//! ([`read_entry`]), which the apps' packages use.
//!
//! An entry is a file or a whole tree. A tree, such as git's
//! `usr/libexec/git-core`, is walked in name order so the archive is the same
//! bytes every time, and its symbolic links go in as links. A file's
//! permissions are 0755 when it starts as an ELF program or a `#!` script and
//! 0644 otherwise, rather than read from the build host, so a tree copied to a
//! Windows machine gives the same archive.
//!
//! The scripts need a Linux host with gcc and the kernel's UAPI headers, as
//! `build.sh` for busybox does. On Windows they run in WSL's default
//! distribution, as a script app's `build.sh` does, and the ports are
//! installed in its home (`crate::wsl::home`). There is no native Windows
//! build of them yet.

use std::path::{Path, PathBuf};

use crate::paths::Arch;
use crate::{Error, Result};

/// The ports `cargo xtask ports` builds, in order, each a directory of
/// `src/user/system/linux/ferrousli/tools/ports/` holding a `build.sh`: the
/// libraries apps build against, which install nothing an image carries.
/// `libcxx` is the C++ runtime the btop app links; `zlib` is what git links.
/// The apps that need one build it first when it is not there, so this is
/// for building them ahead.
const PORTS: &[&str] = &["libcxx", "zlib"];

/// The ports AArch64 and ARMv7-A build: zlib, for git. The C++ runtime needs
/// the target's g++.
const ARM_PORTS: &[&str] = &["zlib"];

/// The ports `arch` builds and carries.
fn ports_for(arch: Arch) -> &'static [&'static str] {
    if arch == Arch::X86_64 {
        PORTS
    } else {
        ARM_PORTS
    }
}

/// What an installed path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Content {
    /// A regular file's bytes.
    Bytes(Vec<u8>),
    /// A symbolic link's target.
    Link(String),
    /// A directory, which the archive makes before what is in it.
    Directory,
}

/// A path an image carries, with what is there: a file, a link or a directory.
#[derive(Debug, Clone)]
pub(crate) struct File {
    pub(crate) path: String,
    pub(crate) mode: u32,
    pub(crate) content: Content,
}

/// The permissions an installed regular file is given: executable when it
/// starts as an ELF program or a script.
fn mode_of(bytes: &[u8]) -> u32 {
    if bytes.starts_with(b"\x7fELF") || bytes.starts_with(b"#!") {
        0o755
    } else {
        0o644
    }
}

/// Read what is at `path` on the host, the archive path `name`, and, for a
/// directory walked as a tree, everything beneath it in name order.
pub(crate) fn read_entry(
    path: &Path,
    name: &str,
    mode: u32,
    walk: bool,
    out: &mut Vec<File>,
) -> Result<()> {
    // A link WSL wrote on a Windows drive, which Windows cannot open, is read
    // in WSL: a script app's package, built there into this checkout.
    let wsl_link = |error: std::io::Error, out: &mut Vec<File>| {
        let target = crate::wsl::read_link(path)
            .ok_or_else(|| Error::new(format!("reading {}: {error}", path.display())))?;
        out.push(File {
            path: name.to_owned(),
            mode: 0o777,
            content: Content::Link(target),
        });
        Ok(())
    };
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) => return wsl_link(error, out),
    };
    if meta.file_type().is_symlink() {
        // A link in WSL's home, where Windows reads the ports from, is read
        // there.
        let target = match std::fs::read_link(path) {
            Ok(target) => target.to_string_lossy().replace('\\', "/"),
            Err(error) => crate::wsl::read_link(path)
                .ok_or_else(|| Error::new(format!("reading {}: {error}", path.display())))?,
        };
        out.push(File {
            path: name.to_owned(),
            mode: 0o777,
            content: Content::Link(target),
        });
    } else if meta.is_dir() {
        if !walk {
            return Err(Error::new(format!("{} is a directory", path.display())));
        }
        out.push(File {
            path: name.to_owned(),
            mode: 0o755,
            content: Content::Directory,
        });
        let mut children: Vec<_> = std::fs::read_dir(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<_>>()?;
        children.sort();
        for child in children {
            read_entry(
                &path.join(&child),
                &format!("{name}/{child}"),
                mode,
                true,
                out,
            )?;
        }
    } else {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => return wsl_link(error, out),
        };
        let mode = if walk { mode_of(&bytes) } else { mode };
        out.push(File {
            path: name.to_owned(),
            mode,
            content: Content::Bytes(bytes),
        });
    }
    Ok(())
}

/// The directory the ports are installed under: on Windows, WSL's home's,
/// where the scripts that build them run, and the btop app's libcxx with them.
pub(crate) fn root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("FERRIX_PORTS") {
        return Ok(PathBuf::from(dir));
    }
    let home = if cfg!(windows)
        && let Some(home) = crate::wsl::home()
    {
        Some(home)
    } else {
        std::env::home_dir()
    };
    home.map(|home| {
        [".local", "share", "ferrix", "ports", "ferrousli"]
            .iter()
            .fold(home, |dir, name| dir.join(name))
    })
    .ok_or_else(|| Error::new("no home directory to find the ports under; set FERRIX_PORTS"))
}

/// `cargo xtask ports`: run the `build.sh` of every port `arch` builds, in
/// order, stopping at the first that fails.
pub(crate) fn build(arch: Arch) -> Result<()> {
    let root = root()?;
    let ferrousli = crate::paths::workspace_root().join("src/user/system/linux/ferrousli");
    let ports = ports_for(arch);
    // One build of them all, in order, since each later port reads what an
    // earlier one installed: a build `FERRIX_BUILDS` may record or replay,
    // reading the sources the scripts would otherwise download from
    // `root/src`, and making the tree images carry, `root/<arch>`.
    let script = format!(
        "set -e\nfor port in {}; do bash \"tools/ports/$port/build.sh\" --arch {}; done",
        ports.join(" "),
        arch.name()
    );
    if cfg!(windows) {
        // In WSL, as an app's build.sh is. Without `FERRIX_PORTS` the
        // scripts' own default there is `root`; a Windows `FERRIX_PORTS` is
        // handed over as WSL names it.
        crate::wsl::require_toolchain(
            "the ports are built with a Linux host's gcc and its kernel's UAPI headers",
        )?;
        let given = std::env::var_os("FERRIX_PORTS");
        let (script, arguments) = match &given {
            Some(dir) => (
                format!("export FERRIX_PORTS=\"$(wslpath -u \"$1\")\"\n{script}"),
                vec![dir.to_string_lossy().into_owned()],
            ),
            None => (script, Vec::new()),
        };
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let status = crate::wsl::bash(&ferrousli, &script, &arguments)
            .stdin(std::process::Stdio::null())
            .status()
            .map_err(|error| Error::new(format!("could not run wsl.exe: {error}")))?;
        if !status.success() {
            return Err(Error::new(format!(
                "the ports' build scripts for {arch} in WSL: {status}"
            )));
        }
        println!(
            "\nbuilt {} for {arch} under {}",
            ports.join(", "),
            root.display()
        );
        return Ok(());
    }
    let mut build = crate::builds::Build::bash(
        format!(
            "src/user/system/linux/ferrousli/tools/ports ({}) for {arch}",
            ports.join(", ")
        ),
        &ferrousli,
    )
    .args(["-c", &script])
    .env("FERRIX_PORTS", &root)
    .reads_dir(root.join("src"))
    .output(root.join(arch.name()));
    // ferrousli gets a directory of its own inside the caller's target
    // directory, as it does for busybox.
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR").filter(|dir| !dir.is_empty()) {
        build = build.env("CARGO_TARGET_DIR", Path::new(&dir).join("ferrousli"));
    }
    build.run()?;
    println!(
        "\nbuilt {} for {arch} under {}",
        ports.join(", "),
        root.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_is_read_in_name_order_with_its_links_and_modes() {
        let dir = std::env::temp_dir().join(format!("xtask-ports-tree-{}", std::process::id()));
        let tree = dir.join("git-core");
        std::fs::create_dir_all(tree.join("mergetools")).unwrap();
        std::fs::write(tree.join("git-sh-setup"), b"#!/bin/sh\n").unwrap();
        std::fs::write(tree.join("b-data"), b"plain").unwrap();
        std::fs::write(tree.join("mergetools").join("vimdiff"), b"# sourced\n").unwrap();
        let mut files = Vec::new();
        read_entry(&tree, "usr/libexec/git-core", 0o755, true, &mut files).unwrap();
        let names: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            names,
            [
                "usr/libexec/git-core",
                "usr/libexec/git-core/b-data",
                "usr/libexec/git-core/git-sh-setup",
                "usr/libexec/git-core/mergetools",
                "usr/libexec/git-core/mergetools/vimdiff",
            ]
        );
        assert_eq!(files[0].content, Content::Directory);
        assert_eq!(files[1].mode, 0o644);
        assert_eq!(files[2].mode, 0o755);
        assert_eq!(mode_of(b"\x7fELF\x02"), 0o755);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn arm_builds_what_git_links_and_nothing_else() {
        for arch in [Arch::AArch64, Arch::Armv7a] {
            assert_eq!(ports_for(arch), ["zlib"], "{arch}");
        }
        assert_eq!(ports_for(Arch::X86_64), PORTS);
    }
}
