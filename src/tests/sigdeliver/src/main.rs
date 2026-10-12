//! What the kernel's `sigpaths` boot check proves at the delivery decision,
//! shown end to end by a user program, run as init by
//! `cargo xtask test-sigdeliver`.
//!
//! The boot check drives the kernel's own decision functions: where a handler
//! with `SA_ONSTACK` gets its frame, whether a fault forces its signal, whether
//! an interrupted call restarts. This program makes the real events those
//! decisions answer -- a fault, a stack overflow, a timer and another thread's
//! signal arriving while a read waits -- and requires what a program sees:
//!
//! * **altstack**: a write to a `PROT_NONE` page raises `SIGSEGV` with
//!   `SEGV_ACCERR` and the page's address; the `SA_ONSTACK` handler runs on the
//!   alternate stack (an address of its own frame lies inside it, and
//!   `sigaltstack` reports `SS_ONSTACK`), makes the page writable and returns,
//!   and the write it interrupted is retried and lands. After the return the
//!   program is off the alternate stack again;
//! * **overflow**: a child that recurses until its 8 MiB stack runs into the
//!   guard below it gets `SIGSEGV` on the alternate stack, below where its
//!   stack started, and reports it before `_exit(0)`. The main stack is
//!   exhausted, so a handler can only run because its frame went elsewhere;
//!   this is what `rustc`'s and every Rust program's stack-overflow report
//!   needs;
//! * **alarm-restart** / **alarm-eintr**: a child blocked in `read` of an
//!   empty pipe is interrupted by `SIGALRM` from `ITIMER_REAL`; with
//!   `SA_RESTART` its read restarts, still waits, and returns the eight bytes
//!   written after the handler ran; without it, the read returns `EINTR`;
//! * **thread-restart** / **thread-eintr**: the same, with `SIGUSR2` sent to
//!   the reading thread by `pthread_kill` from another thread of its process.
//!
//! Each restart step has its `EINTR` contrast with the same timing: that the
//! flagless read returns `EINTR` shows the signal did arrive while the read
//! waited, so the restarted read's data was read by a restarted call.
//!
//! Each step prints `sigdeliver: <step> ok`, and the program ends with
//! `sigdeliver: all ok` and status 0, or `sigdeliver: FAILED <step>: <what>`
//! and 1.

use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::io::Write;
use std::time::Duration;

/// How long a step waits for what it requires.
const PATIENCE: Duration = Duration::from_secs(2);
/// How long a restarted read is given to show it still waits.
const SETTLE: Duration = Duration::from_millis(100);
/// How long after arming the timer or starting the thread the signal comes:
/// long enough for the child to be blocked in its read.
const DELAY: Duration = Duration::from_millis(200);
/// [`DELAY`] in microseconds, for `ITIMER_REAL`.
const DELAY_MICROS: i32 = 200_000;
/// The alternate stack's size.
const ALT_SIZE: usize = 64 * 1024;
/// Linux's `si_code` for a fault on a mapped page the access is not allowed
/// to (`include/uapi/asm-generic/siginfo.h`).
const SEGV_ACCERR: i32 = 2;

/// A step's failure.
type Step = Result<(), String>;

/// A named step.
type Named = (&'static str, fn() -> Step);

/// The alternate stack's lowest address, for the handlers.
static ALT_LOW: AtomicUsize = AtomicUsize::new(0);
/// The page the altstack step writes to.
static PAGE: AtomicUsize = AtomicUsize::new(0);
/// How many times the altstack step's handler ran.
static CALLS: AtomicUsize = AtomicUsize::new(0);
/// An address in the altstack step's handler's frame.
static HANDLER_AT: AtomicUsize = AtomicUsize::new(0);
/// The fault address the altstack step's handler was given.
static FAULT_AT: AtomicUsize = AtomicUsize::new(0);
/// The `si_code` the altstack step's handler was given.
static FAULT_CODE: AtomicI32 = AtomicI32::new(0);
/// The flags `sigaltstack` reported inside the altstack step's handler.
static HANDLER_SS_FLAGS: AtomicI32 = AtomicI32::new(-1);
/// The overflow child's stack pointer before it recursed.
static STACK_START: AtomicUsize = AtomicUsize::new(0);
/// Where a handler writes what it saw: the overflow child's report, or the
/// byte that says an interrupting handler ran.
static REPORT_FD: AtomicI32 = AtomicI32::new(-1);

/// Whether `at` lies inside the alternate stack.
fn on_alt(at: usize) -> bool {
    let low = ALT_LOW.load(Ordering::Relaxed);
    at > low && at - low <= ALT_SIZE
}

/// An address in the caller's frame.
#[inline(always)]
fn here() -> usize {
    let marker = 0u8;
    core::hint::black_box(&raw const marker) as usize
}

/// The size of a page.
fn page_size() -> usize {
    // SAFETY: no arguments beyond the name.
    usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096)
}

