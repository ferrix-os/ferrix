//! Binds, proved at boot (`docs/NAMESPACES.md`, landing N2): what bubblewrap
//! builds its container from, driven through `mount` and `umount2` by
//! number, as a program's calls would be.
//!
//! The check mounts a tmpfs `src` of its own under `/tmp`, holding a file, a
//! directory with a second tmpfs mounted inside it, and a listening Unix
//! socket, and then requires:
//!
//! * a bind of the directory without `MS_REC` shows the file and not the
//!   submount, whose mount point it can neither remove nor rename (`EBUSY`);
//!   with `MS_REC` it shows the submount too;
//! * a bind of a subdirectory, of a file, and of the socket -- which a
//!   `connect` through the bind then reaches -- and a directory bound onto
//!   itself, as bubblewrap binds `newroot`; a directory onto a file and a file
//!   onto a directory are `ENOTDIR`;
//! * `mountinfo` names each bind as `readlink` of an `O_PATH` descriptor of
//!   it does, with its root inside the filesystem (`/a`, `/f`);
//! * `MS_REMOUNT | MS_BIND | MS_RDONLY` makes one bind read-only and no other
//!   mount of the filesystem, and a plain `MS_REMOUNT | MS_RDONLY` makes every
//!   mount of it read-only, and nothing of another filesystem (the interim
//!   reviewer's note on N1); a plain remount back makes them all writable;
//! * `MS_PRIVATE`, `MS_SLAVE`, `MS_UNBINDABLE` accepted on a mount's root,
//!   and `MS_SHARED`, two types at once, a place inside a mount, and
//!   `MS_MOVE` refused `EINVAL`;
//! * `umount2` of a mount with one inside it is `EBUSY`, and with
//!   `MNT_DETACH` takes both: the path no longer reaches the submount, and
//!   `..` from a descriptor kept inside it stays where it is, since a
//!   detached mount has no parent; a bind whose source is in the detached
//!   mount is `EINVAL`, and a bind outlives the unmount of its source.

use alloc::format;
use alloc::vec::Vec;

use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::socket::{AF_UNIX, SOCK_STREAM};
use ferrix_linux_abi::types::{
    AT_FDCWD, AT_REMOVEDIR, MNT_DETACH, MS_BIND, MS_MOVE, MS_PRIVATE, MS_RDONLY, MS_REC,
    MS_REMOUNT, MS_SHARED, MS_SLAVE, MS_UNBINDABLE, O_CREAT, O_PATH, O_RDONLY, O_WRONLY,
};
use ferrix_vfs::Errno;

use crate::fs::mount_check::{Page, READ_ROOM, Report, Tally, by_number, close, open, page_for};
use crate::syscall::process;

/// Where the check works.
const BASE: &[u8] = b"/tmp/.bind-check";

/// The directories and files the check makes under [`BASE`], in order.
const DIRECTORIES: [&[u8]; 5] = [b"src", b"d1", b"d2", b"d3", b"d4"];
/// The plain files it binds onto.
const TARGET_FILES: [&[u8]; 2] = [b"ffile", b"sockfile"];

/// What `src/f` holds.
const CONTENT: &[u8] = b"bind";

/// A path under [`BASE`].
fn at(name: &[u8]) -> Vec<u8> {
    let mut path = BASE.to_vec();
    if !name.is_empty() {
        path.push(b'/');
        path.extend_from_slice(name);
    }
    path
}

/// `mount(source, target, type, flags, NULL)` by number.
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

/// `mount(source, target, NULL, MS_BIND | extra)`.
fn bind(
    page: &mut Page<'_>,
    source: &[u8],
    target: &[u8],
    extra: u32,
) -> Result<Result<usize, Errno>, &'static str> {
    mount(page, &at(source), &at(target), b"none", MS_BIND | extra)
}

/// `umount2(target, flags)`.
fn unmount(
    page: &mut Page<'_>,
    target: &[u8],
    flags: u32,
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    let target = page.put(&at(target))?;
    Ok(by_number(
        page.process,
        Syscall::Umount2,
        [target, u64::from(flags), 0, 0, 0, 0],
    ))
}

/// Open `name` under [`BASE`] with `flags`, from a fresh page.
fn open_at(
    page: &mut Page<'_>,
    name: &[u8],
    flags: u32,
) -> Result<Result<usize, Errno>, &'static str> {
    page.reset();
    open(page, &at(name), flags, 0o644)
}

