//! The self-check of timerfds.
//!
//! Made by number, a timer takes `TFD_NONBLOCK` and `TFD_CLOEXEC` and refuses
//! every other flag and every clock but the three it counts on. Disarmed, it
//! reads `EAGAIN`. Armed once, it is not readable before its deadline, then
//! reads 1 and is empty again. Armed at an absolute time in the past with an
//! interval, it is readable at once and reads every interval that has passed,
//! no fewer and no more than the clock says. `timerfd_gettime` reports the
//! time left and the interval, in both `itimerspec` layouts ARMv7-A has, a
//! setting answers the one it replaced, and a zero value disarms, keeping the
//! interval. A set of `CLOCK_REALTIME` fires an absolute real-time timer it
//! carried past, cancels one armed with `TFD_TIMER_CANCEL_ON_SET`, whose read
//! is then `ECANCELED` once, and leaves a monotonic one alone.
//!
//! The one that matters most: a blocked `read`, a `poll` and an `epoll_wait`,
//! each in a task of its own and each waiting before the timer is armed, are
//! ended by the `timerfds` thread's wake at the deadline, not by their own
//! one-second recheck -- the wake count says which, and the time from the
//! deadline to the waiter coming back must stay under [`LATE_LIMIT_NANOS`],
//! a quarter of that recheck.
//!
//! Each of the three is timed up to [`ATTEMPTS`] times, and passes on the
//! first attempt that meets both. An emulator whose host stops it for a few
//! hundred milliseconds across a deadline makes the thread's timer fire that
//! much late, and a stop of most of a second lets the recheck get there
//! first: the guest's clock ran on while none of it did, which no kernel can
//! tell from oversleeping. Every attempt is judged on the same evidence and
//! printed, so a kernel that sleeps past deadlines, or never wakes the
//! queue, fails all of them; only a stall gets another chance (FX-0883).
//!
//! Refused as Linux refuses: a read into fewer than eight bytes, a write, and
//! a read at an offset. `lseek` answers 0, as Linux's `noop_llseek` does. The
//! run is done twice and must keep no frame, the thread's stack included: with
//! nothing armed it exits by itself.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, EPOLL_CTL_ADD,
    EPOLLIN, F_GETFD, F_GETFL, FD_CLOEXEC, MAP_ANONYMOUS, MAP_PRIVATE, O_NONBLOCK, PROT_READ,
    PROT_WRITE, SEEK_CUR, SEEK_SET, TFD_CLOEXEC, TFD_NONBLOCK, TFD_TIMER_ABSTIME,
    TFD_TIMER_CANCEL_ON_SET,
};
use ferrix_vfs::{Errno, OpenFile};

use crate::fs::timerfd::{self, TimerFd, VALUE_BYTES};
use crate::mm;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::check as syscall_check;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::time::{self, CLOCK_BOOTTIME_ALARM, TimeWidth};
use crate::syscall::timerfd as calls;
use crate::syscall::{epoll, fd, file, poll, uaccess};

/// Where reads land.
const AT_VALUE: u64 = 0;
/// Where a new setting, or a clock time, is staged.
const AT_SPEC: u64 = 64;
/// Where replaced and current settings are written.
const AT_OLD: u64 = 128;
/// Where a `pollfd` or an epoll event is staged.
const AT_EVENT: u64 = 192;
/// Where epoll waits write.
const AT_EVENTS: u64 = 256;

/// Nanoseconds in a millisecond.
const MILLI: u64 = 1_000_000;
/// Nanoseconds in a second.
const SECOND: u64 = 1_000_000_000;

/// A one-shot deadline far enough out that no host is slow enough to reach it
/// between the check's next two calls.
const FAR_NANOS: u64 = 500 * MILLI;
/// A one-shot deadline the check waits out.
const NEAR_NANOS: u64 = 10 * MILLI;
/// How long the check sleeps past [`NEAR_NANOS`].
const NEAR_SLEEP_NANOS: u64 = 30 * MILLI;
/// The periodic timer's interval.
const PERIOD_NANOS: u64 = SECOND;
/// How far in the past the periodic timer's first expiration is put.
const PAST_NANOS: u64 = 2_500 * MILLI;
/// The value and interval `timerfd_gettime` is checked against.
const LONG_VALUE_NANOS: u64 = 10 * SECOND;
/// See [`LONG_VALUE_NANOS`].
const LONG_INTERVAL_NANOS: u64 = 3 * SECOND;

