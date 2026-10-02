//! `mmap`, `munmap`, `mprotect`, `mremap`, `madvise` and `brk`.
//!
//! The four calls a program reshapes its own address space with, and the first
//! four a static musl binary makes: it allocates with `mmap` before it does
//! anything else, and `brk` only as a fallback. (A traced static *glibc*
//! binary is the other way round, which is worth knowing because planning from
//! a glibc trace would have these built in the wrong order.)
//!
//! # Where the work actually is
//!
//! Not here. `src/lib/kernel/vma` already implements the interval tree and the three
//! operations that reshape it — insert, remove and protect, with splitting and
//! merging, host-tested — and `AddressSpace` already turns a region into pages
//! on demand. What is left in this module is argument decoding, which sounds
//! trivial and is where the bugs are: a length that wraps when rounded up, an
//! offset counted in the wrong unit, a `PROT_NONE` that is silently turned
//! into a readable page.

use alloc::sync::Arc;
use core::any::Any;

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    F_SEAL_FUTURE_WRITE, F_SEAL_WRITE, MADV_COLD, MADV_DODUMP, MADV_DOFORK, MADV_DONTDUMP,
    MADV_DONTFORK, MADV_DONTNEED, MADV_DONTNEED_LOCKED, MADV_FREE, MADV_HUGEPAGE, MADV_NOHUGEPAGE,
    MADV_NORMAL, MADV_PAGEOUT, MADV_RANDOM, MADV_REMOVE, MADV_SEQUENTIAL, MADV_WILLNEED,
    MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE, MAP_SHARED, MREMAP_FIXED,
    MREMAP_MAYMOVE, MS_ASYNC, MS_INVALIDATE, MS_SYNC, PROT_EXEC, PROT_GROWSDOWN, PROT_GROWSUP,
    PROT_READ, PROT_SEM, PROT_WRITE,
};
use ferrix_vfs::OpenFile;
use ferrix_vma::VmaFlags;

use crate::syscall::fd;
use crate::syscall::process::Process;
use crate::user::space::{
    Advice, Declined, Destination, FileMapping, FilePlace, MMAP_MIN_ADDR, SpaceError,
};
use crate::user::vmo::Vmo;

/// What `mmap`'s sixth argument is counted in.
///
/// The reason [`super::Syscall::Mmap2`] is a different call rather than a
/// different number for the same one. Getting this wrong maps the wrong part
/// of a file, and silently: every address is valid, just not the one asked
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffsetUnit {
    /// `mmap`: bytes.
    Bytes,
    /// `mmap2`, ARMv7-A only: 4096-byte units, because a 32-bit register
    /// cannot carry a file offset in bytes.
    Pages,
}

/// Turn `PROT_*` into region flags.
///
/// `PROT_NONE` is zero and means no access at all, which [`VmaFlags::NONE`]
/// says exactly. It is a real request — libc reserves guard pages with it —
/// and must not be quietly widened to readable.
fn protection(prot: u32) -> Result<VmaFlags, Errno> {
    const KNOWN: u32 = PROT_READ | PROT_WRITE | PROT_EXEC;
    if prot & !KNOWN != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(VmaFlags {
        read: prot & PROT_READ != 0,
        write: prot & PROT_WRITE != 0,
        execute: prot & PROT_EXEC != 0,
        ..VmaFlags::NONE
    })
}

/// Everything an address space can refuse, as the program sees it.
pub(crate) fn refused(error: SpaceError) -> Errno {
    match error {
        SpaceError::OutOfMemory | SpaceError::Backing(_) => Errno::ENOMEM,
        SpaceError::Unreadable(_) => Errno::EIO,
        // A fault window is unmapped, replaced, moved or reprotected only
        // whole: anything else is refused with the map unchanged.
        SpaceError::NotUserRange(_) | SpaceError::BadRange | SpaceError::WindowChange => {
            Errno::EINVAL
        }
        // A copy into a file mapping past the file's end is EFAULT from a
        // system call, where the same touch from user mode is SIGBUS; so is
        // a copy into a fault window's page the server did not serve.
        SpaceError::NotMapped(_)
        | SpaceError::Refused(_)
        | SpaceError::PastEnd(_)
        | SpaceError::WindowFault(_) => Errno::EFAULT,
    }
}

