//! Stage 13's exit criterion, run as init by `cargo xtask test-container`:
//! *an unprivileged user namespace runs a process whose pid is 1 inside it,
//! under a memory limit that triggers a scoped OOM kill, with a seccomp filter
//! that blocks a syscall.*
//!
//! Init (root) delegates a cgroup to uid 1000 with `memory.max` of 8 MiB. A
//! child becomes uid 1000, sets `no_new_privs`, joins the cgroup, makes a user
//! namespace and maps itself to root in it, then makes the other seven kinds,
//! and forks: the grandchild is pid 1 of its pid namespace. In it:
//!
//! * **namespaces**: each of the eight `/proc/self/ns/*` links differs from
//!   the one init has;
//! * **pid**: `getpid` is 1, `getppid` is 0, `status` has two `NSpid`s;
//! * **ids**: `getuid` is 0 inside, the kernel says 1000 outside, and
//!   `sethostname` works in its own UTS namespace and does not reach init's;
//! * **seccomp**: a filter that answers `getsid` with `EPERM` does, a second
//!   process under a filter that kills for `getsid` dies of `SIGSYS`;
//! * **memory**: touching four times `memory.max` ends it with `SIGKILL`, the
//!   cgroup's `memory.events` counts `oom_kill`, and its parent, in the same
//!   cgroup, and init, outside it, are alive to say so.
//!
//! Every step prints `container: <step> ok`; the program ends with
//! `container: all ok` and status 0, or `container: FAILED <what>` and status
//! 1. Built with `negative-control` the filter is never installed, and the
//! seccomp step must fail.

use std::ffi::CString;
use std::fs;
use std::io::Write;

use libc::{c_int, c_long, c_uint, c_ulong, pid_t};

/// The cgroup the container lives in, under the cgroup2 mount.
const CGROUP: &str = "/sys/fs/cgroup";
/// Its memory limit, bytes.
const LIMIT: usize = 8 * 1024 * 1024;
/// The uid the container's owner has outside.
const OWNER: u32 = 1000;
/// Every namespace kind, as `/proc/<pid>/ns/` names it.
const KINDS: [&str; 8] = ["mnt", "user", "pid", "net", "ipc", "uts", "cgroup", "time"];
/// `CLONE_NEWTIME`, which `libc` may not name.
const CLONE_NEWTIME: c_int = 0x80;

/// Print a progress line.
fn step(what: &str) {
    println!("container: {what} ok");
    let _ = std::io::stdout().flush();
}

/// Say what failed and end this process with status 1.
fn fail(what: &str) -> ! {
    println!("container: FAILED {what}");
    let _ = std::io::stdout().flush();
    // SAFETY: `_exit` takes a status and never returns; nothing is left to clean.
    unsafe { libc::_exit(1) }
}

/// Write `text` to `path`, or fail naming `what`.
fn put(path: &str, text: &str, what: &str) {
    if fs::write(path, text).is_err() {
        fail(what);
    }
}

/// The text of `path`, or fail naming `what`.
fn get(path: &str, what: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|_| fail(what))
}

/// What each `/proc/self/ns/*` link reads.
fn namespaces() -> Vec<String> {
    KINDS
        .iter()
        .map(|kind| {
            fs::read_link(format!("/proc/self/ns/{kind}"))
                .map(|target| target.display().to_string())
                .unwrap_or_default()
        })
        .collect()
}

/// `fork`, answering the child's pid in the parent and `0` in the child.
fn fork() -> pid_t {
    // SAFETY: a single-threaded program; the child continues with its own copy.
    unsafe { libc::fork() }
}

/// Wait for `pid` and answer the raw status.
fn wait_for(pid: pid_t) -> c_int {
    let mut status = 0;
    // SAFETY: `status` is a valid place for the kernel to write an `int`.
    if unsafe { libc::waitpid(pid, &mut status, 0) } != pid {
        fail("waitpid did not return the child");
    }
    status
}

/// Whether `status` says the process ended by `signal`.
fn killed_by(status: c_int, signal: c_int) -> bool {
    libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == signal
}

