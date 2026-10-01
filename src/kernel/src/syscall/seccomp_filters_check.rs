//! seccomp's filters, proved at boot (`docs/SECCOMP.md` §8.1, S3): who may
//! install one, what a filter makes of a call, which of a chain's answers
//! wins, who inherits it, what a thread that is killed leaves behind, and what
//! a chain costs.
//!
//! Most of it runs in a task of its own that is a real thread of a check
//! process, because the hook finds the thread of the task that makes the call:
//! the task installs filters through `seccomp(2)` and `prctl` and makes the
//! calls the filters judge through the core's own entry
//! (`arch::drive_system_call`), as a program does. What does not need to run
//! (a fork child's or a thread's chain, an unprivileged process's refusal) is
//! asked of the thread directly.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_seccomp::{
    ALLOW, ERRNO, Insn, KILL_PROCESS, KILL_THREAD, LOG, MAX_INSNS, MAX_INSNS_PER_PATH, TRACE,
    USER_NOTIF,
};

use crate::arch;
use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::check::map_rw;
use crate::syscall::process::{self, Process};
use crate::syscall::seccomp::{self, State};
use crate::syscall::thread::Thread;
use crate::syscall::{attributes, credentials, launch, uaccess};
use crate::trap::{Abi, SyscallArgs, Verdict};

/// What the check saw, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Calls a filtered thread made through the core's entry.
    pub(crate) calls: usize,
    /// Answers `seccomp(2)` and `prctl` gave to what Chromium's probes ask.
    pub(crate) probes: usize,
    /// Children and threads that judged a call as their creator's chain did.
    pub(crate) inherited: usize,
    /// Processes a filter ended with `SIGSYS` or `SIGKILL`.
    pub(crate) killed: usize,
    /// Filters in the longest chain made and released.
    pub(crate) chain: usize,
    /// Traps delivered as a signal with the call's information, read back, and
    /// a blocked and ignored one that ended its process.
    pub(crate) traps: usize,
}

/// `PR_SET_NO_NEW_PRIVS` and `PR_GET_NO_NEW_PRIVS`.
const NO_NEW_PRIVS: i32 = 38;

/// `seccomp(2)`'s operations and flags, as `linux/seccomp.h` has them.
const SET_MODE_STRICT: u64 = 0;
/// See [`SET_MODE_STRICT`].
const SET_MODE_FILTER: u64 = 1;
/// See [`SET_MODE_STRICT`].
const GET_ACTION_AVAIL: u64 = 2;
/// See [`SET_MODE_STRICT`].
const GET_NOTIF_SIZES: u64 = 3;
/// `SECCOMP_FILTER_FLAG_TSYNC`: not built until the landing that synchronises
/// threads.
const TSYNC: u64 = 1 << 0;
/// `SECCOMP_FILTER_FLAG_LOG`.
const FLAG_LOG: u64 = 1 << 1;
/// `SECCOMP_FILTER_FLAG_SPEC_ALLOW`.
const SPEC_ALLOW: u64 = 1 << 2;
/// `SECCOMP_FILTER_FLAG_NEW_LISTENER`.
const NEW_LISTENER: u64 = 1 << 3;
/// `SECCOMP_FILTER_FLAG_TSYNC_ESRCH`.
const TSYNC_ESRCH: u64 = 1 << 4;
/// `SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV`.
const WAIT_KILLABLE_RECV: u64 = 1 << 5;
/// A bit Linux's headers do not name.
const UNKNOWN_FLAG: u64 = 1 << 6;

/// `SECCOMP_RET_TRAP`: not built until `SIGSYS` is delivered.
const TRAP: u32 = ferrix_seccomp::TRAP;

/// `EPERM`, the errno the filters in these checks give.
const EPERM: u32 = 1;
/// `ENOSYS`.
const ENOSYS: u32 = 38;

/// A filter that answers `action` for each call number named and allows every
/// other: `ld [0]`, then for each `jeq nr` the action to return.
fn answering(calls: &[(usize, u32)]) -> Vec<Insn> {
    let mut program = vec![Insn::new(0x20, 0, 0, 0)];
    for &(number, action) in calls {
        program.push(Insn::new(0x15, 0, 1, number as u32));
        program.push(Insn::new(0x06, 0, 0, action));
    }
    program.push(Insn::new(0x06, 0, 0, ALLOW));
    program
}

/// The number `call` has on this architecture's own entry.
fn number(call: Syscall) -> Result<usize, &'static str> {
    (0..=1024)
        .find(|&candidate| arch::decode_syscall(candidate) == Some(call))
        .ok_or("the decoder has no number for a call the seccomp check uses")
}

/// A process of the check's own with one thread and a page of its memory.
struct Env {
    /// The process.
    process: Arc<Process>,
    /// Its first thread.
    thread: Arc<Thread>,
    /// A page of its memory the check writes programs into.
    page: u64,
}

