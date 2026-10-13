//! A 32-bit program's system call arguments, rewritten into the layout the
//! handlers read (`docs/I386.md` §3.2, I3).
//!
//! The entry zero-extends each of an i386 call's six registers, which is
//! right for every `int`, `unsigned` and pointer argument: the handlers
//! narrow an `int` themselves. Two kinds of argument are not right that way,
//! and both are the register layout rather than a structure in memory:
//!
//! * **A 64-bit value** (`loff_t`, a `u64`) arrives in two registers, low
//!   word first, with no alignment to an even register -- Linux's
//!   `compat_arg_u64` on x86 and its `ia32_*` wrappers in
//!   `arch/x86/kernel/sys_ia32.c`, unlike ARMv7-A's EABI. It is joined into
//!   one argument and the ones after it move up, so the handler sees the
//!   64-bit prototype.
//! * **A signed `long`** (`off_t`) is 32 bits and has to be sign-extended, or
//!   `lseek(fd, -1, SEEK_CUR)` would seek four gigabytes forward.
//!
//! After this, `linux::wide` and `linux::native_signed` read the arguments as
//! they would a 64-bit program's. A call that passes a structure in memory
//! instead asks its handler to read it at the call's word width, and
//! [`ioctl_passes`] keeps back the `ioctl` requests no handler reads that way
//! yet. [`socketcall`] turns i386's one socket entry into the call it names.

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    FIOCLEX, FIONBIO, FIONCLEX, FIONREAD, TCFLSH, TCGETS, TCGETS2, TCSETS, TCSETS2, TCSETSF,
    TCSETSF2, TCSETSW, TCSETSW2, TCXONC, TIOCGPGRP, TIOCGPTN, TIOCGSID, TIOCGWINSZ, TIOCNOTTY,
    TIOCOUTQ, TIOCSCTTY, TIOCSPGRP, TIOCSPTLCK, TIOCSWINSZ,
};

use crate::syscall::process::Process;
use crate::syscall::uaccess;

/// `call`'s arguments as a 64-bit program would have passed them.
///
/// Only the calls i386's table maps reach here, and only those whose
/// registers need rewriting are rewritten; everything else is returned as it
/// came.
pub(crate) fn normalize(call: Syscall, a: [u64; 6]) -> [u64; 6] {
    let pair = |low: u64, high: u64| (high & 0xFFFF_FFFF) << 32 | (low & 0xFFFF_FFFF);
    let signed = |value: u64| value as u32 as i32 as i64 as u64;
    let [a0, a1, a2, a3, a4, a5] = a;
    match call {
        // `pread64(fd, buf, count, pos)`: the position is registers 3 and 4.
        Syscall::Pread64 | Syscall::Pwrite64 => [a0, a1, a2, pair(a3, a4), 0, 0],
        // `ftruncate64(fd, length)`, `truncate64(path, length)`.
        Syscall::Ftruncate64 | Syscall::Truncate64 => [a0, pair(a1, a2), 0, 0, 0, 0],
        // `fallocate(fd, mode, offset, len)`, both 64-bit.
        Syscall::Fallocate => [a0, a1, pair(a2, a3), pair(a4, a5), 0, 0],
        // `readahead(fd, offset, count)`.
        Syscall::Readahead => [a0, pair(a1, a2), a3, 0, 0, 0],
        // `lseek(fd, offset, whence)`, `ftruncate(fd, length)` and
        // `truncate(path, length)` take a 32-bit `off_t`.
        Syscall::Lseek => [a0, signed(a1), a2, a3, a4, a5],
        Syscall::Ftruncate | Syscall::Truncate => [a0, signed(a1), a2, a3, a4, a5],
        _ => a,
    }
}

