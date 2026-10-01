//! Exception entry on `AArch64`.
//!
//! # Why there is assembly here
//!
//! The architecture defines a *table*, not a function: `VBAR_EL1` points at
//! sixteen entries at fixed 128-byte offsets, and the CPU jumps to the one that
//! matches what happened and where it came from. The layout is the interface —
//! a Rust array of function pointers is not what the hardware reads — and each
//! entry is entered with the interrupted program's registers still live.
//!
//! Sixteen entries, four for each of the four kinds of exception:
//!
//! | Offset | Taken from |
//! |---|---|
//! | `0x000` | the current EL, while `SP_EL0` is selected |
//! | `0x200` | the current EL, while `SP_ELx` is selected — where the kernel's own faults arrive |
//! | `0x400` | a lower EL running `AArch64` — where user mode's arrive |
//! | `0x600` | a lower EL running `AArch32`, which Ferrix never enters |
//!
//! and within each block, synchronous, `IRQ`, `FIQ`, `SError` in that order.
//!
//! Each entry is far too small for a register save, so it stashes `x0`/`x1`,
//! puts its own index in `x0` and branches to the common path.

use super::cpu;

pub(super) mod check;

/// Bytes in a saved frame. Must match [`TrapFrame`] exactly, and must be a
/// multiple of sixteen because `AArch64` faults on a misaligned stack pointer.
const FRAME_SIZE: usize = 304;

/// The register state at the point an exception was taken.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct TrapFrame {
    /// `x0` through `x30`. `x30` is the link register; there is no `x31`,
    /// because that encoding means the zero register or the stack pointer
    /// depending on the instruction.
    pub(crate) x: [u64; 31],
    /// `SP_EL0`: the user stack pointer when the exception came from EL0.
    pub(crate) sp: u64,
    /// Exception link register — where to return to.
    pub(crate) elr: u64,
    /// Saved program status.
    pub(crate) spsr: u64,
    /// Exception syndrome: what happened, and why.
    pub(crate) esr: u64,
    /// Fault address, for an abort.
    pub(crate) far: u64,
    /// Which of the sixteen vector entries was taken.
    pub(crate) kind: u64,
    /// Padding, so the frame is a multiple of sixteen bytes.
    pub(crate) reserved: u64,
}

impl TrapFrame {
    /// Where the trapping instruction is, for a report.
    pub(crate) const fn instruction_pointer(&self) -> u64 {
        self.elr
    }

    /// True if the exception came from EL0.
    ///
    /// Read from the vector entry rather than from `SPSR`: the four "lower EL"
    /// entries are, by definition, the only ones a user-mode exception can
    /// arrive through.
    pub(crate) const fn came_from_user(&self) -> bool {
        self.kind >= 8
    }

    /// The exception class: the top six bits of the syndrome.
    pub(crate) const fn exception_class(&self) -> u64 {
        self.esr >> 26
    }

    /// The instruction-specific syndrome: the bottom twenty-five bits.
    pub(crate) const fn syndrome(&self) -> u64 {
        self.esr & 0x01FF_FFFF
    }
}

// The frame layout is asserted against the assembly's fixed offsets below, so
// that adding a field without updating the save sequence is a build failure
// rather than a corrupted register.
const _: () = assert!(
    size_of::<TrapFrame>() == FRAME_SIZE,
    "the trap frame and the save sequence in the assembly must agree"
);
const _: () = assert!(
    FRAME_SIZE.is_multiple_of(16),
    "AArch64 requires a 16-byte aligned stack"
);

