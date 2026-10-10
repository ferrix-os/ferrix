//! What an i386 program's signal handling looks like in memory on the x86-64
//! kernel: the two frames a handler is entered on, the context they carry,
//! the `siginfo` and `sigaction` a 32-bit program reads and writes, and its
//! `stack_t` (`docs/I386.md`, I2b).
//!
//! The frames and `struct ucontext_ia32` are the kernel's own, not UAPI:
//! `arch/x86/include/asm/sigframe.h` and `asm/ia32.h`, read from the Linux
//! 7.0 headers on the build host. `struct sigcontext_32` and `struct
//! _fpstate_32` are UAPI (`asm/sigcontext.h`), and `compat_sigaction` and
//! `compat_stack_t` are `include/linux/compat.h`'s. Every offset below is a
//! sum of those declarations' field sizes, and a host test holds each one.
//!
//! musl and glibc both install every handler with `SA_RESTORER` and both use
//! both frames: `rt_sigframe_ia32` for a handler with `SA_SIGINFO`, returned
//! from through `rt_sigreturn` (173), and `sigframe_ia32` for one without,
//! returned from through `sigreturn` (119).

use crate::wire;

/// `struct sigcontext_32`: the registers a frame saves, 88 bytes. Selectors
/// are 16-bit with 16 bits of padding above each.
pub mod sigcontext {
    /// `gs`.
    pub const GS: usize = 0;
    /// `fs`.
    pub const FS: usize = 4;
    /// `es`.
    pub const ES: usize = 8;
    /// `ds`.
    pub const DS: usize = 12;
    /// `di`, the first of the eight general registers in `pushal`'s order
    /// reversed: `di`, `si`, `bp`, `sp`, `bx`, `dx`, `cx`, `ax`.
    pub const DI: usize = 16;
    /// `si`.
    pub const SI: usize = 20;
    /// `bp`.
    pub const BP: usize = 24;
    /// `sp`.
    pub const SP: usize = 28;
    /// `bx`.
    pub const BX: usize = 32;
    /// `dx`.
    pub const DX: usize = 36;
    /// `cx`.
    pub const CX: usize = 40;
    /// `ax`.
    pub const AX: usize = 44;
    /// `trapno`: the vector, for a fault.
    pub const TRAPNO: usize = 48;
    /// `err`: the error code, for a fault.
    pub const ERR: usize = 52;
    /// `ip`.
    pub const IP: usize = 56;
    /// `cs`.
    pub const CS: usize = 60;
    /// `flags`.
    pub const FLAGS: usize = 64;
    /// `sp_at_signal`.
    pub const SP_AT_SIGNAL: usize = 68;
    /// `ss`.
    pub const SS: usize = 72;
    /// `fpstate`: where the `_fpstate_32` is, zero for none.
    pub const FPSTATE: usize = 76;
    /// `oldmask`: the saved mask's first word.
    pub const OLDMASK: usize = 80;
    /// `cr2`: the address of a page fault.
    pub const CR2: usize = 84;
    /// Bytes in the structure.
    pub const SIZE: usize = 88;
}

/// `compat_stack_t`, 12 bytes: `ss_sp`, `ss_flags`, `ss_size`.
pub mod stack {
    /// `ss_sp`.
    pub const SP: usize = 0;
    /// `ss_flags`.
    pub const FLAGS: usize = 4;
    /// `ss_size`.
    pub const SIZE_FIELD: usize = 8;
    /// Bytes in the structure.
    pub const SIZE: usize = 12;
}

/// `struct ucontext_ia32`, relative to its start: flags, link, the
/// alternate stack, the registers, and the mask last.
pub mod ucontext {
    use super::{sigcontext, stack};
    /// `uc_flags`.
    pub const FLAGS: usize = 0;
    /// `uc_link`.
    pub const LINK: usize = 4;
    /// `uc_stack`, a `compat_stack_t`.
    pub const STACK: usize = 8;
    /// `uc_mcontext`, a `struct sigcontext_32`.
    pub const MCONTEXT: usize = STACK + stack::SIZE;
    /// `uc_sigmask`, a 64-bit `compat_sigset_t`.
    pub const SIGMASK: usize = MCONTEXT + sigcontext::SIZE;
    /// Bytes in the structure.
    pub const SIZE: usize = SIGMASK + 8;
}

