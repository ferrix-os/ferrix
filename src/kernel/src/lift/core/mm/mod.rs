//! Memory as linuxd sees it (docs/microkernel/M5-LIFT.md §3).
//!
//! **Frames are pages of one VMO, the arena**, mapped once into the shared
//! region: a `Frame`'s number is its page's offset in it, and `direct_map` is that
//! mapping, so the load's and `user::vmo`'s frame arithmetic runs unchanged. A
//! program's address space keeps its real page tables here as a **shadow**, in
//! arena frames, walked by the same `ferrix-paging`; the kernel's restricted space
//! is a cache of it, filled by `SPACE_MAP` of arena pages at a fault
//! (`crate::fill`) and emptied by `SPACE_UNMAP` at every unmap below.

pub mod scrub;

use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::SpinLock as PlainLock;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_frame::{Frame, Frames, PageEntry};
use ferrix_paging::{MapFlags, Mapper, PhysAddr, PhysMem, Released, VirtAddr};

use crate::fallible;

/// Frames in the arena. Untouched pages cost nothing.
const ARENA_FRAMES: u64 = (4_u64 << 30) / PAGE_SIZE;

static ARENA_AT: AtomicU64 = AtomicU64::new(0);
static MANAGED: AtomicU64 = AtomicU64::new(0);
static FRAMES: PlainLock<Option<Frames<'static>>> = PlainLock::new(None);

