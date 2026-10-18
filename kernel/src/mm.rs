//! Physical memory and the kernel heap.
//!
//! Stage 2 of `docs/ROADMAP.md`. Two things happen here, and the second is what
//! the rest of the kernel is waiting for:
//!
//! 1. the buddy allocator is given every usable frame firmware reported, and
//! 2. `alloc` starts working, so `Box`, `Vec` and `BTreeMap` exist.
//!
//! Both allocators themselves live in `libs/` — see `ferrix_frame` and
//! `ferrix_heap` — where they are ordinary Rust that `cargo test`, Miri and a
//! fuzzer can drive. What is here is the part that genuinely needs a machine:
//! deciding where the per-frame array goes, and reaching physical memory
//! through the direct map.
//!
//! # The chicken and the egg
//!
//! The buddy allocator needs one [`PageEntry`] per frame, and that array has to
//! be allocated before there is an allocator. So it is carved out of the front
//! of the largest usable region firmware reported, and that region is then
//! handed to the allocator with the carved part left out. After that, every
//! allocation goes through the allocator like anything else.
//!
//! # One CPU
//!
//! The two globals below are reached without a lock, which is sound today for
//! the reason stated on each: no second CPU has been started and interrupts are
//! masked. Stage 4 brings both of those to an end, and turns each of these into
//! a per-CPU cache in front of a locked global.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::{BootView, MemKind, MemRegion, PAGE_SIZE};
use ferrix_frame::{Frame, Frames, PageEntry};
use ferrix_heap::{Backing, Heap, Request};
use ferrix_paging::{Leaf, MapFlags, Mapper, PhysAddr, PhysMem, Released, VirtAddr, WalkOutcome};

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
}

