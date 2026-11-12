//! The scoped OOM kill: what a program's page fault does when its cgroup's
//! `memory.max` is full (`docs/CGROUPS.md` §6 and §7.1, stage 13's M1).
//!
//! A charge past a job's memory limit is refused (`object::quota`). Inside
//! a system call made for an object -- a descriptor, a pipe's buffer, a
//! region -- that stays `ENOMEM`, as Linux's `__GFP_ACCOUNT` allocations
//! fail. A page of the program's own memory is different: a fault has no
//! error to return, and Linux, when reclaim cannot help (Ferrix has none
//! yet, M2), kills inside the cgroup that is full. So does this:
//!
//! * `memory.events`' `oom` counts, in the job whose limit refused and in
//!   every job above it, as Linux's hierarchical `memory.events` does;
//! * the victim is the process in that job or beneath it with the most
//!   resident memory -- never one outside it, never pid 1, and never a
//!   kernel task, which is no process -- ended as `SIGKILL` does
//!   ([`KILLED_STATUS`]), and `oom_kill` counts in its job and above;
//! * every job counted wakes whatever polls its `memory.events`, which
//!   reports `POLLPRI` until it is read again from its start;
//! * and the fault is tried again: at once when the faulting process was
//!   the victim, which then leaves on its way back to user mode, and once
//!   the victim's memory is given back otherwise.
//!
//! A killed process gives its memory back when its last reference goes,
//! which for a zombie is its parent's `wait4`; Linux gives it back at exit.
//! So a victim that has let go of everything else, and whose address space
//! no live process shares, has that space handed to the emptier, a task of
//! its own, as Linux's OOM reaper is a kernel thread (`mm/oom_kill.c`,
//! `oom_reaper`): never emptied where the kill was asked, which for the
//! kernel's heap is under an allocation's stack (F-107, `docs/OOM-REAPER.md`).
//! A fault that finds a victim of its job still ending, or handed and not
//! yet emptied, waits for it a moment and tries again, as Linux's selection
//! aborts while a victim is still exiting; one whose emptying makes no
//! progress for [`EMPTY_PATIENCE_NANOS`] is given up and passed over, as
//! Linux's `MMF_OOM_SKIP`.
//!
//! With nothing killable in the job, the fault fails as it did before: a
//! `SIGSEGV` for the program's own, `EFAULT` for a page a system call touched
//! for it.
//!
//! # The machine out of frames
//!
//! With no limit full, the machine itself is out: the free count is at the
//! floor programs may not take (`mm::watermark`). Clean file cache anywhere
//! is room, and a fault takes some back and tries again -- but not without
//! end (`p1-002`): two programs at the floor could each take back what the
//! other's retry was about to use. So the retries one fault makes are
//! counted, and once [`MACHINE_RECLAIMS`] reclaims have not let it in, or a
//! reclaim gave back nothing at all, the progress test has failed and the
//! machine-wide kill runs, as Linux's global OOM killer does
//! (`mm/oom_kill.c`, `out_of_memory` with no memcg): the live process with
//! the most resident memory anywhere, never pid 1 and never a kernel task,
//! ended as `SIGKILL` does, its job's `oom_kill` counted, and the fault
//! tried again. A victim still ending is waited for, as in a job, and no
//! second machine-wide kill is made within [`KILL_GRACE_NANOS`] of one: a
//! victim its parent has reaped is off the list before its memory is back,
//! which Linux's OOM reaper covers. A fault
//! whose refusal the allocation check injected (`fallible`) kills nobody: the
//! machine is not out. And every fault's answers are bounded in all
//! ([`MOST_ANSWERS`]): one that still finds no room fails as above.
//!
//! `memory.oom.group` and `oom_score_adj` are not built: a victim is one
//! process, chosen by its resident pages alone.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;

use super::job::{Job, KILLED_STATUS};
use super::process::{self, Host};
use super::quota::{self, Counter, Resource};
use crate::console::println;
use crate::user::cache;
use crate::user::space::{Access, AddressSpace, SpaceError};
use crate::user::vmo::VmoError;

/// What a fault the scoped OOM kill answered does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Fault again: memory was given back, or is being.
    Retry,
    /// The faulting process was chosen, or is already ending: let it go.
    Victim,
    /// Nothing to kill, or no limit full: fail the fault as before.
    Refused,
}

/// How long a fault waits before trying again while another OOM kill runs,
/// or a victim of its job is still ending.
const PAUSE_NANOS: u64 = 1_000_000;

/// Pages one reclaim is asked for when a fault finds its job's limit full:
/// a fill's worth, so that the next faults find room without asking again.
const RECLAIM_PAGES: u64 = 32;

/// Reclaims one fault makes with the machine out of frames before the
/// progress test fails and the machine-wide kill runs: Linux's
/// `MAX_RECLAIM_RETRIES`.
pub const MACHINE_RECLAIMS: u32 = 16;

/// Answers one fault takes in all, every retry and every wait for an ending
/// victim counted, before it fails as it would without the OOM kill: about a
/// second of [`PAUSE_NANOS`] waits, so that a victim that never ends cannot
/// hold a fault for ever.
// CHECKS-ONLY: object/quota_check.rs, besides `out_of_memory_at` here.
pub const MOST_ANSWERS: u32 = 1024;

/// Set while one fault chooses and kills, as Linux's `oom_lock` is held:
/// two faults at one full limit kill one process, not two. A flag, not a
/// lock, since a kill wakes tasks and must hold no spin lock
/// (`crate::sync::SpinLock`), and a fault that finds it set waits and
/// tries again.
static CHOOSING: AtomicBool = AtomicBool::new(false);

