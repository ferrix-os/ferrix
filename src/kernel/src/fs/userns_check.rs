//! User namespaces, proved at boot (`docs/NAMESPACES.md`, landing N4): the
//! rules of §4 attempted and refused, driven through the system-call layer as
//! a program's calls would be.
//!
//! A process that becomes uid 1000 and makes a user namespace must:
//!
//! * be named apart from the first namespace by `/proc/<pid>/ns/user`, and
//!   read every id it is given as 65534 until it is mapped (U9);
//! * be refused a `gid_map` before `setgroups` is denied (U4), a `uid_map`
//!   that names kernel root or more than one id (U2), and a second write (U3);
//! * be accepted its own id, mapped to 0, and then read 0 for `getuid` and in
//!   `status`, where the first namespace reads the kernel's 1000;
//! * hold every capability in the namespace and none outside it (U1): it is
//!   refused `sethostname`, a `mount`, `setuid` to an id it does not map
//!   (`EINVAL`) and `setgroups` (`EPERM`, U4), and can drop a capability from
//!   its bounding set, which `PR_CAPBSET_READ` then reports.
//!
//! A chrooted process is refused a user namespace (U6). The rules that need a
//! credential no system call can build are proved on `Credentials` directly:
//! a map written by a writer who is not privileged over the parent, through a
//! file a privileged process opened, is refused (U3); `execve` in a child
//! namespace ignores a set-user-id bit and gives the capability sets only to
//! the namespace's root (U7). And `/proc/sys`, bound onto itself read-only as
//! bubblewrap does, refuses a write with `EROFS`, which is what keeps a
//! container's root from the host's sysctls (the N3 review's condition).

use alloc::format;
use alloc::vec::Vec;

use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{AT_FDCWD, AT_REMOVEDIR, MNT_DETACH, MS_BIND, MS_RDONLY, MS_REMOUNT};
use ferrix_vfs::Errno;

use crate::fs::mount_check::{Page, Report as Counts, Tally, by_number, close, open, page_for};
use crate::fs::namespace_check::{read_file, read_link, staged, unshare};
use crate::syscall::credentials::Credentials;
use crate::syscall::family;
use crate::syscall::namespace::{CLONE_NEWNS, CLONE_NEWUSER};
use crate::syscall::process::{self, Process};
use crate::syscall::registry;
use crate::syscall::signal::{self, Origin};
use crate::syscall::userns::{self, CAP_SYS_ADMIN, Kind};

/// A directory the chroot test works in.
const JAIL: &[u8] = b"/tmp/.userns-jail";

/// `prctl`'s capability bounding set options.
const PR_CAPBSET_READ: u64 = 23;
/// See [`PR_CAPBSET_READ`].
const PR_CAPBSET_DROP: u64 = 24;

/// `status`'s `CapEff` for every capability: bits 0 to 40.
const EVERY_CAPABILITY: &[u8] = b"CapEff:\t000001ffffffffff";

/// The ids this check uses.
const UID: u32 = 1000;

/// What the check saw, for the boot line.
pub(crate) fn run() -> Result<Counts, &'static str> {
    let mut counts = Counts::default();
    let mut tally = Tally {
        report: &mut counts,
    };
    let user = process::new_for_check()
        .map_err(|_| "could not make the user namespace check's process")?;
    let mut page = page_for(&user)?;
    let outcome = user_namespace(&mut page, &mut tally)
        .and_then(|()| chrooted(&mut tally))
        .and_then(|()| root_made(&mut tally))
        .and_then(|()| clone_flags(&mut tally))
        .and_then(|()| sender_ids(&mut tally))
        .and_then(|()| nested(&mut tally))
        .and_then(|()| credentials(&mut tally))
        .and_then(|()| read_only_sysctls(&mut tally));
    outcome.map(|()| counts)
}

/// `call` by number, on a scratch page of the process's own.
fn call(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    by_number(process, call, args)
}

/// `body` with the page's process as the one making the calls, which the
/// boot check's own thread, with no process, cannot be.
fn acting<R>(pid: u32, body: impl FnOnce() -> R) -> Result<R, &'static str> {
    let process = registry::find(pid).ok_or("the check's process was not registered")?;
    userns::acting_as(&process, body)
}