impl Env {
    /// A new process, its leader thread (listed, so `status` finds it) and a
    /// page.
    fn new() -> Result<Env, &'static str> {
        let process =
            process::new_for_check().map_err(|_| "no process for the seccomp filters check")?;
        let thread = Arc::new(
            Thread::leader(&process).map_err(|_| "no memory for a seccomp check's thread")?,
        );
        process.add_thread(&thread);
        let page = map_rw(&process, PAGE_SIZE)?;
        Ok(Env {
            process,
            thread,
            page,
        })
    }

    /// Write a `struct sock_fprog` at the start of the page and the program
    /// after it, and answer where the `sock_fprog` is. Its length field is the
    /// program's, unless `claimed` says another.
    fn put(&self, program: &[Insn], claimed: Option<u16>) -> Result<u64, &'static str> {
        const HEADER: u64 = 64;
        let wide = size_of::<usize>() == 8;
        let mut bytes: Vec<u8> = Vec::new();
        let length = claimed.unwrap_or(program.len() as u16);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.resize(if wide { 8 } else { 4 }, 0);
        let at = self.page + HEADER;
        if wide {
            bytes.extend_from_slice(&at.to_le_bytes());
        } else {
            bytes.extend_from_slice(&(at as u32).to_le_bytes());
        }
        uaccess::copy_to_user(self.process.space(), self.page, &bytes)
            .map_err(|_| "a seccomp check could not write its program header")?;
        let mut code: Vec<u8> = Vec::new();
        for insn in program {
            code.extend_from_slice(&insn.code.to_le_bytes());
            code.push(insn.jt);
            code.push(insn.jf);
            code.extend_from_slice(&insn.k.to_le_bytes());
        }
        if !code.is_empty() {
            uaccess::copy_to_user(self.process.space(), at, &code)
                .map_err(|_| "a seccomp check could not write its program")?;
        }
        Ok(self.page)
    }

    /// `seccomp(operation, flags, ...)` as this check's thread, directly.
    fn seccomp(&self, operation: u64, flags: u64, uargs: u64) -> Result<usize, Errno> {
        seccomp::do_seccomp(
            &self.process,
            &self.thread,
            operation,
            flags,
            uargs,
            Abi::Native,
        )
    }

    /// Install `program` with `flags`, directly.
    fn install(&self, program: &[Insn], flags: u64) -> Result<(), &'static str> {
        let at = self.put(program, None)?;
        self.seccomp(SET_MODE_FILTER, flags, at)
            .map(drop)
            .map_err(|_| "a good filter was refused")
    }

    /// What this thread's chain answers for `call` with no arguments: the
    /// errno it fails with, `Some(0)` if it lets the call go on, `None` if it
    /// ends the thread.
    fn judges(&self, call: usize) -> Option<u32> {
        judge_thread(&self.thread, call)
    }
}

/// What `thread`'s chain answers for the native call `call`.
fn judge_thread(thread: &Thread, call: usize) -> Option<u32> {
    let args = SyscallArgs {
        abi: Abi::Native,
        number: call,
        args: [0; 6],
        ip: 0x1000,
    };
    match seccomp::judge(thread, &args) {
        Some(Verdict::Continue) => Some(0),
        Some(Verdict::Errno(errno)) => Some(errno),
        Some(Verdict::Trap) => Some(TRAPPED),
        None => None,
    }
}

/// Run every kind.
///
/// # Errors
///
/// Which property failed.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    probes(&mut report)?;
    privilege()?;
    ordering()?;
    heredity(&mut report)?;
    entry_task(&mut report)?;
    rollback_through_the_core()?;
    speculation(&mut report)?;
    report.chain = longest_chain()?;
    Ok(report)
}

