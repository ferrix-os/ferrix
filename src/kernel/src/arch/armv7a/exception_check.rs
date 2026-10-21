//! What K4's boot check needs to know about ARMv7-A
//! (`docs/microkernel/K4.md` §5): a program that takes a breakpoint, where a
//! resumed program's exit is, and the forged register sets case b offers a
//! supervisor's write.
//!
//! Verification, not product code: the generic check
//! (`object::exception_check`) names no architecture.

use alloc::vec::Vec;

use ferrix_native_abi::exception::Armv7aRegisters as Registers;

/// `bkpt #0`, then `exit_group(97)`, which only a program resumed past its
/// breakpoint reaches. ARM state.
///
/// ```text
///   bkpt #0
///   mov r7, #248 ; mov r0, #97 ; svc #0
/// ```
pub(crate) const BREAKPOINT_PROGRAM: &[u8] = &[
    0x70, 0x00, 0x20, 0xe1, // bkpt #0
    0xf8, 0x70, 0xa0, 0xe3, // mov  r7, #248
    0x61, 0x00, 0xa0, 0xe3, // mov  r0, #97
    0x00, 0x00, 0x00, 0xef, // svc  #0
];

/// How a program's breakpoint instruction is reported.
pub(crate) const BREAKPOINT_KIND: u32 = ferrix_native_abi::exception::KIND_BREAKPOINT;

/// The architecture's code a breakpoint's report carries: not compared,
/// the `IFSR` a `bkpt` leaves being the debug event's encoding of the core.
pub(crate) const BREAKPOINT_CODE: Option<u64> = None;

/// How far past a breakpoint's own address a supervisor resumes: `bkpt`
/// leaves the pc at itself.
pub(crate) const BREAKPOINT_SKIP: u64 = 4;

/// Whether a breakpoint's report says its pc is the trapping instruction's.
pub(crate) const BREAKPOINT_PC_AT_TRAP: bool = true;

/// In `USER_FAULT_PROGRAM`: the faulting store's offset, and its `svc`'s.
pub(crate) const FAULT_AT: u64 = 4;
/// See [`FAULT_AT`].
pub(crate) const FAULT_EXIT_AT: u64 = 16;

/// Point `registers` at an `exit_group(status)` instruction at `pc`.
pub(crate) fn exit_with(registers: &mut Registers, pc: u64, status: u64) {
    registers.r[15] = pc as u32;
    registers.r[7] = 248;
    registers.r[0] = status as u32;
}

/// The program counter.
pub(crate) fn pc(registers: &Registers) -> u64 {
    u64::from(registers.r[15])
}

/// Set the program counter.
pub(crate) fn set_pc(registers: &mut Registers, pc: u64) {
    registers.r[15] = pc as u32;
}

/// Register sets a supervisor's write must be refused, each named, made
/// from the thread's own `base` (L.armv7a.28; the consultant's condition 5,
/// ledger 2871).
pub(crate) fn forgeries(base: &Registers) -> Vec<(Registers, &'static str)> {
    let mut forged = Vec::new();
    let with = |change: &dyn Fn(&mut Registers)| {
        let mut registers = *base;
        change(&mut registers);
        registers
    };
    forged.push((with(&|r| r.cpsr = r.cpsr & !0x1f | 0x13), "SVC mode"));
    forged.push((with(&|r| r.cpsr = r.cpsr & !0x1f | 0x1f), "SYS mode"));
    forged.push((with(&|r| r.cpsr |= 1 << 7), "IRQs masked"));
    forged.push((with(&|r| r.cpsr &= !(1 << 6)), "FIQs unmasked"));
    forged.push((with(&|r| r.cpsr |= 1 << 24), "J"));
    forged.push((with(&|r| r.cpsr |= 1 << 9), "E"));
    forged.push((
        with(&|r| r.r[15] |= 1),
        "the pc's low bit, which is never a switch to Thumb",
    ));
    forged.push((with(&|r| r.r[15] |= 2), "a pc misaligned for ARM state"));
    forged.push((
        with(&|r| {
            r.cpsr |= 1 << 5;
            r.r[15] |= 1;
        }),
        "an odd pc in Thumb state",
    ));
    forged.push((with(&|r| r.r[15] = 0xffff_0000), "a kernel pc"));
    forged.push((with(&|r| r.r[13] = 0xffff_0000), "a kernel sp"));
    forged.push((with(&|r| r.reserved = 1), "a reserved word"));
    forged
}

/// Register sets a supervisor's write must be accepted.
pub(crate) fn accepted(base: &Registers) -> Vec<(Registers, &'static str)> {
    let mut accepted = Vec::new();
    let mut flags = *base;
    flags.cpsr |= 1 << 31;
    accepted.push((flags, "the N flag, which a program may set"));
    let mut thumb = *base;
    thumb.cpsr |= 1 << 5;
    thumb.r[15] &= !1;
    accepted.push((thumb, "Thumb state with a halfword-aligned pc"));
    let mut general = *base;
    general.r[3] ^= 0x5a5a;
    accepted.push((general, "one general register changed"));
    accepted
}
