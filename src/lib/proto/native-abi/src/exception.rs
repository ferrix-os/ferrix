//! What an exception channel carries, and what its calls take
//! (`docs/microkernel/K4.md` §3).
//!
//! Every report is written by the kernel, which holds the only writing end
//! of the channel, so a supervisor may trust every field. Fixed layouts, the
//! same in every build of one architecture, read and written as
//! native-endian words.

/// A report's [`Report::kind`]: a translation fault the address space did
/// not resolve.
pub const KIND_PAGE_FAULT: u32 = 1;
/// A report's [`Report::kind`]: a touch of a file mapping past its file's
/// end, which Linux reports as `SIGBUS`.
pub const KIND_BUS: u32 = 2;
/// A report's [`Report::kind`]: a breakpoint instruction.
pub const KIND_BREAKPOINT: u32 = 3;
/// A report's [`Report::kind`]: an instruction the processor refused.
pub const KIND_ILLEGAL: u32 = 4;
/// A report's [`Report::kind`]: any other fault; [`Report::code`] is the
/// architecture's own word for it.
pub const KIND_OTHER: u32 = 5;

/// [`Report::flags`]: the access was a write.
pub const FLAG_WRITE: u32 = 1 << 0;
/// [`Report::flags`]: the access was an instruction fetch.
pub const FLAG_EXECUTE: u32 = 1 << 1;
/// [`Report::flags`]: a mapping was there and refused the access.
pub const FLAG_PRESENT: u32 = 1 << 2;
/// [`Report::flags`]: [`Report::pc`] is the trapping instruction's own
/// address, so resuming unchanged executes it again. Clear only for x86-64's
/// `int3`, whose saved pc is the instruction after it.
pub const FLAG_PC_AT_TRAP: u32 = 1 << 3;

/// `exception_state`'s `op`: copy the faulting thread's registers out.
pub const STATE_READ: u64 = 0;
/// `exception_state`'s `op`: replace them, checked as `sigreturn` checks a
/// frame, and refused whole when any field is one a program may not hold.
pub const STATE_WRITE: u64 = 1;

/// `exception_resolve`'s verdict: resume the thread with its registers as
/// they now stand.
pub const RESUME: u64 = 0;
/// `exception_resolve`'s verdict: end the process, as a job kill ends it.
pub const KILL: u64 = 1;
/// `exception_resolve`'s verdict: hand the fault on to the next supervisor
/// up, or to the path a fault takes with none.
pub const HAND_ON: u64 = 2;

/// One fault, as the kernel reports it: the message's bytes. The message
/// also carries exactly one handle, the exception, with
/// [`crate::rights::Rights::EXCEPTION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Report {
    /// [`KIND_PAGE_FAULT`] and the rest.
    pub kind: u32,
    /// 0 for a channel on the faulting process, then 1, 2 and up for each
    /// job from the process's own upwards.
    pub level: u32,
    /// The faulting process.
    pub pid: u32,
    /// How many reports in a row before this one the thread raised at the
    /// same pc and address.
    pub repeat: u32,
    /// The faulting thread's task.
    pub tid: u64,
    /// The faulting process's job.
    pub job: u64,
    /// The address the access was to; zero when there is none.
    pub address: u64,
    /// The architecture's own code: a vector, an ESR or an FSR.
    pub code: u64,
    /// The saved program counter.
    pub pc: u64,
    /// [`FLAG_WRITE`] and the rest.
    pub flags: u32,
    /// Zero.
    pub reserved: u32,
}

/// A report's size in bytes.
pub const REPORT_BYTES: usize = 64;

