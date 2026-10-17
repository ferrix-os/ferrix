//! Linux's `prctl(PR_SET_SYSCALL_USER_DISPATCH)`, for Wine and Proton.
//!
//! Wine runs Windows programs as Linux processes. Some of them make Windows
//! system calls directly, with a `syscall` instruction in their own code.
//! On Linux, Proton's Wine asks the kernel to hand such calls back to it:
//! it names its libc as the range whose calls always run, and keeps one byte
//! in its memory, the selector, that says whether calls from anywhere else
//! run (`SYSCALL_DISPATCH_FILTER_ALLOW`) or come back to it as a `SIGSYS`
//! (`SYSCALL_DISPATCH_FILTER_BLOCK`). Its `SIGSYS` handler then performs the
//! Windows call itself. This module is that interface, as Linux's
//! `kernel/entry/syscall_user_dispatch.c` defines it.
//!
//! A call that is handed back does not run. It is answered with S4's
//! [`Verdict::Trap`]: the registers read as they did when the call was made,
//! and a `SIGSYS` with `si_code` `SYS_USER_DISPATCH` is delivered before the
//! program runs on. So the program can only lose calls through it, never
//! gain one (H.TRAP.16), and the core does not change.
//!
//! It is judged inside the personality's look at every call
//! (`seccomp::check`), before seccomp's filters, in the order Linux uses.
//! Its design and the certification consultant's verdict are in
//! `docs/SECCOMP.md` §13 and the review ledger, line 2323.
//!
//! **Per thread, never copied.** Every new thread, fork child and vfork
//! child starts with it off (`Thread::with`), as Linux's `dup_task_struct`
//! clears it, and `execve` clears it for the calling thread. Only the thread
//! itself writes it; Ferrix has no ptrace.
//!
//! **The selector is read through `uaccess::copy_from_user`** at every call
//! that needs it, which checks and clamps the address each time. The read
//! may bring the page in, and so may wait: it runs with interrupts open and
//! with no lock held, the state's own leaf lock let go first. The core's
//! contract for a registered look (`trap::SyscallFilter`) binds the path
//! every call of every program takes; this read is only on the path of a
//! thread that asked for dispatch, for a call from outside its range
//! (the consultant's condition 4).
//!
//! **x86-64 only**, as on Linux 6.18 and 7.0, which offer it only where the
//! architecture has `GENERIC_ENTRY`. On AArch64 and ARMv7-A every mode is
//! `EINVAL` and nothing is set.

use ferrix_linux_abi::errno::Errno;

use super::{ENOSYS, SIGSYS, mark_dead, report};
use crate::arch;
use crate::syscall::process;
use crate::syscall::signal::{Origin, Posted, SYS_USER_DISPATCH};
use crate::syscall::thread::Thread;
use crate::syscall::{deliver, uaccess};
use crate::trap::{SyscallArgs, Verdict};

/// The `prctl` option.
pub(crate) const PR_SET_SYSCALL_USER_DISPATCH: i32 = 59;
/// Off: offset, length and selector must all be zero.
pub(crate) const PR_SYS_DISPATCH_OFF: u64 = 0;
/// Calls from outside `[offset, offset + len)` are judged; also spelled
/// `PR_SYS_DISPATCH_ON`.
pub(crate) const PR_SYS_DISPATCH_EXCLUSIVE_ON: u64 = 1;
/// Calls from inside `[offset, offset + len)` are judged.
pub(crate) const PR_SYS_DISPATCH_INCLUSIVE_ON: u64 = 2;
/// The selector's value that lets a call run.
pub(crate) const SYSCALL_DISPATCH_FILTER_ALLOW: u8 = 0;
/// The selector's value that hands a call back.
pub(crate) const SYSCALL_DISPATCH_FILTER_BLOCK: u8 = 1;
/// `SIGSEGV`: what ends a process whose selector cannot be read.
const SIGSEGV: u32 = 11;

/// Whether this architecture offers it.
pub(crate) const BUILT: bool = matches!(arch::ARCH, ferrix_bootinfo::Arch::X86_64);

/// One thread's dispatch settings, as Linux's `struct syscall_user_dispatch`
/// stores them: an inclusive range is kept inverted, so one test serves both
/// modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Dispatch {
    /// Whether it is on.
    on: bool,
    /// The start of the range whose calls run without a look.
    offset: u64,
    /// Its length; the test wraps, as Linux's does.
    len: u64,
    /// The selector byte's address; zero for none, when every call from
    /// outside the range is handed back.
    selector: u64,
}

impl Dispatch {
    /// Off: what every thread starts with and `execve` leaves.
    pub(crate) const OFF: Dispatch = Dispatch {
        on: false,
        offset: 0,
        len: 0,
        selector: 0,
    };

    /// Whether it is on.
    pub(crate) const fn is_on(&self) -> bool {
        self.on
    }

    /// Whether a call whose next instruction is at `ip` runs without a look:
    /// Linux's `ip - offset < len`, in wrapping arithmetic.
    pub(crate) const fn exempts(&self, ip: u64) -> bool {
        ip.wrapping_sub(self.offset) < self.len
    }

    /// The selector's address, if it has one.
    pub(crate) const fn selector(&self) -> Option<u64> {
        if self.selector == 0 {
            None
        } else {
            Some(self.selector)
        }
    }
}

