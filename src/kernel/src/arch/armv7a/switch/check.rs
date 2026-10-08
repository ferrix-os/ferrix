//! The switch's user state on ARMv7-A, checked with programs in USR mode:
//! the two thread ID registers (`docs/OPAQUE-KERNEL.md` §9.14, 3b and F-66)
//! and the VFP registers of a task resumed from a blocking native call (3a's
//! port).
//!
//! Verification, not the switch: a file of its own so that the manifest
//! counts it as the test it is. Each case is a real program, built into an
//! ELF and run by the Linux personality's loader, pinned with a second one to
//! this core, because only a program's own registers, read in USR mode after
//! a real switch, show what the switch gave it. The programs are fixtures,
//! assembled by GNU `as` for ARMv7-A and read back out of the object file;
//! their source is in the constants' documentation.
//!
//! `TPIDRURO`, the thread pointer `set_tls` sets, is the task's record: a
//! program reads its own back after a `nanosleep` and while it trades the
//! core with another. `TPIDRURW`, which a program writes itself, is saved at
//! every switch out and written at every switch in: two programs trading the
//! core each read their own; one whose value equals the one the core last had
//! written still reads its own beside one that changed the register after
//! that write (the consultant's condition 8); a fork child reads its
//! parent's; and an image `execve` gave reads zero in both.
//!
//! 3a's port: a task blocks in `channel_write_read` or `object_wait_one` with
//! every D register holding its pattern and `FPSCR` its own, a second program
//! on the same core fills them with another and runs, and the first is woken
//! -- by a message, by its peer's close, and by a message with a signal
//! pending, whose handler must have run -- and must read back its own `FPSCR`
//! and `d8`-`d15` and zero in every other D register. The same task blocked
//! in a Linux `nanosleep`, or preempted in user mode, reads back its own
//! pattern; and one woken from `channel_write_read` keeps its registers
//! across a `nanosleep` after it, because the mark is the call's.

use alloc::sync::Arc;

use ferrix_linux_abi::types::SIGUSR1;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Rights;

use super::UserState;
use crate::console::println;
use crate::object::Object;
use crate::object::channel::Endpoint;
use crate::object::job::KILLED_STATUS;
use crate::sched::Task;
use crate::syscall::process::{self, Process};
use crate::syscall::signal::Origin;

/// How long a case may take before its program is called stuck: generous
/// against a loaded host under TCG.
const PATIENCE_NANOS: u64 = 60_000_000_000;

/// The thread-register fixture. Mode `t` sets its thread pointer and
/// `TPIDRURW` and reads both back after each of its rounds of `sched_yield`;
/// `n` after one `nanosleep`; `c` writes its `TPIDRURW` and then the second
/// value in turn, reading each back after a yield; `f` forks a child that
/// reads its parent's `TPIDRURW` after its rounds; `x` copies its own image to
/// `/tmp/tls-exec` and `execve`s it, and the new image reads zero in both
/// after its rounds. Exits 0, or the code [`tls_verdict`] names. Patched at 4
/// (mode), 8 (thread pointer), 12 (`TPIDRURW`), 16 (the second value) and 20
/// (rounds).
///
/// ```text
/// @ Thread-register fixture for OPAQUE-KERNEL.md 9.14 (c) and F-66. Patched by
/// @ the kernel: 4 mode, 8 TPIDRURO (set_tls), 12 own TPIDRURW, 16 the changer's
/// @ second TPIDRURW, 20 rounds. Exits 0, or the code tls_verdict names.
///         .syntax unified
///         .arm
///         .text
///         .globl _start
/// _start: b     start
/// mode:   .byte '?', 0, 0, 0
/// tp:     .word 0
/// rw:     .word 0
/// alt:    .word 0
/// rounds: .word 0
/// nap:    .word 0, 5000000
/// path:   .asciz "/tmp/tls-exec"
///         .balign 4
/// zname:  .asciz "z"
///         .balign 4
/// start:
///         ldr   r0, [sp, #4]          @ argv[0]: "z" is the image execve gave
///         ldrb  r0, [r0]
///         cmp   r0, #'z'
///         beq   zeroed
///         ldrb  r11, mode
///         ldr   r6, rounds
///         ldr   r0, tp
///         bl    set_tls
///         ldr   r4, rw
///         mcr   p15, 0, r4, c13, c0, 2
///         cmp   r11, #'c'
///         beq   changer
///         cmp   r11, #'n'
///         beq   napper
///         cmp   r11, #'x'
///         beq   execer
///         cmp   r11, #'f'
///         beq   forker
/// 1:      bl    yield                 @ 't': trade, reading both after each turn
///         bl    check_both
///         subs  r6, r6, #1
///         bne   1b
/// pass:   mov   r0, #0
/// exit:   mov   r7, #248
///         svc   #0
///         udf   #0
/// fail:   mov   r0, #8
///         b     exit
///
/// @ TPIDRURO the set_tls value, else 1; TPIDRURW r4, else 2.
/// check_both:
///         mrc   p15, 0, r0, c13, c0, 3
///         ldr   r1, tp
///         cmp   r0, r1
///         movne r0, #1
///         bne   exit
///         mrc   p15, 0, r0, c13, c0, 2
///         cmp   r0, r4
///         movne r0, #2
///         bne   exit
///         bx    lr
///
/// @ Condition 8: W, yield, W read back (3); Z, yield, Z read back (4).
/// changer:
///         ldr   r5, alt
/// 1:      mcr   p15, 0, r4, c13, c0, 2
///         bl    yield
///         mrc   p15, 0, r0, c13, c0, 2
///         cmp   r0, r4
///         movne r0, #3
///         bne   exit
///         mcr   p15, 0, r5, c13, c0, 2
///         bl    yield
///         mrc   p15, 0, r0, c13, c0, 2
///         cmp   r0, r5
///         movne r0, #4
///         bne   exit
///         subs  r6, r6, #1
///         bne   1b
///         b     pass
///
/// @ Blocked in a Linux nanosleep, then both read back.
/// napper:
///         adr   r0, nap
///         mov   r1, #0
///         mov   r7, #162
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bl    check_both
///         b     pass
///
/// @ fork: the child, switched out and in, reads its parent's TPIDRURW (7);
/// @ the parent exits with the child's status.
/// forker:
///         mov   r7, #2
///         svc   #0
///         cmp   r0, #0
///         blt   fail
///         beq   2f
///         sub   sp, sp, #8
///         mov   r1, sp
///         mov   r2, #0
///         mov   r3, #0
///         mov   r7, #114              @ wait4(pid, &status, 0, 0)
///         svc   #0
///         cmp   r0, #0
///         blt   fail
///         ldr   r0, [sp]
///         lsr   r0, r0, #8
///         and   r0, r0, #255
///         b     exit
/// 2:      bl    yield
///         subs  r6, r6, #1
///         bne   2b
///         mrc   p15, 0, r0, c13, c0, 2
///         cmp   r0, r4
///         movne r0, #7
///         bne   exit
///         b     pass
///
/// @ execve: this image copied to /tmp/tls-exec -- its text segment, then the
/// @ data segment's 16 bytes at 4096 -- and run with argv {"z"}.
/// execer:
///         adr   r0, path
///         movw  r1, #0x241            @ O_WRONLY | O_CREAT | O_TRUNC
///         movw  r2, #0x1ed            @ 0755
///         mov   r7, #5
///         svc   #0
///         cmp   r0, #0
///         blt   fail
///         mov   r5, r0
///         mov   r1, #0x400000         @ the text segment, from the file's start
///         movw  r2, #1140
///         mov   r7, #4
///         svc   #0
///         cmp   r0, r2
///         bne   fail
///         mov   r0, r5
///         mov   r1, #4096
///         mov   r2, #0
///         mov   r7, #19               @ lseek(fd, 4096, SEEK_SET)
///         svc   #0
///         mov   r0, r5
///         mov   r1, #0x410000         @ the data segment
///         mov   r2, #16
///         mov   r7, #4
///         svc   #0
///         cmp   r0, #16
///         bne   fail
///         mov   r0, r5
///         mov   r7, #6
///         svc   #0
///         sub   sp, sp, #16
///         adr   r1, zname
///         str   r1, [sp]
///         mov   r1, #0
///         str   r1, [sp, #4]
///         str   r1, [sp, #8]
///         adr   r0, path
///         mov   r1, sp
///         add   r2, sp, #8
///         mov   r7, #11
///         svc   #0
///         b     fail
///
/// @ The image execve gave: switched out and in, both registers 0 (5, 6).
/// zeroed:
///         ldr   r6, rounds
/// 1:      bl    yield
///         subs  r6, r6, #1
///         bne   1b
///         mrc   p15, 0, r0, c13, c0, 3
///         cmp   r0, #0
///         movne r0, #5
///         bne   exit
///         mrc   p15, 0, r0, c13, c0, 2
///         cmp   r0, #0
///         movne r0, #6
///         bne   exit
///         b     pass
///
/// yield:  mov   r7, #158
///         svc   #0
///         bx    lr
///
/// set_tls:
///         movw  r7, #5
///         movt  r7, #0xf
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bx    lr
/// ```
const TLS_PROGRAM: &[u8] = &[
    0x0b, 0x00, 0x00, 0xea, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x4b, 0x4c, 0x00,
    0x2f, 0x74, 0x6d, 0x70, 0x2f, 0x74, 0x6c, 0x73, 0x2d, 0x65, 0x78, 0x65, 0x63, 0x00, 0x00, 0x00,
    0x7a, 0x00, 0x00, 0x00, 0x04, 0x00, 0x9d, 0xe5, 0x00, 0x00, 0xd0, 0xe5, 0x7a, 0x00, 0x50, 0xe3,
    0x7b, 0x00, 0x00, 0x0a, 0x48, 0xb0, 0x5f, 0xe5, 0x3c, 0x60, 0x1f, 0xe5, 0x4c, 0x00, 0x1f, 0xe5,
    0x87, 0x00, 0x00, 0xeb, 0x50, 0x40, 0x1f, 0xe5, 0x50, 0x4f, 0x0d, 0xee, 0x63, 0x00, 0x5b, 0xe3,
    0x19, 0x00, 0x00, 0x0a, 0x6e, 0x00, 0x5b, 0xe3, 0x27, 0x00, 0x00, 0x0a, 0x78, 0x00, 0x5b, 0xe3,
    0x46, 0x00, 0x00, 0x0a, 0x66, 0x00, 0x5b, 0xe3, 0x2b, 0x00, 0x00, 0x0a, 0x79, 0x00, 0x00, 0xeb,
    0x07, 0x00, 0x00, 0xeb, 0x01, 0x60, 0x56, 0xe2, 0xfb, 0xff, 0xff, 0x1a, 0x00, 0x00, 0xa0, 0xe3,
    0xf8, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0xf0, 0x00, 0xf0, 0xe7, 0x08, 0x00, 0xa0, 0xe3,
    0xfa, 0xff, 0xff, 0xea, 0x70, 0x0f, 0x1d, 0xee, 0xa8, 0x10, 0x1f, 0xe5, 0x01, 0x00, 0x50, 0xe1,
    0x01, 0x00, 0xa0, 0x13, 0xf5, 0xff, 0xff, 0x1a, 0x50, 0x0f, 0x1d, 0xee, 0x04, 0x00, 0x50, 0xe1,
    0x02, 0x00, 0xa0, 0x13, 0xf1, 0xff, 0xff, 0x1a, 0x1e, 0xff, 0x2f, 0xe1, 0xc4, 0x50, 0x1f, 0xe5,
    0x50, 0x4f, 0x0d, 0xee, 0x63, 0x00, 0x00, 0xeb, 0x50, 0x0f, 0x1d, 0xee, 0x04, 0x00, 0x50, 0xe1,
    0x03, 0x00, 0xa0, 0x13, 0xe9, 0xff, 0xff, 0x1a, 0x50, 0x5f, 0x0d, 0xee, 0x5d, 0x00, 0x00, 0xeb,
    0x50, 0x0f, 0x1d, 0xee, 0x05, 0x00, 0x50, 0xe1, 0x04, 0x00, 0xa0, 0x13, 0xe3, 0xff, 0xff, 0x1a,
    0x01, 0x60, 0x56, 0xe2, 0xf1, 0xff, 0xff, 0x1a, 0xdf, 0xff, 0xff, 0xea, 0xfc, 0x00, 0x4f, 0xe2,
    0x00, 0x10, 0xa0, 0xe3, 0xa2, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3,
    0xdd, 0xff, 0xff, 0x1a, 0xde, 0xff, 0xff, 0xeb, 0xd7, 0xff, 0xff, 0xea, 0x02, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0xd7, 0xff, 0xff, 0xba, 0x0b, 0x00, 0x00, 0x0a,
    0x08, 0xd0, 0x4d, 0xe2, 0x0d, 0x10, 0xa0, 0xe1, 0x00, 0x20, 0xa0, 0xe3, 0x00, 0x30, 0xa0, 0xe3,
    0x72, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0xce, 0xff, 0xff, 0xba,
    0x00, 0x00, 0x9d, 0xe5, 0x20, 0x04, 0xa0, 0xe1, 0xff, 0x00, 0x00, 0xe2, 0xc7, 0xff, 0xff, 0xea,
    0x3c, 0x00, 0x00, 0xeb, 0x01, 0x60, 0x56, 0xe2, 0xfc, 0xff, 0xff, 0x1a, 0x50, 0x0f, 0x1d, 0xee,
    0x04, 0x00, 0x50, 0xe1, 0x07, 0x00, 0xa0, 0x13, 0xc0, 0xff, 0xff, 0x1a, 0xbe, 0xff, 0xff, 0xea,
    0x5e, 0x0f, 0x4f, 0xe2, 0x41, 0x12, 0x00, 0xe3, 0xed, 0x21, 0x00, 0xe3, 0x05, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0xbb, 0xff, 0xff, 0xba, 0x00, 0x50, 0xa0, 0xe1,
    0x01, 0x15, 0xa0, 0xe3, 0x74, 0x24, 0x00, 0xe3, 0x04, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x02, 0x00, 0x50, 0xe1, 0xb4, 0xff, 0xff, 0x1a, 0x05, 0x00, 0xa0, 0xe1, 0x01, 0x1a, 0xa0, 0xe3,
    0x00, 0x20, 0xa0, 0xe3, 0x13, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x05, 0x00, 0xa0, 0xe1,
    0x41, 0x18, 0xa0, 0xe3, 0x10, 0x20, 0xa0, 0xe3, 0x04, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x10, 0x00, 0x50, 0xe3, 0xa8, 0xff, 0xff, 0x1a, 0x05, 0x00, 0xa0, 0xe1, 0x06, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x10, 0xd0, 0x4d, 0xe2, 0x1e, 0x1e, 0x4f, 0xe2, 0x00, 0x10, 0x8d, 0xe5,
    0x00, 0x10, 0xa0, 0xe3, 0x04, 0x10, 0x8d, 0xe5, 0x08, 0x10, 0x8d, 0xe5, 0x81, 0x0f, 0x4f, 0xe2,
    0x0d, 0x10, 0xa0, 0xe1, 0x08, 0x20, 0x8d, 0xe2, 0x0b, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x99, 0xff, 0xff, 0xea, 0x28, 0x62, 0x1f, 0xe5, 0x0a, 0x00, 0x00, 0xeb, 0x01, 0x60, 0x56, 0xe2,
    0xfc, 0xff, 0xff, 0x1a, 0x70, 0x0f, 0x1d, 0xee, 0x00, 0x00, 0x50, 0xe3, 0x05, 0x00, 0xa0, 0x13,
    0x8e, 0xff, 0xff, 0x1a, 0x50, 0x0f, 0x1d, 0xee, 0x00, 0x00, 0x50, 0xe3, 0x06, 0x00, 0xa0, 0x13,
    0x8a, 0xff, 0xff, 0x1a, 0x88, 0xff, 0xff, 0xea, 0x9e, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x1e, 0xff, 0x2f, 0xe1, 0x05, 0x70, 0x00, 0xe3, 0x0f, 0x70, 0x40, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0x84, 0xff, 0xff, 0x1a, 0x1e, 0xff, 0x2f, 0xe1,
];

