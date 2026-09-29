//! `/proc` as two of the Steam client's helpers read it, run as init by
//! `cargo xtask test-procfs`.
//!
//! Each step prints `procfs: <step> ok`, and the program ends with
//! `procfs: all ok` and status 0, or `procfs: FAILED <step>: <what>` and
//! status 1.
//!
//! * **tcp**, **unix**, **eventfd**, **epoll**, **pipe**, **memfd**: `stat`
//!   through `/proc/self/fd/<n>` is `fstat` of the descriptor -- the same
//!   device, inode number and mode -- while `lstat` is the link itself and
//!   `readlink` names the object as Linux does. Opening the link again is
//!   `ENXIO` for a socket and an anonymous file, and a working open for a
//!   pipe and a memfd. `lsof -i`, which Steam runs to accept its web helper's
//!   websocket, finds the processes holding a TCP port exactly so: the
//!   **tcp** step also requires `/proc/net/tcp`'s row for its listening port
//!   to carry the inode number `stat` gave.
//! * **inodes**: every name under `/proc`, listed recursively with
//!   `getdents64` and `lstat`ed, has an inode number that fits 32 bits, and
//!   no two names share one; and every entry's `d_off` fits a 32-bit
//!   `off_t`. A 32-bit glibc program's `readdir` and `stat` without
//!   large-file support fail with `EOVERFLOW` on a wider number or offset,
//!   and the Steam client then lists no processes; built for `i686`, this
//!   program is one, and its calls take the kernel's 32-bit x86 paths.
//!
//! Built with `negative-fd`, the link steps `lstat` where they mean `stat`
//! and must fail on the first; with `negative-ino`, the inode step holds the
//! numbers to 16 bits and must fail; with `negative-off`, it holds the
//! offsets to 16 bits, and must fail at the first process.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use libc::c_int;

/// A step's failure.
type Step = Result<(), String>;

