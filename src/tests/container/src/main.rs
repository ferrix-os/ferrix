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
//! * **pid**: `getpid` is 1 and `getppid` 0, and the owner's
//!   `/proc/<pid>/status` lists its `NSpid` as its own number and 1;
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
//! `container: all ok` and exits 0, or prints `container: FAILED <what>` and
//! exits 1. `cargo xtask test-container` judges the lines; `docs/CGROUPS.md`
//! §15 lists the negative controls that break each clause.

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
/// Where init mounts it: a mount of its own, so the page cache is new and
/// what pid 1 reads is charged to pid 1's cgroup. The boot's btrfs check
/// leaves its own mount of the disk on `/mnt`, whose pages it read itself.
const MOUNT: &str = "/tmp/ro";
/// A file of 140,000 bytes on it (`tools/common/gen/gen-btrfs-fixtures.py`).
const FILE: &str = "/tmp/ro/big.txt";

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
    let (Ok(source), Ok(target), Ok(kind)) = (
        CString::new(source),
        CString::new(target),
        CString::new(kind),
    ) else {
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
    if let Err(error) = fs::create_dir_all(MOUNT) {
        fail(&format!("{MOUNT} could not be made ({error})"));
    }
    mount(
        DISK,
        MOUNT,
        "btrfs",
        libc::MS_RDONLY,
        "the btrfs test disk could not be mounted read-only",
    );
    let _ = fs::create_dir_all(CGROUP);
    mount("none", CGROUP, "cgroup2", 0, "cgroup2 could not be mounted");
    put(
        &format!("{CGROUP}/cgroup.subtree_control"),
        "+memory",
        "the memory controller could not be enabled",
    );
    if let Err(error) = fs::create_dir(CTN) {
        fail(&format!(
            "the container's cgroup could not be made ({error})"
        ));
    }
    let limit = LIMIT.to_string();
    put(
        &format!("{CTN}/memory.max"),
        &limit,
        "memory.max could not be set",
    );
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
        put(
            &format!("{CTN}/cgroup.procs"),
            "0",
            "the owner could not be put in its cgroup",
        );
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
    say(&format!(
        "uid {uid} euid {euid} CapEff {effective} made a user namespace"
    ));
    step("unprivileged");
    put(
        "/proc/self/uid_map",
        &format!("0 {OWNER} 1\n"),
        "uid_map could not be written",
    );
    put(
        "/proc/self/setgroups",
        "deny\n",
        "setgroups could not be denied",
    );
    put(
        "/proc/self/gid_map",
        &format!("0 {OWNER} 1\n"),
        "gid_map could not be written",
    );
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
    let mut gate: [c_int; 2] = [0; 2];
    // SAFETY: `gate` is a valid place for the kernel to write two descriptors.
    if unsafe { libc::pipe(gate.as_mut_ptr()) } != 0 {
        fail("a pipe could not be made");
    }
    let [wait_end, go_end] = gate;
    let init = fork();
    if init == 0 {
        // Pid 1 waits until its parent has looked at it from outside.
        let mut byte = [0_u8; 1];
        // SAFETY: `byte` is valid for one byte; the descriptors are this process's.
        let got = unsafe {
            let _ = libc::close(go_end);
            libc::read(wait_end, byte.as_mut_ptr().cast(), 1)
        };
        if got != 1 {
            fail("pid 1 was never told to go on");
        }
        pid_one(outside, host);
    }
    // What the owner holds itself, after the fork so none of it is pid 1's
    // too. Empty: the kill must choose pid 1, the cgroup's largest; the scope
    // control makes it the largest itself, and the judge must fail.
    let ballast: Vec<u8> = Vec::new();
    // From the owner's namespace pid 1 has two numbers, its own last: the
    // owner's `status` view is told in the reader's namespace (`docs/PIDNS.md`
    // §6), so this is read here and not by pid 1.
    let status = get(
        &format!("/proc/{init}/status"),
        "pid 1's status could not be read",
    );
    let numbers: Vec<&str> = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .map(|line| line.split_whitespace().collect())
        .unwrap_or_default();
    let own = init.to_string();
    if numbers.len() != 2 || numbers.last() != Some(&"1") || numbers.first() != Some(&own.as_str())
    {
        fail(&format!(
            "pid {init}'s status did not show it as 1 in a namespace below (NSpid {numbers:?})"
        ));
    }
    // SAFETY: `go_end` is valid for one byte from a valid buffer.
    let told = unsafe {
        let _ = libc::close(wait_end);
        libc::write(go_end, [1_u8].as_ptr().cast(), 1)
    };
    if told != 1 {
        fail("pid 1 could not be told to go on");
    }
    let status = wait_for(init);
    drop(ballast);
    let stat = get(
        &format!("{CTN}/memory.stat"),
        "memory.stat could not be read",
    );
    let events = get(
        &format!("{CTN}/memory.events"),
        "memory.events could not be read",
    );
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
    if cfg!(feature = "lift") {
        say("heap-reclaim not judged: linuxd's kernel-heap charges do not reclaim (BACKLOG)");
    } else {
        heap_charge_reclaims();
        step("heap-reclaim");
    }
    // SAFETY: `_exit` takes a status and never returns.
    unsafe { libc::_exit(0) }
}

