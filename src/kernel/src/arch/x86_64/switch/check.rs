//! The switch's user state, checked with programs in ring 3: the vector-state
//! contract of the native calls that block, and the `FS` base kept in the
//! task (`docs/OPAQUE-KERNEL.md` §9.8, 3a and 3b).
//!
//! Verification, not the switch: a file of its own so that the manifest
//! counts it as the test it is. Each case is a real program, built into an
//! ELF and run by the Linux personality's loader, pinned with a second one
//! to this processor, because only a program's own registers, read in ring 3
//! after a real switch, show what the switch gave it. The programs are
//! fixtures, assembled by GNU `as` and read back out of the object file; their
//! source is in the constants' documentation.
//!
//! 3a, stage 9: a task blocks in `channel_write_read` or `object_wait_one`
//! with every vector register holding its pattern, a second program on the
//! same processor fills them with another and runs, and the first is woken
//! -- by a message, by its peer's close, and by a message with a signal
//! pending, whose handler's frame carries the post-call state -- and must read
//! back the initial state with its own `MXCSR` and x87 control word: neither
//! pattern. The same task blocked in a Linux `nanosleep`, or preempted in
//! user mode, reads back its own pattern; and one woken from
//! `channel_write_read` keeps its registers across a `nanosleep` after it,
//! because the mark is the call's. 3b: two programs with different `FS`
//! bases trade the processor; and one that loads `USER_DS` and then a null
//! selector into `FS` gets its recorded base back after a switch, while a
//! second, whose recorded base equals the one the processor last wrote, still
//! reads its own (the consultant's condition 8). The `DS`/`ES` skip (3b, the
//! consultant's S4): a program beside one that left 3, `USER_DS`, or a based
//! descriptor and then 0 in `DS` and `ES` reads its own; an i386 and a 64-bit
//! program trading the processor each read their own; and two programs with
//! null `DS` and `ES` see the skip taken.

use alloc::sync::Arc;

use ferrix_linux_abi::types::SIGUSR1;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Rights;

use super::super::{cpu, gdt};
use super::{FpuArea, UserState};
use crate::console::println;
use crate::object::Object;
use crate::object::channel::Endpoint;
use crate::object::job::KILLED_STATUS;
use crate::sched::Task;
use crate::syscall::process::{self, Process};
use crate::syscall::signal::Origin;

/// What the checks counted, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Wakes from a blocking native call that found the registers reset.
    pub(crate) reset: u32,
    /// Switches out of a Linux call, a preemption or past a woken native
    /// call that kept the registers whole.
    pub(crate) kept: u32,
    /// Times two programs traded the processor with their own `FS` bases.
    pub(crate) traded: u32,
    /// Whether the processor saves with `XSAVE`, without which the vector
    /// cases are not run: the reset is an `XRSTOR` of an empty header.
    pub(crate) xsave: bool,
    /// Whether the reset takes the fast form (`VZEROALL`), `XCR0` being x87,
    /// SSE and AVX exactly.
    pub(crate) fast: bool,
    /// Fast resets during the x87 cases that reset the x87, and that left it.
    pub(crate) x87: (u64, u64),
    /// What the `XINUSE` probe read after a busy and after a quiet program,
    /// where `XGETBV` 1 is offered.
    pub(crate) xinuse: Option<(i32, i32)>,
}

/// How long a case may take before its program is called stuck: generous
/// against a loaded host under TCG.
const PATIENCE_NANOS: u64 = 60_000_000_000;

/// The handle a fixture finds its channel under: the first a fresh process
/// is given.
const BOOTSTRAP: Handle = Handle(1);

/// The victim's pattern in every vector register: pi, as a double.
const VICTIM_PATTERN: u64 = 0x4009_21FB_5444_2D18;
/// The other program's: e.
const OTHER_PATTERN: u64 = 0x4005_BF0A_8B14_5769;
/// The victim's `MXCSR`: every exception masked, rounding toward zero.
const VICTIM_MXCSR: u32 = 0x7F80;
/// The victim's x87 control word: every exception masked, rounding toward
/// zero.
const VICTIM_CONTROL: u16 = 0x0F7F;
/// The other program's: rounding down.
const OTHER_MXCSR: u32 = 0x3F80;
/// The other program's control word: rounding down.
const OTHER_CONTROL: u16 = 0x077F;

