//! Restricted mode and shared processes, as a program sees them
//! (`docs/microkernel/K1.md`, `docs/microkernel/K11.md`).
//!
//! A thread of a shared process runs `linuxd`'s code in *normal mode* and a
//! Linux program's in *restricted mode*. `RESTRICTED_ENTER`,
//! `RESTRICTED_RESUME` and `RESTRICTED_CONTINUE` go into restricted mode, and
//! return, like one long system call, by an *exit*: its [`Reason`] in the
//! reason register, the program's number in its own register, and six words
//! in the registers the program's arguments were in (K1 §2.2).
//!
//! | | x86-64 | `AArch64` |
//! |---|---|---|
//! | reason ([`Reason`]; zero: the call was refused) | `r12` | `x9` |
//! | number or vector | `rax` | `x8` |
//! | words 0 to 5 | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | `x0` to `x5` |

/// The top of a restricted space, and the bottom of a region: an address at or
/// above it is the shared region's, and never reachable from restricted mode.
/// The same on x86-64 and `AArch64`, whose user half ends at
/// `0x8000_0000_0000` (K11 §2.1).
pub const SHARED_BASE: u64 = 0x7000_0000_0000;

/// Why a thread left restricted mode. Zero in the reason register is no exit:
/// the call was refused, and the usual return register holds its status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Reason {
    /// A system call: the number and six arguments as the program left them.
    Syscall = 1,
    /// `int $0x80` on x86-64: the number and words zero-extended, in `ebx`,
    /// `ecx`, `edx`, `esi`, `edi`, `ebp`'s order.
    SyscallCompat = 2,
    /// A fault the kernel could not resolve: the number is the vector (x86-64)
    /// or `ESR_EL1.EC`, and the words are [`FAULT_CODE`] and its neighbours.
    Fault = 3,
    /// A kick: the thread is to look at what `linuxd` wants of it.
    Kick = 4,
}

impl Reason {
    /// The reason the register holds, if it holds one.
    #[must_use]
    pub const fn from_register(value: u64) -> Option<Reason> {
        match value {
            1 => Some(Reason::Syscall),
            2 => Some(Reason::SyscallCompat),
            3 => Some(Reason::Fault),
            4 => Some(Reason::Kick),
            _ => None,
        }
    }
}

/// In a [`Reason::Fault`] exit: the word holding the error code, or the whole
/// `ESR_EL1`.
pub const FAULT_CODE: usize = 0;
/// In a [`Reason::Fault`] exit: the word holding the fault's address, zero
/// where there is none.
pub const FAULT_ADDRESS: usize = 1;
/// In a [`Reason::Fault`] exit: the word holding the [`FaultKind`].
pub const FAULT_KIND: usize = 2;
/// In a [`Reason::Fault`] or [`Reason::Kick`] exit: the word holding the
/// program's instruction pointer.
pub const EXIT_PC: usize = 3;
/// In a [`Reason::Fault`] or [`Reason::Kick`] exit: the word holding the
/// program's stack pointer.
pub const EXIT_SP: usize = 4;

/// What kind of fault a [`Reason::Fault`] was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum FaultKind {
    /// Not a memory fault.
    NotMemory = 0,
    /// No mapping: Linux's `SEGV_MAPERR`.
    NoMapping = 1,
    /// A mapping that did not permit it: `SEGV_ACCERR`.
    Protection = 2,
    /// Past the end of an object, or a pager that failed: `SIGBUS`,
    /// `BUS_ADRERR`.
    PastEnd = 3,
    /// Memory poison: `SIGBUS`, `BUS_MCEERR_AR`.
    Poison = 4,
    /// An answered call's copy missed the restricted space: the thread is back at
    /// the call, to run it again once the page is there, or to be told `FAULT`
    /// (`docs/microkernel/K1.md` §2.2, ledger 6326, 6529). Never a signal.
    CallCopy = 5,
}

