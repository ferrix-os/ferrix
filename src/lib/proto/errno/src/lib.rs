//! Linux's error numbers and the negative-return convention.
//!
//! **The numbers here are Linux's.** The kernel's native calls travel as the
//! same numbers (`ferrix_native_abi::status` gives each one it uses a native
//! meaning), so that a program's C library prints something a person can act
//! on.
//!
//! # Why a crate of its own
//!
//! The kernel's core and item, and the native ABI, answer with these numbers
//! whether or not the kernel is built with the Linux personality. As
//! `ferrix-linux-abi`'s `errno` module they kept that whole crate in a kernel
//! built without the Linux ABI. Here they are a crate with no dependency, and
//! `ferrix_linux_abi::errno` and `ferrix_vfs::Errno` re-export these same
//! items: one type under three paths, never a second type or a conversion
//! (`docs/microkernel/CORE-WITHOUT-EXTENSIONS.md` E2, ledger 9212 Q2.1). The
//! struct and its constants are as they were in that module.
//!
//! `no_std`, no dependency, no `unsafe`.
//!
//! # The convention
//!
//! A Linux system call has one return register and no separate error channel.
//! Failure is signalled by returning `-errno`, and the C library turns that
//! back into `-1` plus a thread-local `errno`. The kernel side of the bargain
//! is therefore purely arithmetic, and [`encode`] is the one place this crate
//! performs it.
//!
//! The values are the generic ones from `include/uapi/asm-generic/errno-base.h`
//! and `errno.h`. Both architectures Ferrix targets use them unchanged; only
//! the MIPS and Alpha families renumber, and neither is a target here.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

/// A Linux error number, as a positive value.
///
/// Stored positive because that is how the number is written everywhere except
/// in the return register; [`Errno::as_return_value`] performs the single
/// negation on the way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Errno(
    /// The positive error number, for example `2` for [`Errno::ENOENT`].
    pub u16,
);

impl Errno {
    /// Operation not permitted.
    pub const EPERM: Self = Self(1);
    /// No such file or directory.
    pub const ENOENT: Self = Self(2);
    /// No such process.
    pub const ESRCH: Self = Self(3);
    /// Interrupted system call.
    pub const EINTR: Self = Self(4);
    /// Input/output error.
    pub const EIO: Self = Self(5);
    /// No such device or address.
    pub const ENXIO: Self = Self(6);
    /// Argument list too long.
    pub const E2BIG: Self = Self(7);
    /// Executable format error.
    pub const ENOEXEC: Self = Self(8);
    /// Bad file descriptor.
    pub const EBADF: Self = Self(9);
    /// No child processes.
    pub const ECHILD: Self = Self(10);
    /// Resource temporarily unavailable; also `EWOULDBLOCK` on Linux.
    pub const EAGAIN: Self = Self(11);
    /// Cannot allocate memory.
    pub const ENOMEM: Self = Self(12);
    /// Permission denied.
    pub const EACCES: Self = Self(13);
    /// Bad address: a pointer argument was not accessible.
    pub const EFAULT: Self = Self(14);
    /// Block device required.
    pub const ENOTBLK: Self = Self(15);
    /// Device or resource busy.
    pub const EBUSY: Self = Self(16);
    /// File exists.
    pub const EEXIST: Self = Self(17);
    /// Invalid cross-device link.
    pub const EXDEV: Self = Self(18);
    /// No such device.
    pub const ENODEV: Self = Self(19);
    /// Not a directory.
    pub const ENOTDIR: Self = Self(20);
    /// Is a directory.
    pub const EISDIR: Self = Self(21);
    /// Invalid argument.
    pub const EINVAL: Self = Self(22);
    /// Too many open files in system.
    pub const ENFILE: Self = Self(23);
    /// Too many open files in this process.
    pub const EMFILE: Self = Self(24);
    /// Inappropriate ioctl for device.
    pub const ENOTTY: Self = Self(25);
    /// Text file busy.
    pub const ETXTBSY: Self = Self(26);
    /// File too large.
    pub const EFBIG: Self = Self(27);
    /// No space left on device.
    pub const ENOSPC: Self = Self(28);
    /// Illegal seek: the file descriptor is a pipe or socket.
    pub const ESPIPE: Self = Self(29);
    /// Read-only file system.
    pub const EROFS: Self = Self(30);
    /// Too many links.
    pub const EMLINK: Self = Self(31);
    /// Broken pipe.
    pub const EPIPE: Self = Self(32);
    /// Numerical argument out of domain.
    pub const EDOM: Self = Self(33);
    /// Numerical result out of range.
    pub const ERANGE: Self = Self(34);
    /// Resource deadlock avoided.
    pub const EDEADLK: Self = Self(35);
    /// File name too long.
    pub const ENAMETOOLONG: Self = Self(36);
    /// No locks available.
    pub const ENOLCK: Self = Self(37);
    /// Function not implemented; what an unknown system call number returns.
    pub const ENOSYS: Self = Self(38);
    /// Directory not empty.
    pub const ENOTEMPTY: Self = Self(39);
    /// Too many levels of symbolic links.
    pub const ELOOP: Self = Self(40);
    /// No message of the desired type.
    pub const ENOMSG: Self = Self(42);
    /// Identifier removed.
    pub const EIDRM: Self = Self(43);
    /// No data available: what asking for an extended attribute a file does
    /// not have is told.
    pub const ENODATA: Self = Self(61);
    /// Value too large for defined data type.
    pub const EOVERFLOW: Self = Self(75);
    /// File descriptor in bad state: what a PCM stream answers a request its
    /// state does not allow, and every request once its card is gone.
    pub const EBADFD: Self = Self(77);
    /// Streams pipe error: what a PCM stream answers while its device is
    /// suspended.
    pub const ESTRPIPE: Self = Self(86);
    /// Too many users: what making a user namespace past the nesting limit
    /// answers.
    pub const EUSERS: Self = Self(87);
    /// Socket operation on non-socket.
    pub const ENOTSOCK: Self = Self(88);
    /// Destination address required.
    pub const EDESTADDRREQ: Self = Self(89);
    /// Message too long.
    pub const EMSGSIZE: Self = Self(90);
    /// Protocol wrong type for socket.
    pub const EPROTOTYPE: Self = Self(91);
    /// Protocol not available.
    pub const ENOPROTOOPT: Self = Self(92);
    /// Protocol not supported.
    pub const EPROTONOSUPPORT: Self = Self(93);
    /// Socket type not supported.
    pub const ESOCKTNOSUPPORT: Self = Self(94);
    /// Operation not supported; also `ENOTSUP` on Linux.
    pub const EOPNOTSUPP: Self = Self(95);
    /// Address family not supported by protocol.
    pub const EAFNOSUPPORT: Self = Self(97);
    /// Address already in use.
    pub const EADDRINUSE: Self = Self(98);
    /// Cannot assign requested address.
    pub const EADDRNOTAVAIL: Self = Self(99);
    /// Network is down.
    pub const ENETDOWN: Self = Self(100);
    /// Network is unreachable: no route to the destination's network.
    pub const ENETUNREACH: Self = Self(101);
    /// Software caused connection abort.
    pub const ECONNABORTED: Self = Self(103);
    /// Connection reset by peer.
    pub const ECONNRESET: Self = Self(104);
    /// No buffer space available.
    pub const ENOBUFS: Self = Self(105);
    /// Transport endpoint is already connected.
    pub const EISCONN: Self = Self(106);
    /// Transport endpoint is not connected.
    pub const ENOTCONN: Self = Self(107);
    /// Cannot send after transport endpoint shutdown.
    pub const ESHUTDOWN: Self = Self(108);
    /// Connection timed out.
    pub const ETIMEDOUT: Self = Self(110);
    /// Connection refused.
    pub const ECONNREFUSED: Self = Self(111);
    /// No route to host.
    pub const EHOSTUNREACH: Self = Self(113);
    /// Operation already in progress: what a second `connect` on a socket
    /// still connecting answers.
    pub const EALREADY: Self = Self(114);
    /// Operation now in progress.
    pub const EINPROGRESS: Self = Self(115);
    /// A quota was exceeded (Ferrix: a device's DMA quarantine is full).
    pub const EDQUOT: Self = Self(122);
    /// Operation canceled: a timerfd armed with `TFD_TIMER_CANCEL_ON_SET`
    /// read after the real-time clock was set.
    pub const ECANCELED: Self = Self(125);