/// The vector-state fixture. Mode `w`, `o`, `g`, `s`, `p` or `m` is the
/// victim of [`Wake`]'s cases; mode `b` fills every vector register with its
/// pattern and yields until it is killed. It writes and reads the registers
/// through an `XSAVE` area, and exits 0, or with the code [`vector_verdict`]
/// names. Patched at 4 (mode), 8 (handle), 12 (`MXCSR`), 16 (control word),
/// 24 (its pattern) and 32 (the other's).
///
/// ```text
/// # Vector-state fixture for OPAQUE-KERNEL.md 9.8 3a. Patched by the kernel:
/// #  4 mode, 8 handle, 12 MXCSR, 16 control word, 24 own pattern, 32 other's.
/// # The registers are written and read through an XSAVE area at r15+64:
/// # XMM0-15 at 160, the YMM upper halves at 576, XSTATE_BV at 512.
///         .text
///         .globl _start
/// _start:
///         jmp start
///         .byte 0x90, 0x90
/// mode:   .byte '?'
///         .byte 0, 0, 0
/// handle: .long 0
/// mxcsr:  .long 0x1F80
/// fcw:    .word 0x037F
///         .word 0
///         .long 0
/// own:    .quad 0
/// other:  .quad 0
/// nap:    .quad 0, 30000000
/// start:
///         movzbl mode(%rip), %r13d
///         movl handle(%rip), %r12d
///         subq $2048, %rsp
///         andq $-64, %rsp
///         leaq 256(%rsp), %r15
///         movl $0, (%r15)
///         movl $1, %eax
///         cpuid
///         btl $27, %ecx  # OSXSAVE
///         jnc fail
///         xorl %ecx, %ecx
///         xgetbv
///         movl %eax, %r14d  # XCR0: the components to save
///         cmpb $'g', %r13b
///         jne 2f
///         leaq handler(%rip), %rax
///         movq %rax, (%rsp)
///         movq $0x04000004, 8(%rsp)  # SA_RESTORER | SA_SIGINFO
///         leaq restorer(%rip), %rax
///         movq %rax, 16(%rsp)
///         movq $0, 24(%rsp)
///         movl $13, %eax  # rt_sigaction(SIGUSR1, act, 0, 8)
///         movl $10, %edi
///         movq %rsp, %rsi
///         xorl %edx, %edx
///         movl $8, %r10d
///         syscall
///         testq %rax, %rax
///         jnz fail
/// 2:      call set_pattern
///         cmpb $'b', %r13b
///         je pollute
///         cmpb $'s', %r13b
///         je sleeper
///         cmpb $'p', %r13b
///         je spinner
///         cmpb $'o', %r13b
///         je closer
///         movl $0x1013, %eax  # channel_write_read, sending nothing
///         movl %r12d, %edi
///         movq $-1, %rsi
///         xorl %edx, %edx
///         xorl %r10d, %r10d
///         xorl %r8d, %r8d
///         syscall
///         testq %rax, %rax
///         js fail
///         call check_reset
///         testl %eax, %eax
///         jnz exit
///         cmpb $'g', %r13b
///         jne 3f
///         movl $7, %eax
///         cmpl $1, (%r15)
///         jne exit
/// 3:      cmpb $'m', %r13b
///         jne pass
///         call set_pattern
///         call nap_once
///         call check_own
///         testl %eax, %eax
///         jz exit
///         addl $20, %eax
///         jmp exit
/// closer: movl $0x1008, %eax  # object_wait_one(h, PEER_CLOSED, 0, 0)
///         movl %r12d, %edi
///         movl $4, %esi
///         xorl %edx, %edx
///         xorl %r10d, %r10d
///         syscall
///         testq %rax, %rax
///         jnz fail
///         call check_reset
///         jmp exit
/// sleeper:
///         call nap_once
///         call check_own
///         jmp exit
/// spinner:
///         movq $100000000, %rbx
/// 4:      decq %rbx
///         jnz 4b
///         call check_own
///         jmp exit
/// pollute:
///         movl $24, %eax  # sched_yield, until killed
///         syscall
///         jmp pollute
/// pass:   xorl %eax, %eax
///         jmp exit
/// fail:   movl $10, %eax
/// exit:   movl %eax, %edi
///         movl $231, %eax
///         syscall
///         ud2
///
/// nap_once:
///         movl $35, %eax
///         leaq nap(%rip), %rdi
///         xorl %esi, %esi
///         syscall
///         testq %rax, %rax
///         jnz fail
///         ret
///
/// # Zero the area, then XSAVE every enabled component into it.
/// save:   leaq 64(%r15), %rdi
///         xorl %eax, %eax
///         movl $128, %ecx
///         rep stosq
///         movl %r14d, %eax
///         xorl %edx, %edx
///         xsave64 64(%r15)
///         ret
///
/// # Every XMM register and YMM upper half the pattern, MXCSR and the x87
/// # control word this program's, and two x87 registers the pattern.
/// set_pattern:
///         ldmxcsr mxcsr(%rip)
///         fninit
///         fldcw fcw(%rip)
///         fldl own(%rip)
///         fldl own(%rip)
///         call save
///         movq own(%rip), %rax
///         leaq 224(%r15), %rdi  # 64 + 160: XMM0
///         movl $32, %ecx
///         rep stosq
///         leaq 640(%r15), %rdi  # 64 + 576: the YMM upper halves
///         movl $32, %ecx
///         rep stosq
///         movl %r14d, %eax
///         andl $6, %eax
///         orl %eax, 576(%r15)  # 64 + 512: XSTATE_BV holds SSE and AVX
///         movl %r14d, %eax
///         xorl %edx, %edx
///         xrstor64 64(%r15)
///         ret
///
/// # The 16 bytes at (%rsi): eax 0 zero, 1 the other's pattern in either
/// # half, 2 this program's in either, 3 anything else.
/// classify:
///         movq (%rsi), %rax
///         orq 8(%rsi), %rax
///         jz 6f
///         movq other(%rip), %rax
///         cmpq %rax, (%rsi)
///         je 7f
///         cmpq %rax, 8(%rsi)
///         je 7f
///         movq own(%rip), %rax
///         cmpq %rax, (%rsi)
///         je 8f
///         cmpq %rax, 8(%rsi)
///         je 8f
///         movl $3, %eax
///         ret
/// 6:      xorl %eax, %eax
///         ret
/// 7:      movl $1, %eax
///         ret
/// 8:      movl $2, %eax
///         ret
///
/// # Walk the 32 slots of 16 bytes -- XMM0-15, then the YMM upper halves when
/// # AVX is enabled -- calling %rbx on each; eax the first nonzero answer.
/// walk:   leaq 224(%r15), %rsi
///         movl $16, %ecx
///         call 10f
///         jnz 11f
///         testl $4, %r14d
///         jz 11f
///         leaq 640(%r15), %rsi
///         movl $16, %ecx
///         call 10f
/// 11:     ret
/// 10:     pushq %rcx
///         pushq %rsi
///         call *%rbx
///         popq %rsi
///         popq %rcx
///         testl %eax, %eax
///         jnz 12f
///         addq $16, %rsi
///         decl %ecx
///         jnz 10b
///         testl %eax, %eax
/// 12:     ret
///
/// # The reset state: every slot zero, MXCSR and the control word this
/// # program's, every x87 register empty. eax 0, or 1 to 6.
/// check_reset:
///         call save
///         leaq classify(%rip), %rbx
///         call walk
///         jnz 9f
///         call controls
///         testl %eax, %eax
///         jnz 9f
///         fnstenv 16(%r15)
///         fldcw fcw(%rip)
///         movzwl 24(%r15), %eax  # the tag word: all empty
///         cmpl $0xFFFF, %eax
///         movl $6, %eax
///         jne 9f
///         xorl %eax, %eax
/// 9:      ret
///
/// # MXCSR and the control word this program's: eax 0, 4 or 5.
/// controls:
///         stmxcsr 48(%r15)
///         movl 48(%r15), %eax
///         cmpl mxcsr(%rip), %eax
///         movl $4, %eax
///         jne 13f
///         fnstcw 48(%r15)
///         movzwl 48(%r15), %eax
///         cmpw fcw(%rip), %ax
///         movl $5, %eax
///         jne 13f
///         xorl %eax, %eax
/// 13:     ret
///
/// # This program's own state kept: every slot its pattern exactly, MXCSR and
/// # the control word its own, st(0) its pattern. eax 0, or 11 to 16.
/// check_own:
///         call save
///         leaq own_exactly(%rip), %rbx
///         call walk
///         jnz 14f
///         call controls
///         testl %eax, %eax
///         jz 15f
///         addl $10, %eax
///         jmp 14f
/// 15:     fstpl 16(%r15)
///         movq 16(%r15), %rax
///         cmpq own(%rip), %rax
///         movl $16, %eax
///         jne 14f
///         xorl %eax, %eax
/// 14:     ret
///
/// own_exactly:
///         movq own(%rip), %rax
///         cmpq %rax, (%rsi)
///         jne 16f
///         cmpq %rax, 8(%rsi)
///         jne 16f
///         xorl %eax, %eax
///         ret
/// 16:     call classify
///         addl $11, %eax  # 0 zero -> 11 ... remapped below
///         cmpl $12, %eax
///         je 17f  # the other's: 11
///         cmpl $11, %eax
///         je 18f  # zero: 12
///         movl $13, %eax
///         ret
/// 17:     movl $11, %eax
///         ret
/// 18:     movl $12, %eax
///         ret
///
/// # SIGUSR1: note that it ran, in the word the interrupted r15 names
/// # (uc_mcontext's r15, at 96 in the ucontext rdx points to).
/// handler:
///         movq 96(%rdx), %rax
///         movl $1, (%rax)
///         ret
/// restorer:
///         movl $15, %eax
///         syscall
///         ud2
/// ```
const VECTOR_PROGRAM: &[u8] = &[
    0xeb, 0x36, 0x90, 0x90, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x1f, 0x00, 0x00,
    0x7f, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x80, 0xc3, 0xc9, 0x01, 0x00, 0x00, 0x00, 0x00, 0x44, 0x0f, 0xb6, 0x2d, 0xc4, 0xff, 0xff, 0xff,
    0x44, 0x8b, 0x25, 0xc1, 0xff, 0xff, 0xff, 0x48, 0x81, 0xec, 0x00, 0x08, 0x00, 0x00, 0x48, 0x83,
    0xe4, 0xc0, 0x4c, 0x8d, 0xbc, 0x24, 0x00, 0x01, 0x00, 0x00, 0x41, 0xc7, 0x07, 0x00, 0x00, 0x00,
    0x00, 0xb8, 0x01, 0x00, 0x00, 0x00, 0x0f, 0xa2, 0x0f, 0xba, 0xe1, 0x1b, 0x0f, 0x83, 0x2a, 0x01,
    0x00, 0x00, 0x31, 0xc9, 0x0f, 0x01, 0xd0, 0x41, 0x89, 0xc6, 0x41, 0x80, 0xfd, 0x67, 0x75, 0x49,
    0x48, 0x8d, 0x05, 0x13, 0x03, 0x00, 0x00, 0x48, 0x89, 0x04, 0x24, 0x48, 0xc7, 0x44, 0x24, 0x08,
    0x04, 0x00, 0x00, 0x04, 0x48, 0x8d, 0x05, 0x0a, 0x03, 0x00, 0x00, 0x48, 0x89, 0x44, 0x24, 0x10,
    0x48, 0xc7, 0x44, 0x24, 0x18, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x0d, 0x00, 0x00, 0x00, 0xbf, 0x0a,
    0x00, 0x00, 0x00, 0x48, 0x89, 0xe6, 0x31, 0xd2, 0x41, 0xba, 0x08, 0x00, 0x00, 0x00, 0x0f, 0x05,
    0x48, 0x85, 0xc0, 0x0f, 0x85, 0xd3, 0x00, 0x00, 0x00, 0xe8, 0x0d, 0x01, 0x00, 0x00, 0x41, 0x80,
    0xfd, 0x62, 0x0f, 0x84, 0xb7, 0x00, 0x00, 0x00, 0x41, 0x80, 0xfd, 0x73, 0x0f, 0x84, 0x8e, 0x00,
    0x00, 0x00, 0x41, 0x80, 0xfd, 0x70, 0x0f, 0x84, 0x90, 0x00, 0x00, 0x00, 0x41, 0x80, 0xfd, 0x6f,
    0x74, 0x5e, 0xb8, 0x13, 0x10, 0x00, 0x00, 0x44, 0x89, 0xe7, 0x48, 0xc7, 0xc6, 0xff, 0xff, 0xff,
    0xff, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x45, 0x31, 0xc0, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x88,
    0x88, 0x00, 0x00, 0x00, 0xe8, 0xa4, 0x01, 0x00, 0x00, 0x85, 0xc0, 0x0f, 0x85, 0x80, 0x00, 0x00,
    0x00, 0x41, 0x80, 0xfd, 0x67, 0x75, 0x0b, 0xb8, 0x07, 0x00, 0x00, 0x00, 0x41, 0x83, 0x3f, 0x01,
    0x75, 0x6f, 0x41, 0x80, 0xfd, 0x6d, 0x75, 0x60, 0xe8, 0x9e, 0x00, 0x00, 0x00, 0xe8, 0x6a, 0x00,
    0x00, 0x00, 0xe8, 0xe0, 0x01, 0x00, 0x00, 0x85, 0xc0, 0x74, 0x56, 0x83, 0xc0, 0x14, 0xeb, 0x51,
    0xb8, 0x08, 0x10, 0x00, 0x00, 0x44, 0x89, 0xe7, 0xbe, 0x04, 0x00, 0x00, 0x00, 0x31, 0xd2, 0x45,
    0x31, 0xd2, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x33, 0xe8, 0x4f, 0x01, 0x00, 0x00, 0xeb, 0x31,
    0xe8, 0x37, 0x00, 0x00, 0x00, 0xe8, 0xad, 0x01, 0x00, 0x00, 0xeb, 0x25, 0x48, 0xc7, 0xc3, 0x00,
    0xe1, 0xf5, 0x05, 0x48, 0xff, 0xcb, 0x75, 0xfb, 0xe8, 0x9a, 0x01, 0x00, 0x00, 0xeb, 0x12, 0xb8,
    0x18, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb, 0xf7, 0x31, 0xc0, 0xeb, 0x05, 0xb8, 0x0a, 0x00, 0x00,
    0x00, 0x89, 0xc7, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b, 0xb8, 0x23, 0x00, 0x00,
    0x00, 0x48, 0x8d, 0x3d, 0x70, 0xfe, 0xff, 0xff, 0x31, 0xf6, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75,
    0xdb, 0xc3, 0x49, 0x8d, 0x7f, 0x40, 0x31, 0xc0, 0xb9, 0x80, 0x00, 0x00, 0x00, 0xf3, 0x48, 0xab,
    0x44, 0x89, 0xf0, 0x31, 0xd2, 0x49, 0x0f, 0xae, 0x67, 0x40, 0xc3, 0x0f, 0xae, 0x15, 0x2a, 0xfe,
    0xff, 0xff, 0xdb, 0xe3, 0xd9, 0x2d, 0x26, 0xfe, 0xff, 0xff, 0xdd, 0x05, 0x28, 0xfe, 0xff, 0xff,
    0xdd, 0x05, 0x22, 0xfe, 0xff, 0xff, 0xe8, 0xc7, 0xff, 0xff, 0xff, 0x48, 0x8b, 0x05, 0x16, 0xfe,
    0xff, 0xff, 0x49, 0x8d, 0xbf, 0xe0, 0x00, 0x00, 0x00, 0xb9, 0x20, 0x00, 0x00, 0x00, 0xf3, 0x48,
    0xab, 0x49, 0x8d, 0xbf, 0x80, 0x02, 0x00, 0x00, 0xb9, 0x20, 0x00, 0x00, 0x00, 0xf3, 0x48, 0xab,
    0x44, 0x89, 0xf0, 0x83, 0xe0, 0x06, 0x41, 0x09, 0x87, 0x40, 0x02, 0x00, 0x00, 0x44, 0x89, 0xf0,
    0x31, 0xd2, 0x49, 0x0f, 0xae, 0x6f, 0x40, 0xc3, 0x48, 0x8b, 0x06, 0x48, 0x0b, 0x46, 0x08, 0x74,
    0x2a, 0x48, 0x8b, 0x05, 0xd8, 0xfd, 0xff, 0xff, 0x48, 0x39, 0x06, 0x74, 0x21, 0x48, 0x39, 0x46,
    0x08, 0x74, 0x1b, 0x48, 0x8b, 0x05, 0xbe, 0xfd, 0xff, 0xff, 0x48, 0x39, 0x06, 0x74, 0x15, 0x48,
    0x39, 0x46, 0x08, 0x74, 0x0f, 0xb8, 0x03, 0x00, 0x00, 0x00, 0xc3, 0x31, 0xc0, 0xc3, 0xb8, 0x01,
    0x00, 0x00, 0x00, 0xc3, 0xb8, 0x02, 0x00, 0x00, 0x00, 0xc3, 0x49, 0x8d, 0xb7, 0xe0, 0x00, 0x00,
    0x00, 0xb9, 0x10, 0x00, 0x00, 0x00, 0xe8, 0x1d, 0x00, 0x00, 0x00, 0x75, 0x1a, 0x41, 0xf7, 0xc6,
    0x04, 0x00, 0x00, 0x00, 0x74, 0x11, 0x49, 0x8d, 0xb7, 0x80, 0x02, 0x00, 0x00, 0xb9, 0x10, 0x00,
    0x00, 0x00, 0xe8, 0x01, 0x00, 0x00, 0x00, 0xc3, 0x51, 0x56, 0xff, 0xd3, 0x5e, 0x59, 0x85, 0xc0,
    0x75, 0x0a, 0x48, 0x83, 0xc6, 0x10, 0xff, 0xc9, 0x75, 0xee, 0x85, 0xc0, 0xc3, 0xe8, 0x00, 0xff,
    0xff, 0xff, 0x48, 0x8d, 0x1d, 0x6f, 0xff, 0xff, 0xff, 0xe8, 0xac, 0xff, 0xff, 0xff, 0x75, 0x26,
    0xe8, 0x22, 0x00, 0x00, 0x00, 0x85, 0xc0, 0x75, 0x1d, 0x41, 0xd9, 0x77, 0x10, 0xd9, 0x2d, 0x2d,
    0xfd, 0xff, 0xff, 0x41, 0x0f, 0xb7, 0x47, 0x18, 0x3d, 0xff, 0xff, 0x00, 0x00, 0xb8, 0x06, 0x00,
    0x00, 0x00, 0x75, 0x02, 0x31, 0xc0, 0xc3, 0x41, 0x0f, 0xae, 0x5f, 0x30, 0x41, 0x8b, 0x47, 0x30,
    0x3b, 0x05, 0x06, 0xfd, 0xff, 0xff, 0xb8, 0x04, 0x00, 0x00, 0x00, 0x75, 0x19, 0x41, 0xd9, 0x7f,
    0x30, 0x41, 0x0f, 0xb7, 0x47, 0x30, 0x66, 0x3b, 0x05, 0xf3, 0xfc, 0xff, 0xff, 0xb8, 0x05, 0x00,
    0x00, 0x00, 0x75, 0x02, 0x31, 0xc0, 0xc3, 0xe8, 0x96, 0xfe, 0xff, 0xff, 0x48, 0x8d, 0x1d, 0x2e,
    0x00, 0x00, 0x00, 0xe8, 0x42, 0xff, 0xff, 0xff, 0x75, 0x26, 0xe8, 0xb8, 0xff, 0xff, 0xff, 0x85,
    0xc0, 0x74, 0x05, 0x83, 0xc0, 0x0a, 0xeb, 0x18, 0x41, 0xdd, 0x5f, 0x10, 0x49, 0x8b, 0x47, 0x10,
    0x48, 0x3b, 0x05, 0xc1, 0xfc, 0xff, 0xff, 0xb8, 0x10, 0x00, 0x00, 0x00, 0x75, 0x02, 0x31, 0xc0,
    0xc3, 0x48, 0x8b, 0x05, 0xb0, 0xfc, 0xff, 0xff, 0x48, 0x39, 0x06, 0x75, 0x09, 0x48, 0x39, 0x46,
    0x08, 0x75, 0x03, 0x31, 0xc0, 0xc3, 0xe8, 0xbd, 0xfe, 0xff, 0xff, 0x83, 0xc0, 0x0b, 0x83, 0xf8,
    0x0c, 0x74, 0x0b, 0x83, 0xf8, 0x0b, 0x74, 0x0c, 0xb8, 0x0d, 0x00, 0x00, 0x00, 0xc3, 0xb8, 0x0b,
    0x00, 0x00, 0x00, 0xc3, 0xb8, 0x0c, 0x00, 0x00, 0x00, 0xc3, 0x48, 0x8b, 0x42, 0x60, 0xc7, 0x00,
    0x01, 0x00, 0x00, 0x00, 0xc3, 0xb8, 0x0f, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// The `FS`-base fixture. Mode `t` maps its word at its base, sets the base
/// with `arch_prctl` and reads `%fs:0` between yields; mode `c` also loads
/// `USER_DS` and a null selector into `FS` before each yield, then once more
/// before a `nanosleep`, and reads its word after it. Exits 0, 1 to 3 for a
/// wrong word, 8 or 9 for a failed call; a read of an unmapped base ends it
/// with `SIGSEGV`. Patched at 4 (mode), 8 (base), 16 (word), 24 (`USER_DS`)
/// and 28 (rounds).
///
/// ```text
/// # FS-base fixture for OPAQUE-KERNEL.md 9.8 3b. Layout patched by the kernel:
/// #  4 mode, 8 base, 16 magic, 24 USER_DS selector, 28 rounds, 32 timespec
///         .text
///         .globl _start
/// _start:
///         jmp start
///         .byte 0x90, 0x90
/// mode:   .byte '?'
///         .byte 0, 0, 0
/// base:   .quad 0
/// magic:  .quad 0
/// userds: .word 0
///         .word 0
/// rounds: .long 0
/// nap:    .quad 0, 5000000
/// start:
///         movzbl mode(%rip), %r13d
///         movq base(%rip), %rdi
///         movl $4096, %esi
///         movl $3, %edx
///         movl $0x32, %r10d
///         movq $-1, %r8
///         xorl %r9d, %r9d
///         movl $9, %eax
///         syscall
///         cmpq base(%rip), %rax
///         movl $9, %edi
///         jne exit
///         movq magic(%rip), %rcx
///         movq %rcx, (%rax)
///         call set_fs
///         movl rounds(%rip), %ebx
/// 1:
///         cmpb $'c', %r13b
///         jne 2f
///         call set_fs
///         movl $2, %edi
///         call read_fs
///         call drop_fs
///         jmp 3f
/// 2:
///         movl $1, %edi
///         call read_fs
/// 3:
///         movl $24, %eax
///         syscall
///         decl %ebx
///         jnz 1b
///         cmpb $'c', %r13b
///         jne pass
///         // Linux's rule: a base cleared by a null selector comes back as the
///         // recorded one once the program has been switched out and in.
///         call set_fs
///         call drop_fs
///         movl $35, %eax
///         leaq nap(%rip), %rdi
///         xorl %esi, %esi
///         syscall
///         movl $3, %edi
///         call read_fs
/// pass:
///         xorl %edi, %edi
/// exit:
///         movl $231, %eax
///         syscall
///         ud2
///
/// # %fs:0 must be the magic word; else exit with %edi.
/// read_fs:
///         movq %fs:0, %rax
///         cmpq magic(%rip), %rax
///         jne exit
///         ret
/// # arch_prctl(ARCH_SET_FS, base)
/// set_fs:
///         movl $158, %eax
///         movl $0x1002, %edi
///         movq base(%rip), %rsi
///         syscall
///         testq %rax, %rax
///         movl $8, %edi
///         jnz exit
///         ret
/// # USER_DS into FS, then a null selector: the base is the descriptor's or
/// # cleared, by vendor, and no selector read shows which.
/// drop_fs:
///         movw userds(%rip), %ax
///         movw %ax, %fs
///         xorl %eax, %eax
///         movw %ax, %fs
///         ret
/// ```
const FS_PROGRAM: &[u8] = &[
    0xeb, 0x2e, 0x90, 0x90, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x4b, 0x4c, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x44, 0x0f, 0xb6, 0x2d, 0xcc, 0xff, 0xff, 0xff, 0x48, 0x8b, 0x3d, 0xc9, 0xff, 0xff, 0xff, 0xbe,
    0x00, 0x10, 0x00, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x32, 0x00, 0x00, 0x00, 0x49,
    0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9, 0xb8, 0x09, 0x00, 0x00, 0x00, 0x0f, 0x05,
    0x48, 0x3b, 0x05, 0xa1, 0xff, 0xff, 0xff, 0xbf, 0x09, 0x00, 0x00, 0x00, 0x75, 0x72, 0x48, 0x8b,
    0x0d, 0x9b, 0xff, 0xff, 0xff, 0x48, 0x89, 0x08, 0xe8, 0x7f, 0x00, 0x00, 0x00, 0x8b, 0x1d, 0x99,
    0xff, 0xff, 0xff, 0x41, 0x80, 0xfd, 0x63, 0x75, 0x16, 0xe8, 0x6e, 0x00, 0x00, 0x00, 0xbf, 0x02,
    0x00, 0x00, 0x00, 0xe8, 0x51, 0x00, 0x00, 0x00, 0xe8, 0x7d, 0x00, 0x00, 0x00, 0xeb, 0x0a, 0xbf,
    0x01, 0x00, 0x00, 0x00, 0xe8, 0x40, 0x00, 0x00, 0x00, 0xb8, 0x18, 0x00, 0x00, 0x00, 0x0f, 0x05,
    0xff, 0xcb, 0x75, 0xcf, 0x41, 0x80, 0xfd, 0x63, 0x75, 0x24, 0xe8, 0x3d, 0x00, 0x00, 0x00, 0xe8,
    0x56, 0x00, 0x00, 0x00, 0xb8, 0x23, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x3d, 0x50, 0xff, 0xff, 0xff,
    0x31, 0xf6, 0x0f, 0x05, 0xbf, 0x03, 0x00, 0x00, 0x00, 0xe8, 0x0b, 0x00, 0x00, 0x00, 0x31, 0xff,
    0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b, 0x64, 0x48, 0x8b, 0x04, 0x25, 0x00, 0x00,
    0x00, 0x00, 0x48, 0x3b, 0x05, 0x17, 0xff, 0xff, 0xff, 0x75, 0xe5, 0xc3, 0xb8, 0x9e, 0x00, 0x00,
    0x00, 0xbf, 0x02, 0x10, 0x00, 0x00, 0x48, 0x8b, 0x35, 0xfb, 0xfe, 0xff, 0xff, 0x0f, 0x05, 0x48,
    0x85, 0xc0, 0xbf, 0x08, 0x00, 0x00, 0x00, 0x75, 0xc7, 0xc3, 0x66, 0x8b, 0x05, 0xf7, 0xfe, 0xff,
    0xff, 0x8e, 0xe0, 0x31, 0xc0, 0x8e, 0xe0, 0xc3,
];

/// Run every check of the switch's user state.
///
/// # Errors
///
/// The first case that read what it must not, with what it read.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    check_the_three_numbers()?;
    check_unsaved_reads_as_initial()?;
    check_pkru_is_never_reset()?;
    report.xsave = cpu::extended_state_components() != 0;
    if report.xsave {
        for wake in [Wake::Message, Wake::Close, Wake::Signal] {
            run_vector_case(wake)?;
            report.reset += 1;
        }
        for wake in [Wake::LinuxSleep, Wake::Preempted, Wake::MarkLowered] {
            run_vector_case(wake)?;
            report.kept += 1;
        }
        // The fast reset's two x87 branches, each seen taken (the
        // consultant's V5): beside a program that used the x87, and beside
        // one that left it initial, which only `XINUSE` can tell.
        let fast = cpu::reset_components() == cpu::XSTATE_X87_SSE | cpu::XSTATE_AVX;
        let before = super::x87_resets();
        run_vector_case(Wake::Message)?;
        let after = super::x87_resets();
        if fast && after.0 <= before.0 {
            return Err("a wake beside a program that used the x87 did not reset the x87");
        }
        run_vector_case_beside(Wake::Message, false)?;
        let last = super::x87_resets();
        if fast && cpu::xinuse_readable() && last.1 <= after.1 {
            return Err(
                "a wake beside a program that left the x87 initial reset it: XINUSE was not read",
            );
        }
        report.reset += 2;
        report.x87 = (last.0 - before.0, last.1 - before.1);
        report.fast = fast;
        report.xinuse = check_xinuse_tells_nothing()?;
        for wake in [Wake::Message, Wake::Signal] {
            run_sending_vector_case(wake)?;
            report.reset += 1;
        }
    }
    report.traded = check_programs_trade_their_fs_bases()?;
    check_a_cleared_base_comes_back_and_leaks_nowhere()?;
    Ok(report)
}

