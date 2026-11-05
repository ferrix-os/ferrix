//! The restricted images on `AArch64`: as x86-64's, in A64.

use core::arch::{asm, global_asm};

use ferrix_native_abi::restricted::Context;

/// `ESR_EL1.EC` of `brk`.
pub(crate) const VECTOR_BREAKPOINT: u64 = 0x3C;
/// Of an unknown instruction.
pub(crate) const VECTOR_UNDEFINED: u64 = 0;
/// What [`step`] stops with: A64 has no flag EL0 can raise, so an unknown
/// instruction that stays where it is.
pub(crate) const VECTOR_DEBUG: u64 = 0;
/// What a divide by zero would stop with: nothing does.
pub(crate) const VECTOR_DIVIDE: u64 = 0;
/// Of a data abort from EL0.
pub(crate) const VECTOR_PAGE: u64 = 0x24;
/// Of an instruction abort from EL0.
pub(crate) const VECTOR_FETCH: u64 = 0x20;
/// What the stop after [`step`]'s is: the same one, retried.
pub(crate) const VECTOR_AFTER_STEP: u64 = 0;
/// Whether a divide by zero stops at all.
pub(crate) const HAS_DIVIDE: bool = false;

global_asm!(
    r#"
.arch_extension fp
.arch_extension simd
.section .rodata.restricted_image, "a"
.globl restricted_image
restricted_image:
// `sys_twice`: this one, then image_sys's, the same number in X8.
    svc #0
.globl image_sys
image_sys:
    svc #0
    brk #0
.globl image_load
image_load:
    ldr x0, [x0]
    brk #0
.globl image_store
image_store:
    str x1, [x0]
    brk #0
.globl image_fetch
image_fetch:
    br x0
.globl image_undefined
image_undefined:
    udf #0
.globl image_divide
image_divide:
    brk #0
.globl image_touch
image_touch:
    mov x1, #7
    str x1, [x0]
    ldr x0, [x0]
    brk #0
.globl image_spin
image_spin:
    add x19, x19, #1
    b image_spin
.globl image_step
image_step:
    udf #0
// The row: X20 round trips of a Linux number, each timed from inside the program
// and its ticks stored at X21.
.globl image_bench
image_bench:
    isb
    mrs x22, cntvct_el0
    mov x8, #173
    svc #0
    isb
    mrs x23, cntvct_el0
    sub x23, x23, x22
    str x23, [x21], #8
    subs x20, x20, #1
    b.ne image_bench
    brk #0
// V5's two halves set from X0, kept across a system call, read back to X0 and
// X19.
.globl image_vectors
image_vectors:
    fmov d5, x0
    mov v5.d[1], x0
    mov x8, x1
    svc #0
    fmov x0, d5
    mov x19, v5.d[1]
    brk #0
.globl image_compat
image_compat:
    brk #0
.globl restricted_image_end
restricted_image_end:

"#
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
);

/// A context at `pc` on `sp`, with nothing else set.
pub(crate) fn context(pc: u64, sp: u64) -> Context {
    Context {
        pc,
        sp,
        ..Context::default()
    }
}

/// No 32-bit mode here.
pub(crate) fn compat_context(pc: u64, sp: u64, _base: u32) -> Context {
    context(pc, sp)
}

/// The argument registers of a system call in a context, by number and words.
pub(crate) fn set_call(context: &mut Context, number: u64, words: [u64; 6]) {
    context.x[..6].copy_from_slice(&words);
    context.x[8] = number;
}

/// Put `value` in an image's first argument register.
pub(crate) const fn set_first(context: &mut Context, value: u64) {
    context.x[0] = value;
}

/// Put `value` in an image's second argument register.
pub(crate) const fn set_second(context: &mut Context, value: u64) {
    context.x[1] = value;
}

/// The result register in a context.
pub(crate) const fn result_of(context: &Context) -> u64 {
    context.x[0]
}

/// The next register [`vectors`] reads back to.
pub(crate) const fn second_result_of(context: &Context) -> u64 {
    context.x[19]
}

/// The instruction pointer in a context.
pub(crate) const fn pc_of(context: &Context) -> u64 {
    context.pc
}

