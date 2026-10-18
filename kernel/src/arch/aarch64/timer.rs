//! The architected generic timer.
//!
//! Unlike x86-64 there is nothing to calibrate and nothing to map: the counter
//! is a system register, and `CNTFRQ_EL0` states its frequency. That is the
//! whole of the clock.
//!
//! # Why the virtual timer
//!
//! There are two the kernel could use. `CNTP` is the physical timer; `CNTV`
//! counts the same crystal offset by `CNTVOFF_EL2`, which is zero on a machine
//! with no hypervisor and not zero on one that has been migrated. A kernel at
//! EL1 is by definition below any hypervisor present, so the virtual timer is
//! the one that belongs to it — and on bare metal the two are identical, so
//! nothing is given up by choosing it.

use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_acpi::Acpi;

use super::cpu;
use crate::acpi::DirectMap;

/// `CNTV_CTL_EL0`: the timer is enabled.
const CTL_ENABLE: u64 = 1 << 0;

/// The virtual timer's private peripheral interrupt on every machine that
/// follows the ARM base system architecture, and the fallback when firmware's
/// tables do not say.
const DEFAULT_VIRTUAL_PPI: u32 = 27;

/// The lowest and highest private peripheral interrupt. A timer described as
/// anything outside this range has been described wrongly.
const PPI_RANGE: core::ops::Range<u32> = 16..32;

/// Which interrupt this machine's virtual timer signals on.
static IRQ: AtomicU32 = AtomicU32::new(DEFAULT_VIRTUAL_PPI);

/// Learn where the timer's interrupt arrives, and check the counter runs.
///
/// # Errors
///
/// If `CNTFRQ_EL0` reads zero, which means firmware never programmed it. Every
/// duration the kernel computes would divide by it.
pub(crate) fn init(acpi: &Acpi<'_, DirectMap>) -> Result<(), &'static str> {
    let described = acpi
        .gtdt()
        .ok()
        .map(|gtdt| gtdt.virtual_el1_timer().gsiv)
        .filter(|gsiv| PPI_RANGE.contains(gsiv));
    IRQ.store(described.unwrap_or(DEFAULT_VIRTUAL_PPI), Ordering::Relaxed);

    if cpu::read_cntfrq() == 0 {
        return Err("firmware left CNTFRQ_EL0 at zero, so the counter has no frequency");
    }
    disarm();
    Ok(())
}

/// The interrupt the timer signals on.
pub(crate) fn irq() -> u32 {
    IRQ.load(Ordering::Relaxed)
}

/// The counter's current value.
pub(crate) fn counter_now() -> u64 {
    cpu::read_cntvct()
}

/// How fast it counts.
pub(crate) fn counter_hz() -> u64 {
    cpu::read_cntfrq()
}

/// Fire the timer interrupt once, `nanos` from now.
///
/// The generic timer compares against an absolute instant rather than counting
/// down, so this reads the counter and adds — which also means an interval
/// that has already elapsed by the time the register is written fires
/// immediately rather than being lost.
pub(crate) fn arm(nanos: u64) {
    let hz = cpu::read_cntfrq();
    if hz == 0 {
        return;
    }
    let ticks = u128::from(nanos) * u128::from(hz) / 1_000_000_000;
    let delta = u64::try_from(ticks.max(1)).unwrap_or(u64::MAX);

    cpu::write_cntv_cval(cpu::read_cntvct().wrapping_add(delta));
    cpu::write_cntv_ctl(CTL_ENABLE);
}

/// Stop the timer.
pub(crate) fn disarm() {
    cpu::write_cntv_ctl(0);
}
