//! Signals reaching a program: the way back to user mode, handler frames,
//! `rt_sigreturn`, faults, and the calls that wait for a signal.
//!
//! # Where delivery happens
//!
//! On the way back to user mode, and nowhere else, because that is the only
//! place the program's registers are to hand and about to be used. Every way
//! back calls [`needs_attention`] and, when it answers yes, [`return_to_user`]
//! with the registers as an [`arch::UserContext`]: the end of a system call on
//! all three architectures, and the end of every trap -- a tick, an IPI, a
//! fault -- taken from user mode. A process ended from outside leaves there;
//! a stopped one waits there; and a signal with a handler is delivered by
//! rewriting the registers to enter the handler, on a frame written to the
//! program's stack that records the registers it had.
//!
//! # What is the architecture's
//!
//! The frame. Linux fixed a different `rt_sigframe` for each architecture, and
//! a C library's handler trampolines and `ucontext_t` readers depend on every
//! offset, so each architecture writes and reads its own
//! ([`arch::setup_signal_frame`], [`arch::restore_signal_frame`]). What is
//! decided here is everything else: which signal, whether it has a handler,
//! the mask while it runs, the stack it runs on, and what happens when the
//! frame cannot be written.
//!
//! # Interrupted calls, and `SA_RESTART`
//!
//! A call that waits -- `wait4`, `poll`, a pipe, `pause`, `rt_sigsuspend` --
//! also stops waiting when a signal is deliverable. What it returns is not
//! `EINTR` but one of the kernel-internal restart codes ([`Errno::is_restart`]),
//! which never reaches the program: this module turns each into a restart of
//! the interrupted call or into `EINTR`, exactly as Linux's
//! `arch_do_signal_or_restart` does, before the program is resumed.
//!
//! The rule, per code:
//!
//! * `ERESTARTSYS` (a read, write, `wait4`, pipe or futex wait): the call
//!   restarts if a handler with `SA_RESTART` runs, or if no handler runs at
//!   all; otherwise `EINTR`.
//! * `ERESTARTNOHAND` (`poll`, `select`, `pselect6`): `EINTR` if a handler
//!   runs, restart if none does. So a handler -- the usual interrupter --
//!   always sees `EINTR` from these.
//! * `ERESTARTNOINTR`: always restarts.
//! * `ERESTART_RESTARTBLOCK` (`nanosleep`, `clock_nanosleep`): `EINTR` if a
//!   handler runs; otherwise the call is re-entered as `restart_syscall`,
//!   which waits out the time left rather than the whole sleep again.
//!
//! Restarting means rewinding the saved program counter to the call's own
//! instruction and putting back the argument register the return value
//! clobbered, so that resuming re-executes the call. The rewind differs per
//! architecture and lives in each `arch::UserContext`; the decision is here.
//! It is made before any handler frame is built, so the frame saves the
//! resolved resume point and `rt_sigreturn` returns straight into it.

use ferrix_bootinfo::is_user_address;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{SA_RESTART, SA_RESTORER, SIG_DFL, SIG_IGN, SIGSEGV};

use crate::arch;
use crate::console::println;

/// How long a parked thread trusts the wakes that end its park: an hour, which
/// is a net under a missed one and no cost to a frozen cgroup.
const PARKED_RECHECK_NANOS: u64 = 3_600_000_000_000;

use crate::object::process::Host;
use crate::syscall::process::{self, Process};
use crate::syscall::signal::{
    self, DefaultAction, Origin, Posted, Restart, SIGSET_SIZE, Taken, UNBLOCKABLE,
};
use crate::syscall::thread::{self, Thread};
use crate::syscall::time::TimeWidth;
use crate::syscall::uaccess;

use crate::signal_frame::{BadFrame, FrameRequest, StackRecord};

/// How many signals one round of the way back to user mode acts on before it
/// looks again with interrupts masked. One more than there are signals, so
/// every pending one can be taken in one round. Not a bound on the time spent
/// in the kernel: [`return_to_user`] goes round again while there is more to
/// do, as Linux does, so a task sent stops and continues in a tight loop stays
/// in the kernel for as long as they keep coming -- interruptible throughout.
const DELIVERY_ROUNDS: usize = 65;