/// A file of `/proc` written once from its start: the result of the write,
/// or why it would not open.
fn write_to(
    page: &mut Page<'_>,
    path: &[u8],
    data: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    // Opened and written as the same process: the map file judges who opened
    // it beside who writes.
    let process = registry::find(page.process.pid()).ok_or("the check's process is gone")?;
    let opened = userns::acting_as(&process, || {
        open(page, path, ferrix_linux_abi::types::O_WRONLY, 0)
    })??;
    let fd = match opened {
        Ok(fd) => fd,
        Err(errno) => return Ok(Err(errno)),
    };
    let at = page.put_bytes(data)?;
    let written = userns::acting_as(&process, || {
        call(
            page.process,
            Syscall::Write,
            [fd as u64, at, data.len() as u64, 0, 0, 0],
        )
    })?;
    close(page.process, fd);
    Ok(written)
}

/// The value of `key` (up to its newline) in `/proc/self/status`.
fn status(page: &mut Page<'_>, key: &[u8]) -> Result<Vec<u8>, &'static str> {
    let pid = page.process.pid();
    let text = acting(pid, || {
        read_file(page, format!("/proc/{pid}/status").as_bytes())
    })??
    .map_err(|_| "a user namespace check's status could not be read")?;
    text.split(|&byte| byte == b'\n')
        .find(|line| line.starts_with(key))
        .map(<[u8]>::to_vec)
        .ok_or("a line was missing from status")
}

/// `getuid()` and `getgid()`.
fn who(process: &Process) -> Result<(usize, usize), &'static str> {
    let uid = call(process, Syscall::Getuid, [0; 6]).map_err(|_| "getuid failed")?;
    let gid = call(process, Syscall::Getgid, [0; 6]).map_err(|_| "getgid failed")?;
    Ok((uid, gid))
}

/// A process that is uid and gid 1000 makes a user namespace and is mapped.
fn user_namespace(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let process = page.process;
    let pid = process.pid();
    tally.ok(
        call(process, Syscall::Setgid, [u64::from(UID), 0, 0, 0, 0, 0]),
        "the user namespace check's process could not become gid 1000",
    )?;
    tally.ok(
        call(process, Syscall::Setuid, [u64::from(UID), 0, 0, 0, 0, 0]),
        "the user namespace check's process could not become uid 1000",
    )?;
    let first = read_link(page, format!("/proc/{pid}/ns/user").as_bytes())?;
    if !first.starts_with(b"user:[") {
        return Err("/proc/<pid>/ns/user did not read user:[...]");
    }
    tally.ok(
        unshare(process, CLONE_NEWUSER),
        "unshare(CLONE_NEWUSER) was refused to uid 1000",
    )?;
    let own = read_link(page, format!("/proc/{pid}/ns/user").as_bytes())?;
    if own == first {
        return Err("a new user namespace was named as the first is");
    }

    // U9: nothing is mapped yet, so every id reads as the overflow id.
    if who(process)? != (65_534, 65_534) {
        return Err("an unmapped namespace did not read its ids as 65534");
    }

    let uid_map = format!("/proc/{pid}/uid_map");
    let gid_map = format!("/proc/{pid}/gid_map");
    let setgroups = format!("/proc/{pid}/setgroups");
    // U4: no group map while `setgroups` still allows dropping groups.
    tally.refused(
        write_to(page, gid_map.as_bytes(), b"0 1000 1\n")?,
        Errno::EPERM,
        "a gid_map was accepted before setgroups was denied (CVE-2014-8989)",
    )?;
    // U2: kernel root is not the writer's to map, and only one id is.
    tally.refused(
        write_to(page, uid_map.as_bytes(), b"0 0 1\n")?,
        Errno::EPERM,
        "an unprivileged writer mapped kernel uid 0",
    )?;
    tally.refused(
        write_to(page, uid_map.as_bytes(), b"0 1000 2\n")?,
        Errno::EPERM,
        "an unprivileged writer mapped two ids",
    )?;
    match write_to(page, uid_map.as_bytes(), b"garbage\n")? {
        Err(Errno::EINVAL) => {}
        Ok(_) => return Err("a malformed uid_map was accepted"),
        Err(Errno::EPERM) => return Err("a malformed uid_map was refused EPERM, not EINVAL"),
        Err(_) => return Err("a malformed uid_map was refused with another errno"),
    }
    tally.ok(
        write_to(page, uid_map.as_bytes(), b"0 1000 1\n")?,
        "the owner's own id could not be mapped",
    )?;
    // U3: the map is written once.
    tally.refused(
        write_to(page, uid_map.as_bytes(), b"0 1000 1\n")?,
        Errno::EPERM,
        "a uid_map was written twice",
    )?;
    tally.ok(
        write_to(page, setgroups.as_bytes(), b"deny\n")?,
        "setgroups could not be denied",
    )?;
    tally.refused(
        write_to(page, setgroups.as_bytes(), b"allow\n")?,
        Errno::EPERM,
        "setgroups was allowed again after deny",
    )?;
    tally.ok(
        write_to(page, gid_map.as_bytes(), b"0 1000 1\n")?,
        "the owner's own group could not be mapped",
    )?;

    // The namespace's own view: root, and a map that reads back as written.
    if who(process)? != (0, 0) {
        return Err("a mapped namespace did not read uid and gid 0");
    }
    let map = acting(pid, || read_file(page, uid_map.as_bytes()))??
        .map_err(|_| "uid_map could not be read")?;
    if map != b"         0       1000          1\n" {
        return Err("uid_map did not read back as written");
    }
    let uid_line = status(page, b"Uid:")?;
    if uid_line != b"Uid:\t0\t0\t0\t0" {
        return Err("status did not show the namespace's own ids");
    }

    unmapped_owner(page, tally)?;
    fake_root(page, tally)?;
    seen_from_outside(pid, &uid_map)
}

