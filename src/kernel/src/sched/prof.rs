//! PROFILE, NOT FOR LANDING (po7-ipcM, `os-ipc/prof2` refreshed): where one
//! direction of a `channel_write_read` round trip spends its time.
//!
//! A direction runs from one side's entry into `channel_write_read` to the
//! other side's next entry into it. Every point along it stamps the TSC. Only
//! a direction whose stamps came in exactly the order of [`Point`], with no
//! other stamp between and no `IBPB` at its switch (a switch between two
//! domains), is counted: a timer interrupt that switched, the general trip,
//! the cross-domain `call` run and the warm-up's first sleeps all drop out.
//! So every span is of the same path, and the spans add up to the direction.
//!
//! One processor only (`--smp 1`): the state is plain statics.
//!
//! Printed when the built-in shell exits: per span its p50, p10, p90 and mean
//! in ticks and ns, the stamp's own cost measured at reset, and the
//! direction's whole.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// The points, in the order a direction passes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Point {
    /// user+sysret+syscall+stub
    Entry = 0,
    /// entry: filter+early
    EFilter,
    /// entry: vectors-dead mark
    EMark,
    /// entry: sti+regs copy
    ESti,
    /// system_call: call_entered
    ECallEntered,
    /// dispatch+native_call current()
    ECurrent,
    /// native_call: thread/process
    Dispatched,
    /// handle lookup
    LookedUp,
    /// write_small
    Written,
    /// wake: waiters lock+drain
    WDrained,
    /// wake: wake_onto (declined)
    WOnto,
    /// wake: home lock+insert
    WInserted,
    /// wake: kick/arm_timer+restore
    Woken,
    /// read_small empty
    ReadEmpty,
    /// wait: list+block mark
    Listed,
    /// schedule+rq lock
    Locked,
    /// choose: now_nanos
    ANow,
    /// choose: account
    AAccount,
    /// choose: wake_sleepers
    Accounted,
    /// detach+pick
    Picked,
    /// bookkeeping+arm_timer
    Booked,
    /// install: domain+join
    Domain,
    /// CR3 write
    Cr3,
    /// refill (barrier)
    Barrier,
    /// install rest
    Spaced,
    /// save_user_state
    Saved,
    /// restore: write_tls
    RTls,
    /// restore: ds/es/fs load
    RSel,
    /// restore: FS base wrmsr
    RFs,
    /// restore: gs load
    RGs,
    /// restore: GS base wrmsr
    RGsBase,
    /// restore: vector reset/xrstor
    RVec,
    /// restore: entry stack
    Restored,
    /// switch_to
    Switched,
    /// finish_switch
    Finished,
    /// unblock+unqueue
    Unblocked,
    /// read_small
    Read,
    /// record_call
    Recorded,
    /// regroup+call_left
    Left,
    /// exit: frame+attention
    Exit,
}

/// Ablation: reload `CR3` at every profiled exit, so the user span pays a
/// second refill of its translations.
pub(crate) const EXTRA_FLUSH: bool = option_env!("PO7_EXTRA_FLUSH").is_some();

/// How many points.
const POINTS: usize = 40;

/// Names for the print, in order.
const NAMES: [&str; POINTS] = [
    "user+sysret+syscall+stub",
    "entry: filter+early",
    "entry: vectors-dead mark",
    "entry: sti+regs copy",
    "system_call: call_entered",
    "dispatch+native_call current()",
    "native_call: thread/process",
    "handle lookup",
    "write_small",
    "wake: waiters lock+drain",
    "wake: wake_onto (declined)",
    "wake: home lock+insert",
    "wake: kick/arm_timer+restore",
    "read_small empty",
    "wait: list+block mark",
    "schedule+rq lock",
    "choose: now_nanos",
    "choose: account",
    "choose: wake_sleepers",
    "detach+pick",
    "bookkeeping+arm_timer",
    "install: domain+join",
    "CR3 write",
    "refill (barrier)",
    "install rest",
    "save_user_state",
    "restore: write_tls",
    "restore: ds/es/fs load",
    "restore: FS base wrmsr",
    "restore: gs load",
    "restore: GS base wrmsr",
    "restore: vector reset/xrstor",
    "restore: entry stack",
    "switch_to",
    "finish_switch",
    "unblock+unqueue",
    "read_small",
    "record_call",
    "regroup+call_left",
    "exit: frame+attention",
];

/// Histogram buckets of one tick, the last for everything above.
const BUCKETS: usize = 2048;

/// Per span: counts per tick.
static HIST: [[AtomicU32; BUCKETS]; POINTS] =
    [const { [const { AtomicU32::new(0) }; BUCKETS] }; POINTS];