/// What this personality does on the way back to user mode.
///
/// `crate::trap` owns the trap return and states the interface; this is the
/// Linux personality filling it in. Registered by [`install`] before any user
/// mode runs, so the core never names this module.
static RETURN_PATH: crate::trap::ReturnPath = crate::trap::ReturnPath {
    needs_attention,
    return_to_user,
    sigreturn,
    fault_signal,
};

/// Force `signal` on the running program for a fault, and say what became of
/// it in terms the core owns.
///
/// The core decides a fault is the program's problem; this decides what that
/// means. `Posted` and `Origin` stay on this side of the interface.
fn fault_signal(signal: u32, code: i32, address: u64) -> crate::trap::FaultOutcome {
    use crate::trap::FaultOutcome;

    match force(signal, Origin::Fault { code, address }) {
        None => FaultOutcome::NoProcess,
        Some(Posted::Fatal) => FaultOutcome::Ended {
            pid: process::current().map_or(0, |process| process.pid()),
        },
        Some(Posted::Discarded | Posted::Pending) => FaultOutcome::Delivered,
    }
}

/// Hand the core this personality's return path.
///
/// Called once from `_start`, before the first user program -- which is a boot
/// check, not init, so this cannot wait for `init::run`.
pub(crate) fn install() {
    crate::trap::set_return_path(&RETURN_PATH);
}

/// Whether the way back to user mode has anything to do for the running task:
/// its process ended or stopped, a signal to deliver to its thread, or a mask
/// to put back.
///
/// Cheap, and asked on every way back, so that the registers are only copied
/// out into an [`arch::UserContext`] when something will use them.
pub(crate) fn needs_attention() -> bool {
    thread::current().is_some_and(|thread| {
        let process = thread.process();
        process.must_leave(&thread)
            || process.must_park()
            || thread.with_signals(|shared, own| signal::needs_attention(shared, own))
    })
}

/// Act on everything [`needs_attention`] found, with `context` the registers
/// the program is about to resume with.
///
/// Called with interrupts masked, and returns with them masked. They are open
/// in between: writing a frame can fault in a page of the program's stack,
/// and a stopped process waits here for as long as it stays stopped.
///
/// **Returns only once a look with interrupts masked finds nothing to do**, as
/// Linux's `exit_to_user_mode_loop` does. A stop, a kill or a signal that
/// arrives while they are open finds this task in the kernel, and the
/// interrupt that was to bring it back through here -- the kick
/// [`crate::sched::interrupt`] sends a task running in user mode -- is taken
/// on the spot and spent, since only an interrupt from user mode looks. Left
/// unchecked, the task went back to its program with its process stopped and
/// nothing pending to stop it, and a task alone on its processor gets no tick:
/// FX-0701's "a thread of a stopped process kept running instead of stopping".
/// Every `SIGSTOP` to a process of spinning threads sets it up, because the
/// kick is a broadcast and a signal pending for the process draws every
/// thread in here at once: one takes the stop, and another, having seen no
/// stop and then no signal, was on its way out as the stop's own kick landed.
///
/// Does not return when the process has ended.
pub(crate) fn return_to_user(context: &mut arch::UserContext) {
    let Some(thread) = thread::current() else {
        return;
    };
    let process = thread.process();

    loop {
        arch::enable_interrupts();

        // A blocking call interrupted by a signal returns a restart code in the
        // return register. Resolve it against the signal about to be delivered,
        // the way Linux does on the syscall exit path. `take_restart` answers
        // `Some` only just after such a call, and takes it once so a later trap
        // -- or a later round of this loop -- cannot act on a stale one; the
        // register is checked too, so a way back that is not a syscall return
        // -- a tick, a fault -- is never rewound.
        let mut restart = thread
            .with_own_signals(signal::ThreadSignals::take_restart)
            .zip(RestartKind::of(context.syscall_result()));

        for _ in 0..DELIVERY_ROUNDS {
            if process.must_leave(&thread) {
                break;
            }
            if process.must_park() {
                // Stopped, or frozen: a freeze counts its parked threads,
                // and says when the last has parked (`cgroup.events`).
                let frozen = process.core().is_frozen();
                if frozen {
                    process.thread_parked();
                    crate::fs::cgroupfs::settle_frozen(&process.core().job());
                }
                // Woken by everything that ends a park (a continue, a thaw, an
                // end, an `execve`), so it looks again only now and then: a
                // parked thread that woke every few milliseconds to find
                // itself still parked was charged the time to do it, and a
                // frozen cgroup is to use no processor.
                let _ = crate::sched::WaitQueue::wait_on_any(
                    &[process.resumed()],
                    || !process.must_park() || process.must_leave(&thread),
                    u64::MAX,
                    PARKED_RECHECK_NANOS,
                );
                if frozen {
                    process.thread_unparked();
                }
                continue;
            }
            let Some(taken) = thread.with_signals(signal::take_next) else {
                break;
            };
            // The first signal that runs a handler settles the restart: only a
            // handler can turn one into `EINTR`. A default action -- a stop, an
            // ignore -- leaves it pending, so a stop then continue restarts
            // transparently and a later handler still gets to decide.
            if let Some((ctx, kind)) = restart
                && runs_a_handler(&taken)
            {
                resolve_restart(context, &ctx, kind, taken.action.flags);
                restart = None;
            }
            act(&thread, context, &taken);
        }
        // No handler ran -- a stop, an ignore, or nothing was left to deliver --
        // so the call restarts transparently.
        if let Some((ctx, kind)) = restart {
            restart_call(context, &ctx, kind);
        }
        restore_saved_mask(&thread);
        if process.must_leave(&thread) {
            // Left with interrupts still open, as the release its exit may run
            // needs. Nothing after the thread's exit runs to drop it.
            drop(thread);
            process::leave_current();
        }
        arch::disable_interrupts();
        if !needs_attention() {
            return;
        }
    }
}

