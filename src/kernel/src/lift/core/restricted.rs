//! The restricted loop (docs/microkernel/M5-LIFT.md §4.2): one Linux thread on
//! one kernel thread, alternating between the program and the load.
//!
//! What the kernel's entry stub does around the load on a trap, this does around
//! it on a restricted exit: the same dispatch, the same return path, on linuxd's
//! stack. K1 holds the program's registers; `RESTRICTED_RESUME` puts back a
//! result, `RESTRICTED_ENTER` any other change.

use core::sync::atomic::{AtomicPtr, Ordering};

use ferrix_native::region::{Context, FaultKind, Reason};
use ferrix_rt::Kernel;
use ferrix_rt::restricted as k1;

#[cfg(target_arch = "x86_64")]
use crate::arch::x86_64::gdt::{USER_CODE, USER_CODE32, USER_DATA};
use crate::arch::{TrapFrame, UserContext, UserRegs, UserState};
use crate::sched;
use crate::trap::{self, Abi, Outcome, SyscallArgs};

/// What answers a system call: the load's `dispatch_with::<Linux>`, which this
/// crate cannot name.
pub type Dispatch = fn(&SyscallArgs, Option<&UserRegs>) -> Outcome;
static DISPATCH: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_dispatch(dispatch: Dispatch) {
    DISPATCH.store(dispatch as *mut (), Ordering::Release);
}
pub fn dispatched(args: &SyscallArgs, regs: Option<&UserRegs>) -> Outcome {
    dispatch(args, regs)
}
fn dispatch(args: &SyscallArgs, regs: Option<&UserRegs>) -> Outcome {
    let raw = DISPATCH.load(Ordering::Acquire);
    assert!(!raw.is_null(), "no dispatch registered");
    // SAFETY: only `set_dispatch` writes it, with a `Dispatch`.
    let dispatch: Dispatch = unsafe { core::mem::transmute(raw) };
    dispatch(args, regs)
}

/// Where `FXSAVE` keeps `MXCSR_MASK`, and the word the first capture read there.
#[cfg(target_arch = "x86_64")]
const MXCSR_MASK_AT: usize = 28;
#[cfg(target_arch = "x86_64")]
static MXCSR_MASK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[cfg(target_arch = "x86_64")]
const RFLAGS_USER: u64 = 0x202;
#[cfg(target_arch = "x86_64")]
const SYSCALL_BYTES: u64 = 2;
#[cfg(target_arch = "aarch64")]
const SYSCALL_BYTES: u64 = 4;

/// The program's registers as K1 holds them: read once per change, written back
/// by the next `enter`.
fn read_context() -> Context {
    ferrix_native::region::context_read(Kernel).unwrap_or_else(|why| {
        crate::println!("linuxd: the program's context could not be read: {why:?}");
        Context::default()
    })
}

#[cfg(target_arch = "aarch64")]
fn to_trap(c: &Context) -> TrapFrame {
    TrapFrame {
        x: c.x,
        sp: c.sp,
        elr: c.pc,
        spsr: c.pstate,
        esr: 0,
        far: 0,
    }
}
#[cfg(target_arch = "aarch64")]
fn from_trap(f: &TrapFrame, c: &mut Context) {
    c.x = f.x;
    c.sp = f.sp;
    c.pc = f.elr;
    c.pstate = f.spsr;
}
#[cfg(target_arch = "aarch64")]
const fn set_call_answer(c: &mut Context, value: u64) {
    c.x[0] = value;
}
#[cfg(target_arch = "x86_64")]
const fn set_call_answer(c: &mut Context, value: u64) {
    c.rax = value;
}
#[cfg(target_arch = "aarch64")]
const fn program_counter(c: &Context) -> u64 {
    c.pc
}
#[cfg(target_arch = "x86_64")]
const fn program_counter(c: &Context) -> u64 {
    c.rip
}
#[cfg(target_arch = "x86_64")]
fn to_trap(c: &Context) -> TrapFrame {
    TrapFrame {
        r15: c.r15,
        r14: c.r14,
        r13: c.r13,
        r12: c.r12,
        r11: c.r11,
        r10: c.r10,
        r9: c.r9,
        r8: c.r8,
        rbp: c.rbp,
        rdi: c.rdi,
        rsi: c.rsi,
        rdx: c.rdx,
        rcx: c.rcx,
        rbx: c.rbx,
        rax: c.rax,
        vector: 0,
        error_code: 0,
        rip: c.rip,
        cs: c.cs,
        rflags: c.rflags,
        rsp: c.rsp,
        ss: c.ss,
    }
}
#[cfg(target_arch = "x86_64")]
fn from_trap(f: &TrapFrame, c: &mut Context) {
    c.r15 = f.r15;
    c.r14 = f.r14;
    c.r13 = f.r13;
    c.r12 = f.r12;
    c.r11 = f.r11;
    c.r10 = f.r10;
    c.r9 = f.r9;
    c.r8 = f.r8;
    c.rbp = f.rbp;
    c.rdi = f.rdi;
    c.rsi = f.rsi;
    c.rdx = f.rdx;
    c.rcx = f.rcx;
    c.rbx = f.rbx;
    c.rax = f.rax;
    c.rip = f.rip;
    c.cs = f.cs;
    c.rflags = f.rflags;
    c.rsp = f.rsp;
    c.ss = f.ss;
}

