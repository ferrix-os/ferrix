//! Restricted mode on x86-64 (`docs/microkernel/K1.md` §3, §4).
//!
//! # Going in
//!
//! The call's frame is saved as the normal mode's, the user state is swapped,
//! the mode and the root are changed, and the context goes out by `SYSRET`
//! where it can and `IRETQ` where it cannot.
//!
//! # Coming back
//!
//! **An exit is rewriting a frame**, on every path: the entry saved the
//! program's registers on the kernel stack as it always does; the exit copies
//! them to the held context, replaces the frame with the normal mode's, with
//! the exit's registers written over it, and goes on down the entry's ordinary
//! way out. So there is no second way out of the kernel to get right.
//!
//! # Why the context is the kernel's
//!
//! A page `linuxd` could write is one every exit and entry would have to treat
//! as hostile; here, the only write is [`enter_prepared`]'s, from a copy the
//! kernel checked, and `RESTRICTED_RESUME` has no pointer to read at all.

use ferrix_bootinfo::is_user_address;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::nr;
use ferrix_native_abi::restricted::{CONTEXT_BYTES, Context, FaultKind, Reason, SHARED_BASE};
use ferrix_native_abi::restricted::{ENTER_ALL_CALLS_EXIT, ENTER_COPY_MISS_EXITS, may_continue};
use ferrix_native_abi::status;

use super::gdt;
use super::switch::UserState;
use super::syscall::SyscallFrame;
use super::trap::TrapFrame;
use crate::arch::speculation;
use crate::fallible;
use crate::sched;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;

/// `RFLAGS.IF`.
const FLAGS_IF: u64 = 1 << 9;
/// The bit of `RFLAGS` that always reads as one.
const FLAGS_ONE: u64 = 1 << 1;
/// What a context handed in may hold in its flags: carry, parity, adjust,
/// zero, sign, direction, overflow and alignment check, with `IF` and bit 1,
/// which it must. `IOPL`, `VM`, `VIF`, `VIP`, `NT`, `RF`, `TF` and `ID` are
/// refused (K1 §4).
const FLAGS_ALLOWED: u64 = (1 << 0)
    | (1 << 2)
    | (1 << 4)
    | (1 << 6)
    | (1 << 7)
    | (1 << 10)
    | (1 << 11)
    | (1 << 18)
    | FLAGS_IF
    | FLAGS_ONE;
/// Ring 3's 64-bit code selector, with its RPL.
const USER_CS: u64 = (gdt::USER_CODE | 3) as u64;
/// Ring 3's 32-bit code selector, with its RPL.
const USER_CS32: u64 = (gdt::USER_CODE32 | 3) as u64;
/// Ring 3's stack selector, with its RPL.
const USER_SS: u64 = (gdt::USER_DATA | 3) as u64;

/// What a thread that has entered restricted mode holds, for its own life: only
/// the running task itself ever reaches it, so it needs no lock.
#[derive(Debug)]
pub(crate) struct RestrictedState {
    /// The program's registers, as the processor saved them or a check let
    /// through. Raw: `RESTRICTED_CONTINUE` re-enters it as it stands.
    context: TrapFrame,
    /// The normal mode's frame, as its entry saved it.
    normal: SyscallFrame,
    /// `ENTER_COPY_MISS_EXITS`, until the next entry.
    copy_miss_exits: bool,
    /// Where an answered call's copy last missed, and whether it wrote.
    copy_miss: Option<(u64, bool)>,
    /// The program's user state. `linuxd`'s is the task's own record, and
    /// `Task::user_state` answers with this one while the task is in restricted mode
    /// (`set_active_user_state`), so that neither path copies a record (K1 §7.1 (3)).
    other: UserState,
    /// Whether a native number exits too, as `RESTRICTED_ENTER` asked: tested only for
    /// a native number, so a Linux number's path does not read it.
    all_calls_exit: bool,
    /// Whether the vector registers hold the program's while normal mode runs, with
    /// `CR0.TS` set, so that its first vector instruction takes `#NM` (K1 §7.1).
    lazy: bool,
    /// Whether both modes' selectors are null and their descriptors alike, so that an
    /// exit and a re-entry need not touch them: set where the general path has just
    /// saved both, and read with the selectors the processor holds (K1 §7.1 (4)).
    clean: bool,
    /// Whether a context is held: from the first entry on.
    held: bool,
    /// The last exit's reason, zero before the first.
    last: u64,
    /// Its window, where the build has one and frames were to be had.
    #[cfg(ferrix_restricted_window = "on")]
    window: Option<Window>,
}

impl RestrictedState {
    /// A thread's, before its first entry: the vector registers' initial
    /// state, never `linuxd`'s.
    fn new() -> RestrictedState {
        RestrictedState {
            context: TrapFrame::default(),
            copy_miss_exits: false,
            copy_miss: None,
            normal: SyscallFrame {
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
                r11: 0,
                rcx: 0,
                user_rsp: 0,
            },
            other: UserState::new(),
            all_calls_exit: false,
            #[cfg(ferrix_restricted_window = "on")]
            window: None,
            lazy: false,
            clean: false,
            held: false,
            last: 0,
        }
    }
}

/// Whether `number` is one of the three calls that go in, for the entry: they
/// own the frame, as `rt_sigreturn` does.
pub(super) const fn goes_in(number: usize) -> bool {
    matches!(
        number,
        nr::RESTRICTED_ENTER | nr::RESTRICTED_RESUME | nr::RESTRICTED_CONTINUE
    )
}

/// The running task's state, made if it has none.
fn own_state() -> Result<&'static mut RestrictedState, Errno> {
    // SAFETY: (CONTEXT) only the running task reaches its own record, and the
    // reference does not outlive this call's caller, on the task's own stack.
    let slot = unsafe { sched::own_restricted_slot() }.ok_or(status::BAD_STATE)?;
    if slot.is_none() {
        *slot = Some(fallible::try_box(RestrictedState::new()).map_err(|_| status::NO_MEMORY)?);
        #[cfg(ferrix_restricted_window = "on")]
        if let Some(state) = slot.as_deref_mut() {
            state.window = window_make();
        }
    }
    slot.as_deref_mut().ok_or(status::BAD_STATE)
}

/// Check `context` as `RESTRICTED_ENTER` does: the same rules `rt_sigreturn`
/// has, refused rather than masked (K1 §4). Each failure names its rule for
/// the boot check.
///
/// # Errors
///
/// The rule that failed.
pub(crate) fn check_context(context: &Context) -> Result<(), &'static str> {
    let code_32 = match context.cs {
        USER_CS => false,
        USER_CS32 => true,
        _ => return Err("cs"),
    };
    if context.ss != USER_SS {
        return Err("ss");
    }
    if !in_window(context.rip) {
        return Err("rip");
    }
    if code_32 && context.rip > u64::from(u32::MAX) {
        return Err("rip in 32-bit code");
    }
    if !is_user_address(context.rsp) && context.rsp != SHARED_BASE || context.rsp > SHARED_BASE {
        return Err("rsp");
    }
    if context.rflags & !FLAGS_ALLOWED != 0 {
        return Err("rflags");
    }
    if context.rflags & FLAGS_IF == 0 || context.rflags & FLAGS_ONE == 0 {
        return Err("rflags");
    }
    for descriptor in context.tls {
        if descriptor != 0 && !gdt::ring_3_segment(descriptor) {
            return Err("tls");
        }
    }
    for selector in [context.ds, context.es, context.fs, context.gs] {
        let Ok(value) = u16::try_from(selector) else {
            return Err("selector width");
        };
        // Null is index zero, with any RPL.
        if value >> 2 == 0 && value & 0b100 == 0 {
            continue;
        }
        if gdt::loadable(value, &context.tls) != value | 3 {
            return Err("selector");
        }
    }
    for base in [context.fs_base, context.gs_base] {
        if base != 0 && !in_window(base) {
            return Err("base");
        }
    }
    Ok(())
}

