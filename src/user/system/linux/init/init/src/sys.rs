//! The system calls init makes, each behind a safe function.
//!
//! Everything `unsafe` in init is here, one call per block. The functions a
//! child runs between `clone3` and `execve` ([`crate::spawn`]) allocate
//! nothing: they take C strings made before the clone, and their errors are
//! [`io::Error::last_os_error`], which is a number and not an allocation.

use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

use ferrix_svc::event::Exit;
use ferrix_svc::value::Signal;

/// Native calls (`docs/ARCHITECTURE.md` §2), made the way a Linux program
/// makes them: through the same `syscall` instruction, numbered from
/// `0x1000`. `src/lib/proto/native` describes each call as a [`Raw`] and asks this to
/// make it.
///
/// [`Raw`]: ferrix_native::Raw
#[derive(Debug, Clone, Copy)]
pub(crate) struct Native;

impl ferrix_native::Syscall for Native {
    fn call(self, raw: ferrix_native::Raw<'_>) -> usize {
        let [a0, a1, a2, a3, a4, a5] = raw.args();
        let number = libc::c_long::try_from(raw.number()).unwrap_or(-1);
        // SAFETY: `src/lib/proto/native` built `raw` from borrowed memory, and every
        // pointer among its arguments names memory it borrows for as long as
        // `raw` lives, which is past this call (`ferrix_native::call`).
        let ret = unsafe { libc::syscall(number, a0, a1, a2, a3, a4, a5) };
        if ret == -1 {
            // The C library turned the kernel's `-errno` into -1 and errno;
            // `src/lib/proto/native` decodes the kernel's form, so give it that back.
            let errno = io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO);
            (-isize::try_from(errno).unwrap_or(isize::MAX)).cast_unsigned()
        } else {
            usize::try_from(ret).unwrap_or(usize::MAX)
        }
    }
}

/// `clone3` (`asm-generic/unistd.h`, `asm-x86/unistd_64.h` and
/// `asm-arm/unistd-common.h` all give 435).
const SYS_CLONE3: libc::c_long = 435;

/// `clone3` only: start the child in the cgroup `cgroup` names
/// (`linux/sched.h`).
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// `struct clone_args`, as far as `cgroup` (`CLONE_ARGS_SIZE_VER2`).
#[repr(C)]
#[derive(Debug, Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// The value a call returned, or the error it set.
fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// A descriptor a call just returned, owned.
fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    let fd = check(fd)?;
    // SAFETY: the call returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A signal set holding `signals`.
fn set_of(signals: &[libc::c_int]) -> libc::sigset_t {
    let mut set = MaybeUninit::<libc::sigset_t>::zeroed();
    // SAFETY: `set` is a valid, writable signal set.
    let _ = unsafe { libc::sigemptyset(set.as_mut_ptr()) };
    for &signal in signals {
        // SAFETY: as above; an invalid signal number only fails the call.
        let _ = unsafe { libc::sigaddset(set.as_mut_ptr(), signal) };
    }
    // SAFETY: `sigemptyset` initialised it.
    unsafe { set.assume_init() }
}

/// Block `signals`, so they arrive only through a signalfd, and return the
/// set.
pub(crate) fn block(signals: &[libc::c_int]) -> io::Result<libc::sigset_t> {
    let set = set_of(signals);
    // SAFETY: `set` is initialised; the old mask is not wanted.
    let ret = unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, ptr::null_mut()) };
    check(ret).map(|_| set)
}

/// Unblock every signal: the child's first step, so the program it becomes
/// starts with the mask a program expects.
pub(crate) fn unblock_all() -> io::Result<()> {
    let set = set_of(&[]);
    // SAFETY: `set` is initialised and empty.
    let ret = unsafe { libc::sigprocmask(libc::SIG_SETMASK, &set, ptr::null_mut()) };
    check(ret).map(drop)
}

/// Set `signal`'s disposition to `handler`: `SIG_IGN` or `SIG_DFL`.
pub(crate) fn disposition(signal: libc::c_int, handler: libc::sighandler_t) {
    // SAFETY: `SIG_IGN` and `SIG_DFL` are not functions, so nothing runs; a
    // signal that cannot be changed (`SIGKILL`, `SIGSTOP`) only fails.
    let _ = unsafe { libc::signal(signal, handler) };
}