/// A named step.
type Named = (&'static str, fn() -> Step);

/// The widest inode number the inode step accepts: `u32::MAX`, as a 32-bit
/// `struct dirent` and `struct stat` hold, or 16 bits for the negative
/// control.
const WIDEST: u64 = if cfg!(feature = "negative-ino") {
    0xffff
} else {
    0xffff_ffff
};

/// The furthest directory offset the inode step accepts: `i32::MAX`, as a
/// 32-bit `off_t` holds, or 16 bits for the negative control. A 32-bit
/// glibc program's `readdir` fails with `EOVERFLOW` on an entry whose
/// `d_off` does not fit, as it does on a wide `d_ino`.
const FURTHEST: i64 = if cfg!(feature = "negative-off") {
    0xffff
} else {
    0x7fff_ffff
};

/// More names than the boot's `/proc` can honestly have: a walk past it does
/// not end.
const LISTING_LIMIT: usize = 20_000;

/// Print one line at once, so the serial log has it before the next step.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The last error's number.
fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A path as a C string.
fn c_path(path: &str) -> Result<CString, String> {
    CString::new(path).map_err(|_| format!("{path} has a NUL in it"))
}

/// `stat`, or `lstat` with `follow` false.
fn stat_path(path: &str, follow: bool) -> Result<libc::stat, String> {
    let c = c_path(path)?;
    // SAFETY: an all-zero `struct stat` is a valid value of it.
    let mut out: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `out` is a `struct stat` to fill.
    let status = unsafe {
        if follow {
            libc::stat(c.as_ptr(), &raw mut out)
        } else {
            libc::lstat(c.as_ptr(), &raw mut out)
        }
    };
    if status != 0 {
        let how = if follow { "stat" } else { "lstat" };
        return Err(format!("{how} {path} failed with errno {}", errno()));
    }
    Ok(out)
}

/// `fstat`.
fn stat_fd(fd: RawFd) -> Result<libc::stat, String> {
    // SAFETY: an all-zero `struct stat` is a valid value of it.
    let mut out: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `out` is a `struct stat` to fill.
    if unsafe { libc::fstat(fd, &raw mut out) } != 0 {
        return Err(format!(
            "fstat of descriptor {fd} failed with errno {}",
            errno()
        ));
    }
    Ok(out)
}

/// How a descriptor's link must open again: refused with `ENXIO`, or opened
/// with these flags.
#[derive(Debug, Clone, Copy)]
enum Reopen {
    /// `ENXIO`, as Linux's `sock_no_open` and `no_open` answer.
    Refused,
    /// Opened, with these `open` flags.
    Opens(c_int),
}

/// The link checks for descriptor `fd`: `stat` through the link is its
/// `fstat`, of file type `kind`; `lstat` is a link; `readlink` is `name`; and
/// the link opens again as `reopen` says. Returns `stat`'s inode number.
fn check_link(fd: RawFd, kind: libc::mode_t, name: &str, reopen: Reopen) -> Result<u64, String> {
    let link = format!("/proc/self/fd/{fd}");
    let described = stat_fd(fd)?;
    let followed = stat_path(&link, !cfg!(feature = "negative-fd"))?;
    if followed.st_dev != described.st_dev
        || followed.st_ino != described.st_ino
        || followed.st_mode != described.st_mode
    {
        return Err(format!(
            "stat through {link} is not fstat: device {:#x} inode {} mode {:o}, where fstat says \
             device {:#x} inode {} mode {:o}",
            followed.st_dev,
            followed.st_ino,
            followed.st_mode,
            described.st_dev,
            described.st_ino,
            described.st_mode
        ));
    }
    if described.st_mode & libc::S_IFMT != kind {
        return Err(format!(
            "fstat's mode {:o} is not type {kind:o}",
            described.st_mode
        ));
    }
    let itself = stat_path(&link, false)?;
    if itself.st_mode & libc::S_IFMT != libc::S_IFLNK {
        return Err(format!(
            "lstat of {link} is not a link: mode {:o}",
            itself.st_mode
        ));
    }
    let target = std::fs::read_link(&link).map_err(|error| format!("readlink {link}: {error}"))?;
    if target.as_os_str().as_encoded_bytes() != name.as_bytes() {
        return Err(format!(
            "readlink {link} is {}, not {name}",
            target.display()
        ));
    }
    let c = c_path(&link)?;
    let flags = match reopen {
        Reopen::Refused => libc::O_RDONLY,
        Reopen::Opens(flags) => flags,
    };
    // SAFETY: `c` is NUL-terminated.
    let opened = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC) };
    let why = errno();
    if opened >= 0 {
        // SAFETY: a descriptor this call just opened and nothing else owns.
        drop(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    match reopen {
        Reopen::Refused if opened >= 0 => Err(format!("{link} opened again; Linux refuses it")),
        Reopen::Refused if why != libc::ENXIO => Err(format!(
            "opening {link} again failed with errno {why}, not ENXIO"
        )),
        Reopen::Opens(_) if opened < 0 => Err(format!("{link} did not open again: errno {why}")),
        _ => Ok(described.st_ino),
    }
}

/// A descriptor from a call that returns one or -1.
fn owned(fd: c_int, what: &str) -> Result<OwnedFd, String> {
    if fd < 0 {
        return Err(format!("{what} failed with errno {}", errno()));
    }
    // SAFETY: a descriptor the call just made and nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A listening TCP socket's link, and its `/proc/net/tcp` row: the row for
/// its port carries the inode number `stat` through the link gives, which is
/// how `lsof -i TCP@127.0.0.1:<port>` finds the process.
fn tcp() -> Step {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|error| format!("bind 127.0.0.1:0: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("getsockname: {error}"))?
        .port();
    let fd = listener.as_raw_fd();
    let ino = stat_fd(fd)?.st_ino;
    let _ = check_link(
        fd,
        libc::S_IFSOCK,
        &format!("socket:[{ino}]"),
        Reopen::Refused,
    )?;
    let table = std::fs::read_to_string("/proc/net/tcp")
        .map_err(|error| format!("/proc/net/tcp: {error}"))?;
    let local = format!("0100007F:{port:04X}");
    let row = table
        .lines()
        .skip(1)
        .find(|row| row.split_whitespace().nth(1) == Some(local.as_str()))
        .ok_or_else(|| format!("/proc/net/tcp has no row for {local}"))?;
    let column = row
        .split_whitespace()
        .nth(9)
        .ok_or_else(|| format!("/proc/net/tcp's row for {local} has no inode column"))?;
    if column != ino.to_string() {
        return Err(format!(
            "/proc/net/tcp's row for {local} has inode {column}, where stat through the link \
             says {ino}"
        ));
    }
    Ok(())
}

/// A connected pair of `AF_UNIX` stream sockets.
fn unix() -> Step {
    let mut pair = [-1; 2];
    // SAFETY: `pair` has room for the two descriptors.
    let status =
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) };
    let first = owned(if status == 0 { pair[0] } else { -1 }, "socketpair")?;
    let _second = owned(pair[1], "socketpair")?;
    let fd = first.as_raw_fd();
    let ino = stat_fd(fd)?.st_ino;
    check_link(
        fd,
        libc::S_IFSOCK,
        &format!("socket:[{ino}]"),
        Reopen::Refused,
    )
    .map(drop)
}

/// An eventfd, which Linux shows as a regular file on `anon_inodefs`.
fn eventfd() -> Step {
    // SAFETY: no pointers.
    let fd = owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) }, "eventfd")?;
    check_link(
        fd.as_raw_fd(),
        libc::S_IFREG,
        "anon_inode:[eventfd]",
        Reopen::Refused,
    )
    .map(drop)
}

