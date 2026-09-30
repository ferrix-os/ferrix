//! Stage 7's self-checks: the dispatch path, on the real architecture.
//!
//! These do something the host tests in `src/lib/proto/linux-abi` cannot, and it is the
//! whole reason they exist. That crate checks all three number tables against
//! each other; what it cannot check is *which one this kernel was built to
//! use*. A build that reached for the wrong table would pass every host test
//! and then answer a program's `write` with `unlink`, and nothing short of
//! running on the machine can tell the difference.
//!
//! So the checks below assert the identity of the table by its content: they
//! ask for a number that means one thing on this architecture and something
//! else, or nothing, on the other two.
//!
//! The second thing they establish is that the path is total. A trap vector
//! has nowhere to report a failure to — a program is sitting on the other end
//! of it — so `dispatch` has to end in a value for every input, including the
//! numbers no table has.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_bootinfo::{KERNEL_HALF_BASE, PAGE_SIZE};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall as Call;
use ferrix_linux_abi::socket::{
    AF_MAX, AF_UNIX, CmsgHdr, MSG_CTRUNC, MSG_NOSIGNAL, MSG_OOB, MSG_PEEK, MSG_TRUNC, MSG_WAITALL,
    MsgHdr, SCM_CREDENTIALS, SCM_MAX_FD, SCM_RIGHTS, SHUT_RD, SHUT_WR, SIOCINQ, SIOCOUTQ,
    SO_ACCEPTCONN, SO_DOMAIN, SO_ERROR, SO_PASSCRED, SO_PEERCRED, SO_PROTOCOL, SO_RCVBUF,
    SO_RCVTIMEO_OLD, SO_TYPE, SOCK_DGRAM, SOCK_NONBLOCK, SOCK_RAW, SOCK_RDM, SOCK_SEQPACKET,
    SOCK_STREAM, SOCKET_BUFFER_MIN, SOL_SOCKET, Ucred, Width, cmsg_len, cmsg_space,
};
use ferrix_linux_abi::types::{
    AT_FDCWD, F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC,
    MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE, MAP_SHARED, MREMAP_FIXED,
    MREMAP_MAYMOVE, O_APPEND, O_CLOEXEC, O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC, PROT_READ,
    PROT_WRITE, SEEK_CUR, SEEK_END, SEEK_SET, TCGETS, TCGETS2, TIOCGWINSZ,
};

use crate::arch;
use crate::mm;
use ferrix_elf::Class;

use crate::console::println;
use crate::syscall::credentials::Credentials;
use crate::syscall::load::Source::Bytes;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::{Outcome, SyscallArgs, dispatch, uaccess};
use crate::syscall::{exec, fd, file, image, load, signal, sockets, system, time};
use crate::user::space::MMAP_MIN_ADDR;

/// What the checks measured, for the boot log.
#[derive(Debug)]
pub(crate) struct Report {
    /// Numbers put through `dispatch`, across every check.
    pub(crate) dispatched: u32,
    /// How many of them were answered rather than refused.
    pub(crate) answered: u32,
    /// The architecture's number for `getpid`, printed so the boot log says
    /// which table this build actually used rather than asserting it silently.
    pub(crate) getpid_number: usize,
    /// Pages the handler checks mapped, wrote through and gave back.
    pub(crate) pages: u64,
    /// Frames the whole check cost once everything was dropped. Zero, or a
    /// handler is leaking.
    pub(crate) leaked: i64,
    /// The status a program run in user mode exited with, if this
    /// architecture can run one yet.
    pub(crate) user_status: Option<i32>,
    /// How many times each of two programs sharing one processor was switched
    /// to. Both at least twice, or they ran one after the other.
    pub(crate) concurrent: Option<(u64, u64)>,
    /// The status a spinning program reported after being killed from outside.
    pub(crate) killed: Option<i32>,
    /// What a program that forks and waits exited with: 24 when right.
    pub(crate) forked: Option<i32>,
    /// The status a program that made threads exited with.
    pub(crate) threaded: Option<i32>,
    /// How many programs whose last two threads called `exit` together each
    /// ended with their first thread's status.
    pub(crate) exits_together: Option<u32>,
    /// What a process ended with whose second thread replaced the program
    /// while its first waited: the new program's status when right.
    pub(crate) dethreaded: Option<i32>,
    /// What a program of three threads, stopped and continued, was killed
    /// with afterwards: 137 when right.
    pub(crate) stopped_threads: Option<i32>,
    /// What a program of two threads, whose signals reached and were handed on
    /// to the thread that could take them, was killed with afterwards: 137
    /// when right.
    pub(crate) handed_on: Option<i32>,
    /// How many runs of that program gave back every frame once reaped, and in
    /// which measured window they first did.
    pub(crate) reclaimed: Option<(u32, u32)>,
    /// What a program that signals itself exited with once its handler had
    /// run and returned: 77 when right.
    pub(crate) signalled: Option<i32>,
    /// What a program exited with whose parent and child each wrote a page
    /// they shared copy-on-write: 61 when right.
    pub(crate) copied: Option<i32>,
    /// What a program exited with whose child wrote a `MAP_SHARED` and a
    /// `MAP_PRIVATE` page: 62 when right.
    pub(crate) shared: Option<i32>,
    /// What a program that wrote a page after `mprotect` made it read-only was
    /// ended with: 139, `SIGSEGV`, when right.
    pub(crate) narrowed: Option<i32>,
    /// What a program exited with that `execve`d a program which exists, then
    /// one that does not: 42 and 2 when right.
    pub(crate) execed: Option<(i32, i32)>,
    /// What a program exited with that was started with 57 as its start
    /// argument and exits with its first argument register: 57 when right.
    pub(crate) started_with: Option<i32>,
    /// Processes the pid registry numbered, found, listed and let go.
    pub(crate) pids: u32,
    /// Whether an unmap on one processor was seen to wait for a copy holding
    /// its page on another; `false` with one processor, where it cannot run.
    pub(crate) unmap_waited: bool,
    /// Futex waiters a wake, a requeue and a wake across a `fork` roused: 3
    /// when right.
    pub(crate) futex_woken: usize,
    /// How the vDSO answered a program's clock -- by reading the TSC or by
    /// making the system call -- or `None` on an architecture without one.
    pub(crate) vdso: Option<&'static str>,
    /// Whether that vDSO is the signal return trampoline alone, with no
    /// clock for a program to call.
    pub(crate) vdso_trampoline_only: bool,
    /// Guest milliseconds each group of checks took, in order: the dispatch
    /// table, the handler checks with their leak window, and then each of the
    /// program checks.
    pub(crate) spent_ms: [u64; 9],
}

/// Run them. `Err` names the first thing that was not true.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut counter = Counter::default();
    // Guest milliseconds per group, printed at the end as stage 5 prints its
    // own: these checks are the boot's largest single stretch, two seconds of
    // a five-second boot under `tcg`, and a cost nobody can see is one nobody
    // will act on.
    let mut spent = [0u64; 9];
    let mut at = crate::timer::now_nanos();
    macro_rules! mark {
        ($index:expr) => {{
            let now = crate::timer::now_nanos();
            if let Some(slot) = spent.get_mut($index) {
                *slot = now.saturating_sub(at) / 1_000_000;
            }
            at = now;
        }};
    }

    let getpid_number = check_the_right_table_was_compiled_in(&mut counter)?;
    check_identity_answers(&mut counter)?;
    let pids = crate::syscall::registry::check()?;
    check_orphans_are_reparented()?;
    check_an_unknown_number_is_enosys(&mut counter)?;
    check_errors_encode_as_negative(&mut counter)?;
    check_a_call_needing_a_process_says_so(&mut counter)?;
    mark!(0);

    // Everything the handler checks allocate must come back. Measured around
    // the whole group rather than per check, so a leak anywhere in it shows.
    //
    // **Run twice, and measured on the second run.** The first run is warm-up
    // and its cost is not a leak: the kernel heap keeps the last page of each
    // size class it has used, deliberately, so that a workload oscillating
    // across a page boundary does not pay a buddy allocation per cycle. A
    // check that allocates a size nothing else does will therefore take a page
    // from the allocator and keep it, exactly once, and a single measured run
    // cannot tell that apart from a leak. The second run allocates the same
    // shapes into the pages the first left behind, so anything it fails to
    // return is real.
    //
    // This was not a hypothesis. A one-frame discrepancy appeared when the
    // loader checks landed and survived every attempt to find it in the
    // address space -- map, commit and drop in isolation was clean, and so was
    // building the image in isolation.
    //
    // The number sweep is inside the window with the handler checks, run once
    // for warm-up and once measured on the same terms, so that its process,
    // its address space and its task are held to the same count.
    let _warm = check_handlers(Output::Quiet)?;
    check_the_whole_number_space_is_total(&mut Counter::default())?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let pages = check_handlers(Output::Show)?;
    check_the_whole_number_space_is_total(&mut counter)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    // Checked, not only printed: a count nothing tested would boot green
    // through the very leak it exists to show.
    if leaked != 0 {
        mm::print_frame_delta("handlers", leaked);
        window.report("handlers");
        return Err("the handler checks did not give every frame back");
    }
    mark!(1);

    let user_status = check_a_program_runs_in_user_mode()?;
    mark!(2);
    let concurrent = check_two_programs_take_turns_on_one_processor()?;
    mark!(3);
    let killed = check_a_program_is_killed_from_outside()?;
    mark!(4);
    let forked = check_a_forked_child_is_waited_for()?;
    let threaded = check_a_thread_shares_its_process_and_ends_alone()?;
    check_clone_refuses_every_namespace()?;
    let exits_together = check_two_last_threads_exiting_together_end_their_process()?;
    check_a_reader_blocked_in_syslog_is_released_by_a_kill()?;
    // The hand-off decided in the kernel first, so that a fault at one of its
    // sites fails here by its own message before the program checks run it end
    // to end.
    check_a_signal_blocked_after_it_was_sent_is_handed_on()?;
    let dethreaded = check_execve_from_a_thread_ends_the_others()?;
    let stopped_threads = check_a_stop_stops_every_thread_and_a_continue_restarts_their_calls()?;
    let handed_on = check_a_signal_reaches_the_thread_that_can_take_it()?;
    mark!(5);
    let reclaimed = check_ended_programs_give_their_frames_back()?;
    let signalled = check_a_handler_runs_and_returns()?;
    check_a_poisoned_xsave_header_comes_back_safely()?;
    check_untested_signal_paths()?;
    mark!(6);
    let copied = check_a_copy_on_write_page_is_copied_for_the_side_that_writes()?;
    let shared = check_a_shared_mapping_is_shared_across_fork()?;
    let narrowed = check_a_write_after_mprotect_read_only_faults()?;
    check_an_ended_process_closes_its_descriptors()?;
    let unmap_waited =
        crate::syscall::unmap_check::check_an_unmap_waits_for_a_copy_holding_its_page()?;
    let execed = check_execve_replaces_the_program()?;
    let vdso = crate::syscall::vdso_check::check_the_vdso()?;
    mark!(7);
    let started_with = check_a_program_is_handed_its_start_argument()?;
    let futex_woken = check_futexes()?;
    a_requeue_crosses_buckets()?;
    check_brk_and_fork_wait_for_the_heap_lock()?;
    mark!(8);
    let _ = at;

    Ok(Report {
        dispatched: counter.dispatched,
        answered: counter.answered,
        getpid_number,
        pages,
        leaked,
        user_status,
        concurrent,
        killed,
        forked,
        threaded,
        exits_together,
        dethreaded,
        stopped_threads,
        handed_on,
        reclaimed,
        signalled,
        copied,
        shared,
        narrowed,
        execed,
        started_with,
        pids,
        unmap_waited,
        futex_woken,
        vdso,
        vdso_trampoline_only: vdso == Some(crate::syscall::vdso_check::SIGRETURN_ONLY),
        spent_ms: spent,
    })
}

/// A call that needs an address space is refused, rather than faulting.
///
/// Today every such call takes this path, because nothing creates a process
/// yet. When stage 6's transition lands, this check keeps meaning something:
/// a kernel thread making a system call must still be told no rather than
/// dereferencing a `None`.
fn check_a_call_needing_a_process_says_so(counter: &mut Counter) -> Result<(), &'static str> {
    let Some(number) = number_for(ferrix_linux_abi::nr::Syscall::Brk) else {
        return Err("this architecture has no number for brk");
    };
    let esrch = Outcome::Return(Errno::ESRCH.as_return_value());
    if counter.call(number) != esrch {
        return Err("a call needing a process was not refused with ESRCH");
    }
    Ok(())
}

/// Counts what went through, so the report is a measurement and not a claim.
#[derive(Default)]
struct Counter {
    dispatched: u32,
    answered: u32,
}

impl Counter {
    /// Dispatch one number with no arguments, and count it.
    fn call(&mut self, number: usize) -> Outcome {
        self.call_with(number, [0; 6])
    }

    /// Dispatch one number with arguments, and count it.
    fn call_with(&mut self, number: usize, args: [u64; 6]) -> Outcome {
        let outcome = dispatch(
            &SyscallArgs {
                abi: crate::trap::Abi::Native,
                number,
                args,
                ip: 0,
            },
            None,
        );
        self.dispatched = self.dispatched.saturating_add(1);
        if let Outcome::Return(value) = outcome
            && value >= 0
        {
            self.answered = self.answered.saturating_add(1);
        }
        outcome
    }
}

/// The number this architecture gives `getpid`, and proof it is that one.
///
/// `getpid` is the probe because all three tables have it and no two agree:
/// 39 on x86-64, 172 on AArch64, 20 on ARMv7-A. Asking the facade for its own
/// number and then requiring the *other two* numbers to mean something else is
/// what makes this a check rather than a tautology.
/// Verifies: `L.x86_64.119`, H.TRAP.12
fn check_the_right_table_was_compiled_in(counter: &mut Counter) -> Result<usize, &'static str> {
    let candidates = [
        ferrix_linux_abi::nr::x86_64::GETPID,
        ferrix_linux_abi::nr::aarch64::GETPID,
        ferrix_linux_abi::nr::arm::GETPID,
    ];

    let mut mine = None;
    for number in candidates {
        if arch::decode_syscall(number) == Some(ferrix_linux_abi::nr::Syscall::Getpid) {
            if mine.is_some() {
                return Err("two different numbers both decode to getpid");
            }
            mine = Some(number);
        }
    }
    let Some(number) = mine else {
        return Err("no table's getpid number decodes to getpid on this build");
    };

    // And it answers, rather than merely decoding.
    match counter.call(number) {
        Outcome::Return(value) if value > 0 => Ok(number),
        _ => Err("getpid decoded but did not answer with a process identifier"),
    }
}

/// The calls that need no process state answer, and agree with each other.
fn check_identity_answers(counter: &mut Counter) -> Result<(), &'static str> {
    let uid_calls = [
        ferrix_linux_abi::nr::Syscall::Getuid,
        ferrix_linux_abi::nr::Syscall::Geteuid,
        ferrix_linux_abi::nr::Syscall::Getgid,
        ferrix_linux_abi::nr::Syscall::Getegid,
    ];
    for call in uid_calls {
        let Some(number) = number_for(call) else {
            return Err("this architecture has no number for a credential call");
        };
        if counter.call(number) != Outcome::Return(0) {
            return Err("a credential call did not report root");
        }
    }

    // `getpid` and `gettid` must agree while there is one thread per process,
    // and disagreeing later is how a threaded program discovers there is more
    // than one of it.
    let pid = number_for(ferrix_linux_abi::nr::Syscall::Getpid)
        .ok_or("this architecture has no number for getpid")?;
    let tid = number_for(ferrix_linux_abi::nr::Syscall::Gettid)
        .ok_or("this architecture has no number for gettid")?;
    if counter.call(pid) != counter.call(tid) {
        return Err("getpid and gettid disagree with one thread running");
    }
    Ok(())
}

/// A process's children outlive it with a parent: the nearest ancestor still
/// running that set `PR_SET_CHILD_SUBREAPER`, or else init. An orphan that had
/// already ended is a zombie init's `wait4` takes with its status; one still
/// running ends later as init's child. The control is the same orphan with no
/// init to take it, which is left with no parent at all.
///
/// Runs before init starts, so it plays init itself, with pid 1, and gives
/// the pid back at the end.
fn check_orphans_are_reparented() -> Result<(), &'static str> {
    use crate::syscall::{attributes, registry};
    use crate::user::space::AddressSpace;

    const NO_SPACE: &str = "no address space for the orphan check";
    let make = || process::new_for_check().map_err(|_| NO_SPACE);
    let child_of = |parent: &Arc<Process>| -> Result<Arc<Process>, &'static str> {
        let space = AddressSpace::new().map_err(|_| NO_SPACE)?;
        let child = registry::register(
            Process::forked(parent, space, false, false).map_err(|_| "no memory for a fork")?,
        );
        parent.adopt(Arc::clone(&child));
        Ok(child)
    };
    let parent_is = |child: &Process, expected: &Arc<Process>| {
        child
            .parent()
            .is_some_and(|found| Arc::ptr_eq(&found, expected))
    };

    // The control, while nothing holds pid 1: nobody to hand the orphan to.
    if !registry::is_free(registry::INIT_PID) {
        return Err("init's pid was taken before the orphan check");
    }
    let parent = make()?;
    let orphan = child_of(&parent)?;
    process::kill(&parent, 0);
    if orphan.parent().is_some() {
        return Err("an orphan kept a parent when there was no init to take it");
    }
    process::kill(&orphan, 0);
    drop((parent, orphan));

    let space = AddressSpace::new().map_err(|_| NO_SPACE)?;
    let init = registry::register(Process::new_init(space).map_err(|_| "no memory for init")?);
    if init.pid() != registry::INIT_PID {
        return Err("the orphan check's init was not given pid 1");
    }

    // To init: one child still running, one already ended with status 3.
    let parent = make()?;
    let running = child_of(&parent)?;
    let ended = child_of(&parent)?;
    process::kill(&ended, 3);
    process::kill(&parent, 0);
    if !parent_is(&running, &init) || !parent_is(&ended, &init) {
        return Err("an orphan was not handed to init");
    }
    if !init.has_child(running.pid()) || !init.has_child(ended.pid()) {
        return Err("init's children do not include the orphans it was handed");
    }
    let ended_pid = ended.pid();
    match init.reap_child(&|child: &Process| child.pid() == ended_pid, true) {
        Ok(Some(child)) if child.wait_status() == Some(3 << 8) => {}
        _ => return Err("init could not reap an orphan that had ended, with its status"),
    }
    let running_pid = running.pid();
    process::kill(&running, 5);
    match init.reap_child(&|child: &Process| child.pid() == running_pid, true) {
        Ok(Some(child)) if child.wait_status() == Some(5 << 8) => {}
        _ => return Err("an orphan that ended under init was not init's to reap"),
    }
    drop((parent, running, ended));

    // An ancestor that reaps orphans takes them before init does, and once it
    // has ended itself they go on to init.
    let keeper = make()?;
    attributes::update(&keeper, |set| set.child_subreaper = true);
    let middle = child_of(&keeper)?;
    let grandchild = child_of(&middle)?;
    process::kill(&middle, 0);
    if !parent_is(&grandchild, &keeper) {
        return Err("an orphan went past an ancestor that reaps orphans");
    }
    process::kill(&keeper, 0);
    if !parent_is(&grandchild, &init) {
        return Err("the orphans of an ended reaper did not go on to init");
    }

    process::kill(&grandchild, 0);
    process::kill(&init, 0);
    if grandchild.parent().is_some() {
        return Err("init's children kept init as their parent after it ended");
    }
    drop((keeper, middle, grandchild, init));
    if !registry::is_free(registry::INIT_PID) {
        return Err("the orphan check's init did not give pid 1 back");
    }
    Ok(())
}

/// A number no table carries is `ENOSYS`, not a panic and not a wrong handler.
/// Verifies: L.trap.4, H.TRAP.11
fn check_an_unknown_number_is_enosys(counter: &mut Counter) -> Result<(), &'static str> {
    let enosys = Outcome::Return(Errno::ENOSYS.as_return_value());
    // 0xDEAD is above every table on all three architectures; the other two
    // are the edges, where an implementation that indexed rather than matched
    // would fall off.
    for number in [0xDEAD, usize::MAX, usize::MAX - 1] {
        if counter.call(number) != enosys {
            return Err("an unknown system call number was not refused with ENOSYS");
        }
    }
    Ok(())
}

/// Every number in the plausible range is answered by its handler rather than
/// trapped.
///
/// The sweep is the point: a `match` that decoded a number into a handler
/// which then read an argument it was not given would fault here, on a kernel
/// stack, with the scheduler running — which is a much better place to find it
/// than under a user program.
///
/// **From inside a process.** With no process, `dispatch` answers almost every
/// call `ESRCH` before it looks at an argument. A sweep from the boot task
/// therefore reached no handler, and it passed whatever the handlers did with
/// a poisoned register. So the sweep runs on a task of a check process, where
/// [`process::current`] finds that process, and each call gets as far into its
/// handler as its arguments let it.
///
/// **Skipped: `exit`, `exit_group`, `pause` and `alarm`, and nothing else.**
/// Ending the task is what the first two are for. `pause` takes no argument to
/// poison and waits for a signal nothing will send. `alarm` has no value to
/// refuse: any number arms a timer, and arming one starts the `itimers` thread,
/// which outlives the call and would be counted against the frame window.
/// Every other call is swept,
/// including the ones that could end or block a process given the right
/// arguments, because poisoned arguments must be refused before either can
/// happen:
///
/// * `fork`, `vfork`, `clone` and `clone3` are refused, because a kernel caller
///   has no saved registers for a child to resume from;
/// * `execve` and `execveat` are refused at the path or the descriptor, which
///   comes before the point of no return;
/// * `wait4` and `waitid` refuse the option bits, and the process has no child
///   to wait for anyway;
/// * `nanosleep` refuses the request pointer, `clock_nanosleep` the clock, and
///   `futex` the command;
/// * `reboot` refuses the magic numbers;
/// * `kill`, `tkill` and `tgkill` refuse the signal number or the thread id,
///   and `rt_sigsuspend` and `rt_sigtimedwait` the signal set's size;
/// * `poll` and `ppoll` refuse the descriptor count or the timeout pointer, and
///   `read` and the other descriptor calls a descriptor no table has.
///
/// If a handler blocks or ends the process on poisoned arguments, the check
/// fails rather than hangs: the process's exit status says which, and a
/// deadline covers a call that never returns. A call added later that would
/// block or end the process on these arguments belongs in the skip list with
/// its reason.
///
/// [`run`] calls this inside its frame-count window, so the process and its
/// task have to give back every frame. That is why each sweep waits until the
/// task is reaped before returning.
fn check_the_whole_number_space_is_total(counter: &mut Counter) -> Result<(), &'static str> {
    let swept = sweep_in_a_process()?;
    counter.dispatched = counter.dispatched.saturating_add(swept.dispatched);
    counter.answered = counter.answered.saturating_add(swept.answered);
    Ok(())
}

/// What one sweep put through `dispatch`, and how much of it was answered.
struct Swept {
    dispatched: u32,
    answered: u32,
}

/// How the sweep's task ends its process: every number answered.
const SWEEP_DONE: i32 = 83;
/// A call asked to enter user mode.
const SWEEP_ENTERED: i32 = 84;
/// The task did not find itself in the sweep's process.
const SWEEP_UNSEEN: i32 = 85;
/// `brk`, which needs a process, was refused for want of one.
const SWEEP_REFUSED: i32 = 86;

/// The pid of the process the sweep's task should find itself in.
static SWEEP_PID: AtomicU32 = AtomicU32::new(0);
/// Calls the sweep's task put through `dispatch`.
static SWEEP_DISPATCHED: AtomicU32 = AtomicU32::new(0);
/// How many of them answered with a value rather than an error.
static SWEEP_ANSWERED: AtomicU32 = AtomicU32::new(0);

/// Run one sweep on a task of a fresh check process, and wait until the task,
/// the process and its address space are gone.
fn sweep_in_a_process() -> Result<Swept, &'static str> {
    let arena = crate::vmap::usage().allocations;
    SWEEP_DISPATCHED.store(0, Ordering::Release);
    SWEEP_ANSWERED.store(0, Ordering::Release);

    let process = process::new_for_check().map_err(|_| "could not make a process for the sweep")?;
    SWEEP_PID.store(process.pid(), Ordering::Release);
    let thread = Arc::new(
        crate::syscall::thread::Thread::leader(&process)
            .map_err(|_| "no memory for a check's thread")?,
    );
    let task = crate::sched::spawn_user("sweep", sweep, thread, None, None)
        .map_err(|_| "could not start the sweep's task")?;

    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    match process.wait_for_exit(deadline) {
        Some(SWEEP_DONE) => {}
        Some(SWEEP_ENTERED) => {
            return Err("a system call in the ordinary range asked to enter user mode");
        }
        Some(SWEEP_UNSEEN) => {
            return Err("the sweep's task was not in its process, so it reached no handler");
        }
        Some(SWEEP_REFUSED) => return Err("brk was refused with ESRCH from inside a process"),
        Some(_) => return Err("a system call with poisoned arguments ended its process"),
        None => return Err("a system call with poisoned arguments never returned"),
    }

    // The task holds the process, and through it the address space, until the
    // scheduler has reaped it. Waited for by the task itself: the arena's count
    // alone comes back as soon as *some* earlier check's task is reaped.
    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    loop {
        let _ = crate::sched::reap();
        if task.is_dead()
            && Arc::strong_count(&task) == 1
            && crate::vmap::usage().allocations <= arena
        {
            break;
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("the sweep's task never gave its stack back");
        }
        crate::sched::yield_now();
    }
    drop(task);
    drop(process);

    Ok(Swept {
        dispatched: SWEEP_DISPATCHED.load(Ordering::Acquire),
        answered: SWEEP_ANSWERED.load(Ordering::Acquire),
    })
}

/// The sweep's task: sweep, then end the process with how it went.
fn sweep(_argument: usize) {
    process::exit_current(sweep_every_number());
}

/// Put every number in `0..=600` through `dispatch` with poisoned arguments,
/// from inside the sweep's process, and answer one of the `SWEEP_` statuses.
fn sweep_every_number() -> i32 {
    // Dropped before the first call. A call that ended the task would
    // otherwise strand this reference on its stack, and the process with it.
    let Some(pid) = process::current().map(|process| process.pid()) else {
        return SWEEP_UNSEEN;
    };
    if pid != SWEEP_PID.load(Ordering::Acquire) {
        return SWEEP_UNSEEN;
    }

    // Deliberately non-zero and not a valid pointer: a handler that decided to
    // dereference an argument should fault rather than quietly succeed.
    let poison = [0xAAAA_AAAA_AAAA_AAA0_u64; 6];
    let brk = number_for(ferrix_linux_abi::nr::Syscall::Brk);
    let esrch = Errno::ESRCH.as_return_value();
    for number in 0..=600 {
        if matches!(
            arch::decode_syscall(number),
            Some(
                ferrix_linux_abi::nr::Syscall::Exit
                    | ferrix_linux_abi::nr::Syscall::ExitGroup
                    | ferrix_linux_abi::nr::Syscall::Pause
                    | ferrix_linux_abi::nr::Syscall::Alarm
            )
        ) {
            continue;
        }
        let outcome = dispatch(
            &SyscallArgs {
                abi: crate::trap::Abi::Native,
                number,
                args: poison,
                ip: 0,
            },
            None,
        );
        let _ = SWEEP_DISPATCHED.fetch_add(1, Ordering::Relaxed);
        let Outcome::Return(value) = outcome else {
            return SWEEP_ENTERED;
        };
        if value >= 0 {
            let _ = SWEEP_ANSWERED.fetch_add(1, Ordering::Relaxed);
        }
        if Some(number) == brk && value == esrch {
            return SWEEP_REFUSED;
        }
    }
    SWEEP_DONE
}

/// A refusal lands in the range Linux reserves for one.
///
/// `include/linux/err.h` reserves `-4095..=-1`. A pointer-returning call whose
/// success value strayed into that range would be read as a failure by every C
/// library, so the boundary is worth asserting where it is decided.
fn check_errors_encode_as_negative(counter: &mut Counter) -> Result<(), &'static str> {
    let Outcome::Return(value) = counter.call(0xDEAD) else {
        return Err("an unknown number asked to enter user mode");
    };
    if !(-4095..0).contains(&value) {
        return Err("ENOSYS did not encode into the reserved error range");
    }
    if value != -38 {
        return Err("ENOSYS is 38 on every architecture Ferrix targets");
    }
    Ok(())
}

/// This architecture's number for a call, by asking the decoder rather than
/// naming a table.
///
/// A linear sweep because the tables run one way only: `src/lib/proto/linux-abi` maps a
/// number to a call and deliberately offers no inverse, since an inverse would
/// be a second copy of the table to disagree with the first.
fn number_for(call: ferrix_linux_abi::nr::Syscall) -> Option<usize> {
    (0..=600).find(|&number| arch::decode_syscall(number) == Some(call))
}

/// Make `call` for `process` as a program on this architecture would: by its
/// number, decoded by this build's own table, and through the table
/// `dispatch` uses once it has found a process.
///
/// For the self-checks outside this module that want a call to go in the way
/// a program's does, so that a number missing from one architecture's table,
/// or a routing line that sends the call elsewhere, fails on that architecture.
pub(crate) fn call_by_number(
    process: &Process,
    call: ferrix_linux_abi::nr::Syscall,
    args: [u64; 6],
) -> Result<usize, Errno> {
    let number = number_for(call).ok_or(Errno::ENOSYS)?;
    let decoded = arch::decode_syscall(number).ok_or(Errno::ENOSYS)?;
    crate::syscall::linux::handle(
        decoded,
        &SyscallArgs {
            abi: crate::trap::Abi::Native,
            number,
            args,
            ip: 0,
        },
        Some(process),
    )
}

// ---------------------------------------------------------------------------
// The handlers, against a real address space
//
// Everything below builds a `Process` over a real `AddressSpace` and calls the
// handlers the way `dispatch` will. That is the whole reason the handlers take
// `&Process` rather than reaching for a current one: `mmap` is exercised
// against the actual VMA tree and the actual page tables, on all three
// architectures, before a program exists that could call it.
//
// The leak check around them is not decoration. A `mmap` that forgets to
// release its object on `munmap` leaks at a rate nothing reports, and the
// machine dies of it an hour into a build.
// ---------------------------------------------------------------------------

/// Where the checks put a mapping. Well clear of where an ELF image would go,
/// and page-aligned.
const TEST_BASE: u64 = 0x2000_0000;

/// Run the handler checks, reporting how many pages ended up faulted in.
/// Whether this pass should run the checks that print.
///
/// The group runs twice and only the second is measured. Without this the boot
/// log would carry every `write` check's output twice, which reads as a bug in
/// `write` rather than as a deliberate warm-up.
///
/// Skipping them on the warm-up costs the measurement nothing: they allocate
/// exactly what the other checks do -- one mapping through `map_rw` -- and
/// `console::write_bytes` touches no heap unless the transmit ring is full,
/// when its wait lists the task the way every other wait does, so there is no
/// size class reachable only through them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    /// The warm-up run.
    Quiet,
    /// The measured run.
    Show,
}

fn check_handlers(output: Output) -> Result<u64, &'static str> {
    let process = process::new_for_check().map_err(|_| "could not make a process")?;

    check_mmap_returns_usable_memory(&process)?;
    check_mmap_rejects_what_it_should(&process)?;
    check_memory_and_time_answer_as_linux_does(&process)?;
    check_fixed_mapping_lands_where_asked(&process)?;
    check_nothing_is_mapped_near_page_zero(&process)?;
    check_copy_crosses_a_page_boundary(&process)?;
    check_a_user_pointer_into_the_kernel_is_refused(&process)?;
    check_an_unmapped_address_is_efault_not_a_kernel_fault(&process)?;
    check_a_c_string_stops_at_its_nul(&process)?;
    check_mprotect_takes_write_away(&process)?;
    check_mremap_moves_the_contents_and_shrinks_in_place(&process)?;
    check_mremap_below_mmap_min_addr_is_eperm(&process)?;
    check_brk_grows_and_shrinks(&process)?;
    check_set_tid_address_answers_with_a_thread_id(&process)?;
    check_a_robust_list_is_each_threads_own(&process)?;
    check_no_new_privs_crosses_a_fork()?;
    check_a_change_of_ids_ends_dumpability()?;
    check_no_new_privs_reaches_a_native_child()?;
    check_uname_names_the_system(&process)?;
    check_poll_reports_ready_invalid_and_skipped(&process)?;
    check_select_answers_with_the_sets_that_are_ready(&process)?;
    check_the_line_discipline_follows_its_settings()?;
    check_a_raw_read_waits_as_vmin_and_vtime_say()?;
    check_a_raw_read_waits_for_vmin()?;
    check_a_pseudoterminal_slave_reads_as_vmin_and_vtime_say()?;
    check_the_console_answers_as_a_terminal(&process)?;
    check_a_signal_disposition_reads_back_as_it_was_set(&process)?;
    check_the_blocked_mask_follows_how(&process)?;
    check_kill_finds_its_targets_and_refuses_what_it_should(&process)?;
    check_an_alternate_stack_is_recorded_and_refused_when_small(&process)?;
    check_an_image_loads_where_its_headers_say(&process)?;
    check_a_static_pie_loads_at_its_base()?;
    check_a_dynamic_program_is_loaded_with_its_linker()?;
    check_the_loader_refuses_a_linker_it_cannot_use()?;
    check_a_dynamic_program_enters_its_linker()?;
    check_the_loader_refuses_what_it_cannot_run(&process)?;
    check_descriptors(&process)?;
    check_what_an_applet_asks_of_the_system(&process)?;
    if output == Output::Show {
        check_write_reaches_the_console(&process)?;
        check_writev_gathers_in_order(&process)?;
    }

    // Counted rather than asserted. An earlier version of this reported a
    // constant, which says nothing about what actually ran -- a check that
    // returned early would have reported the same number.
    Ok(pages_touched(&process))
}

/// How many pages this process has a translation for.
///
/// Asked of the page tables, not of the VMA map: a region is a promise and a
/// translation is the thing that was actually paid for.
fn pages_touched(process: &Process) -> u64 {
    let at = map_rw(process, PAGE_SIZE * 4).unwrap_or(0);
    if at == 0 {
        return 0;
    }
    let root = process.space().root_table();
    let mut count = 0;
    for page in 0..4 {
        let address = at + page * PAGE_SIZE;
        // Touch it, then confirm the touch produced a translation.
        if uaccess::copy_to_user(process.space(), address, b"x").is_ok()
            && mm::translate_in(root, address).is_some()
        {
            count += 1;
        }
    }
    let _ = memory::sys_munmap(process, at, PAGE_SIZE * 4);
    count
}

/// `mmap` hands back memory the kernel can then write and read back.
fn check_mmap_returns_usable_memory(process: &Process) -> Result<(), &'static str> {
    let at = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: PAGE_SIZE,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "mmap of one anonymous page was refused")?;
    let at = u64::try_from(at).map_err(|_| "mmap returned an impossible address")?;

    if !at.is_multiple_of(PAGE_SIZE) {
        return Err("mmap returned an unaligned address");
    }

    let written = b"stage 7 was here";
    uaccess::copy_to_user(process.space(), at, written).map_err(|_| "could not write the page")?;
    let mut read = [0_u8; 16];
    uaccess::copy_from_user(process.space(), at, &mut read)
        .map_err(|_| "could not read the page back")?;
    if &read != written {
        return Err("what came back out of the page is not what went in");
    }

    // And it goes away again, pages and all.
    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    let mut after = [0_u8; 1];
    if uaccess::copy_from_user(process.space(), at, &mut after).is_ok() {
        return Err("an unmapped page was still readable");
    }
    Ok(())
}

/// The argument checks, which is where `mmap`'s bugs live.
fn check_mmap_rejects_what_it_should(process: &Process) -> Result<(), &'static str> {
    let anon = MAP_ANONYMOUS | MAP_PRIVATE;
    let cases: [(u64, u64, u32, u32, i64, &str); 5] = [
        (0, 0, PROT_READ, anon, -1, "a zero length"),
        (
            0,
            u64::MAX,
            PROT_READ,
            anon,
            -1,
            "a length that wraps when rounded",
        ),
        (0, PAGE_SIZE, 0x40, anon, -1, "an unknown protection bit"),
        (
            0,
            PAGE_SIZE,
            PROT_READ,
            MAP_PRIVATE,
            -1,
            "a file mapping, with no VFS",
        ),
        (
            0,
            PAGE_SIZE,
            PROT_READ,
            MAP_ANONYMOUS | MAP_PRIVATE | MAP_SHARED,
            -1,
            "both SHARED and PRIVATE",
        ),
    ];
    for (addr, len, prot, flags, fd, what) in cases {
        let request = MmapRequest {
            addr,
            len,
            prot,
            flags,
            fd,
            offset: 0,
            unit: OffsetUnit::Bytes,
        };
        if memory::sys_mmap(process, &request).is_ok() {
            let _ = what;
            return Err("mmap accepted arguments it should have refused");
        }
    }
    Ok(())
}

/// Where Linux answers a memory or time call differently from the obvious
/// reading, the answer here is Linux's.
fn check_memory_and_time_answer_as_linux_does(process: &Process) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{CLOCK_TAI, PROT_GROWSDOWN, PROT_GROWSUP, PROT_SEM};

    // An anonymous mapping ignores its fd, and a whole-page offset; a byte
    // offset off a page boundary is still refused.
    let anon = |fd: i64, offset: u64| MmapRequest {
        addr: 0,
        len: PAGE_SIZE,
        prot: PROT_READ | PROT_WRITE,
        flags: MAP_ANONYMOUS | MAP_PRIVATE,
        fd,
        offset,
        unit: OffsetUnit::Bytes,
    };
    let at = memory::sys_mmap(process, &anon(3, PAGE_SIZE))
        .map_err(|_| "an anonymous mapping with a real fd was refused")?;
    let at = u64::try_from(at).map_err(|_| "mmap returned an impossible address")?;
    if memory::sys_mmap(process, &anon(-1, 1)) != Err(Errno::EINVAL) {
        return Err("mmap accepted a byte offset off a page boundary");
    }

    // mprotect: a zero length succeeds before anything else is looked at,
    // PROT_SEM is accepted, a range with nothing mapped is ENOMEM, and the
    // growth flags are EINVAL on a region that does not grow.
    let answers = [
        (memory::sys_mprotect(process, at, 0, 0x40), Ok(0)),
        (
            memory::sys_mprotect(process, at, PAGE_SIZE, PROT_READ | PROT_SEM),
            Ok(0),
        ),
        (
            memory::sys_mprotect(process, TEST_BASE, PAGE_SIZE, PROT_READ),
            Err(Errno::ENOMEM),
        ),
        (
            memory::sys_mprotect(process, at, PAGE_SIZE, PROT_READ | PROT_GROWSDOWN),
            Err(Errno::EINVAL),
        ),
        (
            memory::sys_mprotect(process, at, PAGE_SIZE, PROT_GROWSDOWN | PROT_GROWSUP),
            Err(Errno::EINVAL),
        ),
    ];
    let _ = memory::sys_munmap(process, at, PAGE_SIZE);
    if answers.iter().any(|(got, want)| got != want) {
        return Err("mprotect did not answer as Linux does");
    }

    // gettimeofday writes a zeroed timezone, with or without a timeval.
    let page = map_rw(process, PAGE_SIZE)?;
    uaccess::copy_to_user(process.space(), page, &[0xFF; 8])
        .map_err(|_| "could not fill the timezone")?;
    let _ =
        time::sys_gettimeofday(process, 0, page).map_err(|_| "gettimeofday refused a timezone")?;
    let mut tz = [0xFF_u8; 8];
    uaccess::copy_from_user(process.space(), page, &mut tz)
        .map_err(|_| "could not read the timezone back")?;
    let told = time::sys_clock_gettime(process, u64::from(CLOCK_TAI), page, time::TimeWidth::Wide);
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    if tz != [0; 8] {
        return Err("gettimeofday did not write a zeroed timezone");
    }
    if told.is_err() {
        return Err("clock_gettime refused CLOCK_TAI");
    }
    check_the_cpu_time_clocks(process)
}

/// `CLOCK_THREAD_CPUTIME_ID` counts the time this thread runs, charged up to
/// the instant it is read, and never faster than the wall clock; and
/// `CLOCK_PROCESS_CPUTIME_ID` answers. Chrome's `ThreadTicks` asserts the
/// first succeeds, and ended its renderer when it was `EINVAL`.
fn check_the_cpu_time_clocks(process: &Process) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID};
    const MILLI: u64 = 1_000_000;
    let page = map_rw(process, PAGE_SIZE)?;
    let read = |clock: u32| -> Result<u64, &'static str> {
        let _ = time::sys_clock_gettime(process, u64::from(clock), page, time::TimeWidth::Native)
            .map_err(|_| "clock_gettime refused a CPU-time clock")?;
        let pair = read_user::<16>(process, page)?;
        let word = size_of::<usize>();
        Ok(le_at(&pair, 0, word)
            .saturating_mul(1_000_000_000)
            .saturating_add(le_at(&pair, word, word)))
    };
    let outcome = (|| {
        let wall_start = crate::timer::now_nanos();
        let start = read(CLOCK_THREAD_CPUTIME_ID)?;
        // Spin until the thread has run a millisecond by its own clock, or a
        // second has passed on the wall's: a clock that never moves fails
        // there rather than hanging the boot.
        let mut now = start;
        while now.saturating_sub(start) < MILLI {
            if crate::timer::now_nanos().saturating_sub(wall_start) > 1_000 * MILLI {
                return Err("CLOCK_THREAD_CPUTIME_ID did not advance while the thread ran");
            }
            core::hint::spin_loop();
            now = read(CLOCK_THREAD_CPUTIME_ID)?;
        }
        let wall = crate::timer::now_nanos().saturating_sub(wall_start);
        if now.saturating_sub(start) > wall {
            return Err("CLOCK_THREAD_CPUTIME_ID ran faster than the wall clock");
        }
        let _ = read(CLOCK_PROCESS_CPUTIME_ID)?;
        Ok(())
    })();
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome
}

/// Nothing lands below `MMAP_MIN_ADDR`: a fixed request there is `EPERM`, as on
/// Linux, and a hint there is moved above it rather than rounded down to zero.
///
/// With no SMAP or PAN, a page mapped at zero is what a kernel null
/// dereference would read.
///
/// Verifies: L.user.45
fn check_nothing_is_mapped_near_page_zero(process: &Process) -> Result<(), &'static str> {
    let request = |addr: u64, flags: u32| MmapRequest {
        addr,
        len: PAGE_SIZE,
        prot: PROT_READ | PROT_WRITE,
        flags: MAP_ANONYMOUS | MAP_PRIVATE | flags,
        fd: -1,
        offset: 0,
        unit: OffsetUnit::Bytes,
    };
    for addr in [0, PAGE_SIZE, MMAP_MIN_ADDR - PAGE_SIZE] {
        for fixed in [MAP_FIXED, MAP_FIXED_NOREPLACE] {
            if memory::sys_mmap(process, &request(addr, fixed)) != Err(Errno::EPERM) {
                return Err("a fixed mapping below mmap_min_addr was not refused with EPERM");
            }
        }
    }

    // A hint of 0x10 used to round down to page zero.
    let at = memory::sys_mmap(process, &request(0x10, 0))
        .map_err(|_| "a mapping hinted near page zero was refused")?;
    let at = u64::try_from(at).map_err(|_| "mmap returned an impossible address")?;
    let _ = memory::sys_munmap(process, at, PAGE_SIZE);
    if at < MMAP_MIN_ADDR {
        return Err("a hint near page zero placed a mapping below mmap_min_addr");
    }

    // The floor holds whichever call asks, not only `mmap`.
    if process
        .space()
        .map_anonymous(0, PAGE_SIZE, ferrix_vma::VmaFlags::READ_WRITE)
        .is_ok()
    {
        return Err("the address space mapped page zero");
    }

    // And the floor itself is mappable: a static ARM binary is linked there.
    let at = memory::sys_mmap(process, &request(MMAP_MIN_ADDR, MAP_FIXED))
        .map_err(|_| "a fixed mapping at mmap_min_addr was refused")?;
    let _ = memory::sys_munmap(process, MMAP_MIN_ADDR, PAGE_SIZE);
    if u64::try_from(at).unwrap_or(0) != MMAP_MIN_ADDR {
        return Err("a fixed mapping at mmap_min_addr landed elsewhere");
    }
    Ok(())
}

/// `MAP_FIXED` puts the mapping exactly where it was told.
fn check_fixed_mapping_lands_where_asked(process: &Process) -> Result<(), &'static str> {
    let at = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: TEST_BASE,
            len: PAGE_SIZE,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE | MAP_FIXED,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "a fixed mapping was refused")?;
    if u64::try_from(at).unwrap_or(0) != TEST_BASE {
        return Err("MAP_FIXED did not map where it was asked to");
    }
    let _ = memory::sys_munmap(process, TEST_BASE, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// A copy spanning two pages copies both halves, which a one-page-at-a-time
/// walk gets wrong by exactly one page if the chunking is off.
fn check_copy_crosses_a_page_boundary(process: &Process) -> Result<(), &'static str> {
    let len = PAGE_SIZE * 2;
    let at = map_rw(process, len)?;
    // Straddle the boundary: eight bytes before it and eight after.
    let straddling = at + PAGE_SIZE - 8;
    let written = *b"ABCDEFGHIJKLMNOP";
    uaccess::copy_to_user(process.space(), straddling, &written)
        .map_err(|_| "a straddling write failed")?;
    let mut read = [0_u8; 16];
    uaccess::copy_from_user(process.space(), straddling, &mut read)
        .map_err(|_| "a straddling read failed")?;
    if read != written {
        return Err("a copy across a page boundary lost or moved bytes");
    }
    let _ = memory::sys_munmap(process, at, len).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// A user pointer naming a kernel address is refused before it is followed.
///
/// The one check here that is a security property rather than a correctness
/// one. Nothing in the hardware enforces it on this tree -- no SMAP, no PAN --
/// so this bound is the only thing between a program's pointer and a read of
/// the kernel at kernel privilege.
fn check_a_user_pointer_into_the_kernel_is_refused(process: &Process) -> Result<(), &'static str> {
    let mut out = [0_u8; 8];
    if uaccess::copy_from_user(process.space(), KERNEL_HALF_BASE, &mut out).is_ok() {
        return Err("a user pointer into the kernel half was followed");
    }
    // And a length that would carry a legal start address into the kernel.
    let at = map_rw(process, PAGE_SIZE)?;
    let mut huge = [0_u8; 8];
    if uaccess::copy_from_user(process.space(), u64::MAX - 3, &mut huge).is_ok() {
        return Err("a range that wraps the address space was accepted");
    }
    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// An address in no region is `EFAULT` to the program, not a kernel fault.
fn check_an_unmapped_address_is_efault_not_a_kernel_fault(
    process: &Process,
) -> Result<(), &'static str> {
    let mut out = [0_u8; 4];
    // A user address that is legal and mapped by nothing.
    if uaccess::copy_from_user(process.space(), TEST_BASE + 0x10_0000, &mut out).is_ok() {
        return Err("reading an unmapped user address succeeded");
    }
    Ok(())
}

/// `copy_cstr_from_user` stops at the NUL, and refuses a string without one.
fn check_a_c_string_stops_at_its_nul(process: &Process) -> Result<(), &'static str> {
    let at = map_rw(process, PAGE_SIZE)?;
    uaccess::copy_to_user(process.space(), at, b"/bin/sh\0and then some")
        .map_err(|_| "could not stage a string")?;

    let mut out = Vec::new();
    uaccess::copy_cstr_from_user(process.space(), at, 64, &mut out)
        .map_err(|_| "reading a C string failed")?;
    if out.as_slice() != b"/bin/sh" {
        return Err("a C string did not stop at its terminator");
    }

    // A limit shorter than the string is EFAULT, not a truncated answer: a
    // path silently cut in half is a file operation on the wrong file.
    let mut short = Vec::new();
    if uaccess::copy_cstr_from_user(process.space(), at, 3, &mut short).is_ok() {
        return Err("an over-long C string was truncated instead of refused");
    }
    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// `mprotect` to read-only stops a copy into the region, and takes the old
/// writable translation down.
///
/// **Two separate properties, and only one of them is about permissions.**
///
/// The copy is refused because [`AddressSpace::fault`] checks the region's
/// flags before it does anything else, and every `copy_to_user` goes through
/// it. That is worth asserting, but note what it does *not* prove: the kernel
/// reaches the page through the direct map, which is writable for all of RAM,
/// so no user page-table permission bit is ever consulted on this path. A
/// `mprotect` that updated the map and left the tables alone would pass a
/// check that only tried to copy.
///
/// So the second half asks the tables directly. After `mprotect` the leaf must
/// be gone, because the region's permissions changed and the translation
/// carrying the old ones is still writable in hardware until something removes
/// it. Nothing can observe that from user mode yet -- there is no user mode --
/// which is exactly why it is worth checking from here.
///
/// Verifies: L.user.86
fn check_mprotect_takes_write_away(process: &Process) -> Result<(), &'static str> {
    let at = map_rw(process, PAGE_SIZE)?;
    uaccess::copy_to_user(process.space(), at, b"before")
        .map_err(|_| "could not write before mprotect")?;

    let root = process.space().root_table();
    if mm::translate_in(root, at).is_none() {
        return Err("the page was not mapped after being written through");
    }

    let _ = memory::sys_mprotect(process, at, PAGE_SIZE, PROT_READ)
        .map_err(|_| "mprotect to read-only was refused")?;

    if mm::translate_in(root, at).is_some() {
        return Err("mprotect left the old translation in the page tables");
    }
    if uaccess::copy_to_user(process.space(), at, b"after").is_ok() {
        return Err("a read-only region was still writable");
    }
    // Reading still works, and still sees what was there.
    let mut out = [0_u8; 6];
    uaccess::copy_from_user(process.space(), at, &mut out)
        .map_err(|_| "a read-only region was not readable")?;
    if &out != b"before" {
        return Err("mprotect lost the contents of the page");
    }

    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// `mremap` grows a mapping that has a neighbour in the way by moving it, and
/// the contents go with it; shrinks one where it is; moves one to a fixed
/// address; and refuses what it should.
///
/// The contents are the point. glibc's `realloc` hands a large block to
/// `mremap` and carries on using it, so a move that arrived with the right
/// length and the wrong bytes -- zeros, or the pages of another object -- is
/// a corrupted heap that no call ever reports. The string written across the
/// page boundary is there so a move that got one page right and the other
/// wrong, or both in the wrong order, is caught too.
///
/// Verifies: L.user.30, L.user.73
fn check_mremap_moves_the_contents_and_shrinks_in_place(
    process: &Process,
) -> Result<(), &'static str> {
    const FIRST: &[u8] = b"the first page";
    const ACROSS: &[u8] = b"across the page boundary";
    let space = process.space();

    // Two writable pages, and a third made read-only so it is a separate
    // region in the way: growing the two cannot happen where they are.
    let at = map_rw(process, PAGE_SIZE * 3)?;
    uaccess::copy_to_user(space, at, FIRST).map_err(|_| "could not write before mremap")?;
    let across = at + PAGE_SIZE - 8;
    uaccess::copy_to_user(space, across, ACROSS).map_err(|_| "could not write before mremap")?;
    let _ = memory::sys_mprotect(process, at + PAGE_SIZE * 2, PAGE_SIZE, PROT_READ)
        .map_err(|_| "mprotect of the page in the way was refused")?;

    refuses(
        memory::sys_mremap(process, at, PAGE_SIZE * 2, PAGE_SIZE * 4, 0, 0),
        Errno::ENOMEM,
        "mremap without MREMAP_MAYMOVE grew over the mapping in its way",
    )?;
    refuses(
        memory::sys_mremap(
            process,
            at,
            PAGE_SIZE * 2,
            PAGE_SIZE * 4,
            MREMAP_FIXED,
            at + PAGE_SIZE * 8,
        ),
        Errno::EINVAL,
        "mremap accepted MREMAP_FIXED without MREMAP_MAYMOVE",
    )?;
    refuses(
        memory::sys_mremap(process, at + 1, PAGE_SIZE, PAGE_SIZE * 2, MREMAP_MAYMOVE, 0),
        Errno::EINVAL,
        "mremap accepted an unaligned old address",
    )?;

    let moved = memory::sys_mremap(process, at, PAGE_SIZE * 2, PAGE_SIZE * 4, MREMAP_MAYMOVE, 0)
        .map_err(|_| "mremap with MREMAP_MAYMOVE refused to grow a mapping")?;
    let moved = u64::try_from(moved).map_err(|_| "mremap returned an impossible address")?;
    if moved == at {
        return Err("mremap grew a mapping over its neighbour rather than moving it");
    }
    let mut first = [0_u8; FIRST.len()];
    let mut straddle = [0_u8; ACROSS.len()];
    uaccess::copy_from_user(space, moved, &mut first)
        .map_err(|_| "the moved mapping was not readable")?;
    uaccess::copy_from_user(space, moved + PAGE_SIZE - 8, &mut straddle)
        .map_err(|_| "the moved mapping was not readable across its first page")?;
    if first != FIRST || straddle != ACROSS {
        return Err("the contents of a mapping did not survive mremap moving it");
    }
    let mut byte = [0xFF_u8; 1];
    uaccess::copy_from_user(space, at, &mut byte).map_or(Ok(()), |()| {
        Err("the old address still read after mremap moved it")
    })?;
    uaccess::copy_from_user(space, moved + PAGE_SIZE * 3, &mut byte)
        .map_err(|_| "the grown part of a moved mapping was not readable")?;
    if byte != [0] {
        return Err("the grown part of a moved mapping was not zero");
    }

    answers(
        memory::sys_mremap(process, moved, PAGE_SIZE * 4, PAGE_SIZE, 0, 0),
        usize::try_from(moved).unwrap_or(0),
        "mremap did not shrink a mapping where it was",
    )?;
    uaccess::copy_from_user(space, moved, &mut first)
        .map_err(|_| "a shrunk mapping lost the page it kept")?;
    if first != FIRST {
        return Err("a shrunk mapping lost the contents of the page it kept");
    }
    uaccess::copy_from_user(space, moved + PAGE_SIZE, &mut byte).map_or(Ok(()), |()| {
        Err("the tail mremap shrank away was still readable")
    })?;

    // And to a fixed address: the old one, which the move left free.
    answers(
        memory::sys_mremap(
            process,
            moved,
            PAGE_SIZE,
            PAGE_SIZE,
            MREMAP_MAYMOVE | MREMAP_FIXED,
            at,
        ),
        usize::try_from(at).unwrap_or(0),
        "mremap with MREMAP_FIXED did not land where it was told",
    )?;
    uaccess::copy_from_user(space, at, &mut first)
        .map_err(|_| "a mapping moved to a fixed address was not readable")?;
    if first != FIRST {
        return Err("the contents did not survive mremap moving to a fixed address");
    }

    let _ = memory::sys_munmap(process, at, PAGE_SIZE * 3).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// `mremap` to a fixed address below `MMAP_MIN_ADDR` is `EPERM`, as `mmap` is
/// -- but only once the call is otherwise sound, because Linux refuses it late.
///
/// `check_mremap_params` in `mm/mremap.c` answers `EINVAL` for a destination
/// that overlaps the old range, then the old mapping is looked up (`EFAULT`),
/// and only then does `get_unmapped_area` reach `security_mmap_addr` and
/// `EPERM`. A refused call leaves the old mapping where it was.
fn check_mremap_below_mmap_min_addr_is_eperm(process: &Process) -> Result<(), &'static str> {
    const MOVE_TO: u32 = MREMAP_MAYMOVE | MREMAP_FIXED;
    const MARK: &[u8] = b"still here";
    let space = process.space();
    let at = map_rw(process, PAGE_SIZE * 2)?;
    uaccess::copy_to_user(space, at, MARK).map_err(|_| "could not write before mremap")?;
    let _ =
        memory::sys_munmap(process, at + PAGE_SIZE, PAGE_SIZE).map_err(|_| "munmap was refused")?;

    for target in [0, PAGE_SIZE, MMAP_MIN_ADDR - PAGE_SIZE] {
        refuses(
            memory::sys_mremap(process, at, PAGE_SIZE, PAGE_SIZE, MOVE_TO, target),
            Errno::EPERM,
            "mremap to a fixed address below mmap_min_addr was not refused with EPERM",
        )?;
    }
    // Growing across the floor is the same address, so the same answer.
    refuses(
        memory::sys_mremap(
            process,
            at,
            PAGE_SIZE,
            PAGE_SIZE * 2,
            MOVE_TO,
            MMAP_MIN_ADDR - PAGE_SIZE,
        ),
        Errno::EPERM,
        "mremap growing across mmap_min_addr was not refused with EPERM",
    )?;
    // An old range that is not mapped is found out first.
    refuses(
        memory::sys_mremap(process, at + PAGE_SIZE, PAGE_SIZE, PAGE_SIZE, MOVE_TO, 0),
        Errno::EFAULT,
        "mremap below mmap_min_addr from an unmapped range was not EFAULT",
    )?;
    // And a destination reaching over the old range before that.
    refuses(
        memory::sys_mremap(process, at, PAGE_SIZE, at + PAGE_SIZE, MOVE_TO, 0),
        Errno::EINVAL,
        "mremap below mmap_min_addr over its own old range was not EINVAL",
    )?;

    let mut mark = [0_u8; MARK.len()];
    uaccess::copy_from_user(space, at, &mut mark)
        .map_err(|_| "a refused mremap below mmap_min_addr unmapped the old range")?;
    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    if mark != MARK {
        return Err("a refused mremap below mmap_min_addr changed the old range");
    }
    Ok(())
}

/// `brk` moves the break, reports it, and never reports an error.
fn check_brk_grows_and_shrinks(process: &Process) -> Result<(), &'static str> {
    let start = memory::sys_brk(process, 0).map_err(|_| "brk(0) failed")?;
    let start = u64::try_from(start).map_err(|_| "brk returned an impossible address")?;
    if start == 0 {
        return Err("brk(0) reported no heap at all");
    }

    let want = start + 8192;
    let grown = memory::sys_brk(process, want).map_err(|_| "growing the break failed")?;
    if u64::try_from(grown).unwrap_or(0) != want {
        return Err("brk did not grow to where it was asked");
    }

    // The new heap is usable.
    uaccess::copy_to_user(process.space(), start, b"heap")
        .map_err(|_| "the grown heap was not writable")?;

    let shrunk = memory::sys_brk(process, start).map_err(|_| "shrinking the break failed")?;
    if u64::try_from(shrunk).unwrap_or(0) != start {
        return Err("brk did not shrink back");
    }

    // A request below the start is refused *by reporting the current break*,
    // which is the convention: brk has no error channel, and a libc that got
    // a negative number back would read it as an enormous valid heap.
    let refused = memory::sys_brk(process, 1).map_err(|_| "brk(1) failed")?;
    if u64::try_from(refused).unwrap_or(0) != start {
        return Err("a refused brk did not report the unchanged break");
    }
    Ok(())
}

/// `set_tid_address` answers with a thread id, not with zero.
///
/// musl uses the return value as its process id during startup, so this is one
/// of the few calls where a plausible-looking stub is worse than an error.
fn check_set_tid_address_answers_with_a_thread_id(
    process: &Arc<Process>,
) -> Result<(), &'static str> {
    let at = map_rw(process, PAGE_SIZE)?;
    let thread = crate::syscall::thread::Thread::leader(process)
        .map_err(|_| "no memory for a check's thread")?;
    let tid = thread.set_clear_child_tid(at);
    if tid == 0 {
        return Err("set_tid_address reported thread zero");
    }
    if tid != process.pid() {
        return Err("a process's first thread was not numbered by its pid");
    }
    if thread.clear_child_tid() != at {
        return Err("set_tid_address did not record the address");
    }
    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// A robust futex list head is its thread's, as on Linux: a C library
/// registers one for each thread and clears it as the thread unmaps its
/// stack, so one thread clearing its own must leave another's, and a fork
/// child starts with none. Kept per process, a thread's exit forgot every
/// other thread's head, and Chromium's `ForkWithFlags`, which reads it back
/// with `get_robust_list`, stopped the Steam client's browser helper.
fn check_a_robust_list_is_each_threads_own(process: &Arc<Process>) -> Result<(), &'static str> {
    use crate::syscall::thread::Thread;
    const NO_MEMORY: &str = "no memory for a check's thread";
    let first = Thread::leader(process).map_err(|_| NO_MEMORY)?;
    let second = Thread::sibling(process, process.pid() + 1, &first).map_err(|_| NO_MEMORY)?;
    if first.robust_list() != 0 || second.robust_list() != 0 {
        return Err("a new thread started with a robust list");
    }
    first.set_robust_list(0x1000);
    second.set_robust_list(0x2000);
    if first.robust_list() != 0x1000 || second.robust_list() != 0x2000 {
        return Err("two threads' robust lists were not each their own");
    }
    second.set_robust_list(0);
    if first.robust_list() != 0x1000 {
        return Err("a thread clearing its robust list cleared another's");
    }
    let child = Thread::forked(process, &first).map_err(|_| NO_MEMORY)?;
    if child.robust_list() != 0 {
        return Err("a fork child's thread inherited a robust list");
    }
    Ok(())
}

/// `PR_GET_DUMPABLE`, `PR_SET_DUMPABLE` and `PR_GET_NO_NEW_PRIVS`, from
/// `linux/prctl.h`, for the checks that read them back.
const PR_GET_DUMPABLE: i32 = 3;
/// See [`PR_GET_DUMPABLE`].
const PR_SET_DUMPABLE: i32 = 4;
/// See [`PR_GET_DUMPABLE`].
const PR_SET_NO_NEW_PRIVS: i32 = 38;
/// See [`PR_GET_DUMPABLE`].
const PR_GET_NO_NEW_PRIVS: i32 = 39;

/// A fork child of a process that set `PR_SET_NO_NEW_PRIVS` reads it as 1,
/// and so does the child's own child, as on Linux, where the flag is copied
/// with the task and can never be cleared. Before, the child started from the
/// defaults and could take the privilege of a set-user-id program its parent
/// had given up; `cargo xtask test-vfs`'s permissions row shows that end of
/// it, a set-user-id program run by such a child. A child forked before the
/// parent set it keeps reading 0 -- the control, that a fork copies and does
/// not share. Dumpability, which Linux keeps in the memory descriptor and
/// copies with it, crosses the fork the same way.
fn check_no_new_privs_crosses_a_fork() -> Result<(), &'static str> {
    use crate::syscall::attributes::sys_prctl;
    let parent = process::new_for_check()
        .map_err(|_| "could not make a process for the no-new-privs check")?;
    let fork = |from: &Arc<Process>| {
        process::fork_for_check(from).map_err(|_| "could not fork for the no-new-privs check")
    };
    let before = fork(&parent)?;
    answers(
        sys_prctl(&parent, PR_SET_NO_NEW_PRIVS, [1, 0, 0, 0]),
        0,
        "PR_SET_NO_NEW_PRIVS was refused",
    )?;
    answers(
        sys_prctl(&parent, PR_SET_DUMPABLE, [0, 0, 0, 0]),
        0,
        "PR_SET_DUMPABLE 0 was refused",
    )?;
    let child = fork(&parent)?;
    let grandchild = fork(&child)?;
    answers(
        sys_prctl(&child, PR_GET_NO_NEW_PRIVS, [0; 4]),
        1,
        "a fork child of a no-new-privs process did not read PR_GET_NO_NEW_PRIVS as 1",
    )?;
    answers(
        sys_prctl(&grandchild, PR_GET_NO_NEW_PRIVS, [0; 4]),
        1,
        "the fork child of a no-new-privs process's child did not read PR_GET_NO_NEW_PRIVS as 1",
    )?;
    answers(
        sys_prctl(&child, PR_GET_DUMPABLE, [0; 4]),
        0,
        "a fork child of a process that is not dumpable was dumpable",
    )?;
    answers(
        sys_prctl(&before, PR_GET_NO_NEW_PRIVS, [0; 4]),
        0,
        "a child forked before its parent set no-new-privs read it as set",
    )?;
    answers(
        sys_prctl(&before, PR_GET_DUMPABLE, [0; 4]),
        1,
        "a child forked before its parent stopped being dumpable is not dumpable",
    )
}

/// A change of the ids a process acts as ends its dumpability and its
/// parent-death signal, as Linux's `commit_creds` ends them -- the effective
/// uid, and the filesystem gid on its own -- and a call that asks for the ids
/// the process already has ends neither.
fn check_a_change_of_ids_ends_dumpability() -> Result<(), &'static str> {
    use crate::syscall::attributes::{get, sys_prctl, update};
    use ferrix_linux_abi::nr::Syscall as Call;
    let unchanged = u64::from(u32::MAX);
    let process = process::new_for_check()
        .map_err(|_| "could not make a process for the dumpability check")?;
    let dumpable = |want: usize, what: &'static str| {
        answers(sys_prctl(&process, PR_GET_DUMPABLE, [0; 4]), want, what)
    };
    let make_dumpable = || {
        answers(
            sys_prctl(&process, PR_SET_DUMPABLE, [1, 0, 0, 0]),
            0,
            "PR_SET_DUMPABLE 1 was refused",
        )
    };

    expect_credential(
        &process,
        Call::Setuid,
        [0, 0, 0],
        Ok(0),
        "root could not setuid(0)",
    )?;
    dumpable(
        1,
        "setuid to the uid a process already has ended its dumpability",
    )?;
    expect_credential(
        &process,
        Call::Setfsgid,
        [7, 0, 0],
        Ok(0),
        "setfsgid did not answer the old filesystem gid",
    )?;
    dumpable(
        0,
        "a process that moved its filesystem gid is still dumpable",
    )?;
    make_dumpable()?;
    update(&process, |set| set.parent_death_signal = 9);
    expect_credential(
        &process,
        Call::Setresuid,
        [unchanged, 1000, unchanged],
        Ok(0),
        "root could not move its effective uid to 1000",
    )?;
    dumpable(
        0,
        "a process that moved its effective uid is still dumpable",
    )?;
    if get(&process).parent_death_signal != 0 {
        return Err("a process that moved its effective uid kept its parent-death signal");
    }
    Ok(())
}

/// A native child, made by `process_create` through the personality's
/// `LoadNative` ([`crate::syscall::launch::load_native`]), of a process that
/// set `PR_SET_NO_NEW_PRIVS` reads it as 1 and is not dumpable when its
/// creator is not. A native process reaches the Linux table for any number
/// outside the native range, so a child without the flag could `execve` a
/// set-user-id file. A child made before the creator set either reads the
/// defaults -- the control, that the child copies and does not share.
///
/// The child's `prctl` is the Linux handler called for the child, not one the
/// child's own program makes: a native program that makes a Linux call is a
/// user program of its own to build, and the handler is what that call
/// reaches, through the same table.
fn check_no_new_privs_reaches_a_native_child() -> Result<(), &'static str> {
    use crate::object::process::{Host, downcast};
    use crate::syscall::attributes::sys_prctl;
    let creator = process::new_for_check()
        .map_err(|_| "could not make a process for the native no-new-privs check")?;
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_ARGUMENT_PROGRAM,
    );
    let create = |name: &'static [u8]| {
        crate::syscall::launch::load_native(Some(&*creator as &dyn Host), &file, name)
            .ok()
            .and_then(downcast::<Process>)
            .ok_or("a native child could not be made for the no-new-privs check")
    };
    let before = create(b"/nnp-before")?;
    answers(
        sys_prctl(&creator, PR_SET_NO_NEW_PRIVS, [1, 0, 0, 0]),
        0,
        "PR_SET_NO_NEW_PRIVS was refused",
    )?;
    answers(
        sys_prctl(&creator, PR_SET_DUMPABLE, [0, 0, 0, 0]),
        0,
        "PR_SET_DUMPABLE 0 was refused",
    )?;
    let child = create(b"/nnp-child")?;
    let outcome = answers(
        sys_prctl(&child, PR_GET_NO_NEW_PRIVS, [0; 4]),
        1,
        "a native child of a no-new-privs process did not read PR_GET_NO_NEW_PRIVS as 1",
    )
    .and_then(|()| {
        answers(
            sys_prctl(&child, PR_GET_DUMPABLE, [0; 4]),
            0,
            "a native child of a process that is not dumpable was dumpable",
        )
    })
    .and_then(|()| {
        answers(
            sys_prctl(&before, PR_GET_NO_NEW_PRIVS, [0; 4]),
            0,
            "a native child made before its creator set no-new-privs read it as set",
        )
    });
    // Never started: ended here, as the other checks end theirs.
    process::kill(&child, 137);
    process::kill(&before, 137);
    outcome
}

/// `uname` fills all six fields, NUL-terminates each within its 65 bytes, and
/// names the system `Ferrix` and the host `ferrix`, with a Linux release and
/// machine name.
///
/// The buffer is poisoned first. The structure is fixed-width and a reader
/// stops at the first NUL, so a handler that wrote the strings and left the
/// padding alone would pass a check on an all-zero page and hand a real
/// program whatever the page held before -- which is usually zero and
/// occasionally not.
fn check_uname_names_the_system(process: &Process) -> Result<(), &'static str> {
    const FIELD: usize = 65;
    const SIZE: usize = FIELD * 6;

    let at = map_rw(process, PAGE_SIZE)?;
    uaccess::copy_to_user(process.space(), at, &[0xAA_u8; SIZE])
        .map_err(|_| "could not poison the utsname buffer")?;
    if system::sys_uname(process, at) != Ok(0) {
        return Err("uname was refused");
    }
    let mut out = [0_u8; SIZE];
    uaccess::copy_from_user(process.space(), at, &mut out)
        .map_err(|_| "could not read utsname back")?;

    // Every field is a string, and everything after its NUL is NUL too.
    let mut fields: [&[u8]; 6] = [&[]; 6];
    for (slot, field) in out.chunks(FIELD).zip(fields.iter_mut()) {
        let end = slot
            .iter()
            .position(|&byte| byte == 0)
            .ok_or("a utsname field is not NUL-terminated")?;
        let (text, padding) = slot
            .split_at_checked(end)
            .ok_or("a utsname field is not NUL-terminated")?;
        if text.is_empty() {
            return Err("a utsname field is empty");
        }
        if padding.iter().any(|&byte| byte != 0) {
            return Err("a utsname field is not NUL-padded to its full width");
        }
        *field = text;
    }
    let [sysname, nodename, release, _version, machine, _domainname] = fields;

    if sysname != b"Ferrix" {
        return Err("uname did not name the system Ferrix");
    }
    if nodename != b"ferrix" {
        return Err("uname did not say ferrix where a person looks");
    }
    if !release.ends_with(b"-ferrix") {
        return Err("the release does not carry the Ferrix suffix");
    }
    // The machine name is decided by the build, and a 32-bit one must not
    // claim to be a 64-bit machine: glibc's loader and every `configure`
    // script size the world by this field.
    let class = match machine {
        b"x86_64" | b"aarch64" => Class::Elf64,
        b"armv7l" => Class::Elf32,
        _ => return Err("the machine name is not one Linux uses"),
    };
    if class != class_of_this_build() {
        return Err("the machine name does not match the build's word size");
    }

    // A pointer into the kernel is EFAULT, not a write.
    if system::sys_uname(process, KERNEL_HALF_BASE) != Err(Errno::EFAULT) {
        return Err("uname into a kernel address was not EFAULT");
    }

    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// `rt_sigaction` hands back, as `oldact`, exactly what the previous call set
/// -- which is all a program starting up can see of signals today.
///
/// Read back through a second call rather than from the table, because the
/// layout is the thing most likely to be wrong: three native words and an
/// 8-byte mask, which is 32 bytes on 64-bit machines and 20 on ARMv7-A. A
/// handler that wrote the 64-bit layout on a 32-bit build would put the flags
/// where the program reads the restorer, and a check against the table would
/// still pass.
fn check_a_signal_disposition_reads_back_as_it_was_set(
    process: &Process,
) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{SA_RESTART, SA_RESTORER, SIGINT, SIGKILL, SIGUSR1};

    let word = size_of::<usize>();
    let size = word * 3 + 8;
    let at = map_rw(process, PAGE_SIZE)?;
    let old = at + PAGE_SIZE / 2;
    let set = signal::SIGSET_SIZE;

    // The action to install, in this architecture's own layout.
    let mut act = [0_u8; 32];
    let fields = [0x0001_2340_u64, SA_RESTORER | SA_RESTART, 0x0005_6780];
    for (slot, value) in act.chunks_mut(word).zip(fields) {
        slot.copy_from_slice(value.to_le_bytes().get(..word).ok_or("impossible word")?);
    }
    let mask = (1_u64 << (SIGUSR1 - 1)) | (1_u64 << (SIGKILL - 1));
    act.get_mut(word * 3..size)
        .ok_or("impossible sigaction size")?
        .copy_from_slice(&mask.to_le_bytes());
    let act_bytes = act.get(..size).ok_or("impossible sigaction size")?;
    uaccess::copy_to_user(process.space(), at, act_bytes)
        .map_err(|_| "could not stage a sigaction")?;

    // The first call reports the default, into a poisoned buffer.
    uaccess::copy_to_user(process.space(), old, &[0xAA_u8; 32])
        .map_err(|_| "could not poison oldact")?;
    if signal::sys_rt_sigaction(process, SIGINT, at, old, set, crate::trap::Abi::Native) != Ok(0) {
        return Err("rt_sigaction refused a valid handler");
    }
    let mut back = [0_u8; 32];
    uaccess::copy_from_user(process.space(), old, &mut back)
        .map_err(|_| "could not read oldact")?;
    let (first, tail) = back
        .split_at_checked(size)
        .ok_or("impossible sigaction size")?;
    if first.iter().any(|&byte| byte != 0) {
        return Err("the first oldact was not the zeroed default");
    }
    if tail.iter().any(|&byte| byte != 0xAA) {
        return Err("rt_sigaction wrote past the end of this architecture's sigaction");
    }

    // The second reports the first, with SIGKILL taken out of the mask.
    if signal::sys_rt_sigaction(process, SIGINT, 0, old, set, crate::trap::Abi::Native) != Ok(0) {
        return Err("rt_sigaction refused a query");
    }
    uaccess::copy_from_user(process.space(), old, &mut back)
        .map_err(|_| "could not read oldact")?;
    let mut expected = act;
    expected
        .get_mut(word * 3..size)
        .ok_or("impossible sigaction size")?
        .copy_from_slice(&(1_u64 << (SIGUSR1 - 1)).to_le_bytes());
    if back.get(..size) != expected.get(..size) {
        return Err("oldact was not the action set before it, field for field");
    }

    // What Linux refuses.
    if signal::sys_rt_sigaction(process, SIGINT, at, 0, 16, crate::trap::Abi::Native)
        != Err(Errno::EINVAL)
    {
        return Err("a sigsetsize other than 8 was accepted");
    }
    if signal::sys_rt_sigaction(process, 0, 0, old, set, crate::trap::Abi::Native)
        != Err(Errno::EINVAL)
    {
        return Err("signal 0 was accepted");
    }
    if signal::sys_rt_sigaction(process, 65, 0, old, set, crate::trap::Abi::Native)
        != Err(Errno::EINVAL)
    {
        return Err("signal 65 was accepted");
    }
    if signal::sys_rt_sigaction(process, SIGKILL, at, 0, set, crate::trap::Abi::Native)
        != Err(Errno::EINVAL)
    {
        return Err("a handler for SIGKILL was accepted");
    }
    if signal::sys_rt_sigaction(process, SIGKILL, 0, old, set, crate::trap::Abi::Native) != Ok(0) {
        return Err("asking what SIGKILL does was refused");
    }
    if signal::sys_rt_sigaction(
        process,
        SIGINT,
        KERNEL_HALF_BASE,
        0,
        set,
        crate::trap::Abi::Native,
    ) != Err(Errno::EFAULT)
    {
        return Err("an action at a kernel address was not EFAULT");
    }

    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// A thread of `process`, for a check that calls a handler acting on a thread's
/// own signal state as a program's thread would. The check's process runs no
/// thread of its own, so one is made for it, found again by its pid for the
/// shared reference a thread holds.
fn check_thread(process: &Process) -> Result<crate::syscall::thread::Thread, &'static str> {
    let process = crate::syscall::registry::find(process.pid())
        .ok_or("a check's process was not findable by its pid")?;
    crate::syscall::thread::Thread::leader(&process).map_err(|_| "no memory for a check's thread")
}

/// `rt_sigprocmask` applies `how`, never blocks SIGKILL, and leaves `oldset`
/// untouched when it refuses.
fn check_the_blocked_mask_follows_how(process: &Process) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK, SIGINT, SIGKILL, SIGUSR1};

    let thread = check_thread(process)?;
    let at = map_rw(process, PAGE_SIZE)?;
    let old = at + 8;
    let set = signal::SIGSET_SIZE;
    let usr1 = 1_u64 << (SIGUSR1 - 1);
    let int = 1_u64 << (SIGINT - 1);
    let kill = 1_u64 << (SIGKILL - 1);

    let read_old = || -> Result<u64, &'static str> {
        let mut bytes = [0_u8; 8];
        uaccess::copy_from_user(process.space(), old, &mut bytes)
            .map_err(|_| "could not read oldset")?;
        Ok(u64::from_le_bytes(bytes))
    };
    let stage = |value: u64| {
        uaccess::copy_to_user(process.space(), at, &value.to_le_bytes())
            .map_err(|_| "could not stage a set")
    };

    stage(usr1 | kill)?;
    if signal::sys_rt_sigprocmask(&thread, SIG_BLOCK, at, old, set) != Ok(0) {
        return Err("SIG_BLOCK was refused");
    }
    stage(int)?;
    if signal::sys_rt_sigprocmask(&thread, SIG_BLOCK, at, old, set) != Ok(0) || read_old()? != usr1
    {
        return Err("the mask after blocking SIGUSR1 and SIGKILL was not SIGUSR1 alone");
    }
    stage(usr1)?;
    if signal::sys_rt_sigprocmask(&thread, SIG_UNBLOCK, at, old, set) != Ok(0)
        || read_old()? != usr1 | int
    {
        return Err("SIG_BLOCK did not add to the mask");
    }
    stage(0)?;
    if signal::sys_rt_sigprocmask(&thread, SIG_SETMASK, at, old, set) != Ok(0) || read_old()? != int
    {
        return Err("SIG_UNBLOCK did not take away from the mask");
    }

    // A refused `how` writes nothing; with no set it is not even looked at.
    uaccess::copy_to_user(process.space(), old, &[0xAA_u8; 8])
        .map_err(|_| "could not poison oldset")?;
    if signal::sys_rt_sigprocmask(&thread, 7, at, old, set) != Err(Errno::EINVAL) {
        return Err("a nonsense how was accepted");
    }
    if read_old()? != u64::from_le_bytes([0xAA; 8]) {
        return Err("a refused sigprocmask still wrote oldset");
    }
    if signal::sys_rt_sigprocmask(&thread, 7, 0, old, set) != Ok(0) || read_old()? != 0 {
        return Err("a query with a nonsense how was refused, or misreported the mask");
    }
    if signal::sys_rt_sigprocmask(&thread, SIG_BLOCK, at, old, 4) != Err(Errno::EINVAL) {
        return Err("a sigsetsize other than 8 was accepted");
    }

    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// `sigaltstack` records a stack big enough, refuses one too small, and
/// reports `SS_DISABLE` when none is installed.
fn check_an_alternate_stack_is_recorded_and_refused_when_small(
    process: &Process,
) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::SS_DISABLE;

    let word = size_of::<usize>();
    let size = word * 3;
    let at = map_rw(process, PAGE_SIZE)?;
    let old = at + PAGE_SIZE / 2;

    let stage = |sp: u64, flags: i32, bytes: u64| {
        let mut raw = [0_u8; 24];
        let _ = raw
            .get_mut(..word)
            .map(|slot| slot.copy_from_slice(sp.to_le_bytes().get(..word).unwrap_or(&[])));
        let _ = raw
            .get_mut(word..word + 4)
            .map(|slot| slot.copy_from_slice(&flags.to_le_bytes()));
        let _ = raw
            .get_mut(word * 2..size)
            .map(|slot| slot.copy_from_slice(bytes.to_le_bytes().get(..word).unwrap_or(&[])));
        uaccess::copy_to_user(process.space(), at, raw.get(..size).unwrap_or(&[]))
            .map_err(|_| "could not stage a stack_t")
    };
    let read_old = || -> Result<(u64, i32, u64), &'static str> {
        let mut raw = [0_u8; 24];
        let bytes = raw.get_mut(..size).ok_or("impossible stack_t size")?;
        uaccess::copy_from_user(process.space(), old, bytes).map_err(|_| "could not read old")?;
        let mut sp = [0_u8; 8];
        let mut flags = [0_u8; 4];
        let mut length = [0_u8; 8];
        sp.get_mut(..word)
            .ok_or("impossible word")?
            .copy_from_slice(bytes.get(..word).ok_or("impossible word")?);
        flags.copy_from_slice(bytes.get(word..word + 4).ok_or("impossible word")?);
        length
            .get_mut(..word)
            .ok_or("impossible word")?
            .copy_from_slice(bytes.get(word * 2..size).ok_or("impossible word")?);
        Ok((
            u64::from_le_bytes(sp),
            i32::from_le_bytes(flags),
            u64::from_le_bytes(length),
        ))
    };

    let thread = check_thread(process)?;
    if signal::sys_sigaltstack(&thread, 0, old, 0, crate::trap::Abi::Native) != Ok(0)
        || read_old()? != (0, SS_DISABLE, 0)
    {
        return Err("with no alternate stack, sigaltstack did not report SS_DISABLE");
    }
    stage(0x0001_0000, 0, 1024)?;
    if signal::sys_sigaltstack(&thread, at, 0, 0, crate::trap::Abi::Native) != Err(Errno::ENOMEM) {
        return Err("an alternate stack below MINSIGSTKSZ was accepted");
    }
    stage(0x0001_0000, 5, 65536)?;
    if signal::sys_sigaltstack(&thread, at, 0, 0, crate::trap::Abi::Native) != Err(Errno::EINVAL) {
        return Err("a nonsense ss_flags was accepted");
    }
    stage(0x0001_0000, 0, 65536)?;
    if signal::sys_sigaltstack(&thread, at, 0, 0, crate::trap::Abi::Native) != Ok(0) {
        return Err("a valid alternate stack was refused");
    }
    if signal::sys_sigaltstack(&thread, 0, old, 0, crate::trap::Abi::Native) != Ok(0)
        || read_old()? != (0x0001_0000, 0, 65536)
    {
        return Err("the installed alternate stack did not read back");
    }
    stage(0, SS_DISABLE, 0)?;
    if signal::sys_sigaltstack(&thread, at, old, 0, crate::trap::Abi::Native) != Ok(0)
        || read_old()? != (0x0001_0000, 0, 65536)
    {
        return Err("disabling did not report the stack it replaced");
    }

    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// `poll` answers each descriptor for what it is: the console is writable, a
/// descriptor that names nothing is `POLLNVAL`, and a negative one is left
/// alone -- and with nothing ready it waits out its timeout rather than
/// returning at once.
///
/// Busybox's `while read` asks `poll` about its file before every read, and a
/// refused `poll` makes it read nothing, which is how this call was found to be
/// missing.
fn check_poll_reports_ready_invalid_and_skipped(process: &Process) -> Result<(), &'static str> {
    use crate::syscall::poll::{self, POLLIN, POLLNVAL, POLLOUT, POLLWRNORM};

    let at = map_rw(process, PAGE_SIZE)?;
    let entry = |fd: i32, events: u16| {
        let mut bytes = [0_u8; 8];
        let fields = fd.to_le_bytes().into_iter().chain(events.to_le_bytes());
        for (slot, byte) in bytes.iter_mut().zip(fields) {
            *slot = byte;
        }
        bytes
    };
    let mut array = [0_u8; 24];
    let three = entry(1, POLLOUT | POLLIN)
        .into_iter()
        .chain(entry(4000, POLLIN))
        .chain(entry(-1, POLLIN));
    for (slot, byte) in array.iter_mut().zip(three) {
        *slot = byte;
    }
    uaccess::copy_to_user(process.space(), at, &array)
        .map_err(|_| "could not stage a pollfd array")?;

    if poll::sys_poll(process, at, 3, 0) != Ok(2) {
        return Err("poll did not count exactly the console and the closed descriptor");
    }
    let mut back = [0_u8; 24];
    uaccess::copy_from_user(process.space(), at, &mut back)
        .map_err(|_| "could not read revents")?;
    let revents = |at: usize| {
        u16::from_le_bytes([
            back.get(at + 6).copied().unwrap_or(0xFF),
            back.get(at + 7).copied().unwrap_or(0xFF),
        ])
    };
    // Only what was asked for comes back, plus hang-ups and errors: the
    // console was asked about `POLLIN|POLLOUT`, so `POLLWRNORM` must not appear
    // even though the console is writable.
    if revents(0) & POLLOUT == 0 {
        return Err("poll did not report the console writable");
    }
    if revents(0) & POLLWRNORM != 0 {
        return Err("poll reported an event that was not asked for");
    }
    if revents(8) != POLLNVAL {
        return Err("poll did not answer POLLNVAL for a descriptor that names nothing");
    }
    if revents(16) != 0 {
        return Err("poll touched a negative descriptor it should have skipped");
    }

    // Nothing ready: only the skipped slot. The call must take its timeout.
    uaccess::copy_to_user(process.space(), at, &entry(-1, POLLIN))
        .map_err(|_| "could not stage a pollfd")?;
    let before = crate::timer::now_nanos();
    if poll::sys_poll(process, at, 1, 10) != Ok(0) {
        return Err("poll with nothing ready did not time out with zero");
    }
    if crate::timer::now_nanos().saturating_sub(before) < 10_000_000 {
        return Err("poll with nothing ready returned before its timeout");
    }

    let thread = check_thread(process)?;
    if poll::sys_ppoll(&thread, at, 1, 0, at, 16, time::TimeWidth::Native) != Err(Errno::EINVAL) {
        return Err("ppoll accepted a signal set of the wrong size");
    }
    // On a 32-bit build a 64-bit `tv_nsec` is its low half, as Linux's
    // `get_timespec64` has it: the upper half is padding a libc need not
    // write, and ferrousli does not, so curl's every poll on ARMv7-A failed.
    if size_of::<usize>() == 4 {
        let mut timeout = [0_u8; 16];
        let fields = 0_u64
            .to_le_bytes()
            .into_iter()
            .chain(1_000_000_u32.to_le_bytes())
            .chain(0xDEAD_BEEF_u32.to_le_bytes());
        for (slot, byte) in timeout.iter_mut().zip(fields) {
            *slot = byte;
        }
        let tmo = at + POLL_TIMEOUT;
        uaccess::copy_to_user(process.space(), tmo, &timeout)
            .map_err(|_| "could not stage a timespec")?;
        if poll::sys_ppoll(&thread, at, 1, tmo, 0, 8, time::TimeWidth::Wide) != Ok(0) {
            return Err("ppoll refused a 64-bit timeout for the padding above its tv_nsec");
        }
    }
    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// Where on its page the `poll` check stages a `ppoll` timeout.
const POLL_TIMEOUT: u64 = 256;

/// Where on its page the `select` check stages each argument.
const SELECT_WRITE: u64 = 256;
/// See [`SELECT_WRITE`].
const SELECT_EXCEPT: u64 = 512;
/// See [`SELECT_WRITE`].
const SELECT_TIME: u64 = 768;
/// See [`SELECT_WRITE`].
const SELECT_MASK: u64 = 800;
/// See [`SELECT_WRITE`].
const SELECT_PACK: u64 = 816;
/// See [`SELECT_WRITE`].
const SELECT_OLD_MASK: u64 = 840;

/// `select` and `pselect6` answer from the same readiness as `poll`, as
/// bitmaps: the console's descriptors are writable and never exceptional, a
/// set bit on a closed descriptor is `EBADF`, bits past `nfds` are neither
/// asked about nor left set, a timeout is waited out and written back, and
/// `pselect6`'s signal mask is checked for size and put back afterwards.
fn check_select_answers_with_the_sets_that_are_ready(
    process: &Process,
) -> Result<(), &'static str> {
    let page = map_rw(process, PAGE_SIZE)?;
    let outcome = select_answers(process, page).and_then(|()| pselect_answers(process, page));
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome
}

/// See [`check_select_answers_with_the_sets_that_are_ready`].
fn select_answers(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::poll;

    let stage = |offset: u64, bytes: &[u8]| {
        uaccess::copy_to_user(process.space(), page + offset, bytes)
            .map_err(|_| "could not stage a select argument")
    };
    let byte_at = |offset: u64| {
        let mut byte = [0_u8; 1];
        uaccess::copy_from_user(process.space(), page + offset, &mut byte)
            .map_err(|_| "could not read a select answer back")?;
        Ok::<u8, &'static str>(u8::from_le_bytes(byte))
    };
    let word = size_of::<usize>();
    let (write_set, except_set) = (page + SELECT_WRITE, page + SELECT_EXCEPT);

    // Descriptors 1 and 2 asked about for writing, 1 for exceptions, with a
    // zero timeout: two bits come back, and the exception set comes back
    // empty.
    stage(SELECT_WRITE, &[0b110])?;
    stage(SELECT_EXCEPT, &[0b010])?;
    stage(SELECT_TIME, &word_pair(0, 0))?;
    if poll::sys_select(process, 3, [0, write_set, except_set], page + SELECT_TIME) != Ok(2) {
        return Err("select did not count the console's two writable descriptors");
    }
    if byte_at(SELECT_WRITE)? != 0b110 || byte_at(SELECT_EXCEPT)? != 0 {
        return Err("select did not write back exactly the bits that were ready");
    }

    // A bit past `nfds` inside the last word read is not asked about, and is
    // cleared in the answer.
    stage(SELECT_WRITE, &[0b010, 0, 0b1_0000])?;
    if poll::sys_select(process, 2, [0, write_set, 0], page + SELECT_TIME) != Ok(1) {
        return Err("select looked at a descriptor past nfds");
    }
    if byte_at(SELECT_WRITE + 2)? != 0 {
        return Err("select left a bit set past nfds");
    }

    // Descriptor 40 is not open.
    stage(SELECT_WRITE, &[0b010, 0, 0, 0, 0, 0b1])?;
    if poll::sys_select(process, 41, [0, write_set, 0], page + SELECT_TIME) != Err(Errno::EBADF) {
        return Err("select did not refuse a closed descriptor with EBADF");
    }
    if poll::sys_select(process, -1, [0, 0, 0], 0) != Err(Errno::EINVAL) {
        return Err("select accepted a negative nfds");
    }
    stage(SELECT_TIME, &word_pair(0, usize::MAX))?;
    if poll::sys_select(process, 0, [0, 0, 0], page + SELECT_TIME) != Err(Errno::EINVAL) {
        return Err("select accepted a negative microsecond count");
    }

    // Nothing asked: the timeout is waited out, and the time left, none, is
    // written back.
    stage(SELECT_TIME, &word_pair(0, 10_000))?;
    let before = crate::timer::now_nanos();
    if poll::sys_select(process, 0, [0, 0, 0], page + SELECT_TIME) != Ok(0) {
        return Err("select with nothing to wait for did not time out with zero");
    }
    if crate::timer::now_nanos().saturating_sub(before) < 10_000_000 {
        return Err("select returned before its timeout");
    }
    let mut left = [0_u8; 16];
    uaccess::copy_from_user(process.space(), page + SELECT_TIME, &mut left)
        .map_err(|_| "could not read the time left back")?;
    if left.iter().take(word * 2).any(|&byte| byte != 0) {
        return Err("select did not write back that no time was left");
    }

    Ok(())
}

/// See [`check_select_answers_with_the_sets_that_are_ready`]: `pselect6`'s
/// mask.
fn pselect_answers(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::poll;
    use crate::syscall::time::TimeWidth;

    let stage = |offset: u64, bytes: &[u8]| {
        uaccess::copy_to_user(process.space(), page + offset, bytes)
            .map_err(|_| "could not stage a pselect6 argument")
    };
    let write_set = page + SELECT_WRITE;
    // `pselect6`'s mask: a set of the wrong size is refused, a null set's size
    // is not looked at, and a mask waited under is put back.
    let mask = 1_u64 << 9;
    stage(SELECT_MASK, &mask.to_le_bytes())?;
    stage(SELECT_WRITE, &[0b110])?;
    stage(SELECT_TIME, &word_pair(0, 0))?;
    let thread = check_thread(process)?;
    let pselect = |nfds: i32, set: usize, size: usize| {
        let pack = word_pair(set, size);
        stage(SELECT_PACK, &pack)?;
        Ok::<_, &'static str>(poll::sys_pselect6(
            &thread,
            nfds,
            [0, write_set, 0],
            page + SELECT_TIME,
            page + SELECT_PACK,
            TimeWidth::Native,
            size_of::<usize>(),
        ))
    };
    let mask_at = usize::try_from(page + SELECT_MASK).map_err(|_| "an impossible address")?;
    if pselect(3, mask_at, 16)? != Err(Errno::EINVAL) {
        return Err("pselect6 accepted a signal set of the wrong size");
    }
    if pselect(3, 0, 16)? != Ok(2) {
        return Err("pselect6 looked at the size of a signal set it was not given");
    }
    let blocked = |process: &Process| {
        let _ = signal::sys_rt_sigprocmask(&thread, 0, 0, page + SELECT_OLD_MASK, 8)
            .map_err(|_| "rt_sigprocmask could not report the mask")?;
        let mut bytes = [0_u8; 8];
        uaccess::copy_from_user(process.space(), page + SELECT_OLD_MASK, &mut bytes)
            .map_err(|_| "could not read the mask back")?;
        Ok::<u64, &'static str>(u64::from_le_bytes(bytes))
    };
    let before = blocked(process)?;
    stage(SELECT_WRITE, &[0b110])?;
    if pselect(3, mask_at, 8)? != Ok(2) {
        return Err("pselect6 refused a well-formed signal mask");
    }
    if blocked(process)? != before {
        return Err("pselect6 did not put the caller's signal mask back");
    }
    Ok(())
}

/// Two native words, little-endian, in the first bytes of sixteen: a
/// `timeval`, a native `timespec`, or `pselect6`'s set-and-size pair.
fn word_pair(first: usize, second: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    for (slot, byte) in bytes
        .iter_mut()
        .zip(first.to_le_bytes().into_iter().chain(second.to_le_bytes()))
    {
        *slot = byte;
    }
    bytes
}

/// The line discipline, driven directly: the default settings edit and echo a
/// line as the console always has, end of file and a line without a newline
/// both come through `VEOF`, raw mode neither waits for a line nor echoes, and
/// the interrupt character is a signal that takes the half-typed line with it.
fn check_the_line_discipline_follows_its_settings() -> Result<(), &'static str> {
    use crate::fs::terminal::{Discipline, Termios};
    use ferrix_linux_abi::types::{ECHO, ICANON, SIGINT};

    let mut discipline = Discipline::new();
    let mut echo = Vec::new();
    let mut buf = [0_u8; 8];
    let feed = |discipline: &mut Discipline, bytes: &[u8], echo: &mut Vec<u8>| {
        bytes
            .iter()
            .filter_map(|&byte| discipline.receive(byte, echo))
            .last()
    };

    if feed(&mut discipline, b"ab\x7fc\r", &mut echo).is_some() {
        return Err("an ordinary keystroke raised a signal");
    }
    if echo != b"ab\x08 \x08c\n" {
        return Err("the default settings did not echo and erase as the console always has");
    }
    if discipline.take(&mut buf) != Some(3) || buf.get(..3) != Some(b"ac\n".as_slice()) {
        return Err("a canonical line did not read back as it was edited");
    }
    let _ = feed(&mut discipline, b"x", &mut echo);
    if discipline.readable() {
        return Err("half a line was readable in canonical mode");
    }
    let _ = feed(&mut discipline, b"\x04", &mut echo);
    if discipline.take(&mut buf) != Some(1) || buf.first() != Some(&b'x') {
        return Err("VEOF did not end a line without a newline");
    }
    let _ = feed(&mut discipline, b"\x04", &mut echo);
    if discipline.take(&mut buf) != Some(0) || discipline.take(&mut buf).is_some() {
        return Err("VEOF on an empty line was not one end of file");
    }

    let raw = Termios {
        lflag: Termios::DEFAULT.lflag & !(ICANON | ECHO),
        ..Termios::DEFAULT
    };
    discipline.set_termios(raw);
    echo.clear();
    let _ = feed(&mut discipline, b"q", &mut echo);
    if !echo.is_empty() {
        return Err("a keystroke was echoed with ECHO off");
    }
    if discipline.take(&mut buf) != Some(1) || buf.first() != Some(&b'q') {
        return Err("a keystroke in raw mode was not readable at once");
    }

    discipline.set_termios(Termios::DEFAULT);
    echo.clear();
    if feed(&mut discipline, b"z\x03", &mut echo) != Some(SIGINT) {
        return Err("the interrupt character did not raise SIGINT");
    }
    if echo != b"z^C" {
        return Err("the interrupt character was not echoed as ^C");
    }
    let _ = feed(&mut discipline, b"\r", &mut echo);
    if discipline.take(&mut buf) != Some(1) {
        return Err("the interrupt character did not discard the line being typed");
    }
    Ok(())
}

/// Raw settings with `VMIN` and `VTIME` as given, for the raw-read checks.
fn raw_termios(min: u8, time: u8) -> crate::fs::terminal::Termios {
    use crate::fs::terminal::Termios;
    use ferrix_linux_abi::types::{ECHO, ICANON, VMIN, VTIME};

    let mut termios = Termios {
        lflag: Termios::DEFAULT.lflag & !(ICANON | ECHO),
        ..Termios::DEFAULT
    };
    if let Some(slot) = termios.cc.get_mut(VMIN) {
        *slot = min;
    }
    if let Some(slot) = termios.cc.get_mut(VTIME) {
        *slot = time;
    }
    termios
}

/// `VTIME`'s unit, a tenth of a second, in nanoseconds.
const TENTH: u64 = 100_000_000;
/// Where the raw-read checks start their own counter.
const RAW_START: u64 = 1_000 * TENTH;

/// Put `bytes` into a discipline as though they were typed.
fn type_into(discipline: &mut crate::fs::terminal::Discipline, bytes: &[u8]) {
    let mut echo = Vec::new();
    for &byte in bytes {
        let _ = discipline.receive(byte, &mut echo);
    }
}

/// A raw read waits as `VMIN` and `VTIME` say, as Linux's `n_tty_read` does:
/// the decision, driven with a counter of the check's own, with `VMIN` zero.
/// [`check_a_raw_read_waits_for_vmin`] has the rest, and
/// [`check_a_pseudoterminal_slave_reads_as_vmin_and_vtime_say`] the slave
/// that decides the same way.
///
/// btop sets both to zero and reads until a read gives 0. A slave that
/// waited for input whatever they said gave it its key and then held it in
/// the next read for good: `q` did nothing, and a resize never redrew.
fn check_a_raw_read_waits_as_vmin_and_vtime_say() -> Result<(), &'static str> {
    use crate::fs::terminal::{Discipline, ReadStep, ReadTimer};

    let start = RAW_START;
    let mut buf = [0_u8; 8];
    let mut discipline = Discipline::new();

    // Both zero: what there is, and 0 at once when there is nothing.
    discipline.set_termios(raw_termios(0, 0));
    let mut timer = ReadTimer::new(start);
    if discipline.read_step(&mut buf, &mut timer, start, false) != ReadStep::Took(0) {
        return Err("VMIN 0 VTIME 0 with nothing typed did not read 0 at once");
    }
    type_into(&mut discipline, b"q");
    let mut timer = ReadTimer::new(start);
    if discipline.read_step(&mut buf, &mut timer, start, false) != ReadStep::Took(1)
        || buf.first() != Some(&b'q')
        || discipline.read_step(&mut buf, &mut timer, start, false) != ReadStep::Took(0)
    {
        return Err("VMIN 0 VTIME 0 did not read the key and then 0");
    }
    if discipline.read_step(&mut buf, &mut timer, start, true) != ReadStep::Took(0) {
        return Err("VMIN 0 VTIME 0 with O_NONBLOCK was not 0, as n_tty_read's is");
    }

    // VTIME alone: from the read's start, the first byte or 0.
    discipline.set_termios(raw_termios(0, 1));
    let mut timer = ReadTimer::new(start);
    let waits = ReadStep::Wait(start + TENTH);
    if discipline.read_step(&mut buf, &mut timer, start, false) != waits
        || discipline.read_step(&mut buf, &mut timer, start + TENTH - 1, false) != waits
        || discipline.read_step(&mut buf, &mut timer, start + TENTH, false) != ReadStep::Took(0)
    {
        return Err("VMIN 0 VTIME 1 did not wait a tenth of a second from the start, then read 0");
    }
    let mut timer = ReadTimer::new(start);
    type_into(&mut discipline, b"a");
    if discipline.read_step(&mut buf, &mut timer, start + 1, false) != ReadStep::Took(1) {
        return Err("VMIN 0 VTIME 1 did not read a byte that came before the timer");
    }
    Ok(())
}

/// See [`check_a_raw_read_waits_as_vmin_and_vtime_say`]: `VMIN` alone, then
/// with `VTIME` -- an inter-byte timer that starts at the first byte and
/// not before, so that with nothing typed the read waits however long --
/// and canonical mode still a line.
fn check_a_raw_read_waits_for_vmin() -> Result<(), &'static str> {
    use crate::fs::terminal::{Discipline, ReadStep, ReadTimer, Termios};

    let start = RAW_START;
    let never = ReadStep::Wait(u64::MAX);
    let mut buf = [0_u8; 8];
    let mut discipline = Discipline::new();

    // VMIN alone: that many bytes, however long.
    discipline.set_termios(raw_termios(2, 0));
    let mut timer = ReadTimer::new(start);
    type_into(&mut discipline, b"b");
    if discipline.read_step(&mut buf, &mut timer, start + 100 * TENTH, false) != never {
        return Err("VMIN 2 VTIME 0 returned with one byte of two");
    }
    if discipline.read_step(&mut buf, &mut timer, start, true) != ReadStep::Took(1) {
        return Err("VMIN 2 with O_NONBLOCK did not take the byte there was");
    }
    let mut timer = ReadTimer::new(start);
    type_into(&mut discipline, b"cd");
    if discipline.read_step(&mut buf, &mut timer, start, false) != ReadStep::Took(2) {
        return Err("VMIN 2 VTIME 0 did not read two bytes once they came");
    }

    // Both: the timer starts at the first byte, again at each, not before.
    discipline.set_termios(raw_termios(3, 1));
    let mut timer = ReadTimer::new(start);
    let first = start + 100 * TENTH;
    if discipline.read_step(&mut buf, &mut timer, start, false) != never
        || discipline.read_step(&mut buf, &mut timer, first, false) != never
    {
        return Err("VMIN 3 VTIME 1 timed out with nothing typed");
    }
    type_into(&mut discipline, b"e");
    if discipline.read_step(&mut buf, &mut timer, first, false) != ReadStep::Wait(first + TENTH) {
        return Err("VMIN 3 VTIME 1 did not start its timer at the first byte");
    }
    let second = first + TENTH * 6 / 10;
    type_into(&mut discipline, b"f");
    let later = ReadStep::Wait(second + TENTH);
    if discipline.read_step(&mut buf, &mut timer, second, false) != later
        || discipline.read_step(&mut buf, &mut timer, first + TENTH, false) != later
    {
        return Err("VMIN 3 VTIME 1 did not start its timer again at the second byte");
    }
    if discipline.read_step(&mut buf, &mut timer, second + TENTH, false) != ReadStep::Took(2) {
        return Err("VMIN 3 VTIME 1 did not read what came once the gap passed VTIME");
    }
    let mut timer = ReadTimer::new(start);
    type_into(&mut discipline, b"ghi");
    if discipline.read_step(&mut buf, &mut timer, start, false) != ReadStep::Took(3) {
        return Err("VMIN 3 VTIME 1 did not read three bytes once they came");
    }

    // Canonical mode is still a line.
    discipline.set_termios(Termios::DEFAULT);
    type_into(&mut discipline, b"x");
    let mut timer = ReadTimer::new(start);
    if discipline.read_step(&mut buf, &mut timer, first, false) != never {
        return Err("half a line was read in canonical mode");
    }
    Ok(())
}

/// A pseudoterminal pair's slave reads as `VMIN` and `VTIME` say: see
/// [`check_a_raw_read_waits_as_vmin_and_vtime_say`].
///
/// The reads that cannot wait come first. A slave that stopped honouring
/// `VMIN` again fails here by name, on a read with `O_NONBLOCK`, rather than
/// holding the boot in a read that never ends.
fn check_a_pseudoterminal_slave_reads_as_vmin_and_vtime_say() -> Result<(), &'static str> {
    use ferrix_vfs::Inode as _;

    let mut buf = [0_u8; 8];
    let master = crate::fs::pty::open_master().map_err(|_| "no pseudoterminal pair to check")?;
    crate::fs::pty::set_locked(&master.pty, false);
    let slave = crate::fs::pty::open_slave(master.pty.number)
        .map_err(|_| "the check's pseudoterminal slave did not open")?;
    master.pty.set_termios(raw_termios(0, 0), true);
    if slave.read_stream(&mut buf, true) != Ok(0) {
        return Err("a slave at VMIN 0 VTIME 0 with nothing typed did not read 0");
    }
    if master.write_stream(b"q", false) != Ok(1)
        || slave.read_stream(&mut buf, true) != Ok(1)
        || slave.read_stream(&mut buf, true) != Ok(0)
    {
        return Err("a slave at VMIN 0 VTIME 0 did not read the key and then 0");
    }
    if slave.read_stream(&mut buf, false) != Ok(0) {
        return Err("a slave's waiting read at VMIN 0 VTIME 0 did not read 0");
    }
    master.pty.set_termios(raw_termios(1, 0), true);
    if slave.read_stream(&mut buf, true) != Err(Errno::EAGAIN) {
        return Err("a slave at VMIN 1 with O_NONBLOCK and nothing typed was not EAGAIN");
    }
    // The waits: a tenth of a second, and under a second here, since a wait
    // is served by the timer and a loaded host may be late but not early.
    let waited = |slave: &crate::fs::pty::SlaveFile, buf: &mut [u8]| {
        let began = crate::timer::now_nanos();
        let read = slave.read_stream(buf, false);
        (read, crate::timer::now_nanos().saturating_sub(began))
    };
    master.pty.set_termios(raw_termios(0, 1), true);
    let (read, took) = waited(&slave, &mut buf);
    if read != Ok(0) || !(TENTH..10 * TENTH).contains(&took) {
        return Err("a slave at VMIN 0 VTIME 1 did not read 0 after a tenth of a second");
    }
    master.pty.set_termios(raw_termios(2, 1), true);
    let _ = master.write_stream(b"r", false);
    let (read, took) = waited(&slave, &mut buf);
    if read != Ok(1) || buf.first() != Some(&b'r') || !(TENTH..10 * TENTH).contains(&took) {
        return Err("a slave at VMIN 2 VTIME 1 did not read its one byte once VTIME passed");
    }
    Ok(())
}

/// Where on its page the terminal check stages each argument.
const TTY_TERMIOS: u64 = 0;
/// See [`TTY_TERMIOS`].
const TTY_INT: u64 = 64;
/// See [`TTY_TERMIOS`].
const TTY_PATH: u64 = 128;

/// The console answers the terminal requests an interactive shell makes:
/// settings that read back as they were set, a size, and job control for a
/// session leader -- and leaves the terminal as it found it.
fn check_the_console_answers_as_a_terminal(process: &Process) -> Result<(), &'static str> {
    use crate::fs::terminal;

    let page = map_rw(process, PAGE_SIZE)?;
    let saved = terminal::with(|terminal| {
        (
            terminal.discipline.termios(),
            terminal.winsize,
            terminal.session,
            terminal.foreground,
        )
    });
    let outcome = terminal_settings_answer(process, page)
        .and_then(|()| terminal_job_control_answers(process, page))
        .and_then(|()| terminal_answers_through_dev_tty(process, page));
    terminal::with(|terminal| {
        let (termios, winsize, session, foreground) = saved;
        terminal.discipline.set_termios(termios);
        terminal.winsize = winsize;
        terminal.session = session;
        terminal.foreground = foreground;
    });
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome
}

/// See [`check_the_console_answers_as_a_terminal`]: the same questions through
/// `/dev/tty`, a devfs node of its own that opens the console -- which is the
/// descriptor busybox's shell asks for the foreground group on, and which was
/// once refused because `fstat` names a different inode than the console.
fn terminal_answers_through_dev_tty(process: &Process, page: u64) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::TIOCGPGRP;

    uaccess::copy_to_user(process.space(), page + TTY_PATH, b"/dev/tty\0")
        .map_err(|_| "could not stage /dev/tty")?;
    let tty = fd::sys_openat(process, AT_FDCWD, page + TTY_PATH, O_RDWR, 0)
        .map_err(|_| "/dev/tty did not open")?;
    let tty = i32::try_from(tty).map_err(|_| "an impossible descriptor")?;
    let outcome = answers(
        fd::sys_ioctl(process, tty, TCGETS, page + TTY_TERMIOS),
        0,
        "TCGETS through /dev/tty was refused",
    )
    .and_then(|()| {
        answers(
            fd::sys_ioctl(process, tty, TIOCGPGRP, page + TTY_INT),
            0,
            "TIOCGPGRP through /dev/tty was refused to a session leader",
        )
    });
    let _ = fd::sys_close(process, tty);
    outcome
}

/// See [`check_the_console_answers_as_a_terminal`].
fn terminal_settings_answer(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::fs::terminal::{Termios, Winsize};
    use ferrix_linux_abi::types::{
        ECHO, ICANON, ICRNL, ISIG, ONLCR, OPOST, TCFLSH, TCSETSF, TCSETSW, TCXONC, TERMIOS_BYTES,
        VERASE, VMIN,
    };

    let read_termios = || {
        let mut bytes = [0_u8; TERMIOS_BYTES];
        uaccess::copy_from_user(process.space(), page + TTY_TERMIOS, &mut bytes)
            .map_err(|_| "could not read a termios back")?;
        Ok::<_, &'static str>(Termios::from_bytes(&bytes))
    };
    answers(
        fd::sys_ioctl(process, 0, TCGETS, page + TTY_TERMIOS),
        0,
        "TCGETS on the console was refused",
    )?;
    let settings = read_termios()?;
    let local = ICANON | ECHO | ISIG;
    if settings.lflag & local != local
        || settings.iflag & ICRNL == 0
        || settings.oflag & (OPOST | ONLCR) != OPOST | ONLCR
        || settings.cc(VMIN) != 1
        || settings.cc(VERASE) != 0x7F
    {
        return Err("the console's settings were not a canonical, echoing terminal's");
    }

    // Raw mode, as `sh -i` sets it, reads back as set; then the original.
    let raw = Termios {
        lflag: settings.lflag & !(ICANON | ECHO),
        ..settings
    };
    uaccess::copy_to_user(process.space(), page + TTY_TERMIOS, &raw.to_bytes())
        .map_err(|_| "could not stage a termios")?;
    answers(
        fd::sys_ioctl(process, 0, TCSETSW, page + TTY_TERMIOS),
        0,
        "TCSETSW on the console was refused",
    )?;
    answers(
        fd::sys_ioctl(process, 1, TCGETS, page + TTY_TERMIOS),
        0,
        "TCGETS on the console was refused",
    )?;
    if read_termios()? != raw {
        return Err("TCGETS did not report what TCSETSW set");
    }
    terminal_termios2_answers(process, page, settings, raw)?;
    uaccess::copy_to_user(process.space(), page + TTY_TERMIOS, &settings.to_bytes())
        .map_err(|_| "could not stage a termios")?;
    answers(
        fd::sys_ioctl(process, 0, TCSETSF, page + TTY_TERMIOS),
        0,
        "TCSETSF on the console was refused",
    )?;

    answers(
        fd::sys_ioctl(process, 1, TIOCGWINSZ, page + TTY_TERMIOS),
        0,
        "TIOCGWINSZ on the console was refused",
    )?;
    let mut size = [0_u8; 8];
    uaccess::copy_from_user(process.space(), page + TTY_TERMIOS, &mut size)
        .map_err(|_| "could not read a winsize back")?;
    if Winsize::from_bytes(size) != Winsize::DEFAULT {
        return Err("the console did not report 24 rows of 80 columns");
    }
    refuses(
        fd::sys_ioctl(process, 1, TIOCGWINSZ, 0),
        Errno::EFAULT,
        "a terminal's answer was written to address zero",
    )?;
    refuses(
        fd::sys_ioctl(process, 0, TCFLSH, 7),
        Errno::EINVAL,
        "TCFLSH accepted a queue that does not exist",
    )?;
    answers(
        fd::sys_ioctl(process, 0, TCXONC, 1),
        0,
        "TCXONC refused to restart output",
    )?;
    refuses(
        fd::sys_ioctl(process, 0, 0x5480, 0),
        Errno::ENOTTY,
        "an unknown terminal request was not ENOTTY",
    )
}

/// See [`check_the_console_answers_as_a_terminal`]: `TCGETS2` and `TCSETS2`,
/// which a newer glibc's `tcgetattr` and `tcsetattr` ask instead. Called with
/// the console in `raw` mode, and leaves it in `raw` mode.
fn terminal_termios2_answers(
    process: &Process,
    page: u64,
    settings: crate::fs::terminal::Termios,
    raw: crate::fs::terminal::Termios,
) -> Result<(), &'static str> {
    use crate::fs::terminal::Termios;
    use ferrix_linux_abi::types::{
        B115200, BOTHER, CBAUD, TCGETS2, TCSETS2, TCSETSF2, TERMIOS_BYTES, TERMIOS2_BYTES,
    };

    // A `struct termios2` of `termios`'s flags, with `BOTHER` and the speed
    // `rate` as a number where `termios` names a code.
    let stage = |termios: Termios, rate: u32| {
        let numbered = Termios {
            cflag: (termios.cflag & !CBAUD) | BOTHER,
            ..termios
        };
        let mut bytes = [0_u8; TERMIOS2_BYTES];
        let all = numbered
            .to_bytes()
            .into_iter()
            .chain(rate.to_le_bytes())
            .chain(rate.to_le_bytes());
        for (slot, byte) in bytes.iter_mut().zip(all) {
            *slot = byte;
        }
        uaccess::copy_to_user(process.space(), page + TTY_TERMIOS, &bytes)
            .map_err(|_| "could not stage a termios2")
    };
    let read_termios2 = || {
        let mut bytes = [0_u8; TERMIOS2_BYTES];
        uaccess::copy_from_user(process.space(), page + TTY_TERMIOS, &mut bytes)
            .map_err(|_| "could not read a termios2 back")?;
        Ok::<_, &'static str>(bytes)
    };
    let speeds = |bytes: &[u8; TERMIOS2_BYTES]| {
        let (_, tail) = bytes.split_first_chunk::<TERMIOS_BYTES>()?;
        let input = u32::from_le_bytes(*tail.first_chunk::<4>()?);
        let output = u32::from_le_bytes(*tail.last_chunk::<4>()?);
        Some((input, output))
    };

    // The same settings `TCGETS` reported, and the serial console's speed.
    answers(
        fd::sys_ioctl(process, 0, TCGETS2, page + TTY_TERMIOS),
        0,
        "TCGETS2 on the console was refused",
    )?;
    let got = read_termios2()?;
    if got.first_chunk::<TERMIOS_BYTES>() != Some(&raw.to_bytes()) {
        return Err("TCGETS2 did not report the settings TCGETS does");
    }
    if raw.cflag & CBAUD != B115200 || speeds(&got) != Some((115_200, 115_200)) {
        return Err("TCGETS2 did not report the console's 115200 baud");
    }

    // `TCSETS2` changes them: back to canonical mode, with the speed given
    // as a number, which settles on the code for it.
    stage(settings, 115_200)?;
    answers(
        fd::sys_ioctl(process, 0, TCSETS2, page + TTY_TERMIOS),
        0,
        "TCSETS2 on the console was refused",
    )?;
    answers(
        fd::sys_ioctl(process, 0, TCGETS, page + TTY_TERMIOS),
        0,
        "TCGETS on the console was refused",
    )?;
    let mut bytes = [0_u8; TERMIOS_BYTES];
    uaccess::copy_from_user(process.space(), page + TTY_TERMIOS, &mut bytes)
        .map_err(|_| "could not read a termios back")?;
    if Termios::from_bytes(&bytes) != settings {
        return Err("TCGETS did not report what TCSETS2 set");
    }

    // A speed no code names cannot be kept, and the console's stays.
    stage(raw, 12_345)?;
    answers(
        fd::sys_ioctl(process, 0, TCSETSF2, page + TTY_TERMIOS),
        0,
        "TCSETSF2 on the console was refused",
    )?;
    answers(
        fd::sys_ioctl(process, 0, TCGETS2, page + TTY_TERMIOS),
        0,
        "TCGETS2 on the console was refused",
    )?;
    let got = read_termios2()?;
    if got.first_chunk::<TERMIOS_BYTES>() != Some(&raw.to_bytes())
        || speeds(&got) != Some((115_200, 115_200))
    {
        return Err("TCSETSF2 at a speed no code names changed the console's speed");
    }
    Ok(())
}

/// See [`check_the_console_answers_as_a_terminal`].
fn terminal_job_control_answers(process: &Process, page: u64) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{FIONREAD, TIOCGPGRP, TIOCGSID, TIOCNOTTY, TIOCSCTTY, TIOCSPGRP};

    let at = page + TTY_INT;
    let read_int = || {
        let mut bytes = [0_u8; 4];
        uaccess::copy_from_user(process.space(), at, &mut bytes)
            .map_err(|_| "could not read an int back")?;
        Ok::<u32, &'static str>(u32::from_le_bytes(bytes))
    };
    let stage_int = |value: i32| {
        uaccess::copy_to_user(process.space(), at, &value.to_le_bytes())
            .map_err(|_| "could not stage an int")
    };

    // The check's process leads its own session, so the free console becomes
    // its controlling terminal when it asks.
    answers(
        fd::sys_ioctl(process, 0, TIOCGPGRP, at),
        0,
        "TIOCGPGRP was refused to a session leader",
    )?;
    if read_int()? != process.pgid() {
        return Err("the console's foreground group was not its session leader's");
    }
    answers(
        fd::sys_ioctl(process, 0, TIOCGSID, at),
        0,
        "TIOCGSID was refused",
    )?;
    if read_int()? != process.sid() {
        return Err("the console did not report its session");
    }
    answers(
        fd::sys_ioctl(process, 0, TIOCSCTTY, 0),
        0,
        "TIOCSCTTY was refused for the terminal the session already has",
    )?;
    stage_int(i32::try_from(process.pgid()).map_err(|_| "an impossible group")?)?;
    answers(
        fd::sys_ioctl(process, 0, TIOCSPGRP, at),
        0,
        "TIOCSPGRP was refused the caller's own group",
    )?;
    stage_int(-1)?;
    refuses(
        fd::sys_ioctl(process, 0, TIOCSPGRP, at),
        Errno::EINVAL,
        "TIOCSPGRP accepted a negative group",
    )?;
    stage_int(0x7FFF_FFF0)?;
    refuses(
        fd::sys_ioctl(process, 0, TIOCSPGRP, at),
        Errno::ESRCH,
        "TIOCSPGRP accepted a group nobody is in",
    )?;
    answers(
        fd::sys_ioctl(process, 0, FIONREAD, at),
        0,
        "FIONREAD was refused",
    )?;
    answers(
        fd::sys_ioctl(process, 0, TIOCNOTTY, 0),
        0,
        "TIOCNOTTY was refused to the session holding the terminal",
    )
}

/// Map `len` bytes of read/write shared anonymous memory, wherever it fits:
/// memory a `fork` of `process` sees as the same pages.
fn map_shared_rw(process: &Process, len: u64) -> Result<u64, &'static str> {
    let at = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_SHARED,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "a shared mapping was refused")?;
    u64::try_from(at).map_err(|_| "mmap returned an impossible address")
}

/// Map `len` bytes of read/write anonymous memory, wherever it fits.
fn map_rw(process: &Process, len: u64) -> Result<u64, &'static str> {
    let at = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "a working mapping was refused")?;
    u64::try_from(at).map_err(|_| "mmap returned an impossible address")
}

// ---------------------------------------------------------------------------
// The ELF loader
//
// `src/lib/platform/elf` parses and is fuzzed; what it cannot check is the half that
// touches memory. These build an image for whichever architecture is running,
// load it into a real address space, and then read the result back out of the
// page tables -- which is the only place the answer actually lives.
// ---------------------------------------------------------------------------

/// Load a synthetic image and check everything about where it landed.
fn check_an_image_loads_where_its_headers_say(process: &Process) -> Result<(), &'static str> {
    let class = class_of_this_build();
    let file = image::build(class, arch::ARCH.elf_machine(), image::Shape::Good);

    let loaded =
        load::load(process.space(), Bytes(&file), None).map_err(|_| "a good image was refused")?;

    if loaded.entry != image::ENTRY {
        return Err("the loader reported the wrong entry point");
    }
    // AT_PHDR must land inside the text segment, at the header offset the
    // image declares. musl reads its own PT_TLS through this, so a plausible
    // but wrong answer is worse than none.
    let expected_phdr = image::BASE + class.header_size() as u64;
    if loaded.phdr != expected_phdr {
        return Err("AT_PHDR does not point at the program headers");
    }
    if loaded.phnum != 2 {
        return Err("the loader miscounted the program headers");
    }

    // The file contents of the writable segment are where the headers said.
    let mut read = [0_u8; image::DATA_FILESZ];
    uaccess::copy_from_user(process.space(), image::DATA_VADDR, &mut read)
        .map_err(|_| "the loaded data segment was not readable")?;
    if read != image::DATA_MARK {
        return Err("the data segment's contents are not at its virtual address");
    }

    // And the `.bss` tail past `p_filesz` is zero, which it gets for free from
    // a committed anonymous page -- the loader must not have copied anything
    // over it, and must not have left it unmapped either.
    let mut tail = [0xFF_u8; 32];
    uaccess::copy_from_user(
        process.space(),
        image::DATA_VADDR + image::DATA_FILESZ as u64,
        &mut tail,
    )
    .map_err(|_| "the bss tail was not mapped")?;
    if tail.iter().any(|&b| b != 0) {
        return Err("the bss tail is not zero");
    }

    check_the_segments_got_their_own_permissions(process)?;

    let end = loaded.end;
    let _ = process.space().unmap(image::BASE, end - image::BASE);
    Ok(())
}

/// The text segment is executable and not writable; the data segment is the
/// other way round.
///
/// Asked of the address space rather than of the loader, because the loader
/// reporting what it meant to do proves nothing about what it did.
fn check_the_segments_got_their_own_permissions(process: &Process) -> Result<(), &'static str> {
    // Writing into the text segment must be refused: it is read-execute.
    if uaccess::copy_to_user(process.space(), image::BASE, b"x").is_ok() {
        return Err("the text segment was left writable");
    }
    // Reading it must work.
    let mut magic = [0_u8; 4];
    uaccess::copy_from_user(process.space(), image::BASE, &mut magic)
        .map_err(|_| "the text segment was not readable")?;
    if magic != [0x7F, b'E', b'L', b'F'] {
        return Err("the text segment does not hold the image it was loaded from");
    }
    // The data segment is writable.
    uaccess::copy_to_user(process.space(), image::DATA_VADDR, b"w")
        .map_err(|_| "the data segment was not writable")?;
    Ok(())
}

/// What a static PIE that was not moved fails with, which the negative
/// control requires by name.
const PIE_NOT_MOVED: &str =
    "a static PIE's entry point is not its link-time one moved to the PIE base";

/// A position-independent image with no interpreter -- the shape rustc gives a
/// musl program -- loads with every address it names moved to
/// [`load::PIE_BASE`]: the entry point, `AT_PHDR`, the end the heap starts at,
/// and the segments themselves, with nothing left at the link-time address.
/// And `execve`'s check accepts it, as the loader does.
fn check_a_static_pie_loads_at_its_base() -> Result<(), &'static str> {
    let class = class_of_this_build();
    let file = image::build(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::PositionIndependent,
    );
    if load::check(Bytes(&file), None).is_err() {
        return Err("execve's image check refused a static PIE");
    }
    let scratch = process::new_for_check().map_err(|_| "could not make a process")?;
    let space = scratch.space();
    let loaded =
        load::load(space, Bytes(&file), None).map_err(|_| "the loader refused a static PIE")?;
    let moved = |linked: u64| linked - image::BASE + load::PIE_BASE;

    if loaded.entry != moved(image::ENTRY) {
        return Err(PIE_NOT_MOVED);
    }
    if loaded.phdr != moved(image::BASE + class.header_size() as u64) {
        return Err("a static PIE's AT_PHDR is not its program headers' moved address");
    }
    if loaded.end <= moved(image::DATA_VADDR) {
        return Err("a static PIE's end is below its moved data segment");
    }
    let mut read = [0_u8; image::DATA_FILESZ];
    uaccess::copy_from_user(space, moved(image::DATA_VADDR), &mut read)
        .map_err(|_| "a static PIE's data segment is not at its moved address")?;
    if read != image::DATA_MARK {
        return Err("a static PIE's data segment does not hold its contents at the moved address");
    }
    let mut magic = [0_u8; 4];
    if uaccess::copy_from_user(space, image::BASE, &mut magic).is_ok() {
        return Err("a static PIE left something mapped at its link-time address");
    }
    Ok(())
}

/// A dynamically linked program, started from files, enters its linker and not
/// itself.
///
/// The numbers are checked next door in
/// [`check_a_dynamic_program_is_loaded_with_its_linker`]; this is the same
/// thing end to end, with a linker that is a real file read from a real path
/// and a processor that really enters ring 3. It is the only check that proves
/// the path in the program's `PT_INTERP` is the one opened.
///
/// The linker's payload is the program that prints a line and exits 42. The
/// *program's* payload writes through a null pointer, so a kernel that entered
/// the program rather than its linker does not quietly pass: it ends with a
/// `SIGSEGV`, or with 98 if the write did not even fault. Either is a failure
/// here, and 42 can only be reached through the linker.
fn check_a_dynamic_program_enters_its_linker() -> Result<(), &'static str> {
    if arch::USER_TEST_PROGRAM.is_empty() || arch::USER_FAULT_PROGRAM.is_empty() {
        return Ok(());
    }
    let class = class_of_this_build();
    let machine = arch::ARCH.elf_machine();
    let program = image::build_with(
        class,
        machine,
        image::Shape::Dynamic,
        arch::USER_FAULT_PROGRAM,
    );
    let linker = image::build_with(
        class,
        machine,
        image::Shape::PositionIndependent,
        arch::USER_TEST_PROGRAM,
    );

    // The path the image names, without the NUL a `PT_INTERP` carries.
    let path = image::INTERP_PATH
        .split_last()
        .map_or(&b""[..], |(_, rest)| rest);
    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let outcome = write_and_run(ns, &ctx, path, &program, &linker);
    let _ = ns.unlink(&ctx, None, path);
    outcome
}

/// [`check_a_dynamic_program_enters_its_linker`]'s middle, so that the file it
/// makes is removed whatever happens.
fn write_and_run(
    ns: &ferrix_vfs::Namespace,
    ctx: &ferrix_vfs::Context,
    path: &[u8],
    program: &[u8],
    linker: &[u8],
) -> Result<(), &'static str> {
    let flags = ferrix_vfs::OpenFlags {
        write: true,
        create: true,
        truncate: true,
        ..ferrix_vfs::OpenFlags::default()
    };
    let file = ns
        .open(ctx, None, path, &flags, 0o755)
        .map_err(|_| "the linker's file could not be created")?;
    if file
        .write(linker)
        .map_err(|_| "the linker would not write")?
        != linker.len()
    {
        return Err("the linker's file came back short");
    }
    drop(file);

    // Read back through the same path the kernel will use, which is what
    // `execve` does and what proves the `PT_INTERP` names something findable.
    let found = exec::linker_for(ctx, Bytes(program))
        .map_err(|_| "the linker named by PT_INTERP could not be read")?
        .ok_or("a program naming a linker was read as naming none")?;
    if found.len() != linker.len() as u64 {
        return Err("the linker read back from its path is not the one written");
    }

    let status = exec::run_with_linker(
        program,
        load::Source::File(&found),
        &[b"/dynamic"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a dynamically linked program could not be started")?;
    match status {
        arch::USER_TEST_STATUS => Ok(()),
        98 => Err("a dynamic program was entered itself, and its null write did not fault"),
        _ => Err("a dynamic program did not exit as its linker does"),
    }
}

/// A program naming a dynamic linker is loaded as two images, and it is the
/// linker the processor is given.
///
/// What the kernel owes a dynamically linked program is exactly this much:
/// both images placed, and the auxiliary vector telling the linker where each
/// one went. Resolving symbols is the linker's, so there is nothing else here
/// to check and nothing else the kernel should be doing.
///
/// Four numbers, and each would be wrong in a different way:
///
/// * `start` is the linker's entry, because entering the program directly
///   would run code whose every imported symbol is still zero.
/// * `entry` is the *program's* entry and not the linker's, because that is
///   what `AT_ENTRY` hands the linker to jump to when it has finished.
/// * `base` is where the linker landed, which is what `AT_BASE` tells it, and
///   without which it cannot find its own segments to relocate itself.
/// * `end` is past the *program*, because that is where the heap starts; a
///   heap placed past the linker would leave a hole, and one placed past
///   whichever image happened to be higher would move with the linker's size.
fn check_a_dynamic_program_is_loaded_with_its_linker() -> Result<(), &'static str> {
    let class = class_of_this_build();
    let machine = arch::ARCH.elf_machine();
    let program = image::build(class, machine, image::Shape::Dynamic);
    let linker = image::build(class, machine, image::Shape::PositionIndependent);

    if load::check(Bytes(&program), Some(Bytes(&linker))).is_err() {
        return Err("execve's image check refused a program with its linker");
    }
    let scratch = process::new_for_check().map_err(|_| "could not make a process")?;
    let space = scratch.space();
    let loaded = load::load(space, Bytes(&program), Some(Bytes(&linker)))
        .map_err(|_| "the loader refused a program with its linker")?;

    let in_program = |linked: u64| linked - image::BASE + load::PIE_BASE;
    let in_linker = |linked: u64| linked - image::BASE + load::INTERP_BASE;

    if loaded.start != in_linker(image::ENTRY) {
        return Err("a dynamic program does not start at its linker's entry point");
    }
    if loaded.entry != in_program(image::ENTRY) {
        return Err("a dynamic program's AT_ENTRY is not the program's own entry point");
    }
    if loaded.base != load::INTERP_BASE {
        return Err("a dynamic program's AT_BASE is not where the linker was placed");
    }
    if loaded.phdr != in_program(image::BASE + class.header_size() as u64) {
        return Err("a dynamic program's AT_PHDR is not the program's program headers");
    }
    if loaded.end <= in_program(image::DATA_VADDR) {
        return Err("a dynamic program's heap would start below its own data segment");
    }
    if loaded.end > load::PIE_BASE + (1 << 30) {
        return Err("a dynamic program's heap starts past the program, not past the linker");
    }

    // Both images are really there, each at its own base, with their data
    // segments holding their contents.
    for at in [in_program(image::DATA_VADDR), in_linker(image::DATA_VADDR)] {
        let mut read = [0_u8; image::DATA_FILESZ];
        uaccess::copy_from_user(space, at, &mut read)
            .map_err(|_| "an image of a dynamic program was not at the base it was given")?;
        if read != image::DATA_MARK {
            return Err("an image of a dynamic program does not hold its contents");
        }
    }
    Ok(())
}

/// The three ways a dynamic program and its linker can be refused, each by
/// name and before anything is entered.
fn check_the_loader_refuses_a_linker_it_cannot_use() -> Result<(), &'static str> {
    let class = class_of_this_build();
    let machine = arch::ARCH.elf_machine();
    let program = image::build(class, machine, image::Shape::Dynamic);

    // A program that names a linker, handed to a loader given none. This is
    // the state every caller but `execve` is in, and the answer it had before
    // any of this existed.
    let scratch = process::new_for_check().map_err(|_| "could not make a process")?;
    if !matches!(
        load::load(scratch.space(), Bytes(&program), None),
        Err(load::LoadError::NeedsInterpreter)
    ) {
        return Err("the loader entered a dynamic program with no linker");
    }

    // A linker linked to a fixed address: it could not be placed.
    let fixed = image::build(class, machine, image::Shape::Good);
    let scratch = process::new_for_check().map_err(|_| "could not make a process")?;
    if !matches!(
        load::load(scratch.space(), Bytes(&program), Some(Bytes(&fixed))),
        Err(load::LoadError::InterpreterNotPie)
    ) {
        return Err("the loader accepted a linker that is not position-independent");
    }

    // A linker that names a linker of its own. Linux refuses rather than
    // following the chain, because a chain has no end it can prove.
    let scratch = process::new_for_check().map_err(|_| "could not make a process")?;
    if !matches!(
        load::load(scratch.space(), Bytes(&program), Some(Bytes(&program))),
        Err(load::LoadError::InterpreterChain)
    ) {
        return Err("the loader followed a linker that names a linker of its own");
    }

    // And `execve`'s check refuses all three before its point of no return,
    // which is the only reason a failed `execve` can leave the caller running.
    for (linker, what) in [
        (None, "a dynamic program with no linker"),
        (Some(&fixed), "a linker that is not position-independent"),
        (Some(&program), "a linker that names one of its own"),
    ] {
        if load::check(
            Bytes(&program),
            linker.map(|linker| Bytes(linker.as_slice())),
        )
        .is_ok()
        {
            let _ = what;
            return Err("execve's image check accepted a linker the loader refuses");
        }
    }
    Ok(())
}

/// The images the loader must refuse, and refuse by name.
/// Verifies: `L.x86_64.74`
fn check_the_loader_refuses_what_it_cannot_run(process: &Process) -> Result<(), &'static str> {
    let class = class_of_this_build();
    let machine = arch::ARCH.elf_machine();

    let cases = [
        (
            image::Shape::ForeignMachine,
            "an image for another architecture",
        ),
        (
            image::Shape::WriteExecute,
            "an image needing a write-execute page",
        ),
    ];
    for (shape, _what) in cases {
        let file = image::build(class, machine, shape);
        // Each refusal gets a space of its own: a failed load may have mapped
        // part of the image, and that is exactly why `execve` will load into a
        // fresh space and swap it in only on success.
        let scratch = process::new_for_check().map_err(|_| "could not make a process")?;
        if load::load(scratch.space(), Bytes(&file), None).is_ok() {
            return Err("the loader accepted an image it cannot run");
        }
    }

    // And bytes that are not an ELF at all.
    let scratch = process::new_for_check().map_err(|_| "could not make a process")?;
    if load::load(scratch.space(), Bytes(b"not an ELF image"), None).is_ok() {
        return Err("the loader accepted something that is not an ELF image");
    }

    // An entry point past the user half, which x86-64's `sysretq` would fault
    // on in ring 0. Refused by name, and by `check` too, which is what
    // `execve` asks before its point of no return.
    let file = image::build(class, machine, image::Shape::EntryOutsideUser);
    let scratch = process::new_for_check().map_err(|_| "could not make a process")?;
    if !matches!(
        load::load(scratch.space(), Bytes(&file), None),
        Err(load::LoadError::EntryNotUser(_))
    ) {
        return Err("the loader accepted an entry point outside the user half");
    }
    if !matches!(
        load::check(Bytes(&file), None),
        Err(load::LoadError::EntryNotUser(_))
    ) {
        return Err("execve's image check accepted an entry point outside the user half");
    }
    let _ = process;
    Ok(())
}

/// The ELF class this kernel's own architecture uses.
///
/// From the pointer width rather than from a `cfg`, because generic kernel
/// code naming an architecture is what the layering check forbids -- and
/// because the question really is about width.
fn class_of_this_build() -> Class {
    if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    }
}

// ---------------------------------------------------------------------------
// `write` and `writev`
//
// The bytes really do reach the console, which is checked the only way it can
// be from in here: by writing a marker the boot test's own log will carry. If
// the line below this check's report is missing from the serial log, `write`
// did not work, whatever this file claims.
// ---------------------------------------------------------------------------

/// `write` copies from the program's memory and reports what it wrote.
fn check_write_reaches_the_console(process: &Process) -> Result<(), &'static str> {
    let at = map_rw(process, PAGE_SIZE)?;
    let message = b"  hello   from a user buffer, by way of write(2)\n";
    uaccess::copy_to_user(process.space(), at, message)
        .map_err(|_| "could not stage the message")?;

    let written = file::sys_write(process, 1, at, message.len() as u64)
        .map_err(|_| "write to fd 1 was refused")?;
    if written != message.len() {
        return Err("write reported the wrong count");
    }

    // A zero-length write is not a no-op: it still validates the descriptor.
    if file::sys_write(process, 1, at, 0) != Ok(0) {
        return Err("a zero-length write to a good descriptor was refused");
    }
    if file::sys_write(process, 7, at, 1) != Err(Errno::EBADF) {
        return Err("a descriptor that names nothing was not EBADF");
    }
    // And EBADF is decided before the buffer is looked at, so a bad descriptor
    // with a wild pointer is EBADF and not EFAULT.
    if file::sys_write(process, 7, KERNEL_HALF_BASE, 1) != Err(Errno::EBADF) {
        return Err("a bad descriptor with a kernel pointer did not report EBADF");
    }
    // A good descriptor with a pointer into the kernel is EFAULT.
    if file::sys_write(process, 1, KERNEL_HALF_BASE, 1) != Err(Errno::EFAULT) {
        return Err("writing from a kernel address was not EFAULT");
    }

    let _ = memory::sys_munmap(process, at, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// `writev` gathers the segments in order, and checks the whole array first.
fn check_writev_gathers_in_order(process: &Process) -> Result<(), &'static str> {
    let at = map_rw(process, PAGE_SIZE * 2)?;
    // The three pieces, laid out end to end; then an iovec array pointing at
    // them in an order that is *not* their order in memory, which is what
    // proves the gather follows the array rather than the addresses.
    let pieces: [&[u8]; 3] = [b"write(2)\n", b"  gathered by ", b"  three pieces, "];
    let mut offsets = [0_u64; 3];
    let mut cursor = at;
    for (slot, piece) in offsets.iter_mut().zip(pieces) {
        uaccess::copy_to_user(process.space(), cursor, piece)
            .map_err(|_| "could not stage a segment")?;
        *slot = cursor;
        cursor += piece.len() as u64;
    }

    // The array goes in the middle of the second page, well clear of the data.
    let array = at + PAGE_SIZE;
    let word = size_of::<usize>() as u64;
    let order = [2_usize, 1, 0];
    for (index, &which) in order.iter().enumerate() {
        let entry = array + (index as u64) * word * 2;
        let at = *offsets.get(which).ok_or("bad segment index")?;
        let piece = *pieces.get(which).ok_or("bad segment index")?;
        write_word(process, entry, at)?;
        write_word(process, entry + word, piece.len() as u64)?;
    }

    let total: usize = pieces.iter().map(|p| p.len()).sum();
    let written = file::sys_writev(process, 1, array, 3).map_err(|_| "writev was refused")?;
    if written != total {
        return Err("writev reported the wrong count");
    }

    // An impossible segment count is refused rather than walked.
    if file::sys_writev(process, 1, array, 100_000) != Err(Errno::EINVAL) {
        return Err("writev accepted more segments than IOV_MAX");
    }
    if file::sys_writev(process, 1, array, 0) != Ok(0) {
        return Err("writev with no segments was not zero");
    }

    let _ = memory::sys_munmap(process, at, PAGE_SIZE * 2).map_err(|_| "munmap was refused")?;
    Ok(())
}

/// Write one pointer-sized word into the program's memory.
fn write_word(process: &Process, at: u64, value: u64) -> Result<(), &'static str> {
    let bytes = value.to_le_bytes();
    let width = size_of::<usize>();
    let slot = bytes.get(..width).ok_or("impossible pointer width")?;
    uaccess::copy_to_user(process.space(), at, slot).map_err(|_| "could not stage a word")
}

// ---------------------------------------------------------------------------
// Descriptors
//
// A file under `/tmp`, through the handlers: created, written, sought, read
// back through a second descriptor that shares its offset, changed with
// `fcntl`, truncated and closed. `src/lib/fs/vfs` tests the same rules on the host;
// what only this can test is the layer between -- arguments narrowed as the
// ABI narrows them, this architecture's `open` flag bits, the table lock let
// go before the file is touched, and every description closed and every page
// of the file given back, which the frame count around `check_handlers` sees.
//
// Called directly rather than through `dispatch`, like the handler checks
// above: `dispatch` finds its process through the running task, and the boot
// task has none.
// ---------------------------------------------------------------------------

/// The file the descriptor checks make, NUL-terminated as a program passes it.
const CHECK_PATH: &[u8] = b"/tmp/descriptor-check\0";

/// Its name within `/tmp`, for the check that opens it relative to a
/// directory descriptor.
const CHECK_NAME: &[u8] = b"descriptor-check\0";

/// `/tmp` itself.
const TMP_PATH: &[u8] = b"/tmp\0";

/// What the checks write into it.
const CHECK_DATA: &[u8] = b"descriptors, stage 8";

/// Where on the scratch page each piece goes.
const AT_PATH: u64 = 0;
/// See [`AT_PATH`].
const AT_NAME: u64 = 64;
/// See [`AT_PATH`].
const AT_TMP: u64 = 128;
/// See [`AT_PATH`].
const AT_DATA: u64 = 256;
/// See [`AT_PATH`].
const AT_BACK: u64 = 512;
/// See [`AT_PATH`].
const AT_RESULT: u64 = 1024;
/// See [`AT_PATH`].
const AT_FULL: u64 = 1536;

/// The file an `openat` at the descriptor limit is refused, and must not
/// leave behind.
const FULL_PATH: &[u8] = b"/tmp/descriptor-full\0";

/// Run the descriptor checks on a page of their own, and leave nothing behind:
/// every descriptor they opened closed, the file unlinked, the page unmapped.
fn check_descriptors(process: &Process) -> Result<(), &'static str> {
    let page = map_rw(process, PAGE_SIZE)?;
    for (offset, bytes) in [
        (AT_PATH, CHECK_PATH),
        (AT_NAME, CHECK_NAME),
        (AT_TMP, TMP_PATH),
        (AT_DATA, CHECK_DATA),
        (AT_FULL, FULL_PATH),
    ] {
        uaccess::copy_to_user(process.space(), page + offset, bytes)
            .map_err(|_| "could not stage the descriptor checks")?;
    }

    let outcome = check_a_new_process_has_the_console(process)
        .and_then(|()| check_a_full_table_creates_nothing(process, page))
        .and_then(|()| check_a_file_opens_on_the_lowest_free_descriptor(process, page))
        .and_then(|()| check_a_dup_shares_the_offset(process, page))
        .and_then(|()| check_fcntl_and_dup3_follow_linux(process, page))
        .and_then(|()| check_descriptors_are_refused_by_kind(process, page))
        .and_then(|()| check_flock_belongs_to_the_description(process, page))
        .and_then(|()| check_readahead_accepts_only_a_readable_file(process, page))
        .and_then(|()| check_record_locks_follow_linux(process, page));

    // Cleaned up whatever happened, so that a failure is reported as itself
    // and not also as leaked frames.
    for fd in 3..32 {
        let _ = fd::sys_close(process, fd);
    }
    let namespace = crate::fs::namespace();
    for staged in [CHECK_PATH, FULL_PATH] {
        let path = staged.strip_suffix(b"\0").unwrap_or(staged);
        let _ = namespace.unlink(&namespace.context(), None, path);
    }
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome
}

/// Stage a 32-byte `struct flock` -- `struct flock64` on ARMv7-A, where the
/// checks use the `64` commands -- at `at`.
fn stage_flock(
    on: &Process,
    at: u64,
    kind: i16,
    start: i64,
    len: i64,
    pid: i32,
) -> Result<(), &'static str> {
    let mut bytes = [0_u8; 32];
    for (offset, value) in [
        (0, &kind.to_le_bytes()[..]),
        (2, &0_i16.to_le_bytes()[..]),
        (8, &start.to_le_bytes()[..]),
        (16, &len.to_le_bytes()[..]),
        (24, &pid.to_le_bytes()[..]),
    ] {
        if let Some(slot) = bytes.get_mut(offset..offset + value.len()) {
            slot.copy_from_slice(value);
        }
    }
    uaccess::copy_to_user(on.space(), at, &bytes).map_err(|_| "could not stage a struct flock")
}

/// What `F_GETLK` wrote at `at`: type, start, length and pid.
fn reported_flock(on: &Process, at: u64) -> Result<(i16, i64, i64, i32), &'static str> {
    let bytes: [u8; 32] = read_user(on, at)?;
    let word = |from: usize| {
        bytes
            .get(from..from + 8)
            .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
            .map_or(0, i64::from_le_bytes)
    };
    let kind = bytes
        .get(..2)
        .and_then(|slice| <[u8; 2]>::try_from(slice).ok())
        .map_or(-1, i16::from_le_bytes);
    let pid = bytes
        .get(24..28)
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .map_or(0, i32::from_le_bytes);
    Ok((kind, word(8), word(16), pid))
}

/// One record-lock command on `on`, through `fcntl64` -- which a 64-bit build
/// reads exactly as `fcntl` -- with its structure at `at`.
fn record_lock(on: &Process, fd: i32, cmd: u32, at: u64) -> Result<usize, Errno> {
    crate::syscall::flock::sys_fcntl_lock(
        on,
        fd,
        cmd,
        at,
        ferrix_linux_abi::nr::Syscall::Fcntl64,
        crate::trap::Abi::Native,
    )
}

/// Record locks follow Linux.
///
/// Two descriptions' OFD write locks over one range conflict -- the negative
/// control -- and `F_OFD_GETLK` reports the holder with pid -1, while ranges
/// that only touch do not conflict. A classic read lock conflicts with its own
/// process's OFD lock and with another process's write lock, and `F_GETLK`
/// reports it with its process's pid. Closing any descriptor the process has
/// on the file releases it, not only the one that set it. Releasing the middle
/// of a lock leaves the parts either side. A bad type is `EINVAL`, a write lock
/// on a read-only descriptor `EBADF`, and an OFD request with a pid `EINVAL`.
fn check_record_locks_follow_linux(process: &Process, page: u64) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{F_OFD_GETLK, F_OFD_SETLK, F_RDLCK, F_UNLCK, F_WRLCK};
    let narrow = size_of::<usize>() == 4;
    let (getlk, setlk) = if narrow {
        (
            ferrix_linux_abi::types::F_GETLK64,
            ferrix_linux_abi::types::F_SETLK64,
        )
    } else {
        (
            ferrix_linux_abi::types::F_GETLK,
            ferrix_linux_abi::types::F_SETLK,
        )
    };
    let at = page + AT_RESULT;

    let first = open_check_file(process, page, O_RDWR | O_CREAT)?;
    let second = open_check_file(process, page, O_RDWR)?;
    stage_flock(process, at, F_WRLCK, 0, 100, 0)?;
    answers(
        record_lock(process, first, F_OFD_SETLK, at),
        0,
        "an OFD write lock on an unlocked range was refused",
    )?;
    stage_flock(process, at, F_WRLCK, 50, 10, 0)?;
    refuses(
        record_lock(process, second, F_OFD_SETLK, at),
        Errno::EAGAIN,
        "a second description was granted an OFD write lock over the first's",
    )?;
    answers(
        record_lock(process, second, F_OFD_GETLK, at),
        0,
        "F_OFD_GETLK was refused",
    )?;
    if reported_flock(process, at)? != (F_WRLCK, 0, 100, -1) {
        return Err("F_OFD_GETLK did not report the OFD write lock over 0..100 with pid -1");
    }
    stage_flock(process, at, F_WRLCK, 100, 10, 7)?;
    refuses(
        record_lock(process, second, F_OFD_SETLK, at),
        Errno::EINVAL,
        "an OFD lock request with a pid was not EINVAL",
    )?;
    stage_flock(process, at, F_WRLCK, 100, 10, 0)?;
    answers(
        record_lock(process, second, F_OFD_SETLK, at),
        0,
        "an OFD lock on a range that only touches another was refused",
    )?;

    stage_flock(process, at, F_RDLCK, 200, 0, 0)?;
    answers(
        record_lock(process, first, setlk, at),
        0,
        "a classic read lock to the end of the file was refused",
    )?;
    stage_flock(process, at, F_WRLCK, 300, 10, 0)?;
    refuses(
        record_lock(process, second, F_OFD_SETLK, at),
        Errno::EAGAIN,
        "an OFD write lock was granted over its own process's classic read lock",
    )?;

    let other = process::new_for_check()
        .map_err(|_| "could not make a process for the record-lock check")?;
    let other_page = map_rw(&other, PAGE_SIZE)?;
    let outcome =
        check_record_locks_between_processes(process, &other, other_page, [getlk, setlk], page);
    let _ = memory::sys_munmap(&other, other_page, PAGE_SIZE);
    outcome?;

    stage_flock(process, at, 7, 0, 1, 0)?;
    refuses(
        record_lock(process, second, setlk, at),
        Errno::EINVAL,
        "a record lock of type 7 was not EINVAL",
    )?;
    let read_only = open_check_file(process, page, O_RDONLY)?;
    stage_flock(process, at, F_WRLCK, 0, 1, 0)?;
    let refused = record_lock(process, read_only, setlk, at);
    let _ = fd::sys_close(process, read_only);
    refuses(
        refused,
        Errno::EBADF,
        "a write lock through a read-only descriptor was not EBADF",
    )?;
    stage_flock(process, at, F_UNLCK, 0, 0, 0)?;
    answers(
        record_lock(process, second, F_OFD_SETLK, at),
        0,
        "releasing every OFD lock was refused",
    )
}

/// The classic-lock half of [`check_record_locks_follow_linux`], between the
/// check process, which holds a read lock from byte 200 to the end, and
/// `other`.
fn check_record_locks_between_processes(
    process: &Process,
    other: &Process,
    other_page: u64,
    [getlk, setlk]: [u32; 2],
    page: u64,
) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{F_RDLCK, F_UNLCK, F_WRLCK};
    uaccess::copy_to_user(other.space(), other_page + AT_PATH, CHECK_PATH)
        .map_err(|_| "could not stage the path in another process")?;
    let theirs = open_check_file(other, other_page, O_RDWR)?;
    let at = other_page + AT_RESULT;

    stage_flock(other, at, F_WRLCK, 200, 10, 0)?;
    refuses(
        record_lock(other, theirs, setlk, at),
        Errno::EAGAIN,
        "another process was granted a write lock over a classic read lock",
    )?;
    stage_flock(other, at, F_WRLCK, 250, 1, 0)?;
    answers(
        record_lock(other, theirs, getlk, at),
        0,
        "F_GETLK was refused",
    )?;
    let pid = i32::try_from(process.pid()).unwrap_or(-1);
    if reported_flock(other, at)? != (F_RDLCK, 200, 0, pid) {
        return Err(
            "F_GETLK did not report the classic read lock from 200 to the end with its process's pid",
        );
    }

    // Any close of the file by the process releases its classic lock.
    let another = open_check_file(process, page, O_RDONLY)?;
    let _ = fd::sys_close(process, another);
    stage_flock(other, at, F_WRLCK, 200, 10, 0)?;
    answers(
        record_lock(other, theirs, setlk, at),
        0,
        "closing another descriptor of the file left the process's classic lock in place",
    )?;

    stage_flock(other, at, F_UNLCK, 203, 2, 0)?;
    answers(
        record_lock(other, theirs, setlk, at),
        0,
        "releasing the middle of a lock was refused",
    )?;
    let mine = page + AT_RESULT;
    stage_flock(process, mine, F_WRLCK, 200, 10, 0)?;
    let second = open_check_file(process, page, O_RDONLY)?;
    let asked = record_lock(process, second, getlk, mine);
    let _ = fd::sys_close(process, second);
    answers(asked, 0, "F_GETLK was refused to the check process")?;
    let their_pid = i32::try_from(other.pid()).unwrap_or(-1);
    if reported_flock(process, mine)? != (F_WRLCK, 200, 3, their_pid) {
        return Err("F_GETLK did not report the part of a lock left before a released range");
    }
    let _ = fd::sys_close(other, theirs);
    Ok(())
}

/// Open the descriptor checks' file with `flags`, as a descriptor number.
fn open_check_file(process: &Process, page: u64, flags: u32) -> Result<i32, &'static str> {
    fd::sys_openat(process, AT_FDCWD, page + AT_PATH, flags, 0o644)
        .ok()
        .and_then(|fd| i32::try_from(fd).ok())
        .ok_or("could not open the descriptor checks' file")
}

/// A `flock` lock is the open file description's, not the descriptor's.
///
/// A second `open` of the file is refused `LOCK_NB` while the first holds
/// `LOCK_EX` -- the negative control, that a lock is really held -- and is
/// still refused after the first descriptor closes while a `dup` of it keeps
/// the description alive. Once the description is gone it is granted. Between
/// those, two shared locks coexist and a `dup` converts its description's
/// lock in place. A bad operation is `EINVAL`, and a closed or `O_PATH`
/// descriptor `EBADF`.
fn check_flock_belongs_to_the_description(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    use crate::syscall::flock::sys_flock;
    use ferrix_linux_abi::types::{LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN, O_PATH};
    let nb = |fd: i32, operation: u32| sys_flock(process, fd, operation | LOCK_NB);

    let first = open_check_file(process, page, O_RDWR | O_CREAT)?;
    let second = open_check_file(process, page, O_RDONLY)?;
    answers(
        sys_flock(process, first, LOCK_EX),
        0,
        "flock(LOCK_EX) on a file nobody had locked was refused",
    )?;
    refuses(
        nb(second, LOCK_EX),
        Errno::EAGAIN,
        "a second description was granted LOCK_EX while the first held LOCK_EX",
    )?;
    refuses(
        nb(second, LOCK_SH),
        Errno::EAGAIN,
        "a second description was granted LOCK_SH while the first held LOCK_EX",
    )?;
    answers(
        nb(first, LOCK_EX),
        0,
        "a description asking again for the lock it holds was refused",
    )?;

    let shared = fd::sys_dup(process, first)
        .ok()
        .and_then(|fd| i32::try_from(fd).ok())
        .ok_or("dup was refused in the flock check")?;
    answers(
        nb(shared, LOCK_SH),
        0,
        "a dup could not convert its description's LOCK_EX to LOCK_SH",
    )?;
    answers(
        nb(second, LOCK_SH),
        0,
        "two descriptions could not both hold LOCK_SH",
    )?;
    answers(
        sys_flock(process, second, LOCK_UN),
        0,
        "LOCK_UN was refused",
    )?;
    answers(
        nb(shared, LOCK_EX),
        0,
        "a description alone on a file could not convert LOCK_SH back to LOCK_EX",
    )?;

    let _ = fd::sys_close(process, first);
    refuses(
        nb(second, LOCK_SH),
        Errno::EAGAIN,
        "closing one of two descriptors of a description released its lock",
    )?;
    let _ = fd::sys_close(process, shared);
    answers(
        nb(second, LOCK_EX),
        0,
        "LOCK_NB was still refused after the holding description was dropped",
    )?;
    answers(
        sys_flock(process, second, LOCK_UN),
        0,
        "LOCK_UN was refused",
    )?;

    refuses(
        sys_flock(process, second, 0),
        Errno::EINVAL,
        "flock with no operation was not EINVAL",
    )?;
    refuses(
        sys_flock(process, second, LOCK_SH | LOCK_EX),
        Errno::EINVAL,
        "flock asking for both LOCK_SH and LOCK_EX was not EINVAL",
    )?;
    let _ = fd::sys_close(process, second);
    refuses(
        sys_flock(process, second, LOCK_SH),
        Errno::EBADF,
        "flock on a closed descriptor was not EBADF",
    )?;
    let path_only = open_check_file(process, page, O_PATH)?;
    let refused = sys_flock(process, path_only, LOCK_SH);
    let _ = fd::sys_close(process, path_only);
    refuses(
        refused,
        Errno::EBADF,
        "flock on an O_PATH descriptor was not EBADF",
    )
}

/// `readahead` has nothing to fill and answers 0 for a readable regular file;
/// a descriptor not open for reading is `EBADF`, and the console, which is
/// not a regular file, is `EINVAL`.
fn check_readahead_accepts_only_a_readable_file(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    use crate::syscall::fsctl::sys_readahead;
    use ferrix_linux_abi::types::O_WRONLY;

    let readable = open_check_file(process, page, O_RDONLY)?;
    let answered = sys_readahead(process, readable, 4096);
    let too_long = sys_readahead(process, readable, u64::MAX);
    let _ = fd::sys_close(process, readable);
    answers(
        answered,
        0,
        "readahead on a readable regular file was refused",
    )?;
    refuses(
        too_long,
        Errno::EINVAL,
        "a readahead count too large for a loff_t was accepted",
    )?;
    let written = open_check_file(process, page, O_WRONLY)?;
    let refused = sys_readahead(process, written, 4096);
    let _ = fd::sys_close(process, written);
    refuses(
        refused,
        Errno::EBADF,
        "readahead on a descriptor open only for writing was not EBADF",
    )?;
    refuses(
        sys_readahead(process, 0, 4096),
        Errno::EINVAL,
        "readahead on the console was not EINVAL",
    )
}

/// Require a handler to have answered `want`.
fn answers(got: Result<usize, Errno>, want: usize, what: &'static str) -> Result<(), &'static str> {
    if got == Ok(want) { Ok(()) } else { Err(what) }
}

/// Require a handler to have refused with `errno`.
fn refuses(
    got: Result<usize, Errno>,
    errno: Errno,
    what: &'static str,
) -> Result<(), &'static str> {
    if got == Err(errno) { Ok(()) } else { Err(what) }
}

/// Descriptors 0, 1 and 2 are the console, open for reading and writing --
/// which is what busybox's `printf` asks of descriptor 1 before it prints.
fn check_a_new_process_has_the_console(process: &Process) -> Result<(), &'static str> {
    for fd in 0..3 {
        answers(
            fd::sys_fcntl(process, fd, F_GETFL, 0),
            O_RDWR as usize,
            "a new process's standard descriptor did not report O_RDWR from F_GETFL",
        )?;
    }
    refuses(
        fd::sys_lseek(process, 1, 0, SEEK_CUR),
        Errno::ESPIPE,
        "the console could be sought, as if it were a file",
    )?;
    refuses(
        fd::sys_ioctl(process, 99, TCGETS, 0),
        Errno::EBADF,
        "an ioctl on a closed descriptor was not EBADF",
    )
}

/// `openat` with every descriptor taken is `EMFILE` and creates nothing: the
/// same `O_CREAT|O_EXCL` succeeds once a descriptor is free again, where a
/// file left behind by the refused call would make it `EEXIST`. Linux takes
/// the descriptor number before it touches the path, for this reason.
fn check_a_full_table_creates_nothing(process: &Process, page: u64) -> Result<(), &'static str> {
    let flags = O_RDWR | O_CREAT | O_EXCL;
    let limit = process.files().lock().limit();
    // Descriptors 0, 1 and 2 are the console, so a limit of three leaves
    // none free.
    process
        .files()
        .lock()
        .set_limit(3)
        .map_err(|_| "could not lower the descriptor limit")?;
    let refused = fd::sys_openat(process, AT_FDCWD, page + AT_FULL, flags, 0o644);
    let restored = process.files().lock().set_limit(limit);
    restored.map_err(|_| "could not restore the descriptor limit")?;
    refuses(
        refused,
        Errno::EMFILE,
        "openat with every descriptor taken was not EMFILE",
    )?;
    let opened = fd::sys_openat(process, AT_FDCWD, page + AT_FULL, flags, 0o644);
    if let Ok(fd) = opened {
        let _ = fd::sys_close(process, i32::try_from(fd).unwrap_or(-1));
    }
    let path = FULL_PATH.strip_suffix(b"\0").unwrap_or(FULL_PATH);
    let namespace = crate::fs::namespace();
    let _ = namespace.unlink(&namespace.context(), None, path);
    answers(
        opened,
        3,
        "openat refused for EMFILE had created its file anyway",
    )
}

/// `openat` with `O_CREAT` lands on descriptor 3, close-on-exec as asked, and
/// `F_SETFD` takes the flag away again.
fn check_a_file_opens_on_the_lowest_free_descriptor(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let flags = O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC;
    answers(
        fd::sys_openat(process, AT_FDCWD, page + AT_PATH, flags, 0o644),
        3,
        "a file created under /tmp did not open on descriptor 3",
    )?;
    answers(
        fd::sys_fcntl(process, 3, F_GETFD, 0),
        FD_CLOEXEC as usize,
        "O_CLOEXEC did not reach the descriptor",
    )?;
    answers(
        fd::sys_fcntl(process, 3, F_SETFD, 0),
        0,
        "F_SETFD was refused",
    )?;
    answers(
        fd::sys_fcntl(process, 3, F_GETFD, 0),
        0,
        "F_SETFD did not clear close-on-exec",
    )?;
    answers(
        fd::sys_fcntl(process, 3, F_GETFL, 0),
        O_RDWR as usize,
        "a file opened O_RDWR did not report it from F_GETFL",
    )
}

/// A write, then a read back through `dup`'s descriptor, which shares the
/// offset -- and `pread64` and `pwrite64`, which leave it alone.
fn check_a_dup_shares_the_offset(process: &Process, page: u64) -> Result<(), &'static str> {
    let len = CHECK_DATA.len();
    let back = page + AT_BACK;
    answers(
        file::sys_write(process, 3, page + AT_DATA, len as u64),
        len,
        "a write to a file under /tmp was short",
    )?;
    answers(fd::sys_dup(process, 3), 4, "dup did not take descriptor 4")?;
    answers(
        fd::sys_lseek(process, 4, 0, SEEK_CUR),
        len,
        "a duplicated descriptor does not share the offset the write moved",
    )?;
    answers(
        fd::sys_lseek(process, 3, 0, SEEK_SET),
        0,
        "SEEK_SET was refused",
    )?;
    answers(
        file::sys_read(process, 4, back, 64),
        len,
        "reading back through the duplicate did not start where the seek put the shared offset",
    )?;
    let mut read = [0_u8; 64];
    let read = read.get_mut(..len).ok_or("impossible check data length")?;
    uaccess::copy_from_user(process.space(), back, read).map_err(|_| "could not read back")?;
    if read != CHECK_DATA {
        return Err("what was read back is not what was written");
    }
    answers(
        file::sys_read(process, 4, back, 64),
        0,
        "a read at the end was not end of file",
    )?;

    answers(
        file::sys_pwrite64(process, 3, page + AT_DATA, 3, 0),
        3,
        "pwrite64 was short",
    )?;
    answers(
        file::sys_pread64(process, 4, back, 5, 3),
        5,
        "pread64 did not read five bytes from the middle",
    )?;
    answers(
        fd::sys_lseek(process, 3, 0, SEEK_CUR),
        len,
        "pread64 or pwrite64 moved the offset",
    )?;
    refuses(
        file::sys_pread64(process, 3, back, 1, -1),
        Errno::EINVAL,
        "pread64 accepted a negative offset",
    )
}

/// `F_DUPFD`, `F_SETFL`, `dup3`, `ftruncate` and `_llseek`, and what each of
/// them refuses.
fn check_fcntl_and_dup3_follow_linux(process: &Process, page: u64) -> Result<(), &'static str> {
    let len = CHECK_DATA.len();
    answers(
        fd::sys_fcntl(process, 3, F_DUPFD, 10),
        10,
        "F_DUPFD did not start at 10",
    )?;
    answers(
        fd::sys_fcntl(process, 3, F_DUPFD_CLOEXEC, 10),
        11,
        "F_DUPFD_CLOEXEC did not take the next free descriptor",
    )?;
    answers(
        fd::sys_fcntl(process, 11, F_GETFD, 0),
        FD_CLOEXEC as usize,
        "F_DUPFD_CLOEXEC did not set close-on-exec",
    )?;
    refuses(
        fd::sys_fcntl(process, 3, 999, 0),
        Errno::EINVAL,
        "an unknown fcntl was not EINVAL",
    )?;
    refuses(
        fd::sys_fcntl(process, 99, 999, 0),
        Errno::EBADF,
        "an unknown fcntl on a closed descriptor was not EBADF",
    )?;
    refuses(
        fd::sys_dup3(process, 3, 3, 0),
        Errno::EINVAL,
        "dup3 onto itself was not EINVAL",
    )?;
    refuses(
        fd::sys_dup3(process, 3, 20, 1),
        Errno::EINVAL,
        "dup3 accepted an unknown flag",
    )?;
    answers(
        fd::sys_dup3(process, 3, 20, O_CLOEXEC),
        20,
        "dup3 did not install at 20",
    )?;
    answers(
        fd::sys_fcntl(process, 20, F_GETFD, 0),
        FD_CLOEXEC as usize,
        "dup3's O_CLOEXEC did not reach the descriptor",
    )?;
    answers(
        fd::sys_dup2(process, 3, 3),
        3,
        "dup2 onto itself was not a no-op",
    )?;

    // O_APPEND through F_SETFL: a write after seeking to the start still
    // lands at the end.
    answers(
        fd::sys_fcntl(process, 3, F_SETFL, u64::from(O_APPEND)),
        0,
        "F_SETFL was refused",
    )?;
    answers(
        fd::sys_fcntl(process, 4, F_GETFL, 0),
        (O_RDWR | O_APPEND) as usize,
        "O_APPEND set on one descriptor did not show on its duplicate",
    )?;
    answers(
        fd::sys_lseek(process, 3, 0, SEEK_SET),
        0,
        "SEEK_SET was refused",
    )?;
    answers(
        file::sys_write(process, 3, page + AT_DATA, 1),
        1,
        "an append was short",
    )?;
    answers(
        fd::sys_lseek(process, 3, 0, SEEK_CUR),
        len + 1,
        "a write under O_APPEND did not land at the end",
    )?;

    answers(fd::sys_ftruncate(process, 3, 4), 0, "ftruncate was refused")?;
    answers(
        fd::sys_lseek(process, 4, 0, SEEK_END),
        4,
        "the file was not four bytes long after ftruncate",
    )?;
    refuses(
        fd::sys_ftruncate(process, 3, -1),
        Errno::EINVAL,
        "ftruncate accepted a negative length",
    )?;

    let result = page + AT_RESULT;
    answers(
        fd::sys_llseek(process, 3, 0, 2, result, SEEK_SET),
        0,
        "_llseek was refused",
    )?;
    let mut offset = [0_u8; 8];
    uaccess::copy_from_user(process.space(), result, &mut offset)
        .map_err(|_| "could not read _llseek's result")?;
    if u64::from_le_bytes(offset) != 2 {
        return Err("_llseek did not write the new offset through its pointer");
    }
    Ok(())
}

/// A directory descriptor as a starting point, `O_DIRECTORY` with this
/// architecture's bit, and the refusals that depend on what a descriptor names.
fn check_descriptors_are_refused_by_kind(process: &Process, page: u64) -> Result<(), &'static str> {
    let directory = arch::OPEN_FLAGS.directory;
    refuses(
        fd::sys_openat(process, AT_FDCWD, page + AT_PATH, O_RDONLY | directory, 0),
        Errno::ENOTDIR,
        "O_DIRECTORY, in this architecture's bits, opened a regular file",
    )?;
    let tmp = fd::sys_openat(process, AT_FDCWD, page + AT_TMP, O_RDONLY | directory, 0)
        .map_err(|_| "/tmp did not open as a directory")?;
    let tmp = i32::try_from(tmp).map_err(|_| "an impossible descriptor")?;

    match fd::start_location(process, AT_FDCWD) {
        Ok(None) => {}
        _ => return Err("AT_FDCWD did not mean the working directory"),
    }
    if !matches!(fd::start_location(process, tmp), Ok(Some(_))) {
        return Err("a directory descriptor was not a starting point");
    }
    if !matches!(fd::start_location(process, 3), Err(Errno::ENOTDIR)) {
        return Err("a file descriptor was accepted as a starting point");
    }
    if !matches!(fd::start_location(process, 99), Err(Errno::EBADF)) {
        return Err("a closed descriptor was accepted as a starting point");
    }

    let relative = fd::sys_openat(process, tmp, page + AT_NAME, O_RDONLY, 0)
        .map_err(|_| "a name relative to a directory descriptor did not open")?;
    let relative = i32::try_from(relative).map_err(|_| "an impossible descriptor")?;
    refuses(
        fd::sys_ioctl(process, relative, TCGETS, page + AT_RESULT),
        Errno::ENOTTY,
        "a regular file answered TCGETS, as if it were a terminal",
    )?;
    refuses(
        fd::sys_ioctl(process, relative, TCGETS2, page + AT_RESULT),
        Errno::ENOTTY,
        "a regular file answered TCGETS2, as if it were a terminal",
    )?;
    refuses(
        fd::sys_ioctl(process, relative, TIOCGWINSZ, page + AT_RESULT),
        Errno::ENOTTY,
        "a regular file answered TIOCGWINSZ, as if it were a terminal",
    )?;
    refuses(
        file::sys_write(process, relative, page + AT_DATA, 1),
        Errno::EBADF,
        "a descriptor opened O_RDONLY was written",
    )?;
    refuses(
        file::sys_read(process, tmp, page + AT_BACK, 1),
        Errno::EISDIR,
        "a directory was read as a file",
    )?;

    answers(fd::sys_close(process, relative), 0, "close was refused")?;
    refuses(
        fd::sys_close(process, relative),
        Errno::EBADF,
        "a second close was not EBADF",
    )?;
    refuses(
        file::sys_read(process, relative, page + AT_BACK, 1),
        Errno::EBADF,
        "a closed descriptor was read",
    )
}

// ---------------------------------------------------------------------------
// Ring 3
//
// The one check here that cannot be faked. Everything above it calls handlers
// from kernel code with a `Process` in hand; this hands the processor to an
// address space the kernel built, at a privilege level where none of the
// kernel's own memory is reachable, and waits to be asked for something.
//
// If the loader mapped the wrong page, the stack image put `argc` in the wrong
// place, the trampoline mismatched its pushes, or `swapgs` went the wrong way,
// the result is not a wrong answer. It is a fault in ring 3 with no handler
// that can say anything useful -- which is why every part of this was checked
// separately first.
// ---------------------------------------------------------------------------

/// Run a program in user mode and require it to come back correctly.
/// Verifies: L.trap.2
fn check_a_program_runs_in_user_mode() -> Result<Option<i32>, &'static str> {
    if arch::USER_TEST_PROGRAM.is_empty() {
        // No transition on this architecture yet. Reported as absent rather
        // than skipped silently: the boot log should say which architectures
        // can do this and which cannot.
        return Ok(None);
    }

    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_TEST_PROGRAM,
    );

    let faults_before = crate::trap::handled_fault_count();
    let status = exec::run(
        &file,
        &[b"/hello", b"--first"],
        &[b"FERRIX=1"],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the program could not be started")?;

    // Stage 6's exit criterion is a program that runs "with a page fault
    // serviced along the way", so require one rather than assume it.
    //
    // Today the fault is incidental, and that is exactly why it is asserted.
    // The loader maps the image writable, copies it in -- which faults every
    // page in -- and then narrows the text to executable with `protect`, which
    // takes those translations down. The program's first instruction fetch
    // re-faults the page. A natural fix to `protect`, rewriting permissions in
    // place instead of unmapping, would make that fault disappear with every
    // other check still passing, and the criterion would quietly stop being
    // exercised. This makes that visible instead.
    if crate::trap::handled_fault_count().saturating_sub(faults_before) == 0 {
        return Err("the program ran without a page fault being serviced");
    }

    if status != arch::USER_TEST_STATUS {
        return Err("the program exited with the wrong status");
    }
    Ok(Some(status))
}

// ---------------------------------------------------------------------------
// Programs as tasks
//
// Two programs at once, and one ended from outside. What the single-program
// check above cannot show: that a program is preempted in user mode, which
// needs interrupts open there; that two programs keep their own registers,
// address spaces and exit statuses while taking turns; and that a program
// which never calls `exit_group` can still be ended.
// ---------------------------------------------------------------------------

/// Loop iterations each spinning program runs before it writes and exits.
///
/// Enough to cover several scheduling slices under KVM, where the loop is
/// fastest, and a second or so under `tcg`, where it is slowest.
const SPIN_ROUNDS: u32 = 30_000_000;

/// The status a spinning program exits with when its stack pointer changed
/// across the loop, which is to say while it was preempted.
const SPIN_STACK_CHANGED: i32 = 99;

/// How long the checks wait for a program before calling it lost.
const PROGRAM_PATIENCE_NANOS: u64 = 120_000_000_000;

/// How long the killed program is left spinning first.
const KILL_AFTER_NANOS: u64 = 20_000_000;

/// How soon after its kill a spinning program's task must be gone.
///
/// Far shorter than the program would take to finish its loop on its own, so a
/// kill that did not reach it fails here rather than passing late.
const KILL_REACH_NANOS: u64 = 1_000_000_000;

/// The status a program is killed with: 128 plus `SIGKILL`, which is what a
/// shell reports for one.
const KILL_STATUS: i32 = 137;

/// Build a spinning program tagged `tag` that loops `rounds` times and exits
/// with `status`, and load it into a process of its own.
pub(crate) fn spinner(tag: u8, rounds: u32, status: u32) -> Result<Arc<Process>, &'static str> {
    let mut program = arch::USER_SPIN_PROGRAM.to_vec();
    let at = program
        .len()
        .checked_sub(10)
        .ok_or("the spinning program is shorter than its own layout")?;

    let mut tail = [0_u8; 10];
    tail[0] = tag;
    tail[1] = b'\n';
    let words = rounds.to_le_bytes().into_iter().chain(status.to_le_bytes());
    for (slot, byte) in tail.iter_mut().skip(2).zip(words) {
        *slot = byte;
    }
    let slot = program
        .get_mut(at..)
        .ok_or("the spinning program is shorter than its own layout")?;
    if slot.first() != Some(&b'?') || slot.get(1) != Some(&b'\n') {
        return Err("the spinning program's tail is not the layout it documents");
    }
    slot.copy_from_slice(&tail);

    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        &program,
    );
    // The second spinner's argument vector is longer by more than a stack
    // alignment, so the two programs' stack pointers differ. With identical
    // startup stacks, a kernel that handed one program the other's stack
    // pointer would pass the comparison each makes.
    let args: &[&[u8]] = if tag == b'2' {
        &[
            b"/spin",
            b"an argument long enough to move the stack by more than sixteen bytes",
        ]
    } else {
        &[b"/spin"]
    };
    process::load(&file, args, &[], [0x5a; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a spinning program could not be loaded")
}

/// Two programs pinned to one processor both finish, each with its own
/// status, and each is switched to more than once.
///
/// Two properties, because the obvious one is not enough. Each program must be
/// switched to more than once, or they ran one after the other. But that alone
/// passes with interrupts masked in user mode: each program's `write` opens
/// them inside the kernel, a tick that was pending all along is taken there,
/// and the program is switched away from and back to without ever having been
/// preempted in user mode. So interrupts must also have arrived *in user mode*
/// while the two ran. Masking them there fails this with its own message; that
/// was tried, and the switch count alone did not notice.
/// Verifies: `L.x86_64.59`, `L.x86_64.79`
fn check_two_programs_take_turns_on_one_processor() -> Result<Option<(u64, u64)>, &'static str> {
    if arch::USER_SPIN_PROGRAM.is_empty() {
        return Ok(None);
    }
    let here = crate::smp::this_cpu()
        .ok_or("no processor to run two programs on")?
        .logical;

    // What is required of a pair is that each program was *preempted* at
    // least twice: switched away from while still runnable, by an interrupt
    // that arrived in its own code. Not that each was switched to twice,
    // which the check used to ask and which a program that was never
    // preempted also shows -- once when it starts and once coming back from
    // its first write -- so a kernel that never rescheduled at interrupt exit
    // passed it. Nor every switch away while runnable: a pending reschedule
    // is also taken when a lock that disables preemption is released, and a
    // kernel that never preempted user code still switched a program out two
    // and three times inside its one write, at each lock the timer ticked
    // under. Twice rather than once so that the evidence is an alternation
    // -- each cut at least once after the other had run -- and not one tick.
    //
    // More than one pair, because one pair measures the host as much as the
    // kernel: an emulator whose host stalls this virtual processor while
    // program A is running charges the stall to A as service, EEVDF then owes
    // B the same amount and lets B run its whole loop before A is looked at
    // again, and B finishes never preempted -- the scheduler doing the right
    // thing with accounting the host faked. Every attempt is judged on the
    // same evidence, and printed, so a kernel that never preempts still
    // fails all of them; only a stall gets another chance.
    for attempt in 1..=TURN_ATTEMPTS {
        let overrun_before =
            crate::sched::cpu_report(here).map_or(0, |report| report.worst_overrun);
        let user_interrupts = crate::trap::user_interrupt_count();
        let Turns {
            switched,
            preempted,
        } = one_pair_takes_turns(here)?;
        let overrun = crate::sched::cpu_report(here).map_or(0, |report| report.worst_overrun);
        if preempted.0 >= 2 && preempted.1 >= 2 && switched.0 >= 2 && switched.1 >= 2 {
            if crate::trap::user_interrupt_count() == user_interrupts {
                return Err("no interrupt arrived while two spinning programs were in user mode");
            }
            return Ok(Some(switched));
        }
        crate::console::println!(
            "  procs    attempt {attempt}: preempted {} and {} times, switched to {} and {}; this \
             processor's worst overrun {} us (was {} us), {} preemption-disabling locks held",
            preempted.0,
            preempted.1,
            switched.0,
            switched.1,
            overrun / 1000,
            overrun_before / 1000,
            crate::sched::preemption_held(here),
        );
    }
    Err(
        "two programs on one processor were never both preempted twice, in every attempt: a \
         host stall charged to one as service would explain one attempt, not all",
    )
}

/// How many pairs of programs the turn-taking check runs before it decides
/// the processor never preempts.
const TURN_ATTEMPTS: usize = 3;

/// What one pair of spinning programs showed: per program, how many times
/// it was switched to and how many times it was preempted.
struct Turns {
    /// Switched-to counts, first and second program.
    switched: (u64, u64),
    /// Preemption counts, first and second program.
    preempted: (u64, u64),
}

/// One pair: two spinning programs on `here`, both required to exit with
/// their own status. Answers how many times each was switched to, and how
/// many times each was preempted.
fn one_pair_takes_turns(here: usize) -> Result<Turns, &'static str> {
    let first = spinner(b'1', SPIN_ROUNDS, 41)?;
    let second = spinner(b'2', SPIN_ROUNDS, 43)?;
    let first_task = process::start_on(&first, Some(here))
        .map_err(|_| "the first of two programs could not be started")?;
    let second_task = process::start_on(&second, Some(here))
        .map_err(|_| "the second of two programs could not be started")?;

    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    let statuses = (
        first.wait_for_exit(deadline),
        second.wait_for_exit(deadline),
    );
    if statuses.0 == Some(SPIN_STACK_CHANGED) || statuses.1 == Some(SPIN_STACK_CHANGED) {
        return Err("a program's stack pointer changed while another program ran on its processor");
    }
    if statuses.0 != Some(41) {
        return Err("the first of two programs did not exit with its own status");
    }
    if statuses.1 != Some(43) {
        return Err("the second of two programs did not exit with its own status");
    }
    Ok(Turns {
        switched: (first_task.switches(), second_task.switches()),
        preempted: (first_task.preemptions(), second_task.preemptions()),
    })
}

/// A program that would spin for minutes is ended from outside: it reports
/// the status it was killed with, not its own, and its task actually stops.
fn check_a_program_is_killed_from_outside() -> Result<Option<i32>, &'static str> {
    if arch::USER_SPIN_PROGRAM.is_empty() {
        return Ok(None);
    }
    let here = crate::smp::this_cpu()
        .ok_or("no processor to run a program on")?
        .logical;
    // On another processor from this one, when there is one, so the victim
    // spins there alone. A task alone on its processor gets no timer tick, so
    // only the interrupt `kill` sends can bring it back through the kernel;
    // a victim sharing this processor would be reached by this checker's own
    // wake-ups and prove nothing about that.
    let count = crate::smp::count();
    let elsewhere = if count > 1 { (here + 1) % count } else { here };

    let victim = spinner(b'k', u32::MAX, 5)?;
    let task = process::start_on(&victim, Some(elsewhere))
        .map_err(|_| "a program to kill could not be started")?;
    crate::sched::sleep_for(KILL_AFTER_NANOS);
    if victim.is_terminated() {
        return Err(
            "a program that should still have been spinning had already ended, so nothing \
             preempted it in user mode",
        );
    }

    process::kill(&victim, KILL_STATUS);
    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    if victim.wait_for_exit(deadline) != Some(KILL_STATUS) {
        return Err("a killed program did not report the status it was killed with");
    }

    // The status alone would pass with a task still spinning in user mode
    // under a process that says it has ended -- and so would a generous
    // deadline, because the loop does end eventually. So the task must be gone
    // soon, not merely at some point.
    let reach = crate::timer::now_nanos().saturating_add(KILL_REACH_NANOS);
    while !task.is_dead() {
        if crate::timer::now_nanos() >= reach {
            return Err("a killed program kept running on its processor after its kill");
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(Some(KILL_STATUS))
}

// ---------------------------------------------------------------------------
// Stage 8: the calls that take a path
// ---------------------------------------------------------------------------

pub(crate) use paths::run_paths;

/// The path calls, against the real namespace under `/tmp`.
///
/// Every call goes in by its number, decoded by this build's own table, and
/// through the table `dispatch` uses once it has found a process -- so the one
/// line that hands path calls to `syscall::path` is exercised with the
/// handlers. Names and buffers live in a real user address space, and every
/// `stat` record is decoded back out of it in this architecture's layout.
///
/// A module of its own inside this file so that its imports stay its own.
mod paths {
    use alloc::vec;
    use alloc::vec::Vec;
    use core::mem::offset_of;

    use ferrix_bootinfo::PAGE_SIZE;
    use ferrix_linux_abi::errno::Errno;
    use ferrix_linux_abi::nr::Syscall;
    use ferrix_linux_abi::types::{
        self, AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW, DT_REG, O_PATH, O_RDONLY,
        O_WRONLY, R_OK, RENAME_EXCHANGE, RENAME_NOREPLACE, S_IFBLK, S_IFCHR, S_IFDIR, S_IFLNK,
        S_IFREG, STATX_BASIC_STATS, Statx, UTIME_OMIT, W_OK, X_OK,
    };
    use ferrix_vfs::dirent::{self, Record};
    use ferrix_vfs::initramfs::makedev;
    use ferrix_vfs::{FileType, Metadata, OpenFlags, Stat, Timespec};

    use super::{map_rw, number_for};
    use crate::arch;
    use crate::arch::StatLayout;
    use crate::fs;
    use crate::mm;
    use crate::syscall::process::{self, Process};
    use crate::syscall::{memory, uaccess};

    /// What the path checks measured, for the boot log.
    #[derive(Debug)]
    pub(crate) struct PathReport {
        /// Calls made on the measured run.
        pub(crate) calls: u32,
        /// Names `getdents64` reported from the directory it read in pieces.
        pub(crate) listed: usize,
        /// How many `getdents64` calls that took.
        pub(crate) listing_calls: u32,
        /// Device nodes `mknodat` made and the check opened by number.
        pub(crate) devices: usize,
        /// Frames the measured run cost once everything was removed. Zero, or
        /// a path call is leaking.
        pub(crate) leaked: i64,
        /// Dentries the namespace's cache held after the measured run that it
        /// did not hold before it. Reported beside `leaked` because the cache
        /// is the one thing that may legitimately keep memory across runs, and
        /// a frame it keeps is not a frame a call lost.
        pub(crate) cache_growth: i64,
    }

    /// Where the checks work. The last of them removes it again.
    const ROOT: &[u8] = b"/tmp/pathcheck";

    /// How many names the listing check makes.
    const LISTED: usize = 40;

    /// The `getdents64` buffer the listing is read with: four short entries.
    const LISTING_BUFFER: u64 = 96;

    /// What the symbolic link points at. Nothing: a dangling link is still a
    /// link, and following it must say so.
    ///
    /// Absolute, and directly in `/tmp`, for the same reason as [`LINK`].
    const TARGET: &[u8] = b"/tmp/pathcheck-nowhere";

    /// Where the link is made, and where the rename check looks for it after
    /// moving it away.
    ///
    /// In `/tmp` rather than under [`ROOT`], so that a lookup that misses
    /// leaves its negative dentry in a directory that outlives the run, where
    /// the next run finds it and reuses it. A negative entry cached under
    /// `ROOT` would keep `ROOT`'s own dentry alive after `rmdir`, and the
    /// second run's measurement would count a directory the cache kept as a
    /// leak.
    const LINK: &[u8] = b"/tmp/pathcheck-link";

    /// `AT_FDCWD`, as a register carries it.
    const CWD: u64 = AT_FDCWD as i64 as u64;

    /// Where the second string argument of a call is staged.
    const SECOND: u64 = PAGE_SIZE / 2;

    /// Run the checks twice and measure the second run, for the reason
    /// `check::run` gives: the first pays for size classes the heap keeps.
    pub(crate) fn run_paths() -> Result<PathReport, &'static str> {
        // The reaper first. `check_syscalls`, just before this, starts
        // programs whose tasks exit, and the idle loop frees a finished task's
        // stack and its process's address space whenever it next runs. One
        // freed inside the measured window raises the free count, which this
        // check read as a leak, and failed intermittently for it. So no count
        // is taken with an exited task left unreaped.
        let _warm = check_path_calls()?;
        let cached = fs::namespace().cached();
        crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
        let window = mm::FrameWindow::open();
        let mut report = check_path_calls()?;
        crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
        report.leaked = window.kept();
        report.cache_growth = i64::try_from(fs::namespace().cached()).unwrap_or(i64::MAX)
            - i64::try_from(cached).unwrap_or(i64::MAX);
        // Checked, not only printed. The dentry cache is the one thing that
        // may keep memory across runs, and it is not subtracted, because by
        // construction it does not grow here: the second run finds the
        // dentries the first left in /tmp, which is why `LINK` lives there. A
        // run that grew it has stopped being repeatable, and is told apart
        // from a call that lost a frame.
        mm::print_frame_delta("paths", report.leaked);
        if report.leaked != 0 {
            window.report("paths");
        }
        if report.leaked < 0 {
            return Err(
                "the free frame count rose across the path calls: something outside them freed frames in the window",
            );
        }
        if report.leaked != 0 && report.cache_growth != 0 {
            return Err("the path calls kept frames, and the dentry cache grew across the run");
        }
        if report.leaked != 0 {
            return Err("the path calls did not give every frame back");
        }
        Ok(report)
    }

    /// One run of every check, in a process of its own.
    fn check_path_calls() -> Result<PathReport, &'static str> {
        check_every_encoder_round_trips()?;
        let process = process::new_for_check().map_err(|_| "could not make a process")?;
        let mut p = Paths::new(&process)?;
        check_names_are_made_and_read(&mut p)?;
        check_creat_truncates_and_opens_for_writing(&mut p)?;
        check_renames_replace_only_when_allowed(&mut p)?;
        check_every_stat_describes_the_same_file(&mut p)?;
        let (listed, listing_calls) = check_a_listing_in_pieces_sees_each_name_once(&mut p)?;
        check_the_working_directory_follows_chdir(&mut p)?;
        check_access_and_attributes(&mut p)?;
        let devices = check_device_nodes_open_by_number(&mut p)?;
        check_names_are_removed(&mut p)?;
        let calls = p.calls;
        p.release()?;
        Ok(PathReport {
            calls,
            listed,
            listing_calls,
            devices,
            leaked: 0,
            cache_growth: 0,
        })
    }

    /// A process, a page for the strings a call takes, and a page for what it
    /// gives back.
    struct Paths<'a> {
        process: &'a Process,
        strings: u64,
        out: u64,
        calls: u32,
    }

    impl<'a> Paths<'a> {
        fn new(process: &'a Process) -> Result<Paths<'a>, &'static str> {
            Ok(Paths {
                process,
                strings: map_rw(process, PAGE_SIZE)?,
                out: map_rw(process, PAGE_SIZE)?,
                calls: 0,
            })
        }

        fn release(self) -> Result<(), &'static str> {
            for at in [self.strings, self.out] {
                let _ = memory::sys_munmap(self.process, at, PAGE_SIZE)
                    .map_err(|_| "munmap was refused")?;
            }
            Ok(())
        }

        /// Copy `bytes` into the program at `at`.
        fn stage(&self, at: u64, bytes: &[u8]) -> Result<u64, &'static str> {
            uaccess::copy_to_user(self.process.space(), at, bytes)
                .map_err(|_| "could not stage an argument")?;
            Ok(at)
        }

        /// A path argument, NUL-terminated.
        fn path(&self, text: &[u8]) -> Result<u64, &'static str> {
            let mut string = Vec::from(text);
            string.push(0);
            self.stage(self.strings, &string)
        }

        /// A second path argument, alongside [`Paths::path`]'s.
        fn second(&self, text: &[u8]) -> Result<u64, &'static str> {
            let mut string = Vec::from(text);
            string.push(0);
            self.stage(self.strings + SECOND, &string)
        }

        /// Make `call` as a program on this architecture would.
        fn call(&mut self, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
            self.calls = self.calls.saturating_add(1);
            super::call_by_number(self.process, call, args)
        }

        /// Fill the start of the output page with `byte`.
        fn fill_out(&self, len: usize, byte: u8) -> Result<(), &'static str> {
            let _ = self.stage(self.out, &vec![byte; len])?;
            Ok(())
        }

        /// The start of the output page.
        fn read_out(&self, len: usize) -> Result<Vec<u8>, &'static str> {
            let mut bytes = vec![0_u8; len];
            uaccess::copy_from_user(self.process.space(), self.out, &mut bytes)
                .map_err(|_| "could not read a result back")?;
            Ok(bytes)
        }
    }

    /// The first of `calls` this architecture has a number for.
    fn first_of(calls: &[Syscall]) -> Option<Syscall> {
        calls
            .iter()
            .copied()
            .find(|&call| number_for(call).is_some())
    }

    /// `newfstatat`, or `fstatat64` where that is the only form.
    fn fstatat() -> Result<Syscall, &'static str> {
        first_of(&[Syscall::Newfstatat, Syscall::Fstatat64])
            .ok_or("no fstatat on this architecture")
    }

    /// Open `path` and install it in the process's descriptor table.
    ///
    /// Directly rather than through `openat`, which belongs to the descriptor
    /// calls: what is checked here is the calls that take the descriptor.
    fn install(process: &Process, path: &[u8], directory: bool) -> Result<u64, &'static str> {
        let ns = fs::namespace();
        let flags = OpenFlags {
            read: true,
            directory,
            ..OpenFlags::default()
        };
        let file = ns
            .open(&ns.context(), None, path, &flags, 0)
            .map_err(|_| "could not open a file for a descriptor check")?;
        let fd = process
            .files()
            .lock()
            .insert(file, false)
            .map_err(|_| "the descriptor table was full")?;
        u64::try_from(fd).map_err(|_| "a negative descriptor")
    }

    /// Take a descriptor [`install`] made out of the table again.
    fn uninstall(process: &Process, fd: u64) -> Result<(), &'static str> {
        let fd = i32::try_from(fd).map_err(|_| "an impossible descriptor")?;
        let file = process.files().lock().remove(fd);
        file.map(drop).map_err(|_| "a descriptor vanished")
    }

    // -- stat records -------------------------------------------------------

    /// The fields of a `stat` record the checks compare.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Decoded {
        dev: u64,
        ino: u64,
        mode: u32,
        nlink: u64,
        uid: u64,
        size: u64,
        mtime: u64,
        rdev: u64,
    }

    /// An unsigned little-endian field of `width` bytes.
    fn le(bytes: &[u8], at: usize, width: usize) -> Option<u64> {
        let mut word = [0_u8; 8];
        word.get_mut(..width)?
            .copy_from_slice(bytes.get(at..at.checked_add(width)?)?);
        Some(u64::from_le_bytes(word))
    }

    /// Read a record back the way a C library would, at the offsets the
    /// `src/lib/proto/linux-abi` structure gives -- a second reading of the layout,
    /// independent of the encoder that wrote it.
    fn decode(layout: StatLayout, b: &[u8]) -> Option<Decoded> {
        use types::aarch64::Stat as Generic;
        use types::arm::Stat64;
        use types::x86_64::Stat as Legacy;
        Some(match layout {
            StatLayout::Legacy => Decoded {
                dev: le(b, offset_of!(Legacy, st_dev), 8)?,
                ino: le(b, offset_of!(Legacy, st_ino), 8)?,
                mode: u32::try_from(le(b, offset_of!(Legacy, st_mode), 4)?).ok()?,
                nlink: le(b, offset_of!(Legacy, st_nlink), 8)?,
                uid: le(b, offset_of!(Legacy, st_uid), 4)?,
                size: le(b, offset_of!(Legacy, st_size), 8)?,
                mtime: le(b, offset_of!(Legacy, st_mtime), 8)?,
                rdev: le(b, offset_of!(Legacy, st_rdev), 8)?,
            },
            StatLayout::Generic => Decoded {
                dev: le(b, offset_of!(Generic, st_dev), 8)?,
                ino: le(b, offset_of!(Generic, st_ino), 8)?,
                mode: u32::try_from(le(b, offset_of!(Generic, st_mode), 4)?).ok()?,
                nlink: le(b, offset_of!(Generic, st_nlink), 4)?,
                uid: le(b, offset_of!(Generic, st_uid), 4)?,
                size: le(b, offset_of!(Generic, st_size), 8)?,
                mtime: le(b, offset_of!(Generic, st_mtime), 8)?,
                rdev: le(b, offset_of!(Generic, st_rdev), 8)?,
            },
            StatLayout::Stat64 => Decoded {
                dev: le(b, offset_of!(Stat64, st_dev), 8)?,
                ino: le(b, offset_of!(Stat64, st_ino), 8)?,
                mode: u32::try_from(le(b, offset_of!(Stat64, st_mode), 4)?).ok()?,
                nlink: le(b, offset_of!(Stat64, st_nlink), 4)?,
                uid: le(b, offset_of!(Stat64, st_uid), 4)?,
                size: le(b, offset_of!(Stat64, st_size), 8)?,
                mtime: le(b, offset_of!(Stat64, st_mtime), 4)?,
                rdev: le(b, offset_of!(Stat64, st_rdev), 8)?,
            },
        })
    }

    /// Every encoder puts every field where its layout says -- all three, on
    /// every architecture, not only the one this build answers with.
    ///
    /// The size is over four gibibytes and the inode number over 32 bits, so
    /// a field written at half its width, or a layout that truncates the size,
    /// cannot pass.
    fn check_every_encoder_round_trips() -> Result<(), &'static str> {
        let time = |tv_sec| Timespec { tv_sec, tv_nsec: 5 };
        let stat = Stat {
            dev: 0x0803,
            metadata: Metadata {
                ino: 0x1_0000_0042,
                kind: FileType::Regular,
                permissions: 0o640,
                nlink: 3,
                uid: 7,
                gid: 9,
                size: 0x1_2345_6789,
                rdev: 0x0105,
                blocks: 11,
                block_size: 4096,
                atime: time(1000),
                mtime: time(2000),
                ctime: time(3000),
            },
        };
        let want = Decoded {
            dev: 0x0803,
            ino: 0x1_0000_0042,
            mode: S_IFREG | 0o640,
            nlink: 3,
            uid: 7,
            size: 0x1_2345_6789,
            mtime: 2000,
            rdev: 0x0105,
        };
        for layout in [StatLayout::Legacy, StatLayout::Generic, StatLayout::Stat64] {
            let bytes = layout.encode(&stat);
            if bytes.len() != layout.size() {
                return Err("a stat record is not the size of its layout");
            }
            if decode(layout, &bytes) != Some(want) {
                return Err("a stat encoder put a field where its layout does not");
            }
        }
        // `stat64`'s other inode field: the low half, for old readers.
        let stat64 = StatLayout::Stat64.encode(&stat);
        if le(&stat64, offset_of!(types::arm::Stat64, __st_ino), 4) != Some(0x42) {
            return Err("stat64's truncated inode field is not the low half");
        }
        Ok(())
    }

    /// Make `call` fill the output page with a record, and decode it --
    /// checking that it wrote exactly its layout's size and not a byte more.
    fn stat_into(
        p: &mut Paths<'_>,
        call: Syscall,
        args: [u64; 6],
    ) -> Result<Decoded, &'static str> {
        const SENTINEL: u8 = 0xA5;
        let size = arch::STAT_LAYOUT.size();
        p.fill_out(size + 8, SENTINEL)?;
        if p.call(call, args) != Ok(0) {
            return Err("a stat call was refused");
        }
        let bytes = p.read_out(size + 8)?;
        if bytes.get(size..) != Some(&[SENTINEL; 8][..]) {
            return Err("a stat call wrote past the end of its layout");
        }
        if bytes.get(size - 1) == Some(&SENTINEL) {
            return Err("a stat call did not write to the end of its layout");
        }
        decode(arch::STAT_LAYOUT, &bytes).ok_or("a stat record could not be decoded")
    }

    // -- the checks ---------------------------------------------------------

    /// `mkdirat`, `mknodat` and `symlinkat` make names, and `readlinkat` reads
    /// a link back unterminated and cut silently to the buffer.
    fn check_names_are_made_and_read(p: &mut Paths<'_>) -> Result<(), &'static str> {
        let root = p.path(ROOT)?;
        if p.call(Syscall::Mkdirat, [CWD, root, 0o777, 0, 0, 0]) != Ok(0) {
            return Err("mkdirat under /tmp was refused");
        }
        if p.call(Syscall::Mkdirat, [CWD, root, 0o777, 0, 0, 0]) != Err(Errno::EEXIST) {
            return Err("mkdirat over an existing name was not EEXIST");
        }
        // The file the stat checks describe, and the one the link is renamed
        // over: a rename onto an existing name, which is what `mv` does to a
        // file it replaces.
        let regular = u64::from(S_IFREG | 0o666);
        for name in [&b"/tmp/pathcheck/file"[..], b"/tmp/pathcheck/moved"] {
            let name = p.path(name)?;
            if p.call(Syscall::Mknodat, [CWD, name, regular, 0, 0, 0]) != Ok(0) {
                return Err("mknodat of a regular file was refused");
            }
        }
        let target = p.path(TARGET)?;
        let link = p.second(LINK)?;
        if p.call(Syscall::Symlinkat, [target, CWD, link, 0, 0, 0]) != Ok(0) {
            return Err("symlinkat was refused");
        }

        let link = p.path(LINK)?;
        p.fill_out(64, 0xEE)?;
        if p.call(Syscall::Readlinkat, [CWD, link, p.out, 64, 0, 0]) != Ok(TARGET.len()) {
            return Err("readlinkat did not report the target's length");
        }
        let read = p.read_out(TARGET.len() + 1)?;
        if read.get(..TARGET.len()) != Some(TARGET) || read.get(TARGET.len()) != Some(&0xEE) {
            return Err("readlinkat did not return the target, unterminated");
        }
        if p.call(Syscall::Readlinkat, [CWD, link, p.out, 4, 0, 0]) != Ok(4) {
            return Err("readlinkat did not cut the target to the buffer");
        }
        if p.call(Syscall::Readlinkat, [CWD, link, p.out, 0, 0, 0]) != Err(Errno::EINVAL) {
            return Err("readlinkat with no room was not EINVAL");
        }
        if p.call(Syscall::Readlinkat, [CWD, root, p.out, 64, 0, 0]) != Err(Errno::EINVAL) {
            // `root` still points at the first string slot, now the link's path,
            // so restage the directory before asking.
            let root = p.path(ROOT)?;
            if p.call(Syscall::Readlinkat, [CWD, root, p.out, 64, 0, 0]) != Err(Errno::EINVAL) {
                return Err("readlinkat of a directory was not EINVAL");
            }
        }
        Ok(())
    }

    /// `creat` is `open` with `O_CREAT | O_WRONLY | O_TRUNC`: a file that held
    /// bytes is empty after it, and the descriptor it returns writes and does
    /// not read. Where the architecture has no number for it -- AArch64 --
    /// the call is `ENOSYS`, as a program there would find.
    ///
    /// Chrome's headless shell writes its screenshot with `creat`, and was
    /// told `ENOSYS` before this.
    fn check_creat_truncates_and_opens_for_writing(p: &mut Paths<'_>) -> Result<(), &'static str> {
        use ferrix_linux_abi::types::{F_GETFL, O_ACCMODE, O_WRONLY, SEEK_END};
        let file = p.path(b"/tmp/pathcheck/file")?;
        if number_for(Syscall::Creat).is_none() {
            return if p.call(Syscall::Creat, [file, 0o644, 0, 0, 0, 0]) == Err(Errno::ENOSYS) {
                Ok(())
            } else {
                Err("creat answered on an architecture that has no number for it")
            };
        }
        let fd = p
            .call(Syscall::Openat, [CWD, file, u64::from(O_WRONLY), 0, 0, 0])
            .map_err(|_| "the file creat is checked on did not open")?;
        let fd = fd as u64;
        p.fill_out(4, b'x')?;
        let wrote = p.call(Syscall::Write, [fd, p.out, 4, 0, 0, 0]);
        let _ = p.call(Syscall::Close, [fd, 0, 0, 0, 0, 0]);
        if wrote != Ok(4) {
            return Err("a write to the file creat is checked on was short");
        }
        let fd = p
            .call(Syscall::Creat, [file, 0o644, 0, 0, 0, 0])
            .map_err(|_| "creat of an existing file was refused")? as u64;
        let mode = p.call(Syscall::Fcntl, [fd, u64::from(F_GETFL), 0, 0, 0, 0]);
        let end = p.call(Syscall::Lseek, [fd, 0, u64::from(SEEK_END), 0, 0, 0]);
        let _ = p.call(Syscall::Close, [fd, 0, 0, 0, 0, 0]);
        if mode.map(|flags| flags as u32 & O_ACCMODE) != Ok(O_WRONLY) {
            return Err("creat did not open its file for writing only");
        }
        if end != Ok(0) {
            return Err("creat did not truncate the file it opened");
        }
        Ok(())
    }

    /// `renameat2` moves a name across directories and over an existing file,
    /// refuses to replace one under `RENAME_NOREPLACE`, and refuses
    /// `RENAME_EXCHANGE` outright.
    fn check_renames_replace_only_when_allowed(p: &mut Paths<'_>) -> Result<(), &'static str> {
        let from = p.path(LINK)?;
        let to = p.second(b"/tmp/pathcheck/moved")?;
        if p.call(Syscall::Renameat2, [CWD, from, CWD, to, 0, 0]) != Ok(0) {
            return Err("renameat2 over an existing file was refused");
        }
        if p.call(Syscall::Readlinkat, [CWD, from, p.out, 64, 0, 0]) != Err(Errno::ENOENT) {
            return Err("a name survived being renamed away");
        }
        let to = p.path(b"/tmp/pathcheck/moved")?;
        if p.call(Syscall::Readlinkat, [CWD, to, p.out, 64, 0, 0]) != Ok(TARGET.len()) {
            return Err("the renamed link is not where it was renamed to");
        }
        let from = p.path(b"/tmp/pathcheck/moved")?;
        let to = p.second(b"/tmp/pathcheck/file")?;
        let noreplace = u64::from(RENAME_NOREPLACE);
        if p.call(Syscall::Renameat2, [CWD, from, CWD, to, noreplace, 0]) != Err(Errno::EEXIST) {
            return Err("RENAME_NOREPLACE replaced a name");
        }
        let exchange = u64::from(RENAME_EXCHANGE);
        if p.call(Syscall::Renameat2, [CWD, from, CWD, to, exchange, 0]) != Err(Errno::EINVAL) {
            return Err("RENAME_EXCHANGE was not EINVAL");
        }
        Ok(())
    }

    /// Every form of `stat` this architecture has describes the same file the
    /// same way, and `statx` agrees with them.
    fn check_every_stat_describes_the_same_file(p: &mut Paths<'_>) -> Result<(), &'static str> {
        let at = fstatat()?;
        let file = p.path(b"/tmp/pathcheck/file")?;
        let by_path = stat_into(p, at, [CWD, file, p.out, 0, 0, 0])?;
        // 0o666 through the default umask of 0o022.
        if by_path.mode != S_IFREG | 0o644 || by_path.nlink != 1 || by_path.size != 0 {
            return Err("fstatat did not describe a new file with the umask applied");
        }

        let link = p.path(b"/tmp/pathcheck/moved")?;
        let nofollow = u64::from(AT_SYMLINK_NOFOLLOW);
        let as_link = stat_into(p, at, [CWD, link, p.out, nofollow, 0, 0])?;
        if as_link.mode != S_IFLNK | 0o777 || as_link.size != TARGET.len() as u64 {
            return Err("AT_SYMLINK_NOFOLLOW did not describe the link itself");
        }
        if p.call(at, [CWD, link, p.out, 0, 0, 0]) != Err(Errno::ENOENT) {
            return Err("following a dangling link found something");
        }
        if let Some(lstat) = first_of(&[Syscall::Lstat, Syscall::Lstat64])
            && stat_into(p, lstat, [link, p.out, 0, 0, 0, 0])? != as_link
        {
            return Err("lstat and fstatat disagree about a link");
        }

        let fd = install(p.process, b"/tmp/pathcheck/file", false)?;
        let fstat = first_of(&[Syscall::Fstat, Syscall::Fstat64]).ok_or("no fstat here")?;
        let by_fd = stat_into(p, fstat, [fd, p.out, 0, 0, 0, 0]);
        let empty = p.path(b"")?;
        let empty_path = u64::from(AT_EMPTY_PATH);
        let by_empty = stat_into(p, at, [fd, empty, p.out, empty_path, 0, 0]);
        uninstall(p.process, fd)?;
        if by_fd? != by_path || by_empty? != by_path {
            return Err("fstat, AT_EMPTY_PATH and a path disagree about one file");
        }

        let statx_size = size_of::<Statx>();
        p.fill_out(statx_size, 0xA5)?;
        let basic = u64::from(STATX_BASIC_STATS);
        let link = p.path(b"/tmp/pathcheck/moved")?;
        if p.call(Syscall::Statx, [CWD, link, nofollow, basic, p.out, 0]) != Ok(0) {
            return Err("statx was refused");
        }
        let record = p.read_out(statx_size)?;
        let field = |at, width| le(&record, at, width).unwrap_or(u64::MAX);
        if field(offset_of!(Statx, stx_mask), 4) & basic != basic
            || field(offset_of!(Statx, stx_ino), 8) != as_link.ino
            || field(offset_of!(Statx, stx_size), 8) != as_link.size
            || field(offset_of!(Statx, stx_mode), 2) != u64::from(as_link.mode)
        {
            return Err("statx disagrees with fstatat about a link");
        }
        Ok(())
    }

    /// The path of the listing check's `index`th name, `e00` to `e39`.
    fn entry(index: usize) -> Vec<u8> {
        let mut path = Vec::from(&b"/tmp/pathcheck/many/e"[..]);
        path.extend_from_slice(&[b'0' + (index / 10) as u8, b'0' + (index % 10) as u8]);
        path
    }

    /// `getdents64` over forty names with room for four at a time reports
    /// every name exactly once, and `EINVAL` when there is no room for one.
    fn check_a_listing_in_pieces_sees_each_name_once(
        p: &mut Paths<'_>,
    ) -> Result<(usize, u32), &'static str> {
        let dir = p.path(b"/tmp/pathcheck/many")?;
        if p.call(Syscall::Mkdirat, [CWD, dir, 0o755, 0, 0, 0]) != Ok(0) {
            return Err("mkdirat of the listing directory was refused");
        }
        for index in 0..LISTED {
            let name = p.path(&entry(index))?;
            if p.call(
                Syscall::Mknodat,
                [CWD, name, u64::from(S_IFREG | 0o600), 0, 0, 0],
            ) != Ok(0)
            {
                return Err("mknodat in the listing directory was refused");
            }
        }

        let fd = install(p.process, b"/tmp/pathcheck/many", true)?;
        let listing = list(p, fd);
        uninstall(p.process, fd)?;
        let (seen, dots, listed, calls) = listing?;
        if dots != 2 || seen.iter().any(|&count| count != 1) || listed != LISTED + 2 {
            return Err("getdents64 did not report every name exactly once");
        }
        if calls < 3 {
            return Err("the listing was not split across calls");
        }
        Ok((listed, calls))
    }

    /// Read the directory open on `fd` to its end.
    fn list(p: &mut Paths<'_>, fd: u64) -> Result<([u8; LISTED], usize, usize, u32), &'static str> {
        if p.call(Syscall::Getdents64, [fd, p.out, 16, 0, 0, 0]) != Err(Errno::EINVAL) {
            return Err("getdents64 with no room for an entry was not EINVAL");
        }
        let (mut seen, mut dots, mut listed, mut calls) = ([0_u8; LISTED], 0, 0, 0_u32);
        loop {
            calls += 1;
            if calls > 64 {
                return Err("getdents64 never reached the end of the directory");
            }
            let used = p
                .call(Syscall::Getdents64, [fd, p.out, LISTING_BUFFER, 0, 0, 0])
                .map_err(|_| "getdents64 was refused")?;
            if used == 0 {
                return Ok((seen, dots, listed, calls));
            }
            for record in dirent::records(&p.read_out(used)?) {
                listed += 1;
                tally(&record, &mut seen, &mut dots)?;
            }
        }
    }

    /// Count one listed name.
    fn tally(
        record: &Record<'_>,
        seen: &mut [u8; LISTED],
        dots: &mut usize,
    ) -> Result<(), &'static str> {
        let [b'e', tens, ones] = *record.name else {
            if record.name == b"." || record.name == b".." {
                *dots += 1;
                return Ok(());
            }
            return Err("getdents64 reported a name nobody made");
        };
        let index =
            usize::from(tens.wrapping_sub(b'0')) * 10 + usize::from(ones.wrapping_sub(b'0'));
        let count = seen
            .get_mut(index)
            .ok_or("getdents64 reported a name nobody made")?;
        *count = count.saturating_add(1);
        if record.kind != DT_REG {
            return Err("getdents64 reported a regular file as something else");
        }
        Ok(())
    }

    /// `getcwd` reports exactly `want`, terminated, and counts the terminator.
    fn expect_cwd(p: &mut Paths<'_>, want: &[u8]) -> Result<(), &'static str> {
        let len = p
            .call(Syscall::Getcwd, [p.out, 256, 0, 0, 0, 0])
            .map_err(|_| "getcwd was refused")?;
        let got = p.read_out(len)?;
        if len != want.len() + 1 || got.get(..want.len()) != Some(want) || got.last() != Some(&0) {
            return Err("getcwd did not report the directory chdir chose");
        }
        Ok(())
    }

    /// `chdir`, `fchdir` and a relative path agree about where the process is,
    /// and `getcwd` says `ERANGE` for a short buffer and `ENOENT` once the
    /// directory is gone.
    fn check_the_working_directory_follows_chdir(p: &mut Paths<'_>) -> Result<(), &'static str> {
        let many = p.path(b"/tmp/pathcheck/many")?;
        if p.call(Syscall::Chdir, [many, 0, 0, 0, 0, 0]) != Ok(0) {
            return Err("chdir was refused");
        }
        expect_cwd(p, b"/tmp/pathcheck/many")?;
        // One byte short: the terminator counts.
        if p.call(Syscall::Getcwd, [p.out, 19, 0, 0, 0, 0]) != Err(Errno::ERANGE) {
            return Err("getcwd into a buffer one byte short was not ERANGE");
        }

        let sub = p.path(b"sub")?;
        if p.call(Syscall::Mkdirat, [CWD, sub, 0o755, 0, 0, 0]) != Ok(0)
            || p.call(Syscall::Chdir, [sub, 0, 0, 0, 0, 0]) != Ok(0)
        {
            return Err("a relative mkdirat or chdir was refused");
        }
        expect_cwd(p, b"/tmp/pathcheck/many/sub")?;
        let gone = p.path(b"/tmp/pathcheck/many/sub")?;
        if p.call(
            Syscall::Unlinkat,
            [CWD, gone, u64::from(AT_REMOVEDIR), 0, 0, 0],
        ) != Ok(0)
        {
            return Err("rmdir of the working directory was refused");
        }
        if p.call(Syscall::Getcwd, [p.out, 256, 0, 0, 0, 0]) != Err(Errno::ENOENT) {
            return Err("getcwd in a removed directory was not ENOENT");
        }

        let fd = install(p.process, ROOT, true)?;
        let moved = p.call(Syscall::Fchdir, [fd, 0, 0, 0, 0, 0]);
        uninstall(p.process, fd)?;
        if moved != Ok(0) {
            return Err("fchdir was refused");
        }
        expect_cwd(p, ROOT)?;
        let slash = p.path(b"/")?;
        if p.call(Syscall::Chdir, [slash, 0, 0, 0, 0, 0]) != Ok(0) {
            return Err("chdir to the root was refused");
        }
        expect_cwd(p, b"/")
    }

    /// Two `timespec`s at this architecture's `long` width.
    fn timespecs(values: [i64; 4]) -> Vec<u8> {
        let width = size_of::<usize>();
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend_from_slice(value.to_le_bytes().get(..width).unwrap_or(&[]));
        }
        bytes
    }

    /// `faccessat` answers as root does, and `chmod`, `chown`, `utimensat`
    /// and `umask` change what they say they change.
    fn check_access_and_attributes(p: &mut Paths<'_>) -> Result<(), &'static str> {
        let file = p.path(b"/tmp/pathcheck/file")?;
        if p.call(
            Syscall::Faccessat,
            [CWD, file, u64::from(R_OK | W_OK), 0, 0, 0],
        ) != Ok(0)
        {
            return Err("root could not read and write a file");
        }
        if p.call(Syscall::Faccessat, [CWD, file, u64::from(X_OK), 0, 0, 0]) != Err(Errno::EACCES) {
            return Err("a file with no execute bit passed X_OK");
        }
        if p.call(Syscall::Fchmodat, [CWD, file, 0o755, 0, 0, 0]) != Ok(0)
            || p.call(Syscall::Faccessat2, [CWD, file, u64::from(X_OK), 0, 0, 0]) != Ok(0)
        {
            return Err("chmod did not make a file executable");
        }
        if p.call(Syscall::Fchownat, [CWD, file, 7, u64::from(u32::MAX), 0, 0]) != Ok(0) {
            return Err("fchownat was refused");
        }
        let times = p.stage(p.strings + SECOND, &timespecs([1, UTIME_OMIT, 1234, 0]))?;
        if p.call(Syscall::Utimensat, [CWD, file, times, 0, 0, 0]) != Ok(0) {
            return Err("utimensat was refused");
        }
        let after = stat_into(p, fstatat()?, [CWD, file, p.out, 0, 0, 0])?;
        if after.mode != S_IFREG | 0o755 || after.uid != 7 || after.mtime != 1234 {
            return Err("chmod, chown or utimensat did not show in stat");
        }
        if p.call(Syscall::Umask, [0o7077, 0, 0, 0, 0, 0]) != Ok(0o022)
            || p.call(Syscall::Umask, [0o022, 0, 0, 0, 0, 0]) != Ok(0o077)
        {
            return Err("umask did not swap the mask, keeping permission bits only");
        }
        Ok(())
    }

    /// A device node and the number `mknodat` was given for it.
    struct DeviceNode {
        path: &'static [u8],
        kind: u32,
        major: u32,
        minor: u32,
    }

    /// A node with `/dev/null`'s number.
    const NULL_NODE: DeviceNode = DeviceNode {
        path: b"/tmp/pathcheck/null",
        kind: S_IFCHR,
        major: 1,
        minor: 3,
    };

    /// A node with `/dev/zero`'s number.
    const ZERO_NODE: DeviceNode = DeviceNode {
        path: b"/tmp/pathcheck/zero",
        kind: S_IFCHR,
        major: 1,
        minor: 5,
    };

    /// Nodes nothing answers: a character number devfs does not have, and a
    /// block device.
    const UNANSWERED_NODES: [DeviceNode; 2] = [
        DeviceNode {
            path: b"/tmp/pathcheck/unregistered",
            kind: S_IFCHR,
            major: 240,
            minor: 0,
        },
        DeviceNode {
            path: b"/tmp/pathcheck/disk",
            kind: S_IFBLK,
            major: 8,
            minor: 0,
        },
    ];

    /// `mknodat` makes character and block device nodes with the number it
    /// was given and the umask applied; a character node opens as the devfs
    /// device with its number, whatever filesystem it is on; a character
    /// number devfs does not have, and a block number no disk has, is `ENXIO`
    /// on open but not with `O_PATH`; and a directory is still `EPERM`.
    /// Returns the nodes made.
    fn check_device_nodes_open_by_number(p: &mut Paths<'_>) -> Result<usize, &'static str> {
        let directory = p.path(b"/tmp/pathcheck/dir")?;
        if p.call(
            Syscall::Mknodat,
            [CWD, directory, u64::from(S_IFDIR | 0o755), 0, 0, 0],
        ) != Err(Errno::EPERM)
        {
            return Err("mknodat of a directory was not EPERM");
        }
        let every_node = || {
            [&NULL_NODE, &ZERO_NODE]
                .into_iter()
                .chain(&UNANSWERED_NODES)
        };
        for node in every_node() {
            let name = p.path(node.path)?;
            // Bits above the low 32 are set, and must be ignored: Linux takes
            // `dev` as an `unsigned int`.
            let dev = makedev(node.major, node.minor) | 0xDEAD_0000_0000_0000;
            let mode = u64::from(node.kind | 0o666);
            if p.call(Syscall::Mknodat, [CWD, name, mode, dev, 0, 0]) != Ok(0) {
                return Err("mknodat of a device node was refused");
            }
        }

        let null = p.path(NULL_NODE.path)?;
        let described = stat_into(p, fstatat()?, [CWD, null, p.out, 0, 0, 0])?;
        // 0o666 through the default umask of 0o022.
        if described.mode != S_IFCHR | 0o644 || described.rdev != makedev(1, 3) {
            return Err(
                "stat of a character node did not report S_IFCHR, its number and the umask",
            );
        }
        let fd = p
            .call(Syscall::Openat, [CWD, null, u64::from(O_WRONLY), 0, 0, 0])
            .map_err(|_| "a character node devfs has a number for did not open")?;
        let fd = fd as u64;
        let _ = p.stage(p.out, b"swallowed")?;
        let wrote = p.call(Syscall::Write, [fd, p.out, 9, 0, 0, 0]);
        let fstat = first_of(&[Syscall::Fstat, Syscall::Fstat64]).ok_or("no fstat here")?;
        let by_fd = stat_into(p, fstat, [fd, p.out, 0, 0, 0, 0]);
        if p.call(Syscall::Close, [fd, 0, 0, 0, 0, 0]) != Ok(0) {
            return Err("close of a device node's descriptor was refused");
        }
        if wrote != Ok(9) {
            return Err("a node numbered 1:3 did not swallow a write as /dev/null does");
        }
        if by_fd? != described {
            return Err("fstat of an opened node did not describe the node itself");
        }

        let zero = p.path(ZERO_NODE.path)?;
        let fd = p
            .call(Syscall::Openat, [CWD, zero, u64::from(O_RDONLY), 0, 0, 0])
            .map_err(|_| "a character node devfs has a number for did not open")?;
        let fd = fd as u64;
        p.fill_out(64, 0xA5)?;
        let read = p.call(Syscall::Read, [fd, p.out, 64, 0, 0, 0]);
        if p.call(Syscall::Close, [fd, 0, 0, 0, 0, 0]) != Ok(0) {
            return Err("close of a device node's descriptor was refused");
        }
        if read != Ok(64) || p.read_out(64)?.iter().any(|&byte| byte != 0) {
            return Err("a node numbered 1:5 did not read as zeros as /dev/zero does");
        }
        if readv_of_two(p, ZERO_NODE.path)? != readv_of_two(p, b"/dev/zero")? {
            return Err(
                "readv of a node numbered 1:5 did not stop where /dev/zero's does: it read the \
                 node as a file with a position rather than as the device's stream",
            );
        }
        check_a_write_of_nothing_asks_a_memory_device(p)?;

        for node in &UNANSWERED_NODES {
            let name = p.path(node.path)?;
            let described = stat_into(p, fstatat()?, [CWD, name, p.out, 0, 0, 0])?;
            if described.mode != node.kind | 0o644
                || described.rdev != makedev(node.major, node.minor)
            {
                return Err("stat of a device node did not report its kind and number");
            }
            if p.call(Syscall::Openat, [CWD, name, u64::from(O_RDONLY), 0, 0, 0])
                != Err(Errno::ENXIO)
            {
                return Err("a device node with no device behind it did not open ENXIO");
            }
            let handle = p
                .call(Syscall::Openat, [CWD, name, u64::from(O_PATH), 0, 0, 0])
                .map_err(|_| "O_PATH of a device node with no device behind it was refused")?;
            if p.call(Syscall::Close, [handle as u64, 0, 0, 0, 0, 0]) != Ok(0) {
                return Err("close of an O_PATH descriptor was refused");
            }
        }

        let mut made = 0;
        for node in every_node() {
            let name = p.path(node.path)?;
            if p.call(Syscall::Unlinkat, [CWD, name, 0, 0, 0, 0]) != Ok(0) {
                return Err("unlinkat of a device node was refused");
            }
            made += 1;
        }
        Ok(made)
    }

    /// What `readv` of two eight-byte segments takes from the character device
    /// at `path`.
    ///
    /// Whether it goes on into the second segment is the read loop asking
    /// whether the file is a stream, which a stream answers no to by stopping
    /// at the first segment that took anything. That question is asked of what
    /// reads go to, so a node `mknodat` made on tmpfs, which is no stream
    /// itself, answers as the device its number names does. It was once asked
    /// of the node: `readv` read on into the second segment, and of a FIFO or
    /// a terminal, whose next read waits, it would have waited there.
    fn readv_of_two(p: &mut Paths<'_>, path: &[u8]) -> Result<usize, &'static str> {
        let name = p.path(path)?;
        let fd = p
            .call(Syscall::Openat, [CWD, name, u64::from(O_RDONLY), 0, 0, 0])
            .map_err(|_| "a character device did not open for readv")? as u64;
        let word = size_of::<usize>() as u64;
        let array = p.out + PAGE_SIZE / 2;
        let segments = [p.out, 8, p.out + 8, 8];
        for (index, value) in (0_u64..).zip(segments) {
            super::write_word(p.process, array + index * word, value)?;
        }
        let took = p.call(Syscall::Readv, [fd, array, 2, 0, 0, 0]);
        if p.call(Syscall::Close, [fd, 0, 0, 0, 0, 0]) != Ok(0) {
            return Err("close of a device node's descriptor was refused");
        }
        took.map_err(|_| "readv of a character device was refused")
    }

    /// A write of nothing reaches a memory device, as it does on Linux, where
    /// the device answers for itself: `write` and `pwrite64` of nothing are
    /// ENOSPC on /dev/full and 0 on a node numbered as /dev/null, and
    /// `writev` of an empty segment asks no file, so is 0 on /dev/full too.
    /// Measured on a Linux 7.0 host.
    fn check_a_write_of_nothing_asks_a_memory_device(
        p: &mut Paths<'_>,
    ) -> Result<(), &'static str> {
        let word = size_of::<usize>() as u64;
        let array = p.out + PAGE_SIZE / 2;
        super::write_word(p.process, array, p.out)?;
        super::write_word(p.process, array + word, 0)?;
        for (path, answer) in [
            (&b"/dev/full"[..], Err(Errno::ENOSPC)),
            (NULL_NODE.path, Ok(0)),
        ] {
            let name = p.path(path)?;
            let fd = p
                .call(Syscall::Openat, [CWD, name, u64::from(O_WRONLY), 0, 0, 0])
                .map_err(|_| "a memory device did not open for writing")?
                as u64;
            let wrote = p.call(Syscall::Write, [fd, p.out, 0, 0, 0, 0]);
            let placed = p.call(Syscall::Pwrite64, [fd, p.out, 0, 0, 0, 0]);
            let gathered = p.call(Syscall::Writev, [fd, array, 1, 0, 0, 0]);
            if p.call(Syscall::Close, [fd, 0, 0, 0, 0, 0]) != Ok(0) {
                return Err("close of a memory device's descriptor was refused");
            }
            if wrote != answer || placed != answer {
                return Err(
                    "a write of nothing to a memory device did not answer as the device does",
                );
            }
            if gathered != Ok(0) {
                return Err("writev of an empty segment to a memory device was not 0");
            }
        }
        Ok(())
    }

    /// `unlinkat` removes names, with and without `AT_REMOVEDIR`, and refuses
    /// the wrong kind of each; nothing the checks made is left.
    fn check_names_are_removed(p: &mut Paths<'_>) -> Result<(), &'static str> {
        let removedir = u64::from(AT_REMOVEDIR);
        let many = p.path(b"/tmp/pathcheck/many")?;
        if p.call(Syscall::Unlinkat, [CWD, many, removedir, 0, 0, 0]) != Err(Errno::ENOTEMPTY) {
            return Err("rmdir of a directory with entries was not ENOTEMPTY");
        }
        if p.call(Syscall::Unlinkat, [CWD, many, 0, 0, 0, 0]) != Err(Errno::EISDIR) {
            return Err("unlink of a directory was not EISDIR");
        }
        for index in 0..LISTED {
            let name = p.path(&entry(index))?;
            if p.call(Syscall::Unlinkat, [CWD, name, 0, 0, 0, 0]) != Ok(0) {
                return Err("unlinkat of a listed file was refused");
            }
        }
        let many = p.path(b"/tmp/pathcheck/many")?;
        if p.call(Syscall::Unlinkat, [CWD, many, removedir, 0, 0, 0]) != Ok(0) {
            return Err("rmdir of an emptied directory was refused");
        }
        let file = p.path(b"/tmp/pathcheck/file")?;
        if p.call(Syscall::Unlinkat, [CWD, file, removedir, 0, 0, 0]) != Err(Errno::ENOTDIR) {
            return Err("rmdir of a file was not ENOTDIR");
        }
        for name in [&b"/tmp/pathcheck/file"[..], b"/tmp/pathcheck/moved"] {
            let name = p.path(name)?;
            if p.call(Syscall::Unlinkat, [CWD, name, 0, 0, 0, 0]) != Ok(0) {
                return Err("unlinkat of a file was refused");
            }
        }
        let root = p.path(ROOT)?;
        if p.call(Syscall::Unlinkat, [CWD, root, removedir, 0, 0, 0]) != Ok(0) {
            return Err("rmdir of the check's own directory was refused");
        }
        if p.call(fstatat()?, [CWD, root, p.out, 0, 0, 0]) != Err(Errno::ENOENT) {
            return Err("a removed directory could still be described");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Processes making processes
// ---------------------------------------------------------------------------

/// What [`arch::USER_FORK_PROGRAM`] exits with when everything is right.
const FORK_STATUS: i32 = 24;

/// A program forks, its child exits 23, the parent waits for it and exits with
/// the child's code plus one.
///
/// One number covers the whole path: the child resumed from a copy of its
/// parent's registers with the call returning zero, it ran in a copy of the
/// parent's memory and exited, the parent's `wait4` found that child and not
/// another, and the status word put the exit code in its second byte.
/// Verifies: `L.x86_64.63`
fn check_a_forked_child_is_waited_for() -> Result<Option<i32>, &'static str> {
    if arch::USER_FORK_PROGRAM.is_empty() {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_FORK_PROGRAM,
    );
    let status = exec::run(&file, &[b"/fork"], &[], [0x5a; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a program that forks could not be started")?;
    match status {
        FORK_STATUS => Ok(Some(status)),
        99 => Err("wait4 reported a child other than the one fork made"),
        _ => Err("a program that forks and waits did not see its child exit with 23"),
    }
}

/// What [`arch::USER_THREAD_PROGRAM`] exits with when everything is right.
const THREAD_STATUS: i32 = 42;

/// A program makes threads with `clone(CLONE_THREAD)` and exits with 42.
///
/// The number covers: `CLONE_VM` without a thread or `vfork` still refused; a
/// thread running in its process's memory, resumed from the caller's
/// registers on a stack of its own; `exit` ending that thread and not the
/// process; its tid written to `parent_tid`, differing from the pid, and
/// cleared from `child_tid` with a futex wake as it ended, which is what the
/// caller waits for; and, as the negative control, a thread made without
/// `CLONE_CHILD_CLEARTID` whose word is left alone, so a timed wait on it runs
/// out. The arguments are placed as each architecture's `clone` takes them.
fn check_a_thread_shares_its_process_and_ends_alone() -> Result<Option<i32>, &'static str> {
    if arch::USER_THREAD_PROGRAM.is_empty() {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_THREAD_PROGRAM,
    );
    let status = exec::run(
        &file,
        &[b"/threads"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that makes threads could not be started")?;
    match status {
        THREAD_STATUS => Ok(Some(status)),
        5 | 6 => Err("a thread's exit ended its whole process"),
        95 => Err("clone with CLONE_VM but neither CLONE_THREAD nor CLONE_VFORK was not ENOSYS"),
        96 => Err("a thread made without CLONE_CHILD_CLEARTID had its id word cleared"),
        97 => Err("a thread did not run in its process's memory"),
        98 => Err("clone did not write a thread's id to parent_tid"),
        99 => Err("a thread's id was its process's pid"),
        _ => Err("a program that makes threads did not exit as it should"),
    }
}

/// What [`arch::USER_NAMESPACE_PROGRAM`] exits with when both `clone` calls
/// were refused.
const NAMESPACE_STATUS: i32 = 44;

/// A program asking `clone` for a namespace is refused, whichever it asks for.
///
/// There are no namespaces here, and `unshare` has always said so. `clone` and
/// `clone3` did not: they never looked at the `CLONE_NEW*` bits, so a program
/// that asked for a sandbox got an ordinary child in the one namespace there
/// is, and no way to tell. The two calls now answer alike, with the `EINVAL` a
/// Linux built without `CONFIG_*_NS` answers. The program's two calls name
/// every namespace flag `clone` can reach between them; see
/// [`arch::USER_NAMESPACE_PROGRAM`] for why `CLONE_NEWTIME` is not among them.
fn check_clone_refuses_every_namespace() -> Result<(), &'static str> {
    if arch::USER_NAMESPACE_PROGRAM.is_empty() {
        return Ok(());
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_NAMESPACE_PROGRAM,
    );
    let status = exec::run(
        &file,
        &[b"/namespaces"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that asks clone for namespaces could not be started")?;
    match status {
        NAMESPACE_STATUS => Ok(()),
        98 => Err("clone with CLONE_NEWUSER, CLONE_NEWPID and CLONE_NEWNS was not refused"),
        97 => Err(
            "clone with CLONE_NEWCGROUP, CLONE_NEWUTS, CLONE_NEWIPC and CLONE_NEWNET was not refused",
        ),
        _ => Err("a program that asks clone for namespaces did not exit as it should"),
    }
}

/// Runs of [`arch::USER_EXITS_PROGRAM`].
const EXITS_RUNS: u32 = 16;

/// What [`arch::USER_EXITS_PROGRAM`]'s first thread exits with, and so its
/// process; its other thread exits with 9.
const EXITS_STATUS: i32 = 7;

/// A program whose last two threads call `exit` at the same instant ends, with
/// its first thread's status and not the last thread's, every time.
///
/// The two threads meet on a spin, each on its own processor, and exit with
/// no other call between: the thread with 9, the main thread with 7. That the
/// process ends at all, and with 7, is what this shows. It does not show the
/// race the end of a thread was built against -- two last threads each seeing
/// the other still there -- because that window is narrower than the two can
/// be made to hit: a negative control with the old decision put back passed
/// all sixteen runs. The fix rests on its review, not on this check. Skipped on
/// one processor; each run waits with a deadline, so a process that never ends
/// fails here by name rather than stopping the boot.
fn check_two_last_threads_exiting_together_end_their_process() -> Result<Option<u32>, &'static str>
{
    if arch::USER_EXITS_PROGRAM.is_empty() || crate::smp::count() < 2 {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_EXITS_PROGRAM,
    );
    for _ in 0..EXITS_RUNS {
        let racer = process::load(
            &file,
            &[b"/exits"],
            &[],
            [0x5a; ferrix_ustack::RANDOM_BYTES],
        )
        .map_err(|_| "a program whose threads exit together could not be loaded")?;
        let _task = process::start(&racer)
            .map_err(|_| "a program whose threads exit together could not be started")?;
        let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
        match racer.wait_for_exit(deadline) {
            Some(EXITS_STATUS) => {}
            Some(_) => {
                return Err(
                    "a process whose last two threads called exit together ended with another \
                     status than its first thread's",
                );
            }
            None => {
                return Err("a process whose last two threads called exit together never ended");
            }
        }
    }
    Ok(Some(EXITS_RUNS))
}

/// The status [`check_a_reader_blocked_in_syslog_is_released_by_a_kill`] kills
/// with: not 128 plus a signal, so the kill posts none.
const SYSLOG_KILL_STATUS: i32 = 3;

/// How often the check looks for the `syslog` reader's task to have blocked.
const SYSLOG_POLL_NANOS: u64 = 1_000_000;

/// A program blocked in `syslog`'s read, killed with a status and no signal, is
/// released with that status.
///
/// The read waits for a signal, and a kill that posts none still ends the wait
/// because a terminated process counts as one pending. A wait that did not
/// would never let its thread reach its exit, and the process would never be
/// released: `wait_for_exit` would run to its deadline.
///
/// The kernel log always has something to read at boot, so the system's one
/// reader is parked past its end for the check, and put back after.
fn check_a_reader_blocked_in_syslog_is_released_by_a_kill() -> Result<(), &'static str> {
    if arch::USER_SYSLOG_PROGRAM.is_empty() {
        return Ok(());
    }
    let parked = system::park_reader();
    let outcome = kill_a_reader_blocked_in_syslog();
    system::unpark_reader(parked);
    outcome
}

/// [`check_a_reader_blocked_in_syslog_is_released_by_a_kill`], with the reader
/// parked.
fn kill_a_reader_blocked_in_syslog() -> Result<(), &'static str> {
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_SYSLOG_PROGRAM,
    );
    let reader = process::load(
        &file,
        &[b"/klogd"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that reads syslog could not be loaded")?;
    let task =
        process::start(&reader).map_err(|_| "a program that reads syslog could not be started")?;
    // Killed only once its task is blocked, so that the kill meets the read's
    // wait rather than a program not yet in it.
    let settle = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    while !task.is_blocked() {
        if reader.is_terminated() {
            return Err("a program blocked in syslog's read ended before it was killed");
        }
        if crate::timer::now_nanos() >= settle {
            return Err("a program that reads syslog never blocked in the read");
        }
        crate::sched::sleep_for(SYSLOG_POLL_NANOS);
    }
    process::kill(&reader, SYSLOG_KILL_STATUS);
    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    if reader.wait_for_exit(deadline) != Some(SYSLOG_KILL_STATUS) {
        return Err(
            "a program killed with no signal while blocked in syslog's read was never released",
        );
    }
    Ok(())
}

/// A program's second thread `execve`s the plain test program while its first
/// thread waits in `FUTEX_WAIT` on a word nobody wakes: the first thread must
/// be ended, the second must take the pid and give its own id back, and the
/// process must end with the new program's status, recorded as the file it
/// ran.
///
/// Waited for with a deadline, so an `execve` whose wait for the other thread
/// never ends fails by name rather than hanging the boot.
fn check_execve_from_a_thread_ends_the_others() -> Result<Option<i32>, &'static str> {
    use ferrix_vfs::OpenFlags;

    if arch::USER_DETHREAD_PROGRAM.is_empty() {
        return Ok(None);
    }
    let class = class_of_this_build();
    let machine = arch::ARCH.elf_machine();
    let target = image::build_with(class, machine, image::Shape::Good, arch::USER_TEST_PROGRAM);
    let caller = image::build_with(
        class,
        machine,
        image::Shape::Good,
        arch::USER_DETHREAD_PROGRAM,
    );

    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let create = OpenFlags {
        read: false,
        write: true,
        create: true,
        exclusive: false,
        truncate: true,
        append: false,
        directory: false,
        nofollow: false,
        path: false,
        nonblock: false,
    };
    let file = ns
        .open(&ctx, None, EXEC_TARGET, &create, 0o755)
        .map_err(|_| "could not create the program a thread is to execve")?;
    if file.write(&target) != Ok(target.len()) {
        return Err("could not write the program a thread is to execve");
    }
    drop(file);
    let outcome = run_a_thread_that_execs(&caller);
    ns.unlink(&ctx, None, EXEC_TARGET)
        .map_err(|_| "could not remove the program a thread ran through execve")?;
    outcome
}

/// Run [`arch::USER_DETHREAD_PROGRAM`] from `image` and judge how it ended,
/// for [`check_execve_from_a_thread_ends_the_others`].
fn run_a_thread_that_execs(image: &[u8]) -> Result<Option<i32>, &'static str> {
    let execing = process::load(
        image,
        &[b"/dethread"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that execs from a thread could not be loaded")?;
    let pid = execing.pid();
    let _task = process::start(&execing)
        .map_err(|_| "a program that execs from a thread could not be started")?;
    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    let status = execing
        .wait_for_exit(deadline)
        .ok_or("a process whose thread called execve while its first thread waited never ended")?;
    match status {
        96 => return Err("a program that execs from a thread could not make its thread"),
        97 => return Err("execve from a thread other than the first returned to the program"),
        status if status != arch::USER_TEST_STATUS => {
            return Err(
                "a process whose thread called execve did not end with the new program's status",
            );
        }
        _ => {}
    }
    if execing.pid() != pid || execing.exe() != EXEC_TARGET {
        return Err("a thread's execve did not run the new program in the same process");
    }
    if crate::syscall::registry::numbers_naming(&execing) != 1 {
        return Err(
            "a thread that replaced its process's program kept a thread id of its own beside the pid",
        );
    }
    Ok(Some(status))
}

/// Where [`arch::USER_STOPPED_PROGRAM`] keeps its two counts, the word its
/// third thread waits on, and what that wait returned.
const STOPPED_PAGE: u64 = 0x6000_0000;

/// What [`check_a_stop_stops_every_thread_and_a_continue_restarts_their_calls`]
/// kills the program with, and the status it must then end with.
const STOPPED_KILL_STATUS: i32 = 137;

/// How long each step of that check waits for the program to get there.
const STOP_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How often it looks meanwhile.
const STOP_POLL_NANOS: u64 = 1_000_000;

/// How long a stopped program's counts must stay still.
const STOP_STILL_NANOS: u64 = 50_000_000;

/// A program of three threads -- two counting on their own words, one waiting
/// in `FUTEX_WAIT` with no timeout -- is stopped and continued from outside.
///
/// Both counts must have moved and the third thread be waiting before the
/// stop, so a thread that never ran cannot pass for a stopped one. After
/// `SIGSTOP` every one of its tasks must be blocked, and both counts must stay
/// still across a window; after `SIGCONT` both must move again and the waiter
/// must be back in its wait, never having returned from it -- a stop ends a
/// blocked call, and the call must restart, not fail. `SIGKILL` then ends it.
/// Verifies: `L.x86_64.66`
fn check_a_stop_stops_every_thread_and_a_continue_restarts_their_calls()
-> Result<Option<i32>, &'static str> {
    use crate::syscall::signal::Origin;
    use ferrix_linux_abi::types::SIGKILL;

    if arch::USER_STOPPED_PROGRAM.is_empty() {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_STOPPED_PROGRAM,
    );
    let stopped = process::load(
        &file,
        &[b"/stopped"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program of three threads could not be loaded")?;
    let _task =
        process::start(&stopped).map_err(|_| "a program of three threads could not be started")?;
    let outcome = stop_and_continue(&stopped);
    crate::syscall::kill::send(&stopped, SIGKILL, Origin::Kernel);
    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    let status = stopped.wait_for_exit(deadline);
    if status == Some(96) {
        return Err("a program of three threads could not map its page or make its threads");
    }
    outcome?;
    match status {
        Some(STOPPED_KILL_STATUS) => Ok(Some(STOPPED_KILL_STATUS)),
        Some(_) => Err("a program of three threads killed with SIGKILL ended with another status"),
        None => Err("a program of three threads was never released after SIGKILL"),
    }
}

/// Where [`arch::USER_HANDOFF_PROGRAM`] keeps the id of the thread its
/// `SIGUSR1` handler ran on, its two threads' ids, and its main thread's word.
const HANDOFF_PAGE: u64 = 0x6000_0000;

/// A signal sent to a process of two threads reaches the thread that can take
/// it, in a program whose first thread waits in `FUTEX_WAIT` and whose second
/// spins, alone on its processor where it gets no tick.
///
/// First, sent while the first thread blocks it: the second must take it --
/// the one it is given to must be the thread that does not block it. Then the
/// first thread is sent a `SIGUSR2` of its own and the process a `SIGUSR1`,
/// neither waking anyone, and only the first thread is woken: it takes its own
/// signal first, and its handler's mask blocks `SIGUSR1` while that is still
/// pending, so only the hand-off from the handler's mask can bring it to the
/// second thread. That handler waits for it to arrive there, and returns
/// after about three seconds if it never does -- when the first thread would
/// take it itself, which fails by name. `SIGKILL` then ends the program.
fn check_a_signal_reaches_the_thread_that_can_take_it() -> Result<Option<i32>, &'static str> {
    use crate::syscall::signal::Origin;
    use ferrix_linux_abi::types::SIGKILL;

    if arch::USER_HANDOFF_PROGRAM.is_empty() {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_HANDOFF_PROGRAM,
    );
    let handing = process::load(
        &file,
        &[b"/handoff"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program of two threads with handlers could not be loaded")?;
    let _task = process::start(&handing)
        .map_err(|_| "a program of two threads with handlers could not be started")?;
    let outcome = hand_a_signal_on(&handing);
    crate::syscall::kill::send(&handing, SIGKILL, Origin::Kernel);
    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    let status = handing.wait_for_exit(deadline);
    if status == Some(96) {
        return Err(
            "a program of two threads with handlers could not map its page, install its handlers \
             or make its thread",
        );
    }
    outcome?;
    match status {
        Some(STOPPED_KILL_STATUS) => Ok(Some(STOPPED_KILL_STATUS)),
        Some(_) => {
            Err("a program of two threads with handlers ended with another status than SIGKILL's")
        }
        None => Err("a program of two threads with handlers was never released after SIGKILL"),
    }
}

/// The two sends of [`check_a_signal_reaches_the_thread_that_can_take_it`].
fn hand_a_signal_on(handing: &Process) -> Result<(), &'static str> {
    use crate::syscall::signal::Origin;
    use ferrix_linux_abi::types::{SIGUSR1, SIGUSR2};

    let read = |offset: u64| -> Option<u32> {
        let mut bytes = [0_u8; 4];
        uaccess::copy_from_user(handing.space(), HANDOFF_PAGE + offset, &mut bytes)
            .ok()
            .map(|()| u32::from_le_bytes(bytes))
    };
    let until = |ready: &dyn Fn() -> bool, failure: &'static str| -> Result<(), &'static str> {
        let deadline = crate::timer::now_nanos().saturating_add(STOP_PATIENCE_NANOS);
        loop {
            if ready() {
                return Ok(());
            }
            if handing.is_terminated() {
                return Err(
                    "a program of two threads with handlers ended before the check was done",
                );
            }
            if crate::timer::now_nanos() >= deadline {
                return Err(failure);
            }
            crate::sched::sleep_for(STOP_POLL_NANOS);
        }
    };

    until(
        &|| {
            read(4).is_some_and(|tid| tid != 0)
                && read(12).is_some_and(|tid| tid != 0)
                && futex::waiters_on(handing, HANDOFF_PAGE + 8) == 1
        },
        "a program of two threads with handlers never had its second thread running and its first \
         waiting",
    )?;
    let second = read(4).ok_or("could not read the hand-off program's second thread's id")?;
    let first_tid = read(12).ok_or("could not read the hand-off program's first thread's id")?;
    let first = handing
        .thread_by_tid(first_tid)
        .ok_or("the hand-off program's first thread was not found by its id")?;

    signal::change_blocked(&first, |_, own| {
        let _ = own.replace_blocked(signal::bit(SIGUSR1));
    });
    let _ = handing.take_handed_to();
    crate::syscall::kill::send(handing, SIGUSR1, Origin::Kernel);
    match handing.take_handed_to() {
        tid if tid == second => {}
        0 => return Err("a signal sent to a process of two threads was given to no thread"),
        _ => {
            return Err(
                "a signal sent to a process was given to a thread that blocks it, not the one that \
                 does not",
            );
        }
    }
    until(
        &|| read(0).is_some_and(|tid| tid != 0),
        "a signal sent to a process whose first thread blocks it never reached its second thread",
    )?;
    if read(0) != Some(second) {
        return Err(
            "a signal sent to a process was taken by a thread other than the one not blocking it",
        );
    }
    uaccess::copy_to_user(handing.space(), HANDOFF_PAGE, &0_u32.to_le_bytes())
        .map_err(|_| "could not clear the hand-off program's word")?;
    signal::change_blocked(&first, |_, own| {
        let _ = own.replace_blocked(0);
    });

    let _ = handing.post_signal_to(&first, SIGUSR2, Origin::Kernel);
    let _ = handing.post_signal(SIGUSR1, Origin::Kernel);
    handing.notify_signal_to(&first);
    until(
        &|| read(0).is_some_and(|tid| tid != 0),
        "a signal a handler's mask blocked in the thread that would take it never reached another \
         thread",
    )?;
    if read(0) != Some(second) {
        return Err(
            "a signal a handler's mask blocked was taken by the thread that blocked it, not handed on",
        );
    }
    // Zero is the second thread having come back through the kernel -- a tick,
    // an interrupt, a reschedule -- and taken SIGUSR1 itself before the first
    // thread's handler blocked it: a right outcome too, and one no program can
    // rule out. The hand-off itself is decided deterministically, without a
    // program, by `check_a_signal_blocked_after_it_was_sent_is_handed_on`; here
    // it only has to have reached the right thread.
    match handing.take_handed_to() {
        tid if tid == second => {
            println!(
                "  threads  on {}, SIGUSR1 was handed on from the handler's mask",
                arch::NAME
            );
            Ok(())
        }
        0 => {
            println!(
                "  threads  on {}, SIGUSR1 was taken by the second thread before the hand-off",
                arch::NAME
            );
            Ok(())
        }
        _ => Err("a signal a handler's mask blocked was handed on to a thread that blocks it"),
    }
}

/// The stop and the continue of
/// [`check_a_stop_stops_every_thread_and_a_continue_restarts_their_calls`].
fn stop_and_continue(stopped: &Process) -> Result<(), &'static str> {
    use crate::syscall::signal::Origin;
    use ferrix_linux_abi::types::{SIGCONT, SIGSTOP};

    let word = |offset: u64| -> Result<u32, &'static str> {
        let mut bytes = [0_u8; 4];
        uaccess::copy_from_user(stopped.space(), STOPPED_PAGE + offset, &mut bytes)
            .map_err(|_| "could not read the page of a program of three threads")?;
        Ok(u32::from_le_bytes(bytes))
    };
    let waiting = |wait: &dyn Fn() -> Result<bool, &'static str>| -> Result<bool, &'static str> {
        if word(12)? != 1 {
            return Err("a FUTEX_WAIT stopped and continued returned instead of being restarted");
        }
        Ok(futex::waiters_on(stopped, STOPPED_PAGE + 8) == 1 && wait()?)
    };
    let until = |ready: &dyn Fn() -> Result<bool, &'static str>,
                 failure: &'static str|
     -> Result<(), &'static str> {
        let deadline = crate::timer::now_nanos().saturating_add(STOP_PATIENCE_NANOS);
        loop {
            if ready()? {
                return Ok(());
            }
            if stopped.is_terminated() {
                return Err("a program of three threads ended before the check was done with it");
            }
            if crate::timer::now_nanos() >= deadline {
                return Err(failure);
            }
            crate::sched::sleep_for(STOP_POLL_NANOS);
        }
    };

    // The page is the program's to map: until it has, and has marked its waiter
    // as waiting, nothing on it can be read.
    until(
        &|| {
            let mut bytes = [0_u8; 4];
            Ok(
                uaccess::copy_from_user(stopped.space(), STOPPED_PAGE + 12, &mut bytes).is_ok()
                    && u32::from_le_bytes(bytes) == 1,
            )
        },
        "a program of three threads never mapped its page",
    )?;
    until(
        &|| waiting(&|| Ok(word(0)? != 0 && word(4)? != 0)),
        "a program of three threads never had both counts moving and its third thread waiting",
    )?;
    crate::syscall::kill::send(stopped, SIGSTOP, Origin::Kernel);
    until(
        &|| Ok(stopped.is_stopped() && stopped.every_task_blocked()),
        "a thread of a stopped process kept running instead of stopping",
    )?;
    let still = (word(0)?, word(4)?);
    crate::sched::sleep_for(STOP_STILL_NANOS);
    if (word(0)?, word(4)?) != still {
        return Err("a thread of a stopped process went on counting");
    }
    crate::syscall::kill::send(stopped, SIGCONT, Origin::Kernel);
    until(
        &|| waiting(&|| Ok(word(0)? != still.0 && word(4)? != still.1)),
        "a stopped process's threads did not all run again after SIGCONT",
    )?;
    // A waiter the stop woke is still listed on its word until it answers, so
    // the look above can pass while it is on its way out with the wrong
    // answer. Give it the same window, and look at what it recorded again.
    crate::sched::sleep_for(STOP_STILL_NANOS);
    if word(12)? != 1 {
        return Err("a FUTEX_WAIT stopped and continued returned instead of being restarted");
    }
    Ok(())
}

/// Runs of the forking program measured by
/// [`check_ended_programs_give_their_frames_back`].
const RECLAIM_RUNS: u32 = 4;

/// How long each look for the reaper to have finished waits first.
const RECLAIM_SETTLE_NANOS: u64 = 20_000_000;

/// The most looks before frames still out are taken to be kept: two seconds.
const RECLAIM_ROUNDS: u32 = 100;

/// Programs that fork, wait for their child and exit give back every frame
/// they used -- the parent's and the child's tables and pages, and their kernel
/// stacks -- once their tasks are reaped.
///
/// No check that runs a program counted frames, because a task not yet reaped
/// looks exactly like a leak. So nothing noticed that every program leaked
/// everything: `task_start` held the task's own reference across an entry that
/// never returns (fixed in 0510a8a), which kept the task, and through it the
/// process and its address space, forever. Under busybox it showed as `free`
/// rising by about a megabyte for every process that exited, and a long
/// session ending in `Out of memory`.
///
/// Measured the way that makes a reaped task count: the program runs
/// [`RECLAIM_RUNS`] times before any window, for the reason [`run`] gives, and
/// the reaper is settled before each first count, so that nothing an earlier
/// run started is freed inside the window. The second count is waited for
/// rather than read once, because an ended task is freed only after its
/// processor has switched away from it and a reaper has run, which is shortly
/// after `exec::run` returns, not before.
///
/// **Up to [`RECLAIM_WINDOWS`] windows, and one that comes back exactly is
/// enough.** What lives across programs -- the reaper's list, the run queues
/// and their sleeper sets, the pid table, the heap's size-class pages -- grows
/// to a size that depends on how the processors interleaved. On x86-64 and on
/// ARMv7-A a first window has ended one frame short with no process left in
/// the pid table and every kernel stack back in the arena, and the next window
/// has come back exactly. A leak is not like that: every program that leaks
/// keeps at least its page tables, in every window.
fn check_ended_programs_give_their_frames_back() -> Result<Option<(u32, u32)>, &'static str> {
    if arch::USER_FORK_PROGRAM.is_empty() {
        return Ok(None);
    }
    for _ in 0..RECLAIM_RUNS {
        let _warm = check_a_forked_child_is_waited_for()?;
    }
    let mut rose = false;
    for window in 1..=RECLAIM_WINDOWS {
        let before = settled_free_frames();
        for _ in 0..RECLAIM_RUNS {
            let _ = check_a_forked_child_is_waited_for()?;
        }
        let after = free_frames_back_to(before);
        if after == before {
            return Ok(Some((RECLAIM_RUNS, window)));
        }
        rose |= after > before;
    }
    if rose {
        return Err(
            "the free frame count rose across programs that forked and exited: something \
             outside them freed frames in the window",
        );
    }
    Err("programs that forked and exited did not give every frame back once reaped")
}

/// Windows [`check_ended_programs_give_their_frames_back`] measures before
/// frames still out in every one are taken to be kept.
const RECLAIM_WINDOWS: u32 = 4;

/// The held frame count ([`mm::held_frames`]) once it is back to `before`, or as it stands after
/// [`RECLAIM_ROUNDS`] looks, reaping between them.
fn free_frames_back_to(before: u64) -> u64 {
    let mut after = mm::held_frames();
    for _ in 0..RECLAIM_ROUNDS {
        if after == before {
            break;
        }
        crate::sched::sleep_for(RECLAIM_SETTLE_NANOS);
        let _ = crate::sched::reap();
        after = mm::held_frames();
    }
    after
}

/// The held frame count ([`mm::held_frames`]) once the reaper has nothing left to do: no ended task
/// was found on a look, and the count did not move since the one before.
fn settled_free_frames() -> u64 {
    let mut last = mm::held_frames();
    for _ in 0..RECLAIM_ROUNDS {
        crate::sched::sleep_for(RECLAIM_SETTLE_NANOS);
        let reaped = crate::sched::reap();
        let now = mm::held_frames();
        if reaped == 0 && now == last {
            return now;
        }
        last = now;
    }
    last
}

/// What [`arch::USER_SIGNAL_PROGRAM`] exits with when everything is right.
const SIGNAL_STATUS: i32 = 77;

/// A program installs a handler with a restorer, signals itself with `tgkill`
/// -- the way `abort` does -- and exits with a register its handler changed
/// through the `ucontext` of the frame it ran on.
///
/// One number covers the whole round trip: the signal was pending on the way
/// back from `tgkill`, a frame in Linux's layout was written to the program's
/// stack, the handler was entered with the signal, `siginfo` and `ucontext` in
/// the right registers and the signal blocked, it returned into its restorer,
/// and `rt_sigreturn` read the frame back -- the changed register included --
/// and put the old mask back. On ARMv7-A the program does it a second time
/// without `SA_SIGINFO`, through the other frame and `sigreturn`.
/// Verifies: `L.x86_64.42`, `L.x86_64.55`, H.TRAP.9
fn check_a_handler_runs_and_returns() -> Result<Option<i32>, &'static str> {
    if arch::USER_SIGNAL_PROGRAM.is_empty() {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_SIGNAL_PROGRAM,
    );
    let status = exec::run(
        &file,
        &[b"/signal"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that handles a signal could not be started")?;
    match status {
        SIGNAL_STATUS => Ok(Some(status)),
        7 => Err(
            "a signal handler did not run, or returning from it did not restore the registers \
             its frame held",
        ),
        98 => Err("a signal handler was entered with the wrong signal, siginfo or blocked mask"),
        99 => Err("rt_sigaction, tgkill or the mask after a handler returned was wrong"),
        129..=192 => Err(
            "a program that handles a signal was killed by a signal instead: its frame, its \
             handler's entry or its return was wrong",
        ),
        _ => Err("a program that handles a signal did not exit with 77"),
    }
}

/// What [`arch::USER_XSTATE_PROGRAM`] exits with when its frame carried an
/// `XSAVE` area, which its handler poisoned, and the program ran on after
/// `rt_sigreturn`.
const XSTATE_POISONED: i32 = 79;

/// What it exits with when its frame carried `FXSAVE` alone: a processor
/// without `XSAVE`, or AVX, where there is no header to poison.
const XSTATE_ABSENT: i32 = 78;

/// A handler writes ones over its frame's `XSAVE` header -- `XSTATE_BV`
/// naming every component, AVX-512's and PKRU's among them, `XCOMP_BV` with
/// the compacted form's bit, the reserved bytes -- keeping both magic words
/// and the sizes, and returns through `rt_sigreturn`.
///
/// `XRSTOR64` refuses such a header with `#GP`, in ring 0, where the return
/// loads it: any program could stop the machine. The return must instead
/// take `XSTATE_BV` masked -- to AVX, x87 and SSE in `restore_fpu`, and to
/// `XCR0` in `UserState::set_xstate_bv` -- and nothing else of the header, so
/// the program runs on and exits with its handler's count. With both masks
/// taken out (scratch, 2026-09-30) the boot stops here on the `#GP`, under
/// TCG, whose `XRSTOR` checks the header, and under KVM. With only
/// `set_xstate_bv`'s taken out it does not: a frame carries an `XSAVE` area
/// only when `XCR0` has AVX, so `restore_fpu`'s mask is already inside it.
/// Verifies: `L.x86_64.122`
fn check_a_poisoned_xsave_header_comes_back_safely() -> Result<(), &'static str> {
    if arch::USER_XSTATE_PROGRAM.is_empty() {
        return Ok(());
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_XSTATE_PROGRAM,
    );
    let status = exec::run(
        &file,
        &[b"/xstate"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that poisons its signal frame could not be started")?;
    match status {
        XSTATE_POISONED => {
            println!(
                "  xstate   a signal frame's XSAVE header, every bit set, came back through \
                 rt_sigreturn masked to XCR0, without a fault"
            );
            Ok(())
        }
        XSTATE_ABSENT => {
            println!("  xstate   no XSAVE area in signal frames on this processor: FXSAVE alone");
            Ok(())
        }
        99 => Err("rt_sigaction or tgkill failed in the program that poisons its frame"),
        _ => Err("a program whose handler poisoned its XSAVE header did not run on after it"),
    }
}

/// A user address a handler is pretended to live at, for the checks that drive
/// the delivery decision kernel-side: never entered, only inspected, so any
/// user address distinguishable from `SIG_DFL` (0) and `SIG_IGN` (1) does.
const CHECK_HANDLER: u64 = 0x4000;

/// The signal paths nothing else here drives, each with a negative control,
/// checked against the kernel's own decision functions -- what the boot log's
/// one `sigpaths` line stands for.
///
/// These are kernel-side rather than hand-assembled programs because what is
/// untested is the *decision*: which signal becomes deliverable, what `wait4`
/// reports, when an alarm fires, where a handler's frame lands, whether a fault
/// forces its signal, and whether an interrupted call restarts. Each is a
/// method the delivery path calls, driven here against processes the check
/// builds, the way `check_futexes` drives the futex table.
fn check_untested_signal_paths() -> Result<(), &'static str> {
    check_a_childs_end_reaches_a_sigchld_handler_and_still_reaps()?;
    check_a_childs_stop_and_continue_are_reported()?;
    check_an_alarm_delivers_sigalrm()?;
    check_sa_onstack_puts_the_handler_on_the_alternate_stack()?;
    check_a_fault_forces_its_signal_past_a_block()?;
    check_a_threads_own_signal_goes_before_its_processs()?;
    check_a_signal_is_judged_against_its_takers_mask()?;
    check_signals_are_decided_across_threads()?;
    crate::syscall::deliver::check_restart_decisions()?;
    check_signal_state_reports_running_out()?;
    println!(
        "  sigpaths SIGCHLD reached a handler and wait4 still reaped; a stop and continue were \
         reported; an alarm raised SIGALRM; SA_ONSTACK chose the alternate stack; a blocked \
         fault was forced; a thread took its own signal before its process's; a signal was \
         judged against its taker's mask, a fork child's included; SA_RESTART restarts, poll \
         and a flagless handler do not; signal state and a new process's descriptors with no memory are refused, not \
         fatal"
    );
    Ok(())
}

/// A process's and a thread's signal state report running out of memory
/// rather than stop the machine (finding F-23): with every fallible
/// allocation of this task failing, a new process's tables, a fork child's
/// and a first thread's each come back as an error, and without the failing
/// each is made.
///
/// They were `vec!` behind a `Default` and a `Clone`, which no injection
/// reaches: with the old `Signals::default` put back (scratch), the first
/// test here fails, since the tables are made with every allocation failing.
/// On a real refusal that `vec!` reached the allocation error handler, on
/// every `fork`, `clone`, `process_create` and `process_start`.
fn check_signal_state_reports_running_out() -> Result<(), &'static str> {
    use crate::syscall::thread::Thread;

    let process = process::new_for_check().map_err(|_| "could not make a process")?;
    let task = crate::sched::current_id().ok_or("the signal checks run outside a task")?;
    crate::fallible::inject(task, 1);
    let fresh = signal::Signals::new().is_err();
    let forked = process.with_signals(|signals| signals.for_fork().is_err());
    let thread = Thread::leader(&process).is_err();
    let failed = crate::fallible::stop_injecting();
    if !fresh || !forked || !thread || failed < 3 {
        return Err("signal state was made with every allocation failing");
    }
    let made = signal::Signals::new().is_ok()
        && process.with_signals(|signals| signals.for_fork().is_ok())
        && Thread::leader(&process).is_ok();
    if !made {
        return Err("signal state was refused with memory to spare");
    }
    check_standard_streams_report_running_out(task)
}

/// A new process's descriptors 0, 1 and 2 report running out of memory as an
/// error, which `process_create` answers with `NO_MEMORY`, rather than stop
/// the kernel (finding F-23): with every fallible allocation of this task
/// failing the table is refused, and without it the table holds three.
///
/// `standard_streams` used to turn any failure into a fatal stop
/// (`CONSOLE_DESCRIPTORS`). The table's own growth does not ask the
/// injection policy, so `standard_streams` asks it first: without that
/// (scratch), the first test here fails, since the table is made with every
/// allocation failing.
fn check_standard_streams_report_running_out(
    task: crate::sched::TaskId,
) -> Result<(), &'static str> {
    crate::fallible::inject(task, 1);
    let refused = fd::standard_streams().is_err();
    let failed = crate::fallible::stop_injecting();
    if !refused || failed == 0 {
        return Err("a new process's descriptors were made with every allocation failing");
    }
    match fd::standard_streams() {
        Ok(table) if table.len() == 3 => Ok(()),
        _ => Err("a new process's descriptors were refused with memory to spare"),
    }
}

/// Build a child linked to `parent`, registered and adopted, ready to end.
fn child_of(parent: &Arc<Process>) -> Result<Arc<Process>, &'static str> {
    let space = crate::user::space::AddressSpace::new()
        .map_err(|_| "a check could not make an address space")?;
    let child = crate::syscall::registry::register(
        Process::forked(parent, space, false, false).map_err(|_| "no memory for a fork")?,
    );
    parent.adopt(Arc::clone(&child));
    Ok(child)
}

/// (a) A parent with a `SIGCHLD` handler: when its child ends, `SIGCHLD` reaches
/// the handler and `wait4` still reaps the child. The negative control is a
/// parent that ignores `SIGCHLD`, for which the child's end leaves nothing to
/// deliver -- Linux discards a `SIG_IGN` `SIGCHLD` rather than queuing it.
fn check_a_childs_end_reaches_a_sigchld_handler_and_still_reaps() -> Result<(), &'static str> {
    use crate::syscall::family;
    use ferrix_linux_abi::types::{SA_RESTART, SIG_IGN, SIGCHLD};

    let parent = process::new_for_check().map_err(|_| "could not make a parent process")?;
    parent.with_signals(|signals| signals.install_action(SIGCHLD, CHECK_HANDLER, SA_RESTART));
    let child = child_of(&parent)?;
    let child_pid = child.pid();
    process::kill(&child, 0);
    let taken = crate::syscall::thread::Thread::leader(&parent)
        .map_err(|_| "no memory for a check's thread")?
        .with_signals(signal::take_next)
        .ok_or("a child's end did not reach a parent that handles SIGCHLD")?;
    if taken.signal != SIGCHLD || taken.action.handler != CHECK_HANDLER {
        return Err("a child's end delivered the wrong signal, or not to the handler");
    }
    let reaped = family::sys_wait4(&parent, child_pid as i32, 0, 0, 0, crate::trap::Abi::Native)
        .map_err(|_| "wait4 refused to reap a child whose SIGCHLD had a handler")?;
    if reaped != child_pid as usize {
        return Err("wait4 did not reap the child SIGCHLD announced");
    }

    let ignorer = process::new_for_check().map_err(|_| "could not make a parent process")?;
    ignorer.with_signals(|signals| signals.install_action(SIGCHLD, SIG_IGN, 0));
    let orphan = child_of(&ignorer)?;
    process::kill(&orphan, 0);
    if ignorer.with_signals(|signals| signals.pending() & signal::bit(SIGCHLD) != 0) {
        return Err("a parent ignoring SIGCHLD was still given one to deliver");
    }
    Ok(())
}

/// (b) A stopped child is reported to `wait4` with `WUNTRACED`, and a continued
/// one with `WCONTINUED`. The negative controls ask without each flag and must
/// see nothing, since Linux reports a stop or a continue only when asked.
fn check_a_childs_stop_and_continue_are_reported() -> Result<(), &'static str> {
    use ferrix_linux_abi::types::SIGTSTP;

    let parent = process::new_for_check().map_err(|_| "could not make a parent process")?;
    let child = child_of(&parent)?;
    let child_pid = child.pid();
    let any = |_: &Process| true;

    if parent.changed_child(&any, true, true, false).is_some() {
        return Err("a child that had not stopped was reported as stopped");
    }
    child.enter_stop(SIGTSTP);
    if parent.changed_child(&any, false, false, false).is_some() {
        return Err("a stop was reported to a wait without WUNTRACED");
    }
    let (stopped, signal) = parent
        .changed_child(&any, true, false, true)
        .ok_or("WUNTRACED did not report a stopped child")?;
    if stopped.pid() != child_pid || signal != SIGTSTP {
        return Err("WUNTRACED reported the wrong child or stop signal");
    }

    child.leave_stop();
    if parent.changed_child(&any, false, false, false).is_some() {
        return Err("a continue was reported to a wait without WCONTINUED");
    }
    let (continued, signal) = parent
        .changed_child(&any, false, true, true)
        .ok_or("WCONTINUED did not report a continued child")?;
    if continued.pid() != child_pid || signal != 0 {
        return Err("WCONTINUED reported the wrong child, or a stop signal for a continue");
    }
    Ok(())
}

/// (c) An armed `ITIMER_REAL` is due at its deadline and raises `SIGALRM`, then
/// disarms itself, being one-shot. The negative control ticks it a nanosecond
/// early and must find it not yet due.
fn check_an_alarm_delivers_sigalrm() -> Result<(), &'static str> {
    use crate::syscall::kill;
    use crate::syscall::signal::{Alarm, Origin};
    use ferrix_linux_abi::types::SIGALRM;

    let process = process::new_for_check().map_err(|_| "could not make a process")?;
    process.with_signals(|signals| signals.install_action(SIGALRM, CHECK_HANDLER, 0));
    let now = crate::timer::now_nanos();
    let deadline = now.saturating_add(1_000_000).max(2);
    process.with_signals(|signals| {
        let _ = signals.set_alarm(Alarm {
            deadline,
            interval: 0,
        });
    });

    if process.with_signals(|signals| signals.tick_alarm(deadline - 1)) {
        return Err("an interval timer fired before its deadline");
    }
    if !process.with_signals(|signals| signals.tick_alarm(deadline)) {
        return Err("an interval timer did not fire at its deadline");
    }
    kill::send(&process, SIGALRM, Origin::Kernel);
    if process.with_signals(|signals| signals.pending() & signal::bit(SIGALRM) == 0) {
        return Err("a due alarm did not raise a deliverable SIGALRM");
    }
    if process.with_signals(|signals| signals.alarm().deadline) != 0 {
        return Err("a one-shot alarm did not disarm after firing");
    }
    Ok(())
}

/// (d) A handler installed with `SA_ONSTACK` has its frame built on the
/// alternate stack; the handler's stack pointer will be inside its range. The
/// negative control, the same handler without `SA_ONSTACK`, stays on the
/// program's own stack.
fn check_sa_onstack_puts_the_handler_on_the_alternate_stack() -> Result<(), &'static str> {
    use ferrix_linux_abi::types::SA_ONSTACK;

    let process = process::new_for_check().map_err(|_| "could not make a process")?;
    let alt_sp = 0x2000_0000_u64;
    let alt_size = 0x4000_u64;
    let program_sp = 0x7000_0000_u64;
    let thread = crate::syscall::thread::Thread::leader(&process)
        .map_err(|_| "no memory for a check's thread")?;
    thread.with_own_signals(|signals| signals.arm_alt_stack_for_check(alt_sp, alt_size));

    let on_alt = thread.with_own_signals(|signals| signals.frame_base(SA_ONSTACK, program_sp));
    if !(on_alt > alt_sp && on_alt <= alt_sp + alt_size) {
        return Err("SA_ONSTACK did not put the handler frame on the alternate stack");
    }
    let off_alt = thread.with_own_signals(|signals| signals.frame_base(0, program_sp));
    if off_alt > alt_sp && off_alt <= alt_sp + alt_size {
        return Err("a handler without SA_ONSTACK was put on the alternate stack");
    }
    Ok(())
}

/// (e) A fault becomes a signal its handler catches: a `SIGSEGV` handler that
/// the program did not block is kept and made deliverable when the fault is
/// forced, which is exactly what lets rustc's guard-page handler catch a stack
/// overflow rather than the process dying. The negative control forces the same
/// fault against a program that had blocked it: `force_sig_info` resets it to
/// its default and past the block to fatal, so a program cannot mask a fault to
/// spin on the faulting instruction for ever.
fn check_a_fault_forces_its_signal_past_a_block() -> Result<(), &'static str> {
    use crate::syscall::signal::{Origin, Posted};
    use ferrix_linux_abi::types::{SA_ONSTACK, SIGSEGV};

    // `SEGV_MAPERR`: nothing mapped at the address, as `arch::fault_signal`
    // reports for a write to address zero. Local, as it is there.
    const SEGV_MAPERR: i32 = 1;
    let fault = Origin::Fault {
        code: SEGV_MAPERR,
        address: 0,
    };

    let caught = process::new_for_check().map_err(|_| "could not make a process")?;
    caught.with_signals(|signals| signals.install_action(SIGSEGV, CHECK_HANDLER, SA_ONSTACK));
    let caught_thread = crate::syscall::thread::Thread::leader(&caught)
        .map_err(|_| "no memory for a check's thread")?;
    let posted =
        caught_thread.with_signals(|shared, own| signal::force(shared, own, SIGSEGV, fault));
    if posted != Posted::Pending {
        return Err("a fault with an unblocked handler was not made pending for it");
    }
    let taken = caught_thread
        .with_signals(signal::take_next)
        .ok_or("a forced fault did not become deliverable to its handler")?;
    if taken.signal != SIGSEGV || taken.action.handler != CHECK_HANDLER {
        return Err("a forced fault reset the handler the program had not blocked");
    }

    let dies = process::new_for_check().map_err(|_| "could not make a process")?;
    let dies_thread = crate::syscall::thread::Thread::leader(&dies)
        .map_err(|_| "no memory for a check's thread")?;
    signal::change_blocked(&dies_thread, |shared, own| {
        shared.install_action(SIGSEGV, CHECK_HANDLER, SA_ONSTACK);
        let _ = own.replace_blocked(signal::bit(SIGSEGV));
    });
    let fatal = dies_thread.with_signals(|shared, own| signal::force(shared, own, SIGSEGV, fault));
    if fatal != Posted::Fatal {
        return Err("a fault a program had blocked was not forced past the block to fatal");
    }
    Ok(())
}

/// (f) A thread takes the signals sent to it alone before the ones sent to its
/// process, whatever their numbers, as Linux's `dequeue_signal` does: a
/// `SIGUSR2` (12) forced on the thread goes before a `SIGINT` (2) sent to the
/// process. Neither is a fault's, so the order is the queues' and not the
/// preference for synchronous signals. The negative control sends both to the
/// process, where the lower number goes first.
fn check_a_threads_own_signal_goes_before_its_processs() -> Result<(), &'static str> {
    use crate::syscall::signal::{Origin, Posted};
    use crate::syscall::thread::Thread;
    use ferrix_linux_abi::types::{SIGINT, SIGUSR2};

    let process = process::new_for_check().map_err(|_| "could not make a process")?;
    let (sent, forced, first, second) = Thread::leader(&process)
        .map_err(|_| "no memory for a check's thread")?
        .with_signals(|shared, own| {
            shared.install_action(SIGINT, CHECK_HANDLER, 0);
            shared.install_action(SIGUSR2, CHECK_HANDLER, 0);
            let sent = shared.post(own.blocked(), own.blocked(), SIGINT, Origin::Kernel);
            let forced = signal::force(shared, own, SIGUSR2, Origin::Kernel);
            let first = signal::take_next(shared, own).map(|taken| taken.signal);
            let second = signal::take_next(shared, own).map(|taken| taken.signal);
            (sent, forced, first, second)
        });
    if sent != Posted::Pending || forced != Posted::Pending {
        return Err("a handled signal was not left pending for a thread or its process");
    }
    if (first, second) != (Some(SIGUSR2), Some(SIGINT)) {
        return Err("a thread did not take its own signal before its process's lower-numbered one");
    }

    let control = process::new_for_check().map_err(|_| "could not make a process")?;
    let first = Thread::leader(&control)
        .map_err(|_| "no memory for a check's thread")?
        .with_signals(|shared, own| {
            shared.install_action(SIGINT, CHECK_HANDLER, 0);
            shared.install_action(SIGUSR2, CHECK_HANDLER, 0);
            let _ = shared.post(own.blocked(), own.blocked(), SIGUSR2, Origin::Kernel);
            let _ = shared.post(own.blocked(), own.blocked(), SIGINT, Origin::Kernel);
            signal::take_next(shared, own).map(|taken| taken.signal)
        });
    if first != Some(SIGINT) {
        return Err("of two signals sent to a process, the lower-numbered was not taken first");
    }
    Ok(())
}

/// (g) A signal sent to a process is judged against the mask of the thread that
/// will take it -- a fork child's too, before the child can be found, which is
/// when a process group's signal can reach it. A thread that blocks `SIGTERM`
/// leaves it pending, and so does a fork child whose thread inherited the
/// block; the negative control, a thread that does not block it, is told it is
/// fatal. Nothing is killed: `post_signal` only decides, and `kill::send` acts.
fn check_a_signal_is_judged_against_its_takers_mask() -> Result<(), &'static str> {
    use crate::syscall::signal::{Origin, Posted};
    use crate::syscall::thread::Thread;
    use ferrix_linux_abi::types::SIGTERM;

    let parent = process::new_for_check().map_err(|_| "could not make a process")?;
    let parent_thread =
        Arc::new(Thread::leader(&parent).map_err(|_| "no memory for a check's thread")?);
    parent.add_thread(&parent_thread);
    signal::change_blocked(&parent_thread, |_, own| {
        let _ = own.replace_blocked(signal::bit(SIGTERM));
    });
    if parent.post_signal(SIGTERM, Origin::Kernel) != Posted::Pending {
        return Err("a SIGTERM its only thread blocks was not left pending");
    }

    let space = crate::user::space::AddressSpace::new()
        .map_err(|_| "a check could not make an address space")?;
    let child = Arc::new(
        Process::forked(&parent, space, false, false).map_err(|_| "no memory for a fork")?,
    );
    let child_thread = Arc::new(
        Thread::forked(&child, &parent_thread).map_err(|_| "no memory for a check's thread")?,
    );
    child.add_thread(&child_thread);
    if child.post_signal(SIGTERM, Origin::Kernel) != Posted::Pending {
        return Err("a fork child was judged without the mask its thread inherited");
    }

    let control = process::new_for_check().map_err(|_| "could not make a process")?;
    let unblocked =
        Arc::new(Thread::leader(&control).map_err(|_| "no memory for a check's thread")?);
    control.add_thread(&unblocked);
    if control.post_signal(SIGTERM, Origin::Kernel) != Posted::Fatal {
        return Err("a SIGTERM nothing blocks was not judged fatal");
    }
    Ok(())
}

/// (h) Signals across a process's threads, decided without a program. A
/// signal sent to a process of two threads is fatal while either does not
/// block it, and waits in the process's queue once both do. One sent to a
/// single thread waits in that thread's queue alone when it blocks it, and is
/// fatal when it does not. A continue cancels a stop pending in any thread's
/// queue, and a stop a continue. `tkill` finds a live thread by its id, but
/// not one that has begun to end, and `tgkill` not one under another process.
/// Nothing is killed: posting only decides.
fn check_signals_are_decided_across_threads() -> Result<(), &'static str> {
    use crate::syscall::kill::{sys_tgkill, sys_tkill};
    use crate::syscall::signal::{Origin, Posted};
    use crate::syscall::thread::Thread;
    use ferrix_linux_abi::types::{SIGCONT, SIGTERM, SIGTSTP, SIGUSR1};

    let process = process::new_for_check().map_err(|_| "could not make a process")?;
    let first = Arc::new(Thread::leader(&process).map_err(|_| "no memory for a check's thread")?);
    process.add_thread(&first);
    let tid = crate::syscall::registry::allocate_thread(&process)
        .ok_or("no thread id for the check of signals across threads")?;
    let second = Arc::new(
        Thread::sibling(&process, tid, &first).map_err(|_| "no memory for a check's thread")?,
    );
    process.add_thread(&second);
    let block = |thread: &Thread, signals: u64| {
        signal::change_blocked(thread, |_, own| {
            let _ = own.replace_blocked(signals);
        });
    };
    let shared = || process.with_signals(|signals| signals.pending());
    let own = |thread: &Thread| thread.with_own_signals(|own| own.pending());

    block(&first, signal::bit(SIGTERM));
    block(&second, 0);
    if process.post_signal(SIGTERM, Origin::Kernel) != Posted::Fatal {
        return Err(
            "a SIGTERM sent to a process was not fatal while one of its threads did not block it",
        );
    }
    block(&second, signal::bit(SIGTERM));
    if process.post_signal(SIGTERM, Origin::Kernel) != Posted::Pending
        || shared() & signal::bit(SIGTERM) == 0
    {
        return Err(
            "a SIGTERM both of a process's threads block was not left pending for the process",
        );
    }

    block(&first, signal::bit(SIGUSR1));
    block(&second, 0);
    if process.post_signal_to(&first, SIGUSR1, Origin::Kernel) != Posted::Pending
        || own(&first) & signal::bit(SIGUSR1) == 0
        || (own(&second) | shared()) & signal::bit(SIGUSR1) != 0
    {
        return Err(
            "a SIGUSR1 sent to one thread that blocks it was not left pending for it alone",
        );
    }
    if process.post_signal_to(&second, SIGUSR1, Origin::Kernel) != Posted::Fatal {
        return Err("a SIGUSR1 sent to a thread that does not block it was not fatal");
    }

    block(&second, signal::bit(SIGTSTP));
    let _ = process.post_signal_to(&second, SIGTSTP, Origin::Kernel);
    if own(&second) & signal::bit(SIGTSTP) == 0 {
        return Err("a SIGTSTP sent to a thread that blocks it was not left pending");
    }
    let _ = process.post_signal(SIGCONT, Origin::Kernel);
    if own(&second) & signal::bit(SIGTSTP) != 0 {
        return Err(
            "a SIGCONT sent to a process left a stop pending in one of its threads' queues",
        );
    }
    block(&first, signal::bit(SIGCONT));
    let _ = process.post_signal_to(&first, SIGCONT, Origin::Kernel);
    if own(&first) & signal::bit(SIGCONT) == 0 {
        return Err("a SIGCONT sent to a thread that blocks it was not left pending");
    }
    let _ = process.post_signal(SIGTSTP, Origin::Kernel);
    if own(&first) & signal::bit(SIGCONT) != 0 {
        return Err(
            "a stop sent to a process left a SIGCONT pending in one of its threads' queues",
        );
    }

    let number = i32::try_from(tid).map_err(|_| "a thread id past i32")?;
    if sys_tkill(&process, number, 0) != Ok(0) {
        return Err("tkill did not find a live thread by its id");
    }
    let other = process::new_for_check().map_err(|_| "could not make a process")?;
    let other_pid = i32::try_from(other.pid()).map_err(|_| "a pid past i32")?;
    if sys_tgkill(&process, other_pid, number, 0) != Err(Errno::ESRCH) {
        return Err("tgkill found a thread under a process it does not belong to");
    }
    second.mark_gone();
    if sys_tkill(&process, number, 0) != Err(Errno::ESRCH) {
        return Err("tkill found a thread that had begun to end");
    }
    Ok(())
}

/// Where [`check_a_signal_blocked_after_it_was_sent_is_handed_on`] says its
/// handlers are. Never run: no task of that process ever enters user mode.
const HANDOFF_HANDLER: u64 = 0x1000;

/// (i) A signal a process was sent, which the thread that would take it then
/// blocks, goes on to another thread. With `SIGUSR1` pending for a process of
/// two threads, `rt_sigsuspend` with a mask that blocks it -- ending at once,
/// since the first thread has a `SIGUSR2` of its own to take -- hands it to the
/// second, and so do `rt_sigprocmask` blocking it, the mask `rt_sigsuspend`
/// saved coming back on the way to user mode, a handler frame's mask coming
/// back through `rt_sigreturn`, and a handler's own mask. No task runs: the
/// thread the hand-off woke is read back.
fn check_a_signal_blocked_after_it_was_sent_is_handed_on() -> Result<(), &'static str> {
    use crate::syscall::deliver::sys_rt_sigsuspend;
    use crate::syscall::signal::{Origin, sys_rt_sigprocmask};
    use crate::syscall::thread::Thread;
    use ferrix_linux_abi::types::{SIG_BLOCK, SIGUSR1, SIGUSR2};

    let process = process::new_for_check().map_err(|_| "could not make a process")?;
    let first = Arc::new(Thread::leader(&process).map_err(|_| "no memory for a check's thread")?);
    process.add_thread(&first);
    let tid = crate::syscall::registry::allocate_thread(&process)
        .ok_or("no thread id for the hand-off check")?;
    let second = Arc::new(
        Thread::sibling(&process, tid, &first).map_err(|_| "no memory for a check's thread")?,
    );
    process.add_thread(&second);
    process.with_signals(|signals| {
        signals.install_action(SIGUSR1, HANDOFF_HANDLER, 0);
        signals.install_action(SIGUSR2, HANDOFF_HANDLER, 0);
    });
    let page = map_rw(&process, PAGE_SIZE)?;
    uaccess::copy_to_user(process.space(), page, &signal::bit(SIGUSR1).to_le_bytes())
        .map_err(|_| "could not write the hand-off check's signal set")?;

    let _ = process.post_signal(SIGUSR1, Origin::Kernel);
    let _ = process.post_signal_to(&first, SIGUSR2, Origin::Kernel);
    let _ = sys_rt_sigsuspend(&first, page, 8);
    if process.take_handed_to() != tid {
        return Err(
            "a signal pending for a process was not handed on when rt_sigsuspend blocked it in the \
             thread that would take it",
        );
    }
    // Put back through the funnel directly, not the way back's helper, so that
    // a fault in that helper fails its own case below rather than this one.
    signal::change_blocked(&first, |_, own| own.restore_saved_mask());

    let _ = sys_rt_sigprocmask(&first, SIG_BLOCK, page, 0, 8)
        .map_err(|_| "rt_sigprocmask refused the hand-off check's block")?;
    if process.take_handed_to() != tid {
        return Err(
            "a signal pending for a process was not handed on when rt_sigprocmask blocked it in the \
             thread that would take it",
        );
    }

    // The saved mask coming back on the way to user mode: one that blocks
    // SIGUSR1, saved by an rt_sigsuspend with an empty mask, comes back while
    // SIGUSR1 is still pending for the process.
    signal::change_blocked(&first, |_, own| own.suspend_with(0));
    let _ = process.take_handed_to();
    crate::syscall::deliver::restore_saved_mask(&first);
    if process.take_handed_to() != tid {
        return Err(
            "a signal pending for a process was not handed on when the mask rt_sigsuspend saved came \
             back on the way to user mode",
        );
    }

    // A handler's frame putting back a mask that blocks SIGUSR1, as
    // rt_sigreturn does.
    signal::change_blocked(&first, |_, own| {
        let _ = own.replace_blocked(0);
    });
    let _ = process.take_handed_to();
    crate::syscall::deliver::leave_handler(
        &first,
        signal::bit(SIGUSR1),
        Some(crate::signal_frame::StackRecord::default()),
        0,
    );
    if process.take_handed_to() != tid {
        return Err(
            "a signal pending for a process was not handed on when rt_sigreturn restored a mask that \
             blocks it in the thread that would take it",
        );
    }

    // A handler's mask: the first thread takes a SIGUSR2 of its own, whose
    // handler blocks SIGUSR1, while SIGUSR1 is still pending for the process.
    signal::change_blocked(&first, |_, own| {
        let _ = own.replace_blocked(0);
    });
    process.with_signals(|signals| {
        signals.install_action_masked(SIGUSR2, HANDOFF_HANDLER, 0, signal::bit(SIGUSR1));
    });
    let _ = process.post_signal_to(&first, SIGUSR2, Origin::Kernel);
    let taken = first
        .with_signals(signal::take_next)
        .ok_or("the hand-off check's first thread had no signal to take")?;
    if taken.signal != SIGUSR2 {
        return Err("a thread did not take a signal of its own before its process's");
    }
    let _ = crate::syscall::deliver::enter_handler_for(&first, &taken, 0);
    if process.take_handed_to() != tid {
        return Err(
            "a signal pending for a process was not handed on when a handler's mask blocked it in \
             the thread that would take it",
        );
    }
    let _ = memory::sys_munmap(&process, page, PAGE_SIZE);
    Ok(())
}

// ---------------------------------------------------------------------------
// A program's own writes
//
// Stage 6's permissions and sharing, tested from the side where they matter. A
// check that writes user memory from the kernel goes through the direct map: it
// sees no permission bit and nothing a processor cached, so it passes against a
// stale translation that a program's own write would go straight through. The
// writes below are the programs' own.
// ---------------------------------------------------------------------------

/// What [`arch::USER_COW_PROGRAM`] exits with when everything is right.
const COW_STATUS: i32 = 61;

/// A forked child and its parent each write a page the other still shares
/// copy-on-write, and neither sees the other's write.
///
/// Each write is the program's own instruction faulting on the read-only entry
/// its read of the page left, so the copy, the replacement of a live
/// translation and the invalidation after it are all on the path. A fault that
/// mapped the shared frame writable instead of copying it would pass every
/// kernel-side check of `fork`; it fails this one.
///
/// Verifies: H.MEM.8, L.user.61
fn check_a_copy_on_write_page_is_copied_for_the_side_that_writes()
-> Result<Option<i32>, &'static str> {
    if arch::USER_COW_PROGRAM.is_empty() {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_COW_PROGRAM,
    );
    let status = exec::run(&file, &[b"/cow"], &[], [0x5a; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a program that writes after fork could not be started")?;
    match status {
        COW_STATUS => Ok(Some(status)),
        12 => Err("a parent's write to a page it shared copy-on-write showed in its child"),
        5 => Err("a child's write to a page it shared copy-on-write showed in its parent"),
        6 | 13 => Err("a write to a copy-on-write page did not stick for the side that made it"),
        4 => Err("a private page did not hold after fork what was written to it before"),
        10 => Err("the child of a program that writes after fork was killed by a signal"),
        97 => Err("mmap of two private anonymous pages was refused"),
        11 | 98 => Err("pipe2, fork, read, write or wait4 was refused in a program that forks"),
        _ => Err(
            "a program whose parent and child each write a page they share copy-on-write did \
             not exit with 61",
        ),
    }
}

/// What [`arch::USER_SHARED_PROGRAM`] exits with when everything is right.
const SHARED_STATUS: i32 = 62;

/// A forked child's writes to `MAP_SHARED` anonymous pages reach its parent,
/// and its write to a `MAP_PRIVATE` page does not.
///
/// One of the shared pages is first touched by the child, so the page the
/// child's fault commits has to land in the object both processes name rather
/// than in a copy of it.
///
/// Verifies: H.MEM.16, L.user.62
fn check_a_shared_mapping_is_shared_across_fork() -> Result<Option<i32>, &'static str> {
    if arch::USER_SHARED_PROGRAM.is_empty() {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_SHARED_PROGRAM,
    );
    let status = exec::run(
        &file,
        &[b"/shared"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that shares a mapping with its child could not be started")?;
    match status {
        SHARED_STATUS => Ok(Some(status)),
        2 => Err("a child's write to a MAP_SHARED anonymous page did not reach its parent"),
        3 => Err(
            "a MAP_SHARED anonymous page first touched by a child was not the page its parent \
             reads",
        ),
        4 => Err("a child's write to a MAP_PRIVATE anonymous page reached its parent"),
        10..=20 => Err("the child of a program that shares a mapping did not exit with 0"),
        97 => Err("mmap of a MAP_SHARED or MAP_PRIVATE anonymous mapping was refused"),
        98 => Err("fork or wait4 was refused in a program that shares a mapping"),
        _ => Err("a program that shares a mapping with its child did not exit with 62"),
    }
}

/// What [`arch::USER_MPROTECT_PROGRAM`] is ended with when everything is
/// right: death by `SIGSEGV`.
const MPROTECT_STATUS: i32 = 128 + ferrix_linux_abi::types::SIGSEGV as i32;

/// A program writes a page, makes it read-only with `mprotect`, writes it
/// again, and is ended by `SIGSEGV`.
///
/// The first write leaves a writable translation in the processor's TLB, and
/// only an invalidation takes it out: `mprotect` removing the entry from the
/// tables is not enough, because a processor holding the old entry never walks
/// them. Without the invalidation the second write goes through and the program
/// exits with 1.
///
/// **Pinned to a processor other than this one**, which is what makes the
/// result the same on every boot. Alone there, the program is neither
/// preempted nor moved between its two writes -- this checker only waits -- so
/// the processor that took the first write takes the second, and no switch in
/// between reloads its root and drops the entry by accident. A program free to
/// move would pass without the invalidation whenever it was switched between
/// the two, which is the stale entry hidden rather than removed.
///
/// Verifies: L.user.87
fn check_a_write_after_mprotect_read_only_faults() -> Result<Option<i32>, &'static str> {
    if arch::USER_MPROTECT_PROGRAM.is_empty() {
        return Ok(None);
    }
    let here = crate::smp::this_cpu()
        .ok_or("no processor to run a program on")?
        .logical;
    let count = crate::smp::count();
    let elsewhere = if count > 1 { (here + 1) % count } else { here };

    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_MPROTECT_PROGRAM,
    );
    let program = process::load(
        &file,
        &[b"/mprotect"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that narrows its own mapping could not be loaded")?;
    let _task = process::start_on(&program, Some(elsewhere))
        .map_err(|_| "a program that narrows its own mapping could not be started")?;
    let deadline = crate::timer::now_nanos().saturating_add(PROGRAM_PATIENCE_NANOS);
    match program.wait_for_exit(deadline) {
        Some(MPROTECT_STATUS) => Ok(Some(MPROTECT_STATUS)),
        Some(1) => Err(
            "a program wrote a page it had just made read-only with mprotect: a writable \
             translation outlived the change in a processor's TLB",
        ),
        Some(97) => Err("mmap of one private anonymous page was refused"),
        Some(98) => Err("mprotect to PROT_READ was refused"),
        Some(_) => {
            Err("a program that wrote a page it had made read-only was not ended by SIGSEGV")
        }
        None => Err("a program that narrows its own mapping never ended"),
    }
}

/// `kill` finds a process by pid, refuses a signal past 64, and discards a
/// signal the process ignores by default rather than leaving it pending; its
/// thread forms refuse a thread that does not exist -- which, for a process
/// with no thread, is even the one numbered by its pid.
///
/// Against a process with no task, so nothing sent here is ever delivered:
/// signal zero, which only asks, and `SIGCHLD`, which is ignored.
fn check_kill_finds_its_targets_and_refuses_what_it_should(
    process: &Process,
) -> Result<(), &'static str> {
    use crate::syscall::kill;
    use ferrix_linux_abi::types::SIGCHLD;

    let pid = i32::try_from(process.pid()).map_err(|_| "a pid does not fit an int")?;
    if kill::sys_kill(process, pid, 0) != Ok(0) {
        return Err("kill with signal zero did not find the process by its pid");
    }
    if kill::sys_kill(process, i32::MAX, 0) != Err(Errno::ESRCH) {
        return Err("kill of a pid nobody has was not ESRCH");
    }
    if kill::sys_kill(process, pid, 65) != Err(Errno::EINVAL) {
        return Err("kill with signal 65 was not EINVAL");
    }
    if kill::sys_tgkill(process, pid, pid.saturating_add(1), 0) != Err(Errno::ESRCH) {
        return Err("tgkill of a thread that is not its process's was not ESRCH");
    }
    if kill::sys_tkill(process, 0, 0) != Err(Errno::EINVAL) {
        return Err("tkill of thread zero was not EINVAL");
    }
    if kill::sys_tgkill(process, pid, pid, 0) != Err(Errno::ESRCH) {
        return Err("tgkill found a thread in a process that has none");
    }
    if kill::sys_kill(process, pid, SIGCHLD) != Ok(0)
        || process.with_signals(|signals| signals.pending()) != 0
    {
        return Err("SIGCHLD, ignored by default, was left pending instead of discarded");
    }
    Ok(())
}

/// Where [`arch::USER_EXEC_PROGRAM`] looks for its target: a symbolic link,
/// in this check, to [`EXEC_REAL`].
const EXEC_TARGET: &[u8] = b"/exec-target";

/// The file [`EXEC_TARGET`] links to, which is the one actually run.
const EXEC_REAL: &[u8] = b"/exec-target-real";

/// A program `execve`s another by path and takes on its status; with the file
/// gone, the same program gets `ENOENT` back and exits with it.
///
/// The path it asks for is a symbolic link, and the process must end up
/// recorded as the file the link names, absolute -- what `/proc/self/exe`
/// reports, and what glibc's static startup asserts is absolute. The program
/// passes the link's own name as `argv[0]`, so recording that instead fails
/// here.
/// Verifies: `L.x86_64.69`
fn check_execve_replaces_the_program() -> Result<Option<(i32, i32)>, &'static str> {
    use ferrix_vfs::OpenFlags;

    if arch::USER_EXEC_PROGRAM.is_empty() {
        return Ok(None);
    }
    let class = class_of_this_build();
    let machine = arch::ARCH.elf_machine();
    let target = image::build_with(class, machine, image::Shape::Good, arch::USER_TEST_PROGRAM);
    let caller = image::build_with(class, machine, image::Shape::Good, arch::USER_EXEC_PROGRAM);

    let ns = crate::fs::namespace();
    let ctx = ns.context();
    let create = OpenFlags {
        read: false,
        write: true,
        create: true,
        exclusive: false,
        truncate: true,
        append: false,
        directory: false,
        nofollow: false,
        path: false,
        nonblock: false,
    };
    let file = ns
        .open(&ctx, None, EXEC_REAL, &create, 0o755)
        .map_err(|_| "could not create the program execve is to run")?;
    if file.write(&target) != Ok(target.len()) {
        return Err("could not write the program execve is to run");
    }
    drop(file);
    ns.symlink(&ctx, None, EXEC_TARGET, EXEC_REAL)
        .map_err(|_| "could not link to the program execve is to run")?;

    // Loaded and waited for by hand rather than through `exec::run`, to keep
    // the process and read what it was recorded as after it has ended.
    let execing = exec::load(
        &caller,
        &[b"/exec-caller"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that calls execve could not be loaded")?;
    let task =
        process::start(&execing).map_err(|_| "a program that calls execve could not be started")?;
    let found = execing
        .wait_for_exit(u64::MAX)
        .ok_or("a program that calls execve never reported how it ended")?;
    drop(task);
    let recorded = execing.exe();
    drop(execing);
    ns.unlink(&ctx, None, EXEC_TARGET)
        .map_err(|_| "could not remove the link execve ran through")?;
    ns.unlink(&ctx, None, EXEC_REAL)
        .map_err(|_| "could not remove the program execve ran")?;
    let missing = exec::run(
        &caller,
        &[b"/exec-caller"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "a program that calls execve could not be started a second time")?;

    if found != arch::USER_TEST_STATUS {
        return Err("a program that called execve did not end with the new program's status");
    }
    if recorded != EXEC_REAL {
        return Err(
            "execve through a symbolic link did not record the absolute path of the file it ran",
        );
    }
    if missing != Errno::ENOENT.0 as i32 {
        return Err("execve of a path that does not exist did not return ENOENT");
    }
    Ok(Some((found, missing)))
}

// ---------------------------------------------------------------------------
// Futexes
//
// Through the handler, on a word in a process the check builds, with a kernel
// task as the sleeper: a kernel task can sleep in `futex` as well as a
// program can, and needs no program assembled for three architectures to do
// it. The wake is the part worth a second task, and it is checked twice: once
// working, and once with a wake that takes its waiter off the table and
// reports it woken without rousing it. That second run must fail, and fail
// for that reason -- a check that only compared the count a wake returns
// would pass it.
// ---------------------------------------------------------------------------

use ferrix_linux_abi::types::{
    FUTEX_CLOCK_REALTIME, FUTEX_CMP_REQUEUE, FUTEX_PRIVATE_FLAG, FUTEX_WAIT, FUTEX_WAIT_BITSET,
    FUTEX_WAKE, FUTEX_WAKE_OP,
};

use crate::sched::WaitQueue;
use crate::syscall::futex;

/// What the check's futex word holds.
const FUTEX_WORD: u32 = 0x0F07_E100;

/// The timeout of a wait that should time out.
const FUTEX_SHORT_NANOS: u64 = 20_000_000;

/// How long the waiter a wake is meant for will sleep, in seconds. Far longer
/// than a working wake takes to arrive, even on a host that stalls the
/// machine for a while, and never actually waited out: the wake comes first.
const FUTEX_LONG_SECONDS: u64 = 2;

/// How long the negative control's waiter sleeps, which it does in full: no
/// wake is coming, and the check requires it to time out. Far longer than the
/// millisecond it takes to reach the table, where it must be found before it
/// is forgotten; far shorter than the two seconds it used to be, which were
/// the boot's single most expensive check and proved nothing extra.
const FUTEX_FORGOTTEN_NANOS: u64 = 200_000_000;

/// How long the check waits for its waiter to start waiting, or to return.
const FUTEX_PATIENCE_NANOS: u64 = 30_000_000_000;

/// The failure a wake that never rouses its waiter produces, which the
/// negative control requires by name.
const SLEPT_THROUGH_WAKE: &str = "a futex waiter the wake counted slept on to its timeout";

/// The failure a wake that did not find its waiter produces, which the shared
/// check's negative control requires by name.
const WAKE_MISSED_ITS_WAITER: &str = "a futex wake did not report the one waiter it had";

/// What the waiting task waits on: the process, the word, the timeout, and
/// the operation with its flags.
type FutexSubject = (Arc<Process>, u64, u64, u32);

/// The [`FutexSubject`] the waiting task takes when it starts.
static FUTEX_SUBJECT: crate::sync::SpinLock<Option<FutexSubject>> =
    crate::sync::SpinLock::new(None);

/// What the waiting task's `FUTEX_WAIT` answered.
static FUTEX_ANSWER: crate::sync::SpinLock<Option<Result<usize, Errno>>> =
    crate::sync::SpinLock::new(None);

/// Woken when it has answered.
static FUTEX_ANSWERED: WaitQueue = WaitQueue::new();

/// One wake, as the check applies it to the word its waiter sleeps on.
type FutexWake = fn(&Process, u64) -> Result<usize, Errno>;

/// `futex` with six arguments and a native timespec.
fn futex_call(process: &Process, a: [u64; 6]) -> Result<usize, Errno> {
    futex::sys_futex(process, &a, time::TimeWidth::Native)
}

/// Write a native `struct timespec` at `at`.
fn write_timespec(
    process: &Process,
    at: u64,
    seconds: u64,
    nanos: u64,
) -> Result<(), &'static str> {
    write_word(process, at, seconds)?;
    write_word(process, at + size_of::<usize>() as u64, nanos)
}

/// A wait on a changed word is `EAGAIN`, a timed wait nobody wakes is
/// `ETIMEDOUT` and not early, what is not implemented says so, and a waiter
/// is roused by a wake and by a requeue followed by a wake -- but not by a
/// wake that only pretends. Answers how many waiters were roused.
fn check_futexes() -> Result<usize, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the futex check")?;
    let page = map_rw(&process, PAGE_SIZE)?;
    let word = page;
    let timeout = page + 16;
    uaccess::copy_to_user(process.space(), word, &FUTEX_WORD.to_le_bytes())
        .map_err(|_| "could not stage the futex word")?;
    let wait = u64::from(FUTEX_WAIT | FUTEX_PRIVATE_FLAG);
    let expected = u64::from(FUTEX_WORD);

    if futex_call(&process, [word, wait, expected + 1, 0, 0, 0]) != Err(Errno::EAGAIN) {
        return Err("FUTEX_WAIT on a word that had changed did not answer EAGAIN");
    }
    if futex_call(&process, [word + 1, wait, expected, 0, 0, 0]) != Err(Errno::EINVAL) {
        return Err("FUTEX_WAIT on a misaligned word was not EINVAL");
    }

    write_timespec(&process, timeout, 0, FUTEX_SHORT_NANOS)?;
    let started = crate::timer::now_nanos();
    if futex_call(&process, [word, wait, expected, timeout, 0, 0]) != Err(Errno::ETIMEDOUT) {
        return Err("a timed FUTEX_WAIT nobody woke did not answer ETIMEDOUT");
    }
    if crate::timer::now_nanos().saturating_sub(started) < FUTEX_SHORT_NANOS {
        return Err("a timed FUTEX_WAIT came back before its timeout");
    }

    // Absolute, one nanosecond after the counter started: long past.
    write_timespec(&process, timeout, 0, 1)?;
    let bitset_wait = u64::from(FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    if futex_call(&process, [word, bitset_wait, expected, timeout, 0, 1]) != Err(Errno::ETIMEDOUT) {
        return Err("FUTEX_WAIT_BITSET with a deadline already past did not answer ETIMEDOUT");
    }
    if futex_call(&process, [word, bitset_wait, expected, timeout, 0, 0]) != Err(Errno::EINVAL) {
        return Err("FUTEX_WAIT_BITSET with an empty bitset was not EINVAL");
    }
    let realtime_wake = u64::from(FUTEX_WAKE | FUTEX_CLOCK_REALTIME);
    if futex_call(&process, [word, realtime_wake, 1, 0, 0, 0]) != Err(Errno::ENOSYS) {
        return Err("FUTEX_WAKE accepted FUTEX_CLOCK_REALTIME");
    }
    if futex_call(
        &process,
        [word, u64::from(FUTEX_WAKE_OP), 1, 1, word + 4, 0],
    ) != Err(Errno::ENOSYS)
    {
        return Err("FUTEX_WAKE_OP, which is not implemented, did not answer ENOSYS");
    }
    if futex_call(&process, [word, u64::from(FUTEX_WAKE), 1, 0, 0, 0]) != Ok(0) {
        return Err("FUTEX_WAKE with nobody waiting did not answer zero");
    }
    let cmp_requeue = u64::from(FUTEX_CMP_REQUEUE);
    if futex_call(&process, [word, cmp_requeue, 1, 1, word + 4, expected + 1]) != Err(Errno::EAGAIN)
    {
        return Err("FUTEX_CMP_REQUEUE on a word that had changed did not answer EAGAIN");
    }

    let mut woken = wait_then_wake(
        &process,
        word,
        timeout,
        (FUTEX_LONG_SECONDS, 0),
        |process, word| {
            futex_call(
                process,
                [word, u64::from(FUTEX_WAKE | FUTEX_PRIVATE_FLAG), 1, 0, 0, 0],
            )
        },
    )?;
    // Moved to the next word, then woken there: the requeue must have taken
    // it, since nothing else wakes the second word.
    woken += wait_then_wake(
        &process,
        word,
        timeout,
        (FUTEX_LONG_SECONDS, 0),
        |process, word| {
            let requeue = u64::from(FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG);
            let moved = futex_call(
                process,
                [word, requeue, 0, 1, word + 4, u64::from(FUTEX_WORD)],
            )?;
            if moved != 1 || futex::waiters_on(process, word) != 0 {
                return Ok(0);
            }
            futex_call(
                process,
                [
                    word + 4,
                    u64::from(FUTEX_WAKE | FUTEX_PRIVATE_FLAG),
                    1,
                    0,
                    0,
                    0,
                ],
            )
        },
    )?;

    a_forgotten_waiter_is_caught(&process, word, timeout)?;

    let _ = memory::sys_munmap(&process, page, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    woken += a_shared_futex_crosses_a_fork()?;
    Ok(woken)
}

/// Bytes the bucket check maps: four pages, a word in which is all but
/// certain to hash to another bucket than the first word's.
const BUCKET_CHECK_BYTES: u64 = 4 * PAGE_SIZE;

/// Where the bucket check's words start, past the timeout at 16.
const BUCKET_CHECK_FIRST: u64 = 64;

/// The first word after `word`, in the pages the bucket check maps, whose
/// private key hashes to another bucket of the futex table than `word`'s.
fn a_word_in_another_bucket(process: &Process, word: u64) -> Option<u64> {
    let bucket = futex::bucket_of(process, word);
    (word + 4..word - BUCKET_CHECK_FIRST + BUCKET_CHECK_BYTES)
        .step_by(4)
        .find(|&other| futex::bucket_of(process, other) != bucket)
}

/// `FUTEX_CMP_REQUEUE` of one private waiter from `word` to `target`.
fn requeue_one(process: &Process, word: u64, target: u64) -> Result<usize, Errno> {
    let requeue = u64::from(FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG);
    futex_call(
        process,
        [word, requeue, 0, 1, target, u64::from(FUTEX_WORD)],
    )
}

/// A private `FUTEX_WAKE` of one waiter on `word`.
fn wake_one(process: &Process, word: u64) -> Result<usize, Errno> {
    futex_call(
        process,
        [word, u64::from(FUTEX_WAKE | FUTEX_PRIVATE_FLAG), 1, 0, 0, 0],
    )
}

/// The futex table is in buckets by key, and a requeue between two words in
/// different buckets must move its waiter into the other one: a wake on the
/// second word then rouses it, and neither word has anyone left on it. A
/// waiter requeued and then left to time out must take its entry out of the
/// bucket it was moved to, not look for it in the one it started in. The
/// negative control requeues by changing the key and leaving the entry where
/// it was -- the bug a table in buckets invites -- and the wake on the second
/// word must be caught missing its waiter.
fn a_requeue_crosses_buckets() -> Result<(), &'static str> {
    let process = process::new_for_check()
        .map_err(|_| "could not make a process for the futex bucket check")?;
    let page = map_rw(&process, BUCKET_CHECK_BYTES)?;
    let word = page + BUCKET_CHECK_FIRST;
    let timeout = page + 16;
    let target = a_word_in_another_bucket(&process, word)
        .ok_or("no futex word in four pages hashed to another bucket than the first")?;
    for at in [word, target] {
        uaccess::copy_to_user(process.space(), at, &FUTEX_WORD.to_le_bytes())
            .map_err(|_| "could not stage the futex bucket check's words")?;
    }
    let (from, to) = (
        futex::bucket_of(&process, word),
        futex::bucket_of(&process, target),
    );

    let _ = wait_then_wake(
        &process,
        word,
        timeout,
        (FUTEX_LONG_SECONDS, 0),
        |process, word| {
            let Some(target) = a_word_in_another_bucket(process, word) else {
                return Ok(0);
            };
            let moved = requeue_one(process, word, target)?;
            if moved != 1
                || futex::waiters_on(process, word) != 0
                || futex::waiters_on(process, target) != 1
            {
                return Ok(0);
            }
            wake_one(process, target)
        },
    )?;
    if futex::waiters_on(&process, word) != 0 || futex::waiters_on(&process, target) != 0 {
        return Err("a waiter requeued to another futex bucket and woken there left an entry");
    }

    match wait_then_wake(
        &process,
        word,
        timeout,
        (0, FUTEX_FORGOTTEN_NANOS),
        |process, word| match a_word_in_another_bucket(process, word) {
            Some(target) => requeue_one(process, word, target),
            None => Ok(0),
        },
    ) {
        Err(problem) if problem == SLEPT_THROUGH_WAKE => {}
        Err(_) => {
            return Err("a futex waiter requeued and left to time out failed for another reason");
        }
        Ok(_) => return Err("a futex waiter requeued and never woken came back woken"),
    }
    if futex::waiters_on(&process, word) != 0 || futex::waiters_on(&process, target) != 0 {
        return Err(
            "a futex waiter requeued to another bucket and timed out left its entry behind",
        );
    }

    match wait_then_wake(
        &process,
        word,
        timeout,
        (0, FUTEX_FORGOTTEN_NANOS),
        |process, word| {
            let Some(target) = a_word_in_another_bucket(process, word) else {
                return Ok(0);
            };
            if futex::requeue_without_moving(process, word, target) != 1 {
                return Ok(0);
            }
            wake_one(process, target)
        },
    ) {
        Err(problem) if problem == WAKE_MISSED_ITS_WAITER => {}
        Err(_) => {
            return Err(
                "the futex bucket check failed a requeue left in place, for another reason",
            );
        }
        Ok(_) => return Err("the futex bucket check passed a requeue that never moved its waiter"),
    }
    if futex::waiters_on(&process, word) != 0 || futex::waiters_on(&process, target) != 0 {
        return Err("the negative control's waiter left an entry on the futex table");
    }

    let _ =
        memory::sys_munmap(&process, page, BUCKET_CHECK_BYTES).map_err(|_| "munmap was refused")?;
    println!(
        "  futex    a waiter requeued from bucket {from} to bucket {to} was woken there, one \
         left to time out took its entry out of bucket {to}, and a requeue that left its waiter \
         in bucket {from} was caught"
    );
    Ok(())
}

/// A futex in `MAP_SHARED` memory is one futex on both sides of a `fork`: a
/// waiter in the parent that left out `FUTEX_PRIVATE_FLAG` is roused by the
/// child waking the same word, which it names through its own mapping.
///
/// The negative control comes first, on the same word and by the same path:
/// the child's wake with `FUTEX_PRIVATE_FLAG`, keyed by the child's own space
/// -- which is how every futex was keyed before shared keys existed -- must
/// find nobody, and the check must say so by name. Answers one.
///
/// Verifies: L.user.78
fn a_shared_futex_crosses_a_fork() -> Result<usize, &'static str> {
    let parent = process::new_for_check()
        .map_err(|_| "could not make a process for the shared futex check")?;
    let page = map_shared_rw(&parent, PAGE_SIZE)?;
    let (word, timeout) = (page, page + 16);
    uaccess::copy_to_user(parent.space(), word, &FUTEX_WORD.to_le_bytes())
        .map_err(|_| "could not stage the shared futex word")?;
    let child = process::fork_for_check(&parent)
        .map_err(|_| "could not fork a process for the shared futex check")?;
    let shared_wait = FUTEX_WAIT;

    match wait_then_wake_from(
        (&parent, &child),
        word,
        timeout,
        (0, FUTEX_FORGOTTEN_NANOS),
        shared_wait,
        |waking, word| {
            futex_call(
                waking,
                [word, u64::from(FUTEX_WAKE | FUTEX_PRIVATE_FLAG), 1, 0, 0, 0],
            )
        },
    ) {
        Err(problem) if problem == WAKE_MISSED_ITS_WAITER => {}
        Err(_) => {
            return Err(
                "the shared futex check failed a wake keyed by the waker's own space, for another reason",
            );
        }
        Ok(_) => {
            return Err(
                "a futex wake keyed by the waker's own space roused a waiter in its parent",
            );
        }
    }

    let woken = wait_then_wake_from(
        (&parent, &child),
        word,
        timeout,
        (FUTEX_LONG_SECONDS, 0),
        shared_wait,
        |waking, word| futex_call(waking, [word, u64::from(FUTEX_WAKE), 1, 0, 0, 0]),
    )
    .map_err(|problem| {
        if problem == WAKE_MISSED_ITS_WAITER {
            "a futex wake in a fork child did not find its parent's waiter on a MAP_SHARED word"
        } else {
            problem
        }
    })?;

    for process in [&child, &parent] {
        let _ = memory::sys_munmap(process, page, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    }
    Ok(woken)
}

/// The negative control: a wake that counts a waiter but never rouses it
/// must be reported as exactly that, by the same path the positive checks
/// use. The waiter sleeps its whole, short, timeout.
fn a_forgotten_waiter_is_caught(
    process: &Arc<Process>,
    word: u64,
    timeout: u64,
) -> Result<(), &'static str> {
    match wait_then_wake(
        process,
        word,
        timeout,
        (0, FUTEX_FORGOTTEN_NANOS),
        |process, word| Ok(futex::forget_waiters(process, word, 1)),
    ) {
        Err(problem) if problem == SLEPT_THROUGH_WAKE => Ok(()),
        Err(_) => Err("the futex check failed a wake that roused nobody, for another reason"),
        Ok(_) => Err("the futex check passed a wake that never roused its waiter"),
    }
}

/// Start a task sleeping in `FUTEX_WAIT` with `FUTEX_PRIVATE_FLAG` on `word`
/// for `sleep` (seconds, nanoseconds), wait until it is on the table, `wake`
/// it from the same process, and require it back with zero and the wake to
/// have counted it. Answers one.
fn wait_then_wake(
    process: &Arc<Process>,
    word: u64,
    timeout: u64,
    sleep: (u64, u64),
    wake: FutexWake,
) -> Result<usize, &'static str> {
    wait_then_wake_from(
        (process, process),
        word,
        timeout,
        sleep,
        FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
        wake,
    )
}

/// [`wait_then_wake`] with the waiter in `waiting` sleeping by `wait` -- the
/// operation and its flags -- and the wake made from `waking`, which may be
/// another process mapping the same word.
fn wait_then_wake_from(
    (waiting, waking): (&Arc<Process>, &Arc<Process>),
    word: u64,
    timeout: u64,
    sleep: (u64, u64),
    wait: u32,
    wake: FutexWake,
) -> Result<usize, &'static str> {
    let process = waiting;
    write_timespec(process, timeout, sleep.0, sleep.1)?;
    *FUTEX_ANSWER.lock() = None;
    *FUTEX_SUBJECT.lock() = Some((Arc::clone(process), word, timeout, wait));
    let waiter = crate::sched::spawn("futex-waiter", futex_waiter, 0, ferrix_sched::NICE_0_WEIGHT)?;

    let deadline = crate::timer::now_nanos().saturating_add(FUTEX_PATIENCE_NANOS);
    while futex::waiters_on(process, word) == 0 {
        if FUTEX_ANSWER.lock().is_some() {
            return Err("a futex waiter returned without ever waiting");
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("a futex waiter never started waiting");
        }
        crate::sched::sleep_for(1_000_000);
    }
    let count = wake(waking, word);

    let deadline = crate::timer::now_nanos().saturating_add(FUTEX_PATIENCE_NANOS);
    let _ = FUTEX_ANSWERED.wait_until_deadline(|| FUTEX_ANSWER.lock().is_some(), deadline);
    let answer = FUTEX_ANSWER.lock().take();
    drop(waiter);
    match (count, answer) {
        (_, None) => Err("a futex waiter never came back"),
        (Ok(1), Some(Ok(0))) => Ok(1),
        (Ok(1), Some(Err(error))) if error == Errno::ETIMEDOUT => Err(SLEPT_THROUGH_WAKE),
        (Ok(1), Some(_)) => Err("a woken futex waiter did not answer zero"),
        (_, Some(_)) => Err(WAKE_MISSED_ITS_WAITER),
    }
}

/// The waiting task: one `FUTEX_WAIT` on what [`FUTEX_SUBJECT`] names.
fn futex_waiter(_argument: usize) {
    let subject = FUTEX_SUBJECT.lock().take();
    let answer = match subject {
        Some((process, word, timeout, wait)) => futex_call(
            &process,
            [word, u64::from(wait), u64::from(FUTEX_WORD), timeout, 0, 0],
        ),
        None => Err(Errno::ESRCH),
    };
    *FUTEX_ANSWER.lock() = Some(answer);
    FUTEX_ANSWERED.wake_all();
}

// ---------------------------------------------------------------------------
// The heap lock
//
// `brk` writes the heap's new end before it unmaps a shrunk tail, because the
// unmap waits for a shootdown and the state lock may not be held across one.
// A `fork` or a growing `brk` on another thread between the two would see a
// heap that says it ends below pages still mapped. Both take the heap lock,
// a lock that may sleep, across the whole of it; this shows each waits for the
// other. The lock is held here by the check, the way either would hold it,
// and a task makes the other call: it must not finish while the lock is held,
// and must finish once it goes. A call that skipped the lock finishes at once,
// and fails by name.
// ---------------------------------------------------------------------------

/// Which call the heap task makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeapCall {
    /// Shrink the heap by `brk`.
    Shrink,
    /// Copy the space as `fork` does.
    Fork,
    /// Map a page anywhere by `mmap`, while the space's layout is held
    /// rather than the heap lock.
    Map,
}

/// How long the heap task is given to go past the lock, once started, before
/// the check decides it waited. A call that skipped the lock ends in well
/// under this even under a slow host.
const HEAP_HELD_NANOS: u64 = 50_000_000;

/// How long the check waits for its heap task to start or to finish.
const HEAP_PATIENCE_NANOS: u64 = 30_000_000_000;

/// The failure an `mmap` that does not wait for the space's layout produces.
const MMAP_IGNORED_LAYOUT: &str =
    "an mmap changed the address space while another call held its layout";

/// The failure a `brk` that does not wait for the heap lock produces.
const BRK_IGNORED_HEAP_LOCK: &str = "a brk shrank the heap while a fork held the heap lock";

/// The failure a `fork` that does not wait for the heap lock produces.
const FORK_IGNORED_HEAP_LOCK: &str = "a fork copied the space while a brk held the heap lock";

/// What the heap task works on, taken when it starts.
static HEAP_SUBJECT: crate::sync::SpinLock<Option<(Arc<Process>, HeapCall, u64)>> =
    crate::sync::SpinLock::new(None);

/// Raised by the heap task just before its call.
static HEAP_STARTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// What the heap task's call answered: the new break, or the copy's region
/// count.
static HEAP_ANSWER: crate::sync::SpinLock<Option<u64>> = crate::sync::SpinLock::new(None);

/// A `brk` waits for a `fork` holding the heap lock, and a `fork` for a `brk`;
/// and an `mmap` waits for a call holding the space's layout, which is what
/// makes plain `MAP_FIXED`'s unmap and map one step to every other thread.
fn check_brk_and_fork_wait_for_the_heap_lock() -> Result<(), &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the heap lock check")?;
    process.set_heap_base(MMAP_MIN_ADDR * 16);
    let (start, _) = process
        .heap_range()
        .ok_or("the heap lock check's process has no heap")?;
    let grown = start + 4 * PAGE_SIZE;
    if process.set_break(grown) != grown {
        return Err("brk did not grow the heap lock check's heap");
    }
    let shrunk = start + PAGE_SIZE;
    let answer = call_past_the_heap_lock(&process, HeapCall::Shrink, shrunk)?;
    if answer != shrunk {
        return Err("a brk that waited for the heap lock did not shrink the heap");
    }
    let _ = call_past_the_heap_lock(&process, HeapCall::Fork, 0)?;
    if call_past_the_heap_lock(&process, HeapCall::Map, 0)? == 0 {
        return Err("an mmap that waited for the layout was refused");
    }
    Ok(())
}

/// Hold `process`'s heap lock, or for [`HeapCall::Map`] its space's layout,
/// have a task make `call`, require it not to finish while the lock is held
/// and to finish once it goes. Answers what the call answered.
fn call_past_the_heap_lock(
    process: &Arc<Process>,
    call: HeapCall,
    want: u64,
) -> Result<u64, &'static str> {
    HEAP_STARTED.store(false, Ordering::SeqCst);
    *HEAP_ANSWER.lock() = None;
    *HEAP_SUBJECT.lock() = Some((Arc::clone(process), call, want));
    let held = match call {
        HeapCall::Map => process.space().layout(),
        HeapCall::Shrink | HeapCall::Fork => process.hold_heap_for_check(),
    };
    let task = crate::sched::spawn("heap-lock", heap_caller, 0, ferrix_sched::NICE_0_WEIGHT)?;

    let deadline = crate::timer::now_nanos().saturating_add(HEAP_PATIENCE_NANOS);
    while !HEAP_STARTED.load(Ordering::SeqCst) {
        if crate::timer::now_nanos() >= deadline {
            return Err("the heap lock check's task never started");
        }
        crate::sched::sleep_for(1_000_000);
    }
    crate::sched::sleep_for(HEAP_HELD_NANOS);
    let early = HEAP_ANSWER.lock().is_some();
    drop(held);

    let deadline = crate::timer::now_nanos().saturating_add(HEAP_PATIENCE_NANOS);
    while !task.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err("the heap lock check's task never finished");
        }
        crate::sched::sleep_for(1_000_000);
    }
    if early {
        return Err(match call {
            HeapCall::Shrink => BRK_IGNORED_HEAP_LOCK,
            HeapCall::Fork => FORK_IGNORED_HEAP_LOCK,
            HeapCall::Map => MMAP_IGNORED_LAYOUT,
        });
    }
    HEAP_ANSWER
        .lock()
        .take()
        .ok_or("the heap lock check's task finished without answering")
}

/// The heap task: one `brk`, one space copy or one `mmap` on what
/// [`HEAP_SUBJECT`] names.
fn heap_caller(_argument: usize) {
    let Some((process, call, want)) = HEAP_SUBJECT.lock().take() else {
        return;
    };
    HEAP_STARTED.store(true, Ordering::SeqCst);
    let answer = match call {
        HeapCall::Shrink => Some(process.set_break(want)),
        HeapCall::Fork => process
            .fork_memory(|space| space.region_count() as u64)
            .ok(),
        HeapCall::Map => map_rw(&process, PAGE_SIZE).ok(),
    };
    *HEAP_ANSWER.lock() = answer;
}

// ---------------------------------------------------------------------------
// What a busybox applet asks of the system
//
// `free`, `ulimit`, `nproc`, `renice`, `hostname`, `date -s`, `sleep`: each is
// one or two calls about the machine, its limits or its clocks, and each call
// writes a structure whose size changes with the word. So every check below
// poisons the buffer first and requires the byte after the structure to
// survive -- a handler that wrote the 64-bit layout on ARMv7-A fails here, not
// under a program that reads the wrong field and prints nonsense.
// ---------------------------------------------------------------------------

/// The byte the checks below fill a buffer with before a handler writes it.
const UNWRITTEN: u8 = 0xAA;

/// The system, limit, clock, credential and socket calls, on one page.
fn check_what_an_applet_asks_of_the_system(process: &Process) -> Result<(), &'static str> {
    let page = map_rw(process, PAGE_SIZE)?;
    let outcome = check_sysinfo_describes_the_machine(process, page)
        .and_then(|()| check_limits_read_back_and_reach_the_descriptor_table(process, page))
        .and_then(|()| check_affinity_names_the_running_processors(process, page))
        .and_then(|()| check_a_task_name_round_trips(process, page))
        .and_then(|()| check_credentials_follow_linux_rules(process, page))
        .and_then(|()| check_nanosleep_takes_its_time(process, page))
        .and_then(|()| check_setting_the_clock_moves_only_realtime(process, page))
        .and_then(|()| check_a_host_name_reaches_uname(process, page))
        .and_then(|()| check_unix_sockets(process, page));
    let outcome = outcome.and_then(|()| check_time_is_the_realtime_seconds(process, page));
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome
}

/// Fill `len` bytes at `at` with [`UNWRITTEN`].
fn poison_user(process: &Process, at: u64, len: usize) -> Result<(), &'static str> {
    let bytes = [UNWRITTEN; 256];
    let span = bytes
        .get(..len)
        .ok_or("a poisoned span longer than the check allows")?;
    uaccess::copy_to_user(process.space(), at, span).map_err(|_| "could not poison a buffer")
}

/// Read `N` bytes back from `at`.
fn read_user<const N: usize>(process: &Process, at: u64) -> Result<[u8; N], &'static str> {
    let mut out = [0_u8; N];
    uaccess::copy_from_user(process.space(), at, &mut out)
        .map_err(|_| "could not read a buffer back")?;
    Ok(out)
}

/// A little-endian unsigned field of `width` bytes at `at`, or `u64::MAX` if
/// the field is not inside `bytes` -- which no check below expects to see.
fn le_at(bytes: &[u8], at: usize, width: usize) -> u64 {
    bytes.get(at..at + width).map_or(u64::MAX, |field| {
        field
            .iter()
            .rev()
            .fold(0, |value, &byte| value << 8 | u64::from(byte))
    })
}

/// Whether every byte of `bytes[from..to]` is `value`.
fn all_are(bytes: &[u8], from: usize, to: usize, value: u8) -> bool {
    bytes
        .get(from..to)
        .is_some_and(|span| span.iter().all(|&byte| byte == value))
}

/// `sysinfo` writes exactly `struct sysinfo`, its memory counts are the frame
/// allocator's in the unit it names, its loads are the load average's, and
/// every field it has nothing for is zero rather than whatever the buffer
/// held.
fn check_sysinfo_describes_the_machine(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::system::{SYSINFO_SIZE, sysinfo_at};
    const WORD: usize = size_of::<usize>();

    poison_user(process, page, 128)?;
    // The averages on either side of the call: a fold falls between them
    // at most once, every five seconds.
    let (_, before) = crate::fs::procfs::loadavg::now();
    answers(system::sys_sysinfo(process, page), 0, "sysinfo was refused")?;
    let (_, after) = crate::fs::procfs::loadavg::now();
    let out: [u8; 128] = read_user(process, page)?;

    let unit = le_at(&out, sysinfo_at::MEM_UNIT, 4);
    if unit != 1 && unit != PAGE_SIZE {
        return Err("sysinfo's mem_unit is neither a byte nor a page");
    }
    let total = le_at(&out, sysinfo_at::TOTALRAM, WORD).saturating_mul(unit);
    let free = le_at(&out, sysinfo_at::FREERAM, WORD).saturating_mul(unit);
    if total != mm::managed_frames() * PAGE_SIZE {
        return Err("sysinfo's total memory is not the frame allocator's");
    }
    if free == 0 || free > total {
        return Err("sysinfo's free memory is not a part of its total");
    }
    if le_at(&out, sysinfo_at::UPTIME, WORD) == 0 {
        return Err("sysinfo reported no uptime on a machine that has been up");
    }
    if le_at(&out, sysinfo_at::PROCS, 2) == 0 {
        return Err("sysinfo counted no processes while this one is registered");
    }
    // The loads, in sixteen fractional bits where the averages have eleven.
    let shift = 16 - ferrix_procfs::loadavg::FSHIFT;
    let loads = |averages: [u64; 3]| {
        (0..3).all(|index| {
            let word = le_at(&out, sysinfo_at::LOADS + index * WORD, WORD);
            averages
                .get(index)
                .is_some_and(|&load| word == load << shift)
        })
    };
    if !loads(before) && !loads(after) {
        return Err("sysinfo's loads are not the load average's");
    }
    // Shared, buffer and swap; the padding after procs; high memory; and the
    // tail after mem_unit. All zero, and all written.
    let zero = [
        (WORD * 6, WORD * 10),
        (sysinfo_at::PROCS + 2, WORD * 11),
        (WORD * 11, WORD * 13),
        (sysinfo_at::MEM_UNIT + 4, SYSINFO_SIZE),
    ];
    if !zero.iter().all(|&(from, to)| all_are(&out, from, to, 0)) {
        return Err("a sysinfo field with nothing to report was not written as zero");
    }
    if !all_are(&out, SYSINFO_SIZE, out.len(), UNWRITTEN) {
        return Err("sysinfo wrote past the end of this build's struct sysinfo");
    }
    refuses(
        system::sys_sysinfo(process, KERNEL_HALF_BASE),
        Errno::EFAULT,
        "sysinfo into a kernel address was not EFAULT",
    )
}

/// Stage a 16-byte `struct rlimit64` at `at`.
fn stage_rlimit64(process: &Process, at: u64, soft: u64, hard: u64) -> Result<(), &'static str> {
    let mut bytes = [0_u8; 16];
    let fields = soft.to_le_bytes().into_iter().chain(hard.to_le_bytes());
    for (slot, byte) in bytes.iter_mut().zip(fields) {
        *slot = byte;
    }
    uaccess::copy_to_user(process.space(), at, &bytes).map_err(|_| "could not stage an rlimit64")
}

/// `RLIMIT_NOFILE` is the descriptor table's limit in both directions, a limit
/// set through `setrlimit` reads back through `prlimit64`, and the refusals
/// are `do_prlimit`'s.
fn check_limits_read_back_and_reach_the_descriptor_table(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    use crate::syscall::limits::{sys_getrlimit, sys_prlimit64, sys_setrlimit};
    const WORD: usize = size_of::<usize>();
    const NOFILE: u32 = 7;
    let table_limit = || u64::from(process.files().lock().limit());
    let own = i32::try_from(process.pid()).map_err(|_| "a pid does not fit a pid_t")?;

    poison_user(process, page, WORD * 3)?;
    answers(
        sys_getrlimit(process, NOFILE, page),
        0,
        "getrlimit(RLIMIT_NOFILE) was refused",
    )?;
    let out: [u8; 24] = read_user(process, page)?;
    if le_at(&out, 0, WORD) != table_limit() || le_at(&out, WORD, WORD) < table_limit() {
        return Err("RLIMIT_NOFILE does not read as the descriptor table's limit");
    }
    if !all_are(&out, WORD * 2, WORD * 3, UNWRITTEN) {
        return Err("getrlimit wrote past this build's struct rlimit");
    }

    write_word(process, page, 64)?;
    write_word(process, page + WORD as u64, 128)?;
    answers(
        sys_setrlimit(process, NOFILE, page),
        0,
        "setrlimit(RLIMIT_NOFILE) was refused",
    )?;
    if table_limit() != 64 {
        return Err("setrlimit(RLIMIT_NOFILE) did not reach the descriptor table");
    }

    // prlimit64 by the caller's own pid: set {1024, 4096}, get back {64, 128}.
    stage_rlimit64(process, page, 1024, 4096)?;
    poison_user(process, page + 64, 24)?;
    answers(
        sys_prlimit64(process, own, NOFILE, page, page + 64),
        0,
        "prlimit64 on the caller's own pid was refused",
    )?;
    let old: [u8; 24] = read_user(process, page + 64)?;
    if le_at(&old, 0, 8) != 64 || le_at(&old, 8, 8) != 128 || !all_are(&old, 16, 24, UNWRITTEN) {
        return Err("prlimit64 did not report the limit setrlimit set, in a 16-byte rlimit64");
    }
    if table_limit() != 1024 {
        return Err("prlimit64(RLIMIT_NOFILE) did not reach the descriptor table");
    }
    check_limits_are_refused_as_linux_refuses_them(process, page)
}

/// The refusals, and the two defaults a shell's `ulimit` prints.
fn check_limits_are_refused_as_linux_refuses_them(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    use crate::syscall::limits::{sys_getrlimit, sys_prlimit64};
    const WORD: usize = size_of::<usize>();
    const STACK: u32 = 3;
    const CORE: u32 = 4;
    const NOFILE: u32 = 7;

    stage_rlimit64(process, page, 10, 5)?;
    refuses(
        sys_prlimit64(process, 0, NOFILE, page, 0),
        Errno::EINVAL,
        "a soft limit above its hard limit was not EINVAL",
    )?;
    stage_rlimit64(
        process,
        page,
        1024,
        u64::from(ferrix_vfs::fd::MAX_LIMIT) + 1,
    )?;
    refuses(
        sys_prlimit64(process, 0, NOFILE, page, 0),
        Errno::EPERM,
        "RLIMIT_NOFILE above nr_open was not EPERM",
    )?;
    refuses(
        sys_getrlimit(process, 16, page),
        Errno::EINVAL,
        "RLIM_NLIMITS was accepted as a resource",
    )?;
    let nobody = i32::try_from(crate::syscall::registry::PID_MAX).unwrap_or(i32::MAX);
    refuses(
        sys_prlimit64(process, nobody, NOFILE, 0, 0),
        Errno::ESRCH,
        "prlimit64 on a pid nothing has was not ESRCH",
    )?;

    // The stack's default, at the native width: 8 MiB and RLIM_INFINITY.
    answers(
        sys_getrlimit(process, STACK, page),
        0,
        "getrlimit(RLIMIT_STACK) was refused",
    )?;
    let stack: [u8; 16] = read_user(process, page)?;
    if le_at(&stack, 0, WORD) != 8 << 20 || le_at(&stack, WORD, WORD) != usize::MAX as u64 {
        return Err("RLIMIT_STACK is not 8 MiB soft and unlimited hard");
    }
    // Any other resource keeps what it was given.
    stage_rlimit64(process, page, 0, u64::MAX)?;
    answers(
        sys_prlimit64(process, 0, CORE, page, 0),
        0,
        "prlimit64(RLIMIT_CORE) was refused",
    )?;
    answers(
        sys_getrlimit(process, CORE, page),
        0,
        "getrlimit(RLIMIT_CORE) was refused",
    )?;
    if le_at(&read_user::<16>(process, page)?, 0, WORD) != 0 {
        return Err("RLIMIT_CORE did not read back the limit it was set to");
    }
    Ok(())
}

/// `sched_getaffinity` returns the bytes it wrote, writes no more, and sets
/// one bit per running processor; a mask naming none is refused.
fn check_affinity_names_the_running_processors(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    use crate::syscall::limits::{sys_sched_getaffinity, sys_sched_setaffinity};
    const WORD: usize = size_of::<usize>();

    poison_user(process, page, 64)?;
    let written = sys_sched_getaffinity(process, 0, 64, page, WORD)
        .map_err(|_| "sched_getaffinity with a 64-byte mask was refused")?;
    if written != crate::smp::count().div_ceil(WORD * 8) * WORD {
        return Err("sched_getaffinity did not return whole words covering every processor");
    }
    let out: [u8; 64] = read_user(process, page)?;
    if !all_are(&out, written, out.len(), UNWRITTEN) {
        return Err("sched_getaffinity wrote past the length it returned");
    }
    let bits: u32 = out
        .get(..written)
        .unwrap_or_default()
        .iter()
        .copied()
        .map(u8::count_ones)
        .sum();
    let running = crate::smp::topology().map_or(1, crate::smp::Topology::online);
    if usize::try_from(bits).ok() != Some(running) || out.first().is_none_or(|byte| byte & 1 == 0) {
        return Err("the affinity mask does not have one bit per running processor");
    }
    refuses(
        sys_sched_getaffinity(process, 0, WORD as u32 - 1, page, WORD),
        Errno::EINVAL,
        "an affinity length that is not whole words was accepted",
    )?;
    uaccess::copy_to_user(process.space(), page, &[0_u8; 8])
        .map_err(|_| "could not stage a mask")?;
    refuses(
        sys_sched_setaffinity(process, 0, 8, page),
        Errno::EINVAL,
        "an affinity mask naming no processor was accepted",
    )
}

/// `PR_SET_NAME` keeps fifteen bytes and `PR_GET_NAME` gives them back in
/// sixteen, NUL-terminated, writing nothing past them.
fn check_a_task_name_round_trips(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::attributes::sys_prctl;
    const PR_SET_NAME: i32 = 15;
    const PR_GET_NAME: i32 = 16;

    uaccess::copy_to_user(process.space(), page, b"a-name-longer-than-fifteen\0")
        .map_err(|_| "could not stage a task name")?;
    answers(
        sys_prctl(process, PR_SET_NAME, [page, 0, 0, 0]),
        0,
        "PR_SET_NAME was refused",
    )?;
    poison_user(process, page + 64, 24)?;
    answers(
        sys_prctl(process, PR_GET_NAME, [page + 64, 0, 0, 0]),
        0,
        "PR_GET_NAME was refused",
    )?;
    let out: [u8; 24] = read_user(process, page + 64)?;
    if out.get(..16) != Some(b"a-name-longer-t\0".as_slice()) || !all_are(&out, 16, 24, UNWRITTEN) {
        return Err("PR_GET_NAME did not give back fifteen bytes of the name in sixteen");
    }
    answers(
        sys_prctl(process, PR_GET_DUMPABLE, [0; 4]),
        1,
        "a new process is not dumpable",
    )?;
    refuses(
        sys_prctl(process, 0x7FFF, [0; 4]),
        Errno::EINVAL,
        "an unknown prctl option was not EINVAL",
    )
}

/// One credential call on `on`, with three arguments.
fn credential(
    on: &Process,
    call: ferrix_linux_abi::nr::Syscall,
    [first, second, third]: [u64; 3],
) -> Option<Result<usize, Errno>> {
    crate::syscall::credentials::dispatch(call, &[first, second, third, 0, 0, 0], on)
}

/// Credentials follow Linux's rules, with an effective uid of 0 standing in
/// for `CAP_SETUID` and `CAP_SETGID`: see `credentials`.
///
/// The shared check process stays root, so no later check runs as anyone
/// else: it is only read, and refuses `setuid(-1)`. The rules are exercised on
/// a process of their own, in [`check_a_process_that_drops_root`].
fn check_credentials_follow_linux_rules(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::credentials::sys_getgroups;
    use ferrix_linux_abi::nr::Syscall as Call;
    let unchanged = u64::from(u32::MAX);

    if credential(process, Call::Setuid, [unchanged, 0, 0]) != Some(Err(Errno::EINVAL)) {
        return Err("setuid(-1) was not EINVAL");
    }
    poison_user(process, page, 16)?;
    if credential(process, Call::Getresuid, [page, page + 4, page + 8]) != Some(Ok(0)) {
        return Err("getresuid was refused");
    }
    let out: [u8; 16] = read_user(process, page)?;
    if !all_are(&out, 0, 12, 0) || !all_are(&out, 12, 16, UNWRITTEN) {
        return Err("getresuid did not write three 32-bit zeros and nothing else");
    }
    // `id` asks for the count with a size of zero, then for the list.
    answers(
        sys_getgroups(process, 0, 0),
        1,
        "getgroups(0, NULL) did not count root's group 0",
    )?;
    poison_user(process, page, 8)?;
    answers(
        sys_getgroups(process, 4, page),
        1,
        "getgroups did not fill its list",
    )?;
    let groups: [u8; 8] = read_user(process, page)?;
    if !all_are(&groups, 0, 4, 0) || !all_are(&groups, 4, 8, UNWRITTEN) {
        return Err("getgroups did not write group 0 as one 32-bit gid_t");
    }

    let user = process::new_for_check()
        .map_err(|_| "could not make a process for the credential check")?;
    let scratch = map_rw(&user, PAGE_SIZE)?;
    let outcome = check_a_process_that_drops_root(&user, scratch);
    let _ = memory::sys_munmap(&user, scratch, PAGE_SIZE);
    outcome
}

/// Require one credential call on `on` to have answered `want`.
fn expect_credential(
    on: &Process,
    call: ferrix_linux_abi::nr::Syscall,
    args: [u64; 3],
    want: Result<usize, Errno>,
    what: &'static str,
) -> Result<(), &'static str> {
    if credential(on, call, args) == Some(want) {
        Ok(())
    } else {
        Err(what)
    }
}

/// Put one gid at `page`, as a one-group `setgroups` list.
fn stage_group(on: &Process, page: u64, gid: u32) -> Result<(), &'static str> {
    uaccess::put_u32(on.space(), page, gid).map_err(|_| "could not stage a group list")
}

/// A root process moves its effective uid away and back, joins group 1000 and
/// drops to uid and gid 1000, the order `su` uses, after which every id reads
/// 1000. Then [`check_uid_1000_cannot_take_root_back`] and
/// [`check_what_uid_1000_is_told`].
fn check_a_process_that_drops_root(user: &Arc<Process>, page: u64) -> Result<(), &'static str> {
    use crate::syscall::credentials::identity;
    use ferrix_linux_abi::nr::Syscall as Call;
    let unchanged = u64::from(u32::MAX);

    // An effective uid moved away while the real and saved ids stay 0 comes
    // back without privilege, because 0 is still one of its own.
    expect_credential(
        user,
        Call::Setresuid,
        [unchanged, 1000, unchanged],
        Ok(0),
        "root could not move its effective uid to 1000",
    )?;
    if (identity(Call::Getuid, user), identity(Call::Geteuid, user)) != (Some(0), Some(1000)) {
        return Err("seteuid(1000) did not leave the real uid 0 and the effective uid 1000");
    }
    stage_group(user, page, 1000)?;
    expect_credential(
        user,
        Call::Setgroups,
        [1, page, 0],
        Err(Errno::EPERM),
        "setgroups was accepted from effective uid 1000",
    )?;
    expect_credential(
        user,
        Call::Setuid,
        [0, 0, 0],
        Ok(0),
        "a process whose real uid is 0 could not take back effective uid 0",
    )?;

    expect_credential(
        user,
        Call::Setgroups,
        [1, page, 0],
        Ok(0),
        "root could not set its supplementary groups to 1000",
    )?;
    expect_credential(
        user,
        Call::Setgid,
        [1000, 0, 0],
        Ok(0),
        "root could not setgid(1000)",
    )?;
    expect_credential(
        user,
        Call::Setuid,
        [1000, 0, 0],
        Ok(0),
        "root could not setuid(1000)",
    )?;
    let reported =
        [Call::Getuid, Call::Geteuid, Call::Getgid, Call::Getegid].map(|call| identity(call, user));
    if reported != [Some(1000); 4] {
        return Err(
            "getuid, geteuid, getgid and getegid did not all read 1000 after dropping root",
        );
    }
    check_uid_1000_cannot_take_root_back(user, page)?;
    check_what_uid_1000_is_told(user, page)
}

/// Once uid 1000, `setuid(0)` is `EPERM` -- the negative control, that the
/// drop took -- and so is every other way back to an id or a group it gave up.
/// Its own uid is still accepted, and `setfsuid(0)` answers 1000 and changes
/// nothing, however often it is asked.
fn check_uid_1000_cannot_take_root_back(user: &Process, page: u64) -> Result<(), &'static str> {
    use ferrix_linux_abi::nr::Syscall as Call;
    let unchanged = u64::from(u32::MAX);
    let refusals = [
        (
            Call::Setuid,
            [0, 0, 0],
            "setuid(0) was not refused after root dropped to uid 1000",
        ),
        (
            Call::Setresuid,
            [unchanged, 0, unchanged],
            "an unprivileged setresuid took back effective uid 0",
        ),
        (
            Call::Setreuid,
            [0, unchanged, 0],
            "an unprivileged setreuid took back real uid 0",
        ),
        (
            Call::Setgid,
            [0, 0, 0],
            "an unprivileged setgid took back gid 0",
        ),
    ];
    for (call, args, what) in refusals {
        expect_credential(user, call, args, Err(Errno::EPERM), what)?;
    }
    stage_group(user, page, 0)?;
    expect_credential(
        user,
        Call::Setgroups,
        [1, page, 0],
        Err(Errno::EPERM),
        "an unprivileged setgroups was accepted",
    )?;
    expect_credential(
        user,
        Call::Setuid,
        [1000, 0, 0],
        Ok(0),
        "an unprivileged setuid to its own uid was refused",
    )?;
    for what in [
        "setfsuid(0) did not answer the filesystem uid 1000",
        "an unprivileged setfsuid(0) changed the filesystem uid",
    ] {
        expect_credential(user, Call::Setfsuid, [0, 0, 0], Ok(1000), what)?;
    }
    Ok(())
}

/// What uid 1000 is told: `getresuid` writes 1000 three times, `getgroups`
/// the one group it set, `capget` no capabilities at all, and a child forked
/// from it starts with every id and group it has.
fn check_what_uid_1000_is_told(user: &Arc<Process>, page: u64) -> Result<(), &'static str> {
    use crate::syscall::credentials::sys_getgroups;
    use ferrix_linux_abi::nr::Syscall as Call;

    poison_user(user, page, 16)?;
    if credential(user, Call::Getresuid, [page, page + 4, page + 8]) != Some(Ok(0)) {
        return Err("getresuid was refused to uid 1000");
    }
    let ids: [u8; 16] = read_user(user, page)?;
    if ids.get(..12) != Some([0xE8, 0x03, 0, 0].repeat(3).as_slice())
        || !all_are(&ids, 12, 16, UNWRITTEN)
    {
        return Err("getresuid did not write 1000 three times as 32-bit ids");
    }
    poison_user(user, page, 8)?;
    answers(
        sys_getgroups(user, 4, page),
        1,
        "getgroups did not report the one group set",
    )?;
    let groups: [u8; 8] = read_user(user, page)?;
    if groups.get(..4) != Some([0xE8, 0x03, 0, 0].as_slice()) || !all_are(&groups, 4, 8, UNWRITTEN)
    {
        return Err("getgroups did not write group 1000");
    }

    // A version 3 header for the caller, then two data structures to fill.
    uaccess::put_u32(user.space(), page, 0x2008_0522).map_err(|_| "could not stage capget")?;
    uaccess::put_u32(user.space(), page + 4, 0).map_err(|_| "could not stage capget")?;
    poison_user(user, page + 8, 24)?;
    if credential(user, Call::Capget, [page, page + 8, 0]) != Some(Ok(0)) {
        return Err("capget was refused to uid 1000");
    }
    let sets: [u8; 24] = read_user(user, page + 8)?;
    if !all_are(&sets, 0, 24, 0) {
        return Err("capget reported capabilities for uid 1000");
    }

    // No CAP_NET_RAW, so no raw socket: the negative control for the raw
    // sockets `check_the_internet_families_open` opens as root. Protocol zero
    // stays EPROTONOSUPPORT, because inet_create looks the protocol up before
    // it asks about the capability.
    let raw = u64::from(SOCK_RAW);
    refuses(
        socket_call(user, Call::Socket, &[2, raw, 1, 0, 0, 0]),
        Errno::EPERM,
        "uid 1000 opened a raw socket",
    )?;
    refuses(
        socket_call(user, Call::Socket, &[2, raw, 0, 0, 0, 0]),
        Errno::EPROTONOSUPPORT,
        "uid 1000's raw socket at protocol zero was not EPROTONOSUPPORT",
    )?;
    // A packet socket too, and there the capability is asked before the type:
    // `packet_create` refuses a stream packet socket to uid 1000 with EPERM.
    refuses(
        socket_call(
            user,
            Call::Socket,
            &[17, u64::from(SOCK_DGRAM), 0x0008, 0, 0, 0],
        ),
        Errno::EPERM,
        "uid 1000 opened a packet socket",
    )?;
    refuses(
        socket_call(
            user,
            Call::Socket,
            &[17, u64::from(SOCK_STREAM), 0, 0, 0, 0],
        ),
        Errno::EPERM,
        "uid 1000's stream packet socket was refused for its type before its capability",
    )?;

    let space = crate::user::space::AddressSpace::new()
        .map_err(|_| "no address space for the credential check's child")?;
    let child = Process::forked(user, space, false, false).map_err(|_| "no memory for a fork")?;
    if child.with_credentials(|credentials| credentials.clone())
        != user.with_credentials(|credentials| credentials.clone())
    {
        return Err("a forked child did not start with its parent's ids and groups");
    }
    Ok(())
}

/// `nanosleep` does not come back until its time has passed, and refuses a
/// nanosecond field of a whole second.
fn check_nanosleep_takes_its_time(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::time::sys_nanosleep;
    const NAP: u64 = 20_000_000;
    let word = size_of::<usize>() as u64;
    let thread = check_thread(process)?;

    write_word(process, page, 0)?;
    write_word(process, page + word, NAP)?;
    let start = crate::timer::now_nanos();
    answers(sys_nanosleep(&thread, page, 0), 0, "nanosleep was refused")?;
    if crate::timer::now_nanos().saturating_sub(start) < NAP {
        return Err("nanosleep returned before its time had passed");
    }
    write_word(process, page + word, 1_000_000_000)?;
    refuses(
        sys_nanosleep(&thread, page, 0),
        Errno::EINVAL,
        "a nanosecond field of a whole second was accepted",
    )
}

/// `clock_settime(CLOCK_REALTIME)` moves what `CLOCK_REALTIME` reads and
/// leaves `CLOCK_MONOTONIC` alone, which cannot be set at all. The clock is put
/// back afterwards whatever happened, so nothing that runs later reads 2001.
fn check_setting_the_clock_moves_only_realtime(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    use crate::syscall::time;
    let saved = time::realtime_offset();
    let outcome = set_the_clock_and_read_it_back(process, page);
    time::restore_realtime_offset(saved);
    outcome
}

/// The body of [`check_setting_the_clock_moves_only_realtime`], which puts
/// the clock back whatever this returns.
fn set_the_clock_and_read_it_back(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::time::{self, TimeWidth};
    use ferrix_linux_abi::types::{CLOCK_MONOTONIC, CLOCK_REALTIME};
    const SEPTEMBER_2001: u64 = 1_000_000_000;
    let word = size_of::<usize>() as u64;

    write_word(process, page, SEPTEMBER_2001)?;
    write_word(process, page + word, 0)?;
    answers(
        time::sys_clock_settime(process, CLOCK_REALTIME as i32, page, TimeWidth::Native),
        0,
        "clock_settime(CLOCK_REALTIME) was refused",
    )?;
    let read = |clock: u32| -> Result<u64, &'static str> {
        answers(
            time::sys_clock_gettime(process, u64::from(clock), page + 32, TimeWidth::Native),
            0,
            "clock_gettime was refused",
        )?;
        Ok(le_at(
            &read_user::<8>(process, page + 32)?,
            0,
            size_of::<usize>(),
        ))
    };
    if !(SEPTEMBER_2001..SEPTEMBER_2001 + 60).contains(&read(CLOCK_REALTIME)?) {
        return Err("CLOCK_REALTIME did not read the time it was set to");
    }
    if read(CLOCK_MONOTONIC)? >= SEPTEMBER_2001 {
        return Err("setting CLOCK_REALTIME moved CLOCK_MONOTONIC");
    }
    refuses(
        time::sys_clock_settime(process, CLOCK_MONOTONIC as i32, page, TimeWidth::Native),
        Errno::EINVAL,
        "CLOCK_MONOTONIC could be set",
    )
}

/// `time` answers `gettimeofday`'s seconds, writes the same seconds through a
/// pointer, and is `EFAULT` through an unmapped one. The clock is set to 2001
/// first, so an answer of zero cannot pass for a clock read near the epoch,
/// and put back afterwards whatever happened. Skipped where this build's table
/// has no number for `time`, which is every table but x86-64's.
fn check_time_is_the_realtime_seconds(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::time;
    use ferrix_linux_abi::nr::{Syscall, x86_64};
    const SEPTEMBER_2001_NANOS: i64 = 1_000_000_000_000_000_000;
    if arch::decode_syscall(x86_64::TIME) != Some(Syscall::Time) {
        return Ok(());
    }
    let saved = time::realtime_offset();
    time::restore_realtime_offset(saved.saturating_add(SEPTEMBER_2001_NANOS));
    let outcome = read_the_clock_through_time(process, page);
    time::restore_realtime_offset(saved);
    outcome
}

/// The body of [`check_time_is_the_realtime_seconds`], which puts the clock
/// back whatever this returns.
fn read_the_clock_through_time(process: &Process, page: u64) -> Result<(), &'static str> {
    use crate::syscall::time;
    let seconds_at =
        |at: u64| -> Result<u64, &'static str> { Ok(le_at(&read_user::<8>(process, at)?, 0, 8)) };

    answers(
        time::sys_gettimeofday(process, page, 0),
        0,
        "gettimeofday was refused",
    )?;
    let before = seconds_at(page)?;
    let returned = time::sys_time(process, 0).map_err(|_| "time(NULL) was refused")? as u64;
    if !(before..=before + 1).contains(&returned) {
        return Err("time(NULL) is not gettimeofday's seconds");
    }
    poison_user(process, page + 16, 8)?;
    let written = time::sys_time(process, page + 16).map_err(|_| "time(page) was refused")? as u64;
    if seconds_at(page + 16)? != written || !(returned..=returned + 1).contains(&written) {
        return Err("time did not write the seconds it returned");
    }
    refuses(
        time::sys_time(process, TEST_BASE + 0x10_0000),
        Errno::EFAULT,
        "time through an unmapped address was not EFAULT",
    )
}

/// A name `sethostname` sets is the `nodename` `uname` reports, and a name
/// longer than 64 bytes is refused. The name is forgotten afterwards, so the
/// `uname` check and a person reading `uname -a` both still see `ferrix`.
fn check_a_host_name_reaches_uname(process: &Process, page: u64) -> Result<(), &'static str> {
    uaccess::copy_to_user(process.space(), page, b"check-host")
        .map_err(|_| "could not stage a host name")?;
    let outcome = answers(
        system::sys_sethostname(process, page, 10),
        0,
        "sethostname was refused",
    )
    .and_then(|()| {
        answers(
            system::sys_uname(process, page + 512),
            0,
            "uname was refused",
        )
    })
    .and_then(|()| {
        let out: [u8; 130] = read_user(process, page + 512)?;
        if out.get(65..76) != Some(b"check-host\0".as_slice()) || !all_are(&out, 75, 130, 0) {
            return Err("uname's nodename is not the name sethostname set");
        }
        Ok(())
    })
    .and_then(|()| {
        refuses(
            system::sys_sethostname(process, page, 65),
            Errno::EINVAL,
            "a 65-byte host name was accepted",
        )
    });
    system::forget_hostname();
    outcome
}

// ---------------------------------------------------------------------------
// Unix-domain sockets
//
// Through `dispatch`, the path a program arrives by: a pair of each type,
// what it carries, what it reports about itself, and what the calls still
// refuse. Every socket here is non-blocking, because a boot check must never
// be the thing that waits, and every send passes `MSG_NOSIGNAL`, because the
// boot task is not a process that could take a `SIGPIPE`.
// ---------------------------------------------------------------------------

/// Where a socket check stages what it sends, as an offset in its page.
const SENT: u64 = 0x100;

/// Where it reads what arrived.
const RECEIVED: u64 = 0x200;

/// Where it builds a `msghdr`, with its iovecs and control buffer after it.
const MESSAGE: u64 = 0x300;

/// Where a socket check puts a control buffer, as an offset in its page.
const CONTROL: u64 = 0x600;

/// `AF_UNIX` sockets: what a pair carries, what a socket reports, what the
/// calls refuse, and what a name carries.
fn check_unix_sockets(process: &Process, page: u64) -> Result<(), &'static str> {
    check_what_the_socket_calls_refuse(process, page)
        .and_then(|()| check_a_stream_pair_carries_bytes(process, page))
        .and_then(|()| check_records_keep_their_boundaries(process, page))
        .and_then(|()| check_shutdown_ends_one_direction(process, page))
        .and_then(|()| check_a_socket_reports_itself(process, page))
        .and_then(|()| check_a_message_scatters_and_gathers(process, page))
        .and_then(|()| check_a_stream_read_runs_into_descriptors(process, page))
        .and_then(|()| check_a_name_carries_a_connection(process, page))
        .and_then(|()| check_an_accepted_socket_blocks_unless_asked(process, page))
        .and_then(|()| check_a_path_carries_a_connection(process, page))
        .and_then(|()| check_what_a_name_refuses(process, page))
        .and_then(|()| check_peer_credentials_are_the_callers(process, page))?;
    println!(
        "  unix     a stream pair carried bytes across two writes and a peek left them; \
         records kept their boundaries and MSG_TRUNC their lengths; shutdown ended one \
         direction; a socket reported its type, buffers, credentials and unnamed address; \
         a connection crossed an abstract name and a path, and 6 name calls were refused \
         as specified; a non-blocking listener's accepted socket blocked, and accept4's \
         SOCK_NONBLOCK made one that did not; a descriptor travelled with a message, kept its file open in the \
         queue, and was closed when a receive had no room for it; a cycle of sockets in \
         flight was collected at its last close, and one a descriptor reached was kept"
    );
    Ok(())
}

/// `SO_PEERCRED` names who connected and who listened, each as they were
/// at that call, not who made the sockets (`docs/AUTH.md` §8.3, E-01, and
/// K-E): what `authd` decides who is asking by.
///
/// One process plays every part by changing its effective ids between the
/// calls, as a program that drops privilege does: the listener is made as
/// root and listens as uid 4242; the client is made as root and connects as
/// uid 1000; then the process becomes uid 2000, standing for another process
/// the connected descriptor was handed to. The accepted end must still name
/// uid 1000, and the connecting end uid 4242. Its negative control is the
/// creation-time ids put back in `connect_stream`, which name root on both
/// ends and fail the first line. The process is root again when it returns,
/// whatever it returns.
fn check_peer_credentials_are_the_callers(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let root = process.with_credentials(|ids| ids.clone());
    let outcome = peer_credentials_are_the_callers(process, page);
    process.with_credentials(|ids| *ids = root);
    outcome?;
    println!(
        "  unix     SO_PEERCRED named the uid that connected and the uid that listened, as each \
         was at its call, not root that made both sockets, and kept naming them when the \
         descriptor's holder changed uid"
    );
    Ok(())
}

/// The name [`check_peer_credentials_are_the_callers`] binds.
const PEERCRED_NAME: &[u8] = b"\0ferrix-peercred-check";

/// Become `uid` and `gid` as the effective ids, as `setresuid` would.
fn act_as(process: &Process, uid: u32, gid: u32) {
    process.with_credentials(|ids| {
        ids.user.effective = uid;
        ids.user.filesystem = uid;
        ids.group.effective = gid;
        ids.group.filesystem = gid;
    });
}

/// Send `fd` over a new pair with `SCM_RIGHTS`, close it, and answer the
/// descriptor the receive installed for the same file.
fn pass_on(process: &Process, page: u64, fd: i32) -> Result<i32, &'static str> {
    let pair = socket_pair(process, page, SOCK_SEQPACKET)?;
    let moved = (|| {
        answers(
            message_with_control(process, page, pair.0, SCM_RIGHTS, &rights(&[fd]), None),
            1,
            "the credentials check could not send its connection with SCM_RIGHTS",
        )?;
        let _ = fd::sys_close(process, fd);
        let capacity = cmsg_space(4, width());
        let (_count, _flags, _len, control) =
            receive_with_control(process, page, pair.1, capacity)?;
        let data = control
            .get(CmsgHdr::size(width())..CmsgHdr::size(width()) + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .ok_or("the credentials check's connection did not arrive with SCM_RIGHTS")?;
        Ok(i32::from_le_bytes(data))
    })();
    close_socket_pair(process, pair);
    moved
}

/// `SO_PEERCRED` on `fd`.
fn peer_of(process: &Process, page: u64, fd: i32) -> Result<Ucred, &'static str> {
    let bytes = socket_option(process, page, fd, SO_PEERCRED, Ucred::SIZE)
        .map_err(|_| "SO_PEERCRED was refused on a connection")?;
    Ucred::from_bytes(&bytes).ok_or("SO_PEERCRED was too short")
}

/// [`check_peer_credentials_are_the_callers`]'s steps, as root on entry.
fn peer_credentials_are_the_callers(process: &Process, page: u64) -> Result<(), &'static str> {
    let server = unix_socket(process, SOCK_STREAM)?;
    let client = unix_socket(process, SOCK_STREAM)?;
    let (at, len) = put_unix_address(process, page + ADDRESS, PEERCRED_NAME)?;
    let steps = (|| {
        if socket_call(process, Call::Bind, &[as_arg(server), at, len, 0, 0, 0]) != Ok(0) {
            return Err("the credentials check's listener would not take its name");
        }
        act_as(process, 4242, 4343);
        if socket_call(process, Call::Listen, &[as_arg(server), 1, 0, 0, 0, 0]) != Ok(0) {
            return Err("the credentials check's listener would not listen");
        }
        act_as(process, 1000, 1001);
        if socket_call(process, Call::Connect, &[as_arg(client), at, len, 0, 0, 0]) != Ok(0) {
            return Err("the credentials check's client would not connect");
        }
        act_as(process, 0, 0);
        let taken = socket_call(process, Call::Accept, &[as_arg(server), 0, 0, 0, 0, 0])
            .map_err(|_| "the credentials check's connection was not there to accept")?;
        let accepted = i32::try_from(taken).map_err(|_| "accept gave no descriptor")?;
        // The descriptor is handed on, by `SCM_RIGHTS`, to a holder that is
        // someone else again, and the original closed: what is asked below
        // is asked through the descriptor that arrived.
        act_as(process, 2000, 2001);
        let accepted = match pass_on(process, page, accepted) {
            Ok(arrived) => arrived,
            Err(why) => {
                let _ = fd::sys_close(process, accepted);
                return Err(why);
            }
        };
        let judged = (|| {
            let pid = i32::try_from(process.pid()).unwrap_or(-1);
            let connector = peer_of(process, page, accepted)?;
            if (connector.uid, connector.gid) != (1000, 1001) || connector.pid != pid {
                return Err(
                    "SO_PEERCRED on the accepted end did not name who connected, as they were when they connected",
                );
            }
            let listener = peer_of(process, page, client)?;
            if (listener.uid, listener.gid) != (4242, 4343) || listener.pid != pid {
                return Err(
                    "SO_PEERCRED on the connecting end did not name who listened, as they were when they listened",
                );
            }
            Ok(())
        })();
        let _ = fd::sys_close(process, accepted);
        judged
    })();
    let _ = fd::sys_close(process, client);
    let _ = fd::sys_close(process, server);
    steps
}

/// Where a name check builds a `sockaddr_un`, as an offset in its page.
const ADDRESS: u64 = 0x400;

/// Where it reads one back.
const ADDRESS_BACK: u64 = 0x480;

/// Where it puts the length a call reads a name's back through.
const ADDRESS_LEN: u64 = 0x4F8;

/// The abstract name the check binds. Leading NUL and all, as a program
/// writes one.
const ABSTRACT: &[u8] = b"\0ferrix-boot-check";

/// The path it binds, on the tmpfs the boot check already has.
const SOCKET_PATH: &[u8] = b"/tmp/ferrix-check.sock";

/// Write a `sockaddr_un` for `name` into the page, answering the pointer and
/// the length a call must be given.
///
/// `sun_path` carries exactly the bytes, with no terminator, and the length
/// is the family plus them -- which is how a program names an abstract socket
/// and the only way to name one whose bytes hold a NUL.
fn put_unix_address(process: &Process, at: u64, name: &[u8]) -> Result<(u64, u64), &'static str> {
    let mut bytes = [0_u8; 2 + 108];
    let family = AF_UNIX.to_ne_bytes();
    for (slot, byte) in bytes.iter_mut().zip(family.iter().chain(name.iter())) {
        *slot = *byte;
    }
    let len = 2 + name.len();
    uaccess::copy_to_user(process.space(), at, bytes.get(..len).unwrap_or_default())
        .map_err(|_| "could not write a socket address")?;
    Ok((at, len as u64))
}

/// A non-blocking `AF_UNIX` socket of `kind`.
fn unix_socket(process: &Process, kind: u32) -> Result<i32, &'static str> {
    let made = socket_call(
        process,
        Call::Socket,
        &[
            u64::from(AF_UNIX),
            u64::from(kind | SOCK_NONBLOCK),
            0,
            0,
            0,
            0,
        ],
    )
    .map_err(|_| "a Unix socket could not be made")?;
    i32::try_from(made).map_err(|_| "a socket got no descriptor")
}

/// A name, a listener, a connection across it, and a byte through it.
///
/// Non-blocking throughout, because a boot check must never be the thing that
/// waits: the connection completes when it is queued, so the `accept` after
/// it finds one waiting without anything having to sleep.
fn check_a_name_carries_a_connection(process: &Process, page: u64) -> Result<(), &'static str> {
    let server = unix_socket(process, SOCK_STREAM)?;
    let client = unix_socket(process, SOCK_STREAM)?;
    let outcome = a_name_carries_a_connection(process, page, server, client);
    let _ = fd::sys_close(process, client);
    let _ = fd::sys_close(process, server);
    outcome
}

/// `poll`'s answer for `socket`, asked about input with no waiting: its
/// `revents`, which for a listener is `POLLIN` while a connection waits and
/// nothing otherwise -- no hang-up.
fn listener_poll(process: &Process, page: u64, socket: i32) -> Result<u16, &'static str> {
    use crate::syscall::poll::{self, POLLIN};

    let at = page + POLLFD;
    let mut entry = [0_u8; 8];
    for (slot, byte) in entry
        .iter_mut()
        .zip(socket.to_le_bytes().into_iter().chain(POLLIN.to_le_bytes()))
    {
        *slot = byte;
    }
    uaccess::copy_to_user(process.space(), at, &entry).map_err(|_| "could not stage a pollfd")?;
    // The count says the same as `revents` does, which is what is judged.
    let _ready =
        poll::sys_poll(process, at, 1, 0).map_err(|_| "poll refused a listening socket")?;
    uaccess::copy_from_user(process.space(), at, &mut entry)
        .map_err(|_| "could not read revents back")?;
    Ok(u16::from_le_bytes([entry[6], entry[7]]))
}

/// Where a check stages a `pollfd`, as an offset in its page: past the
/// name's slots.
const POLLFD: u64 = 0x500;

/// [`check_a_name_carries_a_connection`]'s questions.
fn a_name_carries_a_connection(
    process: &Process,
    page: u64,
    server: i32,
    client: i32,
) -> Result<(), &'static str> {
    let (at, len) = put_unix_address(process, page + ADDRESS, ABSTRACT)?;
    if socket_call(process, Call::Bind, &[as_arg(server), at, len, 0, 0, 0]) != Ok(0) {
        return Err("a Unix socket would not take an abstract name");
    }
    if socket_call(process, Call::Listen, &[as_arg(server), 4, 0, 0, 0, 0]) != Ok(0) {
        return Err("a bound Unix socket would not listen");
    }
    // A listener with nothing waiting is neither readable nor hung up, which
    // is what lets a program sleep in `poll` on it.
    if listener_poll(process, page, server)? != 0 {
        return Err("poll reported a listening socket with nothing waiting as ready");
    }
    // What it is bound to, read back: the same bytes, the leading NUL and all.
    let mut back = [0_u8; 2 + 108];
    uaccess::copy_to_user(
        process.space(),
        page + ADDRESS_LEN,
        &(len as u32).to_ne_bytes(),
    )
    .map_err(|_| "could not write an address length")?;
    if socket_call(
        process,
        Call::Getsockname,
        &[
            as_arg(server),
            page + ADDRESS_BACK,
            page + ADDRESS_LEN,
            0,
            0,
            0,
        ],
    ) != Ok(0)
    {
        return Err("getsockname on a bound Unix socket failed");
    }
    uaccess::copy_from_user(
        process.space(),
        page + ADDRESS_BACK,
        back.get_mut(..len as usize).ok_or("impossible")?,
    )
    .map_err(|_| "could not read a name back")?;
    if back.get(2..len as usize) != Some(ABSTRACT) {
        return Err("getsockname did not answer the abstract name that was bound");
    }
    if socket_call(process, Call::Connect, &[as_arg(client), at, len, 0, 0, 0]) != Ok(0) {
        return Err("a Unix socket would not connect to an abstract name");
    }
    // And with a connection waiting it is readable, and still not hung up.
    if listener_poll(process, page, server)? != crate::syscall::poll::POLLIN {
        return Err("poll did not report a waiting connection as readable, and only that");
    }
    let taken = socket_call(process, Call::Accept, &[as_arg(server), 0, 0, 0, 0, 0])
        .map_err(|_| "a queued connection was not there to accept")?;
    let accepted = i32::try_from(taken).map_err(|_| "accept gave no descriptor")?;
    let outcome = (|| {
        answers(
            socket_send(process, page, client, b"named", 0),
            5,
            "a connection over a name would not take five bytes",
        )?;
        answers(
            socket_recv(process, page, accepted, 8, 0),
            5,
            "the far end of a named connection did not read five bytes",
        )
    })();
    let _ = fd::sys_close(process, accepted);
    outcome
}

/// The abstract name [`check_an_accepted_socket_blocks_unless_asked`] binds.
const ACCEPTING: &[u8] = b"\0ferrix-boot-check-accept";

/// A socket `accept` takes is non-blocking only when `accept4` asked for it
/// with `SOCK_NONBLOCK`, never because its listener is, as on Linux: the
/// listener's `O_NONBLOCK` decides only whether `accept` waits. A server
/// that accepts on a non-blocking listener and then reads with a timeout --
/// yserver, hyprix's control socket -- otherwise has its first read answer
/// `EAGAIN` before the client's bytes arrive, and drops the client.
///
/// The listener here is non-blocking, so nothing waits: each connection is
/// queued before it is taken.
fn check_an_accepted_socket_blocks_unless_asked(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let server = unix_socket(process, SOCK_STREAM)?;
    let first = unix_socket(process, SOCK_STREAM)?;
    let second = unix_socket(process, SOCK_STREAM)?;
    let outcome = accepted_sockets_block_unless_asked(process, page, server, [first, second]);
    for socket in [second, first, server] {
        let _ = fd::sys_close(process, socket);
    }
    outcome
}

/// [`check_an_accepted_socket_blocks_unless_asked`]'s questions.
fn accepted_sockets_block_unless_asked(
    process: &Process,
    page: u64,
    server: i32,
    clients: [i32; 2],
) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::O_NONBLOCK;

    let nonblocking = |fd: i32| -> Result<bool, &'static str> {
        let flags = fd::sys_fcntl(process, fd, F_GETFL, 0)
            .map_err(|_| "F_GETFL refused an accepted socket")?;
        Ok(flags & O_NONBLOCK as usize != 0)
    };
    let (at, len) = put_unix_address(process, page + ADDRESS, ACCEPTING)?;
    if socket_call(process, Call::Bind, &[as_arg(server), at, len, 0, 0, 0]) != Ok(0) {
        return Err("a Unix socket would not take a second abstract name");
    }
    if socket_call(process, Call::Listen, &[as_arg(server), 4, 0, 0, 0, 0]) != Ok(0) {
        return Err("a bound Unix socket would not listen");
    }
    if !nonblocking(server)? {
        return Err("a listener made with SOCK_NONBLOCK did not report O_NONBLOCK");
    }
    for (client, flags, want, what) in [
        (
            clients[0],
            0,
            false,
            "accept4 without SOCK_NONBLOCK on a non-blocking listener gave a non-blocking socket",
        ),
        (
            clients[1],
            u64::from(SOCK_NONBLOCK),
            true,
            "accept4 with SOCK_NONBLOCK gave a socket without O_NONBLOCK",
        ),
    ] {
        if socket_call(process, Call::Connect, &[as_arg(client), at, len, 0, 0, 0]) != Ok(0) {
            return Err("a Unix socket would not connect to a listener's abstract name");
        }
        let taken = socket_call(process, Call::Accept4, &[as_arg(server), 0, 0, flags, 0, 0])
            .map_err(|_| "a queued connection was not there to accept4")?;
        let accepted = i32::try_from(taken).map_err(|_| "accept4 gave no descriptor")?;
        let got = nonblocking(accepted);
        let _ = fd::sys_close(process, accepted);
        if got? != want {
            return Err(what);
        }
    }
    // With nothing waiting, the non-blocking listener still does not wait.
    refuses(
        socket_call(process, Call::Accept4, &[as_arg(server), 0, 0, 0, 0, 0]),
        Errno::EAGAIN,
        "accept4 on a non-blocking listener with nothing waiting was not EAGAIN",
    )
}

/// The same over a path, which is a node in the filesystem rather than a name
/// in a table of its own.
fn check_a_path_carries_a_connection(process: &Process, page: u64) -> Result<(), &'static str> {
    let server = unix_socket(process, SOCK_STREAM)?;
    let client = unix_socket(process, SOCK_STREAM)?;
    let outcome = (|| {
        // Unlinked first, as every program that binds a path does: the node a
        // bind makes outlives the socket, so a check that ran once has left
        // one behind and a second bind to it is `EADDRINUSE`.
        let namespace = crate::fs::namespace();
        let context = crate::syscall::path::context(process);
        let _ = namespace.unlink(&context, None, SOCKET_PATH);
        let (at, len) = put_unix_address(process, page + ADDRESS, SOCKET_PATH)?;
        if socket_call(process, Call::Bind, &[as_arg(server), at, len, 0, 0, 0]) != Ok(0) {
            return Err("a Unix socket would not take a path");
        }
        // The bind made a node, and it is a socket: that is what a connect
        // walks to, and what makes the directory's permissions mean something.
        let made = namespace
            .resolve(&context, None, SOCKET_PATH, false)
            .map_err(|_| "a bind to a path left no node behind")?;
        if namespace
            .stat(&made)
            .map_err(|_| "the node a bind made could not be stat'ed")?
            .metadata
            .kind
            != ferrix_vfs::FileType::Socket
        {
            return Err("the node a bind made is not a socket");
        }
        // The bind made a node, which `stat` sees as a socket: that node is
        // what a connect walks to, and what makes the directory's permissions
        // mean something.
        if socket_call(process, Call::Listen, &[as_arg(server), 1, 0, 0, 0, 0]) != Ok(0) {
            return Err("a path-bound Unix socket would not listen");
        }
        // A second bind to the same path is the name already being a file.
        let second = unix_socket(process, SOCK_STREAM)?;
        let taken = socket_call(process, Call::Bind, &[as_arg(second), at, len, 0, 0, 0]);
        let _ = fd::sys_close(process, second);
        refuses(
            taken,
            Errno::EADDRINUSE,
            "a second bind to a path was not EADDRINUSE",
        )?;
        if socket_call(process, Call::Connect, &[as_arg(client), at, len, 0, 0, 0]) != Ok(0) {
            return Err("a Unix socket would not connect to a path");
        }
        let taken = socket_call(process, Call::Accept, &[as_arg(server), 0, 0, 0, 0, 0])
            .map_err(|_| "a connection over a path was not there to accept")?;
        let accepted = i32::try_from(taken).map_err(|_| "accept gave no descriptor")?;
        let outcome = (|| {
            answers(
                socket_send(process, page, accepted, b"path", 0),
                4,
                "a connection over a path would not take four bytes",
            )?;
            answers(
                socket_recv(process, page, client, 8, 0),
                4,
                "the near end of a path connection did not read four bytes",
            )
        })();
        let _ = fd::sys_close(process, accepted);
        outcome
    })();
    let _ = fd::sys_close(process, client);
    let _ = fd::sys_close(process, server);
    outcome
}

/// What the name calls refuse, each as Linux refuses it.
fn check_what_a_name_refuses(process: &Process, page: u64) -> Result<(), &'static str> {
    let socket = unix_socket(process, SOCK_STREAM)?;
    let outcome = (|| {
        // A name nobody has bound, in each namespace. A path that is not
        // there is `ENOENT`; an abstract name that is not there is a
        // connection refused, because there is no path to be missing.
        let (at, len) = put_unix_address(process, page + ADDRESS, b"\0ferrix-nobody")?;
        refuses(
            socket_call(process, Call::Connect, &[as_arg(socket), at, len, 0, 0, 0]),
            Errno::ECONNREFUSED,
            "connecting to an unbound abstract name was not ECONNREFUSED",
        )?;
        let (at, len) = put_unix_address(process, page + ADDRESS, b"/tmp/ferrix-nobody.sock")?;
        refuses(
            socket_call(process, Call::Connect, &[as_arg(socket), at, len, 0, 0, 0]),
            Errno::ENOENT,
            "connecting to a path that is not there was not ENOENT",
        )?;
        // A path that is there and is not a socket.
        let (at, len) = put_unix_address(process, page + ADDRESS, b"/tmp")?;
        refuses(
            socket_call(process, Call::Connect, &[as_arg(socket), at, len, 0, 0, 0]),
            Errno::ECONNREFUSED,
            "connecting to a path that is not a socket was not ECONNREFUSED",
        )?;
        // Listening before binding, and binding twice.
        refuses(
            socket_call(process, Call::Listen, &[as_arg(socket), 1, 0, 0, 0, 0]),
            Errno::EINVAL,
            "listen on an unbound Unix socket was not EINVAL",
        )?;
        let (at, len) = put_unix_address(process, page + ADDRESS, b"\0ferrix-twice")?;
        if socket_call(process, Call::Bind, &[as_arg(socket), at, len, 0, 0, 0]) != Ok(0) {
            return Err("a Unix socket would not take a name to bind twice");
        }
        refuses(
            socket_call(process, Call::Bind, &[as_arg(socket), at, len, 0, 0, 0]),
            Errno::EINVAL,
            "a second bind on one socket was not EINVAL",
        )?;
        // Accepting on a socket that never listened.
        refuses(
            socket_call(process, Call::Accept, &[as_arg(socket), 0, 0, 0, 0, 0]),
            Errno::EINVAL,
            "accept on a Unix socket that is not listening was not EINVAL",
        )
    })();
    let _ = fd::sys_close(process, socket);
    outcome
}

/// One socket call, through the dispatcher every socket call arrives by.
fn socket_call(process: &Process, call: Call, a: &[u64; 6]) -> Result<usize, Errno> {
    sockets::dispatch(call, a, process, crate::trap::Abi::Native).unwrap_or(Err(Errno::ENOSYS))
}

/// A descriptor as the call argument it arrives as.
fn as_arg(fd: i32) -> u64 {
    u64::from(fd.cast_unsigned())
}

/// This build's pointer width, which every structure a socket call reads has.
fn width() -> Width {
    if size_of::<usize>() == 8 {
        Width::Bits64
    } else {
        Width::Bits32
    }
}

/// A connected pair of non-blocking sockets of `kind`.
fn socket_pair(process: &Process, page: u64, kind: u32) -> Result<(i32, i32), &'static str> {
    let family = u64::from(AF_UNIX);
    let kind = u64::from(kind | SOCK_NONBLOCK);
    if socket_call(process, Call::Socketpair, &[family, kind, 0, page, 0, 0]) != Ok(0) {
        return Err("socketpair would not make a pair of Unix sockets");
    }
    let mut numbers = [0_u8; 8];
    uaccess::copy_from_user(process.space(), page, &mut numbers)
        .map_err(|_| "could not read back the pair's descriptors")?;
    let one = i32::from_le_bytes(*numbers.first_chunk::<4>().ok_or("impossible")?);
    let other = i32::from_le_bytes(*numbers.last_chunk::<4>().ok_or("impossible")?);
    if one < 0 || other < 0 || one == other {
        return Err("socketpair reported two descriptors that cannot be a pair");
    }
    Ok((one, other))
}

/// Close a pair, whatever the check that used it decided.
fn close_socket_pair(process: &Process, pair: (i32, i32)) {
    let _ = fd::sys_close(process, pair.0);
    let _ = fd::sys_close(process, pair.1);
}

/// Send `data` through `fd`, staged in the page, never raising `SIGPIPE`.
fn socket_send(
    process: &Process,
    page: u64,
    fd: i32,
    data: &[u8],
    flags: u32,
) -> Result<usize, Errno> {
    let at = page + SENT;
    uaccess::copy_to_user(process.space(), at, data).map_err(|_| Errno::EFAULT)?;
    let flags = u64::from(flags | MSG_NOSIGNAL);
    socket_call(
        process,
        Call::Sendto,
        &[as_arg(fd), at, data.len() as u64, flags, 0, 0],
    )
}

/// Receive up to `len` bytes from `fd` into the page.
fn socket_recv(
    process: &Process,
    page: u64,
    fd: i32,
    len: u64,
    flags: u32,
) -> Result<usize, Errno> {
    socket_call(
        process,
        Call::Recvfrom,
        &[as_arg(fd), page + RECEIVED, len, u64::from(flags), 0, 0],
    )
}

/// What the last receive left in the page: `len` bytes of it.
fn socket_received(process: &Process, page: u64, len: usize) -> Result<Vec<u8>, &'static str> {
    let mut out = alloc::vec![0_u8; len];
    uaccess::copy_from_user(process.space(), page + RECEIVED, &mut out)
        .map_err(|_| "could not read back what a socket received")?;
    Ok(out)
}

/// What `ioctl` says is queued: `SIOCINQ` to read, `SIOCOUTQ` to send.
fn socket_queued(process: &Process, page: u64, fd: i32, request: u32) -> Result<u32, &'static str> {
    let _ = fd::sys_ioctl(process, fd, request, page + RECEIVED)
        .map_err(|_| "a socket would not say what it has queued")?;
    uaccess::get_u32(process.space(), page + RECEIVED).map_err(|_| "could not read a queue length")
}

/// `getsockopt(SOL_SOCKET, name)`, up to `len` bytes of it.
fn socket_option(
    process: &Process,
    page: u64,
    fd: i32,
    name: i32,
    len: usize,
) -> Result<Vec<u8>, Errno> {
    let length = page + RECEIVED;
    let value = length + 8;
    uaccess::put_u32(process.space(), length, u32::try_from(len).unwrap_or(0))?;
    let level = u64::from(SOL_SOCKET.cast_unsigned());
    let name = u64::from(name.cast_unsigned());
    let _ = socket_call(
        process,
        Call::Getsockopt,
        &[as_arg(fd), level, name, value, length, 0],
    )?;
    let mut out = alloc::vec![0_u8; len];
    uaccess::copy_from_user(process.space(), value, &mut out).map_err(|_| Errno::EFAULT)?;
    Ok(out)
}

/// The first four bytes of an option, as the `int` most of them are.
fn socket_option_int(process: &Process, page: u64, fd: i32, name: i32) -> Result<i32, Errno> {
    let bytes = socket_option(process, page, fd, name, 4)?;
    let bytes = bytes.first_chunk::<4>().ok_or(Errno::EINVAL)?;
    Ok(i32::from_le_bytes(*bytes))
}

/// `setsockopt(SOL_SOCKET, name, value)`.
fn set_socket_option(
    process: &Process,
    page: u64,
    fd: i32,
    name: i32,
    value: &[u8],
) -> Result<usize, Errno> {
    let at = page + SENT;
    uaccess::copy_to_user(process.space(), at, value).map_err(|_| Errno::EFAULT)?;
    let level = u64::from(SOL_SOCKET.cast_unsigned());
    let name = u64::from(name.cast_unsigned());
    socket_call(
        process,
        Call::Setsockopt,
        &[as_arg(fd), level, name, at, value.len() as u64, 0],
    )
}

/// What the socket calls refuse: the types and protocols no family has, a
/// call on something that is not a socket, and one on a closed descriptor.
fn check_what_the_socket_calls_refuse(process: &Process, page: u64) -> Result<(), &'static str> {
    let unix = u64::from(AF_UNIX);
    let stream = u64::from(SOCK_STREAM);
    let socket = |a: [u64; 6]| socket_call(process, Call::Socket, &a);
    check_the_internet_families_open(process)?;
    refuses(
        socket([u64::from(AF_MAX), stream, 0, 0, 0, 0]),
        Errno::EAFNOSUPPORT,
        "socket for a family past AF_MAX was not EAFNOSUPPORT",
    )?;
    refuses(
        socket([unix, stream | 0x100, 0, 0, 0, 0]),
        Errno::EINVAL,
        "a socket type with an unknown flag was not EINVAL",
    )?;
    refuses(
        socket([unix, 11, 0, 0, 0, 0]),
        Errno::EINVAL,
        "a socket type past SOCK_MAX was not EINVAL",
    )?;
    refuses(
        socket([unix, u64::from(SOCK_RDM), 0, 0, 0, 0]),
        Errno::ESOCKTNOSUPPORT,
        "SOCK_RDM on AF_UNIX was not ESOCKTNOSUPPORT",
    )?;
    refuses(
        socket([unix, stream, 6, 0, 0, 0]),
        Errno::EPROTONOSUPPORT,
        "TCP's protocol number on AF_UNIX was not EPROTONOSUPPORT",
    )?;
    refuses(
        socket_call(
            process,
            Call::Socketpair,
            &[unix, stream, 0, KERNEL_HALF_BASE, 0, 0],
        ),
        Errno::EFAULT,
        "socketpair wrote its descriptors into the kernel half",
    )?;
    refuses(
        socket_call(process, Call::Bind, &[1, 0, 0, 0, 0, 0]),
        Errno::ENOTSOCK,
        "bind on the console was not ENOTSOCK",
    )?;
    refuses(
        socket_call(process, Call::Listen, &[99, 0, 0, 0, 0, 0]),
        Errno::EBADF,
        "listen on a closed descriptor was not EBADF",
    )?;
    check_a_socket_refuses_what_is_still_to_come(process, page)
}

/// The internet families open, and refuse the types and protocols they do not
/// have.
///
/// A raw socket opens for root, as it does for a process with `CAP_NET_RAW`;
/// [`check_what_uid_1000_is_told`] is the negative control, where the same call
/// is `EPERM`. Protocol zero on a raw socket matches nothing in
/// `inet_create`'s lookup and is `EPROTONOSUPPORT`, and a protocol at or past
/// `IPPROTO_MAX` is `EINVAL` before any lookup.
fn check_the_internet_families_open(process: &Process) -> Result<(), &'static str> {
    const INET: u64 = 2;
    const PACKET: u64 = 17;
    // `htons(ETH_P_IP)` as a little-endian machine passes it.
    const ETH_P_IP_NETWORK_ORDER: u64 = 0x0008;
    const INET6: u64 = 10;
    const TCP: u64 = 6;
    const UDP: u64 = 17;
    const ICMP: u64 = 1;
    const ICMPV6: u64 = 58;
    let stream = u64::from(SOCK_STREAM);
    let datagram = u64::from(SOCK_DGRAM);
    let raw = u64::from(SOCK_RAW);
    let socket = |a: [u64; 6]| socket_call(process, Call::Socket, &a);

    for arguments in [
        [INET, stream, 0, 0, 0, 0],
        [INET, stream, TCP, 0, 0, 0],
        [INET, datagram, 0, 0, 0, 0],
        [INET, datagram, UDP, 0, 0, 0],
        [INET, datagram, ICMP, 0, 0, 0],
        [INET, raw, ICMP, 0, 0, 0],
        [INET, raw, 255, 0, 0, 0],
        [PACKET, datagram, ETH_P_IP_NETWORK_ORDER, 0, 0, 0],
        [PACKET, raw, 0, 0, 0, 0],
        [INET6, stream, 0, 0, 0, 0],
        [INET6, datagram, 0, 0, 0, 0],
        [INET6, raw, ICMPV6, 0, 0, 0],
    ] {
        let Ok(opened) = socket(arguments) else {
            return Err("an internet socket a program may open was refused");
        };
        let descriptor = i32::try_from(opened).map_err(|_| "a socket got no descriptor")?;
        if fd::sys_close(process, descriptor).is_err() {
            return Err("an internet socket could not be closed");
        }
    }
    refuses(
        socket([INET, raw, 0, 0, 0, 0]),
        Errno::EPROTONOSUPPORT,
        "a raw socket at protocol zero was not EPROTONOSUPPORT",
    )?;
    refuses(
        socket([INET6, raw, 0, 0, 0, 0]),
        Errno::EPROTONOSUPPORT,
        "an IPv6 raw socket at protocol zero was not EPROTONOSUPPORT",
    )?;
    refuses(
        socket([INET, raw, 263, 0, 0, 0]),
        Errno::EINVAL,
        "a raw socket at IPPROTO_MAX was not EINVAL",
    )?;
    refuses(
        socket([PACKET, stream, 0, 0, 0, 0]),
        Errno::ESOCKTNOSUPPORT,
        "a stream packet socket was not ESOCKTNOSUPPORT",
    )?;
    refuses(
        socket([INET, stream, UDP, 0, 0, 0]),
        Errno::EPROTONOSUPPORT,
        "UDP's protocol number on a stream socket was not EPROTONOSUPPORT",
    )?;
    refuses(
        socket([INET, datagram, TCP, 0, 0, 0]),
        Errno::EPROTONOSUPPORT,
        "TCP's protocol number on a datagram socket was not EPROTONOSUPPORT",
    )?;
    refuses(
        socket_call(process, Call::Socketpair, &[INET, stream, 0, 0, 0, 0]),
        Errno::EOPNOTSUPP,
        "socketpair on AF_INET was not EOPNOTSUPP",
    )
}

/// On a socket that is real, the calls the next landings bring say so, and
/// the arguments none of them will ever take are refused as Linux refuses
/// them.
fn check_a_socket_refuses_what_is_still_to_come(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let pair = socket_pair(process, page, SOCK_STREAM)?;
    let outcome = (|| {
        let fd = as_arg(pair.0);
        // A pair is connected already, so it may neither be given a name nor
        // listen -- Linux's `unix_bind` and `unix_listen` both refuse.
        refuses(
            socket_call(process, Call::Listen, &[fd, 1, 0, 0, 0, 0]),
            Errno::EINVAL,
            "listen on a connected Unix socket was not EINVAL",
        )?;
        refuses(
            socket_call(process, Call::Accept4, &[fd, page, 0, 0x4000, 0, 0]),
            Errno::EINVAL,
            "accept4 with an unknown flag was not EINVAL",
        )?;
        refuses(
            socket_send(process, page, pair.0, b"x", MSG_OOB),
            Errno::EOPNOTSUPP,
            "MSG_OOB on a Unix socket was not EOPNOTSUPP",
        )?;
        refuses(
            socket_call(process, Call::Shutdown, &[fd, 7, 0, 0, 0, 0]),
            Errno::EINVAL,
            "shutdown with an unknown direction was not EINVAL",
        )
    })();
    close_socket_pair(process, pair);
    outcome
}

/// A stream pair: bytes in one end and out of the other, across writes, with
/// the queue lengths, a peek, and what an empty non-blocking socket says.
fn check_a_stream_pair_carries_bytes(process: &Process, page: u64) -> Result<(), &'static str> {
    let pair = socket_pair(process, page, SOCK_STREAM)?;
    let outcome = stream_pair_answers(process, page, pair);
    close_socket_pair(process, pair);
    outcome
}

/// [`check_a_stream_pair_carries_bytes`]'s questions.
fn stream_pair_answers(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
) -> Result<(), &'static str> {
    refuses(
        socket_recv(process, page, other, 8, 0),
        Errno::EAGAIN,
        "an empty non-blocking socket did not answer EAGAIN",
    )?;
    let empty = fd::file(process, one).map_err(|_| "a pair lost a descriptor")?;
    if !empty.io().poll().writable || empty.io().poll().readable {
        return Err("an empty socket was not writable and only writable");
    }
    answers(
        socket_send(process, page, one, b"hel", 0),
        3,
        "a stream socket did not take three bytes",
    )?;
    answers(
        socket_send(process, page, one, b"lo", 0),
        2,
        "a stream socket did not take two more bytes",
    )?;
    if socket_queued(process, page, one, SIOCOUTQ)? != 5
        || socket_queued(process, page, other, SIOCINQ)? != 5
    {
        return Err("a stream pair did not report five bytes queued between them");
    }
    // A peek reads without taking, so the queue is as long afterwards.
    answers(
        socket_recv(process, page, other, 8, MSG_PEEK),
        5,
        "a peek did not see five bytes",
    )?;
    if socket_received(process, page, 5)? != b"hello" {
        return Err("a peek did not see both writes as one stream");
    }
    if socket_queued(process, page, other, SIOCINQ)? != 5 {
        return Err("a peek took the bytes it looked at");
    }
    // Then a read that crosses both writes, and one that finds the rest.
    answers(
        socket_recv(process, page, other, 4, 0),
        4,
        "a stream read did not cross the two writes",
    )?;
    if socket_received(process, page, 4)? != b"hell" {
        return Err("a stream read gave back the wrong bytes");
    }
    // `MSG_WAITALL` on a non-blocking socket takes what is there.
    answers(
        socket_recv(process, page, other, 8, MSG_WAITALL),
        1,
        "MSG_WAITALL on a non-blocking socket did not take what was queued",
    )?;
    // A length with no address names nothing, as on Linux.
    let at = page + SENT;
    uaccess::copy_to_user(process.space(), at, b"z").map_err(|_| "could not stage a byte")?;
    answers(
        socket_call(
            process,
            Call::Sendto,
            &[as_arg(one), at, 1, u64::from(MSG_NOSIGNAL), 0, 16],
        ),
        1,
        "sendto with a length but no address was refused on a connected stream",
    )?;
    answers(
        socket_recv(process, page, other, 8, 0),
        1,
        "a send with a length but no address did not arrive",
    )?;
    refuses(
        socket_recv(process, page, other, 8, 0),
        Errno::EAGAIN,
        "a drained non-blocking socket did not answer EAGAIN",
    )
}

/// A sequenced-packet pair keeps each send whole and truncates a record that
/// does not fit, and a datagram pair reports the next record's length rather
/// than everything it holds.
fn check_records_keep_their_boundaries(process: &Process, page: u64) -> Result<(), &'static str> {
    let pair = socket_pair(process, page, SOCK_SEQPACKET)?;
    let outcome = record_pair_answers(process, page, pair);
    close_socket_pair(process, pair);
    outcome?;
    let pair = socket_pair(process, page, SOCK_DGRAM)?;
    let outcome = datagram_pair_answers(process, page, pair);
    close_socket_pair(process, pair);
    outcome
}

/// [`check_records_keep_their_boundaries`]'s sequenced-packet half.
fn record_pair_answers(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
) -> Result<(), &'static str> {
    answers(
        socket_send(process, page, one, b"abc", 0),
        3,
        "a sequenced-packet socket did not take a three-byte record",
    )?;
    answers(
        socket_send(process, page, one, b"wxyz", 0),
        4,
        "a sequenced-packet socket did not take a second record",
    )?;
    // A short read takes the record's head and drops its tail, and with
    // `MSG_TRUNC` says how long the whole record was.
    answers(
        socket_recv(process, page, other, 2, MSG_TRUNC),
        3,
        "MSG_TRUNC did not report the whole record's length",
    )?;
    if socket_received(process, page, 2)? != b"ab" {
        return Err("a short record read gave back the wrong bytes");
    }
    // The next read finds the second record, not the first one's tail.
    answers(
        socket_recv(process, page, other, 8, 0),
        4,
        "a record read did not find the whole second record",
    )?;
    if socket_received(process, page, 4)? != b"wxyz" {
        return Err("the second record came back changed");
    }
    refuses(
        socket_recv(process, page, other, 8, 0),
        Errno::EAGAIN,
        "a drained record socket did not answer EAGAIN",
    )
}

/// [`check_records_keep_their_boundaries`]'s datagram half.
fn datagram_pair_answers(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
) -> Result<(), &'static str> {
    answers(
        socket_send(process, page, one, b"abc", 0),
        3,
        "a datagram socket did not take a datagram",
    )?;
    answers(
        socket_send(process, page, one, b"wxyz", 0),
        4,
        "a datagram socket did not take a second datagram",
    )?;
    if socket_queued(process, page, other, SIOCINQ)? != 3 {
        return Err("a datagram socket did not report the next datagram's length");
    }
    // A datagram socket that stopped receiving refuses with EPIPE, not as if
    // it had gone.
    let how = u64::from(SHUT_RD);
    if socket_call(process, Call::Shutdown, &[as_arg(other), how, 0, 0, 0, 0]) != Ok(0) {
        return Err("shutdown(SHUT_RD) on a datagram socket was refused");
    }
    refuses(
        socket_send(process, page, one, b"x", 0),
        Errno::EPIPE,
        "a datagram sent to a socket that stopped receiving was not EPIPE",
    )?;
    // A socket `socket` made has no peer: a name for one is the next landing.
    let kind = u64::from(SOCK_DGRAM | SOCK_NONBLOCK);
    let alone = socket_call(
        process,
        Call::Socket,
        &[u64::from(AF_UNIX), kind, 0, 0, 0, 0],
    )
    .map_err(|_| "socket would not make an unconnected datagram socket")?;
    let alone = i32::try_from(alone).map_err(|_| "an impossible descriptor")?;
    let outcome = refuses(
        socket_send(process, page, alone, b"x", 0),
        Errno::ENOTCONN,
        "a send on an unconnected datagram socket was not ENOTCONN",
    );
    let _ = fd::sys_close(process, alone);
    outcome
}

/// `shutdown` ends one direction at a time: the peer of a socket that has
/// stopped sending reads what is queued and then end of file, and a socket
/// that has stopped receiving breaks its peer's sends.
fn check_shutdown_ends_one_direction(process: &Process, page: u64) -> Result<(), &'static str> {
    let pair = socket_pair(process, page, SOCK_STREAM)?;
    let outcome = shutdown_answers(process, page, pair);
    close_socket_pair(process, pair);
    outcome?;
    let pair = socket_pair(process, page, SOCK_STREAM)?;
    let outcome = (|| {
        let how = u64::from(SHUT_RD);
        if socket_call(process, Call::Shutdown, &[as_arg(pair.1), how, 0, 0, 0, 0]) != Ok(0) {
            return Err("shutdown(SHUT_RD) was refused");
        }
        refuses(
            socket_send(process, page, pair.0, b"x", 0),
            Errno::EPIPE,
            "a send to a socket that stopped receiving was not EPIPE",
        )
    })();
    close_socket_pair(process, pair);
    outcome
}

/// [`check_shutdown_ends_one_direction`]'s writer half.
fn shutdown_answers(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
) -> Result<(), &'static str> {
    answers(
        socket_send(process, page, one, b"last", 0),
        4,
        "a stream socket did not take its last four bytes",
    )?;
    let how = u64::from(SHUT_WR);
    if socket_call(process, Call::Shutdown, &[as_arg(one), how, 0, 0, 0, 0]) != Ok(0) {
        return Err("shutdown(SHUT_WR) was refused");
    }
    refuses(
        socket_send(process, page, one, b"more", 0),
        Errno::EPIPE,
        "a send after shutdown(SHUT_WR) was not EPIPE",
    )?;
    // What was queued before the shutdown is still there to be read.
    answers(
        socket_recv(process, page, other, 8, 0),
        4,
        "the bytes queued before a shutdown were lost",
    )?;
    answers(
        socket_recv(process, page, other, 8, 0),
        0,
        "a drained socket whose peer stopped sending did not read end of file",
    )?;
    let ended = fd::file(process, other).map_err(|_| "a pair lost a descriptor")?;
    if !ended.io().poll().readable {
        return Err("a socket at end of file was not readable");
    }
    Ok(())
}

/// What a socket says about itself: its type, its family, its buffer sizes,
/// the credentials of whoever made it, a timeout that reads back, and the
/// unnamed address it has until names land.
fn check_a_socket_reports_itself(process: &Process, page: u64) -> Result<(), &'static str> {
    let pair = socket_pair(process, page, SOCK_SEQPACKET)?;
    let outcome = socket_reports(process, page, pair.0);
    close_socket_pair(process, pair);
    outcome
}

/// [`check_a_socket_reports_itself`]'s questions.
fn socket_reports(process: &Process, page: u64, fd: i32) -> Result<(), &'static str> {
    let option = |name: i32| socket_option_int(process, page, fd, name);
    if option(SO_TYPE) != Ok(SOCK_SEQPACKET.cast_signed()) {
        return Err("SO_TYPE did not report the type the socket was made with");
    }
    if option(SO_DOMAIN) != Ok(i32::from(AF_UNIX)) || option(SO_PROTOCOL) != Ok(0) {
        return Err("a Unix socket did not report its family and protocol");
    }
    if option(SO_ERROR) != Ok(0) || option(SO_ACCEPTCONN) != Ok(0) {
        return Err("a socket that is neither failed nor listening said it was");
    }
    // Linux keeps twice what a program asks for, and reports what it kept.
    if set_socket_option(process, page, fd, SO_RCVBUF, &8192_i32.to_le_bytes()) != Ok(0)
        || option(SO_RCVBUF) != Ok(16384)
    {
        return Err("SO_RCVBUF did not read back as twice what was set");
    }
    if set_socket_option(process, page, fd, SO_RCVBUF, &1_i32.to_le_bytes()) != Ok(0)
        || option(SO_RCVBUF) != Ok(i32::try_from(SOCKET_BUFFER_MIN).unwrap_or(0))
    {
        return Err("SO_RCVBUF went below the smallest socket buffer");
    }
    refuses(
        set_socket_option(process, page, fd, SO_RCVBUF, &[1, 0]),
        Errno::EINVAL,
        "a socket option shorter than an int was not EINVAL",
    )?;
    if option(999) != Err(Errno::ENOPROTOOPT) {
        return Err("an unknown socket option was not ENOPROTOOPT");
    }
    socket_credentials_and_timeouts(process, page, fd)?;
    socket_names_are_unnamed(process, page, fd)
}

/// The credentials a pair reports, and a timeout that reads back as it was
/// set -- with microseconds that are not microseconds refused.
fn socket_credentials_and_timeouts(
    process: &Process,
    page: u64,
    fd: i32,
) -> Result<(), &'static str> {
    let credentials = socket_option(process, page, fd, SO_PEERCRED, Ucred::SIZE)
        .map_err(|_| "SO_PEERCRED was refused")?;
    let credentials = Ucred::from_bytes(&credentials).ok_or("SO_PEERCRED was too short")?;
    if credentials.pid != i32::try_from(process.pid()).unwrap_or(-1) {
        return Err("SO_PEERCRED did not name the process that made the pair");
    }
    let word = size_of::<usize>();
    let mut timeout = alloc::vec![0_u8; word * 2];
    stage_word(&mut timeout, 0, 2)?;
    stage_word(&mut timeout, word, 500)?;
    if set_socket_option(process, page, fd, SO_RCVTIMEO_OLD, &timeout) != Ok(0) {
        return Err("SO_RCVTIMEO was refused");
    }
    let read_back = socket_option(process, page, fd, SO_RCVTIMEO_OLD, word * 2)
        .map_err(|_| "SO_RCVTIMEO would not read back")?;
    if read_back != timeout {
        return Err("SO_RCVTIMEO did not read back the time it was set to");
    }
    let mut absurd = alloc::vec![0_u8; word * 2];
    stage_word(&mut absurd, word, 2_000_000)?;
    refuses(
        set_socket_option(process, page, fd, SO_RCVTIMEO_OLD, &absurd),
        Errno::EDOM,
        "a timeout of more microseconds than a second was accepted",
    )
}

/// Write one pointer-sized word into a buffer this check is building.
fn stage_word(bytes: &mut [u8], at: usize, value: u64) -> Result<(), &'static str> {
    width()
        .put_word(bytes, at, value)
        .ok_or("could not stage a word")
}

/// Every socket is unnamed until names land, which `getsockname` reports as
/// the family alone; a socket with no peer has none to report at all.
fn socket_names_are_unnamed(process: &Process, page: u64, fd: i32) -> Result<(), &'static str> {
    let length = page + RECEIVED;
    let address = length + 8;
    uaccess::put_u32(process.space(), length, 128).map_err(|_| "could not stage a length")?;
    let call = [as_arg(fd), address, length, 0, 0, 0];
    if socket_call(process, Call::Getsockname, &call) != Ok(0) {
        return Err("getsockname on a Unix socket was refused");
    }
    let reported =
        uaccess::get_u32(process.space(), length).map_err(|_| "could not read a length back")?;
    let mut family = [0_u8; 2];
    uaccess::copy_from_user(process.space(), address, &mut family)
        .map_err(|_| "could not read an address")?;
    if reported != 2 || u16::from_le_bytes(family) != AF_UNIX {
        return Err("an unnamed socket did not report AF_UNIX and nothing else");
    }
    // The peer of a pair is unnamed too; a socket without one says so.
    if socket_call(process, Call::Getpeername, &call) != Ok(0) {
        return Err("getpeername on a connected socket was refused");
    }
    let alone = socket_call(
        process,
        Call::Socket,
        &[u64::from(AF_UNIX), u64::from(SOCK_STREAM), 0, 0, 0, 0],
    )
    .map_err(|_| "socket would not make an unconnected socket")?;
    let alone = i32::try_from(alone).map_err(|_| "an impossible descriptor")?;
    let unconnected = fd::file(process, alone)
        .map_err(|_| "a socket lost its descriptor")?
        .io()
        .poll();
    if !unconnected.writable || !unconnected.hangup {
        let _ = fd::sys_close(process, alone);
        return Err("an unconnected stream socket did not poll as writable and hung up");
    }
    let outcome = refuses(
        socket_recv(process, page, alone, 8, 0),
        Errno::EINVAL,
        "a receive on a stream socket that was never connected was not EINVAL",
    )
    .and_then(|()| {
        refuses(
            socket_call(
                process,
                Call::Getpeername,
                &[as_arg(alone), address, length, 0, 0, 0],
            ),
            Errno::ENOTCONN,
            "getpeername on an unconnected socket was not ENOTCONN",
        )
    });
    let _ = fd::sys_close(process, alone);
    outcome
}

/// `sendmsg` gathers a message's buffers and `recvmsg` scatters what arrives
/// over its own, whatever their shapes; a message carrying descriptors waits
/// for the landing that carries them.
fn check_a_message_scatters_and_gathers(process: &Process, page: u64) -> Result<(), &'static str> {
    let pair = socket_pair(process, page, SOCK_SEQPACKET)?;
    let outcome = message_answers(process, page, pair);
    close_socket_pair(process, pair);
    outcome
}

/// [`check_a_message_scatters_and_gathers`]'s questions.
fn message_answers(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
) -> Result<(), &'static str> {
    let word = size_of::<usize>() as u64;
    let header = page + MESSAGE;
    let iov = header + 0x40;
    // Two buffers out of the staged bytes, three back into the page.
    uaccess::copy_to_user(process.space(), page + SENT, b"gathered")
        .map_err(|_| "could not stage a message")?;
    write_word(process, iov, page + SENT)?;
    write_word(process, iov + word, 4)?;
    write_word(process, iov + word * 2, page + SENT + 4)?;
    write_word(process, iov + word * 3, 4)?;
    stage_header(process, header, iov, 2)?;
    let sent = socket_call(
        process,
        Call::Sendmsg,
        &[as_arg(one), header, u64::from(MSG_NOSIGNAL), 0, 0, 0],
    );
    if sent != Ok(8) {
        return Err("sendmsg did not gather the whole message");
    }
    for (index, len) in [3_u64, 3, 4].into_iter().enumerate() {
        let entry = iov + (index as u64) * word * 2;
        write_word(process, entry, page + RECEIVED + (index as u64) * 8)?;
        write_word(process, entry + word, len)?;
    }
    stage_header(process, header, iov, 3)?;
    let received = socket_call(process, Call::Recvmsg, &[as_arg(other), header, 0, 0, 0, 0]);
    if received != Ok(8) {
        return Err("recvmsg did not scatter the whole message");
    }
    let out = socket_received(process, page, 24)?;
    if out.get(..3) != Some(b"gat".as_slice())
        || out.get(8..11) != Some(b"her".as_slice())
        || out.get(16..18) != Some(b"ed".as_slice())
    {
        return Err("a message did not scatter into the buffers it was given");
    }
    check_descriptors_travel_with_a_message(process, page, (one, other))?;
    check_credentials_travel_with_a_message(process, page, (one, other))?;
    check_a_cycle_in_flight_is_collected(process, page)?;
    check_a_cycle_a_descriptor_reaches_is_kept(process, page)
}

/// The sender's credentials travel with a message (`SCM_CREDENTIALS`) to a
/// socket that asked for them with `SO_PASSCRED`, which is how Chrome's
/// zygote learns the pid of a child it forked.
///
/// Sent plainly, the byte must arrive with one `SCM_CREDENTIALS` message
/// naming the sender's pid and effective ids; sent with the sender's own
/// credentials named, the same; and once the receiver lets `SO_PASSCRED` go,
/// a byte arrives with no control message at all.
fn check_credentials_travel_with_a_message(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
) -> Result<(), &'static str> {
    let own = crate::fs::socket::credentials_of(process);
    let asked = |on: i32| set_socket_option(process, page, other, SO_PASSCRED, &on.to_le_bytes());
    answers(asked(1), 0, "SO_PASSCRED could not be set")?;
    answers(
        socket_send(process, page, one, b"c", 0),
        1,
        "a plain send to a socket asking for credentials failed",
    )?;
    let capacity = cmsg_space(Ucred::SIZE, width());
    if received_credentials(process, page, other, capacity)? != Some(own) {
        return Err("a plain send did not arrive with its sender's credentials");
    }
    answers(
        message_with_control(process, page, one, SCM_CREDENTIALS, &own.to_bytes(), None),
        1,
        "a message naming its sender's own credentials was not sent",
    )?;
    if received_credentials(process, page, other, capacity)? != Some(own) {
        return Err("named credentials did not arrive as they were named");
    }
    answers(asked(0), 0, "SO_PASSCRED could not be cleared")?;
    answers(
        socket_send(process, page, one, b"c", 0),
        1,
        "a send after SO_PASSCRED was cleared failed",
    )?;
    if received_credentials(process, page, other, capacity)?.is_some() {
        return Err("credentials arrived at a socket that no longer asked for them");
    }
    Ok(())
}

/// Receive one byte from `fd` with `capacity` bytes of control buffer, and
/// the credentials its one control message carries, if it carried one.
fn received_credentials(
    process: &Process,
    page: u64,
    fd: i32,
    capacity: usize,
) -> Result<Option<Ucred>, &'static str> {
    let (count, flags, control_len, control) = receive_with_control(process, page, fd, capacity)?;
    if count != 1 || flags & MSG_CTRUNC != 0 {
        return Err("a receive with room for credentials did not take its byte whole");
    }
    if control_len == 0 {
        return Ok(None);
    }
    let header = CmsgHdr::decode(&control, width()).ok_or("could not read a control message")?;
    if header.level != SOL_SOCKET
        || header.kind != SCM_CREDENTIALS
        || header.len != cmsg_len(Ucred::SIZE, width()) as u64
    {
        return Err("the control message a receive wrote was not SCM_CREDENTIALS");
    }
    let body = CmsgHdr::size(width());
    let field = |at: usize| {
        control
            .get(body + at..body + at + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
            .ok_or("an SCM_CREDENTIALS message was cut short")
    };
    Ok(Some(Ucred {
        pid: field(0)?.cast_signed(),
        uid: field(4)?,
        gid: field(8)?,
    }))
}

/// Write a `msghdr` naming the `count` buffers at `iov` and nothing else.
fn stage_header(process: &Process, header: u64, iov: u64, count: u64) -> Result<(), &'static str> {
    let message = MsgHdr {
        iov,
        iov_len: count,
        ..MsgHdr::default()
    };
    let mut bytes = [0_u8; MsgHdr::size(Width::Bits64)];
    let staged = bytes
        .get_mut(..MsgHdr::size(width()))
        .ok_or("impossible pointer width")?;
    message
        .encode(staged, width())
        .ok_or("could not build a msghdr")?;
    uaccess::copy_to_user(process.space(), header, staged).map_err(|_| "could not stage a msghdr")
}

/// What a receive that does not install the descriptor it was sent fails
/// with, which the negative control requires by name.
const DESCRIPTOR_NOT_INSTALLED: &str =
    "recvmsg did not install the descriptor that came with the bytes";

/// Descriptors travel with a message (`SCM_RIGHTS`) on a sequenced-packet pair.
///
/// A pipe's write end is sent and the sender's own descriptor for it closed:
/// the pipe must see no hangup, because the reference in the queue keeps the
/// file open. The receive must install a new descriptor for that same file,
/// say so in one `SCM_RIGHTS` message filling the control buffer's
/// `CMSG_SPACE`, and flag nothing truncated. Sent again and received with no
/// control buffer, the file must be closed on the way -- the pipe now sees its
/// hangup -- and the message flagged `MSG_CTRUNC`. Then the refusals: a
/// descriptor that is not open is `EBADF`, more than `SCM_MAX_FD` is `EINVAL`,
/// and a control message shorter than its own header is `EINVAL`, as
/// `CMSG_OK` failing is on Linux.
fn check_descriptors_travel_with_a_message(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
) -> Result<(), &'static str> {
    // The check holds the read end, and only a weak reference to the write
    // end: the descriptors and the queue are all that may keep it open.
    let (reader, writer) =
        crate::fs::pipe::new_pipe(true, (0, 0)).map_err(|_| "could not make a pipe to pass")?;
    let sent = Arc::downgrade(&writer);
    let sent_fd = process
        .files()
        .lock()
        .insert(writer, false)
        .map_err(|_| "could not give the check a pipe's write end")?;
    let outcome = a_descriptor_travels(process, page, (one, other), sent_fd, &reader, &sent)
        .and_then(|installed| {
            a_descriptor_with_no_room_is_closed(process, page, (one, other), installed, &reader)
        })
        .and_then(|()| what_passing_descriptors_refuses(process, page, one));
    let _ = fd::sys_close(process, sent_fd);
    outcome
}

/// What a stream read that stops before the bytes bringing a descriptor
/// fails with, which the negative control requires by name.
const STOPPED_BEFORE_DESCRIPTOR: &str =
    "a stream read stopped before the bytes that brought a descriptor";

/// On a stream, a read runs on through plain bytes into the bytes that bring
/// a descriptor, and stops after them, as Linux's `unix_stream_read_generic`
/// does. Measured on a 7.0 host: 100 plain bytes, 100 sent with a descriptor
/// and 100 plain read as 200 and then 100 -- by `read`, by `recvmsg` with a
/// control buffer, which installs the descriptor, and without one, which
/// flags `MSG_CTRUNC` and closes the file -- and a peek sees the same 200.
///
/// Here three, one and three bytes: `recvmsg` with room takes four and
/// installs the descriptor, then three; a peek sees four and takes nothing;
/// `recvmsg` with no room takes four, flags `MSG_CTRUNC` and closes the file,
/// which the pipe it is the write end of sees as a hangup; then three.
fn check_a_stream_read_runs_into_descriptors(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let (reader, writer) =
        crate::fs::pipe::new_pipe(true, (0, 0)).map_err(|_| "could not make a pipe to pass")?;
    // Bound on its own, so the table's lock is gone before the calls below
    // take it.
    let sent_fd = process
        .files()
        .lock()
        .insert(writer, false)
        .map_err(|_| "could not give the check a pipe's write end")?;
    let pair = socket_pair(process, page, SOCK_STREAM);
    let outcome =
        pair.and_then(|pair| stream_runs_into_descriptors(process, page, pair, sent_fd, &reader));
    if let Ok(pair) = pair {
        close_socket_pair(process, pair);
    } else {
        let _ = fd::sys_close(process, sent_fd);
    }
    outcome
}

/// Queue three plain bytes, one that brings `fd`, and three plain on `one`,
/// and close `fd`, whose file the queue then keeps open alone.
fn queue_around_a_descriptor(
    process: &Process,
    page: u64,
    one: i32,
    fd: i32,
) -> Result<(), &'static str> {
    let sent = socket_send(process, page, one, b"abc", 0)
        .and_then(|_| message_with_control(process, page, one, SCM_RIGHTS, &rights(&[fd]), None));
    let _ = fd::sys_close(process, fd);
    answers(
        sent,
        1,
        "a descriptor could not be sent after plain bytes on a stream",
    )?;
    answers(
        socket_send(process, page, one, b"xyz", 0),
        3,
        "plain bytes could not follow a descriptor on a stream",
    )
}

/// [`check_a_stream_read_runs_into_descriptors`]'s two rounds, passing
/// `sent_fd`, the write end of the pipe `reader` reads.
fn stream_runs_into_descriptors(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
    sent_fd: i32,
    reader: &Arc<ferrix_vfs::OpenFile>,
) -> Result<(), &'static str> {
    queue_around_a_descriptor(process, page, one, sent_fd)?;
    let (count, flags, _, control) =
        receive_with_control(process, page, other, cmsg_space(4, width()))?;
    let installed = installed_descriptor(&control).ok_or(STOPPED_BEFORE_DESCRIPTOR)?;
    if count != 4 {
        let _ = fd::sys_close(process, installed);
        return Err(STOPPED_BEFORE_DESCRIPTOR);
    }
    if flags & MSG_CTRUNC != 0 {
        let _ = fd::sys_close(process, installed);
        return Err("a descriptor a stream read ran into was flagged MSG_CTRUNC");
    }
    answers(
        socket_recv(process, page, other, 8, 0),
        3,
        "a stream read ran on past the bytes that brought a descriptor",
    )?;

    queue_around_a_descriptor(process, page, one, installed)?;
    answers(
        socket_recv(process, page, other, 8, MSG_PEEK),
        4,
        "a peek of a stream stopped before the bytes that brought a descriptor",
    )?;
    if reader.poll().hangup {
        return Err("a peek closed a descriptor it ran into");
    }
    let (count, flags, control_len, _) = receive_with_control(process, page, other, 0)?;
    if count != 4 || control_len != 0 || flags & MSG_CTRUNC == 0 {
        return Err("a stream read with no room for a descriptor it ran into was not MSG_CTRUNC");
    }
    if !reader.poll().hangup {
        return Err("a descriptor a stream read ran into with no room for it was not closed");
    }
    answers(
        socket_recv(process, page, other, 8, 0),
        3,
        "a stream read with no room for a descriptor ran on past its bytes",
    )
}

/// The bytes of an `SCM_RIGHTS` message naming `fds`.
fn rights(fds: &[i32]) -> Vec<u8> {
    fds.iter().flat_map(|fd| fd.to_le_bytes()).collect()
}

/// Send `sent_fd` and close it; the queue keeps the file open; the receive
/// installs a descriptor for the same file. Answers that descriptor.
fn a_descriptor_travels(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
    sent_fd: i32,
    reader: &Arc<ferrix_vfs::OpenFile>,
    sent: &alloc::sync::Weak<ferrix_vfs::OpenFile>,
) -> Result<i32, &'static str> {
    answers(
        message_with_control(process, page, one, SCM_RIGHTS, &rights(&[sent_fd]), None),
        1,
        "sendmsg with one descriptor did not send its byte",
    )?;
    let _ = fd::sys_close(process, sent_fd);
    if reader.poll().hangup {
        return Err("a descriptor in a socket's queue did not keep its file open");
    }

    let capacity = cmsg_space(4, width());
    let (count, flags, control_len, control) =
        receive_with_control(process, page, other, capacity)?;
    if count != 1 {
        return Err("recvmsg did not receive the byte the descriptor came with");
    }
    let header = CmsgHdr::decode(&control, width()).ok_or("could not read a control message")?;
    if control_len != capacity as u64
        || header.level != SOL_SOCKET
        || header.kind != SCM_RIGHTS
        || header.len != cmsg_len(4, width()) as u64
    {
        return Err(DESCRIPTOR_NOT_INSTALLED);
    }
    if flags & MSG_CTRUNC != 0 {
        return Err("a descriptor that fitted its control buffer was flagged MSG_CTRUNC");
    }
    let data = control
        .get(CmsgHdr::size(width())..CmsgHdr::size(width()) + 4)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .ok_or("a control message had no descriptor in it")?;
    let installed = i32::from_le_bytes(data);
    let file = fd::file(process, installed).map_err(|_| DESCRIPTOR_NOT_INSTALLED)?;
    if !core::ptr::eq(Arc::as_ptr(&file), sent.as_ptr()) {
        return Err("recvmsg installed a descriptor for another file than the one sent");
    }
    Ok(installed)
}

/// Send `installed` and close it; a receive with no control buffer closes the
/// file on the way -- the pipe sees its hangup -- and flags `MSG_CTRUNC`.
fn a_descriptor_with_no_room_is_closed(
    process: &Process,
    page: u64,
    (one, other): (i32, i32),
    installed: i32,
    reader: &Arc<ferrix_vfs::OpenFile>,
) -> Result<(), &'static str> {
    let sent = message_with_control(process, page, one, SCM_RIGHTS, &rights(&[installed]), None);
    let _ = fd::sys_close(process, installed);
    answers(
        sent,
        1,
        "sendmsg with a received descriptor did not send its byte",
    )?;
    if reader.poll().hangup {
        return Err("a descriptor sent a second time did not keep its file open");
    }
    let (count, flags, control_len, _) = receive_with_control(process, page, other, 0)?;
    if count != 1 || control_len != 0 || flags & MSG_CTRUNC == 0 {
        return Err("a descriptor with no control buffer to go in was not flagged MSG_CTRUNC");
    }
    if !reader.poll().hangup {
        return Err("a descriptor a receive had no room for was not closed");
    }
    Ok(())
}

/// What a cycle nobody can reach that outlives its last close fails with,
/// which the negative control requires by name.
const CYCLE_NOT_COLLECTED: &str =
    "a cycle of sockets passed over each other outlived the last close of them";

/// What an emptied queue a descriptor could still read fails with.
const REACHABLE_EMPTIED: &str =
    "a queue a program could still read was emptied as though nobody could";

/// Make a sequenced-packet pair and pass each end over itself, so each end's
/// open file sits in the other's queue. Answers the two descriptors.
fn socket_cycle(process: &Process, page: u64) -> Result<(i32, i32), &'static str> {
    let (one, other) = socket_pair(process, page, SOCK_SEQPACKET)?;
    let passed = message_with_control(process, page, one, SCM_RIGHTS, &rights(&[one]), None)
        .and_then(|_| {
            message_with_control(process, page, other, SCM_RIGHTS, &rights(&[other]), None)
        });
    if passed != Ok(1) {
        close_socket_pair(process, (one, other));
        return Err("a socket could not be passed over its own connection");
    }
    Ok((one, other))
}

/// A pair whose ends were each passed over themselves and then closed is
/// referred to only by the other's queue: the close must collect both, so that
/// neither open file outlives it.
fn check_a_cycle_in_flight_is_collected(process: &Process, page: u64) -> Result<(), &'static str> {
    let (one, other) = socket_cycle(process, page)?;
    let files = (
        fd::file(process, one).map(|file| Arc::downgrade(&file)),
        fd::file(process, other).map(|file| Arc::downgrade(&file)),
    );
    let _ = fd::sys_close(process, one);
    let _ = fd::sys_close(process, other);
    let (Ok(one), Ok(other)) = files else {
        return Err("a socket of a cycle had no open file");
    };
    if one.upgrade().is_some() || other.upgrade().is_some() {
        return Err(CYCLE_NOT_COLLECTED);
    }
    Ok(())
}

/// The same cycle, with a third descriptor still open on one end: nothing may
/// be collected, and the queues must still hold what was sent, which a program
/// reads back through that descriptor and the one it receives.
fn check_a_cycle_a_descriptor_reaches_is_kept(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let (one, other) = socket_cycle(process, page)?;
    let Ok(kept) = fd::sys_dup(process, one).map(|fd| fd as i32) else {
        close_socket_pair(process, (one, other));
        return Err("could not keep a descriptor for a socket of a cycle");
    };
    let _ = fd::sys_close(process, one);
    let _ = fd::sys_close(process, other);
    let outcome = (|| {
        let capacity = cmsg_space(4, width());
        let (count, _, _, control) = receive_with_control(process, page, kept, capacity)?;
        let other = installed_descriptor(&control)
            .filter(|_| count == 1)
            .ok_or(REACHABLE_EMPTIED)?;
        let received = receive_with_control(process, page, other, capacity);
        let one = received.as_ref().ok().and_then(|(count, _, _, control)| {
            installed_descriptor(control).filter(|_| *count == 1)
        });
        let _ = fd::sys_close(process, other);
        let one = one.ok_or(REACHABLE_EMPTIED)?;
        let _ = fd::sys_close(process, one);
        Ok(())
    })();
    let _ = fd::sys_close(process, kept);
    outcome
}

/// The descriptor an `SCM_RIGHTS` message of one names, if `control` holds one.
fn installed_descriptor(control: &[u8]) -> Option<i32> {
    let header = CmsgHdr::decode(control, width())?;
    if header.level != SOL_SOCKET || header.kind != SCM_RIGHTS {
        return None;
    }
    control
        .get(CmsgHdr::size(width())..CmsgHdr::size(width()) + 4)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(i32::from_le_bytes)
}

/// A descriptor that is not open is `EBADF`, more than `SCM_MAX_FD` is
/// `EINVAL`, credentials are `EOPNOTSUPP` until they land, and a control
/// message shorter than its own header is `EINVAL`.
fn what_passing_descriptors_refuses(
    process: &Process,
    page: u64,
    one: i32,
) -> Result<(), &'static str> {
    refuses(
        message_with_control(process, page, one, SCM_RIGHTS, &rights(&[-1]), None),
        Errno::EBADF,
        "a message naming a closed descriptor was not EBADF",
    )?;
    let too_many = alloc::vec![0_i32; SCM_MAX_FD + 1];
    refuses(
        message_with_control(process, page, one, SCM_RIGHTS, &rights(&too_many), None),
        Errno::EINVAL,
        "a message with more than SCM_MAX_FD descriptors was not EINVAL",
    )?;
    refuses(
        message_with_control(process, page, one, SCM_RIGHTS, &[], Some(4)),
        Errno::EINVAL,
        "a control message shorter than its own header was not EINVAL",
    )
}

/// `sendmsg` of one byte on `fd` with one `SOL_SOCKET` control message of
/// `kind` and `data`; `len` overrides its `cmsg_len`.
fn message_with_control(
    process: &Process,
    page: u64,
    fd: i32,
    kind: i32,
    data: &[u8],
    len: Option<u64>,
) -> Result<usize, Errno> {
    let header = page + MESSAGE;
    let iov = header + 0x40;
    let control = page + CONTROL;
    let word = size_of::<usize>() as u64;
    write_word(process, iov, page + SENT).map_err(|_| Errno::EFAULT)?;
    write_word(process, iov + word, 1).map_err(|_| Errno::EFAULT)?;
    let mut buffer = alloc::vec![0_u8; cmsg_space(data.len(), width())];
    CmsgHdr {
        len: len.unwrap_or(cmsg_len(data.len(), width()) as u64),
        level: SOL_SOCKET,
        kind,
    }
    .encode(&mut buffer, width())
    .ok_or(Errno::EINVAL)?;
    let body = CmsgHdr::size(width());
    buffer
        .get_mut(body..body + data.len())
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(data);
    uaccess::copy_to_user(process.space(), control, &buffer).map_err(|_| Errno::EFAULT)?;
    stage_header(process, header, iov, 1).map_err(|_| Errno::EFAULT)?;
    uaccess::put_word(
        process.space(),
        header + MsgHdr::control_offset(width()) as u64,
        control,
    )?;
    uaccess::put_word(
        process.space(),
        header + MsgHdr::control_len_offset(width()) as u64,
        buffer.len() as u64,
    )?;
    socket_call(
        process,
        Call::Sendmsg,
        &[as_arg(fd), header, u64::from(MSG_NOSIGNAL), 0, 0, 0],
    )
}

/// `recvmsg` of up to eight bytes on `fd` with a control buffer of `capacity`
/// bytes, none if zero. Answers the count, `msg_flags`, `msg_controllen` and
/// the control buffer's bytes.
fn receive_with_control(
    process: &Process,
    page: u64,
    fd: i32,
    capacity: usize,
) -> Result<(usize, u32, u64, Vec<u8>), &'static str> {
    let header = page + MESSAGE;
    let iov = header + 0x40;
    let control = page + CONTROL;
    let word = size_of::<usize>() as u64;
    write_word(process, iov, page + RECEIVED)?;
    write_word(process, iov + word, 8)?;
    stage_header(process, header, iov, 1)?;
    let (at, len) = if capacity == 0 {
        (0, 0)
    } else {
        (control, capacity as u64)
    };
    uaccess::put_word(
        process.space(),
        header + MsgHdr::control_offset(width()) as u64,
        at,
    )
    .map_err(|_| "could not stage a control buffer")?;
    uaccess::put_word(
        process.space(),
        header + MsgHdr::control_len_offset(width()) as u64,
        len,
    )
    .map_err(|_| "could not stage a control buffer")?;
    let count = socket_call(process, Call::Recvmsg, &[as_arg(fd), header, 0, 0, 0, 0])
        .map_err(|_| "recvmsg of a message carrying a descriptor failed")?;
    let flags = uaccess::get_u32(
        process.space(),
        header + MsgHdr::flags_offset(width()) as u64,
    )
    .map_err(|_| "could not read msg_flags back")?;
    let control_len = uaccess::get_word(
        process.space(),
        header + MsgHdr::control_len_offset(width()) as u64,
    )
    .map_err(|_| "could not read msg_controllen back")?;
    let mut bytes = alloc::vec![0_u8; capacity];
    if capacity != 0 {
        uaccess::copy_from_user(process.space(), control, &mut bytes)
            .map_err(|_| "could not read a control buffer back")?;
    }
    Ok((count, flags, control_len, bytes))
}

/// A process that ends closes its descriptors then, not when it is reaped.
///
/// A never-started process holds a pipe's only write end; after its kill, the
/// read end must see the hangup that end of file is made of, while the process
/// itself is still referenced here, as an unreaped child is by its parent.
/// Without it, `ls | wc -l` in the shell hangs.
fn check_an_ended_process_closes_its_descriptors() -> Result<(), &'static str> {
    let holder = process::new_for_check().map_err(|_| "no process to hold a pipe's write end")?;
    let (reader, writer) =
        crate::fs::pipe::new_pipe(false, (0, 0)).map_err(|_| "could not make a pipe")?;
    let _fd = holder
        .files()
        .lock()
        .insert(writer, false)
        .map_err(|_| "could not give a process a pipe's write end")?;
    if reader.poll().hangup {
        return Err("a pipe's read end saw a hangup while its writer was still open");
    }
    process::kill(&holder, 137);
    if !reader.poll().hangup {
        return Err("a process that ended kept its descriptors open until it was let go");
    }
    Ok(())
}

/// A start prepared and then dropped gives back all it took -- its stack's
/// frames, its thread's live count and the claim -- and leaves the process
/// startable.
///
/// This is what lets `process_start` make its child's task before it moves the
/// bootstrap handle: a move refused afterwards drops the prepared start, and
/// nothing is left to undo. The program is started for real afterwards, by
/// [`check_a_program_is_handed_its_start_argument`], so a drop that gave back
/// too much would show there too.
fn check_a_dropped_prepared_start_gives_everything_back(
    program: &Arc<Process>,
) -> Result<(), &'static str> {
    // Once outside the window, for the reason the other frame checks give: the
    // heap keeps a page of each size class the first run touches.
    prepare_and_drop_a_start(program)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    prepare_and_drop_a_start(program)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    // Above zero the drop kept frames; below zero something outside the check
    // gave frames back inside the window, which is not this check's to judge.
    let kept = window.kept();
    if kept > 0 {
        mm::print_frame_delta("prepare", kept);
        window.report("prepare");
        return Err("a dropped prepared start did not give its task's stack back");
    }
    let claim = process::claim_start(program)
        .map_err(|_| "a dropped prepared start did not give its claim back")?;
    drop(claim);
    Ok(())
}

/// Prepare `program`'s start and drop it: its thread is counted live and the
/// claim held while it is prepared, and neither once it is dropped.
fn prepare_and_drop_a_start(program: &Arc<Process>) -> Result<(), &'static str> {
    let prepared = process::claim_start(program)
        .map_err(|_| "a loaded program's start could not be claimed to prepare it")?
        .prepare(None)
        .map_err(|_| "a claimed start could not be prepared")?;
    if program.live_thread_count() != 1 {
        drop(prepared);
        return Err("a prepared start did not count its thread live");
    }
    if process::claim_start(program).is_ok() {
        drop(prepared);
        return Err("a prepared start did not keep its claim");
    }
    drop(prepared);
    if program.live_thread_count() != 0 {
        return Err("a dropped prepared start kept its thread counted live");
    }
    Ok(())
}

/// The start argument [`check_a_program_is_handed_its_start_argument`] passes.
const START_ARGUMENT: u64 = 57;
/// What the program must exit with when the argument arrived.
const START_STATUS: i32 = 57;

/// A program started with an argument finds it in its first argument register.
///
/// Every general register is cleared on entry so nothing of the kernel's leaks
/// into user mode, and the one exception is this value: zero for a Linux
/// program, a native process's bootstrap handle. The program exits with the
/// register as its status, so an entry path that cleared it after loading it,
/// or loaded the wrong register, exits with 0 or garbage instead.
/// Verifies: `L.x86_64.68`
fn check_a_program_is_handed_its_start_argument() -> Result<Option<i32>, &'static str> {
    if arch::USER_ARGUMENT_PROGRAM.is_empty() {
        return Ok(None);
    }
    let file = image::build_with(
        class_of_this_build(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_ARGUMENT_PROGRAM,
    );
    let program = exec::load_native(&file, b"/argument", Credentials::root(), None)
        .map_err(|_| "a program to hand a start argument to could not be loaded")?;
    let claim = process::claim_start(&program)
        .map_err(|_| "a loaded program's start could not be claimed")?;
    if process::claim_start(&program).is_ok() {
        return Err("a process's start was claimed twice at once");
    }
    drop(claim);
    check_a_dropped_prepared_start_gives_everything_back(&program)?;
    let claim = process::claim_start(&program)
        .map_err(|_| "a released claim on a process could not be taken again")?;
    let _task = claim
        .start(None, START_ARGUMENT)
        .map_err(|_| "a program handed a start argument could not be started")?;
    if process::start(&program).is_ok() {
        return Err("a process that had already started was started a second time");
    }

    // A process ended before anyone started it cannot be claimed: a native
    // `process_start` on a killed child would otherwise report a start whose
    // task returns before entering the program.
    let ended = exec::load_native(&file, b"/argument", Credentials::root(), None)
        .map_err(|_| "a program to end before its start could not be loaded")?;
    process::kill(&ended, 137);
    if process::claim_start(&ended).is_ok() {
        return Err("the start of a process that had already ended was claimed");
    }
    drop(ended);

    let status = program
        .wait_for_exit(u64::MAX)
        .ok_or("a program handed a start argument never reported how it ended")?;
    if status != START_STATUS {
        return Err("a program did not find its start argument in its first argument register");
    }
    Ok(Some(status))
}

/// `name_to_handle_at` walks the name and then answers `EOPNOTSUPP`, the one
/// failure libudev does not print when it asks it of `/dev`; a name that is
/// not there is still `ENOENT`, and flags it does not know, or a connectable
/// handle only for comparing, are `EINVAL`.
///
/// At the end of the file, and not among the path checks, so that no checked
/// function above it moves off the lines its coverage was measured on.
pub(crate) fn run_handles() -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{AT_FDCWD, AT_HANDLE_CONNECTABLE, AT_HANDLE_FID};
    const CWD: u64 = AT_FDCWD as i64 as u64;
    let process = process::new_for_check().map_err(|_| "could not make a process")?;
    let page = map_rw(&process, PAGE_SIZE)?;
    let (root, missing) = (page, page + 64);
    let (handle, mount) = (page + 128, page + 512);
    uaccess::copy_to_user(process.space(), root, b"/\0")
        .and_then(|()| uaccess::copy_to_user(process.space(), missing, b"/tmp/no-handle-here\0"))
        .map_err(|_| "could not stage a path")?;
    let call = |args| call_by_number(&process, Call::NameToHandleAt, args);
    if call([CWD, root, handle, mount, 0, 0]) != Err(Errno::EOPNOTSUPP) {
        return Err("name_to_handle_at on the root was not EOPNOTSUPP");
    }
    if call([CWD, missing, handle, mount, 0, 0]) != Err(Errno::ENOENT) {
        return Err("name_to_handle_at on a missing name was not ENOENT");
    }
    for flags in [0x8000, AT_HANDLE_CONNECTABLE | AT_HANDLE_FID] {
        if call([CWD, root, handle, mount, u64::from(flags), 0]) != Err(Errno::EINVAL) {
            return Err("name_to_handle_at took flags it must refuse");
        }
    }
    let _ = memory::sys_munmap(&process, page, PAGE_SIZE).map_err(|_| "munmap was refused")?;
    Ok(())
}
