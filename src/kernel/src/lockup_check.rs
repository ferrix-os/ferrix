//! `ferrix.check.lockup`: a processor masked on purpose, for the gates that
//! show the lockup detector and the watchdog catch it (`docs/POSTMORTEM.md`
//! §6.5). Verification code, never in an integrator's command line.
//!
//! `irqoff`: once every processor's heartbeat has moved, the boot processor
//! says so in one line, masks its interrupts and spins for good. Its watcher
//! must find it within the detector's bound.

use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::BootView;

use crate::console::{self, println};

/// The option, and the one value it takes.
const OPTION: &str = "ferrix.check.lockup";
const IRQOFF: &str = "irqoff";

/// How long to wait for every processor's first watch tick before masking.
const SETTLE_NANOS: u64 = 3_000_000_000;

/// Whether `ferrix.check.lockup=irqoff` was asked for.
static ASKED: AtomicBool = AtomicBool::new(false);

/// Read the option: once, at boot, beside `lockup::read_option`.
pub(crate) fn read_option(view: &BootView<'_>) {
    match view.option(OPTION) {
        None => {}
        Some(IRQOFF) => ASKED.store(true, Ordering::Release),
        Some(other) => {
            println!("  check    {OPTION}={other} is not understood; nothing will be masked");
        }
    }
}

/// Run the check, if the command line asked for it: never returns when it
/// does.
pub(crate) fn run() {
    if !ASKED.load(Ordering::Acquire) {
        return;
    }
    if !crate::watch::watching() {
        println!(
            "  check    {OPTION}={IRQOFF} needs the watch (ferrix.lockup=on); nothing is masked"
        );
        return;
    }
    settle();
    let me = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    println!(
        "  check    {OPTION}={IRQOFF}: processor {me} masks its interrupts and spins, for the lockup detector to find"
    );
    console::drain();
    crate::arch::disable_interrupts();
    loop {
        core::hint::spin_loop();
    }
}

/// Wait, at most [`SETTLE_NANOS`], until every online processor's heartbeat
/// has moved: the watch has reached them all.
fn settle() {
    let Some(topology) = crate::smp::topology() else {
        return;
    };
    let start = crate::timer::now_nanos();
    loop {
        let all = topology
            .cpus()
            .iter()
            .filter(|cpu| cpu.is_online())
            .all(|cpu| crate::watch::beats(cpu.logical) > 0);
        if all || crate::timer::now_nanos().saturating_sub(start) >= SETTLE_NANOS {
            return;
        }
        core::hint::spin_loop();
    }
}
