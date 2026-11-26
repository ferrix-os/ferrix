//! The boot check of `schedule=windows` (`docs/microkernel/K9.md` §4, ledger 7460): the frame's
//! refusals through the bodies the boot authority calls, a job's partition, and on one held
//! processor a check's own frame, whose threads' stamps show each runs only in its own window,
//! the processor idles in a gap, a higher level runs first, equals take turns, and a wake into
//! another partition's window kicks nothing. After the boot's seal, [`after_seal`] reads every
//! refusal a sealed frame owes.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_sched::CpuSet;
use ferrix_sched_windows::frame::{Refusal, Window};

use super::windows::{self, Plan, Refused};
use super::{Task, queue_of, spawn_on, wait_until_gone};
use crate::object::job;

const MS: u64 = 1_000_000;
const US: u64 = 1_000;
/// The check's major frame.
const MAJOR: u64 = 10 * MS;
/// The worst overrun x86-64 allows: from a window's end to the pick that ended it, judged where
/// the timer is honest (KVM and TCG alike, ledger 7540 WX.4, 7549 WY.12). Never the samples'
/// test: those are judged against the pick itself (WX.3). On AArch64 the overrun is printed and
/// not judged: every AArch64 gate today is TCG, whose window-end interrupt arrives up to tens of
/// milliseconds late at gate load, and `-icount` is no instrument for it (a single-threaded
/// round robin skews the arm by 18-21 ms, WX.5). BACKLOG owes the judged form on an honest Arm
/// timer, and the cfg goes then (WY.11).
const SLACK: u64 = 500 * US;
/// How long each spinner runs, in wall time.
const RUN: u64 = 60 * MS;
/// How far ahead of its install the check's frame begins: the threads are woken before it, in
/// the frame's lead, where no partition's window is open and the held processor idles, so each
/// wake is one into another partition's window that would have kicked (L.sched.164), every run.
/// 100 ms is ten frames of idle.
const LEAD: u64 = 100 * MS;

/// The check's own frame, never the boot's, and its scratch frame for the refusals: statics,
/// since a frame is too large for a kernel stack.
static CHECK: Plan = Plan::new();
static SCRATCH: Plan = Plan::new();

/// The threads: A and C in partition 2, A above C; D and E equals in 3; F sleeps in 3.
const THREADS: usize = 5;
const NAMES: [&str; THREADS] = ["win-a", "win-c", "win-d", "win-e", "win-f"];
const PARTITION: [u8; THREADS] = [2, 2, 3, 3, 3];
const NICE: [i32; THREADS] = [-10, 10, 0, 0, 0];
/// Each one's window, offset and end in the major frame.
const fn window_of(partition: u8) -> (u64, u64) {
    if partition == 2 {
        (MS, 3 * MS)
    } else {
        (3 * MS, 6 * MS)
    }
}
/// The ends of the held processor's rows in the major frame: 0 to 1 ms, 2 to 3, 3 to 6, and the
/// gap to the frame's end (`frame_on`).
const ENDS: [u64; 4] = [MS, 3 * MS, 6 * MS, MAJOR];

/// The frames a thread's samples are kept for: a spinner runs [`RUN`] of wall time from its first
/// pick, in the frame's first window of its partition, so it samples in at most `RUN / MAJOR + 1`
/// frames (7) and the sleeper's 40 sleeps of 0.7 ms one per window in as many; 64 is room for a
/// late end. A sample past it fails by name (WY.8).
const FRAMES: usize = 64;
/// The window ends the held processor logs with the pick that ended each: at most three a frame
/// (partitions 0, 2 and 3; nothing runs in the gap) over the spinners' at most `3 * RUN` (18
/// frames), the kick case's 20 tries of up to two frames (40) and the waits between: 3 * 64. A
/// full log fails by name, and never passes (WY.8).
const LOG: usize = 3 * FRAMES;

static EPOCH: AtomicU64 = AtomicU64::new(0);
static INSIDE: [AtomicU64; THREADS] = [const { AtomicU64::new(0) }; THREADS];
static FIRST: [AtomicU64; THREADS] = [const { AtomicU64::new(u64::MAX) }; THREADS];
static LAST: [AtomicU64; THREADS] = [const { AtomicU64::new(0) }; THREADS];
/// Per thread and frame, the first and last sample outside its window, by time: each lies after
/// the end of its window in that frame (WX.3).
static OUT_FIRST: [[AtomicU64; FRAMES]; THREADS] =
    [const { [const { AtomicU64::new(u64::MAX) }; FRAMES] }; THREADS];
static OUT_LAST: [[AtomicU64; FRAMES]; THREADS] =
    [const { [const { AtomicU64::new(0) }; FRAMES] }; THREADS];
/// Samples outside a window with no end of it before them (before its first window), or past
/// [`FRAMES`]: each a failure.
static UNPLACED: AtomicU64 = AtomicU64::new(0);

/// The held processor while the check's frame is installed on it; `usize::MAX` otherwise.
static HELD: AtomicUsize = AtomicUsize::new(usize::MAX);
/// The arms judged on it (WX.1) and those that broke the rule, with the first broken pair.
static ARMS: AtomicU64 = AtomicU64::new(0);
static WRONG_ARMS: AtomicU64 = AtomicU64::new(0);
static WRONG_ASK: AtomicU64 = AtomicU64::new(0);
static WRONG_WANT: AtomicU64 = AtomicU64::new(0);
/// What reached the timer judged apart (WZ.2): the arms whose ask was nearer than the arm's
/// floor, so the floor decided them (K9 §4: their lateness may carry up to `MIN_ARM_NS`), and
/// those asked later than the ask or the floor, with the first such pair.
static FLOORED: AtomicU64 = AtomicU64::new(0);
static WRONG_REQUESTED: AtomicU64 = AtomicU64::new(0);
static WRONG_REQUESTED_AT: AtomicU64 = AtomicU64::new(0);
static WRONG_REQUESTED_BOUND: AtomicU64 = AtomicU64::new(0);
/// The log of window ends a task ran past on the held processor: (end, pick, requested).
static LOG_END: [AtomicU64; LOG] = [const { AtomicU64::new(0) }; LOG];
static LOG_PICK: [AtomicU64; LOG] = [const { AtomicU64::new(0) }; LOG];
static LOG_REQUESTED: [AtomicU64; LOG] = [const { AtomicU64::new(0) }; LOG];
static LOGGED: AtomicUsize = AtomicUsize::new(0);
static LOG_FULL: AtomicBool = AtomicBool::new(false);

/// The end of the window open at `at` in the check's own frame on the held processor, from the
/// check's constants alone, never the class's: the frame's epoch before it begins.
fn frame_end(at: u64) -> u64 {
    let epoch = EPOCH.load(Ordering::Acquire);
    if at < epoch {
        return epoch;
    }
    let phase = (at - epoch) % MAJOR;
    let start = at - phase;
    start + ENDS.into_iter().find(|&end| phase < end).unwrap_or(MAJOR)
}

fn on_held() -> bool {
    crate::smp::this_cpu().is_some_and(|cpu| cpu.logical == HELD.load(Ordering::Acquire))
}

