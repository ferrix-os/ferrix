//! `domain-exec` enters at its own function, past the C library's start-up,
//! which would read a Linux start-up stack the process does not have.

fn main() {
    // The C start-up objects are still linked; the entry is set past them.
    println!("cargo:rustc-link-arg-bin=domain-exec=-Wl,-e,domain_exec_start");
    println!("cargo:rerun-if-changed=build.rs");
}
