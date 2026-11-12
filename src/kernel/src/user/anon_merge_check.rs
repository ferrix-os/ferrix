//! The core self-check of anonymous mappings made in a neighbour's object
//! (`docs/ANON-MERGE.md`, `L.user.242` to `L.user.244`), on spaces of its
//! own, every profile: Wine's pattern and `brk`'s join one object, each case
//! that must decline makes a new one, every window reads zero, every own
//! private anonymous object stays on one line, and the resident counts equal
//! a recount after every step.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vma::VmaFlags;

use crate::mm;
use crate::user::rss_check;
use crate::user::space::{self, Access, AddressSpace, Destination};
use crate::user::vmo::Vmo;

/// Where each case maps, inside every architecture's user half.
const BASE: u64 = 0x4000_0000;

/// A Wine view's pages.
const VIEW: u64 = 4 * PAGE_SIZE;

/// Views in the Wine case's reserve.
const VIEWS: u64 = 64;

/// Views in the large case's reserve, every second one writable.
pub(crate) const LARGE_VIEWS: u64 = 4_000;

// CHECKS-ONLY: stages_check.rs
/// What [`run`] found.
#[derive(Debug, Default)]
pub struct AnonFound {
    /// Cases run.
    pub cases: u32,
    /// Comparisons of a space's own objects' lines and resident counts.
    pub compared: u32,
    /// Regions the Wine case's reserve ended as, protections aside.
    pub wine_regions: usize,
    /// Regions of the large case before its whole-space unmap.
    pub large_regions: usize,
}

/// A space of the check's own.
fn space() -> Result<Arc<AddressSpace>, &'static str> {
    AddressSpace::new().map_err(|_| "no address space for the anonymous mapping check")
}

/// `mmap(MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS)` as `sys_mmap` makes it:
/// what is there unmapped, then mapped beside, under the layout. The id of
/// the object the mapping names.
fn fixed(space: &AddressSpace, at: u64, len: u64, flags: VmaFlags) -> Result<u64, &'static str> {
    let layout = space.layout();
    let _ = space.unmap(at, len);
    layout
        .map_anonymous_beside(at, len, flags)
        .map_err(|_| "a fixed anonymous mapping was refused")
}

/// Every own private anonymous object of `space` on one line and its
/// resident counts equal a recount, counted in `found`: the own objects.
fn same(space: &AddressSpace, found: &mut AnonFound) -> Result<usize, &'static str> {
    found.compared += 1;
    let own = space::own_lines_for_check(space)?;
    rss_check::agrees(space).map_err(|_| "a space's kept resident counts differ from a recount")?;
    Ok(own)
}

/// How many regions `space` holds.
fn regions(space: &AddressSpace) -> usize {
    space.with_regions(|regions| regions.count())
}

/// A write fault at `at`, then `byte` written to the page's first byte.
fn write(space: &AddressSpace, at: u64, byte: u8) -> Result<(), &'static str> {
    space
        .fault(at, Access::WRITE)
        .map_err(|_| "a write fault of the anonymous mapping check was refused")?;
    let frame = space::frame_at_for_check(space, at).ok_or("a page written had no frame")?;
    // SAFETY: (FRAME) the frame is the page just faulted in, committed to an
    // object of this check's own space; the direct map covers it whole.
    unsafe { (mm::direct_map(frame * PAGE_SIZE) as *mut u8).write_volatile(byte) };
    Ok(())
}

/// Every page of `len` bytes at `at` reads zero (`L.user.243`): faulted in
/// for reading, and each byte of its frame, if one was committed, zero.
fn reads_zero(space: &AddressSpace, at: u64, len: u64) -> Result<(), &'static str> {
    for page in (at..at + len).step_by(PAGE_SIZE as usize) {
        space
            .fault(page, Access::READ)
            .map_err(|_| "a read fault of the anonymous mapping check was refused")?;
        let Some(frame) = space::frame_at_for_check(space, page) else {
            continue;
        };
        let start = mm::direct_map(frame * PAGE_SIZE);
        for byte in 0..PAGE_SIZE {
            // SAFETY: (FRAME) a frame of this check's own space's object,
            // which the direct map covers whole.
            if unsafe { ((start + byte) as *const u8).read_volatile() } != 0 {
                return Err("a fresh anonymous mapping read a byte that was not zero");
            }
        }
    }
    Ok(())
}

