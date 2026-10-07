//! PROFILE ONLY, never for landing (po9-obj, 2026-10-07): the fast path's
//! object side stamped at fine points on one processor. A span i-1 -> i is
//! counted only when the stamp before it was i-1 and it took under CAP ticks;
//! its mean is printed in ns, with span 0 -> 1 (two stamps back to back) the
//! stamp's own cost to subtract. Printed as the shell exits.

#![allow(clippy::all, clippy::pedantic, unused_unsafe, dead_code)]

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) const N: usize = 24;
const CAP: u64 = 4000;
const NAMES: [&str; N] = [
    "-",
    "calibration: stamp to stamp",
    "set_in_call_masked",
    "filter_quiet",
    "T3",
    "with_current; thread, process, core",
    "handles try_lock",
    "table.get, type, rights, Arc clone",
    "handles unlock",
    "reply_words",
    "first half try_lock",
    "second half try_lock",
    "sendable (T6-T10)",
    "direct::begin (T11-T13)",
    "parked.take, fill_reply",
    "hand_over",
    "drop peer_inbox",
    "drop own_inbox",
    "count(Trip)",
    "-- switch (peer resumes) --",
    "take_reply",
    "write_read_outcome",
    "closure exit (Arc<Endpoint> drop)",
    "IN_CALL check, return",
];

static LAST: AtomicU64 = AtomicU64::new(0);
static LAST_POINT: AtomicU64 = AtomicU64::new(u64::MAX);
static SUM: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static CNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
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
    if i > 0 && LAST_POINT.load(Relaxed) == (i - 1) as u64 {
        let d = now.wrapping_sub(LAST.load(Relaxed));
        if d < CAP {
            SUM[i].store(SUM[i].load(Relaxed) + d, Relaxed);
            CNT[i].store(CNT[i].load(Relaxed) + 1, Relaxed);
        }
    }
    LAST_POINT.store(i as u64, Relaxed);
    LAST.store(tsc(), Relaxed);
}

/// Start counting afresh.
pub(crate) fn reset() {
    for k in 0..N {
        SUM[k].store(0, Relaxed);
        CNT[k].store(0, Relaxed);
    }
    LAST_POINT.store(u64::MAX, Relaxed);
    T0.store(tsc(), Relaxed);
    NS0.store(crate::timer::now_nanos(), Relaxed);
}

/// Print each span's mean, in tenths of ns.
pub(crate) fn print() {
    let ticks = tsc().wrapping_sub(T0.load(Relaxed)).max(1);
    let nanos = crate::timer::now_nanos().wrapping_sub(NS0.load(Relaxed)).max(1);
    let per_us = (ticks.saturating_mul(1000) / nanos).max(1);
    let cal_n = CNT[1].load(Relaxed).max(1);
    let cal = SUM[1].load(Relaxed) * 10_000 / cal_n / per_us;
    crate::console::println!("  oprof    tsc {} ticks/us, stamp cost {}.{} ns", per_us, cal / 10, cal % 10);
    for k in 1..N {
        let n = CNT[k].load(Relaxed);
        let mean = SUM[k].load(Relaxed) * 10_000 / n.max(1) / per_us;
        let net = mean.saturating_sub(cal);
        crate::console::println!(
            "  oprof {:>2} {:<40} n {:>7} mean {:>4}.{} ns net {:>4}.{} ns",
            k,
            NAMES[k],
            n,
            mean / 10,
            mean % 10,
            net / 10,
            net % 10
        );
    }
}
