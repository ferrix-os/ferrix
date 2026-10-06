//! Signal frames on x86-64, and the way back to ring 3 that restores every
//! register.
//!
//! # The frame
//!
//! Linux's `struct rt_sigframe` from `arch/x86/include/asm/sigframe.h`, the
//! same layout QEMU's `linux-user/i386/signal.c` writes:
//!
//! | Offset | Field |
//! |---|---|
//! | 0 | `pretcode`: the restorer, which the handler's `ret` pops |
//! | 8 | `struct ucontext`: flags, link, `uc_stack`, `uc_mcontext` (256 bytes), `uc_sigmask` |
//! | 312 | `struct siginfo`, 128 bytes |
//!
//! and below it, 64-byte aligned, the 512-byte `FXSAVE` area `uc_mcontext.fpstate`
//! points at -- the first part of an 832-byte `XSAVE` area when the program
//! may use AVX, as Linux lays one out: its software-reserved bytes say so,
//! the header and the upper halves of the `YMM` registers follow, then
//! `FP_XSTATE_MAGIC2`, and `uc_flags` has `UC_FP_XSTATE`. The vDSO holds no way back from a handler -- Linux's x86-64 one
//! holds none either -- and there is no other, so a handler installed without
//! `SA_RESTORER` cannot be entered: Linux refuses the frame, and so does this.
//! Every libc sets it.
//!
//! # Why `rt_sigreturn` does not leave through `SYSRET`
//!
//! `SYSRET` loads the instruction pointer from `RCX` and the flags from `R11`,
//! so a program resumed through it gets those two registers back as the
//! address and the flags rather than as what the interrupted code held in
//! them. A handler can interrupt code anywhere, with `RCX` live, so the return
//! from one leaves through the trap stub's `IRETQ` path instead, which restores
//! all sixteen. Linux does the same.

mod compat;

use ferrix_bootinfo::is_user_address;
use ferrix_linux_abi::types::SA_RESTORER;

use super::cpu;
use super::gdt;
use super::switch::{self, UserState};
use super::syscall::SyscallFrame;
use super::trap::TrapFrame;
use crate::signal_frame::{BadFrame, FrameBytes, FrameRequest, Restored, SIGINFO_BYTES};
use crate::user::space::AddressSpace;

/// Bytes below the stack pointer the System V ABI lets a leaf function use
/// without moving it, which a frame must not overwrite.
pub(crate) const SIGNAL_RED_ZONE: u64 = 128;

/// Where `struct ucontext` starts in the frame.
const UC: usize = 8;
/// Where `uc_stack` is.
const UC_STACK: usize = UC + 16;
/// Where `uc_mcontext`, `struct sigcontext`, is.
const MCONTEXT: usize = UC + 40;
/// Where `uc_sigmask` is.
const UC_SIGMASK: usize = UC + 296;
/// Where the `siginfo` is.
const INFO: usize = UC + 304;
/// Bytes in the frame.
const FRAME_BYTES: usize = INFO + SIGINFO_BYTES;

/// `sigcontext.eflags`, relative to `uc_mcontext`. The seventeen general
/// registers come before it, in [`UserContext::registers`]'s order.
const EFLAGS: usize = 136;
/// `sigcontext.cs`, a 16-bit selector.
const CS: usize = 144;
/// `sigcontext.ss`, a 16-bit selector where Linux used to have padding.
const SS: usize = 150;
/// `sigcontext.oldmask`: the saved mask's first word, for old readers.
const OLDMASK: usize = 168;
/// `sigcontext.fpstate`: where the `FXSAVE` area is, or zero for none.
const FPSTATE: usize = 184;

/// `UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS`: the frame records `SS`, and a
/// return restores it. What Linux writes on a machine without `XSAVE` in use.
const UC_FLAGS: u64 = 0x2 | 0x4;

