//! Exception entry on ARMv7-A.
//!
//! # Why there is assembly here
//!
//! As on AArch64 the architecture defines a *table*, not a function: `VBAR`
//! points at eight entries of one instruction each, and the CPU branches to
//! the one that matches what happened. What is different is where it arrives.
//! An AArch64 exception lands at EL1 on the kernel's own stack; an ARMv7-A one
//! lands in one of five processor modes, each with a banked stack pointer and
//! link register of its own and the interrupted program's registers still live.
//!
//! The kernel gives none of those modes a stack. Each stub corrects its mode's
//! link register to the instruction the exception interrupted — the offset
//! is the architecture's and differs per exception — and then `srsdb` stores
//! that return address and the saved status *on the SVC-mode stack*, and
//! `cps` switches to SVC mode. From there it is one common path, on the stack
//! the kernel was already running on, ending in `rfeia`, which loads the
//! program counter and the status from the same two words in one instruction.
//! This is the construct `srs` and `rfe` exist for, and it is why no mode but
//! SVC ever needs a stack.
//!
//! | Offset | Exception | Return address is |
//! |---|---|---|
//! | `0x00` | reset | never taken through `VBAR` |
//! | `0x04` | undefined instruction | `lr - 4`, the instruction |
//! | `0x08` | supervisor call | `lr`, the one after it |
//! | `0x0C` | prefetch abort, including `bkpt` | `lr - 4`, the instruction |
//! | `0x10` | data abort | `lr - 8`, the instruction, so it retries |
//! | `0x14` | hypervisor trap | never taken below HYP |
//! | `0x18` | IRQ | `lr - 4`, the instruction interrupted |
//! | `0x1C` | FIQ | `lr - 4` |

use super::cpu;

pub(super) mod check;

/// Bytes in a saved frame. Must match [`TrapFrame`] exactly, and a multiple of
/// eight, which is what the AAPCS requires of the stack at a call.
const FRAME_SIZE: usize = 80;

/// The register state at the point an exception was taken, in the order the
/// save path leaves it on the stack: lowest address first.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TrapFrame {
    /// Which of the eight vector entries was taken.
    pub(crate) kind: u32,
    /// The fault status register, for an abort: `DFSR` or `IFSR`.
    pub(crate) fsr: u32,
    /// The fault address register, for an abort: `DFAR` or `IFAR`.
    pub(crate) far: u32,
    /// Padding, so the frame keeps the stack eight-byte aligned.
    pub(crate) reserved: u32,
    /// `r0` through `r12`.
    pub(crate) r: [u32; 13],
    /// The interrupted code's link register — its SVC-mode one.
    pub(crate) lr: u32,
    /// Where to return to: the interrupted instruction, or for a system call
    /// the one after it.
    pub(crate) pc: u32,
    /// The interrupted code's program status.
    pub(crate) cpsr: u32,
}

impl TrapFrame {
    /// Where the trapping instruction is, for a report: the saved `pc`,
    /// which for an abort is already the faulting instruction's.
    pub(crate) const fn instruction_pointer(&self) -> u64 {
        self.pc as u64
    }

    /// True if the exception came from user mode.
    pub(crate) const fn came_from_user(&self) -> bool {
        self.cpsr & 0x1F == MODE_USR
    }
}

// The frame layout is asserted against the assembly's fixed offsets, so that
// adding a field without updating the save sequence is a build failure rather
// than a corrupted register.
const _: () = assert!(
    size_of::<TrapFrame>() == FRAME_SIZE,
    "the trap frame and the save sequence in the assembly must agree"
);
const _: () = assert!(
    FRAME_SIZE.is_multiple_of(8),
    "the AAPCS requires an eight-byte aligned stack at a call"
);

/// `CPSR.M` for user mode.
const MODE_USR: u32 = 0x10;

/// Vector entries, as the stubs number them.
const KIND_RESET: u32 = 0;
const KIND_UNDEFINED: u32 = 1;
const KIND_SVC: u32 = 2;
const KIND_PREFETCH_ABORT: u32 = 3;
const KIND_DATA_ABORT: u32 = 4;
const KIND_HYP_TRAP: u32 = 5;
const KIND_IRQ: u32 = 6;
const KIND_FIQ: u32 = 7;

