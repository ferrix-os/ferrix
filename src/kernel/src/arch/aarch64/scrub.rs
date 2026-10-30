//! The RAM scrubber's architecture half on AArch64 (`docs/RAM-SCRUBBER.md`
//! §3.2, `L.aarch64.68`): writes that read nothing, the eviction, and
//! whether this kernel runs as a guest.

use core::arch::asm;

use ferrix_bootinfo::BootView;

use super::cpu;

/// `DCZID_EL0.DZP`: `DC ZVA` is prohibited.
const DZP: u64 = 1 << 4;

/// `DCZID_EL0`.
fn dczid() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading `DCZID_EL0` has no side effects.
    unsafe { asm!("mrs {}, dczid_el0", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Whether this processor can scrub: only where `DC ZVA` is allowed, the
/// one AArch64 write of a whole block that reads nothing.
pub(crate) fn scrub_ready() -> Result<(), &'static str> {
    if dczid() & DZP != 0 {
        return Err("DC ZVA is prohibited here (DCZID_EL0.DZP)");
    }
    Ok(())
}

/// Write `len` bytes at `virt` -- whole `DC ZVA` blocks, `virt`
/// block-aligned -- with `value(offset)` in each 64-bit word, reading no
/// memory the scrubber has not written: `DC ZVA` zeroes each block (the
/// size from `DCZID_EL0.BS`) without a linefill; the stores that follow
/// may fill a line from memory, and what they read there is the zeros just
/// written.
pub(crate) fn fill_without_reading(virt: u64, len: u64, value: &dyn Fn(u64) -> u64) {
    let block = 4u64 << (dczid() & 0xF);
    let end = virt.saturating_add(len);
    let mut at = virt;
    while at < end {
        // SAFETY: (SYSREG) `dc zva` zeroes one block at an address in the
        // caller's range, which is mapped normal memory and the scrubber's
        // alone.
        unsafe { asm!("dc zva, {}", in(reg) at, options(nostack, preserves_flags)) };
        at = at.saturating_add(block);
    }
    // SAFETY: (SYSREG) a barrier, completing the zeroing before the stores.
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
    let mut word = virt;
    while word < end {
        // SAFETY: (KMEM) an aligned word inside the caller's range, which is
        // mapped and the scrubber's alone.
        unsafe { core::ptr::write_volatile(word as *mut u64, value(word - virt)) };
        word += 8;
    }
}

/// Evict every line of `virt..virt + len` to the point of coherency: `DC
/// CIVAC` per line (`CTR_EL0.DminLine`), then `DSB SY`
/// (`cpu::clean_invalidate_to_poc`).
pub(crate) fn evict_to_coherency(virt: u64, len: u64) {
    cpu::clean_invalidate_to_poc(virt, len);
}

/// Whether this kernel runs as a guest, for the stand-down
/// (`docs/RAM-SCRUBBER.md` §3.3): a hypervisor is seen
/// ([`hypervisor_seen`]), or firmware names no PSCI conduit at all, which is
/// taken for a guest: fail closed.
pub(crate) fn is_guest(view: &BootView<'_>) -> bool {
    hypervisor_seen(view) || super::smp::psci_conduit(view).is_err()
}

/// Whether a hypervisor is positively seen, the only case the gate's
/// override is honoured in (ledger 5140 (1)): PSCI is reached through `hvc`,
/// so a hypervisor answers it (QEMU's `virt`, the Pixel 7's crosvm), or the
/// device tree has a `/hypervisor` node. A conduit that cannot be read is
/// not evidence.
pub(crate) fn hypervisor_seen(view: &BootView<'_>) -> bool {
    let hypervisor_node = view.raw().rsdp == 0
        && crate::discovery::fdt::open(view)
            .is_ok_and(|tree| tree.find_node("/hypervisor").is_some());
    hypervisor_node || matches!(super::smp::psci_conduit(view), Ok(super::smp::Conduit::Hvc))
}
