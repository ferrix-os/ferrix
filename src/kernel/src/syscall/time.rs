//! Clocks, sleeping, and the bytes a libc asks for before `main`.
//!
//! Two groups of call that look unrelated and are here for the same reason:
//! a static binary reaches for both during startup, before it has printed
//! anything, and a missing answer to either does not fail loudly. The first
//! real program to run on Ferrix -- busybox's `sh` -- span forever on
//! `clock_gettime(CLOCK_MONOTONIC)` returning `ENOSYS`, waiting for time to
//! pass on a clock that could not say it had.

use core::sync::atomic::{AtomicI64, Ordering};

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW,
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_COARSE, CLOCK_THREAD_CPUTIME_ID,
};

use crate::arch;
use crate::sched;
use crate::sched::WaitQueue;
use crate::syscall::attributes::int;
use crate::syscall::credentials;
use crate::syscall::process::Process;
use crate::syscall::signal::RestartBlock;
use crate::syscall::thread::Thread;
use crate::syscall::timens::{self, Shift};
use crate::syscall::uaccess::{self, WORD};
use crate::trap::Abi;

/// Nanoseconds in a second.
const NANOS: u64 = 1_000_000_000;

/// `CLOCK_REALTIME_ALARM`, `CLOCK_BOOTTIME_ALARM` and `CLOCK_TAI`, from
/// `linux/time.h`: clocks Linux has that no call here sets. Of the three only
/// `CLOCK_TAI` can be read, in [`sys_clock_gettime`].
pub(crate) const CLOCK_REALTIME_ALARM: u32 = 8;
/// See [`CLOCK_REALTIME_ALARM`].
pub(crate) const CLOCK_BOOTTIME_ALARM: u32 = 9;
/// See [`CLOCK_REALTIME_ALARM`].
const CLOCK_TAI: u32 = 11;

/// How wide the two fields of a `timespec` or `timeval` are.
///
/// The reason `clock_gettime` and `clock_gettime64` are separate calls on
/// ARMv7-A: the first writes two `long`s, which are 32 bits there, and the
/// second writes two 64-bit fields whatever the architecture. On the 64-bit
/// pair both come out the same -- except to an i386 program, whose `long` is
/// 32 bits on this 64-bit kernel too: [`TimeWidth::in_abi`] gives the two
/// forms it passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeWidth {
    /// Two `long`s: this architecture's pointer width.
    Native,
    /// Two 64-bit fields.
    Wide,
    /// An i386 program's two `long`s: Linux's `old_timespec32` and
    /// `old_timeval32`, 32 bits each.
    Compat,
    /// An i386 program's `__kernel_timespec`: two 64-bit fields, of which
    /// `tv_nsec`'s upper half is padding, as for any 32-bit program.
    CompatWide,
}

impl TimeWidth {
    /// This width as a call that came in by `abi` passes it: an i386
    /// program's `long` is 32 bits.
    pub(crate) const fn in_abi(self, abi: Abi) -> TimeWidth {
        match (self, abi) {
            (TimeWidth::Native, Abi::Compat) => TimeWidth::Compat,
            (TimeWidth::Wide, Abi::Compat) => TimeWidth::CompatWide,
            (width, _) => width,
        }
    }

    /// Bytes in each of the two fields.
    pub(crate) const fn field_bytes(self) -> usize {
        match self {
            TimeWidth::Native => WORD,
            TimeWidth::Wide | TimeWidth::CompatWide => 8,
            TimeWidth::Compat => 4,
        }
    }

    /// Whether the caller's `long` is 32 bits, so that a 64-bit `tv_nsec`
    /// keeps only its low half, as `get_timespec64` keeps it for a 32-bit
    /// program: the rest is padding a libc need not write.
    pub(crate) const fn long_is_32(self) -> bool {
        match self {
            TimeWidth::Native | TimeWidth::Wide => WORD == 4,
            TimeWidth::Compat | TimeWidth::CompatWide => true,
        }
    }
}

