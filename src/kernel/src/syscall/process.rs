//! The per-process state a system call reads or changes.
//!
//! Everything here is state that belongs to a *program*, not to a thread and
//! not to the kernel: where its heap ends, what it has asked to happen on each
//! signal, and the address it wants cleared when it dies. A [`Process`] owns
//! an [`AddressSpace`] and the handlers take `&Process`, so a handler never
//! has to reach for an ambient "current" anything.
//!
//! # Why the handlers take this explicitly
//!
//! Because it is the only way to test them before user mode exists. The boot
//! self-check builds a real `Process` over a real `AddressSpace` and calls the
//! handlers directly, so `mmap` is exercised against the actual VMA tree and
//! the actual page tables on all three architectures — months before a program
//! can call it. A handler that read a global "current process" instead could
//! not be reached at all until the privilege transition landed, and would then
//! be tested for the first time in the same commit as the transition.
//!
//! The one place that *does* need an ambient answer is [`super::dispatch`],
//! which has to find the caller's process from the running task. That is
//! [`current`], one function, which asks the scheduler for the running task and
//! the task for its process.
//!
//! # The core's half and this one
//!
//! A [`Process`] here is the POSIX process, and it contains the core's
//! ([`crate::object::process::Process`]): the address space, the pid, the
//! handle table, the job and how it ended. Everything else -- descriptors,
//! root and working directory, signal state, `brk`, credentials, parent and
//! children, the threads and how the process ends -- is kept here, beside it,
//! and the core never sees it. Where the core has to hold a process as a
//! whole, it holds this one as an [`object::process::Host`], which is how a
//! job's kill and a native handle's drop reach [`kill`] without naming this
//! module.
//!
//! # A process is a task's, not the other way round
//!
//! A program runs as a scheduled task of its own ([`start`]), and the task
//! holds the [`Arc`] that keeps its process alive. The process keeps only weak
//! references back, which is enough to find its tasks when something outside
//! ends it ([`kill`]). Ending has two moments. The request, after which
//! [`Process::is_terminated`] is true and its threads leave; and the release,
//! once the last of them has, after which [`Process::is_released`] is true and
//! [`Process::wait_for_exit`] returns. `exit_group`, a last thread's `exit` and
//! `kill` all make the same request, and the first one to get there decides the
//! status.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use crate::fallible::{self, AllocError};
use crate::sync::SpinLock;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::signals::Signals as ObjectSignals;
use ferrix_sync::{SleepLock, SleepLockGuard};
use ferrix_vfs::fd::FdTable;
use ferrix_vfs::{Context, Location, OpenFile};
use ferrix_vma::VmaFlags;

use crate::fs;
use crate::fs::console::Reading;
use crate::object::job::{self, Job};
use crate::object::process::Host;
use crate::object::{self, HandleTable};
use crate::sched::{self, Task, WaitQueue};
use crate::syscall::credentials::Credentials;
use crate::syscall::fd;
use crate::syscall::nsproxy::NsProxy;
use crate::syscall::pidns::{self, Numbers, PidNamespace};
use crate::syscall::registry;
use crate::syscall::signal::{Origin, Posted, Signals};
use crate::syscall::thread::{self, Thread};
use crate::syscall::{attributes, futex, kill, sem, uaccess};
use crate::user::space::{AddressSpace, MMAP_MIN_ADDR, SpaceError};
use ferrix_linux_abi::types::SIGCHLD;

/// A program, as far as the system call layer is concerned.
#[derive(Debug)]
pub(crate) struct Process {
    /// What the core enforces and reports: its address space, its pid, when
    /// it was made, its handles, its job and how it ended. First, so that it
    /// is dropped first -- its job's count and its pid are given back before
    /// the descriptors below are closed, as they were before the two halves
    /// were split.
    ///
    /// Reached by [`Deref`](core::ops::Deref), because a POSIX process *is*
    /// a core process with more beside it: `process.space()` and
    /// `process.pid()` read the same here as in the core.
    core: Arc<object::process::Process>,
    /// The file mode creation mask: the permission bits a new file or
    /// directory is made without. An atomic rather than a field under the
    /// state lock, because `umask` is a swap and nothing reads it together
    /// with anything else.
    umask: AtomicU32,
    /// `/proc/<pid>/oom_score_adj`: how much sooner, from -1000 to 1000, an
    /// out-of-memory killer should pick it. There is no such killer yet, so
    /// the value is kept and reported and changes nothing; Chrome sets one
    /// for each renderer and says so in its log when it cannot.
    oom_score_adj: AtomicI32,
    /// What it was started as, which only `/proc` reads.
    identity: SpinLock<Identity>,
    /// The System V semaphore sets it holds undo records in, applied as it
    /// is released (`sem::exit`). Its own, never a parent's: a fork child
    /// owes nothing, as Linux's child without `CLONE_SYSVSEM` owes nothing.
    sem_undo: sem::UndoList,
    /// Its file descriptors, and the open file descriptions they name.
    ///
    /// A lock of its own for the reason `handles` has one, and one more: a
    /// `read` of the console waits for a person to type, so the table is only
    /// ever held for the lookup, and nothing else should have to wait behind a
    /// lookup either. See `crate::syscall::fd`.
    ///
    /// Behind an `Arc` so that `clone(CLONE_FILES)` can give a second process
    /// the same table rather than a copy of it.
    files: Arc<SpinLock<FdTable<Arc<OpenFile>>>>,
    /// Where its `/` and its working directory are.
    ///
    /// Cloned out by every call that walks a path, rather than held across
    /// the walk: a walk calls into filesystems, and a `chdir` on another thread
    /// has no reason to wait for one. Behind an `Arc` for `clone(CLONE_FS)`,
    /// as `files` is for `CLONE_FILES`.
    fs: Arc<SpinLock<Context>>,
    /// Everything else, behind one lock. One lock per process rather than a
    /// global one, for the same reason the address space has its own: two
    /// processes calling `brk` at once should contend for nothing.
    state: SpinLock<State>,
    /// Held by `brk` from its read of the heap to the unmap of a shrunk tail,
    /// and by `fork` from its copy of the address space to its copy of the
    /// heap, so that each sees the other's work whole. A lock that may sleep,
    /// because both hold it across a shootdown; see [`Process::set_break`].
    /// Taken before `state`, and never with a spin lock held.
    heap_lock: SleepLock<()>,
    /// Where its first task enters user mode. Set once, by `exec::load`.
    startup: SpinLock<Option<Startup>>,
    /// Set by the first start, so a process runs one program's task and a
    /// second start is refused rather than running a second task in it.
    start_claimed: AtomicBool,
    /// Set by whichever of `exit_group` and `kill` gets there first.
    ending: AtomicBool,
    /// Its threads that have started and not yet ended: raised before a
    /// thread's task is spawned, lowered as the task ends. When it reaches
    /// zero on a process that is ending, the process lets go of what it holds.
    live_threads: AtomicU32,
    /// The id of a thread replacing the program while other threads of it are
    /// live, or zero. While it is set every other thread leaves on its way back
    /// to user mode, and no new thread starts. See
    /// [`Process::end_other_threads`].
    exec_thread: AtomicU32,
    /// Woken whenever one of its threads has gone, for an `execve` waiting for
    /// the others to leave.
    thread_left: WaitQueue,
    /// The seccomp state its first thread is to start with, for a native child
    /// of a filtered creator: the creator's mode and chain, set between the
    /// child's load and its start, and taken by [`Thread::leader`]
    /// (`docs/SECCOMP.md` §3.3). A leaf lock, taken once.
    first_seccomp: SpinLock<Option<crate::syscall::seccomp::State>>,
    /// The id of the thread a signal sent to it was last given to -- chosen by
    /// [`Process::notify_signal`], or handed on by
    /// [`Process::hand_on_newly_blocked`] -- or zero: for the checks that such
    /// a signal reaches the thread that can take it.
    handed_to: AtomicU32,
    /// Set while the process is findable but `attributes::inherit` has not yet
    /// given it its parent's `no_new_privs` and dumpability: a fork child from
    /// its making, a native child of a creator likewise. `ptrace_may_access`
    /// reads it as not dumpable, so the window refuses and does not leak.
    attributes_pending: AtomicBool,
    /// Set by the one [`Process::release`] that runs, as it starts.
    released: AtomicBool,
    /// Set as that release finishes, after its orphans have gone on and before
    /// its parent is told: what `wait4` reaps by.
    release_finished: AtomicBool,
    /// The status its first thread left with through `exit`, which is the
    /// process's status when its last thread ends the same way.
    leader_status: AtomicI32,
    /// Its threads, each listed before it can run -- and a fork child's before
    /// the child can be found -- so that a signal sent to the process is
    /// judged against the mask of the thread that will take it. Weak, as
    /// `tasks` is.
    threads: SpinLock<Vec<Weak<Thread>>>,
    /// The process that created it, or the one it was handed to when that one
    /// ended, if that process still exists. Weak, because a parent keeps its
    /// children (until it waits for them) and not the other way round.
    parent: SpinLock<Weak<Process>>,
    /// Its process group, which job control and `kill(0, …)` address.
    pgid: AtomicU32,
    /// Its session.
    sid: AtomicU32,
    /// Its numbers in the pid namespaces below the first it is in, or `None`
    /// in the first, where its pid is all it has. Fixed when it is made
    /// (`docs/PIDNS.md` §2.2).
    pids: Option<Arc<Numbers>>,
    /// The pid namespace its children are made in, when `unshare` set one;
    /// its own otherwise.
    pid_for_children: SpinLock<Option<Arc<PidNamespace>>>,
    /// The numbers of its group and its session, kept so that both can be
    /// told in a namespace after their leaders are reaped.
    groups: SpinLock<Groups>,
    /// The children it has not yet waited for, ended or not. Strong, so an
    /// ended child stays findable -- a zombie -- until `wait4` takes it.
    children: SpinLock<Vec<Arc<Process>>>,
    /// Woken whenever one of its children ends.
    child_exited: WaitQueue,
    /// The signal its parent is told with when it ends; `SIGCHLD` for an
    /// ordinary fork, whatever `clone` asked for otherwise.
    exit_signal: AtomicU32,
    /// Set by a successful `execve`: what a `vfork` parent waits for, besides
    /// the child ending.
    execed: AtomicBool,
    /// Woken when `execed` is set or it ends.
    vfork_done: WaitQueue,
    /// Woken when a signal is sent to it: what `pause`, `rt_sigsuspend` and
    /// `rt_sigtimedwait` wait on.
    signalled: WaitQueue,
    /// Woken when a signal becomes pending for it or any of its threads, and
    /// nothing else: what a signalfd's reader, `poll` and `epoll_wait` sleep
    /// on, as Linux's `sighand->signalfd_wqh`. Shared, because a wait holds
    /// on to the queues it sleeps on; and a queue of its own, so that every
    /// wake it counts is a signal's arrival.
    signal_arrived: Arc<WaitQueue>,
    /// Woken as it is released, for a pidfd's `poll`: made by the first
    /// `pidfd_open` of it, so a process nobody holds a pidfd for has none.
    pidfd_queue: SpinLock<Option<Arc<WaitQueue>>>,
    /// The signal that stopped it, or zero while it runs.
    stopped: AtomicU32,
    /// A stop its parent has not yet been told of by `wait4`, or zero.
    stop_report: AtomicU32,
    /// Whether it continued since its parent was last told.
    continue_report: AtomicBool,
    /// Woken when it continues, or ends, which is what a stopped task waits for.
    resumed: WaitQueue,
    /// How many of its threads are parked by `cgroup.freeze`, waiting on
    /// their way back to user mode (`docs/CGROUPS.md` §11).
    parked: AtomicU32,
    /// Its user and group ids and supplementary groups. A lock of its own:
    /// `getuid` has no business waiting on a `brk`, and a `set*id` call must
    /// see and change every id it names at once.
    credentials: SpinLock<Credentials>,
    /// Its UTS, IPC, cgroup and network namespaces, named together (`docs/NAMESPACES.md`
    /// §12). A leaf lock: cloned out before anything is done with what it
    /// names.
    nsproxy: SpinLock<NsProxy>,
}

