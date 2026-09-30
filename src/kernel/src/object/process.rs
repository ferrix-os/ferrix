//! The process as the core knows it: an address space, a number, a handle
//! table and a place in the job tree -- and nothing a personality adds.
//!
//! # What is the core's and what is the personality's
//!
//! A process is the container the core enforces isolation between, so the
//! core has to be able to name one. But almost everything a running program
//! has is a personality's: its descriptors, its root and working directory,
//! its signal dispositions and masks, its `brk`, its credentials, its parent
//! and children. None of that has to be correct for one process to be kept
//! out of another's memory, so none of it is here.
//!
//! What is here is what the core itself enforces or reports:
//!
//! - the [`AddressSpace`], which is the isolation;
//! - the pid, which is how anything outside the process names it;
//! - when it was made;
//! - the native ABI's [`HandleTable`], which is its capabilities;
//! - its [`Job`], which is where kill authority over it lives, and the
//!   tasks it has charged there (`object::quota`);
//! - how it ended ([`Exit`]), which is what a handle to it holds;
//! - the one handle waiting for it to take with `process_bootstrap`
//!   ([`Bootstrap`]), which outlives an `execve` as the handle table does.
//!
//! # How the personality's half is reached
//!
//! Not from here. A personality's process *contains* one of these and adds
//! its own state beside it; this type has no field that leads back. Where the
//! core has to hold a process as a whole -- the pid table, a native handle to
//! a process nobody has started, a scheduled thread -- it holds a [`Host`]:
//! the personality's object, seen only through the few questions the core
//! has to ask of it. The personality recovers its own type from a `Host` by
//! [`downcast`], which answers `None` for any other.
//!
//! So the dependency points one way. The personality names this module; this
//! module names nothing above the core.
//!
//! # The pid table
//!
//! Which process is pid 42, and which processes exist at all, are questions
//! only something outside every process can answer; a job answers the second
//! whenever it is killed. The table is here, keyed by number, holding a weak
//! [`Host`] per number so that being listed never keeps a process alive.
//!
//! Numbers are chosen cyclically, as Linux's `alloc_pid` does: the number
//! after the last one handed out, skipping any still in use, wrapping past
//! [`PID_MAX`] to [`RESERVED`]. Not the lowest free number, which would hand a
//! just-freed pid straight to the next process -- so a `kill` aimed at a
//! process that had just exited would reach an unrelated one. A number is
//! reserved from the moment it is chosen, before the process is shared, so
//! two processes made at once cannot be given the same one.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals as ObjectSignals;

use crate::fallible::{self, AllocError};
use crate::object::job::{self, Job, JobError};
use crate::object::port::{self, Observer, Observers, PortError};
use crate::object::quota::{self, Resource};
use crate::object::{self as objects, HandleTable, Object};
use crate::sched::{Task, WaitQueue};
use crate::sync::SpinLock;
use crate::user::space::AddressSpace;

/// A process, as the core enforces and reports it.
#[derive(Debug)]
pub(crate) struct Process {
    /// What it can see.
    space: Arc<AddressSpace>,
    /// Its number, from [`allocate`]: what the pid table finds it by. Zero
    /// only if every number was in use when it was made, in which case
    /// nothing can find it.
    pid: u32,
    /// When it was made, in nanoseconds on the counter.
    started: u64,
    /// The handles it holds, for the native ABI.
    ///
    /// A lock of its own: a channel write looks handles up and takes them
    /// out, and has no business waiting on anything else the process has.
    handles: SpinLock<HandleTable>,
    /// The job it is in, which is its cgroup. Every process is in exactly one:
    /// the root job, its parent's for a fork, or wherever it was moved. Its
    /// lock comes before any job's (see `object::job`, "Lock order").
    membership: SpinLock<Arc<Job>>,
    /// Whether it is counted among its job's live members: from when it is
    /// made until it leaves. Changed only under `membership`.
    counted: AtomicBool,
    /// How many tasks it has charged to its job's quota: itself, and each
    /// thread beside its first. Moved with it; changed only under
    /// `membership`.
    tasks: AtomicU64,
    /// Its job's quota slot, read without the membership lock: what the
    /// scheduler files its threads under. Written only under `membership`.
    slot: AtomicU32,
    /// Whether its job, or one above, is frozen (`cgroup.freeze`): its
    /// threads stop on their way back to user mode, and nothing but the
    /// process ending lets them go. Written under `membership`, so that a
    /// move and a freeze cannot pass each other.
    frozen: AtomicBool,
    /// Whether its job's task limit refused it as it was made. Such a
    /// process charged nothing and is never started: `fork` answers
    /// `EAGAIN`, as it does for a process that got no pid.
    over_quota: bool,
    /// How it ended, and who is waiting to hear. Apart from the process,
    /// because a handle to the process holds it: see [`Exit`].
    exit: Arc<Exit>,
    /// Its bootstrap handle until it takes it: see [`Bootstrap`]. A lock of
    /// its own, taken after a handle table's and never before one.
    bootstrap: SpinLock<Bootstrap>,
    /// The speculation domain it was born in and has not left, zero for
    /// none (`docs/OPAQUE-KERNEL.md` §9.2): its job's as it is made or born
    /// there ([`Process::move_new_to`]), and zero for good once it moves
    /// between jobs or leaves ([`Process::leave_speculation_domain`]).
    domain: AtomicU64,
    /// The tasks running its code, each listed before it can run
    /// ([`Process::list_task`]). The core's, so that the core itself tells
    /// every one of them the process has ended ([`Process::end_record`]).
    task_list: SpinLock<TaskList>,
}

