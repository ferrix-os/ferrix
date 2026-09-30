//! Physical memory and the kernel heap.
//!
//! Stage 2 of `docs/ROADMAP.md`. Two things happen here, and the second is what
//! the rest of the kernel is waiting for:
//!
//! 1. the buddy allocator is given every usable frame firmware reported, and
//! 2. `alloc` starts working, so `Box`, `Vec` and `BTreeMap` exist.
//!
//! Both allocators themselves live in `src/lib/` — see `ferrix_frame` and
//! `ferrix_heap` — where they are ordinary Rust that `cargo test`, Miri and a
//! fuzzer can drive. What is here is the part that genuinely needs a machine:
//! deciding where the per-frame array goes, and reaching physical memory
//! through the direct map.
//!
//! # The chicken and the egg
//!
//! The buddy allocator needs one [`PageEntry`] per frame, and that array has to
//! be allocated before there is an allocator. So it is carved out of the front
//! of the largest usable region firmware reported *that the direct map
//! reaches*, and that region is then handed to the allocator with the carved
//! part left out. The qualification matters only on a 32-bit machine with more
//! RAM than its direct map, and there it is the difference between an array
//! and a zeroed kernel image. After that, every
//! allocation goes through the allocator like anything else.
//!
//! # Locks
//!
//! Three globals, each behind an interrupt-masking lock, nesting in one fixed
//! order:
//!
//! * [`TABLES`] serialises every walk and change of the kernel's page tables.
//!   A mapping that needs a new table takes a frame for it, and an unmap
//!   writes down what it released in a `Vec`, so this is taken before both of
//!   the others.
//! * [`HEAP`] is the kernel heap. A slab that runs dry takes a page, so this is
//!   taken before [`FRAMES`] too.
//! * [`FRAMES`] is the buddy allocator, and the innermost lock in the kernel:
//!   nothing is taken while it is held.
//!
//! Interrupt-masking rather than plain, because the page fault handler maps
//! pages and so reaches the first and the last: a handler that interrupted a
//! CPU holding either would spin on its own CPU's lock. Masking interrupts does
//! not mask a *fault*, which is why nothing holding these touches the
//! on-demand window.
//!
//! One lock in front of each is the simplest correct shape, and the
//! architecture asks for more: per-CPU caches in front of the frame allocator
//! and the heap, so the common allocation takes no lock at all. Those need a
//! per-CPU area to live in, and come after it.

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::{BootView, MemKind, MemRegion, PAGE_SIZE, direct_map_runs};
use ferrix_frame::{Frame, Frames, PageEntry};
use ferrix_heap::{Backing, Heap, Request};
use ferrix_paging::coherence::{Clean, Unpublished, Walked};
use ferrix_paging::{Encoding, MapFlags, Mapper, PhysAddr, PhysMem, Released, VirtAddr};
use ferrix_sync::IrqSpinLock;

use crate::fallible;

pub(crate) mod check;
mod reserve;
mod unlinked;

pub(crate) use reserve::{
    Reserved, bypass_heap_in_sections, fill_reserve, refuse_reserve_fills, reserve, reserve_counts,
};
pub(crate) use unlinked::{Owner as TableOwner, UnlinkedTables, tables_kept};

/// Why memory could not be brought up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MemoryError {
    /// Firmware reported no usable memory at all.
    NoUsableMemory,
    /// No single usable region is large enough to hold the per-frame array.
    ///
    /// Carries the bytes needed. On a machine small enough for this, the array
    /// is most of what there is.
    NoRoomForPageArray(u64),
    /// The memory map's RAM falls into more separate runs than
    /// [`MOST_RAM_RUNS`], so the direct map's RAM cannot all be recorded.
    ///
    /// Carries how many runs there were. Refused rather than recorded in part:
    /// a run left out would make every copy through its frames fail.
    TooManyRamRuns(usize),
}

impl core::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MemoryError::NoUsableMemory => f.write_str("firmware reported no usable memory"),
            MemoryError::NoRoomForPageArray(bytes) => {
                write!(f, "no usable region holds the {bytes}-byte page array")
            }
            MemoryError::TooManyRamRuns(runs) => {
                write!(f, "RAM falls into {runs} runs, more than can be recorded")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------

/// The buddy allocator, once [`init`] has run.
///
/// The innermost lock in the kernel; the module documentation gives the order.
static FRAMES: IrqSpinLock<Option<Frames<'static>>, crate::arch::Irq> = IrqSpinLock::new(None);

/// The kernel heap.
static HEAP: IrqSpinLock<Heap, crate::arch::Irq> = IrqSpinLock::new(Heap::new());

/// Held for every walk and every change of the kernel's page tables.
///
/// Guards no data of its own: the tables are physical memory reached through
/// the direct map, and what this lock owns is the right to walk them.
/// [`with_tables`] and the boot sweeps' `check::sweep` are the only places that take it.
static TABLES: IrqSpinLock<(), crate::arch::Irq> = IrqSpinLock::new(());

/// Base of the direct map, learned from the hand-off.
///
/// An atomic rather than another `UnsafeCell`: it is written once and read from
/// every allocation, and there is no reason to spend an `unsafe` on a `u64`.
static PHYSMAP: AtomicU64 = AtomicU64::new(0);

/// The physical address that appears at [`PHYSMAP`]: the lowest RAM address,
/// which is zero on x86-64 and a gibibyte on QEMU's Arm machines.
static PHYSMAP_PHYS: AtomicU64 = AtomicU64::new(0);

/// How many separate runs of RAM [`RAM_RUNS`] can hold.
///
/// A memory map's RAM regions mostly touch, and merged they are a handful of
/// runs: one or two on a device-tree board, a few dozen on a PC whose firmware
/// scatters reserved ranges through it. A map with more is refused at
/// bring-up ([`MemoryError::TooManyRamRuns`]), not recorded in part.
pub(crate) const MOST_RAM_RUNS: usize = 128;

/// The runs of RAM the direct map translates, as `(first byte, one past the
/// last)`, lowest first; the first [`RAM_RUN_COUNT`] of them are meaningful.
///
/// Written once by [`record_ram`], on the boot processor before any other
/// starts, and read without a lock by [`direct_map_ram`], which the panic
/// path uses too. Atomics rather than a cell, so that neither side needs an
/// `unsafe`.
static RAM_RUNS: [(AtomicU64, AtomicU64); MOST_RAM_RUNS] =
    [const { (AtomicU64::new(0), AtomicU64::new(0)) }; MOST_RAM_RUNS];

/// How many of [`RAM_RUNS`] [`record_ram`] filled. Stored last, with release,
/// so a reader that sees a count sees the runs it counts.
static RAM_RUN_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The virtual address at which physical address `phys` is readable.
fn physmap(phys: u64) -> u64 {
    PHYSMAP.load(Ordering::Relaxed) + (phys - PHYSMAP_PHYS.load(Ordering::Relaxed))
}

/// The physical address behind a direct-map virtual address.
fn unmap(virt: u64) -> u64 {
    virt - PHYSMAP.load(Ordering::Relaxed) + PHYSMAP_PHYS.load(Ordering::Relaxed)
}

/// Run `body` with the frame allocator, or return `None` before [`init`].
fn with_frames<T>(body: impl FnOnce(&mut Frames<'static>) -> T) -> Option<T> {
    FRAMES.lock().as_mut().map(body)
}

/// Run `body` with the heap.
fn with_heap<T>(body: impl FnOnce(&mut Heap) -> T) -> T {
    body(&mut HEAP.lock())
}

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

/// What memory looks like once it is up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stats {
    /// Frames the buddy allocator was given.
    pub(crate) managed_frames: u64,
    /// Frames currently free.
    pub(crate) free_frames: u64,
    /// Bytes spent on the per-frame array.
    pub(crate) page_array_bytes: u64,
    /// Physical address the per-frame array was placed at.
    pub(crate) page_array_at: u64,
}

/// Bring up the frame allocator and the heap.
pub(crate) fn init(view: &BootView<'_>) -> Result<Stats, MemoryError> {
    PHYSMAP.store(view.raw().physmap_base, Ordering::Relaxed);
    PHYSMAP_PHYS.store(view.raw().physmap_phys, Ordering::Relaxed);
    ROOT_TABLE.store(view.raw().root_table_phys, Ordering::Relaxed);
    IMAGE_PHYS.store(view.raw().kernel_phys, Ordering::Relaxed);
    IMAGE_LEN.store(view.raw().kernel_len, Ordering::Relaxed);
    record_ram(view)?;

    // No higher than the direct map reaches: a frame the kernel cannot address
    // is not a frame it can hand out. Only a 32-bit machine with more RAM than
    // its direct map holds has any such frames, and `kmain` says how many.
    let highest = view
        .max_ram_address()
        .min(view.physmap_limit())
        .div_ceil(PAGE_SIZE);
    let lowest = lowest_ram_frame(view);
    if highest <= lowest {
        return Err(MemoryError::NoUsableMemory);
    }

    // One entry per frame of RAM -- from the *lowest* RAM frame, not from zero.
    // That distinction is worth an offset on every lookup: QEMU's AArch64
    // `virt` machine starts RAM at 1 GiB, so an array based at zero would spend
    // 4 MiB describing a gigabyte of nothing. It is the same array either way
    // on x86-64, where RAM does start at zero.
    //
    // This is still a flat array, which is the right shape up to a few hundred
    // gibibytes and the wrong one beyond: a machine with RAM in widely
    // separated banks wants the array split per bank. `Frames` takes a base
    // precisely so that change stays inside this function.
    let entries = Frames::entries_needed(lowest, highest);
    let bytes = (entries * size_of::<PageEntry>()) as u64;
    if view.usable_ram() == 0 {
        return Err(MemoryError::NoUsableMemory);
    }
    // Among usable regions clipped to the direct map, not merely the longest
    // one: the array is zeroed through the direct map, and on a 32-bit board
    // with more RAM than that map holds the longest region can lie above it.
    let host = view
        .page_array_host(bytes)
        .ok_or(MemoryError::NoRoomForPageArray(bytes))?;

    let array = place_page_array(host.base, entries);
    let mut frames = Frames::new(array, lowest);

    // Everything firmware called usable, minus the part the array now occupies.
    let array_end = host.base + bytes.next_multiple_of(PAGE_SIZE);
    for region in view.regions() {
        if region.kind.is_free_at_boot() {
            insert_region(&mut frames, region, host.base, array_end);
        }
    }

    let stats = Stats {
        managed_frames: frames.managed_frames(),
        free_frames: frames.free_frames(),
        page_array_bytes: bytes,
        page_array_at: host.base,
    };

    *FRAMES.lock() = Some(frames);
    Ok(stats)
}

/// The lowest frame of anything firmware called memory.
///
/// RAM rather than *usable* RAM: the array has to have an entry for the
/// kernel's own pages and for firmware's, because a refcount on a frame is how
/// copy-on-write will work and those frames get shared too.
fn lowest_ram_frame(view: &BootView<'_>) -> u64 {
    view.regions()
        .iter()
        .filter(|region| region.kind.is_ram())
        .map(|region| region.base / PAGE_SIZE)
        .min()
        .unwrap_or(0)
}

/// Zero `entries` records at the front of `base` and take them as a slice.
///
/// The array outlives everything, so `'static` is honest: it is carved out of
/// physical memory that is then excluded from the allocator, and there is no
/// path that gives it back.
fn place_page_array(base: u64, entries: usize) -> &'static mut [PageEntry] {
    let virt = physmap(base) as *mut PageEntry;
    let bytes = entries * size_of::<PageEntry>();

    // Zero first, then take the slice. Forming a `&mut [PageEntry]` over memory
    // holding whatever the last owner left would be undefined behaviour: `State`
    // is an enum, and an undefined discriminant is not merely a strange value.
    // `ferrix_frame` guarantees zero is a state it defines.
    //
    // SAFETY: (KMEM) `base` is the start of a run of RAM firmware called usable, and
    // which is about to be excluded from the allocator, so nothing else refers
    // to it. `BootView::page_array_host` chose that run from inside the direct
    // map, so all `bytes` of it are mapped and writable at `virt` -- which is
    // not the same as "the direct map covers all of RAM": on ARMv7-A it covers
    // 1.25 GiB, and the rest is not mapped anywhere.
    unsafe { core::ptr::write_bytes(virt.cast::<u8>(), 0, bytes) };

    // SAFETY: (KMEM) the range was just zeroed, is inside a usable region large enough
    // for it, is excluded from the allocator below, and is naturally aligned
    // because the region base is page aligned.
    unsafe { core::slice::from_raw_parts_mut(virt, entries) }
}

