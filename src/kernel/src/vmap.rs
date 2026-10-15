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
//! and protect. `src/lib/kernel/vma` was written for stage 6 and is already host-tested
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
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::{KERNEL_VMAP_BASE, KERNEL_VMAP_RESERVED, KERNEL_VMAP_SIZE, PAGE_SIZE};
use ferrix_paging::MapFlags;
use ferrix_sync::IrqSpinLock;
use ferrix_vma::{AddressSpace, Backing, PageRange, VmaError, VmaFlags};

use crate::fallible;
use crate::mm;

// The address space below the arena is left to the fixed windows early boot
// places there before there is an allocator: the Arm consoles at offset zero,
// the framebuffer, and the on-demand window stage 3 faults into.
//
// They sit at fractions of `KERNEL_VMAP_RESERVED` rather than at literal
// offsets, because how much that is depends on the word width: four gibibytes
// on the 64-bit pair — extravagant, and deliberately so — and 64 MiB of a
// 32-bit kernel's 512 MiB. Either way the alternative is the arena handing out
// an address one of those windows already owns, which does not fail; it
// succeeds, over the top of the console.

/// Where early boot maps a framebuffer, when firmware left one.
pub const FRAMEBUFFER_WINDOW: u64 = KERNEL_VMAP_BASE + KERNEL_VMAP_RESERVED / 16;

/// How much the framebuffer window holds: up to where the demand window starts.
pub const FRAMEBUFFER_WINDOW_SIZE: u64 = DEMAND_WINDOW - FRAMEBUFFER_WINDOW;

/// Where stage 3's on-demand window begins.
pub const DEMAND_WINDOW: u64 = KERNEL_VMAP_BASE + KERNEL_VMAP_RESERVED / 8;

/// Where the arena starts.
pub(crate) const ARENA_BASE: u64 = KERNEL_VMAP_BASE + KERNEL_VMAP_RESERVED;

/// Where its region ends: the rest of the kernel's dynamic area. The arena
/// itself ends at or below this, where the loader put its top
/// ([`ferrix_bootinfo::Kaslr::vmap_end`]), since the search that places
/// allocations starts from the top.
pub(crate) const ARENA_END: u64 = KERNEL_VMAP_BASE + KERNEL_VMAP_SIZE;

/// Unmapped pages on each side of every allocation.
const GUARD_PAGES: u64 = 1;

/// Bytes of guard, on each side.
const GUARD_BYTES: u64 = GUARD_PAGES * PAGE_SIZE;

/// Why an allocation could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmapError {
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
    /// [`map_device`] was asked for a window over the kernel's own image,
    /// at this physical address ([`mm::overlaps_image`]).
    KernelImage(u64),
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
            VmapError::KernelImage(at) => {
                write!(f, "{at:#x} is the kernel's own image, not a device")
            }
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
    Device {
        /// The page-aligned physical address mapped.
        phys: u64,
    },
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

/// Spans [`release_span`] could not give back for want of memory to split
/// the region around them.
static SPANS_KEPT: AtomicU64 = AtomicU64::new(0);

/// The arena, once [`init`] has run.
///
/// Interrupt-masking, and that is not caution for its own sake. A plain
/// spin lock taken by a handler that interrupted its own holder deadlocks:
/// the handler spins for a release that cannot happen until the handler
/// returns. Nothing in an interrupt handler maps address space *today*, but
/// "today" is a poor thing to build a lock discipline on, and the cost is a
/// `cli` and an `sti` around a few hundred instructions.
static ARENA: IrqSpinLock<Option<Arena>, crate::arch::Irq> = IrqSpinLock::new(None);