/// Whether an i386 program's `ioctl` `request` may reach the handlers, which
/// read their argument as a 64-bit program lays it out.
///
/// Linux answers a 32-bit program's `ioctl` through `compat_ioctl`, which
/// either passes a request through -- its argument is the same at both widths
/// -- or translates it, and refuses anything it knows neither way with
/// `ENOTTY`. These are the requests passed through: the terminal's, whose
/// `termios`, `termios2` and `winsize` hold only 32-bit and narrower fields,
/// and the ones whose argument is an `int` or nothing. Everything else is
/// refused until it is translated, rather than read at the wrong width.
pub(crate) fn ioctl_passes(request: u32) -> bool {
    matches!(
        request,
        TCGETS
            | TCSETS
            | TCSETSW
            | TCSETSF
            | TCGETS2
            | TCSETS2
            | TCSETSW2
            | TCSETSF2
            | TCXONC
            | TCFLSH
            | TIOCSCTTY
            | TIOCNOTTY
            | TIOCGPGRP
            | TIOCSPGRP
            | TIOCGSID
            | TIOCOUTQ
            | TIOCGWINSZ
            | TIOCSWINSZ
            | TIOCGPTN
            | TIOCSPTLCK
            | FIONREAD
            | FIONBIO
            | FIOCLEX
            | FIONCLEX
    )
}

/// Words in each `socketcall` sub-call's block, by sub-call number: Linux's
/// `nargs` in `net/socket.c`, in 32-bit words rather than bytes. Zero is no
/// call; 19 and 20 are `recvmmsg` and `sendmmsg`.
const SOCKETCALL_WORDS: [usize; 21] = [
    0, 3, 3, 3, 2, 3, 3, 3, 4, 4, 4, 6, 6, 2, 5, 5, 3, 3, 4, 5, 4,
];

/// i386's `socketcall(call, args)`: the socket call sub-call `call` names,
/// with its arguments read from the block at `args` as 32-bit words and
/// zero-extended, as `compat_sys_socketcall` reads them.
///
/// Linux's order: a sub-call outside 1 to 20 is `EINVAL` before anything is
/// read, and a block that cannot be read is `EFAULT`. `send` and `recv` are
/// `sendto` and `recvfrom` with no address, as Linux answers them.
/// `recvmmsg` and `sendmmsg` are answered by their own numbers (i386's 337,
/// 345 and 417, which glibc uses from Linux 4.3 on) but are still `ENOSYS`
/// here once their block has been read: the i386 boot check in
/// `arch/x86_64/trap/check.rs`, a core-ring file, requires that of sub-call
/// 19, and changing it goes through the certification review.
///
/// # Errors
///
/// As above.
pub(crate) fn socketcall(process: &Process, a: [u64; 6]) -> Result<(Syscall, [u64; 6]), Errno> {
    let number = a[0] as u32 as i32;
    let words = usize::try_from(number)
        .ok()
        .filter(|&number| number >= 1)
        .and_then(|number| SOCKETCALL_WORDS.get(number).copied())
        .ok_or(Errno::EINVAL)?;
    let mut bytes = [0_u8; 6 * 4];
    let block = bytes.get_mut(..words * 4).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), a[1] & 0xFFFF_FFFF, block)
        .map_err(|_| Errno::EFAULT)?;
    let mut w = [0_u64; 6];
    for (slot, word) in w.iter_mut().zip(bytes.chunks_exact(4)) {
        let mut four = [0_u8; 4];
        four.copy_from_slice(word);
        *slot = u64::from(u32::from_le_bytes(four));
    }
    let call = match number {
        1 => Syscall::Socket,
        2 => Syscall::Bind,
        3 => Syscall::Connect,
        4 => Syscall::Listen,
        5 => Syscall::Accept,
        6 => Syscall::Getsockname,
        7 => Syscall::Getpeername,
        8 => Syscall::Socketpair,
        // `send(fd, buf, len, flags)` and `recv`: no address.
        9 => return Ok((Syscall::Sendto, [w[0], w[1], w[2], w[3], 0, 0])),
        10 => return Ok((Syscall::Recvfrom, [w[0], w[1], w[2], w[3], 0, 0])),
        11 => Syscall::Sendto,
        12 => Syscall::Recvfrom,
        13 => Syscall::Shutdown,
        14 => Syscall::Setsockopt,
        15 => Syscall::Getsockopt,
        16 => Syscall::Sendmsg,
        17 => Syscall::Recvmsg,
        18 => Syscall::Accept4,
        _ => return Err(Errno::ENOSYS),
    };
    Ok((call, w))
}