/// Whether `name` opens for writing: `Ok(())`, or the refusal.
fn writes(page: &mut Page<'_>, name: &[u8]) -> Result<Result<usize, Errno>, &'static str> {
    let got = open_at(page, name, O_WRONLY)?;
    if let Ok(fd) = got {
        close(page.process, fd);
    }
    Ok(got.map(|_| 0))
}

/// The bytes of `name`, which must open.
fn contents(page: &mut Page<'_>, name: &[u8]) -> Result<Vec<u8>, &'static str> {
    let fd = open_at(page, name, O_RDONLY)?.map_err(|_| "a file the check reads would not open")?;
    let got = by_number(
        page.process,
        Syscall::Read,
        [fd as u64, page.buffer(), READ_ROOM, 0, 0, 0],
    );
    close(page.process, fd);
    let len = got.map_err(|_| "a file the check reads would not read")?;
    page.read_back(len)
}

/// What `/proc/<pid>/fd/<fd>` reads.
fn link_of(page: &mut Page<'_>, fd: usize) -> Result<Vec<u8>, &'static str> {
    page.reset();
    let pid = page.process.pid();
    let link = page.put(format!("/proc/{pid}/fd/{fd}").as_bytes())?;
    let len = by_number(
        page.process,
        Syscall::Readlinkat,
        [AT_FDCWD as u64, link, page.buffer(), READ_ROOM, 0, 0],
    )
    .map_err(|_| "a descriptor's /proc link would not read")?;
    page.read_back(len)
}

/// The `mountinfo` line whose mount point is `point`, split into fields.
fn mountinfo_line(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    point: &[u8],
) -> Result<Option<Vec<Vec<u8>>>, &'static str> {
    let pid = page.process.pid();
    let info = crate::fs::mount_check::read_whole(
        page,
        tally,
        format!("/proc/{pid}/mountinfo").as_bytes(),
    )?;
    // The last line for the point: a mount on top of another is later.
    Ok(info
        .split(|&byte| byte == b'\n')
        .filter_map(|line| {
            let fields: Vec<Vec<u8>> = line
                .split(|&byte| byte == b' ')
                .map(<[u8]>::to_vec)
                .collect();
            (fields.get(4).map(Vec::as_slice) == Some(point)).then_some(fields)
        })
        .last())
}

/// Run the check.
pub(crate) fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the bind check")?;
    let mut page = page_for(&process)?;
    let mut report = Report::default();
    let mut descriptors = Vec::new();
    let outcome = check(&mut page, &mut report, &mut descriptors);
    for fd in descriptors {
        close(&process, fd);
    }
    clean_up(&mut page);
    outcome.map(|()| report)
}

/// Take down whatever the check left: every mount it made, detached, then
/// every name.
fn clean_up(page: &mut Page<'_>) {
    for _ in 0..3 {
        for name in [
            &b"d1"[..],
            b"d2",
            b"d3",
            b"d4",
            b"ffile",
            b"sockfile",
            b"src",
        ] {
            let _ = unmount(page, name, MNT_DETACH);
        }
    }
    for name in TARGET_FILES {
        page.reset();
        if let Ok(path) = page.put(&at(name)) {
            let _ = by_number(
                page.process,
                Syscall::Unlinkat,
                [AT_FDCWD as u64, path, 0, 0, 0, 0],
            );
        }
    }
    for name in DIRECTORIES.iter().chain([&&b""[..]]) {
        page.reset();
        if let Ok(path) = page.put(&at(name)) {
            let _ = by_number(
                page.process,
                Syscall::Unlinkat,
                [AT_FDCWD as u64, path, u64::from(AT_REMOVEDIR), 0, 0, 0],
            );
        }
    }
}

/// The check proper; [`run`] cleans up after it either way. Descriptors it
/// keeps open go in `descriptors` for [`run`] to close.
fn check(
    page: &mut Page<'_>,
    report: &mut Report,
    descriptors: &mut Vec<usize>,
) -> Result<(), &'static str> {
    let mut tally = Tally { report };
    let listener = set_up(page, &mut tally)?;
    descriptors.push(listener);
    binds(page, &mut tally, descriptors)?;
    remounts(page, &mut tally)?;
    propagation(page, &mut tally)?;
    detach(page, &mut tally, descriptors)
}

