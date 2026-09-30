//! PID namespaces, proved at boot (`docs/PIDNS.md`): the rules P1 to P11
//! attempted and refused, driven through the system-call layer with
//! processes the check makes, as a program's calls would be.
//!
//! * **P1, P2, P10.** The first process made in a namespace is pid 1 there
//!   and has another number outside; the next is 2; an `unshare` moves the
//!   caller's later children and not the caller.
//! * **P3.** `kill`, `wait4`, `getppid`, `getpgid`, `getsid` and `setsid`
//!   speak the caller's namespace, both ways: a process a namespace cannot
//!   see is `ESRCH` or 0, and a namespace's init has no parent.
//! * **P4, P5.** An orphan goes to its namespace's init; the init's end kills
//!   the namespace and shuts it to new members.
//! * **P6.** An init ignores what it has no handler for, from inside and
//!   outside alike, except `SIGKILL` and `SIGSTOP` from an ancestor.
//! * **P7.** `si_pid`, `ssi_pid` and `SO_PEERCRED` tell the reader's number,
//!   and 0 for a sender the reader cannot see.
//! * **P8.** A procfs mounted in a namespace lists only its processes, by its
//!   numbers; `self`, `NSpid` and `ns/pid` follow.
//! * **P9.** `CLONE_NEWPID` needs privilege, except with `CLONE_NEWUSER`; is
//!   refused with `CLONE_THREAD` and `CLONE_PARENT`; and `ENOSPC` past 32.
//! * **P11.** `cgroup.procs` reads and writes the reader's numbers.
//!
//! P12, the charge, is `fs::kmem_check`'s.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::socket::Ucred;
use ferrix_linux_abi::types::{MNT_DETACH, SIGKILL, SIGSTOP, SIGTERM, SIGUSR1, SIGUSR2};
use ferrix_vfs::{Errno, OpenFlags};

use crate::fs;
use crate::fs::mount_check::{self, Page, Report as Counts, Tally, by_number, page_for};
use crate::fs::namespace_check::{read_file, read_link, unshare};
use crate::syscall::family;
use crate::syscall::namespace::CLONE_NEWUSER;
use crate::syscall::pidns::{self, CLONE_NEWPID};
use crate::syscall::process::{self, Process};
use crate::syscall::signal::{self, Origin};
use crate::syscall::userns;

/// `CLONE_THREAD` and `CLONE_PARENT`, which `CLONE_NEWPID` refuses.
const CLONE_THREAD: u64 = 0x0001_0000;
/// See [`CLONE_THREAD`].
const CLONE_PARENT: u64 = 0x0000_8000;

/// A handler address that is not `SIG_DFL` or `SIG_IGN`.
const HANDLER: u64 = 0x4000;

/// Where the check mounts a procfs and a cgroupfs.
const PROC_POINT: &[u8] = b"/tmp/.pidns-proc";
/// See [`PROC_POINT`].
const CGROUP_POINT: &[u8] = b"/tmp/.pidns-cgroup";

/// The ids the unprivileged part of the check runs as.
const UID: u32 = 1000;

/// Run the check.
///
/// # Errors
///
/// Which rule failed.
pub(crate) fn run() -> Result<Counts, &'static str> {
    let mut counts = Counts::default();
    let mut tally = Tally {
        report: &mut counts,
    };
    numbering(&mut tally)?;
    translation(&mut tally)?;
    orphans(&mut tally)?;
    init_death(&mut tally)?;
    protection(&mut tally)?;
    reported(&mut tally)?;
    procfs(&mut tally)?;
    flags(&mut tally)?;
    cgroup_procs(&mut tally)?;
    Ok(counts)
}

/// A namespace with its init (pid 1 there) and a second process, both made
/// as `clone(CLONE_NEWPID)` and `fork` make them, and the process that made
/// the namespace in the first.
struct World {
    /// In the first namespace, which made the namespace with `unshare`.
    maker: Arc<Process>,
    /// Pid 1 of the namespace.
    init: Arc<Process>,
    /// Pid 2 of it, a child of `init`.
    member: Arc<Process>,
}

