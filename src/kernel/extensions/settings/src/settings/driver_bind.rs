//! A write to a driver's `bind` or `unbind` under `/sys/bus/<bus>/drivers/`:
//! an administrator asking `devmgr` to bind or unbind a device by hand (from
//! `fs/sysfs.rs`'s `write`; `docs/KERNEL-CRATES.md` §6.2). `devmgr`'s own
//! matching of drivers to devices is not a setting and stays (ledger 3558
//! (8)).

use ferrix_kernel_load::discovery::devmgr::{self, Bus, Request};
use ferrix_kernel_load::fs::sysfs;
use ferrix_linux_abi::errno::Errno;
use ferrix_sysfs::name;

/// A write of `data` to `driver`'s `bind`, or with `unbind` its `unbind`,
/// on `bus`: a request to `devmgr`.
///
/// What Linux checks before it asks a driver is checked here: the name must
/// be a device on the driver's bus (`ENODEV`), and an unbind must be of this
/// driver's device (`ENODEV`). Everything else is `devmgr`'s to answer.
pub fn driver_bind(bus: Bus, driver: u16, unbind: bool, data: &[u8]) -> Result<usize, Errno> {
    // A row's kill of a driver, which is no Linux process here (ledger 6268, L9):
    // only in a row's image. By location; devmgr holds the handle.
    #[cfg(ferrix_row_driver_kill)]
    if unbind && let Some(named) = data.strip_prefix(b"kill:") {
        let named = name::written(named).ok_or(Errno::ENODEV)?;
        let device = sysfs::device_named(bus, named).ok_or(Errno::ENODEV)?;
        devmgr::request(Request::Unbind, device, devmgr::KILL_DRIVER)?;
        return Ok(data.len());
    }
    let named = name::written(data).ok_or(Errno::ENODEV)?;
    let device = sysfs::device_named(bus, named).ok_or(Errno::ENODEV)?;
    if unbind {
        if devmgr::driver_of(device).map(|(at, _)| at) != Some(driver) {
            return Err(Errno::ENODEV);
        }
        devmgr::request(Request::Unbind, device, driver)?;
    } else {
        devmgr::request(Request::Bind, device, driver)?;
    }
    Ok(data.len())
}
