//! Signal state: what a program asked to happen on each signal, what it has
//! blocked, and what is waiting to reach it.
//!
//! # Three modules, one table
//!
//! This one keeps the table and answers the calls that only read or change it
//! -- `rt_sigaction`, `rt_sigprocmask`, `sigaltstack`. Sending a signal is
//! `super::kill`'s, and a signal reaching a program -- a frame on its stack, a
//! handler run, `rt_sigreturn` unwinding it -- is `super::deliver`'s. Both work
//! through the methods here, under the process lock, so that "is it blocked,
//! is it ignored, is it already pending" is one decision rather than three
//! reads with a sender racing in between.
//!
//! # What the process has and what each thread has
//!
//! Linux splits the state in two, and so does this. [`Signals`] is the
//! process's: the dispositions every thread shares, the signals sent to the
//! process as a whole, and `ITIMER_REAL`. [`ThreadSignals`] is one thread's:
//! its blocked mask, its alternate stack, the signals sent to it alone (a
//! fault's), the mask `rt_sigsuspend` replaced, and the call it is to restart.
//! Anything that reads both takes the process's lock before the thread's,
//! never the other way round.
//!
//! A thread takes its own signals before the process's, and within each set a
//! fault's before any other and then the lowest number: Linux's
//! `dequeue_signal`, which looks in the thread's private queue first and in
//! the shared one only when that has nothing.
//!
//! # Layouts
//!
//! Both structures are built from native words, so they are narrower on
//! ARMv7-A, and they are read and written as native words here for the reason
//! `writev` gives: `src/lib/proto/linux-abi`'s `Sigaction` and `Stack` are the 64-bit
//! layouts, and using them for a 32-bit program would read its fields at twice
//! the stride.
//!
//! * `struct sigaction`, the kernel's and not the C library's: handler, flags
//!   and restorer as three words, then the 8-byte mask. All three
//!   architectures define `SA_RESTORER`, so the field is present on each.
//! * `stack_t`: the stack pointer as a word, the flags as an `int`, the size
//!   as a word. On a 64-bit machine the `int` is padded to the word, which is
//!   why the size sits at the second word on both widths.

use alloc::boxed::Box;

use ferrix_bootinfo::Arch;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    NSIG, SA_NOCLDSTOP, SA_NOCLDWAIT, SA_NODEFER, SA_ONSTACK, SA_RESETHAND, SIG_BLOCK, SIG_DFL,
    SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIGABRT, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGILL, SIGKILL,
    SIGQUIT, SIGSEGV, SIGSTOP, SIGSYS, SIGTRAP, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH,
    SIGXCPU, SIGXFSZ, SS_DISABLE, SS_ONSTACK,
};

use crate::arch;
use crate::fallible::{self, AllocError};
use crate::signal_frame::StackRecord;
use crate::syscall::credentials;
use crate::syscall::deliver;
use crate::syscall::pidns;
use crate::syscall::process::Process;
use crate::syscall::thread::Thread;
use crate::syscall::time::{self, TimeWidth};
use crate::syscall::{epoll, poll, uaccess};
use crate::trap::Abi;

/// The only `sigsetsize` the kernel accepts: one 64-bit word.
///
/// The C library's own `sigset_t` is far larger, and the system call takes a
/// size precisely so the two can differ. Linux refuses anything else with
/// `EINVAL`, and so does this.
pub(crate) const SIGSET_SIZE: u64 = 8;

/// `SS_AUTODISARM`: clear the alternate stack when a handler is entered on
/// it. Not in `src/lib/proto/linux-abi`; it is a flag *bit* on top of the mode, and the
/// mode check has to take it off before comparing.
const SS_AUTODISARM: i32 = i32::MIN;

/// Bytes in a native word.
const WORD: usize = size_of::<usize>();

/// Bytes in a word of the program that made a call in `abi`: the native word,
/// or four for a 32-bit program on a 64-bit kernel, whose `struct sigaction`
/// and `stack_t` are Linux's `compat_sigaction` and `compat_stack_t` --
/// the same fields, each as wide as its pointers (`docs/I386.md` §3.2).
pub(crate) const fn word_of(abi: Abi) -> usize {
    match abi {
        Abi::Native => WORD,
        Abi::Compat => 4,
    }
}

/// The two signals nothing may catch, block or ignore.
pub(crate) const UNBLOCKABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);

/// The signals whose default action is to stop.
const STOP_SIGNALS: u64 = bit(SIGSTOP) | bit(SIGTSTP) | bit(SIGTTIN) | bit(SIGTTOU);

/// The signals a fault raises, which Linux hands to a program before any
/// other pending signal: the instruction that faulted is the one it is about.
const SYNCHRONOUS: u64 =
    bit(SIGSEGV) | bit(SIGBUS) | bit(SIGILL) | bit(SIGTRAP) | bit(SIGFPE) | bit(SIGSYS);

/// What a program asked to happen on one signal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Disposition {
    /// The handler address, or `SIG_DFL` or `SIG_IGN`.
    pub(crate) handler: u64,
    /// The `SA_*` flags.
    pub(crate) flags: u64,
    /// The trampoline the handler returns into, which issues `rt_sigreturn`.
    pub(crate) restorer: u64,
    /// Signals blocked while the handler runs, never including the two that
    /// cannot be blocked.
    pub(crate) mask: u64,
}

/// What happens to a signal nobody installed a handler for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultAction {
    /// The process ends, reported as killed by the signal.
    Terminate,
    /// The same, for the signals Linux would also dump core for. Nothing
    /// dumps core here, so a waiting parent sees no core flag.
    Core,
    /// Nothing happens.
    Ignore,
    /// The process stops until `SIGCONT`.
    Stop,
    /// A stopped process continues; nothing else happens.
    Continue,
}

/// The default action of `signal`, from `signal(7)`'s table.
pub(crate) const fn default_action(signal: u32) -> DefaultAction {
    match signal {
        SIGCHLD | SIGURG | SIGWINCH => DefaultAction::Ignore,
        SIGCONT => DefaultAction::Continue,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGBUS | SIGFPE | SIGSEGV | SIGXCPU | SIGXFSZ
        | SIGSYS => DefaultAction::Core,
        _ => DefaultAction::Terminate,
    }
}

