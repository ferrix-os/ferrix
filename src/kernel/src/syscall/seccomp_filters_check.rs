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
    /// What a call costs that chain, in nanoseconds: 6,554 runs of one step.
    pub(crate) walk_many: u64,
    /// What a call costs a chain of seven 4,096-instruction filters, the most
    /// steps there can be, in nanoseconds.
    pub(crate) walk_long: u64,
    /// What releasing the longest chain costs, in nanoseconds.
    pub(crate) release: u64,
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
        None => None,
    }
}

/// Run every kind.
///
/// # Errors
///
/// Which property failed.
pub(crate) fn run() -> Result<Report, &'static str> {
    let before = seccomp::filtered_threads();
    let mut report = Report::default();
    probes(&mut report)?;
    privilege()?;
    ordering()?;
    heredity(&mut report)?;
    entry_task(&mut report)?;
    let (chain, many, released) = longest_chain()?;
    report.chain = chain;
    report.walk_many = many;
    report.release = released;
    report.walk_long = longest_steps()?;
    // Every thread this check filtered, a made, an inherited and an ended
    // one, has gone: the count the entry's first look reads is back where it
    // was, so a kernel whose filtered threads have all ended looks for no
    // thread at a call again.
    if seccomp::filtered_threads() != before {
        return Err("the count of filtered threads did not come back once they had gone");
    }
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
    /// A thread other than the first is killed by its filter: it alone ends.
    Member,
    /// The first thread is killed by its filter while another lives: the
    /// process ends, when the other does, by `SIGSYS`.
    Leader,
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
    // Its thread and task listed, as a started process lists its first: an
    // end the core records posts to the tasks it lists (`sched::work`).
    let task = crate::syscall::check::spawn_in(&process, "seccomp-check", scenario_task, None)?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let waited = process.wait_for_exit(deadline);
    let status = waited.ok_or("the seccomp scenario task never ended its process")?;
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
const PATIENCE_NANOS: u64 = 60_000_000_000;
/// How long it lets a reaped task settle.
const SETTLE_NANOS: u64 = 20_000_000;

/// The page the scenario task writes its programs in.
static PAGE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Whether the scenario task makes a native child.
static NATIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// What [`in_the_process`] leaves its task to do once it has returned, and so
/// let go of every reference it held.
enum Then {
    /// Report this.
    Report(Found),
    /// Report `found`, then make `call`, which a filter ends the thread for;
    /// report `returned` instead if it comes back.
    Ending {
        /// What the scenario saw before the call.
        found: Found,
        /// The call the filter ends the thread for.
        call: usize,
        /// The check's complaint when it comes back.
        returned: &'static str,
    },
}

/// The scenario task: report what it found, then do what must end it.
///
/// The call that ends it is made here, in a frame that holds no reference: a
/// thread a filter ends never returns to the frames below the call, so
/// whatever they held -- its thread above all, which counts as filtered until
/// it is dropped -- would never be let go.
fn scenario_task(_argument: usize) {
    let ending = ENDING.lock().take();
    let found = match in_the_process(ending) {
        Ok(Then::Report(found)) => found,
        Ok(Then::Ending {
            found,
            call,
            returned,
        }) => {
            *FOUND.lock() = Some(found);
            let _ = arch::drive_system_call(Abi::Native, call, [0; 6], 0x1000);
            // As `in_the_process`'s calls do: the x86-64 entry ends a thread
            // whose process was ended on its way out, which the Arm pair's
            // vector does after `system_call` returns.
            if crate::syscall::thread::current().is_some_and(|me| me.process().is_terminated()) {
                process::leave_current();
            }
            Err(returned)
        }
        Err(problem) => Err(problem),
    };
    let broken = found.is_err();
    *FOUND.lock() = Some(found);
    process::exit_current(if broken { BROKEN } else { SURVIVED });
}

