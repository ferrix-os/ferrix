//! The context switch.
//!
//! # Why there is assembly here
//!
//! The same argument as the other two architectures': a function that returns
//! onto another stack cannot be written in Rust. What is particular to this
//! one is which stack. Every exception on ARMv7-A is taken into a mode with
//! its own banked stack pointer, and `src/kernel/src/arch/armv7a/trap.rs` answers
//! that by keeping everything on the SVC-mode stack — so a task's context is
//! its SVC stack pointer, and this saves and restores exactly that.
//!
//! The AAPCS callee-saved set is `r4` through `r11` and the link register. A
//! ninth register is pushed with them as padding, because nine words would
//! leave the stack four-byte aligned and the ABI requires eight at a call.
//! There is no floating-point state: the kernel is soft-float throughout.

use core::arch::global_asm;

/// Bytes the switch pushes: nine registers and one word of padding.
const FRAME_BYTES: u32 = 40;

global_asm!(
    r#"
.arm
.section .text

// void ferrix_switch(u32 *save, u32 next)
//   r0 = where to write this context's stack pointer
//   r1 = the stack pointer to resume
.globl ferrix_switch
ferrix_switch:
    push {{r4-r11, lr}}
    sub  sp, sp, #4
    str  sp, [r0]
    mov  sp, r1
    add  sp, sp, #4
    pop  {{r4-r11, pc}}

// Where a task starts the first time it is switched to: `prepare_stack` left
// its entry point in r4 and its argument in r5.
.globl ferrix_task_entry
ferrix_task_entry:
    mov r0, r5
    blx r4
    udf #0
"#
);

unsafe extern "C" {
    /// Save this context and resume another. Declared here; defined above.
    fn ferrix_switch(save: *mut u32, next: u32);
    /// The first instruction a new task runs.
    fn ferrix_task_entry();
}

/// Stop running on this stack and continue on `next`, writing this context's
/// stack pointer to `save` so it can be resumed later.
///
/// The facade's addresses are 64-bit on every architecture; here they are
/// narrowed to the 32 bits an address actually has, which loses nothing.
///
/// # Safety
///
/// (CONTEXT) `save` must be the stack-pointer slot of the context calling this, and
/// `next` must be a stack pointer that [`prepare_stack`] produced or that an
/// earlier call to this function saved. No other processor may be running on
/// either stack, and both must stay mapped for as long as their contexts
/// exist.
pub(crate) unsafe fn switch_to(save: *mut u64, next: u64) {
    // SAFETY: (CONTEXT) the caller's guarantee is the assembly's contract. The slot is
    // a `u64` the kernel keeps for every architecture; on this one only its
    // low half is used, and the assembly writes exactly that half — so the
    // pointer is cast rather than the value, and the high half stays zero.
    unsafe { ferrix_switch(save.cast::<u32>(), next as u32) };
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
    // Lowest address first: the padding word the `sub` leaves, then r4
    // through r11, then the link register the `pop` loads into the program
    // counter. Written out rather than indexed so this reads against the
    // `push` above. Two of the registers carry the entry point and its
    // argument, because the trampoline named by the last word is the only
    // code that runs before Rust does and has nowhere else to read them from.
    let frame: [u32; 10] = [
        0,                                              // the alignment padding word
        entry as usize as u32,                          // r4
        argument as u32,                                // r5
        0,                                              // r6
        0,                                              // r7
        0,                                              // r8
        0,                                              // r9
        0,                                              // r10
        0,                                              // r11
        ferrix_task_entry as *const () as usize as u32, // lr, popped into pc
    ];

    let stack_pointer = (top as u32) - FRAME_BYTES;
    // SAFETY: (CONTEXT) the caller guarantees the stack is mapped, writable and theirs,
    // and the frame is written entirely inside it.
    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), stack_pointer as *mut u32, frame.len());
    };
    u64::from(stack_pointer)
}

