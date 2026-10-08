//! What happens when the CPU stops running the program and enters the kernel.
//!
//! The architectures disagree about almost everything here — x86-64 has 256
//! vectors and an error code that exists for ten of them; AArch64 has sixteen
//! vector-table entries and a syndrome register; ARMv7-A has eight entries, five
//! processor modes and a fault status register per kind of abort — so each
//! `arch` module classifies its own frame into the [`Trap`] below, and the
//! policy is written once.

use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use ferrix_linux_abi::errno::Errno;
use ferrix_sync::Once;

use crate::arch;
use crate::console::println;

pub(crate) mod check;

/// Why the kernel was entered, in terms every architecture shares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Trap {
    /// A translation fault: the address was not mapped, or was mapped without
    /// the permission the access needed.
    PageFault(PageFault),
    /// A debugger breakpoint. `int3` on x86-64, `brk` on AArch64.
    Breakpoint,
    /// An instruction the CPU refused to execute.
    IllegalInstruction,
    /// A device interrupt, by controller-assigned number.
    Interrupt(u32),
    /// A deliberate entry from user mode. Stage 7 gives this a body.
    SystemCall,
    /// Anything else, which at this stage means the kernel has a bug.
    Fault {
        /// The architecture's name for it.
        name: &'static str,
        /// The architecture's own code: a vector on x86-64, a syndrome on
        /// `AArch64`.
        code: u64,
    },
}

/// The details of a translation fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PageFault {
    /// The address the access was to.
    pub(crate) address: u64,
    /// True if the access was a write.
    pub(crate) write: bool,
    /// True if it was an instruction fetch.
    pub(crate) execute: bool,
    /// True if the fault came from user mode.
    pub(crate) user: bool,
    /// True if a mapping existed but denied the access; false if there was no
    /// mapping at all. The difference decides whether this is a protection
    /// violation or a page that has not been faulted in yet.
    pub(crate) present: bool,
}

/// Breakpoints taken since boot.
///
/// Counted rather than merely handled, so the boot self-check can prove the
/// whole path ran — entry, save, dispatch, restore, return — rather than only
/// that it did not crash.
static BREAKPOINTS: AtomicU64 = AtomicU64::new(0);

/// Page faults the kernel resolved by mapping a page.
static FAULTS_HANDLED: AtomicU64 = AtomicU64::new(0);

/// Interrupts that arrived while a program was running in user mode.
///
/// Counted so a check can tell a program preempted in user mode from one that
/// was only ever switched away from inside a system call: both show up as a
/// task switched to more than once, and only the first needs interrupts open
/// in user mode.
static USER_INTERRUPTS: AtomicU64 = AtomicU64::new(0);

/// How many breakpoints have been taken.
pub(crate) fn breakpoint_count() -> u64 {
    BREAKPOINTS.load(Ordering::Relaxed)
}

/// How many page faults have been resolved.
pub(crate) fn handled_fault_count() -> u64 {
    FAULTS_HANDLED.load(Ordering::Relaxed)
}

/// How many interrupts have arrived while a program was in user mode.
pub(crate) fn user_interrupt_count() -> u64 {
    USER_INTERRUPTS.load(Ordering::Relaxed)
}

