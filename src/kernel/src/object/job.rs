//! Jobs: containers of processes, and where kill authority lives.
//!
//! `docs/ARCHITECTURE.md` §3 and §7. A userspace driver that wedges has to be
//! killable as a unit, together with anything it started, and a job is that
//! unit: a tree of jobs, each holding processes, where killing one ends every
//! process in it and in every job beneath it.
//!
//! Since stage 13 a job is also a cgroup (`docs/CGROUPS.md`): cgroupfs is a
//! view of this tree. So every process is in exactly one job -- the root
//! job's, [`root`], unless something put it elsewhere -- and a fork's child
//! is in its parent's.
//!
//! # Who keeps whom alive
//!
//! A process holds its job strongly (`Process::job`), a child holds its
//! parent strongly, and a parent holds a *named* child strongly, as a
//! directory holds its entries, until the name is removed. An anonymous child,
//! which native `job_create` makes, is held only by its handles, its members
//! and its children. A job does not hold its processes: it finds them by
//! walking the pid table (`object::process`) for the ones whose job it is, or
//! is above.
//!
//! # Populated, counted
//!
//! Whether a job has a member that has not ended is what `cgroup.events`
//! reports and what a service manager waits for, so it is counted rather than
//! found: `live` is how many of its own members have not been released, and
//! `busy` how many of its children are populated. Every change to either is
//! made under [`TREE`], one lock for the whole tree, so that a flip of a child
//! and the matching change to its parent cannot be applied out of order.
//!
//! The same state is native `EMPTY` (`docs/CGROUPS.md` §5): a level a job
//! asserts while it is not populated, which [`notify`] fires registrations
//! for at the flip, so a native service manager holding the job, from
//! `job_for_cgroup`, waits for it where a Linux one polls `cgroup.events`.
//!
//! # Two kills
//!
//! [`Job::kill`] is the native `job_kill`: it ends everything and seals the
//! job, which then refuses new processes and children for good.
//! [`Job::kill_members`] is cgroupfs's `cgroup.kill`: it ends everything and
//! leaves the job usable. While either runs, a fork into the job is ended as
//! soon as it is findable ([`Job::is_dying`]), so a loop of forks cannot
//! outrun it.
//!
//! # Lock order
//!
//! A process's membership lock, then [`TREE`], then one job's `state`. Two
//! jobs' `state` are held at once in one place only,
//! [`Job::remove_named_child`], parent then child; nothing takes a child's
//! and then its parent's, since the count walk takes one at a time going up.
//! The kills take one `state` at a time and let go of it before ending
//! anything, because a process's kill wakes tasks and takes the scheduler's
//! locks. Nothing is woken under any of these.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::sync::SpinLock;

use ferrix_cgroupfs::controllers::{self, Change, Set, Standing};
use ferrix_cgroupfs::write::Limit;
use ferrix_native_abi::signals::Signals;
use ferrix_sync::Once;

use super::port::{Observer, Observers, PortError, deliver, register, trigger};
use super::process::{self, Process};
use super::quota::{self, Charge, Counter, Mark, Quota, Resource, Usage};
use crate::fallible::{self, AllocError};
use crate::sched::WaitQueue;

/// The status a process ended by a job kill reports.
///
/// 128 plus `SIGKILL`, which is what a shell prints for a killed child and
/// what `process::kill`'s own boot check uses.
pub(crate) const KILLED_STATUS: i32 = 137;

/// Why a job refused something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobError {
    /// The job has been killed, and takes no new processes or children.
    Killed,
    /// A child of that name already exists.
    Exists,
    /// No child has that name.
    Missing,
    /// It still has members or children, so it cannot be removed.
    Busy,
    /// A limit above it (`cgroup.max.depth`, `cgroup.max.descendants`)
    /// allows no further job there.
    Limited,
    /// It has been removed by `rmdir`, and takes no process.
    Removed,
    /// The no-internal-process rule forbids it (`docs/CGROUPS.md` §3.1):
    /// processes beside controllers enabled for the children.
    Internal,
    /// There was no memory for it.
    NoMemory,
}

impl From<AllocError> for JobError {
    fn from(_: AllocError) -> JobError {
        JobError::NoMemory
    }
}

/// Serialises every change to a job's counts, across the whole tree.
///
/// One lock, as Linux's `css_set_lock` is one, because a change propagates
/// upward: two processes leaving two sibling jobs at once must each find the
/// parent's count as the other left it.
static TREE: SpinLock<()> = SpinLock::new(());

/// The next job's number.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The next speculation domain's number: never zero, never reused
/// (`docs/OPAQUE-KERNEL.md` §9.2).
static NEXT_DOMAIN: AtomicU64 = AtomicU64::new(1);

/// The root of the tree every process is in.
static ROOT: Once<Arc<Job>> = Once::new();

/// The root job: cgroupfs's root, and every process's job until something
/// moves it or it was forked from one elsewhere.
///
/// Made by the first call, which bring-up makes (`main.rs`) before the first
/// program runs, so the allocation is a boot one.
pub(crate) fn root() -> &'static Arc<Job> {
    ROOT.call_once(|| {
        // FATAL-ALLOC: the root job is made once, during bring-up, before any
        // program exists to be told memory ran out.
        Job::new_tree_root().unwrap_or_else(|_| {
            crate::panic::fatal!(
                crate::panic::catalog::BOOT_OUT_OF_MEMORY,
                "no memory for the root job"
            )
        })
    })
}