fn wrong_arm(ask: u64, want: u64) {
    if WRONG_ARMS.fetch_add(1, Ordering::Relaxed) == 0 {
        WRONG_ASK.store(ask, Ordering::Relaxed);
        WRONG_WANT.store(want, Ordering::Relaxed);
    }
}

/// At every arm on the held processor while the check's frame is on it (WX.1, L.sched.201): the
/// instant the class asks for is its window's end from the check's frame, or a quantum's end
/// before it while an equal of the running task waits, exactly, in nanoseconds on
/// `now_nanos`'s timescale (WY.5); none asked fails. What reaches the timer, the instant handed to
/// `after_from`, is no later than the ask, or than `MIN_ARM_NS` from now where the ask is nearer
/// (WY.1): the arm's own floor. What is left unjudged is the counter write's own skew, which the
/// samples' judge measures as lateness (WX.3). Whether it judged.
pub(crate) fn judge_ask(
    class: &super::queue::Tier<Arc<Task>>,
    ask: Option<u64>,
    handed: Option<u64>,
    now: u64,
) -> bool {
    if !on_held() || class.part().is_none() {
        return false;
    }
    let _ = ARMS.fetch_add(1, Ordering::Relaxed);
    let at = class.now();
    let end = frame_end(at);
    let quantum = class
        .current()
        .filter(|_| class.runnable_now())
        .and_then(|_| class.remaining_ns())
        .map(|left| at.saturating_add(left));
    let want = quantum.map_or(end, |quantum| quantum.min(end));
    let floor = now.saturating_add(super::queue::MIN_ARM_NS);
    if ask != Some(want) || !handed.is_some_and(|handed| handed <= want.max(floor)) {
        wrong_arm(ask.unwrap_or(0), want);
    }
    true
}

/// After a judged arm: the deadline the one-shot was asked for is no later than the ask, or the
/// arm's floor (WY.1), counted apart from the ask's own judge (WZ.2), and the arms the floor
/// decided counted.
pub(crate) fn judge_requested(ask: Option<u64>, now: u64) {
    let requested = crate::timer::requested_here();
    let floor = now.saturating_add(super::queue::MIN_ARM_NS);
    let bound = ask.unwrap_or(0).max(floor);
    if ask.is_some_and(|ask| ask < floor) {
        let _ = FLOORED.fetch_add(1, Ordering::Relaxed);
    }
    if requested > bound && WRONG_REQUESTED.fetch_add(1, Ordering::Relaxed) == 0 {
        WRONG_REQUESTED_AT.store(requested, Ordering::Relaxed);
        WRONG_REQUESTED_BOUND.store(bound, Ordering::Relaxed);
    }
}

/// After every pick on the held processor while the check's frame is on it: a window's end a
/// task still ran at, the pick that ended it (the class's own stamp, the samples' timescale),
/// and the deadline the timer was asked for, logged once each (WX.3, WY.9).
pub(crate) fn note_pick(class: &super::queue::Tier<Arc<Task>>) {
    if !on_held() || class.part().is_none() {
        return;
    }
    // WR.3 (ledger 7648): which frames had a pick inside partition 2's window.
    let epoch = EPOCH.load(Ordering::Acquire);
    if let Some(since) = class.now().checked_sub(epoch)
        && partition_at(since % MAJOR) == Some(2)
    {
        let frame = since / MAJOR;
        if let Some(at) = usize::try_from(frame % 64)
            .ok()
            .and_then(|at| PICKED_IN_TWO.get(at))
        {
            at.store(frame + 1, Ordering::Relaxed);
        }
    }
    let Some((end, pick)) = class.last_overrun() else {
        return;
    };
    let logged = LOGGED.load(Ordering::Relaxed);
    if let Some(last) = logged.checked_sub(1)
        && LOG_END
            .get(last)
            .is_some_and(|at| at.load(Ordering::Relaxed) == end)
        && LOG_PICK
            .get(last)
            .is_some_and(|at| at.load(Ordering::Relaxed) == pick)
    {
        return;
    }
    let (Some(at_end), Some(at_pick), Some(at_requested)) = (
        LOG_END.get(logged),
        LOG_PICK.get(logged),
        LOG_REQUESTED.get(logged),
    ) else {
        LOG_FULL.store(true, Ordering::Relaxed);
        return;
    };
    at_end.store(end, Ordering::Relaxed);
    at_pick.store(pick, Ordering::Relaxed);
    at_requested.store(crate::timer::requested_here(), Ordering::Relaxed);
    LOGGED.store(logged + 1, Ordering::Release);
}