/// Where every trap arrives, from any architecture's entry stub.
pub(crate) fn dispatch(frame: &mut arch::TrapFrame) {
    let trap = arch::classify(frame);
    match trap {
        // A program's own breakpoint, undefined instruction or other fault is
        // the program's problem, not the kernel's: a signal, as on Linux.
        Trap::Breakpoint | Trap::IllegalInstruction | Trap::Fault { .. }
            if frame.came_from_user() =>
        {
            user_fault(frame, &trap);
        }
        Trap::Breakpoint => {
            // The architectures disagree about where the saved instruction
            // pointer lands: x86-64's `int3` pushes the address after itself,
            // AArch64's `brk` and ARMv7-A's `bkpt` the address *of* themselves.
            // Returning without asking would loop forever on two of them.
            arch::advance_past_breakpoint(frame);
            let _ = BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
        }
        Trap::PageFault(fault) => handle_page_fault(frame, fault),
        // The controller, not the CPU, knows which interrupt arrived and how
        // it is acknowledged, and the architectures disagree about both.
        // So the architecture claims, dispatches and retires; what crosses
        // back into generic code is a number.
        Trap::Interrupt(_) => {
            if frame.came_from_user() {
                let _ = USER_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
            }
            arch::service_interrupts(frame, crate::irq::dispatch);
            // And only now, with the controller told this interrupt is done,
            // may the processor go and run something else. Switching inside
            // the handler would leave an interrupt in service for as long as
            // the next task ran, and a controller still servicing one delivers
            // nothing further.
            crate::sched::preempt_on_irq_exit(frame.came_from_user());
        }
        // On x86-64 a system call never arrives here: `SYSCALL` has an entry of
        // its own. On both Arm architectures `svc` is an exception like any
        // other and this is the only way in, so the architecture decides what
        // the registers mean.
        Trap::SystemCall => {
            if let Err(why) = arch::system_call(frame) {
                fatal(frame, why, &crate::panic::catalog::SYSTEM_CALL_TRAP);
            }
        }
        Trap::IllegalInstruction => fatal(
            frame,
            "illegal instruction",
            &crate::panic::catalog::ILLEGAL_INSTRUCTION,
        ),
        Trap::Fault { name, .. } => {
            fatal(frame, name, &crate::panic::catalog::UNEXPECTED_EXCEPTION)
        }
    }

    // A process moved to another job runs and is charged there from its
    // next way back to user mode (`object::quota`): one word read when
    // nothing has moved.
    if frame.came_from_user() {
        crate::sched::regroup_current();
        // And waits out the rest of a `cpu.max` period its job has used up,
        // unless there is a kill or a signal to deal with first.
        crate::sched::throttle_current(must_attend);
    }

    // On the way back to a program, which is where a program killed from
    // outside finds out and a signal is delivered: a task spinning in user
    // mode reaches here on its next tick, and one that was preempted reaches
    // here when it is resumed. See `crate::syscall::deliver`.
    if let Some(path) = return_path().filter(|_| frame.came_from_user())
        && attention_due(path)
    {
        let mut context = arch::UserContext::from_trap(frame);
        (path.return_to_user)(&mut context);
        context.store_trap(frame);
    }
}

/// A system call as it arrived, before anything has been decided about it.
///
/// Deliberately dumb. The number is raw — this architecture's, not folded onto
/// a personality's table yet — and the arguments are in the order the
/// architecture's calling convention puts them, because the only code that can
/// put them in that order is the code that read the registers. Public fields,
/// no constructor and nothing fallible in it: a trampoline that has already
/// switched stacks must not meet a `Result`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SyscallArgs {
    /// Which table [`SyscallArgs::number`] is in: the entry's, not the
    /// process's, as Linux decides it (`in_compat_syscall()`).
    pub(crate) abi: Abi,
    /// The number the program passed, in [`SyscallArgs::abi`]'s table.
    pub(crate) number: usize,
    /// The six argument registers, in order. A call taking fewer leaves the
    /// rest as whatever the program happened to have in them, which is why no
    /// handler may read past its own arity.
    pub(crate) args: [u64; 6],
    /// The instruction after the one that made the call: what the entry's
    /// saved program counter holds. Read by [`filter_system_call`] alone
    /// (`seccomp_data.instruction_pointer`, and a `SIGSYS`'s `si_call_addr`);
    /// zero from a kernel caller, which has no instruction.
    pub(crate) ip: u64,
}

/// The ABI a system call was made in, or a program is entered in.
///
/// Decided by the way in, never by asking the process: on x86-64 a
/// `SYSCALL` from 64-bit code is an x86-64 call and `int $0x80` is an i386
/// call, whatever image the process was started from (`docs/I386.md` §3.2).
/// Every other entry on every architecture is [`Abi::Native`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Abi {
    /// The architecture's own: x86-64's, AArch64's, or ARMv7-A's EABI.
    Native,
    /// A 32-bit program's on a 64-bit kernel: i386's, on x86-64. The kernel's
    /// words are wider than the program's here, so every width the program
    /// sees is chosen for it.
    Compat,
}

