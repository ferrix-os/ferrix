//! The run-time setting entries, through one typed interface the binary's
//! composition root binds (`docs/KERNEL-CRATES.md` §6; the certification
//! consultant's ledger lines 3558 (9) and 3623).
//!
//! Each entry by which a program changes a system setting at run time -- a
//! system call's handler or a file's write, whole -- lives in the removable
//! crate `ferrix-kernel-settings`, which depends on this crate and never the
//! other way round. The load reaches it only through [`Setters`], a trait the
//! binary's composition root chooses the implementation of statically
//! (`docs/KERNEL-CRATES.md` §6.3, ledger 3934): `main.rs` invokes
//! [`bind_setters!`](crate::bind_setters) once, with the crate's `Live` in a
//! `runtime_settings = "live"` build and with [`Absent`] in a `fixed` one,
//! whose binary names the crate nowhere, so none of the entries' code is in
//! it. Every call site calls this module's function of the entry, a direct
//! call the linker resolves to the chosen implementation; a root that binds
//! nothing does not link.
//!
//! The state each entry changes, and the mechanism that changes it, stay
//! where they are (the UTS namespace's names, the wall clock, the log's
//! clear mark, `devmgr`'s requests): an entry is the only way a program
//! reaches the setting half of each.

pub use ferrix_linux_abi::errno::Errno;

pub use crate::discovery::devmgr::Bus;
pub use crate::fs::procfs::Kernel;
pub use crate::syscall::process::Process;
pub use crate::syscall::time::TimeWidth;

crate::seam! {
    $;
    /// The run-time setting entries, one associated function per entry.
    trait Setters;
    /// Bind [`Setters`] to the chosen type: the composition root's, once.
    bind bind_setters in setters;
    /// `sethostname(name, len)`.
    fn sethostname as __ferrix_setters_sethostname(
        process: &::ferrix_kernel_load::setters::Process,
        name: u64,
        len: i32,
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// `setdomainname(name, len)`.
    fn setdomainname as __ferrix_setters_setdomainname(
        process: &::ferrix_kernel_load::setters::Process,
        name: u64,
        len: i32,
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// `syslog(SYSLOG_ACTION_CLEAR)`, after the call's privilege check.
    fn syslog_clear as __ferrix_setters_syslog_clear(
        process: &::ferrix_kernel_load::setters::Process,
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// `syslog(SYSLOG_ACTION_READ_CLEAR, buf, len)`, after the call's
    /// privilege check.
    fn syslog_read_clear as __ferrix_setters_syslog_read_clear(
        process: &::ferrix_kernel_load::setters::Process,
        buf: u64,
        len: i32,
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// `reboot(.., RESTART2, word)`, after the call's privilege check and
    /// its magic numbers: the commits, the record, the word and the reset.
    fn restart_with_word as __ferrix_setters_restart_with_word(
        process: &::ferrix_kernel_load::setters::Process,
        word: u64,
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// `clock_settime(clock, at)` and `clock_settime64`.
    fn clock_settime as __ferrix_setters_clock_settime(
        process: &::ferrix_kernel_load::setters::Process,
        clock: i32,
        at: u64,
        width: ::ferrix_kernel_load::setters::TimeWidth,
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// `settimeofday(tv, tz)`.
    fn settimeofday as __ferrix_setters_settimeofday(
        process: &::ferrix_kernel_load::setters::Process,
        tv: u64,
        tz: u64,
        width: ::ferrix_kernel_load::setters::TimeWidth,
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// A write to `/proc/sysrq-trigger`.
    fn sysrq_trigger as __ferrix_setters_sysrq_trigger(
        kernel: &::ferrix_kernel_load::setters::Kernel,
        data: &[u8],
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// A write to `/proc/sys/kernel/hostname`.
    fn sysctl_hostname as __ferrix_setters_sysctl_hostname(
        kernel: &::ferrix_kernel_load::setters::Kernel,
        data: &[u8],
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// A write to `/proc/sys/kernel/domainname`.
    fn sysctl_domainname as __ferrix_setters_sysctl_domainname(
        kernel: &::ferrix_kernel_load::setters::Kernel,
        data: &[u8],
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// A write to `/proc/sys/vm/memfd_noexec` by the process given.
    fn sysctl_memfd_noexec as __ferrix_setters_sysctl_memfd_noexec(
        process: &::ferrix_kernel_load::setters::Process,
        data: &[u8],
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
    /// A write to a driver's `bind` (`false`) or `unbind` (`true`) under
    /// `/sys/bus/<bus>/drivers/<driver>/`.
    fn driver_bind as __ferrix_setters_driver_bind(
        bus: ::ferrix_kernel_load::setters::Bus,
        driver: u16,
        unbind: bool,
        data: &[u8],
    ) -> Result<usize, ::ferrix_kernel_load::setters::Errno>;
}

/// The implementation of a build without the crate: every entry is refused
/// before it reads an argument, `EPERM`, as Linux refuses a caller without
/// the capability the setting needs.
#[derive(Debug, Clone, Copy)]
pub struct Absent;

impl Setters for Absent {
    fn sethostname(_: &Process, _: u64, _: i32) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn setdomainname(_: &Process, _: u64, _: i32) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn syslog_clear(_: &Process) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn syslog_read_clear(_: &Process, _: u64, _: i32) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn restart_with_word(_: &Process, _: u64) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn clock_settime(_: &Process, _: i32, _: u64, _: TimeWidth) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn settimeofday(_: &Process, _: u64, _: u64, _: TimeWidth) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn sysrq_trigger(_: &Kernel, _: &[u8]) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn sysctl_hostname(_: &Kernel, _: &[u8]) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn sysctl_domainname(_: &Kernel, _: &[u8]) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn sysctl_memfd_noexec(_: &Process, _: &[u8]) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
    fn driver_bind(_: Bus, _: u16, _: bool, _: &[u8]) -> Result<usize, Errno> {
        Err(Errno::EPERM)
    }
}
