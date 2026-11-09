//! Kernel heap requests above a page, from single frames
//! (`docs/ROBUSTNESS.md` §2.6, `L.mm.141`-`L.mm.145`).
//!
//! A heap request above a page is a run of frames from the buddy, and on a
//! machine whose free memory is in single frames there may be no run of its
//! order while gigabytes are free: an infallible 128 KiB request then stopped
//! the machine (FX-0008). This is Linux's `kvmalloc` fallback: where the
//! buddy refuses, the request is served from single frames mapped into an
//! area of kernel address space set aside at boot. The heap tries it only
//! after the buddy has refused, so nothing changes while a run exists.
//!
//! The area's address bookkeeping is [`ferrix_frame::area`]. What is here
//! needs the machine:
//!
//! * **Tables made at boot.** Every page-level table of the area is made by
//!   [`init`] and never pruned or freed, and the area is aligned to 2 MiB,
//!   so none is shared with an arena neighbour. Nothing but this module
//!   writes a descriptor inside the area, and each leaf only by the one
//!   owner of the block it lies in. So a request allocates nothing on its
//!   way (no table, no heap), and a leaf is written with no lock: neither
//!   `TABLES` nor `HEAP` is taken here, only the area's own lock, alone, and
//!   the frame allocator's, never nested.
//! * **No shootdown on a free.** A free clears its leaves and invalidates
//!   each page; where that invalidation is broadcast (both Arm builds) its
//!   frames go back at once. Elsewhere (x86-64) another processor may still
//!   hold a translation, so the frames are kept, chained through their own
//!   first word, until a purge's one shootdown has returned (`H.MEM.17`).
//!   The block's addresses wait on the lazy list for that purge in every
//!   build. The purge runs only where the caller may block: on a free or
//!   an allocation that may, and otherwise in the reaper, which a free past
//!   the lazy threshold asks for.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::{KERNEL_VMAP_BASE, KERNEL_VMAP_SIZE, PAGE_SIZE};
use ferrix_frame::area::{self, Area, Block, UNIT_PAGES};
use ferrix_frame::{Frame, PageEntry};
use ferrix_heap::Request;
use ferrix_paging::{MapError, MapFlags, Mapper, PhysAddr, PhysMem, VirtAddr};
use ferrix_sync::IrqSpinLock;

use super::reservation::Floor;
use crate::vmap;

/// The area's bookkeeping, once [`init`] has run. Interrupt-masking, since a
/// free may come from an interrupt; a leaf lock, taken alone.
static AREA: IrqSpinLock<Option<Area<'static>>, crate::arch::Irq> = IrqSpinLock::new(None);

/// The area's first address, and one past its last: 0 and 0 while it is off.
static BASE: AtomicU64 = AtomicU64::new(0);
static END: AtomicU64 = AtomicU64::new(0);

/// Lazy units past which a free asks for a purge: an eighth of the area.
static LAZY_MAX: AtomicU64 = AtomicU64::new(u64::MAX);

/// Set by a free past [`LAZY_MAX`] that may not block, for the reaper.
static PURGE_WANTED: AtomicBool = AtomicBool::new(false);

/// Set while one purge runs: one at a time.
static PURGING: AtomicBool = AtomicBool::new(false);

/// Frames kept for the next purge's shootdown, now (x86-64 only).
static KEPT: AtomicU64 = AtomicU64::new(0);

/// The most frames ever kept at once, for the boot report.
static MOST_KEPT: AtomicU64 = AtomicU64::new(0);

/// Requests served, purges made, and the defects counted rather than acted
/// on: a free of an address in the kernel's dynamic space outside the area
/// (`L.mm.142`), a leaf found present at a map or absent at a free, or a
/// block retired twice (two owners, `L.mm.144`), and a bad link in the kept
/// frames' chain (`L.mm.143`). The three defect counts are 0 on a correct
/// kernel, and the boot check holds them to it.
static SERVED: AtomicU64 = AtomicU64::new(0);
static PURGES: AtomicU64 = AtomicU64::new(0);
static OUT_OF_AREA: AtomicU64 = AtomicU64::new(0);
static TWO_OWNERS: AtomicU64 = AtomicU64::new(0);
static BAD_LINKS: AtomicU64 = AtomicU64::new(0);

/// The alignment of the area's base and end: the span one page-level table
/// maps, on x86-64, AArch64 with a 4 KiB granule and LPAE alike (R6F.3).
const AREA_ALIGN: u64 = 2 * 1024 * 1024;

/// Bytes in one unit of the area's buddy.
const UNIT_BYTES: u64 = UNIT_PAGES * PAGE_SIZE;