/// What a thread's next `enter` must change, as `UserState`'s setters ask.
fn with_pending<R>(change: impl FnOnce(&mut Context) -> R) -> R {
    let task = sched::current().expect("a program's thread has a task");
    let mut pending = task.pending.lock();
    let context = pending.get_or_insert_with(read_context);
    task.pending_up.store(true, Ordering::Relaxed);
    change(context)
}

/// Round to where a program's thread goes next: back with `value`, or by `enter`
/// if anything changed.
#[inline(always)]
fn back(value: u64, not_a_call: bool) -> k1::Exit {
    // The usual way, every call's: nothing held and nothing due, with the task lent. A
    // clone of it is two locked operations a call; only this thread raises the flag.
    let due = ferrix_lift_attention();
    let plain = (!due)
        & (sched::with_current(|task| !task.pending_up.load(Ordering::Relaxed)) == Some(true));
    if plain {
        // `RESTRICTED_RESUME` answers a system call exit alone (K1 §2.1).
        return if not_a_call {
            again_continue()
        } else {
            again_resume(value)
        };
    }
    unusual();
    let path = trap::return_path();
    let task = sched::current().expect("a program's thread has a task");
    let pending = if task.pending_up.swap(false, Ordering::Relaxed) {
        task.pending.lock().take()
    } else {
        None
    };
    if pending.is_none() && !due {
        return if not_a_call {
            again_continue()
        } else {
            again_resume(value)
        };
    }
    let mut context = pending.unwrap_or_else(read_context);
    // A call's answer; after any other exit the program's own register stays.
    if !not_a_call {
        set_call_answer(&mut context, value);
    }
    if let (true, Some(path)) = (due, path) {
        let mut user = UserContext::from_trap(&to_trap(&context));
        let _ = task.pending.lock().replace(context);
        (path.return_to_user)(&mut user);
        task.pending_up.store(false, Ordering::Relaxed);
        let mut after = task.pending.lock().take().unwrap_or_else(read_context);
        let mut frame = to_trap(&after);
        user.store_trap(&mut frame);
        from_trap(&frame, &mut after);
        context = after;
    }
    again_enter(&context)
}

#[cfg(target_arch = "aarch64")]
fn fresh(entry: u64, stack: u64, argument: u64, _abi: Abi) -> Context {
    let mut context = Context::default();
    context.pc = entry;
    context.sp = stack;
    context.x[0] = argument;
    context
}
#[cfg(target_arch = "x86_64")]
fn fresh(entry: u64, stack: u64, argument: u64, abi: Abi) -> Context {
    let mut context = Context::default();
    context.rip = entry;
    context.rsp = stack;
    context.rdi = argument;
    context.rflags = RFLAGS_USER;
    context.cs = u64::from(
        if abi == Abi::Compat {
            USER_CODE32
        } else {
            USER_CODE
        } | 3,
    );
    context.ss = u64::from(USER_DATA | 3);
    if abi == Abi::Compat {
        // 32-bit code addresses through them: a null selector there is #GP.
        context.ds = u64::from(USER_DATA | 3);
        context.es = u64::from(USER_DATA | 3);
    }
    context
}

