//! Mount namespaces, proved at boot (`docs/NAMESPACES.md`, landing N3):
//! `unshare(CLONE_NEWNS)`, bubblewrap's sequence as root to its last
//! `pivot_root(".", ".")`, and the two places a namespace meets the rest of
//! the kernel, driven by number as a program's calls would be.
//!
//! Two processes of the check's own: the first stays in the first
//! namespace, the second makes a copy of it. The check requires:
//!
//! * `openat2`'s resolve flags, which bubblewrap from 0.12 opens every place
//!   it binds through: from a descriptor of the check's directory, an
//!   absolute link out of it stays inside with `RESOLVE_IN_ROOT`, is `ELOOP`
//!   with `RESOLVE_NO_SYMLINKS`, `..` is `EXDEV` with `RESOLVE_BENEATH`, a
//!   descriptor's `/proc` link is `ELOOP` with `RESOLVE_NO_MAGICLINKS`, and
//!   the size of `struct open_how` is judged as Linux judges it;
//! * the second's copy is private both ways: a tmpfs each mounts after the
//!   copy is in its own `mountinfo` and not in the other's, and
//!   `/proc/<pid>/ns/mnt` names the same namespace before the `unshare` and
//!   different ones after; `unshare(CLONE_NEWNS)` by uid 1000 is `EPERM`, and
//!   a namespace Ferrix does not have is still `EINVAL`;
//! * bubblewrap's calls as root, in its order, from the namespace's own `/`
//!   -- the kernel's tmpfs, in memory, on the bottom mount -- as on a machine
//!   with no disk: a tmpfs for the new root,
//!   `newroot` bound onto itself, `pivot_root(base, "oldroot")`, binds into
//!   `newroot` from the old root -- a file, and with a data disk a btrfs --
//!   and a `proc` and a tmpfs of its own, the old root detached, then
//!   `pivot_root(".", ".")` and `umount2(".", MNT_DETACH)`. After it `/` is
//!   the new tree, `..` from it stays, nothing of the old tree is reachable
//!   by any path, and `mountinfo` lists the new tree's mounts alone;
//! * a native child the second makes is in its namespace, root and working
//!   directory, not the first's (§2.5): it finds the file bound into the new
//!   root and not one the first namespace alone has, and its
//!   `/proc/<pid>/ns/mnt` is its creator's;
//! * with the stage 12 disk mounted at `/mnt-rw`, a file written into the
//!   btrfs inside the old root just before that root is detached is on the
//!   disk after it: a second, read-only mount of the disk, which reads
//!   nothing but what was committed, finds it (the N2 review's condition
//!   for N3, `docs/NAMESPACES.md` §12).

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_elf::Class;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_FDCWD, AT_REMOVEDIR, MNT_DETACH, MS_BIND, MS_NODEV, MS_NOSUID, MS_REC, O_CREAT, O_PATH,
    O_RDONLY, O_TRUNC, O_WRONLY,
};
use ferrix_vfs::Errno;
use ferrix_vfs::initramfs::makedev;

use crate::arch;
use crate::fs;
use crate::fs::mount_check::{
    Page, READ_ROOM, Report as Counts, Tally, by_number, close, open, page_for,
};
use crate::interfaces::block_ring::VIRTIO_BLK_MAJOR;
use crate::object::process::{self as core_process, Host};
use crate::syscall::image;
use crate::syscall::launch;
use crate::syscall::namespace::CLONE_NEWNS;
use crate::syscall::process::{self, Process};

/// Where the check works, in the first namespace.
const BASE: &[u8] = b"/tmp/.ns-check";

/// A file the first namespace alone has: outside [`BASE`], so nothing the
/// second process's root reaches after its `chroot`.
const FIRST_ONLY: &[u8] = b"/tmp/.ns-first-only";

/// What [`BASE`]`/old-only` holds, bound into the new root as `/file`.
const CONTENT: &[u8] = b"namespace";

/// What the btrfs file written before the detach holds.
const COMMITTED: &[u8] = b"written inside a subtree just before MNT_DETACH";

/// Its name, at the top of the stage 12 volume.
const DETACH_FILE: &[u8] = b"ns-check-detach";

/// The stage 12 disk, `vdc`, which `btrfs_write_check` leaves mounted at
/// `/mnt-rw`.
const DISK_INDEX: u32 = 2;