core::arch::global_asm!(
    r#"
.section .text

// One vector entry: stash x0 and x1, record which entry this is, and go.
// `.align 7` is 128 bytes, which is the spacing the architecture fixes.
.macro FERRIX_VECTOR index
.align 7
    sub  sp, sp, #304
    stp  x0, x1, [sp, #0]
    mov  x0, #\index
    b    ferrix_trap_save
.endm

// The same, for an entry from EL0, with the Spectre-BHB loop first on a core
// Arm lists: that many taken branches overwrite the branch history a program
// left, so no indirect branch in the kernel is predicted from it, and the
// barrier keeps anything after from running before they have. The count is
// the largest any core needs, zero for none; see `super::speculation`.
.macro FERRIX_VECTOR_EL0 index
.align 7
    sub  sp, sp, #304
    stp  x0, x1, [sp, #0]
.if {hardened}
    adrp x0, {bhb_loops}
    ldr  x0, [x0, :lo12:{bhb_loops}]
    cbz  x0, 2f
1:  b    3f
3:  subs x0, x0, #1
    b.ne 1b
    dsb  nsh
    isb
2:
.endif
    mov  x0, #\index
    b    ferrix_trap_save
.endm

.globl ferrix_vectors
.align 11
ferrix_vectors:
    FERRIX_VECTOR 0    // current EL, SP_EL0: synchronous
    FERRIX_VECTOR 1    // current EL, SP_EL0: IRQ
    FERRIX_VECTOR 2    // current EL, SP_EL0: FIQ
    FERRIX_VECTOR 3    // current EL, SP_EL0: SError
    FERRIX_VECTOR 4    // current EL, SP_ELx: synchronous
    FERRIX_VECTOR 5    // current EL, SP_ELx: IRQ
    FERRIX_VECTOR 6    // current EL, SP_ELx: FIQ
    FERRIX_VECTOR 7    // current EL, SP_ELx: SError
    FERRIX_VECTOR_EL0 8    // lower EL, AArch64: synchronous
    FERRIX_VECTOR_EL0 9    // lower EL, AArch64: IRQ
    FERRIX_VECTOR_EL0 10   // lower EL, AArch64: FIQ
    FERRIX_VECTOR_EL0 11   // lower EL, AArch64: SError
    FERRIX_VECTOR_EL0 12   // lower EL, AArch32: synchronous
    FERRIX_VECTOR_EL0 13   // lower EL, AArch32: IRQ
    FERRIX_VECTOR_EL0 14   // lower EL, AArch32: FIQ
    FERRIX_VECTOR_EL0 15   // lower EL, AArch32: SError

ferrix_trap_save:
    stp  x2, x3, [sp, #16]
    stp  x4, x5, [sp, #32]
    stp  x6, x7, [sp, #48]
    stp  x8, x9, [sp, #64]
    stp  x10, x11, [sp, #80]
    stp  x12, x13, [sp, #96]
    stp  x14, x15, [sp, #112]
    stp  x16, x17, [sp, #128]
    stp  x18, x19, [sp, #144]
    stp  x20, x21, [sp, #160]
    stp  x22, x23, [sp, #176]
    stp  x24, x25, [sp, #192]
    stp  x26, x27, [sp, #208]
    stp  x28, x29, [sp, #224]
    str  x30, [sp, #240]
    mrs  x9, sp_el0
    mrs  x10, elr_el1
    mrs  x11, spsr_el1
    mrs  x12, esr_el1
    mrs  x13, far_el1
    stp  x9, x10, [sp, #248]
    stp  x11, x12, [sp, #264]
    stp  x13, x0, [sp, #280]
    str  xzr, [sp, #296]
    mov  x0, sp
    bl   ferrix_trap_entry
.globl ferrix_trap_restore
ferrix_trap_restore:
    ldp  x9, x10, [sp, #248]
    ldr  x11, [sp, #264]
    msr  sp_el0, x9
    msr  elr_el1, x10
    msr  spsr_el1, x11
    ldp  x2, x3, [sp, #16]
    ldp  x4, x5, [sp, #32]
    ldp  x6, x7, [sp, #48]
    ldp  x8, x9, [sp, #64]
    ldp  x10, x11, [sp, #80]
    ldp  x12, x13, [sp, #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x19, [sp, #144]
    ldp  x20, x21, [sp, #160]
    ldp  x22, x23, [sp, #176]
    ldp  x24, x25, [sp, #192]
    ldp  x26, x27, [sp, #208]
    ldp  x28, x29, [sp, #224]
    ldr  x30, [sp, #240]
    ldp  x0, x1, [sp, #0]
    add  sp, sp, #304
    eret
"#,
    hardened = const super::speculation::ENTRY_HARDENING,
    bhb_loops = sym super::speculation::BHB_LOOPS,
);

// Entering EL0.
//
// The return half of the trap path above, for a program that has never been in
// EL0: it builds the state an `eret` loads, and takes it. It does not return,
// and nothing is parked for a way back. A program leaves EL0 only through an
// exception, and leaves it for good through `exit_group`, which ends its task
// in the scheduler rather than unwinding to whoever started it.
//
// `SP_EL1` is left where it is. It is the stack every exception from EL0 lands
// on, and here it is the task's own kernel stack a few frames below its top.
// The frames above belong to a function that is never returned to, so an
// exception may land beside them without harm -- and because each task has its
// own stack, two programs never share one.
core::arch::global_asm!(
    r#"
.section .text

// x0 = a `TrapFrame`, sixteen-byte aligned. Never returns: the frame is
// restored by the same instructions every exception from EL0 returns through.
.globl ferrix_resume_user
.align 4
ferrix_resume_user:
    msr  daifset, #0xf
    mov  sp, x0
    b    ferrix_trap_restore

// x0 = entry, x1 = user stack. Never returns.
.globl ferrix_enter_user
.align 4
ferrix_enter_user:
    // Masked until the eret, which opens them from `USER_SPSR`: an interrupt
    // taken between loading ELR_EL1 and the eret would overwrite it.
    msr  daifset, #0xf
    msr  sp_el0, x1
    msr  elr_el1, x0
    mov  x10, #{user_spsr}
    msr  spsr_el1, x10

    // Nothing of the kernel's survives into EL0, but the one value a program
    // may be handed on entry: zero for a Linux program, a native process's
    // bootstrap handle. From x2, before the clearing below reaches it.
    mov  x0, x2
    mov  x1, xzr
    mov  x2, xzr
    mov  x3, xzr
    mov  x4, xzr
    mov  x5, xzr
    mov  x6, xzr
    mov  x7, xzr
    mov  x8, xzr
    mov  x9, xzr
    mov  x10, xzr
    mov  x11, xzr
    mov  x12, xzr
    mov  x13, xzr
    mov  x14, xzr
    mov  x15, xzr
    mov  x16, xzr
    mov  x17, xzr
    mov  x18, xzr
    mov  x19, xzr
    mov  x20, xzr
    mov  x21, xzr
    mov  x22, xzr
    mov  x23, xzr
    mov  x24, xzr
    mov  x25, xzr
    mov  x26, xzr
    mov  x27, xzr
    mov  x28, xzr
    mov  x29, xzr
    mov  x30, xzr
    eret
"#,
    user_spsr = const USER_SPSR,
);

/// `SPSR_EL1` for a program: `EL0t`, with debug, asynchronous aborts and FIQs
/// masked and IRQs open.
///
/// IRQs were masked here while a program ran as a guest of the boot task,
/// whose address space field is `None`: a tick taken in EL0 could switch to a
/// task that had a space and back again, and the switch back uninstalled the
/// program's root. A program is a task of its own now, carrying its space, so
/// a tick in EL0 is an ordinary preemption and the scheduler puts the right
/// root back when it returns.
pub(super) const USER_SPSR: u64 = (1 << 9) | (1 << 8) | (1 << 6);

unsafe extern "C" {
    /// Enter EL0 at `entry` on `stack`.
    fn ferrix_enter_user(entry: u64, stack: u64, argument: u64) -> !;
    /// Resume EL0 from a saved frame.
    fn ferrix_resume_user(frame: *const TrapFrame) -> !;
}

/// A program's registers as its system call saved them: the whole trap frame,
/// which on this architecture includes `SP_EL0`, the return address and the
/// saved status.
///
/// Aligned to sixteen because the resume path makes its address the stack
/// pointer, and `AArch64` faults on a misaligned one.
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub(crate) struct UserRegs(pub(super) TrapFrame);

impl UserRegs {
    /// The same registers, as a child sees them: the call returned zero.
    pub(crate) const fn for_child(&self) -> UserRegs {
        let mut frame = self.0;
        frame.x[0] = 0;
        UserRegs(frame)
    }

    /// Start on `stack` instead, as `clone` with a stack argument asks.
    pub(crate) const fn set_stack(&mut self, state: &mut super::UserState, stack: u64) {
        let _ = state;
        self.0.sp = stack;
    }

    /// The stack pointer the program made the call with.
    pub(crate) const fn stack_pointer(&self) -> u64 {
        self.0.sp
    }
}

/// The signal Linux raises for a trap a program's own instruction took and
/// nothing resolved, as `(signal, si_code, si_addr)`: an abort `SIGSEGV` at
/// the fault address, a misaligned program counter or stack pointer
/// `SIGBUS`, a floating-point exception `SIGFPE`, a `brk` `SIGTRAP`, and an
/// undefined instruction or anything else `SIGILL` at the instruction.
pub(crate) fn fault_signal(frame: &TrapFrame, trap: &crate::trap::Trap) -> (u32, i32, u64) {
    use crate::trap::Trap;
    use ferrix_linux_abi::types::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};

    /// `si_code`: raised by the kernel for its own reasons.
    const SI_KERNEL: i32 = 0x80;
    /// `SIGSEGV`: nothing mapped at the address.
    const SEGV_MAPERR: i32 = 1;
    /// `SIGSEGV`: mapped, and the access refused.
    const SEGV_ACCERR: i32 = 2;
    /// `SIGBUS`: a misaligned address.
    const BUS_ADRALN: i32 = 1;
    /// `SIGILL`: an illegal opcode.
    const ILL_ILLOPC: i32 = 1;
    /// `SIGTRAP`: a breakpoint.
    const TRAP_BRKPT: i32 = 1;

    /// Exception class: the program counter was misaligned.
    const EC_PC_ALIGNMENT: u64 = 0b100010;
    /// Exception class: the stack pointer was misaligned.
    const EC_SP_ALIGNMENT: u64 = 0b100110;
    /// Exception class: a trapped floating-point exception.
    const EC_FP_EXCEPTION: u64 = 0b101100;

    match trap {
        Trap::PageFault(fault) if fault.present => (SIGSEGV, SEGV_ACCERR, fault.address),
        Trap::PageFault(fault) => (SIGSEGV, SEGV_MAPERR, fault.address),
        Trap::Breakpoint => (SIGTRAP, TRAP_BRKPT, frame.elr),
        _ => match frame.exception_class() {
            EC_PC_ALIGNMENT => (SIGBUS, BUS_ADRALN, frame.elr),
            EC_SP_ALIGNMENT => (SIGBUS, BUS_ADRALN, frame.sp),
            EC_FP_EXCEPTION => (SIGFPE, SI_KERNEL, frame.elr),
            _ => (SIGILL, ILL_ILLOPC, frame.elr),
        },
    }
}

/// Resume EL0 from `regs`. Does not return.
///
/// # Safety
///
/// (CONTEXT) Must be called by a user task with its address space installed and its user
/// state loaded, and `regs` must be a frame a system call from that address
/// space saved. `regs` must live on the running task's kernel stack: its
/// address becomes `SP_EL1`, and the stack an exception from EL0 lands on is
/// just above it.
pub(crate) unsafe fn resume_user(regs: &UserRegs) -> ! {
    // SAFETY: (CONTEXT) the caller's guarantee is the assembly's contract.
    unsafe { ferrix_resume_user(core::ptr::from_ref(&regs.0)) }
}

/// Enter EL0 for the first time, at `entry` on `stack`, with `argument` in x0.
/// Does not return.
///
/// # Safety
///
/// (CONTEXT) Must be called by a user task, on its own kernel stack, with its address
/// space installed; `entry` and `stack` must be addresses inside that space.
pub(crate) unsafe fn enter_user(entry: u64, stack: u64, argument: u64, abi: crate::trap::Abi) -> ! {
    // One mode of user code on this architecture: every program is entered
    // as [`crate::trap::Abi::Native`], whatever it was asked to be.
    let _ = abi;
    // SAFETY: (CONTEXT) the caller's guarantee is the assembly's contract.
    unsafe { ferrix_enter_user(entry, stack, argument) }
}

/// Service a system call made from EL0 with `svc #0`.
///
/// The number is in `x8` and the arguments in `x0` to `x5`, and the result goes
/// back in `x0`. `ELR_EL1` already points past the `svc`, so returning resumes
/// the program at the next instruction with nothing to adjust — unlike a
/// breakpoint, which reports its own address.
///
/// Interrupts are open while the call is served, as on the other two
/// architectures: a call may block, and a processor taking no ticks while one
/// did would never switch away from it.
///
/// # Errors
///
/// A system call from EL1, which is a kernel bug, or an `execve` this path does
/// not yet know how to honour.
pub(crate) fn system_call(frame: &mut TrapFrame) -> Result<(), &'static str> {
    use crate::trap::{Outcome, SyscallArgs, system_call as dispatch};

    if !frame.came_from_user() {
        return Err("a system call from EL1");
    }
    let [x0, x1, x2, x3, x4, x5, _, _, x8, ..] = frame.x;
    let args = SyscallArgs {
        abi: crate::trap::Abi::Native,
        number: x8 as usize,
        args: [x0, x1, x2, x3, x4, x5],
        // `ELR_EL1` already points past the `svc`.
        ip: frame.elr,
    };

    // The registered filter looks at the call first, before `rt_sigreturn`
    // is answered below (`docs/SECCOMP.md` §3.3).
    let outcome = match (if matches!(super::decode_syscall(args.number), Some(ferrix_linux_abi::nr::Syscall::RtSigreturn)) { None } else { crate::trap::filter_system_call(&args) }) {
        Some(outcome) => outcome,
        None => {
            // `rt_sigreturn` replaces the whole frame, `x0` included, so it has
            // no return value to write: answered here rather than through
            // `dispatch`.
            if let Some(ferrix_linux_abi::nr::Syscall::RtSigreturn) =
                super::decode_syscall(args.number)
            {
                let mut context = super::signal::UserContext::from_trap(frame);
                super::enable_interrupts();
                if let Some(path) = crate::trap::return_path() {
                    (path.sigreturn)(&mut context, true);
                }
                super::disable_interrupts();
                context.store_trap(frame);
                return Ok(());
            }

            let regs = UserRegs(*frame);
            super::enable_interrupts();
            let outcome = dispatch(&args, Some(&regs));
            super::disable_interrupts();
            outcome
        }
    };

    match outcome {
        Outcome::Return(value) => {
            if let Some(result) = frame.x.first_mut() {
                *result = value as u64;
            }
            Ok(())
        }
        // `execve`: the registers belong to a program that no longer exists, so
        // they are replaced rather than returned into.
        Outcome::Enter { entry, stack, .. } => {
            frame.x = [0; 31];
            frame.sp = stack;
            frame.elr = entry;
            frame.spsr = USER_SPSR;
            Ok(())
        }
    }
}

unsafe extern "C" {
    /// The vector table, aligned as `VBAR_EL1` requires.
    static ferrix_vectors: [u8; 16 * 128];
}

/// Where the save sequence hands over.
///
/// Not called from Rust — the `bl` in the assembly above is its only caller.
#[unsafe(no_mangle)]
extern "C" fn ferrix_trap_entry(frame: &mut TrapFrame) {
    crate::trap::dispatch(frame);
}

/// Exception class: a data abort taken from a lower exception level.
const EC_DATA_ABORT_LOWER: u64 = 0b100100;
/// Exception class: a data abort taken from the current exception level.
const EC_DATA_ABORT_SAME: u64 = 0b100101;
/// Exception class: an instruction abort taken from a lower exception level.
const EC_INSTRUCTION_ABORT_LOWER: u64 = 0b100000;
/// Exception class: an instruction abort taken from the current level.
const EC_INSTRUCTION_ABORT_SAME: u64 = 0b100001;
/// Exception class: an `SVC` from `AArch64` — a system call.
const EC_SVC: u64 = 0b010101;
/// Exception class: a `BRK` instruction — a breakpoint.
const EC_BRK: u64 = 0b111100;
/// Exception class: the CPU could not classify the instruction.
const EC_UNKNOWN: u64 = 0b000000;

/// Data abort syndrome bit: the access was a write rather than a read.
const ISS_WRITE: u64 = 1 << 6;
/// Data and instruction abort syndrome field: the fault status code.
const ISS_FAULT_STATUS: u64 = 0b11_1111;

/// Mask selecting the *kind* of fault from a fault status code, leaving out
/// the two bits that say which level of the walk it happened at.
const FAULT_KIND: u64 = 0b111100;

/// `0b0011xx`: mapped, and the mapping refused the access.
///
/// The neighbouring encodings are `0b0001xx` for a translation fault, where
/// nothing was mapped, and `0b0010xx` for an access-flag fault, where something
/// was but had not been touched. Both mean "there is effectively nothing there"
/// as far as the fault handler is concerned, which is why only this one needs a
/// name: everything that is not a permission fault is treated as absent.
const FAULT_PERMISSION: u64 = 0b001100;

/// Vector entries 1, 5, 9 and 13 are `IRQ`.
const fn is_irq(kind: u64) -> bool {
    kind % 4 == 1
}

/// Turn an `AArch64` trap frame into the architecture-neutral description the
/// generic dispatcher works with.
pub(crate) fn classify(frame: &TrapFrame) -> crate::trap::Trap {
    use crate::trap::Trap;

    if is_irq(frame.kind) {
        // Unlike x86-64 there is no interrupt number in the exception itself:
        // which one fired has to be asked of the controller, and there is none
        // yet. Report the vector entry rather than inventing a number — this
        // classifies *what happened*, and what to do about it is the
        // dispatcher's decision, not this function's.
        return Trap::Interrupt(frame.kind as u32);
    }

    let class = frame.exception_class();
    match class {
        EC_BRK => Trap::Breakpoint,
        EC_SVC => Trap::SystemCall,
        EC_UNKNOWN => Trap::IllegalInstruction,
        EC_DATA_ABORT_LOWER
        | EC_DATA_ABORT_SAME
        | EC_INSTRUCTION_ABORT_LOWER
        | EC_INSTRUCTION_ABORT_SAME => Trap::PageFault(abort(frame, class)),
        _ => Trap::Fault {
            name: class_name(class),
            code: frame.esr,
        },
    }
}

/// Decode a data or instruction abort.
fn abort(frame: &TrapFrame, class: u64) -> crate::trap::PageFault {
    let data = class == EC_DATA_ABORT_LOWER || class == EC_DATA_ABORT_SAME;
    let status = frame.syndrome() & ISS_FAULT_STATUS;

    crate::trap::PageFault {
        address: frame.far,
        // The write bit only means anything for a *data* abort; on an
        // instruction abort that bit position is part of another field.
        write: data && frame.syndrome() & ISS_WRITE != 0,
        execute: !data,
        user: frame.came_from_user(),
        // A permission fault means a mapping existed and refused the access. A
        // translation or access-flag fault means there was effectively nothing
        // there, which is the case the demand-paging path handles.
        present: status & FAULT_KIND == FAULT_PERMISSION,
    }
}

/// A readable name for an exception class.
const fn class_name(class: u64) -> &'static str {
    match class {
        0b000000 => "unknown reason",
        0b000001 => "trapped WFI or WFE",
        0b000111 => "trapped SIMD or floating point",
        0b001110 => "illegal execution state",
        0b010101 => "supervisor call",
        0b011000 => "trapped system register access",
        0b100000 | 0b100001 => "instruction abort",
        0b100010 => "misaligned program counter",
        0b100100 | 0b100101 => "data abort",
        0b100110 => "stack pointer alignment fault",
        0b101100 => "floating point exception",
        0b101111 => "SError",
        0b111100 => "breakpoint instruction",
        _ => "exception",
    }
}

/// Print the interrupted state, unlogged (`console::write_unlogged`).
pub(crate) fn report_trap(frame: &TrapFrame) {
    use crate::console::println_unlogged;

    println_unlogged!(
        "  esr      {:#018x}  class {:#04x} ({})",
        frame.esr,
        frame.exception_class(),
        class_name(frame.exception_class())
    );
    println_unlogged!("  far      {:#018x}", frame.far);
    println_unlogged!("  elr      {:#018x}  spsr {:#018x}", frame.elr, frame.spsr);
    println_unlogged!("  sp_el0   {:#018x}  vector entry {}", frame.sp, frame.kind);

    for pair in 0..15 {
        let low = frame.x.get(pair * 2).copied().unwrap_or(0);
        let high = frame.x.get(pair * 2 + 1).copied().unwrap_or(0);
        println_unlogged!(
            "  x{:<2} {:#018x}  x{:<2} {:#018x}",
            pair * 2,
            low,
            pair * 2 + 1,
            high
        );
    }
    println_unlogged!("  x30 {:#018x}", frame.x.get(30).copied().unwrap_or(0));
    println_unlogged!(
        "  from     {}",
        if frame.came_from_user() {
            "EL0"
        } else {
            "the kernel"
        }
    );
}

/// Install the vector table on this core.
///
/// # Safety
///
/// (ENTRY) Must be called on every core, once, before anything on it can fault and
/// before it unmasks interrupts. One table serves every core: it holds code,
/// and no per-core state.
pub(crate) unsafe fn init() {
    let table = (&raw const ferrix_vectors) as u64;
    // SAFETY: (ENTRY) `table` is the vector table in this image, 2048-byte aligned as
    // `VBAR_EL1` requires, and every entry branches to a real save sequence.
    unsafe { cpu::write_vbar(table) };
    // Here because this runs on every core, and `CPACR_EL1` is per core.
    cpu::enable_user_fpu();
}

/// Raise a breakpoint, so the boot self-check can prove the trap path runs.
pub(crate) fn breakpoint() {
    // SAFETY: (PROBE) `brk` raises a synchronous exception the vector table handles.
    // Unlike x86-64's `int3`, the link register points *at* this instruction
    // rather than past it, which is why returning needs `advance_past_breakpoint`.
    unsafe {
        core::arch::asm!("brk #0", options(nomem, nostack));
    }
}

/// Step the return address over a breakpoint.
///
/// `AArch64` and x86-64 differ here and the difference is easy to miss: `int3`
/// leaves the saved instruction pointer *after* the trap, so returning
/// continues; `brk` leaves it *on* the instruction, so returning re-executes it
/// forever. Every `AArch64` instruction is four bytes.
pub(crate) const fn advance_past_breakpoint(frame: &mut TrapFrame) {
    frame.elr += 4;
}