/// Take the live tasks of `tasks` from `next` into the empty `batch`, calling
/// `visit` on each as it is taken, until the batch is full or the list ends:
/// the index to go on from. Under the list's lock; the tasks are kept in the
/// batch, every one, so that none is let go there.
fn take_batch(
    tasks: &[Weak<Task>],
    mut next: usize,
    batch: &mut [Option<Arc<Task>>],
    mut visit: impl FnMut(&Arc<Task>),
) -> usize {
    for slot in batch.iter_mut() {
        let Some(task) = next_live(tasks, &mut next) else {
            break;
        };
        visit(&task);
        *slot = Some(task);
    }
    next
}

/// The first task of `tasks` from `next` that is still alive, with `next`
/// moved past it; `None` at the end of the list.
fn next_live(tasks: &[Weak<Task>], next: &mut usize) -> Option<Arc<Task>> {
    while let Some(listed) = tasks.get(*next) {
        *next += 1;
        if let Some(task) = listed.upgrade() {
            return Some(task);
        }
    }
    None
}

/// A process's tasks, weakly: a task keeps its process alive and not the
/// other way round.
#[derive(Debug)]
pub(crate) struct TaskList {
    /// The tasks, in the order they were listed, with those gone forgotten
    /// as the next is listed.
    tasks: Vec<Weak<Task>>,
    /// Counted up at every change, so that a walk that let go of the lock
    /// between batches can tell its place in the list is stale.
    version: u64,
}

/// Where a process's bootstrap handle waits for `process_bootstrap`
/// (`docs/INIT.md` §6, K2 and K3).
///
/// Outside the handle table, so that the handle has no number until the
/// process asks for it: a program cannot close it, or be handed another
/// object under its number, before it knows it has one. And outside the
/// personality, because what the slot holds is an object, which is the
/// core's to keep and to close.
#[derive(Debug)]
pub(crate) enum Bootstrap {
    /// Nothing given yet, and one may be: the process has not completed an
    /// `execve`.
    Open,
    /// Given, and not yet taken.
    Held(Object, Rights),
    /// Given and taken. Nothing more is given.
    Taken,
    /// It completed an `execve` with nothing given, or it has ended. Nothing
    /// is given from here on.
    Sealed,
}

/// Why a bootstrap could not be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GiveRefused {
    /// It was given one before.
    Given,
    /// It has completed an `execve`, or ended.
    Sealed,
}

impl Bootstrap {
    /// Why nothing may be given now, or `None` if something may.
    pub(crate) fn refusal(&self) -> Option<GiveRefused> {
        match self {
            Bootstrap::Open => None,
            Bootstrap::Held(..) | Bootstrap::Taken => Some(GiveRefused::Given),
            Bootstrap::Sealed => Some(GiveRefused::Sealed),
        }
    }

    /// Hold `object` as the bootstrap, if one may still be given; `object`
    /// back if not.
    ///
    /// # Errors
    ///
    /// [`GiveRefused`], with the object.
    pub(crate) fn give(
        &mut self,
        object: Object,
        rights: Rights,
    ) -> Result<(), (GiveRefused, Object)> {
        if let Some(why) = self.refusal() {
            return Err((why, object));
        }
        *self = Bootstrap::Held(object, rights);
        Ok(())
    }

    /// The bootstrap, taken, if it is waiting.
    pub(crate) fn take(&mut self) -> Option<(Object, Rights)> {
        if !matches!(self, Bootstrap::Held(..)) {
            return None;
        }
        match core::mem::replace(self, Bootstrap::Taken) {
            Bootstrap::Held(object, rights) => Some((object, rights)),
            _ => None,
        }
    }

    /// Put back what [`Bootstrap::take`] took and could not be delivered,
    /// unless the process has ended since; the object back if so.
    ///
    /// # Errors
    ///
    /// The object, for the caller to dispose of.
    pub(crate) fn put_back(&mut self, object: Object, rights: Rights) -> Result<(), Object> {
        if matches!(self, Bootstrap::Taken) {
            *self = Bootstrap::Held(object, rights);
            Ok(())
        } else {
            Err(object)
        }
    }
}

/// A [`Process`]'s domain once it has left one, for good: see
/// [`Process::leave_speculation_domain`]. Read as zero, no domain.
const LEFT: u64 = u64::MAX;

impl Process {
    /// A process over `space`, numbered `pid` -- which the caller has
    /// reserved with [`allocate`] or [`allocate_init`], or zero -- and
    /// counted in `job` from now on.
    ///
    /// # Errors
    ///
    /// [`AllocError`], before it is counted anywhere.
    pub(crate) fn new(
        space: Arc<AddressSpace>,
        pid: u32,
        job: Arc<Job>,
    ) -> Result<Process, AllocError> {
        let exit = fallible::try_arc(Exit::new())?;
        let slot = job.quota_index();
        // Charged before it is counted, and never started if refused: the
        // limit is on tasks that exist, not on tasks that run.
        let over_quota = quota::charge(slot, Resource::Tasks, 1).is_err();
        let mut flipped = job::Flipped::new();
        job.count_in(&mut flipped);
        job::notify(flipped);
        // As it is made in `job`, born in its domain, if it is one: its space
        // says so too, and a space another domain already claimed is out.
        let domain = job.domain();
        space.claim_domain(domain);
        let frozen = job.freezing();
        Ok(Process {
            space,
            pid,
            started: crate::timer::now_nanos(),
            handles: SpinLock::new(HandleTable::new(objects::HANDLE_LIMIT)),
            membership: SpinLock::new(job),
            counted: AtomicBool::new(true),
            tasks: AtomicU64::new(u64::from(!over_quota)),
            slot: AtomicU32::new(slot),
            frozen: AtomicBool::new(frozen),
            over_quota,
            exit,
            bootstrap: SpinLock::new(Bootstrap::Open),
            domain: AtomicU64::new(domain),
            task_list: SpinLock::new(TaskList {
                tasks: Vec::new(),
                version: 0,
            }),
        })
    }

