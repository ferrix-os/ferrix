//! Syscall User Dispatch, proved at boot (`docs/SECCOMP.md` §13.5, the
//! design's 7.1; the certification consultant's ledger line 2323).
//!
//! The settings and `prctl`'s rows are asked of the module directly. The
//! calls a dispatching thread makes run in a task that is a real thread of a
//! check process, as seccomp's filters check runs its scenarios: the task
//! makes them through the core's own entry (`arch::drive_system_call`) with
//! the instruction address each case needs, because the rule reads the
//! running thread and forces its signal on it. A case that ends its process
//! (a bad selector, a thread a filter is ending) runs in a process of its
//! own and is read by how that process ended.
//!
//! What is not here: `execve` clearing the settings (the check processes are
//! never started, so no boot check execs), which the guest program
//! `src/tests/sud` checks from the dispatching thread itself (its step 8);
//! and a selector on a file-backed page, since the check processes map
//! anonymous memory only. The never-touched anonymous page below is the
//! fault-in case.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;

use super::user_dispatch::{
    self, PR_SET_SYSCALL_USER_DISPATCH, PR_SYS_DISPATCH_EXCLUSIVE_ON, PR_SYS_DISPATCH_INCLUSIVE_ON,
    PR_SYS_DISPATCH_OFF, SYSCALL_DISPATCH_FILTER_ALLOW, SYSCALL_DISPATCH_FILTER_BLOCK,
};
use crate::arch;
use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::check::map_rw;
use crate::syscall::process::{self, Process};
use crate::syscall::seccomp;
use crate::syscall::signal;
use crate::syscall::thread::Thread;
use crate::syscall::uaccess;
use crate::trap::{Abi, SyscallArgs, Verdict};

/// What the check saw, for the `dispatch` boot line.
#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    /// `prctl` rows answered as Linux answers them.
    pub rows: usize,
    /// Calls a dispatching thread made through the entry and judged.
    pub calls: usize,
    /// Processes a dispatch ended.
    pub ended: usize,
    /// What a call from inside the range costs, in nanoseconds.
    pub in_range: u64,
    /// What a call that reads an ALLOW selector costs, in nanoseconds.
    pub selector: u64,
    /// What a dispatched call costs, signal raised and taken, in nanoseconds.
    pub dispatch: u64,
    /// Whether the architecture offers it: `false` on the Arm pair, where
    /// every mode was refused.
    pub built: bool,
}

/// The range the dispatching thread names: calls from `[START, START + LEN)`
/// run without a look.
const START: u64 = 0x2000;
/// Its length.
const LEN: u64 = 0x1000;
/// An address outside it.
const OUTSIDE: u64 = 0x1000;
/// A user address no check process maps.
const UNMAPPED: u64 = 0x0010_0000_0000;
/// A handler address the check gives `SIGSYS`, so that a dispatch is not
/// fatal; nothing runs it, since the task takes the signal back itself.
const HANDLER: u64 = 0x0040_0000;
/// `SIGSYS`.
const SIGSYS: u32 = 31;
/// A process ended by `SIGSYS`, as a shell spells it.
const SIGSYS_STATUS: i32 = 128 + 31;
/// A process ended by `SIGSEGV`.
const SIGSEGV_STATUS: i32 = 128 + 11;
/// A process ended by `SIGKILL`.
const SIGKILL_STATUS: i32 = 128 + 9;
/// How the task ends its process when it was not ended: a pass for the
/// cases that must survive, a failure for those that must not.
const SURVIVED: i32 = 66;
/// How it ends when it could not finish.
const BROKEN: i32 = 67;
/// `channel_write_read`, a native-range number.
const NATIVE_CALL: usize = 0x1013;
/// Calls each cost is the median of.
const TIMED: usize = 64;