core::arch::global_asm!(
    r#"
.arm
.section .text.vectors, "ax"

// The table itself: one branch per exception, 32-byte aligned as VBAR requires.
.balign 32
.globl ferrix_vectors
ferrix_vectors:
    b ferrix_stub_reset
    b ferrix_stub_undefined
    b ferrix_stub_svc
    b ferrix_stub_prefetch_abort
    b ferrix_stub_data_abort
    b ferrix_stub_hyp_trap
    b ferrix_stub_irq
    b ferrix_stub_fiq

// Each stub: correct the banked link register to the return address, store it
// and the banked saved status on the SVC stack, move to SVC mode, save the
// general registers, and pass the common path the kind of exception in r0 and
// the fault status and address in r1 and r2. r0 to r3 are free to use once the
// push has saved them.
ferrix_stub_reset:
    srsdb sp!, #0x13
    cps   #0x13
    push  {{r0-r12, lr}}
    mov   r0, #0
    mov   r1, #0
    mov   r2, #0
    b     ferrix_trap_common

ferrix_stub_undefined:
    sub   lr, lr, #4
    srsdb sp!, #0x13
    cps   #0x13
    push  {{r0-r12, lr}}
    mov   r0, #1
    mov   r1, #0
    mov   r2, #0
    b     ferrix_trap_common

ferrix_stub_svc:
    srsdb sp!, #0x13
    cps   #0x13
    push  {{r0-r12, lr}}
    mov   r0, #2
    mov   r1, #0
    mov   r2, #0
    b     ferrix_trap_common

ferrix_stub_prefetch_abort:
    sub   lr, lr, #4
    srsdb sp!, #0x13
    cps   #0x13
    push  {{r0-r12, lr}}
    mov   r0, #3
    mrc   p15, 0, r1, c5, c0, 1
    mrc   p15, 0, r2, c6, c0, 2
    b     ferrix_trap_common

ferrix_stub_data_abort:
    sub   lr, lr, #8
    srsdb sp!, #0x13
    cps   #0x13
    push  {{r0-r12, lr}}
    mov   r0, #4
    mrc   p15, 0, r1, c5, c0, 0
    mrc   p15, 0, r2, c6, c0, 0
    b     ferrix_trap_common

ferrix_stub_hyp_trap:
    srsdb sp!, #0x13
    cps   #0x13
    push  {{r0-r12, lr}}
    mov   r0, #5
    mov   r1, #0
    mov   r2, #0
    b     ferrix_trap_common

ferrix_stub_irq:
    sub   lr, lr, #4
    srsdb sp!, #0x13
    cps   #0x13
    push  {{r0-r12, lr}}
    mov   r0, #6
    mov   r1, #0
    mov   r2, #0
    b     ferrix_trap_common

ferrix_stub_fiq:
    sub   lr, lr, #4
    srsdb sp!, #0x13
    cps   #0x13
    push  {{r0-r12, lr}}
    mov   r0, #7
    mov   r1, #0
    mov   r2, #0
    b     ferrix_trap_common

// The common path. The frame is complete once kind, status, address and a
// padding word are pushed below the registers. The stack an exception
// interrupted is only four-byte aligned in general, so it is aligned to eight
// for the call, and r4 and r5 -- saved above, and preserved by the callee --
// remember the frame and the adjustment.
ferrix_trap_common:
    mov   r3, #0
    push  {{r0-r3}}
    mov   r4, sp
    and   r5, sp, #4
    sub   sp, sp, r5
    mov   r0, r4
    bl    ferrix_trap_entry
    add   sp, sp, r5
    add   sp, sp, #16
    pop   {{r0-r12, lr}}
    rfeia sp!
"#
);

