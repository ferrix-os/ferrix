//! The boot check of `schedule=edf`'s reservations (`docs/microkernel/K9.md`, ledger 7011 S.6,
//! 7088): kernel threads through the same body the native call goes through. What a kernel
//! thread cannot reach, the call's decode and a restricted thread's budget, is a program's row.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use super::reserve::{self, Refusal, Reservation};
use super::{Task, spawn_on, wait_until_gone};

const MS: u64 = 1_000_000;

/// A thread of the check on `cpu`, which may run nowhere else.
fn start(
    cpu: usize,
    name: &'static str,
    entry: fn(usize),
    argument: usize,
) -> Result<Arc<Task>, &'static str> {
    spawn_on(name, entry, argument, NICE_0_WEIGHT, cpu, CpuSet::of(cpu))
        .map_err(|_| "edf: no thread")
}
/// How long the spinners run.
const WATCHED_NS: u64 = 400 * MS;
/// A budget of 20 ms in every 100 ms.
const LOW: Reservation = Reservation {
    runtime_ns: 20 * MS,
    deadline_ns: 100 * MS,
    period_ns: 100 * MS,
};
/// A budget of 4 ms in every 10 ms.
const SHORT: Reservation = Reservation {
    runtime_ns: 4 * MS,
    deadline_ns: 10 * MS,
    period_ns: 10 * MS,
};
/// 90% of a processor, twice past the limit.
const WIDE: Reservation = Reservation {
    runtime_ns: 90 * MS,
    deadline_ns: 100 * MS,
    period_ns: 100 * MS,
};

/// What the check's reserved threads found; zero is well.
static FOUND: AtomicUsize = AtomicUsize::new(0);
/// What a fair thread's spin was given to run, in its own count.
static STARTED: AtomicUsize = AtomicUsize::new(0);
/// Where the first spinner's reservation landed.
static LANDED: AtomicUsize = AtomicUsize::new(usize::MAX);
static STOP_AT: AtomicU64 = AtomicU64::new(0);
/// How often the shorter reservation waited past [`LONG_GAP_NS`] in the watch.
static LONG_GAPS: AtomicU64 = AtomicU64::new(0);
/// Its period less its budget is 6 ms; the other's budget is 20 ms.
const LONG_GAP_NS: u64 = 15 * MS;
/// How many runs a share's reading gets, for the host's stalls, as the kernel-time cases do. A
/// reading of shares and not of a charge.
const WATCHES: usize = 3;

const FAIL_REFUSALS: usize = 1;
const FAIL_NOT_ADMITTED: usize = 2;
const FAIL_AFFINITY: usize = 4;
const FAIL_CHILD: usize = 8;
const FAIL_GIVE_BACK: usize = 16;

fn spin_until_stopped() {
    while crate::timer::now_nanos() < STOP_AT.load(Ordering::Relaxed) {
        core::hint::spin_loop();
    }
}

/// A child of a reserved thread: it starts on the fair class.
fn child(_: usize) {
    if reserve::holds_own() {
        let _ = FOUND.fetch_or(FAIL_CHILD, Ordering::Relaxed);
    }
}

/// What a reserved thread does: every refusal by name, its own affinity refused, a child that
/// does not inherit, then its budget spun out until the watch ends.
fn reserved_low(argument: usize) {
    for (asked, want) in [
        (
            Reservation {
                runtime_ns: 1,
                deadline_ns: 10 * MS,
                period_ns: 10 * MS,
            },
            Refusal::Malformed,
        ),
        (
            Reservation {
                runtime_ns: MS,
                deadline_ns: 5 * MS,
                period_ns: 10 * MS,
            },
            Refusal::Malformed,
        ),
    ] {
        if reserve::take(asked) != Err(want) {
            let _ = FOUND.fetch_or(FAIL_REFUSALS, Ordering::Relaxed);
        }
    }
    if reserve::take(LOW).is_err() {
        let _ = FOUND.fetch_or(FAIL_NOT_ADMITTED, Ordering::Relaxed);
        return;
    }
    if reserve::take(LOW) != Err(Refusal::Already) {
        let _ = FOUND.fetch_or(FAIL_REFUSALS, Ordering::Relaxed);
    }
    let here = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    LANDED.store(here, Ordering::Release);
    if argument == 1 {
        if let Some(task) = super::current() {
            let other = CpuSet::of((here + 1) % crate::smp::count());
            if super::set_affinity(&task, &other) != Err(super::AffinityRefusal::Reserved) {
                let _ = FOUND.fetch_or(FAIL_AFFINITY, Ordering::Relaxed);
            }
        }
        if let Ok(task) = start(here, "reserve-child", child, 0) {
            let _ = wait_until_gone(&task, 2_000 * MS);
        }
    }
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    spin_until_stopped();
}