/// How long after a waiter is listed its timer is armed to expire.
const ARM_DELAY_NANOS: u64 = 50 * MILLI;
/// The latest a waiter may come back after its deadline: a quarter of the
/// trusted recheck, so a waiter its recheck ended -- which comes back most of
/// a second late, having started waiting before the timer was armed -- cannot
/// pass, while a loaded host running other gates has a margin of hundreds of
/// milliseconds over the few a wake takes.
const LATE_LIMIT_NANOS: u64 = crate::fs::wake::TRUSTED_RECHECK_NANOS / 4;
/// How many times a waiter is timed before a late one is a failure: enough
/// that one stall of the host, which spoils the attempt it lands in, cannot
/// fail the check, and few enough that a kernel that is late every time is
/// not waited on for long.
const ATTEMPTS: u32 = 3;
/// How long the check gives a waiter to start waiting, and to come back once
/// its timer has expired.
const PATIENCE_NANOS: u64 = 2_000_000_000;
/// How often the check looks for a waiter on the queue.
const LISTED_LOOK_NANOS: u64 = MILLI;
/// The waiting `poll` or `epoll_wait`'s own timeout, far past the patience.
const WAITER_TIMEOUT_MILLIS: i32 = 10_000;

/// What the waiting task waits on, and how.
static WAITER: SpinLock<Option<Waiter>> = SpinLock::new(None);
/// What it answered, and when, on the counter.
static ANSWER: SpinLock<Option<(Result<usize, Errno>, u64)>> = SpinLock::new(None);
/// Woken when it has answered.
static DONE: WaitQueue = WaitQueue::new();

/// What a waiting task is to wait on.
struct Waiter {
    process: Arc<Process>,
    page: u64,
    /// The descriptor `poll` watches, or the epoll set `epoll_wait` waits on.
    target: i32,
    /// The timer's open file, which a read reads.
    file: Arc<OpenFile>,
    how: HowWaits,
}

/// Which call the waiting task makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HowWaits {
    /// A blocking `read`.
    Read,
    /// `poll` on the timer.
    Poll,
    /// `epoll_wait` on a set holding it.
    Epoll,
}

impl HowWaits {
    /// The call, as the boot log names it.
    const fn call(self) -> &'static str {
        match self {
            HowWaits::Read => "read",
            HowWaits::Poll => "poll",
            HowWaits::Epoll => "epoll_wait",
        }
    }
}

/// How one timed waiter came back after its deadline, on the counter.
#[derive(Debug, Clone, Copy)]
enum Timed {
    /// Ended by the deadline's wake, within [`LATE_LIMIT_NANOS`].
    OnTime(u64),
    /// Ended by the deadline's wake, but later than that.
    Late(u64),
    /// Ended by its own recheck, the wake never having reached it.
    Rechecked(u64),
}

impl Timed {
    /// How long after its deadline the waiter came back.
    const fn late(self) -> u64 {
        match self {
            Timed::OnTime(late) | Timed::Late(late) | Timed::Rechecked(late) => late,
        }
    }

    /// What ended the wait, for the line an attempt prints.
    const fn ended_by(self) -> &'static str {
        match self {
            Timed::OnTime(_) | Timed::Late(_) => "woken by the deadline",
            Timed::Rechecked(_) => "ended by its own recheck",
        }
    }

    /// What an attempt that came back like this is a failure of, if every
    /// attempt did.
    const fn failure(self) -> &'static str {
        match self {
            Timed::OnTime(_) | Timed::Late(_) => {
                "a waiter on a timerfd came back too long after its deadline"
            }
            Timed::Rechecked(_) => {
                "a waiter on a timerfd was ended by its recheck, not by the deadline's wake"
            }
        }
    }
}

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Expirations read back.
    pub expirations: u64,
    /// Calls refused as Linux refuses them.
    pub refusals: u32,
    /// The latest a waiter came back after its deadline, in microseconds.
    pub late_micros: u64,
    /// Frames the second run cost.
    pub leaked: i64,
}

/// What one run counts.
#[derive(Debug, Default)]
struct Counts {
    expirations: u64,
    refusals: u32,
    late: u64,
}

/// Run it twice, measured on the second.
pub fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the timerfd check")?;
    let _warm = check_once(&process)?;
    quiet()?;
    let window = mm::FrameWindow::open();
    let counts = check_once(&process)?;
    quiet()?;
    let leaked = window.kept();
    if leaked != 0 {
        window.report("timerfd");
        crate::console::println!("  timerfd  {leaked} frames across the second run");
        return Err("the timerfd check did not give back every frame it took");
    }
    Ok(Report {
        expirations: counts.expirations,
        refusals: counts.refusals,
        late_micros: counts.late / 1_000,
        leaked,
    })
}

/// Wait for the thread to leave and the reaper to finish, and forget the
/// closed timers, at either edge of the frame window.
fn quiet() -> Result<(), &'static str> {
    timerfd::wait_until_clock_gone(crate::sched::REAPER_PATIENCE_NANOS)?;
    timerfd::forget_closed();
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)
}

