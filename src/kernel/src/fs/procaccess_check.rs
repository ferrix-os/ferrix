//! What a process keeps private in `/proc`, proved at boot
//! (`docs/NAMESPACES.md` M8, landing NP): `/proc/<pid>/root`, `cwd`, `exe`,
//! `fd`, `fdinfo`, `maps` and `ns/*` of another process are refused
//! `EACCES` unless Linux's `ptrace_may_access` allows them.
//!
//! Four targets -- a dumpable process of uid 1000, one of uid 1000 that cleared
//! `PR_SET_DUMPABLE`, one of uid 2000 and one of root -- and three readers:
//! uid 1000, root, and root inside a user namespace (kernel uid 1000). The
//! check requires:
//!
//! * the dumpable same-uid target is readable by uid 1000, and every other
//!   target is refused it: the non-dumpable one (a set-id `execve` makes a
//!   process one) and the other uids' processes;
//! * root reads every one (`CAP_SYS_PTRACE`);
//! * root inside a user namespace reads what its kernel uid may and nothing of
//!   another uid's, however the ids read inside, and is refused the `fd`
//!   directory and `maps` of root's process;
//! * a process that cleared `PR_SET_DUMPABLE` and made a user namespace is
//!   refused to a process of its own user, whose capability over the namespace
//!   does not reach a target that is not dumpable;
//! * a child findable before it has its attributes reads as not dumpable;
//! * a caller whose filesystem id is the target's reads it through `/proc`,
//!   and one whose real id only is, does not;
//! * every entry is looked at -- links, listings, files, and the entries
//!   under `fd` and `fdinfo` -- and an allowed look must answer, not merely
//!   fail with something else;
//! * `get_robust_list` asks the same of a thread that is not the caller's
//!   own, with the real ids, through the system call.

use alloc::format;
use alloc::sync::Arc;

use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{AT_FDCWD, O_DIRECTORY, O_RDONLY};
use ferrix_vfs::Errno;

use crate::fs::mount_check::{Report as Counts, Tally, by_number, page_for};
use crate::fs::namespace_check::{staged, unshare};
use crate::syscall::namespace::CLONE_NEWUSER;
use crate::syscall::process::{self, Process};
use crate::syscall::registry;
use crate::syscall::thread::Thread;
use crate::syscall::userns;

/// `PR_SET_DUMPABLE`.
const PR_SET_DUMPABLE: u64 = 4;

/// A target made with ids `uid`, and `PR_SET_DUMPABLE` set as `undumpable`
/// says: a change of ids makes a process undumpable, so it is set again, as
/// bubblewrap does after it drops its ids.
fn target(uid: u64, undumpable: bool) -> Result<Arc<Process>, &'static str> {
    let made = dropped(uid)?;
    let _ = by_number(
        &made,
        Syscall::Prctl,
        [PR_SET_DUMPABLE, u64::from(!undumpable), 0, 0, 0, 0],
    )
    .map_err(|_| "a /proc target could not set PR_SET_DUMPABLE")?;
    Ok(made)
}

/// A target that changed its ids and did nothing more.
fn dropped(uid: u64) -> Result<Arc<Process>, &'static str> {
    let made = process::new_for_check().map_err(|_| "could not make a /proc target")?;
    if uid != 0 {
        for call in [Syscall::Setgid, Syscall::Setuid] {
            let _ = by_number(&made, call, [uid, 0, 0, 0, 0, 0])
                .map_err(|_| "a /proc target could not take its ids")?;
        }
    }
    // A descriptor to look at.
    let mut page = page_for(&made)?;
    let _ = crate::fs::mount_check::open(&mut page, b"/", O_RDONLY, 0)?
        .map_err(|_| "a /proc target could not open /")?;
    Ok(made)
}

/// How a reader looks at one of a process's entries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `readlink`.
    Link,
    /// An open of a directory, and one `getdents64` of it: listing is what
    /// `ptrace_may_access` guards.
    Listing,
    /// An open and one read.
    File,
    /// An open of a link, which follows it: a namespace link opens the
    /// namespace as a file, which its own rule guards, not `readlink`'s.
    Opened,
}

