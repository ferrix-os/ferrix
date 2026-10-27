//! `SCHED_FIFO` and `SCHED_RR` as a program asks for them, run as init by
//! `cargo xtask test-rtsched` (`docs/SCHED-CLASSES.md` §9e, R12).
//!
//! musl's `sched_setscheduler` and its kin are stubs that answer `ENOSYS`,
//! so every call here is the raw system call, which is also what checks
//! each call's encoding. Each step prints `rtsched: <step> ok`, and the
//! program ends with `rtsched: all ok` and status 0, or
//! `rtsched: FAILED <what>` and status 1.
//!
//! On a build with the realtime tier (`desktop`):
//!
//! * **encoding**: a thread starts `SCHED_OTHER` at 0, and the priority
//!   range of each policy is Linux's;
//! * **chrt**: the caller moves itself to `SCHED_FIFO` 10 and reads it back
//!   through `sched_getscheduler`, `sched_getparam` and `sched_getattr`, to
//!   `SCHED_RR` 20 (`sched_rr_get_interval` answers 100 ms), to 30 by
//!   `sched_setparam`, through `sched_setattr`, and back to `SCHED_OTHER`,
//!   whose nice value `sched_setattr` sets;
//! * **errors**: each refusal Linux makes, with its errno (L.sched.117);
//! * **permission**: uid 1000 with `RLIMIT_RTPRIO` 0 is `EPERM`, with 20 may
//!   take up to 20 and not 30, and may not clear reset-on-fork
//!   (L.sched.111);
//! * **inherit**: a forked child and a thread keep `SCHED_FIFO` 10, and with
//!   reset-on-fork a forked child starts `SCHED_OTHER` (L.sched.113).
//!
//! Built with `present`, for the build without the tier: every real-time
//! policy is `EINVAL`, by each call that could set one (L.sched.116).
//!
//! Built with `negative-control`, the inherit step expects a reset-on-fork
//! child to keep its parent's policy, and the program must fail on that
//! step.

use std::io::Write;

use libc::{c_int, c_long, pid_t};

/// `SCHED_OTHER`.
const SCHED_OTHER: c_int = 0;
/// `SCHED_FIFO`.
const SCHED_FIFO: c_int = 1;
/// `SCHED_RR`.
const SCHED_RR: c_int = 2;
/// `SCHED_RESET_ON_FORK`.
const SCHED_RESET_ON_FORK: c_int = 0x4000_0000;
/// `RLIMIT_RTPRIO`.
const RLIMIT_RTPRIO: c_int = 14;
/// The unprivileged user the permission step becomes.
const OWNER: u32 = 1000;

/// The kernel's `struct sched_param`: one `int` (musl's carries more, for
/// a sporadic server Linux does not have).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Param {
    /// The priority.
    sched_priority: c_int,
}

/// `struct sched_attr`, `SCHED_ATTR_SIZE_VER1`, and room past it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SchedAttr {
    /// Its size.
    size: u32,
    /// The policy.
    policy: u32,
    /// `SCHED_FLAG_*`.
    flags: u64,
    /// The nice value, under `SCHED_OTHER`.
    nice: i32,
    /// The real-time priority.
    priority: u32,
    /// `SCHED_DEADLINE`'s three.
    runtime: u64,
    /// See `runtime`.
    deadline: u64,
    /// See `runtime`.
    period: u64,
    /// The utilisation clamps.
    util_min: u32,
    /// See `util_min`.
    util_max: u32,
    /// Bytes past the structure Linux knows, for the size checks.
    tail: [u8; 8],
}

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

/// A raw call's answer: its value, or the errno it failed with.
fn answer(value: c_long) -> Result<c_long, c_int> {
    if value < 0 { Err(errno()) } else { Ok(value) }
}

/// `sched_setscheduler(pid, policy, &priority)`.
fn setscheduler(pid: pid_t, policy: c_int, priority: c_int) -> Result<c_long, c_int> {
    let param = Param {
        sched_priority: priority,
    };
    // SAFETY: the parameter is alive for the call.
    answer(unsafe { libc::syscall(libc::SYS_sched_setscheduler, pid, policy, &param) })
}

/// `sched_setscheduler` with a null parameter.
fn setscheduler_null(pid: pid_t, policy: c_int) -> Result<c_long, c_int> {
    // SAFETY: a null pointer, which the kernel must refuse.
    answer(unsafe {
        libc::syscall(
            libc::SYS_sched_setscheduler,
            pid,
            policy,
            core::ptr::null::<Param>(),
        )
    })
}

/// `sched_setparam(pid, &priority)`.
fn setparam(pid: pid_t, priority: c_int) -> Result<c_long, c_int> {
    let param = Param {
        sched_priority: priority,
    };
    // SAFETY: the parameter is alive for the call.
    answer(unsafe { libc::syscall(libc::SYS_sched_setparam, pid, &param) })
}

