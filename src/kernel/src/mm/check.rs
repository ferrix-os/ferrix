//! Stage 2's self-checks: the frame allocator, the heap, the vmap arena and
//! its device windows and kernel stacks, and the two sweeps of the kernel's
//! own page tables that end bring-up -- W^X, and the sealed image.
//!
//! These were functions of `main.rs` and of `mm.rs` until W-8's low-level
//! requirements (docs/certification/IMPLEMENTATION.md) needed a `Verifies:`
//! tag on them, which may go only on a function in a check file. They run
//! from the same two points of bring-up as before: [`memory_check`] from
//! `main.rs`'s `check_allocators_and_traps`, and [`sweep_w_xor_x`] from its
//! `finish_memory`, once the loader's identity map has gone.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_paging::{Leaf, MapFlags, Mapper, PhysAddr, WalkOutcome};

use super::{KernelPhysMem, PHYSMAP, PHYSMAP_PHYS, ROOT_TABLE, TABLES, direct_map};
use crate::console::{println, println_unlogged};
use crate::{fallible, mm, vmap};
use ferrix_paging::PhysMem;

// ---------------------------------------------------------------------------
// The sweeps at the end of bring-up
// ---------------------------------------------------------------------------

/// The W^X sweep after the identity map has gone: no mapping the kernel
/// holds is both writable and executable, and there are executable ones to
/// have swept.
///
/// Verifies: H.MEM.4, L.mm.32
pub fn sweep_w_xor_x(view: &BootView<'_>) -> Result<(), &'static str> {
    let wx = match check_w_xor_x(view) {
        Ok(report) => report,
        Err(found) => {
            // Printed rather than counted: one offending mapping is enough,
            // and an address is what makes it findable. A count would say
            // there is a problem without saying where.
            println_unlogged!(
                "  w^x      {:#x} is writable and executable, {} bytes of it",
                found.virt,
                found.len
            );
            return Err("a mapping is both writable and executable");
        }
    };
    if wx.executable == 0 {
        return Err("the sweep found no executable mapping at all, so it swept nothing");
    }
    println!(
        "  w^x      {} mappings swept, {} executable, none writable",
        wx.leaves, wx.executable
    );

    // And the frames the image's text sits in, through every mapping of them:
    // the direct map aliases them, never executably, so the sweep above
    // cannot see an alias that writes the code it just passed.
    let sealed = match check_sealed_image(view) {
        Ok(report) => report,
        Err(found) if found.len == 0 => {
            println_unlogged!(
                "  sealed   the direct map does not alias all of the image's text at {:#x}",
                found.virt
            );
            return Err("the direct map does not alias the kernel's text, so nothing was swept");
        }
        Err(found) => {
            println_unlogged!(
                "  sealed   {:#x} writes the kernel's text or read-only data, {} bytes of it",
                found.virt,
                found.len
            );
            return Err("a mapping can write the kernel's text or read-only data");
        }
    };
    println!(
        "  sealed   {} KiB of text and read-only data, {} mappings of it, none writable",
        sealed.bytes / 1024,
        sealed.mappings
    );
    Ok(())
}

/// A mapping that is both writable and executable.
#[derive(Clone, Copy, Debug)]
pub struct WriteExecute {
    /// Where it starts.
    pub(crate) virt: u64,
    /// How much of the address space it covers.
    pub(crate) len: u64,
}

/// What the W^X sweep found.
#[derive(Clone, Copy, Debug)]
pub struct WxReport {
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
pub fn check_w_xor_x(view: &BootView<'_>) -> Result<WxReport, WriteExecute> {
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

// Where the link script puts the image's first byte and the first byte of its
// data: everything between the two is text or read-only data. Declared rather
// than defined, because only their addresses mean anything.
unsafe extern "C" {
    static __kernel_start: u8;
    static __data_start: u8;
}

/// The physical span of the image's text and read-only data, as its first
/// byte and a length.
fn sealed_span(view: &BootView<'_>) -> (u64, u64) {
    let start = u64::try_from((&raw const __kernel_start).addr()).unwrap_or(u64::MAX);
    let data = u64::try_from((&raw const __data_start).addr()).unwrap_or(0);
    (view.raw().kernel_phys, data.saturating_sub(start))
}

/// What the sealed-image sweep found.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SealReport {
    /// Bytes of text and read-only data the image holds.
    pub(crate) bytes: u64,
    /// Mappings of any of those bytes, the image's own included.
    pub(crate) mappings: u64,
}

/// Walk the kernel's tables and require that no mapping of the physical pages
/// holding the kernel's text and read-only data is writable.
///
/// The W^X sweep asks each mapping about itself, and the image mapping passes
/// it. The direct map is a second mapping of the same frames, never executable
/// and so never a W^X violation, and until the loaders sealed it it was
/// writable: a write through the alias changed the code the image mapping
/// runs. So this asks about the *frames*, whatever maps them — the direct
/// map, the image, or a device window somebody opened over the image.
///
/// And it requires the direct map to alias every byte of the span, which is
/// what makes a clean result mean something: a sweep that never met the alias
/// it is looking for would pass on a kernel whose direct map moved.
///
/// # Errors
///
/// The first writable mapping of a sealed frame, or, as a zero-length
/// [`WriteExecute`] at the span's direct-map address, a direct map that does
/// not cover the whole span.
///
/// Verifies: H.MEM.5, L.mm.33
pub(crate) fn check_sealed_image(view: &BootView<'_>) -> Result<SealReport, WriteExecute> {
    let (low, bytes) = sealed_span(view);
    let high = low.saturating_add(bytes);
    let physmap = PHYSMAP.load(Ordering::Relaxed);
    let physmap_phys = PHYSMAP_PHYS.load(Ordering::Relaxed);

    let mut report = SealReport { bytes, mappings: 0 };
    let mut aliased = 0u64;
    let mut offender = None;
    let _ = sweep(ROOT_TABLE.load(Ordering::Relaxed), |leaf| {
        let first = leaf.phys.0;
        let end = first.saturating_add(leaf.bytes());
        if end <= low || first >= high {
            return true;
        }
        report.mappings += 1;
        if leaf.flags.write {
            offender = Some(WriteExecute {
                virt: leaf.virt.0,
                len: leaf.bytes(),
            });
            return false;
        }
        let direct = first
            .checked_sub(physmap_phys)
            .and_then(|offset| physmap.checked_add(offset));
        if direct == Some(leaf.virt.0) {
            aliased += end.min(high) - first.max(low);
        }
        true
    });

    if let Some(found) = offender {
        return Err(found);
    }
    if bytes == 0 || aliased != bytes {
        return Err(WriteExecute {
            virt: direct_map(low),
            len: 0,
        });
    }
    Ok(report)
}

/// Visit every leaf in the tree rooted at `root`, in address order, holding
/// [`TABLES`](super::TABLES) throughout.
///
/// Takes a root rather than going through [`with_tables`](super::with_tables) because the W^X
/// sweep also walks the loader's identity map, which on `AArch64` is a second
/// tree with a root of its own.
fn sweep(root: u64, visit: impl FnMut(Leaf) -> bool) -> WalkOutcome {
    let _held = TABLES.lock();
    let mapper: Mapper<crate::arch::PageEncoding> = Mapper::new(PhysAddr(root));
    mapper.for_each_leaf(&KernelPhysMem, visit)
}

/// What `virt` is mapped as, or `None` if it is not mapped.
fn permissions_of(virt: u64) -> Option<MapFlags> {
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
// Stage 2's allocators
// ---------------------------------------------------------------------------

/// Stage 2's exit criterion.
///
/// Every one of these is an invariant a later subsystem will assume without
/// checking, because by then there will be no way to check it: a scheduler that
/// gets a `Vec` back with the wrong contents has no idea the heap is at fault.
///
/// Verifies: H.MEM.14, L.mm.13
pub fn memory_check(stats: &mm::Stats) -> Result<(), &'static str> {
    if stats.managed_frames == 0 {
        return Err("the frame allocator was given nothing");
    }

    check_frames()?;
    check_retired()?;

    // The heap must come back to where it started, and "where it started" is
    // not zero: the vmap arena below is a live `Vec` and a live `BTreeMap`,
    // and requiring zero afterwards would be requiring the arena not to exist.
    // So the balance is checked around the allocations that are supposed to be
    // transient, before anything permanent is built on top of them.
    // The processors' caches hold freed objects, and with them their slab
    // pages: drained before each count, so the pages are the heap's own.
    let _ = mm::drain_caches();
    let heap_before = mm::heap_allocated();
    let pages_before = mm::heap_pages();
    check_heap()?;
    if mm::heap_allocated() != heap_before {
        return Err("the heap did not give everything back");
    }
    let _ = mm::drain_caches();

    // And gave the *pages* back too, not only the objects. `check_heap` grows
    // a `Vec` to 32 KiB and a `BTreeMap` to two thousand nodes, which is tens
    // of slab pages across several classes; a heap that kept them would pass
    // every other check here and grow monotonically for the life of the
    // system. What it is allowed to keep is one page per size class, which is
    // the rule `ferrix_heap` states.
    let kept = mm::heap_pages().saturating_sub(pages_before);
    if kept > ferrix_heap::CLASSES {
        return Err("the heap kept more slab pages than one per size class");
    }

    // The device-window checks' aperture: a frame of RAM this check owns,
    // which no device is and nothing else maps writable. Not the image, which
    // `vmap::map_device` refuses.
    let frame = mm::allocate_frames(0).ok_or("no frame for the device-window checks")?;
    let checked = check_vmap(frame * PAGE_SIZE);
    mm::deallocate_frames(frame, 0);
    checked?;
    check_no_device_window_over_the_image()?;
    check_no_device_window_wraps()?;
    check_stacks()?;
    check_only_ram_has_a_checked_alias()?;
    check_the_watermark(stats)?;
    Ok(())
}

/// Frames taken for a check and held through themselves: each frame's first
/// word is the next one's number, so holding all of memory takes no heap,
/// however much memory there is.
struct Chain {
    /// The last frame taken, or [`Chain::END`].
    head: u64,
    /// How many frames are held.
    len: u64,
}

impl Chain {
    /// No frame: what the last frame's first word holds.
    const END: u64 = u64::MAX;

    const fn new() -> Chain {
        Chain {
            head: Chain::END,
            len: 0,
        }
    }

    /// Hold `frame`, which the caller has just taken and nothing else uses.
    fn push(&mut self, frame: u64) {
        KernelPhysMem.write(PhysAddr(frame * PAGE_SIZE), self.head);
        self.head = frame;
        self.len += 1;
    }

    /// Let go of the frame held last.
    fn pop(&mut self) -> Option<u64> {
        if self.head == Chain::END {
            return None;
        }
        let frame = self.head;
        self.head = KernelPhysMem.read(PhysAddr(frame * PAGE_SIZE));
        self.len -= 1;
        Some(frame)
    }
}

/// The kernel's watermark (`mm::WATERMARK`): a program's frames, its runs
/// of frames and its page tables are refused exactly where taking them would
/// leave fewer free than the watermark, with nothing taken, while the
/// kernel's own frames and the heap's pages are still given below it.
///
/// At stage 2, on one processor with no program running, so nothing else
/// moves the free count: most of memory is taken as the largest blocks by
/// the kernel's route, then program frames until refused, and the free
/// count must then be the watermark itself. At that count a program's
/// page table is refused; with one frame back it is given, and the count is
/// the watermark again. A run of two frames is given at two above and
/// refused at one above, with a block of two free to show the refusal is
/// the watermark's and not the allocator's, and a single frame is given
/// there. Then, at the watermark and below it, a kernel frame is given and
/// the heap takes a fresh page. Every
/// frame goes back and the window must close on the count it opened at.
///
/// Nothing is charged here: the running group at stage 2 is no job's, so
/// every route takes its `quota::NONE` branch; a refusal returns before the
/// frame is tagged and through the same uncharge branch as an empty
/// allocator, which is what `L.mm.72` holds.
///
/// Its negative controls are each route made to take its frames with
/// `mm::allocate_frames` (the frame, the run and the table), and the
/// watermark applied to `mm::allocate_frames` itself (the kernel's step).
///
/// Verifies: H.QUOTA.13
/// Verifies: L.mm.71
/// Verifies: L.mm.72
/// Verifies: L.mm.73
/// Verifies: L.mm.74
fn check_the_watermark(stats: &mm::Stats) -> Result<(), &'static str> {
    let floor = mm::watermark();
    if floor != mm::watermark_for(stats.managed_frames)
        || mm::watermark_for(0) != 256
        || mm::watermark_for(128 * 300) != 300
        || mm::watermark_for(u64::MAX) != 8192
    {
        return Err("the watermark is not managed frames / 128 within 256 and 8192");
    }
    let window = mm::FrameWindow::open();
    // Most of memory taken as the allocator's largest blocks, by the
    // kernel's own route, which the watermark does not hold: one link
    // written per block rather than per frame, which under emulation is
    // most of the check's time. What is left, a block's worth above the
    // watermark and whatever smaller blocks lie free, the program routes
    // take below.
    let largest = 1u64 << ferrix_frame::MAX_ORDER;
    let mut blocks = Chain::new();
    while mm::free_frames() > floor + 2 * largest {
        match mm::allocate_frames(ferrix_frame::MAX_ORDER) {
            Some(block) => blocks.push(block),
            None => break,
        }
    }
    let mut frames = Chain::new();
    let outcome = watermark_routes(floor, &mut frames);
    let taken = frames.len + blocks.len * largest;
    while let Some(frame) = frames.pop() {
        let _ = mm::release_frame(frame);
    }
    while let Some(block) = blocks.pop() {
        mm::deallocate_frames(block, ferrix_frame::MAX_ORDER);
    }
    outcome?;
    let kept = window.kept();
    if kept != 0 {
        window.report("watermark");
        return Err("the watermark check did not give back every frame it took");
    }
    println!(
        "  watermark {} KiB kept from programs of {} KiB: frames, runs and tables refused there, \
         the kernel served below it ({} frames taken and given back)",
        floor * 4,
        stats.managed_frames * 4,
        taken,
    );
    Ok(())
}

/// [`check_the_watermark`]'s steps, every frame a program route gave held in
/// `frames` for the caller to give back.
fn watermark_routes(floor: u64, frames: &mut Chain) -> Result<(), &'static str> {
    while let Some(frame) = mm::allocate_user_frame() {
        frames.push(frame);
    }
    if mm::free_frames() != floor {
        return Err("a program's frame was refused away from the watermark");
    }
    // The table route, at the watermark and one above it.
    let mut tables = super::ChargedTables {
        owner: crate::object::quota::NONE,
        program: true,
    };
    if let Some(table) = tables.allocate_table() {
        mm::deallocate_frames(table.0 / PAGE_SIZE, 0);
        return Err("a program's page table was taken at the watermark");
    }
    let one = frames.pop().ok_or("no program frame was taken")?;
    let _ = mm::release_frame(one);
    let table = tables
        .allocate_table()
        .ok_or("a program's page table was refused above the watermark")?;
    let at_floor = mm::free_frames() == floor;
    mm::deallocate_frames(table.0 / PAGE_SIZE, 0);
    if !at_floor {
        return Err("a program's page table was not one frame of the free count");
    }
    // The run route: given at two above, refused at one above while a block
    // of two is free.
    let spare = frames.pop().ok_or("no second program frame was taken")?;
    let _ = mm::release_frame(spare);
    if mm::free_frames() != floor + 2 {
        return Err("the free count is not two above the watermark");
    }
    let run = mm::allocate_user_run(1).ok_or("a program's run was refused above the watermark")?;
    frames.push(run);
    frames.push(run + 1);
    let back = frames.pop().ok_or("the run was not held")?;
    let _ = mm::release_frame(back);
    let block_free =
        super::with_frames(|all| all.free_blocks().iter().skip(1).any(|&n| n > 0)).unwrap_or(false);
    if !block_free {
        return Err("no block of two was free to show a run's refusal");
    }
    if let Some(run) = mm::allocate_user_run(1) {
        frames.push(run);
        frames.push(run + 1);
        return Err("a program's run was taken across the watermark");
    }
    // A program's frame is still given at one above, which leaves the
    // watermark; then the kernel's own: a frame and a heap page, at the
    // watermark and below it.
    let last =
        mm::allocate_user_frame().ok_or("a program's frame was refused above the watermark")?;
    frames.push(last);
    if mm::free_frames() != floor {
        return Err("the free count is not the watermark before the kernel's step");
    }
    let kernel = mm::allocate_frames(0).ok_or("a kernel frame was refused at the watermark")?;
    let pages = mm::heap_pages();
    let block: Result<Vec<u8>, _> =
        fallible::try_with_capacity(usize::try_from(PAGE_SIZE * 4).unwrap_or(usize::MAX));
    let grew = mm::heap_pages() > pages;
    drop(block);
    mm::deallocate_frames(kernel, 0);
    if !grew {
        return Err("the heap took no fresh page below the watermark");
    }
    Ok(())
}

