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
//! * **mincore**: what Chromium's memory dumps count a mapping's resident
//!   bytes with. Over four written pages it answers 0 and a byte per page
//!   with the resident bit set; with the third page unmapped it is `ENOMEM`,
//!   having written the first two bytes and left the rest; an address off a
//!   page boundary is `EINVAL`, a vector at address 0 `EFAULT`, and a length
//!   of 0 answers 0.
//! * **debug** (`docs/DEBUG.md` §1): a thread blocked in `read` of a pipe,
//!   read from outside it through `/proc/self/task/<tid>/syscall`, `wchan`
//!   and `stack`; a uid 1000 child refused them but `wchan`, which reads
//!   `0`; and `/proc/self/environ`, the environment block in the program's memory.
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

/// `mincore` over four pages, then over the same range with a hole in it.
fn mincore() -> Step {
    const PAGES: usize = 4;
    let page = 4096;
    // SAFETY: a fresh private anonymous mapping, unmapped before returning.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGES * page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(format!("mmap: errno {}", errno()));
    }
    let at = at.cast::<u8>();
    let outcome = (|| {
        for index in 0..PAGES {
            // SAFETY: inside the mapping, which is writable.
            unsafe { at.add(index * page).write(1) };
        }
        let mut vec = [0xEE_u8; PAGES];
        // SAFETY: `vec` has a byte for each page.
        let got = unsafe { libc::mincore(at.cast(), PAGES * page, vec.as_mut_ptr()) };
        if got != 0 || vec.iter().any(|&byte| byte & 1 == 0) {
            return Err(format!(
                "over four written pages: {got} {vec:?}, errno {}",
                errno()
            ));
        }
        // SAFETY: the third page of the mapping.
        if unsafe { libc::munmap(at.add(2 * page).cast(), page) } != 0 {
            return Err(format!("munmap: errno {}", errno()));
        }
        let mut vec = [0xEE_u8; PAGES];
        // SAFETY: as above.
        let got = unsafe { libc::mincore(at.cast(), PAGES * page, vec.as_mut_ptr()) };
        if got != -1 || errno() != libc::ENOMEM || vec != [1, 1, 0xEE, 0xEE] {
            return Err(format!("over a hole: {got} {vec:?}, errno {}", errno()));
        }
        // SAFETY: as above; the call must refuse before writing.
        let got = unsafe { libc::mincore(at.add(1).cast(), page, vec.as_mut_ptr()) };
        if got != -1 || errno() != libc::EINVAL {
            return Err(format!("off a page boundary: {got}, errno {}", errno()));
        }
        // SAFETY: the kernel writes nothing through a vector at 0.
        let got = unsafe { libc::mincore(at.cast(), page, std::ptr::null_mut()) };
        if got != -1 || errno() != libc::EFAULT {
            return Err(format!("a vector at 0: {got}, errno {}", errno()));
        }
        // SAFETY: no bytes.
        let got = unsafe { libc::mincore(at.cast(), 0, vec.as_mut_ptr()) };
        if got != 0 {
            return Err(format!("no bytes: {got}, errno {}", errno()));
        }
        Ok(())
    })();
    // SAFETY: the mapping made above; its unmapped page is skipped.
    unsafe {
        let _ = libc::munmap(at.cast(), 2 * page);
        let _ = libc::munmap(at.add(3 * page).cast(), page);
    }
    outcome
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

/// What the blocked thread of the debug step reads into, so that its
/// address is known to the step reading `syscall`.
static mut DEBUG_BUFFER: [u8; DEBUG_LEN] = [0; DEBUG_LEN];

/// How much the blocked thread asks `read` for.
const DEBUG_LEN: usize = 77;

/// A whole `/proc` file, or its errno.
fn read_proc(path: &str) -> Result<Vec<u8>, c_int> {
    std::fs::read(path).map_err(|error| error.raw_os_error().unwrap_or(0))
}

/// The child that reads its parent's files as uid 1000: `wchan` must be
/// `0`, and `syscall`, `stack` and `environ` refused `EACCES`.
fn refused_child(parent: i32, tid: i32) -> ! {
    // SAFETY: plain calls; the child is single-threaded after `fork`.
    let dropped = unsafe { libc::setgid(1000) == 0 && libc::setuid(1000) == 0 };
    if !dropped {
        std::process::exit(10);
    }
    let base = format!("/proc/{parent}/task/{tid}");
    let code = if read_proc(&format!("{base}/wchan")).as_deref() != Ok(&b"0"[..]) {
        11
    } else if read_proc(&format!("{base}/syscall")) != Err(libc::EACCES) {
        12
    } else if read_proc(&format!("{base}/stack")) != Err(libc::EACCES) {
        13
    } else if read_proc(&format!("/proc/{parent}/environ")) != Err(libc::EACCES) {
        14
    } else {
        0
    };
    std::process::exit(code)
}

