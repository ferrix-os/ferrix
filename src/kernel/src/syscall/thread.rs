//! A thread: the part of a program that runs, as distinct from the program.
//!
//! # What is the thread's and what is the process's
//!
//! A [`Process`] is what a program has: its address space, descriptors,
//! credentials, signal dispositions and children. A [`Thread`] is what one
//! line of execution through it has: its thread id; the address
//! `set_tid_address` or `CLONE_CHILD_CLEARTID` asked to have cleared when that
//! line ends, which is how `pthread_join` learns it has; and its own signal
//! state -- the blocked mask, the alternate stack, the signals sent to it
//! alone and the call it is to restart ([`ThreadSignals`]). Its kernel stack
//! and its saved user registers stay on the scheduler's
//! [`crate::sched::Task`], which holds the thread, as the thread holds its
//! process.
//!
//! # Thread ids
//!
//! A process's first thread, its leader, has the process id for its thread
//! id: what `gettid` answers and what a libc hands `tgkill`. Every other
//! thread, made by `clone` with `CLONE_THREAD`, has a number of its own from
//! the same space, which finds its process too, and gives it back as it goes.
//!
//! # Locks
//!
//! A thread's signal state has a lock of its own, taken inside its process's
//! signal lock when both are needed ([`Thread::with_signals`]) and never the
//! other way round.

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::fallible::AllocError;
use crate::object::process::{self as pids, Host};
use crate::sched::{self, Task, UserThread};
use crate::sync::SpinLock;
use crate::syscall::pidns::Numbers;
use crate::syscall::process::Process;
use crate::syscall::seccomp;
use crate::syscall::signal::{self, Inherited, Signals, ThreadSignals};

/// One line of execution through a process.
#[derive(Debug)]
pub(crate) struct Thread {
    /// Its thread id. A leader's is its process's pid, and zero with it. It
    /// changes once at most: a thread that replaces its process's program
    /// while not the leader takes the pid, as the leader it becomes.
    tid: AtomicU32,
    /// Set as it begins to end, after which a signal is never chosen for it
    /// and its process no longer lists it.
    gone: AtomicBool,
    /// Its numbers in the pid namespaces below the first, when it is a thread
    /// beside its process's first and its process is in one. The first
    /// thread's are the process's ([`Thread::numbers`]).
    numbers: SpinLock<Option<Arc<Numbers>>>,
    /// The program it runs. Holding it is what keeps the process alive while
    /// the thread is: a process does not own its threads, its threads own it.
    process: Arc<Process>,
    /// The address `set_tid_address` or `CLONE_CHILD_CLEARTID` registered, to
    /// be zeroed and woken when the thread ends. Zero means none.
    clear_child_tid: AtomicU64,
    /// The head of its robust futex list, as `set_robust_list` registered it;
    /// zero for none. Every thread starts with none, a fork child's and a
    /// `clone` sibling's too, as Linux's `copy_process` clears it.
    robust_list: AtomicU64,
    /// Set while it waits in `vfork` for its child to let go of the address
    /// space, which `/proc` shows as `D`, as Linux does.
    in_vfork: AtomicBool,
    /// Its own signal state.
    signals: SpinLock<ThreadSignals>,
    /// Registers its task resumes from instead of entering the program: a
    /// fork child's first thread, and every thread `clone` makes. Taken once.
    resume: SpinLock<Option<crate::arch::UserRegs>>,
    /// Its seccomp mode and filter chain, which Linux keeps per thread
    /// (`task_struct.seccomp`). A leaf lock: nothing is taken inside it, and
    /// neither a filter nor the memory of a dropped chain is ever run or freed
    /// under it (`docs/SECCOMP.md` §7).
    seccomp: SpinLock<seccomp::State>,
    /// Whether [`Thread::seccomp`] holds anything, for the system call entry's
    /// fast path: written under that lock after the state it summarises, with
    /// release, and read with acquire. A thread that has never been filtered
    /// pays this load and nothing else.
    filtered: AtomicBool,
}

impl Thread {
    /// The first thread of `process`, numbered by its pid, blocking nothing.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for its signal state; this and
    /// the two below are on paths that answer `ENOMEM` or `NO_MEMORY`.
    pub(crate) fn leader(process: &Arc<Process>) -> Result<Thread, AllocError> {
        // A native child of a filtered creator starts with its creator's chain
        // (`seccomp::inherit_native`), however many times its start is made;
        // any other first thread starts with none.
        let seccomp = process.first_seccomp().unwrap_or_default();
        Ok(Thread::with(
            process,
            ThreadSignals::new(Inherited::NONE)?,
            seccomp,
        ))
    }

    /// The first thread of a fork child `process`, made by `parent`: it
    /// inherits the parent's blocked mask and alternate stack, and nothing the
    /// parent was sent or was in the middle of.
    ///
    /// # Errors
    ///
    /// As [`Thread::leader`].
    pub(crate) fn forked(process: &Arc<Process>, parent: &Thread) -> Result<Thread, AllocError> {
        let inherited = parent.with_own_signals(|signals| signals.inherited());
        // The parent's mode and chain, shared: a child never escapes a filter
        // by being forked.
        Ok(Thread::with(
            process,
            ThreadSignals::new(inherited)?,
            parent.seccomp_copy(),
        ))
    }

