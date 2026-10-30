//! Applied affinity: `sched_setaffinity` that pins (F3,
//! `docs/GAME-MODE-SCHED.md` §2.1; the consultant's design reviews, ledger
//! 4446 (D1)-(D12) and 5156 (A1)-(A12)).
//!
//! # The two halves of where a task may go
//!
//! Every placer asks two questions, each one conjunct: whether the processor
//! is in service ([`super::placeable`], L.sched.102, quarantine's) and
//! whether the task's mask allows it ([`allowed`], L.sched.97, this module's).
//! The mask is [`mask_for`]: the mask stored in the task, or -- for a task
//! that runs a program and whose stored mask names no processor in service
//! -- every processor of the domain, so that a mask never vetoes a park
//! (L.sched.123). The stored mask is kept through a park, and applies again
//! once one of its processors returns. The **effective mask** is the two
//! together ([`effective_mask`]): what `sched_getaffinity` answers.
//!
//! # The readers, and the locks they hold (ledger 5156 (A5))
//!
//! The stored mask is a sequence-counted copy of a `CpuSet`
//! (`task::Affinity`), written only by [`set_mask`] under the lock of the
//! queue that owns the task. Readers that hold that lock read it exactly:
//! the steal's and the evacuation's filters (`CpuQueue::steal_candidate`,
//! `take_movable_sleeper`, the victim's lock), `place_on_idle` (the home's),
//! the pick push ([`push_picked`], the home's), the re-checks under both
//! locks in `wake_onto` and `rehome_blocked`, and the push of a running task
//! off its processor ([`push_off`], both). Readers that hold none read a
//! consistent copy as a hint, re-checked under the move's locks or caught
//! by a backstop: `may_place` (the Sync wake, the pull, `rehome_blocked`'s
//! choice), `idle_allowed`, `wake_at_home`'s re-home test, `choose_cpu`'s
//! callers, the way out's test ([`keep_inside`]) and the setter's wait.
//!
//! # Where a task outside its mask is moved
//!
//! * **Blocked**: at its wake, by `wake_at_home`'s re-home, before it is
//!   queued (L.sched.97).
//! * **Queued**: at its pick, by the pick push in `choose_next` (L.sched.98),
//!   the backstop; nothing steals a task whose mask names one processor.
//! * **Running**: the setter posts [`super::work::PLACE`] and interrupts its
//!   processor; every way back to user mode on every architecture acts on
//!   the bit before any user instruction ([`keep_inside`], L.sched.99): it
//!   switches away, and the switch's tail moves it off ([`push_off`]). A
//!   setter whose target runs on another processor waits for it to be off
//!   that processor (L.sched.100).
//!
//! At a return to service nothing walks: a task whose stored mask applies
//! again is moved at its next placement, pick or way out (it carries
//! `PLACE` while its mask is narrower than the domain).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched::{CpuSet, MAX_CPUS};
use ferrix_sync::IrqControl;

use super::queue::CpuQueue;
use super::task::{RUNNABLE, Task};
use super::{ACTIVE, IDLE, RUNNING, domain_cpus, kick, move_onto, placeable, queue_of, work};
use crate::arch;

/// Why [`set_affinity`] refused a mask.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AffinityRefusal {
    /// A kernel thread: its spawn mask is its own and never changes
    /// (L.sched.96, ledger 4446 (D4)), as Linux's `PF_NO_SETAFFINITY`.
    KernelThread,
    /// The mask names no processor of the domain in service (ledger 5156
    /// (A1), Linux's `cpu_active_mask`).
    Empty,
}

/// The processors in service, as a set: `ACTIVE`'s words.
fn in_service_set() -> CpuSet {
    CpuSet::from_words(ACTIVE.each_ref().map(|word| word.load(Ordering::Acquire)))
}

/// The set `task`'s placement is asked against beside `placeable`: its
/// stored mask; or, for a task that runs a program whose stored mask names
/// no processor in service, every processor of the domain, the fallback
/// (L.sched.123: a mask never vetoes a park). A kernel thread's spawn mask
/// stands as it is: the per-processor kernel tasks stay where they are.
pub(crate) fn mask_for(task: &Task) -> CpuSet {
    let stored = task.affinity();
    if task.thread().is_none() || !stored.intersection(&in_service_set()).is_empty() {
        return stored;
    }
    domain_cpus().copied().unwrap_or(stored)
}

/// Whether `task`'s mask allows `cpu` (L.sched.97): the conjunct every
/// placer asks beside `placeable(cpu)`. One word and one bit when the
/// stored mask names `cpu` and `cpu` is in service, for then the mask is the
/// stored one.
pub(crate) fn allowed(task: &Task, cpu: usize) -> bool {
    (task.may_run_on(cpu) && super::in_service(cpu)) || mask_for(task).contains(cpu)
}

/// Whether `task`'s mask names one processor or none: never stolen,
/// balanced, pulled or placed on an idle processor, since it can be in one
/// place only.
pub(crate) fn pinned(task: &Task) -> bool {
    mask_for(task).len() <= 1
}

