//! The self-check of applied affinity (L.sched.96-98;
//! `docs/GAME-MODE-SCHED.md` §2.3, ledger 4446 (D11), 5156 (A9)).
//!
//! * **The kernel-thread refusal** (L.sched.96): a kernel thread asked a
//!   mask through `set_affinity`, the door `sched_setaffinity` takes, is
//!   refused, and its mask is the one it was spawned with.
//! * **A mask changed while asleep at home** (L.sched.97): a task pinned to
//!   processor 0 sleeps there; a waker on processor 0 gives it the last
//!   processor alone and wakes it, with preemption held so that nothing runs
//!   between the wake and its look. The task is queued on the last
//!   processor by the time the wake returns -- re-homed by `wake_at_home`,
//!   not moved later by a pick -- and runs there.
//! * **A mask changed while queued** (L.sched.98): the same, but the waker
//!   wakes the task first, which queues it on processor 0 behind the waker,
//!   and gives it the last processor alone after. Nothing steals a task whose
//!   mask names one processor, so only the pick on processor 0 can move it:
//!   it runs on the last processor, and the pick push counts one more.
//!
//! The two tasks of the second and third are kernel threads moved through
//! `set_mask`, the door below the refusal, which only this check uses.
//! With one processor neither is run, and the check says so.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::{IrqControl, SpinLock};

use super::Task;
use super::affinity::{AffinityRefusal, pushed, set_affinity, set_mask};
use crate::arch;
use crate::smp::Topology;

/// How long a sleeper sleeps unless woken: far longer than the check.
const SLEEP_NANOS: u64 = 10_000_000_000;

/// The processor the sleeper ran on after its wake, plus one; zero before.
static RAN_ON: AtomicU64 = AtomicU64::new(0);
/// The processor the woken task's home was when the wake returned, plus
/// one; zero before.
static HOME_AFTER_WAKE: AtomicU64 = AtomicU64::new(0);
/// The sleeper the waker works on.
static SLEEPER: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);
/// The processor the waker gives the sleeper.
static TARGET: AtomicU64 = AtomicU64::new(0);

/// Set to end the canary.
static CANARY_STOP: AtomicBool = AtomicBool::new(false);

/// The canary of the refusal: it only sleeps until it is told to stop.
fn canary(_argument: usize) {
    while !CANARY_STOP.load(Ordering::Acquire) {
        super::sleep_for(1_000_000);
    }
}

/// The sleeper: asleep at home until woken, then says where it ran.
fn sleeper(_argument: usize) {
    super::sleep_for(SLEEP_NANOS);
    let cpu = super::this_cpu().map_or(0, |cpu| cpu as u64 + 1);
    RAN_ON.store(cpu, Ordering::Release);
}

/// Lend or take the sleeper, with interrupts masked around the lock.
fn with_sleeper<R>(visit: impl FnOnce(&mut Option<Arc<Task>>) -> R) -> R {
    let saved = <arch::Irq as IrqControl>::disable();
    let answer = visit(&mut SLEEPER.lock());
    <arch::Irq as IrqControl>::restore(saved);
    answer
}

/// Run the check.
///
/// # Errors
///
/// The sentence of the rule that was broken.
///
/// Verifies: `L.sched.96`, `L.sched.97`, `L.sched.98`
pub(super) fn run(topology: &Topology) -> Result<(), &'static str> {
    a_kernel_thread_is_refused(topology)?;
    let online = topology.online();
    if online < 2 {
        crate::console::println!("  sched    affinity: one processor, masks not moved");
        return Ok(());
    }
    a_mask_set_while_asleep_is_obeyed_at_the_wake(online - 1)?;
    a_mask_set_while_queued_is_obeyed_at_the_pick(online - 1)?;
    crate::console::println!(
        "  sched    affinity: a kernel thread refused; a mask set asleep re-homes the wake, one set queued pushes the pick"
    );
    Ok(())
}

