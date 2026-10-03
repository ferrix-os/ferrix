//! Whether this boot runs the stages' self-checks, or only brings up what
//! they check.
//!
//! Every stage's exit criterion is a self-check in `kmain`, and by default
//! every one of them runs on every boot: that is what `cargo xtask test-boot`
//! and every other row reads, and a check that only ran when somebody asked
//! for it would be a check nobody ran. They are also most of the boot. On an
//! STM32MP157D-DK1 the kernel took 6.5 s from its banner to the marker on
//! 2026-09-24, and all but about a second of that was checks: stage 5's
//! thousand threads 0.9 s, stage 7's programs 1.5 s, stage 8's calls 1.1 s,
//! stage 9's device handles 0.4 s. A desktop somebody switches on
//! waits for all of it, to learn what the last boot test already said.
//!
//! So `ferrix.checks=skip` on the command line keeps every step that brings
//! something up -- the allocators, interrupts, the processors, the scheduler,
//! the root from the initramfs, the device nodes, `devmgr`, the disks, the net
//! core, the reclaim of boot memory -- and leaves out the steps that only
//! prove it works. Stage 1's hand-off check still runs, because it is
//! microseconds and everything after it stands on what it verifies; so do the
//! few checks that sit inside a bring-up and cost nothing (the console's
//! input ring, the random generator's).
//!
//! A boot that skipped them must never pass for one that ran them. It says so
//! as soon as the option is read, and it ends with `FERRIX-BOOT-UNCHECKED`
//! instead of `FERRIX-BOOT-OK`: a different word rather than the same marker
//! with a note after it, because every reader of the marker -- `test-boot`,
//! `watch-serial`, the rows that find init's output after it -- matches the
//! word, and a note is exactly the part such a reader does not look at. The
//! option is read from the loader's command line first and from the device
//! tree's `/chosen/bootargs` after, as `ferrix.onexit` is; `ferrix.checks=run`
//! asks for the default explicitly, which is how a card's `CMDLINE.TXT` turns
//! the checks back on over an image that carries `skip` among its defaults.

use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::{BootView, option_in};

use crate::console::println;

/// The command-line option, and its two values.
const OPTION: &str = "ferrix.checks";
const SKIP: &str = "skip";
const RUN: &str = "run";

/// Whether the checks are skipped. Only [`init`] writes it, once, before the
/// first check that could be.
static SKIPPED: AtomicBool = AtomicBool::new(false);

/// Read `ferrix.checks`, and say what it asked for.
///
/// Any value but the two is reported and ignored: running every check is the
/// safe default, and a misspelt option should cost a slow boot rather than
/// an unchecked one.
pub(crate) fn init(view: &BootView<'_>) {
    let value = view.option(OPTION).or_else(|| {
        crate::discovery::fdt::open(view)
            .ok()
            .and_then(|tree| option_in(tree.bootargs()?, OPTION))
    });
    match value {
        None => {}
        Some(SKIP) => {
            SKIPPED.store(true, Ordering::Relaxed);
            println!(
                "  checks   {OPTION}={SKIP}: stages 2 to 12 are brought up and not checked; \
                 this boot is not a boot test"
            );
        }
        Some(RUN) => println!("  checks   {OPTION}={RUN}: every self-check runs"),
        Some(other) => {
            println!("  checks   {OPTION}={other} is not understood; every self-check runs");
        }
    }
    // The way out's check that no poster forgot its bit runs with the rest
    // (`sched::work::audit`); the core keeps its own copy of the answer.
    crate::sched::work::set_auditing(run());
    // The boot's other configuration item read here, at the same point and
    // the same way: whether `channel_write_read` may take the fast path.
    // Stage 9 says which; a boot that skips stage 9's checks says it now.
    crate::fastpath::init(view);
    if !run() {
        crate::fastpath::report();
    }
}

/// Whether the self-checks run on this boot: true unless [`init`] read
/// `ferrix.checks=skip`.
pub(crate) fn run() -> bool {
    !SKIPPED.load(Ordering::Relaxed)
}