/// `sched_getscheduler(pid)`.
fn getscheduler(pid: pid_t) -> Result<c_long, c_int> {
    // SAFETY: plain integers.
    answer(unsafe { libc::syscall(libc::SYS_sched_getscheduler, pid) })
}

/// `sched_getparam(pid)`'s priority.
fn getparam(pid: pid_t) -> Result<c_int, c_int> {
    let mut param = Param { sched_priority: -1 };
    // SAFETY: the parameter is alive for the call.
    answer(unsafe { libc::syscall(libc::SYS_sched_getparam, pid, &mut param) })
        .map(|_| param.sched_priority)
}

/// `sched_setattr(pid, attr, flags)`.
fn setattr(pid: pid_t, attr: &mut SchedAttr, flags: u32) -> Result<c_long, c_int> {
    // SAFETY: the structure is alive for the call, and as long as its size.
    answer(unsafe {
        libc::syscall(
            libc::SYS_sched_setattr,
            pid,
            attr as *mut SchedAttr,
            flags,
        )
    })
}

/// `sched_getattr(pid, attr, size, flags)`.
fn getattr(pid: pid_t, size: u32, flags: u32) -> Result<SchedAttr, c_int> {
    let mut attr = SchedAttr::default();
    // SAFETY: the structure is alive for the call, and as long as `size`
    // whenever the call is asked for at most 64 bytes.
    answer(unsafe {
        libc::syscall(
            libc::SYS_sched_getattr,
            pid,
            &mut attr as *mut SchedAttr,
            size,
            flags,
        )
    })
    .map(|_| attr)
}

/// `sched_rr_get_interval(0)`, through musl, which handles 64-bit time on
/// ARMv7-A.
fn rr_interval() -> Result<(i64, i64), c_int> {
    let mut ts = libc::timespec {
        tv_sec: -1,
        tv_nsec: -1,
    };
    // SAFETY: the structure is alive for the call.
    let done = unsafe { libc::sched_rr_get_interval(0, &mut ts) };
    if done != 0 {
        return Err(errno());
    }
    #[allow(
        clippy::useless_conversion,
        reason = "the same type on the 64-bit targets, a narrower one on ARMv7-A"
    )]
    let pair = (i64::from(ts.tv_sec), i64::from(ts.tv_nsec));
    Ok(pair)
}

/// Require `got` to be `want`, or say what `what` was.
fn expect<T: PartialEq + std::fmt::Debug>(what: &str, got: T, want: T) -> Step {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, expected {want:?}"))
    }
}

/// A policy and priority read back by both calls.
fn reads(what: &str, policy: c_long, priority: c_int) -> Step {
    expect(what, getscheduler(0), Ok(policy))?;
    expect(what, getparam(0), Ok(priority))
}

/// `fork`, running `child` in the child, which exits with its answer, and
/// answering the child's status in the parent.
fn in_child(child: impl FnOnce() -> Step) -> Result<c_int, String> {
    // SAFETY: init is single-threaded here; the child calls only what is
    // safe after fork, and exits.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork failed, errno {}", errno()));
    }
    if pid == 0 {
        let status = match child() {
            Ok(()) => 0,
            Err(what) => {
                say(&format!("rtsched: child: {what}"));
                1
            }
        };
        // SAFETY: the child ends here.
        unsafe { libc::_exit(status) };
    }
    let mut status = 0;
    // SAFETY: waiting for the child just made.
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    if !libc::WIFEXITED(status) {
        return Err("a child did not exit".to_owned());
    }
    Ok(libc::WEXITSTATUS(status))
}

/// [`in_child`], requiring status 0.
fn child_passes(what: &str, child: impl FnOnce() -> Step) -> Step {
    expect(what, in_child(child)?, 0)
}

/// Become uid and gid [`OWNER`].
fn become_owner() -> Step {
    // SAFETY: plain integers.
    if unsafe { libc::setgid(OWNER) != 0 || libc::setuid(OWNER) != 0 } {
        return Err(format!("could not become uid {OWNER}, errno {}", errno()));
    }
    Ok(())
}

/// Set `RLIMIT_RTPRIO` to `limit`, soft and hard.
fn rtprio_limit(limit: u64) -> Step {
    let value = libc::rlimit {
        rlim_cur: limit,
        rlim_max: limit,
    };
    // SAFETY: the structure is alive for the call.
    if unsafe { libc::setrlimit(RLIMIT_RTPRIO as _, &value) } != 0 {
        return Err(format!("setrlimit(RLIMIT_RTPRIO) failed, errno {}", errno()));
    }
    Ok(())
}