// Entering USR mode.
//
// The return half of the vector path above, for a program that has never run:
// that one stores a return address and status with `srsdb` and leaves with
// `rfeia`; this builds a return address and status and leaves with the same
// `rfeia`. It does not return, and nothing is parked for a way back -- a program
// leaves USR mode for good through `exit_group`, which ends its task.
//
// The SVC stack pointer is left where it is. Every exception from USR is stored
// onto it with `srsdb`, and here it is the task's own kernel stack a few frames
// below its top, beside frames that are never returned to.
core::arch::global_asm!(
    r#"
.section .text
.arm

// r0 = a `TrapFrame`. Never returns: the frame is restored by the same
// instructions the vector path above returns through. The user stack pointer is
// banked, not in the frame, and is the task's user state's to load.
.globl ferrix_resume_user
.balign 4
ferrix_resume_user:
    cpsid if
    mov   sp, r0
    add   sp, sp, #16
    pop   {{r0-r12, lr}}
    rfeia sp!

// r0 = entry, r1 = user stack, r2 = the program's CPSR. Never returns.
.globl ferrix_enter_user
.balign 4
ferrix_enter_user:
    // Masked until the `rfeia`, which opens them from the CPSR it loads.
    cpsid if

    // The program's stack pointer is banked, and System mode shares USR's, so
    // it is set from there without ever running in USR with a wrong one.
    cps   #0x1f
    mov   sp, r1
    mov   lr, #0
    cps   #0x13

    // What `rfeia` takes: the program's first instruction, then its status.
    sub   sp, sp, #8
    str   r0, [sp]
    str   r2, [sp, #4]

    // Nothing of the kernel's survives into USR mode, but the one value a
    // program may be handed on entry: zero for a Linux program, a native
    // process's bootstrap handle. From r3, before the clearing below reaches it.
    mov   r0, r3
    mov   r1, #0
    mov   r2, #0
    mov   r3, #0
    mov   r4, #0
    mov   r5, #0
    mov   r6, #0
    mov   r7, #0
    mov   r8, #0
    mov   r9, #0
    mov   r10, #0
    mov   r11, #0
    mov   r12, #0
    rfeia sp!
"#
);

/// `CPSR` for a program: USR mode, little-endian, with asynchronous aborts and
/// FIQs masked and IRQs open. ARM state, unless the entry point says Thumb --
/// see [`CPSR_THUMB`].
///
/// IRQs were masked while a program ran as a guest of the boot task, for
/// AArch64's reason: a tick in USR mode could switch to a task with an address
/// space and back, and the switch back uninstalled the program's root. A
/// program is a task of its own now, so a tick is an ordinary preemption.
pub(super) const USER_CPSR: u32 = MODE_USR | (1 << 8) | (1 << 6);

/// `CPSR.T`: execute Thumb instructions.
///
/// Set when the entry point's bit 0 is, which is the interworking convention an
/// ELF follows and the one Linux's `start_thread` honours. Toolchains building
/// for ARMv7-A default to Thumb-2, so this is the common case rather than an
/// exotic one: Alpine's busybox enters at an odd address, and entered in ARM
/// state its first Thumb instructions decode as undefined three words in.
const CPSR_THUMB: u32 = 1 << 5;

unsafe extern "C" {
    /// Enter USR mode at `entry` on `stack` with `cpsr`.
    fn ferrix_enter_user(entry: u32, stack: u32, cpsr: u32, argument: u32) -> !;
    /// Resume USR mode from a saved frame.
    fn ferrix_resume_user(frame: *const TrapFrame) -> !;
}

/// A program's registers as its system call saved them.
///
/// Not the whole story on this architecture: the user stack pointer and link
/// register are banked and never in a trap frame, so they travel in the task's
/// user state instead, which is why [`UserRegs::set_stack`] writes there.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub(crate) struct UserRegs(pub(super) TrapFrame);

impl UserRegs {
    /// The same registers, as a child sees them: the call returned zero.
    pub(crate) const fn for_child(&self) -> UserRegs {
        let mut frame = self.0;
        frame.r[0] = 0;
        UserRegs(frame)
    }

    /// Start on `stack` instead, as `clone` with a stack argument asks. The
    /// user stack pointer is banked, so it goes in `state`.
    pub(crate) fn set_stack(&mut self, state: &mut super::UserState, stack: u64) {
        state.set_user_stack(stack as u32);
    }

    /// The stack pointer the program made the call with: banked, so read
    /// from the processor, which inside the call still holds it.
    pub(crate) fn stack_pointer(&self) -> u64 {
        u64::from(super::switch::user_banked().0)
    }
}

/// The signal Linux raises for a trap a program's own instruction took and
/// nothing resolved, as `(signal, si_code, si_addr)`: a translation or
/// permission fault `SIGSEGV`, an alignment fault `SIGBUS` with
/// `BUS_ADRALN` and any other abort `SIGBUS` with `BUS_OBJERR`, all at the
/// fault address; a `bkpt` `SIGTRAP`; and an undefined instruction or anything
/// else `SIGILL` at the instruction.
pub(crate) fn fault_signal(frame: &TrapFrame, trap: &crate::trap::Trap) -> (u32, i32, u64) {
    use crate::trap::Trap;
    use ferrix_linux_abi::types::{SIGBUS, SIGILL, SIGSEGV, SIGTRAP};

    /// `SIGSEGV`: nothing mapped at the address.
    const SEGV_MAPERR: i32 = 1;
    /// `SIGSEGV`: mapped, and the access refused.
    const SEGV_ACCERR: i32 = 2;
    /// `SIGBUS`: a misaligned address.
    const BUS_ADRALN: i32 = 1;
    /// `SIGBUS`: an error the hardware reported for the object.
    const BUS_OBJERR: i32 = 3;
    /// `SIGILL`: an illegal opcode.
    const ILL_ILLOPC: i32 = 1;
    /// `SIGTRAP`: a breakpoint.
    const TRAP_BRKPT: i32 = 1;

    let pc = u64::from(frame.pc);
    let far = u64::from(frame.far);
    let abort = frame.kind == KIND_DATA_ABORT || frame.kind == KIND_PREFETCH_ABORT;
    match trap {
        Trap::PageFault(fault) if fault.present => (SIGSEGV, SEGV_ACCERR, fault.address),
        Trap::PageFault(fault) => (SIGSEGV, SEGV_MAPERR, fault.address),
        Trap::Breakpoint => (SIGTRAP, TRAP_BRKPT, pc),
        _ if abort && frame.fsr & FSR_STATUS == STATUS_ALIGNMENT => (SIGBUS, BUS_ADRALN, far),
        _ if abort => (SIGBUS, BUS_OBJERR, far),
        _ => (SIGILL, ILL_ILLOPC, pc),
    }
}

/// Resume USR mode from `regs`. Does not return.
///
/// # Safety
///
/// (CONTEXT) Must be called by a user task with its address space installed and its user
/// state -- banked stack pointer included -- loaded, and `regs` must be a frame
/// a system call from that address space saved, living on the task's kernel
/// stack.
pub(crate) unsafe fn resume_user(regs: &UserRegs) -> ! {
    // SAFETY: (CONTEXT) the caller's guarantee is the assembly's contract.
    unsafe { ferrix_resume_user(core::ptr::from_ref(&regs.0)) }
}

/// Enter USR mode for the first time, at `entry` on `stack`, with `argument`'s
/// low half in r0. Does not return.
///
/// # Safety
///
/// (CONTEXT) Must be called by a user task, on its own kernel stack, with its address
/// space installed; `entry` and `stack` must be addresses inside that space.
pub(crate) unsafe fn enter_user(entry: u64, stack: u64, argument: u64, abi: crate::trap::Abi) -> ! {
    // One mode of user code on this architecture: every program is entered
    // as [`crate::trap::Abi::Native`], whatever it was asked to be.
    let _ = abi;
    // A user address on this processor is below 4 GiB by construction: the
    // loader and the stack builder produced these inside a 32-bit user half.
    let entry = entry as u32;
    let (entry, cpsr) = if entry & 1 == 0 {
        (entry, USER_CPSR)
    } else {
        (entry & !1, USER_CPSR | CPSR_THUMB)
    };
    // SAFETY: (CONTEXT) the caller's guarantee is the assembly's contract.
    unsafe { ferrix_enter_user(entry, stack as u32, cpsr, argument as u32) }
}

/// Service a system call made from USR mode with `svc #0`.
///
/// The EABI puts the number in `r7`, the arguments in `r0` to `r5`, and the
/// result back in `r0`. The saved `pc` already points past the `svc`, so the
/// program resumes at the next instruction. Interrupts are open while the call
/// is served, because a call may block.
///
/// # Errors
///
/// A system call from SVC mode, which is a kernel bug, or an `execve` this path
/// does not yet honour.
pub(crate) fn system_call(frame: &mut TrapFrame) -> Result<(), &'static str> {
    use crate::trap::{Outcome, SyscallArgs};
    use ferrix_linux_abi::nr::Syscall;

    if !frame.came_from_user() {
        return Err("a system call from SVC mode");
    }
    let [r0, r1, r2, r3, r4, r5, _, r7, ..] = frame.r;
    let args = SyscallArgs {
        abi: crate::trap::Abi::Native,
        number: r7 as usize,
        args: [
            r0.into(),
            r1.into(),
            r2.into(),
            r3.into(),
            r4.into(),
            r5.into(),
        ],
        // The saved `pc` already points past the `svc`, ARM or Thumb.
        ip: frame.pc.into(),
    };

    // The registered filter looks at the call first, before `set_tls` and the
    // signal returns are answered below (`docs/SECCOMP.md` §3.3).
    let outcome = match crate::trap::filter_system_call(&args) {
        Some(outcome) => outcome,
        None => {
            // `set_tls` writes a coprocessor register, which is a fact about
            // this processor rather than about the process, so it is answered
            // here for the same reason x86-64 answers `arch_prctl` in its own
            // trap path. It cannot fail: any value is a valid thread pointer
            // to hold, and Linux returns zero without looking at it.
            if let Some(Syscall::ArmSetTls) = super::decode_syscall(args.number) {
                answer_set_tls(frame, r0);
                return Ok(());
            }

            // `rt_sigreturn` and `sigreturn` replace the whole frame, `r0`
            // included, so they have no return value to write: answered here
            // rather than through `dispatch`.
            if let Some(call @ (Syscall::RtSigreturn | Syscall::Sigreturn)) =
                super::decode_syscall(args.number)
            {
                let mut context = super::signal::UserContext::from_trap(frame);
                super::enable_interrupts();
                if let Some(path) = crate::trap::return_path() {
                    (path.sigreturn)(&mut context, call == Syscall::RtSigreturn);
                }
                super::disable_interrupts();
                context.store_trap(frame);
                return Ok(());
            }

            dispatch_marked(&args, &UserRegs(*frame))
        }
    };

    match outcome {
        Outcome::Return(value) => store_words(frame, &[value as u64]),
        // The value in R0, the words in R1 to R3.
        Outcome::ReturnWords { value, words } => {
            store_words(frame, &[value as u64, words[0], words[1], words[2]])
        }
        // `execve`: the registers belong to a program that no longer exists, so
        // they are replaced rather than returned into. The stack pointer is
        // banked and set directly; Thumb follows the entry point's bit 0.
        Outcome::Enter { entry, stack, .. } => {
            let entry = entry as u32;
            frame.r = [0; 13];
            frame.lr = 0;
            frame.pc = entry & !1;
            frame.cpsr = if entry & 1 == 0 {
                USER_CPSR
            } else {
                USER_CPSR | CPSR_THUMB
            };
            super::switch::set_user_stack(stack as u32);
            Ok(())
        }
    }
}

