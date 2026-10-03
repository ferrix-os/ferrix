//! Time: RM's clocks and delays, from `clock_gettime` and
//! `clock_nanosleep` (`docs/NVIDIA.md` §4.1).

use crate::libc::{self, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME, Timespec};
use crate::status::{NvStatus, OK};

/// Nanoseconds in a second.
pub(crate) const SECOND: u64 = 1_000_000_000;

/// Below this a delay spins rather than sleeps: a sleep's own cost.
const SPIN_BELOW_NS: u64 = 20_000;

/// A clock, in nanoseconds; 0 if it cannot be read.
fn clock(which: core::ffi::c_int) -> u64 {
    let mut now = Timespec::default();
    // SAFETY: `now` is writable.
    if unsafe { libc::clock_gettime(which, &raw mut now) } != 0 {
        return 0;
    }
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanoseconds = u64::try_from(now.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(SECOND).saturating_add(nanoseconds)
}

/// The monotonic clock, in nanoseconds since boot.
pub(crate) fn monotonic() -> u64 {
    clock(CLOCK_MONOTONIC)
}

/// A `Timespec` of `nanoseconds`.
pub(crate) fn timespec(nanoseconds: u64) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(nanoseconds / SECOND).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from(nanoseconds % SECOND).unwrap_or(0),
    }
}

/// Wait `nanoseconds`: spin when short, sleep otherwise.
pub(crate) fn delay(nanoseconds: u64) {
    if nanoseconds < SPIN_BELOW_NS {
        let until = monotonic().saturating_add(nanoseconds);
        while monotonic() < until {
            core::hint::spin_loop();
        }
        return;
    }
    let until = timespec(monotonic().saturating_add(nanoseconds));
    // TIMER_ABSTIME, so that a signal's early return just sleeps again.
    // SAFETY: `until` is a live `Timespec`; no remainder is asked for.
    while unsafe {
        libc::clock_nanosleep(CLOCK_MONOTONIC, 1, &raw const until, core::ptr::null_mut())
    } != 0
    {}
}

/// `os_get_system_time`: the wall clock, as seconds and microseconds.
///
/// # Safety
///
/// Both pointers are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn os_get_system_time(seconds: *mut u32, microseconds: *mut u32) -> NvStatus {
    let now = clock(CLOCK_REALTIME);
    // SAFETY: the caller vouches for both pointers. RM's 32-bit seconds
    // wrap in 2106, as Linux's do here.
    unsafe {
        seconds.write((now / SECOND) as u32);
    }
    // SAFETY: as above.
    unsafe {
        microseconds.write(((now % SECOND) / 1000) as u32);
    }
    OK
}

/// `os_get_monotonic_time_ns`.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_monotonic_time_ns() -> u64 {
    monotonic()
}

/// `os_get_monotonic_time_ns_hr`: the raw monotonic clock, as Linux's
/// `ktime_get_raw_ts64`.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_monotonic_time_ns_hr() -> u64 {
    clock(CLOCK_MONOTONIC_RAW)
}

/// `os_get_monotonic_tick_resolution_ns`: the monotonic clock's resolution.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_monotonic_tick_resolution_ns() -> u64 {
    let mut resolution = Timespec::default();
    // SAFETY: `resolution` is writable.
    if unsafe { libc::clock_getres(CLOCK_MONOTONIC, &raw mut resolution) } != 0 {
        return 1;
    }
    u64::try_from(resolution.tv_nsec).unwrap_or(1).max(1)
}

/// `os_delay_us`.
#[unsafe(no_mangle)]
pub extern "C" fn os_delay_us(microseconds: u32) -> NvStatus {
    delay(u64::from(microseconds) * 1000);
    OK
}

/// `os_delay`, in milliseconds.
#[unsafe(no_mangle)]
pub extern "C" fn os_delay(milliseconds: u32) -> NvStatus {
    delay(u64::from(milliseconds) * 1_000_000);
    OK
}

/// `os_get_cpu_frequency`: the time stamp counter's rate, counted over
/// 50 ms, as Linux counts it over 250 ms where it has no cpufreq.
#[unsafe(no_mangle)]
pub extern "C" fn os_get_cpu_frequency() -> u64 {
    // SAFETY: `rdtsc` only reads a counter. Were CR4.TSD set it would
    // fault rather than misbehave; Linux and Ferrix leave it clear.
    let before = unsafe { core::arch::x86_64::_rdtsc() };
    let start = monotonic();
    delay(50_000_000);
    // SAFETY: as above.
    let after = unsafe { core::arch::x86_64::_rdtsc() };
    let elapsed = monotonic().saturating_sub(start).max(1);
    after.saturating_sub(before).saturating_mul(SECOND) / elapsed
}
