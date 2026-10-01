//! seccomp: a thread may give up the system calls it will ever make, or have
//! a program of its own judge each one (`docs/SECCOMP.md`).
//!
//! # The hook
//!
//! The core's entries call [`check`] first, for every call of every program,
//! before their own early answers and before the native range is split from
//! the Linux tables (`crate::trap::filter_system_call`, §3.3). An unfiltered
//! thread pays that call and one load of its own `filtered` flag. A filtered
//! one has its call turned into a [`SeccompData`] ([`data`]) and judged by
//! every filter of its chain, newest first; the most restrictive answer wins,
//! and on a tie the newest filter's. What the answer can do is only less than
//! the call would: fail it with an errno, end the thread or the process, or
//! let it go on. A [`Verdict`] can say nothing else.
//!
//! # What a thread holds
//!
//! A [`State`] on the [`Thread`], as Linux keeps it on the task: a mode and
//! the newest [`Filter`] of a chain, each filter pointing at the one before.
//! Threads of one process can hold different chains (`TSYNC`, when it is
//! built, makes them equal). A fork child or a `clone`d thread takes its
//! creator's chain, shared; a native child of a filtered creator takes it
//! through [`inherit_native`]; `execve` keeps it, which is what makes a
//! filter a sandbox and not a request. A filter is charged to the job that
//! installed it (F-37) and stays charged until the last thread and child
//! holding it goes. Releasing a chain is iterative: a chain can be 6,554
//! filters long.
//!
//! # Who may install one
//!
//! A thread whose process has set `PR_SET_NO_NEW_PRIVS`, or is privileged
//! ([`may_install`]): otherwise a program could run a set-user-id file under
//! a filter that makes it believe it had dropped privileges.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use ferrix_kmem::{Charge, arc_footprint, buffer_footprint};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_seccomp::{
    ACTION_FULL, ALLOW, DATA, ERRNO, Insn, KILL_PROCESS, KILL_THREAD, LOG, MAX_INSNS, Program,
    SeccompData, TRACE, TRAP, USER_NOTIF,
};

use crate::arch;
use crate::console::println;
use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};
use crate::syscall::signal::{Origin, Posted};
use crate::syscall::thread::{self, Thread};
use crate::syscall::{attributes, deliver, uaccess, userns};
use crate::trap::{Abi, SyscallArgs, Verdict};

/// The `arch` a call in the native range carries.
///
/// A native call (`0x1000..=0x1FFF` from the native entry) is not a Linux
/// call, and a filter that reads `arch` first, as Chromium's and systemd's do,
/// must not take it for one. So it has a token of its own, which Linux never
/// uses: an `e_machine` (0x0F1F) in the range IANA has not assigned, with the
/// 64-bit and little-endian flags every call made through a 64-bit register
/// file carries. A filter that wants these calls allows this `arch`
/// explicitly; one that allowlists numbers refuses them as it refuses any
/// number it does not know (`docs/SECCOMP.md` §3.3, §12 Q2).
pub(crate) const NATIVE_ARCH: u32 = 0xC000_0F1F;

/// `seccomp`'s operations.
mod op {
    /// `SECCOMP_SET_MODE_STRICT`.
    pub(super) const SET_MODE_STRICT: u64 = 0;
    /// `SECCOMP_SET_MODE_FILTER`.
    pub(super) const SET_MODE_FILTER: u64 = 1;
    /// `SECCOMP_GET_ACTION_AVAIL`.
    pub(super) const GET_ACTION_AVAIL: u64 = 2;
    /// `SECCOMP_GET_NOTIF_SIZES`.
    pub(super) const GET_NOTIF_SIZES: u64 = 3;
}

/// The filter flags `seccomp(SET_MODE_FILTER)` knows.
pub(crate) mod flag {
    /// `SECCOMP_FILTER_FLAG_TSYNC`: give every thread of the process the new
    /// filter, or none (`docs/SECCOMP.md` §3.7).
    pub(crate) const TSYNC: u64 = 1 << 0;
    /// `SECCOMP_FILTER_FLAG_TSYNC_ESRCH`: with `TSYNC`, report a thread that
    /// cannot be synchronised as `ESRCH` rather than as its thread id.
    pub(crate) const TSYNC_ESRCH: u64 = 1 << 4;
    /// `SECCOMP_FILTER_FLAG_LOG`.
    pub(crate) const LOG: u64 = 1 << 1;
    /// `SECCOMP_FILTER_FLAG_SPEC_ALLOW`: accepted, and it turns nothing off,
    /// since no mitigation here is ever turned off (`docs/SECCOMP.md` §3.9).
    pub(crate) const SPEC_ALLOW: u64 = 1 << 2;
    /// The flags this kernel builds; any other bit is `EINVAL`. `TSYNC` and
    /// its `ESRCH` form (bits 0 and 4) wait for the landing that synchronises
    /// threads (§3.7): refused rather than accepted and ignored, because a
    /// program that asked for every thread to be filtered and was told yes
    /// would be wrong. `NEW_LISTENER` and `WAIT_KILLABLE_RECV` (bits 3 and 5)
    /// are not built at all, and Linux's bits beyond the sixth are unknown.
    pub(crate) const BUILT: u64 = LOG | SPEC_ALLOW | TSYNC | TSYNC_ESRCH;
}

/// `prctl`'s seccomp options, and the speculation controls a filter's installer
/// asks about (`docs/SECCOMP.md` §3.9).
pub(crate) mod option {
    /// `PR_GET_SECCOMP`.
    pub(crate) const PR_GET_SECCOMP: i32 = 21;
    /// `PR_SET_SECCOMP`.
    pub(crate) const PR_SET_SECCOMP: i32 = 22;
    /// `PR_GET_SPECULATION_CTRL`.
    pub(crate) const PR_GET_SPECULATION_CTRL: i32 = 52;
    /// `PR_SET_SPECULATION_CTRL`.
    pub(crate) const PR_SET_SPECULATION_CTRL: i32 = 53;
}

