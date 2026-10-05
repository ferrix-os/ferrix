//! A 32-bit program's signal frames: `rt_sigframe_ia32` for a handler with
//! `SA_SIGINFO` and `sigframe_ia32` for one without, as Linux's
//! `ia32_setup_rt_frame` and `ia32_setup_frame` build them, and their way back
//! through `rt_sigreturn` and `sigreturn` (`docs/I386.md` I2b). The layouts are
//! `ferrix_linux_abi::sigframe32`'s.
//!
//! # Where everything goes
//!
//! Below the stack the generic code chose, the 512-byte `FXSAVE` image,
//! 64-byte aligned, with the 112-byte `fsave` environment of `_fpstate_32`
//! just below it -- `fpu__alloc_mathframe` for an ia32 frame -- and below
//! that the frame, placed so that `sp + 4` is 16-byte aligned, as a function
//! entered by `call` expects (`align_sigframe`'s ia32 case).
//!
//! # What is left out
//!
//! The `fsave` environment carries the control and status words and the
//! `FXSR` magic, not the converted x87 tag and registers `convert_from_fxsr`
//! writes: neither libc's handlers read them, and the return restores from
//! the `FXSAVE` image, which holds all of it. A handler without
//! `SA_RESTORER` is refused, as for a 64-bit one: there is no 32-bit vDSO
//! holding Linux's default restorer, and both libcs always give one.

use ferrix_linux_abi::sigframe32::{
    self, FPSTATE_MAGIC, FPSTATE_SIZE, FPSTATE_STATUS, FXSAVE_AT, RETCODE, RT_RETCODE,
    SIGINFO_SIZE, frame, rt_frame, sigcontext, stack, ucontext,
};
use ferrix_linux_abi::types::SA_SIGINFO;

use super::super::{cpu, gdt, switch};
use super::{
    FLAGS_ALWAYS, FLAGS_CLEARED_FOR_HANDLER, FLAGS_RESTORABLE, FXSAVE_BYTES, USER_CS32, USER_SS,
    UserContext, restore_fpu,
};
use crate::signal_frame::{BadFrame, FrameBytes, FrameRequest, Restored, StackRecord};
use crate::user::space::AddressSpace;

/// The first address past a 32-bit program's reach.
const FOUR_GIB: u64 = 1 << 32;

/// Write `request`'s frame below its stack and point `context` at the
/// handler: `sig` in `EAX`, and for an `SA_SIGINFO` frame the `siginfo` and
/// `ucontext` in `EDX` and `ECX`, as Linux's `-mregparm=3` convention has it,
/// with the same three on the stack after `pretcode` for everyone else.
pub(super) fn setup(
    space: &AddressSpace,
    context: &mut UserContext,
    request: &FrameRequest,
) -> Result<(), BadFrame> {
    let rt = request.flags & SA_SIGINFO != 0;
    let (fpstate, at) = place(request.stack, rt)?;
    write_fpstate(space, fpstate)?;
    let mcontext = sigcontext_of(context, word(fpstate)?, request.mask as u32)?;
    let bytes = if rt {
        rt_frame_bytes(request, at, &mcontext)?
    } else {
        plain_frame_bytes(request, &mcontext)?
    };
    bytes.write(space, at)?;
    enter_handler(context, request, at, rt);
    Ok(())
}

/// A frame's address as a 32-bit program's word: every one of them is below
/// 4 GiB, since the stack the frame goes below is.
fn word(value: u64) -> Result<u32, BadFrame> {
    u32::try_from(value).map_err(|_| BadFrame)
}

/// Where the floating-point state and the frame go below `stack`: the
/// `FXSAVE` image 64-byte aligned, the `fsave` environment just below it,
/// and the frame below that with `sp + 4` 16-byte aligned. Answers the
/// `_fpstate_32`'s address and the frame's.
fn place(stack: u64, rt: bool) -> Result<(u64, u64), BadFrame> {
    if stack > FOUR_GIB {
        return Err(BadFrame);
    }
    let fxsave = stack.checked_sub(FXSAVE_BYTES as u64).ok_or(BadFrame)? & !63;
    let fpstate = fxsave.checked_sub(FXSAVE_AT as u64).ok_or(BadFrame)?;
    let size = if rt { rt_frame::SIZE } else { frame::SIZE };
    let below = fpstate.checked_sub(size as u64).ok_or(BadFrame)?;
    let at = ((below + 4) & !15).checked_sub(4).ok_or(BadFrame)?;
    Ok((fpstate, at))
}

