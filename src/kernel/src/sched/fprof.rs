//! PROFILE ONLY, never for landing (po8, 2026-10-06): the fast path's own
//! spans on one processor. Each stamp records the time since the stamp
//! before it, but only when that stamp was its immediate predecessor in the
//! fast path's order, so a declined call or an interrupt between them that
//! stamps nothing cannot mix in. Printed as the shell exits.

#![allow(clippy::all, clippy::pedantic, unused_unsafe)]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// The points, in the order a direction passes them.
pub(crate) const N: usize = 10;
const NAMES: [&str; N] = [
    "ring 3 + SYSRET + SYSCALL + stub (from the last exit)",
    "fast entry: IN_CALL, T2 filter_quiet",
    "T3, T4/T5 handle lookup",
    "send_direct: two half locks, sendable (T6-T10)",
    "direct::begin: run-queue lock, T11-T13",
    "fill_reply, hand_over, unlocks, count",
    "switch_chosen: install (CR3), user state",
    "switch_to + finish_switch (now the peer)",
    "peer: back up to take_reply",
    "peer: frame_tail, write, exit",
];
/// Histogram buckets of 4 ticks each, up to 4096 ticks.
const WIDTH: u64 = 4;
const BUCKETS: usize = 1024;

static LAST: AtomicU64 = AtomicU64::new(0);
static LAST_POINT: AtomicU64 = AtomicU64::new(u64::MAX);
static SUM: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static CNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static OVER: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static HIST: [[AtomicU32; BUCKETS]; N] = [const { [const { AtomicU32::new(0) }; BUCKETS] }; N];
static T0: AtomicU64 = AtomicU64::new(0);
static NS0: AtomicU64 = AtomicU64::new(0);

fn tsc() -> u64 {
    // SAFETY: PROFILE ONLY: RDTSC has no preconditions.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Stamp point `i`.
#[inline(always)]
pub(crate) fn stamp(i: usize) {
    let now = tsc();
    let before = LAST_POINT.load(Relaxed);
    let expected = (i + N - 1) % N;
    if before == expected as u64 {
        let d = now.wrapping_sub(LAST.load(Relaxed));
        if d < WIDTH * BUCKETS as u64 {
            SUM[i].store(SUM[i].load(Relaxed) + d, Relaxed);
            CNT[i].store(CNT[i].load(Relaxed) + 1, Relaxed);
            let b = &HIST[i][(d / WIDTH) as usize];
            b.store(b.load(Relaxed) + 1, Relaxed);
        } else {
            OVER[i].store(OVER[i].load(Relaxed) + 1, Relaxed);
        }
    }
    LAST_POINT.store(i as u64, Relaxed);
    LAST.store(tsc(), Relaxed);
}

/// Start counting afresh.
pub(crate) fn reset() {
    for i in 0..N {
        SUM[i].store(0, Relaxed);
        CNT[i].store(0, Relaxed);
        OVER[i].store(0, Relaxed);
        for b in &HIST[i] {
            b.store(0, Relaxed);
        }
    }
    LAST_POINT.store(u64::MAX, Relaxed);
    T0.store(tsc(), Relaxed);
    NS0.store(crate::timer::now_nanos(), Relaxed);
}

/// Print each span's p50 and trimmed mean.
pub(crate) fn print() {
    let ticks = tsc().wrapping_sub(T0.load(Relaxed)).max(1);
    let nanos = crate::timer::now_nanos().wrapping_sub(NS0.load(Relaxed)).max(1);
    // Ticks per microsecond, so the ns figures need no floats.
    let per_us = ticks.saturating_mul(1000) / nanos;
    // The stamp's own cost: two stamps back to back.
    let a = tsc();
    let b = tsc();
    let own = b.wrapping_sub(a);
    crate::console::println!(
        "  fprof    tsc {} ticks/us, rdtsc pair {} ticks; per span: p50, trimmed mean, samples, over 4096 ticks",
        per_us,
        own
    );
    let mut total_p50 = 0;
    for i in 0..N {
        let n = CNT[i].load(Relaxed);
        let mut seen = 0;
        let mut p50 = 0;
        for (k, bucket) in HIST[i].iter().enumerate() {
            seen += u64::from(bucket.load(Relaxed));
            if seen * 2 >= n && n > 0 {
                p50 = k as u64 * WIDTH + WIDTH / 2;
                break;
            }
        }
        let mean = SUM[i].load(Relaxed) / n.max(1);
        total_p50 += p50;
        crate::console::println!(
            "  fprof {:>2} {:<52} p50 {:>4} ns  mean {:>4} ns  n {:>6}  over {}",
            i,
            NAMES[i],
            p50 * 1000 / per_us.max(1),
            mean * 1000 / per_us.max(1),
            n,
            OVER[i].load(Relaxed)
        );
    }
    crate::console::println!(
        "  fprof    sum of p50s {} ns a direction",
        total_p50 * 1000 / per_us.max(1)
    );
}
