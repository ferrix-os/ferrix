//! The context switch.
//!
//! # Why there is assembly here
//!
//! A context switch is a function that returns onto a *different stack* from
//! the one it was called on. Rust has no way to say that: the callee-saved
//! registers it would restore on the way out belong to the caller it is about
//! to stop being, and the return address it would use lives on a stack that
//! is no longer current. So the whole of it is six pushes, one store, one
//! load, six pops and a return — and what makes it a switch rather than a
//! no-op is the two instructions in the middle.
//!
//! The System V ABI's callee-saved set is `rbx`, `rbp` and `r12` through
//! `r15`. Everything else is the caller's problem, and the caller here is
//! Rust, which has already spilled whatever it cared about. There is no
//! floating-point state to save: the kernel is built for a target with no
//! SSE, so it uses none. A program's is saved beside its other user state
//! ([`UserState`]), with `XSAVE` where the processor has it.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_sched::MAX_CPUS;

use super::cpu;
use super::gdt;

pub(super) mod check;

/// Bytes the switch pushes: six registers and the return address.
const FRAME_BYTES: u64 = 7 * 8;

/// Save this context and resume another: six pushes, one store, one load,
/// six pops and a return.
///
/// A naked function rather than a `global_asm!` symbol declared `extern "C"`
/// (Q8, `docs/OPAQUE-KERNEL.md` §9.11): under the position-independent model
/// the kernel is built with for KASLR, a call to an external symbol goes
/// through the GOT (`call *slot(%rip)`, `R_X86_64_GOTPCREL`, which the linker
/// may not relax), while a function of this crate is called directly, PC
/// relative, with no relocation for KASLR to apply. The instructions are the
/// same.
///
/// `rdi` is where to write this context's stack pointer, `rsi` the stack
/// pointer to resume.
///
/// # Safety
///
/// (CONTEXT) As [`switch_to`], whose contract this is.
#[unsafe(naked)]
unsafe extern "C" fn ferrix_switch(save: *mut u64, next: u64) {
    core::arch::naked_asm!(
        "pushq %rbp",
        "pushq %rbx",
        "pushq %r12",
        "pushq %r13",
        "pushq %r14",
        "pushq %r15",
        "movq  %rsp, (%rdi)",
        "movq  %rsi, %rsp",
        "popq  %r15",
        "popq  %r14",
        "popq  %r13",
        "popq  %r12",
        "popq  %rbx",
        "popq  %rbp",
        "retq",
        options(att_syntax)
    );
}

global_asm!(
    r#"
.section .text

// Where a task starts the first time it is switched to: `prepare_stack` left
// its entry point in r12 and its argument in r13, and the stack is aligned as
// the ABI wants it at a call.
.globl ferrix_task_entry
ferrix_task_entry:
    movq %r13, %rdi
    callq *%r12
    ud2
"#,
    options(att_syntax)
);