/// The numbers of a process group and a session in the namespaces below the
/// first (`None` for one led from the first, which a namespace cannot see).
#[derive(Debug, Clone, Default)]
struct Groups {
    /// The group's.
    pgrp: Option<Arc<Numbers>>,
    /// The session's.
    session: Option<Arc<Numbers>>,
}

/// Where a program starts: the two numbers `exec::load` computes and the task
/// that runs it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Startup {
    /// Its first instruction.
    pub(crate) entry: u64,
    /// Its initial stack pointer, with the startup image above it.
    pub(crate) stack: u64,
    /// What its first argument register holds on entry: zero for a Linux
    /// program, whose startup image is on its stack, and a native process's
    /// bootstrap handle.
    pub(crate) argument: u64,
    /// Where its vDSO's image is, the page above its data page, or zero
    /// for none: what names `[vdso]` and `[vvar]`.
    pub(crate) vdso: u64,
    /// The mode it is entered in: its image's (`docs/I386.md` §3.4).
    pub(crate) abi: crate::trap::Abi,
}

/// The parts of a process the lock protects.
///
/// No `Default` or `Clone`: its signal tables are allocated, and a process
/// is made on paths that answer `ENOMEM` and `NO_MEMORY` (finding F-23).
#[derive(Debug)]
struct State {
    /// The heap, once something has asked for one.
    heap: Option<Heap>,
    /// Dispositions, the blocked mask and the alternate stack.
    signals: Signals,
}

impl State {
    /// A new process's: no heap, every signal at its default.
    fn new() -> Result<State, AllocError> {
        Ok(State {
            heap: None,
            signals: Signals::new()?,
        })
    }

    /// Everything it holds, leaving it holding nothing and allocating
    /// nothing: what `mem::take` did when a `Default` could be had for free.
    fn take(&mut self) -> State {
        State {
            heap: self.heap.take(),
            signals: core::mem::replace(&mut self.signals, Signals::released()),
        }
    }

    /// A fork child's: the parent's heap and dispositions, nothing pending.
    fn for_fork(&self) -> Result<State, AllocError> {
        Ok(State {
            heap: self.heap,
            signals: self.signals.for_fork()?,
        })
    }
}

/// What a process was started as.
///
/// A lock of its own, like the handle table: `/proc/<pid>/cmdline` read from
/// another process has no business waiting on this one's `brk`.
///
/// A `fork` child is what its parent was started as until it runs a program
/// of its own, as on Linux, where the child shares the parent's `exe_file`
/// and argument area. Chrome starts every child process by forking and
/// running `/proc/self/exe`, which a child with no identity cannot do.
#[derive(Debug, Default, Clone)]
struct Identity {
    /// The path it was started from: `/proc/<pid>/exe`'s text.
    exe: Vec<u8>,
    /// The file it was started from, when there was one: where
    /// `/proc/<pid>/exe` leads, as a magic link, whatever became of the path.
    exe_at: Option<Location>,
    /// Its argument vector: `/proc/<pid>/cmdline`.
    args: Vec<Vec<u8>>,
}

/// The classic `brk` heap: one region that grows upward.
#[derive(Debug, Clone, Copy)]
struct Heap {
    /// Where it starts, fixed for the life of the process.
    start: u64,
    /// The program break: the first address past the heap.
    brk: u64,
    /// How much is actually reserved, which is `brk` rounded up to a page.
    mapped_to: u64,
}

impl Process {
    /// A process over an address space, with no heap yet.
    ///
    /// It is in the root job, and counted there from now on. Its descriptors
    /// 0 to 2 may write the console and never read it: it is a native
    /// process, which anyone may start, and a fresh open that could read
    /// would undo a hangup of the console (`fs::console`).
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the core's part of it could not be allocated.
    pub(crate) fn new(space: Arc<AddressSpace>) -> Result<Process, AllocError> {
        Process::reading(space, Reading::Never)
    }

    /// [`Process::new`], with the console on its descriptors reading as
    /// `reading` says.
    fn reading(space: Arc<AddressSpace>, reading: Reading) -> Result<Process, AllocError> {
        Process::with_pid(
            space,
            object::process::allocate().unwrap_or(0),
            Arc::clone(job::root()),
            reading,
        )
    }

    /// [`Process::new`], for the process init starts: pid 1 when no other
    /// process holds it, any other pid when one does.
    ///
    /// # Errors
    ///
    /// As [`Process::new`].
    pub(crate) fn new_init(space: Arc<AddressSpace>) -> Result<Process, AllocError> {
        let pid = object::process::allocate_init()
            .or_else(object::process::allocate)
            .unwrap_or(0);
        Process::with_pid(space, pid, Arc::clone(job::root()), Reading::Always)
    }

    /// A process over an address space, numbered `pid`, which the caller has
    /// reserved in the registry, and counted in `job`, with the console on
    /// its descriptors 0 to 2 reading as `reading` says (`fs::console`).
    fn with_pid(
        space: Arc<AddressSpace>,
        pid: u32,
        job: Arc<Job>,
        reading: Reading,
    ) -> Result<Process, AllocError> {
        let files = fallible::try_arc(SpinLock::new(fd::standard_streams(reading)?))?;
        let fs = fallible::try_arc(SpinLock::new(fs::root_disk::process_context()))?;
        Process::with_context(space, pid, job, files, fs)
    }

    /// [`Process::with_pid`] over a descriptor table and a directory context
    /// that are already made. They are made first, and by the caller, because
    /// opening the console is among the deepest things a fork does, and a
    /// `Process` is over a kilobyte: built after them, it is not on the stack
    /// while they run. A kernel stack is four pages (`docs/PIDNS.md` §10).
    fn with_context(
        space: Arc<AddressSpace>,
        pid: u32,
        job: Arc<Job>,
        files: Arc<SpinLock<FdTable<Arc<OpenFile>>>>,
        fs: Arc<SpinLock<Context>>,
    ) -> Result<Process, AllocError> {
        Ok(Process {
            core: fallible::try_arc(object::process::Process::new(space, pid, job)?)?,
            umask: AtomicU32::new(DEFAULT_UMASK),
            oom_score_adj: AtomicI32::new(0),
            identity: SpinLock::new(Identity::default()),
            sem_undo: sem::UndoList::new(),
            files,
            fs,
            state: SpinLock::new(State::new()?),
            heap_lock: SleepLock::new((), &crate::sync::SchedParker),
            startup: SpinLock::new(None),
            start_claimed: AtomicBool::new(false),
            ending: AtomicBool::new(false),
            live_threads: AtomicU32::new(0),
            exec_thread: AtomicU32::new(0),
            thread_left: WaitQueue::new(),
            first_seccomp: SpinLock::new(None),
            handed_to: AtomicU32::new(0),
            attributes_pending: AtomicBool::new(false),
            released: AtomicBool::new(false),
            release_finished: AtomicBool::new(false),
            leader_status: AtomicI32::new(0),
            threads: SpinLock::new(Vec::new()),
            parent: SpinLock::new(Weak::new()),
            // A process the kernel starts leads its own group and session.
            // A fork child inherits its parent's instead, below.
            pgid: AtomicU32::new(pid),
            sid: AtomicU32::new(pid),
            pids: None,
            pid_for_children: SpinLock::new(None),
            groups: SpinLock::new(Groups::default()),
            children: SpinLock::new(Vec::new()),
            child_exited: WaitQueue::new(),
            exit_signal: AtomicU32::new(SIGCHLD),
            execed: AtomicBool::new(false),
            vfork_done: WaitQueue::new(),
            signalled: WaitQueue::new(),
            signal_arrived: fallible::try_arc(WaitQueue::new())?,
            pidfd_queue: SpinLock::new(None),
            stopped: AtomicU32::new(0),
            stop_report: AtomicU32::new(0),
            continue_report: AtomicBool::new(false),
            resumed: WaitQueue::new(),
            parked: AtomicU32::new(0),
            // A process the kernel starts is root's. A fork child takes its
            // parent's instead, below.
            credentials: SpinLock::new(Credentials::root()),
            nsproxy: SpinLock::new(NsProxy::initial()),
        })
    }

    /// A copy of `parent` over `space`, which is already a copy of its
    /// address space: what `fork` makes.
    ///
    /// What is copied is what Linux copies: the file descriptor table and the
    /// working directory and root (or the same ones, shared, when `clone` asks
    /// for `CLONE_FILES` or `CLONE_FS`), the heap and the signal dispositions,
    /// the process group and session, the umask, `oom_score_adj`, the user
    /// and group ids and supplementary groups, the program's start and what
    /// it was started as
    /// -- `/proc/<pid>/exe` and `cmdline` -- and its job, which is its
    /// cgroup. What is not is what belongs to the parent alone: its pid, its
    /// children, its threads, and its handles, which the native ABI passes on
    /// only explicitly.
    ///
    /// The job is the one the parent is in as this reads it. A move of the
    /// parent after that leaves the child where it started, which the fork's
    /// caller settles: `clone_with` ends a child whose job is being killed
    /// once the child is findable.
    pub(crate) fn forked(
        parent: &Arc<Process>,
        space: Arc<AddressSpace>,
        share_files: bool,
        share_fs: bool,
    ) -> Result<Process, AllocError> {
        let child = Process::forked_into(parent, space, share_files, share_fs, None, None)?;
        // Nothing else holds it yet.
        Arc::into_inner(child).ok_or(AllocError)
    }

    /// [`Process::forked`], into `job` rather than the parent's when one is
    /// given: `clone3`'s `CLONE_INTO_CGROUP`, whose caller has checked that
    /// the parent may put a process there. The child is counted there from
    /// the start, so it is never in the parent's job at all.
    ///
    /// Shared but not registered, and built in its `Arc` from the start: this
    /// is on the fork path, which stands deep on a four-page kernel stack, and
    /// a `Process` of over a kilobyte held by value in each frame between
    /// here and `clone_with` was most of it.
    #[inline(never)]
    pub(crate) fn forked_into(
        parent: &Arc<Process>,
        space: Arc<AddressSpace>,
        share_files: bool,
        share_fs: bool,
        job: Option<Arc<Job>>,
        pid_ns: Option<Arc<PidNamespace>>,
    ) -> Result<Arc<Process>, AllocError> {
        let job = job.unwrap_or_else(|| parent.job());
        // The child's own descriptors and directories, which are the parent's
        // or a copy of them: never the console `with_pid` would open for a
        // kernel-made process and the copy would then replace.
        let files = if share_files {
            Arc::clone(&parent.files)
        } else {
            let copied = parent.files.lock().try_clone().map_err(|_| AllocError)?;
            fallible::try_arc(SpinLock::new(copied))?
        };
        let fs = if share_fs {
            Arc::clone(&parent.fs)
        } else {
            fallible::try_arc(SpinLock::new(parent.fs.lock().clone()))?
        };
        let mut shared = Process::shared_with_context(
            space,
            object::process::allocate().unwrap_or(0),
            job,
            files,
            fs,
        )?;
        let child = Arc::get_mut(&mut shared).ok_or(AllocError)?;
        // Born in its job's speculation domain only as the child of a member
        // in that same domain: a process that moved into a marked job, or
        // left its domain, has children outside it too
        // (`docs/OPAQUE-KERNEL.md` §9.2). A child sent elsewhere by
        // `CLONE_INTO_CGROUP` is in that job's domain only if its parent is.
        if child.core.speculation_domain() != parent.core.speculation_domain() {
            child.core.leave_speculation_domain_unstarted();
        }
        // In `pid_ns` when `CLONE_NEWPID` made one, else where the parent's
        // children go. Numbered in every namespace from there up, so a
        // namespace that is ending, or a job out of memory, refuses the fork.
        let children_ns = pid_ns.or_else(|| parent.children_namespace());
        if let Some(namespace) = children_ns
            && child.pid() != 0
        {
            child.pids = Some(pidns::assign(&namespace, child.pid()).map_err(|_| AllocError)?);
        }
        child.groups = SpinLock::new(parent.groups.lock().clone());
        child.state = SpinLock::new(parent.state.lock().for_fork()?);
        child.startup = SpinLock::new(parent.startup());
        child.parent = SpinLock::new(Arc::downgrade(parent));
        child.pgid = AtomicU32::new(parent.pgid());
        child.sid = AtomicU32::new(parent.sid());
        child.umask = AtomicU32::new(parent.umask());
        child.oom_score_adj = AtomicI32::new(parent.oom_score_adj());
        child.credentials = SpinLock::new(parent.credentials.lock().clone());
        child.nsproxy = SpinLock::new(parent.nsproxy.lock().clone());
        child.identity = SpinLock::new(parent.identity.lock().clone());
        child.attributes_pending = AtomicBool::new(true);
        Ok(shared)
    }