/// One run.
fn check_once(process: &Arc<Process>) -> Result<Counts, &'static str> {
    let page = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: PAGE_SIZE,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "a page for the timerfd check was refused")?;
    let page = u64::try_from(page).map_err(|_| "mmap returned an impossible address")?;
    let mut counts = Counts::default();
    let outcome = check_refusals(process, page, &mut counts)
        .and_then(|()| check_one_shot(process, page, &mut counts))
        .and_then(|()| check_overruns(process, page, &mut counts))
        .and_then(|()| check_settings(process, page, TimeWidth::Native, &mut counts))
        .and_then(|()| check_settings(process, page, TimeWidth::Wide, &mut counts))
        .and_then(|()| check_the_clock_being_set(process, page, &mut counts))
        .and_then(|()| check_waiters_are_woken(process, page, &mut counts));
    for fd in 3..32 {
        let _ = fd::sys_close(process, fd);
    }
    let _ = WAITER.lock().take();
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome.map(|()| counts)
}

/// The flags, the clocks, and every refusal that needs no deadline.
fn check_refusals(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let timer = create(process, CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC)?;
    if fd::sys_fcntl(process, timer, F_GETFD, 0) != Ok(FD_CLOEXEC as usize) {
        return Err("timerfd_create's TFD_CLOEXEC did not reach the descriptor");
    }
    if fd::sys_fcntl(process, timer, F_GETFL, 0).map(|flags| flags as u32 & O_NONBLOCK)
        != Ok(O_NONBLOCK)
    {
        return Err("timerfd_create's TFD_NONBLOCK did not reach the open file");
    }
    refused(
        read_count(process, page, timer),
        Errno::EAGAIN,
        "a disarmed non-blocking timerfd did not answer EAGAIN",
        counts,
    )?;
    let clock_refusals = [
        (CLOCK_MONOTONIC, TFD_TIMER_ABSTIME, Errno::EINVAL),
        (CLOCK_PROCESS_CPUTIME_ID, 0, Errno::EINVAL),
        (99, 0, Errno::EINVAL),
        (CLOCK_BOOTTIME_ALARM, 0, alarm_refusal(process)),
    ];
    for (clock, flags, wanted) in clock_refusals {
        refused(
            by_number(
                process,
                Syscall::TimerfdCreate,
                [u64::from(clock), u64::from(flags), 0, 0, 0, 0],
            ),
            wanted,
            "timerfd_create took a flag or a clock Linux refuses",
            counts,
        )?;
    }
    for clock in [CLOCK_REALTIME, CLOCK_BOOTTIME] {
        let made = create(process, clock, 0)?;
        closed(process, made)?;
    }
    check_file_refusals(process, page, timer, counts)?;
    check_setting_refusals(process, page, timer, counts)?;
    closed(process, timer)
}

/// What an alarm clock is answered with, for this process.
fn alarm_refusal(process: &Process) -> Errno {
    if process.with_credentials(|ids| ids.privileged()) {
        Errno::EOPNOTSUPP
    } else {
        Errno::EPERM
    }
}

/// A short read, a write and a seek.
fn check_file_refusals(
    process: &Process,
    page: u64,
    timer: i32,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    refused(
        file::sys_read(process, timer, page + AT_VALUE, 4),
        Errno::EINVAL,
        "a timerfd read into four bytes was not EINVAL",
        counts,
    )?;
    refused(
        file::sys_write(process, timer, page + AT_VALUE, 8),
        Errno::EINVAL,
        "a timerfd could be written",
        counts,
    )?;
    // `lseek` is Linux's `noop_llseek`: 0, whatever it is asked. A read at an
    // offset is still refused, as Linux refuses it.
    if [(100, SEEK_SET), (-5, SEEK_CUR)]
        .into_iter()
        .any(|(offset, whence)| fd::sys_lseek(process, timer, offset, whence) != Ok(0))
    {
        return Err("lseek on a timerfd did not answer 0, as Linux's does");
    }
    refused(
        file::sys_pread64(process, timer, page + AT_VALUE, 8, 0),
        Errno::ESPIPE,
        "a timerfd could be read at an offset",
        counts,
    )
}

