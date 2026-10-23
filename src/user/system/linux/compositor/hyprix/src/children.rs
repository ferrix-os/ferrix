//! Reaping the programs the compositor starts, and, as pid 1, every orphan.
//!
//! [`crate::state::start`] starts an `exec-once` program and does not wait
//! for it, so one that ended stayed a zombie for as long as the compositor
//! ran: a killed Chrome among them. And while the images boot the compositor
//! as pid 1 -- until init takes that place (`docs/INIT.md`, L10) -- the
//! kernel hands it every orphan too, and a pipeline's earlier stages piled up
//! under `PPid: 1`.
//!
//! `SIGCHLD` writes a byte to a pipe the event loop polls, so the loop wakes
//! when a child ends, whichever of the compositor's threads took the signal.
//! Each pass then reaps the programs it started, by pid. Only as pid 1 does it
//! reap by `-1`: otherwise the vtest renderer waits for its own
//! `virgl_test_server`, and would find it gone.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

/// The pids [`started`] was given and nobody has reaped yet.
static STARTED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// The pipe's writing end, for the handler; -1 until [`Children::watch`].
static WAKE: AtomicI32 = AtomicI32::new(-1);

/// The pipe's reading end, made once for the process: the handler is the
/// process's, whatever number of loops run in it.
static PIPE: OnceLock<OwnedFd> = OnceLock::new();

/// Remember a program just started, to reap it once it ends.
pub(crate) fn started(pid: u32) {
    if let Ok(mut started) = STARTED.lock() {
        started.push(pid);
    }
}

/// Whether started program `pid` has ended, reaping it if it has: what the
/// compositor waits on for its `exec-shutdown` programs once the loop,
/// which reaps the rest, has stopped.
pub(crate) fn ended(pid: u32) -> bool {
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return true;
    };
    let mut status = 0;
    // SAFETY: `status` is writable.
    let got = unsafe { libc::waitpid(raw, &raw mut status, libc::WNOHANG) };
    // 0 is still running; the pid is reaped now, and anything else
    // (ECHILD) was reaped already or is not this process's.
    if got == 0 {
        return false;
    }
    if let Ok(mut started) = STARTED.lock() {
        started.retain(|&known| known != pid);
    }
    true
}

/// What the event loop polls and reaps with.
pub(crate) struct Children {
    /// Whether this process is pid 1, and so every orphan's parent.
    init: bool,
}

impl Children {
    /// Make the pipe and catch `SIGCHLD` into it.
    ///
    /// # Errors
    ///
    /// When the pipe cannot be made or the handler set; the caller still
    /// reaps on every pass, only later than a child's end.
    pub(crate) fn watch() -> io::Result<Children> {
        if PIPE.get().is_none() {
            let mut fds = [0; 2];
            // SAFETY: `fds` has room for the two descriptors `pipe2` writes.
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `pipe2` succeeded, so both are open and this call's own.
            let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
            // SAFETY: as above.
            let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
            if PIPE.set(read).is_err() {
                // Another loop made it first; this pair closes here.
                drop(write);
            } else {
                // The writing end lives as long as the process, as the handler does.
                WAKE.store(write.into_raw_fd(), Ordering::Release);
            }
        }
        // SAFETY: zeroed is a valid `sigaction`; every field used is set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_child as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // Stopped children are not ends, and a restarted call is what every
        // thread here expects of a signal it did not ask about.
        action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        // SAFETY: `sa_mask` is the struct's own.
        let _ = unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
        // SAFETY: `action` is a whole `sigaction` and outlives the call.
        let set =
            unsafe { libc::sigaction(libc::SIGCHLD, &raw const action, std::ptr::null_mut()) };
        if set != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Children {
            init: std::process::id() == 1,
        })
    }

    /// The descriptor the loop polls: readable once a child has ended.
    pub(crate) fn raw_fd(&self) -> Option<RawFd> {
        PIPE.get().map(AsRawFd::as_raw_fd)
    }

    /// Empty the pipe, then reap: as pid 1 every child that has ended, and
    /// otherwise each started program that has. Gives how many were reaped.
    pub(crate) fn reap(&self) -> usize {
        if let Some(pipe) = PIPE.get() {
            let mut bytes = [0_u8; 64];
            // SAFETY: `bytes` is writable for its length; the pipe does not block.
            while unsafe { libc::read(pipe.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) }
                > 0
            {}
        }
        let mut reaped = 0;
        if self.init {
            loop {
                let mut status = 0;
                // SAFETY: `status` is writable.
                if unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) } <= 0 {
                    break;
                }
                reaped += 1;
            }
        }
        if let Ok(mut started) = STARTED.lock() {
            started.retain(|&pid| {
                let Ok(pid) = libc::pid_t::try_from(pid) else {
                    return false;
                };
                let mut status = 0;
                // SAFETY: `status` is writable.
                match unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) } {
                    0 => true,
                    got if got == pid => {
                        reaped += 1;
                        false
                    }
                    // ECHILD: reaped by `-1` above, or not this process's.
                    _ => false,
                }
            });
        }
        reaped
    }
}

/// `SIGCHLD`'s handler: one byte into the pipe, keeping `errno`, since the
/// thread it interrupted may be about to read it.
extern "C" fn on_child(_: libc::c_int) {
    let fd = WAKE.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    // SAFETY: `__errno_location` gives the calling thread's `errno`, which
    // is valid to read and write for as long as the thread runs.
    let errno = unsafe { libc::__errno_location() };
    // SAFETY: as above.
    let saved = unsafe { *errno };
    // SAFETY: `write` is async-signal-safe, and the byte outlives the call; a
    // full pipe already says a child ended.
    let _ = unsafe { libc::write(fd, [1_u8].as_ptr().cast(), 1) };
    // SAFETY: as above.
    unsafe { *errno = saved };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A started program that ends is reaped, and its end wakes the pipe.
    #[test]
    fn a_started_program_is_reaped_once_it_ends() {
        let children = Children::watch().expect("the pipe and the handler");
        let child = std::process::Command::new("true")
            .spawn()
            .expect("`true` starts");
        let pid = child.id();
        drop(child);
        started(pid);
        let fd = children.raw_fd().expect("a pipe");
        let mut reaped = 0;
        for _ in 0..500 {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one `pollfd`, alive for the call.
            let _ = unsafe { libc::poll(&raw mut poll, 1, 10) };
            reaped += children.reap();
            if !STARTED.lock().expect("not poisoned").contains(&pid) {
                break;
            }
        }
        assert!(reaped >= 1, "the program was never reaped");
        assert!(!STARTED.lock().expect("not poisoned").contains(&pid));
        // Gone from the process table, not only from the list: no zombie is left.
        // SAFETY: signal 0 only asks whether the pid exists.
        let exists = unsafe { libc::kill(libc::pid_t::try_from(pid).expect("a pid"), 0) } == 0;
        assert!(!exists, "{pid} is still in the process table");
    }
}