/// `CLONE_NEWNET`: a namespace Ferrix does not have.
const CLONE_NEWNET: u64 = 0x4000_0000;

/// What the check saw, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Calls answered as Linux answers them, and the refusals among them.
    pub(crate) counts: Counts,
    /// Whether the detach was shown to commit a btrfs inside the subtree;
    /// not, on a machine without the stage 12 disk.
    pub(crate) committed: bool,
}

/// `path` with its NUL, staged on a fresh page.
pub(super) fn staged(page: &mut Page<'_>, path: &[u8]) -> Result<u64, &'static str> {
    page.reset();
    page.put(path)
}

/// A call taking one path and three more arguments.
fn with_path(
    page: &mut Page<'_>,
    call: Syscall,
    path: &[u8],
    rest: [u64; 3],
) -> Result<Result<usize, Errno>, &'static str> {
    let at = staged(page, path)?;
    Ok(by_number(
        page.process,
        call,
        [at, rest[0], rest[1], rest[2], 0, 0],
    ))
}

/// `mkdirat(AT_FDCWD, path, 0755)`.
fn mkdir(page: &mut Page<'_>, path: &[u8]) -> Result<Result<usize, Errno>, &'static str> {
    let at = staged(page, path)?;
    Ok(by_number(
        page.process,
        Syscall::Mkdirat,
        [AT_FDCWD as u64, at, 0o755, 0, 0, 0],
    ))
}

/// `mount(source, target, type, flags, NULL)`.
fn mount(
    page: &mut Page<'_>,
    source: &[u8],
    target: &[u8],
    kind: &[u8],
    flags: u32,
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let source = page.put(source)?;
    let target = page.put(target)?;
    let kind = page.put(kind)?;
    Ok(by_number(
        page.process,
        Syscall::Mount,
        [source, target, kind, u64::from(flags), 0, 0],
    ))
}

/// `umount2(target, flags)`.
fn unmount(
    page: &mut Page<'_>,
    target: &[u8],
    flags: u32,
) -> Result<Result<usize, Errno>, &'static str> {
    with_path(page, Syscall::Umount2, target, [u64::from(flags), 0, 0])
}

/// `pivot_root(new_root, put_old)`.
fn pivot_root(
    page: &mut Page<'_>,
    new_root: &[u8],
    put_old: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let new_root = page.put(new_root)?;
    let put_old = page.put(put_old)?;
    Ok(by_number(
        page.process,
        Syscall::PivotRoot,
        [new_root, put_old, 0, 0, 0, 0],
    ))
}

/// `unshare(flags)`.
pub(super) fn unshare(process: &Process, flags: u64) -> Result<usize, Errno> {
    by_number(process, Syscall::Unshare, [flags, 0, 0, 0, 0, 0])
}

/// Write `data` to a new file at `path`, as `>` would.
fn write_file(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    path: &[u8],
    data: &[u8],
    what: &'static str,
) -> Result<(), &'static str> {
    page.reset();
    let fd = open(page, path, O_CREAT | O_WRONLY | O_TRUNC, 0o644)?;
    let fd = tally.done(fd, what)?;
    let at = page.put_bytes(data);
    let written = at.map(|at| {
        by_number(
            page.process,
            Syscall::Write,
            [fd as u64, at, data.len() as u64, 0, 0, 0],
        )
    });
    close(page.process, fd);
    if tally.done(written?, what)? != data.len() {
        return Err(what);
    }
    Ok(())
}

/// The file at `path`, up to [`READ_ROOM`] bytes of it, or why it would not
/// open.
pub(super) fn read_file(
    page: &mut Page<'_>,
    path: &[u8],
) -> Result<Result<Vec<u8>, Errno>, &'static str> {
    page.reset();
    let fd = match open(page, path, O_RDONLY, 0)? {
        Ok(fd) => fd,
        Err(errno) => return Ok(Err(errno)),
    };
    let read = by_number(
        page.process,
        Syscall::Read,
        [fd as u64, page.buffer(), READ_ROOM, 0, 0, 0],
    );
    close(page.process, fd);
    match read {
        Ok(len) => page.read_back(len).map(Ok),
        Err(errno) => Ok(Err(errno)),
    }
}

