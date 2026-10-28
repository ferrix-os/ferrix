//! Scheduling domains made at run time: docs/SCHED-CLASSES.md §9f, stage
//! 14's step 5, as its review has it (OK IF V1-V16).
//!
//! The boot domain, slot [`BOOT`], holds every processor until a `SoftRt`
//! domain, slot [`SOFT`], takes some of them out of it, and gives them back
//! when it is dissolved. Each processor has a word naming the slot of its
//! domain ([`DOMAIN_OF`]), read without a lock by placement, stealing, the
//! pull and the balance; each task carries the slot of its own
//! (`Task::domain_slot`). A task runs only on a processor whose slot is its
//! own (D-1, [`admits`]): the per-processor kernel tasks excepted, which a
//! constructor marks [`EXEMPT`] (V7).
//!
//! **When the answer may change.** [`admits`] turns from yes to no for a
//! task and a processor only under that processor's queue lock: a
//! processor's word is written under its lock ([`set_slots`]), and a task's
//! slot under the lock of the queue that owns it ([`settle_task`]). Every
//! placement, steal and pull asks it under the target's lock, so the answer
//! it acts on holds until it lets go; the checks that stop the machine on a
//! foreign task (`move_onto`, the pick, the direct switch) ask it under the
//! same lock, after the placement did. A dissolution changes answers only
//! from no to yes: it clears the `SoftRt` slot's bit in [`LIVE`] first, which
//! makes every word naming it read as the boot domain's at once, processors
//! and tasks together, and puts the words right after.
//!
//! **Moving a task.** A task found outside its domain is never moved from
//! one queue to another under two locks by the switch. It is taken off its
//! queue where it is (at its processor's decision for a leaving processor,
//! by the admission for a queued member, at the switch away from it for a
//! running one, V4) and filed in [`MOVING`], from which [`drain`] places it
//! on a processor of its domain once no queue lock is held.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};

use ferrix_sched_class::{CpuSet, MAX_CPUS, Mode};
use ferrix_sync::IrqSpinLock;

use crate::arch;
use crate::object::job::Job;

use super::task::Task;

/// How many domains there may be at once: the boot domain and one `SoftRt`
/// domain (V8).
pub(crate) const MAX_DOMAINS: u8 = 2;
/// The boot `Throughput` domain's slot.
pub(crate) const BOOT: u8 = 0;
/// The `SoftRt` domain's slot.
pub(crate) const SOFT: u8 = 1;
/// A processor leaving the boot domain for a `SoftRt` one: in no domain,
/// so that nothing but its exempt tasks may run or be placed there.
pub(crate) const LEAVING: u8 = 0x80;
/// A per-processor kernel task's mark: in no domain, run wherever its one
/// processor is (V7). Never a processor's word.
pub(crate) const EXEMPT: u8 = 0xFF;

/// The slot of each processor's domain: any word but [`BOOT`] and [`SOFT`]
/// is no domain, for every task but the exempt (V2, V8).
static DOMAIN_OF: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(BOOT) }; MAX_CPUS];

/// The slots whose domain exists, a bit each: the boot domain's always.
static LIVE: AtomicU8 = AtomicU8::new(1 << BOOT);

/// The `SoftRt` domain's generation, counted up as each is dissolved: a job
/// admitted to one keeps the generation in its word ([`word`]), so that a
/// job admitted to a domain since dissolved is in the boot domain again
/// without anything walking the jobs (V10).
static GENERATION: AtomicU32 = AtomicU32::new(1);

/// Whether a `SoftRt` domain exists or is being made or dissolved: one at a
/// time (D-3, V8).
static CLAIMED: AtomicBool = AtomicBool::new(false);

/// How many processors the `SoftRt` domain has, zero for none: what the
/// boot domain's count is reckoned from (`count`).
static SOFT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Taken for every change of a task's slot or a processor's, and by
/// [`drain`]: so that the slot of a task being placed from [`MOVING`]
/// cannot change between the placement's question and its insert. Before
/// any queue lock.
static SWITCH: IrqSpinLock<(), arch::Irq> = IrqSpinLock::new(());