/// A signalfd for `set`, non-blocking.
pub(crate) fn signalfd(set: &libc::sigset_t) -> io::Result<OwnedFd> {
    // SAFETY: `set` is initialised; -1 asks for a new descriptor.
    owned(unsafe { libc::signalfd(-1, set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) })
}

/// The next signal a signalfd holds, if any.
pub(crate) fn next_signal(fd: BorrowedFd<'_>) -> io::Result<Option<u32>> {
    let mut info = MaybeUninit::<libc::signalfd_siginfo>::zeroed();
    let size = size_of::<libc::signalfd_siginfo>();
    // SAFETY: `info` is writable for `size` bytes.
    let got = unsafe { libc::read(fd.as_raw_fd(), info.as_mut_ptr().cast(), size) };
    if got < 0 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::WouldBlock => Ok(None),
            _ => Err(error),
        };
    }
    if usize::try_from(got).ok() != Some(size) {
        return Ok(None);
    }
    // SAFETY: the kernel wrote a whole `signalfd_siginfo`.
    Ok(Some(unsafe { info.assume_init() }.ssi_signo))
}

/// A new epoll set.
pub(crate) fn epoll() -> io::Result<OwnedFd> {
    // SAFETY: no pointers.
    owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })
}

/// Watch `fd` in `epoll` for `events`, reported with `token`.
pub(crate) fn watch(epoll: BorrowedFd<'_>, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    let mut event = libc::epoll_event { events, u64: token };
    // SAFETY: `event` is a valid `epoll_event`.
    let ret = unsafe { libc::epoll_ctl(epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut event) };
    check(ret).map(drop)
}

/// Stop watching `fd`.
pub(crate) fn unwatch(epoll: BorrowedFd<'_>, fd: RawFd) {
    // SAFETY: a null event is allowed for `EPOLL_CTL_DEL`.
    let ret =
        unsafe { libc::epoll_ctl(epoll.as_raw_fd(), libc::EPOLL_CTL_DEL, fd, ptr::null_mut()) };
    let _ = check(ret);
}

/// Wait up to `timeout` milliseconds (-1 for ever) and return the tokens
/// that are ready, with their events.
pub(crate) fn wait(epoll: BorrowedFd<'_>, timeout: i32) -> io::Result<Vec<(u32, u64)>> {
    const MAX: usize = 32;
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; MAX];
    loop {
        // SAFETY: `events` is writable for `MAX` entries.
        let ret = unsafe {
            libc::epoll_wait(
                epoll.as_raw_fd(),
                events.as_mut_ptr(),
                MAX as libc::c_int,
                timeout,
            )
        };
        match check(ret) {
            Ok(count) => {
                let count = usize::try_from(count).unwrap_or(0);
                return Ok(events
                    .iter()
                    .take(count)
                    .map(|event| (event.events, event.u64))
                    .collect());
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// A new inotify instance, non-blocking, for device units' nodes.
pub(crate) fn inotify() -> io::Result<OwnedFd> {
    // SAFETY: no pointers.
    owned(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) })
}

/// `inotify_add_watch(2)`: watch `path` for `mask`; its watch descriptor.
pub(crate) fn inotify_add(fd: RawFd, path: &CStr, mask: u32) -> io::Result<i32> {
    // SAFETY: `path` is NUL-terminated.
    check(unsafe { libc::inotify_add_watch(fd, path.as_ptr(), mask) })
}

/// `inotify_rm_watch(2)`. A watch the kernel has dropped already only
/// fails, which is ignored.
pub(crate) fn inotify_rm(fd: RawFd, wd: i32) {
    // SAFETY: no pointers.
    let _ = unsafe { libc::inotify_rm_watch(fd, wd) };
}

/// Reap one child that has ended, if any has.
pub(crate) fn reap() -> io::Result<Option<(u32, Exit)>> {
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is writable.
    let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
    if pid < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ECHILD) => Ok(None),
            _ => Err(error),
        };
    }
    if pid == 0 {
        return Ok(None);
    }
    let how = if libc::WIFSIGNALED(status) {
        Exit::Signal {
            signal: Signal(u8::try_from(libc::WTERMSIG(status)).unwrap_or(0)),
            core: libc::WCOREDUMP(status),
        }
    } else {
        Exit::Code(libc::WEXITSTATUS(status))
    };
    Ok(Some((pid.unsigned_abs(), how)))
}

