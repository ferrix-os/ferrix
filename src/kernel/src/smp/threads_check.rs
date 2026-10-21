//! Stage 4's look at how many threads of a core run work on the Arm
//! architectures (`docs/BUILD-FLAGS.md` §3.1 DF4 (b) and (c)).

use core::sync::atomic::{AtomicUsize, Ordering};

use super::threads::{Found, found};
use crate::println;

/// Online processors whose `MPIDR.MT` reads 1.
static MT_SET: AtomicUsize = AtomicUsize::new(0);
/// Online processors that looked.
static LOOKED: AtomicUsize = AtomicUsize::new(0);

/// `MPIDR.MT`, bit 24, on the Arm architectures.
const MPIDR_MT: u64 = 1 << 24;

/// On each online processor: read its own `MPIDR.MT`.
fn look_at_mt(_me: &'static super::PerCpu) {
    if crate::arch::mpidr() & MPIDR_MT != 0 {
        let _ = MT_SET.fetch_add(1, Ordering::Relaxed);
    }
    let _ = LOOKED.fetch_add(1, Ordering::Relaxed);
}

/// Under `smt=off`, no core lists two threads among the processors firmware
/// enables (every one of which this kernel starts), as the PPTT or the
/// device tree's `cpu-map` says; with neither table, only where every
/// online processor reads `MPIDR.MT` clear, which says its affinity level 0
/// is a core (an `MT` of 1 is never read as a claim: the A78AE sets it with
/// one thread a core). Under `stibp` the line says what the table says.
///
/// # Errors
///
/// Under `smt=off`: a core listing two threads, a malformed table, or no
/// table and an online processor reading `MPIDR.MT` set.
///
/// Verifies: `L.aarch64.65`, `L.armv7a.27`
pub(crate) fn check_arm_smt() -> Result<(), &'static str> {
    MT_SET.store(0, Ordering::Relaxed);
    LOOKED.store(0, Ordering::Relaxed);
    super::run_everywhere(look_at_mt)?;
    if LOOKED.load(Ordering::Relaxed) != super::count() {
        return Err("smt: a processor did not read its own MPIDR");
    }
    let mt = MT_SET.load(Ordering::Relaxed);
    let off = cfg!(ferrix_smt = "off");
    match found() {
        Found::Map {
            table,
            cores,
            most_a_core,
        } => {
            if off && most_a_core > 1 {
                return Err(
                    "smt: a core lists two threads of processors that run work under smt=off",
                );
            }
            println!(
                "  smt      {}: {table} lists {cores} cores, {} thread{} a core at most",
                if off { "off" } else { "stibp" },
                most_a_core.max(1),
                if most_a_core > 1 { "s" } else { "" }
            );
        }
        Found::NoTable => {
            if off && mt != 0 {
                return Err(
                    "smt: no topology table, and a processor's MPIDR.MT is set, so one thread a \
                     core cannot be shown under smt=off",
                );
            }
            println!(
                "  smt      {}: no topology table; MPIDR.MT clear on {} of {} processors",
                if off { "off" } else { "stibp" },
                super::count() - mt,
                super::count()
            );
        }
        Found::Malformed(why) => {
            if off {
                return Err(why);
            }
            println!("  smt      stibp: the topology table is malformed: {why}");
        }
    }
    Ok(())
}
