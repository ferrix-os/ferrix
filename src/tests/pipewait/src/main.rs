//! A process blocked in a pipe's read, ended by each thing outside the pipe
//! that can end it, run as init by `cargo xtask test-pipewait`.
//!
//! A pipe's waits trust their queues (po10-pipe P3): no recheck ends a wait a
//! waker forgot, so a missing wake hangs the program for good. The kernel's
//! boot check covers the pipe's own wakers; this covers the ones that reach a
//! waiter through its signal state (the consultant's C6). Each step forks a
//! child that blocks in `read` of an empty pipe, and requires within two
//! seconds:
//!
//! * **eintr**: a caught `SIGUSR1` without `SA_RESTART` ends the read with
//!   `EINTR`;
//! * **restart**: with `SA_RESTART` the read restarts, still waits, and then
//!   reads what is written;
//! * **kill**: `SIGKILL` ends the child, and `wait4` says so;
//! * **stop**: `SIGSTOP` stops it (`WUNTRACED`), `SIGCONT` continues it, its
//!   read still waits, and then reads what is written;
//! * **freeze**: frozen through `cgroup.freeze`, it does not read what is
//!   written while frozen, and reads it once thawed.
//!
//! Each step prints `pipewait: <step> ok`, and the program ends with
//! `pipewait: all ok` and status 0, or `pipewait: FAILED <what>` and 1.

use std::io::Write;
use std::time::{Duration, Instant};

/// How long a step waits for what it requires.
const PATIENCE: Duration = Duration::from_secs(2);
/// How long a child is given to block in its read.
const SETTLE: Duration = Duration::from_millis(100);
/// The cgroup the freeze step uses.
const CGROUP: &str = "/sys/fs/cgroup/pipewait";

/// A step's failure.
type Step = Result<(), String>;

/// A named step.
type Named = (&'static str, fn() -> Step);

/// A child blocked in a read, and the pipes it was given.
struct Reader {
    /// Its pid.
    pid: libc::pid_t,
    /// Where the parent writes what the child reads.
    data: i32,
    /// Where the child writes what its read answered.
    result: i32,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader").field("pid", &self.pid).finish()
    }
}

/// The handler for `SIGUSR1`: nothing but the interruption.
extern "C" fn on_usr1(_: i32) {}

