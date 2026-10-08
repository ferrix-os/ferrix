//! Stage 6's self-checks, so far: the memory objects a process is built from,
//! and a processor translating through one.
//!
//! What most of these measure is not "does a VMO work" but the two properties
//! the rest of stage 6 will rest on: that a reservation costs nothing until it
//! is touched, and that every frame an object was given comes back when it is
//! dropped. The second is the one that fails silently — a process that leaks
//! its anonymous memory on exit leaks it at a rate nothing reports, and the
//! machine dies of it an hour into a `rustc` build.
//!
//! The last one measures something different in kind: that the *hardware*
//! agrees with the tables, which no amount of walking them in software can
//! establish. It is the first thing in the tree to put a root the kernel built
//! into a processor's root register.

use ferrix_bootinfo::{KERNEL_HALF_BASE, PAGE_SIZE};

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::IrqControl;
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::mm;
use crate::sync::SpinLock;
use crate::user::space::{Access, AddressSpace, FilePlace, SpaceError, WindowPages};
use crate::user::vmo::{Vmo, VmoError};

/// What the checks measured, for the boot log.
#[derive(Debug)]
pub(crate) struct Report {
    /// Pages a reservation promised.
    pub(crate) reserved: u64,
    /// Pages of it actually committed.
    pub(crate) committed: usize,
    /// Frames the whole check cost, once everything was dropped. Zero, or the
    /// check failed.
    pub(crate) leaked: i64,
    /// Pages faulted into an address space and read back.
    pub(crate) faulted: u64,
    /// Pages the processor itself translated, through an address space
    /// installed on it.
    pub(crate) walked: u64,
    /// Pages a write actually copied, out of those a fork shared.
    pub(crate) copied: u64,
    /// Reads two tasks made of one virtual address in two different address
    /// spaces, each seeing its own.
    pub(crate) swapped: u64,
}

/// Run them. `Err` names the first thing that was not true.
///
/// Verifies: L.user.101
pub(crate) fn run() -> Result<Report, &'static str> {
    // Once before the window, for the reason `object::check::run` gives: the
    // heap keeps a page of each size class the first run touched, and the
    // held-page check is the first to touch some of them.
    hold_pages_through_everything()?;
    check_a_frame_window_sees_a_kept_page_and_a_large_buffer()?;
    // Then wait for the reaper. Stage 5 has just finished a thousand tasks
    // and a few dozen spinners, and the idle loop frees a finished task's
    // stack and drops the task whenever it next runs; every such free can
    // take a heap page or give one back. One landing inside the window below
    // moved the free count by a frame, which this check read as a leak. So
    // every count in this file is taken with no exited task left unreaped.
    let before = quiet_frames()?;
    let window = last_window();

    check_reservation_is_lazy()?;
    check_a_committed_page_is_zeroed()?;
    check_commit_is_idempotent()?;
    check_out_of_range_is_refused()?;
    check_a_shared_page_survives_one_drop()?;
    check_replacing_a_page_releases_the_old_one()?;
    check_a_held_page_keeps_its_frame()?;

    let reserved = 2048;
    let committed = check_only_what_is_touched_is_paid_for(reserved)?;

    check_an_empty_space_maps_nothing()?;
    check_a_region_outside_the_user_half_is_refused()?;
    check_the_kernel_image_is_no_device_memory()?;
    check_a_device_range_that_wraps_is_refused()?;
    check_a_fault_outside_every_region_is_a_segfault()?;
    check_a_write_to_a_read_only_region_is_refused()?;
    check_a_read_of_an_inaccessible_region_is_refused()?;
    let faulted = check_pages_arrive_on_demand_and_go_back()?;
    let walked = check_the_processor_walks_an_installed_space()?;

    check_a_shared_region_survives_fork_as_one_object()?;
    let copied = check_fork_shares_pages_and_a_write_copies_one()?;
    check_a_page_made_writable_after_fork_is_copied()?;
    let swapped = check_two_tasks_keep_their_own_address_spaces()?;
    arch::check_address_space_ids()?;
    let refused = crate::user::edge_check::run()?;
    crate::console::println!(
        "  edges    {refused} refusals of an address space, an object and the kernel arena, each \
         as specified; a window, an in-place growth, a held page and a shared page's copy as \
         they must be"
    );

    // Every unmap above handed the tables it emptied to its shootdown, and
    // every shootdown gave them back: a list dropped unreleased keeps them for
    // good and counts them here (finding F-36).
    if mm::tables_kept() != 0 {
        return Err("an unmap's page tables were never given back: its shootdown did not run");
    }

    // Everything above dropped its objects before returning, so the allocator
    // must be exactly where it started. Signed, because a check that somehow
    // *gained* frames is as wrong as one that lost them and the number should
    // say which.
    let after = quiet_frames()?;
    let leaked = i64::try_from(before).unwrap_or(i64::MAX) - i64::try_from(after).unwrap_or(0);
    if leaked != 0 {
        mm::print_frame_delta("objects", leaked);
        if let Some(window) = window {
            window.report("objects");
        }
        return Err("the checks did not give back every frame they took");
    }

    Ok(Report {
        reserved,
        committed,
        leaked,
        faulted,
        walked,
        copied,
        swapped,
    })
}

/// A fresh object holds no frames at all.
///
/// Verifies: L.user.1
fn check_reservation_is_lazy() -> Result<(), &'static str> {
    let before = quiet_frames()?;
    let vmo = Vmo::new_anonymous(1024).map_err(|_| "no memory for a VMO")?;

    if vmo.committed() != 0 {
        return Err("a fresh object had pages committed");
    }
    expect_frames(before, "reserving a thousand pages cost a frame")?;
    if vmo.len_pages() != 1024 || vmo.len_bytes() != 1024 * PAGE_SIZE {
        return Err("an object's size in bytes disagrees with its size in pages");
    }
    Ok(())
}

/// A committed page reads back as zero, all the way across.
///
/// Verifies: L.user.2
fn check_a_committed_page_is_zeroed() -> Result<(), &'static str> {
    let vmo = Vmo::new_anonymous(4).map_err(|_| "no memory for a VMO")?;
    let frame = vmo.commit(2).map_err(|_| "committing a page failed")?;

    // Through the direct map, which is the only way the kernel can see a page
    // that belongs to an object it has not mapped anywhere.
    let base = mm::direct_map(frame * PAGE_SIZE) as *const u8;
    for offset in 0..PAGE_SIZE as usize {
        // SAFETY: (FRAME) `offset` is below `PAGE_SIZE` and `base` is the start of a
        // page, so the result is inside that page.
        let at = unsafe { base.add(offset) };
        // SAFETY: (FRAME) `frame` was just committed, so it is allocated and nothing
        // else refers to it; the direct map covers every frame of RAM.
        let byte = unsafe { at.read_volatile() };
        if byte != 0 {
            return Err("a freshly committed page was not zeroed");
        }
    }
    Ok(())
}

