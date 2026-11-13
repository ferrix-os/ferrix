//! Restricted mode on `AArch64` (`docs/microkernel/K1.md` §8).
//!
//! As on x86-64, an exit is rewriting a frame; here every way out of EL0 is an
//! exception and every way back `eret` from one frame, so there is one kind of
//! frame to rewrite. `TTBR0_EL1` holds the restricted space alone in restricted
//! mode and the normal root in normal mode; the kernel stays in `TTBR1_EL1`.

use ferrix_bootinfo::is_user_address;
use ferrix_errno::Errno;
use ferrix_native_abi::nr;
use ferrix_native_abi::restricted::{CONTEXT_BYTES, Context, FaultKind, Reason, SHARED_BASE};
use ferrix_native_abi::restricted::{ENTER_ALL_CALLS_EXIT, ENTER_COPY_MISS_EXITS, may_continue};
use ferrix_native_abi::status;

use super::switch::UserState;
use super::trap::TrapFrame;
use crate::arch::speculation;
use crate::fallible;
use crate::sched;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;

/// The condition flags: all of `pstate` a context may hold.
const PSTATE_NZCV: u64 = 0xF000_0000;
/// Where the reason goes in an exit's registers.
const REASON_REGISTER: usize = 9;
/// Where the number goes.
const NUMBER_REGISTER: usize = 8;

/// What a thread that has entered restricted mode holds, for its own life: only
/// the running task itself ever reaches it.
#[derive(Debug)]
pub(crate) struct RestrictedState {
    /// The program's registers, raw, as the processor saved them or a check let
    /// through.
    context: TrapFrame,
    /// The normal mode's frame, as its entry saved it.
    normal: TrapFrame,
    /// The user state of the mode that is not running.
    other: UserState,
    /// Whether a native number exits too, as `RESTRICTED_ENTER` asked: tested only for
    /// a native number, so a Linux number's path does not read it.
    all_calls_exit: bool,
    /// `ENTER_COPY_MISS_EXITS`, until the next entry.
    copy_miss_exits: bool,
    /// Where an answered call's copy last missed, and whether it wrote.
    copy_miss: Option<(u64, bool)>,
    /// Whether a context is held: from the first entry on.
    held: bool,
    /// The last exit's reason, zero before the first.
    last: u64,
}

impl RestrictedState {
    /// A thread's, before its first entry.
    fn new() -> RestrictedState {
        RestrictedState {
            context: TrapFrame::default(),
            normal: TrapFrame::default(),
            other: UserState::new(),
            all_calls_exit: false,
            copy_miss_exits: false,
            copy_miss: None,
            held: false,
            last: 0,
        }
    }
}

/// Whether `number` is one of the three calls that go in.
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

/// Check `context` as `RESTRICTED_ENTER` does (K1 §4), refused rather than
/// masked.
///
/// # Errors
///
/// The rule that failed.
pub(crate) fn check_context(context: &Context) -> Result<(), &'static str> {
    if !is_user_address(context.pc) || context.pc >= SHARED_BASE {
        return Err("pc");
    }
    if !is_user_address(context.sp) || context.sp > SHARED_BASE {
        return Err("sp");
    }
    if context.pstate & !PSTATE_NZCV != 0 {
        return Err("pstate");
    }
    // Only A32 code reads it, which nothing on this kernel runs: never written,
    // so that no value crosses a switch.
    if context.tpidrro != 0 {
        return Err("tpidrro");
    }
    Ok(())
}

/// The context's registers as a frame the way out restores.
fn frame_of(context: &Context) -> TrapFrame {
    TrapFrame {
        x: context.x,
        sp: context.sp,
        elr: context.pc,
        spsr: context.pstate & PSTATE_NZCV | super::trap::USER_SPSR,
        ..TrapFrame::default()
    }
}

/// What `RESTRICTED_CONTEXT_READ` gives.
fn read_context(state: &RestrictedState) -> Context {
    Context {
        x: state.context.x,
        sp: state.context.sp,
        pc: state.context.elr,
        pstate: state.context.spsr & PSTATE_NZCV,
        tpidr: state.other.thread_pointer(),
        tpidrro: 0,
    }
}

