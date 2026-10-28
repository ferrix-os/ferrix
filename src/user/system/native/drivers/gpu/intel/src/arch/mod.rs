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
    {
        // SAFETY: `mfence` only orders; SSE2 is in every x86-64 part.
        unsafe { core::arch::x86_64::_mm_mfence() };
        // SAFETY: `address` is inside a mapping this process holds;
        // `clflush` only evicts the line, unprivileged (CPUID.01H:EDX.CLFSH,
        // every x86-64 part).
        unsafe { core::arch::x86_64::_mm_clflush(address as *const u8) };
        // SAFETY: as the first.
        unsafe { core::arch::x86_64::_mm_mfence() };
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = address;
}

/// `clflush` every cache line of `[address, address + len)`, then one
/// fence: memory the processor wrote that a device reads past the caches
/// (`drm_clflush_virt_range`), the second scanout's surface. One fence for
/// the lot, as i915 fences once around the range.
pub(crate) fn flush_range(address: usize, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let start = address & !63;
        let end = address.saturating_add(len);
        let mut line = start;
        while line < end {
            // SAFETY: `line` is in a mapping this process holds (the caller's
            // range, rounded down to its cache line); `clflush` only evicts.
            unsafe { core::arch::x86_64::_mm_clflush(line as *const u8) };
            line += 64;
        }
        // SAFETY: `mfence` only orders; SSE2 is in every x86-64 part.
        unsafe { core::arch::x86_64::_mm_mfence() };
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (address, len);
}

/// Evict every cache line of `[address, address + len)` with no fence:
/// lines a device may have written past the caches, evicted so the next
/// read goes to memory. Several ranges, then one [`fence`]. `clflush`, not
/// `clflushopt`, whose evictions would overlap: Rust has no spelling of it
/// but assembly, which this tree keeps to what runs before the first
/// instruction (`docs/ASSEMBLY.md`).
pub(crate) fn evict_range(address: usize, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let end = address.saturating_add(len);
        let mut line = address & !63;
        while line < end {
            // SAFETY: `line` is in a mapping this process holds (the
            // caller's range, rounded down to its cache line); `clflush`
            // only evicts, and is allowed on a read-only mapping.
            unsafe { core::arch::x86_64::_mm_clflush(line as *const u8) };
            line += 64;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (address, len);
}

/// Order every `clflush` before it ahead of every load after it.
pub(crate) fn fence() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `mfence` only orders; SSE2 is in every x86-64 part.
    unsafe {
        core::arch::x86_64::_mm_mfence();
    }
}