/// The vmap arena hands out address space, maps it, and takes it back.
///
/// The frame count is what makes this a measurement: an arena that mapped the
/// pages and never unmapped them would pass every read-back check here and
/// leak a frame per page. Requiring the free count to return to exactly where
/// it started is the only assertion that notices.
///
/// Verifies: H.MEM.15, L.mm.42, L.mm.46
fn check_vmap(aperture: u64) -> Result<(), &'static str> {
    const PAGES: u64 = 8;

    let free_before = warm_vmap(PAGES)?;

    let first = vmap::allocate(PAGES, MapFlags::KERNEL_DATA).map_err(|_| "vmap refused a range")?;
    let second =
        vmap::allocate(PAGES, MapFlags::KERNEL_DATA).map_err(|_| "vmap refused a second range")?;

    if first.base == second.base {
        return Err("vmap handed out the same address twice");
    }
    if first.base < vmap::ARENA_BASE || second.end() > vmap::ARENA_END {
        return Err("vmap handed out an address outside its own arena");
    }

    // Two allocations may not touch: there is a guard page on each side of
    // each, so even adjacent ones are two pages apart.
    let (low, high) = if first.base < second.base {
        (first, second)
    } else {
        (second, first)
    };
    if high.base < low.end() + 2 * PAGE_SIZE {
        return Err("two vmap allocations are not separated by their guard pages");
    }

    check_vmap_contents(first)?;
    check_vmap_guards(first)?;
    check_vmap_protection(first)?;
    check_device_windows(aperture)?;
    vmap::check_failed_device_map(aperture)?;
    vmap::check_invariants()?;

    free_pair(first, second)?;

    if vmap::free(first.base).is_ok() {
        return Err("vmap freed the same allocation twice");
    }
    if mm::translate(first.base).is_some() {
        return Err("freeing a vmap allocation left the mapping behind");
    }
    if mm::free_frames() != free_before {
        return Err("a vmap allocation leaked frames: the free count moved");
    }
    Ok(())
}