/// Committing the same page twice hands back the same frame and costs nothing.
///
/// Verifies: L.user.4
fn check_commit_is_idempotent() -> Result<(), &'static str> {
    let vmo = Vmo::new_anonymous(8).map_err(|_| "no memory for a VMO")?;
    let first = vmo.commit(3).map_err(|_| "committing a page failed")?;

    let between = quiet_frames()?;
    let second = vmo.commit(3).map_err(|_| "re-committing a page failed")?;

    if first != second {
        return Err("committing a page twice produced two different frames");
    }
    expect_frames(between, "re-committing a page allocated a second frame")?;
    if vmo.page(3) != Some(first) {
        return Err("a committed page is not where the object says it is");
    }
    if vmo.page(4).is_some() {
        return Err("an untouched page reported a frame");
    }
    Ok(())
}

/// A page past the end of the object is refused rather than allocated.
///
/// Verifies: L.user.5
fn check_out_of_range_is_refused() -> Result<(), &'static str> {
    let vmo = Vmo::new_anonymous(4).map_err(|_| "no memory for a VMO")?;
    match vmo.commit(4) {
        Err(VmoError::OutOfRange { index: 4, pages: 4 }) => {}
        _ => return Err("a page past the end of an object was not refused"),
    }
    if vmo.committed() != 0 {
        return Err("a refused commit allocated a frame anyway");
    }
    Ok(())
}

/// A frame two objects hold is not freed when the first of them goes.
///
/// This is copy-on-write's invariant expressed at the object level, and the
/// check that would catch the whole class of bug the allocator's `StillShared`
/// guard exists for.
///
/// Verifies: L.mm.7, L.user.8
fn check_a_shared_page_survives_one_drop() -> Result<(), &'static str> {
    let before = quiet_frames()?;

    let first = Vmo::new_anonymous(1).map_err(|_| "no memory for a VMO")?;
    let frame = first.commit(0).map_err(|_| "committing a page failed")?;

    // A second holder, as `fork` would install.
    if mm::share_frame(frame) != Some(2) {
        return Err("sharing a committed page did not raise its count");
    }

    drop(first);
    if mm::frame_references(frame) != 1 {
        return Err("dropping one holder did not leave exactly one reference");
    }
    expect_frames(
        before - 1,
        "a page with a holder left was returned to the allocator",
    )?;

    // And the last holder gives it back.
    if !mm::release_frame(frame) {
        return Err("releasing the last reference did not free the frame");
    }
    expect_frames(before, "the frame did not come back")?;
    Ok(())
}

/// Replacing a page hands the old frame back, which is what a copy-on-write
/// fault does once it has copied.
///
/// The old frame must not be freed while the other holder still has it, and
/// must be freed when it does not — the same invariant as
/// [`check_a_shared_page_survives_one_drop`], reached through the object
/// rather than through the allocator.
///
/// Verifies: L.user.14
fn check_replacing_a_page_releases_the_old_one() -> Result<(), &'static str> {
    let before = quiet_frames()?;
    let vmo = Vmo::new_anonymous(1).map_err(|_| "no memory for a VMO")?;
    let original = vmo.commit(0).map_err(|_| "committing a page failed")?;

    // A second holder, so the replace below must not free the original.
    if mm::share_frame(original) != Some(2) {
        return Err("sharing a committed page did not raise its count");
    }

    let copy = mm::allocate_frames(0).ok_or("no frame for the copy")?;
    mm::zero_frame(copy);

    if vmo.replace(0, copy) != Some(original) {
        return Err("replacing a page did not report the frame it displaced");
    }
    if vmo.page(0) != Some(copy) {
        return Err("the object still names the page it replaced");
    }
    if mm::frame_references(original) != 1 {
        return Err("replacing a shared page did not drop the object's reference");
    }

    // The other holder gives it back, and the object gives back the copy.
    if !mm::release_frame(original) {
        return Err("releasing the last reference to the original did not free it");
    }
    drop(vmo);
    expect_frames(
        before,
        "replacing a page leaked either the original or the copy",
    )?;
    Ok(())
}

/// A held page keeps its frame through everything that would otherwise take
/// it away or swap it, and is ordinary again once the last hold goes.
///
/// What a device is given an address for. The frame it writes has to stay the
/// one the object names: decommitting skips it, a copy-on-write replace is
/// refused, a fork copies it rather than sharing it, and a write through the
/// object lands on it.
///
/// Verifies: L.user.27
fn check_a_held_page_keeps_its_frame() -> Result<(), &'static str> {
    let before = quiet_frames()?;
    hold_pages_through_everything()?;
    expect_frames(before, "holding pages leaked a frame")?;
    Ok(())
}

/// What [`check_a_held_page_keeps_its_frame`] measures.
///
/// Verifies: L.user.21, L.user.22, L.user.24, L.user.25, L.user.26
fn hold_pages_through_everything() -> Result<(), &'static str> {
    let vmo = Vmo::new_anonymous(4).map_err(|_| "no memory for a VMO")?;
    vmo.write_page(1, 0, b"held")
        .map_err(|_| "writing a page failed")?;

    // A fork leaves page 1 shared, so holding it has to copy it first.
    let sibling = vmo.fork().map_err(|_| "forking an object failed")?;
    let shared = vmo.page(1).ok_or("a written page has no frame")?;

    let held = vmo.hold(1, 2).map_err(|_| "holding two pages failed")?;
    let &[one, two] = held.frames() else {
        return Err("a hold of two pages did not report two frames");
    };
    if vmo.page(1) != Some(one) || vmo.page(2) != Some(two) {
        return Err("a hold reported frames the object does not name");
    }
    if one == shared || mm::frame_references(one) != 1 || sibling.page(1) != Some(shared) {
        return Err("holding a page a fork left shared did not copy it");
    }
    let mut read = [0; 4];
    vmo.read_page(1, 0, &mut read)
        .map_err(|_| "reading a held page failed")?;
    if &read != b"held" {
        return Err("holding a page lost what the page held");
    }
    drop(sibling);

    if vmo.decommit_range(0, 4) != 0 || vmo.decommit_from(0) != 0 || vmo.committed() != 2 {
        return Err("decommitting took a held page away");
    }
    let copy = mm::allocate_frames(0).ok_or("no frame for a replace")?;
    if vmo.replace(1, copy).is_some() || vmo.page(1) != Some(one) {
        return Err("a copy-on-write replace swapped a held page");
    }
    if !mm::release_frame(copy) {
        return Err("a refused replace did not leave its frame with the caller");
    }

    let forked = vmo
        .fork()
        .map_err(|_| "forking an object with held pages failed")?;
    if forked.page(1) == Some(one) || mm::frame_references(one) != 1 {
        return Err("a fork shared a held page rather than copying it");
    }
    forked
        .read_page(1, 0, &mut read)
        .map_err(|_| "reading a fork's copy of a held page failed")?;
    if &read != b"held" {
        return Err("a fork's copy of a held page lost what the page held");
    }
    drop(forked);

    vmo.write_page(1, 0, b"more")
        .map_err(|_| "writing a held page failed")?;
    if vmo.page(1) != Some(one) {
        return Err("a write moved a held page to another frame");
    }

    // Holds nest: a page stays held until the last hold on it goes.
    let again = vmo
        .hold(2, 1)
        .map_err(|_| "holding a held page again failed")?;
    drop(held);
    if vmo.decommit_range(0, 4) != 1 || vmo.page(2) != Some(two) {
        return Err("dropping one of two holds let a page still held go");
    }
    drop(again);
    if vmo.decommit_range(0, 4) != 1 || vmo.committed() != 0 {
        return Err("dropping the last hold did not let its page go");
    }

    if vmo.hold(3, 2).is_ok() || vmo.hold(0, 0).is_ok() {
        return Err("a hold past the end, or of no pages, was accepted");
    }
    Ok(())
}