/// Make a [`World`].
fn make_world() -> Result<World, &'static str> {
    let maker = process::new_for_check().map_err(|_| "no process for the pid namespace check")?;
    let made = unshare(&maker, CLONE_NEWPID);
    if made.is_err() {
        return Err("unshare(CLONE_NEWPID) was refused to root");
    }
    let init = child_in(&maker)?;
    let member = child_in(&init)?;
    Ok(World {
        maker,
        init,
        member,
    })
}

/// A fork child of `parent`, listed among its children: where the parent's
/// children go.
fn child_in(parent: &Arc<Process>) -> Result<Arc<Process>, &'static str> {
    let child = process::fork_for_check_in(parent, None)
        .map_err(|_| "no memory for a fork in a pid namespace check")?;
    parent.adopt(Arc::clone(&child));
    Ok(child)
}

/// `call` by number, as the process.
fn call(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    by_number(process, call, args)
}

/// An argument that is a `pid_t`.
fn pid_arg(pid: i64) -> u64 {
    u64::from(pid as i32 as u32)
}

/// `getpid()` or `getppid()` of `process`.
fn id_of(process: &Process, call_: Syscall) -> Result<usize, &'static str> {
    call(process, call_, [0; 6]).map_err(|_| "getpid or getppid failed")
}

/// `kill(pid, signal)` by `process`.
fn kill(process: &Process, pid: i64, signal: u32) -> Result<usize, Errno> {
    call(
        process,
        Syscall::Kill,
        [pid_arg(pid), u64::from(signal), 0, 0, 0, 0],
    )
}

/// Whether `signal` is pending for `process`, taking it away if so.
fn pending(process: &Process, signal: u32) -> bool {
    process.with_signals(|signals| {
        let was = signals.pending() & signal::bit(signal) != 0;
        signals.cancel(signal);
        was
    })
}

/// Give `process` a handler for `signal`, so that it is not at its default.
fn handle(process: &Process, signal: u32) {
    process.with_signals(|signals| signals.install_action(signal, HANDLER, 0));
}

/// P1, P2 and P10: who is pid 1, who is pid 2, and who moved.
fn numbering(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let world = make_world()?;
    let (maker, init, member) = (&world.maker, &world.init, &world.member);
    let before = maker.pid();
    // P10: the caller did not move; its children did.
    if id_of(maker, Syscall::Getpid)? != before as usize || maker.numbers().is_some() {
        return Err("unshare(CLONE_NEWPID) moved the caller into the new namespace");
    }
    let mut page = page_for(maker)?;
    let own = read_link(&mut page, format!("/proc/{before}/ns/pid").as_bytes())?;
    let children = read_link(
        &mut page,
        format!("/proc/{before}/ns/pid_for_children").as_bytes(),
    )?;
    if own == children || !own.starts_with(b"pid:[") {
        return Err("pid_for_children after unshare(CLONE_NEWPID) names the namespace it is in");
    }
    // P1: the first process of a namespace is pid 1 in it and another outside.
    tally.report.calls += 2;
    if id_of(init, Syscall::Getpid)? != 1 {
        return Err("the first process made in a pid namespace was not pid 1 there");
    }
    if init.pid() < 3 || member.pid() < 3 {
        return Err("a namespace's processes were numbered in the kernel's own small numbers");
    }
    if pidns::to_user(maker, init) != init.pid() {
        return Err("the process that made a namespace does not see its init by its own number");
    }
    // P2: the next is 2, and its parent is 1.
    if id_of(member, Syscall::Getpid)? != 2 {
        return Err("the second process made in a pid namespace was not pid 2 there");
    }
    if id_of(member, Syscall::Getppid)? != 1 {
        return Err("a namespace's second process does not see its init as its parent");
    }
    // Another namespace numbers independently.
    let other = make_world()?;
    if id_of(&other.init, Syscall::Getpid)? != 1 || id_of(&other.member, Syscall::Getpid)? != 2 {
        return Err("a second pid namespace did not number from 1 by itself");
    }
    // The init's own pid_for_children is its namespace.
    let inside = read_link(
        &mut page_for(init)?,
        format!("/proc/{}/ns/pid_for_children", init.pid()).as_bytes(),
    )?;
    let outside = read_link(
        &mut page_for(init)?,
        format!("/proc/{}/ns/pid", init.pid()).as_bytes(),
    )?;
    if inside != outside || inside == own {
        return Err("a namespace's init is not in the namespace its children are made in");
    }
    Ok(())
}

