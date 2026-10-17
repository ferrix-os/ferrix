//! `ferrix.panic=reset`: the machine resets itself 10 seconds after a
//! panic's report, instead of halting.
//!
//! **A stop-gap** (2026-10-17, for the customer's N150 box, which has no
//! serial port and nobody at it): a halted box is recovered by a power cycle,
//! which loses the kernel log the panic left in RAM (`console::pstore`). A
//! warm reset keeps it, and the next boot's loader reports it. The persistent
//! event log of `docs/BACKLOG.md`'s row for it replaces this; until then it
//! is the smallest form of AB-BOOT §6.4's design (ledger line 793): off
//! unless asked for, read once after stage 1's check, the first report alone
//! resets, after its last line is on the port and the pstore written back.
//!
//! What runs here runs in a panic: the counter and `arch::reset`, no lock, no
//! allocation, no device but the counter and the reset's own ports.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;

use crate::arch;
use crate::console::{self, println};

/// The command-line option, and the one value that arms the reset.
pub(crate) const OPTION: &str = "ferrix.panic";
const RESET: &str = "reset";
/// The value said for the default.
const HALT: &str = "halt";

/// How long the report stays on the screen before the reset: long enough
/// to read the code or photograph the QR code. One fixed value.
pub(crate) const WAIT_SECONDS: u64 = 10;

/// Whether `ferrix.panic=reset` was asked for.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Whether the first report has announced its reset ([`announce`]).
static ANNOUNCED: AtomicBool = AtomicBool::new(false);

/// The per-CPU register of the processor whose first report announced the
/// reset: an abridged report on that processor -- a fault while it drew the
/// screen -- goes on to the reset instead of halting.
static RESETTING_ON: AtomicU64 = AtomicU64::new(0);

/// Read `ferrix.panic` from the loader's command line: once, after stage 1's
/// check, on the boot processor. Exactly `reset` arms it; `halt` or no
/// option is the default; anything else is said and taken as `halt`, so a
/// misspelt option never turns into a reset nobody asked for.
///
/// Verifies nothing by itself; `cargo xtask test-postmortem`'s reset case
/// and its default case judge it.
pub(crate) fn read_option(view: &BootView<'_>) {
    match view.option(OPTION) {
        None => {}
        Some(RESET) => {
            ARMED.store(true, Ordering::Release);
            println!(
                "  panic    {OPTION}={RESET}: a panic resets the machine {WAIT_SECONDS} s after its report (a stop-gap)"
            );
        }
        Some(HALT) => println!("  panic    {OPTION}={HALT}: a panic halts"),
        Some(other) => {
            println!("  panic    {OPTION}={other} is not understood; a panic halts");
        }
    }
}

/// Whether a panic will reset the machine.
pub(crate) fn armed() -> bool {
    ARMED.load(Ordering::Acquire)
}

/// Say the reset is coming, and remember which processor will make it. In the
/// first report, after its last logged line and before the pstore's
/// write-back, so the line is on the port, in the record and on the screen.
pub(crate) fn announce() {
    println!("  reset     in {WAIT_SECONDS} s, as {OPTION}={RESET} asks");
    console::drain();
    RESETTING_ON.store(arch::cpu_local_register(), Ordering::Relaxed);
    ANNOUNCED.store(true, Ordering::Release);
}

/// Whether this processor is the one whose report announced the reset.
pub(crate) fn resetting_here() -> bool {
    armed()
        && ANNOUNCED.load(Ordering::Acquire)
        && RESETTING_ON.load(Ordering::Relaxed) == arch::cpu_local_register()
}

/// Wait [`WAIT_SECONDS`] on the free-running counter, then reset the
/// machine. With the counter's rate not yet known (x86-64 before stage 3),
/// no wait.
pub(crate) fn wait_then_reset() -> ! {
    let hz = arch::counter_hz();
    if hz != 0 {
        let start = arch::counter_now();
        let ticks = hz.saturating_mul(WAIT_SECONDS);
        while arch::counter_now().wrapping_sub(start) < ticks {
            core::hint::spin_loop();
        }
    }
    arch::reset()
}