/// A large reservation costs exactly the pages that are touched.
///
/// Verifies: L.user.7
fn check_only_what_is_touched_is_paid_for(reserved: u64) -> Result<usize, &'static str> {
    let before = quiet_frames()?;
    let vmo = Vmo::new_anonymous(reserved).map_err(|_| "no memory for a VMO")?;

    // Touch a scattered few, out of order, so a commit that quietly filled a
    // range rather than a page would show up here.
    let touched = [0, 1, 2, 700, 12, 2047, 699];
    for &index in &touched {
        let _ = vmo.commit(index).map_err(|_| "committing a page failed")?;
    }

    let committed = vmo.committed();
    if committed != touched.len() {
        return Err("committing scattered pages did not commit exactly those pages");
    }
    expect_frames(
        before - committed as u64,
        "a reservation cost more frames than the pages touched",
    )?;

    drop(vmo);
    expect_frames(
        before,
        "dropping an object did not give back every page it held",
    )?;
    Ok(committed)
}

// ---------------------------------------------------------------------------
// Address spaces
// ---------------------------------------------------------------------------
//
// Most of these build address spaces and fault pages into them without ever
// installing one on a processor, which is deliberate: the mapper reaches any
// root through the direct map, so the whole of demand paging can be exercised
// before there is a thread at a lower privilege level to exercise it from. The
// last one closes that gap — it installs a space and makes the processor
// translate through it — and what is still not checked here is the privilege
// change, which is the next piece of stage 6.

/// A fresh space has a root, no regions, and the kernel in reach.
///
/// Verifies: L.user.44
fn check_an_empty_space_maps_nothing() -> Result<(), &'static str> {
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;

    if space.region_count() != 0 {
        return Err("a fresh address space already had regions");
    }
    if space.root_table() == 0 || !space.root_table().is_multiple_of(PAGE_SIZE) {
        return Err("an address space's root is not a page-aligned frame");
    }
    Ok(())
}

/// A mapping outside the user half is refused rather than made.
///
/// The check that stops a process asking for kernel addresses and being given
/// them, which on x86-64 -- where both halves share a root -- would hand it the
/// kernel's own tables.
///
/// Verifies: L.user.46
fn check_a_region_outside_the_user_half_is_refused() -> Result<(), &'static str> {
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;

    match space.map_anonymous(KERNEL_HALF_BASE, PAGE_SIZE, VmaFlags::READ_WRITE) {
        Err(SpaceError::NotUserRange(_)) => {}
        _ => return Err("a mapping in the kernel half was not refused"),
    }
    if space.region_count() != 0 {
        return Err("a refused mapping was inserted anyway");
    }
    Ok(())
}

/// A device or window mapping of the kernel's own image is refused, whatever
/// the caller holds, and inserts nothing.
///
/// Both take a physical address from their caller: `io_mapping_map` an
/// aperture's, `mmap` of a GPU blob a window's. Neither can name the image
/// today, since no aperture overlaps the memory map, and a window is a BAR;
/// this is the refusal that holds if either ever does. The image's text has
/// no writable mapping anywhere (`mm::check::check_sealed_image`), and these map
/// read-write.
///
/// Verifies: L.user.47
fn check_the_kernel_image_is_no_device_memory() -> Result<(), &'static str> {
    let (image, _) = mm::image_span();
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;

    match space.map_device(None, PAGE_SIZE, image, VmaFlags::READ_WRITE, false) {
        Err(SpaceError::Refused(_)) => {}
        _ => return Err("a user device mapping of the kernel's image was not refused"),
    }
    let keeper: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
    match space.map_window(
        FilePlace::Anywhere(None),
        2 * PAGE_SIZE,
        WindowPages::all(image.saturating_sub(PAGE_SIZE), 2 * PAGE_SIZE),
        VmaFlags::READ,
        true,
        keeper,
    ) {
        Err(SpaceError::Refused(_)) => {}
        _ => return Err("a user window over the kernel's image was not refused"),
    }
    if space.region_count() != 0 {
        return Err("a refused mapping of the kernel's image was inserted anyway");
    }
    Ok(())
}

/// A device or window mapping whose physical range wraps the address space
/// is a bad range, and inserts nothing.
///
/// The image test clamps the end of the range it is given, so a range that
/// wrapped past the top came out clear of an image low in memory; the map
/// then refused it for its backing, but only because it happened to check
/// the same sum. The top page, whose end is one past the last address, and a
/// whole-pages length from a low page that runs past the top.
///
/// Verifies: L.user.48
fn check_a_device_range_that_wraps_is_refused() -> Result<(), &'static str> {
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
    // The last page's address, and as a length the most whole pages there are.
    let top = !(PAGE_SIZE - 1);
    for (physical, len) in [(top, PAGE_SIZE), (PAGE_SIZE, top)] {
        match space.map_device(None, len, physical, VmaFlags::READ_WRITE, false) {
            Err(SpaceError::BadRange) => {}
            _ => return Err("a user device mapping whose range wraps was not a bad range"),
        }
        let keeper: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
        match space.map_window(
            FilePlace::Anywhere(None),
            len,
            WindowPages::all(physical, len),
            VmaFlags::READ,
            false,
            keeper,
        ) {
            Err(SpaceError::BadRange) => {}
            _ => return Err("a user window whose range wraps was not a bad range"),
        }
    }
    if space.region_count() != 0 {
        return Err("a refused mapping whose range wraps was inserted anyway");
    }
    Ok(())
}

/// A fault where nothing is mapped is the segmentation fault.
///
/// Verifies: L.user.51
fn check_a_fault_outside_every_region_is_a_segfault() -> Result<(), &'static str> {
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let _ = space
        .map_anonymous(0x10_000, 2 * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "mapping failed")?;

    // Just past the region, which is the off-by-one a fault handler gets wrong.
    match space.fault(0x10_000 + 2 * PAGE_SIZE, Access::READ) {
        Err(SpaceError::NotMapped(_)) => Ok(()),
        _ => Err("a fault outside every region was not a segmentation fault"),
    }
}

/// A write to a region that does not permit writing is refused.
///
/// Verifies: L.user.52
fn check_a_write_to_a_read_only_region_is_refused() -> Result<(), &'static str> {
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let _ = space
        .map_anonymous(0x20_000, PAGE_SIZE, VmaFlags::READ)
        .map_err(|_| "mapping failed")?;

    match space.fault(0x20_000, Access::WRITE) {
        Err(SpaceError::Refused(_)) => {}
        _ => return Err("a write to a read-only region was not refused"),
    }
    // And the read it does permit still works.
    space
        .fault(0x20_000, Access::READ)
        .map_err(|_| "a read of a readable region was refused")?;
    Ok(())
}

