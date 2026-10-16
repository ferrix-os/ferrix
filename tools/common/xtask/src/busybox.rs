//! ferrousli's busybox: built by `src/user/system/linux/ferrousli/tools/busybox/build.sh`, or on
//! Windows by `build-windows.sh` beside it, and installed as
//! `<arch>/bin/busybox.static` under `~/.local/share/ferrix/busybox/ferrousli`
//! (or `$FERRIX_BUSYBOX`), where `--init ferrousli` finds it.
//!
//! `build.sh` compiles x86-64's with the host's `cc` against the host's kernel
//! UAPI headers, and AArch64's and ARMv7-A's with gcc for the target against
//! Alpine's pinned UAPI headers for it. Windows has no `cc`, so
//! `build-windows.sh` cross-compiles x86-64's with clang against Alpine's
//! pinned headers, in Git for Windows' bash; it builds no other. All of them
//! build the same pinned sources with the same config.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, cargo, ferrousli};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "busybox",
    help_at: 1080,
    help: &["busybox       Build busybox against ferrousli (x86_64) for --init ferrousli"],
    run: |_, args| build(args.single_arch()?).map(|_| ()),
}];

/// The `--init` value that names ferrousli's busybox rather than a path.
pub(crate) const INIT_NAME: &str = "ferrousli";

/// The environment variable naming the install directory, and its default.
const VAR: &str = "FERRIX_BUSYBOX";
/// The default install directory under the home directory.
const HOME_SEGMENTS: &[&str] = &[".local", "share", "ferrix", "busybox", "ferrousli"];

/// The directory ferrousli's busybox is installed under.
fn root() -> Result<PathBuf> {
    ferrousli::install_root(VAR, HOME_SEGMENTS, "ferrousli's busybox")
}

/// Run by `bash -c` in `src/user/system/linux/ferrousli/`, with `$1` the build script in
/// `tools/busybox/`, `$2` the directory to install into and `$3` the
/// architecture.
///
/// `$out` is the scripts' own: `$FERRIX_BUSYBOX`, with their default. Normally
/// it is the install directory itself, and the copy is skipped. Both lists are
/// removed first, so a list read after a failure is this run's.
const SCRIPT: &str = r#"set -uo pipefail
root=$2
arch=$3
out=${FERRIX_BUSYBOX:-$HOME/.local/share/ferrix/busybox/ferrousli}
mkdir -p "$root/$arch/bin" || exit 1
rm -f "$out/undefined-symbols.txt" "$root/undefined-symbols.txt"
if ! bash "tools/busybox/$1" --arch "$arch"; then
    if [ -f "$out/undefined-symbols.txt" ] && ! [ "$out" -ef "$root" ]; then
        cp "$out/undefined-symbols.txt" "$root/"
    fi
    exit 1
fi
install -m 755 "$out/$arch/busybox" "$root/$arch/bin/busybox.static"
"#;

/// The installed binary for `arch`, beneath `root`.
fn installed(root: &Path, arch: Arch) -> PathBuf {
    root.join(arch.name()).join("bin").join("busybox.static")
}

/// Refuse an architecture this host's build script does not build for:
/// `build-windows.sh` builds x86-64's alone.
fn refuse_unbuildable(arch: Arch) -> Result<()> {
    if arch == Arch::X86_64 || !cfg!(windows) {
        Ok(())
    } else {
        Err(Error::new(format!(
            "ferrousli's busybox for {arch} is cross-compiled with gcc for it, which \
             build-windows.sh does not do; build it on Linux"
        )))
    }
}

/// What a busybox built against ferrousli is made from, relative to
/// `src/user/system/linux/ferrousli/`: the library and its entry object, and the pinned busybox
/// sources and configuration the build scripts use.
const INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "crt",
    "include",
    "src",
    "tools/busybox",
];

/// The installed busybox for `arch` if there already is one, and `None`
/// rather than a build.
///
/// [`program`] is the strict answer `--init ferrousli` wants: it builds when
/// the binary is missing or older than ferrousli, because what boots must be
/// the tree as it stands. A watched boot wants the opposite -- somebody
/// asked to look at the compositor, and a ten-minute busybox build is not
/// what they asked for -- so this takes whatever is there and says so.
pub(crate) fn installed_program(arch: Arch) -> Option<PathBuf> {
    let root = root().ok()?;
    let program = installed(&root, arch);
    program.is_file().then_some(program)
}

