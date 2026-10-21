//! ARMv7-A's frames as `sched::inspect` reads them (`docs/DEBUG.md` §1.1).
//!
//! Every exception from user mode, `SVC` among them, `srsdb`s its return
//! address and status onto the SVC-mode stack and pushes the rest of a
//! [`TrapFrame`] there (`trap.rs`): one frame for a call and a trap, the
//! number in `r7` (EABI) and the arguments in `r0` to `r5`, with no user
//! stack pointer, which is banked. The SVC stack pointer is then wherever it
//! stood at the task's last return to user mode, no fixed place, so the view
//! reads no call here yet (`docs/DEBUG.md` §1.1). The chain, which starts at
//! the switch frame, it reads.

use core::mem::offset_of;

use super::trap::TrapFrame;
use crate::arch::inspect::{EntryFrame, SwitchFrame};
use crate::trap::Abi;

/// One register of the frame's `r` array.
const fn r(register: usize) -> usize {
    offset_of!(TrapFrame, r) + register * size_of::<u32>()
}

/// The exception frame, as the vector stores it.
const FRAME: EntryFrame = EntryFrame {
    bytes: size_of::<TrapFrame>(),
    number: r(7),
    args: [r(0), r(1), r(2), r(3), r(4), r(5)],
    sp: None,
    pc: offset_of!(TrapFrame, pc),
};

/// The exception frame: the same for a call and a trap.
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

/// `ferrix_switch` is `push {r4-r11, lr}` then `sub sp, sp, #4`: the saved
/// stack pointer names the padding word, r4 is one word above, the frame
/// pointer r11 eight, and the link register nine.
pub(crate) const SWITCH_FRAME: SwitchFrame = SwitchFrame {
    frame_pointer: size_of::<[u32; 8]>(),
    return_address: size_of::<[u32; 9]>(),
};
