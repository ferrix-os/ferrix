//! Link at the region's window, with a layout of this program's own: it runs
//! in a shared process's normal mode, above `SHARED_BASE`
//! (`docs/microkernel/K11.md` §2.1).

// A build script talks to cargo over stdout; that is the whole interface.
#![allow(
    clippy::print_stdout,
    reason = "stdout is how a build script communicates with cargo"
)]

use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
    // ARMv7-A has no region to be linked in (`docs/microkernel/K1.md` §8): the
    // runtime's own layout, for the empty program it is there.
    let script = if std::env::var("CARGO_CFG_TARGET_ARCH")? == "arm" {
        std::path::PathBuf::from(std::env::var("DEP_FERRIX_RT_LINKER_SCRIPT")?)
    } else {
        Path::new(&manifest).join("linker").join("region.ld")
    };
    println!("cargo::rustc-link-arg-bins=-T{}", script.display());
    println!("cargo::rerun-if-changed={}", script.display());
    println!("cargo::rerun-if-changed=build.rs");
    Ok(())
}