/// Whether `address` is in the restricted window.
const fn in_window(address: u64) -> bool {
    is_user_address(address) && address < SHARED_BASE
}

/// The context's registers as a frame the way out restores.
const fn frame_of(context: &Context) -> TrapFrame {
    TrapFrame {
        r15: context.r15,
        r14: context.r14,
        r13: context.r13,
        r12: context.r12,
        r11: context.r11,
        r10: context.r10,
        r9: context.r9,
        r8: context.r8,
        rbp: context.rbp,
        rdi: context.rdi,
        rsi: context.rsi,
        rdx: context.rdx,
        rcx: context.rcx,
        rbx: context.rbx,
        rax: context.rax,
        vector: 0,
        error_code: 0,
        rip: context.rip,
        cs: context.cs,
        rflags: context.rflags,
        rsp: context.rsp,
        ss: context.ss,
    }
}

/// What `RESTRICTED_CONTEXT_READ` gives: the registers, the flags normalised
/// so that an entry accepts them, and the bases, selectors and descriptors of
/// the state the program left.
fn read_context(state: &RestrictedState) -> Context {
    let r = &state.context;
    let selectors = state.other.selectors();
    Context {
        r15: r.r15,
        r14: r.r14,
        r13: r.r13,
        r12: r.r12,
        rbp: r.rbp,
        rbx: r.rbx,
        r11: r.r11,
        r10: r.r10,
        r9: r.r9,
        r8: r.r8,
        rax: r.rax,
        rcx: r.rcx,
        rdx: r.rdx,
        rsi: r.rsi,
        rdi: r.rdi,
        rip: r.rip,
        rsp: r.rsp,
        rflags: normalised_flags(r.rflags),
        cs: r.cs,
        ss: r.ss,
        ds: u64::from(selectors[0]),
        es: u64::from(selectors[1]),
        fs: u64::from(selectors[2]),
        gs: u64::from(selectors[3]),
        fs_base: state.other.thread_pointer(),
        gs_base: state.other.gs_base(),
        tls: state.other.tls(),
    }
}

/// The flags a read gives: what an entry accepts, as K4's `supervised()` has
/// them. A control leaves these raw, and the `popf` case fires.
const fn normalised_flags(flags: u64) -> u64 {
    flags & FLAGS_ALLOWED | FLAGS_IF | FLAGS_ONE
}

/// Swap the running task's user state with `other`: saved from the processor,
/// and the other mode's loaded.
fn swap_user_state(to_program: bool, program: &mut UserState) {
    let top = sched::with_current(sched::Task::stack_top)
        .flatten()
        .unwrap_or(0);
    let target: *mut UserState = if to_program {
        program
    } else {
        core::ptr::null_mut()
    };
    // SAFETY: (CONTEXT) interrupts are masked by every caller for the length of the
    // change, and these registers are the running task's: its record of the mode it leaves.
    let save = |state: &mut UserState| unsafe { super::switch::save_user_state(state, false) };
    // SAFETY: (CONTEXT) as above.
    let _ = unsafe { sched::with_own_user_state(save) };
    sched::set_active_user_state(target);
    // SAFETY: (CONTEXT) as above; the running task's own record of the mode it enters.
    let restore = |state: &mut UserState| unsafe { super::switch::restore_user_state(state, top) };
    // SAFETY: (CONTEXT) as above.
    let _ = unsafe { sched::with_own_user_state(restore) };
}

/// Change the running task's mode, and the root under it.
#[inline(always)]
fn flip(restricted: bool) {
    if restricted {
        flip_to::<true>();
    } else {
        flip_to::<false>();
    }
}

/// [`flip`], one copy a direction: each closure has one call site, and goes inline.
#[inline(always)]
fn flip_to<const RESTRICTED: bool>() {
    sched::set_running_restricted(RESTRICTED);
    let _ = sched::with_current(|task| {
        if let Some(space) = task.address_space() {
            // SAFETY: (TRANSLATE) interrupts are masked by every caller; the task
            // holds the space, whose sets this processor joined at the switch.
            unsafe { space.flip_root(RESTRICTED) };
        }
    });
}

/// Marks the way it is called on as the unusual one, so the usual way falls through:
/// after the barrier a taken branch costs about 3 ns, one not taken nothing.
#[cold]
#[inline(always)]
#[cfg_attr(
    not(ferrix_restricted_fastpath = "on"),
    allow(dead_code, reason = "K1's fast crossings alone ask it")
)]
const fn unusual() {}

/// The registers `RESTRICTED_RESUME`'s caller keeps across the call, the ones not declared
/// clobbered by `ferrix_rt`, with `SYSRET`'s two and its stack pointer. Every other word of
/// a frame is written by the fast exit, so nothing else goes across and nothing of the
/// program's stays.
#[inline(always)]
#[cfg_attr(
    not(ferrix_restricted_fastpath = "on"),
    allow(dead_code, reason = "K1's fast crossings alone ask it")
)]
fn copy_kept(to: &mut SyscallFrame, from: &SyscallFrame) {
    to.r15 = from.r15;
    to.r14 = from.r14;
    to.r13 = from.r13;
    to.rbp = from.rbp;
    to.rbx = from.rbx;
    to.r11 = from.r11;
    to.rcx = from.rcx;
    to.user_rsp = from.user_rsp;
}

/// Answer `RESTRICTED_ENTER`, `RESTRICTED_RESUME` or `RESTRICTED_CONTINUE`.
/// Returns with `frame` for the stub's own way out, an exit or a refusal in
/// it or the program's registers, or does not return at all where only
/// `IRETQ` will do. With interrupts masked.
pub(super) fn answer_going_in(frame: &mut SyscallFrame, number: usize) {
    if sched::running_restricted() {
        frame.rax = status::BAD_STATE.as_return_value() as u64;
        return;
    }
    if !sched::running_in_region() {
        frame.rax = status::BAD_STATE.as_return_value() as u64;
        return;
    }
    super::enable_interrupts();
    let prepared = prepare(number, frame.rdi, frame.rsi);
    super::disable_interrupts();
    if let Err(why) = prepared {
        frame.rax = why.as_return_value() as u64;
        return;
    }
    let Ok(state) = own_state() else {
        frame.rax = status::BAD_STATE.as_return_value() as u64;
        return;
    };
    state.normal = *frame;
    // What came while interrupts were open: a process ended from outside ends
    // here, ahead of the program's next instruction (K1 §3).
    super::syscall::look_on_the_way_out(frame);
    // A kick made since the last exit: taken at once, running no instruction
    // of the program, and what this call handed in is held all the same
    // (K1 §2.4).
    if sched::work::take_kick() {
        write_exit(frame, state, Reason::Kick as u64, 0, [0; 3]);
        return;
    }
    materialise(state);
    swap_user_state(true, &mut state.other);
    state.clean = both_modes_clean(state);
    flip(true);
    speculation::restricted_ran();
    let context = state.context;
    if context.cs == USER_CS
        && context.rcx == context.rip
        && context.r11 == context.rflags
        && in_window(context.rip)
    {
        store_syscall(frame, &context);
        return;
    }
    sched::vtime::leaving();
    let resume = super::signal::UserContext::from_trap(&context);
    // SAFETY: (CONTEXT) the running task's own call, on its own kernel stack,
    // where nothing owned is left; `resume`'s selectors and addresses passed
    // `check_context`, or the processor saved them.
    unsafe { super::signal::resume_context(&resume) }
}

