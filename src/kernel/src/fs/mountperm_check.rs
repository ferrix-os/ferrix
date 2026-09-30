//! Who may change mounts, proved at boot (`docs/NAMESPACES.md` M1 to M4, and
//! landing N5): unprivileged mounting from a user namespace, and what it may
//! and may not do.
//!
//! A directory `/tmp/.mp-check/host` holds a tmpfs the first namespace
//! mounted `ro,nosuid,nodev`, and `/tmp/.mp-check/pin` a plain directory. A
//! process of uid 1000 must be:
//!
//! * refused `mount` while it shares the first namespace's mounts (M1), and
//!   also in a user namespace of its own that did not copy them: a capability
//!   in the namespace is not one over the mounts its parent owns;
//! * given a copy of the mounts by `unshare(CLONE_NEWUSER | CLONE_NEWNS)`, and
//!   in it allowed a `tmpfs` that comes out `nosuid,nodev` whatever it asked
//!   (M2), and refused `proc`, `devtmpfs`, `sysfs`, `cgroup2` and `btrfs`;
//! * unable to clear `ro` on the host's tmpfs it was given a copy of, and
//!   free to add to it (M3); unable to unmount it (M4), or to bind `/`
//!   without `MS_REC` and so show what it covers;
//! * unable to `MS_REMOUNT` the host's filesystem as a whole, and able to for
//!   its own tmpfs (N5);
//! * unable to pin a directory of the first namespace against `rmdir` by
//!   mounting over it: the first namespace removes it, and the mount goes.

use alloc::format;

use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_FDCWD, MNT_DETACH, MS_BIND, MS_NODEV, MS_NOSUID, MS_RDONLY, MS_REMOUNT,
};
use ferrix_vfs::Errno;

use crate::fs::mount_check::{Page, Report as Counts, Tally, by_number, page_for};
use crate::fs::namespace_check::{mount, read_file, unmount, unshare};
use crate::syscall::namespace::{CLONE_NEWNS, CLONE_NEWUSER};
use crate::syscall::process::{self, Process};
use crate::syscall::registry;
use crate::syscall::userns;

/// Where the check works, in the first namespace.
const BASE: &[u8] = b"/tmp/.mp-check";
/// The host's tmpfs, `ro,nosuid,nodev`.
const HOST: &[u8] = b"/tmp/.mp-check/host";
/// A directory to pin.
const PIN: &[u8] = b"/tmp/.mp-check/pin";
/// The child's own tmpfs.
const OWN: &[u8] = b"/tmp/.mp-check/own";

/// `mkdirat` with mode 0777, so the unprivileged process may make its own.
fn make_directory(page: &mut Page<'_>, path: &[u8]) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let at = page.put(path)?;
    Ok(by_number(
        page.process,
        Syscall::Mkdirat,
        [AT_FDCWD as u64, at, 0o777, 0, 0, 0],
    ))
}

/// The options `mountinfo` gives the mount at `point`, if it lists one.
fn options_of(reader: &Process, point: &[u8]) -> Result<Option<alloc::vec::Vec<u8>>, &'static str> {
    let mut page = page_for(reader)?;
    let process = registry::find(reader.pid()).ok_or("the mount check's process is gone")?;
    let path = format!("/proc/{}/mountinfo", reader.pid());
    let info = userns::acting_as(&process, || read_file(&mut page, path.as_bytes()))??
        .map_err(|_| "a mount check's mountinfo could not be read")?;
    Ok(info.split(|&byte| byte == b'\n').find_map(|line| {
        let mut fields = line.split(|&byte| byte == b' ');
        (fields.nth(4)? == point).then(|| fields.next().unwrap_or_default().to_vec())
    }))
}

/// What the check saw, for the boot line.
pub(crate) fn run() -> Result<Counts, &'static str> {
    let mut counts = Counts::default();
    let mut tally = Tally {
        report: &mut counts,
    };
    let root = process::new_for_check().map_err(|_| "could not make the mount check's root")?;
    let mut page = page_for(&root)?;
    let outcome = set_up(&mut page, &mut tally).and_then(|()| unprivileged(&mut tally, &mut page));
    clean_up(&mut page);
    outcome.map(|()| counts)
}

/// The host's tmpfs and the directories.
fn set_up(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    tally.ok(
        make_directory(page, BASE)?,
        "the mount check's directory could not be made",
    )?;
    for path in [HOST, PIN, OWN] {
        tally.ok(
            make_directory(page, path)?,
            "a mount check's directory could not be made",
        )?;
    }
    tally.ok(
        mount(
            page,
            b"none",
            HOST,
            b"tmpfs",
            MS_RDONLY | MS_NOSUID | MS_NODEV,
        )?,
        "the host's read-only tmpfs could not be mounted",
    )
}

/// Take down what the check made in the first namespace.
fn clean_up(page: &mut Page<'_>) {
    let _ = unmount(page, HOST, MNT_DETACH);
    for path in [OWN, HOST, PIN, BASE] {
        let _ = unmount(page, path, MNT_DETACH);
        page.reset();
        if let Ok(at) = page.put(path) {
            let _ = by_number(
                page.process,
                Syscall::Unlinkat,
                [AT_FDCWD as u64, at, 0x200, 0, 0, 0],
            );
        }
    }
}