/// `set_tls`: the register and the running task's record together, with
/// interrupts still masked from the `svc` entry. The record is the truth the
/// switch loads, and is never read back from the register
/// (`docs/OPAQUE-KERNEL.md` §9.14, 3b).
fn answer_set_tls(frame: &mut TrapFrame, value: u32) {
    cpu::write_tpidruro(value);
    // SAFETY: (CONTEXT) interrupts are masked here, in the running task's own
    // system call.
    let _ = unsafe {
        crate::sched::with_own_user_state(|state| state.set_thread_pointer(u64::from(value)))
    };
    if let Some(result) = frame.r.first_mut() {
        *result = 0;
    }
}

/// Serve `args` with interrupts open, as every call that may block is.
///
/// A native call that blocks lets the caller-saved VFP registers go for its
/// length (`docs/OPAQUE-KERNEL.md` §9.14, 3a's port): the mark tells the
/// switch so, raised with interrupts still masked and lowered once they are
/// masked again, before the outcome is written and before anything on the way
/// out can block in a call that keeps them.
fn dispatch_marked(args: &crate::trap::SyscallArgs, regs: &UserRegs) -> crate::trap::Outcome {
    let blocking = vectors_die_in(args.number);
    if blocking {
        mark_vectors_dead(true);
    }
    super::enable_interrupts();
    let outcome = crate::trap::system_call(args, Some(regs));
    super::disable_interrupts();
    if blocking {
        mark_vectors_dead(false);
    }
    outcome
}

