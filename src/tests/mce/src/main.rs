//! The guest half of `cargo xtask test-mce`'s recovery modes
//! (`docs/MACHINE-CHECK.md` §5), run as init on x86-64.
//!
//! It prints `/proc/ferrix-mce`, then forks a victim that maps a page, writes
//! it, prints `mce-test: victim pid=<pid> va=<address>` and reads the page in
//! a loop for [`SPIN_SECONDS`]. The gate finds the page's physical address
//! through QEMU's monitor and injects a machine check at it. With a handler
//! for `SIGBUS` the victim prints what `siginfo` said --
//! `mce-test: SIGBUS code=<n> addr=<hex> lsb=<n> err=<hex>` -- and exits 42; a victim
//! the kernel ends without its handler is reported by the parent as killed.
//! An untouched victim exits 0 when its time is up.
//!
//! The parent then prints how the victim ended, both `/proc` files, runs a
//! second child to show the kernel goes on, and ends with
//! `mce-test: done` and status 0. Any failure of its own is
//! `mce-test: FAILED <what>` and status 1.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// How long the victim reads its page before it exits on its own.
const SPIN_SECONDS: u64 = 20;
/// The victim's status when its `SIGBUS` handler ran.
const HANDLED: i32 = 42;
/// The byte the victim writes and reads back.
const PATTERN: u8 = 0xa5;

fn main() -> ExitCode {
    say("mce-test: started");
    show("/proc/ferrix-mce");
    // SAFETY: fork in a single-threaded program; the child runs only
    // `victim`, which never returns.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return failed("fork");
    }
    if pid == 0 {
        victim();
    }
    let mut status = 0;
    // SAFETY: `status` is a valid place for the status.
    if unsafe { libc::waitpid(pid, &raw mut status, 0) } != pid {
        return failed("waitpid");
    }
    if libc::WIFEXITED(status) {
        say(&format!(
            "mce-test: victim exited {}",
            libc::WEXITSTATUS(status)
        ));
    } else if libc::WIFSIGNALED(status) {
        say(&format!(
            "mce-test: victim killed by signal {}",
            libc::WTERMSIG(status)
        ));
    } else {
        return failed("the victim neither exited nor was killed");
    }
    show("/proc/ferrix-mce");
    show("/proc/ferrix-mce-retired");
    // SAFETY: as above; the child exits at once.
    let second = unsafe { libc::fork() };
    if second == 0 {
        // SAFETY: ends the child without running the parent's handlers.
        unsafe { libc::_exit(0) };
    }
    // SAFETY: as above.
    if second < 0 || unsafe { libc::waitpid(second, &raw mut status, 0) } != second {
        return failed("a second child after the machine check");
    }
    say("mce-test: a second program ran");
    say("mce-test: done");
    ExitCode::SUCCESS
}

/// The victim: a handler for `SIGBUS`, a page, its address, then reads.
fn victim() -> ! {
    // SAFETY: a zeroed `sigaction` is a valid start; the handler is
    // async-signal-safe (it formats on its stack and calls `write` and
    // `_exit`).
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigbus as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO;
        if libc::sigaction(libc::SIGBUS, &raw const action, std::ptr::null_mut()) != 0 {
            say("mce-test: FAILED sigaction");
            libc::_exit(1);
        }
    }
    // SAFETY: an anonymous private page; nothing else refers to it.
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if page == libc::MAP_FAILED {
        say("mce-test: FAILED mmap");
        // SAFETY: ends the child.
        unsafe { libc::_exit(1) };
    }
    let byte = page.cast::<u8>();
    // SAFETY: the page is mapped writable and four kilobytes long.
    unsafe { byte.write_volatile(PATTERN) };
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() };
    say(&format!(
        "mce-test: victim pid={pid} va={:#x}",
        page as usize
    ));
    let until = Instant::now() + Duration::from_secs(SPIN_SECONDS);
    let mut rounds = 0_u64;
    loop {
        // SAFETY: as above.
        if unsafe { byte.read_volatile() } != PATTERN {
            say("mce-test: FAILED the page changed under the victim");
            // SAFETY: ends the child.
            unsafe { libc::_exit(1) };
        }
        rounds = rounds.wrapping_add(1);
        if rounds % (1 << 20) == 0 && Instant::now() >= until {
            break;
        }
    }
    // SAFETY: ends the child.
    unsafe { libc::_exit(0) }
}