/// What the link at `path` reads.
pub(super) fn read_link(page: &mut Page<'_>, path: &[u8]) -> Result<Vec<u8>, &'static str> {
    let at = staged(page, path)?;
    let len = by_number(
        page.process,
        Syscall::Readlinkat,
        [AT_FDCWD as u64, at, page.buffer(), READ_ROOM, 0, 0],
    )
    .map_err(|_| "a namespace check's link could not be read")?;
    page.read_back(len)
}

/// `/proc/<pid>/ns/mnt` of `pid`, read by the page's process.
fn namespace_of(page: &mut Page<'_>, pid: u32) -> Result<Vec<u8>, &'static str> {
    read_link(page, format!("/proc/{pid}/ns/mnt").as_bytes())
}

/// `/proc/<pid>/mountinfo` of the page's own process.
fn own_mountinfo(page: &mut Page<'_>) -> Result<Vec<u8>, &'static str> {
    let pid = page.process.pid();
    read_file(page, format!("/proc/{pid}/mountinfo").as_bytes())?
        .map_err(|_| "a namespace check's mountinfo could not be read")
}

/// Whether `haystack` holds `needle`.
fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Whether a `mountinfo` names `point` as a mount point: its fifth field.
fn lists(mountinfo: &[u8], point: &[u8]) -> bool {
    mountinfo.split(|&byte| byte == b'\n').any(|line| {
        line.split(|&byte| byte == b' ')
            .nth(4)
            .is_some_and(|field| field == point)
    })
}

/// A path under [`BASE`] as it is reached through bubblewrap's `oldroot`
/// after its first `pivot_root`, which puts the old `/` there.
fn old(name: &[u8]) -> Vec<u8> {
    let mut path = b"/oldroot".to_vec();
    path.extend_from_slice(&under(name));
    path
}

/// A path under [`BASE`].
fn under(name: &[u8]) -> Vec<u8> {
    let mut path = BASE.to_vec();
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

/// Run the check. `disk` says the stage 12 check left its volume at
/// `/mnt-rw`, for the detach's write-out to be shown on.
///
/// # Errors
///
/// What did not hold, as a sentence.
pub(crate) fn run(disk: bool) -> Result<Report, &'static str> {
    let first =
        process::new_for_check().map_err(|_| "could not make a process for the namespace check")?;
    let mut page = page_for(&first)?;
    let mut report = Report::default();
    let outcome = check(&mut page, &mut report, disk);
    clean_up(&mut page);
    outcome.map(|()| report)
}

/// Take down what the check made in the first namespace. The second
/// process, and its namespace with it, has ended by now, so nothing of that
/// namespace still covers a place here.
fn clean_up(page: &mut Page<'_>) {
    for _ in 0..3 {
        let _ = unmount(page, &under(b"m2"), MNT_DETACH);
        let _ = unmount(page, &under(b"disk"), MNT_DETACH);
        let _ = unmount(page, BASE, MNT_DETACH);
    }
    let mut on_disk = b"/mnt-rw/".to_vec();
    on_disk.extend_from_slice(DETACH_FILE);
    for (path, flags) in [(&on_disk[..], 0), (FIRST_ONLY, 0), (BASE, AT_REMOVEDIR)] {
        if let Ok(at) = staged(page, path) {
            let _ = by_number(
                page.process,
                Syscall::Unlinkat,
                [AT_FDCWD as u64, at, u64::from(flags), 0, 0, 0],
            );
        }
    }
    // The stage 12 volume has no committer of its own: its file's removal
    // goes to the disk now, so the image `test-btrfs` judges is as the
    // stage 12 check left it.
    let _ = by_number(page.process, Syscall::Sync, [0; 6]);
}

/// The check proper; [`run`] cleans up after it either way.
fn check(page: &mut Page<'_>, report: &mut Report, disk: bool) -> Result<(), &'static str> {
    let Report { counts, committed } = report;
    let mut tally = Tally { report: counts };
    set_up(page, &mut tally, disk)?;
    openat2_resolves(page, &mut tally)?;
    let second = process::new_for_check()
        .map_err(|_| "could not make the namespace check's second process")?;
    let mut inside = page_for(&second)?;
    private_both_ways(page, &mut inside, &mut tally)?;
    bubblewrap_as_root(&mut inside, &mut tally, disk)?;
    if disk {
        *committed = on_the_disk()?;
        if !*committed {
            return Err(
                "a btrfs file written inside a subtree just before MNT_DETACH was \
                        not on the disk after it",
            );
        }
    }
    native_child_stays(&mut inside, &mut tally, &second)?;
    Ok(())
}

/// `openat2(dirfd, path, how, size)` with `how` as `flags`, `mode` and
/// `resolve`, and `tail` more bytes after them.
fn openat2(
    page: &mut Page<'_>,
    dirfd: usize,
    path: &[u8],
    resolve: u64,
    size: u64,
    tail: &[u8],
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let path = page.put(path)?;
    let mut how = Vec::new();
    how.extend_from_slice(&u64::from(O_RDONLY).to_ne_bytes());
    how.extend_from_slice(&0_u64.to_ne_bytes());
    how.extend_from_slice(&resolve.to_ne_bytes());
    how.extend_from_slice(tail);
    let how = page.put_bytes(&how)?;
    Ok(by_number(
        page.process,
        Syscall::Openat2,
        [dirfd as u64, path, how, size, 0, 0],
    ))
}

/// `openat2`'s resolve flags, which bubblewrap from 0.12 opens every place
/// it binds through, and its size rules: from a descriptor of [`BASE`], a
/// link to [`FIRST_ONLY`] stays inside with `RESOLVE_IN_ROOT`, is `ELOOP`
/// with `RESOLVE_NO_SYMLINKS`, `..` is `EXDEV` with `RESOLVE_BENEATH`, and a
/// descriptor's `/proc` link is `ELOOP` with `RESOLVE_NO_MAGICLINKS`.
fn openat2_resolves(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    page.reset();
    let target = page.put(FIRST_ONLY)?;
    let link = page.put(&under(b"abs"))?;
    tally.ok(
        by_number(
            page.process,
            Syscall::Symlinkat,
            [target, AT_FDCWD as u64, link, 0, 0, 0],
        ),
        "the openat2 check's link could not be made",
    )?;
    page.reset();
    let dir = open(page, BASE, O_PATH, 0)?;
    let dir = tally.done(dir, "the openat2 check's directory would not open")?;
    let outcome = openat2_with(page, tally, dir);
    close(page.process, dir);
    outcome
}

/// [`openat2_resolves`] with its directory open as `dir`.
fn openat2_with(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    dir: usize,
) -> Result<(), &'static str> {
    const IN_ROOT: u64 = 0x10;
    const BENEATH: u64 = 0x08;
    const NO_SYMLINKS: u64 = 0x04;
    const NO_MAGICLINKS: u64 = 0x02;
    const SIZE: u64 = 24;
    // Without a resolve flag the link leads out to the file; in root, the
    // same absolute target is looked for inside, where there is none.
    let plain = tally.done(
        openat2(page, dir, b"abs", 0, SIZE, &[])?,
        "openat2 with no resolve flag did not follow a link out",
    )?;
    close(page.process, plain);
    tally.refused(
        openat2(page, dir, b"abs", IN_ROOT, SIZE, &[])?,
        Errno::ENOENT,
        "openat2 RESOLVE_IN_ROOT followed an absolute link out of its root",
    )?;
    tally.refused(
        openat2(page, dir, b"abs", NO_SYMLINKS, SIZE, &[])?,
        Errno::ELOOP,
        "openat2 RESOLVE_NO_SYMLINKS followed a link",
    )?;
    tally.refused(
        openat2(page, dir, b"../.ns-first-only", BENEATH, SIZE, &[])?,
        Errno::EXDEV,
        "openat2 RESOLVE_BENEATH let .. out of its directory",
    )?;
    let own = format!("/proc/{}/fd/{dir}", page.process.pid());
    tally.refused(
        openat2(
            page,
            AT_FDCWD as usize,
            own.as_bytes(),
            NO_MAGICLINKS,
            SIZE,
            &[],
        )?,
        Errno::ELOOP,
        "openat2 RESOLVE_NO_MAGICLINKS followed a descriptor's /proc link",
    )?;
    tally.refused(
        openat2(page, dir, b"abs", IN_ROOT | BENEATH, SIZE, &[])?,
        Errno::EINVAL,
        "openat2 took RESOLVE_IN_ROOT and RESOLVE_BENEATH together",
    )?;
    tally.refused(
        openat2(page, dir, b"abs", 0, SIZE - 1, &[])?,
        Errno::EINVAL,
        "openat2 took a struct open_how shorter than its first version",
    )?;
    tally.refused(
        openat2(page, dir, b"abs", 0, SIZE + 8, &[1, 0, 0, 0, 0, 0, 0, 0])?,
        Errno::E2BIG,
        "openat2 took a longer struct open_how with a field it does not know set",
    )?;
    let longer = tally.done(
        openat2(page, dir, b"abs", 0, SIZE + 8, &[0; 8])?,
        "openat2 refused a longer struct open_how whose extra fields are zero",
    )?;
    close(page.process, longer);
    Ok(())
}

/// In the first namespace: a tmpfs at [`BASE`] with the file to bind and
/// the directories to mount on, the stage 12 volume bound in at `disk`, and
/// a file outside it.
fn set_up(page: &mut Page<'_>, tally: &mut Tally<'_>, disk: bool) -> Result<(), &'static str> {
    tally.ok(
        mkdir(page, BASE)?,
        "the namespace check's directory could not be made",
    )?;
    tally.ok(
        mount(page, b"none", BASE, b"tmpfs", 0)?,
        "the namespace check's tmpfs could not be mounted",
    )?;
    for name in [&b"m1"[..], b"m2", b"base", b"disk"] {
        tally.ok(
            mkdir(page, &under(name))?,
            "the namespace check's directories could not be made",
        )?;
    }
    write_file(
        page,
        tally,
        &under(b"old-only"),
        CONTENT,
        "the namespace check's file could not be written",
    )?;
    write_file(
        page,
        tally,
        FIRST_ONLY,
        CONTENT,
        "the namespace check's file outside it could not be written",
    )?;
    if disk {
        tally.ok(
            mount(page, b"/mnt-rw", &under(b"disk"), b"none", MS_BIND)?,
            "the stage 12 volume could not be bound into the namespace check",
        )?;
    }
    Ok(())
}

/// The second process's copy: private both ways, named apart, and refused
/// without privilege.
fn private_both_ways(
    outside: &mut Page<'_>,
    inside: &mut Page<'_>,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let (first_pid, second_pid) = (outside.process.pid(), inside.process.pid());
    if namespace_of(outside, first_pid)? != namespace_of(outside, second_pid)? {
        return Err("two processes of the first namespace named different namespaces");
    }

    let unprivileged = process::new_for_check()
        .map_err(|_| "could not make the namespace check's unprivileged process")?;
    tally.ok(
        by_number(&unprivileged, Syscall::Setuid, [1000, 0, 0, 0, 0, 0]),
        "the namespace check's process could not become uid 1000",
    )?;
    tally.refused(
        unshare(&unprivileged, CLONE_NEWNS),
        Errno::EPERM,
        "unshare(CLONE_NEWNS) by uid 1000 was not refused EPERM",
    )?;
    drop(unprivileged);
    tally.refused(
        unshare(inside.process, CLONE_NEWNET),
        Errno::EINVAL,
        "unshare of a namespace Ferrix does not have was not refused EINVAL",
    )?;

    tally.ok(
        unshare(inside.process, CLONE_NEWNS),
        "unshare(CLONE_NEWNS) was refused to root",
    )?;
    let theirs = namespace_of(outside, first_pid)?;
    let ours = namespace_of(outside, second_pid)?;
    if theirs == ours || !ours.starts_with(b"mnt:[") {
        return Err("/proc/<pid>/ns/mnt did not name the copy apart from the first namespace");
    }

    let m1 = under(b"m1");
    let m2 = under(b"m2");
    tally.ok(
        mount(inside, b"none", &m1, b"tmpfs", 0)?,
        "a tmpfs could not be mounted in a copied namespace",
    )?;
    tally.ok(
        mount(outside, b"none", &m2, b"tmpfs", 0)?,
        "a tmpfs could not be mounted in the first namespace after a copy",
    )?;
    let seen_outside = own_mountinfo(outside)?;
    let seen_inside = own_mountinfo(inside)?;
    if lists(&seen_outside, &m1) {
        return Err("a mount made in a copied namespace showed in the first");
    }
    if lists(&seen_inside, &m2) {
        return Err("a mount made in the first namespace after a copy showed in the copy");
    }
    if !lists(&seen_inside, &m1) || !lists(&seen_outside, &m2) {
        return Err("a namespace's own mount was missing from its mountinfo");
    }
    Ok(())
}

/// bubblewrap's calls as root, from its `setup_newroot` and `main`, to the
/// last `pivot_root(".", ".")`, and then only the new tree left.
fn bubblewrap_as_root(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    disk: bool,
) -> Result<(), &'static str> {
    first_pivot(page, tally)?;
    fill_newroot(page, tally, disk)?;
    last_pivot(page, tally)?;
    only_the_new_tree(page, disk)
}

/// A tmpfs base with `newroot` bound onto itself, and the first
/// `pivot_root`, which puts the old root at `oldroot`.
fn first_pivot(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    // From the namespace's own `/`, the kernel's tmpfs in memory, as a
    // program's root is where no disk was switched to: the mount the
    // bottom one holds up.
    let base = under(b"base");
    tally.ok(
        mount(page, b"tmpfs", &base, b"tmpfs", MS_NODEV | MS_NOSUID)?,
        "bubblewrap's base tmpfs could not be mounted",
    )?;
    tally.ok(
        with_path(page, Syscall::Chdir, &base, [0, 0, 0])?,
        "bubblewrap could not chdir to its base",
    )?;
    tally.ok(
        mkdir(page, b"newroot")?,
        "bubblewrap's newroot could not be made",
    )?;
    tally.ok(
        mount(page, b"newroot", b"newroot", b"none", MS_BIND | MS_REC)?,
        "newroot could not be bound onto itself",
    )?;
    tally.ok(
        mkdir(page, b"oldroot")?,
        "bubblewrap's oldroot could not be made",
    )?;
    tally.refused(
        pivot_root(page, b"newroot", &base)?,
        Errno::EINVAL,
        "pivot_root to a place put_old is not below was not refused EINVAL",
    )?;
    tally.ok(
        pivot_root(page, &base, b"oldroot")?,
        "pivot_root(base, oldroot) from the namespace's own / was refused",
    )?;
    tally.ok(
        with_path(page, Syscall::Chdir, b"/", [0, 0, 0])?,
        "bubblewrap could not chdir to its new root",
    )?;
    // The caller's root moved with the pivot, so the old root is below it.
    if read_file(page, &old(b"old-only"))? != Ok(CONTENT.to_vec()) {
        return Err("after pivot_root the old root was not at put_old");
    }
    Ok(())
}

/// What bubblewrap puts in `newroot` from the old root -- a file, a `proc`, a
/// tmpfs, and with a disk the stage 12 btrfs, written through the old root --
/// and the old root detached.
fn fill_newroot(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    disk: bool,
) -> Result<(), &'static str> {
    for name in [&b"/newroot/proc"[..], b"/newroot/tmp", b"/newroot/disk"] {
        tally.ok(mkdir(page, name)?, "a place in newroot could not be made")?;
    }
    write_file(
        page,
        tally,
        b"/newroot/file",
        b"",
        "a file to bind onto could not be made in newroot",
    )?;
    tally.ok(
        mount(page, &old(b"old-only"), b"/newroot/file", b"none", MS_BIND)?,
        "a file from the old root could not be bound into newroot",
    )?;
    tally.ok(
        mount(
            page,
            b"proc",
            b"/newroot/proc",
            b"proc",
            MS_NODEV | MS_NOSUID,
        )?,
        "a proc could not be mounted in newroot",
    )?;
    tally.ok(
        mount(page, b"tmpfs", b"/newroot/tmp", b"tmpfs", MS_NODEV)?,
        "a tmpfs could not be mounted in newroot",
    )?;
    if disk {
        tally.ok(
            mount(
                page,
                &old(b"disk"),
                b"/newroot/disk",
                b"none",
                MS_BIND | MS_REC,
            )?,
            "the btrfs could not be bound into newroot",
        )?;
        // Written through the old root, which is detached next: its
        // write-out is what puts this on the disk.
        let mut path = old(b"disk/");
        path.extend_from_slice(DETACH_FILE);
        write_file(
            page,
            tally,
            &path,
            COMMITTED,
            "a file could not be written into the btrfs inside the old root",
        )?;
    }
    tally.ok(
        unmount(page, b"/oldroot", MNT_DETACH)?,
        "the old root could not be detached",
    )?;
    if read_file(page, &old(b"old-only"))? != Err(Errno::ENOENT) {
        return Err("the old root was still reachable after its detach");
    }
    Ok(())
}

