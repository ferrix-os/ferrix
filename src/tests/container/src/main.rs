//! Stage 13's exit criterion, run as init by `cargo xtask test-container`:
//! *an unprivileged user namespace runs a process whose pid is 1 inside it,
//! under a memory limit that triggers scoped reclaim and a scoped OOM kill,
//! with a seccomp filter that blocks a syscall.*
//!
//! Init (root) mounts the btrfs test disk read-only, makes a cgroup with
//! `memory.max` of 8 MiB, hands it to uid 1000 and forks the container's
//! owner into it. The owner becomes uid 1000, with no capability left, sets
//! `no_new_privs`, makes a user namespace and maps itself to root in it, then
//! makes six more kinds, and forks: the grandchild is pid 1 of its pid
//! namespace. Each clause is a line of its own:
//!
//! * **unprivileged**: the user namespace is made by a process whose real and
//!   effective uid are 1000 and whose effective capability set is empty;
//! * **pid**: `getpid` is 1, `getppid` is 0, `status` has two `NSpid`s;
//! * **namespaces**: each of the seven `/proc/self/ns/*` links differs from
//!   the one init has;
//! * **ids**: `getuid` is 0 inside, and `sethostname` works in its own UTS
//!   namespace and does not reach init's;
//! * **seccomp**: a filter that answers `getsid` with `EPERM` does, and a
//!   second process under a filter that kills for `getsid` dies of `SIGSYS`;
//! * **reclaim**: pid 1 reads a file of the read-only btrfs mount, so its
//!   clean pages are charged to the cgroup, and then touches four times
//!   `memory.max`; the cgroup's `memory.stat` counts pages reclaimed
//!   (`pgsteal`) before anything is killed;
//! * **oom**: pid 1 is ended with `SIGKILL`, `memory.events` counts one
//!   `oom_kill`, and its parent, in the same cgroup, and init, outside it,
//!   are alive to say so.
//!
//! Every step prints `container: <step> ok`; the program ends with
//! `container: all ok` and status 0, or `container: FAILED <what>` and status
//! 1. `cargo xtask test-container` judges the lines; `docs/CGROUPS.md` §15
//! lists the negative controls that break each clause.

use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use libc::{c_int, c_uint, c_ulong, pid_t};

/// The cgroup2 mount.
const CGROUP: &str = "/sys/fs/cgroup";
/// The container's cgroup.
const CTN: &str = "/sys/fs/cgroup/ctn";
/// Its memory limit, bytes.
const LIMIT: usize = 8 * 1024 * 1024;
/// The uid the container's owner has outside.
const OWNER: u32 = 1000;
/// Every namespace kind Ferrix gives, as `/proc/<pid>/ns/` names it.
const KINDS: [&str; 7] = ["mnt", "user", "pid", "net", "ipc", "uts", "cgroup"];
/// The btrfs test disk every QEMU machine carries as `vdb`, read-only
/// (`tools/common/xtask/src/btrfs_disk.rs`).
const DISK: &str = "/dev/vdb";
/// Where init mounts it.
const MOUNT: &str = "/mnt/ro";
/// A file of 140,000 bytes on it (`tools/common/gen/gen-btrfs-fixtures.py`).
const FILE: &str = "/mnt/ro/big.txt";

/// Print a progress line.
fn step(what: &str) {
    println!("container: {what} ok");
    let _ = std::io::stdout().flush();
}

/// Print a line that is not judged.
fn say(what: &str) {
    println!("container: {what}");
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
    if let Err(error) = fs::write(path, text) {
        fail(&format!("{what} ({error})"));
    }
}

/// The text of `path`, or fail naming `what`.
fn get(path: &str, what: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| fail(&format!("{what} ({error})")))
}

/// The number after `key` in a `key value` file's text, or 0.
fn field(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(key)).then(|| words.next()?.parse().ok())?
        })
        .unwrap_or(0)
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
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail("fork failed");
    }
    pid
}