/// The lowest frame the allocator is ever given.
///
/// Frame 0 stays `Reserved` on every architecture, because physical address 0
/// must never be a frame: it means "none" in places -- a GIC base, the
/// loader's root checks -- and Linux never hands out page 0 either. The rule
/// is the same wherever RAM starts. OVMF reports 0x0-0x9FFFF as conventional
/// memory, so before it frame 0 was a free frame on x86-64 from stage 2 on,
/// handed out whenever the buddy allocator split that low block: under KVM,
/// as the root of an address space (FX-0601).
const FIRST_MANAGED_FRAME: Frame = 1;

/// Hand one usable region to the allocator, skipping the page array and
/// anything below [`FIRST_MANAGED_FRAME`].
fn insert_region(frames: &mut Frames<'static>, region: &MemRegion, hole: u64, hole_end: u64) {
    let start = region.base.max(FIRST_MANAGED_FRAME * PAGE_SIZE);
    let end = region.end();
    if end <= start {
        return;
    }

    // The common case: the region has nothing to do with the array.
    if end <= hole || start >= hole_end {
        frames.insert_free(start / PAGE_SIZE, (end - start) / PAGE_SIZE);
        return;
    }

    // Otherwise add whatever lies either side of it. The array is placed at the
    // front of its host region, so in practice only the tail exists — but a
    // future placement policy should not silently lose the head.
    if start < hole {
        frames.insert_free(start / PAGE_SIZE, (hole - start) / PAGE_SIZE);
    }
    if end > hole_end {
        frames.insert_free(hole_end / PAGE_SIZE, (end - hole_end) / PAGE_SIZE);
    }
}

// ---------------------------------------------------------------------------
// The public allocation interface
// ---------------------------------------------------------------------------

/// Take `2^order` contiguous frames.
///
/// `None` when there are none -- or when the allocation check is failing
/// this task's allocations one at a time and this is the one
/// (`fallible::frame_refused`), which reaches every caller's answer to an
/// empty allocator without emptying it for the rest of the machine.
pub(crate) fn allocate_frames(order: u8) -> Option<Frame> {
    if fallible::frame_refused() {
        return None;
    }
    let frame = with_frames(|frames| frames.allocate(order))??;
    count(Route::Allocated, 1 << order);
    Some(frame)
}

/// Take one frame of a program's memory, charged to the job the running task
/// is in (`object::quota`, `FRU_RSA.1`): refused, as if memory had run out,
/// when that job or one above it is at its memory limit.
///
/// The charge is the job of whoever caused the frame, as Linux charges the
/// first toucher, and stays there until the frame is freed, by whoever and
/// under whatever lock: the frame record keeps the job's slot, and
/// [`release_frame`] and [`deallocate_frames`] take the charge back from it.
pub(crate) fn allocate_user_frame() -> Option<Frame> {
    let owner = crate::sched::running_group();
    crate::object::quota::charge_frame(owner).ok()?;
    let frame = allocate_frames(0).and_then(|frame| {
        with_frames(|frames| frames.set_owner(frame, owner))
            .and_then(Result::ok)
            .map(|()| frame)
            .or_else(|| {
                deallocate_frames(frame, 0);
                None
            })
    });
    if frame.is_none() {
        crate::object::quota::uncharge_frame(owner);
    }
    frame
}

/// Take `2^order` frames lying back to back as single frames of a program's
/// memory, each charged to the running task's job as [`allocate_user_frame`]
/// charges one and given back on its own with [`release_frame`]: a pin that
/// must be one run (`PIN_CONTIGUOUS`, `docs/NVIDIA.md` §4.3). The job is
/// charged for the whole run before a frame is taken, so a job at its limit
/// is refused with nothing taken; `None` then, or with no block free, and
/// nothing is charged.
pub(crate) fn allocate_user_run(order: u8) -> Option<Frame> {
    let owner = crate::sched::running_group();
    let count = 1u64 << order;
    let uncharge = |charged: u64| {
        for _ in 0..charged {
            crate::object::quota::uncharge_frame(owner);
        }
    };
    let charged = (0..count)
        .take_while(|_| crate::object::quota::charge_frame(owner).is_ok())
        .count() as u64;
    if charged < count {
        uncharge(charged);
        return None;
    }
    let Some(block) = allocate_frames(order) else {
        uncharge(count);
        return None;
    };
    if !split_frames(block, order) {
        deallocate_frames(block, order);
        uncharge(count);
        return None;
    }
    // Each frame owned by the job, so that releasing it takes its charge
    // back. One that cannot be (a fresh single frame always can) goes back
    // with the rest, its charge by hand.
    let owned = (block..block + count)
        .take_while(|&frame| {
            with_frames(|frames| frames.set_owner(frame, owner)).is_some_and(|set| set.is_ok())
        })
        .count() as u64;
    if owned < count {
        for frame in block..block + count {
            let _ = release_frame(frame);
        }
        uncharge(count - owned);
        return None;
    }
    Some(block)
}

/// Take `blocks` blocks of `2^MAX_ORDER` frames lying back to back, for
/// memory that has to be one run longer than a block: the first frame, each
/// block given back or split on its own as one from [`allocate_frames`]
/// would be (`ferrix_frame::Frames::allocate_run`).
pub(crate) fn allocate_frame_run(blocks: u64) -> Option<Frame> {
    if fallible::frame_refused() {
        return None;
    }
    let frame = with_frames(|frames| frames.allocate_run(blocks))??;
    count(Route::Allocated, blocks << ferrix_frame::MAX_ORDER);
    Some(frame)
}

