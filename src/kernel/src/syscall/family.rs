//! Processes making processes, and waiting for them: `clone`, `fork`, `vfork`,
//! `wait4`, `waitid`, and the process groups and sessions a shell's job control
//! is built on.
//!
//! # Processes and threads
//!
//! A child made here is a new process with a copy of its parent's address
//! space and a task of its own -- or, with `CLONE_THREAD`, a new thread of the
//! caller's own process, sharing everything the process has and numbered from
//! the same space. What Linux allows between the two is refused with `ENOSYS`,
//! honestly: memory shared between processes without `CLONE_VFORK`, handlers
//! shared between processes, and a thread with a descriptor table or
//! directories of its own. Nothing a C library makes asks for those.
//!
//! # Namespaces
//!
//! There are none, and a `CLONE_NEW*` flag is `EINVAL` here, which is what a
//! Linux built without the matching `CONFIG_*_NS` answers. `unshare` has
//! always said so; `clone` used to ignore the flags and hand back an ordinary
//! child in the one namespace there is, so a program that asked to be
//! sandboxed was told it got what it asked for. The two calls now agree.
//!
//! # `vfork` copies
//!
//! Linux's `vfork` child borrows its parent's memory until it calls `execve`
//! or exits, and the parent sleeps meanwhile. Here the child gets a
//! copy-on-write copy instead, and the parent still sleeps until the child
//! calls `execve` or ends. Everything a correct `vfork` child may do -- call
//! `execve` or `_exit` -- behaves the same. What differs is what POSIX already
//! calls undefined: a child writing to memory its parent then reads, which
//! glibc's `posix_spawn` does to report an `execve` failure. There the parent
//! sees success and the child exits with 127, as a shell would report anyway.

use alloc::sync::Arc;

use alloc::vec;

use ferrix_bootinfo::{Arch, PAGE_SIZE, is_user_address};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::SIGCHLD;

use crate::arch;
use crate::fs::cgroupfs;
use crate::object::job::{self, Job};
use crate::syscall::process::{self, Process};
use crate::syscall::thread::{self, Thread};
use crate::syscall::{fd, namespace, registry, thread_area, uaccess, userns, vdso};
use crate::trap::Abi;

/// The low byte of `clone`'s flags: the signal the parent is told with.
const CSIGNAL: u64 = 0xFF;
/// Every flag the legacy `clone` has room for; `clone3` alone has more.
const CLONE_LEGACY_FLAGS: u64 = 0xFFFF_FFFF;
/// Put a pidfd for the child in the parent.
const CLONE_PIDFD: u64 = 0x0000_1000;
/// Make the child its parent's sibling.
const CLONE_PARENT: u64 = 0x0000_8000;
/// Ignored by `clone` since Linux 2.6.2, and refused by `clone3`.
const CLONE_DETACHED: u64 = 0x0040_0000;
/// `clone3` only: reset the child's signal handlers to the default.
const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
/// `clone3` only: start the child in the cgroup `cgroup` names.
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;
/// The size of the first published `struct clone_args`, and the least
/// `clone3` accepts.
const CLONE_ARGS_SIZE_VER0: u64 = 64;
/// The size of the latest `struct clone_args` this kernel knows the fields of
/// (`CLONE_ARGS_SIZE_VER2`).
const CLONE_ARGS_KNOWN: usize = 88;
/// The most levels of pid namespace `set_tid` may name (`MAX_PID_NS_LEVEL`).
const MAX_PID_NS_LEVEL: u64 = 32;
/// The highest signal number.
const NSIG: u64 = 64;
/// Share the address space.
const CLONE_VM: u64 = 0x0000_0100;
/// Share the working directory and root.
const CLONE_FS: u64 = 0x0000_0200;
/// Share the file descriptor table.
const CLONE_FILES: u64 = 0x0000_0400;
/// Share signal handlers.
const CLONE_SIGHAND: u64 = 0x0000_0800;
/// The parent sleeps until the child calls `execve` or ends.
const CLONE_VFORK: u64 = 0x0000_4000;
/// Make a thread of the same process.
const CLONE_THREAD: u64 = 0x0001_0000;
/// Give the child this thread pointer.
const CLONE_SETTLS: u64 = 0x0008_0000;
/// Write the child's id into the parent's memory.
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
/// Have the child's id cleared in its memory when it ends.
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
/// Write the child's id into the child's memory.
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

/// Give the child a mount namespace of its own.
const CLONE_NEWNS: u64 = 0x0002_0000;
/// ... a cgroup namespace.
const CLONE_NEWCGROUP: u64 = 0x0200_0000;
/// ... a UTS namespace: its own host and domain name.
const CLONE_NEWUTS: u64 = 0x0400_0000;
/// ... a System V IPC and POSIX message queue namespace.
const CLONE_NEWIPC: u64 = 0x0800_0000;
/// ... a user namespace, which is what an unprivileged sandbox asks for first.
const CLONE_NEWUSER: u64 = 0x1000_0000;
/// ... a pid namespace.
const CLONE_NEWPID: u64 = 0x2000_0000;
/// ... a network namespace.
const CLONE_NEWNET: u64 = 0x4000_0000;
/// Every namespace a child could be asked to be given. A mount and a user
/// namespace exist; the rest do not. `CLONE_NEWTIME` is not among them because it is not reachable:
/// its bit is inside `CSIGNAL`, so `clone` reads it as an exit signal, as
/// Linux does, and [`clone3_request`] refuses `CSIGNAL` outright.
const CLONE_NAMESPACES: u64 = CLONE_NEWNS
    | CLONE_NEWCGROUP
    | CLONE_NEWUTS
    | CLONE_NEWIPC
    | CLONE_NEWUSER
    | CLONE_NEWPID
    | CLONE_NEWNET;

