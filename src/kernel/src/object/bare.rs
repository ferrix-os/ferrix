//! A process no personality holds (`docs/microkernel/K11.md` §4): what a
//! shared process is, in every build, and what a native process will be once
//! `personality=user` compiles the Linux personality out of ring 0.
//!
//! It answers every question the core asks of a [`Host`] and holds no Linux
//! state, so no personality's code runs for it. Its threads run native code;
//! a Linux number from normal mode answers `ENOSYS`.
//!
//! # The way back to user mode
//!
//! A thread of one is told apart from a personality's by the processor's
//! record of what it runs (`sched::running_bare`), and [`RETURN_PATH`] answers
//! its way out: a kill ends the thread, and a fault past its supervisor ends
//! the process and, for a process of a region, the region's job (K11 §5, the
//! consultant's N3): `linuxd`'s state is the partition's, so one of its
//! processes faulting in normal mode has left it all in doubt.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::fallible::{self, AllocError};
use crate::object::job::{self, Job};
use crate::object::process::{self, Host, Process};
use crate::object::region::Region;
use crate::object::{self, HandleTable};
use crate::sched::{self, Task, UserThread, WaitQueue};
use crate::sync::SpinLock;
use crate::trap::{FaultOutcome, ReturnPath};
use crate::user::space::AddressSpace;

/// The process.
#[derive(Debug)]
pub struct Bare {
    /// What the core enforces.
    core: Arc<Process>,
    /// Threads not yet gone.
    live: AtomicU32,
    /// Whether it has been ended: once.
    ending: AtomicBool,
    /// Whether what it held has been let go of: once.
    released: AtomicBool,
    /// Whether it has been started: once.
    started: AtomicBool,
    /// Itself, for a thread to hold.
    me: Weak<Bare>,
}

/// A thread of one: where it starts, and what a handle to it holds.
#[derive(Debug)]
pub struct BareThread {
    /// Its process.
    process: Arc<Bare>,
    /// Where it enters user mode.
    entry: u64,
    /// The stack it starts on.
    stack: u64,
    /// Its first argument register.
    argument: u64,
    /// What its handles name.
    object: Arc<ThreadRef>,
}

/// What a thread handle names: `Object::Thread`.
#[derive(Debug)]
pub struct ThreadRef {
    /// The task, once it is made; never kept alive from here.
    task: SpinLock<Weak<Task>>,
    /// Whether it has ended: its handle's `TERMINATED`.
    ended: AtomicBool,
    /// Woken as it ends.
    waiters: WaitQueue,
    /// Whether its process is a region's: only those can be kicked.
    in_region: bool,
    /// Its job's criticality level.
    level: u8,
}

impl ThreadRef {
    /// Its job's criticality level.
    pub const fn level(&self) -> u8 {
        self.level
    }

    /// Whether it has ended.
    pub fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// The queue woken as it ends.
    pub fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    /// Whether its process is a region's.
    pub fn in_region(&self) -> bool {
        self.in_region
    }

    /// Its task, while it has one.
    pub fn task(&self) -> Option<Arc<Task>> {
        self.task.lock().upgrade()
    }

    /// Its task is `task`.
    fn set_task(&self, task: &Arc<Task>) {
        *self.task.lock() = Arc::downgrade(task);
    }

    /// It has ended.
    fn mark_ended(&self) {
        self.ended.store(true, Ordering::Release);
        self.waiters.wake_all();
    }
}

impl UserThread for BareThread {
    fn process(&self) -> &dyn Host {
        &*self.process
    }

    fn is_bare(&self) -> bool {
        true
    }
}

impl BareThread {
    /// Its process.
    pub fn process_of(&self) -> &Arc<Bare> {
        &self.process
    }

    /// What its handles name.
    pub fn object(&self) -> &Arc<ThreadRef> {
        &self.object
    }
}

/// The thread running this code, if it is one of these.
pub fn current_thread() -> Option<Arc<BareThread>> {
    sched::with_current(|task| {
        let thread = Arc::clone(task.thread()?);
        let any: Arc<dyn core::any::Any + Send + Sync> = thread;
        any.downcast::<BareThread>().ok()
    })
    .flatten()
}