/// Swap the running task's user state with `other`.
fn swap_user_state(other: &mut UserState) {
    let swap = |state: &mut UserState| {
        // SAFETY: (CONTEXT) the outer block's: this task's registers, masked.
        unsafe { super::switch::save_user_state(state, false) };
        core::mem::swap(state, other);
        // SAFETY: (CONTEXT) the outer block's: this task's own record.
        unsafe { super::switch::restore_user_state(state, 0) };
    };
    // SAFETY: (CONTEXT) interrupts are masked by every caller, for the whole of
    // the swap, and these registers are the running task's.
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

/// Write `value` to the first register: a refusal, with the reason left zero.
fn refuse(frame: &mut TrapFrame, why: Errno) {
    if let Some(first) = frame.x.first_mut() {
        *first = why.as_return_value() as u64;
    }
}

/// Answer `RESTRICTED_ENTER`, `RESTRICTED_RESUME` or `RESTRICTED_CONTINUE`.
/// With interrupts masked.
pub(super) fn answer_going_in(frame: &mut TrapFrame, number: usize) {
    if !sched::running_in_region() {
        refuse(frame, status::BAD_STATE);
        return;
    }
    super::enable_interrupts();
    let prepared = prepare(number, frame.x[0], frame.x[1]);
    super::disable_interrupts();
    if let Err(why) = prepared {
        refuse(frame, why);
        return;
    }
    let Ok(state) = own_state() else {
        refuse(frame, status::BAD_STATE);
        return;
    };
    state.normal = *frame;
    // A kick made since the last exit: taken at once (K1 §2.4).
    if sched::work::take_kick() {
        write_exit(
            frame,
            &state.normal,
            Reason::Kick as u64,
            0,
            [0, 0, 0, state.context.elr, state.context.sp, 0],
        );
        state.last = Reason::Kick as u64;
        return;
    }
    swap_user_state(&mut state.other);
    flip(true);
    speculation::restricted_ran();
    *frame = state.context;
}

/// Whether the running thread's context asked that a native number exit too. A read of
/// its own record, with no allocation: it has entered, or it would not be here.
fn all_calls_exit() -> bool {
    // SAFETY: (CONTEXT) only the running task reaches its own record, for this call.
    unsafe { sched::own_restricted_slot() }
        .and_then(|slot| slot.as_deref())
        .is_some_and(|state| state.all_calls_exit)
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
    state.context = frame_of(&context);
    state.other.set_thread_pointer(context.tpidr);
    state.held = true;
    state.last = 0;
    Ok(())
}

/// What `number` changes in the held context, with interrupts open.
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
            if let Some(result) = state.context.x.first_mut() {
                *result = argument;
            }
        }
        _ => {
            // `ERET` to any address faults at EL0, as a program's own jump would.
            if !may_continue(state.held, state.last, state.context.elr, u64::MAX) {
                return Err(status::BAD_STATE);
            }
        }
    }
    Ok(())
}

/// Make `frame` the normal mode's, with the exit in its registers.
fn write_exit(
    frame: &mut TrapFrame,
    normal: &TrapFrame,
    reason: u64,
    number: u64,
    words: [u64; 6],
) {
    *frame = *normal;
    if let Some(registers) = frame.x.get_mut(..6) {
        registers.copy_from_slice(&words);
    }
    if let Some(slot) = frame.x.get_mut(NUMBER_REGISTER) {
        *slot = number;
    }
    if let Some(slot) = frame.x.get_mut(REASON_REGISTER) {
        *slot = reason;
    }
}

/// Rewrite `frame`, from restricted mode, into the normal mode's, with the exit
/// in its registers. With interrupts masked.
fn exit(frame: &mut TrapFrame, reason: u64, number: u64, words: [u64; 6]) {
    let Ok(state) = own_state() else {
        return;
    };
    state.context = *frame;
    swap_user_state(&mut state.other);
    speculation::restricted_left();
    flip(false);
    state.last = reason;
    write_exit(frame, &state.normal, reason, number, words);
}