fn sample(index: usize) {
    let now = crate::timer::now_nanos();
    let epoch = EPOCH.load(Ordering::Acquire);
    let (start, end) = window_of(PARTITION.get(index).copied().unwrap_or(0));
    let phase = now.saturating_sub(epoch) % MAJOR;
    if now >= epoch && phase >= start && phase < end {
        if let Some(count) = INSIDE.get(index) {
            let _ = count.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        // Outside: kept against the latest end of its window before it (WX.3).
        let frame = now
            .checked_sub(epoch + end)
            .map(|since| since / MAJOR)
            .and_then(|frame| usize::try_from(frame).ok());
        match frame.and_then(|frame| {
            Some((
                OUT_FIRST.get(index)?.get(frame)?,
                OUT_LAST.get(index)?.get(frame)?,
            ))
        }) {
            Some((first, last)) => {
                let _ = first.fetch_min(now, Ordering::Relaxed);
                let _ = last.fetch_max(now, Ordering::Relaxed);
            }
            None => {
                let _ = UNPLACED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if let Some(first) = FIRST.get(index) {
        let _ = first.fetch_min(now, Ordering::Relaxed);
    }
    if let Some(last) = LAST.get(index) {
        let _ = last.fetch_max(now, Ordering::Relaxed);
    }
}

/// The log's entry for the window end `end`: its pick and requested deadline.
/// The log entry of the window end `end`, by its index.
fn logged_index(end: u64) -> Option<usize> {
    (0..LOGGED.load(Ordering::Acquire)).find(|&at| {
        LOG_END
            .get(at)
            .is_some_and(|logged| logged.load(Ordering::Relaxed) == end)
    })
}

fn logged_at(end: u64) -> Option<(u64, u64)> {
    (0..LOGGED.load(Ordering::Acquire)).find_map(|at| {
        (LOG_END.get(at)?.load(Ordering::Relaxed) == end).then(|| {
            (
                LOG_PICK
                    .get(at)
                    .map_or(0, |pick| pick.load(Ordering::Relaxed)),
                LOG_REQUESTED
                    .get(at)
                    .map_or(0, |requested| requested.load(Ordering::Relaxed)),
            )
        })
    })
}

/// One window's outside samples, judged: what was found.
struct Judged {
    windows: u64,
    outside: u64,
    /// Over the windows that had a task running past their end (a sample outside, WJ.1), the
    /// worst time run past it, the last sample outside less the end (WH.1, ledger 7732): the
    /// overrun judged, with that window's end, first and last outside sample, relative to the
    /// epoch, its lateness and its pick's distance past the end.
    worst: (u64, u64, u64, u64, u64, u64),
    /// Windows whose pick came past the bound while their last sample lay within it: a stall
    /// after the window's end (the host's, under a hypervisor), counted, with the worst pick
    /// past and its end (WH.2).
    stalls: u64,
    stall_worst: (u64, u64),
    /// The worst pick past the end of a window that ended with nothing of it running: the next
    /// window's start delay, with that end, counted and not judged (ledger 7714 WJ.2).
    start_delay: (u64, u64),
    late_worst: u64,
    /// Picks that came more than a frame after their window's end (WU.1), and the worst.
    frame_late: u64,
    frame_late_worst: u64,
}

/// Every sample outside its window lies between that window's end and the pick that ended it;
/// none after that pick, none before the window's first start (WX.3, H.SCHED.26 as built).
/// The logged end of a thread's window (offset `end` in the frame) at or before `first`
/// whose pick came after `last`: one overrun spanning the frames between.
fn spanning(first: u64, last: u64, end: u64) -> Option<usize> {
    let epoch = EPOCH.load(Ordering::Acquire);
    (0..LOGGED.load(Ordering::Acquire)).find(|&at| {
        let logged = LOG_END.get(at).map_or(0, |end| end.load(Ordering::Relaxed));
        let pick = LOG_PICK
            .get(at)
            .map_or(0, |pick| pick.load(Ordering::Relaxed));
        let own = logged >= epoch && (logged - epoch) % MAJOR == end;
        own && logged <= first && pick > last
    })
}

fn judge_samples() -> Result<Judged, &'static str> {
    let epoch = EPOCH.load(Ordering::Acquire);
    let mut found = Judged {
        windows: 0,
        outside: 0,
        worst: (0, 0, 0, 0, 0, 0),
        start_delay: (0, 0),
        stalls: 0,
        stall_worst: (0, 0),
        late_worst: 0,
        frame_late: 0,
        frame_late_worst: 0,
    };
    for at in 0..LOGGED.load(Ordering::Acquire) {
        let end = LOG_END.get(at).map_or(0, |end| end.load(Ordering::Relaxed));
        let pick = LOG_PICK
            .get(at)
            .map_or(0, |pick| pick.load(Ordering::Relaxed));
        let requested = LOG_REQUESTED
            .get(at)
            .map_or(0, |requested| requested.load(Ordering::Relaxed));
        found.windows += 1;
        found.late_worst = found.late_worst.max(pick.saturating_sub(requested));
        // WU.1: a pick more than a frame late is counted; the timer asked for later than the
        // window's end there is a wrong arm on every architecture, not the host's lateness (an
        // earlier ask is a quantum's end or the floor, which the arm's own judge reads).
        if pick.saturating_sub(end) > MAJOR {
            found.frame_late += 1;
            found.frame_late_worst = found.frame_late_worst.max(pick - end);
            if requested > end {
                print_window(
                    0,
                    end - epoch.min(end),
                    0,
                    0,
                    Some((pick - epoch.min(pick), requested)),
                );
                return Err(
                    "windows: a pick came more than a frame late and the timer was asked past the window's end",
                );
            }
        }
    }
    // Which logged ends had a task running past them (WJ.1): only those are overruns.
    let mut ran_past = [false; LOG];
    let mut broken = None;
    for index in 0..THREADS {
        let (_, end) = window_of(PARTITION.get(index).copied().unwrap_or(0));
        for frame in 0..FRAMES {
            let first = OUT_FIRST
                .get(index)
                .and_then(|row| row.get(frame))
                .map_or(u64::MAX, |at| at.load(Ordering::Acquire));
            if first == u64::MAX {
                continue;
            }
            let last = OUT_LAST
                .get(index)
                .and_then(|row| row.get(frame))
                .map_or(0, |at| at.load(Ordering::Acquire));
            found.outside += 1;
            let window_end = epoch + frame as u64 * MAJOR + end;
            // Where one end's pick came frames late (TCG, WX.4), the samples of the frames between
            // lie past that earlier end of the thread's own window, before its pick: judge them
            // against it (the rule is the same: past an end only until the pick that ended it).
            let logged = logged_index(window_end).or_else(|| spanning(first, last, end));
            let Some(at) = logged else {
                let _ = broken.get_or_insert("windows: a task ran outside its window and no pick after the window's end was logged");
                print_window(index, window_end - epoch, first - epoch, last - epoch, None);
                continue;
            };
            let entry =
                |log: &[AtomicU64; LOG]| log.get(at).map_or(0, |v| v.load(Ordering::Relaxed));
            let (logged_end, pick, requested) =
                (entry(&LOG_END), entry(&LOG_PICK), entry(&LOG_REQUESTED));
            if let Some(mark) = ran_past.get_mut(at) {
                *mark = true;
            }
            let ran = last.saturating_sub(logged_end);
            let pick_past = pick.saturating_sub(logged_end);
            if ran > found.worst.0 {
                found.worst = (
                    ran,
                    logged_end - epoch.min(logged_end),
                    first - epoch,
                    last - epoch,
                    pick.saturating_sub(requested),
                    pick_past,
                );
            }
            if pick_past > SLACK && ran <= SLACK {
                found.stalls += 1;
                if pick_past > found.stall_worst.0 {
                    found.stall_worst = (pick_past, logged_end - epoch.min(logged_end));
                }
            }
            if last >= pick || first < window_end {
                let _ = broken.get_or_insert(
                    "windows: a task ran outside its window past the pick that ended it",
                );
                print_window(
                    index,
                    window_end - epoch,
                    first - epoch,
                    last - epoch,
                    Some((pick - epoch.min(pick), requested)),
                );
            }
        }
    }
    // WJ.2: an end with nothing of its window running past it: the next window's start delay.
    for (at, ran) in ran_past
        .iter()
        .enumerate()
        .take(LOGGED.load(Ordering::Acquire))
    {
        let end = LOG_END.get(at).map_or(0, |end| end.load(Ordering::Relaxed));
        let pick = LOG_PICK
            .get(at)
            .map_or(0, |pick| pick.load(Ordering::Relaxed));
        if !ran && pick.saturating_sub(end) > found.start_delay.0 {
            found.start_delay = (pick - end, end - epoch.min(end));
        }
    }
    if LOG_FULL.load(Ordering::Acquire) {
        return Err("windows: the check's log of window ends was full");
    }
    if UNPLACED.load(Ordering::Acquire) != 0 {
        return Err(
            "windows: a task ran outside its window before its first one, or past the frames kept",
        );
    }
    if let Some(broken) = broken {
        print_log(epoch);
        return Err(broken);
    }
    Ok(found)
}

static PRINTED: AtomicUsize = AtomicUsize::new(0);
/// The most per-window lines a failure prints (WY.7).
const PRINT_CAP: usize = 40;

fn print_window(index: usize, end: u64, first: u64, last: u64, pick: Option<(u64, u64)>) {
    if PRINTED.fetch_add(1, Ordering::Relaxed) >= PRINT_CAP {
        return;
    }
    let name = NAMES.get(index).copied().unwrap_or("?");
    match pick {
        Some((pick, requested)) => crate::println!(
            "  windows  {name} window ending {end} ns: outside {first}..{last}, picked at {pick}, requested {requested}"
        ),
        None => crate::println!(
            "  windows  {name} window ending {end} ns: outside {first}..{last}, no pick logged"
        ),
    }
}

/// On failure, every logged window: its end, its pick and the deadline asked, relative to the
/// epoch, capped (WY.7, WY.9).
fn print_log(epoch: u64) {
    let logged = LOGGED.load(Ordering::Acquire);
    for at in 0..logged {
        if PRINTED.fetch_add(1, Ordering::Relaxed) >= PRINT_CAP {
            crate::println!("  windows  and {} more", logged - at);
            return;
        }
        let end = LOG_END.get(at).map_or(0, |end| end.load(Ordering::Relaxed));
        let pick = LOG_PICK
            .get(at)
            .map_or(0, |pick| pick.load(Ordering::Relaxed));
        let requested = LOG_REQUESTED
            .get(at)
            .map_or(0, |requested| requested.load(Ordering::Relaxed));
        crate::println!(
            "  windows  window ending {} ns: picked +{} ns, requested {} ns, lateness {} ns",
            end.saturating_sub(epoch),
            pick.saturating_sub(end),
            requested.saturating_sub(epoch),
            pick.saturating_sub(requested)
        );
    }
}

/// A spinner: waits to be put in its partition and woken, then samples for [`RUN`].
fn spinner(index: usize) {
    super::sleep_for(10_000 * MS);
    let until = crate::timer::now_nanos() + RUN;
    while crate::timer::now_nanos() < until {
        sample(index);
        core::hint::spin_loop();
    }
}

/// Set to end [`woken_in`].
static STOP_K: AtomicBool = AtomicBool::new(false);

/// The kick case's thread, in partition 2: sleeps until woken, until told to stop (WX.6).
fn woken_in(_index: usize) {
    while !STOP_K.load(Ordering::Acquire) {
        super::sleep_for(10_000 * MS);
        WOKEN_AT.store(crate::timer::now_nanos(), Ordering::Release);
    }
}

/// WX.1's judgement of the held processor's arms so far (L.sched.201): run after the main case,
/// before the kick cases whose timing a planted fault would break first, and again at the end.
fn judge_arms(arms: u64) -> Result<(), &'static str> {
    if WRONG_ARMS.load(Ordering::Acquire) != 0 {
        let epoch = EPOCH.load(Ordering::Acquire);
        crate::println!(
            "  windows  {} of {arms} arms wrong, the first asked {} ns for {} ns (from the epoch)",
            WRONG_ARMS.load(Ordering::Acquire),
            WRONG_ASK.load(Ordering::Acquire).wrapping_sub(epoch) as i64,
            WRONG_WANT.load(Ordering::Acquire).wrapping_sub(epoch) as i64
        );
        return Err("windows: the deadline asked at a window's end is not the window's end");
    }
    if WRONG_REQUESTED.load(Ordering::Acquire) != 0 {
        let epoch = EPOCH.load(Ordering::Acquire);
        crate::println!(
            "  windows  {} of {arms} arms reached the timer late, the first at {} ns for at most {} ns (from the epoch)",
            WRONG_REQUESTED.load(Ordering::Acquire),
            WRONG_REQUESTED_AT
                .load(Ordering::Acquire)
                .wrapping_sub(epoch) as i64,
            WRONG_REQUESTED_BOUND
                .load(Ordering::Acquire)
                .wrapping_sub(epoch) as i64
        );
        return Err("windows: the timer was asked later than the class's ask or the arm's floor");
    }
    Ok(())
}

/// The WN.4 case's attempts, one a frame.
const TRIES: usize = 50;

/// Per frame modulo 64, the latest frame (plus one) in which the held processor picked inside
/// partition 2's window (WR.3).
static PICKED_IN_TWO: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// Whether the held processor picked inside partition 2's window of `frame` (since the epoch).
fn picked_in_two(frame: u64) -> bool {
    usize::try_from(frame % 64)
        .ok()
        .and_then(|at| PICKED_IN_TWO.get(at))
        .is_some_and(|at| at.load(Ordering::Relaxed) == frame + 1)
}

/// The partition whose window on the held processor holds `phase` in the check's frame
/// (`frame_on`); `None` in the gap.
const fn partition_at(phase: u64) -> Option<u8> {
    if phase < MS {
        Some(0)
    } else if phase < 3 * MS {
        Some(2)
    } else if phase < 6 * MS {
        Some(3)
    } else {
        None
    }
}

/// WS.1 (ledger 7645): a failed WN.4 attempt's figures, relative to the epoch: the stamp, the
/// window's start and end, the logged pick at that end with the deadline asked, the stamp's
/// phase in its frame and whose window holds it, and the log's fill.
fn print_attempt(ran: u64, window_start: u64, end: u64, epoch: u64, looked: (u64, u64, u64)) {
    let (woke, looks, quiet) = looked;
    let at = |time: u64| time.saturating_sub(epoch);
    let phase = at(ran) % MAJOR;
    let holder = match partition_at(phase) {
        Some(partition) => partition_name(partition),
        None => "the gap",
    };
    let logged = LOGGED.load(Ordering::Acquire);
    let full = LOG_FULL.load(Ordering::Acquire);
    crate::println!(
        "  windows  WN.4 attempt: woken at {} ns, {looks} looks, {quiet} read a gap",
        at(woke)
    );
    match logged_at(end) {
        Some((pick, requested)) => crate::println!(
            "  windows  WN.4 attempt: ran {} ns, window {}..{} ns, picked at {} requested {}, phase {phase} ns in {holder}, log {logged} of {LOG}{}",
            at(ran),
            at(window_start),
            at(end),
            at(pick),
            at(requested),
            if full { ", full" } else { "" }
        ),
        None => crate::println!(
            "  windows  WN.4 attempt: ran {} ns, window {}..{} ns, no pick logged at its end, phase {phase} ns in {holder}, log {logged} of {LOG}{}",
            at(ran),
            at(window_start),
            at(end),
            if full { ", full" } else { "" }
        ),
    }
}

const fn partition_name(partition: u8) -> &'static str {
    match partition {
        0 => "partition 0",
        2 => "partition 2",
        _ => "partition 3",
    }
}

/// WS.2 (ledger 7645) and WR.3 (7648), AArch64 alone: a stamp past its window's end passes when
/// it lies inside partition 2's window of a later frame k and, in every frame between, either the
/// held processor made no pick inside partition 2's window (it stalled across it) or that
/// window's end has a logged pick (the task was picked and stalled before its stamp); past frame
/// k's window's end only before its logged pick (WU.3). A pick inside such a window with no
/// logged end means the task was runnable there and not run. Each frame missed is counted.
fn late_stamp(ran: u64, window_start: u64, epoch: u64) -> Result<(), &'static str> {
    let no_pick = || {
        if LOG_FULL.load(Ordering::Acquire) {
            "windows: a task woken in partition 0's window ran late and the log of window ends was full"
        } else {
            "windows: a task woken in partition 0's window ran late past an end with no logged pick"
        }
    };
    let frames = (ran - window_start) / MAJOR;
    let start = window_start + frames * MAJOR;
    let end = start + 2 * MS;
    let first = window_start.saturating_sub(epoch) / MAJOR;
    let mut missed = 0_u64;
    for frame in 0..frames {
        let ends = window_start + frame * MAJOR + 2 * MS;
        if logged_at(ends).is_some() {
            continue;
        }
        if picked_in_two(first + frame) {
            return Err("windows: a task was runnable in its window and not run");
        }
        missed += 1;
    }
    let _ = MISSED.fetch_add(missed, Ordering::Relaxed);
    if missed != 0 {
        crate::println!(
            "  windows  WN.4: the held processor made no pick in {missed} of partition 2's windows before its stamp, {} ns late (a stalled vCPU, WR.3)",
            ran - window_start
        );
    }
    if ran < end {
        return Ok(());
    }
    match logged_at(end) {
        Some((pick, _)) if ran < pick => Ok(()),
        Some(_) => Err("windows: a task woken in partition 0's window ran outside its own"),
        None => Err(no_pick()),
    }
}