/// Whether a call reads the program's registers: a fork's child resumes from
/// them, `sigreturn` and `arch_prctl` change them, a filter is shown the
/// instruction pointer, and `sigaltstack` asks the stack pointer. Every other
/// call goes without the kernel entry a read costs (MICROKERNEL.md §7).
#[inline(always)]
fn needs_context(abi: Abi, number: usize) -> bool {
    /// What reads a register, in `dispatch_with`'s callees: a fork's child resumes from
    /// them, `sigaltstack` asks the stack pointer, and the two `answer_early` takes, by
    /// x86-64 number: clone, clone3, fork, vfork, sigaltstack, arch_prctl, rt_sigreturn.
    const READS: [u64; 8] = {
        let mut bits = [0_u64; 8];
        // By AArch64's numbers: clone, clone3, sigaltstack, rt_sigreturn; it has no fork,
        // vfork or arch_prctl.
        #[cfg(target_arch = "aarch64")]
        let numbers = [220_usize, 435, 132, 139];
        #[cfg(target_arch = "x86_64")]
        let numbers = [56_usize, 435, 57, 58, 131, 158, 15];
        let mut at = 0;
        while at < numbers.len() {
            bits[numbers[at] / 64] |= 1 << (numbers[at] % 64);
            at += 1;
        }
        bits
    };
    // A 32-bit program's table has sigreturns and segments of its own: on no row, so every
    // call of one reads them first rather than a list being kept here. A filter is shown the
    // instruction pointer. A number past the table is nobody's.
    if (!matches!(abi, Abi::Native)) | (number >= 512) | (!ferrix_lift_quiet()) {
        unusual();
        return true;
    }
    READS[number / 64] & (1 << (number % 64)) != 0
}

/// Whether no program has installed a filter: the load's own word, told here.
static QUIET: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// How to ask whether the running program has a seccomp filter.
pub fn set_quiet(quiet: fn() -> bool) {
    QUIET.store(quiet as *mut (), Ordering::Release);
}

fn again_continue() -> k1::Exit {
    sched::account::entering();
    k1::continue_()
}
#[inline(always)]
fn again_resume(value: u64) -> k1::Exit {
    sched::account::entering();
    k1::resume(value)
}
fn again_enter(context: &Context) -> k1::Exit {
    sched::account::entering();
    k1::enter_with(context, ferrix_native::region::ENTER_COPY_MISS_EXITS)
}

#[cold]
#[inline(always)]
const fn unusual() {}

// SAFETY: (LINK) linuxd's composition root defines each with these signatures, and they
// take nothing the caller has not checked.
unsafe extern "Rust" {
    /// linuxd's dispatch, by name, for LTO: no indirect call on a program's every call.
    safe fn ferrix_lift_dispatch(args: &SyscallArgs, regs: Option<&UserRegs>) -> Outcome;
    /// Whether the way back has anything to do for the running thread.
    safe fn ferrix_lift_attention() -> bool;
    /// Whether no filter or probe asks about the running thread.
    safe fn ferrix_lift_quiet() -> bool;
    /// A call `linuxd` answers straight through (`docs/microkernel/PERFORMANCE.md`), by
    /// name so that LTO can take it in line: `u64::MAX` for the general path.
    safe fn ferrix_lift_hot_answer(number: usize) -> u64;
}

/// Every taken branch here runs cold after the exit's barrier: one straight run, back into
/// the program for as long as the answer is this simple, and on anything unusual out to
/// the general path. Its time counts as the program's, as the kernel's fast crossing's does.
#[unsafe(link_section = ".text.ferrix_straight")]
#[inline(never)]
fn straight_run(mut exit: k1::Exit) -> k1::Exit {
    loop {
        let value = ferrix_lift_hot_answer(exit.number as usize);
        let plain = sched::with_current(|task| !task.pending_up.load(Ordering::Relaxed));
        if (exit.reason != Reason::Syscall as u64) | (value == u64::MAX) | (plain != Some(true)) {
            return exit;
        }
        exit = k1::resume(value);
    }
}