/// Make the directories, `src` as a tmpfs with a file, `a/deep` with a
/// second tmpfs holding `inner`, and a socket listening at `src/sock`;
/// answer the listening socket.
fn set_up(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<usize, &'static str> {
    let process = page.process;
    for name in [&b""[..]].into_iter().chain(DIRECTORIES) {
        page.reset();
        let path = page.put(&at(name))?;
        let got = by_number(
            process,
            Syscall::Mkdirat,
            [AT_FDCWD as u64, path, 0o755, 0, 0, 0],
        );
        tally.ok(got, "a directory of the bind check could not be made")?;
    }
    for name in TARGET_FILES {
        let got = open_at(page, name, O_CREAT | O_WRONLY)?;
        let fd = tally.done(got, "a file of the bind check could not be made")?;
        close(process, fd);
    }
    let got = mount(page, b"tmpfs", &at(b"src"), b"tmpfs", 0)?;
    tally.ok(got, "the bind check's tmpfs could not be mounted")?;
    for name in [&b"src/a"[..], b"src/a/deep"] {
        page.reset();
        let path = page.put(&at(name))?;
        let got = by_number(
            process,
            Syscall::Mkdirat,
            [AT_FDCWD as u64, path, 0o755, 0, 0, 0],
        );
        tally.ok(
            got,
            "a directory in the bind check's tmpfs could not be made",
        )?;
    }
    let got = mount(page, b"tmpfs", &at(b"src/a/deep"), b"tmpfs", 0)?;
    tally.ok(got, "the bind check's inner tmpfs could not be mounted")?;
    for (name, bytes) in [(&b"src/f"[..], CONTENT), (b"src/a/deep/inner", b"in")] {
        let fd = open_at(page, name, O_CREAT | O_WRONLY)?;
        let fd = tally.done(fd, "a file in the bind check's tmpfs could not be made")?;
        let data = page.put(bytes)?;
        let got = by_number(
            process,
            Syscall::Write,
            [fd as u64, data, bytes.len() as u64, 0, 0, 0],
        );
        close(process, fd);
        tally.ok(got, "a file in the bind check's tmpfs could not be written")?;
    }
    let listener = by_number(
        process,
        Syscall::Socket,
        [u64::from(AF_UNIX), u64::from(SOCK_STREAM), 0, 0, 0, 0],
    );
    let listener = tally.done(listener, "the bind check's socket could not be made")?;
    let (address, len) = socket_address(page, b"src/sock")?;
    let got = by_number(
        process,
        Syscall::Bind,
        [listener as u64, address, len, 0, 0, 0],
    );
    tally.ok(got, "the bind check's socket could not be bound")?;
    let got = by_number(process, Syscall::Listen, [listener as u64, 4, 0, 0, 0, 0]);
    tally.ok(got, "the bind check's socket would not listen")?;
    Ok(listener)
}

