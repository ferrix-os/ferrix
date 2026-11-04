//! The processor as a ring-3 program can ask it.

pub const XSTATE_X87_SSE: u64 = 0b011;
pub const XSTATE_AVX: u64 = 0b100;
pub const XSAVE_AREA_BYTES: u64 = 832;

/// What K1's image holds: x87, SSE and AVX where CPUID says so.
///
/// Asked once and kept: `CPUID` is a virtual machine exit on every hypervisor, about
/// 1.6 us under KVM, and a signal's way in and out asked seven times (docs/microkernel/
/// PERFORMANCE.md). The processor's answer does not change while linuxd runs.
pub fn extended_state_components() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COMPONENTS: AtomicU64 = AtomicU64::new(0);
    let known = COMPONENTS.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    // SAFETY: CPUID is unprivileged, and every x86-64 processor has it.
    let leaf1 = unsafe { core::arch::x86_64::__cpuid(1) };
    let avx = leaf1.ecx & (1 << 28) != 0 && leaf1.ecx & (1 << 27) != 0;
    let components = if avx {
        XSTATE_X87_SSE | XSTATE_AVX
    } else {
        XSTATE_X87_SSE
    };
    COMPONENTS.store(components, Ordering::Relaxed);
    components
}
/// The data selectors: K1's context holds them at a restricted exit.
pub fn read_data_selectors() -> [u16; 4] {
    crate::restricted::selectors()
}