    /// The speculation domain it is in: zero for none, which a process that
    /// has left one is in for good.
    pub(crate) fn speculation_domain(&self) -> u64 {
        match self.domain.load(Ordering::Acquire) {
            LEFT => 0,
            domain => domain,
        }
    }

    /// Leave its speculation domain, for good, and take its space out of
    /// every domain with it (`docs/OPAQUE-KERNEL.md` §9.2, §9.3a A1).
    ///
    /// One way only: nothing puts a process back. The core's move between
    /// jobs calls it, and the personality calls it when a process stops
    /// being dumpable -- a set-id `execve`, a change of credentials,
    /// `PR_SET_DUMPABLE` -- since a program that has risen in privilege must
    /// no longer share predictors with the programs it was born beside.
    ///
    /// **At once, not at the next switch** (the consultant's F1). A program
    /// that rises in privilege goes on in the same space, an `execve` in
    /// place, and its threads on the processors they are on, whose
    /// predictors its domain's other members may have trained a moment ago.
    /// So every processor that last ran a space of the domain issues the
    /// barrier before this returns: this one at once, the others as they
    /// answer the grace period this waits for (`arch::leaving_domain`, and
    /// its local check, the certification finding F-60).
    /// Leaving is rare -- a move between jobs, a loss of dumpability -- and
    /// the wait is the one a grace period costs. Never with a lock held,
    /// which no caller does: see `smp::synchronize`.
    ///
    /// Out for good: [`LEFT`], which a birth by [`Process::move_new_to`]
    /// leaves as it is, so a process that left in the root job before it was
    /// moved into the job it was made for (the consultant's F2: `inherit` of
    /// a parent that is not dumpable, before `process_create` moves the
    /// child) is never a member.
    pub(crate) fn leave_speculation_domain(&self) {
        let was = self.domain.swap(LEFT, Ordering::AcqRel);
        self.space.leave_domain();
        if was != 0 && was != LEFT {
            // The grace period below waits on every processor: never under a
            // spin lock, never with interrupts masked, which is a read-side
            // section here (`smp::synchronize`'s rule, the consultant's C1).
            // Checked, so that a caller that breaks it stops at boot.
            if !crate::sched::may_block() {
                crate::panic::fatal!(
                    crate::panic::catalog::SPECULATION_DOMAIN_LEAVE_MAY_NOT_WAIT,
                    "a member left its speculation domain where it may not wait: {} \
                     preemption-disabling locks held, interrupts {}",
                    crate::sched::locks_here().map_or(0, |(_, held, _)| held),
                    if crate::arch::interrupts_enabled() {
                        "on"
                    } else {
                        "masked"
                    }
                );
            }
            // Waits for a grace period, the barrier's answers in it.
            crate::arch::leaving_domain(was);
        }
    }

    /// [`Process::leave_speculation_domain`] for a process that has never
    /// run, whose space no processor has run in its name: out for good, with
    /// no barrier to ask for. A fork's child, as `fork` makes it.
    pub(crate) fn leave_speculation_domain_unstarted(&self) {
        let _ = self.domain.swap(LEFT, Ordering::AcqRel);
        self.space.leave_domain();
    }

    /// Whether its job's task limit refused it as it was made: a process
    /// that must not be started.
    pub(crate) fn over_quota(&self) -> bool {
        self.over_quota
    }

    /// Whether its job, or one above, is frozen: the answer is read on every
    /// way back to user mode, and is a load.
    pub(crate) fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    /// Look at its job again, and answer whether it is to be frozen and
    /// whether that changed. Under `membership`, which a move also holds.
    pub(crate) fn sync_freeze(&self) -> (bool, bool) {
        let membership = self.membership.lock();
        let now = membership.freezing();
        let before = self.frozen.swap(now, Ordering::SeqCst);
        (now, before != now)
    }

    /// Its job's quota slot, read without a lock.
    pub(crate) fn quota_slot(&self) -> u32 {
        self.slot.load(Ordering::Acquire)
    }

    /// Charge a thread beside its first to its job, before the thread's id
    /// is chosen.
    ///
    /// # Errors
    ///
    /// [`quota::Exceeded`] when the job, or one above it, is at its task
    /// limit.
    pub(crate) fn charge_thread(&self) -> Result<(), quota::Exceeded> {
        let membership = self.membership.lock();
        quota::charge(membership.quota_index(), Resource::Tasks, 1)?;
        let _ = self.tasks.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Take back every task it charged, once it has been reaped: Linux's
    /// `release_task`, where `pids` uncharges. A zombie keeps its charge, as
    /// it keeps its pid, so a loop of forks nobody waits for is bounded too;
    /// a reaped one gives it back at once, not when the last reference to it
    /// goes, which a task not yet freed may still hold. Nothing on a second
    /// call, nor for a thread's charge let go after.
    pub(crate) fn uncharge_tasks(&self) {
        let membership = self.membership.lock();
        let charged = self.tasks.swap(0, Ordering::AcqRel);
        quota::uncharge(membership.quota_index(), Resource::Tasks, charged);
    }

    /// Take back what [`Process::charge_thread`] charged, as the thread's id
    /// is given back.
    pub(crate) fn uncharge_thread(&self) {
        let membership = self.membership.lock();
        let charged = self
            .tasks
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |tasks| {
                tasks.checked_sub(1)
            });
        if charged.is_ok() {
            quota::uncharge(membership.quota_index(), Resource::Tasks, 1);
        }
    }

