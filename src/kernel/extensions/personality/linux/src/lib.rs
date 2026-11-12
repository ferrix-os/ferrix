//! The Ferrix kernel's load ring: the file systems, the network stack, the
//! Linux personality's system calls and the cores of the ring-3 drivers'
//! protocols -- what the kernel runs and does not vouch for.
//!
//! One of the kernel's crates (`docs/KERNEL-CRATES.md`): it depends on the
//! core and the item and re-exports their modules, so that `crate::mm` names in
//! a load file what it named before the split. The check files raised above
//! their ring (`raised_checks` in `tools/common/data/certification-item.json`)
//! are not its: since S5 of the crate split they are
//! `ferrix-kernel-raised-checks`'s (`kernel_raised.rs`), which only the binary
//! names.

#![no_std]
// The same allow as `main.rs`'s, for this crate: a kernel without the Linux
// ABI is build-only and carries no claim (LINUX-ABI-SEPARATION.md §11.11,
// ledger 3257 (3)), and each of the kernel's crates is compiled with its
// flags. Never in a build with `linux_abi=on`.
#![cfg_attr(
    ferrix_load_built = "none",
    allow(
        dead_code,
        unused_imports,
        unused_variables,
        unfulfilled_lint_expectations,
        unused_crate_dependencies,
        reason = "linux_abi=off is build-only until the checks that run a Linux program are re-homed"
    )
)]
// The shared root built for linuxd (docs/microkernel/M5-LIFT.md §1): the same
// allow, until the shim's stubs are real code. In no kernel image.
#![cfg_attr(
    ferrix_load_built = "lift",
    allow(
        dead_code,
        unused_imports,
        unused_variables,
        unfulfilled_lint_expectations,
        unused_crate_dependencies,
        reason = "the shim's stubs leave load code unused until each milestone reaches it"
    )
)]

extern crate alloc;
// The load by its own name, so a seam's signatures (`seam!`) resolve both
// here and in the binary's root where its binding macro expands.
extern crate self as ferrix_kernel_load;

/// One composition-root seam from one list (`seam!`, KERNEL-CRATES §6.3).
mod seam;

/// The build flags this crate was compiled with (`build_flag_word!`, ledger
/// line 3282, (K4)).
#[allow(
    unused_qualifications,
    reason = "named by its crate's path, so the interface's list sees the core's fn used here"
)]
pub const LOAD_FLAG_WORD: u64 =
    ferrix_kernel_core::flag_word(&ferrix_kernel_core::build_flag_word!());

pub use ferrix_kernel_item::*;

#[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
pub mod fs;
#[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
pub mod interfaces;
#[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
pub mod net;
#[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
pub mod setters;
/// The architecture modules (the core's), with the architecture checks raised to this crate, each declared at its position and compiled on its own architecture only.
/// The system call layer: the item's `syscall/mod.rs`, with the Linux personality's calls and their checks.
pub mod syscall {
    pub use ferrix_kernel_item::syscall::*;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod attributes;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod check;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod compat;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod credentials;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod deliver;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod epoll;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod eventfd;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod exec;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod family;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod fd;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod file;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod flock;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod fsctl;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod futex;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod image;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod init_calls_check;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod kill;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod launch;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod limits;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod linux;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod load;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod memfd;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod memory;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod namespace;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod nsproxy;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod path;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod pidns;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod pipe;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod poll;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod process;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod registry;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod seccomp;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod seccomp_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod seccomp_filters_check;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod sem;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod sem_check;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod shm;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod shm_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod side_check;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod signal;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod signalfd;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod sigpage;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod sockets;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod stat;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod system;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod thread;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod thread_area;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod time;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod timens;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod timerfd;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod tty;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod userns;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod vdso;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod vdso_check;
}
