//! The **slice-request** step (L.sched.109, `docs/GAME-MODE-SCHED.md` §2.3):
//! `sched_setattr`'s `sched_runtime`, a fair thread's slice request, as
//! Linux 7.0 answers it. A request round-trips through `sched_getattr`; one
//! below 0.1 ms or above 100 ms is clamped, not refused (`__setparam_fair`);
//! an unprivileged thread cannot take `SCHED_FIFO` (`EINVAL` here, `EPERM` on
//! Linux, which has the class and refuses it without `RLIMIT_RTPRIO`);
//! `SCHED_FLAG_RESET_ON_FORK` is `EINVAL` here and taken by Linux, and the
//! step accepts either; a new thread and a fork child start with their
//! creator's request; `execve` keeps it; and a thread of another user's is
//! `EPERM`. No line prints a slice, so the step's lines are Linux's on any
//! processor count.
#![allow(unsafe_code, reason = "the step's calls are raw system calls")]

use std::thread;

/// `struct sched_attr`, `SCHED_ATTR_SIZE_VER1`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

/// `SCHED_FIFO`.
const SCHED_FIFO: u32 = 1;
/// `SCHED_FLAG_RESET_ON_FORK`.
const SCHED_FLAG_RESET_ON_FORK: u64 = 1;
/// The request the inheritance cases set: half a millisecond.
const INHERITED: u64 = 500_000;
/// The argument that makes this program the exec case's probe.
pub(crate) const EXEC_PROBE: &str = "--slice-request-probe";

/// `sched_setattr(tid, attr, 0)`: zero or the error number.
fn set(tid: i32, attr: SchedAttr) -> i32 {
    let mut attr = attr;
    attr.size = size_of::<SchedAttr>() as u32;
    // SAFETY: the kernel reads `size` bytes of a live structure.
    let answer = unsafe { libc::syscall(libc::SYS_sched_setattr, tid, &raw mut attr, 0) };
    if answer == 0 { 0 } else { errno() }
}

/// `sched_getattr` of the calling thread's `sched_runtime`.
fn runtime() -> Option<u64> {
    let mut attr = SchedAttr::default();
    let size = size_of::<SchedAttr>() as u32;
    // SAFETY: the kernel writes at most `size` bytes into a live structure.
    let answer = unsafe { libc::syscall(libc::SYS_sched_getattr, 0, &raw mut attr, size, 0) };
    (answer == 0).then_some(attr.sched_runtime)
}

/// The calling thread's error number.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Ask for `nanos` on the calling thread.
fn ask(nanos: u64) -> i32 {
    set(
        0,
        SchedAttr {
            sched_runtime: nanos,
            ..SchedAttr::default()
        },
    )
}

/// The exec case's probe: exit 0 if the request survived the exec.
pub(crate) fn probe() -> ! {
    std::process::exit(if runtime() == Some(INHERITED) { 0 } else { 1 })
}

/// A fork child that runs `body` and exits with its answer; the answer.
fn in_child(body: impl FnOnce() -> i32) -> i32 {
    // SAFETY: the child calls only async-signal-safe raw calls and `_exit`,
    // except in the exec case, where it calls `execv`.
    match unsafe { libc::fork() } {
        0 => {
            let code = body();
            // SAFETY: ends the child without running the parent's exit path.
            unsafe { libc::_exit(code) }
        }
        -1 => -1,
        child => {
            let mut status = 0;
            // SAFETY: waits for the child just made.
            let _ = unsafe { libc::waitpid(child, &raw mut status, 0) };
            if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                -1
            }
        }
    }
}

/// The step; `fail` names what went wrong and ends the program.
pub(crate) fn check_slice_request(fail: fn(&str) -> !) {
    if ask(700_000) != 0 || runtime() != Some(700_000) {
        fail("sched_setattr's request did not read back");
    }
    if ask(99_000) != 0 || runtime() != Some(100_000) {
        fail("a request below 0.1 ms was not clamped to it");
    }
    if ask(101_000_000) != 0 || runtime() != Some(100_000_000) {
        fail("a request above 100 ms was not clamped to it");
    }
    if ask(INHERITED) != 0 {
        fail("sched_setattr refused half a millisecond");
    }
    let from_thread = thread::spawn(runtime)
        .join()
        .unwrap_or_else(|_| fail("the slice thread panicked"));
    let from_fork = in_child(|| i32::from(runtime() != Some(INHERITED)));
    if from_thread != Some(INHERITED) || from_fork != 0 {
        fail("a child did not start with its creator's slice request");
    }
    // The program's own path, as it was started. Booted as init by
    // `test-threads` it came from no file (`build_kernel_with_init`), and
    // there is nothing to exec: the case is run where the program is a file,
    // as on Linux and from a disk, and says so where it is not.
    let path = std::env::args().next().unwrap_or_default();
    if std::path::Path::new(&path).is_file() {
        let exe = std::ffi::CString::new(path).unwrap_or_default();
        let flag = std::ffi::CString::new(EXEC_PROBE).unwrap_or_default();
        let exec = in_child(|| {
            let argv = [exe.as_ptr(), flag.as_ptr(), core::ptr::null()];
            // SAFETY: a null-terminated argument vector of live strings.
            let _ = unsafe { libc::execv(exe.as_ptr(), argv.as_ptr()) };
            2
        });
        match exec {
            0 => println!("threads: slice-request: kept across execve"),
            2 => fail("execve of this program failed"),
            _ => fail("execve did not keep the slice request"),
        }
    } else {
        println!("threads: slice-request: execve not run: this program came from no file");
    }
    let other = in_child(|| {
        // SAFETY: drops the child's privilege, for the permission case.
        if unsafe { libc::setuid(1000) } != 0 {
            return 3;
        }
        if set(1, SchedAttr::default()) != libc::EPERM {
            return 4;
        }
        // In this child, which ends here: Linux takes the flag from anyone
        // and then refuses to clear it without CAP_SYS_NICE, so it is set
        // where nothing needs it cleared. Ferrix refuses it.
        let reset = SchedAttr {
            sched_flags: SCHED_FLAG_RESET_ON_FORK,
            ..SchedAttr::default()
        };
        if !matches!(set(0, reset), 0 | libc::EINVAL) {
            return 6;
        }
        let fifo = SchedAttr {
            sched_policy: SCHED_FIFO,
            ..SchedAttr::default()
        };
        match set(0, fifo) {
            libc::EINVAL | libc::EPERM => 0,
            _ => 5,
        }
    });
    match other {
        0 => {}
        5 => fail("an unprivileged thread took SCHED_FIFO"),
        6 => fail("SCHED_FLAG_RESET_ON_FORK was neither taken nor EINVAL"),
        _ => fail("another user's thread took a slice request"),
    }
    if ask(0) != 0 {
        fail("sched_setattr refused to clear the request");
    }
}
