//! AArch64's frames as `sched::inspect` reads them (`docs/DEBUG.md` §1.1).
//!
//! Every exception from EL0, a system call's `SVC` among them, takes the
//! vector's `sub sp, sp, #304` on `SP_EL1` and stores a [`TrapFrame`] there:
//! one frame for a call and a trap alike, the number in `x8` and the
//! arguments in `x0` to `x5`. But `SP_EL1` is, at that moment, wherever it
//! stood at the task's last `eret`, which `ferrix_enter_user` and
//! `ferrix_resume_user` leave where their caller's stack was: no fixed place
//! the view could find, so it reads no call here yet (`docs/DEBUG.md` §1.1).
//! The chain, which starts at the switch frame, it reads.

use core::mem::offset_of;

use super::trap::TrapFrame;
use crate::arch::inspect::{EntryFrame, SwitchFrame};
use crate::trap::Abi;

/// One register of the frame's `x` array.
const fn x(register: usize) -> usize {
    offset_of!(TrapFrame, x) + register * size_of::<u64>()
}

/// The exception frame, as the vector stores it.
const FRAME: EntryFrame = EntryFrame {
    bytes: size_of::<TrapFrame>(),
    number: x(8),
    args: [x(0), x(1), x(2), x(3), x(4), x(5)],
    sp: Some(offset_of!(TrapFrame, sp)),
    pc: offset_of!(TrapFrame, elr),
};

/// The exception frame: the same for a call and a trap, and for the one ABI
/// there is.
///
/// Not served yet (`docs/DEBUG.md` §1.1): the exception's frame is pushed
/// on the stack pointer the task last returned to user mode with, which is
/// wherever the kernel stood when it first entered user mode, not a fixed
/// place. Until the user entry starts every task at its stack's top, the
/// view reads no call here.
pub(crate) const fn entry_frame(_abi: Abi, _in_call: bool) -> Option<EntryFrame> {
    let _ = FRAME;
    None
}

/// `ferrix_switch`'s first store is `stp x29, x30, [sp, #-96]!`, so the
/// saved stack pointer names x29 and the link register is the word above.
pub(crate) const SWITCH_FRAME: SwitchFrame = SwitchFrame {
    frame_pointer: 0,
    return_address: size_of::<u64>(),
};