/// Bytes in the `FXSAVE` area.
const FXSAVE_BYTES: usize = 512;
/// Where `MXCSR` is in the area.
const MXCSR: usize = 24;
/// Where `MXCSR_MASK` is: which `MXCSR` bits this processor has.
const MXCSR_MASK: usize = 28;
/// Where the software-reserved bytes' first magic word is. Zero says the area
/// is plain `FXSAVE`, with no extended state after it; [`FP_XSTATE_MAGIC1`]
/// says an `XSAVE` area follows, as Linux's `struct _fpx_sw_bytes` has it.
const SW_MAGIC1: usize = 464;
/// `_fpx_sw_bytes.extended_size`: the area's bytes with the second magic word.
const SW_EXTENDED_SIZE: usize = 468;
/// `_fpx_sw_bytes.xfeatures`: the components the area has room for.
const SW_XFEATURES: usize = 472;
/// `_fpx_sw_bytes.xstate_size`: the `XSAVE` area's bytes.
const SW_XSTATE_SIZE: usize = 480;
/// Linux's `FP_XSTATE_MAGIC1`.
const FP_XSTATE_MAGIC1: u32 = 0x4650_5853;
/// Linux's `FP_XSTATE_MAGIC2`, the word just past the `XSAVE` area.
const FP_XSTATE_MAGIC2: u32 = 0x4650_5845;
/// Where `XSTATE_BV` is: the header's first word.
const XSTATE_BV: usize = 512;
/// Where the upper halves of the `YMM` registers are, in the standard form.
const AVX_AT: usize = 576;
/// Bytes of the `XSAVE` area a frame carries, for x87, SSE and AVX.
const XSTATE_BYTES: usize = cpu::XSAVE_AREA_BYTES as usize;
/// `UC_FP_XSTATE`: `fpstate` is an `XSAVE` area, not `FXSAVE` alone.
const UC_FP_XSTATE: u64 = 0x1;
/// The mask to assume when a processor reports none: Intel's documented
/// default, every bit but `DAZ`.
const DEFAULT_MXCSR_MASK: u32 = 0xFFBF;

/// The flags a frame may give back: carry, parity, adjust, zero, sign,
/// direction, overflow and alignment check -- Linux's `FIX_EFLAGS` without the
/// trap and resume flags, since a trap flag set from a frame would single-step
/// the program into a kernel that has no `#DB` handler for it. Never `IOPL`,
/// never interrupts off.
const FLAGS_RESTORABLE: u64 =
    (1 << 0) | (1 << 2) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 18);
/// The flags every program runs with: interrupts on, and the reserved bit
/// that always reads as one.
const FLAGS_ALWAYS: u64 = 0x202;
/// The flags a handler starts with cleared: trap, direction and resume, as on
/// Linux, so a handler runs its string instructions forwards.
const FLAGS_CLEARED_FOR_HANDLER: u64 = (1 << 8) | (1 << 10) | (1 << 16);

/// A program's registers as the way back to ring 3 will load them: the whole
/// trap frame, whichever way the program came in.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub(crate) struct UserContext(TrapFrame);

impl UserContext {
    /// The registers a trap from ring 3 saved.
    pub(crate) const fn from_trap(frame: &TrapFrame) -> UserContext {
        UserContext(*frame)
    }

    /// Put them back where the trap stub restores them from.
    pub(crate) const fn store_trap(&self, frame: &mut TrapFrame) {
        *frame = self.0;
    }

    /// The program's stack pointer.
    pub(crate) const fn stack_pointer(&self) -> u64 {
        self.0.rsp
    }

    /// The value in the return register, read as a signed result: what a
    /// system call left there, which the restart logic inspects for a
    /// kernel-internal restart code.
    pub(crate) const fn syscall_result(&self) -> isize {
        self.0.rax as isize
    }

    /// Overwrite the return register with `value`: how the restart logic turns
    /// a restart code into `EINTR` for a call that will not be restarted.
    pub(crate) const fn set_syscall_result(&mut self, value: isize) {
        self.0.rax = value as u64;
    }

    /// Rewind so the interrupted `syscall` re-executes when the program
    /// resumes, as Linux's `arch_do_signal_or_restart` does. The two-byte
    /// `syscall` opcode sits at `RIP - 2`, and `RAX` carried both the number
    /// and the result, so it is restored to the number the call is re-entered
    /// with -- `restart_syscall`'s number for a `restart_block` resume, the
    /// original number otherwise. The System V argument registers were never
    /// clobbered, so `orig_arg0` is not needed here.
    ///
    /// A call through `int $0x80` rewinds the same way -- its opcode is two
    /// bytes too -- and restarts through i386's `restart_syscall`, which is
    /// the table that call was made in (`docs/I386.md` §3.2). `orig_nr` is
    /// already that table's number, being the one the program passed.
    pub(crate) const fn rewind_syscall(
        &mut self,
        orig_nr: u64,
        orig_arg0: u64,
        restart_block: bool,
    ) {
        let _ = orig_arg0;
        self.0.rax = match (
            restart_block,
            self.0.vector == super::trap::LEGACY_SYSCALL_VECTOR,
        ) {
            (true, true) => ferrix_linux_abi::nr::i386::RESTART_SYSCALL as u64,
            (true, false) => ferrix_linux_abi::nr::x86_64::RESTART_SYSCALL as u64,
            (false, _) => orig_nr,
        };
        self.0.rip = self.0.rip.wrapping_sub(2);
    }