/// Take `2^order` contiguous frames lying wholly below frame `limit`.
///
/// For the one caller that cares where its memory is: x86-64's trampoline,
/// which real mode can only reach below one mebibyte.
#[allow(
    dead_code,
    reason = "only x86-64 starts processors from low memory; see the doc comment"
)]
pub(crate) fn allocate_frames_below(order: u8, limit: Frame) -> Option<Frame> {
    with_frames(|frames| frames.allocate_below(order, limit))?
}

/// What the allocator records for `frame`, or `None` if its array does not
/// cover the frame or the allocator is not up.
pub(crate) fn frame_state(frame: Frame) -> Option<ferrix_frame::State> {
    with_frames(|frames| frames.state(frame))?
}

/// Take exactly `frame`, if it is free: for a check that must get back the
/// frame it just gave up. `None` if something else has it.
pub(crate) fn claim_frame(frame: Frame) -> Option<Frame> {
    with_frames(|frames| frames.claim(frame))?
}

/// Turn a block taken with [`allocate_frames`] into `2^order` single frames,
/// each given back on its own with [`release_frame`]: memory that must be
/// contiguous when it is taken and is owned page by page after. Whether it
/// was split.
pub(crate) fn split_frames(frame: Frame, order: u8) -> bool {
    with_frames(|frames| frames.split(frame, order).is_ok()).unwrap_or(false)
}

/// Give back frames taken with [`allocate_frames`].
pub(crate) fn deallocate_frames(frame: Frame, order: u8) {
    count(Route::Freed, 1 << order);
    let owner = with_frames(|frames| {
        let owner = frames.owner(frame);
        frames.deallocate(frame, order).map(|()| owner).ok()
    });
    // After the allocator's lock: an uncharge is atomics, but nothing needs
    // the two held together.
    if let Some(Some(owner)) = owner {
        crate::object::quota::uncharge_frame(owner);
    }
}

/// Record another reference to a frame, for a page two address spaces share.
///
/// Returns the new count, or `None` if the allocator refused — which it does
/// for a frame that is not allocated, or one whose count would wrap. Both are
/// kernel bugs rather than conditions a caller can recover from, so the caller
/// that maps the page must treat `None` as "do not map it".
pub(crate) fn share_frame(frame: Frame) -> Option<u32> {
    with_frames(|frames| frames.share(frame).ok())?
}

/// Drop one reference to a frame, freeing it if it was the last.
///
/// Returns true if the frame went back to the allocator. Anonymous memory is
/// freed through here rather than through [`deallocate_frames`], because a
/// copy-on-write page may still be mapped by somebody else and the allocator
/// refuses to free it while it is.
pub(crate) fn release_frame(frame: Frame) -> bool {
    // Qualified: `Released` is already `ferrix_paging`'s in this module, and
    // the two mean different things -- a page table given back versus a frame.
    let freed = with_frames(|frames| {
        let owner = frames.owner(frame);
        matches!(frames.release(frame), Ok(ferrix_frame::Released::Freed)).then_some(owner)
    })
    .flatten();
    if let Some(owner) = freed {
        count(Route::Released, 1);
        // The job the frame was charged to, as it goes back: under whatever
        // lock the caller holds, which is why the charge is atomics only.
        crate::object::quota::uncharge_frame(owner);
    }
    freed.is_some()
}

/// Take a frame's charge off the job it was charged to, and charge it to
/// nobody from now on: for a frame the kernel keeps after that job's process
/// has gone (`object::pin`'s quarantine), so the job's counters come back to
/// zero while the frame is still held. Nothing for a frame charged to nobody,
/// or one that is not an allocated single frame.
pub(crate) fn disown_frame(frame: Frame) {
    let owner = with_frames(|frames| {
        let owner = frames.owner(frame);
        frames
            .set_owner(frame, crate::object::quota::NONE)
            .ok()
            .map(|()| owner)
    })
    .flatten();
    // After the allocator's lock, as `release_frame` does.
    if let Some(owner) = owner {
        crate::object::quota::uncharge_frame(owner);
    }
}

/// The quota slot a frame is charged to, or `quota::NONE`: which job a page
/// reclaim finds belongs to (`user::cache`).
pub(crate) fn frame_owner(frame: Frame) -> u32 {
    // NOALLOC: the frame allocator's per-frame record, looked up.
    with_frames(|frames| frames.owner(frame)).unwrap_or(crate::object::quota::NONE)
}

/// How many references there are to a frame.
///
/// For the fault handler's one real decision: a copy-on-write fault on a page
/// nobody else holds any more does not need to copy anything.
pub(crate) fn frame_references(frame: Frame) -> u32 {
    // NOALLOC: the frame allocator's per-frame record, looked up.
    with_frames(|frames| frames.entry(frame).map_or(0, PageEntry::refcount)).unwrap_or(0)
}

/// Frames currently free.
pub(crate) fn free_frames() -> u64 {
    with_frames(|frames| frames.free_frames()).unwrap_or(0)
}

/// Print which way a frame-count window moved, for a check about to fail on
/// it. `leaked` is the count expected at the end less the count found there:
/// above zero the check kept frames, below it something outside the window
/// gave frames back while it was open, which is not the check's leak.
pub(crate) fn print_frame_delta(check: &str, leaked: i64) {
    if leaked > 0 {
        crate::console::println!("  {check:<8} {leaked} frames not given back");
    } else if leaked < 0 {
        crate::console::println!(
            "  {check:<8} {} frames came back from outside the window",
            leaked.unsigned_abs()
        );
    }
}

/// Frames the allocator was given to manage: what `MemTotal` reports.
pub(crate) fn managed_frames() -> u64 {
    with_frames(|frames| frames.managed_frames()).unwrap_or(0)
}

/// Bytes currently out on the kernel heap.
pub(crate) fn heap_allocated() -> usize {
    with_heap(|heap| heap.allocated_bytes())
}

/// Pages the heap is holding for its size classes.
pub(crate) fn heap_pages() -> usize {
    with_heap(|heap| heap.slab_pages() + heap.large_pages())
}

// ---------------------------------------------------------------------------
// The heap's page supply
// ---------------------------------------------------------------------------

/// Gives the heap pages out of the buddy allocator, addressed through the
/// direct map.
struct KernelPages;

// SAFETY: (KMEM) `allocate_pages` returns frames the buddy allocator handed out, which
// are contiguous, page aligned and owned by nobody else, addressed through the
// direct map, which covers all of RAM for the life of the system. `read_link`
// and `write_link` are only ever called by the heap on addresses inside pages
// it took from `allocate_pages` and has not given back, so nothing aliases
// them.
unsafe impl Backing for KernelPages {
    fn allocate_pages(&mut self, order: u8) -> Option<u64> {
        let frame = allocate_frames(order)?;
        count(Route::HeapTaken, 1 << order);
        Some(physmap(frame * PAGE_SIZE))
    }

    fn deallocate_pages(&mut self, address: u64, order: u8) {
        count(Route::HeapReturned, 1 << order);
        deallocate_frames(unmap(address) / PAGE_SIZE, order);
    }

    fn read_link(&self, at: u64) -> u64 {
        // SAFETY: (KMEM) as documented on the impl. Eight-byte aligned because every
        // size class is a multiple of eight and slab pages are page aligned.
        unsafe { core::ptr::read_volatile(at as *const u64) }
    }

    fn write_link(&mut self, at: u64, value: u64) {
        // SAFETY: (KMEM) as `read_link`.
        unsafe { core::ptr::write_volatile(at as *mut u64, value) };
    }

    fn slab_free(&self, page: u64) -> u16 {
        with_frames(|frames| frames.slab_free(unmap(page) / PAGE_SIZE)).unwrap_or(0)
    }

    fn set_slab_free(&mut self, page: u64, objects: u16) {
        let _ = with_frames(|frames| frames.set_slab_free(unmap(page) / PAGE_SIZE, objects));
    }
}

/// The kernel's `alloc` implementation.
struct KernelAllocator;

// SAFETY: (KMEM) `alloc` returns either null or the address of a block of at least
// `layout.size()` bytes, aligned to `layout.align()`, which no other live
// allocation overlaps — that is `ferrix_heap`'s contract, and its tests are
// where it is checked. `dealloc` is only called with a pointer and layout from
// a matching `alloc`, which is what `GlobalAlloc` requires of its caller.
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let request = Request::new(layout.size(), layout.align());
        if reserve::bypassing_heap()
            && let Some(address) = reserve::draw(request)
        {
            return address as *mut u8;
        }
        let address = with_heap(|heap| heap.allocate(&mut KernelPages, request).ok())
            // Refused: inside a reserved section, the reserve serves it
            // instead (`mm/reserve.rs`, finding F-23).
            .or_else(|| reserve::draw(request));
        // A null return is how `GlobalAlloc` reports failure. A fallible
        // caller -- `try_reserve`, `ferrix_fallible` -- turns it into an
        // error; an infallible one calls the allocation error handler, and
        // the count is how the panic that follows is recognised as that.
        address.map_or_else(
            || {
                let _ = REFUSALS.fetch_add(1, Ordering::Relaxed);
                core::ptr::null_mut()
            },
            |address| address as *mut u8,
        )
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let request = Request::new(layout.size(), layout.align());
        with_heap(|heap| heap.deallocate(&mut KernelPages, pointer as u64, request));
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