/// The task choosing and killing for the kernel's heap under [`CHOOSING`]
/// ([`for_kernel_heap`]), zero for none: its allocations the heap refuses
/// are served from its processor's allocation reserve
/// ([`killing_for_the_heap`]).
static HEAP_KILLER: AtomicU64 = AtomicU64::new(0);

/// Whether the running task, outside interrupt context, is the one
/// choosing and killing for the kernel's heap (L.mm.139).
/// An interrupt taken while the killer runs is not the killer: its
/// handler's refusals stand as before. The kill is called because the heap is empty
/// -- no frame free, no clean cache to reclaim -- and the list of the live
/// processes it chooses from, and what the kill itself allocates, would
/// find it empty too: the kill then answered refused and the allocation it
/// was asked for failed (test-btrfs-pressure's hog boot, 2026-11-08, "no
/// list of the live processes" with 0 frames free). So the allocator serves
/// this task's refused allocations from the processor's reserve
/// (`mm::reserve::draw`), which the victim's freed memory fills again at
/// the next section. Asked only on an allocation the heap refused.
///
/// The emptier ([`emptier`]) is served the same way: it empties what a kill
/// handed it with the machine as short as the kill found it.
pub(crate) fn killing_for_the_heap() -> bool {
    let killer = HEAP_KILLER.load(Ordering::Acquire);
    let emptier = EMPTIER.load(Ordering::Acquire);
    (killer != 0 || emptier != 0)
        && !crate::irq::in_interrupt()
        && crate::sched::current_id().is_some_and(|me| me == killer || me == emptier)
}

// CHECKS-ONLY: mm/check.rs
/// Name `task` as the OOM kill's emptier, as [`emptier`] names itself, for
/// the reserve's check alone (L.mm.139): answers who was named before, for
/// the check to put back.
pub(crate) fn name_emptier_for_check(task: u64) -> u64 {
    EMPTIER.swap(task, Ordering::AcqRel)
}

// CHECKS-ONLY: mm/check.rs
/// Name `task` as the heap's killer, or none with 0, as
/// [`for_kernel_heap`] does under [`CHOOSING`]: for the reserve's check
/// alone (L.mm.139), which runs before any process can be killed.
pub(crate) fn name_heap_killer_for_check(task: u64) {
    HEAP_KILLER.store(task, Ordering::Release);
}

/// When the last machine-wide kill was made, in `timer::now_nanos`; zero
/// for none.
static LAST_MACHINE_KILL: AtomicU64 = AtomicU64::new(0);

/// How long after a machine-wide kill no other is made: time for the
/// victim's memory to come back after its parent's `wait4` took it off the
/// list of the living, as Linux's OOM reaper is given before it gives up.
const KILL_GRACE_NANOS: u64 = 250_000_000;

/// Whether a machine-wide kill was made less than [`KILL_GRACE_NANOS`] ago.
fn in_grace() -> bool {
    let last = LAST_MACHINE_KILL.load(Ordering::Relaxed);
    last != 0 && crate::timer::now_nanos().saturating_sub(last) < KILL_GRACE_NANOS
}

// CHECKS-ONLY: object/quota_check.rs
/// Make the last machine-wide kill look older than the grace, so that the
/// quota check proves a kill after it without sleeping [`KILL_GRACE_NANOS`]
/// in every boot.
#[doc(hidden)]
pub fn forget_last_machine_kill_for_check() {
    LAST_MACHINE_KILL.store(0, Ordering::Relaxed);
}

/// Processes the scoped OOM kill has ended since boot, for the check.
static KILLS: AtomicU64 = AtomicU64::new(0);

/// How many processes the scoped OOM kill has ended since boot.
pub fn kills() -> u64 {
    KILLS.load(Ordering::Relaxed)
}

/// Whether a fault failed the way a refused charge fails it: no frame for
/// the page, its copy, or a table.
pub fn is_charge_refusal(error: &SpaceError) -> bool {
    matches!(
        error,
        SpaceError::OutOfMemory | SpaceError::Backing(VmoError::OutOfMemory)
    )
}

/// [`AddressSpace::fault`] for a program's own fault, from the trap path:
/// a page past its cgroup's `memory.max` kills inside the cgroup
/// ([`out_of_memory`]), and the instruction is retried -- by the victim too,
/// which leaves on its way back to user mode instead of running it.
///
/// # Errors
///
/// As [`AddressSpace::fault`], and the refusal itself when nothing could be
/// killed.
pub fn user_fault(space: &AddressSpace, address: u64, access: Access) -> Result<(), SpaceError> {
    // The faulting task's own memory, adopted ticket or not
    // (`docs/microkernel/K7.md` §2.3).
    quota::charging_own(|| user_fault_own(space, address, access))
}

/// [`user_fault`], charging the task's own job.
fn user_fault_own(space: &AddressSpace, address: u64, access: Access) -> Result<(), SpaceError> {
    quota::count(crate::sched::running_group(), Counter::Faults, 1);
    // Retried here rather than by running the instruction again, so that the
    // answers one fault takes are counted ([`out_of_memory_at`]).
    let mut attempt = 0_u32;
    loop {
        // A program's own fault: never refused for a pager-backed object's
        // `PAGER_VMO_NO_COPY_WAIT`, which is about copies (K3.md §2.4).
        match space.fault_with(address, access, crate::object::pager::FaultMode::Program) {
            Err(error) if is_charge_refusal(&error) => match out_of_memory_at(space, attempt) {
                Answer::Refused => return Err(error),
                Answer::Victim => return Ok(()),
                Answer::Retry => attempt = attempt.saturating_add(1),
            },
            Ok(()) => {
                throttle();
                return Ok(());
            }
            done => return done,
        }
    }
}

