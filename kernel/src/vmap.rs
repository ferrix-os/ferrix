//! The kernel's dynamic virtual address space, and the stacks built on it.
//!
//! Stage 2 of `docs/ROADMAP.md`. `mm` answers "where is this physical frame
//! readable" with the direct map, which is a constant offset and needs no
//! allocator at all. This module answers the other question — "give me a
//! *range of addresses*, and let me decide later what goes behind it" — which
//! is what a device window, a non-contiguous buffer and a guard-paged stack
//! all need and none of which the direct map can express.
//!
//! # Why the arena is a `ferrix_vma::AddressSpace`
//!
//! Because it is the same problem. A kernel arena and a process address space
//! are both a sorted set of non-overlapping ranges reshaped by insert, remove
//! and protect. `libs/vma` was written for stage 6 and is already host-tested
//! against sixty cases; using it here means stage 6 inherits an arena that has
//! been running on a real machine for four stages rather than one written the
//! week it is needed — and the bugs are found by the kernel that boots on
//! every commit, not by the first process to call `mmap`.
//!
//! What the arena cannot do on its own is remember *allocations*, only
//! *ranges*: two adjacent anonymous regions with equal permissions are merged
//! into one, which is exactly right for a process — Linux does it, and without
//! it an `mprotect` loop leaks regions — and exactly wrong for an allocator,
//! which has to hand back precisely what it handed out. So the identity of
//! each allocation lives in a second map beside the arena, keyed by the
//! address the caller holds, and the arena is left to do the thing it is good
//! at.
//!
//! # Guard pages are not decoration
//!
//! Every allocation reserves one unmapped page on each side, *inside* the
//! range the arena hands out, so the guard cannot be given to anybody else.
//! The one below a kernel stack is the reason this module exists at all: a
//! stack that overflows into the allocation beneath it corrupts unrelated
//! memory and the symptom appears somewhere else entirely, possibly much
//! later. A stack that overflows into an unmapped page takes a page fault at
//! the instruction that did it.
//!
//! The guard is *reserved* rather than *mapped without permissions* because an
//! unmapped page costs nothing — no frame, no descriptor — and faults just as
//! hard.

use alloc::collections::BTreeMap;

use ferrix_bootinfo::{KERNEL_VMAP_BASE, KERNEL_VMAP_SIZE, PAGE_SIZE};
use ferrix_paging::MapFlags;
use ferrix_sync::IrqSpinLock;
use ferrix_vma::{AddressSpace, Backing, PageRange, VmaFlags};

use crate::mm;

/// Address space below the arena, left to the fixed windows early boot placed
/// there before there was an allocator: the `AArch64` console at offset zero,
/// the framebuffer, and the on-demand window stage 3 faults into.
///
/// Four gibibytes for a handful of windows is extravagant, and deliberately
/// so. The alternative is the arena handing out an address one of those
/// windows already owns, which does not fail — it succeeds, over the top of
/// the console.
const RESERVED_LOW: u64 = 0x1_0000_0000;

/// Where the arena starts.
pub(crate) const ARENA_BASE: u64 = KERNEL_VMAP_BASE + RESERVED_LOW;

/// Where it ends: the rest of the kernel's dynamic area.
pub(crate) const ARENA_END: u64 = KERNEL_VMAP_BASE + KERNEL_VMAP_SIZE;

/// Unmapped pages on each side of every allocation.
const GUARD_PAGES: u64 = 1;

/// Bytes of guard, on each side.
const GUARD_BYTES: u64 = GUARD_PAGES * PAGE_SIZE;

/// Why an allocation could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VmapError {
    /// [`init`] has not run, so there is no arena yet.
    NotReady,
    /// A zero, unaligned or absurd length.
    BadLength(u64),
    /// The arena has no free range of that size left.
    NoAddressSpace(u64),
    /// The frame allocator could not supply a page.
    OutOfMemory,
    /// The page tables refused the mapping.
    MapFailed(ferrix_paging::MapError),
    /// [`free`] was given an address the arena never handed out.
    NotAllocated(u64),
}

impl core::fmt::Display for VmapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VmapError::NotReady => f.write_str("the vmap arena is not up yet"),
            VmapError::BadLength(len) => write!(f, "{len} is not a length a mapping can have"),
            VmapError::NoAddressSpace(len) => {
                write!(f, "no free range of {len} bytes in the kernel arena")
            }
            VmapError::OutOfMemory => f.write_str("no frame available"),
            VmapError::MapFailed(error) => write!(f, "the mapping was refused: {error}"),
            VmapError::NotAllocated(at) => write!(f, "{at:#x} was not allocated by vmap"),
        }
    }
}

/// What is behind an allocation, and so what freeing it has to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Frames from the buddy allocator, to be given back.
    Anonymous,
    /// A physical aperture that was never allocated, so there is nothing to
    /// give back — and handing the buddy allocator a frame number that is
    /// really a `PCI` window would corrupt the per-frame array.
    Device,
}

/// One live allocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Allocation {
    /// The guarded span, which is what the arena holds.
    span: PageRange,
    /// Usable bytes, which is the span less both guards.
    len: u64,
    /// What freeing it has to do.
    kind: Kind,
}

