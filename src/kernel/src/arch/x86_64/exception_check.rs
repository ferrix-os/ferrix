//! What K4's boot check needs to know about x86-64 (`docs/microkernel/K4.md`
//! §5): a program that takes a breakpoint, where a resumed program's exit
//! is, and the forged register sets case b offers a supervisor's write.
//!
//! Verification, not product code: the generic check
//! (`object::exception_check`) names no architecture.

use alloc::vec::Vec;

use ferrix_native_abi::exception::X86_64Registers as Registers;

// CHECKS-ONLY: object/exception_check.rs
/// `int3`, then `exit_group(97)`, which only a program resumed past it
/// reaches.
///
/// A program's `int3` is a breakpoint here (vector 3's gate is DPL 3 since
/// `SIGTRAP` came to programs): reported as [`BREAKPOINT_KIND`], its pc past
/// the one-byte instruction, as the processor leaves it.
///
/// ```text
///   int3
///   movl $231, %eax ; movl $97, %edi ; syscall
/// ```
#[doc(hidden)]
pub const BREAKPOINT_PROGRAM: &[u8] = &[
    0xcc, // int3
    0xb8, 0xe7, 0x00, 0x00, 0x00, // movl $231, %eax
    0xbf, 0x61, 0x00, 0x00, 0x00, // movl $97, %edi
    0x0f, 0x05, // syscall
];

// CHECKS-ONLY: object/exception_check.rs
/// How a program's breakpoint instruction is reported here.
#[doc(hidden)]
pub const BREAKPOINT_KIND: u32 = ferrix_native_abi::exception::KIND_BREAKPOINT;

// CHECKS-ONLY: object/exception_check.rs
/// The architecture's code a breakpoint's report carries: vector 3 over no
/// error code.
#[doc(hidden)]
pub const BREAKPOINT_CODE: Option<u64> = Some(3 << 32);

// CHECKS-ONLY: object/exception_check.rs
/// How far past the reported pc a supervisor resumes: none, the processor
/// has already stepped over the `int3`.
#[doc(hidden)]
pub const BREAKPOINT_SKIP: u64 = 0;

// CHECKS-ONLY: object/exception_check.rs
/// Whether a breakpoint's report says its pc is the trapping instruction's.
#[doc(hidden)]
pub const BREAKPOINT_PC_AT_TRAP: bool = false;

// CHECKS-ONLY: object/exception_check.rs
/// In `USER_FAULT_PROGRAM`: the faulting store's offset, and its `syscall`'s.
#[doc(hidden)]
pub const FAULT_AT: u64 = 2;
// CHECKS-ONLY: object/exception_check.rs
/// See [`FAULT_AT`].
#[doc(hidden)]
pub const FAULT_EXIT_AT: u64 = 15;

// CHECKS-ONLY: object/exception_check.rs
/// Point `registers` at an `exit_group(status)` instruction at `pc`.
#[doc(hidden)]
pub fn exit_with(registers: &mut Registers, pc: u64, status: u64) {
    registers.rip = pc;
    registers.general[0] = 231;
    registers.general[5] = status;
}

/// The program counter.
pub fn pc(registers: &Registers) -> u64 {
    registers.rip
}

// CHECKS-ONLY: object/exception_check.rs
/// Set the program counter.
#[doc(hidden)]
pub fn set_pc(registers: &mut Registers, pc: u64) {
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

// CHECKS-ONLY: object/exception_check.rs
/// Register sets a supervisor's write must be refused, each named, made
/// from the thread's own `base` (L.x86_64.186; the consultant's condition 5,
/// ledger 2871).
#[doc(hidden)]
pub fn forgeries(base: &Registers) -> Vec<(Registers, &'static str)> {
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
pub fn accepted(base: &Registers) -> Vec<(Registers, &'static str)> {
    let mut accepted = Vec::new();
    let mut ac = *base;
    ac.rflags |= 1 << 18;
    accepted.push((ac, "AC, which a program may set"));
    let mut general = *base;
    general.general[3] ^= 0x5a5a;
    accepted.push((general, "one general register changed"));
    accepted
}