/// Who or what raised a pending signal: the `siginfo` a handler is given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The kernel, for its own reasons: an interval timer, a broken pipe.
    #[default]
    Kernel,
    /// `kill` from the process with this pid.
    User {
        /// The sender.
        pid: u32,
        /// Its real user id, as a kernel id: told to the receiver as its
        /// namespace names it, or 65534 (`si_uid`).
        uid: u32,
    },
    /// `tkill` or `tgkill` from the process with this pid.
    Thread {
        /// The sender.
        pid: u32,
        /// Its real user id, as [`Origin::User`]'s.
        uid: u32,
    },
    /// A child changed state.
    Child {
        /// `CLD_EXITED`, `CLD_KILLED`, `CLD_STOPPED` or `CLD_CONTINUED`.
        code: i32,
        /// The child.
        pid: u32,
        /// Its real user id, as [`Origin::User`]'s.
        uid: u32,
        /// Its exit code, or the signal that ended, stopped or continued it.
        status: i32,
    },
    /// A system call a seccomp filter trapped: `SIGSYS` with `si_code`
    /// `SYS_SECCOMP`, the filter's data as `si_errno`, and the call, as
    /// `_sigsys` (`_call_addr`, `_syscall`, `_arch`).
    Sys {
        /// `SECCOMP_RET_DATA`: the 16 bits the filter returned with `TRAP`.
        errno: u32,
        /// The instruction after the call: where the program's context says it
        /// is, so that a handler can check the signal is the kernel's.
        call_addr: u64,
        /// The call's number.
        syscall: i32,
        /// The `AUDIT_ARCH_*` token of the entry the call was made through.
        arch: u32,
    },
    /// A fault in the program's own instruction.
    Fault {
        /// `SEGV_MAPERR`, `SEGV_ACCERR`, `ILL_ILLOPC` and their like.
        code: i32,
        /// The address the fault was about.
        address: u64,
    },
}

/// `si_code` for a signal the kernel raised.
const SI_KERNEL: i32 = 0x80;
/// `si_code` for `kill`.
const SI_USER: i32 = 0;
/// `si_code` for `tkill` and `tgkill`.
const SI_TKILL: i32 = -6;
/// `si_code` for a system call a seccomp filter trapped.
pub(crate) const SYS_SECCOMP: i32 = 1;

/// Bytes in `siginfo_t` on every architecture.
pub(crate) use crate::signal_frame::SIGINFO_BYTES;

impl Origin {
    /// The `siginfo_t` a handler sees for `signal` raised this way.
    ///
    /// `si_signo`, `si_errno` and `si_code` are three `int`s; the union starts
    /// at the first pointer-aligned offset after them, 16 on a 64-bit machine
    /// and 12 on a 32-bit one. In it, `kill` and a child put the pid and uid
    /// first, a child its status after them, and a fault the address.
    pub(crate) fn encode(self, signal: u32) -> [u8; SIGINFO_BYTES] {
        let mut info = [0_u8; SIGINFO_BYTES];
        let union = if WORD == 8 { 16 } else { 12 };
        put_int(&mut info, 0, signal as i32);
        let code = match self {
            Origin::Kernel => SI_KERNEL,
            Origin::User { pid, uid } => {
                put_int(&mut info, union, pidns::show_pid(pid) as i32);
                put_int(&mut info, union + 4, credentials::show_uid(uid) as i32);
                SI_USER
            }
            Origin::Thread { pid, uid } => {
                put_int(&mut info, union, pidns::show_pid(pid) as i32);
                put_int(&mut info, union + 4, credentials::show_uid(uid) as i32);
                SI_TKILL
            }
            Origin::Child {
                code,
                pid,
                uid,
                status,
            } => {
                put_int(&mut info, union, pidns::show_pid(pid) as i32);
                put_int(&mut info, union + 4, credentials::show_uid(uid) as i32);
                put_int(&mut info, union + 8, status);
                code
            }
            Origin::Sys {
                errno,
                call_addr,
                syscall,
                arch,
            } => {
                // `siginfo_t._sigsys`: a pointer, then two `int`s, laid out
                // for this machine's word. A 32-bit program on a 64-bit
                // kernel gets its own layout from
                // `sigframe32::siginfo_from_64`, which knows this origin.
                put_int(&mut info, 4, errno as i32);
                let _ = put_word(&mut info, union, call_addr, WORD);
                put_int(&mut info, union + WORD, syscall);
                put_int(&mut info, union + WORD + 4, arch as i32);
                SYS_SECCOMP
            }
            Origin::Fault { code, address } => {
                let _ = put_word(&mut info, union, address, WORD);
                code
            }
        };
        put_int(&mut info, 8, code);
        info
    }

    /// The `struct signalfd_siginfo` a signalfd read answers for `signal`
    /// raised this way: fixed-width fields, the same on every architecture
    /// (`include/uapi/linux/signalfd.h`). `ssi_signo`, `ssi_errno` and
    /// `ssi_code` at 0, 4 and 8; the sender's `ssi_pid` at 12 and `ssi_uid`
    /// at 16; a child's `ssi_status` at 40; a fault's `ssi_addr` at 72. The
    /// sender's uid is the reader's namespace's name for it, or 65534.
    pub(crate) fn encode_signalfd(self, signal: u32) -> [u8; SIGINFO_BYTES] {
        let mut info = [0_u8; SIGINFO_BYTES];
        put_int(&mut info, 0, signal as i32);
        let code = match self {
            Origin::Kernel => SI_KERNEL,
            Origin::User { pid, uid } => {
                put_int(&mut info, 12, pidns::show_pid(pid) as i32);
                put_int(&mut info, 16, credentials::show_uid(uid) as i32);
                SI_USER
            }
            Origin::Thread { pid, uid } => {
                put_int(&mut info, 12, pidns::show_pid(pid) as i32);
                put_int(&mut info, 16, credentials::show_uid(uid) as i32);
                SI_TKILL
            }
            Origin::Child {
                code,
                pid,
                uid,
                status,
            } => {
                put_int(&mut info, 12, pidns::show_pid(pid) as i32);
                put_int(&mut info, 16, credentials::show_uid(uid) as i32);
                put_int(&mut info, 40, status);
                code
            }
            Origin::Sys { errno, .. } => {
                put_int(&mut info, 4, errno as i32);
                SYS_SECCOMP
            }
            Origin::Fault { code, address } => {
                if let Some(slot) = info.get_mut(72..80) {
                    slot.copy_from_slice(&address.to_le_bytes());
                }
                code
            }
        };
        put_int(&mut info, 8, code);
        info
    }
}

/// Put an `int` into a `siginfo` buffer. Every offset used is a constant well
/// inside the 128 bytes.
fn put_int(buffer: &mut [u8], at: usize, value: i32) {
    if let Some(slot) = buffer.get_mut(at..at + 4) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

/// An installed alternate signal stack. A size of zero means none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AltStack {
    /// Its lowest address.
    sp: u64,
    /// Its size in bytes.
    size: u64,
    /// Whether `SS_AUTODISARM` was asked for.
    autodisarm: bool,
}

/// `ITIMER_REAL`: when `SIGALRM` is next due, and how often after.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Alarm {
    /// When it is next due, in nanoseconds on the counter; zero when disarmed.
    pub(crate) deadline: u64,
    /// The period it re-arms with once due; zero for a one-shot.
    pub(crate) interval: u64,
}

/// What [`Signals::post`] decided about a signal sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Posted {
    /// Ignored, and gone.
    Discarded,
    /// Its default action ends the process, and nothing blocks it: the sender
    /// ends the process at once rather than leaving it to find out.
    Fatal,
    /// Waiting for the process to reach user mode, or to unblock it.
    Pending,
}