/// What a program owns on this processor that no trap saves: its thread
/// pointer, and its floating-point registers.
///
/// The kernel is soft-float and never touches either, so a trap from USR mode
/// leaves them as the program had them. Two programs taking turns need them
/// saved and loaded by the scheduler whenever it switches between tasks that
/// run user code.
#[repr(C)]
#[derive(Debug, Clone)]
pub(crate) struct UserState {
    /// `TPIDRURO`, which `set_tls` asks the kernel to write.
    thread_pointer: u32,
    /// `FPSCR`.
    fpscr: u32,
    /// `d0` to `d31`; only the first sixteen are used on a core that has
    /// sixteen.
    doubles: [u64; 32],
    /// USR mode's banked stack pointer.
    ///
    /// No trap saves it: every exception is taken into a mode with a stack
    /// pointer of its own, and USR's is simply left in the register. So a
    /// second program running on the core between two of the first program's
    /// traps hands the first one back the second one's stack. x86-64 pushes the
    /// user stack pointer on every entry and AArch64 saves `SP_EL0` in the
    /// frame; this architecture keeps it here, where the other registers no
    /// trap saves already move from task to task.
    user_sp: u32,
    /// USR mode's banked link register, for the same reason.
    user_lr: u32,
}

const _: () = assert!(
    core::mem::offset_of!(UserState, fpscr) == 4,
    "the save sequence stores FPSCR at offset 4"
);
const _: () = assert!(
    core::mem::offset_of!(UserState, doubles) == 8,
    "the save sequence stores d0 at offset 8"
);
const _: () = assert!(
    core::mem::offset_of!(UserState, user_lr) == core::mem::offset_of!(UserState, user_sp) + 4,
    "the banked save stores lr one word after sp"
);

impl UserState {
    /// A program's state before it has run: no thread pointer, the default
    /// floating-point mode, every register zero.
    pub(crate) const fn new() -> UserState {
        UserState {
            thread_pointer: 0,
            fpscr: 0,
            doubles: [0; 32],
            user_sp: 0,
            user_lr: 0,
        }
    }

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
        self.thread_pointer = pointer as u32;
    }

    /// A 32-bit x86 program's thread-local segment: there are none on this
    /// architecture, and no table here maps a call that asks for one.
    pub(crate) const fn set_thread_area(&mut self, _index: usize, _descriptor: u64) -> bool {
        false
    }

    /// Give the program `stack` as its USR stack pointer.
    pub(crate) const fn set_user_stack(&mut self, stack: u32) {
        self.user_sp = stack;
    }

    /// `FPSCR` and `d0` to `d31`, as a signal frame's VFP record carries them.
    pub(super) const fn fp(&self) -> (u32, &[u64; 32]) {
        (self.fpscr, &self.doubles)
    }

    /// Replace the floating-point registers, as `sigreturn` does.
    pub(super) const fn set_fp(&mut self, fpscr: u32, doubles: [u64; 32]) {
        self.fpscr = fpscr;
        self.doubles = doubles;
    }
}

/// USR mode's banked stack pointer and link register, as they are now.
///
/// What a signal frame saves: no trap frame holds either on this architecture.
pub(super) fn user_banked() -> (u32, u32) {
    let mut words = [0_u32; 2];
    // SAFETY: (CONTEXT) two writable words; reading USR's banked registers through System
    // mode changes nothing.
    unsafe { ferrix_user_banked_save(words.as_mut_ptr()) };
    let [sp, lr] = words;
    (sp, lr)
}

/// Set USR mode's banked stack pointer and link register, as entering a
/// signal handler and returning from one do.
pub(super) fn set_user_banked(sp: u32, lr: u32) {
    let words = [sp, lr];
    // SAFETY: (CONTEXT) two readable words; loading USR's banked registers cannot affect
    // the kernel, which runs in SVC mode.
    unsafe { ferrix_user_banked_restore(words.as_ptr()) };
}

/// Load `state`'s floating-point registers and nothing else, if this core has
/// any: what `sigreturn` puts back.
///
/// # Safety
///
/// (CONTEXT) The registers must be the calling task's own.
pub(super) unsafe fn load_user_fpu(state: &UserState) {
    let doubles = super::cpu::user_fpu_doubles();
    if doubles != 0 {
        // SAFETY: (CONTEXT) the FPU exists and is enabled, and `state` is a live
        // `UserState` whose layout is asserted above; loading user registers
        // cannot affect the kernel, which uses none of them.
        unsafe { ferrix_user_fpu_restore(core::ptr::from_ref(state), u32::from(doubles == 32)) };
    }
}