/// Fake root -- inside id 0, kernel uid 1000 -- is refused what only root
/// may do, holds every capability in its own namespace, and can drop one.
fn fake_root(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let process = page.process;
    let name = page.put(b"ferrix")?;
    tally.refused(
        call(process, Syscall::Sethostname, [name, 6, 0, 0, 0, 0]),
        Errno::EPERM,
        "fake root in a user namespace set the host name",
    )?;
    let source = page.put(b"none")?;
    let target = page.put(b"/tmp")?;
    let kind = page.put(b"tmpfs")?;
    tally.refused(
        call(process, Syscall::Mount, [source, target, kind, 0, 0, 0]),
        Errno::EPERM,
        "fake root in a user namespace mounted a filesystem",
    )?;
    tally.refused(
        call(process, Syscall::Setuid, [5, 0, 0, 0, 0, 0]),
        Errno::EINVAL,
        "setuid to an id the namespace does not map was not refused EINVAL",
    )?;
    tally.refused(
        call(process, Syscall::Setgroups, [0, 0, 0, 0, 0, 0]),
        Errno::EPERM,
        "setgroups was allowed in a namespace that denied it",
    )?;
    audited_as_the_kernel_knows_it(process)?;
    if status(page, b"CapEff:")? != EVERY_CAPABILITY {
        return Err("the creator of a user namespace did not hold every capability in it");
    }
    tally.ok(
        call(process, Syscall::Prctl, [PR_CAPBSET_READ, 21, 0, 0, 0, 0])
            .and_then(|held| if held == 1 { Ok(1) } else { Err(Errno::EINVAL) }),
        "CAP_SYS_ADMIN was not in the new namespace's bounding set",
    )?;
    tally.ok(
        call(process, Syscall::Prctl, [PR_CAPBSET_DROP, 21, 0, 0, 0, 0]),
        "a capability could not be dropped from the bounding set",
    )?;
    tally.ok(
        call(process, Syscall::Prctl, [PR_CAPBSET_READ, 21, 0, 0, 0, 0])
            .and_then(|held| if held == 0 { Ok(0) } else { Err(Errno::EINVAL) }),
        "PR_CAPBSET_READ did not report a dropped capability",
    )?;

    Ok(())
}