/// A system call to restart because a signal interrupted it, captured when the
/// call returned a restart code.
///
/// The number and first argument are kept because the return register
/// overwrites one or the other of them: the number on x86-64, whose `RAX` is
/// both, and the first argument on the two Arm architectures, whose `x0`/`r0`
/// is both. The way back to user mode puts back whichever its architecture
/// clobbered before rewinding the program counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Restart {
    /// The call's number, in the architecture's own table.
    pub(crate) nr: u64,
    /// The value in its first argument register when it was made.
    pub(crate) arg0: u64,
}

/// A sleep to resume through `restart_syscall`: what `nanosleep` and
/// `clock_nanosleep` leave behind when a signal interrupts them with time
/// still to run, so the resume waits out the time left rather than starting
/// the whole sleep again. Linux keeps this in `current->restart_block`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RestartBlock {
    /// The absolute deadline on the counter the sleep runs to.
    pub(crate) deadline: u64,
    /// Where the time left is written if the resume is interrupted again, or
    /// zero for an absolute sleep, which never reports a remainder.
    pub(crate) rem: u64,
    /// The width of the `timespec` at `rem`.
    pub(crate) width: TimeWidth,
}

/// A signal taken off a pending set, with everything delivery needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Taken {
    /// Its number.
    pub(crate) signal: u32,
    /// Who raised it.
    pub(crate) origin: Origin,
    /// What the program asked to happen.
    pub(crate) action: Disposition,
}

/// Signals raised and not yet delivered, and who raised each.
///
/// One bit each: a second `SIGUSR1` sent before the first is delivered is the
/// same pending signal, as on Linux for the classic signals. Both the
/// process's shared set and each thread's private one are a `Queue`.
///
/// The origins are on the heap rather than inline. Inline they are over a
/// kibibyte, and a `Process` carries its signals by value through
/// `Process::new`, `registry::register` and `Arc::new` -- each a copy on a
/// sixteen-kibibyte kernel stack. That was the x86-64 boot's double fault in
/// `Process::new`, and the AArch64 boot's hang at the same check.
///
/// Made with [`Queue::new`], which reports running out of memory, and never
/// cloned: a process or thread is made on paths that answer `ENOMEM` or
/// `NO_MEMORY`, and a `Default` or `Clone` that allocated would stop the
/// machine there instead (finding F-23).
#[derive(Debug)]
struct Queue {
    /// Bit `n - 1` for signal `n`.
    pending: u64,
    /// Who raised each pending signal, first sender kept. Always 64 entries.
    origins: Box<[Origin]>,
}

impl Queue {
    /// Nothing pending.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for the origins.
    fn new() -> Result<Queue, AllocError> {
        Ok(Queue {
            pending: 0,
            origins: fallible::try_boxed_filled(Origin::Kernel, NSIG as usize)?,
        })
    }

    /// Record `signal` as pending, keeping the first sender's origin.
    fn add(&mut self, signal: u32, origin: Origin) {
        if self.pending & bit(signal) == 0
            && let Ok(index) = index_of(signal)
            && let Some(slot) = self.origins.get_mut(index)
        {
            *slot = origin;
        }
        self.pending |= bit(signal);
    }

    /// The signal Linux's `next_signal` would take from this set among `ready`:
    /// a fault's first, then the lowest numbered. Zero when there is none.
    const fn next(&self, ready: u64) -> u32 {
        let ready = self.pending & ready;
        if ready == 0 {
            return 0;
        }
        let chosen = if ready & SYNCHRONOUS != 0 {
            ready & SYNCHRONOUS
        } else {
            ready
        };
        chosen.trailing_zeros() + 1
    }

    /// Take `signal` off the set, with who raised it.
    fn take(&mut self, signal: u32) -> Origin {
        self.pending &= !bit(signal);
        index_of(signal)
            .ok()
            .and_then(|index| self.origins.get(index).copied())
            .unwrap_or_default()
    }
}

/// A process's signal state: the dispositions its threads share, the signals
/// sent to it as a whole, and `ITIMER_REAL`.
///
/// The disposition table is on the heap rather than inline, for the reason
/// [`Queue`]'s origins are -- and so, like a [`Queue`], it is made with
/// [`Signals::new`] or [`Signals::for_fork`], which report running out of
/// memory, and has no `Default` or `Clone` that would not.
#[derive(Debug)]
pub(crate) struct Signals {
    /// Signal `n` is at index `n - 1`. Always 64 entries.
    actions: Box<[Disposition]>,
    /// Signals sent to the process, for whichever thread takes them first.
    shared: Queue,
    /// `ITIMER_REAL`.
    alarm: Alarm,
}

impl Signals {
    /// Every signal at its default, nothing pending, no alarm: a process the
    /// kernel starts.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for the tables.
    pub(crate) fn new() -> Result<Signals, AllocError> {
        Ok(Signals {
            actions: fallible::try_boxed_filled(Disposition::default(), NSIG as usize)?,
            shared: Queue::new()?,
            alarm: Alarm::default(),
        })
    }

    /// What a process that has ended is left holding once its tables are
    /// taken: none, and so every signal reads as at its default, as a fresh
    /// table would. Allocates nothing -- an empty boxed slice is a dangling
    /// pointer -- so a release that runs as the process ends cannot fail.
    pub(crate) fn released() -> Signals {
        Signals {
            // NOALLOC: an empty boxed slice, which is a dangling pointer.
            actions: Box::default(),
            shared: Queue {
                pending: 0,
                // NOALLOC: as above.
                origins: Box::default(),
            },
            alarm: Alarm::default(),
        }
    }

    /// A fork child's: the same dispositions, and nothing pending and no
    /// alarm, since neither is inherited.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for the tables.
    pub(crate) fn for_fork(&self) -> Result<Signals, AllocError> {
        Ok(Signals {
            actions: fallible::try_boxed_slice(&self.actions)?,
            shared: Queue::new()?,
            alarm: Alarm::default(),
        })
    }

    /// Whether `signal` is at its default action: no handler and not
    /// ignored. What a namespace's init does not take from inside
    /// (`docs/PIDNS.md` §5).
    pub(crate) fn is_default(&self, signal: u32) -> bool {
        self.actions
            .get((signal as usize).wrapping_sub(1))
            .is_none_or(|action| action.handler == SIG_DFL)
    }

    /// Whether a child that ends is released without being waited for:
    /// `SIGCHLD` ignored, or its handler installed with `SA_NOCLDWAIT`. What a
    /// daemon that never calls `wait` relies on to leave no zombies.
    pub(crate) fn reaps_children_automatically(&self) -> bool {
        self.actions
            .get(SIGCHLD as usize - 1)
            .is_some_and(|action| action.handler == SIG_IGN || action.flags & SA_NOCLDWAIT != 0)
    }

    /// Whether a child stopping or continuing should go untold: `SIGCHLD`
    /// installed with `SA_NOCLDSTOP`.
    pub(crate) fn ignores_child_stops(&self) -> bool {
        self.actions
            .get(SIGCHLD as usize - 1)
            .is_some_and(|action| action.flags & SA_NOCLDSTOP != 0)
    }

