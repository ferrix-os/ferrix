//! The framebuffer window's write-combining, x86-64's side
//! (`docs/DISPLAY.md` §8).
//!
//! The kernel maps the boot framebuffer write-combining from its first map,
//! on every boot that has one, lent under `ferrix.efifb` or not: the boot
//! processor programs its PAT before the map ([`super::program_memory_types`]),
//! and every secondary in its trampoline, before it turns paging on
//! (`smp`), so no processor translates the window's leaves by any other
//! table (`L.x86_64.127`, `L.x86_64.176`).
//!
//! What the rest of the kernel asks of the processor here: the flags the
//! window is mapped with, chosen from the boot processor's table as it read
//! back ([`framebuffer_flags`]); the window's cache lines flushed before
//! its first store ([`flush_window`]); one entry of its page attribute
//! table read back; the memory type its MTRRs give a physical range; and a
//! store fence that empties its write-combining buffers. The PAT is
//! programmed at each processor's bring-up (`cpu::program_pat`) and the
//! MTRRs are firmware's: nothing here writes either.
//!
//! The MSR numbers are Linux's (`arch/x86/include/asm/msr-index.h`:
//! `MSR_MTRRcap` 0xFE, `MSR_MTRRdefType` 0x2FF, `MSR_IA32_CR_PAT` 0x277)
//! and QEMU's (`target/i386/cpu.h`: `MSR_MTRRphysBase(n)` 0x200 + 2n,
//! `MSR_MTRRphysMask(n)` 0x201 + 2n); the type encodings are Linux's
//! `uapi/asm/mtrr.h` (`MTRR_TYPE_UNCACHABLE` 0, `WRCOMB` 1, `WRTHROUGH` 4,
//! `WRPROT` 5, `WRBACK` 6), which the PAT's entries share.

use core::arch::asm;
use core::arch::x86_64::__cpuid;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_paging::MapFlags;

use super::cpu;

pub(crate) mod check;

/// The boot processor's `IA32_PAT` as it read back when the window was
/// mapped, or 0 when it was not read (no table programmed, no `CLFLUSH`):
/// what [`framebuffer_flags`] chose by, kept for stage 9's check.
static WINDOW_PAT: AtomicU64 = AtomicU64::new(0);
/// The cache lines [`flush_window`] flushed.
static FLUSHED_LINES: AtomicU64 = AtomicU64::new(0);
/// And the time stamp counter's ticks it took, for the boot line.
static FLUSH_TICKS: AtomicU64 = AtomicU64::new(0);

/// Whether this processor has `CLFLUSH`: CPUID.(EAX=1):EDX.CLFSH, bit 19.
fn has_clflush() -> bool {
    __cpuid(1).edx & (1 << 19) != 0
}

/// The flags the kernel's framebuffer window is mapped with, first thing in
/// `kmain` and after [`super::program_memory_types`]: write-combining when
/// the boot processor's PAT read back [`ferrix_paging::X86_PAT`] and it can
/// flush the window's cache lines first, device memory otherwise
/// ([`MapFlags::x86_framebuffer`]). The table read is kept for stage 9.
pub(crate) fn framebuffer_flags() -> MapFlags {
    let pat = (super::boot_pat_programmed() && has_clflush()).then(|| {
        // SAFETY: (SYSREG) the boot processor programmed and read back its
        // PAT, so CPUID reported the register.
        unsafe { cpu::read_msr(cpu::IA32_PAT) }
    });
    WINDOW_PAT.store(pat.unwrap_or(0), Ordering::Release);
    // Control (c)'s base: write-combining forced, whatever the table.
    let _ = MapFlags::x86_framebuffer(pat);
    MapFlags::KERNEL_COMBINING
}

/// The table [`framebuffer_flags`] chose the window's type by, when it read
/// one.
pub(crate) fn window_pat() -> Option<u64> {
    Some(WINDOW_PAT.load(Ordering::Acquire)).filter(|&pat| pat != 0)
}

/// Write back and invalidate every cache line of `virt..virt + len`, the
/// framebuffer window just mapped, and fence: before the boot console's or
/// the panic screen's first store. Firmware mapped the framebuffer before
/// the kernel did, under its own tables, and where its MTRR type is
/// write-back a line of it may still be in a cache: one beside the
/// write-combining leaves is the alias the SDM (Vol. 3A §11.12.4) forbids.
/// `CLFLUSH` reaches every cache of the coherence domain whatever the type
/// of the address it is given. Nothing without `CLFLUSH`: the window is
/// then device memory ([`framebuffer_flags`]), and an uncached store
/// allocates no line.
pub(crate) fn flush_window(virt: u64, len: u64) {
    if !has_clflush() {
        return;
    }
    let line = line_size();
    let start = cpu::rdtsc();
    let mut lines = 0_u64;
    let mut at = virt - virt % line;
    let end = virt.saturating_add(len);
    while at < end {
        // SAFETY: (SYSREG) `at` is inside the window `kmain` has just mapped,
        // so the flush's linear address translates; `CLFLUSH` writes no byte
        // of memory a reference reaches.
        unsafe { asm!("clflush [{}]", in(reg) at, options(nostack, preserves_flags)) };
        lines = lines.saturating_add(1);
        at = at.saturating_add(line);
    }
    // SAFETY: (SYSREG) a fence: it changes no register or memory. `CLFLUSH`
    // is ordered by `MFENCE`, so every line is gone before the first store.
    unsafe { asm!("mfence", options(nostack, preserves_flags)) };
    FLUSHED_LINES.store(lines, Ordering::Release);
    FLUSH_TICKS.store(cpu::rdtsc().wrapping_sub(start), Ordering::Release);
}

