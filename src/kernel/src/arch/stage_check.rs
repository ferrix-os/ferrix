//! The architecture's stage checks, by one name on every architecture: the
//! boot check for exceptions that arrive wherever the processor is (stage 3)
//! and what the switch gives a program back (stage 9).
//!
//! Each is check code, and lives in the check file it runs
//! (`<isa>/trap/check.rs`, `<isa>/switch/check.rs`); this file only gives the
//! stage checks one name for it, as `arch/mod.rs` does for the product
//! interface. Until 2026-10-15 these wrappers were product functions in each
//! architecture's `mod.rs`; they moved so that no product file names a check
//! that runs Linux programs (`docs/KERNEL-CRATES.md` §3 (3)).

// The boot check for exceptions that arrive wherever the processor is.
#[cfg(target_arch = "aarch64")]
pub use super::aarch64::trap::check::check_exception_entry;
#[cfg(target_arch = "arm")]
pub use super::armv7a::trap::check::check_exception_entry;
#[cfg(target_arch = "x86_64")]
pub use super::x86_64::trap::check::check_exception_entry;

// The core's own system call entry, run with a call the boot check chose
// (`syscall::seccomp_check`). Until the kernel split these were re-exported by
// `arch/mod.rs`; on the Arm pair they are in the raised `trap/check.rs`, so
// they are reached through here (ledger line 1585, Q2).
#[cfg(target_arch = "aarch64")]
pub use super::aarch64::trap::check::{drive_native_words, drive_system_call};
#[cfg(target_arch = "arm")]
pub use super::armv7a::trap::check::{drive_native_words, drive_system_call};
#[cfg(target_arch = "x86_64")]
pub use ferrix_kernel_core::arch::x86_64::syscall::check::{drive_native_words, drive_system_call};

// What the switch gives a program back, checked in user mode: x86-64's
// vector-state contract and its FS base kept in the task (docs/OPAQUE-KERNEL.md
// §9.8, 3a and 3b), and ARMv7-A's thread ID registers (§9.14, 3b and F-66).
#[cfg(target_arch = "arm")]
pub use super::armv7a::switch::check::check_switch_state;
#[cfg(target_arch = "x86_64")]
pub use super::x86_64::switch::check::check_switch_state;

/// AArch64: no vector-state contract and no record of its own here yet.
///
/// # Errors
///
/// None.
#[cfg(target_arch = "aarch64")]
pub const fn check_switch_state() -> Result<(), &'static str> {
    Ok(())
}