/// Tasks taken off a queue for running outside their domain, waiting for
/// [`drain`] to place them in it: runnable, on no queue, each filed in its
/// own run slot. Taken after any queue lock, never before one.
static MOVING: IrqSpinLock<Moving, arch::Irq> = IrqSpinLock::new(Moving::new());

/// How many tasks [`MOVING`] holds, read without its lock by [`drain`]'s
/// callers on every switch.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Tasks [`push_moving`] could not file, for want of their run slot: zero
/// on a correct kernel, which a check requires.
static MOVING_LOST: AtomicUsize = AtomicUsize::new(0);

/// The tasks in [`MOVING`], oldest first.
struct Moving {
    /// The tasks, keyed by the order they were filed in.
    list: ferrix_sched_fair::Timeline<Arc<Task>>,
    /// The next one's place in that order.
    next: u64,
}

impl Moving {
    /// None.
    const fn new() -> Moving {
        Moving {
            list: ferrix_sched_fair::Timeline::new(),
            next: 0,
        }
    }

    /// File `task` in its run slot, which a task on no queue holds.
    fn push(&mut self, task: Arc<Task>) {
        let Some(slot) = task.take_run_slot() else {
            let _ = MOVING_LOST.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let at = self.next;
        self.next = self.next.wrapping_add(1);
        // NOALLOC: a `Timeline` files the task in the slot it is given.
        self.list.insert(at, task.id, task, slot);
        let _ = PENDING.fetch_add(1, Ordering::Release);
    }

    /// Take the oldest out.
    fn pop(&mut self) -> Option<Arc<Task>> {
        let (_, task, slot) = self.list.pop_due(u64::MAX)?;
        task.return_run_slot(slot);
        let _ = PENDING.fetch_sub(1, Ordering::Release);
        Some(task)
    }
}

/// The slot of `cpu`'s domain, as its word has it: [`LEAVING`] for a
/// processor past the table.
pub(crate) fn slot_of(cpu: usize) -> u8 {
    DOMAIN_OF
        .get(cpu)
        .map_or(LEAVING, |slot| slot.load(Ordering::Acquire))
}

/// The domain a word names: its own slot while that domain exists, the boot
/// domain's when it was dissolved, and `None` for no domain.
fn effective(slot: u8) -> Option<u8> {
    if slot >= MAX_DOMAINS {
        return None;
    }
    if LIVE.load(Ordering::Acquire) & (1 << slot) != 0 {
        Some(slot)
    } else {
        Some(BOOT)
    }
}

/// Whether `task` may run on `cpu` for its domain (D-1): it is exempt, or
/// the processor's domain is its own. Not its affinity, which the caller
/// asks as well. Fails closed: a processor in no domain admits no task but
/// the exempt (V2).
pub(crate) fn admits(task: &Task, cpu: usize) -> bool {
    if task.is_exempt() {
        return true;
    }
    let here = effective(slot_of(cpu));
    here.is_some() && here == effective(task.domain_slot())
}

/// Whether `cpu` takes work from its neighbours: only in the boot domain,
/// the one `Throughput` domain (D-2, `Mode::steals_work`).
pub(crate) fn steals(cpu: usize) -> bool {
    effective(slot_of(cpu))
        .and_then(mode_of)
        .is_some_and(Mode::steals_work)
}

/// The mode of the domain in `slot`.
fn mode_of(slot: u8) -> Option<Mode> {
    match slot {
        BOOT => Some(Mode::Throughput),
        SOFT => Some(Mode::SoftRt),
        _ => None,
    }
}

/// The processors below `online` in the domain `slot` names now.
pub(crate) fn cpus_of(slot: u8, online: usize) -> CpuSet {
    let mut set = CpuSet::empty();
    let wanted = effective(slot);
    for cpu in (0..online).filter(|cpu| wanted.is_some() && effective(slot_of(*cpu)) == wanted) {
        // NOALLOC: a `CpuSet` is a fixed bitmap.
        let _ = set.insert(cpu);
    }
    set
}

/// How many processors of `online` the domain of `task` has: the pull's
/// factor (`ferrix_sched_class::wake_wide`).
pub(crate) fn count(task: &Task, online: usize) -> usize {
    let soft = SOFT_COUNT.load(Ordering::Acquire);
    match effective(task.domain_slot()) {
        Some(SOFT) => soft,
        _ => online.saturating_sub(soft),
    }
}

/// The word a job admitted to the domain in `slot` keeps (`Job`'s
/// `sched_domain`): the slot, and above it the generation of the `SoftRt`
/// domain it names.
fn word(slot: u8) -> u32 {
    (GENERATION.load(Ordering::Acquire) << 8) | u32::from(slot)
}

/// The slot a job's word names: zero, or a word of a dissolved domain, is
/// the boot domain's.
pub(crate) fn slot_of_word(word: u32) -> u8 {
    let slot = (word & 0xFF) as u8;
    let current = (word >> 8) == GENERATION.load(Ordering::Acquire);
    if word != 0 && current && effective(slot) == Some(slot) {
        slot
    } else {
        BOOT
    }
}

/// File `task`, taken off its queue (or just switched away from) for
/// running outside its domain, to be placed in it by [`drain`]. Under any
/// queue lock or none.
pub(crate) fn push_moving(task: Arc<Task>) {
    // NOALLOC: `Moving::push` files the task in its own run slot.
    MOVING.lock().push(task);
}

/// Whether tasks wait to be placed in their domains.
pub(crate) fn moving() -> usize {
    PENDING.load(Ordering::Acquire)
}

/// Tasks lost by [`push_moving`], for the checks.
pub(crate) fn moving_lost() -> usize {
    MOVING_LOST.load(Ordering::Relaxed)
}

/// Place the tasks [`push_moving`] filed, each on a processor of its domain
/// that it may run on (`sched::place_moving`); one that has none now is
/// filed again for a later call. With no queue lock held. Skipped while a
/// switch holds [`SWITCH`], whose own wait calls it again: called from a
/// switch away from a task (`finish_switch`), it never waits for one.
pub(crate) fn drain() {
    if moving() == 0 {
        return;
    }
    let Some(_switch) = SWITCH.try_lock() else {
        return;
    };
    drain_held();
}

/// [`drain`], under [`SWITCH`]: each task filed now is looked at once.
fn drain_held() {
    for _ in 0..moving() {
        let Some(task) = MOVING.lock().pop() else {
            return;
        };
        if !super::place_moving(&task) {
            // NOALLOC: `Moving::push` files the task in its own run slot.
            MOVING.lock().push(task);
        }
    }
}

/// Write `slot` as the domain of each of `cpus`, each under its queue's
/// lock, and ask each for a decision.
fn set_slots(cpus: &CpuSet, slot: u8) {
    {
        let _switch = SWITCH.lock();
        for cpu in cpus.iter() {
            let Some(lock) = super::queue_of(cpu) else {
                continue;
            };
            let _queue = lock.lock();
            if let Some(word) = DOMAIN_OF.get(cpu) {
                word.store(slot, Ordering::Release);
            }
        }
    }
    for cpu in cpus.iter() {
        super::ask_decision(cpu);
    }
}

/// Put `task` in the domain of `slot`, under the lock of the queue that owns
/// it: a queued task that may no longer run there is taken off and filed
/// for [`drain`]; a running one is asked to decide, and is moved once the
/// switch away from it has saved its context (V4); a blocked one is placed
/// in its domain when it wakes (V11).
fn settle_task(task: &Arc<Task>, slot: u8) {
    let _switch = SWITCH.lock();
    loop {
        let cpu = task.cpu();
        let Some(lock) = super::queue_of(cpu) else {
            return;
        };
        let mut queue = lock.lock();
        // It may have moved between the read and the lock.
        if task.cpu() != cpu {
            continue;
        }
        if !task.set_domain_slot(slot) || admits(task, cpu) {
            return;
        }
        if queue.is_current(task) {
            drop(queue);
            super::ask_decision(cpu);
        } else if task.is_queued()
            && let Some(task) = queue.release(task.id, task.queued_upper())
        {
            push_moving(task);
        }
        return;
    }
}

/// Why a domain was not made, or a job not admitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Refused {
    /// This build has no tier above fair: `SoftRt` is `ModeUnavailable`.
    Unavailable,
    /// Fewer than four processors (D-3, V9).
    TooFew,
    /// No processors asked for, or more than all but two (D-3, V9).
    Count,
    /// A `SoftRt` domain exists already, or one is being made or dissolved
    /// (D-3, V8).
    Exists,
    /// The processors did not empty in time (V6): given back.
    Timeout,
    /// No `SoftRt` domain to admit to.
    NoDomain,
    /// A member may run on none of the domain's processors.
    Unplaceable,
    /// No memory for the walk.
    NoMemory,
}

