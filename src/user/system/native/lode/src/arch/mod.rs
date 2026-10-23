//! The Linux calls whose numbers differ in name between the architectures:
//! ARMv7-A's 32-bit uid calls and `mmap2`, where the 64-bit tables have the
//! plain names. Nothing else in lode names an architecture.

use ferrix_rt::linux::numbers;

/// `fchown`.
#[cfg(target_arch = "arm")]
pub(crate) const FCHOWN: usize = numbers::FCHOWN32;
/// `fchown`.
#[cfg(not(target_arch = "arm"))]
pub(crate) const FCHOWN: usize = numbers::FCHOWN;

/// `setgroups`.
#[cfg(target_arch = "arm")]
pub(crate) const SETGROUPS: usize = numbers::SETGROUPS32;
/// `setgroups`.
#[cfg(not(target_arch = "arm"))]
pub(crate) const SETGROUPS: usize = numbers::SETGROUPS;

/// `setresgid`.
#[cfg(target_arch = "arm")]
pub(crate) const SETRESGID: usize = numbers::SETRESGID32;
/// `setresgid`.
#[cfg(not(target_arch = "arm"))]
pub(crate) const SETRESGID: usize = numbers::SETRESGID;

/// `setresuid`.
#[cfg(target_arch = "arm")]
pub(crate) const SETRESUID: usize = numbers::SETRESUID32;
/// `setresuid`.
#[cfg(not(target_arch = "arm"))]
pub(crate) const SETRESUID: usize = numbers::SETRESUID;

/// `mmap`, with its offset in pages on ARMv7-A (`mmap2`), which lode only
/// ever passes as 0.
#[cfg(target_arch = "arm")]
pub(crate) const MMAP: usize = numbers::MMAP2;
/// `mmap`.
#[cfg(not(target_arch = "arm"))]
pub(crate) const MMAP: usize = numbers::MMAP;
