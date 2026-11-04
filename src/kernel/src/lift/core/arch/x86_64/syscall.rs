//! The saved program registers, as the load names them: filled from K1's context.

/// A user program's registers, as the trampoline saves them.
///
/// Laid out to match the pushes in `ferrix_syscall_stub` exactly, in reverse:
/// the last thing pushed is the first field. `repr(C)` because assembly is the
/// other half of this type's definition.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct SyscallFrame {
    /// Callee-saved, and the ABI's sixth argument register is not among them.
    pub(crate) r15: u64,
    /// Callee-saved.
    pub(crate) r14: u64,
    /// Callee-saved.
    pub(crate) r13: u64,
    /// Callee-saved.
    pub(crate) r12: u64,
    /// Callee-saved.
    pub(crate) rbp: u64,
    /// Callee-saved.
    pub(crate) rbx: u64,
    /// Sixth argument.
    pub(crate) r9: u64,
    /// Fifth argument.
    pub(crate) r8: u64,
    /// Fourth argument. The ABI uses `R10` here and not `RCX`, because
    /// `SYSCALL` destroys `RCX`.
    pub(crate) r10: u64,
    /// Third argument.
    pub(crate) rdx: u64,
    /// Second argument.
    pub(crate) rsi: u64,
    /// First argument.
    pub(crate) rdi: u64,
    /// The system call number on the way in, the result on the way out.
    pub(crate) rax: u64,
    /// The user `RFLAGS`, which `SYSCALL` left here.
    pub(crate) r11: u64,
    /// The user return address, which `SYSCALL` left here.
    pub(crate) rcx: u64,
    /// The user stack pointer, taken out of the per-CPU scratch word.
    pub(crate) user_rsp: u64,
}

/// A program's registers as its system call saved them.
///
/// What a fork child starts from: the same registers as the parent at the
/// moment it asked, with the return register changed. Opaque outside this
/// architecture, because every architecture keeps a different set and the
/// system call layer only ever copies one and asks for two changes to it.
///
/// Two shapes, because there are two ways in: `SYSCALL`'s frame, which
/// `SYSRET` resumes, and a trap frame from `int $0x80`, which only `IRETQ`
/// can resume -- a 32-bit program's, whose child has to come back in
/// compatibility mode with every register, `RCX` and `R11` included
/// (`docs/I386.md` I2b).
#[derive(Debug, Clone, Copy)]
pub enum UserRegs {
    /// Saved by `SYSCALL`'s trampoline.
    Syscall(SyscallFrame),
    /// Saved by the trap stub: a system call through `int $0x80`.
    Trap(super::trap::TrapFrame),
}

impl UserRegs {
    /// The same registers, as a child sees them: the call returned zero.
    pub const fn for_child(&self) -> UserRegs {
        match *self {
            UserRegs::Syscall(mut frame) => {
                frame.rax = 0;
                UserRegs::Syscall(frame)
            }
            UserRegs::Trap(mut frame) => {
                frame.rax = 0;
                UserRegs::Trap(frame)
            }
        }
    }

    /// Start on `stack` instead, as `clone` with a stack argument asks. The
    /// stack pointer is in the frame on this architecture, so `state` is
    /// untouched.
    pub const fn set_stack(&mut self, state: &mut super::UserState, stack: u64) {
        let _ = state;
        match self {
            UserRegs::Syscall(frame) => frame.user_rsp = stack,
            UserRegs::Trap(frame) => frame.rsp = stack,
        }
    }

    /// The stack pointer the program made the call with.
    pub const fn stack_pointer(&self) -> u64 {
        match self {
            UserRegs::Syscall(frame) => frame.user_rsp,
            UserRegs::Trap(frame) => frame.rsp,
        }
    }
}

/// A fork child's first entry.
///
/// # Safety
///
/// On the child's own thread, as the kernel's is.
pub unsafe fn resume_user(regs: &UserRegs) -> ! {
    crate::restricted::resume(regs)
}
pub fn user_base(code: u64) -> Option<u64> {
    let _ = code;
    Some(0)
}
/// # Safety
///
/// `entry` and `stack` are the program's to choose; called on its own thread,
/// which it does not return to.
pub unsafe fn enter_user(entry: u64, stack: u64, argument: u64, abi: crate::trap::Abi) -> ! {
    crate::restricted::enter(entry, stack, argument, abi)
}
