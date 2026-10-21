//! What differs by processor: the one cache operation the driver needs.

/// `clflush` the cache line at `address`, fenced on both sides: i915's
/// `invalidate_csb_entries` and `drm_clflush_virt_range` for the status
/// page the GPU writes and this driver reads. On the N150 box (2026-10-21,
/// Endfield on anv under `ferrix.intelgpu.uapitrace`) a request whose batch
/// had finished stayed unseen for seconds while the box was idle, and was
/// seen within milliseconds of the console's drawing (which evicts lines):
/// the processor kept reading a stale status page from its own cache. i915
/// flushes the CSB entries it consumed after each pass; this flushes every
/// status-page dword before it is read and after it is written, which
/// covers the CSB write pointer and each context's seqno too.
#[inline]
pub(crate) fn flush_line(address: usize) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `address` is inside a mapping this process holds; `clflush`
    // and `mfence` only order and evict, unprivileged (CPUID.01H:EDX.CLFSH,
    // every x86-64 part).
    unsafe {
        core::arch::x86_64::_mm_mfence();
        core::arch::x86_64::_mm_clflush(address as *const u8);
        core::arch::x86_64::_mm_mfence();
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = address;
}