global_asm!(
    r#"
.arm
.fpu vfpv3
.section .text

// void ferrix_user_banked_save(u32 *out): out[0] = USR sp, out[1] = USR lr.
// System mode shares USR's banked registers, so they are read from there, with
// the mode put back before returning through SVC's own lr.
.globl ferrix_user_banked_save
ferrix_user_banked_save:
    mrs    r3, cpsr
    cps    #0x1f
    str    sp, [r0]
    str    lr, [r0, #4]
    msr    cpsr_c, r3
    bx     lr

// void ferrix_user_banked_restore(const u32 *from)
.globl ferrix_user_banked_restore
ferrix_user_banked_restore:
    mrs    r3, cpsr
    cps    #0x1f
    ldr    sp, [r0]
    ldr    lr, [r0, #4]
    msr    cpsr_c, r3
    bx     lr

// void ferrix_fpu_enable(void): FPEXC.EN
.globl ferrix_fpu_enable
ferrix_fpu_enable:
    mov    r0, #0x40000000
    vmsr   fpexc, r0
    bx     lr

// u32 ferrix_fpu_features(void): MVFR0
.globl ferrix_fpu_features
ferrix_fpu_features:
    vmrs   r0, mvfr0
    bx     lr

// u32 ferrix_fpu_features1(void): MVFR1
.globl ferrix_fpu_features1
ferrix_fpu_features1:
    vmrs   r0, mvfr1
    bx     lr

// void ferrix_user_fpu_save(UserState *state, u32 all_32)
.globl ferrix_user_fpu_save
ferrix_user_fpu_save:
    vmrs   r2, fpscr
    str    r2, [r0, #4]
    add    r3, r0, #8
    vstmia r3!, {{d0-d15}}
    cmp    r1, #0
    beq    1f
    vstmia r3!, {{d16-d31}}
1:  bx     lr

// void ferrix_user_fpu_restore(const UserState *state, u32 all_32)
.globl ferrix_user_fpu_restore
ferrix_user_fpu_restore:
    ldr    r2, [r0, #4]
    vmsr   fpscr, r2
    add    r3, r0, #8
    vldmia r3!, {{d0-d15}}
    cmp    r1, #0
    beq    1f
    vldmia r3!, {{d16-d31}}
1:  bx     lr
"#
);

unsafe extern "C" {
    /// Store USR mode's banked stack pointer and link register at `out`.
    fn ferrix_user_banked_save(out: *mut u32);
    /// Load USR mode's banked stack pointer and link register from `from`.
    fn ferrix_user_banked_restore(from: *const u32);
    /// Set `FPEXC.EN`.
    fn ferrix_fpu_enable();
    /// Read `MVFR0`.
    fn ferrix_fpu_features() -> u32;
    /// Read `MVFR1`.
    fn ferrix_fpu_features1() -> u32;
    /// Store this processor's floating-point registers into `state`.
    fn ferrix_user_fpu_save(state: *mut UserState, all_32: u32);
    /// Load `state`'s floating-point registers onto this processor.
    fn ferrix_user_fpu_restore(state: *const UserState, all_32: u32);
}

/// Turn the FPU on, on this core.
///
/// # Safety
///
/// (SYSREG) `CPACR` must grant access to coprocessors 10 and 11 on this core, which is
/// what `cpu::enable_user_fpu` checks before calling this.
pub(super) unsafe fn fpu_enable() {
    // SAFETY: (SYSREG) the caller guarantees access; setting `EN` changes nothing the
    // soft-float kernel uses.
    unsafe { ferrix_fpu_enable() };
}

/// `MVFR0`, the FPU's feature register.
///
/// # Safety
///
/// (SYSREG) As [`fpu_enable`].
pub(super) unsafe fn fpu_features() -> u32 {
    // SAFETY: (SYSREG) the caller guarantees access; the read has no side effects.
    unsafe { ferrix_fpu_features() }
}

/// `MVFR1`, the FPU's second feature register.
///
/// # Safety
///
/// (SYSREG) As [`fpu_enable`].
pub(super) unsafe fn fpu_features1() -> u32 {
    // SAFETY: (SYSREG) the caller guarantees access; the read has no side effects.
    unsafe { ferrix_fpu_features1() }
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
    use crate::prof::{Ablation, ablate};
    // MEASUREMENT ONLY (os07-prof): the ablations, inside a window only.
    if !ablate(Ablation::SkipTls) && !ablate(Ablation::SkipTlsRead) {
        state.thread_pointer = super::cpu::read_tpidruro();
    }
    crate::prof::stamp(crate::prof::Point::STls);
    // SAFETY: (CONTEXT) `user_sp` and `user_lr` are two adjacent `u32`s in a `repr(C)`
    // structure, which is the two words the assembly writes.
    unsafe { ferrix_user_banked_save(core::ptr::from_mut(&mut state.user_sp)) };
    crate::prof::stamp(crate::prof::Point::SBanked);
    let doubles = super::cpu::user_fpu_doubles();
    if doubles != 0 && !ablate(Ablation::SkipVfp) {
        // SAFETY: (CONTEXT) the FPU exists and is enabled, and `state` is a live,
        // exclusively borrowed `UserState` whose layout is asserted above.
        unsafe { ferrix_user_fpu_save(core::ptr::from_mut(state), u32::from(doubles == 32)) };
    }
    crate::prof::stamp(crate::prof::Point::Saved);
}

/// Load `state` onto this processor for the task about to run.
///
/// `entry_stack` is for x86-64. Here an exception from USR mode lands on the
/// SVC stack, which each task's own stack already is when it returns to USR.
///
/// # Safety
///
/// (CONTEXT) The task `state` belongs to must be the one this processor is switching to.
pub(crate) unsafe fn restore_user_state(state: &UserState, entry_stack: u64) {
    use crate::prof::{Ablation, ablate};
    let _ = entry_stack;
    // MEASUREMENT ONLY (os07-prof): the ablations, inside a window only.
    if !ablate(Ablation::SkipTls) {
        super::cpu::write_tpidruro(state.thread_pointer);
    }
    crate::prof::stamp(crate::prof::Point::RTls);
    // SAFETY: (CONTEXT) as in `save_user_state`, read rather than written.
    unsafe { ferrix_user_banked_restore(core::ptr::from_ref(&state.user_sp)) };
    crate::prof::stamp(crate::prof::Point::RBanked);
    let doubles = super::cpu::user_fpu_doubles();
    if doubles != 0 && !ablate(Ablation::SkipVfp) {
        // SAFETY: (CONTEXT) as above; loading user registers cannot affect the kernel,
        // which uses none of them.
        unsafe { ferrix_user_fpu_restore(core::ptr::from_ref(state), u32::from(doubles == 32)) };
    }
    crate::prof::stamp(crate::prof::Point::Restored);
}

/// Set USR mode's banked stack pointer, as `execve` does for the new program.
pub(crate) fn set_user_stack(stack: u32) {
    let words = [stack, 0];
    // SAFETY: (CONTEXT) two readable words; loading USR's banked registers cannot affect
    // the kernel, which runs in SVC mode.
    unsafe { ferrix_user_banked_restore(words.as_ptr()) };
}

/// Put this processor's user state back to a program's starting state, as
/// `execve` does. The banked stack pointer is `set_user_stack`'s.
///
/// # Safety
///
/// (CONTEXT) Must be called by the user task whose registers these are.
pub(crate) unsafe fn reset_user_state() {
    super::cpu::write_tpidruro(0);
    let doubles = super::cpu::user_fpu_doubles();
    if doubles != 0 {
        let fresh = UserState::new();
        // SAFETY: (CONTEXT) the FPU exists and is enabled; the state is all zeros.
        unsafe { ferrix_user_fpu_restore(core::ptr::from_ref(&fresh), u32::from(doubles == 32)) };
    }
}
