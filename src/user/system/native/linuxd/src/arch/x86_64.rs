//! The stack switch, on x86-64.

/// Leave this stack for `top`, and call `start`, which does not return.
///
/// # Safety
///
/// `top` is the top of a stack of this thread's, and nothing on the old stack is used again.
pub(crate) unsafe fn onto_stack(top: usize, start: extern "C" fn() -> !) -> ! {
    // SAFETY: as the caller says.
    unsafe {
        core::arch::asm!(
            "mov rsp, {top}",
            "xor ebp, ebp",
            "call {start}",
            "ud2",
            top = in(reg) top,
            start = in(reg) start,
            options(noreturn),
        );
    }
}
