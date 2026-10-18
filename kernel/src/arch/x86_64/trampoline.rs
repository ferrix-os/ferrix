//! The sixteen-bit code an application processor starts in.
//!
//! This is the one place in Ferrix with real-mode assembly, and it is on the
//! allow-list for a reason no amount of Rust can get around: a processor woken
//! by a `SIPI` starts in 16-bit real mode, with no paging, no long mode, no
//! stack and a code segment the interrupt itself chose. There is no calling
//! convention to enter a Rust function with and no address space for it to run
//! in. Something has to build both, and it has to do it in the mode the CPU
//! actually starts in.
//!
//! # What it does, in order
//!
//! 1. Works out where it is. The `SIPI` sets `CS` to the page it was pointed at
//!    shifted right by four, so `CS << 4` is this blob's physical address.
//!    Deriving it rather than being told it is what lets the trampoline live on
//!    whichever low page the frame allocator had spare, instead of on a fixed
//!    address something else might want.
//! 2. Relocates the four absolute addresses it contains — two descriptor table
//!    bases and two far-jump targets — by adding that base.
//! 3. Enters protected mode, then enables `PAE`, loads the kernel's `CR3`, sets
//!    `EFER.LME` and turns paging on, which is the transition to long mode.
//! 4. Far-jumps into a 64-bit code segment, takes the stack the boot CPU
//!    allocated for it, and jumps to Rust with its CPU number in `RDI`.
//!
//! Steps 3 and 4 execute at a *physical* address, because paging has just been
//! turned on and the program counter has not moved. That is why the boot CPU
//! maps this page at its own address before starting anyone — see
//! [`super::smp`].
//!
//! # Why it is assembled rather than emitted as bytes
//!
//! The alternative to `global_asm!` here is a `[u8; N]` of hand-assembled
//! opcodes, which is not more Rust in any sense that matters: it is the same
//! instructions with the mnemonics removed and a new way to be wrong. The
//! `.code16`/`.code32`/`.code64` directives are also the only way to say "these
//! three stretches of instructions are in three different modes", which is the
//! single most important fact about this file.

use core::arch::global_asm;

