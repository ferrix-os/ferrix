use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_sched::MAX_CPUS;
use ferrix_sync::IrqControl;
use ferrix_sync::IrqSpinLock;
pub fn dispatch(irq: u32) {}
pub const COUNTED: usize = 128;
pub fn delivered_on(cpu: usize, irq: u32) -> u32 {
    0
}
pub fn delivered() -> u64 {
    0
}
pub fn unclaimed() -> u64 {
    0
}

pub fn in_interrupt() -> bool {
    false
}
pub fn serviced<R>(service: impl FnOnce() -> R) -> R {
    service()
}
pub fn unmarked() -> u64 {
    0
}

pub type Handler = fn(u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqError;
pub fn register(irq: u32, handler: Handler) -> Result<(), IrqError> {
    Err(IrqError)
}
