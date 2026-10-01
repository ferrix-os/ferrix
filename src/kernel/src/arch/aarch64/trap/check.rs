//! A program's own exceptions end it with the signal Linux gives each.
//!
//! Verification, not the handlers: a file of its own so that the manifest
//! counts it as the test it is (`tools/common/data/certification-item.json`,
//! `test_file_patterns`). Each case is a real program, built into an ELF and
//! run by the Linux personality's loader, because only an instruction a
//! program executes at EL0 takes the path under test: the vector stub,
//! `classify`, the generic dispatcher's user fault and `fault_signal`, and
//! the kill that ends the program with the signal. `super::super::check`
//! drives the decoder with every syndrome from built frames; this is the
//! same decoding reached the way a program reaches it, for the cases a
//! program can raise on any core.
//!
//! Before this, the one program the boot ended by a fault wrote through a
//! null pointer (`object/check.rs`), so a misaligned program counter, an
//! undefined instruction and a `brk` a program executes itself had never
//! been taken from EL0.

use ferrix_linux_abi::types::{SIGBUS, SIGILL, SIGTRAP};

use crate::console::println;

/// `br` to an address two bytes into this program: the next fetch is from a
/// misaligned program counter, exception class `0b100010`.
///
/// ```text
///   adr x0, . ; add x0, x0, #2 ; br x0
///   mov x8, #94 ; mov x0, #97 ; svc #0              ; exit_group(97)
/// ```
///
/// Every program here ends with that `exit_group(97)`, which only a program
/// the exception did not end reaches. Encoded by hand from the Arm ARM's
/// tables, one little-endian word per instruction.
const MISALIGNED_PC: &[u8] = &[
    0x00, 0x00, 0x00, 0x10, 0x00, 0x08, 0x00, 0x91, 0x00, 0x00, 0x1f, 0xd6, 0xc8, 0x0b, 0x80, 0xd2,
    0x20, 0x0c, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
];

/// `udf #0`: an unallocated encoding, exception class `0b000000`.
const UNDEFINED: &[u8] = &[
    0x00, 0x00, 0x00, 0x00, 0xc8, 0x0b, 0x80, 0xd2, 0x20, 0x0c, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
];

/// `brk #0` from EL0: exception class `0b111100`, which the kernel steps over
/// when it raised it itself and turns into a signal when a program did.
const BREAKPOINT: &[u8] = &[
    0x00, 0x00, 0x20, 0xd4, 0xc8, 0x0b, 0x80, 0xd2, 0x20, 0x0c, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
];

/// A read of the second page of a shared mapping of a one-page file: a
/// translation fault the address space answers with the file's end rather
/// than a missing mapping, so the dispatcher sends `SIGBUS` itself and not
/// what `fault_signal` would say. Exits with 96 if the file or the mapping
/// could not be made. x86-64's check has the same program.
///
/// ```text
///   adr x0, name ; mov x1, #0 ; mov x8, #279 ; svc #0     ; memfd_create
///   tbnz x0, #63, 1f ; mov x9, x0
///   mov x1, #4096 ; mov x8, #46 ; svc #0 ; cbnz x0, 1f    ; ftruncate to a page
///   mov x0, #0 ; mov x1, #8192 ; mov x2, #1 ; mov x3, #1
///   mov x4, x9 ; mov x5, #0 ; mov x8, #222 ; svc #0       ; two pages, shared
///   cmn x0, #4095 ; b.hs 1f
///   ldr x0, [x0, #4096]                                   ; past the file
///   mov x8, #94 ; mov x0, #97 ; svc #0
/// 1: mov x8, #94 ; mov x0, #96 ; svc #0
/// name: .asciz "bus"
/// ```
///
/// Assembled by GNU `as` for AArch64 and read back out of the object file.
const PAST_END: &[u8] = &[
    0x60, 0x03, 0x00, 0x10, 0x01, 0x00, 0x80, 0xd2, 0xe8, 0x22, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
    0x80, 0x02, 0xf8, 0xb7, 0xe9, 0x03, 0x00, 0xaa, 0x01, 0x00, 0x82, 0xd2, 0xc8, 0x05, 0x80, 0xd2,
    0x01, 0x00, 0x00, 0xd4, 0xe0, 0x01, 0x00, 0xb5, 0x00, 0x00, 0x80, 0xd2, 0x01, 0x00, 0x84, 0xd2,
    0x22, 0x00, 0x80, 0xd2, 0x23, 0x00, 0x80, 0xd2, 0xe4, 0x03, 0x09, 0xaa, 0x05, 0x00, 0x80, 0xd2,
    0xc8, 0x1b, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4, 0x1f, 0xfc, 0x3f, 0xb1, 0xa2, 0x00, 0x00, 0x54,
    0x00, 0x00, 0x48, 0xf9, 0xc8, 0x0b, 0x80, 0xd2, 0x20, 0x0c, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
    0xc8, 0x0b, 0x80, 0xd2, 0x00, 0x0c, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4, 0x62, 0x75, 0x73, 0x00,
];