/// `rt_sigframe_ia32`, to be written at `at`.
fn rt_frame_bytes(
    request: &FrameRequest,
    at: u64,
    mcontext: &FrameBytes,
) -> Result<FrameBytes, BadFrame> {
    let uc = rt_frame::UC;
    let mut bytes = FrameBytes::zeroed(rt_frame::SIZE)?;
    for (offset, value) in [
        (rt_frame::PRETCODE, word(request.restorer)?),
        (rt_frame::SIG, request.signal),
        (rt_frame::PINFO, word(at + rt_frame::INFO as u64)?),
        (rt_frame::PUC, word(at + uc as u64)?),
    ] {
        bytes.put_u32(offset, value)?;
    }
    bytes.put(rt_frame::INFO, &sigframe32::siginfo_from_64(&request.info))?;
    put_stack(&mut bytes, uc + ucontext::STACK, request.altstack)?;
    bytes.put(uc + ucontext::MCONTEXT, mcontext.get(0, sigcontext::SIZE)?)?;
    bytes.put_u64(uc + ucontext::SIGMASK, request.mask)?;
    bytes.put(rt_frame::RETCODE, &RT_RETCODE)?;
    Ok(bytes)
}

/// `sigframe_ia32`: the mask's low word is the `sigcontext`'s `oldmask`,
/// its high word `extramask`.
fn plain_frame_bytes(
    request: &FrameRequest,
    mcontext: &FrameBytes,
) -> Result<FrameBytes, BadFrame> {
    let mut bytes = FrameBytes::zeroed(frame::SIZE)?;
    for (offset, value) in [
        (frame::PRETCODE, word(request.restorer)?),
        (frame::SIG, request.signal),
        (frame::EXTRAMASK, (request.mask >> 32) as u32),
    ] {
        bytes.put_u32(offset, value)?;
    }
    bytes.put(frame::SC, mcontext.get(0, sigcontext::SIZE)?)?;
    bytes.put(frame::RETCODE, &RETCODE)?;
    Ok(bytes)
}

/// Point `context` at the handler, with the frame at `at` its stack.
fn enter_handler(context: &mut UserContext, request: &FrameRequest, at: u64, rt: bool) {
    let regs = &mut context.0;
    regs.rax = u64::from(request.signal);
    (regs.rdx, regs.rcx) = if rt {
        (at + rt_frame::INFO as u64, at + rt_frame::UC as u64)
    } else {
        (0, 0)
    };
    regs.rsp = at;
    regs.rip = request.handler;
    regs.rflags &= !FLAGS_CLEARED_FOR_HANDLER;
    regs.cs = USER_CS32;
    regs.ss = USER_SS;
    // Linux loads user data into `DS` and `ES` for the handler, whatever the
    // interrupted code had there; `FS` and `GS`, the thread pointer, stay.
    let [_, _, fs, gs] = cpu::read_data_selectors();
    let data = gdt::USER_DATA | 3;
    switch::load_program_selectors([data, data, fs, gs]);
}

/// Take back the registers, mask and -- for `rt_sigreturn` -- alternate
/// stack the frame the handler returned from saved, `rt` saying which frame.
///
/// The handler's `ret` popped `pretcode`, and the restorer, `sigreturn`'s,
/// also popped `sig`, so the frame is four bytes below the stack pointer for
/// the one and eight for the other.
///
/// A program may write anything into its frame, so everything is read and
/// checked before anything is loaded (certification review, T.ESCALATE path
/// 8): see [`Saved::check`]. A forged frame ends the program with `SIGSEGV`
/// with nothing of it loaded. `EIP` and `ESP` are 32-bit fields, so they are
/// canonical, and user addresses, whatever they hold: the classic `IRETQ`
/// `#GP` on a non-canonical return address cannot arise from here.
pub(super) fn restore(
    space: &AddressSpace,
    context: &mut UserContext,
    rt: bool,
) -> Result<Restored, BadFrame> {
    let esp = context.0.rsp & 0xFFFF_FFFF;
    let (below, size, sc) = if rt {
        (4, rt_frame::SIZE, rt_frame::UC + ucontext::MCONTEXT)
    } else {
        (8, frame::SIZE, frame::SC)
    };
    let at = esp.checked_sub(below).ok_or(BadFrame)?;
    let bytes = FrameBytes::read(space, at, size)?;
    let saved = Saved::read(&bytes, sc)?;
    saved.check()?;
    let restored = restored_of(&bytes, sc, rt)?;
    // The floating-point state is the one part that can still be refused --
    // an unreadable area -- so it goes first, and a refusal there too leaves
    // the registers as they were.
    if saved.fpstate != 0 {
        restore_fpu(space, saved.fpstate + FXSAVE_AT as u64)?;
    }
    saved.load(context);
    Ok(restored)
}