/// P3: calls speak the caller's namespace.
fn translation(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let World {
        maker,
        init,
        member,
    } = make_world()?;
    let outsider = process::new_for_check().map_err(|_| "no outsider for the translation check")?;
    handle(&member, SIGUSR1);
    // kill by the namespace's number reaches the process that has it.
    tally.ok(
        kill(&init, 2, SIGUSR1),
        "kill(2) from inside a pid namespace found nothing",
    )?;
    if !pending(&member, SIGUSR1) {
        return Err("kill by a namespace's number did not reach the process that has it");
    }
    // From outside it is the kernel's number, and inside a kernel number of
    // a process the namespace has no name for is no process.
    tally.ok(
        kill(&maker, i64::from(member.pid()), SIGUSR1),
        "kill from outside a pid namespace by the kernel's number found nothing",
    )?;
    if !pending(&member, SIGUSR1) {
        return Err("kill by the kernel's number did not reach a process in a namespace");
    }
    tally.refused(
        kill(&init, i64::from(outsider.pid()), 0),
        Errno::ESRCH,
        "a pid namespace could signal a process it cannot see",
    )?;
    tally.refused(
        kill(&init, i64::from(member.pid()), 0),
        Errno::ESRCH,
        "a pid namespace took the kernel's number for one of its own",
    )?;
    // A namespace's init has no parent there; nor does a process whose
    // parent is outside.
    if id_of(&init, Syscall::Getppid)? != 0 {
        return Err("getppid of a pid namespace's init was not 0");
    }
    // Groups and sessions: the init inherited its parent's, which it cannot
    // see; a session it leads it sees as 1, and the outside as its own number.
    if call(&init, Syscall::Getpgid, [0; 6]) != Ok(0)
        || call(&init, Syscall::Getsid, [0; 6]) != Ok(0)
    {
        return Err("a namespace's init saw the process group or session of its parent outside");
    }
    tally.ok(
        call(&member, Syscall::Setsid, [0; 6]),
        "setsid in a pid namespace failed",
    )?;
    let seen_inside = call(&init, Syscall::Getsid, [2, 0, 0, 0, 0, 0]);
    let seen_outside = call(
        &maker,
        Syscall::Getsid,
        [u64::from(member.pid()), 0, 0, 0, 0, 0],
    );
    if seen_inside != Ok(2) || seen_outside != Ok(member.pid() as usize) {
        return Err("a session was not told in the reader's namespace's numbers");
    }
    if call(&init, Syscall::Getpgid, [2, 0, 0, 0, 0, 0]) != Ok(2) {
        return Err("a process group was not told in the reader's namespace's numbers");
    }
    // A wait names a child by the caller's number, and reaps it under it.
    tally.refused(
        family::sys_wait4(
            &init,
            member.pid() as i32,
            0,
            1,
            0,
            crate::trap::Abi::Native,
        ),
        Errno::ECHILD,
        "wait4 took a kernel number for a child of a pid namespace",
    )?;
    let member_pid = member.pid();
    process::kill(&member, 0);
    let reaped = family::sys_wait4(&init, 2, 0, 0, 0, crate::trap::Abi::Native)
        .map_err(|_| "wait4 by a namespace's number found no child")?;
    if reaped != 2 {
        return Err("wait4 did not tell the reaped child's number in the caller's namespace");
    }
    drop(member);
    if pidns::find_in(&maker, member_pid).is_some() {
        return Err("a reaped process of a pid namespace was still found by its number");
    }
    Ok(())
}

/// P4: an orphan goes to its namespace's init.
fn orphans(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let World { init, member, .. } = make_world()?;
    let grandchild = child_in(&member)?;
    process::kill(&member, 0);
    let parent = grandchild
        .parent()
        .ok_or("an orphan in a pid namespace was given no parent")?;
    if !Arc::ptr_eq(&parent, &init) {
        return Err("an orphan in a pid namespace was not given to its namespace's init");
    }
    tally.report.calls += 1;
    if id_of(&grandchild, Syscall::Getppid)? != 1 {
        return Err("an orphan in a pid namespace did not see its namespace's init as its parent");
    }
    Ok(())
}