    /// [`Thread::sibling_numbered`] for a process in the first namespace,
    /// which has no numbers to give it.
    ///
    /// # Errors
    ///
    /// As [`Thread::leader`].
    pub(crate) fn sibling(
        process: &Arc<Process>,
        tid: u32,
        caller: &Thread,
    ) -> Result<Thread, AllocError> {
        Thread::sibling_numbered(process, tid, None, caller)
    }

    /// A thread of `process` other than its first, numbered `tid`, made by
    /// `caller`: with the caller's blocked mask, no alternate stack and nothing
    /// pending, as Linux's `copy_process` makes a `CLONE_THREAD` child.
    ///
    /// # Errors
    ///
    /// As [`Thread::leader`]. `tid` is then still the caller's to give back.
    pub(crate) fn sibling_numbered(
        process: &Arc<Process>,
        tid: u32,
        numbers: Option<Arc<Numbers>>,
        caller: &Thread,
    ) -> Result<Thread, AllocError> {
        let inherited = caller
            .with_own_signals(|signals| signals.inherited())
            .without_alt_stack();
        let mut thread = Thread::with(
            process,
            ThreadSignals::new(inherited)?,
            seccomp::State::default(),
        );
        *thread.tid.get_mut() = tid;
        *thread.numbers.get_mut() = numbers;
        Ok(thread)
    }

    /// The first thread of `process`, numbered by its pid, with `signals` and
    /// `seccomp`.
    fn with(process: &Arc<Process>, signals: ThreadSignals, seccomp: seccomp::State) -> Thread {
        let filtered = AtomicBool::new(seccomp.is_active());
        Thread {
            tid: AtomicU32::new(process.pid()),
            gone: AtomicBool::new(false),
            numbers: SpinLock::new(None),
            process: Arc::clone(process),
            clear_child_tid: AtomicU64::new(0),
            robust_list: AtomicU64::new(0),
            in_vfork: AtomicBool::new(false),
            signals: SpinLock::new(signals),
            resume: SpinLock::new(None),
            seccomp: SpinLock::new(seccomp),
            filtered,
        }
    }

    /// Whether it is under seccomp at all: the load the system call entry
    /// makes on every call.
    pub(crate) fn is_filtered(&self) -> bool {
        self.filtered.load(Ordering::Acquire)
    }

    /// Read or change its seccomp state, under its leaf lock, and publish
    /// whether it holds anything afterwards.
    ///
    /// A closure for the reason [`Thread::with_own_signals`] is one. What it
    /// returns is returned after the lock is released, so a chain it swapped
    /// out is dropped by the caller and not under the lock.
    pub(crate) fn with_seccomp<R>(&self, change: impl FnOnce(&mut seccomp::State) -> R) -> R {
        let mut held = self.seccomp.lock();
        let answer = change(&mut held);
        let active = held.is_active();
        // Written only on change: the flag is read on every call of a filtered
        // thread, and a store each time would bounce its cache line.
        if active != self.filtered.load(Ordering::Relaxed) {
            if active {
                seccomp::note_filtered();
            }
            self.filtered.store(active, Ordering::Release);
        }
        answer
    }

    /// Read its seccomp state under its leaf lock, changing nothing and
    /// storing nothing: what every call of a filtered thread does.
    pub(crate) fn read_seccomp<R>(&self, read: impl FnOnce(&seccomp::State) -> R) -> R {
        read(&self.seccomp.lock())
    }

    /// A copy of its seccomp state for a thread or process made of it: the
    /// mode, and the chain shared, one reference more.
    pub(crate) fn seccomp_copy(&self) -> seccomp::State {
        self.seccomp.lock().clone()
    }

    /// Have its task resume from `regs` rather than enter the program.
    pub(crate) fn set_resume(&self, regs: crate::arch::UserRegs) {
        *self.resume.lock() = Some(regs);
    }

    /// The registers to resume from, once.
    pub(crate) fn take_resume(&self) -> Option<crate::arch::UserRegs> {
        self.resume.lock().take()
    }

    /// Its thread id; zero if its process was made with every pid in use.
    pub(crate) fn tid(&self) -> u32 {
        self.tid.load(Ordering::Acquire)
    }

    /// Take `pid` as its thread id, as a thread that replaced its process's
    /// program while not the first does, becoming the leader. Answers the id
    /// it had, which its process then gives back.
    pub(crate) fn take_pid(&self, pid: u32) -> u32 {
        // Its own numbers go with the id it gives up; the process's are its
        // now. Dropped with no lock held.
        let given_up = self.numbers.lock().take();
        drop(given_up);
        self.tid.swap(pid, Ordering::AcqRel)
    }

    /// Its numbers in the pid namespaces below the first it is in: its
    /// process's for the first thread, its own for another; `None` in the
    /// first namespace.
    pub(crate) fn numbers(&self) -> Option<Arc<Numbers>> {
        if self.tid() == self.process.pid() {
            return self.process.numbers().cloned();
        }
        self.numbers.lock().clone()
    }

