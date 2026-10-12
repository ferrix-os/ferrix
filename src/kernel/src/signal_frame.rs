//! The bytes of a signal frame, and the types an architecture builds one from.
//!
//! These are data, and they sit in the trusted core for the reason
//! `docs/certification/ITEM.md` gives: `arch/*/signal.rs` writes and reads each
//! architecture's `rt_sigframe`, and it cannot be made to depend on the Linux
//! personality to name the types it does that with. `crate::syscall::deliver`
//! owns the *policy* -- when a signal is delivered and what happens after --
//! and imports these from here.
//!
//! `FrameBytes` is the only one with behaviour, and it is the reason this
//! module is worth having: a frame is built and read through offset-checked
//! accessors, so an architecture's layout is a table of constants and a
//! mistake in one is a refused frame rather than a panic.

use alloc::vec::Vec;

use crate::syscall::uaccess;
use crate::user::space::AddressSpace;

/// How many bytes a `siginfo_t` is, on every architecture Linux defines one
/// for. A frame carries one already encoded.
pub(crate) const SIGINFO_BYTES: usize = 128;

/// A frame the architecture could not write, or would not accept back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BadFrame;

/// `stack_t` as a frame records it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StackRecord {
    /// `ss_sp`.
    pub(crate) sp: u64,
    /// `ss_flags`.
    pub(crate) flags: i32,
    /// `ss_size`.
    pub(crate) size: u64,
}

/// Everything an architecture needs to put a handler's frame on the stack.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameRequest {
    /// The signal.
    pub(crate) signal: u32,
    /// Its `siginfo_t`, already encoded.
    pub(crate) info: [u8; SIGINFO_BYTES],
    /// Where the handler is.
    pub(crate) handler: u64,
    /// The handler's `SA_*` flags.
    pub(crate) flags: u64,
    /// Where the handler returns to.
    pub(crate) restorer: u64,
    /// The mask the frame saves, which `rt_sigreturn` puts back.
    pub(crate) mask: u64,
    /// The address the frame is built below, alternate stack already chosen.
    pub(crate) stack: u64,
    /// The alternate stack, as `uc_stack` records it.
    pub(crate) altstack: StackRecord,
    /// The ABI of the call that installed the handler: on x86-64 an i386
    /// handler is entered on an i386 frame and any other on a 64-bit one,
    /// whatever mode the interrupted code was in (Linux's `SA_IA32_ABI`).
    #[cfg_attr(
        not(target_arch = "x86_64"),
        expect(dead_code, reason = "only x86-64 has a second frame to choose")
    )]
    pub(crate) abi: crate::trap::Abi,
    /// Where the process's signal return page is, on an architecture that
    /// has one (ARMv7-A's, `syscall::sigpage`): what a handler without a
    /// restorer returns through (F-48).
    #[cfg_attr(
        not(target_arch = "arm"),
        expect(dead_code, reason = "only ARMv7-A has a signal return page to read")
    )]
    pub(crate) sigpage: Option<u64>,
}

/// What an architecture read back out of a frame, besides the registers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Restored {
    /// The mask the frame saved.
    pub(crate) mask: u64,
    /// The alternate stack the frame recorded, or `None` for a frame that
    /// records none -- i386's `sigframe`, whose `sigreturn` leaves the
    /// alternate stack as it is, as Linux's does.
    pub(crate) altstack: Option<StackRecord>,
}

/// A frame's bytes while they are built or read: offsets checked, so an
/// architecture's layout is written as a table of constants and a mistake in
/// one is a refused frame rather than a panic.
#[derive(Debug)]
pub(crate) struct FrameBytes(Vec<u8>);

impl FrameBytes {
    /// `len` zero bytes.
    ///
    /// # Errors
    ///
    /// [`BadFrame`] when there is no memory for them: a signal whose frame
    /// cannot be built is handled as one whose frame cannot be written.
    pub(crate) fn zeroed(len: usize) -> Result<FrameBytes, BadFrame> {
        crate::fallible::try_filled(0, len)
            .map(FrameBytes)
            .map_err(|_| BadFrame)
    }