/// Round a length up to a whole number of pages.
///
/// A length of zero is `EINVAL` for `mmap`, and the rounding must not wrap:
/// `mmap(NULL, usize::MAX, ...)` is a thing programs do by accident and it
/// must be an error, not a very small mapping.
fn pages_for(len: u64) -> Result<u64, Errno> {
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    len.checked_add(PAGE_SIZE - 1)
        .map(|len| len & !(PAGE_SIZE - 1))
        .ok_or(Errno::ENOMEM)
}

/// `mmap`'s arguments, as the ABI passes them.
///
/// A struct rather than seven parameters because seven positional arguments of
/// which three are integers is a call nobody can read, and because the trap
/// path hands them over as a block anyway.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MmapRequest {
    /// Where the program wants it; a hint unless `MAP_FIXED` is set.
    pub(crate) addr: u64,
    /// How many bytes, before rounding.
    pub(crate) len: u64,
    /// `PROT_*`.
    pub(crate) prot: u32,
    /// `MAP_*`.
    pub(crate) flags: u32,
    /// The file to map. Ignored for anonymous memory, as Linux ignores it.
    pub(crate) fd: i64,
    /// The offset into that file, in `unit`s.
    pub(crate) offset: u64,
    /// What `offset` is counted in.
    pub(crate) unit: OffsetUnit,
}

/// `mmap` and `mmap2`.
///
/// Anonymous memory, and a file. A shared file mapping shows the file's own
/// pages, the ones `read` copies out of, so a write through either is seen
/// through the other. A private one shows the same pages until it writes one,
/// and the write copies that page into an object of the mapping's own, so the
/// file never sees it.
pub(crate) fn sys_mmap(process: &Process, request: &MmapRequest) -> Result<usize, Errno> {
    let &MmapRequest {
        addr,
        len,
        prot,
        flags,
        fd,
        offset,
        unit,
    } = request;
    let mut vma = protection(prot)?;
    let len = pages_for(len)?;

    // Exactly one of SHARED and PRIVATE, which is what Linux requires.
    let shared = flags & MAP_SHARED != 0;
    if shared == (flags & MAP_PRIVATE != 0) {
        return Err(Errno::EINVAL);
    }
    vma.shared = shared;

    // What Linux refuses of any offset is refused: a byte offset that is not
    // a page boundary, which the entry point checks before it looks at any
    // flag, and an offset whose pages would wrap.
    let page_offset = match unit {
        OffsetUnit::Bytes if !offset.is_multiple_of(PAGE_SIZE) => return Err(Errno::EINVAL),
        OffsetUnit::Bytes => offset / PAGE_SIZE,
        OffsetUnit::Pages => offset,
    };
    let page_offset = usize::try_from(page_offset).map_err(|_| Errno::EOVERFLOW)?;
    let pages = usize::try_from(len / PAGE_SIZE).map_err(|_| Errno::ENOMEM)?;
    if page_offset.checked_add(pages).is_none() {
        return Err(Errno::EOVERFLOW);
    }

    // From here the call looks at the map and changes it, perhaps twice.
    let _layout = process.space().layout();
    if flags & MAP_ANONYMOUS == 0 {
        let offset = (page_offset as u64)
            .checked_mul(PAGE_SIZE)
            .ok_or(Errno::EOVERFLOW)?;
        return map_file(process, fd, addr, len, flags, vma, offset);
    }

    // Linux ignores an anonymous mapping's descriptor, and its offset once the
    // offset is whole pages, so a program passing a real fd with
    // `MAP_ANONYMOUS` gets zeroes there and must get them here.
    let _ = fd;
    match place(process, addr, len, flags)? {
        FilePlace::Anywhere(hint) => process
            .space()
            .map_anywhere(hint, len, vma)
            .map(usize_of)
            .map_err(refused),
        FilePlace::Fixed(at) => process
            .space()
            .map_anonymous(at, len, vma)
            .map(|_| usize_of(at))
            .map_err(refused),
    }
}