/// Wait for `pid` and answer the raw status.
fn wait_for(pid: pid_t) -> c_int {
    let mut status = 0;
    // SAFETY: `status` is a valid place for the kernel to write an `int`.
    if unsafe { libc::waitpid(pid, &raw mut status, 0) } != pid {
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

/// `mount(2)` with no data, or fail naming `what`.
fn mount(source: &str, target: &str, kind: &str, flags: c_ulong, what: &str) {
    let (Ok(source), Ok(target), Ok(kind)) =
        (CString::new(source), CString::new(target), CString::new(kind))
    else {
        fail(what);
    };
    // SAFETY: the strings are NUL-terminated and outlive the call.
    let done = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if done != 0 {
        fail(&format!("{what} ({})", std::io::Error::last_os_error()));
    }
}

fn main() {
    let outside = namespaces();
    let host = hostname();
    // The disk's driver says hello on its own time; wait for its node.
    let since = Instant::now();
    while fs::metadata(DISK).is_err() {
        if since.elapsed() > Duration::from_secs(30) {
            fail("the btrfs test disk never appeared");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = fs::create_dir_all(MOUNT);
    mount(DISK, MOUNT, "btrfs", libc::MS_RDONLY, "the btrfs test disk could not be mounted read-only");
    let _ = fs::create_dir_all(CGROUP);
    mount("none", CGROUP, "cgroup2", 0, "cgroup2 could not be mounted");
    put(
        &format!("{CGROUP}/cgroup.subtree_control"),
        "+memory",
        "the memory controller could not be enabled",
    );
    if let Err(error) = fs::create_dir(CTN) {
        fail(&format!("the container's cgroup could not be made ({error})"));
    }
    let limit = LIMIT.to_string();
    put(&format!("{CTN}/memory.max"), &limit, "memory.max could not be set");
    for file in ["", "/cgroup.procs", "/cgroup.subtree_control"] {
        let path = CString::new(format!("{CTN}{file}")).unwrap_or_default();
        // SAFETY: `path` is NUL-terminated and outlives the call.
        if unsafe { libc::chown(path.as_ptr(), OWNER, OWNER) } != 0 {
            fail("the cgroup could not be delegated to its owner");
        }
    }
    step("delegation");

    let owner = fork();
    if owner == 0 {
        // Into the cgroup while still root: a move from the root cgroup needs
        // write access to the root's `cgroup.procs`, which only root has, on
        // Linux as here (`docs/CGROUPS.md` §3.1).
        put(&format!("{CTN}/cgroup.procs"), "0", "the owner could not be put in its cgroup");
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
    step("isolation");
    say("all ok");
    // SAFETY: `_exit` takes a status and never returns.
    unsafe { libc::_exit(0) }
}

/// The host name, as `uname` says it.
fn hostname() -> String {
    // SAFETY: an all-zero `utsname` is valid, and `uname` fills it.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a valid place for the kernel to write a `utsname`.
    if unsafe { libc::uname(&raw mut name) } != 0 {
        fail("uname failed");
    }
    name.nodename
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8 as char)
        .collect()
}

/// The container's owner: uid 1000, which makes the namespaces and is the
/// parent of pid 1, in the same cgroup.
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
    // Unprivileged: no root id and no capability, before the user namespace.
    // SAFETY: the id calls take nothing.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    let status = get("/proc/self/status", "status could not be read");
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    if uid == 0 || euid == 0 || effective.is_empty() || effective.bytes().any(|b| b != b'0') {
        fail(&format!(
            "the user namespace's maker was not an unprivileged user (uid {uid}, euid {euid}, \
             CapEff {effective})"
        ));
    }
    // SAFETY: `unshare` with a flag and nothing else.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        fail(&format!(
            "an unprivileged user could not make a user namespace ({})",
            std::io::Error::last_os_error()
        ));
    }
    say(&format!("uid {uid} euid {euid} CapEff {effective} made a user namespace"));
    step("unprivileged");
    put("/proc/self/uid_map", &format!("0 {OWNER} 1\n"), "uid_map could not be written");
    put("/proc/self/setgroups", "deny\n", "setgroups could not be denied");
    put("/proc/self/gid_map", &format!("0 {OWNER} 1\n"), "gid_map could not be written");
    let flags = libc::CLONE_NEWNS
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWCGROUP
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWPID;
    // SAFETY: `unshare` with flags and nothing else.
    if unsafe { libc::unshare(flags) } != 0 {
        fail(&format!(
            "the other six namespaces could not be made ({})",
            std::io::Error::last_os_error()
        ));
    }
    // What the owner holds itself; the scoped kill must pass it over.
    let ballast: Vec<u8> = Vec::new();
    let init = fork();
    if init == 0 {
        pid_one(outside, host);
    }
    let status = wait_for(init);
    drop(ballast);
    let stat = get(&format!("{CTN}/memory.stat"), "memory.stat could not be read");
    let events = get(&format!("{CTN}/memory.events"), "memory.events could not be read");
    let (scanned, stolen) = (field(&stat, "pgscan"), field(&stat, "pgsteal"));
    say(&format!(
        "memory.stat pgscan {scanned} pgsteal {stolen} file {}; memory.events max {} oom {} \
         oom_kill {}",
        field(&stat, "file"),
        field(&events, "max"),
        field(&events, "oom"),
        field(&events, "oom_kill"),
    ));
    if stolen == 0 {
        fail("memory.stat counted no page reclaimed in the cgroup before the kill");
    }
    step("reclaim");
    // The OOM kill ended pid 1 and not its parent, which is in the same cgroup
    // and is saying this.
    if !killed_by(status, libc::SIGKILL) {
        fail("pid 1 was not ended by SIGKILL at the memory limit");
    }
    if field(&events, "oom_kill") != 1 {
        fail("memory.events did not count one OOM kill");
    }
    step("oom");
    // SAFETY: `_exit` takes a status and never returns.
    unsafe { libc::_exit(0) }
}

