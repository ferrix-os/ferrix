//! Tasks, and the scheduler that decides which of them runs.
//!
//! Stage 5 of `docs/ROADMAP.md`. Until now the kernel has run one thread of
//! control per processor, and every "CPU" in the code has meant a processor.
//! From here a processor runs a *task*, chosen from a queue of its own, and
//! the choosing is `ferrix_sched`'s EEVDF fair class — written in `src/lib/` and
//! tested on the host, because a scheduler that is wrong is wrong in a way
//! nothing on the machine can print.
//!
//! # What a switch is, and who holds the lock across it
//!
//! Deciding and switching are one operation. The run queue's lock is taken
//! before the decision and released *after* the switch, by whichever context
//! ends up running — the one switched to, or this one when nothing changed.
//! That is not an optimisation: it is what stops another processor picking up
//! the outgoing task in the window between it being put back on the queue and
//! its registers being saved. Linux hands its own `rq` lock across
//! `context_switch` for exactly this reason, and `SpinLock::lock_manually`
//! exists to say so in the type system's absence.
//!
//! # Where preemption happens
//!
//! Nowhere except on the way out of an interrupt. A timer interrupt does not
//! switch: it sets this processor's `need_resched` flag and returns, and the
//! generic trap path calls [`preempt_on_irq_exit`] once the controller has
//! been acknowledged. Switching inside the handler would leave an interrupt
//! in service on the local APIC for as long as the next task ran, which is a
//! machine that takes one interrupt and then no more.
//!
//! Because every lock that an interrupt handler may take masks interrupts, a
//! task cannot be preempted while it holds one — so the scheduler never has
//! to reason about a task switched out mid-critical-section.
//!
//! # Per-processor state and preemption
//!
//! Anything read through the per-processor register is only valid while
//! preemption cannot happen, which means with interrupts masked. A task that
//! read `smp::this_cpu()` and then blocked could wake on another processor
//! holding another processor's record. Every use here is inside an
//! interrupts-masked region for that reason.

mod check;
mod queue;
mod task;
pub(crate) mod trip;
mod wait;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ferrix_sched::{Balance, CpuSet, Domain, Mode, NICE_0_WEIGHT, Placement, check_partition};
use ferrix_sync::{IrqControl, IrqSpinLock, Once, SpinLock};

use crate::arch;
use crate::fallible;
use crate::object::quota;
use crate::smp::Topology;
use queue::CpuQueue;
use task::{DEAD, RUNNABLE};

pub(crate) use check::before_start as check_before_start;
pub(crate) use check::run as run_checks;
pub(crate) use queue::{MIN_SLICE_NS, SLICE_NS};
pub(crate) use task::{Task, TaskId, UserThread};
pub(crate) use wait::WaitQueue;

/// One run queue per logical processor.
static QUEUES: Once<Vec<SpinLock<CpuQueue>>> = Once::new();

/// One flag per processor, set by an interrupt that wants a decision made on
/// the way out of it.
static NEED_RESCHED: Once<Vec<AtomicBool>> = Once::new();

/// Per processor: a kick's interrupt is on its way that the processor has not
/// yet taken. See [`kick`].
static KICK_PENDING: Once<Vec<AtomicBool>> = Once::new();

/// One flag per processor, set while its idle task holds an exited task's
/// stack it is about to free. Read by the checks that count frames, which
/// must not measure while a free is in flight: see [`reaping_anywhere`].
static REAPING: Once<Vec<AtomicBool>> = Once::new();

/// One count per processor of how many reasons the running context has not
/// to be switched out. While it is above zero, [`preempt_on_irq_exit`] leaves
/// a pending reschedule unmade, and it is made when the count comes back to
/// zero.
///
/// Raised by every [`crate::sync::SpinLock`] for as long as it is held, and
/// by the idle task while it frees a stack. The lock is the reason it exists:
/// a ticket lock hands itself to whoever is next in line whether or not that
/// context is running, so a holder switched out for the few instructions it
/// holds the lock stalls every waiter for a round of the run queue, and
/// waiters switched out holding tickets pass the stall on. The thousand-task
/// check spent fifty seconds that way on a queue lock that was plain; with
/// the count, a holder is never switched out and the lock is held for the
/// instructions it covers and no longer.
///
/// **A context with the count raised must not block.** The count belongs to
/// the processor, and a task that slept with it raised would leave the
/// processor unable to preempt whatever ran next, then lower the count on
/// whichever processor woke it. `schedule` stops the machine if it is asked
/// to switch with the count raised, so the boot test finds any such holder.
static PREEMPT_OFF: Once<Vec<AtomicU32>> = Once::new();

/// Where each processor's count was last raised: the file and line that took
/// the lock, kept so that a holder found asleep is named rather than counted.
/// A pointer to a `'static` `Location`, or null.
static PREEMPT_SITE: Once<Vec<core::sync::atomic::AtomicPtr<core::panic::Location<'static>>>> =
    Once::new();

/// How much of each processor's count a *lock* raised, as against a
/// [`preempt_disable`] made by hand.
///
/// A shootdown waits for every other processor, so it may not run under a
/// lock another processor could be spinning on with preemption off -- and
/// [`crate::smp::flush_tlb_everywhere`] asserts that it does not, by asking
/// this. The count alone cannot answer: the reaper raises it by hand around
/// freeing a stack, and that free is a shootdown, by design. So the two are
/// told apart here rather than by the last site recorded above, which a lock
/// taken inside a by-hand window would overwrite.
static LOCKS_HELD: Once<Vec<AtomicU32>> = Once::new();

/// Where each processor's *outermost* lock was taken: the site recorded when
/// [`LOCKS_HELD`] went from zero to one, and what a shootdown asked for under
/// a lock names.
///
/// [`PREEMPT_SITE`] is the last raise, which is right for a holder found
/// asleep: the lock it took last is the one it sleeps under. It is wrong
/// for a shootdown under a lock, where the count is still up from a lock
/// taken earlier and every lock taken and released since has overwritten
/// the site: `channel_read`'s negative control named the VMO's page-list
/// lock, released a moment before, instead of the topology lock it held.
static LOCK_SITE: Once<Vec<core::sync::atomic::AtomicPtr<core::panic::Location<'static>>>> =
    Once::new();

/// The kernel's half of `ferrix_sync::PreemptControl`: this processor's
/// entry in [`PREEMPT_OFF`].
pub(crate) struct Preempt;

// SAFETY: (SHARED) `disable` raises this processor's count and `preempt_on_irq_exit`
// switches nothing while it is raised; `enable` lowers it and makes the
// decision that was deferred. Both are no-ops before the scheduler has a
// count or a processor has a number, when nothing can be switched out.
unsafe impl ferrix_sync::PreemptControl for Preempt {
    #[track_caller]
    fn disable() {
        preempt_disable_at(core::panic::Location::caller(), true);
    }

    fn enable() {
        preempt_enable_from(true);
    }
}

/// Keep the running context on this processor until the matching
/// [`preempt_enable`].
#[track_caller]
pub(crate) fn preempt_disable() {
    preempt_disable_at(core::panic::Location::caller(), false);
}

/// [`preempt_disable`], remembering `site` as the reason, and whether a lock
/// is the reason, for [`LOCKS_HELD`].
///
/// **Which processor, and the increment, under masked interrupts.** The
/// count is what keeps a task on its processor, and until it is raised the
/// task can still be switched out: an interrupt between reading the
/// processor's number and incrementing that processor's count could
/// preempt the task, an idle processor could steal it, and the increment
/// would then land on the processor it had left. That processor stayed at
/// one for good, the lock was held on the new one with nothing keeping its
/// holder there, and the next task to make a decision on the old one --
/// typically one exiting -- stopped the machine for a lock it never held.
/// Both hits were on the processor that had just run the job check's kill
/// interrupts, which is where preemptions of user tasks come thickest.
fn preempt_disable_at(site: &'static core::panic::Location<'static>, by_lock: bool) {
    let saved = <arch::Irq as IrqControl>::disable();
    if let Some(cpu) = this_cpu()
        && let Some(count) = PREEMPT_OFF.get().and_then(|counts| counts.get(cpu))
    {
        let _ = count.fetch_add(1, Ordering::AcqRel);
        if let Some(slot) = PREEMPT_SITE.get().and_then(|sites| sites.get(cpu)) {
            slot.store(core::ptr::from_ref(site).cast_mut(), Ordering::Release);
        }
        if by_lock && let Some(held) = LOCKS_HELD.get().and_then(|counts| counts.get(cpu)) {
            let was = held.fetch_add(1, Ordering::AcqRel);
            if was == 0
                && let Some(slot) = LOCK_SITE.get().and_then(|sites| sites.get(cpu))
            {
                slot.store(core::ptr::from_ref(site).cast_mut(), Ordering::Release);
            }
        }
    }
    <arch::Irq as IrqControl>::restore(saved);
}

/// Undo one [`preempt_disable`], and if that was the last, make the decision
/// an interrupt asked for meanwhile.
///
/// Only with interrupts on: a lock dropped inside a masked section, such as
/// under an `IrqSpinLock`, leaves the decision to the interrupt exit that
/// masked section will end with. And only once the scheduler runs.
///
/// The read of the processor's number and the decrement are under masked
/// interrupts, as in [`preempt_disable_at`]. With the count raised the task
/// cannot be switched out between them, so this is for symmetry and for
/// the enable that finds nothing to lower, which is not tolerated: it means
/// the count was raised on another processor than this one, and that
/// processor is now unpreemptible for good. It used to be floored at zero
/// and forgotten, which turned one lost increment into FX-0503 on an
/// innocent task some time later.
pub(crate) fn preempt_enable() {
    preempt_enable_from(false);
}

/// [`preempt_enable`], saying whether a lock's guard is what is being
/// released, for [`LOCKS_HELD`].
fn preempt_enable_from(by_lock: bool) {
    let saved = <arch::Irq as IrqControl>::disable();
    let (cpu, was) = match this_cpu().and_then(|cpu| Some((cpu, PREEMPT_OFF.get()?.get(cpu)?))) {
        Some((cpu, count)) => {
            if by_lock && let Some(held) = LOCKS_HELD.get().and_then(|counts| counts.get(cpu)) {
                // Saturating rather than checked: the count below is what
                // catches an enable without a disable, and names the site.
                let _ = held.fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                    Some(held.saturating_sub(1))
                });
            }
            (
                cpu,
                count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_sub(1)
                }),
            )
        }
        None => {
            <arch::Irq as IrqControl>::restore(saved);
            return;
        }
    };
    <arch::Irq as IrqControl>::restore(saved);
    match was {
        Ok(1) => {
            if started() && arch::interrupts_enabled() && take_resched(cpu) {
                schedule();
            }
        }
        Ok(_) => {}
        Err(_) => {
            let site = preempt_site(cpu);
            crate::panic::fatal!(
                crate::panic::catalog::SCHEDULE_WITH_PREEMPTION_HELD,
                "a lock that disables preemption was released on processor {cpu}, whose count \
                 was already zero: it was taken on another processor (this one's count was \
                 last raised at {}:{})",
                site.map_or("?", |site| site.file()),
                site.map_or(0, core::panic::Location::line),
            );
        }
    }
}

/// The file and line that last raised `cpu`'s count, if any is recorded.
pub(crate) fn preempt_site(cpu: usize) -> Option<&'static core::panic::Location<'static>> {
    let pointer = PREEMPT_SITE.get()?.get(cpu)?.load(Ordering::Acquire);
    // SAFETY: (SHARED) only `preempt_disable_at` stores here, and only a pointer to a
    // `'static` location the compiler handed it.
    unsafe { pointer.cast_const().as_ref() }
}

/// How many locks that disable preemption `cpu`'s running context holds,
/// for a check to print beside a result that preemption would explain.
pub(crate) fn preemption_held(cpu: usize) -> u32 {
    preempt_count(cpu)
}

/// The file and line that took the outermost lock `cpu`'s running context
/// still holds, if any is recorded: see [`LOCK_SITE`]. Meaningful only while
/// [`locks_held`] is above zero.
pub(crate) fn lock_site(cpu: usize) -> Option<&'static core::panic::Location<'static>> {
    let pointer = LOCK_SITE.get()?.get(cpu)?.load(Ordering::Acquire);
    // SAFETY: (SHARED) only `preempt_disable_at` stores here, and only a pointer to a
    // `'static` location the compiler handed it.
    unsafe { pointer.cast_const().as_ref() }
}

/// This processor's number, how many preemption-disabling locks it holds and
/// where the outermost was taken -- read together, so that the three are one
/// processor's answer.
///
/// **Why together.** A caller that is not itself pinned reads which processor
/// it is on, is preempted, and reads the counts of the processor it has left.
/// Those counts belong to whoever runs there now, so the answer describes two
/// tasks and neither of them faithfully. That is not a hypothetical: it failed
/// a boot of `test-threads` from `sys_rt_sigprocmask`, naming a futex lock
/// that no part of that call takes, and the panic came out of processor 3
/// while the message named processor 0 -- the two reads, from the two
/// processors the task ran on. Interrupts are masked for the read rather than
/// preemption disabled, because raising the count would change the very thing
/// being read.
pub(crate) fn locks_here() -> Option<(usize, u32, Option<&'static core::panic::Location<'static>>)>
{
    let saved = <arch::Irq as IrqControl>::disable();
    let answer = this_cpu().map(|cpu| (cpu, locks_held(cpu), lock_site(cpu)));
    <arch::Irq as IrqControl>::restore(saved);
    answer
}

