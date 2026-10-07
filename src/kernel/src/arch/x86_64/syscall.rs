//! The `SYSCALL` entry path, and the way into ring 3.
//!
//! # Why `SYSCALL` and not an interrupt gate
//!
//! An interrupt gate with `DPL=3` would be far less work: the processor
//! switches to the kernel stack out of the TSS by itself, and the existing
//! trap machinery would carry it. Linux still has one, at `int $0x80`.
//!
//! But nothing calls it. Every current libc on x86-64 issues `syscall`, so an
//! interrupt gate would be a path the kernel could test and no real program
//! would take — which is the worst kind of working code.
//!
//! # What `SYSCALL` does not do
//!
//! It does almost nothing, and the gaps are the whole of this file. It loads
//! `CS` and `SS` from `STAR`, puts the return address in `RCX` and the flags
//! in `R11`, masks the flags with `SFMASK`, and jumps to `LSTAR`. It does
//! **not** switch stacks: the first instruction of the kernel runs on the
//! user's stack, at the user's mercy. It does not save a single register
//! beyond the two it clobbers. And `RCX` and `R11` are gone, which is why the
//! ABI puts the fourth argument in `R10` rather than `RCX` as the C one does.
//!
//! So the trampoline's first job is to get off the user stack without using a
//! register, which is what `GS` and the per-CPU record are for. `swapgs`
//! exchanges `GS_BASE` with `KERNEL_GS_BASE`, so one instruction turns a
//! user-controlled `GS` into the kernel's own — and the same instruction on
//! the way out turns it back.
//!
//! # The rule that makes `swapgs` safe
//!
//! `swapgs` is not idempotent and the processor does not tell you which way
//! round `GS` currently is. Get it wrong and the kernel reads its per-CPU
//! record through a pointer the program chose. The rule here is the one Linux
//! uses: **swap exactly when the privilege level changed**, decided by the
//! saved `CS`, and never on a path that cannot have come from user mode.

use core::mem::offset_of;

use crate::smp::PerCpu;
use crate::trap::{Abi, Outcome};

use super::cpu;
use super::gdt;

pub(super) mod check;

/// `IA32_EFER`. Bit 0 enables `SYSCALL`/`SYSRET`.
const IA32_EFER: u32 = 0xC000_0080;
/// `IA32_STAR`. Holds the two segment bases.
const IA32_STAR: u32 = 0xC000_0081;
/// `IA32_LSTAR`. The 64-bit entry point.
const IA32_LSTAR: u32 = 0xC000_0082;
/// `IA32_CSTAR`. Where `SYSCALL` from compatibility mode goes, on AMD; Intel
/// raises `#UD` instead.
const IA32_CSTAR: u32 = 0xC000_0083;
/// `IA32_SYSENTER_CS`. Zero makes `SYSENTER` fault with `#GP`.
const IA32_SYSENTER_CS: u32 = 0x174;
/// `IA32_FMASK`. Flags cleared on entry.
const IA32_FMASK: u32 = 0xC000_0084;
/// `IA32_KERNEL_GS_BASE`. What `swapgs` exchanges `GS_BASE` with.
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;
/// `IA32_FS_BASE`, which `arch_prctl(ARCH_SET_FS)` writes: a program's thread
/// pointer, and the first thing a libc sets up.
const IA32_FS_BASE: u32 = 0xC000_0100;

/// `EFER.SCE`: system call extensions.
const EFER_SCE: u64 = 1;

/// Flags cleared on entry to the kernel.
///
/// `IF` matters because without it the kernel would run the first
/// instructions of every system call with interrupts still enabled on a stack
/// it has not switched to yet. `TF` matters for the same reason and is worse:
/// a program may set it with `popfq`, and the processor decides whether to
/// single-step after `SYSCALL` from the flags the instruction leaves behind. Left
/// set, `#DB` is raised on the trampoline's first instruction, in ring 0,
/// before `swapgs` and before the stack switch -- so the processor pushes the
/// exception frame onto whatever `RSP` the program chose, a kernel address
/// included, and the kernel stops. `DF` matters because the System V ABI lets a
/// program leave the direction flag set and every `rep movs` in the kernel
/// assumes it is clear. `NT` and `RF` are cleared because neither describes
/// the kernel's own state: `NT` marks a nested task, and `RF` suppresses an
/// instruction breakpoint on whatever instruction runs next. `AC` is cleared
/// so that a future `SMAP` cannot be left open by the caller. Linux masks all
/// of these and a few arithmetic flags besides, which are harmless either way.
///
/// The program's own flags are untouched: `SYSCALL` saved them in `R11` before
/// masking, and `SYSRET` puts them back, trap flag and all. A program that
/// returns from a call with the trap flag set then takes `#DB` in ring 3 at its
/// return address, before running anything there, and that is its own
/// business: the trap raises `SIGTRAP`, which by default ends the program, not
/// the kernel. Linux returns through `IRET` whenever the flag is set, so that a
/// single-stepping debugger sees one instruction run first; that belongs with
/// `ptrace`, not here.
const FMASK: u64 = RFLAGS_TF | RFLAGS_IF | RFLAGS_DF | RFLAGS_NT | RFLAGS_RF | RFLAGS_AC;