    /// What `execve` does to them: every handler goes back to the default,
    /// because the new program has none of the old one's code to run; a signal
    /// that was ignored stays ignored, which is how `nohup` works; the pending
    /// signals and the interval timer are kept. The calling thread's alternate
    /// stack goes too, which is [`ThreadSignals::reset_for_exec`].
    pub(crate) fn reset_for_exec(&mut self) {
        for action in self.actions.iter_mut() {
            let ignored = action.handler == SIG_IGN;
            *action = Disposition::default();
            if ignored {
                action.handler = SIG_IGN;
            }
        }
    }

    /// Install a disposition directly, for the boot self-checks: they drive the
    /// delivery decision without a user-space `rt_sigaction`, which would need a
    /// `struct sigaction` written into a program's own memory first.
    pub(crate) fn install_action(&mut self, signal: u32, handler: u64, flags: u64) {
        self.install_action_masked(signal, handler, flags, 0);
    }

    /// [`Signals::install_action`], with `mask` blocked while the handler runs.
    pub(crate) fn install_action_masked(
        &mut self,
        signal: u32,
        handler: u64,
        flags: u64,
        mask: u64,
    ) {
        if let Ok(index) = index_of(signal)
            && let Some(slot) = self.actions.get_mut(index)
        {
            *slot = Disposition {
                handler,
                flags,
                restorer: 0,
                mask: mask & !UNBLOCKABLE,
            };
        }
    }

    /// Signals sent to the process and not yet taken, blocked ones included.
    pub(crate) const fn pending(&self) -> u64 {
        self.shared.pending
    }

    /// Record `signal` sent to the process, or decide it needs no recording.
    /// `blocked` is the mask of the thread it is judged for, and
    /// `blocked_everywhere` what every thread that could take it blocks: both
    /// zero for a process with no thread. See [`post_into`].
    pub(crate) fn post(
        &mut self,
        blocked: u64,
        blocked_everywhere: u64,
        signal: u32,
        origin: Origin,
    ) -> Posted {
        post_into(
            &self.actions,
            &mut self.shared,
            blocked,
            blocked_everywhere,
            signal,
            origin,
        )
    }

    /// Forget `signal` if the process as a whole has it pending.
    fn discard(&mut self, signal: u32) {
        self.shared.pending &= !bit(signal);
    }

    /// Forget what sending `signal` cancels, if the process as a whole has it
    /// pending. See [`cancelled_by`].
    pub(crate) fn cancel(&mut self, signal: u32) {
        self.shared.pending &= !cancelled_by(signal);
    }

    /// `ITIMER_REAL` as it stands.
    pub(crate) const fn alarm(&self) -> Alarm {
        self.alarm
    }

    /// Replace `ITIMER_REAL`, and answer what it was.
    pub(crate) const fn set_alarm(&mut self, alarm: Alarm) -> Alarm {
        core::mem::replace(&mut self.alarm, alarm)
    }

    /// Whether `ITIMER_REAL` is due at `now`, re-arming or disarming it if so.
    /// A periodic timer that fell more than a period behind is re-armed from
    /// `now`, so a stalled machine owes one `SIGALRM`, not a backlog.
    pub(crate) fn tick_alarm(&mut self, now: u64) -> bool {
        let Alarm { deadline, interval } = self.alarm;
        if deadline == 0 || deadline > now {
            return false;
        }
        self.alarm.deadline = match deadline.checked_add(interval) {
            _ if interval == 0 => 0,
            Some(next) if next > now => next,
            _ => now.saturating_add(interval),
        };
        true
    }
}

/// One thread's signal state: its blocked mask, its alternate stack, the
/// signals sent to it alone, and what it is in the middle of.
///
/// Made with [`ThreadSignals::new`], for the reason [`Queue`] is.
#[derive(Debug)]
pub(crate) struct ThreadSignals {
    /// The blocked mask, bit `n - 1` for signal `n`.
    blocked: u64,
    /// The alternate stack, if one is installed.
    alt: AltStack,
    /// Signals sent to this thread alone: a fault's.
    private: Queue,
    /// The mask `rt_sigsuspend` replaced, to be put back on the way to user
    /// mode -- after a handler's frame has saved it, if one runs.
    saved_mask: Option<u64>,
    /// The call to restart, set when a blocking call returned a restart code
    /// and consumed on the way back to user mode.
    restart: Option<Restart>,
    /// A sleep to resume through `restart_syscall`.
    restart_block: Option<RestartBlock>,
}

impl ThreadSignals {
    /// A thread's that takes `inherited` from the thread that made it, with
    /// nothing pending and nothing in progress; [`Inherited::NONE`] for a
    /// process's first thread, which blocks nothing.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when there is no memory for its pending set.
    pub(crate) fn new(inherited: Inherited) -> Result<ThreadSignals, AllocError> {
        Ok(ThreadSignals {
            blocked: inherited.blocked,
            alt: inherited.alt,
            private: Queue::new()?,
            saved_mask: None,
            restart: None,
            restart_block: None,
        })
    }

    /// Replace the blocked mask with `mask`, returning the mask it replaced:
    /// what `ppoll` and `pselect6` wait under, and put back afterwards.
    /// `SIGKILL` and `SIGSTOP` are never blocked, whatever `mask` says.
    fn replace_blocked(&mut self, mask: u64) -> u64 {
        core::mem::replace(&mut self.blocked, mask & !UNBLOCKABLE)
    }

    /// What `execve` does to the calling thread's: the alternate stack goes,
    /// since it was the old program's memory. The blocked mask and pending
    /// signals are kept.
    pub(crate) fn reset_for_exec(&mut self) {
        self.alt = AltStack::default();
    }

    /// What a thread made by this one inherits: the blocked mask and the
    /// alternate stack, and nothing it was sent or was in the middle of.
    /// Copied under the lock; the new thread's state is built from it after,
    /// since that allocates.
    pub(crate) const fn inherited(&self) -> Inherited {
        Inherited {
            blocked: self.blocked,
            alt: self.alt,
        }
    }

    /// Record that the running call is to be restarted if a signal it is about
    /// to meet allows it: its number and first argument, kept because the
    /// return register overwrites one of them. Set by dispatch when a blocking
    /// call returns a restart code.
    pub(crate) const fn mark_restart(&mut self, nr: u64, arg0: u64) {
        self.restart = Some(Restart { nr, arg0 });
    }

    /// Take the call to restart, if one was marked. Consumed once, on the way
    /// back to user mode, so a later trap cannot act on a stale one.
    pub(crate) const fn take_restart(&mut self) -> Option<Restart> {
        self.restart.take()
    }

    /// Leave a sleep to resume through `restart_syscall`.
    pub(crate) const fn set_restart_block(&mut self, block: RestartBlock) {
        self.restart_block = Some(block);
    }

