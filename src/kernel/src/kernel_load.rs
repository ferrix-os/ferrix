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

#[cfg(ferrix_linux_abi = "on")]
pub mod fs;
#[cfg(ferrix_linux_abi = "on")]
pub mod interfaces;
#[cfg(ferrix_linux_abi = "on")]
pub mod net;
#[cfg(ferrix_linux_abi = "on")]
pub mod service_check;
#[cfg(ferrix_linux_abi = "on")]
pub mod setters;
/// The architecture modules (the core's), with the architecture checks raised to this crate, each declared at its position and compiled on its own architecture only.
pub mod arch {
    pub use ferrix_kernel_item::arch::*;
    // The stage checks every architecture answers, by one name (`stage_check.rs`).
    #[cfg(ferrix_linux_abi = "on")]
    pub mod stage_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub use stage_check::{drive_native_words, drive_system_call};
    /// AArch64's raised checks.
    pub mod aarch64 {
        /// The trap entry's check.
        pub mod trap {
            #[cfg(ferrix_linux_abi = "on")]
            pub mod check;
        }
    }
    /// ARMv7-A's raised checks.
    pub mod armv7a {
        /// The context switch's check.
        pub mod switch {
            #[cfg(ferrix_linux_abi = "on")]
            pub mod check;
        }
        /// The trap entry's check.
        pub mod trap {
            #[cfg(ferrix_linux_abi = "on")]
            pub mod check;
        }
    }
    /// x86-64's raised checks.
    pub mod x86_64 {
        /// The paranoid entries' check.
        pub mod paranoid {
            #[cfg(ferrix_linux_abi = "on")]
            pub mod check;
        }
        /// The context switch's check.
        pub mod switch {
            #[cfg(ferrix_linux_abi = "on")]
            pub mod check;
        }
        /// The trap entry's check.
        pub mod trap {
            #[cfg(ferrix_linux_abi = "on")]
            pub mod check;
        }
    }
}
/// Device objects (the core's), with the configuration-space check raised to this crate.
pub mod device {
    pub use ferrix_kernel_item::device::*;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod config_check;
}
/// The IOMMU (the core's), with stage 10's domain check raised to this crate.
pub mod iommu {
    pub use ferrix_kernel_core::iommu::*;
    // Stage 10's check, raised above the core with the kernel split
    // (`docs/KERNEL-CRATES.md`): `check_iommu`, which main.rs runs.
    #[cfg(ferrix_linux_abi = "on")]
    pub mod domain_check;
}
/// Kernel objects (the core's), with the object checks raised to this crate.
pub mod object {
    pub use ferrix_kernel_core::object::*;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod alloc_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod blocks_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod device_copy_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod domain_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod edge_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod fast_path_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod format_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod quota_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod write_read_check;
    /// Pinned memory (the core's), with its check raised to this crate.
    pub mod pin {
        pub use ferrix_kernel_core::object::pin::*;
        #[cfg(ferrix_linux_abi = "on")]
        pub mod check;
    }
}
/// What belongs to one system on chip rather than to an architecture: the kernel's part in a board's devices.
pub mod platform {
    /// Google's systems on chip.
    pub mod google {
        /// The Pixel 7's Tensor G2: the load's part of it.
        pub mod gs201 {
            pub mod usb;
        }
    }
    /// ST's systems on chip, by their vendor prefix `st`.
    pub mod st {
        pub mod stm32mp1;
    }
}
/// The system call layer: the item's `syscall/mod.rs`, with the Linux personality's calls and the checks raised to this crate.
pub mod syscall {
    pub use ferrix_kernel_item::syscall::*;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod attributes;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod compat;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod credentials;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod deliver;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod epoll;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod eventfd;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod exec;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod family;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod fd;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod file;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod flock;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod fsctl;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod futex;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod image;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod init_calls_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod kill;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod launch;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod limits;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod linux;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod load;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod memfd;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod memory;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod namespace;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod native_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod nsproxy;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod path;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod pidns;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod pipe;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod poll;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod process;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod program_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod registry;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod seccomp;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod seccomp_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod seccomp_filters_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod sem;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod sem_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod shm;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod shm_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod signal;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod signalfd;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod sigpage;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod sockets;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod stat;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod system;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod thread;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod thread_area;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod time;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod timens;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod timerfd;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod tty;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod unmap_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod userns;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod vdso;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod vdso_check;
}
/// User address spaces (the core's), with the checks raised to this crate.
pub mod user {
    pub use ferrix_kernel_core::user::*;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod alloc_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod madvise_check;
    #[cfg(ferrix_linux_abi = "on")]
    pub mod rmap_check;
}
