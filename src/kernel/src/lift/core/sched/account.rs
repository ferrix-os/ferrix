//! A program's processor time (docs/microkernel/M5-LIFT.md §4.7): what the
//! hybrid's scheduler charges at a tick and a switch, the restricted loop charges
//! at every crossing here, from the counter the clock is made of.
//!
//! Time in the program is wall time between entry and exit: the kernel may run
//! something else on the processor meanwhile, and this does not see it. Under
//! contention the figure is high, and `cpu.max` holds a group to less than it asked.
//! Time in linuxd is the same less what it spent parked.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::Task;
use crate::object::quota;
use crate::sync::SpinLock;

/// Charged at most this far behind: about a millisecond.
const CHARGE_EVERY_CYCLES: u64 = 3_000_000;

/// Every task that has entered a program and not ended.
static RUNNING: SpinLock<Vec<Arc<Task>>> = SpinLock::new(Vec::new());
static THROTTLED: super::WaitQueue = super::WaitQueue::new();

#[derive(Debug)]
pub struct Time {
    entered: AtomicU64,
    left: AtomicU64,
    parked: AtomicU64,
    in_program: AtomicU64,
    in_linuxd: AtomicU64,
    runtime: AtomicU64,
    kernel_time: AtomicU64,
}

impl Time {
    pub const fn new() -> Time {
        Time {
            entered: AtomicU64::new(0),
            left: AtomicU64::new(0),
            parked: AtomicU64::new(0),
            in_program: AtomicU64::new(0),
            in_linuxd: AtomicU64::new(0),
            runtime: AtomicU64::new(0),
            kernel_time: AtomicU64::new(0),
        }
    }
    pub fn runtime(&self) -> u64 {
        self.runtime.load(Ordering::Relaxed)
    }
    pub fn kernel_time(&self) -> u64 {
        self.kernel_time.load(Ordering::Relaxed)
    }
}

fn cycles() -> u64 {
    crate::arch::counter_now()
}

fn nanos_of(amount: u64) -> u64 {
    ferrix_vdso::counter_nanos(amount, crate::arch::counter_hz())
}

fn charge(task: &Task) {
    let in_program = task.time.in_program.swap(0, Ordering::Relaxed);
    let in_linuxd = task.time.in_linuxd.swap(0, Ordering::Relaxed);
    let now = crate::timer::now_nanos();
    let user = nanos_of(in_program);
    let system = nanos_of(in_linuxd);
    let _ = task
        .time
        .runtime
        .fetch_add(user + system, Ordering::Relaxed);
    let _ = task.time.kernel_time.fetch_add(system, Ordering::Relaxed);
    let group = task.group.load(Ordering::Relaxed);
    let mut pending = quota::Pending::new();
    if user != 0 {
        quota::charge_cpu(&mut pending, group, now, user, true);
    }
    if system != 0 {
        quota::charge_cpu(&mut pending, group, now, system, false);
    }
    quota::settle(&mut pending);
}

/// A task's first entry: from here on it is in the list and on the counters.
pub fn starts_program(task: &Arc<Task>) {
    task.time.left.store(cycles(), Ordering::Relaxed);
    RUNNING.lock().push(Arc::clone(task));
}

/// The program stopped running: its time so far is known, and so is its group,
/// which a write to `cgroup.procs` may have changed since its last call.
pub fn left_program() {
    let _ = super::with_current(|task| {
        let now = cycles();
        // A swap: `charge_running` may be charging this same stretch from outside.
        let since = task.time.entered.swap(now, Ordering::AcqRel);
        let _ = task
            .time
            .in_program
            .fetch_add(now.saturating_sub(since), Ordering::Relaxed);
        task.time.left.store(now, Ordering::Relaxed);
        if let Some(thread) = task.thread() {
            let slot = thread.process().core().quota_slot();
            if slot != task.group.load(Ordering::Relaxed) {
                charge(task);
                task.set_group(slot);
            }
        }
    });
}