/// `wait4`: return at once if nothing has ended.
const WNOHANG: u32 = 1;
/// `wait4`: report stopped children too; `WSTOPPED` to `waitid`.
const WUNTRACED: u32 = 2;
/// `waitid`: report children that exited.
const WEXITED: u32 = 4;
/// `wait4`/`waitid`: report continued children too.
const WCONTINUED: u32 = 8;
/// `waitid`: leave the child waitable.
const WNOWAIT: u32 = 0x0100_0000;
/// Linux's thread-selection bits (`__WNOTHREAD`, `__WALL`, `__WCLONE`), accepted
/// on `wait4` and not acted on: a thread is never a child here, so every child
/// is one a wait may take whatever they say.
const WAIT_THREAD_BITS: u32 = 0xE000_0000;

/// `waitid`'s `idtype`: any child.
const P_ALL: u32 = 0;
/// `waitid`'s `idtype`: the child with this pid.
const P_PID: u32 = 1;
/// `waitid`'s `idtype`: any child in this process group.
const P_PGID: u32 = 2;
/// `waitid`'s `idtype`: the child a pidfd names.
const P_PIDFD: u32 = 3;
/// `pidfd_open`'s one flag, `O_NONBLOCK` under another name.
const PIDFD_NONBLOCK: u32 = ferrix_linux_abi::types::O_NONBLOCK;

use crate::syscall::kill::{CLD_CONTINUED, CLD_EXITED, CLD_KILLED, CLD_STOPPED};

/// Bytes in `siginfo_t` on every architecture.
const SIGINFO_BYTES: usize = 128;

/// What a new process is asked for, however the call spelled it.
#[derive(Debug, Clone, Copy, Default)]
struct CloneRequest {
    /// The `CLONE_*` flags, with the exit signal in the low byte.
    flags: u64,
    /// The child's stack pointer, or zero to keep the parent's.
    stack: u64,
    /// Where `CLONE_PARENT_SETTID` writes the child's id.
    parent_tid: u64,
    /// Where `CLONE_CHILD_SETTID` writes it and `CLONE_CHILD_CLEARTID` clears it.
    child_tid: u64,
    /// The thread pointer `CLONE_SETTLS` gives the child.
    tls: u64,
    /// For a 32-bit program, what `CLONE_SETTLS`'s `tls` named instead: a
    /// `user_desc`, read and checked, as the thread-local slot and the
    /// descriptor to put in it (`docs/I386.md` §3.5).
    thread_area: Option<(usize, u64)>,
    /// `clone3`'s `CLONE_INTO_CGROUP`: the descriptor of the cgroup
    /// directory the child starts in.
    cgroup: Option<i32>,
}

/// `clone`, `clone3`, `fork` and `vfork`. Answers the child's pid to the
/// parent; the child answers zero, from a copy of the parent's registers.
///
/// # Errors
///
/// `ENOSYS` for a thread or a pidfd; `EINVAL` for a `CLONE_NEW*` namespace,
/// none of which exists; `ENOMEM` if the address space cannot be copied;
/// `EAGAIN` with every pid in use or no task for the child; `EFAULT` for a bad
/// id pointer; and what [`clone3_request`] refuses.
pub(crate) fn sys_clone(
    parent: &Arc<Process>,
    call: Syscall,
    a: &[u64; 6],
    regs: &arch::UserRegs,
    abi: Abi,
) -> Result<usize, Errno> {
    let request = match call {
        Syscall::Fork => CloneRequest {
            flags: u64::from(SIGCHLD),
            ..CloneRequest::default()
        },
        Syscall::Vfork => CloneRequest {
            flags: CLONE_VM | CLONE_VFORK | u64::from(SIGCHLD),
            ..CloneRequest::default()
        },
        Syscall::Clone3 => clone3_request(parent, a[0], a[1])?,
        // `CONFIG_CLONE_BACKWARDS` on both Arm architectures and on i386 puts
        // the thread pointer before the child's id pointer; x86-64 has them
        // the other way. The flags are an `unsigned long` Linux narrows to 32
        // bits, so a 64-bit caller cannot reach `clone3`'s flags through here.
        _ => match (arch::ARCH, abi) {
            (Arch::X86_64, Abi::Native) => CloneRequest {
                flags: a[0] & CLONE_LEGACY_FLAGS,
                stack: a[1],
                parent_tid: a[2],
                child_tid: a[3],
                tls: a[4],
                thread_area: None,
                cgroup: None,
            },
            _ => CloneRequest {
                flags: a[0] & CLONE_LEGACY_FLAGS,
                stack: a[1],
                parent_tid: a[2],
                tls: a[3],
                child_tid: a[4],
                thread_area: None,
                cgroup: None,
            },
        },
    };
    // A thread pointer that is not a user address is refused before anything
    // is made, as Linux refuses it: on x86-64 it is written to `FS_BASE` at the
    // next switch, where a non-canonical value is a fault in the kernel. The
    // Arm architectures' thread registers hold any value.
    if request.flags & CLONE_SETTLS != 0
        && arch::ARCH == Arch::X86_64
        && request.tls != 0
        && !is_user_address(request.tls)
    {
        return Err(Errno::EPERM);
    }
    // A 32-bit program's `tls` is a `struct user_desc` for the child's
    // thread-local segment, read and refused here, before anything is made.
    let request = if request.flags & CLONE_SETTLS != 0 && abi == Abi::Compat {
        CloneRequest {
            thread_area: Some(thread_area::clone_descriptor(parent, request.tls)?),
            ..request
        }
    } else {
        request
    };
    // So is a stack that is not in the user half: the system call returns on
    // it, and on x86-64 the kernel briefly runs on the stack pointer it is
    // given. `clone3`'s stack was checked as it was read.
    if request.stack != 0 && !is_user_address(request.stack.wrapping_sub(1)) {
        return Err(Errno::EINVAL);
    }
    clone_with(parent, &request, regs)
}