/// `struct rt_sigframe_ia32`: what a handler with `SA_SIGINFO` is entered
/// on. `pretcode`, which the handler's `ret` pops, then the signal and
/// pointers to the two structures after them, `-mregparm=3`-style.
pub mod rt_frame {
    use super::{SIGINFO_SIZE, ucontext};
    /// `pretcode`: the restorer.
    pub const PRETCODE: usize = 0;
    /// `sig`.
    pub const SIG: usize = 4;
    /// `pinfo`: the address of `info`.
    pub const PINFO: usize = 8;
    /// `puc`: the address of `uc`.
    pub const PUC: usize = 12;
    /// `info`, a `compat_siginfo_t`.
    pub const INFO: usize = 16;
    /// `uc`, a `struct ucontext_ia32`.
    pub const UC: usize = INFO + SIGINFO_SIZE;
    /// `retcode`: eight bytes gdb still looks for, the restorer's code as
    /// Linux writes it.
    pub const RETCODE: usize = UC + ucontext::SIZE;
    /// Bytes in the frame; the floating-point state goes below it.
    pub const SIZE: usize = RETCODE + 8;
}

/// `struct sigframe_ia32`: what a handler without `SA_SIGINFO` is entered
/// on. The `_fpstate_32` inside it is unused, kept so `extramask` stays where
/// old programs look; the real one goes below the frame as for the other.
pub mod frame {
    use super::{FPSTATE_SIZE, sigcontext};
    /// `pretcode`: the restorer.
    pub const PRETCODE: usize = 0;
    /// `sig`.
    pub const SIG: usize = 4;
    /// `sc`, a `struct sigcontext_32`, whose `oldmask` is the mask's low word.
    pub const SC: usize = 8;
    /// `extramask[1]`: the mask's high word.
    pub const EXTRAMASK: usize = SC + sigcontext::SIZE + FPSTATE_SIZE;
    /// `retcode`.
    pub const RETCODE: usize = EXTRAMASK + 4;
    /// Bytes in the frame.
    pub const SIZE: usize = RETCODE + 8;
}

/// Bytes in a `siginfo`, 32-bit or not.
pub const SIGINFO_SIZE: usize = 128;

/// Bytes in `struct _fpstate_32`: the 112-byte `fsave` environment and
/// registers, then the 512-byte `FXSAVE` image.
pub const FPSTATE_SIZE: usize = FXSAVE_AT + 512;

/// Where the `FXSAVE` image starts in a `_fpstate_32`: after the legacy
/// environment, seven words and eight ten-byte registers, and the `status`
/// and `magic` halves.
pub const FXSAVE_AT: usize = 112;

/// Where `magic` is in a `_fpstate_32`: `0x0000`, `X86_FXSR_MAGIC`, says the
/// `FXSAVE` image follows.
pub const FPSTATE_MAGIC: usize = 110;

/// Where `status` is: the x87 status word, as `fnsave` would have left it.
pub const FPSTATE_STATUS: usize = 108;

/// `rt_sigreturn`'s code, as Linux puts it in `retcode`:
/// `movl $173, %eax ; int $0x80`, and a pad byte.
pub const RT_RETCODE: [u8; 8] = [0xb8, 0xad, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x00];

/// `sigreturn`'s: `popl %eax ; movl $119, %eax ; int $0x80`.
pub const RETCODE: [u8; 8] = [0x58, 0xb8, 0x77, 0x00, 0x00, 0x00, 0xcd, 0x80];