/// Where a mapping of `len` bytes asked for at `addr` with `flags` goes, with
/// the range already cleared for plain `MAP_FIXED`.
///
/// Called once everything else about the call has been checked, because the
/// clearing is the one step that changes the address space: a call refused
/// after it would have unmapped what the program had there for nothing.
fn place(process: &Process, addr: u64, len: u64, flags: u32) -> Result<FilePlace, Errno> {
    if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) == 0 {
        // A non-null address without MAP_FIXED is a hint, and a hint that does
        // not fit is answered elsewhere rather than refused.
        return Ok(FilePlace::Anywhere((addr != 0).then_some(addr)));
    }
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    // Below the floor is `EPERM`, which is what Linux answers a process without
    // `CAP_SYS_RAWIO`. There are no capabilities here to hold, so that is every
    // process, root or not. Checked before the unmap below, so a refused call
    // changes nothing. A hint is not refused for the same address: the search
    // simply starts above the floor.
    if addr < MMAP_MIN_ADDR {
        return Err(Errno::EPERM);
    }
    // A fault window is replaced only whole: a fixed mapping over part of
    // one is refused, with nothing changed, whichever kind of fixed it is.
    if process.space().cuts_window(addr, len) {
        return Err(Errno::EINVAL);
    }
    if flags & MAP_FIXED_NOREPLACE == 0 {
        // Plain MAP_FIXED replaces whatever is there. Unmapping first is what
        // makes that true; an unmap of a range holding nothing is not an
        // error here, because the program asked for the result, not the steps.
        let _ = process.space().unmap(addr, len);
    }
    Ok(FilePlace::Fixed(addr))
}

/// `mmap` of the file `fd` names, from byte `offset`.
///
/// The refusals come in Linux's order: a descriptor that names nothing, or
/// only a path, is `EBADF`; one not open for reading is `EACCES`, and so is a
/// shared writable mapping of one not open for writing -- a private one never
/// writes the file, so it may be writable either way; a file with no pages
/// to map -- a directory, a pipe, a device, a generated `/proc` file -- is
/// `ENODEV`. `O_APPEND` refuses nothing: Linux refuses only an inode marked
/// append-only, which no filesystem here can mark.
fn map_file(
    process: &Process,
    descriptor: i64,
    addr: u64,
    len: u64,
    flags: u32,
    vma: VmaFlags,
    offset: u64,
) -> Result<usize, Errno> {
    let file = fd::file(process, fd::arg(descriptor as u64)).map_err(|_| Errno::EBADF)?;
    if file.is_path() {
        return Err(Errno::EBADF);
    }
    if !file.readable() || (vma.shared && vma.write && !file.writable()) {
        return Err(Errno::EACCES);
    }
    // Linux's `do_mmap`: a file on a `noexec` mount is never mapped
    // executable.
    if vma.execute && file.location().mount.no_exec() {
        return Err(Errno::EPERM);
    }
    // The open's own object is asked first: a render node's buffer objects
    // belong to the open, not to the name it was opened by. For a file whose
    // open is its inode this is one question asked once.
    let (object, offset) = file
        .io()
        .mapping_at(offset)
        .or_else(|| file.inode().mapping_at(offset))
        .ok_or(Errno::ENODEV)?;
    let vmo = match object.downcast::<Vmo>() {
        Ok(vmo) => vmo,
        Err(object) => return map_window(process, object, addr, len, flags, vma, offset),
    };
    // A sealed file: a shared writable mapping is `EPERM`, and a shared
    // read-only one may never be made writable. Linux's `seal_check_write`.
    let write_sealed = |seals: u32| seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0;
    let sealed = write_sealed(file.inode().seals().unwrap_or(0));
    if vma.shared && vma.write && sealed {
        return Err(Errno::EPERM);
    }
    let may_write = vma.shared && file.writable() && !sealed;
    let at = place(process, addr, len, flags)?;
    let mapping = FileMapping {
        file: Arc::clone(&file) as Arc<dyn Any + Send + Sync>,
        may_write,
    };
    let mapped = process
        .space()
        .map_file(at, len, vma, vmo, offset, mapping)
        .map_err(refused)?;
    // The second look. The mapping counted itself as it went into the tables,
    // so a write seal stored before that count is seen here, and one stored
    // after it has seen the count and refused itself with `EBUSY`.
    if may_write && write_sealed(file.inode().seals().unwrap_or(0)) {
        let _ = process.space().unmap(mapped, len);
        return Err(Errno::EPERM);
    }
    Ok(usize_of(mapped))
}

