//! The restricted images on x86-64: position-independent, touching nothing but
//! what the context hands them, and copied into a VMO the kernel maps below
//! `SHARED_BASE`.

use core::arch::{asm, global_asm};

use ferrix_native_abi::restricted::Context;

/// `int3`'s vector.
pub(crate) const VECTOR_BREAKPOINT: u64 = 3;
/// `#DB`'s.
pub(crate) const VECTOR_DEBUG: u64 = 1;
/// `#UD`'s.
pub(crate) const VECTOR_UNDEFINED: u64 = 6;
/// `#DE`'s.
pub(crate) const VECTOR_DIVIDE: u64 = 0;
/// `#PF`'s.
pub(crate) const VECTOR_PAGE: u64 = 14;

global_asm!(
    r#"
.section .rodata.restricted_image, "a"
.globl restricted_image
restricted_image:

// A system call whose number and arguments the context chose, then a stop.
// `sys_twice`: this one, then image_sys's, whose number is what this one's way back left in RAX.
    syscall
.globl image_sys
image_sys:
    syscall
    int3

// Load from RDI; a stop with what came in RAX.
.globl image_load
image_load:
    movq (%rdi), %rax
    int3

// Store RAX at RDI.
.globl image_store
image_store:
    movq %rax, (%rdi)
    int3

// Run what is at RDI.
.globl image_fetch
image_fetch:
    jmpq *%rdi

// An instruction no processor defines.
.globl image_undefined
image_undefined:
    ud2

// Divide by zero.
.globl image_divide
image_divide:
    xorl %ecx, %ecx
    divl %ecx
    int3

// Touch RDI's page, which is demand-zero: no exit before the stop.
.globl image_touch
image_touch:
    movq $7, (%rdi)
    movq (%rdi), %rax
    int3

// Count in R12 for ever: kicks and preemption.
.globl image_spin
image_spin:
    incq %r12
    jmp image_spin

// Raise the trap flag from ring 3.
.globl image_step
image_step:
    pushfq
    orq $0x100, (%rsp)
    popfq
    nop
    int3

// XMM5 and the high half of YMM5 set from RDI, kept across a system call, read
// back to RAX and RBX.
.globl image_vectors
image_vectors:
    movq %rdi, %xmm5
    vinserti128 $1, %xmm5, %ymm5, %ymm5
    movq %rsi, %rax
    syscall
    vmovq %xmm5, %rax
    vextracti128 $1, %ymm5, %xmm4
    vmovq %xmm4, %rbx
    int3

// The row: R12 round trips of a Linux number, each timed from inside the program
// and its ticks stored at R13 (`MICROKERNEL.md` §7, `bench-rows`' method).
.globl image_bench
image_bench:
    lfence
    rdtsc
    lfence
    shlq $32, %rdx
    orq %rdx, %rax
    movq %rax, %r14
    movl $110, %eax
    syscall
    lfence
    rdtsc
    lfence
    shlq $32, %rdx
    orq %rdx, %rax
    subq %r14, %rax
    movq %rax, (%r13)
    addq $8, %r13
    decq %r12
    jnz image_bench
    int3

// The window (`restricted_window`): calls with nothing between, so an answer of the number's
// own value leaves every register as it was. The first exits and arms; the rest take the stub.
.globl image_armed
image_armed:
    syscall
    syscall
    syscall
    syscall
    int3

// The same, then a load at RDI: what a program can reach once a stub has closed.
.globl image_armed_load
image_armed_load:
    syscall
    syscall
    syscall
    movq (%rdi), %rax
    int3

// R12 armed calls, each answer held to R15; `ud2` if one is not.
.globl image_armed_loop
image_armed_loop:
    movl $110, %eax
    syscall
    cmpq %r15, %rax
    jne image_armed_wrong
    decq %r12
    jnz image_armed_loop
    int3
image_armed_wrong:
    ud2

// XMM5 and YMM5 kept over two stubs, read back to RAX and RBX.
.globl image_armed_vectors
image_armed_vectors:
    movq %rdi, %xmm5
    vinserti128 $1, %xmm5, %ymm5, %ymm5
    movl $110, %eax
    syscall
    syscall
    syscall
    vmovq %xmm5, %rax
    vextracti128 $1, %ymm5, %xmm4
    vmovq %xmm4, %rbx
    int3

// Armed, a stub, another number, then the armed number again.
.globl image_armed_other
image_armed_other:
    syscall
    syscall
    movl $1000, %eax
    syscall
    movl $110, %eax
    syscall
    int3

// 32-bit code: read through GS's descriptor and make an i386 system call.
.code32
.globl image_compat
image_compat:
    movl %gs:0, %ebx
    movl $20, %eax
    movl $0x2222, %ecx
    movl $0x3333, %edx
    movl $0x4444, %esi
    movl $0x5555, %edi
    movl $0x6666, %ebp
    int $0x80
    int3
.code64

.globl restricted_image_end
restricted_image_end:

"#,
    options(att_syntax)
);

