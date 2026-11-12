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
pub struct Report {
    /// Calls a filtered thread made through the core's entry.
    pub calls: usize,
    /// Answers `seccomp(2)` and `prctl` gave to what Chromium's probes ask.
    pub probes: usize,
    /// Children and threads that judged a call as their creator's chain did.
    pub inherited: usize,
    /// Processes a filter ended with `SIGSYS` or `SIGKILL`.
    pub killed: usize,
    /// Filters in the longest chain made and released.
    pub chain: usize,
    /// What a call costs that chain, in nanoseconds: 6,554 runs of one step.
    pub walk_many: u64,
    /// What a call costs a chain of seven 4,096-instruction filters, the most
    /// steps there can be, in nanoseconds.
    pub walk_long: u64,
    /// What releasing the longest chain costs, in nanoseconds.
    pub release: u64,
    /// What `TSYNC`'s test of one other thread costs against the longest
    /// chain, walked whole, in nanoseconds.
    pub ancestor: u64,
    /// What the whole `TSYNC` of the running-threads case took, in
    /// nanoseconds: both holds of the thread-list lock and the test between
    /// them, so an upper bound on each hold.
    pub sync_nanos: u64,
    /// How many threads the process had as that `TSYNC` began.
    pub sync_threads: usize,
    /// Its longest hold of the thread-list lock, in nanoseconds.
    pub sync_hold: u64,
    /// How many holds it made: two a round.
    pub sync_holds: u64,
    /// Traps delivered as a signal with the call's information, read back, and
    /// a blocked and ignored one that ended its process.
    pub(crate) traps: usize,
    /// Threads that held the chain `TSYNC` gave, forty of them made while it
    /// ran.
    pub synced: usize,
    /// Cases of `TSYNC`'s all-or-nothing rule checked.
    pub refusals: usize,
    /// Threads that changed their own seccomp state in one of a `TSYNC`'s two
    /// windows and kept what they did: two that it saw and gave up its round
    /// for, and the racers that had begun their call while it held the second
    /// open (none on one processor).
    pub raced: usize,
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
/// `SECCOMP_FILTER_FLAG_TSYNC`.
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

    /// [`Env::judges`] with the call's arguments.
    fn judges_with(&self, call: usize, args: [u64; 6]) -> Option<u32> {
        judge_with(&self.thread, call, args)
    }
}

/// What `thread`'s chain answers for the native call `call`.
fn judge_thread(thread: &Thread, call: usize) -> Option<u32> {
    judge_with(thread, call, [0; 6])
}

