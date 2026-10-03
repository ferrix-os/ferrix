//! Whether `channel_write_read` may take the fast path: `ferrix.fastpath`
//! (`docs/OPAQUE-KERNEL.md` §9.7, part 6).
//!
//! A configuration item, read once at boot and never changed after: the
//! fast path of §9.7 tests it first (T1), and the gate boots x86-64 once with
//! it off and once with it on to compare the two paths' transcripts
//! (`cargo xtask test-ipc-equiv`). **Off unless asked for**: the certified
//! configuration the Safety Manual names runs it off for the first certified
//! release (the customer's call of 2026-10-02), and development builds and
//! the perf rows turn it on with `ferrix.fastpath=on`.
//!
//! No fast path exists yet. Until it does, `on` is recorded and printed and
//! changes nothing: every call takes the general path either way, which the
//! stage-9 line says.
//!
//! Read as `ferrix.checks` is (`checks.rs`): from the loader's command line
//! first, then the device tree's `/chosen/bootargs`. A value other than the
//! two is reported and ignored, leaving it off.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use ferrix_bootinfo::{BootView, option_in};

use crate::console::println;

/// The command-line option, and its two values.
const OPTION: &str = "ferrix.fastpath";
const ON: &str = "on";
const OFF: &str = "off";

/// Whether the fast path is asked for. Only [`init`] writes it, once, before
/// the first program.
static ON_AT_BOOT: AtomicBool = AtomicBool::new(false);

/// What [`init`] read: [`READ_NONE`], [`READ_ON`], [`READ_OFF`] or
/// [`READ_OTHER`], for the stage-9 line.
static READ: AtomicU8 = AtomicU8::new(READ_NONE);
const READ_NONE: u8 = 0;
const READ_ON: u8 = 1;
const READ_OFF: u8 = 2;
const READ_OTHER: u8 = 3;

/// Read `ferrix.fastpath`, once, before the first program.
pub(crate) fn init(view: &BootView<'_>) {
    let value = view.option(OPTION).or_else(|| {
        crate::discovery::fdt::open(view)
            .ok()
            .and_then(|tree| option_in(tree.bootargs()?, OPTION))
    });
    let read = match value {
        None => READ_NONE,
        Some(ON) => READ_ON,
        Some(OFF) => READ_OFF,
        Some(_) => READ_OTHER,
    };
    ON_AT_BOOT.store(read == READ_ON, Ordering::Relaxed);
    READ.store(read, Ordering::Relaxed);
}

/// Whether the fast path is on for this boot: false unless [`init`] read
/// `ferrix.fastpath=on`.
pub(crate) fn on() -> bool {
    ON_AT_BOOT.load(Ordering::Relaxed)
}

/// Stage 9's line: which path `channel_write_read` takes, and how the option
/// was read.
pub(crate) fn report() {
    let state = if on() { ON } else { OFF };
    let how = match READ.load(Ordering::Relaxed) {
        READ_ON | READ_OFF => "as ferrix.fastpath asked",
        READ_OTHER => "ferrix.fastpath's value was not understood and is ignored",
        _ => "by default",
    };
    println!(
        "  fastpath ipc fast path for channel_write_read: {state} ({how}); no fast path is built \
         yet, so every call takes the general path"
    );
}
