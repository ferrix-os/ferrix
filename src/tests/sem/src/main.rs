//! System V semaphores as a program uses them, run as init by `cargo xtask
//! test-sem`: through musl's `semget`, `semop`, `semtimedop` and `semctl`,
//! which on 32-bit x86 go through `ipc` (117), as glibc's do for Steam.
//!
//! Each step prints `sem: <step> ok`, and the program ends with
//! `sem: all ok` and status 0, or `sem: FAILED <what>` and status 1.
//!
//! * **create**: a private set of two, `SETVAL` and `GETVAL`, `IPC_STAT`'s
//!   mode and `sem_nsems`;
//! * **nowait**: a decrement below zero with `IPC_NOWAIT` is `EAGAIN`;
//! * **timeout**: `semtimedop` for 50 ms on one that cannot go is `EAGAIN`,
//!   and not before 50 ms;
//! * **contention**: four forked children take and give a `SEM_UNDO` mutex
//!   200 times each around an unlocked read-modify-write of a shared
//!   counter, which must come to 800;
//! * **undo**: a child that took the mutex with `SEM_UNDO` and was killed
//!   gives it back, and is its last operator;
//! * **zero**: a child waiting for zero is counted by `GETZCNT` and let go by
//!   `SETVAL` 0;
//! * **eintr**: a child blocked in `semop` with an `SA_RESTART` handler is
//!   ended with `EINTR` by its signal;
//! * **eidrm**: a child blocked in `semop` is counted by `GETNCNT` and ended
//!   with `EIDRM` by `IPC_RMID`.
//!
//! Built with `negative-control`, the undo step's child takes the mutex
//! without `SEM_UNDO`, and the program must fail on that step.

use std::io::Write;
use std::time::{Duration, Instant};

use libc::{c_int, c_short, c_ushort, sembuf, size_t, timespec};

unsafe extern "C" {
    /// musl's `semtimedop`, which the `libc` crate does not declare.
    fn semtimedop(id: c_int, sops: *mut sembuf, nsops: size_t, timeout: *const timespec) -> c_int;
}

/// `IPC_PRIVATE`.
const IPC_PRIVATE: libc::key_t = 0;
/// `IPC_CREAT`.
const IPC_CREAT: c_int = 0o1000;
/// `IPC_NOWAIT`.
const IPC_NOWAIT: c_short = 0o4000;
/// `SEM_UNDO`.
const SEM_UNDO: c_short = 0x1000;
/// `IPC_RMID`.
const IPC_RMID: c_int = 0;
/// `IPC_STAT`.
const IPC_STAT: c_int = 2;
/// `GETPID`.
const GETPID: c_int = 11;
/// `GETVAL`.
const GETVAL: c_int = 12;
/// `GETNCNT`.
const GETNCNT: c_int = 14;
/// `GETZCNT`.
const GETZCNT: c_int = 15;
/// `SETVAL`.
const SETVAL: c_int = 16;

/// Children in the contention step, and each one's rounds.
const CHILDREN: usize = 4;
/// See [`CHILDREN`].
const ROUNDS: usize = 200;

/// Where `IPC_STAT` puts `sem_nsems` in musl's `struct semid_ds` here.
const NSEMS_AT: usize = if cfg!(target_arch = "x86_64") {
    80
} else if cfg!(target_arch = "aarch64") {
    64
} else {
    52
};

/// A step's failure.
type Step = Result<(), String>;

/// A named step.
type Named = (&'static str, fn() -> Step);

/// Print one line at once, so the serial log has it before the next step.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The last error's number.
fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A new private set of `n`, mode 0600.
fn new_set(n: c_int) -> Result<c_int, String> {
    // SAFETY: semget takes plain integers.
    let id = unsafe { libc::semget(IPC_PRIVATE, n, IPC_CREAT | 0o600) };
    if id < 0 {
        return Err(format!("semget failed, errno {}", errno()));
    }
    Ok(id)
}

/// `semctl(id, num, cmd, value)` for the commands that take an int or
/// nothing.
fn ctl(id: c_int, num: c_int, cmd: c_int, value: c_int) -> c_int {
    // SAFETY: the commands passed here read `value` as an int, or nothing.
    unsafe { libc::semctl(id, num, cmd, value) }
}

/// One operation on `id`.
fn op(id: c_int, num: u16, delta: i16, flags: c_short) -> c_int {
    let mut one = sembuf {
        sem_num: num as c_ushort,
        sem_op: delta,
        sem_flg: flags,
    };
    // SAFETY: one sembuf, alive for the call.
    unsafe { libc::semop(id, &mut one, 1) }
}

/// Fork, running `child` in the child, which ends with its answer. The pid.
fn fork_with(child: impl FnOnce() -> c_int) -> Result<libc::pid_t, String> {
    // SAFETY: the child calls only libc functions and `_exit`s.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork failed, errno {}", errno()));
    }
    if pid == 0 {
        let status = child();
        // SAFETY: ends the child without running the parent's destructors.
        unsafe { libc::_exit(status) };
    }
    Ok(pid)
}

