//! x86-64's frames as `sched::inspect` reads them (`docs/DEBUG.md` §1.1).
//!
//! A 64-bit program's `SYSCALL` leaves a [`SyscallFrame`] at the top of its
//! task's kernel stack, at `gs:8`, which `set_entry_stack` points at that
//! top. Every other way in from ring 3 -- a fault, an interrupt, and a
//! 32-bit program's `int $0x80` -- is a [`TrapFrame`] pushed from `RSP0`, the
//! same top. So a task inside a call made through the 64-bit entry has the
//! first, and anything else the second. The two are 128 and 176 bytes, and
//! which one is read is decided by the entry's ABI and `in_call`, never by
//! guessing from the words.

use core::mem::offset_of;

use super::syscall::SyscallFrame;
use super::trap::TrapFrame;
use crate::arch::inspect::{EntryFrame, SwitchFrame};
use crate::trap::Abi;

/// A 64-bit program's system call, as `ferrix_syscall_stub` saved it.
const SYSCALL: EntryFrame = EntryFrame {
    bytes: size_of::<SyscallFrame>(),
    number: offset_of!(SyscallFrame, rax),
    args: [
        offset_of!(SyscallFrame, rdi),
        offset_of!(SyscallFrame, rsi),
        offset_of!(SyscallFrame, rdx),
        offset_of!(SyscallFrame, r10),
        offset_of!(SyscallFrame, r8),
        offset_of!(SyscallFrame, r9),
    ],
    sp: Some(offset_of!(SyscallFrame, user_rsp)),
    pc: offset_of!(SyscallFrame, rcx),
};

/// A trap from ring 3, and a 32-bit program's call through `int $0x80`,
/// whose arguments are i386's: EBX, ECX, EDX, ESI, EDI, EBP.
const TRAP: EntryFrame = EntryFrame {
    bytes: size_of::<TrapFrame>(),
    number: offset_of!(TrapFrame, rax),
    args: [
        offset_of!(TrapFrame, rbx),
        offset_of!(TrapFrame, rcx),
        offset_of!(TrapFrame, rdx),
        offset_of!(TrapFrame, rsi),
        offset_of!(TrapFrame, rdi),
        offset_of!(TrapFrame, rbp),
    ],
    sp: Some(offset_of!(TrapFrame, rsp)),
    pc: offset_of!(TrapFrame, rip),
};

/// The frame at the top of a task's kernel stack: the system call frame for
/// a call through the 64-bit entry, the trap frame otherwise.
pub(crate) const fn entry_frame(abi: Abi, in_call: bool) -> Option<EntryFrame> {
    match (abi, in_call) {
        (Abi::Native, true) => Some(SYSCALL),
        _ => Some(TRAP),
    }
}

/// `ferrix_switch` pushes RBP, RBX, R12, R13, R14, R15, so its saved stack
/// pointer names R15 and RBP is five words above it, with the return
/// address the `call` pushed above that.
pub(crate) const SWITCH_FRAME: SwitchFrame = SwitchFrame {
    frame_pointer: 5 * size_of::<u64>(),
    return_address: 6 * size_of::<u64>(),
};