/// Partition 2's windows WN.4's task missed whole while the held processor stalled (WR.3).
static MISSED: AtomicU64 = AtomicU64::new(0);

/// When [`woken_in`] last ran after a wake.
static WOKEN_AT: AtomicU64 = AtomicU64::new(0);

/// The sleeper: twenty short sleeps, a sample at each wake.
fn sleeper(index: usize) {
    super::sleep_for(10_000 * MS);
    for _ in 0..40 {
        super::sleep_for(700 * US);
        sample(index);
    }
}

fn expect(got: Result<(), Refused>, want: Refused, what: &'static str) -> Result<(), &'static str> {
    if got == Err(want) { Ok(()) } else { Err(what) }
}

/// Verifies: L.sched.157, L.sched.158, L.sched.159
fn refusals(cpus: usize) -> Result<(), &'static str> {
    let w = |partition, offset_ns, length_ns| Window {
        partition,
        offset_ns,
        length_ns,
    };
    let plan = &SCRATCH;
    plan.clear();
    windows::set_window(plan, 0, w(0, 0, MS)).map_err(|_| "windows: a first window was refused")?;
    expect(
        windows::set_window(plan, 0, w(1, MS / 2, MS)),
        Refused::Frame(Refusal::Overlap),
        "windows: an overlapping window was accepted",
    )?;
    expect(
        windows::set_window(plan, 0, w(8, 2 * MS, MS)),
        Refused::Frame(Refusal::Partition),
        "windows: a window of no partition was accepted",
    )?;
    windows::set_window(plan, 0, w(1, MS, 9 * MS))
        .map_err(|_| "windows: a second window was refused")?;
    expect(
        windows::check_seal(plan, 9 * MS),
        Refused::Frame(Refusal::PastMajor),
        "windows: a window past the major frame was sealed",
    )?;
    if cpus >= 2 {
        // Processor 1 is in service and has no partition-0 window.
        expect(
            windows::check_seal(plan, MAJOR),
            Refused::Frame(Refusal::NoPartitionZero),
            "windows: a processor with no partition-0 window was sealed",
        )?;
    }
    if cpus < 64 {
        let stray = &SCRATCH;
        stray.clear();
        for cpu in 0..cpus {
            windows::set_window(stray, cpu, w(0, 0, MS))
                .map_err(|_| "windows: a window was refused")?;
        }
        windows::set_window(stray, cpus, w(0, 0, MS))
            .map_err(|_| "windows: a window was refused")?;
        expect(
            windows::check_seal(stray, MAJOR),
            Refused::Frame(Refusal::OutOfService),
            "windows: a window on a processor out of service was sealed",
        )?;
    }
    Ok(())
}