    /// A program about to run from its first instruction, as `execve` leaves
    /// it: every register clear, interrupts open, and the selectors of the
    /// mode `abi` names -- 32-bit user code for an i386 image.
    pub(super) const fn entering(entry: u64, stack: u64, abi: crate::trap::Abi) -> UserContext {
        let (cs, ss) = match abi {
            crate::trap::Abi::Native => (USER_CS, USER_SS),
            crate::trap::Abi::Compat => (USER_CS32, USER_SS),
        };
        UserContext(TrapFrame {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rbp: 0,
            rdi: 0,
            rsi: 0,
            rdx: 0,
            rcx: 0,
            rbx: 0,
            rax: 0,
            vector: 0,
            error_code: 0,
            rip: entry,
            cs,
            rflags: FLAGS_ALWAYS,
            rsp: stack,
            ss,
        })
    }

    /// The same registers with only what ring 3 may hold in the selectors and
    /// flags: user code (64- or 32-bit, as they were) and user data, and the
    /// flags a frame may give back (certification review, T.ESCALATE path 8).
    /// For a fork child resumed from its parent's trap frame, which the
    /// processor saved and so already holds nothing else; checked all the
    /// same, as a frame read back from a program is.
    pub(super) const fn sanitised(mut self) -> UserContext {
        if self.0.cs != USER_CS {
            self.0.cs = USER_CS32;
        }
        self.0.ss = USER_SS;
        self.0.rflags = self.0.rflags & FLAGS_RESTORABLE | FLAGS_ALWAYS;
        self
    }

    /// True if these registers are a 32-bit program's: its code selector is
    /// compatibility mode's.
    pub(crate) const fn is_compat(&self) -> bool {
        self.0.cs == USER_CS32
    }

    /// The registers a system call saved. `SYSCALL` put the return address in
    /// `RCX` and the flags in `R11`, so those two are also what the program
    /// holds in them when it resumes.
    pub(super) const fn from_syscall(frame: &SyscallFrame) -> UserContext {
        UserContext(TrapFrame {
            r15: frame.r15,
            r14: frame.r14,
            r13: frame.r13,
            r12: frame.r12,
            r11: frame.r11,
            r10: frame.r10,
            r9: frame.r9,
            r8: frame.r8,
            rbp: frame.rbp,
            rdi: frame.rdi,
            rsi: frame.rsi,
            rdx: frame.rdx,
            rcx: frame.rcx,
            rbx: frame.rbx,
            rax: frame.rax,
            vector: 0,
            error_code: 0,
            rip: frame.rcx,
            cs: USER_CS,
            rflags: frame.r11,
            rsp: frame.user_rsp,
            ss: USER_SS,
        })
    }

    /// Put them back where the `SYSRET` path restores them from. That path
    /// takes the address from `RCX` and the flags from `R11`, so those are
    /// what goes there: right for entering a handler, whose `RCX` and `R11`
    /// hold nothing, and never used to resume an interrupted context.
    pub(super) const fn store_syscall(&self, frame: &mut SyscallFrame) {
        let regs = &self.0;
        frame.r15 = regs.r15;
        frame.r14 = regs.r14;
        frame.r13 = regs.r13;
        frame.r12 = regs.r12;
        frame.rbp = regs.rbp;
        frame.rbx = regs.rbx;
        frame.r9 = regs.r9;
        frame.r8 = regs.r8;
        frame.r10 = regs.r10;
        frame.rdx = regs.rdx;
        frame.rsi = regs.rsi;
        frame.rdi = regs.rdi;
        frame.rax = regs.rax;
        frame.r11 = regs.rflags;
        frame.rcx = regs.rip;
        frame.user_rsp = regs.rsp;
    }

    /// The seventeen registers `struct sigcontext` opens with, in its order.
    const fn registers(&self) -> [u64; 17] {
        let r = &self.0;
        [
            r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15, r.rdi, r.rsi, r.rbp, r.rbx,
            r.rdx, r.rax, r.rcx, r.rsp, r.rip,
        ]
    }