/// The mask, and for `rt_sigreturn` the alternate stack, the frame saved.
fn restored_of(bytes: &FrameBytes, sc: usize, rt: bool) -> Result<Restored, BadFrame> {
    if rt {
        let uc = rt_frame::UC;
        return Ok(Restored {
            mask: bytes.u64_at(uc + ucontext::SIGMASK)?,
            altstack: Some(stack_at(bytes, uc + ucontext::STACK)?),
        });
    }
    let low = u64::from(bytes.u32_at(sc + sigcontext::OLDMASK)?);
    let high = u64::from(bytes.u32_at(frame::EXTRAMASK)?);
    Ok(Restored {
        mask: low | (high << 32),
        altstack: None,
    })
}

/// A `sigcontext_32` as read back from a frame, before it is trusted.
struct Saved {
    /// `di`, `si`, `bp`, `sp`, `bx`, `dx`, `cx`, `ax` and `ip`.
    registers: [u64; 9],
    /// `cs` and `ss`, with RPL 3 put in.
    cs: u64,
    ss: u64,
    /// `flags`.
    flags: u64,
    /// `ds`, `es`, `fs` and `gs`.
    segments: [u16; 4],
    /// `fpstate`.
    fpstate: u64,
}

impl Saved {
    /// Read the `sigcontext_32` at `sc`.
    fn read(bytes: &FrameBytes, sc: usize) -> Result<Saved, BadFrame> {
        let mut words = [0_u64; 22];
        for (index, slot) in words.iter_mut().enumerate() {
            *slot = u64::from(bytes.u32_at(sc + index * 4)?);
        }
        let at = |offset: usize| words.get(offset / 4).copied().unwrap_or(0);
        let selector = |offset: usize| at(offset) as u16;
        Ok(Saved {
            registers: [
                sigcontext::DI,
                sigcontext::SI,
                sigcontext::BP,
                sigcontext::SP,
                sigcontext::BX,
                sigcontext::DX,
                sigcontext::CX,
                sigcontext::AX,
                sigcontext::IP,
            ]
            .map(at),
            cs: u64::from(selector(sigcontext::CS) | 3),
            ss: u64::from(selector(sigcontext::SS) | 3),
            flags: at(sigcontext::FLAGS),
            segments: [
                sigcontext::DS,
                sigcontext::ES,
                sigcontext::FS,
                sigcontext::GS,
            ]
            .map(selector),
            fpstate: at(sigcontext::FPSTATE),
        })
    }

    /// Refuse a forged frame: a code selector that is not 32-bit user code,
    /// a stack selector that is not user data, a flag no program can set for
    /// itself, or a data segment selector that is neither null nor one
    /// [`gdt::loadable`] would load -- a kernel slot, the TSS or the LDT.
    fn check(&self) -> Result<(), BadFrame> {
        // The running thread's own slots, which `switch::thread_area` reads
        // with interrupts masked.
        let tls: [u64; gdt::TLS_SLOTS] =
            core::array::from_fn(|index| switch::thread_area(index).unwrap_or(0));
        let forged_segment = self
            .segments
            .iter()
            .any(|&segment| segment & !3 != 0 && gdt::loadable(segment, &tls) == 0);
        let forged = self.cs != USER_CS32
            || self.ss != USER_SS
            || self.flags & FLAGS_PRIVILEGED != 0
            || forged_segment;
        if forged { Err(BadFrame) } else { Ok(()) }
    }

    /// Load what was checked into `context` and the segment registers, the
    /// flags masked as a 64-bit frame's are.
    fn load(&self, context: &mut UserContext) {
        let [di, si, bp, sp, bx, dx, cx, ax, ip] = self.registers;
        let regs = &mut context.0;
        (regs.rdi, regs.rsi, regs.rbp, regs.rsp) = (di, si, bp, sp);
        (regs.rbx, regs.rdx, regs.rcx, regs.rax) = (bx, dx, cx, ax);
        regs.rip = ip;
        regs.rflags = self.flags & FLAGS_RESTORABLE | FLAGS_ALWAYS;
        regs.cs = USER_CS32;
        regs.ss = USER_SS;
        switch::load_program_selectors(self.segments);
    }
}

