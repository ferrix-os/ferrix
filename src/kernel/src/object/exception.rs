//! Exception channels: a supervisor attached to a process or a job
//! (`docs/microkernel/K4.md`).
//!
//! A supervisor holds the read end of an ordinary channel; the kernel keeps
//! the other end, the only one that writes, in the [`Supervisor`] slot of
//! the process or job it was attached to. So everything a supervisor reads
//! from it is the kernel's, as K6's attested sender is.
//!
//! # The chain
//!
//! [`raise`], from the trap path, before the personality's signal: the
//! faulting process's own slot (level 0), then each job's from the process's
//! own to the root. At each live channel the kernel writes a [`Report`] and
//! one [`Exception`] handle, and the faulting thread waits -- interrupts
//! open, no lock held -- for a verdict, its process's end, or a deadline:
//! the level's timeout, and never past one chain deadline,
//! `EXCEPTION_TIMEOUT_MAX_MS` after the first report. `HAND_ON`, or the
//! exception's handle closed undecided, goes to the next level; the end of
//! the chain, or an expiry, takes the path a fault takes with no supervisor
//! (§2.3-§2.6).
//!
//! A level whose report cannot be made or queued -- a refused charge or
//! allocation, a full queue, a closed peer -- is skipped and counted, never
//! a panic.
//!
//! # Detaching
//!
//! Closing every handle to the read end. The slot notices the next time it
//! is asked -- an attach or a fault -- and lets the kernel's end go. A process's slot is also emptied when it ends
//! ([`Supervisor::close`]), so its supervisor sees `PEER_CLOSED` then.
//!
//! # Locks
//!
//! Each slot's lock is taken alone, in task context, and nothing is taken
//! under it. A kernel end the slot lets go of is taken out under the lock
//! and handed back to the caller, which disposes of it with no lock held
//! (`object::dispose`): an end's close takes its peer's locks and wakes its
//! waiters (`channel`'s `Drop`).
//!
//! An exception's lock is taken alone too. Its `Drop` -- a handle closed, a
//! queued message dropped with the end that held it, a dying supervisor's
//! table -- runs in task context: every such drop goes through
//! `object::dispose` or a channel end's close, which drops what its queue
//! held after its locks are let go (`channel`'s `Drop`, the consultant's
//! condition 3, ledger 2871).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_native_abi::exception::{
    FLAG_EXECUTE, FLAG_PC_AT_TRAP, FLAG_PRESENT, FLAG_WRITE, KIND_BREAKPOINT, KIND_BUS,
    KIND_ILLEGAL, KIND_OTHER, KIND_PAGE_FAULT, REPORT_BYTES, Registers, Report,
};
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::types::EXCEPTION_TIMEOUT_MAX_MS;

use crate::arch;
use crate::audit;
use crate::fallible::{self, AllocError};
use crate::object::channel::Endpoint;
use crate::object::job::{self, Job};
use crate::object::process::Process;
use crate::object::{self, Object};
use crate::sched::{self, WaitQueue};
use crate::sync::SpinLock;
use crate::trap::Trap;

/// An attached channel: the kernel's writing end, and how long a report on
/// it waits for an answer.
#[derive(Debug)]
struct Attached {
    /// The end the kernel writes reports to; its peer is the supervisor's.
    end: Arc<Endpoint>,
    /// How long one report waits for an answer, in milliseconds.
    timeout_ms: u64,
}

/// Why an attach was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachRefused {
    /// A channel whose read end is open is attached already.
    Bound,
    /// The process has ended, and its slot was closed for good.
    Closed,
}

/// The supervisor slot of a process or a job: empty, or one attached
/// channel.
#[derive(Debug)]
pub(crate) struct Supervisor {
    /// The attached channel, if any. A channel whose read end has closed
    /// stays here until the slot is next asked.
    slot: SpinLock<Slot>,
    /// The last report a thread of this process raised and was resumed
    /// from, for [`Report::repeat`]; only a process's slot keeps one.
    last: SpinLock<Last>,
    /// Set for good once its process ran with ids it was raised to, or
    /// made itself not dumpable: from then on no fault of it is offered to
    /// any supervisor ([`Supervisor::refuse`]). Only a process's slot is
    /// ever marked.
    refused: AtomicBool,
}