/// Read and check `clone3`'s `struct clone_args`, `size` bytes of it at `at`.
///
/// # The size is a version
///
/// The structure grows at its end, and `size` says which version the caller
/// was built against. Less than the first version is `EINVAL`. More than this
/// kernel knows is fine as long as every byte it does not know is zero -- a
/// newer program asking for nothing new -- and `E2BIG` otherwise, which is
/// how a program learns the kernel is older than the feature it wanted. More
/// than a page is `E2BIG` without looking.
///
/// # Errors
///
/// Those above; `EFAULT` for a structure that cannot be read; `EINVAL` for
/// what Linux's `clone3_args_valid` refuses -- unknown flags, an exit signal
/// both in the flags and in its field, `CLONE_SIGHAND` with
/// `CLONE_CLEAR_SIGHAND`, a stack without a size or a size without a stack;
/// `ENOSYS` for `set_tid`, which needs pid namespaces this kernel does not
/// have; `EINVAL` for `CLONE_INTO_CGROUP` with a descriptor past `INT_MAX` or
/// a structure too old to carry one, as `copy_clone_args_from_user` refuses.
fn clone3_request(parent: &Process, at: u64, size: u64) -> Result<CloneRequest, Errno> {
    if size > PAGE_SIZE {
        return Err(Errno::E2BIG);
    }
    if size < CLONE_ARGS_SIZE_VER0 {
        return Err(Errno::EINVAL);
    }
    let size = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    let mut bytes = [0_u8; CLONE_ARGS_KNOWN];
    let known = bytes
        .get_mut(..size.min(CLONE_ARGS_KNOWN))
        .ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(parent.space(), at, known).map_err(|_| Errno::EFAULT)?;
    if let Some(extra) = size
        .checked_sub(CLONE_ARGS_KNOWN)
        .filter(|&extra| extra > 0)
    {
        let mut rest = vec![0_u8; extra];
        let from = at
            .checked_add(CLONE_ARGS_KNOWN as u64)
            .ok_or(Errno::EFAULT)?;
        uaccess::copy_from_user(parent.space(), from, &mut rest).map_err(|_| Errno::EFAULT)?;
        if rest.iter().any(|&byte| byte != 0) {
            return Err(Errno::E2BIG);
        }
    }
    let [
        flags,
        _pidfd,
        child_tid,
        parent_tid,
        exit_signal,
        stack,
        stack_size,
        tls,
        set_tid,
        set_tid_size,
        cgroup,
    ]: [u64; 11] = core::array::from_fn(|index| {
        bytes
            .get(index * 8..index * 8 + 8)
            .and_then(|field| field.try_into().ok())
            .map_or(0, u64::from_le_bytes)
    });

    if set_tid_size > MAX_PID_NS_LEVEL
        || (set_tid == 0 && set_tid_size > 0)
        || (set_tid != 0 && set_tid_size == 0)
    {
        return Err(Errno::EINVAL);
    }
    if exit_signal > NSIG {
        return Err(Errno::EINVAL);
    }
    if flags & !(CLONE_LEGACY_FLAGS | CLONE_CLEAR_SIGHAND | CLONE_INTO_CGROUP) != 0
        || flags & (CLONE_DETACHED | CSIGNAL) != 0
        || flags & (CLONE_SIGHAND | CLONE_CLEAR_SIGHAND) == CLONE_SIGHAND | CLONE_CLEAR_SIGHAND
        || (flags & (CLONE_THREAD | CLONE_PARENT) != 0 && exit_signal != 0)
    {
        return Err(Errno::EINVAL);
    }
    // A stack is its lowest address and a size, where `clone` took its top.
    let stack_top = match (stack, stack_size) {
        (0, 0) => 0,
        (0, _) | (_, 0) => return Err(Errno::EINVAL),
        (base, size) => {
            let top = base.checked_add(size).ok_or(Errno::EINVAL)?;
            if !is_user_address(base) || !is_user_address(top - 1) {
                return Err(Errno::EINVAL);
            }
            top
        }
    };
    let cgroup = if flags & CLONE_INTO_CGROUP == 0 {
        None
    } else if size < CLONE_ARGS_KNOWN {
        return Err(Errno::EINVAL);
    } else {
        Some(i32::try_from(cgroup).map_err(|_| Errno::EINVAL)?)
    };
    if set_tid != 0 {
        return Err(Errno::ENOSYS);
    }
    Ok(CloneRequest {
        // The exit signal has a field of its own here and the low byte of the
        // flags in `clone`; checked above to be a signal and the byte to be
        // clear.
        flags: flags | exit_signal,
        stack: stack_top,
        parent_tid,
        child_tid,
        tls,
        thread_area: None,
        cgroup,
    })
}