/// [`AddressSpace::fault`] for a page a system call touches on the running
/// program's behalf, which Linux faults as the program's own: a charge past
/// a job's `memory.max` there kills inside the job too ([`out_of_memory`]),
/// and the fault is tried again after another process was killed.
///
/// # Errors
///
/// As [`AddressSpace::fault`], and the refusal itself when the running
/// process was the victim or nothing could be killed.
pub fn fault(space: &AddressSpace, address: u64, access: Access) -> Result<(), SpaceError> {
    // The faulting task's own memory, adopted ticket or not: a user copy a
    // native call makes from or to the caller's own buffers is the caller's
    // (`docs/microkernel/K7.md` §2.3).
    quota::charging_own(|| fault_own(space, address, access))
}

/// [`fault`], charging the task's own job.
fn fault_own(space: &AddressSpace, address: u64, access: Access) -> Result<(), SpaceError> {
    quota::count(crate::sched::running_group(), Counter::Faults, 1);
    let mut attempt = 0_u32;
    loop {
        match space.fault(address, access) {
            Err(error) if is_charge_refusal(&error) => {
                if out_of_memory_at(space, attempt) != Answer::Retry {
                    return Err(error);
                }
                attempt = attempt.saturating_add(1);
            }
            Ok(()) => {
                throttle();
                return Ok(());
            }
            done => return done,
        }
    }
}

/// A program that just took a page: if its job, or one above, is over its
/// `memory.high`, reclaim from that job, and, if that gave back less than the
/// excess, slow the program down for a moment. Linux's
/// `mem_cgroup_handle_over_high`, at the same place -- before the program
/// goes on -- and with a fixed pause where Linux's grows with the excess.
///
/// `memory.events`' `high` counts in the job whose mark it was and above, and
/// wakes whatever polls their `memory.events`. Holds no lock, and must be
/// called with none held, as every fault is.
pub fn throttle() {
    let Some((over, excess)) = quota::over_high(crate::sched::running_group()) else {
        return;
    };
    quota::count(over, Counter::High, 1);
    let pages = excess.div_ceil(PAGE_SIZE);
    let done = cache::reclaim(over, pages);
    if let Ok(live) = process::live()
        && let Some(job) = live
            .iter()
            .find_map(|host| at_or_above(host.core().job(), over))
    {
        job.wake_memory_events();
    }
    if done.bytes() < excess {
        crate::sched::sleep_for(PAUSE_NANOS);
    }
}

/// A fault in `space` by the running task failed for want of memory, its
/// first answer: [`out_of_memory_at`] with nothing tried yet.
pub fn out_of_memory(space: &AddressSpace) -> Answer {
    out_of_memory_at(space, 0)
}

// CHECKS-ONLY: object/quota_check.rs, besides `out_of_memory`, `user_fault` and `fault` here.
/// A fault in `space` by the running task failed for want of memory, after
/// `attempt` answers to the same fault: find the job whose limit is full, and
/// kill in it; or, with no limit full, reclaim anywhere and, once the
/// progress test fails, kill anywhere. See the module.
pub fn out_of_memory_at(space: &AddressSpace, attempt: u32) -> Answer {
    if attempt >= MOST_ANSWERS {
        return Answer::Refused;
    }
    let group = crate::sched::running_group();
    let Some(full) = quota::at_limit(group, Resource::Memory, PAGE_SIZE) else {
        return machine_out_of_memory(space, attempt);
    };
    // Reclaim inside the job before any kill: its own clean pages, and those
    // of the jobs beneath it, never a sibling's (`user::cache`).
    if cache::reclaim(full, RECLAIM_PAGES).gave() {
        return Answer::Retry;
    }
    if CHOOSING.swap(true, Ordering::AcqRel) {
        crate::sched::sleep_for(PAUSE_NANOS);
        return Answer::Retry;
    }
    let (answer, pause) = kill_within(space, full);
    CHOOSING.store(false, Ordering::Release);
    if pause {
        crate::sched::sleep_for(PAUSE_NANOS);
    }
    answer
}

/// Pages one reclaim with the machine out of frames asks for: a fill's
/// worth, and what the free count lacks of the floor besides, so that the
/// retry finds a frame unless another task took it first (as
/// `fs::pages::reclaim_want` asks for a fill).
fn machine_want() -> u64 {
    let short = crate::mm::watermark()
        .saturating_add(1)
        .saturating_sub(crate::mm::free_frames());
    RECLAIM_PAGES.saturating_add(short)
}

/// The most pages [`reclaim_for`] asks one reclaim for: 64 MiB, so that one
/// round's cost stays bounded however large the charge it makes room for.
const RECLAIM_FOR_MOST: u64 = 16384;

/// Make room for a charge of `pages` frames that was refused, as a fault's
/// refusal is answered before any kill: clean cache charged to the job whose
/// memory limit is full or beneath it, or, with no limit full, anywhere and
/// what the free count lacks of the floor besides. At most
/// [`RECLAIM_FOR_MOST`] pages and the shortfall a round. Whether it gave any
/// back: the caller tries again if it did. For a charge a system call makes
/// for an object, such as `vmo_pin`'s, which answers `ENOMEM` rather than
/// kill.
///
/// Must be called with no spin lock held, as [`cache::reclaim`] must.
pub fn reclaim_for(pages: u64) -> bool {
    let run = pages.clamp(1, RECLAIM_FOR_MOST);
    let group = crate::sched::running_group();
    match quota::at_limit(group, Resource::Memory, PAGE_SIZE) {
        Some(full) => reclaim_within(full, run),
        // A purge first, where frames a large free keeps would be room
        // (`mm::large`, R6F.7).
        None if crate::mm::large::purge_held() => true,
        None => {
            let short = crate::mm::watermark()
                .saturating_add(1)
                .saturating_sub(crate::mm::free_frames());
            cache::reclaim(quota::NONE, run.saturating_add(short)).gave()
        }
    }
}