    /// What it can see.
    pub(crate) fn space(&self) -> &Arc<AddressSpace> {
        &self.space
    }

    /// Its process id; zero if it was made with every pid in use.
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// When it was made, in nanoseconds on the counter.
    pub(crate) fn started(&self) -> u64 {
        self.started
    }

    /// Do something with the handle table, under its lock.
    ///
    /// Whatever `change` takes out of the table it should hand back rather
    /// than drop, so that the object dies after the lock is released: an
    /// object's drop can free memory and drain other objects, and
    /// `crate::object::dispose` is where that belongs.
    pub(crate) fn with_handles<R>(&self, change: impl FnOnce(&mut HandleTable) -> R) -> R {
        change(&mut self.handles.lock())
    }

    /// The job it is in: its cgroup.
    pub(crate) fn job(&self) -> Arc<Job> {
        Arc::clone(&self.membership.lock())
    }

    /// Move it into `to`, counting it there and not where it was, if it is
    /// still counted. Its threads go with it: a job holds processes.
    ///
    /// # Errors
    ///
    /// [`JobError::Killed`] if `to`, or a job above it, has been killed;
    /// [`JobError::Removed`] if `rmdir` took it; [`JobError::Internal`] if
    /// the no-internal-process rule keeps processes out of it.
    pub(crate) fn move_to(&self, to: &Arc<Job>) -> Result<(), JobError> {
        self.move_charged(to, false)
    }

    /// [`Process::move_to`] for a process being made, which a task limit at
    /// `to` refuses as it would refuse the process made there: native
    /// `process_create`, whose child is built in the root job and then put
    /// in its own. A move of a running process is not refused for it, as
    /// Linux's is not.
    ///
    /// # Errors
    ///
    /// As [`Process::move_to`], and [`JobError::Limited`] for the limit.
    pub(crate) fn move_new_to(&self, to: &Arc<Job>) -> Result<(), JobError> {
        self.move_charged(to, true)
    }

    /// Move it, and the tasks it charged, into `to`; with `checked`, only if
    /// `to`'s task limits allow them.
    fn move_charged(&self, to: &Arc<Job>, checked: bool) -> Result<(), JobError> {
        let mut flipped = job::Flipped::new();
        let left = {
            let mut membership = self.membership.lock();
            if to.refuses() {
                return Err(JobError::Killed);
            }
            if Arc::ptr_eq(&membership, to) {
                return Ok(());
            }
            let tasks = self.tasks.load(Ordering::Acquire);
            if checked {
                quota::charge(to.quota_index(), Resource::Tasks, tasks)
                    .map_err(|_| JobError::Limited)?;
            }
            let counted = if self.counted.load(Ordering::Acquire) {
                to.count_in_checked(&mut flipped).map(|()| {
                    membership.count_out(&mut flipped);
                })
            } else {
                to.admits()
            };
            if let Err(why) = counted {
                if checked {
                    quota::uncharge(to.quota_index(), Resource::Tasks, tasks);
                }
                return Err(why);
            }
            if !checked {
                quota::charge_regardless(to.quota_index(), Resource::Tasks, tasks);
            }
            quota::uncharge(membership.quota_index(), Resource::Tasks, tasks);
            self.slot.store(to.quota_index(), Ordering::Release);
            // Frozen with the cgroup it goes into, thawed with the one it
            // leaves; the personality kicks its threads, having moved it.
            self.frozen.store(to.freezing(), Ordering::SeqCst);
            core::mem::replace(&mut *membership, Arc::clone(to))
        };
        // Outside the lock: it may be the last reference to that job.
        drop(left);
        // Its speculation domain (`docs/OPAQUE-KERNEL.md` §9.2): a process
        // being made is born in the job it was made for, its space with it;
        // a running one that moves leaves its domain for good, wherever it
        // went, and joins none by moving in.
        if checked {
            let domain = to.domain();
            let held = self.domain.load(Ordering::Acquire);
            // A process that left a domain before its birth, as one made not
            // dumpable by its creator does, is out for good (F2).
            if held != LEFT && (held != 0 || domain != 0) {
                self.domain.store(domain, Ordering::Release);
                self.space.set_birth_domain(domain);
            }
        } else {
            self.leave_speculation_domain();
        }
        job::notify(flipped);
        crate::sched::note_moved();
        Ok(())
    }

    /// Stop counting it among its job's live members, once. What makes its
    /// job empty when it was the last: called as the process lets go of what
    /// it holds, and as it is dropped if it never did.
    pub(crate) fn leave_job(&self) {
        let mut flipped = job::Flipped::new();
        {
            let membership = self.membership.lock();
            if self.counted.swap(false, Ordering::AcqRel) {
                membership.count_out(&mut flipped);
            }
        }
        job::notify(flipped);
    }

    /// How it ended, and who is waiting to hear.
    pub(crate) fn exit(&self) -> &Exit {
        &self.exit
    }

