//! `timerfd_create`, `timerfd_settime` and `timerfd_gettime`: the calls around
//! [`crate::fs::timerfd`], and on ARMv7-A the `time64` forms of the last two.
//!
//! # Clocks
//!
//! `CLOCK_MONOTONIC`, `CLOCK_REALTIME` and `CLOCK_BOOTTIME`, as on Linux; any
//! other clock is `EINVAL`, as is a flag other than `TFD_CLOEXEC` and
//! `TFD_NONBLOCK`. `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM` are
//! `EPERM` without privilege, which is Linux's check, and `EOPNOTSUPP` with
//! it. That second answer is this kernel's and not Linux's `timerfd_create`,
//! which would make a timer that behaves as its base clock's and cannot wake a
//! suspended machine. It is the answer Linux's `timer_create` and
//! `clock_nanosleep` give for those clocks on a machine with no wake-capable
//! real-time clock, which is what this kernel is, and `sys_clock_nanosleep`
//! gives it already.
//!
//! # `struct itimerspec`
//!
//! Two `timespec`s, the interval first. Native words on each architecture:
//! 32 bytes on the 64-bit pair and 16 on ARMv7-A, whose `timerfd_settime64`
//! and `timerfd_gettime64` take the 32-byte `__kernel_itimerspec` instead.
//! A `timespec` with a negative second or a nanosecond outside 0..10^9 is
//! `EINVAL`, in either half.

use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_REALTIME, TFD_CLOEXEC, TFD_NONBLOCK, TFD_TIMER_ABSTIME,
    TFD_TIMER_CANCEL_ON_SET,
};

use crate::fs::timerfd::{self, Clock, SetFlags, Setting, TimerFd};
use crate::syscall::credentials;
use crate::syscall::fd;
use crate::syscall::process::Process;
use crate::syscall::time::{self, CLOCK_BOOTTIME_ALARM, CLOCK_REALTIME_ALARM, TimeWidth};

/// Nanoseconds in a second.
const NANOS: u64 = 1_000_000_000;

/// `timerfd_create`: a disarmed timer on `clock`.
///
/// # Errors
///
/// `EINVAL` for a flag other than `TFD_CLOEXEC` and `TFD_NONBLOCK` or a clock
/// a timerfd cannot count on; `EPERM` and `EOPNOTSUPP` for the alarm clocks
/// (see the module); `EMFILE` for a full table.
pub(crate) fn sys_timerfd_create(
    process: &Process,
    clock: i32,
    flags: u32,
) -> Result<usize, Errno> {
    if flags & !(TFD_CLOEXEC | TFD_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    let clock = match u32::try_from(clock) {
        Ok(CLOCK_MONOTONIC) => Clock::Monotonic,
        Ok(CLOCK_REALTIME) => Clock::Realtime,
        Ok(CLOCK_BOOTTIME) => Clock::Boottime,
        Ok(CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM) => {
            credentials::require_privilege(process)?;
            return Err(Errno::EOPNOTSUPP);
        }
        _ => return Err(Errno::EINVAL),
    };
    let file = timerfd::create(clock, flags & TFD_NONBLOCK != 0)?;
    let fd = process
        .files()
        .lock()
        .insert(file, flags & TFD_CLOEXEC != 0)?;
    usize::try_from(fd).map_err(|_| Errno::EMFILE)
}

/// `timerfd_settime` and `timerfd_settime64`: arm or disarm the timer `fd`
/// names, writing the setting it replaced to `old` when that is not null.
///
/// `do_timerfd_settime`'s order: the new setting is read first (`EFAULT`),
/// then the flags and the setting are checked (`EINVAL`), then the
/// descriptor (`EBADF`, and `EINVAL` for one that is not a timerfd). The old
/// setting is written after the timer is set, so a bad `old` is `EFAULT` with
/// the timer already armed, as on Linux.
pub(crate) fn sys_timerfd_settime(
    process: &Process,
    fd: i32,
    flags: u32,
    [new, old]: [u64; 2],
    width: TimeWidth,
) -> Result<usize, Errno> {
    let [interval, value] = read_itimerspec(process, new, width)?;
    if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
        return Err(Errno::EINVAL);
    }
    let mut setting = Setting {
        interval: time::nanos_of(interval.0, interval.1)?,
        value: time::nanos_of(value.0, value.1)?,
    };
    let timer = timer_of(process, fd)?;
    // An absolute time on a clock a time namespace shifts is given in the
    // caller's namespace: the timer keeps the host's (`docs/NAMESPACES.md`
    // §12.1). A result of zero would disarm it, and it means a time already
    // past, so it is one nanosecond.
    if flags & TFD_TIMER_ABSTIME != 0 && setting.value != 0 {
        let clock = match timer.clock() {
            Clock::Monotonic => CLOCK_MONOTONIC,
            Clock::Boottime => CLOCK_BOOTTIME,
            Clock::Realtime => CLOCK_REALTIME,
        };
        setting.value = time::host_from(process, clock, setting.value).max(1);
    }
    let flags = SetFlags {
        absolute: flags & TFD_TIMER_ABSTIME != 0,
        cancel_on_set: flags & TFD_TIMER_CANCEL_ON_SET != 0,
    };
    let replaced = timer.set(flags, setting)?;
    if old != 0 {
        write_itimerspec(process, old, replaced, width)?;
    }
    Ok(0)
}

/// `timerfd_gettime` and `timerfd_gettime64`: the time left to the next
/// expiration, zero when disarmed, and the interval.
pub(crate) fn sys_timerfd_gettime(
    process: &Process,
    fd: i32,
    at: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    let timer = timer_of(process, fd)?;
    write_itimerspec(process, at, timer.get(), width)?;
    Ok(0)
}

/// The timerfd `fd` names: `EBADF` for no descriptor, `EINVAL` for one that
/// is something else.
fn timer_of(process: &Process, fd: i32) -> Result<Arc<TimerFd>, Errno> {
    let file = fd::file(process, fd)?;
    timerfd::of(&file).ok_or(Errno::EINVAL)
}

/// Bytes in one `timespec` of the call's width.
fn timespec_bytes(width: TimeWidth) -> u64 {
    width.field_bytes() as u64 * 2
}

/// Read an `itimerspec`: the interval's and the value's second and
/// nanosecond, unchecked.
fn read_itimerspec(process: &Process, at: u64, width: TimeWidth) -> Result<[(i64, i64); 2], Errno> {
    let interval = time::read_pair(process, at, width)?;
    let value = time::read_pair(process, at.wrapping_add(timespec_bytes(width)), width)?;
    Ok([interval, value])
}

/// Write an `itimerspec` for `setting`.
fn write_itimerspec(
    process: &Process,
    at: u64,
    setting: Setting,
    width: TimeWidth,
) -> Result<(), Errno> {
    let pair = |nanos: u64| (nanos / NANOS, nanos % NANOS);
    let (seconds, nanos) = pair(setting.interval);
    time::write_pair(process, at, seconds, nanos, width)?;
    let (seconds, nanos) = pair(setting.value);
    time::write_pair(
        process,
        at.wrapping_add(timespec_bytes(width)),
        seconds,
        nanos,
        width,
    )
}