/// The program is about to run again. Past its group's `cpu.max`, it waits for
/// the period's end first, unless something asks the thread to attend.
pub fn entering() {
    let _ = super::with_current(|task| {
        task.note_no_call();
        let now = cycles();
        let spent = now
            .saturating_sub(task.time.left.load(Ordering::Relaxed))
            .saturating_sub(task.time.parked.swap(0, Ordering::Relaxed));
        let _ = task.time.in_linuxd.fetch_add(spent, Ordering::Relaxed);
        let unsettled = task.time.in_program.load(Ordering::Relaxed)
            + task.time.in_linuxd.load(Ordering::Relaxed);
        let bandwidth = quota::bandwidth_in_use();
        if bandwidth || unsettled > CHARGE_EVERY_CYCLES {
            charge(task);
        }
        if bandwidth {
            throttle(task);
            task.time.entered.store(cycles(), Ordering::Relaxed);
        } else {
            // The clock just read: a charge here is smaller than the counter's cost.
            task.time.entered.store(now, Ordering::Relaxed);
        }
    });
}

fn throttle(task: &Task) {
    let group = task.group.load(Ordering::Relaxed);
    if group == quota::NONE {
        return;
    }
    let leave = || crate::trap::return_path().is_some_and(crate::trap::attention_due);
    loop {
        let now = crate::timer::now_nanos();
        let Some(until) = quota::throttled_until(group, now) else {
            return;
        };
        let _ = THROTTLED.wait_until_deadline(
            || leave() || quota::throttled_until(group, crate::timer::now_nanos()).is_none(),
            until,
        );
        if leave() {
            return;
        }
    }
}

/// What a park took: not this task's processor time.
pub fn parked(cycles_parked: u64) {
    let _ = super::with_current(|task| {
        let _ = task.time.parked.fetch_add(cycles_parked, Ordering::Relaxed);
    });
}
pub fn parking() -> u64 {
    cycles()
}
pub fn unparked(since: u64) {
    parked(cycles().saturating_sub(since));
}

/// Whatever the calling task has run, charged now.
pub fn settle_own() {
    let _ = super::with_current(charge);
}

/// The calling task ends.
pub fn left_for_good() {
    let Some(task) = super::current() else {
        return;
    };
    charge(&task);
    RUNNING.lock().retain(|other| !Arc::ptr_eq(other, &task));
}

/// Charge what each task has run so far, from outside for one in its program,
/// which no tick interrupts here and a loop with no call never leaves. One over
/// its group's `cpu.max` is kicked out, and held at its way back in.
pub fn charge_running() {
    settle_own();
    let bandwidth = quota::bandwidth_in_use();
    for task in RUNNING.lock().iter() {
        let at = cycles();
        let entered = task.time.entered.load(Ordering::Acquire);
        if entered <= task.time.left.load(Ordering::Acquire) {
            continue;
        }
        if task
            .time
            .entered
            .compare_exchange(entered, at, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        let _ = task
            .time
            .in_program
            .fetch_add(at.saturating_sub(entered), Ordering::Relaxed);
        charge(task);
        if bandwidth
            && quota::throttled_until(
                task.group.load(Ordering::Relaxed),
                crate::timer::now_nanos(),
            )
            .is_some()
            && task.in_restricted.load(Ordering::Acquire)
        {
            crate::sys::kick(task.thread_handle.load(Ordering::Acquire));
        }
    }
}

/// `cpu.max` needs a look at programs that do not call: a thread of its own,
/// awake often only while some group has one.
pub fn start_keeper() {
    let keeper: fn(usize) = |_| loop {
        let using = quota::bandwidth_in_use();
        if using {
            charge_running();
        }
        super::sleep_for(if using { 5_000_000 } else { 200_000_000 });
    };
    let _ = super::spawn("cpu.max", keeper, 0, 0);
}