/// What the scenario task does: install filters as a program does, make the
/// calls they judge through the core's own entry, and last name the call that
/// ends it, for [`scenario_task`] to make.
fn in_the_process(ending: Option<Ending>) -> Result<Then, &'static str> {
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
    let seccomp_call = number(Syscall::Seccomp)?;
    let mut seen = Seen::default();
    let call = |nr: usize, args: [u64; 6]| -> Result<isize, &'static str> {
        let answer = arch::drive_system_call(Abi::Native, nr, args, 0x1000)
            .ok_or("this architecture has no entry to drive")?;
        // The x86-64 entry ends a thread whose process was ended on its way out.
        // On the Arm pair that step is the vector's, after `system_call`
        // returns, which a driven call skips: do what it does.
        if process.is_terminated() {
            process::leave_current();
        }
        Ok(answer)
    };

    if let Some(then) = ended_early(ending, &env, &call)? {
        return Ok(then);
    }

    // No-new-privs first, then a filter that fails getppid and clone3 and a
    // second one that lets everything go, both through `seccomp(2)`'s own entry.
    if attributes::sys_prctl(&process, NO_NEW_PRIVS, [1, 0, 0, 0]) != Ok(0) {
        return Err("PR_SET_NO_NEW_PRIVS was refused");
    }
    // A process of the check's own has no parent that a program would name, so
    // getppid answers whatever it answers (zero or init's pid), never an error.
    if call(getppid, [0; 6])? < 0 {
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
        _ => return Ok(Then::Report(Ok(seen))),
    };
    let at = env.put(&answering(&[victim]), None)?;
    if call(seccomp_call, [SET_MODE_FILTER, 0, at, 0, 0, 0])? != 0 {
        return Err("a filter that kills was refused through the entry");
    }
    Ok(Then::Ending {
        found: Ok(seen),
        call: victim.0,
        returned: "a call a filter kills for returned",
    })
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
    // A start that is refused (a bad argument handle, no memory for the thread)
    // and made again makes a new first thread, which starts under the creator's
    // chain too: the first one is made and dropped, as a refused start's is.
    drop(Thread::leader(&child).map_err(|_| "no thread for a native seccomp child")?);
    let thread =
        Arc::new(Thread::leader(&child).map_err(|_| "no thread for a native seccomp child")?);
    child.add_thread(&thread);
    let answer = judge_thread(&thread, getppid);
    let shown = status_shows(&child, 2, 2);
    // It starts with its creator's no-new-privs too: a filter under no-new-privs
    // is only safe if every child keeps it.
    let privileges = attributes::sys_prctl(&child, 39, [0; 4]);
    process::kill(&child, 137);
    if answer != Some(EPERM) {
        return Err(
            "a native child of a filtered process was not filtered once its start was refused \
             and made again",
        );
    }
    shown?;
    if privileges != Ok(1) {
        return Err("a native child of a no-new-privs process could gain privileges");
    }
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

    // A thread other than the first killed by its filter ends alone.
    let (seen, status) = scenario(Ending::Member, false)?;
    if status != SURVIVED {
        return Err(
            "a thread killed by its filter ended its process otherwise than as the check did",
        );
    }
    report.calls += seen.calls;
    report.killed += 1;

    // The first thread killed while another lives: the process ends by SIGSYS
    // when the other does.
    let (_, status) = scenario(Ending::Leader, false)?;
    if status != SIGSYS_STATUS {
        return Err("a killed leader's process did not end by SIGSYS");
    }
    report.killed += 1;
    Ok(())
}

/// A thread of the scenario's process, started and counted as `clone` starts
/// one, running `entry`.
fn spawn_member(
    process: &Arc<Process>,
    creator: &Thread,
    entry: fn(usize),
) -> Result<Arc<sched::Task>, &'static str> {
    let tid = crate::syscall::registry::allocate_thread(process).ok_or("no thread id")?;
    let thread = Thread::sibling(process, tid, creator).map_err(|_| "no memory for a thread")?;
    let thread = Arc::new(thread);
    process.add_thread(&thread);
    let prepared = sched::prepare_user("seccomp-member", entry, thread, None, None)
        .map_err(|_| "no task for a thread of the check")?;
    // Listed before it runs, as `clone` lists a thread's task, so that an end
    // the core records posts to it (`sched::work`).
    crate::object::process::Host::core(&**process)
        .list_task(prepared.task())
        .map_err(|_| "no memory to list a thread of the check")?;
    Ok(prepared.launch())
}