/// Give back up to `pages` clean cache pages, at most [`RECLAIM_FOR_MOST`],
/// charged to `full` -- the job whose memory limit refused a charge -- or
/// beneath it, never a sibling's: whether any came back. [`reclaim_for`]'s
/// scoped half, and what a kernel-heap charge refused at the limit asks for
/// (`quota::KernelHeap`, fl-096), which has no machine-wide half.
///
/// Must be called with no spin lock held, as [`cache::reclaim`] must.
pub(crate) fn reclaim_within(full: u32, pages: u64) -> bool {
    cache::reclaim(full, pages.clamp(1, RECLAIM_FOR_MOST)).gave()
}

/// No limit full: the machine out of frames. Clean cache anywhere is room,
/// and so is heap the dentry cache gives back, while a reclaim gives some and
/// fewer than [`MACHINE_RECLAIMS`] have been tried for this fault; then the
/// machine-wide kill ([`machine_answer`]).
fn machine_out_of_memory(space: &AddressSpace, attempt: u32) -> Answer {
    machine_out_of_memory_among(space, attempt, process::live)
}

// CHECKS-ONLY: object/quota_check.rs, besides `machine_out_of_memory` here.
/// [`machine_out_of_memory`] with the processes a kill may choose from
/// given by `live`, asked only once the progress test has failed: the
/// machine's living in production, a check's own processes in the quota
/// check, which drives the progress test through it.
pub fn machine_out_of_memory_among<E>(
    space: &AddressSpace,
    attempt: u32,
    live: impl FnOnce() -> Result<Vec<Arc<dyn Host>>, E>,
) -> Answer {
    let reclaimed =
        attempt < MACHINE_RECLAIMS && cache::reclaim(quota::NONE, machine_want()).gave();
    // Frames a large free keeps for its shootdown are room a purge gives
    // back, before anyone is killed for them (`mm::large`, R6F.7).
    if reclaimed || crate::mm::large::purge_held() {
        return Answer::Retry;
    }
    let Ok(live) = live() else {
        return Answer::Refused;
    };
    machine_answer(space, &live)
}

// CHECKS-ONLY: object/quota_check.rs, besides `machine_out_of_memory_among` here.
/// [`machine_out_of_memory`]'s decision once the progress test has failed,
/// over the processes `live`: refuse a fault whose refusal the allocation
/// check injected, and otherwise kill the largest anywhere ([`kill_in`],
/// [`Scope::Machine`]). Apart from the reclaim so that a check can drive it
/// over processes of its own.
pub fn machine_answer(space: &AddressSpace, live: &[Arc<dyn Host>]) -> Answer {
    if crate::fallible::injecting_here() {
        return Answer::Refused;
    }
    if CHOOSING.swap(true, Ordering::AcqRel) {
        crate::sched::sleep_for(PAUSE_NANOS);
        return Answer::Retry;
    }
    let (answer, pause) = kill_in(Some(space), Scope::Machine, live);
    CHOOSING.store(false, Ordering::Release);
    if pause {
        crate::sched::sleep_for(PAUSE_NANOS);
    }
    answer
}

/// Answers one kernel-heap allocation takes from [`for_kernel_heap`] in
/// all before its refusal stands: about a second of [`PAUSE_NANOS`] waits,
/// as a fault's [`MOST_ANSWERS`] are.
pub(crate) const KERNEL_HEAP_ANSWERS: u32 = MOST_ANSWERS;

/// The machine out of memory for an allocation of the kernel's own heap
/// that may block, whose reclaims gave nothing back or did not let it in
/// (L.mm.138): no clean cache is left, and only a program's memory can
/// make room. So the machine-wide kill runs as a fault's would once its
/// progress test failed (`machine_answer`): the live process with the most
/// resident memory, never pid 1 and never a kernel task, ended as
/// `SIGKILL` does, with the same grace after a kill and the same wait for
/// a victim still ending. Linux's slow path does the same for an
/// allocation of its own that may not fail (`__alloc_pages_may_oom`).
///
/// [`Answer::Retry`]: try the heap again; [`Answer::Victim`]: the running
/// process was chosen, or is ending, and the allocation fails so that it
/// leaves; [`Answer::Refused`]: nothing to kill, the failure injection's
/// refusal, or no memory to list the processes. Called with no spin lock
/// held and marked inside a reclaim, so that what it allocates fails
/// rather than reclaims again (`mm::heap_or_kill`).
///
/// Every caller must return first unless `mm::heap_may_reclaim()` holds and
/// run it inside `mm::while_killing`, as `mm::heap_or_kill` and
/// `mm::reservation_grow` do: that is the bound `check-stack-depth`'s
/// recursion row argues for the kill allocating (ledger 9286 K7).
pub(crate) fn for_kernel_heap() -> Answer {
    if crate::fallible::injecting_here() {
        return Answer::Refused;
    }
    if CHOOSING.swap(true, Ordering::AcqRel) {
        crate::sched::sleep_for(PAUSE_NANOS);
        return Answer::Retry;
    }
    HEAP_KILLER.store(crate::sched::current_id().unwrap_or(0), Ordering::Release);
    let space = crate::sched::with_current(|task| task.address_space().cloned()).flatten();
    let (answer, pause) = match process::live() {
        Ok(live) => kill_in(space.as_deref(), Scope::Kernel, &live),
        Err(_) => (Answer::Refused, false),
    };
    HEAP_KILLER.store(0, Ordering::Release);
    CHOOSING.store(false, Ordering::Release);
    if pause {
        crate::sched::sleep_for(PAUSE_NANOS);
    }
    answer
}

