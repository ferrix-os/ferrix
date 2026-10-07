//! The architected generic timer, on ARMv7-A.
//!
//! The same counter and the same comparator as AArch64's, reached through
//! coprocessor 15 rather than system register names, and the virtual one for
//! the same reason: a kernel at PL1 is below any hypervisor present, and on a
//! machine with none the two timers count the same crystal. What differs is
//! where the interrupt number comes from — the device tree's timer node here,
//! the GTDT there.

use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_fdt::{Fdt, TimerInterrupt};

use super::cpu;

/// `CNTV_CTL`: the timer is enabled.
const CTL_ENABLE: u32 = 1 << 0;

/// The virtual timer's private peripheral interrupt on every machine that
/// follows the Arm base system architecture, and the fallback when the tree
/// does not say.
const DEFAULT_VIRTUAL_PPI: u32 = 27;

/// The private peripheral interrupts. A timer described outside this range
/// has been described wrongly.
const PPI_RANGE: core::ops::Range<u32> = 16..32;

/// Which interrupt this machine's virtual timer signals on.
static IRQ: AtomicU32 = AtomicU32::new(DEFAULT_VIRTUAL_PPI);

/// Learn where the timer's interrupt arrives, and check the counter runs.
///
/// # Errors
///
/// If `CNTFRQ` reads zero, which means firmware never programmed it. Every
/// duration the kernel computes would divide by it.
pub(crate) fn init(tree: &Fdt<'_>) -> Result<(), &'static str> {
    let described = tree
        .timer_interrupt(TimerInterrupt::Virtual)
        .filter(|id| PPI_RANGE.contains(id));
    IRQ.store(described.unwrap_or(DEFAULT_VIRTUAL_PPI), Ordering::Relaxed);

    if cpu::read_cntfrq() == 0 {
        return Err("firmware left CNTFRQ at zero, so the counter has no frequency");
    }
    // The boot processor's; each secondary sets its own as it starts.
    cpu::allow_user_counter();
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
    u64::from(cpu::read_cntfrq())
}

/// Fire the timer interrupt once, `nanos` from now.
///
/// An absolute comparator, as on AArch64, so an interval that has already
/// passed by the time the register is written fires at once rather than never.
pub(crate) fn arm(nanos: u64) {
    let hz = counter_hz();
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