/// The `SIGBUS` handler: prints `si_code`, `si_addr` and `si_addr_lsb`,
/// then exits [`HANDLED`]. Formats on its own stack: nothing here may
/// allocate.
extern "C" fn on_sigbus(_signal: i32, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid `siginfo_t`; `_addr_lsb` is the
    // `short` after `si_addr` in `_sigfault`, at 24 on x86-64.
    let (code, address, lsb) = unsafe {
        let bytes = info.cast::<u8>();
        (
            (*info).si_code,
            (*info).si_addr() as usize,
            bytes.add(24).cast::<i16>().read_unaligned(),
        )
    };
    let mut line = [0_u8; 96];
    let mut at = put(&mut line, 0, b"mce-test: SIGBUS code=");
    at = put_number(&mut line, at, u64::from(code.unsigned_abs()), 10);
    at = put(&mut line, at, b" addr=0x");
    at = put_number(&mut line, at, address as u64, 16);
    at = put(&mut line, at, b" lsb=");
    at = put_number(&mut line, at, u64::from(lsb.unsigned_abs()), 10);
    // SAFETY: with SA_SIGINFO the third argument is the `ucontext_t` the
    // kernel built; `REG_ERR` is the trap's error code it copied there,
    // which must not carry the frame's physical address.
    let err = unsafe {
        (*context.cast::<libc::ucontext_t>())
            .uc_mcontext
            .gregs
            .get(libc::REG_ERR as usize)
            .copied()
            .unwrap_or(-1)
    };
    at = put(&mut line, at, b" err=0x");
    at = put_number(&mut line, at, err as u64, 16);
    at = put(&mut line, at, b"\n");
    // SAFETY: `line[..at]` is initialised; write and _exit are
    // async-signal-safe.
    unsafe {
        let _ = libc::write(1, line.as_ptr().cast(), at);
        libc::_exit(HANDLED);
    }
}

/// Copy `bytes` into `line` at `at`; where it ends.
fn put(line: &mut [u8], at: usize, bytes: &[u8]) -> usize {
    let mut at = at;
    for &byte in bytes {
        if let Some(slot) = line.get_mut(at) {
            *slot = byte;
            at += 1;
        }
    }
    at
}

/// Write `value` in `base` (10 or 16) into `line` at `at`; where it ends.
fn put_number(line: &mut [u8], at: usize, value: u64, base: u64) -> usize {
    let mut digits = [0_u8; 20];
    let mut count = 0;
    let mut rest = value;
    loop {
        let digit = (rest % base) as u8;
        if let Some(slot) = digits.get_mut(count) {
            *slot = if digit < 10 {
                b'0' + digit
            } else {
                b'a' + digit - 10
            };
        }
        count += 1;
        rest /= base;
        if rest == 0 || count == digits.len() {
            break;
        }
    }
    let mut at = at;
    for index in (0..count).rev() {
        if let Some(&digit) = digits.get(index) {
            at = put(line, at, &[digit]);
        }
    }
    at
}

/// Print a line and flush it.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Print a file, each line prefixed with its name.
fn show(path: &str) {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            for line in text.lines() {
                say(&format!("mce-test: {path}: {line}"));
            }
            say(&format!("mce-test: {path} ends"));
        }
        Err(error) => say(&format!("mce-test: {path}: {error}")),
    }
}

/// Report a failure and the status for it.
fn failed(what: &str) -> ExitCode {
    say(&format!("mce-test: FAILED {what}"));
    ExitCode::FAILURE
}
