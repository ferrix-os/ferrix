//! seccomp-bpf as a program uses it, run as init by `cargo xtask test-seccomp`
//! (`docs/SECCOMP.md` §8.2): through raw `seccomp(2)` and `prctl(2)` calls and
//! hand-written classic-BPF programs, as Chromium, bubblewrap and Flatpak write
//! them, on x86-64, i386 on x86-64, AArch64 and ARMv7-A.
//!
//! Each step prints `seccomp: <step> ok`, and the program ends with
//! `seccomp: all ok` and status 0, or `seccomp: FAILED <step>: <what>` and
//! status 1.
//!
//! * **probes**: Chromium's detection (`prctl(PR_SET_SECCOMP, 2, NULL)` and
//!   `seccomp(SET_MODE_FILTER, flag, NULL)` are `EFAULT`, an unknown flag
//!   `EINVAL`), `GET_ACTION_AVAIL`, the speculation controls;
//! * **errno**: `ERRNO` for one call and `ENOSYS` for `clone3`, an errno of
//!   512 coming back as 512, one past 4095 as 4095;
//! * **order**: the strictest answer of a chain wins, the newest filter's data
//!   on a tie, `TRACE` and `USER_NOTIF` are `ENOSYS`;
//! * **unprivileged**: a child that is not root and has not set no-new-privs
//!   is refused `EACCES`;
//! * **status**: `/proc/self/status` shows `NoNewPrivs`, `Seccomp` and
//!   `Seccomp_filters`;
//! * **twoarch**: one filter for x86-64 and i386 together, as Flatpak's, refusing
//!   System V IPC in each one's own numbers and nothing else;
//! * **emulate**: Chromium's shape: an `arch` check, the x32 bit, a `TRAP` with
//!   data for one call, and a `SIGSYS` handler that checks Chromium's five
//!   sanity conditions and writes the call's result into the context, which
//!   the program then gets back from the trapped call; with `SIGSYS` blocked,
//!   and with a trap inside the handler (`SA_NODEFER`);
//! * **ignored**: with `SIGSYS` ignored, a trapped call ends the process with
//!   `SIGSYS`;
//! * **kill**: `KILL_PROCESS` and `KILL_THREAD` end a single-threaded process
//!   with `SIGSYS`, and a thread killed by its filter leaves the others;
//! * **strict**: strict mode allows `write` and ends the thread on `getpid`
//!   with `SIGKILL`;
//! * **bwrap**: bubblewrap's path: no-new-privs, `prctl(PR_SET_SECCOMP, 2,
//!   &prog)`, then `execve` of a program that finds itself filtered;
//! * **tsync**: eight threads and a ninth making more while `TSYNC` runs: every
//!   thread, those too, is refused; a thread with a filter of its own makes
//!   `TSYNC` answer its id, or `ESRCH` with `TSYNC_ESRCH`, and changes none.
//!
//! Built with `negative-control-trap`, the handler leaves the result out, and
//! the program must fail on the emulation step. Built with
//! `negative-control-privilege`, the unprivileged step sets no-new-privs
//! first, and must fail on that step.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use libc::{c_int, c_long, c_ulong, c_void};

/// A step's failure.
type Step = Result<(), String>;

/// A named step.
type Named = (&'static str, fn() -> Step);

// ---------------------------------------------------------------------------
// The kernel's constants, from `linux/seccomp.h`, `linux/filter.h`,
// `linux/audit.h` and `linux/prctl.h`.
// ---------------------------------------------------------------------------

/// `SECCOMP_SET_MODE_FILTER`.
const SET_MODE_FILTER: c_ulong = 1;
/// `SECCOMP_GET_ACTION_AVAIL`.
const GET_ACTION_AVAIL: c_ulong = 2;
/// `SECCOMP_GET_NOTIF_SIZES`.
const GET_NOTIF_SIZES: c_ulong = 3;
/// `SECCOMP_FILTER_FLAG_TSYNC`.
const FLAG_TSYNC: c_ulong = 1 << 0;
/// `SECCOMP_FILTER_FLAG_LOG`.
const FLAG_LOG: c_ulong = 1 << 1;
/// `SECCOMP_FILTER_FLAG_SPEC_ALLOW`.
const FLAG_SPEC_ALLOW: c_ulong = 1 << 2;
/// `SECCOMP_FILTER_FLAG_NEW_LISTENER`.
const FLAG_NEW_LISTENER: c_ulong = 1 << 3;
/// `SECCOMP_FILTER_FLAG_TSYNC_ESRCH`.
const FLAG_TSYNC_ESRCH: c_ulong = 1 << 4;
/// A flag bit Linux does not know.
const FLAG_UNKNOWN: c_ulong = 1 << 6;

/// `SECCOMP_RET_KILL_PROCESS`.
const RET_KILL_PROCESS: u32 = 0x8000_0000;
/// `SECCOMP_RET_KILL_THREAD`.
const RET_KILL_THREAD: u32 = 0x0000_0000;
/// `SECCOMP_RET_TRAP`.
const RET_TRAP: u32 = 0x0003_0000;
/// `SECCOMP_RET_ERRNO`.
const RET_ERRNO: u32 = 0x0005_0000;
/// `SECCOMP_RET_USER_NOTIF`.
const RET_USER_NOTIF: u32 = 0x7fc0_0000;
/// `SECCOMP_RET_TRACE`.
const RET_TRACE: u32 = 0x7ff0_0000;
/// `SECCOMP_RET_LOG`.
const RET_LOG: u32 = 0x7ffc_0000;
/// `SECCOMP_RET_ALLOW`.
const RET_ALLOW: u32 = 0x7fff_0000;

/// `PR_SET_SECCOMP`.
const PR_SET_SECCOMP: c_int = 22;
/// `PR_GET_SECCOMP`.
const PR_GET_SECCOMP: c_int = 21;
/// `PR_SET_NO_NEW_PRIVS`.
const PR_SET_NO_NEW_PRIVS: c_int = 38;
/// `PR_GET_SPECULATION_CTRL`.
const PR_GET_SPECULATION_CTRL: c_int = 52;
/// `PR_SET_SPECULATION_CTRL`.
const PR_SET_SPECULATION_CTRL: c_int = 53;
/// `SECCOMP_MODE_FILTER` for `prctl`.
const MODE_FILTER: c_ulong = 2;
/// `SECCOMP_MODE_STRICT` for `prctl`.
const MODE_STRICT: c_ulong = 1;

/// `AUDIT_ARCH_X86_64`.
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
/// `AUDIT_ARCH_I386`.
const AUDIT_ARCH_I386: u32 = 0x4000_0003;
/// `AUDIT_ARCH_AARCH64`.
const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;
/// `AUDIT_ARCH_ARM`.
const AUDIT_ARCH_ARM: u32 = 0x4000_0028;

/// The token of the ABI this program is built for.
const OWN_ARCH: u32 = if cfg!(target_arch = "x86_64") {
    AUDIT_ARCH_X86_64
} else if cfg!(target_arch = "x86") {
    AUDIT_ARCH_I386
} else if cfg!(target_arch = "aarch64") {
    AUDIT_ARCH_AARCH64
} else {
    AUDIT_ARCH_ARM
};

/// `__X32_SYSCALL_BIT`.
const X32_BIT: u32 = 0x4000_0000;

/// Classic BPF opcodes, as `linux/bpf_common.h` spells them.
const BPF_LD_W_ABS: u16 = 0x20;
/// `BPF_JMP | BPF_JEQ | BPF_K`.
const BPF_JEQ_K: u16 = 0x15;
/// `BPF_JMP | BPF_JSET | BPF_K`.
const BPF_JSET_K: u16 = 0x45;
/// `BPF_RET | BPF_K`.
const BPF_RET_K: u16 = 0x06;

/// `struct sock_filter`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SockFilter {
    /// The opcode.
    code: u16,
    /// Jump if true.
    jt: u8,
    /// Jump if false.
    jf: u8,
    /// The operand.
    k: u32,
}

