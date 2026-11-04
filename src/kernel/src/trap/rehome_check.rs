//! The core's answer to a system call filter, and what the architectures say of
//! an entry, on slots and values of the check's own: no personality, no program.
//! Each architecture's own are in `arch::rehome_check`, which it runs. Its caller is `syscall::rehome_check`, and it builds in the core's
//! crate, so that boundary itself says it names no load
//! (`docs/microkernel/B5.md`, class A).

use ferrix_sync::Once;

use crate::arch;
use crate::trap::{self, Abi, MAX_ERRNO, Outcome, SyscallArgs, SyscallFilter, Verdict};

/// A number no table has.
const UNKNOWN: usize = 0x7777;

/// A call of the check's own, first argument `first`.
const fn call(number: usize, first: u64) -> SyscallArgs {
    SyscallArgs {
        abi: Abi::Native,
        number,
        args: [first, 2, 3, 4, 5, 6],
        ip: 0x0040_1234,
    }
}

/// The errno the check's filter answers, and what it answers for a slot.
static ERRNO: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// A filter failing every call with [`ERRNO`].
fn failing(_: &SyscallArgs) -> Verdict {
    Verdict::Errno(ERRNO.load(core::sync::atomic::Ordering::Relaxed))
}

/// A filter with another errno, registered second.
fn other(_: &SyscallArgs) -> Verdict {
    Verdict::Errno(1)
}

/// A slot's filter asked, or the check's sentence.
fn asked(slot: &Once<SyscallFilter>, args: &SyscallArgs) -> Option<isize> {
    match trap::ask(slot, args) {
        Some(Outcome::Return(value)) => Some(value),
        _ => None,
    }
}

/// No filter, nothing answered; the first registration stands; and an errno is
/// cut to 4095 before it is the call's result.
///
/// Verifies: L.trap.7
fn check_the_filter_slot() -> Result<(), &'static str> {
    let args = call(UNKNOWN, 0);
    let slot: Once<SyscallFilter> = Once::new();
    if trap::ask(&slot, &args).is_some() {
        return Err("a call was answered with no filter registered");
    }
    trap::register(&slot, failing);
    trap::register(&slot, other);
    let max = MAX_ERRNO as isize;
    for (errno, expected) in [
        (13, -13),
        (0, 0),
        (512, -512),
        (MAX_ERRNO, -max),
        (MAX_ERRNO + 1, -max),
        (5000, -max),
        (u32::MAX, -max),
    ] {
        ERRNO.store(errno, core::sync::atomic::Ordering::Relaxed);
        if asked(&slot, &args) == Some(-1) {
            return Err("a later registration replaced the first");
        }
        if asked(&slot, &args) != Some(expected) {
            return Err("the core let a filter's errno out of 0 to 4095 through");
        }
    }
    Ok(())
}

/// A Trap verdict is answered with the architecture's rollback value of the
/// call as the entry read it and no other.
///
/// Verifies: L.trap.8
fn check_a_trap_verdict() -> Result<(), &'static str> {
    fn trapping(_: &SyscallArgs) -> Verdict {
        Verdict::Trap
    }
    let slot: Once<SyscallFilter> = Once::new();
    trap::register(&slot, trapping);
    for first in [0x1111, (-512_i64) as u64, (-516_i64) as u64, u64::MAX] {
        let args = call(0x2222, first);
        let value = arch::syscall_rollback_value(Abi::Native, args.number, &args.args);
        if asked(&slot, &args) != Some(value) {
            return Err("a Trap verdict was answered with other than the call's rollback value");
        }
    }
    Ok(())
}

/// Run them, and answer how many cases they are.
///
/// # Errors
///
/// The first thing that was not so, by name.
// CHECKS-ONLY: syscall/rehome_check.rs
pub fn run() -> Result<u64, &'static str> {
    check_the_filter_slot()?;
    check_a_trap_verdict()?;
    Ok(2 + arch::rehome_check::run()?)
}
