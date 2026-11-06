//! A write to `/sys/devices/system/cpu/cpuN/online`: root taking a
//! processor out of service, or bringing a quarantined one back at run time
//! (`docs/CORE-QUARANTINE.md` §4, the customer's choice of 2026-10-30). The
//! change itself is `smp::service`'s, the same function lode's door ends in.

use ferrix_kernel_load::audit;
use ferrix_kernel_load::object::process::Host;
use ferrix_kernel_load::sched::ServiceRefusal;
use ferrix_kernel_load::smp::service::{self, Door, Refusal};
use ferrix_kernel_load::syscall::process::Process;
use ferrix_kernel_load::syscall::userns::{self, CAP_SYS_ADMIN};
use ferrix_linux_abi::errno::Errno;

/// A write of `data` to processor `cpu`'s `online` by `process`: `0` takes
/// it out of service, `1` brings it back, each with white space around it
/// allowed. Linux's `online_store` takes any `kstrtobool` word; `chcpu` and
/// a shell's `echo` write these two.
///
/// # Errors
///
/// `EPERM` without `CAP_SYS_ADMIN` in the first user namespace, asked
/// first -- a processor is the machine's, not a container's; `EINVAL` for
/// anything but `0` or `1`; `EBUSY` for a processor that may not go
/// (`smp::service::Refusal`), `ETIMEDOUT` for one that did not answer.
pub fn cpu_online(process: &Process, cpu: u32, data: &[u8]) -> Result<usize, Errno> {
    if !process.with_credentials(|held| userns::capable_over(held, userns::first(), CAP_SYS_ADMIN))
    {
        return Err(Errno::EPERM);
    }
    let wanted = match data.trim_ascii() {
        b"0" => false,
        b"1" => true,
        _ => return Err(Errno::EINVAL),
    };
    let subject = audit::Subject::of(Host::core(process));
    let cpu = usize::try_from(cpu).map_err(|_| Errno::ENODEV)?;
    service::set_in_service(cpu, wanted, Door::Root, subject).map_err(errno_of)?;
    Ok(data.len())
}

/// What `online_store` answers for each refusal.
const fn errno_of(refusal: Refusal) -> Errno {
    match refusal {
        Refusal::NoSuchProcessor => Errno::ENODEV,
        Refusal::Unsupported => Errno::EOPNOTSUPP,
        Refusal::Scheduler(ServiceRefusal::TimedOut) => Errno::ETIMEDOUT,
        Refusal::Interrupts | Refusal::Scheduler(_) => Errno::EBUSY,
    }
}