/// Bring up the arena, with its top at `top`: [`ARENA_END`], or below it
/// where the loader moved it, so that kernel stacks and device windows are
/// not at the same addresses every boot (KASLR,
/// `docs/certification/SPECULATION.md` §6). `BootInfo::validate` has already
/// held `top` to the places the layout allows.
///
/// Must run after `mm::init`: an `AddressSpace` is a `Vec`, and there is no
/// heap before then.
///
/// # Errors
///
/// Only if the window constants above are inconsistent, or `top` leaves the
/// arena empty, which is a bug rather than a condition a machine can produce.
pub fn init(top: u64) -> Result<(), VmapError> {
    let top = top.min(ARENA_END);
    let space = AddressSpace::new(ARENA_BASE, top).map_err(|_| VmapError::BadLength(top))?;
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

/// A zeroed kernel buffer on pages that need not be contiguous, freed when it
/// is dropped.
///
/// For whole files read into the kernel, which is what loading a program is.
/// The heap takes a large allocation from the buddy allocator in one piece,
/// and its largest piece is `2^MAX_ORDER` frames, four mebibytes: a program
/// larger than that, btop at 4.6 MiB the first, could not be read at all,
/// however much memory was free. A buffer here is limited by free frames and
/// by the arena instead.
#[derive(Debug)]
pub struct Buffer {
    /// The pages, or `None` for an empty buffer, which needs none.
    mapping: Option<Mapping>,
    /// Bytes in use, at most the mapping's length.
    len: usize,
}

impl Buffer {
    /// `len` zeroed bytes.
    ///
    /// # Errors
    ///
    /// [`allocate`]'s, when the frames or the addresses for it are not there.
    pub fn zeroed(len: usize) -> Result<Buffer, VmapError> {
        if len == 0 {
            return Ok(Buffer { mapping: None, len });
        }
        let bytes = u64::try_from(len).map_err(|_| VmapError::BadLength(u64::MAX))?;
        let mapping = allocate(bytes.div_ceil(PAGE_SIZE), MapFlags::KERNEL_DATA)?;
        Ok(Buffer {
            mapping: Some(mapping),
            len,
        })
    }

    /// Keep only the first `len` bytes. The pages stay until the drop.
    pub fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
}

impl core::ops::Deref for Buffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self.mapping {
            None => &[],
            // SAFETY: (KMEM) the mapping is `len` bytes or more of readable kernel
            // memory this buffer alone owns, zeroed when it was mapped, and it
            // stays mapped until the drop.
            Some(mapping) => unsafe {
                core::slice::from_raw_parts(mapping.base as usize as *const u8, self.len)
            },
        }
    }
}

impl core::ops::DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self.mapping {
            None => &mut [],
            // SAFETY: (KMEM) as `deref`, and writable; `&mut self` makes this the
            // only reference.
            Some(mapping) => unsafe {
                core::slice::from_raw_parts_mut(mapping.base as usize as *mut u8, self.len)
            },
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if let Some(mapping) = self.mapping.take() {
            let _ = free(mapping.base);
        }
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
    // Room first for the record of the allocation, so nothing is changed
    // unless all of it can be.
    let held = fallible::reserve().map_err(|_| VmapError::OutOfMemory)?;
    arena
        .space
        // Private, and offset by its own address: the convention `Backing`
        // documents, which keeps two adjacent spans contiguous so the arena
        // merges them exactly as it did before anonymous memory named an
        // object. Allocation identity does not depend on that -- it lives in
        // `arena.live` beside the arena, for the reason this module's header
        // gives -- but the region count does.
        // FALLIBLE: the arena map's insert refuses with `VmaError::NoMemory`.
        .insert(span, flags, Backing::Anonymous { id: 0, offset: at })
        .map_err(|error| match error {
            VmaError::NoMemory => VmapError::OutOfMemory,
            _ => VmapError::NoAddressSpace(span_bytes),
        })?;

    let base = at + GUARD_BYTES;
    let _ = fallible::insert_held(&held, &mut arena.live, base, Allocation { span, len, kind });
    Ok(Mapping { base, len })
}

/// Give a reservation back, and say what was behind it.
///
/// For a reservation nothing was ever mapped into. A live mapping goes through
/// [`claim`] and [`release_span`] instead, which are the same two steps with
/// the unmapping in between.
fn release(base: u64) -> Result<Allocation, VmapError> {
    let allocation = claim(base)?;
    release_span(allocation.span)?;
    Ok(allocation)
}

