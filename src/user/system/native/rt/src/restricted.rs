//! Going into restricted mode and coming back (`docs/microkernel/K1.md`).
//!
//! Three calls answer in more registers than `ferrix_native::call::Syscall`
//! sees, so they are here, on the architecture's trap. Everything else about
//! restricted mode is an ordinary call in [`ferrix_native::region`].
//!
//! Each of them looks like one long system call: it comes back when the
//! thread leaves restricted mode, with [`Exit`] saying why.

use ferrix_native::error::Error;
use ferrix_native::region::{Context, Reason};
use ferrix_native_abi::nr;

use crate::arch;

/// What came back from restricted mode: `K1.md` §2.2's registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// The reason register, zero for a call the kernel refused.
    pub reason: u64,
    /// The program's system call number, or the fault's vector or class; a
    /// refused call's status.
    pub number: u64,
    /// The six argument registers: a system call's arguments, or what
    /// `ferrix_native_abi::restricted` names for a fault or a kick.
    pub words: [u64; 6],
}

impl Exit {
    /// Why it came back, or the call's refusal.
    ///
    /// # Errors
    ///
    /// The status of a call the kernel refused, with the program's state
    /// unchanged. For a wrong context that is the program's own doing, its
    /// `SIGSEGV`, unless `linuxd` made the context up.
    pub fn reason(&self) -> Result<Reason, Error> {
        match Reason::from_register(self.reason) {
            Some(reason) => Ok(reason),
            None => match ferrix_native::error::decode(self.number as usize) {
                Err(refused) => Err(refused),
                Ok(_) => Err(Error::BadState),
            },
        }
    }
}

/// `restricted_enter`: run `context`, checked first, until it exits.
#[must_use]
pub fn enter(context: &Context) -> Exit {
    let bytes = context.to_bytes();
    let at = bytes.as_ptr().addr() as u64;
    // SAFETY: `at` is this frame's own copy of the context, valid across the
    // call, which only reads it, and only before anything else happens.
    unsafe { arch::restricted(nr::RESTRICTED_ENTER, at, 0) }
}

/// [`enter`], an answered call's copy that misses exiting (`ENTER_COPY_MISS_EXITS`), and any
/// other option asked.
#[must_use]
pub fn enter_with(context: &Context, options: u64) -> Exit {
    let bytes = context.to_bytes();
    let at = bytes.as_ptr().addr() as u64;
    // SAFETY: as [`enter`].
    unsafe { arch::restricted(nr::RESTRICTED_ENTER, at, options) }
}

/// [`enter`], every system call exiting, a native number too (`ENTER_ALL_CALLS_EXIT`).
#[must_use]
pub fn enter_all_calls_exit(context: &Context) -> Exit {
    let bytes = context.to_bytes();
    let at = bytes.as_ptr().addr() as u64;
    // SAFETY: as [`enter`].
    unsafe {
        arch::restricted(
            nr::RESTRICTED_ENTER,
            at,
            ferrix_native::region::ENTER_ALL_CALLS_EXIT,
        )
    }
}

/// `restricted_resume`: put `value` in the program's result register and go
/// back in: the way back from a system call exit, with nothing else changed.
#[must_use]
#[inline(always)]
pub fn resume(value: u64) -> Exit {
    // SAFETY: no pointer: `value` goes to the program's register.
    unsafe { arch::restricted(nr::RESTRICTED_RESUME, value, 0) }
}

/// `resume`, asking that the same `number` be answered with `value` until any other exit, on
/// a kernel that has a window (`docs/microkernel/RESTRICTED-EXIT.md`). Elsewhere, as `resume`.
#[inline(always)]
#[must_use]
pub fn resume_armed(value: u64, number: u64) -> Exit {
    // SAFETY: no pointer: `value` goes to the program's register.
    unsafe { arch::restricted(nr::RESTRICTED_RESUME, value, number + 1) }
}

/// `restricted_continue`: go back in with nothing changed: the way back from a
/// kick, or to retry a fault.
#[must_use]
pub fn continue_() -> Exit {
    // SAFETY: no pointer.
    unsafe { arch::restricted(nr::RESTRICTED_CONTINUE, 0, 0) }
}

unsafe extern "C" {
    /// `n` bytes from `from` to `to`, in the copy window: zero, or the fault's
    /// [`FaultKind`].
    fn ferrix_program_copy(to: *mut u8, from: *const u8, n: usize) -> u64;
    static ferrix_program_copy_end: u8;
    static ferrix_program_copy_landing: u8;
}

/// Declare this runtime's copy routine as the region's copy window
/// (`docs/microkernel/K1.md` §2.5), once, before the first copy of a program's
/// memory.
///
/// # Errors
///
/// As [`ferrix_native::region::copy_window`].
pub fn declare_copy_window() -> Result<(), Error> {
    ferrix_native::region::copy_window(
        crate::Kernel,
        (ferrix_program_copy as *const ()).addr(),
        (&raw const ferrix_program_copy_end).addr(),
        (&raw const ferrix_program_copy_landing).addr(),
    )
}

/// Copy out of the program: `to.len()` bytes from its address `from`.
///
/// # Errors
///
/// The fault's kind, with what came before it copied.
pub fn copy_from_program(to: &mut [u8], from: u64) -> Result<(), u64> {
    // SAFETY: `to` is this program's own for the call; `from` is only ever read,
    // and a fault there lands, by the window [`declare_copy_window`] declared.
    match unsafe { ferrix_program_copy(to.as_mut_ptr(), from as *const u8, to.len()) } {
        0 => Ok(()),
        kind => Err(kind),
    }
}

/// Copy into the program: `from` to its address `to`.
///
/// # Errors
///
/// The fault's kind, with what came before it copied.
pub fn copy_to_program(to: u64, from: &[u8]) -> Result<(), u64> {
    // SAFETY: `from` is this program's own for the call; `to` is the program's,
    // and a fault there lands, by the window [`declare_copy_window`] declared.
    match unsafe { ferrix_program_copy(to as *mut u8, from.as_ptr(), from.len()) } {
        0 => Ok(()),
        kind => Err(kind),
    }
}
