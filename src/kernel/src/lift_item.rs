//! linuxd's shim of the kernel's item (docs/microkernel/M5-LIFT.md).

#![no_std]
#![allow(
    unused_qualifications,
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

pub use ferrix_kernel_core::*;

/// The shim's own modules, found by position under `src/kernel/src/lift/item`.
pub mod lift {
    pub mod item {
        pub mod cores;
        pub mod discovery;
        pub mod power;
        pub mod random;
        pub mod syscall;
    }
}
pub use lift::item::{discovery, power, random};
/// The system call layer: the shim's, and the file that is the same source.
pub mod syscall {
    pub use crate::lift::item::syscall::*;
    pub mod program;
}
/// The device cores' end of devmgr's channels (M5-LIFT.md §11).
pub mod cores {
    pub use crate::lift::item::cores::*;
}
pub mod init;
/// The fast path is the kernel's.
pub mod fastpath {
    pub fn report_counts() {}
}
