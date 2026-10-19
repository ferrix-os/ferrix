//! The `cpu` controller's files: `cpu.max`, `cpu.weight.nice` and `cpu.stat`.
//!
//! Parsed and printed as Linux's `kernel/sched/core.c` does (`cpu_max_write`,
//! `cpu_period_quota_parse`, `tg_set_cfs_bandwidth`, `cpu_weight_nice_read_s64`
//! and `cpu_extra_stat_show`), checked against a 7.0 host's files.

use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::Refusal;
use crate::write::{WEIGHT_DEFAULT, WEIGHT_MAX, WEIGHT_MIN, strip};

/// `cpu.max`'s default period, in microseconds: `default_cfs_period()`.
pub const PERIOD_DEFAULT_US: u64 = 100_000;
/// The least quota or period Linux takes, `min_cfs_quota_period`: 1 ms.
const MIN_NS: u64 = 1_000_000;
/// The greatest period, `max_cfs_quota_period`: 1 s.
const MAX_PERIOD_NS: u64 = 1_000_000_000;
/// The greatest quota, `max_cfs_runtime`: `MAX_BW` microseconds, 2^44 - 1.
const MAX_QUOTA_NS: u64 = ((1 << 44) - 1) * 1000;

/// What `cpu.max` holds: a quota per period, both in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Max {
    /// The runtime a cgroup's subtree may use each period, or `None` for
    /// `max`.
    pub quota: Option<u64>,
    /// The period.
    pub period: u64,
}

impl Max {
    /// No limit, in the default period: what a new cgroup holds.
    pub const DEFAULT: Max = Max {
        quota: None,
        period: PERIOD_DEFAULT_US,
    };
}

/// The decimal digits that start `text` as `sscanf("%llu")` reads them, and
/// whether there were any. A number too large is the largest `u64`.
fn leading_number(text: &[u8]) -> Option<u64> {
    let digits = text.iter().take_while(|byte| byte.is_ascii_digit());
    let mut any = false;
    let mut value: u64 = 0;
    for &byte in digits {
        any = true;
        value = value
            .saturating_mul(10)
            .saturating_add(u64::from(byte - b'0'));
    }
    any.then_some(value)
}

/// Parse a write to `cpu.max`, as `cpu_max_write` takes it: a quota, in
/// microseconds or `max`, and optionally a period, in microseconds. A period
/// left out, or one that is not a number, keeps `current`'s.
///
/// # Errors
///
/// [`Refusal::Invalid`] for a first word that is neither a number nor
/// `max`, for a quota or a period under 1 ms, a period over 1 s, or a
/// quota past what the scheduler can count.
pub fn parse_max(text: &[u8], current_period: u64) -> Result<Max, Refusal> {
    let text = strip(text);
    let mut words = text
        .split(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .filter(|word| !word.is_empty());
    let first = words.next().ok_or(Refusal::Invalid)?;
    // `%20s`: the word, cut at twenty bytes.
    let first = first.get(..20).unwrap_or(first);
    let period = words
        .next()
        .and_then(leading_number)
        .unwrap_or(current_period);
    // Microseconds to nanoseconds as the kernel multiplies them: modulo 2^64.
    let period_ns = period.wrapping_mul(1000);
    let quota = match leading_number(first) {
        Some(quota) => Some(quota.wrapping_mul(1000)),
        None if first == b"max" => None,
        None => return Err(Refusal::Invalid),
    };
    if quota.is_some_and(|quota| !(MIN_NS..=MAX_QUOTA_NS).contains(&quota))
        || !(MIN_NS..=MAX_PERIOD_NS).contains(&period_ns)
    {
        return Err(Refusal::Invalid);
    }
    Ok(Max {
        quota: quota.map(|quota| quota / 1000),
        period: period_ns / 1000,
    })
}

/// Append `cpu.max`: `max 100000` or `50000 100000`.
pub fn render_max(out: &mut Vec<u8>, max: Max) {
    let _ = match max.quota {
        None => writeln!(Out(out), "max {}", max.period),
        Some(quota) => writeln!(Out(out), "{quota} {}", max.period),
    };
}

/// A `Vec<u8>` that `write!` can format into.
struct Out<'a>(&'a mut Vec<u8>);