/// Allocations the heap has refused, reserve and all, since boot.
static REFUSALS: AtomicU64 = AtomicU64::new(0);

/// How many allocations the heap has refused since boot: nonzero before the
/// panic an infallible allocation's failure ends in.
pub(crate) fn heap_refusals() -> u64 {
    REFUSALS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Kernel mappings
// ---------------------------------------------------------------------------

/// Physical address of the root page table the loader installed.
static ROOT_TABLE: AtomicU64 = AtomicU64::new(0);

/// A window of kernel address space with nothing mapped in it, faulted in a
/// page at a time on first touch.
///
/// Stage 3 uses it to prove the fault path works end to end. It is the same
/// mechanism stage 6 uses for every anonymous user mapping, which is the point
/// of testing it this way: a fault is resolved by *making the mapping true* and
/// letting the instruction retry, never by stepping over it.
pub(crate) const DEMAND_WINDOW: u64 = crate::vmap::DEMAND_WINDOW;

/// Size of that window.
pub(crate) const DEMAND_WINDOW_SIZE: u64 = 2 * 1024 * 1024;

/// True if `address` is inside the on-demand window.
pub(crate) fn is_demand_window(address: u64) -> bool {
    (DEMAND_WINDOW..DEMAND_WINDOW + DEMAND_WINDOW_SIZE).contains(&address)
}

/// Physical memory as the kernel sees it once the frame allocator is up:
/// through the direct map, with page table frames from the buddy.
///
/// Replaces `EarlyMemory`'s pool in `.bss`, which only ever had sixteen frames
/// and existed to get the console mapped before there was an allocator.
struct KernelPhysMem;

// SAFETY: (TRANSLATE) `read` and `write` go through the direct map, which covers every byte
// of RAM and is the only mapping of it the kernel holds, so nothing can alias
// them. `allocate_table` returns a frame from the buddy allocator, which hands
// each one out once; it is page aligned because frames are, and zeroed here
// before it is returned.
unsafe impl PhysMem for KernelPhysMem {
    fn read(&self, at: PhysAddr) -> u64 {
        // SAFETY: (TRANSLATE) as documented on the impl; `at` is a descriptor address.
        unsafe { core::ptr::read_volatile(physmap(at.0) as *const u64) }
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        // SAFETY: (TRANSLATE) as documented on the impl.
        unsafe { core::ptr::write_volatile(physmap(at.0) as *mut u64, value) };
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        let frame = allocate_frames(0)?;
        let address = frame * PAGE_SIZE;
        // A page table built on someone else's leftovers translates to wherever
        // they pointed, so this zeroing is not optional.
        //
        // SAFETY: (TRANSLATE) the frame was just allocated to us and nothing else refers to
        // it; the direct map makes it writable.
        unsafe { core::ptr::write_bytes(physmap(address) as *mut u8, 0, PAGE_SIZE as usize) };
        Some(PhysAddr(address))
    }
}

/// A program's page tables: [`KernelPhysMem`], with every table it makes
/// charged to the job the running task is in, as [`allocate_user_frame`]
/// charges a page (`object::quota`). A table is freed through
/// [`deallocate_frames`], which takes the charge back wherever that happens.
struct ChargedTables {
    /// The job's quota slot, or `quota::NONE`.
    owner: u32,
}

// SAFETY: (TRANSLATE) every descriptor access is `KernelPhysMem`'s, whose argument this
// inherits; `allocate_table` returns a frame the buddy allocator handed out
// once, page aligned, zeroed before it is returned, and charged besides.
unsafe impl PhysMem for ChargedTables {
    fn read(&self, at: PhysAddr) -> u64 {
        KernelPhysMem.read(at)
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        KernelPhysMem.write(at, value);
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        if self.owner == crate::object::quota::NONE {
            return KernelPhysMem.allocate_table();
        }
        crate::object::quota::charge_frame(self.owner).ok()?;
        let table = KernelPhysMem.allocate_table();
        let owned = table.and_then(|table| {
            with_frames(|frames| frames.set_owner(table.0 / PAGE_SIZE, self.owner))
                .and_then(Result::ok)
                .map(|()| table)
        });
        match (table, owned) {
            (_, Some(table)) => Some(table),
            (Some(table), None) => {
                // Not tagged, so this uncharges nothing; the charge goes below.
                deallocate_frames(table.0 / PAGE_SIZE, 0);
                crate::object::quota::uncharge_frame(self.owner);
                None
            }
            (None, None) => {
                crate::object::quota::uncharge_frame(self.owner);
                None
            }
        }
    }
}

/// Map `len` bytes of kernel address space at `virt` onto `phys`.
pub(crate) fn map_kernel(
    virt: u64,
    phys: u64,
    len: u64,
    flags: MapFlags,
) -> Result<(), ferrix_paging::MapError> {
    with_tables(|mapper| {
        mapper.map_range(
            &mut KernelPhysMem,
            VirtAddr(virt),
            PhysAddr(phys),
            len.next_multiple_of(PAGE_SIZE),
            flags,
        )
    })?;
    // The table walker is a separate observer of memory: on AArch64 it cannot
    // see a descriptor still sitting in a store buffer, and the barrier inside
    // this is what makes the mapping real.
    crate::arch::flush_tlb();
    Ok(())
}

/// Resolve a fault in the on-demand window by mapping a fresh zeroed page.
pub(crate) fn map_demand_page(address: u64) -> Result<(), MemoryError> {
    let page = address & !(PAGE_SIZE - 1);
    let frame = allocate_frames(0).ok_or(MemoryError::NoUsableMemory)?;

    // Anonymous memory reads as zero, and a page handed to a program still
    // holding the last owner's data is an information leak, so this is a
    // correctness requirement rather than tidiness.
    //
    // SAFETY: (FRAME) the frame was just allocated to us and the direct map covers it.
    unsafe { core::ptr::write_bytes(physmap(frame * PAGE_SIZE) as *mut u8, 0, PAGE_SIZE as usize) };

    map_kernel(page, frame * PAGE_SIZE, PAGE_SIZE, MapFlags::KERNEL_DATA)
        .map_err(|_| MemoryError::NoUsableMemory)
}

/// Translate a kernel virtual address the way the hardware would.
pub(crate) fn translate(virt: u64) -> Option<u64> {
    with_tables(|mapper| {
        mapper
            .translate(&KernelPhysMem, VirtAddr(virt))
            .map(|at| at.0)
    })
}

/// Translate an address through the tree rooted at `root`, the way the
/// hardware would if that root were installed.
///
/// The counterpart of [`map_in`], and it takes no lock for the same reason
/// that one does not: the caller owns the tree. An address space that other
/// processors may be faulting in holds its own lock across this.
pub(crate) fn translate_in(root: u64, virt: u64) -> Option<u64> {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    mapper
        .translate(&KernelPhysMem, VirtAddr(virt))
        .map(|at| at.0)
}

/// The flags of the leaf mapping `virt` in the tree rooted at `root`, as
/// [`translate_in`] finds it: what a check reads a mapping's memory type from.
pub(crate) fn flags_in(root: u64, virt: u64) -> Option<MapFlags> {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    mapper.leaf_flags(&KernelPhysMem, VirtAddr(virt))
}

/// Physical address of the kernel's root page table, for a processor about to
/// install it.
pub(crate) fn root_table() -> u64 {
    ROOT_TABLE.load(Ordering::Relaxed)
}

/// Where physical address `phys` can be read and written: its alias in the
/// direct map. Read only over the kernel's own text and read-only data, which
/// the loaders seal there ([`check::check_sealed_image`]); every frame the allocator
/// hands out is outside that span, since the image is never freed.
pub(crate) fn direct_map(phys: u64) -> u64 {
    physmap(phys)
}

/// [`direct_map`] for an address that has to be RAM: its direct-map alias if
/// `phys` is in a run [`record_ram`] recorded, and `None` otherwise.
///
/// What a path that reaches memory a program's tables name asks, rather than
/// [`direct_map`]'s bare arithmetic. That arithmetic answers for any physical
/// address: above the direct map's span with an address nothing translates,
/// so the copy faults in the kernel (F-55); inside the span, in a hole or on
/// a device's registers, with an address that is unmapped or would be a
/// cacheable alias of memory mapped uncached elsewhere, which the Arm
/// architecture makes unpredictable and x86 answers with a machine check.
/// This answers only for RAM, where the direct map's attributes are the
/// memory's own.
///
/// The runs are whole pages, so an answer for one byte holds for its page.
/// Takes no lock and reads only atomics, so the panic path may ask it too.
pub(crate) fn direct_map_ram(phys: u64) -> Option<u64> {
    let count = RAM_RUN_COUNT.load(Ordering::Acquire);
    RAM_RUNS
        .iter()
        .take(count)
        .any(|(first, end)| {
            (first.load(Ordering::Relaxed)..end.load(Ordering::Relaxed)).contains(&phys)
        })
        .then(|| physmap(phys))
}

/// Whether [`record_ram`] has run, so that [`direct_map_ram`]'s `None` means
/// "not RAM" rather than "not known yet".
pub(crate) fn ram_recorded() -> bool {
    RAM_RUN_COUNT.load(Ordering::Acquire) != 0
}

/// Record the runs of RAM the direct map translates, for [`direct_map_ram`].
///
/// RAM as the memory map calls it ([`MemKind::is_ram`]) less a framebuffer
/// firmware carved out of it, whose pages are the display's and are mapped
/// to programs as a device's; merged where regions touch and clipped to the
/// direct map's span, as the loaders' own [`direct_map_runs`] walk is. Every
/// frame the allocator is ever given is in one: it is given usable RAM at
/// [`init`], and the loader's and ACPI's reclaimable RAM later.
///
/// # Errors
///
/// [`MemoryError::TooManyRamRuns`] if the runs do not fit [`RAM_RUNS`].
fn record_ram(view: &BootView<'_>) -> Result<(), MemoryError> {
    let info = view.raw();
    let ram = view
        .regions()
        .iter()
        .copied()
        .filter(|region| region.kind.is_ram() && region.kind != MemKind::Framebuffer);
    let mut runs = direct_map_runs(ram, info.physmap_phys, info.physmap_len);
    let mut count = 0_usize;
    // `RAM_RUNS` first: a zip stops at the shorter without taking one more
    // from the other, so a run that does not fit is left for the count below.
    for ((first, end), (start, len)) in RAM_RUNS.iter().zip(runs.by_ref()) {
        first.store(start, Ordering::Relaxed);
        end.store(start.saturating_add(len), Ordering::Relaxed);
        count = count.saturating_add(1);
    }
    let left = runs.count();
    if left != 0 {
        return Err(MemoryError::TooManyRamRuns(count.saturating_add(left)));
    }
    RAM_RUN_COUNT.store(count, Ordering::Release);
    Ok(())
}

// ---------------------------------------------------------------------------
// Page tables beside the kernel's
// ---------------------------------------------------------------------------

/// Map `len` bytes at `virt` onto `phys` in the tree rooted at `root`.
///
/// What starting a processor needs: a mapping the kernel's own tables must not
/// have — an identity map of the instructions that turn the MMU on — in a tree
/// only that processor installs, and only for as long as it takes. The caller
/// allocates and zeroes `root`, because where it may live differs: anywhere on
/// `AArch64`, below 4 GiB for x86-64's trampoline.
///
/// Takes no lock: a tree no processor has installed is nobody else's to walk.
/// `root` must not be the kernel's, which [`map_kernel`] is for.
pub(crate) fn map_in(
    root: u64,
    virt: u64,
    phys: u64,
    len: u64,
    flags: MapFlags,
) -> Result<(), ferrix_paging::MapError> {
    // Code a program will run: what the kernel wrote there, a file's pages or
    // a copy, has to be what every processor fetches before any can reach it.
    if flags.user && flags.execute {
        crate::arch::sync_instructions(direct_map(phys), len.next_multiple_of(PAGE_SIZE));
    }
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    // A program's tables are charged to its job, which a user mapping always
    // is: the running task's, whoever's space it is (`object::quota`).
    let owner = if flags.user {
        crate::sched::running_group()
    } else {
        crate::object::quota::NONE
    };
    mapper.map_range(
        &mut ChargedTables { owner },
        VirtAddr(virt),
        PhysAddr(phys),
        len.next_multiple_of(PAGE_SIZE),
        flags,
    )
}

/// Make top-level `slots` of the tree rooted at `root` point where the
/// kernel's do, so that part of the address space looks the same through
/// either root.
///
/// Shared, not copied: the tables under those slots are the kernel's own. So
/// nothing may be mapped or unmapped through `root` inside them — [`map_in`]
/// and [`unmap_in`] on `root` have to stay in the slots that are its own.
///
/// x86-64 only, and deliberately not behind a conditional, for the reason
/// [`clear_root_slots`] gives: `AArch64` splits the halves across two base
/// registers, so a tree there never needs the kernel's half in it.
#[allow(
    dead_code,
    reason = "only x86-64 keeps both halves in one root; see the doc comment"
)]
pub(crate) fn share_kernel_slots(root: u64, slots: core::ops::Range<usize>) {
    let kernel = ROOT_TABLE.load(Ordering::Relaxed);
    with_tables(|_| {
        for slot in slots {
            let offset = (slot as u64) * 8;
            let entry = KernelPhysMem.read(PhysAddr(kernel + offset));
            KernelPhysMem.write(PhysAddr(root + offset), entry);
        }
    });
}