/// P5: the end of an init ends its namespace.
fn init_death(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let World {
        maker,
        init,
        member,
    } = make_world()?;
    let namespace = maker
        .children_namespace()
        .ok_or("a namespace's maker lost the namespace it made")?;
    process::kill(&init, 0);
    tally.report.calls += 1;
    if !member.is_terminated() {
        return Err("a process in a pid namespace outlived its init");
    }
    // Nothing joins a namespace that is ending, whoever is asked to.
    if process::fork_for_check_in(&maker, Some(Arc::clone(&namespace))).is_ok() {
        return Err("a process joined a pid namespace whose init had gone");
    }
    tally.report.refusals += 1;
    Ok(())
}

/// P6: what an init does not take, from inside and from outside.
fn protection(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let World {
        maker,
        init,
        member,
    } = make_world()?;
    // From inside: nothing that would end or stop it, and a signal it
    // catches is delivered.
    tally.ok(
        kill(&member, 1, SIGTERM),
        "kill of a namespace's init from inside was refused, not ignored",
    )?;
    if init.is_terminated() || pending(&init, SIGTERM) {
        return Err("the init of a pid namespace took SIGTERM from inside it");
    }
    tally.ok(
        kill(&member, 1, SIGKILL),
        "SIGKILL to a namespace's init from inside was refused, not ignored",
    )?;
    if init.is_terminated() {
        return Err("SIGKILL from inside a pid namespace ended its init");
    }
    tally.ok(
        kill(&member, 1, SIGSTOP),
        "SIGSTOP to a namespace's init from inside was refused, not ignored",
    )?;
    if pending(&init, SIGSTOP) {
        return Err("SIGSTOP from inside a pid namespace reached its init");
    }
    handle(&init, SIGUSR1);
    tally.ok(
        kill(&member, 1, SIGUSR1),
        "a signal an init catches was refused from inside",
    )?;
    if !pending(&init, SIGUSR1) {
        return Err("a signal an init catches was not delivered from inside its namespace");
    }
    // From outside: SIGSTOP reaches it, SIGTERM without a handler does not,
    // SIGKILL ends it and with it the namespace.
    tally.ok(
        kill(&maker, i64::from(init.pid()), SIGTERM),
        "SIGTERM to a namespace's init from outside was refused",
    )?;
    if init.is_terminated() || pending(&init, SIGTERM) {
        return Err("the init of a pid namespace took SIGTERM, which it does not catch");
    }
    tally.ok(
        kill(&maker, i64::from(init.pid()), SIGSTOP),
        "SIGSTOP to a namespace's init from outside was refused",
    )?;
    if !pending(&init, SIGSTOP) {
        return Err("SIGSTOP from an ancestor namespace did not reach a namespace's init");
    }
    tally.ok(
        kill(&maker, i64::from(init.pid()), SIGKILL),
        "SIGKILL to a namespace's init from outside was refused",
    )?;
    if !init.is_terminated() || !member.is_terminated() {
        return Err(
            "SIGKILL from an ancestor namespace did not end a namespace's init and with it the namespace",
        );
    }
    Ok(())
}

/// The pid `origin` tells a reader, from the `signalfd_siginfo` and the
/// handler's `siginfo`.
fn told(reader: &Arc<Process>, origin: Origin) -> Result<(u32, u32), &'static str> {
    userns::acting_as(reader, || {
        let field = |bytes: &[u8], at: usize| {
            bytes
                .get(at..at + 4)
                .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
                .map_or(u32::MAX, u32::from_le_bytes)
        };
        let union = if size_of::<usize>() == 8 { 16 } else { 12 };
        (
            field(&origin.encode(SIGUSR2), union),
            field(&origin.encode_signalfd(SIGUSR2), 12),
        )
    })
}

