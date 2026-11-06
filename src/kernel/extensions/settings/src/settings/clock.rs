//! `clock_settime` and `settimeofday`: the wall clock, set by a program
//! (moved whole from `syscall/time.rs`; `docs/KERNEL-CRATES.md` §6.2). The
//! clock and `set_realtime` stay the load's, which also sets it at boot.

use ferrix_kernel_load::syscall::credentials;
use ferrix_kernel_load::syscall::process::Process;
use ferrix_kernel_load::syscall::time::{TimeWidth, nanos_of, read_pair, set_realtime};
use ferrix_kernel_load::syscall::uaccess;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::CLOCK_REALTIME;

/// The latest second the wall clock may be set to: `TIME_SETTOD_SEC_MAX`,
/// which is `KTIME_SEC_MAX` less thirty years of uptime, so that the clock
/// cannot be set to where adding the uptime would overflow.
const SETTOD_SECONDS_MAX: u64 = 9_223_372_036 - 30 * 365 * 24 * 3600;

/// `timespec64_valid_settod`: valid, and not past [`SETTOD_SECONDS_MAX`].
fn settable(seconds: i64, nanos: i64) -> Result<u64, Errno> {
    if u64::try_from(seconds).is_ok_and(|seconds| seconds >= SETTOD_SECONDS_MAX) {
        return Err(Errno::EINVAL);
    }
    nanos_of(seconds, nanos)
}

/// `clock_settime` and `clock_settime64`: only `CLOCK_REALTIME` may be set.
///
/// Every other clock is `EINVAL`, as Linux answers for a clock with no
/// `clock_set` -- the monotonic clocks are defined by never being set. The
/// clock is checked before the time is read, which is Linux's order.
pub fn sys_clock_settime(
    process: &Process,
    clock: i32,
    at: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    if u32::try_from(clock) != Ok(CLOCK_REALTIME) {
        return Err(Errno::EINVAL);
    }
    let (seconds, nanos) = read_pair(process, at, width)?;
    let target = settable(seconds, nanos)?;
    // `CAP_SYS_TIME`, after the time is found settable, as Linux orders it.
    credentials::require_privilege(process)?;
    set_realtime(target);
    ferrix_kernel_load::fs::timerfd::clock_was_set();
    Ok(0)
}

/// `settimeofday`: set the real-time clock in microseconds, and check the
/// timezone, which is accepted and not kept.
///
/// `kernel/time/time.c`'s order: the `timeval` is read and its microseconds
/// checked (up to and including 1 000 000, which the next check then
/// refuses), the timezone is read, the time is checked as a settable time,
/// the timezone's minutes-west checked to within fifteen hours, and only then
/// is anything set. Both pointers may be null. The `timeval` is `width`'s:
/// an i386 program's is two 32-bit fields.
pub fn sys_settimeofday(
    process: &Process,
    tv: u64,
    tz: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    let time = if tv == 0 {
        None
    } else {
        let (seconds, micros) = read_pair(process, tv, width)?;
        if !(0..=1_000_000).contains(&micros) {
            return Err(Errno::EINVAL);
        }
        Some((seconds, micros * 1_000))
    };
    let minutes_west = if tz == 0 {
        None
    } else {
        Some(uaccess::get_u32(process.space(), tz)? as i32)
    };
    let target = time
        .map(|(seconds, nanos)| settable(seconds, nanos))
        .transpose()?;
    if minutes_west.is_some_and(|minutes| !(-15 * 60..=15 * 60).contains(&minutes)) {
        return Err(Errno::EINVAL);
    }
    credentials::require_privilege(process)?;
    if let Some(target) = target {
        set_realtime(target);
        ferrix_kernel_load::fs::timerfd::clock_was_set();
    }
    Ok(0)
}
