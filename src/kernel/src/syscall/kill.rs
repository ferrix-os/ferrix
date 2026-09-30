//! Sending signals: `kill`, `tkill`, `tgkill`, a child telling its parent, and
//! the interval timer that raises `SIGALRM`.
//!
//! # A send decides, delivery acts
//!
//! [`send`] makes the one decision a sender can: through
//! [`Signals::post`](super::signal::Signals::post), under the target's lock,
//! whether the signal is discarded, pending, or fatal now. Everything else --
//! choosing a handler, building its frame -- happens in the target's own task
//! on its way back to user mode (`super::deliver`), because only that task has
//! the registers. What a sender owes the target is to make sure that way back
//! is taken soon: a task blocked in a call is woken, and one running in user
//! mode on another processor is interrupted.
//!
//! # A process or one thread
//!
//! `kill` sends to a process as a whole: any of its threads that does not
//! block the signal may take it, and one is woken to. `tkill`, `tgkill` and a
//! write to a broken pipe send to one thread, into its own queue, where only
//! that thread takes it -- which is how glibc's and musl's `raise`, `abort`
//! and `pthread_kill` reach the thread they mean. Either way a stop, a
//! continue or a fatal default acts on the whole process.
//!
//! # The interval timer
//!
//! `ITIMER_REAL` is a deadline in the process's signal state and one kernel
//! thread, `itimers`, that sleeps until the earliest deadline across every
//! process, raises `SIGALRM` where one is due, and exits when none is armed. A
//! thread rather than a check on the way back to user mode, because the
//! process that asked for an alarm is usually not coming back on its own: it
//! is blocked in `pause`, or spinning alone on a processor that takes no tick.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::SpinLock;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{NSIG, SIGALRM, SIGCHLD, SIGCONT};

use crate::sched::{self, WaitQueue};
use crate::syscall::credentials;
use crate::syscall::deliver;
use crate::syscall::pidns;
use crate::syscall::process::{self, Process};
use crate::syscall::registry;
use crate::syscall::signal::{Alarm, Origin, Posted};
use crate::syscall::thread::{self, Thread};
use crate::syscall::uaccess;

/// `si_code` for a child that exited.
pub(crate) const CLD_EXITED: i32 = 1;
/// `si_code` for a child a signal ended.
pub(crate) const CLD_KILLED: i32 = 2;
/// `si_code` for a child that stopped.
pub(crate) const CLD_STOPPED: i32 = 5;
/// `si_code` for a child that continued.
pub(crate) const CLD_CONTINUED: i32 = 6;

/// The signal calls that send, wait or time, or `None` for any other call.
///
/// A table of its own, as `super::fsctl` has, so that the dispatch table gains
/// one line rather than a dozen arms.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
) -> Option<Result<usize, Errno>> {
    let word = |at: usize| a.get(at).copied().unwrap_or(0);
    let int = |at: usize| word(at) as i32;
    let flag = |at: usize| word(at) as u32;
    let answer = match call {
        Syscall::Kill => sys_kill(process, int(0), flag(1)),
        Syscall::PidfdSendSignal => {
            sys_pidfd_send_signal(process, int(0), flag(1), word(2), flag(3))
        }
        Syscall::Tkill => sys_tkill(process, int(0), flag(1)),
        Syscall::Tgkill => sys_tgkill(process, int(0), int(1), flag(2)),
        Syscall::Pause => deliver::sys_pause(process),
        Syscall::Alarm => Ok(sys_alarm(process, flag(0))),
        Syscall::Getitimer => sys_getitimer(process, flag(0), word(1)),
        Syscall::Setitimer => sys_setitimer(process, flag(0), word(1), word(2)),
        _ => return None,
    };
    Some(answer)
}

/// Send `signal` to `target` as `origin`. Zero and numbers past 64 send
/// nothing, and neither does anything sent to a process that has ended.
///
/// `SIGCONT` continues a stopped process as it is sent, whatever the process
/// does with the signal itself: that is what makes it the one signal a stopped
/// process can act on.
pub(crate) fn send(target: &Process, signal: u32, origin: Origin) {
    if signal == 0 || signal > NSIG || target.is_terminated() {
        return;
    }
    if signal == SIGCONT {
        target.leave_stop();
    }
    if pidns::discards(target, signal, origin) {
        return;
    }
    match target.post_signal(signal, origin) {
        Posted::Discarded => {}
        Posted::Fatal => process::kill(target, 128 + signal as i32),
        Posted::Pending => target.notify_signal(signal),
    }
}