/// Allocate and free a pair of `pages`-page ranges, and answer the free
/// frames after, for [`check_vmap`] to count from: the arena's own records
/// grow on first use and keep what they grew to, and since the allocation
/// reserve holds the spare objects of every size class, that growth can take
/// a slab page the pair measured afterwards then reuses. Growth is not a
/// leak.
fn warm_vmap(pages: u64) -> Result<u64, &'static str> {
    let warm = vmap::allocate(pages, MapFlags::KERNEL_DATA).map_err(|_| "vmap refused a range")?;
    let warmer =
        vmap::allocate(pages, MapFlags::KERNEL_DATA).map_err(|_| "vmap refused a second range")?;
    free_pair(warm, warmer)?;
    Ok(mm::free_frames())
}

/// Free two vmap allocations.
fn free_pair(first: vmap::Mapping, second: vmap::Mapping) -> Result<(), &'static str> {
    vmap::free(first.base).map_err(|_| "vmap refused to free its own allocation")?;
    vmap::free(second.base).map_err(|_| "vmap refused to free its own allocation")
}

/// Every page of an allocation is mapped, distinct and zeroed.
///
/// Verifies: L.mm.44
fn check_vmap_contents(mapping: vmap::Mapping) -> Result<(), &'static str> {
    let pages = mapping.len / PAGE_SIZE;
    let mut previous = None;

    for page in 0..pages {
        let at = mapping.base + page * PAGE_SIZE;
        let phys = mm::translate(at).ok_or("a vmap page is not mapped")?;
        if Some(phys) == previous {
            return Err("two vmap pages resolve to the same frame");
        }
        previous = Some(phys);

        // SAFETY: (KMEM) `at` is inside an allocation this function was handed, so it
        // is mapped writable and nothing else refers to it.
        let existing = unsafe { core::ptr::read_volatile(at as *const u64) };
        if existing != 0 {
            return Err("a vmap page was not zeroed before it was handed out");
        }
        // SAFETY: (KMEM) as above.
        unsafe { core::ptr::write_volatile(at as *mut u64, 0xA11C_0000 + page) };
    }

    for page in 0..pages {
        let at = mapping.base + page * PAGE_SIZE;
        // SAFETY: (KMEM) as above; written a moment ago.
        if unsafe { core::ptr::read_volatile(at as *const u64) } != 0xA11C_0000 + page {
            return Err("a vmap page did not hold what was written to it");
        }
    }
    Ok(())
}

/// Permissions can be changed on a live mapping, and the change is in the
/// tables rather than only in the caller's head.
///
/// Read back by walking the page tables, not by remembering what was asked
/// for: what matters is what the hardware will do, and the whole reason the
/// W^X sweep reads descriptors is that those are two different things.
///
/// Verifies: L.mm.31
fn check_vmap_protection(mapping: vmap::Mapping) -> Result<(), &'static str> {
    let before = permissions_of(mapping.base).ok_or("a vmap page has no permissions at all")?;
    if !before.write {
        return Err("a fresh vmap allocation is not writable");
    }

    mm::protect_kernel(mapping.base, PAGE_SIZE, MapFlags::KERNEL_RODATA)
        .map_err(|_| "protecting a vmap page was refused")?;
    let after = permissions_of(mapping.base).ok_or("protecting a page unmapped it")?;
    if after.write {
        return Err("protecting a page read-only left it writable");
    }
    if mm::translate(mapping.base).is_none() {
        return Err("protecting a page moved what it translates to");
    }

    // And back, so the caller's own read-back check below still holds.
    mm::protect_kernel(mapping.base, PAGE_SIZE, MapFlags::KERNEL_DATA)
        .map_err(|_| "restoring a vmap page's permissions was refused")?;
    Ok(())
}

/// A kernel stack is guard-paged at both ends and usable in between.
///
/// Not run *on* — switching stacks is stage 5's context switch, and doing it
/// here would need the assembly that stage owns. What is checked is everything
/// that has to be true before a stack can be switched to: it is mapped, it is
/// writable to its last byte, the page below it is not, and freeing it gives
/// the frames back.
///
/// Verifies: H.FAIL.2, L.mm.55
fn check_stacks() -> Result<(), &'static str> {
    let free_before = mm::free_frames();
    let stack = vmap::allocate_stack().map_err(|_| "no kernel stack could be allocated")?;

    if stack.len() != vmap::STACK_PAGES * PAGE_SIZE {
        return Err("a kernel stack is not the size it was asked for");
    }
    if !stack.top.is_multiple_of(16) {
        // Both architectures require a 16-byte aligned stack pointer at a
        // function call boundary, and neither faults on it — the symptom is a
        // misaligned spill somewhere deep in the callee.
        return Err("a kernel stack top is not sixteen-byte aligned");
    }

    // The last usable word, which is where the first push lands, and the first,
    // which is the byte an overflow reaches last before the guard.
    for at in [stack.top - 8, stack.base] {
        // SAFETY: (KMEM) inside the stack's own mapping, which is writable and which
        // nothing else refers to — no CPU is running on this stack.
        unsafe { core::ptr::write_volatile(at as *mut u64, 0x57AC_0000_0000_0000) };
        // SAFETY: (KMEM) as above.
        if unsafe { core::ptr::read_volatile(at as *const u64) } != 0x57AC_0000_0000_0000 {
            return Err("a kernel stack did not hold what was written to it");
        }
    }

    if mm::translate(stack.base - PAGE_SIZE).is_some() {
        return Err("a kernel stack has no guard page below it, so an overflow would be silent");
    }
    if mm::translate(stack.top).is_some() {
        return Err("a kernel stack has no guard page above it");
    }

    // SAFETY: (KMEM) nothing is running on it; it was allocated a few lines above and
    // never installed anywhere.
    unsafe { vmap::free_stack(stack) }.map_err(|_| "a kernel stack could not be freed")?;
    if mm::free_frames() != free_before {
        return Err("a kernel stack leaked frames");
    }
    Ok(())
}