    /// Load the seventeen from a `struct sigcontext`'s order.
    const fn set_registers(&mut self, values: [u64; 17]) {
        let [
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
            rdi,
            rsi,
            rbp,
            rbx,
            rdx,
            rax,
            rcx,
            rsp,
            rip,
        ] = values;
        let r = &mut self.0;
        r.r8 = r8;
        r.r9 = r9;
        r.r10 = r10;
        r.r11 = r11;
        r.r12 = r12;
        r.r13 = r13;
        r.r14 = r14;
        r.r15 = r15;
        r.rdi = rdi;
        r.rsi = rsi;
        r.rbp = rbp;
        r.rbx = rbx;
        r.rdx = rdx;
        r.rax = rax;
        r.rcx = rcx;
        r.rsp = rsp;
        r.rip = rip;
    }
}

/// Ring 3's code selector, with its requested privilege level.
const USER_CS: u64 = (gdt::USER_CODE | 3) as u64;
/// Ring 3's 32-bit code selector, likewise: compatibility mode's.
const USER_CS32: u64 = (gdt::USER_CODE32 | 3) as u64;

/// Ring 3's stack selector, likewise.
const USER_SS: u64 = (gdt::USER_DATA | 3) as u64;

/// `uc_flags`: [`UC_FLAGS`], and [`UC_FP_XSTATE`] when the frame carries an
/// `XSAVE` area.
fn uc_flags() -> u64 {
    if frame_has_xstate() {
        UC_FLAGS | UC_FP_XSTATE
    } else {
        UC_FLAGS
    }
}

/// Whether a frame carries an `XSAVE` area: when the switch saves AVX, so
/// that a handler which uses it does not hand the interrupted code back
/// different `YMM` registers.
fn frame_has_xstate() -> bool {
    cpu::extended_state_components() & cpu::XSTATE_AVX != 0
}

/// Bytes the floating-point area takes below the frame: `FXSAVE`'s alone, or
/// the `XSAVE` area and the magic word after it.
fn fp_area_bytes() -> usize {
    if frame_has_xstate() {
        XSTATE_BYTES + 4
    } else {
        FXSAVE_BYTES
    }
}

/// Write the program's x87 and SSE state, as `FXSAVE` lays it out, to
/// `fpstate` -- and its AVX state after it, as `XSAVE` does, with the
/// software-reserved words that say so, when the switch saves AVX.
fn write_fp_area(space: &AddressSpace, fpstate: u64) -> Result<(), BadFrame> {
    // SAFETY: (CONTEXT) on the running task's own way back to ring 3, so the processor
    // holds this program's x87, SSE and AVX registers.
    let state = unsafe { UserState::capture() };
    fp_area(&state)?.write(space, fpstate)
}

/// [`write_fp_area`]'s bytes for `state`: the `FXSAVE` image, and the rest of
/// the `XSAVE` area after it when the switch saves AVX. A 32-bit frame
/// carries the same bytes after its `fsave` environment (`compat`).
fn fp_area(state: &UserState) -> Result<FrameBytes, BadFrame> {
    let mut area = FrameBytes::zeroed(fp_area_bytes())?;
    area.put(0, &state.fxsave())?;
    if frame_has_xstate() {
        area.put_u32(SW_MAGIC1, FP_XSTATE_MAGIC1)?;
        area.put_u32(SW_EXTENDED_SIZE, (XSTATE_BYTES + 4) as u32)?;
        area.put_u64(SW_XFEATURES, cpu::extended_state_components())?;
        area.put_u32(SW_XSTATE_SIZE, XSTATE_BYTES as u32)?;
        area.put_u64(XSTATE_BV, state.xstate_bv())?;
        area.put(AVX_AT, &state.avx())?;
        area.put_u32(XSTATE_BYTES, FP_XSTATE_MAGIC2)?;
    } else {
        area.put_u32(SW_MAGIC1, 0)?;
    }
    Ok(area)
}

/// Whether a frame can be built for `request`: the handler must name a
/// restorer, because neither mode has a default one to return through here
/// -- there is no vDSO holding one -- and every libc gives one.
const fn frame_is_buildable(request: &FrameRequest) -> Result<(), BadFrame> {
    if request.flags & SA_RESTORER == 0 {
        return Err(BadFrame);
    }
    Ok(())
}