/// The entry raises the mark for exactly the three native calls that block,
/// by number: none of the rest of the native range, and no Linux number.
///
/// Verifies: `L.x86_64.152`
fn check_the_three_numbers() -> Result<(), &'static str> {
    use ferrix_native_abi::nr::{CHANNEL_WRITE_READ, FIRST, LAST, OBJECT_WAIT_ONE, PORT_WAIT};

    use super::super::syscall::vectors_die_in;

    let marked = (FIRST..=LAST)
        .chain(0..1024)
        .filter(|&number| vectors_die_in(number))
        .count();
    if marked != 3
        || !vectors_die_in(CHANNEL_WRITE_READ)
        || !vectors_die_in(OBJECT_WAIT_ONE)
        || !vectors_die_in(PORT_WAIT)
    {
        return Err(
            "the vector-state contract does not cover exactly the three native calls that block",
        );
    }
    Ok(())
}

/// A state saved only in part reads, through the one accessor, as the
/// initial state with its own `MXCSR` and control word, never the bytes its
/// area still holds; and a writer turns it into that state before it writes.
///
/// Verifies: `L.x86_64.156`
fn check_unsaved_reads_as_initial() -> Result<(), &'static str> {
    let mut state = UserState::new();
    state.fxsave_mut().fill(0xA5);
    state.avx_mut().fill(0x5A);
    state.set_xstate_bv(u64::MAX);
    state.keep_vector_controls(VICTIM_MXCSR, VICTIM_CONTROL);
    let initial = FpuArea::initial(VICTIM_MXCSR, VICTIM_CONTROL);
    if !state.is_unsaved()
        || state.fxsave() != initial.legacy
        || state.avx() != [0; 256]
        || state.xstate_bv() != cpu::XSTATE_X87_SSE
    {
        return Err(
            "a vector state saved only in part did not read as the initial state with its own MXCSR and control word",
        );
    }
    let _ = state.fxsave_mut();
    if state.is_unsaved() || state.fxsave() != initial.legacy || state.avx() != [0; 256] {
        return Err(
            "a writer of a vector state saved only in part did not start from the state it reads as",
        );
    }
    Ok(())
}