impl Report {
    /// The report as the bytes a message carries.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; REPORT_BYTES] {
        let mut out = [0_u8; REPORT_BYTES];
        let words32 = [self.kind, self.level, self.pid, self.repeat];
        for (index, word) in words32.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&word.to_ne_bytes());
        }
        let words64 = [self.tid, self.job, self.address, self.code, self.pc];
        for (index, word) in words64.iter().enumerate() {
            let at = 16 + index * 8;
            out[at..at + 8].copy_from_slice(&word.to_ne_bytes());
        }
        out[56..60].copy_from_slice(&self.flags.to_ne_bytes());
        out[60..64].copy_from_slice(&self.reserved.to_ne_bytes());
        out
    }

    /// A report back from a message's bytes, or `None` if they are not one.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Report> {
        if bytes.len() != REPORT_BYTES {
            return None;
        }
        let u32_at = |at: usize| {
            bytes
                .get(at..at + 4)
                .and_then(|word| <[u8; 4]>::try_from(word).ok())
                .map(u32::from_ne_bytes)
        };
        let u64_at = |at: usize| {
            bytes
                .get(at..at + 8)
                .and_then(|word| <[u8; 8]>::try_from(word).ok())
                .map(u64::from_ne_bytes)
        };
        Some(Report {
            kind: u32_at(0)?,
            level: u32_at(4)?,
            pid: u32_at(8)?,
            repeat: u32_at(12)?,
            tid: u64_at(16)?,
            job: u64_at(24)?,
            address: u64_at(32)?,
            code: u64_at(40)?,
            pc: u64_at(48)?,
            flags: u32_at(56)?,
            reserved: u32_at(60)?,
        })
    }
}

/// An x86-64 thread's registers, as `exception_state` reads and writes
/// them: 22 words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct X86_64Registers {
    /// `rax`, `rbx`, `rcx`, `rdx`, `rsi`, `rdi`, `rbp`, `rsp`, then `r8` to
    /// `r15`, in that order.
    pub general: [u64; 16],
    /// The instruction pointer.
    pub rip: u64,
    /// The flags. A read shows only the flags a program may hold; a write
    /// holding any other is refused.
    pub rflags: u64,
    /// The code selector: 64-bit user code, or 32-bit user code with `rip`
    /// below 4 GiB.
    pub cs: u64,
    /// The stack selector: user data, and nothing else.
    pub ss: u64,
    /// Zero.
    pub reserved: [u64; 2],
}

/// An `AArch64` thread's registers, as `exception_state` reads and writes
/// them: 34 words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Aarch64Registers {
    /// `x0` to `x30`.
    pub x: [u64; 31],
    /// The stack pointer.
    pub sp: u64,
    /// The program counter.
    pub pc: u64,
    /// `PSTATE`: the condition flags over EL0t; a write holding anything
    /// else is refused.
    pub pstate: u64,
}

impl Default for Aarch64Registers {
    fn default() -> Aarch64Registers {
        Aarch64Registers {
            x: [0; 31],
            sp: 0,
            pc: 0,
            pstate: 0,
        }
    }
}

/// An ARMv7-A thread's registers, as `exception_state` reads and writes
/// them: 17 words of 32 bits, and one of padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Armv7aRegisters {
    /// `r0` to `r15`; `r13` is the stack pointer and `r15` the pc.
    pub r: [u32; 16],
    /// `CPSR`: USR mode, with only the flags, GE, the IT state and T a
    /// program may hold; a write holding anything else is refused.
    pub cpsr: u32,
    /// Zero.
    pub reserved: u32,
}

/// The registers of the architecture this is built for.
#[cfg(target_arch = "x86_64")]
pub type Registers = X86_64Registers;
/// The registers of the architecture this is built for.
#[cfg(target_arch = "aarch64")]
pub type Registers = Aarch64Registers;
/// The registers of the architecture this is built for.
#[cfg(target_arch = "arm")]
pub type Registers = Armv7aRegisters;

/// Bytes in [`X86_64Registers`].
pub const X86_64_REGISTERS_BYTES: usize = 22 * 8;
/// Bytes in [`Aarch64Registers`].
pub const AARCH64_REGISTERS_BYTES: usize = 34 * 8;
/// Bytes in [`Armv7aRegisters`].
pub const ARMV7A_REGISTERS_BYTES: usize = 18 * 4;