/// How many locks that disable preemption `cpu`'s running context holds,
/// not counting a [`preempt_disable`] made by hand: what a shootdown may not
/// be asked for under. See [`LOCKS_HELD`].
pub(crate) fn locks_held(cpu: usize) -> u32 {
    LOCKS_HELD
        .get()
        .and_then(|counts| counts.get(cpu))
        .map_or(0, |count| count.load(Ordering::Acquire))
}

/// How many reasons `cpu`'s running context has not to be switched out.
fn preempt_count(cpu: usize) -> u32 {
    PREEMPT_OFF
        .get()
        .and_then(|counts| counts.get(cpu))
        .map_or(0, |count| count.load(Ordering::Acquire))
}

/// The task each processor is running, by identifier, for a reader that must
/// not take the run queue's lock: [`current_id`]. Zero before the processor
/// runs one.
static RUNNING: Once<Vec<AtomicU64>> = Once::new();

/// The job whose quota the task each processor runs is charged to, by slot,
/// for a charge made without the run queue's lock: [`running_group`].
/// `quota::NONE` for a kernel thread and for the root job.
static RUNNING_GROUP: Once<Vec<AtomicU32>> = Once::new();

/// How many moves between jobs the task each processor runs had seen when it
/// last looked: what lets [`regroup_current`] answer from one word on every
/// way back to user mode.
static RUNNING_SEEN: Once<Vec<AtomicU64>> = Once::new();

/// Make the per-processor words [`NEXT_BALANCE`], [`RUNNING`] and
/// [`RUNNING_GROUP`].
fn per_cpu_words(online: usize) {
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = NEXT_BALANCE.call_once(|| (0..online).map(|_| AtomicU64::new(0)).collect());
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = RUNNING.call_once(|| (0..online).map(|_| AtomicU64::new(0)).collect());
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = RUNNING_GROUP.call_once(|| (0..online).map(|_| AtomicU32::new(quota::NONE)).collect());
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = RUNNING_SEEN.call_once(|| (0..online).map(|_| AtomicU64::new(0)).collect());
}

/// Record that `cpu` now runs task `id`, charged to `group`, which had seen
/// `seen` moves between jobs.
fn note_running(cpu: usize, id: TaskId, group: u32, seen: u64) {
    if let Some(slot) = RUNNING.get().and_then(|running| running.get(cpu)) {
        slot.store(id, Ordering::Release);
    }
    if let Some(slot) = RUNNING_GROUP.get().and_then(|running| running.get(cpu)) {
        slot.store(group, Ordering::Release);
    }
    if let Some(slot) = RUNNING_SEEN.get().and_then(|running| running.get(cpu)) {
        slot.store(seen, Ordering::Release);
    }
}

/// The quota slot of the job the running task is charged to, read without a
/// lock: `quota::NONE` for a kernel thread, the root job, or before the
/// scheduler runs.
///
/// Stable for as long as the caller runs: only the task itself changes its
/// group ([`regroup_current`], [`set_current_group`]), and the task holds the
/// slot while it names it, so a charge made to the answer is to a live slot.
pub(crate) fn running_group() -> u32 {
    let saved = <arch::Irq as IrqControl>::disable();
    let group = this_cpu()
        .and_then(|cpu| RUNNING_GROUP.get()?.get(cpu))
        .map_or(quota::NONE, |slot| slot.load(Ordering::Acquire));
    <arch::Irq as IrqControl>::restore(saved);
    group
}

/// Moves of a process between jobs, ever: what a task compares with the
/// count it last saw, to follow its process at its next way back to user
/// mode without taking a lock on every one ([`regroup_current`]).
static MOVES: AtomicU64 = AtomicU64::new(0);

/// A process moved to another job: every running task looks again.
pub(crate) fn note_moved() {
    let _ = MOVES.fetch_add(1, Ordering::AcqRel);
    regroup_current();
}

/// Have the running task run, and charge, in its process's job, if a move
/// since it last looked changed that. Called on the way back to user mode,
/// where the task holds no lock.
pub(crate) fn regroup_current() {
    let moves = MOVES.load(Ordering::Acquire);
    let saved = <arch::Irq as IrqControl>::disable();
    let cpu = this_cpu();
    // The one word most returns read: nothing moved since this task looked.
    let seen = cpu
        .and_then(|cpu| RUNNING_SEEN.get()?.get(cpu))
        .is_none_or(|seen| seen.swap(moves, Ordering::AcqRel) == moves);
    let task = if seen {
        None
    } else {
        cpu.and_then(queue_of)
            .and_then(|lock| lock.lock().current.clone())
    };
    <arch::Irq as IrqControl>::restore(saved);
    let Some(task) = task else {
        return;
    };
    if task.seen_moves(moves) {
        return;
    }
    let Some(thread) = task.thread() else {
        return;
    };
    let slot = thread.process().core().quota_slot();
    if slot != task.group() {
        set_task_group(&task, slot);
    }
    // Only its own drop gives the last reference back, in task context.
    drop(task);
}

/// Have the running task wait out the rest of its job's `cpu.max` period if
/// its job, or one above it, has used the quota: called on the way back to
/// user mode, where the task holds no lock, as [`regroup_current`] is. A
/// machine with no `cpu.max` set pays one load.
pub(crate) fn throttle_current() {
    if !quota::bandwidth_in_use() {
        return;
    }
    let group = running_group();
    if group == quota::NONE {
        return;
    }
    while let Some(until) = quota::throttled_until(group, crate::timer::now_nanos()) {
        sleep_until(until);
    }
}

/// Run the calling task in `index`'s share, and charge what it does to it:
/// for a check that acts as a program in a job would.
pub(crate) fn set_current_group(index: u32) {
    let saved = <arch::Irq as IrqControl>::disable();
    let task = this_cpu()
        .and_then(queue_of)
        .and_then(|lock| lock.lock().current.clone());
    <arch::Irq as IrqControl>::restore(saved);
    if let Some(task) = task {
        set_task_group(&task, index);
    }
}

/// Move `task`, the running one, to `index`, and say so where charges look.
fn set_task_group(task: &Arc<Task>, index: u32) {
    task.set_group(index);
    let saved = <arch::Irq as IrqControl>::disable();
    if let Some(slot) = this_cpu().and_then(|cpu| RUNNING_GROUP.get()?.get(cpu)) {
        slot.store(index, Ordering::Release);
    }
    <arch::Irq as IrqControl>::restore(saved);
}

/// The identifier of the task running on this processor, read without a lock.
///
/// [`current`] takes the run queue's lock to clone the task, which a caller
/// that may itself be running under that lock cannot do. The failure
/// injection policy in `fallible.rs` is such a caller: it is asked from inside
/// allocations, and the scheduler allocates. `None` before the scheduler runs.
pub(crate) fn current_id() -> Option<TaskId> {
    let saved = <arch::Irq as IrqControl>::disable();
    let id = this_cpu()
        .and_then(|cpu| RUNNING.get()?.get(cpu))
        .map(|slot| slot.load(Ordering::Acquire))
        .filter(|&id| id != 0);
    <arch::Irq as IrqControl>::restore(saved);
    id
}

/// The machine's one scheduling domain, until stage 14 makes more.
static DOMAIN: Once<Domain> = Once::new();

/// Whether [`init`] has run.
static STARTED: AtomicBool = AtomicBool::new(false);

/// How many processors have taken up their idle task.
///
/// Waited for by [`init`], because a check that measures work stealing is
/// measuring nothing until there is a second processor in the scheduler to
/// steal. Secondaries arrive under their own steam — they are woken by an
/// IPI and have to get from stage 4's job loop to `enter_idle` — so "the
/// scheduler is up" is not the same instant on every processor, and the
/// difference is milliseconds an emulated machine can easily stretch.
static IN_SCHEDULER: AtomicU64 = AtomicU64::new(0);

/// The next task identifier. Never reused.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One bit per processor whose idle task is looking for work or asleep: set
/// by the idle loop before it looks, and by a switch to the idle task;
/// cleared by the idle loop when it finds work, and by a switch to any other
/// task, before that task runs (`choose_next`).
///
/// Read by [`wake_idle_processors`], so that a spawn does not interrupt
/// every processor on the machine when none of them is idle: a thousand
/// spawns in a row used to be three thousand interrupts taken by processors
/// that were busy running the previous spawns. The bit is set *before* the
/// idle loop looks for work and the spawn looks at the mask only *after* it
/// has queued the task, each behind a full fence, so either the idle
/// processor's look finds the task or the spawn finds the bit -- the same
/// argument as a sleeping reader against a signalling writer.
static IDLE: AtomicU64 = AtomicU64::new(0);

/// Note that `cpu`'s idle task is, or has stopped, looking.
fn set_idle(cpu: Option<usize>, idle: bool) {
    let Some(bit) = cpu.filter(|cpu| *cpu < 64).map(|cpu| 1u64 << cpu) else {
        return;
    };
    if idle {
        let _ = IDLE.fetch_or(bit, Ordering::SeqCst);
    } else {
        let _ = IDLE.fetch_and(!bit, Ordering::SeqCst);
    }
}

/// Tasks that have exited, waiting for their stacks to be freed.
///
/// Not freed where they exit: a task cannot unmap the stack it is standing
/// on, and the context that switched away from it is holding a run queue lock
/// while `vmap::free_stack` needs to invalidate other processors' translations
/// and wait for them.
///
/// Each held in its own run slot, which a task that has left its run queue
/// for good no longer needs: filing a zombie happens as the scheduler
/// switches away from it, where nothing may allocate (finding F-23).
static ZOMBIES: IrqSpinLock<Zombies, arch::Irq> = IrqSpinLock::new(Zombies::new());

/// The dead tasks in [`ZOMBIES`], oldest first.
#[derive(Debug)]
struct Zombies {
    /// The tasks, keyed by the order they died in.
    list: ferrix_sched::Timeline<Arc<Task>>,
    /// The next one's place in that order.
    next: u64,
}

impl Zombies {
    /// None.
    const fn new() -> Zombies {
        Zombies {
            list: ferrix_sched::Timeline::new(),
            next: 0,
        }
    }

