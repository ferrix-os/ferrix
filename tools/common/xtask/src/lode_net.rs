//! lode over the network (`src/user/system/linux/lode-net`,
//! `docs/LOGGING.md` §10.2, §10.3): `lode-upload` built for an
//! architecture and carried by the images that send their log on, and
//! `lode-receive` built for this host.

use std::path::PathBuf;

use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::{Error, Result, zinc};

/// The workspace.
const DIR: &str = "src/user/system/linux/lode-net";

/// Where the uploader goes.
pub(crate) const PROGRAM: &str = "bin/lode-upload";

/// Build `lode-upload` for `arch`, or `None` where there is no musl target.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn build(arch: Arch) -> Result<Option<PathBuf>> {
    let Some(target) = zinc::target(arch) else {
        println!("  lode-upload is not built for {arch} yet");
        return Ok(None);
    };
    let target_dir = paths::target_dir().join("lode-net");
    let program = target_dir.join(target).join("release").join("lode-upload");
    crate::builds::Build::cargo(
        format!("cargo build (lode-upload) --target {target}"),
        paths::workspace_root().join(DIR),
    )
    .args([
        "build",
        "--release",
        "--bin",
        "lode-upload",
        "--target",
        target,
    ])
    .env("CARGO_TARGET_DIR", &target_dir)
    // As dhcp's: a static std program linked by rust-lld against the
    // target's own musl; the TLS is pure Rust, so no C compiler is asked.
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .env(&crate::apps::linker_variable(target), "rust-lld")
    .output(&program)
    .run()?;
    Ok(Some(program))
}

/// Build `lode-receive` for this host.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn build_receiver() -> Result<PathBuf> {
    let target_dir = paths::target_dir().join("lode-net-host");
    let program = target_dir.join("release").join("lode-receive");
    crate::builds::Build::cargo(
        "cargo build (lode-receive)",
        paths::workspace_root().join(DIR),
    )
    .args(["build", "--release", "--bin", "lode-receive"])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program)
    .run()?;
    Ok(program)
}

/// What an image carries to send its log on: `/bin/lode-upload`.
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