/// Rounds each trading program makes.
const TLS_ROUNDS: u32 = 1_000;
/// Rounds a fork child, an `execve`d image and the program beside them make.
const SIDE_ROUNDS: u32 = 200;

/// The trading programs' thread pointers and `TPIDRURW` values.
const TP_FIRST: u32 = 0x1111_1111;
/// See [`TP_FIRST`].
const TP_SECOND: u32 = 0x2222_2222;
/// See [`TP_FIRST`].
const RW_FIRST: u32 = 0x3333_3333;
/// See [`TP_FIRST`].
const RW_SECOND: u32 = 0x4444_4444;
/// The value both programs of the condition-8 case hold, and the one the
/// changer writes after the switch in that wrote it.
const RW_SHARED: u32 = 0x5555_5555;
/// See [`RW_SHARED`].
const RW_CHANGED: u32 = 0x6666_6666;

/// Run every check of the thread ID registers, and answer the switches the
/// trading programs made.
///
/// # Errors
///
/// The first case that read what it must not.
pub(crate) fn run_tls() -> Result<u32, &'static str> {
    check_a_thread_pointer_comes_back()?;
    check_programs_trade_their_thread_registers()?;
    check_a_changed_register_leaks_nowhere()?;
    check_a_fork_child_inherits_tpidrurw()?;
    check_an_execed_image_starts_from_zero()?;
    Ok(TLS_ROUNDS * 2)
}

/// A program sets its thread pointer and `TPIDRURW`, blocks in a Linux
/// `nanosleep`, and reads both back: `set_tls` wrote the record the switch
/// loads, and the save read `TPIDRURW`.
///
/// Verifies: `L.armv7a.10`, `L.armv7a.11`
fn check_a_thread_pointer_comes_back() -> Result<(), &'static str> {
    let napper = tls_program(b'n', TP_FIRST, RW_FIRST, 0, SIDE_ROUNDS)?;
    let [status] = run_pinned([&napper])?;
    tls_verdict(
        status,
        "a program did not get its own thread pointer back after a switch",
        "a program did not get its own TPIDRURW back after a switch",
    )
}

/// Two programs with different thread pointers and `TPIDRURW` trade one core
/// [`TLS_ROUNDS`] times each, each reading both after every yield: the
/// switch writes the incoming task's at every switch.
///
/// Verifies: `L.armv7a.10`, `L.armv7a.11`
fn check_programs_trade_their_thread_registers() -> Result<(), &'static str> {
    let first = tls_program(b't', TP_FIRST, RW_FIRST, 0, TLS_ROUNDS)?;
    let second = tls_program(b't', TP_SECOND, RW_SECOND, 0, TLS_ROUNDS)?;
    for status in run_pinned([&first, &second])? {
        tls_verdict(
            status,
            "two programs trading one core did not each read their own TPIDRURO",
            "two programs trading one core did not each read their own TPIDRURW",
        )?;
    }
    Ok(())
}

/// The consultant's condition 8 on this register: the changer's switch in
/// writes [`RW_SHARED`], the changer then writes [`RW_CHANGED`] itself and
/// yields, and the reader, whose own value is [`RW_SHARED`] too, is switched
/// in. A write skipped because it equals the one last written would leave it
/// running on the changer's value.
///
/// Verifies: `L.armv7a.11`
fn check_a_changed_register_leaks_nowhere() -> Result<(), &'static str> {
    let changer = tls_program(b'c', TP_FIRST, RW_SHARED, RW_CHANGED, TLS_ROUNDS)?;
    let reader = tls_program(b't', TP_SECOND, RW_SHARED, 0, TLS_ROUNDS * 2)?;
    let [changed, read] = run_pinned([&changer, &reader])?;
    tls_verdict(
        read,
        "a program whose TPIDRURW equals the one last written ran on the value another program left",
        "a program whose TPIDRURW equals the one last written ran on the value another program left",
    )?;
    tls_verdict(
        changed,
        "a program that changed its own TPIDRURW lost its thread pointer",
        "a program that changed its own TPIDRURW did not read back what it wrote",
    )
}

/// A parent writes its `TPIDRURW` and forks; the child, switched out and in
/// beside a second program, reads its parent's value.
///
/// Verifies: `L.armv7a.12`
fn check_a_fork_child_inherits_tpidrurw() -> Result<(), &'static str> {
    let parent = tls_program(b'f', TP_FIRST, RW_FIRST, 0, SIDE_ROUNDS)?;
    let beside = tls_program(b't', TP_SECOND, RW_SECOND, 0, SIDE_ROUNDS * 2)?;
    let [forked, other] = run_pinned([&parent, &beside])?;
    tls_verdict(
        other,
        "a program beside a fork did not read its own TPIDRURO",
        "a program beside a fork did not read its own TPIDRURW",
    )?;
    match forked {
        Some(7) => Err("a fork child did not inherit its parent's TPIDRURW"),
        status => tls_verdict(
            status,
            "a forking program did not read its own TPIDRURO",
            "a forking program did not read its own TPIDRURW",
        ),
    }
}

/// A program sets its thread pointer and `TPIDRURW` and `execve`s a copy of
/// itself; the new image, switched out and in beside a second program, reads
/// zero in both: `execve` zeroes the registers and the record together.
///
/// Verifies: `L.armv7a.10`, `L.armv7a.12`
fn check_an_execed_image_starts_from_zero() -> Result<(), &'static str> {
    let execer = tls_program(b'x', TP_FIRST, RW_FIRST, 0, SIDE_ROUNDS)?;
    let beside = tls_program(b't', TP_SECOND, RW_SECOND, 0, SIDE_ROUNDS * 2)?;
    let [execed, other] = run_pinned([&execer, &beside])?;
    tls_verdict(
        other,
        "a program beside an execve did not read its own TPIDRURO",
        "a program beside an execve did not read its own TPIDRURW",
    )?;
    match execed {
        Some(5) => Err("an execve'd image got the old program's TPIDRURO back"),
        Some(6) => Err("an execve'd image read the old program's TPIDRURW"),
        status => tls_verdict(
            status,
            "an execve'd image did not read TPIDRURO",
            "an execve'd image did not read TPIDRURW",
        ),
    }
}

/// What a thread-register program's status says: 0 is its own at every read,
/// 1 a wrong `TPIDRURO`, 2 to 4 a wrong `TPIDRURW`.
fn tls_verdict(
    status: Option<i32>,
    wrong_ro: &'static str,
    wrong_rw: &'static str,
) -> Result<(), &'static str> {
    match status {
        Some(0) => Ok(()),
        Some(1) => Err(wrong_ro),
        Some(2..=4) => Err(wrong_rw),
        Some(8) => Err("a call of the thread-register program failed"),
        Some(status) => {
            println!("  tls      the thread-register program ended with {status}");
            Err("the thread-register program ended with a status it has no meaning for")
        }
        None => Err("the thread-register program never ended"),
    }
}

/// A copy of [`TLS_PROGRAM`] in `mode`, loaded and not started.
fn tls_program(
    mode: u8,
    thread_pointer: u32,
    own: u32,
    second: u32,
    rounds: u32,
) -> Result<Arc<Process>, &'static str> {
    let mut code = TLS_PROGRAM.to_vec();
    if code.get(4) != Some(&b'?') {
        return Err("the thread-register program's mode byte is not where its layout says");
    }
    patch(&mut code, 4, &[mode])?;
    patch(&mut code, 8, &thread_pointer.to_le_bytes())?;
    patch(&mut code, 12, &own.to_le_bytes())?;
    patch(&mut code, 16, &second.to_le_bytes())?;
    patch(&mut code, 20, &rounds.to_le_bytes())?;
    load(b"/tls", &code)
}

/// Start `programs` pinned to this core, in order, and answer their statuses.
fn run_pinned<const N: usize>(
    programs: [&Arc<Process>; N],
) -> Result<[Option<i32>; N], &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let mut tasks: alloc::vec::Vec<Arc<Task>> = alloc::vec::Vec::with_capacity(N);
    for program in programs {
        tasks.push(
            process::start_on(program, Some(cpu))
                .map_err(|_| "a switch-state program could not be started")?,
        );
    }
    let statuses = programs.map(|program| program.wait_for_exit(deadline));
    drop(tasks);
    Ok(statuses)
}

/// Write `bytes` at `at` in `code`.
fn patch(code: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), &'static str> {
    code.get_mut(at..at + bytes.len())
        .ok_or("a fixture is shorter than its own layout")?
        .copy_from_slice(bytes);
    Ok(())
}

/// Build `code` into a 32-bit program called `name` and load it, not started.
///
/// `image::build_with` maps `0x400` bytes past its program headers, from the
/// file's start, and puts the code at the entry, `0x100` in: a fixture longer
/// than the rest would run into bytes that are not its own.
fn load(name: &[u8], code: &[u8]) -> Result<Arc<Process>, &'static str> {
    /// The bytes of code `build_with`'s text segment holds at the entry: an
    /// ELF32 header and two program headers, `0x400` bytes, less the entry's
    /// offset.
    const TEXT_ROOM: usize = 52 + 2 * 32 + 0x400 - 0x100;
    if code.len() > TEXT_ROOM {
        return Err("a switch-state fixture is longer than the loader's text segment holds");
    }
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf32,
        super::super::ARCH.elf_machine(),
        crate::syscall::image::Shape::Good,
        code,
    );
    crate::syscall::exec::load(&file, &[name], &[], [0x7e; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a switch-state program could not be loaded")
}

// ---------------------------------------------------------------------------
// 3a's port: the VFP registers of a task resumed from a blocking native call
// ---------------------------------------------------------------------------

/// What the vector cases counted, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct VectorReport {
    /// Wakes from a blocking native call that found the reset state.
    pub(crate) reset: u32,
    /// Switches out of a Linux call, a preemption or past a woken native
    /// call that kept the registers whole.
    pub(crate) kept: u32,
    /// Whether the cases ran: the fixture names `d16`-`d31`, which a D16 core
    /// does not have.
    pub(crate) ran: bool,
}

/// The handle a fixture finds its channel under: the first a fresh process
/// is given.
const BOOTSTRAP: Handle = Handle(1);

/// The victim's pattern in every D register: pi, as a double.
const VICTIM_PATTERN: u64 = 0x4009_21FB_5444_2D18;
/// The other program's: e.
const OTHER_PATTERN: u64 = 0x4005_BF0A_8B14_5769;
/// The victim's `FPSCR`: `Z`, default NaN, round toward zero, inexact.
const VICTIM_FPSCR: u32 = 0x42C0_0010;
/// The other program's: `N`, flush to zero, round toward minus infinity,
/// division by zero.
const OTHER_FPSCR: u32 = 0x8180_0002;