/// Take down what [`map_in`] built at `virt` in a live tree, and add to
/// `flush` both the addresses to shoot down and every table under `root` the
/// removal leaves empty.
///
/// The tables are unlinked here and given back only by
/// [`crate::smp::flush_tlb_pages`] for `flush`, once every processor it
/// reaches has answered: a processor may hold a walk through a table in its
/// paging-structure or walk caches until then (finding F-36, and
/// [`UnlinkedTables`]). The range is added to `flush` here rather than by
/// the caller so that the two cannot part: a table on the list is always
/// covered by an address the same shootdown invalidates.
///
/// Neither the root nor the frames the mappings pointed at are freed: the
/// caller allocated both and knows what they are.
pub(crate) fn unmap_in(
    root: u64,
    virt: u64,
    len: u64,
    flush: &mut crate::smp::TlbPages,
) -> Result<(), ferrix_paging::MapError> {
    let len = len.next_multiple_of(PAGE_SIZE);
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    let tables = flush.tables();
    let unmapped = mapper.unmap_range(&mut KernelPhysMem, VirtAddr(virt), len, |freed| {
        if let Released::Table { phys } = freed {
            // NOALLOC: the list is linked through the tables themselves.
            tables.push(phys);
        }
    });
    // Whatever came out before an error is down too, and is shot down with
    // the rest.
    flush.add_range(virt, len);
    let _ = unmapped?;
    Ok(())
}

/// [`unmap_in`] for a tree no processor can walk and none has a walk of
/// cached: one never installed, one every processor that ran it has left
/// with a flush of its whole TLB (a started processor's bring-up tree), or an
/// address space being dropped, which every processor left through the root
/// write that dropped its entries. Its tables go back at once.
///
/// Anything else is [`unmap_in`]'s, for the reason it gives.
pub(crate) fn unmap_unwalked(
    root: u64,
    virt: u64,
    len: u64,
) -> Result<(), ferrix_paging::MapError> {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    let _ = mapper.unmap_range(
        &mut KernelPhysMem,
        VirtAddr(virt),
        len.next_multiple_of(PAGE_SIZE),
        |freed| {
            if let Released::Table { phys } = freed {
                count(Route::UserTables, 1);
                deallocate_frames(phys.0 / PAGE_SIZE, 0);
            }
        },
    )?;
    Ok(())
}

/// Give back every table under `root` in the `len` bytes at `virt` that maps
/// nothing: what a [`map_in`] that ran out of memory part-way made above the
/// page it could not map, which no [`unmap_in`] prunes, since only a page's
/// removal does (`ferrix_paging::Mapper::prune_range`).
///
/// For a tree no processor can walk any more -- one being torn down -- which
/// is when such a table may go: a processor that walked through one while it
/// was linked may still hold the walk in its caches.
pub(crate) fn prune_in(root: u64, virt: u64, len: u64) {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    let _ = mapper.prune_range(&mut KernelPhysMem, VirtAddr(virt), len, |freed| {
        if let Released::Table { phys } = freed {
            count(Route::UserTables, 1);
            deallocate_frames(phys.0 / PAGE_SIZE, 0);
        }
    });
}

/// Map the 4 KiB page `phys` at I/O address `iova` in the IOMMU tree rooted at
/// `root`, whose descriptors `E` encodes.
///
/// The IOMMU counterpart of [`map_in`]: the tables come from the frame
/// allocator through the direct map, and the caller owns the tree, so no lock
/// is taken. One page at a time, so no block mapping is ever built, which a
/// unit without superpages could not walk.
///
/// Every entry it writes and every table it makes is noted in `writes`, for a
/// unit whose walk does not snoop the caches (finding F-58): a fresh table is
/// cleaned to memory before it is linked, and the entries wait for the
/// caller's [`Unpublished::publish`].
pub(crate) fn map_io<E: Encoding>(
    root: u64,
    iova: u64,
    phys: u64,
    flags: MapFlags,
    writes: &mut Unpublished,
) -> Result<(), ferrix_paging::MapError> {
    let mut mapper: Mapper<E> = Mapper::new(PhysAddr(root));
    mapper.pages_only();
    mapper.map_range(
        &mut Walked::new(&mut KernelPhysMem, writes, WalkerClean),
        VirtAddr(iova),
        PhysAddr(phys),
        PAGE_SIZE,
        flags,
    )
}