/// The space of the last process [`for_kernel_heap`] killed, and whether the
/// kill is done waiting for it -- the emptier emptied it, or its emptying
/// was given up: no other is killed for the kernel's heap until then, or
/// until it is gone. A leaf lock, never taken with [`EMPTYING`]'s.
static KERNEL_VICTIM: crate::sync::SpinLock<Option<(Weak<AddressSpace>, bool)>> =
    crate::sync::SpinLock::new(None);

/// Whether the space of the last process [`for_kernel_heap`] killed is
/// still held by anyone and the kill still waits for it: not emptied, not
/// given up (L.mm.138). Reads counts and a flag; upgrades nothing, so no
/// last reference is ever dropped here.
fn kernel_victim_held() -> bool {
    KERNEL_VICTIM
        .lock()
        .as_ref()
        .is_some_and(|(space, done)| !done && space.strong_count() > 0)
}

/// The kill is done waiting for the space at `space`: emptied or given up.
fn victim_done(space: *const AddressSpace) {
    if let Some((victim, done)) = KERNEL_VICTIM.lock().as_mut()
        && Weak::as_ptr(victim) == space
    {
        *done = true;
    }
}

/// Where an OOM kill chooses its victim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    /// The job whose memory quota slot is this, and the jobs beneath it.
    Job(u32),
    /// Every process: the machine is out of frames.
    Machine,
    /// Every process, as [`Scope::Machine`], for the kernel's own heap
    /// (L.mm.138): no clean cache was left to reclaim for it.
    Kernel,
}

/// The job at or above `job` whose quota slot is `full`, if there is one.
fn at_or_above(job: Arc<Job>, full: u32) -> Option<Arc<Job>> {
    let mut at = Some(job);
    while let Some(current) = at {
        if current.quota_index() == full {
            return Some(current);
        }
        at = current.parent().cloned();
    }
    None
}

/// Whether a process not yet ended runs in `space`.
fn shared_with_the_living(space: &Arc<AddressSpace>, live: &[Arc<dyn Host>]) -> bool {
    live.iter()
        .any(|host| !host.core().is_terminated() && Arc::ptr_eq(host.core().own_space(), space))
}

/// Choose and kill in the job whose memory quota slot is `full`, for a
/// fault in `space`, and whether to pause before the fault is tried again:
/// a victim is still ending. Under [`CHOOSING`].
fn kill_within(space: &AddressSpace, full: u32) -> (Answer, bool) {
    let Ok(live) = process::live() else {
        return (Answer::Refused, false);
    };
    kill_in(Some(space), Scope::Job(full), &live)
}

/// Choose among `live` in `scope_of` and kill, for a fault in `space` --
/// or, with `None`, for a kernel task's own allocation, which no process
/// makes ([`for_kernel_heap`]) -- and whether to pause before the fault is
/// tried again: a victim is still ending. Under [`CHOOSING`].
fn kill_in(
    space: Option<&AddressSpace>,
    scope_of: Scope,
    live: &[Arc<dyn Host>],
) -> (Answer, bool) {
    let looked = match survey(space, scope_of, live) {
        Ok(looked) => looked,
        Err(answer) => return answer,
    };
    let Some(scope) = looked.scope else {
        return (Answer::Refused, false);
    };
    if looked.ending {
        return (Answer::Retry, true);
    }
    // A victim reaped by its parent leaves the list before its memory is
    // back: a fault in that gap found nothing ending and killed again (a
    // driver, beside a 400 MiB program, in a 512 MiB guest). So no second
    // machine-wide kill within [`KILL_GRACE_NANOS`] of the last.
    if matches!(scope_of, Scope::Machine | Scope::Kernel) && in_grace() {
        return (Answer::Retry, true);
    }
    // The last victim chosen for the kernel's heap still holds its space:
    // its memory is not back yet, and another kill would take a second
    // process for the room the first will give -- one 0.8 s after the first
    // took the disk's driver (2026-11-08). The survey above has handed it
    // to the emptier if it has ended; wait for it to be emptied or given up.
    if scope_of == Scope::Kernel && kernel_victim_held() {
        return (Answer::Retry, true);
    }
    // Counted once a kill is asked for, not at each look while a victim
    // ends: a poller is told of each OOM, not of each retry. A machine out
    // of frames is no job's OOM: only the victim's job counts, its kill, as
    // Linux's global OOM counts `oom_kill` in the victim's memcg alone.
    if matches!(scope_of, Scope::Job(_)) {
        scope.count_oom(false);
    }
    let Some((resident, victim)) = looked.victim else {
        return (Answer::Refused, false);
    };
    kill_victim(scope_of, &scope, victim, resident);
    if space.is_some_and(|space| core::ptr::eq(Arc::as_ptr(victim.core().own_space()), space)) {
        (Answer::Victim, false)
    } else {
        (Answer::Retry, false)
    }
}

