//! `domain-exec` is its own entry point: no C library start-up, which would
//! read a Linux start-up stack the process does not have.

fn main() {
    println!("cargo:rustc-link-arg-bin=domain-exec=-nostartfiles");
    println!("cargo:rerun-if-changed=build.rs");
}
