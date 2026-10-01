//! A mount's own flags, proved at boot: `MS_RDONLY`, `MS_NODEV`, `MS_NOEXEC`
//! and `MS_NOSUID` enforced, `MS_REMOUNT` and `MS_REMOUNT | MS_BIND`
//! changing them, and `/proc/<pid>/mountinfo`, `/proc/<pid>/mounts` and
//! `statfs` showing them (`docs/NAMESPACES.md`, landing N1).
//!
//! The check mounts a tmpfs of its own under `/tmp`, fills it -- a file, a
//! directory, a character device, a set-user-id file owned by uid 1000 --
//! and remounts it `ro,nosuid,nodev,noexec`. Then every change a program can
//! ask for through a path or a descriptor must be `EROFS`, before anything
//! else is asked, as Linux's `mnt_want_write` has it; the device must be
//! `EACCES`; the file must not run, nor be mapped executable (`EPERM`) nor
//! made so by `mprotect` (`EACCES`); and the set-user-id bit must be
//! ignored. What Linux still allows must still work: reading, a name that
//! exists answering `EEXIST`, a device opened for writing through a mount
//! that is read-only but not `nodev`, and a file opened for writing before
//! the remount writing on. A remount of a place that is not a mount's root is
//! `EINVAL`, and one by a caller that is not root `EPERM`. `mountinfo` must
//! name the mount by the path an `O_PATH` descriptor's `/proc/<pid>/fd` link
//! reads -- the comparison bubblewrap makes after every bind -- with its
//! options; a remount naming no access-time flag must keep the old one; and
//! a remount read-write must give everything back.

use alloc::vec;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_FDCWD, AT_REMOVEDIR, ArmStatfs, MAP_ANONYMOUS, MAP_PRIVATE, MS_BIND, MS_NOATIME, MS_NODEV,
    MS_NOEXEC, MS_NOSUID, MS_RDONLY, MS_REMOUNT, O_CREAT, O_EXCL, O_PATH, O_RDONLY, O_WRONLY,
    PROT_EXEC, PROT_READ, PROT_WRITE, S_IFCHR, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RDONLY, ST_VALID,
    Statfs,
};
use ferrix_vfs::Errno;

use crate::fs;
use crate::syscall::check as syscall_check;
use crate::syscall::fsctl;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::uaccess;

/// Where the check mounts its tmpfs.
const MOUNT_POINT: &[u8] = b"/tmp/.mount-check";
/// Where the check binds the mount, to show the bind keeps what it was given.
const BIND_POINT: &[u8] = b"/tmp/.mount-check-bind";
/// Whether [`under`] names the bind and not the mount.
static THROUGH_THE_BIND: AtomicBool = AtomicBool::new(false);

/// What the check saw, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Calls answered as Linux answers them.
    pub(crate) calls: u32,
    /// Of them, refusals with Linux's error.
    pub(crate) refusals: u32,
}

/// The check's scratch page: paths are staged at the start, a read lands in
/// the second half.
pub(super) struct Page<'a> {
    pub(super) process: &'a Process,
    base: u64,
    /// Where the next staged string goes.
    next: u64,
}

/// Half a page for staged strings, half for what is read back.
pub(super) const STAGED: u64 = 2048;
/// Bytes a read may bring back.
pub(super) const READ_ROOM: u64 = 8192;

impl Page<'_> {
    /// Stage `text` with its NUL and answer its address.
    pub(super) fn put(&mut self, text: &[u8]) -> Result<u64, &'static str> {
        let at = self.base + self.next;
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text);
        bytes.push(0);
        let len = bytes.len() as u64;
        if self.next + len > STAGED {
            return Err("the mount check ran out of room for its paths");
        }
        uaccess::copy_to_user(self.process.space(), at, &bytes)
            .map_err(|_| "the mount check could not stage a path")?;
        self.next += len;
        Ok(at)
    }

    /// Stage `bytes` as they are, with no NUL, and answer their address.
    pub(super) fn put_bytes(&mut self, bytes: &[u8]) -> Result<u64, &'static str> {
        let at = self.base + self.next;
        let len = bytes.len() as u64;
        if self.next + len > STAGED {
            return Err("a check ran out of room for its staged bytes");
        }
        uaccess::copy_to_user(self.process.space(), at, bytes)
            .map_err(|_| "a check could not stage its bytes")?;
        self.next += len;
        Ok(at)
    }

    /// Where a read lands.
    pub(super) fn buffer(&self) -> u64 {
        self.base + STAGED
    }

    /// `len` bytes of what a read left.
    pub(super) fn read_back(&self, len: usize) -> Result<Vec<u8>, &'static str> {
        let mut bytes = vec![0_u8; len];
        uaccess::copy_from_user(self.process.space(), self.buffer(), &mut bytes)
            .map_err(|_| "the mount check could not read its buffer")?;
        Ok(bytes)
    }

    /// Start staging again.
    pub(super) fn reset(&mut self) {
        self.next = 0;
    }
}