/// The job `CLONE_INTO_CGROUP` asks a child of `parent` to start in, from
/// the descriptor `descriptor`: `cgroupfs::clone_target`'s answer, and for a
/// thread, which cannot leave its process's cgroup, `EOPNOTSUPP` unless that
/// is the one named, as Linux answers a thread asked into another domain.
fn cgroup_target(parent: &Process, descriptor: i32, thread: bool) -> Result<Arc<Job>, Errno> {
    let file = fd::file(parent, descriptor).map_err(|_| Errno::EBADF)?;
    let from = parent.job();
    let to = cgroupfs::clone_target(&file, parent, &from)?;
    if thread && !Arc::ptr_eq(&to, &from) {
        return Err(Errno::EOPNOTSUPP);
    }
    Ok(to)
}

/// Give a child `CLONE_SETTLS`'s thread pointer: a base, or for a 32-bit
/// program the thread-local descriptor its `user_desc` described, which the
/// child's `%gs`, its parent's selector, then reads through.
fn give_thread_pointer(state: &mut arch::UserState, tls: u64, thread_area: Option<(usize, u64)>) {
    match thread_area {
        Some((index, descriptor)) => {
            let _ = state.set_thread_area(index, descriptor);
        }
        None => state.set_thread_pointer(tls),
    }
}

/// Whether the namespaces `flags` asks for can be given: `EINVAL` for one
/// Ferrix does not have and for `CLONE_NEWNS` with `CLONE_FS`, as Linux
/// refuses a namespace a shared fs context would leave; `EPERM` for
/// `CLONE_NEWNS` without privilege.
pub(crate) fn namespaces_asked(parent: &Process, flags: u64) -> Result<(), Errno> {
    if flags & (CLONE_NAMESPACES & !(CLONE_NEWNS | CLONE_NEWUSER)) != 0 {
        return Err(Errno::EINVAL);
    }
    // CVE-2013-1858 (U5): a root shared with a process outside, then `chroot`
    // inside. And a thread cannot have a user namespace of its own.
    if flags & CLONE_NEWUSER != 0 && flags & (CLONE_FS | CLONE_THREAD) != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_NEWNS != 0 {
        if flags & CLONE_FS != 0 {
            return Err(Errno::EINVAL);
        }
        // With a user namespace the child holds the capability in it.
        if flags & CLONE_NEWUSER == 0
            && !parent.with_credentials(|held| held.holds(userns::CAP_SYS_ADMIN))
        {
            return Err(Errno::EPERM);
        }
    }
    if flags & CLONE_NEWUSER != 0 {
        // Refused here, before anything is made, what `unshare` refuses the
        // same way; the namespace itself is made with the child.
        namespace::make_user_namespace(parent).map(drop)?;
    }
    Ok(())
}

/// Give `child`, forked from `parent`, the vDSO view its time namespace
/// needs, if that is not its parent's.
///
/// # Errors
///
/// `ENOMEM`; the child is to be abandoned unstarted.
pub(crate) fn retarget_vdso(parent: &Arc<Process>, child: &Arc<Process>) -> Result<(), Errno> {
    if Arc::ptr_eq(parent.space(), child.space()) {
        return Ok(());
    }
    let shifted = vdso::is_shifted(&child.time_namespace());
    if shifted == vdso::is_shifted(&parent.time_namespace()) {
        return Ok(());
    }
    vdso::retarget(child.space(), shifted).map_err(|()| Errno::ENOMEM)
}

