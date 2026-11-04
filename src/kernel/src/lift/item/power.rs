use crate::arch;
use crate::console::println;
use crate::hooks::Full;
use crate::hooks::Hooks;
use crate::panic::catalog;
use crate::panic::fatal;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::BootView;
use ferrix_bootinfo::option_in;
use ferrix_bootinfo::pstore::Stop;
use ferrix_linux_abi::errno::Errno;
use ferrix_sync::Once;
#[derive(Debug)]
pub struct Flush {
    pub mount: &'static str,
    pub commit: fn() -> Result<(), Errno>,
}
static FLUSHES: crate::sync::SpinLock<alloc::vec::Vec<&'static Flush>> =
    crate::sync::SpinLock::new(alloc::vec::Vec::new());
pub fn register_flush(flush: &'static Flush) -> Result<(), Full> {
    FLUSHES.lock().push(flush);
    Ok(())
}
pub type BootMode = fn(&str) -> Result<&'static str, &'static str>;
pub fn register_boot_mode(set: BootMode) {}
pub fn request_boot_mode(word: &str) -> Result<&'static str, &'static str> {
    Err("no boot mode under personality=user")
}
pub fn sync_disks() {
    let flushes = FLUSHES.lock().clone();
    for flush in flushes {
        if (flush.commit)().is_err() {
            crate::println!(
                "  power    {} could not be committed before the power-off",
                flush.mount
            );
        }
    }
}
