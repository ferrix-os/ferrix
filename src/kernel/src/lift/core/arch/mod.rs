#[cfg(target_arch = "aarch64")]
pub mod aarch64;
pub mod percpu;
pub mod speculation;
#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
pub use percpu::this_cpu_add;
pub use speculation::HARDENED;
pub use speculation::switch_barriers;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;
/// Which `struct stat` the stat calls fill in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatLayout {
    Legacy,
    Generic,
    Stat64,
}
pub const CAN_MOVE_INTERRUPTS: bool = cfg!(target_arch = "x86_64");
#[cfg(not(target_arch = "x86_64"))]
pub const fn thread_core(_hardware_id: u64) -> Option<(u64, u32)> {
    None
}
#[cfg(not(target_arch = "x86_64"))]
pub const fn cpuid(_leaf: u32, _subleaf: u32) -> Option<[u32; 4]> {
    None
}
#[cfg(not(target_arch = "x86_64"))]
pub const fn user_extended_state() -> u64 {
    0
}
pub const fn midr_of(_logical: usize) -> Option<u32> {
    None
}
pub const fn sigpage_code() -> Option<&'static [u8]> {
    None
}
pub const fn user_combining() -> bool {
    false
}
/// `index` if it is below `end`, with no speculation past the test (defence in depth
/// only: linuxd is outside the TSF).
pub fn nospec_index(index: usize, end: usize) -> Option<usize> {
    (index < end).then_some(index)
}