/// The last OS error, as text.
fn os_error() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Map an alternate stack and install it for the calling thread.
fn install_alt_stack() -> Step {
    // SAFETY: a fresh private anonymous mapping, no file.
    let low = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            ALT_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if low == libc::MAP_FAILED {
        return Err(format!("mmap of the alternate stack: {}", os_error()));
    }
    ALT_LOW.store(low as usize, Ordering::Relaxed);
    let stack = libc::stack_t {
        ss_sp: low,
        ss_flags: 0,
        ss_size: ALT_SIZE,
    };
    // SAFETY: `stack` names the mapping just made, for its whole length.
    if unsafe { libc::sigaltstack(&raw const stack, core::ptr::null_mut()) } != 0 {
        return Err(format!("sigaltstack: {}", os_error()));
    }
    Ok(())
}

/// Install `handler` for `signal` with `SA_SIGINFO` and `flags`.
fn catch(
    signal: i32,
    handler: extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void),
    flags: i32,
) -> Step {
    // SAFETY: a zeroed sigaction with a handler of the three-argument shape
    // `SA_SIGINFO` asks for.
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = handler as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | flags;
        if libc::sigaction(signal, &raw const action, core::ptr::null_mut()) != 0 {
            return Err(format!("sigaction: {}", os_error()));
        }
    }
    Ok(())
}

/// The altstack step's `SIGSEGV` handler: record where it runs and what it
/// was told, make the page writable, and return so the write is retried.
extern "C" fn on_page_fault(_: i32, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    let _ = CALLS.fetch_add(1, Ordering::Relaxed);
    HANDLER_AT.store(here(), Ordering::Relaxed);
    // SAFETY: the kernel passes a valid siginfo with `SA_SIGINFO`.
    let (addr, code) = unsafe { ((*info).si_addr() as usize, (*info).si_code) };
    FAULT_AT.store(addr, Ordering::Relaxed);
    FAULT_CODE.store(code, Ordering::Relaxed);
    // SAFETY: `old` is writable; no new stack is set.
    let mut old: libc::stack_t = unsafe { core::mem::zeroed() };
    if unsafe { libc::sigaltstack(core::ptr::null(), &raw mut old) } == 0 {
        HANDLER_SS_FLAGS.store(old.ss_flags, Ordering::Relaxed);
    }
    let page = PAGE.load(Ordering::Relaxed);
    if addr != page {
        // Any other fault: returning would fault again for ever.
        // SAFETY: ends the process.
        unsafe { libc::_exit(3) };
    }
    // SAFETY: the page this step mapped, one page long.
    let _ = unsafe {
        libc::mprotect(
            page as *mut libc::c_void,
            page_size(),
            libc::PROT_READ | libc::PROT_WRITE,
        )
    };
}