/// The effective mask: [`mask_for`] and in service (L.sched.123). What
/// `sched_getaffinity` answers.
pub fn effective_mask(task: &Task) -> CpuSet {
    mask_for(task).intersection(&in_service_set())
}

/// Give `task`, which runs a program, the stored mask `asked` and the
/// domain and in service (ledger 5156 (A1)), and see that it runs inside it
/// (L.sched.96-100). Answers the mask stored.
///
/// # Errors
///
/// [`AffinityRefusal::KernelThread`] for a task with no thread, fail closed
/// (L.sched.96); [`AffinityRefusal::Empty`] when nothing is left.
pub fn set_affinity(task: &Arc<Task>, asked: &CpuSet) -> Result<CpuSet, AffinityRefusal> {
    if task.thread().is_none() {
        return Err(AffinityRefusal::KernelThread);
    }
    set_mask(task, asked)
}

/// [`set_affinity`] without its refusal of a kernel thread: what it does
/// once the refusal has passed, and the door stage 5's affinity check moves
/// its own kernel threads through (`affinity_check`), which no other caller
/// has.
///
/// # Errors
///
/// [`AffinityRefusal::Empty`].
pub(super) fn set_mask(task: &Arc<Task>, asked: &CpuSet) -> Result<CpuSet, AffinityRefusal> {
    let domain = domain_cpus().ok_or(AffinityRefusal::Empty)?;
    let mask = asked.intersection(domain).intersection(&in_service_set());
    if mask.is_empty() {
        return Err(AffinityRefusal::Empty);
    }
    // Under the lock of the queue that owns it, found as `set_request` finds
    // it: the one writer at a time, and every reader under that lock exact.
    let saved = <arch::Irq as IrqControl>::disable();
    loop {
        let cpu = task.cpu();
        let Some(lock) = queue_of(cpu) else {
            task.store_affinity(mask);
            break;
        };
        let _queue = lock.lock();
        if task.cpu() != cpu {
            continue;
        }
        task.store_affinity(mask); // The one writer, under the home's lock.
        break;
    }
    <arch::Irq as IrqControl>::restore(saved);
    // After the store (C before D, `work`'s argument): its next way back to
    // user mode looks at the mask before any user instruction (L.sched.99).
    work::post(task, work::PLACE);
    wait_until_off(task);
    Ok(mask)
}

/// The longest a setter waits for a target running on a processor its new
/// mask excludes to be off it (L.sched.100): 100 ms. A target still there
/// then is in the kernel -- a user instruction of its would have taken the
/// way out the kick sends it, which moves it first -- so the setter
/// returns, and the target moves before its next user instruction all the
/// same.
const SETTER_WAIT_NANOS: u64 = 100_000_000;

/// Whether `task` is running now on a processor its mask does not allow.
fn running_outside(task: &Task) -> bool {
    let cpu = task.cpu();
    let running = RUNNING
        .get()
        .and_then(|running| running.get(cpu))
        .is_some_and(|id| id.load(Ordering::Acquire) == task.id);
    running && !allowed(task, cpu)
}

/// The setter's wait (L.sched.100, ledger 4446 (D7), Linux's
/// `affine_move_task`): while `task`, another task, runs on a processor its
/// mask excludes, interrupt that processor and wait, yielding, until it is
/// off it, it has died, or [`SETTER_WAIT_NANOS`] have passed. The caller
/// itself is moved by its own way out, after this returns.
fn wait_until_off(task: &Arc<Task>) {
    let me = super::with_current(|current| current.id);
    if me == Some(task.id) || !running_outside(task) {
        return;
    }
    super::interrupt(task);
    let deadline = crate::timer::now_nanos().saturating_add(SETTER_WAIT_NANOS);
    while running_outside(task) && !task.is_dead() && crate::timer::now_nanos() < deadline {
        super::yield_now();
    }
}

/// The way out's move (L.sched.99): called with interrupts masked by every
/// way back to user mode whose task has [`work::PLACE`] posted, before it
/// returns to user mode or builds a signal frame; returns masked, on a
/// processor the task's mask allows.
///
/// Each round is one decision: `choose_next` takes the task off this
/// processor's queue as it switches away, and the switch's tail queues it on
/// one its mask allows ([`push_off`]), which cannot fail; the task resumes
/// there and looks again. So a round is repeated only for a mask set again,
/// or a processor leaving service, while it moved. No lock is held and the
/// preemption count is zero at the switch: `schedule` stops the machine
/// otherwise (FX-0503).
pub(crate) fn keep_inside() {
    loop {
        let Some(cpu) = super::this_cpu() else {
            return;
        };
        if super::with_current(|task| allowed(task, cpu)).unwrap_or(true) {
            return;
        }
        super::schedule();
    }
}

/// Whether the way out may let go of `PLACE` for the running task: its mask
/// is the whole domain again, so no placement can leave it outside.
pub(crate) fn unmasked(task: &Task) -> bool {
    domain_cpus().is_some_and(|domain| domain.intersection(&task.affinity()) == *domain)
}