/// A `sockaddr_un` for `name` under [`BASE`], staged: its address and length.
fn socket_address(page: &mut Page<'_>, name: &[u8]) -> Result<(u64, u64), &'static str> {
    page.reset();
    let mut bytes = AF_UNIX.to_ne_bytes().to_vec();
    bytes.extend_from_slice(&at(name));
    bytes.push(0);
    let len = bytes.len() as u64;
    Ok((page.put_bytes(&bytes)?, len))
}

/// Each kind of bind, and what it shows.
fn binds(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    descriptors: &mut Vec<usize>,
) -> Result<(), &'static str> {
    let process = page.process;
    let got = bind(page, b"src/f", b"d4", 0)?;
    tally.refused(got, Errno::ENOTDIR, "a file was bound onto a directory")?;
    let got = bind(page, b"src/a", b"ffile", 0)?;
    tally.refused(got, Errno::ENOTDIR, "a directory was bound onto a file")?;

    // The directory, without and with MS_REC.
    let got = bind(page, b"src", b"d1", 0)?;
    tally.ok(got, "a directory could not be bound")?;
    if contents(page, b"d1/f")? != CONTENT {
        return Err("a bound directory does not show its file");
    }
    let got = open_at(page, b"d1/a/deep/inner", O_RDONLY)?;
    tally.refused(
        got,
        Errno::ENOENT,
        "a bind without MS_REC showed a submount",
    )?;
    // What that bind shows in the submount's place is still a mount point,
    // though the walk through the bind crossed no mount there: it can be
    // neither removed nor renamed, as Linux's `d_mountpoint` refuses.
    page.reset();
    let covered = page.put(&at(b"d1/a/deep"))?;
    let got = by_number(
        process,
        Syscall::Unlinkat,
        [AT_FDCWD as u64, covered, u64::from(AT_REMOVEDIR), 0, 0, 0],
    );
    tally.refused(
        got,
        Errno::EBUSY,
        "a mount point reached through another bind was removed",
    )?;
    let moved = page.put(&at(b"d1/a/moved"))?;
    let got = by_number(
        process,
        Syscall::Renameat2,
        [AT_FDCWD as u64, covered, AT_FDCWD as u64, moved, 0, 0],
    );
    tally.refused(
        got,
        Errno::EBUSY,
        "a mount point reached through another bind was renamed",
    )?;
    let got = bind(page, b"src", b"d2", MS_REC)?;
    tally.ok(got, "a directory could not be bound with MS_REC")?;
    if contents(page, b"d2/a/deep/inner")? != b"in" {
        return Err("a bind with MS_REC did not copy the submount");
    }

    // A subdirectory, a file, and a directory onto itself.
    let got = bind(page, b"src/a", b"d3", 0)?;
    tally.ok(got, "a subdirectory could not be bound")?;
    let got = open_at(page, b"d3/deep", O_PATH)?;
    let deep = tally.done(got, "a bound subdirectory does not show its own directory")?;
    close(process, deep);
    let got = bind(page, b"src/f", b"ffile", 0)?;
    tally.ok(got, "a file could not be bound")?;
    if contents(page, b"ffile")? != CONTENT {
        return Err("a bound file does not read as its source");
    }
    let got = bind(page, b"d4", b"d4", MS_REC)?;
    tally.ok(got, "a directory could not be bound onto itself")?;
    page.reset();
    let inside = page.put(&at(b"d4/made"))?;
    let got = by_number(
        process,
        Syscall::Mkdirat,
        [AT_FDCWD as u64, inside, 0o755, 0, 0, 0],
    );
    tally.ok(got, "a directory bound onto itself took no directory")?;

    // The socket, and a connection through the bind.
    let got = bind(page, b"src/sock", b"sockfile", 0)?;
    tally.ok(got, "a socket could not be bound")?;
    let client = by_number(
        process,
        Syscall::Socket,
        [u64::from(AF_UNIX), u64::from(SOCK_STREAM), 0, 0, 0, 0],
    );
    let client = tally.done(client, "the bind check's client socket could not be made")?;
    descriptors.push(client);
    let (address, len) = socket_address(page, b"sockfile")?;
    let got = by_number(
        process,
        Syscall::Connect,
        [client as u64, address, len, 0, 0, 0],
    );
    tally.ok(
        got,
        "a connect through a bound socket did not reach the listener",
    )?;

    // What bubblewrap compares after every bind: the descriptor's link and
    // mountinfo's mount point, and the root inside the filesystem.
    for (name, root) in [(&b"d3"[..], &b"/a"[..]), (b"ffile", b"/f"), (b"d1", b"/")] {
        let got = open_at(page, name, O_PATH)?;
        let held = tally.done(got, "a bind would not open O_PATH")?;
        let link = link_of(page, held);
        close(process, held);
        let link = link?;
        if link != at(name) {
            return Err("an O_PATH descriptor's link did not name the bind");
        }
        let Some(fields) = mountinfo_line(page, tally, &link)? else {
            return Err("mountinfo has no line for a bind");
        };
        if fields.get(3).map(Vec::as_slice) != Some(root)
            || fields.get(7).map(Vec::as_slice) != Some(&b"tmpfs"[..])
        {
            return Err("mountinfo's line for a bind has the wrong root or type");
        }
    }
    Ok(())
}

/// The two kinds of remount: one mount, or the filesystem.
fn remounts(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let got = mount(
        page,
        b"none",
        &at(b"d1"),
        b"none",
        MS_REMOUNT | MS_BIND | MS_RDONLY,
    )?;
    tally.ok(got, "a bind remount read-only was refused")?;
    let got = writes(page, b"d1/f")?;
    tally.refused(got, Errno::EROFS, "a bind remounted read-only took a write")?;
    for name in [&b"src/f"[..], b"d2/f", b"ffile"] {
        let got = writes(page, name)?;
        tally.ok(
            got,
            "a bind remount read-only reached another mount of the filesystem",
        )?;
    }
    let got = mount(page, b"none", &at(b"d1"), b"none", MS_REMOUNT | MS_BIND)?;
    tally.ok(got, "a bind remount read-write was refused")?;
    let got = writes(page, b"d1/f")?;
    tally.ok(got, "a bind remounted read-write refused a write")?;

    // A plain remount: the filesystem, through every bind of it.
    let got = mount(page, b"none", &at(b"d1"), b"none", MS_REMOUNT | MS_RDONLY)?;
    tally.ok(got, "a plain remount read-only was refused")?;
    for name in [&b"d1/f"[..], b"src/f", b"d2/f", b"ffile"] {
        let got = writes(page, name)?;
        tally.refused(
            got,
            Errno::EROFS,
            "a plain remount read-only left a bind of the filesystem writable",
        )?;
    }
    let got = writes(page, b"d2/a/deep/inner")?;
    tally.ok(got, "a plain remount read-only reached another filesystem")?;
    let Some(fields) = mountinfo_line(page, tally, &at(b"src"))? else {
        return Err("mountinfo has no line for the bind check's tmpfs");
    };
    if fields.get(5).map(Vec::as_slice) != Some(&b"rw"[..])
        || fields.get(9).map(Vec::as_slice) != Some(&b"ro"[..])
    {
        return Err("mountinfo does not show a filesystem read-only under a writable mount");
    }
    let got = mount(page, b"none", &at(b"d1"), b"none", MS_REMOUNT)?;
    tally.ok(got, "a plain remount read-write was refused")?;
    for name in [&b"d1/f"[..], b"src/f", b"d2/f", b"ffile"] {
        let got = writes(page, name)?;
        tally.ok(got, "a plain remount read-write left a bind read-only")?;
    }
    Ok(())
}