/// `mmap` of a render node's blob: pages of the device's host-visible window
/// rather than of a VMO (`docs/GPU.md` §6.1).
///
/// Shared only, as Linux's `virtio_gpu_vram_mmap` takes it: the pages are the
/// device's, and a private copy of them would be a copy of what the host is
/// still writing. The range must lie inside the blob. The region keeps the
/// blob, so the device is not told to let it go while a program can still
/// reach its pages.
fn map_window(
    process: &Process,
    object: Arc<dyn Any + Send + Sync>,
    addr: u64,
    len: u64,
    flags: u32,
    vma: VmaFlags,
    offset: u64,
) -> Result<usize, Errno> {
    let window = object
        .downcast::<crate::interfaces::render::node::Window>()
        .map_err(|_| Errno::ENODEV)?;
    let (phys, bytes, cached) = window.place().ok_or(Errno::ENODEV)?;
    if !vma.shared {
        return Err(Errno::EINVAL);
    }
    if offset
        .checked_add(len)
        .is_none_or(|end| end > bytes.next_multiple_of(PAGE_SIZE))
    {
        return Err(Errno::EINVAL);
    }
    let start = phys.checked_add(offset).ok_or(Errno::EINVAL)?;
    let at = place(process, addr, len, flags)?;
    let mapped = process
        .space()
        .map_window(at, len, start, vma, cached, window)
        .map_err(refused)?;
    Ok(usize_of(mapped))
}

/// `msync`.
///
/// Nothing is written back, because nothing needs to be: a shared file
/// mapping's pages are the file's own pages, so a `read` already sees every
/// write. tmpfs and a read-only btrfs have no disk to flush them to, and a
/// writable btrfs writes a mapped file's pages at its next commit, which this
/// does not bring forward. What is left is what Linux checks, in its order: unknown flags,
/// `MS_ASYNC` with `MS_SYNC`, and an address off a page boundary are
/// `EINVAL`; a range that wraps, or is not wholly mapped, is `ENOMEM`.
pub(crate) fn sys_msync(
    process: &Process,
    addr: u64,
    len: u64,
    flags: u32,
) -> Result<usize, Errno> {
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0
        || (flags & MS_ASYNC != 0 && flags & MS_SYNC != 0)
        || !addr.is_multiple_of(PAGE_SIZE)
    {
        return Err(Errno::EINVAL);
    }
    let len = len
        .checked_add(PAGE_SIZE - 1)
        .map(|len| len & !(PAGE_SIZE - 1))
        .ok_or(Errno::ENOMEM)?;
    let end = addr.checked_add(len).ok_or(Errno::ENOMEM)?;
    let covered = process.space().with_regions(|regions| {
        let mut covered = addr;
        for region in regions {
            if covered >= end || region.start > covered {
                break;
            }
            covered = covered.max(region.end);
        }
        covered
    });
    if covered < end {
        return Err(Errno::ENOMEM);
    }
    Ok(0)
}