/// A job.
#[derive(Debug)]
pub(crate) struct Job {
    /// The job this one is inside, kept alive by it. `None` for a root.
    ///
    /// Holding it is its purpose: a parent outlives every job beneath it, so
    /// a kill of the parent can still reach them, and a count can propagate
    /// to it.
    parent: Option<Arc<Job>>,
    /// Its number, unique for the life of the kernel: what cgroupfs names an
    /// anonymous job by (`job-<id>`) and numbers its inodes from.
    id: u64,
    /// Its name among its parent's children, or `None` for one native
    /// `job_create` made.
    name: Option<Box<str>>,
    /// Its members' counts, its children, and whether it has been killed.
    state: SpinLock<Members>,
    /// Woken when it is killed and whenever it becomes populated or empty,
    /// for a native wait on `TERMINATED` or `EMPTY`.
    waiters: WaitQueue,
    /// Woken whenever it becomes populated or empty. Shared, so that a
    /// `poll` of its `cgroup.events` can hold it for as long as it sleeps.
    events: Arc<WaitQueue>,
    /// Woken whenever its `memory.events` counts an OOM or an OOM kill, in
    /// it or beneath it (`object::oom`); shared as `events` is.
    memory_events: Arc<WaitQueue>,
    /// `memory.events`' `oom`: how many times a fault in it or beneath it
    /// found a memory limit here or beneath it full, and asked for a kill.
    ooms: AtomicU64,
    /// `memory.events`' `oom_kill`: how many processes in it or beneath it
    /// the scoped OOM kill ended.
    oom_kills: AtomicU64,
    /// `cgroup.freeze`: whether it asks that its processes, and those of
    /// every cgroup beneath it, stop where they can be stopped again by
    /// nothing a program can send.
    freeze: AtomicBool,
    /// What `cgroup.events` last said of `frozen`: frozen is asked for and
    /// every process beneath it has stopped. Kept here so that a change is
    /// noticed once and its pollers woken once.
    frozen_seen: AtomicBool,
    /// The owner, group and mode `chown` and `chmod` gave its cgroupfs
    /// directory and files, by each node's slot there. cgroupfs's, kept here
    /// because a directory there is a view made afresh at every lookup, and
    /// the job is what lasts. A node not listed has cgroupfs's defaults.
    nodes: SpinLock<Vec<(u64, NodeAttributes)>>,
    /// What it and everything beneath it may hold, and hold now
    /// (`object::quota`, `FRU_RSA.1`). `None` for the tree's root, which
    /// nothing is charged to and nothing limits.
    quota: Option<Quota>,
    /// The kernel object it is, charged to the job of whoever made it: a
    /// program's `job_create` or `mkdir` counts against its own job's object
    /// limit, not the new job's. Held for its drop, which uncharges it.
    charge: Charge,
    /// The job a refusal of its members is charged to in the audit record's
    /// fairness (`audit`, `docs/certification/AUDIT.md` §3), fixed when it
    /// is made: its own id, or its parent's budget when its maker's own
    /// authority could have made it ([`Budget`]).
    audit_budget: u64,
    /// The speculation domain it is, or zero for none
    /// (`docs/OPAQUE-KERNEL.md` §9.2): set only as it is made, by
    /// [`Job::new_child_domain`], and never changed.
    domain: u64,
}

/// Whose audit budget a new job's refusals are charged to: decided by
/// whoever makes it, since whether a directory is writable by someone other
/// than root is cgroupfs's to know, not the core's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Budget {
    /// Its own: a job only root could have made, such as a unit's cgroup
    /// init made in a directory only root may write.
    Own,
    /// Its parent's: a job its members' own authority could have made -- an
    /// anonymous one `job_create` made, or a named one in a directory
    /// someone other than root may write, a delegatee's `mkdir`.
    Parents,
}

/// Who owns one node of a job's cgroupfs directory, and its mode: what
/// delegation by `chown` changes (`docs/CGROUPS.md` §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NodeAttributes {
    /// The owner.
    pub(crate) uid: u32,
    /// The group.
    pub(crate) gid: u32,
    /// The permission bits.
    pub(crate) permissions: u32,
}

/// What a job holds.
#[derive(Debug)]
struct Members {
    /// Set once, by the first [`Job::kill`] reaching this job, and never
    /// cleared.
    killed: bool,
    /// How many [`Job::kill_members`] are running over it.
    killing: u32,
    /// The jobs directly inside this one that have no name.
    children: Vec<Weak<Job>>,
    /// The jobs directly inside this one that have a name, held until the
    /// name is removed.
    named: Vec<Arc<Job>>,
    /// Its own members not yet released. Changed only under [`TREE`].
    live: usize,
    /// Its children that are populated. Changed only under [`TREE`].
    busy: usize,
    /// Port registrations waiting for it to be killed or to become empty.
    observers: Observers,
    /// `cgroup.max.depth`: how many levels of jobs may be made beneath it.
    max_depth: Limit,
    /// `cgroup.max.descendants`: how many jobs may be beneath it at once.
    max_descendants: Limit,
    /// Set by `rmdir` ([`Job::remove_named_child`]): a removed job takes no
    /// process, so none can be moved into a directory that is gone.
    removed: bool,
    /// `cgroup.subtree_control`: the controllers it enables for its children.
    subtree_control: Set,
}