/// `PR_SPEC_STORE_BYPASS` and `PR_SPEC_INDIRECT_BRANCH`: the two features
/// `PR_{GET,SET}_SPECULATION_CTRL` name that this kernel mitigates.
const PR_SPEC_STORE_BYPASS: u64 = 0;
/// See [`PR_SPEC_STORE_BYPASS`].
const PR_SPEC_INDIRECT_BRANCH: u64 = 1;
/// `PR_SPEC_PRCTL`: controllable by `prctl`.
const PR_SPEC_PRCTL: usize = 1 << 0;
/// `PR_SPEC_ENABLE`: the speculation is on.
const PR_SPEC_ENABLE: u64 = 1 << 1;
/// `PR_SPEC_DISABLE`: the speculation is off.
const PR_SPEC_DISABLE: u64 = 1 << 2;
/// `PR_SPEC_FORCE_DISABLE`: off, and not to be turned on again.
const PR_SPEC_FORCE_DISABLE: u64 = 1 << 3;

/// `prctl(PR_GET_SPECULATION_CTRL, which)`: what a program is told about the
/// mitigations of store bypass and indirect branches. On Ferrix they are on for
/// every program and no program may turn them off (`docs/certification/
/// SPECULATION.md`), so both answer "force-disabled", the state Linux reports
/// for a task whose mitigation was forced on: Chromium's `DisableIBSpec` reads
/// it and has nothing to do. Any other feature is `ENODEV`, as Linux answers a
/// feature it has no control for.
fn speculation_get(which: u64, rest: [u64; 3]) -> Result<usize, Errno> {
    if rest != [0; 3] {
        return Err(Errno::EINVAL);
    }
    match which {
        PR_SPEC_STORE_BYPASS | PR_SPEC_INDIRECT_BRANCH => {
            Ok(PR_SPEC_PRCTL | PR_SPEC_FORCE_DISABLE as usize)
        }
        _ => Err(Errno::ENODEV),
    }
}

/// `prctl(PR_SET_SPECULATION_CTRL, which, control)`: a request for more
/// mitigation is already true and is accepted; one for less, `PR_SPEC_ENABLE`,
/// is `EPERM`, as Linux refuses it for a feature that was forced off (SR14: no
/// filter and no flag lowers a mitigation). A value that is no control is
/// `ERANGE`, and a non-zero argument beyond is `EINVAL`.
fn speculation_set(which: u64, control: u64, rest: [u64; 2]) -> Result<usize, Errno> {
    if rest != [0; 2] {
        return Err(Errno::EINVAL);
    }
    if !matches!(which, PR_SPEC_STORE_BYPASS | PR_SPEC_INDIRECT_BRANCH) {
        return Err(Errno::ENODEV);
    }
    match control {
        PR_SPEC_DISABLE | PR_SPEC_FORCE_DISABLE => Ok(0),
        PR_SPEC_ENABLE => Err(Errno::EPERM),
        _ => Err(Errno::ERANGE),
    }
}

/// `SECCOMP_MODE_*`, as `PR_GET_SECCOMP` and `/proc/<pid>/status` report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    /// `SECCOMP_MODE_DISABLED`.
    #[default]
    Disabled,
    /// `SECCOMP_MODE_STRICT`: `read`, `write`, `exit` and `sigreturn`.
    Strict,
    /// `SECCOMP_MODE_FILTER`.
    Filter,
    /// `SECCOMP_MODE_DEAD`: the thread was killed by its filter and should
    /// serve no call of its own should anything return to it.
    Dead,
}

impl Mode {
    /// The number `PR_GET_SECCOMP` answers.
    pub(crate) const fn number(self) -> u32 {
        match self {
            Mode::Disabled | Mode::Dead => 0,
            Mode::Strict => 1,
            Mode::Filter => 2,
        }
    }
}

/// What a thread holds of seccomp.
#[derive(Debug, Clone, Default)]
pub(crate) struct State {
    /// The mode.
    mode: Mode,
    /// The newest filter; each points at the one before it.
    filter: Option<Arc<Filter>>,
}

impl State {
    /// A thread in filter mode that holds no filter: a state nothing can make,
    /// for the boot check that a thread found in it is ended, not let go on.
    pub(crate) fn filter_mode_with_no_filter() -> State {
        State {
            mode: Mode::Filter,
            filter: None,
        }
    }

    /// Whether the thread is under seccomp at all.
    pub(crate) fn is_active(&self) -> bool {
        self.mode != Mode::Disabled
    }

    /// The mode.
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// How many filters the chain holds: `Seccomp_filters`.
    pub(crate) fn filters(&self) -> u32 {
        self.filter.as_ref().map_or(0, |newest| newest.depth)
    }
}

/// One installed filter: a program, verified, and the chain before it.
#[derive(Debug)]
pub(crate) struct Filter {
    /// The program; immutable once made.
    program: Program,
    /// `SECCOMP_FILTER_FLAG_LOG`: log every action but `ALLOW` this filter's
    /// answer leads to.
    log: bool,
    /// The filter installed before this one, if any. Set once, when this one
    /// is attached, while nobody else can see it.
    previous: Option<Arc<Filter>>,
    /// What Linux counts against `MAX_INSNS_PER_PATH`: this filter's
    /// instructions and four more, and all those before it.
    cost: u32,
    /// How many filters there are, this one included.
    depth: u32,
    /// Its memory, charged to the job of the thread that installed it, given
    /// back when the last reference goes (F-37).
    _charge: Charge,
}

