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

/// Timer interrupts taken since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// The period to re-arm with on each tick, or zero for a one-shot.
static INTERVAL: AtomicU64 = AtomicU64::new(0);

/// When the next periodic tick is due, on [`now_nanos`]'s timescale.
///
/// A periodic timer is a *schedule*, not a delay repeated: tick `n` is due at
/// `start + n * interval`, and the instant it actually arrived has no say in
/// when tick `n + 1` is due. Keeping the deadline here rather than re-deriving
/// it from the clock inside the handler is what makes that true.
static DEADLINE: AtomicU64 = AtomicU64::new(0);

/// How far behind the schedule may fall before it is abandoned rather than
/// caught up.
///
/// Catching up matters: a tick delivered late must not push the next one late
/// as well, or a periodic timer on a busy machine drifts without bound. But
/// catching up without a limit is its own failure — a kernel held off for a
/// second with a millisecond period owes a thousand interrupts, and delivering
/// them back to back is a storm that arrives exactly when the machine is least
/// able to absorb it. Past this many intervals the debt is written off and the
/// schedule restarts from now.
const MAX_CATCH_UP: u64 = 16;

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
        // **From the deadline that just passed, not from now.** Arming for
        // `interval` here would make the period `interval` plus however long
        // this interrupt took to arrive and be handled, every single time —
        // so the error would not average out, it would accumulate, and a
        // timer asked for a thousand ticks a second would deliver however
        // many the machine's interrupt latency allowed. Under an emulator
        // that is a factor of two.
        let next = DEADLINE.load(Ordering::Relaxed).saturating_add(interval);
        arm_periodic(next, interval);
    } else {
        // **Not optional, and not symmetry for its own sake.** AArch64's timer
        // interrupt is level triggered: the line stays asserted for as long as
        // the comparator is in the past. A one-shot that returned without
        // disarming would be acknowledged, re-asserted before the handler had
        // returned, and the machine would take that interrupt forever.
        // Nothing is armed from here on, whichever deadline this was.
        if let Some(slot) = armed_slot() {
            slot.store(0, Ordering::Relaxed);
        }
        arch::timer_disarm_fired();
    }
    // The scheduler arms this timer for the moment its processor next has a
    // decision to make, so every expiry is one. It only sets a flag: the
    // decision itself is made at interrupt exit, once the controller has been
    // acknowledged. Nothing happens here before the scheduler is up.
    crate::sched::timer_expired();
}

/// The most processors whose one-shot this module remembers.
const PROCESSORS: usize = 256;

/// When each processor's one-shot is armed to fire, on [`now_nanos`]'s
/// timescale, or zero for none armed (or not known).
///
/// # Why a decision is not a reprogramming
///
/// The scheduler asks for its next decision at every switch and every wake
/// that wants one, which is several times a round trip between two programs.
/// Under a hypervisor each reprogramming is an exit -- on x86-64 the local
/// APIC's registers are memory the hypervisor emulates -- and a channel round
/// trip took four of them. So a one-shot already armed no later than the
/// deadline asked for is left as it is: it fires early, the scheduler finds
/// nothing yet due and asks again, and the asking is what was going to happen
/// anyway. Only a deadline earlier than the one armed is written. An early
/// interrupt costs one decision; a late one would cost a task its turn.
///
/// # Why a skip is never late
///
/// What is kept is an upper bound on when the interrupt fires: the clock is
/// read *after* the architecture has armed the hardware, plus the delay
/// asked for. The hardware's count starts at or before that read -- the
/// local APIC's at the write of its initial count, the generic timer's from
/// the counter value its comparator was computed from -- and is the delay
/// rounded down to whole timer ticks, so it fires no later than the bound.
/// A request is skipped only when that bound is no later than its own
/// deadline, so a skipped request is never served late: the one exception
/// is the hardware's own, a delay shorter than one timer tick, which every
/// arm rounds up to one tick whether or not anything is skipped. Read before
/// the write, the bound could be early by the time the write took -- an exit
/// under a hypervisor -- and a skip late by as much.
static ARMED: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];

/// The deadline each processor's armed one-shot was asked for, on
/// [`now_nanos`]'s timescale: what [`ARMED`]'s bound was written for, a
/// little earlier than it by the write's own time. Meaningful only while
/// [`ARMED`] is not zero.
///
/// **Why it is kept.** A sleeper's deadline is asked for again at every
/// decision, as the same instant. Its arm's bound is read after the write,
/// so it is later than the deadline by the write's time, and the second
/// request, for the same instant, found the bound later than it and wrote
/// the hardware again: an exit at every switch of a round trip while any
/// task slept on the processor. A request no earlier than the one the
/// armed one-shot was written for is served by that one-shot exactly as
/// well as the first request was, so it is skipped too.
static REQUESTED: [AtomicU64; PROCESSORS] = [const { AtomicU64::new(0) }; PROCESSORS];

/// This processor's slot in [`ARMED`], once processors have records.
fn armed_slot() -> Option<&'static AtomicU64> {
    crate::smp::this_cpu().and_then(|cpu| ARMED.get(cpu.logical))
}

