//! Where a task that is not running keeps what `sched::inspect` reads of it
//! (`docs/DEBUG.md` §1.1): the frame its entry from user mode saved at the
//! top of its kernel stack, and the two slots of the frame `ferrix_switch`
//! left at its saved stack pointer that start its call chain.
//!
//! Every offset is a byte offset taken with `offset_of!` from the struct the
//! assembly is already asserted against, never a number written here, so a
//! frame reordered there moves the view with it. Each architecture's own
//! values are in its `inspect.rs`.

/// Where the entry from user mode put a program's registers, as byte offsets
/// from the frame's start, which is `bytes` below the top of the task's
/// kernel stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntryFrame {
    /// The frame's size: it begins this far below the stack's top.
    pub(crate) bytes: usize,
    /// The system call number.
    pub(crate) number: usize,
    /// The six argument registers, in order.
    pub(crate) args: [usize; 6],
    /// The program's stack pointer, where the frame holds it; `None` on
    /// ARMv7-A, whose user stack pointer is banked and not in the frame.
    pub(crate) sp: Option<usize>,
    /// Where the program resumes.
    pub(crate) pc: usize,
}

/// The two slots of the switch frame that start a switched-away task's call
/// chain, as byte offsets from its saved stack pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SwitchFrame {
    /// The frame pointer `ferrix_switch`'s caller ran with.
    pub(crate) frame_pointer: usize,
    /// Where `ferrix_switch` returns to.
    pub(crate) return_address: usize,
}
