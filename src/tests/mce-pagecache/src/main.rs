//! The guest half of `cargo xtask test-mce-pagecache`
//! (`docs/MACHINE-CHECK-PAGECACHE.md` §6), run as init on x86-64.
//!
//! A child writes a 64-page file on the writable disk whose every 8-byte word
//! is [`MAGIC`] above its page's index and syncs it -- in the dirty modes a
//! `memfd` instead, whose pages have no other copy (a disk page written and
//! not written back is written back within seconds, so the gate could not
//! keep one dirty; stage 12's check covers that case) -- and then, for
//! [`SPIN_SECONDS`]:
//!
//! * **read** (default): reads the file whole in a loop, checking every word,
//!   and prints `mce-pc: ready read pages=64`; at the end `mce-pc: reads all
//!   right <n>`, or `mce-pc: read failed errno=<n>` the first time one fails;
//! * **dirty** (`--features dirty`): the same with the file not synced, so a
//!   read that consumes poison must fail: `mce-pc: read failed errno=5`;
//! * **mapped** (`--features mapped`): maps the file and loads every word in a
//!   loop, printing `mce-pc: ready mapped va=<hex> pages=64`; a `SIGBUS`
//!   prints `mce-pc: SIGBUS code=<n> addr=<hex>` and ends the child with 42;
//!   at the end `mce-pc: mapped reads all right <n>`. With `dirty` too, the
//!   file is not synced and a `SIGBUS` is what the gate wants;
//! * **poll** (`--features poll`): maps the file, touches it, prints `mce-pc:
//!   ready polled va=<hex> pages=64`, and reads it every 100 ms; at the end
//!   `mce-pc: polled reads all right <n>`.
//!
//! The parent prints how the child ended, `/proc/meminfo`'s `Dirty:` line,
//! the `/proc` machine-check files, and `mce-pc: done`.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::io::AsRawFd as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// How long the child keeps at it.
const SPIN_SECONDS: u64 = 30;
/// The file's pages.
const PAGES: usize = 64;
/// A page.
const PAGE: usize = 4096;
/// The high half of every word: `MCPC`.
const MAGIC: u64 = 0x4d43_5043;
/// The file, on the writable disk the kernel mounts at stage 12.
const PATH: &str = "/mnt-rw/mce-pagecache";

fn main() -> ExitCode {
    say("mce-pc: started");
    // SAFETY: fork in a single-threaded program; the child runs only `child`,
    // which never returns.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return failed("fork");
    }
    if pid == 0 {
        child();
    }
    let mut status = 0;
    // SAFETY: `status` is a valid place for the status.
    if unsafe { libc::waitpid(pid, &raw mut status, 0) } != pid {
        return failed("waitpid");
    }
    if libc::WIFEXITED(status) {
        say(&format!("mce-pc: child exited {}", libc::WEXITSTATUS(status)));
    } else if libc::WIFSIGNALED(status) {
        say(&format!("mce-pc: child killed by signal {}", libc::WTERMSIG(status)));
    }
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines().filter(|line| line.starts_with("Dirty:")) {
            say(&format!("mce-pc: {line}"));
        }
    }
    show("/proc/ferrix-mce");
    show("/proc/ferrix-mce-retired");
    say("mce-pc: done");
    ExitCode::SUCCESS
}

/// The child: the file, then the mode's loop.
fn child() -> ! {
    // SAFETY: a zeroed `sigaction` is a valid start; the handler is
    // async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigbus as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO;
        let _ = libc::sigaction(libc::SIGBUS, &raw const action, std::ptr::null_mut());
    }
    let mut bytes = Vec::with_capacity(PAGES * PAGE);
    for word in 0..PAGES * PAGE / 8 {
        let value = (MAGIC << 32) | (word * 8 / PAGE) as u64;
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    let mut file = if cfg!(feature = "dirty") {
        // SAFETY: a valid, terminated name.
        let fd = unsafe { libc::memfd_create(c"mce-pc".as_ptr(), 0) };
        if fd < 0 {
            exit_with("mce-pc: FAILED memfd_create", 1);
        }
        // SAFETY: the descriptor was just made and nothing else owns it.
        unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) }
    } else {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(PATH);
        let Ok(file) = file else {
            exit_with("mce-pc: FAILED open", 1);
        };
        file
    };
    if file.write_all(&bytes).is_err() {
        exit_with("mce-pc: FAILED write", 1);
    }
    if !cfg!(feature = "dirty") {
        if file.sync_all().is_err() {
            exit_with("mce-pc: FAILED sync", 1);
        }
        // Read and mapped through a descriptor that cannot write: a shared
        // mapping of a writable one counts as one that may write, whose
        // pages the kernel rightly never takes as clean.
        let Ok(read_only) = OpenOptions::new().read(true).open(PATH) else {
            exit_with("mce-pc: FAILED open read-only", 1);
        };
        file = read_only;
    }
    let until = Instant::now() + Duration::from_secs(SPIN_SECONDS);
    if cfg!(feature = "mapped") || cfg!(feature = "poll") {
        mapped(&file, until)
    } else {
        reads(&file, until)
    }
}