/// A device window lands where it was asked to, offset and all, and can be
/// taken back.
///
/// The aperture used is a frame of RAM the caller allocated for it, rather
/// than registers — nothing is read or written through the window, only
/// translated, because reading RAM through an uncached device mapping while
/// the same bytes sit in a cache is exactly the aliasing the architecture does
/// not define. It used to be the kernel's own image, which `vmap::map_device`
/// now refuses ([`check_no_device_window_over_the_image`]).
/// What is under test is the *address arithmetic*, which is where the bugs
/// are: an I/O APIC's registers start at an offset within their page, and a
/// window that rounded that away would work perfectly for the GIC and silently
/// address the wrong register here.
///
/// A retired frame is never handed out again (`docs/MACHINE-CHECK.md`
/// §3.4): one retired while shared stays allocated when both references
/// go; one retired inside a two-frame block stays allocated when the block
/// is freed, and its neighbour is free; one retired while free is taken off
/// the free lists. And the set refuses a frame once full, saying so once
/// (here with its room lowered to what the check has used). Everything the
/// check retired is forgotten and freed at the end.
///
/// Verifies: L.mm.91, L.mm.92
fn check_retired() -> Result<(), &'static str> {
    use ferrix_frame::State;
    let before = super::RETIRED_COUNT.load(Ordering::Relaxed);
    let checked = retire_three();
    let retired = super::RETIRED_COUNT
        .load(Ordering::Relaxed)
        .saturating_sub(before);
    super::forget_checked_retirements(retired);
    let (shared, block, free) = checked?;
    // The block's free half, claimed back by the check; the rest the
    // forgetting freed.
    mm::deallocate_frames(block + 1, 0);
    if [shared, block, block + 1, free]
        .iter()
        .any(|&frame| !matches!(mm::frame_state(frame), Some(State::Free | State::FreeTail)))
    {
        return Err("a frame the retirement check retired was not free once forgotten");
    }
    println!(
        "  retired  a retired frame shared, in a block and free never came back; a full set \
         refused"
    );
    Ok(())
}

/// [`check_retired`]'s body: three frames retired and their outcomes
/// checked, then the full set. The frames, for the caller to free once they
/// are forgotten.
fn retire_three() -> Result<(super::Frame, super::Frame, super::Frame), &'static str> {
    use ferrix_frame::State;
    let allocated = |frame| mm::frame_state(frame) == Some(State::Allocated);

    let shared = mm::allocate_frames(0).ok_or("no frame for the retirement check")?;
    if mm::share_frame(shared) != Some(2) || mm::retire_frame(shared) != mm::Retired::Marked {
        return Err("a shared frame was not marked retired");
    }
    if mm::retire_frame(shared) != mm::Retired::Already {
        return Err("a retired frame was retired twice");
    }
    let _ = mm::release_frame(shared);
    let _ = mm::release_frame(shared);
    if !allocated(shared) || mm::claim_frame(shared).is_some() {
        return Err("a retired frame went back to the allocator with its last reference");
    }

    let block = mm::allocate_frames(1).ok_or("no block for the retirement check")?;
    if mm::retire_frame(block) != mm::Retired::Marked {
        return Err("a frame in a block was not marked retired");
    }
    mm::deallocate_frames(block, 1);
    if !allocated(block)
        || !matches!(
            mm::frame_state(block + 1),
            Some(State::Free | State::FreeTail)
        )
    {
        return Err("a block holding a retired frame was freed whole, or not at all");
    }
    // Back to the caller as a single allocated frame, freed after the
    // forgetting.
    if mm::claim_frame(block + 1).is_none() {
        return Err("the free half of a retired block could not be claimed");
    }

    let free = mm::allocate_frames(0).ok_or("no frame for the retirement check")?;
    mm::deallocate_frames(free, 0);
    if mm::retire_frame(free) != mm::Retired::Claimed || !allocated(free) {
        return Err("a free frame retired was not taken off the free lists");
    }

    let spare = mm::allocate_frames(0).ok_or("no frame for the retirement check")?;
    super::RETIRED_ROOM.store(
        super::RETIRED_COUNT.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    let full = mm::retire_frame(spare);
    let said = super::RETIRED_FULL_SAID.load(Ordering::Relaxed);
    let again = mm::retire_frame(spare);
    let refused = !mm::is_retired(spare);
    mm::deallocate_frames(spare, 0);
    if full != mm::Retired::Full || again != mm::Retired::Full || !said || !refused {
        return Err("a full set of retired frames took one more, or did not say it was full");
    }
    Ok((shared, block, free))
}

/// Verifies: L.mm.49
fn check_device_windows(aperture: u64) -> Result<(), &'static str> {
    const OFFSET: u64 = 0x40;

    let free_before = mm::free_frames();
    let at = vmap::map_device(aperture + OFFSET, 0x100)
        .map_err(|_| "a device window could not be mapped")?;

    if at % PAGE_SIZE != OFFSET {
        return Err("a device window did not preserve its offset within the page");
    }
    if mm::translate(at) != Some(aperture + OFFSET) {
        return Err("a device window does not resolve to the registers it was asked for");
    }
    match permissions_of(at) {
        Some(flags) if flags.device && !flags.execute => {}
        Some(_) => return Err("a device window is not mapped as device memory"),
        None => return Err("a device window is not mapped at all"),
    }

    vmap::unmap_device(at).map_err(|_| "a device window could not be unmapped")?;
    if mm::translate(at).is_some() {
        return Err("unmapping a device window left the mapping behind");
    }
    if mm::free_frames() != free_before {
        return Err("a device window gave the aperture's frames to the buddy allocator");
    }
    Ok(())
}

/// A device window over any part of the kernel's image is refused, before
/// anything is mapped: its first bytes of text, a range that starts below it
/// and runs into it, its last byte, which is `.bss`, and a page in between.
///
/// The image's text and read-only data have no writable mapping anywhere
/// (`mm::check::check_sealed_image`), and a device window is writable, so one over
/// them would write the code the image mapping runs. This is what used to
/// allow it: `vmap::map_device` mapped any physical address it was given.
/// Nothing is mapped, so nothing is read or written.
///
/// Verifies: L.mm.50
fn check_no_device_window_over_the_image() -> Result<(), &'static str> {
    let (image, len) = mm::image_span();
    if len == 0 {
        return Err("memory bring-up did not record where the kernel image is");
    }
    let windows_before = vmap::usage().allocations;
    let last = image + len - 1;
    let probes = [
        (image, 0x100),
        (image.saturating_sub(PAGE_SIZE), 2 * PAGE_SIZE),
        (last, 1),
        (image + len / 2, PAGE_SIZE),
    ];
    for (phys, bytes) in probes {
        match vmap::map_device(phys, bytes) {
            Err(vmap::VmapError::KernelImage(at)) if at == phys => {}
            Err(_) => {
                return Err("a device window over the kernel image failed for the wrong reason");
            }
            Ok(at) => {
                let _ = vmap::unmap_device(at);
                return Err("a device window over the kernel image was mapped");
            }
        }
    }
    if vmap::usage().allocations != windows_before {
        return Err("a refused device window over the kernel image kept its address space");
    }
    Ok(())
}

/// A device window whose end, rounded out to a page, is past the top of the
/// address space is refused as a bad length, and keeps no address space.
///
/// `map_device` used to add the length to the offset within the page, and
/// round the sum up, with no check: with overflow checks on in every
/// profile, a caller's wrapped range stopped the kernel there, and without
/// them it would have sized the window from the wrapped sum. The ranges are
/// the ways a sum can wrap: the offset plus the length, the rounding up of
/// that, and the base plus the rounded span, the last with an in-range
/// length at the very top page. Nothing is mapped, so nothing is read.
///
/// Verifies: L.mm.51
fn check_no_device_window_wraps() -> Result<(), &'static str> {
    let windows_before = vmap::usage().allocations;
    let top_page = !(PAGE_SIZE - 1);
    let probes = [
        (0x1040, u64::MAX - 0x20),
        (0x1000, u64::MAX),
        (top_page, PAGE_SIZE),
        (top_page + 0x40, 0x100),
    ];
    for (phys, bytes) in probes {
        match vmap::map_device(phys, bytes) {
            Err(vmap::VmapError::BadLength(len)) if len == bytes => {}
            Err(_) => return Err("a device window that wraps failed for the wrong reason"),
            Ok(at) => {
                let _ = vmap::unmap_device(at);
                return Err("a device window that wraps was mapped");
            }
        }
    }
    if vmap::usage().allocations != windows_before {
        return Err("a refused device window that wraps kept its address space");
    }
    Ok(())
}

/// The guard pages either side of an allocation are not mapped.
///
/// Not read or written, only translated. A guard page whose absence is proved
/// by touching it proves it once and takes the machine down with it — the
/// whole point is that there is no handler for a fault there, and stage 3's
/// on-demand window is the only place a kernel fault is resolved rather than
/// reported.
///
/// Verifies: L.mm.43
fn check_vmap_guards(mapping: vmap::Mapping) -> Result<(), &'static str> {
    if mm::translate(mapping.base - PAGE_SIZE).is_some() {
        return Err("the guard page below a vmap allocation is mapped");
    }
    if mm::translate(mapping.end()).is_some() {
        return Err("the guard page above a vmap allocation is mapped");
    }
    Ok(())
}

