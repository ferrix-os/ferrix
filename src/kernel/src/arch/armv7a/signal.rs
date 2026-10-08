//! Signal frames on ARMv7-A: two of them.
//!
//! Linux's `struct sigframe` and `struct rt_sigframe` from
//! `arch/arm/kernel/signal.h`, the layouts QEMU's `linux-user/arm/signal.c`
//! writes. A handler installed with `SA_SIGINFO` gets the second, and returns
//! through `rt_sigreturn`; one without gets the first, with no `siginfo`, and
//! returns through `sigreturn`. musl and glibc both choose the restorer that
//! way, so both frames are needed.
//!
//! | Offset | `sigframe` | `rt_sigframe` |
//! |---|---|---|
//! | 0 | `struct ucontext`, 744 bytes | `struct siginfo`, 128 bytes |
//! | 744 / 128 | `retcode[4]` | `struct sigframe` |
//!
//! In `struct ucontext`: flags, link, `uc_stack` at 8, `uc_mcontext` at 20 --
//! trap number, error code, old mask, `r0`-`r10`, `fp`, `ip`, `sp`, `lr`, `pc`,
//! `cpsr`, fault address -- `uc_sigmask` at 104, and `uc_regspace` at 232,
//! where the VFP record goes: magic, size, `d0`-`d31`, `fpscr`, `fpexc`,
//! `fpinst`, `fpinst2`, 288 bytes, then a zero word.
//!
//! USR mode's stack pointer and link register are banked rather than in the
//! trap frame, so [`UserContext`] carries them beside it.

use ferrix_linux_abi::types::{SA_RESTORER, SA_SIGINFO};

use super::cpu;
use super::switch::{self, UserState};
use super::trap::{TrapFrame, USER_CPSR};
use crate::signal_frame::{BadFrame, FrameBytes, FrameRequest, Restored, SIGINFO_BYTES};
use crate::user::space::AddressSpace;

/// No red zone in the AAPCS.
pub(crate) const SIGNAL_RED_ZONE: u64 = 0;

/// Bytes in `struct ucontext`.
const UCONTEXT_BYTES: usize = 744;
/// Where `uc_stack` is, in the `ucontext`.
const UC_STACK: usize = 8;
/// Where `uc_mcontext.oldmask` is.
const OLDMASK: usize = 28;
/// Where `uc_mcontext.arm_r0` is; `r1` to `r12` follow it.
const R0: usize = 32;
/// Where `uc_mcontext.arm_sp` is.
const SP: usize = 84;
/// Where `uc_mcontext.arm_lr` is.
const LR: usize = 88;
/// Where `uc_mcontext.arm_pc` is.
const PC: usize = 92;
/// Where `uc_mcontext.arm_cpsr` is.
const CPSR: usize = 96;
/// Where `uc_sigmask` is.
const UC_SIGMASK: usize = 104;
/// Where `uc_regspace` is.
const REGSPACE: usize = 232;
/// Where `retcode` is, after the `ucontext`.
const RETCODE: usize = UCONTEXT_BYTES;
/// Bytes in `struct sigframe`: the `ucontext` and four words of `retcode`.
const SIGFRAME_BYTES: usize = UCONTEXT_BYTES + 16;

/// `uc_flags` in a frame with no `siginfo`: a value `trap_no` never has, by
/// which Linux once told the two layouts apart.
const UC_FLAGS_PLAIN: u32 = 0x5ac3_c35a;

/// `VFP_MAGIC`.
const VFP_MAGIC: u32 = 0x5646_5001;
/// Bytes in `struct vfp_sigframe`, which is eight-byte aligned.
const VFP_BYTES: usize = 288;
/// `FPEXC.EN`, as a frame reports the exception register.
const FPEXC_ENABLED: u32 = 1 << 30;

/// `sigreturn`'s number, for the `retcode` a frame carries.
const NR_SIGRETURN: u32 = 119;
/// `rt_sigreturn`'s.
const NR_RT_SIGRETURN: u32 = 173;

/// `CPSR.T`.
const CPSR_THUMB: u32 = 1 << 5;
/// The IT block state, which a handler starts outside of.
const CPSR_IT: u32 = 0x0600_FC00;
/// What a frame may give back: `NZCVQ`, the IT state, `GE` and the Thumb bit.
/// Never the mode, and never the interrupt masks.
const CPSR_RESTORABLE: u32 = 0xF800_0000 | CPSR_IT | 0x000F_0000 | CPSR_THUMB;

