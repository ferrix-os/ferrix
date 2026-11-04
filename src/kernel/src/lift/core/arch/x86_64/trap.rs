//! A program's registers in a form the load's signal frames know.

/// The register state at the point a trap was taken.
///
/// Field order is the order the entry stub pushes them, which is the reverse of
/// the order they appear in memory — the stack grows down, so the last register
/// pushed has the lowest address and is the first field.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TrapFrame {
    pub(crate) r15: u64,
    pub(crate) r14: u64,
    pub(crate) r13: u64,
    pub(crate) r12: u64,
    pub(crate) r11: u64,
    pub(crate) r10: u64,
    pub(crate) r9: u64,
    pub(crate) r8: u64,
    pub(crate) rbp: u64,
    pub(crate) rdi: u64,
    pub(crate) rsi: u64,
    pub(crate) rdx: u64,
    pub(crate) rcx: u64,
    pub(crate) rbx: u64,
    pub(crate) rax: u64,

    /// Which vector was taken. Pushed by the stub.
    pub(crate) vector: u64,
    /// The CPU's error code, or zero where the CPU pushes none.
    pub(crate) error_code: u64,

    // Pushed by the CPU itself. `rsp` and `ss` are pushed unconditionally in
    // long mode, even for a trap that did not change privilege level — unlike
    // 32-bit x86, where they appear only on a ring change.
    pub(crate) rip: u64,
    pub(crate) cs: u64,
    pub(crate) rflags: u64,
    pub(crate) rsp: u64,
    pub(crate) ss: u64,
}

pub const LEGACY_SYSCALL_VECTOR: u64 = 0x80;