/// Hammer the frame allocator and require the books to balance.
///
/// **Deliberately allocates nothing on the heap.** `Vec` would be far more
/// convenient here and would also make the check meaningless: growing one takes
/// slab pages out of this very allocator, and `ferrix_heap` documents that slab
/// pages are never returned. The first version of this used a `Vec` and
/// reported a leak that was the heap working as designed.
///
/// Verifies: L.mm.4
fn frames_hammer() -> Result<(), &'static str> {
    /// Blocks held at once. Sixteen bytes each on a 64 KiB boot stack.
    const BATCH: usize = 256;
    /// How many times to fill and drain the batch.
    const ROUNDS: usize = 16;

    let before = mm::free_frames();

    for round in 0..ROUNDS {
        let mut taken = [(0u64, 0u8); BATCH];
        let mut held = 0usize;

        for (step, slot) in taken.iter_mut().enumerate() {
            // Orders 0..4, in a pattern that shifts each round so blocks do not
            // always pair up the same way.
            let order = ((step + round) % 5) as u8;
            let Some(frame) = mm::allocate_frames(order) else {
                break;
            };
            *slot = (frame, order);
            held += 1;
        }
        if held == 0 {
            return Err("the frame allocator handed out nothing");
        }

        // Free every other block first, then the rest. Buddies come apart and
        // then back together, which is the path that actually exercises
        // coalescing -- freeing in allocation order barely does.
        for (frame, order) in taken.iter().take(held).skip(1).step_by(2) {
            mm::deallocate_frames(*frame, *order);
        }
        for (frame, order) in taken.iter().take(held).step_by(2) {
            mm::deallocate_frames(*frame, *order);
        }
    }

    let after = mm::free_frames();
    if after != before {
        return Err("frames leaked: the free count did not return to where it started");
    }
    Ok(())
}

/// Frame 0 is neither managed nor free, and cannot be allocated, on every
/// architecture.
///
/// Physical address 0 must never be a frame: it means "none" in places -- a
/// GIC base, the loader's root checks -- and Linux never hands out page 0
/// either. The assertion is the same on all three architectures, whatever
/// their RAM looks like. On x86-64 it is load-bearing, because OVMF reports
/// 0x0-0x9FFFF as conventional memory: without `mm`'s exclusion frame 0 was an
/// ordinary free frame, and under KVM it became an address space's root
/// (FX-0601). On the Arm machines RAM starts above zero, so frame 0 is outside
/// the allocator's array as well as excluded.
///
/// Verifies: L.mm.2
fn check_frame_zero_is_never_managed() -> Result<(), &'static str> {
    if mm::frame_state(0).is_some_and(|state| state != ferrix_frame::State::Reserved) {
        return Err("frame 0 is managed by the frame allocator");
    }
    if let Some(frame) = mm::allocate_frames_below(0, 1) {
        mm::deallocate_frames(frame, 0);
        return Err("frame 0 was handed out");
    }
    Ok(())
}

/// Check the frame allocator hands out distinct, aligned blocks.
///
/// Verifies: L.mm.3
fn check_frames() -> Result<(), &'static str> {
    check_frame_zero_is_never_managed()?;
    let first = mm::allocate_frames(0).ok_or("no frame available")?;
    let second = mm::allocate_frames(0).ok_or("only one frame available")?;
    if first == second {
        return Err("the same frame was handed out twice");
    }

    let block = mm::allocate_frames(4).ok_or("no sixteen-frame block available")?;
    if !block.is_multiple_of(16) {
        return Err("a sixteen-frame block is not sixteen-frame aligned");
    }

    mm::deallocate_frames(block, 4);
    mm::deallocate_frames(second, 0);
    mm::deallocate_frames(first, 0);

    frames_hammer()
}

/// What the heap check says when the heap refuses it.
const NO_HEAP: &str = "the heap refused an allocation it had room for";

/// Check that `alloc` works, which is the whole point of the stage.
///
/// Verifies: L.mm.12
fn check_heap() -> Result<(), &'static str> {
    // A `Box`, which is the smallest possible proof that `GlobalAlloc` is wired
    // up at all.
    let boxed = fallible::try_box(0x5EED_1234_ABCD_0001u64).map_err(|_| NO_HEAP)?;
    if *boxed != 0x5EED_1234_ABCD_0001 {
        return Err("a Box did not hold what was put in it");
    }
    drop(boxed);

    // A `Vec` that grows through several reallocations, so the heap has to
    // move data between size classes and then between whole pages.
    let mut values: Vec<u64> = Vec::new();
    for value in 0..4096u64 {
        fallible::try_push(&mut values, value.wrapping_mul(2_654_435_761)).map_err(|_| NO_HEAP)?;
    }
    for (index, value) in values.iter().enumerate() {
        if *value != (index as u64).wrapping_mul(2_654_435_761) {
            return Err("a Vec did not survive its own reallocations");
        }
    }
    drop(values);

    // A `BTreeMap`, which allocates nodes of an awkward size and frees them in
    // an order nothing controls.
    let mut map: BTreeMap<u64, u64> = BTreeMap::new();
    for key in 0..2048u64 {
        let _ = fallible::insert(&mut map, key.wrapping_mul(2_654_435_761) % 100_003, key)
            .map_err(|_| NO_HEAP)?;
    }
    let entries = map.len();
    if entries == 0 {
        return Err("a BTreeMap held nothing");
    }
    for (key, value) in &map {
        if value.wrapping_mul(2_654_435_761) % 100_003 != *key {
            return Err("a BTreeMap returned a value under the wrong key");
        }
    }
    drop(map);

    if mm::heap_pages() == 0 {
        return Err("the heap never took a page, so nothing was really allocated");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The direct map's RAM (F-55)
// ---------------------------------------------------------------------------

/// The runs of RAM [`super::record_ram`] recorded, lowest first.
fn ram_runs() -> impl Iterator<Item = (u64, u64)> {
    let count = super::RAM_RUN_COUNT.load(Ordering::Acquire);
    super::RAM_RUNS
        .iter()
        .take(count)
        .map(|(first, end)| (first.load(Ordering::Relaxed), end.load(Ordering::Relaxed)))
}

/// A physical page past every byte of RAM the direct map translates: the
/// first gibibyte boundary at or past the end of the highest run, where the
/// window a virtual GPU exposes host memory through sits on a machine with
/// that much RAM (F-55).
pub fn past_ram() -> u64 {
    let end = ram_runs().map(|(_, end)| end).max().unwrap_or(0);
    end.next_multiple_of(1 << 30)
}

/// A physical page between two runs of RAM, inside the span the direct map
/// covers but not RAM, if the memory map has one: where a device's window
/// would have a cacheable alias if the direct map translated it.
pub fn hole_in_ram() -> Option<u64> {
    let mut runs = ram_runs().peekable();
    while let Some((_, end)) = runs.next() {
        if let Some(&(next, _)) = runs.peek()
            && next > end
        {
            return Some(end);
        }
    }
    None
}

/// `direct_map_ram` answers for RAM and for nothing else.
///
/// The runs are there, whole pages, ascending and apart, as the loaders' own
/// walk of the memory map makes them. A frame the allocator hands out is
/// RAM, and gets the same alias [`direct_map`] gives it; the page past the
/// highest run, and a page between two runs where the map has one, get none.
/// Nothing is read through any of them.
///
/// Verifies: L.mm.62
fn check_only_ram_has_a_checked_alias() -> Result<(), &'static str> {
    let mut previous_end = None;
    for (first, end) in ram_runs() {
        if first >= end || !first.is_multiple_of(PAGE_SIZE) || !end.is_multiple_of(PAGE_SIZE) {
            return Err("a recorded run of RAM is empty or not whole pages");
        }
        if previous_end.is_some_and(|previous| first < previous) {
            return Err("the recorded runs of RAM overlap or are out of order");
        }
        previous_end = Some(end);
    }
    if previous_end.is_none() {
        return Err("no run of RAM was recorded");
    }
    let frame = mm::allocate_frames(0).ok_or("no frame for the direct-map check")?;
    let answered = mm::direct_map_ram(frame * PAGE_SIZE);
    mm::deallocate_frames(frame, 0);
    if answered != Some(direct_map(frame * PAGE_SIZE)) {
        return Err("a frame the allocator handed out has no checked direct-map alias");
    }
    if mm::direct_map_ram(past_ram()).is_some() {
        return Err("a page past all RAM has a checked direct-map alias");
    }
    if hole_in_ram().is_some_and(|hole| mm::direct_map_ram(hole).is_some()) {
        return Err("a page between two runs of RAM has a checked direct-map alias");
    }
    Ok(())
}

