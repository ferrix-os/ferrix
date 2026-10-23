//! The Ferrix kernel's item ring above the core: the native and Linux system
//! call entry, device discovery, the checks flag's parsing, power and
//! randomness.
//!
//! One of the kernel's four crates (`docs/KERNEL-CRATES.md`): it depends on the
//! core alone and re-exports the core's modules, so that `crate::mm` names in an
//! item file what it named before the split. Its `pub` items are listed in
//! `docs/generated/KERNEL-INTERFACE.md`.

#![no_std]
// The same allow as `main.rs`'s, for this crate: a kernel without the Linux
// ABI is build-only and carries no claim (LINUX-ABI-SEPARATION.md §11.11,
// ledger 3257 (3)), and each of the kernel's crates is compiled with its
// flags. Never in a build with `linux_abi=on`.
#![cfg_attr(
    ferrix_linux_abi = "off",
    allow(
        dead_code,
        unused_imports,
        unused_variables,
        unfulfilled_lint_expectations,
        unused_crate_dependencies,
        reason = "linux_abi=off is build-only until the checks that run a Linux program are re-homed"
    )
)]

extern crate alloc;

/// The build flags this crate was compiled with (`build_flag_word!`, ledger
/// line 3282, (K4)).
#[allow(
    unused_qualifications,
    reason = "named by its crate's path, so the interface's list sees the core's fn used here"
)]
pub const ITEM_FLAG_WORD: u64 =
    ferrix_kernel_core::flag_word(&ferrix_kernel_core::build_flag_word!());

pub use ferrix_kernel_core::*;

pub mod build_flags;
pub mod checks;
pub mod discovery;
pub mod fastpath;
pub mod init;
pub mod init_check;
pub mod power;
pub mod random;
pub mod syscall;
/// The architecture modules (the core's), with x86-64's framebuffer window check raised to this crate.
pub mod arch {
    pub use ferrix_kernel_core::arch::*;
    /// x86-64's raised check.
    pub mod x86_64 {
        /// The framebuffer window's check.
        pub mod framebuffer {
            pub mod check;
        }
    }
}
