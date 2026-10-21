//! What K4's boot check needs to know about x86-64 (`docs/microkernel/K4.md`
//! §5): a program that takes a breakpoint, where a resumed program's exit
//! is, and the forged register sets case b offers a supervisor's write.
//!
//! Verification, not product code: the generic check
//! (`object::exception_check`) names no architecture.

use alloc::vec::Vec;

use ferrix_native_abi::exception::X86_64Registers as Registers;

/// `int3`, then `exit_group(97)`, which only a program resumed past it
/// reaches.
///
/// Every vector's gate but the legacy system call's has DPL 0 here
/// (`trap.rs`, the IDT), so a program's `int3` is a general protection fault
/// at the `int3` itself, not a breakpoint: reported as [`BREAKPOINT_KIND`],
/// vector 13, its pc at the instruction, which is one byte.
///
/// ```text
///   int3
///   movl $231, %eax ; movl $97, %edi ; syscall
/// ```
pub(crate) const BREAKPOINT_PROGRAM: &[u8] = &[
    0xcc, // int3
    0xb8, 0xe7, 0x00, 0x00, 0x00, // movl $231, %eax
    0xbf, 0x61, 0x00, 0x00, 0x00, // movl $97, %edi
    0x0f, 0x05, // syscall
];

/// How a program's breakpoint instruction is reported here: as the general
/// protection fault the DPL 0 gate makes it.
pub(crate) const BREAKPOINT_KIND: u32 = ferrix_native_abi::exception::KIND_OTHER;

/// The architecture's code a breakpoint's report carries: vector 13 over
/// the error code naming IDT vector 3 (`3 << 3 | 2`).
pub(crate) const BREAKPOINT_CODE: Option<u64> = Some(13 << 32 | 0x1a);

/// How far past a breakpoint's own address a supervisor resumes: the
/// `int3`'s one byte.
pub(crate) const BREAKPOINT_SKIP: u64 = 1;

/// Whether a breakpoint's report says its pc is the trapping instruction's.
pub(crate) const BREAKPOINT_PC_AT_TRAP: bool = true;

/// In `USER_FAULT_PROGRAM`: the faulting store's offset, and its `syscall`'s.
pub(crate) const FAULT_AT: u64 = 2;
/// See [`FAULT_AT`].
pub(crate) const FAULT_EXIT_AT: u64 = 15;

/// Point `registers` at an `exit_group(status)` instruction at `pc`.
pub(crate) fn exit_with(registers: &mut Registers, pc: u64, status: u64) {
    registers.rip = pc;
    registers.general[0] = 231;
    registers.general[5] = status;
}

/// The program counter.
pub(crate) fn pc(registers: &Registers) -> u64 {
    registers.rip
}

/// Set the program counter.
pub(crate) fn set_pc(registers: &mut Registers, pc: u64) {
    registers.rip = pc;
}

/// `rflags` bits a program may hold (`FLAGS_RESTORABLE`), and the two it
/// always holds, restated here so that the check does not take them from
/// the code it checks.
const ALLOWED_FLAGS: u64 = (1 << 0)
    | (1 << 2)
    | (1 << 4)
    | (1 << 6)
    | (1 << 7)
    | (1 << 10)
    | (1 << 11)
    | (1 << 18)
    | 0x202;

/// Register sets a supervisor's write must be refused, each named, made
/// from the thread's own `base` (L.x86_64.186; the consultant's condition 5,
/// ledger 2871).
pub(crate) fn forgeries(base: &Registers) -> Vec<(Registers, &'static str)> {
    let mut forged = Vec::new();
    let with = |change: &dyn Fn(&mut Registers)| {
        let mut registers = *base;
        change(&mut registers);
        registers
    };
    forged.push((with(&|r| r.cs = 0x08), "a kernel code selector"));
    forged.push((
        with(&|r| r.cs = 0x2b),
        "a 64-bit selector that is not user code",
    ));
    forged.push((
        with(&|r| {
            r.cs = 0x23;
            r.rip = 1 << 32;
        }),
        "32-bit user code with rip at 4 GiB",
    ));
    forged.push((with(&|r| r.ss = 0x10), "a kernel stack selector"));
    forged.push((with(&|r| r.rflags |= 3 << 12), "IOPL 3"));
    forged.push((with(&|r| r.rflags |= 1 << 17), "VM"));
    forged.push((with(&|r| r.rflags |= 1 << 8), "TF"));
    forged.push((with(&|r| r.rflags |= 1 << 14), "NT"));
    forged.push((with(&|r| r.rflags |= 1 << 16), "RF"));
    forged.push((with(&|r| r.rflags &= !(1 << 9)), "interrupts off"));
    for bit in 0..64 {
        if ALLOWED_FLAGS & (1 << bit) == 0 {
            forged.push((
                with(&|r| r.rflags |= 1 << bit),
                "a flag outside FLAGS_RESTORABLE",
            ));
        }
    }
    forged.push((with(&|r| r.rip = 0xffff_8000_0000_0000), "a kernel rip"));
    forged.push((
        with(&|r| r.general[7] = 0x0000_8000_0000_0000),
        "a non-canonical rsp",
    ));
    forged.push((with(&|r| r.reserved[0] = 1), "a reserved word"));
    forged
}

/// Register sets a supervisor's write must be accepted.
pub(crate) fn accepted(base: &Registers) -> Vec<(Registers, &'static str)> {
    let mut accepted = Vec::new();
    let mut ac = *base;
    ac.rflags |= 1 << 18;
    accepted.push((ac, "AC, which a program may set"));
    let mut general = *base;
    general.general[3] ^= 0x5a5a;
    accepted.push((general, "one general register changed"));
    accepted
}
