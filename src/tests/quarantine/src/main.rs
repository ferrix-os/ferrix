//! The probes of `cargo xtask test-quarantine` (`docs/CORE-QUARANTINE.md`
//! §4): a write of `0` to a processor's `online` file by a caller that may
//! not take a processor out of service.
//!
//! * `quarantine-test nonroot <path>`: as uid and gid 65534;
//! * `quarantine-test userns <path>`: as root of a user namespace of its
//!   own, its uid 0 mapped to the machine's 0 -- so the file's owner and
//!   mode let it open the file, and only the kernel's check of
//!   `CAP_SYS_ADMIN` in the first user namespace refuses the write.
//!
//! Each prints `quarantine-probe: <mode> open=<ok|errno N>
//! write=<ok|errno N|skipped>` and exits 0; a step of its own that failed
//! is `quarantine-probe: FAILED <what>` and status 1.
//!
//! And two for the fallback of applied affinity (L.sched.123,
//! `docs/GAME-MODE-SCHED.md` §2.1), which busybox has no `taskset` for:
//!
//! * `quarantine-test pin <cpu> <file>`: `sched_setaffinity` to that
//!   processor alone, then spin for ever, writing `<n> <processor>` to the
//!   file every few milliseconds, `n` counting up, the processor `getcpu`'s
//!   (`/proc/<pid>/stat`'s processor field names the reader's);
//! * `quarantine-test mask <pid>`: print `sched_getaffinity(pid)` as sysfs
//!   lists processors, `0-1,3`.

use std::ffi::CString;
use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(mode), Some(path)) = (args.get(1), args.get(2)) else {
        return failed("usage: quarantine-test nonroot|userns <path> | pin <cpu> | mask <pid>");
    };
    match mode.as_str() {
        "pin" => return pin(path, args.get(3).map_or("/run/qt/pinned", String::as_str)),
        "mask" => return mask(path),
        _ => {}
    }
    let ready = match mode.as_str() {
        "nonroot" => become_nobody(),
        "userns" => become_namespace_root(),
        _ => Err("an unknown mode"),
    };
    if let Err(what) = ready {
        return failed(what);
    }
    let Ok(path) = CString::new(path.as_bytes()) else {
        return failed("a path with a NUL");
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY) };
    if fd < 0 {
        say(&format!(
            "quarantine-probe: {mode} open=errno {} write=skipped",
            errno()
        ));
        return ExitCode::SUCCESS;
    }
    // SAFETY: `fd` is open, and the buffer is two valid bytes.
    let wrote = unsafe { libc::write(fd, b"0\n".as_ptr().cast(), 2) };
    let write = if wrote < 0 {
        format!("errno {}", errno())
    } else {
        "ok".to_owned()
    };
    // SAFETY: `fd` is ours to close.
    let _ = unsafe { libc::close(fd) };
    say(&format!("quarantine-probe: {mode} open=ok write={write}"));
    ExitCode::SUCCESS
}

/// Drop to uid and gid 65534.
fn become_nobody() -> Result<(), &'static str> {
    // SAFETY: plain system calls with no pointers.
    if unsafe { libc::setgroups(0, core::ptr::null()) } != 0 {
        return Err("setgroups");
    }
    // SAFETY: as above.
    if unsafe { libc::setgid(65534) } != 0 || unsafe { libc::setuid(65534) } != 0 {
        return Err("setuid to 65534");
    }
    Ok(())
}

/// Make a user namespace of its own, uid and gid 0 in it mapped to the
/// machine's 0.
fn become_namespace_root() -> Result<(), &'static str> {
    // SAFETY: a plain system call with no pointers.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Err("unshare(CLONE_NEWUSER)");
    }
    std::fs::write("/proc/self/uid_map", b"0 0 1\n").map_err(|_| "writing uid_map")?;
    std::fs::write("/proc/self/setgroups", b"deny\n").map_err(|_| "writing setgroups")?;
    std::fs::write("/proc/self/gid_map", b"0 0 1\n").map_err(|_| "writing gid_map")?;
    Ok(())
}

/// Pin this process to processor `cpu` alone and spin for ever, saying
/// where it runs in `file`.
fn pin(cpu: &str, file: &str) -> ExitCode {
    let Ok(cpu) = cpu.parse::<usize>() else {
        return failed("a processor that is not a number");
    };
    // SAFETY: a zeroed `cpu_set_t` is the empty set.
    let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
    // SAFETY: sets one bit of a live set.
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: the kernel reads `size_of` bytes of a live set.
    if unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set) } != 0 {
        return failed(&format!("sched_setaffinity to {cpu}: errno {}", errno()));
    }
    let mut count = 0_u64;
    loop {
        for _ in 0..1_000_000 {
            core::hint::spin_loop();
        }
        count += 1;
        let mut now = 0_u32;
        // SAFETY: getcpu writes one u32 through a live pointer.
        let _ = unsafe {
            libc::syscall(
                libc::SYS_getcpu,
                &raw mut now,
                core::ptr::null_mut::<u32>(),
                core::ptr::null_mut::<u8>(),
            )
        };
        let _ = std::fs::write(file, format!("{count} {now}\n"));
    }
}

/// Print `pid`'s mask as sysfs lists processors.
fn mask(pid: &str) -> ExitCode {
    let Ok(pid) = pid.parse::<i32>() else {
        return failed("a pid that is not a number");
    };
    // SAFETY: a zeroed `cpu_set_t` is the empty set.
    let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
    // SAFETY: the kernel writes at most `size_of` bytes into a live set.
    if unsafe { libc::sched_getaffinity(pid, size_of::<libc::cpu_set_t>(), &raw mut set) } != 0 {
        return failed(&format!("sched_getaffinity of {pid}: errno {}", errno()));
    }
    let cpus: Vec<usize> = (0..libc::CPU_SETSIZE as usize)
        // SAFETY: reads one bit of a live set.
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
        .collect();
    let mut parts = Vec::new();
    let mut at = 0;
    while let Some(&start) = cpus.get(at) {
        let mut end = start;
        while cpus.get(at + 1) == Some(&(end + 1)) {
            end += 1;
            at += 1;
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        at += 1;
    }
    say(&parts.join(","));
    ExitCode::SUCCESS
}

/// The last error number.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Print a line and flush it.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Say what failed, and fail.
fn failed(what: &str) -> ExitCode {
    say(&format!("quarantine-probe: FAILED {what}"));
    ExitCode::FAILURE
}