/// The region kept across a reset stays the kernel's: after the loader's and
/// the firmware's memory is reclaimed, every frame of it is Reserved to the
/// allocator (or outside its array), so none is free or handed out.
/// Whatever `checks` says, since the reclaim runs either way. Takes the
/// frame allocator's lock once a frame, 257 times for the 1 MiB region.
///
/// Verifies: `L.mm.75`
pub fn check_pstore_never_free() -> Result<(), &'static str> {
    let Some((phys, len)) = crate::console::pstore::attached() else {
        return Ok(());
    };
    let first = phys / PAGE_SIZE;
    for frame in first..first + len / PAGE_SIZE {
        // Reserved, or outside the allocator's array: anything else is a
        // frame the allocator owns, free or already handed out.
        if !matches!(
            mm::frame_state(frame),
            None | Some(ferrix_frame::State::Reserved)
        ) {
            return Err("a frame of the pstore region is the allocator's, free or handed out");
        }
    }
    println!("  pstore   none of its {} frames is free", len / PAGE_SIZE);
    Ok(())
}

// CHECKS-ONLY: object/alloc_check.rs
/// A section entered for a large block and closed without using it gives the
/// block back: while it is open the heap has the block's pages out, and once
/// it has closed the heap's large pages and the free frames are where they
/// were before it. The section's end asks the reserve whether it holds a
/// large block before it takes the heap's lock (`Reserve::holds_large`), so a
/// wrong answer there would keep the pages out of the frame allocator, which
/// nothing else would show.
///
/// Verifies: L.mm.38
///
/// Both counts are the whole machine's, and another processor's page taken
/// or given between two reads moves them: an attempt so disturbed is made
/// again, at most [`LARGE_BLOCK_ATTEMPTS`] times, and one undisturbed attempt
/// passes. A defect fails every attempt; the boot line says how many were
/// disturbed and why the first was.
#[doc(hidden)]
pub fn check_a_large_block_goes_back_with_the_section() -> Result<(), &'static str> {
    let mut outcome = Ok(());
    let mut disturbed = 0usize;
    let mut first = None;
    for _ in 0..LARGE_BLOCK_ATTEMPTS {
        outcome = a_large_block_goes_back_once();
        match outcome {
            Ok(()) => break,
            Err(why) => {
                disturbed += 1;
                first = first.or(Some(why));
            }
        }
    }
    println!(
        "  reserve  a large block back with its section: {} of {} attempts disturbed{}{}",
        disturbed,
        LARGE_BLOCK_ATTEMPTS,
        if first.is_some() { ", the first: " } else { "" },
        first.unwrap_or(""),
    );
    // Every attempt failed: the first one's reason is the one to give, since
    // a later attempt starts from whatever the failed one left behind.
    outcome.map_err(|last| first.unwrap_or(last))
}

/// How many times [`check_a_large_block_goes_back_with_the_section`] tries.
const LARGE_BLOCK_ATTEMPTS: usize = 5;

/// One attempt of [`check_a_large_block_goes_back_with_the_section`].
fn a_large_block_goes_back_once() -> Result<(), &'static str> {
    // Three pages: past every size class, an order-two block.
    let layout = core::alloc::Layout::from_size_align(3 * PAGE_SIZE as usize, 8)
        .map_err(|_| "no layout for a large block")?;
    // A section first, so the reserve's size classes are full and the one
    // measured moves only the large block.
    drop(mm::reserve(None).map_err(|_| "the reserve could not be filled")?);
    let (pages, frames) = (mm::heap_large_pages(), mm::free_frames());
    let section = mm::reserve(Some(layout))
        .map_err(|_| "a section for a large block could not be entered")?;
    let held = mm::heap_large_pages();
    drop(section);
    if held <= pages {
        return Err("a section entered for a large block held none");
    }
    if mm::heap_large_pages() != pages || mm::free_frames() != frames {
        return Err("a section closed without using its large block kept it");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The per-processor heap caches (docs/HEAP-CACHES.md)
// ---------------------------------------------------------------------------

/// The class the cache checks use, 2 KiB: the one the kernel allocates
/// least of between two lines of a check, so an interrupt's allocation on
/// the same processor is least likely to move its counts.
const CHECK_CLASS: usize = ferrix_heap::CLASSES - 1;

/// [`CHECK_CLASS`]'s size, and a layout of it.
const CHECK_SIZE: usize = ferrix_heap::CLASS_SIZES[CHECK_CLASS];

/// A layout of [`CHECK_CLASS`].
fn check_layout() -> core::alloc::Layout {
    // A power of two and at most a page: always a layout.
    core::alloc::Layout::from_size_align(CHECK_SIZE, 8).unwrap_or(core::alloc::Layout::new::<u8>())
}

/// Allocate one object of [`CHECK_CLASS`] through the global allocator.
fn take_one() -> Result<*mut u8, &'static str> {
    // SAFETY: (KMEM) a layout of non-zero size; the pointer is given back through
    // `give_one` with the same layout, or kept as a check requires.
    // `black_box`, as in `injected_refusal_is_not_retried`.
    let object = core::hint::black_box(unsafe { alloc::alloc::alloc(check_layout()) });
    if object.is_null() {
        return Err("an allocation of the check's class was refused");
    }
    Ok(object)
}

/// Free an object [`take_one`] made.
fn give_one(object: *mut u8) {
    // SAFETY: (KMEM) `object` came from `take_one`, with this layout, and is freed
    // once.
    unsafe { alloc::alloc::dealloc(object, check_layout()) }
}

// CHECKS-ONLY: object/alloc_check.rs
/// The per-processor heap caches, on the processor the check runs on,
/// preemption held off so every step is that processor's: an empty cache
/// refills `REFILL` objects under one hold and hands out one; the rest come
/// from the cache without the heap's count moving; frees go to the cache
/// without it moving; a full cache's next free gives back `REFILL` and that
/// one; `heap_allocated()` is the heap's count less the cached bytes; the
/// drain leaves the cache empty and the count where it was. Then the
/// injection rule and the refusal's drain, and every processor at once.
///
/// Verifies: L.mm.93, L.mm.94, L.mm.96
#[doc(hidden)]
pub fn check_heap_caches() -> Result<(), &'static str> {
    crate::sched::preempt_disable();
    let outcome = caches_on_one_processor();
    crate::sched::preempt_enable();
    outcome?;
    injected_refusal_is_not_retried()?;
    refusal_drains_every_cache()?;
    reserve_refusals_drain_every_cache()?;
    caches_on_every_processor()
}

/// [`check_heap_caches`]'s single-processor half. The heap's count it reads
/// is the whole machine's, and another processor's allocation or free
/// between two reads moves it: an attempt so disturbed is made again, at
/// most [`ONE_PROCESSOR_ATTEMPTS`] times, and one undisturbed attempt with
/// every count exact passes. A defect fails every attempt.
fn caches_on_one_processor() -> Result<(), &'static str> {
    let mut outcome = Ok(());
    let mut disturbed = 0usize;
    let mut first = None;
    for _ in 0..ONE_PROCESSOR_ATTEMPTS {
        outcome = caches_on_one_processor_once();
        match outcome {
            Ok(()) => break,
            Err(why) => {
                disturbed += 1;
                first = first.or(Some(why));
            }
        }
    }
    // Said on every boot, zero included, so a defect that fails only some
    // attempts shows in the row's log even when the check passes.
    crate::console::println!(
        "  heap     caches: {} of {} single-processor attempts disturbed{}{}",
        disturbed,
        ONE_PROCESSOR_ATTEMPTS,
        if first.is_some() { ", the first: " } else { "" },
        first.unwrap_or(""),
    );
    outcome
}

/// How many times [`caches_on_one_processor`] tries.
const ONE_PROCESSOR_ATTEMPTS: usize = 5;

