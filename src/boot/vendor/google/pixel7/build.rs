//! Link the Pixel 7 loader at the address the Android bootloader loads it to,
//! and embed the kernel and initramfs it starts.
//!
//! The script is passed here rather than in `.cargo/config.toml` for the reason
//! `src/kernel/build.rs` gives: cargo joins `rustflags` from every config file
//! between the invocation directory and the root, so a checkout inside another
//! checkout would pass `-T` twice.
//!
//! The payload is named by `FERRIX_PIXEL7_KERNEL` and `FERRIX_PIXEL7_INITRD`,
//! the stripped `KERNEL.ELF` and `INITRD.IMG` that `cargo xtask flash --stage`
//! writes. ABL loads only one image, so the kernel and its archive have to be
//! inside it. Without the variables the loader is built with nothing to load,
//! which is what linting it needs, and says so when run.
//!
//! Beside each goes `<VARIABLE>_DIGEST`, the file's SHA-256, because the
//! file's mtime cannot be trusted to say it changed: cargo reruns this script
//! only for a file newer than its last run, and a kernel put back at the same
//! path with an older mtime would leave the old one embedded, to be booted on
//! the phone without anyone noticing. The variables are a rebuild key and
//! nothing more: the loader checks no digest at boot (VULNERABILITY-ANALYSIS
//! V-12).

#![allow(
    clippy::print_stdout,
    reason = "stdout is how a build script communicates with cargo"
)]

use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
    let script = Path::new(&manifest).join("linker").join("pixel7.ld");
    println!("cargo::rustc-link-arg-bins=-T{}", script.display());
    println!("cargo::rerun-if-changed={}", script.display());
    println!("cargo::rerun-if-changed=build.rs");

    // Words appended to the phone's kernel command line, `board::CMDLINE`.
    println!("cargo::rerun-if-env-changed=FERRIX_PIXEL7_CMDLINE_EXTRA");
    let extra = std::env::var("FERRIX_PIXEL7_CMDLINE_EXTRA").unwrap_or_default();
    let extra = extra.trim();
    if extra.contains(['\n', '\r']) {
        return Err("FERRIX_PIXEL7_CMDLINE_EXTRA must be one line".into());
    }
    let spaced = if extra.is_empty() {
        String::new()
    } else {
        format!(" {extra}")
    };
    println!("cargo::rustc-env=PIXEL7_CMDLINE_EXTRA={spaced}");

    println!("cargo::rustc-check-cfg=cfg(payload)");
    println!("cargo::rerun-if-env-changed=FERRIX_PIXEL7_KERNEL");
    println!("cargo::rerun-if-env-changed=FERRIX_PIXEL7_INITRD");
    let kernel = std::env::var_os("FERRIX_PIXEL7_KERNEL");
    let initrd = std::env::var_os("FERRIX_PIXEL7_INITRD");
    if let (Some(kernel), Some(initrd)) = (kernel, initrd) {
        for (name, path) in [("PIXEL7_KERNEL", &kernel), ("PIXEL7_INITRD", &initrd)] {
            let path = Path::new(path).canonicalize()?;
            println!("cargo::rerun-if-changed={}", path.display());
            println!("cargo::rustc-env={name}={}", path.display());
            content_named(&format!("FERRIX_{name}"));
        }
        println!("cargo::rustc-cfg=payload");
    }
    Ok(())
}

/// Declare `<variable>_DIGEST`, the SHA-256 of the file `variable` names, as
/// what reruns this script, and warn when it was not given: without it an
/// older file put back at the same path is not embedded.
fn content_named(variable: &str) {
    let digest = format!("{variable}_DIGEST");
    println!("cargo::rerun-if-env-changed={digest}");
    if std::env::var_os(&digest).is_none_or(|value| value.is_empty()) {
        println!(
            "cargo::warning={variable} is set without {digest}, the file's SHA-256: \
             an older file put back at the same path will not be embedded"
        );
    }
}