    /// [`Process::with_context`], shared. Apart, so that the `Process` it
    /// builds is on the stack only while it moves to the heap.
    #[inline(never)]
    fn shared_with_context(
        space: Arc<AddressSpace>,
        pid: u32,
        job: Arc<Job>,
        files: Arc<SpinLock<FdTable<Arc<OpenFile>>>>,
        fs: Arc<SpinLock<Context>>,
    ) -> Result<Arc<Process>, AllocError> {
        fallible::try_arc(Process::with_context(space, pid, job, files, fs)?)
    }

    /// The network namespace it is in.
    pub(crate) fn net_ns(&self) -> Arc<crate::net::NetNamespace> {
        let held = self.nsproxy.lock().net.clone();
        held.unwrap_or_else(|| Arc::clone(crate::net::first()))
    }

    /// Put it in another network namespace. What it held is dropped after the
    /// lock is released, since the namespace it named may end there.
    pub(crate) fn set_net_ns(&self, namespace: Arc<crate::net::NetNamespace>) {
        let displaced = self.nsproxy.lock().net.replace(namespace);
        drop(displaced);
    }

    /// Whether the attributes a fork child takes from its parent are still to
    /// be given (`attributes::inherit` ends it).
    pub(crate) fn attributes_pending(&self) -> bool {
        self.attributes_pending.load(Ordering::Acquire)
    }

    /// The attributes have been given: [`Process::attributes_pending`] ends.
    pub(crate) fn settle_attributes(&self) {
        self.attributes_pending.store(false, Ordering::Release);
    }

    /// For a native child of a creator, made before its attributes are given.
    pub(crate) fn await_attributes(&self) {
        self.attributes_pending.store(true, Ordering::Release);
    }

    /// Its descriptor table.
    ///
    /// A lock rather than a closure, unlike [`Process::with_handles`], because
    /// the table's own methods already hand back what they displace. Hold the
    /// guard for a table operation and no longer: clone the description out,
    /// and drop what `remove` or `install` returns after the guard is gone.
    pub(crate) fn files(&self) -> &Arc<SpinLock<FdTable<Arc<OpenFile>>>> {
        &self.files
    }

    /// Its root and working directory. Clone the context out before walking a
    /// path with it.
    pub(crate) fn fs_context(&self) -> &Arc<SpinLock<Context>> {
        &self.fs
    }

    /// Record what it was started as: the path, the file when there was one,
    /// and the arguments.
    pub(crate) fn record_exec(&self, exe: &[u8], exe_at: Option<Location>, args: &[&[u8]]) {
        let exe = exe.to_vec();
        let args = args.iter().map(|arg| arg.to_vec()).collect();
        let mut identity = self.identity.lock();
        identity.exe = exe;
        identity.args = args;
        // The old file goes after the lock: its dentry's last reference may be
        // this one.
        let old = core::mem::replace(&mut identity.exe_at, exe_at);
        drop(identity);
        drop(old);
    }

    /// The path it was started from, empty if nothing was.
    pub(crate) fn exe(&self) -> Vec<u8> {
        self.identity.lock().exe.clone()
    }

    /// The file it was started from, if it was started from one.
    pub(crate) fn exe_location(&self) -> Option<Location> {
        self.identity.lock().exe_at.clone()
    }

    /// Its argument vector.
    pub(crate) fn args(&self) -> Vec<Vec<u8>> {
        self.identity.lock().args.clone()
    }

    /// The command name: the last component of the path it was started from,
    /// cut to the fifteen bytes Linux's `TASK_COMM_LEN` leaves room for.
    pub(crate) fn comm(&self) -> Vec<u8> {
        let identity = self.identity.lock();
        let base = identity
            .exe
            .rsplit(|&byte| byte == b'/')
            .next()
            .unwrap_or_default();
        base.iter().copied().take(15).collect()
    }

    /// The heap's start and the end of what is reserved for it, once
    /// something has placed it.
    pub(crate) fn heap_range(&self) -> Option<(u64, u64)> {
        self.state
            .lock()
            .heap
            .map(|heap| (heap.start, heap.mapped_to))
    }

    /// Copy its address space for `fork`, and make the child from the copy
    /// with `make`, both under the heap lock: the heap `make` copies out of
    /// this process then describes the space it was given, not a `brk` half
    /// done on another thread. Never with a spin lock held.
    ///
    /// # Errors
    ///
    /// As [`AddressSpace::fork`].
    pub(crate) fn fork_memory<R>(
        &self,
        make: impl FnOnce(Arc<AddressSpace>) -> R,
    ) -> Result<R, SpaceError> {
        let _heap = self.heap_lock.lock();
        // Not between the two halves of another thread's `MAP_FIXED`.
        let _layout = self.space().layout();
        let space = self.space().fork()?;
        Ok(make(space))
    }

    /// Hold the heap lock, as `brk` and `fork` do: for the check that shows
    /// each waits for it.
    pub(crate) fn hold_heap_for_check(&self) -> SleepLockGuard<'_, ()> {
        self.heap_lock.lock()
    }

    /// Read or change the signal state, under the process lock.
    ///
    /// A closure rather than a guard, so that nothing a handler does with user
    /// memory can happen while the lock is held: every copy to or from the
    /// program is outside it.
    pub(crate) fn with_signals<R>(&self, change: impl FnOnce(&mut Signals) -> R) -> R {
        change(&mut self.state.lock().signals)
    }

    /// Forget what the old program set up, as `execve` does before loading the
    /// new one: its heap and its signal handlers. The address space is emptied
    /// by the caller, which also forgets the address its thread asked to have
    /// cleared.
    pub(crate) fn reset_for_exec(&self) {
        let mut state = self.state.lock();
        state.heap = None;
        state.signals.reset_for_exec();
    }

    /// Put the heap just past the loaded image, before anything asks for it.
    ///
    /// Without this, the first `brk` places the heap above the highest thing
    /// mapped -- and the highest thing mapped is the stack, at the very top of
    /// the user half, so the heap would begin at `USER_VIRT_END` and every
    /// attempt to grow it would be refused. glibc survives that by falling back
    /// to `mmap` for everything, which is how the first busybox run showed it:
    /// `brk(0)` answering `0x800000000000`.
    ///
    /// A page of gap is left after the image, so that a heap overrun backwards
    /// faults rather than writing over the last page of `.bss`.
    pub(crate) fn set_heap_base(&self, image_end: u64) {
        let Some(start) = round_up(image_end).and_then(|end| end.checked_add(PAGE_SIZE)) else {
            return;
        };
        let mut state = self.state.lock();
        if state.heap.is_none() {
            state.heap = Some(Heap {
                start,
                brk: start,
                mapped_to: start,
            });
        }
    }

    /// Move the program break, and report where it now is.
    ///
    /// # The convention, which is not an error convention
    ///
    /// `brk` does not report failure. It returns the break, and the caller
    /// compares it with what it asked for: unchanged means refused. That is
    /// why this returns a bare `u64` and why a request that cannot be met
    /// returns the *current* break rather than an error — a libc that got
    /// `-ENOMEM` here would read it as an enormous valid break and walk off
    /// the end of its heap.
    ///
    /// `brk(0)` is the query every libc opens with.
    pub(crate) fn set_break(&self, want: u64) -> u64 {
        let _heap = self.heap_lock.lock();
        // The heap grows into whatever no mapping holds, which another
        // thread's `mmap` may be choosing at the same moment.
        let _layout = self.space().layout();
        let mut state = self.state.lock();

        let heap = match state.heap {
            Some(heap) => heap,
            None => {
                // First call. Place the heap above everything the ELF loader
                // mapped, so the two never have to agree on a number, with a
                // page of gap so a heap overrun cannot walk straight into the
                // last data page. An empty space starts from the lowest address
                // anything may be mapped at, not from zero.
                let after = self.space().highest_mapped().unwrap_or(MMAP_MIN_ADDR);
                let start = after.saturating_add(PAGE_SIZE);
                let heap = Heap {
                    start,
                    brk: start,
                    mapped_to: start,
                };
                state.heap = Some(heap);
                heap
            }
        };

        // A query, or a request below the start: report where we are.
        if want == 0 || want < heap.start {
            return heap.brk;
        }

        let page_end = round_up(want);
        let Some(page_end) = page_end else {
            return heap.brk;
        };

        if page_end > heap.mapped_to {
            // Growing. Reserve the new pages; they cost nothing until touched.
            let len = page_end - heap.mapped_to;
            if self
                .space()
                .map_anonymous(heap.mapped_to, len, VmaFlags::READ_WRITE)
                .is_err()
            {
                return heap.brk;
            }
        }

        let old_mapped_to = heap.mapped_to;
        let heap = Heap {
            start: heap.start,
            brk: want,
            mapped_to: page_end,
        };
        state.heap = Some(heap);
        drop(state);

        if page_end < old_mapped_to {
            // Shrinking. Give the pages back now rather than at exit: a
            // program that frees half its heap expects the memory returned.
            //
            // **After the state is written and its lock gone.** An unmap
            // waits for every other processor to drop its translations, and
            // `state` is the lock a signal or a `kill` from another processor
            // spins on; a shootdown may not be asked for under a lock that
            // disables preemption, and `smp` checks that it is not. The
            // range is this heap's own, page-aligned and non-empty, so the
            // unmap cannot be refused.
            //
            // **Under `heap_lock`, which may sleep.** Between the write and
            // the unmap the heap says it ends here while the tail is still
            // mapped. Another thread's `brk` growing into that window would
            // find the pages mapped and be refused, and another thread's
            // `fork` would clone the tail into a child whose heap already
            // ends below it, so that the child's heap could never grow over
            // it. Both take this lock first, so neither can see the window.
            let _ = self.space().unmap(page_end, old_mapped_to - page_end);
        }
        heap.brk
    }
}

/// The mask a process starts with: group and others may not write.
///
/// Linux's own default for `init`, and what nearly every login leaves in
/// place, so a file a program creates before anything has called `umask` gets
/// the mode it would get on Linux.
const DEFAULT_UMASK: u32 = 0o022;

impl Process {
    /// The permission bits new files and directories are made without.
    pub(crate) fn umask(&self) -> u32 {
        self.umask.load(Ordering::Relaxed)
    }

    /// `umask`: replace the mask, keeping only permission bits, and report
    /// the old one.
    pub(crate) fn set_umask(&self, mask: u32) -> u32 {
        self.umask.swap(mask & 0o777, Ordering::Relaxed)
    }

    /// What `/proc/<pid>/oom_score_adj` says: see the field.
    pub(crate) fn oom_score_adj(&self) -> i32 {
        self.oom_score_adj.load(Ordering::Relaxed)
    }

    /// Set `oom_score_adj`, which the caller has checked is in range.
    pub(crate) fn set_oom_score_adj(&self, value: i32) {
        self.oom_score_adj.store(value, Ordering::Relaxed);
    }

    /// Its user and group ids and supplementary groups, under their lock:
    /// `change` sees all of them at once, and what it changes changes
    /// together. Nothing that waits may be done inside it.
    pub(crate) fn with_credentials<R>(&self, change: impl FnOnce(&mut Credentials) -> R) -> R {
        change(&mut self.credentials.lock())
    }

    /// Its UTS, IPC and cgroup namespaces, as they are now.
    pub(crate) fn nsproxy(&self) -> NsProxy {
        self.nsproxy.lock().clone()
    }