/// The first process of the pid namespace.
fn pid_one(outside: &[String], host: &str) -> ! {
    // SAFETY: `getpid` and `getppid` take nothing.
    let (pid, parent) = unsafe { (libc::getpid(), libc::getppid()) };
    if pid != 1 || parent != 0 {
        fail(&format!(
            "the first process of a pid namespace was not pid 1 with no parent (pid {pid}, \
             ppid {parent})"
        ));
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
            fail(&format!("the {kind} namespace is not a new one"));
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
    // The file's clean pages, read into the cache by a process of the
    // cgroup, are charged to it: what reclaim may give back.
    let mut bytes = Vec::new();
    match fs::File::open(FILE).and_then(|mut file| file.read_to_end(&mut bytes)) {
        Ok(140_000) => {}
        Ok(other) => fail(&format!("{FILE} read {other} bytes, not 140000")),
        Err(error) => fail(&format!("{FILE} could not be read ({error})")),
    }
    drop(bytes);
    let stat = get(&format!("{CTN}/memory.stat"), "memory.stat could not be read");
    say(&format!("file {} charged to the cgroup after the read", field(&stat, "file")));
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
    let installed = true;
    if installed {
        install(&filter(getsid, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32));
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
        // ld [0]: seccomp_data.nr
        insn(0x20, 0, 0, 0),
        // jeq #number, 0, 1
        insn(0x15, 0, 1, number),
        // ret #action
        insn(0x06, 0, 0, action),
        // ret #ALLOW
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
            libc::SECCOMP_SET_MODE_FILTER as c_ulong,
            0 as c_ulong,
            std::ptr::from_ref(&fprog),
        )
    };
    if done != 0 {
        fail("a seccomp filter could not be installed");
    }
}

/// The calling thread's `errno`.
fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
