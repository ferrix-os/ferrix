//! The Ferrix kernel's run-time setting entries: every system call and file
//! write by which a program changes a system setting while the machine runs.
//!
//! A removable crate of the load ring (`docs/KERNEL-CRATES.md` §6; the
//! certification consultant's ledger lines 3558 (9) and 3623). It depends on
//! `ferrix-kernel-load` and nothing depends on it but the binary, whose
//! composition root names it only in a `runtime_settings = "live"` build:
//! there it binds [`Live`] as the load's `setters::Setters`, and in
//! a `fixed` build it names this crate nowhere, so none of it is linked.
//!
//! The entries moved here whole from the load files that dispatch them, each
//! file's body unchanged but for the paths it names
//! (`crate::` became `ferrix_kernel_load::`); what each changes, and the
//! mechanism it changes it through, stayed in the load. This crate defines no
//! exported, `#[used]` or section-placed item and no link-time hook, so a
//! build that does not name it links nothing of it (ledger 3623 (R2)).
#![no_std]
// The same allow as the load's, for this crate: a kernel without the Linux
// ABI is build-only and carries no claim (LINUX-ABI-SEPARATION.md §11.11),
// and every entry here is the Linux personality's. Never in a build with
// `linux_abi=on`.
#![cfg_attr(
    ferrix_linux_abi = "off",
    allow(
        dead_code,
        unused_imports,
        unused_variables,
        unfulfilled_lint_expectations,
        unused_crate_dependencies,
        reason = "linux_abi=off is build-only until the checks that run a Linux program are re-homed"
    )
)]

extern crate alloc;

/// The build flags this crate was compiled with (`build_flag_word!`, ledger
/// line 3282, (K4)).
#[allow(
    unused_qualifications,
    reason = "named by its crate's path, so the interface's list sees the core's fn used here"
)]
pub const SETTINGS_FLAG_WORD: u64 =
    ferrix_kernel_core::flag_word(&ferrix_kernel_core::build_flag_word!());

/// The entries, by what they set.
#[cfg(ferrix_linux_abi = "on")]
pub mod settings {
    pub mod clock;
    pub mod driver_bind;
    pub mod log;
    pub mod memfd_noexec;
    pub mod reboot;
    pub mod sysctl;
    pub mod sysrq;
    pub mod uts;
}

/// This crate's implementation of the load's run-time setting entries, which
/// a `runtime_settings = "live"` composition root binds
/// (`ferrix_kernel_load::bind_setters!`).
#[cfg(ferrix_linux_abi = "on")]
#[derive(Debug, Clone, Copy)]
pub struct Live;

#[cfg(ferrix_linux_abi = "on")]
mod live {
    use ferrix_kernel_load::setters::{Bus, Errno, Kernel, Process, Setters, TimeWidth};

    use super::{Live, settings};

    // Out of line, so each `__ferrix_setters_*` symbol of a `live` kernel
    // shows the call into this crate (check-removable-symbols, ledger 3934
    // (4)).
    impl Setters for Live {
        #[inline(never)]
        fn sethostname(process: &Process, name: u64, len: i32) -> Result<usize, Errno> {
            settings::uts::sys_sethostname(process, name, len)
        }
        #[inline(never)]
        fn setdomainname(process: &Process, name: u64, len: i32) -> Result<usize, Errno> {
            settings::uts::sys_setdomainname(process, name, len)
        }
        #[inline(never)]
        fn syslog_clear(process: &Process) -> Result<usize, Errno> {
            settings::log::clear(process)
        }
        #[inline(never)]
        fn syslog_read_clear(process: &Process, buf: u64, len: i32) -> Result<usize, Errno> {
            settings::log::read_clear(process, buf, len)
        }
        #[inline(never)]
        fn restart_with_word(process: &Process, word: u64) -> Result<usize, Errno> {
            settings::reboot::restart_with_word(process, word)
        }
        #[inline(never)]
        fn clock_settime(
            process: &Process,
            clock: i32,
            at: u64,
            width: TimeWidth,
        ) -> Result<usize, Errno> {
            settings::clock::sys_clock_settime(process, clock, at, width)
        }
        #[inline(never)]
        fn settimeofday(
            process: &Process,
            tv: u64,
            tz: u64,
            width: TimeWidth,
        ) -> Result<usize, Errno> {
            settings::clock::sys_settimeofday(process, tv, tz, width)
        }
        #[inline(never)]
        fn sysrq_trigger(kernel: &Kernel, data: &[u8]) -> Result<usize, Errno> {
            settings::sysrq::sysrq_trigger(kernel, data)
        }
        #[inline(never)]
        fn sysctl_hostname(kernel: &Kernel, data: &[u8]) -> Result<usize, Errno> {
            settings::sysctl::set_hostname(kernel, data)
        }
        #[inline(never)]
        fn sysctl_domainname(kernel: &Kernel, data: &[u8]) -> Result<usize, Errno> {
            settings::sysctl::set_domainname(kernel, data)
        }
        #[inline(never)]
        fn sysctl_memfd_noexec(process: &Process, data: &[u8]) -> Result<usize, Errno> {
            settings::memfd_noexec::set_noexec(process, data)
        }
        #[inline(never)]
        fn driver_bind(bus: Bus, driver: u16, unbind: bool, data: &[u8]) -> Result<usize, Errno> {
            settings::driver_bind::driver_bind(bus, driver, unbind, data)
        }
    }
}