impl Drop for Filter {
    /// Release the chain behind it by walking it, stopping at the first filter
    /// someone else still holds. A recursive drop of a chain of 6,554 filters
    /// would run the kernel stack out: Linux's `__put_seccomp_filter` walks for
    /// the same reason (SR11).
    fn drop(&mut self) {
        // For the boot check that a long chain's release is flat: the stack
        // the measured task's releases use, as the span between the highest and
        // the lowest address of a local across them. A walk uses one frame's
        // worth however long the chain is; a recursive drop one frame for each
        // filter.
        if sched::current_id() == Some(RELEASE_TASK.load(Ordering::Relaxed)) {
            let probe = 0_u8;
            let here = core::ptr::from_ref(&probe) as usize;
            let _ = RELEASE_LOWEST.fetch_min(here, Ordering::Relaxed);
            let _ = RELEASE_HIGHEST.fetch_max(here, Ordering::Relaxed);
        }
        let mut next = self.previous.take();
        while let Some(filter) = next {
            // `Some` only for the last reference, and then the filter goes
            // with its own `previous` already taken, so its drop is shallow.
            next = Arc::into_inner(filter).and_then(|mut last| last.previous.take());
        }
    }
}

/// The task whose releases are being measured, or zero.
static RELEASE_TASK: AtomicU64 = AtomicU64::new(0);
/// The lowest address of a local in a release of that task.
static RELEASE_LOWEST: AtomicUsize = AtomicUsize::new(usize::MAX);
/// The highest.
static RELEASE_HIGHEST: AtomicUsize = AtomicUsize::new(0);

/// Start measuring how much stack the running task's releases of filters use.
pub(crate) fn measure_releases() {
    RELEASE_LOWEST.store(usize::MAX, Ordering::Relaxed);
    RELEASE_HIGHEST.store(0, Ordering::Relaxed);
    RELEASE_TASK.store(sched::current_id().unwrap_or(0), Ordering::Relaxed);
}

/// Stop measuring, and answer how many bytes of stack the releases spread over:
/// a few hundred for a walk, a frame for each filter of a recursive drop.
pub(crate) fn release_stack_span() -> usize {
    RELEASE_TASK.store(0, Ordering::Relaxed);
    RELEASE_HIGHEST.load(Ordering::Relaxed).saturating_sub(
        RELEASE_LOWEST
            .load(Ordering::Relaxed)
            .min(RELEASE_HIGHEST.load(Ordering::Relaxed)),
    )
}

/// The `seccomp_data` a call becomes.
///
/// The architecture token is the entry's own (SR1): the same [`Abi`] that
/// picks the table, never the image the process runs. A number is judged as
/// its low 32 bits, as Linux reads it (`int nr`); the dispatcher uses the
/// whole register, so a number with upper bits set is in no table and is
/// `ENOSYS` whatever the filter said, and a number is either dispatched as the
/// value the filter judged or not at all (SR2). The native range is decided by
/// the dispatcher's own predicate, so the token and the route agree.
pub(crate) fn data(args: &SyscallArgs) -> SeccompData {
    let native = args.abi == Abi::Native && ferrix_native_abi::nr::is_native(args.number);
    SeccompData {
        nr: args.number as i32,
        arch: if native {
            NATIVE_ARCH
        } else {
            arch::audit_arch(args.abi)
        },
        instruction_pointer: args.ip,
        args: args.args,
    }
}

/// What the hook decided, before the core sees it.
enum Decision {
    /// Answer the core with this.
    Verdict(Verdict),
    /// The thread ends, through the path `exit` takes, with this status: 128
    /// plus the signal, as Linux's `do_exit(SIGSYS)` leaves it, so that a
    /// leader ended this way is what `wait` reports for its process. Not
    /// returned from the hook's own frame: every reference is dropped first.
    Leave(i32),
}

/// Whether any thread has ever held a filter. Set once and never cleared: until
/// one has, no call of any program looks for its thread, and the hook costs the
/// registration's load and this flag's.
static EVER_FILTERED: AtomicBool = AtomicBool::new(false);

/// Note that a thread now holds seccomp state, for [`check`]'s fast path.
pub(crate) fn note_filtered() {
    EVER_FILTERED.store(true, Ordering::Release);
}

/// The function the core asks about every system call.
///
/// Allocates nothing and takes no sleeping lock: it runs on every call of
/// every program. A kernel in which no thread has installed a filter costs two
/// loads (this flag and the boot check's probe word); a thread with none costs
/// the running task, its thread and one more load; the rest is for a thread
/// that has one. The chain is walked with interrupts open and under no lock.
pub(crate) fn check(args: &SyscallArgs) -> Verdict {
    if PROBE_TASK.load(Ordering::Relaxed) != 0 {
        let verdict = probed(args);
        if verdict != Verdict::Continue {
            return verdict;
        }
    }
    if !EVER_FILTERED.load(Ordering::Acquire) {
        return Verdict::Continue;
    }
    let Some(thread) = thread::current() else {
        return Verdict::Continue;
    };
    if !thread.is_filtered() {
        return Verdict::Continue;
    }
    // A filter chain can take tens of microseconds and a kill needs the locks
    // `exit` takes, and the entry opens them a few instructions later anyway.
    // The hook returns with them masked again, as the core's contract has it.
    arch::enable_interrupts();
    let decision = decide(&thread, args);
    arch::disable_interrupts();
    // The thread is dropped before it leaves: nothing after the task ends runs
    // to drop it.
    drop(thread);
    match decision {
        Decision::Verdict(verdict) => verdict,
        Decision::Leave(status) => process::exit_thread_current(status),
    }
}

/// Judge the call `args` describes for `thread`, which may be any thread, not
/// only the running one: what [`check`] does, less the interrupts and the
/// ending of a thread, so that a boot check can ask what a thread it holds
/// would be told. `None` is "ends the thread".
pub(crate) fn judge(thread: &Thread, args: &SyscallArgs) -> Option<Verdict> {
    match decide(thread, args) {
        Decision::Verdict(verdict) => Some(verdict),
        Decision::Leave(_) => None,
    }
}