/// A program's registers as the way back to USR mode will load them: the trap
/// frame, and the banked stack pointer and link register.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UserContext {
    /// Everything the trap path saved.
    frame: TrapFrame,
    /// USR mode's stack pointer.
    sp: u32,
    /// USR mode's link register.
    lr: u32,
}

impl UserContext {
    /// The registers a trap from USR mode saved, with the banked two read from
    /// the processor.
    pub(crate) fn from_trap(frame: &TrapFrame) -> UserContext {
        let (sp, lr) = switch::user_banked();
        UserContext {
            frame: *frame,
            sp,
            lr,
        }
    }

    /// Put them back: the frame where the trap path restores it from, and the
    /// banked two onto the processor.
    pub(crate) fn store_trap(&self, frame: &mut TrapFrame) {
        *frame = self.frame;
        switch::set_user_banked(self.sp, self.lr);
    }

    /// The program's stack pointer.
    pub(crate) fn stack_pointer(&self) -> u64 {
        u64::from(self.sp)
    }

    /// The value in the return register (`r0`), read as a signed result: what
    /// a system call left there, which the restart logic inspects for a
    /// kernel-internal restart code. Sign-extended from 32 bits, so `-512`
    /// reads as `-512` and not as a large positive.
    pub(crate) fn syscall_result(&self) -> isize {
        self.frame.r[0] as i32 as isize
    }

    /// Overwrite the return register (`r0`) with `value`: how the restart
    /// logic turns a restart code into `EINTR` for a call that will not be
    /// restarted.
    pub(crate) fn set_syscall_result(&mut self, value: isize) {
        self.frame.r[0] = value as u32;
    }

    /// Rewind so the interrupted `svc #0` re-executes when the program
    /// resumes, as Linux's `do_signal` does. The instruction sits at `PC - 4`
    /// in ARM state and `PC - 2` in Thumb, and `r0` carried both the first
    /// argument and the result, so it is restored to `orig_arg0`. `r7`, the
    /// number, was never clobbered, so only a `restart_block` resume touches
    /// it -- to point the call at `restart_syscall`.
    pub(crate) fn rewind_syscall(&mut self, orig_nr: u64, orig_arg0: u64, restart_block: bool) {
        let _ = orig_nr;
        self.frame.r[0] = orig_arg0 as u32;
        if restart_block {
            self.frame.r[7] = ferrix_linux_abi::nr::arm::RESTART_SYSCALL as u32;
        }
        let back = if self.frame.cpsr & CPSR_THUMB != 0 {
            2
        } else {
            4
        };
        self.frame.pc = self.frame.pc.wrapping_sub(back);
    }
}

/// The signal return page's code (`syscall::sigpage`), as Linux's
/// `arch/arm/kernel/sigreturn_codes.S` lays it out, a word each: `sigreturn`
/// in ARM state (`mov r7, #119`, `svc`) at word 0 and in Thumb (`movs r7,
/// #119`, `svc #0`) at word 2, then `rt_sigreturn` the same way at words 3
/// and 5. [`return_offset`] picks one.
const SIGPAGE_CODE: [u8; 24] = {
    const ARM_SIGRETURN: [u32; 2] = [0xe3a0_7000 | NR_SIGRETURN, 0xef00_0000 | NR_SIGRETURN];
    const ARM_RT: [u32; 2] = [0xe3a0_7000 | NR_RT_SIGRETURN, 0xef00_0000 | NR_RT_SIGRETURN];
    let [arm_sigreturn_mov, arm_sigreturn_svc] = ARM_SIGRETURN;
    let [arm_rt_mov, arm_rt_svc] = ARM_RT;
    // Two Thumb halfwords in one little-endian word: `movs r7, #n` first.
    const THUMB_SIGRETURN: u32 = 0xdf00_0000 | 0x2700 | NR_SIGRETURN;
    const THUMB_RT: u32 = 0xdf00_0000 | 0x2700 | NR_RT_SIGRETURN;
    let [a, b, c, d] = arm_sigreturn_mov.to_le_bytes();
    let [e, f, g, h] = arm_sigreturn_svc.to_le_bytes();
    let [i, j, k, l] = THUMB_SIGRETURN.to_le_bytes();
    let [m, n, o, p] = arm_rt_mov.to_le_bytes();
    let [q, r, s, t] = arm_rt_svc.to_le_bytes();
    let [u, v, w, x] = THUMB_RT.to_le_bytes();
    [
        a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u, v, w, x,
    ]
};