unsafe extern "C" {
    static restricted_image: u8;
    static restricted_image_end: u8;
    static image_sys: u8;
    static image_load: u8;
    static image_store: u8;
    static image_fetch: u8;
    static image_undefined: u8;
    static image_divide: u8;
    static image_touch: u8;
    static image_spin: u8;
    static image_step: u8;
    static image_vectors: u8;
    static image_compat: u8;
    static image_bench: u8;
    static image_armed: u8;
    static image_armed_load: u8;
    static image_armed_loop: u8;
    static image_armed_vectors: u8;
    static image_armed_other: u8;
}

/// The images as bytes.
pub(crate) fn image() -> &'static [u8] {
    let start = (&raw const restricted_image).addr();
    let end = (&raw const restricted_image_end).addr();
    // SAFETY: the section this file's assembly defines, between its two symbols,
    // read-only for the program's life.
    unsafe { core::slice::from_raw_parts(start as *const u8, end - start) }
}

/// An image's entry, as an offset in [`image`].
fn offset_of(symbol: *const u8) -> u64 {
    (symbol.addr() - (&raw const restricted_image).addr()) as u64
}

/// The named images, as offsets.
macro_rules! entries {
    ($($name:ident: $symbol:ident),* $(,)?) => {
        $(
            pub(crate) fn $name() -> u64 {
                offset_of(&raw const $symbol)
            }
        )*
    };
}
entries!(
    sys: image_sys,
    load: image_load,
    store: image_store,
    fetch: image_fetch,
    undefined: image_undefined,
    divide: image_divide,
    touch: image_touch,
    spin: image_spin,
    step: image_step,
    vectors: image_vectors,
    compat: image_compat,
    bench: image_bench,
    armed: image_armed,
    armed_load: image_armed_load,
    armed_loop: image_armed_loop,
    armed_vectors: image_armed_vectors,
    armed_other: image_armed_other,
);

/// The code selector of 64-bit user code.
const USER_CS: u64 = 0x33;
/// The code selector of 32-bit user code.
const USER_CS32: u64 = 0x23;
/// The stack selector.
const USER_SS: u64 = 0x2b;

/// A context at `pc` on `sp`, with nothing else set.
pub(crate) fn context(pc: u64, sp: u64) -> Context {
    Context {
        rip: pc,
        rsp: sp,
        rflags: 0x202,
        cs: USER_CS,
        ss: USER_SS,
        ..Context::default()
    }
}

