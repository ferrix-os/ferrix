//! The **setuid** step: musl's `set*id` broadcast over two threads, as root.
//!
//! Linux keeps credentials per thread, so musl's `setuid` makes every thread
//! take the new ids: `__setxid` runs the system call on the calling thread
//! and then, through `__synccall`, on each of the others, which it signals and
//! holds until all have done it. If a leg fails after the first succeeded,
//! the threads' ids would differ, and musl kills the whole process with
//! `SIGKILL` rather than go on (`src/unistd/setxid.c`).
//!
//! Ferrix keeps credentials per process, a written deviation from Linux
//! (`docs/BACKLOG.md`, stage 7). The first leg already changes every thread's
//! ids, so the other legs run as uid 1000 and ask for 1000. They succeed only
//! because setting an id to one the process already has is permitted without
//! privilege. This step is the proof that the deviation is safe under musl's
//! broadcast: if a repeated leg were refused, musl would kill the process.
//!
//! The step runs in a forked child, so the test itself stays root, and the
//! child is single-threaded until it spawns the one thread the step needs.
//! The child:
//!
//! * spawns a thread that says it is alive and then blocks on a channel,
//!   which is where it is when `setuid` is called, so its leg of the
//!   broadcast interrupts a wait;
//! * calls `setuid(1000)`, which must return 0;
//! * requires `getuid` and `geteuid` to be 1000 on the main thread and, once
//!   let go, on the other;
//! * requires `setuid(0)` to fail with `EPERM` now that no id is 0.
//!
//! The parent requires the child to exit 0, and names a `SIGKILL` as musl's
//! answer to a failed leg.

#![expect(
    unsafe_code,
    reason = "setuid, getuid, geteuid, fork, waitpid and _exit are libc's, and are what this step tests"
)]

use std::sync::mpsc;
use std::thread;

/// The uid the step moves to.
const USER: libc::uid_t = 1000;

/// What `getuid` and `geteuid` answer on the calling thread.
fn ids() -> (libc::uid_t, libc::uid_t) {
    // SAFETY: neither call takes an argument or can fail.
    unsafe { (libc::getuid(), libc::geteuid()) }
}

/// Say what failed in the child and end it with status 1, without running
/// anything the parent's copy of the program owns.
fn child_fail(what: &str) -> ! {
    println!("threads: FAILED {what}");
    // SAFETY: `_exit` ends the child at once; nothing after it runs.
    unsafe { libc::_exit(1) }
}

/// The child's side of the step. Returns only by `_exit`.
fn child() -> ! {
    let (alive_tx, alive_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (ids_tx, ids_rx) = mpsc::channel();
    let other = thread::Builder::new()
        .name("setuid-other".into())
        .spawn(move || {
            let _ = alive_tx.send(());
            // Blocked here while the main thread calls setuid.
            let _ = go_rx.recv();
            let _ = ids_tx.send(ids());
        })
        .unwrap_or_else(|_| child_fail("the setuid step could not spawn its second thread"));
    if alive_rx.recv().is_err() {
        child_fail("the setuid step's second thread did not start");
    }

    // SAFETY: a plain system call wrapper; the uid is a valid one.
    let answer = unsafe { libc::setuid(USER) };
    if answer != 0 {
        println!(
            "threads: setuid({USER}) as root returned {answer}, errno {}",
            std::io::Error::last_os_error()
        );
        child_fail("setuid(1000) as root with two threads did not return 0");
    }
    let main_ids = ids();
    if main_ids != (USER, USER) {
        println!("threads: after setuid the main thread has uid, euid {main_ids:?}");
        child_fail("setuid(1000) did not leave the main thread uid and euid 1000");
    }

    if go_tx.send(()).is_err() {
        child_fail("the setuid step's second thread died during setuid");
    }
    let other_ids = ids_rx
        .recv()
        .unwrap_or_else(|_| child_fail("the setuid step's second thread did not report its ids"));
    if other_ids != (USER, USER) {
        println!("threads: after setuid the second thread has uid, euid {other_ids:?}");
        child_fail("setuid(1000) did not leave the second thread uid and euid 1000");
    }
    if other.join().is_err() {
        child_fail("the setuid step's second thread panicked");
    }

    // SAFETY: as above.
    let back = unsafe { libc::setuid(0) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    if back != -1 || errno != Some(libc::EPERM) {
        println!("threads: setuid(0) as uid {USER} returned {back}, errno {errno:?}");
        child_fail("setuid(0) after giving up root did not fail with EPERM");
    }
    // SAFETY: as in `child_fail`.
    unsafe { libc::_exit(0) }
}

/// The step: fork, run [`child`] there, and require it to exit 0.
pub(crate) fn check_setuid(fail: fn(&str) -> !) {
    // SAFETY: the process is single-threaded here -- every thread the earlier
    // steps spawned has been joined -- so the child is a whole copy of it.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail("fork for the setuid step");
    }
    if pid == 0 {
        child();
    }
    let mut status = 0;
    // SAFETY: `status` is a valid place for the answer; `pid` is our child.
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    if waited != pid {
        fail("waitpid for the setuid step's child");
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        println!("threads: the setuid step's child was killed by signal {signal}");
        if signal == libc::SIGKILL {
            fail("a leg of musl's setuid broadcast failed and musl killed the process");
        }
        fail("the setuid step's child was killed by a signal");
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        fail("the setuid step's child did not exit 0");
    }
}
