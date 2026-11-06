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

use crate::arch::aarch64 as core_aarch64;
use crate::arch::aarch64::trap as core_trap;

// CHECKS-ONLY: syscall/check.rs, syscall/seccomp_check.rs, syscall/seccomp_filters_check.rs, syscall/seccomp/user_dispatch_check.rs, object/fast_path_check.rs, object/sender_check.rs, object/space_check.rs, object/write_read_check.rs, through `arch`; home: in crate::arch
/// Take the entry a system call arrives through, with a frame built from these
/// registers as an `svc` from EL0 leaves them, and answer what the entry left
/// in `x0`.
///
/// For the seccomp check (`syscall::seccomp_check`), which needs the core's own
/// entry run with a call it chose -- the registered filter is asked first, and
/// the early answers (`rt_sigreturn`) follow -- without a program to make it.
/// Interrupts are as the caller had them: a real entry starts with them masked
/// and leaves them so, this wrapper does both for a task that runs with them
/// open. `None` for a mode of entry this architecture does not have.
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
        x: [0; 31],
        sp: 0x7fff_e000,
        elr: ip,
        spsr: core_trap::USER_SPSR,
        // An `svc` from AArch64, class 0b010101, from a lower exception level.
        esr: 0b01_0101 << 26,
        far: 0,
        kind: 8,
        reserved: 0,
    };
    frame.x[..6].copy_from_slice(&args);
    frame.x[8] = number as u64;
    let saved = <core_aarch64::Irq as IrqControl>::disable();
    let done = core_trap::system_call(&mut frame);
    <core_aarch64::Irq as IrqControl>::restore(saved);
    done.ok()?;
    Some(frame.x[0] as i64 as isize)
}

// CHECKS-ONLY: syscall/check.rs, syscall/seccomp_check.rs, syscall/seccomp_filters_check.rs, syscall/seccomp/user_dispatch_check.rs, object/fast_path_check.rs, object/sender_check.rs, object/space_check.rs, object/write_read_check.rs, through `arch`; home: in crate::arch
/// Take the `svc` entry with a native call of `number` and these argument
/// registers, and answer what the entry left in `x0` and in `x1` to `x3`:
/// where `channel_write_read` answers a message's words, and what a call
/// that fails must leave as the program set them.
///
/// For the `channel_write_read` check (`object::write_read_check`), run on a
/// thread of a check's process, as [`drive_system_call`] is run. An entry
/// that refuses the frame answers `ENOSYS` with the registers as sent.
pub fn drive_native_words(number: usize, args: [u64; 6]) -> (isize, [u64; 3]) {
    use ferrix_sync::IrqControl;

    let mut frame = core_trap::TrapFrame {
        x: [0; 31],
        sp: 0x7fff_e000,
        elr: 0x40_1000,
        spsr: core_trap::USER_SPSR,
        // An `svc` from AArch64, class 0b010101, from a lower exception level.
        esr: 0b01_0101 << 26,
        far: 0,
        kind: 8,
        reserved: 0,
    };
    frame.x[..6].copy_from_slice(&args);
    frame.x[8] = number as u64;
    let saved = <core_aarch64::Irq as IrqControl>::disable();
    let done = core_trap::system_call(&mut frame);
    <core_aarch64::Irq as IrqControl>::restore(saved);
    let value = if done.is_ok() {
        frame.x[0] as i64 as isize
    } else {
        ferrix_linux_abi::errno::Errno::ENOSYS.as_return_value()
    };
    (value, [frame.x[1], frame.x[2], frame.x[3]])
}