/// Nanoseconds since the high-resolution counter started.
///
/// In 128 bits, and not as a courtesy. The counter runs at 100 MHz on the
/// x86-64 QEMU machine, so `counter * 10^9` overflows 64 bits after about three
/// minutes of uptime -- and with overflow checks on, that is a kernel panic
/// inside a system call three minutes into the first interactive session.
pub(crate) fn now_nanos() -> u64 {
    let hz = arch::counter_hz();
    if hz == 0 {
        return 0;
    }
    let nanos = u128::from(arch::counter_now()) * u128::from(NANOS) / u128::from(hz);
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

/// What `CLOCK_REALTIME` reads minus what the counter reads, in nanoseconds.
///
/// Zero until something sets the clock, so the wall clock starts at the epoch
/// at boot (see [`sys_clock_gettime`]). An offset rather than a stored time so
/// that the real-time clock goes on advancing with the counter after it is
/// set, which is the whole of what setting a clock means.
static REALTIME_OFFSET: AtomicI64 = AtomicI64::new(0);

/// `CLOCK_REALTIME`, in nanoseconds since the epoch.
pub(crate) fn realtime_nanos() -> u64 {
    let real = i128::from(now_nanos()) + i128::from(REALTIME_OFFSET.load(Ordering::Relaxed));
    u64::try_from(real.max(0)).unwrap_or(u64::MAX)
}

/// The real-time clock's offset from the counter, for a check that sets the
/// clock and has to put it back.
pub(crate) fn realtime_offset() -> i64 {
    REALTIME_OFFSET.load(Ordering::Relaxed)
}

/// Put back an offset [`realtime_offset`] reported.
pub(crate) fn restore_realtime_offset(offset: i64) {
    super::vdso::publish_realtime_offset(|| REALTIME_OFFSET.store(offset, Ordering::Relaxed));
}

/// Set `CLOCK_REALTIME` to `target` nanoseconds since the epoch.
/// Start `CLOCK_REALTIME` at the time firmware's clock gave the loader, and
/// return it in whole seconds; `None`, leaving the clock at the epoch, when
/// firmware had no clock.
pub(crate) fn set_boot_time(info: &ferrix_bootinfo::BootInfo) -> Option<u64> {
    if info.firmware_flags & ferrix_bootinfo::FIRMWARE_TIME == 0 {
        return None;
    }
    let nanos = u64::try_from(info.firmware_time).ok()?;
    set_realtime(nanos);
    Some(nanos / NANOS)
}

fn set_realtime(target: u64) {
    let offset = i128::from(target) - i128::from(now_nanos());
    let offset = i64::try_from(offset).unwrap_or(if offset < 0 { i64::MIN } else { i64::MAX });
    // With the vDSO's copy, which programs read the clock through.
    super::vdso::publish_realtime_offset(|| REALTIME_OFFSET.store(offset, Ordering::Relaxed));
}

/// Answer the sleeps and `restart_syscall`, or `None` for any other call.
///
/// Apart from [`dispatch`] because an interrupted sleep leaves its restart in
/// the calling thread, so these take the thread rather than its process; they
/// are reached through `super::signal::dispatch`.
pub(crate) fn sleep_dispatch(
    call: Syscall,
    a: &[u64; 6],
    thread: &Thread,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let native = TimeWidth::Native.in_abi(abi);
    let wide = TimeWidth::Wide.in_abi(abi);
    let answer = match call {
        Syscall::RestartSyscall => sys_restart_syscall(thread),
        Syscall::Nanosleep => sys_nanosleep_at_width(thread, a[0], a[1], native),
        Syscall::ClockNanosleep => {
            sys_clock_nanosleep(thread, int(a[0]), a[1], [a[2], a[3]], native)
        }
        Syscall::ClockNanosleepTime64 => {
            sys_clock_nanosleep(thread, int(a[0]), a[1], [a[2], a[3]], wide)
        }
        _ => return None,
    };
    Some(answer)
}

/// Answer `call` if it is one of the calls this module added to the table in
/// `mod.rs`. (`clock_gettime`, `gettimeofday` and `getrandom` are dispatched
/// there, where they were first.)
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let native = TimeWidth::Native.in_abi(abi);
    let wide = TimeWidth::Wide.in_abi(abi);
    let answer = match call {
        Syscall::ClockSettime => sys_clock_settime(process, int(a[0]), a[1], native),
        Syscall::ClockSettime64 => sys_clock_settime(process, int(a[0]), a[1], wide),
        Syscall::Settimeofday => sys_settimeofday(process, a[0], a[1], native),
        Syscall::Adjtimex => sys_adjtimex(process, a[0], native),
        Syscall::ClockAdjtime => sys_clock_adjtime(process, int(a[0]), a[1], native),
        Syscall::ClockAdjtime64 => sys_clock_adjtime(process, int(a[0]), a[1], wide),
        _ => return None,
    };
    Some(answer)
}