    /// Take the sleep left for `restart_syscall` to resume.
    pub(crate) const fn take_restart_block(&mut self) -> Option<RestartBlock> {
        self.restart_block.take()
    }

    /// Arm an alternate stack directly, for the boot self-check that a handler
    /// with `SA_ONSTACK` is placed on it.
    pub(crate) const fn arm_alt_stack_for_check(&mut self, sp: u64, size: u64) {
        self.alt = AltStack {
            sp,
            size,
            autodisarm: false,
        };
    }

    /// The blocked mask.
    pub(crate) const fn blocked(&self) -> u64 {
        self.blocked
    }

    /// Signals sent to this thread alone and not yet taken, blocked ones
    /// included.
    pub(crate) const fn pending(&self) -> u64 {
        self.private.pending
    }

    /// Where a handler's frame goes below, for a program whose stack pointer
    /// is `sp`: the alternate stack's top when the handler asked for it and the
    /// program is not already on it, and otherwise the program's own stack
    /// past the red zone the ABI lets a leaf function use without moving `sp`.
    pub(crate) fn frame_base(&self, flags: u64, sp: u64) -> u64 {
        let base = sp.wrapping_sub(arch::SIGNAL_RED_ZONE);
        if flags & SA_ONSTACK != 0 && self.alt.size != 0 && !self.on_alt_stack(base) {
            self.alt.sp.wrapping_add(self.alt.size)
        } else {
            base
        }
    }

    /// Whether `sp` is on the alternate stack. Never, with `SS_AUTODISARM`:
    /// the stack is disarmed while a handler runs on it, so nothing is on it.
    fn on_alt_stack(&self, sp: u64) -> bool {
        !self.alt.autodisarm && sp > self.alt.sp && sp - self.alt.sp <= self.alt.size
    }

    /// The alternate stack's flags as `sigaltstack` and a frame report them.
    fn alt_flags(&self) -> i32 {
        let mut flags = if self.alt.size == 0 { SS_DISABLE } else { 0 };
        if self.alt.autodisarm {
            flags |= SS_AUTODISARM;
        }
        flags
    }

    /// Leave a handler, as `rt_sigreturn` does: the mask its frame saved comes
    /// back, and so does the alternate stack, if that is still a stack
    /// `sigaltstack` would accept from a program whose stack pointer is `sp`.
    fn leave_handler(&mut self, mask: u64, stack: Option<StackRecord>, sp: u64) {
        self.blocked = mask & !UNBLOCKABLE;
        if let Some(stack) = stack {
            let _ = self.install_alt_stack((stack.sp, stack.flags, stack.size), sp);
        }
    }

    /// Put back the mask `rt_sigsuspend` replaced, if no handler's frame took
    /// it first.
    fn restore_saved_mask(&mut self) {
        if let Some(mask) = self.saved_mask.take() {
            self.blocked = mask;
        }
    }

    /// Block `mask` instead until the way back to user mode, as
    /// `rt_sigsuspend` does.
    fn suspend_with(&mut self, mask: u64) {
        if self.saved_mask.is_none() {
            self.saved_mask = Some(self.blocked);
        }
        self.blocked = mask & !UNBLOCKABLE;
    }

    /// Forget `signal` if this thread alone has it pending.
    pub(crate) fn discard(&mut self, signal: u32) {
        self.private.pending &= !bit(signal);
    }

    /// Forget what sending `signal` cancels, if this thread alone has it
    /// pending. See [`cancelled_by`].
    pub(crate) fn cancel(&mut self, signal: u32) {
        self.private.pending &= !cancelled_by(signal);
    }

    /// Record `signal` sent to this thread alone, judged against its own mask
    /// and `process`'s dispositions, or decide it needs no recording.
    pub(crate) fn post(&mut self, process: &Signals, signal: u32, origin: Origin) -> Posted {
        let blocked = self.blocked;
        post_into(
            &process.actions,
            &mut self.private,
            blocked,
            blocked,
            signal,
            origin,
        )
    }

    /// Install an alternate stack from `sigaltstack`'s `(sp, flags, size)`,
    /// for a program whose stack pointer is `sp_now`.
    fn install_alt_stack(&mut self, stack: (u64, i32, u64), sp_now: u64) -> Result<(), Errno> {
        let (sp, flags, size) = stack;
        if self.on_alt_stack(sp_now) {
            return Err(Errno::EPERM);
        }
        let autodisarm = flags & SS_AUTODISARM != 0;
        self.alt = match flags & !SS_AUTODISARM {
            SS_DISABLE => AltStack::default(),
            0 | SS_ONSTACK if size < minimum_stack() => return Err(Errno::ENOMEM),
            0 | SS_ONSTACK => AltStack {
                sp,
                size,
                autodisarm,
            },
            _ => return Err(Errno::EINVAL),
        };
        Ok(())
    }
}

/// What a new thread takes from the one that made it. See
/// [`ThreadSignals::inherited`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Inherited {
    /// The blocked mask.
    blocked: u64,
    /// The alternate stack.
    alt: AltStack,
}

impl Inherited {
    /// Nothing: no mask and no alternate stack.
    pub(crate) const NONE: Inherited = Inherited {
        blocked: 0,
        alt: AltStack {
            sp: 0,
            size: 0,
            autodisarm: false,
        },
    };

    /// The same without the alternate stack: what a `CLONE_THREAD` child
    /// inherits, since its stack is not the one the alternate stack was set
    /// up beside.
    pub(crate) const fn without_alt_stack(self) -> Inherited {
        Inherited {
            blocked: self.blocked,
            alt: AltStack {
                sp: 0,
                size: 0,
                autodisarm: false,
            },
        }
    }
}

/// A thread's signal state, open to a change of its blocked mask: what
/// [`change_blocked`] hands its closure, and outside this module the only way
/// to write a blocked mask.
pub(crate) struct Blocking<'a>(&'a mut ThreadSignals);

impl Blocking<'_> {
    /// The thread's signal state as it stands.
    pub(crate) const fn signals(&self) -> &ThreadSignals {
        self.0
    }

    /// Replace the blocked mask with `mask`, answering the mask it replaced.
    pub(crate) fn replace_blocked(&mut self, mask: u64) -> u64 {
        self.0.replace_blocked(mask)
    }

    /// Block `mask` instead until the way back to user mode, saving the mask it
    /// replaces, as `rt_sigsuspend`, `ppoll` and `pselect6` do.
    pub(crate) fn suspend_with(&mut self, mask: u64) {
        self.0.suspend_with(mask);
    }

    /// Put back the mask [`Blocking::suspend_with`] saved, if no handler's
    /// frame took it first.
    pub(crate) fn restore_saved_mask(&mut self) {
        self.0.restore_saved_mask();
    }

    /// Put back a handler frame's mask and alternate stack, as `rt_sigreturn`
    /// does, for a program whose stack pointer is `sp`.
    pub(crate) fn leave_handler(&mut self, mask: u64, stack: Option<StackRecord>, sp: u64) {
        self.0.leave_handler(mask, stack, sp);
    }
}