/// [`judge_thread`] with the call's arguments.
fn judge_with(thread: &Thread, call: usize, args: [u64; 6]) -> Option<u32> {
    let args = SyscallArgs {
        abi: Abi::Native,
        number: call,
        args,
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
pub fn run() -> Result<Report, &'static str> {
    let before = seccomp::filtered_threads();
    let mut report = Report::default();
    probes(&mut report)?;
    privilege()?;
    ordering()?;
    heredity(&mut report)?;
    entry_task(&mut report)?;
    speculation(&mut report)?;
    tsync_rules(&mut report)?;
    tsync_tested_window(&mut report)?;
    let (chain, many, released, ancestor) = longest_chain()?;
    report.chain = chain;
    report.walk_many = many;
    report.release = released;
    report.ancestor = ancestor;
    report.walk_long = longest_steps()?;
    // Every thread this check filtered, a made, an inherited and an ended
    // one, has gone: the count the entry's first look reads is back where it
    // was, so a kernel whose filtered threads have all ended looks for no
    // thread at a call again. Each scenario's task was reaped before it
    // returned, which drops its thread; the reap is given a bounded while
    // more on a processor that frees a task's stack late. Nothing else runs
    // filtered during the boot's checks.
    let patience = crate::timer::now_nanos().saturating_add(SETTLE_NANOS * 50);
    while seccomp::filtered_threads() != before && crate::timer::now_nanos() < patience {
        sched::sleep_for(SETTLE_NANOS);
        let _ = sched::reap();
    }
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
    for flags in [
        0,
        FLAG_LOG,
        SPEC_ALLOW,
        TSYNC,
        TSYNC_ESRCH,
        TSYNC | TSYNC_ESRCH,
    ] {
        if env.seccomp(SET_MODE_FILTER, flags, 0) != Err(Errno::EFAULT) {
            return Err("an unknown seccomp flag was answered EFAULT");
        }
        report.probes += 1;
    }
    // A flag that is not built, or that Linux does not know, is refused before
    // the program is read: `EINVAL`, and never `EFAULT`.
    for flags in [NEW_LISTENER, WAIT_KILLABLE_RECV, UNKNOWN_FLAG, 1 << 31] {
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
        (TRAP, true),
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
    let sibling = Arc::new(
        Thread::sibling(
            &env.process,
            env.process.pid().saturating_add(1),
            &env.thread,
        )
        .map_err(|_| "no thread for a seccomp sibling")?,
    );
    // Filtered as it is made, before anything lists it: a path that listed a
    // thread without `add_thread_from`'s copy would still list a filtered
    // one (the consultant's S5 condition 1, fail closed).
    if judge_thread(&sibling, getppid) != Some(EPERM) {
        return Err("a thread of a filtered process was made unfiltered, before it was listed");
    }
    env.process.add_thread_from(&sibling, &env.thread);
    if judge_thread(&sibling, getppid) != Some(EPERM) {
        return Err("a thread of a filtered process was not filtered");
    }
    report.inherited += 1;

    status_shows(&env.process, 2, 2)?;

    // The thread the chain was installed on goes while the two made of it
    // live: they are still filtered, so a call still looks for its thread
    // (`seccomp::any_filtered`, which `check` and the fast path's T2 read
    // first). Counted as each was made, not only as a filter was installed.
    let Env {
        process: filtered,
        thread: installer,
        ..
    } = env;
    drop(installer);
    if !seccomp::any_filtered() {
        return Err(
            "the threads that kept a chain were not looked for at a call once its installer had gone",
        );
    }
    if judge_thread(&child_thread, getppid) != Some(EPERM)
        || judge_thread(&sibling, getppid) != Some(EPERM)
    {
        return Err("a thread that kept its creator's chain lost it once its creator had gone");
    }

    // A process nothing filtered reads none.
    let clean = Env::new()?;
    status_shows(&clean.process, 0, 0)?;
    process::kill(&clean.process, 137);
    process::kill(&child, 137);
    process::kill(&filtered, 137);
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
    /// Threads that held the chain `TSYNC` gave, the ones made while it ran
    /// among them.
    threads: usize,
    /// Threads that raced a `TSYNC`'s second window while it was open.
    raced: usize,
    /// What the running-threads case's `TSYNC` took, in nanoseconds.
    sync_nanos: u64,
    /// How many threads its process had as it began.
    sync_threads: usize,
    /// Its longest hold of the thread-list lock, in nanoseconds.
    sync_hold: u64,
    /// How many holds it made.
    sync_holds: u64,
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
    /// `TRAP`: the signal and its information, with `SIGSYS` blocked.
    Trap,
    /// `TRAP` with `SIGSYS` blocked and ignored: the process dies of it.
    TrapIgnored,
    /// `TRAP` for a call in the native range, which `SIGSYS` cannot express:
    /// the process is killed, though a handler is installed.
    TrapNative,
    /// `TSYNC` among threads that are made while it runs.
    Sync,
    /// `TSYNC` with threads changing themselves in its second window.
    Race,
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
    let (_, status) = scenario(Ending::TrapNative, false)?;
    if status != SIGSYS_STATUS {
        return Err("a trapped native-range call did not end the process that made it");
    }
    report.traps += 1;
    report.killed += 1;
    // `TSYNC` while threads are being made: every thread has the chain.
    let (seen, status) = scenario(Ending::Sync, false)?;
    if status != SURVIVED {
        return Err("the synchronised threads' process did not end as the check ended it");
    }
    report.synced = seen.threads;
    report.sync_nanos = seen.sync_nanos;
    report.sync_threads = seen.sync_threads;
    report.sync_hold = seen.sync_hold;
    report.sync_holds = seen.sync_holds;
    // `TSYNC` while threads change themselves in its second window.
    let (seen, status) = scenario(Ending::Race, false)?;
    if status != SURVIVED {
        return Err("the threads racing TSYNC's process did not end as the check ended it");
    }
    report.raced += seen.raced;
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
    process.add_thread_from(&thread, creator);
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

/// How many times [`longest_chain`] times `TSYNC`'s walk of the chain.
const ANCESTOR_WALKS: u64 = 20;

/// The longest chain Linux allows is made and released: 6,554 one-instruction
/// filters, each pointing at the one before, dropped by one reference. A
/// recursive release would run the kernel stack out (SR11).
fn longest_chain() -> Result<(usize, u64, u64, u64), &'static str> {
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
    // What `TSYNC` pays to test one other thread against this chain at its
    // worst: a filter that is in no chain is looked for in all of it, 6,554
    // comparisons, which the sync makes under the thread-list lock.
    let stranger = seccomp::prepare(&one, false).map_err(|_| "a filter could not be made")?;
    let start = crate::timer::now_nanos();
    for _ in 0..ANCESTOR_WALKS {
        if core::hint::black_box(seccomp::found_in_chain(&env.thread, &stranger)) {
            return Err("a filter in no chain was found in the longest one");
        }
    }
    let ancestor = crate::timer::now_nanos().saturating_sub(start) / ANCESTOR_WALKS;
    drop(stranger);
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
    Ok((made, walked, released, ancestor))
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
/// mode, a member killed by its filter, a leader killed while a member lives
/// on, the traps, and `TSYNC` among threads being made. `None` for the rest;
/// an error is the check's.
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
        Some(Ending::Trap | Ending::TrapIgnored | Ending::TrapNative) => {
            trap_endings(ending, env, call)
        }
        Some(Ending::Sync) => {
            let _ = attributes::sys_prctl(process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            (seen.threads, seen.sync_nanos, seen.sync_threads) = synchronised(env)?;
            seen.sync_hold = HOLD_LONGEST.load(core::sync::atomic::Ordering::Acquire);
            seen.sync_holds = HOLDS.load(core::sync::atomic::Ordering::Acquire);
            Ok(Some(Then::Report(Ok(seen))))
        }
        Some(Ending::Race) => {
            let _ = attributes::sys_prctl(process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            seen.raced = raced(env)?;
            Ok(Some(Then::Report(Ok(seen))))
        }
        Some(Ending::Thread | Ending::Process) | None => Ok(None),
    }
}

/// [`ended_early`]'s traps: one a handler takes, one with `SIGSYS` blocked and
/// ignored, and one of a native-range call. The last two end the process, so
/// each names the trapped call for [`scenario_task`] to make.
fn trap_endings(
    ending: Option<Ending>,
    env: &Env,
    call: &dyn Fn(usize, [u64; 6]) -> Result<isize, &'static str>,
) -> Result<Option<Then>, &'static str> {
    let seccomp_call = number(Syscall::Seccomp)?;
    let process = &env.process;
    let thread = &env.thread;
    let mut seen = Seen::default();
    match ending {
        Some(Ending::Trap) => {
            seen.trapped = trapped_in_the_task(env)?;
            Ok(Some(Then::Report(Ok(seen))))
        }
        Some(Ending::TrapIgnored) => {
            let _ = attributes::sys_prctl(process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            let getsid = number(Syscall::Getsid)?;
            // Blocked and ignored: a trap must still end the program.
            thread.with_signals(|shared, _| shared.install_action(SIGSYS_NUMBER, 1, 0));
            crate::syscall::signal::change_blocked(thread, |_, own| {
                let _ = own.replace_blocked(crate::syscall::signal::bit(SIGSYS_NUMBER));
            });
            let at = env.put(&answering(&[(getsid, TRAP | 1)]), None)?;
            if call(seccomp_call, [SET_MODE_FILTER, 0, at, 0, 0, 0])? != 0 {
                return Err("a trapping filter was refused through the entry");
            }
            Ok(Some(Then::Ending {
                found: Ok(seen),
                call: getsid,
                returned: "a blocked and ignored SIGSYS let a trapped call return",
            }))
        }
        Some(Ending::TrapNative) => {
            let _ = attributes::sys_prctl(process, NO_NEW_PRIVS, [1, 0, 0, 0]);
            let gap = ferrix_native_abi::nr::LAST;
            thread.with_signals(|shared, _| shared.install_action(SIGSYS_NUMBER, HANDLER, 0));
            let at = env.put(&answering(&[(gap, TRAP | 1)]), None)?;
            if call(seccomp_call, [SET_MODE_FILTER, 0, at, 0, 0, 0])? != 0 {
                return Err("a trapping filter was refused through the entry");
            }
            Ok(Some(Then::Ending {
                found: Ok(seen),
                call: gap,
                returned: "a trapped native-range call returned",
            }))
        }
        _ => Ok(None),
    }
}

/// What `judge_thread` answers for a call a filter trapped.
const TRAPPED: u32 = u32::MAX;

/// A handler address the check gives `SIGSYS`: any address that is not
/// `SIG_DFL` or `SIG_IGN`, since nothing runs it.
const HANDLER: u64 = 0x0040_0000;

/// `SIGSYS`.
const SIGSYS_NUMBER: u32 = 31;

/// `TRAP` in a thread that has a handler: the call does not run, `SIGSYS` is
/// pending for this thread, and the `siginfo` a handler would read says what Linux's
/// `force_sig_seccomp` says (`docs/SECCOMP.md` §3.6, SR9). Runs in the task of
/// a check process, because the signal is forced on the running thread.
fn trapped_in_the_task(env: &Env) -> Result<usize, &'static str> {
    use crate::syscall::signal;
    let getppid = number(Syscall::Getppid)?;
    let _ = attributes::sys_prctl(&env.process, NO_NEW_PRIVS, [1, 0, 0, 0]);
    // A handler, unblocked, as Chromium installs it. (Blocked, Linux's
    // `force_sig_info_to_task` resets it to the default and the trap ends the
    // process: the `TrapIgnored` scenario.)
    env.thread
        .with_signals(|shared, _| shared.install_action(SIGSYS_NUMBER, HANDLER, 0));
    env.install(&answering(&[(getppid, TRAP | 0x1234)]), 0)?;
    if env.judges(getppid) != Some(TRAPPED) {
        return Err("a TRAP result did not trap the call");
    }
    let taken = env
        .thread
        .with_signals(signal::take_next)
        .ok_or("a trapped call raised no signal its handler could take")?;
    let signal::Origin::Sys {
        code,
        errno,
        call_addr,
        syscall,
        arch: token,
    } = taken.origin
    else {
        return Err("a trapped call raised a signal that is not a seccomp trap");
    };
    if taken.signal != SIGSYS_NUMBER
        || code != signal::SYS_SECCOMP
        || errno != 0x1234
        || call_addr != 0x1000
        || syscall != getppid as i32
        || token != arch_token()
    {
        return Err("a trapped call's siginfo did not carry the call, its ip and its arch");
    }
    // A trap is never a restart: a call whose first argument reads as
    // `ERESTARTSYS` (-512) is trapped like any, and the thread has no call
    // marked for restart afterwards. Restarts are marked by the dispatcher, which
    // a trap never reaches.
    let thread_restart = env.thread.with_signals(|_, own| own.take_restart());
    if thread_restart.is_some() {
        return Err("a trapped call left a restart marked");
    }
    let minus_512 = (-512_i64) as u64;
    if env.judges_with(getppid, [minus_512, 0, 0, 0, 0, 0]) != Some(TRAPPED) {
        return Err("a trapped call whose first argument reads as ERESTARTSYS was not trapped");
    }
    if env
        .thread
        .with_signals(|_, own| own.take_restart())
        .is_some()
    {
        return Err(
            "a trapped call whose first argument reads as ERESTARTSYS was marked for restart",
        );
    }
    // The signal it raised is the trap's, once: take it as the first did.
    let again = env
        .thread
        .with_signals(signal::take_next)
        .ok_or("a trapped call raised no signal")?;
    if again.signal != SIGSYS_NUMBER {
        return Err("a trapped call raised a signal other than SIGSYS");
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

/// `TSYNC`'s rules on threads that are only objects: the new chain reaches
/// every thread, a thread with a chain of its own makes the whole call fail
/// with its id (or `ESRCH`) and changes nothing, and a thread in strict mode
/// does the same (`docs/SECCOMP.md` §3.7, SR10).
fn tsync_rules(report: &mut Report) -> Result<(), &'static str> {
    let getppid = number(Syscall::Getppid)?;
    let getuid = number(Syscall::Getuid)?;
    let env = Env::new()?;
    if attributes::sys_prctl(&env.process, NO_NEW_PRIVS, [1, 0, 0, 0]) != Ok(0) {
        return Err("PR_SET_NO_NEW_PRIVS was refused");
    }
    // One sibling made before any filter, one after the first.
    let early = member_thread(&env, &env.thread)?;
    env.install(&answering(&[(getppid, ERRNO | EPERM)]), 0)?;
    let late = member_thread(&env, &env.thread)?;
    if early.with_seccomp(|state| state.filters()) != 0
        || late.with_seccomp(|state| state.filters()) != 1
    {
        return Err("a thread was made with another chain than its creator's");
    }
    // Everyone gets the second filter, the thread that had none included.
    let second = env.put(&answering(&[(getuid, ERRNO | 13)]), None)?;
    if env.seccomp(SET_MODE_FILTER, TSYNC, second) != Ok(0) {
        return Err("TSYNC among threads that can all take the filter was refused");
    }
    for thread in [&env.thread, &early, &late] {
        if judge_thread(thread, getuid) != Some(13) {
            return Err("a thread was left without the chain TSYNC gave");
        }
    }
    if early.with_seccomp(|state| state.filters()) != 2 {
        return Err("a thread that had no filter did not get the whole chain");
    }
    report.refusals += 1;

    // A thread with a chain of its own: the whole call fails with its id, and
    // nothing changes -- not the caller, not the threads that could have taken
    // the filter.
    let foreign = member_thread(&env, &env.thread)?;
    foreign.with_seccomp(|state| *state = State::default());
    let own = Env {
        process: Arc::clone(&env.process),
        thread: Arc::clone(&foreign),
        page: env.page,
    };
    own.install(&answering(&[(getppid, ERRNO | 5)]), 0)?;
    let chains = |threads: [&Arc<Thread>; 4]| -> Vec<u32> {
        threads
            .iter()
            .map(|thread| thread.with_seccomp(|state| state.filters()))
            .collect()
    };
    let before = chains([&env.thread, &early, &late, &foreign]);
    let third = env.put(&answering(&[(getppid, ERRNO | ENOSYS)]), None)?;
    if env.seccomp(SET_MODE_FILTER, TSYNC, third) != Ok(foreign.tid() as usize) {
        return Err("a TSYNC blocked by a thread with a chain of its own did not answer its id");
    }
    if env.seccomp(SET_MODE_FILTER, TSYNC | TSYNC_ESRCH, third) != Err(Errno::ESRCH) {
        return Err("a blocked TSYNC with TSYNC_ESRCH did not answer ESRCH");
    }
    if before != chains([&env.thread, &early, &late, &foreign]) {
        return Err("a TSYNC that failed changed a thread's chain");
    }
    report.refusals += 2;
    process::kill(&env.process, 137);
    Ok(())
}

/// A thread object of the check's process, made by `creator` and listed as a
/// `clone(CLONE_THREAD)` lists one, which never runs.
fn member_thread(env: &Env, creator: &Thread) -> Result<Arc<Thread>, &'static str> {
    let tid = crate::syscall::registry::allocate_thread(&env.process)
        .ok_or("no thread id for a seccomp check")?;
    let thread = Thread::sibling(&env.process, tid, creator)
        .map_err(|_| "no memory for a seccomp check's thread")?;
    let thread = Arc::new(thread);
    env.process.add_thread_from(&thread, creator);
    Ok(thread)
}

/// How many threads the cloner has made, and 1,000 more once it has made all.
static CLONED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// The process's threads stop when this is set.
static LEAVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// How many threads the cloner makes.
const CLONES: u32 = 40;
/// How many it has made when the sync starts.
const CLONES_BEFORE_SYNC: u32 = 6;

/// A member's task: runs until told to leave, then ends its thread.
fn member(_argument: usize) {
    while !LEAVE.load(core::sync::atomic::Ordering::Acquire) {
        sched::yield_now();
    }
    process::leave_current();
}

/// The cloning thread's task: makes [`CLONES`] more threads, each by the path
/// `clone(CLONE_THREAD)` takes (a thread made of its creator's state and
/// listed under the lock), with the sync running in the middle.
fn cloner(_argument: usize) {
    if let Some(me) = crate::syscall::thread::current() {
        let process = Arc::clone(me.process());
        for _ in 0..CLONES {
            if spawn_member(&process, &me, member).is_err() {
                break;
            }
            let _ = CLONED.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            sched::yield_now();
        }
        // Done: the count says so, and this thread stays until told to leave.
        let _ = CLONED.fetch_add(1_000, core::sync::atomic::Ordering::AcqRel);
    }
    member(0);
}

/// `TSYNC` among threads of a process that are being made while it runs: eight
/// threads and a ninth that makes [`CLONES`] more, one of them in the middle of
/// the sync. Every thread, those too, must hold the chain after (SR10).
fn synchronised(env: &Env) -> Result<(usize, u64, usize), &'static str> {
    use core::sync::atomic::Ordering;
    let getppid = number(Syscall::Getppid)?;
    CLONED.store(0, Ordering::Release);
    LEAVE.store(false, Ordering::Release);
    let leader = Arc::clone(&env.thread);
    let mut tasks = Vec::new();
    for _ in 0..7 {
        tasks.push(spawn_member(&env.process, &leader, member)?);
    }
    // The cloning thread, made and listed as the others are, its task too.
    tasks.push(spawn_member(&env.process, &leader, cloner)?);
    // Let it run until it has made a few, then synchronise in the middle of
    // the rest.
    let patience = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while CLONED.load(Ordering::Acquire) < CLONES_BEFORE_SYNC {
        if crate::timer::now_nanos() > patience {
            LEAVE.store(true, Ordering::Release);
            return Err("the cloning thread made no threads");
        }
        sched::yield_now();
    }
    let at = env.put(&answering(&[(getppid, ERRNO | EPERM)]), None)?;
    // Timed whole: both holds of the thread-list lock and the test between
    // them (`docs/certification/MEMORY-AND-TIMING.md` §2.2b).
    let threads_then = env.process.threads().len();
    HOLD_LONGEST.store(0, Ordering::Release);
    HOLDS.store(0, Ordering::Release);
    seccomp::arm_sync_pause(&env.thread, time_holds);
    let start = crate::timer::now_nanos();
    let synced = env.seccomp(SET_MODE_FILTER, TSYNC, at);
    let took = crate::timer::now_nanos().saturating_sub(start);
    seccomp::disarm_sync_pause();
    // Wait for the rest to be made.
    while CLONED.load(Ordering::Acquire) < 1_000 {
        if crate::timer::now_nanos() > patience {
            LEAVE.store(true, Ordering::Release);
            return Err("the cloning thread never finished");
        }
        sched::yield_now();
    }
    let listed = env.process.threads();
    let unfiltered = listed
        .iter()
        .filter(|thread| judge_thread(thread, getppid) != Some(EPERM))
        .count();
    let count = listed.len();
    LEAVE.store(true, Ordering::Release);
    drop(listed);
    drop(tasks);
    if synced != Ok(0) {
        return Err("TSYNC among running threads was refused");
    }
    if unfiltered != 0 {
        return Err("a thread made during TSYNC was not filtered");
    }
    if count < 9 + CLONES as usize {
        return Err("the threads of the TSYNC check were not all there");
    }
    Ok((count, took, threads_then))
}

/// When the running-threads case's `TSYNC` took the thread-list lock last.
static HOLD_START: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The longest it held it, in nanoseconds.
static HOLD_LONGEST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many holds it made.
static HOLDS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Time each hold of the thread-list lock a `TSYNC` makes, from just after it
/// is taken to just before it is released (`docs/certification/
/// MEMORY-AND-TIMING.md` §2.2b): the time other threads of the process wait
/// on it, with preemption off on this processor.
fn time_holds(window: seccomp::Window) {
    use core::sync::atomic::Ordering;
    match window {
        seccomp::Window::Holding => HOLD_START.store(crate::timer::now_nanos(), Ordering::Release),
        seccomp::Window::Held => {
            let held = crate::timer::now_nanos().saturating_sub(HOLD_START.load(Ordering::Acquire));
            let _ = HOLD_LONGEST.fetch_max(held, Ordering::AcqRel);
            let _ = HOLDS.fetch_add(1, Ordering::AcqRel);
        }
        seccomp::Window::Tested | seccomp::Window::Confirmed => {}
    }
}

/// What `TSYNC`'s first window hook changes: set before the call, taken by the
/// hook, so that it changes them once.
struct Changers {
    /// A thread object that installs a filter of its own in the window.
    filterer: Arc<Thread>,
    /// The filter it installs.
    filter: Arc<seccomp::Filter>,
    /// A thread object that enters strict mode in the window.
    strict: Arc<Thread>,
}

/// The threads [`change_in_tested_window`] changes.
static TESTED_WINDOW: SpinLock<Option<Changers>> = SpinLock::new(None);
/// Whether it changed both.
static TESTED_CHANGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `TSYNC`'s pause after its test, under no lock: one thread installs a filter
/// of its own there and another enters strict mode, each as its own call
/// would, through the functions the calls use.
fn change_in_tested_window(window: seccomp::Window) {
    if window != seccomp::Window::Tested {
        return;
    }
    let taken = TESTED_WINDOW.lock().take();
    let Some(changers) = taken else {
        return;
    };
    let attached = seccomp::attach(&changers.filterer, changers.filter).is_ok();
    let strict = seccomp::do_seccomp(
        changers.strict.process(),
        &changers.strict,
        SET_MODE_STRICT,
        0,
        0,
        Abi::Native,
    ) == Ok(0);
    TESTED_CHANGED.store(attached && strict, core::sync::atomic::Ordering::Release);
}

/// `TSYNC`'s first window (the consultant's B1 on S5): its step 2 tests every
/// other thread with no lock held, and between that test and the confirmation
/// under the thread-list lock one thread installs a filter of its own and
/// another enters strict mode. The round must see both and be made again, so
/// that the call answers the first changed thread's id, both keep what they
/// did (SR5: filters only accumulate, and strict mode is never loosened), and
/// nothing changes (SR10).
fn tsync_tested_window(report: &mut Report) -> Result<(), &'static str> {
    let getppid = number(Syscall::Getppid)?;
    let getuid = number(Syscall::Getuid)?;
    let env = Env::new()?;
    if attributes::sys_prctl(&env.process, NO_NEW_PRIVS, [1, 0, 0, 0]) != Ok(0) {
        return Err("PR_SET_NO_NEW_PRIVS was refused");
    }
    let filterer = member_thread(&env, &env.thread)?;
    let strict = member_thread(&env, &env.thread)?;
    let filter = seccomp::prepare(&answering(&[(getuid, ERRNO | RACED_ERRNO)]), false)
        .map_err(|_| "a filter could not be made")?;
    TESTED_CHANGED.store(false, core::sync::atomic::Ordering::Release);
    *TESTED_WINDOW.lock() = Some(Changers {
        filterer: Arc::clone(&filterer),
        filter,
        strict: Arc::clone(&strict),
    });
    let at = env.put(&answering(&[(getppid, ERRNO | EPERM)]), None)?;
    seccomp::arm_sync_pause(&env.thread, change_in_tested_window);
    let synced = env.seccomp(SET_MODE_FILTER, TSYNC, at);
    seccomp::disarm_sync_pause();
    let left = TESTED_WINDOW.lock().take();
    drop(left);
    if !TESTED_CHANGED.load(core::sync::atomic::Ordering::Acquire) {
        return Err("the TSYNC check's threads could not change themselves in its first window");
    }
    if judge_thread(&filterer, getuid) != Some(RACED_ERRNO) {
        return Err("a filter a thread installed while TSYNC tested was taken off it");
    }
    if strict.read_seccomp(State::mode) != seccomp::Mode::Strict {
        return Err(
            "a thread that entered strict mode while TSYNC tested was put back to filter mode",
        );
    }
    if synced != Ok(filterer.tid() as usize) {
        return Err(
            "a TSYNC whose threads changed while it tested did not answer the changed thread's id",
        );
    }
    if env.judges(getppid) != Some(0) {
        return Err("a TSYNC that failed changed its caller's chain");
    }
    report.raced += 2;
    process::kill(&env.process, 137);
    Ok(())
}