impl core::fmt::Write for Out<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

/// Linux's `sched_prio_to_weight`: the weight of nice -20 to 19.
const PRIO_TO_WEIGHT: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904,
    3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87,
    70, 56, 45, 36, 29, 23, 18, 15,
];

/// The least nice value.
pub const NICE_MIN: i32 = -20;
/// The greatest nice value.
pub const NICE_MAX: i32 = 19;

/// `sched_weight_from_cgroup`: `cpu.weight` to the scheduler's weight.
fn weight_from_cgroup(weight: u64) -> u64 {
    (weight * 1024 + 50) / 100
}

/// `sched_weight_to_cgroup`: the scheduler's weight to `cpu.weight`.
fn weight_to_cgroup(weight: u32) -> u32 {
    let scaled = (u64::from(weight) * WEIGHT_DEFAULT + 512) / 1024;
    u32::try_from(scaled.clamp(WEIGHT_MIN, WEIGHT_MAX)).unwrap_or(100)
}

/// The nice value `cpu.weight.nice` reads for a `cpu.weight` of `weight`: the
/// one whose weight is nearest, as `cpu_weight_nice_read_s64` searches.
pub fn nice_from_weight(weight: u32) -> i32 {
    let want = i64::try_from(weight_from_cgroup(u64::from(weight))).unwrap_or(i64::MAX);
    let mut last = i64::MAX;
    let mut prio = 0;
    for (at, &candidate) in PRIO_TO_WEIGHT.iter().enumerate() {
        let delta = (i64::from(candidate) - want).abs();
        if delta >= last {
            break;
        }
        last = delta;
        prio = at + 1;
    }
    NICE_MIN + i32::try_from(prio).unwrap_or(1) - 1
}

/// Parse a write to `cpu.weight.nice`, as `cpu_weight_nice_write_s64` takes
/// it, and answer the `cpu.weight` it sets.
///
/// # Errors
///
/// What `kstrtoll` refuses, and [`Refusal::Range`] outside -20 to 19.
pub fn parse_nice(text: &[u8]) -> Result<u32, Refusal> {
    let nice = crate::write::kstrtoint(strip(text))?;
    if !(NICE_MIN..=NICE_MAX).contains(&nice) {
        return Err(Refusal::Range);
    }
    let at = usize::try_from(nice - NICE_MIN).map_err(|_| Refusal::Range)?;
    let weight = PRIO_TO_WEIGHT.get(at).copied().ok_or(Refusal::Range)?;
    Ok(weight_to_cgroup(weight))
}

/// What `cpu.stat` prints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
    /// Microseconds of processor time its tasks used, in user and kernel
    /// mode.
    pub usage: u64,
    /// Of those, in user mode.
    pub user: u64,
    /// Of those, in kernel mode.
    pub system: u64,
    /// Periods that have run under a `cpu.max`.
    pub periods: u64,
    /// Periods in which it used its whole quota and was throttled.
    pub throttled: u64,
    /// Microseconds it spent throttled.
    pub throttled_us: u64,
}

/// Append `cpu.stat`, the keys Ferrix has a source for in Linux's order.
/// Every cgroup has the first three; the period and throttle counts are
/// printed only where the `cpu` controller is enabled, as Linux prints them.
pub fn render_stat(out: &mut Vec<u8>, stat: Stat, controlled: bool) {
    let _ = write!(
        Out(out),
        "usage_usec {}\nuser_usec {}\nsystem_usec {}\n",
        stat.usage,
        stat.user,
        stat.system
    );
    if controlled {
        let _ = write!(
            Out(out),
            "nr_periods {}\nnr_throttled {}\nthrottled_usec {}\n",
            stat.periods,
            stat.throttled,
            stat.throttled_us
        );
    }
}
