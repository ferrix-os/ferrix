//! The self-check of a wait queue's marks: what an epoll set's ready list
//! rests on (`sched::wait`'s "Marks", `fs::epoll`).
//!
//! Three cases, one per requirement:
//!
//! * **Told** (L.sched.74): a mark hooked on a queue is told once by each of
//!   three wakes, by none once it is taken off, and told it is gone, with
//!   nothing woken, when the queue is dropped.
//! * **Published** (L.sched.75): a waker on another processor is held in its
//!   wake just before the wake's waiter-lock section (`wait::arm_hook_meet`)
//!   while this task hooks a mark on the queue; let go, the wake tells it.
//!   Reading the count of marks before that section instead -- outside the
//!   lock the hooker stores it under -- misses the mark, and the case says so.
//! * **Listed once** (L.sched.76): a mark told twice before its taker clears
//!   it is listed once, and wakes its list's queue once; another mark of the
//!   list told before the next take wakes it no more, one told after does; a
//!   list with no room left is flagged overflowed rather than grown.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use super::WaitQueue;
use super::wait::{self, Mark, ReadyList};
use crate::fallible;
use crate::smp::Topology;

/// How long a case waits for a task it started.
const PATIENCE_NANOS: u64 = 5_000_000_000;

/// The queue the published case's waker wakes, and nothing else does.
static PUBLISHED_QUEUE: WaitQueue = WaitQueue::new();

/// Set once the meet is armed: the waker waits for it before it wakes.
static PUBLISHED_ARMED: AtomicBool = AtomicBool::new(false);

/// Set by the waker once its wake is over.
static PUBLISHED_DONE: AtomicU64 = AtomicU64::new(0);

/// What the checker waits for the waker on.
static PUBLISHED_FINISHED: WaitQueue = WaitQueue::new();

/// Run the three cases.
///
/// # Errors
///
/// The sentence of the first case that failed.
pub(super) fn run(topology: &Topology) -> Result<(), &'static str> {
    marks_are_told()?;
    a_mark_hooked_before_a_wake_is_told(topology)?;
    a_mark_is_listed_once()?;
    Ok(())
}

/// A list with room for `room` numbers, and a mark on it numbered `serial`.
fn list_and_mark(room: usize, serial: u64) -> Result<(Arc<ReadyList>, Arc<Mark>), &'static str> {
    let list = ReadyList::new().map_err(|_| "marks: no memory for a ready list")?;
    let _ = list
        .reserve(room)
        .map_err(|_| "marks: no memory for a ready list's room")?;
    let mark = Mark::new(&list, serial).map_err(|_| "marks: no memory for a mark")?;
    Ok((list, mark))
}

/// Take what `list` holds, clearing each mark as a taker does.
fn take(list: &ReadyList, marks: &[&Arc<Mark>]) -> Result<(Vec<u64>, bool), &'static str> {
    let mut into = fallible::try_with_capacity(16).map_err(|_| "marks: no memory to take into")?;
    let overflowed = list.take(&mut into);
    for mark in marks {
        mark.clear();
    }
    Ok((into, overflowed))
}

/// Every wake tells a mark hooked on its queue once; a mark taken off is
/// told by none; a queue dropped tells its mark it is gone and wakes nothing.
///
/// Verifies: L.sched.74
fn marks_are_told() -> Result<(), &'static str> {
    let (list, mark) = list_and_mark(4, 74)?;
    let queue = WaitQueue::new();
    queue
        .hook(&mark)
        .map_err(|_| "marks: no memory to hook a mark")?;
    for _ in 0..3 {
        let before = list.tells();
        queue.wake_all();
        if list.tells() != before.wrapping_add(1) {
            return Err("a wake did not tell the mark hooked on its queue exactly once");
        }
        let (taken, _) = take(&list, &[&mark])?;
        if taken.as_slice() != [74] {
            return Err("a wake's telling did not put the mark's number on its list");
        }
    }
    queue.unhook(&mark);
    let before = list.tells();
    queue.wake_all();
    if list.tells() != before || queue.hooks() != 0 {
        return Err("a mark taken off its queue was told by a later wake");
    }

    let going = fallible::try_arc(WaitQueue::new()).map_err(|_| "marks: no memory for a queue")?;
    going
        .hook(&mark)
        .map_err(|_| "marks: no memory to hook a mark")?;
    let (tells, woken) = (list.tells(), list.queue().wakes());
    drop(going);
    if list.tells() != tells.wrapping_add(1) || !mark.is_listed() {
        return Err("a queue dropped did not tell the mark hooked on it that it was gone");
    }
    if list.queue().wakes() != woken {
        return Err("a queue dropped woke its mark's list: a going wakes nothing");
    }
    let (taken, _) = take(&list, &[&mark])?;
    if taken.as_slice() != [74] {
        return Err("a queue dropped did not put its mark's number on its list");
    }
    crate::console::println!(
        "  tasks    a mark on a queue told once by each of 3 wakes, by none once taken off, and gone, waking nothing, when the queue is dropped"
    );
    Ok(())
}

