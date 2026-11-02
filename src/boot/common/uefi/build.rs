//! Where the ARMv7-A loader's link script is named, and which trust-policy
//! block the loader carries (`trust_policy`).
//!
//! # Why this is not in `.cargo/config.toml`
//!
//! It was, as `-Tboot/uefi/linker/armv7a.ld`, and that path had two problems the
//! kernel's link script had before it (see `src/kernel/build.rs`). Cargo merges
//! every `.cargo/config.toml` from the invocation directory up to the root,
//! joining `rustflags` end to end, so a worktree inside another checkout
//! linked with the script given twice. And the path was relative to wherever
//! cargo was run, so a worktree whose tree differs from the enclosing
//! checkout's -- one on a branch from before a directory moved -- was handed
//! a script that does not exist in it, and its loader failed to link.
//!
//! A build script runs once per build of this package, and names the script
//! absolutely, from `CARGO_MANIFEST_DIR`. Only the ARMv7-A target has one: the
//! x86-64 and AArch64 loaders are PE images the UEFI targets lay out
//! themselves. The other ARMv7-A flags stay in `.cargo/config.toml`, where
//! being passed twice changes nothing.

// A build script talks to cargo over stdout; that is the whole interface.
#![allow(
    clippy::print_stdout,
    reason = "stdout is how a build script communicates with cargo"
)]

use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=build.rs");
    trust_policy()?;
    bad_frames()?;
    // A reproducible PE (lld-link's /Brepro): its timestamp and debug
    // signature are hashes of its contents, not the clock, so one commit
    // and one block build the same bytes, which the signing gate's relink
    // comparison needs (docs/SIGNING.md §6.4; ledger 4016 (5)).
    if std::env::var("CARGO_CFG_TARGET_OS")? == "uefi" {
        println!("cargo::rustc-link-arg-bins=/Brepro");
    }
    if std::env::var("CARGO_CFG_TARGET_ARCH")? != "arm" {
        return Ok(());
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
    let script = Path::new(&manifest).join("linker").join("armv7a.ld");
    println!("cargo::rustc-link-arg-bins=-T{}", script.display());
    // Relink when the script changes, which cargo would not otherwise notice.
    println!("cargo::rerun-if-changed={}", script.display());
    Ok(())
}

/// Why a build without `FERRIX_ANCHORS` is refused, never given a default.
const ANCHORS_NOT_SET: &str = "FERRIX_ANCHORS is not set: the loader is built by xtask, which names its \
     trust-policy block (docs/SIGNING.md §10.6)";

/// The trust-policy blocks `tools/common/gen/gen-anchors.py` writes.
const TABLES: [&str; 4] = [
    "release-off",
    "release-required",
    "development-off",
    "development-required",
];

/// The loader's bad-frame mode (`docs/MACHINE-CHECK-RESTART.md` §10.2,
/// ledger 5579 (3)): `FERRIX_BAD_FRAMES` is the image key `bad_frames`,
/// `persist` or `choice`, which xtask always sets; a build without it, or
/// with another value, fails. It becomes `cfg(ferrix_bad_frames = "...")`,
/// under which a `persist` loader compiles no read of `ferrix.badframes`.
fn bad_frames() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-env-changed=FERRIX_BAD_FRAMES");
    println!("cargo::rustc-check-cfg=cfg(ferrix_bad_frames, values(\"persist\", \"choice\"))");
    let value = std::env::var("FERRIX_BAD_FRAMES").map_err(|_| {
        "FERRIX_BAD_FRAMES is not set: the loader is built by xtask, which sets it from the \
         profile's image key bad_frames (docs/MACHINE-CHECK-RESTART.md §10.2)"
    })?;
    if !["persist", "choice"].contains(&value.as_str()) {
        return Err(format!("FERRIX_BAD_FRAMES is {value:?}, not persist or choice").into());
    }
    println!("cargo::rustc-cfg=ferrix_bad_frames=\"{value}\"");
    Ok(())
}

