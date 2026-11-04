#[cfg(not(target_arch = "x86_64"))]
use core::sync::atomic::Ordering;
use core::sync::atomic::compiler_fence;
#[cfg(not(target_arch = "x86_64"))]
use ferrix_sync::IrqControl;
#[cfg(target_arch = "x86_64")]
/// Nothing is per processor here: the count is linuxd's own.
///
/// # Safety
///
/// None beyond the kernel's signature, which callers keep.
pub unsafe fn this_cpu_add(offset: usize, delta: u64) -> u64 {
    delta
}
#[cfg(not(target_arch = "x86_64"))]
/// Nothing is per processor here: the count is linuxd's own.
///
/// # Safety
///
/// None beyond the kernel's signature, which callers keep.
pub unsafe fn this_cpu_add(offset: usize, delta: u64) -> u64 {
    delta
}