/// The number a context's call holds.
pub(crate) const fn call_number_of(context: &Context) -> u64 {
    context.x[8]
}

/// Nothing of a call's clobbers on AArch64: `SVC` leaves every register.
pub(crate) const fn as_a_call_left(_: &Context, _: u64) -> bool {
    true
}

/// Past the call at the context's pc, answering `value`.
pub(crate) const fn answer_past_call(context: &mut Context, value: u64) {
    context.pc += 4;
    context.x[0] = value;
}

/// Set in a context every register a program can hold a marked value in.
pub(crate) fn mark_every_register(context: &mut Context) {
    for (register, value) in context.x[19..=28].iter_mut().zip(0xB0_u64..) {
        *register = value;
    }
}

/// Whether [`mark_every_register`]'s values are all there.
pub(crate) fn every_register_marked(context: &Context) -> bool {
    context.x[19..=28]
        .iter()
        .zip(0xB0_u64..)
        .all(|(got, want)| *got == want)
}

/// A context wrong in the `case`th way (`docs/microkernel/K1.md` §4), or
/// `None` past the last.
pub(crate) fn forged(good: &Context, case: usize) -> Option<Context> {
    let mut bad = *good;
    match case {
        0 => bad.pc = 0xFFFF_8000_0000_0000,
        1 => bad.pc = ferrix_native_abi::restricted::SHARED_BASE,
        2 => bad.sp = 0xFFFF_8000_0000_0000,
        // EL1h, the interrupt masks, the single step and illegal state, DIT,
        // PAN, SSBS and a branch type.
        3 => bad.pstate = 0x5,
        4 => bad.pstate = 1 << 4,
        5 => bad.pstate = 1 << 7,
        6 => bad.pstate = 1 << 9,
        7 => bad.pstate = 1 << 21,
        8 => bad.pstate = 1 << 20,
        9 => bad.pstate = 1 << 22,
        10 => bad.pstate = 1 << 24,
        11 => bad.pstate = 1 << 12,
        12 => bad.pstate = 1 << 10,
        13 => bad.tpidrro = 1,
        _ => return None,
    }
    Some(bad)
}

/// What [`clobber_vectors`] left, if this mode's registers are its own again.
pub(crate) fn read_vectors() -> u64 {
    let value: u64;
    // SAFETY: a read of this mode's own `v5`, as `clobber_vectors` wrote it.
    unsafe {
        core::arch::asm!(
            ".arch_extension fp",
            "fmov {value}, d5",
            value = out(reg) value,
            options(nostack, nomem),
        );
    }
    value
}

/// Set this thread's own V5 to `value`.
pub(crate) fn clobber_vectors(value: u64) {
    // SAFETY: only the register named, which the program's soft-float code never
    // holds anything in.
    unsafe {
        asm!(
            ".arch_extension fp",
            ".arch_extension simd",
            "fmov d5, {value}",
            "mov v5.d[1], {value}",
            value = in(reg) value,
        );
    }
}

/// Whether the processor has what [`vectors`] runs: every `AArch64` here.
pub(crate) const fn has_vectors() -> bool {
    true
}

/// Say how many round trips [`bench`] makes, and where it stores their ticks.
pub(crate) const fn set_bench(context: &mut Context, count: u64, at: u64) {
    context.x[20] = count;
    context.x[21] = at;
}

/// The counter [`bench`] reads.
pub(crate) fn ticks() -> u64 {
    let value;
    // SAFETY: the kernel lets EL0 read the virtual counter; nothing else is
    // touched.
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) value) };
    value
}

/// Whether this architecture has 32-bit mode for programs.
pub(crate) const HAS_COMPAT: bool = false;
/// `FPCR`'s second byte in the vector image: with [`crate::cases`]'s two added, a
/// reserved bit set.
pub(crate) const VECTOR_BAD_AT: usize = 512 + 1;

/// The value of `getppid` on this table.
pub(crate) const GETPPID: u64 = 173;
/// No i386 table here.
pub(crate) const I386_GETPID: u64 = 0;

/// A second system call just before [`sys`]'s: `svc` is four bytes.
pub(crate) fn sys_twice() -> u64 {
    sys() - 4
}