/// Whether `status` says the process exited 0.
fn exited_ok(status: c_int) -> bool {
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

fn main() {
    let outside = namespaces();
    let host = hostname();
    // init is root; it delegates a cgroup with a memory limit to the owner.
    let _ = fs::create_dir_all(CGROUP);
    // SAFETY: the strings are NUL-terminated and outlive the call.
    let mounted = unsafe {
        let (source, target, kind) = (c"none", c"/sys/fs/cgroup", c"cgroup2");
        libc::mount(source.as_ptr(), target.as_ptr(), kind.as_ptr(), 0, std::ptr::null())
    };
    if mounted != 0 {
        fail("cgroup2 could not be mounted");
    }
    put(
        &format!("{CGROUP}/cgroup.subtree_control"),
        "+memory",
        "the memory controller could not be enabled",
    );
    let ctn = format!("{CGROUP}/ctn");
    if fs::create_dir(&ctn).is_err() {
        fail("the container's cgroup could not be made");
    }
    put(
        &format!("{ctn}/memory.max"),
        &LIMIT.to_string(),
        "memory.max could not be set",
    );
    for file in ["", "/cgroup.procs"] {
        let path = CString::new(format!("{ctn}{file}")).unwrap_or_default();
        // SAFETY: `path` is NUL-terminated and outlives the call.
        if unsafe { libc::chown(path.as_ptr(), OWNER, OWNER) } != 0 {
            fail("the cgroup could not be delegated to its owner");
        }
    }
    step("delegation");

    let owner = fork();
    if owner == 0 {
        container(&outside, &host);
    }
    let status = wait_for(owner);
    if !exited_ok(status) {
        fail("the container's owner did not exit 0");
    }
    // Outside, nothing the container did reached init.
    if hostname() != host {
        fail("the container's sethostname reached the host");
    }
    let events = get(&format!("{ctn}/memory.events"), "memory.events could not be read");
    if !events.lines().any(|line| line.trim() == "oom_kill 1") {
        fail("memory.events did not count the OOM kill");
    }
    step("isolation");
    println!("container: all ok");
    let _ = std::io::stdout().flush();
    // SAFETY: `_exit` takes a status and never returns.
    unsafe { libc::_exit(0) }
}

/// The host name, as `uname` says it.
fn hostname() -> String {
    // SAFETY: an all-zero `utsname` is valid, and `uname` fills it.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a valid place for the kernel to write a `utsname`.
    if unsafe { libc::uname(&mut name) } != 0 {
        fail("uname failed");
    }
    name.nodename
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8 as char)
        .collect()
}

/// The container's owner: uid 1000, which makes the namespaces and is the
/// parent of pid 1.
fn container(outside: &[String], host: &str) -> ! {
    // SAFETY: plain id changes; the group first, while it may still.
    let dropped = unsafe { libc::setgid(OWNER) == 0 && libc::setuid(OWNER) == 0 };
    if !dropped {
        fail("uid 1000 could not be taken");
    }
    // SAFETY: `prctl` with constants and no pointers.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        fail("no_new_privs could not be set");
    }
    put(
        &format!("{CGROUP}/ctn/cgroup.procs"),
        "0",
        "the owner could not join its cgroup",
    );
    // The user namespace first: it is what makes the rest unprivileged.
    // SAFETY: `unshare` with a flag and nothing else.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        fail("an unprivileged user could not make a user namespace");
    }
    put("/proc/self/uid_map", &format!("0 {OWNER} 1\n"), "uid_map could not be written");
    put("/proc/self/setgroups", "deny\n", "setgroups could not be denied");
    put("/proc/self/gid_map", &format!("0 {OWNER} 1\n"), "gid_map could not be written");
    let flags = libc::CLONE_NEWNS
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWCGROUP
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWPID
        | CLONE_NEWTIME;
    // SAFETY: `unshare` with flags and nothing else.
    if unsafe { libc::unshare(flags) } != 0 {
        fail("the other seven namespaces could not be made");
    }
    let init = fork();
    if init == 0 {
        pid_one(outside, host);
    }
    let status = wait_for(init);
    // The OOM kill ended pid 1 and not its parent, which is in the same cgroup.
    if !killed_by(status, libc::SIGKILL) {
        fail("pid 1 was not ended by SIGKILL at the memory limit");
    }
    step("memory");
    // SAFETY: `_exit` takes a status and never returns.
    unsafe { libc::_exit(0) }
}

