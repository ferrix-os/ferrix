//! linuxd lives in its region, above `SHARED_BASE`, wherever K1 can reach it from
//! a restricted exit (`docs/microkernel/K11.md` §2.1). It takes the one flag its
//! own root reads, as the kernel's composition root does.

fn main() {
    println!("cargo::rustc-link-arg=--image-base=0x710000000000");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=FERRIX_BUILD_FLAGS");
    println!("cargo::rustc-check-cfg=cfg(ferrix_runtime_settings, values(\"fixed\", \"live\"))");
    if let Ok(flags) = std::env::var("FERRIX_BUILD_FLAGS") {
        for word in flags.split_whitespace() {
            if let Some(value) = word.strip_prefix("runtime_settings=") {
                println!("cargo::rustc-cfg=ferrix_runtime_settings=\"{value}\"");
            }
        }
    }
}
