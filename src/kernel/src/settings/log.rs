//! `syslog`'s two clears, `SYSLOG_ACTION_CLEAR` and `SYSLOG_ACTION_READ_CLEAR`
//! (from `syscall/system.rs`'s `sys_syslog` and `read_all`;
//! `docs/KERNEL-CRATES.md` §6.2). The log and its clear mark stay the load's;
//! the call's privilege check, which every action takes, stays in front of
//! both.

use core::sync::atomic::Ordering;

use ferrix_kernel_load::console::log;
use ferrix_kernel_load::syscall::process::Process;
use ferrix_kernel_load::syscall::system::{self, CLEARED};
use ferrix_linux_abi::errno::Errno;

/// `SYSLOG_ACTION_CLEAR` (5): `READ_ALL` starts from here on.
pub fn clear(_: &Process) -> Result<usize, Errno> {
    let _ = CLEARED.fetch_max(log::lines_begun(), Ordering::Relaxed);
    Ok(0)
}

/// `SYSLOG_ACTION_READ_CLEAR` (4): what `READ_ALL` reads, and then a clear
/// from the line after the last it read.
pub fn read_clear(process: &Process, buf: u64, len: i32) -> Result<usize, Errno> {
    let Some(len) = system::user_buffer(buf, len)? else {
        return Ok(0);
    };
    let (done, seq) = system::read_all(process, buf, len)?;
    let _ = CLEARED.fetch_max(seq, Ordering::Relaxed);
    Ok(done)
}
