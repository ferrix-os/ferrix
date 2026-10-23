//! The Linux calls lode makes, by number through `ferrix_rt::linux::call`
//! (an app's way: nothing is added to the runtime).
//!
//! Where the three architectures differ beyond the number -- ARMv7-A's
//! 32-bit uid calls and `mmap2` -- it is said here and nowhere else. Every
//! structure passed is one whose layout is the same on all three: a
//! `timespec` of two machine words, `pollfd`, `linux_dirent64`, `utsname`.

use alloc::vec::Vec;

use ferrix_rt::linux::{call, numbers};

use crate::arch;

/// An error number.
pub(crate) use ferrix_linux_abi::errno::Errno;

/// `-100`, the working directory, as `openat` takes it.
pub(crate) const AT_FDCWD: usize = -100_isize as usize;
/// `O_RDONLY`.
pub(crate) const O_RDONLY: usize = 0;
/// `O_WRONLY`.
pub(crate) const O_WRONLY: usize = 1;
/// `O_CREAT`.
pub(crate) const O_CREAT: usize = 0o100;
/// `O_EXCL`.
pub(crate) const O_EXCL: usize = 0o200;
/// `O_TRUNC`.
pub(crate) const O_TRUNC: usize = 0o1000;
/// `O_APPEND`.
pub(crate) const O_APPEND: usize = 0o2000;
/// `O_NONBLOCK`.
pub(crate) const O_NONBLOCK: usize = 0o4000;
/// `O_CLOEXEC`.
pub(crate) const O_CLOEXEC: usize = 0o2_000_000;
/// `EPIPE`.
pub(crate) const EPIPE: u16 = 32;
/// `EINVAL`.
pub(crate) const EINVAL: u16 = 22;
/// `EEXIST`.
pub(crate) const EEXIST: u16 = 17;
/// `EACCES`.
pub(crate) const EACCES: u16 = 13;
/// `EINTR`.
pub(crate) const EINTR: u16 = 4;
/// `POLLIN`.
const POLLIN: i16 = 1;

/// `path` with its terminating NUL.
fn c_path(path: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(path.len() + 1);
    bytes.extend_from_slice(path);
    bytes.push(0);
    bytes
}

/// `openat(dir, path, flags, mode)`.
pub(crate) fn openat(dir: usize, path: &[u8], flags: usize, mode: usize) -> Result<usize, Errno> {
    let path = c_path(path);
    // SAFETY: `path` is a NUL-terminated buffer that lives for the call.
    unsafe {
        call(
            numbers::OPENAT,
            [dir, path.as_ptr() as usize, flags, mode, 0, 0],
        )
    }
}

/// `open(path, flags)` from the working directory.
pub(crate) fn open(path: &[u8], flags: usize) -> Result<usize, Errno> {
    openat(AT_FDCWD, path, flags | O_CLOEXEC, 0)
}

/// `mkdirat(dir, path, mode)`; one that is there already is not an error.
pub(crate) fn mkdirat(dir: usize, path: &[u8], mode: usize) -> Result<(), Errno> {
    let path = c_path(path);
    // SAFETY: `path` is a NUL-terminated buffer that lives for the call.
    match unsafe {
        call(
            numbers::MKDIRAT,
            [dir, path.as_ptr() as usize, mode, 0, 0, 0],
        )
    } {
        Ok(_) => Ok(()),
        Err(error) if error.0 == EEXIST => Ok(()),
        Err(error) => Err(error),
    }
}

/// `unlinkat(dir, path, 0)`.
pub(crate) fn unlinkat(dir: usize, path: &[u8]) -> Result<(), Errno> {
    let path = c_path(path);
    // SAFETY: `path` is a NUL-terminated buffer that lives for the call.
    unsafe { call(numbers::UNLINKAT, [dir, path.as_ptr() as usize, 0, 0, 0, 0]) }.map(|_| ())
}

/// `renameat(dir, from, dir, to)`.
pub(crate) fn renameat(dir: usize, from: &[u8], to: &[u8]) -> Result<(), Errno> {
    let from = c_path(from);
    let to = c_path(to);
    // SAFETY: both are NUL-terminated buffers that live for the call.
    unsafe {
        call(
            numbers::RENAMEAT,
            [dir, from.as_ptr() as usize, dir, to.as_ptr() as usize, 0, 0],
        )
    }
    .map(|_| ())
}