/// What the trap path should do when a call returns.
///
/// Two variants rather than a bare `isize` because "put this in the return
/// register" does not describe every call. `execve` and a freshly created
/// `clone` child both resume on a register frame that was *constructed*
/// rather than returned into, so there is nothing to return. Saying that as
/// data — an entry point and a stack pointer — keeps everything above the
/// trap path free of any architecture's `TrapFrame`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Write this into the return register and resume the program.
    ///
    /// Already encoded as Linux encodes it: a value in `-4095..=-1` is
    /// `-errno`, anything else is success.
    Return(isize),
    /// [`Outcome::Return`], and three more words for the second, third and
    /// fourth argument registers: a message `channel_write_read` hands back
    /// in registers rather than through the program's memory. A call made
    /// through an entry that has no such registers to give back answers the
    /// value alone.
    ReturnWords {
        /// What goes in the return register, encoded as for `Return`.
        value: isize,
        /// The message's words, in register order.
        words: [u64; 3],
    },
    /// Discard the saved registers and begin executing at `entry` with `stack`.
    ///
    /// `execve`, and the child side of `clone`. Data rather than "the frame has
    /// been replaced", so that nothing above this names a `TrapFrame`.
    Enter {
        /// Where the program's first instruction is.
        entry: u64,
        /// The stack pointer it starts with, already 16-byte aligned.
        stack: u64,
        /// The mode it runs in: a 32-bit image on x86-64 is entered in
        /// compatibility mode, whatever mode the `execve` came from.
        abi: Abi,
    },
}

/// What answers a system call: `regs` is the caller's saved user registers,
/// which a fork child resumes from, and `None` from a kernel caller.
pub(crate) type SyscallEntry = fn(&SyscallArgs, Option<&arch::UserRegs>) -> Outcome;

/// The registered answer to a system call, set once at bring-up.
///
/// The trap path owns the way in; which calls exist and what they do is the
/// item's dispatcher above it (`crate::syscall::dispatch`), which the core may
/// not name (`docs/certification/FINDINGS.md`, F-09). So the core states the
/// shape of the call and `main.rs` registers the dispatcher into it, as the
/// personality registers the [`ReturnPath`]. A [`Once`] rather than a lock:
/// this is read on every system call, and the read is one acquiring load.
static SYSCALL_ENTRY: Once<SyscallEntry> = Once::new();

/// Answer system calls with `entry` from now on. The first registration
/// stands; `main.rs` makes it before anything can enter user mode.
pub(crate) fn set_syscall_entry(entry: SyscallEntry) {
    let _ = SYSCALL_ENTRY.call_once(|| entry);
}

/// What a registered filter decided about a call before anything answered it.
///
/// Deliberately so small that no variant can make a call do more than it
/// could: the call goes on, or it fails with an errno. There is no variant that
/// carries an [`Outcome`], so a filter cannot start a program, choose the
/// registers one starts with or return anything the dispatcher could not. A
/// filter that must end the thread or the process does so itself, through the
/// paths `exit` and a fatal signal take, and answers [`Verdict::Errno`] for the
/// call it is in the middle of. `docs/SECCOMP.md` §3.3, F-09's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Nothing to add: go on as a kernel with no filter would.
    Continue,
    /// The call does not run, and fails with this errno. The core clamps it to
    /// [`MAX_ERRNO`] and writes `-errno` into the return register: a filter
    /// cannot return a value that is not an errno, nor one that is a restart
    /// code the dispatcher keeps for itself (`ERESTARTSYS` is 512).
    Errno(u32),
}

/// The largest errno a call returns: Linux's `MAX_ERRNO`.
pub(crate) const MAX_ERRNO: u32 = 4095;

/// A registered look at a call before anything answers it, given the call as
/// the entry read it. Runs with interrupts masked, as the entry holds them,
/// and returns with them masked. Takes no sleeping lock and allocates nothing
/// on the path every call of every program takes. A body that needs more than a
/// bounded, short time -- a filter's program is up to 32,768 steps -- opens
/// them itself for exactly that stretch and closes them again before it
/// returns, as the dispatcher does around the call it serves.
pub(crate) type SyscallFilter = fn(&SyscallArgs) -> Verdict;