unsafe extern "C" {
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
/// The frame is what [`switch_to`] pops: six callee-saved registers and a
/// return address. Two of the registers carry the entry point and its
/// argument, because the trampoline the return address names is the only code
/// that runs before Rust does and it has nowhere else to read them from.
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
    let frame: [u64; 7] = [
        0,                     // r15
        0,                     // r14
        argument as u64,       // r13
        entry as usize as u64, // r12
        0,                     // rbx
        0,                     // rbp
        ferrix_task_entry as *const () as usize as u64,
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
/// pointer and `GS` base, its x87, SSE and AVX state, and -- for a 32-bit program,
/// which addresses through segments -- its data segment selectors and the
/// three thread-local descriptors they may name (`docs/I386.md` §3.5).
///
/// The kernel is built for a target with no SSE and never touches any of
/// these, so a trap from ring 3 leaves them as the program had them. Two
/// programs taking turns need them saved and loaded by the scheduler whenever
/// it switches between tasks that run user code.
///
/// # The FS and GS bases are the record's (`docs/OPAQUE-KERNEL.md` §9.8, 3b)
///
/// The two bases are kept here as the truth and never read back from the
/// MSRs at a switch: `arch_prctl` writes the register and this record
/// together, `execve` zeroes both, and the switch loads them at every switch
/// to a task with user state. A program cannot set either base without a
/// call (`FSGSBASE` stays off), except by loading a selector: a non-null one
/// takes the descriptor's base, which the restore reloads with the selector;
/// a null one leaves the record's, as Linux's `save_base_legacy` does. A
/// program that clears its own base with a null selector on a processor
/// that clears it gets its recorded base back at its next switch-in, its own
/// value. The write at every switch is not skipped when it equals what the
/// processor last held: the consultant's condition 8 found that a skip leaks
/// across programs on both vendors.
///
/// # The vector-state contract (`docs/OPAQUE-KERNEL.md` §9.8, 3a)
///
/// Through `channel_write_read`, `object_wait_one` and `port_wait`, the
/// native calls that block, a program keeps only `MXCSR` and the x87 control
/// word of its vector state. The entry raises `vectors_dead` for the length
/// of such a call; a switch away from the task while it is blocked in the
/// call saves just those two and marks the state `unsaved`; and the switch
/// back resets every vector register to its initial state with the task's own
/// two words, whoever switches to it. Only the reset clears the mark.
#[repr(C, align(64))]
#[derive(Debug, Clone)]
pub(crate) struct UserState {
    /// `FS_BASE`, which `arch_prctl(ARCH_SET_FS)` writes: the record the
    /// switch loads, never reads back.
    thread_pointer: u64,
    /// The program's `GS_BASE`, which sits in `KERNEL_GS_BASE` while the
    /// kernel runs: likewise the record's.
    gs_base: u64,
    /// GDT slots 12 to 14: the thread-local descriptors `set_thread_area`
    /// installed, zero for none.
    tls: [u64; gdt::TLS_SLOTS],
    /// `DS`, `ES`, `FS` and `GS`, as the program left them.
    selectors: [u16; 4],
    /// Raised by the `SYSCALL` entry for the length of a blocking native
    /// call, whose contract lets the caller's vector registers go.
    vectors_dead: bool,
    /// The area holds only `MXCSR` and the x87 control word, with an empty
    /// `XSTATE_BV`: the next switch to the task resets the registers, and
    /// every reader sees the initial state ([`UserState::vectors`]).
    unsaved: bool,
    /// The x87, SSE and AVX registers, as `XSAVE` or `FXSAVE` wrote them.
    fpu: FpuArea,
}

/// An `XSAVE` area in the standard form, for the components
/// `cpu::enable_extended_state` enables: the 512-byte legacy area `FXSAVE`
/// also writes, the header, and the upper halves of the sixteen `YMM`
/// registers. `FXSAVE` uses the first part alone.
#[repr(C, align(64))]
#[derive(Debug, Clone)]
struct FpuArea {
    /// x87 and SSE, as `FXSAVE` lays them out.
    legacy: [u8; 512],
    /// `XSTATE_BV`, which components the area holds rather than leaves in
    /// their initial state, then `XCOMP_BV` and reserved bytes, all zero.
    header: [u8; 64],
    /// `YMM_Hi128`: bits 128 to 255 of `YMM0` to `YMM15`.
    avx: [u8; 256],
}

const _: () = assert!(
    core::mem::offset_of!(UserState, fpu).is_multiple_of(64),
    "XSAVE64 wants its area 64-byte aligned"
);
const _: () = assert!(
    size_of::<FpuArea>() as u64 == cpu::XSAVE_AREA_BYTES,
    "the area is the standard form's size for x87, SSE and AVX"
);

impl UserState {
    /// A copy of the user state this processor holds right now: what a fork
    /// child inherits.
    ///
    /// # Safety
    ///
    /// (CONTEXT) The registers must be the calling task's own, which they are inside its
    /// own system call.
    ///
    /// The bases are read from the MSRs here, not from the running task's
    /// record: a copy of what the processor holds, outside the switch, which
    /// is the one place that reads no base (3b).
    pub(crate) unsafe fn capture() -> UserState {
        let mut state = UserState::new();
        // SAFETY: (CONTEXT) the caller's guarantee; the registers are the
        // running task's and it is not blocked.
        unsafe { save_user_state(&mut state, false) };
        // SAFETY: (CONTEXT) reading `FS_BASE` has no side effects.
        state.thread_pointer = unsafe { super::syscall::thread_pointer() };
        // SAFETY: (CONTEXT) the kernel side of `swapgs`, where the shadow is the
        // program's.
        state.gs_base = unsafe { super::syscall::program_gs_base() };
        state
    }

    /// Raise or lower the vector-state contract's mark: raised by the
    /// `SYSCALL` entry for a blocking native call, lowered as the call
    /// returns (3a). Lowering it does not touch `unsaved`, which only the
    /// reset clears.
    pub(super) const fn set_vectors_dead(&mut self, dead: bool) {
        self.vectors_dead = dead;
    }

    /// Give the program `fs_base` and `gs_base` in this record, beside the
    /// MSRs the caller writes: what `execve` does (3b).
    pub(super) const fn set_bases(&mut self, fs_base: u64, gs_base: u64) {
        self.thread_pointer = fs_base;
        self.gs_base = gs_base;
    }

    /// Keep only `mxcsr` and `control` of the vector state, and mark it
    /// `unsaved`: what the switch does to a task blocked in a call whose
    /// contract lets its vector registers go (3a). The area becomes the
    /// standard form's reset image: `XSTATE_BV` empty, `XCOMP_BV` zero (as
    /// it always is), and the task's own `MXCSR` and control word in the
    /// legacy region, which `XRSTOR64` loads `MXCSR` from whatever
    /// `XSTATE_BV` says (Intel SDM Vol. 1 §13.8.1). The legacy region's other
    /// bytes are this task's own, from its last full save; no reader sees
    /// them ([`UserState::vectors`]).
    pub(super) fn keep_vector_controls(&mut self, mxcsr: u32, control: u16) {
        let [c0, c1] = control.to_le_bytes();
        let [m0, m1, m2, m3] = mxcsr.to_le_bytes();
        let legacy = &mut self.fpu.legacy;
        legacy[FCW_AT] = c0;
        legacy[FCW_AT + 1] = c1;
        legacy[MXCSR_AT] = m0;
        legacy[MXCSR_AT + 1] = m1;
        legacy[MXCSR_AT + 2] = m2;
        legacy[MXCSR_AT + 3] = m3;
        if let Some(word) = self.fpu.header.first_chunk_mut::<8>() {
            *word = [0; 8];
        }
        self.unsaved = true;
    }

    /// Whether the area holds only `MXCSR` and the control word, and the
    /// next switch to the task resets its vector registers.
    pub(super) const fn is_unsaved(&self) -> bool {
        self.unsaved
    }

    /// The task's vector state: the one way any reader looks at a saved area.
    ///
    /// A state saved in full answers its own area. An `unsaved` one answers
    /// the initial state with the task's own `MXCSR` and x87 control word,
    /// which is what the task will hold when it next runs, never the stale
    /// bytes the area still has. Today the switch is the only reader of a
    /// task's saved area, and a signal frame is built from the live registers
    /// after the reset (`UserState::capture`); a signal frame built from a
    /// saved area later, a core dump, or a register report that reads another
    /// task's area reads it through here (the consultant's condition 7). The
    /// boot's check (`check::check_unsaved_reads_as_initial`) holds it.
    fn vectors(&self) -> alloc::borrow::Cow<'_, FpuArea> {
        if !self.unsaved {
            return alloc::borrow::Cow::Borrowed(&self.fpu);
        }
        let legacy = &self.fpu.legacy;
        let control = u16::from_le_bytes([legacy[FCW_AT], legacy[FCW_AT + 1]]);
        let mxcsr = u32::from_le_bytes([
            legacy[MXCSR_AT],
            legacy[MXCSR_AT + 1],
            legacy[MXCSR_AT + 2],
            legacy[MXCSR_AT + 3],
        ]);
        alloc::borrow::Cow::Owned(FpuArea::initial(mxcsr, control))
    }

    /// Give the program `pointer` as its thread pointer, as `CLONE_SETTLS`
    /// asks.
    pub(crate) const fn set_thread_pointer(&mut self, pointer: u64) {
        self.thread_pointer = pointer;
    }

    /// Put `descriptor` in thread-local slot `index` of this saved state: a
    /// 32-bit program's `CLONE_SETTLS`, which names a `user_desc` for the
    /// child rather than a base (`docs/I386.md` §3.5). The child's `%gs`
    /// keeps its parent's selector and so reads through the new descriptor.
    /// False for an `index` past the three.
    pub(crate) fn set_thread_area(&mut self, index: usize, descriptor: u64) -> bool {
        self.tls
            .get_mut(index)
            .map(|slot| *slot = descriptor)
            .is_some()
    }

    /// A program's state before it has run: no thread pointer, and the x87 and
    /// SSE control words a processor has at reset ([`FpuArea::initial`]).
    pub(crate) const fn new() -> UserState {
        UserState {
            thread_pointer: 0,
            gs_base: 0,
            tls: [0; gdt::TLS_SLOTS],
            selectors: [0; 4],
            vectors_dead: false,
            unsaved: false,
            fpu: FpuArea::initial(INITIAL_MXCSR, INITIAL_X87_CONTROL),
        }
    }

    /// The 512-byte `FXSAVE` area: what a signal frame carries as `fpstate`,
    /// through [`UserState::vectors`].
    pub(super) fn fxsave(&self) -> [u8; 512] {
        self.vectors().legacy
    }

    /// The same area, for `rt_sigreturn` to fill from the frame.
    pub(super) fn fxsave_mut(&mut self) -> &mut [u8; 512] {
        self.materialise();
        &mut self.fpu.legacy
    }

    /// Which components the area holds, `XSTATE_BV`: a component left out
    /// is in its initial state, all zero for AVX.
    pub(super) fn xstate_bv(&self) -> u64 {
        self.vectors()
            .header
            .first_chunk::<8>()
            .map_or(0, |word| u64::from_le_bytes(*word))
    }

    /// Set `XSTATE_BV`. `XRSTOR` refuses with `#GP` a component `XCR0` does
    /// not enable, so only those are kept.
    pub(super) fn set_xstate_bv(&mut self, components: u64) {
        let kept = components & cpu::extended_state_components();
        self.materialise();
        if let Some(word) = self.fpu.header.first_chunk_mut::<8>() {
            *word = kept.to_le_bytes();
        }
    }

    /// The upper halves of the sixteen `YMM` registers, through
    /// [`UserState::vectors`].
    pub(super) fn avx(&self) -> [u8; 256] {
        self.vectors().avx
    }

    /// The same, for `rt_sigreturn` to fill from the frame.
    pub(super) fn avx_mut(&mut self) -> &mut [u8; 256] {
        self.materialise();
        &mut self.fpu.avx
    }

    /// Before a writer changes part of the area: an `unsaved` state becomes
    /// the initial image it reads as, saved in full, so that what is written
    /// joins what every reader already saw rather than the stale bytes.
    fn materialise(&mut self) {
        let initial = match self.vectors() {
            alloc::borrow::Cow::Owned(initial) => Some(initial),
            alloc::borrow::Cow::Borrowed(_) => None,
        };
        if let Some(initial) = initial {
            self.fpu = initial;
            self.unsaved = false;
        }
    }
}

/// Where the x87 control word sits in the legacy region.
const FCW_AT: usize = 0;
/// Where `MXCSR` sits in the legacy region.
const MXCSR_AT: usize = 24;
/// The x87 control word a processor has at reset, and `XRSTOR`'s initial
/// state: every x87 exception masked.
pub(super) const INITIAL_X87_CONTROL: u16 = 0x037F;
/// The `MXCSR` a processor has at reset: every SSE exception masked.
pub(super) const INITIAL_MXCSR: u32 = 0x1F80;

impl FpuArea {
    /// A program's vector state before it has run, with `mxcsr` and
    /// `control` in place of the reset ones: what [`UserState::new`] starts
    /// with, and what an `unsaved` state reads as.
    ///
    /// Not all zeros, and the difference is a crash: an all-zero `MXCSR`
    /// unmasks every SSE exception, so a program's first inexact division
    /// would take `#XM` instead of rounding. `0x1F80` masks them all, and
    /// `0x037F` does the same for the x87. `XSTATE_BV` names x87 and SSE, so
    /// `XRSTOR` loads the control words rather than its own initial ones, and
    /// leaves AVX out, so it starts zero.
    const fn initial(mxcsr: u32, control: u16) -> FpuArea {
        let mut legacy = [0_u8; 512];
        let control = control.to_le_bytes();
        legacy[FCW_AT] = control[0];
        legacy[FCW_AT + 1] = control[1];
        let mxcsr = mxcsr.to_le_bytes();
        legacy[MXCSR_AT] = mxcsr[0];
        legacy[MXCSR_AT + 1] = mxcsr[1];
        legacy[MXCSR_AT + 2] = mxcsr[2];
        legacy[MXCSR_AT + 3] = mxcsr[3];
        let mut header = [0_u8; 64];
        header[0] = cpu::XSTATE_X87_SSE as u8;
        FpuArea {
            legacy,
            header,
            avx: [0; 256],
        }
    }
}

/// Load `state`'s x87, SSE and AVX registers and nothing else: not the
/// thread pointer, and not the entry stack. What `rt_sigreturn` puts back.
///
/// # Safety
///
/// (CONTEXT) The registers must be the calling task's own, and `state`'s `MXCSR` must
/// have no reserved bit set, which `FXRSTOR64` and `XRSTOR64` answer with
/// `#GP` in ring 0; its header is kept valid by [`UserState::set_xstate_bv`].
pub(super) unsafe fn load_fpu(state: &UserState) {
    // SAFETY: (CONTEXT) an area inside a 64-byte-aligned structure, whose `MXCSR`
    // the caller has masked and whose header only names enabled components.
    unsafe { ferrix_fpu_restore(&raw const state.fpu, cpu::extended_state_components()) };
}

/// `XSAVE64` of `components` into `area`, or `FXSAVE64` for none: the
/// processor's own instructions, which `core::arch` spells, rather than
/// assembly (`docs/ASSEMBLY.md`).
///
/// # Safety
///
/// (CONTEXT) `area` must be a whole [`FpuArea`], and `components` zero or a subset
/// of `XCR0`, which `cpu::extended_state_components` is.
unsafe fn ferrix_fpu_save(area: *mut FpuArea, components: u64) {
    let at = area.cast::<u8>();
    if components == 0 {
        // SAFETY: (CONTEXT) 512 bytes, sixteen-byte aligned, inside the area.
        unsafe { core::arch::x86_64::_fxsave64(at) };
    } else {
        // SAFETY: (CONTEXT) the standard form's size for these components,
        // 64-byte aligned; `OSXSAVE` is set whenever they are not zero.
        unsafe { core::arch::x86_64::_xsave64(at, components) };
    }
}

/// `XRSTOR64` of `components` from `area`, or `FXRSTOR64` for none.
///
/// # Safety
///
/// (CONTEXT) As [`ferrix_fpu_save`], and the area's `MXCSR` and header must be ones
/// the processor accepts: written by it, built by [`UserState::new`], or
/// masked by `rt_sigreturn`.
unsafe fn ferrix_fpu_restore(area: *const FpuArea, components: u64) {
    let at = area.cast::<u8>();
    if components == 0 {
        // SAFETY: (CONTEXT) the caller's guarantee.
        unsafe { core::arch::x86_64::_fxrstor64(at) };
    } else {
        // SAFETY: (CONTEXT) the caller's guarantee.
        unsafe { core::arch::x86_64::_xrstor64(at, components) };
    }
}

/// Store the program state this processor holds into `state`.
///
/// No base MSR is read (3b): the `FS` and `GS` bases are the record's, which
/// `arch_prctl` and `execve` keep, and a non-null selector, saved here, brings
/// its descriptor's base back when the restore reloads it.
///
/// The vector registers are saved in full, except for a task `blocked` in a
/// native call whose contract lets them go (`vectors_dead`, 3a): then only
/// `MXCSR` and the x87 control word are kept, and the state is marked
/// `unsaved` for the switch back to reset. A task switched out runnable --
/// preempted, even inside one of those calls -- and one blocked in any other
/// call keep everything. On a processor without `XSAVE` everything is saved
/// too, since the reset is an `XRSTOR` of an empty header.
///
/// # Safety
///
/// (CONTEXT) The registers must belong to the task `state` is for: it was the last task
/// with user state to run on this processor. `blocked` must say whether that
/// task is switched out blocked rather than runnable.
pub(crate) unsafe fn save_user_state(state: &mut UserState, blocked: bool) {
    state.selectors = cpu::read_data_selectors();
    // SAFETY: (CONTEXT) the caller switches tasks with interrupts masked, so these are
    // this processor's slots and the outgoing thread's.
    state.tls = unsafe { gdt::read_tls() };
    let components = cpu::extended_state_components();
    if blocked && state.vectors_dead && components != 0 {
        // SAFETY: (SYSREG) the processor runs programs with SSE, and these
        // are the outgoing task's registers.
        let (mxcsr, control) = unsafe { cpu::read_vector_controls() };
        state.keep_vector_controls(mxcsr, control);
        return;
    }
    // SAFETY: (CONTEXT) an area of the standard form's size for the enabled
    // components, 64-byte aligned, which is what `XSAVE64` and `FXSAVE64` write.
    unsafe { ferrix_fpu_save(&raw mut state.fpu, components) };
}

/// Load `state` onto this processor for the task about to run, and point the
/// ways in from ring 3 at `entry_stack`.
///
/// The bases are written at every switch from the record, never skipped
/// when they equal what this processor last held (3b, the consultant's
/// condition 8). A state marked `unsaved` is reset rather than restored, and
/// the reset is the only thing that clears the mark (3a, condition 7):
/// every way a task is resumed -- a message, a close, a kill, a signal, a
/// direct switch or any `choose_next` -- switches to it through here.
///
/// # Safety
///
/// (CONTEXT) The task `state` belongs to must be the one this processor is switching to,
/// and `entry_stack` the top of its kernel stack.
pub(crate) unsafe fn restore_user_state(state: &mut UserState, entry_stack: u64) {
    // SAFETY: (CONTEXT) the caller switches with interrupts masked; the descriptors are
    // ones `set_thread_area` built, or zero.
    unsafe { gdt::write_tls(&state.tls) };
    // SAFETY: (CONTEXT) each selector checked loadable against the slots just written.
    unsafe {
        load_selectors(
            state.selectors,
            &state.tls,
            state.thread_pointer,
            state.gs_base,
        );
    }
    if state.unsaved {
        // SAFETY: (CONTEXT) the incoming task's registers, its area the reset
        // image `keep_vector_controls` left.
        unsafe { reset_vectors(state) };
    } else {
        // SAFETY: (CONTEXT) an area this module initialised or `XSAVE64` wrote, so every
        // reserved bit `XRSTOR64` checks is clear.
        unsafe { ferrix_fpu_restore(&raw const state.fpu, cpu::extended_state_components()) };
    }
    // SAFETY: (ENTRY) the caller guarantees the stack.
    unsafe { super::syscall::set_entry_stack(entry_stack) };
}

/// Fast resets that reset the x87 because `XINUSE` said, or could not say,
/// it was in use; and those that left it, already initial: per processor,
/// for the check, which must see each branch taken on its own processor.
/// Each processor writes only its own slots, by a load and a store with
/// interrupts masked, as 2f's words are (`docs/OPAQUE-KERNEL.md` §9.8): no
/// global and no locked write in the switch.
static X87_RESETS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// See [`X87_RESETS`].
static X87_LEFT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Loads of `DS` and `ES` that left one or both unloaded, 0 over 0 (the
/// `DS`/`ES` skip): per processor, kept as [`X87_RESETS`] is, for the check,
/// which must see the skip taken.
static SELECTOR_SKIPS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Loads of `FS` that left it unloaded, 0 over 0 (the `FS`/`GS` skip, §9.8
/// 3c): per processor, kept as [`X87_RESETS`] is, apart from `GS`'s so that
/// the check sees each register's skip taken on its own.
static FS_SKIPS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Loads of `GS` that left it unloaded, 0 over 0: as [`FS_SKIPS`].
static GS_SKIPS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// This processor's logical number, zero before it has a record.
fn this_logical() -> usize {
    crate::smp::this_cpu().map_or(0, |cpu| cpu.logical)
}

/// Add one to this processor's slot of `counters`.
fn count(counters: &[AtomicU64; MAX_CPUS]) {
    if let Some(counter) = counters.get(this_logical()) {
        counter.store(
            counter.load(Ordering::Relaxed).wrapping_add(1),
            Ordering::Relaxed,
        );
    }
}

/// This processor's fast resets so far that reset the x87, and that left it
/// alone.
pub(crate) fn x87_resets() -> (u64, u64) {
    let cpu = this_logical();
    let read = |counters: &[AtomicU64; MAX_CPUS]| {
        counters
            .get(cpu)
            .map_or(0, |counter| counter.load(Ordering::Relaxed))
    };
    (read(&X87_RESETS), read(&X87_LEFT))
}

/// This processor's switches so far that left `DS` or `ES` unloaded, 0 over
/// 0.
pub(crate) fn selector_skips() -> u64 {
    SELECTOR_SKIPS
        .get(this_logical())
        .map_or(0, |counter| counter.load(Ordering::Relaxed))
}

/// This processor's switches so far that left `FS` unloaded, and those that
/// left `GS` unloaded, each 0 over 0.
pub(crate) fn fs_gs_skips() -> (u64, u64) {
    let cpu = this_logical();
    let read = |counters: &[AtomicU64; MAX_CPUS]| {
        counters
            .get(cpu)
            .map_or(0, |counter| counter.load(Ordering::Relaxed))
    };
    (read(&FS_SKIPS), read(&GS_SKIPS))
}

/// Every processor's switches so far that left `DS` or `ES` unloaded, that
/// left `FS` unloaded, and that left `GS` unloaded, in that order: for the
/// fast path's counts, which show its direct switches taking the skips
/// through the same `restore_user_state` (the consultant's E3 (iv) and K5).
pub(crate) fn selector_skips_total() -> Option<[u64; 3]> {
    let total = |counters: &[AtomicU64; MAX_CPUS]| {
        counters
            .iter()
            .map(|counter| counter.load(Ordering::Relaxed))
            .fold(0, u64::wrapping_add)
    };
    Some([total(&SELECTOR_SKIPS), total(&FS_SKIPS), total(&GS_SKIPS)])
}

/// Reset the vector registers of a task whose state is `unsaved`: every
/// enabled component to its initial state, with the task's own `MXCSR` and
/// x87 control word, and clear the mark (3a).
///
/// `XRSTOR64` of the area, whose header's `XSTATE_BV` is empty and whose
/// `XCOMP_BV` is zero (the standard form), with `XCR0`'s enabled set less
/// `PKRU` requested: each requested component whose `XSTATE_BV` bit is clear
/// is initialised, so no register keeps what the program that ran before
/// left in it, and `MXCSR` is loaded from the legacy region because SSE or
/// AVX is requested (Intel SDM Vol. 1 §13.8.1). The x87's initial control
/// word is `0x037F`, so the task's own is loaded after when it differs.
///
/// Where `XCR0` less `PKRU` is x87, SSE and AVX exactly -- the reference
/// configuration -- the same state is reached by cheaper means (step 5,
/// `docs/OPAQUE-KERNEL.md` §9.10: an `XRSTOR` costs about 70 ns here
/// whatever it restores): `VZEROALL` zeroes `YMM0` to `YMM15` whole, `LDMXCSR`
/// gives the task its own `MXCSR`, and the x87 is reset by an `XRSTOR` of its
/// component alone unless `XINUSE` (`XGETBV` 1, where `CPUID` offers it) says
/// it is already in its initial state, whose control word is `0x037F`. Any
/// other `XCR0` takes the `XRSTOR` of every component.
///
/// # Safety
///
/// (CONTEXT) The registers must be the incoming task's, and `state` marked
/// `unsaved` by [`save_user_state`], whose image `XRSTOR64` accepts.
unsafe fn reset_vectors(state: &mut UserState) {
    let components = cpu::reset_components();
    if components == cpu::XSTATE_X87_SSE | cpu::XSTATE_AVX {
        // Step 5 (`docs/OPAQUE-KERNEL.md` §9.10): the same initial state by
        // cheaper means where `XCR0` is x87, SSE and AVX exactly. `VZEROALL`
        // zeroes `YMM0` to `YMM15` whole, which is SSE's and AVX's initial
        // state; `MXCSR`, the one other part of either, is the task's own,
        // loaded from the image. The x87 is reset by an `XRSTOR` of its
        // component alone from the image's empty header, unless `XINUSE`
        // says it is in its initial state already.
        let mxcsr = u32::from_le_bytes([
            state.fpu.legacy[MXCSR_AT],
            state.fpu.legacy[MXCSR_AT + 1],
            state.fpu.legacy[MXCSR_AT + 2],
            state.fpu.legacy[MXCSR_AT + 3],
        ]);
        // SAFETY: (CONTEXT) the incoming task's registers; AVX is in `XCR0`,
        // and `MXCSR` is one the processor held when the task blocked.
        unsafe { cpu::zero_vectors(mxcsr) };
        if cpu::x87_in_use() {
            // SAFETY: (CONTEXT) the caller's guarantee: the image's header
            // names no component, so the x87 is initialised.
            unsafe { ferrix_fpu_restore(&raw const state.fpu, cpu::XSTATE_X87) };
            count(&X87_RESETS);
        } else {
            count(&X87_LEFT);
        }
    } else {
        // SAFETY: (CONTEXT) the caller's guarantee: the image's `MXCSR` is one the
        // processor held, and its header names no component.
        unsafe { ferrix_fpu_restore(&raw const state.fpu, components) };
    }
    let control = u16::from_le_bytes([state.fpu.legacy[FCW_AT], state.fpu.legacy[FCW_AT + 1]]);
    if control != INITIAL_X87_CONTROL {
        // SAFETY: (SYSREG) the incoming task's own control word.
        unsafe { cpu::load_x87_control(control) };
    }
    state.unsaved = false;
}

/// Put this processor's user state back to a program's starting state: no
/// thread pointer, reset floating-point control. What `execve` does to the
/// registers the old program left.
///
/// # Safety
///
/// (CONTEXT) Must be called by the user task whose registers these are, from inside its
/// own system call.
pub(crate) unsafe fn reset_user_state() {
    let fresh = UserState::new();
    // `execve` empties the thread-local slots and every selector, as Linux's
    // `flush_thread` and `start_thread` do: the new image starts with none of
    // the old one's segments. A 32-bit one is given user data in `DS` and `ES`
    // as it is entered (`enter_compat_segments`).
    with_interrupts_masked(|| {
        // SAFETY: (CONTEXT) interrupts masked; zero descriptors.
        unsafe { gdt::write_tls(&fresh.tls) };
        // SAFETY: (CONTEXT) null selectors always load; the bases are zero, which is
        // valid.
        unsafe { load_selectors(fresh.selectors, &fresh.tls, 0, 0) };
        // The bases are the record's (3b), so the record is zeroed with the
        // MSRs: the new image never gets the old one's thread pointer back
        // at its next switch-in.
        // SAFETY: (CONTEXT) interrupts masked, inside the running task's own call.
        let _ = unsafe { crate::sched::with_own_user_state(|state| state.set_bases(0, 0)) };
    });
    // SAFETY: (CONTEXT) an area built by `UserState::new`, whose reserved bits are clear.
    unsafe { ferrix_fpu_restore(&raw const fresh.fpu, cpu::extended_state_components()) };
}

/// Load a program's four data selectors, each checked against `tls`, and
/// the `FS` and `GS` bases a null selector leaves to the MSRs: the thread
/// pointer `arch_prctl` set, and whatever `GS` base the program had.
///
/// # Safety
///
/// (CONTEXT) Interrupts masked, `tls` already in this processor's thread-local slots,
/// and both bases the program's own.
unsafe fn load_selectors(
    selectors: [u16; 4],
    tls: &[u64; gdt::TLS_SLOTS],
    fs_base: u64,
    gs_base: u64,
) {
    let [ds, es, fs, gs] = selectors.map(|selector| gdt::loadable(selector, tls));
    // The `DS`/`ES` skip (`docs/OPAQUE-KERNEL.md` §9.8, 3b, the consultant's
    // S1 to S7): `DS` or `ES` is left unloaded only where the selector to
    // load and the one the processor holds, read here with no load between,
    // are both exactly 0 -- not merely null, since 1 to 3 are null selectors
    // whose RPL bits a program reads back -- and never by a remembered value.
    // In long mode only an explicit load writes `DS` or `ES`, so one reading
    // 0 was last loaded with 0, and a vendor's null load is idempotent:
    // loading 0 again would leave the hidden part exactly as it is.
    let [held_ds, held_es, held_fs, held_gs] = cpu::read_data_selectors();
    let load_ds = ds != 0 || held_ds != 0;
    let load_es = es != 0 || held_es != 0;
    if load_ds {
        // SAFETY: (CONTEXT) null or loadable, as `gdt::loadable` checked.
        unsafe { cpu::load_ds(ds) };
    }
    if load_es {
        // SAFETY: (CONTEXT) null or loadable, as `gdt::loadable` checked.
        unsafe { cpu::load_es(es) };
    }
    if !(load_ds && load_es) {
        count(&SELECTOR_SKIPS);
    }
    // The `FS`/`GS` skip (§9.8 3c, A1 to A5; the consultant's K1 to K10):
    // the same rule, by the same read. `FS` and `GS` read 0 only after a load
    // of 0 -- a switch's, a program's, or the one `set_cpu_local` makes on
    // every processor at bring-up -- so the skip leaves the hidden part as a
    // load would. The one part of it a program can read through a null `FS`
    // or `GS` is the base, which is written below from the record wherever
    // the record's selector is 0, skipped or not: never compared with
    // anything (condition 8). A skipped `GS` load drops its `swapgs` pair and
    // the interrupt mask around it; the program's base is in
    // `KERNEL_GS_BASE`, which that pair never touched.
    if fs != 0 || held_fs != 0 {
        // SAFETY: (CONTEXT) null or loadable, as `gdt::loadable` checked.
        unsafe { cpu::load_fs(fs) };
    } else {
        count(&FS_SKIPS);
    }
    if fs == 0 {
        // SAFETY: (CONTEXT) a user address the program set, or zero.
        unsafe { super::syscall::set_thread_pointer(fs_base) };
    }
    if gs != 0 || held_gs != 0 {
        // SAFETY: (CONTEXT) as for the other three.
        unsafe { cpu::load_user_gs(gs) };
    } else {
        count(&GS_SKIPS);
    }
    if gs == 0 {
        // SAFETY: (CONTEXT) the program's own base, into the shadow it lives in while
        // the kernel runs.
        unsafe { super::syscall::set_program_gs_base(gs_base) };
    }
}

/// Load `selectors` -- `DS`, `ES`, `FS`, `GS` -- as a program's own, each
/// checked against this processor's thread-local slots, which are the
/// running thread's: what a 32-bit program's signal handler is entered with
/// and what its return from one puts back.
pub(crate) fn load_program_selectors(selectors: [u16; 4]) {
    with_interrupts_masked(|| {
        // SAFETY: (CONTEXT) interrupts masked, so these are the running thread's.
        let tls = unsafe { gdt::read_tls() };
        let [ds, es, fs, gs] = selectors.map(|selector| gdt::loadable(selector, &tls));
        // SAFETY: (CONTEXT) each checked loadable against the live slots.
        unsafe { cpu::load_data_selectors(ds, es, fs) };
        // SAFETY: (CONTEXT) as above.
        unsafe { cpu::load_user_gs(gs) };
    });
}

/// Give a 32-bit program user data in `DS` and `ES` as it is entered, from
/// its first instruction or after an `execve`: compatibility mode faults on a
/// null one, which is what `execve` and a new task leave. From then on the
/// program's own selectors travel with it (`save_user_state`).
///
/// `FS` stays null: both ways in come after `execve`'s reset or from a new
/// task's state, which leave it so.
pub(crate) fn enter_compat_segments() {
    let data = gdt::USER_DATA | 3;
    // SAFETY: (CONTEXT) user data is a present ring 3 data segment in every GDT this
    // kernel builds, and the null selector always loads.
    unsafe { cpu::load_data_selectors(data, data, 0) };
}

/// Install `descriptor` in the calling thread's thread-local slot `index`, or
/// in its first empty one when `index` is `None`, and answer the slot used:
/// `set_thread_area`'s half that is the processor's.
///
/// The slots are this processor's GDT's while the thread runs -- the switch
/// to it loaded them, and the switch away saves them -- so the write goes
/// there, with interrupts masked so the thread cannot move in between. A
/// data segment register naming the slot is reloaded, as Linux does, so the
/// program sees the new descriptor at once rather than at its next switch.
///
/// `None` when every slot is taken (`ESRCH` on Linux) or `index` is past
/// the three.
pub(crate) fn set_thread_area(index: Option<usize>, descriptor: u64) -> Option<usize> {
    with_interrupts_masked(|| {
        // SAFETY: (CONTEXT) interrupts masked, so these are the calling thread's.
        let mut tls = unsafe { gdt::read_tls() };
        let index = match index {
            Some(index) => index,
            None => tls.iter().position(|slot| *slot == 0)?,
        };
        *tls.get_mut(index)? = descriptor;
        // SAFETY: (CONTEXT) interrupts masked; `descriptor` is one `user_desc` built,
        // ring 3 data, or zero.
        unsafe { gdt::write_tls(&tls) };
        let named = |selector: u16| usize::from(selector >> 3) == gdt::TLS_FIRST_SLOT + index;
        let [ds, es, fs, gs] = cpu::read_data_selectors();
        if [ds, es, fs].into_iter().any(named) {
            let [ds, es, fs] = [ds, es, fs].map(|selector| gdt::loadable(selector, &tls));
            // SAFETY: (CONTEXT) each checked loadable against the slots just written.
            unsafe { cpu::load_data_selectors(ds, es, fs) };
        }
        if named(gs) {
            // SAFETY: (CONTEXT) as above.
            unsafe { cpu::load_user_gs(gdt::loadable(gs, &tls)) };
        }
        Some(index)
    })
}

/// The descriptor in the calling thread's thread-local slot `index`, zero
/// for an empty one: `get_thread_area`'s half that is the processor's.
pub(crate) fn thread_area(index: usize) -> Option<u64> {
    // SAFETY: (CONTEXT) interrupts masked by the closure's caller.
    with_interrupts_masked(|| unsafe { gdt::read_tls() }.get(index).copied())
}

/// Run `f` with interrupts masked, and put them back as they were.
fn with_interrupts_masked<R>(f: impl FnOnce() -> R) -> R {
    let open = cpu::read_rflags() & (1 << 9) != 0;
    cpu::disable_interrupts();
    let result = f();
    if open {
        cpu::enable_interrupts();
    }
    result
}