/// The VFP-state fixture. Mode `w`, `o`, `g`, `s`, `p` or `m` is the victim of
/// [`Wake`]'s cases; mode `b` fills every D register with its pattern and
/// `FPSCR` with its own and yields until it is killed. It writes and reads the
/// registers through a 256-byte area on its stack, and exits 0, or with the
/// code [`vector_verdict`] names. Patched at 4 (mode), 8 (handle), 12
/// (`FPSCR`), 16 (its pattern) and 24 (the other's).
///
/// ```text
/// @ VFP-state fixture for OPAQUE-KERNEL.md 9.14 (b). Patched by the kernel:
/// @  4 mode, 8 handle, 12 own FPSCR, 16 own pattern (8 bytes), 24 the other's.
/// @ r10 is 512 bytes of stack: r10+0 the flag the SIGUSR1 handler sets,
/// @ r10+16 the 256 bytes d0-d31 are written and read through.
///         .syntax unified
///         .arm
///         .fpu neon-vfpv4
///         .text
///         .globl _start
/// _start: b     start
/// mode:   .byte '?', 0, 0, 0
/// handle: .word 0
/// fpscr:  .word 0
/// own:    .word 0, 0
/// other:  .word 0, 0
/// nap:    .word 0, 30000000
/// start:
///         ldrb  r11, mode
///         ldr   r9, handle
///         sub   sp, sp, #512
///         bic   sp, sp, #7
///         mov   r10, sp
///         mov   r0, #0
///         str   r0, [r10]
///         cmp   r11, #'g'
///         bne   2f
///         add   r1, r10, #288         @ SA_SIGINFO, no restorer, empty mask
///         adr   r0, handler
///         str   r0, [r1]
///         mov   r0, #4
///         str   r0, [r1, #4]
///         mov   r0, #0
///         str   r0, [r1, #8]
///         str   r0, [r1, #12]
///         str   r0, [r1, #16]
///         mov   r0, #10               @ rt_sigaction(SIGUSR1, act, 0, 8)
///         mov   r2, #0
///         mov   r3, #8
///         mov   r7, #174
///         svc   #0
///         cmp   r0, #0
///         bne   fail
/// 2:      bl    set_pattern
///         cmp   r11, #'b'
///         beq   pollute
///         cmp   r11, #'s'
///         beq   sleeper
///         cmp   r11, #'p'
///         beq   spinner
///         cmp   r11, #'o'
///         beq   closer
///         mov   r0, r9                @ channel_write_read(handle, nothing, ...)
///         mvn   r1, #0
///         mov   r2, #0
///         mov   r3, #0
///         mov   r4, #0
///         mov   r5, #0
///         movw  r7, #0x1013
///         svc   #0
///         cmp   r0, #0
///         blt   fail
///         bl    check_reset
///         cmp   r0, #0
///         bne   exit
///         cmp   r11, #'g'
///         bne   3f
///         ldr   r1, [r10]
///         cmp   r1, #1
///         movne r0, #7
///         bne   exit
/// 3:      cmp   r11, #'m'
///         bne   pass
///         bl    set_pattern
///         bl    nap_once
///         bl    check_own
///         cmp   r0, #0
///         beq   exit
///         add   r0, r0, #20
///         b     exit
/// closer: mov   r0, r9                @ object_wait_one(handle, PEER_CLOSED, 0, 0)
///         mov   r1, #4
///         mov   r2, #0
///         mov   r3, #0
///         movw  r7, #0x1008
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bl    check_reset
///         b     exit
/// sleeper:
///         bl    nap_once
///         bl    check_own
///         b     exit
/// spinner:
///         movw  r4, #0xe100           @ 100,000,000
///         movt  r4, #0x05f5
/// 4:      subs  r4, r4, #1
///         bne   4b
///         bl    check_own
///         b     exit
/// pollute:
///         mov   r7, #158              @ sched_yield, until killed
///         svc   #0
///         b     pollute
/// pass:   mov   r0, #0
///         b     exit
/// fail:   mov   r0, #10
/// exit:   mov   r7, #248
///         svc   #0
///         udf   #0
///
/// nap_once:
///         adr   r0, nap
///         mov   r1, #0
///         mov   r7, #162
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bx    lr
///
/// @ Every D register this program's pattern, and FPSCR its own.
/// set_pattern:
///         ldr   r2, own
///         ldr   r3, own + 4
///         add   r0, r10, #16
///         mov   r1, #32
/// 5:      strd  r2, r3, [r0], #8
///         subs  r1, r1, #1
///         bne   5b
///         add   r0, r10, #16
///         vldmia r0!, {d0-d15}
///         vldmia r0, {d16-d31}
///         ldr   r0, fpscr
///         vmsr  fpscr, r0
///         bx    lr
///
/// save:   add   r0, r10, #16
///         vstmia r0!, {d0-d15}
///         vstmia r0, {d16-d31}
///         bx    lr
///
/// @ Walk the 32 slots. r8 1: every slot its own is due; 0: d8-d15 its own and
/// @ zero elsewhere (the reset). r0: 0 as due; 1 the other's pattern; where its
/// @ own is due, 12 zero or 13 else; where zero is due, 2 its own or 3 else.
/// compare:
///         push  {r4-r6, lr}
///         add   r4, r10, #16
///         mov   r5, #0
/// 9:      ldrd  r2, r3, [r4], #8
///         mov   r6, r8
///         cmp   r5, #8
///         blt   10f
///         cmp   r5, #16
///         movlt r6, #1
/// 10:     ldr   r0, own
///         ldr   r1, own + 4
///         cmp   r6, #0
///         moveq r0, #0
///         moveq r1, #0
///         cmp   r2, r0
///         cmpeq r3, r1
///         beq   11f
///         ldr   r0, other
///         ldr   r1, other + 4
///         cmp   r2, r0
///         cmpeq r3, r1
///         moveq r0, #1
///         beq   12f
///         cmp   r6, #0
///         beq   13f
///         orrs  r0, r2, r3
///         moveq r0, #12
///         movne r0, #13
///         b     12f
/// 13:     ldr   r0, own
///         ldr   r1, own + 4
///         cmp   r2, r0
///         cmpeq r3, r1
///         moveq r0, #2
///         movne r0, #3
///         b     12f
/// 11:     add   r5, r5, #1
///         cmp   r5, #32
///         bne   9b
///         mov   r0, #0
/// 12:     pop   {r4-r6, pc}
///
/// @ After a blocking native call: FPSCR its own (else 4), d8-d15 its own
/// @ (else 15), the rest zero (compare's 1 to 3).
/// check_reset:
///         push  {r8, lr}
///         vmrs  r0, fpscr
///         ldr   r1, fpscr
///         cmp   r0, r1
///         movne r0, #4
///         bne   14f
///         bl    save
///         mov   r8, #0
///         bl    compare
///         cmp   r0, #12
///         movge r0, #15
/// 14:     pop   {r8, pc}
///
/// @ Kept whole: FPSCR its own (else 14), every slot its own (11 the other's,
/// @ 12 zero, 13 else).
/// check_own:
///         push  {r8, lr}
///         vmrs  r0, fpscr
///         ldr   r1, fpscr
///         cmp   r0, r1
///         movne r0, #14
///         bne   15f
///         bl    save
///         mov   r8, #1
///         bl    compare
///         cmp   r0, #1
///         moveq r0, #11
/// 15:     pop   {r8, pc}
///
/// @ SIGUSR1 with SA_SIGINFO: note that it ran, in the word the interrupted r10
/// @ names (uc_mcontext's arm_r10, at 72 in the ucontext r2 points to).
/// handler:
///         ldr   r3, [r2, #72]
///         mov   r0, #1
///         str   r0, [r3]
///         bx    lr
/// ```
const VECTOR_PROGRAM: &[u8] = &[
    0x08, 0x00, 0x00, 0xea, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x80, 0xc3, 0xc9, 0x01, 0x2c, 0xb0, 0x5f, 0xe5, 0x2c, 0x90, 0x1f, 0xe5,
    0x02, 0xdc, 0x4d, 0xe2, 0x07, 0xd0, 0xcd, 0xe3, 0x0d, 0xa0, 0xa0, 0xe1, 0x00, 0x00, 0xa0, 0xe3,
    0x00, 0x00, 0x8a, 0xe5, 0x67, 0x00, 0x5b, 0xe3, 0x0f, 0x00, 0x00, 0x1a, 0x12, 0x1e, 0x8a, 0xe2,
    0xa7, 0x0f, 0x8f, 0xe2, 0x00, 0x00, 0x81, 0xe5, 0x04, 0x00, 0xa0, 0xe3, 0x04, 0x00, 0x81, 0xe5,
    0x00, 0x00, 0xa0, 0xe3, 0x08, 0x00, 0x81, 0xe5, 0x0c, 0x00, 0x81, 0xe5, 0x10, 0x00, 0x81, 0xe5,
    0x0a, 0x00, 0xa0, 0xe3, 0x00, 0x20, 0xa0, 0xe3, 0x08, 0x30, 0xa0, 0xe3, 0xae, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0x3c, 0x00, 0x00, 0x1a, 0x46, 0x00, 0x00, 0xeb,
    0x62, 0x00, 0x5b, 0xe3, 0x34, 0x00, 0x00, 0x0a, 0x73, 0x00, 0x5b, 0xe3, 0x29, 0x00, 0x00, 0x0a,
    0x70, 0x00, 0x5b, 0xe3, 0x2a, 0x00, 0x00, 0x0a, 0x6f, 0x00, 0x5b, 0xe3, 0x1b, 0x00, 0x00, 0x0a,
    0x09, 0x00, 0xa0, 0xe1, 0x00, 0x10, 0xe0, 0xe3, 0x00, 0x20, 0xa0, 0xe3, 0x00, 0x30, 0xa0, 0xe3,
    0x00, 0x40, 0xa0, 0xe3, 0x00, 0x50, 0xa0, 0xe3, 0x13, 0x70, 0x01, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0x29, 0x00, 0x00, 0xba, 0x6d, 0x00, 0x00, 0xeb, 0x00, 0x00, 0x50, 0xe3,
    0x27, 0x00, 0x00, 0x1a, 0x67, 0x00, 0x5b, 0xe3, 0x03, 0x00, 0x00, 0x1a, 0x00, 0x10, 0x9a, 0xe5,
    0x01, 0x00, 0x51, 0xe3, 0x07, 0x00, 0xa0, 0x13, 0x21, 0x00, 0x00, 0x1a, 0x6d, 0x00, 0x5b, 0xe3,
    0x1c, 0x00, 0x00, 0x1a, 0x28, 0x00, 0x00, 0xeb, 0x20, 0x00, 0x00, 0xeb, 0x6c, 0x00, 0x00, 0xeb,
    0x00, 0x00, 0x50, 0xe3, 0x1a, 0x00, 0x00, 0x0a, 0x14, 0x00, 0x80, 0xe2, 0x18, 0x00, 0x00, 0xea,
    0x09, 0x00, 0xa0, 0xe1, 0x04, 0x10, 0xa0, 0xe3, 0x00, 0x20, 0xa0, 0xe3, 0x00, 0x30, 0xa0, 0xe3,
    0x08, 0x70, 0x01, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0x0f, 0x00, 0x00, 0x1a,
    0x53, 0x00, 0x00, 0xeb, 0x0e, 0x00, 0x00, 0xea, 0x10, 0x00, 0x00, 0xeb, 0x5c, 0x00, 0x00, 0xeb,
    0x0b, 0x00, 0x00, 0xea, 0x00, 0x41, 0x0e, 0xe3, 0xf5, 0x45, 0x40, 0xe3, 0x01, 0x40, 0x54, 0xe2,
    0xfd, 0xff, 0xff, 0x1a, 0x56, 0x00, 0x00, 0xeb, 0x05, 0x00, 0x00, 0xea, 0x9e, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0xfc, 0xff, 0xff, 0xea, 0x00, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xea,
    0x0a, 0x00, 0xa0, 0xe3, 0xf8, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0xf0, 0x00, 0xf0, 0xe7,
    0x5e, 0x0f, 0x4f, 0xe2, 0x00, 0x10, 0xa0, 0xe3, 0xa2, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0xf5, 0xff, 0xff, 0x1a, 0x1e, 0xff, 0x2f, 0xe1, 0xa4, 0x21, 0x1f, 0xe5,
    0xa4, 0x31, 0x1f, 0xe5, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x10, 0xa0, 0xe3, 0xf8, 0x20, 0xc0, 0xe0,
    0x01, 0x10, 0x51, 0xe2, 0xfc, 0xff, 0xff, 0x1a, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x0b, 0xb0, 0xec,
    0x20, 0x0b, 0xd0, 0xec, 0xd0, 0x01, 0x1f, 0xe5, 0x10, 0x0a, 0xe1, 0xee, 0x1e, 0xff, 0x2f, 0xe1,
    0x10, 0x00, 0x8a, 0xe2, 0x20, 0x0b, 0xa0, 0xec, 0x20, 0x0b, 0xc0, 0xec, 0x1e, 0xff, 0x2f, 0xe1,
    0x70, 0x40, 0x2d, 0xe9, 0x10, 0x40, 0x8a, 0xe2, 0x00, 0x50, 0xa0, 0xe3, 0xd8, 0x20, 0xc4, 0xe0,
    0x08, 0x60, 0xa0, 0xe1, 0x08, 0x00, 0x55, 0xe3, 0x01, 0x00, 0x00, 0xba, 0x10, 0x00, 0x55, 0xe3,
    0x01, 0x60, 0xa0, 0xb3, 0x0c, 0x02, 0x1f, 0xe5, 0x0c, 0x12, 0x1f, 0xe5, 0x00, 0x00, 0x56, 0xe3,
    0x00, 0x00, 0xa0, 0x03, 0x00, 0x10, 0xa0, 0x03, 0x00, 0x00, 0x52, 0xe1, 0x01, 0x00, 0x53, 0x01,
    0x12, 0x00, 0x00, 0x0a, 0x24, 0x02, 0x1f, 0xe5, 0x24, 0x12, 0x1f, 0xe5, 0x00, 0x00, 0x52, 0xe1,
    0x01, 0x00, 0x53, 0x01, 0x01, 0x00, 0xa0, 0x03, 0x10, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x56, 0xe3,
    0x03, 0x00, 0x00, 0x0a, 0x03, 0x00, 0x92, 0xe1, 0x0c, 0x00, 0xa0, 0x03, 0x0d, 0x00, 0xa0, 0x13,
    0x0a, 0x00, 0x00, 0xea, 0x5c, 0x02, 0x1f, 0xe5, 0x5c, 0x12, 0x1f, 0xe5, 0x00, 0x00, 0x52, 0xe1,
    0x01, 0x00, 0x53, 0x01, 0x02, 0x00, 0xa0, 0x03, 0x03, 0x00, 0xa0, 0x13, 0x03, 0x00, 0x00, 0xea,
    0x01, 0x50, 0x85, 0xe2, 0x20, 0x00, 0x55, 0xe3, 0xdb, 0xff, 0xff, 0x1a, 0x00, 0x00, 0xa0, 0xe3,
    0x70, 0x80, 0xbd, 0xe8, 0x00, 0x41, 0x2d, 0xe9, 0x10, 0x0a, 0xf1, 0xee, 0x98, 0x12, 0x1f, 0xe5,
    0x01, 0x00, 0x50, 0xe1, 0x04, 0x00, 0xa0, 0x13, 0x04, 0x00, 0x00, 0x1a, 0xcb, 0xff, 0xff, 0xeb,
    0x00, 0x80, 0xa0, 0xe3, 0xcd, 0xff, 0xff, 0xeb, 0x0c, 0x00, 0x50, 0xe3, 0x0f, 0x00, 0xa0, 0xa3,
    0x00, 0x81, 0xbd, 0xe8, 0x00, 0x41, 0x2d, 0xe9, 0x10, 0x0a, 0xf1, 0xee, 0xc8, 0x12, 0x1f, 0xe5,
    0x01, 0x00, 0x50, 0xe1, 0x0e, 0x00, 0xa0, 0x13, 0x04, 0x00, 0x00, 0x1a, 0xbf, 0xff, 0xff, 0xeb,
    0x01, 0x80, 0xa0, 0xe3, 0xc1, 0xff, 0xff, 0xeb, 0x01, 0x00, 0x50, 0xe3, 0x0b, 0x00, 0xa0, 0x03,
    0x00, 0x81, 0xbd, 0xe8, 0x48, 0x30, 0x92, 0xe5, 0x01, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x83, 0xe5,
    0x1e, 0xff, 0x2f, 0xe1,
];

/// Run every check of the vector-state contract.
///
/// # Errors
///
/// The first case that read what it must not.
pub(crate) fn run_vectors() -> Result<VectorReport, &'static str> {
    let mut report = VectorReport::default();
    check_the_three_numbers()?;
    check_unsaved_reads_as_reset()?;
    if super::super::cpu::user_fpu_doubles() != 32 {
        return Ok(report);
    }
    report.ran = true;
    for wake in [Wake::Message, Wake::Close, Wake::Signal] {
        run_vector_case(wake)?;
        report.reset += 1;
    }
    for wake in [Wake::LinuxSleep, Wake::Preempted, Wake::MarkLowered] {
        run_vector_case(wake)?;
        report.kept += 1;
    }
    Ok(report)
}

