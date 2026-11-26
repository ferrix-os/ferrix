//! The **sched-policy** step, built only for a `schedule=windows` kernel (the
//! `windows` feature, which `cargo xtask test-threads` sets from the run's
//! flags): linuxd's answers under windows (`docs/microkernel/M5-LIFT.md`,
//! ledger 7460 WN.10, 7540 WX.8). `SCHED_OTHER`, `SCHED_BATCH` and
//! `SCHED_IDLE` are accepted by `sched_setscheduler` and by `sched_setattr`,
//! `sched_getscheduler` then reads `SCHED_OTHER`, and `SCHED_IDLE` leaves the
//! nice at 19; `SCHED_FIFO`, `SCHED_RR` and `SCHED_DEADLINE` are `EPERM` from
//! both. Then the spinner case: a spinner at nice -20 on every processor, and
//! a program at nice -20 beside them still has its calls answered by linuxd,
//! whose level is above every program's.
#![allow(unsafe_code, reason = "the step's calls are raw system calls")]

use std::time::{Duration, Instant};

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

/// The accepted policies: `SCHED_OTHER`, `SCHED_BATCH`, `SCHED_IDLE`.
const ACCEPTED: [(i32, &str); 3] = [(0, "SCHED_OTHER"), (3, "SCHED_BATCH"), (5, "SCHED_IDLE")];
/// The refused ones: `SCHED_FIFO`, `SCHED_RR`, `SCHED_DEADLINE`.
const REFUSED: [(i32, &str); 3] = [(1, "SCHED_FIFO"), (2, "SCHED_RR"), (6, "SCHED_DEADLINE")];
/// `SCHED_IDLE`.
const SCHED_IDLE: i32 = 5;
/// How many calls the program beside the spinners makes.
const CALLS: u32 = 200;
/// How long they may take: far above a TCG boot's, finite so a starved linuxd fails by name.
const PATIENCE: Duration = Duration::from_secs(30);

/// The calling thread's error number.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `sched_setscheduler(0, policy, {priority})` by its number (musl's wrapper is a stub): zero
/// or the error number.
fn set_scheduler(policy: i32, priority: i32) -> i32 {
    // The kernel's `struct sched_param` is one int (musl's carries more, unread).
    let param: i32 = priority;
    // SAFETY: the kernel reads one live int.
    let answer =
        unsafe { libc::syscall(libc::SYS_sched_setscheduler, 0, policy, &raw const param) };
    if answer == 0 { 0 } else { errno() }
}

/// `sched_setattr(0, {policy, nice}, 0)`: zero or the error number.
fn set_attr(policy: i32, nice: i32) -> i32 {
    let mut attr = SchedAttr {
        size: size_of::<SchedAttr>() as u32,
        sched_policy: policy as u32,
        sched_nice: nice,
        sched_priority: 0,
        ..SchedAttr::default()
    };
    if policy == 6 {
        attr.sched_runtime = 1_000_000;
        attr.sched_deadline = 10_000_000;
        attr.sched_period = 10_000_000;
    }
    // SAFETY: the kernel reads `size` bytes of a live structure.
    let answer = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw mut attr, 0) };
    if answer == 0 { 0 } else { errno() }
}

/// The calling thread's policy and nice, by `sched_getscheduler` and `sched_getattr`.
fn policy_and_nice() -> (i64, Option<i32>) {
    // SAFETY: no memory is passed.
    let policy = unsafe { libc::syscall(libc::SYS_sched_getscheduler, 0) };
    let mut attr = SchedAttr::default();
    let size = size_of::<SchedAttr>() as u32;
    // SAFETY: the kernel writes at most `size` bytes into a live structure.
    let answer = unsafe { libc::syscall(libc::SYS_sched_getattr, 0, &raw mut attr, size, 0) };
    (policy, (answer == 0).then_some(attr.sched_nice))
}

/// A fork child that runs `body` and exits with its answer; the answer, or -1.
fn in_child(body: impl FnOnce() -> i32) -> i32 {
    // SAFETY: the child makes raw calls and ends by `_exit`.
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

/// One accepted policy through `how` in a child: 0, 1 when refused, 2 when not read back as
/// `SCHED_OTHER`, 3 when `SCHED_IDLE` left the nice other than 19.
fn accepted(policy: i32, how: fn(i32) -> i32) -> i32 {
    in_child(|| {
        if how(policy) != 0 {
            return 1;
        }
        let (now, nice) = policy_and_nice();
        if now != 0 {
            return 2;
        }
        if policy == SCHED_IDLE && nice != Some(19) {
            return 3;
        }
        0
    })
}

/// The spinner case, in a child: `cpus` spinners at nice -20, and this program at nice -20
/// making [`CALLS`] calls linuxd answers. 0, or 1 for a call refused, 2 past [`PATIENCE`].
fn beside_spinners(cpus: usize) -> i32 {
    in_child(|| {
        if set_attr(0, -20) != 0 {
            return 4;
        }
        let mut spinners = Vec::with_capacity(cpus);
        for _ in 0..cpus {
            // SAFETY: the child only spins.
            match unsafe { libc::fork() } {
                0 => loop {
                    core::hint::spin_loop();
                },
                -1 => break,
                child => spinners.push(child),
            }
        }
        let started = Instant::now();
        let mut code = 0;
        for _ in 0..CALLS {
            if std::fs::read_to_string("/proc/self/status").is_err() {
                code = 1;
                break;
            }
            if started.elapsed() > PATIENCE {
                code = 2;
                break;
            }
        }
        let took = started.elapsed();
        for child in &spinners {
            // SAFETY: signals and reaps the spinners this child made.
            let _ = unsafe { libc::kill(*child, libc::SIGKILL) };
            let mut status = 0;
            // SAFETY: as above.
            let _ = unsafe { libc::waitpid(*child, &raw mut status, 0) };
        }
        if spinners.len() != cpus {
            return 5;
        }
        println!(
            "threads: sched-policy: {CALLS} calls answered beside {cpus} nice -20 spinners in {} ms",
            took.as_millis()
        );
        code
    })
}

/// The step; `fail` names what went wrong and ends the program.
pub(crate) fn check_sched_policy(fail: fn(&str) -> !) {
    println!("threads: sched-policy");
    for (policy, name) in ACCEPTED {
        if accepted(policy, |policy| set_scheduler(policy, 0)) != 0 {
            println!("threads: sched-policy: sched_setscheduler {name}");
            fail("sched_setscheduler did not take an accepted policy as its nice");
        }
        if accepted(policy, |policy| set_attr(policy, 0)) != 0 {
            println!("threads: sched-policy: sched_setattr {name}");
            fail("sched_setattr did not take an accepted policy as its nice");
        }
    }
    for (policy, name) in REFUSED {
        let priority = if policy == 6 { 0 } else { 1 };
        let by_scheduler = in_child(|| set_scheduler(policy, priority));
        let by_attr = in_child(|| set_attr(policy, 0));
        if by_scheduler != libc::EPERM || by_attr != libc::EPERM {
            println!(
                "threads: sched-policy: {name}: sched_setscheduler {by_scheduler}, sched_setattr {by_attr}"
            );
            fail("a real-time policy was not refused EPERM");
        }
    }
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    match beside_spinners(cpus) {
        0 => {}
        2 => fail("linuxd did not answer a program beside nice -20 spinners in time"),
        _ => fail("the nice -20 spinner case could not run"),
    }
    println!("threads: sched-policy ok");
}