/// Whether the running thread's context asked that a native number exit too. A read of
/// its own record, with no allocation: it has entered, or it would not be here.
fn all_calls_exit() -> bool {
    // SAFETY: (CONTEXT) only the running task reaches its own record, for this call.
    unsafe { sched::own_restricted_slot() }
        .and_then(|slot| slot.as_deref())
        .is_some_and(|state| state.all_calls_exit)
}

/// Whether this build has the fast paths (`restricted_fastpath`, BUILD-FLAGS).
#[cfg_attr(
    not(ferrix_restricted_fastpath = "on"),
    allow(dead_code, reason = "K1's fast crossings alone ask it")
)]
const FAST: bool = cfg!(ferrix_restricted_fastpath = "on");

/// `CR0.TS` is set on this processor by [`fast_exit`]. Read at every switch: one byte.
static TS_SET: [core::sync::atomic::AtomicBool; ferrix_sched::MAX_CPUS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; ferrix_sched::MAX_CPUS];

#[inline(always)]
fn ts_set_here() -> Option<&'static core::sync::atomic::AtomicBool> {
    crate::smp::this_cpu().and_then(|cpu| TS_SET.get(cpu.logical))
}

/// Whether the program's and normal mode's records need nothing written for their
/// selectors or descriptors. With interrupts masked.
fn both_modes_clean(state: &mut RestrictedState) -> bool {
    let (program_selectors, program_tls) = (state.other.selectors(), state.other.tls());
    // SAFETY: (CONTEXT) the caller masks interrupts; a read of the task's own record,
    // which the switch path just left.
    unsafe {
        sched::with_own_task_record(|normal| {
            program_selectors == [0; 4]
                && normal.selectors() == [0; 4]
                && normal.tls() == program_tls
        })
    }
    .unwrap_or(false)
}

/// The two bases at a fast exit. `FS` stays the program's while normal mode's is zero: a
/// normal mode whose base is zero keeps nothing in `FS`, and it holds the program's
/// registers anyway; a switch or a kick takes the mark down and goes general, whose
/// restore writes the record (K1 §7.1 (4), ledger 6464). `GS` is the shadow, under `swapgs`.
#[inline(always)]
#[cfg_attr(
    not(ferrix_restricted_fastpath = "on"),
    allow(dead_code, reason = "K1's fast crossings alone ask it")
)]
fn write_exit_bases(normal_fs: u64, normal_gs: u64, program_gs: u64) {
    if normal_fs != 0 {
        // SAFETY: (SYSREG) normal mode's own record.
        unsafe { super::cpu::write_msr(super::syscall::IA32_FS_BASE, normal_fs) };
    }
    if normal_gs != program_gs {
        // SAFETY: (SYSREG) as above.
        unsafe { super::cpu::write_msr(super::syscall::IA32_KERNEL_GS_BASE, normal_gs) };
    }
}

/// The two bases at a fast re-entry, undoing [`write_exit_bases`].
#[inline(always)]
#[cfg_attr(
    not(ferrix_restricted_fastpath = "on"),
    allow(dead_code, reason = "K1's fast crossings alone ask it")
)]
fn write_entry_bases(normal_fs: u64, normal_gs: u64, program_fs: u64, program_gs: u64) {
    if normal_fs != 0 {
        // SAFETY: (SYSREG) a canonical base: checked as `RESTRICTED_ENTER` took it.
        unsafe { super::cpu::write_msr(super::syscall::IA32_FS_BASE, program_fs) };
    }
    if program_gs != normal_gs {
        // SAFETY: (SYSREG) as above.
        unsafe { super::cpu::write_msr(super::syscall::IA32_KERNEL_GS_BASE, program_gs) };
    }
}

/// The window's slot's first address, and the stub and answer pages in it.
#[cfg(ferrix_restricted_window = "on")]
const WINDOW_STUB: u64 = (crate::mm::WINDOW_ROOT_SLOT as u64) << 39;
/// See [`WINDOW_STUB`].
#[cfg(ferrix_restricted_window = "on")]
const WINDOW_DATA: u64 = WINDOW_STUB + 0x1000;
/// The stub, an exact pattern the kernel writes: `movabs $WINDOW_DATA, %rsi; mov (%rsi), %rdi;
/// mov $RESTRICTED_RESUME, %eax; xor %esi, %esi; syscall; ud2`. No loop, no return, no indirect
/// branch, no vector register (ledger 6787 W3, 6826).
#[cfg(ferrix_restricted_window = "on")]
const WINDOW_CODE: [u8; 24] = {
    let a = WINDOW_DATA.to_le_bytes();
    let n = (nr::RESTRICTED_RESUME as u32).to_le_bytes();
    [
        0x48, 0xbe, a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], 0x48, 0x8b, 0x3e, 0xb8, n[0],
        n[1], n[2], n[3], 0x31, 0xf6, 0x0f, 0x05, 0x0f, 0x0b,
    ]
};
/// Where `SYSCALL` in the stub leaves `rcx`.
#[cfg(ferrix_restricted_window = "on")]
const WINDOW_RETURN: u64 = WINDOW_STUB + 22;

/// The page under the window's tables that nothing touches and nothing invalidates: after a
/// close a program's load there must fault, or the processor kept an upper level (ledger 6787 W6).
#[cfg(ferrix_restricted_window = "on")]
pub(crate) const WINDOW_CANARY: u64 = WINDOW_STUB + 0x2000;

/// A thread's window: its private tables and answer, made at its first entry, charged to its
/// job. They die with the state, and a thread that has a state to drop is not running a stub.
#[cfg(ferrix_restricted_window = "on")]
#[derive(Debug)]
struct Window {
    /// The value the restricted root's slot takes while open.
    value: u64,
    /// The number armed across consecutive trips, `u64::MAX` for none.
    number: u64,
    /// The answer word, through the direct map.
    data: *mut u64,
    /// The job they were charged to.
    owner: u32,
    /// Its frames' addresses: three tables and the answer.
    frames: [u64; WINDOW_FRAMES],
}

/// A window's own frames.
#[cfg(ferrix_restricted_window = "on")]
const WINDOW_FRAMES: usize = 4;

// SAFETY: (SHARED) its pointer is this thread's page, reached by the running task alone.
#[cfg(ferrix_restricted_window = "on")]
unsafe impl Send for Window {}
// SAFETY: (SHARED) as above.
#[cfg(ferrix_restricted_window = "on")]
unsafe impl Sync for Window {}

#[cfg(ferrix_restricted_window = "on")]
impl Drop for Window {
    fn drop(&mut self) {
        let _ = WINDOWS_LIVE.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        for at in self.frames {
            crate::mm::window_frame_free(at);
        }
        crate::object::quota::uncharge_kernel(self.owner, WINDOW_FRAMES as u64 * 4096);
    }
}

/// How many windows there are, for the check that they go with their threads.
#[cfg(ferrix_restricted_window = "on")]
static WINDOWS_LIVE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The space whose window this processor holds, null for none: the mark every way in reads first.
#[cfg(ferrix_restricted_window = "on")]
static WINDOW_OPEN: [core::sync::atomic::AtomicPtr<AddressSpace>; ferrix_sched::MAX_CPUS] =
    [const { core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()) }; ferrix_sched::MAX_CPUS];