    /// Record how it ended, as [`Exit::record`] does, and then post
    /// [`END`](crate::sched::work::END) to every task of it, waking each and
    /// interrupting its processor. The store of `terminated` and the bit the
    /// way back to user mode reads are both the core's, so that a task
    /// spinning in user mode is ended whatever ended its process: an
    /// `exit_group`, a kill, a job's kill, a last thread's exit. The
    /// personality's end does this once, first.
    ///
    /// A task listed after the walk here read the list sees `terminated`: it
    /// is listed under the list's lock, after this store, and its start reads
    /// `terminated` again once it has listed it.
    ///
    /// The fence pairs with `WaitQueue::wait_trusting`'s, after the waiter
    /// stores `BLOCKED`: of the two the second sees the other side's store,
    /// so a waiter that looked before the end is woken here.
    pub(crate) fn end_record(&self, status: i32, signal: u32) {
        let _posting = crate::sched::work::posting();
        self.exit.record(status, signal);
        core::sync::atomic::fence(Ordering::SeqCst);
        self.post_to_tasks(crate::sched::work::END, None);
    }

    /// List `task` as running its code, before it can run, forgetting the
    /// tasks that have gone first: a process that starts and joins threads
    /// in a loop would otherwise keep every task's allocation.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the list cannot grow: the start that asked fails,
    /// before its task runs.
    pub(crate) fn list_task(&self, task: &Arc<Task>) -> Result<(), AllocError> {
        let mut list = self.task_list.lock();
        list.tasks.retain(|listed| listed.strong_count() > 0);
        fallible::try_push(&mut list.tasks, Arc::downgrade(task))?;
        list.version = list.version.wrapping_add(1);
        Ok(())
    }

    /// Look at the tasks listed, under the list's lock. `look` must not drop
    /// a task it upgrades there: the last reference to a task gives back an
    /// address space, which no lock may be held across.
    pub(crate) fn with_tasks<R>(&self, look: impl FnOnce(&[Weak<Task>]) -> R) -> R {
        look(&self.task_list.lock().tasks)
    }

    /// Post `bits` to every task listed but `except`, waking each and
    /// interrupting its processor, as `sched::work::notify` does.
    ///
    /// Nothing allocated and no task let go under the list's lock: the tasks
    /// are taken out a batch at a time onto this stack, posted to under the
    /// lock, and woken and let go once it is. A batch that finds the list
    /// changed since the last starts again from the top; posting twice is
    /// harmless, and the list changes only as a start lists a task, which
    /// reads what was posted for after it lists its own.
    pub(crate) fn post_to_tasks(&self, bits: u32, except: Option<&Task>) {
        const BATCH: usize = 8;
        let is_except = |task: &Arc<Task>| {
            except.is_some_and(|except| core::ptr::eq(except, Arc::as_ptr(task)))
        };
        let post = |task: &Arc<Task>| {
            if !is_except(task) {
                crate::sched::work::post(task, bits);
            }
        };
        let mut place = (0, None);
        loop {
            let mut batch: [Option<Arc<Task>>; BATCH] = [const { None }; BATCH];
            let done = self.take_listed(&mut place, &mut batch, post);
            for task in batch.iter().flatten().filter(|task| !is_except(task)) {
                crate::sched::work::wake_posted(task);
            }
            drop(batch);
            if done {
                return;
            }
        }
    }

    /// One batch of [`Process::post_to_tasks`]'s walk, under the list's lock:
    /// from `place` -- the next index, and the list's version it was taken
    /// at, which starts the walk again from the top when the list changed --
    /// into `batch`, with `visit` called on each task taken. Answers whether
    /// the walk reached the end of the list.
    fn take_listed(
        &self,
        place: &mut (usize, Option<u64>),
        batch: &mut [Option<Arc<Task>>],
        visit: impl FnMut(&Arc<Task>),
    ) -> bool {
        let list = self.task_list.lock();
        if place.1 != Some(list.version) {
            *place = (0, Some(list.version));
        }
        place.0 = take_batch(&list.tasks, place.0, batch, visit);
        place.0 >= list.tasks.len()
    }

    /// Do something with its bootstrap slot, under its lock. What `change`
    /// takes out it hands back, for the reason [`Process::with_handles`]
    /// gives.
    pub(crate) fn with_bootstrap<R>(&self, change: impl FnOnce(&mut Bootstrap) -> R) -> R {
        change(&mut self.bootstrap.lock())
    }

    /// It has completed an `execve`: a bootstrap not yet given never will be.
    /// One already given stays for the new program to take.
    pub(crate) fn seal_bootstrap(&self) {
        let mut slot = self.bootstrap.lock();
        if slot.refusal().is_none() {
            *slot = Bootstrap::Sealed;
        }
    }

    /// It has ended: seal the slot, and answer what it still held, for the
    /// caller to dispose of with no lock held.
    pub(crate) fn close_bootstrap(&self) -> Option<Object> {
        match core::mem::replace(&mut *self.bootstrap.lock(), Bootstrap::Sealed) {
            Bootstrap::Held(object, _) => Some(object),
            _ => None,
        }
    }

    /// A reference to how it ends, which outlives it.
    pub(crate) fn exit_record(&self) -> Arc<Exit> {
        Arc::clone(&self.exit)
    }

    /// Whether it has terminated, by exiting or by being killed.
    pub(crate) fn is_terminated(&self) -> bool {
        self.exit.is_terminated()
    }

    /// How it ended, once it has.
    pub(crate) fn exit_status(&self) -> Option<i32> {
        self.exit.status()
    }

    /// The queue woken as it lets go of what it held: once its handles and
    /// descriptors are closed, and again once it is released.
    pub(crate) fn exited(&self) -> &WaitQueue {
        self.exit.exited()
    }
}

impl Drop for Process {
    /// Leave its job's count if it never did, and give the pid back. The
    /// number is not used again until allocation comes round to it.
    ///
    /// A process dropped without being released -- one built and never
    /// shared, as a failed `execve` of a new program leaves -- leaves its
    /// job's count here instead. A released one already has, and this does
    /// nothing, so the reaper, which drops released processes only, never
    /// wakes anything from here.
    fn drop(&mut self) {
        self.leave_job();
        let charged = core::mem::take(self.tasks.get_mut());
        quota::uncharge(
            self.membership.get_mut().quota_index(),
            Resource::Tasks,
            charged,
        );
        if self.pid != 0 {
            release(self.pid);
        }
    }
}

