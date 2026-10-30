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

use std::ffi::CString;
use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (Some(mode), Some(path)) = (args.get(1), args.get(2)) else {
        return failed("usage: quarantine-test nonroot|userns <path>");
    };
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
