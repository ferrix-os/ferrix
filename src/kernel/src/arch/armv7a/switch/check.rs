//! The switch's user state on ARMv7-A, checked with programs in USR mode:
//! the two thread ID registers (`docs/OPAQUE-KERNEL.md` §9.14, 3b and F-66).
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

use alloc::sync::Arc;

use crate::console::println;
use crate::sched::Task;
use crate::syscall::process::{self, Process};

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
