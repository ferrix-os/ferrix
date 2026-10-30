//! The guest half of `cargo xtask test-mce-copy`
//! (`docs/MACHINE-CHECK-COPY.md` §6), run as init on x86-64.
//!
//! **The copy modes** (default): a victim maps a 1 MiB buffer whose every
//! 8-byte word is [`MAGIC`] above its page's index, prints `mce-copy: victim
//! pid=<pid> va=<address> len=<length>`, and writes the buffer to a memfd in a
//! loop for [`SPIN_SECONDS`], so the kernel's copy of it runs nearly all the
//! time. The gate stops the machine with a processor at the copy's `rep
//! movsb` reading the buffer, and injects a machine check at that page. The
//! victim's `SIGBUS` handler keeps what `siginfo` said and returns; when a
//! write fails the victim prints `mce-copy: write failed errno=<n>` (or, where
//! it stored the pages before the poisoned one, `mce-copy: write ended short
//! at <offset>`), the
//! signal (`mce-copy: SIGBUS code=<n> addr=<hex> lsb=<n>` or `mce-copy: no
//! SIGBUS`), writes a fresh buffer (`mce-copy: a fresh write of <n> bytes`)
//! and exits 0. A victim whose writes never fail exits 3.
//!
//! **The environ mode** (`--features environ`): the program runs itself again
//! as a target with a 64 KiB environment and reads `/proc/<pid>/environ` in a
//! loop; the gate injects at a page of that environment while the kernel
//! copies it. A read that ends short prints `mce-copy: environ short <n> of
//! <m>`; neither process may get `SIGBUS` (`mce-copy: reader SIGBUS`, `mce-copy:
//! target SIGBUS` if one does); `mce-copy: target alive` when it still is.
//!
//! Either way the parent then prints both `/proc` files and ends with
//! `mce-copy: done` and status 0; a failure of its own is `mce-copy: FAILED
//! <what>` and status 1.

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long the victim, or the reader, keeps at it.
const SPIN_SECONDS: u64 = 30;
/// The buffer's length.
const LENGTH: usize = 1 << 20;
/// The high half of every word of the victim's buffer: `MCEC`.
const MAGIC: u64 = 0x4d43_4543;
/// The environment variable that makes the program the environ mode's target.
const TARGET: &str = "MCE_COPY_TARGET";
/// The environ mode's padding: 64 KiB of `Z`.
const PAD: usize = 64 << 10;

/// What the `SIGBUS` handler saw: `si_code`, or -1 for none yet.
static CODE: AtomicI32 = AtomicI32::new(-1);
/// Its `si_addr`.
static ADDRESS: AtomicU64 = AtomicU64::new(0);
/// Its `si_addr_lsb`.
static LSB: AtomicI64 = AtomicI64::new(0);

fn main() -> ExitCode {
    if std::env::var_os(TARGET).is_some() {
        target();
    }
    say("mce-copy: started");
    show("/proc/ferrix-mce");
    let code = if cfg!(feature = "environ") {
        environ_mode()
    } else {
        copy_mode()
    };
    if code != ExitCode::SUCCESS {
        return code;
    }
    show("/proc/ferrix-mce");
    show("/proc/ferrix-mce-retired");
    say("mce-copy: done");
    ExitCode::SUCCESS
}

/// The copy modes: fork the victim, and say how it ended.
fn copy_mode() -> ExitCode {
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
            "mce-copy: victim exited {}",
            libc::WEXITSTATUS(status)
        ));
    } else if libc::WIFSIGNALED(status) {
        say(&format!(
            "mce-copy: victim killed by signal {}",
            libc::WTERMSIG(status)
        ));
    } else {
        return failed("the victim neither exited nor was killed");
    }
    ExitCode::SUCCESS
}

/// The victim: a handler that keeps what it saw, the buffer, then writes.
fn victim() -> ! {
    handle_sigbus(on_sigbus_keep);
    let Some(buffer) = fill() else {
        exit_with("mce-copy: FAILED mmap", 1);
    };
    // A memfd, not a file on the root: its pages stay in memory, so a write
    // is nearly all the kernel's copy of the buffer, with no device work.
    // SAFETY: a valid, terminated name.
    let fd = unsafe { libc::memfd_create(c"mce-copy".as_ptr(), 0) };
    if fd < 0 {
        exit_with("mce-copy: FAILED memfd_create", 1);
    }
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() };
    say(&format!(
        "mce-copy: victim pid={pid} va={:#x} len={LENGTH:#x}",
        buffer as usize
    ));
    let until = Instant::now() + Duration::from_secs(SPIN_SECONDS);
    loop {
        // SAFETY: `buffer` is `LENGTH` readable bytes; `fd` is open.
        let wrote = unsafe { libc::pwrite(fd, buffer.cast(), LENGTH, 0) };
        if wrote < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            say(&format!("mce-copy: write failed errno={errno}"));
            break;
        }
        // A write that stored pages before the poisoned one reports them, as
        // for any fault there.
        if wrote as usize != LENGTH {
            say(&format!("mce-copy: write ended short at {wrote:#x}"));
            break;
        }
        if Instant::now() >= until {
            exit_with("mce-copy: write never failed", 3);
        }
    }
    match CODE.load(Ordering::SeqCst) {
        -1 => say("mce-copy: no SIGBUS"),
        code => say(&format!(
            "mce-copy: SIGBUS code={code} addr={:#x} lsb={}",
            ADDRESS.load(Ordering::SeqCst),
            LSB.load(Ordering::SeqCst)
        )),
    }
    let Some(fresh) = fill() else {
        exit_with("mce-copy: FAILED a fresh mmap", 1);
    };
    // SAFETY: as above.
    let wrote = unsafe { libc::pwrite(fd, fresh.cast(), LENGTH, 0) };
    if wrote as usize != LENGTH {
        exit_with("mce-copy: FAILED the fresh write", 1);
    }
    say(&format!("mce-copy: a fresh write of {wrote} bytes"));
    exit_with("mce-copy: victim ends", 0)
}