fn run(mut exit: k1::Exit) -> ! {
    loop {
        exit = straight_run(exit);
        sched::account::left_program();
        exit = match exit.reason() {
            Ok(Reason::Syscall) | Ok(Reason::SyscallCompat) => {
                let abi = if matches!(exit.reason(), Ok(Reason::SyscallCompat)) {
                    Abi::Compat
                } else {
                    Abi::Native
                };
                let number = exit.number as usize;
                let _ = sched::with_current(|task| task.note_call(exit.number, &exit.words));
                let needs = needs_context(abi, number);
                let context = needs.then(read_context);
                let args = SyscallArgs {
                    abi,
                    number,
                    args: exit.words,
                    ip: context.as_ref().map_or(0, program_counter),
                };
                let regs = context.as_ref().map(|context| regs_of(to_trap(context)));
                match context
                    .as_ref()
                    .and_then(|context| answer_early(&args, context))
                {
                    Some(next) => next,
                    None => match (if ferrix_lift_quiet() {
                        None
                    } else {
                        trap::filter_system_call(&args)
                    })
                    .unwrap_or_else(|| ferrix_lift_dispatch(&args, regs.as_ref()))
                    {
                        Outcome::Return(value) => back(value as u64, false),
                        Outcome::ReturnWords { value, words } => {
                            let mut context = context.unwrap_or_else(read_context);
                            set_answer_words(&mut context, value as u64, words);
                            again_enter(&context)
                        }
                        Outcome::Enter { entry, stack, abi } => {
                            // A new program: nothing the old one's `pending` held goes on.
                            if let Some(task) = sched::current() {
                                let _ = task.pending.lock().take();
                            }
                            let context = fresh(entry, stack, 0, abi);
                            again_enter(&context)
                        }
                    },
                }
            }
            Ok(Reason::Fault) => fault(&exit),
            Ok(Reason::Kick) => back(0, true),
            Err(why) => {
                crate::println!("linuxd: a restricted entry was refused: {why:?}");
                crate::panic::end_thread();
            }
        };
    }
}

#[cfg(target_arch = "x86_64")]
const fn regs_of(frame: TrapFrame) -> UserRegs {
    UserRegs::Trap(frame)
}
#[cfg(target_arch = "aarch64")]
const fn regs_of(frame: TrapFrame) -> UserRegs {
    UserRegs(frame)
}
/// A native call's answer with its words, where the native ABI has them.
#[cfg(target_arch = "x86_64")]
fn set_answer_words(context: &mut Context, value: u64, words: [u64; 3]) {
    context.rax = value;
    context.rsi = words[0];
    context.rdx = words[1];
    context.r10 = words[2];
}
#[cfg(target_arch = "aarch64")]
fn set_answer_words(context: &mut Context, value: u64, words: [u64; 3]) {
    context.x[0] = value;
    context.x[1] = words[0];
    context.x[2] = words[1];
    context.x[3] = words[2];
}

