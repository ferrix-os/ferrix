//! `epoll_create1`, `epoll_create`, `epoll_ctl`, `epoll_wait`, `epoll_pwait`
//! and `epoll_pwait2`.
//!
//! The set is [`crate::fs::epoll`]; this is the calls around it, in Linux's
//! order of checks, which a program can observe: `epoll_ctl` reads the event
//! before it looks up either descriptor, and asks whether the target can be
//! waited on (`EPERM`) before whether the set is a set (`EINVAL`); a wait
//! checks `maxevents` and the buffer's range before the descriptor.
//!
//! A wait sleeps on the set's own queue, which a wake of any registered
//! file's queue wakes (the set's ready list, [`crate::fs::epoll`]), until it
//! is woken, the timeout passes or a signal arrives. A signal ends it with `EINTR`, never a
//! restart, because `epoll_wait` is one of the calls Linux does not restart
//! even under `SA_RESTART`.

use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EPOLLERR, EPOLLET, EPOLLEXCLUSIVE,
    EPOLLHUP, EPOLLIN, EPOLLOUT, EPOLLWAKEUP,
};
use ferrix_vfs::OpenFile;

use crate::fs;
use crate::fs::epoll::{self, Event, Interest};
use crate::syscall::fd;
use crate::syscall::poll;
use crate::syscall::process::Process;
use crate::syscall::thread::Thread;
use crate::syscall::time::TimeWidth;
use crate::syscall::uaccess;

/// Bytes in a `struct epoll_event`: packed to 12 on x86-64, 16 elsewhere.
pub(crate) const EVENT_BYTES: usize = crate::arch::EPOLL_EVENT_BYTES;

/// Where `data` lies in a `struct epoll_event`.
const DATA_AT: usize = EVENT_BYTES - 8;

/// The most events one wait may ask for: Linux's `EP_MAX_EVENTS`.
const MAX_EVENTS: usize = i32::MAX as usize / EVENT_BYTES;

/// The flags `EPOLLEXCLUSIVE` may be combined with.
const EXCLUSIVE_OK: u32 =
    EPOLLIN | EPOLLOUT | EPOLLERR | EPOLLHUP | EPOLLWAKEUP | EPOLLET | EPOLLEXCLUSIVE;

/// Nanoseconds in a millisecond.
const NANOS_PER_MILLI: u64 = 1_000_000;

/// `epoll_create1`.
///
/// # Errors
///
/// `EINVAL` for a flag other than `EPOLL_CLOEXEC`; `EMFILE` for a full table.
pub(crate) fn sys_epoll_create1(process: &Process, flags: u32) -> Result<usize, Errno> {
    if flags & !EPOLL_CLOEXEC != 0 {
        return Err(Errno::EINVAL);
    }
    let set = epoll::create()?;
    let fd = process
        .files()
        .lock()
        .insert(set, flags & EPOLL_CLOEXEC != 0)?;
    usize::try_from(fd).map_err(|_| Errno::EMFILE)
}

/// `epoll_create`: a size that is not positive is `EINVAL`, and any other is
/// ignored, as it has been since Linux 2.6.8.
///
/// # Errors
///
/// As [`sys_epoll_create1`].
pub(crate) fn sys_epoll_create(process: &Process, size: i32) -> Result<usize, Errno> {
    if size <= 0 {
        return Err(Errno::EINVAL);
    }
    sys_epoll_create1(process, 0)
}

