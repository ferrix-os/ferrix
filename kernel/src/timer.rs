//! Time: a counter that always runs, and an interrupt that arrives later.
//!
//! Two different things, deliberately kept apart. The *counter* answers "how
//! long since boot" and is read, never waited on — the HPET's main counter on
//! x86-64, `CNTVCT_EL0` on AArch64. The *timer* is an interrupt scheduled for
//! a future instant: the local APIC timer, or `CNTV_CVAL_EL0`. A kernel that
//! conflates them ends up measuring elapsed time by counting its own ticks,
//! which measures the interrupt rate rather than the passage of time and
//! cannot notice when the two disagree.
//!
//! Stage 3's exit criterion depends on that separation: it counts ticks with
//! one and measures how long they took with the other.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;
use crate::irq::{self, IrqError};

/// Nanoseconds in a second, as the unit conversions want it.
const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Timer interrupts taken since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// The period to re-arm with on each tick, or zero for a one-shot.
static INTERVAL: AtomicU64 = AtomicU64::new(0);

/// Register the tick handler on whichever interrupt this machine's timer uses.
///
/// # Errors
///
/// Whatever [`irq::register`] reports, which at this point can only mean two
/// subsystems claimed the same line.
pub(crate) fn init() -> Result<(), IrqError> {
    irq::register(arch::timer_irq(), on_tick)
}

/// What a timer interrupt does.
///
/// Re-arming here rather than in hardware's periodic mode is a deliberate
/// choice: AArch64's generic timer has no periodic mode at all — it compares
/// against an absolute instant — so a kernel that relied on the local APIC's
/// would need two different notions of "periodic". One-shot is the primitive
/// both machines have, and stage 5's tickless scheduler wants exactly that.
fn on_tick(_irq: u32) {
    let _ = TICKS.fetch_add(1, Ordering::Relaxed);
    let interval = INTERVAL.load(Ordering::Relaxed);
    if interval != 0 {
        arch::timer_arm(interval);
    } else {
        // **Not optional, and not symmetry for its own sake.** AArch64's timer
        // interrupt is level triggered: the line stays asserted for as long as
        // the comparator is in the past. A one-shot that returned without
        // disarming would be acknowledged, re-asserted before the handler had
        // returned, and the machine would take that interrupt forever.
        arch::timer_disarm();
    }
}

/// Fire the timer interrupt once, `nanos` from now.
pub(crate) fn after(nanos: u64) {
    INTERVAL.store(0, Ordering::Relaxed);
    arch::timer_arm(nanos);
}

/// Fire the timer interrupt every `nanos` until [`stop`].
pub(crate) fn every(nanos: u64) {
    INTERVAL.store(nanos, Ordering::Relaxed);
    arch::timer_arm(nanos);
}

/// Stop the timer. The counter keeps running; it always does.
pub(crate) fn stop() {
    INTERVAL.store(0, Ordering::Relaxed);
    arch::timer_disarm();
}

/// Timer interrupts taken since boot.
pub(crate) fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Nanoseconds since the counter started, which is some point inside firmware.
///
/// Only differences between two of these mean anything. The arithmetic is done
/// in 128 bits because the obvious 64-bit form overflows after about eighteen
/// seconds at a 1 `GHz` counter, which is exactly long enough to pass every
/// test and fail on a real machine.
pub(crate) fn now_nanos() -> u64 {
    let hz = arch::counter_hz();
    if hz == 0 {
        return 0;
    }
    let scaled = u128::from(arch::counter_now()) * NANOS_PER_SECOND / u128::from(hz);
    // Saturating rather than truncating: a wrong answer that is obviously
    // wrong beats one that looks plausible.
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// How fast the free-running counter counts.
pub(crate) fn counter_hz() -> u64 {
    arch::counter_hz()
}