/// The errno the filters the racing threads install give `getuid`.
const RACED_ERRNO: u32 = 21;
/// How many threads race the second window.
const RACERS: u32 = 2;
/// How long the second window's hook waits, under the thread-list lock, for
/// the racers to begin their calls: they are running on other processors.
const RACE_ENTER_NANOS: u64 = 50_000_000;
/// How long it then holds the window open for them to finish, which they can
/// only if their calls do not take the thread-list lock.
const RACE_HOLD_NANOS: u64 = 2_000_000;

/// Whether the second window's hook is still to run.
static RACE_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The thread object its hook marks as killed by its own filter.
static RACE_DEAD: SpinLock<Option<Arc<Thread>>> = SpinLock::new(None);
/// The filter the racing filterer installs.
static RACE_FILTER: SpinLock<Option<Arc<seccomp::Filter>>> = SpinLock::new(None);
/// Set by the hook: the racers make their calls.
static RACE_GO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Racers that have begun their call.
static RACE_ENTERED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Racers whose call has returned.
static RACE_DONE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Racers the hook saw begin their call while it held the window open.
static RACE_SEEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// The racing filterer's thread id, once it runs.
static RACE_FILTERER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// The racing strict thread's id, once it runs.
static RACE_STRICT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Whether the filterer's filter was attached.
static RACE_ATTACHED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Whether the strict thread was let into strict mode.
static RACE_STRICT_OK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `TSYNC`'s pause once every thread is confirmed, under the thread-list lock,
/// before anything is changed. A filter marks one thread killed (`Dead`), as
/// it does with no lock but the thread's own, and the two racers, running on
/// other processors, are let go: one installs a filter of its own, the other
/// enters strict mode. The window is held open until both have begun and for
/// a while more, in which each finishes unless its call waits for the
/// thread-list lock.
fn race_in_confirmed_window(window: seccomp::Window) {
    use core::sync::atomic::Ordering;
    if window != seccomp::Window::Confirmed || !RACE_ARMED.swap(false, Ordering::AcqRel) {
        return;
    }
    // The check's own lock, a leaf here: the thread's leaf lock is taken
    // inside it, and the reference stays in the static, so nothing is freed.
    if let Some(dead) = RACE_DEAD.lock().as_ref() {
        seccomp::mark_dead(dead);
    }
    RACE_GO.store(true, Ordering::Release);
    let start = crate::timer::now_nanos();
    while RACE_ENTERED.load(Ordering::Acquire) < RACERS
        && crate::timer::now_nanos().saturating_sub(start) < RACE_ENTER_NANOS
    {
        core::hint::spin_loop();
    }
    RACE_SEEN.store(RACE_ENTERED.load(Ordering::Acquire), Ordering::Release);
    let start = crate::timer::now_nanos();
    while RACE_DONE.load(Ordering::Acquire) < RACERS
        && crate::timer::now_nanos().saturating_sub(start) < RACE_HOLD_NANOS
    {
        core::hint::spin_loop();
    }
}