/// altstack.
fn altstack() -> Step {
    install_alt_stack()?;
    catch(libc::SIGSEGV, on_page_fault, libc::SA_ONSTACK)?;
    // SAFETY: a fresh private anonymous mapping no access is allowed to.
    let page = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            page_size(),
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if page == libc::MAP_FAILED {
        return Err(format!("mmap of the page: {}", os_error()));
    }
    PAGE.store(page as usize, Ordering::Relaxed);
    let main_at = here();
    // SAFETY: the page is mapped; the write faults, and the handler makes it
    // writable before the write is retried.
    unsafe { core::ptr::write_volatile(page.cast::<u32>(), 0x5167_5345) };
    // SAFETY: the page is readable now.
    let read_back = unsafe { core::ptr::read_volatile(page.cast::<u32>()) };

    let calls = CALLS.load(Ordering::Relaxed);
    if calls != 1 {
        return Err(format!(
            "the handler ran {calls} times for one fault, not once"
        ));
    }
    let code = FAULT_CODE.load(Ordering::Relaxed);
    if code != SEGV_ACCERR {
        println!("sigdeliver: DIAG the fault's si_code was {code}, not SEGV_ACCERR");
    }
    let at = HANDLER_AT.load(Ordering::Relaxed);
    if !on_alt(at) {
        return Err(format!(
            "the handler ran at {at:#x}, outside the alternate stack at {:#x} (the program's stack \
             is at {main_at:#x})",
            ALT_LOW.load(Ordering::Relaxed)
        ));
    }
    let flags = HANDLER_SS_FLAGS.load(Ordering::Relaxed);
    if flags & libc::SS_ONSTACK == 0 {
        return Err(format!(
            "sigaltstack in the handler reported flags {flags}, without SS_ONSTACK"
        ));
    }
    if read_back != 0x5167_5345 {
        return Err(format!("the retried write left {read_back:#x}"));
    }
    // SAFETY: `old` is writable; no new stack is set.
    let mut old: libc::stack_t = unsafe { core::mem::zeroed() };
    if unsafe { libc::sigaltstack(core::ptr::null(), &raw mut old) } != 0 {
        return Err(format!("sigaltstack: {}", os_error()));
    }
    if old.ss_flags & libc::SS_ONSTACK != 0 {
        return Err(
            "after the handler returned the program was still on the alternate stack".to_owned(),
        );
    }
    // SAFETY: restoring the default action.
    let _ = unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
    Ok(())
}

/// The overflow child's `SIGSEGV` handler: report whether it runs on the
/// alternate stack and whether the fault lay below the stack's start, and end.
extern "C" fn on_overflow(_: i32, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo with `SA_SIGINFO`.
    let addr = unsafe { (*info).si_addr() as usize };
    let start = STACK_START.load(Ordering::Relaxed);
    let below = addr < start && start - addr <= 16 * 1024 * 1024;
    let report = [b'S', u8::from(on_alt(here())), u8::from(below)];
    // SAFETY: `report` is readable for its length; the child then ends.
    unsafe {
        let _ = libc::write(
            REPORT_FD.load(Ordering::Relaxed),
            report.as_ptr().cast(),
            report.len(),
        );
        libc::_exit(0);
    }
}

/// Recurse with a page of stack a level until the stack runs out.
#[inline(never)]
fn descend(depth: usize) -> usize {
    let frame = core::hint::black_box([u8::try_from(depth & 0xff).unwrap_or(0); 4096]);
    if core::hint::black_box(depth) == usize::MAX {
        return 0;
    }
    descend(depth + 1).wrapping_add(usize::from(frame.first().copied().unwrap_or(0)))
}

