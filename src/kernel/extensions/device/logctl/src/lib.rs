//! The Ferrix kernel's end of the kernel log's control channel
//! (`ferrix-logctl`): the reader a ring-3 log service claims.
//!
//! An optional part (`docs/microkernel/CRATE-SPLIT.md` §2.1, S3). Its files
//! keep the module path they had in `src/kernel/src` (`interfaces::logctl`),
//! and the item's tree is re-exported beneath it, so every `crate::` path in
//! them names what it named before. The binary names it (its composition
//! root's `install`, and stage 9's check); linuxd builds the same root
//! through the twin in `lift/`.
#![no_std]
// The same allow as the load's, for this crate: a kernel without the Linux
// ABI is build-only and carries no claim (LINUX-ABI-SEPARATION.md §11.11),
// and a personality=user kernel builds none of this part's modules. Never in
// a build with `linux_abi=on`.
#![cfg_attr(
    ferrix_load_built = "none",
    allow(
        unused_crate_dependencies,
        reason = "linux_abi=off is build-only until the checks that run a Linux program are re-homed"
    )
)]
// Built for linuxd by the twin, where the ring-0 check is cfg'd out: the
// one item only that check uses (`claimed`) is then dead, as it was in
// ferrix-lift-load, whose root allows it the same way. In no kernel image.
#![cfg_attr(
    ferrix_load_built = "lift",
    allow(
        dead_code,
        reason = "claimed() serves only the ring-0 check, which linuxd's build leaves out"
    )
)]

extern crate alloc;

/// The build flags this crate was compiled with (`build_flag_word!`, ledger
/// line 3282, (K4)).
#[allow(
    unused_qualifications,
    reason = "named by its crate's path, so the interface's list sees the core's fn used here"
)]
pub const LOGCTL_FLAG_WORD: u64 =
    ferrix_kernel_core::flag_word(&ferrix_kernel_core::build_flag_word!());

// Held to the item's word, which main.rs holds to the binary's: the assert
// lives here, not in main.rs, so that a part adds no line to a
// `personality=user` ring 0 (test-ring0-budget, TINY-KERNEL.md). Not in
// linuxd's build, whose shim of the item has no flag word of its own.
#[cfg(not(ferrix_load_built = "lift"))]
const _: () = assert!(
    LOGCTL_FLAG_WORD == ITEM_FLAG_WORD,
    "ferrix-kernel-logctl was compiled with other build flags than the item"
);

pub use ferrix_kernel_item::*;

/// The kernel's interfaces to ring-3 drivers; this part holds the log's.
/// Built where the load's `interfaces` is: in ring 0 and in linuxd, not in a
/// `personality=user` kernel (`ferrix_load_built = "none"`).
#[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
pub mod interfaces {
    pub mod logctl;
}
