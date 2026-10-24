//! Stage 4's look at every processor's own control registers once all are
//! online: the x87 controls (`L.x86_64.178`) and the machine check
//! (`L.x86_64.175`, F-73). Each processor reads its own register, and every
//! bring-up was counted setting it.
//!
//! Moved here unchanged from the trap's and the paranoid entries' checks,
//! which run a Linux program and so are raised above the core
//! (`docs/KERNEL-CRATES.md`): these two run none, and `smp`'s stage 4 check,
//! a core check, calls them.

use core::sync::atomic::Ordering;

use super::super::cpu;
use crate::console::println;

/// Processors that looked at their own `CR0` for [`check_x87_controls`].
static X87_LOOKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Of those, the ones whose `CR0` does not hold the x87 controls.
static X87_WRONG: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// One processor's share: its own `CR0`, read on it.
fn look_at_x87_controls(_me: &'static crate::smp::PerCpu) {
    use core::sync::atomic::Ordering;

    if !cpu::x87_controls_held() {
        let _ = X87_WRONG.fetch_add(1, Ordering::Relaxed);
    }
    let _ = X87_LOOKED.fetch_add(1, Ordering::Relaxed);
}

/// Every processor reports a program's x87 error as `#MF`: after every
/// processor is online, each reads its own `CR0` with `NE` and `MP` set and
/// `EM` and `TS` clear, and every bring-up was counted setting them.
///
/// Stage 3's look at `CR0.NE` and its x87 program run on the boot processor
/// alone; this is the secondaries' share. The firmware's `CR0` says nothing
/// here: a secondary starts from the boot processor's with `NE` taken out,
/// so a bring-up that did not set its own reads clear.
///
/// # Errors
///
/// A processor that did not look, one whose `CR0` does not hold the
/// controls, or a bring-up count short of the processors.
///
/// Verifies: `L.x86_64.178`
pub(crate) fn check_x87_controls() -> Result<(), &'static str> {
    use core::sync::atomic::Ordering;

    X87_LOOKED.store(0, Ordering::Relaxed);
    X87_WRONG.store(0, Ordering::Relaxed);
    crate::smp::run_everywhere(look_at_x87_controls)?;
    let processors = crate::smp::count();
    if X87_LOOKED.load(Ordering::Relaxed) != processors {
        return Err("a processor did not report its CR0's x87 controls");
    }
    if X87_WRONG.load(Ordering::Relaxed) != 0 {
        return Err(
            "a processor's CR0 does not hold NE and MP set and EM and TS clear: an x87 error \
             there is not an exception",
        );
    }
    let taken = super::super::X87_CONTROLS_TAKEN.load(Ordering::Acquire);
    if taken != processors {
        return Err("a processor's bring-up did not set CR0's x87 controls");
    }
    println!(
        "  x87      CR0.NE and MP set, EM and TS clear at the bring-up of {taken} of \
         {processors} processors and read so on each in stage 4"
    );
    Ok(())
}

/// Processors that looked at their own `CR4.MCE` for
/// [`check_machine_check_enabled`].
static MCE_LOOKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Of those, the ones that offer the exception and read the bit clear.
static MCE_CLEAR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Of those, the ones whose `CPUID` offers no machine-check exception.
static MCE_ABSENT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// One processor's share: its own `CR4`, read on it.
fn look_at_machine_check(_me: &'static crate::smp::PerCpu) {
    if !cpu::has_machine_check() {
        let _ = MCE_ABSENT.fetch_add(1, Ordering::Relaxed);
    } else if !cpu::machine_check_enabled() {
        let _ = MCE_CLEAR.fetch_add(1, Ordering::Relaxed);
    }
    let _ = MCE_LOOKED.fetch_add(1, Ordering::Relaxed);
}

/// Every processor takes a machine check as the exception, not as a
/// shutdown (F-73): after every processor is online, each reads its own
/// `CR4` and finds `MCE` set, and every bring-up was counted setting it.
/// A processor whose `CPUID` offers no machine-check exception has no bit
/// to set, and fails the check too: the fail-stop on a hardware error
/// rests on the exception, so a machine or hypervisor that hides it is
/// refused rather than run without it.
///
/// The firmware's `CR4` says nothing here: a secondary starts from the boot
/// processor's with the bit taken out, so a bring-up that did not set its
/// own reads clear whatever firmware did.
///
/// # Errors
///
/// A processor that did not look, one that offers no machine-check
/// exception, one that reads the bit clear, or a bring-up count short of
/// the processors.
///
/// Verifies: `L.x86_64.175`
pub(crate) fn check_machine_check_enabled() -> Result<(), &'static str> {
    for count in [&MCE_LOOKED, &MCE_CLEAR, &MCE_ABSENT] {
        count.store(0, Ordering::Relaxed);
    }
    crate::smp::run_everywhere(look_at_machine_check)?;
    let processors = crate::smp::count();
    let looked = MCE_LOOKED.load(Ordering::Relaxed);
    if looked != processors {
        return Err("a processor did not report whether it takes a machine check");
    }
    if MCE_ABSENT.load(Ordering::Relaxed) != 0 {
        return Err(
            "a processor's CPUID offers no machine-check exception: a hardware error there would \
             not stop the machine",
        );
    }
    if MCE_CLEAR.load(Ordering::Relaxed) != 0 {
        return Err(
            "a processor reads CR4.MCE clear: a machine check there would shut it down unreported",
        );
    }
    let enabled = super::super::MACHINE_CHECK_ENABLED.load(Ordering::Acquire);
    if enabled != processors {
        return Err("a processor's bring-up did not set CR4.MCE");
    }
    println!(
        "  mce      CR4.MCE set at the bring-up of {enabled} of {processors} processors and read \
         set on each in stage 4; a machine check stops the machine as FX-9005"
    );
    Ok(())
}