/// `read(fd, buf)`.
pub(crate) fn read(fd: usize, buf: &mut [u8]) -> Result<usize, Errno> {
    // SAFETY: `buf` is writable for its whole length for the call.
    unsafe {
        call(
            numbers::READ,
            [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
        )
    }
}

/// `write(fd, buf)`, once.
pub(crate) fn write(fd: usize, buf: &[u8]) -> Result<usize, Errno> {
    // SAFETY: `buf` is readable for its whole length for the call.
    unsafe {
        call(
            numbers::WRITE,
            [fd, buf.as_ptr() as usize, buf.len(), 0, 0, 0],
        )
    }
}

/// All of `buf` to `fd`, going on after a short write or an interruption.
pub(crate) fn write_all(fd: usize, mut buf: &[u8]) -> Result<(), Errno> {
    while !buf.is_empty() {
        match write(fd, buf) {
            Ok(0) => return Err(Errno(5)),
            Ok(done) => buf = buf.get(done..).unwrap_or_default(),
            Err(error) if error.0 == EINTR => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// `close(fd)`.
pub(crate) fn close(fd: usize) {
    // SAFETY: no pointer arguments.
    let _ = unsafe { call(numbers::CLOSE, [fd, 0, 0, 0, 0, 0]) };
}

/// `lseek(fd, offset, whence)`.
pub(crate) fn lseek(fd: usize, offset: usize, whence: usize) -> Result<usize, Errno> {
    // SAFETY: no pointer arguments.
    unsafe { call(numbers::LSEEK, [fd, offset, whence, 0, 0, 0]) }
}

/// `SEEK_SET`.
pub(crate) const SEEK_SET: usize = 0;
/// `SEEK_END`.
pub(crate) const SEEK_END: usize = 2;

/// `ftruncate(fd, length)`.
pub(crate) fn ftruncate(fd: usize, length: usize) -> Result<(), Errno> {
    // SAFETY: no pointer arguments.
    unsafe { call(numbers::FTRUNCATE, [fd, length, 0, 0, 0, 0]) }.map(|_| ())
}

/// `fdatasync(fd)`.
pub(crate) fn fdatasync(fd: usize) -> Result<(), Errno> {
    // SAFETY: no pointer arguments.
    unsafe { call(numbers::FDATASYNC, [fd, 0, 0, 0, 0, 0]) }.map(|_| ())
}

/// `fchmod(fd, mode)`.
pub(crate) fn fchmod(fd: usize, mode: usize) -> Result<(), Errno> {
    // SAFETY: no pointer arguments.
    unsafe { call(numbers::FCHMOD, [fd, mode, 0, 0, 0, 0]) }.map(|_| ())
}

/// `fchown(fd, uid, gid)`.
pub(crate) fn fchown(fd: usize, uid: u32, gid: u32) -> Result<(), Errno> {
    let number = arch::FCHOWN;
    // SAFETY: no pointer arguments.
    unsafe { call(number, [fd, uid as usize, gid as usize, 0, 0, 0]) }.map(|_| ())
}

/// Become `uid` and `gid` with no supplementary groups, for good.
pub(crate) fn become_ids(uid: u32, gid: u32) -> Result<(), Errno> {
    let (groups, resgid, resuid) = (arch::SETGROUPS, arch::SETRESGID, arch::SETRESUID);
    let (uid, gid) = (uid as usize, gid as usize);
    // SAFETY: an empty list: no pointer is read.
    let _ = unsafe { call(groups, [0, 0, 0, 0, 0, 0]) }?;
    // SAFETY: no pointer arguments.
    let _ = unsafe { call(resgid, [gid, gid, gid, 0, 0, 0]) }?;
    // SAFETY: no pointer arguments.
    unsafe { call(resuid, [uid, uid, uid, 0, 0, 0]) }.map(|_| ())
}

/// `CLOCK_REALTIME`.
pub(crate) const CLOCK_REALTIME: usize = 0;
/// `CLOCK_MONOTONIC`.
pub(crate) const CLOCK_MONOTONIC: usize = 1;

/// A clock, in microseconds.
#[must_use]
pub(crate) fn clock_us(clock: usize) -> u64 {
    let mut spec = [0_isize; 2];
    // SAFETY: `spec` is a `timespec` of two machine words, writable for the
    // call, on every architecture lode is built for.
    let read = unsafe {
        call(
            numbers::CLOCK_GETTIME,
            [clock, spec.as_mut_ptr() as usize, 0, 0, 0, 0],
        )
    };
    if read.is_err() {
        return 0;
    }
    let [seconds, nanos] = spec;
    (seconds.max(0) as u64) * 1_000_000 + (nanos.max(0) as u64) / 1000
}

/// Sleep `millis` milliseconds.
pub(crate) fn sleep_ms(millis: u64) {
    let spec = [
        isize::try_from(millis / 1000).unwrap_or(0),
        isize::try_from((millis % 1000) * 1_000_000).unwrap_or(0),
    ];
    // SAFETY: `spec` is a `timespec`, read for the call; no remainder.
    let _ = unsafe { call(numbers::NANOSLEEP, [spec.as_ptr() as usize, 0, 0, 0, 0, 0]) };
}

/// `getrandom(buf, 0)`, all of it.
pub(crate) fn random(buf: &mut [u8]) -> Result<(), Errno> {
    let mut done = 0;
    while done < buf.len() {
        let rest = buf.get_mut(done..).unwrap_or_default();
        // SAFETY: `rest` is writable for its whole length for the call.
        let got = unsafe {
            call(
                numbers::GETRANDOM,
                [rest.as_mut_ptr() as usize, rest.len(), 0, 0, 0, 0],
            )
        }?;
        done += got;
    }
    Ok(())
}

/// The machine's node name, `uname`'s second field.
#[must_use]
pub(crate) fn hostname() -> Vec<u8> {
    let mut buf = [0_u8; 6 * 65];
    // SAFETY: `buf` is a `struct utsname` (six 65-byte fields), writable.
    if unsafe { call(numbers::UNAME, [buf.as_mut_ptr() as usize, 0, 0, 0, 0, 0]) }.is_err() {
        return Vec::new();
    }
    let field = buf.get(65..130).unwrap_or_default();
    let end = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    Vec::from(field.get(..end).unwrap_or_default())
}

/// Every name in the directory open on `fd`.
pub(crate) fn list_dir(fd: usize) -> Result<Vec<Vec<u8>>, Errno> {
    let mut names = Vec::new();
    let mut buf = alloc::vec![0_u8; 16 * 1024];
    loop {
        // SAFETY: `buf` is writable for its whole length for the call.
        let got = unsafe {
            call(
                numbers::GETDENTS64,
                [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
            )
        }?;
        if got == 0 {
            return Ok(names);
        }
        let mut at = 0;
        while at + 19 <= got {
            // linux_dirent64: u64 ino, i64 off, u16 reclen, u8 type, name.
            let reclen = buf
                .get(at + 16..at + 18)
                .and_then(|bytes| bytes.try_into().ok())
                .map_or(0, |bytes: [u8; 2]| usize::from(u16::from_ne_bytes(bytes)));
            if reclen == 0 {
                return Ok(names);
            }
            let name = buf.get(at + 19..at + reclen).unwrap_or_default();
            let end = name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(name.len());
            names.push(Vec::from(name.get(..end).unwrap_or_default()));
            at += reclen;
        }
    }
}

/// A whole file's bytes, from `offset` on.
pub(crate) fn read_file_from(dir: usize, name: &[u8], offset: usize) -> Result<Vec<u8>, Errno> {
    let fd = openat(dir, name, O_RDONLY | O_CLOEXEC, 0)?;
    let read = read_fd_from(fd, offset);
    close(fd);
    read
}

/// What `fd` holds from `offset` to its end.
pub(crate) fn read_fd_from(fd: usize, offset: usize) -> Result<Vec<u8>, Errno> {
    let _ = lseek(fd, offset, SEEK_SET)?;
    let mut bytes = Vec::new();
    let mut chunk = alloc::vec![0_u8; 64 * 1024];
    loop {
        match read(fd, &mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(got) => bytes.extend_from_slice(chunk.get(..got).unwrap_or_default()),
            Err(error) if error.0 == EINTR => {}
            Err(error) => return Err(error),
        }
    }
}

/// A small file's bytes, or none.
#[must_use]
pub(crate) fn read_small(path: &[u8]) -> Option<Vec<u8>> {
    let fd = open(path, O_RDONLY).ok()?;
    let bytes = read_fd_from(fd, 0).ok();
    close(fd);
    bytes
}

/// Map `length` bytes of fresh memory, or none.
#[must_use]
pub(crate) fn map(length: usize) -> Option<*mut u8> {
    const PROT_READ_WRITE: usize = 3;
    const MAP_PRIVATE_ANONYMOUS: usize = 0x22;
    let number = arch::MMAP;
    // SAFETY: an anonymous private mapping the kernel places; no pointer
    // of ours is read or written.
    let address = unsafe {
        call(
            number,
            [
                0,
                length,
                PROT_READ_WRITE,
                MAP_PRIVATE_ANONYMOUS,
                usize::MAX,
                0,
            ],
        )
    }
    .ok()?;
    Some(address as *mut u8)
}

/// Unmap what [`map`] gave.
pub(crate) fn unmap(address: *mut u8, length: usize) {
    let _ = ferrix_rt::linux::munmap(address as usize, length);
}

/// `getuid()`.
#[must_use]
pub(crate) fn getuid() -> u32 {
    // SAFETY: no pointer arguments.
    unsafe { call(arch::GETUID, [0; 6]) }.map_or(u32::MAX, |uid| uid as u32)
}

/// `fchmodat(AT_FDCWD, path, mode, 0)`.
pub(crate) fn chmod(path: &[u8], mode: usize) -> Result<(), Errno> {
    let path = c_path(path);
    // SAFETY: `path` is a NUL-terminated buffer that lives for the call.
    unsafe {
        call(
            numbers::FCHMODAT,
            [AT_FDCWD, path.as_ptr() as usize, mode, 0, 0, 0],
        )
    }
    .map(|_| ())
}

/// `SOCK_SEQPACKET`: a connection that keeps each write a message.
pub(crate) const SOCK_SEQPACKET: usize = 5;

/// Make `fd` non-blocking.
pub(crate) fn set_nonblocking(fd: usize) -> Result<(), Errno> {
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    // SAFETY: `fcntl` with integer arguments only.
    let flags = unsafe { call(numbers::FCNTL, [fd, F_GETFL, 0, 0, 0, 0]) }?;
    // SAFETY: as above.
    unsafe { call(numbers::FCNTL, [fd, F_SETFL, flags | O_NONBLOCK, 0, 0, 0]) }.map(|_| ())
}

/// A listening stream socket at `path`, made afresh, non-blocking.
pub(crate) fn listen_at(path: &[u8]) -> Result<usize, Errno> {
    listen_kind(path, ferrix_rt::linux::SOCK_STREAM)
}

/// A listening socket of `kind` (`SOCK_STREAM`, [`SOCK_SEQPACKET`]) at
/// `path`, made afresh, non-blocking.
pub(crate) fn listen_kind(path: &[u8], kind: usize) -> Result<usize, Errno> {
    use ferrix_rt::linux as rt;
    let _ = unlinkat(AT_FDCWD, path);
    let fd = rt::socket(rt::AF_UNIX, kind | rt::SOCK_NONBLOCK | O_CLOEXEC, 0)?;
    let Some((address, length)) = rt::sockaddr_un(path) else {
        close(fd);
        return Err(Errno(22));
    };
    let bound = rt::bind(fd, &address, length).and_then(|_| rt::listen(fd, 16));
    if let Err(error) = bound {
        close(fd);
        return Err(error);
    }
    Ok(fd)
}

/// A connection waiting on `listener`, non-blocking, if there is one.
pub(crate) fn accept(listener: usize) -> Option<usize> {
    ferrix_rt::linux::accept4(listener, ferrix_rt::linux::SOCK_NONBLOCK | O_CLOEXEC).ok()
}

/// The uid at the other end of a connected socket.
pub(crate) fn peer_uid(fd: usize) -> Option<u32> {
    ferrix_rt::linux::peer_uid(fd).ok()
}

/// Wait up to `millis` for any of `fds` to be readable.
pub(crate) fn wait_any(fds: &[usize], millis: u64) {
    // struct pollfd { int fd; short events; short revents; }, each 8 bytes.
    let mut table = alloc::vec![0_u8; fds.len() * 8];
    for (index, &fd) in fds.iter().enumerate() {
        if let Some(slot) = table.get_mut(index * 8..index * 8 + 4) {
            slot.copy_from_slice(&i32::try_from(fd).unwrap_or(-1).to_ne_bytes());
        }
        if let Some(slot) = table.get_mut(index * 8 + 4..index * 8 + 6) {
            slot.copy_from_slice(&POLLIN.to_ne_bytes());
        }
    }
    let spec = [
        isize::try_from(millis / 1000).unwrap_or(0),
        isize::try_from((millis % 1000) * 1_000_000).unwrap_or(0),
    ];
    // SAFETY: `table` is `fds.len()` `struct pollfd`s, writable for the
    // call, and `spec` a `timespec` read for it; no signal mask.
    let _ = unsafe {
        call(
            numbers::PPOLL,
            [
                table.as_mut_ptr() as usize,
                fds.len(),
                spec.as_ptr() as usize,
                0,
                0,
                0,
            ],
        )
    };
}

/// `renameat` from the working directory.
pub(crate) fn rename(from: &[u8], to: &[u8]) -> Result<(), Errno> {
    let from = c_path(from);
    let to = c_path(to);
    // SAFETY: both are NUL-terminated buffers that live for the call.
    unsafe {
        call(
            numbers::RENAMEAT,
            [
                AT_FDCWD,
                from.as_ptr() as usize,
                AT_FDCWD,
                to.as_ptr() as usize,
                0,
                0,
            ],
        )
    }
    .map(|_| ())
}

/// The pid and uid at the other end of a connected socket (`SO_PEERCRED`).
pub(crate) fn peer(fd: usize) -> Option<(u32, u32)> {
    const SOL_SOCKET: usize = 1;
    const SO_PEERCRED: usize = 17;
    // `struct ucred`: pid, uid, gid, each 32 bits.
    let mut credentials = [0_u32; 3];
    let mut length: u32 = 12;
    // SAFETY: `credentials` is 12 bytes and `length` says so; the kernel
    // writes at most that many, then the length.
    let got = unsafe {
        call(
            numbers::GETSOCKOPT,
            [
                fd,
                SOL_SOCKET,
                SO_PEERCRED,
                credentials.as_mut_ptr() as usize,
                (&raw mut length) as usize,
                0,
            ],
        )
    };
    let _ = got.ok()?;
    let [pid, uid, _] = credentials;
    Some((pid, uid))
}

/// Write what `buf` can take now, without waiting: how much was written.
pub(crate) fn write_some(fd: usize, buf: &[u8]) -> usize {
    write(fd, buf).unwrap_or(0)
}

/// `SOL_SOCKET`.
const SOL_SOCKET: usize = 1;
/// `SO_PASSCRED`.
const SO_PASSCRED: usize = 16;
/// `SCM_RIGHTS`.
const SCM_RIGHTS: i32 = 1;
/// `SCM_CREDENTIALS`.
const SCM_CREDENTIALS: i32 = 2;
/// `SOCK_DGRAM`.
const SOCK_DGRAM: usize = 2;

/// A datagram socket bound at `path`, made afresh, non-blocking, its
/// receives stamped with their senders' credentials (`SO_PASSCRED`).
pub(crate) fn datagram_at(path: &[u8]) -> Result<usize, Errno> {
    use ferrix_rt::linux as rt;
    let _ = unlinkat(AT_FDCWD, path);
    let fd = rt::socket(rt::AF_UNIX, SOCK_DGRAM | rt::SOCK_NONBLOCK | O_CLOEXEC, 0)?;
    let one: i32 = 1;
    // SAFETY: `one` is an `int`, read for the call.
    let passed = unsafe {
        call(
            numbers::SETSOCKOPT,
            [fd, SOL_SOCKET, SO_PASSCRED, (&raw const one) as usize, 4, 0],
        )
    };
    let Some((address, length)) = rt::sockaddr_un(path) else {
        close(fd);
        return Err(Errno(22));
    };
    let bound = passed.and_then(|_| rt::bind(fd, &address, length));
    if let Err(error) = bound {
        close(fd);
        return Err(error);
    }
    Ok(fd)
}

/// One datagram received.
pub(crate) struct Received {
    /// Its length.
    pub(crate) length: usize,
    /// Its sender's pid, uid and gid, as the kernel stamped them.
    pub(crate) sender: Option<(u32, u32, u32)>,
    /// A descriptor it carried, a memfd for a large record.
    pub(crate) passed: Option<usize>,
}

/// Receive one datagram into `buf` without waiting, with its credentials
/// and at most one passed descriptor (any more are closed).
pub(crate) fn receive(fd: usize, buf: &mut [u8]) -> Option<Received> {
    receive_flags(fd, buf, 0x40)
}

/// [`receive`] with `recvmsg`'s `flags`, `MSG_CMSG_CLOEXEC` added.
fn receive_flags(fd: usize, buf: &mut [u8], flags: usize) -> Option<Received> {
    let word = size_of::<usize>();
    let mut control = [0_usize; 16];
    let iov = [buf.as_mut_ptr() as usize, buf.len()];
    // struct msghdr as machine words: name, namelen, iov, iovlen, control,
    // controllen, flags -- the 32-bit fields in the low half of a word on
    // the 64-bit targets, which are little-endian.
    let mut header = [
        0,
        0,
        iov.as_ptr() as usize,
        1,
        control.as_mut_ptr() as usize,
        control.len() * word,
        0,
    ];
    const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;
    // SAFETY: `header` is a `struct msghdr` laid out in words, whose iovec
    // and control buffer live for the call; the kernel writes the lengths
    // back into it.
    let length = unsafe {
        call(
            numbers::RECVMSG,
            [
                fd,
                header.as_mut_ptr() as usize,
                flags | MSG_CMSG_CLOEXEC,
                0,
                0,
                0,
            ],
        )
    }
    .ok()?;
    let used = header
        .get(5)
        .copied()
        .unwrap_or(0)
        .min(control.len() * word);
    let bytes: Vec<u8> = control
        .iter()
        .flat_map(|w| w.to_ne_bytes())
        .take(used)
        .collect();
    let mut received = Received {
        length,
        sender: None,
        passed: None,
    };
    let mut at = 0;
    while at + word + 8 <= bytes.len() {
        let size = usize::from_ne_bytes(bytes.get(at..at + word)?.try_into().ok()?);
        let level = i32::from_ne_bytes(bytes.get(at + word..at + word + 4)?.try_into().ok()?);
        let kind = i32::from_ne_bytes(bytes.get(at + word + 4..at + word + 8)?.try_into().ok()?);
        let data = bytes.get(at + word + 8..at + size.max(word + 8))?;
        if level == SOL_SOCKET as i32 && kind == SCM_CREDENTIALS && data.len() >= 12 {
            let field = |n: usize| {
                u32::from_ne_bytes(
                    data.get(n * 4..n * 4 + 4)
                        .and_then(|b| b.try_into().ok())
                        .unwrap_or([0; 4]),
                )
            };
            received.sender = Some((field(0), field(1), field(2)));
        } else if level == SOL_SOCKET as i32 && kind == SCM_RIGHTS {
            for chunk in data.chunks_exact(4) {
                let passed = usize::try_from(i32::from_ne_bytes(chunk.try_into().ok()?)).ok()?;
                if received.passed.is_none() {
                    received.passed = Some(passed);
                } else {
                    close(passed);
                }
            }
        }
        if size == 0 {
            break;
        }
        at += size.next_multiple_of(word);
    }
    Some(received)
}

/// Send `data` on the connected `fd` with `passed`, if any, beside it
/// (`SCM_RIGHTS`), without waiting.
pub(crate) fn send_with(fd: usize, data: &[u8], passed: Option<usize>) -> Result<usize, Errno> {
    let word = size_of::<usize>();
    // One `struct cmsghdr` (a word of length, two ints) and one int, padded
    // to a word: the space CMSG_SPACE(sizeof(int)) gives.
    let mut control = [0_usize; 4];
    let mut used = 0;
    if let Some(passed) = passed {
        let length = word + 8 + 4;
        let mut bytes = [0_u8; 4 * 8];
        let mut put = |at: usize, value: &[u8]| {
            if let Some(slot) = bytes.get_mut(at..at + value.len()) {
                slot.copy_from_slice(value);
            }
        };
        put(0, &length.to_ne_bytes());
        put(word, &(SOL_SOCKET as i32).to_ne_bytes());
        put(word + 4, &SCM_RIGHTS.to_ne_bytes());
        put(word + 8, &i32::try_from(passed).unwrap_or(-1).to_ne_bytes());
        for (index, slot) in control.iter_mut().enumerate() {
            let mut chunk = [0_u8; size_of::<usize>()];
            if let Some(from) = bytes.get(index * word..(index + 1) * word) {
                chunk.copy_from_slice(from);
            }
            *slot = usize::from_ne_bytes(chunk);
        }
        used = length.next_multiple_of(word);
    }
    let iov = [data.as_ptr() as usize, data.len()];
    let header = [
        0,
        0,
        iov.as_ptr() as usize,
        1,
        if used > 0 {
            control.as_ptr() as usize
        } else {
            0
        },
        used,
        0,
    ];
    const MSG_DONTWAIT: usize = 0x40;
    const MSG_NOSIGNAL: usize = 0x4000;
    // SAFETY: `header` is a `struct msghdr` laid out in words, whose iovec
    // and control buffer live for the call and are only read.
    unsafe {
        call(
            numbers::SENDMSG,
            [
                fd,
                header.as_ptr() as usize,
                MSG_DONTWAIT | MSG_NOSIGNAL,
                0,
                0,
                0,
            ],
        )
    }
}

/// Receive one message into `buf`, waiting for it, with at most one passed
/// descriptor (any more are closed).
pub(crate) fn receive_waiting(fd: usize, buf: &mut [u8]) -> Option<Received> {
    receive_flags(fd, buf, 0)
}

/// The pid, uid and gid at the other end of a connected socket.
pub(crate) fn peer_all(fd: usize) -> Option<(u32, u32, u32)> {
    let (pid, uid) = peer(fd)?;
    let mut credentials = [0_u32; 3];
    let mut length: u32 = 12;
    const SO_PEERCRED: usize = 17;
    // SAFETY: as in `peer`: 12 bytes, and the length says so.
    let _ = unsafe {
        call(
            numbers::GETSOCKOPT,
            [
                fd,
                SOL_SOCKET,
                SO_PEERCRED,
                credentials.as_mut_ptr() as usize,
                (&raw mut length) as usize,
                0,
            ],
        )
    };
    let [_, _, gid] = credentials;
    Some((pid, uid, gid))
}

/// `symlinkat(target, AT_FDCWD, link)`.
pub(crate) fn symlink(target: &[u8], link: &[u8]) -> Result<(), Errno> {
    let target = c_path(target);
    let link = c_path(link);
    // SAFETY: both are NUL-terminated buffers that live for the call.
    unsafe {
        call(
            numbers::SYMLINKAT,
            [
                target.as_ptr() as usize,
                AT_FDCWD,
                link.as_ptr() as usize,
                0,
                0,
                0,
            ],
        )
    }
    .map(|_| ())
}

/// `AF_INET`.
const AF_INET: usize = 2;

/// A `sockaddr_in` for `address`:`port`.
fn sockaddr_in(address: [u8; 4], port: u16) -> [u8; 16] {
    let mut out = [0_u8; 16];
    out[..2].copy_from_slice(&u16::try_from(AF_INET).unwrap_or(2).to_ne_bytes());
    out[2..4].copy_from_slice(&port.to_be_bytes());
    out[4..8].copy_from_slice(&address);
    out
}

/// A listening TCP socket on `127.0.0.1`:`port`, blocking.
pub(crate) fn tcp_listen(port: u16) -> Result<usize, Errno> {
    use ferrix_rt::linux as rt;
    let fd = rt::socket(AF_INET, rt::SOCK_STREAM | O_CLOEXEC, 0)?;
    let sockaddr = sockaddr_in([127, 0, 0, 1], port);
    let bound = rt::bind(fd, &sockaddr, sockaddr.len()).and_then(|_| rt::listen(fd, 16));
    if let Err(error) = bound {
        close(fd);
        return Err(error);
    }
    Ok(fd)
}

/// The next connection on a blocking `listener`.
pub(crate) fn accept_blocking(listener: usize) -> Result<usize, Errno> {
    ferrix_rt::linux::accept4(listener, O_CLOEXEC)
}