/// Wait for one particular child, for up to `patience` polls of 10 ms, and
/// say how it ended; `None` when it had not ended by then.
pub(crate) fn wait_for(pid: u32, patience: u32) -> Option<Exit> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    for _ in 0..patience {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is writable.
        let got = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if got == pid {
            return Some(if libc::WIFSIGNALED(status) {
                Exit::Signal {
                    signal: Signal(u8::try_from(libc::WTERMSIG(status)).unwrap_or(0)),
                    core: libc::WCOREDUMP(status),
                }
            } else {
                Exit::Code(libc::WEXITSTATUS(status))
            });
        }
        if got < 0 {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    None
}

/// Nanoseconds on `CLOCK_MONOTONIC`.
pub(crate) fn monotonic() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is writable.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// `mount(2)`.
pub(crate) fn mount(
    source: &CStr,
    target: &CStr,
    fs_type: &CStr,
    flags: libc::c_ulong,
    data: Option<&CStr>,
) -> io::Result<()> {
    let data = data.map_or(ptr::null(), |data| data.as_ptr().cast());
    // SAFETY: every pointer is a NUL-terminated string or null.
    let ret = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fs_type.as_ptr(),
            flags,
            data,
        )
    };
    check(ret).map(drop)
}

/// `umount2(2)`.
pub(crate) fn unmount(target: &CStr) -> io::Result<()> {
    // SAFETY: `target` is NUL-terminated.
    check(unsafe { libc::umount2(target.as_ptr(), 0) }).map(drop)
}

/// `kill(2)`.
pub(crate) fn kill(pid: u32, signal: libc::c_int) -> io::Result<()> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?;
    // SAFETY: no pointers.
    check(unsafe { libc::kill(pid, signal) }).map(drop)
}

/// `PR_SET_CHILD_SUBREAPER` (§5.7).
pub(crate) fn subreaper() -> io::Result<()> {
    // SAFETY: no pointers.
    check(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }).map(drop)
}

/// `sync(2)`.
pub(crate) fn sync() {
    // SAFETY: no arguments.
    unsafe { libc::sync() };
}

/// `reboot(2)`, which returns only when it fails.
pub(crate) fn reboot(command: libc::c_int) -> io::Error {
    // SAFETY: no pointers.
    let _ = unsafe { libc::reboot(command) };
    io::Error::last_os_error()
}

/// The lowest descriptor a pipe of init's is moved to, above any a unit
/// names (`NotifyFd=`), so a child's `dup2` onto one never lands on another.
const HIGH_FD: libc::c_int = 100;

/// A copy of `fd` at [`HIGH_FD`] or above, closed on `execve`; `fd` is
/// closed.
fn high(fd: OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: no pointers.
    owned(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, HIGH_FD) })
}

/// A copy of `fd`, which stays init's, at [`HIGH_FD`] or above.
pub(crate) fn duplicate_high(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: no pointers.
    owned(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, HIGH_FD) })
}

/// The calling process's pid, from the kernel: the child's own, after
/// `clone3`.
pub(crate) fn own_pid() -> u32 {
    // SAFETY: no arguments.
    unsafe { libc::getpid() }.unsigned_abs()
}

/// A pipe whose ends close on `execve`, both at [`HIGH_FD`] or above.
pub(crate) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is writable for two descriptors.
    let _ = check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    let [read, write] = fds;
    // SAFETY: `pipe2` returned two new descriptors that nothing else owns.
    let read = unsafe { OwnedFd::from_raw_fd(read) };
    // SAFETY: as above.
    let write = unsafe { OwnedFd::from_raw_fd(write) };
    Ok((high(read)?, high(write)?))
}

/// Make `fd` non-blocking.
pub(crate) fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: no pointers.
    let flags = check(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    // SAFETY: no pointers.
    check(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }).map(drop)
}

/// The uid of the process at the other end of a Unix socket.
pub(crate) fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut size = libc::socklen_t::try_from(size_of::<libc::ucred>()).unwrap_or(0);
    // SAFETY: `credentials` is writable for `size` bytes.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            ptr::from_mut(&mut credentials).cast(),
            &mut size,
        )
    };
    check(ret).map(|_| credentials.uid)
}