/// What [`survey`] found in a scope.
struct Survey<'a> {
    /// The first job in scope a process was looked at in, if any was. Under
    /// the machine scope only whether any process was: a machine-wide OOM is
    /// counted in no job.
    scope: Option<Arc<Job>>,
    /// The living process with the most resident pages, never pid 1, and
    /// its count.
    victim: Option<(u64, &'a Arc<dyn Host>)>,
    /// Whether an ended process in scope still holds memory.
    ending: bool,
}

/// Look over `live` in `scope_of` for a fault in `space`: the victim to
/// choose, or the answer at once when the faulting process itself has
/// already ended or an ended one's space was handed to the emptier here.
fn survey<'a>(
    space: Option<&AddressSpace>,
    scope_of: Scope,
    live: &'a [Arc<dyn Host>],
) -> Result<Survey<'a>, (Answer, bool)> {
    let ours = |host: &Arc<dyn Host>| {
        space.is_some_and(|space| core::ptr::eq(Arc::as_ptr(host.core().own_space()), space))
    };
    // Whether a process not yet ended faults here: one that shares the
    // space with an ended one (`vfork`) is not that one.
    let living = live
        .iter()
        .any(|host| ours(host) && !host.core().is_terminated());
    let mut looked = Survey {
        scope: None,
        victim: None,
        ending: false,
    };
    for host in live {
        let core = host.core();
        let within = match scope_of {
            Scope::Job(full) => at_or_above(core.job(), full),
            Scope::Machine | Scope::Kernel => Some(core.job()),
        };
        let Some(within) = within else {
            continue;
        };
        let _ = looked.scope.get_or_insert(within);
        let resident = core.own_space().resident_pages().unwrap_or(0);
        if !core.is_terminated() {
            if core.pid() > 1 && looked.victim.is_none_or(|(most, _)| resident > most) {
                looked.victim = Some((resident, host));
            }
            continue;
        }
        if ours(host) && !living {
            // Its own process, already ending: nothing to wait for.
            return Err((Answer::Victim, false));
        }
        match ended(host, resident, live) {
            Ended::Gone => {}
            Ended::Handed => return Err((Answer::Retry, true)),
            Ended::Ending => looked.ending = true,
        }
    }
    Ok(looked)
}

/// What [`kill_in`] makes of a process that has ended.
enum Ended {
    /// Nothing of its memory to wait for: none resident, shared with the
    /// living, or its emptying given up.
    Gone,
    /// Its space is the emptier's, handed now or before and not yet emptied.
    Handed,
    /// Still ending, its memory still held; or another space is the
    /// emptier's.
    Ending,
}

/// An ended process `host` with `resident` pages, among `live`: hand its
/// space to the emptier if nothing of it runs and nobody else maps what it
/// held, as Linux's OOM reaper is handed a victim, rather than wait for its
/// parent's `wait4` (L.object.103). Never emptied here (L.object.374).
fn ended(host: &Arc<dyn Host>, resident: u64, live: &[Arc<dyn Host>]) -> Ended {
    let core = host.core();
    let space = core.own_space();
    if resident == 0 || space.oom_emptying_given_up() || shared_with_the_living(space, live) {
        return Ended::Gone;
    }
    if !core.exit().is_closed() {
        return Ended::Ending;
    }
    let (answer, gave_up) = hand_over(space);
    let whose = match gave_up {
        GaveUp::Nothing => return answer,
        GaveUp::This => "its",
        GaveUp::Earlier => "an earlier victim's",
    };
    println!(
        "  oom      pid {}: {whose} space was not emptied, no progress in {} ms; the kill moves \
         on",
        core.pid(),
        EMPTY_PATIENCE_NANOS / 1_000_000
    );
    answer
}

/// Whose emptying a hand-off gave up.
enum GaveUp {
    /// Nobody's.
    Nothing,
    /// The space handed now, handed before and stalled.
    This,
    /// The stalled space the slot held, which then took this one.
    Earlier,
}

// CHECKS-ONLY: object/quota_check.rs
/// How long a handed space may go with no chunk emptied, from its hand-off
/// or its last chunk, before the kill gives it up (L.object.375): half of
/// what an allocation or a fault waits in all, so that the one that waited
/// can still see the next kill.
pub const EMPTY_PATIENCE_NANOS: u64 = 500_000_000;

// CHECKS-ONLY: object/quota_check.rs
/// The pages one chunk of an emptying takes, from the top of the highest
/// region (L.object.376): the space's lock is held for this many leaves at
/// most, and `Vmo::take_range`'s list of them, 16 bytes a page, fits the
/// heap's largest size class, which the reserve holds.
pub const EMPTY_CHUNK_PAGES: u64 = 128;

/// The emptier's slot. A leaf lock: nothing is taken under it, never in
/// interrupt context, and what it held is dropped after it. Only `Weak`s:
/// nothing here upgrades one, so no last reference to a space is dropped
/// on a kill's way.
static EMPTYING: crate::sync::SpinLock<Slot> = crate::sync::SpinLock::new(Slot {
    handed: None,
    given_up: None,
});

/// What [`EMPTYING`] holds.
struct Slot {
    /// The space handed to the emptier, until it is gone, emptied or given up.
    handed: Option<Weak<AddressSpace>>,
    /// The last space whose emptying was given up while another took the
    /// slot: marked as such at its own next look ([`ended`]), which holds it.
    given_up: Option<Weak<AddressSpace>>,
}

/// When the space in [`EMPTYING`] was handed, or last had a chunk emptied,
/// in `timer::now_nanos`.
static EMPTY_PROGRESS: AtomicU64 = AtomicU64::new(0);

/// Whether the emptier is asked to look at [`EMPTYING`]: what it waits on,
/// so that its wait takes no lock but its queue's.
static EMPTY_ASKED: AtomicBool = AtomicBool::new(false);