/// A read of a region that permits nothing is refused, and installs nothing.
///
/// The guard page's check. A region with no permissions exists to make an
/// access fail, and the failure that matters is the quiet one: a *read* of it
/// being serviced with a fresh zero page, which a program sees as memory rather
/// than as `SIGSEGV`. What is asserted is that no translation appears, which is
/// the property; a frame count would be a weaker proxy and one that heap
/// warm-up can move for reasons of its own.
///
/// Verifies: L.user.53
fn check_a_read_of_an_inaccessible_region_is_refused() -> Result<(), &'static str> {
    let at = 0x2800_0000;
    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let _ = space
        .map_anonymous(at, PAGE_SIZE, VmaFlags::NONE)
        .map_err(|_| "mapping an inaccessible region failed")?;

    match space.fault(at, Access::READ) {
        Err(SpaceError::Refused(_)) => {}
        _ => return Err("a read of a region that permits nothing was let through"),
    }
    if mm::translate_in(space.root_table(), at).is_some() {
        return Err("a refused read of an inaccessible region still installed a translation");
    }
    Ok(())
}

/// Pages arrive on the fault that needs them, hold what is written through the
/// space's own tables, and every frame goes back when the space is dropped.
///
/// Verifies: L.user.54
fn check_pages_arrive_on_demand_and_go_back() -> Result<u64, &'static str> {
    let before = quiet_frames()?;
    let base = 0x4000_0000;
    let pages = 4;

    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let _ = space
        .map_anonymous(base, pages * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "mapping failed")?;

    // A whole region mapped and not one frame spent on it yet.
    expect_frames(before - 1, "mapping a region cost more than the root table")?;

    // Fault them in out of order, so a handler that mapped a fixed address
    // rather than the faulting one would fail here.
    for index in [2, 0, 3, 1] {
        space
            .fault(base + index * PAGE_SIZE, Access::WRITE)
            .map_err(|_| "a fault in a mapped region was not resolved")?;
    }

    // Each page must now translate through this space's root, be writable, and
    // hold what is put in it -- read back through the direct map, since the
    // kernel is not running in this address space and cannot use the address
    // the process would.
    for index in 0..pages {
        let virt = base + index * PAGE_SIZE;
        let phys = mm::translate_in(space.root_table(), virt)
            .ok_or("a faulted page does not translate in its own address space")?;

        let at = mm::direct_map(phys) as *mut u64;
        let written = 0xFEED_0000 + index;
        // SAFETY: (FRAME) `phys` is the frame the fault above committed for this page,
        // it is mapped nowhere else, and the direct map covers all of RAM.
        unsafe { at.write_volatile(written) };
        // SAFETY: (FRAME) the same address, just written.
        if unsafe { at.read_volatile() } != written {
            return Err("a faulted page did not hold what was written to it");
        }
    }

    // Re-faulting a page already present must not cost a second frame.
    let settled = quiet_frames()?;
    space
        .fault(base, Access::WRITE)
        .map_err(|_| "re-faulting a present page failed")?;
    expect_frames(
        settled,
        "re-faulting a present page allocated a second frame",
    )?;

    // Unmapping half the region gives back exactly those pages and leaves the
    // rest mapped, which is what `munmap` of part of a mapping has to do.
    let before_unmap = quiet_frames()?;
    let unmap_window = last_window();
    space
        .unmap(base, 2 * PAGE_SIZE)
        .map_err(|_| "unmapping part of a region failed")?;
    let unmapped = quiet_frames()?;
    if unmapped < before_unmap + 2 {
        mm::print_frame_delta("objects", frames_delta(before_unmap + 2, unmapped));
        if let Some(window) = unmap_window {
            window.report("objects");
        }
        return Err("unmapping two pages did not give back two frames");
    }
    if mm::translate_in(space.root_table(), base).is_some() {
        return Err("an unmapped page still translates");
    }
    if mm::translate_in(space.root_table(), base + 2 * PAGE_SIZE).is_none() {
        return Err("unmapping part of a region unmapped the rest of it");
    }

    drop(space);
    expect_frames(
        before,
        "dropping an address space did not give back every frame",
    )?;
    Ok(pages)
}

/// The processor translates through an address space installed on it.
///
/// Everything above walked page tables in software, through the direct map,
/// which proves the tables say the right thing and not that the hardware
/// agrees. This is the one that puts a root in `CR3` or `TTBR0` and then uses
/// an address that means nothing until it is there.
///
/// The proof is the alias. A write through the user virtual address must show
/// up in the frame the address space says backs that page, read back through
/// the direct map — an address that is nothing to do with the tables under
/// test. A processor that had ignored the new root, or found the page through
/// some leftover translation, would fail that comparison rather than merely
/// not crashing.
///
/// # Why it installs twice
///
/// Because on the Arm architectures the first install is the easy one and the
/// second is the real one. This runs before `drop_identity_map`, so the first
/// time round `TTBR0` still holds the loader's identity map and its regime is
/// *enabled* — a root written there walks whether or not anything thought
/// about `TCR.EPD0` / `TTBCR.EPD0`. Uninstalling switches the regime off, as
/// it must, and the second install is therefore the one that has to turn it
/// back on. That is the state every address space switch after the first one
/// happens in, and a version of `install_user_root` that wrote the root and
/// left `EPD0` alone passes the first round and fails the second.
///
/// One consequence worth naming: on Arm this leaves the lower half switched
/// off earlier than `drop_identity_map` would have. That is safe because
/// nothing has executed or read through the lower half since the secondary
/// processors finished starting, and the W^X sweep reaches the identity map's
/// tables through the direct map rather than through `TTBR0` — but it is a
/// real reordering and not an accident.
///
/// Verifies: L.user.55, `L.x86_64.88`
fn check_the_processor_walks_an_installed_space() -> Result<u64, &'static str> {
    let before = quiet_frames()?;
    let base = 0x5000_0000;
    let pages = 2;

    let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let _ = space
        .map_anonymous(base, ROUNDS * pages * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "mapping failed")?;
    for index in 0..ROUNDS * pages {
        space
            .fault(base + index * PAGE_SIZE, Access::WRITE)
            .map_err(|_| "a fault in a mapped region was not resolved")?;
    }

    // Interrupts masked across the whole window. The scheduler does not know
    // about address spaces yet, so a preemption here would resume some other
    // task on a processor translating through this space's tables — and that
    // task's own user addresses, when it has them, would mean the wrong thing.
    let state = <arch::Irq as IrqControl>::disable();

    let mut walked = Ok(());
    for round in 0..ROUNDS {
        // Each round on pages no earlier round touched: with ASIDs a round's
        // entries outlive its uninstall, and a second round on the same pages
        // could hit them rather than walk (`docs/OPAQUE-KERNEL.md` §9.13).
        let first = base + round * pages * PAGE_SIZE;
        walked = walk_through_installed(&space, first, pages, round);

        // Unconditionally, and before the mask is lifted or `space` is
        // dropped: a processor left translating through tables that are then
        // freed is walking memory the allocator has given to somebody else.
        //
        // SAFETY: (TRANSLATE) nothing after this wants a user address — the next round
        // installs its own, and every read outside this loop goes through the
        // direct map.
        unsafe { space.uninstall() };

        if walked.is_err() {
            break;
        }
    }

    <arch::Irq as IrqControl>::restore(state);
    walked?;

    // The user addresses must be meaningless again. Not checked by touching
    // one, which would be a fault with no handler; checked by the kernel still
    // working, which the rest of boot does at length.
    drop(space);
    expect_frames(before, "installing an address space leaked frames")?;
    Ok(pages)
}

