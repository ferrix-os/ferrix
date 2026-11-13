//! The core's own system call entry, driven from a frame a check builds:
//! what `syscall::seccomp_check` and `object::write_read_check` run a call
//! through without a program to make it.
//!
//! Verification, not the entry: a file of its own so that the manifest counts
//! it as the test it is (`tools/common/data/certification-item.json`,
//! `test_file_patterns`). A child of the entry's module, built by the core as
//! x86-64's `syscall/check.rs` is, because the load's checks call it and the
//! load cannot name the raised checks' crate (`docs/microkernel/CRATE-SPLIT.md`
//! S5). Until S5 these two functions were in the raised `trap/check.rs`, the
//! bodies unchanged.

use crate::arch::armv7a as core_armv7a;
use crate::arch::armv7a::trap as core_trap;

// CHECKS-ONLY: syscall/check.rs, syscall/seccomp_check.rs, syscall/seccomp_filters_check.rs, syscall/seccomp/user_dispatch_check.rs, object/fast_path_check.rs, object/sender_check.rs, object/space_check.rs, object/write_read_check.rs, through `arch`; home: in crate::arch
/// Take the entry a system call arrives through, with a frame built from these
/// registers as an `svc` from USR mode leaves them, and answer what the entry
/// left in `r0`.
///
/// For the seccomp check (`syscall::seccomp_check`), which needs the core's own
/// entry run with a call it chose -- the registered filter is asked first, and
/// the early answers (`set_tls`, the signal returns) follow -- without a
/// program to make it. Interrupts are as the caller had them: a real entry
/// starts with them masked and leaves them so, this wrapper does both for a
/// task that runs with them open. `None` for a mode of entry this architecture
/// does not have.
pub fn drive_system_call(
    abi: crate::trap::Abi,
    number: usize,
    args: [u64; 6],
    ip: u64,
) -> Option<isize> {
    use ferrix_sync::IrqControl;

    if abi != crate::trap::Abi::Native {
        return None;
    }
    let mut frame = core_trap::TrapFrame {
        kind: 2,
        fsr: 0,
        far: 0,
        reserved: 0,
        r: [0; 13],
        lr: 0,
        pc: ip as u32,
        cpsr: core_trap::USER_CPSR,
    };
    for (register, value) in frame.r.iter_mut().zip(args) {
        *register = value as u32;
    }
    frame.r[7] = number as u32;
    let saved = <core_armv7a::Irq as IrqControl>::disable();
    let done = core_trap::system_call(&mut frame);
    <core_armv7a::Irq as IrqControl>::restore(saved);
    done.ok()?;
    Some(frame.r[0] as i32 as isize)
}

// CHECKS-ONLY: syscall/check.rs, syscall/seccomp_check.rs, syscall/seccomp_filters_check.rs, syscall/seccomp/user_dispatch_check.rs, object/fast_path_check.rs, object/sender_check.rs, object/space_check.rs, object/write_read_check.rs, through `arch`; home: in crate::arch
/// Take the `svc` entry with a native call of `number` and these argument
/// registers, and answer what the entry left in `r0` and in `r1` to `r3`:
/// where `channel_write_read` answers a message's words, and what a call
/// that fails must leave as the program set them. Each register is 32 bits,
/// as a 32-bit program's words are.
///
/// For the `channel_write_read` check (`object::write_read_check`), run on a
/// thread of a check's process, as [`drive_system_call`] is run. An entry
/// that refuses the frame answers `ENOSYS` with the registers as sent.
pub fn drive_native_words(number: usize, args: [u64; 6]) -> (isize, [u64; 3]) {
    use ferrix_sync::IrqControl;

    let mut frame = core_trap::TrapFrame {
        kind: 2,
        fsr: 0,
        far: 0,
        reserved: 0,
        r: [0; 13],
        lr: 0,
        pc: 0x40_1000,
        cpsr: core_trap::USER_CPSR,
    };
    for (register, value) in frame.r.iter_mut().zip(args) {
        *register = value as u32;
    }
    frame.r[7] = number as u32;
    let saved = <core_armv7a::Irq as IrqControl>::disable();
    let done = core_trap::system_call(&mut frame);
    <core_armv7a::Irq as IrqControl>::restore(saved);
    let value = if done.is_ok() {
        frame.r[0] as i32 as isize
    } else {
        ferrix_errno::Errno::ENOSYS.as_return_value()
    };
    (
        value,
        [
            u64::from(frame.r[1]),
            u64::from(frame.r[2]),
            u64::from(frame.r[3]),
        ],
    )
}