/// Change `thread`'s blocked mask through `change`, and hand on to another
/// thread every signal the change newly blocks while it is pending for the
/// process: the one way a blocked mask is written, so that no writer can
/// strand a signal the thread was chosen to take.
///
/// `rt_sigprocmask`, a handler's entry and return, `rt_sigsuspend`, `ppoll`
/// and `pselect6`, and the saved mask coming back on the way to user mode all
/// come through here, as every Linux path comes through `__set_task_blocked`
/// and `retarget_shared_pending`. The change is made under the process's
/// signal lock as well as the thread's, so a signal sent meanwhile either sees
/// the new mask when it chooses a taker or is seen pending here; the hand-off
/// happens once both locks are let go.
pub(crate) fn change_blocked<R>(
    thread: &Thread,
    change: impl FnOnce(&mut Signals, &mut Blocking<'_>) -> R,
) -> R {
    let (answer, newly) = thread.with_signals(|shared, own| {
        let before = own.blocked;
        let answer = change(shared, &mut Blocking(own));
        (answer, own.blocked & !before & shared.pending())
    });
    thread.process().hand_on_newly_blocked(thread, newly);
    answer
}

/// Pending signals a thread does not block, its own and its process's: what
/// delivery has to act on.
pub(crate) const fn deliverable(process: &Signals, thread: &ThreadSignals) -> u64 {
    (process.shared.pending | thread.private.pending) & !thread.blocked
}

/// Whether the way back to user mode has anything to do for a thread: a signal
/// to deliver, a mask `rt_sigsuspend` left to put back, or a call to restart.
///
/// The last even with no signal left: a process-directed signal wakes a
/// thread's wait, and another thread may take it before this one gets back.
/// Without it the way back was skipped and the call's restart code reached
/// the program -- glibc's `pthread_join` under a 1 ms timer saw `-512` and
/// aborted with "The futex facility returned an unexpected error code"
/// (ferrix-ea, ARMv7-A, 2026-09-27).
pub(crate) const fn needs_attention(process: &Signals, thread: &ThreadSignals) -> bool {
    deliverable(process, thread) != 0 || thread.saved_mask.is_some() || thread.restart.is_some()
}

/// Raise `signal` on `thread` for a fault it cannot be allowed to ignore: a
/// blocked or ignored one is unblocked in the thread and reset to its default
/// in the process first, so the program either handles it or dies of it, and
/// never retries the instruction for ever. Linux's `force_sig_info_to_task`.
pub(crate) fn force(
    process: &mut Signals,
    thread: &mut ThreadSignals,
    signal: u32,
    origin: Origin,
) -> Posted {
    let Ok(index) = index_of(signal) else {
        return Posted::Discarded;
    };
    let blocked = thread.blocked & bit(signal) != 0;
    if let Some(action) = process.actions.get_mut(index)
        && (blocked || action.handler == SIG_IGN)
    {
        action.handler = SIG_DFL;
    }
    thread.blocked &= !bit(signal);
    let blocked = thread.blocked;
    post_into(
        &process.actions,
        &mut thread.private,
        blocked,
        blocked,
        signal,
        origin,
    )
}

/// Record `signal` in `queue`, judged against `actions` and two masks, or
/// decide it needs no recording. `blocked` is the mask of the thread the
/// signal is sent to -- for one sent to a process, its first thread's -- and
/// `blocked_everywhere` holds what every thread that could take it blocks.
///
/// Linux's order. A signal the program ignores, explicitly or by default, is
/// discarded -- unless the thread it is sent to blocks it, because the program
/// may install a handler before it unblocks it. One whose default action is
/// fatal is fatal now when some thread that could take it does not block it,
/// as Linux's `complete_signal` finds one; blocked by all of them, it waits.
/// What a stop signal or `SIGCONT` cancels is left to the caller, which can
/// reach every queue ([`cancelled_by`]).
fn post_into(
    actions: &[Disposition],
    queue: &mut Queue,
    blocked: u64,
    blocked_everywhere: u64,
    signal: u32,
    origin: Origin,
) -> Posted {
    let Ok(index) = index_of(signal) else {
        return Posted::Discarded;
    };
    if signal == SIGKILL {
        return Posted::Fatal;
    }
    let action = actions.get(index).copied().unwrap_or_default();
    let default = default_action(signal);
    let ignored = action.handler == SIG_IGN
        || action.handler == SIG_DFL
            && matches!(default, DefaultAction::Ignore | DefaultAction::Continue);
    if ignored && blocked & bit(signal) == 0 {
        return Posted::Discarded;
    }
    if action.handler == SIG_DFL
        && blocked_everywhere & bit(signal) == 0
        && matches!(default, DefaultAction::Terminate | DefaultAction::Core)
    {
        return Posted::Fatal;
    }
    queue.add(signal, origin);
    Posted::Pending
}

/// What sending `signal` cancels among the signals already pending: a stop
/// signal cancels `SIGCONT`, and `SIGCONT` every stop signal. Applied to a
/// process's own queue and to each of its threads', as Linux's
/// `prepare_signal` does, since a stop pending for any one thread stops them
/// all and a continue pending anywhere would undo a later stop.
const fn cancelled_by(signal: u32) -> u64 {
    if bit(signal) & STOP_SIGNALS != 0 {
        bit(SIGCONT)
    } else if signal == SIGCONT {
        STOP_SIGNALS
    } else {
        0
    }
}

/// Take the next signal `thread` should act on: from its own pending set
/// first and its process's only when that has none, and within each a fault's
/// first, then the lowest numbered, as Linux's `dequeue_signal` chooses.
pub(crate) fn take_next(process: &mut Signals, thread: &mut ThreadSignals) -> Option<Taken> {
    take_among(process, thread, !thread.blocked)
}

/// Take the first pending signal in `set`, blocked or not, in the order
/// [`take_next`] uses: what `rt_sigtimedwait` accepts.
pub(crate) fn take_from(
    process: &mut Signals,
    thread: &mut ThreadSignals,
    set: u64,
) -> Option<Taken> {
    take_among(process, thread, set)
}

/// Take the first pending signal in `set` from the process's own queue, for a
/// reader that is none of its threads: a kernel task reading a signalfd on
/// its behalf, as the boot check does.
pub(crate) fn take_shared(process: &mut Signals, set: u64) -> Option<Taken> {
    let signal = match process.shared.next(set) {
        0 => return None,
        signal => signal,
    };
    let origin = process.shared.take(signal);
    let action = index_of(signal)
        .ok()
        .and_then(|index| process.actions.get(index).copied())
        .unwrap_or_default();
    Some(Taken {
        signal,
        origin,
        action,
    })
}

/// Take the first pending signal among `ready`, own set before shared.
fn take_among(process: &mut Signals, thread: &mut ThreadSignals, ready: u64) -> Option<Taken> {
    let (signal, origin) = match thread.private.next(ready) {
        0 => match process.shared.next(ready) {
            0 => return None,
            signal => (signal, process.shared.take(signal)),
        },
        signal => (signal, thread.private.take(signal)),
    };
    let action = index_of(signal)
        .ok()
        .and_then(|index| process.actions.get(index).copied())
        .unwrap_or_default();
    Some(Taken {
        signal,
        origin,
        action,
    })
}

/// Enter a handler for `taken` on `thread`: answer the mask its frame saves
/// and the alternate stack as its frame records it, then block what the
/// handler asked to have blocked, forget the handler if it was one-shot, and
/// disarm the alternate stack if it asked for that.
pub(crate) fn enter_handler(
    process: &mut Signals,
    blocking: &mut Blocking<'_>,
    taken: &Taken,
) -> (u64, StackRecord) {
    let thread = &mut *blocking.0;
    let saved = thread.saved_mask.take().unwrap_or(thread.blocked);
    let mut adding = taken.action.mask;
    if taken.action.flags & SA_NODEFER == 0 {
        adding |= bit(taken.signal);
    }
    thread.blocked = (thread.blocked | adding) & !UNBLOCKABLE;
    if taken.action.flags & SA_RESETHAND != 0
        && let Ok(index) = index_of(taken.signal)
        && let Some(action) = process.actions.get_mut(index)
    {
        action.handler = SIG_DFL;
    }
    let record = StackRecord {
        sp: thread.alt.sp,
        flags: thread.alt_flags(),
        size: thread.alt.size,
    };
    if thread.alt.autodisarm {
        thread.alt = AltStack::default();
    }
    (saved, record)
}

/// The mask bit for signal `number`, which must be `1..=64`.
pub(crate) const fn bit(number: u32) -> u64 {
    1 << (number - 1)
}

/// Whether `call` reads or changes the calling thread's own signal state, or
/// waits under it: the blocked mask, the alternate stack, the pending set a
/// thread sees, the waits that swap the mask, and the sleeps that leave a
/// restart behind.
const fn acts_on_a_thread(call: Syscall) -> bool {
    matches!(
        call,
        Syscall::RtSigprocmask
            | Syscall::Sigaltstack
            | Syscall::RtSigsuspend
            | Syscall::RtSigpending
            | Syscall::RtSigtimedwait
            | Syscall::RtSigtimedwaitTime64
            | Syscall::Ppoll
            | Syscall::PpollTime64
            | Syscall::Pselect6
            | Syscall::Pselect6Time64
            | Syscall::EpollPwait
            | Syscall::EpollPwait2
            | Syscall::RestartSyscall
            | Syscall::Nanosleep
            | Syscall::ClockNanosleep
            | Syscall::ClockNanosleepTime64
    )
}

/// Answer the calls [`acts_on_a_thread`] names, or `None` for any other call.
///
/// A table of its own, as `super::fsctl` has, because every one of them needs
/// the calling thread rather than only its process. A caller with no thread --
/// a kernel task -- gets `ESRCH`, as it does for every call that needs a
/// process. `sp` is the caller's stack pointer, for `sigaltstack`; zero from
/// the self-checks, which is on no stack.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    thread: Option<&Thread>,
    sp: u64,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    if !acts_on_a_thread(call) {
        return None;
    }
    let Some(thread) = thread else {
        return Some(Err(Errno::ESRCH));
    };
    let width = match call {
        Syscall::RtSigtimedwaitTime64 | Syscall::PpollTime64 | Syscall::Pselect6Time64 => {
            TimeWidth::Wide
        }
        _ => TimeWidth::Native,
    }
    .in_abi(abi);
    Some(match call {
        Syscall::RtSigprocmask => sys_rt_sigprocmask(thread, a[0] as u32, a[1], a[2], a[3]),
        Syscall::Sigaltstack => sys_sigaltstack(thread, a[0], a[1], sp, abi),
        Syscall::RtSigsuspend => deliver::sys_rt_sigsuspend(thread, a[0], a[1]),
        Syscall::RtSigpending => deliver::sys_rt_sigpending(thread, a[0], a[1]),
        Syscall::RtSigtimedwait | Syscall::RtSigtimedwaitTime64 => {
            deliver::sys_rt_sigtimedwait(thread, a[0], a[1], a[2], a[3], width)
        }
        Syscall::Ppoll | Syscall::PpollTime64 => {
            poll::sys_ppoll(thread, a[0], a[1], a[2], a[3], a[4], width)
        }
        Syscall::Pselect6 | Syscall::Pselect6Time64 => {
            let sets = [a[1], a[2], a[3]];
            poll::sys_pselect6(thread, a[0] as i32, sets, a[4], a[5], width, word_of(abi))
        }
        Syscall::EpollPwait => epoll::sys_epoll_pwait(thread, *a),
        Syscall::EpollPwait2 => epoll::sys_epoll_pwait2(thread, *a),
        _ => time::sleep_dispatch(call, a, thread, abi).unwrap_or(Err(Errno::ENOSYS)),
    })
}

