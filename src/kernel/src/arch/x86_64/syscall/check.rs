//! A program's saved registers, copied for a fork child, are the parent's with
//! the return register zero, and read back register by register.
//!
//! Verification, not the entry: a file of its own so that the manifest counts
//! it as the test it is (`tools/common/data/certification-item.json`,
//! `test_file_patterns`). A child of the entry's module, so it can make a
//! [`UserRegs`] from a frame, which nothing outside this architecture can.
//!
//! The register names are what a failure report shows of a thread: its
//! derived `Debug` is the only thing that prints a saved frame, and until this
//! ran no boot had printed one.

use alloc::format;

use super::{SyscallFrame, UserRegs};
use crate::console::println;

/// Copy a frame of distinct values for a child and read every register back.
///
/// # Errors
///
/// A register the copy changed, or one the printed frame does not name with
/// its value.
/// Verifies: `L.x86_64.62`
pub(crate) fn run() -> Result<(), &'static str> {
    let frame = SyscallFrame {
        r15: 15,
        r14: 14,
        r13: 13,
        r12: 12,
        rbp: 0x7fff_0000,
        rbx: 3,
        r9: 9,
        r8: 8,
        r10: 10,
        rdx: 2,
        rsi: 6,
        rdi: 7,
        rax: 57,
        r11: 0x202,
        rcx: 0x40_1000,
        user_rsp: 0x7fff_e000,
    };
    let child = UserRegs::Syscall(frame).for_child();
    if child.stack_pointer() != frame.user_rsp {
        return Err("a fork child's registers do not keep the parent's stack pointer");
    }
    let printed = format!("{child:?}");
    let expected = [
        "r15: 15,",
        "r14: 14,",
        "r13: 13,",
        "r12: 12,",
        "rbp: 2147418112,",
        "rbx: 3,",
        "r9: 9,",
        "r8: 8,",
        "r10: 10,",
        "rdx: 2,",
        "rsi: 6,",
        "rdi: 7,",
        // The one register a child sees changed: its `fork` returned zero.
        "rax: 0,",
        "r11: 514,",
        "rcx: 4198400,",
        "user_rsp: 2147475456 ",
    ];
    if !printed.starts_with("Syscall(SyscallFrame { ") {
        return Err("a thread's saved registers do not print as its frame");
    }
    if expected.iter().any(|field| !printed.contains(field)) {
        return Err("a fork child's saved registers are not the parent's with rax zero");
    }
    println!(
        "  regs     a fork child's saved registers are its parent's with rax zero, and all {} \
         print by name",
        expected.len()
    );
    Ok(())
}

/// Take the entry a system call of `abi` arrives through, with a frame built
/// from these registers as the processor leaves them, and answer what the
/// entry left in `RAX`.
///
/// For the seccomp check (`syscall::seccomp_check`), which needs the core's own
/// entry run with a call it chose -- the registered filter is asked first, and
/// the early answers (`arch_prctl`, the signal returns) follow -- without a
/// program to make it. `Native` is `SYSCALL`'s entry, `Compat` is `int $0x80`'s.
/// Interrupts are as the caller had them: a real entry starts with them masked
/// and leaves them so, this wrapper does both for a task that runs with them
/// open.
pub(crate) fn drive_system_call(
    abi: crate::trap::Abi,
    number: usize,
    args: [u64; 6],
    ip: u64,
) -> Option<isize> {
    use ferrix_sync::IrqControl;

    let saved = <super::super::Irq as IrqControl>::disable();
    let result = match abi {
        crate::trap::Abi::Native => {
            let mut frame = SyscallFrame {
                r15: 0,
                r14: 0,
                r13: 0,
                r12: 0,
                rbp: 0,
                rbx: 0,
                r9: args[5],
                r8: args[4],
                r10: args[3],
                rdx: args[2],
                rsi: args[1],
                rdi: args[0],
                rax: number as u64,
                r11: 0x202,
                rcx: ip,
                user_rsp: 0x7fff_e000,
            };
            super::ferrix_syscall_entry(&mut frame);
            Some(frame.rax as i64 as isize)
        }
        crate::trap::Abi::Compat => {
            let mut frame = super::super::trap::TrapFrame {
                r15: 0,
                r14: 0,
                r13: 0,
                r12: 0,
                r11: 0,
                r10: 0,
                r9: 0,
                r8: 0,
                rbp: args[5],
                rdi: args[4],
                rsi: args[3],
                rdx: args[2],
                rcx: args[1],
                rbx: args[0],
                rax: number as u64,
                vector: 0x80,
                error_code: 0,
                rip: ip,
                // Ring 3's 32-bit code and data selectors.
                cs: 0x23,
                rflags: 0x202,
                rsp: 0x7fff_e000,
                ss: 0x2b,
            };
            super::super::system_call(&mut frame)
                .ok()
                .map(|()| frame.rax as i64 as isize)
        }
    };
    <super::super::Irq as IrqControl>::restore(saved);
    result
}