/// `madvise`.
///
/// What `PartitionAlloc`, V8 and every `malloc` give memory back with while
/// keeping the addresses: `MADV_DONTNEED` and `MADV_FREE` drop the pages of a
/// range and leave it mapped, so the next touch reads zeros -- or, in a
/// private file mapping, the file -- and the frames are the allocator's
/// again at once. `MADV_FREE` is allowed to keep the pages until memory is
/// wanted and a later write cancels it; here it drops them at once, as
/// `MADV_DONTNEED` does, which is what Linux does itself when there is no
/// swap to age them against and what a program may always be given. See
/// [`crate::user::space::AddressSpace::advise`] for what each mapping kind
/// does and `Advice` for the rest.
///
/// `MADV_REMOVE` punches a hole in shared anonymous memory; on a shared file
/// mapping it is `EOPNOTSUPP`, which is `fallocate`'s answer here for the
/// same hole. The hints -- `MADV_NORMAL`, `MADV_RANDOM`, `MADV_SEQUENTIAL`,
/// `MADV_WILLNEED`, `MADV_DONTFORK`, `MADV_DOFORK`, `MADV_HUGEPAGE`,
/// `MADV_NOHUGEPAGE`, `MADV_DONTDUMP`, `MADV_DODUMP`, `MADV_COLD` and
/// `MADV_PAGEOUT` -- are accepted wherever Linux accepts them and change
/// nothing: there is no read-ahead, no huge page, no core dump and no swap for
/// them to steer. `MADV_DONTFORK` in particular is not honoured -- a `fork`
/// child still gets the range, copy-on-write -- which costs such a child
/// memory it was not meant to have, not correctness.
///
/// Everything else is `EINVAL`, as from a Linux built without what it needs:
/// KSM's pair, `MADV_POPULATE_*`, `MADV_COLLAPSE`, the poisoning and guard
/// advice, and `MADV_WIPEONFORK` with `MADV_KEEPONFORK`. Those two change what
/// a `fork` child sees, and `BoringSSL` keys its random generator's reseeding
/// on them: accepting one without honouring it would hand a child its
/// parent's random state, where a refusal makes `BoringSSL` look another way.
///
/// The checks run in `do_madvise`'s order: the advice, then an address off a
/// page boundary, then a length that wraps when rounded up or when added --
/// all `EINVAL` -- and then a zero length succeeds. Part of the range mapped
/// by nothing is `ENOMEM`, once the rest has been advised.
pub(crate) fn sys_madvise(
    process: &Process,
    addr: u64,
    len: u64,
    advice: i32,
) -> Result<usize, Errno> {
    let advice = match advice {
        MADV_DONTNEED => Advice::Discard { locked_too: false },
        MADV_DONTNEED_LOCKED => Advice::Discard { locked_too: true },
        MADV_FREE => Advice::Free,
        MADV_REMOVE => Advice::Remove,
        // `madvise_update_vma` refuses `MADV_DOFORK` on `VM_IO`, and
        // `MADV_DODUMP` on `VM_SPECIAL`, which a device's registers are.
        MADV_DOFORK | MADV_DODUMP => Advice::Hint {
            not_on_device: true,
            not_on_locked: false,
        },
        // `can_madv_lru_vma`: not on `VM_LOCKED` or `VM_PFNMAP`.
        MADV_COLD | MADV_PAGEOUT => Advice::Hint {
            not_on_device: true,
            not_on_locked: true,
        },
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL | MADV_WILLNEED | MADV_DONTFORK
        | MADV_HUGEPAGE | MADV_NOHUGEPAGE | MADV_DONTDUMP => Advice::Hint {
            not_on_device: false,
            not_on_locked: false,
        },
        _ => return Err(Errno::EINVAL),
    };
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    // `size_t` arithmetic, so a 32-bit caller's length wraps where Linux's
    // `PAGE_ALIGN` wraps it.
    let start = usize::try_from(addr).map_err(|_| Errno::EINVAL)?;
    let rounded = usize::try_from(len)
        .ok()
        .and_then(|len| len.checked_add(PAGE_SIZE as usize - 1))
        .map(|len| len & !(PAGE_SIZE as usize - 1))
        .ok_or(Errno::EINVAL)?;
    if start.checked_add(rounded).is_none() {
        return Err(Errno::EINVAL);
    }
    if rounded == 0 {
        return Ok(0);
    }
    process
        .space()
        .advise(addr, rounded as u64, advice)
        .map(|()| 0)
        .map_err(|declined| match declined {
            Declined::Unmapped => Errno::ENOMEM,
            Declined::Invalid => Errno::EINVAL,
            Declined::Denied => Errno::EACCES,
            Declined::Unsupported => Errno::EOPNOTSUPP,
            Declined::NoMemory => Errno::EAGAIN,
        })
}