/// Counts what was answered.
pub(super) struct Tally<'a> {
    pub(super) report: &'a mut Report,
}

impl Tally<'_> {
    /// Require `got` to succeed.
    pub(super) fn done(
        &mut self,
        got: Result<usize, Errno>,
        what: &'static str,
    ) -> Result<usize, &'static str> {
        self.report.calls += 1;
        got.map_err(|_| what)
    }

    /// Require `got` to succeed, whatever it answered.
    pub(super) fn ok(
        &mut self,
        got: Result<usize, Errno>,
        what: &'static str,
    ) -> Result<(), &'static str> {
        self.done(got, what).map(drop)
    }

    /// Require `got` to be `Err(wanted)`.
    pub(super) fn refused(
        &mut self,
        got: Result<usize, Errno>,
        wanted: Errno,
        what: &'static str,
    ) -> Result<(), &'static str> {
        self.report.calls += 1;
        if got != Err(wanted) {
            return Err(what);
        }
        self.report.refusals += 1;
        Ok(())
    }
}

/// Make `call` by its number, as a program would.
pub(super) fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}

/// `mount("none", target, "tmpfs" or "none", flags, NULL)`.
fn mount(
    page: &mut Page<'_>,
    target: &[u8],
    kind: &[u8],
    flags: u32,
) -> Result<Result<usize, Errno>, &'static str> {
    let source = page.put(b"none")?;
    let target = page.put(target)?;
    let kind = page.put(kind)?;
    Ok(by_number(
        page.process,
        Syscall::Mount,
        [source, target, kind, u64::from(flags), 0, 0],
    ))
}

/// `openat(AT_FDCWD, path, flags, mode)`.
pub(super) fn open(
    page: &mut Page<'_>,
    path: &[u8],
    flags: u32,
    mode: u32,
) -> Result<Result<usize, Errno>, &'static str> {
    let at = page.put(path)?;
    Ok(by_number(
        page.process,
        Syscall::Openat,
        [AT_FDCWD as u64, at, u64::from(flags), u64::from(mode), 0, 0],
    ))
}

/// `close(fd)`, whose answer the check does not need.
pub(super) fn close(process: &Process, fd: usize) {
    let _ = by_number(process, Syscall::Close, [fd as u64, 0, 0, 0, 0, 0]);
}

/// A path under [`MOUNT_POINT`], or under [`BIND_POINT`] while the bind is checked.
fn under(name: &[u8]) -> Vec<u8> {
    let mut path = if THROUGH_THE_BIND.load(Ordering::Relaxed) {
        BIND_POINT
    } else {
        MOUNT_POINT
    }
    .to_vec();
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

/// A scratch page in `process`, for staging paths and reading back.
pub(super) fn page_for(process: &Process) -> Result<Page<'_>, &'static str> {
    let base = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: STAGED + READ_ROOM,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "a mount check's page was refused")? as u64;
    Ok(Page {
        process,
        base,
        next: 0,
    })
}

/// Run the check.
pub(crate) fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the mount check")?;
    let mut page = page_for(&process)?;
    let mut report = Report::default();
    let outcome = check(&mut page, &mut report);
    // Whatever happened, the mount goes: a remount read-write first, so the
    // files can be removed if a failure left it read-only, then the unmount.
    page.reset();
    let _ = mount(&mut page, MOUNT_POINT, b"none", MS_REMOUNT);
    page.reset();
    if let Ok(target) = page.put(MOUNT_POINT) {
        let _ = by_number(&process, Syscall::Umount2, [target, 0, 0, 0, 0, 0]);
        let _ = by_number(
            &process,
            Syscall::Unlinkat,
            [AT_FDCWD as u64, target, u64::from(AT_REMOVEDIR), 0, 0, 0],
        );
    }
    outcome.map(|()| report)
}