/// Where the emptier waits.
static EMPTIER_WAIT: crate::sched::WaitQueue = crate::sched::WaitQueue::new();

/// The emptier's task id, zero before it starts.
static EMPTIER: AtomicU64 = AtomicU64::new(0);

/// Chunks the emptier has emptied, and of those how many outside a reclaim
/// (none, L.object.376): what the OOM check reads.
static CHUNKS: AtomicU64 = AtomicU64::new(0);
static CHUNKS_OUTSIDE_RECLAIM: AtomicU64 = AtomicU64::new(0);

/// The OOM check's gate on the emptier, read before each chunk: below zero
/// open; zero shut; above zero, that many chunks let through.
// CHECKS-ONLY: object/quota_check.rs
static EMPTY_GATE: AtomicI64 = AtomicI64::new(-1);

/// Hand `space` to the emptier (L.object.103, L.object.374), allocating
/// nothing: a `Weak`, three stores and a wake. Answers what the look makes
/// of it, and whose emptying was given up now: `space` itself, once it made
/// no progress in [`EMPTY_PATIENCE_NANOS`], or the one the slot held, which
/// then lets the slot go to `space`.
#[inline(never)]
fn hand_over(space: &Arc<AddressSpace>) -> (Ended, GaveUp) {
    let now = crate::timer::now_nanos();
    let stalled =
        now.saturating_sub(EMPTY_PROGRESS.load(Ordering::Acquire)) >= EMPTY_PATIENCE_NANOS;
    let me = Arc::as_ptr(space);
    let (answer, gave_up, done, dropped) = {
        let mut slot = EMPTYING.lock();
        let handed = slot.handed.as_ref().map(Weak::as_ptr);
        if slot.given_up.as_ref().map(Weak::as_ptr) == Some(me) {
            // Given up while another took the slot: marked now, said then.
            let _ = space.give_up_oom_emptying();
            (Ended::Gone, GaveUp::Nothing, None, slot.given_up.take())
        } else if handed == Some(me) {
            if stalled {
                let first = !space.give_up_oom_emptying();
                let gave = if first { GaveUp::This } else { GaveUp::Nothing };
                (Ended::Gone, gave, Some(me), slot.handed.take())
            } else {
                (Ended::Handed, GaveUp::Nothing, None, None)
            }
        } else {
            let busy = slot
                .handed
                .as_ref()
                .is_some_and(|other| other.strong_count() > 0);
            // A space given up while another took the slot, and not marked
            // yet, is never let go unmarked: no second displacement until it
            // is (by its own look, or the emptier's next wake), or it is gone.
            let unmarked = slot
                .given_up
                .as_ref()
                .is_some_and(|given| given.strong_count() > 0);
            if busy && (!stalled || unmarked) {
                (Ended::Ending, GaveUp::Nothing, None, None)
            } else {
                let earlier = slot.handed.replace(Arc::downgrade(space));
                EMPTY_PROGRESS.store(now.max(1), Ordering::Release);
                if busy {
                    let earlier_at = earlier.as_ref().map(Weak::as_ptr);
                    let old = core::mem::replace(&mut slot.given_up, earlier);
                    (Ended::Handed, GaveUp::Earlier, earlier_at, old)
                } else {
                    (Ended::Handed, GaveUp::Nothing, None, earlier)
                }
            }
        }
    };
    drop(dropped);
    if let Some(done) = done {
        victim_done(done);
    }
    if matches!(answer, Ended::Handed) {
        EMPTY_ASKED.store(true, Ordering::Release);
        EMPTIER_WAIT.wake_all();
    }
    (answer, gave_up)
}

/// The emptier: a task of its own, as Linux's `oom_reaper` is a kernel
/// thread (L.object.376). Inside a reclaim for its whole life, so that no
/// allocation it makes is answered by a reclaim or a kill -- it never waits
/// on a kill, its own hand-off's included -- and served from its
/// processor's reserve where the heap refuses ([`killing_for_the_heap`]).
pub(crate) fn emptier(_: usize) {
    let _ = crate::sched::with_current(|task| task.swap_reclaiming(true));
    EMPTIER.store(crate::sched::current_id().unwrap_or(0), Ordering::Release);
    loop {
        let _ = EMPTIER_WAIT.wait_trusting(|| EMPTY_ASKED.load(Ordering::Acquire));
        EMPTY_ASKED.store(false, Ordering::Release);
        // A space given up while another took the slot is marked here, where
        // an upgrade's last reference may be dropped, if its own look has not
        // marked it yet (L.object.375).
        let given_up = EMPTYING.lock().given_up.take();
        if let Some(space) = given_up.and_then(|space| space.upgrade()) {
            let _ = space.give_up_oom_emptying();
        }
        // The `Weak` is cloned under the lock and upgraded after it, so that
        // a last reference is never dropped under it.
        let handed = EMPTYING.lock().handed.clone();
        if let Some(space) = handed.and_then(|space| space.upgrade()) {
            empty(&space);
        }
    }
}