/// `RFLAGS.TF`: trap after each instruction.
const RFLAGS_TF: u64 = 1 << 8;
/// `RFLAGS.IF`: interrupts enabled.
const RFLAGS_IF: u64 = 1 << 9;
/// `RFLAGS.DF`: string operations count down.
const RFLAGS_DF: u64 = 1 << 10;
/// `RFLAGS.NT`: nested task.
const RFLAGS_NT: u64 = 1 << 14;
/// `RFLAGS.RF`: resume without an instruction breakpoint.
const RFLAGS_RF: u64 = 1 << 16;
/// `RFLAGS.AC`: alignment check, and `SMAP`'s override.
const RFLAGS_AC: u64 = 1 << 18;

// The trampoline reaches the per-CPU record from assembly, so these offsets
// are part of the contract between this file and `crate::smp`. Asserting them
// here means a field reordered over there fails the build rather than sending
// the kernel to a stack made of somebody's `logical` number.
const KERNEL_STACK_OFFSET: usize = offset_of!(PerCpu, kernel_stack);
const USER_STACK_OFFSET: usize = offset_of!(PerCpu, user_stack);
const _: () = assert!(
    KERNEL_STACK_OFFSET == 8,
    "the syscall trampoline loads the kernel stack from gs:8"
);
const _: () = assert!(
    USER_STACK_OFFSET == 16,
    "the syscall trampoline parks the user stack at gs:16"
);

/// A user program's registers, as the trampoline saves them.
///
/// Laid out to match the pushes in `ferrix_syscall_stub` exactly, in reverse:
/// the last thing pushed is the first field. `repr(C)` because assembly is the
/// other half of this type's definition.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub(crate) struct SyscallFrame {
    /// Callee-saved, and the ABI's sixth argument register is not among them.
    pub(crate) r15: u64,
    /// Callee-saved.
    pub(crate) r14: u64,
    /// Callee-saved.
    pub(crate) r13: u64,
    /// Callee-saved.
    pub(crate) r12: u64,
    /// Callee-saved.
    pub(crate) rbp: u64,
    /// Callee-saved.
    pub(crate) rbx: u64,
    /// Sixth argument.
    pub(crate) r9: u64,
    /// Fifth argument.
    pub(crate) r8: u64,
    /// Fourth argument. The ABI uses `R10` here and not `RCX`, because
    /// `SYSCALL` destroys `RCX`.
    pub(crate) r10: u64,
    /// Third argument.
    pub(crate) rdx: u64,
    /// Second argument.
    pub(crate) rsi: u64,
    /// First argument.
    pub(crate) rdi: u64,
    /// The system call number on the way in, the result on the way out.
    pub(crate) rax: u64,
    /// The user `RFLAGS`, which `SYSCALL` left here.
    pub(crate) r11: u64,
    /// The user return address, which `SYSCALL` left here.
    pub(crate) rcx: u64,
    /// The user stack pointer, taken out of the per-CPU scratch word.
    pub(crate) user_rsp: u64,
}