/// `arch_prctl` and `rt_sigreturn`, which the kernel's entry answers before
/// dispatch for the same reasons: each changes registers the dispatch does not hold.
fn answer_early(args: &SyscallArgs, context: &Context) -> Option<k1::Exit> {
    use ferrix_linux_abi::nr::Syscall;
    if args.abi != Abi::Native {
        return None;
    }
    match crate::arch::decode_syscall(args.number)? {
        #[cfg(target_arch = "x86_64")]
        Syscall::ArchPrctl => {
            const ARCH_SET_GS: u64 = 0x1001;
            const ARCH_SET_FS: u64 = 0x1002;
            let (code, base) = (args.args[0], args.args[1]);
            if code != ARCH_SET_FS && code != ARCH_SET_GS {
                return None;
            }
            if !ferrix_bootinfo::is_user_address(base) {
                return Some(again_resume(
                    ferrix_linux_abi::errno::Errno::EPERM.as_return_value() as u64,
                ));
            }
            let mut context = *context;
            if code == ARCH_SET_FS {
                context.fs_base = base;
            } else {
                context.gs_base = base;
            }
            context.rax = 0;
            Some(again_enter(&context))
        }
        Syscall::RtSigreturn => {
            let path = trap::return_path()?;
            let task = sched::current()?;
            let mut user = UserContext::from_trap(&to_trap(context));
            *task.pending.lock() = Some(*context);
            task.pending_up.store(true, Ordering::Relaxed);
            // Only `restore_fpu` captures in a sigreturn, for the mask alone.
            task.sigreturning.store(true, Ordering::Relaxed);
            (path.sigreturn)(&mut user, true);
            task.sigreturning.store(false, Ordering::Relaxed);
            task.pending_up.store(false, Ordering::Relaxed);
            let mut after = task.pending.lock().take().unwrap_or(*context);
            let mut frame = to_trap(&after);
            user.store_trap(&mut frame);
            from_trap(&frame, &mut after);
            Some(again_enter(&after))
        }
        _ => None,
    }
}

/// A fault: the shadow may hold what the kernel's cache lacks (M5-LIFT.md §3);
/// `AddressSpace::fault` may make it so; otherwise it is the program's.
fn fault(exit: &k1::Exit) -> k1::Exit {
    let vector = exit.number;
    let code = exit.words[ferrix_native_abi::restricted::FAULT_CODE];
    let (is_page_fault, write, execute) = page_fault_of(vector, code);
    let address = exit.words[ferrix_native_abi::restricted::FAULT_ADDRESS];
    let mut kind = exit.words[ferrix_native_abi::restricted::FAULT_KIND];
    let pc = exit.words[ferrix_native_abi::restricted::EXIT_PC];
    if is_page_fault {
        let access = crate::user::space::Access { write, execute };
        // The kernel's window has no regions, so only the shadow's answer says why: a page
        // past a file's end is the program's SIGBUS.
        use crate::user::space::SpaceError;
        match crate::fill::resolve(address, access) {
            // Nothing of the program changed: back at the instruction, no read.
            Ok(()) => return back(0, true),
            Err(Some(SpaceError::PastEnd(_) | SpaceError::Unreadable(_))) => {
                kind = ferrix_native_abi::restricted::FaultKind::PastEnd as u64;
            }
            Err(Some(SpaceError::Refused(_))) => {
                kind = ferrix_native_abi::restricted::FaultKind::Protection as u64;
            }
            Err(_) => {}
        }
        if kind == ferrix_native_abi::restricted::FaultKind::CallCopy as u64 {
            // A bad pointer in a native call: its status, never a signal (P4).
            with_pending(|context| step_past_call(context));
            return back(
                ferrix_native_abi::status::FAULT.as_return_value() as u64,
                false,
            );
        }
    }
    let (signal, si_code) = signal_for(vector, kind);
    if let Some(path) = trap::return_path() {
        match (path.fault_signal)(signal, si_code, address, pc) {
            trap::FaultOutcome::Delivered => {}
            trap::FaultOutcome::Ended { pid } => {
                crate::println!(
                    "  signal   pid {pid} ended by signal {signal} at {address:#x}, pc {pc:#x}"
                );
            }
            trap::FaultOutcome::NoProcess => crate::panic::end_thread(),
        }
    }
    back(0, true)
}