/// P7: `si_pid`, `ssi_pid` and `SO_PEERCRED` are told to the reader.
fn reported(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let World {
        maker,
        init,
        member,
    } = make_world()?;
    let other = make_world()?;
    let from_member = Origin::User {
        pid: member.pid(),
        uid: 0,
    };
    tally.report.calls += 1;
    if told(&init, from_member)? != (2, 2) {
        return Err("si_pid was not told in the reader's namespace's numbers");
    }
    if told(&maker, from_member)? != (member.pid(), member.pid()) {
        return Err("si_pid was not told as the kernel's number to a reader outside");
    }
    if told(&other.init, from_member)? != (0, 0) {
        return Err("si_pid named a sender the reader's namespace cannot see");
    }
    let from_outside = Origin::Thread {
        pid: maker.pid(),
        uid: 0,
    };
    if told(&init, from_outside)? != (0, 0) {
        return Err("si_pid named a sender outside the reader's namespace");
    }
    // SO_PEERCRED and SCM_CREDENTIALS are told by the same rule.
    let stamp = Ucred {
        pid: i32::try_from(member.pid()).unwrap_or(0),
        uid: 0,
        gid: 0,
    };
    let seen = userns::acting_as(&init, || fs::socket::as_seen(stamp).pid)?;
    if seen != 2 {
        return Err("SO_PEERCRED named the peer by the kernel's number inside a pid namespace");
    }
    Ok(())
}

/// The numeric names in directory `path`, as a listing reads them.
fn numbers_in(path: &[u8]) -> Result<Vec<u32>, &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let flags = OpenFlags {
        read: true,
        directory: true,
        ..OpenFlags::default()
    };
    let dir = ns
        .open(&ctx, None, path, &flags, 0)
        .map_err(|_| "a procfs of a pid namespace would not open")?;
    let mut names = Vec::new();
    dir.read_dir(&mut |entry| {
        if let Some(number) = core::str::from_utf8(entry.name)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
        {
            names.push(number);
        }
        true
    })
    .map_err(|_| "a procfs of a pid namespace could not be listed")?;
    names.sort_unstable();
    Ok(names)
}

/// Mount `kind` on `point` as `process`, making the directory first.
fn mount_on(
    process: &Process,
    page: &mut Page<'_>,
    point: &[u8],
    kind: &[u8],
) -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    ns.mkdir(&ctx, None, point, 0o755)
        .map_err(|_| "no directory for a pid namespace check's mount")?;
    page.reset();
    let (source, target, name) = (page.put(b"none")?, page.put(point)?, page.put(kind)?);
    call(process, Syscall::Mount, [source, target, name, 0, 0, 0])
        .map(drop)
        .map_err(|_| "a pid namespace check's filesystem would not mount")
}