    // The signal-restart codes. Linux keeps these in `include/linux/errno.h`,
    // above the numbers a program can see, precisely because they never reach
    // one: a blocking call returns one of them when a signal interrupts it, and
    // the way back to user mode turns it into a restart of the call or into
    // `EINTR`, depending on the handler, before the program is resumed. Nothing
    // here ever writes one to a `copy_to_user` or leaves one in the return
    // register of a program that runs. See `crate::syscall::deliver`.

    /// Restart the call if a handler with `SA_RESTART` runs, or if none does;
    /// `EINTR` otherwise. What an ordinary blocking read, write, `wait4`, pipe
    /// or futex wait returns.
    pub const ERESTARTSYS: Self = Self(512);
    /// Restart the call whether a handler runs or not: the call had no visible
    /// effect to interrupt.
    pub const ERESTARTNOINTR: Self = Self(513);
    /// Restart the call only if no handler runs; `EINTR` if one does. What
    /// `poll`, `select` and `pselect6` return.
    pub const ERESTARTNOHAND: Self = Self(514);
    /// Restart through `restart_syscall`, which resumes with the time left
    /// rather than from the top; `EINTR` if a handler runs. What `nanosleep`
    /// and `clock_nanosleep` return.
    pub const ERESTART_RESTARTBLOCK: Self = Self(516);

    /// Whether this is one of the kernel-internal restart codes above, which
    /// must never be encoded into a running program's return register.
    #[must_use]
    pub const fn is_restart(self) -> bool {
        matches!(
            self,
            Self::ERESTARTSYS
                | Self::ERESTARTNOINTR
                | Self::ERESTARTNOHAND
                | Self::ERESTART_RESTARTBLOCK
        )
    }

    /// The value this error takes in the system call return register.
    ///
    /// The whole convention in one expression. The cast cannot lose data and
    /// the negation cannot overflow, because the number came from a `u16`.
    #[must_use]
    pub const fn as_return_value(self) -> isize {
        -(self.0 as isize)
    }
}

/// The largest `errno` the kernel encodes as a negative return value.
///
/// `include/linux/err.h` reserves the range `-MAX_ERRNO..=-1` for errors, so a
/// successful return of, say, `0xFFFF_FFFF_FFFF_F000` as a pointer would be
/// misread as one. Nothing in this crate depends on the bound, but the kernel's
/// pointer-returning calls (`mmap`, `brk`) do.
pub const MAX_ERRNO: u16 = 4095;

/// Turn a system call result into the value the return register carries.
///
/// Success is returned as-is and failure as `-errno`. The `as` cast is
/// deliberate: a success value is a count, a file descriptor or an address that
/// the kernel has already bounded below `isize::MAX`, and wrapping is preferable
/// to a panic on an entry path that has no way to report one.
#[must_use]
pub const fn encode(result: Result<usize, Errno>) -> isize {
    match result {
        Ok(value) => value as isize,
        Err(error) => error.as_return_value(),
    }
}