/// Which restart code a system call left in the return register, if any. The
/// kernel-internal codes are the only errors above what a program can see, so
/// a value outside them -- an ordinary result, or the live register of a way
/// back that is not a syscall return -- is `None` and starts no restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartKind {
    /// `ERESTARTSYS`: restart under `SA_RESTART`, or with no handler.
    Sys,
    /// `ERESTARTNOINTR`: always restart.
    NoIntr,
    /// `ERESTARTNOHAND`: restart only with no handler.
    NoHand,
    /// `ERESTART_RESTARTBLOCK`: resume through `restart_syscall`.
    RestartBlock,
}

impl RestartKind {
    /// The code a return value of `value` carries, if it is a restart code.
    fn of(value: isize) -> Option<RestartKind> {
        if value == Errno::ERESTARTSYS.as_return_value() {
            Some(RestartKind::Sys)
        } else if value == Errno::ERESTARTNOINTR.as_return_value() {
            Some(RestartKind::NoIntr)
        } else if value == Errno::ERESTARTNOHAND.as_return_value() {
            Some(RestartKind::NoHand)
        } else if value == Errno::ERESTART_RESTARTBLOCK.as_return_value() {
            Some(RestartKind::RestartBlock)
        } else {
            None
        }
    }
}

/// Whether `taken` runs a handler of the program's own, as opposed to a
/// default action or being ignored -- the only case that can turn a restart
/// into `EINTR`.
fn runs_a_handler(taken: &Taken) -> bool {
    !matches!(taken.action.handler, SIG_DFL | SIG_IGN)
}

/// Settle a restart against a handler that is about to run: restart the call
/// if the code and the handler's `SA_RESTART` flag allow it, and otherwise
/// leave `EINTR` in the return register, at the call's own resume point.
fn resolve_restart(context: &mut arch::UserContext, ctx: &Restart, kind: RestartKind, flags: u64) {
    if restarts(kind, true, flags) {
        restart_call(context, ctx, kind);
    } else {
        context.set_syscall_result(Errno::EINTR.as_return_value());
    }
}

/// Whether a call with restart code `kind` restarts, given whether a handler
/// runs and its `SA_*` flags. Linux's rule, in one place so the boot self-check
/// can assert the whole truth table:
///
/// * with no handler, every code restarts (the call had nothing to report to);
/// * `ERESTARTNOINTR` always restarts;
/// * `ERESTARTSYS` restarts only under `SA_RESTART`;
/// * `ERESTARTNOHAND` and `ERESTART_RESTARTBLOCK` never restart through a
///   handler -- they become `EINTR`.
fn restarts(kind: RestartKind, runs_handler: bool, flags: u64) -> bool {
    if !runs_handler {
        return true;
    }
    match kind {
        RestartKind::NoIntr => true,
        RestartKind::Sys => flags & SA_RESTART != 0,
        RestartKind::NoHand | RestartKind::RestartBlock => false,
    }
}