/// The second reservation on the first's processor: a shorter period, so the earlier deadline.
/// It counts its long gaps: run by earliest deadline it waits at most its period less its
/// budget, and passed over for the other's 20 ms budget it waits longer every 100 ms.
fn reserved_short(_: usize) {
    if reserve::take(SHORT).is_err() {
        let _ = FOUND.fetch_or(FAIL_NOT_ADMITTED, Ordering::Relaxed);
        return;
    }
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    let mut last = crate::timer::now_nanos();
    while last < STOP_AT.load(Ordering::Relaxed) {
        let now = crate::timer::now_nanos();
        if now.saturating_sub(last) >= LONG_GAP_NS {
            let _ = LONG_GAPS.fetch_add(1, Ordering::Relaxed);
        }
        last = now;
        core::hint::spin_loop();
    }
}

fn fair(_: usize) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    spin_until_stopped();
}

/// A reservation given back runs as before: the body's other half.
fn given_back(_: usize) {
    let refused = reserve::give_back();
    let took = reserve::take(LOW);
    let gave = reserve::give_back();
    if refused != Err(Refusal::Already) || took.is_err() || gave.is_err() || reserve::holds_own() {
        let _ = FOUND.fetch_or(FAIL_GIVE_BACK, Ordering::Relaxed);
    }
}

fn wait_for_started(count: usize) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos() + 5_000 * MS;
    while STARTED.load(Ordering::Acquire) < count {
        if crate::timer::now_nanos() > deadline {
            return Err("edf: a thread of the check did not start");
        }
        super::sleep_for(MS);
    }
    Ok(())
}

/// Whether `ran` of `of` is `want` parts per thousand, give or take a tenth of it and 5 ms.
fn near(ran: u64, of: u64, want: u64) -> bool {
    let expected = of / 1000 * want;
    let slack = expected / 10 + 5 * MS;
    ran.abs_diff(expected) <= slack
}

