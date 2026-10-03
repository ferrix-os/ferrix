//! RM's own functions that run a privileged instruction, answered in ring 3
//! (`docs/NVIDIA.md` §4.1, "The core").
//!
//! RM's core was built for ring 0. Of its 13 MB, one function executes an
//! instruction ring 3 may not: `osNv_rdcr4`, `mov %cr4, %rax`, which faults.
//! It is defined inside `nv-kernel.o`, not imported, so the core's link
//! binds it to [`nvos_rdcr4`] instead (`core/core-link.py`, `REPLACED`);
//! RM's own copy stays in the core, unchanged and never called. The link
//! refuses a core that has any other privileged instruction
//! (`core-link.py check-core`), so a release that adds one is found at
//! build time, not by a fault.

use core::arch::x86_64::__cpuid;

/// `CR4.OSFXSR`: the OS saves the SSE state across switches.
const CR4_OSFXSR: u32 = 1 << 9;
/// `CR4.OSXSAVE`: the OS enabled `XSAVE` and `XGETBV`.
const CR4_OSXSAVE: u32 = 1 << 18;
/// `CPUID.1:ECX.OSXSAVE`, which mirrors `CR4.OSXSAVE`.
const CPUID_OSXSAVE: u32 = 1 << 27;

/// What `osNv_rdcr4` answers: the two bits RM reads
/// (`src/nvidia/src/kernel/platform/cpu.c`, `RmInitCpuInfo`), and no
/// others. `OSFXSR` is set by every x86-64 kernel that runs SSE code in
/// user space, as Ferrix does; `OSXSAVE` is exactly what `CPUID.1:ECX`
/// bit 27 reports, by the architecture's definition.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_rdcr4() -> u32 {
    let leaf = __cpuid(1);
    let mut value = CR4_OSFXSR;
    if leaf.ecx & CPUID_OSXSAVE != 0 {
        value |= CR4_OSXSAVE;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{CR4_OSFXSR, CR4_OSXSAVE, nvos_rdcr4};

    #[test]
    fn only_the_two_bits_rm_reads() {
        let value = nvos_rdcr4();
        assert_ne!(value & CR4_OSFXSR, 0);
        assert_eq!(value & !(CR4_OSFXSR | CR4_OSXSAVE), 0);
        // Every host this runs on has XSAVE enabled by its kernel.
        assert_ne!(value & CR4_OSXSAVE, 0);
    }
}