/// `rt_sigaction`.
///
/// Linux's order, which a program can observe: the new action is read before
/// anything changes, so a bad `act` pointer changes nothing; the old action
/// is written after, so a bad `oldact` pointer reports `EFAULT` with the new
/// action already installed. A pending signal the new action ignores is
/// discarded, as it would have been had it arrived now -- from the process's
/// set and from each of its threads' own.
pub(crate) fn sys_rt_sigaction(
    process: &Process,
    signal: u32,
    act: u64,
    old: u64,
    sigsetsize: u64,
    abi: Abi,
) -> Result<usize, Errno> {
    if sigsetsize != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let index = index_of(signal)?;
    let new = if act == 0 {
        None
    } else if signal == SIGKILL || signal == SIGSTOP {
        return Err(Errno::EINVAL);
    } else {
        Some(read_sigaction(process, act, word_of(abi))?)
    };

    let (previous, ignored) = process.with_signals(|signals| {
        let slot = signals.actions.get_mut(index).ok_or(Errno::EINVAL)?;
        let previous = *slot;
        let mut ignored = false;
        if let Some(mut new) = new {
            new.mask &= !UNBLOCKABLE;
            *slot = new;
            ignored = new.handler == SIG_IGN
                || new.handler == SIG_DFL
                    && matches!(
                        default_action(signal),
                        DefaultAction::Ignore | DefaultAction::Continue
                    );
            if ignored {
                signals.discard(signal);
            }
        }
        Ok((previous, ignored))
    })?;
    if ignored {
        for thread in process.threads() {
            thread.with_own_signals(|own| own.discard(signal));
        }
    }

    if old != 0 {
        write_sigaction(process, old, previous, word_of(abi))?;
    }
    Ok(0)
}