/// Trips and declines, this processor's, for the row and the check.
#[cfg(ferrix_restricted_window = "on")]
static WINDOW_TRIPS: [core::sync::atomic::AtomicU64; ferrix_sched::MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; ferrix_sched::MAX_CPUS];
/// See [`WINDOW_TRIPS`].
#[cfg(ferrix_restricted_window = "on")]
static WINDOW_DECLINES: [core::sync::atomic::AtomicU64; ferrix_sched::MAX_CPUS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; ferrix_sched::MAX_CPUS];
/// The stub's page and the canary's, once each for the machine, zero until the first window.
#[cfg(ferrix_restricted_window = "on")]
static WINDOW_SHARED: [core::sync::atomic::AtomicU64; 2] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 2];

/// Why this machine has no window, by the boot's decision (ledger 6787 W2): failing closed to
/// two roots. An index into [`WINDOW_WHY`]: 0 is that it has one, and nothing was decided is 1.
#[cfg(ferrix_restricted_window = "on")]
static WINDOW_BARRED: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(1);
/// What the boot line says of [`WINDOW_BARRED`].
#[cfg(ferrix_restricted_window = "on")]
const WINDOW_WHY: [&str; 6] = [
    "",
    "the defences are off",
    "PCIDs are on, and INVLPG here is per PCID, unreviewed",
    "EFER.TCE is set",
    "no IBPB",
    "not smt=off, or a family other than AMD's 1Ah",
];

/// What this processor adds to the decision: `CR4.PCIDE` and `EFER.TCE` clear, as the argument
/// that a close leaves nothing cached rests on them (ledger 6998, X8).
#[cfg(ferrix_restricted_window = "on")]
fn window_barred_here() -> u8 {
    /// `IA32_EFER`.
    const EFER: u32 = 0xC000_0080;
    /// `EFER.TCE`: translation cache extension, an INVLPG that keeps upper levels.
    const TCE: u64 = 1 << 15;
    if super::cpu::read_cr4() & super::pcid::CR4_PCIDE != 0 {
        return 2;
    }
    // SAFETY: (SYSREG) `IA32_EFER` exists on every processor that has long mode.
    if unsafe { super::cpu::read_msr(EFER) } & TCE != 0 {
        return 3;
    }
    0
}

/// The boot processor's decision, once `ibpb` is known and before any secondary.
#[cfg(ferrix_restricted_window = "on")]
pub(super) fn window_decide(ibpb: bool, known_family: bool) {
    let here = window_barred_here();
    let barred = if here != 0 {
        here
    } else if !ibpb {
        4
    } else if !cfg!(ferrix_smt = "off") || !known_family {
        5
    } else {
        0
    };
    WINDOW_BARRED.store(barred, core::sync::atomic::Ordering::Relaxed);
}

/// A secondary's part of it: it can only bar, before the first program.
#[cfg(ferrix_restricted_window = "on")]
pub(super) fn window_decide_here() {
    let here = window_barred_here();
    if here != 0 {
        WINDOW_BARRED.store(here, core::sync::atomic::Ordering::Relaxed);
    }
}

/// The boot line of the decision.
#[cfg(ferrix_restricted_window = "on")]
pub fn window_report() {
    let barred = WINDOW_BARRED.load(core::sync::atomic::Ordering::Relaxed);
    if barred == 0 {
        crate::console::println!(
            "  rwindow  armed calls take a stub, no root write, while alone in a space"
        );
    } else {
        let why = WINDOW_WHY.get(usize::from(barred)).copied().unwrap_or("");
        crate::console::println!("  rwindow  two roots: {why}");
    }
}

/// One of the machine's two shared pages, made on first asking.
#[cfg(ferrix_restricted_window = "on")]
fn window_shared(which: usize, bytes: &[u8]) -> Option<u64> {
    use core::sync::atomic::Ordering;
    let slot = WINDOW_SHARED.get(which)?;
    let held = slot.load(Ordering::Acquire);
    if held != 0 {
        return Some(held);
    }
    let (at, page) = crate::mm::window_frame()?;
    // SAFETY: (FRAME) a fresh frame of ours, through the direct map, that no root names yet.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), page.cast::<u8>(), bytes.len()) };
    match slot.compare_exchange(0, at, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(at),
        Err(other) => {
            crate::mm::window_frame_free(at);
            Some(other)
        }
    }
}

/// Make a thread's window, at its first entry, with interrupts open. A machine barred, a job at
/// its limit or no frame: no window, and the thread takes two roots (ledger 6787 W11).
#[cfg(ferrix_restricted_window = "on")]
fn window_make() -> Option<Window> {
    const TABLE: u64 = 0x7 | 0x20;
    const CODE: u64 = 0x5 | 0x20;
    const DATA: u64 = 0x5 | 0x20 | (1 << 63);
    if WINDOW_BARRED.load(core::sync::atomic::Ordering::Relaxed) != 0 {
        return None;
    }
    let text = window_shared(0, &WINDOW_CODE)?;
    let canary = window_shared(1, &[])?;
    let owner = crate::object::quota::charge_target();
    crate::object::quota::charge_kernel(owner, WINDOW_FRAMES as u64 * 4096).ok()?;
    let mut frames = [0_u64; WINDOW_FRAMES];
    let mut pointers = [core::ptr::null_mut::<u64>(); WINDOW_FRAMES];
    for at in 0..WINDOW_FRAMES {
        let Some((frame, pointer)) = crate::mm::window_frame() else {
            for made in frames.iter().copied().filter(|made| *made != 0) {
                crate::mm::window_frame_free(made);
            }
            crate::object::quota::uncharge_kernel(owner, WINDOW_FRAMES as u64 * 4096);
            return None;
        };
        frames[at] = frame;
        pointers[at] = pointer;
    }
    let _ = WINDOWS_LIVE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let [pdpt_at, pd_at, pt_at, data_at] = frames;
    let [pdpt, pd, pt, data] = pointers;
    // SAFETY: (FRAME) fresh frames of ours, through the direct map, and no root names them yet.
    unsafe {
        pdpt.write(pd_at | TABLE);
        pd.write(pt_at | TABLE);
        pt.write(text | CODE);
        pt.add(1).write(data_at | DATA);
        pt.add(2).write(canary | DATA);
    }
    Some(Window {
        value: pdpt_at | TABLE,
        number: u64::MAX,
        data,
        owner,
        frames,
    })
}

/// Open the window for `frame`'s call, if armed for it and this processor is alone in the
/// space. Takes the barrier as any exit does; `CR0.TS` is not set, on the customer's trade for
/// this exact stub (ledger 6826). `true`: `frame` runs the stub, masked.
#[cfg(ferrix_restricted_window = "on")]
#[inline(always)]
fn window_open(
    frame: &mut SyscallFrame,
    state: &RestrictedState,
    task: &sched::Task,
    cpu: usize,
) -> bool {
    let Some(window) = state.window.as_ref() else {
        return false;
    };
    if window.number != frame.rax {
        return false;
    }
    // SAFETY: (CONTEXT) a program's thread holds its space.
    let space = unsafe { task.address_space().unwrap_unchecked() };
    if !space.window_take(cpu) {
        unusual();
        // SAFETY: (CONTEXT) a processor's logical number is below `MAX_CPUS`.
        let declines = unsafe { WINDOW_DECLINES.get_unchecked(cpu) };
        declines.store(
            declines.load(core::sync::atomic::Ordering::Relaxed) + 1,
            core::sync::atomic::Ordering::Relaxed,
        );
        return false;
    }
    speculation::restricted_left_window(cpu);
    space
        .window_entry()
        .store(window.value, core::sync::atomic::Ordering::Relaxed);
    // SAFETY: (CONTEXT) a processor's logical number is below `MAX_CPUS`.
    unsafe { WINDOW_OPEN.get_unchecked(cpu) }.store(
        alloc::sync::Arc::as_ptr(space).cast_mut(),
        core::sync::atomic::Ordering::Relaxed,
    );
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
        r11: 2,
        rcx: WINDOW_STUB,
        user_rsp: 0,
    };
    true
}

