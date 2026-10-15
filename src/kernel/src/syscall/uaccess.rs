//! Reading and writing a user program's memory.
//!
//! Every system call that takes a pointer goes through here. That is the point
//! of the module: there is exactly one place where the kernel touches memory a
//! program chose the address of, so there is exactly one place to get the
//! checks right and one place to change when the hardware starts helping.
//!
//! # Why not just dereference it
//!
//! Three reasons, and each of them is a bug that has been shipped by somebody.
//!
//! **The address may not be mapped yet.** A user page is reserved by `mmap`
//! and paid for on first touch. The program may have written to a buffer, or
//! may only have reserved it; either way the kernel is often the first to
//! touch a given page. Dereferencing would fault in the kernel, on a kernel
//! stack, at a point where the fault handler cannot tell a legitimate
//! demand-paged access from a wild pointer. So the copy asks
//! [`AddressSpace::fault`] first, deliberately, and a page that cannot be
//! faulted in is [`UserError::Fault`] — a clean `EFAULT` to the program rather
//! than a kernel fault.
//!
//! **The address may not be the program's.** Nothing stops a program passing
//! a kernel address as a buffer, and nothing in the *hardware* stops the
//! kernel following one: SMAP on x86-64 and PAN on AArch64, which the kernel
//! turns on where the processor has them (finding F-32), refuse ring 0 a
//! *user* page, not its own memory. The bound check below is therefore the
//! only thing between a user pointer and a read of kernel memory at kernel
//! privilege, which is why it is the first statement in both functions and
//! why it happens before any arithmetic that could wrap. What SMAP and PAN
//! catch is the other mistake, a path that dereferences a user pointer
//! instead of coming here; this module reaches user pages through the direct
//! map, which neither of them sees.
//!
//! **The program's tables are not the kernel's.** A user address means nothing
//! in the kernel's own translation until the space is installed on this
//! processor, and even then only for the lower half. The copy resolves through
//! the target [`AddressSpace`] explicitly and reaches the page through the
//! direct map, so it works on a space that is *not* installed anywhere — which
//! is what `execve` needs when it writes an argument vector into a space the
//! processor has not switched to yet.
//!
//! # Page at a time, and why the bound is checked twice
//!
//! A user buffer is contiguous in the program's address space and need not be
//! contiguous in physical memory, so the copy is split at page boundaries and
//! each page resolved separately. The end of the range is bound-checked before
//! the loop *and* each page is checked as it is reached, because the first
//! check proves the caller asked for something sensible and the second proves
//! the walk stayed inside it.
//!
//! # Each page is copied under the space's lock
//!
//! Resolving a page and then copying through it with no lock held leaves a gap
//! in which another thread of the same process can unmap the page, and the
//! copy then lands in a frame that belongs to somebody else by now. So each
//! page's copy runs inside [`AddressSpace::with_page`], which holds the page's
//! translation still until the copy is done. Nothing reaches that gap today --
//! a process has one thread, and `vfork` copies the space rather than sharing
//! it -- but `clone(CLONE_VM)` will, and nothing about the copy would say so.

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END, is_user_address};
use ferrix_linux_abi::errno::Errno;

use crate::arch;

use crate::user::space::{Access, AddressSpace, SpaceError};

/// Why a copy to or from user memory failed.
///
/// Deliberately coarse. A program learns only `EFAULT`: telling it *which* of
/// its pages was unmapped is telling it about the kernel's address space, and
/// the distinctions matter to the kernel's own log rather than to the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserError {
    /// The address, or the end of the range, is not a user address.
    NotUserRange,
    /// The range is so long that its end cannot be computed.
    Overflow,
    /// The page is not mapped, is not writable, or could not be faulted in.
    Fault,
    /// There was no memory to copy into.
    NoMemory,
}

impl From<SpaceError> for UserError {
    /// Everything the address space can refuse is `EFAULT` to a program.
    fn from(_: SpaceError) -> Self {
        UserError::Fault
    }
}

/// Check that `[at, at + len)` lies wholly in the half a program gets.
///
/// Separated out and called first by both directions, because the ordering is
/// the security property: a length that wraps must be refused *before* it is
/// added to anything, or the addition is the bug.
fn check_range(at: u64, len: u64) -> Result<(), UserError> {
    if len == 0 {
        return Ok(());
    }
    if !is_user_address(at) {
        return Err(UserError::NotUserRange);
    }
    // `len - 1`, not `len`: a buffer ending exactly at the top of the user
    // half is legal, and checking the first byte past it would refuse it.
    let last = at
        .checked_add(len.wrapping_sub(1))
        .ok_or(UserError::Overflow)?;
    if !is_user_address(last) {
        return Err(UserError::NotUserRange);
    }
    Ok(())
}