/// Change what `fd` is watched for.
pub(crate) fn rewatch(epoll: BorrowedFd<'_>, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    let mut event = libc::epoll_event { events, u64: token };
    // SAFETY: `event` is a valid `epoll_event`.
    let ret = unsafe { libc::epoll_ctl(epoll.as_raw_fd(), libc::EPOLL_CTL_MOD, fd, &mut event) };
    check(ret).map(drop)
}

/// Which side of a `clone3` this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Forked {
    /// The child, which must end in `execve` or `_exit`.
    Child,
    /// The parent, with the child's pid.
    Parent(u32),
}

/// `clone3` as `fork`, with the child in the cgroup `cgroup` is open on
/// from its first instruction (§5.2).
pub(crate) fn fork_into(cgroup: BorrowedFd<'_>) -> io::Result<Forked> {
    let args = CloneArgs {
        flags: CLONE_INTO_CGROUP,
        exit_signal: libc::SIGCHLD as u64,
        cgroup: u64::try_from(cgroup.as_raw_fd()).unwrap_or(0),
        ..CloneArgs::default()
    };
    // SAFETY: `args` is a valid `clone_args` of the size given. With no
    // `CLONE_VM` and no stack the child runs on a copy of the address space,
    // as after `fork`; init has one thread, so no lock is held in the copy.
    let ret = unsafe { libc::syscall(SYS_CLONE3, ptr::from_ref(&args), size_of::<CloneArgs>()) };
    match ret {
        ..0 => Err(io::Error::last_os_error()),
        0 => Ok(Forked::Child),
        pid => Ok(Forked::Parent(u32::try_from(pid).unwrap_or(0))),
    }
}

/// `fork(2)`, for a generator, which runs before any cgroup is made, and for
/// the helper that makes a native service as its user.
pub(crate) fn fork() -> io::Result<Forked> {
    // SAFETY: init has one thread, so the child's copy holds no lock.
    match unsafe { libc::fork() } {
        ..0 => Err(io::Error::last_os_error()),
        0 => Ok(Forked::Child),
        pid => Ok(Forked::Parent(pid.unsigned_abs())),
    }
}

// What a child calls between `clone3` and `execve`. None allocates.

/// `setsid(2)`.
pub(crate) fn setsid() -> io::Result<()> {
    // SAFETY: no arguments.
    check(unsafe { libc::setsid() }).map(drop)
}

/// `open(2)`, returning the raw descriptor for the child to `dup2`.
pub(crate) fn open(path: &CStr, flags: libc::c_int) -> io::Result<RawFd> {
    // SAFETY: `path` is NUL-terminated; the mode is read only with `O_CREAT`.
    check(unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC, 0o644) })
}

/// `dup2(2)`, leaving the copy open across `execve`.
pub(crate) fn dup2(from: RawFd, to: RawFd) -> io::Result<()> {
    if from == to {
        // `dup2` onto itself keeps `FD_CLOEXEC`, so clear it instead.
        // SAFETY: no pointers.
        return check(unsafe { libc::fcntl(from, libc::F_SETFD, 0) }).map(drop);
    }
    // SAFETY: no pointers.
    check(unsafe { libc::dup2(from, to) }).map(drop)
}

/// `TIOCSCTTY`: make the terminal on `fd` the caller's controlling
/// terminal, taking it from another session when `force`.
pub(crate) fn take_terminal(fd: RawFd, force: bool) -> io::Result<()> {
    // SAFETY: `TIOCSCTTY` takes an integer, not a pointer.
    check(unsafe { libc::ioctl(fd, libc::TIOCSCTTY, libc::c_int::from(force)) }).map(drop)
}

/// `chdir(2)`.
pub(crate) fn chdir(path: &CStr) -> io::Result<()> {
    // SAFETY: `path` is NUL-terminated.
    check(unsafe { libc::chdir(path.as_ptr()) }).map(drop)
}

/// Become `uid`, `gid` and `groups`, in the order that leaves no way back.
pub(crate) fn become_user(uid: u32, gid: u32, groups: &[u32]) -> io::Result<()> {
    // SAFETY: `groups` is readable for its length.
    let _ = check(unsafe { libc::setgroups(groups.len(), groups.as_ptr()) })?;
    // SAFETY: no pointers.
    let _ = check(unsafe { libc::setgid(gid) })?;
    // SAFETY: no pointers.
    check(unsafe { libc::setuid(uid) }).map(drop)
}