/// What Chromium's probes, bubblewrap's call and libseccomp's start-up ask,
/// answered as Linux answers them (`docs/SECCOMP.md` §1.1, §3.5).
fn probes(report: &mut Report) -> Result<(), &'static str> {
    let env = Env::new()?;
    // A NULL program is `EFAULT` for every flag the kernel builds: Chromium's
    // probe for "seccomp-bpf exists", and for the flags it asks after.
    for flags in [0, FLAG_LOG, SPEC_ALLOW] {
        if env.seccomp(SET_MODE_FILTER, flags, 0) != Err(Errno::EFAULT) {
            return Err("an unknown seccomp flag was answered EFAULT");
        }
        report.probes += 1;
    }
    // A flag that is not built, or that Linux does not know, is refused before
    // the program is read: `EINVAL`, and never `EFAULT`.
    for flags in [
        TSYNC,
        NEW_LISTENER,
        TSYNC_ESRCH,
        WAIT_KILLABLE_RECV,
        UNKNOWN_FLAG,
        1 << 31,
    ] {
        if env.seccomp(SET_MODE_FILTER, flags, 0) != Err(Errno::EINVAL) {
            return Err("an unknown seccomp flag was answered EFAULT");
        }
        report.probes += 1;
    }
    // An unknown operation, and strict mode's arguments.
    if env.seccomp(99, 0, 0) != Err(Errno::EINVAL)
        || env.seccomp(SET_MODE_STRICT, 1, 0) != Err(Errno::EINVAL)
        || env.seccomp(SET_MODE_STRICT, 0, 8) != Err(Errno::EINVAL)
    {
        return Err("seccomp accepted an operation or arguments Linux refuses");
    }
    // `prctl(PR_SET_SECCOMP, 2, NULL)` is `EFAULT`, a mode that is neither is
    // `EINVAL`, and a program of no instructions or of too many is `EINVAL`.
    if seccomp::prctl_set(&env.process, &env.thread, 2, 0, Abi::Native) != Err(Errno::EFAULT)
        || seccomp::prctl_set(&env.process, &env.thread, 3, 0, Abi::Native) != Err(Errno::EINVAL)
    {
        return Err("prctl(PR_SET_SECCOMP) did not answer as Linux does");
    }
    let empty = env.put(&[], Some(0))?;
    let too_long = env.put(&[], Some(MAX_INSNS as u16 + 1))?;
    if env.seccomp(SET_MODE_FILTER, 0, empty) != Err(Errno::EINVAL)
        || env.seccomp(SET_MODE_FILTER, 0, too_long) != Err(Errno::EINVAL)
    {
        return Err("a program of no instructions, or too many, was not refused");
    }
    // A program the verifier refuses: no `RET` at the end.
    let no_return = env.put(&[Insn::new(0x20, 0, 0, 0)], None)?;
    if env.seccomp(SET_MODE_FILTER, 0, no_return) != Err(Errno::EINVAL) {
        return Err("a filter that does not end in a return was accepted");
    }
    // Which actions a filter may return.
    for (action, available) in [
        (KILL_PROCESS, true),
        (KILL_THREAD, true),
        (ERRNO, true),
        (TRACE, true),
        (LOG, true),
        (ALLOW, true),
        (TRAP, false),
        (USER_NOTIF, false),
        (0x1234_0000, false),
    ] {
        uaccess::copy_to_user(env.process.space(), env.page, &action.to_le_bytes())
            .map_err(|_| "a seccomp check could not write an action")?;
        let answer = env.seccomp(GET_ACTION_AVAIL, 0, env.page);
        let wanted = if available {
            Ok(0)
        } else {
            Err(Errno::EOPNOTSUPP)
        };
        if answer != wanted || env.seccomp(GET_ACTION_AVAIL, 1, env.page) != Err(Errno::EINVAL) {
            return Err("GET_ACTION_AVAIL did not answer as Linux does");
        }
        report.probes += 1;
    }
    // The sizes of the structures a supervisor would use: 80, 24 and 64.
    if env.seccomp(GET_NOTIF_SIZES, 0, env.page) != Ok(0) {
        return Err("GET_NOTIF_SIZES was refused");
    }
    let mut sizes = [0_u8; 6];
    uaccess::copy_from_user(env.process.space(), env.page, &mut sizes)
        .map_err(|_| "a seccomp check could not read the notification sizes")?;
    if sizes != [80, 0, 24, 0, 64, 0] {
        return Err("GET_NOTIF_SIZES wrote other sizes than Linux's");
    }
    process::kill(&env.process, 137);
    Ok(())
}

/// A filter is installed by a process that set no-new-privs or is privileged:
/// otherwise a program could confuse a set-user-id one it runs next
/// (`docs/SECCOMP.md` §3.5, SR4).
fn privilege() -> Result<(), &'static str> {
    let unchanged = u64::from(u32::MAX);
    let env = Env::new()?;
    // Root may, with no-new-privs unset.
    env.install(&answering(&[]), 0)
        .map_err(|_| "root could not install a filter")?;
    process::kill(&env.process, 137);

    // The same process as uid 1000 may not, until it sets no-new-privs.
    let env = Env::new()?;
    let dropped = credentials::dispatch(
        Syscall::Setresuid,
        &[1000, 1000, 1000, unchanged, unchanged, 0],
        &env.process,
    );
    if dropped != Some(Ok(0)) {
        return Err("root could not become uid 1000 for the seccomp check");
    }
    let at = env.put(&answering(&[]), None)?;
    if env.seccomp(SET_MODE_FILTER, 0, at) != Err(Errno::EACCES) {
        return Err("an unprivileged filter was installed without no-new-privs");
    }
    if attributes::sys_prctl(&env.process, NO_NEW_PRIVS, [1, 0, 0, 0]) != Ok(0) {
        return Err("PR_SET_NO_NEW_PRIVS was refused");
    }
    if env.seccomp(SET_MODE_FILTER, 0, at) != Ok(0) {
        return Err("a filter was refused to an unprivileged process with no-new-privs");
    }
    process::kill(&env.process, 137);
    Ok(())
}