/// Make the process `request` asks for. See [`sys_clone`].
fn clone_with(
    parent: &Arc<Process>,
    request: &CloneRequest,
    regs: &arch::UserRegs,
) -> Result<usize, Errno> {
    let CloneRequest {
        flags,
        stack,
        parent_tid,
        child_tid,
        tls,
        thread_area,
        cgroup,
    } = *request;
    // A namespace asked for is a namespace that has to exist. Ignoring the
    // flag would answer a sandbox's request for isolation with a child that
    // has none and no way to tell, which is worse than refusing; `EINVAL` is
    // what a kernel built without the namespace answers, and what `unshare`
    // here answers for the same flags. A mount namespace exists: a copy of
    // the parent's, for a child that shares no fs context with it (Linux's
    // `EINVAL` for `CLONE_NEWNS | CLONE_FS`), made with privilege.
    namespaces_asked(parent, flags)?;
    // Linux's own refusals: a thread shares its process's handlers, and
    // handlers shared without the memory they are in would run nothing.
    if flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0 {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0 {
        return Err(Errno::EINVAL);
    }
    let into = cgroup
        .map(|descriptor| cgroup_target(parent, descriptor, flags & CLONE_THREAD != 0))
        .transpose()?;
    if flags & CLONE_THREAD != 0 {
        return clone_thread(parent, request, regs);
    }
    if flags & CLONE_SIGHAND != 0 {
        return Err(Errno::ENOSYS);
    }
    if flags & CLONE_VM != 0 && flags & CLONE_VFORK == 0 {
        return Err(Errno::ENOSYS);
    }
    // No pidfds yet. Refused rather than ignored: the caller would read a
    // descriptor number out of memory nothing wrote.
    if flags & CLONE_PIDFD != 0 {
        return Err(Errno::ENOSYS);
    }

    // Not findable yet. Everything a signal sent to it is judged by -- its
    // dispositions and its thread's mask -- is in place before `kill`, a
    // process group's signal or `/proc` can reach it. Its space and its heap
    // are copied under the heap lock, so a `brk` on another thread is seen
    // whole or not at all.
    // A child asked into a cgroup is counted there from the start, and never
    // in its parent's: its first instruction already runs in it.
    let child = fork_into(parent, flags, into.as_ref())?;
    let pid = child.pid();
    // No pid left, or the job's task limit (`pids.max`) reached: Linux's
    // `EAGAIN` for both. The child charged nothing, and goes unstarted.
    if pid == 0 || child.over_quota() {
        return Err(Errno::EAGAIN);
    }
    // A child made in another time namespace than its parent is, with a copy
    // of its parent's space, reads the clock through the parent's vDSO view:
    // give it the other (`docs/NAMESPACES.md` §12.1). A `CLONE_VFORK` child
    // shares the space and keeps the parent's until its `exec`.
    retarget_vdso(parent, &child)?;
    // Its own copy of the namespace, before anything can see it: a refusal
    // goes with the child, unstarted.
    if flags & CLONE_NEWUSER != 0 {
        let fresh = namespace::make_user_namespace(parent)?;
        namespace::enter_user_namespace(&child, fresh);
    }
    if flags & CLONE_NEWNS != 0 {
        namespace::copy_namespace(child.fs_context())?;
    }
    child.set_exit_signal((flags & CSIGNAL) as u32);
    // What glibc's `posix_spawn` asks for, so that its child need not reset
    // every handler itself before `execve`. Linux leaves the alternate stack
    // alone here, and so does this; the child is about to `execve`, where it
    // goes anyway.
    if flags & CLONE_CLEAR_SIGHAND != 0 {
        child.with_signals(crate::syscall::signal::Signals::reset_for_exec);
    }
    // The child's one thread, made here so that the address it is to clear
    // when it ends is recorded before it can run. It inherits the calling
    // thread's blocked mask and alternate stack.
    let caller = thread::current_of(parent);
    let thread = match &caller {
        Some(caller) => Thread::forked(&child, caller),
        None => Thread::leader(&child),
    }
    .and_then(crate::fallible::try_arc)
    .map_err(|_| Errno::ENOMEM)?;
    if flags & CLONE_CHILD_CLEARTID != 0 {
        let _ = thread.set_clear_child_tid(child_tid);
    }
    registry::publish_forked(&child, &thread);
    // Findable now, so a kill of its job that began before this line finds it,
    // and one that began after is seen here: a loop of forks cannot outrun
    // `cgroup.kill` or `job_kill` (`object::job`, "Two kills"). Ended before
    // it runs, as Linux ends a child forked into a cgroup being killed; the
    // parent, which is in the same job, is being ended too.
    if child.job().is_dying() {
        process::kill(&child, job::KILLED_STATUS);
        return Err(Errno::EAGAIN);
    }
    // And a cgroup `rmdir` removed between the check and the count, which
    // Linux's `cgroup_mutex` shuts out and this sees here instead.
    if into.is_some() && child.job().is_removed() {
        process::kill(&child, job::KILLED_STATUS);
        return Err(Errno::ENODEV);
    }

    // A failure to write either id is ignored, as Linux ignores it: the child
    // exists by now, and the addresses were the program's to get right.
    let id = pid.to_le_bytes();
    if flags & CLONE_PARENT_SETTID != 0 {
        let _ = uaccess::copy_to_user(parent.space(), parent_tid, &id);
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        let _ = uaccess::copy_to_user(child.space(), child_tid, &id);
    }

    // The child starts with its parent's registers as they are right now, in
    // this system call: its thread pointer and floating-point state, and the
    // saved frame with the return value made zero.
    // SAFETY: inside the parent's own system call, so the live user registers
    // are the parent's.
    let mut state = unsafe { arch::UserState::capture() };
    if flags & CLONE_SETTLS != 0 {
        give_thread_pointer(&mut state, tls, thread_area);
    }
    let mut child_regs = regs.for_child();
    if stack != 0 {
        child_regs.set_stack(&mut state, stack);
    }
    thread.set_resume(child_regs);

    parent.adopt(Arc::clone(&child));
    if process::start_forked(thread, state).is_err() {
        parent.disown(&child);
        return Err(Errno::EAGAIN);
    }
    if flags & CLONE_VFORK != 0 {
        if let Some(caller) = &caller {
            caller.set_in_vfork(true);
        }
        child.wait_vfork_release(parent);
        if let Some(caller) = &caller {
            caller.set_in_vfork(false);
        }
    }
    Ok(pid as usize)
}

/// Copy `parent` for a child in `into`, or in the parent's job: the copy is
/// charged to the job the child will be in (`object::quota`).
///
/// # Errors
///
/// `ENOMEM`, for memory or for that job's limits.
fn fork_into(
    parent: &Arc<Process>,
    flags: u64,
    into: Option<&Arc<Job>>,
) -> Result<Arc<Process>, Errno> {
    let own = crate::sched::running_group();
    if let Some(into) = into {
        crate::sched::set_current_group(into.quota_index());
    }
    let forked = parent.fork_memory(|space| {
        Process::forked_into(
            parent,
            space,
            flags & CLONE_FILES != 0,
            flags & CLONE_FS != 0,
            into.cloned(),
        )
    });
    if into.is_some() {
        crate::sched::set_current_group(own);
    }
    forked
        .map_err(|_| Errno::ENOMEM)?
        .map(Arc::new)
        .map_err(|_| Errno::ENOMEM)
}

/// Make a thread of `parent`'s process beside the calling thread: what `clone`
/// with `CLONE_THREAD` asks for. See [`sys_clone`].
///
/// The thread is no child: it has no exit signal, is in nobody's list of
/// children, and `wait4` never sees it. It inherits the caller's blocked mask
/// and nothing else of its signal state, resumes from a copy of the caller's
/// registers with the call answering zero, on `stack` if one is given and with
/// `tls` as its thread pointer if `CLONE_SETTLS` asks, and answers its tid to
/// the caller.
///
/// # Errors
///
/// `ENOSYS` for a thread with a descriptor table or directories of its own, or
/// with `CLONE_VFORK` or `CLONE_PIDFD`; `EAGAIN` with every id in use or the
/// process already ending; `ESRCH` from a caller that is not a thread of it.
fn clone_thread(
    parent: &Arc<Process>,
    request: &CloneRequest,
    regs: &arch::UserRegs,
) -> Result<usize, Errno> {
    let CloneRequest {
        flags,
        stack,
        parent_tid,
        child_tid,
        tls,
        thread_area,
        ..
    } = *request;
    if flags & (CLONE_FILES | CLONE_FS) != CLONE_FILES | CLONE_FS
        || flags & (CLONE_VFORK | CLONE_PIDFD) != 0
    {
        return Err(Errno::ENOSYS);
    }
    let caller = thread::current_of(parent).ok_or(Errno::ESRCH)?;
    let tid = registry::allocate_thread(parent).ok_or(Errno::EAGAIN)?;
    let thread = Thread::sibling(parent, tid, &caller).map_err(|_| {
        // Never a thread, so nothing gives its id back but this.
        registry::release_thread(tid, parent);
        Errno::ENOMEM
    })?;
    // A thread made and not shared gives its id back as it is dropped.
    let thread = crate::fallible::try_arc(thread).map_err(|_| Errno::ENOMEM)?;

    // Ignored if they fail, as for a process; see `clone_with`.
    let id = tid.to_le_bytes();
    if flags & CLONE_PARENT_SETTID != 0 {
        let _ = uaccess::copy_to_user(parent.space(), parent_tid, &id);
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        let _ = uaccess::copy_to_user(parent.space(), child_tid, &id);
    }
    if flags & CLONE_CHILD_CLEARTID != 0 {
        let _ = thread.set_clear_child_tid(child_tid);
    }

    // SAFETY: inside the caller's own system call, so the live user registers
    // are the caller's.
    let mut state = unsafe { arch::UserState::capture() };
    if flags & CLONE_SETTLS != 0 {
        give_thread_pointer(&mut state, tls, thread_area);
    }
    let mut thread_regs = regs.for_child();
    if stack != 0 {
        thread_regs.set_stack(&mut state, stack);
    }
    thread.set_resume(thread_regs);
    let _task = process::start_thread(thread, state).map_err(|_| Errno::EAGAIN)?;
    Ok(tid as usize)
}

/// Which children a wait is for, decoded from `wait4`'s `pid`.
fn wait4_selector(process: &Process, pid: i32) -> impl Fn(&Process) -> bool {
    let own_group = process.pgid();
    move |child: &Process| match pid {
        -1 => true,
        0 => child.pgid() == own_group,
        pid if pid > 0 => child.pid() == pid.unsigned_abs(),
        pid => child.pgid() == pid.unsigned_abs(),
    }
}

/// What a wait found a child to have done.
#[derive(Debug)]
enum Change {
    /// Ended.
    Ended(Arc<Process>),
    /// Stopped, for this signal.
    Stopped(Arc<Process>, u32),
    /// Continued.
    Continued(Arc<Process>),
}

impl Change {
    /// The child.
    fn child(&self) -> &Arc<Process> {
        match self {
            Change::Ended(child) | Change::Stopped(child, _) | Change::Continued(child) => child,
        }
    }
}

/// Wait until a child `select` accepts has ended (with `WEXITED`), stopped
/// (with `WUNTRACED`) or continued (with `WCONTINUED`), then take what it did
/// (or leave it, for `WNOWAIT`). `None` with `WNOHANG` when none has yet.
///
/// # Errors
///
/// `ECHILD` with no child `select` accepts; `EINTR` when a signal the caller
/// does not block arrives first, or the caller is ended.
fn wait_for_child(
    process: &Process,
    select: &dyn Fn(&Process) -> bool,
    options: u32,
    remove: bool,
) -> Result<Option<Change>, Errno> {
    let exits = options & WEXITED != 0;
    let stops = options & WUNTRACED != 0;
    let continues = options & WCONTINUED != 0;
    loop {
        let ended = process.reap_child(select, remove && exits)?;
        if exits && let Some(child) = ended {
            return Ok(Some(Change::Ended(child)));
        }
        if let Some((child, signal)) = process.changed_child(select, stops, continues, remove) {
            return Ok(Some(match signal {
                0 => Change::Continued(child),
                signal => Change::Stopped(child, signal),
            }));
        }
        if options & WNOHANG != 0 {
            return Ok(None);
        }
        if process.signal_pending() {
            // A restart code, not `EINTR`: `wait4` restarts under `SA_RESTART`.
            // The way back turns it into `EINTR` when the handler lacks it.
            return Err(Errno::ERESTARTSYS);
        }
        let _ = process.child_exited().wait_until_deadline(
            || {
                process.signal_pending()
                    || (exits && process.has_ended_child(select))
                    || process
                        .changed_child(select, stops, continues, false)
                        .is_some()
            },
            u64::MAX,
        );
    }
}

/// `wait4`.
///
/// # Errors
///
/// `EINVAL` for an unknown option; `ECHILD` with no child to wait for; `EINTR`
/// if a signal arrives or the caller is ended while it waits; `EFAULT` for a
/// bad pointer.
pub(crate) fn sys_wait4(
    process: &Process,
    pid: i32,
    wstatus: u64,
    options: u32,
    rusage: u64,
    abi: Abi,
) -> Result<usize, Errno> {
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WAIT_THREAD_BITS) != 0 {
        return Err(Errno::EINVAL);
    }
    let select = wait4_selector(process, pid);
    let Some(change) = wait_for_child(process, &select, options | WEXITED, true)? else {
        return Ok(0);
    };
    let child = change.child();
    if wstatus != 0 {
        // The status word: an exit or a killing signal as `wait_status` has
        // it; a stop as the signal in the second byte over 0x7f; a continue
        // as 0xffff.
        let status = match &change {
            Change::Ended(_) => child.wait_status().unwrap_or(0),
            Change::Stopped(_, signal) => ((*signal as i32) << 8) | 0x7f,
            Change::Continued(_) => 0xffff,
        };
        uaccess::copy_to_user(process.space(), wstatus, &status.to_le_bytes())
            .map_err(|_| Errno::EFAULT)?;
    }
    if rusage != 0 {
        zero_rusage(process, rusage, super::signal::word_of(abi))?;
    }
    Ok(child.pid() as usize)
}