/// The last report resumed: by which task, at which pc and address, and how
/// many in a row.
#[derive(Debug, Clone, Copy, Default)]
struct Last {
    /// The task.
    tid: u64,
    /// Its pc.
    pc: u64,
    /// Its address.
    address: u64,
    /// Reports in a row at both, the first counting zero.
    count: u32,
}

/// What a slot holds.
#[derive(Debug)]
enum Slot {
    /// Nothing attached: an attach may come.
    Open(Option<Attached>),
    /// Its process has ended: nothing is ever attached again.
    Closed,
}

impl Supervisor {
    /// An empty slot.
    pub(crate) const fn new() -> Supervisor {
        Supervisor {
            slot: SpinLock::new(Slot::Open(None)),
            last: SpinLock::new(Last {
                tid: 0,
                pc: 0,
                address: 0,
                count: 0,
            }),
            refused: AtomicBool::new(false),
        }
    }

    /// Attach `end`, the kernel's end of a fresh channel, with `timeout_ms`.
    ///
    /// Refused while a channel whose read end is open is attached, and once
    /// the slot is closed; one whose read end has closed is replaced.
    /// Answers the kernel end that was let go, if any, for the caller to
    /// dispose of with no lock held.
    ///
    /// # Errors
    ///
    /// [`AttachRefused`], with `end` handed back.
    pub(crate) fn attach(
        &self,
        end: Arc<Endpoint>,
        timeout_ms: u64,
    ) -> Result<Option<Object>, (AttachRefused, Arc<Endpoint>)> {
        let mut slot = self.slot.lock();
        let Slot::Open(attached) = &mut *slot else {
            return Err((AttachRefused::Closed, end));
        };
        if attached
            .as_ref()
            .is_some_and(|attached| !attached.end.peer_closed())
        {
            return Err((AttachRefused::Bound, end));
        }
        let gone = attached.replace(Attached { end, timeout_ms });
        drop(slot);
        Ok(gone.map(|attached| Object::Channel(attached.end)))
    }

    /// The attached channel's kernel end and timeout, if its read end is
    /// open. One whose read end has closed is let go of here, and answered
    /// as the second half for the caller to dispose of with no lock held.
    fn live(&self) -> (Option<(Arc<Endpoint>, u64)>, Option<Object>) {
        let mut slot = self.slot.lock();
        let Slot::Open(attached) = &mut *slot else {
            return (None, None);
        };
        match attached {
            Some(live) if !live.end.peer_closed() => {
                (Some((Arc::clone(&live.end), live.timeout_ms)), None)
            }
            Some(_) => (None, attached.take().map(|dead| Object::Channel(dead.end))),
            None => (None, None),
        }
    }

    /// Offer none of this process's faults to any supervisor again
    /// (`docs/microkernel/K4.md` §13, the consultant's condition 1, ledger
    /// 2973): what an `execve` that raised its ids, a change of its ids, and
    /// `PR_SET_DUMPABLE` 0 call, as Linux's `ptrace_may_access` refuses a
    /// tracer such a process. For good: a supervisor attached later, to the
    /// process or to a job above it, hears nothing of it.
    pub(crate) fn refuse(&self) {
        self.refused.store(true, Ordering::Release);
    }

    /// Whether [`Supervisor::refuse`] was called.
    pub(crate) fn is_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }

    /// Whether a channel whose read end is open is attached.
    pub(crate) fn is_live(&self) -> bool {
        match &*self.slot.lock() {
            Slot::Open(Some(attached)) => !attached.end.peer_closed(),
            Slot::Open(None) | Slot::Closed => false,
        }
    }

    /// Let go of the attached channel if its read end has closed, and
    /// answer its kernel end for the caller to dispose of with no lock held:
    /// what an attach whose read end could not be placed does.
    pub(crate) fn let_go_if_dead(&self) -> Option<Object> {
        let mut slot = self.slot.lock();
        let Slot::Open(attached) = &mut *slot else {
            return None;
        };
        if attached
            .as_ref()
            .is_some_and(|attached| attached.end.peer_closed())
        {
            attached
                .take()
                .map(|attached| Object::Channel(attached.end))
        } else {
            None
        }
    }

    /// Close the slot for good: what a process that has ended does, so its
    /// supervisor sees `PEER_CLOSED` then, and an attach after it is
    /// refused. Answers the kernel end for the caller to dispose of with no
    /// lock held.
    pub(crate) fn close(&self) -> Option<Object> {
        match core::mem::replace(&mut *self.slot.lock(), Slot::Closed) {
            Slot::Open(attached) => attached.map(|attached| Object::Channel(attached.end)),
            Slot::Closed => None,
        }
    }
}

