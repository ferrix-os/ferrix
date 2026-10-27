//! Stage 13's `POLLPRI` check, landing G3 (`docs/CGROUPS.md` §4): a service
//! manager learns that a cgroup emptied by waiting on its `cgroup.events`.
//!
//! A cgroup gets two members, and its `cgroup.events`, read once, goes into
//! an epoll set asking for `EPOLLPRI`. A task waits on the set as
//! `epoll_wait` does. The first member's release leaves the cgroup populated
//! and the waiter asleep; the second's empties it, and the waiter comes back
//! with `EPOLLPRI` and the cookie, ended by the job's wake rather than its own
//! recheck. Then the file polls `POLLPRI` (and is in `select`'s exception
//! set) until it is read again, and not after.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::types::EPOLLPRI;
use ferrix_vfs::{OpenFile, Whence};

use super::{Checked, Harness};
use crate::fs::epoll::{self, Interest};
use crate::object::job::KILLED_STATUS;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::poll::{self, POLLERR, POLLIN, POLLPRI};
use crate::syscall::process;

/// What a populated cgroup's `cgroup.events` says.
const FULL: &[u8] = b"populated 1\nfrozen 0\n";
/// What an empty one's says.
const EMPTY: &[u8] = b"populated 0\nfrozen 0\n";

/// The cookie the registration carries, which the waiter must bring back.
const COOKIE: u64 = 0xC6_0E_7E;

/// How long the check waits for the waiter to get onto the job's queue, and
/// to come back after the release.
pub(super) const PATIENCE_NANOS: u64 = 5_000_000_000;
/// How long the waiter waits in all: longer than the check's patience, so a
/// failed check's waiter still goes.
const WAITER_DEADLINE_NANOS: u64 = 2 * PATIENCE_NANOS;
/// How long the waiter must stay asleep after a release that left the
/// cgroup populated.
const STILL_WAITING_NANOS: u64 = 20_000_000;
/// How often the check looks for the waiter on the queue.
pub(super) const LISTED_LOOK_NANOS: u64 = 1_000_000;

/// The epoll set the waiter waits on; the OOM check's waiter's too.
pub(super) static WAITER_SET: SpinLock<Option<Arc<OpenFile>>> = SpinLock::new(None);
/// What the waiter's wait delivered: the first event's bits and cookie, or
/// `None` if it gave up.
pub(super) static WAITER_ANSWER: SpinLock<Option<Option<(u32, u64)>>> = SpinLock::new(None);
/// Woken when the waiter has answered.
pub(super) static WAITER_DONE: WaitQueue = WaitQueue::new();

/// The check. Answers how many waits on `cgroup.events` a release's wake
/// ended: one.
///
/// # Errors
///
/// The first thing that was not as Linux has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u32> {
    harness
        .mkdir(b"/check-e")
        .map_err(|_| "mkdir of the cgroup.events check's cgroup failed")?;
    harness.report.made += 1;
    let first =
        process::new_for_check().map_err(|_| "could not make a process for the POLLPRI check")?;
    let second =
        process::new_for_check().map_err(|_| "could not make a process for the POLLPRI check")?;
    for member in [&first, &second] {
        let listed = alloc::format!("{}\n", member.pid());
        let _ = harness
            .write(b"/check-e/cgroup.procs", listed.as_bytes())
            .map_err(|_| "writing a pid to cgroup.procs failed")?;
    }

    let events = harness
        .open_read(b"/check-e/cgroup.events")
        .map_err(|_| "cgroup.events did not open")?;
    if !events.poll().priority {
        return Err(
            "a cgroup.events opened and not yet read did not poll POLLPRI, as Linux's does",
        );
    }
    if Harness::read_to_end(&events).as_deref() != Ok(FULL) {
        return Err("a cgroup with two members does not say it is populated");
    }
    if events.poll().priority {
        return Err("a cgroup.events just read, with nothing changed, polls POLLPRI");
    }

    let set = epoll::create().map_err(|_| "an epoll set was refused")?;
    let inner = epoll::of(&set).ok_or("an epoll set is not one")?;
    inner
        .add(
            0,
            &events,
            Interest {
                events: EPOLLPRI,
                data: COOKIE,
            },
        )
        .map_err(|_| "epoll refused to watch cgroup.events")?;
    if !inner.ready(4).is_empty() {
        return Err("an epoll on an unchanged cgroup.events reported an event");
    }

    let woken = wait_through_the_releases(&set, [&first, &second])?;
    drop((first, second));

    let ready = events.poll();
    if poll::revents(ready, POLLPRI | POLLIN) != POLLPRI | POLLIN | POLLERR
        || !poll::select_sets(ready)[2]
    {
        return Err(
            "an emptied cgroup's cgroup.events did not poll POLLPRI and POLLERR, \
             or was not in select's exception set",
        );
    }
    if events.seek(0, Whence::Set) != Ok(0) || Harness::read_to_end(&events).as_deref() != Ok(EMPTY)
    {
        return Err("cgroup.events read again from its start does not say the cgroup is empty");
    }
    let again = events.poll();
    if again.priority || poll::revents(again, POLLPRI) != 0 || poll::select_sets(again)[2] {
        return Err("cgroup.events still polled POLLPRI after it was read again");
    }
    if !inner.ready(4).is_empty() {
        return Err("an epoll on cgroup.events still reported EPOLLPRI after it was read again");
    }
    drop((set, events));
    harness
        .rmdir(b"/check-e")
        .map_err(|_| "rmdir of the emptied cgroup.events cgroup failed")?;
    Ok(woken)
}