/// An epoll set.
fn epoll() -> Step {
    // SAFETY: no pointers.
    let fd = owned(
        unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) },
        "epoll_create1",
    )?;
    check_link(
        fd.as_raw_fd(),
        libc::S_IFREG,
        "anon_inode:[eventpoll]",
        Reopen::Refused,
    )
    .map(drop)
}

/// A pipe's read end, which opens again as a new end of the same pipe.
fn pipe() -> Step {
    let mut ends = [-1; 2];
    // SAFETY: `ends` has room for the two descriptors.
    let status = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) };
    let read = owned(if status == 0 { ends[0] } else { -1 }, "pipe2")?;
    let _write = owned(ends[1], "pipe2")?;
    let fd = read.as_raw_fd();
    let ino = stat_fd(fd)?.st_ino;
    let reopen = Reopen::Opens(libc::O_RDONLY | libc::O_NONBLOCK);
    check_link(fd, libc::S_IFIFO, &format!("pipe:[{ino}]"), reopen).map(drop)
}

/// A memfd, which opens again as the same file.
fn memfd() -> Step {
    let name = c_path("procfs-test")?;
    // SAFETY: `name` is NUL-terminated.
    let fd = owned(
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) },
        "memfd_create",
    )?;
    check_link(
        fd.as_raw_fd(),
        libc::S_IFREG,
        "memfd:procfs-test",
        Reopen::Opens(libc::O_RDWR),
    )
    .map(drop)
}