/// What `reader` is told of `path`: the refusal, or how much it got.
fn ask(reader: &Process, path: &str, kind: Kind) -> Result<Result<usize, Errno>, &'static str> {
    let mut page = page_for(reader)?;
    let process = registry::find(reader.pid()).ok_or("the /proc reader is gone")?;
    userns::acting_as(&process, || match kind {
        Kind::File => crate::fs::namespace_check::read_file(&mut page, path.as_bytes())
            .map(|got| got.map(|bytes| bytes.len())),
        Kind::Link => {
            let at = staged(&mut page, path.as_bytes())?;
            Ok(by_number(
                reader,
                Syscall::Readlinkat,
                [AT_FDCWD as u64, at, page.buffer(), 256, 0, 0],
            ))
        }
        Kind::Opened => {
            let at = staged(&mut page, path.as_bytes())?;
            let opened = by_number(
                reader,
                Syscall::Openat,
                [AT_FDCWD as u64, at, u64::from(O_RDONLY), 0, 0, 0],
            );
            Ok(opened.map(|fd| {
                let _ = by_number(reader, Syscall::Close, [fd as u64, 0, 0, 0, 0, 0]);
                0
            }))
        }
        Kind::Listing => {
            let at = staged(&mut page, path.as_bytes())?;
            let opened = by_number(
                reader,
                Syscall::Openat,
                [
                    AT_FDCWD as u64,
                    at,
                    u64::from(O_RDONLY | O_DIRECTORY),
                    0,
                    0,
                    0,
                ],
            );
            Ok(opened.and_then(|fd| {
                let listed = by_number(
                    reader,
                    Syscall::Getdents64,
                    [fd as u64, page.buffer(), 512, 0, 0, 0],
                );
                let _ = by_number(reader, Syscall::Close, [fd as u64, 0, 0, 0, 0, 0]);
                listed
            }))
        }
    })?
}

/// The lowest descriptor `target` holds: `target` opened `/` when it was made.
fn descriptor_of(target: &Process) -> usize {
    (0..64_i32)
        .find(|&fd| target.files().lock().get(fd).is_ok())
        .map_or(0, |fd| usize::try_from(fd).unwrap_or(0))
}

/// What a reader that may not look at `target` is refused, each entry a name
/// under `/proc/<pid>` and how it is looked at.
fn guarded(target: &Process) -> alloc::vec::Vec<(alloc::string::String, Kind)> {
    let fd = descriptor_of(target);
    let mut entries: alloc::vec::Vec<(alloc::string::String, Kind)> = [
        ("root", Kind::Link),
        ("cwd", Kind::Link),
        ("exe", Kind::Link),
        ("fd", Kind::Listing),
        ("fdinfo", Kind::Listing),
        ("ns/mnt", Kind::Link),
        ("ns/user", Kind::Link),
        ("ns/uts", Kind::Link),
        ("ns/ipc", Kind::Link),
        ("ns/cgroup", Kind::Link),
        ("ns/pid", Kind::Link),
        ("ns/pid_for_children", Kind::Link),
        ("ns/net", Kind::Link),
        // Followed: the network namespace's own open (`net::netns_file`).
        ("ns/net", Kind::Opened),
        ("maps", Kind::File),
    ]
    .into_iter()
    .map(|(name, kind)| (alloc::string::String::from(name), kind))
    .collect();
    entries.push((format!("fd/{fd}"), Kind::Link));
    entries.push((format!("fdinfo/{fd}"), Kind::File));
    entries
}

/// A message built at the moment of failure, which stops the boot.
fn message(text: alloc::string::String) -> &'static str {
    alloc::boxed::Box::leak(text.into_boxed_str())
}

/// What the check saw, for the boot line.
pub(crate) fn run() -> Result<Counts, &'static str> {
    let mut counts = Counts::default();
    let mut tally = Tally {
        report: &mut counts,
    };
    let dumpable = target(1000, false)?;
    let private = target(1000, true)?;
    let other = target(2000, false)?;
    let root = target(0, false)?;

    let user = target(1000, false)?;
    readers(&mut tally, &user, &dumpable, &private, &other, &root)?;

    let administrator = process::new_for_check().map_err(|_| "could not make the root reader")?;
    for victim in [&dumpable, &private, &other, &root] {
        allowed(
            &mut tally,
            &administrator,
            victim,
            "root was refused another process's /proc",
        )?;
    }

    // Root inside a namespace it made: kernel uid 1000 again.
    let fake = target(1000, false)?;
    tally.ok(
        unshare(&fake, CLONE_NEWUSER),
        "unshare(CLONE_NEWUSER) was refused to uid 1000",
    )?;
    for victim in [&other, &root] {
        refused(
            &mut tally,
            &fake,
            victim,
            "root inside a namespace read another uid's /proc",
        )?;
    }
    allowed(
        &mut tally,
        &fake,
        &dumpable,
        "root inside a namespace was refused a same-uid process",
    )?;

    // Chrome's and bubblewrap's pattern: a process clears `PR_SET_DUMPABLE`
    // and makes a user namespace. Its owner from outside is every other
    // process of its user, and none of them may read it: the capability over
    // the namespace is not the capability over a process that is not dumpable.
    let sealed = target(1000, true)?;
    tally.ok(
        unshare(&sealed, CLONE_NEWUSER),
        "unshare(CLONE_NEWUSER) was refused to a process that is not dumpable",
    )?;
    refused(
        &mut tally,
        &user,
        &sealed,
        "a process that cleared PR_SET_DUMPABLE and made a user namespace",
    )?;

    // A caller whose real and filesystem ids differ: `/proc` asks the
    // filesystem ids, `get_robust_list` the real ones.
    let split = process::new_for_check().map_err(|_| "could not make the split reader")?;
    for call in [Syscall::Setresgid, Syscall::Setresuid] {
        tally.ok(
            by_number(&split, call, [1000, 2000, 2000, 0, 0, 0]),
            "the split reader could not take its ids",
        )?;
    }
    allowed(
        &mut tally,
        &split,
        &other,
        "a caller whose filesystem id is the target's was refused",
    )?;
    refused(
        &mut tally,
        &split,
        &dumpable,
        "a caller whose real id only is the target's read its /proc",
    )?;
    robust_lists(&mut tally, &user, &split, &dumpable, &other)?;
    fdinfo(&mut tally)?;
    Ok(counts)
}