/// Send `signal` to `thread` alone, as `origin`: as [`send`], except that it
/// is recorded for that thread, judged against that thread's mask, and only
/// that thread is woken to take it.
pub(crate) fn send_to_thread(thread: &Thread, signal: u32, origin: Origin) {
    let target = thread.process();
    if signal == 0 || signal > NSIG || target.is_terminated() {
        return;
    }
    if signal == SIGCONT {
        target.leave_stop();
    }
    if pidns::discards(target, signal, origin) {
        return;
    }
    match target.post_signal_to(thread, signal, origin) {
        Posted::Discarded => {}
        Posted::Fatal => process::kill(target, 128 + signal as i32),
        Posted::Pending => target.notify_signal_to(thread),
    }
}

/// Send `signal` to the running task's own thread, from the kernel: what a
/// write to a pipe with no reader does with `SIGPIPE`, which Linux sends to
/// the writing thread. Nothing, for a kernel thread.
pub(crate) fn send_to_current(signal: u32) {
    if let Some(thread) = thread::current() {
        send_to_thread(&thread, signal, Origin::Kernel);
    }
}

/// `kill`.
///
/// `pid` above zero is that process; zero, every process in the caller's
/// group; -1, every process but the caller and pid 1; below -1, every process
/// in group `-pid`. Signal zero sends nothing and only asks whether a target
/// exists and may be signalled. A target the caller may not signal
/// ([`may_signal`]) is passed over; a group or `-1` succeeds if any member was
/// signalled, as Linux's `__kill_pgrp_info` does.
///
/// # Errors
///
/// `EINVAL` for a signal past 64; `ESRCH` when no process matches; `EPERM`
/// when none that matched may be signalled.
#[inline(never)]
pub(crate) fn sys_kill(process: &Process, pid: i32, signal: u32) -> Result<usize, Errno> {
    if signal > NSIG {
        return Err(Errno::EINVAL);
    }
    let caller = process.pid();
    // Every number here is one in the caller's namespace, and a process it
    // cannot see is not a target (`docs/PIDNS.md` §3).
    let targets: Vec<Arc<Process>> = if pid > 0 {
        pidns::find_in(process, pid.unsigned_abs())
            .into_iter()
            .collect()
    } else {
        let group = if pid == 0 {
            Some(process.pgid())
        } else {
            pidns::from_user(process, pid.unsigned_abs())
        };
        pidns::live_in(process)?
            .into_iter()
            .filter(|other| match pid {
                // Not the caller, and not the init of its namespace.
                -1 => other.pid() != caller && pidns::to_user(process, other) > 1,
                _ => group == Some(other.pgid()),
            })
            .collect()
    };
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    let mut sent = false;
    for target in &targets {
        if may_signal(process, target, signal) {
            send(
                target,
                signal,
                Origin::User {
                    pid: caller,
                    uid: real_uid(process),
                },
            );
            sent = true;
        }
    }
    if !sent {
        return Err(Errno::EPERM);
    }
    Ok(0)
}

/// `pidfd_send_signal`'s flag for the whole process, as no flag is.
const PIDFD_SIGNAL_THREAD_GROUP: u32 = 1 << 1;

/// `pidfd_send_signal`: `kill` for the one process a pidfd names, which a
/// pid reused since cannot redirect. A process that has ended and not been
/// reaped takes the call and ignores the signal, as `kill` on it does; once
/// reaped, its pid is gone and the call is `ESRCH`.
///
/// # Errors
///
/// `EBADF` for a descriptor that is not a pidfd; `EINVAL` for a flag other
/// than `PIDFD_SIGNAL_THREAD_GROUP`, for a
/// `siginfo_t` (not taken yet: the call with none is `kill`'s) or a signal
/// past 64; `ESRCH` once the process is reaped; `EPERM` as `kill`.
pub(crate) fn sys_pidfd_send_signal(
    process: &Process,
    pidfd: i32,
    signal: u32,
    info: u64,
    flags: u32,
) -> Result<usize, Errno> {
    let file = crate::syscall::fd::file(process, pidfd)?;
    let pidfd = crate::fs::pidfd::of(&file).ok_or(Errno::EBADF)?;
    // `PIDFD_SIGNAL_THREAD_GROUP` names what no flag does for a process's
    // pidfd; the thread and process-group forms are not taken yet.
    if flags & !PIDFD_SIGNAL_THREAD_GROUP != 0 || info != 0 || signal > NSIG {
        return Err(Errno::EINVAL);
    }
    let target = pidfd.process();
    if target.is_reaped() || pidns::to_user(process, target) == 0 {
        return Err(Errno::ESRCH);
    }
    if !may_signal(process, target, signal) {
        return Err(Errno::EPERM);
    }
    send(
        target,
        signal,
        Origin::User {
            pid: process.pid(),
            uid: real_uid(process),
        },
    );
    Ok(0)
}

