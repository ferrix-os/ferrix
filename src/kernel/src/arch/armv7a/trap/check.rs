//! A program's own exceptions end it with the signal Linux gives each.
//!
//! Verification, not the handlers: a file of its own so that the manifest
//! counts it as the test it is (`tools/common/data/certification-item.json`,
//! `test_file_patterns`). Each case is a real program, built into an ELF and
//! run by the Linux personality's loader, because only an instruction a
//! program executes in USR mode takes the path under test: the vector stub,
//! `classify`, the generic dispatcher's user fault and `fault_signal`, and
//! the kill that ends the program with the signal. `super::super::check`
//! drives the decoder with every vector entry and fault status from built
//! frames; this is the same decoding reached the way a program reaches it,
//! for the cases a program can raise on any core.
//!
//! Before this, the one program the boot ended by a fault wrote through a
//! null pointer (`object/check.rs`), so an undefined instruction, a `bkpt`
//! and an alignment fault a program raises itself had never been taken from
//! USR mode. The last program installs a signal handler without a restorer,
//! whose frame nothing else builds.

use ferrix_linux_abi::types::{SIGBUS, SIGILL, SIGTRAP};

use crate::console::println;

/// `udf #0` in the ARM instruction set: the undefined-instruction vector.
///
/// ```text
///   udf #0
///   mov r7, #248 ; mov r0, #97 ; svc #0             ; exit_group(97)
/// ```
///
/// Every program here ends with that `exit_group(97)`, which only a program
/// the exception did not end reaches. Encoded by hand from the ARM ARM's
/// tables, one little-endian word per instruction.
const UNDEFINED: &[u8] = &[
    0xf0, 0x00, 0xf0, 0xe7, 0xf8, 0x70, 0xa0, 0xe3, 0x61, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
];

/// `bkpt #0`: a prefetch abort with the debug-event status.
const BREAKPOINT: &[u8] = &[
    0x70, 0x00, 0x20, 0xe1, 0xf8, 0x70, 0xa0, 0xe3, 0x61, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
];

/// `ldm` from one byte past the stack pointer: a load-multiple takes an
/// alignment fault at any address that is not a word's, whatever `SCTLR.A`
/// says, and the stack is mapped, so no other fault can come first.
///
/// ```text
///   mov r0, sp ; add r0, r0, #1 ; ldm r0, {r1}
/// ```
const MISALIGNED: &[u8] = &[
    0x0d, 0x00, 0xa0, 0xe1, 0x01, 0x00, 0x80, 0xe2, 0x02, 0x00, 0x90, 0xe8, 0xf8, 0x70, 0xa0, 0xe3,
    0x61, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
];

/// A handler installed without `SA_RESTORER`, which the frame then returns
/// into through the copy of the return sequence it carries on the stack
/// (`super::super::signal`): the handler is entered, and exits with 55.
///
/// ```text
///   sub sp, sp, #24 ; adr r0, handler ; str r0, [sp]       ; sa_handler
///   mov r0, #0 ; str r0, [sp, #4..#16]                      ; no flags, no restorer, empty mask
///   rt_sigaction(SIGUSR1, sp, 0, 8) ; bne fail
///   kill(getpid(), SIGUSR1)
/// fail: exit_group(97)
/// handler: exit_group(55)
/// ```
///
/// Assembled by GNU `as` for ARMv7-A and read back out of the object file.
/// musl, and every program the images carry, installs its handlers with a
/// restorer, so no other program takes this frame.
const NO_RESTORER: &[u8] = &[
    0x18, 0xd0, 0x4d, 0xe2, 0x54, 0x00, 0x8f, 0xe2, 0x00, 0x00, 0x8d, 0xe5, 0x00, 0x00, 0xa0, 0xe3,
    0x04, 0x00, 0x8d, 0xe5, 0x08, 0x00, 0x8d, 0xe5, 0x0c, 0x00, 0x8d, 0xe5, 0x10, 0x00, 0x8d, 0xe5,
    0x0a, 0x00, 0xa0, 0xe3, 0x0d, 0x10, 0xa0, 0xe1, 0x00, 0x20, 0xa0, 0xe3, 0x08, 0x30, 0xa0, 0xe3,
    0xae, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0x04, 0x00, 0x00, 0x1a,
    0x14, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x0a, 0x10, 0xa0, 0xe3, 0x25, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x61, 0x00, 0xa0, 0xe3, 0xf8, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x37, 0x00, 0xa0, 0xe3, 0xf8, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
];