#[cfg(target_arch = "x86_64")]
const fn step_past_call(context: &mut Context) {
    context.rip += SYSCALL_BYTES;
}
#[cfg(target_arch = "aarch64")]
const fn step_past_call(context: &mut Context) {
    context.pc += SYSCALL_BYTES;
}
/// Whether this fault's vector is a translation's, and what it asked.
#[cfg(target_arch = "x86_64")]
const fn page_fault_of(vector: u64, code: u64) -> (bool, bool, bool) {
    const PF_WRITE: u64 = 1 << 1;
    const PF_FETCH: u64 = 1 << 4;
    (vector == 14, code & PF_WRITE != 0, code & PF_FETCH != 0)
}
/// The exception class, and the syndrome: `WnR` of a data abort; an instruction abort
/// is a fetch.
#[cfg(target_arch = "aarch64")]
const fn page_fault_of(vector: u64, code: u64) -> (bool, bool, bool) {
    const DATA_ABORT_LOWER: u64 = 0b100100;
    const INSTRUCTION_ABORT_LOWER: u64 = 0b100000;
    const WNR: u64 = 1 << 6;
    let data = vector == DATA_ABORT_LOWER;
    (
        data || vector == INSTRUCTION_ABORT_LOWER,
        data && code & WNR != 0,
        vector == INSTRUCTION_ABORT_LOWER,
    )
}

#[cfg(target_arch = "aarch64")]
fn signal_for(vector: u64, kind: u64) -> (u32, i32) {
    use ferrix_linux_abi::types::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};
    match vector {
        0b100100 | 0b100000 => match kind {
            2 => (SIGSEGV, 2),
            3 => (SIGBUS, 2),
            4 => (SIGBUS, 4),
            _ => (SIGSEGV, 1),
        },
        0b100010 | 0b100110 => (SIGBUS, 1),
        0b101100 => (SIGFPE, 128),
        0b111100 => (SIGTRAP, 1),
        _ => (SIGILL, 1),
    }
}

#[cfg(target_arch = "x86_64")]
fn signal_for(vector: u64, kind: u64) -> (u32, i32) {
    use ferrix_linux_abi::types::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};
    match vector {
        0 => (SIGFPE, 1),
        1 | 3 => (SIGTRAP, 1),
        6 => (SIGILL, 2),
        14 => match kind {
            2 => (SIGSEGV, 2),
            3 => (SIGBUS, 2),
            4 => (SIGBUS, 4),
            _ => (SIGSEGV, 1),
        },
        16 | 19 => (SIGFPE, 0),
        _ => (SIGSEGV, 128),
    }
}

/// A thread's first entry into a program.
pub fn enter(entry: u64, stack: u64, argument: u64, abi: Abi) -> ! {
    let context = fresh(entry, stack, argument, abi);
    prepare_first();
    run(again_enter(&context))
}
/// A fork child's first entry: its parent's registers, from the system call.
#[cfg(target_arch = "aarch64")]
pub fn resume(regs: &UserRegs) -> ! {
    let mut context = Context::default();
    from_trap(&regs.0, &mut context);
    apply_state(&mut context);
    prepare_first();
    run(again_enter(&context))
}
#[cfg(target_arch = "x86_64")]
/// A fork child's first entry: its parent's registers, from the system call.
pub fn resume(regs: &UserRegs) -> ! {
    let mut context = Context::default();
    let frame = match regs {
        UserRegs::Trap(frame) => *frame,
        UserRegs::Syscall(_) => panic!("the shim's regs are always a trap frame"),
    };
    from_trap(&frame, &mut context);
    if context.cs == u64::from(USER_CODE32 | 3) {
        context.ds = u64::from(USER_DATA | 3);
        context.es = u64::from(USER_DATA | 3);
    } else {
        context.ds = 0;
        context.es = 0;
    }
    apply_state(&mut context);
    prepare_first();
    run(again_enter(&context))
}
/// What `spawn_user`'s state says about the first entry.
fn apply_state(context: &mut Context) {
    let task = sched::current().expect("a program's thread has a task");
    if let Some(state) = task.user_state.lock().as_ref() {
        state.write_context(context);
    }
}
fn prepare_first() {
    let task = sched::current().expect("a program's thread has a task");
    let state = task.user_state.lock().take();
    if let Some(state) = state {
        load_vectors(&state);
    }
    sched::account::starts_program(&task);
    task.in_restricted.store(true, Ordering::Release);
}

