//! The run-time setting entries, through one typed interface the binary's
//! composition root binds (`docs/KERNEL-CRATES.md` §6; the certification
//! consultant's ledger lines 3558 (9) and 3623).
//!
//! Each entry by which a program changes a system setting at run time -- a
//! system call's handler or a file's write, whole -- lives in the removable
//! crate `ferrix-kernel-settings`, which depends on this crate and never the
//! other way round. The load reaches it only through [`Setters`]: a struct of
//! function pointers, one per entry, registered once by `main.rs` before the
//! first user program. A `runtime_settings = "live"` build registers the
//! crate's [`Setters`]; a `fixed` one registers [`ABSENT`], and its
//! binary names the crate nowhere, so none of the entries' code is in it.
//! Until something is registered, [`bound`] answers [`ABSENT`]: the
//! interface fails closed.
//!
//! The state each entry changes, and the mechanism that changes it, stay
//! where they are (the UTS namespace's names, the wall clock, the log's
//! clear mark, `devmgr`'s requests): an entry is the only way a program
//! reaches the setting half of each.

use ferrix_linux_abi::errno::Errno;
use ferrix_sync::Once;

use crate::discovery::devmgr::Bus;
use crate::fs::procfs::Kernel;
use crate::syscall::process::Process;
use crate::syscall::time::TimeWidth;

/// The run-time setting entries, one function per entry. There is no
/// `Default` and no constructor: the two bindings are statics, the
/// crate's and [`ABSENT`].
#[derive(Debug, Clone, Copy)]
pub struct Setters {
    /// `sethostname(name, len)`.
    pub sethostname: fn(&Process, u64, i32) -> Result<usize, Errno>,
    /// `setdomainname(name, len)`.
    pub setdomainname: fn(&Process, u64, i32) -> Result<usize, Errno>,
    /// `syslog(SYSLOG_ACTION_CLEAR)`, after the call's privilege check.
    pub syslog_clear: fn(&Process) -> Result<usize, Errno>,
    /// `syslog(SYSLOG_ACTION_READ_CLEAR, buf, len)`, after the call's
    /// privilege check.
    pub syslog_read_clear: fn(&Process, u64, i32) -> Result<usize, Errno>,
    /// `reboot(.., RESTART2, word)`, after the call's privilege check and
    /// its magic numbers: the commits, the record, the word and the reset.
    pub restart_with_word: fn(&Process, u64) -> Result<usize, Errno>,
    /// `clock_settime(clock, at)` and `clock_settime64`.
    pub clock_settime: fn(&Process, i32, u64, TimeWidth) -> Result<usize, Errno>,
    /// `settimeofday(tv, tz)`.
    pub settimeofday: fn(&Process, u64, u64, TimeWidth) -> Result<usize, Errno>,
    /// A write to `/proc/sysrq-trigger`.
    pub sysrq_trigger: fn(&Kernel, &[u8]) -> Result<usize, Errno>,
    /// A write to `/proc/sys/kernel/hostname`.
    pub sysctl_hostname: fn(&Kernel, &[u8]) -> Result<usize, Errno>,
    /// A write to `/proc/sys/kernel/domainname`.
    pub sysctl_domainname: fn(&Kernel, &[u8]) -> Result<usize, Errno>,
    /// A write to `/proc/sys/vm/memfd_noexec` by the process given.
    pub sysctl_memfd_noexec: fn(&Process, &[u8]) -> Result<usize, Errno>,
    /// A write to a driver's `bind` (`false`) or `unbind` (`true`) under
    /// `/sys/bus/<bus>/drivers/<driver>/`.
    pub driver_bind: fn(Bus, u16, bool, &[u8]) -> Result<usize, Errno>,
}

/// How many entries [`Setters`] has, which the stage check holds every
/// binding's answers to (ledger 3623 (R15)).
pub const ENTRIES: usize = 12;

/// The binding of a build without the crate: every entry is refused before
/// it reads an argument, `EPERM`, as Linux refuses a caller without the
/// capability the setting needs. A static, so that the stage check can tell
/// it from the crate's binding by address.
pub static ABSENT: Setters = Setters {
    sethostname: |_, _, _| Err(Errno::EPERM),
    setdomainname: |_, _, _| Err(Errno::EPERM),
    syslog_clear: |_| Err(Errno::EPERM),
    syslog_read_clear: |_, _, _| Err(Errno::EPERM),
    restart_with_word: |_, _| Err(Errno::EPERM),
    clock_settime: |_, _, _, _| Err(Errno::EPERM),
    settimeofday: |_, _, _, _| Err(Errno::EPERM),
    sysrq_trigger: |_, _| Err(Errno::EPERM),
    sysctl_hostname: |_, _| Err(Errno::EPERM),
    sysctl_domainname: |_, _| Err(Errno::EPERM),
    sysctl_memfd_noexec: |_, _| Err(Errno::EPERM),
    driver_bind: |_, _, _, _| Err(Errno::EPERM),
};

/// What the composition root registered.
static BOUND: Once<&'static Setters> = Once::new();

/// A second registration: the composition root registers once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Twice;

/// Register `setters` as the build's binding: once, from the composition
/// root, before the first user program.
///
/// # Errors
///
/// [`Twice`] when something was registered already, which the composition
/// root stops the boot on.
pub fn bind(setters: &'static Setters) -> Result<(), Twice> {
    if BOUND.is_completed() {
        return Err(Twice);
    }
    let _ = BOUND.call_once(|| setters);
    Ok(())
}

/// The registered binding, or [`ABSENT`] before one is.
pub fn bound() -> &'static Setters {
    BOUND.get().copied().unwrap_or(&ABSENT)
}