/// The owner a kept frame's record carries while it waits on the chain: no
/// job's slot (it has [`super::TABLE_OWNER`]'s bit and a slot past every
/// limit), so a stray uncharge fails closed. Put back to none before the
/// frame is freed.
const KEPT_OWNER: u32 = u32::MAX - 1;

/// The area's size for a machine of `ram` bytes: an eighth of RAM between
/// 64 MiB and 2 GiB on the 64-bit pair, a quarter between 16 MiB and 128 MiB
/// in ARMv7-A's small kernel half; a whole number of [`AREA_ALIGN`]s.
const fn area_bytes(ram: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    let (share, least, most) = if cfg!(target_pointer_width = "64") {
        (ram / 8, 64 * MIB, 2048 * MIB)
    } else {
        (ram / 4, 16 * MIB, 128 * MIB)
    };
    let bytes = if share < least {
        least
    } else if share > most {
        most
    } else {
        share
    };
    bytes & !(AREA_ALIGN - 1)
}

/// Set the area up for a machine of `managed` frames: its span set aside in
/// the arena, every page-level table over it made, its side arrays mapped.
/// Before the first process, so that x86-64's roots copy root slots that
/// already name the area's tables.
///
/// # Errors
///
/// What failed, with the large path left off and the heap as before.
pub fn init(managed: u64) -> Result<(), &'static str> {
    let bytes = area_bytes(managed.saturating_mul(PAGE_SIZE));
    let units = bytes / UNIT_BYTES;
    if units > area::MOST_UNITS {
        return Err("the large-request area is too large for its bookkeeping");
    }
    let base = vmap::set_aside(bytes, AREA_ALIGN)
        .map_err(|_| "no address space for the large-request area")?;
    if base % AREA_ALIGN != 0 {
        return Err("the large-request area is not aligned to 2 MiB");
    }
    super::with_tables(|_| {
        mapper().populate(
            &mut super::KernelPhysMem,
            VirtAddr(base),
            bytes,
            MapFlags::KERNEL_DATA,
        )
    })
    .map_err(|_| "no frames for the large-request area's page tables")?;
    let count = usize::try_from(units).map_err(|_| "too many units")?;
    let entries = side_array::<PageEntry>(count)?;
    let lazy = side_array::<u32>(count)?;
    let made = Area::new(entries, lazy, units).ok_or("the area's side arrays are short")?;
    *AREA.lock() = Some(made);
    LAZY_MAX.store(units / 8, Ordering::Relaxed);
    END.store(base + bytes, Ordering::Relaxed);
    BASE.store(base, Ordering::Release);
    Ok(())
}

/// `count` zeroed `T`s on pages of their own, for the system's life.
fn side_array<T>(count: usize) -> Result<&'static mut [T], &'static str> {
    let bytes = count
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or("the area's side array is too large")?;
    let mapping = vmap::allocate(bytes.div_ceil(PAGE_SIZE).max(1), MapFlags::KERNEL_DATA)
        .map_err(|_| "no memory for the large-request area's side arrays")?;
    // SAFETY: (KMEM) the mapping is at least `count` `T`s of zeroed, page
    // aligned kernel memory that nothing else names and that is never freed;
    // both `T`s it is used for (`PageEntry`, `u32`) are valid all zero
    // (`PageEntry`'s `repr(u8)` state is `Reserved` at zero).
    Ok(unsafe { core::slice::from_raw_parts_mut(mapping.base as usize as *mut T, count) })
}

/// The kernel's tables, pages only: the area is mapped a page at a time.
pub(super) fn mapper() -> Mapper<crate::arch::PageEncoding> {
    let mut mapper = Mapper::new(PhysAddr(super::ROOT_TABLE.load(Ordering::Relaxed)));
    mapper.pages_only();
    mapper
}

/// The area's tables, as a mapping into it reaches them: through the direct
/// map, allocating none.
pub(super) struct AreaTables;

// SAFETY: (TRANSLATE) reads and writes go through the direct map, as
// `KernelPhysMem`'s do, at descriptor addresses of the kernel's own tables;
// no table is ever handed out. A write is one aligned 64-bit store on the
// 64-bit pair, and on ARMv7-A two 32-bit stores in the order that never shows
// a walker a valid descriptor with half an address (R6F.4).
unsafe impl PhysMem for AreaTables {
    fn read(&self, at: PhysAddr) -> u64 {
        super::KernelPhysMem.read(at)
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        #[cfg(target_pointer_width = "64")]
        super::KernelPhysMem.write(at, value);
        #[cfg(target_pointer_width = "32")]
        {
            // Little-endian: the low word, which holds the valid bit, first in
            // memory and the high word four bytes on.
            let low = (super::physmap(at.0), value as u32);
            let high = (super::physmap(at.0) + 4, (value >> 32) as u32);
            // Valid: the address and attributes first, then the valid bit.
            // Invalid: the valid bit first.
            let (first, second) = if value & 1 != 0 {
                (high, low)
            } else {
                (low, high)
            };
            store_word(first.0, first.1);
            core::sync::atomic::fence(Ordering::SeqCst);
            store_word(second.0, second.1);
        }
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        None
    }
}

