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
use core::sync::atomic::{AtomicBool, Ordering};

pub(super) mod check;

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

/// What a program owns on this processor that no trap saves: its two thread
/// ID registers, and its floating-point registers.
///
/// The kernel is soft-float and never touches them, so a trap from USR mode
/// leaves them as the program had them. Two programs taking turns need them
/// saved and loaded by the scheduler whenever it switches between tasks that
/// run user code.
///
/// # The thread ID registers (`docs/OPAQUE-KERNEL.md` §9.14, 3b and F-66)
///
/// `TPIDRURO`, which USR mode reads and cannot write, is kept here as the
/// truth and never read back at a switch: `set_tls` writes the register and
/// this record together, `execve` zeroes both, and the switch writes it at
/// every switch to a task with user state. `TPIDRURW`, which USR mode writes
/// itself with no call, is read at every switch out and written at every
/// switch in, as Linux's `switch_tls` does. Neither write is ever skipped
/// because it equals what the processor last held: a program changes
/// `TPIDRURW` without the kernel knowing, so a remembered value would name one
/// no longer there (the consultant's condition 8).
///
/// # The vector-state contract (`docs/OPAQUE-KERNEL.md` §9.14, 3a's port)
///
/// Through `channel_write_read`, `object_wait_one` and `port_wait` a program
/// keeps `d8`-`d15` and `FPSCR` of its VFP state, the AAPCS's callee-saved
/// part, and loses the rest. The `svc` entry raises `vectors_dead` for the
/// length of such a call; a switch away from the task while it is blocked in
/// the call stores just those and marks the state `unsaved`; and the switch
/// back loads them and zeroes every other VFP register the core has, whoever
/// switches to it. Only that reset clears `unsaved`.
#[repr(C)]
#[derive(Debug, Clone)]
pub(crate) struct UserState {
    /// `TPIDRURO`, which `set_tls` asks the kernel to write: the record the
    /// switch loads, never reads back.
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
    /// `TPIDRURW`, which USR mode writes itself: read at every switch out.
    user_rw: u32,
    /// Raised by the `svc` entry for the length of a blocking native call,
    /// whose contract lets the caller-saved VFP registers go; lowered by it
    /// alone.
    vectors_dead: bool,
    /// The record holds only `FPSCR` and `d8`-`d15`: the next switch to the
    /// task resets the rest, and every reader sees them zero
    /// ([`UserState::fp`]). Set by the partial save of a task with
    /// [`UserState::vfp`], cleared by the reset and by a writer alone.
    unsaved: bool,
    /// The task has used VFP (`docs/OPAQUE-KERNEL.md` §9.15, D1): set by its
    /// first VFP instruction ([`take_first_use`]), copied by `capture`,
    /// cleared by `execve` alone. Without it the task runs with `FPEXC.EN`
    /// clear, no switch moves a VFP register for it, and this record is its
    /// VFP state.
    vfp: bool,
    /// How many times a switch or a first use moved VFP state or wrote
    /// `FPEXC` for this task: what the lazy check (C1) reads of two programs
    /// that never use VFP. Counted on the VFP paths only.
    vfp_moves: u32,
}

/// The first callee-saved double, `d8`: where the partial save stores and
/// the reset loads the eight the contract keeps.
const KEPT_FIRST: usize = 8;
/// One past the last, `d15`.
const KEPT_END: usize = 16;

/// What the reset loads into every VFP register a blocking native call
/// loses: zeros, in read-only data, so the reset cannot be pointed at a
/// program's memory.
static ZERO_DOUBLES: [u64; 32] = [0; 32];