/// Verifies: L.sched.162
fn jobs() -> Result<(), &'static str> {
    let parent = job::root().new_child().map_err(|_| "windows: no job")?;
    let child = parent.new_child().map_err(|_| "windows: no job")?;
    expect(
        windows::assign(&parent, 2),
        Refused::Job,
        "windows: a job with a child took a partition",
    )?;
    expect(
        windows::assign(&child, 2),
        Refused::Job,
        "windows: a job not the root's child took a partition",
    )?;
    let free = job::root().new_child().map_err(|_| "windows: no job")?;
    expect(
        windows::assign(&free, 0),
        Refused::Frame(Refusal::Partition),
        "windows: a job took partition 0",
    )?;
    windows::assign(&free, 2)
        .map_err(|_| "windows: an empty child of the root took no partition")?;
    let grandchild = free.new_child().map_err(|_| "windows: no job")?;
    if grandchild.partition() != 2 {
        return Err("windows: a job's child is not in its partition");
    }
    Ok(())
}

/// The check's frame on processor `held`: partition 0 everywhere else, and on `held` 0, 2, 3
/// and a gap.
fn frame_on(held: usize, cpus: usize) -> Result<(), &'static str> {
    let w = |partition, offset_ns, length_ns| Window {
        partition,
        offset_ns,
        length_ns,
    };
    for cpu in (0..cpus).filter(|&cpu| super::in_service(cpu)) {
        let rows: &[Window] = if cpu == held {
            &[w(0, 0, MS), w(2, MS, 2 * MS), w(3, 3 * MS, 3 * MS)]
        } else {
            &[w(0, 0, MAJOR)]
        };
        for window in rows {
            windows::set_window(&CHECK, cpu, *window).map_err(|why| why.says())?;
        }
    }
    let epoch = crate::timer::now_nanos() + LEAD;
    EPOCH.store(epoch, Ordering::Release);
    windows::install_check(&CHECK, held, MAJOR, epoch).map_err(|why| why.says())
}

fn asleep(held: usize) -> Result<[Arc<Task>; THREADS], &'static str> {
    let made: [Option<Arc<Task>>; THREADS] = core::array::from_fn(|index| {
        let entry: fn(usize) = if index == THREADS - 1 {
            sleeper
        } else {
            spinner
        };
        let weight = ferrix_sched::weight_of_nice(NICE.get(index).copied().unwrap_or(0))?;
        spawn_on(
            NAMES.get(index).copied().unwrap_or("win"),
            entry,
            index,
            weight,
            held,
            CpuSet::of(held),
        )
        .ok()
    });
    let mut tasks: [Option<Arc<Task>>; THREADS] = made;
    let deadline = crate::timer::now_nanos() + 2_000 * MS;
    for (index, task) in tasks.iter().enumerate() {
        let task = task.as_ref().ok_or("windows: no thread")?;
        let partition = PARTITION.get(index).copied().unwrap_or(0);
        while windows::rehome_for_check(task, partition).is_err() {
            if crate::timer::now_nanos() > deadline {
                return Err("windows: a check thread never slept");
            }
            super::sleep_for(MS);
        }
    }
    let mut out: [Option<Arc<Task>>; THREADS] = [const { None }; THREADS];
    for (slot, task) in out.iter_mut().zip(tasks.iter_mut()) {
        *slot = task.take();
    }
    let [Some(a), Some(c), Some(d), Some(e), Some(f)] = out else {
        return Err("windows: no thread");
    };
    Ok([a, c, d, e, f])
}