fn with_frames<T>(body: impl FnOnce(&mut Frames<'static>) -> T) -> Option<T> {
    FRAMES.lock().as_mut().map(body)
}

/// Make the arena: before anything takes a frame. `at` is where its VMO is
/// mapped, `table` memory for the frame records.
pub fn init(at: u64, frames: u64) {
    ARENA_AT.store(at, Ordering::Release);
    let frames = frames.min(ARENA_FRAMES);
    let entries = Frames::entries_needed(1, frames);
    let layout = core::alloc::Layout::array::<PageEntry>(entries).expect("the frame table's size");
    // SAFETY: not zero sized; the table lives as long as linuxd.
    let table = unsafe { alloc::alloc::alloc_zeroed(layout) }.cast::<PageEntry>();
    assert!(!table.is_null(), "no memory for the frame table");
    // SAFETY: just allocated, `entries` long, zero: Frames::new initialises it.
    let table = unsafe { core::slice::from_raw_parts_mut(table, entries) };
    let frames_wanted = frames;
    let mut frames = Frames::new(table, 1);
    frames.insert_free(1, frames_wanted - 1);
    MANAGED.store(frames.managed_frames(), Ordering::Release);
    *FRAMES.lock() = Some(frames);
}

/// Arena pages by their "physical" address, as `ferrix-paging` reads them.
#[derive(Debug, Clone, Copy)]
pub struct KernelPhysMem;

// SAFETY: every address the tables name is a frame in the arena; a table comes
// from the allocator, zeroed, and is the shadow's until unlinked.
unsafe impl PhysMem for KernelPhysMem {
    fn read(&self, at: PhysAddr) -> u64 {
        // SAFETY: as the impl says.
        unsafe { core::ptr::read_volatile(direct_map(at.0) as *const u64) }
    }
    fn write(&mut self, at: PhysAddr, value: u64) {
        // SAFETY: as the impl says.
        unsafe { core::ptr::write_volatile(direct_map(at.0) as *mut u64, value) }
    }
    fn allocate_table(&mut self) -> Option<PhysAddr> {
        let frame = allocate_frames(0)?;
        zero_frame(frame);
        Some(PhysAddr(frame * PAGE_SIZE))
    }
}

pub type Reservation = u8;
pub const NO_RESERVATION: Reservation = 0;
pub mod reservation {
    pub const NONE: u8 = 0;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Short(u64),
    Unbound,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retired {
    Claimed,
    Marked,
    Already,
    Full,
    NotCovered,
}
/// Frames a program's page does not get: the heap is on the same pool, and a
/// section the load entered on `reserve` allocates without being able to refuse.
const KEPT_FROM_PROGRAMS: u64 = 16 << 20 >> 12;

/// The shim's section guard.
#[derive(Debug)]
pub struct Reserved;
pub fn reserve(large: Option<core::alloc::Layout>) -> Result<Reserved, fallible::AllocError> {
    let wanted = large.map_or(0, |layout| layout.size() as u64 / PAGE_SIZE);
    if free_frames() < KEPT_FROM_PROGRAMS / 2 + wanted {
        return Err(fallible::AllocError);
    }
    Ok(Reserved)
}
pub fn reservation_bind() -> Option<Reservation> {
    None
}
pub fn reservation_resize(entry: Reservation, frames: u64) -> Result<(), Refused> {
    Err(Refused::Unbound)
}
pub fn reservation_unbind(entry: Reservation) {}
pub fn reservation_grow(entry: Reservation, frames: u64) -> Result<(), Refused> {
    Err(Refused::Unbound)
}
/// The shim's heap scope: linuxd binds no reservation, so there is none to
/// enter (`mm::heap_scope`, L.mm.140).
#[derive(Debug)]
pub struct HeapScope;
pub fn heap_scope(entry: Reservation) -> Option<HeapScope> {
    None
}
pub fn heap_scope_suspended() -> Option<HeapScope> {
    None
}
pub fn heap_scope_overruns(entry: Reservation) -> u64 {
    0
}
pub fn reservation_usage(entry: Reservation) -> (u64, u64) {
    (0, 0)
}
pub fn drawn_from(frame: Frame) -> Reservation {
    0
}
pub fn heap_drawn_from(address: u64) -> Reservation {
    0
}

pub const fn watermark_for(managed: u64) -> u64 {
    0
}
pub fn watermark() -> u64 {
    KEPT_FROM_PROGRAMS
}
pub fn program_floor() -> u64 {
    KEPT_FROM_PROGRAMS
}
pub fn outstanding_promise() -> u64 {
    0
}
pub fn held_frames() -> u64 {
    managed_frames().saturating_sub(free_frames())
}
/// The arena frame `address` is in, if it is in the arena's own frames.
pub fn frame_of(address: usize) -> Option<Frame> {
    let at = ARENA_AT.load(Ordering::Acquire) as usize;
    let frame = (address.checked_sub(at)? as u64) / PAGE_SIZE;
    (frame < ARENA_FRAMES).then_some(frame)
}
pub fn allocate_frames(order: u8) -> Option<Frame> {
    with_frames(|frames| frames.allocate(order)).flatten()
}
pub fn allocate_user_frame() -> Option<Frame> {
    if free_frames() < KEPT_FROM_PROGRAMS {
        return None;
    }
    let owner = crate::sched::running_group();
    crate::object::quota::charge_frame(owner).ok()?;
    let frame = allocate_frames(0).and_then(|frame| {
        with_frames(|frames| frames.set_owner(frame, owner))
            .and_then(Result::ok)
            .map(|()| frame)
    });
    if frame.is_none() {
        crate::object::quota::uncharge_frame(owner);
    }
    frame
}
pub fn allocate_user_run(order: u8) -> Option<Frame> {
    let frame = allocate_frames(order)?;
    let _ = with_frames(|frames| frames.split(frame, order));
    Some(frame)
}
pub fn allocate_frame_run(blocks: u64) -> Option<Frame> {
    with_frames(|frames| frames.allocate_run(blocks)).flatten()
}
pub fn split_frames(frame: Frame, order: u8) -> bool {
    with_frames(|frames| frames.split(frame, order).is_ok()).unwrap_or(false)
}
pub fn deallocate_frames(frame: Frame, order: u8) {
    if is_foreign(frame) {
        return;
    }
    let _ = with_frames(|frames| frames.deallocate(frame, order));
}
/// A foreign page: one of a VMO a driver made, mapped above the arena so that
/// `direct_map` reaches it (`crate::bridge`). It is its VMO's; no count here.
pub const fn is_foreign(frame: Frame) -> bool {
    frame >= ARENA_FRAMES
}
/// How much of linuxd's address space foreign windows may ever take.
const FOREIGN_FRAMES: u64 = (16_u64 << 30) / PAGE_SIZE;
static FOREIGN_NEXT: AtomicU64 = AtomicU64::new(ARENA_FRAMES);
/// Room for `pages` foreign pages: answers the first frame, and where it maps.
///
/// `None` past [`FOREIGN_FRAMES`]: windows are not given back yet (M5-LIFT.md §11),
/// so a card's buffer churn ends here, and an answer of no memory.
pub fn foreign_window(pages: u64) -> Option<(Frame, u64)> {
    let frame = FOREIGN_NEXT
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            let end = next.checked_add(pages)?;
            (end <= ARENA_FRAMES + FOREIGN_FRAMES).then_some(end)
        })
        .ok()?;
    Some((frame, direct_map(frame * PAGE_SIZE)))
}
/// A foreign window: first frame, pages, the kernel VMO's handle.
static FOREIGN: PlainLock<alloc::vec::Vec<(Frame, u64, u32)>> =
    PlainLock::new(alloc::vec::Vec::new());