/// `epoll_ctl`.
///
/// # Errors
///
/// `EFAULT` for an unreadable event on an operation that takes one; `EBADF`
/// for a descriptor that names nothing; `EPERM` for a target that cannot be
/// waited on, a regular file or a directory; `EINVAL` for a set that is not
/// one, a set added to itself, an unknown operation or a misuse of
/// `EPOLLEXCLUSIVE`; `ELOOP` for a set that would nest in itself or too deep;
/// `EEXIST` and `ENOENT` for a registration that is or is not there.
pub(crate) fn sys_epoll_ctl(
    process: &Process,
    epfd: i32,
    op: u32,
    fd: i32,
    event: u64,
) -> Result<usize, Errno> {
    let interest = if op == EPOLL_CTL_DEL {
        None
    } else {
        Some(read_event(process, event)?)
    };
    let set_file = waitable(process, epfd)?;
    let target = waitable(process, fd)?;
    if target.poll_changes().is_none() {
        return Err(Errno::EPERM);
    }
    let set = epoll::of(&set_file)
        .filter(|_| !Arc::ptr_eq(&set_file, &target))
        .ok_or(Errno::EINVAL)?;
    if let Some(interest) = interest
        && interest.events & EPOLLEXCLUSIVE != 0
    {
        if op == EPOLL_CTL_MOD {
            return Err(Errno::EINVAL);
        }
        if op == EPOLL_CTL_ADD
            && (epoll::of(&target).is_some() || interest.events & !EXCLUSIVE_OK != 0)
        {
            return Err(Errno::EINVAL);
        }
    }
    match (op, interest) {
        (EPOLL_CTL_ADD, Some(interest)) => set.add(fd, &target, interest)?,
        (EPOLL_CTL_MOD, Some(interest)) => set.modify(fd, &target, interest)?,
        (EPOLL_CTL_DEL, _) => set.remove(fd, &target)?,
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}

/// `epoll_wait`: `timeout` in milliseconds, negative for no limit.
///
/// # Errors
///
/// `EINVAL` for `maxevents` not positive or past `EP_MAX_EVENTS`, or a
/// descriptor that is not a set; `EFAULT` for a buffer outside the program's
/// half, or one no event could be written into; `EBADF` for a descriptor that
/// names nothing; `EINTR` when a signal arrives first.
pub(crate) fn sys_epoll_wait(
    process: &Process,
    epfd: i32,
    events: u64,
    maxevents: i32,
    timeout: i32,
) -> Result<usize, Errno> {
    let deadline = u64::try_from(timeout)
        .ok()
        .map(|millis| now().saturating_add(millis.saturating_mul(NANOS_PER_MILLI)));
    wait(process, epfd, events, maxevents, deadline)
}

/// `epoll_pwait`: as `epoll_wait`, under the signal mask at `sigmask` if
/// there is one.
///
/// # Errors
///
/// `EINVAL` for a signal set of the wrong size and `EFAULT` for an unreadable
/// one, before anything else; then as [`sys_epoll_wait`].
pub(crate) fn sys_epoll_pwait(
    thread: &Thread,
    [epfd, events, maxevents, timeout, sigmask, sigsetsize]: [u64; 6],
) -> Result<usize, Errno> {
    let process = thread.process();
    let mask = poll::read_sigset(process, sigmask, sigsetsize)?;
    poll::with_sigmask(thread, mask, || {
        sys_epoll_wait(
            process,
            fd::arg(epfd),
            events,
            fd::arg(maxevents),
            fd::arg(timeout),
        )
    })
}

/// `epoll_pwait2`: as `epoll_pwait`, with `timeout` a `struct timespec` of two
/// 64-bit words on every architecture, or null for no limit.
///
/// # Errors
///
/// `EFAULT` for an unreadable timeout and `EINVAL` for a malformed one, first;
/// then as [`sys_epoll_pwait`].
pub(crate) fn sys_epoll_pwait2(
    thread: &Thread,
    [epfd, events, maxevents, timeout, sigmask, sigsetsize]: [u64; 6],
) -> Result<usize, Errno> {
    let process = thread.process();
    let deadline = if timeout == 0 {
        None
    } else {
        let nanos = poll::read_timespec(process, timeout, TimeWidth::Wide)?;
        Some(now().saturating_add(nanos))
    };
    let mask = poll::read_sigset(process, sigmask, sigsetsize)?;
    poll::with_sigmask(thread, mask, || {
        wait(process, fd::arg(epfd), events, fd::arg(maxevents), deadline)
    })
}

/// The wait all three take.
fn wait(
    process: &Process,
    epfd: i32,
    events: u64,
    maxevents: i32,
    deadline: Option<u64>,
) -> Result<usize, Errno> {
    let limit = usize::try_from(maxevents)
        .ok()
        .filter(|&limit| limit > 0 && limit <= MAX_EVENTS)
        .ok_or(Errno::EINVAL)?;
    let bytes = u64::try_from(limit.saturating_mul(EVENT_BYTES)).map_err(|_| Errno::EINVAL)?;
    if !uaccess::is_user_range(events, bytes) {
        return Err(Errno::EFAULT);
    }
    let file = waitable(process, epfd)?;
    let set = epoll::of(&file).ok_or(Errno::EINVAL)?;
    loop {
        let ready = set.ready(limit);
        if !ready.is_empty() {
            return deliver(process, &set, events, &ready);
        }
        if deadline.is_some_and(|at| now() >= at) {
            return Ok(0);
        }
        if process.signal_pending() {
            return Err(Errno::EINTR);
        }
        // Asleep on the set's queue, which a wake of any registered file's
        // queue wakes, until it is woken, a signal wakes the task or the
        // deadline passes. A look in between is a set with something to
        // report; the loop asks again for what, and delivers it. So is a
        // look that found the set no longer trusted: the loop sleeps again
        // with the shorter recheck.
        let trusted = set.trusted();
        let _ = fs::wake::wait_on(
            set.queue(),
            || !set.ready(1).is_empty() || process.signal_pending() || set.trusted() != trusted,
            deadline.unwrap_or(u64::MAX),
            trusted,
        );
    }
}

/// Write `ready` into the program's buffer, one event after another, and
/// record as delivered the ones that were written: a write that fails stops
/// there and leaves the rest due, and a wait that wrote none is `EFAULT`, as
/// `ep_send_events` answers.
fn deliver(
    process: &Process,
    set: &epoll::Epoll,
    at: u64,
    ready: &[Event],
) -> Result<usize, Errno> {
    let mut written = 0;
    for event in ready {
        let offset = u64::try_from(written * EVENT_BYTES).map_err(|_| Errno::EINVAL)?;
        let encoded = encode(event.events, event.data);
        if uaccess::copy_to_user(process.space(), at.saturating_add(offset), &encoded).is_err() {
            break;
        }
        written += 1;
    }
    set.delivered(ready.get(..written).unwrap_or(ready));
    if written == 0 {
        return Err(Errno::EFAULT);
    }
    Ok(written)
}

/// A `struct epoll_event` in this architecture's layout.
pub(crate) fn encode(events: u32, data: u64) -> [u8; EVENT_BYTES] {
    let mut bytes = [0_u8; EVENT_BYTES];
    for (slot, byte) in bytes.iter_mut().zip(events.to_le_bytes()) {
        *slot = byte;
    }
    for (slot, byte) in bytes.iter_mut().skip(DATA_AT).zip(data.to_le_bytes()) {
        *slot = byte;
    }
    bytes
}

/// Read the program's `struct epoll_event`.
fn read_event(process: &Process, at: u64) -> Result<Interest, Errno> {
    let mut bytes = [0_u8; EVENT_BYTES];
    uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut events = [0_u8; 4];
    let mut data = [0_u8; 8];
    for (slot, byte) in events.iter_mut().zip(bytes.iter()) {
        *slot = *byte;
    }
    for (slot, byte) in data.iter_mut().zip(bytes.iter().skip(DATA_AT)) {
        *slot = *byte;
    }
    Ok(Interest {
        events: u32::from_le_bytes(events),
        data: u64::from_le_bytes(data),
    })
}

/// The file a descriptor names, as `fdget` finds one: `EBADF` for nothing,
/// and for an `O_PATH` descriptor, which `fdget` does not see.
fn waitable(process: &Process, descriptor: i32) -> Result<Arc<OpenFile>, Errno> {
    let file = fd::file(process, descriptor)?;
    if file.is_path() {
        return Err(Errno::EBADF);
    }
    Ok(file)
}

/// The counter's reading, in nanoseconds.
fn now() -> u64 {
    crate::timer::now_nanos()
}