/// Unmount what [`mount_on`] mounted, and remove the directory.
fn unmount(process: &Process, page: &mut Page<'_>, point: &[u8]) -> Result<(), &'static str> {
    page.reset();
    let target = page.put(point)?;
    let gone = call(
        process,
        Syscall::Umount2,
        [target, u64::from(MNT_DETACH), 0, 0, 0, 0],
    );
    let ns = fs::namespace();
    let ctx = ns.context();
    let removed = ns.rmdir(&ctx, None, point);
    if gone.is_err() || removed.is_err() {
        return Err("a pid namespace check's mount would not go");
    }
    Ok(())
}

/// The value of `key` in `process`'s status, as `reader` reads it.
fn status_line(reader: &Arc<Process>, path: &[u8], key: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut page = page_for(reader)?;
    let text = userns::acting_as(reader, || read_file(&mut page, path))??
        .map_err(|_| "a status file in a pid namespace could not be read")?;
    text.split(|&byte| byte == b'\n')
        .find(|line| line.starts_with(key))
        .map(<[u8]>::to_vec)
        .ok_or("a line was missing from a status file in a pid namespace")
}

/// P8: a procfs mounted in a namespace.
fn procfs(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let World {
        maker,
        init,
        member,
    } = make_world()?;
    let mut page = page_for(&init)?;
    mount_on(&init, &mut page, PROC_POINT, b"proc")?;
    let outcome = in_procfs(tally, &maker, &init, &member);
    let unmounted = unmount(&init, &mut page, PROC_POINT);
    outcome.and(unmounted)
}

/// [`procfs`], with the mount made.
fn in_procfs(
    tally: &mut Tally<'_>,
    maker: &Arc<Process>,
    init: &Arc<Process>,
    member: &Arc<Process>,
) -> Result<(), &'static str> {
    let root = |tail: &str| format!("{}{tail}", core::str::from_utf8(PROC_POINT).unwrap_or(""));
    if numbers_in(PROC_POINT)? != [1, 2] {
        return Err(
            "a procfs of a pid namespace did not list exactly its processes by its numbers",
        );
    }
    tally.report.calls += 1;
    let mut page = page_for(init)?;
    let kernel_number = root(&format!("/{}/status", member.pid()));
    tally.refused(
        userns::acting_as(init, || read_file(&mut page, kernel_number.as_bytes()))??
            .map(|text| text.len()),
        Errno::ENOENT,
        "a procfs of a pid namespace found a process by the kernel's number",
    )?;
    // Status, read by a process in the namespace and by one outside.
    let inside = status_line(init, root("/2/status").as_bytes(), b"NSpid:")?;
    if inside != b"NSpid:\t2" {
        return Err("NSpid in a pid namespace did not list the number there alone");
    }
    let parent = status_line(init, root("/2/status").as_bytes(), b"PPid:")?;
    if parent != b"PPid:\t1" {
        return Err("PPid in a pid namespace was not told in its numbers");
    }
    let outside = status_line(
        maker,
        format!("/proc/{}/status", member.pid()).as_bytes(),
        b"NSpid:",
    )?;
    if outside != format!("NSpid:\t{}\t2", member.pid()).as_bytes() {
        return Err("NSpid did not list the numbers from the reader's namespace down");
    }
    // `self`, as the namespace that mounted it numbers the reader.
    let mut page = page_for(member)?;
    let link = userns::acting_as(member, || read_link(&mut page, root("/self").as_bytes()))??;
    if link != b"2" {
        return Err("/proc/self in a pid namespace was not the reader's number there");
    }
    // `ns/pid` names the namespace, apart from the first.
    let mut page = page_for(maker)?;
    let first = read_link(
        &mut page,
        format!("/proc/{}/ns/pid", maker.pid()).as_bytes(),
    )?;
    let own = read_link(&mut page, root("/1/ns/pid").as_bytes())?;
    if first == own
        || own != read_link(&mut page, format!("/proc/{}/ns/pid", init.pid()).as_bytes())?
    {
        return Err(
            "ns/pid did not name a pid namespace apart from the first, the same from every mount",
        );
    }
    Ok(())
}

/// A process that is uid and gid 1000 in the first namespace.
fn unprivileged() -> Result<Arc<Process>, &'static str> {
    let user = process::new_for_check().map_err(|_| "no process for the pid flags check")?;
    let ids = [u64::from(UID), 0, 0, 0, 0, 0];
    if call(&user, Syscall::Setgid, ids).is_err() || call(&user, Syscall::Setuid, ids).is_err() {
        return Err("the pid flags check's process could not become uid 1000");
    }
    Ok(user)
}