/// Two ends of a new pipe, read end first.
fn pipe() -> Result<(i32, i32), String> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` holds the two descriptors `pipe` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }
    Ok((fds[0], fds[1]))
}

/// Fork a child that, with a `SIGUSR1` handler if `handler` (`SA_RESTART`
/// if `restart`), reads eight bytes from a pipe and reports what the read
/// answered: `D` and the count, or `E` and the errno.
fn reader(handler: bool, restart: bool) -> Result<Reader, String> {
    let (data_r, data_w) = pipe()?;
    let (result_r, result_w) = pipe()?;
    // SAFETY: one thread; the child only makes system calls and exits.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        if handler {
            // SAFETY: a zeroed sigaction with a handler and the flags asked.
            unsafe {
                let mut action: libc::sigaction = core::mem::zeroed();
                action.sa_sigaction = on_usr1 as *const () as usize;
                action.sa_flags = if restart { libc::SA_RESTART } else { 0 };
                let _ = libc::sigaction(libc::SIGUSR1, &raw const action, core::ptr::null_mut());
            }
        }
        let mut buf = [0u8; 8];
        // SAFETY: `buf` is writable for its length.
        let n = unsafe { libc::read(data_r, buf.as_mut_ptr().cast(), buf.len()) };
        let report = if n < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            [b'E', u8::try_from(errno).unwrap_or(255)]
        } else {
            [b'D', u8::try_from(n).unwrap_or(255)]
        };
        // SAFETY: `report` is readable for its length; the child then ends.
        unsafe {
            let _ = libc::write(result_w, report.as_ptr().cast(), report.len());
            libc::_exit(0);
        }
    }
    // SAFETY: closing the parent's copies of the child's ends.
    unsafe {
        let _ = libc::close(data_r);
        let _ = libc::close(result_w);
    }
    std::thread::sleep(SETTLE);
    let reader = Reader {
        pid,
        data: data_w,
        result: result_r,
    };
    if answered(&reader, Duration::ZERO).is_some() {
        return Err("the child's read of an empty pipe did not wait".to_owned());
    }
    Ok(reader)
}

/// What the child's read answered, if it did within `within`.
fn answered(reader: &Reader, within: Duration) -> Option<[u8; 2]> {
    let mut poll = libc::pollfd {
        fd: reader.result,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(within.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one pollfd, valid for the call.
    if unsafe { libc::poll(&raw mut poll, 1, millis) } <= 0 {
        return None;
    }
    let mut report = [0u8; 2];
    // SAFETY: `report` is writable for its length.
    let n = unsafe { libc::read(reader.result, report.as_mut_ptr().cast(), report.len()) };
    (n == 2).then_some(report)
}

/// Send `signal` to the child.
fn signal(reader: &Reader, signal: i32) -> Step {
    // SAFETY: a signal to our own child.
    if unsafe { libc::kill(reader.pid, signal) } != 0 {
        return Err(format!("kill: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Write eight bytes for the child to read.
fn feed(reader: &Reader) -> Step {
    // SAFETY: eight readable bytes.
    if unsafe { libc::write(reader.data, b"pipewait".as_ptr().cast(), 8) } != 8 {
        return Err(format!("write: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// `wait4`'s status for the child, with `flags`, within the patience.
fn status(reader: &Reader, flags: i32) -> Option<i32> {
    let start = Instant::now();
    while start.elapsed() < PATIENCE {
        let mut status = 0;
        // SAFETY: `status` is writable; the pid is our child's.
        let got = unsafe { libc::waitpid(reader.pid, &raw mut status, flags | libc::WNOHANG) };
        if got == reader.pid {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

/// Reap the child and close its pipes.
fn done(reader: Reader) -> Step {
    let ended = status(&reader, 0);
    // SAFETY: closing our own descriptors.
    unsafe {
        let _ = libc::close(reader.data);
        let _ = libc::close(reader.result);
    }
    match ended {
        Some(_) => Ok(()),
        None => Err("the child did not end".to_owned()),
    }
}

/// Require the child's read to answer `want` within the patience.
fn expect(reader: &Reader, want: [u8; 2], what: &str) -> Step {
    match answered(reader, PATIENCE) {
        Some(got) if got == want => Ok(()),
        Some(got) => Err(format!("{what}: the read answered {got:?}, not {want:?}")),
        None => Err(format!("{what}: the read never answered: a missing wake")),
    }
}

/// eintr.
fn eintr() -> Step {
    let reader = reader(true, false)?;
    signal(&reader, libc::SIGUSR1)?;
    expect(
        &reader,
        [b'E', u8::try_from(libc::EINTR).unwrap_or(0)],
        "a caught signal",
    )?;
    done(reader)
}

/// restart.
fn restart() -> Step {
    let reader = reader(true, true)?;
    signal(&reader, libc::SIGUSR1)?;
    std::thread::sleep(SETTLE);
    if answered(&reader, Duration::ZERO).is_some() {
        return Err("a read with an SA_RESTART handler did not restart".to_owned());
    }
    feed(&reader)?;
    expect(&reader, [b'D', 8], "a restarted read")?;
    done(reader)
}

/// kill.
fn kill() -> Step {
    let reader = reader(false, false)?;
    signal(&reader, libc::SIGKILL)?;
    let status = status(&reader, 0).ok_or("SIGKILL did not end a child blocked in read")?;
    if !libc::WIFSIGNALED(status) || libc::WTERMSIG(status) != libc::SIGKILL {
        return Err(format!("the killed child's status was {status:#x}"));
    }
    // SAFETY: closing our own descriptors.
    unsafe {
        let _ = libc::close(reader.data);
        let _ = libc::close(reader.result);
    }
    Ok(())
}

/// stop.
fn stop() -> Step {
    let reader = reader(false, false)?;
    signal(&reader, libc::SIGSTOP)?;
    let stopped = status(&reader, libc::WUNTRACED).ok_or("SIGSTOP did not stop a child in read")?;
    if !libc::WIFSTOPPED(stopped) {
        return Err(format!("the stopped child's status was {stopped:#x}"));
    }
    signal(&reader, libc::SIGCONT)?;
    std::thread::sleep(SETTLE);
    if answered(&reader, Duration::ZERO).is_some() {
        return Err("a continued read did not wait again".to_owned());
    }
    feed(&reader)?;
    expect(&reader, [b'D', 8], "a read stopped and continued")?;
    done(reader)
}

/// Write `value` into the cgroup's `file`.
fn cgroup_write(file: &str, value: &str) -> Step {
    std::fs::write(format!("{CGROUP}/{file}"), value)
        .map_err(|error| format!("writing {file}: {error}"))
}

/// freeze.
fn freeze() -> Step {
    for (source, target, kind) in [
        (c"sys", c"/sys", c"sysfs"),
        (c"cgroup2", c"/sys/fs/cgroup", c"cgroup2"),
    ] {
        // SAFETY: NUL-terminated strings, no data; a mount there is as good.
        let _ = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                kind.as_ptr(),
                0,
                core::ptr::null(),
            )
        };
    }
    let _ = std::fs::create_dir(CGROUP);
    let reader = reader(false, false)?;
    cgroup_write("cgroup.procs", &reader.pid.to_string())?;
    cgroup_write("cgroup.freeze", "1")?;
    let start = Instant::now();
    loop {
        let events = std::fs::read_to_string(format!("{CGROUP}/cgroup.events")).unwrap_or_default();
        if events.lines().any(|line| line.trim() == "frozen 1") {
            break;
        }
        if start.elapsed() > PATIENCE {
            return Err("the cgroup with a child blocked in read never froze".to_owned());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    feed(&reader)?;
    std::thread::sleep(SETTLE);
    if answered(&reader, Duration::ZERO).is_some() {
        return Err("a frozen child read what was written".to_owned());
    }
    cgroup_write("cgroup.freeze", "0")?;
    expect(&reader, [b'D', 8], "a read frozen and thawed")?;
    done(reader)
}

fn main() {
    let steps: [Named; 5] = [
        ("eintr", eintr),
        ("restart", restart),
        ("kill", kill),
        ("stop", stop),
        ("freeze", freeze),
    ];
    for (name, step) in steps {
        if let Err(why) = step() {
            println!("pipewait: FAILED {name}: {why}");
            let _ = std::io::stdout().flush();
            std::process::exit(1);
        }
        println!("pipewait: {name} ok");
    }
    println!("pipewait: all ok");
    let _ = std::io::stdout().flush();
}