    /// Put it in `proxy`'s namespaces. What it was in goes after the lock, as
    /// the last reference to a namespace may end it.
    pub(crate) fn set_nsproxy(&self, proxy: NsProxy) {
        let old = core::mem::replace(&mut *self.nsproxy.lock(), proxy);
        drop(old);
    }

    /// The semaphore sets it holds undo records in.
    pub(crate) fn sem_undo(&self) -> &sem::UndoList {
        &self.sem_undo
    }
}

/// Round up to a page, or `None` if that would leave the address space.
fn round_up(at: u64) -> Option<u64> {
    at.checked_add(PAGE_SIZE - 1)
        .map(|at| at & !(PAGE_SIZE - 1))
}

impl Process {
    /// Record where its first task enters user mode.
    pub(crate) fn set_startup(&self, startup: Startup) {
        *self.startup.lock() = Some(startup);
    }

    /// Where its first task enters user mode, once a program is loaded.
    pub(crate) fn startup(&self) -> Option<Startup> {
        *self.startup.lock()
    }

    /// Block until it has ended and let go of what it held, or `deadline`
    /// passes, and report how it ended if it has.
    ///
    /// Never for a process of the caller's own: its release waits for the
    /// caller's thread to leave, which a caller waiting here never does.
    pub(crate) fn wait_for_exit(&self, deadline: u64) -> Option<i32> {
        let _ = self
            .exited()
            .wait_until_deadline(|| self.is_released(), deadline);
        if self.is_released() {
            self.exit_status()
        } else {
            None
        }
    }

    /// Whether it has ended and let go of its handles and descriptors: its
    /// last thread gone, or none ever started. What `wait4` reaps by, so that
    /// no process is reaped while a thread of it is still in the kernel.
    pub(crate) fn is_released(&self) -> bool {
        self.release_finished.load(Ordering::Acquire)
    }

    /// How many of its threads have started and not yet ended.
    pub(crate) fn live_thread_count(&self) -> u32 {
        self.live_threads.load(Ordering::Acquire)
    }

    /// Whether every task running its code is blocked or has ended: for the
    /// check that a stopped process's threads have all parked. The tasks are
    /// taken out of the list before they are looked at, so none is dropped
    /// under its lock.
    pub(crate) fn every_task_blocked(&self) -> bool {
        self.tasks()
            .iter()
            .all(|task| task.is_blocked() || task.is_dead())
    }

    /// The tasks running its code that are still alive.
    ///
    /// Taken out of the list rather than looked at under its lock, so that
    /// dropping the last reference to one -- which gives back an address
    /// space -- never happens with the lock held.
    pub(crate) fn tasks(&self) -> Vec<Arc<Task>> {
        self.core()
            .with_tasks(|tasks| tasks.iter().filter_map(Weak::upgrade).collect())
    }

    /// Whether the running task, waiting on behalf of this process, is to stop
    /// waiting and leave: the process is ending, or the task is a thread of it
    /// that another thread's `execve` is ending. What a wait that does not look
    /// at signals checks instead, so that neither an exit nor an `execve` waits
    /// for it for ever.
    pub(crate) fn caller_must_leave(&self) -> bool {
        self.is_terminated()
            || thread::current_of(self).is_some_and(|thread| self.must_leave(&thread))
    }

    /// Whether `thread` is to leave rather than run on: its process is ending,
    /// or another of its threads is replacing the program.
    pub(crate) fn must_leave(&self, thread: &Thread) -> bool {
        if self.is_terminated() {
            return true;
        }
        let replacing = self.exec_thread.load(Ordering::Acquire);
        replacing != 0 && replacing != thread.tid()
    }

    /// End every thread of it but `caller`, which is about to replace the
    /// program, as Linux's `de_thread` does; then let `caller` take the pid if
    /// it was not the first thread, since it is the leader from here on.
    ///
    /// The other threads are made to come back through the kernel -- woken
    /// from any wait, which [`Thread::signal_pending`] ends, interrupted in
    /// user mode, released from a stop -- and each finds it must leave, and
    /// leaves, writing its cleared id into the memory that is still the old
    /// program's. A thread starting at this moment leaves before it enters the
    /// program. The wait lasts until `caller` is the only live thread.
    ///
    /// # Errors
    ///
    /// `EAGAIN` when another thread is already replacing the program, which
    /// makes this one leave, or when the process begins to end during the
    /// wait. Either way `caller` never returns to the program.
    pub(crate) fn end_other_threads(&self, caller: &Thread) -> Result<(), Errno> {
        // A thread numbered zero -- of a process made with every pid in use --
        // cannot mark itself as the one replacing the program, since zero
        // means none: it refuses while other threads are live, as `execve` did
        // before threads could be ended.
        if caller.tid() == 0 {
            return if self.live_thread_count() > 1 {
                Err(Errno::EAGAIN)
            } else {
                Ok(())
            };
        }
        let posting = sched::work::posting();
        if self
            .exec_thread
            .compare_exchange(0, caller.tid(), Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Errno::EAGAIN);
        }
        self.signalled.wake_all();
        self.resumed.wake_all();
        // `END` to every other thread: each must leave (`must_leave`).
        self.wake_other_tasks(sched::work::END);
        drop(posting);
        let _ = self.thread_left.wait_until_deadline(
            || self.live_thread_count() <= 1 || self.is_terminated(),
            u64::MAX,
        );
        // Cleared before the id changes hands, so the caller is never, even
        // for a moment, a thread that must leave.
        self.exec_thread.store(0, Ordering::Release);
        if self.is_terminated() {
            return Err(Errno::EAGAIN);
        }
        let pid = self.pid();
        let own = caller.tid();
        if own != pid {
            let _ = caller.take_pid(pid);
            registry::release_thread(own, self);
        }
        Ok(())
    }

    /// Count a thread about to start. Before its task is spawned, because the
    /// task can reach its exit on another processor before the spawn returns.
    pub(crate) fn thread_starting(&self) {
        let _ = self.live_threads.fetch_add(1, Ordering::AcqRel);
    }

    /// Count a thread gone -- one that `ended`, or one whose task could not be
    /// spawned -- and, if that was its last thread, end the process or let go
    /// of what it holds.
    ///
    /// The count reaching zero is one step, so this is where a last thread's
    /// `exit` ends the process: two last threads leaving at once cannot each
    /// see the other and both leave it running. With the process already
    /// ending, this releases it; either this sees the ending, or the ending
    /// sees no thread live, since each reads what the other writes after
    /// writing its own. A thread that never started ends nothing, so a start
    /// that failed leaves the process to be started again.
    pub(crate) fn thread_gone(&self, ended: bool) {
        let before = self
            .live_threads
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                live.checked_sub(1)
            });
        self.thread_left.wake_all();
        // A thread that leaves a frozen cgroup's process may have been the
        // last one the cgroup waited for.
        if self.core().is_frozen() {
            fs::cgroupfs::settle_frozen(&self.core().job());
        }
        if before != Ok(1) {
            return;
        }
        if self.is_terminated() {
            self.release();
        } else if ended {
            // With the status its first thread left with. `end` finds no
            // thread live and releases.
            let _ = self.end(self.leader_status.load(Ordering::Acquire), 0);
        }
    }

    /// End it with `status`, unless something already has. Answers whether
    /// this call was the one that did.
    fn terminate(&self, status: i32) -> bool {
        self.end(status, 0)
    }

    /// End it with `status`, recording `signal` as what ended it when that is
    /// not zero. The one path `exit_group`, `kill` and a last thread's `exit`
    /// take.
    ///
    /// Ending has two moments, as on Linux. This is the first: the status is
    /// recorded, [`Process::is_terminated`] turns true, and whatever its threads
    /// wait in is woken so that they leave. The second, [`Process::release`],
    /// lets go of what it holds once its last thread has gone -- here and now
    /// if none is live: a process never started, or one whose threads have all
    /// ended. Native process creation's `Control` relies on that when it drops
    /// an unstarted child, and the orphan checks when they end processes that
    /// never ran.
    ///
    /// A thread still in the kernel therefore keeps its process's descriptors
    /// and orphans until it reaches its exit, which every wait it can be in
    /// allows: each ends once its process is terminated or a signal is
    /// pending. A wait that did not would hold the release back for as long as
    /// it lasted.
    fn end(&self, status: i32, signal: u32) -> bool {
        if self.ending.swap(true, Ordering::AcqRel) {
            return false;
        }
        // The core stores `terminated` and posts `END` to every task, waking
        // and interrupting each (`object::process::Process::end_record`).
        self.core().end_record(status, signal);
        // A `vfork` parent waits for this, and a thread in `pause` or stopped
        // waits for a signal or a continue; none should wait for the release.
        self.vfork_done.wake_all();
        self.signalled.wake_all();
        self.resumed.wake_all();
        if self.live_threads.load(Ordering::Acquire) == 0 {
            self.release();
        }
        true
    }

    /// Let go of everything it holds beyond its address space, once, after
    /// [`Process::end`] and when no thread of it is live.
    ///
    /// The address space goes when the last task holding it is reaped, because
    /// a processor may still be translating through it until then. The waiters
    /// are woken last, when there is nothing left to observe half done. Runs
    /// with interrupts open: closing handles and firing watchers take plain
    /// locks.
    fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        // A session's leader ending lets go of the session's terminal, the
        // console's or a pty's, as Linux's `disassociate_ctty`: otherwise a
        // later leader given the same pid would find it its own.
        if self.pid() == self.sid() {
            fs::terminal::forget_session(self.sid());
            fs::pty::forget_session(self.sid());
        }
        // The heap record and the signal tables go now. Taken under the lock
        // and dropped after it.
        let state = self.state.lock().take();
        drop(state);
        // The handles too, and outside every lock: an object's drop can free
        // memory and drain other objects, which is `object::dispose`'s job,
        // and must not run under this process's table lock or state lock.
        object::dispose(self.with_handles(HandleTable::close));
        // And a bootstrap handle it was given and never took (`docs/INIT.md`
        // §6), which no table held.
        object::dispose(self.close_bootstrap());
        // Its descriptors close now, as Linux's exit closes them, rather than
        // when the last reference to the process goes -- which a parent that
        // has not reaped it yet still holds. Otherwise a pipe's write end
        // outlives the program that wrote, and `ls | wc -l` hangs: `wc` waits
        // for an end of file that only comes when the shell reaps `ls`, and the
        // shell reaps nothing until `wc` ends. Only when no other process
        // shares the table (`CLONE_FILES`), whose descriptors are still its
        // own. Taken out under the lock and dropped after it, because closing
        // a pipe end wakes its queues.
        //
        // The table's room goes with them, and the charge its job carries for
        // it, as Linux's exit puts its `files_struct`: left in place it was
        // given back only when the last task was reaped, which a machine with
        // work to do puts off, and a parent that had waited for its child
        // still read the child's table in its cgroup's `memory.current`.
        if Arc::strong_count(&self.files) == 1 {
            let (closed, emptied): (Vec<Arc<OpenFile>>, _) = {
                let mut files = self.files.lock();
                let open: Vec<i32> = files.iter().map(|(fd, _)| fd).collect();
                let closed = open
                    .into_iter()
                    .filter_map(|fd| files.remove(fd).ok())
                    .collect();
                let mut fresh = FdTable::new();
                let _ = fresh.set_limit(files.limit());
                (closed, core::mem::replace(&mut *files, fresh))
            };
            for file in &closed {
                fd::closed(self, file);
            }
            drop(closed);
            drop(emptied);
            let _ = fs::socket::collect_cycles();
        }
        // What it owes the semaphore sets it used `SEM_UNDO` on is paid now,
        // before its parent is told, so a parent's `wait4` sees the sets as
        // the ending left them, as Linux's `exit_sem` runs before
        // `exit_notify`. Spin locks and wakes only; see `sem::exit`.
        sem::exit(&self.sem_undo, self.pid());
        // Its System V shared memory attaches are let go now too, as Linux's
        // `exit_mm` lets them go before `exit_notify`: a parent's `wait4`
        // sees the attach count without them, and a removed segment goes
        // with its last attach rather than when this process is reaped. No
        // other process shares the space: `clone` refuses `CLONE_VM` without
        // `CLONE_THREAD`, and `vfork` copies.
        crate::syscall::shm::exit(self.space());
        // Nothing of it can run any more, so it leaves its job's count: a job
        // is empty once its last member gets here, not once that member is
        // reaped, which is what `cgroup.events` says on Linux too.
        self.leave_job();
        // Whoever watches it through a port hears now, once its handles and
        // descriptors are closed: a driver's pins are given back or kept
        // before `devmgr` learns that the driver has gone. Taken before the
        // queue is woken, so a waiter it wakes sees the handle's `TERMINATED`.
        let observers = self.exit().close();
        self.exited().wake_all();
        self.vfork_done.wake_all();
        self.signalled.wake_all();
        self.resumed.wake_all();
        for observer in observers {
            observer.fire(ObjectSignals::TERMINATED);
        }

        // Its own children are orphaned, and go where Linux's
        // `forget_original_parent` sends them: to the nearest ancestor still
        // running that asked to reap orphaned descendants, or else to init.
        // Each is sent the signal it asked for on its parent's death.
        //
        // An orphan is put in its new parent's list before its parent is
        // changed, so one ending at this moment disowns itself from a list it
        // is already in. One that ended before the change told this process,
        // which has ended and hears nothing, so its new parent is told here;
        // if the orphan ends in between, the new parent is told twice, and
        // `SIGCHLD` does not queue.
        //
        // With nobody to take them -- the boot checks run before init, and init
        // itself may end -- they are released as before: an ended one here, a
        // running one when it ends.
        // The init of a pid namespace takes its namespace with it: nothing
        // may join it, and everything in it is killed (`docs/PIDNS.md` §5).
        pidns::init_gone(self);
        let orphans = core::mem::take(&mut *self.children.lock());
        let reaper = self.reaper_for_orphans();
        for orphan in orphans {
            let death_signal = attributes::get(&orphan).parent_death_signal;
            let Some(reaper) = &reaper else {
                *orphan.parent.lock() = Weak::new();
                kill::send(&orphan, death_signal, Origin::Kernel);
                continue;
            };
            // Linux's reason, verbatim: "We don't want people slaying init."
            // An orphan made with another exit signal tells its new parent with
            // `SIGCHLD`, which the new parent expects.
            orphan.exit_signal.store(SIGCHLD, Ordering::Release);
            reaper.adopt(Arc::clone(&orphan));
            *orphan.parent.lock() = Arc::downgrade(reaper);
            kill::send(&orphan, death_signal, Origin::Kernel);
            if orphan.is_released() {
                if reaper.with_signals(|signals| signals.reaps_children_automatically()) {
                    reaper.disown(&orphan);
                }
                let (code, told) = orphan.end_report();
                kill::tell_parent(&orphan, SIGCHLD, code, told);
            }
        }

        // A parent that asked never to wait lets it go before it can be seen
        // released, so that parent's `wait4` never reaps it.
        let parent = self.parent.lock().upgrade();
        if let Some(parent) = parent
            && parent.with_signals(|signals| signals.reaps_children_automatically())
        {
            parent.disown(self);
        }
        // Released: what `wait4` reaps by and `wait_for_exit` waits for. After
        // its orphans have gone on, so a parent that reaps it finds nothing left
        // to do, and the queue woken again for the waiters that wait for this.
        self.release_finished.store(true, Ordering::Release);
        self.exited().wake_all();
        let pidfds = self.pidfd_queue.lock().clone();
        if let Some(queue) = pidfds {
            queue.wake_all();
        }
        // Its parent is told -- woken, and sent the signal it was created with,
        // `SIGCHLD` for a fork. It stays in the parent's list, ended, until
        // `wait4` takes it.
        let (code, told) = self.end_report();
        kill::tell_parent(self, self.exit_signal.load(Ordering::Acquire), code, told);
    }

    /// How its end is reported to a parent: the `si_code` and the status or
    /// signal that goes with it. Valid once it has terminated.
    fn end_report(&self) -> (i32, i32) {
        match self.exit().signal() {
            None => (kill::CLD_EXITED, self.exit_status().unwrap_or(0) & 0xFF),
            Some(signal) => (kill::CLD_KILLED, signal as i32),
        }
    }

    /// Where its children go when it ends: the nearest ancestor *in its own
    /// namespace* still running that set `PR_SET_CHILD_SUBREAPER`, or else
    /// its namespace's init -- pid 1 of the machine, in the first namespace --
    /// if that is running and is not this process.
    fn reaper_for_orphans(&self) -> Option<Arc<Process>> {
        let mut ancestor = self.parent();
        while let Some(candidate) = ancestor {
            if !pidns::same_namespace(self, &candidate) {
                break;
            }
            if !candidate.is_terminated() && attributes::get(&candidate).child_subreaper {
                return Some(candidate);
            }
            ancestor = candidate.parent();
        }
        if let Some(local) = pidns::local_reaper(self) {
            return local;
        }
        registry::find(registry::INIT_PID)
            .filter(|init| !init.is_terminated() && !core::ptr::eq(Arc::as_ptr(init), self))
    }
}