/// Assert the restart decision matches Linux's for every code, with and without
/// a handler and its `SA_RESTART` flag -- the boot self-check for `SA_RESTART`,
/// each row its own negative control. It proves the decision this module makes;
/// the per-architecture rewind that carries it out is in each `UserContext` and
/// cross-checked against `arch/*/kernel/signal.c` and QEMU's `cpu_loop`.
pub(crate) fn check_restart_decisions() -> Result<(), &'static str> {
    // Only the kernel-internal codes classify; an ordinary result or `EINTR`
    // must not, or a live register at a tick could be mistaken for one.
    if RestartKind::of(0).is_some()
        || RestartKind::of(Errno::EINTR.as_return_value()).is_some()
        || RestartKind::of(Errno::EFAULT.as_return_value()).is_some()
    {
        return Err("an ordinary return value was read as a restart code");
    }
    for (value, kind) in [
        (Errno::ERESTARTSYS.as_return_value(), RestartKind::Sys),
        (Errno::ERESTARTNOINTR.as_return_value(), RestartKind::NoIntr),
        (Errno::ERESTARTNOHAND.as_return_value(), RestartKind::NoHand),
        (
            Errno::ERESTART_RESTARTBLOCK.as_return_value(),
            RestartKind::RestartBlock,
        ),
    ] {
        if RestartKind::of(value) != Some(kind) {
            return Err("a restart code did not classify as itself");
        }
    }
    // (kind, a handler runs, its flags, whether the call should restart).
    let matrix = [
        (RestartKind::Sys, true, SA_RESTART, true),
        (RestartKind::Sys, true, 0, false),
        (RestartKind::NoIntr, true, 0, true),
        (RestartKind::NoHand, true, SA_RESTART, false),
        (RestartKind::RestartBlock, true, SA_RESTART, false),
        (RestartKind::Sys, false, 0, true),
        (RestartKind::NoHand, false, 0, true),
        (RestartKind::RestartBlock, false, 0, true),
        (RestartKind::NoIntr, false, 0, true),
    ];
    for (kind, handler, flags, expect) in matrix {
        if restarts(kind, handler, flags) != expect {
            return Err("the restart decision does not match Linux's for some case");
        }
    }
    Ok(())
}

/// Rewind the saved registers so the interrupted call re-executes: back to its
/// own instruction, with the clobbered argument register restored. A
/// `restart_block` call is re-entered as `restart_syscall` instead of itself,
/// so it waits out only the time left.
fn restart_call(context: &mut arch::UserContext, ctx: &Restart, kind: RestartKind) {
    let restart_block = matches!(kind, RestartKind::RestartBlock);
    context.rewind_syscall(ctx.nr, ctx.arg0, restart_block);
}

/// Do what `taken` asks of `thread`: nothing, the default action, or its
/// handler.
fn act(thread: &Thread, context: &mut arch::UserContext, taken: &Taken) {
    let process = thread.process();
    match taken.action.handler {
        SIG_IGN => {}
        SIG_DFL => match signal::default_action(taken.signal) {
            DefaultAction::Terminate | DefaultAction::Core => {
                process::kill(process, 128 + taken.signal as i32);
            }
            DefaultAction::Stop => process.enter_stop(taken.signal),
            DefaultAction::Ignore | DefaultAction::Continue => {}
        },
        _ => {
            if run_handler(thread, context, taken).is_err() {
                // Linux's `force_sigsegv`: a handler that cannot be entered
                // is a program that cannot go on.
                println!(
                    "  signal   pid {} has no room for signal {}'s frame; ending it with SIGSEGV",
                    process.pid(),
                    taken.signal
                );
                process::kill(process, 128 + SIGSEGV as i32);
            }
        }
    }
}

/// Enter `taken`'s handler: choose the stack, change the mask, and have the
/// architecture write the frame and point the registers at the handler.
fn run_handler(
    thread: &Thread,
    context: &mut arch::UserContext,
    taken: &Taken,
) -> Result<(), BadFrame> {
    if !is_user_address(taken.action.handler) {
        return Err(BadFrame);
    }
    let sp = context.stack_pointer();
    let (mask, altstack, stack) = enter_handler_for(thread, taken, sp);
    // A handler with no restorer of its own goes back through the vDSO's
    // trampoline where the architecture's has one, as Linux's AArch64 does:
    // glibc there sets no `SA_RESTORER`, having no trampoline of its own.
    let (flags, restorer) = match super::vdso::sigreturn(thread.process().space()) {
        Some(trampoline) if taken.action.flags & SA_RESTORER == 0 => {
            (taken.action.flags | SA_RESTORER, trampoline)
        }
        _ => (taken.action.flags, taken.action.restorer),
    };
    let request = FrameRequest {
        signal: taken.signal,
        info: taken.origin.encode(taken.signal),
        handler: taken.action.handler,
        flags,
        restorer,
        mask,
        stack,
        altstack,
        sigpage: super::sigpage::address(thread.process().space(), 0),
    };
    arch::setup_signal_frame(thread.process().space(), context, &request)
}