/// Wait for the hook to let the racers go; `false` if the check gave up.
fn race_wait() -> bool {
    use core::sync::atomic::Ordering;
    let patience = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !RACE_GO.load(Ordering::Acquire) {
        if LEAVE.load(Ordering::Acquire) || crate::timer::now_nanos() > patience {
            return false;
        }
        sched::yield_now();
    }
    true
}

/// The racing filterer's task: installs a filter of its own in the window.
fn race_filter(_argument: usize) {
    use core::sync::atomic::Ordering;
    if let Some(me) = crate::syscall::thread::current() {
        let filter = RACE_FILTER.lock().take();
        RACE_FILTERER.store(me.tid(), Ordering::Release);
        if race_wait() {
            let _ = RACE_ENTERED.fetch_add(1, Ordering::AcqRel);
            let attached = filter.is_some_and(|filter| seccomp::attach(&me, filter).is_ok());
            RACE_ATTACHED.store(attached, Ordering::Release);
            let _ = RACE_DONE.fetch_add(1, Ordering::AcqRel);
        }
    }
    member(0);
}

/// The racing strict thread's task: enters strict mode in the window, through
/// `seccomp(SET_MODE_STRICT)`.
fn race_strict(_argument: usize) {
    use core::sync::atomic::Ordering;
    if let Some(me) = crate::syscall::thread::current() {
        RACE_STRICT.store(me.tid(), Ordering::Release);
        if race_wait() {
            let _ = RACE_ENTERED.fetch_add(1, Ordering::AcqRel);
            let entered =
                seccomp::do_seccomp(me.process(), &me, SET_MODE_STRICT, 0, 0, Abi::Native) == Ok(0);
            RACE_STRICT_OK.store(entered, Ordering::Release);
            let _ = RACE_DONE.fetch_add(1, Ordering::AcqRel);
        }
    }
    member(0);
}