/// Wait for `pid` and return its exit status, or 1000 plus the signal that
/// ended it.
fn reap(pid: libc::pid_t) -> c_int {
    let mut status = 0;
    // SAFETY: status is a live int.
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1000 + libc::WTERMSIG(status)
    }
}

/// Wait until `semctl(id, 0, cmd)` reads `want`, for up to five seconds.
fn wait_for(id: c_int, cmd: c_int, want: c_int) -> Step {
    let start = Instant::now();
    while ctl(id, 0, cmd, 0) != want {
        if start.elapsed() > Duration::from_secs(5) {
            return Err(format!("semctl {cmd} never read {want}"));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

/// Remove set `id`.
fn remove(id: c_int) {
    let _ = ctl(id, 0, IPC_RMID, 0);
}

/// **create**.
fn create() -> Step {
    let id = new_set(2)?;
    if ctl(id, 1, SETVAL, 5) != 0 || ctl(id, 1, GETVAL, 0) != 5 {
        return Err("SETVAL then GETVAL did not read 5".into());
    }
    let mut stat = [0_u8; 256];
    // SAFETY: stat is larger than any semid_ds.
    let answered = unsafe { libc::semctl(id, 0, IPC_STAT, stat.as_mut_ptr()) };
    let nsems = stat.get(NSEMS_AT..NSEMS_AT + 2).map_or(0, |two| {
        u16::from_le_bytes([
            two.first().copied().unwrap_or(0),
            two.get(1).copied().unwrap_or(0),
        ])
    });
    let mode = stat.get(20..22).map_or(0, |two| {
        u16::from_le_bytes([
            two.first().copied().unwrap_or(0),
            two.get(1).copied().unwrap_or(0),
        ])
    });
    remove(id);
    if answered != 0 || nsems != 2 || mode != 0o600 {
        return Err(format!(
            "IPC_STAT answered {answered}, sem_nsems {nsems}, mode {mode:o}"
        ));
    }
    Ok(())
}

/// **nowait**.
fn nowait() -> Step {
    let id = new_set(1)?;
    let answer = op(id, 0, -1, IPC_NOWAIT);
    let error = errno();
    remove(id);
    if answer != -1 || error != libc::EAGAIN {
        return Err(format!("IPC_NOWAIT answered {answer}, errno {error}"));
    }
    Ok(())
}

/// **timeout**.
fn timeout() -> Step {
    let id = new_set(1)?;
    let mut one = sembuf {
        sem_num: 0,
        sem_op: -1,
        sem_flg: 0,
    };
    let limit = timespec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    };
    let start = Instant::now();
    // SAFETY: one sembuf and one timespec, alive for the call.
    let answer = unsafe { semtimedop(id, &mut one, 1, &limit) };
    let error = errno();
    let took = start.elapsed();
    remove(id);
    if answer != -1 || error != libc::EAGAIN {
        return Err(format!("semtimedop answered {answer}, errno {error}"));
    }
    if took < Duration::from_millis(50) {
        return Err(format!("semtimedop timed out after {took:?}, before 50 ms"));
    }
    Ok(())
}

/// **contention**.
fn contention() -> Step {
    let id = new_set(1)?;
    let _ = ctl(id, 0, SETVAL, 1);
    // SAFETY: an anonymous shared page, never unmapped while used.
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if page == libc::MAP_FAILED {
        return Err("no shared page".into());
    }
    let counter = page.cast::<u64>();
    let mut children = Vec::new();
    for _ in 0..CHILDREN {
        children.push(fork_with(|| {
            for _ in 0..ROUNDS {
                if op(id, 0, -1, SEM_UNDO) != 0 {
                    return 2;
                }
                // SAFETY: the shared page, under the semaphore; read and
                // written apart, with a yield between, so only the
                // semaphore keeps the count.
                let seen = unsafe { counter.read_volatile() };
                // SAFETY: yields the processor.
                let _ = unsafe { libc::sched_yield() };
                // SAFETY: as above.
                unsafe { counter.write_volatile(seen + 1) };
                if op(id, 0, 1, SEM_UNDO) != 0 {
                    return 3;
                }
            }
            0
        })?);
    }
    let statuses: Vec<c_int> = children.into_iter().map(reap).collect();
    // SAFETY: the shared page, every child gone.
    let total = unsafe { counter.read_volatile() };
    let value = ctl(id, 0, GETVAL, 0);
    remove(id);
    if statuses.iter().any(|&status| status != 0) {
        return Err(format!("a child failed: {statuses:?}"));
    }
    if total != (CHILDREN * ROUNDS) as u64 || value != 1 {
        return Err(format!("the counter came to {total}, the mutex to {value}"));
    }
    Ok(())
}

/// **undo**.
fn undo() -> Step {
    let id = new_set(1)?;
    let _ = ctl(id, 0, SETVAL, 1);
    let flags = if cfg!(feature = "negative-control") {
        0
    } else {
        SEM_UNDO
    };
    let child = fork_with(|| {
        if op(id, 0, -1, flags) != 0 {
            return 2;
        }
        loop {
            // SAFETY: waits for a signal.
            let _ = unsafe { libc::pause() };
        }
    })?;
    wait_for(id, GETVAL, 0)?;
    // SAFETY: the child is ours.
    let _ = unsafe { libc::kill(child, libc::SIGKILL) };
    let status = reap(child);
    let value = ctl(id, 0, GETVAL, 0);
    let last = ctl(id, 0, GETPID, 0);
    remove(id);
    if status != 1000 + libc::SIGKILL {
        return Err(format!("the child ended with {status}"));
    }
    if value != 1 {
        return Err(format!(
            "a killed child's SEM_UNDO decrement was not undone: value {value}"
        ));
    }
    if last != child {
        return Err(format!(
            "the undo's last operator was {last}, not the child {child}"
        ));
    }
    Ok(())
}

/// **zero**.
fn zero() -> Step {
    let id = new_set(1)?;
    let _ = ctl(id, 0, SETVAL, 1);
    let child = fork_with(|| if op(id, 0, 0, 0) == 0 { 0 } else { errno() })?;
    wait_for(id, GETZCNT, 1)?;
    let _ = ctl(id, 0, SETVAL, 0);
    let status = reap(child);
    remove(id);
    if status != 0 {
        return Err(format!("the zero waiter ended with {status}"));
    }
    Ok(())
}

/// A handler that does nothing, for **eintr**.
extern "C" fn nothing(_signal: c_int) {}

/// **eintr**.
fn eintr() -> Step {
    let id = new_set(1)?;
    let child = fork_with(|| {
        // SAFETY: a zeroed sigaction with a handler and SA_RESTART.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = nothing as extern "C" fn(c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: a valid action.
        let _ = unsafe { libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()) };
        if op(id, 0, -1, 0) == -1 && errno() == libc::EINTR {
            44
        } else {
            errno()
        }
    })?;
    wait_for(id, GETNCNT, 1)?;
    // SAFETY: the child is ours.
    let _ = unsafe { libc::kill(child, libc::SIGUSR1) };
    let status = reap(child);
    let waiting = ctl(id, 0, GETNCNT, 0);
    remove(id);
    if status != 44 || waiting != 0 {
        return Err(format!(
            "the signalled waiter ended with {status}, {waiting} still counted"
        ));
    }
    Ok(())
}

/// **eidrm**.
fn eidrm() -> Step {
    let id = new_set(1)?;
    let child = fork_with(|| {
        if op(id, 0, -1, SEM_UNDO) == -1 && errno() == libc::EIDRM {
            43
        } else {
            errno()
        }
    })?;
    wait_for(id, GETNCNT, 1)?;
    remove(id);
    let status = reap(child);
    if status != 43 {
        return Err(format!("the waiter on a removed set ended with {status}"));
    }
    Ok(())
}

/// `semget(key, 1, flags)`, or the errno negated.
fn get(key: libc::key_t, flags: c_int) -> c_int {
    // SAFETY: plain integers.
    let id = unsafe { libc::semget(key, 1, flags) };
    if id < 0 { -errno() } else { id }
}

/// Steam's restarted client, as a host trace of it shows: a first process
/// makes a 0666 set and a 0600 mutex, holds the mutex with `SEM_UNDO`, and
/// exits; a second finds both by key, reads `GETNCNT`, `GETZCNT` and
/// `GETPID`, finds that process gone, removes the mutex and makes it again,
/// and tries the first set without waiting. Its exit status is the step
/// that failed, and the child's errno says why.
fn second_owner(first: libc::pid_t, keys: (libc::key_t, libc::key_t)) -> c_int {
    let (shared, mutex) = keys;
    if get(shared, IPC_CREAT | 0o2000 | 0o666) != -libc::EEXIST {
        return 10;
    }
    let shared_id = get(shared, 0o666);
    if shared_id < 0 {
        return 11;
    }
    if ctl(shared_id, 0, GETNCNT, 0) != 0 || ctl(shared_id, 0, GETZCNT, 0) != 0 {
        return 12;
    }
    if ctl(shared_id, 0, GETPID, 0) != first {
        return 13;
    }
    if ctl(shared_id, 0, SETVAL, 0) != 0 {
        return 14;
    }
    if get(mutex, IPC_CREAT | 0o2000 | 0o600) != -libc::EEXIST {
        return 15;
    }
    let mutex_id = get(mutex, 0o600);
    if mutex_id < 0 || ctl(mutex_id, 0, GETVAL, 0) != 1 {
        return 16;
    }
    if ctl(mutex_id, 0, GETNCNT, 0) != 0 || ctl(mutex_id, 0, GETPID, 0) != first {
        return 17;
    }
    // Reaped before this process was forked, so gone at once, as on Linux:
    // no wait for a lazy reaper (BACKLOG, done 2026-10-12).
    // SAFETY: a probe of a pid that has gone.
    let answer = unsafe { libc::kill(first, 0) };
    if answer != -1 || errno() != libc::ESRCH {
        say(&format!(
            "sem: kill(first, 0) answered {answer} after its reap"
        ));
        return 18;
    }
    if ctl(mutex_id, 0, IPC_RMID, 0) != 0 {
        return 19;
    }
    let again = get(mutex, IPC_CREAT | 0o2000 | 0o600);
    if again < 0 || ctl(again, 0, SETVAL, 1) != 0 {
        return 20;
    }
    if op(again, 0, -1, SEM_UNDO) != 0 || op(again, 0, 1, SEM_UNDO) != 0 {
        return 21;
    }
    if op(shared_id, 0, -1, IPC_NOWAIT) != -1 || errno() != libc::EAGAIN {
        return 22;
    }
    remove(again);
    remove(shared_id);
    0
}

/// **reopen**: sets made by a process that has gone, found by key and used
/// by a new one, as the Steam client's restart does.
fn reopen() -> Step {
    const SHARED: libc::key_t = 0x5e3a_17d9;
    const MUTEX: libc::key_t = 0x5e3a_813e;
    let first = fork_with(|| {
        let shared = get(SHARED, IPC_CREAT | 0o2000 | 0o666);
        let mutex = get(MUTEX, IPC_CREAT | 0o2000 | 0o600);
        if shared < 0
            || mutex < 0
            || ctl(shared, 0, SETVAL, 0) != 0
            || ctl(mutex, 0, SETVAL, 1) != 0
        {
            return 2;
        }
        if op(mutex, 0, -1, SEM_UNDO) != 0 || op(mutex, 0, 1, SEM_UNDO) != 0 {
            return 3;
        }
        // Held as it exits: the undo gives it back.
        if op(mutex, 0, -1, SEM_UNDO) != 0 {
            return 4;
        }
        42
    })?;
    let status = reap(first);
    if status != 42 {
        return Err(format!("the first owner ended with {status}"));
    }
    let second = fork_with(|| {
        let step = second_owner(first, (SHARED, MUTEX));
        if step != 0 {
            say(&format!("sem: reopen failed at {step}, errno {}", errno()));
        }
        step
    })?;
    let status = reap(second);
    if status != 0 {
        return Err(format!("the second owner failed at step {status}"));
    }
    Ok(())
}

fn main() {
    let steps: [Named; 9] = [
        ("create", create),
        ("nowait", nowait),
        ("timeout", timeout),
        ("contention", contention),
        ("undo", undo),
        ("zero", zero),
        ("eintr", eintr),
        ("eidrm", eidrm),
        ("reopen", reopen),
    ];
    for (name, step) in steps {
        if let Err(what) = step() {
            say(&format!("sem: FAILED {name}: {what}"));
            std::process::exit(1);
        }
        say(&format!("sem: {name} ok"));
    }
    say("sem: all ok");
}