/// What one look at the held processor's queue found: whether it was in a gap with work of a
/// partition queued, and whether it then said it had work (WN.4).
fn look(held: usize) -> (bool, bool) {
    let Some(lock) = queue_of(held) else {
        return (false, false);
    };
    let saved = <crate::arch::Irq as ferrix_sync::IrqControl>::disable();
    let queue = lock.lock();
    // At this look's instant, as every reader outside a pick (WT.1): the held processor's class
    // time is its last pick's.
    let now = crate::timer::now_nanos();
    let gap = !queue.fair.runnable_at(now) && queue.fair.queued() > 0;
    // `has_work` reads the clock again, later: it counts as busy only where the gap still holds
    // after it, so that a window opening between the two reads is not taken for work in a gap.
    let busy = gap
        && queue.has_work()
        && !queue.fair.runnable_at(crate::timer::now_nanos())
        && queue.fair.current().is_none();
    // L.sched.166: what steal, balance and push may take (`latest_where`, the body
    // `steal_candidate` asks) is only a task of the partition whose window is open.
    if queue.fair.latest_where_at(now, |_| true).is_some() {
        let _ = OFFERED.fetch_add(1, Ordering::Relaxed);
    }
    if queue
        .fair
        .latest_where_at(now, |task| {
            !queue
                .fair
                .admits_at(task.partition.load(Ordering::Relaxed), now)
        })
        .is_some()
    {
        let _ = OFFERED_ACROSS.fetch_add(1, Ordering::Relaxed);
    }
    drop(queue);
    <crate::arch::Irq as ferrix_sync::IrqControl>::restore(saved);
    (gap, busy)
}

/// Looks at the held processor that found a task steal could take, and one of a partition
/// whose window was not open (L.sched.166: never).
static OFFERED: AtomicU64 = AtomicU64::new(0);
static OFFERED_ACROSS: AtomicU64 = AtomicU64::new(0);

/// Set to end [`parked_peer`], and what it waits on: a wait with no deadline, so the peer holds
/// its sleep slot as a parked task does (`direct::begin`'s T11).
#[cfg(ferrix_fast_ipc)]
static STOP_H: AtomicBool = AtomicBool::new(false);
#[cfg(ferrix_fast_ipc)]
static PEER: super::WaitQueue = super::WaitQueue::new();

/// The hand-over case's peer: blocked on the check's processor, in partition 2 (L.sched.165).
#[cfg(ferrix_fast_ipc)]
fn parked_peer(_index: usize) {
    let _ = PEER.wait_trusting(|| STOP_H.load(Ordering::Acquire));
}

/// L.sched.165: a hand-over from the check's own thread (partition 0) to a peer asleep on the
/// same processor in partition 2 is declined for its partitions, by `direct::begin`'s body.
/// Tried until the partitions' test is reached, since the tests before it (the peer's home,
/// nothing else waiting) may decline first; never accepted. Built only with the hand-over
/// (`ipc_fastpath=on` on x86-64): without it there is no hand-over to decline.
#[cfg(ferrix_fast_ipc)]
fn hand_over_declines() -> Result<&'static str, &'static str> {
    let me = super::current().ok_or("windows: the check has no task")?;
    let cpu = me.cpu();
    let peer = spawn_on(
        "win-h",
        parked_peer,
        0,
        ferrix_sched::NICE_0_WEIGHT,
        cpu,
        CpuSet::of(cpu),
    )
    .map_err(|_| "windows: no thread")?;
    let deadline = crate::timer::now_nanos() + 2_000 * MS;
    while windows::rehome_for_check(&peer, 2).is_err() {
        if crate::timer::now_nanos() > deadline {
            return Err("windows: a check thread never slept");
        }
        super::sleep_for(MS);
    }
    let before = windows::declined();
    let mut outcome =
        Err("windows: hand_over across partitions never reached its partitions' test");
    for _ in 0..200 {
        let saved = <crate::arch::Irq as ferrix_sync::IrqControl>::disable();
        let tried = super::direct::begin(&me, &peer);
        let accepted = tried.is_ok();
        drop(tried);
        <crate::arch::Irq as ferrix_sync::IrqControl>::restore(saved);
        if accepted {
            outcome = Err("windows: hand_over across partitions was accepted");
            break;
        }
        if windows::declined() != before {
            outcome = Ok("a hand-over across partitions declined");
            break;
        }
        super::sleep_for(MS);
    }
    STOP_H.store(true, Ordering::Release);
    PEER.wake_all();
    wait_until_gone(&peer, 2_000 * MS).map_err(|_| "windows: a check thread did not end")?;
    outcome
}

fn load(at: &[AtomicU64; THREADS], index: usize) -> u64 {
    at.get(index)
        .map_or(0, |count| count.load(Ordering::Acquire))
}

/// The figures behind a failure of the order cases (never ran, a lower level first, equals not
/// in turn): each thread's first and last stamp from the epoch and its count inside its window,
/// beside the held processor's picks more than a frame late, so that a held vCPU's stall (the
/// host's, BACKLOG's TCG row) is told from a pick that broke the order (ledger 10087 A6 (c)).
/// It prints; it judges nothing.
fn print_threads(judged: &Judged) {
    let epoch = EPOCH.load(Ordering::Acquire);
    for index in 0..THREADS {
        // An unset first stamp (`u64::MAX`) prints as 0.
        let first = Some(load(&FIRST, index))
            .filter(|&at| at != u64::MAX)
            .map_or(0, |at| at.saturating_sub(epoch));
        let last = load(&LAST, index);
        crate::println!(
            "  windows  {}: first stamp {} ns, last {} ns from the epoch, {} inside its window",
            NAMES.get(index).copied().unwrap_or("?"),
            first,
            last.saturating_sub(epoch),
            load(&INSIDE, index)
        );
    }
    crate::println!(
        "  windows  {} window ends logged, {} picks more than a frame late (worst {} ns), worst lateness {} ns",
        judged.windows,
        judged.frame_late,
        judged.frame_late_worst,
        judged.late_worst
    );
}