/// How long a switch waits for its processors to empty (V6).
const PATIENCE_NANOS: u64 = 1_000_000_000;

/// How long it sleeps between looks.
const LOOK_EVERY_NANOS: u64 = 1_000_000;

/// Make a `SoftRt` domain of `count` processors, the highest-numbered of the
/// boot domain's, and answer which (docs/SCHED-CLASSES.md §9f, the switch).
/// From a task that may sleep, holding no lock (V6).
///
/// # Errors
///
/// [`Refused`], with nothing changed: the build has no tier above fair, the
/// machine has fewer than four processors, `count` is zero or more than all
/// but two, a domain exists, or the processors did not empty in time.
pub(crate) fn create_soft(count: usize) -> Result<CpuSet, Refused> {
    let online = super::online();
    if !ferrix_kernel_sched_select::mode_available(Mode::SoftRt) {
        return Err(Refused::Unavailable);
    }
    if online < 4 {
        return Err(Refused::TooFew);
    }
    if count == 0 || count > online - 2 {
        return Err(Refused::Count);
    }
    if CLAIMED.swap(true, Ordering::AcqRel) {
        return Err(Refused::Exists);
    }
    if let Err(refused) = sweep() {
        CLAIMED.store(false, Ordering::Release);
        return Err(refused);
    }
    let mut leaving = CpuSet::empty();
    for cpu in online - count..online {
        // NOALLOC: a `CpuSet` is a fixed bitmap.
        let _ = leaving.insert(cpu);
    }
    // Step 2: nothing of the boot domain is put on them from here.
    set_slots(&leaving, LEAVING);
    // Step 3: each moves what it holds out at its decision.
    if !wait_until_empty(&leaving) {
        set_slots(&leaving, BOOT);
        drain();
        CLAIMED.store(false, Ordering::Release);
        return Err(Refused::Timeout);
    }
    // Step 4: the slot is live before any processor names it, so that no
    // processor reads it as the boot domain's.
    {
        let _switch = SWITCH.lock();
        SOFT_COUNT.store(count, Ordering::Release);
        let _ = LIVE.fetch_or(1 << SOFT, Ordering::AcqRel);
    }
    set_slots(&leaving, SOFT);
    Ok(leaving)
}