/// No page of `len` bytes at `at` has a frame: a window just mapped
/// `PROT_NONE`, which no fault may read, holds nothing (`L.user.243`).
fn holds_nothing(space: &AddressSpace, at: u64, len: u64) -> Result<(), &'static str> {
    if (at..at + len)
        .step_by(PAGE_SIZE as usize)
        .any(|page| space::frame_at_for_check(space, page).is_some())
    {
        return Err("a window mapped again still held a page");
    }
    Ok(())
}

/// The first byte of the page at `at`.
fn first_byte(space: &AddressSpace, at: u64) -> Result<u8, &'static str> {
    let frame = space::frame_at_for_check(space, at).ok_or("a written page had no frame")?;
    // SAFETY: (FRAME) as `write`'s.
    Ok(unsafe { (mm::direct_map(frame * PAGE_SIZE) as *const u8).read_volatile() })
}

// CHECKS-ONLY: stages_check.rs
/// The core self-check of mappings made beside another (`L.user.242` to
/// `L.user.244`): each case on a space of its own, its own private anonymous
/// objects' lines and its resident counts compared after every step.
///
/// Verifies: L.user.242
/// Verifies: L.user.243
/// Verifies: L.user.244
///
/// # Errors
///
/// What was not as `L.user.242` to `L.user.244` have it.
pub fn run() -> Result<AnonFound, &'static str> {
    let mut found = AnonFound::default();
    wine(&mut found)?;
    heap(&mut found)?;
    right_with_room(&mut found)?;
    over_mapped(&mut found)?;
    declined(&mut found)?;
    windows(&mut found)?;
    after_fork(&mut found)?;
    large(&mut found)?;
    Ok(found)
}

/// Wine's allocator: one reserve, views mapped `MAP_FIXED` into it, parts
/// decommitted and views released the same way. Every mapping takes the
/// reserve's object, released views read zero, and an `mprotect` making the
/// protections equal leaves one region.
fn wine(found: &mut AnonFound) -> Result<(), &'static str> {
    found.cases += 1;
    let space = space()?;
    let len = VIEWS * VIEW;
    let reserve = space
        .map_anonymous(BASE, len, VmaFlags::NONE)
        .map_err(|_| "the reserve was refused")?;
    for view in 0..VIEWS {
        let at = BASE + view * VIEW;
        if fixed(&space, at, VIEW, VmaFlags::READ_WRITE)? != reserve {
            return Err("a view mapped into the reserve did not take the reserve's object");
        }
        if view == 0 {
            // The line, before anything reads through it.
            let _ = same(&space, found)?;
        }
        write(&space, at, 0xA5)?;
    }
    for view in (0..VIEWS).step_by(4) {
        let at = BASE + view * VIEW + PAGE_SIZE;
        if fixed(&space, at, 2 * PAGE_SIZE, VmaFlags::NONE)? != reserve {
            return Err("a part decommitted did not take the reserve's object");
        }
    }
    for view in (0..VIEWS).step_by(3) {
        let at = BASE + view * VIEW;
        if fixed(&space, at, VIEW, VmaFlags::NONE)? != reserve {
            return Err("a view released did not take the reserve's object");
        }
        holds_nothing(&space, at, VIEW)?;
    }
    if same(&space, found)? != 1 {
        return Err("Wine's pattern left more than the reserve's object");
    }
    found.wine_regions = regions(&space);
    space
        .protect(BASE, len, VmaFlags::NONE)
        .map_err(|_| "an mprotect over the reserve was refused")?;
    if regions(&space) != 1 {
        return Err("the reserve under one protection is not one region");
    }
    same(&space, found).map(|_| ())
}