// Selectors in the trampoline's own GDT. One table serves all three modes: the
// 16-bit code that loads it, the 32-bit code that runs next, and the 64-bit
// code that runs after that.
//
// It is thrown away as soon as Rust is reached, which installs this CPU's real
// GDT. What it has to do is be valid for the four instructions in between.
global_asm!(
    r#"
    .section .rodata.ferrix_ap_trampoline, "a", @progbits
    .balign 16

    .globl ferrix_ap_trampoline_start
ferrix_ap_trampoline_start:
    .code16

    cli
    cld

    // CS was set by the SIPI to (physical address >> 4); DS follows it so that
    // the 16-bit references below, which the assembler emitted as offsets from
    // the start of the blob, land on the blob.
    movw    %cs, %ax
    movw    %ax, %ds
    movzwl  %ax, %ebx
    shll    $4, %ebx

    // The four absolute addresses. Each was assembled as an offset from the
    // start of the blob, so adding the base turns it into a linear address.
    // This is why the boot CPU copies a fresh blob per processor: relocating
    // an already-relocated one would double the base.
    addl    %ebx, (.Lferrix_ap_gdt_pointer + 2 - ferrix_ap_trampoline_start)
    addl    %ebx, (.Lferrix_ap_jump32 - ferrix_ap_trampoline_start)
    addl    %ebx, (.Lferrix_ap_jump64 - ferrix_ap_trampoline_start)

    lgdtl   (.Lferrix_ap_gdt_pointer - ferrix_ap_trampoline_start)

    // Protected mode. The far jump is what reloads CS; until it retires the CPU
    // is still fetching 16-bit instructions.
    movl    %cr0, %eax
    orl     $1, %eax
    movl    %eax, %cr0
    ljmpl   *(.Lferrix_ap_jump32 - ferrix_ap_trampoline_start)

    .code32
.Lferrix_ap_protected:
    movw    $0x10, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs

    // PAE is required for long mode; PGE is set here rather than in Rust
    // because the kernel's mappings are already marked global and a CPU that
    // ignores the bit would keep entries the boot CPU expects it to drop.
    movl    %cr4, %eax
    orl     $((1 << 5) | (1 << 7)), %eax
    movl    %eax, %cr4

    // EBX survived the mode switch, so the parameter block is still reachable.
    movl    (.Lferrix_ap_param_cr3 - ferrix_ap_trampoline_start)(%ebx), %eax
    movl    %eax, %cr3

    // EFER.LME arms long mode; EFER.NXE makes the no-execute bit mean what the
    // kernel's page tables already say it means. Without NXE every mapping the
    // W^X sweep passed becomes executable on this CPU and nowhere else.
    movl    $0xC0000080, %ecx
    rdmsr
    orl     $((1 << 8) | (1 << 11)), %eax
    wrmsr

    // Paging on: with LME set, this is the instruction that enters long mode.
    // WP with it, so that a write to a read-only mapping faults in ring 0 too.
    movl    %cr0, %eax
    orl     $((1 << 31) | (1 << 16)), %eax
    movl    %eax, %cr0

    ljmpl   *(.Lferrix_ap_jump64 - ferrix_ap_trampoline_start)(%ebx)

    .code64
.Lferrix_ap_long:
    movq    (.Lferrix_ap_param_stack - ferrix_ap_trampoline_start)(%rbx), %rsp
    movq    (.Lferrix_ap_param_index - ferrix_ap_trampoline_start)(%rbx), %rdi
    movq    (.Lferrix_ap_param_entry - ferrix_ap_trampoline_start)(%rbx), %rax
    // A null frame pointer and return address, so that anything walking this
    // CPU's stack stops here rather than wandering into the trampoline.
    xorq    %rbp, %rbp
    pushq   $0
    jmpq    *%rax

    .balign 8
.Lferrix_ap_gdt:
    .quad 0                        // null
    .quad 0x00CF9A000000FFFF       // 0x08: 32-bit code, 4 GiB, ring 0
    .quad 0x00CF92000000FFFF       // 0x10: 32-bit data, 4 GiB, ring 0
    .quad 0x00AF9A000000FFFF       // 0x18: 64-bit code, ring 0
.Lferrix_ap_gdt_end:

.Lferrix_ap_gdt_pointer:
    .word .Lferrix_ap_gdt_end - .Lferrix_ap_gdt - 1
    .long .Lferrix_ap_gdt - ferrix_ap_trampoline_start

.Lferrix_ap_jump32:
    .long .Lferrix_ap_protected - ferrix_ap_trampoline_start
    .word 0x08

.Lferrix_ap_jump64:
    .long .Lferrix_ap_long - ferrix_ap_trampoline_start
    .word 0x18

    // Everything the boot CPU has to tell this one. Written through the direct
    // map after the blob is copied, and read here through physical addressing.
    .balign 8
    .globl ferrix_ap_params
ferrix_ap_params:
.Lferrix_ap_param_cr3:
    .quad 0
.Lferrix_ap_param_stack:
    .quad 0
.Lferrix_ap_param_entry:
    .quad 0
.Lferrix_ap_param_index:
    .quad 0

    .globl ferrix_ap_trampoline_end
ferrix_ap_trampoline_end:
"#,
    options(att_syntax)
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
///
/// Named rather than numeric because the assembler decides them: the layout
/// above can grow an instruction without anything here needing to change.
#[derive(Clone, Copy, Debug)]
pub(super) struct Parameters {
    /// Byte offset of `CR3` within the blob.
    pub(super) cr3: usize,
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
    let params = ((&raw const ferrix_ap_params) as usize).saturating_sub(start);
    Parameters {
        cr3: params,
        stack: params + 8,
        entry: params + 16,
        index: params + 24,
    }
}