/// Linux's `check_kill_permission`: the ids allow it, or the signal is
/// `SIGCONT` to a process in the caller's own session.
fn may_signal(sender: &Process, target: &Process, signal: u32) -> bool {
    credentials::may_signal_ids(sender, target)
        || (signal == SIGCONT && sender.sid() == target.sid())
}

/// `tkill`: signal thread `tid` alone, found through its process.
///
/// # Errors
///
/// `EINVAL` for a thread id below one or a signal past 64; `ESRCH` for a
/// thread nobody has, or one that has begun to end.
pub(crate) fn sys_tkill(process: &Process, tid: i32, signal: u32) -> Result<usize, Errno> {
    if tid <= 0 || signal > NSIG {
        return Err(Errno::EINVAL);
    }
    let thread = find_thread(process, tid.unsigned_abs(), None)?;
    if !may_signal(process, thread.process(), signal) {
        return Err(Errno::EPERM);
    }
    send_to_thread(
        &thread,
        signal,
        Origin::Thread {
            pid: process.pid(),
            uid: real_uid(process),
        },
    );
    Ok(0)
}

/// The live thread `viewer` calls `tid`, required to be one of the process
/// it calls `tgid` when that is given.
#[inline(never)]
fn find_thread(viewer: &Process, tid: u32, tgid: Option<u32>) -> Result<Arc<Thread>, Errno> {
    let kernel = pidns::from_user(viewer, tid).ok_or(Errno::ESRCH)?;
    let target = registry::find(kernel).ok_or(Errno::ESRCH)?;
    if tgid.is_some_and(|tgid| pidns::to_user(viewer, &target) != tgid) {
        return Err(Errno::ESRCH);
    }
    target.thread_by_tid(kernel).ok_or(Errno::ESRCH)
}

/// `tgkill`: signal thread `tid` of process `tgid`, which is what glibc's
/// `raise`, `abort` and `pthread_kill` call. As [`sys_tkill`], with the thread
/// required to be one of `tgid`'s.
///
/// # Errors
///
/// As [`sys_tkill`], and `ESRCH` for a thread not in that group.
pub(crate) fn sys_tgkill(
    process: &Process,
    tgid: i32,
    tid: i32,
    signal: u32,
) -> Result<usize, Errno> {
    if tgid <= 0 || tid <= 0 || signal > NSIG {
        return Err(Errno::EINVAL);
    }
    let thread = find_thread(process, tid.unsigned_abs(), Some(tgid.unsigned_abs()))?;
    if !may_signal(process, thread.process(), signal) {
        return Err(Errno::EPERM);
    }
    send_to_thread(
        &thread,
        signal,
        Origin::Thread {
            pid: process.pid(),
            uid: real_uid(process),
        },
    );
    Ok(0)
}

/// A process's real user id, as a kernel id: what `si_uid` is made from.
fn real_uid(process: &Process) -> u32 {
    process.with_credentials(|held| held.user.real)
}

/// Tell `child`'s parent that the child changed state: wake anything waiting
/// for a child, and send the signal. `SIGCHLD` for a stop or a continue unless
/// the parent asked not to be told with `SA_NOCLDSTOP`; for an end, the signal
/// the child was created with, which `clone` may have made none.
pub(crate) fn tell_parent(child: &Process, signal: u32, code: i32, status: i32) {
    let Some(parent) = child.parent() else {
        return;
    };
    parent.child_exited().wake_all();
    let about_a_stop = code == CLD_STOPPED || code == CLD_CONTINUED;
    if about_a_stop && parent.with_signals(|signals| signals.ignores_child_stops()) {
        return;
    }
    let signal = if about_a_stop { SIGCHLD } else { signal };
    send(
        &parent,
        signal,
        Origin::Child {
            code,
            pid: child.pid(),
            uid: real_uid(child),
            status,
        },
    );
}

// ---------------------------------------------------------------------------
// Interval timers
// ---------------------------------------------------------------------------

/// `ITIMER_REAL`, the only timer that counts wall-clock time.
const ITIMER_REAL: u32 = 0;
/// `ITIMER_VIRTUAL`: user time. Not kept.
const ITIMER_VIRTUAL: u32 = 1;
/// `ITIMER_PROF`: user and system time. Not kept.
const ITIMER_PROF: u32 = 2;