/// Where the floating-point area and the frame go below `stack`: the area
/// 64-byte aligned, and the frame below it aligned so that on entry
/// `(sp + 8) % 16 == 0`, which is what a function expects just after a
/// `call` pushed its return address. Answers both addresses.
fn place(stack: u64) -> Result<(u64, u64), BadFrame> {
    let fpstate = stack.checked_sub(fp_area_bytes() as u64).ok_or(BadFrame)? & !63;
    let below = fpstate.checked_sub(FRAME_BYTES as u64).ok_or(BadFrame)?;
    let frame_at = ((below + 8) & !15).checked_sub(8).ok_or(BadFrame)?;
    Ok((fpstate, frame_at))
}

/// Write `request`'s frame below its stack and point `context` at the handler:
/// `RDI` the signal, `RSI` the `siginfo`, `RDX` the `ucontext`, and the stack
/// pointer at `pretcode`, as if the handler had just been called from it.
pub(crate) fn setup_signal_frame(
    space: &AddressSpace,
    context: &mut UserContext,
    request: &FrameRequest,
) -> Result<(), BadFrame> {
    frame_is_buildable(request)?;
    // A 32-bit program's handler is entered on an i386 frame, in its mode.
    if context.is_compat() {
        return compat::setup(space, context, request);
    }
    let (fpstate, frame_at) = place(request.stack)?;
    write_fp_area(space, fpstate)?;

    let mut frame = FrameBytes::zeroed(FRAME_BYTES)?;
    frame.put_u64(0, request.restorer)?;
    frame.put_u64(UC, uc_flags())?;
    frame.put_stack(UC_STACK, request.altstack)?;
    for (index, value) in context.registers().iter().enumerate() {
        frame.put_u64(MCONTEXT + index * 8, *value)?;
    }
    frame.put_u64(MCONTEXT + EFLAGS, context.0.rflags)?;
    frame.put(MCONTEXT + CS, &(gdt::USER_CODE | 3).to_le_bytes())?;
    frame.put(MCONTEXT + SS, &(gdt::USER_DATA | 3).to_le_bytes())?;
    frame.put_u64(MCONTEXT + OLDMASK, request.mask)?;
    frame.put_u64(MCONTEXT + FPSTATE, fpstate)?;
    frame.put_u64(UC_SIGMASK, request.mask)?;
    frame.put(INFO, &request.info)?;
    frame.write(space, frame_at)?;

    let regs = &mut context.0;
    regs.rdi = u64::from(request.signal);
    regs.rsi = frame_at + INFO as u64;
    regs.rdx = frame_at + UC as u64;
    regs.rax = 0;
    regs.rsp = frame_at;
    regs.rip = request.handler;
    regs.rflags &= !FLAGS_CLEARED_FOR_HANDLER;
    regs.cs = USER_CS;
    regs.ss = USER_SS;
    Ok(())
}

/// Read the frame a handler returned through -- its restorer's `ret` left the
/// stack pointer just past `pretcode` -- and load `context` from it.
///
/// Refused: an instruction pointer or stack pointer outside the user half,
/// which `IRETQ` would fault on in ring 0. The flags are masked to what a
/// program may set, the selectors are ring 3's whatever the frame says, and
/// `MXCSR` is masked to the bits the processor has before `FXRSTOR64` sees it.
pub(crate) fn restore_signal_frame(
    space: &AddressSpace,
    context: &mut UserContext,
    rt: bool,
) -> Result<Restored, BadFrame> {
    // A 32-bit program returns through an i386 frame, of the kind `rt` says.
    if context.is_compat() {
        return compat::restore(space, context, rt);
    }
    let _ = rt;
    let frame_at = context.0.rsp.checked_sub(8).ok_or(BadFrame)?;
    let frame = FrameBytes::read(space, frame_at, INFO)?;
    let mut registers = [0_u64; 17];
    for (index, slot) in registers.iter_mut().enumerate() {
        *slot = frame.u64_at(MCONTEXT + index * 8)?;
    }
    let [.., rsp, rip] = registers;
    if !is_user_address(rip) || !is_user_address(rsp) {
        return Err(BadFrame);
    }
    let flags = frame.u64_at(MCONTEXT + EFLAGS)?;
    let fpstate = frame.u64_at(MCONTEXT + FPSTATE)?;
    let restored = Restored {
        mask: frame.u64_at(UC_SIGMASK)?,
        altstack: Some(frame.stack_at(UC_STACK)?),
    };
    if fpstate != 0 {
        restore_fpu(space, fpstate)?;
    }
    context.set_registers(registers);
    context.0.rflags = flags & FLAGS_RESTORABLE | FLAGS_ALWAYS;
    context.0.cs = USER_CS;
    context.0.ss = USER_SS;
    Ok(restored)
}