/// A setting flag `timerfd_settime` does not take, a nanosecond count of a
/// second, a negative second, a descriptor that is not a timerfd and one that
/// is no descriptor.
fn check_setting_refusals(
    process: &Process,
    page: u64,
    timer: i32,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    stage_spec(process, page, 0, MILLI, TimeWidth::Native)?;
    refused(
        settime_by_number(process, page, timer, 4),
        Errno::EINVAL,
        "timerfd_settime took a flag Linux refuses",
        counts,
    )?;
    let not_a_timer = descriptor(
        by_number(process, Syscall::Eventfd2, [0, 0, 0, 0, 0, 0]),
        "eventfd2 was refused",
    )?;
    refused(
        settime_by_number(process, page, not_a_timer, 0),
        Errno::EINVAL,
        "timerfd_settime took a descriptor that is not a timerfd",
        counts,
    )?;
    closed(process, not_a_timer)?;
    refused(
        by_number(
            process,
            Syscall::TimerfdGettime,
            [1_000, page + AT_OLD, 0, 0, 0, 0],
        ),
        Errno::EBADF,
        "timerfd_gettime took a descriptor no table has",
        counts,
    )?;
    for width in [TimeWidth::Native, TimeWidth::Wide] {
        stage_raw(process, page, [0, 0, 0, SECOND], width)?;
        refused(
            calls::sys_timerfd_settime(process, timer, 0, [page + AT_SPEC, 0], width),
            Errno::EINVAL,
            "timerfd_settime took a nanosecond count of a whole second",
            counts,
        )?;
    }
    // A negative second, in the layout that can carry one on every
    // architecture.
    stage_raw(process, page, [0, 0, u64::MAX, 0], TimeWidth::Wide)?;
    refused(
        calls::sys_timerfd_settime(process, timer, 0, [page + AT_SPEC, 0], TimeWidth::Wide),
        Errno::EINVAL,
        "timerfd_settime took a negative second",
        counts,
    )
}

/// Armed once: not readable before the deadline, one expiration after it,
/// then empty; the replaced setting answered; disarmed after.
fn check_one_shot(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let timer = create(process, CLOCK_MONOTONIC, TFD_NONBLOCK)?;
    let opened = fd::file(process, timer).map_err(|_| "the timerfd is gone")?;
    let _ = set(process, page, timer, 0, [0, FAR_NANOS], TimeWidth::Native)?;
    if opened.poll().readable {
        return Err("a timerfd was readable long before its deadline");
    }
    refused(
        read_count(process, page, timer),
        Errno::EAGAIN,
        "a timerfd was read before its deadline",
        counts,
    )?;
    let replaced = set(process, page, timer, 0, [0, NEAR_NANOS], TimeWidth::Native)?;
    if replaced.0 != 0 || replaced.1 == 0 || replaced.1 > FAR_NANOS {
        return Err("timerfd_settime did not answer the time left on the setting it replaced");
    }
    crate::sched::sleep_for(NEAR_NANOS + NEAR_SLEEP_NANOS);
    if !opened.poll().readable {
        return Err("a timerfd past its deadline was not readable");
    }
    if read_count(process, page, timer) != Ok(1) {
        return Err("a one-shot timerfd past its deadline did not read 1");
    }
    counts.expirations += 1;
    refused(
        read_count(process, page, timer),
        Errno::EAGAIN,
        "a one-shot timerfd read twice",
        counts,
    )?;
    if get(process, page, timer, TimeWidth::Native)? != (0, 0) {
        return Err("an expired one-shot timerfd did not report itself disarmed");
    }
    drop(opened);
    closed(process, timer)
}

/// Armed at an absolute time in the past with an interval: readable at once,
/// and every interval since counted; a new setting throws the count away.
fn check_overruns(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let timer = create(process, CLOCK_MONOTONIC, TFD_NONBLOCK)?;
    let opened = fd::file(process, timer).map_err(|_| "the timerfd is gone")?;
    let first = crate::timer::now_nanos().saturating_sub(PAST_NANOS).max(1);
    let _ = set(
        process,
        page,
        timer,
        TFD_TIMER_ABSTIME,
        [PERIOD_NANOS, first],
        TimeWidth::Native,
    )?;
    if !opened.poll().readable {
        return Err("a timerfd armed at an absolute time in the past was not readable at once");
    }
    let expected = |at: u64| (at - first) / PERIOD_NANOS + 1;
    let before = crate::timer::now_nanos();
    let read = read_count(process, page, timer);
    let after = crate::timer::now_nanos();
    let Ok(read) = read else {
        return Err("a periodic timerfd with intervals behind it could not be read");
    };
    let read = read as u64;
    if read < expected(before) || read > expected(after) {
        crate::console::println!(
            "  timerfd  read {read} expirations, between {} and {} were due",
            expected(before),
            expected(after)
        );
        return Err("a periodic timerfd read late did not count every interval that passed");
    }
    counts.expirations += read;
    let (interval, value) = get(process, page, timer, TimeWidth::Native)?;
    if interval != PERIOD_NANOS || value > PERIOD_NANOS {
        return Err("a periodic timerfd did not report its interval and the time to the next");
    }
    // Due again at once, then set anew before being read: nothing to read.
    let _ = set(
        process,
        page,
        timer,
        TFD_TIMER_ABSTIME,
        [0, first],
        TimeWidth::Native,
    )?;
    let _ = set(process, page, timer, 0, [0, FAR_NANOS], TimeWidth::Native)?;
    refused(
        read_count(process, page, timer),
        Errno::EAGAIN,
        "timerfd_settime kept an expiration the setting it replaced had",
        counts,
    )?;
    drop(opened);
    closed(process, timer)
}