/// uid 1000 is refused every other kind of target, and reads the dumpable one.
fn readers(
    tally: &mut Tally<'_>,
    user: &Process,
    dumpable: &Process,
    private: &Process,
    other: &Process,
    root: &Process,
) -> Result<(), &'static str> {
    // Whatever the dumpable flag says, another uid's and root's are refused.
    refused(tally, user, other, "a user read another uid's process")?;
    refused(tally, user, root, "a user read root's process")?;
    refused(
        tally,
        user,
        private,
        "a user read a process that is not dumpable",
    )?;
    // A process that changed its ids is not dumpable until it says so.
    let changed = dropped(1000)?;
    refused(
        tally,
        user,
        &changed,
        "a process that changed its ids stayed dumpable",
    )?;
    // A child findable before its attributes are given is not dumpable yet:
    // the window refuses, and the same process settled is read.
    let newborn = target(1000, false)?;
    newborn.await_attributes();
    refused(
        tally,
        user,
        &newborn,
        "a process read before its attributes were given was dumpable",
    )?;
    newborn.settle_attributes();
    allowed(
        tally,
        user,
        &newborn,
        "a process whose attributes were given was still refused",
    )?;
    allowed(
        tally,
        user,
        dumpable,
        "a user was refused a dumpable process of its own",
    )?;
    // Its own, always.
    allowed(tally, user, user, "a process was refused its own /proc")
}

/// `reader` may look at `target`: every entry answers, none is refused.
fn allowed(
    tally: &mut Tally<'_>,
    reader: &Process,
    target: &Process,
    what: &'static str,
) -> Result<(), &'static str> {
    for (name, kind) in guarded(target) {
        // A check's process has no program file to lead to.
        if name == "exe" {
            continue;
        }
        let path = format!("/proc/{}/{name}", target.pid());
        tally.report.calls += 1;
        if let Err(errno) = ask(reader, &path, kind)? {
            return Err(message(format!(
                "{what}: /proc/<pid>/{name} answered {errno:?}"
            )));
        }
    }
    Ok(())
}

/// `reader` is refused every one of them, `EACCES`, and told so by the entry
/// and the target's kind, so that a failure names the rule that broke.
fn refused(
    tally: &mut Tally<'_>,
    reader: &Process,
    target: &Process,
    what: &'static str,
) -> Result<(), &'static str> {
    for (name, kind) in guarded(target) {
        let path = format!("/proc/{}/{name}", target.pid());
        let got = ask(reader, &path, kind)?;
        tally.report.calls += 1;
        if got != Err(Errno::EACCES) {
            return Err(message(format!(
                "{what}: /proc/<pid>/{name} was not refused"
            )));
        }
        tally.report.refusals += 1;
    }
    Ok(())
}