/// Which of a chain's answers wins: the strictest, and on a tie the newest
/// filter's data (SR5).
fn ordering() -> Result<(), &'static str> {
    let getppid = number(Syscall::Getppid)?;
    let getpid = number(Syscall::Getpid)?;
    let getuid = number(Syscall::Getuid)?;
    let env = Env::new()?;
    // An older ERRNO with a newer ALLOW over it: ERRNO.
    env.install(&answering(&[(getppid, ERRNO | EPERM)]), 0)?;
    env.install(&answering(&[(getppid, ALLOW)]), 0)?;
    if env.judges(getppid) != Some(EPERM) {
        return Err("a newer ALLOW overrode an older ERRNO");
    }
    // Two ERRNOs: the newest's data.
    env.install(&answering(&[(getppid, ERRNO | ENOSYS)]), 0)?;
    if env.judges(getppid) != Some(ENOSYS) {
        return Err("the newest filter's data did not stand on a tie");
    }
    // An older ALLOW under a newer ERRNO: the ERRNO, for a call only it names.
    env.install(&answering(&[(getuid, ERRNO | 13)]), 0)?;
    if env.judges(getuid) != Some(13) || env.judges(getpid) != Some(0) {
        return Err(
            "a newer ERRNO did not hold over an older ALLOW, or judged a call it does not name",
        );
    }
    // TRACE and USER_NOTIF have no tracer or listener: `ENOSYS`, and never a
    // call that runs.
    let traced = Env::new()?;
    traced.install(&answering(&[(getppid, TRACE)]), 0)?;
    traced.install(&answering(&[(getuid, USER_NOTIF)]), 0)?;
    if traced.judges(getppid) != Some(ENOSYS) || traced.judges(getuid) != Some(ENOSYS) {
        return Err("a TRACE or USER_NOTIF result let the call run");
    }
    // LOG lets the call go on.
    let logged = Env::new()?;
    logged.install(&answering(&[(getppid, LOG)]), 0)?;
    if logged.judges(getppid) != Some(0) {
        return Err("a LOG result stopped the call");
    }
    // A thread in filter mode with no filter -- which nothing can make -- is
    // ended, never let go on: Linux answers it with a kill.
    let hollow = Env::new()?;
    hollow
        .thread
        .with_seccomp(|state| *state = State::filter_mode_with_no_filter());
    let hollow_answer = hollow.judges(getppid);
    let hollow_ended = hollow.process.is_terminated();
    process::kill(&hollow.process, 137);
    if hollow_answer == Some(0) || !hollow_ended {
        return Err("a thread in filter mode with no filter was let go on");
    }
    // A filter that is not ours to read, an action nobody defined, is the
    // strictest there is: it ends the process.
    let odd = Env::new()?;
    odd.install(&answering(&[(getppid, 0x1234_0000)]), 0)?;
    let odd_answer = odd.judges(getppid);
    process::kill(&env.process, 137);
    process::kill(&traced.process, 137);
    process::kill(&logged.process, 137);
    process::kill(&odd.process, 137);
    if odd_answer != Some(ENOSYS) {
        return Err("an action nobody defined did not end the process");
    }
    Ok(())
}

/// Who holds a chain: a fork child, a thread, and a native child, each
/// refused the call its creator's filter refuses, with the figures in
/// `/proc/<pid>/status` (SR4, SR6).
fn heredity(report: &mut Report) -> Result<(), &'static str> {
    let getppid = number(Syscall::Getppid)?;
    let env = Env::new()?;
    if attributes::sys_prctl(&env.process, NO_NEW_PRIVS, [1, 0, 0, 0]) != Ok(0) {
        return Err("PR_SET_NO_NEW_PRIVS was refused");
    }
    env.install(&answering(&[(getppid, ERRNO | EPERM)]), 0)?;
    env.install(&answering(&[]), 0)?;

    // A fork child: a copy of the process, and a thread made of the caller's.
    let child = process::fork_for_check(&env.process)
        .map_err(|_| "could not fork for the seccomp inheritance check")?;
    let child_thread = Arc::new(
        Thread::forked(&child, &env.thread).map_err(|_| "no thread for a seccomp fork child")?,
    );
    child.add_thread(&child_thread);
    if judge_thread(&child_thread, getppid) != Some(EPERM) {
        return Err("a forked child of a filtered process was not filtered");
    }
    if attributes::sys_prctl(&child, 39, [0; 4]) != Ok(1) {
        return Err("a forked child of a no-new-privs process could gain privileges");
    }
    status_shows(&child, 2, 2)?;
    report.inherited += 1;

    // A second thread of the same process: the caller's chain too.
    let sibling = Thread::sibling(
        &env.process,
        env.process.pid().saturating_add(1),
        &env.thread,
    )
    .map_err(|_| "no thread for a seccomp sibling")?;
    if judge_thread(&sibling, getppid) != Some(EPERM) {
        return Err("a thread of a filtered process was not filtered");
    }
    report.inherited += 1;

    status_shows(&env.process, 2, 2)?;

    // A process nothing filtered reads none.
    let clean = Env::new()?;
    status_shows(&clean.process, 0, 0)?;
    process::kill(&clean.process, 137);
    process::kill(&child, 137);
    process::kill(&env.process, 137);
    Ok(())
}

/// `/proc/<pid>/status` of `process` reads `Seccomp: mode` and
/// `Seccomp_filters: filters`.
fn status_shows(process: &Process, mode: u32, filters: u32) -> Result<(), &'static str> {
    let text =
        crate::fs::procfs::status_text(process).ok_or("no status for a seccomp check process")?;
    let text = core::str::from_utf8(&text).map_err(|_| "a status file that is not text")?;
    let field = |name: &str| -> Option<u32> {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim().parse().ok())
    };
    if field("Seccomp:") != Some(mode) || field("Seccomp_filters:") != Some(filters) {
        return Err("status did not show the seccomp mode and the filters a thread holds");
    }
    if field("NoNewPrivs:").is_none() {
        return Err("status has no NoNewPrivs line");
    }
    Ok(())
}