/// A mark hooked while a wake on another processor has not yet begun its
/// waiter-lock section is told by that wake.
///
/// Verifies: L.sched.75
fn a_mark_hooked_before_a_wake_is_told(topology: &Topology) -> Result<(), &'static str> {
    let here = super::current()
        .ok_or("the checking task is not running")?
        .cpu();
    let Some(there) = (0..topology.online()).find(|&cpu| cpu != here) else {
        crate::console::println!(
            "  tasks    a mark hooked as a wake begins: not run, one processor"
        );
        return Ok(());
    };
    let (list, mark) = list_and_mark(4, 75)?;
    PUBLISHED_ARMED.store(false, Ordering::Release);
    PUBLISHED_DONE.store(0, Ordering::Release);
    let waker = super::spawn_on(
        "check-hook-meet",
        published_waker,
        0,
        NICE_0_WEIGHT,
        there,
        CpuSet::of(there),
    )?;
    wait::arm_hook_meet(waker.id, &PUBLISHED_QUEUE);
    PUBLISHED_ARMED.store(true, Ordering::Release);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !wait::hook_meet_arrived() && crate::timer::now_nanos() < deadline {
        core::hint::spin_loop();
    }
    let arrived = wait::hook_meet_arrived();
    let hooked = arrived && PUBLISHED_QUEUE.hook(&mark).is_ok();
    let tells = list.tells();
    wait::release_hook_meet();
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let finished = PUBLISHED_FINISHED
        .wait_until_deadline(|| PUBLISHED_DONE.load(Ordering::Acquire) != 0, deadline);
    PUBLISHED_QUEUE.unhook(&mark);
    if !finished {
        return Err("the hook-meet waker never finished its wake");
    }
    super::wait_until_gone(&waker, super::REAPER_PATIENCE_NANOS)?;
    drop(waker);
    if !arrived {
        return Err("the hook-meet waker never reached the meet before its waiter-lock section");
    }
    if !hooked {
        return Err("marks: no memory to hook a mark");
    }
    if list.tells() == tells || list.listed() != 1 {
        return Err("a mark hooked before a wake's waiter-lock section was not told by that wake");
    }
    crate::console::println!(
        "  tasks    a mark hooked while a wake on another processor waited to begin was told by it"
    );
    Ok(())
}

/// The published case's waker: once the meet is armed, one wake of
/// [`PUBLISHED_QUEUE`], held in the meet.
fn published_waker(_argument: usize) {
    while !PUBLISHED_ARMED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    PUBLISHED_QUEUE.wake_all();
    PUBLISHED_DONE.store(1, Ordering::Release);
    PUBLISHED_FINISHED.wake_all();
}

/// A mark is on its list once until its taker clears it, and wakes the
/// list's queue only the first time; a full list is flagged, not grown.
///
/// Verifies: L.sched.76
fn a_mark_is_listed_once() -> Result<(), &'static str> {
    let (list, mark) = list_and_mark(4, 76)?;
    let queue = WaitQueue::new();
    queue
        .hook(&mark)
        .map_err(|_| "marks: no memory to hook a mark")?;
    let woken = list.queue().wakes();
    queue.wake_all();
    if list.queue().wakes() != woken.wrapping_add(1) {
        queue.unhook(&mark);
        return Err("a mark told of a wake did not wake its list's queue");
    }
    queue.wake_all();
    let listed = list.listed();
    let woken_twice = list.queue().wakes() != woken.wrapping_add(1);
    let (taken, overflowed) = take(&list, &[&mark])?;
    queue.unhook(&mark);
    if listed != 1 || taken.as_slice() != [76] || overflowed {
        return Err("a mark told twice before its taker cleared it was listed twice");
    }
    if woken_twice {
        return Err("a mark told twice before its taker cleared it woke its list's queue twice");
    }
    // Two marks of one list: the second, told before the next take, wakes
    // nothing more; after the take the first wakes it again.
    let other = Mark::new(&list, 77).map_err(|_| "marks: no memory for a mark")?;
    let beside = WaitQueue::new();
    let hooked = queue.hook(&mark).is_ok() && beside.hook(&other).is_ok();
    let woken = list.queue().wakes();
    queue.wake_all();
    beside.wake_all();
    let once = list.queue().wakes() == woken.wrapping_add(1);
    let _ = take(&list, &[&mark, &other])?;
    queue.wake_all();
    let again = list.queue().wakes() == woken.wrapping_add(2);
    let _ = take(&list, &[&mark, &other])?;
    queue.unhook(&mark);
    beside.unhook(&other);
    if !hooked {
        return Err("marks: no memory to hook a mark");
    }
    if !once {
        return Err("two marks of one list told before a take woke its queue other than once");
    }
    if !again {
        return Err("a mark told of a wake after its list was taken did not wake its list's queue");
    }

    // A list one place short: every mark but the last finds room.
    let list = ReadyList::new().map_err(|_| "marks: no memory for a ready list")?;
    let room = list
        .reserve(1)
        .map_err(|_| "marks: no memory for a ready list's room")?;
    let mut marks: Vec<Arc<Mark>> =
        fallible::try_with_capacity(room + 1).map_err(|_| "marks: no memory for marks")?;
    for serial in 0..=room {
        let made = Mark::new(&list, u64::try_from(serial).unwrap_or(u64::MAX))
            .map_err(|_| "marks: no memory for a mark")?;
        let _ = fallible::push_within(&mut marks, made);
    }
    let crowded = WaitQueue::new();
    let mut hooked = true;
    for made in &marks {
        hooked &= crowded.hook(made).is_ok();
    }
    crowded.wake_all();
    let mut into =
        fallible::try_with_capacity(room + 1).map_err(|_| "marks: no memory to take into")?;
    let overflowed = list.take(&mut into);
    for made in &marks {
        crowded.unhook(made);
    }
    if !hooked {
        return Err("marks: no memory to hook a mark");
    }
    if !overflowed || into.len() != room {
        return Err("a full ready list did not flag that it overflowed");
    }
    crate::console::println!(
        "  tasks    a mark told twice listed once, waking its list once; {} marks on a list of {room} flagged it overflowed",
        room + 1
    );
    Ok(())
}