/// The entry raises the mark for exactly the three native calls that block,
/// by number: none of the rest of the native range, of Linux's first 1,024
/// numbers, or of the Arm private range.
///
/// Verifies: `L.armv7a.5`
fn check_the_three_numbers() -> Result<(), &'static str> {
    use ferrix_native_abi::nr::{CHANNEL_WRITE_READ, FIRST, LAST, OBJECT_WAIT_ONE, PORT_WAIT};

    use super::super::trap::vectors_die_in;

    let marked = (FIRST..=LAST)
        .chain(0..1024)
        .chain(0x000F_0000..0x000F_0100)
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

/// A state saved only in part reads, through the one accessor, as its own
/// `FPSCR` and `d8`-`d15` and zero elsewhere, never the doubles its record
/// still holds; and a writer leaves it whole, with no reset to come.
///
/// Verifies: `L.armv7a.9`
fn check_unsaved_reads_as_reset() -> Result<(), &'static str> {
    let mut stale = [0xA5A5_A5A5_A5A5_A5A5_u64; 32];
    for (index, slot) in stale.iter_mut().enumerate() {
        *slot ^= index as u64;
    }
    let mut state = UserState::new();
    state.set_fp(VICTIM_FPSCR, stale);
    state.mark_unsaved();
    let mut expected = [0_u64; 32];
    if let (Some(kept), Some(own)) = (expected.get_mut(8..16), stale.get(8..16)) {
        kept.copy_from_slice(own);
    }
    if !state.is_unsaved() || state.fp() != (VICTIM_FPSCR, expected) {
        return Err(
            "a VFP state saved only in part did not read as zero with its own FPSCR and d8-d15",
        );
    }
    state.set_fp(OTHER_FPSCR, stale);
    if state.is_unsaved() || state.fp() != (OTHER_FPSCR, stale) {
        return Err("a VFP state written by set_fp was still marked for a reset");
    }
    Ok(())
}

/// How a vector case's program is let go of the core and brought back.
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
/// Verifies: `L.armv7a.5`, `L.armv7a.6`, `L.armv7a.7`, `L.armv7a.8`
fn run_vector_case(wake: Wake) -> Result<(), &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let (mine, theirs) = Endpoint::pair().map_err(|_| "could not make a channel")?;
    let victim = vector_program(wake.mode(), VICTIM_PATTERN, OTHER_PATTERN, VICTIM_FPSCR)?;
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
    let other = vector_program(b'b', OTHER_PATTERN, VICTIM_PATTERN, OTHER_FPSCR)?;
    let other_task = process::start_on(&other, Some(cpu))
        .map_err(|_| "the program that fills the VFP registers could not be started")?;
    if wake.blocks_natively() {
        // Switched to twice: it has set its pattern and yielded at least
        // once since the victim blocked.
        wait_until(
            deadline,
            "the program that fills the VFP registers never ran",
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
        return Err("the program that fills the VFP registers never ran while the other slept");
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
            "a task resumed from a blocking native call kept its own caller-saved VFP registers: the reset did not run",
        ),
        Some(3) => Err(
            "a task resumed from a blocking native call read VFP registers that are neither reset nor any program's pattern",
        ),
        Some(4) => Err("a task resumed from a blocking native call did not get its own FPSCR back"),
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
        Some(13 | 14) => {
            Err("a task blocked in a Linux call or preempted did not get its own VFP state back")
        }
        Some(15) => {
            Err("a task resumed from a blocking native call lost its callee-saved VFP registers")
        }
        Some(31) => Err(
            "the mark outlived its call: a Linux sleep after a blocking native call let another program's vector registers in",
        ),
        Some(32) => Err(
            "the mark outlived its call: a Linux sleep after a blocking native call lost the vector registers to a reset",
        ),
        Some(33 | 34) => Err(
            "the mark outlived its call: a Linux sleep after a blocking native call did not keep the VFP state",
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

/// A copy of [`VECTOR_PROGRAM`] in `mode`, with `pattern` and `fpscr` its
/// own and `other` the pattern it must never read, loaded and not started.
fn vector_program(
    mode: u8,
    pattern: u64,
    other: u64,
    fpscr: u32,
) -> Result<Arc<Process>, &'static str> {
    let mut code = VECTOR_PROGRAM.to_vec();
    if code.get(4) != Some(&b'?') {
        return Err("the vector program's mode byte is not where its layout says");
    }
    patch(&mut code, 4, &[mode])?;
    patch(&mut code, 8, &BOOTSTRAP.0.to_le_bytes())?;
    patch(&mut code, 12, &fpscr.to_le_bytes())?;
    patch(&mut code, 16, &pattern.to_le_bytes())?;
    patch(&mut code, 24, &other.to_le_bytes())?;
    load(b"/vectors", &code)
}

// ---------------------------------------------------------------------------
// Â§9.15: lazy VFP -- no VFP work for a program that never uses it
// ---------------------------------------------------------------------------

/// What the lazy cases counted, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct LazyReport {
    /// Whether the cases ran: the fixtures name `d16`-`d31`.
    pub(crate) ran: bool,
    /// Round trips two programs without VFP made, moving nothing.
    pub(crate) trips: u32,
    /// Whether a program with VFP state was resumed on another processor.
    pub(crate) migrated: bool,
}