    /// How many are waiting.
    fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether none are.
    fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// File `task`, in its run slot. A task still holding a run queue's slot
    /// cannot be filed, and is counted as lost instead: its stack is never
    /// freed, which is a leak and not a fault. `DEAD_STILL_QUEUED` counts
    /// the same condition, and a check requires it to be zero.
    fn push(&mut self, task: Arc<Task>) {
        let Some(slot) = task.take_run_slot() else {
            let _ = ZOMBIES_LOST.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let at = self.next;
        self.next = self.next.wrapping_add(1);
        // NOALLOC: a `Timeline` files the task in the slot it is given.
        self.list.insert(at, task.id, task, slot);
    }

    /// Take the oldest out.
    fn pop(&mut self) -> Option<Arc<Task>> {
        let (_, task, slot) = self.list.pop_due(u64::MAX)?;
        task.return_run_slot(slot);
        Some(task)
    }
}

/// Dead tasks [`Zombies::push`] could not file.
static ZOMBIES_LOST: AtomicU64 = AtomicU64::new(0);

/// Queueings refused because the task had no run slot to be queued in: see
/// `CpuQueue::insert`. Zero on a correct kernel.
static MISSING_SLOTS: AtomicU64 = AtomicU64::new(0);

/// Count a queueing refused for want of a run slot.
pub(super) fn note_missing_slot() {
    let _ = MISSING_SLOTS.fetch_add(1, Ordering::Relaxed);
}

/// How many stacks one pass of the idle loop's reaper frees.
///
/// A shootdown costs the same whether it invalidates one stack or sixteen,
/// and it is an interrupt every other processor has to answer: the batch is
/// what stops a program's thread churn being paid for by every other program
/// on the machine. Sixteen rather than the whole list because the batch is
/// freed with preemption off — see [`reap_batch`] — so it is also how long this
/// processor may be from taking the task an interrupt has just woken. The
/// stacks themselves are four pages each and give their frames straight back,
/// so a batch is not memory held either.
const REAP_BATCH: usize = 16;

/// Switches away from a dead task that a run queue still counted as queued.
///
/// Counted in [`finish_switch`], the one place that sees every dead task
/// leave its processor for the last time, and reported by
/// [`check_invariants`]. Sticky, because the moment passes: the reaper frees
/// the task within milliseconds, and a check that went looking afterwards
/// would find nothing to object to.
static DEAD_STILL_QUEUED: AtomicU64 = AtomicU64::new(0);

/// Tasks that have exited and whose reaper has not yet dropped them.
///
/// Raised in [`exit`] before the task is marked dead, so before anything can
/// see it as not running, and lowered in [`reap_batch`] and [`reap`] only once
/// the reaper's reference is dropped. For [`wait_until_reaper_quiet`]: the
/// zombie list misses a task that has exited and not yet switched away, and
/// one an idle processor has taken off the list and not yet dropped.
static EXITED_UNREAPED: AtomicUsize = AtomicUsize::new(0);

/// Whether the scheduler is running.
pub(crate) fn started() -> bool {
    STARTED.load(Ordering::Acquire)
}

/// Whether the running context may block: a task, on a processor taking
/// interrupts, with nothing holding preemption off.
///
/// Which processor this is and that processor's count are read with
/// interrupts closed, as [`current`] reads its queue. A task preempted
/// between the two reads may be moved, and then reads another processor's
/// count, which any spin lock a task there holds has raised: a debug kernel
/// then stopped at a sleeping lock taken in a system call, as if the caller
/// held a spin lock. Stage 20's many-threaded builds, every `mmap` of which
/// takes the space's layout lock, met it about one boot in six.
pub(crate) fn may_block() -> bool {
    if !started() || !arch::interrupts_enabled() {
        return false;
    }
    let saved = <arch::Irq as IrqControl>::disable();
    let own = this_cpu().is_some_and(|cpu| preempt_count(cpu) == 0);
    <arch::Irq as IrqControl>::restore(saved);
    own && current().is_some()
}

/// The next identifier to give a task.
fn next_id() -> TaskId {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// The run queue of logical processor `cpu`.
fn queue_of(cpu: usize) -> Option<&'static SpinLock<CpuQueue>> {
    QUEUES.get()?.get(cpu)
}

/// Which processor this is, or `None` before the per-processor register is
/// installed.
fn this_cpu() -> Option<usize> {
    crate::smp::this_cpu().map(|cpu| cpu.logical)
}

/// Say that `cpu` should pick again on the way out of the interrupt it is in.
fn mark_resched(cpu: usize) {
    if let Some(flag) = NEED_RESCHED.get().and_then(|flags| flags.get(cpu)) {
        flag.store(true, Ordering::Release);
    }
}

/// Make this processor decide again soon, from any context running on it.
///
/// Called with interrupts masked, on `cpu` itself, because the timer it arms
/// is this processor's. The flag alone is read only on the way out of an
/// interrupt. A task that makes something runnable from a system call, or a
/// kernel thread doing the same, goes back to what it was doing through no
/// such exit. If it was alone its timer is stopped, and what it just made
/// runnable waits for an interrupt with no reason to come. So the timer is
/// armed too, for the shortest interval worth arming. From inside an interrupt
/// the exit comes first, and `choose_next` re-arms the timer for the real
/// decision before this one fires.
fn resched_here(cpu: usize) {
    mark_resched(cpu);
    crate::timer::after(queue::MIN_ARM_NS);
}

/// Interrupt another processor so it notices its flag: that one alone, and
/// only if an interrupt from an earlier kick has not yet reached it.
///
/// **This was a broadcast**, and every wake of a task on another processor
/// interrupted all of them: on four, three interrupts to tell one, the other
/// two woken out of `hlt` -- under a hypervisor an exit and a host thread
/// woken each -- to find nothing and wait again. A browser wakes tasks across
/// processors thousands of times a second.
///
/// One interrupt in flight is enough for any number of kicks. Each kick sets
/// the target's flag before it looks at [`KICK_PENDING`], and the target
/// lowers that mark with a swap on the way out of its next interrupt, before
/// it reads the flag (`preempt_on_irq_exit`): a kick that found the mark up
/// is read by that exit, and one after it finds the mark down and sends
/// another. The flag itself means what it always did.
fn kick(cpu: usize) {
    // The flag is set here, on the target's behalf, rather than by the target
    // inside its own interrupt handler. So the interrupt carries no meaning
    // of its own: it exists to make the target *reach* an interrupt exit,
    // where `preempt_on_irq_exit` reads the flag. That is why there is no
    // scheduler hook in the IPI handler, and why a processor woken by
    // somebody else's shootdown finds this flag and acts on it just as well.
    mark_resched(cpu);
    let pending = KICK_PENDING.get().and_then(|marks| marks.get(cpu));
    if pending.is_some_and(|mark| mark.swap(true, Ordering::AcqRel)) {
        return;
    }
    crate::smp::interrupt_one(cpu);
}

/// Each processor's reschedule flag, and its mark that a kick's interrupt is
/// on its way: the two [`kick`] raises, made together.
fn init_resched_flags(online: usize) {
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = NEED_RESCHED.call_once(|| (0..online).map(|_| AtomicBool::new(false)).collect());
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = KICK_PENDING.call_once(|| (0..online).map(|_| AtomicBool::new(false)).collect());
}

/// Bring up the scheduler, and make the context calling it a task.
///
/// Must run after `smp::discover` and `smp::start_secondaries`, because it
/// gives every processor a queue and expects the secondaries to be waiting
/// for work.
///
/// # Errors
///
/// If the machine has more processors than the domain can name, or there is
/// no memory for a queue or an idle task's stack.
pub(crate) fn init(topology: &'static Topology) -> Result<(), &'static str> {
    let online = topology.count();
    let cpus =
        CpuSet::first(online).map_err(|_| "more processors than a scheduling domain holds")?;
    let domain =
        Domain::new(cpus, Mode::Throughput).map_err(|_| "the Throughput domain was refused")?;
    check_partition(core::slice::from_ref(&domain), online)
        .map_err(|_| "the scheduling domains do not cover every processor exactly once")?;
    let _ = DOMAIN.call_once(|| domain);

    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let mut queues = Vec::with_capacity(online);
    for _ in 0..online {
        // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
        queues.push(SpinLock::new(CpuQueue::new()?));
    }
    let _ = QUEUES.call_once(|| queues);
    init_resched_flags(online);
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = REAPING.call_once(|| (0..online).map(|_| AtomicBool::new(false)).collect());
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = PREEMPT_OFF.call_once(|| (0..online).map(|_| AtomicU32::new(0)).collect());
    // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
    let _ = LOCKS_HELD.call_once(|| (0..online).map(|_| AtomicU32::new(0)).collect());
    let _ = PREEMPT_SITE.call_once(|| {
        (0..online)
            .map(|_| core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()))
            // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
            .collect()
    });
    let _ = LOCK_SITE.call_once(|| {
        (0..online)
            .map(|_| core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()))
            // FATAL-ALLOC: boot only: stage 5 makes each processor's scheduling state once, as the scheduler starts.
            .collect()
    });
    per_cpu_words(online);

    adopt_boot_task()?;
    joined(0);
    STARTED.store(true, Ordering::Release);

    // The secondaries are asleep in `smp::secondary_main`. Wake them, so each
    // takes up the idle loop that is its half of the scheduler.
    let _ = arch::send_ipi_to_others();
    wait_for_processors(online)
}

/// Wait until every processor is in the scheduler, or say which never came.
///
/// The boot processor counts itself, having just adopted its own context; the
/// rest arrive through [`enter_idle`]. Bounded, because a processor that never
/// arrives should be a sentence in the boot log rather than a wait that never
/// ends — and the interrupt is re-sent, because the first one may have been
/// sent while a processor was still on its way into the halt that was meant
/// to receive it.
///
/// **Re-sent on an interval, not every time round.** Broadcasting on every
/// iteration of this spin is an interrupt storm, and it starves exactly the
/// processors the wait is waiting for: one woken by a nudge is interrupted
/// again before it can run the few instructions between `wait_for_work`
/// returning and `enter_idle` recording its arrival, so it never gets to
/// record it. On an STM32MP157D-DK1 that cost the entire five seconds and
/// then blamed a processor that was awake the whole time, trying to join.
fn wait_for_processors(online: usize) -> Result<(), &'static str> {
    /// How long to give them. Generous: an emulated processor may be a host
    /// thread that is not currently running.
    const PATIENCE_NANOS: u64 = 5_000_000_000;
    /// How long to leave a processor alone between nudges. Long enough that a
    /// woken processor reaches [`enter_idle`] undisturbed, short enough that a
    /// nudge lost to the race above costs a millisecond rather than the wait.
    const NUDGE_INTERVAL_NANOS: u64 = 1_000_000;

    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let all = if online >= 64 {
        u64::MAX
    } else {
        (1u64 << online) - 1
    };

    let mut next_nudge = 0_u64;
    while IN_SCHEDULER.load(Ordering::Acquire) != all {
        let now = crate::timer::now_nanos();
        if now >= deadline {
            return Err("a processor never joined the scheduler");
        }
        if now >= next_nudge {
            next_nudge = now.saturating_add(NUDGE_INTERVAL_NANOS);
            let _ = arch::send_ipi_to_others();
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Record that `cpu` is now running tasks.
fn joined(cpu: usize) {
    if cpu < 64 {
        let _ = IN_SCHEDULER.fetch_or(1u64 << cpu, Ordering::AcqRel);
    }
}

/// Make the context that called [`init`] the boot processor's first task, and
/// give that processor an idle task to fall back to.
fn adopt_boot_task() -> Result<(), &'static str> {
    let boot = Task::adopt(next_id(), "kmain", NICE_0_WEIGHT, 0)
        .and_then(fallible::try_arc)
        .map_err(|_| "no memory for the boot task")?;
    let idle = new_idle_task(0)?;
    let lock = queue_of(0).ok_or("the boot processor has no run queue")?;

    let saved = <arch::Irq as IrqControl>::disable();
    {
        let mut queue = lock.lock();
        queue.idle = Some(idle);
        // NOALLOC: `CpuQueue::insert` queues the task in its own run slot.
        queue.insert(&boot);
        // Make it the running entity rather than one waiting to run: it is
        // already on the processor.
        let _ = queue.fair.pick_next();
        queue.exec_start = crate::timer::now_nanos();
        note_running(0, boot.id, quota::NONE, 0);
        queue.current = Some(boot);
    }
    <arch::Irq as IrqControl>::restore(saved);
    Ok(())
}

/// An idle task with a stack of its own, for a processor whose own context is
/// busy being something else.
fn new_idle_task(cpu: usize) -> Result<Arc<Task>, &'static str> {
    let stack = crate::vmap::allocate_stack().map_err(|_| "no stack for an idle task")?;
    // SAFETY: (CONTEXT) the stack was allocated a moment ago, is mapped and writable,
    // and nothing else refers to it.
    let stack_pointer = unsafe { arch::prepare_stack(stack.top, task_start, 0) };
    let task = Task::new(task::NewTask {
        id: next_id(),
        name: "idle",
        entry: |_| idle_loop(),
        argument: 0,
        stack,
        stack_pointer,
        weight: NICE_0_WEIGHT,
        cpu,
        affinity: CpuSet::of(cpu),
        // A processor's idle task is the kernel's own and has no user half.
        address_space: None,
        thread: None,
        user_state: None,
    })
    .and_then(fallible::try_arc);
    task.map_err(|_| {
        // SAFETY: (KMEM) allocated above and never run on.
        let _ = unsafe { crate::vmap::free_stack(stack) };
        "no memory for an idle task"
    })
}

/// Where a secondary processor joins the scheduler: its bring-up context
/// becomes its idle task, and it never returns.
pub(crate) fn enter_idle() -> ! {
    let Some(cpu) = this_cpu() else { arch::halt() };
    // FATAL-ALLOC: boot only: a secondary processor adopts its idle context
    // once, as it comes up.
    let idle = Arc::new(
        Task::adopt(next_id(), "idle", NICE_0_WEIGHT, cpu).unwrap_or_else(|_| {
            crate::panic::fatal!(
                crate::panic::catalog::BOOT_OUT_OF_MEMORY,
                "no memory for processor {cpu}'s idle task"
            )
        }),
    );

    if let Some(lock) = queue_of(cpu) {
        let saved = <arch::Irq as IrqControl>::disable();
        {
            let mut queue = lock.lock();
            queue.idle = Some(Arc::clone(&idle));
            note_running(cpu, idle.id, quota::NONE, 0);
            queue.current = Some(idle);
            queue.exec_start = crate::timer::now_nanos();
        }
        <arch::Irq as IrqControl>::restore(saved);
    }
    joined(cpu);
    idle_loop()
}

/// What a processor does with nothing to run: look for work to take from a
/// busier processor, tidy up after tasks that have exited, and otherwise
/// sleep until an interrupt says something has changed.
///
/// One exited task's stack per turn, and a look at the queue between stacks:
/// the idle task runs only while nothing else can, so anything it holds while
/// it is switched out is held for as long as the processor stays busy. See
/// [`reap_batch`] for the boot that showed why.
fn idle_loop() -> ! {
    let cpu = this_cpu();
    // A secondary arrives here with interrupts masked, from the look-and-wait
    // step of `smp::secondary_main` that handed it over, and the first thing
    // below is a reap, which frees a stack, which is a shootdown that waits
    // for every other processor: that must run with interrupts on, as
    // `reap_batch` says and `smp` checks. The halt path enables them in any
    // case; enabling here makes the first pass like every later one.
    arch::enable_interrupts();
    loop {
        let reaped = reap_batch(false);
        // An interrupt that arrived while the stack was held was not allowed
        // to switch this task out. Make the decision it asked for now, through
        // `schedule` and not through the look at the queue below: a sleeper
        // whose timer fired meanwhile is in the sleeper set, not the fair
        // class, and only `choose_next` moves it across and re-arms the timer.
        // Halting here instead left it asleep until some other interrupt
        // happened to arrive.
        if reaped && cpu.is_some_and(take_resched) {
            schedule();
            continue;
        }

        // Idle to the rest of the machine from here: before the look, so
        // that a spawn made after the look sees the bit and sends the
        // interrupt the halt below is waiting for. See `IDLE`.
        set_idle(cpu, true);
        if steal_work() {
            set_idle(cpu, false);
            schedule();
            continue;
        }

        // Masked while looking, so that an interrupt arriving after the look
        // wakes the wait rather than being taken just before it.
        arch::disable_interrupts();
        if has_work() {
            set_idle(cpu, false);
            arch::enable_interrupts();
            schedule();
        } else if reaped {
            // More may be waiting: look again rather than halt with stacks
            // still to free.
            arch::enable_interrupts();
        } else if machine_is_quiet() && !ZOMBIES.lock().is_empty() {
            // Nothing anywhere else to do, and stacks waiting for a batch
            // that may never fill. Free them now -- all of them under one
            // shootdown -- rather than halt holding them: a processor that
            // halts arms nothing, so "later" could be the next interrupt from
            // anywhere.
            //
            // **Only when every other processor is idle too.** The cost a
            // shootdown imposes is paid by whoever it interrupts, so a
            // straggler freed while the rest of the machine works is the very
            // thing `REAP_BATCH` exists to stop. While anything else is
            // running, a partial batch waits: at sixteen stacks it goes
            // regardless, and sixteen stacks is 256 KiB.
            arch::enable_interrupts();
            let _ = reap_batch(true);
        } else {
            arch::wait_for_work();
            set_idle(cpu, false);
        }
    }
}