/// Take an allocation out of the live set, leaving its address *still
/// reserved*.
///
/// # Why this is not just `release`
///
/// The address must not become available again until nothing is mapped at it.
/// Freeing the range first and unmapping afterwards leaves a window in which
/// another processor can be handed the same address and try to map it, and
/// the symptom is the mapper refusing a perfectly ordinary allocation:
///
/// ```text
/// no stack for worker: the mapping was refused: VirtAddr(...) is already mapped
/// ```
///
/// which is what stage 5 hit the moment work stealing started — a thousand
/// tasks spawning on one processor while three others ran them to completion
/// and reaped their stacks.
///
/// The unmapping cannot happen under the arena lock either: it invalidates
/// other processors' translations and waits for them to answer, and a
/// processor spinning for this lock with interrupts masked cannot answer. So
/// the two steps are separate, with the lock dropped in between and the
/// address reserved throughout.
fn claim(base: u64) -> Result<Allocation, VmapError> {
    let mut locked = ARENA.lock();
    let arena = locked.as_mut().ok_or(VmapError::NotReady)?;

    // Keyed by the address the caller holds, so freeing a pointer somebody did
    // arithmetic on is reported rather than half-executed.
    arena
        .live
        .remove(&base)
        .ok_or(VmapError::NotAllocated(base))
}

/// Make a claimed span available again, once nothing is mapped in it.
fn release_span(span: PageRange) -> Result<(), VmapError> {
    let mut locked = ARENA.lock();
    let arena = locked.as_mut().ok_or(VmapError::NotReady)?;
    // Quietly: a span goes back as something is torn down, and the list of
    // what went would be the one allocation it made. Giving back a span from
    // the middle of a merged region splits it, which can need the map to
    // grow; with no memory for that the span stays reserved -- address space
    // lost, not memory, since its frames are already free -- and is counted.
    match arena.space.remove_quietly(span) {
        Ok(()) => Ok(()),
        Err(VmaError::NoMemory) => {
            let _ = SPANS_KEPT.fetch_add(1, Ordering::Relaxed);
            Err(VmapError::OutOfMemory)
        }
        Err(_) => Err(VmapError::NotAllocated(span.start())),
    }
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
    // Out of the live set, but still reserved: see `claim` for why the address
    // may not be handed out again until the unmapping below has finished.
    let allocation = claim(base)?;
    let removed = match allocation.kind {
        Kind::Anonymous => mm::unmap_kernel(base, allocation.len, mm::deallocate_frames),
        Kind::Device { .. } => mm::unmap_kernel(base, allocation.len, |_, _| {}),
    };
    // The span goes back whether or not the unmapping succeeded: an address
    // nobody can reuse is a leak, and the mapper's error is reported either
    // way.
    let released = release_span(allocation.span);
    let _ = removed.map_err(VmapError::MapFailed)?;
    released
}

/// Map `len` bytes of device registers at `phys` and return where they landed.
///
/// The offset within the page is preserved, so a register block that does not
/// begin on a page boundary still reads correctly — the usual case for an I/O
/// APIC or a GIC redistributor.
///
/// # Errors
///
/// [`VmapError::BadLength`] for a range whose end, rounded out to a page,
/// is past the top of the address space -- `phys + len` wrapping, which
/// would otherwise size the window from the wrapped sum, or stop the kernel
/// on the overflow check; [`VmapError::KernelImage`] for a range that
/// touches the kernel's own image, whose text and read-only data have no
/// writable mapping anywhere (`mm::check::check_sealed_image`) and which holds no
/// device's registers; and whatever [`reserve`] or the mapper refuses.
pub fn map_device(phys: u64, len: u64) -> Result<u64, VmapError> {
    let offset = phys % PAGE_SIZE;
    let base = phys - offset;
    let span = len
        .checked_add(offset)
        .and_then(|bytes| bytes.checked_next_multiple_of(PAGE_SIZE))
        .filter(|&span| base.checked_add(span).is_some())
        .ok_or(VmapError::BadLength(len))?;
    if mm::overlaps_image(base, span) {
        return Err(VmapError::KernelImage(phys));
    }

    let mapping = reserve(span, VmaFlags::READ_WRITE, Kind::Device { phys: base })?;
    if let Err(error) = map_reserved(mapping, base, span) {
        // Only now that nothing is mapped in it: see `claim` for why an
        // address may not become available while something still is.
        let _ = release(mapping.base);
        return Err(error);
    }

    Ok(mapping.base + offset)
}