/// Wait for child `pid` and return its exit status, or what stopped it.
fn status_of(pid: i32) -> Result<c_int, String> {
    let mut status: c_int = 0;
    // SAFETY: `status` is an int to fill.
    if unsafe { libc::waitpid(pid, &raw mut status, 0) } != pid {
        return Err(format!("waitpid failed with errno {}", errno()));
    }
    if libc::WIFEXITED(status) {
        Ok(libc::WEXITSTATUS(status))
    } else {
        Err(format!("child ended by status {status:#x}"))
    }
}

/// **debug** (`docs/DEBUG.md` §1): a thread blocked in `read` of a pipe, read
/// from outside it. Its `wchan` names the call and the pipe; its `syscall`
/// is `read`'s number, the descriptor, the buffer and the length, and a
/// stack and instruction pointer; the reader's own `syscall` is `running`;
/// its `stack` is a line per frame; a uid 1000 child reads `wchan` as `0` and
/// is refused the rest; and `/proc/self/environ` is the program's own
/// environment block, read from its memory.
fn debug() -> Step {
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` holds the two descriptors `pipe` makes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe failed with errno {}", errno()));
    }
    let [read_end, write_end] = fds;
    let tid = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
    let told = std::sync::Arc::clone(&tid);
    let address = (&raw mut DEBUG_BUFFER).cast::<u8>() as usize;
    let blocked = std::thread::spawn(move || {
        // SAFETY: `gettid` has no arguments.
        let own = unsafe { libc::gettid() };
        told.store(own, std::sync::atomic::Ordering::Release);
        // SAFETY: the buffer is a static of `DEBUG_LEN` bytes only this thread
        // writes; the step reads its address, never its bytes.
        unsafe { libc::read(read_end, address as *mut libc::c_void, DEBUG_LEN) }
    });
    let result = debug_while_blocked(&tid, read_end, address);
    // SAFETY: one byte from a static, to the pipe the thread reads.
    let _ = unsafe { libc::write(write_end, c"x".as_ptr().cast(), 1) };
    let _ = blocked.join();
    // SAFETY: the step's own descriptors, closed once.
    unsafe {
        let _ = libc::close(read_end);
        let _ = libc::close(write_end);
    }
    result
}

/// The looks of [`debug`], while its thread is blocked.
fn debug_while_blocked(
    tid: &std::sync::atomic::AtomicI32,
    read_end: c_int,
    address: usize,
) -> Step {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let tid = loop {
        let tid = tid.load(std::sync::atomic::Ordering::Acquire);
        if tid != 0 {
            break tid;
        }
        if std::time::Instant::now() > deadline {
            return Err("the blocked thread never started".into());
        }
        std::thread::yield_now();
    };
    let base = format!("/proc/self/task/{tid}");
    if !CALLS_VISIBLE {
        return debug_without_calls(&base, tid);
    }
    // Until the thread is in its call: a bounded poll of `wchan`.
    let wchan = loop {
        let wchan = read_proc(&format!("{base}/wchan"))
            .map_err(|errno| format!("{base}/wchan: errno {errno}"))?;
        if wchan.starts_with(b"read:") {
            break wchan;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "wchan: the blocked thread's wchan stayed {:?}",
                String::from_utf8_lossy(&wchan)
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    if !wchan.starts_with(b"read:pipe:[") {
        return Err(format!(
            "wchan: {:?} does not name the pipe",
            String::from_utf8_lossy(&wchan)
        ));
    }
    let syscall =
        read_proc(&format!("{base}/syscall")).map_err(|errno| format!("syscall: errno {errno}"))?;
    let text = String::from_utf8_lossy(&syscall);
    let fields: Vec<&str> = text.split_whitespace().collect();
    let number = fields.first().and_then(|n| n.parse::<i64>().ok());
    let hex = |index: usize| {
        fields
            .get(index)
            .and_then(|field| field.strip_prefix("0x"))
            .and_then(|digits| u64::from_str_radix(digits, 16).ok())
    };
    let want = [
        Some(read_end as u64),
        Some(address as u64),
        Some(DEBUG_LEN as u64),
    ];
    if fields.len() != 9
        || number != Some(libc::SYS_read as i64)
        || [hex(1), hex(2), hex(3)] != want
        || hex(8).is_none_or(|pc| pc == 0)
        || hex(7).is_none()
    {
        return Err(format!(
            "syscall: read {text:?}, not read({read_end}, {address:#x}, {DEBUG_LEN}) as number {}",
            libc::SYS_read
        ));
    }
    // SAFETY: `gettid` has no arguments.
    let own = unsafe { libc::gettid() };
    let mine = read_proc(&format!("/proc/self/task/{own}/syscall"))
        .map_err(|errno| format!("syscall: own errno {errno}"))?;
    if mine != b"running\n" {
        return Err(format!(
            "syscall: the reader's own read {:?}, not running",
            String::from_utf8_lossy(&mine)
        ));
    }
    let stack =
        read_proc(&format!("{base}/stack")).map_err(|errno| format!("stack: errno {errno}"))?;
    if !is_a_chain(&stack) {
        return Err(format!(
            "stack: {:?} is not a line per frame",
            String::from_utf8_lossy(&stack)
        ));
    }
    refused_and_environ(tid)
}

/// Whether `stack` is two or more `[<0>] 0x<address>` lines.
fn is_a_chain(stack: &[u8]) -> bool {
    let stack = String::from_utf8_lossy(stack);
    let lines: Vec<&str> = stack.lines().collect();
    lines.len() >= 2
        && lines.iter().all(|line| {
            line.strip_prefix("[<0>] 0x")
                .is_some_and(|digits| u64::from_str_radix(digits, 16).is_ok())
        })
}

/// Whether the kernel reads a blocked thread's call on this architecture:
/// x86-64's entry frame is at a fixed place, the Arm architectures' not yet
/// (`docs/DEBUG.md` §1.1), where `syscall` is `EOPNOTSUPP` and `wchan` `0`.
const CALLS_VISIBLE: bool = cfg!(any(target_arch = "x86_64", target_arch = "x86"));

/// [`debug_while_blocked`] where calls are not read: the thread's `stack`
/// polled until it is a chain, `syscall` refused `EOPNOTSUPP`, `wchan` `0`.
fn debug_without_calls(base: &str, tid: i32) -> Step {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let stack =
            read_proc(&format!("{base}/stack")).map_err(|errno| format!("stack: errno {errno}"))?;
        if is_a_chain(&stack) {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "stack: the blocked thread's stack stayed {:?}",
                String::from_utf8_lossy(&stack)
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if read_proc(&format!("{base}/syscall")) != Err(libc::EOPNOTSUPP) {
        return Err("syscall: not EOPNOTSUPP where calls are not read".into());
    }
    if read_proc(&format!("{base}/wchan")).as_deref() != Ok(&b"0"[..]) {
        return Err("wchan: not 0 where calls are not read".into());
    }
    refused_and_environ(tid)
}

/// The refused reader and `environ`, after the thread's own files.
fn refused_and_environ(tid: i32) -> Step {
    // A reader who may not look.
    // SAFETY: plain calls.
    let parent = unsafe { libc::getpid() };
    // SAFETY: `fork` in a program whose other thread is blocked in `read`;
    // the child calls only what it needs and `exit`s.
    let child = unsafe { libc::fork() };
    if child == 0 {
        refused_child(parent, tid);
    }
    match status_of(child)? {
        0 => {}
        code => return Err(format!("refused: the uid 1000 child exited {code}")),
    }
    environ_reads_memory()
}

/// `/proc/self/environ` is the environment block in the program's own memory:
/// the strings `environ` points at, each with its terminator, and a byte
/// changed in place there is read back changed, as Linux reads
/// `env_start..env_end` rather than a copy.
fn environ_reads_memory() -> Step {
    unsafe extern "C" {
        static mut environ: *mut *mut libc::c_char;
    }
    // SAFETY: the C library's `environ`, NULL-terminated, which nothing in
    // this program changes; read before any `setenv`.
    let first = unsafe { *(&raw const environ).read() };
    if first.is_null() {
        return Err("environ: this program was started with no environment".into());
    }
    let mut want = Vec::new();
    for (key, value) in std::env::vars_os() {
        want.extend_from_slice(key.as_encoded_bytes());
        want.push(b'=');
        want.extend_from_slice(value.as_encoded_bytes());
        want.push(0);
    }
    let got = read_proc("/proc/self/environ").map_err(|errno| format!("environ: errno {errno}"))?;
    if got != want {
        return Err(format!(
            "environ: read {:?}, not {:?}",
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want)
        ));
    }
    // SAFETY: the first environment string's first byte, in this program's
    // own stack, written and put back while nothing else reads it.
    let changed = unsafe {
        let byte = first.cast::<u8>();
        let old = byte.read();
        byte.write(b'#');
        let changed = read_proc("/proc/self/environ");
        byte.write(old);
        changed
    };
    match changed {
        Ok(changed) if changed.first() == Some(&b'#') && changed.get(1..) == want.get(1..) => {
            Ok(())
        }
        other => Err(format!(
            "environ: a byte changed in place read back as {:?}",
            other.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        )),
    }
}

fn main() {
    let steps: [Named; 9] = [
        ("tcp", tcp),
        ("unix", unix),
        ("eventfd", eventfd),
        ("epoll", epoll),
        ("pipe", pipe),
        ("memfd", memfd),
        ("inodes", inodes),
        ("mincore", mincore),
        ("debug", debug),
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
