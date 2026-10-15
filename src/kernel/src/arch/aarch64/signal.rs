//! Signal frames on `AArch64`.
//!
//! Linux's `struct rt_sigframe` from `arch/arm64/kernel/signal.c`, the layout
//! QEMU's `linux-user/aarch64/signal.c` writes:
//!
//! | Offset | Field |
//! |---|---|
//! | 0 | `struct siginfo`, 128 bytes |
//! | 128 | `struct ucontext`: flags, link, `uc_stack`, `uc_sigmask`, 120 unused bytes |
//! | 304 | `uc_mcontext`, 16-byte aligned: fault address, `x0`-`x30`, `sp`, `pc`, `pstate` |
//! | 592 | `__reserved`, 4096 bytes of records: an `fpsimd_context`, then an end record |
//!
//! 4688 bytes, and above it a frame record -- the interrupted `x29` and `x30`
//! -- that the handler's `x29` points at, so a backtrace from inside a handler
//! walks on into the code it interrupted.
//!
//! The handler returns to `x30`, the restorer. Linux points it at the vDSO's
//! `__kernel_rt_sigreturn` when the handler has no `SA_RESTORER`, and so does
//! Ferrix: signal delivery hands this the vDSO's trampoline as the restorer
//! then (`syscall::vdso::sigreturn`). musl sets `SA_RESTORER` for every
//! handler; glibc on AArch64 never does, having no trampoline of its own, and
//! before the vDSO had one every glibc handler returned to address zero.

use ferrix_linux_abi::types::SA_RESTORER;

use super::switch::{self, UserState};
use super::trap::{TrapFrame, USER_SPSR};
use crate::signal_frame::{BadFrame, FrameBytes, FrameRequest, Restored};
use crate::user::space::AddressSpace;

/// No red zone in the AAPCS64: nothing below the stack pointer is the
/// program's.
pub const SIGNAL_RED_ZONE: u64 = 0;

/// Where `struct ucontext` starts.
const UC: usize = 128;
/// Where `uc_stack` is.
const UC_STACK: usize = UC + 16;
/// Where `uc_sigmask` is.
const UC_SIGMASK: usize = UC + 40;
/// Where `uc_mcontext` is, after 120 unused bytes and alignment to sixteen.
const MCONTEXT: usize = UC + 176;
/// Where `regs[0]` is.
const REGS: usize = MCONTEXT + 8;
/// Where `sp` is.
const SP: usize = MCONTEXT + 256;
/// Where `pc` is.
const PC: usize = MCONTEXT + 264;
/// Where `pstate` is.
const PSTATE: usize = MCONTEXT + 272;
/// Where `__reserved` is, 16-byte aligned.
const RESERVED: usize = MCONTEXT + 288;
/// Bytes in the frame.
const FRAME_BYTES: usize = RESERVED + 4096;

/// `FPSIMD_MAGIC`.
const FPSIMD_MAGIC: u32 = 0x4650_8001;
/// Bytes in `struct fpsimd_context`: header, `fpsr`, `fpcr`, 32 128-bit registers.
const FPSIMD_BYTES: usize = 528;
/// Bytes in the frame record above the frame.
const RECORD_BYTES: u64 = 16;

/// The condition flags, which are all of `pstate` a program may set.
const PSTATE_NZCV: u64 = 0xF000_0000;

/// A program's registers as the way back to EL0 will load them.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct UserContext(TrapFrame);

impl UserContext {
    /// The registers a trap from EL0 saved.
    pub const fn from_trap(frame: &TrapFrame) -> UserContext {
        UserContext(*frame)
    }

    /// Put them back where the trap path restores them from.
    pub(crate) const fn store_trap(&self, frame: &mut TrapFrame) {
        *frame = self.0;
    }

    /// The program's stack pointer, `SP_EL0`.
    pub const fn stack_pointer(&self) -> u64 {
        self.0.sp
    }

    /// The value in the return register (`x0`), read as a signed result: what
    /// a system call left there, which the restart logic inspects for a
    /// kernel-internal restart code.
    pub const fn syscall_result(&self) -> isize {
        self.0.x[0] as isize
    }

    /// Overwrite the return register (`x0`) with `value`: how the restart
    /// logic turns a restart code into `EINTR` for a call that will not be
    /// restarted.
    pub const fn set_syscall_result(&mut self, value: isize) {
        self.0.x[0] = value as u64;
    }

    /// Rewind so the interrupted `svc #0` re-executes when the program
    /// resumes, as Linux's `arch_do_signal_or_restart` does. The instruction
    /// sits at `PC - 4`, and `x0` carried both the first argument and the
    /// result, so it is restored to `orig_arg0`. `x8`, the number, was never
    /// clobbered, so only a `restart_block` resume touches it -- to point the
    /// call at `restart_syscall`.
    pub const fn rewind_syscall(
        &mut self,
        orig_nr: u64,
        orig_arg0: u64,
        restart_block: bool,
    ) {
        let _ = orig_nr;
        self.0.x[0] = orig_arg0;
        if restart_block {
            self.0.x[8] = ferrix_linux_abi::nr::aarch64::RESTART_SYSCALL as u64;
        }
        self.0.elr = self.0.elr.wrapping_sub(4);
    }
}

/// Write the frame record the handler's frame pointer names: the
/// interrupted frame pointer and link register.
fn write_frame_record(
    space: &AddressSpace,
    at: u64,
    frame_pointer: Option<&u64>,
    link: Option<&u64>,
) -> Result<(), BadFrame> {
    let mut record = FrameBytes::zeroed(RECORD_BYTES as usize)?;
    record.put_u64(0, frame_pointer.copied().unwrap_or(0))?;
    record.put_u64(8, link.copied().unwrap_or(0))?;
    record.write(space, at)
}