/// The flags no program can set for itself, and so no frame of a program's
/// may carry: `IOPL`, virtual-8086 mode, and the virtual interrupt flag and
/// its pending bit. `popf` in ring 3 leaves every one of them alone, so a
/// frame holding one was written by hand.
const FLAGS_PRIVILEGED: u64 = (3 << 12) | (1 << 17) | (1 << 19) | (1 << 20);

/// The `struct sigcontext_32` for `context`: its registers' low halves, the
/// selectors the program holds, `fpstate` and the mask's low word.
fn sigcontext_of(
    context: &UserContext,
    fpstate: u32,
    oldmask: u32,
) -> Result<FrameBytes, BadFrame> {
    let regs = &context.0;
    let low = |value: u64| value as u32;
    let [ds, es, fs, gs] = cpu::read_data_selectors();
    let mut sc = FrameBytes::zeroed(sigcontext::SIZE)?;
    for (at, value) in [
        (sigcontext::GS, u32::from(gs)),
        (sigcontext::FS, u32::from(fs)),
        (sigcontext::ES, u32::from(es)),
        (sigcontext::DS, u32::from(ds)),
        (sigcontext::DI, low(regs.rdi)),
        (sigcontext::SI, low(regs.rsi)),
        (sigcontext::BP, low(regs.rbp)),
        (sigcontext::SP, low(regs.rsp)),
        (sigcontext::BX, low(regs.rbx)),
        (sigcontext::DX, low(regs.rdx)),
        (sigcontext::CX, low(regs.rcx)),
        (sigcontext::AX, low(regs.rax)),
        (sigcontext::IP, low(regs.rip)),
        (sigcontext::CS, low(regs.cs)),
        (sigcontext::FLAGS, low(regs.rflags)),
        (sigcontext::SP_AT_SIGNAL, low(regs.rsp)),
        (sigcontext::SS, low(regs.ss)),
        (sigcontext::FPSTATE, fpstate),
        (sigcontext::OLDMASK, oldmask),
    ] {
        sc.put_u32(at, value)?;
    }
    // A fault's vector and error code, which a handler for `SIGSEGV` reads;
    // zero for a signal the program did not cause.
    if regs.vector < 32 {
        sc.put_u32(sigcontext::TRAPNO, regs.vector as u32)?;
        sc.put_u32(sigcontext::ERR, regs.error_code as u32)?;
    }
    Ok(sc)
}

/// Write the program's x87 and SSE state at `fpstate` as `_fpstate_32`: the
/// `FXSAVE` image at its 112th byte, `magic` saying so, and the control and
/// status words in the `fsave` environment's first two.
fn write_fpstate(space: &AddressSpace, fpstate: u64) -> Result<(), BadFrame> {
    // SAFETY: (CONTEXT) on the running task's own way back to ring 3, so the processor
    // holds this program's x87 and SSE registers.
    let state = unsafe { switch::UserState::capture() };
    let image = state.fxsave();
    let [fcw_low, fcw_high, fsw_low, fsw_high, ..] = image;
    let control = u32::from(u16::from_le_bytes([fcw_low, fcw_high]));
    let status = u16::from_le_bytes([fsw_low, fsw_high]);
    let mut area = FrameBytes::zeroed(FPSTATE_SIZE)?;
    area.put_u32(0, control)?;
    area.put_u32(4, u32::from(status))?;
    area.put(FPSTATE_STATUS, &status.to_le_bytes())?;
    area.put(FPSTATE_MAGIC, &0_u16.to_le_bytes())?;
    area.put(FXSAVE_AT, &image)?;
    area.write(space, fpstate)
}

/// Put `record` at `at` as a `compat_stack_t`.
fn put_stack(bytes: &mut FrameBytes, at: usize, record: StackRecord) -> Result<(), BadFrame> {
    bytes.put_u32(at + stack::SP, record.sp as u32)?;
    bytes.put_u32(at + stack::FLAGS, record.flags as u32)?;
    bytes.put_u32(at + stack::SIZE_FIELD, record.size as u32)
}

/// The `compat_stack_t` at `at`.
fn stack_at(bytes: &FrameBytes, at: usize) -> Result<StackRecord, BadFrame> {
    Ok(StackRecord {
        sp: u64::from(bytes.u32_at(at + stack::SP)?),
        flags: bytes.u32_at(at + stack::FLAGS)? as i32,
        size: u64::from(bytes.u32_at(at + stack::SIZE_FIELD)?),
    })
}

const _: () = assert!(
    SIGINFO_SIZE == crate::signal_frame::SIGINFO_BYTES,
    "a 32-bit siginfo is as long as a 64-bit one"
);
