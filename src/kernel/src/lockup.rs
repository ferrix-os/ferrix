//! The lockup detector: processors watch each other, and one that takes no
//! interrupt for ten seconds ends in a failure report (`docs/POSTMORTEM.md`
//! §6.2, `ferrix.lockup=on`).
//!
//! A processor spinning with interrupts masked never panics and never lets
//! `ferrix.panic=reset` run: the machine just stops. Here, each processor's
//! watch tick (`watch`, at least once a second) reads the heartbeat of the
//! next online processor, in a ring, so the watcher is watched too. A
//! heartbeat unchanged for [`STUCK_NANOS`] on the counter is a lockup.
//!
//! * On x86-64 the watcher records the stop word (`a lockup`), names the
//!   stuck processor for the NMI (`watch::name_for_nmi`) and sends it one.
//!   Its NMI handler reports with its own frame, so the backtrace is the
//!   stuck code's. If no report has begun [`NMI_NANOS`] later, decided on a
//!   later tick and never by waiting in one, the watcher reports instead.
//! * On AArch64 and ARMv7-A, which have no NMI here, the watcher reports at
//!   once, naming the processor.
//!
//! Off unless asked for. One processor has nobody to watch it.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::BootView;

use crate::console::println;
use crate::panic::{catalog, fatal};
use crate::watch::{self, Watch};

/// The command-line option, and the one value that turns the detector on.
pub(crate) const OPTION: &str = "ferrix.lockup";
const ON: &str = "on";
const OFF: &str = "off";

/// How long a heartbeat may stand still before its processor is stuck: ten
/// seconds, the bound past which a wait for another processor calls it stuck
/// (`smp`, 2026-10-07): under it, a virtual processor its host starved is
/// slow, not stuck.
pub(crate) const STUCK_NANOS: u64 = 10_000_000_000;

/// How long after its NMI a stuck processor has to begin its own report
/// before its watcher reports instead.
const NMI_NANOS: u64 = 2_000_000_000;

/// The most processors the detector keeps a watcher's state for.
const PROCESSORS: usize = 256;

/// Whether `ferrix.lockup=on` was asked for.
static ASKED: AtomicBool = AtomicBool::new(false);

/// What each watcher last saw of the processor it watches, by the watcher's
/// logical number: the target plus one (0 for none yet), the target's
/// heartbeat, since when it has stood at that, and when an NMI went to it
/// (0 for none). Each is written by its watcher alone.
static TARGET: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];
static SEEN: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];
static SINCE: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];
static NMI_AT: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];

/// The detector's watch.
static DETECTOR: Watch = Watch { tick: on_tick };

/// Read `ferrix.lockup` from the loader's command line: once, at boot.
/// Exactly `on` asks for the detector; no option or `off` is the default;
/// anything else is said and taken as off.
pub(crate) fn read_option(view: &BootView<'_>) {
    match view.option(OPTION) {
        None | Some(OFF) => {}
        Some(ON) => ASKED.store(true, Ordering::Release),
        Some(other) => {
            println!("  lockup   {OPTION}={other} is not understood; the detector is off");
        }
    }
}

/// Whether the detector was asked for.
pub(crate) fn asked() -> bool {
    ASKED.load(Ordering::Acquire)
}

/// Start the detector, if asked: once every processor is online, before
/// init. Says what it watches.
pub(crate) fn start() {
    if !asked() {
        return;
    }
    let online = crate::smp::topology().map_or(1, crate::smp::Topology::online);
    if watch::register(&DETECTOR).is_err() {
        println!("  lockup   no room to register the detector; it is off");
        return;
    }
    watch::start();
    if online < 2 {
        println!("  lockup   {OPTION}={ON}: one processor, which nothing watches");
    } else {
        println!(
            "  lockup   {OPTION}={ON}: {online} processors watch each other in a ring; one that takes no interrupt for {} s is reported",
            STUCK_NANOS / 1_000_000_000
        );
    }
}

/// The processor `me` watches: the next online one after it, in logical
/// order, round the ring; none when it is the only one.
fn target_of(me: usize) -> Option<usize> {
    let cpus = crate::smp::topology()?.cpus();
    let count = cpus.len();
    (1..count)
        .map(|step| (me + step) % count)
        .find(|&cpu| cpus.get(cpu).is_some_and(crate::smp::PerCpu::is_online))
}

/// A watch tick on this processor: look at the one it watches.
fn on_tick(now: u64) {
    if crate::smp::stopping() {
        return;
    }
    let Some(me) = crate::smp::this_cpu().map(|cpu| cpu.logical) else {
        return;
    };
    let (Some(target_slot), Some(seen), Some(since), Some(nmi_at)) =
        (TARGET.get(me), SEEN.get(me), SINCE.get(me), NMI_AT.get(me))
    else {
        return;
    };
    let Some(target) = target_of(me) else {
        return;
    };
    let beats = watch::beats(target);
    let named = target as u64 + 1;
    if target_slot.load(Ordering::Relaxed) != named || seen.load(Ordering::Relaxed) != beats {
        target_slot.store(named, Ordering::Relaxed);
        seen.store(beats, Ordering::Relaxed);
        since.store(now, Ordering::Relaxed);
        nmi_at.store(0, Ordering::Relaxed);
        return;
    }
    let stood = now.saturating_sub(since.load(Ordering::Relaxed));
    if stood < STUCK_NANOS {
        return;
    }
    stuck(target, stood, now, nmi_at);
}

/// Processor `target` has taken no interrupt for `stood` nanoseconds.
fn stuck(target: usize, stood: u64, now: u64, nmi_at: &AtomicU64) {
    let seconds = stood / 1_000_000_000;
    let sent = nmi_at.load(Ordering::Relaxed);
    if sent == 0 {
        // How the boot stopped, before anything else: whichever processor
        // ends up reporting, the next loader says a lockup.
        crate::console::pstore::locked_up();
        if send_nmi(target) {
            nmi_at.store(now.max(1), Ordering::Relaxed);
            return;
        }
        fatal!(
            catalog::LOCKUP,
            "processor {target} took no interrupt for {seconds} s"
        );
    }
    if now.saturating_sub(sent) >= NMI_NANOS {
        fatal!(
            catalog::LOCKUP,
            "processor {target} took no interrupt for {seconds} s, and began no report {} s after its NMI",
            NMI_NANOS / 1_000_000_000
        );
    }
}

/// Send processor `target` the lockup's NMI, where the architecture has one:
/// whether it went.
fn send_nmi(target: usize) -> bool {
    let Some(cpu) = crate::smp::record(target) else {
        return false;
    };
    watch::name_for_nmi(target);
    crate::arch::send_lockup_nmi(cpu.hardware_id).is_ok()
}