/// `HANDLE_TO_SHARED`'s option: leave the private handle where it is, which
/// needs `DUPLICATE` beside `TRANSFER`.
pub const HANDLE_KEEP: usize = 1;

/// `RESTRICTED_ENTER`'s option: every system call of this context exits, a native number
/// too, until the next entry (`docs/microkernel/K1.md` §2.2). What `linuxd` asks for a thread
/// under Syscall User Dispatch, whose program asked that the kernel take its calls away.
pub const ENTER_ALL_CALLS_EXIT: u64 = 1;

/// `RESTRICTED_ENTER`'s option: an answered call whose copy misses this context's own space
/// exits instead of answering `FAULT`, where its row in `nr::copy_miss` says it may run again,
/// until the next entry (`docs/microkernel/K1.md` §2.2). What `linuxd` asks for, whose
/// restricted space is only a cache of its shadow.
pub const ENTER_COPY_MISS_EXITS: u64 = 2;

/// A program's registers on x86-64, as `RESTRICTED_ENTER` takes them and
/// `RESTRICTED_CONTEXT_READ` gives them (K1 §2.3). Every field is a whole word,
/// so that nothing is truncated into validity: a bit above a selector's width
/// is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct ContextX86_64 {
    /// `r15`.
    pub r15: u64,
    /// `r14`.
    pub r14: u64,
    /// `r13`.
    pub r13: u64,
    /// `r12`.
    pub r12: u64,
    /// `rbp`.
    pub rbp: u64,
    /// `rbx`.
    pub rbx: u64,
    /// `r11`.
    pub r11: u64,
    /// `r10`.
    pub r10: u64,
    /// `r9`.
    pub r9: u64,
    /// `r8`.
    pub r8: u64,
    /// `rax`.
    pub rax: u64,
    /// `rcx`.
    pub rcx: u64,
    /// `rdx`.
    pub rdx: u64,
    /// `rsi`.
    pub rsi: u64,
    /// `rdi`.
    pub rdi: u64,
    /// The instruction pointer.
    pub rip: u64,
    /// The stack pointer.
    pub rsp: u64,
    /// The flags: bit 1 and `IF` set, and otherwise only what a program may
    /// hold.
    pub rflags: u64,
    /// The code selector: 64-bit or 32-bit user code, exactly.
    pub cs: u64,
    /// The stack selector: user data, exactly.
    pub ss: u64,
    /// `ds`: null, or loadable under `tls` with RPL 3.
    pub ds: u64,
    /// `es`, as `ds`.
    pub es: u64,
    /// `fs`, as `ds`.
    pub fs: u64,
    /// `gs`, as `ds`.
    pub gs: u64,
    /// The FS base: zero, or an address in the restricted window.
    pub fs_base: u64,
    /// The GS base, as `fs_base`.
    pub gs_base: u64,
    /// The three thread-local descriptors, zero for none.
    pub tls: [u64; 3],
}

/// A program's registers on `AArch64`, as [`ContextX86_64`] is on x86-64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct ContextAarch64 {
    /// `x0` to `x30`.
    pub x: [u64; 31],
    /// `SP_EL0`.
    pub sp: u64,
    /// `ELR_EL1`: where the program resumes.
    pub pc: u64,
    /// `SPSR_EL1`: only the condition flags, NZCV.
    pub pstate: u64,
    /// `TPIDR_EL0`.
    pub tpidr: u64,
    /// `TPIDRRO_EL0`, which a program may read and only the kernel writes.
    pub tpidrro: u64,
}

/// Whether `RESTRICTED_CONTINUE` may go back in: a context held, its last exit a kick
/// or a fault, and a program counter the way back cannot fault on, `user_end` being one
/// past the last such address (`docs/microkernel/K1.md` §2.1, ledger 6282 R7).
///
/// A jump past the user half reports the jump's own address on the Ryzen 9 9900X, and
/// another part may report the target, so a program may not reach this test: which is
/// why it is a function, tested with the address it refuses.
#[must_use]
pub const fn may_continue(held: bool, last: u64, program_counter: u64, user_end: u64) -> bool {
    held && (last == Reason::Kick as u64 || last == Reason::Fault as u64)
        && program_counter < user_end
}