/// Store one 32-bit half of a descriptor at `at`, a direct-map address.
#[cfg(target_pointer_width = "32")]
fn store_word(at: u64, value: u32) {
    // SAFETY: (TRANSLATE) `at` is the low or high word of an eight-byte aligned
    // descriptor in one of the area's tables, reached through the direct map,
    // which `AreaTables` writes for the leaf's one owner alone.
    unsafe { core::ptr::write_volatile(at as usize as *mut u32, value) };
}

/// The pages a request of `request` maps, if the large path may serve it:
/// above a page, at most `2^MAX_ORDER` pages, aligned to a page at most, with
/// the area set up, in a build whose heap is not routed by the charge (K16).
fn pages_for(request: Request) -> Option<u64> {
    let size = u64::try_from(request.size).ok()?;
    let fits = size > PAGE_SIZE
        && size <= PAGE_SIZE << super::MAX_ORDER
        && u64::try_from(request.align).is_ok_and(|align| align <= PAGE_SIZE)
        && !cfg!(ferrix_kernel_memory = "retyped")
        && BASE.load(Ordering::Acquire) != 0;
    fits.then(|| size.div_ceil(PAGE_SIZE))
}

/// Serve `request` from single frames taken for `floor`, or `None`: for a
/// request the path does not serve, with no block of address space free (a
/// purge tried first where the caller may block), or with a frame refused,
/// in which case every frame taken goes back and the block waits on the
/// lazy list (`L.mm.144`). Nothing is allocated on the way.
pub(super) fn allocate(request: Request, floor: Floor) -> Option<u64> {
    let pages = pages_for(request)?;
    let block = take_block(pages)?;
    let base = BASE.load(Ordering::Relaxed) + block.first_page() * PAGE_SIZE;
    for page in 0..pages {
        let at = base + page * PAGE_SIZE;
        let Some(frame) = super::take_frames(0, floor) else {
            retire(block, release_pages(base, page));
            return None;
        };
        if let Err(error) = mapper().map_range(
            &mut AreaTables,
            VirtAddr(at),
            PhysAddr(frame * PAGE_SIZE),
            PAGE_SIZE,
            MapFlags::KERNEL_DATA,
        ) {
            if matches!(error, MapError::AlreadyMapped(_)) {
                let _ = TWO_OWNERS.fetch_add(1, Ordering::Relaxed);
            }
            super::deallocate_frames(frame, 0);
            retire(block, release_pages(base, page));
            return None;
        }
    }
    crate::arch::publish_tables(base);
    let _ = SERVED.fetch_add(1, Ordering::Relaxed);
    Some(base)
}

/// A block for `pages`, purging once first where none is free, lazy blocks
/// wait and the caller may block.
fn take_block(pages: u64) -> Option<Block> {
    let (block, lazy) = with_area(|area| (area.take(pages), area.lazy_units()))?;
    if block.is_some() || lazy == 0 || !purge() {
        return block;
    }
    with_area(|area| area.take(pages)).flatten()
}

/// Whether `address` is in the kernel's dynamic space, which no heap memory
/// but the area's is: one subtraction and one compare with constants.
pub(super) const fn in_dynamic_space(address: u64) -> bool {
    address.wrapping_sub(KERNEL_VMAP_BASE) < KERNEL_VMAP_SIZE
}

/// Free a block [`allocate`] returned for a request of `size` bytes: its
/// leaves cleared, its frames back (or kept, x86-64), its addresses on the
/// lazy list (`L.mm.142`, `L.mm.143`). An address outside the area, or not a
/// block's base, is counted and not freed.
pub(super) fn free(address: u64, size: usize) {
    let base = BASE.load(Ordering::Acquire);
    let pages = u64::try_from(size).map_or(0, |size| size.div_ceil(PAGE_SIZE));
    let offset = address.wrapping_sub(base);
    let block = area::order_for(pages).map(|order| Block {
        unit: offset / UNIT_BYTES,
        order,
    });
    let owned = block.filter(|&block| owns(base, offset, block));
    let Some(block) = owned else {
        let _ = OUT_OF_AREA.fetch_add(1, Ordering::Relaxed);
        return;
    };
    retire(block, release_pages(address, pages));
}