/// A thread starts `SCHED_OTHER` at 0; the ranges are Linux's.
fn encoding() -> Step {
    reads("a new thread", 0, 0)?;
    // SAFETY: plain integers.
    let (max, min) = unsafe {
        (
            libc::sched_get_priority_max(SCHED_FIFO),
            libc::sched_get_priority_min(SCHED_RR),
        )
    };
    expect("the real-time range", (min, max), (1, 99))?;
    expect("the round-robin interval of a fair thread", rr_interval(), Ok((0, 0)))
}

/// The caller moves itself between policies, `chrt`'s way and
/// `sched_setattr`'s, and back.
fn chrt() -> Step {
    expect("SCHED_FIFO 10", setscheduler(0, SCHED_FIFO, 10), Ok(0))?;
    reads("after SCHED_FIFO 10", 1, 10)?;
    let attr = getattr(0, 56, 0).map_err(|e| format!("sched_getattr failed, errno {e}"))?;
    expect(
        "sched_getattr under SCHED_FIFO 10",
        (attr.size, attr.policy, attr.priority, attr.flags),
        (56, 1, 10, 0),
    )?;
    expect("FIFO's round-robin interval", rr_interval(), Ok((0, 0)))?;
    expect("SCHED_RR 20", setscheduler(0, SCHED_RR, 20), Ok(0))?;
    reads("after SCHED_RR 20", 2, 20)?;
    expect("RR's interval", rr_interval(), Ok((0, 100_000_000)))?;
    expect("sched_setparam 30", setparam(0, 30), Ok(0))?;
    reads("after sched_setparam 30", 2, 30)?;
    let mut attr = SchedAttr {
        size: 48,
        policy: 1,
        priority: 15,
        ..SchedAttr::default()
    };
    expect("sched_setattr SCHED_FIFO 15", setattr(0, &mut attr, 0), Ok(0))?;
    reads("after sched_setattr", 1, 15)?;
    let mut attr = SchedAttr {
        size: 48,
        policy: 0,
        nice: 5,
        ..SchedAttr::default()
    };
    expect("sched_setattr SCHED_OTHER nice 5", setattr(0, &mut attr, 0), Ok(0))?;
    reads("back to SCHED_OTHER", 0, 0)?;
    // SAFETY: plain integers.
    let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    expect("the nice value sched_setattr set", nice, 5)?;
    let attr = getattr(0, 48, 0).map_err(|e| format!("sched_getattr failed, errno {e}"))?;
    expect("sched_getattr's nice and size", (attr.nice, attr.size), (5, 48))?;
    attr_back_to_zero()
}

/// Nice 0 again, by `sched_setattr`.
fn attr_back_to_zero() -> Step {
    let mut attr = SchedAttr {
        size: 48,
        ..SchedAttr::default()
    };
    expect("sched_setattr nice 0", setattr(0, &mut attr, 0), Ok(0))
}

/// Each refusal Linux makes, with its errno.
fn errors() -> Step {
    let einval = Err(libc::EINVAL);
    expect("SCHED_FIFO 0", setscheduler(0, SCHED_FIFO, 0), einval)?;
    expect("SCHED_FIFO 100", setscheduler(0, SCHED_FIFO, 100), einval)?;
    expect("SCHED_OTHER 5", setscheduler(0, SCHED_OTHER, 5), einval)?;
    for policy in [3, 5, 6, 7, 8] {
        expect("an unsupported policy", setscheduler(0, policy, 0), einval)?;
    }
    expect("a negative policy", setscheduler(0, -1, 0), einval)?;
    expect("a negative pid", setscheduler(-1, SCHED_FIFO, 10), einval)?;
    expect("a null parameter", setscheduler_null(0, SCHED_FIFO), einval)?;
    expect(
        "a pid that is no thread",
        setscheduler(999_999, SCHED_FIFO, 10),
        Err(libc::ESRCH),
    )?;
    expect("getscheduler of no thread", getscheduler(999_999), Err(libc::ESRCH))?;
    expect("getscheduler of -1", getscheduler(-1), einval)?;
    expect("setparam out of range", setparam(0, 5), einval)?;
    let mut attr = SchedAttr {
        size: 48,
        policy: 1,
        priority: 10,
        ..SchedAttr::default()
    };
    expect("sched_setattr's flags", setattr(0, &mut attr, 1), einval)?;
    attr.size = 8;
    expect("a sched_attr too small", setattr(0, &mut attr, 0), Err(libc::E2BIG))?;
    expect("the size written back", attr.size, 56)?;
    attr.size = 64;
    attr.tail = [0, 0, 0, 1, 0, 0, 0, 0];
    expect("a nonzero tail", setattr(0, &mut attr, 0), Err(libc::E2BIG))?;
    let mut attr = SchedAttr {
        size: 48,
        flags: 0x80,
        ..SchedAttr::default()
    };
    expect("an unknown sched_flag", setattr(0, &mut attr, 0), einval)?;
    attr.flags = 0x20;
    attr.size = 56;
    expect("a utilisation clamp", setattr(0, &mut attr, 0), Err(libc::EOPNOTSUPP))?;
    expect("sched_getattr too small", getattr(0, 8, 0).map(|_| ()), Err(libc::EINVAL))?;
    expect("sched_getattr's flags", getattr(0, 56, 1).map(|_| ()), Err(libc::EINVAL))?;
    // A zeroed tail past the known size is taken.
    let mut attr = SchedAttr {
        size: 64,
        ..SchedAttr::default()
    };
    expect("a zeroed tail", setattr(0, &mut attr, 0), Ok(0))?;
    reads("after the refusals", 0, 0)
}

