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

// ---------------------------------------------------------------------------
// The sweeps at the end of bring-up
// ---------------------------------------------------------------------------

/// The W^X sweep after the identity map has gone: no mapping the kernel
/// holds is both writable and executable, and there are executable ones to
/// have swept.
///
/// Verifies: H.MEM.4, L.mm.32
pub(crate) fn sweep_w_xor_x(view: &BootView<'_>) -> Result<(), &'static str> {
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
    #[cfg(target_arch = "arm")]
    check_kernel_tree_is_global()?;

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

/// Once the loader's alias has gone, the kernel's own tree holds no
/// non-global leaf, so `TTBR0`'s tables are the only ones whose entries an
/// ASID tags: what lets one `TTBR0` write change root and ASID together
/// (DDI 0406C.d B3.10.4; `docs/OPAQUE-KERNEL.md` §9.13, item 5's DK1
/// exception).
///
/// Verifies: L.armv7a.17
#[cfg(target_arch = "arm")]
fn check_kernel_tree_is_global() -> Result<(), &'static str> {
    let (leaves, global) = global_leaves(ROOT_TABLE.load(Ordering::Relaxed));
    if leaves == 0 || global != leaves {
        println_unlogged!(
            "  global   {} of the kernel's {leaves} leaves are not global",
            leaves - global
        );
        return Err("the kernel's tree holds a non-global leaf after the loader's alias went");
    }
    println!("  global   {leaves} kernel leaves, every one global");
    Ok(())
}

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

/// The leaves under `root`, and how many of them are global: for ARMv7-A's
/// ASIDs, whose every argument rests on a user leaf being non-global and,
/// once the loader's alias has gone, a kernel leaf global (L.armv7a.17).
#[cfg(target_arch = "arm")]
pub(crate) fn global_leaves(root: u64) -> (u64, u64) {
    let mut global = 0;
    let outcome = sweep(root, |leaf| {
        if leaf.flags.global {
            global += 1;
        }
        true
    });
    (outcome.leaves, global)
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
pub(crate) fn memory_check(stats: &mm::Stats) -> Result<(), &'static str> {
    if stats.managed_frames == 0 {
        return Err("the frame allocator was given nothing");
    }

    check_frames()?;

    // The heap must come back to where it started, and "where it started" is
    // not zero: the vmap arena below is a live `Vec` and a live `BTreeMap`,
    // and requiring zero afterwards would be requiring the arena not to exist.
    // So the balance is checked around the allocations that are supposed to be
    // transient, before anything permanent is built on top of them.
    let heap_before = mm::heap_allocated();
    let pages_before = mm::heap_pages();
    check_heap()?;
    if mm::heap_allocated() != heap_before {
        return Err("the heap did not give everything back");
    }

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
pub(crate) fn past_ram() -> u64 {
    let end = ram_runs().map(|(_, end)| end).max().unwrap_or(0);
    end.next_multiple_of(1 << 30)
}

/// A physical page between two runs of RAM, inside the span the direct map
/// covers but not RAM, if the memory map has one: where a device's window
/// would have a cacheable alias if the direct map translated it.
pub(crate) fn hole_in_ram() -> Option<u64> {
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