/// L.sched.96.
fn a_kernel_thread_is_refused(topology: &Topology) -> Result<(), &'static str> {
    let last = topology.online().saturating_sub(1);
    let spawned = CpuSet::of(0);
    CANARY_STOP.store(false, Ordering::Release);
    let task = super::spawn_on("affinity-canary", canary, 0, NICE_0_WEIGHT, 0, spawned)?;
    let answer = set_affinity(&task, &CpuSet::of(last));
    let kept = task.affinity() == spawned;
    CANARY_STOP.store(true, Ordering::Release);
    super::wait_until_gone(&task, super::REAPER_PATIENCE_NANOS)?;
    if answer != Err(AffinityRefusal::KernelThread) || !kept {
        return Err("a kernel thread's mask was changed by sched_setaffinity's door");
    }
    Ok(())
}

/// The waker of L.sched.97, on processor 0 with the sleeper: give it the
/// target processor alone, then wake it, with nothing run between.
fn wake_after_mask(_argument: usize) {
    waker(true);
}

/// The waker of L.sched.98: wake the sleeper, queueing it behind this task,
/// then give it the target alone.
fn mask_after_wake(_argument: usize) {
    waker(false);
}

/// The two wakers' body; `mask_first` for L.sched.97's order.
fn waker(mask_first: bool) {
    let target = TARGET.load(Ordering::Acquire) as usize;
    let Some(task) = with_sleeper(|sleeper| sleeper.clone()) else {
        return;
    };
    // Asleep at home: blocked, and not running, since this runs on its
    // processor.
    while task.state() != super::task::BLOCKED {
        super::sleep_for(1_000_000);
    }
    super::preempt_disable();
    if mask_first {
        let _ = set_mask(&task, &CpuSet::of(target));
        super::wake(&task);
    } else {
        super::wake(&task);
        let _ = set_mask(&task, &CpuSet::of(target));
    }
    HOME_AFTER_WAKE.store(task.cpu() as u64 + 1, Ordering::Release);
    super::preempt_enable();
}

/// One sleeper and one waker on processor 0, the waker running `waker`;
/// answers where the sleeper's home was as the wake returned and where it
/// ran, each a processor.
fn sleeper_and_waker(target: usize, waker: fn(usize)) -> Result<(usize, usize), &'static str> {
    RAN_ON.store(0, Ordering::Release);
    HOME_AFTER_WAKE.store(0, Ordering::Release);
    TARGET.store(target as u64, Ordering::Release);
    let sleeping = super::spawn_on(
        "affinity-sleeper",
        sleeper,
        0,
        NICE_0_WEIGHT,
        0,
        CpuSet::of(0),
    )?;
    with_sleeper(|sleeper| *sleeper = Some(Arc::clone(&sleeping)));
    let waking = super::spawn_on("affinity-waker", waker, 0, NICE_0_WEIGHT, 0, CpuSet::of(0))?;
    super::wait_until_gone(&waking, super::REAPER_PATIENCE_NANOS)?;
    super::wait_until_gone(&sleeping, super::REAPER_PATIENCE_NANOS)?;
    drop(with_sleeper(Option::take));
    let home = HOME_AFTER_WAKE.load(Ordering::Acquire);
    let ran = RAN_ON.load(Ordering::Acquire);
    if home == 0 || ran == 0 {
        return Err("the affinity check's sleeper was never woken");
    }
    Ok((home as usize - 1, ran as usize - 1))
}

/// L.sched.97.
fn a_mask_set_while_asleep_is_obeyed_at_the_wake(target: usize) -> Result<(), &'static str> {
    let (home, ran) = sleeper_and_waker(target, wake_after_mask)?;
    if home != target {
        crate::console::println!(
            "  sched    affinity: woken with processor {target} alone, queued on {home}"
        );
        return Err("a task woken with a mask that excludes its home was queued there");
    }
    if ran != target {
        return Err("a task woken with a mask that excludes its home ran outside it");
    }
    Ok(())
}

/// L.sched.98.
fn a_mask_set_while_queued_is_obeyed_at_the_pick(target: usize) -> Result<(), &'static str> {
    let before = pushed();
    let (_, ran) = sleeper_and_waker(target, mask_after_wake)?;
    if ran != target {
        crate::console::println!(
            "  sched    affinity: queued with processor {target} alone, ran on {ran}"
        );
        return Err("a task picked on a processor its mask excludes ran there");
    }
    if pushed() == before {
        return Err("a task queued outside its mask reached it with no push");
    }
    Ok(())
}