/// `munmap`.
///
/// Unmapping a range that is only partly mapped is not an error: Linux removes
/// what is there and succeeds, and a libc freeing an arena relies on it.
pub(crate) fn sys_munmap(process: &Process, addr: u64, len: u64) -> Result<usize, Errno> {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let len = pages_for(len)?;
    let _layout = process.space().layout();
    process.space().unmap(addr, len).map_err(refused)?;
    Ok(0)
}

/// `mprotect`.
///
/// Load-bearing, and one of only three calls a static binary cannot survive
/// losing: a libc applies `RELRO` with it after relocation and aborts if that
/// fails.
///
/// The checks run in Linux's order, because the order decides which error a
/// bad call gets: both growth flags, then alignment, then a zero length --
/// which succeeds before `prot` is even looked at -- then wrapping, then the
/// protection bits. `PROT_SEM` is accepted and means nothing, as on Linux.
///
/// `PROT_GROWSDOWN` and `PROT_GROWSUP` extend the change to the start or end of
/// a region that grows. No region here grows -- the stack is a fixed
/// reservation -- and Linux answers either flag on a region that does not grow
/// with `EINVAL`, which is what they get.
pub(crate) fn sys_mprotect(
    process: &Process,
    addr: u64,
    len: u64,
    prot: u32,
) -> Result<usize, Errno> {
    const GROWS: u32 = PROT_GROWSDOWN | PROT_GROWSUP;
    if prot & GROWS == GROWS || !addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    let len = pages_for(len)?;
    let vma = protection(prot & !(PROT_SEM | GROWS))?;
    if prot & GROWS != 0 {
        return Err(Errno::EINVAL);
    }
    let _layout = process.space().layout();
    if vma.execute && maps_noexec_file(process, addr, len)? {
        return Err(Errno::EACCES);
    }
    process
        .space()
        .protect(addr, len, vma)
        .map_err(|error| match error {
            // A range that is not wholly mapped, or runs out of the user half,
            // is `ENOMEM` from `mprotect`, not `EINVAL`.
            SpaceError::NotUserRange(_) | SpaceError::BadRange => Errno::ENOMEM,
            // A region `vmo_map` made, whose protection its handle's rights
            // decided: Linux's answer for a mapping the caller may not change.
            SpaceError::Refused(_) => Errno::EACCES,
            other => refused(other),
        })?;
    Ok(0)
}

/// Whether any region in `[addr, addr + len)` maps a file on a `noexec`
/// mount: what Linux's cleared `VM_MAYEXEC` refuses `mprotect(PROT_EXEC)`
/// on with `EACCES`, so a mapping `mmap` could not have made executable is
/// not made so afterwards.
///
/// # Errors
///
/// `ENOMEM` when there is no memory to list the regions' files in.
fn maps_noexec_file(process: &Process, addr: u64, len: u64) -> Result<bool, Errno> {
    let end = addr.saturating_add(len);
    let mut ids = alloc::vec::Vec::new();
    let listed = process.space().with_regions(|regions| {
        for region in regions {
            if region.start < end
                && addr < region.end
                && let Some((id, _)) = region.file
            {
                ids.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
                ids.push(id);
            }
        }
        Ok(())
    });
    listed?;
    Ok(ids.into_iter().any(|id| {
        process
            .space()
            .mapped_file(id)
            .and_then(|file| file.downcast::<OpenFile>().ok())
            .is_some_and(|file| file.location().mount.no_exec())
    }))
}

