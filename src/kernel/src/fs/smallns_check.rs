//! The small namespaces, proved at boot (`docs/NAMESPACES.md` §12): UTS, IPC
//! and cgroup namespaces, namespace files and `setns`, driven through the
//! system-call layer as a program's calls would be.
//!
//! * **UTS.** A namespace made by the `clone` path starts with its creator's
//!   host name and keeps its own after; `unshare` does the same; `uname` and
//!   `/proc/sys/kernel/hostname` tell the caller's. Uid 1000 is refused one
//!   and refused `sethostname`; a process that made a user namespace with it
//!   may name it and, whatever its ids say, never the first's.
//! * **IPC.** A new namespace shares no semaphore keys with the first,
//!   counts only its own sets in `SEM_INFO`, and its sets end with it.
//! * **cgroup.** `/proc/<pid>/cgroup` is told from the reader's namespace
//!   root, with `..` for what lies outside it; a cgroupfs mounted there has
//!   the root as its own; a writer there moves processes only inside it.
//! * **Namespace files.** `open` of a `/proc/<pid>/ns` link is a descriptor:
//!   two opens are one inode, the four requests answer, and `setns` joins by
//!   Linux's rules, each refusal tried.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_FDCWD, AT_REMOVEDIR, CLONE_SYSVSEM, CLONE_THREAD, O_RDONLY, O_WRONLY,
};
use ferrix_vfs::Errno;

use crate::fs::cgroupfs;
use crate::fs::mount_check::{Page, Report as Counts, Tally, by_number, close, open, page_for};
use crate::fs::namespace_check::{read_file, read_link, unshare};
use crate::fs::nsfs::{NS_GET_NSTYPE, NS_GET_OWNER_UID, NS_GET_PARENT, NS_GET_USERNS};
use crate::object::process::Host;
use crate::syscall::family;
use crate::syscall::namespace::{CLONE_NEWNS, CLONE_NEWUSER};
use crate::syscall::nsproxy::{CLONE_NEWCGROUP, CLONE_NEWIPC, CLONE_NEWUTS};
use crate::syscall::process::{self, Process};
use crate::syscall::userns;
use crate::syscall::{fd, sem, system};

/// `CLONE_NEWPID`, which `setns` cannot join (pid namespaces are made, not entered:
/// `docs/PIDNS.md` §8).
const CLONE_NEWPID: u64 = 0x2000_0000;

/// A semaphore key of the check's own.
const KEY: u64 = 0x5150;
/// `IPC_CREAT`, `IPC_EXCL`, and the `semctl` commands used.
const IPC_CREAT: u64 = 0o1000;
/// See [`IPC_CREAT`].
const IPC_EXCL: u64 = 0o2000;
/// `IPC_RMID`.
const IPC_RMID: u64 = 0;
/// `SEM_INFO`.
const SEM_INFO: u64 = 19;

/// Where the cgroup section mounts a cgroupfs of the whole tree, and of the
/// namespace's.
const WHOLE: &[u8] = b"/tmp/.smallns-whole";
/// See [`WHOLE`].
const INSIDE: &[u8] = b"/tmp/.smallns-inside";

/// What the check saw, for the boot line.
pub(crate) fn run() -> Result<Counts, &'static str> {
    let mut counts = Counts::default();
    let mut tally = Tally {
        report: &mut counts,
    };
    uts(&mut tally)?;
    ipc(&mut tally)?;
    cgroup(&mut tally)?;
    joining(&mut tally)?;
    files(&mut tally)?;
    flags(&mut tally)?;
    Ok(counts)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A new process of the check's: kernel root, in the first namespaces.
fn maker() -> Result<Arc<Process>, &'static str> {
    process::new_for_check().map_err(|_| "could not make a small-namespace check's process")
}

/// A new process that is uid and gid 1000.
fn person() -> Result<Arc<Process>, &'static str> {
    let made = maker()?;
    for call in [Syscall::Setgid, Syscall::Setuid] {
        by_number(&made, call, [1000, 0, 0, 0, 0, 0])
            .map(drop)
            .map_err(|_| "a small-namespace check's process could not become 1000")?;
    }
    // A change of ids made it not dumpable, which keeps even its own user from
    // opening its namespace links (`ptrace_may_access`); a program that drops
    // its ids and means to be inspected says so, as bubblewrap does.
    by_number(&made, Syscall::Prctl, [4, 1, 0, 0, 0, 0])
        .map(drop)
        .map_err(|_| "a small-namespace check's process could not become dumpable")?;
    Ok(made)
}

/// `body` with `process` as the caller: what a procfs read or a cgroup write
/// judges by when no task is running.
fn as_caller<R>(process: &Arc<Process>, body: impl FnOnce() -> R) -> Result<R, &'static str> {
    userns::acting_as(process, body)
}

/// `call` by number.
fn call(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    by_number(process, call, args)
}

/// `sethostname` or `setdomainname` of `name`.
fn set_name(
    page: &mut Page<'_>,
    which: Syscall,
    name: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let at = page.put(name)?;
    Ok(call(
        page.process,
        which,
        [at, name.len() as u64, 0, 0, 0, 0],
    ))
}