/// Whether `number` is one of the three native calls declared to lose the
/// caller-saved VFP registers, as across a function call: `channel_write_read`,
/// `object_wait_one` and `port_wait`, the native calls that block (§9.14). By
/// number, at the one `svc` entry every program uses.
pub(super) const fn vectors_die_in(number: usize) -> bool {
    use ferrix_native_abi::nr::{CHANNEL_WRITE_READ, OBJECT_WAIT_ONE, PORT_WAIT};
    matches!(number, CHANNEL_WRITE_READ | OBJECT_WAIT_ONE | PORT_WAIT)
}

/// Raise or lower the running task's `vectors_dead` mark.
///
/// Called by the entry with interrupts masked: before it opens them for the
/// call, and after it closes them again.
fn mark_vectors_dead(dead: bool) {
    // SAFETY: (CONTEXT) interrupts are masked on this processor at both calls,
    // in the running task's own system call.
    let _ = unsafe { crate::sched::with_own_user_state(|state| state.set_vectors_dead(dead)) };
}

/// Put a call's answer in `r0` and on, the rest of the frame as it was: 32
/// bits each, which is all a 32-bit program's words are.
fn store_words(frame: &mut TrapFrame, answer: &[u64]) -> Result<(), &'static str> {
    for (register, word) in frame.r.iter_mut().zip(answer) {
        *register = *word as u32;
    }
    Ok(())
}

