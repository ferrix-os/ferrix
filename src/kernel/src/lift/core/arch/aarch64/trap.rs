//! A program's registers in a form the load's signal frames know.

/// The register state at the point a trap was taken, as K1's context holds it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TrapFrame {
    /// `x0` to `x30`.
    pub x: [u64; 31],
    /// `SP_EL0`.
    pub sp: u64,
    /// Where the program resumes.
    pub elr: u64,
    /// `SPSR_EL1`.
    pub spsr: u64,
    /// Unused by the shim's frames: K1's exit carries the syndrome.
    pub esr: u64,
    /// As `esr`.
    pub far: u64,
}

/// The status a program returns with: EL0t, interrupts open as the kernel's are.
pub const USER_SPSR: u64 = 0;