/// `timerfd_gettime` and the replaced setting, in one `itimerspec` layout, and
/// a zero value disarming while keeping the interval.
fn check_settings(
    process: &Process,
    page: u64,
    width: TimeWidth,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let timer = create(process, CLOCK_MONOTONIC, TFD_NONBLOCK)?;
    let before = crate::timer::now_nanos();
    let replaced = set(
        process,
        page,
        timer,
        0,
        [LONG_INTERVAL_NANOS, LONG_VALUE_NANOS],
        width,
    )?;
    if replaced != (0, 0) {
        return Err("a new timerfd's first setting did not replace a disarmed one");
    }
    let (interval, value) = get(process, page, timer, width)?;
    let elapsed = crate::timer::now_nanos().saturating_sub(before);
    if interval != LONG_INTERVAL_NANOS
        || value > LONG_VALUE_NANOS
        || value < LONG_VALUE_NANOS.saturating_sub(elapsed)
    {
        return Err("timerfd_gettime did not report the time left and the interval");
    }
    let replaced = set(process, page, timer, 0, [LONG_INTERVAL_NANOS, 0], width)?;
    if replaced.0 != LONG_INTERVAL_NANOS || replaced.1 == 0 || replaced.1 > LONG_VALUE_NANOS {
        return Err("disarming a timerfd did not answer the setting it replaced");
    }
    if get(process, page, timer, width)? != (LONG_INTERVAL_NANOS, 0) {
        return Err("a disarmed timerfd did not report no time left and its interval kept");
    }
    refused(
        read_count(process, page, timer),
        Errno::EAGAIN,
        "a disarmed timerfd could be read",
        counts,
    )?;
    closed(process, timer)
}

/// A set of `CLOCK_REALTIME`, with the clock put back whatever happened.
fn check_the_clock_being_set(
    process: &Process,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let saved = time::realtime_offset();
    let outcome = set_the_clock_under_timers(process, page, counts);
    time::restore_realtime_offset(saved);
    outcome
}

/// An hour forward: an absolute real-time timer due in a hundred seconds
/// fires, one due in two hours that asked to be canceled reads `ECANCELED`
/// once, and a monotonic one due in a hundred seconds does not move.
fn set_the_clock_under_timers(
    process: &Process,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let plain = create(process, CLOCK_REALTIME, TFD_NONBLOCK)?;
    let canceled = create(process, CLOCK_REALTIME, TFD_NONBLOCK)?;
    let monotonic = create(process, CLOCK_MONOTONIC, TFD_NONBLOCK)?;
    let now = time::realtime_nanos();
    let absolute = TFD_TIMER_ABSTIME;
    let _ = set(
        process,
        page,
        plain,
        absolute,
        [0, now + 100 * SECOND],
        TimeWidth::Native,
    )?;
    let cancel = absolute | TFD_TIMER_CANCEL_ON_SET;
    let _ = set(
        process,
        page,
        canceled,
        cancel,
        [0, now + 7_200 * SECOND],
        TimeWidth::Native,
    )?;
    let _ = set(
        process,
        page,
        monotonic,
        0,
        [0, 100 * SECOND],
        TimeWidth::Native,
    )?;
    let target = (now + 3_600 * SECOND) / SECOND;
    time::write_pair(process, page + AT_SPEC, target, 0, TimeWidth::Native)
        .map_err(|_| "could not stage a clock time")?;
    let clock = by_number(
        process,
        Syscall::ClockSettime,
        [u64::from(CLOCK_REALTIME), page + AT_SPEC, 0, 0, 0, 0],
    );
    if clock != Ok(0) {
        return Err("clock_settime was refused to the timerfd check");
    }
    if read_count(process, page, plain) != Ok(1) {
        return Err("an absolute real-time timerfd the clock was set past did not fire");
    }
    counts.expirations += 1;
    refused(
        read_count(process, page, canceled),
        Errno::ECANCELED,
        "a timerfd armed with TFD_TIMER_CANCEL_ON_SET did not read ECANCELED after a clock set",
        counts,
    )?;
    refused(
        read_count(process, page, canceled),
        Errno::EAGAIN,
        "a canceled timerfd read ECANCELED twice for one clock set",
        counts,
    )?;
    refused(
        read_count(process, page, monotonic),
        Errno::EAGAIN,
        "a set of the real-time clock fired a monotonic timerfd",
        counts,
    )?;
    closed(process, plain)?;
    closed(process, canceled)?;
    closed(process, monotonic)
}

