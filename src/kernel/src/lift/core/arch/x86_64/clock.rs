use crate::mmio::Mmio;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_acpi::Acpi;
use ferrix_acpi::GenericAddress;
use ferrix_acpi::hpet;
use ferrix_acpi::hpet::Capabilities;
pub fn counter_hz() -> u64 {
    1_000_000_000
}
/// Whether ring 3 reads the kernel's counter: the vDSO's question.
pub fn counter_is_tsc() -> bool {
    super::tsc_hz() != 0
}
