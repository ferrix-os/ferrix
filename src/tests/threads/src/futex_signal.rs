//! The **futex-signal** step: a timed `FUTEX_WAIT` that a handled signal
//! interrupts ends with `EINTR`, even under `SA_RESTART`, as Linux's
//! `futex_wait` answers (its restart block is resumed only when no handler
//! runs). One thread waits 2 s on a word nothing wakes; the main thread
//! sends it `SIGUSR1` 50 ms after it is asleep, whose handler has
//! `SA_RESTART`. The wait
//! must end `EINTR` in under a second. Before the fix it was restarted with
//! its whole relative timeout, and ended `ETIMEDOUT` after 2.05 s.
//!
//! Then **futex-stop**: in a forked child, a 1 s timed `FUTEX_WAIT` is
//! stopped (`SIGSTOP`) about 50 ms in and continued 600 ms later. No handler
//! runs, so the wait resumes through `restart_syscall` to its own deadline
//! and ends `ETIMEDOUT` about 1 s after it began, as on Linux; never `EINTR`
//! or a restart code. Restarted with its whole timeout it would end near
//! 1.65 s; the step wants under 1.4 s.
//!
//! `unsafe` here is the libc calls the step tests: `sigaction`, `syscall`
//! for `futex`, `gettid` and `tgkill`.
#![allow(unsafe_code, reason = "the step's calls are libc's")]

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// The word the waiter waits on, which nothing changes.
static WORD: AtomicU32 = AtomicU32::new(0);
/// The waiter's thread id, for `tgkill`.
static WAITER: AtomicI32 = AtomicI32::new(0);

/// Wait, up to 5 s, until thread `tid` of process `pid` sleeps (`S` in its
/// `stat`): in its futex wait, so a signal sent now finds it there. A signal
/// sent before, on a loaded machine, would be handled first and the wait
/// would run to its timeout, a false failure.
fn until_asleep(pid: u32, tid: i32) {
    let path = format!("/proc/{pid}/task/{tid}/stat");
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        let state = std::fs::read_to_string(&path).ok().and_then(|stat| {
            stat.rsplit_once(") ")
                .and_then(|(_, rest)| rest.chars().next())
        });
        if state == Some('S') {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// `SIGUSR1`'s handler: nothing to do.
extern "C" fn on_usr1(_: libc::c_int) {}

/// The step; `fail` names what went wrong and ends the program.
pub(crate) fn check_futex_signal(fail: fn(&str) -> !) {
    // SAFETY: a zeroed `sigaction` is valid; the handler is a plain function.
    let installed = unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = on_usr1 as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGUSR1, &raw const action, core::ptr::null_mut()) == 0
    };
    if !installed {
        fail("sigaction(SIGUSR1)");
    }
    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        // SAFETY: gettid takes nothing.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        #[allow(
            clippy::useless_conversion,
            reason = "c_long is i32 on ARMv7-A and i64 on the 64-bit architectures"
        )]
        let tid = i32::try_from(tid).unwrap_or(0);
        WAITER.store(tid, Ordering::Release);
        let _ = tx.send(());
        let timeout = libc::timespec {
            tv_sec: 2,
            tv_nsec: 0,
        };
        let began = Instant::now();
        // SAFETY: the word is a live static; the timeout lives on this stack.
        let answer = unsafe {
            libc::syscall(
                libc::SYS_futex,
                WORD.as_ptr(),
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                0_u32,
                &raw const timeout,
                core::ptr::null::<u32>(),
                0_u32,
            )
        };
        let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        (answer, error, began.elapsed())
    });
    if rx.recv().is_err() {
        fail("the waiter never started");
    }
    let pid = std::process::id();
    let tid = WAITER.load(Ordering::Acquire);
    until_asleep(pid, tid);
    thread::sleep(Duration::from_millis(50));
    // SAFETY: tgkill of a live thread of this process.
    let sent = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGUSR1) };
    if sent != 0 {
        fail("tgkill(SIGUSR1)");
    }
    let Ok((answer, error, took)) = waiter.join() else {
        fail("the waiter panicked");
    };
    println!(
        "threads: futex-signal answered {answer} errno {error} after {} ms",
        took.as_millis()
    );
    if answer != -1 || error != libc::EINTR {
        fail("a timed FUTEX_WAIT a handler interrupted did not end EINTR");
    }
    if took >= Duration::from_secs(1) {
        fail("a timed FUTEX_WAIT a handler interrupted waited on");
    }
}

/// A timed wait of one second on [`WORD`] by the calling thread: the
/// answer, its errno and how long it took.
fn timed_wait_one_second() -> (libc::c_long, i32, Duration) {
    let timeout = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let began = Instant::now();
    // SAFETY: the word is a live static; the timeout lives on this stack.
    let answer = unsafe {
        libc::syscall(
            libc::SYS_futex,
            WORD.as_ptr(),
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            0_u32,
            &raw const timeout,
            core::ptr::null::<u32>(),
            0_u32,
        )
    };
    let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    (answer, error, began.elapsed())
}

/// The futex-stop step; `fail` names what went wrong and ends the program.
pub(crate) fn check_futex_stop(fail: fn(&str) -> !) {
    // SAFETY: fork in a program whose other threads have all been joined.
    let child = unsafe { libc::fork() };
    if child < 0 {
        fail("fork for futex-stop");
    }
    if child == 0 {
        let (answer, error, took) = timed_wait_one_second();
        println!(
            "threads: futex-stop answered {answer} errno {error} after {} ms",
            took.as_millis()
        );
        let right =
            answer == -1 && error == libc::ETIMEDOUT && took < Duration::from_millis(1400);
        std::process::exit(i32::from(!right));
    }
    let pid = u32::try_from(child).unwrap_or(0);
    until_asleep(pid, child);
    thread::sleep(Duration::from_millis(50));
    // SAFETY: signals to the child this step forked.
    let stopped = unsafe { libc::kill(child, libc::SIGSTOP) };
    thread::sleep(Duration::from_millis(600));
    // SAFETY: as above.
    let continued = unsafe { libc::kill(child, libc::SIGCONT) };
    if stopped != 0 || continued != 0 {
        fail("kill(SIGSTOP/SIGCONT) of the futex-stop child");
    }
    let mut status = 0;
    // SAFETY: waitpid of the child this step forked, into a local.
    let reaped = unsafe { libc::waitpid(child, &raw mut status, 0) };
    if reaped != child || !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        fail("a stopped and continued timed FUTEX_WAIT did not resume to its own deadline");
    }
}