impl Default for Members {
    fn default() -> Members {
        Members {
            killed: false,
            killing: 0,
            children: Vec::new(),
            named: Vec::new(),
            live: 0,
            busy: 0,
            observers: Observers::new(),
            max_depth: Limit::Max,
            max_descendants: Limit::Max,
            removed: false,
            subtree_control: Set::EMPTY,
        }
    }
}

impl Members {
    /// Whether it, or anything beneath it, has a member not yet released.
    fn populated(&self) -> bool {
        self.live != 0 || self.busy != 0
    }

    /// Its native signals: `TERMINATED` once killed, `EMPTY` while not
    /// populated.
    fn signals(&self) -> Signals {
        let mut signals = Signals::NONE;
        if self.killed {
            signals = signals | Signals::TERMINATED;
        }
        if !self.populated() {
            signals = signals | Signals::EMPTY;
        }
        signals
    }

    /// Where it stands for the no-internal-process rule, a root or not.
    fn standing(&self, root: bool) -> Standing {
        Standing {
            root,
            has_tasks: self.live != 0,
            populated_children: self.busy != 0,
            subtree_control: self.subtree_control,
        }
    }

    /// Whether a process may arrive in it, by a move or `CLONE_INTO_CGROUP`.
    fn admits(&self, root: bool) -> Result<(), JobError> {
        if self.removed {
            return Err(JobError::Removed);
        }
        controllers::vet_destination(self.standing(root)).map_err(|_| JobError::Internal)
    }
}

/// Jobs whose populated state a count change flipped, to be told once every
/// lock is let go ([`notify`]).
///
/// A count change flips a run of jobs going up from the one it started at,
/// so a run is its first job and how many it covers, and nothing has to be
/// allocated to remember it: this is filled as a process leaves its job,
/// where nobody can be told memory ran out. A change is at most a move, one
/// run out of the old job and one into the new, so two runs are enough.
#[derive(Debug, Default)]
pub(crate) struct Flipped {
    /// Each run: its lowest job, and how many jobs from there up flipped.
    runs: [Option<(Arc<Job>, usize)>; 2],
}

impl Flipped {
    /// Nothing flipped yet.
    pub(crate) fn new() -> Flipped {
        Flipped::default()
    }

    /// Record that `job` flipped, as the next job up of the run `starting`
    /// began, or as the start of a new run.
    fn add(&mut self, job: &Arc<Job>, starting: bool) {
        if !starting
            && let Some(Some((_, flipped))) = self.runs.iter_mut().rev().find(|run| run.is_some())
        {
            *flipped += 1;
            return;
        }
        if let Some(free) = self.runs.iter_mut().find(|run| run.is_none()) {
            *free = Some((Arc::clone(job), 1));
        }
    }
}

/// Wake whatever waits on each job a count change flipped: a poll of its
/// `cgroup.events`, a native wait on its handle, and the port registrations
/// waiting for [`Signals::EMPTY`], which fire if the job is empty now.
///
/// "Now", under its lock, and not "when it flipped": a job that filled again
/// between the flip and here no longer asserts `EMPTY`, and a registration
/// is for a level, so it waits on. One made in between found the job empty
/// and fired as it was made ([`Job::observe`]), so none is lost either way.
pub(crate) fn notify(flipped: Flipped) {
    for (lowest, count) in flipped.runs.into_iter().flatten() {
        let mut at = Some(&lowest);
        for _ in 0..count {
            let Some(job) = at else { break };
            let emptied = {
                let mut members = job.state.lock();
                !members.populated() && trigger(&mut members.observers, Signals::EMPTY)
            };
            if emptied {
                deliver(|| job.state.lock().observers.next_fired());
            }
            job.events.wake_all();
            job.waiters.wake_all();
            at = job.parent.as_ref();
        }
    }
}

