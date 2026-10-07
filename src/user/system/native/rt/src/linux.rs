//! The Linux calls a native program may make, for the few that need them.
//!
//! A native program is not a POSIX one and has no libc, no descriptors of its
//! own and no `std`. But the kernel gives *every* process a descriptor table
//! and a namespace (`Process::with_pid` in `src/kernel/src/syscall/process.rs`),
//! and `dispatch` picks the ABI by the number's range and by nothing else --
//! so a native program that wants a socket may simply ask for one. This
//! module is that: the handful of calls `src/user/system/native/drivers/console/vport` needs to put a virtio
//! port on a Unix socket, and no more.
//!
//! `docs/CLIPBOARD.md` §5 is why this exists. It is deliberately not a libc:
//! a call is added here when a program in this tree needs it, with the
//! numbers taken from `ferrix_linux_abi::nr`, which pins them against the
//! kernel's own tables rather than remembering them.
//!
//! An app (`docs/APPS.md`) does not add calls here: it changes nothing
//! outside its own folder. It makes any call by number with [`call`], and
//! [`numbers`] is the running architecture's table to take the number from.
//!
//! # Errors
//!
//! Every call returns `Result<usize, Errno>`: the kernel leaves `-errno` in
//! `-4095..=-1`, exactly as Linux does, and [`decode`] separates the two.

use ferrix_linux_abi::errno::Errno;

pub use crate::arch::numbers;
use crate::arch::{self, nr};

/// The Linux call `number`, with `args`, the unused ones zero: what an app
/// makes a call with that this module has no function for.
///
/// `number` comes from [`numbers`], whose values are this architecture's, so
/// a program spelling `numbers::OPENAT` makes the same call on all three.
/// What differs between them beyond the number -- a structure's width on
/// ARMv7-A, a flag's value -- is the caller's to know.
///
/// # Safety
///
/// The caller is answerable for every pointer in `args`: each must be valid
/// for whatever the call `number` does with it, for the whole of the call,
/// as the kernel's own Linux ABI defines it.
///
/// # Errors
///
/// Whatever the kernel answers.
pub unsafe fn call(number: usize, args: [usize; 6]) -> Result<usize, Errno> {
    // SAFETY: the caller's promise about `args`, forwarded unchanged; the
    // trap itself touches no memory but what the call names.
    decode(unsafe { arch::linux(number, args) })
}

/// `AF_UNIX`, the only family this module has a use for.
pub const AF_UNIX: usize = 1;
/// `SOCK_STREAM`.
pub const SOCK_STREAM: usize = 1;
/// `SOCK_NONBLOCK`, as a flag on `socket` and `accept4`.
pub const SOCK_NONBLOCK: usize = 0o4000;
/// `F_SETFL`.
pub const F_SETFL: usize = 4;
/// `O_NONBLOCK`.
pub const O_NONBLOCK: usize = 0o4000;

/// `CLOCK_MONOTONIC`, the clock a native [`Deadline`] is measured against.
///
/// [`Deadline`]: ferrix_native::handle::Deadline
pub const CLOCK_MONOTONIC: usize = 1;

/// `SOL_SOCKET`, the level of the options every socket has.
pub const SOL_SOCKET: usize = 1;
/// `SO_PEERCRED`: the pid, uid and gid of the peer as it connected.
pub const SO_PEERCRED: usize = 17;
/// `SO_RCVTIMEO` (`SO_RCVTIMEO_OLD`): how long a read waits, as a `timeval`
/// of the architecture's `long`.
pub const SO_RCVTIMEO: usize = 20;

/// Bytes of a `sockaddr_un`: the family, then the path.
pub const SOCKADDR_UN_BYTES: usize = 110;

/// The longest path a Unix socket address carries, with room for its NUL.
pub const PATH_MAX: usize = SOCKADDR_UN_BYTES - 2 - 1;

/// A `sockaddr_un` for `path`, and its length, or `None` if the path is too
/// long to be one.
///
/// The address is `AF_UNIX` as a little-endian `u16`, then the path, then a
/// NUL -- the layout every Linux architecture shares.
#[must_use]
pub fn sockaddr_un(path: &[u8]) -> Option<([u8; SOCKADDR_UN_BYTES], usize)> {
    if path.is_empty() || path.len() > PATH_MAX {
        return None;
    }
    let mut address = [0_u8; SOCKADDR_UN_BYTES];
    let family = (AF_UNIX as u16).to_le_bytes();
    *address.first_mut()? = family[0];
    *address.get_mut(1)? = family[1];
    address.get_mut(2..2 + path.len())?.copy_from_slice(path);
    // The length Linux wants is the family, the path and its NUL.
    Some((address, 2 + path.len() + 1))
}

