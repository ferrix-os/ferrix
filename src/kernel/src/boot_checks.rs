//! Whether this boot runs the stages' self-checks: the core's own copy of
//! the answer `checks::init` reads from `ferrix.checks`.
//!
//! The item's `checks` module parses the option and says what it asked for;
//! the flag itself lives here, in the core, so that a check the core runs
//! inside a bring-up -- the x86-64 local APIC's mode on every processor
//! (its `apic/check.rs`), the VT-d units firmware left on and the console
//! line's masked byte (`iommu::check`) -- can ask without naming a
//! module above the core (`docs/KERNEL-CRATES.md` §3 (3)). The item writes it
//! once, downward, as it does `sched::work::set_auditing`.

use core::sync::atomic::{AtomicBool, Ordering};

/// Whether the checks are skipped. Only [`skip`] writes it, once, before the
/// first check that could be.
static SKIPPED: AtomicBool = AtomicBool::new(false);

/// Record that this boot skips the self-checks: `checks::init`, once, when
/// it read `ferrix.checks=skip`.
pub fn skip() {
    SKIPPED.store(true, Ordering::Relaxed);
}

/// Whether the self-checks run on this boot: true unless [`skip`] was called.
pub fn run() -> bool {
    !SKIPPED.load(Ordering::Relaxed)
}
