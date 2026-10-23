//! Ferrix's `dmesg`, `/bin/dmesg` (`src/user/system/linux/dmesg`): built for
//! an architecture and carried by every image with a shell -- `test-shell`'s
//! and `run --init`'s, the init image `run` boots, `test-vfs`'s and the
//! desktop's. A file at `bin/dmesg` takes the name from busybox's applet
//! (`initramfs::busybox_owns`), so `dmesg` is this program, util-linux's
//! output and flags, rather than busybox's subset.

use std::path::PathBuf;

use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::{Error, Result, zinc};

/// The workspace.
const DIR: &str = "src/user/system/linux/dmesg";

/// Where the program goes.
pub(crate) const PROGRAM: &str = "bin/dmesg";

/// Build `dmesg` for `arch`, or `None` where there is no musl target.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn build(arch: Arch) -> Result<Option<PathBuf>> {
    let Some(target) = zinc::target(arch) else {
        println!("  dmesg is not built for {arch} yet");
        return Ok(None);
    };
    let target_dir = paths::target_dir().join("dmesg");
    let program = target_dir.join(target).join("release").join("dmesg");
    crate::builds::Build::cargo(
        format!("cargo build (dmesg) --target {target}"),
        paths::workspace_root().join(DIR),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    // As dhcpc's: RUSTFLAGS replaces the flags every config file up the
    // tree would merge, and rust-lld is the whole toolchain with the musl
    // targets' own C runtime.
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .env(&crate::apps::linker_variable(target), "rust-lld")
    .output(&program)
    .run()?;
    Ok(Some(program))
}

/// What an image carries for the kernel log on `arch`: `/bin/dmesg`.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn carried(arch: Arch) -> Result<Vec<File>> {
    let Some(program) = build(arch)? else {
        return Ok(Vec::new());
    };
    let bytes = std::fs::read(&program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    Ok(vec![File {
        path: PROGRAM.to_owned(),
        mode: 0o755,
        content: Content::Bytes(bytes),
    }])
}