/// Map a device aperture over a reservation, or leave nothing of it mapped.
///
/// `map_range` has no rollback. A window of several pages that fails part way
/// — a page table it could not allocate, or a mapping already in the way —
/// has its first pages installed and the rest not, and giving the span back
/// in that state hands the next caller an address that is still mapped onto
/// somebody's registers.
fn map_reserved(mapping: Mapping, phys: u64, span: u64) -> Result<(), VmapError> {
    mm::map_kernel(mapping.base, phys, span, MapFlags::KERNEL_DEVICE).map_err(|error| {
        unmap_installed(mapping.base, phys, span);
        VmapError::MapFailed(error)
    })
}

/// Unmap what a failed mapping of `span` bytes at `virt` onto `phys` did
/// install, and nothing else.
///
/// The mapper installs pages in address order, so what it installed is the
/// run from the start that translates to the addresses it was asked for. The
/// page that stopped it is not part of that run, and must not be unmapped: if
/// it stopped because something was already mapped there, that mapping is not
/// this call's to remove.
fn unmap_installed(virt: u64, phys: u64, span: u64) {
    let mut installed = 0;
    while installed < span && mm::translate(virt + installed) == Some(phys + installed) {
        installed += PAGE_SIZE;
    }
    if installed > 0 {
        // A device aperture: nothing behind it goes back to the allocator.
        let _ = mm::unmap_kernel(virt, installed, |_, _| {});
    }
}

/// A device mapping that fails part way leaves nothing of itself mapped, and
/// removes nothing it did not map.
///
/// The failure is made on purpose: a two-page window whose second page is
/// already mapped, so the mapper installs the first page and then stops. The
/// obstacle maps `phys`'s own page, not the page the window wants there, which
/// is how it stays distinguishable from a page the failed call installed.
///
/// Nothing is read or written through either mapping; `phys` is RAM the caller
/// knows, for the reason `check_device_windows` in `mm/check.rs` gives.
pub(crate) fn check_failed_device_map(phys: u64) -> Result<(), &'static str> {
    let aperture = phys & !(PAGE_SIZE - 1);
    let span = 2 * PAGE_SIZE;
    let mapping = reserve(span, VmaFlags::READ_WRITE, Kind::Device { phys: aperture })
        .map_err(|_| "no address space for the failed-mapping check")?;
    let second = mapping.base + PAGE_SIZE;
    if mm::map_kernel(second, aperture, PAGE_SIZE, MapFlags::KERNEL_DEVICE).is_err() {
        let _ = release(mapping.base);
        return Err("the failed-mapping check could not place its obstacle");
    }

    let outcome = map_reserved(mapping, aperture, span);
    let first_left = mm::translate(mapping.base).is_some();
    let obstacle_kept = mm::translate(second) == Some(aperture);

    // Clean up before judging, so a failure does not also leak the span.
    let _ = mm::unmap_kernel(second, PAGE_SIZE, |_, _| {});
    let _ = release(mapping.base);

    if outcome.is_ok() {
        return Err("a device mapping over an existing mapping succeeded");
    }
    if first_left {
        return Err("a failed device mapping left the pages it had mapped behind");
    }
    if !obstacle_kept {
        return Err("a failed device mapping removed a mapping it had not made");
    }
    Ok(())
}