core::arch::global_asm!(
    r#"
// On a processor MDS reaches, clear the buffers it reads on the way out to
// ring 3, after the last load of anything the kernel holds: `VERW` of a data
// selector does it where microcode says `MD_CLEAR`. A byte decides at run
// time, because only the processor can say whether it needs it; the flags it
// clobbers are about to be replaced from R11 or the interrupt frame. Used by
// every return to ring 3, here and in `super::trap`. See `super::speculation`.
.macro FERRIX_CLEAR_BUFFERS
.if {hardened}
    testb $1, {clear}(%rip)
    jz 3f
    verw {selector}(%rip)
3:
.endif
.endm

.section .text
.globl ferrix_syscall_stub
.align 16
ferrix_syscall_stub:
    // Off the user stack first, before anything can be pushed. `swapgs` makes
    // GS the kernel's; the two GS-relative words are the only memory this
    // code can name without a stack.
    swapgs
    movq %rsp, %gs:16
    movq %gs:8, %rsp

    // Rebuild the user's state as a frame, in `SyscallFrame` order.
    pushq %gs:16          // user_rsp
    pushq %rcx            // user rip
    pushq %r11            // user rflags
    pushq %rax
    pushq %rdi
    pushq %rsi
    pushq %rdx
    pushq %r10
    pushq %r8
    pushq %r9
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    // Nothing a program chose reaches the kernel's code in a register: every
    // value is in the frame now, and a register still holding one is an
    // operand a mispredicted branch in the handler could use (Spectre v1).
    // RBP too, which also ends the backtrace's frame chain here.
.if {hardened}
    xorl %eax, %eax
    xorl %ecx, %ecx
    xorl %edx, %edx
    xorl %esi, %esi
    xorl %r8d, %r8d
    xorl %r9d, %r9d
    xorl %r10d, %r10d
    xorl %r11d, %r11d
    xorl %ebx, %ebx
    xorl %ebp, %ebp
    xorl %r12d, %r12d
    xorl %r13d, %r13d
    xorl %r14d, %r14d
    xorl %r15d, %r15d
.endif

    cld
    movq %rsp, %rdi
    callq ferrix_syscall_entry

    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    popq %r9
    popq %r8
    popq %r10
    popq %rdx
    popq %rsi
    popq %rdi
    popq %rax
    popq %r11             // user rflags, back where SYSRET wants it
    popq %rcx             // user rip, back where SYSRET wants it
    popq %rsp             // user stack, straight into RSP

    FERRIX_CLEAR_BUFFERS
    swapgs
    // Named for the boot check that breaks here: ring 0, the program's stack
    // and GS, which only the paranoid entry survives. See `super::paranoid`.
.globl ferrix_syscall_sysret
ferrix_syscall_sysret:
    sysretq

// Enter ring 3 for the first time.
//   rdi = entry point, rsi = user stack pointer
// Never returns: a program leaves ring 3 through a system call or a trap, and
// for good through `exit_group`, which ends its task in the scheduler.
.globl ferrix_enter_user
.align 16
ferrix_enter_user:
    // Masked from here to the `sysretq`, which opens them from R11. Between
    // loading the user stack pointer and dropping privilege this is ring 0 on a
    // user stack, and an interrupt taken there would be pushed onto it.
    cli

    // `SYSRET` takes the address from RCX and the flags from R11, which is
    // exactly the shape of a return from a system call that never happened.
    movq %rdi, %rcx
    movq %rsi, %rsp
    // Interrupts open in ring 3: a program is a scheduled task, carrying its
    // own address space, thread pointer and entry stack, so a tick there is an
    // ordinary preemption. Bit 1 is reserved and always set.
    movq $0x202, %r11

    // The one value a program may be handed on entry, in the first argument
    // register: zero for a Linux program, a native process's bootstrap handle.
    // Taken from RDX before the clearing below reaches it.
    movq %rdx, %rdi

    // Nothing of the kernel's may survive into ring 3. A register left holding
    // a kernel pointer is an information leak that no test will ever notice.
    xorq %rax, %rax
    xorq %rbx, %rbx
    xorq %rdx, %rdx
    xorq %rsi, %rsi
    xorq %rbp, %rbp
    xorq %r8, %r8
    xorq %r9, %r9
    xorq %r10, %r10
    xorq %r12, %r12
    xorq %r13, %r13
    xorq %r14, %r14
    xorq %r15, %r15

    FERRIX_CLEAR_BUFFERS
    swapgs
    sysretq

// `SYSCALL` from compatibility mode, which only AMD processors take: Intel
// raises `#UD` for it. Neither libc enters this way without an `AT_SYSINFO`
// that points here, and Ferrix passes none (`docs/I386.md` §2), so it is
// answered `ENOSYS` rather than supported. It cannot be left at zero: any
// program can far-jump into the 32-bit code segment, and `CSTAR` zero would
// be a ring-0 jump to address zero on the program's stack.
//
// Nothing is pushed and nothing is read, so the stack is not switched and
// `GS` not swapped; `SFMASK` has closed interrupts. `SYSRETL` returns to the
// instruction after, in compatibility mode, with the flags from R11. AMD's
// `SYSRET` leaves `SS`'s cached attributes as they were, which is why every
// other way into 32-bit code is `IRETQ` (`enter_user`); here that is harmless,
// because `SYSCALL` has just loaded them and nothing between it and the
// return can leave `SS` null.
.globl ferrix_syscall32_stub
.align 16
ferrix_syscall32_stub:
    movq $-{enosys}, %rax
    FERRIX_CLEAR_BUFFERS
    sysretl

// Resume a program from a saved system call frame.
//   rdi = a `SyscallFrame`, in the order the stub above pushed it
// Never returns. The frame is popped exactly as the stub pops its own, so a
// fork child leaves the kernel by the same instructions its parent will.
.globl ferrix_resume_user
.align 16
ferrix_resume_user:
    cli
    movq %rdi, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    popq %r9
    popq %r8
    popq %r10
    popq %rdx
    popq %rsi
    popq %rdi
    popq %rax
    popq %r11
    popq %rcx
    popq %rsp
    FERRIX_CLEAR_BUFFERS
    swapgs
    sysretq
"#,
    hardened = const super::speculation::ENTRY_HARDENING,
    clear = sym super::speculation::CLEAR_CPU_BUFFERS,
    selector = sym super::speculation::VERW_SELECTOR,
    enosys = const ferrix_linux_abi::errno::Errno::ENOSYS.0,
    options(att_syntax)
);

/// Where the `CSTAR` entry point is: the stub that refuses a `SYSCALL` from
/// compatibility mode.
fn cstar_stub_address() -> u64 {
    ferrix_syscall32_stub as *const () as usize as u64
}

/// Where the `LSTAR` entry point is: what `IA32_LSTAR` holds, and what the
/// system call window check puts a breakpoint on.
///
/// The one place outside the assembly that names the symbol. Two `extern`
/// declarations of one symbol with different types are merged by the release
/// profile's link-time optimisation into one of them and a renamed
/// `ferrix_syscall_stub.1` nothing defines, which is how the release build
/// stopped linking once the window check declared it as a `static`.
pub(super) fn stub_address() -> u64 {
    ferrix_syscall_stub as *const () as usize as u64
}