/// The check, before the boot's seal. Its line.
///
/// # Errors
///
/// The sentence of the first rule broken.
///
/// Verifies: H.SCHED.26, L.sched.153, L.sched.154, L.sched.155, L.sched.156, L.sched.163, L.sched.164, L.sched.165, L.sched.166, L.sched.167, L.sched.201
pub fn run() -> Result<alloc::string::String, &'static str> {
    let cpus = crate::smp::count();
    refusals(cpus)?;
    jobs()?;
    if !core::any::type_name::<super::queue::Tier<Arc<Task>>>().contains("Windows") {
        return Err("windows: the class is not the windows class");
    }
    let Some(held) = (1..cpus).rev().find(|&cpu| super::in_service(cpu)) else {
        return Ok(alloc::format!(
            "{} cases of the frame's refusals and a job's partition held; one processor: no window run",
            9
        ));
    };
    // L.sched.163: a kernel thread, and so a task with no job, is in partition 0: the check's own,
    // and every thread it makes before it rehomes one.
    if super::current().map(|me| me.partition.load(Ordering::Relaxed)) != Some(0) {
        return Err("windows: a kernel thread is not in partition 0");
    }
    #[cfg(ferrix_fast_ipc)]
    let hand_over = hand_over_declines()?;
    #[cfg(not(ferrix_fast_ipc))]
    let hand_over = "no hand-over built (ipc_fastpath off)";
    // Asleep in their partitions before the frame, which then begins in its lead (see `LEAD`).
    let threads = asleep(held)?;
    let woken = spawn_on(
        "win-k",
        woken_in,
        0,
        ferrix_sched::NICE_0_WEIGHT,
        held,
        CpuSet::of(held),
    )
    .map_err(|_| "windows: no thread")?;
    if woken.partition.load(Ordering::Relaxed) != 0 {
        return Err("windows: a kernel thread is not in partition 0");
    }
    let deadline = crate::timer::now_nanos() + 2_000 * MS;
    while windows::rehome_for_check(&woken, 2).is_err() {
        if crate::timer::now_nanos() > deadline {
            return Err("windows: a check thread never slept");
        }
        super::sleep_for(MS);
    }
    frame_on(held, cpus)?;
    HELD.store(held, Ordering::Release);
    let unkicked = windows::unkicked();
    for task in &threads {
        super::wake(task);
    }
    if crate::timer::now_nanos() >= EPOCH.load(Ordering::Acquire) {
        return Err("windows: the check's threads were woken after its frame began");
    }
    let mut gaps = 0_u64;
    let until = EPOCH.load(Ordering::Acquire) + RUN * 3;
    while threads.iter().any(|task| !task.is_dead()) && crate::timer::now_nanos() < until {
        let (gap, busy) = look(held);
        if busy {
            return Err("windows: a processor in a gap read another partition's task as work");
        }
        gaps += u64::from(gap);
        super::sleep_for(200 * US);
    }
    for task in &threads {
        wait_until_gone(task, 2_000 * MS).map_err(|_| "windows: a check thread did not end")?;
    }
    // The main case judged as it ends (ledger 7663 follow-up): its arms and its samples, before
    // the kick cases, so that a fault planted in the pick or the arm fails by its own sentence
    // rather than by a kick case's that it also breaks.
    judge_arms(ARMS.load(Ordering::Acquire))?;
    let judged = judge_samples()?;
    // The other half (WX.6): with the spinners gone and the held processor idle, a wake of a
    // partition-2 thread inside partition 2's window kicks it. Tried in up to 20 frames, since a
    // look at the phase and the wake are not one instant; a wake that missed the window is the
    // unkicked kind and is not counted here.
    let kicked = windows::kicked();
    let epoch = EPOCH.load(Ordering::Acquire);
    for _ in 0..20 {
        if windows::kicked() != kicked {
            break;
        }
        let at = crate::timer::now_nanos();
        let phase = at.saturating_sub(epoch) % MAJOR;
        let middle = MS + MS / 2;
        super::sleep_for((middle + MAJOR - phase) % MAJOR);
        super::wake(&woken);
        super::sleep_for(MAJOR);
    }
    // WN.4: a wake of the partition-2 thread in partition 0's window, on the held processor with
    // nothing of partition 0 to run, sends no kick (the processor stays halted) and the thread
    // first runs at partition 2's window's start in that frame, its delay printed. Tried in up to
    // `TRIES` frames, since a look at the phase and the wake are not one instant.
    let mut halted = None;
    for _ in 0..TRIES {
        let phase = crate::timer::now_nanos().saturating_sub(epoch) % MAJOR;
        super::sleep_for((3 * MS / 10 + MAJOR - phase) % MAJOR);
        let at = crate::timer::now_nanos();
        let phase = at.saturating_sub(epoch) % MAJOR;
        // Aimed at 0.3 ms; a look that woke late is used up to 0.2 ms before the window opens.
        if phase >= MS - 200 * US {
            continue;
        }
        let window_start = at - phase + MS;
        WOKEN_AT.store(0, Ordering::Release);
        let before = windows::unkicked();
        let kicked_before = windows::kicked();
        super::wake(&woken);
        // WU.4: the attempt counts only when the wake's own stamp, not the look's, lies inside
        // [0, 1) ms of the frame, partition 0's window.
        let woke = crate::timer::now_nanos();
        if woke >= window_start || woke < window_start - MS {
            continue;
        }
        // WZ.7 (b): until partition 2's window opens, the held processor's window-aware
        // `has_work` reads false with the partition-2 task queued. The idle loop halts exactly
        // when `has_work` reads false (`sched::idle`'s look, masked), so these reads are WN.4's
        // "no spin" by that path; one that read true fails by name.
        let mut quiet = 0_u64;
        let mut looks = 0_u64;
        while crate::timer::now_nanos().saturating_add(100 * US) < window_start {
            looks += 1;
            let (gap, busy) = look(held);
            if busy {
                // WS.1: its figures; no stamp yet, so the wake's and the look's.
                crate::println!(
                    "  windows  WN.4 attempt: woken at {} ns, window from {} ns, look {looks} at {} ns",
                    woke.saturating_sub(epoch),
                    window_start.saturating_sub(epoch),
                    crate::timer::now_nanos().saturating_sub(epoch)
                );
                return Err(
                    "windows: in partition 0's window the held processor read the partition-2 task as work",
                );
            }
            quiet += u64::from(gap);
            super::sleep_for(50 * US);
        }
        // It runs at its window's start; under TCG that pick may come tens of ms late (WX.4), so
        // the wait for its stamp is 20 frames, past which it never ran.
        let until = crate::timer::now_nanos() + 20 * MAJOR;
        super::sleep_for(MAJOR);
        while WOKEN_AT.load(Ordering::Acquire) == 0 && crate::timer::now_nanos() < until {
            super::sleep_for(MS);
        }
        let ran = WOKEN_AT.load(Ordering::Acquire);
        // WR.4 (ledger 7648): a wake in the last 100 us before the window leaves no time to look,
        // so it is not an attempt; its stamp was waited for above, so the next starts clean.
        if looks == 0 {
            continue;
        }
        // WS.1 (ledger 7645): every failure of this attempt prints its figures first.
        let end = window_start + 2 * MS;
        let fail = |why: &'static str| {
            print_attempt(ran, window_start, end, epoch, (woke, looks, quiet));
            Err(why)
        };
        if ran == 0 {
            return fail("windows: a task woken in partition 0's window never ran");
        }
        // Never before its window: the halt held.
        if ran < window_start {
            return fail("windows: a task woken in partition 0's window ran before its own");
        }
        // Past its window's end: on x86-64, whose timer is honest, never (WU.3); on AArch64 by
        // ledger 7645 WS.2, which corrects 7614 WU.3's "in its frame".
        if ran >= end {
            if crate::arch::WINDOW_TIMER_HONEST {
                return fail("windows: a task woken in partition 0's window ran outside its own");
            }
            if let Err(why) = late_stamp(ran, window_start, epoch) {
                return fail(why);
            }
        }
        // Two sentences (ledger 7639 WT.4): a kick decided and sent, or no kick decided at all,
        // neither the filter's suppression nor a kick (a wake that needed none, or one placed
        // elsewhere with no note).
        if windows::unkicked() == before {
            if windows::kicked() != kicked_before {
                return fail("windows: a wake in partition 0's window kicked the halted processor");
            }
            return fail(
                "windows: a wake in partition 0's window decided no kick, suppressed or sent",
            );
        }
        if quiet == 0 {
            return fail(
                "windows: has_work was never read false in partition 0's window with the partition-2 task queued",
            );
        }
        halted = Some((ran - window_start, quiet));
        break;
    }
    // WZ.7 (a): an attempt whose wake landed inside [0, 1) ms of the frame is required: one of
    // `TRIES`, since the check's own sleep may wake late on a loaded host (seen at load 36, KVM).
    let Some((halted, quiet)) = halted else {
        return Err("windows: no wake landed in partition 0's window in 50 frames");
    };
    STOP_K.store(true, Ordering::Release);
    super::wake(&woken);
    wait_until_gone(&woken, 2_000 * MS).map_err(|_| "windows: a check thread did not end")?;
    let kicked = windows::kicked() - kicked;
    if kicked == 0 {
        return Err("windows: no wake into the open window's partition kicked its processor");
    }
    let (worst, _) = windows::overruns();
    HELD.store(usize::MAX, Ordering::Release);
    windows::uninstall_check(held).map_err(|why| why.says())?;
    let unkicked = windows::unkicked() - unkicked;
    let arms = ARMS.load(Ordering::Acquire);
    judge_arms(arms)?;
    if arms == 0 {
        return Err("windows: no arm on the held processor was judged");
    }
    let floored = FLOORED.load(Ordering::Acquire);
    if judged.windows == 0 {
        return Err("windows: no window's end was judged");
    }
    for index in 0..THREADS {
        if load(&INSIDE, index) == 0 {
            print_threads(&judged);
            return Err("windows: a task never ran in its window");
        }
    }
    // A (index 0) above C (1): C's first stamp after A's last.
    if load(&FIRST, 1) < load(&LAST, 0) {
        print_threads(&judged);
        return Err("windows: a lower level ran while a higher one was runnable");
    }
    // D (2) and E (3) equals: each ran before the other ended.
    if load(&FIRST, 2) >= load(&LAST, 3) || load(&FIRST, 3) >= load(&LAST, 2) {
        print_threads(&judged);
        return Err("windows: equals did not take turns");
    }
    if gaps == 0 {
        return Err("windows: no look found the held processor in a gap with work queued");
    }
    if unkicked == 0 {
        return Err("windows: no wake into another partition's window was left unkicked");
    }
    if OFFERED_ACROSS.load(Ordering::Acquire) != 0 {
        return Err("windows: steal could take a task of a partition whose window was not open");
    }
    let offered = OFFERED.load(Ordering::Acquire);
    if offered == 0 {
        return Err("windows: no look found a task steal could take");
    }
    // The judged figure is the time run past the end, the last sample outside less the end
    // (WH.1), with the pick's distance beside it; a pick late past the bound after a last sample
    // within it is a stall, counted (WH.2); the start delay of an end with nothing running is
    // counted, never judged (WJ.2).
    let (over, end, first, last, late, pick_past) = judged.worst;
    let (delay, delay_at) = judged.start_delay;
    let (stalls, (stall, stall_at)) = (judged.stalls, judged.stall_worst);
    let late_worst = judged.late_worst;
    let (frame_late, frame_late_worst) = (judged.frame_late, judged.frame_late_worst);
    // x86-64 judges the worst overrun where its timer is honest (WX.4), over the windows a task
    // ran past (WJ.1: an overrun is time run past the window, H.SCHED.26); AArch64 prints it.
    if crate::arch::WINDOW_TIMER_HONEST && over > SLACK {
        crate::println!(
            "  windows  worst run {over} ns past the end at {end} ns (outside {first}..{last}, pick {pick_past} ns past, lateness {late} ns), bound {SLACK} ns; {stalls} stalls after the window's end (worst pick {stall} ns past at {stall_at}); worst start delay {delay} ns at {delay_at} (processor idle at the end)"
        );
        return Err("windows: a window's end was overrun past its bound");
    }
    let bound = if crate::arch::WINDOW_TIMER_HONEST {
        "within 500 us"
    } else {
        "overrun not judged on AArch64 (no honest timer under TCG; BACKLOG `schedule=windows`' 500 us overrun bound on AArch64)"
    };
    Ok(alloc::format!(
        "two partitions' windows of 2 and 3 ms on processor {held}: each ran only in its own and \
         past its end only until the pick that ended it ({} windows judged, {} with samples \
         outside), the gap idled ({gaps} looks), a higher level first, equals in turn, \
         {unkicked} wakes across unkicked, {kicked} in kicked, one in partition 0's window run \
         {halted} ns after its window's start (has_work read false {quiet} times before it), \
         {arms} arms asked the window's end or a quantum's, {floored} of them armed at the 20 us \
         floor, their lateness up to 20 us, {hand_over}, {offered} looks offered steal only the \
         open window's partition; worst run {over} ns past the end at {end} ns (outside {first}..{last}, pick {pick_past} ns past, lateness {late} ns), \
         {stalls} stalls after the window's end (worst pick {stall} ns past at {stall_at}, counted, not judged), \
         worst start delay {delay} ns at {delay_at} (processor idle at the end, counted, not judged), \
         worst lateness {late_worst} ns, {frame_late} picks more than a frame late (worst {frame_late_worst} ns), {bound}; worst overrun {worst} ns over all processors; {} \
         fair-class cases left out",
        judged.windows,
        judged.outside,
        super::check::FAIR_ONLY.len()
    ))
}