/// Fire the timer interrupt once, `nanos` from now: or sooner, when it is
/// armed already for sooner (see [`ARMED`]).
///
/// Called with interrupts masked on the processor whose timer it arms, as the
/// scheduler calls it.
pub(crate) fn after(nanos: u64) {
    after_from(nanos, now_nanos());
}

/// [`after`], for a caller that read the clock at `now` a moment ago: the
/// scheduler, which reads it once a decision. The deadline asked for is
/// taken from `now`, so it is no later than [`after`]'s would be, and a skip
/// it allows is never late; the bound kept after an arm is still read after
/// the write.
pub(crate) fn after_from(nanos: u64, now: u64) {
    // A load first, and the swap only for a periodic timer: a one-shot asked
    // for at every switch makes no read-modify-write here.
    let periodic =
        INTERVAL.load(Ordering::Relaxed) != 0 && INTERVAL.swap(0, Ordering::Relaxed) != 0;
    let Some(cpu) = crate::smp::this_cpu().map(|cpu| cpu.logical) else {
        arch::timer_arm(nanos);
        return;
    };
    let (Some(slot), Some(requested)) = (ARMED.get(cpu), REQUESTED.get(cpu)) else {
        arch::timer_arm(nanos);
        return;
    };
    let wanted = now.saturating_add(nanos).max(1);
    let armed = slot.load(Ordering::Relaxed);
    if !periodic
        && armed != 0
        && (armed <= wanted || requested.load(Ordering::Relaxed) <= wanted)
    {
        return;
    }
    arch::timer_arm(nanos);
    // After the arm, so that what is kept bounds the interrupt from above.
    slot.store(now_nanos().saturating_add(nanos).max(1), Ordering::Relaxed);
    requested.store(wanted, Ordering::Relaxed);
}

/// Fire the timer interrupt every `nanos` until [`stop`].
///
/// Every `nanos` from *now*, and thereafter on that schedule: the periods are
/// measured from the deadlines they were due at rather than from the instants
/// the interrupts arrived, so a late tick does not make its successors late.
pub(crate) fn every(nanos: u64) {
    if let Some(slot) = armed_slot() {
        slot.store(0, Ordering::Relaxed);
    }
    let next = now_nanos().saturating_add(nanos);
    INTERVAL.store(nanos, Ordering::Relaxed);
    arm_periodic(next, nanos);
}

/// Record `next` as the deadline and arm for it, resynchronising if the
/// schedule has fallen too far behind to be worth catching up.
fn arm_periodic(next: u64, interval: u64) {
    let now = now_nanos();
    let behind = now.saturating_sub(next);
    let next = if behind > interval.saturating_mul(MAX_CATCH_UP) {
        now.saturating_add(interval)
    } else {
        next
    };
    DEADLINE.store(next, Ordering::Relaxed);
    arm_at(next);
}

/// Arm the hardware for an absolute deadline.
///
/// The architecture layer takes a delay because that is what a countdown timer
/// like the local APIC's can be given, so the subtraction happens here — once,
/// against the same counter every deadline is expressed in.
///
/// A one-shot at an absolute deadline is what a sleeping task wants, since it
/// knows when it should wake rather than how long it has left; stage 5 can
/// make this public the moment it has a caller for it.
fn arm_at(deadline: u64) {
    // A deadline in the past becomes a delay of zero, which the architecture
    // layer arms as its smallest possible interval: late, but arriving, which
    // is the only useful reading of "wake me at a time that has passed".
    arch::timer_arm(deadline.saturating_sub(now_nanos()));
}

/// Stop the timer. The counter keeps running; it always does.
///
/// A one-shot is left to fire rather than written off, for the reason
/// [`ARMED`] gives: what it costs is one decision that finds nothing to do,
/// where stopping it is an exit under a hypervisor every time a processor
/// goes quiet. A periodic timer, which would fire for ever, is stopped.
pub(crate) fn stop() {
    // A load first, and the swap only for a periodic timer: a one-shot asked
    // for at every switch makes no read-modify-write here.
    let periodic = INTERVAL.load(Ordering::Relaxed) != 0 && INTERVAL.swap(0, Ordering::Relaxed) != 0;
    // A one-shot that is not armed -- one that fired, which leaves the
    // hardware quiet (`timer_disarm_fired`), or one never armed -- has
    // nothing to stop: writing it again was two exits at every switch to a
    // processor with nothing waiting, which is every switch of a round trip.
    // Without a slot to say so, it is stopped as before.
    if periodic || armed_slot().is_none() {
        if let Some(slot) = armed_slot() {
            slot.store(0, Ordering::Relaxed);
        }
        arch::timer_disarm();
    }
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
    ticks_to_nanos(arch::counter_now(), hz)
}

/// `ticks` of a `hz` counter in nanoseconds: `ferrix_vdso::counter_nanos`,
/// whose host tests hold it to the 128-bit formula.
fn ticks_to_nanos(ticks: u64, hz: u64) -> u64 {
    ferrix_vdso::counter_nanos(ticks, hz)
}

/// How fast the free-running counter counts.
pub(crate) fn counter_hz() -> u64 {
    arch::counter_hz()
}