/// `unshare(2)`: leave the namespaces `flags` names for new copies.
pub(crate) fn unshare(flags: libc::c_int) -> io::Result<()> {
    // SAFETY: no pointers.
    check(unsafe { libc::unshare(flags) }).map(drop)
}

/// `mkdir(2)`; a directory that is there already is not an error.
pub(crate) fn make_directory(path: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: `path` is NUL-terminated.
    match check(unsafe { libc::mkdir(path.as_ptr(), mode) }) {
        Ok(_) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        Err(error) => Err(error),
    }
}

/// `chmod(2)`.
pub(crate) fn change_mode(path: &CStr, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: `path` is NUL-terminated.
    check(unsafe { libc::chmod(path.as_ptr(), mode) }).map(drop)
}

/// `struct ifreq`'s size on every architecture init is built for: the
/// interface's name in 16 bytes, then a 24-byte union whose first two bytes
/// are `ifr_flags`.
const IFREQ: usize = 40;

/// Bring the loopback interface of the caller's network namespace up, as
/// `ip link set lo up` does: read its flags, add `IFF_UP`, write them back
/// through a datagram socket made and closed here. Allocates nothing.
pub(crate) fn loopback_up() -> io::Result<()> {
    // SAFETY: no pointers.
    let fd =
        owned(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
    let mut request = [0_u8; IFREQ];
    for (slot, byte) in request.iter_mut().zip(b"lo") {
        *slot = *byte;
    }
    // SAFETY: `request` is a writable `struct ifreq` for the call's length.
    let _ = check(unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            libc::SIOCGIFFLAGS as _,
            request.as_mut_ptr(),
        )
    })?;
    let flags = match request.get(16..18) {
        Some(&[low, high]) => u16::from_ne_bytes([low, high]),
        _ => 0,
    };
    let up = flags | u16::try_from(libc::IFF_UP).unwrap_or(1);
    for (slot, byte) in request.iter_mut().skip(16).zip(up.to_ne_bytes()) {
        *slot = byte;
    }
    // SAFETY: as above; the kernel only reads it.
    check(unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            libc::SIOCSIFFLAGS as _,
            request.as_mut_ptr(),
        )
    })
    .map(drop)
}

/// `SECCOMP_SET_MODE_FILTER` (`linux/seccomp.h`).
const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;

/// `seccomp(SECCOMP_SET_MODE_FILTER, 0, &prog)`: install `filter` for the
/// caller and everything it starts. Allocates nothing.
pub(crate) fn install_filter(filter: &[libc::sock_filter]) -> io::Result<()> {
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: `program` names `filter`, which outlives the call; the kernel
    // only reads both.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            0_u32,
            ptr::from_ref(&program),
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `prctl(PR_SET_NO_NEW_PRIVS, 1)`: no `execve` from here on gains a
/// privilege, for this process and everything it starts.
pub(crate) fn no_new_privileges() -> io::Result<()> {
    // SAFETY: no pointers; the unused arguments are 0, as the call insists.
    check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }).map(drop)
}

/// Block until `fd` has a byte or its writer has gone, reading the byte:
/// the child's wait for init's go-ahead.
pub(crate) fn wait_readable(fd: RawFd) {
    let mut byte = [0_u8; 1];
    loop {
        // SAFETY: `byte` is writable for one byte.
        let got = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        if got >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// `execve(2)`, which returns only when it fails.
pub(crate) fn execve(
    path: &CStr,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
) -> io::Error {
    // SAFETY: both arrays end in a null pointer and hold NUL-terminated
    // strings that outlive the call.
    let _ = unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    io::Error::last_os_error()
}

/// `write(2)` of the whole of `bytes`, once, ignoring failure: the child's
/// last word to its parent.
pub(crate) fn write_once(fd: RawFd, bytes: &[u8]) {
    // SAFETY: `bytes` is readable for its length.
    let _ = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
}

/// `_exit(2)`: end the child without running anything of the parent's.
pub(crate) fn exit_now(status: libc::c_int) -> ! {
    // SAFETY: ends the process.
    unsafe { libc::_exit(status) }
}