/// The host name and the domain name `uname` tells the page's process.
fn names(page: &mut Page<'_>) -> Result<(Vec<u8>, Vec<u8>), &'static str> {
    page.reset();
    call(page.process, Syscall::Uname, [page.buffer(), 0, 0, 0, 0, 0])
        .map(drop)
        .map_err(|_| "uname was refused in a small-namespace check")?;
    let all = page.read_back(65 * 6)?;
    let field = |at: usize| {
        all.get(at * 65..at * 65 + 65)
            .unwrap_or_default()
            .iter()
            .copied()
            .take_while(|&byte| byte != 0)
            .collect::<Vec<u8>>()
    };
    Ok((field(1), field(5)))
}

/// What `/proc/<pid>/ns/<kind>` reads, for the page's process to read.
fn link(page: &mut Page<'_>, pid: u32, kind: &str) -> Result<Vec<u8>, &'static str> {
    read_link(page, format!("/proc/{pid}/ns/{kind}").as_bytes())
}

/// A file of `/proc` or a cgroupfs read in full by `process`.
fn read_as(
    process: &Arc<Process>,
    page: &mut Page<'_>,
    path: &[u8],
) -> Result<Vec<u8>, &'static str> {
    as_caller(process, || read_file(page, path))??
        .map_err(|_| "a file could not be read in a small-namespace check")
}

/// `path` opened for reading by `process`: a descriptor, or why not.
fn open_as(
    process: &Arc<Process>,
    page: &mut Page<'_>,
    path: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    as_caller(process, || open(page, path, O_RDONLY, 0))?
}

/// `data` written to `path` by `process`, as `echo >` writes it.
fn write_as(
    process: &Arc<Process>,
    page: &mut Page<'_>,
    path: &[u8],
    data: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let opened = as_caller(process, || open(page, path, O_WRONLY, 0))??;
    let fd = match opened {
        Ok(fd) => fd,
        Err(errno) => return Ok(Err(errno)),
    };
    let at = page.put_bytes(data)?;
    let written = as_caller(process, || {
        call(
            page.process,
            Syscall::Write,
            [fd as u64, at, data.len() as u64, 0, 0, 0],
        )
    })?;
    close(page.process, fd);
    Ok(written)
}

/// `setns(fd, nstype)`.
fn setns(process: &Process, fd: usize, nstype: u64) -> Result<usize, Errno> {
    call(process, Syscall::Setns, [fd as u64, nstype, 0, 0, 0, 0])
}

/// The inode number `fstat` would give descriptor `fd` of `process`.
fn inode_of(process: &Process, fd: usize) -> Result<u64, &'static str> {
    let file = fd::file(
        process,
        i32::try_from(fd).map_err(|_| "descriptor too large")?,
    )
    .map_err(|_| "a namespace descriptor was not open")?;
    Ok(file.inode().metadata().ino)
}

/// The text `readlink("/proc/self/fd/<fd>")` gives.
fn fd_link(page: &mut Page<'_>, fd: usize) -> Result<Vec<u8>, &'static str> {
    let pid = page.process.pid();
    read_link(page, format!("/proc/{pid}/fd/{fd}").as_bytes())
}

/// A descriptor for `/proc/<pid>/ns/<kind>`, opened by the page's process.
fn ns_fd(
    process: &Arc<Process>,
    page: &mut Page<'_>,
    pid: u32,
    kind: &str,
) -> Result<Result<usize, Errno>, &'static str> {
    open_as(process, page, format!("/proc/{pid}/ns/{kind}").as_bytes())
}

/// Require a descriptor, with the message if it was refused.
fn got(
    tally: &mut Tally<'_>,
    opened: Result<Result<usize, Errno>, &'static str>,
    what: &'static str,
) -> Result<usize, &'static str> {
    tally.done(opened?, what)
}

// ---------------------------------------------------------------------------
// UTS
// ---------------------------------------------------------------------------

/// UTS namespaces: copy on making, privacy after, the owner's right to name.
fn uts(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let root = maker()?;
    let mut page = page_for(&root)?;
    let outcome = uts_names(&root, &mut page, tally).and_then(|()| uts_owner(tally));
    // The first namespace's name, put back whatever happened.
    system::forget_hostname();
    outcome
}

/// A child made by the `clone` path starts from its creator's names, changes
/// only its own, and reads them through `uname` and the sysctl.
fn uts_names(
    root: &Arc<Process>,
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    tally.ok(
        set_name(page, Syscall::Sethostname, b"outer")?,
        "root could not set the first UTS namespace's host name",
    )?;
    let child = maker()?;
    family::give_namespaces(root, &child, CLONE_NEWUTS)
        .map_err(|_| "a child could not be given a UTS namespace")?;
    let mut inner = page_for(&child)?;
    if names(&mut inner)?.0 != b"outer" {
        return Err("a new UTS namespace did not start with its creator's host name");
    }
    tally.ok(
        set_name(&mut inner, Syscall::Sethostname, b"inner")?,
        "root could not set a UTS namespace's host name",
    )?;
    tally.ok(
        set_name(&mut inner, Syscall::Setdomainname, b"child.example")?,
        "root could not set a UTS namespace's domain name",
    )?;
    let (child_node, child_domain) = names(&mut inner)?;
    let (root_node, root_domain) = names(page)?;
    if child_node != b"inner" || child_domain != b"child.example" {
        return Err("uname did not tell a UTS namespace's own names");
    }
    if root_node != b"outer" || root_domain != b"(none)" {
        return Err("a name set in a child UTS namespace reached its creator's");
    }
    let (theirs, ours) = (
        link(page, root.pid(), "uts")?,
        link(page, child.pid(), "uts")?,
    );
    if theirs == ours || !ours.starts_with(b"uts:[") {
        return Err("a new UTS namespace was not named apart by /proc/<pid>/ns/uts");
    }
    let sysctl = b"/proc/sys/kernel/hostname";
    let seen = read_as(&child, &mut inner, sysctl)?;
    let first = read_as(root, page, sysctl)?;
    if seen != b"inner\n" || first != b"outer\n" {
        return Err("/proc/sys/kernel/hostname did not tell the reader's own UTS namespace");
    }
    // `unshare` makes one as well.
    let apart = maker()?;
    tally.ok(
        unshare(&apart, CLONE_NEWUTS),
        "unshare(CLONE_NEWUTS) was refused to root",
    )?;
    let mut alone = page_for(&apart)?;
    if names(&mut alone)?.0 != b"outer" || link(&mut alone, apart.pid(), "uts")? == theirs {
        return Err("unshare(CLONE_NEWUTS) did not give a copy in a namespace of its own");
    }
    Ok(())
}