/// Run every case.
///
/// # Errors
///
/// Which property failed.
pub fn run() -> Result<Report, &'static str> {
    let mut report = Report {
        built: user_dispatch::BUILT,
        ..Report::default()
    };
    if !user_dispatch::BUILT {
        // (l): the Arm pair refuses every mode and sets nothing.
        refused_everywhere(&mut report)?;
        return Ok(report);
    }
    let before = seccomp::filtered_threads();
    rows(&mut report)?;
    heredity()?;
    let seen = scenario(Case::Calls)?;
    report.calls += seen.calls;
    report.in_range = seen.in_range;
    report.selector = seen.selector;
    report.dispatch = seen.dispatch;
    for (case, status, problem) in [
        (
            Case::BadValue,
            SIGSYS_STATUS,
            "an invalid selector did not end the process with SIGSYS",
        ),
        (
            Case::BadAddress,
            SIGSEGV_STATUS,
            "a selector fault did not end the process with SIGSEGV",
        ),
        (
            Case::Dead,
            SIGKILL_STATUS,
            "a dispatch passed by a thread its filter was ending",
        ),
        (
            Case::Blocked,
            SIGSYS_STATUS,
            "a dispatch with SIGSYS blocked and ignored did not end the process",
        ),
    ] {
        if scenario_status(case)? != status {
            return Err(problem);
        }
        report.ended += 1;
    }
    // (i): every watch this check raised has gone with its threads.
    settle(before)?;
    Ok(report)
}

/// (a): `prctl`'s rows, as Linux's `task_set_syscall_user_dispatch` answers.
fn rows(report: &mut Report) -> Result<(), &'static str> {
    use user_dispatch::configure;
    let page = 0x10_0000;
    let refused: [(u64, u64, u64, u64, Errno, &str); 9] = [
        (
            PR_SYS_DISPATCH_OFF,
            1,
            0,
            0,
            Errno::EINVAL,
            "OFF with an offset was accepted",
        ),
        (
            PR_SYS_DISPATCH_OFF,
            0,
            1,
            0,
            Errno::EINVAL,
            "OFF with a length was accepted",
        ),
        (
            PR_SYS_DISPATCH_OFF,
            0,
            0,
            page,
            Errno::EINVAL,
            "OFF with a selector was accepted",
        ),
        (
            PR_SYS_DISPATCH_EXCLUSIVE_ON,
            START,
            0,
            0,
            Errno::EINVAL,
            "an empty range at an offset was accepted",
        ),
        (
            PR_SYS_DISPATCH_EXCLUSIVE_ON,
            u64::MAX - 1,
            4,
            0,
            Errno::EINVAL,
            "a range that wraps was accepted",
        ),
        (
            PR_SYS_DISPATCH_INCLUSIVE_ON,
            START,
            0,
            0,
            Errno::EINVAL,
            "an empty inclusive range was accepted",
        ),
        (
            PR_SYS_DISPATCH_INCLUSIVE_ON,
            u64::MAX - 1,
            4,
            0,
            Errno::EINVAL,
            "an inclusive range that wraps was accepted",
        ),
        (
            3,
            0,
            0,
            0,
            Errno::EINVAL,
            "a mode that is none was accepted",
        ),
        (
            PR_SYS_DISPATCH_EXCLUSIVE_ON,
            START,
            LEN,
            0xFFFF_8000_0000_0000,
            Errno::EFAULT,
            "a kernel-half selector was accepted",
        ),
    ];
    for (mode, offset, len, selector, errno, problem) in refused {
        if configure(mode, offset, len, selector) != Err(errno) {
            return Err(problem);
        }
        report.rows += 1;
    }
    let off = configure(PR_SYS_DISPATCH_OFF, 0, 0, 0).map_err(|_| "OFF was refused")?;
    if off.is_on() {
        return Err("OFF left dispatch on");
    }
    edges(report)
}