/// The boot check's rule: judges the calls of one armed task and no other's.
type Rule = fn(&SeccompData) -> Verdict;

/// The task the armed probe judges, as an address; zero when none is armed.
static PROBE_TASK: AtomicUsize = AtomicUsize::new(0);

/// The armed probe's rule.
static PROBE_RULE: SpinLock<Option<Rule>> = SpinLock::new(None);

/// Judge the calls the running task makes with `rule`, until [`disarm_probe`].
///
/// Test-only, for the boot check that drives the four entries with frames of
/// its own and reads what the filter was shown, which a filter program cannot
/// say: a call made by any other task is unaffected. Only one probe can be
/// armed at a time.
pub(crate) fn arm_probe(rule: Rule) {
    *PROBE_RULE.lock() = Some(rule);
    PROBE_TASK.store(task_key(), Ordering::Release);
}

/// Stop judging by the probe.
pub(crate) fn disarm_probe() {
    PROBE_TASK.store(0, Ordering::Release);
    *PROBE_RULE.lock() = None;
}

/// Who is running, as a number that is never zero: the running task's address,
/// or 1 where no task is (the boot context before the scheduler has one).
fn task_key() -> usize {
    sched::current().map_or(1, |task| Arc::as_ptr(&task) as usize)
}

/// The slow path of [`check`]: a probe is armed, and this call may be its
/// task's.
fn probed(args: &SyscallArgs) -> Verdict {
    let armed = PROBE_TASK.load(Ordering::Acquire);
    if armed == 0 || armed != task_key() {
        return Verdict::Continue;
    }
    let rule = *PROBE_RULE.lock();
    rule.map_or(Verdict::Continue, |rule| rule(&data(args)))
}

/// Judge one call for a thread and decide what happens: the thread's mode, then
/// its chain.
fn decide(thread: &Thread, args: &SyscallArgs) -> Decision {
    evaluate(thread, args)
}

/// Judge the call `args` describes for `thread`, which is filtered.
fn evaluate(thread: &Thread, args: &SyscallArgs) -> Decision {
    let (mode, head) = thread.read_seccomp(|state| (state.mode, state.filter.clone()));
    match mode {
        Mode::Disabled => Decision::Verdict(Verdict::Continue),
        Mode::Strict => strict(thread, args),
        Mode::Dead => kill_thread(thread, args, SIGKILL, false),
        Mode::Filter => {
            // A thread in filter mode holds at least one filter. One that
            // holds none is a defect here, and Linux answers it with a kill
            // (`seccomp_run_filters`'s `WARN_ON`): never "allow".
            let Some(head) = head else {
                return kill_process(thread, args, KILL_PROCESS);
            };
            let data = data(args);
            let (result, log) = run_chain(&head, &data);
            // The chain is released here, after the judging and under no lock:
            // it may have been the last reference to a filter.
            drop(head);
            act(thread, args, result, log)
        }
    }
}

/// Run every filter of the chain from `head`, newest first, and answer the most
/// restrictive result and whether the filter it came from logs: Linux's
/// `seccomp_run_filters`. On a tie the newer filter's result stands.
fn run_chain(head: &Arc<Filter>, data: &SeccompData) -> (u32, bool) {
    let mut best = ALLOW;
    let mut log = false;
    let mut cursor = Some(head);
    while let Some(filter) = cursor {
        let result = ferrix_seccomp::run(&filter.program, data);
        if ferrix_seccomp::more_restrictive(result, best) {
            best = result;
            log = filter.log;
        }
        cursor = filter.previous.as_ref();
    }
    (best, log)
}

/// `SIGSYS`, which ends a thread a filter killed.
const SIGSYS: u32 = 31;
/// `SIGKILL`, which ends a thread in strict mode.
const SIGKILL: u32 = 9;

/// `ENOSYS`: what a call no tracer or listener can judge fails with, and what a
/// call of a thread that is being ended is told on its way out.
const ENOSYS: u32 = 38;

/// Do what a filter's `result` says about the call.
fn act(thread: &Thread, args: &SyscallArgs, result: u32, log: bool) -> Decision {
    let action = result & ACTION_FULL;
    let data = result & DATA;
    match action {
        ALLOW => Decision::Verdict(Verdict::Continue),
        LOG => {
            report(thread, args, "log", action);
            Decision::Verdict(Verdict::Continue)
        }
        ERRNO => {
            if log {
                report(thread, args, "errno", action);
            }
            // The errno a filter chose, as it chose it: the core caps it at
            // Linux's `MAX_ERRNO` and writes it straight into the return
            // register, never through the dispatcher's restart handling, so
            // that a filter's 512 is `-512` for the program and not a
            // restarted call (SR7).
            Decision::Verdict(Verdict::Errno(data))
        }
        // There is no tracer and no listener to ask, which is what Linux
        // answers when nobody asked for one: `ENOSYS`, and never "allow"
        // (SR13).
        TRACE | USER_NOTIF => {
            if log {
                report(thread, args, "unanswerable", action);
            }
            Decision::Verdict(Verdict::Errno(ENOSYS))
        }
        TRAP => trap(thread, args, data, log),
        KILL_THREAD => kill_thread(thread, args, SIGSYS, true),
        // `KILL_PROCESS`, and an action nobody defined, which Linux counts as
        // `KILL_PROCESS` too.
        _ => kill_process(thread, args, action),
    }
}