/// Whether `process` is under a supervisor: a live channel on it or on any
/// job above it. What `execve` asks before it raises credentials
/// (`docs/microkernel/K4.md` §11 (1)).
pub(crate) fn supervised(process: &Process) -> bool {
    process.supervisor().is_live() || job_supervised(&process.job())
}

/// Whether `job` or any job above it holds a live channel: what a
/// `cgroup.procs` move asks of its destination (`docs/microkernel/K4.md`
/// §11 (2)).
pub(crate) fn job_supervised(job: &Arc<Job>) -> bool {
    let mut at = Some(Arc::clone(job));
    while let Some(job) = at {
        if job.supervisor().is_live() {
            return true;
        }
        at = job.parent().cloned();
    }
    false
}

/// A supervisor's decision on one exception (`docs/microkernel/K4.md` §2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Resume the thread with its registers as they now stand.
    Resume,
    /// End its process, as a job kill ends it.
    Kill,
    /// Go to the next level up, or to the path a fault takes with none.
    HandOn,
}

/// Where an exception stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for a verdict.
    Pending,
    /// Decided, by a call or by its handle closed undecided.
    Decided(Verdict),
    /// Its wait ended without one: the deadline, or the process's end.
    Expired,
}

/// What an exception holds under its lock.
#[derive(Debug)]
struct Held {
    /// Where it stands.
    state: State,
    /// The faulting thread's registers: as it trapped, then as written.
    context: arch::UserContext,
    /// Whether a supervisor wrote them.
    changed: bool,
}

/// One faulting thread, suspended in the kernel: what the thread waits on.
#[derive(Debug)]
pub(crate) struct ExceptionCore {
    /// Its state and registers.
    held: SpinLock<Held>,
    /// The faulting thread, waiting for a verdict.
    waiters: WaitQueue,
    /// The kernel heap it and its report take, charged to the faulting
    /// process's job.
    _charge: ferrix_kmem::Charge,
}

impl ExceptionCore {
    /// Whether it is no longer pending.
    fn settled(&self) -> bool {
        self.held.lock().state != State::Pending
    }
}

/// An exception as its handle holds it. Its drop, the last handle gone
/// undecided, hands the fault on.
#[derive(Debug)]
pub(crate) struct Exception {
    /// The exception.
    core: Arc<ExceptionCore>,
}

/// Why an exception call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// A verdict was given already.
    Decided,
    /// Its wait expired: the default action was taken.
    Expired,
    /// The registers written are ones a program may not hold.
    Registers,
}

impl Exception {
    /// The registers it holds, as a supervisor reads them.
    ///
    /// # Errors
    ///
    /// [`Refused::Decided`], [`Refused::Expired`].
    pub(crate) fn registers(&self) -> Result<Registers, Refused> {
        let held = self.core.held.lock();
        pending(held.state)?;
        Ok(held.context.supervised())
    }

    /// Replace its registers with `written`, checked by the architecture's
    /// rule before anything is stored.
    ///
    /// # Errors
    ///
    /// [`Refused::Registers`] with nothing changed; [`Refused::Decided`],
    /// [`Refused::Expired`].
    pub(crate) fn set_registers(&self, written: &Registers) -> Result<(), Refused> {
        let mut held = self.core.held.lock();
        pending(held.state)?;
        let context = held
            .context
            .from_supervisor(written)
            .map_err(|_| Refused::Registers)?;
        held.context = context;
        held.changed = true;
        Ok(())
    }