/// `mremap`.
///
/// What glibc's `realloc` does with a block too large for its heap, so what
/// matters most is that the contents arrive: a block that moved and came
/// back zeroed is a program corrupted far from here. The address space moves
/// the pages rather than copying them; see [`crate::user::space::AddressSpace::remap`].
///
/// # What is refused, and in what order
///
/// Linux's order, from `mm/mremap.c`: unknown flags, then `MREMAP_FIXED`
/// without `MREMAP_MAYMOVE`, then an unaligned old address, then a new length
/// of zero -- all `EINVAL`. `MREMAP_DONTUNMAP` is among the unknown flags,
/// which is what a kernel older than 5.7 answers and what a program must
/// already handle.
///
/// An old length of zero is `EINVAL` too. On Linux it asks for a second
/// mapping of the same pages of a *shared* mapping, which nothing here can
/// make yet; refusing is what Linux does for a private one.
///
/// A fixed destination is then `EINVAL` if unaligned, past the top of user
/// space, or overlapping the old range; an old range that is not mapped is
/// `EFAULT`; and only after both is a destination below `MMAP_MIN_ADDR`
/// `EPERM`, where Linux's `get_unmapped_area` calls `security_mmap_addr`.
pub(crate) fn sys_mremap(
    process: &Process,
    old_addr: u64,
    old_size: u64,
    new_size: u64,
    flags: u32,
    new_addr: u64,
) -> Result<usize, Errno> {
    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & MREMAP_FIXED != 0 && flags & MREMAP_MAYMOVE == 0 {
        return Err(Errno::EINVAL);
    }
    if !old_addr.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    // A length that wraps when rounded is as unusable as zero, and Linux's
    // `PAGE_ALIGN` makes it zero.
    let old_len = pages_for(old_size).map_err(|_| Errno::EINVAL)?;
    let new_len = pages_for(new_size).map_err(|_| Errno::EINVAL)?;

    let _layout = process.space().layout();
    let destination = if flags & MREMAP_FIXED != 0 {
        if !new_addr.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        // `check_mremap_params` goes on to refuse a destination running past
        // the top of user space, then one overlapping the old range: both
        // `EINVAL`, and both before any mapping is looked at.
        let new_end = new_addr
            .checked_add(new_len)
            .filter(|&end| end <= USER_VIRT_END)
            .ok_or(Errno::EINVAL)?;
        if new_addr < old_addr.saturating_add(old_len) && old_addr < new_end {
            return Err(Errno::EINVAL);
        }
        // Below the floor is `EPERM`, as from `mmap`, but later in the call.
        // Linux looks up the old mapping (`EFAULT`), unmaps the destination,
        // and only then reaches `get_unmapped_area`, whose
        // `security_mmap_addr` refuses the address. The old mapping is looked
        // for first here too; the destination is left alone, so a refused
        // call changes nothing.
        if new_addr < MMAP_MIN_ADDR {
            let old_end = old_addr.saturating_add(old_len);
            let mapped = process.space().with_regions(|mut regions| {
                // Through the reference: `any` needs a sized iterator.
                Iterator::any(&mut regions, |region| {
                    region.start <= old_addr && old_end <= region.end
                })
            });
            return Err(if mapped { Errno::EPERM } else { Errno::EFAULT });
        }
        Destination::Fixed(new_addr)
    } else if flags & MREMAP_MAYMOVE != 0 {
        Destination::Anywhere
    } else {
        Destination::InPlace
    };
    process
        .space()
        .remap(old_addr, old_len, new_len, destination)
        .map(usize_of)
        .map_err(refused)
}

/// `brk`.
///
/// Returns the break rather than an error, always — see
/// [`Process::set_break`] for why reporting `-ENOMEM` here would be worse than
/// useless.
pub(crate) fn sys_brk(process: &Process, want: u64) -> Result<usize, Errno> {
    Ok(usize_of(process.set_break(want)))
}

/// An address as the return register carries it.
///
/// The cast cannot lose data on any target Ferrix builds for: a user address
/// is below `USER_VIRT_END`, which is at most the pointer width.
fn usize_of(address: u64) -> usize {
    usize::try_from(address).unwrap_or(usize::MAX)
}