unsafe extern "C" {
    /// The `LSTAR` entry point, defined in the block above.
    fn ferrix_syscall_stub();
    /// Enter ring 3 at `entry` on `stack`.
    fn ferrix_enter_user(entry: u64, stack: u64, argument: u64) -> !;
    /// The `CSTAR` entry point, defined in the block above.
    fn ferrix_syscall32_stub();
    /// Resume ring 3 from a saved system call frame.
    fn ferrix_resume_user(frame: *const SyscallFrame) -> !;
}

/// A program's registers as its system call saved them.
///
/// What a fork child starts from: the same registers as the parent at the
/// moment it asked, with the return register changed. Opaque outside this
/// architecture, because every architecture keeps a different set and the
/// system call layer only ever copies one and asks for two changes to it.
///
/// Two shapes, because there are two ways in: `SYSCALL`'s frame, which
/// `SYSRET` resumes, and a trap frame from `int $0x80`, which only `IRETQ`
/// can resume -- a 32-bit program's, whose child has to come back in
/// compatibility mode with every register, `RCX` and `R11` included
/// (`docs/I386.md` I2b).
#[derive(Debug, Clone, Copy)]
pub(crate) enum UserRegs {
    /// Saved by `SYSCALL`'s trampoline.
    Syscall(SyscallFrame),
    /// Saved by the trap stub: a system call through `int $0x80`.
    Trap(super::trap::TrapFrame),
}

impl UserRegs {
    /// The same registers, as a child sees them: the call returned zero.
    pub(crate) const fn for_child(&self) -> UserRegs {
        match *self {
            UserRegs::Syscall(mut frame) => {
                frame.rax = 0;
                UserRegs::Syscall(frame)
            }
            UserRegs::Trap(mut frame) => {
                frame.rax = 0;
                UserRegs::Trap(frame)
            }
        }
    }

    /// Start on `stack` instead, as `clone` with a stack argument asks. The
    /// stack pointer is in the frame on this architecture, so `state` is
    /// untouched.
    pub(crate) const fn set_stack(&mut self, state: &mut super::UserState, stack: u64) {
        let _ = state;
        match self {
            UserRegs::Syscall(frame) => frame.user_rsp = stack,
            UserRegs::Trap(frame) => frame.rsp = stack,
        }
    }

    /// The stack pointer the program made the call with.
    pub(crate) const fn stack_pointer(&self) -> u64 {
        match self {
            UserRegs::Syscall(frame) => frame.user_rsp,
            UserRegs::Trap(frame) => frame.rsp,
        }
    }
}

/// Resume user mode from `regs`, on the running task's kernel stack. Does not
/// return.
///
/// # Safety
///
/// (CONTEXT) Must be called by a user task with its address space installed and its
/// user state loaded, and `regs` must be a frame a system call from that
/// address space saved.
pub(crate) unsafe fn resume_user(regs: &UserRegs) -> ! {
    match regs {
        // SAFETY: (CONTEXT) the caller's guarantee is the assembly's contract; the frame
        // is read before anything is pushed below it.
        UserRegs::Syscall(frame) => unsafe { ferrix_resume_user(core::ptr::from_ref(frame)) },
        UserRegs::Trap(frame) => {
            let context = super::signal::UserContext::from_trap(frame).sanitised();
            // SAFETY: (CONTEXT) the caller's guarantee; a trap frame from ring 3, whose
            // selectors and addresses the processor itself saved.
            unsafe { super::signal::resume_context(&context) }
        }
    }
}

/// Where the assembly hands a system call to the rest of the kernel.
///
/// The argument order is this architecture's, not the C one: the ABI puts the
/// fourth argument in `R10` because `SYSCALL` destroys `RCX`. Getting that
/// wrong gives every four-argument call a garbage fourth argument, which for
/// `mmap` is the flags word and so fails loudly, and for `openat` is the mode
/// and so does not.
///
/// # Safety
///
/// Called only from `ferrix_syscall_stub`, with `frame` pointing at the frame
/// it just built on this processor's kernel stack.
#[unsafe(no_mangle)]
extern "C" fn ferrix_syscall_entry(frame: &mut SyscallFrame) {
    // Step 4's fast path for `channel_write_read`, on a boot that registered
    // one (`ferrix.fastpath=on`, T1): first, before the filter, which its own
    // T2 stands in for, and with interrupts still masked. Every entry measure
    // has run in the stub by here (`docs/OPAQUE-KERNEL.md` §9.7, condition
    // 2). A declined call goes on below as any other.
    if frame.rax as usize == ferrix_native_abi::nr::CHANNEL_WRITE_READ
        && let Some(fast) = crate::trap::fast_write_read()
    {
        // The call blocks, and lets the vector registers go across it (3a).
        mark_vectors_dead(true);
        let args = [
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ];
        match fast(&args) {
            crate::trap::Fast::Declined => mark_vectors_dead(false),
            crate::trap::Fast::Tail(outcome) => return frame_tail(frame, outcome),
            crate::trap::Fast::Done(outcome) => {
                mark_vectors_dead(false);
                return leave(frame, outcome);
            }
        }
    }

    let args = crate::trap::SyscallArgs {
        abi: Abi::Native,
        number: frame.rax as usize,
        args: [
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ],
        // `SYSCALL` left the address of the next instruction in `RCX`.
        ip: frame.rcx,
    };

    // The registered filter looks at the call first, before the answers below
    // that never reach the dispatcher, so that a filter that denies
    // `arch_prctl` or `rt_sigreturn` is obeyed (`docs/SECCOMP.md` §3.3). An
    // answer is applied as the dispatcher's is; nothing registered is
    // `Continue`.
    let outcome = match crate::trap::filter_system_call(&args) {
        Some(outcome) => outcome,
        None => {
            // A native number goes straight on to the dispatcher, which sends
            // it to the native ABI by the same range test; only a Linux
            // number is decoded here, once, for the two calls this entry
            // answers itself.
            if let Early::Linux(Some(call)) = early(args.number)
                && answer_here(frame, &args, call)
            {
                return;
            }
            // A native call that blocks lets the caller's vector registers go
            // for its length (`docs/OPAQUE-KERNEL.md` §9.8, 3a): the mark tells
            // the switch so, and is lowered as the call returns, before
            // anything on the way out can block in a call that keeps them.
            let blocking = vectors_die_in(args.number);
            if blocking {
                mark_vectors_dead(true);
            }
            // Open while the call is served: a call may block, and one that spins
            // waiting for input must not keep the processor from switching away.
            // `SFMASK` closed them on entry, and they are closed again before the
            // frame is restored, because the way out swaps `GS` on a live stack.
            super::enable_interrupts();
            let regs = UserRegs::Syscall(*frame);
            let outcome = crate::trap::system_call(&args, Some(&regs));
            super::disable_interrupts();
            if blocking {
                mark_vectors_dead(false);
            }
            outcome
        }
    };
    leave(frame, outcome);
}

