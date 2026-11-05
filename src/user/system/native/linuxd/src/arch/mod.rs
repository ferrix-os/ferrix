//! What linuxd's first thread does that has no Rust spelling.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::onto_stack;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::onto_stack;