/// Whether this processor reads as idle to the rest of the machine: its bit
/// in [`IDLE`]. For the check that a processor running a task never does.
pub(super) fn this_cpu_reads_as_idle() -> bool {
    let saved = <arch::Irq as IrqControl>::disable();
    let bit = this_cpu()
        .filter(|cpu| *cpu < 64)
        .map_or(0, |cpu| 1u64 << cpu);
    let idle = IDLE.load(Ordering::SeqCst) & bit != 0;
    <arch::Irq as IrqControl>::restore(saved);
    idle
}

/// Whether every processor that has a run queue is in its idle loop.
///
/// Read from [`IDLE`], whose bit is set before a processor looks for work and
/// cleared when it finds some, so this is a snapshot that may be stale the
/// moment it is taken. That is what it is for: it decides whether a reap that
/// could wait should happen now, and being wrong either way costs one
/// shootdown or delays one, never correctness.
fn machine_is_quiet() -> bool {
    let Some(queues) = QUEUES.get() else {
        return true;
    };
    let Some(all) = (1u64 << queues.len().min(64)).checked_sub(1) else {
        return false;
    };
    IDLE.load(Ordering::SeqCst) & all == all
}

/// Whether this processor has anything but its idle task to run.
fn has_work() -> bool {
    let Some(lock) = this_cpu().and_then(queue_of) else {
        return false;
    };
    let queue = lock.lock();
    queue.has_work()
}

/// Start a task on this processor.
///
/// # Errors
///
/// If there is no stack for it, or the scheduler is not up.
pub(crate) fn spawn(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    weight: u32,
) -> Result<Arc<Task>, &'static str> {
    let here = this_cpu().ok_or("no processor to start a task on")?;
    let anywhere = *domain_cpus().ok_or("the scheduler has no domain")?;
    // Where it *should* go, not where it happens to be created. A burst of
    // tasks created on one processor used to queue behind each other there
    // until some other processor went idle and came looking; now each one is
    // placed when it is made.
    let cpu = choose_cpu(&anywhere, here).unwrap_or(here);
    // Counted at the moment of the decision, which is the only place the
    // decision can be seen. Where a task *ends up* says nothing about
    // placement: an idle processor steals within microseconds of the spawn,
    // so a check reading `task.cpu()` afterwards passes just as well with
    // placement removed entirely — which is exactly what a negative control
    // showed when this was tested that way.
    if cpu != here {
        let _ = PLACED_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
    }
    spawn_on(name, entry, argument, weight, cpu, anywhere)
}

/// The processors the machine's one domain covers.
fn domain_cpus() -> Option<&'static CpuSet> {
    DOMAIN.get().map(Domain::cpus)
}

/// Ask `ferrix_sched` where a task should go, given what every processor is
/// carrying right now.
///
/// Takes every run queue's lock in turn, which is why it is never called with
/// one already held. The snapshot is stale the moment it is taken — another
/// processor may enqueue something before this one acts on the answer — and
/// that is fine: a placement decision is a hint, and the balancer below
/// corrects a bad one. What it must not do is deadlock, hence the ordering
/// rule.
fn choose_cpu(allowed: &CpuSet, prefer: usize) -> Option<usize> {
    let queues = QUEUES.get()?;
    // Folded one processor at a time rather than snapshotted into an array.
    // The array was `[CpuLoad; MAX_CPUS]`, six kilobytes of a sixteen-kilobyte
    // kernel stack, and it is the reason this scheduler wedged one boot in
    // three: `balance` below asks the same question from inside an interrupt,
    // on top of whatever it interrupted, and the guard page caught it.
    let mut choice = Placement::new(prefer);

    let saved = <arch::Irq as IrqControl>::disable();
    for (cpu, lock) in queues.iter().enumerate() {
        if !allowed.contains(cpu) {
            continue;
        }
        let snapshot = lock.lock().snapshot();
        choice.consider(cpu, snapshot);
    }
    <arch::Irq as IrqControl>::restore(saved);

    choice.choice()
}

/// Start a kernel thread on `cpu`, able to run on `affinity`.
///
/// # Errors
///
/// If there is no stack for it, or `cpu` has no run queue.
pub(crate) fn spawn_on(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    weight: u32,
    cpu: usize,
    affinity: CpuSet,
) -> Result<Arc<Task>, &'static str> {
    spawn_on_in(name, entry, argument, weight, cpu, affinity, None)
}

/// Start a task on `cpu` in `address_space`, or as a kernel thread when that
/// is `None`.
///
/// The entry point is a kernel function either way: this makes a thread that
/// *has* an address space, not one running in it at a lower privilege level.
/// The two are separate steps and a processor must be able to do the first
/// without the second, because kernel code servicing a fault runs in the
/// address space that faulted.
///
/// # Errors
///
/// If there is no stack for it, or `cpu` has no run queue.
pub(crate) fn spawn_on_in(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    weight: u32,
    cpu: usize,
    affinity: CpuSet,
    address_space: Option<Arc<crate::user::space::AddressSpace>>,
) -> Result<Arc<Task>, &'static str> {
    spawn_task(
        name,
        entry,
        argument,
        weight,
        cpu,
        affinity,
        address_space,
        None,
        None,
    )
}

/// Start a kernel thread on `cpu` alone, run and charged as a task of the job
/// whose quota slot is `group`: for the checks of a job's processor share.
/// The group is set before any queue sees the task, so no processor ever
/// runs it charged elsewhere.
///
/// # Errors
///
/// If there is no stack for it, or `cpu` has no run queue.
pub(crate) fn spawn_in_group(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    cpu: usize,
    group: u32,
) -> Result<Arc<Task>, &'static str> {
    let queue = queue_of(cpu).ok_or("no such processor")?;
    let task = make_task(
        name,
        entry,
        argument,
        NICE_0_WEIGHT,
        cpu,
        CpuSet::of(cpu),
        None,
        None,
        None,
    )?;
    task.set_group(group);
    enqueue(&task, cpu, queue);
    Ok(task)
}

/// Start the task that runs `process`'s code: a thread in its address space,
/// whose entry point `entry` drops to user mode.
///
/// Placed like any other task unless `cpu` pins it, which is for the checks
/// that need two programs to share a processor.
///
/// # Errors
///
/// As [`prepare_user`].
pub(crate) fn spawn_user(
    name: &'static str,
    entry: fn(usize),
    thread: Arc<dyn UserThread>,
    cpu: Option<usize>,
    state: Option<arch::UserState>,
) -> Result<Arc<Task>, &'static str> {
    prepare_user(name, entry, thread, cpu, state).map(PreparedTask::launch)
}

/// Make the task that would run `thread`, counted but on no queue: everything
/// about starting a program that can fail, done before the one step that
/// cannot.
///
/// A native start moves a handle into its child between the two, so that no
/// start fails after the move and has to move the handle back.
///
/// # Errors
///
/// If there is no processor to place it on, or no stack for it.
pub(crate) fn prepare_user(
    name: &'static str,
    entry: fn(usize),
    thread: Arc<dyn UserThread>,
    cpu: Option<usize>,
    state: Option<arch::UserState>,
) -> Result<PreparedTask, &'static str> {
    let here = this_cpu().ok_or("no processor to start a program on")?;
    let anywhere = *domain_cpus().ok_or("the scheduler has no domain")?;
    let (cpu, affinity) = match cpu {
        Some(cpu) => (cpu, CpuSet::of(cpu)),
        None => (choose_cpu(&anywhere, here).unwrap_or(here), anywhere),
    };
    // Resolved here, so that launching has nothing left to refuse.
    let queue = queue_of(cpu).ok_or("no such processor")?;
    let space = Arc::clone(thread.process().core().space());
    // Counted before the task exists: on another processor it can reach its
    // thread's exit as soon as it is launched. A task that could not be made
    // gives the count back here, and one never launched when it is dropped.
    thread.process().thread_starting();
    let task = make_task(
        name,
        entry,
        0,
        NICE_0_WEIGHT,
        cpu,
        affinity,
        Some(space),
        Some(Arc::clone(&thread)),
        state,
    )
    .inspect_err(|_| thread.process().thread_gone(false))?;
    // Its process's job, for its share of the processor and for what it
    // charges; set before any queue can see it.
    task.set_group(thread.process().core().quota_slot());
    Ok(PreparedTask {
        task,
        cpu,
        queue,
        thread,
        launched: false,
    })
}

/// A user task made and counted, on no queue yet.
///
/// [`PreparedTask::launch`] puts it on its processor's queue and cannot fail.
/// Dropped instead, it frees its stack and then gives its thread's count back,
/// as a spawn that failed does. Freeing a kernel stack waits for every
/// processor to forget the mapping, so one must be dropped in task context,
/// with interrupts enabled and no lock held that keeps preemption off.
#[must_use = "dropping a prepared task frees it instead of running it"]
pub(crate) struct PreparedTask {
    /// The task, which nothing has run.
    task: Arc<Task>,
    /// The processor it is placed on.
    cpu: usize,
    /// That processor's queue, resolved when it was prepared.
    queue: &'static SpinLock<CpuQueue>,
    /// The thread whose process's live-thread count it holds.
    thread: Arc<dyn UserThread>,
    /// Set by [`PreparedTask::launch`], after which a drop gives nothing back.
    launched: bool,
}

impl PreparedTask {
    /// Put it on its processor's queue, where it runs.
    pub(crate) fn launch(mut self) -> Arc<Task> {
        self.launched = true;
        enqueue(&self.task, self.cpu, self.queue);
        Arc::clone(&self.task)
    }
}

impl Drop for PreparedTask {
    /// Free the stack of a task never launched, then give its count back.
    fn drop(&mut self) {
        if self.launched {
            return;
        }
        debug_assert!(
            arch::interrupts_enabled(),
            "a prepared task was dropped with interrupts off, and freeing its stack waits for \
             every processor"
        );
        if let Some(stack) = self.task.stack() {
            // SAFETY: (KMEM) the task was never on a queue, so no processor has run on
            // its stack, and none will: this is its only reference.
            let _ = unsafe { crate::vmap::free_stack(stack) };
        }
        self.thread.process().thread_gone(false);
    }
}

/// Make a task and put it on `cpu`'s queue: the path every kernel task takes.
#[expect(
    clippy::too_many_arguments,
    reason = "private, with two callers that name every argument; a struct would be `NewTask` again"
)]
fn spawn_task(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    weight: u32,
    cpu: usize,
    affinity: CpuSet,
    address_space: Option<Arc<crate::user::space::AddressSpace>>,
    thread: Option<Arc<dyn UserThread>>,
    user_state: Option<arch::UserState>,
) -> Result<Arc<Task>, &'static str> {
    // The queue before the task: a task made for a processor that does not
    // exist would take a stack with it that nothing frees.
    let queue = queue_of(cpu).ok_or("no such processor")?;
    let task = make_task(
        name,
        entry,
        argument,
        weight,
        cpu,
        affinity,
        address_space,
        thread,
        user_state,
    )?;
    enqueue(&task, cpu, queue);
    Ok(task)
}

/// Make a task, with a stack laid out to begin at [`task_start`], on no queue.
#[expect(
    clippy::too_many_arguments,
    reason = "private, with two callers that name every argument; a struct would be `NewTask` again"
)]
fn make_task(
    name: &'static str,
    entry: fn(usize),
    argument: usize,
    weight: u32,
    cpu: usize,
    affinity: CpuSet,
    address_space: Option<Arc<crate::user::space::AddressSpace>>,
    thread: Option<Arc<dyn UserThread>>,
    user_state: Option<arch::UserState>,
) -> Result<Arc<Task>, &'static str> {
    let stack = crate::vmap::allocate_stack().map_err(|problem| {
        // The arena's own reason, because "no stack" has four of them and they
        // want four different fixes: no address space, no frames, the page
        // tables refusing, or the arena not being up at all.
        crate::console::println!("  tasks    no stack for {name}: {problem}");
        "no kernel stack for a new task"
    })?;
    // SAFETY: (CONTEXT) the stack was allocated a moment ago, is mapped and writable,
    // and nothing else refers to it.
    let stack_pointer = unsafe { arch::prepare_stack(stack.top, task_start, 0) };
    let task = Task::new(task::NewTask {
        id: next_id(),
        name,
        entry,
        argument,
        stack,
        stack_pointer,
        weight,
        cpu,
        affinity,
        address_space,
        thread,
        user_state,
    })
    .and_then(fallible::try_arc);
    task.map_err(|_| {
        // SAFETY: (KMEM) allocated above and never run on.
        let _ = unsafe { crate::vmap::free_stack(stack) };
        "no memory for a new task"
    })
}