/// (b): the edges of the two kinds of range, and offset 0 with any length.
fn edges(report: &mut Report) -> Result<(), &'static str> {
    use user_dispatch::configure;
    let page = 0x10_0000;
    // Offset 0 with any length is accepted, as Linux accepts it.
    let _ = configure(PR_SYS_DISPATCH_EXCLUSIVE_ON, 0, 0, 0)
        .map_err(|_| "an exclusive range at offset 0 was refused")?;
    // (b): the edges of an exclusive range, the address after the instruction.
    let exclusive = configure(PR_SYS_DISPATCH_EXCLUSIVE_ON, START, LEN, page)
        .map_err(|_| "a good exclusive range was refused")?;
    if !exclusive.exempts(START)
        || !exclusive.exempts(START + LEN - 1)
        || exclusive.exempts(START + LEN)
        || exclusive.exempts(START - 1)
    {
        return Err("a call at the range's end was not dispatched");
    }
    let inclusive = configure(PR_SYS_DISPATCH_INCLUSIVE_ON, START, LEN, 0)
        .map_err(|_| "a good inclusive range was refused")?;
    if inclusive.exempts(START)
        || inclusive.exempts(START + LEN - 1)
        || !inclusive.exempts(START + LEN)
    {
        return Err("an inclusive range judged the calls outside it");
    }
    report.rows += 5;
    Ok(())
}

/// (l): every mode refused with `EINVAL` on an architecture without it.
fn refused_everywhere(report: &mut Report) -> Result<(), &'static str> {
    for mode in [
        PR_SYS_DISPATCH_OFF,
        PR_SYS_DISPATCH_EXCLUSIVE_ON,
        PR_SYS_DISPATCH_INCLUSIVE_ON,
    ] {
        if user_dispatch::configure(mode, 0, 0, 0) != Err(Errno::EINVAL) {
            return Err("an architecture without dispatch accepted a mode");
        }
        report.rows += 1;
    }
    Ok(())
}

/// (g) and part of (i): a dispatching thread's fork or vfork child (both are
/// made by `Thread::forked`, `family.rs`), its `clone(CLONE_THREAD)` sibling
/// and a native child of its process start with dispatch off; and turning it
/// off gives its watch back.
fn heredity() -> Result<(), &'static str> {
    use crate::object::process::{Host, downcast};
    let before = seccomp::filtered_threads();
    let process = process::new_for_check().map_err(|_| "no process for the dispatch check")?;
    let page = map_rw(&process, PAGE_SIZE)?;
    let first = Arc::new(Thread::leader(&process).map_err(|_| "no memory for a check's thread")?);
    process.add_thread(&first);
    let _ = user_dispatch::prctl_set(&first, [PR_SYS_DISPATCH_EXCLUSIVE_ON, START, LEN, page])
        .map_err(|_| "a good dispatch setting was refused")?;
    if !first.is_dispatching() || seccomp::filtered_threads() != before + 1 {
        return Err("a dispatched thread's call was not looked at");
    }
    let child_process =
        process::new_for_check().map_err(|_| "no process for the dispatch check's child")?;
    let forked =
        Thread::forked(&child_process, &first).map_err(|_| "no memory for a check's thread")?;
    let sibling = Thread::sibling(&process, process.pid() + 1, &first)
        .map_err(|_| "no memory for a check's thread")?;
    let file = crate::syscall::check::native_child_image();
    let native = crate::syscall::launch::load_native(
        Some(&*process as &dyn Host),
        &file,
        b"/dispatch-native",
    )
    .ok()
    .and_then(downcast::<Process>)
    .ok_or("a native child could not be made for the dispatch check")?;
    let native_first =
        Thread::leader(&native).map_err(|_| "no thread for a native dispatch child")?;
    let inherited = [&forked, &sibling, &native_first]
        .iter()
        .any(|thread| thread.is_dispatching() || thread.dispatch().is_on());
    process::kill(&native, 137);
    drop((forked, sibling, native_first));
    if inherited {
        return Err("a fork child started dispatched");
    }
    let _ = user_dispatch::prctl_set(&first, [PR_SYS_DISPATCH_OFF, 0, 0, 0])
        .map_err(|_| "OFF was refused")?;
    if first.is_dispatching() || seccomp::filtered_threads() != before {
        return Err("watched count not back after OFF");
    }
    process::kill(&process, 137);
    Ok(())
}