/// What the first namespace sees of the namespace: the kernel's ids in its
/// `status`, and the same map, read from its parent.
fn seen_from_outside(pid: u32, uid_map: &str) -> Result<(), &'static str> {
    // What the first namespace sees of it: the kernel's ids.
    let watcher = process::new_for_check().map_err(|_| "could not make the watching process")?;
    let mut outside = page_for(&watcher)?;
    let watching = watcher.pid();
    let seen = acting(watching, || {
        read_file(&mut outside, format!("/proc/{pid}/status").as_bytes())
    })??
    .map_err(|_| "a namespace's status could not be read from outside")?;
    if !seen.windows(14).any(|w| w == b"Uid:\t1000\t1000") {
        return Err("the first namespace did not see the kernel's ids in a child's status");
    }
    let seen = acting(watching, || read_file(&mut outside, uid_map.as_bytes()))??
        .map_err(|_| "a namespace's uid_map could not be read from outside")?;
    if seen != b"         0       1000          1\n" {
        return Err("a namespace's uid_map did not read the same from its parent");
    }
    Ok(())
}

/// U1 for the case that matters: kernel root, which really is uid 0, inside a
/// namespace it made. Whatever its ids say there it is refused what only the
/// first namespace's root may do, and is not privileged on `Credentials`.
fn root_made(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let root =
        process::new_for_check().map_err(|_| "could not make the root-made namespace's process")?;
    let mut page = page_for(&root)?;
    tally.ok(
        unshare(&root, CLONE_NEWUSER),
        "unshare(CLONE_NEWUSER) was refused to root",
    )?;
    let name = page.put(b"ferrix")?;
    tally.refused(
        call(&root, Syscall::Sethostname, [name, 6, 0, 0, 0, 0]),
        Errno::EPERM,
        "kernel root inside a namespace it made set the host name",
    )?;
    if root.with_credentials(|held| held.privileged()) {
        return Err(
            "kernel root inside a child namespace was privileged in the whole system's sense",
        );
    }
    Ok(())
}

/// U6: a chrooted process may not make a user namespace.
fn chrooted(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let jailed = process::new_for_check().map_err(|_| "could not make the chrooted process")?;
    let mut page = page_for(&jailed)?;
    let at = staged(&mut page, JAIL)?;
    tally.ok(
        call(
            &jailed,
            Syscall::Mkdirat,
            [AT_FDCWD as u64, at, 0o755, 0, 0, 0],
        ),
        "the chroot test's directory could not be made",
    )?;
    let result = (|| {
        let at = staged(&mut page, JAIL)?;
        tally.ok(
            call(&jailed, Syscall::Chroot, [at, 0, 0, 0, 0, 0]),
            "root could not chroot",
        )?;
        tally.refused(
            unshare(&jailed, CLONE_NEWUSER),
            Errno::EPERM,
            "a chrooted process made a user namespace (CVE-2013-1956)",
        )
    })();
    // The jail is removed from outside it: the jailed process's root is it.
    let other = process::new_for_check().map_err(|_| "could not make the clean-up process")?;
    let mut clean = page_for(&other)?;
    let at = staged(&mut clean, JAIL)?;
    let _ = call(
        &other,
        Syscall::Unlinkat,
        [AT_FDCWD as u64, at, u64::from(AT_REMOVEDIR), 0, 0, 0],
    );
    result
}

/// The rules that need a credential no system call builds, proved on
/// `Credentials` itself.
fn credentials(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let mut caller = Credentials::root();
    for ids in [&mut caller.user, &mut caller.group] {
        ids.real = UID;
        ids.effective = UID;
        ids.saved = UID;
        ids.filesystem = UID;
    }
    let root = Credentials::root();

    // U3: a map file opened by a privileged process and written by an
    // unprivileged one. Linux's `f_cred` fix; the writer is judged as well.
    let namespace = userns::create(&caller).map_err(|_| "a user namespace could not be made")?;
    tally.refused(
        userns::write_map(&namespace, Kind::User, &root, &caller, b"0 0 1\n").map(|_| 0),
        Errno::EPERM,
        "a map opened by root was written wider by an unprivileged holder",
    )?;
    // And the control: root writing it itself may map kernel root.
    let mut root_child = root.clone();
    root_child.user_ns = userns::create(&root).map_err(|_| "a root namespace could not be made")?;
    tally.ok(
        userns::write_map(&root_child.user_ns, Kind::User, &root, &root, b"0 0 1\n").map(|_| 0),
        "root could not map kernel root into a namespace it made",
    )?;

    parent_extents(tally, &root)?;

    inside_rules(tally, &caller, namespace)
}