/// Take down the page [`map_io`] put at `iova`, adding every table under
/// `root` it leaves empty to `tables`.
///
/// Neither the frame the page pointed at nor the tables may be freed before
/// the unit's cached translations are invalidated: a unit caches the walk as
/// well as the leaf (VT-d's paging-structure caches, the SMMU's walk cache),
/// and would walk a freed table as the processor would (finding F-36). The
/// caller releases `tables` after the unit's flush has completed.
///
/// Every entry it clears is noted in `writes`, as [`map_io`]'s are.
pub(crate) fn unmap_io<E: Encoding>(
    root: u64,
    iova: u64,
    tables: &mut UnlinkedTables,
    writes: &mut Unpublished,
) -> Result<(), ferrix_paging::MapError> {
    let mapper: Mapper<E> = Mapper::new(PhysAddr(root));
    let mut kernel = KernelPhysMem;
    let mut memory = Walked::new(&mut kernel, writes, WalkerClean);
    let _ = mapper.unmap_range(&mut memory, VirtAddr(iova), PAGE_SIZE, |freed| {
        if let Released::Table { phys } = freed {
            // NOALLOC: the list is linked through the tables themselves.
            tables.push(phys);
        }
    })?;
    Ok(())
}

/// Cleaning for an IOMMU whose table walk does not snoop the caches: the
/// range, through the direct map, by [`crate::arch::clean_for_walker`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct WalkerClean;

impl Clean for WalkerClean {
    fn clean(&mut self, at: PhysAddr, len: u64) {
        crate::arch::clean_for_walker(physmap(at.0), len);
    }
}

/// Where I/O address `iova` leads in the IOMMU tree rooted at `root`, walked
/// as the unit would.
pub(crate) fn translate_io<E: Encoding>(root: u64, iova: u64) -> Option<u64> {
    let mapper: Mapper<E> = Mapper::new(PhysAddr(root));
    mapper
        .translate(&KernelPhysMem, VirtAddr(iova))
        .map(|at| at.0)
}

/// Run `body` with a mapper over the live kernel page tables, holding
/// [`TABLES`] throughout.
///
/// Reads take the lock as well as writes. A walk racing an unmap on another
/// CPU can follow a descriptor into a table that has just been freed and
/// handed to somebody else, and read whatever they wrote there as though it
/// were a translation.
fn with_tables<T>(body: impl FnOnce(Mapper<crate::arch::PageEncoding>) -> T) -> T {
    let _held = TABLES.lock();
    body(Mapper::new(PhysAddr(ROOT_TABLE.load(Ordering::Relaxed))))
}

/// Remove `len` bytes of kernel mapping at `virt`.
///
/// `released` is called once per leaf with the frame number and the order of
/// the block it was mapped at, and is where the caller decides whether the
/// memory behind the mapping goes back to the buddy allocator. A `vmap` of
/// anonymous pages frees them; a device window never allocated them.
///
/// **Unmap, invalidate everywhere, and only then free** — in that order. A
/// frame handed back before every processor has dropped its translation to it
/// can be allocated to somebody else while another processor still reads and
/// writes it through the old one. That is not a leak or a crash; it is two
/// owners of one page, and the symptom appears in whichever of them notices
/// first. So nothing is released until the shootdown returns — not even the
/// page tables, which a processor's walker may have cached as well.
///
/// Must not be called holding any lock, for the reason
/// [`crate::smp::flush_tlb_everywhere`] gives.
pub(crate) fn unmap_kernel(
    virt: u64,
    len: u64,
    released: impl FnMut(Frame, u8),
) -> Result<u64, ferrix_paging::MapError> {
    unmap_kernel_all(&[(virt, len)], released)
}

/// Remove several kernel mappings, each `(virt, len)`, under one shootdown.
///
/// The same order as [`unmap_kernel`] -- unmap, invalidate everywhere, and
/// only then free -- for all of them at once. A shootdown interrupts every
/// other processor and waits for each to answer, and it costs the same
/// whether one page or a thousand was unmapped before it. Freeing a thousand
/// exited tasks' stacks one at a time was a thousand shootdowns; this is one.
///
/// Returns the bytes removed in all. On an error part-way, whatever was
/// unmapped before it is still invalidated and freed, and the error is
/// returned afterwards.
pub(crate) fn unmap_kernel_all(
    ranges: &[(u64, u64)],
    released: impl FnMut(Frame, u8),
) -> Result<u64, ferrix_paging::MapError> {
    let mut pages: Deferred<(Frame, u8), DEFERRED_PAGES> = Deferred::new((0, 0));
    let mut tables: Deferred<Frame, 8> = Deferred::new(0);

    // Room for everything it could release, made before anything is unmapped.
    // An unmap happens as something is torn down, with nobody to tell that
    // memory ran out (finding F-23), so when there is no room it does the
    // work in pieces small enough for the inline slots instead, one shootdown
    // each: slower, and needing no memory at all.
    let page_bound = ranges.iter().fold(0_u64, |sum, &(_, len)| {
        sum.saturating_add(len.div_ceil(PAGE_SIZE))
    });
    let table_bound = page_bound / 512 + 4 * ranges.len() as u64 + 4;
    if !pages.make_room(page_bound) || !tables.make_room(table_bound) {
        return unmap_kernel_in_pieces(ranges, released);
    }
    unmap_and_release(ranges, &mut pages, &mut tables, released)
}

/// Unmap `ranges`, shoot the translations down everywhere, and only then
/// release what they held, holding it in `pages` and `tables` meanwhile.
fn unmap_and_release<const N: usize>(
    ranges: &[(u64, u64)],
    pages: &mut Deferred<(Frame, u8), N>,
    tables: &mut Deferred<Frame, 8>,
    mut released: impl FnMut(Frame, u8),
) -> Result<u64, ferrix_paging::MapError> {
    let mut removed = Ok(0u64);
    for &(virt, len) in ranges {
        let this = with_tables(|mapper| {
            mapper.unmap_range(
                &mut KernelPhysMem,
                VirtAddr(virt),
                len.next_multiple_of(PAGE_SIZE),
                |freed| match freed {
                    Released::Page { phys, level } => {
                        // Levels run root-to-leaf and orders run
                        // small-to-large, so the conversion is a subtraction
                        // rather than a table: a level-3 leaf is order 0 and
                        // a 2 MiB block is order 9.
                        let order = (ferrix_paging::Level::PAGE.depth() - level.depth()) * 9;
                        // NOALLOC: `Deferred::push`, into room made before the unmap began.
                        pages.push((phys.0 / PAGE_SIZE, order));
                    }
                    // A page table the mapper allocated through
                    // `KernelPhysMem`, which took it from the buddy
                    // allocator. It goes straight back there rather than to
                    // the caller: the caller asked to unmap a range and has
                    // no idea a table existed, and telling it about one would
                    // make every `released` closure in the tree have to know.
                    // NOALLOC: `Deferred::push`, into room made before the unmap began.
                    Released::Table { phys } => tables.push(phys.0 / PAGE_SIZE),
                },
            )
        });
        match (this, &mut removed) {
            (Ok(bytes), Ok(total)) => *total = total.saturating_add(bytes),
            (Err(error), removed) if removed.is_ok() => *removed = Err(error),
            _ => {}
        }
        if removed.is_err() {
            break;
        }
    }

    // Whatever was removed before an error is just as unmapped, and just as
    // cached in somebody's TLB, as it would have been after a success.
    crate::smp::flush_tlb_everywhere();
    for &(frame, order) in pages.iter() {
        released(frame, order);
    }
    for &frame in tables.iter() {
        count(Route::KernelTables, 1);
        deallocate_frames(frame, 0);
    }
    removed
}

/// Pages an unmap holds inline, and the size of the pieces
/// [`unmap_kernel_in_pieces`] cuts a range into.
const DEFERRED_PAGES: usize = 32;

/// [`unmap_kernel_all`] with no memory to spare: each range in pieces of at
/// most [`DEFERRED_PAGES`] pages, each piece unmapped, shot down and released
/// before the next, so that what one releases always fits inline. A piece
/// that small can empty at most one table at each level, which the eight
/// inline table slots hold.
fn unmap_kernel_in_pieces(
    ranges: &[(u64, u64)],
    mut released: impl FnMut(Frame, u8),
) -> Result<u64, ferrix_paging::MapError> {
    let piece = DEFERRED_PAGES as u64 * PAGE_SIZE;
    let mut total = 0_u64;
    for &(virt, len) in ranges {
        let end = virt.saturating_add(len.next_multiple_of(PAGE_SIZE));
        let mut at = virt;
        while at < end {
            let this = piece.min(end - at);
            let mut pages: Deferred<(Frame, u8), DEFERRED_PAGES> = Deferred::new((0, 0));
            let mut tables: Deferred<Frame, 8> = Deferred::new(0);
            let bytes = unmap_and_release(&[(at, this)], &mut pages, &mut tables, &mut released)?;
            total = total.saturating_add(bytes);
            at += this;
        }
    }
    Ok(total)
}