impl ContextX86_64 {
    /// The context from its bytes, as a call's memory holds them.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 232]) -> ContextX86_64 {
        let word = |at: usize| word_at(bytes, at);
        ContextX86_64 {
            r15: word(0),
            r14: word(1),
            r13: word(2),
            r12: word(3),
            rbp: word(4),
            rbx: word(5),
            r11: word(6),
            r10: word(7),
            r9: word(8),
            r8: word(9),
            rax: word(10),
            rcx: word(11),
            rdx: word(12),
            rsi: word(13),
            rdi: word(14),
            rip: word(15),
            rsp: word(16),
            rflags: word(17),
            cs: word(18),
            ss: word(19),
            ds: word(20),
            es: word(21),
            fs: word(22),
            gs: word(23),
            fs_base: word(24),
            gs_base: word(25),
            tls: [word(26), word(27), word(28)],
        }
    }

    /// The context's bytes, as a call's memory holds them.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 232] {
        let [tls0, tls1, tls2] = self.tls;
        let words = [
            self.r15,
            self.r14,
            self.r13,
            self.r12,
            self.rbp,
            self.rbx,
            self.r11,
            self.r10,
            self.r9,
            self.r8,
            self.rax,
            self.rcx,
            self.rdx,
            self.rsi,
            self.rdi,
            self.rip,
            self.rsp,
            self.rflags,
            self.cs,
            self.ss,
            self.ds,
            self.es,
            self.fs,
            self.gs,
            self.fs_base,
            self.gs_base,
            tls0,
            tls1,
            tls2,
        ];
        let mut bytes = [0_u8; 232];
        for (out, value) in bytes.chunks_exact_mut(8).zip(words) {
            out.copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    }
}

impl ContextAarch64 {
    /// The context from its bytes, as a call's memory holds them.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 288]) -> ContextAarch64 {
        let word = |at: usize| word_at(bytes, at);
        ContextAarch64 {
            x: core::array::from_fn(word),
            sp: word(31),
            pc: word(32),
            pstate: word(33),
            tpidr: word(34),
            tpidrro: word(35),
        }
    }

    /// The context's bytes, as a call's memory holds them.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 288] {
        let mut bytes = [0_u8; 288];
        let rest = [self.sp, self.pc, self.pstate, self.tpidr, self.tpidrro];
        for (out, value) in bytes
            .chunks_exact_mut(8)
            .zip(self.x.iter().chain(rest.iter()))
        {
            out.copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    }
}

/// The `index`th word of `bytes`, zero past the end.
fn word_at(bytes: &[u8], index: usize) -> u64 {
    let mut word = [0_u8; 8];
    if let Some(found) = bytes.chunks_exact(8).nth(index) {
        word.copy_from_slice(found);
    }
    u64::from_ne_bytes(word)
}

const _: () = assert!(
    size_of::<ContextX86_64>() == 232,
    "the ABI's x86-64 context"
);
const _: () = assert!(
    size_of::<ContextAarch64>() == 288,
    "the ABI's AArch64 context"
);

pub use crate::arch::{Context, VECTORS_BYTES};

/// The bytes of [`Context`].
pub const CONTEXT_BYTES: usize = size_of::<Context>();

/// The bytes of `RESTRICTED_VECTORS_READ` and `_WRITE`'s image on x86-64: the
/// standard-form `XSAVE` area for x87, SSE and AVX.
pub const VECTORS_BYTES_X86_64: usize = 832;
/// The bytes of that image on `AArch64`: `V0` to `V31`, `FPCR`, `FPSR`.
pub const VECTORS_BYTES_AARCH64: usize = 528;