/// `brk`'s pattern: each step mapped at the end of the last, all one region
/// and one object.
fn heap(found: &mut AnonFound) -> Result<(), &'static str> {
    found.cases += 1;
    let space = space()?;
    let first = fixed(&space, BASE, VIEW, VmaFlags::READ_WRITE)?;
    for step in 1..16 {
        if fixed(&space, BASE + step * VIEW, VIEW, VmaFlags::READ_WRITE)? != first {
            return Err("a heap step did not take the heap's object");
        }
    }
    if regions(&space) != 1 || same(&space, found)? != 1 {
        return Err("sixteen heap steps are not one region and one object");
    }
    Ok(())
}

/// A right neighbour whose offset leaves room below it: taken.
fn right_with_room(found: &mut AnonFound) -> Result<(), &'static str> {
    found.cases += 1;
    let space = space()?;
    let id = space
        .map_anonymous(BASE, 2 * VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    space
        .unmap(BASE, VIEW)
        .map_err(|_| "an unmap of a front was refused")?;
    if fixed(&space, BASE, VIEW, VmaFlags::READ_WRITE)? != id {
        return Err("a mapping below a neighbour with room did not take its object");
    }
    if regions(&space) != 1 || same(&space, found)? != 1 {
        return Err("a front mapped again is not one region and one object");
    }
    Ok(())
}

/// A mapping asked over a range already mapped, as `brk` asks (it unmaps
/// nothing first): refused, taking nothing (ledger 9307 B1). The heap's
/// object is extended by a fixed mapping, whose page is written and split
/// off by an `mprotect`; a heap step over it must leave that page its own.
fn over_mapped(found: &mut AnonFound) -> Result<(), &'static str> {
    found.cases += 1;
    let space = space()?;
    let heap = fixed(&space, BASE, 2 * VIEW, VmaFlags::READ_WRITE)?;
    if fixed(&space, BASE + 2 * VIEW, VIEW, VmaFlags::READ_WRITE)? != heap {
        return Err("a mapping at the heap's end did not take its object");
    }
    write(&space, BASE + 2 * VIEW, 0x99)?;
    space
        .protect(BASE + 2 * VIEW, VIEW, VmaFlags::READ)
        .map_err(|_| "an mprotect split was refused")?;
    if space
        .layout()
        .map_anonymous_beside(BASE + 2 * VIEW, 2 * VIEW, VmaFlags::READ_WRITE)
        .is_ok()
    {
        return Err("a mapping over a mapped range was made");
    }
    if space::frame_at_for_check(&space, BASE + 2 * VIEW).is_none()
        || first_byte(&space, BASE + 2 * VIEW)? != 0x99
    {
        return Err("a mapping refused over a mapped range took its pages");
    }
    same(&space, found).map(|_| ())
}

