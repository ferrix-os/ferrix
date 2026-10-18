//! The instructions an application processor runs before it has an address
//! space.
//!
//! PSCI `CPU_ON` starts a core at a physical address with the MMU off, the
//! caches off and no stack. Everything Rust needs — a translation regime, so
//! that the kernel's virtual addresses mean anything, and a stack pointer — has
//! to be established before the first Rust instruction, which is why this is
//! assembly and why it is on the allow-list.
//!
//! It is the same sequence the loader runs in `boot/src/arch/aarch64.rs`, minus
//! its hardest part: the loader is handed a machine with the MMU already *on*
//! and has to turn it off before it can reprogram `TCR_EL1`, executing the
//! middle of the sequence with the program counter as a physical address. Here
//! the MMU is off to begin with, so this only has to turn it on.
//!
//! # Why it reads its parameters rather than being given them
//!
//! `CPU_ON` carries exactly one argument through to the new core — the context
//! identifier — and nothing else. Everything else has to be found, and the
//! only thing the core knows about itself is the address it is executing at.
//! So the parameters sit at a known offset from that address and are reached
//! with `adr`, which is PC-relative and therefore correct both before the MMU
//! comes on and after, since the page is identity mapped either way.

use core::arch::global_asm;

global_asm!(
    r#"
    .section .rodata.ferrix_ap_trampoline, "a"
    .balign 16

    .globl ferrix_ap_trampoline_start
ferrix_ap_trampoline_start:
    // PSCI leaves the new core with interrupts unmasked and no vector table.
    // Masking them is the first instruction for the same reason `init_traps`
    // is the first thing the boot CPU does: until this core loads VBAR_EL1 an
    // interrupt is a jump into memory that belonged to firmware.
    msr     daifset, #0xf

    // PC-relative, so this is the physical address of the block below.
    adr     x8, ferrix_ap_params

    ldr     x1, [x8, #0]
    msr     mair_el1, x1
    ldr     x2, [x8, #8]
    msr     tcr_el1, x2
    ldr     x3, [x8, #16]
    msr     ttbr0_el1, x3
    ldr     x4, [x8, #24]
    msr     ttbr1_el1, x4
    isb

    // Nothing in this core's TLB can be trusted: it has just been reset, and
    // what a reset leaves there is not architecturally defined.
    tlbi    vmalle1
    dsb     nsh
    isb

    // MMU, data cache and instruction cache on, in one write. The `isb` is what
    // makes the next instruction be fetched through the regime just installed
    // rather than through the physical addressing that got us here — and it
    // works because the address being executed is identity mapped in TTBR0.
    mrs     x7, sctlr_el1
    orr     x7, x7, #(1 << 0)
    orr     x7, x7, #(1 << 2)
    orr     x7, x7, #(1 << 12)
    msr     sctlr_el1, x7
    isb

    ldr     x5, [x8, #32]
    ldr     x6, [x8, #40]
    ldr     x0, [x8, #48]
    mov     sp, x5
    // A null frame pointer and link register, so anything walking this core's
    // stack stops here rather than wandering into the trampoline.
    mov     x29, xzr
    mov     x30, xzr
    br      x6

    .balign 8
    .globl ferrix_ap_params
ferrix_ap_params:
    .quad 0     // MAIR_EL1
    .quad 0     // TCR_EL1
    .quad 0     // TTBR0_EL1: the identity mapping of this page
    .quad 0     // TTBR1_EL1: the kernel
    .quad 0     // stack pointer
    .quad 0     // Rust entry point
    .quad 0     // CPU number

    .globl ferrix_ap_trampoline_end
ferrix_ap_trampoline_end:
"#
);

unsafe extern "C" {
    /// First byte of the blob.
    static ferrix_ap_trampoline_start: u8;
    /// One past its last byte.
    static ferrix_ap_trampoline_end: u8;
    /// The parameter block inside it.
    static ferrix_ap_params: u8;
}

/// Where the parameters sit, as offsets into the blob.
#[derive(Clone, Copy, Debug)]
pub(super) struct Parameters {
    /// Byte offset of `MAIR_EL1`.
    pub(super) mair: usize,
    /// Byte offset of `TCR_EL1`.
    pub(super) tcr: usize,
    /// Byte offset of `TTBR0_EL1`.
    pub(super) ttbr0: usize,
    /// Byte offset of `TTBR1_EL1`.
    pub(super) ttbr1: usize,
    /// Byte offset of the stack pointer.
    pub(super) stack: usize,
    /// Byte offset of the Rust entry point.
    pub(super) entry: usize,
    /// Byte offset of the CPU number.
    pub(super) index: usize,
}

/// The blob, as bytes the caller copies.
pub(super) fn blob() -> &'static [u8] {
    let start = &raw const ferrix_ap_trampoline_start;
    let end = &raw const ferrix_ap_trampoline_end;
    // SAFETY: both symbols are defined by the assembly above, in one section,
    // with `start` before `end`, so the difference is the blob's length and the
    // whole of it is initialised read-only data in the kernel image.
    unsafe { core::slice::from_raw_parts(start, (end as usize).saturating_sub(start as usize)) }
}

/// Where the parameters are inside it.
pub(super) fn parameters() -> Parameters {
    let start = (&raw const ferrix_ap_trampoline_start) as usize;
    let at = ((&raw const ferrix_ap_params) as usize).saturating_sub(start);
    Parameters {
        mair: at,
        tcr: at + 8,
        ttbr0: at + 16,
        ttbr1: at + 24,
        stack: at + 32,
        entry: at + 40,
        index: at + 48,
    }
}