/// Write `request`'s frame and point `context` at the handler: `x0` the
/// signal, `x1` the `siginfo`, `x2` the `ucontext`, `x29` the frame record,
/// `x30` the restorer.
pub fn setup_signal_frame(
    space: &AddressSpace,
    context: &mut UserContext,
    request: &FrameRequest,
) -> Result<(), BadFrame> {
    let record_at = request.stack.checked_sub(RECORD_BYTES).ok_or(BadFrame)? & !15;
    let frame_at = record_at.checked_sub(FRAME_BYTES as u64).ok_or(BadFrame)? & !15;
    let regs = &context.0;

    let mut frame = FrameBytes::zeroed(FRAME_BYTES)?;
    frame.put(0, &request.info)?;
    frame.put_stack(UC_STACK, request.altstack)?;
    frame.put_u64(UC_SIGMASK, request.mask)?;
    for (index, value) in regs.x.iter().enumerate() {
        frame.put_u64(REGS + index * 8, *value)?;
    }
    frame.put_u64(SP, regs.sp)?;
    frame.put_u64(PC, regs.elr)?;
    frame.put_u64(PSTATE, regs.spsr)?;

    // SAFETY: (CONTEXT) on the running task's own way back to EL0, so the processor
    // holds this program's floating-point and SIMD registers.
    let state = unsafe { UserState::capture() };
    let (fpcr, fpsr) = state.fp_control();
    frame.put_u32(RESERVED, FPSIMD_MAGIC)?;
    frame.put_u32(RESERVED + 4, FPSIMD_BYTES as u32)?;
    frame.put_u32(RESERVED + 8, fpsr as u32)?;
    frame.put_u32(RESERVED + 12, fpcr as u32)?;
    frame.put(RESERVED + 16, state.vectors())?;
    // The end record, eight zero bytes after the last one, is already zero.
    frame.write(space, frame_at)?;

    write_frame_record(space, record_at, regs.x.get(29), regs.x.get(30))?;

    let restorer = if request.flags & SA_RESTORER == 0 {
        0
    } else {
        request.restorer
    };
    let regs = &mut context.0;
    let values = [
        (0, u64::from(request.signal)),
        (1, frame_at),
        (2, frame_at + UC as u64),
        (29, record_at),
        (30, restorer),
    ];
    for (index, value) in values {
        if let Some(slot) = regs.x.get_mut(index) {
            *slot = value;
        }
    }
    regs.sp = frame_at;
    regs.elr = request.handler;
    Ok(())
}

/// Read the frame at the stack pointer a handler returned with and load
/// `context` from it.
///
/// Refused: a stack pointer not 16-byte aligned, which no frame this wrote
/// has; a record list that is not one `fpsimd_context` and an end record,
/// since nothing here writes any other. `pstate` keeps only the condition
/// flags, so a frame cannot hand a program another exception level or its
/// interrupts masked.
pub fn restore_signal_frame(
    space: &AddressSpace,
    context: &mut UserContext,
    rt: bool,
) -> Result<Restored, BadFrame> {
    let _ = rt;
    let frame_at = context.0.sp;
    if frame_at & 15 != 0 {
        return Err(BadFrame);
    }
    let frame = FrameBytes::read(space, frame_at, FRAME_BYTES)?;
    let fpsimd = find_fpsimd(&frame)?;

    let mut x = [0_u64; 31];
    for (index, slot) in x.iter_mut().enumerate() {
        *slot = frame.u64_at(REGS + index * 8)?;
    }
    let restored = Restored {
        mask: frame.u64_at(UC_SIGMASK)?,
        altstack: Some(frame.stack_at(UC_STACK)?),
    };
    let mut vectors = [0_u8; 512];
    vectors.copy_from_slice(frame.get(fpsimd + 16, 512)?);
    // SAFETY: (CONTEXT) inside the running task's own system call, so the registers are
    // its own; captured to keep the thread pointer as it is.
    let mut state = unsafe { UserState::capture() };
    state.set_fp(
        u64::from(frame.u32_at(fpsimd + 12)?),
        u64::from(frame.u32_at(fpsimd + 8)?),
        vectors,
    );
    // SAFETY: (CONTEXT) the running task's own registers, loaded from a state whose
    // thread pointer is the one it already has; the entry stack is unused
    // on this architecture.
    unsafe { switch::restore_user_state(&state, 0) };

    let regs = &mut context.0;
    regs.x = x;
    regs.sp = frame.u64_at(SP)?;
    regs.elr = frame.u64_at(PC)?;
    regs.spsr = frame.u64_at(PSTATE)? & PSTATE_NZCV | USER_SPSR;
    Ok(restored)
}

/// The offset of the one `fpsimd_context` in the frame's records.
fn find_fpsimd(frame: &FrameBytes) -> Result<usize, BadFrame> {
    let magic = frame.u32_at(RESERVED)?;
    let size = frame.u32_at(RESERVED + 4)?;
    if magic != FPSIMD_MAGIC || size as usize != FPSIMD_BYTES {
        return Err(BadFrame);
    }
    let end = RESERVED + FPSIMD_BYTES;
    if frame.u32_at(end)? != 0 || frame.u32_at(end + 4)? != 0 {
        return Err(BadFrame);
    }
    Ok(RESERVED)
}
