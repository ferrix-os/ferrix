//! What crosses the seam: counters kept from boot for the second "seam
//! measured" row (`docs/BACKLOG.md`, `docs/OPAQUE-KERNEL.md` S0).
//!
//! The claim under test is the 2026-09-16 decision's: a build-like workload's
//! system calls land on the page cache, a function call away, and only disk
//! traffic crosses to ring 3, batched through a ring. So three things are
//! counted, side by side:
//!
//! * Linux system calls answered, the workload's whole size;
//! * file pages served from a page cache's VMO -- read, or faulted into a
//!   mapping -- against file pages filled from a source, and of those the
//!   ones read from a disk, through the block ring (a self-check's source
//!   makes its pages up);
//! * block-ring submissions and completions, the crossings themselves.
//!
//! `/proc/ferrix-seam` shows them as one line, which `cargo xtask test-vfs`
//! prints at its end. Relaxed atomics: each is a count, read once at the end,
//! and nothing orders against it.

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

/// Linux system calls answered before every processor's record is kept;
/// after that each processor counts its own (`smp::PerCpu::syscalls`), so
/// the add every call makes is no locked add on a line every processor
/// writes (po10-pipe P2, the consultant's B4).
static EARLY_SYSCALLS: AtomicU64 = AtomicU64::new(0);
/// File pages read or faulted in from a page cache's VMO without a fill.
static SERVED: AtomicU64 = AtomicU64::new(0);
/// File pages filled from a source.
static FILLED: AtomicU64 = AtomicU64::new(0);
/// Of those, pages filled by reading a disk: through the block ring.
static FROM_DISK: AtomicU64 = AtomicU64::new(0);
/// Commands put on a block ring.
static SUBMITTED: AtomicU64 = AtomicU64::new(0);
/// Completions taken off a block ring.
static COMPLETED: AtomicU64 = AtomicU64::new(0);

/// One Linux system call answered.
pub(crate) fn syscall() {
    if crate::sched::records_kept() {
        // SAFETY: (SHARED) the records are kept, so this processor's is
        // installed, and the offset is its system call count's, an aligned
        // word only this processor writes.
        let _ = unsafe { crate::arch::this_cpu_add(crate::smp::SYSCALLS_WORD_OFFSET, 1) };
    } else {
        let _ = EARLY_SYSCALLS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Linux system calls answered so far: the early count and every
/// processor's own. `leave_out` names a processor whose count is not added,
/// for the negative control of the check that requires the sum exact.
pub(crate) fn syscalls_except(leave_out: Option<usize>) -> u64 {
    let early = EARLY_SYSCALLS.load(Ordering::Relaxed);
    let cpus = crate::smp::topology().map_or(&[][..], crate::smp::Topology::cpus);
    cpus.iter()
        .enumerate()
        .filter(|&(cpu, _)| Some(cpu) != leave_out)
        .fold(early, |sum, (_, record)| {
            sum.wrapping_add(record.syscalls.read())
        })
}

/// Linux system calls answered so far.
pub(crate) fn syscalls() -> u64 {
    syscalls_except(None)
}

/// `pages` file pages served from a page cache without a fill.
pub(crate) fn served(pages: u64) {
    let _ = SERVED.fetch_add(pages, Ordering::Relaxed);
}

/// `pages` file pages filled from a source, which read a disk if `disk`.
pub(crate) fn filled(pages: u64, disk: bool) {
    let _ = FILLED.fetch_add(pages, Ordering::Relaxed);
    if disk {
        let _ = FROM_DISK.fetch_add(pages, Ordering::Relaxed);
    }
}

/// One command put on a block ring.
pub(crate) fn submitted() {
    let _ = SUBMITTED.fetch_add(1, Ordering::Relaxed);
}

/// One completion taken off a block ring.
pub(crate) fn completed() {
    let _ = COMPLETED.fetch_add(1, Ordering::Relaxed);
}

/// `/proc/ferrix-seam`: the counters as one line, labelled.
pub(crate) fn render() -> Vec<u8> {
    let mut line = alloc::string::String::new();
    let _ = writeln!(
        line,
        "seam syscalls {} served {} filled {} from-disk {} submitted {} completed {}",
        syscalls(),
        SERVED.load(Ordering::Relaxed),
        FILLED.load(Ordering::Relaxed),
        FROM_DISK.load(Ordering::Relaxed),
        SUBMITTED.load(Ordering::Relaxed),
        COMPLETED.load(Ordering::Relaxed),
    );
    line.into_bytes()
}
