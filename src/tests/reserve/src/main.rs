//! A time reservation as a Linux program asks for one, run as init by `cargo xtask
//! test-reserve` on a kernel of `sched_deadline=edf_cbs` (`docs/microkernel/K9.md`, ledger 7011 S.6).
//!
//! Each step prints `reserve: <step> ok`, ending `reserve: all ok` and status 0, or
//! `reserve: FAILED <what>` and status 1.
//!
//! * **malformed**: a deadline short of the period, and too small a budget, are `EINVAL`;
//! * **admitted**: 20 ms in every 100 ms is admitted on this thread, `sched_setattr` answers 0;
//! * **other**: another thread's is `EPERM`;
//! * **affinity**: `sched_setaffinity` is `EBUSY` while it holds it;
//! * **budget**: spinning for a second, it does about a fifth of the work it did free, with
//!   linuxd's time in that budget too;
//! * **child**: a forked child's spin is not held to the budget;
//! * **unschedulable**: with every processor's room taken by this program's threads, the
//!   next is refused `EBUSY`;
//! * **back**: `SCHED_OTHER` gives it back, and a spin is no longer held.

use std::io::Write;
use std::time::{Duration, Instant};

const SCHED_OTHER: u32 = 0;
const SCHED_DEADLINE: u32 = 6;
const MS: u64 = 1_000_000;

/// Linux's `struct sched_attr`.
#[repr(C)]
#[derive(Debug, Default)]
struct SchedAttr {
    size: u32,
    policy: u32,
    flags: u64,
    nice: i32,
    priority: u32,
    runtime: u64,
    deadline: u64,
    period: u64,
}

/// `sched_setattr(tid, attr, 0)`: the errno, 0 for none.
fn set_attr(tid: i32, policy: u32, runtime: u64, deadline: u64, period: u64) -> i32 {
    let attr = SchedAttr {
        size: size_of::<SchedAttr>() as u32,
        policy,
        runtime,
        deadline,
        period,
        ..SchedAttr::default()
    };
    // SAFETY: `attr` lives across the call, and its `size` says how much to read.
    let answer = unsafe { libc::syscall(libc::SYS_sched_setattr, tid, &raw const attr, 0_u32) };
    if answer == 0 {
        0
    } else {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    }
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "reserve: {line}");
    let _ = out.flush();
}

fn fail(what: &str) -> ! {
    say(&format!("FAILED {what}"));
    std::process::exit(1);
}

/// Count for `wall`, reading the clock between counts: how much it was let do. By work and
/// not by `CLOCK_THREAD_CPUTIME_ID`, since linuxd's count of a thread's time is its own
/// and does not see the kernel throttle it (BACKLOG, K9).
fn spin(wall: Duration) -> u64 {
    let begun = Instant::now();
    let mut count = 0_u64;
    while begun.elapsed() < wall {
        count += 1;
        std::hint::spin_loop();
    }
    count
}

/// Whether `got` over `wall_ms` is within `low` to `high` percent of `free`'s rate over
/// `free_ms`.
fn rate_within(got: u64, wall_ms: u64, free: u64, free_ms: u64, low: u64, high: u64) -> bool {
    let rate = got.saturating_mul(free_ms);
    let whole = free.saturating_mul(wall_ms);
    rate.saturating_mul(100) >= whole.saturating_mul(low)
        && rate.saturating_mul(100) <= whole.saturating_mul(high)
}

/// A thread that spins until the stop, holding `share` of a processor if it is admitted, and
/// reports whether it was.
fn holder(answers: std::sync::mpsc::Sender<i32>, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let answer = set_attr(0, SCHED_DEADLINE, 90 * MS, 100 * MS, 100 * MS);
    let _ = answers.send(answer);
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn main() {
    let free = spin(Duration::from_millis(300));
    if set_attr(0, SCHED_DEADLINE, 2 * MS, 5 * MS, 10 * MS) != libc::EINVAL
        || set_attr(0, SCHED_DEADLINE, 10_000, 10 * MS, 10 * MS) != libc::EINVAL
        || set_attr(0, SCHED_DEADLINE, 20 * MS, 10 * MS, 10 * MS) != libc::EINVAL
    {
        fail("a malformed reservation was not EINVAL");
    }
    say("malformed ok");

    let answer = set_attr(0, SCHED_DEADLINE, 20 * MS, 100 * MS, 100 * MS);
    if answer != 0 {
        fail(&format!("20 ms in 100 ms was refused: errno {answer}"));
    }
    say("admitted ok");

    // SAFETY: fork in a single-threaded program, whose child uses async-safe calls and spins.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // SAFETY: as above.
        let _ = unsafe { libc::pause() };
        std::process::exit(0);
    }
    if set_attr(child, SCHED_DEADLINE, MS, 100 * MS, 100 * MS) != libc::EPERM {
        fail("another thread's reservation was not EPERM");
    }
    say("other ok");

    // SAFETY: an all-zero set is valid, then filled.
    let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
    // SAFETY: `set` is the set above.
    unsafe { libc::CPU_SET(0, &mut set) };
    // SAFETY: `set` is the size it says.
    if unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set) } == 0 {
        fail("sched_setaffinity succeeded while reserved");
    }
    if std::io::Error::last_os_error().raw_os_error() != Some(libc::EBUSY) {
        fail("sched_setaffinity was not EBUSY while reserved");
    }
    say("affinity ok");

    let held = spin(Duration::from_millis(1_000));
    say(&format!("budget {held} counts in 1000 ms, free {free} in 300 ms"));
    if !rate_within(held, 1_000, free, 300, 12, 30) {
        fail("a 20% budget was not held to about 20%");
    }
    say("budget ok");

    // SAFETY: as the fork above.
    let spinner = unsafe { libc::fork() };
    if spinner == 0 {
        let got = spin(Duration::from_millis(400));
        std::process::exit(i32::from(!rate_within(got, 400, free, 300, 50, 200)));
    }
    let mut status = 0;
    // SAFETY: `status` is the out parameter.
    let _ = unsafe { libc::waitpid(spinner, &raw mut status, 0) };
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        fail("a forked child was held to its parent's budget");
    }
    say("child ok");

    let processors = std::thread::available_parallelism().map_or(1, usize::from);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut holders = Vec::new();
    let mut refused = false;
    for _ in 0..=processors {
        let (answers, stop) = (sender.clone(), std::sync::Arc::clone(&stop));
        holders.push(std::thread::spawn(move || holder(answers, stop)));
        match receiver.recv_timeout(Duration::from_secs(10)) {
            Ok(0) => {}
            Ok(libc::EBUSY) => {
                refused = true;
                break;
            }
            Ok(other) => fail(&format!("a wide reservation answered errno {other}")),
            Err(_) => fail("a thread asking for a reservation did not answer"),
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for holder in holders {
        let _ = holder.join();
    }
    if !refused {
        fail("an unschedulable set was admitted");
    }
    say("unschedulable ok");

    if set_attr(0, SCHED_OTHER, 0, 0, 0) != 0 {
        fail("SCHED_OTHER was refused");
    }
    let got = spin(Duration::from_millis(400));
    say(&format!("back {got} counts in 400 ms"));
    if !rate_within(got, 400, free, 300, 50, 200) {
        fail("given back, the spin was still held");
    }
    say("back ok");
    // SAFETY: the signal number and a live child.
    let _ = unsafe { libc::kill(child, libc::SIGKILL) };
    say("all ok");
    std::process::exit(0);
}
