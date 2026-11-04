//! Linked where `linuxd` is (`src/user/system/native/linuxd/build.rs`).

fn main() {
    println!("cargo::rustc-link-arg=--image-base=0x710000000000");
    println!("cargo::rerun-if-changed=build.rs");
}
