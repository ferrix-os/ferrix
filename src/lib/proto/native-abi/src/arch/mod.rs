//! What differs by the architecture a program is built for: the ABI's one
//! selection per target (`check-crate-layering.sh`, rule 4).

/// The registers of the architecture this is built for.
#[cfg(target_arch = "x86_64")]
pub type Registers = crate::exception::X86_64Registers;
/// The registers of the architecture this is built for.
#[cfg(target_arch = "aarch64")]
pub type Registers = crate::exception::Aarch64Registers;
/// The registers of the architecture this is built for.
#[cfg(target_arch = "arm")]
pub type Registers = crate::exception::Armv7aRegisters;

/// The restricted-mode context of the architecture this is built for.
#[cfg(not(target_arch = "aarch64"))]
pub type Context = crate::restricted::ContextX86_64;
/// The restricted-mode context of the architecture this is built for.
#[cfg(target_arch = "aarch64")]
pub type Context = crate::restricted::ContextAarch64;

/// The vector image's bytes on the architecture this is built for.
#[cfg(not(target_arch = "aarch64"))]
pub const VECTORS_BYTES: usize = crate::restricted::VECTORS_BYTES_X86_64;
/// The vector image's bytes on the architecture this is built for.
#[cfg(target_arch = "aarch64")]
pub const VECTORS_BYTES: usize = crate::restricted::VECTORS_BYTES_AARCH64;