/// The fast path's frame tail (`docs/OPAQUE-KERNEL.md` §9.7, part 2): the
/// reply a commit handed this task goes into its frame, in the four
/// registers `Outcome::ReturnWords` writes. With nothing due -- no
/// pending work, no decision asked of this processor, no move between jobs,
/// nothing that may filter it -- the call ends here, `IN_CALL` lowered, and
/// the stub's own exit runs. Otherwise the general branch: interrupts
/// opened, the `may_block` check, the call's way back as `trap::system_call`
/// makes it, and the entry's way out. With interrupts masked.
fn frame_tail(frame: &mut SyscallFrame, outcome: Outcome) {
    if crate::sched::nothing_due_here() && crate::trap::filter_quiet() {
        // No decision was asked of this processor (`nothing_due_here`), so
        // `call_left`'s is not made: its flag alone.
        crate::sched::set_in_call_masked(false);
        mark_vectors_dead(false);
        write_outcome(frame, outcome);
        return;
    }
    super::enable_interrupts();
    if !crate::sched::may_block() {
        crate::panic::fatal!(
            crate::panic::catalog::FAST_PATH_CONTINUATION_MASKED,
            "the fast path's frame tail took its general branch where its task may not block"
        );
    }
    crate::sched::regroup_current();
    crate::sched::call_left();
    super::disable_interrupts();
    mark_vectors_dead(false);
    leave(frame, outcome);
}

/// Write `outcome` into the frame, then the way back to user mode's look:
/// how every `SYSCALL` leaves the kernel but the frame tail's quiet one.
fn leave(frame: &mut SyscallFrame, outcome: Outcome) {
    write_outcome(frame, outcome);

    // On the way back: a process ended from outside ends here, a stopped one
    // waits, and a signal with a handler is delivered by pointing the frame at
    // it. See `crate::syscall::deliver`.
    if let Some(path) = crate::trap::return_path()
        && crate::trap::attention_due(path)
    {
        let mut context = super::signal::UserContext::from_syscall(frame);
        (path.return_to_user)(&mut context);
        context.store_syscall(frame);
    }
}

/// Put a call's answer into its frame, where `SYSRET`'s way out restores
/// the registers from.
fn write_outcome(frame: &mut SyscallFrame, outcome: Outcome) {
    match outcome {
        Outcome::Return(value) => {
            frame.rax = value as u64;
        }
        // The value in RAX, the words in the second, third and fourth
        // argument registers, which `SYSRET`'s way out restores from here.
        Outcome::ReturnWords { value, words } => {
            frame.rax = value as u64;
            frame.rsi = words[0];
            frame.rdx = words[1];
            frame.r10 = words[2];
        }
        Outcome::Enter { entry, stack, abi } => enter_program(frame, entry, stack, abi),
    }
}

/// Whether `number` is one of the three native calls declared to destroy the
/// caller's vector registers, as across a function call: `channel_write_read`,
/// `object_wait_one` and `port_wait`, the native calls that block (3a). By
/// number, on this entry alone: no Linux number, and nothing through
/// `int $0x80`.
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

/// What the entry makes of a number before the dispatcher sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Early {
    /// A number in the native range, which no Linux table is asked about.
    Native,
    /// A Linux number, decoded once against x86-64's table behind its clamp:
    /// `None` for a number the table does not hold.
    Linux(Option<ferrix_linux_abi::nr::Syscall>),
}