impl Process {
    /// Its parent, if it has one that still exists.
    pub(crate) fn parent(&self) -> Option<Arc<Process>> {
        self.parent.lock().upgrade()
    }

    /// The queue woken when a signal is sent to it.
    pub(crate) fn signalled(&self) -> &WaitQueue {
        &self.signalled
    }

    /// The queue woken when a signal becomes pending for it or one of its
    /// threads: a signalfd's.
    pub(crate) fn signal_arrived(&self) -> &Arc<WaitQueue> {
        &self.signal_arrived
    }

    /// Make the queue a pidfd for it waits on, unless there is one.
    ///
    /// # Errors
    ///
    /// When the queue cannot be allocated.
    pub(crate) fn make_pidfd_queue(&self) -> Result<(), AllocError> {
        let mut queue = self.pidfd_queue.lock();
        if queue.is_none() {
            *queue = Some(fallible::try_arc(WaitQueue::new())?);
        }
        Ok(())
    }

    /// The queue a pidfd for it waits on, once [`Process::make_pidfd_queue`]
    /// has made it: woken as it is released.
    pub(crate) fn pidfd_queue(&self) -> Option<Arc<WaitQueue>> {
        self.pidfd_queue.lock().clone()
    }

    /// Whether a wait it is in should end: it has ended, or a signal the
    /// waiting thread does not block is pending. What every call that waits
    /// checks, beside its own condition, to return `EINTR`.
    ///
    /// Asked of the calling thread when the caller is one of its threads, and
    /// otherwise of its first. A kernel task waiting on behalf of a process
    /// with no thread at all -- a self-check's -- has no mask to block with,
    /// so any signal sent to the process ends its wait.
    pub(crate) fn signal_pending(&self) -> bool {
        match self.signal_taker() {
            Some(thread) => thread.signal_pending(),
            None => self.is_terminated() || self.with_signals(|signals| signals.pending() != 0),
        }
    }

    /// Make sure a thread of it looks at `signal`, sent to it as a whole and
    /// just made pending.
    ///
    /// Every wait on [`Process::signalled`] is woken, since a thread may be in
    /// `rt_sigtimedwait` for exactly this signal, blocked. Beyond that, one
    /// thread that does not block it -- the caller, if it is one, or else the
    /// first -- is woken from whatever else it waits in and interrupted if it
    /// is running in user mode on another processor, so that it comes back
    /// through the kernel to have the signal delivered, as Linux's
    /// `complete_signal` chooses one. A signal every thread blocks waits in the
    /// process's queue for the first thread to unblock it.
    pub(crate) fn notify_signal(&self, signal: u32) {
        self.signalled.wake_all();
        self.signal_arrived.wake_all();
        let taker = thread::current_of(self)
            .filter(|me| !me.is_gone() && !me.blocks(signal))
            .or_else(|| {
                self.threads()
                    .into_iter()
                    .find(|thread| !thread.blocks(signal))
            });
        if let Some(taker) = taker {
            self.handed_to.store(taker.tid(), Ordering::Release);
            self.wake_thread(&taker);
        }
    }

    /// Make sure `thread` looks at a signal just made pending for it alone.
    pub(crate) fn notify_signal_to(&self, thread: &Thread) {
        self.signalled.wake_all();
        self.signal_arrived.wake_all();
        self.wake_thread(thread);
    }

    /// Make every task of it but the caller's come back through the kernel:
    /// one blocked in a call is woken, and one running in user mode is
    /// interrupted, each to find on its way back to user mode that its process
    /// is ending or stopping.
    ///
    /// Waiting for a tick is not enough, because a task alone on its processor
    /// gets none -- the scheduler leaves a lone task to run -- and a program
    /// spinning there would outlive the change until it chose to make a call.
    /// The caller's own task, if it is one, is already on its way.
    fn wake_other_tasks(&self, bits: u32) {
        // The other half of `WaitQueue::wait_trusting`'s fence pair: the
        // replacing thread or the stop, recorded before this, is ordered
        // before the task states the wakes below read.
        core::sync::atomic::fence(Ordering::SeqCst);
        let current = sched::current();
        self.core().post_to_tasks(bits, current.as_deref());
    }

    /// Hand a signal sent to it as a whole on to a thread that can take it,
    /// when the thread calling this is leaving: interrupt the first remaining
    /// thread that does not block one of the pending signals, as Linux's
    /// `retarget_shared_pending` does. Otherwise a signal only the leaving
    /// thread would have taken waits until another happens to come back
    /// through the kernel.
    pub(crate) fn retarget_shared_signals(&self) {
        self.hand_on(u64::MAX, None);
    }

    /// Hand the signals among `newly` that are pending for the process as a
    /// whole on to a thread other than `from` that does not block them, once
    /// `from` has blocked them.
    ///
    /// A thread chosen to take a signal may block it before it gets there --
    /// through `rt_sigprocmask`, a handler's mask or `rt_sigsuspend` -- and the
    /// signal would then wait for another thread to happen to come back through
    /// the kernel, which a thread alone on its processor may never do. Linux's
    /// `__set_task_blocked` hands it on through `retarget_shared_pending`.
    /// `newly` is worked out under the signal lock the mask changed under, and
    /// this is called once that lock is let go.
    pub(crate) fn hand_on_newly_blocked(&self, from: &Thread, newly: u64) {
        if newly != 0 {
            self.hand_on(newly, Some(from));
        }
    }

    /// Wake live threads but `except`, in order, until each of `signals` that
    /// is pending for the process as a whole has one that does not block it.
    fn hand_on(&self, signals: u64, except: Option<&Thread>) {
        let mut pending = self.with_signals(|shared| shared.pending()) & signals;
        // On until every one of them has a thread that can take it, as Linux's
        // `retarget_shared_pending` goes on: one thread may take one of them
        // and block another.
        for thread in self.threads() {
            if pending == 0 {
                break;
            }
            if except.is_some_and(|except| core::ptr::eq(Arc::as_ptr(&thread), except)) {
                continue;
            }
            let takes = thread.with_own_signals(|own| pending & !own.blocked());
            if takes != 0 {
                self.handed_to.store(thread.tid(), Ordering::Release);
                self.wake_thread(&thread);
                pending &= !takes;
            }
        }
    }

    /// The id of the thread a signal was last handed on to, taken so that the
    /// next hand-off is seen afresh; zero if none was since the last look.
    pub(crate) fn take_handed_to(&self) -> u32 {
        self.handed_to.swap(0, Ordering::AcqRel)
    }

    /// Wake `thread`'s task from any wait it is in, and interrupt it if it is
    /// running.
    pub(crate) fn wake_thread(&self, thread: &Thread) {
        for task in &self.tasks() {
            if thread::of_task(task).is_some_and(|own| core::ptr::eq(own, thread)) {
                sched::work::notify(task, sched::work::SIGNAL);
            }
        }
    }

    /// Have its first thread start under `state`, which a native creator's
    /// thread was under.
    pub(crate) fn set_first_seccomp(&self, state: crate::syscall::seccomp::State) {
        *self.first_seccomp.lock() = Some(state);
    }