/// A namespace's own owner, and `execve` in it (U1, U7).
fn inside_rules(
    tally: &mut Tally<'_>,
    caller: &Credentials,
    namespace: alloc::sync::Arc<userns::UserNamespace>,
) -> Result<(), &'static str> {
    // A namespace mapped by its owner, and a process in it.
    let mut inside = caller.clone();
    inside.user_ns = namespace;
    inside.caps = userns::CapSets::FRESH;
    tally.ok(
        userns::write_setgroups(&inside.user_ns, &inside, &inside, b"deny\n").map(|_| 0),
        "setgroups could not be denied",
    )?;
    tally.ok(
        userns::write_map(&inside.user_ns, Kind::User, &inside, &inside, b"0 1000 1\n").map(|_| 0),
        "the owner could not map its id",
    )?;
    if inside.privileged() {
        return Err("fake root was privileged in the whole system's sense");
    }
    if !userns::capable_over(&inside, &inside.user_ns, CAP_SYS_ADMIN) {
        return Err("a namespace's creator lacked CAP_SYS_ADMIN over it");
    }
    if userns::capable_over(&inside, userns::first(), CAP_SYS_ADMIN) {
        return Err("a namespace's creator held a capability over the first namespace");
    }
    if !userns::capable_over(caller, &inside.user_ns, CAP_SYS_ADMIN) {
        return Err("the owner outside a namespace lacked CAP_SYS_ADMIN over it");
    }

    // U7: a set-user-id bit gives nothing in a child namespace.
    let mut runs = inside.clone();
    let _ = runs.exec(Some(0), Some(0));
    if runs.user.effective != UID || runs.group.effective != UID {
        return Err("execve of a set-id file changed an id in a child namespace");
    }
    // The sets after `execve`: the namespace's root gets the bounding set, any
    // other nothing -- and its ids say which it is.
    if runs.caps.effective != runs.caps.bounding {
        return Err("execve did not give a namespace's root its bounding set");
    }
    let mut other = inside;
    other.user.effective = UID + 1;
    let _ = other.exec(None, None);
    if other.caps.effective != 0 || other.caps.permitted != 0 {
        return Err("execve gave capabilities to a process that is not its namespace's root");
    }
    Ok(())
}