/// A system call from restricted mode: answered in place by the table, or an
/// exit. `true` when it has been dealt with here; `false` when it is a native
/// call for the dispatcher, with the mode still up. With interrupts masked.
pub(super) fn from_restricted(frame: &mut TrapFrame) -> bool {
    let [x0, x1, x2, x3, x4, x5, _, _, x8, ..] = frame.x;
    let number = x8 as usize;
    if nr::is_native(number) && !all_calls_exit() {
        match nr::decode(number).map(nr::from_restricted) {
            Some(nr::FromRestricted::Exits) => {}
            Some(nr::FromRestricted::NormalOnly) => {
                refuse(frame, status::BAD_STATE);
                return true;
            }
            Some(nr::FromRestricted::Answered) | None => {
                forget_copy_miss();
                return false;
            }
        }
    }
    exit(frame, Reason::Syscall as u64, x8, [x0, x1, x2, x3, x4, x5]);
    true
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
/// 6529 Q4): a call that may run again exits instead, decided before `x0` is written, with
/// the registers as at the call, `x8` the number, and the pc back at its `SVC`. The context
/// is held and goes back by `CONTINUE`, under `may_continue`. `true` when it did. With
/// interrupts masked.
pub(super) fn call_copy_exit(
    frame: &mut TrapFrame,
    number: usize,
    outcome: &crate::trap::Outcome,
) -> bool {
    /// `SVC`'s length.
    const SVC_BYTES: u64 = 4;
    /// A data abort from EL0, and its syndrome's write bit.
    const DATA_ABORT: u64 = 0x24;
    const WNR: u64 = 1 << 6;
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
    frame.elr = frame.elr.wrapping_sub(SVC_BYTES);
    let syndrome = (DATA_ABORT << 26) | if write { WNR } else { 0 };
    let words = [
        syndrome,
        address,
        FaultKind::CallCopy as u64,
        frame.elr,
        frame.sp,
        0,
    ];
    exit(frame, Reason::Fault as u64, DATA_ABORT, words);
    true
}

/// A fault from restricted mode the kernel did not resolve: `FAULT`.
pub(crate) fn fault_exit(frame: &mut TrapFrame, kind: FaultKind, address: u64) {
    let words = [frame.esr, address, kind as u64, frame.elr, frame.sp, 0];
    let class = frame.esr >> 26;
    exit(frame, Reason::Fault as u64, class, words);
}

/// The look on the way back to restricted mode: `KICK`, if one has come.
pub(crate) fn kick_on_trap_frame(frame: &mut TrapFrame) {
    if !sched::running_restricted() || !sched::work::take_kick() {
        return;
    }
    let words = [0, 0, 0, frame.elr, frame.sp, 0];
    exit(frame, Reason::Kick as u64, 0, words);
}

/// Put `kind` in the result register and `landing` in the instruction pointer:
/// a copy window's fault (K1 §2.5).
pub(crate) fn land_copy(frame: &mut TrapFrame, landing: u64, kind: FaultKind) {
    frame.elr = landing;
    if let Some(first) = frame.x.first_mut() {
        *first = kind as u64;
    }
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

/// `RESTRICTED_VECTORS_WRITE`.
///
/// # Errors
///
/// `INVALID_ARGS` for reserved bits in `FPCR` or `FPSR`; `NO_MEMORY`.
pub fn set_vector_bytes(
    image: &[u8; ferrix_native_abi::restricted::VECTORS_BYTES],
) -> Result<(), Errno> {
    let state = own_state()?;
    state
        .other
        .set_vector_image(image)
        .map_err(|_| status::INVALID_ARGS)
}

/// What a system call means to restricted mode, ahead of the filter (K1 §3): an
/// answer here, or `None` to go on. A native call from restricted mode goes on,
/// past the filter, with the mode up.
pub(super) fn on_system_call(frame: &mut TrapFrame) -> Option<Result<(), &'static str>> {
    let number = frame.x[NUMBER_REGISTER] as usize;
    if sched::running_restricted() {
        return from_restricted(frame).then_some(Ok(()));
    }
    if goes_in(number) {
        answer_going_in(frame, number);
        return Some(Ok(()));
    }
    None
}

/// The Linux call `number` names, for a thread that has a personality to answer.
pub(super) fn decoded_unless_bare(number: usize) -> Option<ferrix_linux_abi::nr::Syscall> {
    if sched::running_bare() {
        return None;
    }
    super::decode_syscall(number)
}

/// No lazy vector state here: its review comes after the ASIDs (K1 §7.1 (12)).
pub(crate) const fn lazy_vector_trap(_: &TrapFrame) -> bool {
    false
}

/// See [`lazy_vector_trap`].
pub(crate) const fn before_switch(_: bool, _: *mut Option<alloc::boxed::Box<RestrictedState>>) {}

/// See [`lazy_vector_trap`].
pub(crate) const fn is_vector_in_kernel(_: &TrapFrame) -> bool {
    false
}