/// `struct sock_fprog`.
#[repr(C)]
#[derive(Debug)]
struct SockFprog {
    /// Instructions.
    len: u16,
    /// Where they are.
    filter: *const SockFilter,
}

/// Chromium's `BPF_STMT`.
const fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

/// Chromium's `BPF_JUMP`.
const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Load `seccomp_data.nr`.
const LOAD_NR: SockFilter = stmt(BPF_LD_W_ABS, 0);
/// Load `seccomp_data.arch`.
const LOAD_ARCH: SockFilter = stmt(BPF_LD_W_ABS, 4);

/// A filter that answers `action` for the numbers named and allows every other:
/// `ld nr`, then for each a `jeq` and a return.
fn answering(calls: &[(c_long, u32)]) -> Vec<SockFilter> {
    let mut program = vec![LOAD_NR];
    for &(number, action) in calls {
        program.push(jump(BPF_JEQ_K, number as u32, 0, 1));
        program.push(stmt(BPF_RET_K, action));
    }
    program.push(stmt(BPF_RET_K, RET_ALLOW));
    program
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

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

/// `seccomp(operation, flags, args)`: the result and the errno it left.
fn seccomp(operation: c_ulong, flags: c_ulong, args: *const c_void) -> (c_long, c_int) {
    // SAFETY: the kernel reads `args` as the operation says, and every caller
    // passes a null pointer or a live structure of the right kind.
    let answer = unsafe { libc::syscall(libc::SYS_seccomp, operation, flags, args) };
    (answer, errno())
}

/// Install `program` with `seccomp(SET_MODE_FILTER, flags)`.
fn install(program: &[SockFilter], flags: c_ulong) -> (c_long, c_int) {
    let prog = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };
    seccomp(SET_MODE_FILTER, flags, (&raw const prog).cast())
}

/// Install `program` as bubblewrap does, with `prctl(PR_SET_SECCOMP, 2, &prog)`.
fn install_prctl(program: &[SockFilter]) -> (c_int, c_int) {
    let prog = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };
    // SAFETY: `prog` and its instructions are alive for the call.
    let answer = unsafe { libc::prctl(PR_SET_SECCOMP, MODE_FILTER, &raw const prog, 0, 0) };
    (answer, errno())
}

/// `prctl(option, a, b, c, d)`.
fn prctl(option: c_int, a: c_ulong, b: c_ulong, c: c_ulong, d: c_ulong) -> (c_int, c_int) {
    // SAFETY: the options used take plain integers.
    let answer = unsafe { libc::prctl(option, a, b, c, d) };
    (answer, errno())
}

/// `no_new_privs`.
fn no_new_privs() -> Step {
    match prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) {
        (0, _) => Ok(()),
        (answer, error) => Err(format!(
            "PR_SET_NO_NEW_PRIVS answered {answer}, errno {error}"
        )),
    }
}

/// A raw system call with no arguments: the result and the errno.
fn call0(number: c_long) -> (c_long, c_int) {
    // SAFETY: the calls made here take no arguments or ignore them.
    let answer = unsafe { libc::syscall(number) };
    (answer, errno())
}

/// A raw system call with three arguments.
fn call3(number: c_long, a: c_long, b: c_long, c: c_long) -> (c_long, c_int) {
    // SAFETY: the calls made here take plain integers.
    let answer = unsafe { libc::syscall(number, a, b, c) };
    (answer, errno())
}

/// Fork, running `child` in the child, which ends with its answer.
fn fork_with(child: impl FnOnce() -> c_int) -> Result<libc::pid_t, String> {
    // SAFETY: the child calls only libc functions and `_exit`s.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork failed, errno {}", errno()));
    }
    if pid == 0 {
        let status = child();
        // SAFETY: ends the child without running the parent's destructors.
        unsafe { libc::_exit(status) };
    }
    Ok(pid)
}

/// Wait for `pid` and return its exit status, or 1000 plus the signal that
/// ended it.
fn reap(pid: libc::pid_t) -> c_int {
    let mut status = 0;
    // SAFETY: status is a live int.
    let _ = unsafe { libc::waitpid(pid, &mut status, 0) };
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1000 + libc::WTERMSIG(status)
    }
}

/// What a child that ends by `SIGSYS` reports.
const BY_SIGSYS: c_int = 1000 + libc::SIGSYS;
/// What a child that ends by `SIGKILL` reports.
const BY_SIGKILL: c_int = 1000 + libc::SIGKILL;

/// `/proc/self/status`'s value for `key`.
fn status_field(key: &str) -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .map(|rest| rest.trim().to_string())
}

// ---------------------------------------------------------------------------
// Calls whose numbers are the ABI's
// ---------------------------------------------------------------------------