/// Put a task made for `cpu` on `lock`, that processor's queue, and see that
/// it is noticed. Cannot fail: the queue was resolved before the task was made.
fn enqueue(task: &Arc<Task>, cpu: usize, lock: &'static SpinLock<CpuQueue>) {
    let saved = <arch::Irq as IrqControl>::disable();
    let (preempt, stealable) = {
        let mut queue = lock.lock();
        // NOALLOC: `CpuQueue::insert` queues the task in its own run slot.
        queue.insert(task);
        // Three reasons to make the target reschedule, and the third is the
        // one that cost a day. Either something better than what it is
        // running has arrived; or it is not running anything at all and has
        // to be woken to notice; or **this is the first task to be made to
        // wait behind the one it is running**, which means its timer is
        // currently switched off.
        //
        // That last case is not an optimisation, it is a hang. `arm_timer`
        // deliberately leaves a processor alone when nothing is waiting —
        // that is where tickless comes from — so a processor running one task
        // has stopped its timer. Adding a second task from *another*
        // processor does not re-arm it, because only its owner can, and if
        // `should_preempt` says the newcomer should not go first then nothing
        // else would have told it. The result is a processor that runs its
        // one task forever with others queued behind it, which is exactly
        // what the fairness check saw: one spinner with a switch count in the
        // thousands and two with none.
        let wake = queue.should_preempt() || queue.is_running_idle() || queue.waiting() == 1;
        // More than the one task running means there is something here for an
        // idle processor to take. Measured after the insert, so the first task
        // to make the queue worth stealing from is the one that says so.
        (wake, queue.len() > 1)
    };
    // Decided while still masked: a spawn onto this processor arms this
    // processor's timer, which is only this processor's while nothing can move
    // the caller elsewhere.
    let here = this_cpu() == Some(cpu);
    if preempt && here {
        resched_here(cpu);
    }
    <arch::Irq as IrqControl>::restore(saved);

    if preempt && !here {
        kick(cpu);
    }

    // **Tell the idle processors, or they will sleep through this.** An idle
    // processor looks for work to steal and then halts, and nothing wakes it
    // but an interrupt: a queue that fills up on another processor after it
    // halted is invisible to it forever. The first stage-5 run found exactly
    // that — a thousand tasks spawned on one processor, three processors
    // asleep, and stealing that moved nothing.
    //
    // Broadcast and unconditional rather than aimed at the processors that
    // are actually idle: an idle processor is only idle until it looks, so
    // any answer to "which ones" is stale before it is used. One woken
    // processor that finds nothing costs a halt and a wake; the alternative
    // costs the whole machine minus one.
    if stealable {
        wake_idle_processors();
    }
}

/// Wake every other processor so that anything idle looks for work to steal.
///
/// Separate from [`kick`], which is about a specific processor needing to
/// reschedule. This one carries no request at all: the interrupt exists only
/// to return an idle processor to the top of its loop, where it looks.
fn wake_idle_processors() {
    // The fence orders the caller's enqueue before this read of the mask,
    // against the idle loop's setting of its bit before its look: see `IDLE`.
    core::sync::atomic::fence(Ordering::SeqCst);
    if IDLE.load(Ordering::SeqCst) != 0 {
        let _ = arch::send_ipi_to_others();
    }
}

/// Where every task begins.
///
/// Reached from the architecture's trampoline, on a stack `prepare_stack`
/// laid out, with the run queue lock still held by the switch that got here.
extern "C" fn task_start(_argument: usize) -> ! {
    finish_switch();
    arch::enable_interrupts();

    // The task's own reference is dropped before the entry runs, not after
    // it. An entry that never returns -- a program entering user mode, which
    // ends through `exit_group` or a kill -- would otherwise keep it on this
    // frame for good, and with it the task, its process and its address space.
    // NOALLOC: `Task::entry` reads a field.
    let entry = current().and_then(|task| task.entry());
    if let Some((entry, argument)) = entry {
        entry(argument);
    }
    exit()
}

/// End the running task.
pub(crate) fn exit() -> ! {
    // **Nothing switches it out while this frame holds the task.** From the
    // moment it is marked dead, a switch takes it off its queue for good and
    // the reaper frees this stack, with whatever it held still held: the
    // task's own `Arc`, taken here to mark it, leaked with its hold on its
    // job's quota slot and, for a program, its thread's process. So the
    // reference is dropped before interrupts come back on.
    let saved = <arch::Irq as IrqControl>::disable();
    if let Some(task) = current() {
        // Whatever sleep it once meant to take is over: a deadline left here
        // is one `choose_next` would otherwise file the dead task under.
        let _ = task.take_sleep_deadline();
        // Counted before it is marked dead: a check waiting for the reaper
        // must see this task from the moment nothing else sees it running.
        let _ = EXITED_UNREAPED.fetch_add(1, Ordering::AcqRel);
        task.set_state(DEAD);
        drop(task);
    }
    <arch::Irq as IrqControl>::restore(saved);
    schedule();
    // A dead task is never picked again, so this is not reached. If it ever
    // were, stopping is the only answer that cannot corrupt anything.
    loop {
        arch::wait_for_interrupt();
    }
}

/// The task running on this processor.
pub(crate) fn current() -> Option<Arc<Task>> {
    let saved = <arch::Irq as IrqControl>::disable();
    let task = this_cpu()
        .and_then(queue_of)
        .and_then(|lock| lock.lock().current.clone());
    <arch::Irq as IrqControl>::restore(saved);
    task
}

/// Real nanoseconds the task running on this processor has run for,
/// charged up to this instant: what `CLOCK_THREAD_CPUTIME_ID` reads. Zero
/// where nothing runs, before the scheduler has started.
///
/// The queue is charged under its lock, as [`for_each_queue_charged`] does,
/// so a reading taken mid-slice counts the slice so far and two readings
/// never go backwards. The task is released after the lock is.
pub(crate) fn current_runtime() -> u64 {
    let saved = <arch::Irq as IrqControl>::disable();
    let task = this_cpu().and_then(queue_of).and_then(|lock| {
        let mut queue = lock.lock();
        queue.account(crate::timer::now_nanos());
        queue.current.clone()
    });
    <arch::Irq as IrqControl>::restore(saved);
    task.map_or(0, |task| task.runtime())
}

/// Give up the rest of this task's slice.
pub(crate) fn yield_now() {
    let saved = <arch::Irq as IrqControl>::disable();
    if let Some(lock) = this_cpu().and_then(queue_of) {
        lock.lock().fair.yield_curr();
    }
    <arch::Irq as IrqControl>::restore(saved);
    schedule();
}

/// Block the running task until `deadline`, by the counter's reckoning.
pub(crate) fn sleep_until(deadline: u64) {
    let Some(task) = current() else {
        while crate::timer::now_nanos() < deadline {
            core::hint::spin_loop();
        }
        return;
    };
    task.set_sleep_deadline(deadline);
    task.set_state(task::BLOCKED);
    schedule();
}

/// Block the running task for `nanos`.
pub(crate) fn sleep_for(nanos: u64) {
    sleep_until(crate::timer::now_nanos().saturating_add(nanos));
}

/// Block the running task, which has already marked itself blocked.
fn block() {
    schedule();
}

/// Make `task` runnable, wherever it is.
pub(crate) fn wake(task: &Arc<Task>) {
    // **Not re-placed here, and not detached from its sleeper set here
    // either.** Both were tried and both were withdrawn.
    //
    // Choosing a new processor at wake-up is what Linux does and is the
    // better policy. Taking a woken task out of its processor's sleeper set
    // looks like plain hygiene. Each was reverted after making an
    // already-flaky machine reliably worse — the second one wedged every one
    // of five runs, where the first had wedged three.
    //
    // The reason both are harder than they look is the same: a blocked task
    // is not an unattached one, and "blocked" covers several states this code
    // does not currently distinguish. A task can be on a wait queue, in a
    // sleeper set, part-way into `block` and in neither yet, or on both. A
    // waker that reasons about only one of them moves or unfiles a task that
    // something else still believes it owns, and the task is lost rather than
    // run. Getting it right means giving those states names and an order,
    // which is a change of its own and not a corollary of five others.
    let saved = <arch::Irq as IrqControl>::disable();
    let mut kick_cpu = None;
    loop {
        let cpu = task.cpu();
        let Some(lock) = queue_of(cpu) else {
            break;
        };
        let mut queue = lock.lock();
        // It may have moved to another processor between the read and the
        // lock, in which case this is the wrong queue and the wrong lock.
        if task.cpu() != cpu {
            continue;
        }
        if task.state() != task::BLOCKED {
            break;
        }
        // Off the sleeper set as well as onto the run queue: waking a task
        // early does not cancel the deadline it was filed under, and a stale
        // entry wakes it again out of its next sleep. See `remove_sleeper`.
        let _ = queue.remove_sleeper(task.id);
        // **And the deadline itself, if it was never filed.** A task woken
        // after marking itself blocked but before it reached `block` is still
        // running, so `choose_next` never took its deadline, and nothing else
        // would. It then sat on the task until the next time the task left
        // its processor for any reason, exiting included, and filed it as a
        // sleeper there: a dead task, made runnable by the timer on a stack
        // the reaper was freeing.
        let _ = task.take_sleep_deadline();
        task.set_state(RUNNABLE);
        if !task.is_queued() {
            // NOALLOC: `CpuQueue::insert` queues the task in its own run slot.
            queue.insert(task);
        }
        // As `spawn_on`, and for the same three reasons: something better has
        // arrived, or the processor is idle, or this is the first task to
        // wait behind the running one and so the first that needs its timer
        // to exist.
        if queue.should_preempt() || queue.is_running_idle() || queue.waiting() == 1 {
            kick_cpu = Some(cpu);
        }
        break;
    }
    // As `spawn_task`: a wake-up onto this processor arms this processor's
    // timer, so it is decided and done before interrupts come back.
    let here = this_cpu();
    let remote = match kick_cpu {
        Some(cpu) if here == Some(cpu) => {
            resched_here(cpu);
            None
        }
        other => other,
    };
    <arch::Irq as IrqControl>::restore(saved);

    if let Some(cpu) = remote {
        kick(cpu);
    }
}

/// Make the processor `task` is on take an interrupt, so that if it is running
/// in user mode there it comes back through the kernel.
///
/// For ending a program from outside. A task running in user mode only enters
/// the kernel when it makes a call or an interrupt arrives, and a task alone on
/// its processor gets no timer tick, because [`queue::CpuQueue::arm_timer`]
/// leaves a lone task to run. Nothing is needed when the task is on this
/// processor: then it is not the one running.
pub(crate) fn interrupt(task: &Arc<Task>) {
    let cpu = task.cpu();
    if this_cpu() != Some(cpu) {
        kick(cpu);
    }
}

/// Give `task` a new scheduling weight, and ask for a decision if that has
/// changed which task should be running.
///
/// What a nice value means, once it has been turned into a weight: see
/// [`crate::syscall::attributes::sys_setpriority`]. The queue holding the
/// task is found and locked the way [`wake`] finds it, because a task moves
/// between processors and the weight has to reach the queue that really has
/// it; a task on no queue still has its own record set, and is enqueued with
/// it next time.
pub(crate) fn set_weight(task: &Arc<Task>, weight: u32) {
    // The task's own weight; its job's share scales what the queue is told.
    task.set_base_weight(weight);
    let weight = task.effective_weight();
    let saved = <arch::Irq as IrqControl>::disable();
    let mut kick_cpu = None;
    loop {
        let cpu = task.cpu();
        let Some(lock) = queue_of(cpu) else {
            task.set_weight(weight);
            break;
        };
        let mut queue = lock.lock();
        // It may have moved between the read and the lock, as in `wake`.
        if task.cpu() != cpu {
            continue;
        }
        queue.set_weight(task, weight);
        // A task made heavier may now deserve the processor its own change
        // just took it off the front of, and one made lighter may owe it to
        // somebody else. Either way the decision is due now rather than at
        // the end of a slice granted under the old weight.
        if queue.should_preempt() {
            kick_cpu = Some(cpu);
        }
        break;
    }
    // As `wake`: a processor asked to reschedule itself is told before
    // interrupts come back, and another is interrupted once they have.
    let here = this_cpu();
    let remote = match kick_cpu {
        Some(cpu) if here == Some(cpu) => {
            resched_here(cpu);
            None
        }
        other => other,
    };
    <arch::Irq as IrqControl>::restore(saved);

    if let Some(cpu) = remote {
        kick(cpu);
    }
}

/// What a timer interrupt does: ask for a decision on the way out.
pub(crate) fn timer_expired() {
    if let Some(cpu) = this_cpu() {
        mark_resched(cpu);
    }
}

