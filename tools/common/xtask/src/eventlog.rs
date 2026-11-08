//! `ferrix-eventlog`, `/bin/ferrix-eventlog` (`src/lib/proto/eventlog`'s
//! program, `docs/POSTMORTEM.md` §4.4): the event log's reader, built for an
//! architecture against the target's own musl and carried by every image
//! with a shell, so that `ferrix-eventlog list /dev/vda3 --text` reads a
//! machine's panic records on the machine itself, not only on a host that
//! has its disk. The same source is the host's tool.

use std::path::PathBuf;

use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::{Error, Result, zinc};

/// The crate, in the workspace.
const PACKAGE: &str = "ferrix-eventlog";

/// Where the program goes.
pub(crate) const PROGRAM: &str = "bin/ferrix-eventlog";

/// Build `ferrix-eventlog` for `arch`, or `None` where there is no musl
/// target.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn build(arch: Arch) -> Result<Option<PathBuf>> {
    let Some(target) = zinc::target(arch) else {
        println!("  ferrix-eventlog is not built for {arch} yet");
        return Ok(None);
    };
    let target_dir = paths::target_dir().join("eventlog");
    let program = target_dir.join(target).join("release").join(PACKAGE);
    crate::builds::Build::cargo(
        format!("cargo build ({PACKAGE}) --target {target}"),
        paths::workspace_root(),
    )
    .args([
        "build",
        "--release",
        "--package",
        PACKAGE,
        "--bin",
        PACKAGE,
        "--target",
        target,
        // The workspace's release profile keeps the kernel's symbols; a
        // program in the image needs none of them.
        "--config",
        "profile.release.debug=false",
        "--config",
        "profile.release.strip=true",
    ])
    .env("CARGO_TARGET_DIR", &target_dir)
    // As dmesg's: RUSTFLAGS replaces the flags every config file up the
    // tree would merge, and rust-lld is the whole toolchain with the musl
    // targets' own C runtime.
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .env(&crate::apps::linker_variable(target), "rust-lld")
    .output(&program)
    .run()?;
    Ok(Some(program))
}

/// What an image carries for the event log on `arch`: `/bin/ferrix-eventlog`.
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
