//! Process releases that reach other processes, run as init by
//! `cargo xtask test-release-chain`.
//!
//! A process's release tells its parent (its exit signal) and, for a pid
//! namespace's init, kills every member; a fatal signal ends its target, and
//! a target with no thread live is released at once, on the sender's stack.
//! `check-stack-depth` cannot bound that recursion (ledger 9275 K3), so this
//! builds the two shapes that could deepen it and lets the kernel say how
//! deep its releases nested (`release: N releases ran one inside another`):
//!
//! * **chain**: a chain of [`CHAIN`] processes, each made by the one above it
//!   with `SIGKILL` as its exit signal. The bottom one exits; each exit kills
//!   the one above, up to the first, whose parent is this program. Every
//!   process of the chain must be gone, and the first ended by `SIGKILL`;
//!   [`ROUNDS`] times.
//! * **pidns**: pid namespaces nested until the kernel refuses one more
//!   (`ENOSPC` past 32 levels, as Linux), each init with a member besides the
//!   next init. The first init is killed; every process below it must be
//!   gone.
//!
//! Each step prints `releasechain: <step> ok`, and the program ends with
//! `releasechain: all ok` and status 0, or `releasechain: FAILED <what>` and 1.

use std::io::Write;
use std::time::{Duration, Instant};

/// How many processes the chain holds below the first.
const CHAIN: usize = 256;
/// How many chains are made and ended, one after another: a release nests
/// only when a target's own exit wins a race with its killer, so more links
/// give it more chances.
const ROUNDS: usize = 4;
/// The pid namespace levels Linux allows below the first, and Ferrix with it.
const LEVELS: u8 = 32;
/// How long a step waits for what it requires.
const PATIENCE: Duration = Duration::from_secs(60);

/// A step's failure.
type Step = Result<(), String>;