/// `pivot_root(".", ".")` from `newroot`, and the old root stacked on it
/// taken by `umount2(".", MNT_DETACH)`.
fn last_pivot(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    tally.ok(
        with_path(page, Syscall::Chdir, b"/newroot", [0, 0, 0])?,
        "bubblewrap could not chdir into newroot",
    )?;
    tally.ok(
        pivot_root(page, b".", b".")?,
        "pivot_root(\".\", \".\") was refused",
    )?;
    tally.ok(
        unmount(page, b".", MNT_DETACH)?,
        "umount2(\".\", MNT_DETACH) did not take the old root stacked on the new one",
    )?;
    tally.ok(
        with_path(page, Syscall::Chdir, b"/", [0, 0, 0])?,
        "bubblewrap could not chdir to its final root",
    )?;
    Ok(())
}

/// After the last `pivot_root`: `/` is the new tree, `..` stays in it,
/// nothing of the old tree is reachable, and `mountinfo` lists the new
/// tree's mounts alone.
fn only_the_new_tree(page: &mut Page<'_>, disk: bool) -> Result<(), &'static str> {
    if read_file(page, b"/file")? != Ok(CONTENT.to_vec())
        || read_file(page, b"/../../file")? != Ok(CONTENT.to_vec())
    {
        return Err("after the last pivot_root / was not the new tree, or .. left it");
    }
    for gone in [
        &b"/oldroot"[..],
        BASE,
        b"/old-only",
        b"/../old-only",
        FIRST_ONLY,
    ] {
        if read_file(page, gone)? != Err(Errno::ENOENT) {
            return Err("something of the old tree was reachable after the last pivot_root");
        }
    }
    let mountinfo = own_mountinfo(page)?;
    let lines = mountinfo
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .count();
    let wanted = if disk { 5 } else { 4 };
    if lines != wanted
        || !lists(&mountinfo, b"/")
        || !lists(&mountinfo, b"/file")
        || !lists(&mountinfo, b"/proc")
        || holds(&mountinfo, b"ns-check")
        || holds(&mountinfo, b"mnt-rw")
    {
        return Err("mountinfo after the last pivot_root listed more than the new tree");
    }
    Ok(())
}

/// Whether [`DETACH_FILE`] is on the stage 12 disk as written: read by a
/// second mount of it, read-only and of its own, which knows only what was
/// committed.
fn on_the_disk() -> Result<bool, &'static str> {
    let rdev = makedev(VIRTIO_BLK_MAJOR, DISK_INDEX * 16);
    let volume =
        fs::btrfs::mount(rdev).map_err(|_| "the stage 12 disk would not mount read-only")?;
    let Ok(file) = volume.root().lookup(DETACH_FILE) else {
        return Ok(false);
    };
    let mut bytes = vec![0_u8; COMMITTED.len() + 1];
    let len = file
        .read_at(0, &mut bytes)
        .map_err(|_| "the committed file could not be read off the disk")?;
    Ok(bytes.get(..len) == Some(COMMITTED))
}