/// Say whose pages a window's are, for a program's mapping of them.
pub fn foreign_bind(first: Frame, pages: u64, handle: u32) {
    FOREIGN.lock().push((first, pages, handle));
}
/// The kernel VMO a foreign frame is a page of, and its page there.
pub fn foreign_page(frame: Frame) -> Option<(u32, u64)> {
    FOREIGN
        .lock()
        .iter()
        .find(|(first, pages, _)| (*first..*first + *pages).contains(&frame))
        .map(|(first, _, handle)| (*handle, frame - *first))
}
pub fn share_frame(frame: Frame) -> Option<u32> {
    if is_foreign(frame) {
        return Some(2);
    }
    with_frames(|frames| frames.share(frame).ok())?
}
/// The kernel's batched `share_frame` (R5), one at a time here: the lift has
/// no interrupts to mask.
pub(crate) fn share_frames<T>(batch: &[T], frame_of: impl Fn(&T) -> Frame) -> usize {
    batch
        .iter()
        .take(SHARE_BATCH)
        .take_while(|&item| share_frame(frame_of(item)).is_some())
        .count()
}
/// As the kernel's.
pub(crate) const SHARE_BATCH: usize = 128;
pub fn release_frame(frame: Frame) -> bool {
    if is_foreign(frame) {
        return false;
    }
    let freed = with_frames(|frames| {
        let owner = frames.owner(frame);
        matches!(frames.release(frame), Ok(ferrix_frame::Released::Freed)).then_some(owner)
    })
    .flatten();
    if let Some(owner) = freed {
        crate::object::quota::uncharge_frame(owner);
        true
    } else {
        false
    }
}
pub fn disown_frame(frame: Frame) {
    let _ = with_frames(|frames| {
        let owner = frames.owner(frame);
        let _ = frames.set_owner(frame, crate::object::quota::NONE);
        crate::object::quota::uncharge_frame(owner);
    });
}
pub fn frame_references(frame: Frame) -> u32 {
    // One: a page of a driver's VMO is never copied away from the driver.
    if is_foreign(frame) {
        return 1;
    }
    with_frames(|frames| frames.entry(frame).map_or(0, PageEntry::refcount)).unwrap_or(0)
}
pub fn frame_owner(frame: Frame) -> u32 {
    with_frames(|frames| frames.owner(frame)).unwrap_or(crate::object::quota::NONE)
}
pub fn frame_state(frame: Frame) -> Option<ferrix_frame::State> {
    with_frames(|frames| frames.state(frame)).flatten()
}
pub fn frame_retired(frame: Frame) -> bool {
    false
}
pub fn retire_frame(frame: Frame) -> Retired {
    Retired::NotCovered
}
pub const RETIRED_CAPACITY: usize = 256;
pub fn is_retired(frame: Frame) -> bool {
    false
}
pub fn retired_frames(out: &mut [Frame]) -> usize {
    0
}
pub fn free_frames() -> u64 {
    with_frames(|frames| frames.free_frames()).unwrap_or(0)
}
pub fn managed_frames() -> u64 {
    MANAGED.load(Ordering::Relaxed)
}
pub fn heap_pages() -> usize {
    0
}
pub fn tables_kept() -> u64 {
    0
}
pub fn image_span() -> (u64, u64) {
    (0, 0)
}
pub fn overlaps_image(phys: u64, len: u64) -> bool {
    false
}
pub fn direct_map(phys: u64) -> u64 {
    ARENA_AT.load(Ordering::Relaxed) + phys
}
pub fn direct_map_ram(phys: u64) -> Option<u64> {
    (phys < ARENA_FRAMES * PAGE_SIZE).then(|| direct_map(phys))
}
pub fn zero_frame(frame: Frame) {
    // SAFETY: an arena page the caller holds.
    unsafe {
        core::ptr::write_bytes(
            direct_map(frame * PAGE_SIZE) as *mut u8,
            0,
            PAGE_SIZE as usize,
        )
    }
}
pub fn copy_frame(destination: Frame, source: Frame) {
    // SAFETY: two arena pages the caller holds, different.
    unsafe {
        core::ptr::copy_nonoverlapping(
            direct_map(source * PAGE_SIZE) as *const u8,
            direct_map(destination * PAGE_SIZE) as *mut u8,
            PAGE_SIZE as usize,
        );
    }
}