/// uid 1000, in and out of a namespace of its own.
fn unprivileged(tally: &mut Tally<'_>, root_page: &mut Page<'_>) -> Result<(), &'static str> {
    let user = process::new_for_check().map_err(|_| "could not make the unprivileged process")?;
    for call in [Syscall::Setgid, Syscall::Setuid] {
        tally.ok(
            by_number(&user, call, [1000, 0, 0, 0, 0, 0]),
            "the mount check's process could not take uid 1000",
        )?;
    }
    let mut page = page_for(&user)?;
    // M1: sharing the first namespace's mounts, it may not change them.
    tally.refused(
        mount(&mut page, b"none", OWN, b"tmpfs", 0)?,
        Errno::EPERM,
        "uid 1000 mounted a tmpfs in the first namespace",
    )?;
    // A capability in a namespace of its own is not one over its parent's mounts.
    tally.ok(
        unshare(&user, CLONE_NEWUSER),
        "uid 1000 could not make a user namespace",
    )?;
    tally.refused(
        mount(&mut page, b"none", OWN, b"tmpfs", 0)?,
        Errno::EPERM,
        "a user namespace mounted into a mount namespace its parent owns (M1)",
    )?;
    // The mounts copied, it owns them.
    tally.ok(
        unshare(&user, CLONE_NEWNS),
        "CLONE_NEWNS was refused to the owner of a user namespace",
    )?;
    tally.ok(
        mount(&mut page, b"none", OWN, b"tmpfs", 0)?,
        "a tmpfs could not be mounted in a user namespace's own mount namespace",
    )?;
    if options_of(&user, OWN)?.is_none_or(|options| !contains(&options, b"nosuid,nodev")) {
        return Err("a tmpfs mounted from a user namespace was not nosuid,nodev (M2)");
    }
    for kind in [&b"proc"[..], b"devtmpfs", b"sysfs", b"cgroup2", b"btrfs"] {
        tally.refused(
            mount(&mut page, b"none", PIN, kind, 0)?,
            Errno::EPERM,
            "a filesystem of a kind only root may mount was mounted from a user namespace (M2)",
        )?;
    }
    locked(tally, &mut page)?;
    // N5: the filesystem as a whole is the host's; its own tmpfs is its own.
    tally.refused(
        mount(&mut page, b"none", HOST, b"", MS_REMOUNT | MS_RDONLY)?,
        Errno::EPERM,
        "a user namespace remounted the host's filesystem as a whole",
    )?;
    tally.ok(
        mount(&mut page, b"none", OWN, b"", MS_REMOUNT | MS_RDONLY)?,
        "a user namespace could not remount its own tmpfs",
    )?;
    pinning(tally, root_page)
}

/// Whether `haystack` holds `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// M3 and M4: what the host mounted and locked stays as it was.
fn locked(tally: &mut Tally<'_>, page: &mut Page<'_>) -> Result<(), &'static str> {
    let bind = MS_REMOUNT | MS_BIND;
    tally.refused(
        mount(page, b"none", HOST, b"", bind | MS_NOSUID | MS_NODEV)?,
        Errno::EPERM,
        "a user namespace cleared ro on a mount it was given a copy of (CVE-2014-5206)",
    )?;
    tally.refused(
        mount(page, b"none", HOST, b"", bind | MS_RDONLY | MS_NODEV)?,
        Errno::EPERM,
        "a user namespace cleared nosuid on a mount it was given a copy of",
    )?;
    tally.ok(
        mount(
            page,
            b"none",
            HOST,
            b"",
            bind | MS_RDONLY | MS_NOSUID | MS_NODEV,
        )?,
        "a user namespace could not keep the flags of a mount it was given a copy of",
    )?;
    tally.refused(
        unmount(page, HOST, 0)?,
        Errno::EINVAL,
        "a user namespace unmounted a mount locked to its parent (M4)",
    )?;
    tally.refused(
        unmount(page, HOST, MNT_DETACH)?,
        Errno::EINVAL,
        "a user namespace detached a mount locked to its parent (M4)",
    )?;
    // A bind of `/` that leaves the locked mounts out would show what they cover.
    page.reset();
    let (source, target) = (page.put(b"/")?, page.put(PIN)?);
    tally.refused(
        by_number(
            page.process,
            Syscall::Mount,
            [source, target, 0, u64::from(MS_BIND), 0, 0],
        ),
        Errno::EINVAL,
        "a user namespace bound / without MS_REC over mounts locked to their parents (M4)",
    )
}

/// A mount over a directory of the first namespace does not pin it: `OWN` is
/// a mount point in the child's namespace and not in the first, which removes
/// the directory all the same, and the mount goes with it.
fn pinning(tally: &mut Tally<'_>, root_page: &mut Page<'_>) -> Result<(), &'static str> {
    root_page.reset();
    let at = root_page.put(OWN)?;
    tally.ok(
        by_number(
            root_page.process,
            Syscall::Unlinkat,
            [AT_FDCWD as u64, at, 0x200, 0, 0, 0],
        ),
        "a mount in another namespace pinned a directory against its owner",
    )
}
