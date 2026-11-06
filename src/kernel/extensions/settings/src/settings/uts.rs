//! `sethostname` and `setdomainname`: the UTS namespace's names, set by a
//! program (moved whole from `syscall/system.rs`; `docs/KERNEL-CRATES.md`
//! §6.2). The names and their setters stay the namespace's, in the load.

use alloc::sync::Arc;

use ferrix_kernel_load::syscall::process::Process;
use ferrix_kernel_load::syscall::system::{NAME_MAX, SetName, UtsNamespace};
use ferrix_kernel_load::syscall::uaccess;
use ferrix_kernel_load::syscall::userns;
use ferrix_linux_abi::errno::Errno;

/// Read a name for `sethostname` or `setdomainname`.
///
/// `len` bytes exactly, NULs included, as `kernel/sys.c` copies them: the
/// length is the caller's statement of what the name is.
fn read_name(process: &Process, at: u64, len: i32) -> Result<SetName, Errno> {
    let len = usize::try_from(len)
        .ok()
        .filter(|&len| len <= NAME_MAX)
        .ok_or(Errno::EINVAL)?;
    let mut bytes = [0_u8; NAME_MAX];
    let name = bytes.get_mut(..len).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, name).map_err(|_| Errno::EFAULT)?;
    Ok((bytes, len))
}

/// The UTS namespace `process` is in, if it holds `CAP_SYS_ADMIN` over the
/// user namespace that owns it: Linux's `ns_capable(uts_ns->user_ns,
/// CAP_SYS_ADMIN)`. Root in the first namespace does; fake root does for the
/// namespace it made and never for the first's.
fn nameable(process: &Process) -> Result<Arc<UtsNamespace>, Errno> {
    let uts = process.nsproxy().uts;
    let allowed = process
        .with_credentials(|held| userns::capable_over(held, uts.owner(), userns::CAP_SYS_ADMIN));
    if allowed { Ok(uts) } else { Err(Errno::EPERM) }
}

/// `sethostname`: `CAP_SYS_ADMIN` over the namespace's owner, and at most 64
/// bytes, or `EINVAL`.
pub fn sys_sethostname(process: &Process, at: u64, len: i32) -> Result<usize, Errno> {
    let uts = nameable(process)?;
    let (bytes, len) = read_name(process, at, len)?;
    uts.set_hostname(bytes.get(..len).ok_or(Errno::EINVAL)?)?;
    Ok(0)
}

/// `setdomainname`: as `sethostname`, for the other field.
pub fn sys_setdomainname(process: &Process, at: u64, len: i32) -> Result<usize, Errno> {
    let uts = nameable(process)?;
    let (bytes, len) = read_name(process, at, len)?;
    uts.set_domainname(bytes.get(..len).ok_or(Errno::EINVAL)?)?;
    Ok(0)
}