/// Make the decision an interrupt asked for, on the way out of it.
///
/// `from_user` says whether the interrupt arrived in user mode. A task this
/// switches out while it is still runnable was then preempted *as a user
/// program*, in the middle of its own code, which is the one kind of
/// preemption the turn-taking check counts: see `Task::preemptions`.
pub(crate) fn preempt_on_irq_exit(from_user: bool) {
    if !started() {
        return;
    }
    let Some(cpu) = this_cpu() else {
        return;
    };
    // Any interrupt's exit does what a kick's would: see `kick`.
    if let Some(mark) = KICK_PENDING.get().and_then(|marks| marks.get(cpu)) {
        let _ = mark.swap(false, Ordering::AcqRel);
    }

    // Before the switch, not after: `schedule` may not come back to this
    // context for a while, and a balance that runs on the way out of every
    // timer interrupt should not be skipped whenever there is also a
    // reschedule to do.
    balance();

    // **Not while the running context has asked to stay.** The flag is left
    // set, so the decision is made when the count comes back to zero, in
    // `preempt_enable`, or at the next interrupt exit. See `PREEMPT_OFF`.
    if preempt_count(cpu) > 0 {
        return;
    }
    if take_resched(cpu) {
        schedule_from(from_user);
    }
}

/// Whether an interrupt asked `cpu` to reschedule, clearing the request.
fn take_resched(cpu: usize) -> bool {
    NEED_RESCHED
        .get()
        .and_then(|flags| flags.get(cpu))
        .is_some_and(|flag| flag.swap(false, Ordering::AcqRel))
}

/// Whether any processor's idle task is in the middle of freeing a stack.
///
/// For a check that is about to count frames: the zombie list being empty
/// says nothing about a stack already taken off it and part-way through
/// `vmap::free`, whose bookkeeping can take or return a heap page any moment
/// now.
pub(crate) fn reaping_anywhere() -> bool {
    REAPING
        .get()
        .is_some_and(|flags| flags.iter().any(|flag| flag.load(Ordering::Acquire)))
}

/// How long a check counting frames waits for the reaper. Generous: stage 5
/// ends having started a thousand tasks, and a loaded host is slow to reap.
pub(crate) const REAPER_PATIENCE_NANOS: u64 = 20_000_000_000;

/// Wait until every task that has exited has been reaped, reaping on this
/// processor meanwhile. `Err` when `patience_nanos` pass first.
///
/// For a check counting free frames, at both edges of its window: a reaper
/// freeing a stack or dropping a task inside the window moves the count
/// either way. A condition rather than a delay, because a delay only makes
/// the race rarer: no task exited and not yet dropped by its reaper, nothing
/// on the zombie list, and no idle processor part-way through a free. What it
/// does not wait for is a reference the caller holds to a task, a process or
/// an address space: that is the caller's to drop, and to wait for.
pub(crate) fn wait_until_reaper_quiet(patience_nanos: u64) -> Result<(), &'static str> {
    wait_for_reaper(patience_nanos)
}

/// Wait until `task` has exited and been reaped, so that nothing it holds --
/// its kernel stack, and through its thread a process and an address space --
/// comes back after the caller's frame window opens.
///
/// For a check that starts a task and counts frames. A task that has answered
/// the check is not yet gone: it still has to leave, and
/// [`wait_until_reaper_quiet`] counts only tasks already dead, so without this
/// its stack is freed inside whichever window opens next, four frames that
/// window never took. Drop the `Arc<Task>` after this returns.
///
/// # Errors
///
/// When the task is not dead, or the reaper not quiet, within
/// `patience_nanos`.
pub(crate) fn wait_until_gone(task: &Task, patience_nanos: u64) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(patience_nanos);
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err("a task the check started never finished exiting");
        }
        sleep_for(GONE_POLL_NANOS);
    }
    wait_for_reaper(deadline.saturating_sub(crate::timer::now_nanos()))
}

/// How often [`wait_until_gone`] looks at its task.
const GONE_POLL_NANOS: u64 = 1_000_000;

/// [`wait_until_reaper_quiet`]'s body.
fn wait_for_reaper(patience_nanos: u64) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(patience_nanos);
    loop {
        // Here as well as in the idle loops: a yield never picks this
        // processor's idle task while the caller is runnable. Only when there
        // is something to reap, since `reap` shoots down every processor even
        // when there is not.
        if !ZOMBIES.lock().is_empty() {
            let _ = reap();
        }
        if EXITED_UNREAPED.load(Ordering::Acquire) == 0
            && ZOMBIES.lock().is_empty()
            && !reaping_anywhere()
        {
            return Ok(());
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("the reaper never went quiet before the frame count");
        }
        yield_now();
    }
}

/// Say whether this processor's idle task holds a stack it is freeing.
fn set_reaping(cpu: usize, reaping: bool) {
    if let Some(flag) = REAPING.get().and_then(|flags| flags.get(cpu)) {
        flag.store(reaping, Ordering::Release);
    }
}

/// Give the processor to whatever should have it now.
fn schedule() {
    schedule_from(false);
}

/// `schedule`, saying whether the decision was forced on a user program by
/// an interrupt that arrived in user mode -- the case a task counts as a
/// preemption if it is switched out still runnable.
fn schedule_from(interrupted_user: bool) {
    let saved = <arch::Irq as IrqControl>::disable();
    // A switch with the count raised is a holder of a preemption-disabling
    // lock going to sleep, which the count cannot survive: see `PREEMPT_OFF`.
    if let Some(cpu) = this_cpu()
        && preempt_count(cpu) > 0
    {
        let site = preempt_site(cpu);
        crate::panic::fatal!(
            crate::panic::catalog::SCHEDULE_WITH_PREEMPTION_HELD,
            "a task blocked or yielded while holding a lock that disables preemption ({} held; \
             the last was taken at {}:{})",
            preempt_count(cpu),
            site.map_or("?", |site| site.file()),
            site.map_or(0, core::panic::Location::line),
        );
    }
    pick_and_switch(interrupted_user);
    <arch::Irq as IrqControl>::restore(saved);
}

/// With interrupts masked: decide, and switch if the decision changed
/// anything. Returns possibly much later, and possibly on another processor.
fn pick_and_switch(interrupted_user: bool) {
    let Some(cpu) = this_cpu() else {
        return;
    };
    let Some(lock) = queue_of(cpu) else {
        return;
    };
    let Some((save, resume)) = choose_next(lock, cpu, interrupted_user) else {
        return;
    };
    // SAFETY: (CONTEXT) `save` is this context's own slot and `resume` a stack pointer
    // this module prepared or saved; this processor holds the run queue's
    // lock, which keeps every other processor off both until `finish_switch`
    // releases it.
    unsafe { arch::switch_to(save, resume) };
    finish_switch();
}

/// Choose what runs next, leaving the queue's lock held and returning where
/// to save this context and what to resume — or releasing the lock and
/// returning `None` when nothing has to change.
fn choose_next(
    lock: &'static SpinLock<CpuQueue>,
    cpu: usize,
    interrupted_user: bool,
) -> Option<(*mut u64, u64)> {
    // SAFETY: (SHARED) released below when nothing is switched, and otherwise by the
    // context this switches to, in `finish_switch`.
    let queue = unsafe { lock.lock_manually() };

    let now = crate::timer::now_nanos();
    queue.account_in(now, interrupted_user);
    queue.wake_sleepers(now);

    let previous = queue.current.clone();
    if let Some(previous) = previous.as_ref().filter(|task| task.state() != RUNNABLE) {
        queue.detach_current();
        // Taken either way, filed only for a task that can wake: a dead task
        // in the sleeper set is a dead task the timer makes runnable.
        if let Some(at) = previous.take_sleep_deadline()
            && !previous.is_dead()
            && !queue.file_sleeper(at, previous)
        {
            // No sleep slot to file it in: it runs on, and its sleep returns
            // early and asks again. See `CpuQueue::file_sleeper`.
            previous.set_state(RUNNABLE);
            // NOALLOC: `CpuQueue::insert` queues the task in its own run slot.
            queue.insert(previous);
        }
    }

    let next = queue.pick_next();
    if queue.stats.measuring {
        queue.note_pick(now);
    }
    let switching = match (previous.as_ref(), next.as_ref()) {
        (Some(previous), Some(next)) => !Arc::ptr_eq(previous, next),
        (None, Some(_)) => true,
        _ => false,
    };

    if !switching {
        queue.arm_timer(now);
        // SAFETY: (SHARED) taken above, and nothing was switched, so this context is
        // still the holder.
        unsafe { lock.force_unlock() };
        return None;
    }

    let (previous, next) = (previous?, next?);
    queue.stats.switches += 1;
    // Still runnable, cut off in its own code by an interrupt, and yet
    // leaving: a preemption of a user program, the one thing a check about
    // taking turns can count. Not a switch made on the way out of a system
    // call, which is the program's own doing.
    if interrupted_user && previous.state() == RUNNABLE {
        previous.note_preemption();
    }
    queue.previous = Some(Arc::clone(&previous));
    queue.current = Some(Arc::clone(&next));
    note_running(cpu, next.id, next.group(), next.moves_seen());
    // Idle to the rest of the machine exactly while the idle task is what
    // runs: cleared here, before any other task can, and set again when the
    // idle task comes back. The idle loop's own clear came too late for the
    // commonest wake -- an interrupt ending the halt and switching straight to
    // the task it woke, the idle task preempted before its next line -- and
    // the processor read as idle for as long as that task ran. Set again on
    // the way back because the idle task may have been preempted after
    // setting the mark and before its look, and would otherwise halt
    // unmarked, where a spawn's interrupt never reaches it. See `IDLE`.
    set_idle(
        Some(cpu),
        queue
            .idle
            .as_ref()
            .is_some_and(|idle| Arc::ptr_eq(idle, &next)),
    );
    queue.exec_start = now;
    queue.arm_timer(now);
    next.note_switch(cpu);

    // The address space goes on the processor here, under the run queue lock
    // and before the registers move. Not inside `arch::switch_to`, which takes
    // two stack pointers and whose whole job is register operations -- and not
    // after the switch either, because the incoming context resumes on its own
    // stack and would have to be told to do this before touching anything.
    swap_address_space(previous.address_space(), next.address_space());
    switch_user_state(&previous, &next);

    // SAFETY: (SHARED) both tasks belong to this queue and this processor holds its
    // lock, so nothing else may read or write either saved stack pointer.
    let save = unsafe { previous.stack_pointer_slot() };
    // SAFETY: (SHARED) as above.
    let resume = unsafe { next.saved_stack_pointer() };
    Some((save, resume))
}

/// Put the incoming task's address space on this processor.
///
/// Called from [`choose_next`] with the run queue lock held, between deciding
/// to switch and the switch itself.
///
/// # Why the comparison is by pointer
///
/// Because two threads of one process share a root, and an address space
/// switch is the expensive operation this whole stage declined to optimise:
/// stage 6 allocates no `ASID`s or `PCID`s, so installing a root invalidates
/// every user translation this processor had. Switching between two threads of
/// one process must therefore cost nothing, and `Arc::ptr_eq` is what says they
/// are the same space rather than two equal ones.
///
/// # What `None` means, and why it is not lazy
///
/// A kernel thread has no user half, and gets the user half switched *off*
/// rather than left as it was. Leaving the outgoing process's root installed is
/// Linux's lazy TLB and it is faster, and it obliges somebody to keep an
/// address space alive underneath a thread that holds no reference to it. Stage
/// 6 takes the plain version; the reference this relies on is the `Arc` the
/// task itself holds.
///
/// # What this trusts
///
/// That `previous` is what is actually installed on this processor. That holds
/// because `previous` is the queue's `current`, which is the task this
/// processor was running, and the only thing that installs a root is this
/// function. A task may change processor while it is *blocked* -- `balance`
/// moves queued tasks from a third processor -- but a blocked task is not
/// anybody's `current`, so it cannot be the `previous` of a switch it is not
/// part of.
fn swap_address_space(
    previous: Option<&Arc<crate::user::space::AddressSpace>>,
    next: Option<&Arc<crate::user::space::AddressSpace>>,
) {
    match (previous, next) {
        // Two threads of one process, or two kernel threads: nothing to do,
        // and doing it anyway would throw away every user translation.
        (Some(before), Some(after)) if Arc::ptr_eq(before, after) => {}
        (None, None) => {}
        // SAFETY: (TRANSLATE) `next` is the task this processor is about to run, and the
        // queue holds an `Arc` to it for as long as it is `current`, so the
        // tables outlive the installation. Interrupts are off and the run
        // queue lock is held, so nothing else can install a root here first.
        (before, Some(after)) => unsafe { after.install(before.map(|space| &**space)) },
        // SAFETY: (TRANSLATE) the incoming task is a kernel thread and wants no user
        // address; the kernel is reachable without one on every architecture.
        (Some(before), None) => unsafe { before.uninstall() },
    }
}