/// `SECCOMP_RET_TRAP`: the call does not run, `SIGSYS` is forced on the calling
/// thread -- unblocked, and reset to its default if it was ignored or blocked,
/// as Linux's `force_sig_seccomp` does -- and the core gives the registers
/// back as the program made the call (`Verdict::Trap`), so that the signal is
/// delivered on the way out of this very call, before any other instruction of
/// the program runs, with a context in which a handler finds the call. A
/// program that blocks or ignores `SIGSYS` cannot escape a trap: it dies of it
/// (SR9). `si_errno` is the filter's data, `si_call_addr` the instruction
/// after the call, `si_syscall` the number and `si_arch` the entry's token.
fn trap(thread: &Thread, args: &SyscallArgs, errno: u32, log: bool) -> Decision {
    if log {
        report(thread, args, "trap", TRAP);
    }
    let seen = data(args);
    // A native-range call is not a Linux call, and `SIGSYS` is a Linux signal
    // the native ABI cannot express: a filter that traps one gets the most
    // restrictive answer, `KILL_PROCESS`, as it does for an action nobody
    // defined (the consultant's S4 ruling).
    if seen.arch == NATIVE_ARCH {
        return kill_process(thread, args, TRAP);
    }
    let origin = Origin::Sys {
        errno,
        call_addr: seen.instruction_pointer,
        syscall: seen.nr,
        arch: seen.arch,
    };
    // The running thread's own: the hook runs on the thread that made the call.
    match deliver::force(SIGSYS, origin) {
        // Nothing to raise it against, or the process is ended already (the
        // signal's default action, with no handler, is fatal): not a program
        // that may go on.
        None | Some(Posted::Fatal) => kill_process(thread, args, TRAP),
        Some(_) => Decision::Verdict(Verdict::Trap),
    }
}

/// End the whole process with `SIGSYS`. The calling thread finds out on its
/// way back to user mode, where it never arrives.
fn kill_process(thread: &Thread, args: &SyscallArgs, action: u32) -> Decision {
    report(thread, args, "kill process", action);
    thread.with_seccomp(|state| state.mode = Mode::Dead);
    process::kill(thread.process(), 128 + SIGSYS as i32);
    Decision::Verdict(Verdict::Errno(ENOSYS))
}

/// End the calling thread as if killed by `signal`. If it is the last live
/// thread of its process the whole process is killed by it instead, as
/// Linux's exit of the last thread is the group's.
fn kill_thread(thread: &Thread, args: &SyscallArgs, signal: u32, logged: bool) -> Decision {
    if logged {
        report(thread, args, "kill thread", KILL_THREAD);
    }
    thread.with_seccomp(|state| state.mode = Mode::Dead);
    let process = thread.process();
    if process.live_thread_count() <= 1 {
        process::kill(process, 128 + signal as i32);
        return Decision::Verdict(Verdict::Errno(ENOSYS));
    }
    Decision::Leave(128 + signal as i32)
}

/// Strict mode: `read`, `write`, `exit` and the signal return of the entry's
/// own ABI, and `SIGKILL` for anything else.
fn strict(thread: &Thread, args: &SyscallArgs) -> Decision {
    let call = match args.abi {
        Abi::Native => arch::decode_syscall(args.number),
        Abi::Compat => arch::decode_compat_syscall(args.number),
    };
    let allowed = match call {
        Some(Syscall::Read | Syscall::Write | Syscall::Exit) => true,
        // x86-64, AArch64 and EABI return from a handler through
        // `rt_sigreturn`, i386 through `sigreturn`: Linux's
        // `__NR_seccomp_sigreturn` of each.
        Some(Syscall::RtSigreturn) => args.abi == Abi::Native,
        Some(Syscall::Sigreturn) => args.abi == Abi::Compat,
        _ => false,
    };
    if allowed {
        return Decision::Verdict(Verdict::Continue);
    }
    kill_thread(thread, args, SIGKILL, true)
}

/// Lines a second the log takes: a program in a loop under a filter that
/// logs must not be able to fill the console.
const LOG_PER_SECOND: u32 = 10;
/// When the current second of the log began, in nanoseconds.
static LOG_SECOND: AtomicU64 = AtomicU64::new(0);
/// Lines written in it.
static LOG_LINES: AtomicU32 = AtomicU32::new(0);