/// Wait until the count of watches is back at `before`, as seccomp's check
/// waits: a reaped task's thread can go a little late.
fn settle(before: usize) -> Result<(), &'static str> {
    let patience = crate::timer::now_nanos().saturating_add(SETTLE_NANOS * 50);
    while seccomp::filtered_threads() != before && crate::timer::now_nanos() < patience {
        sched::sleep_for(SETTLE_NANOS);
        let _ = sched::reap();
    }
    if seccomp::filtered_threads() != before {
        return Err("watched count not back once the dispatch check's threads had gone");
    }
    Ok(())
}

/// How long the boot task waits for a scenario.
const PATIENCE_NANOS: u64 = 60_000_000_000;
/// How long it lets a reaped task settle.
const SETTLE_NANOS: u64 = 20_000_000;

/// What a scenario's task does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    /// The calls a dispatching thread makes, which its process survives:
    /// (b) to (f), (j), (k) and the costs.
    Calls,
    /// A selector byte of 2: the process ends with `SIGSYS` (d).
    BadValue,
    /// A selector that cannot be read: the process ends with `SIGSEGV` (d).
    BadAddress,
    /// A thread its filter is ending: the end wins over a dispatch
    /// (condition 7).
    Dead,
    /// `SIGSYS` blocked and ignored: a dispatch ends the process, as Linux's
    /// `force_sig_info` resets it.
    Blocked,
}

/// What the `Calls` scenario saw.
#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    /// Calls made through the entry and judged.
    calls: usize,
    /// The median cost of a call from inside the range, in nanoseconds.
    in_range: u64,
    /// That of a call that read an ALLOW selector.
    selector: u64,
    /// That of a dispatched call, its signal taken.
    dispatch: u64,
}

/// The case the next scenario task runs.
static CASE: SpinLock<Option<Case>> = SpinLock::new(None);
/// What the scenario task found.
static FOUND: SpinLock<Option<Result<Seen, &'static str>>> = SpinLock::new(None);
/// The page that holds the selector and the filter program.
static PAGE: AtomicU64 = AtomicU64::new(0);
/// A page the check never touches, for a selector read that faults it in.
static FRESH: AtomicU64 = AtomicU64::new(0);

/// Run `case`, which its process must survive.
fn scenario(case: Case) -> Result<Seen, &'static str> {
    let (seen, status) = run_scenario(case)?;
    if status != SURVIVED {
        return Err("a dispatch scenario that must survive ended its process");
    }
    Ok(seen)
}

/// Run `case` and answer how its process ended.
fn scenario_status(case: Case) -> Result<i32, &'static str> {
    run_scenario(case).map(|(_, status)| status)
}

/// Run a scenario task in a process of its own; answer what it found and
/// how its process ended.
fn run_scenario(case: Case) -> Result<(Seen, i32), &'static str> {
    let process = process::new_for_check().map_err(|_| "no process for the dispatch scenario")?;
    PAGE.store(map_rw(&process, PAGE_SIZE)?, Ordering::Release);
    FRESH.store(map_rw(&process, PAGE_SIZE)?, Ordering::Release);
    *FOUND.lock() = None;
    *CASE.lock() = Some(case);
    let task = crate::syscall::check::spawn_in(&process, "dispatch-check", scenario_task, None)?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let status = process
        .wait_for_exit(deadline)
        .ok_or("the dispatch scenario task never ended its process")?;
    let found = FOUND
        .lock()
        .take()
        .ok_or("the dispatch scenario task ended without a result")?;
    drop(task);
    drop(process);
    sched::sleep_for(SETTLE_NANOS);
    let _ = sched::reap();
    Ok((found?, status))
}

/// What [`in_the_process`] leaves its task to do once it has let go of every
/// reference it held.
enum Then {
    /// Report this.
    Report(Seen),
    /// Report `found`, then make `call` from `ip`, which must end the
    /// process; report `returned` if it comes back.
    Ending {
        /// What was seen before it.
        found: Seen,
        /// The call.
        call: usize,
        /// The address after its instruction.
        ip: u64,
        /// The complaint when it returns.
        returned: &'static str,
    },
}

