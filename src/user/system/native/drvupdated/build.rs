//! Link with the runtime's layout. See `src/user/system/native/rt/build.rs` for why the path
//! arrives this way.

// A build script talks to cargo over stdout; that is the whole interface.
#![allow(
    clippy::print_stdout,
    reason = "stdout is how a build script communicates with cargo"
)]

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let script = std::env::var("DEP_FERRIX_RT_LINKER_SCRIPT")?;
    println!("cargo::rustc-link-arg-bins=-T{script}");
    println!("cargo::rerun-if-changed={script}");
    println!("cargo::rerun-if-env-changed=DEP_FERRIX_RT_LINKER_SCRIPT");
    Ok(())
}