/// One line on the kernel log for a call a filter judged: rate-limited, with
/// the process, the architecture token, the number and the instruction. The
/// command name is left out, because it is a heap allocation and the hook
/// makes none
/// (`docs/SECCOMP.md` §3.5a, Q4). Linux writes these to its audit log; Ferrix's
/// `audit.rs` records the item's decisions, and a filter's is the
/// personality's.
fn report(thread: &Thread, args: &SyscallArgs, what: &str, action: u32) {
    let now = crate::timer::now_nanos();
    let started = LOG_SECOND.load(Ordering::Relaxed);
    if now.saturating_sub(started) >= 1_000_000_000 {
        LOG_SECOND.store(now, Ordering::Relaxed);
        LOG_LINES.store(0, Ordering::Relaxed);
    }
    if LOG_LINES.fetch_add(1, Ordering::Relaxed) >= LOG_PER_SECOND {
        return;
    }
    let data = data(args);
    let process = thread.process();
    println!(
        "  seccomp  {what} ({action:#010x}): pid {} arch {:#x} syscall {} at {:#x}",
        process.pid(),
        data.arch,
        data.nr,
        data.instruction_pointer,
    );
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

/// Whether `process` may install a filter that is not its own to keep:
/// it has set no-new-privs, or holds `CAP_SYS_ADMIN` in its own user namespace,
/// as Linux's `ns_capable(current_user_ns(), CAP_SYS_ADMIN)` asks. The capability
/// only lets a program skip no-new-privs, and a filter can only take away, so a
/// root inside a child namespace gains nothing by it.
pub(crate) fn may_install(process: &Process) -> bool {
    attributes::get(process).no_new_privs
        || process.with_credentials(|held| held.holds(userns::CAP_SYS_ADMIN))
}

/// A filter of `raw`, verified and charged to the running task's job, attached
/// to nobody.
///
/// The charge is made before the program is copied into the heap and the verifier's
/// working memory is allocated, so that a job at its limit is refused
/// `ENOMEM` before it has cost the machine more than this call's bytes
/// (F-37). The persistent part is the filter's record and its program.
///
/// # Errors
///
/// `EINVAL` for a program Linux's verifier refuses, `ENOMEM` past the job's
/// memory limit or the machine's.
pub(crate) fn prepare(raw: &[Insn], log: bool) -> Result<Arc<Filter>, Errno> {
    // What the verifier allocates and frees: a mask of sixteen bits an
    // instruction. It is one charged allocation per install and not a stack
    // array, because 4096 of them is too much for a kernel stack.
    let scratch = Charge::bytes(buffer_footprint::<u16>(raw.len())).map_err(|_| Errno::ENOMEM)?;
    let charge = Charge::bytes(
        arc_footprint::<Filter>().saturating_add(buffer_footprint::<Insn>(raw.len())),
    )
    .map_err(|_| Errno::ENOMEM)?;
    let program = ferrix_seccomp::verify(raw).map_err(|_| Errno::EINVAL)?;
    drop(scratch);
    let cost = u32::try_from(program.len().saturating_add(4)).map_err(|_| Errno::ENOMEM)?;
    crate::fallible::try_arc(Filter {
        program,
        log,
        previous: None,
        cost,
        depth: 1,
        _charge: charge,
    })
    .map_err(|_| Errno::ENOMEM)
}

/// Attach `filter`, which [`prepare`] made and nobody else holds, to `thread`
/// as its newest: the mode becomes `Filter`, and the chain grows by it.
///
/// Everything that can fail has failed before this takes the thread's lock,
/// and nothing is allocated or freed under it: the filter exists already, and
/// what the lock hands back is dropped by the caller after it.
///
/// # Errors
///
/// `EINVAL` if the thread is in strict mode (Linux's
/// `seccomp_may_assign_mode`), `ENOMEM` if the chain would pass
/// `MAX_INSNS_PER_PATH` (Linux's `seccomp_attach_filter`).
pub(crate) fn attach(thread: &Thread, mut filter: Arc<Filter>) -> Result<(), Errno> {
    attach_to(thread, &mut filter)
    // A refused filter is dropped with the argument, outside the lock.
}

/// [`attach`], leaving the caller holding the filter as well, so that it can be
/// attached to other threads as the same chain (`TSYNC`).
fn attach_to(thread: &Thread, filter: &mut Arc<Filter>) -> Result<(), Errno> {
    thread.with_seccomp(|state| {
        if matches!(state.mode, Mode::Strict | Mode::Dead) {
            return Err(Errno::EINVAL);
        }
        // The instructions of the new filter, and of every one before it with
        // four more each: Linux's bound on what one call may run, which the
        // crate states (`fits_path`).
        let before = state.filter.as_ref().map_or(0, |newest| newest.cost);
        let program = filter.program.len();
        let earlier = usize::try_from(before).unwrap_or(usize::MAX);
        if !ferrix_seccomp::fits_path(earlier, program) {
            return Err(Errno::ENOMEM);
        }
        // Nobody else holds the new filter, so this cannot fail; if it did,
        // the filter would simply not be attached.
        let Some(mine) = Arc::get_mut(filter) else {
            return Err(Errno::ENOMEM);
        };
        mine.cost = u32::try_from(ferrix_seccomp::path_cost(earlier, program)).unwrap_or(u32::MAX);
        mine.depth = state.filter.as_ref().map_or(0, |newest| newest.depth) + 1;
        mine.previous = state.filter.take();
        state.filter = Some(Arc::clone(filter));
        state.mode = Mode::Filter;
        Ok(())
    })
}

/// `seccomp(operation, flags, uargs)` for the calling thread of `process`.
///
/// # Errors
///
/// As Linux's `do_seccomp`: `EINVAL` for an operation, flag or program it
/// refuses, `EACCES` without no-new-privs or privilege, `EFAULT`, `ENOMEM`,
/// `EOPNOTSUPP` for an action that is not available, `ESRCH` from a caller
/// that is no thread of `process`.
pub(crate) fn sys_seccomp(
    process: &Process,
    operation: u64,
    flags: u64,
    uargs: u64,
    abi: Abi,
) -> Result<usize, Errno> {
    let thread = thread::current_of(process).ok_or(Errno::ESRCH)?;
    do_seccomp(process, &thread, operation, flags, uargs, abi)
}

/// [`sys_seccomp`] for `thread`, which the caller has found.
pub(crate) fn do_seccomp(
    process: &Process,
    thread: &Thread,
    operation: u64,
    flags: u64,
    uargs: u64,
    abi: Abi,
) -> Result<usize, Errno> {
    match operation {
        op::SET_MODE_STRICT => {
            if flags != 0 || uargs != 0 {
                return Err(Errno::EINVAL);
            }
            set_strict(thread)
        }
        op::SET_MODE_FILTER => set_filter(process, thread, flags, uargs, abi),
        op::GET_ACTION_AVAIL => {
            if flags != 0 {
                return Err(Errno::EINVAL);
            }
            action_available(process, uargs)
        }
        op::GET_NOTIF_SIZES => {
            if flags != 0 {
                return Err(Errno::EINVAL);
            }
            notif_sizes(process, uargs)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `prctl(PR_SET_SECCOMP, mode, filter)`: strict mode, or a filter with no
/// flags.
pub(crate) fn prctl_set(
    process: &Process,
    thread: &Thread,
    mode: u64,
    filter: u64,
    abi: Abi,
) -> Result<usize, Errno> {
    match mode {
        1 => set_strict(thread),
        2 => set_filter(process, thread, 0, filter, abi),
        _ => Err(Errno::EINVAL),
    }
}

/// `prctl(PR_GET_SECCOMP)`: the calling thread's mode, 0 or 2 (and 1 in strict
/// mode, where the call itself has already killed the caller).
pub(crate) fn prctl_get(process: &Process) -> Result<usize, Errno> {
    let thread = thread::current_of(process).ok_or(Errno::ESRCH)?;
    Ok(thread.read_seccomp(|state| state.mode.number()) as usize)
}

/// The calls `seccomp` and `prctl`'s seccomp options are, for the personality's
/// dispatch: `None` for any other call, which is not one of mine.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    match call {
        // `op` and `flags` are `unsigned int` in Linux's prototype: a program
        // that passes a wider value is judged by its low half.
        Syscall::Seccomp => Some(sys_seccomp(
            process,
            a[0] & 0xFFFF_FFFF,
            a[1] & 0xFFFF_FFFF,
            a[2],
            abi,
        )),
        Syscall::Prctl => match a[0] as u32 as i32 {
            option::PR_GET_SPECULATION_CTRL => Some(speculation_get(a[1], [a[2], a[3], a[4]])),
            option::PR_SET_SPECULATION_CTRL => Some(speculation_set(a[1], a[2], [a[3], a[4]])),
            option::PR_GET_SECCOMP => Some(prctl_get(process)),
            option::PR_SET_SECCOMP => Some(
                thread::current_of(process)
                    .ok_or(Errno::ESRCH)
                    .and_then(|thread| prctl_set(process, &thread, a[1], a[2], abi)),
            ),
            _ => None,
        },
        _ => None,
    }
}

/// `SECCOMP_SET_MODE_STRICT`: refused once filters are in.
fn set_strict(thread: &Thread) -> Result<usize, Errno> {
    thread.with_seccomp(|state| match state.mode {
        Mode::Filter | Mode::Dead => Err(Errno::EINVAL),
        Mode::Disabled | Mode::Strict => {
            state.mode = Mode::Strict;
            Ok(0)
        }
    })
}

/// `SECCOMP_SET_MODE_FILTER`, in Linux's order (`seccomp_set_mode_filter`,
/// `seccomp_prepare_filter`): flags, then the program's header (`EFAULT` for a
/// null pointer, which is Chromium's probe), its length, privilege, its body,
/// the verifier, the mode, and the chain's length.
fn set_filter(
    process: &Process,
    thread: &Thread,
    flags: u64,
    uargs: u64,
    abi: Abi,
) -> Result<usize, Errno> {
    if flags & !flag::BUILT != 0 {
        return Err(Errno::EINVAL);
    }
    // Linux: `TSYNC_ESRCH` is a way of reporting `TSYNC`'s failure, not a flag
    // of its own.
    if flags & flag::TSYNC_ESRCH != 0 && flags & flag::TSYNC == 0 {
        return Err(Errno::EINVAL);
    }
    let (length, pointer) = read_header(process, uargs, abi)?;
    if length == 0 || length > MAX_INSNS {
        return Err(Errno::EINVAL);
    }
    if !may_install(process) {
        return Err(Errno::EACCES);
    }
    // Charged before the copy, and dropped with it: the bytes read from the
    // program and the instructions made of them are this call's own.
    let _transient = Charge::bytes(
        buffer_footprint::<u8>(length * ferrix_seccomp::INSN_BYTES)
            .saturating_add(buffer_footprint::<Insn>(length)),
    )
    .map_err(|_| Errno::ENOMEM)?;
    let raw = read_program(process, pointer, length)?;
    let filter = prepare(&raw, flags & flag::LOG != 0)?;
    drop(raw);
    if flags & flag::TSYNC != 0 {
        return attach_sync(process, thread, filter, flags & flag::TSYNC_ESRCH != 0);
    }
    attach(thread, filter)?;
    Ok(0)
}

/// Give every thread of the process the filter, or none: `Ok(0)` when every
/// thread has it, `Ok(tid)` of the first thread that could not take it, or an
/// errno.
///
/// # What it does (`docs/SECCOMP.md` §3.7)
///
/// With the filter made and charged but attached to nobody, it takes the
/// process's thread-list lock, which is what `add_thread_from` holds to list a
/// new thread and copy its creator's chain, and which `Process::threads`
/// holds to list them. Under it:
/// 1. an `execve` that has claimed the process answers `EAGAIN`;
/// 2. every other live thread is checked under its leaf lock: one with no
///    seccomp passes, and so does one whose chain is an ancestor of the
///    caller's; a thread in strict mode or with a chain of its own fails the
///    whole call, answering that thread's id (or `ESRCH` with `TSYNC_ESRCH`)
///    with no thread changed;
/// 3. the filter is attached to the caller as `attach` does, and then every
///    other thread's chain becomes the caller's new one.
///
/// The old chains a thread gave up are dropped after the lock is released,
/// and nothing is allocated under it: the list of threads and the room for
/// those chains are made first.
///
/// # Errors
///
/// `ENOMEM` for the room, the filter's chain bound or the charge; `EAGAIN`
/// for an `execve` in progress; `ESRCH` with `TSYNC_ESRCH`; `EINVAL` if the
/// caller is in strict mode.
pub(crate) fn attach_sync(
    process: &Process,
    caller: &Thread,
    mut filter: Arc<Filter>,
    esrch: bool,
) -> Result<usize, Errno> {
    let mut held: Vec<Arc<Thread>> = Vec::new();
    let mut gave: Vec<Option<Arc<Filter>>> = Vec::new();
    let mut room = 8_usize;
    loop {
        held.try_reserve_exact(room).map_err(|_| Errno::ENOMEM)?;
        gave.try_reserve_exact(room).map_err(|_| Errno::ENOMEM)?;
        let answer = process.with_live_threads(&mut held, |threads| {
            synchronise(process, caller, threads, &mut filter, &mut gave, esrch)
        });
        match answer {
            Ok(answer) => {
                // The locks are released: the chains the threads gave up and
                // the references to the threads go now, here.
                drop(gave);
                drop(held);
                return answer;
            }
            Err(listed) => room = listed.saturating_add(4),
        }
    }
}

/// [`attach_sync`]'s part under the thread-list lock.
fn synchronise(
    process: &Process,
    caller: &Thread,
    threads: &[Arc<Thread>],
    filter: &mut Arc<Filter>,
    gave: &mut Vec<Option<Arc<Filter>>>,
    esrch: bool,
) -> Result<usize, Errno> {
    if process.exec_claimed() {
        return Err(Errno::EAGAIN);
    }
    let others = || {
        threads
            .iter()
            .filter(|thread| !core::ptr::eq(Arc::as_ptr(thread), caller))
    };
    let mine = caller.with_seccomp(|state| state.filter.clone());
    for thread in others() {
        let fits = thread.with_seccomp(|state| match state.mode {
            Mode::Disabled => true,
            Mode::Filter => is_ancestor(state.filter.as_ref(), mine.as_ref()),
            Mode::Strict | Mode::Dead => false,
        });
        if !fits {
            return if esrch {
                Err(Errno::ESRCH)
            } else {
                Ok(thread.tid() as usize)
            };
        }
    }
    attach_to(caller, filter)?;
    for thread in others() {
        thread.with_seccomp(|state| {
            // Reserved by the caller: this cannot allocate.
            gave.push(state.filter.replace(Arc::clone(filter)));
            state.mode = Mode::Filter;
        });
    }
    Ok(0)
}

/// Whether `candidate` is `walk` or a filter behind it: Linux's
/// `is_ancestor`. A chain that is none is an ancestor of every chain.
fn is_ancestor(candidate: Option<&Arc<Filter>>, walk: Option<&Arc<Filter>>) -> bool {
    let Some(candidate) = candidate else {
        return true;
    };
    let mut cursor = walk;
    while let Some(filter) = cursor {
        if Arc::ptr_eq(filter, candidate) {
            return true;
        }
        cursor = filter.previous.as_ref();
    }
    false
}

/// The `struct sock_fprog` at `at`: a 16-bit length and a pointer to that many
/// instructions. The structure is 16 bytes with the pointer at 8 for a 64-bit
/// program, 8 bytes with it at 4 for a 32-bit one, which is an i386 call on
/// x86-64 as well as the Arm architecture's own.
fn read_header(process: &Process, at: u64, abi: Abi) -> Result<(usize, u64), Errno> {
    let wide = size_of::<usize>() == 8 && abi == Abi::Native;
    let mut head = [0_u8; 16];
    let used = head
        .get_mut(..if wide { 16 } else { 8 })
        .ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, used).map_err(|_| Errno::EFAULT)?;
    let [l0, l1, ..] = head;
    let length = usize::from(u16::from_le_bytes([l0, l1]));
    let pointer = if wide {
        let [_, _, _, _, _, _, _, _, p0, p1, p2, p3, p4, p5, p6, p7] = head;
        u64::from_le_bytes([p0, p1, p2, p3, p4, p5, p6, p7])
    } else {
        let [_, _, _, _, p0, p1, p2, p3, ..] = head;
        u64::from(u32::from_le_bytes([p0, p1, p2, p3]))
    };
    Ok((length, pointer))
}

/// `length` instructions copied from the program's memory at `at`.
fn read_program(process: &Process, at: u64, length: usize) -> Result<Vec<Insn>, Errno> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length * ferrix_seccomp::INSN_BYTES)
        .map_err(|_| Errno::ENOMEM)?;
    bytes.resize(length * ferrix_seccomp::INSN_BYTES, 0);
    uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut program = Vec::new();
    program
        .try_reserve_exact(length)
        .map_err(|_| Errno::ENOMEM)?;
    program.extend(
        bytes
            .chunks_exact(ferrix_seccomp::INSN_BYTES)
            .filter_map(Insn::from_bytes),
    );
    Ok(program)
}