/// The lazy-VFP fixture A: the first use, `SIGILL`'s address, the traders.
/// Mode `a` takes its first VFP instruction in ARM state, `t` in Thumb state
/// after a 16-bit increment, `i` in Thumb state inside an IT block; `e`, `f`,
/// `m`, `n` then read `FPEXC`, `FPSID`, `MVFR0`, `MVFR1` at PL0; `u` names
/// its three undefined instructions' addresses in a `SIGILL` handler; `c`
/// and `s` are the client and server of C1, `q` naps without VFP. Exits 0,
/// with `SIGILL` where a read is UNDEFINED, or the code [`lazy_verdict`]
/// names. Patched at 4 (mode), 8 (handle), 12 (rounds).
///
/// ```text
/// @ Lazy-VFP fixture A for OPAQUE-KERNEL.md 9.15. Patched by the kernel:
/// @  4 mode, 8 handle, 12 rounds. r10 is 512 bytes of stack: r10+0 the flag
/// @ the SIGILL handler sets, r10+4 the address it must name, r10+8 where it
/// @ resumes, r10+16 the 256 bytes d0-d31 are read through, r10+288 a sigaction.
///         .syntax unified
///         .arm
///         .fpu neon-vfpv4
///         .text
///         .globl _start
/// _start: b     start
/// mode:   .byte '?', 0, 0, 0
/// handle: .word 0
/// rounds: .word 0
/// nap:    .word 0, 20000000
/// start:
///         ldrb  r11, mode
///         ldr   r9, handle
///         sub   sp, sp, #512
///         bic   sp, sp, #7
///         mov   r10, sp
///         cmp   r11, #'c'
///         beq   client
///         cmp   r11, #'s'
///         beq   server
///         cmp   r11, #'q'
///         beq   quiet
///         cmp   r11, #'t'
///         beq   thumb
///         cmp   r11, #'i'
///         beq   thumb
///         cmp   r11, #'u'
///         beq   sigill
/// @ 'a' and the system-register modes: the first VFP instruction, in ARM state.
///         vmrs  r0, fpscr
///         cmp   r0, #0
///         movne r0, #20
///         bne   exit
///         bl    check_zero
///         cmp   r11, #'e'
///         beq   rd_fpexc
///         cmp   r11, #'f'
///         beq   rd_fpsid
///         cmp   r11, #'m'
///         beq   rd_mvfr0
///         cmp   r11, #'n'
///         beq   rd_mvfr1
///         mov   r2, #0
///         movw  r3, #0
///         movt  r3, #0x3ff0
///         vmov  d16, r2, r3
///         vadd.f64 d17, d16, d16
///         vmov  r0, r1, d17
///         movt  r3, #0x4000
///         cmp   r0, #0
///         cmpeq r1, r3
///         movne r0, #23
///         bne   exit
/// pass:   mov   r0, #0
/// exit:   mov   r7, #248
///         svc   #0
///         udf   #0
/// fail:   mov   r0, #10
///         b     exit
///
/// @ Each a PL0 read the architecture makes UNDEFINED: SIGILL, never 30.
/// rd_fpexc:
///         vmrs  r0, fpexc
///         b     readable
/// rd_fpsid:
///         vmrs  r0, fpsid
///         b     readable
/// rd_mvfr0:
///         vmrs  r0, mvfr0
///         b     readable
/// rd_mvfr1:
///         vmrs  r0, mvfr1
/// readable:
///         mov   r0, #30
///         b     exit
///
/// @ Every D register and FPSCR zero, else 21.
/// check_zero:
///         add   r0, r10, #16
///         vstmia r0!, {d0-d15}
///         vstmia r0, {d16-d31}
///         add   r0, r10, #16
///         mov   r1, #64
/// 1:      ldr   r2, [r0], #4
///         cmp   r2, #0
///         movne r0, #21
///         bne   exit
///         subs  r1, r1, #1
///         bne   1b
///         vmrs  r0, fpscr
///         cmp   r0, #0
///         movne r0, #21
///         bne   exit
///         bx    lr
///
/// @ 't': the first VFP instruction in Thumb state, one 16-bit increment before
/// @ it (22 if it ran twice). 'i': the same inside an IT block (24 if the IT
/// @ state was lost: both slots ran).
/// thumb:
///         adr   r0, t_first
///         cmp   r11, #'i'
///         adreq r0, t_it
///         orr   r0, r0, #1
///         blx   r0
///         cmp   r4, #1
///         beq   1f
///         cmp   r11, #'i'
///         moveq r0, #24
///         movne r0, #22
///         b     exit
/// 1:      cmp   r0, #0
///         movne r0, #20
///         bne   exit
///         bl    check_zero
///         b     pass
///
/// @ SIGILL at its own address (F-68), in a program with the bit: an ARM udf, a
/// @ 16-bit Thumb udf and a 32-bit Thumb udf.w; the handler must name each (41
/// @ to 43 if not).
/// sigill:
///         add   r1, r10, #288
///         adr   r0, ill_handler
///         str   r0, [r1]
///         mov   r0, #4
///         str   r0, [r1, #4]
///         mov   r0, #0
///         str   r0, [r1, #8]
///         str   r0, [r1, #12]
///         str   r0, [r1, #16]
///         mov   r0, #4
///         mov   r2, #0
///         mov   r3, #8
///         mov   r7, #174
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         vmrs  r0, fpscr
///         adr   r0, u_arm
///         add   r1, r0, #4
///         bl    expect
/// u_arm:  udf   #1
///         ldr   r0, [r10]
///         cmp   r0, #1
///         movne r0, #41
///         bne   exit
///         adr   r0, t_ill16
///         add   r1, r0, #2
///         bl    expect
///         orr   r0, r0, #1
///         blx   r0
///         ldr   r0, [r10]
///         cmp   r0, #1
///         movne r0, #42
///         bne   exit
///         adr   r0, t_ill32
///         add   r1, r0, #4
///         bl    expect
///         orr   r0, r0, #1
///         blx   r0
///         ldr   r0, [r10]
///         cmp   r0, #1
///         movne r0, #43
///         bne   exit
///         b     pass
///
/// @ r0 the address the handler must name, r1 where it resumes.
/// expect: str   r0, [r10, #4]
///         str   r1, [r10, #8]
///         mov   r2, #0
///         str   r2, [r10]
///         bx    lr
///
/// @ SIGILL with SA_SIGINFO: 1 in the flag if si_addr and the ucontext's pc
/// @ both name the instruction, else 2; resume past it.
/// ill_handler:
///         ldr   r3, [r2, #72]
///         ldr   r12, [r3, #4]
///         ldr   r0, [r1, #12]
///         cmp   r0, r12
///         ldreq r0, [r2, #92]
///         cmpeq r0, r12
///         moveq r0, #1
///         movne r0, #2
///         str   r0, [r3]
///         ldr   r0, [r3, #8]
///         str   r0, [r2, #92]
///         bx    lr
///
/// @ The two programs of C1: neither touches VFP. The client makes `rounds`
/// @ round trips through channel_write_read; the server answers each.
/// client: ldr   r6, rounds
/// 1:      mov   r0, r9
///         mov   r1, #4
///         mov   r2, r6
///         mov   r3, #0
///         mov   r4, #0
///         movw  r7, #0x1013
///         svc   #0
///         cmp   r0, #4
///         bne   fail
///         subs  r6, r6, #1
///         bne   1b
///         b     pass
/// server: mov   r0, r9
///         mvn   r1, #0
///         mov   r2, #0
///         mov   r3, #0
///         mov   r4, #0
///         movw  r7, #0x1013
///         svc   #0
///         cmp   r0, #4
///         bne   fail
///         ldr   r6, rounds
/// 2:      mov   r0, r9
///         mov   r1, #4
///         mov   r2, r6
///         mov   r3, #0
///         mov   r4, #0
///         movw  r7, #0x1013
///         svc   #0
///         subs  r6, r6, #1
///         beq   pass
///         cmp   r0, #4
///         bne   fail
///         b     2b
///
/// @ 'q': never touches VFP; blocks in nanosleep `rounds` times.
/// quiet:  ldr   r6, rounds
/// 1:      adr   r0, nap
///         mov   r1, #0
///         mov   r7, #162
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         subs  r6, r6, #1
///         bne   1b
///         b     pass
///
///         .thumb
///         .balign 4
/// t_first:
///         movs  r4, #0
///         adds  r4, #1
///         vmrs  r0, fpscr
///         bx    lr
///         .balign 4
/// t_it:
///         movs  r4, #0
///         cmp   r4, #0
///         itte  eq
///         vmrseq r0, fpscr
///         addeq.n r4, #1
///         addne.n r4, #2
///         bx    lr
///         .balign 4
/// t_ill16:
///         udf   #1
///         bx    lr
///         .balign 4
/// t_ill32:
///         udf.w #1
///         bx    lr
/// ```
const LAZY_A: &[u8] = &[
    0x04, 0x00, 0x00, 0xea, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x2d, 0x31, 0x01, 0x1c, 0xb0, 0x5f, 0xe5, 0x1c, 0x90, 0x1f, 0xe5,
    0x02, 0xdc, 0x4d, 0xe2, 0x07, 0xd0, 0xcd, 0xe3, 0x0d, 0xa0, 0xa0, 0xe1, 0x63, 0x00, 0x5b, 0xe3,
    0x8d, 0x00, 0x00, 0x0a, 0x73, 0x00, 0x5b, 0xe3, 0x98, 0x00, 0x00, 0x0a, 0x71, 0x00, 0x5b, 0xe3,
    0xac, 0x00, 0x00, 0x0a, 0x74, 0x00, 0x5b, 0xe3, 0x3a, 0x00, 0x00, 0x0a, 0x69, 0x00, 0x5b, 0xe3,
    0x38, 0x00, 0x00, 0x0a, 0x75, 0x00, 0x5b, 0xe3, 0x46, 0x00, 0x00, 0x0a, 0x10, 0x0a, 0xf1, 0xee,
    0x00, 0x00, 0x50, 0xe3, 0x14, 0x00, 0xa0, 0x13, 0x14, 0x00, 0x00, 0x1a, 0x21, 0x00, 0x00, 0xeb,
    0x65, 0x00, 0x5b, 0xe3, 0x16, 0x00, 0x00, 0x0a, 0x66, 0x00, 0x5b, 0xe3, 0x16, 0x00, 0x00, 0x0a,
    0x6d, 0x00, 0x5b, 0xe3, 0x16, 0x00, 0x00, 0x0a, 0x6e, 0x00, 0x5b, 0xe3, 0x16, 0x00, 0x00, 0x0a,
    0x00, 0x20, 0xa0, 0xe3, 0x00, 0x30, 0x00, 0xe3, 0xf0, 0x3f, 0x43, 0xe3, 0x30, 0x2b, 0x43, 0xec,
    0xa0, 0x1b, 0x70, 0xee, 0x31, 0x0b, 0x51, 0xec, 0x00, 0x30, 0x44, 0xe3, 0x00, 0x00, 0x50, 0xe3,
    0x03, 0x00, 0x51, 0x01, 0x17, 0x00, 0xa0, 0x13, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0xa0, 0xe3,
    0xf8, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0xf0, 0x00, 0xf0, 0xe7, 0x0a, 0x00, 0xa0, 0xe3,
    0xfa, 0xff, 0xff, 0xea, 0x10, 0x0a, 0xf8, 0xee, 0x04, 0x00, 0x00, 0xea, 0x10, 0x0a, 0xf0, 0xee,
    0x02, 0x00, 0x00, 0xea, 0x10, 0x0a, 0xf7, 0xee, 0x00, 0x00, 0x00, 0xea, 0x10, 0x0a, 0xf6, 0xee,
    0x1e, 0x00, 0xa0, 0xe3, 0xf1, 0xff, 0xff, 0xea, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x0b, 0xa0, 0xec,
    0x20, 0x0b, 0xc0, 0xec, 0x10, 0x00, 0x8a, 0xe2, 0x40, 0x10, 0xa0, 0xe3, 0x04, 0x20, 0x90, 0xe4,
    0x00, 0x00, 0x52, 0xe3, 0x15, 0x00, 0xa0, 0x13, 0xe8, 0xff, 0xff, 0x1a, 0x01, 0x10, 0x51, 0xe2,
    0xf9, 0xff, 0xff, 0x1a, 0x10, 0x0a, 0xf1, 0xee, 0x00, 0x00, 0x50, 0xe3, 0x15, 0x00, 0xa0, 0x13,
    0xe2, 0xff, 0xff, 0x1a, 0x1e, 0xff, 0x2f, 0xe1, 0x1e, 0x0e, 0x8f, 0xe2, 0x69, 0x00, 0x5b, 0xe3,
    0x79, 0x0f, 0x8f, 0x02, 0x01, 0x00, 0x80, 0xe3, 0x30, 0xff, 0x2f, 0xe1, 0x01, 0x00, 0x54, 0xe3,
    0x03, 0x00, 0x00, 0x0a, 0x69, 0x00, 0x5b, 0xe3, 0x18, 0x00, 0xa0, 0x03, 0x16, 0x00, 0xa0, 0x13,
    0xd6, 0xff, 0xff, 0xea, 0x00, 0x00, 0x50, 0xe3, 0x14, 0x00, 0xa0, 0x13, 0xd3, 0xff, 0xff, 0x1a,
    0xe0, 0xff, 0xff, 0xeb, 0xd0, 0xff, 0xff, 0xea, 0x12, 0x1e, 0x8a, 0xe2, 0xb8, 0x00, 0x8f, 0xe2,
    0x00, 0x00, 0x81, 0xe5, 0x04, 0x00, 0xa0, 0xe3, 0x04, 0x00, 0x81, 0xe5, 0x00, 0x00, 0xa0, 0xe3,
    0x08, 0x00, 0x81, 0xe5, 0x0c, 0x00, 0x81, 0xe5, 0x10, 0x00, 0x81, 0xe5, 0x04, 0x00, 0xa0, 0xe3,
    0x00, 0x20, 0xa0, 0xe3, 0x08, 0x30, 0xa0, 0xe3, 0xae, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0xc4, 0xff, 0xff, 0x1a, 0x10, 0x0a, 0xf1, 0xee, 0x04, 0x00, 0x8f, 0xe2,
    0x04, 0x10, 0x80, 0xe2, 0x17, 0x00, 0x00, 0xeb, 0xf1, 0x00, 0xf0, 0xe7, 0x00, 0x00, 0x9a, 0xe5,
    0x01, 0x00, 0x50, 0xe3, 0x29, 0x00, 0xa0, 0x13, 0xb8, 0xff, 0xff, 0x1a, 0x56, 0x0f, 0x8f, 0xe2,
    0x02, 0x10, 0x80, 0xe2, 0x0f, 0x00, 0x00, 0xeb, 0x01, 0x00, 0x80, 0xe3, 0x30, 0xff, 0x2f, 0xe1,
    0x00, 0x00, 0x9a, 0xe5, 0x01, 0x00, 0x50, 0xe3, 0x2a, 0x00, 0xa0, 0x13, 0xaf, 0xff, 0xff, 0x1a,
    0x4e, 0x0f, 0x8f, 0xe2, 0x04, 0x10, 0x80, 0xe2, 0x06, 0x00, 0x00, 0xeb, 0x01, 0x00, 0x80, 0xe3,
    0x30, 0xff, 0x2f, 0xe1, 0x00, 0x00, 0x9a, 0xe5, 0x01, 0x00, 0x50, 0xe3, 0x2b, 0x00, 0xa0, 0x13,
    0xa6, 0xff, 0xff, 0x1a, 0xa4, 0xff, 0xff, 0xea, 0x04, 0x00, 0x8a, 0xe5, 0x08, 0x10, 0x8a, 0xe5,
    0x00, 0x20, 0xa0, 0xe3, 0x00, 0x20, 0x8a, 0xe5, 0x1e, 0xff, 0x2f, 0xe1, 0x48, 0x30, 0x92, 0xe5,
    0x04, 0xc0, 0x93, 0xe5, 0x0c, 0x00, 0x91, 0xe5, 0x0c, 0x00, 0x50, 0xe1, 0x5c, 0x00, 0x92, 0x05,
    0x0c, 0x00, 0x50, 0x01, 0x01, 0x00, 0xa0, 0x03, 0x02, 0x00, 0xa0, 0x13, 0x00, 0x00, 0x83, 0xe5,
    0x08, 0x00, 0x93, 0xe5, 0x5c, 0x00, 0x82, 0xe5, 0x1e, 0xff, 0x2f, 0xe1, 0x68, 0x62, 0x1f, 0xe5,
    0x09, 0x00, 0xa0, 0xe1, 0x04, 0x10, 0xa0, 0xe3, 0x06, 0x20, 0xa0, 0xe1, 0x00, 0x30, 0xa0, 0xe3,
    0x00, 0x40, 0xa0, 0xe3, 0x13, 0x70, 0x01, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x04, 0x00, 0x50, 0xe3,
    0x8d, 0xff, 0xff, 0x1a, 0x01, 0x60, 0x56, 0xe2, 0xf4, 0xff, 0xff, 0x1a, 0x86, 0xff, 0xff, 0xea,
    0x09, 0x00, 0xa0, 0xe1, 0x00, 0x10, 0xe0, 0xe3, 0x00, 0x20, 0xa0, 0xe3, 0x00, 0x30, 0xa0, 0xe3,
    0x00, 0x40, 0xa0, 0xe3, 0x13, 0x70, 0x01, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x04, 0x00, 0x50, 0xe3,
    0x81, 0xff, 0xff, 0x1a, 0xc0, 0x62, 0x1f, 0xe5, 0x09, 0x00, 0xa0, 0xe1, 0x04, 0x10, 0xa0, 0xe3,
    0x06, 0x20, 0xa0, 0xe1, 0x00, 0x30, 0xa0, 0xe3, 0x00, 0x40, 0xa0, 0xe3, 0x13, 0x70, 0x01, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x01, 0x60, 0x56, 0xe2, 0x73, 0xff, 0xff, 0x0a, 0x04, 0x00, 0x50, 0xe3,
    0x75, 0xff, 0xff, 0x1a, 0xf3, 0xff, 0xff, 0xea, 0xf4, 0x62, 0x1f, 0xe5, 0xbd, 0x0f, 0x4f, 0xe2,
    0x00, 0x10, 0xa0, 0xe3, 0xa2, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3,
    0x6d, 0xff, 0xff, 0x1a, 0x01, 0x60, 0x56, 0xe2, 0xf7, 0xff, 0xff, 0x1a, 0x66, 0xff, 0xff, 0xea,
    0x00, 0x24, 0x01, 0x34, 0xf1, 0xee, 0x10, 0x0a, 0x70, 0x47, 0x00, 0xbf, 0x00, 0x24, 0x00, 0x2c,
    0x06, 0xbf, 0xf1, 0xee, 0x10, 0x0a, 0x01, 0x34, 0x02, 0x34, 0x70, 0x47, 0x01, 0xde, 0x70, 0x47,
    0xf0, 0xf7, 0x01, 0xa0, 0x70, 0x47, 0x00, 0xbf,
];