/// `getppid`.
const GETPPID: c_long = libc::SYS_getppid as c_long;
/// `getpid`.
const GETPID: c_long = libc::SYS_getpid as c_long;
/// `getuid`.
const GETUID: c_long = libc::SYS_getuid as c_long;
/// `getgid`.
const GETGID: c_long = libc::SYS_getgid as c_long;
/// `getsid`.
const GETSID: c_long = libc::SYS_getsid as c_long;
/// `gettid`.
const GETTID: c_long = libc::SYS_gettid as c_long;
/// `write`.
const WRITE: c_long = libc::SYS_write as c_long;
/// `clone3`, which no libc here defines: 435 on every ABI Ferrix has.
const CLONE3: c_long = 435;

/// **probes**.
fn probes() -> Step {
    // Chromium's detection: a NULL program is `EFAULT` for `prctl` and for
    // every flag the kernel knows.
    let (answer, error) = prctl(PR_SET_SECCOMP, MODE_FILTER, 0, 0, 0);
    if answer != -1 || error != libc::EFAULT {
        return Err(format!(
            "prctl(PR_SET_SECCOMP, 2, NULL) answered {answer}, errno {error}"
        ));
    }
    for flags in [
        0,
        FLAG_TSYNC,
        FLAG_LOG,
        FLAG_SPEC_ALLOW,
        FLAG_TSYNC | FLAG_TSYNC_ESRCH,
    ] {
        let (answer, error) = seccomp(SET_MODE_FILTER, flags, std::ptr::null());
        if answer != -1 || error != libc::EFAULT {
            return Err(format!(
                "seccomp(SET_MODE_FILTER, {flags:#x}, NULL) answered {answer}, errno {error}"
            ));
        }
    }
    // A flag that is not built or not known is `EINVAL` before the program is
    // read; Chromium trips a DCHECK on anything else.
    for flags in [FLAG_NEW_LISTENER, FLAG_TSYNC_ESRCH, FLAG_UNKNOWN, 1 << 31] {
        let (answer, error) = seccomp(SET_MODE_FILTER, flags, std::ptr::null());
        if answer != -1 || error != libc::EINVAL {
            return Err(format!(
                "seccomp(SET_MODE_FILTER, {flags:#x}, NULL) answered {answer}, errno {error}, \
                 not EINVAL"
            ));
        }
    }
    let (answer, error) = seccomp(99, 0, std::ptr::null());
    if answer != -1 || error != libc::EINVAL {
        return Err(format!(
            "an unknown operation answered {answer}, errno {error}"
        ));
    }
    // Which actions a filter may return.
    for (action, available) in [
        (RET_KILL_PROCESS, true),
        (RET_KILL_THREAD, true),
        (RET_TRAP, true),
        (RET_ERRNO, true),
        (RET_TRACE, true),
        (RET_LOG, true),
        (RET_ALLOW, true),
        (RET_USER_NOTIF, false),
    ] {
        let (answer, error) = seccomp(GET_ACTION_AVAIL, 0, (&raw const action).cast());
        let good = if available {
            answer == 0
        } else {
            answer == -1 && error == libc::EOPNOTSUPP
        };
        if !good {
            return Err(format!(
                "GET_ACTION_AVAIL {action:#x} answered {answer}, errno {error}"
            ));
        }
    }
    let mut sizes = [0_u16; 3];
    let (answer, _) = seccomp(GET_NOTIF_SIZES, 0, sizes.as_mut_ptr().cast());
    if answer != 0 || sizes != [80, 24, 64] {
        return Err(format!(
            "GET_NOTIF_SIZES answered {answer}, sizes {sizes:?}"
        ));
    }
    // The speculation controls: mitigated and force-disabled (`PR_SPEC_PRCTL |
    // PR_SPEC_FORCE_DISABLE`), and enabling one is refused.
    for which in [0, 1] {
        let (answer, error) = prctl(PR_GET_SPECULATION_CTRL, which, 0, 0, 0);
        if answer != 9 {
            return Err(format!(
                "PR_GET_SPECULATION_CTRL {which} answered {answer}, errno {error}, not 9"
            ));
        }
    }
    let (answer, error) = prctl(PR_SET_SPECULATION_CTRL, 1, 2, 0, 0);
    if answer != -1 || error != libc::EPERM {
        return Err(format!(
            "enabling indirect branch speculation answered {answer}, errno {error}"
        ));
    }
    // And the mode a thread with nothing installed reads.
    let (mode, _) = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
    if mode != 0 {
        return Err(format!(
            "PR_GET_SECCOMP of an unfiltered thread read {mode}"
        ));
    }
    Ok(())
}

