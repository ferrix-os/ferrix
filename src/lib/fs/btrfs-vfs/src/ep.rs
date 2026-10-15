//! ep-rig scratch counters, never land: where a writable mount's time goes.
#![allow(missing_docs, clippy::missing_docs_in_private_items)]

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub static C: [AtomicU64; 40] = [const { AtomicU64::new(0) }; 40];
pub const NAMES: [&str; 40] = [
    "wr_n", "wr_ns", "sync_n", "sync_ns", "slices_ns", "wb_ns", "commit_ns", "lockwait_ns",
    "lock_n", "fill_n", "fill_ns", "fill_pg", "devw_n", "devw_b", "devw_ns", "devr_n", "devr_b",
    "devr_ns", "devf_n", "devf_ns", "sysw_n", "sysw_ns", "sysr_n", "sysr_ns", "recl_n",
    "recl_ns", "rd_n", "rd_ns", "fill_lock_ns", "wbslice_lockheld_ns", "commit_lockheld_ns",
    "maybe_commit_ns", "fsync_n", "fsync_ns", "x34", "x35", "x36", "x37", "x38", "last_report",
];
pub const WR_N: usize = 0;
pub const WR_NS: usize = 1;
pub const SYNC_N: usize = 2;
pub const SYNC_NS: usize = 3;
pub const SLICES_NS: usize = 4;
pub const WB_NS: usize = 5;
pub const COMMIT_NS: usize = 6;
pub const LOCKWAIT_NS: usize = 7;
pub const LOCK_N: usize = 8;
pub const FILL_N: usize = 9;
pub const FILL_NS: usize = 10;
pub const FILL_PG: usize = 11;
pub const DEVW_N: usize = 12;
pub const DEVW_B: usize = 13;
pub const DEVW_NS: usize = 14;
pub const DEVR_N: usize = 15;
pub const DEVR_B: usize = 16;
pub const DEVR_NS: usize = 17;
pub const DEVF_N: usize = 18;
pub const DEVF_NS: usize = 19;
pub const SYSW_N: usize = 20;
pub const SYSW_NS: usize = 21;
pub const SYSR_N: usize = 22;
pub const SYSR_NS: usize = 23;
pub const RECL_N: usize = 24;
pub const RECL_NS: usize = 25;
pub const RD_N: usize = 26;
pub const RD_NS: usize = 27;
pub const FILL_LOCK_NS: usize = 28;
pub const MAYBE_COMMIT_NS: usize = 31;
pub const FSYNC_N: usize = 32;
pub const FSYNC_NS: usize = 33;
const LAST: usize = 39;

pub fn add(index: usize, value: u64) {
    if let Some(counter) = C.get(index) {
        let _ = counter.fetch_add(value, Relaxed);
    }
}

/// Every counter on one line, at most once per ten seconds of `now`.
pub fn report(now: u64, out: &mut dyn FnMut(core::fmt::Arguments<'_>)) {
    let last = C[LAST].load(Relaxed);
    if now.saturating_sub(last) < 10_000_000_000
        || C[LAST].compare_exchange(last, now, Relaxed, Relaxed).is_err()
    {
        return;
    }
    let mut line = alloc::string::String::new();
    for (name, counter) in NAMES.iter().zip(C.iter()).take(34) {
        let _ = core::fmt::Write::write_fmt(
            &mut line,
            format_args!(" {name}={}", counter.load(Relaxed)),
        );
    }
    out(format_args!("ep: k t={now}{line}"));
}