/// Per span: the sum, for the mean.
static SUM: [AtomicU64; POINTS] = [const { AtomicU64::new(0) }; POINTS];
/// A whole direction, in buckets of four ticks.
static WHOLE: [AtomicU32; BUCKETS] = [const { AtomicU32::new(0) }; BUCKETS];

/// The direction being stamped: each span's ticks so far.
static OPEN: [AtomicU64; POINTS] = [const { AtomicU64::new(0) }; POINTS];
/// The next point expected, or [`BROKEN`].
static NEXT: AtomicUsize = AtomicUsize::new(BROKEN);
/// No direction open.
const BROKEN: usize = usize::MAX;
/// The last stamp's ticks.
static LAST: AtomicU64 = AtomicU64::new(0);
/// The open direction met an `IBPB`.
static TAINTED: AtomicBool = AtomicBool::new(false);
/// A direction ended at [`Point::Exit`] and waits for the next entry.
static ENDED: AtomicBool = AtomicBool::new(false);
/// The direction now open follows a commit.
static SKIP: AtomicBool = AtomicBool::new(false);
/// Counting at all.
static ON: AtomicBool = AtomicBool::new(false);
/// Directions counted, broken, tainted.
static COUNTED: AtomicU64 = AtomicU64::new(0);
/// Directions broken.
static BROKE: AtomicU64 = AtomicU64::new(0);
/// Directions tainted.
static TAINTS: AtomicU64 = AtomicU64::new(0);
/// The stamp's own cost, ticks, measured at reset.
static OVERHEAD: AtomicU64 = AtomicU64::new(0);
/// Where directions broke: the point that came instead, counted.
static BROKE_AT: [AtomicU64; POINTS] = [const { AtomicU64::new(0) }; POINTS];

/// The user span (point 0) again, by the caller's return address: up to
/// four of them, so the client's and the server's sides are told apart.
static BY_RIP: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Their histograms.
static RIP_HIST: [[AtomicU32; BUCKETS]; 4] = [const { [const { AtomicU32::new(0) }; BUCKETS] }; 4];
/// The return address of the open direction's entry.
static OPEN_RIP: AtomicU64 = AtomicU64::new(0);
/// The floor's user span: from a native call's exit to the next one's entry
/// of the same number, with no switch between.
static FLOOR_LAST: AtomicU64 = AtomicU64::new(0);
/// Its histogram.
static FLOOR_HIST: [AtomicU32; BUCKETS] = [const { AtomicU32::new(0) }; BUCKETS];

