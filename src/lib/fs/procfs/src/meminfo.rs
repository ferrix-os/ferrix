//! `/proc/meminfo`.
//!
//! ```text
//! MemTotal:       62103444 kB
//! VmallocTotal:   34359738367 kB
//! ```
//!
//! Linux's `show_val_kb` writes a label already padded to sixteen bytes, colon
//! included, and the value right-aligned in eight; a value wider than eight
//! pushes the unit along rather than being cut, which the second line shows.
//! `free` and a C library's `sysconf(_SC_AVPHYS_PAGES)` read these by label, so
//! a line that is absent is survivable and a line that lies is not — which is
//! why [`Meminfo`] has only the lines a kernel can fill truthfully, in the
//! order Linux prints them.

use alloc::vec::Vec;

use crate::text::{pad_to, put};

/// The width of the label field, colon included.
const LABEL: usize = 16;

/// What the kernel can say about memory, every field in kibibytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Meminfo {
    /// `MemTotal`: memory the kernel manages.
    pub total: u64,
    /// `MemFree`: of that, what is unused.
    pub free: u64,
    /// `MemAvailable`: what a new workload could have without swapping.
    pub available: u64,
    /// `Buffers`: block-device cache.
    pub buffers: u64,
    /// `Cached`: file page cache.
    pub cached: u64,
    /// `SwapCached`: swapped pages still in memory.
    pub swap_cached: u64,
    /// `SwapTotal`.
    pub swap_total: u64,
    /// `SwapFree`.
    pub swap_free: u64,
    /// `Dirty`: file pages written and not yet written back.
    pub dirty: u64,
    /// `Shmem`: the pages of memory filesystems, which `Cached` counts too.
    pub shmem: u64,
    /// `Slab`: the kernel's own small-object allocator.
    pub slab: u64,
}

/// Append the whole file.
pub fn render(out: &mut Vec<u8>, info: &Meminfo) {
    let lines = [
        ("MemTotal", info.total),
        ("MemFree", info.free),
        ("MemAvailable", info.available),
        ("Buffers", info.buffers),
        ("Cached", info.cached),
        ("SwapCached", info.swap_cached),
        ("SwapTotal", info.swap_total),
        ("SwapFree", info.swap_free),
        ("Dirty", info.dirty),
        ("Shmem", info.shmem),
        ("Slab", info.slab),
    ];
    for (label, kib) in lines {
        line(out, label, kib);
    }
}

/// One `show_val_kb` line. Every label passed is shorter than the field.
pub(crate) fn line(out: &mut Vec<u8>, label: &str, kib: u64) {
    let start = out.len();
    out.extend_from_slice(label.as_bytes());
    out.push(b':');
    pad_to(out, start, LABEL);
    put(out, format_args!("{kib:>8} kB\n"));
}
