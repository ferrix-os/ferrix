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
//! no live process shares, has that space emptied here, as Linux's OOM
//! reaper empties a victim's (`mm/oom_kill.c`). A fault that finds a victim
//! of its job still ending waits for it a moment and tries again, as
//! Linux's selection aborts while a victim is still exiting.
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

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END};

use super::job::{Job, KILLED_STATUS};
use super::process::{self, Host};
use super::quota::{self, Counter, Resource};
use crate::console::println;
use crate::user::cache;
use crate::user::space::{Access, AddressSpace, MMAP_MIN_ADDR, SpaceError};
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
pub(crate) fn user_fault(
    space: &AddressSpace,
    address: u64,
    access: Access,
) -> Result<(), SpaceError> {
    quota::count(crate::sched::running_group(), Counter::Faults, 1);
    // Retried here rather than by running the instruction again, so that the
    // answers one fault takes are counted ([`out_of_memory_at`]).
    let mut attempt = 0_u32;
    loop {
        match space.fault(address, access) {
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
        Some(full) => cache::reclaim(full, run).gave(),
        None => {
            let short = crate::mm::watermark()
                .saturating_add(1)
                .saturating_sub(crate::mm::free_frames());
            cache::reclaim(quota::NONE, run.saturating_add(short)).gave()
        }
    }
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
    if reclaimed {
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
    let (answer, pause) = kill_in(space, Scope::Machine, live);
    CHOOSING.store(false, Ordering::Release);
    if pause {
        crate::sched::sleep_for(PAUSE_NANOS);
    }
    answer
}

/// Where an OOM kill chooses its victim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    /// The job whose memory quota slot is this, and the jobs beneath it.
    Job(u32),
    /// Every process: the machine is out of frames.
    Machine,
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
        .any(|host| !host.core().is_terminated() && Arc::ptr_eq(host.core().space(), space))
}

/// Choose and kill in the job whose memory quota slot is `full`, for a
/// fault in `space`, and whether to pause before the fault is tried again:
/// a victim is still ending. Under [`CHOOSING`].
fn kill_within(space: &AddressSpace, full: u32) -> (Answer, bool) {
    let Ok(live) = process::live() else {
        return (Answer::Refused, false);
    };
    kill_in(space, Scope::Job(full), &live)
}

/// Choose among `live` in `scope_of` and kill, for a fault in `space`, and
/// whether to pause before the fault is tried again: a victim is still
/// ending. Under [`CHOOSING`].
fn kill_in(space: &AddressSpace, scope_of: Scope, live: &[Arc<dyn Host>]) -> (Answer, bool) {
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
    if scope_of == Scope::Machine && in_grace() {
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
    if core::ptr::eq(Arc::as_ptr(victim.core().space()), space) {
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
/// already ended or an ended one's space was emptied here.
fn survey<'a>(
    space: &AddressSpace,
    scope_of: Scope,
    live: &'a [Arc<dyn Host>],
) -> Result<Survey<'a>, (Answer, bool)> {
    let ours = |host: &Arc<dyn Host>| core::ptr::eq(Arc::as_ptr(host.core().space()), space);
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
            Scope::Machine => Some(core.job()),
        };
        let Some(within) = within else {
            continue;
        };
        let _ = looked.scope.get_or_insert(within);
        let resident = core.space().resident_pages().unwrap_or(0);
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
            Ended::Emptied => return Err((Answer::Retry, false)),
            Ended::Ending => looked.ending = true,
        }
    }
    Ok(looked)
}

/// What [`kill_in`] makes of a process that has ended.
enum Ended {
    /// Nothing of its memory to wait for: none resident, or shared with
    /// the living.
    Gone,
    /// Its space was emptied here.
    Emptied,
    /// Still ending, its memory still held.
    Ending,
}

/// An ended process `host` with `resident` pages, among `live`: empty its
/// space if nothing of it runs and nobody else maps what it held, as Linux's
/// OOM reaper does, rather than at its parent's `wait4`.
fn ended(host: &Arc<dyn Host>, resident: u64, live: &[Arc<dyn Host>]) -> Ended {
    let core = host.core();
    if resident == 0 || shared_with_the_living(core.space(), live) {
        return Ended::Gone;
    }
    if core.exit().is_closed() {
        let _ = core
            .space()
            .unmap(MMAP_MIN_ADDR, USER_VIRT_END - MMAP_MIN_ADDR);
        return Ended::Emptied;
    }
    Ended::Ending
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
    };
    victim.kill(KILLED_STATUS);
    crate::audit::oom_killed(core.pid(), limited, resident);
    core.job().count_oom(true);
    let _ = KILLS.fetch_add(1, Ordering::Relaxed);
    if scope_of == Scope::Machine {
        LAST_MACHINE_KILL.store(crate::timer::now_nanos().max(1), Ordering::Relaxed);
    }
}