    /// Decide it, once, and wake the faulting thread.
    ///
    /// # Errors
    ///
    /// [`Refused::Decided`], [`Refused::Expired`].
    pub(crate) fn resolve(&self, verdict: Verdict) -> Result<(), Refused> {
        {
            let mut held = self.core.held.lock();
            pending(held.state)?;
            held.state = State::Decided(verdict);
        }
        self.core.waiters.wake_all();
        Ok(())
    }
}

/// `Ok` while pending, else why not.
fn pending(state: State) -> Result<(), Refused> {
    match state {
        State::Pending => Ok(()),
        State::Decided(_) => Err(Refused::Decided),
        State::Expired => Err(Refused::Expired),
    }
}

impl Drop for Exception {
    /// Hand the fault on if nobody decided it: a supervisor that closed it,
    /// that ended, or whose channel closed with the report still queued.
    fn drop(&mut self) {
        let handed_on = {
            let mut held = self.core.held.lock();
            if held.state == State::Pending {
                held.state = State::Decided(Verdict::HandOn);
                true
            } else {
                false
            }
        };
        if handed_on {
            self.core.waiters.wake_all();
        }
    }
}

/// Reports written since boot.
static REPORTED: AtomicU64 = AtomicU64::new(0);
/// Levels skipped since boot because a report could not be made or queued.
static SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Reports written since boot, for the checks.
pub(crate) fn reported() -> u64 {
    REPORTED.load(Ordering::Relaxed)
}

/// Levels skipped since boot, for the checks.
pub(crate) fn skipped() -> u64 {
    SKIPPED.load(Ordering::Relaxed)
}

/// A fault as the trap path hands it to [`raise`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fault {
    /// What the processor reported.
    pub(crate) trap: Trap,
    /// Whether the personality's signal for it is `SIGBUS`: a touch past a
    /// file mapping's end.
    pub(crate) bus: bool,
    /// The fault address, or zero.
    pub(crate) address: u64,
    /// The architecture's own word for it (`arch::exception_code`).
    pub(crate) code: u64,
}

/// What became of a fault the chain was asked about.
#[derive(Debug)]
pub(crate) enum Raised {
    /// Resume with these registers.
    Resume(arch::UserContext),
    /// Its process was ended, by a verdict or from elsewhere while it waited:
    /// the thread leaves on its way out.
    Ended,
    /// No verdict: take the path a fault takes with no supervisor.
    Default,
}

/// What one level made of a report.
enum Asked {
    /// It could not be made or queued.
    Skipped,
    /// Handed on, by a verdict or by its handle closed undecided.
    HandOn,
    /// Resume, with the registers it then held and whether a supervisor
    /// wrote them.
    Resume(arch::UserContext, bool),
    /// End the process.
    Kill,
    /// The process is ending.
    Ending,
    /// The deadline passed undecided.
    Expired,
}

/// Offer the running thread's fault to its supervisors, nearest first
/// (`docs/microkernel/K4.md` §2.3-§2.6), with `context` its registers as it
/// trapped. Called by the trap path with interrupts open and no lock held,
/// before the personality's signal.
pub(crate) fn raise(context: arch::UserContext, fault: &Fault) -> Raised {
    let Some(task) = sched::current() else {
        return Raised::Default;
    };
    let Some(thread) = task.thread() else {
        return Raised::Default;
    };
    let host = thread.process();
    let process = host.core();
    // An ended process leaves; one that was raised, or made itself not
    // dumpable, is never offered to a supervisor (condition 1, 2973).
    if process.is_terminated() || process.supervisor().is_refused() {
        return Raised::Default;
    }
    let mut chain = Chain {
        tid: task.id,
        process,
        context,
        fault,
        level: 0,
        deadline: None,
    };
    let mut job: Option<Arc<Job>> = None;
    loop {
        let slot = match &job {
            None => process.supervisor(),
            Some(job) => job.supervisor(),
        };
        if let Some(raised) = chain.ask_level(slot) {
            if matches!(raised, Asked::Kill) {
                host.kill(job::KILLED_STATUS);
            }
            return raised.into_raised();
        }
        chain.level = chain.level.saturating_add(1);
        job = match job {
            None => Some(process.job()),
            Some(job) => job.parent().cloned(),
        };
        if job.is_none() {
            return Raised::Default;
        }
    }
}

