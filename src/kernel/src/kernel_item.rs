//! The Ferrix kernel's item ring above the core: the native and Linux system
//! call entry, device discovery, the checks flag's parsing, power and
//! randomness.
//!
//! One of the kernel's four crates (`docs/KERNEL-CRATES.md`): it depends on the
//! core alone and re-exports the core's modules, so that `crate::mm` names in an
//! item file what it named before the split. Its `pub` items are listed in
//! `docs/generated/KERNEL-INTERFACE.md`.

#![no_std]

extern crate alloc;

pub use ferrix_kernel_core::*;

pub mod checks;
pub mod discovery;
pub mod fastpath;
pub mod init;
pub mod init_check;
pub mod power;
pub mod random;
pub mod syscall;