/// A blocked read, a `poll` and an `epoll_wait`, each waiting in a task of its
/// own before the timer is armed, are ended by the wake at the deadline --
/// on one of [`ATTEMPTS`] attempts each, for the reason the module gives.
fn check_waiters_are_woken(
    process: &Arc<Process>,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    for how in [HowWaits::Read, HowWaits::Poll, HowWaits::Epoll] {
        let mut attempt = 1;
        let late = loop {
            let timed = time_a_waiter(process, page, how)?;
            if let Timed::OnTime(late) = timed {
                break late;
            }
            crate::console::println!(
                "  timerfd  attempt {attempt} of {ATTEMPTS}: a waiter in {} came back {} ms \
                 after its deadline, {}",
                how.call(),
                timed.late() / MILLI,
                timed.ended_by(),
            );
            if attempt == ATTEMPTS {
                return Err(timed.failure());
            }
            attempt += 1;
        };
        counts.late = counts.late.max(late);
        counts.expirations += 1;
    }
    Ok(())
}

/// One attempt of [`check_waiters_are_woken`]: a new timer, a waiter on it,
/// and how the waiter came back.
fn time_a_waiter(process: &Arc<Process>, page: u64, how: HowWaits) -> Result<Timed, &'static str> {
    let timer = create(process, CLOCK_MONOTONIC, 0)?;
    let file = fd::file(process, timer).map_err(|_| "the timerfd is gone")?;
    let inner = timerfd::of(&file).ok_or("a timerfd is not one")?;
    let target = watch(process, page, timer, how)?;
    *ANSWER.lock() = None;
    *WAITER.lock() = Some(Waiter {
        process: Arc::clone(process),
        page,
        target,
        file: Arc::clone(&file),
        how,
    });
    // An `epoll_wait` sleeps on its set's queue, which the deadline's wake
    // wakes through the registration's hook (`fs::epoll`'s ready list).
    let set = match how {
        HowWaits::Epoll => {
            let file = fd::file(process, target).map_err(|_| "the epoll set is gone")?;
            Some(crate::fs::epoll::of(&file).ok_or("an epoll set is not one")?)
        }
        HowWaits::Read | HowWaits::Poll => None,
    };
    let timed = wait_out_a_deadline(process, page, timer, &inner, set.as_deref())?;
    drop(set);
    // The expiration the waiter was told of, still to be taken. A waiter its
    // recheck ended may have been told of none.
    if how != HowWaits::Read
        && matches!(timed, Timed::OnTime(_) | Timed::Late(_))
        && read_count(process, page, timer) != Ok(1)
    {
        return Err("a timerfd a poll or epoll_wait was woken for did not read 1");
    }
    if how == HowWaits::Epoll {
        closed(process, target)?;
    }
    drop(inner);
    drop(file);
    closed(process, timer)?;
    Ok(timed)
}

/// What the waiting task watches: the timer itself for a read or a `poll`,
/// with a `pollfd` staged for the latter, or a new epoll set holding it.
fn watch(process: &Process, page: u64, timer: i32, how: HowWaits) -> Result<i32, &'static str> {
    match how {
        HowWaits::Read => Ok(timer),
        HowWaits::Poll => {
            stage_pollfd(process, page, timer)?;
            Ok(timer)
        }
        HowWaits::Epoll => {
            let set = descriptor(
                epoll::sys_epoll_create1(process, 0),
                "epoll_create1 was refused",
            )?;
            uaccess::copy_to_user(
                process.space(),
                page + AT_EVENT,
                &epoll::encode(EPOLLIN, 0x7F),
            )
            .map_err(|_| "could not stage an epoll event")?;
            if epoll::sys_epoll_ctl(process, set, EPOLL_CTL_ADD, timer, page + AT_EVENT) != Ok(0) {
                return Err("a timerfd could not be added to an epoll set");
            }
            Ok(set)
        }
    }
}