/// Choose the loader's trust-policy block and say what the build is
/// (`docs/SIGNING.md` §10.6). Both are build inputs xtask sets, never a
/// Cargo feature or `cfg`, and a build without them fails rather than
/// taking a default (ledger 4016 (4)):
///
/// * `FERRIX_ANCHORS` names one of [`TABLES`], which goes into `.fxanch`;
/// * `FERRIX_SLOT_BUILD` is `profile=<byte> claimed=<0|1> families=<ids>`,
///   the ids decimal and comma-separated, of which this arch's are kept.
fn trust_policy() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-env-changed=FERRIX_ANCHORS");
    println!("cargo::rerun-if-env-changed=FERRIX_SLOT_BUILD");
    let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
    let dir = Path::new(&manifest).join("anchors");
    for table in TABLES {
        println!(
            "cargo::rerun-if-changed={}",
            dir.join(format!("{table}.bin")).display()
        );
    }
    let table = std::env::var("FERRIX_ANCHORS").ok();
    let Some(table) = table else {
        return Err(ANCHORS_NOT_SET.into());
    };
    if !TABLES.contains(&table.as_str()) {
        return Err(format!("FERRIX_ANCHORS is {table:?}, not one of {TABLES:?}").into());
    }
    // The block as an array literal, not include_bytes!: a byte-string
    // literal is an allocation of its own, which a debug build keeps beside
    // the static, and the loader must hold the block once, in .fxanch.
    let block = std::fs::read(dir.join(format!("{table}.bin")))?;
    if block.len() != 512 {
        return Err(format!("{table}.bin is {} bytes, not 512", block.len()).into());
    }
    let bytes: Vec<String> = block.iter().map(|b| format!("{b:#04x}")).collect();
    let out = Path::new(&std::env::var("OUT_DIR")?).join("trust_policy.rs");
    std::fs::write(
        &out,
        format!(
            "/// The block, in a read-only section of its own.\n\
             #[unsafe(link_section = \".fxanch\")]\n#[used]\n\
             static TRUST_POLICY: [u8; 512] = [{}];\n",
            bytes.join(", ")
        ),
    )?;

    let arch: u32 = match std::env::var("CARGO_CFG_TARGET_ARCH")?.as_str() {
        "x86_64" => 1,
        "aarch64" => 2,
        "arm" => 3,
        other => return Err(format!("no arch byte for {other}").into()),
    };
    let line = std::env::var("FERRIX_SLOT_BUILD").map_err(|_| {
        "FERRIX_SLOT_BUILD is not set: the loader is built by xtask, which says \
         what the build is (docs/SIGNING.md §10.6)"
    })?;
    let (mut profile, mut claimed, mut families) = (None, None, None);
    for word in line.split_whitespace() {
        match word.split_once('=') {
            Some(("profile", value)) => profile = Some(value.parse::<u8>()?),
            Some(("claimed", "0")) => claimed = Some(false),
            Some(("claimed", "1")) => claimed = Some(true),
            Some(("families", "")) => families = Some(Vec::new()),
            Some(("families", value)) => {
                families = Some(
                    value
                        .split(',')
                        .map(str::parse::<u32>)
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            _ => return Err(format!("FERRIX_SLOT_BUILD: {word:?} is not understood").into()),
        }
    }
    let (Some(profile), Some(claimed), Some(families)) = (profile, claimed, families) else {
        return Err(
            format!("FERRIX_SLOT_BUILD {line:?} lacks profile, claimed or families").into(),
        );
    };
    let ours: Vec<String> = families
        .iter()
        .filter(|id| *id >> 8 == arch)
        .map(|id| format!("{id:#06x}"))
        .collect();
    if ours.len() > 4 {
        return Err(format!(
            "FERRIX_SLOT_BUILD names {} families of this arch; at most 4",
            ours.len()
        )
        .into());
    }
    let out = Path::new(&std::env::var("OUT_DIR")?).join("slot_build.rs");
    std::fs::write(
        &out,
        format!(
            "/// The build's profile byte.\npub(crate) const PROFILE: u8 = {profile};\n\
             /// The build's arch byte.\npub(crate) const ARCH: u8 = {arch};\n\
             /// Whether its profile carries a claim.\npub(crate) const CLAIMED: bool = {claimed};\n\
             /// The families its profile accepts on this arch.\npub(crate) const FAMILIES: &[u32] = &[{}];\n",
            ours.join(", ")
        ),
    )?;
    Ok(())
}
