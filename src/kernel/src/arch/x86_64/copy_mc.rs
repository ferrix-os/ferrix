//! A copy the machine check can stop (`docs/MACHINE-CHECK-COPY.md` §3.1,
//! `L.x86_64.244`): one `rep movsb` at a symbol of its own, so that a
//! machine check taken on one of its loads finds `rip` at that instruction
//! and `rcx` the bytes left, and the ring-0 handler can send it to its exit
//! instead of stopping (`super::mce`, `L.x86_64.246`).

use core::arch::global_asm;

// `rdi` the destination, `rsi` the source, `rdx` the length (the SysV
// order). `rep movsb` is interruptible between its iterations, leaving `rsi`
// at the next byte to load and `rcx` the count left; the direction flag is
// clear on entry to any function, as the ABI requires. A finished copy
// answers `(0, 0)` in `rax:rdx`; the machine check's fixup resumes at
// `ferrix_copy_mc_out` with the page it found in `rdx` and `rcx` as left.
global_asm!(
    ".pushsection .text.ferrix_copy_mc, \"ax\", @progbits",
    ".global ferrix_copy_mc",
    ".global ferrix_copy_mc_movs",
    ".global ferrix_copy_mc_out",
    ".p2align 4",
    "ferrix_copy_mc:",
    "    mov rcx, rdx",
    "ferrix_copy_mc_movs:",
    "    rep movsb",
    "    xor edx, edx",
    "ferrix_copy_mc_out:",
    "    mov rax, rcx",
    "    ret",
    ".popsection",
);

/// What a copy did: `left` bytes not copied and the poisoned `page` with
/// bit 0 set, or both 0 for a finished copy. Returned in `rax:rdx`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Copied {
    /// Bytes not copied.
    pub(crate) left: u64,
    /// The page the machine check found, with bit 0 set; 0 if none.
    pub(crate) page: u64,
}

impl Copied {
    /// The poisoned page's physical address, if the copy was stopped.
    pub(crate) const fn poisoned(self) -> Option<u64> {
        if self.page & 1 != 0 {
            Some(self.page & !1)
        } else {
            None
        }
    }
}

unsafe extern "C" {
    fn ferrix_copy_mc(dst: *mut u8, src: *const u8, len: usize) -> Copied;
    static ferrix_copy_mc_movs: u8;
    static ferrix_copy_mc_out: u8;
}

/// The relocated address of the copy's `rep movsb`.
pub(super) fn movs_address() -> u64 {
    core::ptr::addr_of!(ferrix_copy_mc_movs) as u64
}

/// The relocated address of the copy's exit.
pub(super) fn out_address() -> u64 {
    core::ptr::addr_of!(ferrix_copy_mc_out) as u64
}

/// Copy `out.len()` bytes from the kernel address `source` into `out`.
///
/// # Safety
///
/// (USER-COPY) `source..source + out.len()` is readable kernel memory that
/// stays live for the call.
pub(crate) unsafe fn copy_mc(out: &mut [u8], source: u64) -> Copied {
    // SAFETY: (USER-COPY) the caller's range is readable and live; `out` is
    // writable for its length; the routine writes nothing else, reads only
    // that range and holds no state of its own (`L.x86_64.244`).
    unsafe { ferrix_copy_mc(out.as_mut_ptr(), source as *const u8, out.len()) }
}
