//! The saved program registers, as the load names them: filled from K1's context.

use super::trap::TrapFrame;

/// A program's registers as its system call saved them.
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub struct UserRegs(pub(crate) TrapFrame);

impl UserRegs {
    /// The same registers, as a child sees them: the call returned zero.
    pub const fn for_child(&self) -> UserRegs {
        let mut frame = self.0;
        frame.x[0] = 0;
        UserRegs(frame)
    }

    /// Start on `stack` instead, as `clone` with a stack argument asks.
    pub const fn set_stack(&mut self, state: &mut super::UserState, stack: u64) {
        let _ = state;
        self.0.sp = stack;
    }

    /// The stack pointer the program made the call with.
    pub const fn stack_pointer(&self) -> u64 {
        self.0.sp
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
pub const fn user_base(_code: u64) -> Option<u64> {
    Some(0)
}
/// # Safety
///
/// `entry` and `stack` are the program's to choose; called on its own thread,
/// which it does not return to.
pub unsafe fn enter_user(entry: u64, stack: u64, argument: u64, abi: crate::trap::Abi) -> ! {
    crate::restricted::enter(entry, stack, argument, abi)
}