/// A read of the second page of a shared mapping of a one-page file: a
/// translation fault the address space answers with the file's end rather
/// than a missing mapping, so the dispatcher sends `SIGBUS` itself and not
/// what `fault_signal` would say. Exits with 96 if the file or the mapping
/// could not be made. x86-64's check has the same program.
///
/// ```text
///   adr r0, name ; mov r1, #0 ; movw r7, #385 ; svc #0     ; memfd_create
///   cmp r0, #0 ; blt 1f ; mov r4, r0
///   mov r1, #4096 ; mov r7, #93 ; svc #0 ; cmp r0, #0 ; bne 1f ; ftruncate
///   mov r0, #0 ; mov r1, #8192 ; mov r2, #1 ; mov r3, #1
///   mov r5, #0 ; mov r7, #192 ; svc #0                     ; mmap2, two pages
///   cmn r0, #4096 ; bhi 1f
///   add r0, r0, #4096 ; ldr r0, [r0]                       ; past the file
///   mov r7, #248 ; mov r0, #97 ; svc #0
/// 1: mov r7, #248 ; mov r0, #96 ; svc #0
/// name: .asciz "bus"
/// ```
///
/// Assembled by GNU `as` for ARMv7-A and read back out of the object file.
const PAST_END: &[u8] = &[
    0x6c, 0x00, 0x8f, 0xe2, 0x00, 0x10, 0xa0, 0xe3, 0x81, 0x71, 0x00, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x00, 0x50, 0xe3, 0x13, 0x00, 0x00, 0xba, 0x00, 0x40, 0xa0, 0xe1, 0x01, 0x1a, 0xa0, 0xe3,
    0x5d, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0x0d, 0x00, 0x00, 0x1a,
    0x00, 0x00, 0xa0, 0xe3, 0x02, 0x1a, 0xa0, 0xe3, 0x01, 0x20, 0xa0, 0xe3, 0x01, 0x30, 0xa0, 0xe3,
    0x00, 0x50, 0xa0, 0xe3, 0xc0, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x01, 0x0a, 0x70, 0xe3,
    0x04, 0x00, 0x00, 0x8a, 0x01, 0x0a, 0x80, 0xe2, 0x00, 0x00, 0x90, 0xe5, 0xf8, 0x70, 0xa0, 0xe3,
    0x61, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0xf8, 0x70, 0xa0, 0xe3, 0x60, 0x00, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x62, 0x75, 0x73, 0x00,
];

