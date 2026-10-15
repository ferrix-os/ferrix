//! What the machine does once boot has nothing left to run.
//!
//! By default it powers off: a QEMU run then ends, and `cargo xtask test-boot`
//! and `test-shell` read that as the end of the log. A board is different. A
//! powered-off STM32MP157-DK has to be unplugged and plugged in again before it
//! does anything, and the reset button does not bring it back, so every run on
//! it used to cost a hand at the board. `ferrix.onexit=reset` on the command
//! line asks for a reset instead, which lands back at the firmware's prompt,
//! ready for the next image.
//!
//! `ferrix.onexit=panic` asks for what Linux does when init exits: a panic,
//! here the one the catalog calls `FX-1501`. The disks are committed first,
//! which Linux does not do, because nothing is gained by losing them
//! (`docs/INIT.md` §8.3).
//!
//! The option is read once, early, rather than when boot ends: by then the
//! device tree may be unreachable for reasons that have nothing to do with it,
//! and a typo is better reported at the start of a log than discovered at the
//! end of one.
//!
//! # What power reaches without naming
//!
//! Two things above the certified item have to be told the machine is
//! stopping, and power names neither. A filesystem registers a [`Flush`],
//! and every registered one is committed before the machine stops, in the
//! order they were registered. A board whose firmware keeps a boot mode
//! registers a [`BootMode`], which `reboot(2)`'s RESTART2 word goes to.
//! Both are registered at bring-up, from `main.rs`, and the boot checks that
//! the flushes were before `init` can start a program that writes.

use core::sync::atomic::{AtomicU8, Ordering};

use ferrix_bootinfo::{BootView, option_in};
use ferrix_linux_abi::errno::Errno;
use ferrix_sync::Once;

use crate::arch;
use crate::console::println;
use crate::hooks::{Full, Hooks};
use crate::panic::{catalog, fatal};

/// A filesystem's writes not yet on its disk, committed before the machine
/// stops.
#[derive(Debug)]
pub struct Flush {
    /// Where it is mounted, as the log names it: `/`, `/data`, `/home`.
    pub mount: &'static str,
    /// Commit it. Nothing mounted there is nothing to commit, and `Ok`.
    pub commit: fn() -> Result<(), Errno>,
}

/// Every registered [`Flush`]. Four is one more than the kernel registers.
static FLUSHES: Hooks<Flush, 4> = Hooks::new();

/// Commit `flush` before the machine stops, after every flush registered
/// before it.
///
/// # Errors
///
/// [`Full`] when the list is.
pub fn register_flush(flush: &'static Flush) -> Result<(), Full> {
    FLUSHES.register(flush)
}

/// How many flushes are registered: the boot's check that a filesystem will
/// be committed before the machine stops.
pub fn flushes() -> usize {
    FLUSHES.len()
}

/// Ask the firmware to come back up as a `reboot(2)` word says, at the next
/// reset: what it will do, or why nothing changes.
pub(crate) type BootMode = fn(&str) -> Result<&'static str, &'static str>;

/// The board's [`BootMode`], if its support registered one.
static BOOT_MODE: Once<BootMode> = Once::new();

/// Send `reboot(2)`'s words to `set`. The first registration stands.
pub fn register_boot_mode(set: BootMode) {
    let _ = BOOT_MODE.call_once(|| set);
}

/// Whether a board registered a [`BootMode`].
pub fn has_boot_mode() -> bool {
    BOOT_MODE.get().is_some()
}

/// Pass `word` to the registered [`BootMode`].
///
/// # Errors
///
/// Why the next reset will be as it would have been without the word: a
/// machine with no board support that keeps a boot mode, or whatever the
/// board's own says.
pub fn request_boot_mode(word: &str) -> Result<&'static str, &'static str> {
    let set = BOOT_MODE.get().ok_or("this machine keeps no boot mode")?;
    set(word)
}

/// The command-line option, and the values of it that are honoured.
const OPTION: &str = "ferrix.onexit";
const RESET: &str = "reset";
const PANIC: &str = "panic";

/// What boot ends in: one of the three below.
static ON_EXIT: AtomicU8 = AtomicU8::new(POWER_OFF);
const POWER_OFF: u8 = 0;
const RESET_ON_EXIT: u8 = 1;
const PANIC_ON_EXIT: u8 = 2;

/// Read `ferrix.onexit` from the loader's command line or, on a machine
/// described by a device tree, from `/chosen/bootargs`, where U-Boot puts
/// `bootargs`.
///
/// Any other value is reported and ignored: powering off is the safe
/// default, and a misspelt option should not turn into something nobody asked
/// for.
pub fn init(view: &BootView<'_>) {
    let tree = crate::discovery::fdt::open(view).ok();
    if let Some(tree) = &tree {
        arch::init_watchdogs(tree);
    }
    let value = view.option(OPTION).or_else(|| {
        tree.as_ref()
            .and_then(|tree| option_in(tree.bootargs()?, OPTION))
    });
    match value {
        None => {}
        Some(RESET) => {
            ON_EXIT.store(RESET_ON_EXIT, Ordering::Relaxed);
            println!("  power    {OPTION}={RESET}: the machine resets when boot ends");
        }
        Some(PANIC) => {
            ON_EXIT.store(PANIC_ON_EXIT, Ordering::Relaxed);
            println!("  power    {OPTION}={PANIC}: the kernel panics when init exits");
        }
        Some(other) => {
            println!(
                "  power    {OPTION}={other} is not understood; the machine powers off when boot ends"
            );
        }
    }
}

/// Commit every registered [`Flush`]: on this kernel `/`, `/data` and
/// `/home`, the last half-minute of each, which the root's committer has not
/// reached yet.
///
/// Before the machine stops, whoever stops it: [`finish`] when init has
/// exited, and `reboot(2)` when a program asks, so that a program calling it
/// without a `sync` of its own loses no transaction either. A failure is said
/// and does not stop the machine stopping.
pub fn sync_disks() {
    for flush in FLUSHES.iter() {
        if (flush.commit)().is_err() {
            println!(
                "  power    {} could not be committed before the power-off",
                flush.mount
            );
        }
    }
}

/// End boot the way [`init`] was told to.
pub fn finish() -> ! {
    sync_disks();
    let action = match ON_EXIT.load(Ordering::Relaxed) {
        RESET_ON_EXIT => ferrix_audit::power::RESTART,
        PANIC_ON_EXIT => ferrix_audit::power::PANIC,
        _ => ferrix_audit::power::OFF,
    };
    crate::audit::power(crate::audit::Subject::KERNEL, action);
    match ON_EXIT.load(Ordering::Relaxed) {
        RESET_ON_EXIT => {
            println!("  power    resetting, as {OPTION}={RESET} asks");
            // What the next boot's loader reports, out of caches a reset may
            // discard (`console::pstore`).
            let _ = crate::console::pstore::write_back();
            arch::reset()
        }
        PANIC_ON_EXIT => fatal!(
            catalog::INIT_EXITED,
            "init exited, and {OPTION}={PANIC} asks for a panic"
        ),
        _ => arch::shutdown(),
    }
}