/// The lazy-VFP fixture B: the record while `EN` is clear, fork, migration.
/// Mode `g` gets its pattern into its record by a signal frame and reads it
/// with its first VFP instruction; `h` takes a second frame, which must
/// carry it, and forks a child whose first use must read it; `k` forks
/// after its own first use; `v` naps with its pattern while the check moves
/// it between processors. Patched at 4 (mode), 8 (pattern), 16 (`FPSCR`),
/// 20 (rounds).
///
/// ```text
/// @ Lazy-VFP fixture B for OPAQUE-KERNEL.md 9.15. Patched by the kernel:
/// @  4 mode, 8 own pattern (8 bytes), 16 own FPSCR, 20 rounds.
/// @ r10 is 512 bytes of stack: r10+0 the flag the SIGUSR1 handler keeps,
/// @ r10+16 the 256 bytes d0-d31 are written and read through, r10+288 a
/// @ sigaction.
///         .syntax unified
///         .arm
///         .fpu neon-vfpv4
///         .text
///         .globl _start
/// _start: b     start
/// mode:   .byte '?', 0, 0, 0
/// own:    .word 0, 0
/// fpscr:  .word 0
/// rounds: .word 0
/// nap:    .word 0, 20000000
/// start:
///         sub   sp, sp, #512
///         bic   sp, sp, #7
///         mov   r10, sp
///         ldrb  r11, mode
///         mov   r0, #0
///         str   r0, [r10]
///         cmp   r11, #'v'
///         beq   migrate
///         cmp   r11, #'k'
///         beq   bitfork
/// @ 'g' and 'h': a program with no use of VFP gets its pattern into its
/// @ record through a signal frame and sigreturn.
///         add   r1, r10, #288
///         adr   r0, usr_handler
///         str   r0, [r1]
///         mov   r0, #4
///         str   r0, [r1, #4]
///         mov   r0, #0
///         str   r0, [r1, #8]
///         str   r0, [r1, #12]
///         str   r0, [r1, #16]
///         mov   r0, #10
///         mov   r2, #0
///         mov   r3, #8
///         mov   r7, #174
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bl    raise
///         cmp   r11, #'h'
///         beq   second
/// @ 'g': its first VFP instruction reads the record (52 if not).
///         bl    check_own
///         cmp   r0, #0
///         movne r0, #52
///         b     exit
/// @ 'h': a second frame carries the record (50 if not), and a fork child's
/// @ first VFP instruction reads it (51 if not).
/// second: bl    raise
///         ldr   r0, [r10]
///         cmp   r0, #3
///         movne r0, #50
///         bne   exit
///         mov   r8, #51
///         b     forked
/// @ 'k': its pattern by its first use, then a fork child, switched out and
/// @ in, reads it (53 if not).
/// bitfork:
///         bl    set_pattern
///         mov   r8, #53
/// @ Fork; the child naps, then reads its whole state (r8 if not its own); the
/// @ parent exits with the child's status.
/// forked: mov   r7, #2
///         svc   #0
///         cmp   r0, #0
///         blt   fail
///         beq   child
///         sub   sp, sp, #8
///         mov   r1, sp
///         mov   r2, #0
///         mov   r3, #0
///         mov   r7, #114
///         svc   #0
///         cmp   r0, #0
///         blt   fail
///         ldr   r0, [sp]
///         lsr   r0, r0, #8
///         and   r0, r0, #255
///         b     exit
/// child:  bl    nap_once
///         bl    check_own
///         cmp   r0, #0
///         movne r0, r8
///         b     exit
///
/// @ 'v': its pattern by its first use, then naps, reading it whole after each
/// @ (70 if not): the kernel moves it between processors meanwhile.
/// migrate:
///         bl    set_pattern
///         ldr   r6, rounds
/// 1:      bl    nap_once
///         bl    check_own
///         cmp   r0, #0
///         movne r0, #70
///         bne   exit
///         subs  r6, r6, #1
///         bne   1b
///         b     pass
///
/// pass:   mov   r0, #0
/// exit:   mov   r7, #248
///         svc   #0
///         udf   #0
/// fail:   mov   r0, #10
///         b     exit
///
/// nap_once:
///         adr   r0, nap
///         mov   r1, #0
///         mov   r7, #162
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bx    lr
///
/// @ kill(getpid(), SIGUSR1): the handler runs on the way out.
/// raise:  mov   r7, #20
///         svc   #0
///         mov   r1, #10
///         mov   r7, #37
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bx    lr
///
/// @ Every D register this program's pattern, and FPSCR its own.
/// set_pattern:
///         ldr   r2, own
///         ldr   r3, own + 4
///         add   r0, r10, #16
///         mov   r1, #32
/// 2:      strd  r2, r3, [r0], #8
///         subs  r1, r1, #1
///         bne   2b
///         add   r0, r10, #16
///         vldmia r0!, {d0-d15}
///         vldmia r0, {d16-d31}
///         ldr   r0, fpscr
///         vmsr  fpscr, r0
///         bx    lr
///
/// @ FPSCR and every D register its own: r0 0, else 1.
/// check_own:
///         vmrs  r0, fpscr
///         ldr   r1, fpscr
///         cmp   r0, r1
///         movne r0, #1
///         bxne  lr
///         add   r0, r10, #16
///         vstmia r0!, {d0-d15}
///         vstmia r0, {d16-d31}
///         add   r12, r10, #16
///         ldr   r2, own
///         ldr   r3, own + 4
///         mov   r1, #32
/// 3:      ldm   r12!, {r0, r9}
///         cmp   r0, r2
///         cmpeq r9, r3
///         movne r0, #1
///         bxne  lr
///         subs  r1, r1, #1
///         bne   3b
///         mov   r0, #0
///         bx    lr
///
/// @ SIGUSR1 with SA_SIGINFO, never touching VFP. First (flag 0): write the
/// @ pattern and FPSCR into the frame's VFP record, flag 1. Second (flag 1):
/// @ flag 3 if the frame carries them, else 4. The record is at 232 in the
/// @ ucontext r2 points to: d0 at 240, FPSCR at 496.
/// usr_handler:
///         push  {r4-r8}
///         ldr   r3, [r2, #72]
///         ldr   r4, own
///         ldr   r5, own + 4
///         ldr   r6, fpscr
///         add   r12, r2, #240
///         mov   r7, #32
///         ldr   r8, [r3]
///         cmp   r8, #0
///         bne   5f
/// 4:      strd  r4, r5, [r12], #8
///         subs  r7, r7, #1
///         bne   4b
///         str   r6, [r2, #496]
///         mov   r8, #1
///         b     7f
/// 5:      mov   r8, #3
/// 6:      ldrd  r0, r1, [r12], #8
///         cmp   r0, r4
///         cmpeq r1, r5
///         movne r8, #4
///         subs  r7, r7, #1
///         bne   6b
///         ldr   r1, [r2, #496]
///         cmp   r1, r6
///         movne r8, #4
/// 7:      str   r8, [r3]
///         pop   {r4-r8}
///         bx    lr
/// ```
const LAZY_B: &[u8] = &[
    0x06, 0x00, 0x00, 0xea, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2d, 0x31, 0x01,
    0x02, 0xdc, 0x4d, 0xe2, 0x07, 0xd0, 0xcd, 0xe3, 0x0d, 0xa0, 0xa0, 0xe1, 0x30, 0xb0, 0x5f, 0xe5,
    0x00, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x8a, 0xe5, 0x76, 0x00, 0x5b, 0xe3, 0x37, 0x00, 0x00, 0x0a,
    0x6b, 0x00, 0x5b, 0xe3, 0x1d, 0x00, 0x00, 0x0a, 0x12, 0x1e, 0x8a, 0xe2, 0x1d, 0x0e, 0x8f, 0xe2,
    0x00, 0x00, 0x81, 0xe5, 0x04, 0x00, 0xa0, 0xe3, 0x04, 0x00, 0x81, 0xe5, 0x00, 0x00, 0xa0, 0xe3,
    0x08, 0x00, 0x81, 0xe5, 0x0c, 0x00, 0x81, 0xe5, 0x10, 0x00, 0x81, 0xe5, 0x0a, 0x00, 0xa0, 0xe3,
    0x00, 0x20, 0xa0, 0xe3, 0x08, 0x30, 0xa0, 0xe3, 0xae, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0x33, 0x00, 0x00, 0x1a, 0x3b, 0x00, 0x00, 0xeb, 0x68, 0x00, 0x5b, 0xe3,
    0x03, 0x00, 0x00, 0x0a, 0x4d, 0x00, 0x00, 0xeb, 0x00, 0x00, 0x50, 0xe3, 0x34, 0x00, 0xa0, 0x13,
    0x29, 0x00, 0x00, 0xea, 0x34, 0x00, 0x00, 0xeb, 0x00, 0x00, 0x9a, 0xe5, 0x03, 0x00, 0x50, 0xe3,
    0x32, 0x00, 0xa0, 0x13, 0x24, 0x00, 0x00, 0x1a, 0x33, 0x80, 0xa0, 0xe3, 0x01, 0x00, 0x00, 0xea,
    0x35, 0x00, 0x00, 0xeb, 0x35, 0x80, 0xa0, 0xe3, 0x02, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0x1f, 0x00, 0x00, 0xba, 0x0b, 0x00, 0x00, 0x0a, 0x08, 0xd0, 0x4d, 0xe2,
    0x0d, 0x10, 0xa0, 0xe1, 0x00, 0x20, 0xa0, 0xe3, 0x00, 0x30, 0xa0, 0xe3, 0x72, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0x16, 0x00, 0x00, 0xba, 0x00, 0x00, 0x9d, 0xe5,
    0x20, 0x04, 0xa0, 0xe1, 0xff, 0x00, 0x00, 0xe2, 0x0f, 0x00, 0x00, 0xea, 0x13, 0x00, 0x00, 0xeb,
    0x2e, 0x00, 0x00, 0xeb, 0x00, 0x00, 0x50, 0xe3, 0x08, 0x00, 0xa0, 0x11, 0x0a, 0x00, 0x00, 0xea,
    0x1d, 0x00, 0x00, 0xeb, 0x18, 0x61, 0x1f, 0xe5, 0x0c, 0x00, 0x00, 0xeb, 0x27, 0x00, 0x00, 0xeb,
    0x00, 0x00, 0x50, 0xe3, 0x46, 0x00, 0xa0, 0x13, 0x03, 0x00, 0x00, 0x1a, 0x01, 0x60, 0x56, 0xe2,
    0xf8, 0xff, 0xff, 0x1a, 0xff, 0xff, 0xff, 0xea, 0x00, 0x00, 0xa0, 0xe3, 0xf8, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0xf0, 0x00, 0xf0, 0xe7, 0x0a, 0x00, 0xa0, 0xe3, 0xfa, 0xff, 0xff, 0xea,
    0x15, 0x0e, 0x4f, 0xe2, 0x00, 0x10, 0xa0, 0xe3, 0xa2, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0xf7, 0xff, 0xff, 0x1a, 0x1e, 0xff, 0x2f, 0xe1, 0x14, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x0a, 0x10, 0xa0, 0xe3, 0x25, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0xef, 0xff, 0xff, 0x1a, 0x1e, 0xff, 0x2f, 0xe1, 0x9c, 0x21, 0x1f, 0xe5,
    0x9c, 0x31, 0x1f, 0xe5, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x10, 0xa0, 0xe3, 0xf8, 0x20, 0xc0, 0xe0,
    0x01, 0x10, 0x51, 0xe2, 0xfc, 0xff, 0xff, 0x1a, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x0b, 0xb0, 0xec,
    0x20, 0x0b, 0xd0, 0xec, 0xbc, 0x01, 0x1f, 0xe5, 0x10, 0x0a, 0xe1, 0xee, 0x1e, 0xff, 0x2f, 0xe1,
    0x10, 0x0a, 0xf1, 0xee, 0xcc, 0x11, 0x1f, 0xe5, 0x01, 0x00, 0x50, 0xe1, 0x01, 0x00, 0xa0, 0x13,
    0x1e, 0xff, 0x2f, 0x11, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x0b, 0xa0, 0xec, 0x20, 0x0b, 0xc0, 0xec,
    0x10, 0xc0, 0x8a, 0xe2, 0xf4, 0x21, 0x1f, 0xe5, 0xf4, 0x31, 0x1f, 0xe5, 0x20, 0x10, 0xa0, 0xe3,
    0x01, 0x02, 0xbc, 0xe8, 0x02, 0x00, 0x50, 0xe1, 0x03, 0x00, 0x59, 0x01, 0x01, 0x00, 0xa0, 0x13,
    0x1e, 0xff, 0x2f, 0x11, 0x01, 0x10, 0x51, 0xe2, 0xf8, 0xff, 0xff, 0x1a, 0x00, 0x00, 0xa0, 0xe3,
    0x1e, 0xff, 0x2f, 0xe1, 0xf0, 0x01, 0x2d, 0xe9, 0x48, 0x30, 0x92, 0xe5, 0x2c, 0x42, 0x1f, 0xe5,
    0x2c, 0x52, 0x1f, 0xe5, 0x2c, 0x62, 0x1f, 0xe5, 0xf0, 0xc0, 0x82, 0xe2, 0x20, 0x70, 0xa0, 0xe3,
    0x00, 0x80, 0x93, 0xe5, 0x00, 0x00, 0x58, 0xe3, 0x05, 0x00, 0x00, 0x1a, 0xf8, 0x40, 0xcc, 0xe0,
    0x01, 0x70, 0x57, 0xe2, 0xfc, 0xff, 0xff, 0x1a, 0xf0, 0x61, 0x82, 0xe5, 0x01, 0x80, 0xa0, 0xe3,
    0x09, 0x00, 0x00, 0xea, 0x03, 0x80, 0xa0, 0xe3, 0xd8, 0x00, 0xcc, 0xe0, 0x04, 0x00, 0x50, 0xe1,
    0x05, 0x00, 0x51, 0x01, 0x04, 0x80, 0xa0, 0x13, 0x01, 0x70, 0x57, 0xe2, 0xf9, 0xff, 0xff, 0x1a,
    0xf0, 0x11, 0x92, 0xe5, 0x06, 0x00, 0x51, 0xe1, 0x04, 0x80, 0xa0, 0x13, 0x00, 0x80, 0x83, 0xe5,
    0xf0, 0x01, 0xbd, 0xe8, 0x1e, 0xff, 0x2f, 0xe1,
];