/// `PKRU` is never among the components the reset initialises, and nothing
/// past x87, SSE and AVX is enabled for it to reset without a review.
///
/// Verifies: `L.x86_64.157`
fn check_pkru_is_never_reset() -> Result<(), &'static str> {
    if cpu::reset_components() & cpu::XSTATE_PKRU != 0 {
        return Err("the vector reset would initialise PKRU, widening a program's own protection");
    }
    if cpu::extended_state_components() & !(cpu::XSTATE_X87_SSE | cpu::XSTATE_AVX) != 0 {
        return Err(
            "a state component past x87, SSE and AVX is enabled, which the vector reset has not been reviewed for",
        );
    }
    Ok(())
}

/// How a vector case's program is let go of the processor and brought back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wake {
    /// Blocked in `channel_write_read`, woken by a message.
    Message,
    /// Blocked in `object_wait_one`, woken by its peer's close.
    Close,
    /// Blocked in `channel_write_read` with `SIGUSR1` sent while it waits,
    /// woken by a message: the handler's frame carries the post-call state.
    Signal,
    /// Blocked in a Linux `nanosleep`.
    LinuxSleep,
    /// Preempted in user mode by the timer.
    Preempted,
    /// Woken from `channel_write_read` by a message, then blocked in a Linux
    /// `nanosleep`.
    MarkLowered,
}

impl Wake {
    /// The fixture's mode byte.
    const fn mode(self) -> u8 {
        match self {
            Wake::Message => b'w',
            Wake::Close => b'o',
            Wake::Signal => b'g',
            Wake::LinuxSleep => b's',
            Wake::Preempted => b'p',
            Wake::MarkLowered => b'm',
        }
    }

    /// Whether the program blocks in a native call the check must see it
    /// blocked in before the other program fills the registers.
    const fn blocks_natively(self) -> bool {
        matches!(
            self,
            Wake::Message | Wake::Close | Wake::Signal | Wake::MarkLowered
        )
    }
}

/// One vector case: the victim pinned here with its pattern, the other
/// program pinned beside it with its own, the wake, and the victim's status.
///
/// Verifies: H.SCHED.12, L.sched.54, `L.x86_64.153`, `L.x86_64.154`, `L.x86_64.155`
fn run_vector_case(wake: Wake) -> Result<(), &'static str> {
    run_vector_case_beside(wake, true)
}

/// [`run_vector_case`], the other program using the x87 or, with
/// `other_x87` false, never touching it, so that the victim's reset meets
/// an x87 in its initial state.
fn run_vector_case_beside(wake: Wake, other_x87: bool) -> Result<(), &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let (mine, theirs) = Endpoint::pair().map_err(|_| "could not make a channel")?;
    let victim = vector_program(wake.mode(), VICTIM_PATTERN, VICTIM_MXCSR, VICTIM_CONTROL)?;
    let placed = victim
        .with_handles(|table| table.insert(Object::Channel(mine), Rights::CHANNEL))
        .map_err(|_| "no room for the vector program's channel")?;
    if placed != BOOTSTRAP {
        return Err(
            "a fresh process's first handle is not the one the vector program was built for",
        );
    }
    let victim_task = process::start_on(&victim, Some(cpu))
        .map_err(|_| "the vector program could not be started")?;
    if wake.blocks_natively() {
        wait_until(
            deadline,
            "the vector program never blocked in its native call",
            || victim_task.is_blocked(),
        )?;
    }
    let other = if other_x87 {
        vector_program(b'b', OTHER_PATTERN, OTHER_MXCSR, OTHER_CONTROL)?
    } else {
        vector_program_without_x87(OTHER_PATTERN, OTHER_MXCSR, OTHER_CONTROL)?
    };
    let other_task = process::start_on(&other, Some(cpu))
        .map_err(|_| "the program that fills the vector registers could not be started")?;
    if wake.blocks_natively() {
        // Switched to twice: it has set its pattern and yielded at least
        // once since the victim blocked.
        wait_until(
            deadline,
            "the program that fills the vector registers never ran",
            || other_task.switches() >= 2,
        )?;
    }
    let mut theirs = Some(theirs);
    match wake {
        Wake::Message | Wake::MarkLowered => send(theirs.as_deref())?,
        Wake::Close => theirs = None,
        Wake::Signal => {
            crate::syscall::kill::send(&victim, SIGUSR1, Origin::Kernel);
            crate::sched::sleep_for(5_000_000);
            send(theirs.as_deref())?;
        }
        Wake::LinuxSleep | Wake::Preempted => {}
    }
    let status = victim.wait_for_exit(deadline);
    process::kill(&other, KILLED_STATUS);
    let _ = other.wait_for_exit(deadline);
    drop(theirs);
    if wake == Wake::Preempted && victim_task.preemptions() == 0 {
        return Err("the vector program was never preempted while it spun beside another");
    }
    if matches!(wake, Wake::LinuxSleep | Wake::MarkLowered) && other_task.switches() < 2 {
        return Err("the program that fills the vector registers never ran while the other slept");
    }
    vector_verdict(status)
}

