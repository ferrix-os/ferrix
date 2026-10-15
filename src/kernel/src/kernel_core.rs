//! The Ferrix kernel's core ring: memory, scheduling, traps, objects and the
//! boot -- what every other part of the kernel rests on, and the part the
//! certified item's argument is made about first.
//!
//! One of the kernel's four crates (`docs/KERNEL-CRATES.md`): it depends on no
//! other kernel crate, and its `pub` items are the interface the item, the
//! load and the binary see, listed in `docs/generated/KERNEL-INTERFACE.md`.
//! Every file stays where it was in `src/kernel/src`; this root declares the
//! core's modules by position, beside the binary's `main.rs`.

#![no_std]

extern crate alloc;

pub mod arch;
pub mod audit;
pub mod backtrace;
pub mod boot_checks;
pub mod claim;
pub mod console;
pub mod device;
pub mod early;
pub mod fallible;
pub mod hooks;
pub mod iommu;
pub mod irq;
pub mod mm;
pub mod mmio;
pub mod object;
pub mod panic;
pub mod sched;
pub mod signal_frame;
pub mod smp;
pub mod sync;
pub mod timer;
pub mod trap;
pub mod user;
pub mod vmap;
/// Finding what the machine has: the core's modules of `discovery/` (firmware tables, the device tree and the board they describe), whose `mod.rs` is the item's and re-exports them.
pub mod discovery {
    // ACPI is how the 64-bit pair describe themselves. An ARMv7-A machine has
    // none, and there this module is compiled and never called.
    #[allow(
        dead_code,
        reason = "ARMv7-A describes itself with a device tree, not ACPI"
    )]
    pub(crate) mod acpi;
    pub mod board;
    pub mod description;
    pub mod fdt;
    pub mod finder;
    pub mod tree;
}
// What belongs to one system on chip rather than to an architecture: the
// kernel's part in a board's devices, each found in the device tree at boot
// and a no-op on a machine without one, grouped by the vendor prefix of the
// chip's `compatible`. The gs201 is the Pixel 7's Tensor G2; its watchdogs
// are serviced from `arch::aarch64` alone.
/// What belongs to one system on chip rather than to an architecture: the kernel's part in a board's devices.
pub mod platform {
    /// Google's systems on chip.
    pub mod google {
        /// The Pixel 7's Tensor G2: the core's part of it.
        pub mod gs201 {
            #[allow(dead_code, reason = "only AArch64 has a gs201, the Pixel 7's")]
            pub(crate) mod watchdog;
        }
    }
}
/// The system call layer's one core module, `uaccess`; `syscall/mod.rs` is the item's and re-exports it.
pub mod syscall {
    pub mod uaccess;
}
