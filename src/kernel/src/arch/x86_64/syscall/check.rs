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
pub fn run() -> Result<(), &'static str> {
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
pub fn drive_system_call(
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
            // The checking task never left the kernel, whatever the entry's
            // way out said (`sched::vtime`).
            crate::sched::vtime::entered();
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

/// Take `SYSCALL`'s entry with a native call of `number` and these argument
/// registers, and answer what the entry left in `RAX` and in the second,
/// third and fourth argument registers (`RSI`, `RDX`, `R10`): where
/// `channel_write_read` answers a message's words, and what a call that
/// fails must leave as the program set them.
///
/// For the `channel_write_read` check (`object::write_read_check`), run on a
/// thread of a check's process, as [`drive_system_call`] is run.
pub fn drive_native_words(number: usize, args: [u64; 6]) -> (isize, [u64; 3]) {
    use ferrix_sync::IrqControl;

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
        rcx: 0x40_1000,
        user_rsp: 0x7fff_e000,
    };
    let saved = <super::super::Irq as IrqControl>::disable();
    super::ferrix_syscall_entry(&mut frame);
    // The checking task never left the kernel, whatever the entry's way out
    // said (`sched::vtime`).
    crate::sched::vtime::entered();
    <super::super::Irq as IrqControl>::restore(saved);
    (frame.rax as i64 as isize, [frame.rsi, frame.rdx, frame.r10])
}

/// Sort the numbers `SYSCALL`'s entry can be given the way it sorts them
/// before the dispatcher, and show that the native range is tested first.
///
/// The native number whose low bits are `arch_prctl`'s Linux number
/// (`0x1000 + 158`) is the one a Linux decode of the low bits would answer as
/// `arch_prctl`. Every number of the native range must sort as native, so
/// never reach the Linux table, and `arch_prctl` and `rt_sigreturn` must each
/// come from the one decode of their own number. The answer a program gets
/// for a native gap is the same either way, because the table's clamp sends a
/// number past its end to `None`; what the native test saves is the decode,
/// and this is what shows the test is there.
///
/// # Errors
///
/// A native number decoded against the Linux table, or one of the entry's two
/// calls not decoded as itself.
/// Verifies: `L.x86_64.140`
pub fn run_decode() -> Result<(), &'static str> {
    use ferrix_linux_abi::nr::{Syscall, x86_64};
    use ferrix_native_abi::nr::{FIRST, LAST};

    use super::{Early, early};

    if early(FIRST + x86_64::ARCH_PRCTL) != Early::Native {
        return Err(
            "a native number with arch_prctl's low bits was decoded against the Linux table",
        );
    }
    if (FIRST..=LAST).any(|number| early(number) != Early::Native) {
        return Err("a number of the native range was decoded against the Linux table");
    }
    if early(x86_64::ARCH_PRCTL) != Early::Linux(Some(Syscall::ArchPrctl)) {
        return Err("arch_prctl's number does not decode as arch_prctl at the entry");
    }
    if early(x86_64::RT_SIGRETURN) != Early::Linux(Some(Syscall::RtSigreturn)) {
        return Err("rt_sigreturn's number does not decode as rt_sigreturn at the entry");
    }
    if early(LAST + 1) != Early::Linux(None) || early(FIRST - 1) != Early::Linux(None) {
        return Err("a number either side of the native range is not a Linux number");
    }
    println!(
        "  decode   the native range ({FIRST:#x}..={LAST:#x}, {:#x} with arch_prctl's low bits \
         among it) skips the Linux table; arch_prctl and rt_sigreturn from one decode",
        FIRST + x86_64::ARCH_PRCTL
    );
    Ok(())
}

/// `L.x86_64.211`: every processor that has `RDTSCP` read its own logical
/// number back through it at bring-up. Answers how many processors
/// recorded, or `None` if none has the instruction.
///
/// # Errors
///
/// A processor whose `RDTSCP` named another number, or one that recorded
/// nothing while others did.
/// Verifies: `L.x86_64.211`
pub(crate) fn tsc_aux() -> Result<Option<usize>, &'static str> {
    let processors = crate::smp::count();
    let mut recorded = 0;
    for logical in 0..processors {
        let word = super::TSC_AUX_READ
            .get(logical)
            .map_or(0, |slot| slot.load(core::sync::atomic::Ordering::Acquire));
        if word & super::RECORDED == 0 {
            continue;
        }
        recorded += 1;
        let want = super::tsc_aux_of(logical);
        if word & 0xffff_ffff != want {
            return Err("a processor's RDTSCP did not name its own logical number (TSC_AUX)");
        }
    }
    if recorded == 0 {
        return Ok(None);
    }
    if recorded != processors {
        return Err("a processor recorded no TSC_AUX read-back while others did");
    }
    Ok(Some(recorded))
}