/// Processors with a task to push off at the switch's tail ([`push_off`]),
/// set by `choose_next` and taken by `finish_switch`, each by its own
/// processor only.
static PUSHING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// For `choose_next` on processor `cpu`, whose lock it holds: whether
/// `previous`, the task it runs, is runnable and outside its mask here, so
/// that it leaves this queue with the switch instead of being queued here
/// again; marked for `finish_switch` if so.
pub(super) fn leaves_with_switch(queue: &CpuQueue, cpu: usize, previous: &Task) -> bool {
    // Not on a processor asked to park: `park_pick` queues the running task
    // again for the evacuation, which moves it.
    let leaves = previous.state() == RUNNABLE
        && queue.idle.is_some()
        && super::park_state(cpu) == super::ACTIVE_STATE
        && !allowed(previous, cpu);
    if leaves && let Some(flag) = PUSHING.get(cpu) {
        flag.store(true, Ordering::Relaxed);
    }
    leaves
}

/// For `finish_switch`: whether the task processor `cpu` just switched away
/// from is to be pushed off.
pub(super) fn take_pushing(cpu: usize) -> bool {
    PUSHING
        .get(cpu)
        .is_some_and(|flag| flag.swap(false, Ordering::Relaxed))
}

/// Tasks moved off a processor their mask excludes: by the pick push, and
/// by the push at a switch's tail.
static PUSHED: AtomicU64 = AtomicU64::new(0);

/// [`PUSHED`], for the boot check.
pub(crate) fn pushed() -> u64 {
    PUSHED.load(Ordering::Relaxed)
}

/// The processors `task` may be pushed to from `home`, best first: idle
/// ones (`IDLE`), then the rest, lowest first; each allowed and placeable.
fn destinations(task: &Task, home: usize) -> impl Iterator<Item = usize> {
    let mask = mask_for(task);
    let idle = IDLE.load(Ordering::SeqCst);
    let is_idle = move |cpu: usize| cpu < 64 && idle & (1_u64 << cpu) != 0;
    let usable = move |cpu: &usize| *cpu != home && mask.contains(*cpu) && placeable(*cpu);
    let first = (0..MAX_CPUS)
        .filter(usable)
        .filter(move |&cpu| is_idle(cpu));
    let rest = (0..MAX_CPUS)
        .filter(usable)
        .filter(move |&cpu| !is_idle(cpu));
    first.chain(rest)
}

/// The pick push (L.sched.98, the backstop of ledger 4446 (D5)): `task`,
/// queued on `queue`, processor `home`, whose lock the caller holds, and
/// just picked there though its mask excludes `home`, is taken off and
/// queued on a processor its mask allows. A processor above `home` is
/// locked and waited for, as the lowest-first order allows; one below is
/// only tried (`docs/certification/MEMORY-AND-TIMING.md` §2.2z4). Whether
/// it moved: where every one below was held at that instant and none is
/// above, it stays, and runs here; a task running a program is then moved
/// by its way out before its next user instruction.
pub(super) fn push_picked(queue: &mut CpuQueue, home: usize, task: &Arc<Task>) -> bool {
    for to in destinations(task, home) {
        let Some(lock) = queue_of(to) else {
            continue;
        };
        let theirs = if to > home {
            Some(lock.lock())
        } else {
            lock.try_lock()
        };
        let Some(mut theirs) = theirs else {
            continue;
        };
        let Some((task, state)) = queue.release(task.id) else {
            return false;
        };
        move_onto(&task, Some(state), to, &mut theirs);
        drop(theirs);
        let _ = PUSHED.fetch_add(1, Ordering::Relaxed);
        kick(to);
        return true;
    }
    false
}

/// The push at a switch's tail (L.sched.99): `task`, which processor
/// `home` has just switched away from, runnable and on no queue since
/// [`leaves_with_switch`] took it off, is queued on a processor its mask
/// allows, under both queues' locks taken lower first and waited for,
/// which cannot fail. With none allowed in service now, it is queued at
/// `home` again and its way out looks once more. With interrupts masked,
/// no lock held.
pub(super) fn push_off(task: &Arc<Task>, home: usize) {
    let to = destinations(task, home).next().unwrap_or(home);
    let (Some(mine), Some(theirs)) = (queue_of(home), queue_of(to)) else {
        return;
    };
    if to == home {
        let mut queue = mine.lock();
        move_onto(task, None, home, &mut queue);
        return;
    }
    let (first, second) = if home < to {
        (mine, theirs)
    } else {
        (theirs, mine)
    };
    let mut first_queue = first.lock();
    let mut second_queue = second.lock();
    let destination = if home < to {
        &mut *second_queue
    } else {
        &mut *first_queue
    };
    move_onto(task, None, to, destination);
    drop(second_queue);
    drop(first_queue);
    let _ = PUSHED.fetch_add(1, Ordering::Relaxed);
    kick(to);
}