/// The lazy-VFP fixture C: `execve`. Mode `x` gets its pattern into its
/// record by a signal frame and `execve`s a copy of itself as `z`, whose
/// first VFP instruction must read zero everywhere; `y` takes its pattern by
/// its first use and `execve`s it as `Z`, which naps without VFP while the
/// check reads the registers. Patched as [`LAZY_B`].
///
/// ```text
/// @ Lazy-VFP fixture C for OPAQUE-KERNEL.md 9.15. Patched by the kernel:
/// @  4 mode, 8 own pattern (8 bytes), 16 own FPSCR, 20 rounds.
/// @ r10 is 512 bytes of stack: r10+0 the flag the SIGUSR1 handler keeps,
/// @ r10+16 the 256 bytes d0-d31 are written and read through, r10+288 a
/// @ sigaction.
///         .syntax unified
///         .arm
///         .fpu neon-vfpv4
///         .text
///         .globl _start
/// _start: b     start
/// mode:   .byte '?', 0, 0, 0
/// own:    .word 0, 0
/// fpscr:  .word 0
/// rounds: .word 0
/// nap:    .word 0, 20000000
/// path:   .asciz "/tmp/lazy-exec"
///         .balign 4
/// zname:  .asciz "z"
///         .balign 4
/// bigz:   .asciz "Z"
///         .balign 4
/// start:
///         ldr   r0, [sp, #4]
///         sub   sp, sp, #512
///         bic   sp, sp, #7
///         mov   r10, sp
///         ldrb  r0, [r0]
///         cmp   r0, #'z'
///         beq   zimage
///         cmp   r0, #'Z'
///         beq   naps
///         ldrb  r11, mode
///         mov   r0, #0
///         str   r0, [r10]
///         cmp   r11, #'y'
///         beq   exec_bit
/// @ 'x': a program with no use of VFP gets its pattern into its record
/// @ through a signal frame and sigreturn.
///         add   r1, r10, #288
///         adr   r0, usr_handler
///         str   r0, [r1]
///         mov   r0, #4
///         str   r0, [r1, #4]
///         mov   r0, #0
///         str   r0, [r1, #8]
///         str   r0, [r1, #12]
///         str   r0, [r1, #16]
///         mov   r0, #10
///         mov   r2, #0
///         mov   r3, #8
///         mov   r7, #174
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bl    raise
///         b     exec_record
/// @ 'y': its pattern by its first use, then execve with argv {"Z"}.
/// exec_bit:
///         bl    set_pattern
///         adr   r8, bigz
///         b     exec_image
/// @ 'x': execve with argv {"z"}.
/// exec_record:
///         adr   r8, zname
/// exec_image:
///         adr   r0, path
///         movw  r1, #0x241
///         movw  r2, #0x1ed
///         mov   r7, #5
///         svc   #0
///         cmp   r0, #0
///         blt   fail
///         mov   r5, r0
///         mov   r1, #0x400000
///         movw  r2, #1140
///         mov   r7, #4
///         svc   #0
///         cmp   r0, r2
///         bne   fail
///         mov   r0, r5
///         mov   r1, #4096
///         mov   r2, #0
///         mov   r7, #19
///         svc   #0
///         mov   r0, r5
///         mov   r1, #0x410000
///         mov   r2, #16
///         mov   r7, #4
///         svc   #0
///         cmp   r0, #16
///         bne   fail
///         mov   r0, r5
///         mov   r7, #6
///         svc   #0
///         sub   sp, sp, #16
///         str   r8, [sp]
///         mov   r1, #0
///         str   r1, [sp, #4]
///         str   r1, [sp, #8]
///         adr   r0, path
///         mov   r1, sp
///         add   r2, sp, #8
///         mov   r7, #11
///         svc   #0
///         b     fail
///
/// @ The image 'x' execve'd: its first VFP instruction reads zero everywhere
/// @ and in FPSCR (60 if not).
/// zimage: vmrs  r0, fpscr
///         cmp   r0, #0
///         movne r0, #60
///         bne   exit
///         add   r0, r10, #16
///         vstmia r0!, {d0-d15}
///         vstmia r0, {d16-d31}
///         add   r0, r10, #16
///         mov   r1, #64
/// 1:      ldr   r2, [r0], #4
///         cmp   r2, #0
///         movne r0, #60
///         bne   exit
///         subs  r1, r1, #1
///         bne   1b
///         b     pass
///
/// @ The image 'y' execve'd: never touches VFP, naps 50 times.
/// naps:   mov   r6, #50
/// 1:      bl    nap_once
///         subs  r6, r6, #1
///         bne   1b
/// pass:   mov   r0, #0
/// exit:   mov   r7, #248
///         svc   #0
///         udf   #0
/// fail:   mov   r0, #10
///         b     exit
///
/// nap_once:
///         adr   r0, nap
///         mov   r1, #0
///         mov   r7, #162
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bx    lr
///
/// @ kill(getpid(), SIGUSR1): the handler runs on the way out.
/// raise:  mov   r7, #20
///         svc   #0
///         mov   r1, #10
///         mov   r7, #37
///         svc   #0
///         cmp   r0, #0
///         bne   fail
///         bx    lr
///
/// @ Every D register this program's pattern, and FPSCR its own.
/// set_pattern:
///         ldr   r2, own
///         ldr   r3, own + 4
///         add   r0, r10, #16
///         mov   r1, #32
/// 2:      strd  r2, r3, [r0], #8
///         subs  r1, r1, #1
///         bne   2b
///         add   r0, r10, #16
///         vldmia r0!, {d0-d15}
///         vldmia r0, {d16-d31}
///         ldr   r0, fpscr
///         vmsr  fpscr, r0
///         bx    lr
///
/// @ SIGUSR1 with SA_SIGINFO, never touching VFP. First (flag 0): write the
/// @ pattern and FPSCR into the frame's VFP record, flag 1. Second (flag 1):
/// @ flag 3 if the frame carries them, else 4. The record is at 232 in the
/// @ ucontext r2 points to: d0 at 240, FPSCR at 496.
/// usr_handler:
///         push  {r4-r8}
///         ldr   r3, [r2, #72]
///         ldr   r4, own
///         ldr   r5, own + 4
///         ldr   r6, fpscr
///         add   r12, r2, #240
///         mov   r7, #32
///         ldr   r8, [r3]
///         cmp   r8, #0
///         bne   5f
/// 4:      strd  r4, r5, [r12], #8
///         subs  r7, r7, #1
///         bne   4b
///         str   r6, [r2, #496]
///         mov   r8, #1
///         b     7f
/// 5:      mov   r8, #3
/// 6:      ldrd  r0, r1, [r12], #8
///         cmp   r0, r4
///         cmpeq r1, r5
///         movne r8, #4
///         subs  r7, r7, #1
///         bne   6b
///         ldr   r1, [r2, #496]
///         cmp   r1, r6
///         movne r8, #4
/// 7:      str   r8, [r3]
///         pop   {r4-r8}
///         bx    lr
/// ```
const LAZY_C: &[u8] = &[
    0x0c, 0x00, 0x00, 0xea, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2d, 0x31, 0x01,
    0x2f, 0x74, 0x6d, 0x70, 0x2f, 0x6c, 0x61, 0x7a, 0x79, 0x2d, 0x65, 0x78, 0x65, 0x63, 0x00, 0x00,
    0x7a, 0x00, 0x00, 0x00, 0x5a, 0x00, 0x00, 0x00, 0x04, 0x00, 0x9d, 0xe5, 0x02, 0xdc, 0x4d, 0xe2,
    0x07, 0xd0, 0xcd, 0xe3, 0x0d, 0xa0, 0xa0, 0xe1, 0x00, 0x00, 0xd0, 0xe5, 0x7a, 0x00, 0x50, 0xe3,
    0x44, 0x00, 0x00, 0x0a, 0x5a, 0x00, 0x50, 0xe3, 0x52, 0x00, 0x00, 0x0a, 0x60, 0xb0, 0x5f, 0xe5,
    0x00, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x8a, 0xe5, 0x79, 0x00, 0x5b, 0xe3, 0x11, 0x00, 0x00, 0x0a,
    0x12, 0x1e, 0x8a, 0xe2, 0x71, 0x0f, 0x8f, 0xe2, 0x00, 0x00, 0x81, 0xe5, 0x04, 0x00, 0xa0, 0xe3,
    0x04, 0x00, 0x81, 0xe5, 0x00, 0x00, 0xa0, 0xe3, 0x08, 0x00, 0x81, 0xe5, 0x0c, 0x00, 0x81, 0xe5,
    0x10, 0x00, 0x81, 0xe5, 0x0a, 0x00, 0xa0, 0xe3, 0x00, 0x20, 0xa0, 0xe3, 0x08, 0x30, 0xa0, 0xe3,
    0xae, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0x45, 0x00, 0x00, 0x1a,
    0x4d, 0x00, 0x00, 0xeb, 0x02, 0x00, 0x00, 0xea, 0x53, 0x00, 0x00, 0xeb, 0x90, 0x80, 0x4f, 0xe2,
    0x00, 0x00, 0x00, 0xea, 0x9c, 0x80, 0x4f, 0xe2, 0xb0, 0x00, 0x4f, 0xe2, 0x41, 0x12, 0x00, 0xe3,
    0xed, 0x21, 0x00, 0xe3, 0x05, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3,
    0x38, 0x00, 0x00, 0xba, 0x00, 0x50, 0xa0, 0xe1, 0x01, 0x15, 0xa0, 0xe3, 0x74, 0x24, 0x00, 0xe3,
    0x04, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x02, 0x00, 0x50, 0xe1, 0x31, 0x00, 0x00, 0x1a,
    0x05, 0x00, 0xa0, 0xe1, 0x01, 0x1a, 0xa0, 0xe3, 0x00, 0x20, 0xa0, 0xe3, 0x13, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x05, 0x00, 0xa0, 0xe1, 0x41, 0x18, 0xa0, 0xe3, 0x10, 0x20, 0xa0, 0xe3,
    0x04, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x10, 0x00, 0x50, 0xe3, 0x25, 0x00, 0x00, 0x1a,
    0x05, 0x00, 0xa0, 0xe1, 0x06, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x10, 0xd0, 0x4d, 0xe2,
    0x00, 0x80, 0x8d, 0xe5, 0x00, 0x10, 0xa0, 0xe3, 0x04, 0x10, 0x8d, 0xe5, 0x08, 0x10, 0x8d, 0xe5,
    0x4e, 0x0f, 0x4f, 0xe2, 0x0d, 0x10, 0xa0, 0xe1, 0x08, 0x20, 0x8d, 0xe2, 0x0b, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x17, 0x00, 0x00, 0xea, 0x10, 0x0a, 0xf1, 0xee, 0x00, 0x00, 0x50, 0xe3,
    0x3c, 0x00, 0xa0, 0x13, 0x10, 0x00, 0x00, 0x1a, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x0b, 0xa0, 0xec,
    0x20, 0x0b, 0xc0, 0xec, 0x10, 0x00, 0x8a, 0xe2, 0x40, 0x10, 0xa0, 0xe3, 0x04, 0x20, 0x90, 0xe4,
    0x00, 0x00, 0x52, 0xe3, 0x3c, 0x00, 0xa0, 0x13, 0x07, 0x00, 0x00, 0x1a, 0x01, 0x10, 0x51, 0xe2,
    0xf9, 0xff, 0xff, 0x1a, 0x03, 0x00, 0x00, 0xea, 0x32, 0x60, 0xa0, 0xe3, 0x07, 0x00, 0x00, 0xeb,
    0x01, 0x60, 0x56, 0xe2, 0xfc, 0xff, 0xff, 0x1a, 0x00, 0x00, 0xa0, 0xe3, 0xf8, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0xf0, 0x00, 0xf0, 0xe7, 0x0a, 0x00, 0xa0, 0xe3, 0xfa, 0xff, 0xff, 0xea,
    0x07, 0x0d, 0x4f, 0xe2, 0x00, 0x10, 0xa0, 0xe3, 0xa2, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0xf7, 0xff, 0xff, 0x1a, 0x1e, 0xff, 0x2f, 0xe1, 0x14, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x0a, 0x10, 0xa0, 0xe3, 0x25, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0xef, 0xff, 0xff, 0x1a, 0x1e, 0xff, 0x2f, 0xe1, 0x0c, 0x22, 0x1f, 0xe5,
    0x0c, 0x32, 0x1f, 0xe5, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x10, 0xa0, 0xe3, 0xf8, 0x20, 0xc0, 0xe0,
    0x01, 0x10, 0x51, 0xe2, 0xfc, 0xff, 0xff, 0x1a, 0x10, 0x00, 0x8a, 0xe2, 0x20, 0x0b, 0xb0, 0xec,
    0x20, 0x0b, 0xd0, 0xec, 0x2c, 0x02, 0x1f, 0xe5, 0x10, 0x0a, 0xe1, 0xee, 0x1e, 0xff, 0x2f, 0xe1,
    0xf0, 0x01, 0x2d, 0xe9, 0x48, 0x30, 0x92, 0xe5, 0x48, 0x42, 0x1f, 0xe5, 0x48, 0x52, 0x1f, 0xe5,
    0x48, 0x62, 0x1f, 0xe5, 0xf0, 0xc0, 0x82, 0xe2, 0x20, 0x70, 0xa0, 0xe3, 0x00, 0x80, 0x93, 0xe5,
    0x00, 0x00, 0x58, 0xe3, 0x05, 0x00, 0x00, 0x1a, 0xf8, 0x40, 0xcc, 0xe0, 0x01, 0x70, 0x57, 0xe2,
    0xfc, 0xff, 0xff, 0x1a, 0xf0, 0x61, 0x82, 0xe5, 0x01, 0x80, 0xa0, 0xe3, 0x09, 0x00, 0x00, 0xea,
    0x03, 0x80, 0xa0, 0xe3, 0xd8, 0x00, 0xcc, 0xe0, 0x04, 0x00, 0x50, 0xe1, 0x05, 0x00, 0x51, 0x01,
    0x04, 0x80, 0xa0, 0x13, 0x01, 0x70, 0x57, 0xe2, 0xf9, 0xff, 0xff, 0x1a, 0xf0, 0x11, 0x92, 0xe5,
    0x06, 0x00, 0x51, 0xe1, 0x04, 0x80, 0xa0, 0x13, 0x00, 0x80, 0x83, 0xe5, 0xf0, 0x01, 0xbd, 0xe8,
    0x1e, 0xff, 0x2f, 0xe1,
];

/// Round trips the two programs of C1 make.
const LAZY_TRIPS: u32 = 1_000;

/// The status of a program `SIGILL` ended, as `wait4` reports it.
const KILLED_BY_SIGILL: i32 = 128 + 4;

/// Run every case of Â§9.15, the processor's record of `FPEXC.EN` compared
/// with the register after each.
///
/// # Errors
///
/// The first case that read what it must not.
pub(crate) fn run_lazy() -> Result<LazyReport, &'static str> {
    let mut report = LazyReport::default();
    if super::super::cpu::user_fpu_doubles() != 32 {
        return Ok(report);
    }
    report.ran = true;
    // In this order, so that each control fires with its own case's text:
    // the first use in ARM state before anything that needs it, F-68's
    // case before the Thumb first uses that its sabotage would also break.
    check_bring_up_scrubs()?;
    check_the_first_use(b"a")?;
    check_sigill_names_its_instruction()?;
    check_the_first_use(b"ti")?;
    check_pl0_reads_are_undefined()?;
    report.migrated = check_a_vfp_user_moves_between_processors()?;
    report.trips = check_programs_without_vfp_move_nothing()?;
    check_a_handover_scrubs()?;
    check_the_record_while_en_is_clear()?;
    check_execve_starts_without_vfp()?;
    Ok(report)
}

/// Bring-up's VFP sequence, run over a register file full of another
/// program's values, leaves `EN` clear, the record off and every register
/// and `FPSCR` zero: what firmware leaves there reaches no program (the
/// consultant's L9; QEMU resets the file to zero, so a boot alone shows
/// nothing).
///
/// Verifies: `L.armv7a.24`
fn check_bring_up_scrubs() -> Result<(), &'static str> {
    let mut dirty = UserState::new();
    dirty.set_fp(OTHER_FPSCR, [OTHER_PATTERN; 32]);
    super::masked(|| {
        // SAFETY: (SYSREG) a D32 VFP exists, interrupts are masked, and the
        // check is a kernel thread: no task's state is in the registers (each
        // is saved at its switch out), so filling them takes nobody's.
        unsafe { super::turn_on() };
        // SAFETY: (SYSREG) as above; `EN` was just set.
        unsafe { super::ferrix_user_fpu_restore(&raw const dirty, 1) };
        // SAFETY: (SYSREG) as above; `CPACR` was granted at bring-up.
        let _ = unsafe { super::bring_up_vfp() };
        // Bring-up writes no record (a secondary cannot name its own yet);
        // the check's processor had it on from `turn_on` above, so it is put
        // back to what bring-up left the register as, read from the register.
        super::set_vfp_record(super::fpexc() & super::FPEXC_EN != 0);
        let after = behind_en_masked();
        match after {
            Behind::Set => Err("bring-up left FPEXC.EN set or its record on"),
            Behind::Values => Err("bring-up left values in the VFP registers behind FPEXC.EN"),
            Behind::Zero => Ok(()),
        }
    })
}

/// What a processor's register file holds with `EN` clear.
#[derive(PartialEq, Eq)]
enum Behind {
    /// `EN` is set, or the record disagrees with the register.
    Set,
    /// Something other than zero.
    Values,
    /// Zero in every register and `FPSCR`.
    Zero,
}

/// With interrupts masked: `EN` and the record, then, with `EN` set for the
/// read and the file scrubbed and `EN` cleared again after it, whether every
/// register and `FPSCR` were zero. Leaves I2 holding.
fn behind_en_masked() -> Behind {
    if super::fpexc() & super::FPEXC_EN != 0 || super::vfp_on() {
        return Behind::Set;
    }
    let mut seen = UserState::new();
    // SAFETY: (SYSREG) a D32 VFP exists and interrupts are masked; no task's
    // state is in the registers while `EN` is clear.
    unsafe { super::turn_on() };
    // SAFETY: (SYSREG) as above; `EN` was just set.
    unsafe { super::ferrix_user_fpu_save(&raw mut seen, 1) };
    // SAFETY: (SYSREG) as above; scrubbed again and `EN` cleared, so I2 holds
    // after the read.
    unsafe { super::scrub_off(32) };
    if seen.fp() == (0, [0; 32]) {
        Behind::Zero
    } else {
        Behind::Values
    }
}

/// [`behind_en_masked`] on processor `cpu`, which the check runs on.
fn behind_en(cpu: usize) -> Result<Behind, &'static str> {
    super::masked(|| {
        let here = crate::smp::this_cpu()
            .ok_or("the per-CPU register is not installed")?
            .logical;
        if here != cpu {
            return Err("the lazy check moved off the processor its programs are pinned to");
        }
        Ok(behind_en_masked())
    })
}

/// The record of `FPEXC.EN` equals the register on this processor (I1, D2).
fn record_matches() -> Result<(), &'static str> {
    let matches = super::masked(|| (super::fpexc() & super::FPEXC_EN != 0) == super::vfp_on());
    if matches {
        Ok(())
    } else {
        Err("a processor's record of FPEXC.EN disagreed with the register")
    }
}