/// P9: who may ask for a pid namespace, with what, and how deep.
fn flags(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let user = unprivileged()?;
    tally.refused(
        unshare(&user, CLONE_NEWPID),
        Errno::EPERM,
        "uid 1000 made a pid namespace with unshare",
    )?;
    tally.refused(
        family::namespaces_asked(&user, CLONE_NEWPID).map(|()| 0),
        Errno::EPERM,
        "uid 1000 cloned into a pid namespace",
    )?;
    tally.ok(
        family::namespaces_asked(&user, CLONE_NEWPID | CLONE_NEWUSER).map(|()| 0),
        "uid 1000 could not clone into a pid namespace inside a user namespace of its own",
    )?;
    let root = process::new_for_check().map_err(|_| "no process for the pid flags check")?;
    for bad in [CLONE_THREAD, CLONE_PARENT] {
        tally.refused(
            family::namespaces_asked(&root, CLONE_NEWPID | bad).map(|()| 0),
            Errno::EINVAL,
            "CLONE_NEWPID was accepted with CLONE_THREAD or CLONE_PARENT",
        )?;
    }
    // The unprivileged route: a user namespace first, then the pid namespace
    // it owns, in one call, and the child is pid 1 there.
    tally.ok(
        unshare(&user, CLONE_NEWUSER | CLONE_NEWPID),
        "uid 1000 could not unshare a user and a pid namespace together",
    )?;
    let child = child_in(&user)?;
    if id_of(&child, Syscall::Getpid)? != 1 {
        return Err("a process made in an unprivileged user and pid namespace was not pid 1");
    }
    // Nesting is bounded at 32, and each level is a namespace.
    let deep = process::new_for_check().map_err(|_| "no process for the pid depth check")?;
    for _ in 0..pidns::MAX_LEVEL {
        tally.ok(
            unshare(&deep, CLONE_NEWPID),
            "a pid namespace within the depth limit was refused",
        )?;
    }
    tally.refused(
        unshare(&deep, CLONE_NEWPID),
        Errno::ENOSPC,
        "a 33rd level of pid namespaces was made",
    )?;
    tally.refused(
        family::namespaces_asked(&deep, CLONE_NEWPID).map(|()| 0),
        Errno::ENOSPC,
        "a clone into a 33rd level of pid namespaces was accepted",
    )?;
    let inner = child_in(&deep)?;
    if id_of(&inner, Syscall::Getpid)? != 1 {
        return Err("a process in the deepest pid namespace was not pid 1");
    }
    Ok(())
}

/// The names in `cgroup.procs`, as `reader` reads them.
fn procs_of(reader: &Arc<Process>) -> Result<Vec<u8>, &'static str> {
    let mut page = page_for(reader)?;
    let path = format!(
        "{}/cgroup.procs",
        core::str::from_utf8(CGROUP_POINT).unwrap_or("")
    );
    userns::acting_as(reader, || read_file(&mut page, path.as_bytes()))??
        .map_err(|_| "cgroup.procs could not be read in a pid namespace check")
}

/// P11: `cgroup.procs` lists and takes the reader's numbers.
fn cgroup_procs(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let World {
        maker,
        init,
        member,
    } = make_world()?;
    let mut page = page_for(&maker)?;
    mount_on(&maker, &mut page, CGROUP_POINT, b"cgroup2")?;
    let outcome = in_cgroup(tally, &maker, &init, &member);
    let unmounted = unmount(&maker, &mut page, CGROUP_POINT);
    outcome.and(unmounted)
}

/// [`cgroup_procs`], with the mount made.
fn in_cgroup(
    tally: &mut Tally<'_>,
    maker: &Arc<Process>,
    init: &Arc<Process>,
    member: &Arc<Process>,
) -> Result<(), &'static str> {
    if procs_of(init)? != b"1\n2\n" {
        return Err(
            "cgroup.procs listed more than the reader's namespace's processes by its numbers",
        );
    }
    let outside = procs_of(maker)?;
    let listed = |pid: u32| {
        outside
            .split(|&byte| byte == b'\n')
            .any(|line| line == format!("{pid}").as_bytes())
    };
    if !listed(init.pid()) || !listed(member.pid()) {
        return Err(
            "cgroup.procs did not list a namespace's processes by the kernel's numbers outside it",
        );
    }
    // A write names a process by the writer's number: one it cannot see is
    // no process.
    let mut page = page_for(init)?;
    let path = format!(
        "{}/cgroup.procs",
        core::str::from_utf8(CGROUP_POINT).unwrap_or("")
    );
    let number = format!("{}\n", maker.pid());
    page.reset();
    let opened = userns::acting_as(init, || {
        mount_check::open(
            &mut page,
            path.as_bytes(),
            ferrix_linux_abi::types::O_WRONLY,
            0,
        )
    })??
    .map_err(|_| "cgroup.procs would not open for writing")?;
    let at = page.put_bytes(number.as_bytes())?;
    let written = userns::acting_as(init, || {
        call(
            init,
            Syscall::Write,
            [opened as u64, at, number.len() as u64, 0, 0, 0],
        )
    })?;
    mount_check::close(init, opened);
    tally.refused(
        written,
        Errno::ESRCH,
        "a pid namespace moved a process it cannot see into a cgroup",
    )?;
    Ok(())
}