/// Sort a number from `SYSCALL`'s entry: the native range first, by a
/// compare, so that a native number is never decoded against the Linux table
/// (whose clamp would send it to `None` anyway, at the cost of a decode); a
/// Linux number is decoded once, and [`answer_here`] matches that one answer
/// against both of its calls.
pub(super) fn early(number: usize) -> Early {
    if ferrix_native_abi::nr::is_native(number) {
        return Early::Native;
    }
    Early::Linux(super::decode_syscall(number))
}

/// The two calls this entry answers itself, before the dispatcher, given the
/// call [`early`] decoded: `true` when the call was answered and the frame
/// holds the result. `rt_sigreturn` does not come back.
fn answer_here(
    frame: &mut SyscallFrame,
    args: &crate::trap::SyscallArgs,
    call: ferrix_linux_abi::nr::Syscall,
) -> bool {
    // One call is answered before dispatch, because it is a fact about this
    // processor rather than about the process: `arch_prctl(ARCH_SET_FS)` writes
    // an MSR. It exists on no other architecture -- AArch64 writes `TPIDR_EL0`
    // itself and ARMv7-A has `set_tls` -- so there is nothing for an
    // architecture-neutral dispatch table to say about it. The value survives a
    // switch to another task because the scheduler saves `FS_BASE` with the
    // rest of the program's user state.
    if call == ferrix_linux_abi::nr::Syscall::ArchPrctl {
        frame.rax = arch_prctl(args.args[0], args.args[1]) as u64;
        return true;
    }

    // `rt_sigreturn` puts back every register a signal interrupted, `RCX` and
    // `R11` among them, which `SYSRET` cannot load: it leaves through the trap
    // stub's `IRETQ` instead, and never returns here. See `super::signal`.
    if call == ferrix_linux_abi::nr::Syscall::RtSigreturn {
        let mut context = super::signal::UserContext::from_syscall(frame);
        super::enable_interrupts();
        if let Some(path) = crate::trap::return_path() {
            (path.sigreturn)(&mut context, true);
        }
        super::disable_interrupts();
        if let Some(path) = crate::trap::return_path()
            && crate::trap::attention_due(path)
        {
            (path.return_to_user)(&mut context);
        }
        // SAFETY: (CONTEXT) this task's own system call, on its own kernel stack, with
        // nothing owned on it; `context` holds ring 3's selectors and user
        // instruction and stack pointers, which the frame restore checked, or
        // the process ended and `return_to_user` did not come back.
        unsafe { super::signal::resume_context(&context) }
    }
    false
}

/// Replace the frame with a program's first registers: `execve`, and the child
/// side of a fresh `clone`.
fn enter_program(frame: &mut SyscallFrame, entry: u64, stack: u64, abi: Abi) {
    if abi == Abi::Compat {
        enter_compat_after_execve(entry, stack);
    }
    // The registers this frame holds belong to a program that no longer exists,
    // so they are replaced rather than returned into. Everything else is
    // cleared for the same reason `ferrix_enter_user` clears it -- a register
    // carrying a kernel value into ring 3 is a leak nothing tests.
    *frame = SyscallFrame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        rbp: 0,
        rbx: 0,
        r9: 0,
        r8: 0,
        r10: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        rax: 0,
        // `SYSRET` takes the flags from R11 and the address from RCX, which is
        // why an entry point can be delivered by returning. Interrupts open, as
        // `ferrix_enter_user` leaves them.
        r11: 0x202,
        rcx: entry,
        user_rsp: stack,
    };
}

/// Leave an `execve` of a 32-bit image, made through `SYSCALL`, into the new
/// program. Compatibility mode is entered by `IRETQ` alone (`enter_user` says
/// why), so this leaves the way `rt_sigreturn` does, through the trap stub's
/// restore path, after the way back has had its look at the new program.
fn enter_compat_after_execve(entry: u64, stack: u64) -> ! {
    super::switch::enter_compat_segments();
    let mut context = super::signal::UserContext::entering(entry, stack, Abi::Compat);
    if let Some(path) = crate::trap::return_path()
        && crate::trap::attention_due(path)
    {
        (path.return_to_user)(&mut context);
    }
    // SAFETY: (CONTEXT) the running task's own system call, on its own kernel stack,
    // where `ferrix_syscall_entry` owns nothing; `context` holds ring 3's
    // 32-bit selectors and the entry and stack `execve` placed in the new
    // image, both user addresses, or the process ended and `return_to_user`
    // did not come back.
    unsafe { super::signal::resume_context(&context) }
}

