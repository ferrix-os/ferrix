//! The Ferrix kernel's raised checks: core and item check files that need
//! the load to run what they check (`raised_checks` in
//! `tools/common/data/certification-item.json`, `docs/KERNEL-CRATES.md` §3
//! (3)).
//!
//! A crate of their own (`docs/microkernel/CRATE-SPLIT.md` §2.6 item 5, S5),
//! so that the load's root can leave `src/kernel/src` without taking them:
//! each file stays where it is and in its ring by path, and is declared here
//! at the module path the load declared it at. The load's tree, which
//! re-exports the item's and the core's, is re-exported beneath it, so every
//! `crate::` path in them names what it named before. Only the binary names
//! this crate (`check-crate-layering.sh`); it is verification code, built in
//! ring 0 only (`ferrix_load_built = "ring0"`), never for linuxd.

#![no_std]
// The same allow as the load's, for this crate: a kernel without the Linux
// ABI is build-only and carries no claim (LINUX-ABI-SEPARATION.md §11.11,
// ledger 3257 (3)), and builds none of this crate's modules. Never in a build
// with `linux_abi=on`.
#![cfg_attr(
    ferrix_load_built = "none",
    allow(
        unused_crate_dependencies,
        reason = "linux_abi=off is build-only until the checks that run a Linux program are re-homed"
    )
)]

extern crate alloc;
// The item by its crate's name is x86-64's alone (`arch/stage_check.rs`, its
// framebuffer check); named here so that no architecture's build finds the
// dependency unused, with no `target_arch` outside `arch/`.
use ferrix_kernel_item as _;

/// The build flags this crate was compiled with (`build_flag_word!`, ledger
/// line 3282, (K4)).
#[allow(
    unused_qualifications,
    reason = "named by its crate's path, so the interface's list sees the core's fn used here"
)]
pub const RAISED_FLAG_WORD: u64 =
    ferrix_kernel_core::flag_word(&ferrix_kernel_core::build_flag_word!());

// Held to the load's word, which main.rs holds to the binary's, as a part's
// is (net-ring's `lib.rs`): no line added to main.rs, so none to a
// `personality=user` ring 0 (test-ring0-budget).
const _: () = assert!(
    RAISED_FLAG_WORD == LOAD_FLAG_WORD,
    "ferrix-kernel-raised-checks was compiled with other build flags than the load"
);

pub use ferrix_kernel_load::*;

#[cfg(ferrix_load_built = "ring0")]
pub mod service_check;

/// The architecture (the item's), with the checks raised to this crate.
pub mod arch {
    pub use ferrix_kernel_load::arch::*;
    // The stage checks every architecture answers, by one name (`stage_check.rs`).
    #[cfg(ferrix_load_built = "ring0")]
    pub mod stage_check;
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
    pub use ferrix_kernel_load::device::*;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod config_check;
}
/// The IOMMU (the core's), with stage 10's domain check raised to this crate.
pub mod iommu {
    pub use ferrix_kernel_load::iommu::*;
    // Stage 10's check, raised above the core with the kernel split
    // (`docs/KERNEL-CRATES.md`): `check_iommu`, which main.rs runs.
    #[cfg(ferrix_load_built = "ring0")]
    pub mod blocks_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod domain_check;
}
/// Kernel objects (the core's), with the object checks raised to this crate.
pub mod object {
    pub use ferrix_kernel_load::object::*;
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
        pub use ferrix_kernel_load::object::pin::*;
        #[cfg(ferrix_load_built = "ring0")]
        pub mod check;
    }
}
/// The system call layer (the load's), with the item's checks raised to this crate.
pub mod syscall {
    pub use ferrix_kernel_load::syscall::*;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod build_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod native_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod program_check;
    #[cfg(ferrix_load_built = "ring0")]
    pub mod unmap_check;
}
/// User address spaces (the core's), with the checks raised to this crate.
pub mod user {
    pub use ferrix_kernel_load::user::*;
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