unsafe extern "C" {
    /// The vector table, aligned as `VBAR` requires.
    static ferrix_vectors: [u8; 8 * 4];
}

/// Where the save sequence hands over.
///
/// Not called from Rust — the `bl` in the assembly above is its only caller.
#[unsafe(no_mangle)]
extern "C" fn ferrix_trap_entry(frame: &mut TrapFrame) {
    crate::trap::dispatch(frame);
}

/// Fault status: the long-descriptor format's status field, bits 5:0. The
/// format is the long one because `TTBCR.EAE` is set, and it is AArch64's.
const FSR_STATUS: u32 = 0b11_1111;
/// Data fault status: the access was a write.
const DFSR_WRITE: u32 = 1 << 11;

/// Status with the level bits masked off: what kind of fault, not where.
const STATUS_KIND: u32 = 0b11_1100;
/// A translation fault: nothing mapped.
const STATUS_TRANSLATION: u32 = 0b00_0100;
/// An access-flag fault: mapped, never touched. Treated as nothing mapped.
const STATUS_ACCESS_FLAG: u32 = 0b00_1000;
/// A permission fault: mapped, and the mapping refused the access.
const STATUS_PERMISSION: u32 = 0b00_1100;
/// A debug event, which is how a `bkpt` arrives: as a prefetch abort.
const STATUS_DEBUG: u32 = 0b10_0010;
/// An alignment fault.
const STATUS_ALIGNMENT: u32 = 0b10_0001;

/// Turn an ARMv7-A trap frame into the architecture-neutral description the
/// generic dispatcher works with.
pub(crate) fn classify(frame: &TrapFrame) -> crate::trap::Trap {
    use crate::trap::Trap;

    match frame.kind {
        // As on AArch64, the number of the interrupt is the controller's to
        // say, not the exception's; what is reported is the vector entry.
        KIND_IRQ => Trap::Interrupt(frame.kind),
        KIND_UNDEFINED => Trap::IllegalInstruction,
        KIND_SVC => Trap::SystemCall,
        KIND_PREFETCH_ABORT | KIND_DATA_ABORT => abort(frame),
        _ => Trap::Fault {
            name: kind_name(frame.kind),
            code: u64::from(frame.kind),
        },
    }
}