    /// Whether it has begun to end.
    pub(crate) fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// Record that it has begun to end: from here on its process neither lists
    /// it nor chooses it to take a signal.
    pub(crate) fn mark_gone(&self) {
        self.gone.store(true, Ordering::Release);
    }

    /// Whether it is waiting in `vfork` for its child to let go.
    pub(crate) fn in_vfork(&self) -> bool {
        self.in_vfork.load(Ordering::Acquire)
    }

    /// Record that it has begun (`true`) or ended (`false`) that wait.
    pub(crate) fn set_in_vfork(&self, waiting: bool) {
        self.in_vfork.store(waiting, Ordering::Release);
    }

    /// The process it runs.
    pub(crate) fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Record the address to clear when it ends, and report its thread id,
    /// which is what `set_tid_address` returns.
    ///
    /// musl uses the *return value* as its process id during startup, so this
    /// must answer with a real identifier. It is one of the few calls where a
    /// plausible-looking stub is worse than an error: an `ENOSYS` musl
    /// survives, a wrong pid it does not.
    pub(crate) fn set_clear_child_tid(&self, address: u64) -> u32 {
        self.clear_child_tid.store(address, Ordering::Release);
        self.tid()
    }

    /// The address registered to be cleared, or zero.
    pub(crate) fn clear_child_tid(&self) -> u64 {
        self.clear_child_tid.load(Ordering::Acquire)
    }

    /// Take the address registered to be cleared, leaving none: once, when
    /// the thread ends or its program is replaced.
    pub(crate) fn take_clear_child_tid(&self) -> u64 {
        self.clear_child_tid.swap(0, Ordering::AcqRel)
    }

    /// Record the head of its robust futex list; zero forgets it.
    pub(crate) fn set_robust_list(&self, head: u64) {
        self.robust_list.store(head, Ordering::Release);
    }

    /// The head of its robust futex list, or zero.
    pub(crate) fn robust_list(&self) -> u64 {
        self.robust_list.load(Ordering::Acquire)
    }

    /// Read or change its own signal state, under its lock.
    ///
    /// A closure rather than a guard, as for [`Process::with_signals`]: every
    /// copy to or from the program is outside it.
    pub(crate) fn with_own_signals<R>(&self, change: impl FnOnce(&mut ThreadSignals) -> R) -> R {
        change(&mut self.signals.lock())
    }

    /// Read or change its process's signal state and its own together: the
    /// process's lock first, then its own.
    pub(crate) fn with_signals<R>(
        &self,
        change: impl FnOnce(&mut Signals, &mut ThreadSignals) -> R,
    ) -> R {
        self.process
            .with_signals(|shared| change(shared, &mut self.signals.lock()))
    }

    /// Whether it blocks `signal`.
    pub(crate) fn blocks(&self, signal: u32) -> bool {
        self.with_own_signals(|own| own.blocked() & signal::bit(signal) != 0)
    }

    /// Whether a wait it is in should end: its process has ended or stopped,
    /// another thread is replacing the program, or a signal it does not block
    /// is pending for it or for its process.
    ///
    /// A stop ends a wait so that the thread stops with the rest, as Linux's
    /// group stop wakes every thread: one blocked reading a pipe must not take
    /// input while its process is reported stopped. The waits answer restart
    /// codes, and no handler runs for a stop, so the call resumes after
    /// `SIGCONT` as if it had never left.
    pub(crate) fn signal_pending(&self) -> bool {
        self.process.must_leave(self)
            || self.process.is_stopped()
            || self.with_signals(|shared, own| signal::deliverable(shared, own) != 0)
    }
}

impl UserThread for Thread {
    fn process(&self) -> &dyn Host {
        &*self.process
    }
}

impl Drop for Thread {
    /// Give its thread id back, unless it is its process's first, whose id is
    /// the pid and goes with the process.
    fn drop(&mut self) {
        let tid = *self.tid.get_mut();
        if tid != 0 && tid != self.process.pid() {
            pids::release_naming(tid, &*self.process);
            // The task `allocate_thread` charged with the id.
            self.process.uncharge_thread();
        }
    }
}

/// The thread the running task runs, or `None` for a kernel thread.
pub(crate) fn current() -> Option<Arc<Thread>> {
    let task = sched::current()?;
    let thread: Arc<dyn UserThread> = Arc::clone(task.thread()?);
    let thread: Arc<dyn Any + Send + Sync> = thread;
    thread.downcast().ok()
}

/// The thread `task` runs, if it runs one of this personality's.
///
/// The scheduler holds a task's thread as its own [`UserThread`], which knows
/// nothing of signals or thread ids; this is the way back to the rest.
pub(crate) fn of_task(task: &Task) -> Option<&Thread> {
    let thread: &dyn Any = &**task.thread()?;
    thread.downcast_ref()
}

/// The running task's thread, if it is one of `process`'s.
pub(crate) fn current_of(process: &Process) -> Option<Arc<Thread>> {
    current().filter(|thread| core::ptr::eq(thread.process().as_ref(), process))
}