/// What an unmap released, held until every TLB has forgotten it.
///
/// Inline for an unmap of up to `N` things, which is every unmap the kernel
/// makes today, and spilling to the heap past that -- into room made before
/// the unmap began ([`Deferred::make_room`]), so that holding never
/// allocates. **Not a `Vec` from the
/// start**, and not for speed: an unmap that allocated would, whenever its
/// size class had no slab page, take one from the frame allocator and keep it
/// — and the stage 2 checks, which require an unmap to give back exactly the
/// frames its map took, would be measuring the heap's bookkeeping instead.
/// Frames or tables an unmap could not hold for release: see
/// [`Deferred::push`]. Zero on a correct kernel.
static LOST_TO_UNMAPS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Deferred<T: Copy, const N: usize> {
    /// The first `N` items.
    inline: [T; N],
    /// How many of `inline` are in use.
    held: usize,
    /// The rest, for an unmap larger than `N`.
    spill: Vec<T>,
}

impl<T: Copy, const N: usize> Deferred<T, N> {
    /// An empty list; `blank` fills the unused inline slots.
    const fn new(blank: T) -> Self {
        Deferred {
            inline: [blank; N],
            held: 0,
            spill: Vec::new(),
        }
    }

    /// Make room to hold `count` things in all, beyond the inline ones.
    /// Returns whether there is room.
    fn make_room(&mut self, count: u64) -> bool {
        let spill = usize::try_from(count)
            .unwrap_or(usize::MAX)
            .saturating_sub(N);
        spill == 0 || fallible::try_reserve(&mut self.spill, spill).is_ok()
    }

    /// Hold on to `item`, in the room made for it.
    ///
    /// Past that room it is not held: the thing it names is never released,
    /// which loses memory rather than allocate on a path that cannot report
    /// failure. [`unmap_kernel_all`] makes room for the most an unmap can
    /// release, so this is not reached; [`LOST_TO_UNMAPS`] would count it.
    fn push(&mut self, item: T) {
        if let Some(slot) = self.inline.get_mut(self.held) {
            *slot = item;
            self.held += 1;
        } else if fallible::push_within(&mut self.spill, item).is_err() {
            let _ = LOST_TO_UNMAPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Everything held, in the order it was pushed.
    fn iter(&self) -> impl Iterator<Item = &T> {
        self.inline.iter().take(self.held).chain(self.spill.iter())
    }
}

/// Change what an existing kernel mapping permits.
pub(crate) fn protect_kernel(
    virt: u64,
    len: u64,
    flags: MapFlags,
) -> Result<(), ferrix_paging::MapError> {
    with_tables(|mapper| {
        mapper.protect_range(
            &mut KernelPhysMem,
            VirtAddr(virt),
            len.next_multiple_of(PAGE_SIZE),
            flags,
        )
    })?;
    // Everywhere: a permission narrowed on one processor and still wide in
    // another's TLB is not narrowed.
    crate::smp::flush_tlb_everywhere();
    Ok(())
}

// ---------------------------------------------------------------------------
// The image's span
// ---------------------------------------------------------------------------

/// Where the kernel image is physically, all of it -- text, read-only data,
/// data and `.bss` -- as its first byte and a length, learned from the
/// hand-off in [`init`]. Zero bytes until then.
static IMAGE_PHYS: AtomicU64 = AtomicU64::new(0);
static IMAGE_LEN: AtomicU64 = AtomicU64::new(0);

/// Whether `len` bytes of physical address space at `phys` touch the kernel's
/// own image.
///
/// What every interface that maps a physical address a caller names asks
/// before it maps anything: [`crate::vmap::map_device`], a user space's
/// device and window mappings, and early boot's device windows. The image's
/// text and read-only data have no writable mapping anywhere
/// ([`check::check_sealed_image`]), and a device window is writable, so a window over
/// them would undo the seal; its data and `.bss` are RAM the kernel uses
/// through cacheable mappings, which a device window would alias uncached. No
/// device's registers are in the image, so nothing is lost by refusing all of
/// it. A zero length is taken as one byte, so an empty range at the image is
/// refused too.
pub(crate) fn overlaps_image(phys: u64, len: u64) -> bool {
    image_span_overlaps(
        IMAGE_PHYS.load(Ordering::Relaxed),
        IMAGE_LEN.load(Ordering::Relaxed),
        phys,
        len,
    )
}

/// Whether `len` bytes at `phys` touch the `image_len` bytes at `image_phys`,
/// for a caller that has the hand-off in hand but runs before [`init`].
pub(crate) const fn image_span_overlaps(
    image_phys: u64,
    image_len: u64,
    phys: u64,
    len: u64,
) -> bool {
    let end = phys.saturating_add(if len == 0 { 1 } else { len });
    image_len > 0 && phys < image_phys.saturating_add(image_len) && image_phys < end
}

/// The kernel image's physical span, for the boot check that tries to map it.
pub(crate) fn image_span() -> (u64, u64) {
    (
        IMAGE_PHYS.load(Ordering::Relaxed),
        IMAGE_LEN.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// Reclaiming early boot
// ---------------------------------------------------------------------------

/// What reclaiming early-boot memory recovered.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Reclaimed {
    /// Frames the loader's own code and data occupied.
    pub(crate) loader_frames: u64,
    /// Frames the ACPI tables occupied.
    pub(crate) acpi_frames: u64,
}

impl Reclaimed {
    /// Frames recovered in total.
    pub(crate) const fn total(&self) -> u64 {
        self.loader_frames + self.acpi_frames
    }
}

/// Give the frame allocator the memory early boot has finished with.
///
/// Two kinds, and the ordering constraint on each is the whole of the risk.
///
/// * [`MemKind::Loader`] is the loader's own code and data. The kernel stopped
///   executing it at the jump, and nothing in the hand-off points into it —
///   the memory map and the command line were copied into the `BootInfo`
///   region, which is a different kind and is **not** reclaimed.
/// * [`MemKind::AcpiReclaim`] holds the firmware tables. Those are read
///   through the direct map by `crate::discovery::acpi`, so this must not run until the
///   last parse has finished; it is called from `kmain` after interrupt
///   bring-up for exactly that reason.
///
/// Deliberately *not* reclaimed: [`MemKind::PageTables`], which the kernel is
/// running on; [`MemKind::BootStack`], which it is running on too; and
/// [`MemKind::BootInfo`], which `BootView` borrows for the life of the system.
///
/// # Safety
///
/// (KMEM) Every reference into loader or ACPI-reclaim memory must be dead. In this
/// kernel that means being called from `kmain` after the last use of
/// `crate::discovery::acpi::Firmware`, and it is called exactly once.
pub(crate) unsafe fn reclaim_boot_memory(view: &BootView<'_>) -> Reclaimed {
    let mut reclaimed = Reclaimed::default();

    let _ = with_frames(|frames| {
        for region in view.regions() {
            if !region.kind.is_reclaimable() {
                continue;
            }
            // A region that ends below where the per-frame array starts, or
            // begins above where it ends, has no entry to mark — and handing
            // the allocator a frame it has no record of would be a write past
            // the end of that array. `Frames::insert_free` clamps, but saying
            // so here is what makes the clamp a decision rather than luck.
            // And nothing below `FIRST_MANAGED_FRAME`, should a loader or
            // ACPI region start at zero.
            let base = region.base / PAGE_SIZE;
            let first = base.max(FIRST_MANAGED_FRAME);
            let count = (region.len / PAGE_SIZE).saturating_sub(first - base);
            if count == 0 || first < frames.base() || first >= frames.end() {
                continue;
            }
            let count = count.min(frames.end() - first);

            frames.insert_free(first, count);
            match region.kind {
                MemKind::Loader => reclaimed.loader_frames += count,
                _ => reclaimed.acpi_frames += count,
            }
        }
    });

    reclaimed
}

/// Clear a span of slots in the root page table.
///
/// Dead on `AArch64`, and deliberately not behind a conditional: the identity
/// map there is a whole second translation regime that is switched off at
/// `TCR_EL1`, so there is nothing to clear. A `cfg` here would be the first
/// crack in the rule that says architecture differences live under
/// `src/kernel/src/arch/`.
///
/// The one operation that cannot be expressed as unmapping a range: dropping
/// the loader's identity map on x86-64 means removing *everything* below the
/// upper half, and walking it page by page would be half a million
/// descriptors to discover what one store per top-level slot achieves.
#[allow(
    dead_code,
    reason = "only x86-64 drops its identity map this way; see the doc comment"
)]
pub(crate) fn clear_root_slots(slots: core::ops::Range<usize>) {
    let root = ROOT_TABLE.load(Ordering::Relaxed);
    with_tables(|_| {
        for slot in slots {
            KernelPhysMem.write(PhysAddr(root + (slot as u64) * 8), 0);
        }
    });
    crate::smp::flush_tlb_everywhere();
}

/// Zero a frame through the direct map.
///
/// Every path that hands memory to somebody — a fresh page table, an anonymous
/// page, a kernel stack — goes through this rather than repeating the
/// `write_bytes`, because a page handed out still holding the last owner's
/// data is an information leak and from stage 6 the last owner is another
/// process.
pub(crate) fn zero_frame(frame: Frame) {
    // SAFETY: (FRAME) the caller has just taken `frame` from the buddy allocator, so
    // nothing else refers to it, and the direct map covers every frame of RAM
    // and is writable.
    unsafe { core::ptr::write_bytes(physmap(frame * PAGE_SIZE) as *mut u8, 0, PAGE_SIZE as usize) };
}

/// Copy a whole frame through the direct map.
///
/// The copy in copy-on-write, and the only reason this is not
/// [`zero_frame`]'s neighbour by accident: both exist because a frame handed
/// to somebody must hold exactly what that somebody is entitled to see, and
/// this is the case where that is the sharer's current contents rather than
/// zeroes.
///
/// `destination` must be a frame the caller has just allocated and nothing
/// else refers to; `source` may be shared with any number of readers, which is
/// the situation that made the copy necessary.
pub(crate) fn copy_frame(destination: Frame, source: Frame) {
    // SAFETY: (FRAME) the direct map covers every frame of RAM and is writable. The
    // two frames are distinct -- the caller allocated `destination` while
    // `source` was already committed -- so the regions do not overlap, and
    // nothing else refers to `destination`.
    unsafe {
        core::ptr::copy_nonoverlapping(
            physmap(source * PAGE_SIZE) as *const u8,
            physmap(destination * PAGE_SIZE) as *mut u8,
            PAGE_SIZE as usize,
        );
    }
}

// ---------------------------------------------------------------------------
// Frame windows for the self-checks
// ---------------------------------------------------------------------------

/// A way frames reach or leave the allocator, counted for [`FrameWindow`]'s
/// report.
#[derive(Clone, Copy, Debug)]
enum Route {
    /// Taken with [`allocate_frames`].
    Allocated,
    /// Given back with [`deallocate_frames`].
    Freed,
    /// Freed by [`release_frame`] dropping the last reference.
    Released,
    /// Taken by the heap for a slab or a large allocation. A window nets out
    /// only the slab pages, so a move here does not by itself explain a miss:
    /// the report's large-page line says how much of it was large.
    HeapTaken,
    /// Given back by the heap, slab or large alike.
    HeapReturned,
    /// A user page table given back by [`unmap_unwalked`], or by the flush
    /// after an [`unmap_in`].
    UserTables,
    /// A kernel page table given back by a kernel unmap.
    KernelTables,
}

/// How many [`Route`]s there are.
const ROUTES: usize = 7;

/// Frames through each route since boot. Relaxed and lock-free: they are read
/// only by a check that is about to report, and a count a moment stale says
/// the same thing about which route moved.
static ROUTE_COUNTS: [AtomicU64; ROUTES] = [const { AtomicU64::new(0) }; ROUTES];

/// User page tables given back since boot, by [`unmap_unwalked`] or by the
/// flush after an [`unmap_in`]: for the check that the flush is what gives
/// them back.
pub(crate) fn user_tables_given_back() -> u64 {
    ROUTE_COUNTS
        .get(Route::UserTables as usize)
        .map_or(0, |counter| counter.load(Ordering::Relaxed))
}

/// Count `frames` through `route`.
fn count(route: Route, frames: u64) {
    if let Some(counter) = ROUTE_COUNTS.get(route as usize) {
        let _ = counter.fetch_add(frames, Ordering::Relaxed);
    }
}

/// What a frame window holds at one moment: the free frames and the heap's
/// slab pages together, the heap's live bytes and large pages, and every
/// route's count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    frames: u64,
    heap_bytes: usize,
    large_pages: usize,
    routes: [u64; ROUTES],
}