/// The scenario task. A call that ends it is made here, in a frame that
/// holds no reference, since an ended thread never returns to the frames
/// below it (as seccomp's filters check does).
fn scenario_task(_argument: usize) {
    let case = CASE.lock().take();
    let found = match in_the_process(case) {
        Ok(Then::Report(seen)) => Ok(seen),
        Ok(Then::Ending {
            found,
            call,
            ip,
            returned,
        }) => {
            *FOUND.lock() = Some(Ok(found));
            let _ = arch::drive_system_call(Abi::Native, call, [0; 6], ip);
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

/// The number `call` has on this architecture's own entry.
fn number(call: Syscall) -> Result<usize, &'static str> {
    (0..=1024)
        .find(|&candidate| arch::decode_syscall(candidate) == Some(call))
        .ok_or("the decoder has no number for a call the dispatch check uses")
}

/// The signal a dispatch left pending for `thread`, taken as a handler
/// would take it.
fn take(thread: &Thread) -> Option<signal::Taken> {
    thread.with_signals(signal::take_next)
}

/// Ask the personality's look about a call of the running thread from `ip`,
/// as the entry asks it, and answer its verdict. A dispatched call is judged
/// this way and not through the entry: the entry would deliver the forced
/// `SIGSYS` on its way out, and a check task has no user stack to build the
/// handler's frame on (seccomp's filters check judges its traps the same way).
fn look(number: usize, args: [u64; 6], ip: u64) -> Verdict {
    let args = SyscallArgs {
        abi: Abi::Native,
        number,
        args,
        ip,
    };
    let open = arch::interrupts_enabled();
    // The look runs with interrupts masked, as the entry calls it, and opens
    // them itself for its own stretch.
    arch::disable_interrupts();
    let verdict = seccomp::check(&args);
    if open {
        arch::enable_interrupts();
    }
    verdict
}

/// The median of `costs`.
fn median(costs: &mut [u64; TIMED]) -> u64 {
    costs.sort_unstable();
    costs[TIMED / 2]
}

/// What the scenario task does.
#[expect(
    clippy::too_many_lines,
    reason = "one case after another, as the design lists them"
)]
fn in_the_process(case: Option<Case>) -> Result<Then, &'static str> {
    use ferrix_seccomp::{ALLOW, ERRNO, Insn, KILL_PROCESS};

    let thread = crate::syscall::thread::current().ok_or("the dispatch task has no thread")?;
    let process = Arc::clone(thread.process());
    let page = PAGE.load(Ordering::Acquire);
    let fresh = FRESH.load(Ordering::Acquire);
    let call = |nr: usize, args: [u64; 6], ip: u64| -> Result<isize, &'static str> {
        let answer = arch::drive_system_call(Abi::Native, nr, args, ip)
            .ok_or("this architecture has no entry to drive")?;
        if process.is_terminated() {
            process::leave_current();
        }
        Ok(answer)
    };
    let prctl = number(Syscall::Prctl)?;
    let getpid = number(Syscall::Getpid)?;
    let getppid = number(Syscall::Getppid)?;
    let getsid = number(Syscall::Getsid)?;
    let seccomp_call = number(Syscall::Seccomp)?;
    let pid = process.pid() as isize;
    // Set through the entry, as Wine does, from inside the range.
    let set = |mode: u64, selector: u64| -> Result<isize, &'static str> {
        call(
            prctl,
            [
                PR_SET_SYSCALL_USER_DISPATCH as u64,
                mode,
                START,
                LEN,
                selector,
                0,
            ],
            START,
        )
    };
    let put = |value: u8| -> Result<(), &'static str> {
        uaccess::copy_to_user(process.space(), page, &[value])
            .map_err(|_| "the dispatch check could not write its selector")
    };
    // A handler, so that a dispatch is not fatal.
    thread.with_signals(|shared, _| shared.install_action(SIGSYS, HANDLER, 0));
    let ending = |found, returned| Then::Ending {
        found,
        call: getpid,
        ip: OUTSIDE,
        returned,
    };
    match case {
        Some(Case::BadValue) => {
            if set(PR_SYS_DISPATCH_EXCLUSIVE_ON, page)? != 0 {
                return Err("a good dispatch setting was refused through the entry");
            }
            put(2)?;
            return Ok(ending(
                Seen::default(),
                "an invalid selector did not end the process",
            ));
        }
        Some(Case::BadAddress) => {
            if set(PR_SYS_DISPATCH_EXCLUSIVE_ON, UNMAPPED)? != 0 {
                return Err("a selector in the user half was refused");
            }
            return Ok(ending(
                Seen::default(),
                "a selector fault did not end the process",
            ));
        }
        Some(Case::Dead) => {
            if set(PR_SYS_DISPATCH_EXCLUSIVE_ON, page)? != 0 {
                return Err("a good dispatch setting was refused through the entry");
            }
            put(SYSCALL_DISPATCH_FILTER_BLOCK)?;
            seccomp::mark_dead(&thread);
            return Ok(ending(
                Seen::default(),
                "a dispatch passed by a thread its filter was ending",
            ));
        }
        Some(Case::Blocked) => {
            if set(PR_SYS_DISPATCH_EXCLUSIVE_ON, page)? != 0 {
                return Err("a good dispatch setting was refused through the entry");
            }
            put(SYSCALL_DISPATCH_FILTER_BLOCK)?;
            thread.with_signals(|shared, _| shared.install_action(SIGSYS, 1, 0));
            signal::change_blocked(&thread, |_, own| {
                let _ = own.replace_blocked(signal::bit(SIGSYS));
            });
            return Ok(ending(
                Seen::default(),
                "a dispatch with SIGSYS blocked and ignored did not end the process",
            ));
        }
        Some(Case::Calls) | None => {}
    }

    let mut seen = Seen::default();
    if set(PR_SYS_DISPATCH_EXCLUSIVE_ON, page)? != 0 {
        return Err("a good dispatch setting was refused through the entry");
    }
    if !thread.is_dispatching() {
        return Err("a dispatched thread's call was not looked at");
    }
    // (k): the IPC fast path must not serve a dispatching thread's calls.
    if seccomp::quiet() {
        return Err("a dispatching thread was quiet to the IPC fast path");
    }

    // (b): with BLOCK, a call from inside the range runs; from its end on, it
    // is dispatched.
    put(SYSCALL_DISPATCH_FILTER_BLOCK)?;
    // Asked of the look first: a dispatch through the entry would end this
    // task (no user stack for the handler's frame), and the check must say
    // what went wrong.
    if look(getpid, [0; 6], START) != Verdict::Continue
        || look(getpid, [0; 6], START + LEN - 1) != Verdict::Continue
    {
        return Err("a call from inside the range was dispatched");
    }
    if call(getpid, [0; 6], START)? != pid || call(getpid, [0; 6], START + LEN - 1)? != pid {
        return Err("a call from inside the range was dispatched");
    }
    if take(&thread).is_some() {
        return Err("a call from inside the range raised a signal");
    }
    let answer = look(getpid, [0; 6], START + LEN);
    let taken = take(&thread).ok_or("a call at the range's end was not dispatched")?;
    // (e): the call did not run, its registers read as they were (the core
    // gives a Trap the rollback value, L.trap.8), and the signal says what
    // Linux's says.
    if answer != Verdict::Trap {
        return Err("SIGSYS's context lost the syscall number");
    }
    let signal::Origin::Sys {
        code,
        errno,
        call_addr,
        syscall,
        arch: token,
    } = taken.origin
    else {
        return Err("a dispatched call raised a signal that is not a SIGSYS trap");
    };
    if code == signal::SYS_SECCOMP {
        return Err("a dispatch SIGSYS had si_code 1");
    }
    if taken.signal != SIGSYS
        || code != signal::SYS_USER_DISPATCH
        || errno != 0
        || call_addr != START + LEN
        || syscall != getpid as i32
        || token != arch::audit_arch(Abi::Native)
    {
        return Err("a dispatch SIGSYS did not carry the call, its ip and its arch");
    }
    let info = taken.origin.encode(SIGSYS);
    let small = ferrix_linux_abi::sigframe32::siginfo_from_64(&info);
    let word32 = |at: usize| {
        small
            .get(at..at + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(i32::from_le_bytes)
    };
    if word32(8) != Some(signal::SYS_USER_DISPATCH)
        || word32(12) != Some((START + LEN) as i32)
        || word32(16) != Some(getpid as i32)
    {
        return Err("a dispatch SIGSYS's i386 siginfo had its fields elsewhere");
    }
    seen.calls += 4;

    // A dispatched call whose first argument reads as ERESTARTSYS is no restart.
    let minus_512 = (-512_i64) as u64;
    if look(getpid, [minus_512, 0, 0, 0, 0, 0], OUTSIDE) != Verdict::Trap {
        return Err("a dispatched call whose argument reads as ERESTARTSYS ran");
    }
    let _ = take(&thread).ok_or("a dispatched call raised no signal")?;
    if thread.with_signals(|_, own| own.take_restart()).is_some() {
        return Err("a dispatched call left a restart marked");
    }
    seen.calls += 1;

    // (d): ALLOW lets a call from outside go on: the look says nothing, and
    // through the entry it runs.
    put(SYSCALL_DISPATCH_FILTER_ALLOW)?;
    if look(getpid, [0; 6], OUTSIDE) != Verdict::Continue || take(&thread).is_some() {
        return Err("an ALLOW selector dispatched a call");
    }
    if call(getpid, [0; 6], OUTSIDE)? != pid || take(&thread).is_some() {
        return Err("an ALLOW selector dispatched a call");
    }
    seen.calls += 2;
    put(SYSCALL_DISPATCH_FILTER_BLOCK)?;

    // (j): a native-range number is dispatched, not served and not killed;
    // with ALLOW it is served natively.
    let answer = look(NATIVE_CALL, [0; 6], OUTSIDE);
    if process.is_terminated() {
        return Err("a native-range dispatch killed the process");
    }
    let taken = take(&thread).ok_or("a native-range call was not dispatched")?;
    if answer != Verdict::Trap
        || !matches!(taken.origin, signal::Origin::Sys { syscall, .. } if syscall == NATIVE_CALL as i32)
    {
        return Err("a native-range dispatch did not carry its number");
    }
    put(SYSCALL_DISPATCH_FILTER_ALLOW)?;
    if look(NATIVE_CALL, [0; 6], OUTSIDE) != Verdict::Continue {
        return Err("an allowed native-range call was not served natively");
    }
    let answer = call(NATIVE_CALL, [0; 6], OUTSIDE)?;
    if answer == NATIVE_CALL as isize || take(&thread).is_some() || process.is_terminated() {
        return Err("an allowed native-range call was not served natively");
    }
    seen.calls += 2;

    // The costs, before a filter joins them.
    let mut costs = [0_u64; TIMED];
    for cost in &mut costs {
        let started = crate::timer::now_nanos();
        let _ = call(getpid, [0; 6], START)?;
        *cost = crate::timer::now_nanos().saturating_sub(started);
    }
    seen.in_range = median(&mut costs);
    for cost in &mut costs {
        let started = crate::timer::now_nanos();
        let _ = call(getpid, [0; 6], OUTSIDE)?;
        *cost = crate::timer::now_nanos().saturating_sub(started);
    }
    seen.selector = median(&mut costs);
    put(SYSCALL_DISPATCH_FILTER_BLOCK)?;
    for cost in &mut costs {
        let started = crate::timer::now_nanos();
        let _ = look(getpid, [0; 6], OUTSIDE);
        let _ = take(&thread);
        *cost = crate::timer::now_nanos().saturating_sub(started);
    }
    seen.dispatch = median(&mut costs);
    seen.calls += 3 * TIMED;

    // (c): no selector dispatches every call from outside.
    if set(PR_SYS_DISPATCH_EXCLUSIVE_ON, 0)? != 0 {
        return Err("a dispatch setting with no selector was refused");
    }
    if look(getpid, [0; 6], OUTSIDE) != Verdict::Trap || take(&thread).is_none() {
        return Err("a call with no selector was not dispatched");
    }
    // A selector on a page never touched is brought in by the read: zero,
    // ALLOW.
    if set(PR_SYS_DISPATCH_EXCLUSIVE_ON, fresh)? != 0 {
        return Err("a selector on a fresh page was refused");
    }
    if look(getpid, [0; 6], OUTSIDE) != Verdict::Continue
        || call(getpid, [0; 6], OUTSIDE)? != pid
        || take(&thread).is_some()
    {
        return Err("a selector on a page not yet touched did not read as ALLOW");
    }
    seen.calls += 2;

    // (f): with a filter that fails getppid and kills getsid, dispatch is
    // judged first: a dispatched call meets no filter, and one dispatch lets go
    // on meets it.
    if set(PR_SYS_DISPATCH_EXCLUSIVE_ON, page)? != 0 {
        return Err("a good dispatch setting was refused through the entry");
    }
    put(SYSCALL_DISPATCH_FILTER_BLOCK)?;
    let program = [
        Insn::new(0x20, 0, 0, 0),
        Insn::new(0x15, 0, 1, getppid as u32),
        Insn::new(0x06, 0, 0, ERRNO | 1),
        Insn::new(0x15, 0, 1, getsid as u32),
        Insn::new(0x06, 0, 0, KILL_PROCESS),
        Insn::new(0x06, 0, 0, ALLOW),
    ];
    install_filter(&process, page, &program)?;
    // `PR_SET_NO_NEW_PRIVS` first, then `seccomp(SET_MODE_FILTER)`, both from
    // inside the range.
    if call(prctl, [38, 1, 0, 0, 0, 0], START)? != 0 {
        return Err("PR_SET_NO_NEW_PRIVS was refused");
    }
    if call(seccomp_call, [1, 0, page + 8, 0, 0, 0], START)? != 0 {
        return Err("the dispatch check's filter was refused through the entry");
    }
    let answer = look(getppid, [0; 6], OUTSIDE);
    if answer == Verdict::Errno(1) {
        return Err("a dispatched call met the seccomp filter");
    }
    if answer != Verdict::Trap || take(&thread).is_none() {
        return Err("a dispatched call under a filter was not dispatched");
    }
    if look(getsid, [0; 6], OUTSIDE) != Verdict::Trap || process.is_terminated() {
        return Err("a dispatched call met the seccomp filter");
    }
    let _ = take(&thread);
    if call(getppid, [0; 6], START)? != -1 {
        return Err("a call dispatch let go on was not judged by its filter");
    }
    seen.calls += 3;
    Ok(Then::Report(seen))
}

/// Write `program` as a `struct sock_fprog` at `page + 8` and its
/// instructions at `page + 64`, the selector staying in the page's first
/// byte. x86-64 only, as dispatch is.
fn install_filter(
    process: &Process,
    page: u64,
    program: &[ferrix_seccomp::Insn],
) -> Result<(), &'static str> {
    use alloc::vec::Vec;
    let code_at = page + 64;
    let mut header: Vec<u8> = Vec::new();
    header.extend_from_slice(&(program.len() as u16).to_le_bytes());
    header.resize(8, 0);
    header.extend_from_slice(&code_at.to_le_bytes());
    let mut code: Vec<u8> = Vec::new();
    for insn in program {
        code.extend_from_slice(&insn.code.to_le_bytes());
        code.push(insn.jt);
        code.push(insn.jf);
        code.extend_from_slice(&insn.k.to_le_bytes());
    }
    uaccess::copy_to_user(process.space(), page + 8, &header)
        .and_then(|()| uaccess::copy_to_user(process.space(), code_at, &code))
        .map_err(|_| "the dispatch check could not write its filter")
}
