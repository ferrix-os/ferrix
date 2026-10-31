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
//! * **Where each processor is.** Every interrupt a processor takes while
//!   the watch runs records, in that processor's slot, what it interrupted:
//!   the instruction pointer, whether it was in user mode, the running task
//!   and the counter ([`sample`]). A failure report prints them, one `cpu`
//!   line per online processor ([`report`], `docs/POSTMORTEM.md` §6.9,
//!   `L.lockup.9`): a kernel address as its offset in the image, which names
//!   no slide and so is logged; a user address on the port alone.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_sync::IrqControl;

use crate::console::{println, println_unlogged};
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
pub struct Watch {
    /// What it does on a tick, interrupts masked, on that processor.
    pub tick: fn(now: u64),
}

/// Every registered [`Watch`]: the lockup detector, the user watchdog, and
/// room for S4b's feed and one more.
static WATCHES: Hooks<Watch, 4> = Hooks::new();

/// What each processor's last interrupt interrupted, by logical number:
/// the instruction pointer; the running task's id with [`USER_BIT`] set for
/// user mode; and the counter when it was taken (0 for none yet). Each is
/// written by its own processor alone, with interrupts masked.
static SAMPLE_PC: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];
static SAMPLE_TASK: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];
static SAMPLE_AT: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];

/// In [`SAMPLE_TASK`], the bit that says the interrupt came from user mode.
const USER_BIT: u64 = 1 << 63;

/// A sample older than this says its processor takes no interrupts: twice
/// the watch tick's period.
const STALE_NANOS: u64 = 2 * PERIOD_NANOS;

/// Run `watch` on every watching processor's tick, once [`start`] has run.
///
/// # Errors
///
/// [`Full`] when the list is.
pub fn register(watch: &'static Watch) -> Result<(), Full> {
    WATCHES.register(watch)
}

/// Start watching: from now on every heartbeat counts and every processor
/// ticks at least once a [`PERIOD_NANOS`]. Once, on the boot processor, once
/// every processor is online; the others arm their own watch tick when the
/// interrupt this sends them arrives.
pub fn start() {
    if WATCHING.swap(true, Ordering::AcqRel) {
        return;
    }
    let saved = <crate::arch::Irq as IrqControl>::disable();
    arm_here();
    <crate::arch::Irq as IrqControl>::restore(saved);
    let _ = crate::arch::send_ipi_to_others();
}

/// Whether the watch has started.
pub fn watching() -> bool {
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
pub fn beats(cpu: usize) -> u64 {
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
pub fn name_for_nmi(cpu: usize) {
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

/// Record what this processor's interrupt interrupted: `pc`, and whether it
/// was in user mode. From `trap::dispatch` for every interrupt, interrupts
/// masked; one read of a flag set once until the watch starts.
pub(crate) fn sample(pc: u64, from_user: bool) {
    if !watching() {
        return;
    }
    let Some(cpu) = crate::smp::this_cpu().map(|cpu| cpu.logical) else {
        return;
    };
    let (Some(at_pc), Some(at_task), Some(at)) =
        (SAMPLE_PC.get(cpu), SAMPLE_TASK.get(cpu), SAMPLE_AT.get(cpu))
    else {
        return;
    };
    let task = crate::sched::current_id().unwrap_or(0) & !USER_BIT;
    let mode = if from_user { USER_BIT } else { 0 };
    at_pc.store(pc, Ordering::Relaxed);
    at_task.store(task | mode, Ordering::Relaxed);
    at.store(crate::timer::now_nanos().max(1), Ordering::Relaxed);
}

/// Say where every online processor was at its last interrupt: one `cpu`
/// line each, for a failure report, after its `on` line (`L.lockup.9`).
/// Nothing while the watch is off. Lock-free and allocation-free.
pub(crate) fn report() {
    if !watching() {
        return;
    }
    let Some(topology) = crate::smp::topology() else {
        return;
    };
    let now = crate::timer::now_nanos();
    for cpu in topology.cpus().iter().filter(|cpu| cpu.is_online()) {
        let logical = cpu.logical;
        let at = SAMPLE_AT
            .get(logical)
            .map_or(0, |at| at.load(Ordering::Relaxed));
        let beats = beats(logical);
        let held = crate::sched::preemption_held(logical);
        if at == 0 {
            println!(
                "  cpu {logical:<4}  no interrupt sampled yet, {beats} beats, {held} preemption held"
            );
            continue;
        }
        let pc = SAMPLE_PC
            .get(logical)
            .map_or(0, |pc| pc.load(Ordering::Relaxed));
        let task = SAMPLE_TASK
            .get(logical)
            .map_or(0, |task| task.load(Ordering::Relaxed));
        let age_ms = now.saturating_sub(at) / 1_000_000;
        let stale = if now.saturating_sub(at) > STALE_NANOS {
            ", takes no interrupts"
        } else {
            ""
        };
        let id = task & !USER_BIT;
        if task & USER_BIT != 0 {
            println!(
                "  cpu {logical:<4}  task {id} in user mode, {age_ms} ms ago{stale}, {beats} beats, {held} preemption held"
            );
            println_unlogged!("  cpu {logical:<4}  at user {pc:#x}");
        } else if crate::backtrace::in_image(pc) {
            let offset = pc.wrapping_sub(crate::backtrace::image_start());
            println!(
                "  cpu {logical:<4}  task {id} in the kernel at kernel+{offset:#x}, {age_ms} ms ago{stale}, {beats} beats, {held} preemption held"
            );
        } else {
            println!(
                "  cpu {logical:<4}  task {id} in the kernel, outside the image, {age_ms} ms ago{stale}, {beats} beats, {held} preemption held"
            );
        }
    }
}