    /// The state its first thread is to start under; `None` for a process
    /// nothing filtered made. Kept and not taken: a start that is refused and
    /// made again -- a bad argument handle, no memory for the thread -- makes a
    /// new first thread, which must start under it too, or the child would run
    /// the second time without its creator's filter (the consultant's B1).
    pub(crate) fn first_seccomp(&self) -> Option<crate::syscall::seccomp::State> {
        self.first_seccomp.lock().clone()
    }

    /// Its threads that have not begun to end.
    pub(crate) fn threads(&self) -> Vec<Arc<Thread>> {
        self.threads
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|thread| !thread.is_gone())
            .collect()
    }

    /// Its thread numbered `tid`, if that thread has not begun to end.
    pub(crate) fn thread_by_tid(&self, tid: u32) -> Option<Arc<Thread>> {
        self.threads()
            .into_iter()
            .find(|thread| thread.tid() == tid)
    }

    /// List `thread` as one of its own, if it is not already: before the
    /// thread can run, and for a fork child before the child is published.
    pub(crate) fn add_thread(&self, thread: &Arc<Thread>) {
        let mut threads = self.threads.lock();
        threads.retain(|listed| listed.strong_count() > 0);
        if !threads
            .iter()
            .any(|listed| core::ptr::eq(listed.as_ptr(), Arc::as_ptr(thread)))
        {
            threads.push(Arc::downgrade(thread));
        }
    }

    /// The thread a signal sent to the process is judged for: the caller when
    /// the caller is one of its threads, and its first otherwise.
    fn signal_taker(&self) -> Option<Arc<Thread>> {
        thread::current_of(self).or_else(|| self.threads().into_iter().next())
    }

    /// Record `signal` sent to it as a whole, or decide it needs no recording:
    /// [`Signals::post`] under its signal lock, once what the signal cancels
    /// is gone from every queue.
    ///
    /// Ignoring is judged against its first thread's mask -- or, with that
    /// thread gone, the first still live -- as Linux judges it against the
    /// task the signal is sent to; a fatal default against every live
    /// thread's, since any one that does not block it would take it. A process
    /// with no thread yet -- a native child before `process_start` -- is judged
    /// against no mask, which is the truth: nothing in it can have blocked
    /// anything.
    pub(crate) fn post_signal(&self, signal: u32, origin: Origin) -> Posted {
        self.post(None, signal, origin)
    }

    /// Record `signal` sent to `thread` alone, as `tkill`, `tgkill` and a
    /// broken pipe send one: into the thread's own queue, judged against its
    /// mask alone, once what the signal cancels is gone from every queue.
    pub(crate) fn post_signal_to(&self, thread: &Thread, signal: u32, origin: Origin) -> Posted {
        self.post(Some(thread), signal, origin)
    }

    /// [`Process::post_signal`] without a `target`, and
    /// [`Process::post_signal_to`] with one. The threads are listed before the
    /// signal lock is taken, because the thread list's lock comes first.
    ///
    /// A signal recorded is posted as `SIGNAL` here, with the record: to the
    /// thread it was sent to, or, sent to the process, to every thread that
    /// does not block it. Whichever of those comes back through the kernel
    /// first takes it, as when every thread's way out read the pending set;
    /// only the one [`Process::notify_signal`] gives it to is woken.
    fn post(&self, target: Option<&Thread>, signal: u32, origin: Origin) -> Posted {
        let _posting = sched::work::posting();
        let posted = self.record_signal(target, signal, origin);
        if posted == Posted::Pending {
            for task in &self.tasks() {
                let Some(thread) = thread::of_task(task) else {
                    continue;
                };
                let takes = match target {
                    Some(target) => core::ptr::eq(thread, target),
                    None => !thread.is_gone() && !thread.blocks(signal),
                };
                if takes {
                    sched::work::post(task, sched::work::SIGNAL);
                }
            }
        }
        posted
    }

    /// The record [`Process::post`] makes, under the signal lock.
    fn record_signal(&self, target: Option<&Thread>, signal: u32, origin: Origin) -> Posted {
        let threads = self.threads();
        let first = threads
            .iter()
            .find(|thread| thread.tid() == self.pid())
            .or_else(|| threads.first());
        self.with_signals(|signals| {
            signals.cancel(signal);
            let mut blocked = 0;
            let mut blocked_everywhere = if threads.is_empty() { 0 } else { u64::MAX };
            for thread in &threads {
                let mask = thread.with_own_signals(|own| {
                    own.cancel(signal);
                    own.blocked()
                });
                blocked_everywhere &= mask;
                if first.is_some_and(|first| Arc::ptr_eq(first, thread)) {
                    blocked = mask;
                }
            }
            match target {
                None => signals.post(blocked, blocked_everywhere, signal, origin),
                Some(thread) => thread.with_own_signals(|own| own.post(signals, signal, origin)),
            }
        })
    }

    /// Whether it is stopped.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire) != 0
    }

    /// Whether its threads are to wait on their way back to user mode: it is
    /// stopped, or its cgroup is frozen. A freeze is a stop that `SIGCONT`
    /// does not undo, that no parent is told of, and that a `SIGKILL` ends
    /// as it ends a stop.
    pub(crate) fn must_park(&self) -> bool {
        self.is_stopped() || self.core().is_frozen()
    }

    /// Look at its cgroup's freeze again: after it was made, moved, or its
    /// cgroup's `cgroup.freeze` changed. A frozen process has `sched::work::STOP` posted
    /// to every task, its threads interrupted as a stop has them, so that
    /// they park; a thawed one has them released.
    ///
    /// Both are done even when the freeze did not change here: a move
    /// (`Job::adopt`) writes the freeze before this looks, so a process moved
    /// into a frozen cgroup is posted to, and one moved out of it, its threads
    /// parked, is released. A thread already parked takes an extra post as
    /// one more look, and a release with nobody parked wakes nobody. The
    /// caller of a move holds a `sched::work::posting` across both, so that
    /// the freeze is never written outside one (`sched::work::audit`).
    pub(crate) fn freeze_sync(&self) {
        let posting = sched::work::posting();
        let (frozen, _changed) = self.core().sync_freeze();
        if frozen {
            self.signalled.wake_all();
            // The caller's own task too, when it is one of this process's
            // threads (`echo $$ > cgroup.procs`): it parks on its way back.
            if thread::current_of(self).is_some() {
                sched::work::post_own(sched::work::STOP);
            }
            self.wake_other_tasks(sched::work::STOP);
        } else {
            self.resumed.wake_all();
        }
        drop(posting);
    }

    /// A thread of it is parked by a freeze.
    pub(crate) fn thread_parked(&self) {
        let _ = self.parked.fetch_add(1, Ordering::AcqRel);
    }

    /// A thread of it is no longer parked.
    pub(crate) fn thread_unparked(&self) {
        let _ = self
            .parked
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |parked| {
                parked.checked_sub(1)
            });
    }

    /// Whether every thread of it that has started and not ended is parked
    /// by a freeze: what a frozen cgroup's `frozen 1` waits for.
    pub(crate) fn is_parked(&self) -> bool {
        self.parked.load(Ordering::Acquire) >= self.live_threads.load(Ordering::Acquire)
    }

    /// The queue woken when it continues or ends.
    pub(crate) fn resumed(&self) -> &WaitQueue {
        &self.resumed
    }

    /// Stop it for `signal`, and tell its parent. Its task waits on the way
    /// back to user mode until [`Process::leave_stop`].
    pub(crate) fn enter_stop(&self, signal: u32) {
        if self.is_terminated() {
            return;
        }
        let posting = sched::work::posting();
        self.stopped.store(signal, Ordering::Release);
        self.continue_report.store(false, Ordering::Release);
        self.stop_report.store(signal, Ordering::Release);
        // Every other thread stops too: its wait ends on the stop, or its
        // program is interrupted, and it waits on its way back to user mode.
        // Linux reports a stop once every thread has parked; here the report
        // goes now, from the thread that took the signal, and a thread still
        // on its way to park runs no user code before it does.
        self.signalled.wake_all();
        // `STOP` to every task, the caller's own among them when it is one of
        // this process's threads, which needs no wake: it is on its way back
        // to user mode, where it parks.
        if thread::current_of(self).is_some() {
            sched::work::post_own(sched::work::STOP);
        }
        self.wake_other_tasks(sched::work::STOP);
        drop(posting);
        kill::tell_parent(self, SIGCHLD, kill::CLD_STOPPED, signal as i32);
    }

    /// Continue it if it is stopped, and tell its parent: what `SIGCONT` does
    /// as it is sent.
    pub(crate) fn leave_stop(&self) {
        if self.stopped.swap(0, Ordering::AcqRel) == 0 {
            return;
        }
        self.stop_report.store(0, Ordering::Release);
        self.continue_report.store(true, Ordering::Release);
        self.resumed.wake_all();
        kill::tell_parent(
            self,
            SIGCHLD,
            kill::CLD_CONTINUED,
            ferrix_linux_abi::types::SIGCONT as i32,
        );
    }

    /// A child `select` accepts with a stop (when `stops`) or a continue (when
    /// `continues`) its parent has not been told of, and the signal that
    /// stopped it -- zero for a continue. The report is taken when `consume`.
    pub(crate) fn changed_child(
        &self,
        select: &dyn Fn(&Process) -> bool,
        stops: bool,
        continues: bool,
        consume: bool,
    ) -> Option<(Arc<Process>, u32)> {
        let children = self.children.lock();
        children
            .iter()
            .filter(|child| select(child))
            .find_map(|child| {
                let stop = match (stops, consume) {
                    (false, _) => 0,
                    (true, true) => child.stop_report.swap(0, Ordering::AcqRel),
                    (true, false) => child.stop_report.load(Ordering::Acquire),
                };
                let continued = match (continues, consume) {
                    (false, _) => false,
                    _ if stop != 0 => false,
                    (true, true) => child.continue_report.swap(false, Ordering::AcqRel),
                    (true, false) => child.continue_report.load(Ordering::Acquire),
                };
                (stop != 0 || continued).then(|| (Arc::clone(child), stop))
            })
    }

    /// The number `viewer` calls its parent by, or zero when it has none or
    /// the viewer cannot see it: a process the kernel started, one whose
    /// parent has ended, and a namespace's init.
    pub(crate) fn parent_pid_in(&self, viewer: &Process) -> u32 {
        self.parent
            .lock()
            .upgrade()
            .map_or(0, |parent| pidns::to_user(viewer, &parent))
    }

    /// Its process group, as a kernel number.
    pub(crate) fn pgid(&self) -> u32 {
        self.pgid.load(Ordering::Acquire)
    }

    /// The number `viewer` calls its process group by; zero for one led from
    /// outside the viewer's namespace.
    pub(crate) fn pgid_in(&self, viewer: &Process) -> u32 {
        let record = self.groups.lock().pgrp.clone();
        pidns::group_to_user(viewer, record.as_ref(), self.pgid())
    }

    /// The numbers of its process group, to give to a process joining it.
    pub(crate) fn group_record(&self) -> Option<Arc<Numbers>> {
        self.groups.lock().pgrp.clone()
    }

    /// The numbers of its session.
    pub(crate) fn session_record(&self) -> Option<Arc<Numbers>> {
        self.groups.lock().session.clone()
    }

    /// Move it into process group `pgid`, whose numbers are `record`.
    pub(crate) fn set_pgid(&self, pgid: u32, record: Option<Arc<Numbers>>) {
        self.groups.lock().pgrp = record;
        self.pgid.store(pgid, Ordering::Release);
    }

    /// Its session, as a kernel number.
    pub(crate) fn sid(&self) -> u32 {
        self.sid.load(Ordering::Acquire)
    }

    /// The number `viewer` calls its session by; zero for one led from
    /// outside the viewer's namespace.
    pub(crate) fn sid_in(&self, viewer: &Process) -> u32 {
        let record = self.groups.lock().session.clone();
        pidns::group_to_user(viewer, record.as_ref(), self.sid())
    }

    /// Make it the leader of a new session and of a new process group, both
    /// numbered by its pid: what `setsid` does.
    pub(crate) fn lead_new_session(&self) {
        {
            let mut groups = self.groups.lock();
            groups.session = self.pids.clone();
            groups.pgrp = self.pids.clone();
        }
        self.sid.store(self.pid(), Ordering::Release);
        self.pgid.store(self.pid(), Ordering::Release);
    }

    /// Its numbers in the namespaces below the first it is in; `None` in
    /// the first.
    pub(crate) fn numbers(&self) -> Option<&Arc<Numbers>> {
        self.pids.as_ref()
    }

    /// The namespace its children are made in; `None` for the first.
    pub(crate) fn children_namespace(&self) -> Option<Arc<PidNamespace>> {
        self.pid_for_children.lock().clone().or_else(|| {
            self.pids
                .as_ref()
                .map(|numbers| Arc::clone(numbers.namespace()))
        })
    }

    /// Whether `unshare(CLONE_NEWPID)` has given its children a namespace
    /// that is not its own: Linux's `pid_ns_for_children` differing from the
    /// task's active namespace, which makes another `CLONE_NEWPID` `EINVAL`.
    pub(crate) fn children_in_other_namespace(&self) -> bool {
        self.pid_for_children.lock().is_some()
    }

    /// Number a process that is made and not yet shared in `namespace` and
    /// every one above it, and make it the leader of its own group and
    /// session there: a native child made by a creator that is in a pid
    /// namespace (`docs/PIDNS.md` §4).
    ///
    /// # Errors
    ///
    /// `ENOMEM` for a namespace that is ending, for the job's memory, and for
    /// the maps' nodes.
    pub(crate) fn enter_pid_namespace(
        &mut self,
        namespace: &Arc<PidNamespace>,
    ) -> Result<(), Errno> {
        if self.pid() == 0 {
            return Ok(());
        }
        let numbers = pidns::assign(namespace, self.pid())?;
        self.groups = SpinLock::new(Groups {
            pgrp: Some(Arc::clone(&numbers)),
            session: Some(Arc::clone(&numbers)),
        });
        self.pids = Some(numbers);
        Ok(())
    }

    /// Have its later children made in `namespace` (`unshare(CLONE_NEWPID)`).
    pub(crate) fn set_children_namespace(&self, namespace: Arc<PidNamespace>) {
        let displaced = self.pid_for_children.lock().replace(namespace);
        drop(displaced);
    }

    /// The signal its parent is told with when it ends.
    pub(crate) fn set_exit_signal(&self, signal: u32) {
        self.exit_signal.store(signal, Ordering::Release);
    }

    /// Take `child` into its list of children.
    pub(crate) fn adopt(&self, child: Arc<Process>) {
        self.children.lock().push(child);
    }

    /// Let `child` go from its list of children, if it is there.
    ///
    /// Reaped, so its tasks leave its job's `pids` count now.
    pub(crate) fn disown(&self, child: &Process) {
        self.children
            .lock()
            .retain(|held| !core::ptr::eq(Arc::as_ptr(held), child));
        child.uncharge_tasks();
    }

    /// Whether `pid` is one of its children, ended or not.
    pub(crate) fn has_child(&self, pid: u32) -> bool {
        self.children.lock().iter().any(|child| child.pid() == pid)
    }

    /// Whether it has been reaped: released, and in no parent's list of
    /// children, so no wait can take it any more. Asked by identity, not by
    /// pid, since its pid may already name another process.
    pub(crate) fn is_reaped(&self) -> bool {
        self.is_released()
            && !self.parent().is_some_and(|parent| {
                parent
                    .children
                    .lock()
                    .iter()
                    .any(|child| core::ptr::eq(Arc::as_ptr(child), self))
            })
    }

    /// A child `select` accepts that has ended, taken out of the list when
    /// `remove` is set.
    ///
    /// # Errors
    ///
    /// `ECHILD` if no child at all is one `select` accepts, ended or not: the
    /// difference between "wait longer" and "there is nothing to wait for".
    pub(crate) fn reap_child(
        &self,
        select: &dyn Fn(&Process) -> bool,
        remove: bool,
    ) -> Result<Option<Arc<Process>>, Errno> {
        let mut children = self.children.lock();
        let mut any = false;
        let mut ended = None;
        for (at, child) in children.iter().enumerate() {
            if !select(child) {
                continue;
            }
            any = true;
            if child.is_released() {
                ended = Some(at);
                break;
            }
        }
        match ended {
            Some(at) if remove => {
                let reaped = children.remove(at);
                // Reaped: its tasks leave its job's `pids` count here, as
                // Linux's `release_task` uncharges them.
                reaped.uncharge_tasks();
                Ok(Some(reaped))
            }
            Some(at) => Ok(children.get(at).map(Arc::clone)),
            None if any => Ok(None),
            None => Err(Errno::ECHILD),
        }
    }

    /// Whether a child `select` accepts has ended: a `wait4` sleeper's
    /// condition.
    pub(crate) fn has_ended_child(&self, select: &dyn Fn(&Process) -> bool) -> bool {
        self.children
            .lock()
            .iter()
            .any(|child| select(child) && child.is_released())
    }

    /// The queue woken whenever one of its children ends.
    pub(crate) fn child_exited(&self) -> &WaitQueue {
        &self.child_exited
    }

    /// The status word `wait4` reports for it once it has ended: the exit
    /// code in the second byte, or the signal that ended it in the low seven
    /// bits.
    pub(crate) fn wait_status(&self) -> Option<i32> {
        let status = self.exit_status()?;
        Some(match self.exit().signal() {
            None => (status & 0xFF) << 8,
            Some(signal) => (signal & 0x7F) as i32,
        })
    }

    /// The signal that ended it, if a signal did.
    pub(crate) fn ended_by_signal(&self) -> Option<u32> {
        self.exit().signal()
    }

    /// Record a successful `execve`, releasing a `vfork` parent.
    pub(crate) fn mark_execed(&self) {
        // A bootstrap not given by now never will be; one given stays for
        // the new program to take (`process_give`, `docs/INIT.md` §6).
        self.core.seal_bootstrap();
        self.execed.store(true, Ordering::Release);
        self.vfork_done.wake_all();
    }

    /// Block `caller` until this `vfork` child has called `execve` or ended,
    /// which is what `vfork` promises its parent.
    pub(crate) fn wait_vfork_release(&self, caller: &Process) {
        let _ = self.vfork_done.wait_until_deadline(
            || {
                self.execed.load(Ordering::Acquire)
                    || self.is_terminated()
                    || caller.caller_must_leave()
            },
            u64::MAX,
        );
    }
}