/// The personality's process a core [`Process`] lives in, seen from the core.
///
/// What the core holds when it has to hold a process as a whole, and asks
/// when a decision is the personality's: how a process ends, and what happens
/// as its threads come and go, depend on what the personality keeps beside
/// the core fields -- descriptors to close, a parent to tell -- and the core
/// does not know what that is.
///
/// `Any`, so that the personality can have its own type back ([`downcast`]).
pub(crate) trait Host: Any + Send + Sync + fmt::Debug {
    /// The core process inside it.
    fn core(&self) -> &Process;

    /// End it from outside with `status`: what a job kill, and dropping the
    /// last handle to a process nobody started, do. Its threads find out on
    /// their way back to user mode; nothing here waits for that.
    fn kill(&self, status: i32);

    /// Count a thread about to start. Before its task is spawned, because the
    /// task can reach its exit on another processor before the spawn returns.
    fn thread_starting(&self);

    /// Count a thread gone -- one that `ended`, or one whose task could not be
    /// spawned -- and, if that was its last, end the process or let go of
    /// what it holds, as the personality decides.
    fn thread_gone(&self, ended: bool);

    /// Whether a wait on its behalf should end early: it is ending, or the
    /// personality has something for the waiting thread -- a signal, say --
    /// that no wait may sleep through. What a wait in the item checks beside
    /// its own condition, as the personality's own waits do.
    fn wait_interrupted(&self) -> bool;
}

/// The personality's own type back from a [`Host`], or `None` if `host` is
/// not a `T`.
pub(crate) fn downcast<T: Host>(host: Arc<dyn Host>) -> Option<Arc<T>> {
    let any: Arc<dyn Any + Send + Sync> = host;
    any.downcast::<T>().ok()
}

/// How a process ended, and who is waiting to hear.
///
/// Apart from the process, because this is what a handle to a process holds.
/// A handle kept past the end must not keep the address space and everything
/// else the process owned, and a wait needs nothing else. The personality
/// records how it ended ([`Exit::record`]) and closes it once the process has
/// let go of what it held ([`Exit::close`]); nothing else writes it.
#[derive(Debug)]
pub(crate) struct Exit {
    /// Its exit status, valid once `terminated` is.
    status: AtomicI32,
    /// The signal that ended it, or zero.
    ended_by: AtomicU32,
    /// The terminated condition. Set after `status`, so a reader who sees it
    /// always reads the status that goes with it.
    terminated: AtomicBool,
    /// Woken as it lets go of what it held: once its handles and descriptors are
    /// closed, and again once it is released.
    exited: WaitQueue,
    /// Port registrations waiting for it to end, and `None` once it has
    /// closed its handles and descriptors and they have been taken.
    observers: SpinLock<Option<Observers>>,
    /// Set under the observers lock as they are taken, so that
    /// [`Exit::is_closed`], which every poll of a handle's signals asks, need
    /// not take the lock.
    closed: AtomicBool,
}

impl Exit {
    /// Not ended, and watched by nobody.
    fn new() -> Exit {
        Exit {
            status: AtomicI32::new(0),
            ended_by: AtomicU32::new(0),
            terminated: AtomicBool::new(false),
            exited: WaitQueue::new(),
            observers: SpinLock::new(Some(Observers::new())),
            closed: AtomicBool::new(false),
        }
    }

    /// An end no process has, for a boot check that needs one: terminated
    /// already when `ended`, as a dying process's is before it lets go of
    /// anything (`object::pin`'s quarantine check).
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn for_check(ended: bool) -> Result<Arc<Exit>, AllocError> {
        let exit = fallible::try_arc(Exit::new())?;
        if ended {
            exit.record(0, 0);
        }
        Ok(exit)
    }

    /// Whether it has terminated, by exiting or by being killed. True from the
    /// moment it starts to end, before it has let go of anything.
    pub(crate) fn is_terminated(&self) -> bool {
        self.terminated.load(Ordering::Acquire)
    }

    /// Whether it has ended and closed its handles and descriptors: what a
    /// handle's `TERMINATED` signal reports, a little after
    /// [`Exit::is_terminated`] is true.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Its exit status, once it has terminated.
    pub(crate) fn status(&self) -> Option<i32> {
        self.is_terminated()
            .then(|| self.status.load(Ordering::Acquire))
    }

    /// The signal that ended it, if one did.
    pub(crate) fn signal(&self) -> Option<u32> {
        let signal = self.ended_by.load(Ordering::Acquire);
        (self.is_terminated() && signal != 0).then_some(signal)
    }

    /// The queue woken as it lets go of what it held.
    pub(crate) fn exited(&self) -> &WaitQueue {
        &self.exited
    }

    /// Queue `observer`'s packet once it has ended and closed its handles, or
    /// at once if it already has.
    ///
    /// # Errors
    ///
    /// [`PortError::Full`] at [`port::MAX_OBSERVERS`] registrations;
    /// [`PortError::NoMemory`].
    pub(crate) fn observe(&self, observer: Observer) -> Result<(), PortError> {
        debug_assert!(
            crate::arch::interrupts_enabled(),
            "a process's watchers were reached with interrupts off, which its plain lock may not be"
        );
        let mut observers = self.observers.lock();
        if let Some(list) = observers.as_mut() {
            return port::register(list, observer);
        }
        drop(observers);
        observer.fire(ObjectSignals::TERMINATED);
        Ok(())
    }

