use crate::arch;
use crate::irq;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
/// The kernel's clock, from the same counter and by the same sum where a ring-3
/// read can do it: what a deadline handed to the kernel is compared with.
pub fn now_nanos() -> u64 {
    ferrix_vdso::counter_nanos(arch::counter_now(), arch::counter_hz())
}
pub fn counter_hz() -> u64 {
    arch::counter_hz()
}

pub fn ticks() -> u64 {
    now_nanos() / 1_000_000
}