impl core::ops::Deref for Process {
    type Target = object::process::Process;

    /// The core process inside it: see the field.
    fn deref(&self) -> &object::process::Process {
        &self.core
    }
}

impl Host for Process {
    fn core(&self) -> &object::process::Process {
        &self.core
    }

    fn core_arc(&self) -> &Arc<object::process::Process> {
        &self.core
    }

    fn kill(&self, status: i32) {
        kill(self, status);
    }

    fn thread_starting(&self) {
        Process::thread_starting(self);
    }

    fn thread_gone(&self, ended: bool) {
        Process::thread_gone(self, ended);
    }

    fn wait_interrupted(&self) -> bool {
        self.signal_pending()
    }
}

/// This personality's process behind `host`, if it is one.
///
/// What a handler registered with the item gets its own type back with: the
/// item hands it the caller as a [`Host`], which names nothing of POSIX.
pub(crate) fn of_host(host: &dyn Host) -> Option<&Process> {
    let any: &dyn Any = host;
    any.downcast_ref()
}

/// The process the running task belongs to.
///
/// `None` for a kernel thread.
pub(crate) fn current() -> Option<Arc<Process>> {
    // Through the thread rather than through a `Task::process` accessor: the
    // scheduler is core and `Process` is the Linux personality's, so the core
    // should not carry a way to name one. The thread is what owns the process
    // anyway -- `Thread::process` is the real relationship.
    // The task lent (`sched::with_current`), not cloned: only the process,
    // which the caller keeps, is counted (po10-pipe P2, B1).
    sched::with_current(|task| thread::of_task(task).map(|thread| Arc::clone(thread.process())))
        .flatten()
}

/// Load a program into a new process, without running it.
pub(crate) use super::exec::load;

/// Run `process`'s program as a task of its own.
///
/// Separate from [`load`] so that something can be put into the process
/// between the two -- a handle, say -- before its first instruction runs.
///
/// # Errors
///
/// If no program was loaded into it, if it has already been started, or if the
/// scheduler has no stack for its task.
pub(crate) fn start(process: &Arc<Process>) -> Result<Arc<Task>, &'static str> {
    start_on(process, None)
}

/// [`start`], pinned to processor `cpu` when that is `Some`.
///
/// # Errors
///
/// As [`start`].
pub(crate) fn start_on(
    process: &Arc<Process>,
    cpu: Option<usize>,
) -> Result<Arc<Task>, &'static str> {
    let claim = claim_start(process)?;
    let thread = Thread::leader(process)
        .and_then(fallible::try_arc)
        .map_err(|_| "no memory for the process's first thread")?;
    Ok(claim.prepare_thread(thread, cpu, None)?.launch())
}

/// The right to start a process, held by one starter at a time.
///
/// Taken before anything is put into the process that its program will find on
/// entry -- a native starter's bootstrap handle, and the start argument that
/// names it -- because two starts can race. With the claim taken first, a
/// second `process_start` is refused before it has moved a handle into a table
/// the child may already be reading, or overwritten the argument the first
/// start's task has yet to read. Dropped without starting, it gives the start
/// back, so a starter whose own preparation failed leaves the process
/// startable.
#[must_use = "dropping a claim gives the start back"]
pub(crate) struct StartClaim {
    /// The process it may start.
    process: Arc<Process>,
    /// Set once its task is spawned, after which the start is not given back.
    spent: bool,
}

/// Claim the start of `process`, which must have a program loaded and must not
/// have ended.
///
/// An ended process is refused with the claim already taken, so a kill that
/// lands after this answers finds a claim a starter holds, and the task that
/// start spawns returns before entering the program. Without the refusal, a
/// native `process_start` on a child killed before its start would spawn that
/// task and report a start that never ran anything.
///
/// # Errors
///
/// If no program was loaded into it, it has already ended, or it has already
/// been claimed or started.
pub(crate) fn claim_start(process: &Arc<Process>) -> Result<StartClaim, &'static str> {
    if process.startup().is_none() {
        return Err("the process has no program loaded");
    }
    let claim = take_claim(process)?;
    if process.is_terminated() {
        // Dropping the claim gives the start back, which no one can use now.
        return Err("the process has already ended");
    }
    Ok(claim)
}

/// Take the claim whether or not a program is loaded: a fork child resumes from
/// its parent's registers instead.
fn take_claim(process: &Arc<Process>) -> Result<StartClaim, &'static str> {
    process
        .start_claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| "the process has already started")?;
    Ok(StartClaim {
        process: Arc::clone(process),
        spent: false,
    })
}

impl StartClaim {
    /// Start the process with `argument` in its first argument register.
    ///
    /// # Errors
    ///
    /// As [`StartClaim::prepare`]; the start is given back.
    pub(crate) fn start(
        self,
        cpu: Option<usize>,
        argument: u64,
    ) -> Result<Arc<Task>, &'static str> {
        Ok(self.prepare(cpu)?.start(argument))
    }

    /// Make the process's first task without running it: everything about the
    /// start that can fail, so that what is put into the process afterwards --
    /// a native starter's bootstrap handle -- is never put there by a start
    /// that then fails.
    ///
    /// # Errors
    ///
    /// If the process has no program loaded, or the scheduler has no processor
    /// or stack for its task; the start is given back either way.
    pub(crate) fn prepare(self, cpu: Option<usize>) -> Result<PreparedStart, &'static str> {
        if self.process.startup().is_none() {
            return Err("the process has no program loaded");
        }
        let thread = Thread::leader(&self.process)
            .and_then(fallible::try_arc)
            .map_err(|_| "no memory for the process's first thread")?;
        self.prepare_thread(thread, cpu, None)
    }

    /// Make the process's first task, for `thread`, one of its own: entering
    /// its program, or with `state` resuming a fork child with its parent's
    /// thread pointer and floating-point registers.
    fn prepare_thread(
        self,
        thread: Arc<Thread>,
        cpu: Option<usize>,
        state: Option<crate::arch::UserState>,
    ) -> Result<PreparedStart, &'static str> {
        self.process.add_thread(&thread);
        let task = sched::prepare_user("user", run_program, thread, cpu, state)?;
        // Listed before it can run, so that an end recorded from now on
        // reaches it (`object::process::Process::end_record`).
        self.process
            .core()
            .list_task(task.task())
            .map_err(|_| "no memory to list the process's first task")?;
        Ok(PreparedStart { task, claim: self })
    }
}

