//! A grace period: waiting until nobody can still be looking.
//!
//! The problem this solves appears the moment there are two CPUs and one
//! read-mostly structure. The interrupt handler table is the first: `dispatch`
//! reads it from an interrupt with no lock, because taking one on every
//! interrupt on every core would serialise the machine's entire interrupt load
//! on one cache line. So a writer cannot free what it replaced when it replaces
//! it — another CPU may be between the load and the call.
//!
//! What makes that tractable is a property of *where* the readers are rather
//! than of what they are doing: every reader of such a structure runs with
//! preemption off and finishes before its CPU next goes idle or takes a timer
//! tick. So if every CPU has passed through one of those points since the
//! writer swapped the pointer, every reader that could have been holding the
//! old one has finished with it. That is the whole idea, and it is why this is
//! cheap: readers cost nothing at all, and the writer pays by waiting.
//!
//! **What this is not.** There are no callbacks, no grace-period batching, no
//! expedited mode and no counter rollover handling. Real RCU is those things,
//! and a kernel that needs to free ten thousand objects a second needs them.
//! This is the primitive underneath, correct for the use this kernel has today
//! — a handful of publications during bring-up, and stage 14's domain mode
//! switch, which is one quiescence per switch — and honest about being that.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::percpu::{MAX_CPUS, Padded, PerCpu};
use crate::{arch, smp, timer};

/// How many times each CPU has passed through a point where it is provably not
/// holding a reference to anything RCU protects.
static QUIESCENT: PerCpu<AtomicU64> = PerCpu::new([const { Padded(AtomicU64::new(0)) }; MAX_CPUS]);

/// Grace periods that have completed.
static PERIODS: AtomicU64 = AtomicU64::new(0);

/// Record that this CPU holds no RCU-protected reference.
///
/// Called from the idle loop and from the timer interrupt. Both are points
/// where the CPU is demonstrably between readers rather than inside one: the
/// idle loop because it has nothing else on its stack, and the timer because a
/// reader runs with interrupts masked, so a timer interrupt being taken proves
/// no reader was in progress when it arrived.
pub(crate) fn quiesce() {
    let _ = QUIESCENT.mine().fetch_add(1, Ordering::Release);
}

/// Wait until every online CPU has quiesced at least once.
///
/// Returns false if a CPU did not pass a quiescent point inside
/// [`GRACE_TIMEOUT_NANOS`], which a caller must treat as a failure: freeing
/// after a grace period that did not happen is a use-after-free with a
/// reassuring function call in front of it.
///
/// The snapshot is taken before the wait, and each CPU's counter must be
/// *different* from what it was rather than merely non-zero. A CPU that has
/// quiesced a thousand times and then entered a reader is a CPU this must still
/// wait for.
pub(crate) fn synchronize() -> bool {
    let mut snapshot = [0u64; MAX_CPUS];
    let cpus = smp::online();
    for (cpu, slot) in snapshot.iter_mut().enumerate().take(cpus) {
        *slot = QUIESCENT
            .get(cpu)
            .map_or(0, |counter| counter.load(Ordering::Acquire));
    }

    // This CPU is one of the ones being waited for, and it is inside
    // `synchronize` rather than inside a reader, so it can answer for itself
    // immediately. Without this the boot CPU waits for its own timer tick,
    // which works and adds a millisecond to every grace period for no reason.
    quiesce();

    let deadline = timer::now_nanos() + GRACE_TIMEOUT_NANOS;
    loop {
        let all_passed = snapshot.iter().enumerate().take(cpus).all(|(cpu, &was)| {
            QUIESCENT
                .get(cpu)
                .is_some_and(|counter| counter.load(Ordering::Acquire) != was)
        });
        if all_passed {
            let _ = PERIODS.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        if timer::now_nanos() >= deadline {
            return false;
        }
        // A CPU sitting in `wfi` with its timer armed will quiesce on its own.
        // One that is spinning on something this CPU holds will not, and the
        // timeout above is the only thing that distinguishes those two cases.
        arch::spin_hint();
    }
}

/// How long a grace period may take before something is wrong.
///
/// Two timer periods would be the theoretical bound on an idle machine. This is
/// far more, because the bound that matters is the emulated one: under QEMU's
/// interpreter, on a build machine doing other things, a CPU can be descheduled
/// by the *host* for tens of milliseconds.
const GRACE_TIMEOUT_NANOS: u64 = 200_000_000;

/// Grace periods completed.
pub(crate) fn periods() -> u64 {
    PERIODS.load(Ordering::Relaxed)
}

/// How many times a CPU has quiesced.
pub(crate) fn quiescent_count(cpu: usize) -> Option<u64> {
    QUIESCENT.get(cpu).map(|slot| slot.load(Ordering::Acquire))
}
