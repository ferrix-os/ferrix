//! What ARMv7-A says of its entry, on values of the check's own: no personality,
//! no program. Its caller is `trap::rehome_check::run`, and it builds in the
//! core's crate (`docs/microkernel/B5.md`, class A).

use crate::arch;
use crate::trap::Abi;

/// ARMv7-A's token is `AUDIT_ARCH_ARM` for the svc, and its rollback value the
/// first argument.
///
/// Verifies: L.armv7a.2
fn check_the_token_and_the_rollback() -> Result<(), &'static str> {
    let args = [0x1111, 2, 3, 4, 5, 6];
    if arch::syscall_rollback_value(Abi::Native, 0x2222, &args) != 0x1111 {
        return Err("a rolled-back frame would not read the first argument in r0");
    }
    if arch::audit_arch(Abi::Native) != 0x4000_0028 {
        return Err("the svc's token is not AUDIT_ARCH_ARM");
    }
    Ok(())
}

/// Run it, and answer how many cases it is.
///
/// # Errors
///
/// The first thing that was not so, by name.
pub(crate) fn run() -> Result<u64, &'static str> {
    check_the_token_and_the_rollback()?;
    Ok(1)
}