/// Each neighbour that must not be joined, and the space and mapping that
/// must not join: each makes a new object.
fn declined(found: &mut AnonFound) -> Result<(), &'static str> {
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    // A right neighbour at offset 0: no room below it.
    found.cases += 1;
    let space = space()?;
    let right = space
        .map_anonymous(BASE + VIEW, VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    if fixed(&space, BASE, VIEW, VmaFlags::READ_WRITE)? == right {
        return Err("a mapping below a neighbour at offset 0 took its object");
    }
    let _ = same(&space, found)?;
    // A shared neighbour, and a shared mapping beside a private one.
    found.cases += 1;
    let space = self::space()?;
    let both = space
        .map_anonymous(BASE, VIEW, shared)
        .map_err(|_| "a shared mapping was refused")?;
    if fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)? == both {
        return Err("a private mapping beside a shared one took its object");
    }
    let private = space
        .map_anonymous(BASE + 4 * VIEW, VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    // The lines first: a shared mapping in a private object is off them.
    let got = fixed(&space, BASE + 5 * VIEW, VIEW, shared)?;
    let _ = same(&space, found)?;
    if got == private {
        return Err("a shared mapping beside a private one took its object");
    }
    // A neighbour mapped as an object, by handle: its id is the handle's.
    found.cases += 1;
    let space = self::space()?;
    let object = Vmo::new_anonymous(VIEW / PAGE_SIZE).map_err(|_| "no object to map")?;
    let _ = space
        .map_object(Some(BASE), VIEW, object, 0, VmaFlags::READ_WRITE)
        .map_err(|_| "an object's mapping was refused")?;
    let before = same(&space, found)?;
    let _ = fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)?;
    if same(&space, found)? != before + 1 || regions(&space) != 2 {
        return Err("a mapping beside an object mapped by handle took its object");
    }
    // Private memory allocated by handle (`space_allocate`): its region is
    // private, so only the handle test keeps a mapping out of it.
    found.cases += 1;
    let space = self::space()?;
    let _ = space
        .allocate_by_handle(Some(BASE), VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a private allocation by handle was refused")?;
    let before = same(&space, found)?;
    let _ = fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)?;
    if same(&space, found)? != before + 1 || regions(&space) != 2 {
        return Err("a mapping beside private memory allocated by handle took its object");
    }
    // Kept apart, as exec keeps its stack and the stack's guards.
    found.cases += 1;
    let space = self::space()?;
    let stack = space
        .map_anonymous(BASE, VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    space.keep_apart(stack);
    if fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)? == stack {
        return Err("a mapping beside a region kept apart took its object");
    }
    let _ = same(&space, found)?;
    // A space being emptied.
    found.cases += 1;
    let space = self::space()?;
    let left = space
        .map_anonymous(BASE, VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    space.mark_emptying();
    if fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)? == left {
        return Err("a mapping in a space being emptied took its neighbour's object");
    }
    let _ = same(&space, found)?;
    Ok(())
}

/// The windows a mapping is made in read zero (`L.user.243`): one whose
/// page an unmap kept for a second reference (taken back), one whose page
/// is held (declined), one written and unmapped, and one an `mremap` moved
/// out of.
fn windows(found: &mut AnonFound) -> Result<(), &'static str> {
    // Kept: a second reference across the unmap, as a `madvise` holds one.
    found.cases += 1;
    let space = space()?;
    let id = space
        .map_anonymous(BASE, 2 * VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    write(&space, BASE + VIEW, 0x5A)?;
    let object = space
        .object(id)
        .ok_or("a mapping's object was not in its table")?;
    space
        .unmap(BASE + VIEW, VIEW)
        .map_err(|_| "an unmap of a tail was refused")?;
    if object.page(VIEW / PAGE_SIZE).is_none() {
        return Err("the kept-page window kept no page across its unmap");
    }
    drop(object);
    if fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)? != id {
        return Err("a window with a page kept, not held, was not taken");
    }
    reads_zero(&space, BASE + VIEW, VIEW)?;
    let _ = same(&space, found)?;

    // Held: declined, and the new object's pages read zero.
    found.cases += 1;
    let space = self::space()?;
    let id = space
        .map_anonymous(BASE, 2 * VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    write(&space, BASE + VIEW, 0xC3)?;
    let object = space
        .object(id)
        .ok_or("a mapping's object was not in its table")?;
    let held = object
        .hold(VIEW / PAGE_SIZE, 1)
        .map_err(|_| "a page could not be held")?;
    space
        .unmap(BASE + VIEW, VIEW)
        .map_err(|_| "an unmap of a tail was refused")?;
    // Read first: a window taken with its held page reads that page.
    let got = fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)?;
    reads_zero(&space, BASE + VIEW, VIEW)?;
    if got == id {
        return Err("a window with a held page was taken");
    }
    drop(held);
    drop(object);
    let _ = same(&space, found)?;

    // Written, unmapped, mapped again.
    found.cases += 1;
    let space = self::space()?;
    let id = space
        .map_anonymous(BASE, 2 * VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    write(&space, BASE + VIEW, 0x3C)?;
    space
        .unmap(BASE + VIEW, VIEW)
        .map_err(|_| "an unmap of a tail was refused")?;
    if fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)? != id {
        return Err("a window written and unmapped was not taken");
    }
    reads_zero(&space, BASE + VIEW, VIEW)?;
    let _ = same(&space, found)?;

    // Moved out by `mremap`, its window mapped again.
    found.cases += 1;
    let space = self::space()?;
    let id = space
        .map_anonymous(BASE, 2 * VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    write(&space, BASE + VIEW, 0x77)?;
    let _ = space
        .remap(BASE + VIEW, VIEW, VIEW, Destination::Fixed(BASE + 8 * VIEW))
        .map_err(|_| "an mremap move was refused")?;
    if first_byte(&space, BASE + 8 * VIEW)? != 0x77 {
        return Err("a page moved by mremap lost its bytes");
    }
    if fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)? != id {
        return Err("a window an mremap moved out of was not taken");
    }
    reads_zero(&space, BASE + VIEW, VIEW)?;
    let _ = same(&space, found)?;
    Ok(())
}