    /// `len` bytes of the program's memory at `at`.
    pub(crate) fn read(space: &AddressSpace, at: u64, len: usize) -> Result<FrameBytes, BadFrame> {
        let FrameBytes(mut bytes) = FrameBytes::zeroed(len)?;
        uaccess::copy_from_user(space, at, &mut bytes).map_err(|_| BadFrame)?;
        Ok(FrameBytes(bytes))
    }

    /// Write them into the program's memory at `at`.
    pub(crate) fn write(&self, space: &AddressSpace, at: u64) -> Result<(), BadFrame> {
        uaccess::copy_to_user(space, at, &self.0).map_err(|_| BadFrame)
    }

    /// Put `bytes` at `at`.
    pub(crate) fn put(&mut self, at: usize, bytes: &[u8]) -> Result<(), BadFrame> {
        let end = at.checked_add(bytes.len()).ok_or(BadFrame)?;
        self.0
            .get_mut(at..end)
            .ok_or(BadFrame)?
            .copy_from_slice(bytes);
        Ok(())
    }

    /// Put a 32-bit value at `at`.
    pub(crate) fn put_u32(&mut self, at: usize, value: u32) -> Result<(), BadFrame> {
        self.put(at, &value.to_le_bytes())
    }

    /// Put a 64-bit value at `at`.
    pub(crate) fn put_u64(&mut self, at: usize, value: u64) -> Result<(), BadFrame> {
        self.put(at, &value.to_le_bytes())
    }

    /// Put a native word at `at`.
    fn put_word(&mut self, at: usize, value: u64) -> Result<(), BadFrame> {
        let word = value.to_le_bytes();
        self.put(at, word.get(..size_of::<usize>()).ok_or(BadFrame)?)
    }

    /// Put a `stack_t` at `at`: pointer, `int` flags, size, in native words.
    pub(crate) fn put_stack(&mut self, at: usize, stack: StackRecord) -> Result<(), BadFrame> {
        let word = size_of::<usize>();
        self.put_word(at, stack.sp)?;
        self.put(at + word, &stack.flags.to_le_bytes())?;
        self.put_word(at + 2 * word, stack.size)
    }

    /// The `len` bytes at `at`.
    pub(crate) fn get(&self, at: usize, len: usize) -> Result<&[u8], BadFrame> {
        let end = at.checked_add(len).ok_or(BadFrame)?;
        self.0.get(at..end).ok_or(BadFrame)
    }

    /// The 32-bit value at `at`.
    pub(crate) fn u32_at(&self, at: usize) -> Result<u32, BadFrame> {
        let mut value = [0_u8; 4];
        value.copy_from_slice(self.get(at, 4)?);
        Ok(u32::from_le_bytes(value))
    }

    /// The 64-bit value at `at`.
    pub(crate) fn u64_at(&self, at: usize) -> Result<u64, BadFrame> {
        let mut value = [0_u8; 8];
        value.copy_from_slice(self.get(at, 8)?);
        Ok(u64::from_le_bytes(value))
    }

    /// The native word at `at`, zero-extended.
    fn word_at(&self, at: usize) -> Result<u64, BadFrame> {
        let word = size_of::<usize>();
        let mut value = [0_u8; 8];
        value
            .get_mut(..word)
            .ok_or(BadFrame)?
            .copy_from_slice(self.get(at, word)?);
        Ok(u64::from_le_bytes(value))
    }

    /// The `stack_t` at `at`.
    pub(crate) fn stack_at(&self, at: usize) -> Result<StackRecord, BadFrame> {
        let word = size_of::<usize>();
        Ok(StackRecord {
            sp: self.word_at(at)?,
            flags: self.u32_at(at + word)? as i32,
            size: self.word_at(at + 2 * word)?,
        })
    }
}