/// How many times the space is installed and used. Two, for the reason
/// [`check_the_processor_walks_an_installed_space`] gives at length.
const ROUNDS: u64 = 2;

/// After a fork, both sides must reach the same frames, each with two holders.
///
/// Faults every page back in on both sides for *reading*, which is also what
/// exercises the read fault reinstalling a copy-on-write page read-only — the
/// parent has no mappings left at this point, because fork took them down.
fn check_both_sides_share(
    parent: &AddressSpace,
    child: &AddressSpace,
    base: u64,
    pages: u64,
    original: &[u64],
) -> Result<(), &'static str> {
    for index in 0..pages {
        let at = base + index * PAGE_SIZE;
        parent
            .fault(at, Access::READ)
            .map_err(|_| "the parent could not fault its own page back in")?;
        child
            .fault(at, Access::READ)
            .map_err(|_| "the child could not fault a shared page in")?;

        let parent_phys =
            mm::translate_in(parent.root_table(), at).ok_or("the parent's page does not map")?;
        let child_phys =
            mm::translate_in(child.root_table(), at).ok_or("the child's page does not map")?;
        if parent_phys != child_phys || Some(&parent_phys) != original.get(index as usize) {
            return Err("fork did not share a page: the two sides reach different frames");
        }
        if mm::frame_references(parent_phys / PAGE_SIZE) != 2 {
            return Err("a page shared by two address spaces does not have two holders");
        }
        if peek(child_phys) != PARENT_MARK + index {
            return Err("the child does not see what the parent wrote before forking");
        }
    }
    Ok(())
}

/// Resolve a copy-on-write fault *while the space is installed*, then write
/// through the faulting address, as the retrying instruction would.
///
/// # Why the installation has to bracket the fault
///
/// Because the defect this catches lives in the TLB, and a space that is not
/// installed has nothing in one. The first version of this check installed the
/// space only for the final write, and it passed with the invalidation deleted
/// — installing a root is itself a flush on x86-64, so the stale entry was
/// gone before the write went looking for it. The check was testing its own
/// setup.
///
/// So the order here is the order a real fault happens in. The space goes on
/// the processor, a read through the user address puts the *read-only* entry
/// in the TLB, the handler copies the page and installs a writable entry in
/// the tables, and only then does the write go through the same address. With
/// the invalidation deleted that write meets the stale read-only entry and
/// faults, which is the hang the branch exists to avoid, made loud.
///
/// Nothing else in this file can see a user page's hardware permissions at
/// all: the kernel reaches a page through the direct map, which is writable
/// for all of RAM, so `poke` lands whatever the user entry says. A check that
/// writes that way has tested the region bookkeeping and not one permission
/// bit.
fn fault_and_write_installed(
    space: &AddressSpace,
    at: u64,
    value: u64,
) -> Result<(), &'static str> {
    // Interrupts masked for `install`'s reason: the scheduler does not know
    // about address spaces yet.
    let state = <arch::Irq as IrqControl>::disable();

    // SAFETY: (TRANSLATE) `space` is borrowed across the whole window so its tables
    // outlive the installation, interrupts are masked, and it is uninstalled
    // below before anything else can want a user address.
    unsafe { space.install(None) };

    // As in `walk_through_installed`: reaching a user linear address from ring
    // 0 is the point of the check, and SMAP refuses it without `EFLAGS.AC`.
    arch::permit_user_access();

    let through = at as *mut u64;
    // SAFETY: (PROBE) the caller faulted this page in read-only through `space`, which
    // is installed on this processor. This read is what puts the entry the
    // rest of this function is about into the TLB.
    let _seen = unsafe { through.read_volatile() };

    let resolved = space.fault(at, Access::WRITE);
    if resolved.is_ok() {
        // SAFETY: (PROBE) the handler above has just made this page writable in the
        // tables of the space installed on this processor.
        unsafe { through.write_volatile(value) };
    }

    arch::forbid_user_access();
    // SAFETY: (TRANSLATE) nothing after this wants a user address.
    unsafe { space.uninstall() };
    <arch::Irq as IrqControl>::restore(state);

    resolved.map_err(|_| "a write to a copy-on-write page was not resolved")
}

/// Install `space`, use `pages` pages of it at `base`, and check each write
/// landed in the frame that backs it.
///
/// Split out so that the caller can uninstall on the way out whether this
/// returns `Ok` or `Err`: an early `?` inside the installed window would leave
/// the root on the processor.
///
/// `round` only varies what is written, so that a round finding the previous
/// round's value still in place is a failure rather than a pass.
///
/// Verifies: L.mm.19
fn walk_through_installed(
    space: &AddressSpace,
    base: u64,
    pages: u64,
    round: u64,
) -> Result<(), &'static str> {
    // SAFETY: (TRANSLATE) `space` is borrowed for the whole of this call, so its tables
    // outlive the installation; the caller has masked interrupts and
    // uninstalls before going on.
    unsafe { space.install(None) };

    // SMAP refuses ring 0 a user linear address, and reaching one on purpose is
    // the whole point of this check: it proves the processor walks an installed
    // space. `arch::permit_user_access` sets `EFLAGS.AC` for exactly this, and
    // the window closes below. Nothing in the kernel's ordinary paths needs it
    // -- `syscall::uaccess` goes through the direct map -- which is why this is
    // the only caller.
    arch::permit_user_access();

    // Computed before the window closes, because three of the paths out of
    // this loop are early returns and an `EFLAGS.AC` left set is SMAP switched
    // off for this processor until it next enters user mode.
    let walked = (|| {
        for index in 0..pages {
            let virt = base + index * PAGE_SIZE;
            let written = 0xC0FF_EE00_u64 + round * 0x100 + index;

            let at = virt as *mut u64;
            // SAFETY: (PROBE) the page at `virt` was faulted in by the caller, the
            // region is writable, and this address space is installed on this
            // processor — so this is a write to a page of RAM nothing else is
            // using.
            unsafe { at.write_volatile(written) };
            // SAFETY: (PROBE) the same address, just written.
            if unsafe { at.read_volatile() } != written {
                return Err("a user address did not hold what the processor wrote to it");
            }

            // The alias, and the whole point of the check.
            let Some(phys) = mm::translate_in(space.root_table(), virt) else {
                return Err("a faulted page does not translate in its own address space");
            };
            let alias = mm::direct_map(phys) as *const u64;
            // SAFETY: (FRAME) `phys` is the frame this space says backs `virt`, and
            // the direct map covers every frame of RAM.
            if unsafe { alias.read_volatile() } != written {
                return Err("a write through a user address did not land in the frame behind it");
            }
        }
        Ok(())
    })();

    arch::forbid_user_access();
    walked
}