/// The registered filter, set once at bring-up, beside [`SYSCALL_ENTRY`] and
/// for the same reason: which calls a program may make is the personality's
/// policy and the core may not name it. A [`Once`], read with one acquiring
/// load on every call.
static SYSCALL_FILTER: Once<SyscallFilter> = Once::new();

/// Judge every system call with `filter` from now on, a filter whose quiet
/// predicate is `main.rs`'s `ferrix_filter_quiet`, registered in one call so
/// that the two cannot be registered apart: [`QUIET_REGISTERED`] is raised only by the call that installed
/// `filter`, after it did. A filter already registered by anyone else stands,
/// and leaves the flag down, so its calls are never quiet. `main.rs` makes it
/// before anything can enter user mode.
pub(crate) fn set_quiet_syscall_filter(filter: SyscallFilter) {
    let mut installed = false;
    let _ = SYSCALL_FILTER.call_once(|| {
        installed = true;
        filter
    });
    if installed {
        QUIET_REGISTERED.store(true, Ordering::Release);
    }
}

/// Ask the registered filter about one call: what every architecture's entry
/// calls first, with the registers it read, before its own early answers
/// (`arch_prctl`, `set_tls`, the signal returns) and before
/// [`system_call`], so that no call a program can make escapes the look.
/// `None` is "go on"; `Some` is the answer, which is always a failure with an
/// errno.
///
/// With nothing registered every call goes on, and the core alone behaves as
/// it did before the registration existed.
pub(crate) fn filter_system_call(args: &SyscallArgs) -> Option<Outcome> {
    ask(&SYSCALL_FILTER, args)
}

/// [`set_quiet_syscall_filter`]'s registration for any slot: the first
/// registration stands.
pub(crate) fn register(slot: &Once<SyscallFilter>, filter: SyscallFilter) {
    let _ = slot.call_once(|| filter);
}

/// [`filter_system_call`] for any slot: what the core makes of the filter's
/// verdict, and the only place it is made. An errno is clamped here, in the
/// core, so that no filter can write a value outside `-1..=-4095` or a positive
/// one into a program's return register.
pub(crate) fn ask(slot: &Once<SyscallFilter>, args: &SyscallArgs) -> Option<Outcome> {
    let filter = slot.get()?;
    match filter(args) {
        Verdict::Continue => None,
        Verdict::Errno(errno) => Some(Outcome::Return(-(errno.min(MAX_ERRNO) as isize))),
    }
}

// The two calls the core makes up into the rings above it on the fast
// path, bound when the kernel is linked rather than through a registered
// pointer: an indirect call costs about 3.5 ns more than a direct one here
// (`docs/OPAQUE-KERNEL.md` §9.7, "as built", 2 and 3), and each direction of
// a fast trip made three. The core names neither ring: it declares the two
// symbols, and the composition root, `main.rs`, which names every ring
// already, defines each as a forward to the function above it
// (`crate::hooks`, *Link-time hooks*; check-item-boundary holds both ends).
// Whether either is called is still decided at run time, at every call, by
// the core's own flags, which keep the registrations' meaning:
// [`FAST_PATH_ON`] for T1, and [`QUIET_REGISTERED`] for a filter registered
// with its predicate.
//
// SAFETY: (LINK) each declaration's type is held to its definition's by
// the `const _` assertions below and in `main.rs`, which tie both to one
// alias; Rust checks neither side against the other otherwise.
unsafe extern "Rust" {
    /// `syscall::native::fast_write_read`, defined in `main.rs`.
    safe fn ferrix_fast_write_read(a: &[u64; 6]) -> Fast;
    /// `syscall::seccomp::quiet`, defined in `main.rs`.
    safe fn ferrix_filter_quiet() -> bool;
}

/// The personality's quiet predicate, as `main.rs` defines it: whether the
/// registered filter would let every call of the running task through
/// without looking at it.
pub(crate) type FilterQuiet = fn() -> bool;

// The declarations are the aliases' type, as `main.rs`'s definitions are.
const _: FastWriteRead = ferrix_fast_write_read;
const _: FilterQuiet = ferrix_filter_quiet;