/// The arena and the allocations it has handed out.
#[derive(Debug)]
struct Arena {
    /// Free and reserved address space.
    space: AddressSpace,
    /// Live allocations, keyed by the address the caller holds.
    live: BTreeMap<u64, Allocation>,
}

/// The arena, once [`init`] has run.
///
/// Interrupt-masking, and that is not caution for its own sake. A plain
/// spin lock taken by a handler that interrupted its own holder deadlocks:
/// the handler spins for a release that cannot happen until the handler
/// returns. Nothing in an interrupt handler maps address space *today*, but
/// "today" is a poor thing to build a lock discipline on, and the cost is a
/// `cli` and an `sti` around a few hundred instructions.
static ARENA: IrqSpinLock<Option<Arena>, crate::arch::Irq> = IrqSpinLock::new(None);

/// Bring up the arena.
///
/// Must run after `mm::init`: an `AddressSpace` is a `Vec`, and there is no
/// heap before then.
///
/// # Errors
///
/// Only if the window constants above are inconsistent, which is a bug in this
/// file rather than a condition a machine can produce.
pub(crate) fn init() -> Result<(), VmapError> {
    let space =
        AddressSpace::new(ARENA_BASE, ARENA_END).map_err(|_| VmapError::BadLength(ARENA_END))?;
    *ARENA.lock() = Some(Arena {
        space,
        live: BTreeMap::new(),
    });
    Ok(())
}

/// An allocation as its owner sees it: the usable range, guards excluded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Mapping {
    /// First usable address.
    pub(crate) base: u64,
    /// Usable bytes.
    pub(crate) len: u64,
}

impl Mapping {
    /// One past the last usable byte.
    pub(crate) const fn end(&self) -> u64 {
        self.base + self.len
    }
}

/// Reserve `len` usable bytes with a guard page on each side.
fn reserve(len: u64, flags: VmaFlags, kind: Kind) -> Result<Mapping, VmapError> {
    if len == 0 || !len.is_multiple_of(PAGE_SIZE) || len > ARENA_END - ARENA_BASE - 2 * GUARD_BYTES
    {
        return Err(VmapError::BadLength(len));
    }
    let span_bytes = len + 2 * GUARD_BYTES;

    let mut locked = ARENA.lock();
    let arena = locked.as_mut().ok_or(VmapError::NotReady)?;

    let at = arena
        .space
        .find_free(span_bytes, PAGE_SIZE, None)
        .ok_or(VmapError::NoAddressSpace(span_bytes))?;
    let span = PageRange::from_len(at, span_bytes).map_err(|_| VmapError::BadLength(span_bytes))?;
    arena
        .space
        .insert(span, flags, Backing::Anonymous)
        .map_err(|_| VmapError::NoAddressSpace(span_bytes))?;

    let base = at + GUARD_BYTES;
    let _ = arena.live.insert(base, Allocation { span, len, kind });
    Ok(Mapping { base, len })
}

/// Give a reservation back, and say what was behind it.
fn release(base: u64) -> Result<Allocation, VmapError> {
    let mut locked = ARENA.lock();
    let arena = locked.as_mut().ok_or(VmapError::NotReady)?;

    // Keyed by the address the caller holds, so freeing a pointer somebody did
    // arithmetic on is reported rather than half-executed.
    let allocation = arena
        .live
        .remove(&base)
        .ok_or(VmapError::NotAllocated(base))?;
    let _ = arena
        .space
        .remove(allocation.span)
        .map_err(|_| VmapError::NotAllocated(base))?;
    Ok(allocation)
}

/// Map `pages` of fresh anonymous memory into the arena.
///
/// The frames are taken one at a time and need not be contiguous. That is the
/// whole difference between this and the direct map, and the reason a large
/// buffer can be allocated on a fragmented machine at all.
pub(crate) fn allocate(pages: u64, flags: MapFlags) -> Result<Mapping, VmapError> {
    let len = pages
        .checked_mul(PAGE_SIZE)
        .ok_or(VmapError::BadLength(pages))?;
    let mapping = reserve(len, vma_flags(flags), Kind::Anonymous)?;

    for page in 0..pages {
        let at = mapping.base + page * PAGE_SIZE;
        let Some(frame) = mm::allocate_frames(0) else {
            unwind(mapping, page);
            return Err(VmapError::OutOfMemory);
        };
        mm::zero_frame(frame);

        if let Err(error) = mm::map_kernel(at, frame * PAGE_SIZE, PAGE_SIZE, flags) {
            mm::deallocate_frames(frame, 0);
            unwind(mapping, page);
            return Err(VmapError::MapFailed(error));
        }
    }

    Ok(mapping)
}

/// Undo the first `pages` pages of an [`allocate`] that failed part way.
///
/// A partial mapping left behind would be address space nobody owns holding
/// frames nobody frees — and the failure that produced it is precisely the one
/// where neither can be spared.
fn unwind(mapping: Mapping, pages: u64) {
    if pages > 0 {
        let _ = mm::unmap_kernel(mapping.base, pages * PAGE_SIZE, mm::deallocate_frames);
    }
    let _ = release(mapping.base);
}