/// What the task of the check found, handed back to the boot task: a static
/// because a task's entry takes a `usize` and nothing else, and one check
/// runs, at boot.
static FOUND: SpinLock<Option<Found>> = SpinLock::new(None);

/// What the scenario task saw: calls made, and processes it made as a creator.
type Found = Result<Seen, &'static str>;

/// What the task counted.
#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    /// Calls the task made through the entry.
    calls: usize,
    /// Native children judged.
    inherited: usize,
    /// Traps whose signal and information were read back.
    trapped: usize,
}

/// How the task ends its process when it was not killed: it must have been.
const SURVIVED: i32 = 66;
/// How it ends when it could not finish the check.
const BROKEN: i32 = 67;
/// A process ended by `SIGSYS`, as a shell spells it.
const SIGSYS_STATUS: i32 = 128 + 31;
/// A process ended by `SIGKILL`.
const SIGKILL_STATUS: i32 = 128 + 9;

/// What the task does last, which must end its process.
static ENDING: SpinLock<Option<Ending>> = SpinLock::new(None);

/// How a scenario ends: the call that must kill it.
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// A `KILL_THREAD` filter on this call: `SIGSYS`, the process's only
    /// thread being the last.
    Thread,
    /// A `KILL_PROCESS` filter on this call: `SIGSYS`.
    Process,
    /// Strict mode, then a call it does not allow: `SIGKILL`.
    Strict,
    /// `TRAP`: the signal and its information, with `SIGSYS` blocked.
    Trap,
    /// `TRAP` with `SIGSYS` blocked and ignored: the process dies of it.
    TrapIgnored,
}

/// Run a scenario task in a process of its own, and answer what it found and
/// how its process ended.
fn scenario(ending: Ending, native: bool) -> Result<(Seen, i32), &'static str> {
    let process =
        process::new_for_check().map_err(|_| "no process for the seccomp scenario task")?;
    let page = map_rw(&process, PAGE_SIZE)?;
    PAGE.store(page, core::sync::atomic::Ordering::Release);
    NATIVE.store(native, core::sync::atomic::Ordering::Release);
    *FOUND.lock() = None;
    *ENDING.lock() = Some(ending);
    let thread =
        Arc::new(Thread::leader(&process).map_err(|_| "no memory for the scenario task's thread")?);
    let task = sched::spawn_user("seccomp-check", scenario_task, thread, None, None)
        .map_err(|_| "no task for the seccomp scenario")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let status = process
        .wait_for_exit(deadline)
        .ok_or("the seccomp scenario task never ended its process")?;
    let found = FOUND
        .lock()
        .take()
        .ok_or("the seccomp scenario task ended without a result")?;
    // Leave no stack behind for a later check's frame count to find.
    drop(task);
    drop(process);
    sched::sleep_for(SETTLE_NANOS);
    let _ = sched::reap();
    Ok((found?, status))
}

/// How long the boot task waits for a scenario.
const PATIENCE_NANOS: u64 = 120_000_000_000;
/// How long it lets a reaped task settle.
const SETTLE_NANOS: u64 = 20_000_000;

/// The page the scenario task writes its programs in.
static PAGE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Whether the scenario task makes a native child.
static NATIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The scenario task: report what it found, then do what must end it.
fn scenario_task(_argument: usize) {
    let ending = ENDING.lock().take();
    let found = in_the_process(ending);
    let broken = found.is_err();
    *FOUND.lock() = Some(found);
    process::exit_current(if broken { BROKEN } else { SURVIVED });
}