/// After the boot's seal: every refusal a sealed frame owes (L.sched.160).
///
/// # Errors
///
/// The sentence of the first refusal not made.
///
/// Verifies: H.SCHED.27, L.sched.160, L.sched.170
pub fn after_seal() -> Result<(), &'static str> {
    let w = Window {
        partition: 1,
        offset_ns: 0,
        length_ns: MS,
    };
    if !windows::every_row_installed() {
        return Err("windows: a processor reads the merged set after the seal");
    }
    // A processor coming back takes its sealed row (L.sched.170): each in service holds one, by
    // the body `put_back_in_service` asks.
    for cpu in (0..crate::smp::count()).filter(|&cpu| super::in_service(cpu)) {
        if !windows::may_return(cpu) {
            return Err("windows: a processor with its sealed row may not come back");
        }
    }
    expect(
        windows::set_window_boot(0, w),
        Refused::Sealed,
        "windows: a window after the seal was accepted",
    )?;
    expect(
        windows::seal(MAJOR),
        Refused::Sealed,
        "windows: a second seal was accepted",
    )?;
    expect(
        windows::install_check(&CHECK, 0, MAJOR, 0),
        Refused::Sealed,
        "windows: a check's frame was installed after the seal",
    )?;
    expect(
        windows::uninstall_check(0),
        Refused::Sealed,
        "windows: a frame was taken off after the seal",
    )?;
    let job = job::root().new_child().map_err(|_| "windows: no job")?;
    expect(
        windows::assign(&job, 2),
        Refused::Sealed,
        "windows: a job took a partition the sealed frame does not name",
    )?;
    Ok(())
}
