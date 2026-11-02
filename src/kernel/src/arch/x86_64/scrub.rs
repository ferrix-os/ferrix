//! The RAM scrubber's architecture half on x86-64 (`docs/RAM-SCRUBBER.md`
//! §3.2, `L.x86_64.243`): writes that read nothing, the eviction, and
//! whether this kernel runs as a guest.

use core::arch::asm;

use super::cpu;
use ferrix_bootinfo::BootView;
use ferrix_sync::IrqControl;

/// Whether this processor can scrub: it always can on x86-64, whose
/// `MOVNTI` writes without reading (SSE2, which every x86-64 has).
pub(crate) const fn scrub_ready() -> Result<(), &'static str> {
    Ok(())
}

/// Write `len` bytes at `virt` -- whole cache lines, `virt` line-aligned --
/// with `value(offset)` in each 64-bit word, reading nothing: `MOVNTI` is a
/// non-temporal store, write-combining, with no read for ownership (SDM
/// vol. 2B, MOVNTI). Each line's stores are made with interrupts masked,
/// so that an interrupt cannot flush a part-written line from the
/// write-combining buffer, which a memory controller with ECC would
/// complete by reading the old line; an SMI or NMI still can (SAFETY-MANUAL `AoU-38`'s
/// residual). `SFENCE` then orders them before what follows.
pub(crate) fn fill_without_reading(virt: u64, len: u64, value: &dyn Fn(u64) -> u64) {
    let line = cpu::clflush_line();
    let mut at = virt;
    let end = virt.saturating_add(len);
    while at < end {
        let saved = <super::Irq as IrqControl>::disable();
        let mut word = at;
        while word < at.saturating_add(line) && word < end {
            let written = value(word - virt);
            // SAFETY: (SYSREG) `movnti` stores one 64-bit word at an address in the
            // caller's range, which is mapped and the scrubber's alone.
            unsafe {
                asm!("movnti [{0}], {1}", in(reg) word, in(reg) written, options(nostack, preserves_flags));
            }
            word += 8;
        }
        <super::Irq as IrqControl>::restore(saved);
        at = at.saturating_add(line);
    }
    // SAFETY: (SYSREG) a barrier, ordering the non-temporal stores above before
    // the eviction and the reads that follow.
    unsafe { asm!("sfence", options(nostack, preserves_flags)) };
}

/// Evict every line of `virt..virt + len` to the point of coherency, so
/// that the next read of it is a read of memory: `CLFLUSH` per line, the
/// line from `CPUID` leaf 1, then `MFENCE` (`cpu::clean_for_walker`).
pub(crate) fn evict_to_coherency(virt: u64, len: u64) {
    cpu::clean_for_walker(virt, len);
}

/// Whether this kernel runs as a guest: `CPUID` leaf 1 `ECX` bit 31, the
/// bit hypervisors set and processors leave clear.
pub(crate) fn is_guest(_view: &BootView<'_>) -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 31) != 0
}

/// Whether a hypervisor is positively seen, the only case the gate's
/// override is honoured in (ledger 5140 (1)): on x86-64 the same bit.
pub(crate) fn hypervisor_seen(view: &BootView<'_>) -> bool {
    is_guest(view)
}