/// Send the message that wakes a victim blocked in `channel_write_read`.
fn send(end: Option<&Endpoint>) -> Result<(), &'static str> {
    end.ok_or("the check's end of the channel is gone")?
        .write_small(b"vectors!")
        .map_err(|_| "the check could not write the message that wakes the vector program")
}

/// What a vector program's status says.
fn vector_verdict(status: Option<i32>) -> Result<(), &'static str> {
    match status {
        Some(0) => Ok(()),
        Some(1) => Err(
            "a task resumed from a blocking native call read another program's vector registers",
        ),
        Some(2) => Err(
            "a task resumed from a blocking native call kept its own vector registers: the reset did not run",
        ),
        Some(3) => Err(
            "a task resumed from a blocking native call read vector registers that are neither reset nor any program's pattern",
        ),
        Some(4) => Err("a task resumed from a blocking native call did not get its own MXCSR back"),
        Some(5) => Err(
            "a task resumed from a blocking native call did not get its own x87 control word back",
        ),
        Some(6) => Err("a task resumed from a blocking native call found x87 registers in use"),
        Some(7) => {
            Err("the vector program's signal handler did not run on the way out of its call")
        }
        Some(10) => Err("a call of the vector program failed"),
        Some(11) => Err(
            "a task blocked in a Linux call or preempted read another program's vector registers",
        ),
        Some(12) => {
            Err("a task blocked in a Linux call or preempted lost its vector registers to a reset")
        }
        Some(13..=16) => {
            Err("a task blocked in a Linux call or preempted did not get its own vector state back")
        }
        Some(31) => Err(
            "the mark outlived its call: a Linux sleep after a blocking native call let another program's vector registers in",
        ),
        Some(32) => Err(
            "the mark outlived its call: a Linux sleep after a blocking native call lost the vector registers to a reset",
        ),
        Some(33..=36) => Err(
            "the mark outlived its call: a Linux sleep after a blocking native call did not keep the vector state",
        ),
        Some(status) => {
            println!("  vectors  the vector program ended with {status}");
            Err("the vector program ended with a status it has no meaning for")
        }
        None => Err("the vector program never ended"),
    }
}

/// Wait, sleeping a millisecond at a time, until `done` or the deadline.
fn wait_until(
    deadline: u64,
    stuck: &'static str,
    done: impl Fn() -> bool,
) -> Result<(), &'static str> {
    while !done() {
        if crate::timer::now_nanos() >= deadline {
            return Err(stuck);
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// A copy of [`VECTOR_PROGRAM`] in `mode`, with `pattern`, `mxcsr` and
/// `control` as its own, loaded and not started.
fn vector_program(
    mode: u8,
    pattern: u64,
    mxcsr: u32,
    control: u16,
) -> Result<Arc<Process>, &'static str> {
    let mut code = VECTOR_PROGRAM.to_vec();
    if code.get(4) != Some(&b'?') {
        return Err("the vector program's mode byte is not where its layout says");
    }
    let other = if pattern == VICTIM_PATTERN {
        OTHER_PATTERN
    } else {
        VICTIM_PATTERN
    };
    patch(&mut code, 4, &[mode])?;
    patch(&mut code, 8, &BOOTSTRAP.0.to_le_bytes())?;
    patch(&mut code, 12, &mxcsr.to_le_bytes())?;
    patch(&mut code, 16, &control.to_le_bytes())?;
    patch(&mut code, 24, &pattern.to_le_bytes())?;
    patch(&mut code, 32, &other.to_le_bytes())?;
    load(b"/vectors", &code)
}

/// Where [`VECTOR_PROGRAM`]'s `set_pattern` loads the x87 (`fninit`,
/// `fldcw`, two `fldl`): twenty bytes, which [`vector_program_without_x87`]
/// turns into `nop`s.
const X87_LOADS_AT: usize = 0x1E2;
/// Their first four bytes, checked before they are overwritten.
const X87_LOADS: [u8; 4] = [0xDB, 0xE3, 0xD9, 0x2D];

/// [`vector_program`] in mode `b`, never touching the x87: its
/// `set_pattern` fills `XMM` and `YMM` and leaves the x87 as `execve` left
/// it, initial, which `XSAVE` then records as such.
fn vector_program_without_x87(
    pattern: u64,
    mxcsr: u32,
    control: u16,
) -> Result<Arc<Process>, &'static str> {
    let mut code = VECTOR_PROGRAM.to_vec();
    if code.get(X87_LOADS_AT..X87_LOADS_AT + 4) != Some(&X87_LOADS[..]) {
        return Err("the vector program's x87 loads are not where its layout says");
    }
    patch(&mut code, X87_LOADS_AT, &[0x90; 20])?;
    patch(&mut code, 4, b"b")?;
    patch(&mut code, 8, &BOOTSTRAP.0.to_le_bytes())?;
    patch(&mut code, 12, &mxcsr.to_le_bytes())?;
    patch(&mut code, 16, &control.to_le_bytes())?;
    patch(&mut code, 24, &pattern.to_le_bytes())?;
    patch(&mut code, 32, &VICTIM_PATTERN.to_le_bytes())?;
    load(b"/vectors", &code)
}

/// The `XINUSE` probe: blocks in `channel_write_read` sending nothing, and
/// once woken exits 40 plus `XINUSE`'s x87, SSE and AVX bits (`XGETBV` 1),
/// or 10. Patched at 8 (handle).
///
/// ```text
/// # XINUSE probe for OPAQUE-KERNEL.md 9.10 (po7-ipcM). Patched by the kernel:
/// #  8 handle. Blocks in channel_write_read sending nothing; once woken,
/// #  exits 40 plus XINUSE's x87, SSE and AVX bits (XGETBV 1), or 10.
///         .text
///         .globl _start
/// _start:
///         jmp start
///         .byte 0x90, 0x90
/// mode:   .byte '?'
///         .byte 0, 0, 0
/// handle: .long 0
/// start:
///         movl $0x1013, %eax  # channel_write_read, sending nothing
///         movl handle(%rip), %edi
///         movq $-1, %rsi
///         xorl %edx, %edx
///         xorl %r10d, %r10d
///         xorl %r8d, %r8d
///         syscall
///         testq %rax, %rax
///         js fail
///         movl $1, %ecx
///         xgetbv
///         andl $7, %eax
///         addl $40, %eax
///         jmp exit
/// fail:   movl $10, %eax
/// exit:   movl %eax, %edi
///         movl $231, %eax
///         syscall
///         ud2
/// ```
const PROBE_PROGRAM: &[u8] = &[
    0xeb, 0x0a, 0x90, 0x90, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x13, 0x10, 0x00,
    0x00, 0x8b, 0x3d, 0xf1, 0xff, 0xff, 0xff, 0x48, 0xc7, 0xc6, 0xff, 0xff, 0xff, 0xff, 0x31, 0xd2,
    0x45, 0x31, 0xd2, 0x45, 0x31, 0xc0, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x78, 0x10, 0xb9, 0x01, 0x00,
    0x00, 0x00, 0x0f, 0x01, 0xd0, 0x83, 0xe0, 0x07, 0x83, 0xc0, 0x28, 0xeb, 0x05, 0xb8, 0x0a, 0x00,
    0x00, 0x00, 0x89, 0xc7, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// A program that touches no vector register: `sched_yield` until killed.
///
/// ```text
///         .text
///         .globl _start
/// _start:
///         movl $24, %eax
///         syscall
///         jmp _start
/// ```
const QUIET_PROGRAM: &[u8] = &[0xb8, 0x18, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb, 0xf7];

/// What a program woken from a blocking native call reads in `XINUSE` must
/// not depend on what the program that ran before it did (the consultant's
/// V4): the probe is woken once after a program that filled every vector
/// register and the x87, and once after one that touched none, and both
/// read the same bits. Answers them. Only where `XGETBV` 1 is offered.
///
/// Verifies: `L.x86_64.155`
fn check_xinuse_tells_nothing() -> Result<Option<(i32, i32)>, &'static str> {
    if !cpu::xinuse_readable() {
        return Ok(None);
    }
    let after_busy = run_probe(true)?;
    let after_quiet = run_probe(false)?;
    if !(40..48).contains(&after_busy) || !(40..48).contains(&after_quiet) {
        println!("  vectors  the XINUSE probe ended with {after_busy} and {after_quiet}");
        return Err("the XINUSE probe did not read XINUSE after its wake");
    }
    if after_busy != after_quiet {
        println!(
            "  vectors  XINUSE after a busy program {:#x}, after a quiet one {:#x}",
            after_busy - 40,
            after_quiet - 40
        );
        return Err(
            "a task resumed from a blocking native call read in XINUSE whether the program before it used the vector registers",
        );
    }
    Ok(Some((after_busy - 40, after_quiet - 40)))
}

