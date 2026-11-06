//! The Ferrix kernel's STM32MP1 board glue (the DK1): the load ring's part
//! in the chip's devices, found in the device tree at boot and a no-op on a
//! machine without one.
//!
//! An optional part (`docs/microkernel/CRATE-SPLIT.md` §2.1, S2). Its files
//! keep the module path they had in `src/kernel/src` (`platform::st::stm32mp1`),
//! and the item's tree is re-exported beneath it, so every `crate::` path in
//! them names what it named before. Only the binary names this crate: its
//! composition root calls `install` (§2.3, no seam). Not built into linuxd
//! (§2.6 item 7).
#![no_std]

extern crate alloc;

/// The build flags this crate was compiled with (`build_flag_word!`, ledger
/// line 3282, (K4)).
#[allow(
    unused_qualifications,
    reason = "named by its crate's path, so the interface's list sees the core's fn used here"
)]
pub const BOARD_STM32MP1_FLAG_WORD: u64 =
    ferrix_kernel_core::flag_word(&ferrix_kernel_core::build_flag_word!());

// Held to the item's word, which main.rs holds to the binary's: the assert
// lives here, not in main.rs, so that a part adds no line to a
// `personality=user` ring 0 (test-ring0-budget, TINY-KERNEL.md).
const _: () = assert!(
    BOARD_STM32MP1_FLAG_WORD == ITEM_FLAG_WORD,
    "ferrix-kernel-board-stm32mp1 was compiled with other build flags than the item"
);

pub use ferrix_kernel_item::*;

/// What belongs to one system on chip rather than to an architecture: the kernel's part in a board's devices.
/// Unlike §2.1's shape, no glob of the item's `platform`: nothing here names
/// the core's (its gs201 watchdog), and the glob would add a use of the
/// core's `platform` to its interface (docs/generated/KERNEL-INTERFACE.md).
pub mod platform {
    /// ST's systems on chip, by their vendor prefix `st`.
    pub mod st {
        pub mod stm32mp1;
    }
}