/// `clock_gettime` and `clock_gettime64`.
///
/// # Every clock is the counter, for now
///
/// The monotonic clocks are honestly the counter: they start at boot and never
/// go backwards, which is all they promise. The real-time clocks are the same
/// counter plus whatever offset `clock_settime` or `settimeofday` last set,
/// and so, until something sets them, read as a few seconds past the start of
/// 1970, because nothing yet reads a real-time clock chip. That is a wrong
/// answer rather than a missing one, and it is chosen deliberately: a program
/// that gets `EINVAL` for `CLOCK_REALTIME` usually aborts, while one that gets
/// 1970 usually prints a strange date and carries on. `CLOCK_TAI` is real time
/// plus the TAI offset, which is zero until something sets it, so it reads as
/// the real-time clock does. The CPU-time clocks are refused, because there is
/// no accounting of CPU time per process to report.
///
/// So are `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM`, and that is
/// Linux's answer rather than a gap: they read `EINVAL` there too on a machine
/// with no wake-capable real-time clock registered, and this kernel has none.
pub(crate) fn sys_clock_gettime(
    process: &Process,
    clock: u64,
    at: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    let clock = u32::try_from(clock).map_err(|_| Errno::EINVAL)?;
    let nanos = match clock {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_TAI => realtime_nanos(),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            shown_to(process, clock, now_nanos())
        }
        CLOCK_THREAD_CPUTIME_ID => sched::current_runtime(),
        CLOCK_PROCESS_CPUTIME_ID => process_runtime(process),
        _ => return Err(Errno::EINVAL),
    };
    write_pair(process, at, nanos / NANOS, nanos % NANOS, width)?;
    Ok(0)
}

/// `host` nanoseconds on the counter as `process` reads `clock`: shifted by
/// its time namespace's offset for the monotonic and boot-time clocks
/// (`docs/NAMESPACES.md` §12.1), as they are for every other clock.
pub(crate) fn shown_to(process: &Process, clock: u32, host: u64) -> u64 {
    match timens::shift_of(clock) {
        Some(which) => process.time_namespace().shown(which, host),
        None => host,
    }
}

/// An absolute time `process` gave on `clock`, as the counter reads it: the
/// other way from [`shown_to`].
pub(crate) fn host_from(process: &Process, clock: u32, given: u64) -> u64 {
    match timens::shift_of(clock) {
        Some(which) => process.time_namespace().host(which, given),
        None => given,
    }
}

/// `clock_getres` and `clock_getres_time64`: the resolution of a clock
/// `clock_gettime` reads, written through a non-null `at`.
///
/// A nanosecond for every one of them. Linux answers a tick for the
/// `_COARSE` clocks because it reads them from the last tick; here they read
/// the same counter as the others, so a nanosecond is what they have. The
/// clocks `clock_gettime` refuses are refused here too, with `EINVAL`, before
/// the buffer is touched. Chrome's headless shell asks before it draws.
pub(crate) fn sys_clock_getres(
    process: &Process,
    clock: u64,
    at: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    let clock = u32::try_from(clock).map_err(|_| Errno::EINVAL)?;
    match clock {
        CLOCK_REALTIME
        | CLOCK_REALTIME_COARSE
        | CLOCK_TAI
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_PROCESS_CPUTIME_ID => {}
        _ => return Err(Errno::EINVAL),
    }
    if at != 0 {
        write_pair(process, at, 0, 1, width)?;
    }
    Ok(0)
}

/// `CLOCK_PROCESS_CPUTIME_ID`: what the process's threads have run for.
///
/// The calling thread is charged up to now first; the others count up to
/// their own processors' last charge, which is at most a tick behind. Not as
/// Linux: a thread that has ended takes its time with it, where Linux keeps
/// it in the process's sum, so the clock can step back when one ends.
/// Chrome's `ThreadTicks` reads the thread clock, which is exact.
pub(crate) fn process_runtime(process: &Process) -> u64 {
    let _ = sched::current_runtime();
    process
        .tasks()
        .iter()
        .map(|task| task.runtime())
        .fold(0, u64::saturating_add)
}