/// One run of the probe, woken by a message after the other program -- the
/// vector program in mode `b` when `busy`, [`QUIET_PROGRAM`] otherwise --
/// has run beside it: its status.
fn run_probe(busy: bool) -> Result<i32, &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let (mine, theirs) = Endpoint::pair().map_err(|_| "could not make a channel")?;
    let mut code = PROBE_PROGRAM.to_vec();
    patch(&mut code, 8, &BOOTSTRAP.0.to_le_bytes())?;
    let probe = load(b"/xinuse", &code)?;
    let placed = probe
        .with_handles(|table| table.insert(Object::Channel(mine), Rights::CHANNEL))
        .map_err(|_| "no room for the probe's channel")?;
    if placed != BOOTSTRAP {
        return Err("a fresh process's first handle is not the one the probe was built for");
    }
    let probe_task =
        process::start_on(&probe, Some(cpu)).map_err(|_| "the probe could not be started")?;
    wait_until(
        deadline,
        "the probe never blocked in its native call",
        || probe_task.is_blocked(),
    )?;
    let other = if busy {
        vector_program(b'b', OTHER_PATTERN, OTHER_MXCSR, OTHER_CONTROL)?
    } else {
        load(b"/quiet", QUIET_PROGRAM)?
    };
    let other_task = process::start_on(&other, Some(cpu))
        .map_err(|_| "the program beside the probe could not be started")?;
    wait_until(deadline, "the program beside the probe never ran", || {
        other_task.switches() >= 2
    })?;
    send(Some(&theirs))?;
    let status = probe.wait_for_exit(deadline);
    process::kill(&other, KILLED_STATUS);
    let _ = other.wait_for_exit(deadline);
    drop(theirs);
    status.ok_or("the probe never ended")
}

/// Write `bytes` at `at` in `code`.
fn patch(code: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), &'static str> {
    code.get_mut(at..at + bytes.len())
        .ok_or("a fixture is shorter than its own layout")?
        .copy_from_slice(bytes);
    Ok(())
}

/// Build `code` into a 64-bit program called `name` and load it, not started.
///
/// `image::build_with` maps `0x400` bytes past its program headers, from the
/// file's start, and puts the code at the entry, `0x100` in: a fixture longer
/// than the rest would run into bytes that are not its own.
fn load(name: &[u8], code: &[u8]) -> Result<Arc<Process>, &'static str> {
    /// The bytes of code `build_with`'s text segment holds at the entry: an
    /// ELF64 header and two program headers, `0x400` bytes, less the entry's
    /// offset.
    const TEXT_ROOM: usize = 64 + 2 * 56 + 0x400 - 0x100;
    if code.len() > TEXT_ROOM {
        return Err("a switch-state fixture is longer than the loader's text segment holds");
    }
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf64,
        super::super::ARCH.elf_machine(),
        crate::syscall::image::Shape::Good,
        code,
    );
    crate::syscall::exec::load(&file, &[name], &[], [0x7e; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a switch-state program could not be loaded")
}

/// Where the trading programs map their words, and the clearer and the
/// reader both: the reader's recorded base equals the clearer's, which is
/// what a "last written" skip would have compared against.
const BASE_LOW: u64 = 0x5000_0000;
/// The second trading program's base.
const BASE_HIGH: u64 = 0x5100_0000;
/// Yields each `FS` program makes.
const FS_ROUNDS: u32 = 10_000;

/// Two programs with different `FS` bases trade one processor
/// [`FS_ROUNDS`] times each, each reading its own word through `%fs:0`
/// between yields: the switch writes the incoming task's base at every
/// switch, from its record. Answers the trades.
///
/// Verifies: `L.x86_64.61`, `L.x86_64.158`
fn check_programs_trade_their_fs_bases() -> Result<u32, &'static str> {
    let first = fs_program(b't', BASE_LOW, 0x1111_2222_3333_4444)?;
    let second = fs_program(b't', BASE_HIGH, 0x5555_6666_7777_8888)?;
    let [first, second] = run_fs_pair(&first, &second)?;
    for status in [first, second] {
        fs_verdict(
            status,
            "two programs trading one processor did not each read their own FS base",
        )?;
    }
    Ok(FS_ROUNDS * 2)
}

/// A program that loads `USER_DS` and then a null selector into `FS` --
/// which leaves the base the descriptor's or cleared, by vendor -- gets its
/// recorded base back after a switch (Linux's rule); and a second program,
/// whose recorded base equals the one the processor last wrote, reads its own
/// word at every turn, because the write is never skipped (the consultant's
/// condition 8).
///
/// Verifies: `L.x86_64.61`, `L.x86_64.158`
fn check_a_cleared_base_comes_back_and_leaks_nowhere() -> Result<(), &'static str> {
    let clearer = fs_program(b'c', BASE_LOW, 0x0C0C_0C0C_0C0C_0C0C)?;
    let reader = fs_program(b't', BASE_LOW, 0x0D0D_0D0D_0D0D_0D0D)?;
    let [cleared, read] = run_fs_pair(&clearer, &reader)?;
    fs_verdict(
        read,
        "a program whose recorded FS base equals the one last written ran on the base another program left",
    )?;
    match cleared {
        Some(3 | 139) => Err(
            "a program that cleared its FS base with a null selector did not get its recorded base back after a switch",
        ),
        status => fs_verdict(
            status,
            "a program that set its FS base and cleared it with a null selector did not read its own word",
        ),
    }
}

/// Start both pinned to this processor and answer their statuses.
fn run_fs_pair(
    first: &Arc<Process>,
    second: &Arc<Process>,
) -> Result<[Option<i32>; 2], &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let tasks: [Arc<Task>; 2] = [
        process::start_on(first, Some(cpu)).map_err(|_| "an FS program could not be started")?,
        process::start_on(second, Some(cpu)).map_err(|_| "an FS program could not be started")?,
    ];
    let statuses = [
        first.wait_for_exit(deadline),
        second.wait_for_exit(deadline),
    ];
    drop(tasks);
    Ok(statuses)
}

/// What an `FS` program's status says: 0 is its own word at every read.
fn fs_verdict(status: Option<i32>, wrong: &'static str) -> Result<(), &'static str> {
    match status {
        Some(0) => Ok(()),
        Some(8 | 9) => Err("an FS program's mmap or arch_prctl failed"),
        Some(_) => Err(wrong),
        None => Err("an FS program never ended"),
    }
}

/// A copy of [`FS_PROGRAM`] in `mode`, with its word `magic` at `base`.
fn fs_program(mode: u8, base: u64, magic: u64) -> Result<Arc<Process>, &'static str> {
    let mut code = FS_PROGRAM.to_vec();
    if code.get(4) != Some(&b'?') {
        return Err("the FS program's mode byte is not where its layout says");
    }
    patch(&mut code, 4, &[mode])?;
    patch(&mut code, 8, &base.to_le_bytes())?;
    patch(&mut code, 16, &magic.to_le_bytes())?;
    patch(&mut code, 24, &(gdt::USER_DATA | 3).to_le_bytes())?;
    patch(&mut code, 28, &FS_ROUNDS.to_le_bytes())?;
    load(b"/fs-base", &code)
}

// ---------------------------------------------------------------------------
// Step 4's case 15: a caller the fast path parked, resumed by the general path
// ---------------------------------------------------------------------------

/// Where the fixture's `movq $-1, %rsi` sits -- `channel_write_read`'s count,
/// sending nothing -- which the sending variant patches to a count of zero.
const COUNT_AT: usize = 250;

/// The reader on the other end of a sending vector case, handed its handle.
static FAST_READER: crate::sync::SpinLock<Option<Handle>> = crate::sync::SpinLock::new(None);

/// The reader: two receive-only calls, the second held until its process is
/// killed, so that its end stays open while the victim waits.
fn read_twice(_argument: usize) {
    let taken = FAST_READER.lock().take();
    if let Some(handle) = taken {
        for _ in 0..2 {
            let _ = super::super::syscall::check::drive_native_words(
                ferrix_native_abi::nr::CHANNEL_WRITE_READ,
                [
                    u64::from(handle.0),
                    ferrix_native_abi::nr::WRITE_READ_NOTHING as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            );
        }
    }
    process::exit_current(0)
}

/// Step 4's case 15 (`docs/OPAQUE-KERNEL.md` §9.7, condition 7): the victim
/// *sends* (a count of zero, patched into the fixture) to a reader parked on
/// the other end, so that with the fast path on its send is handed over
/// directly and the victim parked by the fast path, not by the receive half.
/// The other program fills the registers beside it, and the victim is woken
/// by the general path -- a message written by the check, or `SIGUSR1` and
/// then the message -- and must read back the initial state, as every case
/// above. Made again, a few times at most, until a direct hand-over carried
/// the send, on a boot with the fast path and two processors; once
/// otherwise.
///
/// Verifies: `L.x86_64.155`
fn run_sending_vector_case(wake: Wake) -> Result<(), &'static str> {
    use crate::sched::direct::{Count, counts};
    // On one processor the check's own task is runnable beside the victim as
    // it starts -- the victim's arrival preempts it -- so the victim's send
    // finds a task waiting and goes the general way (T12): there the case is
    // made once, holding the general path to the reset.
    let fast = crate::trap::fast_write_read().is_some() && crate::smp::count() >= 2;
    for _ in 0..6 {
        let trips = || counts().get(Count::Trip as usize).copied().unwrap_or(0);
        let before = trips();
        send_then_wake(wake)?;
        if !fast || trips() != before {
            return Ok(());
        }
    }
    Err("case 15: no sending vector program's send was handed over by the fast path")
}