// ---------------------------------------------------------------------------
// fork and copy-on-write
// ---------------------------------------------------------------------------

/// What the parent writes into each page before it forks.
const PARENT_MARK: u64 = 0xBEEF_0000_0000_0001;

/// What the child writes after its copy-on-write fault has resolved.
const CHILD_MARK: u64 = 0xCAFE_0000_0000_0002;

/// Read the first word of `frame` through the direct map.
fn peek(phys: u64) -> u64 {
    let at = mm::direct_map(phys) as *const u64;
    // SAFETY: (FRAME) `phys` is a frame some address space maps, so it is allocated,
    // and the direct map covers every frame of RAM.
    unsafe { at.read_volatile() }
}

/// Write `value` into the first word of `frame` through the direct map.
fn poke(phys: u64, value: u64) {
    let at = mm::direct_map(phys) as *mut u64;
    // SAFETY: (FRAME) as `peek`, and the page belongs to an object this check owns, so
    // nothing else is reading it.
    unsafe { at.write_volatile(value) };
}

/// A `MAP_SHARED` region is one object either side of a fork, and a write
/// through one mapping is visible through the other.
///
/// The case that must *not* be copied. `MAP_SHARED|MAP_ANONYMOUS` is what
/// `musl` uses for a process-shared mutex, and a fork that quietly gave the
/// child a private copy would leave two processes each waiting on a lock the
/// other cannot see.
///
/// Verifies: L.user.57
fn check_a_shared_region_survives_fork_as_one_object() -> Result<(), &'static str> {
    let before = quiet_frames()?;
    let base = 0x7000_0000;

    let parent = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    let _ = parent
        .map_anonymous(base, PAGE_SIZE, shared)
        .map_err(|_| "mapping a shared region failed")?;
    parent
        .fault(base, Access::WRITE)
        .map_err(|_| "a fault in a shared region was not resolved")?;

    let parent_phys =
        mm::translate_in(parent.root_table(), base).ok_or("a faulted shared page does not map")?;
    poke(parent_phys, PARENT_MARK);

    let child = parent.fork().map_err(|_| "fork failed")?;

    child
        .fault(base, Access::WRITE)
        .map_err(|_| "a write fault in a shared region was not resolved")?;
    let child_phys =
        mm::translate_in(child.root_table(), base).ok_or("the child's shared page does not map")?;

    // The same frame, reached from two address spaces: nothing was copied and
    // nothing was marked copy-on-write, which is what `MAP_SHARED` asks for.
    if child_phys != parent_phys {
        return Err("forking a shared region copied a page it was supposed to share");
    }

    // And one holder, not two. A shared region is one *object* seen twice, so
    // the reference belongs to the object rather than to each space -- which
    // is exactly why a write through it must not trigger a copy.
    if mm::frame_references(parent_phys / PAGE_SIZE) != 1 {
        return Err("a shared object's page gained a second holder at fork");
    }

    // The writes really do meet, which is the property rather than the
    // identity of the frame.
    poke(child_phys, CHILD_MARK);
    if peek(parent_phys) != CHILD_MARK {
        return Err("a write through a shared mapping was not visible through the other");
    }

    drop(child);
    drop(parent);
    expect_frames(before, "forking a shared region leaked frames")?;
    Ok(())
}

/// Fork shares every private page, and the first write to one copies it.
///
/// The whole of copy-on-write in the order the mechanism runs: share, fault,
/// copy, and then decline to copy once there is nobody left to copy away from.
///
/// # What this measures, and what it deliberately does not
///
/// Frame *identity* and reference counts, not the free-frame count. Fork takes
/// the parent's mappings down, which gives its now-empty page tables back, and
/// a fault puts tables back to install a page — so the number of free frames
/// moves for reasons that have nothing to do with whether a page was copied.
/// Identity is the property anyway: two spaces reaching one frame is sharing,
/// and reaching different frames is a copy. The free count still has the last
/// word on leaks, once everything is dropped.
///
/// Verifies: L.user.58, L.user.59
fn check_fork_shares_pages_and_a_write_copies_one() -> Result<u64, &'static str> {
    let before = quiet_frames()?;
    let base = 0x6000_0000;
    let pages = 2;

    let parent = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let _ = parent
        .map_anonymous(base, pages * PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "mapping failed")?;

    let mut original = [0_u64; 2];
    for index in 0..pages {
        parent
            .fault(base + index * PAGE_SIZE, Access::WRITE)
            .map_err(|_| "a fault in a mapped region was not resolved")?;
        let phys = mm::translate_in(parent.root_table(), base + index * PAGE_SIZE)
            .ok_or("a faulted page does not map")?;
        poke(phys, PARENT_MARK + index);
        if let Some(slot) = original.get_mut(index as usize) {
            *slot = phys;
        }
    }

    let child = parent.fork().map_err(|_| "fork failed")?;

    // The parent must have lost its writable translations, or its own next
    // write would reach a page the child can see without faulting at all.
    for index in 0..pages {
        if mm::translate_in(parent.root_table(), base + index * PAGE_SIZE).is_some() {
            return Err("fork left the parent a mapping it could still write through");
        }
    }

    check_both_sides_share(&parent, &child, base, pages, &original)?;

    let shared = *original.first().ok_or("no page was recorded")?;

    // The copy, taken with the child installed on this processor so that both
    // the fault and the write after it go through real translations. A
    // different frame for the child, the parent left as the only holder of the
    // original, and the contents carried across.
    fault_and_write_installed(&child, base, CHILD_MARK)?;
    let copy = mm::translate_in(child.root_table(), base).ok_or("the child's copy does not map")?;
    if copy == shared {
        return Err("a write to a copy-on-write page was let through to the shared frame");
    }
    if mm::frame_references(shared / PAGE_SIZE) != 1 {
        return Err("copying a page did not give back the copier's reference to it");
    }
    if mm::frame_references(copy / PAGE_SIZE) != 1 {
        return Err("a freshly copied page is held by more than its copier");
    }

    // The write went through the child's own virtual address, so this reads
    // back the proof that the processor -- not merely the fault handler -- let
    // it through, and that it landed in the copy and not the original.
    if peek(copy) != CHILD_MARK {
        return Err("a write through the child's own address did not reach its copy");
    }
    if peek(shared) != PARENT_MARK {
        return Err("a write after copy-on-write reached the parent's page");
    }
    // And the parent's mapping of it is still the original frame.
    if mm::translate_in(parent.root_table(), base) != Some(shared) {
        return Err("the child's copy moved the parent's page");
    }

    // The fault must not keep copying. The region stays marked copy-on-write
    // -- that is a property of the region and there is no per-page flag -- so
    // a second write comes back here, finds one holder, and lets it through.
    // Getting this wrong is an instruction that faults forever, whose symptom
    // is a hang rather than a wrong answer.
    child
        .fault(base, Access::WRITE)
        .map_err(|_| "a second write to a copied page was not resolved")?;
    if mm::translate_in(child.root_table(), base) != Some(copy) {
        return Err("a second write to a page already copied copied it again");
    }
    if peek(copy) != CHILD_MARK {
        return Err("a second write fault overwrote a page it should have left alone");
    }

    // And a kernel write into it -- `copy_to_user`'s, a time a program asked
    // for or the bytes a read returns -- goes straight through: the page is
    // this side's alone and mapped writable, so there is nothing to fault in
    // and no translation to take down. Faulting first took it down and put it
    // back on every copy, a shootdown each: thirty thousand a second, for a
    // browser that forked once and then asked the time.
    let begun = child.shootdowns_begun();
    child
        .with_page(base, Access::WRITE, |_| ())
        .map_err(|_| "a kernel write to a page already copied was refused")?;
    if child.shootdowns_begun() != begun {
        return Err("a kernel write to a page already its own took a translation down");
    }

    // The sole-holder case, on the page neither side has written. Dropping the
    // parent leaves the child the only holder, so its write must take the
    // frame it already has rather than duplicate data nobody else can see.
    drop(parent);
    let untouched = *original.get(1).ok_or("no second page was recorded")?;
    if mm::frame_references(untouched / PAGE_SIZE) != 1 {
        return Err("dropping the parent did not leave the child as the only holder");
    }

    child
        .fault(base + PAGE_SIZE, Access::WRITE)
        .map_err(|_| "a write to the last holder's page was not resolved")?;
    if mm::translate_in(child.root_table(), base + PAGE_SIZE) != Some(untouched) {
        return Err("a write to a page with one holder copied it anyway");
    }
    if peek(untouched) != PARENT_MARK + 1 {
        return Err("the last holder's page lost its contents");
    }

    drop(child);
    expect_frames(before, "fork and copy-on-write leaked frames")?;
    Ok(1)
}

