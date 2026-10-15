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

extern crate alloc;

pub use ferrix_kernel_item::*;

pub mod fs;
pub mod interfaces;
pub mod net;
pub mod service_check;
/// The architecture modules (the core's), with the architecture checks raised to this crate, each declared at its position and compiled on its own architecture only.
pub mod arch {
    pub use ferrix_kernel_core::arch::*;
    pub use stage_check::{drive_native_words, drive_system_call};
    // The stage checks every architecture answers, by one name (`stage_check.rs`).
    pub mod stage_check;
    /// AArch64's raised checks.
    pub mod aarch64 {
        /// The trap entry's check.
        pub mod trap {
            pub mod check;
        }
    }
    /// ARMv7-A's raised checks.
    pub mod armv7a {
        /// The context switch's check.
        pub mod switch {
            pub mod check;
        }
        /// The trap entry's check.
        pub mod trap {
            pub mod check;
        }
    }
    /// x86-64's raised checks.
    pub mod x86_64 {
        /// The paranoid entries' check.
        pub mod paranoid {
            pub mod check;
        }
        /// The context switch's check.
        pub mod switch {
            pub mod check;
        }
        /// The trap entry's check.
        pub mod trap {
            pub mod check;
        }
    }
}
/// Device objects (the core's), with the configuration-space check raised to this crate.
pub mod device {
    pub use ferrix_kernel_core::device::*;
    pub mod config_check;
}
/// The IOMMU (the core's), with stage 10's domain check raised to this crate.
pub mod iommu {
    pub use ferrix_kernel_core::iommu::*;
    // Stage 10's check, raised above the core with the kernel split
    // (`docs/KERNEL-CRATES.md`): `check_iommu`, which main.rs runs.
    pub mod domain_check;
}
/// Kernel objects (the core's), with the object checks raised to this crate.
pub mod object {
    pub use ferrix_kernel_core::object::*;
    pub mod alloc_check;
    pub mod check;
    pub mod device_copy_check;
    pub mod domain_check;
    pub mod edge_check;
    pub mod fast_path_check;
    pub mod format_check;
    pub mod quota_check;
    pub mod write_read_check;
    /// Pinned memory (the core's), with its check raised to this crate.
    pub mod pin {
        pub use ferrix_kernel_core::object::pin::*;
        pub mod check;
    }
}
// What belongs to one system on chip rather than to an architecture: the
// kernel's part in a board's devices, each found in the device tree at boot
// and a no-op on a machine without one, grouped by the vendor prefix of the
// chip's `compatible`. The gs201 is the Pixel 7's Tensor G2; its watchdogs
// are serviced from `arch::aarch64` alone.
/// What belongs to one system on chip rather than to an architecture: the kernel's part in a board's devices.
pub mod platform {
    pub use ferrix_kernel_core::platform::*;
    /// Google's systems on chip.
    pub mod google {
        pub use ferrix_kernel_core::platform::google::*;
        /// The Pixel 7's Tensor G2: the load's part of it.
        pub mod gs201 {
            pub use ferrix_kernel_core::platform::google::gs201::*;
            pub mod usb;
        }
    }
    /// STMicroelectronics' systems on chip.
    pub mod st {
        pub mod stm32mp1;
    }
}
/// The system call layer: the item's `syscall/mod.rs`, with the Linux personality's calls and the checks raised to this crate.
pub mod syscall {
    pub use ferrix_kernel_item::syscall::*;
    pub mod attributes;
    pub mod check;
    pub mod compat;
    pub mod credentials;
    pub mod deliver;
    pub mod epoll;
    pub mod eventfd;
    pub mod exec;
    pub mod family;
    pub mod fd;
    pub mod file;
    pub mod flock;
    pub mod fsctl;
    pub mod futex;
    pub mod image;
    pub mod init_calls_check;
    pub mod kill;
    pub mod launch;
    pub mod limits;
    pub mod linux;
    pub mod load;
    pub mod memfd;
    pub mod memory;
    pub mod namespace;
    pub mod native_check;
    pub mod nsproxy;
    pub mod path;
    pub mod pidns;
    pub mod pipe;
    pub mod poll;
    pub mod process;
    pub mod program_check;
    pub mod registry;
    pub mod seccomp;
    pub mod seccomp_check;
    pub mod seccomp_filters_check;
    pub mod sem;
    pub mod sem_check;
    pub mod shm;
    pub mod shm_check;
    pub mod signal;
    pub mod signalfd;
    pub mod sigpage;
    pub mod sockets;
    pub mod stat;
    pub mod system;
    pub mod thread;
    pub mod thread_area;
    pub mod time;
    pub mod timerfd;
    pub mod tty;
    pub mod unmap_check;
    pub mod userns;
    pub mod vdso;
    pub mod vdso_check;
}
/// User address spaces (the core's), with two checks raised to this crate.
pub mod user {
    pub use ferrix_kernel_core::user::*;
    pub mod madvise_check;
    pub mod rmap_check;
}
