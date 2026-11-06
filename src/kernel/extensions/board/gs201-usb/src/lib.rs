//! The Ferrix kernel's gs201 USB glue (the Pixel 7's Tensor G2): the load
//! ring's part in the chip's USB controller, found in the device tree at boot
//! and a no-op on a machine without one.
//!
//! An optional part (`docs/microkernel/CRATE-SPLIT.md` §2.1, S2). Its file
//! keeps the module path it had in `src/kernel/src`
//! (`platform::google::gs201::usb`), and the item's tree is re-exported beneath
//! it, so every `crate::` path in it names what it named before. Only the
//! binary names this crate: its composition root calls `install` (§2.3, no
//! seam). Not built into linuxd (§2.6 item 7). The core's
//! `platform::google::gs201::watchdog` is the core's own, `pub(crate)`, and is
//! not seen from here.
#![no_std]

extern crate alloc;

/// The build flags this crate was compiled with (`build_flag_word!`, ledger
/// line 3282, (K4)).
#[allow(
    unused_qualifications,
    reason = "named by its crate's path, so the interface's list sees the core's fn used here"
)]
pub const BOARD_GS201_USB_FLAG_WORD: u64 =
    ferrix_kernel_core::flag_word(&ferrix_kernel_core::build_flag_word!());

// Held to the item's word, which main.rs holds to the binary's: the assert
// lives here, not in main.rs, so that a part adds no line to a
// `personality=user` ring 0 (test-ring0-budget, TINY-KERNEL.md).
const _: () = assert!(
    BOARD_GS201_USB_FLAG_WORD == ITEM_FLAG_WORD,
    "ferrix-kernel-board-gs201-usb was compiled with other build flags than the item"
);

pub use ferrix_kernel_item::*;

/// What belongs to one system on chip rather than to an architecture: the kernel's part in a board's devices.
/// Unlike §2.1's shape, no glob of the item's `platform` or `google`: all
/// they hold below is the core's `pub(crate)` watchdog, so the glob would
/// re-export nothing (rustc's unused import).
pub mod platform {
    /// Google's systems on chip.
    pub mod google {
        /// The Pixel 7's Tensor G2: the load ring's part of it.
        pub mod gs201 {
            pub mod usb;
        }
    }
}