impl Bare {
    /// A process over `space` in `job`, of `region` if there is one, unstarted.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub fn new(
        space: Arc<AddressSpace>,
        job: Arc<Job>,
        region: Option<Arc<Region>>,
    ) -> Result<Arc<Bare>, AllocError> {
        let pid = process::allocate().unwrap_or(0);
        let core = Process::new(space, pid, job)?;
        let core = match region {
            Some(region) => core.in_region(region),
            None => core,
        };
        let core = fallible::try_arc(core)?;
        let host = fallible::try_arc_cyclic(|me: &Weak<Bare>| Bare {
            core,
            live: AtomicU32::new(0),
            ending: AtomicBool::new(false),
            released: AtomicBool::new(false),
            started: AtomicBool::new(false),
            me: me.clone(),
        })?;
        let weak: Weak<dyn Host> = {
            let as_host: Arc<dyn Host> = host.clone();
            Arc::downgrade(&as_host)
        };
        if pid != 0 {
            process::name(pid, weak)?;
        }
        Ok(host)
    }

    /// Start its first thread at `entry` on `stack`, with `argument`, once.
    ///
    /// # Errors
    ///
    /// `Err` with why, the start given back.
    pub fn start(&self, entry: u64, stack: u64, argument: u64) -> Result<(), &'static str> {
        if self.started.swap(true, Ordering::AcqRel) {
            return Err("the process has already started");
        }
        if self.core.is_terminated() {
            return Err("the process has already ended");
        }
        if let Err(why) = self.spawn(entry, stack, argument).map(|_| ()) {
            self.started.store(false, Ordering::Release);
            return Err(why);
        }
        Ok(())
    }

    /// A thread of this process at `entry` on `stack`, with `argument`.
    ///
    /// # Errors
    ///
    /// `Err` with why; nothing has run.
    pub fn spawn(
        &self,
        entry: u64,
        stack: u64,
        argument: u64,
    ) -> Result<Arc<ThreadRef>, &'static str> {
        self.spawn_with(entry, stack, argument, |_| true)
    }

    /// [`Bare::spawn`], with `ready` asked once everything that can fail has been
    /// done but its handle's: a thread it says no to never runs.
    ///
    /// # Errors
    ///
    /// `Err` with why; nothing has run.
    pub fn spawn_with(
        &self,
        entry: u64,
        stack: u64,
        argument: u64,
        ready: impl FnOnce(&Arc<ThreadRef>) -> bool,
    ) -> Result<Arc<ThreadRef>, &'static str> {
        let process = self.me.upgrade().ok_or("the process has gone")?;
        let object = fallible::try_arc(ThreadRef {
            task: SpinLock::new(Weak::new()),
            ended: AtomicBool::new(false),
            waiters: WaitQueue::new(),
            in_region: self.core.region().is_some(),
            level: self.core.exit().level(),
        })
        .map_err(|_| "no memory for the thread")?;
        let thread = fallible::try_arc(BareThread {
            process,
            entry,
            stack,
            argument,
            object: Arc::clone(&object),
        })
        .map_err(|_| "no memory for the thread")?;
        let prepared = sched::prepare_user("bare", run_thread, thread, None, None)?;
        object.set_task(prepared.task());
        self.core
            .list_task(prepared.task())
            .map_err(|_| "no memory to list the thread")?;
        if !ready(&object) {
            return Err("its handle could not be placed");
        }
        // A kill that walked the list before this task was on it stored
        // `terminated` first, which `run_thread` reads.
        let _task = prepared.launch();
        Ok(object)
    }

    /// End it with `status`, once: every thread finds out on its way back to
    /// user mode.
    pub fn end(&self, status: i32) {
        if self.ending.swap(true, Ordering::AcqRel) {
            return;
        }
        self.core.end_record(status, 0);
        if self.live.load(Ordering::Acquire) == 0 {
            self.release();
        }
    }

    /// Let go of what it holds, once, after [`Bare::end`] and when no thread
    /// of it is live: as the personality's release does, what of it is the
    /// core's. Interrupts open, no lock held.
    fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        object::dispose(self.core.with_private_handles(HandleTable::close));
        object::dispose(self.core.close_bootstrap());
        object::dispose(self.core.supervisor().close());
        // Its memory goes before anything can tell a waiter it has ended, and
        // charged to nobody, as the personality's does.
        let space = self.core.own_space();
        let _ = object::quota::charging_nobody(|| {
            space.unmap(
                crate::user::space::MMAP_MIN_ADDR,
                space.ceiling() - crate::user::space::MMAP_MIN_ADDR,
            )
        });
        self.core.leave_job();
        let observers = self.core.exit().close();
        self.core.exited().wake_all();
        for observer in observers {
            observer.fire(ferrix_native_abi::signals::Signals::TERMINATED);
        }
        if let Some(region) = self.core.region() {
            region.leave();
        }
    }
}

