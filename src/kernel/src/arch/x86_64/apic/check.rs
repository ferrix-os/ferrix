//! Check R8 of `docs/NVIDIA.md` §12.3 (condition G6): every processor looks
//! at its own local APIC's mode before it uses the MMIO window, and one left
//! in x2APIC mode is switched to xAPIC and comes up.
//!
//! No firmware QEMU boots leaves a processor in x2APIC mode, so the check
//! puts the first application processor to arrive there itself, where the
//! processor offers x2APIC (CPUID leaf 1, `ECX` bit 21), before that
//! processor's own look: it must report "switched to xAPIC" and come up.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{APIC_BASE_X2APIC, IA32_APIC_BASE};

/// Whether an application processor has been put in x2APIC mode already.
static TAKEN: AtomicBool = AtomicBool::new(false);
/// Processors the check put in x2APIC mode: 0 or 1.
static PUT_IN_X2APIC: AtomicU64 = AtomicU64::new(0);

/// On the first application processor to call it, unless the boot was told
/// to skip its checks or the processor offers no x2APIC: turn x2APIC mode
/// on, as firmware might have left it, before `leave_x2apic` looks.
pub(crate) fn put_first_in_x2apic() {
    if !crate::boot_checks::run() || core::arch::x86_64::__cpuid(1).ecx & 1 << 21 == 0 {
        return;
    }
    if TAKEN.swap(true, Ordering::AcqRel) {
        return;
    }
    // SAFETY: (SYSREG) `IA32_APIC_BASE` exists on every processor with a
    // local APIC, which every x86-64 processor has.
    let base = unsafe { super::super::cpu::read_msr(IA32_APIC_BASE) };
    // SAFETY: (SYSREG) xAPIC to x2APIC, a transition the SDM allows, on
    // this processor's own register, which CPUID says it supports; nothing
    // on this processor has used its local APIC yet, and `leave_x2apic`
    // takes it back before anything does.
    unsafe { super::super::cpu::write_msr(IA32_APIC_BASE, base | APIC_BASE_X2APIC) };
    let _ = PUT_IN_X2APIC.fetch_add(1, Ordering::Relaxed);
}

/// Stage 4, once `processors` processors are up: every one looked at its
/// mode, and exactly the ones the check and firmware left in x2APIC mode
/// were switched. The line to print, or `None` when the checks did not run.
///
/// # Errors
///
/// A processor that did not look, or a switch the check did not see.
///
/// Verifies: `L.x86_64.131`
pub(crate) fn require_every_processor(
    processors: u64,
) -> Result<Option<alloc::string::String>, &'static str> {
    if !crate::boot_checks::run() {
        return Ok(None);
    }
    let (checked, switched) = super::modes_checked();
    let put = PUT_IN_X2APIC.load(Ordering::Relaxed);
    if checked != processors {
        return Err("a processor used its local APIC without looking at its mode first");
    }
    if switched < put {
        return Err("a processor left in x2APIC mode was not switched to xAPIC");
    }
    Ok(Some(alloc::format!(
        "{checked} processors looked at their local APIC's mode before using its window; {put} \
         put in x2APIC mode by the check and switched to xAPIC; firmware left x2APIC on, on {} \
         processors; switched to xAPIC",
        switched - put
    )))
}