impl Job {
    /// A job with no parent.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn new_root() -> Result<Arc<Job>, AllocError> {
        let quota = Quota::new(None)?;
        fallible::try_arc(Job::bare(None, None, Some(quota), Budget::Own)?)
    }

    /// The tree's root: a job with no parent and no quota, since nothing is
    /// charged at the top of the tree every process is in.
    fn new_tree_root() -> Result<Arc<Job>, AllocError> {
        fallible::try_arc(Job::bare(None, None, None, Budget::Own)?)
    }

    /// A job inside `parent`, or none, not yet listed anywhere, whose audit
    /// budget is its own or its parent's as `budget` says.
    fn bare(
        parent: Option<Arc<Job>>,
        name: Option<Box<str>>,
        quota: Option<Quota>,
        budget: Budget,
    ) -> Result<Job, AllocError> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let audit_budget = match (&parent, budget) {
            (Some(parent), Budget::Parents) => parent.audit_budget,
            _ => id,
        };
        if let Some(quota) = &quota {
            quota.identify(id, audit_budget);
        }
        Ok(Job {
            parent,
            id,
            name,
            state: SpinLock::new(Members::default()),
            waiters: WaitQueue::new(),
            events: fallible::try_arc(WaitQueue::new())?,
            memory_events: fallible::try_arc(WaitQueue::new())?,
            ooms: AtomicU64::new(0),
            oom_kills: AtomicU64::new(0),
            freeze: AtomicBool::new(false),
            frozen_seen: AtomicBool::new(false),
            nodes: SpinLock::new(Vec::new()),
            quota,
            charge: Charge::none(Resource::Objects),
            audit_budget,
            domain: 0,
        })
    }

    /// A job inside this one, with a quota inside this one's, that is
    /// speculation domain `domain`, zero for none.
    fn bare_child(
        self: &Arc<Job>,
        name: Option<Box<str>>,
        budget: Budget,
        domain: u64,
    ) -> Result<Arc<Job>, AllocError> {
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| AllocError)?;
        let quota = Quota::new(self.quota.as_ref())?;
        let mut child = Job::bare(Some(Arc::clone(self)), name, Some(quota), budget)?;
        child.charge = charge;
        child.domain = domain;
        fallible::try_arc(child)
    }

    /// A new anonymous job inside this one, charged to this one's audit
    /// budget: anyone holding a job handle that allows it can make one.
    ///
    /// # Errors
    ///
    /// [`JobError::Killed`], [`JobError::NoMemory`].
    pub(crate) fn new_child(self: &Arc<Job>) -> Result<Arc<Job>, JobError> {
        self.new_child_in(0)
    }

    /// [`Job::new_child`], made a speculation domain of its own: the switch
    /// barrier between two of its processes born in it is skipped
    /// (`docs/OPAQUE-KERNEL.md` §9.2). The caller has checked the authority,
    /// MANAGE on this job, and writes the audit record.
    ///
    /// # Errors
    ///
    /// As [`Job::new_child`].
    pub(crate) fn new_child_domain(self: &Arc<Job>) -> Result<Arc<Job>, JobError> {
        self.new_child_in(NEXT_DOMAIN.fetch_add(1, Ordering::Relaxed))
    }

    /// The speculation domain it is: zero for none.
    pub(crate) fn domain(&self) -> u64 {
        self.domain
    }

    /// [`Job::new_child`] as speculation domain `domain`, zero for none.
    fn new_child_in(self: &Arc<Job>, domain: u64) -> Result<Arc<Job>, JobError> {
        let child = self.bare_child(None, Budget::Parents, domain)?;
        let mut members = self.state.lock();
        if members.killed {
            return Err(JobError::Killed);
        }
        // Pruned as it grows, so a job that makes and drops children in a
        // loop does not keep a list of every one it ever had.
        members.children.retain(|child| child.strong_count() > 0);
        fallible::try_push(&mut members.children, Arc::downgrade(&child))?;
        Ok(child)
    }

    /// A new job inside this one called `name`, which this one holds until
    /// the name is removed, with the audit budget its maker says.
    ///
    /// # Errors
    ///
    /// [`JobError::Killed`], [`JobError::Removed`] if `rmdir` took this one,
    /// [`JobError::Exists`] if a named child already has that name, or
    /// [`JobError::Limited`] if a limit at or above it allows no further job,
    /// or [`JobError::NoMemory`].
    pub(crate) fn new_named_child(
        self: &Arc<Job>,
        name: &str,
        budget: Budget,
    ) -> Result<Arc<Job>, JobError> {
        self.room_for_a_child()?;
        let name_held = fallible::try_boxed_str(name)?;
        let child = self.bare_child(Some(name_held), budget, 0)?;
        let mut members = self.state.lock();
        if members.killed {
            return Err(JobError::Killed);
        }
        if members.removed {
            return Err(JobError::Removed);
        }
        if members.named.iter().any(|child| child.name() == Some(name)) {
            return Err(JobError::Exists);
        }
        fallible::try_push(&mut members.named, Arc::clone(&child))?;
        Ok(child)
    }

    /// Its quota slot, or [`quota::NONE`] for the tree's root: what a charge
    /// made on its behalf names.
    pub(crate) fn quota_index(&self) -> u32 {
        self.quota.as_ref().map_or(quota::NONE, Quota::index)
    }

    /// What it holds of `resource`, its limit and its refusals; `None` for
    /// the tree's root, which is charged nothing.
    pub(crate) fn usage(&self, resource: Resource) -> Option<Usage> {
        self.quota.as_ref().map(|quota| quota.usage(resource))
    }

    /// Limit what it and everything beneath it may hold of `resource`.
    /// Whether it could be: the tree's root takes no limit.
    pub(crate) fn set_limit(&self, resource: Resource, limit: u64) -> bool {
        self.quota
            .as_ref()
            .map(|quota| quota.set_limit(resource, limit))
            .is_some()
    }

    /// Its `memory.high`, `memory.low` or `memory.min` mark; a `max` high and
    /// a zero low and min for the tree's root, which has none.
    pub(crate) fn mark(&self, mark: Mark) -> u64 {
        self.quota.as_ref().map_or(
            if mark == Mark::High {
                quota::UNLIMITED
            } else {
                0
            },
            |quota| quota.mark(mark),
        )
    }

    /// Set a mark. Whether it could be: the tree's root takes none.
    pub(crate) fn set_mark(&self, mark: Mark, value: u64) -> bool {
        self.quota
            .as_ref()
            .map(|quota| quota.set_mark(mark, value))
            .is_some()
    }

    /// How many of `counter` were counted in it and beneath it.
    pub(crate) fn counted(&self, counter: Counter) -> u64 {
        self.quota
            .as_ref()
            .map_or(0, |quota| quota.counted(counter))
    }

    /// Wake whatever polls its `memory.events` and every job above it: a
    /// count in it changed (`memory.high`'s).
    pub(crate) fn wake_memory_events(&self) {
        let mut at = Some(self);
        while let Some(job) = at {
            job.memory_events.wake_all();
            at = job.parent.as_deref();
        }
    }

    /// Its `cpu.max`: the quota in nanoseconds, or [`quota::UNLIMITED`], and
    /// the period in nanoseconds.
    pub(crate) fn bandwidth(&self) -> (u64, u64) {
        self.quota.as_ref().map_or(
            (quota::UNLIMITED, quota::DEFAULT_PERIOD_NS),
            Quota::bandwidth,
        )
    }

    /// Set its `cpu.max`. Whether it could be: the tree's root takes none.
    pub(crate) fn set_bandwidth(&self, quota: u64, period: u64) -> bool {
        self.quota
            .as_ref()
            .map(|slot| slot.set_bandwidth(quota, period))
            .is_some()
    }

    /// Nanoseconds its tasks, and those of the jobs beneath it, have used
    /// the processor in user mode and in kernel mode; the whole machine's for
    /// the tree's root.
    pub(crate) fn cpu_times(&self) -> (u64, u64) {
        self.quota
            .as_ref()
            .map_or_else(quota::machine_cpu, Quota::cpu_times)
    }

    /// Every job in this one's subtree, itself first.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the list cannot be made.
    pub(crate) fn subtree(self: &Arc<Job>) -> Result<Vec<Arc<Job>>, AllocError> {
        self.walk(|_| {})
    }

    /// Whether `cgroup.freeze` is set here: its own, not an ancestor's.
    pub(crate) fn freeze_requested(&self) -> bool {
        self.freeze.load(Ordering::Acquire)
    }

    /// Set `cgroup.freeze` here. The processes beneath it are to be told by
    /// the caller ([`Process::sync_freeze`]), after this.
    pub(crate) fn set_freeze(&self, on: bool) {
        self.freeze.store(on, Ordering::SeqCst);
    }

    /// Whether it, or a job above it, asks for its processes to be frozen.
    pub(crate) fn freezing(&self) -> bool {
        let mut at = Some(self);
        while let Some(job) = at {
            if job.freeze.load(Ordering::SeqCst) {
                return true;
            }
            at = job.parent.as_deref();
        }
        false
    }

    /// What `cgroup.events` last said of `frozen`.
    pub(crate) fn frozen_seen(&self) -> bool {
        self.frozen_seen.load(Ordering::Acquire)
    }

    /// Record that `frozen` is now `now`, and, if that changed what
    /// `cgroup.events` says, wake whatever polls it. Whether it changed.
    pub(crate) fn note_frozen(&self, now: bool) -> bool {
        let changed = self.frozen_seen.swap(now, Ordering::AcqRel) != now;
        if changed {
            self.events.wake_all();
            self.waiters.wake_all();
        }
        changed
    }

    /// Its `cpu.weight`: [`quota::DEFAULT_WEIGHT`] for the tree's root.
    pub(crate) fn cpu_weight(&self) -> u32 {
        self.quota
            .as_ref()
            .map_or(quota::DEFAULT_WEIGHT, Quota::weight)
    }

    /// Set its `cpu.weight`. Whether it could be: the tree's root has none.
    pub(crate) fn set_cpu_weight(&self, weight: u32) -> bool {
        self.quota
            .as_ref()
            .map(|quota| quota.set_weight(weight))
            .is_some()
    }

    /// Its name, if it has one.
    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Its number.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Whether it is a root: the tree's, or one a boot check made alone.
    pub(crate) fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    /// The job it is inside, unless it is a root.
    pub(crate) fn parent(&self) -> Option<&Arc<Job>> {
        self.parent.as_ref()
    }

    /// The job its members' refusals are charged to in the audit record.
    pub(crate) fn audit_budget(&self) -> u64 {
        self.audit_budget
    }

    /// Whether `rmdir` has taken it out of its parent.
    pub(crate) fn is_removed(&self) -> bool {
        self.state.lock().removed
    }

    /// Whether a process may be moved into it now, by `cgroup.procs` or
    /// `CLONE_INTO_CGROUP`: it has not been removed, and the
    /// no-internal-process rule allows it. A move that counts the process in
    /// asks again under the lock it counts under ([`Job::count_in_checked`]).
    ///
    /// # Errors
    ///
    /// [`JobError::Removed`] or [`JobError::Internal`].
    pub(crate) fn admits(&self) -> Result<(), JobError> {
        self.state.lock().admits(self.parent.is_none())
    }

    /// Its `cgroup.subtree_control`.
    pub(crate) fn subtree_control(&self) -> Set {
        self.state.lock().subtree_control
    }

    /// Apply a write to its `cgroup.subtree_control`, as Linux's
    /// `cgroup_subtree_control_write` does: a controller already on is not
    /// enabled again, nor one already off disabled; each newly enabled must
    /// be `offered` (its `cgroup.controllers`); none may be disabled that a
    /// child still enables; and what is enabled must pass the
    /// no-internal-process rule, which is decided under the lock a move
    /// counts a process in under.
    ///
    /// # Errors
    ///
    /// [`JobError::Missing`] for a controller not offered, [`JobError::Busy`]
    /// for one a child still enables, [`JobError::Internal`] for the rule,
    /// [`JobError::NoMemory`].
    pub(crate) fn change_subtree_control(
        &self,
        change: Change,
        offered: Set,
    ) -> Result<(), JobError> {
        let current = self.subtree_control();
        let enable = change.enable.minus(current);
        let disable = change.disable.intersect(current);
        if !enable.is_subset(offered) {
            return Err(JobError::Missing);
        }
        if self
            .children()?
            .iter()
            .any(|child| !child.subtree_control().intersect(disable).is_empty())
        {
            return Err(JobError::Busy);
        }
        let mut members = self.state.lock();
        controllers::vet_enable(enable, members.standing(self.parent.is_none()))
            .map_err(|_| JobError::Internal)?;
        members.subtree_control = members.subtree_control.union(enable).minus(disable);
        Ok(())
    }

    /// The owner, group and mode `chown` or `chmod` gave the node at `slot`
    /// of its cgroupfs directory, if either did.
    pub(crate) fn node(&self, slot: u64) -> Option<NodeAttributes> {
        self.nodes
            .lock()
            .iter()
            .find(|(at, _)| *at == slot)
            .map(|(_, attributes)| *attributes)
    }

    /// Record the owner, group and mode of the node at `slot`.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when a node not yet listed could not be.
    pub(crate) fn set_node(&self, slot: u64, attributes: NodeAttributes) -> Result<(), AllocError> {
        let mut nodes = self.nodes.lock();
        match nodes.iter_mut().find(|(at, _)| *at == slot) {
            Some((_, held)) => *held = attributes,
            None => fallible::try_push(&mut nodes, (slot, attributes))?,
        }
        Ok(())
    }

    /// The jobs directly inside it that still exist, named ones first in the
    /// order they were made, then anonymous ones in theirs.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn children(&self) -> Result<Vec<Arc<Job>>, AllocError> {
        let members = self.state.lock();
        fallible::try_collect(
            members
                .named
                .iter()
                .cloned()
                .chain(members.children.iter().filter_map(Weak::upgrade)),
        )
    }

    /// Take the named child `name` out of it, as `rmdir` does: only when
    /// nothing is in the child, neither a member nor a job.
    ///
    /// The child's lock is taken while this one's is held. That is the one
    /// place two job locks are held at once, and it is safe because nothing
    /// takes a child's lock and then its parent's: the count walk takes one at
    /// a time going up.
    ///
    /// # Errors
    ///
    /// [`JobError::Missing`] when it has no such named child, and
    /// [`JobError::Busy`] when that child is populated or has children.
    pub(crate) fn remove_named_child(&self, name: &str) -> Result<Arc<Job>, JobError> {
        let mut members = self.state.lock();
        let at = members
            .named
            .iter()
            .position(|child| child.name() == Some(name))
            .ok_or(JobError::Missing)?;
        let child = members.named.get(at).cloned().ok_or(JobError::Missing)?;
        {
            let mut inner = child.state.lock();
            let busy = inner.populated()
                || !inner.named.is_empty()
                || inner.children.iter().any(|child| child.strong_count() > 0);
            if busy {
                return Err(JobError::Busy);
            }
            // Under the child's lock, where a move counts a process in, so a
            // move either lands first, and the child is busy, or finds it
            // removed.
            inner.removed = true;
        }
        let _ = members.named.remove(at);
        Ok(child)
    }

    /// How many jobs are beneath it.
    ///
    /// # Errors
    ///
    /// [`AllocError`]: counting walks the tree.
    pub(crate) fn descendants(self: &Arc<Job>) -> Result<u32, AllocError> {
        let count = self.walk(|_| {})?.len().saturating_sub(1);
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// Its `cgroup.max.depth` and `cgroup.max.descendants`.
    pub(crate) fn limits(&self) -> (Limit, Limit) {
        let members = self.state.lock();
        (members.max_depth, members.max_descendants)
    }

    /// Set its `cgroup.max.depth`.
    pub(crate) fn set_max_depth(&self, limit: Limit) {
        self.state.lock().max_depth = limit;
    }

    /// Set its `cgroup.max.descendants`.
    pub(crate) fn set_max_descendants(&self, limit: Limit) {
        self.state.lock().max_descendants = limit;
    }

    /// Whether a new child may be made in it, as Linux's
    /// `cgroup_check_hierarchy_limits` decides: no job at or above it may
    /// already hold as many descendants as its `cgroup.max.descendants`
    /// allows, nor be more levels above the new child than its
    /// `cgroup.max.depth` allows.
    fn room_for_a_child(self: &Arc<Job>) -> Result<(), JobError> {
        let mut level: u32 = 1;
        let mut at = Some(Arc::clone(self));
        while let Some(job) = at {
            let (depth, descendants) = job.limits();
            if !descendants.allows(job.descendants()?.saturating_add(1)) || !depth.allows(level) {
                return Err(JobError::Limited);
            }
            level = level.saturating_add(1);
            at = job.parent.clone();
        }
        Ok(())
    }

    /// The names from the root's child down to it, an anonymous job named
    /// `job-<id>`: what cgroupfs and `/proc/<pid>/cgroup` build its path
    /// from. Empty for a root.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn path_names(&self) -> Result<Vec<String>, AllocError> {
        let mut names = Vec::new();
        let mut at = Some(self);
        while let Some(job) = at {
            if job.parent.is_some() {
                fallible::try_push(&mut names, job.display_name()?)?;
            }
            at = job.parent.as_deref();
        }
        names.reverse();
        Ok(names)
    }

    /// The name cgroupfs shows it by in its parent's directory.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn display_name(&self) -> Result<String, AllocError> {
        match &self.name {
            Some(name) => fallible::try_string(name),
            None => fallible::try_format(format_args!("job-{}", self.id)),
        }
    }

    /// Whether it, or a job beneath it, has a member that has not been
    /// released.
    pub(crate) fn is_populated(&self) -> bool {
        self.state.lock().populated()
    }

    /// How many of its own members have not been released.
    pub(crate) fn live(&self) -> usize {
        self.state.lock().live
    }

    /// The queue woken whenever it becomes populated or empty.
    pub(crate) fn events(&self) -> &Arc<WaitQueue> {
        &self.events
    }

    /// The queue woken whenever its `memory.events` counts change.
    pub(crate) fn memory_events(&self) -> &Arc<WaitQueue> {
        &self.memory_events
    }

    /// `memory.events`' `oom` and `oom_kill`, its own and every job's
    /// beneath it, as cgroup v2 counts them.
    pub(crate) fn oom_counts(&self) -> (u64, u64) {
        (
            self.ooms.load(Ordering::Acquire),
            self.oom_kills.load(Ordering::Acquire),
        )
    }

    /// Count an OOM (`killed` false) or an OOM kill in this job, and in
    /// every job above it, as Linux's `memcg_memory_event` counts up the
    /// tree, and wake whatever polls their `memory.events`. Atomics and
    /// wakes only: the caller holds no lock.
    pub(crate) fn count_oom(&self, killed: bool) {
        let mut at = Some(self);
        while let Some(job) = at {
            let count = if killed { &job.oom_kills } else { &job.ooms };
            let _ = count.fetch_add(1, Ordering::AcqRel);
            job.memory_events.wake_all();
            at = job.parent.as_deref();
        }
    }

    /// Whether `job` is this job or beneath it.
    pub(crate) fn contains(&self, job: &Job) -> bool {
        let mut at = Some(job);
        while let Some(current) = at {
            if core::ptr::eq(current, self) {
                return true;
            }
            at = current.parent.as_deref();
        }
        false
    }

    /// Whether a process joining it now would be ended: it or a job above it
    /// has been killed, or is having its members killed.
    pub(crate) fn is_dying(&self) -> bool {
        let mut at = Some(self);
        while let Some(current) = at {
            let members = current.state.lock();
            if members.killed || members.killing != 0 {
                return true;
            }
            drop(members);
            at = current.parent.as_deref();
        }
        false
    }

    /// Count one more member of its own, which a process being made or moved
    /// in is. The jobs whose populated state that flipped go in `flipped`.
    ///
    /// Called with the member's membership lock held, or for a process no
    /// one else can reach yet.
    pub(crate) fn count_in(self: &Arc<Job>, flipped: &mut Flipped) {
        let _ = self.count(true, false, flipped);
    }

    /// [`Job::count_in`] for a process moved in, which the job may refuse:
    /// it asks [`Job::admits`]'s question under the lock it counts under, so
    /// an `rmdir` or a `cgroup.subtree_control` write cannot slip between the
    /// answer and the count. Nothing is counted when it refuses.
    ///
    /// # Errors
    ///
    /// As [`Job::admits`].
    pub(crate) fn count_in_checked(self: &Arc<Job>, flipped: &mut Flipped) -> Result<(), JobError> {
        self.count(true, true, flipped)
    }

    /// Count one member of its own fewer: a process released or moved out.
    pub(crate) fn count_out(self: &Arc<Job>, flipped: &mut Flipped) {
        let _ = self.count(false, false, flipped);
    }

    /// Change `live` by one, and every ancestor's `busy` for as long as the
    /// job below it flipped. With `checked`, an arrival the job does not
    /// admit is refused before anything changes.
    fn count(
        self: &Arc<Job>,
        arriving: bool,
        checked: bool,
        flipped: &mut Flipped,
    ) -> Result<(), JobError> {
        let _tree = TREE.lock();
        let mut job = Arc::clone(self);
        let mut own = true;
        loop {
            let changed = {
                let mut members = job.state.lock();
                if own && checked {
                    members.admits(job.parent.is_none())?;
                }
                let before = members.populated();
                let count = if own {
                    &mut members.live
                } else {
                    &mut members.busy
                };
                // Never below zero: a count that would go there is a bug the
                // boot checks look for, not a reason to wrap.
                *count = if arriving {
                    count.saturating_add(1)
                } else {
                    count.saturating_sub(1)
                };
                before != members.populated()
            };
            if !changed {
                break;
            }
            flipped.add(&job, own);
            let Some(parent) = job.parent.clone() else {
                break;
            };
            job = parent;
            own = false;
        }
        Ok(())
    }

    /// Put `process` in this job, taking it out of the one it is in.
    ///
    /// # Errors
    ///
    /// [`JobError::Killed`] if this job, or one above it, has been killed, in
    /// which case the caller should not start it.
    pub(crate) fn adopt(self: &Arc<Job>, process: &Process) -> Result<(), JobError> {
        process.move_to(self)
    }

    /// Whether it has been killed, and so takes nothing new.
    pub(crate) fn refuses(&self) -> bool {
        let mut at = Some(self);
        while let Some(current) = at {
            if current.state.lock().killed {
                return true;
            }
            at = current.parent.as_deref();
        }
        false
    }

    /// Queue a packet with `observer` when this job is killed or becomes
    /// empty, whichever it waits for, or at once if that is so already.
    ///
    /// Decided under the lock every count change is made under, so a
    /// registration either finds the job empty and fires here, or is listed
    /// before the last member leaves and is fired by [`notify`].
    ///
    /// # Errors
    ///
    /// [`PortError::Full`] when the job already holds
    /// [`super::port::MAX_OBSERVERS`] registrations; [`PortError::NoMemory`].
    pub(crate) fn observe(&self, observer: Observer) -> Result<(), PortError> {
        let mut members = self.state.lock();
        let asserted = members.signals();
        if observer.wants(asserted) {
            drop(members);
            observer.fire(asserted);
            return Ok(());
        }
        register(&mut members.observers, observer)
    }

    /// What a native waiter on it sees: [`Signals::TERMINATED`] once it has
    /// been killed, and [`Signals::EMPTY`] while it is not populated.
    pub(crate) fn signals(&self) -> Signals {
        self.state.lock().signals()
    }

    /// Whether it has been killed.
    pub(crate) fn is_killed(&self) -> bool {
        self.state.lock().killed
    }

    /// The queue woken when it is killed, becomes empty or fills.
    pub(crate) fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    /// This job and every job beneath it, parents before children.
    ///
    /// Walked with a list rather than recursion, for the reason
    /// `object::dispose` drops that way: a job tree is as deep as a program
    /// made it. Each job's lock is taken alone, and `visit` runs under it.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the list could not grow. `visit` has run on some
    /// of the jobs by then, so a caller whose `visit` changes something
    /// walks once without changing anything first.
    fn walk(
        self: &Arc<Job>,
        mut visit: impl FnMut(&mut Members),
    ) -> Result<Vec<Arc<Job>>, AllocError> {
        let mut pending = Vec::new();
        fallible::try_push(&mut pending, Arc::clone(self))?;
        let mut seen = Vec::new();
        while let Some(job) = pending.pop() {
            {
                let mut members = job.state.lock();
                visit(&mut members);
                fallible::try_extend(
                    &mut pending,
                    members.children.iter().filter_map(Weak::upgrade),
                )?;
                fallible::try_extend(&mut pending, members.named.iter().cloned())?;
            }
            fallible::try_push(&mut seen, job)?;
        }
        Ok(seen)
    }

    /// End every live process in this job or beneath it with `status`, found
    /// in the pid table. Returns how many had not already ended.
    ///
    /// A process numbered 0 -- made when every pid was in use -- is not in
    /// the registry and is not found. `fork` refuses to make one, and nothing
    /// else can put one in a job but a boot check.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the pid table could not be listed; nothing has
    /// been ended.
    fn end_members(&self, status: i32) -> Result<usize, AllocError> {
        let mut ended = 0;
        for member in process::live()? {
            if !self.contains(&member.core().job()) {
                continue;
            }
            if !member.core().is_terminated() {
                ended += 1;
            }
            member.kill(status);
        }
        Ok(ended)
    }

    /// End every process in this job and in every job beneath it with
    /// `status`, and refuse anything added to any of them afterwards.
    ///
    /// Every job is marked first, each under its own lock, and the processes
    /// are found afterwards. A fork into one of them is either findable by
    /// then, or finds the mark once it is ([`Job::is_dying`]) and is ended by
    /// the fork itself. Its registrations fire and its waiters wake once its
    /// processes have been told.
    ///
    /// Returns how many of the processes it found had not already ended.
    ///
    /// # Errors
    ///
    /// [`AllocError`], in two places. Listing the jobs comes first, and a
    /// failure there changes nothing. Listing the processes comes after the
    /// jobs are marked, because that order is what keeps a fork from slipping
    /// between the two; a failure there leaves the jobs killed -- refusing
    /// every new member, their registrations fired -- and their processes
    /// running, and a second kill ends them. A job made beneath one of them
    /// between the listing and the marking is not marked itself, but refuses
    /// new members all the same, through the killed job above it.
    pub(crate) fn kill(self: &Arc<Job>, status: i32) -> Result<usize, AllocError> {
        let jobs = self.walk(|_| {})?;
        for job in &jobs {
            let mut members = job.state.lock();
            members.killed = true;
            // Only those waiting for the kill: one waiting for `EMPTY` alone
            // waits on for the members to end.
            let _ = trigger(&mut members.observers, Signals::TERMINATED);
        }
        for job in &jobs {
            deliver(|| job.state.lock().observers.next_fired());
        }
        let ended = self.end_members(status);
        for job in jobs {
            job.waiters.wake_all();
        }
        ended
    }

    /// End every process in this job and beneath it, as `cgroup.kill` does,
    /// and leave every job as usable as it was.
    ///
    /// Returns how many of the processes it found had not already ended.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the jobs or the processes could not be listed;
    /// nothing has been ended, and every job is as it was.
    pub(crate) fn kill_members(self: &Arc<Job>) -> Result<usize, AllocError> {
        let jobs = self.walk(|_| {})?;
        for job in &jobs {
            job.state.lock().killing += 1;
        }
        let ended = self.end_members(KILLED_STATUS);
        for job in jobs {
            let mut members = job.state.lock();
            members.killing = members.killing.saturating_sub(1);
        }
        ended
    }
}

impl Drop for Job {
    /// Let go of the chain of parents in a loop, not by recursion.
    ///
    /// A child holds its parent strongly, so dropping the last reference to
    /// the deepest job of a chain would drop each ancestor inside the drop of
    /// the one below it, a stack frame per job, to whatever depth a program
    /// built. A loop of `job_create` then `handle_close` builds a chain of
    /// hundreds of thousands with one handle open at a time. So each parent
    /// this job was the last holder of is taken apart here: its own parent is
    /// taken out first, and dropping it then has nothing above it to recurse
    /// into.
    fn drop(&mut self) {
        let mut next = self.parent.take();
        while let Some(parent) = next {
            next = match Arc::try_unwrap(parent) {
                Ok(mut owned) => owned.parent.take(),
                // Someone else still holds it, so this reference frees nothing.
                Err(_shared) => None,
            };
        }
    }
}