    /// Record how it ended: with `status`, and by `signal` when that is not
    /// zero. Only [`Process::end_record`], which posts `END` after it, and a
    /// boot check's end with no process ([`Exit::for_check`]) store it.
    fn record(&self, status: i32, signal: u32) {
        self.ended_by.store(signal, Ordering::Release);
        self.status.store(status, Ordering::Release);
        self.terminated.store(true, Ordering::Release);
    }

    /// Take the registrations waiting for it, for the caller to fire once it
    /// holds no lock, and refuse to keep any more.
    pub(crate) fn close(&self) -> Vec<Observer> {
        debug_assert!(
            crate::arch::interrupts_enabled(),
            "a process's watchers were reached with interrupts off, which its plain lock may not be"
        );
        let mut observers = self.observers.lock();
        self.closed.store(true, Ordering::Release);
        observers
            .take()
            .map(|mut observers| observers.take_listed())
            .unwrap_or_default()
    }
}

/// What a handle to a process holds.
///
/// Its [`Exit`], and not the process: see there. A handle `process_create`
/// made also holds the process's [`Control`], shared by every duplicate of
/// that handle: the weak way back to the process that `process_start` needs.
#[derive(Debug, Clone)]
pub(crate) struct ProcessRef {
    /// How it ended.
    exit: Arc<Exit>,
    /// The way back to it, for a process made through the native ABI.
    control: Option<Arc<Control>>,
}

/// The way from a created process's handles back to the process.
///
/// Weak, so that a handle kept past the end keeps nothing of the process but
/// its [`Exit`]. Strong only until it starts: nothing else holds a process no
/// task runs, so its handles have to, and a start hands that reference over to
/// the process's task.
#[derive(Debug)]
pub(crate) struct Control {
    /// The process, while anything else holds it.
    process: Weak<dyn Host>,
    /// The only strong reference to a process nobody has started.
    unstarted: SpinLock<Option<Arc<dyn Host>>>,
}

impl ProcessRef {
    /// A handle's view of `process`, with no way back to it.
    pub(crate) fn new(process: &Process) -> ProcessRef {
        ProcessRef {
            exit: Arc::clone(&process.exit),
            control: None,
        }
    }

    /// A handle to `host`, made and not yet started, which holds it until a
    /// start takes it over or the last such handle is closed.
    ///
    /// # Errors
    ///
    /// [`AllocError`]. `host` has been dropped, unstarted -- and so, by
    /// [`Control`]'s rule, killed first.
    pub(crate) fn created(host: Arc<dyn Host>) -> Result<ProcessRef, AllocError> {
        let exit = Arc::clone(&host.core().exit);
        let process = Arc::downgrade(&host);
        let control = match fallible::try_arc(Control {
            process,
            unstarted: SpinLock::new(Some(Arc::clone(&host))),
        }) {
            Ok(control) => control,
            Err(error) => {
                host.kill(job::KILLED_STATUS);
                return Err(error);
            }
        };
        drop(host);
        Ok(ProcessRef {
            exit,
            control: Some(control),
        })
    }

    /// How it ended, and who is waiting to hear.
    pub(crate) fn exit(&self) -> &Exit {
        &self.exit
    }

    /// Its exit status, once it has terminated.
    pub(crate) fn exit_status(&self) -> Option<i32> {
        self.exit.status()
    }

    /// The way back to the process, if this handle was made with one.
    pub(crate) fn control(&self) -> Option<&Arc<Control>> {
        self.control.as_ref()
    }
}

impl Control {
    /// The process, if it still exists and is a `T`.
    ///
    /// Never asked on a wait path: a wait needs only the [`Exit`], and a
    /// process that has gone answers `None` here, not a panic.
    pub(crate) fn process<T: Host>(&self) -> Option<Arc<T>> {
        downcast(self.process.upgrade()?)
    }

    /// The process, if it still exists, as the core holds it: what a start
    /// through the native ABI hands the personality back.
    pub(crate) fn host(&self) -> Option<Arc<dyn Host>> {
        self.process.upgrade()
    }

    /// Let go of the reference that kept it before it started, now that its
    /// task holds it.
    pub(crate) fn started(&self) {
        let held = self.unstarted.lock().take();
        drop(held);
    }
}

impl Drop for Control {
    /// End a process nobody started, once no handle is left that could start
    /// it.
    ///
    /// Such a process is held only here. Letting go of it without ending it
    /// would free it without its end running, so no status would be recorded
    /// and its watchers would never hear. So it is killed first.
    ///
    /// # Where this runs
    ///
    /// Only where a handle object is dropped. Every such drop goes through
    /// `object::dispose`, and every caller of that is in task context with
    /// interrupts on:
    /// - a native call's handler;
    /// - a channel's own drop or refusal, reached only inside such a drain;
    /// - a process's end closing a handle table, which since the fault-kill
    ///   fix never runs with interrupts masked.
    ///
    /// A drain running on another processor is that processor's calling task,
    /// not an interrupt. The idle reaper, which the rule "never kill in Drop"
    /// is about, drops only processes whose end has already emptied their
    /// table, so it never holds a `Control`. The assertion is the tripwire, as
    /// `Exit::close`'s is. The reference is taken out of the lock before the
    /// kill, so the end runs under nothing of this lock's.
    fn drop(&mut self) {
        let unstarted = self.unstarted.lock().take();
        if let Some(process) = unstarted {
            debug_assert!(
                crate::arch::interrupts_enabled(),
                "an unstarted process's last handle was dropped with interrupts off"
            );
            process.kill(job::KILLED_STATUS);
        }
    }
}