/// Whether the personality said, beside its filter, when that filter lets
/// calls through unlooked at: what the fast path tests first (T2,
/// `docs/OPAQUE-KERNEL.md` §9.7), since it runs before
/// [`filter_system_call`]. A filter registered without saying so is never
/// quiet, and with no filter registered every call is.
static QUIET_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Whether the registered filter would let the running task's calls through
/// without looking: see [`QUIET_REGISTERED`]. Asked afresh at every call,
/// with interrupts masked, as the entry holds them.
///
/// The filter is read before the flag, and the flag is raised only after the
/// filter is in place: a reader sees no filter (quiet), the filter without
/// the flag (never quiet), or both (the predicate's answer), and never the
/// flag beside a filter whose predicate was not registered with it.
pub(crate) fn filter_quiet() -> bool {
    match (
        SYSCALL_FILTER.get(),
        QUIET_REGISTERED.load(Ordering::Acquire),
    ) {
        (None, _) => true,
        (Some(_), true) => ferrix_filter_quiet(),
        (Some(_), false) => false,
    }
}

/// What step 4's fast path made of a `channel_write_read`
/// (`docs/OPAQUE-KERNEL.md` §9.7, part 2).
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64's SYSCALL entry takes the fast path")
)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fast {
    /// A test failed, and nothing changed: the entry goes on as for any
    /// call, from the filter.
    Declined,
    /// Answered with a reply another commit handed over: the frame tail
    /// writes it and leaves by the stub's own exit, or by the general way out
    /// when work is due.
    Tail(Outcome),
    /// Answered by the general continuation, which has run the call's way
    /// back as `system_call` does (`regroup_current`, `call_left`): the entry
    /// writes it and makes its way out.
    Done(Outcome),
}

/// The fast path, given the call's six argument registers. Entered from
/// x86-64's `SYSCALL` entry alone, with interrupts masked, before the
/// filter; returns with them masked.
pub(crate) type FastWriteRead = fn(&[u64; 6]) -> Fast;

/// Whether this boot takes `channel_write_read` through the fast path: set
/// at boot only when `ferrix.fastpath=on` (`fastpath.rs`), so that a clear
/// flag is the switch off (T1). Read at every entry.
static FAST_PATH_ON: AtomicBool = AtomicBool::new(false);

/// Take `channel_write_read` through the fast path from now on. Set once,
/// before the first program, and only on a boot that asks for it.
pub(crate) fn set_fast_write_read() {
    FAST_PATH_ON.store(true, Ordering::Release);
}

/// The fast path, if this boot takes it: `main.rs`'s
/// `ferrix_fast_write_read`, called directly from the entry
/// ([`fast_path_on`], [`fast_write_read_now`]) and named here for the
/// checks that ask whether it is on.
pub(crate) fn fast_write_read() -> Option<FastWriteRead> {
    fast_path_on().then_some(fast_write_read_now as FastWriteRead)
}

/// Whether this boot takes the fast path (T1).
pub(crate) fn fast_path_on() -> bool {
    FAST_PATH_ON.load(Ordering::Acquire)
}

/// The fast path itself, by a direct call: only once [`fast_path_on`] has
/// said yes.
pub(crate) fn fast_write_read_now(a: &[u64; 6]) -> Fast {
    ferrix_fast_write_read(a)
}

