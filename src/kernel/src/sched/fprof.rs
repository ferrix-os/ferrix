//! PROFILE ONLY, never for landing (po8, 2026-10-06): the fast path's own
//! spans on one processor. A direction's ten spans are kept until its last
//! stamp, then filed whole: as a domain trip when its switch (span 6) took
//! under 4096 ticks, otherwise as a cross-domain trip (bench-ipc's `call`,
//! whose switch issues IBPB). A direction any other stamp broke is dropped.
//! Printed as the shell exits.

#![allow(clippy::all, clippy::pedantic, unused_unsafe)]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

pub(crate) const N: usize = 10;
const NAMES: [&str; N] = [
    "ring 3 + SYSRET + SYSCALL + stub (since last exit)",
    "fast entry: IN_CALL, T2 filter_quiet",
    "T3, T4/T5 handle lookup",
    "send_direct: half locks, sendable (T6-T10)",
    "direct::begin: run-queue lock, T11-T13",
    "fill_reply, hand_over, unlocks, count",
    "switch_chosen: install (CR3), user state",
    "switch_to + finish_switch (now the peer)",
    "peer: back up to take_reply",
    "peer: frame_tail, write, exit",
];
const WIDTH: u64 = 4;
const BUCKETS: usize = 4096;
const CLASSES: usize = 2;
const CLASS_NAMES: [&str; CLASSES] = ["domain trips (no barrier decided)", "cross-domain trips (a barrier decided)"];

static LAST: AtomicU64 = AtomicU64::new(0);
/// Barrier decisions on processor 0 when the direction reached point 5.
static DECIDED: AtomicU64 = AtomicU64::new(0);
static LAST_POINT: AtomicU64 = AtomicU64::new(u64::MAX);
static CUR: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static MASK: AtomicU64 = AtomicU64::new(0);
static DIRS: [AtomicU64; CLASSES] = [const { AtomicU64::new(0) }; CLASSES];
static DROPPED: AtomicU64 = AtomicU64::new(0);
static SUM: [[AtomicU64; N]; CLASSES] = [const { [const { AtomicU64::new(0) }; N] }; CLASSES];
static HIST: [[[AtomicU32; BUCKETS]; N]; CLASSES] =
    [const { [const { [const { AtomicU32::new(0) }; BUCKETS] }; N] }; CLASSES];
static T0: AtomicU64 = AtomicU64::new(0);
static NS0: AtomicU64 = AtomicU64::new(0);

fn tsc() -> u64 {
    // SAFETY: PROFILE ONLY: RDTSC has no preconditions.
    unsafe { core::arch::x86_64::_rdtsc() }
}

fn add(a: &AtomicU64, v: u64) {
    a.store(a.load(Relaxed).wrapping_add(v), Relaxed);
}

/// Stamp point `i`.
#[inline(always)]
pub(crate) fn stamp(i: usize) {
    let now = tsc();
    let expected = ((i + N - 1) % N) as u64;
    if LAST_POINT.load(Relaxed) == expected {
        CUR[i].store(now.wrapping_sub(LAST.load(Relaxed)), Relaxed);
        let mask = if i == 0 { 1 } else { MASK.load(Relaxed) | 1 << i };
        MASK.store(mask, Relaxed);
        if i == N - 1 {
            if mask == (1 << N) - 1 {
                let class = usize::from(
                    crate::arch::barrier_decisions_on(0) != DECIDED.load(Relaxed),
                );
                add(&DIRS[class], 1);
                for k in 0..N {
                    let d = CUR[k].load(Relaxed);
                    add(&SUM[class][k], d);
                    let b = &HIST[class][k][((d / WIDTH) as usize).min(BUCKETS - 1)];
                    b.store(b.load(Relaxed) + 1, Relaxed);
                }
            } else {
                add(&DROPPED, 1);
            }
        }
    } else {
        MASK.store(0, Relaxed);
    }
    if i == 5 {
        DECIDED.store(crate::arch::barrier_decisions_on(0), Relaxed);
    }
    LAST_POINT.store(i as u64, Relaxed);
    LAST.store(tsc(), Relaxed);
}

/// Start counting afresh.
pub(crate) fn reset() {
    for c in 0..CLASSES {
        DIRS[c].store(0, Relaxed);
        for k in 0..N {
            SUM[c][k].store(0, Relaxed);
            for b in &HIST[c][k] {
                b.store(0, Relaxed);
            }
        }
    }
    DROPPED.store(0, Relaxed);
    MASK.store(0, Relaxed);
    LAST_POINT.store(u64::MAX, Relaxed);
    T0.store(tsc(), Relaxed);
    NS0.store(crate::timer::now_nanos(), Relaxed);
}

/// Print each class's spans: p50 and mean, in ns.
pub(crate) fn print() {
    let ticks = tsc().wrapping_sub(T0.load(Relaxed)).max(1);
    let nanos = crate::timer::now_nanos().wrapping_sub(NS0.load(Relaxed)).max(1);
    let per_us = (ticks.saturating_mul(1000) / nanos).max(1);
    let ns = |t: u64| t * 1000 / per_us;
    let a = tsc();
    let b = tsc();
    crate::console::println!(
        "  fprof    tsc {} ticks/us, rdtsc pair {} ticks, {} directions dropped",
        per_us,
        b.wrapping_sub(a),
        DROPPED.load(Relaxed)
    );
    for c in 0..CLASSES {
        let n = DIRS[c].load(Relaxed);
        crate::console::println!("  fprof    {}: {} directions", CLASS_NAMES[c], n);
        let (mut p50s, mut means) = (0, 0);
        for k in 0..N {
            let mut seen = 0;
            let mut p50 = 0;
            for (j, bucket) in HIST[c][k].iter().enumerate() {
                seen += u64::from(bucket.load(Relaxed));
                if n > 0 && seen * 2 >= n {
                    p50 = j as u64 * WIDTH + WIDTH / 2;
                    break;
                }
            }
            let mean = SUM[c][k].load(Relaxed) / n.max(1);
            p50s += p50;
            means += mean;
            crate::console::println!(
                "  fprof {} {:>2} {:<50} p50 {:>5} ns  mean {:>5} ns",
                c,
                k,
                NAMES[k],
                ns(p50),
                ns(mean)
            );
        }
        crate::console::println!(
            "  fprof {}    a direction: sum of p50s {} ns, sum of means {} ns",
            c,
            ns(p50s),
            ns(means)
        );
    }
}