/// `SECCOMP_GET_ACTION_AVAIL`: whether a filter may return the action at
/// `at`. `EOPNOTSUPP` for `USER_NOTIF`, which is not built, and for a value
/// that is no action.
fn action_available(process: &Process, at: u64) -> Result<usize, Errno> {
    let action = uaccess::get_u32(process.space(), at).map_err(|_| Errno::EFAULT)?;
    match action {
        KILL_PROCESS | KILL_THREAD | TRAP | ERRNO | TRACE | LOG | ALLOW => Ok(0),
        // `USER_NOTIF` needs a listener, which is not built.
        _ => Err(Errno::EOPNOTSUPP),
    }
}

/// `SECCOMP_GET_NOTIF_SIZES`: the sizes of the three structures a supervisor
/// would use, which cost nothing to state. A program that then asks for a
/// listener is refused `EINVAL` when it installs.
fn notif_sizes(process: &Process, at: u64) -> Result<usize, Errno> {
    // `struct seccomp_notif_sizes { __u16 seccomp_notif; __u16
    // seccomp_notif_resp; __u16 seccomp_data; }`: 80, 24 and 64 bytes.
    let sizes: [u8; 6] = [80, 0, 24, 0, 64, 0];
    uaccess::copy_to_user(process.space(), at, &sizes)
        .map(|()| 0)
        .map_err(|_| Errno::EFAULT)
}

/// Give a native child, `child`, which the native `process_create` has just
/// made for `creator`, the seccomp mode and chain of the creating thread. A
/// native process can make Linux calls, so without this one call would leave
/// the sandbox (`docs/SECCOMP.md` §3.3; `docs/NAMESPACES.md` §2.5 found the
/// same hole for the mount namespace). A creator that is not running as a
/// thread of its own process -- the kernel's own start of a process -- has
/// none to give.
pub(crate) fn inherit_native(creator: &Process, child: &Process) {
    let Some(thread) = thread::current_of(creator) else {
        return;
    };
    let state = thread.seccomp_copy();
    if state.is_active() {
        child.set_first_seccomp(state);
    }
}
