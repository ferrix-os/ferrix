//! The Ferrix kernel's end of a network driver's ring (`ferrix-netring`):
//! the core that turns a driver's frames into an interface of the load's
//! network stack.
//!
//! An optional part (`docs/microkernel/CRATE-SPLIT.md` §2.1, S4). Its files
//! keep the module path they had in `src/kernel/src` (`interfaces::net_ring`),
//! and the load's tree, which re-exports the item's, is re-exported beneath
//! it, so every `crate::` path in them names what it named before: `crate::net`
//! and `crate::interfaces::block_ring` the load's, `crate::object` the
//! core's. The binary names it (its composition root's `install`, and stage
//! 9's check); linuxd builds the same root through the twin in `lift/`.
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

extern crate alloc;

/// The build flags this crate was compiled with (`build_flag_word!`, ledger
/// line 3282, (K4)).
#[allow(
    unused_qualifications,
    reason = "named by its crate's path, so the interface's list sees the core's fn used here"
)]
pub const NET_RING_FLAG_WORD: u64 =
    ferrix_kernel_core::flag_word(&ferrix_kernel_core::build_flag_word!());

// Held to the load's word, which main.rs holds to the binary's: the assert
// lives here, not in main.rs, so that a part adds no line to a
// `personality=user` ring 0 (test-ring0-budget, TINY-KERNEL.md). Not in
// linuxd's build, as logctl's.
#[cfg(not(ferrix_load_built = "lift"))]
const _: () = assert!(
    NET_RING_FLAG_WORD == LOAD_FLAG_WORD,
    "ferrix-kernel-net-ring was compiled with other build flags than the load"
);

pub use ferrix_kernel_load::*;

/// The kernel's interfaces to ring-3 drivers; this part holds the network
/// ring's, beside the load's own (re-exported, so `crate::interfaces::
/// block_ring` resolves). Built where the load's `interfaces` is: in ring 0
/// and in linuxd, not in a `personality=user` kernel
/// (`ferrix_load_built = "none"`).
#[cfg(any(ferrix_load_built = "ring0", ferrix_load_built = "lift"))]
pub mod interfaces {
    pub use ferrix_kernel_load::interfaces::*;

    // Built for linuxd by the twin, where the ring-0 check is cfg'd out: the
    // two helpers only that check calls (`forget_device`,
    // `wait_until_task_stopped`) are then dead. The allow sits on the module,
    // not on the two items, so that the moved files stay R100, their lines
    // where they were (CRATE-SPLIT.md §3.1). In no kernel image.
    #[cfg_attr(
        ferrix_load_built = "lift",
        allow(
            dead_code,
            reason = "forget_device and wait_until_task_stopped serve only the ring-0 check, which linuxd's build leaves out"
        )
    )]
    pub mod net_ring;
}