/// Two ends of a new pipe, read end first.
fn pipe() -> Result<(i32, i32), String> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` holds the two descriptors `pipe` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", os_error()));
    }
    Ok((fds[0], fds[1]))
}

/// Up to `N` bytes from `fd`, if any came within `within`.
fn receive<const N: usize>(fd: i32, within: Duration) -> Option<[u8; N]> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(within.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one pollfd, valid for the call.
    if unsafe { libc::poll(&raw mut poll, 1, millis) } <= 0 {
        return None;
    }
    let mut got = [0u8; N];
    // SAFETY: `got` is writable for its length.
    let n = unsafe { libc::read(fd, got.as_mut_ptr().cast(), got.len()) };
    (usize::try_from(n).ok() == Some(N)).then_some(got)
}

/// Reap `pid` and return its status.
fn reap(pid: libc::pid_t) -> Result<i32, String> {
    let start = std::time::Instant::now();
    while start.elapsed() < PATIENCE {
        let mut status = 0;
        // SAFETY: `status` is writable; the pid is our child's.
        let got = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        if got == pid {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err("the child did not end".to_owned())
}

/// Fork, running `child` in the child, which must not return.
fn fork(child: impl FnOnce()) -> Result<libc::pid_t, String> {
    // SAFETY: one thread in the parent; the child makes system calls, may
    // start threads of its own, and ends with `_exit`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", os_error()));
    }
    if pid == 0 {
        child();
        // SAFETY: ends the child.
        unsafe { libc::_exit(4) };
    }
    Ok(pid)
}

/// overflow.
fn overflow() -> Step {
    let (report_r, report_w) = pipe()?;
    let pid = fork(|| {
        REPORT_FD.store(report_w, Ordering::Relaxed);
        if install_alt_stack().is_err()
            || catch(libc::SIGSEGV, on_overflow, libc::SA_ONSTACK).is_err()
        {
            // SAFETY: ends the child.
            unsafe { libc::_exit(5) };
        }
        STACK_START.store(here(), Ordering::Relaxed);
        let _ = core::hint::black_box(descend(0));
    })?;
    // SAFETY: closing the parent's copy of the child's end.
    let _ = unsafe { libc::close(report_w) };
    let report = receive::<3>(report_r, PATIENCE * 5);
    let status = reap(pid);
    // SAFETY: closing our own descriptor.
    let _ = unsafe { libc::close(report_r) };
    let status = status?;
    match report {
        None if libc::WIFSIGNALED(status) => Err(format!(
            "the child that overflowed its stack was ended by signal {} before its handler reported: \
             no frame on the alternate stack",
            libc::WTERMSIG(status)
        )),
        None => Err(format!(
            "the child that overflowed its stack reported nothing (status {status:#x})"
        )),
        Some([b'S', 1, 1]) if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 => Ok(()),
        Some([b'S', 0, _]) => {
            Err("the overflow's handler ran outside the alternate stack".to_owned())
        }
        Some([b'S', _, 0]) => {
            Err("the overflow's fault address was not below the stack's start".to_owned())
        }
        Some(got) => Err(format!(
            "the child reported {got:?} and ended with {status:#x}"
        )),
    }
}

/// An interrupting handler: say that it ran, and nothing else.
extern "C" fn on_interrupt(_: i32, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    let ran = b"R";
    // SAFETY: `ran` is readable for its length.
    let _ = unsafe { libc::write(REPORT_FD.load(Ordering::Relaxed), ran.as_ptr().cast(), 1) };
}

/// What interrupts the child's read.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// `SIGALRM` from `ITIMER_REAL`.
    Alarm,
    /// `SIGUSR2` from another thread of the child, by `pthread_kill`.
    Thread,
}

/// Fork a child blocked in `read` of an empty pipe that `source` interrupts,
/// with `SA_RESTART` if `restart`; wait for its handler to run; then require
/// the read to restart and read what is written, or to answer `EINTR`.
fn interrupted_read(source: Source, restart: bool) -> Step {
    let (data_r, data_w) = pipe()?;
    let (result_r, result_w) = pipe()?;
    let (ran_r, ran_w) = pipe()?;
    let signal = match source {
        Source::Alarm => libc::SIGALRM,
        Source::Thread => libc::SIGUSR2,
    };
    let pid = fork(|| {
        REPORT_FD.store(ran_w, Ordering::Relaxed);
        let flags = if restart { libc::SA_RESTART } else { 0 };
        if catch(signal, on_interrupt, flags).is_err() {
            // SAFETY: ends the child.
            unsafe { libc::_exit(5) };
        }
        match source {
            Source::Alarm => {
                #[allow(
                    clippy::useless_conversion,
                    reason = "`tv_usec` is 64 bits wide on x86-64 and AArch64, 32 on ARMv7-A"
                )]
                let timer = libc::itimerval {
                    it_interval: libc::timeval {
                        tv_sec: 0,
                        tv_usec: 0,
                    },
                    it_value: libc::timeval {
                        tv_sec: 0,
                        tv_usec: DELAY_MICROS.into(),
                    },
                };
                // SAFETY: `timer` is readable; the old value is not asked for.
                if unsafe {
                    libc::setitimer(libc::ITIMER_REAL, &raw const timer, core::ptr::null_mut())
                } != 0
                {
                    // SAFETY: ends the child.
                    unsafe { libc::_exit(6) };
                }
            }
            Source::Thread => {
                // SAFETY: this thread's own handle.
                // A handle is a pointer on musl, which is not `Send`: carried
                // across as its address.
                let reader = unsafe { libc::pthread_self() } as usize;
                let _ = std::thread::spawn(move || {
                    std::thread::sleep(DELAY);
                    // SAFETY: the reading thread lives until the child ends.
                    let _ = unsafe { libc::pthread_kill(reader as libc::pthread_t, signal) };
                });
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
        // SAFETY: `report` is readable for its length.
        let _ = unsafe { libc::write(result_w, report.as_ptr().cast(), report.len()) };
        // SAFETY: ends the child.
        unsafe { libc::_exit(0) };
    })?;
    // SAFETY: closing the parent's copies of the child's ends.
    unsafe {
        let _ = libc::close(data_r);
        let _ = libc::close(result_w);
        let _ = libc::close(ran_w);
    }
    let outcome = judge(source, restart, data_w, result_r, ran_r);
    if outcome.is_err() {
        // SAFETY: a signal to our own child, which may still wait.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let reaped = reap(pid);
    // SAFETY: closing our own descriptors.
    unsafe {
        let _ = libc::close(data_w);
        let _ = libc::close(result_r);
        let _ = libc::close(ran_r);
    }
    outcome?;
    reaped.map(|_| ())
}

/// The parent's side of [`interrupted_read`].
fn judge(source: Source, restart: bool, data_w: i32, result_r: i32, ran_r: i32) -> Step {
    let what = match source {
        Source::Alarm => "SIGALRM from ITIMER_REAL",
        Source::Thread => "SIGUSR2 from another thread",
    };
    if receive::<1>(ran_r, PATIENCE).is_none() {
        return Err(format!("the handler for {what} never ran"));
    }
    if restart {
        std::thread::sleep(SETTLE);
        if let Some(got) = receive::<2>(result_r, Duration::ZERO) {
            return Err(format!(
                "the read interrupted by {what} under SA_RESTART answered {got:?} instead of restarting"
            ));
        }
        // SAFETY: eight readable bytes.
        if unsafe { libc::write(data_w, b"restarts".as_ptr().cast(), 8) } != 8 {
            return Err(format!("write: {}", os_error()));
        }
        match receive::<2>(result_r, PATIENCE) {
            Some([b'D', 8]) => Ok(()),
            Some(got) => Err(format!("the restarted read answered {got:?}, not 8 bytes")),
            None => Err(format!(
                "the read restarted after {what} never read what was written"
            )),
        }
    } else {
        let eintr = u8::try_from(libc::EINTR).unwrap_or(0);
        match receive::<2>(result_r, PATIENCE) {
            Some([b'E', e]) if e == eintr => Ok(()),
            Some(got) => Err(format!(
                "the read interrupted by {what} without SA_RESTART answered {got:?}, not EINTR"
            )),
            None => Err(format!(
                "the read interrupted by {what} without SA_RESTART never answered"
            )),
        }
    }
}

fn main() {
    let steps: [Named; 6] = [
        ("altstack", altstack),
        ("overflow", overflow),
        ("alarm-restart", || interrupted_read(Source::Alarm, true)),
        ("alarm-eintr", || interrupted_read(Source::Alarm, false)),
        ("thread-restart", || interrupted_read(Source::Thread, true)),
        ("thread-eintr", || interrupted_read(Source::Thread, false)),
    ];
    for (name, step) in steps {
        if let Err(why) = step() {
            println!("sigdeliver: FAILED {name}: {why}");
            let _ = std::io::stdout().flush();
            std::process::exit(1);
        }
        println!("sigdeliver: {name} ok");
        let _ = std::io::stdout().flush();
    }
    println!("sigdeliver: all ok");
    let _ = std::io::stdout().flush();
}
