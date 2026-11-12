//! The Ferrix kernel's load ring: the file systems, the network stack, the
//! Linux personality's system calls and the cores of the ring-3 drivers'
//! protocols -- what the kernel runs and does not vouch for.
//!
//! One of the kernel's four crates (`docs/KERNEL-CRATES.md`): it depends on the
//! core and the item and re-exports their modules, so that `crate::mm` names in
//! a load file what it named before the split. It also holds the check files
//! raised above their ring (`raised_checks` in
//! `tools/common/data/certification-item.json`), each declared here by its
//! position.

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
#[cfg(ferrix_load_built = "ring0")]
pub mod service_check;
#[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
pub mod setters;
/// The architecture modules (the core's), with the architecture checks raised to this crate, each declared at its position and compiled on its own architecture only.
pub mod arch {
    pub use ferrix_kernel_item::arch::*;
    // The stage checks every architecture answers, by one name (`stage_check.rs`).
    #[cfg(ferrix_load_built = "ring0")]
    pub mod stage_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub use stage_check::{drive_native_words, drive_system_call};
    /// AArch64's raised checks.
    pub mod aarch64 {
        /// The trap entry's check.
        pub mod trap {
            #[cfg(ferrix_load_built = "ring0")]
            pub mod check;
        }
    }
    /// ARMv7-A's raised checks.
    pub mod armv7a {
        /// The context switch's check.
        pub mod switch {
            #[cfg(ferrix_load_built = "ring0")]
            pub mod check;
        }
        /// The trap entry's check.
        pub mod trap {
            #[cfg(ferrix_load_built = "ring0")]
            pub mod check;
        }
    }
    /// x86-64's raised checks.
    pub mod x86_64 {
        /// The paranoid entries' check.
        pub mod paranoid {
            #[cfg(ferrix_load_built = "ring0")]
            pub mod check;
        }
        /// The context switch's check.
        pub mod switch {
            #[cfg(ferrix_load_built = "ring0")]
            pub mod check;
        }
        /// The trap entry's check.
        pub mod trap {
            #[cfg(ferrix_load_built = "ring0")]
            pub mod check;
        }
    }
}
/// Device objects (the core's), with the configuration-space check raised to this crate.
pub mod device {
    pub use ferrix_kernel_item::device::*;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod config_check;
}
/// The IOMMU (the core's), with stage 10's domain check raised to this crate.
pub mod iommu {
    pub use ferrix_kernel_core::iommu::*;
    // Stage 10's check, raised above the core with the kernel split
    // (`docs/KERNEL-CRATES.md`): `check_iommu`, which main.rs runs.
    #[cfg(ferrix_load_built = "ring0")]
    pub mod blocks_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod domain_check;
}
/// Kernel objects (the core's), with the object checks raised to this crate.
pub mod object {
    pub use ferrix_kernel_core::object::*;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod alloc_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod authority_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod blocks_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod device_copy_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod domain_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod edge_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod exception_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod fast_path_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod format_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod grant_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod level_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod pager_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod quota_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod sealing_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod sender_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod space_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod ticket_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod timer_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod write_read_check;
    /// Pinned memory (the core's), with its check raised to this crate.
    pub mod pin {
        pub use ferrix_kernel_core::object::pin::*;
        #[cfg(ferrix_load_built = "ring0")]
        pub mod check;
    }
}
/// The system call layer: the item's `syscall/mod.rs`, with the Linux personality's calls and the checks raised to this crate.
pub mod syscall {
    pub use ferrix_kernel_item::syscall::*;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod attributes;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod build_check;
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
    #[cfg(ferrix_load_built = "ring0")]
    pub mod native_check;
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
    #[cfg(ferrix_load_built = "ring0")]
    pub mod program_check;
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
    #[cfg(ferrix_load_built = "ring0")]
    pub mod unmap_check;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod userns;
    #[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
    pub mod vdso;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod vdso_check;
}
/// User address spaces (the core's), with the checks raised to this crate.
pub mod user {
    pub use ferrix_kernel_core::user::*;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod address_wait_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod alloc_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod madvise_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod rmap_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod rss_kept_check;
}