/// Start the waiter, arm the timer once it waits, and answer how late after
/// the deadline it came back, and whether the deadline's wake is what ended
/// its wait.
fn wait_out_a_deadline(
    process: &Process,
    page: u64,
    timer: i32,
    inner: &TimerFd,
    epoll_set: Option<&crate::fs::epoll::Epoll>,
) -> Result<Timed, &'static str> {
    let ended = || {
        epoll_set.map_or_else(
            || inner.waits_ended_by_a_wake(),
            |set| set.queue().waits_ended_by_a_wake(),
        )
    };
    let ended_before = ended();
    let waiter = crate::sched::spawn("timerfd-waiter", waiter, 0, ferrix_sched_fair::NICE_0_WEIGHT)?;
    until_listed(inner, epoll_set);
    if ANSWER.lock().is_some() {
        return Err("a read, poll or epoll_wait on a disarmed timerfd did not wait");
    }
    let armed = crate::timer::now_nanos();
    let _ = set(
        process,
        page,
        timer,
        0,
        [0, ARM_DELAY_NANOS],
        TimeWidth::Native,
    )?;
    let deadline = armed + ARM_DELAY_NANOS;
    let patience = crate::timer::now_nanos().saturating_add(ARM_DELAY_NANOS + PATIENCE_NANOS);
    let _ = DONE.wait_until_deadline(|| ANSWER.lock().is_some(), patience);
    let answer = ANSWER.lock().take();
    crate::sched::wait_until_gone(&waiter, crate::sched::REAPER_PATIENCE_NANOS)?;
    drop(waiter);
    let Some((answer, back)) = answer else {
        return Err("a read, poll or epoll_wait on a timerfd never came back after its deadline");
    };
    if answer != Ok(1) {
        return Err("a read, poll or epoll_wait woken by a timerfd did not answer one expiration");
    }
    if back < deadline {
        return Err("a waiter on a timerfd came back before the deadline");
    }
    let late = back - deadline;
    if ended() == ended_before {
        return Ok(Timed::Rechecked(late));
    }
    if late > LATE_LIMIT_NANOS {
        return Ok(Timed::Late(late));
    }
    Ok(Timed::OnTime(late))
}

/// Wait until a task is listed on the timer's queue, has answered, or the
/// patience runs out -- whichever is first. The check that follows says
/// which. Only then is the timer armed, so the waiter is waiting when it
/// expires however long the host took to run it.
fn until_listed(timer: &TimerFd, set: Option<&crate::fs::epoll::Epoll>) {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let listed = || set.map_or_else(|| timer.readers_listed(), |set| set.queue().listed());
    while listed() == 0 && ANSWER.lock().is_none() && crate::timer::now_nanos() < deadline {
        crate::sched::sleep_for(LISTED_LOOK_NANOS);
    }
}

/// The waiting task: one blocking read, `poll` or `epoll_wait` on what
/// [`WAITER`] names, and the counter when it came back.
fn waiter(_argument: usize) {
    let subject = WAITER.lock().take();
    let answer = match subject {
        Some(Waiter {
            file,
            how: HowWaits::Read,
            ..
        }) => {
            let mut value = [0_u8; VALUE_BYTES];
            file.read(&mut value)
                .map(|_| u64::from_ne_bytes(value) as usize)
        }
        Some(Waiter {
            process,
            page,
            how: HowWaits::Poll,
            ..
        }) => poll::sys_poll(&process, page + AT_EVENT, 1, WAITER_TIMEOUT_MILLIS),
        Some(Waiter {
            process,
            page,
            target,
            how: HowWaits::Epoll,
            ..
        }) => epoll::sys_epoll_wait(&process, target, page + AT_EVENTS, 4, WAITER_TIMEOUT_MILLIS),
        None => Err(Errno::ESRCH),
    };
    let back = crate::timer::now_nanos();
    *ANSWER.lock() = Some((answer, back));
    DONE.wake_all();
}

/// Stage a `struct pollfd` asking `fd` for `POLLIN` where [`AT_EVENT`] is.
fn stage_pollfd(process: &Process, page: u64, fd: i32) -> Result<(), &'static str> {
    let mut entry = [0_u8; 8];
    for (slot, byte) in entry.iter_mut().zip(fd.to_le_bytes()) {
        *slot = byte;
    }
    for (slot, byte) in entry.iter_mut().skip(4).zip(poll::POLLIN.to_le_bytes()) {
        *slot = byte;
    }
    uaccess::copy_to_user(process.space(), page + AT_EVENT, &entry)
        .map_err(|_| "could not stage a pollfd")
}

/// `timerfd_create(clock, flags)` by number, as a descriptor.
fn create(process: &Process, clock: u32, flags: u32) -> Result<i32, &'static str> {
    descriptor(
        by_number(
            process,
            Syscall::TimerfdCreate,
            [u64::from(clock), u64::from(flags), 0, 0, 0, 0],
        ),
        "timerfd_create was refused",
    )
}

/// Bytes in one `timespec` of `width`.
fn timespec_bytes(width: TimeWidth) -> u64 {
    if width == TimeWidth::Wide || uaccess::WORD == 8 {
        16
    } else {
        8
    }
}

/// Stage an `itimerspec` of `width` where [`AT_SPEC`] is.
fn stage_spec(
    process: &Process,
    page: u64,
    interval: u64,
    value: u64,
    width: TimeWidth,
) -> Result<(), &'static str> {
    stage_raw(
        process,
        page,
        [
            interval / SECOND,
            interval % SECOND,
            value / SECOND,
            value % SECOND,
        ],
        width,
    )
}