/// Whether `len` bytes from `at` lie in the program's half, without looking
/// at whether they are mapped: Linux's `access_ok`, which some calls ask
/// before anything else.
pub fn is_user_range(at: u64, len: u64) -> bool {
    check_range(at, len).is_ok()
}

/// How much of a page is left from `at`.
fn to_page_end(at: u64) -> u64 {
    PAGE_SIZE - (at % PAGE_SIZE)
}

/// Run `touch` on the direct-map address of one user byte, with its page held.
///
/// Faults the page in first, then translates through the space's own tables,
/// and runs `touch` before the space lets the page go. `access` decides both
/// what the fault is allowed to do and, through it, whether a copy-on-write
/// page is copied before the kernel writes to it — which is why a write must
/// not be resolved with [`Access::READ`].
fn resolve<R>(
    space: &AddressSpace,
    at: u64,
    access: Access,
    touch: impl FnOnce(u64) -> R,
) -> Result<R, UserError> {
    Ok(space.with_page(user_address(at)?, access, touch)?)
}

/// `at`, if it is a user address, clamped so that a mispredicted check cannot
/// hand the table walk after it a kernel address.
///
/// The walk that follows reads the space's tables -- whose upper half, on
/// x86-64, is the kernel's -- and then the frame they lead to through the
/// direct map, which is a Spectre variant 1 gadget if a processor predicts the
/// check passed. On that path the address is zero instead. See
/// `crate::arch::nospec_below`.
fn user_address(at: u64) -> Result<u64, UserError> {
    arch::nospec_below(at, USER_VIRT_END).ok_or(UserError::NotUserRange)
}

/// Copy `out.len()` bytes out of the program's memory at `from`.
///
/// # Errors
///
/// [`UserError`]. Nothing here panics, and nothing is copied at all if the
/// range check fails.
pub fn copy_from_user(
    space: &AddressSpace,
    from: u64,
    out: &mut [u8],
) -> Result<(), UserError> {
    let copied = copy_from_user_through(space, from, out, Through::Faulting)?;
    debug_assert!(copied, "a copy that faults pages in stopped short");
    Ok(())
}

/// Copy `out.len()` bytes out of the program's memory at `from` only through
/// pages already there to be read, and answer `false` at the first that is
/// not.
///
/// For a read made holding a lock, as [`copy_to_user_present`] is for a write:
/// resolving a fault may allocate a frame and take the space's lock, and once
/// memory can be reclaimed it may wait, which nothing may do holding a lock.
/// This never faults: a page not present yet stops the copy with what came
/// before it already copied, and the caller lets go of its lock, reads the
/// range with [`copy_from_user`], which faults it in, and tries again.
///
/// # Errors
///
/// [`UserError`], for a range that is not the program's or a page it may not
/// read.
pub fn copy_from_user_present(
    space: &AddressSpace,
    from: u64,
    out: &mut [u8],
) -> Result<bool, UserError> {
    copy_from_user_through(space, from, out, Through::Present)
}

/// The copy both [`copy_from_user`] and [`copy_from_user_present`] make:
/// `false` if a page `through` may not fault in stopped it.
fn copy_from_user_through(
    space: &AddressSpace,
    from: u64,
    out: &mut [u8],
    through: Through,
) -> Result<bool, UserError> {
    let len = u64::try_from(out.len()).map_err(|_| UserError::Overflow)?;
    check_range(from, len)?;

    let mut done = 0_usize;
    while done < out.len() {
        let at = from
            .checked_add(u64::try_from(done).map_err(|_| UserError::Overflow)?)
            .ok_or(UserError::Overflow)?;
        let chunk = chunk_len(at, out.len() - done)?;
        let target = out.get_mut(done..done + chunk).ok_or(UserError::Overflow)?;
        let read = |source: u64| {
            // SAFETY: (USER-COPY) both ways here -- `resolve`, having faulted the page in,
            // and `with_present_page`, having found it present -- translated
            // it through the space's own tables and run this with the space's
            // lock held, so `source` is the direct-map address of a frame that
            // stays live for the read; `chunk` was clamped to the remainder of
            // that page, so the whole read is inside it. The direct map is
            // readable for all of RAM.
            let bytes = unsafe { core::slice::from_raw_parts(source as *const u8, chunk) };
            target.copy_from_slice(bytes);
        };
        match through {
            Through::Faulting => resolve(space, at, Access::READ, read)?,
            Through::Present => {
                if space
                    .with_present_page(user_address(at)?, Access::READ, read)?
                    .is_none()
                {
                    return Ok(false);
                }
            }
        }
        done += chunk;
    }
    Ok(true)
}