/// Whether the member that killed itself came back from its call.
static SURVIVED_ITS_KILL: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// A member that gives itself a `KILL_THREAD` filter for `getsid` and makes
/// the call: only it ends.
///
/// The call is made once its references are let go, as [`scenario_task`]
/// makes its last: the frames below a call its filter ends are never
/// returned to.
fn killer(_argument: usize) {
    let armed = crate::syscall::thread::current().and_then(|me| {
        let env = Env {
            process: Arc::clone(me.process()),
            thread: Arc::clone(&me),
            page: PAGE.load(core::sync::atomic::Ordering::Acquire),
        };
        let getsid = number(Syscall::Getsid).ok()?;
        (attributes::sys_prctl(&env.process, NO_NEW_PRIVS, [1, 0, 0, 0]) == Ok(0)
            && env.install(&answering(&[(getsid, KILL_THREAD)]), 0).is_ok())
        .then_some(getsid)
    });
    if let Some(getsid) = armed {
        let _ = arch::drive_system_call(Abi::Native, getsid, [0; 6], 0x1000);
        SURVIVED_ITS_KILL.store(true, core::sync::atomic::Ordering::Release);
    }
    process::leave_current();
}

/// A member that waits for the first thread to be gone and then ends its own
/// thread, and with it, being the last, the process.
fn outlive_the_leader(_argument: usize) {
    if let Some(me) = crate::syscall::thread::current() {
        let process = Arc::clone(me.process());
        let patience = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
        while process.threads().len() > 1 && crate::timer::now_nanos() < patience {
            sched::yield_now();
        }
    }
    process::exit_thread_current(0);
}

/// The longest chain Linux allows is made and released: 6,554 one-instruction
/// filters, each pointing at the one before, dropped by one reference. A
/// recursive release would run the kernel stack out (SR11).
fn longest_chain() -> Result<(usize, u64, u64), &'static str> {
    let env = Env::new()?;
    // The shortest filter there is: one instruction, `ret ALLOW`.
    let one = [Insn::new(0x06, 0, 0, ALLOW)];
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
    // What a call pays for the longest chain, once: 6,554 filters of one
    // instruction, each a call of the interpreter.
    let walked = judge_cost(&env.thread, number(Syscall::Getppid)?);
    // Released in one drop, outside the thread's lock: the state is taken out
    // and the chain goes after the lock does. In production the last drop can
    // fall to the reaper with preemption off, which is what this measures.
    let old = env.thread.with_seccomp(core::mem::take);
    seccomp::measure_releases();
    let start = crate::timer::now_nanos();
    drop(old);
    let released = crate::timer::now_nanos().saturating_sub(start);
    // A walk spreads over a few hundred bytes of this task's stack, a
    // recursive release over a frame for each of the 6,554 filters.
    if seccomp::release_stack_span() > 4096 {
        return Err("a chain's release was recursive, nested as deep as the chain is long");
    }
    process::kill(&env.process, 137);
    Ok((made, walked, released))
}

/// What a call costs a thread whose chain is seven filters of 4,096
/// instructions, the most steps a chain can have, in nanoseconds.
fn longest_steps() -> Result<u64, &'static str> {
    let env = Env::new()?;
    let mut long = vec![Insn::new(0x20, 0, 0, 0); MAX_INSNS - 1];
    long.push(Insn::new(0x06, 0, 0, ALLOW));
    let mut made = 0;
    loop {
        let filter =
            seccomp::prepare(&long, false).map_err(|_| "a long filter could not be made")?;
        match seccomp::attach(&env.thread, filter) {
            Ok(()) => made += 1,
            Err(Errno::ENOMEM) => break,
            Err(_) => return Err("a long filter was refused for another reason"),
        }
    }
    if made != 7 {
        return Err(
            "a chain of 4,096-instruction filters was refused at another length than Linux's",
        );
    }
    let walked = judge_cost(&env.thread, number(Syscall::Getppid)?);
    let old = env.thread.with_seccomp(core::mem::take);
    drop(old);
    process::kill(&env.process, 137);
    Ok(walked)
}