/// A child whose parent ends is told so by `getppid` in a loop of nothing else, whose
/// answer on a user kernel is a kept word (ledger 6747 (4d)): its parent's number here, then
/// this namespace's first process, and never a parent gone.
fn reparented_in_the_namespace() {
    // SAFETY: the child runs below and leaves by `_exit`.
    let parent = unsafe { libc::fork() };
    if parent < 0 {
        fail("fork for a reparent");
    }
    if parent == 0 {
        // SAFETY: as above.
        let grandchild = unsafe { libc::fork() };
        if grandchild < 0 {
            fail("fork for a reparent's child");
        }
        if grandchild == 0 {
            // SAFETY: takes nothing.
            let first = unsafe { libc::getppid() };
            let mut left = 2_000_000_000_u64;
            loop {
                // SAFETY: takes nothing.
                let now = unsafe { libc::getppid() };
                if now == 1 {
                    // SAFETY: as above.
                    unsafe { libc::_exit(0) };
                }
                if now != first || left == 0 {
                    fail(&format!(
                        "getppid after a parent's end read {now} (it was {first}), not the \
                         namespace's first process"
                    ));
                }
                left -= 1;
            }
        }
        // SAFETY: as above.
        unsafe { libc::_exit(0) };
    }
    // The first to end is the parent, and the other is this process's by then: both reaped.
    for _ in 0..2 {
        let mut status = 0;
        // SAFETY: an out-parameter on this stack, any child of this process.
        if unsafe { libc::wait(&raw mut status) } < 0 {
            fail("wait for a reparent");
        }
        if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            fail("a reparented child did not read this namespace's first process");
        }
    }
    step("pid reparent");
}

/// Why a call at the limit failed, for [`heap_charge_reclaims`]: the one sentence a refusal the
/// file cache could have spared prints (fl-096), and the error otherwise.
fn refused(what: &str, error: &std::io::Error) -> ! {
    if error.raw_os_error() == Some(libc::ENOMEM) {
        fail(&format!(
            "{what} was refused at memory.max beside reclaimable file cache ({error})"
        ));
    }
    fail(&format!("{what} failed at memory.max ({error})"))
}

