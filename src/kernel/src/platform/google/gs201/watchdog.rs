//! The Pixel 7's system on chip, Tensor G2 (`gs201`): its two watchdogs.
//!
//! ABL hands over with both running -- `watchdog_cl0@10060000` and
//! `watchdog_cl1@10070000`, Samsung's `s3c2410` design -- and before this
//! module nothing serviced them, so every boot on the phone ended in a
//! watchdog reset about half a minute in, whatever the kernel was doing.
//!
//! They are not simply stopped, because they are also this phone's only
//! record of a run. The console is a `ramoops` record in RAM, and a watchdog
//! reset is the one reset measured to keep it (`src/boot/vendor/google/pixel7/README.md`).
//! So the kernel keeps them fed while it is alive -- a task writes each one's
//! reload value back into its counter every [`FEED_INTERVAL`] -- and a kernel
//! that hangs, or stops scheduling that task, is still reset with its log
//! intact. And where the kernel means to stop the machine, [`reset_now`]
//! makes a watchdog fire at once: powering the phone off would lose the log,
//! and PSCI's reset is unproven here.
//!
//! Nothing happens on a machine whose tree has neither watchdog.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use ferrix_fdt::Fdt;

use crate::console::println;
use crate::mmio::Mmio;
use crate::sched;

/// The two watchdogs' bindings, in the order the tree lists them.
const COMPATIBLES: [&str; 2] = ["google,gs201-cl0-wdt", "google,gs201-cl1-wdt"];

/// Control: bit 5 runs the counter, bit 0 lets it reset the machine.
const WTCON: u64 = 0x0;
/// Reload value, which the counter restarts from.
const WTDAT: u64 = 0x4;
/// The counter, counting down; the watchdog fires at zero.
const WTCNT: u64 = 0x8;
/// Bytes of each register window.
const WINDOW: u64 = 0x100;

/// `WTCON`: the counter runs, and reaching zero resets the machine.
const WTCON_ENABLE_AND_RESET: u32 = (1 << 5) | (1 << 0);

/// How often the task feeds them. Their clocks are not modelled here, so this
/// is set against the shortest period seen at hand-off, `cl1`'s counter at
/// `0x8000`, which is seconds, not tens of them.
const FEED_INTERVAL: u64 = 500_000_000;

/// Each watchdog's mapped registers, or zero where there is none.
static WINDOWS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// What each is fed with: its reload value as ABL left it.
static RELOADS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// Find and map the watchdogs, feed them once, and say so.
///
/// Before the scheduler exists, so the feeding task comes later, from
/// [`start`]; what runs between the two is a second or two of boot.
pub(crate) fn init(tree: &Fdt<'_>) {
    let mut found = 0;
    for ((compatible, window), slot) in COMPATIBLES.iter().zip(&WINDOWS).zip(&RELOADS) {
        let Some(region) = tree
            .compatible_nodes(compatible)
            .next()
            .and_then(|node| node.reg().next())
        else {
            continue;
        };
        let Ok(base) = crate::vmap::map_device(region.address, WINDOW) else {
            println!("  watchdog {compatible} could not be mapped, and will reset the phone");
            continue;
        };
        let registers = Mmio::at(base);
        // `WTDAT` is what `WTCNT` restarts from. Should it read as zero,
        // feeding it would fire the watchdog, so the counter's value at this
        // moment -- which it has not yet reached zero from -- stands in.
        let reload = match registers.read32(WTDAT) & 0xFFFF {
            0 => registers.read32(WTCNT) & 0xFFFF,
            value => value,
        };
        slot.store(reload.max(1), Ordering::Relaxed);
        window.store(base, Ordering::Relaxed);
        found += 1;
    }
    if found == 0 {
        return;
    }
    feed();
    println!(
        "  watchdog {found} fed every {} ms while the kernel runs; a hang resets the phone with its log",
        FEED_INTERVAL / 1_000_000
    );
}

/// Start the task that feeds them, once the scheduler can run one.
pub(crate) fn start() {
    if WINDOWS
        .iter()
        .all(|window| window.load(Ordering::Relaxed) == 0)
    {
        return;
    }
    if sched::spawn("watchdog", feed_forever, 0, ferrix_sched_fair::NICE_0_WEIGHT).is_err() {
        println!("  watchdog no task to feed them: the phone will reset within seconds");
    }
}

/// Restart every counter from its reload value.
fn feed() {
    for (window, reload) in WINDOWS.iter().zip(&RELOADS) {
        let base = window.load(Ordering::Relaxed);
        if base != 0 {
            Mmio::at(base).write32(WTCNT, reload.load(Ordering::Relaxed));
        }
    }
}

/// The task: feed, sleep, forever.
fn feed_forever(_: usize) {
    loop {
        feed();
        sched::sleep_for(FEED_INTERVAL);
    }
}

/// Reset the machine now through a watchdog, if this is a machine that has
/// one; return if not.
///
/// The counter is set one tick from zero with reset enabled, so the reset is
/// the same one a hang would get -- the one the `ramoops` record survives --
/// without the wait. Nothing feeds it again: the caller is on its way to a
/// halt with interrupts masked.
pub(crate) fn reset_now() {
    let Some(base) = WINDOWS
        .iter()
        .map(|window| window.load(Ordering::Relaxed))
        .find(|base| *base != 0)
    else {
        return;
    };
    let registers = Mmio::at(base);
    registers.write32(WTDAT, 1);
    registers.write32(WTCNT, 1);
    let control = registers.read32(WTCON);
    registers.write32(WTCON, control | WTCON_ENABLE_AND_RESET);
    // One tick is far under a second at any rate the watchdog runs at, so
    // waiting here is waiting for the reset. Should it never come, the other
    // watchdog, no longer fed, fires in its own time.
    loop {
        core::hint::spin_loop();
    }
}