/// A fresh `LENGTH`-byte buffer, each word [`MAGIC`] above its page.
fn fill() -> Option<*mut u64> {
    // SAFETY: an anonymous private mapping; nothing else refers to it.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            LENGTH,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return None;
    }
    let words = at.cast::<u64>();
    for index in 0..LENGTH / 8 {
        let page = (index * 8 / 4096) as u64;
        // SAFETY: `index` is inside the mapping, which is writable.
        unsafe { words.add(index).write_volatile((MAGIC << 32) | page) };
    }
    Some(words)
}

/// The environ mode: run the target, then read its environment in a loop.
fn environ_mode() -> ExitCode {
    handle_sigbus(on_sigbus_reader);
    let pad = "Z".repeat(PAD);
    // pid 1's own copy is gone once its inputs are released: the gate puts
    // another in the image.
    let spawned = std::process::Command::new("/bin/mce-copy")
        .env_clear()
        .env(TARGET, "1")
        .env("MCE_COPY_PAD", &pad)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            say(&format!("mce-copy: /bin/mce-copy would not run: {error}"));
            return failed("spawning the target");
        }
    };
    let pid = child.id();
    let path = format!("/proc/{pid}/environ");
    let until = Instant::now() + Duration::from_secs(5);
    let full = loop {
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() > PAD => break bytes.len(),
            _ if Instant::now() >= until => return failed("reading the target's environment"),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    say(&format!("mce-copy: target pid={pid} environ={full}"));
    let until = Instant::now() + Duration::from_secs(SPIN_SECONDS);
    loop {
        let read = std::fs::read(&path).map_or(0, |bytes| bytes.len());
        if read < full {
            say(&format!("mce-copy: environ short {read} of {full}"));
            break;
        }
        if Instant::now() >= until {
            say("mce-copy: environ never ended short");
            break;
        }
    }
    if matches!(child.try_wait(), Ok(None)) {
        say("mce-copy: target alive");
    }
    let _ = child.kill();
    let _ = child.wait();
    ExitCode::SUCCESS
}

/// The environ mode's target: a handler that says a `SIGBUS`, then waits.
fn target() -> ! {
    handle_sigbus(on_sigbus_target);
    std::thread::sleep(Duration::from_secs(SPIN_SECONDS * 2));
    // SAFETY: ends the process.
    unsafe { libc::_exit(0) }
}

/// Install `handler` for `SIGBUS` with `SA_SIGINFO`.
fn handle_sigbus(handler: extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void)) {
    // SAFETY: a zeroed `sigaction` is a valid start; every handler here is
    // async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO;
        if libc::sigaction(libc::SIGBUS, &raw const action, std::ptr::null_mut()) != 0 {
            exit_with("mce-copy: FAILED sigaction", 1);
        }
    }
}

/// The victim's handler: keeps `si_code`, `si_addr` and `si_addr_lsb`, and
/// returns, so that the failed write is seen too.
extern "C" fn on_sigbus_keep(_signal: i32, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid `siginfo_t`; `_addr_lsb` is the
    // `short` after `si_addr` in `_sigfault`, at 24 on x86-64.
    let (code, address, lsb) = unsafe {
        let bytes = info.cast::<u8>();
        (
            (*info).si_code,
            (*info).si_addr() as u64,
            bytes.add(24).cast::<i16>().read_unaligned(),
        )
    };
    ADDRESS.store(address, Ordering::SeqCst);
    LSB.store(i64::from(lsb), Ordering::SeqCst);
    CODE.store(code, Ordering::SeqCst);
}

/// The environ reader's handler: says it, which must never happen.
extern "C" fn on_sigbus_reader(_signal: i32, _info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    raw_say(b"mce-copy: reader SIGBUS\n");
}

/// The environ target's handler: says it, which must never happen.
extern "C" fn on_sigbus_target(_signal: i32, _info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    raw_say(b"mce-copy: target SIGBUS\n");
}

/// `write` a line from a signal handler.
fn raw_say(line: &[u8]) {
    // SAFETY: `line` is valid for its length; write is async-signal-safe.
    let _ = unsafe { libc::write(1, line.as_ptr().cast(), line.len()) };
}

/// Say `line` and end the process with `status`.
fn exit_with(line: &str, status: i32) -> ! {
    say(line);
    // SAFETY: ends the process without running the parent's handlers.
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
                say(&format!("mce-copy: {path}: {line}"));
            }
            say(&format!("mce-copy: {path} ends"));
        }
        Err(error) => say(&format!("mce-copy: {path}: {error}")),
    }
}

/// Report a failure and the status for it.
fn failed(what: &str) -> ExitCode {
    say(&format!("mce-copy: FAILED {what}"));
    ExitCode::FAILURE
}