/// Answer one system call: what every architecture's system call path calls,
/// with the registers it read.
///
/// With nothing registered every call is `ENOSYS`, which is what a kernel
/// with no dispatcher above its core can honestly say, and never a panic: the
/// caller is a trap vector with a program waiting on it.
pub(crate) fn system_call(args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome {
    // A program's call, marked so that a wake it makes leaves the decision to
    // its way out rather than to the timer (`sched::resched_here`).
    if regs.is_some() {
        crate::sched::call_entered();
    }
    let outcome = match SYSCALL_ENTRY.get() {
        Some(entry) => entry(args, regs),
        None => Outcome::Return(Errno::ENOSYS.as_return_value()),
    };
    // As on the way back from a trap: a call that moved its own process
    // (`echo $$ > cgroup.procs`) runs in the new job from here.
    if regs.is_some() {
        crate::sched::regroup_current();
        crate::sched::throttle_current(must_attend);
        crate::sched::call_left();
    }
    outcome
}

/// What the way back to user mode does before the program runs again.
///
/// The core owns the trap return; what happens on it -- a signal delivered, a
/// process that ended finding out, a stop waited on -- is the personality's,
/// and the personality is not part of the certified item. So the core states
/// the interface and the personality fills it in, rather than the trap path
/// naming `crate::syscall::deliver` directly.
///
/// `docs/certification/ITEM.md` is why this shape and not the direct call:
/// an upcall from the most trusted path in the system into the uncertified
/// ring above it means the core cannot be built, analysed or evaluated
/// without that ring present.
/// What became of a signal the core asked the personality to force.
///
/// A core-owned answer on purpose: `Posted` and `Origin` are the personality's
/// types, and a trap path that had to name them would be back where F-02
/// started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FaultOutcome {
    /// Delivered to a handler, or queued, or discarded because the program
    /// ignores it. The program continues either way.
    Delivered,
    /// It ended the program, whose pid this is, for the report.
    Ended {
        /// The program that ended.
        pid: u32,
    },
    /// There was no process to signal, which from user mode is a kernel bug
    /// rather than a program's.
    NoProcess,
}

#[derive(Debug)]
pub(crate) struct ReturnPath {
    /// Whether the way back has anything to do for the running task. Asked by
    /// [`attention_due`] when the task's pending-work word has a bit set, so
    /// the registers are only copied into an [`arch::UserContext`] when
    /// something will use them. It reads the state alone and clears no bit:
    /// the way out has cleared the word's before asking.
    pub(crate) needs_attention: fn() -> bool,
    /// Act on whatever `needs_attention` found. Does not return when the
    /// process has ended.
    pub(crate) return_to_user: fn(&mut arch::UserContext),
    /// Restore the registers a signal frame saved: `sigreturn`, and
    /// `rt_sigreturn` when the flag is set.
    pub(crate) sigreturn: fn(&mut arch::UserContext, bool),
    /// Force a signal on the running program for a fault it took: the
    /// signal number, its `si_code`, and the faulting address.
    ///
    /// The second upcall the core owed an interface for. A fault becoming a
    /// `SIGSEGV` is the personality's policy, but deciding that a fault *is*
    /// the program's problem is the trap path's, and the trap path is core.
    pub(crate) fault_signal: fn(u32, i32, u64) -> FaultOutcome,
}

/// The registered return path, or null until the personality registers one.
///
/// An `AtomicPtr` to a `'static` rather than a lock, as `crate::panic`'s
/// explanation is: this is read on every return to user mode, and a lock on
/// that path would be paid by every trap in the system.
static RETURN_PATH: AtomicPtr<ReturnPath> = AtomicPtr::new(ptr::null_mut());

/// Register what the way back to user mode should do.
///
/// Called once, from init, before any user mode runs. A kernel built without
/// a personality registers nothing and returns to user mode directly, which
/// is the property that makes the core independently analysable.
pub(crate) fn set_return_path(path: &'static ReturnPath) {
    RETURN_PATH.store(ptr::from_ref(path).cast_mut(), Ordering::Release);
}

/// Whether the way back to user mode has anything to do for the running
/// task: asked with interrupts masked, at every way out to ring 3 on every
/// architecture, a call's, an interrupt's and a fault's.
///
/// One look at the task's pending-work word (`sched::work`), which clears the
/// bits it finds before `path`'s `needs_attention` reads what they stand
/// for. The personality is asked only when a bit is set -- or, with the
/// self-checks on, when none is, to show that none should have been
/// (`sched::work::audit`).
pub(crate) fn attention_due(path: &ReturnPath) -> bool {
    if crate::sched::work::wants_attention(crate::sched::work::look()) {
        return (path.needs_attention)();
    }
    crate::sched::work::auditing() && crate::sched::work::audit(path.needs_attention)
}

/// Whether the way back to user mode has anything to do for the running
/// task: a kill, a signal, a stop. What a throttled task looks for.
pub(crate) fn must_attend() -> bool {
    return_path().is_some_and(|path| (path.needs_attention)())
}

