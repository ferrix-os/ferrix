//! The Linux personality's system call dispatcher.
//!
//! The certified item's [`super::dispatch`] takes every system call from the
//! core's trap path, sends the native range to the native ABI, and decodes
//! the rest with `arch::decode_syscall` -- the architecture's table, with the
//! Spectre clamp in front of it. What the decoded call then means is Linux's,
//! and it is answered here: [`Linux`] is the item's
//! [`Personality`](super::Personality), composed with its dispatcher by
//! `main.rs` at compile time, so the personality costs no indirect call of
//! its own.
//!
//! Here rather than in the item because this is the personality's entry
//! point: it names most of the personality's modules, and a dispatcher that
//! does is the personality's, not the item's
//! (`docs/certification/FINDINGS.md`, F-09). The item's interface to it is
//! one trait with one function.
//!
//! # What answers today
//!
//! The calls that need no process state: identity, and yielding; and every
//! call a process makes of its own state, through the tables below.
//! Everything else returns `ENOSYS`, which is a real answer rather than a
//! placeholder -- it is what Linux returns for a call it does not implement,
//! and a program that gets it can fall back. The alternative, a handler that
//! pretends to succeed, is how a program ends up wrong much later for reasons
//! nobody can trace back here.

use ferrix_linux_abi::errno::{self, Errno};
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{AT_FDCWD, O_CREAT, O_TRUNC, O_WRONLY};

use super::{
    Personality, attributes, compat, credentials, epoll, eventfd, exec, family, fd, file, flock,
    fsctl, futex, kill, limits, memfd, memory, namespace, path, poll, process, seccomp, sem, shm,
    signal, signalfd, sockets, system, thread, thread_area, time, timerfd, tty, unanswered,
};
use crate::arch;
use crate::sched;
use crate::syscall::memory::{MmapRequest, OffsetUnit};
use crate::syscall::pidns;
use crate::syscall::process::Process;
use crate::trap::{Abi, Outcome, SyscallArgs};

/// `creat`'s flags: it is `open` with these, by its definition in POSIX and
/// in `fs/open.c`.
const CREAT_FLAGS: u32 = O_CREAT | O_WRONLY | O_TRUNC;

/// The Linux personality, as the item's dispatcher is composed with it:
/// `main.rs` registers `syscall::dispatch_with::<Linux>` as the core's entry.
pub(crate) struct Linux;