/// Whether the address `offset` bytes from the area's `base` is the base of
/// `block`, a live block of the area: inside it (a lower address wraps to an
/// offset past its end), on a unit, and under the area's lock an allocated
/// head of the block's order not on the lazy list. Asked before any leaf is
/// cleared, so that a free of what the area did not hand out changes nothing
/// (`L.mm.142`); `retire` asks again.
fn owns(base: u64, offset: u64, block: Block) -> bool {
    base != 0
        && offset < END.load(Ordering::Relaxed).wrapping_sub(base)
        && offset.is_multiple_of(UNIT_BYTES)
        && with_area(|area| area.is_live(block)).unwrap_or(false)
}

/// The frames a free kept, chained through their own first word.
#[derive(Clone, Copy, Debug, Default)]
struct Chain {
    /// The newest kept frame, which links to the next.
    head: Option<Frame>,
    /// The oldest, whose link is written as the chain joins the area's.
    tail: Option<Frame>,
    /// How many.
    count: u64,
}

/// Clear the first `pages` leaves from `base`, invalidating each, and give
/// each frame back where the invalidation is broadcast or keep it on the
/// answered chain where it is not.
fn release_pages(base: u64, pages: u64) -> Chain {
    let mut chain = Chain::default();
    for page in 0..pages {
        let at = base + page * PAGE_SIZE;
        let Ok(Some(phys)) = mapper().take_page(&mut AreaTables, VirtAddr(at)) else {
            let _ = TWO_OWNERS.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        crate::arch::flush_tlb_page(at);
        let frame = phys.0 / PAGE_SIZE;
        if crate::arch::TLB_FLUSH_IS_BROADCAST {
            // Complete on every processor at its `dsb ish`: nobody holds a
            // translation of the frame any more.
            super::deallocate_frames(frame, 0);
            continue;
        }
        let _ = super::with_frames(|frames| frames.set_owner(frame, KEPT_OWNER));
        link(frame, chain.head);
        chain.head = Some(frame);
        chain.tail = chain.tail.or(Some(frame));
        chain.count += 1;
    }
    chain
}

/// Write `next` into `frame`'s first word: the kept chain's link, through
/// the direct map. `0` ends the chain; a frame `f` is `f + 1`.
fn link(frame: Frame, next: Option<Frame>) {
    // `frame` is a frame of a freed block, kept: this module's alone until
    // the purge gives it back, and the direct map reaches it (x86-64, the
    // only build that keeps frames). Its first word is written as a table's
    // descriptor is, through the direct map.
    super::KernelPhysMem.write(PhysAddr(frame * PAGE_SIZE), next.map_or(0, |next| next + 1));
}

/// The link `frame`'s first word holds.
fn next_of(frame: Frame) -> Option<Frame> {
    // As `link`'s, read by the purge, which has detached the chain and so
    // holds it alone.
    super::KernelPhysMem
        .read(PhysAddr(frame * PAGE_SIZE))
        .checked_sub(1)
}

/// Put `block` on the lazy list, with `chain`'s frames joined to the kept
/// chain, and ask for a purge past the threshold: made here where this may
/// block, else by the reaper.
fn retire(block: Block, chain: Chain) {
    let answer = with_area(|area| {
        if let (Some(head), Some(tail)) = (chain.head, chain.tail) {
            link(tail, area.kept());
            area.keep(head, chain.count);
        }
        // In the hold that changed it, so that a purge on another processor
        // cannot see the count go below what it detached.
        KEPT.store(area.kept_count(), Ordering::Relaxed);
        (
            area.retire(block).is_ok(),
            area.lazy_units(),
            area.kept_count(),
        )
    });
    let Some((retired, lazy, kept)) = answer else {
        return;
    };
    if !retired {
        let _ = TWO_OWNERS.fetch_add(1, Ordering::Relaxed);
    }
    let _ = MOST_KEPT.fetch_max(kept, Ordering::Relaxed);
    if lazy > LAZY_MAX.load(Ordering::Relaxed) && !purge() {
        PURGE_WANTED.store(true, Ordering::Release);
    }
}

/// Make the lazy list's addresses, and the kept frames, free again, under
/// one shootdown (`L.mm.143`): the list and chain detached, the shootdown
/// made and returned, and only then each frame and block given back. Only
/// where the caller may block, and one purge at a time; whether this one
/// gave anything back.
pub(crate) fn purge() -> bool {
    if !crate::sched::may_block() || PURGING.swap(true, Ordering::Acquire) {
        return false;
    }
    PURGE_WANTED.store(false, Ordering::Relaxed);
    let detached = with_area(|area| {
        let detached = area.detach();
        KEPT.store(0, Ordering::Relaxed);
        (!detached.is_empty()).then_some(detached)
    })
    .flatten();
    let Some(mut detached) = detached else {
        PURGING.store(false, Ordering::Release);
        return false;
    };
    crate::smp::flush_tlb_everywhere();
    give_back_kept(detached.kept, detached.kept_count);
    while with_area(|area| area.give_back(&mut detached))
        .flatten()
        .is_some()
    {}
    let _ = PURGES.fetch_add(1, Ordering::Relaxed);
    PURGING.store(false, Ordering::Release);
    true
}

/// Give back the `count` frames of the kept chain from `head`, once the
/// shootdown covering them has returned. Each link is checked before it is
/// followed: a frame the allocator knows, allocated and marked kept, and no
/// more of them than the chain should hold. A bad link is counted and ends
/// the chain, whose rest is lost rather than freed (R6F.8).
fn give_back_kept(head: Option<Frame>, count: u64) {
    let mut next = head;
    let mut given = 0;
    while let Some(frame) = next {
        let kept = given < count
            && super::with_frames(|frames| {
                frames.state(frame) == Some(ferrix_frame::State::Allocated)
                    && frames.owner(frame) == KEPT_OWNER
                    && frames.set_owner(frame, ferrix_frame::NO_OWNER).is_ok()
            })
            .unwrap_or(false);
        if !kept {
            let _ = BAD_LINKS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        next = next_of(frame);
        super::deallocate_frames(frame, 0);
        given += 1;
    }
    if given != count {
        let _ = BAD_LINKS.fetch_add(1, Ordering::Relaxed);
    }
}

/// A purge, where frames are kept and the caller may block: what comes
/// before a reclaim counts a failure, before any kill, and before a
/// program's frame is refused at the floor (R6F.7). One load while nothing
/// is kept, which is always on the Arm pair.
pub(crate) fn purge_held() -> bool {
    KEPT.load(Ordering::Relaxed) != 0 && purge()
}

/// Whether a free asked the reaper for a purge (`sched::leftovers`).
pub(crate) fn purge_wanted() -> bool {
    PURGE_WANTED.load(Ordering::Relaxed)
}

/// `body` on the area under its lock; `None` while it is off.
fn with_area<T>(body: impl FnOnce(&mut Area<'static>) -> T) -> Option<T> {
    AREA.lock().as_mut().map(body)
}

/// The area's counts, for the boot report and the check.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counts {
    /// Requests served.
    pub(crate) served: u64,
    /// Purges made.
    pub(crate) purges: u64,
    /// Units on the lazy list.
    pub(crate) lazy_units: u64,
    /// Frames kept now.
    pub(crate) kept: u64,
    /// The most frames ever kept at once.
    pub(crate) most_kept: u64,
    /// Frees of an address outside the area (a defect).
    pub(crate) out_of_area: u64,
    /// Leaves found present at a map, absent at a free, or a block retired
    /// twice (a defect).
    pub(crate) two_owners: u64,
    /// Bad links in the kept chain (a defect).
    pub(crate) bad_links: u64,
}

/// The area's counts now.
pub(crate) fn counts() -> Counts {
    let lazy_units = with_area(|area| area.lazy_units()).unwrap_or(0);
    Counts {
        served: SERVED.load(Ordering::Relaxed),
        purges: PURGES.load(Ordering::Relaxed),
        lazy_units,
        kept: KEPT.load(Ordering::Relaxed),
        most_kept: MOST_KEPT.load(Ordering::Relaxed),
        out_of_area: OUT_OF_AREA.load(Ordering::Relaxed),
        two_owners: TWO_OWNERS.load(Ordering::Relaxed),
        bad_links: BAD_LINKS.load(Ordering::Relaxed),
    }
}

/// The area's span, `(base, end)`, or `(0, 0)` while it is off.
pub(crate) fn span() -> (u64, u64) {
    (BASE.load(Ordering::Acquire), END.load(Ordering::Relaxed))
}

// CHECKS-ONLY: mm/large_check.rs
/// Set the lazy threshold to `units` (for step 9's case, so that it is
/// passed without holding hundreds of MiB), and answer the one it replaces.
pub(crate) fn set_lazy_max_for_check(units: u64) -> u64 {
    LAZY_MAX.swap(units, Ordering::Relaxed)
}