/// Who may name: uid 1000 not at all, a maker of a user namespace its own
/// UTS namespace and never the first's -- kernel root in it included.
fn uts_owner(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let low = person()?;
    let mut page = page_for(&low)?;
    tally.refused(
        unshare(&low, CLONE_NEWUTS),
        Errno::EPERM,
        "unshare(CLONE_NEWUTS) by uid 1000 was not refused EPERM",
    )?;
    tally.refused(
        set_name(&mut page, Syscall::Sethostname, b"low")?,
        Errno::EPERM,
        "uid 1000 set the first UTS namespace's host name",
    )?;
    let bystander = maker()?;
    for (who, what) in [
        (person()?, "fake root"),
        (maker()?, "kernel root in a user namespace"),
    ] {
        let mut own = page_for(&who)?;
        // Its own link, opened while it still is in the first namespaces.
        let first = got(
            tally,
            ns_fd(&who, &mut own, who.pid(), "uts"),
            "a process could not open its own UTS namespace",
        )?;
        tally.ok(
            unshare(&who, CLONE_NEWUSER | CLONE_NEWUTS),
            "unshare(CLONE_NEWUSER | CLONE_NEWUTS) was refused",
        )?;
        let refused = match what {
            "fake root" => "fake root could not set the UTS namespace it made",
            _ => "kernel root in a user namespace it made could not name the UTS namespace it made",
        };
        tally.ok(set_name(&mut own, Syscall::Sethostname, b"mine")?, refused)?;
        if names(&mut own)?.0 != b"mine" {
            return Err("a name set in a UTS namespace owned by a user namespace was lost");
        }
        // Back into the first namespace's names: no capability over its owner.
        let joined = setns(&who, first, CLONE_NEWUTS);
        if joined.is_ok() {
            return Err("a process in a child user namespace joined the first UTS namespace");
        }
        tally.refused(
            joined,
            Errno::EPERM,
            "joining the first UTS namespace from a child user namespace was not EPERM",
        )?;
    }
    let mut outside = page_for(&bystander)?;
    if names(&mut outside)?.0 == b"mine" {
        return Err("a child user namespace's maker named the first UTS namespace");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// IPC
// ---------------------------------------------------------------------------

/// `semget(key, 1, flags)`.
fn semget(process: &Process, key: u64, flags: u64) -> Result<usize, Errno> {
    call(process, Syscall::Semget, [key, 1, flags, 0, 0, 0])
}

/// The sets `SEM_INFO` counts in the page's process's namespace: `semusz`.
fn sets_seen(page: &mut Page<'_>) -> Result<u32, &'static str> {
    page.reset();
    call(
        page.process,
        Syscall::Semctl,
        [0, 0, SEM_INFO, page.buffer(), 0, 0],
    )
    .map(drop)
    .map_err(|_| "SEM_INFO was refused")?;
    let info = page.read_back(40)?;
    let word: [u8; 4] = info
        .get(28..32)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("a seminfo was too short")?;
    Ok(u32::from_le_bytes(word))
}

/// Semaphore keys and counts are a namespace's own, and its sets end with it.
fn ipc(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let root = maker()?;
    let mut page = page_for(&root)?;
    let first = tally.done(
        semget(&root, KEY, IPC_CREAT | 0o600),
        "semget was refused to root",
    )?;
    let second = tally.done(
        semget(&root, KEY + 1, IPC_CREAT | 0o600),
        "semget was refused to root",
    )?;
    let outcome = ipc_apart(&root, &mut page, tally);
    for id in [first, second] {
        let _ = call(&root, Syscall::Semctl, [id as u64, 0, IPC_RMID, 0, 0, 0]);
    }
    outcome?;
    ipc_ends(tally)?;
    let low = person()?;
    tally.refused(
        unshare(&low, CLONE_NEWIPC),
        Errno::EPERM,
        "unshare(CLONE_NEWIPC) by uid 1000 was not refused EPERM",
    )
}

/// A child in a new namespace sees none of the first's keys and counts its own.
fn ipc_apart(
    root: &Arc<Process>,
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let child = maker()?;
    family::give_namespaces(root, &child, CLONE_NEWIPC)
        .map_err(|_| "a child could not be given an IPC namespace")?;
    let mut inner = page_for(&child)?;
    tally.refused(
        semget(&child, KEY, 0),
        Errno::ENOENT,
        "a new IPC namespace saw the first's semaphore keys",
    )?;
    let made = tally.done(
        semget(&child, KEY, IPC_CREAT | IPC_EXCL | 0o600),
        "the key of another namespace's set was taken in a new IPC namespace",
    )?;
    if sets_seen(&mut inner)? != 1 {
        return Err("SEM_INFO in a new IPC namespace counted the first's sets");
    }
    if sets_seen(page)? < 2 {
        return Err("SEM_INFO in the first IPC namespace lost its sets");
    }
    let (theirs, ours) = (
        link(page, root.pid(), "ipc")?,
        link(page, child.pid(), "ipc")?,
    );
    if theirs == ours || !ours.starts_with(b"ipc:[") {
        return Err("a new IPC namespace was not named apart by /proc/<pid>/ns/ipc");
    }
    let _ = call(&child, Syscall::Semctl, [made as u64, 0, IPC_RMID, 0, 0, 0]);
    Ok(())
}

/// A namespace with sets in it ends when its last holder leaves, and the sets
/// go with it.
fn ipc_ends(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let holder = maker()?;
    let mut page = page_for(&holder)?;
    let before = sem::sets_in_use();
    let home = got(
        tally,
        ns_fd(&holder, &mut page, holder.pid(), "ipc"),
        "a process could not open its own IPC namespace",
    )?;
    tally.ok(
        unshare(&holder, CLONE_NEWIPC),
        "unshare(CLONE_NEWIPC) was refused to root",
    )?;
    let ended = Arc::downgrade(&holder.nsproxy().ipc);
    tally.ok(
        semget(&holder, 0, IPC_CREAT | 0o600).map(|_| 0),
        "a set could not be made in a new IPC namespace",
    )?;
    tally.ok(
        setns(&holder, home, 0),
        "setns back to the first IPC namespace failed",
    )?;
    if ended.upgrade().is_some() {
        return Err("an IPC namespace outlived its last holder");
    }
    if sem::sets_in_use() != before {
        return Err("a set made in an IPC namespace reached the first's table");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// cgroup
// ---------------------------------------------------------------------------

/// The view a cgroup namespace gives: its root, `..` for what is outside it,
/// a cgroupfs rooted in it, a rule for moving, and the creator's cgroup as the
/// root of a clone's.
fn cgroup(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let actor = maker()?;
    let mut page = page_for(&actor)?;
    tally.ok(
        unshare(&actor, CLONE_NEWNS),
        "unshare(CLONE_NEWNS) was refused to root",
    )?;
    let outcome = cgroup_in(&actor, &mut page, tally);
    // Whatever happened: the mounts and directories go.
    for mount in [INSIDE, WHOLE] {
        let at = page.put(mount);
        if let Ok(at) = at {
            let _ = call(&actor, Syscall::Umount2, [at, 0, 0, 0, 0, 0]);
            let _ = call(
                &actor,
                Syscall::Unlinkat,
                [AT_FDCWD as u64, at, u64::from(AT_REMOVEDIR), 0, 0, 0],
            );
        }
        page.reset();
    }
    outcome
}

/// Mount a cgroupfs of the page's process on `path`.
fn mount_cgroup(
    actor: &Arc<Process>,
    page: &mut Page<'_>,
    path: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let (source, target, kind) = (page.put(b"none")?, page.put(path)?, page.put(b"cgroup2")?);
    as_caller(actor, || {
        call(
            page.process,
            Syscall::Mount,
            [source, target, kind, 0, 0, 0],
        )
    })
}

/// `mkdir(path, 0755)` by `actor`: a directory, or in a cgroupfs a cgroup.
fn mkdir_path(
    actor: &Arc<Process>,
    page: &mut Page<'_>,
    path: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let at = page.put(path)?;
    as_caller(actor, || {
        call(
            page.process,
            Syscall::Mkdirat,
            [AT_FDCWD as u64, at, 0o755, 0, 0, 0],
        )
    })
}

/// `rmdir(path)` by `actor`.
fn rmdir_path(
    actor: &Arc<Process>,
    page: &mut Page<'_>,
    path: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let at = page.put(path)?;
    as_caller(actor, || {
        call(
            page.process,
            Syscall::Unlinkat,
            [AT_FDCWD as u64, at, u64::from(AT_REMOVEDIR), 0, 0, 0],
        )
    })
}

/// Move `who` into the cgroup whose `cgroup.procs` is at `procs`, written by
/// `actor`.
fn move_into(
    actor: &Arc<Process>,
    page: &mut Page<'_>,
    procs: &[u8],
    who: &Process,
) -> Result<Result<usize, Errno>, &'static str> {
    write_as(actor, page, procs, format!("{}\n", who.pid()).as_bytes())
}

/// The cgroup section's work, after the mount namespace is the actor's own.
fn cgroup_in(
    actor: &Arc<Process>,
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let whole = |tail: &str| [WHOLE, tail.as_bytes()].concat();
    tally.ok(
        mkdir_path(actor, page, WHOLE)?,
        "a directory for the cgroupfs could not be made",
    )?;
    tally.ok(
        mount_cgroup(actor, page, WHOLE)?,
        "cgroup2 could not be mounted by root",
    )?;
    for name in ["/a", "/a/b", "/c"] {
        tally.ok(
            mkdir_path(actor, page, &whole(name))?,
            "a cgroup could not be made",
        )?;
    }
    let (inside, outside, stranger) = (maker()?, maker()?, maker()?);
    let home = got(
        tally,
        ns_fd(actor, page, actor.pid(), "cgroup"),
        "a process could not open its own cgroup namespace",
    )?;
    for (who, path) in [
        (actor, "/a/cgroup.procs"),
        (&inside, "/a/b/cgroup.procs"),
        (&outside, "/c/cgroup.procs"),
    ] {
        tally.ok(
            move_into(actor, page, &whole(path), who)?,
            "a process could not be moved into a cgroup",
        )?;
    }
    // Seen from outside every namespace: a path from the tree's root.
    let cgroup_of = |pid: u32| format!("/proc/{pid}/cgroup").into_bytes();
    if read_as(
        &stranger,
        &mut page_for(&stranger)?,
        &cgroup_of(actor.pid()),
    )? != b"0::/a\n"
    {
        return Err("/proc/<pid>/cgroup did not read a path from the tree's root");
    }
    tally.ok(
        unshare(actor, CLONE_NEWCGROUP),
        "unshare(CLONE_NEWCGROUP) was refused to root",
    )?;
    let read = |page: &mut Page<'_>, pid: u32| read_as(actor, page, &cgroup_of(pid));
    if read(page, actor.pid())? != b"0::/\n" {
        return Err("/proc/<pid>/cgroup did not read / at the namespace's root");
    }
    if read(page, inside.pid())? != b"0::/b\n" {
        return Err("/proc/<pid>/cgroup did not read a path relative to the namespace's root");
    }
    if read(page, outside.pid())? != b"0::/../c\n" {
        return Err("a cgroup outside the namespace's root was not shown with /..");
    }
    let (seen, here) = (
        link(page, actor.pid(), "cgroup")?,
        link(page, stranger.pid(), "cgroup")?,
    );
    if seen == here || !seen.starts_with(b"cgroup:[") {
        return Err("a new cgroup namespace was not named apart by /proc/<pid>/ns/cgroup");
    }
    cgroup_mounted(actor, page, tally)?;
    cgroup_moves(actor, page, tally, &whole)?;
    cgroup_clone(actor, &stranger)?;
    cgroup_opened_inside(actor, page, tally, &whole, &inside, home)?;
    // Back where it began, so the cgroups can be taken down.
    tally.ok(
        setns(actor, home, 0),
        "setns back to the first cgroup namespace failed",
    )?;
    for (who, _) in [(actor, 0), (&inside, 0), (&outside, 0)] {
        tally.ok(
            move_into(actor, page, &whole("/cgroup.procs"), who)?,
            "a process could not be moved out of a cgroup",
        )?;
    }
    for name in ["/a/b", "/a", "/c"] {
        let _ = rmdir_path(actor, page, &whole(name))?;
    }
    Ok(())
}

/// Whether `path` opens for `actor`.
fn opens(actor: &Arc<Process>, page: &mut Page<'_>, path: &[u8]) -> Result<bool, &'static str> {
    match open_as(actor, page, path)? {
        Ok(fd) => {
            close(page.process, fd);
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// A cgroupfs mounted in the namespace shows the namespace's root as its own.
fn cgroup_mounted(
    actor: &Arc<Process>,
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    tally.ok(
        mkdir_path(actor, page, INSIDE)?,
        "a directory for the namespace's cgroupfs could not be made",
    )?;
    tally.ok(
        mount_cgroup(actor, page, INSIDE)?,
        "cgroup2 could not be mounted in a cgroup namespace",
    )?;
    let procs = read_as(actor, page, &[INSIDE, b"/cgroup.procs"].concat())?;
    let own = format!("{}\n", actor.pid());
    if !procs
        .windows(own.len())
        .any(|window| window == own.as_bytes())
    {
        return Err("a cgroupfs mounted in a cgroup namespace did not have its root as /");
    }
    if !opens(actor, page, &[INSIDE, b"/b/cgroup.procs"].concat())? {
        return Err("the namespace's cgroupfs did not show the namespace's subtree");
    }
    if opens(actor, page, &[INSIDE, b"/c/cgroup.procs"].concat())? {
        return Err("the namespace's cgroupfs showed a cgroup outside the namespace");
    }
    Ok(())
}

/// A writer in the namespace moves processes only between cgroups inside it.
fn cgroup_moves(
    actor: &Arc<Process>,
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    whole: &dyn Fn(&str) -> Vec<u8>,
) -> Result<(), &'static str> {
    tally.refused(
        move_into(actor, page, &whole("/c/cgroup.procs"), actor)?,
        Errno::ENOENT,
        "a process in a cgroup namespace moved itself outside its root",
    )?;
    tally.ok(
        move_into(actor, page, &whole("/a/b/cgroup.procs"), actor)?,
        "a process in a cgroup namespace could not move inside its root",
    )?;
    tally.ok(
        move_into(actor, page, &whole("/a/cgroup.procs"), actor)?,
        "a process in a cgroup namespace could not move back to its root",
    )
}

/// The three ways a process gets into a cgroup are judged in the namespace of
/// whoever holds the descriptor they go through (`docs/NAMESPACES.md` §12):
/// a `cgroup.procs` opened inside a namespace and written after leaving it
/// (Linux's CVE-2021-4197 class: the opener's namespace), a directory
/// descriptor kept from outside a namespace and given to `CLONE_INTO_CGROUP`,
/// and the same descriptor to native `job_for_cgroup`.
///
/// The actor is in a cgroup namespace rooted at `/a`; `/c` is outside it.
fn cgroup_opened_inside(
    actor: &Arc<Process>,
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    whole: &dyn Fn(&str) -> Vec<u8>,
    inside: &Arc<Process>,
    home: usize,
) -> Result<(), &'static str> {
    let procs = got(
        tally,
        as_caller(actor, || open(page, &whole("/c/cgroup.procs"), O_WRONLY, 0))?,
        "a cgroup.procs outside the namespace could not be opened by root",
    )?;
    let outside_dir = got(
        tally,
        as_caller(actor, || open(page, &whole("/c"), O_RDONLY, 0))?,
        "a cgroup directory outside the namespace could not be opened by root",
    )?;
    let inside_dir = got(
        tally,
        as_caller(actor, || open(page, &whole("/a/b"), O_RDONLY, 0))?,
        "a cgroup directory inside the namespace could not be opened by root",
    )?;
    // CLONE_INTO_CGROUP: a descriptor of a cgroup outside the root is refused.
    let file = fd::file(
        actor,
        i32::try_from(outside_dir).map_err(|_| "descriptor too large")?,
    )
    .map_err(|_| "a descriptor just opened was not there")?;
    tally.refused(
        cgroupfs::clone_target(&file, actor, &actor.job()).map(|_| 0),
        Errno::ENOENT,
        "CLONE_INTO_CGROUP started a child outside its creator's cgroup namespace",
    )?;
    drop(file);
    let file = fd::file(
        actor,
        i32::try_from(inside_dir).map_err(|_| "descriptor too large")?,
    )
    .map_err(|_| "a descriptor just opened was not there")?;
    tally.ok(
        cgroupfs::clone_target(&file, actor, &actor.job()).map(|_| 0),
        "CLONE_INTO_CGROUP was refused inside the creator's cgroup namespace",
    )?;
    drop(file);
    // Native job_for_cgroup: no write rights over a cgroup outside the root.
    // Rights::MANAGE, as a register.
    let manage: u64 = 1 << 6;
    tally.refused(
        as_caller(actor, || {
            cgroupfs::job_for_cgroup(
                &**actor as &dyn Host,
                &[outside_dir as u64, manage, 0, 0, 0, 0],
            )
        })?,
        Errno::EACCES,
        "a MANAGE handle was given for a cgroup outside the caller's cgroup namespace",
    )?;
    tally.ok(
        as_caller(actor, || {
            cgroupfs::job_for_cgroup(
                &**actor as &dyn Host,
                &[inside_dir as u64, manage, 0, 0, 0, 0],
            )
        })?,
        "a MANAGE handle was refused for a cgroup inside the caller's cgroup namespace",
    )?;
    // The opener's namespace: the actor leaves it, and writes through the
    // descriptor it opened inside. It is still a move out of the root.
    tally.ok(
        setns(actor, home, 0),
        "setns into the first cgroup namespace failed",
    )?;
    page.reset();
    let line = format!("{}\n", inside.pid());
    let at = page.put_bytes(line.as_bytes())?;
    let len = line.len() as u64;
    let written = as_caller(actor, || {
        call(
            page.process,
            Syscall::Write,
            [procs as u64, at, len, 0, 0, 0],
        )
    })?;
    tally.refused(
        written,
        Errno::ENOENT,
        "a descriptor opened inside a cgroup namespace moved a process out of it after its writer left",
    )?;
    close(page.process, procs);
    close(page.process, outside_dir);
    close(page.process, inside_dir);
    Ok(())
}

/// The creator's cgroup is the clone's namespace root: the clone, in the
/// tree's root, is one level outside it.
fn cgroup_clone(actor: &Arc<Process>, clone: &Arc<Process>) -> Result<(), &'static str> {
    family::give_namespaces(actor, clone, CLONE_NEWCGROUP)
        .map_err(|_| "a child could not be given a cgroup namespace")?;
    let mut page = page_for(clone)?;
    let text = read_as(
        clone,
        &mut page,
        &format!("/proc/{}/cgroup", clone.pid()).into_bytes(),
    )?;
    if text != b"0::/..\n" {
        return Err("a cloned cgroup namespace was not rooted at its creator's cgroup");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// setns
// ---------------------------------------------------------------------------

/// The refusals and the joins of `setns`, each kind's rules tried.
fn joining(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let root = maker()?;
    let mut page = page_for(&root)?;
    let here = got(
        tally,
        ns_fd(&root, &mut page, root.pid(), "uts"),
        "a process could not open its own UTS namespace",
    )?;
    tally.refused(
        setns(&root, 999, 0),
        Errno::EBADF,
        "setns of a closed descriptor was not EBADF",
    )?;
    let status = got(
        tally,
        open_as(
            &root,
            &mut page,
            format!("/proc/{}/status", root.pid()).as_bytes(),
        ),
        "a status file could not be opened",
    )?;
    tally.refused(
        setns(&root, status, 0),
        Errno::EINVAL,
        "setns of a file that is not a namespace was not EINVAL",
    )?;
    tally.refused(
        setns(&root, here, CLONE_NEWIPC),
        Errno::EINVAL,
        "setns with a type that is not the file's was not EINVAL",
    )?;
    tally.ok(
        setns(&root, here, CLONE_NEWUTS),
        "setns of a UTS namespace with its own type was refused",
    )?;
    tally.ok(setns(&root, here, 0), "setns with type 0 was refused")?;
    joining_uts(tally)?;
    joining_user(tally)?;
    joining_mount(tally)?;
    joining_pidfd(tally)
}

/// A process joins the UTS namespace another process made and then tells its
/// names.
fn joining_uts(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let (maker_of, joiner) = (maker()?, maker()?);
    let (mut theirs, mut ours) = (page_for(&maker_of)?, page_for(&joiner)?);
    tally.ok(
        unshare(&maker_of, CLONE_NEWUTS),
        "unshare(CLONE_NEWUTS) was refused to root",
    )?;
    tally.ok(
        set_name(&mut theirs, Syscall::Sethostname, b"joined")?,
        "root could not set a UTS namespace's host name",
    )?;
    let fd = got(
        tally,
        ns_fd(&joiner, &mut ours, maker_of.pid(), "uts"),
        "a namespace's UTS link could not be opened",
    )?;
    tally.ok(
        setns(&joiner, fd, CLONE_NEWUTS),
        "setns into a UTS namespace was refused to root",
    )?;
    if names(&mut ours)?.0 != b"joined" {
        return Err("setns into a UTS namespace did not change the names the caller tells");
    }
    Ok(())
}

/// User namespaces: one's own is `EINVAL`, an ancestor is `EPERM`, the maker
/// may join and then holds every capability there, anyone else may not.
fn joining_user(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let (maker_of, member, stranger) = (person()?, person()?, maker()?);
    let (mut made, mut mine, mut other) = (
        page_for(&maker_of)?,
        page_for(&member)?,
        page_for(&stranger)?,
    );
    let first = got(
        tally,
        ns_fd(&maker_of, &mut made, maker_of.pid(), "user"),
        "a process could not open its own user namespace",
    )?;
    tally.ok(
        unshare(&maker_of, CLONE_NEWUSER),
        "unshare(CLONE_NEWUSER) was refused to uid 1000",
    )?;
    tally.refused(
        setns(&maker_of, first, CLONE_NEWUSER),
        Errno::EPERM,
        "a process joined an ancestor user namespace",
    )?;
    let own = got(
        tally,
        ns_fd(&maker_of, &mut made, maker_of.pid(), "user"),
        "a process could not open its own user namespace",
    )?;
    tally.refused(
        setns(&maker_of, own, 0),
        Errno::EINVAL,
        "setns into the user namespace the caller is in was not EINVAL",
    )?;
    // The same person, from outside: the namespace's owner.
    let fd = got(
        tally,
        ns_fd(&member, &mut mine, maker_of.pid(), "user"),
        "the owner could not open a user namespace's link",
    )?;
    tally.ok(
        setns(&member, fd, CLONE_NEWUSER),
        "a user namespace's owner could not join it",
    )?;
    let everything = userns_caps(&member)?;
    if !everything {
        return Err("joining a user namespace did not give every capability there");
    }
    let (theirs, ours) = (
        link(&mut mine, maker_of.pid(), "user")?,
        link(&mut mine, member.pid(), "user")?,
    );
    if theirs != ours {
        return Err("setns into a user namespace left the caller in another");
    }
    // A mount namespace made inside it is owned by it, so another holder of
    // its capabilities may join that one.
    tally.ok(
        unshare(&maker_of, CLONE_NEWNS),
        "unshare(CLONE_NEWNS) was refused to a holder of CAP_SYS_ADMIN in a user namespace",
    )?;
    let mount = got(
        tally,
        ns_fd(&member, &mut mine, maker_of.pid(), "mnt"),
        "a mount namespace made in a user namespace could not be opened",
    )?;
    tally.ok(
        setns(&member, mount, CLONE_NEWNS),
        "a mount namespace made in a user namespace could not be joined from inside it",
    )?;
    // Root opens the link, then becomes someone else: not the owner.
    let opener = got(
        tally,
        ns_fd(&stranger, &mut other, maker_of.pid(), "user"),
        "root could not open a user namespace's link",
    )?;
    for call_of in [Syscall::Setgid, Syscall::Setuid] {
        call(&stranger, call_of, [1001, 0, 0, 0, 0, 0])
            .map(drop)
            .map_err(|_| "a check's process could not become 1001")?;
    }
    tally.refused(
        setns(&stranger, opener, 0),
        Errno::EPERM,
        "a user namespace was joined by someone who did not own it",
    )
}

/// Whether the process holds every capability, as `status` says.
fn userns_caps(process: &Arc<Process>) -> Result<bool, &'static str> {
    let mut page = page_for(process)?;
    let path = format!("/proc/{}/status", process.pid());
    let status = read_as(process, &mut page, path.as_bytes())?;
    Ok(status
        .split(|&byte| byte == b'\n')
        .any(|line| line == b"CapEff:\t000001ffffffffff"))
}

/// Mount namespaces: joining resets the root and working directory, and
/// fake root may not join the first's.
fn joining_mount(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let (maker_of, joiner) = (maker()?, maker()?);
    let (mut theirs, mut ours) = (page_for(&maker_of)?, page_for(&joiner)?);
    tally.ok(
        unshare(&maker_of, CLONE_NEWNS),
        "unshare(CLONE_NEWNS) was refused to root",
    )?;
    let fd = got(
        tally,
        ns_fd(&joiner, &mut ours, maker_of.pid(), "mnt"),
        "a namespace's mnt link could not be opened",
    )?;
    let dir = ours.put(b"/tmp")?;
    tally.ok(
        call(&joiner, Syscall::Chdir, [dir, 0, 0, 0, 0, 0]),
        "chdir was refused",
    )?;
    tally.ok(
        setns(&joiner, fd, CLONE_NEWNS),
        "setns into a mount namespace was refused to root",
    )?;
    if link(&mut ours, joiner.pid(), "mnt")? != link(&mut theirs, maker_of.pid(), "mnt")? {
        return Err("setns into a mount namespace left the caller in another");
    }
    ours.reset();
    let got_cwd = call(&joiner, Syscall::Getcwd, [ours.buffer(), 64, 0, 0, 0, 0]);
    tally.ok(got_cwd, "getcwd was refused")?;
    if ours.read_back(2)? != b"/\0" {
        return Err("setns into a mount namespace left the working directory behind");
    }
    // Fake root, in a user namespace it made, may not join the first's.
    let fake = person()?;
    let mut page = page_for(&fake)?;
    let first = got(
        tally,
        ns_fd(&fake, &mut page, fake.pid(), "mnt"),
        "a process could not open its own mount namespace",
    )?;
    tally.ok(
        unshare(&fake, CLONE_NEWUSER),
        "unshare(CLONE_NEWUSER) was refused to uid 1000",
    )?;
    tally.refused(
        setns(&fake, first, CLONE_NEWNS),
        Errno::EPERM,
        "fake root joined the first mount namespace",
    )
}

/// A pidfd opened for `pid` by `process`.
fn pidfd(process: &Process, pid: u32) -> Result<usize, Errno> {
    call(process, Syscall::PidfdOpen, [u64::from(pid), 0, 0, 0, 0, 0])
}

/// `setns` on a pidfd: several namespaces of a process at once, all or none.
fn joining_pidfd(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let (target, caller) = (maker()?, maker()?);
    let (mut theirs, mut ours) = (page_for(&target)?, page_for(&caller)?);
    tally.ok(
        unshare(&target, CLONE_NEWUTS | CLONE_NEWIPC),
        "unshare(CLONE_NEWUTS | CLONE_NEWIPC) was refused to root",
    )?;
    tally.ok(
        set_name(&mut theirs, Syscall::Sethostname, b"viapidfd")?,
        "root could not set a UTS namespace's host name",
    )?;
    let fd = tally.done(pidfd(&caller, target.pid()), "pidfd_open was refused")?;
    tally.refused(
        setns(&caller, fd, 0),
        Errno::EINVAL,
        "setns on a pidfd with no namespace flag was not EINVAL",
    )?;
    tally.refused(
        setns(&caller, fd, CLONE_NEWPID),
        Errno::EINVAL,
        "setns on a pidfd with CLONE_NEWPID, which it cannot join, was not EINVAL",
    )?;
    tally.ok(
        setns(&caller, fd, CLONE_NEWUTS | CLONE_NEWIPC),
        "setns on a pidfd was refused to root",
    )?;
    let (same_ipc, same_uts) = (
        link(&mut ours, caller.pid(), "ipc")? == link(&mut theirs, target.pid(), "ipc")?,
        link(&mut ours, caller.pid(), "uts")? == link(&mut theirs, target.pid(), "uts")?,
    );
    if !same_uts || names(&mut ours)?.0 != b"viapidfd" {
        return Err("setns on a pidfd did not join the UTS namespace");
    }
    if !same_ipc {
        return Err("setns on a pidfd did not join every namespace asked for");
    }
    joining_pidfd_user(tally)
}

/// An owner enters a user namespace and the UTS namespace it owns in one
/// call, which neither half does alone; and a refused call changes nothing.
fn joining_pidfd_user(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let (owner, joiner, same) = (person()?, person()?, maker()?);
    let mut made = page_for(&owner)?;
    let mut page = page_for(&joiner)?;
    tally.ok(
        unshare(&owner, CLONE_NEWUSER | CLONE_NEWUTS),
        "unshare(CLONE_NEWUSER | CLONE_NEWUTS) was refused to uid 1000",
    )?;
    tally.ok(
        set_name(&mut made, Syscall::Sethostname, b"entered")?,
        "the owner could not name the UTS namespace it made",
    )?;
    let fd = tally.done(pidfd(&joiner, owner.pid()), "pidfd_open was refused")?;
    tally.refused(
        setns(&joiner, fd, CLONE_NEWUTS),
        Errno::EPERM,
        "a person with no capability joined a UTS namespace alone through a pidfd",
    )?;
    tally.ok(
        setns(&joiner, fd, CLONE_NEWUSER | CLONE_NEWUTS),
        "an owner could not join a user namespace and its UTS namespace together",
    )?;
    if names(&mut page)?.0 != b"entered"
        || link(&mut page, joiner.pid(), "user")? != link(&mut page, owner.pid(), "user")?
    {
        return Err("setns on a pidfd left the caller outside what it joined");
    }
    // Joining one's own user namespace is refused, and the UTS namespace asked
    // for with it is not joined either.
    let mut alone = page_for(&same)?;
    let peer = maker()?;
    tally.ok(
        unshare(&peer, CLONE_NEWUTS),
        "unshare(CLONE_NEWUTS) was refused to root",
    )?;
    let before = link(&mut alone, same.pid(), "uts")?;
    let fd = tally.done(pidfd(&same, peer.pid()), "pidfd_open was refused")?;
    tally.refused(
        setns(&same, fd, CLONE_NEWUSER | CLONE_NEWUTS),
        Errno::EINVAL,
        "setns on a pidfd into the caller's own user namespace was not EINVAL",
    )?;
    if link(&mut alone, same.pid(), "uts")? != before {
        return Err("a refused setns on a pidfd joined part of what it asked for");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Namespace files
// ---------------------------------------------------------------------------

/// Two opens of one namespace are one inode, the link reads its name, the four
/// requests answer, and another person's namespaces do not open.
fn files(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let maker_of = person()?;
    let mut page = page_for(&maker_of)?;
    tally.ok(
        unshare(&maker_of, CLONE_NEWUSER | CLONE_NEWUTS),
        "unshare(CLONE_NEWUSER | CLONE_NEWUTS) was refused to uid 1000",
    )?;
    let pid = maker_of.pid();
    let one = got(
        tally,
        ns_fd(&maker_of, &mut page, pid, "uts"),
        "a UTS link did not open",
    )?;
    let two = got(
        tally,
        ns_fd(&maker_of, &mut page, pid, "uts"),
        "a UTS link did not open twice",
    )?;
    if inode_of(&maker_of, one)? != inode_of(&maker_of, two)? {
        return Err("two opens of one namespace were two inodes");
    }
    let named = link(&mut page, pid, "uts")?;
    if fd_link(&mut page, one)? != named {
        return Err("readlink of a namespace descriptor did not read its namespace's name");
    }
    let user = got(
        tally,
        ns_fd(&maker_of, &mut page, pid, "user"),
        "a user link did not open",
    )?;
    let asked = |process: &Process, fd: usize, request: u32, arg: u64| {
        call(
            process,
            Syscall::Ioctl,
            [fd as u64, u64::from(request), arg, 0, 0, 0],
        )
    };
    if asked(&maker_of, one, NS_GET_NSTYPE, 0) != Ok(CLONE_NEWUTS as usize) {
        return Err("NS_GET_NSTYPE of a UTS namespace did not answer CLONE_NEWUTS");
    }
    if asked(&maker_of, user, NS_GET_NSTYPE, 0) != Ok(CLONE_NEWUSER as usize) {
        return Err("NS_GET_NSTYPE of a user namespace did not answer CLONE_NEWUSER");
    }
    tally.refused(
        asked(&maker_of, one, NS_GET_PARENT, 0),
        Errno::EINVAL,
        "NS_GET_PARENT of a UTS namespace was not EINVAL",
    )?;
    page.reset();
    tally.refused(
        asked(&maker_of, one, NS_GET_OWNER_UID, page.buffer()),
        Errno::EINVAL,
        "NS_GET_OWNER_UID of a UTS namespace was not EINVAL",
    )?;
    tally.ok(
        asked(&maker_of, user, NS_GET_OWNER_UID, page.buffer()),
        "NS_GET_OWNER_UID of a user namespace was refused",
    )?;
    // The owner's kernel id 1000 is told as the caller's namespace names it:
    // nothing is mapped there, so as 65534; and as the first names it, 1000.
    if page.read_back(4)? != 65_534_u32.to_le_bytes() {
        return Err("NS_GET_OWNER_UID did not tell the owner as the caller's namespace names it");
    }
    let viewer = person()?;
    let mut outside = page_for(&viewer)?;
    let theirs = got(
        tally,
        ns_fd(&viewer, &mut outside, pid, "user"),
        "the owner's own ids could not open a user namespace's link",
    )?;
    tally.ok(
        asked(&viewer, theirs, NS_GET_OWNER_UID, outside.buffer()),
        "NS_GET_OWNER_UID was refused to the owner",
    )?;
    if outside.read_back(4)? != 1000_u32.to_le_bytes() {
        return Err("NS_GET_OWNER_UID did not tell the owner's id");
    }
    files_related(tally, &maker_of, one, user)?;
    // Another person's namespaces do not open.
    let other = person()?;
    let mut elsewhere = page_for(&other)?;
    let bystander = maker()?;
    tally.refused(
        ns_fd(&other, &mut elsewhere, bystander.pid(), "uts")?,
        Errno::EACCES,
        "uid 1000 opened another person's namespace",
    )
}

/// `NS_GET_USERNS` opens the owner only for a caller inside what owns it.
fn files_related(
    tally: &mut Tally<'_>,
    maker_of: &Arc<Process>,
    uts: usize,
    user: usize,
) -> Result<(), &'static str> {
    let ask = |fd: usize| {
        call(
            maker_of,
            Syscall::Ioctl,
            [fd as u64, u64::from(NS_GET_USERNS), 0, 0, 0, 0],
        )
    };
    // The UTS namespace's owner is the user namespace the caller is in.
    let owner = tally.done(
        ask(uts),
        "NS_GET_USERNS of the caller's own UTS namespace failed",
    )?;
    if inode_of(maker_of, owner)? != inode_of(maker_of, user)? {
        return Err("NS_GET_USERNS did not open the namespace's owner");
    }
    // The caller's own user namespace's owner is the first, outside it.
    tally.refused(
        ask(user),
        Errno::EPERM,
        "NS_GET_USERNS showed a process a user namespace it is not inside of",
    )
}

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// Flags that cannot be given: a thread cannot have one of the small ones;
/// `CLONE_NEWIPC` excludes
/// `CLONE_SYSVSEM`.
fn flags(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let root = maker()?;
    let asked = |flags: u64| family::namespaces_asked(&root, flags).map(|()| 0);
    tally.refused(
        asked(CLONE_NEWIPC | CLONE_SYSVSEM),
        Errno::EINVAL,
        "CLONE_NEWIPC with CLONE_SYSVSEM was not refused EINVAL",
    )?;
    tally.refused(
        asked(CLONE_NEWUTS | CLONE_THREAD),
        Errno::EINVAL,
        "CLONE_NEWUTS with CLONE_THREAD was not refused EINVAL",
    )?;
    tally.ok(
        asked(CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWCGROUP),
        "the three small namespaces together were refused to root",
    )?;
    let low = person()?;
    tally.refused(
        family::namespaces_asked(&low, CLONE_NEWCGROUP).map(|()| 0),
        Errno::EPERM,
        "clone(CLONE_NEWCGROUP) by uid 1000 was not refused EPERM",
    )?;
    tally.ok(
        family::namespaces_asked(&low, CLONE_NEWCGROUP | CLONE_NEWUSER).map(|()| 0),
        "CLONE_NEWCGROUP with CLONE_NEWUSER was refused to uid 1000",
    )
}