/// What the scenario task does: install filters as a program does, make the
/// calls they judge through the core's own entry, and last make the call that
/// ends it.
fn in_the_process(ending: Option<Ending>) -> Found {
    let thread = crate::syscall::thread::current().ok_or("the scenario task has no thread")?;
    let process = Arc::clone(thread.process());
    let page = PAGE.load(core::sync::atomic::Ordering::Acquire);
    let env = Env {
        process: Arc::clone(&process),
        thread: Arc::clone(&thread),
        page,
    };
    let getppid = number(Syscall::Getppid)?;
    let getpid = number(Syscall::Getpid)?;
    let clone3 = number(Syscall::Clone3)?;
    let getuid = number(Syscall::Getuid)?;
    let getgid = number(Syscall::Getgid)?;
    let write = number(Syscall::Write)?;
    let seccomp_call = number(Syscall::Seccomp)?;
    let mut seen = Seen::default();
    let call = |nr: usize, args: [u64; 6]| -> Result<isize, &'static str> {
        arch::drive_system_call(Abi::Native, nr, args, 0x1000)
            .ok_or("this architecture has no entry to drive")
    };

    match ending {
        Some(Ending::Strict) => {
            // Strict mode: `write` runs (to a descriptor that is not open, so
            // it answers EBADF and does not block), and any other call ends the
            // thread with SIGKILL.
            let _ = attributes::sys_prctl(&process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            if call(seccomp_call, [SET_MODE_STRICT, 0, 0, 0, 0, 0])? != 0 {
                return Err("strict mode was refused");
            }
            seen.calls += 1;
            if call(write, [99, page, 0, 0, 0, 0])? != -9 {
                return Err("strict mode did not let write run");
            }
            seen.calls += 1;
            *FOUND.lock() = Some(Ok(seen));
            let _ = call(getpid, [0; 6]);
            return Err("strict mode let getpid run");
        }
        Some(Ending::Trap) => {
            seen.trapped = trapped_in_the_task(&env)?;
            return Ok(seen);
        }
        Some(Ending::TrapIgnored) => {
            let _ = attributes::sys_prctl(&process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            let getsid = number(Syscall::Getsid)?;
            // Blocked and ignored: a trap must still end the program.
            thread.with_signals(|shared, _| shared.install_action(SIGSYS_NUMBER, 1, 0));
            crate::syscall::signal::change_blocked(&thread, |_, own| {
                let _ = own.replace_blocked(crate::syscall::signal::bit(SIGSYS_NUMBER));
            });
            let at = env.put(&answering(&[(getsid, TRAP | 1)]), None)?;
            if call(seccomp_call, [SET_MODE_FILTER, 0, at, 0, 0, 0])? != 0 {
                return Err("a trapping filter was refused through the entry");
            }
            *FOUND.lock() = Some(Ok(seen));
            let _ = call(getsid, [0; 6]);
            return Err("a blocked and ignored SIGSYS let a trapped call return");
        }
        Some(Ending::Thread | Ending::Process) | None => {}
    }

    // No-new-privs first, then a filter that fails getppid and clone3 and a
    // second one that lets everything go, both through `seccomp(2)`'s own entry.
    if attributes::sys_prctl(&process, NO_NEW_PRIVS, [1, 0, 0, 0]) != Ok(0) {
        return Err("PR_SET_NO_NEW_PRIVS was refused");
    }
    let before = call(getppid, [0; 6])?;
    if before <= 0 {
        return Err("getppid failed before any filter was installed");
    }
    let at = env.put(
        &answering(&[(getppid, ERRNO | EPERM), (clone3, ERRNO | ENOSYS)]),
        None,
    )?;
    if call(seccomp_call, [SET_MODE_FILTER, 0, at, 0, 0, 0])? != 0 {
        return Err("a filter was refused through the entry");
    }
    seen.calls += 1;
    if call(getppid, [0; 6])? != -(EPERM as isize) {
        return Err("a filtered getppid ran");
    }
    if call(clone3, [0; 6])? != -(ENOSYS as isize) {
        return Err("a filtered clone3 was not failed with ENOSYS");
    }
    if call(getpid, [0; 6])? != process.pid() as isize {
        return Err("a call the filter does not name did not run");
    }
    seen.calls += 3;
    // An ERRNO of 512, which the dispatcher keeps for restarting calls, comes
    // back as it is, once; one past 4095 is cut to 4095 (SR7).
    let at = env.put(
        &answering(&[(getuid, ERRNO | 512), (getgid, ERRNO | 5000)]),
        None,
    )?;
    if call(seccomp_call, [SET_MODE_FILTER, 0, at, 0, 0, 0])? != 0 {
        return Err("a second filter was refused through the entry");
    }
    if call(getuid, [0; 6])? != -512 {
        return Err("a filter's ERESTARTSYS restarted the call");
    }
    if call(getgid, [0; 6])? != -4095 {
        return Err("a filter's errno was not cut to 4095");
    }
    seen.calls += 3;

    // A native child of a filtered creator keeps the filter.
    if NATIVE.load(core::sync::atomic::Ordering::Acquire) {
        seen.inherited += native_child(&process, getppid)?;
    }

    // Then what ends it.
    let victim = match ending {
        Some(Ending::Thread) => (number(Syscall::Getsid)?, KILL_THREAD),
        Some(Ending::Process) => (number(Syscall::Getsid)?, KILL_PROCESS),
        _ => return Ok(seen),
    };
    let at = env.put(&answering(&[victim]), None)?;
    if call(seccomp_call, [SET_MODE_FILTER, 0, at, 0, 0, 0])? != 0 {
        return Err("a filter that kills was refused through the entry");
    }
    *FOUND.lock() = Some(Ok(seen));
    let _ = call(victim.0, [0; 6]);
    // Not reached when the filter ended this thread.
    Err("a call a filter kills for returned")
}

/// A native child of the running thread's process judges `getppid` as the
/// creator's chain does, and is never started.
fn native_child(creator: &Arc<Process>, getppid: usize) -> Result<usize, &'static str> {
    use crate::object::process::{Host, downcast};
    let file = crate::syscall::check::native_child_image();
    let child = launch::load_native(Some(&**creator as &dyn Host), &file, b"/seccomp-native")
        .ok()
        .and_then(downcast::<Process>)
        .ok_or("a native child could not be made for the seccomp check")?;
    let thread =
        Arc::new(Thread::leader(&child).map_err(|_| "no thread for a native seccomp child")?);
    child.add_thread(&thread);
    let answer = judge_thread(&thread, getppid);
    let shown = status_shows(&child, 2, 2);
    process::kill(&child, 137);
    if answer != Some(EPERM) {
        return Err("a native child of a filtered process was not filtered");
    }
    shown?;
    Ok(1)
}