/// `gettimeofday`: the real-time clock, in microseconds.
///
/// A null `tv` or `tz` is legal and asks for nothing. The timezone is obsolete,
/// but not ignored: Linux copies out its `struct timezone`, two `int`s that are
/// zero until `settimeofday` sets them, and a program that passes one reads it.
pub(crate) fn sys_gettimeofday(process: &Process, tv: u64, tz: u64) -> Result<usize, Errno> {
    sys_gettimeofday_at_width(process, tv, tz, TimeWidth::Native)
}

/// [`sys_gettimeofday`] into a `timeval` of `width`.
pub(crate) fn sys_gettimeofday_at_width(
    process: &Process,
    tv: u64,
    tz: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    if tv != 0 {
        let nanos = realtime_nanos();
        write_pair(process, tv, nanos / NANOS, (nanos % NANOS) / 1_000, width)?;
    }
    if tz != 0 {
        uaccess::copy_to_user(process.space(), tz, &[0_u8; 8]).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `time`: the real-time clock in whole seconds, as `gettimeofday` reads it.
///
/// The seconds are the return value and, through a non-null `tloc`, also
/// written as a native-word `time_t`; a write that faults is `EFAULT` rather
/// than the seconds, which is `kernel/time/time.c`'s order. Reachable only on
/// x86-64, the one table with a number for it.
pub(crate) fn sys_time(process: &Process, tloc: u64) -> Result<usize, Errno> {
    sys_time_at_width(process, tloc, WORD)
}

/// [`sys_time`] into a `time_t` of `word` bytes: an i386 program's is 32
/// bits, `old_time32_t`, which the answer is too.
pub(crate) fn sys_time_at_width(process: &Process, tloc: u64, word: usize) -> Result<usize, Errno> {
    let seconds = realtime_nanos() / NANOS;
    let seconds = if word == 4 {
        u64::from(u32::try_from(seconds).map_err(|_| Errno::EOVERFLOW)?)
    } else {
        seconds
    };
    let answer = usize::try_from(seconds).map_err(|_| Errno::EOVERFLOW)?;
    if tloc != 0 {
        let bytes = seconds.to_le_bytes();
        let slot = bytes.get(..word).ok_or(Errno::EINVAL)?;
        uaccess::copy_to_user(process.space(), tloc, slot).map_err(|_| Errno::EFAULT)?;
    }
    Ok(answer)
}

/// Write two fields, at the width the call's structure has.
pub(crate) fn write_pair(
    process: &Process,
    at: u64,
    first: u64,
    second: u64,
    width: TimeWidth,
) -> Result<(), Errno> {
    if width.field_bytes() == 8 {
        let mut bytes = [0_u8; 16];
        let fields = first.to_le_bytes().into_iter().chain(second.to_le_bytes());
        for (slot, byte) in bytes.iter_mut().zip(fields) {
            *slot = byte;
        }
        return uaccess::copy_to_user(process.space(), at, &bytes).map_err(|_| Errno::EFAULT);
    }
    // A 32-bit `time_t`. Seconds since boot fit for 136 years; the day they do
    // not is reported rather than wrapped.
    let first = u32::try_from(first).map_err(|_| Errno::EOVERFLOW)?;
    let second = u32::try_from(second).map_err(|_| Errno::EOVERFLOW)?;
    let mut bytes = [0_u8; 8];
    let fields = first.to_le_bytes().into_iter().chain(second.to_le_bytes());
    for (slot, byte) in bytes.iter_mut().zip(fields) {
        *slot = byte;
    }
    uaccess::copy_to_user(process.space(), at, &bytes).map_err(|_| Errno::EFAULT)
}

/// Read a `timespec` or `timeval`: two signed fields at the call's width.
///
/// The second field of a 64-bit `__kernel_timespec` read on a 32-bit build
/// keeps only its low 32 bits, because that is what `get_timespec64` does
/// there: the upper half is padding for a 32-bit program, and a libc may leave
/// anything in it.
pub(crate) fn read_pair(
    process: &crate::object::process::Process,
    at: u64,
    width: TimeWidth,
) -> Result<(i64, i64), Errno> {
    if width.field_bytes() == 8 {
        let mut bytes = [0_u8; 16];
        uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
        let (first, second) = bytes.split_at(8);
        let first = i64::from_le_bytes(first.try_into().map_err(|_| Errno::EFAULT)?);
        let second = i64::from_le_bytes(second.try_into().map_err(|_| Errno::EFAULT)?);
        let second = if width.long_is_32() {
            i64::from(second as i32)
        } else {
            second
        };
        return Ok((first, second));
    }
    let mut bytes = [0_u8; 8];
    uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let (first, second) = bytes.split_at(4);
    let first = i32::from_le_bytes(first.try_into().map_err(|_| Errno::EFAULT)?);
    let second = i32::from_le_bytes(second.try_into().map_err(|_| Errno::EFAULT)?);
    Ok((i64::from(first), i64::from(second)))
}

/// A `timespec` as nanoseconds, if it is valid: `timespec64_valid`, which
/// refuses a negative second and a nanosecond field outside 0..10^9.
pub(crate) fn nanos_of(seconds: i64, nanos: i64) -> Result<u64, Errno> {
    let seconds = u64::try_from(seconds).map_err(|_| Errno::EINVAL)?;
    let nanos = u64::try_from(nanos)
        .ok()
        .filter(|&nanos| nanos < NANOS)
        .ok_or(Errno::EINVAL)?;
    Ok(seconds.saturating_mul(NANOS).saturating_add(nanos))
}

/// Block until the counter reaches `deadline`, the process is ended, or a
/// signal is deliverable.
///
/// A kill wakes the task (see `process::kill`) and ends the sleep with `EINTR`,
/// its time left written to `rem` if the caller gave one, as `nanosleep`
/// promises. A catchable signal ends it too, but through the `restart_block`
/// codes rather than `EINTR`: `nanosleep` and `clock_nanosleep` are meant to
/// resume with the time left when no handler runs (`restart_syscall`, driven
/// from `deliver::return_to_user`), or to return `EINTR` when one does. The
/// absolute deadline is what a resume waits to, so the time left is exact
/// however many times it is interrupted.
///
/// The sleep is a wait on no queue, not `sched::sleep_until`: a wait marks
/// the task blocked before its last look at the kill and the signals, so a
/// kill or signal that lands between the look above and the block finds the
/// task blocked and wakes it. `sched::sleep_until` only blocked, and a wake
/// of a task not yet blocked does nothing, so a kill landing there was lost
/// and the sleep ran to its deadline: a `sleep 30` that `cgroup.kill` had
/// ended would keep its cgroup populated for the rest of the thirty seconds.
fn sleep_until(thread: &Thread, deadline: u64, rem: u64, width: TimeWidth) -> Result<usize, Errno> {
    let process = thread.process();
    loop {
        let now = crate::timer::now_nanos();
        if now >= deadline {
            return Ok(0);
        }
        if process.is_terminated() {
            if rem != 0 {
                let left = deadline.saturating_sub(now);
                write_pair(process, rem, left / NANOS, left % NANOS, width)?;
            }
            return Err(Errno::EINTR);
        }
        if thread.signal_pending() {
            // A catchable signal: report the time left and leave a
            // `restart_block`, so a no-handler resume waits out only that.
            if rem != 0 {
                let left = deadline.saturating_sub(now);
                write_pair(process, rem, left / NANOS, left % NANOS, width)?;
            }
            thread.with_own_signals(|signals| {
                signals.set_restart_block(RestartBlock {
                    deadline,
                    rem,
                    width,
                });
            });
            return Err(Errno::ERESTART_RESTARTBLOCK);
        }
        let _ = WaitQueue::wait_on_any(
            &[],
            || process.is_terminated() || thread.signal_pending(),
            deadline,
            u64::MAX,
        );
    }
}

/// `restart_syscall`: resume the sleep a `restart_block` left behind, waiting
/// out the time that was left rather than a fresh duration. The kernel points
/// an interrupted `nanosleep` or `clock_nanosleep` at this call when no handler
/// ran; no program issues it. With nothing to resume it is `EINTR`, as Linux's
/// `do_no_restart_syscall` answers.
pub(crate) fn sys_restart_syscall(thread: &Thread) -> Result<usize, Errno> {
    let Some(block) = thread.with_own_signals(super::signal::ThreadSignals::take_restart_block)
    else {
        return Err(Errno::EINTR);
    };
    sleep_until(thread, block.deadline, block.rem, block.width)
}

/// `nanosleep`: a relative sleep, in a native-width `timespec` (ARMv7-A has no
/// `time64` form of this call; its libc uses `clock_nanosleep_time64`).
pub(crate) fn sys_nanosleep(thread: &Thread, req: u64, rem: u64) -> Result<usize, Errno> {
    sys_nanosleep_at_width(thread, req, rem, TimeWidth::Native)
}

/// [`sys_nanosleep`] with both `timespec`s of `width`: an i386 program's
/// `nanosleep` takes two 32-bit fields, and musl calls it, not
/// `clock_nanosleep_time64`, for any request whose seconds fit.
pub(crate) fn sys_nanosleep_at_width(
    thread: &Thread,
    req: u64,
    rem: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    let (seconds, nanos) = read_pair(thread.process(), req, width)?;
    let length = nanos_of(seconds, nanos)?;
    let deadline = crate::timer::now_nanos().saturating_add(length);
    sleep_until(thread, deadline, rem, width)
}

/// `TIMER_ABSTIME`: the request is a time on the clock, not a duration.
const TIMER_ABSTIME: u64 = 1;

/// `clock_nanosleep` and `clock_nanosleep_time64`.
///
/// Against the three clocks there are: the monotonic ones, which are the
/// counter, and `CLOCK_REALTIME`, which is the counter plus its offset -- so
/// an absolute real-time deadline is turned into a counter deadline with the
/// offset as it is now. (A clock set during the sleep does not move the
/// wake-up, which on Linux it would; nothing sets the clock often enough for
/// that to matter yet.)
///
/// Linux's refusals, in its order: an unknown clock is `EINVAL`, a known one
/// that cannot be slept on is `EOPNOTSUPP` (`kernel/time/posix-timers.c` has
/// no `nsleep` for the raw and coarse clocks), and the request is read and
/// validated after that. The CPU-time clocks are `EINVAL`, as
/// `clock_gettime` answers them. An absolute sleep never writes `rem`.
pub(crate) fn sys_clock_nanosleep(
    thread: &Thread,
    clock: i32,
    flags: u64,
    [req, rem]: [u64; 2],
    width: TimeWidth,
) -> Result<usize, Errno> {
    let clock = u32::try_from(clock).map_err(|_| Errno::EINVAL)?;
    match clock {
        CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME => {}
        CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_REALTIME_ALARM
        | CLOCK_BOOTTIME_ALARM
        | CLOCK_TAI => {
            return Err(Errno::EOPNOTSUPP);
        }
        _ => return Err(Errno::EINVAL),
    }
    let (seconds, nanos) = read_pair(thread.process(), req, width)?;
    let requested = nanos_of(seconds, nanos)?;
    if flags & TIMER_ABSTIME == 0 {
        let deadline = crate::timer::now_nanos().saturating_add(requested);
        return sleep_until(thread, deadline, rem, width);
    }
    let deadline = if clock == CLOCK_REALTIME {
        let counter = i128::from(requested) - i128::from(realtime_offset());
        u64::try_from(counter.max(0)).unwrap_or(u64::MAX)
    } else {
        host_from(thread.process(), clock, requested)
    };
    sleep_until(thread, deadline, 0, width)
}

/// `USER_HZ`: the clock ticks per second `times` counts in, 100 on all three.
const USER_HZ: u64 = 100;

/// `times`: the process's CPU times, which are zero because nothing accounts
/// for them, and the clock ticks since boot as the return value.
///
/// `struct tms` is four `clock_t`s, which are `long`s: 32 bytes on the 64-bit
/// pair and 16 on ARMv7-A, by `sizeof` against `linux/times.h` compiled for
/// x86-64 and `arm-linux-gnueabihf`. A null buffer is legal and asks only for
/// the ticks. The ticks are what a shell's `time` subtracts, so they are real.
pub(crate) fn sys_times(process: &Process, at: u64, word: usize) -> Result<usize, Errno> {
    if at != 0 {
        let zeros = [0_u8; 32];
        let tms = zeros.get(..word * 4).ok_or(Errno::EFAULT)?;
        uaccess::copy_to_user(process.space(), at, tms).map_err(|_| Errno::EFAULT)?;
    }
    let boot = process.time_namespace().shown(Shift::Boottime, now_nanos());
    Ok((boot / (NANOS / USER_HZ)) as usize)
}

/// `getrusage`: zero usage, for a `who` Linux knows.
///
/// `struct rusage` is two `timeval`s and fourteen `long`s, eighteen native
/// words: 144 bytes on the 64-bit pair and 72 on ARMv7-A, by `sizeof` against
/// `linux/resource.h` for x86-64 and `arm-linux-gnueabihf`, and 72 for an
/// i386 program, whose words are `word` bytes. `RUSAGE_SELF` (0),
/// `RUSAGE_CHILDREN` (-1) and `RUSAGE_THREAD` (1) are accepted; anything else
/// is `EINVAL` before the buffer is touched.
pub(crate) fn sys_getrusage(
    process: &Process,
    who: i32,
    at: u64,
    word: usize,
) -> Result<usize, Errno> {
    if !matches!(who, -1..=1) {
        return Err(Errno::EINVAL);
    }
    let zeros = [0_u8; 144];
    let usage = zeros.get(..word * 18).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), at, usage).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

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
pub(crate) fn sys_clock_settime(
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
    crate::fs::timerfd::clock_was_set();
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
pub(crate) fn sys_settimeofday(
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
        crate::fs::timerfd::clock_was_set();
    }
    Ok(0)
}

/// Bytes in the `struct timex` a call reads and writes.
///
/// 208 for the 64-bit `struct __kernel_timex`, which is what every call uses
/// on the 64-bit pair and what `clock_adjtime64` uses on ARMv7-A; 128 for the
/// 32-bit `struct old_timex32` that ARMv7-A's `adjtimex` and `clock_adjtime`
/// use. By `sizeof` against `linux/timex.h` compiled for x86-64 and
/// `arm-linux-gnueabihf`, where `struct timex` is 208 and 128 and
/// `struct __kernel_timex` 208 on both.
fn timex_size(width: TimeWidth) -> usize {
    if width.field_bytes() == 8 { 208 } else { 128 }
}

/// `ADJ_ADJTIME`: the old `adjtime` interface, riding on `adjtimex`.
const ADJ_ADJTIME: u32 = 0x8000;
/// `ADJ_OFFSET_SINGLESHOT`'s low bit, which `ADJ_ADJTIME` requires.
const ADJ_OFFSET_SINGLESHOT: u32 = 0x0001;
/// `ADJ_OFFSET_READONLY`: `adjtime` asking for the offset without setting it.
const ADJ_OFFSET_READONLY: u32 = 0x2000;
/// `TIME_OK`: the clock is synchronised, with no leap second pending.
const TIME_OK: usize = 0;

/// What a `timex` whose first field is `modes` asks for: a query, which is
/// answered, or an adjustment, which is `EPERM`.
///
/// `timekeeping_validate_timex`'s rules: `ADJ_ADJTIME` needs the single-shot
/// bit or is `EINVAL`, and is a query only with the read-only bit (that is,
/// `ADJ_OFFSET_SS_READ`); otherwise any mode at all is an adjustment. Linux
/// lets root adjust. Here there is no clock discipline to adjust, so an
/// adjustment is refused as it would be for a process without
/// `CAP_SYS_TIME`, rather than accepted and ignored.
fn adjustment(modes: u32) -> Result<usize, Errno> {
    if modes & ADJ_ADJTIME != 0 {
        if modes & ADJ_OFFSET_SINGLESHOT == 0 {
            return Err(Errno::EINVAL);
        }
        if modes & ADJ_OFFSET_READONLY == 0 {
            return Err(Errno::EPERM);
        }
        return Ok(TIME_OK);
    }
    if modes != 0 {
        return Err(Errno::EPERM);
    }
    Ok(TIME_OK)
}

/// Read a `timex`, decide, and write the answer back: `modes` as given and
/// every other field zero for a query.
///
/// `copy_back_on_error` is `adjtimex`'s habit of writing the structure back
/// even when refusing; `clock_adjtime` writes only on success.
fn adjust(
    process: &Process,
    at: u64,
    width: TimeWidth,
    clock: Result<(), Errno>,
    copy_back_on_error: bool,
) -> Result<usize, Errno> {
    let mut bytes = [0_u8; 208];
    let timex = bytes.get_mut(..timex_size(width)).ok_or(Errno::EFAULT)?;
    uaccess::copy_from_user(process.space(), at, timex).map_err(|_| Errno::EFAULT)?;
    clock?;
    let (modes, rest) = timex.split_at_mut(4);
    let modes = u32::from_le_bytes((&*modes).try_into().map_err(|_| Errno::EFAULT)?);
    let answer = adjustment(modes);
    if answer.is_ok() {
        rest.fill(0);
    }
    if answer.is_ok() || copy_back_on_error {
        uaccess::copy_to_user(process.space(), at, timex).map_err(|_| Errno::EFAULT)?;
    }
    answer
}

/// `adjtimex`: the clock discipline's state, which is none.
///
/// A query returns `TIME_OK` with every field but `modes` zeroed: no offset,
/// no frequency correction, no error estimate. That is a description of a
/// clock nobody is disciplining, and it is what `adjtimex -p` has to print.
/// `width` picks the structure: an i386 program's is the 128-byte
/// `old_timex32`.
pub(crate) fn sys_adjtimex(process: &Process, at: u64, width: TimeWidth) -> Result<usize, Errno> {
    adjust(process, at, width, Ok(()), true)
}

/// `clock_adjtime` and `clock_adjtime64`: `adjtimex` for a named clock.
///
/// Only `CLOCK_REALTIME` has a discipline to ask about; the other clocks
/// Linux has are `EOPNOTSUPP` and an unknown clock is `EINVAL`. The structure
/// is read first, which is Linux's order.
pub(crate) fn sys_clock_adjtime(
    process: &Process,
    clock: i32,
    at: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    let clock = match u32::try_from(clock) {
        Ok(CLOCK_REALTIME) => Ok(()),
        Ok(
            CLOCK_MONOTONIC
            | CLOCK_PROCESS_CPUTIME_ID
            | CLOCK_THREAD_CPUTIME_ID
            | CLOCK_MONOTONIC_RAW
            | CLOCK_REALTIME_COARSE
            | CLOCK_MONOTONIC_COARSE
            | CLOCK_BOOTTIME
            | CLOCK_REALTIME_ALARM
            | CLOCK_BOOTTIME_ALARM
            | CLOCK_TAI,
        ) => Err(Errno::EOPNOTSUPP),
        _ => Err(Errno::EINVAL),
    };
    adjust(process, at, width, clock, false)
}

/// The most `getrandom` hands out in one call.
///
/// A short count is legal and every caller loops on it, so this bounds a
/// kernel stack buffer rather than the program's request.
const CHUNK: usize = 256;

/// `getrandom`.
///
/// The bytes are [`fill_random`]'s. No flag changes that: `GRND_RANDOM` is
/// accepted and means nothing different, and nothing blocks, even on a
/// machine whose generator `crate::random` reports as not seeded.
pub(crate) fn sys_getrandom(
    process: &Process,
    buf: u64,
    len: u64,
    flags: u64,
) -> Result<usize, Errno> {
    const GRND_NONBLOCK: u64 = 1;
    const GRND_RANDOM: u64 = 2;
    const GRND_INSECURE: u64 = 4;
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 {
        return Err(Errno::EINVAL);
    }
    let count = usize::try_from(len).unwrap_or(usize::MAX).min(CHUNK);
    if count == 0 {
        return Ok(0);
    }

    let mut bytes = [0_u8; CHUNK];
    let chunk = bytes.get_mut(..count).ok_or(Errno::EINVAL)?;
    fill_random(chunk);
    uaccess::copy_to_user(process.space(), buf, chunk).map_err(|_| Errno::EFAULT)?;
    Ok(count)
}

/// Fill `bytes` from the kernel's one generator, `crate::random`, which
/// `getrandom`, `/dev/random` and `/dev/urandom` all read. `/dev/random` is the
/// same stream as `/dev/urandom`, as it has been on Linux since 5.6.
pub(crate) fn fill_random(bytes: &mut [u8]) {
    crate::random::fill(bytes);
}
