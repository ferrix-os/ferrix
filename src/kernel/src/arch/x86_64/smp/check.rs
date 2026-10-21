//! Stage 4's look at how many hardware threads of each core run work
//! (`docs/BUILD-FLAGS.md` §3.1 DF4 (c), `L.x86_64.185`).

use core::sync::atomic::{AtomicUsize, Ordering};

use super::topology;
use crate::println;

/// Online processors whose own topology leaf names another identifier than
/// the one firmware listed them by.
static ID_MISMATCH: AtomicUsize = AtomicUsize::new(0);
/// Online processors that looked.
static LOOKED: AtomicUsize = AtomicUsize::new(0);

/// On each online processor: compare the identifier its own leaf gives with
/// its record's (P3).
fn look_at_own_id(me: &'static crate::smp::PerCpu) {
    if topology::own_id() != me.hardware_id {
        let _ = ID_MISMATCH.fetch_add(1, Ordering::Relaxed);
    }
    let _ = LOOKED.fetch_add(1, Ordering::Relaxed);
}

/// Under `smt=off`, no two online processors share a core, and every other
/// thread of a core is parked, has reported in, and parked with `CR4.MCE`
/// set. Under `stibp`, the line says how many threads a core run work.
///
/// Each online processor reads its own identifier from its own topology
/// leaf, and it must be the one firmware listed it by, so the core numbers
/// compared are the hardware's, not the table's.
///
/// # Errors
///
/// A processor whose leaf names another identifier, two online processors
/// of one core under `smt=off`, or a parked thread that did not report in or
/// reads `CR4.MCE` clear.
///
/// Verifies: `L.x86_64.185`
pub(crate) fn check_smt() -> Result<(), &'static str> {
    let found = crate::smp::topology().ok_or("the processors have not been discovered")?;
    ID_MISMATCH.store(0, Ordering::Relaxed);
    LOOKED.store(0, Ordering::Relaxed);
    crate::smp::run_everywhere(look_at_own_id)?;
    let processors = found.count();
    if LOOKED.load(Ordering::Relaxed) != processors {
        return Err("smt: a processor did not read its own topology leaf");
    }
    if ID_MISMATCH.load(Ordering::Relaxed) != 0 {
        return Err("smt: a processor's own topology leaf names another APIC ID than firmware's");
    }
    let shift = topology::shift();
    let cpus = found.cpus();
    let shared = cpus.iter().enumerate().any(|(index, one)| {
        cpus.iter().skip(index + 1).any(|other| {
            topology::core_of(one.hardware_id, shift) == topology::core_of(other.hardware_id, shift)
        })
    });
    let parked = found.parked();
    if cfg!(ferrix_smt = "off") {
        if shared {
            return Err("smt: two online processors share a core under smt=off");
        }
        if parked
            .iter()
            .any(|thread| thread.parked.load(Ordering::Acquire) == 0)
        {
            return Err("smt: a parked hardware thread never reported in");
        }
        if parked
            .iter()
            .any(|thread| thread.parked.load(Ordering::Acquire) & super::cpu::CR4_MCE == 0)
        {
            return Err(
                "smt: a parked hardware thread reads CR4.MCE clear: a machine check there would \
                 shut the machine down",
            );
        }
        if parked.is_empty() {
            println!("  smt      off: {processors} cores, firmware runs one thread per core");
        } else {
            println!(
                "  smt      off: {processors} cores, {} siblings parked by the kernel (CR4.MCE set)",
                parked.len()
            );
        }
    } else {
        let threads = 1u64 << shift;
        println!(
            "  smt      stibp: {processors} processors run work, {threads} threads a core at most{}",
            if shared { ", siblings among them" } else { "" }
        );
    }
    Ok(())
}
