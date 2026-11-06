//! `/proc/sysrq-trigger`'s write (moved whole from `fs/procfs.rs`;
//! `docs/KERNEL-CRATES.md` §6.2).

use ferrix_kernel_load::fs::procfs::Kernel;
use ferrix_kernel_load::panic::{catalog, fatal};
use ferrix_vfs::Result;

/// `/proc/sysrq-trigger`: writing `c` panics the kernel, on purpose.
///
/// The one request of Linux's magic `SysRq` set that is implemented, because it
/// is the one somebody working on the failure path needs: a way to reach the
/// report, the backtrace, the screen and its QR code from a shell, without
/// building a kernel with a fault in it. Linux looks only at the first byte,
/// and so does this. Any other byte is accepted and does nothing.
pub fn sysrq_trigger(_: &Kernel, data: &[u8]) -> Result<usize> {
    if data.first() == Some(&b'c') {
        fatal!(
            catalog::SYSRQ_CRASH,
            "panic requested through /proc/sysrq-trigger"
        );
    }
    Ok(data.len())
}