/// Run the check.
///
/// Verifies: H.SCHED.28, L.sched.179, L.sched.180, L.sched.181, L.sched.183
///
/// # Errors
///
/// What did not hold.
pub fn run() -> Result<(), &'static str> {
    let given = start(0, "reserve-back", given_back, 0)?;
    wait_until_gone(&given, 5_000 * MS)?;
    if FOUND.load(Ordering::Relaxed) != 0 {
        return Err("edf: a reservation given back did not come back");
    }

    let cpu = crate::smp::count().saturating_sub(1);
    STOP_AT.store(crate::timer::now_nanos() + 6_000 * MS, Ordering::Relaxed);
    let low = start(cpu, "reserve-low", reserved_low, 1)?;
    wait_for_started(1)?;
    let here = LANDED.load(Ordering::Acquire);
    if here != cpu {
        return Err("edf: a reservation did not stay on the caller's processor");
    }
    let short = start(cpu, "reserve-short", reserved_short, 0)?;
    let spin = start(cpu, "reserve-fair", fair, 0)?;
    wait_for_started(3)?;

    let (mut watched, mut low_ran, mut short_ran, mut fair_ran, mut gaps) = (0, 0, 0, 0, 0);
    for _ in 0..WATCHES {
        LONG_GAPS.store(0, Ordering::Relaxed);
        let first = (low.runtime(), short.runtime(), spin.runtime());
        let begun = crate::timer::now_nanos();
        super::sleep_for(WATCHED_NS);
        watched = crate::timer::now_nanos() - begun;
        let last = (low.runtime(), short.runtime(), spin.runtime());
        (low_ran, short_ran, fair_ran) = (last.0 - first.0, last.1 - first.1, last.2 - first.2);
        gaps = LONG_GAPS.load(Ordering::Relaxed);
        crate::console::println!(
            "  edf      {} ms watched: {} ms, {} ms, {} ms fair; {gaps} long gaps",
            watched / MS,
            low_ran / MS,
            short_ran / MS,
            fair_ran / MS,
        );
        if near(low_ran, watched, 200) && near(short_ran, watched, 400) && gaps < 3 {
            break;
        }
    }
    STOP_AT.store(0, Ordering::Relaxed);
    wait_until_gone(&low, 5_000 * MS)?;
    wait_until_gone(&short, 5_000 * MS)?;
    wait_until_gone(&spin, 5_000 * MS)?;

    let (used, throttles, refills) = reserve::state_of(cpu).ok_or("edf: no such processor")?;
    let found = FOUND.load(Ordering::Relaxed);
    crate::console::println!(
        "  edf      {throttles} throttles, {refills} refills, {used} ppm left"
    );
    if found & FAIL_REFUSALS != 0 {
        return Err("edf: a malformed or repeated reservation was not refused by name");
    }
    if found & FAIL_NOT_ADMITTED != 0 {
        return Err("edf: a reservation with room was refused");
    }
    if found & FAIL_AFFINITY != 0 {
        return Err("edf: a reserved thread's affinity was changed");
    }
    if found & FAIL_CHILD != 0 {
        return Err("edf: a child inherited its parent's reservation");
    }
    if !near(low_ran, watched, 200) {
        return Err("edf: a reservation was not held to 20% against one earlier and a fair one");
    }
    if !near(short_ran, watched, 400) {
        return Err("edf: the earlier deadline did not get its 40%");
    }
    if gaps >= 3 {
        return Err("edf: the earliest deadline was passed over");
    }
    if fair_ran < watched / 4 {
        return Err("edf: the fair class beneath the reservations was starved");
    }
    // The 10 ms budget alone runs out about forty times in the watch: a thread left on the
    // fair class reads the same shares and none of these.
    if throttles < watched / (10 * MS) / 2 || refills < watched / (10 * MS) / 2 {
        return Err("edf: the budgets did not run out and come back");
    }
    if used != 0 {
        return Err("edf: an ended thread's share did not come back");
    }
    unschedulable(cpu)?;
    Ok(())
}

/// A set too wide for every processor the thread may run on is refused by name.
fn unschedulable(cpu: usize) -> Result<(), &'static str> {
    static ANSWERS: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
    fn fill(which: usize) {
        let answer = match reserve::take(WIDE) {
            Ok(()) => 1,
            Err(Refusal::Unschedulable) => 2,
            Err(_) => 3,
        };
        if let Some(slot) = ANSWERS.get(which) {
            slot.store(answer, Ordering::Release);
        }
        while which == 0 && ANSWERS[1].load(Ordering::Acquire) == 0 {
            super::sleep_for(MS);
        }
    }
    let one = start(cpu, "reserve-wide", fill, 0)?;
    while ANSWERS[0].load(Ordering::Acquire) == 0 {
        super::sleep_for(MS);
    }
    let two = start(cpu, "reserve-wide", fill, 1)?;
    wait_until_gone(&two, 5_000 * MS)?;
    wait_until_gone(&one, 5_000 * MS)?;
    if ANSWERS[0].load(Ordering::Acquire) != 1 || ANSWERS[1].load(Ordering::Acquire) != 2 {
        return Err("edf: an unschedulable set was not refused");
    }
    Ok(())
}