/// Move the user registers no trap saves from the outgoing task to the
/// incoming one.
///
/// Called from [`choose_next`] with the run queue lock held, like the address
/// space swap beside it, and for the same reason: the incoming context resumes
/// on its own stack, possibly deep inside a trap it is about to return from to
/// user mode, and it must find its own thread pointer and floating-point state
/// already loaded.
///
/// Eager rather than lazy. A kernel thread switched in between two programs
/// costs a save it did not need, because the kernel never touches these
/// registers; lazy switching would skip that and needs a trap on first use to
/// know when to catch up, which is a mechanism of its own for later.
///
/// A dead task's state is not saved: nothing will ever load it.
fn switch_user_state(previous: &Arc<Task>, next: &Arc<Task>) {
    if !previous.is_dead() {
        // SAFETY: (SHARED) this processor holds the run queue lock that owns `previous`.
        if let Some(state) = unsafe { previous.user_state() } {
            // SAFETY: (SHARED) the pointer is to `previous`'s own boxed state, which
            // nothing else touches while the lock is held.
            let state = unsafe { &mut *state };
            // SAFETY: (CONTEXT) `previous` is the task this processor was running, so the
            // registers are its.
            unsafe { arch::save_user_state(state) };
        }
    }
    // SAFETY: (SHARED) as above, for `next`.
    if let Some(state) = unsafe { next.user_state() } {
        let entry_stack = next.stack_top().unwrap_or(0);
        // SAFETY: (SHARED) as above, `next`'s own boxed state under the queue lock.
        let state = unsafe { &*state };
        // SAFETY: (CONTEXT) `next` is the task this processor is switching to, and its
        // stack is its own and mapped for as long as the queue holds it.
        unsafe { arch::restore_user_state(state, entry_stack) };
    }
}

/// Release the lock the switch handed over, and dispose of what ran before.
fn finish_switch() {
    let Some(lock) = this_cpu().and_then(queue_of) else {
        return;
    };
    // SAFETY: (SHARED) this processor holds this lock — either it took it in
    // `choose_next` and switched to here, or the context that switched to
    // this one did and handed it over.
    let queue = unsafe { lock.locked_data() };
    let previous = queue.previous.take();
    let dead = previous.as_ref().is_some_and(|task| task.is_dead());
    // **The last moment a dead task's queue membership means anything.** It is
    // switched away from for good, and anything that still counts it as
    // queued will pick it again, on a stack the reaper is about to free. Read
    // under the lock, which is what orders it against the detach in
    // `choose_next`, and recorded rather than returned: nothing here can
    // report, and a check that looked at `previous` later always found it
    // already taken, so it could not fail.
    if dead && previous.as_ref().is_some_and(|task| task.is_queued()) {
        let _ = DEAD_STILL_QUEUED.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: (SHARED) held as above, released exactly once, and the queue is not
    // touched afterwards.
    unsafe { lock.force_unlock() };

    if let Some(previous) = previous
        && dead
    {
        // NOALLOC: `Zombies::push` files the task in its run slot.
        ZOMBIES.lock().push(previous);
    }
}

/// Take a task from the busiest other processor in this domain, if one will
/// come.
///
/// Only an idle processor steals, and only from a queue with something
/// waiting rather than merely something running: taking the task another
/// processor is running is not possible, and taking its last waiting one is
/// exactly the work it is about to do itself.
fn steal_work() -> bool {
    let Some(domain) = DOMAIN.get().filter(|domain| domain.mode().steals_work()) else {
        return false;
    };
    let Some(me) = this_cpu() else {
        return false;
    };
    domain
        .cpus()
        .iter()
        .filter(|cpu| *cpu != me)
        .any(|victim| steal_from(me, victim))
}

/// How often a processor looks for an imbalance worth correcting.
///
/// Work stealing already covers the case that matters most — a processor with
/// nothing to do — and it costs nothing, because a processor about to idle is
/// not busy. This is the other case: every processor has work, and one has
/// much more of it. Nothing about that is urgent, and looking often would mean
/// taking every run queue's lock often, so it is deliberately slow.
const BALANCE_INTERVAL_NS: u64 = 16_000_000;

/// When each processor may next look for an imbalance.
static NEXT_BALANCE: Once<Vec<AtomicU64>> = Once::new();

/// Look for work worth pulling from a busier processor, and pull one task.
///
/// Called on the way out of a timer interrupt, from a processor that is
/// *running something* — the idle case is `steal_work`. Rate-limited per
/// processor, and it takes no lock at all in the common case where the
/// interval has not elapsed.
fn balance() {
    let Some(me) = this_cpu() else {
        return;
    };
    let now = crate::timer::now_nanos();
    let Some(next) = NEXT_BALANCE.get().and_then(|times| times.get(me)) else {
        return;
    };
    let due = next.load(Ordering::Relaxed);
    if now < due {
        return;
    }
    // Claimed with a compare-exchange rather than a store, so that two
    // interrupts racing here do not both go on to lock every queue.
    if next
        .compare_exchange(
            due,
            now.saturating_add(BALANCE_INTERVAL_NS),
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }

    let Some(allowed) = domain_cpus() else {
        return;
    };
    let Some(queues) = QUEUES.get() else {
        return;
    };
    let Some(mine) = queue_of(me) else {
        return;
    };

    // **One processor at a time, and never into an array.** This runs on the
    // way out of an interrupt, on the stack of whatever it interrupted, and
    // `[CpuLoad; MAX_CPUS]` is six kilobytes of the sixteen a kernel stack
    // has. The guard page below it turned that into a fault the fault handler
    // had no stack to report, which is a silent wedge rather than a panic.
    let saved = <arch::Irq as IrqControl>::disable();
    let mut folded = {
        let mut queue = mine.lock();
        queue.account_load(now);
        Balance::new(queue.snapshot())
    };
    for (cpu, lock) in queues.iter().enumerate() {
        if cpu == me || !allowed.contains(cpu) {
            continue;
        }
        let snapshot = {
            let mut queue = lock.lock();
            queue.account_load(now);
            queue.snapshot()
        };
        folded.consider(cpu, snapshot);
    }
    <arch::Irq as IrqControl>::restore(saved);

    // Pull first: if somebody is busier than this processor, take from them.
    if let Some(victim) = folded.pull_from() {
        let _ = pull(me, victim);
        return;
    }

    // Otherwise push. **This is the one that matters on a tickless kernel.**
    // A processor alone with one task is never interrupted — `arm_timer`
    // deliberately leaves it alone, because there is nothing to switch to —
    // so it never reaches this function to pull anything towards itself. The
    // overloaded processor is interrupted constantly, precisely because it
    // has tasks to switch between, so it is the only one awake to notice and
    // it has to do the moving.
    //
    // Found by the check below failing with six thousand balance attempts and
    // nothing moved: every one of them was made by the overloaded processor,
    // looking for somebody busier than itself.
    if let Some(target) = folded.push_to()
        && steal_from(target, me)
    {
        let _ = BALANCED.fetch_add(1, Ordering::Relaxed);
        // The receiver may have been asleep with nothing to run; tell it.
        kick(target);
    }
}

/// Take one task from `victim` for `me`, which is this processor, and make
/// sure it gets a turn.
///
/// **Moving it is not enough.** `me` is running something, and if that is all
/// it was running its timer is stopped, so a task added behind it waits until
/// something asks `me` to decide again. Nothing did. `balance` runs on an
/// interrupt exit that asked for no reschedule, or it would have been spent
/// on one, and wake-ups onto `me` found two tasks there and so saw no reason
/// to kick. The balancing check lost its movable tasks this way, one boot in
/// a few: an anchor-only processor took a placement's broadcast, pulled a
/// movable spinner, and never started it. That is "a balancing task never
/// started". On an interrupt exit the flag is read right after `balance`
/// returns, and the timer covers any other caller.
fn pull(me: usize, victim: usize) -> bool {
    if !steal_from(me, victim) {
        return false;
    }
    let _ = BALANCED.fetch_add(1, Ordering::Relaxed);
    let saved = <arch::Irq as IrqControl>::disable();
    if this_cpu() == Some(me) {
        resched_here(me);
    } else {
        kick(me);
    }
    <arch::Irq as IrqControl>::restore(saved);
    true
}

/// Tasks moved by [`balance`], as opposed to by an idle processor stealing.
static BALANCED: AtomicU64 = AtomicU64::new(0);

/// Spawns that `choose_cpu` sent to a processor other than the caller's.
static PLACED_ELSEWHERE: AtomicU64 = AtomicU64::new(0);

/// How many new tasks placement has sent off their creator's processor.
pub(crate) fn placed_elsewhere() -> u64 {
    PLACED_ELSEWHERE.load(Ordering::Relaxed)
}

/// How many tasks periodic balancing has moved.
pub(crate) fn balanced_count() -> u64 {
    BALANCED.load(Ordering::Relaxed)
}

/// Move one task from `victim`'s queue to `me`'s.
fn steal_from(me: usize, victim: usize) -> bool {
    let (Some(mine), Some(theirs)) = (queue_of(me), queue_of(victim)) else {
        return false;
    };

    let saved = <arch::Irq as IrqControl>::disable();
    // Lowest processor number first, always, so that two processors stealing
    // from each other cannot each hold what the other wants.
    let (first, second) = if me < victim {
        (mine, theirs)
    } else {
        (theirs, mine)
    };
    let mut first_queue = first.lock();
    let mut second_queue = second.lock();
    let (mine_queue, theirs_queue) = if me < victim {
        (&mut *first_queue, &mut *second_queue)
    } else {
        (&mut *second_queue, &mut *first_queue)
    };

    let moved = match theirs_queue.steal_candidate(me) {
        Some(id) => match theirs_queue.release(id) {
            Some((task, state)) => {
                task.store_entity_state(state);
                task.set_cpu(me);
                // NOALLOC: `CpuQueue::insert` queues the task in its own run slot.
                mine_queue.insert(&task);
                theirs_queue.stats.stolen_out += 1;
                mine_queue.stats.stolen_in += 1;
                true
            }
            None => false,
        },
        None => false,
    };

    drop(second_queue);
    drop(first_queue);
    <arch::Irq as IrqControl>::restore(saved);
    moved
}

/// Free a batch of exited tasks' stacks, and say whether any were freed.
///
/// `whatever_is_there` frees however few are waiting; without it a batch is
/// freed only once [`REAP_BATCH`] of them have gathered. The idle loop asks
/// the second way first and the first way when it has nothing else left to
/// do, which is what keeps the batch from being a batch of one: a thousand
/// threads that exit while their processors are busy gather, and are freed a
/// batch at a time, where reaping each the moment it appeared was a thousand
/// shootdowns.
///
/// The idle loop's reaper, and its shape is the point. [`reap`] takes the
/// whole list and frees it in a loop with interrupts on, which is right for a
/// task that will be scheduled again and wrong for the idle task, which runs
/// only while its processor has nothing else to do. An interrupt in the
/// middle of that loop — a wake-up's kick, a shootdown, the timer — switched
/// the idle task out with the rest of the list still on its stack, and if the
/// task it was switched out for never blocked, the idle task never ran again
/// and the stacks it held were never freed.
///
/// Stage 5's checker is such a task. It blocks until the last of a phase's
/// tasks has finished, and that task's last act wakes it — onto the processor
/// it blocked on, whose idle task may by then have taken every stack freed so
/// far and be part-way through giving them back. The checker then yields in a
/// loop waiting for exactly those stacks, the yield never picks the idle task
/// while the checker is runnable, and twenty seconds later the boot ended
/// with "a task's stack was never given back", short by the size of the
/// batch: one, seven, and once three hundred and ninety-nine. One boot in
/// three to six on a loaded host, on every architecture.
///
/// So: a bounded batch, with [`REAPING`] set while it is held, so that the
/// switch an interrupt asks for waits until the stacks are free — and the
/// idle loop looks at its queue between batches. Interrupts stay on
/// throughout, and must: freeing a stack invalidates other processors'
/// translations and waits for them to say so, and they may be waiting for
/// this one the same way.
///
/// **Bounded, and not one.** One at a time was one shootdown per stack, and
/// a shootdown interrupts every other processor and waits for each to answer
/// before this one goes on. A program whose threads are short-lived — the
/// compositor draws a frame's bands on a thread each — then pays for its own
/// threads in interruptions to every *other* program on the machine, which
/// is the cost landing on the wrong task. [`REAP_BATCH`] stacks go under one
/// shootdown, which is what [`crate::mm::unmap_kernel_all`] is for.
fn reap_batch(whatever_is_there: bool) -> bool {
    let Some(cpu) = this_cpu() else {
        return false;
    };
    // Looked at before the flag and the count go up, so that a processor with
    // nothing to free does not make every other one wait for its preemption
    // count on the way past.
    if !whatever_is_there && ZOMBIES.lock().len() < REAP_BATCH {
        return false;
    }
    // Set before the stacks are taken, not after: between the two is an
    // interrupt exit like any other. The count is what keeps this task on
    // its processor; the flag is for the checks that count frames.
    preempt_disable();
    set_reaping(cpu, true);
    // Room made before the lock is taken, not under it: the list's lock masks
    // interrupts, and a growing `Vec` under it is the heap's lock taken
    // inside this one for no reason. Freeing the stacks below waits for other
    // processors to answer an interrupt, and they may be waiting for this
    // lock to file a zombie of their own, with interrupts masked in turn — so
    // the lock is released before any of that, at the end of this statement.
    let count = reap_up_to(REAP_BATCH);
    note_reaped(count);
    let reaped = count != 0;
    set_reaping(cpu, false);
    preempt_enable();
    reaped
}

/// Free the stacks of tasks that have exited, and return how many.
///
/// For a task that can afford to be switched out part-way, which the idle
/// task cannot: it uses [`reap_batch`].
pub(crate) fn reap() -> usize {
    let waiting = ZOMBIES.lock().len();
    let count = reap_up_to(waiting);
    note_reaped(count);
    count
}

/// Take up to `most` dead tasks off [`ZOMBIES`], free their stacks, and let
/// go of them; answer how many.
///
/// Their stacks go under one shootdown, which is what the lists here are
/// for: freeing a thousand one at a time interrupted every other processor a
/// thousand times. With no memory for the lists, one at a time is what it
/// does -- slower, and needing none (finding F-23).
fn reap_up_to(most: usize) -> usize {
    // Room made before the lock is taken, not under it: the list's lock masks
    // interrupts, and a growing `Vec` under it is the heap's lock taken
    // inside this one for no reason. Freeing the stacks below waits for other
    // processors to answer an interrupt, and they may be waiting for this
    // lock to file a zombie of their own, with interrupts masked in turn — so
    // the lock is released before any of that.
    let (Ok(mut taken), Ok(mut stacks)) = (
        fallible::try_with_capacity::<Arc<Task>>(most),
        fallible::try_with_capacity::<crate::vmap::Stack>(most),
    ) else {
        return reap_one_at_a_time(most);
    };
    {
        let mut zombies = ZOMBIES.lock();
        while taken.len() < most {
            let Some(task) = zombies.pop() else { break };
            if let Some(stack) = task.stack() {
                let _ = fallible::push_within(&mut stacks, stack);
            }
            let _ = fallible::push_within(&mut taken, task);
        }
    }
    if taken.is_empty() {
        return 0;
    }
    // SAFETY: (KMEM) every task is dead and on no queue, and the processor that
    // switched away from it has finished doing so — which is what put it
    // here. Nothing is running on any of these stacks.
    let _ = unsafe { crate::vmap::free_stacks(&stacks) };
    let count = taken.len();
    // Dropping the last reference to a task gives back its address space and
    // its process, which are no better held across a switch than the stacks.
    drop(taken);
    count
}

/// [`reap_up_to`] with no memory for its lists.
fn reap_one_at_a_time(most: usize) -> usize {
    let mut count = 0;
    while count < most {
        let Some(task) = ZOMBIES.lock().pop() else {
            break;
        };
        if let Some(stack) = task.stack() {
            // SAFETY: (KMEM) as in `reap_up_to`.
            let _ = unsafe { crate::vmap::free_stack(stack) };
        }
        drop(task);
        count += 1;
    }
    count
}

/// Take `count` tasks whose reaper has dropped them off [`EXITED_UNREAPED`].
fn note_reaped(count: usize) {
    let exited = EXITED_UNREAPED
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |exited| {
            Some(exited.saturating_sub(count))
        })
        .unwrap_or_else(|exited| exited);
    // Every dead task went through `exit`, which counted it, and is taken off
    // the zombie list once. More reaped than exited is a way to die that
    // skipped the count; saturated rather than wrapped, so no wait hangs on it.
    debug_assert!(exited >= count, "more tasks reaped than exited");
}