/// A directory's entries through `getdents64`: name, `d_ino`, `d_type` and
/// `d_off`, `.` and `..` left out.
fn list(dir: &str) -> Result<Vec<(String, u64, u8, i64)>, String> {
    let c = c_path(dir)?;
    // SAFETY: `c` is NUL-terminated.
    let fd = owned(
        unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        },
        &format!("open {dir}"),
    )?;
    let mut entries = Vec::new();
    let mut buf = vec![0_u8; 4096];
    loop {
        // SAFETY: `buf` is writable for its length.
        let got = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                fd.as_raw_fd(),
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        if got < 0 {
            return Err(format!("getdents64 of {dir} failed with errno {}", errno()));
        }
        let got = usize::try_from(got).unwrap_or(0);
        if got == 0 {
            return Ok(entries);
        }
        let mut at = 0;
        while at < got {
            // struct linux_dirent64: d_ino u64, d_off i64, d_reclen u16,
            // d_type u8, then the NUL-terminated name.
            let field = |from: usize, len: usize| buf.get(at + from..at + from + len);
            let ino = field(0, 8)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u64::from_ne_bytes);
            let off = field(8, 8)
                .and_then(|bytes| bytes.try_into().ok())
                .map(i64::from_ne_bytes);
            let reclen = field(16, 2)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u16::from_ne_bytes);
            let kind = field(18, 1).and_then(|bytes| bytes.first().copied());
            let (Some(ino), Some(off), Some(reclen), Some(kind)) = (ino, off, reclen, kind) else {
                return Err(format!("getdents64 of {dir} returned a short record"));
            };
            let reclen = usize::from(reclen);
            let name = buf.get(at + 19..at + reclen).unwrap_or_default();
            let name = name.split(|&byte| byte == 0).next().unwrap_or_default();
            let name = String::from_utf8_lossy(name).into_owned();
            if reclen == 0 {
                return Err(format!("getdents64 of {dir} returned a record of length 0"));
            }
            at += reclen;
            if name != "." && name != ".." {
                entries.push((name, ino, kind, off));
            }
        }
    }
}

/// Every name under `/proc` has a 32-bit inode number of its own, as
/// `getdents64` lists it and as `lstat` reports it.
///
/// A second thread is running meanwhile, so `/proc/self/task` has one that
/// is not the first; and a few more descriptors are open than the three a
/// program starts with.
fn inodes() -> Step {
    let (stop, parked) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let _ = parked.recv();
    });
    let _listener = TcpListener::bind("127.0.0.1:0");
    let result = walk_proc();
    let _ = stop.send(());
    let _ = thread.join();
    let (listed, widest) = result?;
    say(&format!(
        "procfs: {listed} names under /proc, the widest inode number {widest:#x}"
    ));
    Ok(())
}

/// See [`inodes`]: the names listed, and the widest number seen.
fn walk_proc() -> Result<(usize, u64), String> {
    let mut seen: BTreeMap<u64, String> = BTreeMap::new();
    let mut pending = vec![String::from("/proc")];
    let mut widest = 0;
    while let Some(dir) = pending.pop() {
        // A process that ended between its listing and this is not a fault;
        // `/proc` itself not listing is.
        let entries = match list(&dir) {
            Ok(entries) => entries,
            Err(_) if dir != "/proc" => continue,
            Err(why) => return Err(why),
        };
        for (name, ino, kind, off) in entries {
            let path = format!("{dir}/{name}");
            if ino > WIDEST {
                return Err(format!(
                    "getdents64 lists {path} with inode {ino:#x}, wider than {WIDEST:#x}"
                ));
            }
            if !(0..=FURTHEST).contains(&off) {
                return Err(format!(
                    "getdents64 lists {path} with offset {off:#x}, past {FURTHEST:#x}"
                ));
            }
            let Ok(stat) = stat_path(&path, false) else {
                continue;
            };
            if stat.st_ino != ino {
                return Err(format!(
                    "{path} is listed with inode {ino} and lstats as {}",
                    stat.st_ino
                ));
            }
            if let Some(other) = seen.insert(ino, path.clone()) {
                return Err(format!("{path} and {other} share inode {ino:#x}"));
            }
            widest = widest.max(ino);
            if seen.len() > LISTING_LIMIT {
                return Err(String::from("listing /proc recursively does not end"));
            }
            if kind == libc::DT_DIR {
                pending.push(path);
            }
        }
    }
    Ok((seen.len(), widest))
}

fn main() {
    let steps: [Named; 7] = [
        ("tcp", tcp),
        ("unix", unix),
        ("eventfd", eventfd),
        ("epoll", epoll),
        ("pipe", pipe),
        ("memfd", memfd),
        ("inodes", inodes),
    ];
    for (name, step) in steps {
        if let Err(what) = step() {
            say(&format!("procfs: FAILED {name}: {what}"));
            std::process::exit(1);
        }
        say(&format!("procfs: {name} ok"));
    }
    say("procfs: all ok");
}