/// Unmap an allocation and give its frames back.
///
/// # Errors
///
/// [`VmapError::NotAllocated`] for an address the arena did not hand out.
pub(crate) fn free(base: u64) -> Result<(), VmapError> {
    let allocation = release(base)?;
    let removed = match allocation.kind {
        Kind::Anonymous => mm::unmap_kernel(base, allocation.len, mm::deallocate_frames),
        Kind::Device => mm::unmap_kernel(base, allocation.len, |_, _| {}),
    };
    let _ = removed.map_err(VmapError::MapFailed)?;
    Ok(())
}

/// Map `len` bytes of device registers at `phys` and return where they landed.
///
/// The offset within the page is preserved, so a register block that does not
/// begin on a page boundary still reads correctly — the usual case for an I/O
/// APIC or a GIC redistributor.
pub(crate) fn map_device(phys: u64, len: u64) -> Result<u64, VmapError> {
    let offset = phys % PAGE_SIZE;
    let base = phys - offset;
    let span = (len + offset).next_multiple_of(PAGE_SIZE);

    let mapping = reserve(span, VmaFlags::READ_WRITE, Kind::Device)?;
    mm::map_kernel(mapping.base, base, span, MapFlags::KERNEL_DEVICE).map_err(|error| {
        let _ = release(mapping.base);
        VmapError::MapFailed(error)
    })?;

    Ok(mapping.base + offset)
}

/// Unmap a device window taken with [`map_device`].
///
/// Takes the address `map_device` returned, page offset included, so a caller
/// does not have to remember how its register block was aligned.
pub(crate) fn unmap_device(at: u64) -> Result<(), VmapError> {
    free(at - at % PAGE_SIZE)
}

/// What the arena has handed out, for the boot report.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Usage {
    /// Live allocations.
    pub(crate) allocations: usize,
    /// Address space they cover, guards included.
    pub(crate) bytes: u64,
}

/// What the arena has handed out.
pub(crate) fn usage() -> Usage {
    let locked = ARENA.lock();
    locked.as_ref().map_or(Usage::default(), |arena| Usage {
        allocations: arena.live.len(),
        bytes: arena.space.total_mapped(),
    })
}

/// The arena's own consistency check, for the boot self-test.
///
/// Two claims, and the second is the one a merge would break: the arena's
/// regions are sorted and non-overlapping, and every live allocation's span is
/// still inside it.
pub(crate) fn check_invariants() -> Result<(), &'static str> {
    let locked = ARENA.lock();
    let arena = locked.as_ref().ok_or("the vmap arena is not up")?;
    arena.space.check_invariants()?;

    for allocation in arena.live.values() {
        if arena.space.find(allocation.span.start()).is_none() {
            return Err("a live allocation is not reserved in the arena");
        }
    }
    Ok(())
}

/// The `libs/vma` spelling of a set of page table flags.
///
/// Only the three permission bits carry over. The arena does not care whether
/// a mapping is cacheable or global — those say how the hardware reaches the
/// memory, not what the range is for — and a `VmaFlags` carrying them would
/// have to be kept in step with an encoding it has no business knowing about.
const fn vma_flags(flags: MapFlags) -> VmaFlags {
    VmaFlags {
        read: flags.read,
        write: flags.write,
        execute: flags.execute,
        shared: false,
        grows_down: false,
        locked: false,
    }
}

// ---------------------------------------------------------------------------
// Kernel stacks
// ---------------------------------------------------------------------------

/// Pages in a kernel stack, not counting the guards.
///
/// Sixteen kibibytes, which is what Linux uses on both of these architectures.
/// It is not generous: the trap path saves a frame, a nested interrupt saves
/// another, and formatting a line for the console is surprisingly deep. The
/// guard page below is what turns "not generous" from a corruption into a
/// fault.
pub(crate) const STACK_PAGES: u64 = 4;

/// A kernel stack, guard-paged at both ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Stack {
    /// Lowest usable address. The page below it is unmapped, and that is what
    /// an overflow hits.
    pub(crate) base: u64,
    /// Initial stack pointer: one past the highest usable byte, because both
    /// architectures push downwards from it.
    pub(crate) top: u64,
}

impl Stack {
    /// Usable bytes.
    pub(crate) const fn len(&self) -> u64 {
        self.top - self.base
    }
}

/// Allocate a guard-paged kernel stack.
pub(crate) fn allocate_stack() -> Result<Stack, VmapError> {
    let mapping = allocate(STACK_PAGES, MapFlags::KERNEL_DATA)?;
    Ok(Stack {
        base: mapping.base,
        top: mapping.end(),
    })
}

/// Give a kernel stack back.
///
/// # Safety
///
/// No CPU may be running on it. Nothing in the type system says so, and
/// freeing the stack under a running task unmaps the memory holding its own
/// return address.
pub(crate) unsafe fn free_stack(stack: Stack) -> Result<(), VmapError> {
    free(stack.base)
}
