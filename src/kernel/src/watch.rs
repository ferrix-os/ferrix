//! The watch: a heartbeat for each processor and a tick at least every
//! second on each, for whatever watches them (`docs/POSTMORTEM.md` §6).
//!
//! Core, so that the timer's tick, the inter-processor interrupt and the
//! idle path reach it without naming what watches: the lockup detector
//! (`lockup`) registers a [`Watch`] here, as a filesystem registers its
//! flush with `power`. Nothing runs here until [`start`] is called, and
//! until then every function below is one read of a flag set once.
//!
//! * **Heartbeats.** Every timer and inter-processor interrupt a processor
//!   takes bumps its own count ([`beat`]). A processor with interrupts
//!   masked bumps nothing, which is what a watcher looks for.
//! * **The watch tick.** Ferrix is tickless: a processor with nothing to do
//!   arms no timer and sleeps. Once watching, each processor keeps a
//!   one-shot armed at most [`PERIOD_NANOS`] ahead ([`arm_here`], from the
//!   tick itself and the idle path), so each one ticks, and runs every
//!   registered [`Watch`], at least once a second.
//! * **The NMI's name.** A watcher that finds a processor stuck names it
//!   here before it sends it a non-maskable interrupt ([`name_for_nmi`]); the
//!   NMI's handler asks whether it is the one named ([`named_here`]) before
//!   anything else.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_sync::IrqControl;

use crate::hooks::{Full, Hooks};

/// The longest a watching processor goes between ticks: one second.
pub(crate) const PERIOD_NANOS: u64 = 1_000_000_000;

/// The most processors the watch keeps a heartbeat for: the timer's own
/// bound.
const PROCESSORS: usize = 256;

/// Whether the watch has started. Set once, never cleared.
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Each processor's heartbeat, by logical number.
static BEATS: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];

/// The processor a watcher sent an NMI for a lockup, plus one; 0 for none.
static NMI_NAMED: AtomicUsize = AtomicUsize::new(0);

/// Something that runs on every watching processor's tick, with the
/// interrupt's time on the counter.
#[derive(Debug)]
pub(crate) struct Watch {
    /// What it does on a tick, interrupts masked, on that processor.
    pub(crate) tick: fn(now: u64),
}

/// Every registered [`Watch`]. Two is one more than the kernel registers.
static WATCHES: Hooks<Watch, 2> = Hooks::new();

/// Run `watch` on every watching processor's tick, once [`start`] has run.
///
/// # Errors
///
/// [`Full`] when the list is.
pub(crate) fn register(watch: &'static Watch) -> Result<(), Full> {
    WATCHES.register(watch)
}

/// Start watching: from now on every heartbeat counts and every processor
/// ticks at least once a [`PERIOD_NANOS`]. Once, on the boot processor, once
/// every processor is online; the others arm their own watch tick when the
/// interrupt this sends them arrives.
pub(crate) fn start() {
    WATCHING.store(true, Ordering::Release);
    let saved = <crate::arch::Irq as IrqControl>::disable();
    arm_here();
    <crate::arch::Irq as IrqControl>::restore(saved);
    let _ = crate::arch::send_ipi_to_others();
}

/// Whether the watch has started.
pub(crate) fn watching() -> bool {
    WATCHING.load(Ordering::Acquire)
}

/// Bump this processor's heartbeat: from every timer and inter-processor
/// interrupt it takes.
pub(crate) fn beat() {
    if !watching() {
        return;
    }
    if let Some(slot) = crate::smp::this_cpu().and_then(|cpu| BEATS.get(cpu.logical)) {
        let _ = slot.fetch_add(1, Ordering::Relaxed);
    }
}

/// Processor `cpu`'s heartbeat: how many interrupts it has taken since the
/// watch started.
pub(crate) fn beats(cpu: usize) -> u64 {
    BEATS
        .get(cpu)
        .map_or(0, |slot| slot.load(Ordering::Relaxed))
}

/// Keep this processor's watch tick armed: a one-shot no later than
/// [`PERIOD_NANOS`] from now. With interrupts masked, as the timer wants.
pub(crate) fn arm_here() {
    if watching() {
        crate::timer::after(PERIOD_NANOS);
    }
}

/// What the timer's tick does for the watch: the heartbeat, every
/// registered [`Watch`], and the next watch tick.
pub(crate) fn tick() {
    if !watching() {
        return;
    }
    beat();
    let now = crate::timer::now_nanos();
    for watch in WATCHES.iter() {
        (watch.tick)(now);
    }
    arm_here();
}

/// Name processor `cpu` as the one a lockup's NMI is for, before the NMI is
/// sent.
pub(crate) fn name_for_nmi(cpu: usize) {
    NMI_NAMED.store(cpu.wrapping_add(1), Ordering::Release);
}

/// Whether this processor is the one a lockup's NMI was sent to: the first
/// thing the NMI's handler asks. Never true before the watch started.
#[allow(
    dead_code,
    reason = "read by x86-64's NMI handler alone: the Arm architectures have no NMI here"
)]
pub(crate) fn named_here() -> bool {
    let named = NMI_NAMED.load(Ordering::Acquire);
    named != 0
        && crate::smp::this_cpu_for_report().is_ok_and(|cpu| cpu.logical.wrapping_add(1) == named)
}