/// The check proper; [`run`] cleans up after it either way.
fn check(page: &mut Page<'_>, report: &mut Report) -> Result<(), &'static str> {
    let process = page.process;
    let mut tally = Tally { report };
    let early = fill(page, &mut tally)?;
    refuses_a_wrong_remount(page, &mut tally)?;

    // ro,nosuid,nodev,noexec, and the access time kept.
    page.reset();
    let got = mount(
        page,
        MOUNT_POINT,
        b"none",
        MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC,
    )?;
    tally.ok(got, "the remount read-only was refused")?;
    changes_are_erofs(page, &mut tally)?;
    attributes_are_erofs(page, &mut tally)?;
    no_devices_no_programs(page, &mut tally)?;
    through_a_bind(page, &mut tally)?;
    page.reset();
    let junk = page.put(b"abc")?;
    let got = by_number(process, Syscall::Write, [early as u64, junk, 3, 0, 0, 0]);
    if tally.done(
        got,
        "a file opened for writing before the remount could not write",
    )? != 3
    {
        return Err("a file opened for writing before the remount wrote short");
    }
    close(process, early);
    shows_the_flags(page, &mut tally, "ro,nosuid,nodev,noexec,noatime")?;

    one_flag_at_a_time(page, &mut tally)?;
    read_write_again(page, &mut tally)
}

/// A bind of the `ro,nosuid,nodev,noexec` mount is `ro,nosuid,nodev,noexec`
/// (`docs/NAMESPACES.md` §8): it shows them, and refuses what each refuses --
/// a write, a device, a program -- since the bind is the boundary a container
/// is given (N5).
fn through_a_bind(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let process = page.process;
    page.reset();
    let point = page.put(BIND_POINT)?;
    tally.ok(
        by_number(
            process,
            Syscall::Mkdirat,
            [AT_FDCWD as u64, point, 0o755, 0, 0, 0],
        ),
        "the bind's mount point could not be made",
    )?;
    let source = page.put(MOUNT_POINT)?;
    let result = by_number(
        process,
        Syscall::Mount,
        [source, point, 0, u64::from(MS_BIND), 0, 0],
    );
    tally.ok(result, "a bind of the check's mount was refused")?;
    THROUGH_THE_BIND.store(true, Ordering::Relaxed);
    let checked = binds_keep_the_flags(page, tally);
    THROUGH_THE_BIND.store(false, Ordering::Relaxed);
    page.reset();
    let point = page.put(BIND_POINT)?;
    let _ = by_number(process, Syscall::Umount2, [point, 0, 0, 0, 0, 0]);
    let _ = by_number(
        process,
        Syscall::Unlinkat,
        [AT_FDCWD as u64, point, u64::from(AT_REMOVEDIR), 0, 0, 0],
    );
    checked
}

/// What [`through_a_bind`] requires of the bind, with it bound.
fn binds_keep_the_flags(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let info = read_whole(
        page,
        tally,
        alloc::format!("/proc/{}/mountinfo", page.process.pid()).as_bytes(),
    )?;
    let options = info.split(|&byte| byte == b'\n').find_map(|line| {
        let fields: Vec<&[u8]> = line.split(|&byte| byte == b' ').collect();
        (fields.get(4).copied() == Some(BIND_POINT)).then(|| fields.get(5).copied())
    });
    let Some(Some(options)) = options else {
        return Err("mountinfo has no line for the check's bind");
    };
    if !options.starts_with(b"ro,nosuid,nodev,noexec") {
        return Err("a bind of a ro,nosuid,nodev,noexec mount did not show those flags");
    }
    changes_are_erofs(page, tally)?;
    attributes_are_erofs(page, tally)?;
    no_devices_no_programs(page, tally)
}

/// Mount the check's tmpfs, `noatime` from the start, and fill it; answer
/// the file, opened for writing before any remount.
fn fill(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<usize, &'static str> {
    let process = page.process;
    let point = page.put(MOUNT_POINT)?;
    let _ = by_number(
        process,
        Syscall::Mkdirat,
        [AT_FDCWD as u64, point, 0o755, 0, 0, 0],
    );
    let got = mount(page, MOUNT_POINT, b"tmpfs", MS_NOATIME)?;
    tally.ok(got, "a tmpfs could not be mounted for the check")?;
    let file = open(page, &under(b"file"), O_CREAT | O_WRONLY | O_EXCL, 0o755)?;
    let file = tally.done(file, "the check's file could not be made")?;
    // Bytes that are not a program: if `noexec` let it through, the loader
    // would refuse it with `ENOEXEC`, not `EACCES`.
    let junk = page.put(b"not a program")?;
    let got = by_number(process, Syscall::Write, [file as u64, junk, 13, 0, 0, 0]);
    tally.ok(got, "the check's file could not be written")?;
    close(process, file);
    let dir = page.put(&under(b"dir"))?;
    let got = by_number(
        process,
        Syscall::Mkdirat,
        [AT_FDCWD as u64, dir, 0o755, 0, 0, 0],
    );
    tally.ok(got, "the check's directory could not be made")?;
    let null = page.put(&under(b"null"))?;
    // /dev/null's number, 1:3, as `makedev` packs it.
    let got = by_number(
        process,
        Syscall::Mknodat,
        [
            AT_FDCWD as u64,
            null,
            u64::from(S_IFCHR | 0o666),
            (1 << 8) | 3,
            0,
            0,
        ],
    );
    tally.ok(got, "the check's device could not be made")?;
    let suid = open(page, &under(b"suid"), O_CREAT | O_WRONLY | O_EXCL, 0o755)?;
    let suid = tally.done(suid, "the check's set-user-id file could not be made")?;
    close(process, suid);
    let suid_at = page.put(&under(b"suid"))?;
    let got = by_number(
        process,
        Syscall::Fchownat,
        [AT_FDCWD as u64, suid_at, 1000, 1000, 0, 0],
    );
    tally.ok(
        got,
        "the check's set-user-id file could not be given to uid 1000",
    )?;
    let got = by_number(
        process,
        Syscall::Fchmodat,
        [AT_FDCWD as u64, suid_at, 0o4755, 0, 0, 0],
    );
    tally.ok(
        got,
        "the check's set-user-id file could not be made set-user-id",
    )?;
    let ctx = crate::syscall::path::context(process);
    let (_, _, set_ids) = fs::open_program(&ctx, None, &under(b"suid"))
        .map_err(|_| "the set-user-id file would not open as a program")?;
    if set_ids.uid != Some(1000) {
        return Err("a set-user-id file on an ordinary mount did not ask for its owner's id");
    }
    // Opened for writing before the remount, and written after it.
    let early = open(page, &under(b"file"), O_WRONLY, 0)?;
    tally.done(early, "the check's file would not open for writing")
}

/// A remount of something that is not a mount's root, and one by a caller
/// that is not root.
fn refuses_a_wrong_remount(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    page.reset();
    let got = mount(page, &under(b"dir"), b"none", MS_REMOUNT | MS_RDONLY)?;
    tally.refused(
        got,
        Errno::EINVAL,
        "a remount inside a mount was not EINVAL",
    )?;
    let stranger =
        process::new_for_check().map_err(|_| "could not make a second process for the check")?;
    stranger.with_credentials(|ids| {
        for role in [&mut ids.user, &mut ids.group] {
            role.real = 1000;
            role.effective = 1000;
            role.saved = 1000;
            role.filesystem = 1000;
        }
    });
    let strangers = memory::sys_mmap(
        &stranger,
        &MmapRequest {
            addr: 0,
            len: STAGED + READ_ROOM,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "the second process's page was refused")? as u64;
    let mut theirs = Page {
        process: &stranger,
        base: strangers,
        next: 0,
    };
    let got = mount(&mut theirs, MOUNT_POINT, b"none", MS_REMOUNT | MS_RDONLY)?;
    tally.refused(got, Errno::EPERM, "a remount by uid 1000 was not EPERM")
}

/// `nosuid` alone, by a plain remount that makes the filesystem writable
/// again, then read-only alone by `MS_REMOUNT | MS_BIND`, as bubblewrap asks.
fn one_flag_at_a_time(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let process = page.process;
    let ctx = crate::syscall::path::context(process);
    // `nosuid` alone: the program runs, without its set-user-id bit. A plain
    // remount, since the read-only one before it was plain and made the
    // filesystem itself read-only, which only a plain remount undoes (a
    // bind remount changes the one mount's flags: `docs/NAMESPACES.md`, N2).
    page.reset();
    let got = mount(page, MOUNT_POINT, b"none", MS_REMOUNT | MS_NOSUID)?;
    tally.ok(got, "a plain remount nosuid was refused")?;
    let (_, _, set_ids) = fs::open_program(&ctx, None, &under(b"suid"))
        .map_err(|_| "a program on a nosuid mount would not open")?;
    if set_ids != fs::SetIds::NONE {
        return Err("a set-user-id bit counted on a nosuid mount");
    }
    shows_the_flags(page, tally, "rw,nosuid,noatime")?;

    // Read-only alone, by `MS_REMOUNT | MS_BIND` as bubblewrap asks: the
    // device opens for writing, and the set-user-id bit counts again.
    page.reset();
    let got = mount(page, MOUNT_POINT, b"none", MS_REMOUNT | MS_BIND | MS_RDONLY)?;
    tally.ok(got, "a bind remount read-only was refused")?;
    let device = open(page, &under(b"null"), O_WRONLY, 0)?;
    let device = tally.done(
        device,
        "a device on a read-only mount would not open for writing",
    )?;
    close(process, device);
    let dir = page.put(&under(b"dir"))?;
    let got = by_number(
        process,
        Syscall::Mkdirat,
        [AT_FDCWD as u64, dir, 0o755, 0, 0, 0],
    );
    tally.refused(
        got,
        Errno::EEXIST,
        "an existing name on a read-only mount was not EEXIST",
    )?;
    let (_, _, set_ids) = fs::open_program(&ctx, None, &under(b"suid"))
        .map_err(|_| "a program on a read-only mount would not open")?;
    if set_ids.uid != Some(1000) {
        return Err("a set-user-id bit was ignored on a mount without nosuid");
    }
    shows_the_flags(page, tally, "ro,noatime")
}

/// Read-write again: everything back, and the check's files removed.
fn read_write_again(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let process = page.process;
    page.reset();
    let got = mount(page, MOUNT_POINT, b"none", MS_REMOUNT)?;
    tally.ok(got, "the remount read-write was refused")?;
    let again = page.put(&under(b"again"))?;
    let got = by_number(
        process,
        Syscall::Mkdirat,
        [AT_FDCWD as u64, again, 0o755, 0, 0, 0],
    );
    tally.ok(
        got,
        "a directory could not be made after the remount read-write",
    )?;
    let got = by_number(
        process,
        Syscall::Unlinkat,
        [AT_FDCWD as u64, again, u64::from(AT_REMOVEDIR), 0, 0, 0],
    );
    tally.ok(got, "the directory made after the remount would not go")?;
    for name in [&b"file"[..], b"null", b"suid"] {
        let at = page.put(&under(name))?;
        let got = by_number(
            process,
            Syscall::Unlinkat,
            [AT_FDCWD as u64, at, 0, 0, 0, 0],
        );
        tally.ok(got, "a file of the check's would not go")?;
    }
    let dir = page.put(&under(b"dir"))?;
    let got = by_number(
        process,
        Syscall::Unlinkat,
        [AT_FDCWD as u64, dir, u64::from(AT_REMOVEDIR), 0, 0, 0],
    );
    tally.ok(got, "the check's directory would not go")?;
    Ok(())
}

/// Through the `ro,nosuid,nodev,noexec` mount: every change to a name
/// `EROFS`.
fn changes_are_erofs(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let process = page.process;
    page.reset();
    let got = open(page, &under(b"file"), O_WRONLY, 0)?;
    tally.refused(
        got,
        Errno::EROFS,
        "a file on a read-only mount opened for writing",
    )?;
    let got = open(page, &under(b"new"), O_CREAT | O_WRONLY, 0o644)?;
    tally.refused(got, Errno::EROFS, "a file was made on a read-only mount")?;
    let new = page.put(&under(b"new"))?;
    let got = by_number(
        process,
        Syscall::Mkdirat,
        [AT_FDCWD as u64, new, 0o755, 0, 0, 0],
    );
    tally.refused(
        got,
        Errno::EROFS,
        "a directory was made on a read-only mount",
    )?;
    let target = page.put(b"file")?;
    let got = by_number(
        process,
        Syscall::Symlinkat,
        [target, AT_FDCWD as u64, new, 0, 0, 0],
    );
    tally.refused(got, Errno::EROFS, "a link was made on a read-only mount")?;
    let file = page.put(&under(b"file"))?;
    let got = by_number(
        process,
        Syscall::Unlinkat,
        [AT_FDCWD as u64, file, 0, 0, 0, 0],
    );
    tally.refused(
        got,
        Errno::EROFS,
        "a file was removed from a read-only mount",
    )?;
    let got = by_number(
        process,
        Syscall::Unlinkat,
        [AT_FDCWD as u64, new, 0, 0, 0, 0],
    );
    tally.refused(
        got,
        Errno::EROFS,
        "a missing name on a read-only mount was not EROFS",
    )?;
    let got = by_number(
        process,
        Syscall::Renameat2,
        [AT_FDCWD as u64, file, AT_FDCWD as u64, new, 0, 0],
    );
    tally.refused(got, Errno::EROFS, "a file was renamed on a read-only mount")
}

/// Through the read-only mount: every change to a file's attributes
/// `EROFS`.
fn attributes_are_erofs(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let process = page.process;
    page.reset();
    let file = page.put(&under(b"file"))?;
    let got = by_number(
        process,
        Syscall::Fchmodat,
        [AT_FDCWD as u64, file, 0o600, 0, 0, 0],
    );
    tally.refused(
        got,
        Errno::EROFS,
        "a file's mode changed on a read-only mount",
    )?;
    let got = by_number(
        process,
        Syscall::Fchownat,
        [AT_FDCWD as u64, file, 0, 0, 0, 0],
    );
    tally.refused(
        got,
        Errno::EROFS,
        "a file's owner changed on a read-only mount",
    )?;
    let got = by_number(
        process,
        Syscall::Utimensat,
        [AT_FDCWD as u64, file, 0, 0, 0, 0],
    );
    tally.refused(
        got,
        Errno::EROFS,
        "a file's times changed on a read-only mount",
    )?;
    let got = by_number(process, Syscall::Truncate, [file, 0, 0, 0, 0, 0]);
    tally.refused(
        got,
        Errno::EROFS,
        "a file was truncated on a read-only mount",
    )?;
    let name = page.put(b"user.check")?;
    let got = by_number(process, Syscall::Setxattr, [file, name, name, 1, 0, 0]);
    tally.refused(
        got,
        Errno::EROFS,
        "an attribute was set on a read-only mount",
    )
}

/// Through the `nodev,noexec` mount: the device `EACCES`, the file neither
/// run nor mapped executable, and reading still allowed.
fn no_devices_no_programs(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let process = page.process;
    page.reset();
    let got = open(page, &under(b"null"), O_RDONLY, 0)?;
    tally.refused(got, Errno::EACCES, "a device on a nodev mount opened")?;
    let ctx = crate::syscall::path::context(process);
    tally.refused(
        fs::open_program(&ctx, None, &under(b"file")).map(|_| 0),
        Errno::EACCES,
        "a program on a noexec mount would run",
    )?;
    match fs::open_program(&ctx, None, &under(b"suid")) {
        Err(Errno::EACCES) => {}
        _ => return Err("a set-user-id program on a noexec mount would run"),
    }

    // Read, then the mappings.
    let reader = open(page, &under(b"file"), O_RDONLY, 0)?;
    let reader = tally.done(
        reader,
        "a file on a read-only mount would not open for reading",
    )?;
    let map = |prot: u32| {
        memory::sys_mmap(
            process,
            &MmapRequest {
                addr: 0,
                len: 4096,
                prot,
                flags: MAP_PRIVATE,
                fd: reader as i64,
                offset: 0,
                unit: OffsetUnit::Bytes,
            },
        )
    };
    tally.refused(
        map(PROT_READ | PROT_EXEC),
        Errno::EPERM,
        "a file on a noexec mount was mapped executable",
    )?;
    let mapped = tally.done(
        map(PROT_READ),
        "a file on a noexec mount could not be mapped to read",
    )?;
    tally.refused(
        memory::sys_mprotect(process, mapped as u64, 4096, PROT_READ | PROT_EXEC),
        Errno::EACCES,
        "a mapping of a file on a noexec mount was made executable",
    )?;
    let _ = by_number(process, Syscall::Munmap, [mapped as u64, 4096, 0, 0, 0, 0]);
    close(process, reader);
    Ok(())
}

/// `mountinfo`, `mounts` and `statfs` show `options` for the check's mount,
/// and `mountinfo` names it as its `/proc/<pid>/fd` link does.
fn shows_the_flags(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    options: &str,
) -> Result<(), &'static str> {
    let process = page.process;
    page.reset();
    // bubblewrap's comparison: an `O_PATH` descriptor's link, read back.
    let held = open(page, MOUNT_POINT, O_PATH, 0)?;
    let held = tally.done(held, "the mount point would not open O_PATH")?;
    let pid = process.pid();
    let link = page.put(alloc::format!("/proc/{pid}/fd/{held}").as_bytes())?;
    let got = by_number(
        process,
        Syscall::Readlinkat,
        [AT_FDCWD as u64, link, page.buffer(), READ_ROOM, 0, 0],
    );
    let len = tally.done(got, "the mount point's descriptor link would not read")?;
    let named = page.read_back(len)?;
    close(process, held);
    if named != MOUNT_POINT {
        return Err("an O_PATH descriptor's link did not name the mount point");
    }

    let info = read_whole(
        page,
        tally,
        alloc::format!("/proc/{pid}/mountinfo").as_bytes(),
    )?;
    let found = info.split(|&byte| byte == b'\n').find_map(|line| {
        let fields: Vec<&[u8]> = line.split(|&byte| byte == b' ').collect();
        (fields.get(4).copied() == Some(MOUNT_POINT)).then_some(fields)
    });
    let Some(fields) = found else {
        return Err("mountinfo has no line for the check's mount");
    };
    if fields.get(3).copied() != Some(&b"/"[..])
        || fields.get(5).copied() != Some(options.as_bytes())
        || fields.get(6).copied() != Some(&b"-"[..])
        || fields.get(7).copied() != Some(&b"tmpfs"[..])
    {
        return Err("mountinfo's line for the check's mount has the wrong root, options or type");
    }
    let mounts = read_whole(page, tally, alloc::format!("/proc/{pid}/mounts").as_bytes())?;
    let mut line = b"tmpfs ".to_vec();
    line.extend_from_slice(MOUNT_POINT);
    line.extend_from_slice(b" tmpfs ");
    line.extend_from_slice(options.as_bytes());
    line.extend_from_slice(b" 0 0");
    if !mounts
        .split(|&byte| byte == b'\n')
        .any(|shown| shown == line.as_slice())
    {
        return Err("/proc/<pid>/mounts does not show the check's mount with its flags");
    }

    // `statfs`'s `f_flags`, at the offset this architecture's layout has.
    page.reset();
    let at = page.put(MOUNT_POINT)?;
    let got = fsctl::sys_statfs(process, at, page.buffer());
    tally.ok(got, "statfs of the check's mount failed")?;
    let flags = if size_of::<usize>() == 8 {
        let bytes = page.read_back(size_of::<Statfs>())?;
        let at = offset_of!(Statfs, f_flags);
        bytes
            .get(at..at + 8)
            .and_then(|word| word.try_into().ok())
            .map_or(0, u64::from_le_bytes)
    } else {
        let bytes = page.read_back(size_of::<ArmStatfs>())?;
        let at = offset_of!(ArmStatfs, f_flags);
        bytes
            .get(at..at + 4)
            .and_then(|word| word.try_into().ok())
            .map_or(0, |word| u64::from(u32::from_le_bytes(word)))
    };
    let mut wanted = ST_VALID;
    for (name, bit) in [
        ("ro", ST_RDONLY),
        ("nosuid", ST_NOSUID),
        ("nodev", ST_NODEV),
        ("noexec", ST_NOEXEC),
    ] {
        if options.split(',').any(|option| option == name) {
            wanted |= bit;
        }
    }
    let shown = ST_VALID | ST_RDONLY | ST_NOSUID | ST_NODEV | ST_NOEXEC;
    if flags & shown != wanted {
        return Err("statfs's f_flags do not show the check's mount's flags");
    }
    Ok(())
}

/// The whole of a small file, read by number.
pub(super) fn read_whole(
    page: &mut Page<'_>,
    tally: &mut Tally<'_>,
    path: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let process = page.process;
    page.reset();
    let fd = open(page, path, O_RDONLY, 0)?;
    let fd = tally.done(fd, "a /proc file of the check's would not open")?;
    let mut whole = Vec::new();
    loop {
        let got = by_number(
            process,
            Syscall::Read,
            [fd as u64, page.buffer(), READ_ROOM, 0, 0, 0],
        );
        let len = got.map_err(|_| "a /proc file of the check's would not read")?;
        if len == 0 {
            break;
        }
        whole.extend_from_slice(&page.read_back(len)?);
    }
    close(process, fd);
    Ok(whole)
}