/// The propagation flags: accepted as the no-ops they are here, or refused.
fn propagation(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    for kind in [
        MS_REC | MS_PRIVATE,
        MS_REC | MS_SLAVE,
        MS_UNBINDABLE,
        MS_PRIVATE,
    ] {
        let got = mount(page, b"none", &at(b"d2"), b"none", kind)?;
        tally.ok(got, "a propagation change on a mount's root was refused")?;
    }
    for (target, kind, why) in [
        (&b"d2"[..], MS_SHARED, "MS_SHARED was accepted"),
        (
            b"d2",
            MS_PRIVATE | MS_SLAVE,
            "two propagation types at once were accepted",
        ),
        (
            b"d2/a",
            MS_PRIVATE,
            "a propagation change inside a mount was accepted",
        ),
        (b"d2", MS_MOVE, "MS_MOVE was accepted"),
    ] {
        let got = mount(page, b"none", &at(target), b"none", kind)?;
        tally.refused(got, Errno::EINVAL, why)?;
    }
    Ok(())
}

/// `umount2`, with and without `MNT_DETACH`.
fn detach(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    descriptors: &mut Vec<usize>,
) -> Result<(), &'static str> {
    let process = page.process;
    let got = open_at(page, b"d2/a/deep", O_PATH)?;
    let kept = tally.done(got, "the copied submount would not open O_PATH")?;
    descriptors.push(kept);
    let got = unmount(page, b"d2", 0)?;
    tally.refused(
        got,
        Errno::EBUSY,
        "a mount with one inside it unmounted without MNT_DETACH",
    )?;
    let got = unmount(page, b"d2/a", MNT_DETACH)?;
    tally.refused(got, Errno::EINVAL, "an unmount inside a mount was accepted")?;
    let got = unmount(page, b"d2", MNT_DETACH)?;
    tally.ok(got, "MNT_DETACH of a mount with one inside it was refused")?;
    let got = open_at(page, b"d2/a", O_PATH)?;
    tally.refused(
        got,
        Errno::ENOENT,
        "a detached bind is still reached by its path",
    )?;
    if mountinfo_line(page, tally, &at(b"d2/a/deep"))?.is_some()
        || mountinfo_line(page, tally, &at(b"d2"))?.is_some()
    {
        return Err("mountinfo still lists a detached mount");
    }
    // `..` from inside the detached submount stays where it is.
    page.reset();
    let dotdot = page.put(b"..")?;
    let got = by_number(
        process,
        Syscall::Openat,
        [kept as u64, dotdot, u64::from(O_PATH), 0, 0, 0],
    );
    let above = tally.done(got, "`..` from a detached mount would not open")?;
    descriptors.push(above);
    if link_of(page, above)? != link_of(page, kept)? {
        return Err("`..` from a detached mount's root left it: the mount kept a parent");
    }
    // A bind from inside the detached mount: not in the tree, EINVAL.
    let pid = process.pid();
    let source = format!("/proc/{pid}/fd/{kept}");
    let got = mount(page, source.as_bytes(), &at(b"d2"), b"none", MS_BIND)?;
    tally.refused(
        got,
        Errno::EINVAL,
        "a bind from a detached mount was accepted",
    )?;

    // The source filesystem's own mount goes; a bind of it stays.
    let got = unmount(page, b"src", MNT_DETACH)?;
    tally.ok(got, "the bind check's tmpfs would not detach")?;
    if contents(page, b"d1/f")? != CONTENT {
        return Err("a bind did not outlive the unmount of its source");
    }
    Ok(())
}
