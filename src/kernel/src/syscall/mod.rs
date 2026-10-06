//! The system call dispatch layer.
//!
//! Stage 7 of `docs/ROADMAP.md`. Everything below this point is the kernel
//! talking to itself; this is where a program that was not written for Ferrix
//! asks it for something, using the numbers and the conventions Linux fixed.
//!
//! # The seam
//!
//! One function, [`dispatch`], agreed with the stage 6 owner so that neither
//! side has to know the other's job. Their trap vector saves registers, fills
//! a [`SyscallArgs`] from the frame, and calls `crate::trap::system_call`,
//! which `main.rs` points at [`dispatch_with`]. It returns an [`Outcome`] which their code
//! applies. That puts every register convention on their side of the line and
//! every ABI decision on this one. Both types are the core's
//! (`crate::trap`), and the core reaches this function only through what was
//! registered with it, so the trap path names nothing above the core
//! (`docs/certification/FINDINGS.md`, F-09).
//!
//! [`Outcome`] has two variants rather than being a bare `isize` because "put
//! this in the return register" does not describe every call; `crate::trap`
//! says why.
//!
//! # Three number tables, one dispatch
//!
//! x86-64, AArch64 and ARMv7-A each number their calls differently, and
//! `src/lib/proto/linux-abi` folds all three onto one [`Syscall`]. Which table applies
//! is the one architecture-dependent fact here, so it is asked of the facade
//! ([`arch::decode_syscall`]) rather than decided with a `cfg` — generic kernel
//! code naming an architecture is what `tools/common/check/check-crate-layering.sh`
//! exists to stop. A fourth table, i386's, is x86-64's compatibility entry
//! ([`arch::decode_compat_syscall`]): which of the two a call is in is the
//! entry's to say, in [`SyscallArgs::abi`], and the facade's to decode
//! (`docs/I386.md` §3.2).
//!
//! # What is decided here, and what above
//!
//! This file is the certified item's: the way in from the core, the native
//! range sent to the native ABI ([`native`]), and a Linux number decoded
//! onto its [`Syscall`] with the clamp that keeps a mispredicted bound from
//! reaching past the table. What a decoded Linux call does is the Linux
//! personality's, which is above the item: `main.rs` composes it with
//! [`dispatch_with`] as a [`Personality`] (`linux.rs`), and this file names
//! none of its modules.
//!
//! The `mod` declarations below are where the personality's modules sit in
//! the tree, not calls into them: `tools/common/check/check-item-boundary.py` counts
//! them apart, as containment.

pub(crate) mod attributes;
pub(crate) mod check;
pub(crate) mod compat;
pub(crate) mod credentials;
pub(crate) mod deliver;
pub(crate) mod epoll;
pub(crate) mod eventfd;
pub(crate) mod exec;
pub(crate) mod family;
pub(crate) mod fd;
pub(crate) mod file;
pub(crate) mod flock;
pub(crate) mod fsctl;
pub(crate) mod futex;
pub(crate) mod image;
pub(crate) mod init_calls_check;
pub(crate) mod kill;
pub(crate) mod launch;
pub(crate) mod limits;
pub(crate) mod linux;
pub(crate) mod load;
pub(crate) mod memfd;
pub(crate) mod memory;
pub(crate) mod namespace;
pub(crate) mod native;
pub(crate) mod native_check;
pub(crate) mod nsproxy;
pub(crate) mod path;
pub(crate) mod pidns;
pub(crate) mod pipe;
pub(crate) mod poll;
pub(crate) mod process;
pub(crate) mod program;
pub(crate) mod program_check;
pub(crate) mod registry;
pub(crate) mod seccomp;
pub(crate) mod seccomp_check;
pub(crate) mod seccomp_filters_check;
pub(crate) mod sem;
pub(crate) mod sem_check;
pub(crate) mod shm;
pub(crate) mod shm_check;
pub(crate) mod signal;
pub(crate) mod signalfd;
pub(crate) mod sigpage;
pub(crate) mod sockets;
pub(crate) mod stat;
pub(crate) mod system;
pub(crate) mod thread;
pub(crate) mod thread_area;
pub(crate) mod time;
pub(crate) mod timerfd;
pub(crate) mod tty;
pub(crate) mod uaccess;
pub(crate) mod unmap_check;
pub(crate) mod userns;
pub(crate) mod vdso;
pub(crate) mod vdso_check;

use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_linux_abi::errno::{self, Errno};
use ferrix_linux_abi::nr::Syscall;

use crate::arch;
use crate::console::println;
use crate::sched;

/// The shape of a system call and of its answer, which the core's trap path
/// owns: re-exported so the personality's handlers and their checks name them
/// where they always have.
pub(crate) use crate::trap::{Abi, Outcome, SyscallArgs};