/// A non-sleeping native call's entry (`exit` false) or exit: the floor's
/// user span between.
#[inline(never)]
pub(crate) fn floor(exit: bool) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let now = tsc();
    if exit {
        FLOOR_LAST.store(now, Ordering::Relaxed);
        return;
    }
    let last = FLOOR_LAST.swap(0, Ordering::Relaxed);
    if last != 0 {
        let bucket = usize::try_from(now.wrapping_sub(last)).unwrap_or(BUCKETS - 1).min(BUCKETS - 1);
        if let Some(cell) = FLOOR_HIST.get(bucket) {
            let _ = cell.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A switch: no floor span crosses it.
pub(crate) fn switched() {
    FLOOR_LAST.store(0, Ordering::Relaxed);
}

/// The entry of the open direction came from `rip`.
pub(crate) fn entry_rip(rip: u64) {
    OPEN_RIP.store(rip, Ordering::Relaxed);
}

/// The TSC.
#[inline(always)]
fn tsc() -> u64 {
    // SAFETY: `rdtsc` reads a counter; no memory.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Stamp `point`.
#[inline(always)]
pub(crate) fn stamp(point: Point) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    stamp_at(point as usize, tsc());
}

/// [`stamp`], out of line.
#[inline(never)]
fn stamp_at(point: usize, now: u64) {
    let delta = now.wrapping_sub(LAST.load(Ordering::Relaxed));
    LAST.store(now, Ordering::Relaxed);
    if point == 0 {
        // A direction that follows a commit is not counted: the commit's
        // stores to the histograms are in its caches and its time.
        let after_commit = SKIP.swap(false, Ordering::Relaxed);
        if ENDED.load(Ordering::Relaxed) && NEXT.load(Ordering::Relaxed) == 0 && !after_commit {
            if let Some(slot) = OPEN.first() {
                slot.store(delta, Ordering::Relaxed);
            }
            commit();
            SKIP.store(true, Ordering::Relaxed);
            LAST.store(tsc(), Ordering::Relaxed);
        }
        ENDED.store(false, Ordering::Relaxed);
        TAINTED.store(false, Ordering::Relaxed);
        NEXT.store(1, Ordering::Relaxed);
        return;
    }
    let next = NEXT.load(Ordering::Relaxed);
    if next == BROKEN {
        return;
    }
    if next != point {
        let _ = BROKE.fetch_add(1, Ordering::Relaxed);
        if let Some(at) = BROKE_AT.get(point) {
            let _ = at.fetch_add(1, Ordering::Relaxed);
        }
        NEXT.store(BROKEN, Ordering::Relaxed);
        ENDED.store(false, Ordering::Relaxed);
        return;
    }
    if let Some(slot) = OPEN.get(point) {
        slot.store(delta, Ordering::Relaxed);
    }
    if point == POINTS - 1 {
        ENDED.store(true, Ordering::Relaxed);
        NEXT.store(0, Ordering::Relaxed);
    } else {
        NEXT.store(point + 1, Ordering::Relaxed);
    }
}

/// The open direction met an `IBPB`: not counted.
pub(crate) fn taint() {
    TAINTED.store(true, Ordering::Relaxed);
}

/// Count the direction just closed.
fn commit() {
    if TAINTED.load(Ordering::Relaxed) {
        let _ = TAINTS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut whole = 0_u64;
    for (i, slot) in OPEN.iter().enumerate() {
        let ticks = slot.load(Ordering::Relaxed);
        whole = whole.saturating_add(ticks);
        let bucket = usize::try_from(ticks).unwrap_or(BUCKETS - 1).min(BUCKETS - 1);
        if let Some(row) = HIST.get(i)
            && let Some(cell) = row.get(bucket)
        {
            let _ = cell.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(sum) = SUM.get(i) {
            let _ = sum.fetch_add(ticks, Ordering::Relaxed);
        }
    }
    let bucket = usize::try_from(whole / 4)
        .unwrap_or(BUCKETS - 1)
        .min(BUCKETS - 1);
    if let Some(cell) = WHOLE.get(bucket) {
        let _ = cell.fetch_add(1, Ordering::Relaxed);
    }
    let user = OPEN.first().map_or(0, |slot| slot.load(Ordering::Relaxed));
    let rip = OPEN_RIP.load(Ordering::Relaxed);
    for (i, slot) in BY_RIP.iter().enumerate() {
        let held = slot.load(Ordering::Relaxed);
        if held == 0 {
            slot.store(rip, Ordering::Relaxed);
        }
        if held == 0 || held == rip {
            let bucket = usize::try_from(user).unwrap_or(BUCKETS - 1).min(BUCKETS - 1);
            if let Some(cell) = RIP_HIST.get(i).and_then(|row| row.get(bucket)) {
                let _ = cell.fetch_add(1, Ordering::Relaxed);
            }
            break;
        }
    }
    let _ = COUNTED.fetch_add(1, Ordering::Relaxed);
}

/// Forget everything, measure the stamp's cost, and start counting.
pub(crate) fn reset() {
    ON.store(false, Ordering::Relaxed);
    for row in &HIST {
        for cell in row {
            cell.store(0, Ordering::Relaxed);
        }
    }
    for cell in &WHOLE {
        cell.store(0, Ordering::Relaxed);
    }
    for sum in SUM.iter().chain(BROKE_AT.iter()) {
        sum.store(0, Ordering::Relaxed);
    }
    COUNTED.store(0, Ordering::Relaxed);
    BROKE.store(0, Ordering::Relaxed);
    TAINTS.store(0, Ordering::Relaxed);
    // The stamp's cost: what one span gains from the stamp that ends it,
    // the p50 of two stamps back to back through the same code.
    // Sixteen stamps in order through the whole path, timed together.
    let mut seen = [0_u32; 256];
    for _ in 0..4096 {
        NEXT.store(1, Ordering::Relaxed);
        let start = tsc();
        for point in 1..=16 {
            stamp_at(point, tsc());
        }
        let ticks = tsc().wrapping_sub(start) / 16;
        let bucket = usize::try_from(ticks).unwrap_or(255).min(255);
        if let Some(cell) = seen.get_mut(bucket) {
            *cell += 1;
        }
    }
    OVERHEAD.store(percentile_of(&seen, 50), Ordering::Relaxed);
    NEXT.store(BROKEN, Ordering::Relaxed);
    ENDED.store(false, Ordering::Relaxed);
    ON.store(true, Ordering::Relaxed);
}

/// The `p`th percentile of a histogram of one-unit buckets.
fn percentile_of(cells: &[u32], p: u64) -> u64 {
    let total: u64 = cells.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return 0;
    }
    let want = (total * p).div_ceil(100).max(1);
    let mut seen = 0_u64;
    for (i, &c) in cells.iter().enumerate() {
        seen += u64::from(c);
        if seen >= want {
            return i as u64;
        }
    }
    cells.len() as u64
}

/// [`percentile_of`] for an atomic row.
fn percentile(row: &[AtomicU32], p: u64) -> u64 {
    let mut plain = [0_u32; BUCKETS];
    for (to, from) in plain.iter_mut().zip(row) {
        *to = from.load(Ordering::Relaxed);
    }
    percentile_of(&plain, p)
}

/// The mean of the samples at or below the `p`th percentile, in tenths of
/// a tick: the TSC moves in steps here, so a short span's p50 is a step,
/// and this is what the steps average to without the tail.
fn trimmed_mean10(row: &[AtomicU32], p: u64) -> u64 {
    let cut = percentile(row, p);
    let (mut sum, mut n) = (0_u64, 0_u64);
    for (i, cell) in row.iter().enumerate().take(usize::try_from(cut).unwrap_or(0) + 1) {
        let c = u64::from(cell.load(Ordering::Relaxed));
        sum += c * i as u64;
        n += c;
    }
    if n == 0 { 0 } else { sum * 10 / n }
}

/// Print every span.
pub(crate) fn print() {
    ON.store(false, Ordering::Relaxed);
    let hz = crate::arch::counter_hz().max(1);
    let ns = |ticks: u64| ticks * 1_000_000_000 / hz;
    let counted = COUNTED.load(Ordering::Relaxed).max(1);
    let overhead = OVERHEAD.load(Ordering::Relaxed);
    crate::console::println!(
        "  prof directions counted={} broken={} tainted={} stamp-overhead={} ticks ({} ns) \
         counter-hz={hz}",
        COUNTED.load(Ordering::Relaxed),
        BROKE.load(Ordering::Relaxed),
        TAINTS.load(Ordering::Relaxed),
        overhead,
        ns(overhead),
    );
    let mut sum_p50 = 0_u64;
    let mut sum_net = 0_u64;
    for (i, name) in NAMES.iter().enumerate() {
        let Some(row) = HIST.get(i) else { continue };
        let p50 = percentile(row, 50);
        let p10 = percentile(row, 10);
        let p90 = percentile(row, 90);
        let mean = SUM.get(i).map_or(0, |s| s.load(Ordering::Relaxed)) / counted;
        let tm10 = trimmed_mean10(row, 90);
        let net10 = tm10.saturating_sub(overhead * 10);
        let net = net10 / 10;
        sum_p50 += p50;
        sum_net += net10;
        crate::console::println!(
            "  prof span {i:>2} {name:<30} p50={p50:>5}t p10={p10:>5}t p90={p90:>5}t \
             mean={mean:>6}t tmean90={}.{}t net={}ns",
            tm10 / 10,
            tm10 % 10,
            ns(net),
        );
    }
    let whole50 = percentile(&WHOLE, 50) * 4;
    let whole10 = percentile(&WHOLE, 10) * 4;
    let whole90 = percentile(&WHOLE, 90) * 4;
    crate::console::println!(
        "  prof direction whole p50={whole50}t ({} ns) p10={whole10}t p90={whole90}t; sum of span \
         p50s={sum_p50}t ({} ns); sum of trimmed means net of stamps={}t ({} ns)",
        ns(whole50),
        ns(sum_p50),
        sum_net / 10,
        ns(sum_net / 10),
    );
    for (i, rip) in BY_RIP.iter().enumerate() {
        let rip = rip.load(Ordering::Relaxed);
        if let Some(row) = RIP_HIST.get(i).filter(|_| rip != 0) {
            let tm = trimmed_mean10(row, 90);
            crate::console::println!(
                "  prof user span of root {rip:#x}: p50={}t tmean90={}.{}t ({} ns net)",
                percentile(row, 50),
                tm / 10,
                tm % 10,
                ns(tm.saturating_sub(overhead * 10) / 10),
            );
        }
    }
    let tm = trimmed_mean10(&FLOOR_HIST, 90);
    crate::console::println!(
        "  prof floor user span (object_wait_one, no switch): p50={}t p10={}t tmean90={}.{}t ({} ns)",
        percentile(&FLOOR_HIST, 50),
        percentile(&FLOOR_HIST, 10),
        tm / 10,
        tm % 10,
        ns(tm / 10),
    );
    for (i, at) in BROKE_AT.iter().enumerate() {
        let n = at.load(Ordering::Relaxed);
        if n != 0 {
            crate::console::println!("  prof broke at point {i}: {n}");
        }
    }
}