/// Nanoseconds in a second.
const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// Nanoseconds in a microsecond, the unit of a `timeval`.
const NANOS_PER_MICRO: u64 = 1_000;

/// Whether the `itimers` thread is running. Taken to decide it may exit, and
/// to decide one must be started, so the two decisions cannot cross.
static CLOCK_RUNNING: SpinLock<bool> = SpinLock::new(false);
/// Set when any process's `ITIMER_REAL` changed, so the thread looks again.
static ALARMS_CHANGED: AtomicBool = AtomicBool::new(false);
/// Where the `itimers` thread sleeps until the next deadline.
static ALARMS: WaitQueue = WaitQueue::new();

/// Tell the `itimers` thread a deadline changed, starting it if it is not
/// running.
fn alarms_changed() {
    let start = {
        let mut running = CLOCK_RUNNING.lock();
        ALARMS_CHANGED.store(true, Ordering::Release);
        // Claimed under the lock, spawned outside it: making a thread frees a
        // half-made stack if it fails, and that is a shootdown, which may not
        // be asked for under a lock that disables preemption. A caller in
        // between sees the clock as running and only wakes it, which is
        // right whether the spawn is still on its way or has just succeeded.
        let start = !*running;
        *running = true;
        start
    };
    if start && sched::spawn("itimers", run_alarm_clock, 0, ferrix_sched::NICE_0_WEIGHT).is_err() {
        // Nothing runs, and whoever arrived meanwhile was told it did: the
        // next `setitimer` tries again, as it always did after a failed
        // spawn. Only out of memory reaches here.
        *CLOCK_RUNNING.lock() = false;
    }
    ALARMS.wake_all();
}

/// The `itimers` thread: raise every `SIGALRM` that is due, then sleep until
/// the next one or until a deadline changes. Returns, ending the thread, when
/// no process has one armed.
fn run_alarm_clock(_argument: usize) {
    loop {
        let now = crate::timer::now_nanos();
        let mut next = u64::MAX;
        // With no memory for the list this round, the next finds the alarms.
        for process in registry::live().unwrap_or_default() {
            let (due, alarm) = process.with_signals(|signals| {
                let due = signals.tick_alarm(now);
                (due, signals.alarm())
            });
            if due {
                send(&process, SIGALRM, Origin::Kernel);
            }
            if alarm.deadline != 0 {
                next = next.min(alarm.deadline);
            }
        }
        if next == u64::MAX {
            let mut running = CLOCK_RUNNING.lock();
            if !ALARMS_CHANGED.swap(false, Ordering::AcqRel) {
                *running = false;
                return;
            }
            continue;
        }
        let _ = ALARMS.wait_until_deadline(|| ALARMS_CHANGED.swap(false, Ordering::AcqRel), next);
    }
}

/// `setitimer`.
///
/// A null new value disarms, as Linux has accepted since before it warned
/// about it. The new value is read before anything changes and the old one
/// written after, so a bad `old` pointer is `EFAULT` with the timer already
/// set, as on Linux.
///
/// # Errors
///
/// `EINVAL` for an unknown timer, a microsecond count of a second or more, a
/// negative time, or an attempt to arm `ITIMER_VIRTUAL` or `ITIMER_PROF`,
/// which nothing here counts; `EFAULT` for a bad pointer.
pub(crate) fn sys_setitimer(
    process: &Process,
    which: u32,
    new: u64,
    old: u64,
) -> Result<usize, Errno> {
    let (interval, value) = if new == 0 {
        (0, 0)
    } else {
        read_itimerval(process, new)?
    };
    match which {
        ITIMER_REAL => {}
        ITIMER_VIRTUAL | ITIMER_PROF if value == 0 => {
            return write_itimerval_if(process, old, 0, 0);
        }
        _ => return Err(Errno::EINVAL),
    }
    let now = crate::timer::now_nanos();
    let deadline = if value == 0 {
        0
    } else {
        now.saturating_add(value).max(1)
    };
    let previous = process.with_signals(|signals| signals.set_alarm(Alarm { deadline, interval }));
    alarms_changed();
    let (interval, remaining) = remaining(previous, now);
    write_itimerval_if(process, old, interval, remaining)
}

/// `getitimer`.
///
/// # Errors
///
/// `EINVAL` for an unknown timer; `EFAULT` for a bad pointer.
pub(crate) fn sys_getitimer(process: &Process, which: u32, at: u64) -> Result<usize, Errno> {
    let (interval, value) = match which {
        ITIMER_REAL => {
            let alarm = process.with_signals(|signals| signals.alarm());
            remaining(alarm, crate::timer::now_nanos())
        }
        ITIMER_VIRTUAL | ITIMER_PROF => (0, 0),
        _ => return Err(Errno::EINVAL),
    };
    write_itimerval(process, at, interval, value)?;
    Ok(0)
}