/// The read modes.
fn reads(file: &std::fs::File, until: Instant) -> ! {
    say(&format!("mce-pc: ready read pages={PAGES}"));
    let mut buffer = vec![0u8; PAGES * PAGE];
    let mut rounds = 0u64;
    while Instant::now() < until {
        // One pread, not read_exact: a read that fails part-way answers the
        // bytes before the failing page, and a retry of the rest would read
        // the page again, where QEMU's poison is not.
        // SAFETY: `buffer` is writable for its length; the descriptor is open.
        let read = unsafe {
            libc::pread(file.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0)
        };
        if read < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            exit_with(&format!("mce-pc: read failed errno={errno}"), 0);
        }
        if read as usize != buffer.len() {
            exit_with(&format!("mce-pc: read ended short at {read:#x}"), 0);
        }
        if !right(&buffer) {
            exit_with("mce-pc: FAILED a read gave other bytes than the file's", 1);
        }
        rounds += 1;
    }
    exit_with(&format!("mce-pc: reads all right {rounds}"), 0)
}

/// The mapped and poll modes.
fn mapped(file: &std::fs::File, until: Instant) -> ! {
    // SAFETY: a shared read-only mapping of an open file of `PAGES` pages.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGES * PAGE,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        exit_with("mce-pc: FAILED mmap", 1);
    }
    let words = at.cast::<u64>();
    // SAFETY: the mapping is `PAGES` pages long.
    let view = || unsafe { std::slice::from_raw_parts(words.cast::<u8>(), PAGES * PAGE) };
    if !right(view()) {
        exit_with("mce-pc: FAILED the mapping read other bytes than the file's", 1);
    }
    let poll = cfg!(feature = "poll");
    let mode = if poll { "polled" } else { "mapped" };
    say(&format!("mce-pc: ready {mode} va={:#x} pages={PAGES}", at as usize));
    let mut rounds = 0u64;
    while Instant::now() < until {
        if !right(view()) {
            exit_with("mce-pc: FAILED the mapping read other bytes than the file's", 1);
        }
        rounds += 1;
        if poll {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    exit_with(&format!("mce-pc: {mode} reads all right {rounds}"), 0)
}

/// Whether every word of `bytes` is [`MAGIC`] above its page.
fn right(bytes: &[u8]) -> bool {
    bytes.chunks_exact(8).enumerate().all(|(word, chunk)| {
        let mut value = [0u8; 8];
        value.copy_from_slice(chunk);
        u64::from_ne_bytes(value) == (MAGIC << 32) | (word * 8 / PAGE) as u64
    })
}

/// The `SIGBUS` handler: prints `si_code` and `si_addr`, then ends the child
/// with 42. Formats on its own stack.
extern "C" fn on_sigbus(_signal: i32, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid `siginfo_t`.
    let (code, address) = unsafe { ((*info).si_code, (*info).si_addr() as u64) };
    let mut line = [0u8; 80];
    let mut at = put(&mut line, 0, b"mce-pc: SIGBUS code=");
    at = put_number(&mut line, at, u64::from(code.unsigned_abs()), 10);
    at = put(&mut line, at, b" addr=0x");
    at = put_number(&mut line, at, address, 16);
    at = put(&mut line, at, b"\n");
    // SAFETY: `line[..at]` is initialised; write and _exit are
    // async-signal-safe.
    unsafe {
        let _ = libc::write(1, line.as_ptr().cast(), at);
        libc::_exit(42);
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
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut rest = value;
    loop {
        let digit = (rest % base) as u8;
        if let Some(slot) = digits.get_mut(count) {
            *slot = if digit < 10 { b'0' + digit } else { b'a' + digit - 10 };
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

/// Say `line` and end the child with `status`.
fn exit_with(line: &str, status: i32) -> ! {
    say(line);
    // SAFETY: ends the child without running the parent's handlers.
    unsafe { libc::_exit(status) }
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
                say(&format!("mce-pc: {path}: {line}"));
            }
        }
        Err(error) => say(&format!("mce-pc: {path}: {error}")),
    }
}

/// Report a failure and the status for it.
fn failed(what: &str) -> ExitCode {
    say(&format!("mce-pc: FAILED {what}"));
    ExitCode::FAILURE
}