/// The mean of a few judgments of `call` by `thread`'s chain, in nanoseconds.
fn judge_cost(thread: &Thread, call: usize) -> u64 {
    const ROUNDS: u64 = 20;
    let start = crate::timer::now_nanos();
    for _ in 0..ROUNDS {
        let _ = core::hint::black_box(judge_thread(thread, core::hint::black_box(call)));
    }
    crate::timer::now_nanos().saturating_sub(start) / ROUNDS
}

/// The endings that leave before the steps every other run shares: strict
/// mode, a member killed by its filter, and a leader killed while a member
/// lives on. `None` for the rest; an error is the check's.
fn ended_early(
    ending: Option<Ending>,
    env: &Env,
    call: &dyn Fn(usize, [u64; 6]) -> Result<isize, &'static str>,
) -> Result<Option<Then>, &'static str> {
    let getppid = number(Syscall::Getppid)?;
    let getpid = number(Syscall::Getpid)?;
    let write = number(Syscall::Write)?;
    let seccomp_call = number(Syscall::Seccomp)?;
    let process = &env.process;
    let thread = &env.thread;
    let page = env.page;
    let mut seen = Seen::default();
    match ending {
        Some(Ending::Strict) => {
            // Strict mode: `write` runs (to a descriptor that is not open, so
            // it answers EBADF and does not block), and any other call ends the
            // thread with SIGKILL.
            let _ = attributes::sys_prctl(process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            if call(seccomp_call, [SET_MODE_STRICT, 0, 0, 0, 0, 0])? != 0 {
                return Err("strict mode was refused");
            }
            seen.calls += 1;
            if call(write, [99, page, 0, 0, 0, 0])? != -9 {
                return Err("strict mode did not let write run");
            }
            seen.calls += 1;
            Ok(Some(Then::Ending {
                found: Ok(seen),
                call: getpid,
                returned: "strict mode let getpid run",
            }))
        }
        Some(Ending::Member) => {
            let _ = attributes::sys_prctl(process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            let _task = spawn_member(process, thread, killer)?;
            let patience = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
            while process.threads().len() > 1 {
                if crate::timer::now_nanos() > patience {
                    return Err("a thread killed by its filter never left");
                }
                if process.is_terminated() {
                    return Err("a thread killed by its filter took its whole process with it");
                }
                sched::yield_now();
            }
            if process.is_terminated()
                || SURVIVED_ITS_KILL.load(core::sync::atomic::Ordering::Acquire)
            {
                return Err("a thread killed by its filter took its whole process with it");
            }
            // Its calls still run: the filter was its own.
            if call(getppid, [0; 6])? < 0 {
                return Err("a thread killed by its filter left the others without their calls");
            }
            seen.calls += 1;
            Ok(Some(Then::Report(Ok(seen))))
        }
        Some(Ending::Leader) => {
            let _ = attributes::sys_prctl(process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            let getsid = number(Syscall::Getsid)?;
            let _task = spawn_member(process, thread, outlive_the_leader)?;
            let at = env.put(&answering(&[(getsid, KILL_THREAD)]), None)?;
            if call(seccomp_call, [SET_MODE_FILTER, 0, at, 0, 0, 0])? != 0 {
                return Err("a filter that kills was refused through the entry");
            }
            Ok(Some(Then::Ending {
                found: Ok(seen),
                call: getsid,
                returned: "a call a filter kills for returned",
            }))
        }
        Some(Ending::Thread | Ending::Process) | None => Ok(None),
    }
}