/// A named step.
type Named = (&'static str, fn() -> Step);

/// Two ends of a new pipe, read end first.
fn pipe() -> Result<(i32, i32), String> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` holds the two descriptors `pipe` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }
    Ok((fds[0], fds[1]))
}

/// A `clone` that goes on as `fork` does, on a copy of this stack, with
/// `flags`: its low byte the signal the parent is sent at the child's exit.
fn clone_with(flags: libc::c_long) -> libc::pid_t {
    // SAFETY: no new stack, no thread: the child goes on with a copy of this
    // single-threaded process, as after `fork`. The other arguments are
    // zero, which every architecture's argument order reads alike.
    let pid = unsafe { libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0) };
    libc::pid_t::try_from(pid).unwrap_or(-1)
}

/// Wait in this process for good, as a link of the chain or a namespace's
/// process does, until a signal ends it.
fn hold() -> ! {
    loop {
        // SAFETY: no argument; it returns only for a caught signal.
        let _ = unsafe { libc::pause() };
    }
}

/// Write `byte` to `fd`, or end this child with status 2.
fn tell(fd: i32, byte: u8) {
    // SAFETY: one byte from a live local.
    if unsafe { libc::write(fd, core::ptr::from_ref(&byte).cast(), 1) } != 1 {
        // SAFETY: ends this child at once.
        unsafe { libc::_exit(2) };
    }
}

/// Close `fd`, which is this process's own.
fn close(fd: i32) {
    // SAFETY: a descriptor this process opened and no longer uses.
    let _ = unsafe { libc::close(fd) };
}

/// The next byte from `fd`, `None` at its end, waiting at most [`PATIENCE`].
fn next_byte(fd: i32, what: &str) -> Result<Option<u8>, String> {
    let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let millis = i32::try_from(PATIENCE.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one live `pollfd`.
    let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
    if ready <= 0 {
        return Err(format!("{what}: nothing in {} s", PATIENCE.as_secs()));
    }
    let mut byte = 0u8;
    // SAFETY: one byte into a live local.
    match unsafe { libc::read(fd, core::ptr::from_mut(&mut byte).cast(), 1) } {
        0 => Ok(None),
        1 => Ok(Some(byte)),
        _ => Err(format!("{what}: read: {}", std::io::Error::last_os_error())),
    }
}

/// Every process holding `fd`'s pipe's write end gone: its end of file.
fn all_gone(fd: i32, what: &str) -> Step {
    match next_byte(fd, what)? {
        None => Ok(()),
        Some(byte) => Err(format!("{what}: a process wrote {byte} where none should")),
    }
}

/// How `pid`, a child of this program, ended, waiting at most [`PATIENCE`].
fn reap(pid: libc::pid_t) -> Result<i32, String> {
    let start = Instant::now();
    loop {
        let mut status = 0;
        // SAFETY: one live status word.
        let got = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        if got == pid {
            return Ok(status);
        }
        if got < 0 {
            return Err(format!("waitpid: {}", std::io::Error::last_os_error()));
        }
        if start.elapsed() > PATIENCE {
            return Err(format!("pid {pid} still running after {} s", PATIENCE.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The chains: each link made with `SIGKILL` as its exit signal, the bottom
/// one exiting.
fn chain() -> Step {
    for _ in 0..ROUNDS {
        one_chain()?;
    }
    Ok(())
}

/// One chain, made and ended.
fn one_chain() -> Step {
    let (alive_r, alive_w) = pipe()?;
    let (ready_r, ready_w) = pipe()?;
    // SAFETY: one thread; the child makes system calls and exits or holds.
    let first = unsafe { libc::fork() };
    if first < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if first == 0 {
        close(alive_r);
        close(ready_r);
        for _ in 0..CHAIN {
            let child = clone_with(libc::c_long::from(libc::SIGKILL));
            if child < 0 {
                tell(ready_w, b'E');
                // SAFETY: ends this child at once.
                unsafe { libc::_exit(2) };
            }
            if child > 0 {
                hold();
            }
        }
        tell(ready_w, b'R');
        // SAFETY: the bottom of the chain ends, and with it the chain.
        unsafe { libc::_exit(0) };
    }
    close(alive_w);
    close(ready_w);
    match next_byte(ready_r, "the chain's bottom")? {
        Some(b'R') => {}
        other => return Err(format!("the chain was not made whole: {other:?}")),
    }
    say(&format!("chain of {CHAIN} made, its bottom ended"));
    let status = reap(first)?;
    if !libc::WIFSIGNALED(status) || libc::WTERMSIG(status) != libc::SIGKILL {
        return Err(format!("the chain's first process ended with status {status:#x}, not by SIGKILL"));
    }
    all_gone(alive_r, "the chain")?;
    close(alive_r);
    close(ready_r);
    Ok(())
}

/// Namespaces nested until refused, each init with a member, the first init
/// killed.
fn pidns() -> Step {
    let (alive_r, alive_w) = pipe()?;
    let (ready_r, ready_w) = pipe()?;
    let (pid_r, pid_w) = pipe()?;
    // SAFETY: one thread; the child makes system calls and holds.
    let outer = unsafe { libc::fork() };
    if outer < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if outer == 0 {
        close(alive_r);
        close(ready_r);
        close(pid_r);
        let flags = libc::c_long::from(libc::CLONE_NEWPID | libc::SIGCHLD);
        let first = clone_with(flags);
        if first > 0 {
            // Only the namespaces' processes hold the end this program
            // waits on.
            close(alive_w);
            for byte in first.to_ne_bytes() {
                tell(pid_w, byte);
            }
            hold();
        }
        if first < 0 {
            tell(ready_w, 0);
            hold();
        }
        let mut levels = 1u8;
        loop {
            // SAFETY: one thread; the member only holds.
            if unsafe { libc::fork() } == 0 {
                hold();
            }
            let next = clone_with(flags);
            if next == 0 {
                levels = levels.saturating_add(1);
                continue;
            }
            if next < 0 {
                tell(ready_w, levels);
            }
            hold();
        }
    }
    close(alive_w);
    close(ready_w);
    close(pid_w);
    let mut first = [0u8; 4];
    for byte in &mut first {
        *byte = next_byte(pid_r, "the first init's pid")?.ok_or("no first init")?;
    }
    let first = libc::pid_t::from_ne_bytes(first);
    let levels = next_byte(ready_r, "the deepest init")?.ok_or("no deepest init")?;
    if levels != LEVELS {
        return Err(format!("{levels} pid namespace levels were made, not {LEVELS}"));
    }
    say(&format!("{levels} namespace levels made, the first init killed"));
    // SAFETY: a process this program's child made.
    if unsafe { libc::kill(first, libc::SIGKILL) } != 0 {
        return Err(format!("kill: {}", std::io::Error::last_os_error()));
    }
    all_gone(alive_r, "the namespaces")?;
    // SAFETY: this program's child.
    let _ = unsafe { libc::kill(outer, libc::SIGKILL) };
    let _ = reap(outer)?;
    close(alive_r);
    close(ready_r);
    close(pid_r);
    Ok(())
}

/// A line of progress, flushed, so a step that stops shows how far it went.
fn say(what: &str) {
    println!("releasechain: {what}");
    let _ = std::io::stdout().flush();
}

fn main() {
    let steps: [Named; 2] = [("chain", chain), ("pidns", pidns)];
    for (name, step) in steps {
        if let Err(why) = step() {
            println!("releasechain: FAILED {name}: {why}");
            let _ = std::io::stdout().flush();
            std::process::exit(1);
        }
        println!("releasechain: {name} ok");
        let _ = std::io::stdout().flush();
    }
    println!("releasechain: all ok");
    let _ = std::io::stdout().flush();
}