/// An arena page's address in the arena's VMO: the `SPACE_MAP` offset.
pub fn arena_offset(phys: u64) -> u64 {
    phys
}

/// Callers that race a change hold the shadow (`fill::with_shadow`); the rest are
/// the real `user/space.rs` under its own lock.
pub fn translate_in(root: u64, virt: u64) -> Option<u64> {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    mapper
        .translate(&KernelPhysMem, VirtAddr(virt))
        .map(|at| at.0)
}
pub fn for_each_leaf_in(root: u64, visit: impl FnMut(ferrix_paging::Leaf) -> bool) {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    let _ = mapper.for_each_leaf(&KernelPhysMem, visit);
}
pub fn flags_in(root: u64, virt: u64) -> Option<MapFlags> {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    mapper.leaf_flags(&KernelPhysMem, VirtAddr(virt))
}
/// In the shadow alone: the kernel's space learns of it at the first fault.
pub fn map_in(
    root: u64,
    virt: u64,
    phys: u64,
    len: u64,
    flags: MapFlags,
) -> Result<(), ferrix_paging::MapError> {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    crate::fill::with_shadow(root, || {
        mapper.map_range(
            &mut KernelPhysMem,
            VirtAddr(virt),
            PhysAddr(phys),
            len.next_multiple_of(PAGE_SIZE),
            flags,
        )
    })
}
/// From the shadow, and from the kernel's space, whose own shootdown covers it.
pub fn unmap_in(
    root: u64,
    virt: u64,
    len: u64,
    flush: &mut crate::smp::TlbPages,
) -> Result<(), ferrix_paging::MapError> {
    let len = len.next_multiple_of(PAGE_SIZE);
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    let tables = flush.tables();
    let unmapped = crate::fill::with_shadow(root, || {
        let unmapped = mapper.unmap_range(&mut KernelPhysMem, VirtAddr(virt), len, |freed| {
            if let Released::Table { phys } = freed {
                tables.push(phys);
            }
        });
        // The kernel's space is a cache of this shadow, filled under this same hold,
        // and every change of the shadow goes through here with its kernel half: a
        // range the shadow had nothing in, the kernel has nothing in. So memory never
        // touched costs no kernel call, which was half of a bare munmap.
        if !matches!(unmapped, Ok(0)) {
            crate::fill::unmap(root, virt, len);
        }
        unmapped
    });
    flush.add_range(virt, len);
    let _ = unmapped?;
    Ok(())
}
pub fn unmap_unwalked(root: u64, virt: u64, len: u64) -> Result<(), ferrix_paging::MapError> {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    let len = len.next_multiple_of(PAGE_SIZE);
    let _ = crate::fill::with_shadow(root, || {
        let unmapped = mapper.unmap_range(&mut KernelPhysMem, VirtAddr(virt), len, |freed| {
            if let Released::Table { phys } = freed {
                deallocate_frames(phys.0 / PAGE_SIZE, 0);
            }
        });
        // As in `unmap_in`.
        if !matches!(unmapped, Ok(0)) {
            crate::fill::unmap(root, virt, len);
        }
        unmapped
    })?;
    Ok(())
}
pub fn prune_in(root: u64, virt: u64, len: u64) {
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    crate::fill::with_shadow(root, || {
        let _ = mapper.prune_range(&mut KernelPhysMem, VirtAddr(virt), len, |freed| {
            if let Released::Table { phys } = freed {
                deallocate_frames(phys.0 / PAGE_SIZE, 0);
            }
        });
    });
}