/// Answer one system call, with `P` answering the Linux calls.
///
/// `regs` is the caller's saved user registers, which a fork child resumes
/// from; `None` from a kernel caller, which cannot fork.
///
/// Never returns an error and never panics: an unknown number is `ENOSYS`, the
/// same as Linux. There is nothing above this to catch a failure — the caller
/// is a trap vector with a program waiting on it — so every path here has to
/// end in a value.
///
/// Generic over the personality rather than holding it in a pointer: `main.rs`
/// registers `dispatch_with::<Linux>` as the core's entry, so the item's code
/// names no module of the personality, and a Linux call pays the one indirect
/// call the core's entry costs and no second one.
pub(crate) fn dispatch_with<P: Personality>(
    args: &SyscallArgs,
    regs: Option<&arch::UserRegs>,
) -> Outcome {
    // The native ABI first, by range, before any Linux table is asked: the
    // two ABIs never have to agree about a number, and `arch::decode_syscall`
    // never sees one of Ferrix's own. See `native`. Only from the native
    // entry: the native ABI is 64-bit words, and a 32-bit program's
    // `int $0x80` reaches the i386 table alone.
    if args.abi == Abi::Native && ferrix_native_abi::nr::is_native(args.number) {
        return native_call(args);
    }
    // The table is the entry's (`crate::trap::Abi`), each behind its own
    // clamp in the facade.
    let decoded = match args.abi {
        Abi::Native => arch::decode_syscall(args.number),
        Abi::Compat => arch::decode_compat_syscall(args.number),
    };
    let Some(call) = decoded else {
        unanswered(None, args.number);
        return Outcome::Return(Errno::ENOSYS.as_return_value());
    };
    P::answer(call, args, regs)
}

/// Answer one system call as a program's is answered: through the entry the
/// core holds (`crate::trap::system_call`), which is what the boot
/// self-checks call so that they go the way a program's call goes.
pub(crate) fn dispatch(args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome {
    crate::trap::system_call(args, regs)
}

/// Answer a native call for the running task.
///
/// The caller as the core holds it: the task's thread, and the process that
/// thread runs in, which is all the native ABI asks of it. No personality's
/// type is named to find it.
fn native_call(args: &SyscallArgs) -> Outcome {
    let task = sched::current();
    crate::sched::prof::stamp(crate::sched::prof::Point::ECurrent);
    let caller = task
        .as_deref()
        .and_then(sched::Task::thread)
        .map(|thread| thread.process());
    // The one call that answers in more than one register.
    if args.number == ferrix_native_abi::nr::CHANNEL_WRITE_READ
        && let Some(caller) = caller
    {
        return write_read_outcome(native::dispatch_write_read(args, caller));
    }
    Outcome::Return(errno::encode(native::dispatch(args, caller)))
}

/// What `channel_write_read`'s answer leaves in the program's registers:
/// the count and the words, or the refusal. The general path's, and the
/// fast path's frame tail and continuation's (`docs/OPAQUE-KERNEL.md` §9.7).
pub(crate) fn write_read_outcome(answered: Result<(usize, [u64; 3]), Errno>) -> Outcome {
    match answered {
        Ok((count, words)) => Outcome::ReturnWords {
            value: errno::encode(Ok(count)),
            words,
        },
        Err(refused) => Outcome::Return(errno::encode(Err(refused))),
    }
}

/// How many more calls answered `ENOSYS` may be reported. See
/// [`report_unanswered`].
static UNANSWERED_LINES: AtomicU32 = AtomicU32::new(0);

/// Report the next `lines` calls answered `ENOSYS` on the console, a line each.
///
/// What turns a foreign program's failure into the name of the call it was
/// missing: busybox refused a call often carries on and fails later, or prints
/// nothing, and the serial log is all a boot test has. Off unless init turns
/// it on around a program, because the boot self-check answers every number up
/// to 600 with `ENOSYS` on purpose. A bound rather than a switch, because a
/// program that retries a refused call forever needs reporting once, not a
/// log of it.
pub(crate) fn report_unanswered(lines: u32) {
    UNANSWERED_LINES.store(lines, Ordering::Relaxed);
}

/// One `ENOSYS`, reported if [`report_unanswered`] left room for it.
pub(crate) fn unanswered(call: Option<Syscall>, number: usize) {
    let room = UNANSWERED_LINES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
        left.checked_sub(1)
    });
    if room.is_err() {
        return;
    }
    match call {
        Some(call) => println!("  syscall  {call:?} (number {number}) answered ENOSYS"),
        None => println!("  syscall  number {number}, in no table, answered ENOSYS"),
    }
}

/// What answers a Linux system call, once the item has decoded its number:
/// the Linux personality, composed with [`dispatch_with`] by `main.rs`.
///
/// The personality is above the certified item -- a compatibility obligation,
/// the largest body of code in the kernel, none of which has to be correct for
/// isolation to hold (`docs/certification/ITEM.md`) -- so the item keeps the
/// way in, the decoding of the number and its Spectre clamp, and hands the
/// decoded call on through this trait, rather than naming the personality's
/// modules (`docs/certification/FINDINGS.md`, F-09).
pub(crate) trait Personality {
    /// Answer `call`, which the item decoded from `args`; `regs` as
    /// [`dispatch_with`] has them.
    fn answer(call: Syscall, args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome;
}