/// The words of a register set, in their order, for copying through a
/// buffer as native-endian bytes.
pub trait Words: Sized {
    /// Bytes in the set.
    const BYTES: usize;

    /// Write the set into `out`, which is [`Words::BYTES`] long.
    fn write(&self, out: &mut [u8]);

    /// Read a set from `bytes`, which is [`Words::BYTES`] long.
    fn read(bytes: &[u8]) -> Option<Self>;
}

/// The `index`th `N`-byte word of `bytes`.
fn word<const N: usize>(bytes: &[u8], index: usize) -> Option<[u8; N]> {
    bytes
        .get(index * N..index * N + N)
        .and_then(|word| <[u8; N]>::try_from(word).ok())
}

/// Write `words` into `out` as native-endian 64-bit words.
fn put64(out: &mut [u8], words: impl IntoIterator<Item = u64>) {
    for (index, value) in words.into_iter().enumerate() {
        if let Some(slot) = out.get_mut(index * 8..index * 8 + 8) {
            slot.copy_from_slice(&value.to_ne_bytes());
        }
    }
}

impl Words for X86_64Registers {
    const BYTES: usize = X86_64_REGISTERS_BYTES;

    fn write(&self, out: &mut [u8]) {
        let tail = [
            self.rip,
            self.rflags,
            self.cs,
            self.ss,
            self.reserved[0],
            self.reserved[1],
        ];
        put64(out, self.general.iter().copied().chain(tail));
    }

    fn read(bytes: &[u8]) -> Option<X86_64Registers> {
        if bytes.len() != Self::BYTES {
            return None;
        }
        let at = |index| word::<8>(bytes, index).map(u64::from_ne_bytes);
        let mut general = [0_u64; 16];
        for (index, slot) in general.iter_mut().enumerate() {
            *slot = at(index)?;
        }
        Some(X86_64Registers {
            general,
            rip: at(16)?,
            rflags: at(17)?,
            cs: at(18)?,
            ss: at(19)?,
            reserved: [at(20)?, at(21)?],
        })
    }
}

impl Words for Aarch64Registers {
    const BYTES: usize = AARCH64_REGISTERS_BYTES;

    fn write(&self, out: &mut [u8]) {
        let tail = [self.sp, self.pc, self.pstate];
        put64(out, self.x.iter().copied().chain(tail));
    }

    fn read(bytes: &[u8]) -> Option<Aarch64Registers> {
        if bytes.len() != Self::BYTES {
            return None;
        }
        let at = |index| word::<8>(bytes, index).map(u64::from_ne_bytes);
        let mut x = [0_u64; 31];
        for (index, slot) in x.iter_mut().enumerate() {
            *slot = at(index)?;
        }
        Some(Aarch64Registers {
            x,
            sp: at(31)?,
            pc: at(32)?,
            pstate: at(33)?,
        })
    }
}

impl Words for Armv7aRegisters {
    const BYTES: usize = ARMV7A_REGISTERS_BYTES;

    fn write(&self, out: &mut [u8]) {
        let words = self.r.iter().copied().chain([self.cpsr, self.reserved]);
        for (index, value) in words.enumerate() {
            if let Some(slot) = out.get_mut(index * 4..index * 4 + 4) {
                slot.copy_from_slice(&value.to_ne_bytes());
            }
        }
    }

    fn read(bytes: &[u8]) -> Option<Armv7aRegisters> {
        if bytes.len() != Self::BYTES {
            return None;
        }
        let at = |index| word::<4>(bytes, index).map(u32::from_ne_bytes);
        let mut r = [0_u32; 16];
        for (index, slot) in r.iter_mut().enumerate() {
            *slot = at(index)?;
        }
        Some(Armv7aRegisters {
            r,
            cpsr: at(16)?,
            reserved: at(17)?,
        })
    }
}