/// Empty `space` a chunk at a time from the top: the highest region's top
/// [`EMPTY_CHUNK_PAGES`] pages, so that no chunk splits a region, until no
/// region is left, then let the slot go. Progress is stamped only when a
/// chunk moved the highest region's end down; a chunk that failed or moved
/// nothing ends this emptying, which the patience then gives up. A space
/// given up, or no longer the slot's, is left at the next chunk's boundary.
fn empty(space: &Arc<AddressSpace>) {
    let top = |space: &AddressSpace| {
        space.with_regions(|regions| regions.last().map(|region| (region.start, region.end)))
    };
    loop {
        if space.oom_emptying_given_up() || !in_slot(space) {
            return;
        }
        let Some((start, end)) = top(space) else {
            let_go(space);
            return;
        };
        if !through_gate(space) {
            return;
        }
        let from = start.max(end.saturating_sub(EMPTY_CHUNK_PAGES * PAGE_SIZE));
        let _ = CHUNKS.fetch_add(1, Ordering::Relaxed);
        if crate::sched::with_current(crate::sched::Task::reclaiming) != Some(true) {
            let _ = CHUNKS_OUTSIDE_RECLAIM.fetch_add(1, Ordering::Relaxed);
        }
        if space.unmap(from, end - from).is_err() {
            return;
        }
        match top(space) {
            Some((_, now)) if now >= end => return,
            Some(_) => EMPTY_PROGRESS.store(crate::timer::now_nanos().max(1), Ordering::Release),
            None => {
                let_go(space);
                return;
            }
        }
    }
}

/// Let the slot go, emptied, if it still holds `space`; the `Weak` taken out
/// is dropped after the lock.
fn let_go(space: &Arc<AddressSpace>) {
    let done = {
        let mut slot = EMPTYING.lock();
        if slot.handed.as_ref().map(Weak::as_ptr) == Some(Arc::as_ptr(space)) {
            slot.handed.take()
        } else {
            None
        }
    };
    drop(done);
    victim_done(Arc::as_ptr(space));
}

/// Whether `space` is still the one handed to the emptier.
fn in_slot(space: &Arc<AddressSpace>) -> bool {
    EMPTYING.lock().handed.as_ref().map(Weak::as_ptr) == Some(Arc::as_ptr(space))
}

/// The OOM check's gate ([`EMPTY_GATE`]): whether the next chunk may go,
/// waiting while it is shut and `space` is not given up.
fn through_gate(space: &AddressSpace) -> bool {
    loop {
        let gate = EMPTY_GATE.load(Ordering::Acquire);
        if gate < 0 {
            return true;
        }
        if gate > 0
            && EMPTY_GATE
                .compare_exchange(gate, gate - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return true;
        }
        if space.oom_emptying_given_up() {
            return false;
        }
        crate::sched::sleep_for(PAUSE_NANOS);
    }
}

// CHECKS-ONLY: object/quota_check.rs
/// Set the emptier's gate for the OOM check: below zero open, zero shut,
/// above zero that many chunks. Answers what it was.
#[doc(hidden)]
pub fn gate_emptier_for_check(gate: i64) -> i64 {
    EMPTY_GATE.swap(gate, Ordering::AcqRel)
}

// CHECKS-ONLY: object/quota_check.rs
/// The gate as it stands, for the check to wait on a chunk let through.
#[doc(hidden)]
pub fn emptier_gate_for_check() -> i64 {
    EMPTY_GATE.load(Ordering::Acquire)
}

// CHECKS-ONLY: object/quota_check.rs
/// Whether nothing is handed to the emptier: the last space it held was
/// emptied, given up or gone.
#[doc(hidden)]
pub fn emptier_holds_nothing() -> bool {
    EMPTYING.lock().handed.is_none()
}

// CHECKS-ONLY: object/quota_check.rs
/// Whether the OOM kill gave up waiting for `space`'s emptying.
#[doc(hidden)]
pub fn emptying_given_up(space: &AddressSpace) -> bool {
    space.oom_emptying_given_up()
}

// CHECKS-ONLY: object/quota_check.rs
/// Chunks emptied since boot, and of those how many outside a reclaim.
#[doc(hidden)]
pub fn emptier_chunks() -> (u64, u64) {
    (
        CHUNKS.load(Ordering::Relaxed),
        CHUNKS_OUTSIDE_RECLAIM.load(Ordering::Relaxed),
    )
}

/// Kill `victim`, with `resident` pages, chosen in `scope` for `scope_of`:
/// say so, audit it, count it in its job and above and in [`KILLS`], and
/// start the grace after a machine-wide kill.
fn kill_victim(scope_of: Scope, scope: &Job, victim: &Arc<dyn Host>, resident: u64) {
    let core = victim.core();
    let limited = match scope_of {
        Scope::Job(_) => {
            println!(
                "  oom      pid {} killed, {resident} pages resident, for a fault past its \
                 cgroup's memory.max",
                core.pid()
            );
            scope.id()
        }
        Scope::Machine => {
            println!(
                "  oom      pid {} killed, {resident} pages resident, for a fault with the \
                 machine out of memory",
                core.pid()
            );
            0
        }
        Scope::Kernel => {
            println!(
                "  oom      pid {} killed, {resident} pages resident, for the kernel's heap \
                 with the machine out of memory ({} frames free, the watermark {})",
                core.pid(),
                crate::mm::free_frames(),
                crate::mm::watermark()
            );
            0
        }
    };
    if scope_of == Scope::Kernel {
        // The old one, if any, is dropped after the lock: a `Weak` holds no
        // space, so nothing is torn down here.
        let old = KERNEL_VICTIM
            .lock()
            .replace((Arc::downgrade(core.own_space()), false));
        drop(old);
    }
    victim.kill(KILLED_STATUS);
    crate::audit::oom_killed(core.pid(), limited, resident);
    core.job().count_oom(true);
    let _ = KILLS.fetch_add(1, Ordering::Relaxed);
    if matches!(scope_of, Scope::Machine | Scope::Kernel) {
        LAST_MACHINE_KILL.store(crate::timer::now_nanos().max(1), Ordering::Relaxed);
    }
}
