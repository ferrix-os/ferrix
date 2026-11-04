//! The images, and what a program's registers are called, per architecture.

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::*;
#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::*;

/// ARMv7-A has no restricted mode (`docs/microkernel/K1.md` §8): the check starts this
/// program there, and it has nothing to run.
#[cfg(target_arch = "arm")]
pub(crate) fn run(_bootstrap: ferrix_rt::Bootstrap) -> i32 {
    0
}
#[cfg(target_arch = "arm")]
use {ferrix_linux_abi as _, ferrix_native_abi as _};
#[cfg(not(target_arch = "arm"))]
#[path = "../cases.rs"]
mod cases;
#[cfg(not(target_arch = "arm"))]
pub(crate) use cases::run;