/// Clear `EN` on this processor, scrubbing first if it was set, so a case
/// starts from a core no program's VFP state is on.
fn start_clear() {
    super::masked(|| {
        if super::vfp_on() {
            // SAFETY: (SYSREG) a D32 VFP exists, `EN` is set, interrupts are
            // masked, and no task's state is only in the registers.
            unsafe { super::scrub_off(32) };
        }
    });
}

/// A program with VFP state, unpinned, naps with its pattern while a
/// program that keeps the processor it slept on busy is pinned there, so
/// that it wakes on the other; it reads its whole state after every nap.
/// Answers whether it ran on two processors: the case did not run unless
/// it did (the consultant's L10).
///
/// Verifies: `L.armv7a.26`
fn check_a_vfp_user_moves_between_processors() -> Result<bool, &'static str> {
    if crate::smp::topology().map_or(0, crate::smp::Topology::online) < 2 {
        return Ok(false);
    }
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let victim = lazy_b(b'v', VICTIM_PATTERN, VICTIM_FPSCR, 20)?;
    let task = process::start_on(&victim, None)
        .map_err(|_| "the migrating program could not be started")?;
    wait_until(deadline, "the migrating program never napped", || {
        task.is_blocked()
    })?;
    let slept_on = task.cpu();
    let hog = tls_program(b't', TP_SECOND, RW_SECOND, 0, 1_000_000)?;
    let hog_task = process::start_on(&hog, Some(slept_on))
        .map_err(|_| "the program that keeps a processor busy could not be started")?;
    let status = victim.wait_for_exit(deadline);
    process::kill(&hog, KILLED_STATUS);
    let _ = hog.wait_for_exit(deadline);
    drop(hog_task);
    match status {
        Some(0) => {}
        Some(70) => {
            return Err(
                "a program with VFP state resumed on another processor did not get its own VFP state",
            );
        }
        Some(KILLED_BY_SIGILL) => {
            return Err("a program with VFP state ran with FPEXC.EN clear");
        }
        status => lazy_verdict(status)?,
    }
    record_matches()?;
    Ok(task.cpus_run_on().count_ones() >= 2)
}

/// Two native programs that never touch VFP make [`LAZY_TRIPS`] round trips
/// through `channel_write_read`, pinned to one processor, and two more
/// trade it by `sched_yield`: no switch moved a VFP register or wrote
/// `FPEXC` for any of them, counted per task (the consultant's L7), and the
/// record still equals the register.
///
/// Verifies: `L.armv7a.21`
fn check_programs_without_vfp_move_nothing() -> Result<u32, &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let (one, other) = Endpoint::pair().map_err(|_| "could not make a channel")?;
    let client = lazy_a(b'c', LAZY_TRIPS)?;
    let server = lazy_a(b's', LAZY_TRIPS)?;
    give_channel(&client, one)?;
    give_channel(&server, other)?;
    let first = tls_program(b't', TP_FIRST, RW_FIRST, 0, TLS_ROUNDS)?;
    let second = tls_program(b't', TP_SECOND, RW_SECOND, 0, TLS_ROUNDS)?;
    start_clear();
    let mut tasks = alloc::vec::Vec::with_capacity(4);
    for program in [&server, &client, &first, &second] {
        tasks.push(
            process::start_on(program, Some(cpu))
                .map_err(|_| "a program without VFP could not be started")?,
        );
    }
    for program in [&server, &client] {
        lazy_verdict(program.wait_for_exit(deadline))?;
    }
    for program in [&first, &second] {
        tls_verdict(
            program.wait_for_exit(deadline),
            "two programs without VFP trading one core did not each read their own TPIDRURO",
            "two programs without VFP trading one core did not each read their own TPIDRURW",
        )?;
    }
    for task in &tasks {
        wait_until(deadline, "a program without VFP never finished", || {
            task.is_dead()
        })?;
        // SAFETY: (SHARED) the task is dead: no switch saves or restores its
        // state again (`switch_user_state` saves no dead task, and a dead task
        // is never chosen), so nothing writes the record this reads.
        let state = unsafe { task.user_state() }.map(|state| unsafe { &*state });
        let Some(state) = state else {
            return Err("a program without VFP had no user state");
        };
        if state.uses_vfp() || state.vfp_moves() != 0 {
            println!(
                "  lazy     {} VFP moves for a program that never used VFP",
                state.vfp_moves()
            );
            return Err("moved VFP state for programs that never used VFP");
        }
    }
    record_matches()?;
    Ok(LAZY_TRIPS)
}

/// Give `process` the channel end `end` as its first handle.
fn give_channel(process: &Arc<Process>, end: Arc<Endpoint>) -> Result<(), &'static str> {
    let placed = process
        .with_handles(|table| table.insert(Object::Channel(end), Rights::CHANNEL))
        .map_err(|_| "no room for a lazy program's channel")?;
    if placed == BOOTSTRAP {
        Ok(())
    } else {
        Err("a fresh process's first handle is not the one the lazy program was built for")
    }
}

/// An ARM `udf`, a 16-bit Thumb `udf` and a 32-bit Thumb `udf.w`, each in a
/// program that has used VFP, end in `SIGILL` whose `si_addr` and saved `pc`
/// name the instruction itself (F-68, the consultant's L5).
///
/// Verifies: `L.armv7a.23`
fn check_sigill_names_its_instruction() -> Result<(), &'static str> {
    let program = lazy_a(b'u', 0)?;
    let [status] = run_pinned([&program])?;
    lazy_verdict(status)
}

/// A program's first VFP instruction, run after a program that filled every
/// register: in ARM state (`a`), in Thumb state (`t`) and inside an IT block
/// (`i`) it is taken and re-run, and reads zero in `FPSCR` and every
/// register.
///
/// Verifies: `L.armv7a.22`
fn check_the_first_use(modes: &[u8]) -> Result<(), &'static str> {
    for &mode in modes {
        let status = beside_a_vfp_user(&lazy_a(mode, 0)?)?;
        lazy_verdict(status)?;
    }
    record_matches()
}

/// After its first use, a PL0 read of `FPEXC`, `FPSID`, `MVFR0` or `MVFR1`
/// ends the program with `SIGILL`: no program reads them (§9.15, S2; shown
/// under QEMU, argued for the board).
///
/// Verifies: `L.armv7a.22`
fn check_pl0_reads_are_undefined() -> Result<(), &'static str> {
    for &mode in b"efmn" {
        let [status] = run_pinned([&lazy_a(mode, 0)?])?;
        match status {
            Some(KILLED_BY_SIGILL) => {}
            Some(30) => return Err("a program read FPEXC, FPSID or MVFR at PL0"),
            status => lazy_verdict(status)?,
        }
    }
    Ok(())
}

/// Run `program` pinned here after a program that fills every VFP register
/// with [`OTHER_PATTERN`] and yields until it is killed.
fn beside_a_vfp_user(program: &Arc<Process>) -> Result<Option<i32>, &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let other = vector_program(b'b', OTHER_PATTERN, VICTIM_PATTERN, OTHER_FPSCR)?;
    let other_task = process::start_on(&other, Some(cpu))
        .map_err(|_| "the program that fills the VFP registers could not be started")?;
    wait_until(
        deadline,
        "the program that fills the VFP registers never ran",
        || other_task.switches() >= 2,
    )?;
    let task =
        process::start_on(program, Some(cpu)).map_err(|_| "a lazy program could not be started")?;
    let status = program.wait_for_exit(deadline);
    process::kill(&other, KILLED_STATUS);
    let _ = other.wait_for_exit(deadline);
    drop(task);
    Ok(status)
}

/// A program with VFP state fills every register and dies; a program that
/// never uses VFP is switched in on that processor and naps; the check,
/// with interrupts masked, finds `EN` clear and every register and `FPSCR`
/// zero behind it (I2).
///
/// Verifies: `L.armv7a.24`
fn check_a_handover_scrubs() -> Result<(), &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let other = vector_program(b'b', OTHER_PATTERN, VICTIM_PATTERN, OTHER_FPSCR)?;
    let other_task = process::start_on(&other, Some(cpu))
        .map_err(|_| "the program that fills the VFP registers could not be started")?;
    wait_until(
        deadline,
        "the program that fills the VFP registers never ran",
        || other_task.switches() >= 2,
    )?;
    process::kill(&other, KILLED_STATUS);
    let _ = other.wait_for_exit(deadline);
    wait_until(
        deadline,
        "the program that fills the VFP registers never died",
        || other_task.is_dead(),
    )?;
    let quiet = lazy_a(b'q', 50)?;
    let quiet_task = process::start_on(&quiet, Some(cpu))
        .map_err(|_| "the program without VFP could not be started")?;
    wait_until(deadline, "the program without VFP never napped", || {
        quiet_task.is_blocked()
    })?;
    let behind = behind_en(cpu)?;
    process::kill(&quiet, KILLED_STATUS);
    let _ = quiet.wait_for_exit(deadline);
    match behind {
        Behind::Zero => Ok(()),
        Behind::Values => Err(
            "a program that never used VFP ran with another program's VFP registers behind FPEXC.EN",
        ),
        Behind::Set => Err("FPEXC.EN was set while a program that never used VFP ran"),
    }
}

/// The record is a program's VFP state while `EN` is clear (D6): a signal
/// frame's state reaches the program's first use (`g`); a second frame and
/// a fork child's first use carry it (`h`, the consultant's L8); a fork
/// child of a program that has used VFP reads its parent's registers (`k`).
///
/// Verifies: `L.armv7a.25`
fn check_the_record_while_en_is_clear() -> Result<(), &'static str> {
    for &mode in b"ghk" {
        let program = lazy_b(mode, VICTIM_PATTERN, VICTIM_FPSCR, 0)?;
        let [status] = run_pinned([&program])?;
        lazy_verdict(status)?;
    }
    record_matches()
}

/// `execve` (D7, the consultant's L4): an image `execve`d by a program whose
/// record held a signal frame's state reads zero at its first use (`x`); one
/// `execve`d by a program with VFP state in the registers starts with `EN`
/// clear and zero behind it (`y`).
///
/// Verifies: `L.armv7a.24`, `L.armv7a.25`
fn check_execve_starts_without_vfp() -> Result<(), &'static str> {
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let [status] = run_pinned([&lazy_c(b'x', VICTIM_PATTERN, VICTIM_FPSCR, 0)?])?;
    lazy_verdict(status)?;
    let execer = lazy_c(b'y', OTHER_PATTERN, OTHER_FPSCR, 0)?;
    let task = process::start_on(&execer, Some(cpu))
        .map_err(|_| "the execve program could not be started")?;
    // The image it gave naps 50 times 20 ms; the copy and the execve before
    // take a fraction of the first.
    crate::sched::sleep_for(100_000_000);
    wait_until(deadline, "the execve'd image never napped", || {
        task.is_blocked()
    })?;
    let behind = behind_en(cpu)?;
    let status = execer.wait_for_exit(deadline);
    match behind {
        Behind::Zero => {}
        Behind::Values => {
            return Err(
                "a program that never used VFP ran with another program's VFP registers behind FPEXC.EN after an execve",
            );
        }
        Behind::Set => {
            return Err("FPEXC.EN was set while an execve'd image that never used VFP ran");
        }
    }
    lazy_verdict(status)?;
    record_matches()
}

/// What a lazy program's status says.
fn lazy_verdict(status: Option<i32>) -> Result<(), &'static str> {
    match status {
        Some(0) => Ok(()),
        Some(KILLED_BY_SIGILL) => Err("a program's first VFP instruction ended it with SIGILL"),
        Some(10) => Err("a call of a lazy program failed"),
        Some(20 | 21) => Err("a program's first VFP instruction did not read zero everywhere"),
        Some(22) => Err("a Thumb program's first VFP instruction did not run as written"),
        Some(23) => Err("a program's first VFP instructions did not compute their sum"),
        Some(24) => {
            Err("a Thumb program's first VFP instruction inside an IT block lost its IT state")
        }
        Some(41) => Err("an ARM program's SIGILL named the wrong instruction"),
        Some(42 | 43) => Err("a Thumb program's SIGILL named the wrong instruction"),
        Some(50 | 51) => Err(
            "a signal frame or fork child did not carry the VFP state of a program that had not used VFP",
        ),
        Some(52) => Err("a signal frame's VFP state did not reach a program that had not used VFP"),
        Some(53) => Err("a fork child did not inherit its parent's VFP registers"),
        Some(60) => {
            Err("an execve'd image's first VFP instruction read the old program's VFP state")
        }
        Some(status) => {
            println!("  lazy     a lazy program ended with {status}");
            Err("a lazy program ended with a status it has no meaning for")
        }
        None => Err("a lazy program never ended"),
    }
}

/// A copy of [`LAZY_A`] in `mode`, loaded and not started.
fn lazy_a(mode: u8, rounds: u32) -> Result<Arc<Process>, &'static str> {
    let mut code = LAZY_A.to_vec();
    if code.get(4) != Some(&b'?') {
        return Err("lazy fixture A's mode byte is not where its layout says");
    }
    patch(&mut code, 4, &[mode])?;
    patch(&mut code, 8, &BOOTSTRAP.0.to_le_bytes())?;
    patch(&mut code, 12, &rounds.to_le_bytes())?;
    load(b"/lazy-a", &code)
}

/// A copy of [`LAZY_B`] in `mode`, loaded and not started.
fn lazy_b(mode: u8, pattern: u64, fpscr: u32, rounds: u32) -> Result<Arc<Process>, &'static str> {
    lazy_bc(LAZY_B, b"/lazy-b", mode, pattern, fpscr, rounds)
}

/// A copy of [`LAZY_C`] in `mode`, loaded and not started.
fn lazy_c(mode: u8, pattern: u64, fpscr: u32, rounds: u32) -> Result<Arc<Process>, &'static str> {
    lazy_bc(LAZY_C, b"/lazy-c", mode, pattern, fpscr, rounds)
}

/// [`LAZY_B`] or [`LAZY_C`], patched.
fn lazy_bc(
    fixture: &[u8],
    name: &[u8],
    mode: u8,
    pattern: u64,
    fpscr: u32,
    rounds: u32,
) -> Result<Arc<Process>, &'static str> {
    let mut code = fixture.to_vec();
    if code.get(4) != Some(&b'?') {
        return Err("a lazy fixture's mode byte is not where its layout says");
    }
    patch(&mut code, 4, &[mode])?;
    patch(&mut code, 8, &pattern.to_le_bytes())?;
    patch(&mut code, 16, &fpscr.to_le_bytes())?;
    patch(&mut code, 20, &rounds.to_le_bytes())?;
    load(name, &code)
}