/// Wait, bounded, until each of `cpus` runs and holds only tasks that may
/// run there, and every task taken off them is placed (V6).
fn wait_until_empty(cpus: &CpuSet) -> bool {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        drain();
        if moving() == 0 && cpus.iter().all(super::holds_only_admitted) {
            return true;
        }
        if crate::timer::now_nanos() >= deadline {
            return false;
        }
        for cpu in cpus.iter() {
            super::ask_decision(cpu);
        }
        super::sleep_for(LOOK_EVERY_NANOS);
    }
}

/// Dissolve the `SoftRt` domain, if there is one: its processors and its
/// members go back to the boot domain at once (D-4). Cannot fail. From a
/// task that may sleep, holding no lock (V6).
pub(crate) fn dissolve() {
    let online = super::online();
    let cpus = cpus_of(SOFT, online);
    {
        let _switch = SWITCH.lock();
        if LIVE.load(Ordering::Acquire) & (1 << SOFT) == 0 {
            return;
        }
        // Every word naming the domain reads as the boot domain's from
        // here: an answer of `admits` only turns from no to yes.
        let _ = LIVE.fetch_and(!(1 << SOFT), Ordering::AcqRel);
        let _ = GENERATION.fetch_add(1, Ordering::AcqRel);
        SOFT_COUNT.store(0, Ordering::Release);
    }
    set_slots(&cpus, BOOT);
    // Members' slots are put right by the next domain's sweep (`sweep`):
    // until then each reads as the boot domain's.
    drain();
    CLAIMED.store(false, Ordering::Release);
}