/// [`context`], in 32-bit mode, with a thread-local descriptor at `base` that
/// `gs` names.
pub(crate) fn compat_context(pc: u64, sp: u64, base: u32) -> Context {
    let base = u64::from(base);
    // Present, S=1, DPL 3, read-write data, G=1, D/B=1, limit 0xFFFFF.
    let descriptor =
        0x00CF_F200_0000_FFFF | ((base & 0xFF_FFFF) << 16) | ((base & 0xFF00_0000) << 32);
    Context {
        rip: pc,
        rsp: sp,
        rflags: 0x202,
        cs: USER_CS32,
        ss: USER_SS,
        ds: USER_SS,
        es: USER_SS,
        gs: (12 << 3) | 3,
        tls: [descriptor, 0, 0],
        ..Context::default()
    }
}

/// The argument registers of a system call in a context, by number and words.
pub(crate) fn set_call(context: &mut Context, number: u64, words: [u64; 6]) {
    context.rax = number;
    context.rdi = words[0];
    context.rsi = words[1];
    context.rdx = words[2];
    context.r10 = words[3];
    context.r8 = words[4];
    context.r9 = words[5];
}

/// Put `value` in an image's first argument register.
pub(crate) const fn set_first(context: &mut Context, value: u64) {
    context.rdi = value;
}

/// Put `value` in an image's second argument register.
pub(crate) const fn set_second(context: &mut Context, value: u64) {
    context.rsi = value;
}

/// The result register in a context.
pub(crate) const fn result_of(context: &Context) -> u64 {
    context.rax
}

/// The number a context's call holds.
pub(crate) const fn call_number_of(context: &Context) -> u64 {
    context.rax
}

/// Whether `rcx` and `r11` are as a `SYSCALL` at `call` leaves them.
pub(crate) const fn as_a_call_left(context: &Context, call: u64) -> bool {
    context.rcx == call + 2 && context.r11 & 0x200 != 0
}

/// Past the call at the context's pc, answering `value`.
pub(crate) const fn answer_past_call(context: &mut Context, value: u64) {
    context.rip += 2;
    context.rax = value;
}

/// The next register [`vectors`] reads back to.
pub(crate) const fn second_result_of(context: &Context) -> u64 {
    context.rbx
}

/// The instruction pointer in a context.
pub(crate) const fn pc_of(context: &Context) -> u64 {
    context.rip
}

/// Set in a context every register a program can hold a marked value in, and
/// say whether a context read back holds them.
pub(crate) fn mark_every_register(context: &mut Context) {
    context.rbx = 0xB0;
    context.rbp = 0xB1;
    context.r12 = 0xB2;
    context.r13 = 0xB3;
    context.r14 = 0xB4;
    context.r15 = 0xB5;
}

/// Whether [`mark_every_register`]'s values are all there.
pub(crate) fn every_register_marked(context: &Context) -> bool {
    context.rbx == 0xB0
        && context.rbp == 0xB1
        && context.r12 == 0xB2
        && context.r13 == 0xB3
        && context.r14 == 0xB4
        && context.r15 == 0xB5
}

/// A context wrong in the `case`th way (`docs/microkernel/K1.md` §4), or
/// `None` past the last.
pub(crate) fn forged(good: &Context, case: usize) -> Option<Context> {
    let mut bad = *good;
    match case {
        0 => bad.cs = 0x08,
        1 => bad.cs = 0x10,
        2 => bad.ss = 0x10,
        3 => bad.ss = 0,
        4 => bad.rip = 0xFFFF_8000_0000_0000,
        5 => bad.rip = ferrix_native_abi::restricted::SHARED_BASE,
        6 => bad.rsp = 0xFFFF_8000_0000_0000,
        7 => bad.rflags = 0x202 | (3 << 12),
        8 => bad.rflags = 0x202 | (1 << 17),
        9 => bad.rflags = 0x202 | (1 << 19),
        10 => bad.rflags = 0x202 | (1 << 20),
        11 => bad.rflags = 0x202 | (1 << 8),
        12 => bad.rflags = 0x202 | (1 << 14),
        13 => bad.rflags = 0x002,
        14 => bad.rflags = 0x200,
        15 => bad.ds = 0x08,
        16 => bad.gs = (13 << 3) | 3,
        17 => bad.es = 1 << 16,
        18 => bad.fs = 4 | 3,
        19 => bad.fs_base = 0x8000_0000_0000,
        20 => bad.gs_base = ferrix_native_abi::restricted::SHARED_BASE,
        // A descriptor with DPL 0, and one with L set, and a system one.
        21 => bad.tls[0] = 0x00CF_9200_0000_FFFF,
        22 => bad.tls[0] = 0x00AF_F200_0000_FFFF,
        23 => bad.tls[1] = 0x0000_E200_0000_FFFF,
        24 => {
            bad.cs = USER_CS32;
            bad.rip = 0x1_0000_0000;
        }
        _ => return None,
    }
    Some(bad)
}