impl Personality for Linux {
    fn answer(call: Syscall, args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome {
        dispatch(call, args, regs)
    }
}

/// Answer one Linux system call, `call`, as the item decoded it from `args`.
///
/// `regs` is the caller's saved user registers, which a fork child resumes
/// from; `None` from a kernel caller, which cannot fork.
///
/// Never returns an error and never panics: an unknown call is `ENOSYS`, the
/// same as Linux. There is nothing above this to catch a failure -- the
/// caller is a trap vector with a program waiting on it -- so every path here
/// has to end in a value.
fn dispatch(call: Syscall, args: &SyscallArgs, regs: Option<&arch::UserRegs>) -> Outcome {
    crate::fs::seam::syscall();
    // A 32-bit program's register pairs and 32-bit `off_t`s, rewritten into
    // the layout every handler below reads (`compat`).
    let normalized;
    let args = if args.abi == Abi::Compat {
        normalized = SyscallArgs {
            args: compat::normalize(call, args.args),
            ..*args
        };
        &normalized
    } else {
        args
    };
    // Resolved once, here, rather than reached for inside each handler: the
    // handlers take `&Process` so that the boot self-check can call them
    // against a process it built itself, months before a program can.
    let process = process::current();

    // What the entry registers held, which a restart puts back: for
    // `socketcall`, the sub-call and the block's address, not the call the
    // block names.
    let first_argument = args.args[0];

    // i386's `socketcall`: the sub-call it names, with the arguments read
    // from the program's block, is answered as that call (`compat`).
    let unpacked;
    let (call, args) = if call == Syscall::Socketcall {
        let Some(caller) = process.as_deref() else {
            return Outcome::Return(Errno::ESRCH.as_return_value());
        };
        match compat::socketcall(caller, args.args) {
            Ok((inner, block)) => {
                unpacked = SyscallArgs {
                    args: block,
                    ..*args
                };
                (inner, &unpacked)
            }
            Err(error) => {
                if error == Errno::ENOSYS {
                    unanswered(Some(call), args.number);
                }
                return Outcome::Return(error.as_return_value());
            }
        }
    } else {
        (call, args)
    };

    // `exit` ends the calling thread and `exit_group` its whole process; both
    // end the task here and never come back, and with one thread they are the
    // same. The reference is dropped first, because nothing after this line
    // runs to drop it. A kernel thread has no process to end, and gets `ESRCH`
    // from the table like every other call that needs one.
    if matches!(call, Syscall::Exit | Syscall::ExitGroup) && process.is_some() {
        drop(process);
        let status = truncate(args.args[0]) as i32 & 0xFF;
        if call == Syscall::Exit {
            process::exit_thread_current(status);
        }
        process::exit_current(status);
    }

    // `clone`, `clone3`, `fork` and `vfork` need the caller's saved registers,
    // which a kernel caller has none of, and a reference to the parent to keep.
    if matches!(
        call,
        Syscall::Clone | Syscall::Clone3 | Syscall::Fork | Syscall::Vfork
    ) {
        let (Some(parent), Some(regs)) = (process.as_ref(), regs) else {
            return Outcome::Return(Errno::ESRCH.as_return_value());
        };
        return Outcome::Return(errno::encode(family::sys_clone(
            parent, call, &args.args, regs, args.abi,
        )));
    }

    // `execve` resumes on a frame it built rather than returning, and a failure
    // past its point of no return ends the process -- after the reference is
    // dropped, for the reason above.
    if matches!(call, Syscall::Execve | Syscall::Execveat) {
        let Some(caller) = process.as_deref() else {
            return Outcome::Return(Errno::ESRCH.as_return_value());
        };
        let a = args.args;
        let word = signal::word_of(args.abi);
        let entered = if matches!(call, Syscall::Execveat) {
            let flags = truncate(a[4]);
            exec::sys_execveat(caller, fd::arg(a[0]), a[1], a[2], a[3], flags, word)
        } else {
            exec::sys_execve(caller, a[0], a[1], a[2], word)
        };
        return match entered {
            Ok((entry, stack, abi)) => Outcome::Enter { entry, stack, abi },
            Err(exec::ExecveError::Refused(error)) => Outcome::Return(error.as_return_value()),
            Err(exec::ExecveError::Lost) => {
                drop(process);
                process::exit_current(exec::lost_status())
            }
        };
    }

    // The calls that act on the calling thread's own signal state go to their
    // own table, with the caller's stack pointer for `sigaltstack`; a kernel
    // caller, with no registers, passes zero, which is on no stack.
    let thread = thread::current();
    let sp = regs.map_or(0, arch::UserRegs::stack_pointer);
    let answer = match signal::dispatch(call, &args.args, thread.as_deref(), sp, args.abi) {
        Some(answer) => answer,
        None => handle(call, args, process.as_deref()),
    };
    if answer == Err(Errno::ENOSYS) {
        unanswered(Some(call), args.number);
    }
    // A blocking call interrupted by a signal returns a restart code, never
    // seen by the program: record the call so the way back to user mode can
    // restart it or turn it into `EINTR`. The number and first argument are
    // captured from the entry registers here, because the return register is
    // about to overwrite one of them. See `deliver::return_to_user`.
    if let (Err(error), Some(thread)) = (answer, thread.as_ref())
        && error.is_restart()
    {
        thread.with_own_signals(|signals| signals.mark_restart(args.number as u64, first_argument));
        // A call to restart: the caller's own `SIGNAL`, for its way out.
        sched::work::post_own(sched::work::SIGNAL);
    }
    Outcome::Return(errno::encode(answer))
}

/// The dispatch table proper.
///
/// Split in two by what a call needs rather than by what it does: the first
/// group answers from the kernel's own state, the second needs the caller's
/// address space and is `ESRCH` without one. `ESRCH` rather than `EFAULT`
/// because the honest failure is "there is no process here", which is true of
/// every call today and will be true of none once stage 6's transition lands.
pub(crate) fn handle(
    call: Syscall,
    args: &SyscallArgs,
    process: Option<&Process>,
) -> Result<usize, Errno> {
    // The process's own number when there is a process, and the running
    // task's when there is not -- the boot self-checks call this with none.
    if matches!(call, Syscall::Getpid | Syscall::Gettid) {
        return Ok(own_number(call, process));
    }
    if let (Syscall::Getppid, Some(process)) = (call, process) {
        return Ok(process.parent_pid_in(process) as usize);
    }
    if let Some(id) = process.and_then(|process| credentials::identity(call, process)) {
        return Ok(id as usize);
    }
    if let Some(answer) = stateless(call, args) {
        return answer;
    }
    let process = process.ok_or(Errno::ESRCH)?;
    with_process(call, args, process)
}

/// What `getpid` and `gettid` answer: the caller's own number in its own
/// namespace. A thread answers `gettid` with its own number, when the caller
/// is a thread of this process. A process's first thread is numbered by its
/// pid, which is what glibc's `raise` and a fork child's `CLONE_CHILD_SETTID`
/// expect to agree. Apart, so that the dispatcher's frame, which every system
/// call pays for on a kernel stack of four pages, does not grow with it.
#[inline(never)]
fn own_number(call: Syscall, process: Option<&Process>) -> usize {
    let pid = process
        .map(|process| pidns::to_user(process, process))
        .filter(|&pid| pid != 0);
    let tid = process
        .and_then(|process| {
            thread::current()
                .filter(|thread| core::ptr::eq(thread.process().as_ref(), process))
                .map(|thread| pidns::tid_to_user(process, &thread))
        })
        .filter(|&tid| tid != 0);
    let id = match call {
        Syscall::Gettid => tid.or(pid),
        _ => pid,
    };
    id.map_or_else(current_id, |id| id as usize)
}

/// The calls that need no process: identity, and yielding.
///
/// `None` means "not one of mine", which is what lets the two tables be read
/// independently rather than as one match with a fallthrough nobody can see
/// the end of.
fn stateless(call: Syscall, args: &SyscallArgs) -> Option<Result<usize, Errno>> {
    let _ = args;
    let answer = match call {
        Syscall::Gettid => Ok(current_id()),
        // Ferrix has one process tree and no init yet, so the boot task's
        // parent is itself. A program that walks up from here terminates.
        Syscall::Getppid => Ok(1),
        // Root's ids, for a caller with no process: the boot checks, calling
        // from a kernel task. A process's own ids are answered from its
        // credentials in `handle`, before this table is asked.
        Syscall::Getuid | Syscall::Geteuid | Syscall::Getgid | Syscall::Getegid => Ok(0),
        Syscall::SchedYield => {
            sched::yield_now();
            Ok(0)
        }
        _ => return None,
    };
    Some(answer)
}

/// The calls that reshape or read the caller's address space.
fn with_process(call: Syscall, args: &SyscallArgs, process: &Process) -> Result<usize, Errno> {
    let a = args.args;
    // The two `timespec` forms, as this call's ABI lays them out.
    let native = time::TimeWidth::Native.in_abi(args.abi);
    let wide = time::TimeWidth::Wide.in_abi(args.abi);
    if let Some(answer) = descriptors(call, &a, process, args.abi) {
        return answer;
    }
    if let Some(answer) = path::dispatch(call, args, process) {
        return answer;
    }
    if let Some(answer) = fsctl::dispatch(call, &a, process, args.abi) {
        return answer;
    }
    if let Some(answer) = at_width(call, &a, process, signal::word_of(args.abi)) {
        return answer;
    }
    // `seccomp` and `prctl`'s seccomp options first: `attributes` answers every
    // other `prctl` option, and knows nothing of these.
    let answer = seccomp::dispatch(call, &a, process, args.abi)
        .or_else(|| attributes::dispatch(call, &a, process))
        .or_else(|| limits::dispatch(call, &a, process))
        .or_else(|| credentials::dispatch(call, &a, process))
        .or_else(|| sockets::dispatch(call, &a, process, args.abi))
        .or_else(|| system::dispatch(call, &a, process))
        .or_else(|| time::dispatch(call, &a, process, args.abi))
        .or_else(|| kill::dispatch(call, &a, process))
        .or_else(|| sem::dispatch(call, &a, process, args.abi))
        .or_else(|| shm::dispatch(call, &a, process, args.abi));
    if let Some(answer) = answer {
        return answer;
    }
    match call {
        // Still `ENOSYS`, each on purpose. There is no swap to turn on or off.
        Syscall::Swapon | Syscall::Swapoff => Err(Errno::ENOSYS),
        // There are no loadable modules: the kernel is one image.
        Syscall::InitModule | Syscall::FinitModule | Syscall::DeleteModule => Err(Errno::ENOSYS),
        // System V IPC's semaphores and shared memory are answered, in `sem`
        // and `shm`, above; its message queues are not, and pipes and sockets
        // stand in for them. Named here so each one is reported by name, not
        // as a number no table has.
        Syscall::Msgget | Syscall::Msgsnd | Syscall::Msgrcv | Syscall::Msgctl => Err(Errno::ENOSYS),
        // No process accounting to switch on.
        Syscall::Acct => Err(Errno::ENOSYS),
        // The console's, before a login (`docs/AUTH.md` §1).
        Syscall::Vhangup => tty::vhangup(process),
        // `rseq` is refused in `attributes::dispatch`, which says why.
        // `mmap` and `mmap2` differ in one argument's unit and nothing else,
        // which is exactly why they are separate calls: the difference is
        // invisible at the call site and catastrophic if guessed.
        Syscall::Mmap => memory::sys_mmap(process, &mmap_request(&a, OffsetUnit::Bytes)),
        Syscall::Mmap2 => memory::sys_mmap(process, &mmap_request(&a, OffsetUnit::Pages)),
        Syscall::Munmap => memory::sys_munmap(process, a[0], a[1]),
        Syscall::Mprotect => memory::sys_mprotect(process, a[0], a[1], truncate(a[2])),
        Syscall::Brk => memory::sys_brk(process, a[0]),
        Syscall::Mremap => memory::sys_mremap(process, a[0], a[1], a[2], truncate(a[3]), a[4]),
        Syscall::Msync => memory::sys_msync(process, a[0], a[1], truncate(a[2])),
        Syscall::Madvise => memory::sys_madvise(process, a[0], a[1], attributes::int(a[2])),
        Syscall::Mincore => memory::sys_mincore(process, a[0], a[1], a[2]),
        Syscall::Unshare => namespace::sys_unshare(process, a[0]),
        Syscall::Setns => namespace::sys_setns(process, fd::arg(a[0]), truncate(a[1])),
        Syscall::SetTidAddress => Ok(set_tid_address(process, a[0])),
        Syscall::SetThreadArea => thread_area::sys_set_thread_area(process, a[0]),
        Syscall::GetThreadArea => thread_area::sys_get_thread_area(process, a[0]),
        Syscall::ClockGettime => time::sys_clock_gettime(process, a[0], a[1], native),
        Syscall::ClockGetres => time::sys_clock_getres(process, a[0], a[1], native),
        Syscall::ClockGetresTime64 => time::sys_clock_getres(process, a[0], a[1], wide),
        Syscall::ClockGettime64 => time::sys_clock_gettime(process, a[0], a[1], wide),
        Syscall::Gettimeofday => time::sys_gettimeofday_at_width(process, a[0], a[1], native),
        Syscall::Time => time::sys_time_at_width(process, a[0], signal::word_of(args.abi)),
        Syscall::Getrandom => time::sys_getrandom(process, a[0], a[1], a[2]),
        Syscall::Uname => system::sys_uname(process, a[0]),
        Syscall::Poll => poll::sys_poll(process, a[0], a[1], a[2] as i32),
        Syscall::Select => {
            let sets = [a[1], a[2], a[3]];
            let word = signal::word_of(args.abi);
            poll::sys_select_at_width(process, a[0] as i32, sets, a[4], (native, word))
        }
        Syscall::Wait4 => {
            family::sys_wait4(process, a[0] as i32, a[1], truncate(a[2]), a[3], args.abi)
        }
        Syscall::Waitid => family::sys_waitid(
            process,
            truncate(a[0]),
            truncate(a[1]),
            (a[2], a[4]),
            truncate(a[3]),
            signal::word_of(args.abi),
        ),
        Syscall::PidfdOpen => family::sys_pidfd_open(process, a[0] as i32, truncate(a[1])),
        Syscall::Setpgid => family::sys_setpgid(process, a[0] as i32, a[1] as i32),
        Syscall::Getpgid => family::sys_getpgid(process, a[0] as i32),
        Syscall::Getpgrp => family::sys_getpgid(process, 0),
        Syscall::Getsid => family::sys_getsid(process, a[0] as i32),
        Syscall::Setsid => family::sys_setsid(process),
        Syscall::Futex => futex::sys_futex(process, &a, native),
        Syscall::FutexTime64 => futex::sys_futex(process, &a, wide),
        Syscall::RtSigaction => {
            signal::sys_rt_sigaction(process, truncate(a[0]), a[1], a[2], a[3], args.abi)
        }
        _ => Err(Errno::ENOSYS),
    }
}

/// The calls whose structure in memory is made of `long`s, answered at the
/// caller's `word`: four bytes for an i386 program on this 64-bit kernel. A
/// table of its own, like [`descriptors`], so that `None` means "not one of
/// mine".
fn at_width(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    word: usize,
) -> Option<Result<usize, Errno>> {
    let answer = match call {
        Syscall::Getrlimit => limits::sys_getrlimit_at_width(process, truncate(a[0]), a[1], word),
        Syscall::Setrlimit => limits::sys_setrlimit_at_width(process, truncate(a[0]), a[1], word),
        Syscall::SetRobustList => attributes::sys_set_robust_list(process, a[0], a[1], word),
        Syscall::GetRobustList => {
            attributes::sys_get_robust_list(process, fd::arg(a[0]), a[1], a[2], word)
        }
        Syscall::SchedGetaffinity => {
            limits::sys_sched_getaffinity(process, fd::arg(a[0]), truncate(a[1]), a[2], word)
        }
        Syscall::Times => time::sys_times(process, a[0], word),
        Syscall::Getrusage => time::sys_getrusage(process, fd::arg(a[0]), a[1], word),
        Syscall::Sysinfo => system::sys_sysinfo_at_width(process, a[0], word),
        _ => return None,
    };
    Some(answer)
}

/// The calls that take a descriptor, or make one.
///
/// A table of its own, like [`stateless`], so that `None` means "not one of
/// mine" and the two can be read separately. Every descriptor is narrowed to
/// the ABI's 32-bit `int` here, once, by [`fd::arg`]. `abi` says how wide the
/// caller's pointers and `long`s are in memory.
fn descriptors(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let fd = fd::arg(a[0]);
    let word = signal::word_of(abi);
    let answer = match call {
        Syscall::Openat => fd::sys_openat(process, fd, a[1], truncate(a[2]), truncate(a[3])),
        Syscall::Openat2 => fd::sys_openat2(process, fd, a[1], a[2], a[3]),
        Syscall::Open => fd::sys_openat(process, AT_FDCWD, a[0], truncate(a[1]), truncate(a[2])),
        Syscall::Creat => fd::sys_openat(process, AT_FDCWD, a[0], CREAT_FLAGS, truncate(a[1])),
        Syscall::Close => fd::sys_close(process, fd),
        Syscall::Read => file::sys_read(process, fd, a[1], a[2]),
        Syscall::Write => file::sys_write(process, fd, a[1], a[2]),
        Syscall::Readv => file::sys_readv_at_width(process, fd, a[1], a[2], word),
        Syscall::Writev => file::sys_writev_at_width(process, fd, a[1], a[2], word),
        Syscall::Pread64 => file::sys_pread64(process, fd, a[1], a[2], wide(a, 3)),
        Syscall::Pwrite64 => file::sys_pwrite64(process, fd, a[1], a[2], wide(a, 3)),
        Syscall::Lseek => fd::sys_lseek(process, fd, native_signed(a[1]), truncate(a[2])),
        Syscall::Llseek => fd::sys_llseek(process, fd, a[1], a[2], a[3], truncate(a[4])),
        Syscall::Dup => fd::sys_dup(process, fd),
        Syscall::Dup2 => fd::sys_dup2(process, fd, fd::arg(a[1])),
        Syscall::Dup3 => fd::sys_dup3(process, fd, fd::arg(a[1]), truncate(a[2])),
        Syscall::Fcntl | Syscall::Fcntl64 if flock::is_record_lock(truncate(a[1]), call, abi) => {
            flock::sys_fcntl_lock(process, fd, truncate(a[1]), a[2], call, abi)
        }
        Syscall::Fcntl | Syscall::Fcntl64 => fd::sys_fcntl(process, fd, truncate(a[1]), a[2]),
        Syscall::Ftruncate => fd::sys_ftruncate(process, fd, native_signed(a[1])),
        Syscall::Ftruncate64 => fd::sys_ftruncate(process, fd, wide(a, 1)),
        Syscall::Ioctl if abi == Abi::Compat && !compat::ioctl_passes(truncate(a[1])) => {
            Err(Errno::ENOTTY)
        }
        Syscall::Ioctl => fd::sys_ioctl(process, fd, truncate(a[1]), a[2]),
        Syscall::Flock => flock::sys_flock(process, fd, truncate(a[1])),
        Syscall::MemfdCreate => memfd::sys_memfd_create(process, a[0], truncate(a[1])),
        Syscall::Eventfd2 => eventfd::sys_eventfd2(process, truncate(a[0]), truncate(a[1])),
        Syscall::Eventfd => eventfd::sys_eventfd(process, truncate(a[0])),
        Syscall::InotifyInit1 => crate::fs::inotify::sys_inotify_init1(process, truncate(a[0])),
        Syscall::InotifyInit => crate::fs::inotify::sys_inotify_init1(process, 0),
        Syscall::InotifyRmWatch => {
            crate::fs::inotify::sys_inotify_rm_watch(process, fd, a[1] as i32)
        }
        Syscall::Signalfd4 => signalfd::sys_signalfd4(process, fd, a[1], a[2], truncate(a[3])),
        Syscall::Signalfd => signalfd::sys_signalfd(process, fd, a[1], a[2]),
        Syscall::TimerfdCreate => {
            timerfd::sys_timerfd_create(process, attributes::int(a[0]), truncate(a[1]))
        }
        Syscall::TimerfdSettime => {
            let width = time::TimeWidth::Native.in_abi(abi);
            timerfd::sys_timerfd_settime(process, fd, truncate(a[1]), [a[2], a[3]], width)
        }
        Syscall::TimerfdSettime64 => {
            let width = time::TimeWidth::Wide.in_abi(abi);
            timerfd::sys_timerfd_settime(process, fd, truncate(a[1]), [a[2], a[3]], width)
        }
        Syscall::TimerfdGettime => {
            let width = time::TimeWidth::Native.in_abi(abi);
            timerfd::sys_timerfd_gettime(process, fd, a[1], width)
        }
        Syscall::TimerfdGettime64 => {
            let width = time::TimeWidth::Wide.in_abi(abi);
            timerfd::sys_timerfd_gettime(process, fd, a[1], width)
        }
        Syscall::EpollCreate1 => epoll::sys_epoll_create1(process, truncate(a[0])),
        Syscall::EpollCreate => epoll::sys_epoll_create(process, fd::arg(a[0])),
        Syscall::EpollCtl => epoll::sys_epoll_ctl(process, fd, truncate(a[1]), fd::arg(a[2]), a[3]),
        Syscall::EpollWait => {
            epoll::sys_epoll_wait(process, fd, a[1], fd::arg(a[2]), fd::arg(a[3]))
        }
        _ => return None,
    };
    Some(answer)
}

/// A signed argument one native word wide: `off_t` and `long`.
///
/// Narrowed to the word before it is widened, so that a 32-bit caller's
/// `-1`, which arrives as `0xFFFF_FFFF`, is `-1` and not four billion. On a
/// 64-bit build the narrowing is the identity.
pub(crate) fn native_signed(value: u64) -> i64 {
    value as usize as isize as i64
}

/// A 64-bit argument the C prototype puts at position `slot`: `loff_t`.
///
/// One register on a 64-bit architecture. On ARMv7-A two, low word first,
/// and -- because the EABI passes a 64-bit value in an even-numbered register
/// pair -- starting at the next even register, which leaves a hole when
/// `slot` is odd. `pread64(fd, buf, count, pos)` therefore takes `pos` from
/// registers 4 and 5, not 3 and 4, and `ftruncate64(fd, length)` from 2 and 3.
/// QEMU's user-mode emulator applies the same rule (`regpairs_aligned` in
/// `linux-user/user-internals.h`, true for ARM EABI).
///
/// The only 32-bit ABI this kernel has is the EABI, so the word size decides
/// it, as it does for `ferrix_ustack`'s layout.
pub(crate) fn wide(a: &[u64; 6], slot: usize) -> i64 {
    if size_of::<usize>() == 8 {
        return a.get(slot).copied().unwrap_or(0) as i64;
    }
    let pair = slot + slot % 2;
    let low = a.get(pair).copied().unwrap_or(0) & 0xFFFF_FFFF;
    let high = a.get(pair + 1).copied().unwrap_or(0) & 0xFFFF_FFFF;
    (high << 32 | low) as i64
}

/// `mmap`'s six registers as a request.
///
/// `unit` is the caller's, not the register block's: it is the whole
/// difference between `mmap` and `mmap2`, and it is not in the arguments.
fn mmap_request(a: &[u64; 6], unit: OffsetUnit) -> MmapRequest {
    MmapRequest {
        addr: a[0],
        len: a[1],
        prot: truncate(a[2]),
        flags: truncate(a[3]),
        fd: signed(a[4]),
        offset: a[5],
        unit,
    }
}

/// A flag word, which is 32 bits wide in the ABI however wide the register is.
///
/// Truncating rather than refusing: a 64-bit caller's upper half is whatever
/// the compiler left in the register, and Linux ignores it. Refusing would
/// break correct programs.
pub(crate) fn truncate(value: u64) -> u32 {
    value as u32
}

/// A file descriptor, which the ABI passes as a signed 32-bit value.
///
/// `mmap` is given `-1` for an anonymous mapping, and `-1` arrives in a 64-bit
/// register as `0xFFFF_FFFF` from a 32-bit caller and `0xFFFF_FFFF_FFFF_FFFF`
/// from a 64-bit one. Narrowing to `i32` first makes both of them `-1`.
fn signed(value: u64) -> i64 {
    i64::from(value as u32 as i32)
}

/// The running task's identifier, or the boot task's if the scheduler has not
/// started.
///
/// Zero is not a valid Linux pid, so the fallback is one: a program reading
/// `getpid()` as zero would conclude something very strange about where it is.
fn current_id() -> usize {
    match sched::current() {
        Some(task) => usize::try_from(task.id).unwrap_or(1),
        None => 1,
    }
}

/// `set_tid_address`: record the address to clear when the calling thread
/// ends, and answer its id.
///
/// The number `gettid` answers, which a libc keeps as the thread's id and
/// later hands to `tgkill`; the task's own number would disagree. The address
/// is the calling thread's, when the caller is a thread of `process`; the
/// self-checks call with none.
fn set_tid_address(process: &Process, address: u64) -> usize {
    let tid = thread::current_of(process).map_or(0, |thread| {
        let _ = thread.set_clear_child_tid(address);
        pidns::tid_to_user(process, &thread)
    });
    match (tid, process.pid()) {
        (0, 0) => current_id(),
        (0, _) => pidns::to_user(process, process) as usize,
        (tid, _) => tid as usize,
    }
}