/// One fault's walk up its levels: what every level's report shares.
struct Chain<'a> {
    /// The faulting task.
    tid: u64,
    /// Its process.
    process: &'a Process,
    /// Its registers as it trapped.
    context: arch::UserContext,
    /// The fault.
    fault: &'a Fault,
    /// The level being asked.
    level: u32,
    /// The chain's deadline, fixed at the first report.
    deadline: Option<u64>,
}

impl Chain<'_> {
    /// Ask the channel in `slot`, if it has a live one: `None` to go on to
    /// the next level, or how the fault ends. A `Kill` is audited here and
    /// carried out by the caller, which holds the process's host.
    fn ask_level(&mut self, slot: &Supervisor) -> Option<Asked> {
        let (live, gone) = slot.live();
        object::dispose(gone);
        let (end, timeout_ms) = live?;
        let now = crate::timer::now_nanos();
        let chain = *self
            .deadline
            .get_or_insert(now.saturating_add(EXCEPTION_TIMEOUT_MAX_MS * 1_000_000));
        if now >= chain {
            killed(self.process, self.level, KILLED_BY_EXPIRY);
            return Some(Asked::Expired);
        }
        let deadline = now.saturating_add(timeout_ms * 1_000_000).min(chain);
        let report = report_for(
            self.tid,
            self.process,
            &self.context,
            self.fault,
            self.level,
        );
        match ask(&end, &report, self.context, deadline) {
            Asked::Skipped => {
                let _ = SKIPPED.fetch_add(1, Ordering::Relaxed);
                None
            }
            Asked::HandOn => None,
            Asked::Resume(resumed, changed) => {
                note_resumed(self.process, &report);
                if changed {
                    audit_context(
                        self.process,
                        self.context.program_counter(),
                        resumed.program_counter(),
                        self.level,
                    );
                }
                Some(Asked::Resume(resumed, changed))
            }
            Asked::Kill => {
                killed(self.process, self.level, KILLED_BY_VERDICT);
                Some(Asked::Kill)
            }
            Asked::Ending => Some(Asked::Ending),
            Asked::Expired => {
                killed(self.process, self.level, KILLED_BY_EXPIRY);
                Some(Asked::Expired)
            }
        }
    }
}

impl Asked {
    /// What the trap path makes of a level's ending answer.
    fn into_raised(self) -> Raised {
        match self {
            Asked::Resume(resumed, _) => Raised::Resume(resumed),
            Asked::Kill | Asked::Ending => Raised::Ended,
            Asked::Skipped | Asked::HandOn | Asked::Expired => Raised::Default,
        }
    }
}

/// Write `report` and a new exception to `end`, and wait for a verdict
/// until `deadline`. Every allocation is fallible and made before the write;
/// the exception and its report are charged to the running task's job, the
/// faulting process's.
fn ask(end: &Endpoint, report: &Report, context: arch::UserContext, deadline: u64) -> Asked {
    let Ok(made) = make(report, context) else {
        return Asked::Skipped;
    };
    let (core, bytes, transfers) = made;
    if end.write(bytes, 1, move || Ok::<_, ()>(transfers)).is_err() {
        // The exception went with the refused write, handed on with nobody
        // waiting on it; this level is skipped.
        return Asked::Skipped;
    }
    let _ = REPORTED.fetch_add(1, Ordering::Relaxed);
    let _ = core
        .waiters
        .wait_until_deadline(|| core.settled() || sched::work::own_end(), deadline);
    let mut held = core.held.lock();
    match held.state {
        State::Decided(Verdict::HandOn) => Asked::HandOn,
        State::Decided(Verdict::Kill) => Asked::Kill,
        State::Decided(Verdict::Resume) => Asked::Resume(held.context, held.changed),
        State::Expired => Asked::Expired,
        State::Pending => {
            held.state = State::Expired;
            if sched::work::own_end() {
                Asked::Ending
            } else {
                Asked::Expired
            }
        }
    }
}