/// `ARCH_SET_FS`, and the three requests that are not it.
///
/// Only the first matters: a static binary cannot start without it, because a
/// libc that cannot place its thread-local block aborts before `main`. The
/// others are answered rather than dispatched so that a program asking gets
/// `EINVAL` and not `ENOSYS`, which is the difference between "this kernel
/// does not know that request" and "this kernel has no `arch_prctl`".
fn arch_prctl(code: u64, value: u64) -> isize {
    /// Set the base `FS` resolves against: the thread pointer.
    const ARCH_SET_FS: u64 = 0x1002;

    match code {
        ARCH_SET_FS => {
            // A thread pointer must be a user address. Nothing here follows
            // it, but a program that set it to a kernel address would have
            // every `FS`-relative access it made afterwards resolve there.
            if !ferrix_bootinfo::is_user_address(value) {
                return ferrix_linux_abi::errno::Errno::EPERM.as_return_value();
            }
            // SAFETY: (CONTEXT) a canonical user address, written to this processor's
            // `FS_BASE`; it changes only how user accesses resolve.
            unsafe { set_thread_pointer(value) };
            // And to the running task's record, which the switch loads and
            // never reads back (3b). The entry answers this call before it
            // opens interrupts, so no switch comes between the two writes.
            // SAFETY: (CONTEXT) interrupts masked, in the running task's own call.
            let _ = unsafe {
                crate::sched::with_own_user_state(|state| state.set_thread_pointer(value))
            };
            0
        }
        // Everything else, `ARCH_GET_FS` included. Reading the thread pointer
        // back means writing it through a user pointer, which needs the
        // caller's address space -- and this path deliberately does not have
        // one, because it is answering a question about the processor. No
        // libc asks at startup; when something does, it belongs in the
        // dispatch table with a `Process` in hand rather than here.
        _ => ferrix_linux_abi::errno::Errno::EINVAL.as_return_value(),
    }
}

/// Turn `SYSCALL` on for this processor.
///
/// Per processor, because every MSR here is per processor. A secondary that
/// skipped this would take `#UD` on the first system call a program made on
/// it, long after boot and nowhere near the cause.
///
/// # Safety
///
/// (ENTRY) Must run on each processor after its per-CPU record is installed in `GS`,
/// and before any program makes a system call there. The GDT the selectors
/// name need not be loaded yet, only by the time that call is made.
pub(crate) unsafe fn init() {
    // SAFETY: (ENTRY) `IA32_EFER` exists on every 64-bit x86; setting SCE only enables
    // an instruction that faults until `LSTAR` is set, two lines below.
    let efer = unsafe { cpu::read_msr(IA32_EFER) };
    // SAFETY: (ENTRY) as above.
    unsafe { cpu::write_msr(IA32_EFER, efer | EFER_SCE) };

    // STAR[47:32] is the kernel selector base, STAR[63:48] the user one.
    //
    // The user base carries RPL 3, and has to. `SYSRET` loads CS from base + 16
    // and forces RPL 3 into it, but loads SS from base + 8 *as written*. With a
    // bare `0x18` a program runs at CPL 3 on SS `0x20`, which nothing checks
    // until an exception pushes that SS and `iretq` back to ring 3 refuses it
    // with `#GP(0x20)` -- on hardware and under KVM, never under `tcg`, which
    // is why it passed the boot test. Linux uses `__USER32_CS | 3` for this.
    let star = (u64::from(gdt::KERNEL_CODE) << 32) | (u64::from(gdt::SYSRET_BASE | 3) << 48);
    // SAFETY: (ENTRY) the selectors are this processor's own GDT entries, and the
    // layout `SYSRET` computes from is asserted at their definition.
    unsafe { cpu::write_msr(IA32_STAR, star) };

    // SAFETY: (ENTRY) the address of a function in the kernel's own text.
    unsafe { cpu::write_msr(IA32_LSTAR, stub_address()) };
    // SAFETY: (ENTRY) likewise; the stub answers `ENOSYS` and returns.
    unsafe { cpu::write_msr(IA32_CSTAR, cstar_stub_address()) };
    // Zero, so that `SYSENTER` from compatibility mode faults rather than
    // entering the kernel wherever firmware left `SYSENTER_EIP` pointing.
    // Linux leaves the same zero when it has no 32-bit entry there.
    // SAFETY: (ENTRY) the MSR exists on every 64-bit x86; zero enables nothing.
    unsafe { cpu::write_msr(IA32_SYSENTER_CS, 0) };
    // SAFETY: (ENTRY) a mask of flag bits.
    unsafe { cpu::write_msr(IA32_FMASK, FMASK) };

    // While the kernel runs, `GS_BASE` holds its per-CPU record -- which
    // `set_cpu_local` has just put there -- and the shadow holds the
    // program's. The first `swapgs` on a processor is always on the way *out*
    // to ring 3, so what the shadow holds now is the `GS` base every program
    // starts with: zero, as on Linux.
    //
    // It used to be the kernel's record, parked "for the first swap to find".
    // Both halves then held the same address, so every program ran with the
    // per-CPU record as its `GS` base -- `mov %gs:0` in ring 3 faulted with it
    // as the address -- and nothing could tell a wrong `swapgs` from a right
    // one. The paranoid entry's rule, that no program's `GS` base is an
    // upper-half address, rests on this.
    // SAFETY: (ENTRY) the shadow MSR exists on every 64-bit x86, and zero is a
    // canonical address that no kernel code reaches through.
    unsafe { cpu::write_msr(IA32_KERNEL_GS_BASE, 0) };
}

/// The `GS` base of the program this processor last ran, read from the kernel
/// side of `swapgs`, where it is parked.
///
/// # Safety
///
/// (CONTEXT) Must be called from the kernel with its own `GS` installed -- anywhere but
/// the trampoline's ring-0 stretches before and after `swapgs`, which run with
/// interrupts masked and cannot call this.
pub(crate) unsafe fn program_gs_base() -> u64 {
    // SAFETY: (CONTEXT) reading the shadow MSR has no side effects.
    unsafe { cpu::read_msr(IA32_KERNEL_GS_BASE) }
}

