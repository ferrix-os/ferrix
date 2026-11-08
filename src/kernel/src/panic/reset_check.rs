//! A fault forced inside the first report, before its reset line, under the
//! check-only option `ferrix.check.panic=early` (`L.panic.4`,
//! `cargo xtask test-postmortem`'s early-fault case); and under
//! `ferrix.check.panic=held` the console port's lock taken for good by the
//! reporting processor (`L.console.15`, the held-lock case).
//!
//! The report's own processor faults -- here a second panic -- after its
//! `stopped` line and before the trace, the explanation and the reset line.
//! Under `ferrix.panic=reset` the abridged report that follows must still say
//! the reset line, write the record back and reset; before 2026-11-08 it
//! halted. Nothing here runs without the option.

use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::BootView;

use crate::console::println;

/// The option, and the one value that arms it.
const OPTION: &str = "ferrix.check.panic";
const EARLY: &str = "early";
const HELD: &str = "held";

/// Whether the option asked for the fault, until it is taken.
static ASKED: AtomicBool = AtomicBool::new(false);
/// Whether the option asked for the held lock, until it is taken.
static HOLD: AtomicBool = AtomicBool::new(false);

/// Read the option: once, beside `ferrix.panic`'s.
pub fn read_option(view: &BootView<'_>) {
    match view.option(OPTION) {
        Some(EARLY) => {
            ASKED.store(true, Ordering::Release);
            println!("  check    {OPTION}={EARLY}: the first report faults before its reset line");
        }
        Some(HELD) => {
            HOLD.store(true, Ordering::Release);
            println!(
                "  check    {OPTION}={HELD}: the first report holds the console's lock for good"
            );
        }
        _ => {}
    }
}

/// Take the console port's lock for good, once, if asked: from the first
/// report as it begins, so every later line of it meets a held lock.
pub(crate) fn hold_if_asked() {
    if HOLD.swap(false, Ordering::AcqRel) {
        println!("  check    {OPTION}={HELD}: the console's lock is held from here");
        let _ = crate::console::log_check::hold_port_for_check();
    }
}

/// Fault now, once, if asked: from the first report, on its own processor.
#[expect(
    clippy::panic,
    reason = "AUDIT: the check's fault is a second panic inside the first report, on purpose (check-only, ferrix.check.panic=early)"
)]
pub(crate) fn fault_if_asked() {
    if ASKED.swap(false, Ordering::AcqRel) {
        println!("  check    {OPTION}={EARLY}: faulting inside the report now");
        panic!("{OPTION}={EARLY}: a fault forced inside the first report");
    }
}