/// Copy `data` into the program's memory at `to`.
///
/// # Errors
///
/// [`UserError`]. A partial copy is possible if a later page cannot be
/// resolved, which matches Linux: `write` and its kin report the count they
/// managed, and a caller that needs all-or-nothing must check the range first.
pub fn copy_to_user(space: &AddressSpace, to: u64, data: &[u8]) -> Result<(), UserError> {
    let copied = copy_to_user_through(space, to, data, Through::Faulting)?;
    debug_assert!(copied, "a copy that faults pages in stopped short");
    Ok(())
}

/// Copy `data` into the program's memory at `to` only through pages already
/// there to be written, and answer `false` at the first that is not.
///
/// For a copy made holding a lock. [`copy_to_user`] faults each page in, and
/// resolving a fault can copy a copy-on-write page and then wait for every
/// processor to drop the translation it replaced, which nothing may do holding
/// a lock. This never faults: a page not present yet, or one still shared
/// copy-on-write, stops the copy with what came before it already copied, and
/// the caller lets go of its lock, faults the range in with
/// [`fault_in_for_write`], and tries again.
///
/// # Errors
///
/// [`UserError`], for a range that is not the program's or a page it may not
/// write.
pub fn copy_to_user_present(
    space: &AddressSpace,
    to: u64,
    data: &[u8],
) -> Result<bool, UserError> {
    copy_to_user_through(space, to, data, Through::Present)
}

/// How a copy to or from user memory reaches each page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Through {
    /// Faulting it in first, as [`copy_from_user`] and [`copy_to_user`] do.
    Faulting,
    /// Only if it is already there, as [`copy_from_user_present`] and
    /// [`copy_to_user_present`] do.
    Present,
}

/// The copy both [`copy_to_user`] and [`copy_to_user_present`] make: `false`
/// if a page `through` may not fault in stopped it.
fn copy_to_user_through(
    space: &AddressSpace,
    to: u64,
    data: &[u8],
    through: Through,
) -> Result<bool, UserError> {
    let len = u64::try_from(data.len()).map_err(|_| UserError::Overflow)?;
    check_range(to, len)?;

    let mut done = 0_usize;
    while done < data.len() {
        let at = to
            .checked_add(u64::try_from(done).map_err(|_| UserError::Overflow)?)
            .ok_or(UserError::Overflow)?;
        let chunk = chunk_len(at, data.len() - done)?;
        // `Access::WRITE`, which is what copies a copy-on-write page before
        // the kernel writes into it -- or, without faulting, refuses one still
        // shared. Resolving with `READ` here would have the kernel writing into
        // a page the parent can still see.
        let source = data.get(done..done + chunk).ok_or(UserError::Overflow)?;
        let write = |target: u64| {
            // SAFETY: (USER-COPY) both ways here -- `resolve`, having faulted the page in
            // for writing, and `with_present_page`, having found it present and
            // writable and not shared copy-on-write -- translated it through
            // the space's own tables and run this with the space's lock held,
            // so `target` is the direct-map address of a frame this space may
            // write and that stays live for the write; `chunk` was clamped to
            // the remainder of that page. The direct map is writable for RAM.
            let bytes = unsafe { core::slice::from_raw_parts_mut(target as *mut u8, chunk) };
            bytes.copy_from_slice(source);
        };
        match through {
            Through::Faulting => resolve(space, at, Access::WRITE, write)?,
            Through::Present => {
                if space
                    .with_present_page(user_address(at)?, Access::WRITE, write)?
                    .is_none()
                {
                    return Ok(false);
                }
            }
        }
        done += chunk;
    }
    Ok(true)
}

/// Fault `[at, at + len)` in for writing, holding no lock, so that a
/// [`copy_to_user_present`] of it straight after finds every page -- unless
/// another thread undoes that in between, and the caller goes round again.
///
/// # Errors
///
/// [`UserError`], for a range that is not the program's or a page it may not
/// write.
pub fn fault_in_for_write(
    space: &AddressSpace,
    at: u64,
    len: usize,
) -> Result<(), UserError> {
    let len = u64::try_from(len).map_err(|_| UserError::Overflow)?;
    check_range(at, len)?;
    let end = at.checked_add(len).ok_or(UserError::Overflow)?;
    let mut page = at;
    while page < end {
        crate::object::oom::fault(space, user_address(page)?, Access::WRITE)?;
        page = (page - page % PAGE_SIZE).saturating_add(PAGE_SIZE);
    }
    Ok(())
}