/// A native process the second makes (`process_create`, through the
/// personality's `LoadNative`) is in its creator's namespace, root and
/// working directory (§2.5). The negative control is a child started in the
/// first namespace's root instead.
fn native_child_stays(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    creator: &Arc<Process>,
) -> Result<(), &'static str> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_ARGUMENT_PROGRAM,
    );
    let host = launch::load_native(Some(&**creator as &dyn Host), &file, b"/ns-check-child")
        .map_err(|_| "a native child could not be made inside a pivoted namespace")?;
    let child = core_process::downcast::<Process>(host)
        .ok_or("a native child was not a process of this personality")?;
    let context = child.fs_context().lock().clone();
    let finds = |path: &[u8]| fs::namespace().resolve(&context, None, path, true).is_ok();
    let (sees_file, sees_first) = (finds(b"/file"), finds(FIRST_ONLY));
    let same = namespace_of(page, child.pid())? == namespace_of(page, creator.pid())?;
    tally.report.calls += 1;
    process::kill(&child, 137);
    drop((child, context));
    if !sees_file || sees_first {
        return Err("a native child made inside a pivoted namespace was not in its creator's root");
    }
    if !same {
        return Err("a native child's /proc/<pid>/ns/mnt was not its creator's");
    }
    Ok(())
}