/// Decode a prefetch or data abort.
fn abort(frame: &TrapFrame) -> crate::trap::Trap {
    use crate::trap::{PageFault, Trap};

    let data = frame.kind == KIND_DATA_ABORT;
    let status = frame.fsr & FSR_STATUS;
    if !data && status == STATUS_DEBUG {
        return Trap::Breakpoint;
    }
    if status == STATUS_ALIGNMENT {
        return Trap::Fault {
            name: "alignment fault",
            code: u64::from(frame.fsr),
        };
    }

    match status & STATUS_KIND {
        STATUS_TRANSLATION | STATUS_ACCESS_FLAG | STATUS_PERMISSION => Trap::PageFault(PageFault {
            address: u64::from(frame.far),
            // The write bit means anything only for a data abort.
            write: data && frame.fsr & DFSR_WRITE != 0,
            execute: !data,
            user: frame.came_from_user(),
            present: status & STATUS_KIND == STATUS_PERMISSION,
        }),
        _ => Trap::Fault {
            name: if data { "data abort" } else { "prefetch abort" },
            code: u64::from(frame.fsr),
        },
    }
}

/// A readable name for a vector entry.
const fn kind_name(kind: u32) -> &'static str {
    match kind {
        KIND_RESET => "reset",
        KIND_UNDEFINED => "undefined instruction",
        KIND_SVC => "supervisor call",
        KIND_PREFETCH_ABORT => "prefetch abort",
        KIND_DATA_ABORT => "data abort",
        KIND_HYP_TRAP => "hypervisor trap",
        KIND_IRQ => "IRQ",
        KIND_FIQ => "FIQ",
        _ => "exception",
    }
}

/// Print the interrupted state, unlogged (`console::write_unlogged`).
pub(crate) fn report_trap(frame: &TrapFrame) {
    use crate::console::println_unlogged;

    println_unlogged!(
        "  vector   {} ({})  fsr {:#010x}  far {:#010x}",
        frame.kind,
        kind_name(frame.kind),
        frame.fsr,
        frame.far
    );
    println_unlogged!(
        "  pc       {:#010x}  cpsr {:#010x}  lr {:#010x}",
        frame.pc,
        frame.cpsr,
        frame.lr
    );
    for quad in 0..4 {
        let register = |index: usize| frame.r.get(index).copied().unwrap_or(0);
        let first = quad * 4;
        println_unlogged!(
            "  r{:<2} {:#010x}  r{:<2} {:#010x}  r{:<2} {:#010x}  r{:<2} {:#010x}",
            first,
            register(first),
            first + 1,
            register(first + 1),
            first + 2,
            register(first + 2),
            first + 3,
            register(first + 3),
        );
    }
    println_unlogged!(
        "  from     {}",
        if frame.came_from_user() {
            "user mode"
        } else {
            "the kernel"
        }
    );
}

/// Install the vector table on this core.
///
/// # Safety
///
/// (ENTRY) Must be called once per CPU, before anything on it can fault and before it
/// unmasks interrupts. `VBAR` and `SCTLR` are banked per core, and the one
/// table serves every core: it holds code, and no per-core state.
pub(crate) unsafe fn init() {
    // The kernel is linked below 4 GiB, so the address fits the register.
    let table = (&raw const ferrix_vectors).addr() as u32;
    // SAFETY: (ENTRY) `table` is the vector table in this image, 32-byte aligned as
    // `VBAR` requires, and every entry branches to a real stub.
    unsafe { cpu::install_vectors(table) };
    // Here because this runs on every core, and both registers are per core.
    cpu::enable_user_fpu();
}

/// Raise a breakpoint, so the boot self-check can prove the trap path runs.
pub(crate) fn breakpoint() {
    // SAFETY: (PROBE) `bkpt` raises a prefetch abort the vector table handles. Like
    // AArch64's `brk`, the return address it leaves is the instruction
    // itself, which is why returning needs `advance_past_breakpoint`.
    unsafe {
        core::arch::asm!("bkpt #0", options(nomem, nostack));
    }
}

/// Step the return address over a breakpoint: every ARM instruction is four
/// bytes, and this kernel is ARM code throughout.
pub(crate) const fn advance_past_breakpoint(frame: &mut TrapFrame) {
    frame.pc += 4;
}

/// Whether `frame` was interrupted with IRQs masked, for the dispatcher's
/// benefit when it reports a fault.
#[expect(
    dead_code,
    reason = "AUDIT: named so the IRQ mask bit has one definition; used from stage 4"
)]
pub(crate) const fn irqs_were_masked(frame: &TrapFrame) -> bool {
    frame.cpsr & cpu::CPSR_IRQ_MASKED != 0
}