/// The first process of the pid namespace.
fn pid_one(outside: &[String], host: &str) -> ! {
    // SAFETY: `getpid` and `getppid` take nothing.
    let (pid, parent) = unsafe { (libc::getpid(), libc::getppid()) };
    if pid != 1 || parent != 0 {
        fail("the first process of a pid namespace was not pid 1 with no parent");
    }
    let status = get("/proc/self/status", "status could not be read");
    let pids = status
        .lines()
        .find(|line| line.starts_with("NSpid:"))
        .map_or(0, |line| line.split_whitespace().count().saturating_sub(1));
    if pids != 2 {
        fail("status did not show the pid in two namespaces");
    }
    step("pid");
    let inside = namespaces();
    for ((kind, before), after) in KINDS.iter().zip(outside).zip(&inside) {
        if before == after || after.is_empty() {
            println!("container: FAILED the {kind} namespace is not a new one");
            // SAFETY: `_exit` takes a status and never returns.
            unsafe { libc::_exit(1) }
        }
    }
    step("namespaces");
    // SAFETY: `getuid` takes nothing.
    if unsafe { libc::getuid() } != 0 {
        fail("uid 1000 did not read as root inside its namespace");
    }
    let name = b"container";
    // SAFETY: `name` is valid for its length.
    if unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } != 0 || hostname() == host {
        fail("root in its own UTS namespace could not set its host name");
    }
    step("ids");
    seccomp();
    // The memory limit last: four times it, touched, cannot all fit.
    let mut held: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut block = vec![0_u8; 1024 * 1024];
        for byte in block.iter_mut().step_by(4096) {
            *byte = 1;
        }
        held.push(block);
        if held.len() > LIMIT / (1024 * 1024) * 4 {
            fail("four times memory.max was touched and nothing ended it");
        }
    }
}

/// The filter that answers `getsid` with `EPERM`, and a second process under
/// one that kills for it.
fn seccomp() {
    let getsid = libc::SYS_getsid as c_uint;
    let answering = filter(getsid, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32);
    if cfg!(not(feature = "negative-control")) {
        install(&answering);
    }
    // SAFETY: `syscall` with the call's number and its one argument.
    let got = unsafe { libc::syscall(libc::SYS_getsid, 0) };
    if got != -1 || errno() != libc::EPERM {
        fail("a call the filter blocks was not refused EPERM");
    }
    let doomed = fork();
    if doomed == 0 {
        install(&filter(getsid, libc::SECCOMP_RET_KILL_PROCESS));
        // SAFETY: `syscall` with the call's number and its one argument.
        let _ = unsafe { libc::syscall(libc::SYS_getsid, 0) };
        fail("a call the filter kills for was made");
    }
    if !killed_by(wait_for(doomed), libc::SIGSYS) {
        fail("SECCOMP_RET_KILL_PROCESS did not end the process with SIGSYS");
    }
    step("seccomp");
}

/// "If the call is `number`, return `action`; else allow."
fn filter(number: c_uint, action: u32) -> [libc::sock_filter; 4] {
    let insn = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
    [
        insn(0x20, 0, 0, 0),
        insn(0x15, 0, 1, number),
        insn(0x06, 0, 0, action),
        insn(0x06, 0, 0, libc::SECCOMP_RET_ALLOW),
    ]
}

/// Install `program` with `seccomp(2)`.
fn install(program: &[libc::sock_filter; 4]) {
    let fprog = libc::sock_fprog {
        len: 4,
        filter: program.as_ptr().cast_mut(),
    };
    // SAFETY: `fprog` and the filter it points at outlive the call.
    let done = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            1 as c_ulong,
            0 as c_ulong,
            std::ptr::from_ref(&fprog),
        )
    };
    let _: c_long = done;
    if done != 0 {
        fail("a seccomp filter could not be installed");
    }
}

/// The calling thread's `errno`.
fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
