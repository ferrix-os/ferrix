//! `/proc/sys/vm/memfd_noexec`'s judge: a pid namespace's scope, set by a
//! program (moved whole from `syscall/memfd.rs`; `docs/KERNEL-CRATES.md`
//! §6.2). The scope stays its namespace's, in the load.

use ferrix_kernel_load::syscall::memfd::SCOPE_NOEXEC_ENFORCED;
use ferrix_kernel_load::syscall::pidns;
use ferrix_kernel_load::syscall::process::Process;
use ferrix_kernel_load::syscall::userns::CAP_SYS_ADMIN;
use ferrix_linux_abi::errno::Errno;

/// A write of `data` to `/proc/sys/vm/memfd_noexec` by `process`: Linux's
/// `pid_mfd_noexec_dointvec_minmax`. One decimal integer, white space around
/// it allowed, from the scope of the namespace's parent -- a namespace may
/// not be set laxer than the one it was made in -- to
/// [`SCOPE_NOEXEC_ENFORCED`]; it sets the writer's own pid namespace.
///
/// Two differences from Linux, both in `docs/BACKLOG.md`: the number is
/// decimal, with an optional `+`, where `proc_dointvec` takes any base and
/// no `+`; and `CAP_SYS_ADMIN` is asked in the writer's own user namespace,
/// where Linux asks it over the pid namespace's owner, which a
/// `PidNamespace` does not record yet.
///
/// # Errors
///
/// `EPERM` without `CAP_SYS_ADMIN`, asked first; `EINVAL` for anything but
/// such a number.
pub fn set_noexec(process: &Process, data: &[u8]) -> Result<usize, Errno> {
    if !process.with_credentials(|held| held.holds(CAP_SYS_ADMIN)) {
        return Err(Errno::EPERM);
    }
    let namespace = process.numbers().map(|numbers| numbers.namespace());
    let floor = pidns::memfd_noexec_floor(namespace);
    let scope = core::str::from_utf8(data)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|scope| (floor..=SCOPE_NOEXEC_ENFORCED).contains(scope))
        .ok_or(Errno::EINVAL)?;
    pidns::set_memfd_noexec(namespace, scope);
    Ok(data.len())
}