/// A result register as the kernel left it.
///
/// # Errors
///
/// The `Errno` a value in `-4095..=-1` names.
pub fn decode(value: usize) -> Result<usize, Errno> {
    // The same window `src/lib/proto/native`'s decode uses, and Linux's own.
    let signed = value as isize;
    if (-4095..0).contains(&signed) {
        Err(Errno(u16::try_from(-signed).unwrap_or(0)))
    } else {
        Ok(value)
    }
}

/// `socket(family, kind, protocol)`.
///
/// # Errors
///
/// Whatever the kernel answers.
pub fn socket(family: usize, kind: usize, protocol: usize) -> Result<usize, Errno> {
    // SAFETY: no pointer arguments.
    decode(unsafe { arch::linux(nr::SOCKET, [family, kind, protocol, 0, 0, 0]) })
}

/// `bind(fd, address, len)`.
///
/// # Errors
///
/// Whatever the kernel answers.
pub fn bind(fd: usize, address: &[u8], len: usize) -> Result<usize, Errno> {
    let at = address.as_ptr().addr();
    // SAFETY: `address` is borrowed for the call and is at least `len` bytes,
    // which the caller and `sockaddr_un` together guarantee; the kernel only
    // reads it.
    decode(unsafe { arch::linux(nr::BIND, [fd, at, len.min(address.len()), 0, 0, 0]) })
}

/// A `sockaddr_un` for the abstract `name`, and its length, or `None` if the
/// name is empty or too long to be one.
///
/// An abstract name is a `sun_path` that starts with a NUL. It is in no
/// directory, so it is the same name whatever root the process that binds it
/// or the one that connects has; its length is exactly the family, the NUL
/// and the name, since the name may itself hold NULs.
#[must_use]
pub fn sockaddr_un_abstract(name: &[u8]) -> Option<([u8; SOCKADDR_UN_BYTES], usize)> {
    if name.is_empty() || name.len() > SOCKADDR_UN_BYTES - 3 {
        return None;
    }
    let mut address = [0_u8; SOCKADDR_UN_BYTES];
    let family = (AF_UNIX as u16).to_le_bytes();
    *address.first_mut()? = family[0];
    *address.get_mut(1)? = family[1];
    // `address[2]` stays the NUL that makes the name abstract.
    address.get_mut(3..3 + name.len())?.copy_from_slice(name);
    Some((address, 3 + name.len()))
}

/// The uid of the process at the other end of the connected Unix socket
/// `fd`, as it was when it connected: `getsockopt(SO_PEERCRED)`.
///
/// # Errors
///
/// Whatever the kernel answers.
pub fn peer_uid(fd: usize) -> Result<u32, Errno> {
    // `struct ucred`: pid, uid, gid, each 32 bits.
    let mut credentials = [0_u32; 3];
    let mut len: u32 = 12;
    let at = credentials.as_mut_ptr().addr();
    let len_at = (&raw mut len).addr();
    // SAFETY: `credentials` is 12 bytes borrowed exclusively for the call, and
    // `len` says so; the kernel writes at most that many and then `len`.
    let _written = decode(unsafe {
        arch::linux(nr::GETSOCKOPT, [fd, SOL_SOCKET, SO_PEERCRED, at, len_at, 0])
    })?;
    Ok(credentials[1])
}

/// `setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO)`: a read of `fd` that waits
/// longer than `seconds` fails with `EAGAIN`. `drvupdated` keeps a client
/// that stops sending from holding it (`docs/DEVMGR.md` §4.1).
///
/// # Errors
///
/// Whatever the kernel answers.
pub fn receive_timeout(fd: usize, seconds: usize) -> Result<usize, Errno> {
    // `struct timeval`: seconds and microseconds, each the width of `long`,
    // which is `usize`'s on every architecture Ferrix runs.
    let timeval = [seconds, 0_usize];
    let at = timeval.as_ptr().addr();
    let len = size_of_val(&timeval);
    // SAFETY: `timeval` is `len` bytes borrowed for the call; the kernel
    // only reads it.
    decode(unsafe { arch::linux(nr::SETSOCKOPT, [fd, SOL_SOCKET, SO_RCVTIMEO, at, len, 0]) })
}

/// `listen(fd, backlog)`.
///
/// # Errors
///
/// Whatever the kernel answers.
pub fn listen(fd: usize, backlog: usize) -> Result<usize, Errno> {
    // SAFETY: no pointer arguments.
    decode(unsafe { arch::linux(nr::LISTEN, [fd, backlog, 0, 0, 0, 0]) })
}

