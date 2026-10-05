//! The context switch.
//!
//! # Why there is assembly here
//!
//! The same argument as x86-64's, in this architecture's registers: a
//! function that returns onto another stack cannot be written in Rust,
//! because the registers it would restore and the return address it would use
//! belong to the context it is in the middle of leaving.
//!
//! The AAPCS callee-saved set is `x19` through `x28`, the frame pointer `x29`
//! and the link register `x30`. There is no floating-point state to save: the
//! kernel is built for `aarch64-unknown-none-softfloat` and never touches the
//! SIMD registers, which is also why `d8`–`d15` — callee-saved on a target
//! that used them — do not appear here.

use core::arch::global_asm;

/// Bytes the switch pushes: twelve registers, which is also a multiple of the
/// sixteen `AArch64` requires of a stack pointer.
const FRAME_BYTES: u64 = 96;

global_asm!(
    r#"
.section .text

// void ferrix_switch(u64 *save, u64 next)
//   x0 = where to write this context's stack pointer
//   x1 = the stack pointer to resume
.globl ferrix_switch
ferrix_switch:
    stp x29, x30, [sp, #-96]!
    stp x27, x28, [sp, #16]
    stp x25, x26, [sp, #32]
    stp x23, x24, [sp, #48]
    stp x21, x22, [sp, #64]
    stp x19, x20, [sp, #80]
    mov x2, sp
    str x2, [x0]
    mov sp, x1
    ldp x19, x20, [sp, #80]
    ldp x21, x22, [sp, #64]
    ldp x23, x24, [sp, #48]
    ldp x25, x26, [sp, #32]
    ldp x27, x28, [sp, #16]
    ldp x29, x30, [sp], #96
    ret

// Where a task starts the first time it is switched to: `prepare_stack` left
// its entry point in x19 and its argument in x20.
.globl ferrix_task_entry
ferrix_task_entry:
    mov x0, x20
    blr x19
    brk #0
"#
);

unsafe extern "C" {
    /// Save this context and resume another. Declared here; defined above.
    fn ferrix_switch(save: *mut u64, next: u64);
    /// The first instruction a new task runs.
    fn ferrix_task_entry();
}

/// Stop running on this stack and continue on `next`, writing this context's
/// stack pointer to `save` so it can be resumed later.
///
/// # Safety
///
/// (CONTEXT) `save` must be the stack-pointer slot of the context calling this, and
/// `next` must be a stack pointer that [`prepare_stack`] produced or that an
/// earlier call to this function saved. No other processor may be running on
/// either stack, and both must stay mapped for as long as their contexts
/// exist.
pub(crate) unsafe fn switch_to(save: *mut u64, next: u64) {
    // SAFETY: (CONTEXT) the caller's guarantee is exactly the assembly's contract.
    unsafe { ferrix_switch(save, next) };
}

/// Lay out a stack so that switching to it calls `entry(argument)`.
///
/// # Safety
///
/// (CONTEXT) `top` must be the top of a mapped, writable stack of at least
/// [`FRAME_BYTES`], owned by the caller and not in use.
pub(crate) unsafe fn prepare_stack(
    top: u64,
    entry: extern "C" fn(usize) -> !,
    argument: usize,
) -> u64 {
    // Written out in the order the switch stores them, lowest address first,
    // so this reads against the `stp` sequence above rather than against a
    // list of indices. Two of the registers carry the entry point and its
    // argument, because the trampoline the link register names is the only
    // code that runs before Rust does and has nowhere else to read them from.
    let frame: [u64; 12] = [
        0,                                              // x29
        ferrix_task_entry as *const () as usize as u64, // x30, the link register
        0,                                              // x27
        0,                                              // x28
        0,                                              // x25
        0,                                              // x26
        0,                                              // x23
        0,                                              // x24
        0,                                              // x21
        0,                                              // x22
        entry as usize as u64,                          // x19
        argument as u64,                                // x20
    ];

    let stack_pointer = top - FRAME_BYTES;
    // SAFETY: (CONTEXT) the caller guarantees the stack is mapped, writable and theirs,
    // and the frame is written entirely inside it.
    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), stack_pointer as *mut u64, frame.len());
    };
    stack_pointer
}

/// What a program owns on this processor that no trap saves: its thread
/// pointer, and its floating-point and SIMD registers.
///
/// The kernel is built soft-float and never touches either, so a trap from EL0
/// leaves them exactly as the program had them, and one program running at a
/// time needed nothing more. Two programs taking turns do: the scheduler saves
/// the outgoing program's copy and loads the incoming one's whenever it
/// switches between tasks that run user code.
#[repr(C)]
#[derive(Debug, Clone)]
pub(crate) struct UserState {
    /// `TPIDR_EL0`, which EL0 writes for itself.
    thread_pointer: u64,
    /// `FPCR`.
    fpcr: u64,
    /// `FPSR`.
    fpsr: u64,
    /// Keeps the vectors at a sixteen-byte offset.
    reserved: u64,
    /// `q0` to `q31`, sixteen bytes each.
    vectors: [u8; 512],
}

const _: () = assert!(
    core::mem::offset_of!(UserState, vectors) == 32,
    "the save sequence stores q0 at offset 32"
);

impl UserState {
    /// A copy of the user state this processor holds right now: what a fork
    /// child inherits.
    ///
    /// # Safety
    ///
    /// (CONTEXT) The registers must be the calling task's own.
    pub(crate) unsafe fn capture() -> UserState {
        let mut state = UserState::new();
        // SAFETY: (CONTEXT) the caller's guarantee.
        unsafe { save_user_state(&mut state, false) };
        state
    }

    /// Give the program `pointer` as its thread pointer, as `CLONE_SETTLS`
    /// asks.
    pub(crate) const fn set_thread_pointer(&mut self, pointer: u64) {
        self.thread_pointer = pointer;
    }

    /// A 32-bit x86 program's thread-local segment: there are none on this
    /// architecture, and no table here maps a call that asks for one.
    pub(crate) const fn set_thread_area(&mut self, _index: usize, _descriptor: u64) -> bool {
        false
    }

    /// A program's state before it has run: no thread pointer, rounding to
    /// nearest, no floating-point exceptions trapped, every register zero.
    pub(crate) const fn new() -> UserState {
        UserState {
            thread_pointer: 0,
            fpcr: 0,
            fpsr: 0,
            reserved: 0,
            vectors: [0; 512],
        }
    }

    /// `FPCR` and `FPSR`, as a signal frame's `fpsimd_context` records them.
    pub(super) const fn fp_control(&self) -> (u64, u64) {
        (self.fpcr, self.fpsr)
    }

    /// `q0` to `q31`, sixteen little-endian bytes each.
    pub(super) const fn vectors(&self) -> &[u8; 512] {
        &self.vectors
    }

    /// Replace the floating-point and SIMD registers, as `rt_sigreturn` does.
    /// The thread pointer is left as it was.
    pub(super) const fn set_fp(&mut self, fpcr: u64, fpsr: u64, vectors: [u8; 512]) {
        self.fpcr = fpcr;
        self.fpsr = fpsr;
        self.vectors = vectors;
    }
}

global_asm!(
    r#"
.arch_extension fp
.arch_extension simd
.section .text

// void ferrix_user_save(UserState *state)
.globl ferrix_user_save
ferrix_user_save:
    mrs  x1, tpidr_el0
    mrs  x2, fpcr
    mrs  x3, fpsr
    stp  x1, x2, [x0, #0]
    str  x3, [x0, #16]
    stp  q0, q1, [x0, #32]
    stp  q2, q3, [x0, #64]
    stp  q4, q5, [x0, #96]
    stp  q6, q7, [x0, #128]
    stp  q8, q9, [x0, #160]
    stp  q10, q11, [x0, #192]
    stp  q12, q13, [x0, #224]
    stp  q14, q15, [x0, #256]
    stp  q16, q17, [x0, #288]
    stp  q18, q19, [x0, #320]
    stp  q20, q21, [x0, #352]
    stp  q22, q23, [x0, #384]
    stp  q24, q25, [x0, #416]
    stp  q26, q27, [x0, #448]
    stp  q28, q29, [x0, #480]
    stp  q30, q31, [x0, #512]
    ret

// void ferrix_user_restore(const UserState *state)
.globl ferrix_user_restore
ferrix_user_restore:
    ldp  x1, x2, [x0, #0]
    ldr  x3, [x0, #16]
    msr  tpidr_el0, x1
    msr  fpcr, x2
    msr  fpsr, x3
    ldp  q0, q1, [x0, #32]
    ldp  q2, q3, [x0, #64]
    ldp  q4, q5, [x0, #96]
    ldp  q6, q7, [x0, #128]
    ldp  q8, q9, [x0, #160]
    ldp  q10, q11, [x0, #192]
    ldp  q12, q13, [x0, #224]
    ldp  q14, q15, [x0, #256]
    ldp  q16, q17, [x0, #288]
    ldp  q18, q19, [x0, #320]
    ldp  q20, q21, [x0, #352]
    ldp  q22, q23, [x0, #384]
    ldp  q24, q25, [x0, #416]
    ldp  q26, q27, [x0, #448]
    ldp  q28, q29, [x0, #480]
    ldp  q30, q31, [x0, #512]
    ret
"#
);

unsafe extern "C" {
    /// Store this processor's user state into `state`.
    fn ferrix_user_save(state: *mut UserState);
    /// Load `state` into this processor's user registers.
    fn ferrix_user_restore(state: *const UserState);
}

/// Store the program state this processor holds into `state`, in full:
/// whether the task leaves `_blocked` matters only to x86-64's vector-state
/// contract (`docs/OPAQUE-KERNEL.md` §9.8, 3a), which this architecture does
/// not have.
///
/// # Safety
///
/// (CONTEXT) The registers must belong to the task `state` is for: it was the last task
/// with user state to run on this processor.
pub(crate) unsafe fn save_user_state(state: &mut UserState, _blocked: bool) {
    // SAFETY: (CONTEXT) `state` is a live, exclusively borrowed `UserState`, whose layout
    // the assembly's offsets are asserted against.
    unsafe { ferrix_user_save(core::ptr::from_mut(state)) };
}

/// Load `state` onto this processor for the task about to run.
///
/// `entry_stack` is for x86-64, which has to be told where an exception from
/// user mode lands. Here that is `SP_EL1`, which each task's own stack already
/// is when it returns to EL0.
///
/// # Safety
///
/// (CONTEXT) The task `state` belongs to must be the one this processor is switching to.
pub(crate) unsafe fn restore_user_state(state: &UserState, entry_stack: u64) {
    let _ = entry_stack;
    // SAFETY: (CONTEXT) as above, and loading user registers cannot affect the kernel,
    // which uses none of them.
    unsafe { ferrix_user_restore(core::ptr::from_ref(state)) };
}

/// Put this processor's user state back to a program's starting state, as
/// `execve` does.
///
/// # Safety
///
/// (CONTEXT) Must be called by the user task whose registers these are.
pub(crate) unsafe fn reset_user_state() {
    let fresh = UserState::new();
    // SAFETY: (CONTEXT) loading zeroed user registers cannot affect the kernel.
    unsafe { ferrix_user_restore(core::ptr::from_ref(&fresh)) };
}