/// The AVX state an area at `at` carries, if it is a whole `XSAVE` area as
/// [`write_fp_area`] writes one: both magic words, the sizes it was written
/// with. `None` for plain `FXSAVE` -- a frame from before, or one a program
/// built -- after which AVX is loaded in its initial state, as Linux does.
fn xstate_in(space: &AddressSpace, at: u64, legacy: &FrameBytes) -> Option<(u64, [u8; 256])> {
    if !frame_has_xstate()
        || legacy.u32_at(SW_MAGIC1).ok()? != FP_XSTATE_MAGIC1
        || legacy.u32_at(SW_XSTATE_SIZE).ok()? as usize != XSTATE_BYTES
        || legacy.u32_at(SW_EXTENDED_SIZE).ok()? as usize != XSTATE_BYTES + 4
    {
        return None;
    }
    let area = FrameBytes::read(space, at, XSTATE_BYTES + 4).ok()?;
    if area.u32_at(XSTATE_BYTES).ok()? != FP_XSTATE_MAGIC2 {
        return None;
    }
    let avx = area.get(AVX_AT, 256).ok()?.first_chunk::<256>().copied()?;
    Some((area.u64_at(XSTATE_BV).ok()?, avx))
}

/// Load the x87 and SSE registers from the `FXSAVE` area at `at`, and the
/// AVX registers from the `XSAVE` area it may begin.
fn restore_fpu(space: &AddressSpace, at: u64) -> Result<(), BadFrame> {
    let area = FrameBytes::read(space, at, FXSAVE_BYTES)?;
    // SAFETY: (CONTEXT) inside the running task's own system call, so the registers are
    // its own; captured only to learn `MXCSR_MASK` and to carry the area.
    let mut state = unsafe { UserState::capture() };
    let mut live = FrameBytes::zeroed(FXSAVE_BYTES)?;
    live.put(0, &state.fxsave())?;
    let mask = match live.u32_at(MXCSR_MASK)? {
        0 => DEFAULT_MXCSR_MASK,
        mask => mask,
    };
    let mut wanted = FrameBytes::zeroed(FXSAVE_BYTES)?;
    wanted.put(0, area.get(0, FXSAVE_BYTES)?)?;
    wanted.put_u32(MXCSR, area.u32_at(MXCSR)? & mask)?;
    state
        .fxsave_mut()
        .copy_from_slice(wanted.get(0, FXSAVE_BYTES)?);
    // x87 and SSE from the legacy area whatever else there is; AVX from the
    // frame when it carries it, its initial state when it does not.
    let (components, avx) = xstate_in(space, at, &area).unwrap_or((0, [0; 256]));
    state.set_xstate_bv(components & cpu::XSTATE_AVX | cpu::XSTATE_X87_SSE);
    *state.avx_mut() = avx;
    // SAFETY: (CONTEXT) the registers are the running task's, `MXCSR` was masked to
    // the bits this processor reports, and `XSTATE_BV` names only enabled
    // components, so neither `FXRSTOR64` nor `XRSTOR64` has anything to refuse.
    unsafe { switch::load_fpu(&state) };
    Ok(())
}

unsafe extern "C" {
    /// Load `frame` onto the stack pointer and leave through the trap stub's
    /// restore path: every register, and `IRETQ`.
    fn ferrix_resume_trap_frame(frame: *const TrapFrame) -> !;
}

/// Resume ring 3 from `context` through `IRETQ`, restoring every register.
/// Does not return.
///
/// # Safety
///
/// (CONTEXT) Must be called by a user task with its address space and user state loaded,
/// from its own kernel stack, with nothing owned left on the stack above, and
/// with `context`'s selectors ring 3's and its instruction and stack pointers
/// user addresses -- which [`restore_signal_frame`] and
/// [`setup_signal_frame`] leave true.
pub(super) unsafe fn resume_context(context: &UserContext) -> ! {
    // SAFETY: (CONTEXT) the caller's guarantee is the assembly's contract.
    unsafe { ferrix_resume_trap_frame(core::ptr::from_ref(&context.0)) }
}