/// The `siginfo` a 32-bit program reads, from the one a 64-bit program
/// would: the three `int`s where they are, and the union four bytes lower.
///
/// The union starts at the first pointer-aligned offset after the three
/// `int`s: 16 with 8-byte pointers, 12 with 4-byte ones. Every field it holds
/// for what the kernel raises -- a sender's pid and uid, a child's status, a
/// fault's address -- keeps its offset from the union's start, being `int`s
/// and one pointer-sized address first, and an address a 32-bit program
/// faulted on has nothing in its upper half. So the move is the whole of the
/// conversion for those; Linux's `copy_siginfo_to_user32` does it field by
/// field because it also carries `clock_t`s and `sigval`s, which a signal
/// queued by a program brings.
///
/// One origin is not the move: a system call a seccomp filter trapped
/// (`SIGSYS`, `si_code` `SYS_SECCOMP`), whose union starts with a pointer that is
/// eight bytes wide here and four there, so the two `int`s behind it are at 24
/// and 28 here and at 16 and 20 there. It is converted field by field:
/// `si_errno` and `si_code` where they are, `_call_addr` truncated to the low
/// word at 12, `_syscall` at 16 and `_arch` at 20, as Linux's
/// `copy_siginfo_to_user32` lays out `_sigsys` for a compat task.
#[must_use]
pub fn siginfo_from_64(info: &[u8; SIGINFO_SIZE]) -> [u8; SIGINFO_SIZE] {
    let mut out = [0_u8; SIGINFO_SIZE];
    if is_seccomp_trap(info) {
        // `si_signo`, `si_errno`, `si_code`.
        if let (Some(to), Some(from)) = (out.get_mut(..12), info.get(..12)) {
            to.copy_from_slice(from);
        }
        // `_call_addr`: the low word of the 64-bit pointer.
        if let (Some(to), Some(from)) = (out.get_mut(12..16), info.get(16..20)) {
            to.copy_from_slice(from);
        }
        // `_syscall` and `_arch`.
        if let (Some(to), Some(from)) = (out.get_mut(16..24), info.get(24..32)) {
            to.copy_from_slice(from);
        }
        return out;
    }
    if let (Some(to), Some(from)) = (out.get_mut(..12), info.get(..12)) {
        to.copy_from_slice(from);
    }
    if let (Some(to), Some(from)) = (out.get_mut(12..SIGINFO_SIZE - 4), info.get(16..)) {
        to.copy_from_slice(from);
    }
    out
}

/// Whether `info` is the `siginfo` of a system call a seccomp filter trapped:
/// `SIGSYS` (31) with `si_code` `SYS_SECCOMP` (1).
fn is_seccomp_trap(info: &[u8; SIGINFO_SIZE]) -> bool {
    let word = |at: usize| {
        info.get(at..at + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(i32::from_le_bytes)
    };
    word(0) == Some(31) && word(8) == Some(1)
}

/// A 32-bit program's `struct sigaction`, as `rt_sigaction` reads and writes
/// it on x86: `compat_sigaction`, 20 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sigaction32 {
    /// `sa_handler`, or `SIG_DFL`/`SIG_IGN`.
    pub handler: u32,
    /// `sa_flags`.
    pub flags: u32,
    /// `sa_restorer`.
    pub restorer: u32,
    /// `sa_mask`, two words, low signals first.
    pub mask: u64,
}

impl Sigaction32 {
    /// Bytes in the structure.
    pub const SIZE: usize = 20;

    /// Read one from a program's bytes.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Sigaction32> {
        let word = |at| wire::array::<4>(bytes, at).map(u32::from_le_bytes);
        Some(Sigaction32 {
            handler: word(0)?,
            flags: word(4)?,
            restorer: word(8)?,
            mask: u64::from(word(12)?) | (u64::from(word(16)?) << 32),
        })
    }

    /// The bytes a program reads back.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut out = [0_u8; Self::SIZE];
        for (at, word) in [
            (0, self.handler),
            (4, self.flags),
            (8, self.restorer),
            (12, self.mask as u32),
            (16, (self.mask >> 32) as u32),
        ] {
            let _ = wire::put(&mut out, at, &word.to_le_bytes());
        }
        out
    }
}