/// One attempt of [`caches_on_one_processor`]; gives back everything it
/// took, whatever it answers.
fn caches_on_one_processor_once() -> Result<(), &'static str> {
    let refill = ferrix_heap::REFILL;
    let depth = ferrix_heap::CACHE_DEPTH;
    // Objects to free into a full cache later, made before the drain so
    // the drain leaves the class empty.
    let mut spare = Vec::new();
    spare
        .try_reserve(depth + 1)
        .map_err(|_| "no room for the check's list")?;
    for _ in 0..=depth {
        spare.push(take_one()?);
    }
    // Every list the check keeps is made before the counts are read: its own
    // allocation would move them.
    let mut out = Vec::new();
    out.try_reserve(refill)
        .map_err(|_| "no room for the check's list")?;
    let _ = mm::drain_caches();
    if mm::cached_here(CHECK_CLASS) != Some(0) {
        for object in spare.drain(..) {
            give_one(object);
        }
        return Err("the drain left an object in this processor's cache");
    }
    let base = mm::heap_raw_allocated();

    // L.mm.93: an empty cache refills under one hold, then serves alone.
    let started = crate::timer::now_nanos();
    let first = take_one()?;
    let refill_nanos = crate::timer::now_nanos().saturating_sub(started);
    let refilled = mm::heap_raw_allocated();
    out.push(first);
    for _ in 1..refill {
        out.push(take_one()?);
    }
    let popped = mm::heap_raw_allocated();
    // L.mm.94: frees go to the cache.
    for &object in &out {
        give_one(object);
    }
    let pushed = mm::heap_raw_allocated();
    let held = mm::cached_here(CHECK_CLASS);
    let started = crate::timer::now_nanos();
    let _ = mm::drain_caches();
    let mut drain_nanos = crate::timer::now_nanos().saturating_sub(started);
    let drained = mm::heap_raw_allocated();
    let first = if refilled != base + refill * CHECK_SIZE {
        Err("an empty cache's refill did not take REFILL objects from the heap")
    } else if popped != refilled {
        Err("an allocation the cache could serve went to the heap")
    } else if pushed != popped || held != Some(refill) {
        Err("a free the cache had room for went to the heap")
    } else if drained != base {
        Err("the drain did not give the cache's objects back to the heap")
    } else {
        Ok(())
    };
    if first.is_err() {
        for object in spare.drain(..) {
            give_one(object);
        }
        let _ = mm::drain_caches();
        return first;
    }

    // L.mm.94: a full cache's next free gives back REFILL and that one;
    // L.mm.96: what callers hold is the heap's count less the cached bytes.
    for &object in spare.iter().take(depth) {
        give_one(object);
    }
    let full = mm::heap_raw_allocated();
    let callers = mm::heap_allocated();
    let cached = mm::cached_bytes();
    let last = spare
        .get(depth)
        .copied()
        .ok_or("the check's list is short")?;
    give_one(last);
    let overflowed = mm::heap_raw_allocated();
    let after = mm::cached_here(CHECK_CLASS);
    let started = crate::timer::now_nanos();
    let _ = mm::drain_caches();
    drain_nanos = drain_nanos.max(crate::timer::now_nanos().saturating_sub(started));
    crate::console::println!(
        "  heap     caches: a refill of {} objects in {} ns, the longest drain {} ns",
        refill,
        refill_nanos,
        drain_nanos,
    );
    if full != base {
        return Err("frees into a cache with room went to the heap");
    }
    if callers != full.saturating_sub(cached) || cached < depth * CHECK_SIZE {
        return Err("heap_allocated is not the heap's count less the cached bytes");
    }
    if overflowed != full - (refill + 1) * CHECK_SIZE || after != Some(depth - refill) {
        return Err("a full cache's free did not give back REFILL objects and the freed one");
    }
    Ok(())
}

/// One page: past every size class, so its allocation always goes to the
/// frame allocator through the heap, whatever the caches and lists hold.
fn page_layout() -> core::alloc::Layout {
    // A page, page-aligned: always a layout.
    core::alloc::Layout::from_size_align(PAGE_SIZE as usize, PAGE_SIZE as usize)
        .unwrap_or(core::alloc::Layout::new::<u8>())
}

/// L.mm.98: a refusal the failure injection made is answered at once, with
/// warm caches: a page's allocation needs a frame, the injection refuses
/// that frame, and the allocation comes back null with exactly one failure
/// counted. A retry after a drain would ask the frame allocator again, and
/// the one-shot injection would not refuse it twice.
///
/// Verifies: L.mm.98
fn injected_refusal_is_not_retried() -> Result<(), &'static str> {
    let task = crate::sched::current_id().ok_or("the cache check runs outside a task")?;
    // Warm: a few objects of the check's class freed into this cache.
    let mut warm = Vec::new();
    warm.try_reserve(4)
        .map_err(|_| "no room for the check's list")?;
    for _ in 0..4 {
        warm.push(take_one()?);
    }
    for object in warm {
        give_one(object);
    }
    fallible::inject_once(task, 1, true);
    // SAFETY: (KMEM) a non-zero layout; a null return is the answer looked for.
    // `black_box`: an allocation whose only use is its free may otherwise be
    // left out by the compiler, and the allocator never asked.
    let refused = core::hint::black_box(unsafe { alloc::alloc::alloc(page_layout()) });
    let failed = fallible::stop_injecting();
    if !refused.is_null() {
        // SAFETY: (KMEM) made just above with this layout.
        unsafe { alloc::alloc::dealloc(refused, page_layout()) };
    }
    let _ = mm::drain_caches();
    if !refused.is_null() || failed != 1 {
        crate::console::println!(
            "  heap     injected refusal: served {}, the injection failed {} allocation(s)",
            !refused.is_null(),
            failed,
        );
        return Err("an injected refusal was retried after a drain instead of answered");
    }
    Ok(())
}

/// L.mm.95, L.mm.97: with every free frame held, a page's allocation the
/// heap cannot make drains every cache and is served from what the drain
/// gave back: this processor's cache holds 2 KiB objects, two a page, made
/// one after another, so whole pages of them come free when they go back.
///
/// Verifies: L.mm.95, L.mm.97
fn refusal_drains_every_cache() -> Result<(), &'static str> {
    const MADE: usize = 32;
    let mut made = Vec::new();
    made.try_reserve(MADE)
        .map_err(|_| "no room for the check's list")?;
    let mut frames = Chain::new();
    crate::sched::preempt_disable();
    let outcome = (|| {
        let _ = mm::drain_caches();
        for _ in 0..MADE {
            made.push(take_one()?);
        }
        // Into this processor's cache, the first ones freed staying there
        // (a full cache gives back its newest), whole pages of them.
        for object in made.drain(..) {
            give_one(object);
        }
        // Every free frame held: the page the next allocation needs is none.
        while let Some(frame) = mm::allocate_frames(0) {
            frames.push(frame);
        }
        let cached_before = mm::cached_bytes();
        // SAFETY: (KMEM) a non-zero layout; freed below with the same one.
        // `black_box`, as in `injected_refusal_is_not_retried`.
        let served = core::hint::black_box(unsafe { alloc::alloc::alloc(page_layout()) });
        let cached_after = mm::cached_bytes();
        if !served.is_null() {
            // SAFETY: (KMEM) made just above with this layout.
            unsafe { alloc::alloc::dealloc(served, page_layout()) };
        }
        if cached_before < 4 * CHECK_SIZE {
            return Err("the check's cache held too little to free a page");
        }
        if cached_after >= cached_before {
            return Err("a refusal did not drain the caches");
        }
        if served.is_null() {
            return Err("a refusal drained the caches but did not try again");
        }
        Ok(())
    })();
    while let Some(frame) = frames.pop() {
        mm::deallocate_frames(frame, 0);
    }
    crate::sched::preempt_enable();
    let _ = mm::drain_caches();
    outcome
}

/// L.mm.95 for a reserve: a section's fill and its large hold, each refused
/// by the heap with every free frame held, drain every cache and are served
/// from what the drain gave back. For the fill, one 2 KiB object is taken
/// out of this processor's reserve and every free object of that class out
/// of the heap, so the fill needs one the caches hold; for the large hold,
/// a page, which only whole pages of cached objects give back.
///
/// Verifies: L.mm.95
fn reserve_refusals_drain_every_cache() -> Result<(), &'static str> {
    crate::sched::preempt_disable();
    let fill = reserve_refusal_drains(None);
    crate::sched::preempt_enable();
    let _ = mm::drain_caches();
    fill.map_err(|why| match why {
        "a refusal did not drain the caches" => "a reserve's fill did not drain the caches",
        other => other,
    })?;
    crate::sched::preempt_disable();
    let large = reserve_refusal_drains(Some(page_layout()));
    crate::sched::preempt_enable();
    let _ = mm::drain_caches();
    large.map_err(|why| match why {
        "a refusal did not drain the caches" => "a reserve's large hold did not drain the caches",
        other => other,
    })
}