impl Drop for Bare {
    /// One that was made and never started, or never ended, leaves its region's
    /// count here: a region's end must not wait on it.
    fn drop(&mut self) {
        if !self.released.load(Ordering::Acquire)
            && let Some(region) = self.core.region()
        {
            region.leave();
        }
    }
}

impl Host for Bare {
    fn core(&self) -> &Process {
        &self.core
    }

    fn core_arc(&self) -> &Arc<Process> {
        &self.core
    }

    fn kill(&self, status: i32) {
        self.end(status);
    }

    fn thread_starting(&self) {
        let _ = self.live.fetch_add(1, Ordering::AcqRel);
    }

    fn thread_gone(&self, ended: bool) {
        let before = self.live.fetch_sub(1, Ordering::AcqRel);
        if before != 1 {
            return;
        }
        if self.core.is_terminated() {
            self.release();
        } else if ended {
            self.end(0);
        }
    }

    fn wait_interrupted(&self) -> bool {
        self.core.is_terminated()
    }
}

/// Where a thread of one begins: into user mode at its entry, unless the
/// process has ended first.
fn run_thread(_argument: usize) {
    let Some(thread) = current_thread() else {
        return;
    };
    // Masked from the test to the entry, for the reason the personality's
    // `run_program` gives.
    crate::arch::disable_interrupts();
    sched::work::place_before_entry();
    if thread.process.core.is_terminated() {
        drop(thread);
        leave_current(None);
    }
    let (entry, stack, argument) = (thread.entry, thread.stack, thread.argument);
    drop(thread);
    sched::vtime::leaving();
    // SAFETY: (CONTEXT) this task was spawned in the process's address space,
    // which the switch that got here installed with its user state and entry
    // stack; `entry` and `stack` were checked as a loader's are by the call
    // that made the thread, and nothing owned is left on this frame.
    unsafe { crate::arch::enter_user(entry, stack, argument, crate::trap::Abi::Native) }
}

/// End the running thread, and its process with `status` when given: the way
/// out of a thread of one. Interrupts are opened first.
pub fn leave_current(status: Option<i32>) -> ! {
    sched::begin_leaving();
    crate::arch::enable_interrupts();
    if let Some(thread) = current_thread() {
        let process = Arc::clone(&thread.process);
        if let Some(status) = status {
            process.end(status);
        }
        thread.object.mark_ended();
        drop(thread);
        process.thread_gone(true);
    }
    sched::exit_leaving()
}

/// Whether the running thread must leave: its process is ending.
fn needs_attention() -> bool {
    sched::work::own_end()
}

/// Act on it.
fn return_to_user(_context: &mut crate::arch::UserContext) {
    if needs_attention() {
        leave_current(None);
    }
}

/// No thread of one has a signal frame to return through.
fn sigreturn(_context: &mut crate::arch::UserContext, _rt: bool) {}

/// A fault past its supervisor: the module's rule.
fn fault_signal(signal: u32, code: i32, address: u64, pc: u64) -> FaultOutcome {
    let Some(thread) = current_thread() else {
        return FaultOutcome::NoProcess;
    };
    let pid = thread.process.core.pid();
    crate::console::println!(
        "  bare     process {pid}: a fault in normal mode at pc {pc:#x}, address {address:#x}, \
         signal {signal} code {code}: its region's job is ended"
    );
    let status = 128 + signal as i32;
    match thread.process.core.region() {
        Some(region) => {
            let ended = region.job().kill(job::KILLED_STATUS).unwrap_or(0);
            crate::audit::ended(
                crate::audit::JOB_KILLED,
                crate::audit::Subject::of(&thread.process.core),
                crate::audit::Target {
                    kind: crate::audit::target::JOB,
                    id: region.job().id(),
                },
                [u32::try_from(ended).unwrap_or(u32::MAX), signal, 0],
            );
        }
        None => thread.process.end(status),
    }
    thread.process.end(status);
    FaultOutcome::Ended { pid }
}

/// The way back for a thread of one.
pub(crate) static RETURN_PATH: ReturnPath = ReturnPath {
    needs_attention,
    return_to_user,
    sigreturn,
    fault_signal,
    fault_end: fault_signal,
};

// CHECKS-ONLY: object/level_check.rs
/// A thread's handle object at `level`, with no task: what the level door reads.
#[doc(hidden)]
pub fn thread_object_at(level: u8) -> Option<Arc<ThreadRef>> {
    fallible::try_arc(ThreadRef {
        task: SpinLock::new(Weak::new()),
        ended: AtomicBool::new(false),
        waiters: WaitQueue::new(),
        in_region: false,
        level,
    })
    .ok()
}