/// A mapping beside a region `fork` made copy-on-write: taken, and the
/// parent's write there and the child's pages each its own.
fn after_fork(found: &mut AnonFound) -> Result<(), &'static str> {
    found.cases += 1;
    let space = space()?;
    let id = space
        .map_anonymous(BASE, VIEW, VmaFlags::READ_WRITE)
        .map_err(|_| "a mapping was refused")?;
    write(&space, BASE, 0x11)?;
    let child = space
        .fork()
        .map_err(|_| "the anonymous mapping check's space would not fork")?;
    if fixed(&space, BASE + VIEW, VIEW, VmaFlags::READ_WRITE)? != id {
        return Err("a mapping beside a copy-on-write region did not take its object");
    }
    write(&space, BASE + VIEW, 0x22)?;
    write(&space, BASE, 0x33)?;
    if first_byte(&child, BASE)? != 0x11 || space::frame_at_for_check(&child, BASE + VIEW).is_some()
    {
        return Err("a parent's writes beside a fork reached its child");
    }
    if first_byte(&space, BASE + VIEW)? != 0x22 {
        return Err("a page written beside a fork lost its byte");
    }
    let _ = same(&space, found)?;
    let _ = same(&child, found)?;
    Ok(())
}

/// [`LARGE_VIEWS`] views in one reserve, every second one writable and
/// touched: one object, counted out by a whole-space unmap with the
/// resident counts equal to a recount after.
fn large(found: &mut AnonFound) -> Result<(), &'static str> {
    found.cases += 1;
    let space = space()?;
    let piece = 2 * PAGE_SIZE;
    let len = LARGE_VIEWS * piece;
    let reserve = space
        .map_anonymous(BASE, len, VmaFlags::NONE)
        .map_err(|_| "the large reserve was refused")?;
    for view in (1..LARGE_VIEWS).step_by(2) {
        let at = BASE + view * piece;
        if fixed(&space, at, piece, VmaFlags::READ_WRITE)? != reserve {
            return Err("a large reserve's view did not take its object");
        }
        write(&space, at, 1)?;
    }
    if same(&space, found)? != 1 {
        return Err("the large reserve is more than one object");
    }
    found.large_regions = regions(&space);
    space
        .unmap(BASE, len)
        .map_err(|_| "the large reserve's unmap was refused")?;
    if regions(&space) != 0 {
        return Err("the large reserve's unmap left a region");
    }
    let _ = same(&space, found)?;
    Ok(())
}