/// The scenarios that end a process: a thread killed by its filter, a process
/// killed, a thread in strict mode, and a native child's inheritance.
fn entry_task(report: &mut Report) -> Result<(), &'static str> {
    // The first kills the process's only thread: with no other, the process
    // ends by `SIGSYS` (SR: a killed thread's process exits by the signal).
    let (seen, status) = scenario(Ending::Thread, true)?;
    if status != SIGSYS_STATUS {
        return Err("a killed thread's process exited otherwise than by SIGSYS");
    }
    report.calls += seen.calls;
    report.inherited += seen.inherited;
    report.killed += 1;

    let (seen, status) = scenario(Ending::Process, false)?;
    if status != SIGSYS_STATUS {
        return Err("a process killed by its filter exited otherwise than by SIGSYS");
    }
    report.calls += seen.calls;
    report.killed += 1;

    let (seen, status) = scenario(Ending::Strict, false)?;
    // The strict scenario reports through the error it ends with when it
    // survived; a kill by `SIGKILL` is its pass.
    let _ = seen;
    if status != SIGKILL_STATUS {
        return Err("strict mode did not end a thread that made a call it does not allow");
    }
    report.calls += 2;
    report.killed += 1;

    // `TRAP`: the signal, forced past a blocked mask, with the call's
    // information; and with `SIGSYS` ignored as well, the process dies of it.
    let (seen, status) = scenario(Ending::Trap, false)?;
    if status != SURVIVED {
        return Err("a trap's check ended its process otherwise than as the check ended it");
    }
    report.traps += seen.trapped;
    let (_, status) = scenario(Ending::TrapIgnored, false)?;
    if status != SIGSYS_STATUS {
        return Err(
            "a blocked and ignored SIGSYS did not end the process that made a trapped call",
        );
    }
    report.traps += 1;
    report.killed += 1;
    Ok(())
}

/// The longest chain Linux allows is made and released: 6,554 one-instruction
/// filters, each pointing at the one before, dropped by one reference. A
/// recursive release would run the kernel stack out (SR11).
fn longest_chain() -> Result<usize, &'static str> {
    let env = Env::new()?;
    let one = answering(&[]);
    let mut made = 0;
    loop {
        let filter = seccomp::prepare(&one, false).map_err(|_| "a filter could not be made")?;
        match seccomp::attach(&env.thread, filter) {
            Ok(()) => made += 1,
            Err(Errno::ENOMEM) => break,
            Err(_) => {
                return Err("a filter was refused for another reason than the chain's length");
            }
        }
        if made > MAX_INSNS_PER_PATH {
            return Err("a chain grew past what Linux allows");
        }
    }
    let expected = {
        let (mut cost, mut count) = (0_usize, 0_usize);
        while ferrix_seccomp::fits_path(cost, one.len()) {
            cost = ferrix_seccomp::path_cost(cost, one.len());
            count += 1;
        }
        count
    };
    if made != expected {
        return Err("a chain was refused at another length than Linux's");
    }
    // Released in one drop.
    env.thread.with_seccomp(|state| *state = State::default());
    process::kill(&env.process, 137);
    Ok(made)
}

/// What `judge_thread` answers for a call a filter trapped.
const TRAPPED: u32 = u32::MAX;

/// A handler address the check gives `SIGSYS`: any address that is not
/// `SIG_DFL` or `SIG_IGN`, since nothing runs it.
const HANDLER: u64 = 0x0040_0000;

/// `SIGSYS`.
const SIGSYS_NUMBER: u32 = 31;

/// `TRAP` in a thread that has a handler: the call does not run, `SIGSYS` is
/// pending for this thread even though the program blocked it (a trap cannot
/// be blocked), and the `siginfo` a handler would read says what Linux's
/// `force_sig_seccomp` says (`docs/SECCOMP.md` §3.6, SR9). Runs in the task of
/// a check process, because the signal is forced on the running thread.
fn trapped_in_the_task(env: &Env) -> Result<usize, &'static str> {
    use crate::syscall::signal;
    let getppid = number(Syscall::Getppid)?;
    let _ = attributes::sys_prctl(&env.process, NO_NEW_PRIVS, [1, 0, 0, 0]);
    // A handler, and the signal blocked: a trap must come through anyway.
    env.thread
        .with_signals(|shared, _| shared.install_action(SIGSYS_NUMBER, HANDLER, 0));
    signal::change_blocked(&env.thread, |_, own| {
        let _ = own.replace_blocked(signal::bit(SIGSYS_NUMBER));
    });
    env.install(&answering(&[(getppid, TRAP | 0x1234)]), 0)?;
    if env.judges(getppid) != Some(TRAPPED) {
        return Err("a TRAP result did not trap the call");
    }
    let taken = env
        .thread
        .with_signals(|shared, own| signal::take_next(shared, own))
        .ok_or("a blocked SIGSYS let a trapped call return")?;
    let signal::Origin::Sys {
        errno,
        call_addr,
        syscall,
        arch: token,
    } = taken.origin
    else {
        return Err("a trapped call raised a signal that is not a seccomp trap");
    };
    if taken.signal != SIGSYS_NUMBER
        || errno != 0x1234
        || call_addr != 0x1000
        || syscall != getppid as i32
        || token != arch_token()
    {
        return Err("a trapped call's siginfo did not carry the call, its ip and its arch");
    }
    // The bytes a handler of this machine reads.
    let info = taken.origin.encode(SIGSYS_NUMBER);
    let word = |at: usize| {
        info.get(at..at + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(i32::from_le_bytes)
    };
    let union = if size_of::<usize>() == 8 { 16 } else { 12 };
    let at = union + size_of::<usize>();
    if word(0) != Some(SIGSYS_NUMBER as i32)
        || word(4) != Some(0x1234)
        || word(8) != Some(1)
        || word(at) != Some(getppid as i32)
        || word(at + 4) != Some(arch_token() as i32)
    {
        return Err("a trapped call's siginfo had its fields at other offsets than Linux's");
    }
    // The i386 program on a 64-bit kernel reads the union four bytes lower
    // and a pointer narrower: `_syscall` at 16, `_arch` at 20.
    if size_of::<usize>() == 8 {
        let small = ferrix_linux_abi::sigframe32::siginfo_from_64(&info);
        let word32 = |at: usize| {
            small
                .get(at..at + 4)
                .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
                .map(i32::from_le_bytes)
        };
        if word32(8) != Some(1)
            || word32(12) != Some(0x1000)
            || word32(16) != Some(getppid as i32)
            || word32(20) != Some(arch_token() as i32)
        {
            return Err("i386 si_syscall was not at offset 16");
        }
    }
    Ok(1)
}