/// The registered return path, if there is one.
pub(crate) fn return_path() -> Option<&'static ReturnPath> {
    let path = RETURN_PATH.load(Ordering::Acquire);
    // SAFETY: (SHARED) only `set_return_path` stores here, and only the address of a
    // `'static` that is never written through.
    unsafe { path.as_ref() }
}

/// A trap a program's own instruction took that nothing resolves: the signal
/// Linux would raise for it, forced on the program so that it either handles
/// it or ends with it as its status. Never a kernel panic -- a program cannot
/// be allowed to stop the machine with `hlt` -- unless the kernel entered user
/// mode with no process to blame.
fn user_fault(frame: &arch::TrapFrame, trap: &Trap) {
    user_fault_as(frame, trap, arch::fault_signal(frame, trap));
}

/// [`user_fault`] with the signal, `si_code` and address already decided, for
/// a fault the architecture cannot classify alone: a touch of a file mapping
/// past the end of its file is `SIGBUS`, which only the address space knows.
fn user_fault_as(frame: &arch::TrapFrame, trap: &Trap, (signal, code, address): (u32, i32, u64)) {
    let Some(path) = return_path() else {
        // No personality registered, so nothing can turn a fault into a
        // signal. A kernel built that way has no user programs to fault.
        fatal(
            frame,
            "a fault from user mode with no personality registered",
            &crate::panic::catalog::UNEXPECTED_EXCEPTION,
        )
    };

    // Open while the signal is forced. A fatal one ends the process here, which
    // wakes and interrupts its other threads and, when none of them is live,
    // closes its handles and descriptors and tells whoever watches it, none of
    // which may run with interrupts masked.
    // A trap from user mode holds no kernel lock, which is also what lets
    // the personality's return path open them from this same dispatch; they
    // are masked again before anything else here runs.
    arch::enable_interrupts();
    let outcome = (path.fault_signal)(signal, code, address);
    arch::disable_interrupts();
    match outcome {
        FaultOutcome::NoProcess => fatal(
            frame,
            "a fault from user mode with no process",
            &crate::panic::catalog::UNEXPECTED_EXCEPTION,
        ),
        FaultOutcome::Ended { pid } => {
            println!(
                "  signal   pid {pid} ended by signal {signal} at {address:#x}, pc {:#x}: {trap:?}",
                frame.instruction_pointer()
            );
        }
        FaultOutcome::Delivered => {}
    }
}

/// Resolve a translation fault, or report it and stop.
///
/// Stage 3 handles exactly one case — a kernel address inside the on-demand
/// window, which is mapped and the instruction retried. That is deliberately
/// the same shape the real handler will have: a fault is resolved by *making
/// the mapping true* and returning, never by stepping over the instruction.
/// Everything else is a bug in the kernel and is fatal.
/// Try to make a user fault true through the running program's address space.
///
/// `Ok` when the instruction may be retried. An error is a real fault: an
/// address in no region, or an access the region forbids, is `SIGSEGV`; a page
/// of a file mapping past the end of its file is `SIGBUS`. `None` in the error
/// means there was no process to ask.
fn resolve_user_fault(fault: &PageFault) -> Result<(), Option<crate::user::space::SpaceError>> {
    // Through the running task rather than through the Linux personality's
    // process: `sched` sets a task's address space from its thread's process
    // when the task is made, so these are the same object, and the fault path
    // is asking the scheduler what is running -- which is what it means.
    let Some(space) = crate::sched::current().and_then(|task| task.address_space().cloned()) else {
        // A fault from user mode with no address space is not a program's
        // mistake, it is the kernel having entered ring 3 without recording
        // who was running. Reported as fatal rather than resolved.
        return Err(None);
    };
    let access = crate::user::space::Access {
        write: fault.write,
        execute: fault.execute,
    };
    crate::object::oom::user_fault(&space, fault.address, access).map_err(Some)
}