/// A child's map must lie wholly inside one of its parent's extents.
fn parent_extents(tally: &mut Tally<'_>, root: &Credentials) -> Result<(), &'static str> {
    // A child's run must lie wholly inside one of its parent's extents. The
    // parent here maps `0 1000 10` and `20 1020 10`; a run of 21 ids from 5
    // has both ends mapped at the same offset and ids between that the
    // parent never had, which Linux's `map_id_range_down` refuses.
    let mut parent = root.clone();
    parent.user_ns = userns::create(root).map_err(|_| "a parent namespace could not be made")?;
    parent.caps = userns::CapSets::FRESH;
    for kind in [Kind::User, Kind::Group] {
        tally.ok(
            userns::write_map(
                &parent.user_ns,
                kind,
                root,
                root,
                b"0 1000 10\n20 1020 10\n",
            )
            .map(|_| 0),
            "root could not give a namespace two extents",
        )?;
    }
    parent.user.effective = 1005;
    parent.group.effective = 1005;
    let grandchild =
        userns::create(&parent).map_err(|_| "a namespace inside a mapped one could not be made")?;
    tally.refused(
        userns::write_map(&grandchild, Kind::User, &parent, &parent, b"0 5 21\n").map(|_| 0),
        Errno::EPERM,
        "a run spanning two of the parent's extents, and the gap between them, was accepted",
    )?;
    tally.ok(
        userns::write_map(&grandchild, Kind::User, &parent, &parent, b"0 5 5\n").map(|_| 0),
        "a run inside one of the parent's extents was refused",
    )?;

    Ok(())
}

/// Bubblewrap's read-only `/proc/sys`: bound onto itself and remounted
/// read-only in a namespace of its own, it refuses a write to a sysctl with
/// `EROFS`. Without it a container's root writes the host's sysctls.
fn read_only_sysctls(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let second =
        process::new_for_check().map_err(|_| "could not make the sysctl check's process")?;
    let mut page = page_for(&second)?;
    tally.ok(
        unshare(&second, CLONE_NEWNS),
        "unshare(CLONE_NEWNS) was refused to root",
    )?;
    let (source, target) = (page.put(b"/proc/sys")?, page.put(b"/proc/sys")?);
    tally.ok(
        call(
            &second,
            Syscall::Mount,
            [source, target, 0, u64::from(MS_BIND), 0, 0],
        ),
        "/proc/sys could not be bound onto itself",
    )?;
    let (source, target) = (page.put(b"none")?, page.put(b"/proc/sys")?);
    tally.ok(
        call(
            &second,
            Syscall::Mount,
            [
                source,
                target,
                0,
                u64::from(MS_REMOUNT | MS_BIND | MS_RDONLY),
                0,
                0,
            ],
        ),
        "the bind of /proc/sys could not be remounted read-only",
    )?;
    let outcome = tally.refused(
        write_to(&mut page, b"/proc/sys/kernel/hostname", b"pwned\n")?,
        Errno::EROFS,
        "a write through a read-only bind of /proc/sys was not refused EROFS",
    );
    let at = staged(&mut page, b"/proc/sys")?;
    let _ = call(
        &second,
        Syscall::Umount2,
        [at, u64::from(MNT_DETACH), 0, 0, 0, 0],
    );
    outcome
}

/// A file of kernel uid 2000, which the namespace under test does not map.
const FILE: &[u8] = b"/tmp/.userns-u8-file";
/// A device node the namespace under test may not make.
const DEVICE: &[u8] = b"/tmp/.userns-u8-device";

/// `SIGUSR1`, on every architecture.
const SIGUSR1: u32 = 10;

/// U8: root inside a namespace holds no override over what it does not own.
/// Against a file and a process of kernel uid 2000, which the namespace does
/// not map, its kernel uid of 1000 is refused as any other user would be: the
/// read of a 0600 file, `chmod`, `chown`, `mknod` and `kill`
/// (CVE-2014-4014 was the first of these for a file whose owner was unmapped).
fn unmapped_owner(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let owner = process::new_for_check().map_err(|_| "could not make the file's owner")?;
    let mut owner_page = page_for(&owner)?;
    let at = staged(&mut owner_page, FILE)?;
    tally.ok(
        call(
            &owner,
            Syscall::Mknodat,
            [AT_FDCWD as u64, at, 0o100_600, 0, 0, 0],
        ),
        "the U8 file could not be made",
    )?;
    tally.ok(
        call(
            &owner,
            Syscall::Fchownat,
            [AT_FDCWD as u64, at, 2000, 2000, 0, 0],
        ),
        "the U8 file could not be given to uid 2000",
    )?;
    let victim = process::new_for_check().map_err(|_| "could not make the U8 process")?;
    tally.ok(
        call(&victim, Syscall::Setuid, [2000, 0, 0, 0, 0, 0]),
        "the U8 process could not become uid 2000",
    )?;
    let result = refused_what_it_does_not_own(page, tally, victim.pid())
        .and_then(|()| kernel_root_keeps_override(tally));
    let at = staged(&mut owner_page, FILE)?;
    let _ = call(&owner, Syscall::Unlinkat, [AT_FDCWD as u64, at, 0, 0, 0, 0]);
    let at = staged(&mut owner_page, DEVICE)?;
    let _ = call(&owner, Syscall::Unlinkat, [AT_FDCWD as u64, at, 0, 0, 0, 0]);
    result
}

/// What U8 does not do, recorded as a check: kernel root, which made the
/// namespace, still reads a 0600 file of an id the namespace does not map,
/// because the file system's `Access::privileged` is `uid == 0` and has no
/// namespace in it. Linux refuses it (`capable_wrt_inode_uidgid`); this is a
/// difference listed in `docs/BACKLOG.md`, and the check is there so that
/// closing it has to change this line too. It is not an escape: that process
/// is root of the first namespace already.
fn kernel_root_keeps_override(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let root = process::new_for_check().map_err(|_| "could not make the kernel-root process")?;
    let mut page = page_for(&root)?;
    tally.ok(
        unshare(&root, CLONE_NEWUSER),
        "unshare(CLONE_NEWUSER) was refused to root",
    )?;
    let file = page.put(FILE)?;
    tally.ok(
        call(&root, Syscall::Openat, [AT_FDCWD as u64, file, 0, 0, 0, 0]),
        "kernel root inside a namespace it made no longer reads a 0600 file of an unmapped id",
    )
}

/// The refusals of [`unmapped_owner`], made by the namespace's root.
fn refused_what_it_does_not_own(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    victim: u32,
) -> Result<(), &'static str> {
    let process = page.process;
    page.reset();
    let file = page.put(FILE)?;
    let device = page.put(DEVICE)?;
    let cwd = AT_FDCWD as u64;
    tally.refused(
        call(process, Syscall::Openat, [cwd, file, 0, 0, 0, 0]),
        Errno::EACCES,
        "root inside a namespace read a 0600 file of an id it does not map (CAP_DAC_OVERRIDE)",
    )?;
    tally.refused(
        call(process, Syscall::Fchmodat, [cwd, file, 0o644, 0, 0, 0]),
        Errno::EPERM,
        "root inside a namespace changed the mode of a file of an id it does not map (CAP_FOWNER)",
    )?;
    tally.refused(
        call(process, Syscall::Fchownat, [cwd, file, 0, 0, 0, 0]),
        Errno::EPERM,
        "root inside a namespace changed the owner of a file of an id it does not map (CAP_CHOWN)",
    )?;
    tally.refused(
        call(
            process,
            Syscall::Mknodat,
            [cwd, device, 0o020_600, 0x103, 0, 0],
        ),
        Errno::EPERM,
        "root inside a namespace made a device node (CAP_MKNOD)",
    )?;
    tally.refused(
        call(process, Syscall::Kill, [u64::from(victim), 0, 0, 0, 0, 0]),
        Errno::EPERM,
        "root inside a namespace signalled a process of an id it does not map (CAP_KILL)",
    )
}