/// The signal return page's code, for `syscall::sigpage` to build it from.
pub(crate) fn sigpage_code() -> &'static [u8] {
    &SIGPAGE_CODE
}

/// Where in the signal return page a handler returns to: the sequence for
/// its frame (`rt_sigreturn` for an `rt_sigframe`) in its own instruction
/// set, with the Thumb bit set for a Thumb one, as Linux's `setup_return`
/// computes `retcode`.
const fn return_offset(rt: bool, thumb: bool) -> u32 {
    let word = if thumb { 2 } else { 0 } + if rt { 3 } else { 0 };
    word * 4 + if thumb { 1 } else { 0 }
}

/// Where the handler returns to: its restorer if it has one, and otherwise
/// the signal return page's sequence for its frame, as Linux's sigpage
/// (F-48). A space the page could not be mapped into gets `copy`, the frame's
/// own, which only an executable stack could run: the handler then faults on
/// its return, as every one did before the page.
fn return_address(
    request: &FrameRequest,
    rt: bool,
    thumb: bool,
    copy: u32,
) -> Result<u32, BadFrame> {
    if request.flags & SA_RESTORER != 0 {
        return u32::try_from(request.restorer).map_err(|_| BadFrame);
    }
    Ok(request
        .sigpage
        .and_then(|page| page.checked_add(u64::from(return_offset(rt, thumb))))
        .and_then(|at| u32::try_from(at).ok())
        .unwrap_or(copy))
}

/// Write the `sigreturn` trampoline a handler without a restorer returns
/// into: `mov r7, #number` and `svc 0`. Written into the frame as Linux
/// writes it, for an unwinder reading the frame; the handler returns through
/// the signal return page's copy, since no stack page may run.
fn write_retcode(frame: &mut FrameBytes, uc: usize, rt: bool) -> Result<(), BadFrame> {
    let number = if rt { NR_RT_SIGRETURN } else { NR_SIGRETURN };
    frame.put_u32(uc + RETCODE, 0xe3a0_7000 | number)?;
    frame.put_u32(uc + RETCODE + 4, 0xef00_0000 | number)
}

/// Write `request`'s frame -- `rt_sigframe` for an `SA_SIGINFO` handler,
/// `sigframe` otherwise -- and point `context` at the handler, in Thumb state
/// if its address says so: `r0` the signal, and for the first `r1` the
/// `siginfo` and `r2` the `ucontext`; `lr` the restorer.
pub(crate) fn setup_signal_frame(
    space: &AddressSpace,
    context: &mut UserContext,
    request: &FrameRequest,
) -> Result<(), BadFrame> {
    let rt = request.flags & SA_SIGINFO != 0;
    let uc = if rt { SIGINFO_BYTES } else { 0 };
    let size = uc + SIGFRAME_BYTES;
    let frame_at = request.stack.checked_sub(size as u64).ok_or(BadFrame)? & !7;
    let frame_at = u32::try_from(frame_at).map_err(|_| BadFrame)?;

    let mut frame = FrameBytes::zeroed(size)?;
    if rt {
        frame.put(0, &request.info)?;
    } else {
        frame.put_u32(uc, UC_FLAGS_PLAIN)?;
    }
    frame.put_stack(uc + UC_STACK, request.altstack)?;
    write_registers(&mut frame, uc, context)?;
    frame.put_u32(uc + OLDMASK, request.mask as u32)?;
    frame.put_u64(uc + UC_SIGMASK, request.mask)?;
    write_vfp(&mut frame, uc + REGSPACE)?;
    write_retcode(&mut frame, uc, rt)?;
    frame.write(space, u64::from(frame_at))?;

    let handler = u32::try_from(request.handler).map_err(|_| BadFrame)?;
    let thumb = handler & 1 != 0;
    let return_to = return_address(request, rt, thumb, frame_at + (uc + RETCODE) as u32)?;
    let regs = &mut context.frame;
    let arguments = [
        (0, request.signal),
        (1, if rt { frame_at } else { regs.r[1] }),
        (2, if rt { frame_at + uc as u32 } else { regs.r[2] }),
    ];
    for (index, value) in arguments {
        if let Some(slot) = regs.r.get_mut(index) {
            *slot = value;
        }
    }
    regs.pc = if thumb { handler & !1 } else { handler & !3 };
    regs.cpsr = regs.cpsr & !(CPSR_IT | CPSR_THUMB) | if thumb { CPSR_THUMB } else { 0 };
    context.sp = frame_at;
    context.lr = return_to;
    Ok(())
}

