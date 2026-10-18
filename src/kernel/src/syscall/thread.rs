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
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::fallible::AllocError;
use crate::object::process::{self as pids, Host};
use crate::sched::{self, Task, UserThread};
use crate::sync::SpinLock;
use crate::syscall::pidns::Numbers;
use crate::syscall::process::Process;
use crate::syscall::seccomp;
use crate::syscall::seccomp::user_dispatch::Dispatch;
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
    /// What the system call entry must look at for it, one bit for each:
    /// [`WATCH_SECCOMP`] while [`Thread::seccomp`] holds anything. Each bit is
    /// changed only under the lock of the state it summarises, after that
    /// state, with release, and the mask is read with acquire. Each bit set is
    /// one count of `seccomp::FILTERED_THREADS`, raised before the bit and
    /// given back after it. A thread with no bit set pays this load and
    /// nothing else.
    watched: AtomicU8,
    /// Its Syscall User Dispatch settings (`seccomp::user_dispatch`), which
    /// Linux keeps per thread (`task_struct.syscall_dispatch`). A leaf lock,
    /// written only by the thread itself, never copied to a thread or
    /// process made of it, and cleared by `execve`.
    dispatch: SpinLock<Dispatch>,
    /// Its name, as `PR_SET_NAME` or a write to `/proc/<pid>/task/<tid>/comm`
    /// set it, NUL-padded; `None` reads as the program's file name. Linux
    /// keeps it per thread (`task_struct.comm`): a thread made by `clone`
    /// or `fork` starts with its creator's, and `execve` gives the thread
    /// that runs the new program the new program's file name. A leaf lock.
    name: SpinLock<Option<[u8; crate::syscall::attributes::TASK_COMM_LEN]>>,
}