impl core::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MemoryError::NoUsableMemory => f.write_str("firmware reported no usable memory"),
            MemoryError::NoRoomForPageArray(bytes) => {
                write!(f, "no usable region holds the {bytes}-byte page array")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------

/// The buddy allocator, once [`init`] has run.
struct FrameState(UnsafeCell<Option<Frames<'static>>>);

// SAFETY: early boot is single-threaded — no other CPU has been started and
// interrupts are masked — so there is never a second accessor. Stage 4 replaces
// this with a per-CPU cache in front of a locked global, and the accessors
// below are the only places that have to change.
unsafe impl Sync for FrameState {}

static FRAMES: FrameState = FrameState(UnsafeCell::new(None));

/// The kernel heap.
struct HeapState(UnsafeCell<Heap>);

// SAFETY: as `FrameState`.
unsafe impl Sync for HeapState {}

static HEAP: HeapState = HeapState(UnsafeCell::new(Heap::new()));

/// Base of the direct map, learned from the hand-off.
///
/// An atomic rather than another `UnsafeCell`: it is written once and read from
/// every allocation, and there is no reason to spend an `unsafe` on a `u64`.
static PHYSMAP: AtomicU64 = AtomicU64::new(0);

/// The virtual address at which physical address `phys` is readable.
fn physmap(phys: u64) -> u64 {
    PHYSMAP.load(Ordering::Relaxed) + phys
}

/// The physical address behind a direct-map virtual address.
fn unmap(virt: u64) -> u64 {
    virt - PHYSMAP.load(Ordering::Relaxed)
}

/// Run `body` with the frame allocator, or return `None` before [`init`].
fn with_frames<T>(body: impl FnOnce(&mut Frames<'static>) -> T) -> Option<T> {
    // SAFETY: single-threaded, as documented on the `Sync` impl for
    // `FrameState`. The reference does not escape `body`.
    let slot = unsafe { &mut *FRAMES.0.get() };
    slot.as_mut().map(body)
}

/// Run `body` with the heap.
fn with_heap<T>(body: impl FnOnce(&mut Heap) -> T) -> T {
    // SAFETY: single-threaded, as documented on the `Sync` impl for
    // `HeapState`. The reference does not escape `body`.
    let heap = unsafe { &mut *HEAP.0.get() };
    body(heap)
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
    ROOT_TABLE.store(view.raw().root_table_phys, Ordering::Relaxed);

    let highest = view.max_ram_address().div_ceil(PAGE_SIZE);
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
    let host = largest_usable(view).ok_or(MemoryError::NoUsableMemory)?;
    if host.len < bytes {
        return Err(MemoryError::NoRoomForPageArray(bytes));
    }

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

    // SAFETY: single-threaded, as documented on the `Sync` impl for
    // `FrameState`, and nothing has taken a reference to the slot yet.
    unsafe { *FRAMES.0.get() = Some(frames) };
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

/// The largest region firmware called usable.
fn largest_usable(view: &BootView<'_>) -> Option<MemRegion> {
    view.regions()
        .iter()
        .filter(|region| region.kind.is_free_at_boot())
        .copied()
        .max_by_key(|region| region.len)
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
    // SAFETY: `base` is the start of a region firmware called usable and which
    // is about to be excluded from the allocator, so nothing else refers to it;
    // the direct map covers all of RAM, so `virt` is mapped and writable.
    unsafe { core::ptr::write_bytes(virt.cast::<u8>(), 0, bytes) };

    // SAFETY: the range was just zeroed, is inside a usable region large enough
    // for it, is excluded from the allocator below, and is naturally aligned
    // because the region base is page aligned.
    unsafe { core::slice::from_raw_parts_mut(virt, entries) }
}

/// Hand one usable region to the allocator, skipping the page array.
fn insert_region(frames: &mut Frames<'static>, region: &MemRegion, hole: u64, hole_end: u64) {
    let start = region.base;
    let end = region.end();

    // The common case: the region has nothing to do with the array.
    if end <= hole || start >= hole_end {
        frames.insert_free(start / PAGE_SIZE, region.len / PAGE_SIZE);
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
pub(crate) fn allocate_frames(order: u8) -> Option<Frame> {
    with_frames(|frames| frames.allocate(order))?
}

/// Give back frames taken with [`allocate_frames`].
pub(crate) fn deallocate_frames(frame: Frame, order: u8) {
    let _ = with_frames(|frames| frames.deallocate(frame, order));
}

/// Frames currently free.
pub(crate) fn free_frames() -> u64 {
    with_frames(|frames| frames.free_frames()).unwrap_or(0)
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

// SAFETY: `allocate_pages` returns frames the buddy allocator handed out, which
// are contiguous, page aligned and owned by nobody else, addressed through the
// direct map, which covers all of RAM for the life of the system. `read_link`
// and `write_link` are only ever called by the heap on addresses inside pages
// it took from `allocate_pages` and has not given back, so nothing aliases
// them.
unsafe impl Backing for KernelPages {
    fn allocate_pages(&mut self, order: u8) -> Option<u64> {
        let frame = allocate_frames(order)?;
        Some(physmap(frame * PAGE_SIZE))
    }

    fn deallocate_pages(&mut self, address: u64, order: u8) {
        deallocate_frames(unmap(address) / PAGE_SIZE, order);
    }

    fn read_link(&self, at: u64) -> u64 {
        // SAFETY: as documented on the impl. Eight-byte aligned because every
        // size class is a multiple of eight and slab pages are page aligned.
        unsafe { core::ptr::read_volatile(at as *const u64) }
    }

    fn write_link(&mut self, at: u64, value: u64) {
        // SAFETY: as `read_link`.
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

// SAFETY: `alloc` returns either null or the address of a block of at least
// `layout.size()` bytes, aligned to `layout.align()`, which no other live
// allocation overlaps — that is `ferrix_heap`'s contract, and its tests are
// where it is checked. `dealloc` is only called with a pointer and layout from
// a matching `alloc`, which is what `GlobalAlloc` requires of its caller.
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let request = Request::new(layout.size(), layout.align());
        with_heap(|heap| match heap.allocate(&mut KernelPages, request) {
            Ok(address) => address as *mut u8,
            // A null return is how `GlobalAlloc` reports failure; the caller
            // turns it into whatever it considers appropriate, which for `Box`
            // is the allocation error handler.
            Err(_) => core::ptr::null_mut(),
        })
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let request = Request::new(layout.size(), layout.align());
        with_heap(|heap| heap.deallocate(&mut KernelPages, pointer as u64, request));
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

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
pub(crate) const DEMAND_WINDOW: u64 = ferrix_bootinfo::KERNEL_VMAP_BASE + 0x2000_0000;

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

// SAFETY: `read` and `write` go through the direct map, which covers every byte
// of RAM and is the only mapping of it the kernel holds, so nothing can alias
// them. `allocate_table` returns a frame from the buddy allocator, which hands
// each one out once; it is page aligned because frames are, and zeroed here
// before it is returned.
unsafe impl PhysMem for KernelPhysMem {
    fn read(&self, at: PhysAddr) -> u64 {
        // SAFETY: as documented on the impl; `at` is a descriptor address.
        unsafe { core::ptr::read_volatile(physmap(at.0) as *const u64) }
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        // SAFETY: as documented on the impl.
        unsafe { core::ptr::write_volatile(physmap(at.0) as *mut u64, value) };
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        let frame = allocate_frames(0)?;
        let address = frame * PAGE_SIZE;
        // A page table built on someone else's leftovers translates to wherever
        // they pointed, so this zeroing is not optional.
        //
        // SAFETY: the frame was just allocated to us and nothing else refers to
        // it; the direct map makes it writable.
        unsafe { core::ptr::write_bytes(physmap(address) as *mut u8, 0, PAGE_SIZE as usize) };
        Some(PhysAddr(address))
    }
}

/// Map `len` bytes of kernel address space at `virt` onto `phys`.
pub(crate) fn map_kernel(
    virt: u64,
    phys: u64,
    len: u64,
    flags: MapFlags,
) -> Result<(), ferrix_paging::MapError> {
    let root = PhysAddr(ROOT_TABLE.load(Ordering::Relaxed));
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(root);
    mapper.map_range(
        &mut KernelPhysMem,
        VirtAddr(virt),
        PhysAddr(phys),
        len.next_multiple_of(PAGE_SIZE),
        flags,
    )?;
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
    // SAFETY: the frame was just allocated to us and the direct map covers it.
    unsafe { core::ptr::write_bytes(physmap(frame * PAGE_SIZE) as *mut u8, 0, PAGE_SIZE as usize) };

    map_kernel(page, frame * PAGE_SIZE, PAGE_SIZE, MapFlags::KERNEL_DATA)
        .map_err(|_| MemoryError::NoUsableMemory)
}

/// Translate a kernel virtual address the way the hardware would.
pub(crate) fn translate(virt: u64) -> Option<u64> {
    kernel_mapper()
        .translate(&KernelPhysMem, VirtAddr(virt))
        .map(|at| at.0)
}

/// The mapper over the live kernel page tables.
fn kernel_mapper() -> Mapper<crate::arch::PageEncoding> {
    Mapper::new(PhysAddr(ROOT_TABLE.load(Ordering::Relaxed)))
}

/// Remove `len` bytes of kernel mapping at `virt`.
///
/// `released` is called once per leaf with the frame number and the order of
/// the block it was mapped at, and is where the caller decides whether the
/// memory behind the mapping goes back to the buddy allocator. A `vmap` of
/// anonymous pages frees them; a device window never allocated them.
pub(crate) fn unmap_kernel(
    virt: u64,
    len: u64,
    mut released: impl FnMut(Frame, u8),
) -> Result<u64, ferrix_paging::MapError> {
    let removed = kernel_mapper().unmap_range(
        &mut KernelPhysMem,
        VirtAddr(virt),
        len.next_multiple_of(PAGE_SIZE),
        |freed| match freed {
            Released::Page { phys, level } => {
                // Levels run root-to-leaf and orders run small-to-large, so
                // the conversion is a subtraction rather than a table: a
                // level-3 leaf is order 0 and a 2 MiB block is order 9.
                let order = (ferrix_paging::Level::PAGE.depth() - level.depth()) * 9;
                released(phys.0 / PAGE_SIZE, order);
            }
            // A page table the mapper allocated through `KernelPhysMem`, which
            // took it from the buddy allocator. It goes straight back there
            // rather than to the caller: the caller asked to unmap a range and
            // has no idea a table existed, and telling it about one would make
            // every `released` closure in the tree have to know.
            Released::Table { phys } => deallocate_frames(phys.0 / PAGE_SIZE, 0),
        },
    )?;

    // Until this runs the old translation is still in the TLB, and a read
    // through it succeeds against a frame that now belongs to somebody else.
    // Unmapping without invalidating is worse than not unmapping at all: it
    // looks like it worked.
    crate::arch::flush_tlb();
    Ok(removed)
}

/// Change what an existing kernel mapping permits.
pub(crate) fn protect_kernel(
    virt: u64,
    len: u64,
    flags: MapFlags,
) -> Result<(), ferrix_paging::MapError> {
    kernel_mapper().protect_range(
        &mut KernelPhysMem,
        VirtAddr(virt),
        len.next_multiple_of(PAGE_SIZE),
        flags,
    )?;
    crate::arch::flush_tlb();
    Ok(())
}

/// Visit every leaf in the tree rooted at `root`, in address order.
fn sweep(root: u64, visit: impl FnMut(Leaf) -> bool) -> WalkOutcome {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    mapper.for_each_leaf(&KernelPhysMem, visit)
}

/// What `virt` is mapped as, or `None` if it is not mapped.
pub(crate) fn permissions_of(virt: u64) -> Option<MapFlags> {
    let mut found = None;
    let _ = sweep(ROOT_TABLE.load(Ordering::Relaxed), |leaf| {
        if virt >= leaf.virt.0 && virt < leaf.virt.0 + leaf.bytes() {
            found = Some(leaf.flags);
            return false;
        }
        true
    });
    found
}

// ---------------------------------------------------------------------------
// W^X
// ---------------------------------------------------------------------------

/// A mapping that is both writable and executable.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WriteExecute {
    /// Where it starts.
    pub(crate) virt: u64,
    /// How much of the address space it covers.
    pub(crate) len: u64,
}

/// What the W^X sweep found.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WxReport {
    /// Mappings the hardware can see.
    pub(crate) leaves: u64,
    /// Of those, how many are executable at all.
    pub(crate) executable: u64,
}

/// Walk the live page tables and require that nothing is writable *and*
/// executable.
///
/// **A measurement of the machine, not of the kernel's intentions.** Every
/// other check of this kind in the tree asserts that a function was called
/// with the right flags; this one reads the descriptors the hardware is going
/// to walk, including the ones the loader wrote and the ones an earlier stage
/// installed and forgot about. The loader's identity map is exactly such a
/// mapping — it has to be writable and executable, because the instruction
/// after the page table switch is fetched through it — so this sweep only
/// passes once that map has been dropped, which is why the two land in the
/// same stage.
///
/// # Errors
///
/// The first offending mapping, which is enough: the fix for one is the fix
/// for all of them, and reporting the address of the first is what makes it
/// findable.
pub(crate) fn check_w_xor_x(view: &BootView<'_>) -> Result<WxReport, WriteExecute> {
    let mut report = WxReport {
        leaves: 0,
        executable: 0,
    };
    let mut offender = None;

    // Every root the hardware can translate through, not only the kernel's.
    // On `AArch64` the identity map is a second regime with its own base
    // register, so a sweep of the kernel's tables alone would report a clean
    // machine while the CPU could still fetch from a writable page.
    let roots = [
        Some(ROOT_TABLE.load(Ordering::Relaxed)),
        crate::arch::identity_root(view),
    ];

    for root in roots.into_iter().flatten() {
        let outcome = sweep(root, |leaf| {
            if leaf.flags.execute {
                report.executable += 1;
            }
            if leaf.is_write_execute() {
                offender = Some(WriteExecute {
                    virt: leaf.virt.0,
                    len: leaf.bytes(),
                });
                return false;
            }
            true
        });
        report.leaves += outcome.leaves;
        if offender.is_some() {
            break;
        }
    }

    match offender {
        Some(found) => Err(found),
        None => Ok(report),
    }
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
///   through the direct map by `crate::acpi`, so this must not run until the
///   last parse has finished; it is called from `kmain` after interrupt
///   bring-up for exactly that reason.
///
/// Deliberately *not* reclaimed: [`MemKind::PageTables`], which the kernel is
/// running on; [`MemKind::BootStack`], which it is running on too; and
/// [`MemKind::BootInfo`], which `BootView` borrows for the life of the system.
///
/// # Safety
///
/// Every reference into loader or ACPI-reclaim memory must be dead. In this
/// kernel that means being called from `kmain` after the last use of
/// `crate::acpi::Firmware`, and it is called exactly once.
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
            let first = region.base / PAGE_SIZE;
            let count = region.len / PAGE_SIZE;
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
/// `kernel/src/arch/`.
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
    for slot in slots {
        KernelPhysMem.write(PhysAddr(root + (slot as u64) * 8), 0);
    }
    crate::arch::flush_tlb();
}

/// Zero a frame through the direct map.
///
/// Every path that hands memory to somebody — a fresh page table, an anonymous
/// page, a kernel stack — goes through this rather than repeating the
/// `write_bytes`, because a page handed out still holding the last owner's
/// data is an information leak and from stage 6 the last owner is another
/// process.
pub(crate) fn zero_frame(frame: Frame) {
    // SAFETY: the caller has just taken `frame` from the buddy allocator, so
    // nothing else refers to it, and the direct map covers every frame of RAM
    // and is writable.
    unsafe { core::ptr::write_bytes(physmap(frame * PAGE_SIZE) as *mut u8, 0, PAGE_SIZE as usize) };
}
