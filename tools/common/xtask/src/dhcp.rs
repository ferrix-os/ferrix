//! The DHCP client, `/bin/dhcpc` (`src/user/system/linux/dhcp`): built for
//! an architecture and carried by every image that takes a lease -- the
//! desktop's, `test-net`'s, `devloop`'s and the scripts' that ask for one
//! before they fetch. It replaces busybox's `udhcpc` and the lease script
//! that ran `ip` and `sed` for it: the client applies the lease itself, over
//! rtnetlink.

use std::path::PathBuf;

use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::{Error, Result, zinc};

/// The workspace.
const DIR: &str = "src/user/system/linux/dhcp";

/// Where the program goes.
pub(crate) const PROGRAM: &str = "bin/dhcpc";

/// Build `dhcpc` for `arch`, or `None` where there is no musl target.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn build(arch: Arch) -> Result<Option<PathBuf>> {
    let Some(target) = zinc::target(arch) else {
        println!("  dhcpc is not built for {arch} yet");
        return Ok(None);
    };
    let target_dir = paths::target_dir().join("dhcp");
    let program = target_dir.join(target).join("release").join("dhcpc");
    crate::builds::Build::cargo(
        format!("cargo build (dhcpc) --target {target}"),
        paths::workspace_root().join(DIR),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    // As pkg's: RUSTFLAGS replaces the flags every config file up the tree
    // would merge, and rust-lld is the whole toolchain with the musl
    // targets' own C runtime.
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .env(&crate::apps::linker_variable(target), "rust-lld")
    .output(&program)
    .run()?;
    Ok(Some(program))
}

/// What an image carries for DHCP on `arch`: `/bin/dhcpc`.
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
