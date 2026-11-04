//! The instructions a native program cannot say in Rust, per architecture.
//!
//! Four things, each allow-listed in `tools/common/data/asm-allowlist.json`:
//!
//! * `_start`, because a process is *entered*, not called: the kernel drops to
//!   user mode at the entry point with a stack pointer and registers it chose,
//!   no return address, and no frame. A Rust function's prologue assumes all
//!   three are a caller's.
//! * The trap instruction and its register assignment, which is the kernel's
//!   system call entry read from the other side.
//! * `exit_group`, the same trap with nothing to return to.
//! * `dsb sy` on the Arm pair, the barrier a driver sharing memory with a
//!   device that does not snoop needs before the device may look.
//!
//! The trap is one block per architecture and serves both ABIs: `call` makes
//! a native call and `linux` a Linux one, and the kernel tells them apart by
//! the number's range and by nothing else (`docs/CLIPBOARD.md` §5).
//!
//! It also owns the two things about a *Linux* call that are the
//! architecture's rather than the caller's: the numbers, which are a
//! different table per architecture; the calls the tables disagree about
//! spelling -- `unlink`, which the newer ones have only as `unlinkat`; and
//! the width of a word a call writes, since a `timespec` is two of them and
//! a caller that guessed wrong would be handing the kernel the wrong amount
//! of its own stack. A caller in `crate::linux` therefore names an operation
//! and never an architecture.
//!
//! This is the facade: the one place under `src/user/system/native/` that selects on
//! `target_arch`, for the reason `src/kernel/src/arch/mod.rs` is the kernel's.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "arm")]
mod armv7a;
#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::restricted;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{
    call, counter, device_barrier, exit, linux, monotonic_nanos, nr, trap_words, unlink,
};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{
    call, counter, device_barrier, exit, linux, monotonic_nanos, nr, trap_words, unlink,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::restricted;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{
    call, counter, device_barrier, exit, linux, monotonic_nanos, nr, trap_words, unlink,
};

// Public, where the rest is the crate's: `crate::linux` hands it on to
// programs.
#[cfg(target_arch = "aarch64")]
pub use aarch64::ELF_MACHINE;
#[cfg(target_arch = "aarch64")]
pub use aarch64::numbers;
#[cfg(target_arch = "arm")]
pub use armv7a::ELF_MACHINE;
#[cfg(target_arch = "arm")]
pub use armv7a::numbers;
#[cfg(target_arch = "x86_64")]
pub use x86_64::ELF_MACHINE;
#[cfg(target_arch = "x86_64")]
pub use x86_64::numbers;

/// Restricted mode, which ARMv7-A does not have (`docs/microkernel/K1.md` §8).
#[cfg(not(target_arch = "arm"))]
#[path = "../restricted.rs"]
pub mod restricted;
#[cfg(target_arch = "arm")]
use ferrix_native_abi as _;

/// What this crate has by architecture beyond the trap.
pub(crate) mod modes {
    #[cfg(not(target_arch = "arm"))]
    pub use super::restricted;
}