/// `alarm`: arm `ITIMER_REAL` for whole seconds, once, and answer the seconds
/// that were left on the one it replaced -- rounded to the nearer second, and
/// never zero for a timer that was armed, which is Linux's promise.
pub(crate) fn sys_alarm(process: &Process, seconds: u32) -> usize {
    let now = crate::timer::now_nanos();
    let deadline = if seconds == 0 {
        0
    } else {
        now.saturating_add(u64::from(seconds) * NANOS_PER_SECOND)
    };
    let previous = process.with_signals(|signals| {
        signals.set_alarm(Alarm {
            deadline,
            interval: 0,
        })
    });
    alarms_changed();
    let (_, left) = remaining(previous, now);
    let whole = left / NANOS_PER_SECOND;
    let rest = left % NANOS_PER_SECOND;
    let rounded = if (whole == 0 && rest != 0) || rest >= NANOS_PER_SECOND / 2 {
        whole + 1
    } else {
        whole
    };
    usize::try_from(rounded).unwrap_or(usize::MAX)
}

/// An alarm's interval and the time left on it at `now`, both in nanoseconds.
/// An armed timer that is due but not yet raised has a microsecond left, as on
/// Linux, so it does not read as disarmed.
fn remaining(alarm: Alarm, now: u64) -> (u64, u64) {
    if alarm.deadline == 0 {
        return (alarm.interval, 0);
    }
    (
        alarm.interval,
        alarm.deadline.saturating_sub(now).max(NANOS_PER_MICRO),
    )
}

/// Read a `struct itimerval` -- two `timeval`s of native words, interval first
/// -- as nanoseconds.
fn read_itimerval(process: &Process, at: u64) -> Result<(u64, u64), Errno> {
    let word = size_of::<usize>();
    let mut bytes = [0_u8; 32];
    let wanted = bytes.get_mut(..word * 4).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, wanted).map_err(|_| Errno::EFAULT)?;
    let field = |index: usize| -> i64 {
        let mut value = [0_u8; 8];
        if let (Some(slot), Some(source)) = (
            value.get_mut(..word),
            wanted.get(index * word..(index + 1) * word),
        ) {
            slot.copy_from_slice(source);
        }
        if word == 8 {
            i64::from_le_bytes(value)
        } else {
            i64::from(u64::from_le_bytes(value) as u32 as i32)
        }
    };
    let interval = timeval_nanos(field(0), field(1))?;
    let value = timeval_nanos(field(2), field(3))?;
    Ok((interval, value))
}

/// A `timeval`'s seconds and microseconds as nanoseconds.
fn timeval_nanos(seconds: i64, micros: i64) -> Result<u64, Errno> {
    let seconds = u64::try_from(seconds).map_err(|_| Errno::EINVAL)?;
    let micros = u64::try_from(micros).map_err(|_| Errno::EINVAL)?;
    if micros >= NANOS_PER_SECOND / NANOS_PER_MICRO {
        return Err(Errno::EINVAL);
    }
    Ok(seconds
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(micros * NANOS_PER_MICRO))
}

/// [`write_itimerval`], when there is somewhere to write it; zero either way.
fn write_itimerval_if(
    process: &Process,
    at: u64,
    interval: u64,
    value: u64,
) -> Result<usize, Errno> {
    if at != 0 {
        write_itimerval(process, at, interval, value)?;
    }
    Ok(0)
}

/// Write a `struct itimerval` of native words from nanoseconds.
fn write_itimerval(process: &Process, at: u64, interval: u64, value: u64) -> Result<(), Errno> {
    let word = size_of::<usize>();
    let fields = [
        interval / NANOS_PER_SECOND,
        interval % NANOS_PER_SECOND / NANOS_PER_MICRO,
        value / NANOS_PER_SECOND,
        value % NANOS_PER_SECOND / NANOS_PER_MICRO,
    ];
    let mut bytes = [0_u8; 32];
    for (index, field) in fields.iter().enumerate() {
        let slot = bytes
            .get_mut(index * word..(index + 1) * word)
            .ok_or(Errno::EINVAL)?;
        slot.copy_from_slice(field.to_le_bytes().get(..word).ok_or(Errno::EINVAL)?);
    }
    uaccess::copy_to_user(
        process.space(),
        at,
        bytes.get(..word * 4).ok_or(Errno::EINVAL)?,
    )
    .map_err(|_| Errno::EFAULT)
}