/// The status of a program a signal ended, as `wait4` reports it to a shell.
const fn killed_by(signal: u32) -> i32 {
    128 + signal as i32
}

/// Run every case, and say what each ended with.
///
/// # Errors
///
/// The first program that could not be run, or ended other than it had to.
///
/// Verifies: L.aarch64.3
pub(crate) fn run() -> Result<(), &'static str> {
    crate::trap::check::outcomes()?;
    let cases: [(&[u8], &[u8], i32, &'static str); 4] = [
        (
            b"/misaligned",
            MISALIGNED_PC,
            killed_by(SIGBUS),
            "a program's misaligned program counter did not end it with SIGBUS",
        ),
        (
            b"/undefined",
            UNDEFINED,
            killed_by(SIGILL),
            "a program's undefined instruction did not end it with SIGILL",
        ),
        (
            b"/breakpoint",
            BREAKPOINT,
            killed_by(SIGTRAP),
            "a program's own breakpoint did not end it with SIGTRAP",
        ),
        (
            b"/past-end",
            PAST_END,
            killed_by(SIGBUS),
            "a program's read past the end of a mapped file did not end it with SIGBUS",
        ),
    ];
    for (name, code, expected, problem) in cases {
        let status = run_program(name, code)?;
        if status != expected {
            println!(
                "  fault    {} ended with {status}, not {expected}",
                core::str::from_utf8(name).unwrap_or("?")
            );
            return Err(problem);
        }
    }
    println!(
        "  fault    a program's misaligned program counter, undefined instruction, own breakpoint \
         and read past a mapped file's end ended it with SIGBUS, SIGILL, SIGTRAP and SIGBUS"
    );
    Ok(())
}

/// Build `code` into a program called `name`, run it, and answer its status.
fn run_program(name: &[u8], code: &[u8]) -> Result<i32, &'static str> {
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf64,
        super::super::ARCH.elf_machine(),
        crate::syscall::image::Shape::Good,
        code,
    );
    crate::syscall::exec::run(&file, &[name], &[], [0x7e; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a program that faults on purpose could not be started")
}

/// Take the entry a system call arrives through, with a frame built from these
/// registers as an `svc` from EL0 leaves them, and answer what the entry left
/// in `x0`.
///
/// For the seccomp check (`syscall::seccomp_check`), which needs the core's own
/// entry run with a call it chose -- the registered filter is asked first, and
/// the early answers (`rt_sigreturn`) follow -- without a program to make it.
/// Interrupts are as the caller had them: a real entry starts with them masked
/// and leaves them so, this wrapper does both for a task that runs with them
/// open. `None` for a mode of entry this architecture does not have.
pub(crate) fn drive_system_call(
    abi: crate::trap::Abi,
    number: usize,
    args: [u64; 6],
    ip: u64,
) -> Option<isize> {
    use ferrix_sync::IrqControl;

    if abi != crate::trap::Abi::Native {
        return None;
    }
    let mut frame = super::TrapFrame {
        x: [0; 31],
        sp: 0x7fff_e000,
        elr: ip,
        spsr: super::USER_SPSR,
        // An `svc` from AArch64, class 0b010101, from a lower exception level.
        esr: 0b01_0101 << 26,
        far: 0,
        kind: 8,
        reserved: 0,
    };
    frame.x[..6].copy_from_slice(&args);
    frame.x[8] = number as u64;
    let saved = <super::super::Irq as IrqControl>::disable();
    let done = super::system_call(&mut frame);
    <super::super::Irq as IrqControl>::restore(saved);
    done.ok()?;
    Some(frame.x[0] as i64 as isize)
}