/// The exception, its report's bytes and the message's one handle: all
/// allocated before anything is written.
fn make(
    report: &Report,
    context: arch::UserContext,
) -> Result<(Arc<ExceptionCore>, Vec<u8>, Vec<object::Transfer>), AllocError> {
    let charge = ferrix_kmem::Charge::bytes(size_of::<ExceptionCore>() + REPORT_BYTES)
        .map_err(|_| AllocError)?;
    let core = fallible::try_arc(ExceptionCore {
        held: SpinLock::new(Held {
            state: State::Pending,
            context,
            changed: false,
        }),
        waiters: WaitQueue::new(),
        _charge: charge,
    })?;
    let exception = fallible::try_arc(Exception {
        core: Arc::clone(&core),
    })?;
    let bytes = fallible::try_to_vec(&report.to_bytes())?;
    let mut transfers = fallible::try_with_capacity(1)?;
    let _ = fallible::push_within(
        &mut transfers,
        (Object::Exception(exception), Rights::EXCEPTION),
    );
    Ok((core, bytes, transfers))
}

/// The report for the running thread's fault at `level`.
fn report_for(
    tid: u64,
    process: &Process,
    context: &arch::UserContext,
    fault: &Fault,
    level: u32,
) -> Report {
    let (kind, mut flags) = match fault.trap {
        Trap::PageFault(page) => {
            let mut flags = 0;
            if page.write {
                flags |= FLAG_WRITE;
            }
            if page.execute {
                flags |= FLAG_EXECUTE;
            }
            if page.present {
                flags |= FLAG_PRESENT;
            }
            let kind = if fault.bus { KIND_BUS } else { KIND_PAGE_FAULT };
            (kind, flags)
        }
        Trap::Breakpoint => (KIND_BREAKPOINT, 0),
        Trap::IllegalInstruction => (KIND_ILLEGAL, 0),
        Trap::Fault { .. } | Trap::Interrupt(_) | Trap::SystemCall => (KIND_OTHER, 0),
    };
    // x86-64's `int3` leaves the pc after itself; every other trap reported
    // here, on every architecture, leaves it at the trapping instruction.
    if !(cfg!(target_arch = "x86_64") && fault.trap == Trap::Breakpoint) {
        flags |= FLAG_PC_AT_TRAP;
    }
    let pc = context.program_counter();
    let repeat = {
        let last = process.supervisor().last.lock();
        if last.tid == tid && last.pc == pc && last.address == fault.address {
            last.count.saturating_add(1)
        } else {
            0
        }
    };
    Report {
        kind,
        level,
        pid: process.pid(),
        repeat,
        tid,
        job: process.job().id(),
        address: fault.address,
        code: fault.code,
        pc,
        flags,
        reserved: 0,
    }
}

/// Keep `report` as the last one resumed, for the next one's repeat count.
fn note_resumed(process: &Process, report: &Report) {
    *process.supervisor().last.lock() = Last {
        tid: report.tid,
        pc: report.pc,
        address: report.address,
        count: report.repeat,
    };
}

/// [`audit::EXCEPTION_KILLED`]'s first detail word: a supervisor's `KILL`.
const KILLED_BY_VERDICT: u32 = 1;
/// [`audit::EXCEPTION_KILLED`]'s first detail word: no answer before the
/// deadline, so the default action.
const KILLED_BY_EXPIRY: u32 = 2;

/// Audit a fault ended by a verdict, or left to the default by an expiry.
fn killed(process: &Process, level: u32, why: u32) {
    audit::record(
        audit::EXCEPTION_KILLED,
        audit::Outcome::Done,
        0,
        audit::Subject::of(process),
        audit::Target {
            kind: audit::target::PROCESS,
            id: u64::from(process.pid()),
        },
        [why, level, 0],
    );
}

/// Audit a resume with registers a supervisor wrote: the pc before and
/// after, low words, and the level.
fn audit_context(process: &Process, before: u64, after: u64, level: u32) {
    audit::record(
        audit::EXCEPTION_CONTEXT,
        audit::Outcome::Done,
        0,
        audit::Subject::of(process),
        audit::Target {
            kind: audit::target::PROCESS,
            id: u64::from(process.pid()),
        },
        [before as u32, after as u32, level],
    );
}
