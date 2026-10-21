//! What K4's boot check needs to know about `AArch64`
//! (`docs/microkernel/K4.md` §5): a program that takes a breakpoint, where a
//! resumed program's exit is, and the forged register sets case b offers a
//! supervisor's write.
//!
//! Verification, not product code: the generic check
//! (`object::exception_check`) names no architecture.

use alloc::vec::Vec;

use ferrix_native_abi::exception::Aarch64Registers as Registers;

/// `brk #0`, then `exit_group(97)`, which only a program resumed past its
/// breakpoint reaches.
///
/// ```text
///   brk #0
///   mov x8, #94 ; mov x0, #97 ; svc #0
/// ```
pub(crate) const BREAKPOINT_PROGRAM: &[u8] = &[
    0x00, 0x00, 0x20, 0xd4, // brk  #0
    0xc8, 0x0b, 0x80, 0xd2, // mov  x8, #94
    0x20, 0x0c, 0x80, 0xd2, // mov  x0, #97
    0x01, 0x00, 0x00, 0xd4, // svc  #0
];

/// How a program's breakpoint instruction is reported.
pub(crate) const BREAKPOINT_KIND: u32 = ferrix_native_abi::exception::KIND_BREAKPOINT;

/// The architecture's code a breakpoint's report carries: `ESR_EL1` for
/// `brk #0`, exception class 0x3C with `IL` set.
pub(crate) const BREAKPOINT_CODE: Option<u64> = Some(0xf200_0000);

/// How far past a breakpoint's own address a supervisor resumes: `brk`
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
    registers.pc = pc;
    registers.x[8] = 94;
    registers.x[0] = status;
}

/// The program counter.
pub(crate) fn pc(registers: &Registers) -> u64 {
    registers.pc
}

/// Set the program counter.
pub(crate) fn set_pc(registers: &mut Registers, pc: u64) {
    registers.pc = pc;
}

/// Register sets a supervisor's write must be refused, each named, made
/// from the thread's own `base` (L.aarch64.66; the consultant's condition 5,
/// ledger 2871).
pub(crate) fn forgeries(base: &Registers) -> Vec<(Registers, &'static str)> {
    let mut forged = Vec::new();
    let with = |change: &dyn Fn(&mut Registers)| {
        let mut registers = *base;
        change(&mut registers);
        registers
    };
    forged.push((with(&|r| r.pstate |= 0b0101), "EL1h"));
    forged.push((with(&|r| r.pstate |= 0b0100), "EL1t"));
    forged.push((with(&|r| r.pstate |= 1 << 7), "IRQs masked"));
    forged.push((with(&|r| r.pstate &= !(1 << 9)), "D unmasked"));
    forged.push((with(&|r| r.pstate |= 1 << 21), "SS"));
    forged.push((with(&|r| r.pstate |= 1 << 20), "IL"));
    forged.push((with(&|r| r.pstate |= 1 << 4), "AArch32 state"));
    forged.push((with(&|r| r.pc = 0xffff_0000_0000_1000), "a kernel pc"));
    forged.push((with(&|r| r.pc |= 2), "a misaligned pc"));
    forged.push((with(&|r| r.sp = 0xffff_0000_0000_1000), "a kernel sp"));
    forged
}

/// Register sets a supervisor's write must be accepted.
pub(crate) fn accepted(base: &Registers) -> Vec<(Registers, &'static str)> {
    let mut accepted = Vec::new();
    let mut flags = *base;
    flags.pstate |= 1 << 31;
    accepted.push((flags, "the N flag, which a program may set"));
    let mut general = *base;
    general.x[3] ^= 0x5a5a;
    accepted.push((general, "one general register changed"));
    accepted
}