/// Set this thread's own XMM5 and the high half of YMM5 to `value`: what
/// normal mode's vector registers are doing while a program's are not its own.
pub(crate) fn clobber_vectors(value: u64) {
    // SAFETY: only the registers named, which the program's soft-float code
    // never holds anything in.
    unsafe {
        asm!(
            "vmovq xmm5, {value}",
            "vinserti128 ymm5, ymm5, xmm5, 1",
            value = in(reg) value,
            out("xmm5") _,
        );
    }
}

/// What [`clobber_vectors`] left, if this mode's registers are its own again.
pub(crate) fn read_vectors() -> u64 {
    let value: u64;
    // SAFETY: AVX where `has_vectors` says so, as for `clobber_vectors`.
    unsafe {
        core::arch::asm!("vmovq {value}, xmm5", value = out(reg) value, options(nostack, nomem));
    }
    value
}

/// Whether the processor has what [`vectors`] runs.
pub(crate) fn has_vectors() -> bool {
    let leaf1 = core::arch::x86_64::__cpuid(1);
    let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
    leaf1.ecx & (1 << 28) != 0 && leaf7.ebx & (1 << 5) != 0
}

/// What a fetch from a page without execute faults with.
pub(crate) const VECTOR_FETCH: u64 = VECTOR_PAGE;
/// What the next stop is, after [`step`]'s.
pub(crate) const VECTOR_AFTER_STEP: u64 = VECTOR_BREAKPOINT;
/// Whether a divide by zero stops at all.
pub(crate) const HAS_DIVIDE: bool = true;
/// Say how many round trips [`bench`] makes, and where it stores their ticks.
pub(crate) const fn set_bench(context: &mut Context, count: u64, at: u64) {
    context.r12 = count;
    context.r13 = at;
}

/// Say how many calls [`armed_loop`] makes and what each must answer.
pub(crate) const fn set_armed_loop(context: &mut Context, count: u64, answer: u64) {
    context.r12 = count;
    context.r15 = answer;
}

/// The counter [`bench`] reads.
pub(crate) fn ticks() -> u64 {
    // SAFETY: `lfence` is unprivileged and touches no memory.
    unsafe { core::arch::x86_64::_mm_lfence() };
    // SAFETY: `rdtsc` is unprivileged and touches no memory.
    let tsc = unsafe { core::arch::x86_64::_rdtsc() };
    // SAFETY: as the first.
    unsafe { core::arch::x86_64::_mm_lfence() };
    tsc
}

/// Whether this architecture has 32-bit mode for programs.
pub(crate) const HAS_COMPAT: bool = true;
/// `MXCSR` at its offset in the vector image, with a reserved bit set: an
/// image the processor would refuse.
pub(crate) const VECTOR_BAD_AT: usize = 24;

/// The value of `getppid` on this table.
pub(crate) const GETPPID: u64 = 110;
/// The value of `getpid` on the i386 table.
pub(crate) const I386_GETPID: u64 = 20;

/// A second system call just before [`sys`]'s: `syscall` is two bytes.
pub(crate) fn sys_twice() -> u64 {
    sys() - 2
}