/// Handlers installed without `SA_RESTORER` that return, four ways -- an ARM
/// and a Thumb handler, each plain and with `SA_SIGINFO` -- and the program
/// going on after each (F-48).
///
/// ```text
///   for k in 0..4:
///     rt_sigaction(SIGUSR1, { handler_arm or handler_thumb|1,
///                               SA_SIGINFO if k & 2, no restorer }, 0, 8)
///     r4 = d8 = 0xdeadbeef + k ; kill(getpid(), SIGUSR1)
///     r4 and d8 still 0xdeadbeef + k, or exit_group(60 + k) / (70 + k)
///   exit_group(55)
/// handler_arm:   mov r4, #0 ; vmov d8, r4, r4 ; bx lr
/// handler_thumb: movs r4, #0 ; vmov d8, r4, r4 ; bx lr      (Thumb)
/// ```
///
/// Each handler zeroes `r4` and `d8`, which only `sigreturn` or
/// `rt_sigreturn` puts back from the frame, so finding the markers after
/// `kill` is the proof that the handler returned through a trampoline and the
/// call, and not merely that the program survived. Assembled by GNU `as` for
/// ARMv7-A and read back out of the object file
/// (`~/.local/share/ferrix/f48/returning.s` on example has the source).
const RETURNING: &[u8] = &[
    0x18, 0xd0, 0x4d, 0xe2, 0x00, 0x50, 0xa0, 0xe3, 0x01, 0x00, 0x15, 0xe3, 0xc8, 0x00, 0x8f, 0x12,
    0x01, 0x00, 0x80, 0x13, 0xb4, 0x00, 0x8f, 0x02, 0x00, 0x00, 0x8d, 0xe5, 0x02, 0x00, 0x15, 0xe3,
    0x04, 0x00, 0xa0, 0x13, 0x00, 0x00, 0xa0, 0x03, 0x04, 0x00, 0x8d, 0xe5, 0x00, 0x00, 0xa0, 0xe3,
    0x08, 0x00, 0x8d, 0xe5, 0x0c, 0x00, 0x8d, 0xe5, 0x10, 0x00, 0x8d, 0xe5, 0x0a, 0x00, 0xa0, 0xe3,
    0x0d, 0x10, 0xa0, 0xe1, 0x00, 0x20, 0xa0, 0xe3, 0x08, 0x30, 0xa0, 0xe3, 0xae, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x00, 0x00, 0x50, 0xe3, 0x19, 0x00, 0x00, 0x1a, 0xef, 0x6e, 0x0b, 0xe3,
    0xad, 0x6e, 0x4d, 0xe3, 0x05, 0x60, 0x86, 0xe0, 0x06, 0x40, 0xa0, 0xe1, 0x18, 0x6b, 0x46, 0xec,
    0x14, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef, 0x0a, 0x10, 0xa0, 0xe3, 0x25, 0x70, 0xa0, 0xe3,
    0x00, 0x00, 0x00, 0xef, 0x06, 0x00, 0x54, 0xe1, 0x09, 0x00, 0x00, 0x1a, 0x18, 0x0b, 0x51, 0xec,
    0x06, 0x00, 0x50, 0xe1, 0x08, 0x00, 0x00, 0x1a, 0x06, 0x00, 0x51, 0xe1, 0x06, 0x00, 0x00, 0x1a,
    0x01, 0x50, 0x85, 0xe2, 0x04, 0x00, 0x55, 0xe3, 0xd6, 0xff, 0xff, 0xba, 0x37, 0x00, 0xa0, 0xe3,
    0x04, 0x00, 0x00, 0xea, 0x3c, 0x00, 0x85, 0xe2, 0x02, 0x00, 0x00, 0xea, 0x46, 0x00, 0x85, 0xe2,
    0x00, 0x00, 0x00, 0xea, 0x61, 0x00, 0xa0, 0xe3, 0xf8, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef,
    0x00, 0x40, 0xa0, 0xe3, 0x18, 0x4b, 0x44, 0xec, 0x1e, 0xff, 0x2f, 0xe1, 0x00, 0x24, 0x44, 0xec,
    0x18, 0x4b, 0x70, 0x47,
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
pub(crate) fn run() -> Result<(), &'static str> {
    crate::trap::check::outcomes()?;
    let cases: [(&[u8], &[u8], i32, &'static str); 4] = [
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
            b"/misaligned",
            MISALIGNED,
            killed_by(SIGBUS),
            "a program's misaligned load-multiple did not end it with SIGBUS",
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
        "  fault    a program's undefined instruction, own breakpoint, misaligned \
         load-multiple and read past a mapped file's end ended it with SIGILL, SIGTRAP, SIGBUS \
         and SIGBUS"
    );

    let status = run_program(b"/no-restorer", NO_RESTORER)?;
    if status != 55 {
        println!("  signal   /no-restorer ended with {status}, not 55");
        return Err("a handler installed without a restorer was not entered");
    }
    println!("  signal   a handler installed without SA_RESTORER was entered from its frame");

    let status = run_program(b"/returning", RETURNING)?;
    if status != 55 {
        println!("  signal   /returning ended with {status}, not 55");
        return Err("a handler installed without a restorer did not return through sigreturn");
    }
    sigpage_is_code()?;
    println!(
        "  signal   ARM and Thumb handlers without SA_RESTORER, plain and SA_SIGINFO, returned \
         through the signal return page with their registers put back"
    );
    Ok(())
}

/// The signal return page maps read-and-run, never writable, and shared:
/// read back from a fresh space's region.
fn sigpage_is_code() -> Result<(), &'static str> {
    let space = crate::user::space::AddressSpace::new()
        .map_err(|_| "no space to map the signal return page into")?;
    let at = crate::syscall::sigpage::map_into(&space)
        .ok_or("the signal return page could not be mapped")?;
    let regions = space
        .regions()
        .map_err(|_| "no memory to list a space's regions")?;
    let page = regions
        .iter()
        .find(|region| region.start == at)
        .ok_or("the signal return page's region was not where it was mapped")?;
    let flags = page.flags;
    if !(flags.read && flags.execute && flags.shared)
        || flags.write
        || page.end != at + ferrix_bootinfo::PAGE_SIZE
    {
        return Err("the signal return page is not one read-and-run, unwritable, shared page");
    }
    Ok(())
}

/// Build `code` into a program called `name`, run it, and answer its status.
fn run_program(name: &[u8], code: &[u8]) -> Result<i32, &'static str> {
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf32,
        super::super::ARCH.elf_machine(),
        crate::syscall::image::Shape::Good,
        code,
    );
    crate::syscall::exec::run(&file, &[name], &[], [0x7e; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "a program that faults on purpose could not be started")
}

/// Take the entry a system call arrives through, with a frame built from these
/// registers as an `svc` from USR mode leaves them, and answer what the entry
/// left in `r0`.
///
/// For the seccomp check (`syscall::seccomp_check`), which needs the core's own
/// entry run with a call it chose -- the registered filter is asked first, and
/// the early answers (`set_tls`, the signal returns) follow -- without a
/// program to make it. Interrupts are as the caller had them: a real entry
/// starts with them masked and leaves them so, this wrapper does both for a
/// task that runs with them open. `None` for a mode of entry this architecture
/// does not have.
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
        kind: 2,
        fsr: 0,
        far: 0,
        reserved: 0,
        r: [0; 13],
        lr: 0,
        pc: ip as u32,
        cpsr: super::USER_CPSR,
    };
    for (register, value) in frame.r.iter_mut().zip(args) {
        *register = value as u32;
    }
    frame.r[7] = number as u32;
    let saved = <super::super::Irq as IrqControl>::disable();
    let done = super::system_call(&mut frame);
    <super::super::Irq as IrqControl>::restore(saved);
    done.ok()?;
    Some(frame.r[0] as i32 as isize)
}