/// The `arch` token of this machine's own entry.
fn arch_token() -> u32 {
    arch::audit_arch(Abi::Native)
}

/// The core gives a trapped call's registers back as the program made it: the
/// value it answers is the architecture's rollback of the call, and nothing a
/// filter chose (`docs/SECCOMP.md` §3.6).
fn rollback_through_the_core() -> Result<(), &'static str> {
    use ferrix_sync::Once;
    fn trapping(_: &SyscallArgs) -> Verdict {
        Verdict::Trap
    }
    let slot: Once<crate::trap::SyscallFilter> = Once::new();
    crate::trap::register(&slot, trapping);
    let args = SyscallArgs {
        abi: Abi::Native,
        number: 0x2222,
        args: [0x1111, 2, 3, 4, 5, 6],
        ip: 0x1000,
    };
    let expected = arch::syscall_rollback_value(Abi::Native, args.number, &args.args);
    match crate::trap::ask(&slot, &args) {
        Some(crate::trap::Outcome::Return(value)) if value == expected => {}
        _ => return Err("SIGSYS's context lost the syscall number"),
    }
    // And that value is the number on x86, the first argument on the Arm pair:
    // what a handler finds in the register that held it.
    let wanted = if arch::ARCH.elf_machine() == 62 {
        0x2222
    } else {
        0x1111
    };
    if expected != wanted {
        return Err("SIGSYS's context lost the syscall number");
    }
    Ok(())
}

/// What a program learns of speculation and may do about it: both features
/// mitigated and force-disabled, enabling one `EPERM`, and a filter installed
/// with `SPEC_ALLOW` changes nothing (SR14).
fn speculation(report: &mut Report) -> Result<(), &'static str> {
    let env = Env::new()?;
    let prctl = |args: [u64; 6]| {
        seccomp::dispatch(Syscall::Prctl, &args, &env.process, Abi::Native)
            .unwrap_or(Err(Errno::EINVAL))
    };
    // Force-disabled is `PR_SPEC_PRCTL | PR_SPEC_FORCE_DISABLE`.
    for which in [0, 1] {
        if prctl([52, which, 0, 0, 0, 0]) != Ok(9) {
            return Err("a mitigated speculation feature was not reported force-disabled");
        }
    }
    if prctl([52, 2, 0, 0, 0, 0]) != Err(Errno::ENODEV) {
        return Err("a feature with no control was not answered ENODEV");
    }
    // Asking for more mitigation is already true; for less is refused.
    if prctl([53, 1, 4, 0, 0, 0]) != Ok(0) || prctl([53, 1, 8, 0, 0, 0]) != Ok(0) {
        return Err("a request for more speculation mitigation was refused");
    }
    if prctl([53, 1, 2, 0, 0, 0]) != Err(Errno::EPERM)
        || prctl([53, 0, 2, 0, 0, 0]) != Err(Errno::EPERM)
    {
        return Err("PR_SET_SPECULATION_CTRL enabled a mitigation");
    }
    if prctl([53, 1, 99, 0, 0, 0]) != Err(Errno::ERANGE)
        || prctl([53, 1, 4, 1, 0, 0]) != Err(Errno::EINVAL)
    {
        return Err("PR_SET_SPECULATION_CTRL did not refuse a value that is no control");
    }
    // `SPEC_ALLOW` is accepted and turns nothing off: the feature reads the same
    // after a filter that asked for it.
    let _ = attributes::sys_prctl(&env.process, NO_NEW_PRIVS, [1, 0, 0, 0]);
    env.install(&answering(&[]), SPEC_ALLOW)?;
    if prctl([52, 1, 0, 0, 0, 0]) != Ok(9) {
        return Err("SPEC_ALLOW turned a speculation mitigation off");
    }
    report.probes += 6;
    process::kill(&env.process, 137);
    Ok(())
}