/// One try of [`run_sending_vector_case`].
fn send_then_wake(wake: Wake) -> Result<(), &'static str> {
    let here = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let count = crate::smp::count();
    let cpu = if count >= 2 { (here + 1) % count } else { here };
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let (mine, theirs) = Endpoint::pair().map_err(|_| "could not make a channel")?;
    let mut code = VECTOR_PROGRAM.to_vec();
    let victim = {
        if code.get(COUNT_AT..COUNT_AT + 7) != Some(&[0x48, 0xc7, 0xc6, 0xff, 0xff, 0xff, 0xff][..])
        {
            return Err("case 15: the vector program's count is not where its layout says");
        }
        patch(&mut code, COUNT_AT + 3, &0_u32.to_le_bytes())?;
        patch(&mut code, 4, &[wake.mode()])?;
        patch(&mut code, 8, &BOOTSTRAP.0.to_le_bytes())?;
        patch(&mut code, 12, &VICTIM_MXCSR.to_le_bytes())?;
        patch(&mut code, 16, &VICTIM_CONTROL.to_le_bytes())?;
        patch(&mut code, 24, &VICTIM_PATTERN.to_le_bytes())?;
        patch(&mut code, 32, &OTHER_PATTERN.to_le_bytes())?;
        load(b"/vectors", &code)?
    };
    let placed = victim
        .with_handles(|table| table.insert(Object::Channel(mine), Rights::CHANNEL))
        .map_err(|_| "no room for the vector program's channel")?;
    if placed != BOOTSTRAP {
        return Err(
            "a fresh process's first handle is not the one the vector program was built for",
        );
    }
    let reader = process::new_for_check().map_err(|_| "no process for case 15's reader")?;
    let handle = reader
        .with_handles(|table| table.insert(Object::Channel(Arc::clone(&theirs)), Rights::CHANNEL))
        .map_err(|_| "no room for case 15's reader")?;
    *FAST_READER.lock() = Some(handle);
    let _reader_task =
        crate::syscall::check::spawn_in(&reader, "case 15 reader", read_twice, Some(cpu))?;
    wait_until(deadline, "case 15: the reader never waited", || {
        theirs.reader_waiting()
    })?;
    let victim_task = process::start_on(&victim, Some(cpu))
        .map_err(|_| "the vector program could not be started")?;
    wait_until(
        deadline,
        "case 15: the vector program never blocked in its call",
        || victim_task.is_blocked() && theirs.reader_waiting(),
    )?;
    let other = vector_program(b'b', OTHER_PATTERN, OTHER_MXCSR, OTHER_CONTROL)?;
    let other_task = process::start_on(&other, Some(cpu))
        .map_err(|_| "the program that fills the vector registers could not be started")?;
    // Alone on its processor once the victim is parked, it is switched to
    // once and yields to nobody: run, then given time to set its pattern.
    wait_until(
        deadline,
        "the program that fills the vector registers never ran",
        || other_task.switches() >= 1,
    )?;
    crate::sched::sleep_for(10_000_000);
    if wake == Wake::Signal {
        crate::syscall::kill::send(&victim, SIGUSR1, Origin::Kernel);
        crate::sched::sleep_for(5_000_000);
    }
    theirs.write_small(b"vectors!").map_err(
        |_| "case 15: the check could not write the message that wakes the vector program",
    )?;
    let status = victim.wait_for_exit(deadline);
    process::kill(&other, KILLED_STATUS);
    let _ = other.wait_for_exit(deadline);
    process::kill(&reader, KILLED_STATUS);
    let _ = reader.wait_for_exit(deadline);
    drop(theirs);
    vector_verdict(status)
}

// ---------------------------------------------------------------------------
// The DS/ES skip: 0 over 0 only, by the processor's own registers
// ---------------------------------------------------------------------------

/// A 64-bit program for the `DS`/`ES` skip's cases (`docs/OPAQUE-KERNEL.md`
/// §9.8, 3b, the consultant's S4), in one of four modes, patched at 4: `L`
/// loads the selector patched at 12 into `DS` and `ES` before each of its
/// yields; `R` reads `DS` and `ES` after each and exits 2 or 3 if either is
/// not 0; `T` installs a flat 32-bit data descriptor based at the data
/// segment through `set_thread_area` (an i386 call), and before each yield
/// loads its selector into `DS` and `ES`, then 0; `C` yields, then far-returns
/// into compatibility mode and reads through `DS`, which is null there, so
/// `SIGSEGV` must end it -- surviving the read exits 5. The yields are
/// patched at 8. Exits 0, or 10 if `set_thread_area` failed.
///
/// ```text
/// _start: jmp start
///         .org 4
/// mode:   .byte '?'
///         .org 8
/// rounds: .long 0x7f7f7f7f
///         .org 12
/// sel:    .word 0x7f7f
///         .org 16
/// start:  movzbl mode(%rip), %r13d
///         cmpb $'T', %r13b
///         jne 1f
///         movl $0x410100, %ecx
///         movl $-1, (%rcx)
///         movl $0x410000, 4(%rcx)
///         movl $0xfffff, 8(%rcx)
///         movl $0x51, 12(%rcx)
///         movl $243, %eax
///         movl $0x410100, %ebx
///         int $0x80
///         movl $10, %edi
///         testl %eax, %eax
///         jnz exit
///         movl $0x410100, %ecx
///         movl (%rcx), %r14d
///         shll $3, %r14d
///         orl $3, %r14d
/// 1:      movl rounds(%rip), %r12d
///         cmpb $'C', %r13b
///         je compat_reader
/// loop:   cmpb $'L', %r13b
///         jne 2f
///         movw sel(%rip), %ax
///         movw %ax, %ds
///         movw %ax, %es
///         jmp yield
/// 2:      cmpb $'T', %r13b
///         jne yield
///         movw %r14w, %ds
///         movw %r14w, %es
///         xorl %eax, %eax
///         movw %ax, %ds
///         movw %ax, %es
/// yield:  movl $24, %eax
///         syscall
///         cmpb $'R', %r13b
///         jne 4f
///         movw %ds, %ax
///         movl $2, %edi
///         testw %ax, %ax
///         jnz exit
///         movw %es, %ax
///         movl $3, %edi
///         testw %ax, %ax
///         jnz exit
/// 4:      decl %r12d
///         jnz loop
///         xorl %edi, %edi
/// exit:   movl $231, %eax
///         syscall
///         ud2
/// compat_reader:
///         movl $24, %eax
///         syscall
///         decl %r12d
///         jnz compat_reader
///         pushq $0x23
///         pushq $compat
///         lretq
///         .code32
/// compat: movl 0x400100, %eax
///         movl $252, %eax
///         movl $5, %ebx
///         int $0x80
///         ud2
/// ```
///
/// Assembled by GNU `as`, linked at the image's entry, and read back.
const SELECTOR_PROGRAM: &[u8] = &[
    0xeb, 0x0e, 0x00, 0x00, 0x3f, 0x00, 0x00, 0x00, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x00, 0x00,
    0x44, 0x0f, 0xb6, 0x2d, 0xec, 0xff, 0xff, 0xff, 0x41, 0x80, 0xfd, 0x54, 0x75, 0x45, 0xb9, 0x00,
    0x01, 0x41, 0x00, 0xc7, 0x01, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x41, 0x04, 0x00, 0x00, 0x41, 0x00,
    0xc7, 0x41, 0x08, 0xff, 0xff, 0x0f, 0x00, 0xc7, 0x41, 0x0c, 0x51, 0x00, 0x00, 0x00, 0xb8, 0xf3,
    0x00, 0x00, 0x00, 0xbb, 0x00, 0x01, 0x41, 0x00, 0xcd, 0x80, 0xbf, 0x0a, 0x00, 0x00, 0x00, 0x85,
    0xc0, 0x75, 0x70, 0xb9, 0x00, 0x01, 0x41, 0x00, 0x44, 0x8b, 0x31, 0x41, 0xc1, 0xe6, 0x03, 0x41,
    0x83, 0xce, 0x03, 0x44, 0x8b, 0x25, 0x9e, 0xff, 0xff, 0xff, 0x41, 0x80, 0xfd, 0x43, 0x74, 0x5c,
    0x41, 0x80, 0xfd, 0x4c, 0x75, 0x0d, 0x66, 0x8b, 0x05, 0x8f, 0xff, 0xff, 0xff, 0x8e, 0xd8, 0x8e,
    0xc0, 0xeb, 0x12, 0x41, 0x80, 0xfd, 0x54, 0x75, 0x0c, 0x41, 0x8e, 0xde, 0x41, 0x8e, 0xc6, 0x31,
    0xc0, 0x8e, 0xd8, 0x8e, 0xc0, 0xb8, 0x18, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x41, 0x80, 0xfd, 0x52,
    0x75, 0x1a, 0x66, 0x8c, 0xd8, 0xbf, 0x02, 0x00, 0x00, 0x00, 0x66, 0x85, 0xc0, 0x75, 0x14, 0x66,
    0x8c, 0xc0, 0xbf, 0x03, 0x00, 0x00, 0x00, 0x66, 0x85, 0xc0, 0x75, 0x07, 0x41, 0xff, 0xcc, 0x75,
    0xaf, 0x31, 0xff, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b, 0xb8, 0x18, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0x41, 0xff, 0xcc, 0x75, 0xf4, 0x6a, 0x23, 0x68, 0xe1, 0x01, 0x40, 0x00, 0x48,
    0xcb, 0xa1, 0x00, 0x01, 0x40, 0x00, 0xb8, 0xfc, 0x00, 0x00, 0x00, 0xbb, 0x05, 0x00, 0x00, 0x00,
    0xcd, 0x80, 0x0f, 0x0b,
];

/// An i386 program that reads `DS` and `ES` after each of its `sched_yield`s
/// and exits 2 or 3 unless both are `USER_DS` (`0x2b`), which the kernel gave
/// it at `execve`; 0 after the yields patched at 1.
///
/// ```text
/// _start: movl $0x7f7f7f7f, %esi
/// 1:      movl $158, %eax
///         int $0x80
///         movw %ds, %ax
///         movl $2, %ebx
///         cmpw $0x2b, %ax
///         jne exit
///         movw %es, %ax
///         movl $3, %ebx
///         cmpw $0x2b, %ax
///         jne exit
///         decl %esi
///         jnz 1b
///         xorl %ebx, %ebx
/// exit:   movl $252, %eax
///         int $0x80
///         ud2
/// ```
///
/// Assembled by GNU `as --32`, linked at the image's entry, and read back.
const SELECTOR_PROGRAM_I386: &[u8] = &[
    0xbe, 0x7f, 0x7f, 0x7f, 0x7f, 0xb8, 0x9e, 0x00, 0x00, 0x00, 0xcd, 0x80, 0x66, 0x8c, 0xd8, 0xbb,
    0x02, 0x00, 0x00, 0x00, 0x66, 0x83, 0xf8, 0x2b, 0x75, 0x13, 0x66, 0x8c, 0xc0, 0xbb, 0x03, 0x00,
    0x00, 0x00, 0x66, 0x83, 0xf8, 0x2b, 0x75, 0x05, 0x4e, 0x75, 0xda, 0x31, 0xdb, 0xb8, 0xfc, 0x00,
    0x00, 0x00, 0xcd, 0x80, 0x0f, 0x0b,
];