/// U5's flag half, on the flags alone: `clone` and `clone3` both ask
/// `namespaces_asked` first, and it refuses `CLONE_NEWUSER` with `CLONE_FS`
/// (CVE-2013-1858) or `CLONE_THREAD`. `unshare` with a shared fs context stands
/// on its code.
fn clone_flags(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{CLONE_FS, CLONE_THREAD};
    let parent =
        process::new_for_check().map_err(|_| "could not make the clone check's process")?;
    let asked = |flags: u64| family::namespaces_asked(&parent, flags).map(|()| 0);
    tally.refused(
        asked(CLONE_NEWUSER | CLONE_FS),
        Errno::EINVAL,
        "CLONE_NEWUSER with CLONE_FS was not refused EINVAL (CVE-2013-1858)",
    )?;
    tally.refused(
        asked(CLONE_NEWUSER | CLONE_THREAD),
        Errno::EINVAL,
        "CLONE_NEWUSER with CLONE_THREAD was not refused EINVAL",
    )?;
    tally.ok(
        asked(CLONE_NEWUSER),
        "CLONE_NEWUSER alone was refused to root",
    )
}

/// `si_uid`: the sender's real uid, told as the receiver's namespace names it.
/// A sender that is uid 1000 reads 1000 to a receiver in the first namespace
/// and 65534 to one in a namespace that maps nothing; `SIGCHLD`'s child the
/// same.
fn sender_ids(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let make = |what: &'static str| process::new_for_check().map_err(|_| what);
    let (sender, same, apart) = (
        make("could not make the signal's sender")?,
        make("could not make a receiver in the first namespace")?,
        make("could not make a receiver in a namespace of its own")?,
    );
    for process in [&sender, &same, &apart] {
        tally.ok(
            call(process, Syscall::Setuid, [u64::from(UID), 0, 0, 0, 0, 0]),
            "a signal check's process could not become uid 1000",
        )?;
    }
    tally.ok(
        unshare(&apart, CLONE_NEWUSER),
        "a receiver could not make a namespace",
    )?;
    // A handler, so that the signal is left pending for the check to read.
    for receiver in [&same, &apart] {
        receiver.with_signals(|signals| signals.install_action(SIGUSR1, 0x1000, 0));
        tally.ok(
            call(
                &sender,
                Syscall::Kill,
                [u64::from(receiver.pid()), u64::from(SIGUSR1), 0, 0, 0, 0],
            ),
            "kill was refused between two processes of one uid",
        )?;
    }
    for (receiver, wanted, what) in [
        (
            &same,
            UID,
            "si_uid did not read the sender's uid in the first namespace",
        ),
        (
            &apart,
            65_534,
            "si_uid did not read 65534 in a namespace that does not map the sender",
        ),
    ] {
        let taken = receiver
            .with_signals(|signals| signal::take_shared(signals, 1 << (SIGUSR1 - 1)))
            .ok_or("a signal sent to a check's process was not left pending")?;
        if seen_uid(receiver.pid(), taken.origin)? != wanted {
            return Err(what);
        }
    }
    let child = Origin::Child {
        code: 1,
        pid: 7,
        uid: UID,
        status: 0,
    };
    if seen_uid(same.pid(), child)? != UID || seen_uid(apart.pid(), child)? != 65_534 {
        return Err("a child's si_uid was not told as the receiver's namespace names it");
    }
    Ok(())
}