/// Change `thread`'s signal state for entering `taken`'s handler from a stack
/// pointer of `sp`: answer the mask and alternate stack the frame saves and
/// where the frame goes, and hand on to another thread what the handler's mask
/// newly blocks while it is pending for the process. What `run_handler` does
/// before it writes the frame, apart so that a boot check can drive it.
pub(crate) fn enter_handler_for(
    thread: &Thread,
    taken: &Taken,
    sp: u64,
) -> (u64, StackRecord, u64) {
    signal::change_blocked(thread, |shared, own| {
        let stack = own.signals().frame_base(taken.action.flags, sp);
        let (mask, altstack) = signal::enter_handler(shared, own, taken);
        (mask, altstack, stack)
    })
}

/// Put back the mask `rt_sigsuspend`, `ppoll` or `pselect6` replaced, if no
/// handler's frame took it first: on the way back to user mode, through
/// [`signal::change_blocked`], so that a signal the mask blocks again while it
/// is pending for the process is handed on. Apart from `return_to_user` so
/// that a boot check can drive it.
pub(crate) fn restore_saved_mask(thread: &Thread) {
    signal::change_blocked(thread, |_, own| own.restore_saved_mask());
}

/// Leave a handler as `rt_sigreturn` does: put back the mask and alternate
/// stack its frame saved, through [`signal::change_blocked`], so that a signal
/// the restored mask blocks while it is pending for the process is handed on.
/// A frame that saved no alternate stack leaves it as it is. Apart from
/// `sigreturn` so that a boot check can drive it.
pub(crate) fn leave_handler(thread: &Thread, mask: u64, altstack: Option<StackRecord>, sp: u64) {
    signal::change_blocked(thread, |_, own| own.leave_handler(mask, altstack, sp));
}

/// `rt_sigreturn`, and ARMv7-A's `sigreturn` when `rt` is false: put back the
/// registers, mask and alternate stack a handler's frame saved. The frame is
/// found from the stack pointer the handler returned with, which is all a
/// restorer leaves.
///
/// A frame that cannot be read, or does not hold together, ends the process
/// with `SIGSEGV`, as on Linux: there is no context left to return an error to.
pub(crate) fn sigreturn(context: &mut arch::UserContext, rt: bool) {
    let Some(thread) = thread::current() else {
        return;
    };
    let process = thread.process();
    match arch::restore_signal_frame(process.space(), context, rt) {
        Ok(restored) => {
            let sp = context.stack_pointer();
            leave_handler(&thread, restored.mask, restored.altstack, sp);
        }
        Err(BadFrame) => {
            println!(
                "  signal   pid {} returned from a handler through a bad frame; ending it with SIGSEGV",
                process.pid()
            );
            process::kill(process, 128 + SIGSEGV as i32);
        }
    }
}

/// Raise `signal` against the running task's thread for a fault its own
/// instruction took, so that it can neither block nor ignore it. Answers what
/// became of it -- [`Posted::Fatal`] when the process has already been ended --
/// or `None` when the running task has no thread, which a fault from user
/// mode never lacks unless the kernel entered user mode without one.
pub(crate) fn force(signal: u32, origin: Origin) -> Option<Posted> {
    let thread = thread::current()?;
    let posted = thread.with_signals(|shared, own| signal::force(shared, own, signal, origin));
    if posted == Posted::Fatal {
        process::kill(thread.process(), 128 + signal as i32);
    }
    Some(posted)
}

/// Wait until a signal `thread` does not block is pending, its process ends,
/// `also` holds, or `deadline` passes.
fn wait_for_signal(thread: &Thread, deadline: u64, mut also: impl FnMut() -> bool) {
    let _ = thread
        .process()
        .signalled()
        .wait_until_deadline(|| thread.signal_pending() || also(), deadline);
}