/// The permission rule, as uid 1000.
fn permission() -> Step {
    child_passes("uid 1000 with RLIMIT_RTPRIO 0", || {
        become_owner()?;
        expect("SCHED_FIFO 10", setscheduler(0, SCHED_FIFO, 10), Err(libc::EPERM))?;
        expect("SCHED_OTHER", setscheduler(0, SCHED_OTHER, 0), Ok(0))
    })?;
    child_passes("uid 1000 with RLIMIT_RTPRIO 20", || {
        rtprio_limit(20)?;
        become_owner()?;
        expect("SCHED_FIFO 10", setscheduler(0, SCHED_FIFO, 10), Ok(0))?;
        expect("SCHED_FIFO 20", setscheduler(0, SCHED_FIFO, 20), Ok(0))?;
        expect("SCHED_FIFO 30", setscheduler(0, SCHED_FIFO, 30), Err(libc::EPERM))?;
        expect("SCHED_RR 5", setscheduler(0, SCHED_RR, 5), Ok(0))?;
        expect("back to SCHED_OTHER", setscheduler(0, SCHED_OTHER, 0), Ok(0))
    })?;
    child_passes("uid 1000 under reset-on-fork", || {
        expect(
            "reset-on-fork as root",
            setscheduler(0, SCHED_OTHER | SCHED_RESET_ON_FORK, 0),
            Ok(0),
        )?;
        become_owner()?;
        expect(
            "reset-on-fork cleared",
            setscheduler(0, SCHED_OTHER, 0),
            Err(libc::EPERM),
        )
    })
}

/// A forked child and a thread inherit; with reset-on-fork a child does
/// not.
fn inherit() -> Step {
    expect("SCHED_FIFO 10", setscheduler(0, SCHED_FIFO, 10), Ok(0))?;
    child_passes("a forked child", || reads("the child", 1, 10))?;
    let thread = std::thread::spawn(|| reads("a thread", 1, 10));
    thread
        .join()
        .map_err(|_| "a thread did not finish".to_owned())??;
    expect(
        "reset-on-fork",
        setscheduler(0, SCHED_FIFO | SCHED_RESET_ON_FORK, 10),
        Ok(0),
    )?;
    let flagged = c_long::from(SCHED_FIFO | SCHED_RESET_ON_FORK);
    expect("the flag read back", getscheduler(0), Ok(flagged))?;
    let reset = in_child(|| {
        if cfg!(feature = "negative-control") {
            reads("the reset child", 1, 10)
        } else {
            reads("the reset child", 0, 0)
        }
    })?;
    expect("back to SCHED_OTHER", setscheduler(0, SCHED_OTHER, 0), Ok(0))?;
    if reset != 0 {
        return Err("a reset-on-fork child did not start SCHED_OTHER".to_owned());
    }
    Ok(())
}

/// The build without the tier: every real-time policy is `EINVAL`.
fn absent() -> Step {
    let einval = Err(libc::EINVAL);
    expect("SCHED_FIFO 10", setscheduler(0, SCHED_FIFO, 10), einval)?;
    expect("SCHED_RR 10", setscheduler(0, SCHED_RR, 10), einval)?;
    let mut attr = SchedAttr {
        size: 48,
        policy: 1,
        priority: 10,
        ..SchedAttr::default()
    };
    expect("sched_setattr SCHED_FIFO", setattr(0, &mut attr, 0), einval)?;
    reads("after the refusals", 0, 0)
}

fn main() {
    let steps: &[Named] = if cfg!(feature = "present") {
        &[("encoding", encoding), ("absent", absent)]
    } else {
        &[
            ("encoding", encoding),
            ("chrt", chrt),
            ("errors", errors),
            ("permission", permission),
            ("inherit", inherit),
        ]
    };
    for (name, step) in steps {
        if let Err(what) = step() {
            say(&format!("rtsched: FAILED {name}: {what}"));
            std::process::exit(1);
        }
        say(&format!("rtsched: {name} ok"));
    }
    say("rtsched: all ok");
}