/// Whether a `SoftRt` domain exists.
pub(crate) fn soft_exists() -> bool {
    LIVE.load(Ordering::Acquire) & (1 << SOFT) != 0
}

/// Admit `job`, and every job beneath it, to the `SoftRt` domain: its
/// processes' tasks are moved in, each at its processor's next decision
/// (docs/SCHED-CLASSES.md §9f, admission). Answers how many tasks were
/// settled.
///
/// # Errors
///
/// [`Refused::NoDomain`] with no domain, [`Refused::Unplaceable`] if a
/// member may run on none of its processors, or [`Refused::NoMemory`].
pub(crate) fn admit(job: &Job) -> Result<usize, Refused> {
    if !soft_exists() {
        return Err(Refused::NoDomain);
    }
    let members = members_of(Some(job))?;
    placeable(&members)?;
    job.set_sched_domain(word(SOFT));
    admit_tasks(&members)
}

/// Refuse a member that may run on none of the `SoftRt` domain's
/// processors: it would wait to be placed for as long as the domain lives.
fn placeable(tasks: &[Arc<Task>]) -> Result<(), Refused> {
    let cpus = cpus_of(SOFT, super::online());
    let stranded = |task: &&Arc<Task>| !cpus.iter().any(|cpu| task.may_run_on(cpu));
    match tasks.iter().find(stranded) {
        Some(_) => Err(Refused::Unplaceable),
        None => Ok(()),
    }
}

/// Move `tasks` into the `SoftRt` domain, each at its processor's next
/// decision: [`admit`]'s second half, and the boot check's admission of
/// kernel tasks, which belong to no job.
///
/// # Errors
///
/// As [`admit`]'s, but for memory.
pub(crate) fn admit_tasks(tasks: &[Arc<Task>]) -> Result<usize, Refused> {
    if !soft_exists() {
        return Err(Refused::NoDomain);
    }
    placeable(tasks)?;
    for task in tasks {
        settle_task(task, SOFT);
    }
    drain();
    Ok(tasks.len())
}

/// Every task of every live process in `job` or beneath it, or of every
/// live process with `None`.
fn members_of(job: Option<&Job>) -> Result<Vec<Arc<Task>>, Refused> {
    let hosts = crate::object::process::live().map_err(|_| Refused::NoMemory)?;
    let mut tasks = Vec::new();
    let inside = |host: &&Arc<dyn crate::object::process::Host>| {
        job.is_none_or(|job| job.contains(&host.core().job()))
    };
    for host in hosts.iter().filter(inside) {
        host.core()
            .with_tasks(|listed| {
                // Room first, so that no task upgraded under the list's lock is
                // let go there (`Process::with_tasks`).
                crate::fallible::try_reserve(&mut tasks, listed.len())?;
                for task in listed.iter().filter_map(Weak::upgrade) {
                    // Within the room just made: never refused.
                    let _ = crate::fallible::push_within(&mut tasks, task);
                }
                Ok::<(), crate::fallible::AllocError>(())
            })
            .map_err(|_| Refused::NoMemory)?;
    }
    Ok(tasks)
}

/// Put every task still marked with the `SoftRt` slot of a dissolved domain
/// back in the boot domain's, before another is made: each reads as the
/// boot domain's already, so nothing it may run on changes.
fn sweep() -> Result<(), Refused> {
    for task in members_of(None)? {
        if task.domain_slot() == SOFT {
            let _ = task.set_domain_slot(BOOT);
        }
    }
    Ok(())
}