/// How many bytes of `remaining` may be copied starting at `at` without
/// leaving the page `at` is in.
fn chunk_len(at: u64, remaining: usize) -> Result<usize, UserError> {
    let to_end = usize::try_from(to_page_end(at)).map_err(|_| UserError::Overflow)?;
    Ok(to_end.min(remaining))
}

/// Read a NUL-terminated string out of the program's memory.
///
/// `limit` bounds the answer, because a program can pass a pointer into a
/// region with no NUL in it and the kernel must not walk to the end of the
/// address space looking for one. Linux calls that bound `PATH_MAX` or
/// `MAX_ARG_STRLEN` depending on the caller; the caller passes it here.
///
/// Returns the bytes without the terminator.
///
/// # Errors
///
/// [`UserError::Fault`] if the string is unterminated within `limit`, which is
/// what Linux reports for a path that long — the program gets `EFAULT` rather
/// than a truncated string it did not ask for.
pub fn copy_cstr_from_user(
    space: &AddressSpace,
    from: u64,
    limit: usize,
    out: &mut alloc::vec::Vec<u8>,
) -> Result<(), UserError> {
    out.clear();
    let mut at = from;
    while out.len() < limit {
        // One page at a time, so that a string near the top of a mapping does
        // not require the *next* page to be mapped at all.
        let span = usize::try_from(to_page_end(at)).map_err(|_| UserError::Overflow)?;
        let span = span.min(limit - out.len());
        // Reserved before the page is held, so that extending `out` under the
        // space's lock never has to allocate.
        crate::fallible::try_reserve(out, span).map_err(|_| UserError::NoMemory)?;
        let terminated = resolve(space, at, Access::READ, |source| {
            // SAFETY: (USER-COPY) as `copy_from_user`. `span` stays inside the held page.
            let bytes = unsafe { core::slice::from_raw_parts(source as *const u8, span) };
            let end = bytes.iter().position(|&b| b == 0);
            // NOALLOC: into the room reserved above.
            out.extend_from_slice(bytes.get(..end.unwrap_or(span)).unwrap_or_default());
            end.is_some()
        })?;
        if terminated {
            return Ok(());
        }
        at = at
            .checked_add(u64::try_from(span).map_err(|_| UserError::Overflow)?)
            .ok_or(UserError::Overflow)?;
    }
    Err(UserError::Fault)
}

/// Bytes in a C `long`, `size_t` or pointer on this build: eight on the two
/// 64-bit architectures, four on ARMv7-A.
///
/// Every structure a system call writes that is made of `long`s changes size
/// with this, which is why the handlers compute their layouts from it rather
/// than writing a size down.
pub const WORD: usize = size_of::<usize>();

/// Write one C `int` or `unsigned int` -- four bytes on every architecture.
///
/// # Errors
///
/// `EFAULT` if the address is not writable user memory.
pub fn put_u32(space: &AddressSpace, at: u64, value: u32) -> Result<(), Errno> {
    copy_to_user(space, at, &value.to_le_bytes()).map_err(|_| Errno::EFAULT)
}

/// Write one native word: a `long`, a `size_t` or a pointer.
///
/// Truncated to four bytes on a 32-bit build, which is the caller's to have
/// made safe: a value that does not fit has to be decided about (clamped to
/// `RLIM_INFINITY`, reported as `EOVERFLOW`) before it gets here.
///
/// # Errors
///
/// `EFAULT` if the address is not writable user memory.
pub fn put_word(space: &AddressSpace, at: u64, value: u64) -> Result<(), Errno> {
    let bytes = value.to_le_bytes();
    let word = bytes.get(..WORD).ok_or(Errno::EFAULT)?;
    copy_to_user(space, at, word).map_err(|_| Errno::EFAULT)
}

/// Read one C `int`, four bytes on every architecture.
///
/// # Errors
///
/// `EFAULT` if the address is not readable user memory.
pub fn get_u32(space: &AddressSpace, at: u64) -> Result<u32, Errno> {
    let mut bytes = [0_u8; 4];
    copy_from_user(space, at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Read one native word, zero-extended: an `unsigned long` or a pointer.
///
/// # Errors
///
/// `EFAULT` if the address is not readable user memory.
pub fn get_word(space: &AddressSpace, at: u64) -> Result<u64, Errno> {
    let mut bytes = [0_u8; 8];
    let word = bytes.get_mut(..WORD).ok_or(Errno::EFAULT)?;
    copy_from_user(space, at, word).map_err(|_| Errno::EFAULT)?;
    Ok(u64::from_le_bytes(bytes))
}