/// Start measuring how far `tasks` stray from their shares.
///
/// Only those tasks are counted, and each from its own service now: a task
/// that joins a queue while the window is open was never owed any of what was
/// handed out before it arrived, and counting it would report an arrival as a
/// violation.
///
/// # Why each queue is charged first, under its lock
///
/// A running task is charged only when its processor next makes a decision,
/// so its runtime as read from another processor is stale by up to a slice.
/// A baseline taken from that number counts the stale part as service inside
/// the window: the task looks up to a slice ahead of its share before the
/// scheduler has done anything. With three spinners per processor the slice
/// is a millisecond and the bound three, so the check's own measurement took
/// a third of what it was measuring against — and the fairness check failed,
/// rarely, on a scheduler that was keeping its promise. Charging the running
/// task here, with its queue's lock held so that its processor cannot charge
/// it in between, makes the baseline exact; `close_window` does the same so
/// that the shares read afterwards end where the window did.
pub(crate) fn open_window(tasks: &[Arc<Task>]) {
    for_each_queue_charged(|cpu, queue| {
        // Charged up to now, and then levelled: whatever anyone was owed or
        // ahead by before this instant — including a host stall charged to
        // whichever spinner was running while its siblings were still being
        // made — is not the window's to repay. See `RunQueue::level`.
        queue.fair.level();
        queue.stats.measuring = true;
        queue.stats.worst_lag = 0;
        queue.stats.worst_overrun = 0;
        queue.stats.overrun_total = 0;
        queue.stats.picks = 0;
        queue.stats.wrong_picks = 0;
        queue.trace = [queue::Pick::default(); queue::TRACE_PICKS];
        queue.trace_next = 0;
        for task in tasks.iter().filter(|task| task.cpu() == cpu) {
            task.open_window();
        }
    });
    // A task on no queue this kernel knows of is still counted from now.
    for task in tasks.iter().filter(|task| !task.is_measured()) {
        task.open_window();
    }
}

/// Stop measuring, and remember where each task's runtime stood.
pub(crate) fn close_window(tasks: &[Arc<Task>]) {
    for_each_queue_charged(|cpu, queue| {
        queue.stats.measuring = false;
        for task in tasks.iter().filter(|task| task.cpu() == cpu) {
            task.close_window();
        }
    });
    for task in tasks.iter().filter(|task| task.is_measured()) {
        task.close_window();
    }
}

/// Visit every run queue with its running task charged up to the instant its
/// lock was taken, holding that lock and with interrupts masked throughout.
///
/// The clock is read under each lock rather than once for all: a processor
/// that made a decision between one reading and its lock being taken has an
/// `exec_start` later than that reading, and charging it "up to" an earlier
/// instant would move its start backwards and bill its task twice for the
/// difference.
fn for_each_queue_charged(mut visit: impl FnMut(usize, &mut CpuQueue)) {
    let Some(queues) = QUEUES.get() else {
        return;
    };
    let saved = <arch::Irq as IrqControl>::disable();
    for (cpu, lock) in queues.iter().enumerate() {
        let mut queue = lock.lock();
        queue.account(crate::timer::now_nanos());
        visit(cpu, &mut queue);
    }
    <arch::Irq as IrqControl>::restore(saved);
}

/// What every processor's scheduling has done.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Summary {
    /// Context switches.
    pub(crate) switches: u64,
    /// Tasks moved from one processor to another.
    pub(crate) steals: u64,
    /// The most any task ran past its deadline before being switched out.
    pub(crate) worst_overrun: u64,
    /// The most any task's service strayed from its share while a measurement
    /// window was open.
    pub(crate) worst_lag: u64,
    /// Tasks on the queues, running ones included.
    pub(crate) tasks: usize,
}

/// Add up what every processor's scheduling has done.
pub(crate) fn summary() -> Summary {
    let mut summary = Summary::default();
    let Some(queues) = QUEUES.get() else {
        return summary;
    };
    let saved = <arch::Irq as IrqControl>::disable();
    for lock in queues {
        let queue = lock.lock();
        summary.switches += queue.stats.switches;
        summary.steals += queue.stats.stolen_in;
        summary.worst_overrun = summary.worst_overrun.max(queue.stats.worst_overrun);
        summary.worst_lag = summary.worst_lag.max(queue.stats.worst_lag);
        summary.tasks += queue.len();
    }
    <arch::Irq as IrqControl>::restore(saved);
    summary
}

/// Take task `id` out of every processor's sleeper set, and say whether any
/// held it. For a check that has to find out whether something was filed
/// there, and must not leave it there if it was.
pub(crate) fn unfile_sleeper(id: TaskId) -> bool {
    let Some(queues) = QUEUES.get() else {
        return false;
    };
    let saved = <arch::Irq as IrqControl>::disable();
    let mut found = false;
    for lock in queues {
        found |= lock.lock().remove_sleeper(id);
    }
    <arch::Irq as IrqControl>::restore(saved);
    found
}

/// Check every run queue's own bookkeeping.
///
/// # Errors
///
/// The first queue that has broken an invariant, as a sentence.
pub(crate) fn check_invariants() -> Result<(), &'static str> {
    let Some(queues) = QUEUES.get() else {
        return Ok(());
    };
    let saved = <arch::Irq as IrqControl>::disable();
    let mut outcome = Ok(());
    for lock in queues {
        let queue = lock.lock();
        outcome = outcome.and(queue.check_invariants());
    }
    <arch::Irq as IrqControl>::restore(saved);
    if DEAD_STILL_QUEUED.load(Ordering::Relaxed) != 0 {
        outcome = outcome.and(Err("a dead task is still queued"));
    }
    outcome
}

/// Print what each processor's run queue holds, for a check that has failed.
pub(crate) fn report_queues() {
    let Some(queues) = QUEUES.get() else {
        return;
    };
    let saved = <arch::Irq as IrqControl>::disable();
    for (cpu, lock) in queues.iter().enumerate() {
        let queue = lock.lock();
        crate::console::println!(
            "  fair     cpu {} holds {} tasks, idle={}, resched={}",
            cpu,
            queue.len(),
            queue.is_running_idle(),
            NEED_RESCHED
                .get()
                .and_then(|flags| flags.get(cpu))
                .is_some_and(|flag| flag.load(Ordering::Relaxed)),
        );
    }
    <arch::Irq as IrqControl>::restore(saved);
}

/// Print the picks `cpu` remembered from the last window, oldest first.
pub(crate) fn print_picks(cpu: usize) {
    let Some(lock) = queue_of(cpu) else {
        return;
    };
    // Room before the lock, as `cpu_times` makes it; with none, nothing is
    // printed.
    let Ok(mut picks) = fallible::try_with_capacity::<queue::Pick>(queue::TRACE_PICKS) else {
        return;
    };
    let saved = <arch::Irq as IrqControl>::disable();
    for pick in lock.lock().picks() {
        let _ = fallible::push_within(&mut picks, *pick);
    }
    <arch::Irq as IrqControl>::restore(saved);
    for pick in picks {
        crate::console::println!(
            "  pick     cpu {cpu} at {} us: chose #{}, scan says #{}, avg v {}",
            pick.at / 1000,
            pick.picked,
            pick.scanned,
            pick.avg,
        );
        for seen in pick.seen.iter().take(pick.count) {
            crate::console::println!(
                "  pick       #{} v {} deadline {} lag {}",
                seen.id,
                seen.vruntime,
                seen.deadline,
                seen.lag,
            );
        }
    }
}

/// One processor's run queue, for a check or the boot log.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CpuReport {
    /// Its decaying load average, out of `ferrix_sched::LOAD_SCALE`.
    pub(crate) load: u64,
    /// The slice it is currently handing out.
    pub(crate) slice_ns: u64,
    /// Tasks on it, the running one included.
    pub(crate) queued: usize,
    /// The worst lag it measured while the last window was open.
    pub(crate) worst_lag: u64,
    /// The worst overrun it has served.
    pub(crate) worst_overrun: u64,
    /// Every overrun it served while the last window was open, added up.
    pub(crate) overrun_total: u64,
    /// Picks made while the last window was open.
    pub(crate) picks: u64,
    /// Of those, picks a scan of the queue disagreed with.
    pub(crate) wrong_picks: u64,
}

/// Read `cpu`'s queue, bringing its load average up to date first.
pub(crate) fn cpu_report(cpu: usize) -> Option<CpuReport> {
    let lock = queue_of(cpu)?;
    let saved = <arch::Irq as IrqControl>::disable();
    let report = {
        let mut queue = lock.lock();
        queue.account_load(crate::timer::now_nanos());
        CpuReport {
            load: queue.load_average(),
            slice_ns: queue.slice_ns(),
            queued: queue.len(),
            worst_lag: queue.stats.worst_lag,
            worst_overrun: queue.stats.worst_overrun,
            overrun_total: queue.stats.overrun_total,
            picks: queue.stats.picks,
            wrong_picks: queue.stats.wrong_picks,
        }
    };
    <arch::Irq as IrqControl>::restore(saved);
    Some(report)
}

/// How one processor has spent its time since it joined the scheduler.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CpuTime {
    /// Nanoseconds running a task other than the idle task.
    pub(crate) busy_ns: u64,
    /// Nanoseconds running the idle task.
    pub(crate) idle_ns: u64,
    /// Context switches.
    pub(crate) switches: u64,
    /// Tasks runnable on it, the running one included.
    pub(crate) runnable: usize,
}

/// Every processor's [`CpuTime`], by logical number, each read up to the
/// moment its lock was taken and without charging any task for it: a reader
/// of `/proc/stat` changes nothing the scheduler decides with.
///
/// # Errors
///
/// [`fallible::AllocError`] when there is no memory for the list.
pub(crate) fn cpu_times() -> Result<Vec<CpuTime>, fallible::AllocError> {
    let Some(queues) = QUEUES.get() else {
        return Ok(Vec::new());
    };
    // Room for every queue before any lock is taken, so nothing under one
    // allocates.
    let mut times = fallible::try_with_capacity(queues.len())?;
    let saved = <arch::Irq as IrqControl>::disable();
    for lock in queues {
        let queue = lock.lock();
        let (busy_ns, idle_ns) = queue.time_spent(crate::timer::now_nanos());
        let _ = fallible::push_within(
            &mut times,
            CpuTime {
                busy_ns,
                idle_ns,
                switches: queue.stats.switches,
                runnable: queue.len(),
            },
        );
    }
    <arch::Irq as IrqControl>::restore(saved);
    Ok(times)
}

/// Tasks made since boot, the boot task and every processor's idle task
/// among them, as Linux's `total_forks` counts its idle tasks.
pub(crate) fn tasks_made() -> u64 {
    NEXT_ID.load(Ordering::Relaxed).saturating_sub(1)
}