fn handle_page_fault(frame: &mut arch::TrapFrame, fault: PageFault) {
    if !fault.present
        && !fault.user
        && crate::mm::is_demand_window(fault.address)
        && crate::mm::map_demand_page(fault.address).is_ok()
    {
        let _ = FAULTS_HANDLED.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // A fault in user mode is the ordinary case, not an error: every page a
    // program touches arrives this way, and so does every copy-on-write copy.
    // Resolved by making the mapping true and returning, which retries the
    // instruction -- never by stepping over it.
    //
    // `present` is deliberately not consulted. A write to a present but
    // read-only copy-on-write page is exactly the fault that must copy, and
    // filtering on `!present` here would send that instruction back to fault
    // for ever.
    //
    // Open while it is resolved, for a fault a program's own instruction
    // took. Resolving one may have to wait for another processor: a write to
    // a copy-on-write page replaces a translation every processor running
    // the same address space may have cached, and the shootdown that takes
    // it back is answered by an interrupt. A program with one thread is on
    // one processor and never waits for another, which is how this stood
    // until a program forked and then started threads -- the compositor,
    // drawing a frame a band a core -- and the fault masked the one
    // interrupt its own shootdown was waiting for. A trap from user mode
    // holds no kernel lock, which is what lets `user_fault_as` and
    // `deliver::return_to_user` open them from this same dispatch; the
    // address was read out of the processor before any of this, and they are
    // masked again before anything else here runs.
    let resolved = if fault.user {
        let open = frame.came_from_user();
        if open {
            arch::enable_interrupts();
        }
        let resolved = resolve_user_fault(&fault);
        if open {
            arch::disable_interrupts();
        }
        resolved
    } else {
        Err(None)
    };
    if resolved.is_ok() {
        let _ = FAULTS_HANDLED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if fault.user && frame.came_from_user() {
        /// `SIGBUS`'s `si_code` for an address with nothing behind it, which
        /// is what Linux reports for a touch of a file mapping past its file.
        const BUS_ADRERR: i32 = 2;
        let trap = Trap::PageFault(fault);
        match resolved {
            Err(Some(
                crate::user::space::SpaceError::PastEnd(address)
                | crate::user::space::SpaceError::Unreadable(address),
            )) => user_fault_as(
                frame,
                &trap,
                (ferrix_linux_abi::types::SIGBUS, BUS_ADRERR, address),
            ),
            _ => {
                // HUNT (os07-phunt, never lands): why the fault was refused.
                println!("  hunt     user fault at {:#x} refused: {:?}", fault.address, resolved);
                user_fault(frame, &trap)
            }
        }
        return;
    }

    report(
        frame,
        format_args!(
            "page fault at {:#x} ({}{}{}{})",
            fault.address,
            if fault.user { "user " } else { "kernel " },
            if fault.write { "write" } else { "read" },
            if fault.execute { ", fetch" } else { "" },
            if fault.present {
                ", protection"
            } else {
                ", not mapped"
            },
        ),
        &crate::panic::catalog::UNHANDLED_PAGE_FAULT,
    );
}

/// Report a trap the kernel cannot continue past, and stop the machine.
///
/// Also for an architecture's own entries that never reach [`dispatch`]:
/// x86-64's paranoid ones, which handle what they can and end here otherwise.
pub(crate) fn fatal(
    frame: &arch::TrapFrame,
    what: &str,
    entry: &'static crate::panic::catalog::Explanation,
) -> ! {
    report(frame, format_args!("{what}"), entry)
}

/// Report a trap under `headline`, with the registers it saved, and stop.
///
/// The opening lines are the trap's own, unlogged (`console::write_unlogged`).
/// Everything after them — the processor, stopping the others, the trace, the
/// explanation, the screen — is what every failure report has, and comes from
/// `panic.rs`, so a fatal trap is drawn on the framebuffer as a panic is. One
/// marker line, not two: the page fault's description is the headline.
fn report(
    frame: &arch::TrapFrame,
    headline: core::fmt::Arguments<'_>,
    entry: &'static crate::panic::catalog::Explanation,
) -> ! {
    let first = crate::panic::begin_report();
    println!();
    crate::console::println_unlogged!("FERRIX-PANIC {headline}");
    arch::report_trap(frame);
    if !first {
        crate::panic::abridged()
    }
    crate::panic::conclude(Some(entry))
}