/// A kernel-heap charge at `memory.max` reclaims before it refuses, as Linux's does (fl-096,
/// ledger 7326). After the kill, in the same cgroup: `FILE` mapped and read, so its clean pages
/// are charged here and mapped in this space; anonymous memory touched until fewer than `NEAR`
/// pages are left, so that no fault comes near the limit, whose refusal would reclaim the file's
/// pages for itself; a pipe written until its buffer's growth is refused, which brings what is
/// left under one growth step; then `HOLD` opens of `memory.events`, held, whose kernel heap does
/// not fit in that. Each must succeed, `pgscan` must move, and `FILE`'s mapping must read as it
/// did, its pages taken down in this same task and filled again from the disk.
///
/// A pipe's buffer, a map entry and a descriptor table's slots are charged under a spin lock,
/// where no reclaim can run, so those refuse at the limit still (BACKLOG): the table is grown
/// first, while there is room, and the pipe's refusal is what brings the job to the limit.
fn heap_charge_reclaims() {
    const PAGE: usize = 4096;
    const FILE_BYTES: usize = 140_000;
    /// Pages walked a look at a time, the rest in one run.
    const CLOSE: usize = 64;
    /// Pages left when the walk stops: over a fault's seven pages of `write_ahead`.
    const NEAR: usize = 24;
    /// Opens held at the limit: over twice what one growth step leaves, at a few hundred bytes
    /// of heap each.
    const HOLD: usize = 128;
    let file = fs::File::open(FILE)
        .unwrap_or_else(|error| fail(&format!("{FILE} could not be opened ({error})")));
    // SAFETY: a fresh private read-only mapping of the file, read below and unmapped at the end.
    let held = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            FILE_BYTES,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            std::os::fd::AsRawFd::as_raw_fd(&file),
            0,
        )
    };
    if held == libc::MAP_FAILED {
        fail("the file could not be mapped");
    }
    let held = held.cast::<u8>();
    let sum = || {
        (0..FILE_BYTES).fold(0_u64, |sum, at| {
            // SAFETY: inside the mapping above.
            sum.wrapping_mul(31)
                .wrapping_add(u64::from(unsafe { held.add(at).read_volatile() }))
        })
    };
    let before_sum = sum();
    let stat = get(
        &format!("{CTN}/memory.stat"),
        "memory.stat could not be read",
    );
    let (cached, scanned) = (field(&stat, "file"), field(&stat, "pgscan"));
    if cached < 80 * 1024 {
        fail(&format!(
            "only {cached} bytes of file cache were charged to the cgroup after the file was mapped"
        ));
    }
    // Pipes enough for the room the walk leaves, `NEAR` pages at most, at 64 KiB each.
    let mut ends: [c_int; 16] = [0; 16];
    for pair in ends.chunks_exact_mut(2) {
        // SAFETY: `pair` is a valid place for the kernel to write two descriptors.
        if unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_NONBLOCK) } != 0 {
            fail("a pipe could not be made");
        }
    }
    // The descriptor table's room for the opens below, had now: its slots are kept when the
    // descriptor closes again.
    // SAFETY: `dup2` and `close` of descriptors this process holds.
    if unsafe { libc::dup2(ends[0], 255) != 255 || libc::close(255) != 0 } {
        fail("the descriptor table could not be grown");
    }
    // Opened afresh each time: a cgroup file's text is made at its open, as Linux's seq files'
    // is at the first read, so a read again from its start gives the same figure.
    let used = || {
        fs::read_to_string(format!("{CTN}/memory.current"))
            .unwrap_or_else(|error| refused("an open and read of memory.current", &error))
            .trim()
            .parse::<usize>()
            .unwrap_or(LIMIT)
    };
    let room = LIMIT.saturating_sub(used());
    let pages = room / PAGE + CLOSE;
    // SAFETY: a fresh anonymous mapping, touched below and unmapped before the return.
    let walk = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pages * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if walk == libc::MAP_FAILED {
        fail("the walk's memory could not be mapped");
    }
    let walk = walk.cast::<u8>();
    // A page in every 2 MiB first, while there is room: no page table is needed near the limit,
    // where a fault's refusal would reclaim the file's pages for itself.
    for page in (0..pages).step_by(512) {
        // SAFETY: inside the mapping above.
        unsafe { walk.add(page * PAGE).write_volatile(1) };
    }
    let run = (LIMIT.saturating_sub(used()) / PAGE)
        .saturating_sub(CLOSE)
        .min(pages);
    for page in 0..run {
        // SAFETY: inside the mapping above.
        unsafe { walk.add(page * PAGE).write_volatile(1) };
    }
    let mut next = run;
    while LIMIT.saturating_sub(used()) >= NEAR * PAGE {
        if next == pages {
            fail("the walk never came near memory.max");
        }
        // SAFETY: inside the mapping above.
        unsafe { walk.add(next * PAGE).write_volatile(1) };
        next += 1;
    }
    let page = [7_u8; PAGE];
    let mut filled = 0_usize;
    let mut writing = ends.iter().skip(1).step_by(2);
    let mut end = writing.next().copied().unwrap_or(-1);
    loop {
        // SAFETY: `page` is valid for its length; the descriptor is a pipe's write end.
        let wrote = unsafe { libc::write(end, page.as_ptr().cast(), page.len()) };
        if let Ok(wrote) = usize::try_from(wrote)
            && wrote > 0
        {
            filled += wrote;
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOMEM) => break,
            // Full: the next pipe.
            Some(libc::EAGAIN) if let Some(next) = writing.next() => end = *next,
            _ => fail(&format!(
                "the pipes stopped at {filled} bytes, not at memory.max, {} bytes under it ({error})",
                LIMIT.saturating_sub(used())
            )),
        }
    }
    let left = LIMIT.saturating_sub(used());
    let mut opened = Vec::with_capacity(HOLD);
    for _ in 0..HOLD {
        let file = fs::File::open(format!("{CTN}/memory.events"))
            .unwrap_or_else(|error| refused("an open of memory.events", &error));
        opened.push(file);
    }
    let events = fs::read_to_string(format!("{CTN}/memory.events"))
        .unwrap_or_else(|error| refused("an open and read of memory.events", &error));
    drop(opened);
    let stat = get(
        &format!("{CTN}/memory.stat"),
        "memory.stat could not be read",
    );
    let moved = field(&stat, "pgscan").saturating_sub(scanned);
    say(&format!(
        "a pipe refused at {filled} bytes, {left} under memory.max, beside {cached} bytes of file \
         cache: {HOLD} opens held and a read given, pgscan moved {moved}, pgsteal {}, \
         memory.events max {}",
        field(&stat, "pgsteal"),
        field(&events, "max")
    ));
    if moved == 0 {
        fail("charges at memory.max were given with no page scanned for them");
    }
    if sum() != before_sum {
        fail("the file's mapping read otherwise once its pages were reclaimed and filled again");
    }
    // SAFETY: the mappings above and the pipe's ends, which nothing else names.
    unsafe {
        let _ = libc::munmap(walk.cast(), pages * PAGE);
        let _ = libc::munmap(held.cast(), FILE_BYTES);
        for end in ends {
            let _ = libc::close(end);
        }
    }
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
    step("pid");
    reparented_in_the_namespace();
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
    let set = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    // A build with no setters refuses every namespace's too (`runtime_settings=fixed`,
    // docs/BUILD-FLAGS.md), and the row holds the refusal.
    if cfg!(feature = "fixed-settings") {
        if set == 0 || hostname() != host {
            fail("a build with no setters let root in its UTS namespace set its host name");
        }
    } else if set != 0 || hostname() == host {
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
    let stat = get(
        &format!("{CTN}/memory.stat"),
        "memory.stat could not be read",
    );
    say(&format!(
        "file {} charged to the cgroup after the read",
        field(&stat, "file")
    ));
    walk_to_the_limit();
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

/// A program walking up through fresh memory to just under `memory.max`, and staying under it,
/// is charged for what it touched: nothing a fault brought along pushes it over, so
/// `memory.events` does not move (`fault_ahead`, ledger 6537). The room left is under
/// `write_ahead`'s seven pages, once the walk's own tables are counted.
///
/// The room is measured last: everything this program allocates for itself (the reads of
/// `memory.events` among them, whose heap growth is the allocator's and differs between builds)
/// happens before `memory.current` is read, and nothing allocates between that read and the
/// walk's end, so the room left is the walk's alone.
fn walk_to_the_limit() {
    const PAGE: usize = 4096;
    /// What is left beyond the walk: about four pages once its tables are charged, under
    /// `write_ahead`'s seven.
    const LEFT: usize = 4;
    let events = || {
        let text = get(
            &format!("{CTN}/memory.events"),
            "memory.events could not be read",
        );
        field(&text, "max")
    };
    // Before `memory.current` is read, not after: this read can grow the heap by a page (seen in
    // the builds of both rustc 1.97.1 and 1.99.1), and after the read that page came out of the
    // room `LEFT` names; under 1.99.1's build the walk then went over.
    let before = events();
    let used = get(
        &format!("{CTN}/memory.current"),
        "memory.current could not be read",
    );
    // Not a walk of no pages, which would pass: a figure that does not read fails the case.
    let Ok(used) = used.trim().parse::<usize>() else {
        fail("memory.current did not read as a number");
    };
    let pages = (LIMIT.saturating_sub(used) / PAGE).saturating_sub(LEFT);
    // Longer than the walk, so that a fault near its end has somewhere ahead to look.
    let mapped = pages + 16;
    // SAFETY: a fresh anonymous mapping, touched below and unmapped before the return.
    let walk = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mapped * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if walk == libc::MAP_FAILED {
        fail("the walk's memory could not be mapped");
    }
    let walk = walk.cast::<u8>();
    for page in 0..pages {
        // SAFETY: inside the mapping above.
        unsafe { walk.add(page * PAGE).write_volatile(1) };
    }
    // SAFETY: the mapping above, which nothing else names.
    if unsafe { libc::munmap(walk.cast(), mapped * PAGE) } != 0 {
        fail("the walk's memory could not be unmapped");
    }
    // Read after the unmap: `memory.events` only counts up, so it still holds anything the walk was
    // refused, and opening a file at the limit is a charge of its own, which is not the walk's.
    let moved = events().saturating_sub(before);
    let now = get(
        &format!("{CTN}/memory.current"),
        "memory.current could not be read",
    );
    say(&format!(
        "a walk of {pages} pages from {used} bytes used, {} once unmapped: memory.events max moved {moved}",
        now.trim()
    ));
    if moved != 0 {
        fail("a walk that fits under memory.max was refused a charge");
    }
}

/// The filter that answers `getsid` with `EPERM`, and a second process under
/// one that kills for it.
fn seccomp() {
    let getsid = libc::SYS_getsid as c_uint;
    let installed = true;
    if installed {
        install(&filter(
            getsid,
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ));
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
