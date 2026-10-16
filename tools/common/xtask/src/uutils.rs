//! uutils/coreutils built against ferrousli: the utilities that replace
//! busybox's, as one static multicall binary.
//!
//! `src/user/system/linux/ferrousli/tools/uutils/build.sh` builds it, or `build-windows.sh` beside
//! it on Windows, and installs it as `x86_64/bin/coreutils` under
//! `~/.local/share/ferrix/uutils/ferrousli` (or `$FERRIX_UUTILS`).
//!
//! It is Rust, so unlike busybox it brings its own `std`, and `std` for the
//! musl target brings its own unwinder; ferrousli answers everything below
//! them. `docs/UUTILS.md` is the plan this is the second slice of, and §3a
//! there says why the target is `x86_64-unknown-linux-musl` rather than the
//! `-gnu` one ferrousli is closer to.
//!
//! What this module shares with [`crate::busybox`] is in [`crate::ferrousli`].

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, cargo, ferrousli};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "uutils",
    help_at: 1090,
    help: &[
        "uutils        Build uutils/coreutils against ferrousli (x86_64), the utilities replacing busybox's",
    ],
    run: |_, args| build(args.single_arch()?).map(|_| ()),
}];

/// The environment variable naming the install directory, and its default.
const VAR: &str = "FERRIX_UUTILS";
/// The default install directory under the home directory.
const HOME_SEGMENTS: &[&str] = &[".local", "share", "ferrix", "uutils", "ferrousli"];

/// Run by `bash -c` in `src/user/system/linux/ferrousli/`, with `$1` the build script in
/// `tools/uutils/` and `$2` the directory to install into.
///
/// `$out` is the script's own: `$FERRIX_UUTILS`, with its default. Normally it
/// is the install directory itself, and the copy is skipped. Both lists are
/// removed first, so a list read after a failure is this run's.
const SCRIPT: &str = r#"set -uo pipefail
root=$2
out=${FERRIX_UUTILS:-$HOME/.local/share/ferrix/uutils/ferrousli}
mkdir -p "$root/x86_64/bin" || exit 1
rm -f "$out/undefined-symbols.txt" "$root/undefined-symbols.txt"
if ! bash "tools/uutils/$1"; then
    if [ -f "$out/undefined-symbols.txt" ] && ! [ "$out" -ef "$root" ]; then
        cp "$out/undefined-symbols.txt" "$root/"
    fi
    exit 1
fi
# Normally $out is the install directory itself and the scripts have already
# put the programs where they are read from; the copy is for a caller whose
# $FERRIX_UUTILS is somewhere else.
if ! [ "$out" -ef "$root" ]; then
    cp -a "$out/x86_64/bin/." "$root/x86_64/bin/" || exit 1
fi
"#;

/// What a uutils built against ferrousli is made from, relative to
/// `src/user/system/linux/ferrousli/`: the library and its entry object, and the pinned sources and
/// the build scripts.
const INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "crt",
    "include",
    "src",
    "tools/uutils",
];

/// The directory uutils is installed under.
fn root() -> Result<PathBuf> {
    ferrousli::install_root(VAR, HOME_SEGMENTS, "ferrousli's uutils")
}

/// The installed binary called `name` for `arch`, beneath `root`.
fn installed(root: &Path, arch: Arch, name: &str) -> PathBuf {
    root.join(arch.name()).join("bin").join(name)
}

/// The first of the family, which stands for the rest: they are built and
/// installed in one run, so if this one is current they all are.
fn first(root: &Path, arch: Arch) -> PathBuf {
    let name = crate::initramfs::FAMILY
        .first()
        .map_or("coreutils", |binary| binary.name);
    installed(root, arch, name)
}

/// Refuse every architecture the build scripts do not build for.
///
/// x86-64 only, because ferrousli is. The musl target this is built for has
/// an AArch64 and an ARMv7-A spelling, and the day ferrousli is built for
/// those this is most of what it takes to follow.
fn refuse_other_than_x86_64(arch: Arch) -> Result<()> {
    if arch == Arch::X86_64 {
        Ok(())
    } else {
        Err(Error::new(format!(
            "ferrousli's uutils is built for x86_64 only, not for {arch}"
        )))
    }
}

/// The installed uutils for `arch`, built first when it is missing or older
/// than ferrousli, so what boots is the tree as it stands.
///
/// Never another build in its place: a build that fails is the error.
pub(crate) fn program(arch: Arch) -> Result<PathBuf> {
    refuse_other_than_x86_64(arch)?;
    let root = root()?;
    let program = first(&root, arch);
    let dir = crate::paths::workspace_root().join("src/user/system/linux/ferrousli");
    if !crate::builds::active() && ferrousli::stale(&program, &dir, INPUTS).is_none() {
        return Ok(program);
    }
    // Another checkout may be building into the same directory: wait for it,
    // then look again, since what it installed may be current.
    let _lock = ferrousli::lock_builds(&root, "uutils")?;
    match ferrousli::stale(&program, &dir, INPUTS) {
        Some(reason) => {
            println!("  ferrousli's uutils {reason}; building it");
            build_locked(arch, &root)
        }
        None => Ok(program),
    }
}