/// [`reserve_refusals_drain_every_cache`]'s one case: enter a section
/// holding `large`, or with the reserve one object short when `None`.
fn reserve_refusal_drains(large: Option<core::alloc::Layout>) -> Result<(), &'static str> {
    const MADE: usize = 32;
    /// The end of the list of the class's spare objects.
    const END: u64 = u64::MAX;
    let request = ferrix_heap::Request::new(CHECK_SIZE, CHECK_SIZE);
    // Full first, so the one refusal the case makes is the one it names.
    drop(super::reserve::reserve(None).map_err(|_| "the reserve did not fill before the check")?);
    let _ = mm::drain_caches();
    let mut made = Vec::new();
    made.try_reserve(MADE)
        .map_err(|_| "no room for the check's list")?;
    let taken = match large {
        Some(_) => None,
        None => Some(
            super::reserve::take_from_reserve(request)
                .ok_or("the reserve held no object of the check's class")?,
        ),
    };
    for _ in 0..MADE {
        made.push(take_one()?);
    }
    for object in made.drain(..) {
        give_one(object);
    }
    let mut frames = Chain::new();
    while let Some(frame) = mm::allocate_frames(0) {
        frames.push(frame);
    }
    // For the fill, the class's free objects in the heap taken as well,
    // linked through their own first word: the fill then needs what the
    // caches hold.
    let mut spare = END;
    if large.is_none() {
        while let Some(object) =
            super::with_heap(|heap| heap.allocate(&mut super::KernelPages, request).ok())
        {
            // SAFETY: (KMEM) a 2 KiB object the heap has just handed out, which
            // nothing else holds; its first word links the list until it is
            // given back below.
            unsafe { (object as *mut u64).write(spare) };
            spare = object;
        }
    }
    let cached_before = mm::cached_bytes();
    let section = super::reserve::reserve(large);
    let cached_after = mm::cached_bytes();
    let entered = section.is_ok();
    drop(section);
    let full = super::reserve::full_here();
    while spare != END {
        // SAFETY: (KMEM) written above, before the object was linked.
        let next = unsafe { (spare as *const u64).read() };
        super::with_heap(|heap| heap.deallocate(&mut super::KernelPages, spare, request));
        spare = next;
    }
    while let Some(frame) = frames.pop() {
        mm::deallocate_frames(frame, 0);
    }
    if let Some(object) = taken {
        super::with_heap(|heap| heap.deallocate(&mut super::KernelPages, object, request));
    }
    if cached_before < 4 * CHECK_SIZE {
        return Err("the check's cache held too little to free a page");
    }
    if cached_after >= cached_before {
        return Err("a refusal did not drain the caches");
    }
    if !entered {
        return Err("a reserve's refusal drained the caches but did not try again");
    }
    if !full {
        return Err("the section's reserve was not full after it was entered");
    }
    Ok(())
}

/// Set when [`cache_worker`]s are to stop.
static CACHE_STOP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// [`cache_worker`]s that have finished.
static CACHE_DONE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// [`cache_worker`]s that saw an allocation refused.
static CACHE_REFUSED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Objects the workers have made and not yet freed.
static CACHE_LIVE: core::sync::atomic::AtomicIsize = core::sync::atomic::AtomicIsize::new(0);

/// Objects handed between processors: one worker allocates, another frees.
static CACHE_HANDOFF: crate::sync::SpinLock<Vec<(usize, usize)>> =
    crate::sync::SpinLock::new(Vec::new());

/// One processor's share of [`caches_on_every_processor`]: allocate objects
/// of every class, free half of them itself and hand the rest to whichever
/// worker frees next, and drain every cache now and then -- a refusal's
/// path -- while the others are in their refills and overflows.
fn cache_worker(index: usize) {
    let mut rounds = 0usize;
    while !CACHE_STOP.load(Ordering::Acquire) && rounds < 2000 {
        let class = (index + rounds) % ferrix_heap::CLASSES;
        let size = ferrix_heap::CLASS_SIZES.get(class).copied().unwrap_or(8);
        let Ok(layout) = core::alloc::Layout::from_size_align(size, 8) else {
            break;
        };
        // SAFETY: (KMEM) a non-zero layout; freed below, here or by another worker,
        // with the layout its class gives.
        let object = core::hint::black_box(unsafe { alloc::alloc::alloc(layout) });
        if !object.is_null() {
            let _ = CACHE_LIVE.fetch_add(1, Ordering::Relaxed);
        }
        if object.is_null() {
            let _ = CACHE_REFUSED.fetch_add(1, Ordering::Relaxed);
        } else if rounds.is_multiple_of(2) {
            // SAFETY: (KMEM) just made with this layout.
            unsafe { alloc::alloc::dealloc(object, layout) };
            let _ = CACHE_LIVE.fetch_sub(1, Ordering::Relaxed);
        } else {
            let mut handoff = CACHE_HANDOFF.lock();
            if handoff.len() < handoff.capacity() {
                handoff.push((object as usize, class));
            } else {
                drop(handoff);
                // SAFETY: (KMEM) just made with this layout.
                unsafe { alloc::alloc::dealloc(object, layout) };
                let _ = CACHE_LIVE.fetch_sub(1, Ordering::Relaxed);
            }
        }
        // Another worker's object, freed on this processor.
        let taken = CACHE_HANDOFF.lock().pop();
        if let Some((address, class)) = taken {
            let size = ferrix_heap::CLASS_SIZES.get(class).copied().unwrap_or(0);
            if let Ok(layout) = core::alloc::Layout::from_size_align(size, 8) {
                // SAFETY: (KMEM) a worker made it with this class's layout, and it
                // left the list once.
                unsafe { alloc::alloc::dealloc(address as *mut u8, layout) };
                let _ = CACHE_LIVE.fetch_sub(1, Ordering::Relaxed);
            }
        }
        if rounds % 64 == 63 {
            let _ = mm::drain_caches();
        }
        rounds += 1;
    }
    let _ = CACHE_DONE.fetch_add(1, Ordering::AcqRel);
}

/// Every processor allocating and freeing at once, objects freed on another
/// processor than they were made on, and drains -- each a refusal's path --
/// while the others hold their cache locks and the heap's: the drain takes
/// one cache lock at a time with nothing held, so this ends rather than
/// deadlocks (the review's D1), and the heap's count comes back.
fn caches_on_every_processor() -> Result<(), &'static str> {
    let online = crate::smp::topology().map_or(1, crate::smp::Topology::online);
    {
        let mut handoff = CACHE_HANDOFF.lock();
        handoff.clear();
        handoff
            .try_reserve(256)
            .map_err(|_| "no room for the handoff list")?;
    }
    CACHE_LIVE.store(0, Ordering::Release);
    CACHE_STOP.store(false, Ordering::Release);
    CACHE_DONE.store(0, Ordering::Release);
    CACHE_REFUSED.store(0, Ordering::Release);
    let mut tasks = Vec::new();
    tasks
        .try_reserve(online)
        .map_err(|_| "no room for the workers")?;
    for cpu in 0..online {
        let mut only = ferrix_sched::CpuSet::empty();
        only.insert(cpu).map_err(|_| "no such processor")?;
        tasks.push(crate::sched::spawn_on(
            "heap-cache",
            cache_worker,
            cpu,
            ferrix_sched::NICE_0_WEIGHT,
            cpu,
            only,
        )?);
    }
    let deadline = crate::timer::now_nanos().saturating_add(20_000_000_000);
    while CACHE_DONE.load(Ordering::Acquire) < online {
        if crate::timer::now_nanos() > deadline {
            CACHE_STOP.store(true, Ordering::Release);
            return Err(
                "the heap cache workers did not finish: a drain deadlocked or a worker hung",
            );
        }
        crate::sched::sleep_for(1_000_000);
    }
    drop(tasks);
    // Whatever is left in the handoff list, freed here.
    let left: Vec<(usize, usize)> = core::mem::take(&mut *CACHE_HANDOFF.lock());
    for (address, class) in left {
        let size = ferrix_heap::CLASS_SIZES.get(class).copied().unwrap_or(0);
        if let Ok(layout) = core::alloc::Layout::from_size_align(size, 8) {
            // SAFETY: (KMEM) as in `cache_worker`.
            unsafe { alloc::alloc::dealloc(address as *mut u8, layout) };
            let _ = CACHE_LIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _ = mm::drain_caches();
    if CACHE_REFUSED.load(Ordering::Relaxed) != 0 {
        return Err("an allocation was refused with memory to spare while every processor ran");
    }
    let _ = mm::drain_caches();
    if CACHE_LIVE.load(Ordering::Acquire) != 0 {
        return Err("an object the workers made was never freed");
    }
    // The other processors keep running the kernel and fill their caches
    // again as soon as the drain has passed them, so one drain leaving
    // nothing is what is asked: at most 100 tries.
    if !(0..100).any(|_| {
        let _ = mm::drain_caches();
        mm::cached_bytes() == 0
    }) {
        return Err("the drain left bytes in the caches");
    }
    Ok(())
}