const _: () = assert!(
    core::mem::offset_of!(UserState, fpscr) == 4,
    "the save sequence stores FPSCR at offset 4"
);
const _: () = assert!(
    core::mem::offset_of!(UserState, doubles) == 8,
    "the save sequence stores d0 at offset 8"
);
const _: () = assert!(
    core::mem::offset_of!(UserState, doubles) + KEPT_FIRST * 8 == 72,
    "the partial save and the reset move d8-d15 at offset 72"
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
            user_rw: 0,
            vectors_dead: false,
            unsaved: false,
            vfp: false,
            vfp_moves: 0,
        }
    }

    /// Whether the task has used VFP (§9.15, D1).
    pub(super) const fn uses_vfp(&self) -> bool {
        self.vfp
    }

    /// How many times VFP state was moved or `FPEXC` written for the task.
    pub(super) const fn vfp_moves(&self) -> u32 {
        self.vfp_moves
    }

    /// The VFP part of a program's starting state, as `execve` leaves it: no
    /// `FPSCR`, every double zero, nothing to reset, and no use of VFP yet
    /// (§9.15, D7 and the consultant's L4).
    const fn clear_vfp(&mut self) {
        self.fpscr = 0;
        self.doubles = [0; 32];
        self.unsaved = false;
        self.vfp = false;
    }

    /// Raise or lower the vector-state contract's mark: raised by the `svc`
    /// entry for a blocking native call, lowered as the call returns. Lowering
    /// it does not touch `unsaved`, which only the reset clears.
    pub(super) const fn set_vectors_dead(&mut self, dead: bool) {
        self.vectors_dead = dead;
    }

    /// Mark the record as holding only `FPSCR` and `d8`-`d15`: what the
    /// partial save does once it has stored them.
    pub(super) const fn mark_unsaved(&mut self) {
        self.unsaved = true;
    }

    /// Whether the record holds only `FPSCR` and `d8`-`d15`, and the next
    /// switch to the task resets the rest.
    pub(super) const fn is_unsaved(&self) -> bool {
        self.unsaved
    }

    /// A copy of the user state this processor holds right now: what a fork
    /// child inherits.
    ///
    /// # Safety
    ///
    /// (CONTEXT) The registers must be the calling task's own.
    ///
    /// `TPIDRURO` is read from the register here, not from the running task's
    /// record: a copy of what the processor holds, outside the switch, which
    /// is the one place that reads it no more (3b). `TPIDRURW` comes with the
    /// copy, so a fork child and a thread inherit it, as Linux's
    /// `copy_thread` gives them.
    ///
    /// The VFP state comes from where it lives (§9.15, D6 and the
    /// consultant's L3): the registers for a task that has used VFP, its
    /// record for one that has not, decided by the running task's own bit and
    /// read in one window with interrupts masked, so that no switch moves it
    /// in between and no VFP instruction runs with `FPEXC.EN` clear. The copy
    /// carries the bit: a fork child or a thread of a task that has used VFP
    /// starts with it; one of a task that has not takes its first use on the
    /// state copied here.
    pub(crate) unsafe fn capture() -> UserState {
        let mut state = UserState::new();
        state.user_rw = super::cpu::read_tpidrurw();
        // SAFETY: (CONTEXT) `user_sp` and `user_lr` are two adjacent `u32`s in
        // a `repr(C)` structure, the two words the assembly writes.
        unsafe { ferrix_user_banked_save(core::ptr::from_mut(&mut state.user_sp)) };
        state.thread_pointer = super::cpu::read_tpidruro();
        let count = super::cpu::user_fpu_doubles();
        if count != 0 {
            masked(|| capture_vfp(&mut state, count));
        }
        state
    }

    /// Give the program `pointer` as its thread pointer, as `CLONE_SETTLS`
    /// and `set_tls` ask.
    pub(crate) const fn set_thread_pointer(&mut self, pointer: u64) {
        self.thread_pointer = pointer as u32;
    }

    /// Zero both thread ID registers' records, as `execve` zeroes the
    /// registers (3b, F-66).
    const fn clear_thread_registers(&mut self) {
        self.thread_pointer = 0;
        self.user_rw = 0;
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

    /// `FPSCR` and `d0` to `d31`, as a signal frame's VFP record carries
    /// them: the one way any reader looks at a saved VFP state.
    ///
    /// A state saved in full answers its own. An `unsaved` one answers its
    /// `FPSCR`, its own `d8`-`d15` and zero in every other register, which is
    /// what the task will hold when it next runs, never the stale doubles the
    /// record still has. Today the signal frame reads a state `capture` took,
    /// which is whole; a frame built from a saved state later, a core dump or
    /// a register report reads it through here (§9.14, R6). The boot's check
    /// (`check::check_unsaved_reads_as_reset`) holds it.
    pub(super) fn fp(&self) -> (u32, [u64; 32]) {
        if !self.unsaved {
            return (self.fpscr, self.doubles);
        }
        let mut doubles = [0; 32];
        if let (Some(kept), Some(own)) = (
            doubles.get_mut(KEPT_FIRST..KEPT_END),
            self.doubles.get(KEPT_FIRST..KEPT_END),
        ) {
            kept.copy_from_slice(own);
        }
        (self.fpscr, doubles)
    }

    /// Replace the floating-point registers, as `sigreturn` does: the state
    /// is whole after, so no reset follows at the next switch in.
    pub(super) const fn set_fp(&mut self, fpscr: u32, doubles: [u64; 32]) {
        self.fpscr = fpscr;
        self.doubles = doubles;
        self.unsaved = false;
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

/// The VFP half of [`UserState::capture`], with interrupts masked: the
/// registers when the running task has the bit (and so `EN`, I1), else its
/// record, read without a VFP instruction.
fn capture_vfp(state: &mut UserState, count: u8) {
    // SAFETY: (CONTEXT) interrupts are masked by the caller, in the running
    // task's own call.
    let own = unsafe { crate::sched::with_own_user_state(|own| (own.vfp, own.fp())) };
    match own {
        Some((true, _)) if vfp_on() => {
            // SAFETY: (CONTEXT) `EN` is set and the registers are the running
            // task's own (I1, I3); `state` is a live local whose layout is
            // asserted above.
            unsafe { ferrix_user_fpu_save(core::ptr::from_mut(state), u32::from(count == 32)) };
            state.vfp = true;
        }
        Some((vfp, (fpscr, doubles))) => {
            state.set_fp(fpscr, doubles);
            state.vfp = vfp;
        }
        None => {}
    }
}

/// Give the running task `fpscr` and `doubles` as its VFP state, if this core
/// has a VFP: what `sigreturn` puts back.
///
/// Where the state lives decides where it goes (§9.15, D6 and the
/// consultant's L3): into the registers for a task that has used VFP, into
/// its record for one that has not, which its first use then loads. The
/// running task's bit is read, and the state written, in one window with
/// interrupts masked.
///
/// # Safety
///
/// (CONTEXT) Must be called by the running user task, inside its own call.
pub(super) unsafe fn load_user_fp(fpscr: u32, doubles: [u64; 32]) {
    let count = super::cpu::user_fpu_doubles();
    if count == 0 {
        return;
    }
    let put = |own: &mut UserState| {
        if own.vfp && vfp_on() {
            let mut loaded = UserState::new();
            loaded.set_fp(fpscr, doubles);
            // SAFETY: (CONTEXT) `EN` is set and the registers are the running
            // task's own (I1); `loaded` is a live local whose layout is
            // asserted above.
            unsafe {
                ferrix_user_fpu_restore(core::ptr::from_ref(&loaded), u32::from(count == 32));
            }
        } else {
            own.set_fp(fpscr, doubles);
        }
    };
    masked(|| {
        // SAFETY: (CONTEXT) interrupts are masked for the window, in the
        // running task's own call.
        let _ = unsafe { crate::sched::with_own_user_state(put) };
    });
}

/// Run `body` with interrupts masked on this processor, as they were after.
fn masked<R>(body: impl FnOnce() -> R) -> R {
    let open = super::interrupts_enabled();
    super::disable_interrupts();
    let result = body();
    if open {
        super::enable_interrupts();
    }
    result
}

/// `FPEXC.EN`.
pub(super) const FPEXC_EN: u32 = 1 << 30;

/// What each processor's `FPEXC.EN` was last written to (§9.15, D2), by
/// logical number. Exact, because only the kernel writes `FPEXC`: no program
/// can name it (S2), unlike `TPIDRURW`, so this is not condition 8's case.
static VFP_ON: [AtomicBool; ferrix_sched::MAX_CPUS] =
    [const { AtomicBool::new(false) }; ferrix_sched::MAX_CPUS];

/// This processor's record, once its per-CPU register is installed.
fn vfp_record() -> Option<&'static AtomicBool> {
    VFP_ON.get(crate::smp::this_cpu()?.logical)
}

/// Whether this processor's `FPEXC.EN` is set: its record, or the register
/// itself before the record can be named.
pub(super) fn vfp_on() -> bool {
    match vfp_record() {
        Some(record) => record.load(Ordering::Relaxed),
        None => fpexc() & FPEXC_EN != 0,
    }
}

/// Write this processor's record.
fn set_vfp_record(on: bool) {
    if let Some(record) = vfp_record() {
        record.store(on, Ordering::Relaxed);
    }
}

/// `FPEXC`, read from the register: for the checks, which compare it with
/// the record, and before the record exists. Zero on a core with no VFP.
pub(super) fn fpexc() -> u32 {
    if super::cpu::user_fpu_doubles() == 0 {
        return 0;
    }
    // SAFETY: (SYSREG) a VFP exists, so `CPACR` granted cp10 and cp11, and a
    // PL1 read of `FPEXC` is permitted whatever `EN` is.
    unsafe { ferrix_fpu_exc() }
}

/// What a scrub loads: no `FPSCR` and every double zero, in read-only data.
static ZERO_STATE: UserState = UserState::new();

/// Turn this processor's VFP on: `FPEXC.EN`, an `isb`, the record.
///
/// # Safety
///
/// (SYSREG) This processor has a VFP and interrupts are masked.
pub(super) unsafe fn turn_on() {
    // SAFETY: (SYSREG) the caller's guarantee.
    unsafe { ferrix_fpu_enable() };
    set_vfp_record(true);
}

/// Zero every VFP register this processor has and `FPSCR`, then clear
/// `FPEXC.EN` and the record (§9.15, I2): what stays behind a clear `EN` is
/// nobody's.
///
/// # Safety
///
/// (SYSREG) This processor has `count` double registers, `EN` is set, and
/// interrupts are masked.
pub(super) unsafe fn scrub_off(count: u8) {
    // SAFETY: (SYSREG) `EN` is set; the source is the read-only zero state.
    unsafe { ferrix_user_fpu_restore(&raw const ZERO_STATE, u32::from(count == 32)) };
    // SAFETY: (SYSREG) the caller's guarantee.
    unsafe { ferrix_fpu_disable() };
    set_vfp_record(false);
}

/// The VFP part of bring-up on this processor (§9.15, D8): `EN` on, the
/// feature registers read, the register file scrubbed of whatever firmware
/// left there, `EN` off. Answers `MVFR0` and `MVFR1`.
///
/// It writes no record: on a secondary processor the per-CPU register that
/// names it is not installed yet. Every record starts off, from its
/// initialiser, which is what this leaves the register as.
///
/// # Safety
///
/// (SYSREG) `CPACR` grants cp10 and cp11 on this core, and interrupts are
/// masked.
pub(super) unsafe fn bring_up_vfp() -> (u32, u32) {
    // SAFETY: (SYSREG) the caller's guarantee.
    unsafe { ferrix_fpu_enable() };
    // SAFETY: (SYSREG) as above; reads of identification registers.
    let features = (unsafe { ferrix_fpu_features() }, {
        // SAFETY: (SYSREG) as above.
        unsafe { ferrix_fpu_features1() }
    });
    if features.0 & 0xF == 2 {
        // SAFETY: (SYSREG) `EN` was just set, and the core has 32 doubles.
        unsafe { ferrix_user_fpu_restore(&raw const ZERO_STATE, 1) };
    } else if features.0 & 0xF == 1 {
        // SAFETY: (SYSREG) as above, 16 doubles.
        unsafe { ferrix_user_fpu_restore(&raw const ZERO_STATE, 0) };
    }
    // SAFETY: (SYSREG) as above.
    unsafe { ferrix_fpu_disable() };
    features
}

/// Load a task's own record into the registers: its reset when it holds
/// only `FPSCR` and `d8`-`d15`, the whole state otherwise. The switch's and
/// the first use's one way to load (the consultant's L2).
///
/// # Safety
///
/// (CONTEXT) `EN` is set, and `state` is the task about to run here.
unsafe fn load_own(state: &mut UserState, count: u8) {
    if state.unsaved {
        // SAFETY: (CONTEXT) the incoming task's registers; its record's
        // `FPSCR` is one the processor held when the task blocked, and the
        // zeros are 256 bytes of read-only data.
        unsafe {
            ferrix_user_fpu_reset(
                core::ptr::from_ref(state),
                u32::from(count == 32),
                ZERO_DOUBLES.as_ptr(),
            );
        }
        state.unsaved = false;
    } else {
        // SAFETY: (CONTEXT) as above; loading user registers cannot affect
        // the kernel, which uses none of them.
        unsafe { ferrix_user_fpu_restore(core::ptr::from_ref(state), u32::from(count == 32)) };
    }
    state.vfp_moves = state.vfp_moves.wrapping_add(1);
}

/// What an undefined instruction from USR mode is, for the trap path
/// (§9.15, D5 and the consultant's L2).
pub(super) enum Undefined {
    /// The running task's first VFP instruction, now taken: re-execute it.
    FirstUse,
    /// An undefined instruction of the program's: `SIGILL`.
    Program,
    /// The processor's record and the task's bit disagree (I1 broken): a
    /// kernel fault, never a first use or a signal.
    Disagree,
}

/// Take an undefined instruction from USR mode as the running task's first
/// use of VFP when it is one: the core has a VFP, the task has user state
/// and its bit is clear. Then, in the masked window the exception entered
/// with, the bit is set, `EN` written with an `isb`, the record set, and the
/// task's own record loaded through the switch's own load. With the bit set
/// and `EN` set, the instruction was not a VFP one, or not one this core has:
/// `SIGILL`. Old seL4's and Linux's way of telling the two apart without
/// decoding the instruction.
///
/// Called from the exception entry, interrupts masked; takes no lock and
/// calls nothing that blocks.
pub(super) fn take_first_use() -> Undefined {
    let count = super::cpu::user_fpu_doubles();
    if count == 0 {
        return Undefined::Program;
    }
    let on = vfp_on();
    let take = |state: &mut UserState| match (state.vfp, on) {
        (false, false) => {
            // SAFETY: (SYSREG) a VFP exists and interrupts are masked.
            unsafe { turn_on() };
            state.vfp = true;
            // SAFETY: (CONTEXT) `EN` was just set, and `state` is the running
            // task's own record.
            unsafe { load_own(state, count) };
            Undefined::FirstUse
        }
        (true, true) => Undefined::Program,
        _ => Undefined::Disagree,
    };
    // SAFETY: (CONTEXT) interrupts are masked from the exception entry, in
    // the running task's own trap.
    let taken = unsafe { crate::sched::with_own_user_state(take) };
    taken.unwrap_or(Undefined::Program)
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

// void ferrix_fpu_enable(void): FPEXC.EN, synchronised for the kernel's own
// VFP instructions after it (9.15)
.globl ferrix_fpu_enable
ferrix_fpu_enable:
    mov    r0, #0x40000000
    vmsr   fpexc, r0
    isb
    bx     lr

// void ferrix_fpu_disable(void): FPEXC.EN clear; the exception return that
// follows synchronises it (9.15)
.globl ferrix_fpu_disable
ferrix_fpu_disable:
    mov    r0, #0
    vmsr   fpexc, r0
    bx     lr

// u32 ferrix_fpu_exc(void): FPEXC, which PL1 reads whatever EN is (9.15)
.globl ferrix_fpu_exc
ferrix_fpu_exc:
    vmrs   r0, fpexc
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

// void ferrix_user_fpu_keep(UserState *state): FPSCR and d8-d15 alone, what
// a blocking native call keeps (3a's port).
.globl ferrix_user_fpu_keep
ferrix_user_fpu_keep:
    vmrs   r2, fpscr
    str    r2, [r0, #4]
    add    r3, r0, #72
    vstmia r3, {{d8-d15}}
    bx     lr

// void ferrix_user_fpu_reset(const UserState *state, u32 all_32, const u64 *zeros):
// the task's own FPSCR and d8-d15, zero in every other register.
.globl ferrix_user_fpu_reset
ferrix_user_fpu_reset:
    ldr    r3, [r0, #4]
    vmsr   fpscr, r3
    vldmia r2, {{d0-d7}}
    add    r3, r0, #72
    vldmia r3, {{d8-d15}}
    cmp    r1, #0
    vldmiane r2, {{d16-d31}}
    bx     lr

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
    /// Set `FPEXC.EN`, with an `isb`.
    fn ferrix_fpu_enable();
    /// Clear `FPEXC.EN`.
    fn ferrix_fpu_disable();
    /// Read `FPEXC`.
    fn ferrix_fpu_exc() -> u32;
    /// Read `MVFR0`.
    fn ferrix_fpu_features() -> u32;
    /// Read `MVFR1`.
    fn ferrix_fpu_features1() -> u32;
    /// Store this processor's floating-point registers into `state`.
    fn ferrix_user_fpu_save(state: *mut UserState, all_32: u32);
    /// Load `state`'s floating-point registers onto this processor.
    fn ferrix_user_fpu_restore(state: *const UserState, all_32: u32);
    /// Store `FPSCR` and `d8`-`d15` alone into `state`.
    fn ferrix_user_fpu_keep(state: *mut UserState);
    /// Load `state`'s `FPSCR` and `d8`-`d15`, and `zeros` into every other
    /// VFP register.
    fn ferrix_user_fpu_reset(state: *const UserState, all_32: u32, zeros: *const u64);
}

/// Store the program state this processor holds into `state`.
///
/// `TPIDRURO` is not read (3b): it is the record's, which `set_tls` and
/// `execve` keep. `TPIDRURW` is, at every switch out (F-66).
///
/// A task that has never used VFP (§9.15) runs with `FPEXC.EN` clear, and no
/// VFP register is saved for it: its record is its state. One that has is
/// saved in full, except when `blocked` in a native call whose contract lets
/// the caller-saved registers go (`vectors_dead`, 3a's port): then only
/// `FPSCR` and `d8`-`d15` are kept, and the state is marked `unsaved` for the
/// switch back to reset. A task switched out runnable -- preempted, even
/// inside one of those calls -- and one blocked in any other call keep
/// everything. Saved at every switch out, so the state never stays in a
/// processor another one would have to fetch it from (D9).
///
/// # Safety
///
/// (CONTEXT) The registers must belong to the task `state` is for: it was the last task
/// with user state to run on this processor. `blocked` must say whether that
/// task is switched out blocked rather than runnable.
pub(crate) unsafe fn save_user_state(state: &mut UserState, blocked: bool) {
    state.user_rw = super::cpu::read_tpidrurw();
    // SAFETY: (CONTEXT) `user_sp` and `user_lr` are two adjacent `u32`s in a `repr(C)`
    // structure, which is the two words the assembly writes.
    unsafe { ferrix_user_banked_save(core::ptr::from_mut(&mut state.user_sp)) };
    let doubles = super::cpu::user_fpu_doubles();
    if doubles == 0 || !state.vfp {
        return;
    }
    if blocked && state.vectors_dead {
        // SAFETY: (CONTEXT) the FPU exists and `EN` is set for a task with the
        // bit (I1), and `state` is a live, exclusively borrowed `UserState`
        // whose `d8` is at the offset asserted above; these are the outgoing
        // task's registers.
        unsafe { ferrix_user_fpu_keep(core::ptr::from_mut(state)) };
        state.mark_unsaved();
    } else {
        // SAFETY: (CONTEXT) as above, with the layout asserted above.
        unsafe { ferrix_user_fpu_save(core::ptr::from_mut(state), u32::from(doubles == 32)) };
    }
    state.vfp_moves = state.vfp_moves.wrapping_add(1);
}

/// Load `state` onto this processor for the task about to run.
///
/// `entry_stack` is for x86-64. Here an exception from USR mode lands on the
/// SVC stack, which each task's own stack already is when it returns to USR.
///
/// A task that has used VFP gets `FPEXC.EN` set, if this core's is clear,
/// and its state: a state marked `unsaved` is reset rather than restored --
/// its own `FPSCR` and `d8`-`d15`, zero in every other VFP register the core
/// has -- and the reset is the only thing that clears the mark (3a's
/// condition 7): every way a task is resumed -- a message, a close, a kill, a
/// signal -- switches to it through here. A task that has not gets `EN`
/// clear: if it was set, every VFP register and `FPSCR` are zeroed first
/// (§9.15, I2), whoever ran before, a dead task included; if it was clear,
/// nothing happens at all, so two such tasks trade a core with no VFP
/// instruction and no `FPEXC` access.
///
/// # Safety
///
/// (CONTEXT) The task `state` belongs to must be the one this processor is switching to.
pub(crate) unsafe fn restore_user_state(state: &mut UserState, entry_stack: u64) {
    let _ = entry_stack;
    // Both thread ID registers at every switch, never skipped (3b, F-66).
    super::cpu::write_tpidruro(state.thread_pointer);
    super::cpu::write_tpidrurw(state.user_rw);
    // SAFETY: (CONTEXT) as in `save_user_state`, read rather than written.
    unsafe { ferrix_user_banked_restore(core::ptr::from_ref(&state.user_sp)) };
    let doubles = super::cpu::user_fpu_doubles();
    if doubles == 0 {
        return;
    }
    let on = vfp_on();
    if state.vfp {
        if !on {
            // SAFETY: (SYSREG) a VFP exists; the switch runs with interrupts
            // masked under the run queue lock.
            unsafe { turn_on() };
        }
        // SAFETY: (CONTEXT) `EN` is set, and `state` is the incoming task's.
        unsafe { load_own(state, doubles) };
    } else if on {
        // SAFETY: (SYSREG) a VFP exists, `EN` is set, interrupts are masked.
        unsafe { scrub_off(doubles) };
        state.vfp_moves = state.vfp_moves.wrapping_add(1);
    }
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
    // Both thread ID registers and the record together, in one masked
    // window (the consultant's R3): `execve` runs with interrupts open, and a
    // switch between the register and the record would give the new image the
    // old thread pointer back at its next switch in. `TPIDRURW` is zeroed as
    // Linux's `flush_tls` zeroes it, so nothing of the old image reaches the
    // new one through it (F-66).
    //
    // The VFP state goes back to a program's start in the same window
    // (§9.15, D7 and the consultant's L4): the record cleared, because the new
    // image's first use loads it, and the registers scrubbed with `EN`
    // cleared if this core had it set, so the image starts as a task that has
    // never used VFP.
    let count = super::cpu::user_fpu_doubles();
    masked(|| {
        super::cpu::write_tpidruro(0);
        super::cpu::write_tpidrurw(0);
        // SAFETY: (CONTEXT) interrupts masked, inside the running task's own call.
        let _ = unsafe {
            crate::sched::with_own_user_state(|state| {
                state.clear_thread_registers();
                state.clear_vfp();
            })
        };
        if count != 0 && vfp_on() {
            // SAFETY: (SYSREG) a VFP exists, `EN` is set, interrupts masked.
            unsafe { scrub_off(count) };
        }
    });
}