/// Unmap a device window taken with [`map_device`].
///
/// Takes the address `map_device` returned, page offset included, so a caller
/// does not have to remember how its register block was aligned.
pub fn unmap_device(at: u64) -> Result<(), VmapError> {
    free(at - at % PAGE_SIZE)
}

/// Every device window mapped right now, as physical `start..end` ranges.
///
/// What stage 10 withholds from device apertures: the registers of every
/// controller the kernel drives through here — the local and I/O APICs, the
/// HPET, the GIC — are ones no driver may be given.
pub(crate) fn device_windows() -> Vec<(u64, u64)> {
    let locked = ARENA.lock();
    locked.as_ref().map_or_else(Vec::new, |arena| {
        arena
            .live
            .values()
            .filter_map(|allocation| match allocation.kind {
                Kind::Device { phys } => Some((phys, phys.saturating_add(allocation.len))),
                Kind::Anonymous => None,
            })
            // FATAL-ALLOC: boot only: stage 10 withholds these windows once, before any program runs.
            .collect()
    })
}

/// What the arena has handed out, for the boot report.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    /// Live allocations.
    pub allocations: usize,
    /// Address space they cover, guards included.
    pub bytes: u64,
}

/// What the arena has handed out.
pub fn usage() -> Usage {
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

/// The `src/lib/kernel/vma` spelling of a set of page table flags.
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
pub struct Stack {
    /// Lowest usable address. The page below it is unmapped, and that is what
    /// an overflow hits.
    pub base: u64,
    /// Initial stack pointer: one past the highest usable byte, because both
    /// architectures push downwards from it.
    pub top: u64,
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
/// (KMEM) No CPU may be running on it. Nothing in the type system says so, and
/// freeing the stack under a running task unmaps the memory holding its own
/// return address.
pub(crate) unsafe fn free_stack(stack: Stack) -> Result<(), VmapError> {
    free(stack.base)
}

/// Give several kernel stacks back under one shootdown.
///
/// What [`free_stack`] does for each, with the invalidation every processor
/// has to answer done once for all of them: see [`mm::unmap_kernel_all`].
///
/// # Safety
///
/// (KMEM) As [`free_stack`], for every one of them.
pub(crate) unsafe fn free_stacks(stacks: &[Stack]) -> Result<(), VmapError> {
    // Out of the live set first, all of them, so the addresses stay reserved
    // until the unmapping below has finished: see `claim`.
    let (Ok(mut ranges), Ok(mut spans)) = (
        fallible::try_with_capacity(stacks.len()),
        fallible::try_with_capacity(stacks.len()),
    ) else {
        // No memory for one shootdown over all of them: one each, which
        // needs none.
        let mut first_error = None;
        for stack in stacks {
            if let Err(error) = free(stack.base) {
                first_error = first_error.or(Some(error));
            }
        }
        return first_error.map_or(Ok(()), Err);
    };
    let mut first_error = None;
    for stack in stacks {
        match claim(stack.base) {
            Ok(allocation) => {
                // A stack is anonymous memory; a device window is never a
                // stack, and the frames behind one must not go to the buddy
                // allocator. Freed on its own if one ever turns up here.
                if matches!(allocation.kind, Kind::Device { .. }) {
                    let _ = mm::unmap_kernel(stack.base, allocation.len, |_, _| {});
                } else {
                    let _ = fallible::push_within(&mut ranges, (stack.base, allocation.len));
                }
                let _ = fallible::push_within(&mut spans, allocation.span);
            }
            Err(error) => first_error = first_error.or(Some(error)),
        }
    }
    let removed = mm::unmap_kernel_all(&ranges, mm::deallocate_frames);
    // The spans go back whether or not the unmapping succeeded: an address
    // nobody can reuse is a leak, and the mapper's error is reported either
    // way.
    for span in spans {
        if let Err(error) = release_span(span) {
            first_error = first_error.or(Some(error));
        }
    }
    if let Err(error) = removed {
        first_error = first_error.or(Some(VmapError::MapFailed(error)));
    }
    first_error.map_or(Ok(()), Err)
}