/// What `prctl(PR_SET_SYSCALL_USER_DISPATCH, mode, offset, len, selector)`
/// stores, or why it is refused: Linux's `task_set_syscall_user_dispatch`,
/// row for row. Validated before anything is stored, so a refused call
/// changes nothing.
///
/// # Errors
///
/// `EINVAL` for a mode that is not one, stray arguments with `OFF`, an empty
/// or wrapping range, and on an architecture without it; `EFAULT` for a
/// selector outside the user half.
pub(crate) fn configure(
    mode: u64,
    offset: u64,
    len: u64,
    selector: u64,
) -> Result<Dispatch, Errno> {
    if !BUILT {
        return Err(Errno::EINVAL);
    }
    let (offset, len) = match mode {
        PR_SYS_DISPATCH_OFF => {
            if offset != 0 || len != 0 || selector != 0 {
                return Err(Errno::EINVAL);
            }
            return Ok(Dispatch::OFF);
        }
        PR_SYS_DISPATCH_EXCLUSIVE_ON => {
            if offset != 0 && offset.wrapping_add(len) <= offset {
                return Err(Errno::EINVAL);
            }
            (offset, len)
        }
        PR_SYS_DISPATCH_INCLUSIVE_ON => {
            if len == 0 || offset.wrapping_add(len) <= offset {
                return Err(Errno::EINVAL);
            }
            // Inverted, so that `exempts` holds outside the range.
            (offset.wrapping_add(len), len.wrapping_neg())
        }
        _ => return Err(Errno::EINVAL),
    };
    if selector != 0 && !uaccess::is_user_range(selector, 1) {
        return Err(Errno::EFAULT);
    }
    Ok(Dispatch {
        on: true,
        offset,
        len,
        selector,
    })
}

/// `prctl(PR_SET_SYSCALL_USER_DISPATCH)` for `thread`, the caller.
///
/// # Errors
///
/// As [`configure`].
pub(crate) fn prctl_set(thread: &Thread, args: [u64; 4]) -> Result<usize, Errno> {
    let [mode, offset, len, selector] = args;
    let settings = configure(mode, offset, len, selector)?;
    thread.with_dispatch(|mine| *mine = settings);
    Ok(0)
}

/// What the selector said about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selected {
    /// Run it.
    Allow,
    /// Hand it back.
    Block,
    /// A value that is neither: the process ends with `SIGSYS`.
    Invalid(u8),
    /// The byte could not be read: the process ends with `SIGSEGV`.
    Unreadable,
}

/// Read `thread`'s selector at `at`, with no lock held.
fn read_selector(thread: &Thread, at: u64) -> Selected {
    let mut byte = [0_u8; 1];
    match uaccess::copy_from_user(thread.process().space(), at, &mut byte) {
        Err(_) => Selected::Unreadable,
        Ok(()) => match byte[0] {
            SYSCALL_DISPATCH_FILTER_ALLOW => Selected::Allow,
            SYSCALL_DISPATCH_FILTER_BLOCK => Selected::Block,
            other => Selected::Invalid(other),
        },
    }
}

/// Judge one call of `thread` (the running thread) by its dispatch
/// settings, with interrupts open. `None`: dispatch has nothing to say, and
/// the call goes on to seccomp's filters. `Some`: the answer, and seccomp is
/// not asked.
///
/// The number is not looked at: a native-range number is handed back like
/// any other (the consultant's ruling on Q3), and `si_arch` is the entry's.
pub(crate) fn judge(thread: &Thread, args: &SyscallArgs) -> Option<Verdict> {
    // Copied out: the leaf lock is let go before the selector is read.
    let settings = thread.dispatch();
    if !settings.is_on() || settings.exempts(args.ip) {
        return None;
    }
    if let Some(at) = settings.selector() {
        match read_selector(thread, at) {
            Selected::Allow => return None,
            Selected::Block => {}
            Selected::Invalid(_) => {
                return Some(end(thread, args, SIGSYS, "dispatch kill (selector value)"));
            }
            Selected::Unreadable => {
                return Some(end(thread, args, SIGSEGV, "dispatch kill (selector fault)"));
            }
        }
    }
    let origin = Origin::Sys {
        code: SYS_USER_DISPATCH,
        errno: 0,
        call_addr: args.ip,
        // The number's low 32 bits, as Linux's `syscall_get_nr` gives it;
        // the dispatcher, which never sees this call, uses the whole register.
        syscall: args.number as i32,
        arch: arch::audit_arch(args.abi),
    };
    match deliver::force(SIGSYS, origin) {
        None | Some(Posted::Fatal) => Some(end(
            thread,
            args,
            SIGSYS,
            "dispatch kill (SIGSYS undeliverable)",
        )),
        Some(_) => Some(Verdict::Trap),
    }
}

/// End `thread`'s process with `signal`, as Linux's `force_exit_sig` does:
/// through the same steps as seccomp's `kill_process`, reported once
/// through seccomp's rate-limited report. The call answers `ENOSYS` and
/// never returns to the program.
fn end(thread: &Thread, args: &SyscallArgs, signal: u32, why: &str) -> Verdict {
    report(thread, args, why, 0);
    mark_dead(thread);
    process::kill(thread.process(), 128 + signal as i32);
    Verdict::Errno(ENOSYS)
}