/// A private page read-only at a fork, made writable afterwards with
/// `mprotect`, is copied by the side that writes it; the other keeps its own.
///
/// Fork marks only the regions writable at the fork copy-on-write, and shares
/// the frames of the rest, since neither side can write them. `mprotect` is
/// what changes that, so it has to mark the region too: until 2026-09-27 it
/// did not, and a child that made such a page writable wrote into the frame
/// its parent still read. Chromium on AArch64 met it as glibc's stack guard,
/// kept in ld.so's RELRO page there, changing under the parent.
///
/// Verifies: L.user.60
fn check_a_page_made_writable_after_fork_is_copied() -> Result<(), &'static str> {
    let before = quiet_frames()?;
    let base = 0x6100_0000;

    let parent = AddressSpace::new().map_err(|_| "could not make an address space")?;
    let _ = parent
        .map_anonymous(base, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "mapping failed")?;
    parent
        .fault(base, Access::WRITE)
        .map_err(|_| "a fault in a mapped region was not resolved")?;
    let shared =
        mm::translate_in(parent.root_table(), base).ok_or("a faulted page does not map")?;
    poke(shared, PARENT_MARK);
    parent
        .protect(base, PAGE_SIZE, VmaFlags::READ)
        .map_err(|_| "mprotect of a private page to read-only was refused")?;

    let child = parent.fork().map_err(|_| "fork failed")?;
    child
        .protect(base, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "mprotect of a private page back to writable was refused")?;
    child
        .fault(base, Access::READ)
        .map_err(|_| "a read of a page made writable again was not resolved")?;
    fault_and_write_installed(&child, base, CHILD_MARK)?;

    let copy = mm::translate_in(child.root_table(), base).ok_or("the child's page does not map")?;
    if copy == shared || peek(shared) != PARENT_MARK {
        return Err(
            "a write to a page a fork left read-only and mprotect made writable reached the \
             other process's page",
        );
    }
    if peek(copy) != CHILD_MARK {
        return Err("a write to a page mprotect made writable did not reach the writer's copy");
    }

    drop(child);
    drop(parent);
    expect_frames(before, "a page made writable after fork leaked frames")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Address spaces and the scheduler
// ---------------------------------------------------------------------------

/// The one virtual address both tasks use, and disagree about.
const SWAP_AT: u64 = 0x3000_0000;

/// How many times each task reads it.
const SWAP_ROUNDS: u64 = 64;

/// What each task's page holds: this plus the task's index.
const SWAP_MARK: usize = 0xA5A5_0000;

/// Tasks that have finished reading.
static SWAP_DONE: AtomicU64 = AtomicU64::new(0);
/// Reads that saw the reader's own address space.
static SWAP_RIGHT: AtomicU64 = AtomicU64::new(0);
/// Reads that saw somebody else's.
static SWAP_WRONG: AtomicU64 = AtomicU64::new(0);

/// Read this task's own virtual address, over and over, yielding between.
///
/// `expected` is what this task's page holds and no other task's does.
fn read_own_space(expected: usize) {
    for _ in 0..SWAP_ROUNDS {
        let at = SWAP_AT as *const u64;
        // Tight around the read, and deliberately not around the loop: the
        // yield below switches tasks, and `EFLAGS.AC` is part of the context a
        // switch carries. A window held across it would hand SMAP's exception
        // to whatever ran next.
        arch::permit_user_access();
        // SAFETY: (PROBE) this task owns an address space in which `SWAP_AT` is mapped
        // and was faulted in before the task existed, and the scheduler
        // installs that space on whichever processor runs the task, before the
        // first instruction of it. That installation is the thing under test:
        // if it does not happen this faults rather than reading rubbish, which
        // is the failure this check wants.
        let seen = unsafe { at.read_volatile() };
        arch::forbid_user_access();
        if seen == expected as u64 {
            let _ = SWAP_RIGHT.fetch_add(1, Ordering::Relaxed);
        } else {
            let _ = SWAP_WRONG.fetch_add(1, Ordering::Relaxed);
        }
        // Give the processor up so the other task runs and the root is
        // swapped. Without this each task would read its own page sixty-four
        // times in one slice and the check would pass without a single switch.
        crate::sched::yield_now();
    }
    let _ = SWAP_DONE.fetch_add(1, Ordering::Release);
}