/// The `si_uid` `origin` reads as to the process `pid`.
fn seen_uid(pid: u32, origin: Origin) -> Result<u32, &'static str> {
    let info = acting(pid, || origin.encode(SIGUSR1))?;
    let at = if size_of::<usize>() == 8 { 20 } else { 16 };
    info.get(at..at + 4)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_le_bytes)
        .ok_or("a siginfo was too short")
}

/// The audit record's subject (`docs/NAMESPACES.md` §2.5): a decision made
/// by root inside a namespace -- kernel uid 1000 -- names the process and its
/// job as the TSF attests them, and a uid that is the kernel's own or none,
/// never the namespace's 0. No personality supplies a uid to the audit trail
/// yet, so this is the contact point a supplier would have to get right; the
/// day one does, this check is the one that stops it recording the inside id.
fn audited_as_the_kernel_knows_it(process: &Process) -> Result<(), &'static str> {
    let subject = crate::audit::Subject::of(process);
    if subject.pid != process.pid() || subject.job != process.job().id() {
        return Err("an audit subject did not name the caller's process and job");
    }
    if subject.uid != crate::audit::NO_UID && subject.uid != UID {
        return Err(
            "an audit record of root inside a namespace recorded an id that is not the kernel's",
        );
    }
    Ok(())
}

/// Namespaces nested to Linux's limit (`docs/SECCOMP.md` R3): each mapped to
/// root by the one above it, 32 deep, and the 33rd refused `EUSERS`. And what
/// `CLONE_NEWUSER` may answer at all (R5): `EPERM`, `EUSERS`, `EINVAL` or
/// `ENOSPC`, never `ENOSYS`, which Chrome reads as "user namespaces are not
/// built in".
fn nested(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let mut creator = Credentials::root();
    let mut depth = 0;
    let refusal = loop {
        let namespace = match userns::create(&creator) {
            Ok(namespace) => namespace,
            Err(errno) => break errno,
        };
        depth += 1;
        for kind in [Kind::User, Kind::Group] {
            tally.ok(
                userns::write_map(&namespace, kind, &creator, &creator, b"0 0 1\n").map(|_| 0),
                "a nested namespace could not be mapped by the one above it",
            )?;
        }
        creator.user_ns = namespace;
        creator.caps = userns::CapSets::FRESH;
        if depth > 40 {
            return Err("user namespaces nested past Linux's limit of 32");
        }
    };
    if depth != 32 {
        return Err("user namespaces did not nest exactly 32 deep");
    }
    tally.refused(
        Err(refusal),
        Errno::EUSERS,
        "the 33rd nested namespace was not EUSERS",
    )?;
    let allowed = [Errno::EPERM, Errno::EUSERS, Errno::EINVAL, Errno::ENOSPC];
    let parent =
        process::new_for_check().map_err(|_| "could not make the errno check's process")?;
    tally.report.calls += 1;
    // `clone` with a shared fs context, and from a chrooted process (above).
    let answers = [
        family::namespaces_asked(&parent, CLONE_NEWUSER | ferrix_linux_abi::types::CLONE_FS),
        Err(refusal),
    ];
    if answers
        .into_iter()
        .any(|answer| answer.is_err_and(|errno| !allowed.contains(&errno)))
    {
        return Err("CLONE_NEWUSER was refused with an errno Chrome would misread");
    }
    Ok(())
}