/// One past the largest pid: Linux's default `pid_max`.
pub(crate) const PID_MAX: u32 = 32_768;

/// Where numbering resumes after wrapping. Linux's `RESERVED_PIDS`: the
/// numbers below it stay with whatever started at boot.
pub(crate) const RESERVED: u32 = 300;

/// The pid Linux gives the first user process, which programs rely on: a shell
/// running as init reports `$$` as 1, its children see 1 as their parent, and
/// busybox's `init` refuses to run as anything else. [`allocate`] never hands
/// it out; [`allocate_init`] does.
pub(crate) const INIT_PID: u32 = 1;

/// The pid table.
#[derive(Debug)]
struct Table {
    /// Every number in use. `None` for one reserved for a process still being
    /// built; an entry that does not upgrade is held by one being dropped.
    live: BTreeMap<u32, Option<Weak<dyn Host>>>,
    /// The number handed out last.
    last: u32,
}

/// The one table: numbers are global until stage 13's pid namespaces.
static TABLE: SpinLock<Table> = SpinLock::new(Table {
    live: BTreeMap::new(),
    // So the first ordinary pid is 2: 1 is init's.
    last: INIT_PID,
});

/// Choose and reserve a number, or `None` if every one is in use or there was
/// no memory to record it.
pub(crate) fn allocate() -> Option<u32> {
    let mut guard = TABLE.lock();
    let table = &mut *guard;
    let mut candidate = table.last;
    for _ in 0..PID_MAX {
        candidate = if candidate + 1 >= PID_MAX {
            RESERVED
        } else {
            candidate + 1
        };
        if !table.live.contains_key(&candidate) {
            let _ = fallible::insert(&mut table.live, candidate, None).ok()?;
            table.last = candidate;
            return Some(candidate);
        }
    }
    None
}

/// Reserve [`INIT_PID`] for the process init starts, or `None` if a process
/// still holds it or there was no memory to record it.
pub(crate) fn allocate_init() -> Option<u32> {
    let mut guard = TABLE.lock();
    if guard.live.contains_key(&INIT_PID) {
        return None;
    }
    let _ = fallible::insert(&mut guard.live, INIT_PID, None).ok()?;
    Some(INIT_PID)
}

/// Whether no process, live or on its way out, holds `number`.
pub(crate) fn is_free(number: u32) -> bool {
    !TABLE.lock().live.contains_key(&number)
}

/// Have `number` find `process` from now on: its pid once it is complete, or
/// one of its threads' numbers, which find their process too.
///
/// A number [`allocate`] reserved is already in the table and naming it
/// allocates nothing.
///
/// # Errors
///
/// [`AllocError`] for a number not reserved, when there was no memory to add
/// it.
pub(crate) fn name(number: u32, process: Weak<dyn Host>) -> Result<(), AllocError> {
    let mut table = TABLE.lock();
    if let Some(entry) = table.live.get_mut(&number) {
        *entry = Some(process);
        return Ok(());
    }
    fallible::insert(&mut table.live, number, Some(process)).map(|_| ())
}

/// Give `number` back, whatever it names. Called by a process as it is
/// dropped, and by the boot check.
pub(crate) fn release(number: u32) {
    let _ = TABLE.lock().live.remove(&number);
}

/// Give `number` back if it still names `process`.
pub(crate) fn release_naming<H: Host>(number: u32, process: &H) {
    let mut table = TABLE.lock();
    if table
        .live
        .get(&number)
        .is_some_and(|entry| names(entry.as_ref(), process))
    {
        let _ = table.live.remove(&number);
    }
}

/// How many numbers name `process`: its pid, and one for each of its threads
/// that holds a number of its own.
pub(crate) fn numbers_naming<H: Host>(process: &H) -> usize {
    TABLE
        .lock()
        .live
        .values()
        .filter(|entry| names(entry.as_ref(), process))
        .count()
}

/// Whether a table entry leads to `process`. Compared by address and never by
/// upgrading: a reference upgraded under the table lock could be a process's
/// last, and dropping a process takes the lock.
fn names<H: Host>(entry: Option<&Weak<dyn Host>>, process: &H) -> bool {
    entry.is_some_and(|entry| core::ptr::addr_eq(entry.as_ptr(), process))
}

/// The live process `number` names.
pub(crate) fn find(number: u32) -> Option<Arc<dyn Host>> {
    TABLE
        .lock()
        .live
        .get(&number)
        .and_then(Option::as_ref)
        .and_then(Weak::upgrade)
}

/// Every live process, in ascending pid order, each once.
///
/// The strong references are taken under the lock and the lock released
/// before they are returned, so a caller that drops the last reference to a
/// process drops it with the table unlocked -- [`release`] needs the lock.
///
/// # Errors
///
/// [`AllocError`].
pub(crate) fn live() -> Result<Vec<Arc<dyn Host>>, AllocError> {
    // Each process once, under its pid and not under its threads' numbers.
    // Filtered after the lock is let go, since a reference dropped here may be
    // a process's last, and dropping a process takes the lock.
    let mut entries: Vec<(u32, Arc<dyn Host>)> = {
        let table = TABLE.lock();
        fallible::try_collect(table.live.iter().filter_map(|(&number, entry)| {
            entry
                .as_ref()
                .and_then(Weak::upgrade)
                .map(|host| (number, host))
        }))?
    };
    // In place: the pairs whose number is not their process's pid go, and
    // the processes move into the space the pairs held.
    entries.retain(|(number, host)| host.core().pid() == *number);
    fallible::try_collect(entries.into_iter().map(|(_, host)| host))
}