/// `EM_386`, the machine of an i386 image.
const EM_386: u16 = 3;

/// Yields each selector program makes.
const SELECTOR_ROUNDS: u32 = 1_000;

/// Yields the compatibility-mode reader makes before its read, enough for the
/// program beside it to have left `DS` and `ES` 0 over its descriptor.
const COMPAT_ROUNDS: u32 = 20;

/// The status `SIGSEGV` ends a program with, as `wait4` reports it.
const KILLED_BY_SIGSEGV: i32 = 128 + ferrix_linux_abi::types::SIGSEGV as i32;

/// What the `DS`/`ES` skip's cases counted, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct SelectorReport {
    /// Whether the compatibility-mode read through a null `DS` was decided:
    /// not under QEMU's TCG, which never checks a data segment's presence.
    pub(crate) compat_decided: bool,
    /// Switches between two programs with null `DS` and `ES` during case
    /// (iv) that left them unloaded.
    pub(crate) skipped: u64,
    /// Yields across which programs read their own `DS` and `ES`.
    pub(crate) rounds: u32,
}

/// Every case of the `DS`/`ES` skip (the consultant's S4), on this processor.
///
/// # Errors
///
/// The first case that read what it must not.
pub(crate) fn run_selectors() -> Result<SelectorReport, &'static str> {
    // `USER_DS` first, so that each control fires in its own case: the skip
    // on the record alone fails here, the skip on "null" in the next.
    check_user_ds_is_loaded_over()?;
    check_a_null_selector_with_rpl_is_loaded_over()?;
    let compat_decided = check_null_ds_faults_in_compatibility_mode()?;
    check_32_and_64_bit_programs_keep_their_selectors()?;
    let skipped = check_the_skip_is_taken()?;
    Ok(SelectorReport {
        compat_decided,
        skipped,
        rounds: SELECTOR_ROUNDS * 4,
    })
}

/// Case (i): a program that loads 3 -- a null selector with RPL 3, which it
/// can read back -- into `DS` and `ES`, beside one whose record is 0: the
/// second reads 0 and 0 at every turn, since the skip is for exactly 0 over
/// exactly 0, not for "null".
///
/// Verifies: `L.x86_64.8`
fn check_a_null_selector_with_rpl_is_loaded_over() -> Result<(), &'static str> {
    let loader = selector_program(b'L', 3, SELECTOR_ROUNDS)?;
    let reader = selector_program(b'R', 0, SELECTOR_ROUNDS)?;
    let [loaded, read] = run_selector_pair(&loader, &reader)?;
    selector_verdict(loaded, "a program that loaded DS and ES with 3 did not run")?;
    selector_verdict(
        read,
        "a program with null DS and ES read the RPL bits of a null selector another program left",
    )
}

/// Case (ii): a program that leaves `USER_DS` in `DS` and `ES`, beside one
/// whose record is 0: the second reads 0 at every turn, since the processor's
/// own selector is compared, not the record alone.
///
/// Verifies: `L.x86_64.8`
fn check_user_ds_is_loaded_over() -> Result<(), &'static str> {
    let loader = selector_program(b'L', gdt::USER_DATA | 3, SELECTOR_ROUNDS)?;
    let reader = selector_program(b'R', 0, SELECTOR_ROUNDS)?;
    let [loaded, read] = run_selector_pair(&loader, &reader)?;
    selector_verdict(
        loaded,
        "a program that loaded DS and ES with USER_DS did not run",
    )?;
    selector_verdict(
        read,
        "a program with null DS and ES read the USER_DS another program left",
    )
}

/// Case (iii), first half: a program that loads a thread-local descriptor
/// with a base into `DS` and `ES` and then 0 before each yield, beside one
/// whose record is 0, which far-returns into compatibility mode and reads
/// through `DS`: the read is `#GP` and `SIGSEGV` ends it, whatever the hidden
/// part of a `DS` that reads 0 still holds -- the same with or without the
/// skip, since a null load over a null selector changes nothing.
///
/// Under QEMU's TCG the read is not decided: its emulation loads a null
/// selector as an absent segment but never checks a data access against
/// that, so the read succeeds with or without the skip (seen with the skip
/// turned off, `po9-sel-c4-off-tcg`). There the reader must end either way,
/// and answers `false`; the case is decided on hardware and under KVM, as the
/// consultant asked (ledger line 412, E3 (iii)).
///
/// Verifies: `L.x86_64.8`
fn check_null_ds_faults_in_compatibility_mode() -> Result<bool, &'static str> {
    let loader = selector_program(b'T', 0, SELECTOR_ROUNDS)?;
    let reader = selector_program(b'C', 0, COMPAT_ROUNDS)?;
    let [loaded, read] = run_selector_pair(&loader, &reader)?;
    match loaded {
        Some(10) => return Err("set_thread_area refused the selector program's descriptor"),
        status => selector_verdict(
            status,
            "a program that loaded a thread-local descriptor into DS and ES did not run",
        )?,
    }
    match read {
        Some(KILLED_BY_SIGSEGV) => Ok(true),
        Some(5) if under_tcg() => Ok(false),
        Some(5) => Err(
            "a program with null DS read through it in compatibility mode after another program left a based descriptor's hidden part",
        ),
        Some(_) => Err("the compatibility-mode reader ended neither by SIGSEGV nor its own exit"),
        None => Err("the compatibility-mode reader never ended"),
    }
}

/// Case (iii), second half: an i386 program, whose `DS` and `ES` are
/// `USER_DS`, and a 64-bit one, whose are 0, trade the processor: each reads
/// its own at every turn, so neither the record alone nor the processor alone
/// decides a skip.
///
/// Verifies: `L.x86_64.8`
fn check_32_and_64_bit_programs_keep_their_selectors() -> Result<(), &'static str> {
    let mut code = SELECTOR_PROGRAM_I386.to_vec();
    if code.get(1..5) != Some(&[0x7f; 4]) {
        return Err("the i386 selector program's count is not where its layout says");
    }
    patch(&mut code, 1, &SELECTOR_ROUNDS.to_le_bytes())?;
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf32,
        EM_386,
        crate::syscall::image::Shape::Good,
        &code,
    );
    let wide = crate::syscall::exec::load(
        &file,
        &[b"/selectors-i386"],
        &[],
        [0x7e; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the i386 selector program could not be loaded")?;
    let narrow = selector_program(b'R', 0, SELECTOR_ROUNDS)?;
    let [wide, narrow] = run_selector_pair(&wide, &narrow)?;
    selector_verdict(
        wide,
        "an i386 program did not read its own USER_DS in DS and ES beside a 64-bit program",
    )?;
    selector_verdict(
        narrow,
        "a 64-bit program did not read its own null DS and ES beside an i386 program",
    )
}

/// Case (iv): two programs with null `DS` and `ES` trade the processor, and
/// the switches between them leave both unloaded: the skip is taken. Answers
/// how many times.
///
/// Verifies: `L.x86_64.8`
fn check_the_skip_is_taken() -> Result<u64, &'static str> {
    let first = selector_program(b'R', 0, SELECTOR_ROUNDS)?;
    let second = selector_program(b'R', 0, SELECTOR_ROUNDS)?;
    let before = super::selector_skips();
    let [first, second] = run_selector_pair(&first, &second)?;
    let after = super::selector_skips();
    for status in [first, second] {
        selector_verdict(
            status,
            "a program with null DS and ES did not read them null beside another",
        )?;
    }
    match after.wrapping_sub(before) {
        0 => Err("no switch between two programs with null DS and ES left them unloaded"),
        skipped => Ok(skipped),
    }
}

/// A copy of [`SELECTOR_PROGRAM`] in `mode`, loading `selector` (mode `L`),
/// for `rounds` yields.
fn selector_program(mode: u8, selector: u16, rounds: u32) -> Result<Arc<Process>, &'static str> {
    let mut code = SELECTOR_PROGRAM.to_vec();
    if code.get(4) != Some(&b'?') || code.get(8..14) != Some(&[0x7f; 6]) {
        return Err("the selector program's fields are not where its layout says");
    }
    patch(&mut code, 4, &[mode])?;
    patch(&mut code, 8, &rounds.to_le_bytes())?;
    patch(&mut code, 12, &selector.to_le_bytes())?;
    load(b"/selectors", &code)
}

/// Start both pinned to this processor and answer their statuses.
fn run_selector_pair(
    first: &Arc<Process>,
    second: &Arc<Process>,
) -> Result<[Option<i32>; 2], &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let tasks: [Arc<Task>; 2] = [
        process::start_on(first, Some(cpu))
            .map_err(|_| "a selector program could not be started")?,
        process::start_on(second, Some(cpu))
            .map_err(|_| "a selector program could not be started")?,
    ];
    let statuses = [
        first.wait_for_exit(deadline),
        second.wait_for_exit(deadline),
    ];
    drop(tasks);
    Ok(statuses)
}

/// What a selector program's status says: 0 is its own selectors at every
/// read.
fn selector_verdict(status: Option<i32>, wrong: &'static str) -> Result<(), &'static str> {
    match status {
        Some(0) => Ok(()),
        Some(_) => Err(wrong),
        None => Err("a selector program never ended"),
    }
}

/// Whether this is QEMU's TCG: a hypervisor is present (`CPUID.1:ECX[31]`)
/// and its leaf `0x4000_0000` names it `TCGTCGTCGTCG` (QEMU's
/// `target/i386/cpu.c`). On hardware the bit is clear and the leaf is not
/// read.
fn under_tcg() -> bool {
    /// `CPUID.1:ECX[31]`: running under a hypervisor.
    const HYPERVISOR: u32 = 1 << 31;
    if core::arch::x86_64::__cpuid(1).ecx & HYPERVISOR == 0 {
        return false;
    }
    let leaf = core::arch::x86_64::__cpuid(0x4000_0000);
    [leaf.ebx, leaf.ecx, leaf.edx]
        == [
            u32::from_le_bytes(*b"TCGT"),
            u32::from_le_bytes(*b"CGTC"),
            u32::from_le_bytes(*b"GTCG"),
        ]
}