/// This processor's cache line size: CPUID.(EAX=1):EBX[15:8], in
/// eight-byte units, or 64 where it says none.
fn line_size() -> u64 {
    let line = u64::from((__cpuid(1).ebx >> 8) & 0xFF) * 8;
    if line == 0 { 64 } else { line }
}

/// The cache lines `virt..virt + len` touches: what [`flush_window`] must
/// have flushed, for stage 9's check.
pub(crate) fn window_lines(virt: u64, len: u64) -> u64 {
    let line = line_size();
    virt.saturating_add(len)
        .saturating_sub(virt - virt % line)
        .div_ceil(line)
}

/// The lines [`flush_window`] flushed and the time stamp counter's ticks
/// it took.
pub(crate) fn window_flush() -> (u64, u64) {
    (
        FLUSHED_LINES.load(Ordering::Acquire),
        FLUSH_TICKS.load(Ordering::Acquire),
    )
}

/// `IA32_MTRRCAP`: the variable ranges' count in bits 7:0.
const MTRR_CAP: u32 = 0xFE;
/// `IA32_MTRR_DEF_TYPE`: the default type in bits 7:0, enable in bit 11.
const MTRR_DEF_TYPE: u32 = 0x2FF;
/// `IA32_MTRR_PHYSBASE0`; range `n`'s base is at `+ 2n`, its mask one after.
const MTRR_PHYS_BASE0: u32 = 0x200;
/// The variable ranges read: processors have 8 to 10, and one with more
/// than this gets no answer rather than a partial one.
const MAX_VARIABLE: usize = 32;

/// This processor's page attribute table, as it reads now: a byte a
/// memory type for each of the eight entries a leaf may select.
pub(crate) fn pat() -> Option<u64> {
    // SAFETY: (SYSREG) every processor this kernel runs on has the PAT: its
    // bring-up programmed it (`cpu::program_pat`), and stage 9 stops the boot
    // where one did not.
    Some(unsafe { cpu::read_msr(cpu::IA32_PAT) })
}

/// The memory type this processor's MTRRs give every page of
/// `phys..phys + len`, when it is one type for all of them
/// ([`ferrix_paging::mtrr::range_type`]); `None` when the processor has no
/// MTRRs, more than [`MAX_VARIABLE`] variable ranges, or the range has no
/// one type.
///
/// For a boot line only: a page the PAT maps write-combining is
/// write-combining under every MTRR type, so nothing is decided by this.
pub(crate) fn mtrr_type(phys: u64, len: u64) -> Option<u8> {
    // CPUID.(EAX=1):EDX.MTRR is bit 12.
    if __cpuid(1).edx & (1 << 12) == 0 {
        return None;
    }
    // SAFETY: (SYSREG) CPUID reported the MTRRs, so the register exists.
    let cap = unsafe { cpu::read_msr(MTRR_CAP) };
    // SAFETY: (SYSREG) as above.
    let default = unsafe { cpu::read_msr(MTRR_DEF_TYPE) };
    let count = usize::try_from(cap & 0xFF).ok()?;
    if count > MAX_VARIABLE {
        return None;
    }
    let mut ranges = [(0_u64, 0_u64); MAX_VARIABLE];
    for (range, slot) in (0_u32..).zip(ranges.iter_mut().take(count)) {
        let at = MTRR_PHYS_BASE0 + 2 * range;
        // SAFETY: (SYSREG) `range` is below `MTRRcap.VCNT`, so its base exists.
        let base = unsafe { cpu::read_msr(at) };
        // SAFETY: (SYSREG) and its mask.
        let mask = unsafe { cpu::read_msr(at + 1) };
        *slot = (base, mask);
    }
    ferrix_paging::mtrr::range_type(default, ranges.get(..count)?, phys, len)
}

/// Make every write-combining store this processor has made globally
/// visible before any store after: `sfence`. What the panic screen ends
/// its drawing with on a framebuffer mapped write-combining, whose stores
/// may otherwise wait in a fill buffer.
pub(crate) fn drain_write_combining() {
    // SAFETY: (SYSREG) a fence: it changes no register or memory. Not `nomem`, so the
    // compiler keeps the drawing's stores before it.
    unsafe { asm!("sfence", options(nostack, preserves_flags)) };
}
