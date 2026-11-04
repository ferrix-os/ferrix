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
use ferrix_native_abi::restricted::may_continue;
use ferrix_native_abi::restricted::{CONTEXT_BYTES, Context, FaultKind, Reason, SHARED_BASE};
use ferrix_native_abi::status;

use super::gdt;
use super::switch::UserState;
use super::syscall::SyscallFrame;
use super::trap::TrapFrame;
use crate::arch::speculation;
use crate::fallible;
use crate::sched;
use crate::syscall::uaccess;

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
    /// The user state of the mode that is not running: `linuxd`'s while the
    /// program runs, the program's while `linuxd` does.
    other: UserState,
    /// Whether a context is held: from the first entry on.
    held: bool,
    /// The last exit's reason, zero before the first.
    last: u64,
}

impl RestrictedState {
    /// A thread's, before its first entry: the vector registers' initial
    /// state, never `linuxd`'s.
    fn new() -> RestrictedState {
        RestrictedState {
            context: TrapFrame::default(),
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
fn swap_user_state(other: &mut UserState) {
    let top = sched::with_current(sched::Task::stack_top)
        .flatten()
        .unwrap_or(0);
    let swap = |state: &mut UserState| {
        // SAFETY: (CONTEXT) this task's registers, and it is not blocked.
        unsafe { super::switch::save_user_state(state, false) };
        core::mem::swap(state, other);
        // SAFETY: (CONTEXT) the running task's own record and kernel stack.
        unsafe { super::switch::restore_user_state(state, top) };
    };
    // SAFETY: (CONTEXT) interrupts are masked by every caller, for the whole
    // of the swap, and these registers are the running task's.
    let _ = unsafe { sched::with_own_user_state(swap) };
}

/// Change the running task's mode, and the root under it.
fn flip(restricted: bool) {
    sched::set_running_restricted(restricted);
    if let Some(space) = sched::with_current(|task| task.address_space().cloned()).flatten() {
        // SAFETY: (TRANSLATE) interrupts are masked by every caller; the task
        // holds the space, whose sets this processor joined at the switch.
        unsafe { space.flip_root(restricted) };
    }
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
    let prepared = prepare(number, frame.rdi);
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
    swap_user_state(&mut state.other);
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
fn prepare(number: usize, argument: u64) -> Result<(), Errno> {
    let state = own_state()?;
    match number {
        nr::RESTRICTED_ENTER => take_context(state, argument)?,
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
    if nr::is_native(number) {
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
                super::enable_interrupts();
                let regs = super::syscall::UserRegs::Syscall(*frame);
                let outcome = crate::trap::system_call(&args, Some(&regs));
                super::disable_interrupts();
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
    swap_user_state(&mut state.other);
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
    Ok(state.other.vector_image())
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
    state
        .other
        .set_vector_image(image)
        .map_err(|_| status::INVALID_ARGS)
}