/// **errno**: in a child, so that the filter dies with it.
fn errno_filters() -> Step {
    let child = fork_with(|| {
        if no_new_privs().is_err() {
            return 2;
        }
        let program = answering(&[
            (GETPPID, RET_ERRNO | libc::EPERM as u32),
            (CLONE3, RET_ERRNO | libc::ENOSYS as u32),
            (GETUID, RET_ERRNO | 512),
            (GETGID, RET_ERRNO | 5000),
        ]);
        if install(&program, 0).0 != 0 {
            return 3;
        }
        let (answer, error) = call0(GETPPID);
        if answer != -1 || error != libc::EPERM {
            return 4;
        }
        let (answer, error) = call3(CLONE3, 0, 0, 0);
        if answer != -1 || error != libc::ENOSYS {
            return 5;
        }
        // The calls the filter does not name run.
        let (pid, _) = call0(GETPID);
        if pid != std::process::id() as c_long {
            return 6;
        }
        // 512 is the kernel's restart code; a filter's is an errno like any.
        let (answer, error) = call0(GETUID);
        if answer != -1 || error != 512 {
            return 7;
        }
        let (answer, error) = call0(GETGID);
        if answer != -1 || error != 4095 {
            return 8;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        2 => Err("no-new-privs was refused".into()),
        3 => Err("a filter was refused".into()),
        4 => Err("a filtered getppid did not fail with EPERM".into()),
        5 => Err("a filtered clone3 did not fail with ENOSYS".into()),
        6 => Err("a call the filter does not name did not run".into()),
        7 => Err("a filter's errno 512 did not come back as 512".into()),
        8 => Err("a filter's errno 5000 did not come back as 4095".into()),
        other => Err(format!("the child ended with {other}")),
    }
}

/// **order**.
fn order() -> Step {
    let child = fork_with(|| {
        if no_new_privs().is_err() {
            return 2;
        }
        // An older ERRNO under a newer ALLOW: still ERRNO.
        if install(&answering(&[(GETPPID, RET_ERRNO | libc::EPERM as u32)]), 0).0 != 0
            || install(&answering(&[(GETPPID, RET_ALLOW)]), 0).0 != 0
        {
            return 3;
        }
        if call0(GETPPID) != (-1, libc::EPERM) {
            return 4;
        }
        // Two ERRNOs: the newest filter's data.
        if install(&answering(&[(GETPPID, RET_ERRNO | libc::EACCES as u32)]), 0).0 != 0 {
            return 5;
        }
        if call0(GETPPID) != (-1, libc::EACCES) {
            return 6;
        }
        // A newer ERRNO over an older ALLOW, for a call only it names.
        if install(&answering(&[(GETUID, RET_ERRNO | libc::EBADF as u32)]), 0).0 != 0 {
            return 7;
        }
        if call0(GETUID) != (-1, libc::EBADF) {
            return 8;
        }
        // TRACE and USER_NOTIF have no tracer or listener: ENOSYS, the call
        // does not run.
        if install(
            &answering(&[(GETGID, RET_TRACE), (GETSID, RET_USER_NOTIF)]),
            0,
        )
        .0 != 0
        {
            return 9;
        }
        if call0(GETGID) != (-1, libc::ENOSYS) || call3(GETSID, 0, 0, 0) != (-1, libc::ENOSYS) {
            return 10;
        }
        // LOG lets the call go on.
        if install(&answering(&[(GETPID, RET_LOG)]), FLAG_LOG).0 != 0 {
            return 11;
        }
        if call0(GETPID).0 != std::process::id() as c_long {
            return 12;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        4 => Err("a newer ALLOW overrode an older ERRNO".into()),
        6 => Err("the newest filter's data did not stand on a tie".into()),
        8 => Err("a newer ERRNO did not hold over an older ALLOW".into()),
        10 => Err("a TRACE or USER_NOTIF result did not fail ENOSYS".into()),
        12 => Err("a LOG result stopped the call".into()),
        other => Err(format!("the child failed at step {other}")),
    }
}

/// **unprivileged**: not root, no no-new-privs.
fn unprivileged() -> Step {
    let child = fork_with(|| {
        // SAFETY: sets the child's own ids.
        if unsafe { libc::setgid(1000) } != 0 || unsafe { libc::setuid(1000) } != 0 {
            return 2;
        }
        if cfg!(feature = "negative-control-privilege") && no_new_privs().is_err() {
            return 3;
        }
        let (answer, error) = install(&answering(&[]), 0);
        if answer != -1 || error != libc::EACCES {
            return 4;
        }
        if no_new_privs().is_err() {
            return 5;
        }
        if install(&answering(&[]), 0).0 != 0 {
            return 6;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        2 => Err("the child could not become uid 1000".into()),
        4 => Err("an unprivileged install without no-new-privs was not refused EACCES".into()),
        6 => Err("an unprivileged install with no-new-privs was refused".into()),
        other => Err(format!("the child failed at step {other}")),
    }
}

/// **status**.
fn status() -> Step {
    let child = fork_with(|| {
        if no_new_privs().is_err() {
            return 2;
        }
        let before = status_field("Seccomp_filters:");
        if status_field("Seccomp:").as_deref() != Some("0")
            || status_field("NoNewPrivs:").as_deref() != Some("1")
            || before.as_deref() != Some("0")
        {
            return 3;
        }
        if install(&answering(&[]), 0).0 != 0 || install(&answering(&[]), 0).0 != 0 {
            return 4;
        }
        if status_field("Seccomp:").as_deref() != Some("2")
            || status_field("Seccomp_filters:").as_deref() != Some("2")
        {
            return 5;
        }
        // A fork child holds the same chain.
        let Ok(grandchild) = fork_with(|| {
            if status_field("Seccomp:").as_deref() == Some("2")
                && status_field("Seccomp_filters:").as_deref() == Some("2")
                && status_field("NoNewPrivs:").as_deref() == Some("1")
            {
                0
            } else {
                1
            }
        }) else {
            return 6;
        };
        if reap(grandchild) != 0 {
            return 7;
        }
        let (mode, _) = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
        if mode != 2 {
            return 8;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        3 => Err("/proc/self/status of an unfiltered process did not read 0 filters".into()),
        5 => Err("/proc/self/status did not show the mode and two filters".into()),
        7 => Err("a forked child did not hold its parent's chain".into()),
        8 => Err("PR_GET_SECCOMP did not read 2".into()),
        other => Err(format!("the child failed at step {other}")),
    }
}

/// The System V IPC calls the two-architecture filter refuses, per ABI, from
/// `asm/unistd_64.h`, `asm/unistd_32.h`, `asm-generic/unistd.h` and
/// `asm/unistd-eabi.h`: the `get` calls of each, and i386's `ipc`
/// multiplexer (117), through which musl reaches them there.
const ABIS: [(u32, &[u32]); 4] = [
    (AUDIT_ARCH_X86_64, &[29, 64, 68]),
    (AUDIT_ARCH_I386, &[117, 393, 395, 399]),
    (AUDIT_ARCH_AARCH64, &[186, 190, 194]),
    (AUDIT_ARCH_ARM, &[299, 303, 307]),
];

/// Flatpak's one filter for every ABI there is: `ld arch`, a `jeq` per ABI to
/// its own block, `ret ALLOW` for any other `arch`, and per block `ld nr` with
/// `ENOSYS` for each of its IPC calls and `ALLOW` for the rest.
fn all_abis_filter() -> Vec<SockFilter> {
    let blocks = ABIS.len();
    // Index of the first block: the `ld arch`, a `jeq` each, the `ret ALLOW`.
    let mut starts = Vec::new();
    let mut at = 1 + blocks + 1;
    for (_, numbers) in ABIS {
        starts.push(at);
        at += 1 + 2 * numbers.len() + 1;
    }
    let mut program = vec![LOAD_ARCH];
    for (position, ((arch, _), start)) in ABIS.iter().zip(&starts).enumerate() {
        let next = 1 + position + 1;
        program.push(jump(BPF_JEQ_K, *arch, (start - next) as u8, 0));
    }
    program.push(stmt(BPF_RET_K, RET_ALLOW));
    for (_, numbers) in ABIS {
        program.push(LOAD_NR);
        for &number in numbers {
            program.push(jump(BPF_JEQ_K, number, 0, 1));
            program.push(stmt(BPF_RET_K, RET_ERRNO | libc::ENOSYS as u32));
        }
        program.push(stmt(BPF_RET_K, RET_ALLOW));
    }
    program
}

/// **twoarch**: Flatpak's one filter for every ABI, which each ABI's build
/// installs and is refused its own calls by, and nothing else: x86-64 and i386
/// share the image on the x86-64 kernel, each judged under its own `arch`.
fn two_architectures() -> Step {
    let child = fork_with(|| {
        if no_new_privs().is_err() {
            return 2;
        }
        if install(&all_abis_filter(), 0).0 != 0 {
            return 3;
        }
        // This ABI's own System V IPC is refused, through the library as a
        // program makes it, and an unrelated call runs.
        // SAFETY: plain integers.
        let id = unsafe { libc::semget(0, 1, libc::IPC_CREAT | 0o600) };
        if id != -1 || errno() != libc::ENOSYS {
            return 4;
        }
        // SAFETY: plain integers.
        let id = unsafe { libc::shmget(0, 4096, libc::IPC_CREAT | 0o600) };
        if id != -1 || errno() != libc::ENOSYS {
            return 5;
        }
        if call0(GETPID).0 != std::process::id() as c_long {
            return 6;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        3 => Err("the all-ABI filter was refused".into()),
        4 | 5 => Err("this ABI's System V IPC was not refused ENOSYS by the filter".into()),
        6 => Err("the all-ABI filter stopped a call it does not name".into()),
        other => Err(format!("the child failed at step {other}")),
    }
}

// ---------------------------------------------------------------------------
// Chromium's shape: a filter with an `arch` check, the x32 bit and a trap, and
// the SIGSYS handler that emulates the call
// ---------------------------------------------------------------------------

/// The data the trap returns.
const TRAP_DATA: u32 = 3;
/// What the handler makes the trapped call return.
const EMULATED: c_long = 4242;

/// Calls the handler has run.
static HANDLED: AtomicU32 = AtomicU32::new(0);
/// What the handler found wrong, as a step number; zero if nothing.
static HANDLER_FOUND: AtomicI32 = AtomicI32::new(0);
/// Whether the handler makes a trapped call of its own, which traps again
/// (`SA_NODEFER`).
static NEST: AtomicBool = AtomicBool::new(false);
/// What the nested call returned, once the handler has made it.
static NESTED_ANSWER: AtomicI32 = AtomicI32::new(0);

/// The context's instruction pointer.
fn context_pc(context: &libc::ucontext_t) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        context.uc_mcontext.gregs[libc::REG_RIP as usize] as u64
    }
    #[cfg(target_arch = "x86")]
    {
        context.uc_mcontext.gregs[libc::REG_EIP as usize] as u32 as u64
    }
    #[cfg(target_arch = "aarch64")]
    {
        context.uc_mcontext.pc
    }
    #[cfg(target_arch = "arm")]
    {
        u64::from(context.uc_mcontext.arm_pc)
    }
}

/// The register that held the call's number: `RAX`, `EAX`, `x8`, `r7`.
fn context_number(context: &libc::ucontext_t) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        context.uc_mcontext.gregs[libc::REG_RAX as usize] as u64
    }
    #[cfg(target_arch = "x86")]
    {
        context.uc_mcontext.gregs[libc::REG_EAX as usize] as u32 as u64
    }
    #[cfg(target_arch = "aarch64")]
    {
        context.uc_mcontext.regs[8]
    }
    #[cfg(target_arch = "arm")]
    {
        u64::from(context.uc_mcontext.arm_r7)
    }
}

/// The register that held the call's first argument: `RDI`, `EBX`, `x0`, `r0`.
fn context_argument(context: &libc::ucontext_t) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        context.uc_mcontext.gregs[libc::REG_RDI as usize] as u64
    }
    #[cfg(target_arch = "x86")]
    {
        context.uc_mcontext.gregs[libc::REG_EBX as usize] as u32 as u64
    }
    #[cfg(target_arch = "aarch64")]
    {
        context.uc_mcontext.regs[0]
    }
    #[cfg(target_arch = "arm")]
    {
        u64::from(context.uc_mcontext.arm_r0)
    }
}

/// Write the call's result: `RAX`, `EAX`, `x0`, `r0`.
fn context_set_result(context: &mut libc::ucontext_t, value: c_long) {
    #[cfg(target_arch = "x86_64")]
    {
        context.uc_mcontext.gregs[libc::REG_RAX as usize] = value as _;
    }
    #[cfg(target_arch = "x86")]
    {
        context.uc_mcontext.gregs[libc::REG_EAX as usize] = value as _;
    }
    #[cfg(target_arch = "aarch64")]
    {
        context.uc_mcontext.regs[0] = value as u64;
    }
    #[cfg(target_arch = "arm")]
    {
        context.uc_mcontext.arm_r0 = value as _;
    }
}

/// `SIGSYS`'s handler, as Chromium's (`sandbox/linux/seccomp-bpf/trap.cc`): it
/// refuses a signal unless every one of Chromium's five conditions holds, then
/// writes a result into the context.
extern "C" fn on_sigsys(signal: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
    let fail = |step: i32| {
        let _ = HANDLER_FOUND.compare_exchange(0, step, Ordering::AcqRel, Ordering::Acquire);
    };
    if info.is_null() || context.is_null() {
        fail(1);
        return;
    }
    // SAFETY: the kernel passes a `siginfo_t` and a `ucontext_t` that are alive
    // for the handler.
    let (info, context) = unsafe { (&*info, &mut *context.cast::<libc::ucontext_t>()) };
    let word = std::mem::size_of::<usize>();
    // `siginfo_t`: three `int`s, then the union at the first pointer-aligned
    // offset; `_sigsys` is a pointer and two `int`s.
    let union = 12_usize.div_ceil(word) * word;
    let base = std::ptr::from_ref(info).cast::<u8>();
    // SAFETY: the offsets are inside `siginfo_t`, which is 128 bytes.
    let (call_addr, syscall, arch) = unsafe {
        (
            base.add(union).cast::<usize>().read_unaligned() as u64,
            base.add(union + word).cast::<i32>().read_unaligned(),
            base.add(union + word + 4).cast::<u32>().read_unaligned(),
        )
    };
    if signal != libc::SIGSYS || info.si_signo != libc::SIGSYS {
        fail(2);
    }
    // `si_code == SYS_SECCOMP` (1).
    if info.si_code != 1 {
        fail(3);
    }
    // `1 <= si_errno <= the trap count`: the data the filter returned.
    if info.si_errno != TRAP_DATA as i32 {
        fail(4);
    }
    // `si_call_addr` is the context's instruction pointer.
    if call_addr != context_pc(context) {
        fail(5);
    }
    // `si_syscall` is the context's system call register, and `si_arch` is ours.
    if syscall as u32 as u64 != context_number(context) & 0xFFFF_FFFF {
        fail(6);
    }
    if arch != OWN_ARCH {
        fail(7);
    }
    // The first argument is as the program made the call.
    let first = context_argument(context);
    let _ = HANDLED.fetch_add(1, Ordering::AcqRel);
    if NEST.swap(false, Ordering::AcqRel) {
        // A trap inside the handler traps again, since `SA_NODEFER` leaves
        // `SIGSYS` unblocked.
        let (answer, _) = call3(GETPPID, 0, 0, 0);
        NESTED_ANSWER.store(answer as i32, Ordering::Release);
    }
    if first != 0x1111 && first != 0 {
        // The call is made with `0x1111` as its first argument when the step
        // asks, and zero otherwise.
        fail(8);
    }
    if cfg!(feature = "negative-control-trap") {
        // Leave the result out: the context's return register holds the call
        // as it was made.
        return;
    }
    context_set_result(context, EMULATED);
}

/// Install the handler for `SIGSYS` with `SA_SIGINFO | SA_NODEFER`.
fn install_handler() -> Step {
    // SAFETY: a zeroed `sigaction` with the fields set is a valid argument.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_sigsys as usize;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
    // SAFETY: `action` is alive for the call.
    let answer = unsafe { libc::sigaction(libc::SIGSYS, &action, std::ptr::null_mut()) };
    if answer != 0 {
        return Err(format!("sigaction(SIGSYS) failed, errno {}", errno()));
    }
    Ok(())
}

/// Chromium's shape: `ld arch; jeq OWN -> next; ret TRAP|10`, `ld nr; jset
/// x32 -> TRAP|9`, and a trap with data for `getppid`, an `ERRNO` for
/// `getuid`, and everything else allowed.
fn chromium_filter() -> Vec<SockFilter> {
    vec![
        LOAD_ARCH,
        jump(BPF_JEQ_K, OWN_ARCH, 1, 0),
        stmt(BPF_RET_K, RET_TRAP | 10),
        LOAD_NR,
        jump(BPF_JSET_K, X32_BIT, 0, 1),
        stmt(BPF_RET_K, RET_TRAP | 9),
        jump(BPF_JEQ_K, GETPPID as u32, 0, 1),
        stmt(BPF_RET_K, RET_TRAP | TRAP_DATA),
        jump(BPF_JEQ_K, GETUID as u32, 0, 1),
        stmt(BPF_RET_K, RET_ERRNO | libc::EPERM as u32),
        stmt(BPF_RET_K, RET_ALLOW),
    ]
}

/// **emulate**.
fn emulate() -> Step {
    let child = fork_with(|| {
        if install_handler().is_err() || no_new_privs().is_err() {
            return 2;
        }
        if install(&chromium_filter(), 0).0 != 0 {
            return 3;
        }
        // The call is trapped, the handler writes 4242 into the context, and
        // the call returns it.
        let (answer, _) = call3(GETPPID, 0x1111, 0, 0);
        if HANDLER_FOUND.load(Ordering::Acquire) != 0 {
            return 10 + HANDLER_FOUND.load(Ordering::Acquire);
        }
        if HANDLED.load(Ordering::Acquire) != 1 {
            return 4;
        }
        if answer != EMULATED {
            return 5;
        }
        // The filter's other rules still apply: an ERRNO, and a call allowed.
        if call0(GETUID) != (-1, libc::EPERM) {
            return 6;
        }
        if call0(GETPID).0 != std::process::id() as c_long {
            return 7;
        }
        // `SIGSYS` blocked: a trap cannot be blocked, so the handler runs.
        // SAFETY: a zeroed set, then one signal added, blocks that signal.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `set` is alive for the calls.
        let _ = unsafe { libc::sigemptyset(&mut set) };
        // SAFETY: as above.
        let _ = unsafe { libc::sigaddset(&mut set, libc::SIGSYS) };
        // SAFETY: as above.
        let _ = unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
        let (answer, _) = call3(GETPPID, 0, 0, 0);
        if answer != EMULATED || HANDLED.load(Ordering::Acquire) != 2 {
            return 8;
        }
        // A trap inside the handler traps again (`SA_NODEFER`): the nested call
        // returns the value its own handler run wrote, and the outer call the
        // one the outer run wrote.
        NEST.store(true, Ordering::Release);
        let (answer, _) = call3(GETPPID, 0, 0, 0);
        if HANDLED.load(Ordering::Acquire) != 4 || answer != EMULATED {
            return 9;
        }
        if NESTED_ANSWER.load(Ordering::Acquire) != EMULATED as i32 {
            return 9;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        3 => Err("Chromium's filter was refused".into()),
        4 => Err("the SIGSYS handler did not run once for the trapped call".into()),
        5 => Err("a trapped call did not return the result its handler wrote".into()),
        6 => Err("the filter's ERRNO for getuid did not hold".into()),
        8 => Err("a trapped call with SIGSYS blocked did not run its handler".into()),
        9 => Err("a trap inside the handler did not trap again".into()),
        status @ 11..=18 => Err(format!(
            "the SIGSYS handler found its context wrong: condition {} of Chromium's five \
             (2 signal, 3 si_code, 4 si_errno, 5 si_call_addr, 6 si_syscall, 7 si_arch, 8 the \
             first argument)",
            status - 10
        )),
        other => Err(format!("the child failed at step {other}")),
    }
}

/// **ignored**: with `SIGSYS` ignored the trap still ends the process.
fn ignored() -> Step {
    let child = fork_with(|| {
        // SAFETY: ignores the signal in this process.
        let _ = unsafe { libc::signal(libc::SIGSYS, libc::SIG_IGN) };
        if no_new_privs().is_err() {
            return 2;
        }
        if install(&answering(&[(GETPPID, RET_TRAP | 1)]), 0).0 != 0 {
            return 3;
        }
        let _ = call0(GETPPID);
        // Not reached when the trap ended the process.
        4
    })?;
    match reap(child) {
        BY_SIGSYS => Ok(()),
        4 => Err("a trapped call returned with SIGSYS ignored".into()),
        other => Err(format!("the child ended with {other}, not by SIGSYS")),
    }
}

/// **kill**.
fn kill() -> Step {
    for (action, name) in [
        (RET_KILL_PROCESS, "KILL_PROCESS"),
        (RET_KILL_THREAD, "KILL_THREAD"),
    ] {
        let child = fork_with(|| {
            if no_new_privs().is_err() {
                return 2;
            }
            if install(&answering(&[(GETPPID, action)]), 0).0 != 0 {
                return 3;
            }
            let _ = call0(GETPPID);
            4
        })?;
        match reap(child) {
            BY_SIGSYS => {}
            4 => return Err(format!("a {name} call returned")),
            other => {
                return Err(format!(
                    "a single-threaded process killed by {name} ended with {other}, not by SIGSYS"
                ));
            }
        }
    }
    // A thread killed by its filter leaves the others, and the process.
    let child = fork_with(|| {
        if no_new_privs().is_err() {
            return 2;
        }
        let mut thread: libc::pthread_t = 0;
        // SAFETY: the entry function is `extern "C"` and takes nothing.
        let created = unsafe {
            libc::pthread_create(
                &mut thread,
                std::ptr::null(),
                dies_by_its_filter,
                std::ptr::null_mut(),
            )
        };
        if created != 0 {
            return 3;
        }
        // SAFETY: joins the thread made above.
        let _ = unsafe { libc::pthread_join(thread, std::ptr::null_mut()) };
        // This thread has no filter: the call runs.
        if call0(GETPPID).0 < 0 {
            return 5;
        }
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        3 => Err("a thread could not be made".into()),
        5 => Err("a thread killed by its filter took the process's calls with it".into()),
        BY_SIGSYS => Err("a thread killed by its filter ended its whole process".into()),
        other => Err(format!("the process of a killed thread ended with {other}")),
    }
}

/// A thread that installs `KILL_THREAD` for `getppid` for itself alone and
/// makes the call.
extern "C" fn dies_by_its_filter(_argument: *mut c_void) -> *mut c_void {
    if install(&answering(&[(GETPPID, RET_KILL_THREAD)]), 0).0 == 0 {
        let _ = call0(GETPPID);
    }
    std::ptr::null_mut()
}

/// **strict**.
fn strict() -> Step {
    let child = fork_with(|| {
        // SAFETY: plain integers.
        let answer = unsafe { libc::prctl(PR_SET_SECCOMP, MODE_STRICT, 0, 0, 0) };
        if answer != 0 {
            return 2;
        }
        // `write` runs (to a descriptor that is not open: EBADF is the call's
        // own answer, so it ran).
        let (answer, error) = call3(WRITE, 99, 0, 0);
        if answer != -1 || error != libc::EBADF {
            return 3;
        }
        let _ = call0(GETPID);
        4
    })?;
    match reap(child) {
        BY_SIGKILL => Ok(()),
        3 => Err("strict mode did not let write run".into()),
        4 => Err("strict mode let getpid run".into()),
        other => Err(format!(
            "a strict-mode thread that made a call ended with {other}"
        )),
    }
}

/// The argument that makes the program the filtered child of **bwrap**.
const CHILD: &str = "--filtered-child";

/// What the program does when it is exec'd by **bwrap**: it finds itself
/// filtered, and its getppid refused.
fn filtered_child() -> c_int {
    if status_field("Seccomp:").as_deref() != Some("2") {
        return 2;
    }
    if status_field("NoNewPrivs:").as_deref() != Some("1") {
        return 3;
    }
    if call0(GETPPID) != (-1, libc::EPERM) {
        return 4;
    }
    0
}

/// **bwrap**: no-new-privs, `prctl(PR_SET_SECCOMP, 2, &prog)` and `execve`.
fn bwrap() -> Step {
    let path = std::env::current_exe().map_err(|e| format!("no path of this program: {e}"))?;
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "a path with a NUL".to_string())?;
    let argument = std::ffi::CString::new(CHILD).map_err(|_| "a NUL".to_string())?;
    let child = fork_with(|| {
        if no_new_privs().is_err() {
            return 2;
        }
        let program = answering(&[(GETPPID, RET_ERRNO | libc::EPERM as u32)]);
        if install_prctl(&program).0 != 0 {
            return 3;
        }
        let argv = [path.as_ptr(), argument.as_ptr(), std::ptr::null()];
        // SAFETY: a NUL-terminated path and argument vector, alive for the call.
        let _ = unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
        5
    })?;
    match reap(child) {
        0 => Ok(()),
        3 => Err("prctl(PR_SET_SECCOMP, 2, &prog) was refused".into()),
        4 => Err(
            "the exec'd program's getppid was not refused: the filter did not survive execve"
                .into(),
        ),
        2 | 5 => Err("the exec'd program did not find itself filtered".into()),
        other => Err(format!("the filtered child ended with {other}")),
    }
}

// ---------------------------------------------------------------------------
// TSYNC
// ---------------------------------------------------------------------------

/// How many threads the first group has.
const THREADS: usize = 8;
/// How many more the ninth thread makes while the sync runs.
const MORE: usize = 12;

/// Thread `n` of the first group has been refused `getppid` at least once.
static REFUSED: [AtomicBool; THREADS + MORE + 1] =
    [const { AtomicBool::new(false) }; THREADS + MORE + 1];
/// The threads stop when this is set.
static STOP: AtomicBool = AtomicBool::new(false);
/// How many more threads the ninth has made.
static MADE: AtomicU32 = AtomicU32::new(0);
/// The thread id a thread with a filter of its own reports.
static FOREIGN_TID: AtomicI32 = AtomicI32::new(0);
/// The foreign thread has its filter.
static FOREIGN_READY: AtomicBool = AtomicBool::new(false);

/// A thread of the first group, or one made by the ninth: calls `getppid` until
/// told to stop and notes when it is refused. `argument` is its number.
extern "C" fn watcher(argument: *mut c_void) -> *mut c_void {
    let me = argument as usize;
    while !STOP.load(Ordering::Acquire) {
        let (answer, error) = call0(GETPPID);
        if answer == -1 && error == libc::EPERM {
            if let Some(flag) = REFUSED.get(me) {
                flag.store(true, Ordering::Release);
            }
        }
        // SAFETY: yields the processor.
        let _ = unsafe { libc::sched_yield() };
    }
    std::ptr::null_mut()
}

/// The ninth thread: makes [`MORE`] more watchers, one after another.
extern "C" fn maker(_argument: *mut c_void) -> *mut c_void {
    for n in 0..MORE {
        let mut thread: libc::pthread_t = 0;
        // SAFETY: the entry function is `extern "C"`; the argument is a number.
        let made = unsafe {
            libc::pthread_create(
                &mut thread,
                std::ptr::null(),
                watcher,
                (THREADS + 1 + n) as *mut c_void,
            )
        };
        if made != 0 {
            break;
        }
        let _ = MADE.fetch_add(1, Ordering::AcqRel);
        // SAFETY: yields the processor.
        let _ = unsafe { libc::sched_yield() };
    }
    watcher((THREADS) as *mut c_void)
}

/// A thread that gives itself a filter of its own, which is not the process's.
extern "C" fn foreign(_argument: *mut c_void) -> *mut c_void {
    if install(&answering(&[(GETSID, RET_ERRNO | libc::EBADF as u32)]), 0).0 == 0 {
        let (tid, _) = call0(GETTID);
        FOREIGN_TID.store(tid as i32, Ordering::Release);
        FOREIGN_READY.store(true, Ordering::Release);
    } else {
        FOREIGN_READY.store(true, Ordering::Release);
    }
    while !STOP.load(Ordering::Acquire) {
        // SAFETY: yields the processor.
        let _ = unsafe { libc::sched_yield() };
    }
    std::ptr::null_mut()
}

/// **tsync**.
fn tsync() -> Step {
    let child = fork_with(|| {
        if no_new_privs().is_err() {
            return 2;
        }
        let mut threads: Vec<libc::pthread_t> = Vec::new();
        for n in 0..THREADS {
            let mut thread: libc::pthread_t = 0;
            // SAFETY: the entry function is `extern "C"`; the argument is a number.
            let made = unsafe {
                libc::pthread_create(&mut thread, std::ptr::null(), watcher, n as *mut c_void)
            };
            if made != 0 {
                return 3;
            }
            threads.push(thread);
        }
        let mut ninth: libc::pthread_t = 0;
        // SAFETY: as above.
        if unsafe {
            libc::pthread_create(&mut ninth, std::ptr::null(), maker, std::ptr::null_mut())
        } != 0
        {
            return 3;
        }
        // Let the ninth make a few before the sync, and the rest happen during
        // and after it.
        let start = std::time::Instant::now();
        while MADE.load(Ordering::Acquire) < 3 {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                return 4;
            }
            // SAFETY: yields the processor.
            let _ = unsafe { libc::sched_yield() };
        }
        let program = answering(&[(GETPPID, RET_ERRNO | libc::EPERM as u32)]);
        let (answer, error) = install(&program, FLAG_TSYNC);
        if answer != 0 {
            let _ = error;
            return 5;
        }
        // Every thread, those made during the sync too, is refused.
        let waiting = std::time::Instant::now();
        loop {
            let refused = REFUSED
                .iter()
                .filter(|flag| flag.load(Ordering::Acquire))
                .count();
            if refused == THREADS + MORE + 1 {
                break;
            }
            if waiting.elapsed() > std::time::Duration::from_secs(10) {
                return 6;
            }
            // SAFETY: yields the processor.
            let _ = unsafe { libc::sched_yield() };
        }
        STOP.store(true, Ordering::Release);
        for thread in threads {
            // SAFETY: joins a thread made above.
            let _ = unsafe { libc::pthread_join(thread, std::ptr::null_mut()) };
        }
        // SAFETY: as above.
        let _ = unsafe { libc::pthread_join(ninth, std::ptr::null_mut()) };
        0
    })?;
    match reap(child) {
        0 => {}
        3 => return Err("a thread could not be made".into()),
        4 => return Err("the thread that makes threads made none".into()),
        5 => return Err("TSYNC among running threads was refused".into()),
        6 => {
            return Err(
                "a thread, one made during TSYNC perhaps, was never refused the call".into(),
            );
        }
        other => return Err(format!("the TSYNC child failed at step {other}")),
    }

    // A thread with a filter of its own makes TSYNC fail with its id, and
    // change nothing; with TSYNC_ESRCH it is `ESRCH`.
    let child = fork_with(|| {
        if no_new_privs().is_err() {
            return 2;
        }
        let mut thread: libc::pthread_t = 0;
        // SAFETY: the entry function is `extern "C"` and takes nothing.
        if unsafe {
            libc::pthread_create(&mut thread, std::ptr::null(), foreign, std::ptr::null_mut())
        } != 0
        {
            return 3;
        }
        let start = std::time::Instant::now();
        while !FOREIGN_READY.load(Ordering::Acquire) {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                return 4;
            }
            // SAFETY: yields the processor.
            let _ = unsafe { libc::sched_yield() };
        }
        let theirs = FOREIGN_TID.load(Ordering::Acquire);
        let program = answering(&[(GETPPID, RET_ERRNO | libc::EPERM as u32)]);
        let (answer, _) = install(&program, FLAG_TSYNC);
        if theirs == 0 || answer != c_long::from(theirs) {
            return 5;
        }
        let (answer, error) = install(&program, FLAG_TSYNC | FLAG_TSYNC_ESRCH);
        if answer != -1 || error != libc::ESRCH {
            return 6;
        }
        // Changed none: this thread's getppid still runs.
        if call0(GETPPID).0 < 0 {
            return 7;
        }
        STOP.store(true, Ordering::Release);
        // SAFETY: joins the thread made above.
        let _ = unsafe { libc::pthread_join(thread, std::ptr::null_mut()) };
        0
    })?;
    match reap(child) {
        0 => Ok(()),
        3 => Err("a thread could not be made".into()),
        5 => Err("TSYNC blocked by a thread's own filter did not answer that thread's id".into()),
        6 => Err("TSYNC_ESRCH blocked by a thread's own filter did not answer ESRCH".into()),
        7 => Err("a TSYNC that failed changed the calling thread's filter".into()),
        other => Err(format!("the foreign-filter child failed at step {other}")),
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some(CHILD) {
        std::process::exit(filtered_child());
    }
    let steps: [Named; 12] = [
        ("probes", probes),
        ("errno", errno_filters),
        ("order", order),
        ("unprivileged", unprivileged),
        ("status", status),
        ("twoarch", two_architectures),
        ("emulate", emulate),
        ("ignored", ignored),
        ("kill", kill),
        ("strict", strict),
        ("bwrap", bwrap),
        ("tsync", tsync),
    ];
    for (name, step) in steps {
        if let Err(what) = step() {
            say(&format!("seccomp: FAILED {name}: {what}"));
            std::process::exit(1);
        }
        say(&format!("seccomp: {name} ok"));
    }
    say("seccomp: all ok");
}