/// `get_robust_list`'s permission, through the system call: a thread of
/// another uid's process is `EPERM`, a same-uid dumpable one is answered, so
/// is the caller's own, and the caller's real ids are what is asked.
fn robust_lists(
    tally: &mut Tally<'_>,
    user: &Arc<Process>,
    split: &Arc<Process>,
    dumpable: &Arc<Process>,
    other: &Arc<Process>,
) -> Result<(), &'static str> {
    // The processes a check makes have no thread listed; each needs one to be asked about.
    let mut threads = alloc::vec::Vec::new();
    for process in [user, split, dumpable, other] {
        let thread = Thread::leader(process)
            .and_then(crate::fallible::try_arc)
            .map_err(|_| "no memory for a check's thread")?;
        process.add_thread(&thread);
        threads.push(thread);
    }
    let ask = |reader: &Arc<Process>, target: &Process| {
        let page = page_for(reader)?;
        let (head, size) = (page.buffer(), page.buffer() + 16);
        Ok::<_, &'static str>(by_number(
            reader,
            Syscall::GetRobustList,
            [u64::from(target.pid()), head, size, 0, 0, 0],
        ))
    };
    tally.ok(
        ask(user, dumpable)?,
        "get_robust_list was refused a thread of a same-uid dumpable process",
    )?;
    tally.ok(
        ask(user, user)?,
        "get_robust_list was refused the caller's own thread",
    )?;
    tally.refused(
        ask(user, other)?,
        Errno::EPERM,
        "get_robust_list was allowed a thread of another uid's process",
    )?;
    // The real ids, not the filesystem ones: this caller's filesystem id is
    // `other`'s and its real id is `dumpable`'s.
    tally.refused(
        ask(split, other)?,
        Errno::EPERM,
        "get_robust_list took the filesystem ids and not the real ones",
    )?;
    tally.ok(
        ask(split, dumpable)?,
        "get_robust_list refused a caller whose real id is the target's",
    )
}

/// `/proc/<pid>/fdinfo` (`docs/SECCOMP.md` R4): a file per open descriptor with
/// Linux's first lines, and the jail Chrome's zygote makes of it -- `chroot`
/// into a child's `fdinfo`, the child ends, and `/` must still be statted and
/// listed, empty, not a fatal error.
fn fdinfo(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let jailed =
        process::new_for_check().map_err(|_| "could not make the fdinfo check's process")?;
    let mut page = page_for(&jailed)?;
    let child = process::new_for_check().map_err(|_| "could not make the fdinfo child")?;
    // The file: a descriptor the process holds reads back its mode and inode.
    let own = format!("/proc/{}/fdinfo", jailed.pid());
    let fd = {
        let path = staged(&mut page, b"/")?;
        by_number(
            &jailed,
            Syscall::Openat,
            [
                AT_FDCWD as u64,
                path,
                u64::from(O_RDONLY | O_DIRECTORY),
                0,
                0,
                0,
            ],
        )
        .map_err(|_| "the fdinfo check could not open /")?
    };
    let text = crate::fs::namespace_check::read_file(&mut page, format!("{own}/{fd}").as_bytes())?
        .map_err(|_| "a descriptor's fdinfo could not be read")?;
    for key in [&b"pos:\t0"[..], b"flags:\t0", b"mnt_id:\t", b"ino:\t"] {
        if !text.windows(key.len()).any(|window| window == key) {
            return Err("fdinfo did not hold pos, flags, mnt_id and ino");
        }
    }
    tally.report.calls += 1;

    // The jail.
    let path = format!("/proc/{}/fdinfo", child.pid());
    let at = staged(&mut page, path.as_bytes())?;
    tally.ok(
        by_number(&jailed, Syscall::Chroot, [at, 0, 0, 0, 0, 0]),
        "a process could not chroot into a child's fdinfo",
    )?;
    process::kill(&child, 137);
    drop(child);
    let root = staged(&mut page, b"/")?;
    // `newfstatat`, or `fstatat64` where that is the only form (ARMv7-A):
    // a call this architecture has no number for answers `ENOSYS`.
    let stat = |call| {
        by_number(
            &jailed,
            call,
            [AT_FDCWD as u64, root, page.buffer(), 0, 0, 0],
        )
    };
    let statted = match stat(Syscall::Newfstatat) {
        Err(Errno::ENOSYS) => stat(Syscall::Fstatat64),
        other => other,
    };
    tally.ok(
        statted,
        "/ could not be statted in a jail whose process ended",
    )?;
    let listing = by_number(
        &jailed,
        Syscall::Openat,
        [
            AT_FDCWD as u64,
            root,
            u64::from(O_RDONLY | O_DIRECTORY),
            0,
            0,
            0,
        ],
    );
    let listed = listing.and_then(|fd| {
        by_number(
            &jailed,
            Syscall::Getdents64,
            [fd as u64, page.buffer(), 512, 0, 0, 0],
        )
    });
    // Empty: only `.` and `..`, or nothing; never an error.
    let Ok(len) = listed else {
        return Err("/ could not be listed in a jail whose process ended");
    };
    let entries = page.read_back(len)?;
    let mut at = 0;
    while at + 19 <= entries.len() {
        let reclen = entries
            .get(at + 16..at + 18)
            .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
            .map_or(0, |bytes| usize::from(u16::from_le_bytes(bytes)));
        let name = entries
            .get(at + 19..)
            .map(|rest| rest.split(|&byte| byte == 0).next().unwrap_or_default())
            .unwrap_or_default();
        if reclen == 0 || (name != b"." && name != b"..") {
            return Err("a jail whose process ended listed something under /");
        }
        at += reclen;
    }
    Ok(())
}