/// What K1 holds of this thread, as a record.
#[cfg(target_arch = "aarch64")]
pub fn capture_state() -> UserState {
    let context = with_pending(|context| *context);
    let mut state = UserState::new();
    state.read_context(&context);
    let mut image = [0_u8; 528];
    if ferrix_native::region::vectors_read(Kernel, &mut image).is_ok() {
        let _ = state.set_vector_image(&image);
    }
    state
}
/// Load `state` as `rt_sigreturn` does: the vectors now, the thread pointer at the next entry.
#[cfg(target_arch = "aarch64")]
pub fn load_state(state: &UserState) {
    with_pending(|context| state.write_context(context));
    load_vectors(state);
}
#[cfg(target_arch = "x86_64")]
pub fn capture_state() -> UserState {
    let context = with_pending(|context| *context);
    let mut state = UserState::new();
    state.read_context(&context);
    // `restore_fpu` captures only to learn `MXCSR_MASK` and to have an area, every byte
    // of which it overwrites from the frame. A kernel call of 1,450 cycles is not
    // needed for that: the mask is the processor's, read once.
    let at = MXCSR_MASK_AT..MXCSR_MASK_AT + 4;
    let known = MXCSR_MASK.load(Ordering::Relaxed);
    let in_sigreturn = sched::with_current(|task| task.sigreturning.load(Ordering::Relaxed));
    if in_sigreturn == Some(true) && known != 0 {
        if let Some(word) = state.image_mut().get_mut(at) {
            word.copy_from_slice(&(known as u32).to_le_bytes());
        }
        return state;
    }
    let _ = ferrix_native::region::vectors_read(Kernel, state.image_mut());
    if let Some(&[a, b, c, d]) = state
        .image()
        .get(at)
        .and_then(|word| <&[u8; 4]>::try_from(word).ok())
    {
        MXCSR_MASK.store(
            u64::from(u32::from_le_bytes([a, b, c, d])),
            Ordering::Relaxed,
        );
    }
    state
}
pub fn load_vectors(state: &UserState) {
    let _ = ferrix_native::region::vectors_write(Kernel, as_bytes(&state.image()));
}
#[cfg(target_arch = "aarch64")]
const fn as_bytes(image: &[u8; 528]) -> &[u8] {
    image
}
#[cfg(target_arch = "x86_64")]
const fn as_bytes(image: &[u8]) -> &[u8] {
    image
}
#[cfg(target_arch = "aarch64")]
pub fn reset_state() {
    with_pending(|context| context.tpidr = 0);
    let _ = ferrix_native::region::vectors_write(Kernel, &UserState::new().image());
}
#[cfg(target_arch = "x86_64")]
pub fn reset_state() {
    with_pending(|context| {
        context.fs_base = 0;
        context.gs_base = 0;
        context.tls = [0; 3];
        context.ds = 0;
        context.es = 0;
        context.fs = 0;
        context.gs = 0;
    });
    let _ = ferrix_native::region::vectors_write(Kernel, UserState::new().image());
}
#[cfg(target_arch = "x86_64")]
pub fn set_thread_area(index: Option<usize>, descriptor: u64) -> Option<usize> {
    with_pending(|context| {
        let at = index.or_else(|| context.tls.iter().position(|&slot| slot == 0))?;
        *context.tls.get_mut(at)? = descriptor;
        Some(at)
    })
}
#[cfg(target_arch = "x86_64")]
pub fn thread_area(index: usize) -> Option<u64> {
    with_pending(|context| context.tls.get(index).copied())
}
#[cfg(target_arch = "x86_64")]
pub fn selectors() -> [u16; 4] {
    with_pending(|context| {
        [
            context.ds as u16,
            context.es as u16,
            context.fs as u16,
            context.gs as u16,
        ]
    })
}
#[cfg(target_arch = "x86_64")]
pub fn set_selectors(selectors: [u16; 4]) {
    with_pending(|context| {
        context.ds = u64::from(selectors[0]);
        context.es = u64::from(selectors[1]);
        context.fs = u64::from(selectors[2]);
        context.gs = u64::from(selectors[3]);
    });
}
#[cfg(target_arch = "x86_64")]
pub fn set_compat() {
    with_pending(|context| {
        context.cs = u64::from(USER_CODE32 | 3);
        context.ds = u64::from(USER_DATA | 3);
        context.es = u64::from(USER_DATA | 3);
    });
}