/// `TSYNC`'s second window (the consultant's B1 on S5): once every thread is
/// confirmed and before any is changed, under the thread-list lock, a filter
/// marks one thread killed and two running threads try to install a filter of
/// their own and to enter strict mode. Their calls take the same lock, so they
/// wait for the sync and act on what it made: the filter goes on top of the
/// synchronised chain, and strict mode is refused, a filter being in. A sync
/// that changed a thread after it changed itself would take its filter off
/// (SR5) or loosen strict mode; the killed thread stays killed. Answers how
/// many racers had begun their call while the window was open, which is
/// what shows the window was raced (none on one processor).
fn raced(env: &Env) -> Result<usize, &'static str> {
    use core::sync::atomic::Ordering;
    let getppid = number(Syscall::Getppid)?;
    let getuid = number(Syscall::Getuid)?;
    for flag in [&RACE_GO, &RACE_ATTACHED, &RACE_STRICT_OK, &LEAVE] {
        flag.store(false, Ordering::Release);
    }
    for count in [
        &RACE_ENTERED,
        &RACE_DONE,
        &RACE_SEEN,
        &RACE_FILTERER,
        &RACE_STRICT,
    ] {
        count.store(0, Ordering::Release);
    }
    let dead = member_thread(env, &env.thread)?;
    *RACE_FILTER.lock() = Some(
        seccomp::prepare(&answering(&[(getuid, ERRNO | RACED_ERRNO)]), false)
            .map_err(|_| "a filter could not be made")?,
    );
    let leader = Arc::clone(&env.thread);
    let tasks = [
        spawn_member(&env.process, &leader, race_filter)?,
        spawn_member(&env.process, &leader, race_strict)?,
    ];
    let patience = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while RACE_FILTERER.load(Ordering::Acquire) == 0 || RACE_STRICT.load(Ordering::Acquire) == 0 {
        if crate::timer::now_nanos() > patience {
            LEAVE.store(true, Ordering::Release);
            return Err("the threads racing TSYNC never ran");
        }
        sched::yield_now();
    }
    *RACE_DEAD.lock() = Some(Arc::clone(&dead));
    RACE_ARMED.store(true, Ordering::Release);
    let at = env.put(&answering(&[(getppid, ERRNO | EPERM)]), None)?;
    seccomp::arm_sync_pause(&env.thread, race_in_confirmed_window);
    let synced = env.seccomp(SET_MODE_FILTER, TSYNC, at);
    seccomp::disarm_sync_pause();
    RACE_ARMED.store(false, Ordering::Release);
    let left = RACE_DEAD.lock().take();
    drop(left);
    // A racer that waited for the lock finishes after the sync.
    while RACE_GO.load(Ordering::Acquire) && RACE_DONE.load(Ordering::Acquire) < RACERS {
        if crate::timer::now_nanos() > patience {
            LEAVE.store(true, Ordering::Release);
            return Err("a thread racing TSYNC never finished its call");
        }
        sched::yield_now();
    }
    let filterer = env
        .process
        .thread_by_tid(RACE_FILTERER.load(Ordering::Acquire));
    let strict = env
        .process
        .thread_by_tid(RACE_STRICT.load(Ordering::Acquire));
    let kept_filter = filterer
        .as_ref()
        .is_some_and(|thread| judge_thread(thread, getuid) == Some(RACED_ERRNO));
    let strict_kept = strict
        .as_ref()
        .is_some_and(|thread| thread.read_seccomp(State::mode) == seccomp::Mode::Strict);
    let dead_kept = dead.read_seccomp(State::mode) == seccomp::Mode::Dead;
    let attached = RACE_ATTACHED.load(Ordering::Acquire);
    let strict_entered = RACE_STRICT_OK.load(Ordering::Acquire);
    LEAVE.store(true, Ordering::Release);
    drop((filterer, strict));
    drop(tasks);
    if synced != Ok(0) {
        return Err("TSYNC with threads racing it was refused");
    }
    if !attached {
        return Err("a thread racing TSYNC was refused a filter of its own");
    }
    if !kept_filter {
        return Err("a filter a thread installed during TSYNC's change was taken off it");
    }
    if strict_entered && !strict_kept {
        return Err(
            "a thread that entered strict mode during TSYNC's change was put back to filter mode",
        );
    }
    if !dead_kept {
        return Err("TSYNC brought a thread its filter had killed back to filter mode");
    }
    Ok(RACE_SEEN.load(Ordering::Acquire) as usize)
}