/// Write `uc_mcontext`'s registers for the `ucontext` at `uc`.
fn write_registers(
    frame: &mut FrameBytes,
    uc: usize,
    context: &UserContext,
) -> Result<(), BadFrame> {
    for (index, value) in context.frame.r.iter().enumerate() {
        frame.put_u32(uc + R0 + index * 4, *value)?;
    }
    frame.put_u32(uc + SP, context.sp)?;
    frame.put_u32(uc + LR, context.lr)?;
    frame.put_u32(uc + PC, context.frame.pc)?;
    frame.put_u32(uc + CPSR, context.frame.cpsr)
}

/// Write the VFP record at `at`, if this core has a VFP; the zero word that
/// ends the record list is already there either way.
fn write_vfp(frame: &mut FrameBytes, at: usize) -> Result<(), BadFrame> {
    if cpu::user_fpu_doubles() == 0 {
        return Ok(());
    }
    // SAFETY: (CONTEXT) on the running task's own way back to USR mode, so the processor
    // holds this program's floating-point registers.
    let state = unsafe { UserState::capture() };
    let (fpscr, doubles) = state.fp();
    frame.put_u32(at, VFP_MAGIC)?;
    frame.put_u32(at + 4, VFP_BYTES as u32)?;
    for (index, value) in doubles.iter().enumerate() {
        frame.put_u64(at + 8 + index * 8, *value)?;
    }
    frame.put_u32(at + 264, fpscr)?;
    frame.put_u32(at + 272, FPEXC_ENABLED)
}

/// Read the frame at the stack pointer a handler returned with -- a
/// `rt_sigframe` when `rt`, a `sigframe` otherwise -- and load `context`
/// from it.
///
/// Refused: a stack pointer not eight-byte aligned, which no frame this wrote
/// has, and on a core with a VFP a record without its magic and size. The
/// status register keeps only what [`CPSR_RESTORABLE`] allows and gets USR
/// mode's, so a frame cannot hand a program a privileged mode.
pub(crate) fn restore_signal_frame(
    space: &AddressSpace,
    context: &mut UserContext,
    rt: bool,
) -> Result<Restored, BadFrame> {
    if context.sp & 7 != 0 {
        return Err(BadFrame);
    }
    let uc = if rt { SIGINFO_BYTES } else { 0 };
    let frame = FrameBytes::read(space, u64::from(context.sp), uc + UCONTEXT_BYTES)?;
    let mut r = [0_u32; 13];
    for (index, slot) in r.iter_mut().enumerate() {
        *slot = frame.u32_at(uc + R0 + index * 4)?;
    }
    let restored = Restored {
        mask: frame.u64_at(uc + UC_SIGMASK)?,
        altstack: Some(frame.stack_at(uc + UC_STACK)?),
    };
    restore_vfp(&frame, uc + REGSPACE)?;
    context.frame.r = r;
    context.frame.pc = frame.u32_at(uc + PC)?;
    context.frame.cpsr = frame.u32_at(uc + CPSR)? & CPSR_RESTORABLE | USER_CPSR;
    context.sp = frame.u32_at(uc + SP)?;
    context.lr = frame.u32_at(uc + LR)?;
    Ok(restored)
}

/// Load the floating-point registers from the VFP record at `at`, if this
/// core has a VFP.
fn restore_vfp(frame: &FrameBytes, at: usize) -> Result<(), BadFrame> {
    if cpu::user_fpu_doubles() == 0 {
        return Ok(());
    }
    if frame.u32_at(at)? != VFP_MAGIC || frame.u32_at(at + 4)? as usize != VFP_BYTES {
        return Err(BadFrame);
    }
    let mut doubles = [0_u64; 32];
    for (index, slot) in doubles.iter_mut().enumerate() {
        *slot = frame.u64_at(at + 8 + index * 8)?;
    }
    // SAFETY: (CONTEXT) inside the running task's own system call, so the registers are
    // its own; captured to carry the new values in the layout the load expects.
    let mut state = unsafe { UserState::capture() };
    state.set_fp(frame.u32_at(at + 264)?, doubles);
    // SAFETY: (CONTEXT) the running task's own registers.
    unsafe { switch::load_user_fpu(&state) };
    Ok(())
}
