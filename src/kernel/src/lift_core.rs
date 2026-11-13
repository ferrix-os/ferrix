//! linuxd's shim of the kernel's core (docs/microkernel/M5-LIFT.md): the core's
//! `pub` interface as the load names it, implemented in user space, so that
//! the load's one source builds into ring 0 for `desktop` and into linuxd for
//! `fast` and `max`.

#![no_std]
#![allow(
    unused_qualifications,
    unused_unsafe,
    unused_extern_crates,
    unused_crate_dependencies,
    dead_code,
    unused_imports,
    unused_variables,
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::restriction,
    clippy::nursery,
    reason = "the shim's stubs, until each milestone reaches them"
)]

extern crate alloc;

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
            cfg!(ferrix_fs = "lift"),
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
            cfg!(ferrix_fault_ahead = "off"),
            cfg!(ferrix_fault_ahead = "on"),
            cfg!(ferrix_fault_ahead = "not_applicable"),
            cfg!(ferrix_restricted_window = "off"),
            cfg!(ferrix_restricted_window = "on"),
            cfg!(ferrix_restricted_window = "not_applicable"),
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
            cfg!(ferrix_board_glue = "on"),
            cfg!(ferrix_board_glue = "off"),
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

/// The shim's own modules, found by position under `src/kernel/src/lift/core`.
pub mod lift {
    pub mod core {
        pub mod arch;
        pub mod audit;
        pub mod console;
        pub mod device;
        pub mod discovery;

        pub mod iommu;
        pub mod irq;
        pub mod mm;
        pub mod mmio;
        pub mod panic;
        pub mod sched;

        pub mod bridge;
        pub mod calls_check;
        pub mod fill;
        pub mod heap;
        pub mod restricted;
        pub mod smp;
        pub mod sync;
        pub mod sys;
        pub mod timer;
        pub mod trap;
        pub mod vmap;
        pub mod watchdog;
    }
}
pub use lift::core::{
    audit, bridge, device, discovery, fill, heap, iommu, irq, mm, mmio, restricted, sched, smp,
    sync, sys, timer, trap, vmap, watchdog,
};
pub mod claim;
pub mod fallible;
pub mod hooks;
pub mod signal_frame;
/// The architecture modules: the shim's, and the files that are the same source.
pub mod arch {
    pub use crate::lift::core::arch::*;
    #[cfg(target_arch = "x86_64")]
    pub use x86_64::vdso::{vdso_can_read_counter, vdso_spec};
    /// x86-64.
    #[cfg(target_arch = "x86_64")]
    pub mod x86_64 {
        pub use crate::lift::core::arch::x86_64::*;
        pub mod signal;
        /// The vDSO's code is a program's, never run here: the image linuxd maps
        /// (lift_reuse's one file holding `global_asm!`).
        pub mod vdso;
    }
    /// AArch64.
    #[cfg(target_arch = "aarch64")]
    pub mod aarch64 {
        pub use crate::lift::core::arch::aarch64::*;
        pub mod signal;
    }
}
/// The console: the shim's, and the log ring, which is the same source.
pub mod console {
    pub use crate::lift::core::console::*;
    pub mod log;
}
/// The panic catalog is the same source.
pub mod panic {
    pub use crate::lift::core::panic::*;
    pub mod catalog;
}
/// The user-memory copies are the same source.
pub mod syscall {
    pub mod uaccess;
}
pub mod object;
pub mod user;
