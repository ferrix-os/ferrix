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

/// The build flags this crate was compiled with, for [`flag_word`] to make one
/// word of: one `bool` for each value of the profile, the level and every
/// flag, in the flag file's order, each the `cfg!` of its value where the
/// macro is expanded.
/// Each of the kernel's crates expands it into a constant of its own, and the
/// binary requires the four equal at compile time (ledger line 3282, (K4)):
/// the build line is computed from the item's `cfg`s, so a crate compiled
/// with others would run code the line does not name. `check-build-flags.py`
/// holds this list to the flag file.
#[macro_export]
macro_rules! build_flag_word {
    () => {
        [
            cfg!(ferrix_profile = "present"),
            cfg!(ferrix_profile = "desktop"),
            cfg!(ferrix_profile = "fast"),
            cfg!(ferrix_profile = "max"),
            cfg!(ferrix_profile = "none"),
            cfg!(ferrix_level = "none"),
            cfg!(ferrix_level = "present"),
            cfg!(ferrix_level = "sc2"),
            cfg!(ferrix_level = "sc4"),
            cfg!(ferrix_personality = "user"),
            cfg!(ferrix_personality = "kernel"),
            cfg!(ferrix_personality = "none"),
            cfg!(ferrix_fs = "server"),
            cfg!(ferrix_fs = "kernel"),
            cfg!(ferrix_fs = "none"),
            cfg!(ferrix_mitigations = "on"),
            cfg!(ferrix_mitigations = "off"),
            cfg!(ferrix_partition_speculation = "exit"),
            cfg!(ferrix_partition_speculation = "partition"),
            cfg!(ferrix_partition_speculation = "not_applicable"),
            cfg!(ferrix_ipc_fastpath = "off"),
            cfg!(ferrix_ipc_fastpath = "on"),
            cfg!(ferrix_restricted_fastpath = "off"),
            cfg!(ferrix_restricted_fastpath = "on"),
            cfg!(ferrix_restricted_fastpath = "not_applicable"),
            cfg!(ferrix_kernel_memory = "retyped"),
            cfg!(ferrix_kernel_memory = "heap"),
            cfg!(ferrix_schedule = "windows"),
            cfg!(ferrix_schedule = "edf"),
            cfg!(ferrix_schedule = "fair"),
            cfg!(ferrix_checks = "on"),
            cfg!(ferrix_checks = "off"),
            cfg!(ferrix_program_barrier = "switch"),
            cfg!(ferrix_program_barrier = "session"),
            cfg!(ferrix_ssbd = "always"),
            cfg!(ferrix_ssbd = "on_request"),
            cfg!(ferrix_smt = "off"),
            cfg!(ferrix_smt = "stibp"),
            cfg!(ferrix_runtime_trades = "none"),
            cfg!(ferrix_runtime_trades = "game_mode"),
            cfg!(ferrix_linux_abi = "on"),
            cfg!(ferrix_linux_abi = "off"),
            cfg!(ferrix_runtime_settings = "fixed"),
            cfg!(ferrix_runtime_settings = "live"),
        ]
    };
}

/// [`build_flag_word!`]'s values as bits, the first lowest.
#[must_use]
pub const fn flag_word(chosen: &[bool]) -> u64 {
    let mut word = 0_u64;
    let mut at = 0;
    let mut rest = chosen;
    while let [set, tail @ ..] = rest {
        if *set && at < 64 {
            word |= 1 << at;
        }
        at += 1;
        rest = tail;
    }
    word
}

/// The build flags the core was compiled with ([`build_flag_word!`]).
pub const CORE_FLAG_WORD: u64 = flag_word(&build_flag_word!());

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