/// `accept4(fd, NULL, NULL, flags)`: the peer's address is not asked for,
/// since a Unix socket's is empty.
///
/// # Errors
///
/// Whatever the kernel answers; `EAGAIN` on a non-blocking socket with
/// nobody waiting.
pub fn accept4(fd: usize, flags: usize) -> Result<usize, Errno> {
    // SAFETY: the two address arguments are null, which `accept4` defines as
    // "do not report the peer".
    decode(unsafe { arch::linux(nr::ACCEPT4, [fd, 0, 0, flags, 0, 0]) })
}

/// `read(fd, bytes)`.
///
/// # Errors
///
/// Whatever the kernel answers; `EAGAIN` when nothing is waiting.
pub fn read(fd: usize, bytes: &mut [u8]) -> Result<usize, Errno> {
    let at = bytes.as_mut_ptr().addr();
    let len = bytes.len();
    // SAFETY: `bytes` is borrowed exclusively for the call and is `len`
    // bytes; the kernel writes at most that many.
    decode(unsafe { arch::linux(nr::READ, [fd, at, len, 0, 0, 0]) })
}

/// `write(fd, bytes)`.
///
/// # Errors
///
/// Whatever the kernel answers; `EAGAIN` when the pipe is full.
pub fn write(fd: usize, bytes: &[u8]) -> Result<usize, Errno> {
    let at = bytes.as_ptr().addr();
    // SAFETY: `bytes` is borrowed for the call and is at least its own
    // length; the kernel only reads it.
    decode(unsafe { arch::linux(nr::WRITE, [fd, at, bytes.len(), 0, 0, 0]) })
}

/// `close(fd)`.
///
/// # Errors
///
/// Whatever the kernel answers.
pub fn close(fd: usize) -> Result<usize, Errno> {
    // SAFETY: no pointer arguments.
    decode(unsafe { arch::linux(nr::CLOSE, [fd, 0, 0, 0, 0, 0]) })
}

/// `reboot(LINUX_REBOOT_CMD_RESTART)`: restart the machine, after the kernel
/// has committed its disks. Returns only when it did not.
///
/// # Errors
///
/// `EPERM` for a process without the privilege, and whatever else the
/// kernel answers.
pub fn reboot_restart() -> Result<usize, Errno> {
    // `linux/reboot.h`: the two magic numbers, and `LINUX_REBOOT_CMD_RESTART`.
    const MAGIC1: usize = 0xFEE1_DEAD;
    const MAGIC2: usize = 0x2812_1969;
    const RESTART: usize = 0x0123_4567;
    // SAFETY: no pointer arguments.
    decode(unsafe { arch::linux(nr::REBOOT, [MAGIC1, MAGIC2, RESTART, 0, 0, 0]) })
}

/// `fcntl(fd, command, argument)`.
///
/// # Errors
///
/// Whatever the kernel answers.
pub fn fcntl(fd: usize, command: usize, argument: usize) -> Result<usize, Errno> {
    // SAFETY: no pointer arguments for the commands this module uses.
    decode(unsafe { arch::linux(nr::FCNTL, [fd, command, argument, 0, 0, 0]) })
}

/// Take `path` out of the filesystem, so that binding it again succeeds.
///
/// Which of `unlink` and `unlinkat` spells this is the architecture's, and is
/// settled behind `crate::arch` so that nothing here has to ask.
///
/// # Errors
///
/// Whatever the kernel answers, `ENOENT` included, which a caller clearing
/// the way for a `bind` should ignore.
pub fn unlink(path: &[u8]) -> Result<usize, Errno> {
    // SAFETY: `path` is borrowed for the call and NUL-terminated by its
    // caller; the kernel only reads it.
    let result = unsafe { arch::unlink(path.as_ptr().addr()) };
    decode(result)
}

/// Nanoseconds on `CLOCK_MONOTONIC`, which is the clock a native
/// `Deadline::At` names.
///
/// This is here so that a driver waiting on a native port can say "for the
/// next ten milliseconds" -- the kernel takes only absolute deadlines, so a
/// relative wait is this plus the interval.
///
/// # Errors
///
/// Whatever the kernel answers. A `timespec` is two words of the
/// architecture's width, so the reading itself is `crate::arch`'s.
pub fn monotonic_nanos() -> Result<u64, Errno> {
    let mut nanos = 0_u64;
    let _read = decode(arch::monotonic_nanos(&mut nanos))?;
    Ok(nanos)
}