/// Tables unlinked until the shootdown has run: here, at once.
#[derive(Debug, Default)]
pub struct UnlinkedTables;
impl UnlinkedTables {
    pub const fn new() -> UnlinkedTables {
        UnlinkedTables
    }
    pub fn push(&mut self, phys: PhysAddr) {
        deallocate_frames(phys.0 / PAGE_SIZE, 0);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FrameWindow;
impl FrameWindow {
    pub fn open() -> FrameWindow {
        FrameWindow
    }
    pub fn report(&self, check: &str) {}
    pub fn kept(&self) -> i64 {
        0
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GivenBack {
    pub frames: u64,
    pub managed: u64,
    pub watermark: u64,
    pub longest_hold_nanos: u64,
    pub holds: u64,
    pub largest_batch: u64,
}
pub const GIVE_BACK_BATCH: u64 = 1024;
static INITRD: AtomicU64 = AtomicU64::new(0);
static INITRD_LEN: AtomicU64 = AtomicU64::new(0);
/// Where the initramfs's VMO is mapped (M5-LIFT.md §6).
pub fn set_initrd(at: u64, len: u64) {
    INITRD.store(at, Ordering::Release);
    INITRD_LEN.store(len, Ordering::Release);
}
pub fn with_initrd<T>(read: impl FnOnce(&[u8]) -> T) -> Option<T> {
    let at = INITRD.load(Ordering::Acquire);
    let len = INITRD_LEN.load(Ordering::Acquire);
    if at == 0 {
        return None;
    }
    // SAFETY: a read-only mapping that stays for as long as linuxd.
    Some(read(unsafe {
        core::slice::from_raw_parts(at as *const u8, len as usize)
    }))
}
pub fn give_back_initrd() -> Option<GivenBack> {
    None
}

pub fn ram_behind(_virt: u64) -> Option<u64> {
    None
}

/// Where the initramfs is: linuxd has it mapped already (M5-LIFT.md §6).
pub fn keep_initrd(view: &ferrix_bootinfo::BootView<'_>) -> Result<Option<u64>, ()> {
    Ok(Some(INITRD_LEN.load(Ordering::Acquire)).filter(|len| *len != 0))
}

pub fn ensure_top_in(root: u64, virt: u64) -> Result<(), ferrix_paging::MapError> {
    Ok(())
}
/// A normal root over a region: a page nobody walks, the shadow space has one
/// tree (M5-LIFT.md §3a) and [`copy_root_slot`] copies nothing.
pub fn allocate_root_for(owner: u32) -> Option<Frame> {
    let frame = allocate_frames(0)?;
    zero_frame(frame);
    Some(frame)
}
pub fn free_root(frame: Frame) {
    deallocate_frames(frame, 0);
}
pub fn copy_root_slot(from: u64, to: u64, slot: usize) -> bool {
    false
}
pub fn root_slot_entry(root: u64, slot: usize) -> u64 {
    0
}
/// The kernel's write-protect of a fork's parent, which the lift does not
/// carry: refused, so that `protect_copied_on_write` takes the region down
/// through `unmap_in` as before, its kernel half included (R5).
pub(crate) fn write_protect_in(
    _root: u64,
    _virt: u64,
    _len: u64,
) -> Result<u64, ferrix_paging::MapError> {
    Err(ferrix_paging::MapError::Misaligned)
}
/// The kernel's in-place reuse of a sole holder's page, which the lift does
/// not carry: refused, so the copy-on-write write takes the old path (R5).
pub(crate) fn allow_write_in(_root: u64, _virt: u64) -> bool {
    false
}
/// `unmap_in`, for a space that keeps its top tables: the shadow has no reader of them.
pub fn unmap_in_keeping(
    root: u64,
    virt: u64,
    len: u64,
    flush: &mut crate::smp::TlbPages,
    keep_top: bool,
) -> Result<(), ferrix_paging::MapError> {
    unmap_in(root, virt, len, flush)
}

/// The kernel's, for a window; a window exists only there.
pub(crate) fn unmap_placed(_root: u64, _virt: u64, _len: u64) -> crate::smp::TlbPages {
    crate::smp::TlbPages::new()
}