/// `cargo xtask uutils`: run `build.sh`, or `build-windows.sh` on Windows,
/// and install the multicall binary.
pub(crate) fn build(arch: Arch) -> Result<PathBuf> {
    refuse_other_than_x86_64(arch)?;
    let root = root()?;
    let _lock = ferrousli::lock_builds(&root, "uutils")?;
    build_locked(arch, &root)
}

/// [`build`], with the build lock already held.
fn build_locked(arch: Arch, root: &Path) -> Result<PathBuf> {
    let root = root.to_path_buf();
    let program = first(&root, arch);
    let dir = crate::paths::workspace_root().join("src/user/system/linux/ferrousli");

    // Linux builds through `crate::builds`; this is the Windows build.
    if !cfg!(windows) {
        return build_here(arch, &root);
    }
    let script = "build-windows.sh";
    let mut command = Command::new(ferrousli::git_bash()?);
    // One spelling of the directory for both the script and its caller,
    // with the separators bash expects.
    let spelled = root.to_string_lossy().replace('\\', "/");
    let _ = command
        .env(VAR, &spelled)
        .args(["-c", SCRIPT, "bash", "build-windows.sh", &spelled]);
    ferrousli::in_ferrousli(&mut command, &dir);

    let description = format!("src/user/system/linux/ferrousli/tools/uutils/{script}");
    if let Err(error) = cargo::run(command, &description) {
        let list = root.join(ferrousli::UNDEFINED);
        return Err(match std::fs::read_to_string(&list) {
            Ok(symbols) if !symbols.trim().is_empty() => {
                ferrousli::not_linked("uutils/coreutils", &symbols, &list)
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

/// The bytes an image carries, built when stale, or `None` on an architecture
/// ferrousli is not built for.
///
/// `None` rather than an error: an image for AArch64 or ARMv7-A is a whole
/// image without these utilities, and refusing to build one would be the
/// wrong answer to "this is x86-64 only for now".
pub(crate) fn carried(arch: Arch) -> Result<Vec<(&'static str, Vec<u8>)>> {
    if arch != Arch::X86_64 {
        println!("  uutils is not built for {} yet", arch.name());
        return Ok(Vec::new());
    }
    // Builds the lot when any of them is stale, and gives back where they
    // were installed.
    let watched = program(arch)?;
    let dir = watched
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::new(format!("{} has no directory", watched.display())))?;
    // A family program that is not installed is left out rather than refused,
    // and the names it owns fall to busybox in the image, exactly as they do
    // on an architecture uutils is not built for. The Windows build makes
    // coreutils alone -- findutils will not link against ferrousli there yet,
    // `rust_begin_unwind` being defined twice -- and before this, an image
    // built on Windows failed on the first name it could not find.
    let mut carried = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for binary in crate::initramfs::FAMILY {
        let path = dir.join(binary.name);
        if !path.is_file() {
            missing.push(binary.name);
            continue;
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
        carried.push((binary.name, bytes));
    }
    if !missing.is_empty() {
        println!(
            "  not built, so busybox keeps their names: {} (from {})",
            missing.join(", "),
            dir.display()
        );
    }
    Ok(carried)
}

/// The Linux build, through [`crate::builds::Build`]: a build `FERRIX_BUILDS`
/// may record or replay, reading the archives `build.sh` would otherwise
/// download from `root/src`, and making every program of the family.
fn build_here(arch: Arch, root: &Path) -> Result<PathBuf> {
    let program = first(root, arch);
    let ferrousli = crate::paths::workspace_root().join("src/user/system/linux/ferrousli");
    let mut build = crate::builds::Build::bash(
        "src/user/system/linux/ferrousli/tools/uutils/build.sh",
        &ferrousli,
    )
    .args(["-c", SCRIPT, "bash", "build.sh"])
    .args([root])
    .reads_dir(root.join("src"));
    for binary in crate::initramfs::FAMILY {
        build = build.output(installed(root, arch, binary.name));
    }
    if let Some(dir) = ferrousli::target_dir(std::env::var_os("CARGO_TARGET_DIR")) {
        build = build.env("CARGO_TARGET_DIR", dir);
    }
    if let Err(error) = build.run() {
        let list = root.join(ferrousli::UNDEFINED);
        return Err(match std::fs::read_to_string(&list) {
            Ok(symbols) if !symbols.trim().is_empty() => {
                ferrousli::not_linked("uutils/coreutils", &symbols, &list)
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
    fn only_x86_64_has_a_ferrousli_uutils() {
        for arch in [Arch::AArch64, Arch::Armv7a] {
            assert!(program(arch).is_err(), "{arch}");
            assert!(build(arch).is_err(), "{arch}");
        }
    }

    #[test]
    fn the_installed_path_sits_beside_the_other_programs() {
        let path = installed(Path::new("root"), Arch::X86_64, "xargs");
        assert_eq!(path, Path::new("root/x86_64/bin/xargs"));
        assert_eq!(
            first(Path::new("root"), Arch::X86_64),
            Path::new("root/x86_64/bin/coreutils"),
            "coreutils is the one the staleness rule watches"
        );
    }
}
