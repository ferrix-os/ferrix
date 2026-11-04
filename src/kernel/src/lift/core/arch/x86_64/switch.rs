//! A program's user state as linuxd keeps it (docs/microkernel/M5-LIFT.md §4.2):
//! the same record as the kernel's; K1's registers fill it at a restricted exit.

use super::{cpu, gdt};

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
pub struct UserState {
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
#[doc(hidden)]
pub struct FpuArea {
    /// x87 and SSE, as `FXSAVE` lays them out.
    pub legacy: [u8; 512],
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
    #[doc(hidden)]
    pub fn keep_vector_controls(&mut self, mxcsr: u32, control: u16) {
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
    #[doc(hidden)]
    pub const fn is_unsaved(&self) -> bool {
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
    pub const fn set_thread_pointer(&mut self, pointer: u64) {
        self.thread_pointer = pointer;
    }

    /// Give the program `base` as its `GS` base in this record, beside the
    /// shadow MSR the caller writes: what `arch_prctl(ARCH_SET_GS)` does.
    pub(super) const fn set_gs_base(&mut self, base: u64) {
        self.gs_base = base;
    }

    /// Put `descriptor` in thread-local slot `index` of this saved state: a
    /// 32-bit program's `CLONE_SETTLS`, which names a `user_desc` for the
    /// child rather than a base (`docs/I386.md` §3.5). The child's `%gs`
    /// keeps its parent's selector and so reads through the new descriptor.
    /// False for an `index` past the three.
    pub fn set_thread_area(&mut self, index: usize, descriptor: u64) -> bool {
        self.tls
            .get_mut(index)
            .map(|slot| *slot = descriptor)
            .is_some()
    }

    /// A program's state before it has run: no thread pointer, and the x87 and
    /// SSE control words a processor has at reset ([`FpuArea::initial`]).
    #[allow(
        clippy::new_without_default,
        reason = "a const constructor for a static; Default is not const"
    )]
    pub const fn new() -> UserState {
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
    #[doc(hidden)]
    pub fn fxsave(&self) -> [u8; 512] {
        self.vectors().legacy
    }

    /// The same area, for `rt_sigreturn` to fill from the frame.
    #[doc(hidden)]
    pub fn fxsave_mut(&mut self) -> &mut [u8; 512] {
        self.materialise();
        &mut self.fpu.legacy
    }

    /// Which components the area holds, `XSTATE_BV`: a component left out
    /// is in its initial state, all zero for AVX.
    #[doc(hidden)]
    pub fn xstate_bv(&self) -> u64 {
        self.vectors()
            .header
            .first_chunk::<8>()
            .map_or(0, |word| u64::from_le_bytes(*word))
    }

    /// Set `XSTATE_BV`. `XRSTOR` refuses with `#GP` a component `XCR0` does
    /// not enable, so only those are kept.
    #[doc(hidden)]
    pub fn set_xstate_bv(&mut self, components: u64) {
        let kept = components & cpu::extended_state_components();
        self.materialise();
        if let Some(word) = self.fpu.header.first_chunk_mut::<8>() {
            *word = kept.to_le_bytes();
        }
    }

    /// The upper halves of the sixteen `YMM` registers, through
    /// [`UserState::vectors`].
    #[doc(hidden)]
    pub fn avx(&self) -> [u8; 256] {
        self.vectors().avx
    }

    /// The same, for `rt_sigreturn` to fill from the frame.
    #[doc(hidden)]
    pub fn avx_mut(&mut self) -> &mut [u8; 256] {
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
    pub const fn initial(mxcsr: u32, control: u16) -> FpuArea {
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

impl UserState {
    /// The state's vector image, as K1 reads and writes it.
    pub fn image(&self) -> &[u8] {
        // SAFETY: FpuArea is repr(C), three byte arrays with no padding.
        unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(&self.fpu).cast::<u8>(),
                core::mem::size_of::<FpuArea>(),
            )
        }
    }
    /// The same, to be written.
    pub fn image_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `image`; every byte pattern is a `u8`.
        unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::from_mut(&mut self.fpu).cast::<u8>(),
                core::mem::size_of::<FpuArea>(),
            )
        }
    }
    /// Take the bases, TLS slots and selectors from K1's context.
    pub fn read_context(&mut self, context: &ferrix_native::region::Context) {
        self.thread_pointer = context.fs_base;
        self.gs_base = context.gs_base;
        self.tls = context.tls;
        self.selectors = [
            context.ds as u16,
            context.es as u16,
            context.fs as u16,
            context.gs as u16,
        ];
    }
    /// Give the bases, TLS slots and selectors to K1's context.
    pub fn write_context(&self, context: &mut ferrix_native::region::Context) {
        context.fs_base = self.thread_pointer;
        context.gs_base = self.gs_base;
        context.tls = self.tls;
        context.ds = u64::from(self.selectors[0]);
        context.es = u64::from(self.selectors[1]);
        context.fs = u64::from(self.selectors[2]);
        context.gs = u64::from(self.selectors[3]);
    }
    /// The running program's state: K1 holds it while linuxd runs.
    ///
    /// # Safety
    ///
    /// Called from the program's own thread, in a restricted exit.
    pub unsafe fn capture() -> UserState {
        crate::restricted::capture_state()
    }
}
/// # Safety
///
/// Called on the thread `state` is the state of.
pub(crate) unsafe fn load_fpu(state: &UserState) {
    crate::restricted::load_vectors(state);
}
/// # Safety
///
/// Called on the thread whose state it resets, between restricted entries.
pub unsafe fn reset_user_state() {
    crate::restricted::reset_state();
}
pub fn set_thread_area(index: Option<usize>, descriptor: u64) -> Option<usize> {
    crate::restricted::set_thread_area(index, descriptor)
}
pub fn thread_area(index: usize) -> Option<u64> {
    crate::restricted::thread_area(index)
}
pub(crate) fn load_program_selectors(selectors: [u16; 4]) {
    crate::restricted::set_selectors(selectors);
}
pub(crate) fn enter_compat_segments() {
    crate::restricted::set_compat();
}