/// A process's first task, made and counted but not yet run, with the claim on
/// its start.
///
/// [`PreparedStart::start`] runs it and cannot fail. Dropped instead, the task
/// is freed and its count given back first, then the start, leaving the process
/// startable; as for [`sched::PreparedTask`], that frees a kernel stack, so it
/// must be dropped in task context with no lock held.
#[must_use = "dropping a prepared start frees its task and gives the start back"]
pub(crate) struct PreparedStart {
    /// Declared first, so dropped first: the task goes before the claim.
    task: sched::PreparedTask,
    /// The claim, spent when the task is launched.
    claim: StartClaim,
}

impl PreparedStart {
    /// Run the process with `argument` in its first argument register.
    ///
    /// The argument is written while the claim is held, so no other start can
    /// change it between here and the task reading it.
    pub(crate) fn start(self, argument: u64) -> Arc<Task> {
        if let Some(startup) = self.claim.process.startup.lock().as_mut() {
            startup.argument = argument;
        }
        self.launch()
    }

    /// Put the task on its queue and spend the claim.
    fn launch(self) -> Arc<Task> {
        let PreparedStart { task, mut claim } = self;
        let posting = sched::work::posting();
        let task = task.launch();
        tell_a_new_task(&claim.process, &task);
        drop(posting);
        claim.spent = true;
        task
    }
}

impl Drop for StartClaim {
    /// Give the start back, unless the task was spawned.
    fn drop(&mut self) {
        if !self.spent {
            self.process.start_claimed.store(false, Ordering::Release);
        }
    }
}

/// Run a fork child's first thread: its process resumes from the registers
/// [`Thread::set_resume`] gave it, with `state` -- its parent's thread pointer
/// and floating-point registers, as they were -- loaded when it is first
/// switched to.
///
/// # Errors
///
/// As [`start`].
pub(crate) fn start_forked(
    thread: Arc<Thread>,
    state: crate::arch::UserState,
) -> Result<Arc<Task>, &'static str> {
    let claim = take_claim(thread.process())?;
    Ok(claim.prepare_thread(thread, None, Some(state))?.launch())
}

/// Run a thread `clone` made in a process already running: it resumes from the
/// registers [`Thread::set_resume`] gave it, with `state` -- the caller's
/// thread pointer, or the one `CLONE_SETTLS` asked for, and its floating-point
/// registers -- loaded when it is first switched to.
///
/// A process that has begun to end is refused. One that begins to end after
/// the test starts the thread anyway, and the thread leaves at once, on its
/// way into the program, as every thread of an ending process does.
///
/// # Errors
///
/// If the process is ending, or the scheduler has no stack for its task.
pub(crate) fn start_thread(
    thread: Arc<Thread>,
    state: crate::arch::UserState,
) -> Result<Arc<Task>, &'static str> {
    let process = Arc::clone(thread.process());
    if process.is_terminated() || process.exec_thread.load(Ordering::Acquire) != 0 {
        return Err("the process is ending, or another thread is replacing its program");
    }
    process.add_thread(&thread);
    let prepared = sched::prepare_user("thread", run_program, thread, None, Some(state))?;
    // Listed before it can run, as a process's first task is.
    process
        .core()
        .list_task(prepared.task())
        .map_err(|_| "no memory to list the thread's task")?;
    let posting = sched::work::posting();
    let task = prepared.launch();
    // At the weight the rest of the process runs at, not at nice 0: a program
    // that was reniced and then started a thread would otherwise take back
    // with every thread what the renice gave away. Linux copies the nice
    // value into the new thread; this reads the same one from the same place.
    attributes::apply_nice(&process, &task);
    tell_a_new_task(&process, &task);
    drop(posting);
    Ok(task)
}

/// Tell `task`, just launched and already listed, what was posted for its
/// process before it was listed: an end the core recorded, whose walk did not
/// find it; a thread replacing the program, which waits for it to leave; and a
/// signal pending for the process that its thread does not block; and a
/// stop or a frozen cgroup, which it parks for. Each is
/// read after the listing, and each poster writes before it walks the list
/// under the same lock, so one of the two sees the other.
fn tell_a_new_task(process: &Process, task: &Arc<Task>) {
    if (process.is_terminated() || process.exec_thread.load(Ordering::Acquire) != 0)
        && !sched::work::has_end(task)
    {
        sched::work::notify(task, sched::work::END);
    }
    // Born into a frozen cgroup, or into a stopped process: it parks before
    // it runs any of the program.
    if process.must_park() {
        sched::work::notify(task, sched::work::STOP);
    }
    if thread::of_task(task).is_some_and(|thread| {
        thread.with_signals(|shared, own| super::signal::needs_attention(shared, own))
    }) {
        sched::work::notify(task, sched::work::SIGNAL);
    }
}

/// End `process` from outside, with `status`.
///
/// Its tasks find out on their way back to user mode: one running there is
/// interrupted to, one blocked in a call is woken to, and one that has not yet
/// entered user mode never does. Nothing here waits for that; a caller
/// that needs the tasks gone waits for them.
pub(crate) fn kill(process: &Process, status: i32) {
    // A status of 128 plus a signal number is how a shell spells death by that
    // signal, and it is how a waiting parent is told: as the signal.
    let signal = if (129..=192).contains(&status) {
        (status - 128) as u32
    } else {
        0
    };
    let _ = process.end(status, signal);
}

/// End the running task's process with `status`, and the task's thread with
/// it: what `exit_group` does.
pub(crate) fn exit_current(status: i32) -> ! {
    end_thread(Some(status), true)
}

/// End the running task's thread with `status`: what `exit` does. Its process
/// ends with it only when no other thread of it is live, with the status its
/// first thread left with.
pub(crate) fn exit_thread_current(status: i32) -> ! {
    end_thread(Some(status), false)
}

/// End the running task's thread with no status of its own: a thread leaving
/// because its process is ending, or one whose program never started.
pub(crate) fn leave_current() -> ! {
    end_thread(None, false)
}

/// End the running task's thread: end its process first when `group` asks;
/// clear and wake the address it asked to have cleared; and count it gone,
/// which ends the process if that was its last thread, or lets go of what the
/// process holds if it was already ending.
///
/// Interrupts are opened first. A thread leaving from the way back to user
/// mode, or from a program killed before it entered, arrives with them masked,
/// and letting go of a process takes plain locks. Every reference is dropped
/// before the task ends, because nothing after `sched::exit` runs to drop it.
fn end_thread(status: Option<i32>, group: bool) -> ! {
    // Before anything here can release the process and wake a waiter: from
    // here to `sched::exit_leaving` the task is ending but not yet exited
    // (FX-0902).
    sched::begin_leaving();
    crate::arch::enable_interrupts();
    if let Some(thread) = thread::current() {
        // First, so that no signal is chosen for a thread on its way out, and
        // then any the process was sent is handed to a thread that stays.
        thread.mark_gone();
        let process = Arc::clone(thread.process());
        process.retarget_shared_signals();
        if let Some(status) = status {
            if thread.tid() == process.pid() {
                process.leader_status.store(status, Ordering::Release);
            }
            if group {
                let _ = process.terminate(status);
            }
        }
        // The address `CLONE_CHILD_CLEARTID` or `set_tid_address` registered
        // is zeroed and its futex woken, which is how a `pthread_join` or a
        // `vfork`ing libc learns the thread is gone. A failure is ignored, as
        // Linux ignores it: the address was the program's to get right.
        let clear_child_tid = thread.take_clear_child_tid();
        if clear_child_tid != 0 {
            let space = process.space();
            let _ = uaccess::copy_to_user(space, clear_child_tid, &0_u32.to_le_bytes());
            let _ = futex::wake_address(space, clear_child_tid, 1);
        }
        drop(thread);
        process.thread_gone(true);
    }
    sched::exit_leaving()
}

/// Where a program's task begins: enter user mode where `exec::load` said.
///
/// Returning ends the task, which is what happens if the process was killed
/// before it ever ran. A check that makes a system call in the process first
/// -- `execve`, in `fs::exec_check` -- enters the program through here after.
pub(crate) fn run_program(_argument: usize) {
    let Some(process) = current() else {
        return;
    };
    // Masked from the test to the entry into user mode. The task starts with
    // interrupts open, and the return-to-user check runs only for traps taken
    // from user mode, so a kill whose interrupt landed between an open test
    // and the entry would be taken here, in kernel mode, and forgotten: the
    // program would enter user mode anyway and, alone on its processor, run
    // until its first system call. Masked, a kill after the test leaves its
    // interrupt pending, and it is taken from user mode on the first
    // instruction, where the check sees it. Entering user mode opens them.
    crate::arch::disable_interrupts();
    if thread::current().is_some_and(|thread| process.must_leave(&thread))
        || process.is_terminated()
    {
        drop(process);
        leave_current();
    }
    let resume = thread::current().and_then(|thread| thread.take_resume());
    if let Some(regs) = resume {
        drop(process);
        // SAFETY: this task was spawned in the child's address space with its
        // parent's user state, both installed by the switch that got here, and
        // `regs` is a copy of the frame the parent's system call saved in that
        // same (forked) space. It lives on this task's own kernel stack, which
        // the resume path requires, and nothing owned is left on this frame.
        unsafe { crate::arch::resume_user(&regs) }
    }
    let startup = process.startup();
    drop(process);
    let Some(Startup {
        entry,
        stack,
        argument,
        abi,
        ..
    }) = startup
    else {
        leave_current();
    };
    // SAFETY: this task was spawned in the process's address space, which the
    // scheduler installed when it switched here, along with the task's user
    // state and entry stack; `entry` and `stack` came from the loader and the
    // stack builder, both inside that space. Nothing owned is left on this
    // frame to leak: the process reference was dropped above.
    unsafe { crate::arch::enter_user(entry, stack, argument, abi) }
}

/// Make a process over a fresh address space, for the self-checks.
///
/// # Errors
///
/// Whatever [`AddressSpace::new`] refuses.
///
/// Its console descriptors read as an open of the console made now does: the
/// checks ask the console's terminal questions through them.
pub(crate) fn new_for_check() -> Result<Arc<Process>, SpaceError> {
    let reading = Reading::Since(fs::terminal::hangups());
    let process =
        Process::reading(AddressSpace::new()?, reading).map_err(|_| SpaceError::OutOfMemory)?;
    Ok(registry::register(process))
}

/// Make a process whose space is a `fork` of `parent`'s, as `fork` makes one
/// but with no task, for the self-checks that need two processes sharing
/// memory.
///
/// # Errors
///
/// As [`AddressSpace::fork`].
pub(crate) fn fork_for_check(parent: &Arc<Process>) -> Result<Arc<Process>, SpaceError> {
    fork_for_check_in(parent, None)
}

/// [`fork_for_check`] into `namespace`, as `clone(CLONE_NEWPID)` makes a
/// child, or where the parent's children go when it is `None`.
pub(crate) fn fork_for_check_in(
    parent: &Arc<Process>,
    namespace: Option<Arc<PidNamespace>>,
) -> Result<Arc<Process>, SpaceError> {
    let child = parent
        .fork_memory(|space| Process::forked_into(parent, space, false, false, None, namespace))?
        .map_err(|_| SpaceError::OutOfMemory)?;
    let child = registry::register_shared(child);
    attributes::inherit(parent, &child);
    Ok(child)
}