/// [`Thread::watched`]'s bit for seccomp state.
pub(crate) const WATCH_SECCOMP: u8 = 1 << 0;
/// [`Thread::watched`]'s bit for Syscall User Dispatch, while it is on.
pub(crate) const WATCH_DISPATCH: u8 = 1 << 1;

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
        let mut thread = Thread::with(
            process,
            ThreadSignals::new(inherited)?,
            parent.seccomp_copy(),
        );
        *thread.name.get_mut() = parent.name();
        Ok(thread)
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
        // Its seccomp state is the creator's from the start, so that a path
        // that lists it without `Process::add_thread_from` still makes a
        // filtered thread (fail closed); `add_thread_from` copies it again
        // under the lock that lists the thread, and that copy is the one a
        // `TSYNC` cannot pass by.
        let mut thread = Thread::with(
            process,
            ThreadSignals::new(inherited)?,
            caller.seccomp_copy(),
        );
        *thread.tid.get_mut() = tid;
        *thread.numbers.get_mut() = numbers;
        *thread.name.get_mut() = caller.name();
        Ok(thread)
    }

    /// The first thread of `process`, numbered by its pid, with `signals` and
    /// `seccomp`.
    fn with(process: &Arc<Process>, signals: ThreadSignals, seccomp: seccomp::State) -> Thread {
        // Counted as it is made filtered, and given back by its drop.
        let mut watched = 0;
        if seccomp.is_active() {
            seccomp::note_filtered();
            watched |= WATCH_SECCOMP;
        }
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
            watched: AtomicU8::new(watched),
            // Off for every thread made, whatever its creator had.
            dispatch: SpinLock::new(Dispatch::OFF),
            name: SpinLock::new(None),
        }
    }

    /// The name set for it, if one was: see [`Thread::name`]'s field.
    pub(crate) fn name(&self) -> Option<[u8; crate::syscall::attributes::TASK_COMM_LEN]> {
        *self.name.lock()
    }

    /// Set its name, or with `None` go back to the program's file name.
    pub(crate) fn set_name(&self, name: Option<[u8; crate::syscall::attributes::TASK_COMM_LEN]>) {
        *self.name.lock() = name;
    }

    /// Whether the system call entry must look at its calls at all: the load
    /// the entry makes on every call, while any thread is watched.
    pub(crate) fn is_watched(&self) -> bool {
        self.watched.load(Ordering::Acquire) != 0
    }

    /// Raise or lower one bit of [`Thread::watched`], counted: raised after
    /// its count is taken, lowered before its count is given back, so that
    /// the count is never below the bits set. Called under the lock of the
    /// state the bit summarises, which is the only writer of that bit; other
    /// bits may change beside it, which the read-modify-write keeps.
    fn set_watch(&self, bit: u8, on: bool) {
        if (self.watched.load(Ordering::Relaxed) & bit != 0) == on {
            return;
        }
        if on {
            seccomp::note_filtered();
            let _ = self.watched.fetch_or(bit, Ordering::Release);
        } else {
            let _ = self.watched.fetch_and(!bit, Ordering::Release);
            seccomp::note_unfiltered();
        }
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
        // Written only on change: the mask is read on every call of a watched
        // thread, and a store each time would bounce its cache line.
        self.set_watch(WATCH_SECCOMP, held.is_active());
        drop(held);
        answer
    }

    /// Change its Syscall User Dispatch settings under their leaf lock, and
    /// raise or lower its watch bit for them.
    pub(crate) fn with_dispatch<R>(&self, change: impl FnOnce(&mut Dispatch) -> R) -> R {
        let mut held = self.dispatch.lock();
        let answer = change(&mut held);
        self.set_watch(WATCH_DISPATCH, held.is_on());
        drop(held);
        answer
    }

    /// Turn its Syscall User Dispatch off: the old program's, which `execve`
    /// clears as Linux's `begin_new_exec` does.
    pub(crate) fn clear_dispatch(&self) {
        self.with_dispatch(|dispatch| *dispatch = Dispatch::OFF);
    }

    /// What `execve` resets in the thread that runs the new program, as
    /// Linux's `begin_new_exec` does: its Syscall User Dispatch, off, and
    /// its name, the new program's file name (`__set_task_comm`).
    pub(crate) fn reset_for_exec(&self) {
        self.clear_dispatch();
        self.set_name(None);
    }

    /// A copy of its Syscall User Dispatch settings, the lock let go before
    /// the copy is used.
    pub(crate) fn dispatch(&self) -> Dispatch {
        *self.dispatch.lock()
    }

    /// Whether its Syscall User Dispatch is on: its watch bit, one load.
    pub(crate) fn is_dispatching(&self) -> bool {
        self.watched.load(Ordering::Acquire) & WATCH_DISPATCH != 0
    }

    /// Read its seccomp state under its leaf lock, changing nothing and
    /// storing nothing: what every call of a filtered thread does.
    pub(crate) fn read_seccomp<R>(&self, read: impl FnOnce(&seccomp::State) -> R) -> R {
        read(&self.seccomp.lock())
    }

    /// Take the seccomp mode and chain of `creator`, the thread that made this
    /// one: what `Process::add_thread_from` does while it holds the thread
    /// list, the creator's leaf lock taken inside it. Returns the state it
    /// replaced, for the caller to drop once the list's lock is released: it
    /// may hold the last reference to a long chain.
    #[must_use = "the replaced state is dropped after the thread-list lock"]
    pub(crate) fn copy_seccomp_from(&self, creator: &Thread) -> seccomp::State {
        let state = creator.seccomp_copy();
        self.with_seccomp(|mine| core::mem::replace(mine, state))
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
            || self.process.must_park()
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
        for _ in 0..self.watched.get_mut().count_ones() {
            seccomp::note_unfiltered();
        }
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
    // The task lent, not cloned; the thread the caller keeps is (B1).
    sched::with_current(|task| {
        let thread: Arc<dyn UserThread> = Arc::clone(task.thread()?);
        let thread: Arc<dyn Any + Send + Sync> = thread;
        thread.downcast().ok()
    })
    .flatten()
}

/// The running task's thread, lent to `lend` by reference, or `None` for a
/// kernel thread: what a caller that does not keep the thread asks instead of
/// [`current`], whose clone and drop of the task and the thread are four
/// locked operations a system call made several times (po10-pipe P2).
///
/// Only through `sched::with_current`'s closure, so the reference cannot
/// leave it (`sched::borrow`'s argument; the consultant's B1): nothing lent
/// here is stored, sent, or put on a list, a timer or a link. A caller that
/// keeps the thread clones it inside the closure.
pub(crate) fn with_current<R>(lend: impl FnOnce(&Thread) -> R) -> Option<R> {
    sched::with_current(|task| of_task(task).map(lend)).flatten()
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