/// Clear the slot and all the processor can hold of it, and give the window back.
#[cfg(ferrix_restricted_window = "on")]
#[inline(always)]
fn window_shut(space: &AddressSpace, cpu: usize) {
    space
        .window_entry()
        .store(0, core::sync::atomic::Ordering::Relaxed);
    super::cpu::invalidate_page(WINDOW_STUB);
    super::cpu::invalidate_page(WINDOW_DATA);
    // SAFETY: (CONTEXT) a processor's logical number is below `MAX_CPUS`.
    unsafe { WINDOW_OPEN.get_unchecked(cpu) }
        .store(core::ptr::null_mut(), core::sync::atomic::Ordering::Relaxed);
    space.window_give();
}

/// The space this processor's mark names.
///
/// # Safety
///
/// (CONTEXT) `at` is the mark's value, not null: the thread that opened it holds that space
/// until it has closed, and is the one running here.
#[cfg(ferrix_restricted_window = "on")]
#[inline(always)]
const unsafe fn window_space<'a>(at: *mut AddressSpace) -> &'a AddressSpace {
    // SAFETY: (CONTEXT) as the function says.
    unsafe { &*at }
}

/// A stop on this processor with its mark up, at the panic handler's first line: a joiner would
/// otherwise wait on a dead holder (ledger 6998, X4). No lock, and it waits on nothing.
#[cfg(ferrix_restricted_window = "on")]
pub(crate) fn window_release_for_stop() {
    let Some(cpu) = crate::smp::this_cpu() else {
        return;
    };
    // SAFETY: (CONTEXT) a processor's logical number is below `MAX_CPUS`.
    let at = unsafe { WINDOW_OPEN.get_unchecked(cpu.logical) }
        .load(core::sync::atomic::Ordering::Relaxed);
    if !at.is_null() {
        // SAFETY: (CONTEXT) the mark is up, on the thread that raised it.
        window_shut(unsafe { window_space(at) }, cpu.logical);
    }
}

/// The stub's `SYSCALL`: close, count the trip and go straight back into the program.
#[cfg(ferrix_restricted_window = "on")]
#[inline(always)]
pub(super) fn window_close_if_open(frame: &mut SyscallFrame) -> bool {
    // SAFETY: (SHARED) a call from ring 3: a running thread, masked.
    let (task, cpu) = unsafe { sched::fast::here() };
    // SAFETY: (CONTEXT) a processor's logical number is below `MAX_CPUS`.
    let at = unsafe { WINDOW_OPEN.get_unchecked(cpu.logical) }
        .load(core::sync::atomic::Ordering::Relaxed);
    if at.is_null() {
        return false;
    }
    // SAFETY: (CONTEXT) the mark is up, on the thread that raised it.
    window_shut(unsafe { window_space(at) }, cpu.logical);
    if frame.rcx != WINDOW_RETURN || frame.rax != nr::RESTRICTED_RESUME as u64 {
        crate::panic::fatal!(
            crate::panic::catalog::WINDOW_HELD,
            "a stub's call was not its own"
        );
    }
    // SAFETY: (CONTEXT) it had one to open.
    let state = unsafe { sched::fast::state(task) };
    state.context.rax = frame.rdi;
    // SAFETY: (CONTEXT) a processor's logical number is below `MAX_CPUS`.
    let trips = unsafe { WINDOW_TRIPS.get_unchecked(cpu.logical) };
    trips.store(
        trips.load(core::sync::atomic::Ordering::Relaxed) + 1,
        core::sync::atomic::Ordering::Relaxed,
    );
    speculation::restricted_ran_on(cpu.logical);
    store_syscall(frame, &state.context);
    true
}

/// A trap in ring 3 with the window open: the stub cannot take one. Shut, so that a joiner
/// does not wait on a dead holder, and stop by name (ledger 6998, X4).
#[cfg(ferrix_restricted_window = "on")]
pub(super) fn window_trap_check(vector: u64) {
    // SAFETY: (SHARED) a trap from ring 3: a running thread, masked.
    let (_, cpu) = unsafe { sched::fast::here() };
    // SAFETY: (CONTEXT) a processor's logical number is below `MAX_CPUS`.
    let at = unsafe { WINDOW_OPEN.get_unchecked(cpu.logical) }
        .load(core::sync::atomic::Ordering::Relaxed);
    if !at.is_null() {
        // SAFETY: (CONTEXT) the mark is up, on the thread that raised it.
        window_shut(unsafe { window_space(at) }, cpu.logical);
        crate::panic::fatal!(
            crate::panic::catalog::WINDOW_HELD,
            "a stub took trap {vector}"
        );
    }
}

/// Let go of the running thread's window, as it ends: running, so no stub is open.
#[cfg(ferrix_restricted_window = "on")]
pub(crate) fn window_forget_own() {
    // SAFETY: (CONTEXT) the running task's own slot, not kept past this call.
    if let Some(Some(state)) = unsafe { sched::own_restricted_slot() } {
        state.window = None;
    }
}

/// How many windows are alive.
#[cfg(ferrix_restricted_window = "on")]
pub(crate) fn windows_live() -> u64 {
    WINDOWS_LIVE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Whether the boot decided this machine has a window.
#[cfg(ferrix_restricted_window = "on")]
pub(crate) fn window_on() -> bool {
    WINDOW_BARRED.load(core::sync::atomic::Ordering::Relaxed) == 0
}

/// Trips and declines on every processor.
#[cfg(ferrix_restricted_window = "on")]
pub(crate) fn window_counts() -> (u64, u64) {
    let sum = |counters: &[core::sync::atomic::AtomicU64]| {
        counters
            .iter()
            .map(|c| c.load(core::sync::atomic::Ordering::Relaxed))
            .sum()
    };
    (sum(&WINDOW_TRIPS), sum(&WINDOW_DECLINES))
}

/// `SYSCALL`'s fast exit, ahead of the entry's kernel-time stamp, if every condition
/// holds: as the native fast path's, its time counts as user time, and no reading is
/// taken for it (`sched::vtime`). `false`: nothing has changed. Every condition is read
/// and one branch taken on all of them, so the usual way runs straight down.
#[inline(always)]
#[cfg_attr(
    not(ferrix_restricted_fastpath = "on"),
    allow(dead_code, reason = "K1's fast crossings alone ask it")
)]
pub(super) fn fast_exit_here(frame: &mut SyscallFrame) -> bool {
    // SAFETY: (CONTEXT) the entry's caller saw restricted mode: a running thread with a
    // record, masked.
    let (task, cpu) = unsafe { sched::fast::here() };
    // SAFETY: (CONTEXT) as above.
    let state = unsafe { sched::fast::state(task) };
    let selectors = super::cpu::read_data_selectors();
    if !FAST
        || nr::is_native(frame.rax as usize)
            | (!state.clean)
            | state.lazy
            | (!sched::fast::quiet(task, cpu.logical))
            | ((selectors[0] | selectors[1] | selectors[2] | selectors[3]) != 0)
    {
        unusual();
        return false;
    }
    state.context = super::signal::UserContext::from_syscall(frame).into_frame();
    #[cfg(ferrix_restricted_window = "on")]
    {
        if window_open(frame, state, task, cpu.logical) {
            state.last = Reason::Syscall as u64;
            return true;
        }
        // Any exit that is not a window's drops the arm (6787 W4).
        if let Some(window) = state.window.as_mut() {
            window.number = u64::MAX;
        }
    }
    let (rax, words) = (
        frame.rax,
        [
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ],
    );
    state.last = Reason::Syscall as u64;
    sched::fast::set_active(task, core::ptr::null_mut());
    let program_gs = state.other.gs_base();
    // SAFETY: (CONTEXT) interrupts are masked; the task's own record, active again.
    let normal = unsafe { sched::fast::own_record(task) };
    let (normal_fs, normal_gs) = (normal.thread_pointer(), normal.gs_base());
    write_exit_bases(normal_fs, normal_gs, program_gs);
    speculation::restricted_left_here(cpu.logical);
    super::cpu::set_task_switched();
    // SAFETY: a processor's logical number is below `MAX_CPUS`.
    unsafe { TS_SET.get_unchecked(cpu.logical) }.store(true, core::sync::atomic::Ordering::Relaxed);
    state.lazy = true;
    sched::fast::set_mode(task, cpu, false);
    // SAFETY: a program's thread holds its space.
    let space = unsafe { task.address_space().unwrap_unchecked() };
    // SAFETY: (TRANSLATE) interrupts are masked; a program's thread holds its space,
    // whose sets this processor joined at the switch.
    unsafe { space.flip_root_on(false, cpu.logical) };
    copy_kept(frame, &state.normal);
    frame.rax = rax;
    [
        frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
    ] = words;
    frame.r12 = Reason::Syscall as u64;
    true
}