/// `rt_sigprocmask`, on the calling thread's mask.
///
/// `how` is only looked at when there is a set to apply, as on Linux: a query
/// with a nonsense `how` succeeds. When `how` is refused the old mask is not
/// written either. A signal this unblocks is delivered on the way back to user
/// mode, before the call appears to return.
pub(crate) fn sys_rt_sigprocmask(
    thread: &Thread,
    how: u32,
    set: u64,
    old: u64,
    sigsetsize: u64,
) -> Result<usize, Errno> {
    if sigsetsize != SIGSET_SIZE {
        return Err(Errno::EINVAL);
    }
    let space = thread.process().space();
    let request = if set == 0 {
        None
    } else {
        let mut bytes = [0_u8; 8];
        uaccess::copy_from_user(space, set, &mut bytes).map_err(|_| Errno::EFAULT)?;
        Some(u64::from_le_bytes(bytes) & !UNBLOCKABLE)
    };

    // Through the funnel every blocked mask is written through, which hands on
    // whatever the new mask blocks while it is pending for the process.
    let previous = change_blocked(thread, |_, blocking| {
        let signals = &mut *blocking.0;
        let previous = signals.blocked;
        if let Some(request) = request {
            signals.blocked = match how {
                SIG_BLOCK => previous | request,
                SIG_UNBLOCK => previous & !request,
                SIG_SETMASK => request,
                _ => return Err(Errno::EINVAL),
            };
        }
        Ok(previous)
    })?;

    if old != 0 {
        uaccess::copy_to_user(space, old, &previous.to_le_bytes()).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `sigaltstack`, for a thread whose stack pointer is `sp`.
///
/// The old stack is reported only if the new one was accepted, which is
/// Linux's order. A program running on its alternate stack sees `SS_ONSTACK`
/// in the old flags and may not change the stack: `EPERM`. The boot
/// self-check, which has no stack pointer, passes zero, which is on no stack.
pub(crate) fn sys_sigaltstack(
    thread: &Thread,
    ss: u64,
    old: u64,
    sp: u64,
    abi: Abi,
) -> Result<usize, Errno> {
    let process = thread.process();
    let request = if ss == 0 {
        None
    } else {
        Some(read_stack(process, ss, word_of(abi))?)
    };

    let (previous, on_stack) = thread.with_own_signals(|signals| {
        let previous = signals.alt;
        let on_stack = signals.on_alt_stack(sp);
        let flags = signals.alt_flags();
        if let Some(request) = request {
            signals.install_alt_stack(request, sp)?;
        }
        Ok::<_, Errno>(((previous, flags), on_stack))
    })?;

    if old != 0 {
        let (previous, mut flags) = previous;
        if on_stack {
            flags = SS_ONSTACK;
        }
        write_stack(
            process,
            old,
            previous.sp,
            flags,
            previous.size,
            word_of(abi),
        )?;
    }
    Ok(0)
}

/// `MINSIGSTKSZ` for this architecture: the smallest alternate stack the
/// kernel accepts. AArch64's is larger because its signal frame carries the
/// full SIMD state.
const fn minimum_stack() -> u64 {
    match arch::ARCH {
        Arch::AArch64 => 5120,
        Arch::X86_64 | Arch::Armv7a => 2048,
    }
}

/// The table index for `signal`, or `EINVAL` outside `1..=64`.
fn index_of(signal: u32) -> Result<usize, Errno> {
    if signal == 0 || signal > NSIG {
        return Err(Errno::EINVAL);
    }
    usize::try_from(signal - 1).map_err(|_| Errno::EINVAL)
}

/// Read a `struct sigaction` from the program, whose words are `word` bytes:
/// handler, flags, restorer, then the 64-bit mask.
fn read_sigaction(process: &Process, at: u64, word: usize) -> Result<Disposition, Errno> {
    let mut buffer = [0_u8; 32];
    let bytes = buffer.get_mut(..word * 3 + 8).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, bytes).map_err(|_| Errno::EFAULT)?;
    let mut mask = [0_u8; 8];
    mask.copy_from_slice(bytes.get(word * 3..).ok_or(Errno::EINVAL)?);
    Ok(Disposition {
        handler: word_at(bytes, 0, word)?,
        flags: word_at(bytes, word, word)?,
        restorer: word_at(bytes, word * 2, word)?,
        mask: u64::from_le_bytes(mask),
    })
}

/// Write a `struct sigaction` to the program, as [`read_sigaction`] reads one.
fn write_sigaction(
    process: &Process,
    at: u64,
    action: Disposition,
    word: usize,
) -> Result<(), Errno> {
    let mut buffer = [0_u8; 32];
    let bytes = buffer.get_mut(..word * 3 + 8).ok_or(Errno::EINVAL)?;
    put_word(bytes, 0, action.handler, word)?;
    put_word(bytes, word, action.flags, word)?;
    put_word(bytes, word * 2, action.restorer, word)?;
    bytes
        .get_mut(word * 3..)
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(&action.mask.to_le_bytes());
    uaccess::copy_to_user(process.space(), at, bytes).map_err(|_| Errno::EFAULT)
}

/// Read a `stack_t` from the program, whose words are `word` bytes: pointer,
/// flags, size.
fn read_stack(process: &Process, at: u64, word: usize) -> Result<(u64, i32, u64), Errno> {
    let mut buffer = [0_u8; 24];
    let bytes = buffer.get_mut(..word * 3).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, bytes).map_err(|_| Errno::EFAULT)?;
    let mut flags = [0_u8; 4];
    flags.copy_from_slice(bytes.get(word..word + 4).ok_or(Errno::EINVAL)?);
    Ok((
        word_at(bytes, 0, word)?,
        i32::from_le_bytes(flags),
        word_at(bytes, word * 2, word)?,
    ))
}

/// Write a `stack_t` to the program, as [`read_stack`] reads one. Padding
/// goes out as zero.
fn write_stack(
    process: &Process,
    at: u64,
    sp: u64,
    flags: i32,
    size: u64,
    word: usize,
) -> Result<(), Errno> {
    let mut buffer = [0_u8; 24];
    let bytes = buffer.get_mut(..word * 3).ok_or(Errno::EINVAL)?;
    put_word(bytes, 0, sp, word)?;
    bytes
        .get_mut(word..word + 4)
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(&flags.to_le_bytes());
    put_word(bytes, word * 2, size, word)?;
    uaccess::copy_to_user(process.space(), at, bytes).map_err(|_| Errno::EFAULT)
}

/// The `width`-byte word at `offset`, zero-extended.
fn word_at(bytes: &[u8], offset: usize, width: usize) -> Result<u64, Errno> {
    let end = offset.checked_add(width).ok_or(Errno::EINVAL)?;
    let mut word = [0_u8; 8];
    word.get_mut(..width)
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(bytes.get(offset..end).ok_or(Errno::EINVAL)?);
    Ok(u64::from_le_bytes(word))
}

/// Put `value` at `offset` as a `width`-byte word. For a 32-bit program every
/// value written here came from a 32-bit read, so the truncation drops
/// nothing.
fn put_word(bytes: &mut [u8], offset: usize, value: u64, width: usize) -> Result<(), Errno> {
    let end = offset.checked_add(width).ok_or(Errno::EINVAL)?;
    bytes
        .get_mut(offset..end)
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(value.to_le_bytes().get(..width).ok_or(Errno::EINVAL)?);
    Ok(())
}
