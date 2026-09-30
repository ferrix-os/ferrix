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
//! With nothing killable left in the job, or no limit full after all -- the
//! machine out of frames, not a job out of quota -- the fault fails as it
//! did before: a `SIGSEGV` for the program's own, `EFAULT` for a page a
//! system call touched for it.
//!
//! `memory.oom.group` and `oom_score_adj` are not built: a victim is one
//! process, chosen by its resident pages alone.

use alloc::sync::Arc;
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
pub(crate) enum Answer {
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

/// Set while one fault chooses and kills, as Linux's `oom_lock` is held:
/// two faults at one full limit kill one process, not two. A flag, not a
/// lock, since a kill wakes tasks and must hold no spin lock
/// (`crate::sync::SpinLock`), and a fault that finds it set waits and
/// tries again.
static CHOOSING: AtomicBool = AtomicBool::new(false);

/// Processes the scoped OOM kill has ended since boot, for the check.
static KILLS: AtomicU64 = AtomicU64::new(0);

/// How many processes the scoped OOM kill has ended since boot.
pub(crate) fn kills() -> u64 {
    KILLS.load(Ordering::Relaxed)
}

/// Whether a fault failed the way a refused charge fails it: no frame for
/// the page, its copy, or a table.
pub(crate) fn is_charge_refusal(error: &SpaceError) -> bool {
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
    match space.fault(address, access) {
        Err(error) if is_charge_refusal(&error) => match out_of_memory(space) {
            Answer::Refused => Err(error),
            Answer::Retry | Answer::Victim => Ok(()),
        },
        Ok(()) => {
            throttle();
            Ok(())
        }
        done => done,
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
pub(crate) fn fault(space: &AddressSpace, address: u64, access: Access) -> Result<(), SpaceError> {
    quota::count(crate::sched::running_group(), Counter::Faults, 1);
    loop {
        match space.fault(address, access) {
            Err(error) if is_charge_refusal(&error) => {
                if out_of_memory(space) != Answer::Retry {
                    return Err(error);
                }
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
pub(crate) fn throttle() {
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
    if done.stolen < pages {
        crate::sched::sleep_for(PAUSE_NANOS);
    }
}

/// A fault in `space` by the running task failed for want of memory: find
/// the job whose limit is full, and kill in it. See the module.
pub(crate) fn out_of_memory(space: &AddressSpace) -> Answer {
    let group = crate::sched::running_group();
    let Some(full) = quota::at_limit(group, Resource::Memory, PAGE_SIZE) else {
        // No limit full: the machine out of frames. Its clean cache is room.
        return if cache::reclaim(quota::NONE, RECLAIM_PAGES).stolen > 0 {
            Answer::Retry
        } else {
            Answer::Refused
        };
    };
    // Reclaim inside the job before any kill: its own clean pages, and those
    // of the jobs beneath it, never a sibling's (`user::cache`).
    if cache::reclaim(full, RECLAIM_PAGES).stolen > 0 {
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
    let ours = |host: &Arc<dyn Host>| core::ptr::eq(Arc::as_ptr(host.core().space()), space);
    // Whether a process not yet ended faults here: one that shares the
    // space with an ended one (`vfork`) is not that one.
    let living = live
        .iter()
        .any(|host| ours(host) && !host.core().is_terminated());
    let mut scope: Option<Arc<Job>> = None;
    let mut victim: Option<(u64, &Arc<dyn Host>)> = None;
    let mut ending = false;
    for host in &live {
        let core = host.core();
        let Some(within) = at_or_above(core.job(), full) else {
            continue;
        };
        let _ = scope.get_or_insert(within);
        let resident = core.space().resident_pages().unwrap_or(0);
        if !core.is_terminated() {
            if core.pid() > 1 && victim.is_none_or(|(most, _)| resident > most) {
                victim = Some((resident, host));
            }
            continue;
        }
        if ours(host) && !living {
            // Its own process, already ending: nothing to wait for.
            return (Answer::Victim, false);
        }
        if resident == 0 || shared_with_the_living(core.space(), &live) {
            continue;
        }
        if core.exit().is_closed() {
            // Nothing of it runs, and nobody else maps what it held: give
            // its memory back now, not at its parent's `wait4`.
            let _ = core
                .space()
                .unmap(MMAP_MIN_ADDR, USER_VIRT_END - MMAP_MIN_ADDR);
            return (Answer::Retry, false);
        }
        ending = true;
    }
    let Some(scope) = scope else {
        return (Answer::Refused, false);
    };
    if ending {
        return (Answer::Retry, true);
    }
    // Counted once a kill is asked for, not at each look while a victim
    // ends: a poller is told of each OOM, not of each retry.
    scope.count_oom(false);
    let Some((resident, victim)) = victim else {
        return (Answer::Refused, false);
    };
    let core = victim.core();
    println!(
        "  oom      pid {} killed, {resident} pages resident, for a fault past its cgroup's \
         memory.max",
        core.pid()
    );
    victim.kill(KILLED_STATUS);
    crate::audit::oom_killed(core.pid(), scope.id(), resident);
    core.job().count_oom(true);
    let _ = KILLS.fetch_add(1, Ordering::Relaxed);
    if ours(victim) {
        (Answer::Victim, false)
    } else {
        (Answer::Retry, false)
    }
}