/// [`fast_resume`] for the entry, by number.
#[inline(always)]
#[cfg_attr(
    not(ferrix_restricted_fastpath = "on"),
    allow(dead_code, reason = "K1's fast crossings alone ask it")
)]
pub(super) fn fast_resume_here(frame: &mut SyscallFrame) -> bool {
    FAST && frame.rax as usize == nr::RESTRICTED_RESUME && fast_resume(frame)
}

/// `RESTRICTED_RESUME` after [`fast_exit`], with the vector registers still the
/// program's, nothing else to do, and the last exit `SYSCALL`'s exactly: `rcx` and `r11`
/// are the processor's, so `SYSRET` is safe, and the context has no check to pass since
/// its only writes are the processor's and a checked entry's. `false` for the general
/// path, with nothing changed.
#[inline(always)]
#[cfg_attr(
    not(ferrix_restricted_fastpath = "on"),
    allow(dead_code, reason = "K1's fast crossings alone ask it")
)]
fn fast_resume(frame: &mut SyscallFrame) -> bool {
    // SAFETY: (SHARED) a call from ring 3: a running thread, masked.
    let (task, cpu) = unsafe { sched::fast::here() };
    let Some(state) = sched::fast::state_of(task) else {
        unusual();
        return false;
    };
    let selectors = super::cpu::read_data_selectors();
    // SAFETY: a `SYSCALL` comes from a user thread, whose task holds its core process.
    let core = unsafe { task.core_process().unwrap_unchecked() };
    if cpu.runs_restricted()
        | core.region().is_none()
        | (!state.lazy)
        | (!state.held)
        | (!state.clean)
        | (state.last != Reason::Syscall as u64)
        | (!sched::fast::quiet(task, cpu.logical))
        | ((selectors[0] | selectors[1] | selectors[2] | selectors[3]) != 0)
    {
        unusual();
        return false;
    }
    state.context.rax = frame.rdi;
    // After the conditions, whose work word holds any `RESTRICTED_KICK` made so far (ledger
    // 6998, X6), and a later one is read by the next trip: the arm the resume asks for, with its
    // value, or none.
    #[cfg(ferrix_restricted_window = "on")]
    if let Some(window) = state.window.as_mut() {
        if frame.rsi != 0 {
            // SAFETY: (FRAME) the thread's own answer page, through the direct map.
            unsafe { core::sync::atomic::AtomicU64::from_ptr(window.data) }
                .store(frame.rdi, core::sync::atomic::Ordering::Relaxed);
            window.number = frame.rsi - 1;
        } else {
            window.number = u64::MAX;
        }
    }
    copy_kept(&mut state.normal, frame);
    // Up first, so that the next exit's barrier cannot be skipped (6093 (2)).
    speculation::restricted_ran();
    super::cpu::clear_task_switched();
    // SAFETY: a processor's logical number is below `MAX_CPUS`.
    unsafe { TS_SET.get_unchecked(cpu.logical) }
        .store(false, core::sync::atomic::Ordering::Relaxed);
    state.lazy = false;
    // SAFETY: (CONTEXT) interrupts are masked; the task's own record, active still.
    let normal = unsafe { sched::fast::own_record(task) };
    let (normal_fs, normal_gs) = (normal.thread_pointer(), normal.gs_base());
    let (program_fs, program_gs) = (state.other.thread_pointer(), state.other.gs_base());
    write_entry_bases(normal_fs, normal_gs, program_fs, program_gs);
    sched::fast::set_active(task, &raw mut state.other);
    sched::fast::set_mode(task, cpu, true);
    // SAFETY: a program's thread holds its space.
    let space = unsafe { task.address_space().unwrap_unchecked() };
    // SAFETY: (TRANSLATE) as at the exit.
    unsafe { space.flip_root_on(true, cpu.logical) };
    store_syscall(frame, &state.context);
    true
}

/// Take the vector registers down, where [`fast_exit`] left them up: the program's into
/// its record, normal mode's back, `CR0.TS` clear. Every way that reads vector state goes
/// through here first. With interrupts masked.
fn materialise(state: &mut RestrictedState) {
    if !state.lazy {
        return;
    }
    state.lazy = false;
    super::cpu::clear_task_switched();
    if let Some(flag) = ts_set_here() {
        flag.store(false, core::sync::atomic::Ordering::Relaxed);
    }
    // SAFETY: (CONTEXT) the registers are the program's, by the mark, and the record the
    // processor wrote or `UserState::new` built.
    unsafe { state.other.save_vectors_only() };
    // SAFETY: (CONTEXT) as above; its own record.
    let restore = |normal: &mut UserState| unsafe { normal.restore_vectors_only() };
    // SAFETY: (CONTEXT) as above.
    let _ = unsafe { sched::with_own_user_state(restore) };
}

/// `#NM` in normal mode with the mark up: materialise and go on. `false` for any other
/// `#NM`, which is whatever it was before (K1 §7.1 (8)). With interrupts masked.
pub(crate) fn lazy_vector_trap(frame: &TrapFrame) -> bool {
    const DEVICE_NOT_AVAILABLE: u64 = 7;
    if frame.vector != DEVICE_NOT_AVAILABLE
        || !frame.came_from_user()
        || sched::running_restricted()
    {
        return false;
    }
    // SAFETY: (CONTEXT) only the running task reaches its own record, for this call.
    let Some(state) =
        (unsafe { sched::own_restricted_slot() }).and_then(|slot| slot.as_deref_mut())
    else {
        return false;
    };
    if !state.lazy {
        return false;
    }
    materialise(state);
    true
}

/// Whether `frame` is `#NM` in ring 0, which its own panic code explains.
pub(crate) fn is_vector_in_kernel(frame: &TrapFrame) -> bool {
    frame.vector == 7 && !frame.came_from_user()
}