/// `pidfd_open`: a descriptor for process `pid`, close-on-exec as Linux
/// always makes it, readable once the process has ended.
///
/// # Errors
///
/// `EINVAL` for a pid below one or a flag other than `PIDFD_NONBLOCK`;
/// `ESRCH` for no such process; `EMFILE` for a full table.
pub(crate) fn sys_pidfd_open(process: &Process, pid: i32, flags: u32) -> Result<usize, Errno> {
    if pid <= 0 || flags & !PIDFD_NONBLOCK != 0 {
        return Err(Errno::EINVAL);
    }
    let target = registry::find(pid.unsigned_abs()).ok_or(Errno::ESRCH)?;
    let file = crate::fs::pidfd::create(target, flags & PIDFD_NONBLOCK != 0)?;
    let fd = process.files().lock().insert(file, true)?;
    usize::try_from(fd).map_err(|_| Errno::EMFILE)
}

/// `waitid`.
///
/// # Errors
///
/// As [`sys_wait4`], and `EINVAL` without one of `WEXITED`, `WSTOPPED` and
/// `WCONTINUED`, or for an unknown `idtype`. The `siginfo` and `rusage` are
/// of `word`-byte words: an i386 program's union starts at 12, as ARMv7-A's
/// does.
pub(crate) fn sys_waitid(
    process: &Process,
    idtype: u32,
    id: u32,
    (infop, rusage): (u64, u64),
    options: u32,
    word: usize,
) -> Result<usize, Errno> {
    if options & (WEXITED | WUNTRACED | WCONTINUED) == 0
        || options & !(WNOHANG | WEXITED | WUNTRACED | WCONTINUED | WNOWAIT | WAIT_THREAD_BITS) != 0
    {
        return Err(Errno::EINVAL);
    }
    if !matches!(idtype, P_ALL | P_PID | P_PGID | P_PIDFD) {
        return Err(Errno::EINVAL);
    }
    // A pidfd's process, by its pid: a child is in the caller's list until
    // it is reaped, so the pid cannot have been given to another child yet.
    let named = if idtype == P_PIDFD {
        let file = fd::file(process, fd::arg(u64::from(id)))?;
        let pidfd = crate::fs::pidfd::of(&file).ok_or(Errno::EBADF)?;
        pidfd.process().pid()
    } else {
        id
    };
    let select = move |child: &Process| match idtype {
        P_ALL => true,
        P_PID | P_PIDFD => child.pid() == named,
        P_PGID => child.pgid() == named,
        _ => false,
    };
    let found = wait_for_child(process, &select, options, options & WNOWAIT == 0)?;
    if infop != 0 {
        let mut info = [0_u8; SIGINFO_BYTES];
        if let Some(change) = &found {
            let child = change.child();
            let (code, status) = match (change, child.ended_by_signal()) {
                (Change::Stopped(_, signal), _) => (CLD_STOPPED, *signal as i32),
                (Change::Continued(_), _) => {
                    (CLD_CONTINUED, ferrix_linux_abi::types::SIGCONT as i32)
                }
                (Change::Ended(_), Some(signal)) => (CLD_KILLED, signal as i32),
                (Change::Ended(_), None) => (CLD_EXITED, child.exit_status().unwrap_or(0) & 0xFF),
            };
            // `si_signo`, `si_errno`, `si_code`, then the union, which starts
            // at the first pointer-aligned offset: 16 on 64-bit, 12 on 32-bit.
            let union = if word == 8 { 16 } else { 12 };
            put_i32(&mut info, 0, SIGCHLD as i32)?;
            put_i32(&mut info, 8, code)?;
            put_i32(&mut info, union, child.pid() as i32)?;
            put_i32(&mut info, union + 8, status)?;
        }
        uaccess::copy_to_user(process.space(), infop, &info).map_err(|_| Errno::EFAULT)?;
    }
    if rusage != 0 {
        zero_rusage(process, rusage, word)?;
    }
    Ok(0)
}