impl Held {
    /// Taken once two reads in a row agree on the frames, so that nothing
    /// freed between reading the free count and the slab count skews the
    /// pair. Only the frames must agree: the heap's bytes move whenever any
    /// processor allocates, console output included, and only feed the
    /// report, so they come from the read that settled the frames.
    ///
    /// A count moves only while other tasks allocate or free -- a draining
    /// reaper, the console thread, a delivery firer -- so after a few quick
    /// re-reads it sleeps between reads rather than holding its processor from
    /// them. Every window is taken in task context after the reaper wait, where
    /// sleeping is allowed; before the scheduler runs it keeps spinning, so a
    /// window opened that early still works. The quick re-reads come first
    /// because the common case settles at once, and a sleep would add a
    /// millisecond to every window. Bounded by the reaper's patience, since a busy
    /// machine may never be still; a count that never settled says so, so a
    /// mismatch from that cause names itself.
    fn now() -> Held {
        let read = || {
            let (slab_pages, large_pages, heap_bytes) = with_heap(|heap| {
                (
                    heap.slab_pages(),
                    heap.large_pages(),
                    heap.allocated_bytes(),
                )
            });
            Held {
                frames: free_frames().saturating_add(slab_pages as u64),
                heap_bytes,
                large_pages,
                routes: core::array::from_fn(|i| {
                    ROUTE_COUNTS
                        .get(i)
                        .map_or(0, |counter| counter.load(Ordering::Relaxed))
                }),
            }
        };
        let deadline =
            crate::timer::now_nanos().saturating_add(crate::sched::REAPER_PATIENCE_NANOS);
        /// Quick re-reads before the reads start sleeping.
        const SPINS: u32 = 8;
        /// How long a read sleeps once the quick re-reads have not settled.
        const SETTLE_NANOS: u64 = 1_000_000;
        let mut last = read();
        let mut reads = 0_u32;
        loop {
            if reads < SPINS || !crate::sched::started() {
                core::hint::spin_loop();
                reads += 1;
            } else {
                crate::sched::sleep_for(SETTLE_NANOS);
            }
            let next = read();
            if next.frames == last.frames {
                return next;
            }
            if crate::timer::now_nanos() >= deadline {
                crate::console::println!(
                    "  frame window: the count never settled, last read {} frames then {}",
                    last.frames,
                    next.frames,
                );
                return next;
            }
            last = next;
        }
    }
}

/// The free frames and the heap's slab pages together, read until stable:
/// what a [`FrameWindow`] holds constant, for a check that compares its own
/// counts.
pub(crate) fn held_frames() -> u64 {
    Held::now().frames
}

/// A self-check's window over the frames, opened after the reaper is quiet
/// and closed the same way.
///
/// What it holds constant is the free frames and the heap's slab pages taken
/// together. A slab page the heap takes or gives back inside the window moves
/// one frame between the two and changes nothing, so the first touch of a size
/// class, or a slab another task drains, no longer reads as a check's leak or
/// as a frame from outside. A large heap allocation stays in the frame count,
/// so a leaked stack, ring or buffer is still a failure.
///
/// So it proves frames and large allocations, and never small objects: a leak
/// of those is invisible to it by construction, because the slab pages such
/// objects force are exactly what it nets out. A check whose point is that a
/// small object is torn down proves that directly, with a `Weak` that must fail
/// to upgrade once the window closes. The heap's live bytes are kept too, and
/// every report prints how they moved, as a clue rather than a verdict: other
/// processors allocate meanwhile.
///
/// Every report on a mismatch prints how each [`Route`] moved, so a failure
/// names the way the frame went.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameWindow {
    opened: Held,
}

impl FrameWindow {
    /// Open a window on the frames as they stand.
    pub(crate) fn open() -> FrameWindow {
        FrameWindow {
            opened: Held::now(),
        }
    }

    /// What the window held when it opened, as [`held_frames`] counts.
    pub(crate) fn held(&self) -> u64 {
        self.opened.frames
    }

    /// Print what the window held at open and holds now, and how the heap
    /// and each route moved between: for a check about to fail on its count.
    pub(crate) fn report(&self, check: &str) {
        self.report_at(check, &Held::now());
    }

    /// Frames kept since the window opened: above zero the check kept them,
    /// below zero something gave frames back inside the window.
    pub(crate) fn kept(&self) -> i64 {
        let now = Held::now();
        i64::try_from(self.opened.frames).unwrap_or(i64::MAX)
            - i64::try_from(now.frames).unwrap_or(i64::MAX)
    }

    /// Print how the heap and each route moved since the window opened.
    fn report_at(&self, check: &str, now: &Held) {
        let routes = |route: Route| {
            let at = |held: &Held| held.routes.get(route as usize).copied().unwrap_or(0);
            at(now).wrapping_sub(at(&self.opened))
        };
        let bytes = i64::try_from(now.heap_bytes).unwrap_or(i64::MAX)
            - i64::try_from(self.opened.heap_bytes).unwrap_or(i64::MAX);
        let large = i64::try_from(now.large_pages).unwrap_or(i64::MAX)
            - i64::try_from(self.opened.large_pages).unwrap_or(i64::MAX);
        crate::console::println!(
            "  {check:<8} held at open {} frames and {} heap bytes, now {} and {}",
            self.opened.frames,
            self.opened.heap_bytes,
            now.frames,
            now.heap_bytes,
        );
        crate::console::println!(
            "  {check:<8} heap {bytes:+} bytes, {large:+} large pages; frames allocated {}, freed {}, released {}, heap taken {}, heap returned {}, user tables {}, kernel tables {}",
            routes(Route::Allocated),
            routes(Route::Freed),
            routes(Route::Released),
            routes(Route::HeapTaken),
            routes(Route::HeapReturned),
            routes(Route::UserTables),
            routes(Route::KernelTables),
        );
    }
}