/// Stage four `itimerspec` fields as they are, the interval's pair first.
fn stage_raw(
    process: &Process,
    page: u64,
    [first, second, third, fourth]: [u64; 4],
    width: TimeWidth,
) -> Result<(), &'static str> {
    let at = page + AT_SPEC;
    let wide = width == TimeWidth::Wide || uaccess::WORD == 8;
    let write = |at: u64, seconds: u64, nanos: u64| {
        if wide {
            time::write_pair(process, at, seconds, nanos, TimeWidth::Wide)
        } else {
            // Truncated to the 32-bit fields as a 32-bit program's would be.
            time::write_pair(
                process,
                at,
                u64::from(seconds as u32),
                u64::from(nanos as u32),
                width,
            )
        }
    };
    write(at, first, second)
        .and_then(|()| write(at + timespec_bytes(width), third, fourth))
        .map_err(|_| "could not stage an itimerspec")
}

/// Read back an `itimerspec` of `width` from [`AT_OLD`], as nanoseconds.
fn read_spec(process: &Process, page: u64, width: TimeWidth) -> Result<(u64, u64), &'static str> {
    let at = page + AT_OLD;
    let as_nanos = |(seconds, nanos): (i64, i64)| time::nanos_of(seconds, nanos);
    let interval = time::read_pair(process, at, width).and_then(as_nanos);
    let value = time::read_pair(process, at + timespec_bytes(width), width).and_then(as_nanos);
    match (interval, value) {
        (Ok(interval), Ok(value)) => Ok((interval, value)),
        _ => Err("timerfd wrote an itimerspec that does not read back"),
    }
}

/// `timerfd_settime` of the Native layout by number, from what is staged.
fn settime_by_number(process: &Process, page: u64, timer: i32, flags: u32) -> Result<usize, Errno> {
    by_number(
        process,
        Syscall::TimerfdSettime,
        [
            timer as u64,
            u64::from(flags),
            page + AT_SPEC,
            page + AT_OLD,
            0,
            0,
        ],
    )
}

/// Set a timer to `[interval, value]` in `width`, answering the setting it
/// replaced: by number in the native layout, through the call's handler in
/// the wide one, which a 64-bit table has no number of its own for.
fn set(
    process: &Process,
    page: u64,
    timer: i32,
    flags: u32,
    [interval, value]: [u64; 2],
    width: TimeWidth,
) -> Result<(u64, u64), &'static str> {
    stage_spec(process, page, interval, value, width)?;
    let answer = match width {
        TimeWidth::Native => settime_by_number(process, page, timer, flags),
        TimeWidth::Wide | TimeWidth::Compat | TimeWidth::CompatWide => calls::sys_timerfd_settime(
            process,
            timer,
            flags,
            [page + AT_SPEC, page + AT_OLD],
            width,
        ),
    };
    if answer != Ok(0) {
        return Err("timerfd_settime was refused");
    }
    read_spec(process, page, width)
}

/// A timer's setting in `width`, as `timerfd_gettime` reports it.
fn get(
    process: &Process,
    page: u64,
    timer: i32,
    width: TimeWidth,
) -> Result<(u64, u64), &'static str> {
    let answer = match width {
        TimeWidth::Native => by_number(
            process,
            Syscall::TimerfdGettime,
            [timer as u64, page + AT_OLD, 0, 0, 0, 0],
        ),
        TimeWidth::Wide | TimeWidth::Compat | TimeWidth::CompatWide => {
            calls::sys_timerfd_gettime(process, timer, page + AT_OLD, width)
        }
    };
    if answer != Ok(0) {
        return Err("timerfd_gettime was refused");
    }
    read_spec(process, page, width)
}

/// Read a timer's count, as a result a refusal can be compared with.
fn read_count(process: &Process, page: u64, timer: i32) -> Result<usize, Errno> {
    let got = file::sys_read(process, timer, page + AT_VALUE, 16)?;
    if got != VALUE_BYTES {
        return Err(Errno::EIO);
    }
    let mut bytes = [0_u8; VALUE_BYTES];
    uaccess::copy_from_user(process.space(), page + AT_VALUE, &mut bytes)
        .map_err(|_| Errno::EFAULT)?;
    usize::try_from(u64::from_ne_bytes(bytes)).map_err(|_| Errno::EOVERFLOW)
}

/// Close a descriptor the check opened.
fn closed(process: &Process, opened: i32) -> Result<(), &'static str> {
    fd::sys_close(process, opened)
        .map(|_| ())
        .map_err(|_| "a descriptor the timerfd check opened would not close")
}

/// Make `call` by its number, as a program would.
fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}

/// A descriptor a call returned.
fn descriptor(got: Result<usize, Errno>, what: &'static str) -> Result<i32, &'static str> {
    got.ok().and_then(|fd| i32::try_from(fd).ok()).ok_or(what)
}

/// Require `got` to be `Err(wanted)`, counting the refusal.
fn refused(
    got: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    if got != Err(wanted) {
        return Err(what);
    }
    counts.refusals += 1;
    Ok(())
}
