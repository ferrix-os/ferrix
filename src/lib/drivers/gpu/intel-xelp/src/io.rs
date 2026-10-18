//! What the driver process supplies to the logic in this crate: access to
//! the device's register window, and time. i915 reaches both through Linux
//! (`readl`/`writel` on `uncore->regs`, `udelay`, `wait_for_atomic`); here
//! they are two traits, so the logic runs against a model in a host test.

/// The device's register window, BAR0's lower half (GTTMMADR).
///
/// Offsets are bytes from the start of BAR0, as i915's `_MMIO()` offsets are.
/// Every access is 32 bits; i915 does not write 64-bit registers in one
/// access either (`intel_uncore.h`, before `intel_uncore_read64`).
pub trait Mmio {
    /// `readl(regs + offset)`.
    fn read32(&mut self, offset: u32) -> u32;
    /// `writel(value, regs + offset)`.
    fn write32(&mut self, offset: u32, value: u32);
}

/// A monotonic clock in microseconds, and a busy wait on it.
pub trait Clock {
    /// Microseconds since some fixed point; never goes back.
    fn now_us(&mut self) -> u64;
    /// Wait at least `us` microseconds (`udelay`).
    fn delay_us(&mut self, us: u32);
}

/// Wait until `(read(offset) & mask) == value`, or `timeout_us` passes:
/// i915's `wait_for_atomic` and `__intel_wait_for_register_fw` in one,
/// polling every microsecond. Returns the last value read, `Ok` when it
/// matched.
pub fn wait_for_register<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    offset: u32,
    mask: u32,
    value: u32,
    timeout_us: u64,
) -> Result<u32, u32> {
    let start = clock.now_us();
    loop {
        let read = mmio.read32(offset);
        if read & mask == value {
            return Ok(read);
        }
        if clock.now_us().saturating_sub(start) >= timeout_us {
            // One read after the deadline, as wait_for() does, so a value
            // that arrived while the clock was read is not missed.
            let last = mmio.read32(offset);
            return if last & mask == value {
                Ok(last)
            } else {
                Err(last)
            };
        }
        clock.delay_us(1);
    }
}