/// `--init ferrousli`: the installed busybox for `arch`, built first when it is
/// missing or older than ferrousli, so what boots is the tree as it stands.
///
/// Never another busybox in its place: a build that fails is the error.
pub(crate) fn program(arch: Arch) -> Result<PathBuf> {
    refuse_unbuildable(arch)?;
    let root = root()?;
    let program = installed(&root, arch);
    let ferrousli = crate::paths::workspace_root().join("src/user/system/linux/ferrousli");
    if !crate::builds::active() && ferrousli::stale(&program, &ferrousli, INPUTS).is_none() {
        return Ok(program);
    }
    // Another checkout may be building into the same directory: wait for
    // it, then look again, since what it installed may be current.
    let _lock = ferrousli::lock_builds(&root, "busybox")?;
    match ferrousli::stale(&program, &ferrousli, INPUTS) {
        Some(reason) => {
            println!("  ferrousli's busybox {reason}; building it");
            build_locked(arch, &root)
        }
        None => Ok(program),
    }
}

/// `cargo xtask busybox`: run `build.sh`, or `build-windows.sh` on Windows,
/// and install the binary for `--init ferrousli`.
///
/// While ferrousli still lacks functions busybox calls, the link fails and
/// this refuses, naming them.
pub(crate) fn build(arch: Arch) -> Result<PathBuf> {
    refuse_unbuildable(arch)?;
    let root = root()?;
    let _lock = ferrousli::lock_builds(&root, "busybox")?;
    build_locked(arch, &root)
}

/// [`build`], with the build lock already held.
fn build_locked(arch: Arch, root: &Path) -> Result<PathBuf> {
    let root = root.to_path_buf();
    let program = installed(&root, arch);
    let ferrousli = crate::paths::workspace_root().join("src/user/system/linux/ferrousli");

    // Linux builds through `crate::builds`; this is the Windows build.
    if !cfg!(windows) {
        return build_here(arch, &root);
    }
    let script = "build-windows.sh";
    let mut command = Command::new(ferrousli::git_bash()?);
    // One spelling of the directory for both the script and its caller,
    // with the separators bash expects.
    let spelled = root.to_string_lossy().replace('\\', "/");
    let _ = command.env(VAR, &spelled).args([
        "-c",
        SCRIPT,
        "bash",
        "build-windows.sh",
        &spelled,
        arch.name(),
    ]);
    ferrousli::in_ferrousli(&mut command, &ferrousli);

    let description = format!("src/user/system/linux/ferrousli/tools/busybox/{script}");
    if let Err(error) = cargo::run(command, &description) {
        let list = root.join(ferrousli::UNDEFINED);
        return Err(match std::fs::read_to_string(&list) {
            Ok(symbols) if !symbols.trim().is_empty() => {
                ferrousli::not_linked("busybox", &symbols, &list)
            }
            _ => error,
        });
    }
    if !program.is_file() {
        return Err(Error::new(format!(
            "{script} reported success but {} does not exist",
            program.display()
        )));
    }
    println!("\nbuilt {}", program.display());
    Ok(program)
}

/// The Linux build, through [`crate::builds::Build`]: a build `FERRIX_BUILDS`
/// may record or replay, reading the sources `build.sh` would otherwise
/// download from `root/src`.
fn build_here(arch: Arch, root: &Path) -> Result<PathBuf> {
    let program = installed(root, arch);
    let ferrousli = crate::paths::workspace_root().join("src/user/system/linux/ferrousli");
    let mut build = crate::builds::Build::bash(
        "src/user/system/linux/ferrousli/tools/busybox/build.sh",
        &ferrousli,
    )
    .args(["-c", SCRIPT, "bash", "build.sh"])
    .args([root.as_os_str(), std::ffi::OsStr::new(arch.name())])
    .reads_dir(root.join("src"))
    .output(&program);
    if let Some(dir) = ferrousli::target_dir(std::env::var_os("CARGO_TARGET_DIR")) {
        build = build.env("CARGO_TARGET_DIR", dir);
    }
    if let Err(error) = build.run() {
        let list = root.join(ferrousli::UNDEFINED);
        return Err(match std::fs::read_to_string(&list) {
            Ok(symbols) if !symbols.trim().is_empty() => {
                ferrousli::not_linked("busybox", &symbols, &list)
            }
            _ => error,
        });
    }
    if !program.is_file() {
        return Err(Error::new(format!(
            "build.sh reported success but {} does not exist",
            program.display()
        )));
    }
    println!("\nbuilt {}", program.display());
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_builds_only_x86_64s_busybox() {
        assert!(refuse_unbuildable(Arch::X86_64).is_ok());
        for arch in [Arch::AArch64, Arch::Armv7a] {
            assert_eq!(refuse_unbuildable(arch).is_err(), cfg!(windows), "{arch}");
        }
    }

    #[test]
    fn the_installed_path_matches_the_musl_busybox_layout() {
        let path = installed(Path::new("root"), Arch::X86_64);
        assert_eq!(path, Path::new("root/x86_64/bin/busybox.static"));
    }
}
