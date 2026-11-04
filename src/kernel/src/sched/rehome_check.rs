//! The scheduler's switch counts, with kernel tasks alone: no process and no
//! personality. Its caller is `syscall::rehome_check`, and it builds
//! in the core's crate, so that boundary itself says it names no load
//! (`docs/microkernel/B5.md`, class A).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use crate::sched::{self, Task};

/// How long the check's tasks have to end.
const PATIENCE_NANOS: u64 = 30_000_000_000;
/// Yields each yielding task makes, once both have arrived.
const YIELDS: u32 = 20;
/// Sleeps the sleeping task makes.
const SLEEPS: u32 = 10;

/// The yielding tasks that have started.
static ARRIVED: AtomicU32 = AtomicU32::new(0);

/// Yield to the other task, still runnable each time.
fn yielder(_argument: usize) {
    let _ = ARRIVED.fetch_add(1, Ordering::AcqRel);
    while ARRIVED.load(Ordering::Acquire) < 2 {
        sched::yield_now();
    }
    for _ in 0..YIELDS {
        sched::yield_now();
    }
}

/// Sleep, each time not runnable.
fn sleeper(_argument: usize) {
    for _ in 0..SLEEPS {
        sched::sleep_for(100_000);
    }
}

/// A task on `cpu` alone, in no job.
fn pinned(name: &'static str, entry: fn(usize), cpu: usize) -> Result<Arc<Task>, &'static str> {
    sched::spawn_on(name, entry, 0, NICE_0_WEIGHT, cpu, CpuSet::of(cpu))
}

/// Two tasks yielding to each other count every switch away as involuntary but
/// the one at their end; a task that sleeps counts each sleep as voluntary.
fn check_switches_are_counted(cpu: usize) -> Result<(), &'static str> {
    ARRIVED.store(0, Ordering::Release);
    let pair = [
        pinned("rh-yield-1", yielder, cpu)?,
        pinned("rh-yield-2", yielder, cpu)?,
    ];
    for task in &pair {
        sched::wait_until_gone(task, PATIENCE_NANOS)?;
    }
    for task in &pair {
        let (voluntary, involuntary) = (task.voluntary_switches(), task.involuntary_switches());
        if voluntary != 1 {
            return Err("a yielding task counted other than one voluntary switch, at its end");
        }
        if voluntary + involuntary != task.switches() {
            return Err("a task's two switch counts do not add to how often it was switched to");
        }
    }
    // At least one between the two: a task may run its yields out alone if the
    // other has not yet been switched to, and then only that other counts one.
    if pair
        .iter()
        .map(|task| task.involuntary_switches())
        .sum::<u64>()
        == 0
    {
        return Err("two tasks that yielded, still runnable, counted no involuntary switch");
    }
    let sleeping = pinned("rh-sleep", sleeper, cpu)?;
    sched::wait_until_gone(&sleeping, PATIENCE_NANOS)?;
    if sleeping.voluntary_switches() < u64::from(SLEEPS) {
        return Err("a task that slept did not count a voluntary switch for each sleep");
    }
    if sleeping.involuntary_switches() >= u64::from(SLEEPS) {
        return Err("a task that slept counted its sleeps as involuntary switches");
    }
    Ok(())
}

/// Run them on a processor other than this one where there is one, and answer
/// how many cases they are.
///
/// # Errors
///
/// The first thing that was not so, by name.
// CHECKS-ONLY: syscall/rehome_check.rs
pub fn run() -> Result<u64, &'static str> {
    let here = crate::smp::this_cpu()
        .ok_or("no processor to run the switch counts on")?
        .logical;
    let count = crate::smp::count();
    check_switches_are_counted(if count > 1 { (here + 1) % count } else { here })?;
    Ok(1)
}