/// Two tasks, two address spaces, one virtual address: each must see its own.
///
/// Everything above this builds address spaces and walks them from the task
/// that made them. This is the first thing that hands one to *another* task and
/// lets the scheduler decide when it is live — which is the whole of what
/// `choose_next`'s root swap has to get right.
///
/// # Why both tasks are pinned to one processor
///
/// Because otherwise the check would pass without testing anything. Two tasks
/// on two processors each get their own root installed once and never switched;
/// the interesting case is one processor alternating between two address spaces,
/// which is what a real machine does and what an incorrect comparison or a
/// missing invalidation would break. Pinned and yielding, the two tasks force
/// roughly `SWAP_ROUNDS` swaps each way.
///
/// A reader that sees the *other* task's marker is a root that did not change
/// when it should have. A reader that faults is no root at all. Both are
/// failures and they look different, which is why the marker carries the task's
/// index rather than being a single sentinel.
///
/// Verifies: H.MEM.1, L.user.56, `L.x86_64.110`
fn check_two_tasks_keep_their_own_address_spaces() -> Result<u64, &'static str> {
    let frames_before = quiet_frames()?;
    let arena_before = crate::vmap::usage().allocations;

    SWAP_DONE.store(0, Ordering::Release);
    SWAP_RIGHT.store(0, Ordering::Release);
    SWAP_WRONG.store(0, Ordering::Release);

    let here = crate::smp::this_cpu()
        .ok_or("no processor to run the address space check on")?
        .logical;

    // Two spaces, the same address in each, different contents.
    let mut spaces = Vec::new();
    for index in 0..2_usize {
        let space = AddressSpace::new().map_err(|_| "could not make an address space")?;
        let _ = space
            .map_anonymous(SWAP_AT, PAGE_SIZE, VmaFlags::READ_WRITE)
            .map_err(|_| "mapping failed")?;
        space
            .fault(SWAP_AT, Access::WRITE)
            .map_err(|_| "a fault in a mapped region was not resolved")?;
        let phys = mm::translate_in(space.root_table(), SWAP_AT)
            .ok_or("a faulted page does not translate in its own address space")?;
        poke(phys, (SWAP_MARK + index) as u64);
        spaces.push(space);
    }

    let mut tasks = Vec::new();
    for (index, space) in spaces.iter().enumerate() {
        tasks.push(
            crate::sched::spawn_on_in(
                "space-reader",
                read_own_space,
                SWAP_MARK + index,
                NICE_0_WEIGHT,
                here,
                CpuSet::of(here),
                Some(Arc::clone(space)),
            )
            .map_err(|_| "could not start a task in an address space")?,
        );
    }

    wait_until(
        || SWAP_DONE.load(Ordering::Acquire) >= 2,
        "a task reading its own address space never finished",
    )?;

    let right = SWAP_RIGHT.load(Ordering::Acquire);
    let wrong = SWAP_WRONG.load(Ordering::Acquire);
    if wrong != 0 {
        return Err("a task read another task's address space at its own address");
    }
    if right != SWAP_ROUNDS * 2 {
        return Err("a task did not read its own address space as many times as it should");
    }

    // The tasks die holding the only other references to these spaces, so the
    // stacks have to come back before the tables can, and each task has to be
    // dropped -- by its reaper, and here -- before a space's last reference
    // is this check's.
    reap_until(arena_before)?;
    crate::sched::wait_until_reaper_quiet(PATIENCE_NANOS)?;
    drop(tasks);
    wait_until(
        || spaces.iter().all(|space| Arc::strong_count(space) == 1),
        "a finished task never let go of its address space",
    )?;
    drop(spaces);

    expect_frames(
        frames_before,
        "running tasks in address spaces leaked frames",
    )?;
    Ok(right)
}

/// Wait for `ready`, yielding, until it is true or the patience runs out.
fn wait_until(mut ready: impl FnMut() -> bool, what: &'static str) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !ready() {
        if crate::timer::now_nanos() >= deadline {
            return Err(what);
        }
        crate::sched::yield_now();
    }
    Ok(())
}

/// The window the latest [`quiet_frames`] opened, for [`expect_frames`] to
/// report the routes against when a count is wrong.
static LAST_WINDOW: SpinLock<Option<mm::FrameWindow>> = SpinLock::new(None);

/// The held frame count -- free frames and the heap's slab pages together,
/// as [`mm::FrameWindow`] counts them -- taken once no exited task is left
/// unreaped, so that no task an earlier check ended is freed between two
/// counts, and no slab page a size class takes or gives back reads as a
/// frame. Opens the window [`expect_frames`] reports against.
fn quiet_frames() -> Result<u64, &'static str> {
    crate::sched::wait_until_reaper_quiet(PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    *LAST_WINDOW.lock() = Some(window);
    Ok(window.held())
}

/// The window the latest [`quiet_frames`] opened, kept by a count that spans
/// helpers which open windows of their own, so its mismatch can still report
/// how every route moved since it began.
fn last_window() -> Option<mm::FrameWindow> {
    *LAST_WINDOW.lock()
}

/// Require the held frame count, taken as [`quiet_frames`] takes it, to be
/// `expected`. Otherwise print by how much and which way, and how every
/// route moved since the latest count, and fail with `what`.
fn expect_frames(expected: u64, what: &'static str) -> Result<(), &'static str> {
    crate::sched::wait_until_reaper_quiet(PATIENCE_NANOS)?;
    let found = mm::held_frames();
    if found != expected {
        mm::print_frame_delta("objects", frames_delta(expected, found));
        if let Some(window) = *LAST_WINDOW.lock() {
            window.report("objects");
        }
        return Err(what);
    }
    Ok(())
}

/// `expected` less `found`, signed: above zero is frames kept.
fn frames_delta(expected: u64, found: u64) -> i64 {
    i64::try_from(expected).unwrap_or(i64::MAX) - i64::try_from(found).unwrap_or(i64::MAX)
}

/// Free every exited task's stack, until the arena is back where it started.
///
/// A yielding loop rather than a blocking wait, for the reason stage 5's
/// version gives: reaping is work *this* task does, so it has to keep being
/// given the processor in order to do it.
fn reap_until(allocations: usize) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        let _ = crate::sched::reap();
        if crate::vmap::usage().allocations <= allocations {
            return Ok(());
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("a task in an address space never gave its stack back");
        }
        crate::sched::yield_now();
    }
}

/// How long any of the waits above will wait. Generous: this runs after stage
/// 5, which has already shown that a thousand threads take real time.
const PATIENCE_NANOS: u64 = 20_000_000_000;

/// The frame window's negative control: netting out the heap's slab pages must
/// not net out a frame, or a large heap allocation, that is really kept.
///
/// A page committed to an object and a large heap buffer are held across a
/// window, and it must count them; given back, it must count nothing. A
/// window that absorbed either would pass every check it guards through the
/// very leak those checks exist to catch, and look exactly like a fixed flake.
/// Checked with [`mm::FrameWindow::kept`] rather than `expect`, so a boot that
/// passes prints nothing; the thresholds are one-sided, because another
/// processor's task may give frames back meanwhile but a slab page never
/// counts either way.
fn check_a_frame_window_sees_a_kept_page_and_a_large_buffer() -> Result<(), &'static str> {
    let page = usize::try_from(PAGE_SIZE).map_err(|_| "the page size does not fit")?;
    crate::sched::wait_until_reaper_quiet(PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();

    let vmo = Vmo::new_anonymous(1).map_err(|_| "no memory for a VMO")?;
    let _ = vmo
        .commit(0)
        .map_err(|_| "no frame for the frame window's negative control")?;
    if window.kept() < 1 {
        return Err("a frame window did not count a page committed and kept across it");
    }
    let buffer: Vec<u8> = Vec::with_capacity(4 * page);
    if window.kept() < 5 {
        return Err("a frame window did not count a large heap buffer kept across it");
    }

    drop(buffer);
    drop(vmo);
    crate::sched::wait_until_reaper_quiet(PATIENCE_NANOS)?;
    if window.kept() > 0 {
        return Err("a frame window still counted a page and a buffer that were given back");
    }
    Ok(())
}