/// Set the `GS` base the program on this processor will have when it next
/// runs: the shadow `swapgs` hands it on the way out.
///
/// # Safety
///
/// (CONTEXT) Must be called from the kernel with its own `GS` installed, as for
/// [`program_gs_base`], and `base` must be a user address, never the kernel's:
/// the paranoid entry tells the two apart by the sign bit.
pub(crate) unsafe fn set_program_gs_base(base: u64) {
    if cpu::FSGSBASE_ON.load(core::sync::atomic::Ordering::Relaxed) {
        // SAFETY: ablation: interrupts masked by the callers; the pair leaves
        // the kernel's GS base in place.
        unsafe {
            core::arch::asm!("swapgs", "wrgsbase {0}", "swapgs", in(reg) base, options(nostack, preserves_flags));
        }
        return;
    }
    // SAFETY: (CONTEXT) writing the shadow MSR changes only what the next `swapgs`
    // installs for ring 3.
    unsafe { cpu::write_msr(IA32_KERNEL_GS_BASE, base) };
}

/// Set the thread pointer a program reads through `FS`.
///
/// What `arch_prctl(ARCH_SET_FS)` does, and the one system call on this
/// architecture that a static binary cannot start without: a libc that cannot
/// place its TLS block aborts before `main`.
///
/// # Safety
///
/// (CONTEXT) `base` is a user address the program chose; nothing dereferences it here.
pub(crate) unsafe fn set_thread_pointer(base: u64) {
    if cpu::FSGSBASE_ON.load(core::sync::atomic::Ordering::Relaxed) {
        // SAFETY: ablation: CR4.FSGSBASE is set.
        unsafe { core::arch::asm!("wrfsbase {0}", in(reg) base, options(nostack, preserves_flags)) };
        return;
    }
    // SAFETY: (CONTEXT) `IA32_FS_BASE` accepts any canonical address. Writing it affects
    // only how this processor resolves `FS`-relative user accesses.
    unsafe { cpu::write_msr(IA32_FS_BASE, base) };
}

/// This processor's `FS_BASE`: the running program's thread pointer.
///
/// # Safety
///
/// (CONTEXT) Reads an MSR; the caller wants the value for the task whose state it is.
pub(crate) unsafe fn thread_pointer() -> u64 {
    // SAFETY: (CONTEXT) `IA32_FS_BASE` exists on every 64-bit x86 and reading it has no
    // side effects.
    unsafe { cpu::read_msr(IA32_FS_BASE) }
}

/// Point both of this processor's ways in from ring 3 at `top`.
///
/// There are two because `SYSCALL` switches no stack and an interrupt gate
/// does: the trampoline loads `gs:8`, and an exception or interrupt from ring 3
/// loads the TSS's `RSP0`. Both must name the running task's own kernel stack,
/// or two programs' traps land on one stack.
///
/// # Safety
///
/// (ENTRY) `top` must be the top of the kernel stack of the task this processor is
/// switching to, and a TSS must be loaded.
pub(crate) unsafe fn set_entry_stack(top: u64) {
    if let Some(cpu) = crate::smp::this_cpu() {
        cpu.kernel_stack
            .store(top, core::sync::atomic::Ordering::Relaxed);
    }
    // SAFETY: (ENTRY) the caller guarantees the stack and the TSS.
    unsafe { gdt::set_privilege_stack(top) };
}

/// Enter ring 3 for the first time, at `entry` on `stack`, with `argument` in
/// RDI, in the mode `abi` names. Does not return.
///
/// A 32-bit program is handed no argument: nothing starts one with a
/// bootstrap handle, and its registers are all zero. It is entered by
/// `IRETQ` rather than `SYSRET`: on AMD, `SYSRET` leaves `SS`'s cached
/// attributes as they were, and after an interrupt from ring 3 the kernel's
/// `SS` is null, which 32-bit code -- unlike 64-bit code -- then cannot push
/// through. So it leaves through the trap stub's restore path, which loads
/// `SS` whole and `DS` and `ES` first (`docs/I386.md` §3.3), from a frame
/// built for its first instruction.
///
/// # Safety
///
/// (CONTEXT) Must be called by a user task, on its own kernel stack, with its address
/// space installed and its entry stack set; `entry` and `stack` must be
/// addresses inside that space, and below 4 GiB for a 32-bit program.
pub(crate) unsafe fn enter_user(entry: u64, stack: u64, argument: u64, abi: Abi) -> ! {
    match abi {
        // SAFETY: (CONTEXT) the caller's guarantee is the assembly's contract.
        Abi::Native => unsafe { ferrix_enter_user(entry, stack, argument) },
        Abi::Compat => {
            super::switch::enter_compat_segments();
            let context = super::signal::UserContext::entering(entry, stack, abi);
            // SAFETY: (CONTEXT) the caller's guarantee: this task's own kernel stack,
            // with its space and user state loaded and nothing owned on it,
            // and `context` a ring-3 frame at user addresses.
            unsafe { super::signal::resume_context(&context) }
        }
    }
}