/// Start the waiter on `set`, release `members` one at a time, and require
/// the waiter to sleep through the first and be woken by the last with
/// `EPOLLPRI` and the cookie. Answers how many of its waits a wake ended: the
/// job's, through the registration's mark on the set's queue.
fn wait_through_the_releases(
    set: &Arc<OpenFile>,
    members: [&Arc<process::Process>; 2],
) -> Checked<u32> {
    let [first, last] = members;
    // The waiter sleeps on the set's queue, which the job's wake wakes
    // through the registration's mark (`fs::epoll`'s ready list).
    let watching = epoll::of(set).ok_or("an epoll set is not one")?;
    *WAITER_ANSWER.lock() = None;
    *WAITER_SET.lock() = Some(Arc::clone(set));
    let ended_before = watching.queue().waits_ended_by_a_wake();
    let waiter = crate::sched::spawn(
        "cgroup-events-waiter",
        waiter,
        0,
        ferrix_sched_fair::NICE_0_WEIGHT,
    )?;
    // The release must find the waiter on the queue: a fixed sleep would
    // only assume it got there (see the eventfd check's `until_listed`).
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while watching.queue().listed() == 0
        && WAITER_ANSWER.lock().is_none()
        && crate::timer::now_nanos() < deadline
    {
        crate::sched::sleep_for(LISTED_LOOK_NANOS);
    }

    process::kill(first, KILLED_STATUS);
    crate::sched::sleep_for(STILL_WAITING_NANOS);
    if WAITER_ANSWER.lock().is_some() {
        return Err("an epoll on cgroup.events woke while the cgroup was still populated");
    }
    process::kill(last, KILLED_STATUS);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let _ = WAITER_DONE.wait_until_deadline(|| WAITER_ANSWER.lock().is_some(), deadline);
    let answer = WAITER_ANSWER.lock().take();
    *WAITER_SET.lock() = None;
    if answer.is_none() {
        return Err("an epoll waiting on cgroup.events never came back after the last release");
    }
    crate::sched::wait_until_gone(&waiter, crate::sched::REAPER_PATIENCE_NANOS)?;
    drop(waiter);
    match answer.flatten() {
        Some((bits, COOKIE)) if bits & EPOLLPRI != 0 => {}
        Some(_) => return Err("an epoll on cgroup.events woke without EPOLLPRI or its cookie"),
        None => {
            return Err("an epoll on cgroup.events did not report EPOLLPRI at the last release");
        }
    }
    let ended = watching
        .queue()
        .waits_ended_by_a_wake()
        .wrapping_sub(ended_before);
    if ended == 0 {
        return Err(
            "an epoll waiting on cgroup.events was ended by its recheck, not by the release's wake",
        );
    }
    Ok(ended)
}

/// The waiting task: `epoll_wait`'s loop on [`WAITER_SET`], with no process
/// behind it, until an event or its deadline.
pub(super) fn waiter(_argument: usize) {
    // Bound first: a guard in the expression below would live through the
    // wait, and a task that sleeps must hold no spin lock.
    let subject = WAITER_SET.lock().clone();
    let answer = subject.and_then(|file| {
        let set = epoll::of(&file)?;
        let deadline = crate::timer::now_nanos().saturating_add(WAITER_DEADLINE_NANOS);
        loop {
            let ready: Vec<epoll::Event> = set.ready(1);
            if let Some(event) = ready.first() {
                set.delivered(&ready);
                return Some((event.events, event.data));
            }
            if crate::timer::now_nanos() >= deadline {
                return None;
            }
            // On the set's queue, which the job's wake wakes through the
            // registration's mark, as `epoll_wait` sleeps.
            let _ = crate::fs::wake::wait_on(
                set.queue(),
                || !set.ready(1).is_empty(),
                deadline,
                set.trusted(),
            );
        }
    });
    *WAITER_ANSWER.lock() = Some(answer);
    WAITER_DONE.wake_all();
}