/// `rt_sigsuspend`: block `mask` instead, wait for a signal, and return
/// `EINTR`. The old mask comes back on the way to user mode -- after a
/// handler's frame has saved it, so the handler runs under `mask` and the
/// program resumes under its own.
///
/// Answers `ERESTARTNOHAND`, as Linux does: `EINTR` when a handler runs, and
/// the call made again when none does -- a stop and a continue, which end
/// every wait -- so the program is still suspended afterwards.
///
/// # Errors
///
/// `EINVAL` for a set size other than eight; `EFAULT` for a bad set; `EINTR`,
/// always, once a handler has run.
pub(crate) fn sys_rt_sigsuspend(
    thread: &Thread,
    mask: u64,
    sigsetsize: u64,
) -> Result<usize, Errno> {
    if sigsetsize != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let mask = read_sigset(thread.process(), mask)?;
    signal::change_blocked(thread, |_, own| own.suspend_with(mask));
    wait_for_signal(thread, u64::MAX, || false);
    Err(Errno::ERESTARTNOHAND)
}

/// `pause`: wait for a signal, and return `EINTR`.
///
/// Answers `ERESTARTNOHAND`, as [`sys_rt_sigsuspend`] does, so a stop and a
/// continue leave the program still paused.
///
/// # Errors
///
/// `EINTR`, always, once a handler has run.
pub(crate) fn sys_pause(process: &Process) -> Result<usize, Errno> {
    let _ = process
        .signalled()
        .wait_until_deadline(|| process.signal_pending(), u64::MAX);
    Err(Errno::ERESTARTNOHAND)
}

/// `rt_sigpending`: the pending signals the calling thread blocks, its own and
/// its process's. Linux copies `sigsetsize` bytes of the set and refuses only a
/// size larger than its own.
///
/// # Errors
///
/// `EINVAL` for a size above eight; `EFAULT` for a bad pointer.
pub(crate) fn sys_rt_sigpending(thread: &Thread, at: u64, sigsetsize: u64) -> Result<usize, Errno> {
    if sigsetsize > SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let process = thread.process();
    let set = thread.with_signals(|shared, own| (shared.pending() | own.pending()) & own.blocked());
    let bytes = set.to_le_bytes();
    let len = usize::try_from(sigsetsize).map_err(|_| Errno::EINVAL)?;
    if len > 0 {
        uaccess::copy_to_user(process.space(), at, bytes.get(..len).ok_or(Errno::EINVAL)?)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `rt_sigtimedwait` and `rt_sigtimedwait_time64`: take a pending signal in
/// `set` -- blocked, as a program waiting this way has made it -- without
/// delivering it, and answer its number, with its `siginfo` written to `info`.
///
/// # Errors
///
/// `EINVAL` for a set size other than eight or a bad timeout; `EFAULT` for a
/// bad pointer; `EAGAIN` when the timeout passes first; `EINTR` when another
/// signal, one the caller does not block, arrives first.
pub(crate) fn sys_rt_sigtimedwait(
    thread: &Thread,
    set: u64,
    info: u64,
    timeout: u64,
    sigsetsize: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    if sigsetsize != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let process = thread.process();
    let set = read_sigset(process, set)? & !UNBLOCKABLE;
    let deadline = if timeout == 0 {
        u64::MAX
    } else {
        crate::timer::now_nanos().saturating_add(read_timespec(process, timeout, width)?)
    };
    wait_for_signal(thread, deadline, || {
        thread.with_signals(|shared, own| (shared.pending() | own.pending()) & set != 0)
    });
    match thread.with_signals(|shared, own| signal::take_from(shared, own, set)) {
        Some(taken) => {
            if info != 0 {
                uaccess::copy_to_user(process.space(), info, &taken.origin.encode(taken.signal))
                    .map_err(|_| Errno::EFAULT)?;
            }
            Ok(taken.signal as usize)
        }
        None if thread.signal_pending() => Err(Errno::EINTR),
        None => Err(Errno::EAGAIN),
    }
}

/// Read an 8-byte signal set from the program.
fn read_sigset(process: &Process, at: u64) -> Result<u64, Errno> {
    let mut bytes = [0_u8; 8];
    uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Read a `struct timespec` of `width` as nanoseconds, by `ppoll`'s rules.
fn read_timespec(process: &Process, at: u64, width: TimeWidth) -> Result<u64, Errno> {
    crate::syscall::poll::read_timespec(process, at, width)
}