/// Write `value` into `buffer` at `at`.
fn put_i32(buffer: &mut [u8], at: usize, value: i32) -> Result<(), Errno> {
    buffer
        .get_mut(at..at + 4)
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Write an empty `struct rusage`: eighteen words of `word` bytes -- two
/// `timeval`s of two words and fourteen `long`s, so a 32-bit program's is
/// eighteen 4-byte ones. Nothing accounts a process's resource use yet, and
/// zero is what a process that used none would be told.
fn zero_rusage(process: &Process, at: u64, word: usize) -> Result<(), Errno> {
    let bytes = [0_u8; 18 * 8];
    let size = 18 * word;
    uaccess::copy_to_user(process.space(), at, bytes.get(..size).ok_or(Errno::EINVAL)?)
        .map_err(|_| Errno::EFAULT)
}

/// `setpgid`.
///
/// # Errors
///
/// `ESRCH` if `pid` is neither the caller nor one of its children; `EINVAL`
/// for a negative group; `EPERM` for a session leader, or a group that does not
/// exist in the caller's session.
pub(crate) fn sys_setpgid(process: &Process, pid: i32, pgid: i32) -> Result<usize, Errno> {
    if pgid < 0 {
        return Err(Errno::EINVAL);
    }
    let target = if pid == 0 || pid.unsigned_abs() == process.pid() {
        None
    } else if pid > 0 && process.has_child(pid.unsigned_abs()) {
        Some(registry::find(pid.unsigned_abs()).ok_or(Errno::ESRCH)?)
    } else {
        return Err(Errno::ESRCH);
    };
    let target_process: &Process = target.as_deref().unwrap_or(process);
    let group = if pgid == 0 {
        target_process.pid()
    } else {
        pgid.unsigned_abs()
    };
    if target_process.sid() == target_process.pid() {
        return Err(Errno::EPERM);
    }
    if group != target_process.pid() {
        let exists = registry::live()?
            .iter()
            .any(|other| other.pgid() == group && other.sid() == process.sid());
        if !exists {
            return Err(Errno::EPERM);
        }
    }
    target_process.set_pgid(group);
    Ok(0)
}

/// `getpgid`: the process group of `pid`, or of the caller for zero.
///
/// # Errors
///
/// `ESRCH` for a pid no process has.
pub(crate) fn sys_getpgid(process: &Process, pid: i32) -> Result<usize, Errno> {
    if pid == 0 {
        return Ok(process.pgid() as usize);
    }
    let other = registry::find(pid.unsigned_abs()).ok_or(Errno::ESRCH)?;
    Ok(other.pgid() as usize)
}

/// `getsid`: the session of `pid`, or of the caller for zero.
///
/// # Errors
///
/// `ESRCH` for a pid no process has.
pub(crate) fn sys_getsid(process: &Process, pid: i32) -> Result<usize, Errno> {
    if pid == 0 {
        return Ok(process.sid() as usize);
    }
    let other = registry::find(pid.unsigned_abs()).ok_or(Errno::ESRCH)?;
    Ok(other.sid() as usize)
}

/// `setsid`: lead a new session and process group.
///
/// # Errors
///
/// `EPERM` if the caller already leads a process group, which is what stops a
/// group leader from leaving its members in a session it no longer belongs to.
pub(crate) fn sys_setsid(process: &Process) -> Result<usize, Errno> {
    let leads_a_group = registry::live()?
        .iter()
        .any(|other| other.pgid() == process.pid());
    if leads_a_group {
        return Err(Errno::EPERM);
    }
    process.lead_new_session();
    Ok(process.pid() as usize)
}