/// Ahead of any switch, dead task or not (6093 (1)): `CR0.TS` clear for whoever comes
/// next, and, for a task that lives on, the registers saved as the program's, its own
/// record told that the switch has nothing to save. So no other task reads the program's
/// values, and `XRSTOR` does not fault in ring 0 (K1 §7.1 (b)). One byte unless it is up.
pub(crate) fn before_switch(dead: bool, slot: *mut Option<alloc::boxed::Box<RestrictedState>>) {
    if !FAST {
        return;
    }
    let Some(flag) = ts_set_here() else {
        return;
    };
    if !flag.swap(false, core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    super::cpu::clear_task_switched();
    if dead {
        return;
    }
    // SAFETY: (CONTEXT) the outgoing task's own slot, this processor still running it,
    // and no other reaches it.
    let Some(state) = (unsafe { &mut *slot }).as_deref_mut() else {
        return;
    };
    state.lazy = false;
    // SAFETY: (CONTEXT) the registers are the program's, as the flag being up says.
    unsafe { state.other.save_vectors_only() };
    // SAFETY: (CONTEXT) the outgoing task's record, active being null in normal mode.
    let _ = unsafe { sched::with_own_user_state(UserState::skip_next_vector_save) };
}

/// `RESTRICTED_ENTER`'s context: copied once and checked, and held.
fn take_context(state: &mut RestrictedState, argument: u64) -> Result<(), Errno> {
    let mut bytes = [0_u8; CONTEXT_BYTES];
    let space = sched::with_current(|task| task.address_space().cloned())
        .flatten()
        .ok_or(status::BAD_STATE)?;
    uaccess::copy_from_user(&space, argument, &mut bytes).map_err(|_| status::INVALID_ARGS)?;
    let context = Context::from_bytes(&bytes);
    check_context(&context).map_err(|_| status::INVALID_ARGS)?;
    enter_prepared(state, &context);
    Ok(())
}

/// What `number` changes in the held context, before the processor is told:
/// with interrupts open, since a copy may fault and a first entry allocates.
fn prepare(number: usize, argument: u64, options: u64) -> Result<(), Errno> {
    let state = own_state()?;
    match number {
        nr::RESTRICTED_ENTER => {
            if options & !(ENTER_ALL_CALLS_EXIT | ENTER_COPY_MISS_EXITS) != 0 {
                return Err(status::INVALID_ARGS);
            }
            take_context(state, argument)?;
            state.all_calls_exit = options & ENTER_ALL_CALLS_EXIT != 0;
            state.copy_miss_exits = options & ENTER_COPY_MISS_EXITS != 0;
        }
        nr::RESTRICTED_RESUME => {
            if !state.held
                || (state.last != Reason::Syscall as u64
                    && state.last != Reason::SyscallCompat as u64)
            {
                return Err(status::BAD_STATE);
            }
            state.context.rax = argument;
        }
        _ => {
            // After a kick or a fault alone (I1), and never at an address `IRETQ`
            // would fault on in ring 0: the rip a program's own jump left.
            if !may_continue(
                state.held,
                state.last,
                state.context.rip,
                ferrix_bootinfo::USER_VIRT_END,
            ) {
                return Err(status::BAD_STATE);
            }
        }
    }
    Ok(())
}

/// Hold `context`, which has passed [`check_context`]. The only write of the
/// held context apart from the processor's own.
fn enter_prepared(state: &mut RestrictedState, context: &Context) {
    #[cfg(ferrix_restricted_window = "on")]
    if let Some(window) = state.window.as_mut() {
        window.number = u64::MAX;
    }
    state.context = frame_of(context);
    state.other.set_thread_pointer(context.fs_base);
    state.other.set_gs_base_record(context.gs_base);
    state.other.set_tls(context.tls);
    state.other.set_selectors([
        context.ds as u16,
        context.es as u16,
        context.fs as u16,
        context.gs as u16,
    ]);
    state.held = true;
    state.last = 0;
}

/// Put `context` where the `SYSRET` way out restores it from.
#[inline(always)]
const fn store_syscall(frame: &mut SyscallFrame, context: &TrapFrame) {
    frame.r15 = context.r15;
    frame.r14 = context.r14;
    frame.r13 = context.r13;
    frame.r12 = context.r12;
    frame.rbp = context.rbp;
    frame.rbx = context.rbx;
    frame.r9 = context.r9;
    frame.r8 = context.r8;
    frame.r10 = context.r10;
    frame.rdx = context.rdx;
    frame.rsi = context.rsi;
    frame.rdi = context.rdi;
    frame.rax = context.rax;
    frame.r11 = context.r11;
    frame.rcx = context.rcx;
    frame.user_rsp = context.rsp;
}

/// A `SYSCALL` from restricted mode: a native number answered here, by the
/// table (K1 §2.2), or an exit. With interrupts masked; returns for the stub's
/// way out, and `true` when the call has been answered by exiting.
pub(super) fn from_restricted(frame: &mut SyscallFrame) -> Option<crate::trap::Outcome> {
    let number = frame.rax as usize;
    if nr::is_native(number) && !all_calls_exit() {
        match nr::decode(number).map(nr::from_restricted) {
            Some(nr::FromRestricted::Exits) => {}
            Some(nr::FromRestricted::NormalOnly) => {
                frame.rax = status::BAD_STATE.as_return_value() as u64;
                return None;
            }
            // A gap in the table is `ENOSYS`, from the dispatcher.
            Some(nr::FromRestricted::Answered) | None => {
                let args = crate::trap::SyscallArgs {
                    abi: crate::trap::Abi::Native,
                    number,
                    args: [
                        frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
                    ],
                    ip: frame.rcx,
                };
                forget_copy_miss();
                super::enable_interrupts();
                let regs = super::syscall::UserRegs::Syscall(*frame);
                let outcome = crate::trap::system_call(&args, Some(&regs));
                super::disable_interrupts();
                if call_copy_exit(frame, number, &outcome) {
                    return None;
                }
                return Some(outcome);
            }
        }
    }
    let Ok(state) = own_state() else {
        return None;
    };
    state.context = super::signal::UserContext::from_syscall(frame).into_frame();
    let (rax, words) = (
        frame.rax,
        [
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ],
    );
    leave_restricted(state);
    *frame = state.normal;
    frame.rax = rax;
    [
        frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
    ] = words;
    frame.r12 = Reason::Syscall as u64;
    state.last = Reason::Syscall as u64;
    None
}

/// A copy of an answered call that missed, in `space`: kept only with the option up, for
/// this thread's own restricted space (ledger 6529 Q1). The one byte first: nothing else is
/// read on `present`'s error path.
pub(crate) fn note_copy_miss(space: &AddressSpace, at: u64, write: bool) {
    if !sched::running_restricted() {
        return;
    }
    let own = sched::with_current(|task| {
        task.address_space()
            .is_some_and(|mine| core::ptr::eq(core::ptr::from_ref(&**mine), space))
    });
    // SAFETY: (CONTEXT) only the running task reaches its own record, for this call.
    let slot = unsafe { sched::own_restricted_slot() };
    if own == Some(true)
        && let Some(state) = slot.and_then(|slot| slot.as_deref_mut())
        && state.copy_miss_exits
    {
        state.copy_miss = Some((at, write));
    }
}

/// A failure the call goes on past is not the miss of a `FAULT` it answers.
pub(crate) fn forget_copy_miss() {
    if !sched::running_restricted() {
        return;
    }
    // SAFETY: (CONTEXT) only the running task reaches its own record, for this call.
    let slot = unsafe { sched::own_restricted_slot() };
    if let Some(state) = slot.and_then(|slot| slot.as_deref_mut()) {
        state.copy_miss = None;
    }
}

/// The call answered `FAULT` because its copy missed (`docs/microkernel/K1.md` §2.2, ledger
/// 6529 Q4): a call that may run again exits instead, decided before anything of the answer
/// is written, with the registers as at the call and the pc back at its `SYSCALL`, `rcx` and
/// `r11` as that left them. The context is held and goes back by `CONTINUE`, under
/// `may_continue`. `true` when it did.
fn call_copy_exit(frame: &mut SyscallFrame, number: usize, outcome: &crate::trap::Outcome) -> bool {
    /// `SYSCALL`'s length.
    const SYSCALL_BYTES: u64 = 2;
    /// The page fault's vector, and its code's write bit.
    const PAGE_FAULT: u64 = 14;
    const PF_WRITE: u64 = 1 << 1;
    let Ok(state) = own_state() else {
        return false;
    };
    let Some((address, write)) = state.copy_miss.take() else {
        return false;
    };
    if !matches!(outcome, crate::trap::Outcome::Return(value) if *value == status::FAULT.as_return_value())
        || !nr::decode(number)
            .is_some_and(|call| !matches!(nr::copy_miss(call), nr::CopyMiss::AnswersFault))
    {
        return false;
    }
    let mut context = super::signal::UserContext::from_syscall(frame).into_frame();
    context.rip = frame.rcx.wrapping_sub(SYSCALL_BYTES);
    state.context = context;
    leave_restricted(state);
    *frame = state.normal;
    write_exit(
        frame,
        state,
        Reason::Fault as u64,
        PAGE_FAULT,
        [
            if write { PF_WRITE } else { 0 },
            address,
            FaultKind::CallCopy as u64,
        ],
    );
    true
}

/// The way back to user mode's look, on the frame a native call from restricted
/// mode answers on: the kick, if one has come; `true` if it exited.
pub(super) fn kick_on_syscall_frame(frame: &mut SyscallFrame) {
    if !sched::running_restricted() || !sched::work::take_kick() {
        return;
    }
    let Ok(state) = own_state() else {
        return;
    };
    state.context = super::signal::UserContext::from_syscall(frame).into_frame();
    leave_restricted(state);
    state.last = Reason::Kick as u64;
    *frame = state.normal;
    write_exit(frame, state, Reason::Kick as u64, 0, [0; 3]);
}

/// Leave restricted mode, with the program's registers already held: the user
/// state, the barrier, the root. With interrupts masked.
fn leave_restricted(state: &mut RestrictedState) {
    #[cfg(ferrix_restricted_window = "on")]
    if let Some(window) = state.window.as_mut() {
        window.number = u64::MAX;
    }
    swap_user_state(false, &mut state.other);
    speculation::restricted_left();
    flip(false);
}

/// Write an exit that did not come from a system call into the normal frame.
fn write_exit(
    frame: &mut SyscallFrame,
    state: &mut RestrictedState,
    reason: u64,
    number: u64,
    words: [u64; 3],
) {
    state.last = reason;
    frame.r12 = reason;
    frame.rax = number;
    [frame.rdi, frame.rsi, frame.rdx] = words;
    frame.r10 = state.context.rip;
    frame.r8 = state.context.rsp;
    frame.r9 = 0;
}

/// Rewrite `frame`, a trap from restricted mode, into the normal mode's, with
/// the exit in its registers: `int $0x80`, an unresolved fault, a kick. With
/// interrupts masked.
fn exit_from_trap(frame: &mut TrapFrame, reason: u64, number: u64, words: [u64; 6]) {
    let Ok(state) = own_state() else {
        return;
    };
    state.context = *frame;
    state.context.vector = 0;
    state.context.error_code = 0;
    leave_restricted(state);
    state.last = reason;
    let normal = state.normal;
    *frame = TrapFrame {
        r15: normal.r15,
        r14: normal.r14,
        r13: normal.r13,
        r12: reason,
        r11: normal.r11,
        r10: words[3],
        r9: words[5],
        r8: words[4],
        rbp: normal.rbp,
        rdi: words[0],
        rsi: words[1],
        rdx: words[2],
        rcx: normal.rcx,
        rbx: normal.rbx,
        rax: number,
        vector: 0,
        error_code: 0,
        rip: normal.rcx,
        cs: USER_CS,
        rflags: normal.r11,
        rsp: normal.user_rsp,
        ss: USER_SS,
    };
}

/// `int $0x80` from restricted mode: always an exit (K1 §2.2).
pub(super) fn compat_exit(frame: &mut TrapFrame) {
    let word = |register: u64| register & 0xFFFF_FFFF;
    let words = [
        word(frame.rbx),
        word(frame.rcx),
        word(frame.rdx),
        word(frame.rsi),
        word(frame.rdi),
        word(frame.rbp),
    ];
    let number = word(frame.rax);
    exit_from_trap(frame, Reason::SyscallCompat as u64, number, words);
}

/// A fault from restricted mode the kernel did not resolve: `FAULT`.
pub(crate) fn fault_exit(frame: &mut TrapFrame, kind: FaultKind, address: u64) {
    let words = [
        frame.error_code,
        address,
        kind as u64,
        frame.rip,
        frame.rsp,
        0,
    ];
    let vector = frame.vector;
    exit_from_trap(frame, Reason::Fault as u64, vector, words);
}

/// The look on a trap's way back to restricted mode: `KICK`, if one has come.
pub(crate) fn kick_on_trap_frame(frame: &mut TrapFrame) {
    if !sched::running_restricted() || !sched::work::take_kick() {
        return;
    }
    let words = [0, 0, 0, frame.rip, frame.rsp, 0];
    exit_from_trap(frame, Reason::Kick as u64, 0, words);
}

/// Put `kind` in the result register and `landing` in the instruction pointer:
/// a copy window's fault (K1 §2.5).
pub(crate) const fn land_copy(frame: &mut TrapFrame, landing: u64, kind: FaultKind) {
    frame.rip = landing;
    frame.rax = kind as u64;
}

/// `RESTRICTED_CONTEXT_READ`'s bytes.
///
/// # Errors
///
/// `BAD_STATE` with no context held.
pub fn context_bytes() -> Result<[u8; CONTEXT_BYTES], Errno> {
    let state = own_state()?;
    if !state.held {
        return Err(status::BAD_STATE);
    }
    Ok(read_context(state).to_bytes())
}

/// `RESTRICTED_VECTORS_READ`'s bytes.
///
/// # Errors
///
/// `BAD_STATE` with no context held.
pub fn vector_bytes() -> Result<[u8; ferrix_native_abi::restricted::VECTORS_BYTES], Errno> {
    let state = own_state()?;
    if !state.held {
        return Err(status::BAD_STATE);
    }
    with_vectors_down(state);
    Ok(state.other.vector_image())
}

/// [`materialise`], masked for its length, from a call with interrupts open.
fn with_vectors_down(state: &mut RestrictedState) {
    let saved = <super::Irq as ferrix_sync::IrqControl>::disable();
    materialise(state);
    <super::Irq as ferrix_sync::IrqControl>::restore(saved);
}

/// `RESTRICTED_VECTORS_WRITE`: `image`, held to `restore_fpu`'s rules, refused
/// rather than masked (K1 §5.2).
///
/// # Errors
///
/// `INVALID_ARGS` for an image the processor would refuse; `NO_MEMORY`.
pub fn set_vector_bytes(
    image: &[u8; ferrix_native_abi::restricted::VECTORS_BYTES],
) -> Result<(), Errno> {
    let state = own_state()?;
    with_vectors_down(state);
    state
        .other
        .set_vector_image(image)
        .map_err(|_| status::INVALID_ARGS)
}
