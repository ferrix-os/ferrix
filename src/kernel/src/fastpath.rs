//! Whether `channel_write_read` takes the fast path: the build flag
//! `ipc_fastpath` (`docs/BUILD-FLAGS.md`, BF6; `docs/OPAQUE-KERNEL.md` §9.7,
//! part 6).
//!
//! Selected when the kernel is built and never at boot: `ferrix_fast_ipc`, the
//! flag on x86-64, is the one predicate over the fast path's code, so that a
//! build without it holds none. **Off in the certified configuration** the Safety
//! Manual names (the customer's call of 2026-10-02), on in `fast` and `desktop`.
//! `cargo xtask test-ipc-equiv` builds both from one commit and compares the two
//! paths' transcripts.

use crate::console::println;

/// Whether this image has the fast path.
pub const BUILT: bool = cfg!(ferrix_fast_ipc);

/// A symbol the fast path's image holds and no other, whatever is inlined, for
/// the check that an `off` image has none of it (ledger 1001 (3)).
#[cfg(ferrix_fast_ipc)]
#[used]
#[unsafe(no_mangle)]
static FERRIX_FAST_IPC_BUILT: u8 = 1;

/// Stage 9's line: which path `channel_write_read` takes.
pub fn report() {
    if BUILT {
        println!(
            "  fastpath ipc fast path for channel_write_read: on (built with ipc_fastpath=on); \
             taken from x86-64's SYSCALL entry"
        );
    } else if cfg!(ferrix_ipc_fastpath = "on") {
        println!(
            "  fastpath ipc fast path for channel_write_read: on (built with ipc_fastpath=on); \
             x86-64 only, so every call here takes the general path"
        );
    } else {
        println!(
            "  fastpath ipc fast path for channel_write_read: off (built with ipc_fastpath=off)"
        );
    }
}

/// What the fast path did on this boot: its trips, its parks and each test's
/// declines (`sched::direct`'s counts), for the figure a benchmark prints to
/// say which path it measured. Printed as the shell exits; no program reads
/// them.
pub(crate) fn report_counts() {
    // An image without the fast path holds no count to read, and says so as one.
    #[cfg(not(ferrix_fast_ipc))]
    println!("  fastpath counts: trips=0");
    #[cfg(ferrix_fast_ipc)]
    {
        let [
            trips,
            parks,
            t2,
            t3,
            t4,
            halves,
            t6,
            t7,
            t8,
            t9,
            t10,
            queue,
            t11,
            t12,
            t13,
            fold,
            separate,
            unplanned,
        ] = crate::sched::direct::counts();
        println!(
            "  fastpath counts: trips={trips} parks={parks} declined T2={t2} T3={t3} T4={t4} \
         halves={halves} T6={t6} T7={t7} T8={t8} T9={t9} T10={t10} queue={queue} T11={t11} \
         T12={t12} T13={t13} jobs folded={fold} apart={separate} unplanned={unplanned}"
        );
    }
    // The selector skips are the switch's, so the direct switch takes them
    // too: DS or ES, FS, and GS, each counted apart.
    if let Some([skipped, fs, gs]) = crate::arch::selector_skips_total() {
        println!("  fastpath switches that left DS or ES unloaded, 0 over 0: {skipped}");
        println!("  fastpath switches that left FS unloaded, 0 over 0: {fs}; GS: {gs}");
    }
}
