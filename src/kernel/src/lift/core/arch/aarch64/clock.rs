//! The counter: CNTVCT_EL0, which a program reads as the kernel does.
pub fn counter_hz() -> u64 {
    super::counter_hz()
}
