//! Stage 6's edges: what an address space and its objects refuse, and the
//! rarer ways they succeed, called by name (finding F-10).
//!
//! The system calls reach most of this through their own argument checks,
//! which refuse a malformed request before the space sees it -- so the
//! space's own refusals, which are what stands between a caller that forgot a
//! check and a mapping of the kernel, ran on no boot. Each is asked for here,
//! directly, with exactly the error it has to answer; and the paths no program
//! in the suite takes -- a window a GPU's blob would be mapped through, an
//! `mremap` that grows where it is or over pages another region names, a page
//! a device holds refusing to move, a page another object shares copied
//! before a write -- are taken, with what they must leave behind.

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END};
use ferrix_vma::VmaFlags;

use crate::mm;
use crate::smp;
use crate::user::space::{
    Access, AddressSpace, Destination, FileMapping, FilePlace, SpaceError, WindowPages,
};
use crate::user::vmo::{Vmo, VmoError};
use crate::vmap::{self, VmapError};

/// Where these checks map.
const LOW: u64 = 0x2000_0000;

/// A page.
const P: u64 = PAGE_SIZE;

/// A shared, writable region.
const SHARED: VmaFlags = VmaFlags {
    shared: true,
    ..VmaFlags::READ_WRITE
};

/// Run them; answers how many refusals were asked for and got.
pub(crate) fn run() -> Result<u32, &'static str> {
    let space = AddressSpace::new().map_err(|_| "no memory for the edge checks' space")?;
    let mut refused = check_device_refusals(&space)?;
    refused += check_mapping_refusals(&space)?;
    refused += check_a_window_keeps_its_keeper(&space)?;
    refused += check_remap_refusals(&space)?;
    check_a_region_grows_where_it_is(&space)?;
    refused += check_a_shared_region_does_not_grow_over_itself(&space)?;
    refused += check_a_held_page_does_not_move(&space)?;
    check_a_shared_page_is_copied_before_a_write()?;
    refused += check_object_refusals(&space)?;
    check_frame_runs_and_splits()?;
    check_a_window_settles_while_frames_move()?;
    refused += check_kernel_arena_refusals()?;
    Ok(refused)
}

/// `value`, hidden from the optimiser: a refusal of a constant argument is
/// otherwise decided where the call is inlined, and the refusing statement
/// itself never runs.
fn opaque(value: u64) -> u64 {
    core::hint::black_box(value)
}

/// Require `result` to be exactly `wanted`.
fn expect<T>(
    result: Result<T, SpaceError>,
    wanted: SpaceError,
    what: &'static str,
) -> Result<u32, &'static str> {
    if result.err() == Some(wanted) {
        Ok(1)
    } else {
        Err(what)
    }
}

/// `map_device` refuses a length that is not whole pages and a range that
/// leaves the user half.
///
/// Verifies: L.user.49
fn check_device_refusals(space: &AddressSpace) -> Result<u32, &'static str> {
    let mut refused = expect(
        space.map_device(Some(LOW), opaque(0), P, VmaFlags::READ_WRITE, false),
        SpaceError::BadRange,
        "a device mapping of no length was taken",
    )?;
    refused += expect(
        space.map_device(Some(LOW), opaque(P / 2), P, VmaFlags::READ_WRITE, false),
        SpaceError::BadRange,
        "a device mapping of half a page was taken",
    )?;
    refused += expect(
        space.map_device(
            Some(opaque(USER_VIRT_END - P)),
            opaque(2 * P),
            P,
            VmaFlags::READ_WRITE,
            false,
        ),
        SpaceError::NotUserRange(USER_VIRT_END - P),
        "a device mapping reaching past the user half was taken",
    )?;
    Ok(refused)
}

/// `map_anywhere`, `map_object`, `map_file` and `protect` each refuse what
/// is theirs to refuse.
///
/// Verifies: L.user.50
fn check_mapping_refusals(space: &AddressSpace) -> Result<u32, &'static str> {
    let mut refused = expect(
        space.map_anywhere(None, opaque(0), VmaFlags::READ_WRITE),
        SpaceError::BadRange,
        "an anonymous mapping of no length was taken",
    )?;
    let vmo = Vmo::new_anonymous(2).map_err(|_| "no memory for a VMO")?;
    refused += expect(
        space.map_object(Some(LOW), P, Arc::clone(&vmo), 0, VmaFlags::READ_EXECUTE),
        SpaceError::Refused(LOW),
        "an object was mapped executable",
    )?;
    refused += expect(
        space.map_object(
            Some(opaque(USER_VIRT_END - P)),
            opaque(2 * P),
            Arc::clone(&vmo),
            0,
            VmaFlags::READ_WRITE,
        ),
        SpaceError::NotUserRange(USER_VIRT_END - P),
        "an object was mapped past the user half",
    )?;
    let opened: Arc<dyn Any + Send + Sync> = Arc::new(());
    refused += expect(
        space.map_file(
            FilePlace::Fixed(LOW),
            P,
            VmaFlags::READ,
            Arc::clone(&vmo),
            opaque(P / 2),
            FileMapping {
                file: opened,
                may_write: false,
            },
        ),
        SpaceError::BadRange,
        "a file was mapped from an offset that is not whole pages",
    )?;
    refused += expect(
        space.protect(opaque(USER_VIRT_END - P), opaque(2 * P), VmaFlags::READ),
        SpaceError::NotUserRange(USER_VIRT_END - P),
        "a protection change reaching past the user half was taken",
    )?;
    Ok(refused)
}

/// A window over a frame is refused executable, of a length that is not
/// whole pages, or past the user half; mapped, it keeps what it was given to
/// keep until nothing maps it, and is the highest thing mapped.
fn check_a_window_keeps_its_keeper(space: &AddressSpace) -> Result<u32, &'static str> {
    let frame = mm::allocate_frames(0).ok_or("no frame for the window check")?;
    let checked = window_over(space, frame * PAGE_SIZE);
    mm::deallocate_frames(frame, 0);
    checked
}

/// [`check_a_window_keeps_its_keeper`] over the frame at `physical`.
///
/// Verifies: L.user.66, L.user.67
fn window_over(space: &AddressSpace, physical: u64) -> Result<u32, &'static str> {
    let keeper: Arc<dyn Any + Send + Sync> = Arc::new(7_u8);
    let window = |place, len, flags| {
        space.map_window(
            place,
            opaque(len),
            WindowPages::all(physical, opaque(len)),
            flags,
            true,
            Arc::clone(&keeper),
        )
    };
    let mut refused = expect(
        window(FilePlace::Fixed(LOW), P, VmaFlags::READ_EXECUTE),
        SpaceError::Refused(0),
        "a window was mapped executable",
    )?;
    refused += expect(
        window(FilePlace::Fixed(LOW), P / 2, VmaFlags::READ_WRITE),
        SpaceError::BadRange,
        "a window of half a page was mapped",
    )?;
    refused += expect(
        window(
            FilePlace::Fixed(USER_VIRT_END - P),
            2 * P,
            VmaFlags::READ_WRITE,
        ),
        SpaceError::NotUserRange(USER_VIRT_END - P),
        "a window reaching past the user half was mapped",
    )?;
    let at = window(FilePlace::Anywhere(None), P, VmaFlags::READ_WRITE)
        .map_err(|_| "a window over a frame was refused")?;
    if Arc::strong_count(&keeper) != 2 {
        return Err("a mapped window did not keep what it was given to keep");
    }
    if space.highest_mapped() != Some(at + P) {
        return Err("the highest mapping was not the window, the only thing mapped");
    }
    // A file mapping beside it, unmapped while the window stays: what the
    // space still names is asked of the window's region too.
    let file = Vmo::new_anonymous(1).map_err(|_| "no memory for a file beside the window")?;
    let opened: Arc<dyn Any + Send + Sync> = Arc::new(());
    let beside = space
        .map_file(
            FilePlace::Fixed(LOW),
            P,
            VmaFlags::READ,
            file,
            0,
            FileMapping {
                file: Arc::clone(&opened),
                may_write: false,
            },
        )
        .map_err(|_| "a file beside the window was refused")?;
    space
        .unmap(beside, P)
        .map_err(|_| "unmapping the file beside the window was refused")?;
    if Arc::strong_count(&opened) != 1 || Arc::strong_count(&keeper) != 2 {
        return Err("unmapping a file beside a window let go of the wrong one");
    }
    space
        .unmap(at, P)
        .map_err(|_| "unmapping a window was refused")?;
    if Arc::strong_count(&keeper) != 1 {
        return Err("an unmapped window still kept what it was given");
    }
    Ok(refused)
}

/// `mremap` refuses a length of zero, an old range longer than its region,
/// and a fixed destination overlapping the old range; and a remap to the
/// same length stays where it is.
///
/// Verifies: L.user.69
fn check_remap_refusals(space: &AddressSpace) -> Result<u32, &'static str> {
    let _ = space
        .map_anonymous(LOW, 2 * P, VmaFlags::READ_WRITE)
        .map_err(|_| "no room for the remap refusals' region")?;
    let mut refused = expect(
        space.remap(LOW, 2 * P, opaque(0), Destination::Anywhere),
        SpaceError::BadRange,
        "a remap to no length was taken",
    )?;
    refused += expect(
        space.remap(LOW, opaque(3 * P), 4 * P, Destination::Anywhere),
        SpaceError::NotMapped(LOW),
        "a remap of more than its region was taken",
    )?;
    refused += expect(
        space.remap(LOW, 2 * P, 2 * P, Destination::Fixed(opaque(LOW + P))),
        SpaceError::BadRange,
        "a remap onto its own old range was taken",
    )?;
    if space.remap(LOW, 2 * P, 2 * P, Destination::Anywhere) != Ok(LOW) {
        return Err("a remap to the same length moved");
    }
    space
        .unmap(LOW, 2 * P)
        .map_err(|_| "unmapping the remap refusals' region was refused")?;
    Ok(refused)
}

/// A region with room after it grows there, even when it may move.
///
/// Verifies: L.user.70
fn check_a_region_grows_where_it_is(space: &AddressSpace) -> Result<(), &'static str> {
    let _ = space
        .map_anonymous(LOW, P, VmaFlags::READ_WRITE)
        .map_err(|_| "no room for the growing region")?;
    let grown = space
        .remap(LOW, P, 3 * P, Destination::Anywhere)
        .map_err(|_| "a region with room after it could not grow")?;
    space
        .unmap(grown, 3 * P)
        .map_err(|_| "unmapping the grown region was refused")?;
    if grown != LOW {
        return Err("a region with room after it moved rather than grow");
    }
    Ok(())
}

/// A shared region may not grow over offsets of its object that another of
/// its regions already names: an unmap of either would give back pages the
/// other still shows.
///
/// Verifies: L.user.71
fn check_a_shared_region_does_not_grow_over_itself(
    space: &AddressSpace,
) -> Result<u32, &'static str> {
    let _ = space
        .map_anonymous(LOW, 3 * P, SHARED)
        .map_err(|_| "no room for the shared region")?;
    let apart = space
        .remap(LOW + 2 * P, P, P, Destination::Fixed(LOW + 8 * P))
        .map_err(|_| "moving a shared region's last page was refused")?;
    let refused = expect(
        space.remap(LOW, 2 * P, 3 * P, Destination::InPlace),
        SpaceError::OutOfMemory,
        "a shared region grew over pages its own moved page names",
    )?;
    space
        .unmap(LOW, 2 * P)
        .and_then(|()| space.unmap(apart, P))
        .map_err(|_| "unmapping the shared regions was refused")?;
    Ok(refused)
}

/// A private region holding a page a device holds cannot move: the device
/// would keep the old frame at an address the program had left.
///
/// Verifies: L.user.29, L.user.72
fn check_a_held_page_does_not_move(space: &AddressSpace) -> Result<u32, &'static str> {
    let id = space
        .map_anonymous(LOW, P, VmaFlags::READ_WRITE)
        .map_err(|_| "no room for the held region")?;
    let _ = space
        .map_anonymous(LOW + P, P, VmaFlags::READ_WRITE)
        .map_err(|_| "no room for the held region's neighbour")?;
    space
        .fault(LOW, Access::WRITE)
        .map_err(|_| "could not touch the held region")?;
    let vmo = space.object(id).ok_or("the held region has no object")?;
    let held = vmo
        .hold(0, 1)
        .map_err(|_| "could not hold the region's page")?;
    let refused = expect(
        space.remap(LOW, P, 2 * P, Destination::Anywhere),
        SpaceError::OutOfMemory,
        "a region holding a page a device holds moved",
    )?;
    drop(held);
    space
        .unmap(LOW, 2 * P)
        .map_err(|_| "unmapping the held region was refused")?;
    Ok(refused)
}

/// A page a fork of the object shares is copied before a write, and the
/// fork keeps what it had; a copy put in place of a page an object does not
/// have is simply its page.
///
/// Verifies: L.user.12, L.user.15
fn check_a_shared_page_is_copied_before_a_write() -> Result<(), &'static str> {
    let vmo = Vmo::new_anonymous(1).map_err(|_| "no memory for a VMO")?;
    vmo.write_page(0, 0, b"before")
        .map_err(|_| "could not write a fresh page")?;
    let fork = vmo.fork().map_err(|_| "could not fork an object")?;
    let shared = vmo.page(0);
    if shared.is_none() || shared != fork.page(0) {
        return Err("a fork of an object did not share its page");
    }
    vmo.write_page(0, 0, b"after!")
        .map_err(|_| "could not write a shared page")?;
    if vmo.page(0) == shared {
        return Err("a write to a page a fork shares was not copied first");
    }
    let (mut mine, mut theirs) = ([0_u8; 6], [0_u8; 6]);
    vmo.read_page(0, 0, &mut mine)
        .and_then(|()| fork.read_page(0, 0, &mut theirs))
        .map_err(|_| "could not read the written page back")?;
    if &mine != b"after!" || &theirs != b"before" {
        return Err("a write to a shared page reached the object sharing it");
    }
    // A copy put in place of a page the object does not have displaces
    // nothing, and the object has the copy from then on.
    let sparse = Vmo::new_anonymous(2).map_err(|_| "no memory for a VMO")?;
    let copy = mm::allocate_frames(0).ok_or("no frame for a replace")?;
    if sparse.replace(1, copy).is_some() || sparse.page(1) != Some(copy) {
        return Err("a copy put in place of an absent page displaced one, or went nowhere");
    }
    Ok(())
}

/// An object refuses a page past its end, and becomes coherent only while
/// nothing maps it.
///
/// Verifies: L.user.6, L.user.36, L.user.64
fn check_object_refusals(space: &AddressSpace) -> Result<u32, &'static str> {
    let vmo = Vmo::new_anonymous(1).map_err(|_| "no memory for a VMO")?;
    if vmo.commit_within(1) != Err(VmoError::OutOfRange { index: 1, pages: 1 }) {
        return Err("a page past an object's end was committed");
    }
    let at = space
        .map_object(None, P, Arc::clone(&vmo), 0, SHARED)
        .map_err(|_| "could not map an object")?;
    if vmo.make_coherent() {
        return Err("a mapped object was made coherent");
    }
    space
        .unmap(at, P)
        .map_err(|_| "could not unmap an object")?;
    if !vmo.make_coherent() || !vmo.is_coherent() {
        return Err("an object nothing maps could not be made coherent");
    }
    // A read-only region refuses a write through the kernel's own lookup.
    let _ = space
        .map_anonymous(LOW, P, VmaFlags::READ)
        .map_err(|_| "no room for a read-only region")?;
    let refused = expect(
        space.with_present_page(LOW, Access::WRITE, |_| ()),
        SpaceError::Refused(LOW),
        "a write was looked up in a read-only region",
    )?;
    space
        .unmap(LOW, P)
        .map_err(|_| "could not unmap the read-only region")?;
    Ok(refused + 2)
}

/// A run of whole blocks back to back, each given back on its own; and a
/// block split into single frames, each its own to give back.
///
/// Verifies: L.mm.5, L.mm.6
fn check_frame_runs_and_splits() -> Result<(), &'static str> {
    use ferrix_frame::State;
    let block = 1_u64 << ferrix_frame::MAX_ORDER;
    let run = mm::allocate_frame_run(2).ok_or("no run of two blocks")?;
    let heads = [run, run + block];
    if heads
        .iter()
        .any(|&head| mm::frame_state(head) != Some(State::Allocated))
    {
        return Err("a run of two blocks was not two allocated blocks");
    }
    for head in heads {
        mm::deallocate_frames(head, ferrix_frame::MAX_ORDER);
    }

    let pair = mm::allocate_frames(1).ok_or("no block of two frames")?;
    if !mm::split_frames(pair, 1) {
        return Err("a block of two frames would not split");
    }
    for frame in [pair, pair + 1] {
        if mm::frame_state(frame) != Some(State::Allocated) || !mm::release_frame(frame) {
            return Err("a frame split from a block was not its own to give back");
        }
    }
    Ok(())
}

/// Set while [`churn_frames`] is to keep going.
static CHURNING: AtomicBool = AtomicBool::new(false);
/// Frames [`churn_frames`] has taken and given back.
static CHURNED: AtomicU64 = AtomicU64::new(0);

/// Take a frame and give it back, over and over, until told to stop.
fn churn_frames(_: usize) {
    while CHURNING.load(Ordering::Acquire) {
        if let Some(frame) = mm::allocate_frames(0) {
            mm::deallocate_frames(frame, 0);
            let _ = CHURNED.fetch_add(1, Ordering::Relaxed);
        }
        core::hint::spin_loop();
    }
}

/// A frame window opened while another processor takes frames and gives
/// them back still opens: it reads the count until two reads agree, sleeping
/// between them once quick re-reads have not, rather than taking the first
/// read of a count that is moving.
fn check_a_window_settles_while_frames_move() -> Result<(), &'static str> {
    const PATIENCE_NANOS: u64 = 10_000_000_000;
    let cpus = smp::count();
    if cpus < 2 {
        return Ok(());
    }
    let here = smp::this_cpu()
        .ok_or("no processor to open the frame windows on")?
        .logical;
    let there = (here + 1) % cpus;
    CHURNED.store(0, Ordering::Relaxed);
    CHURNING.store(true, Ordering::Release);
    let churner = crate::sched::spawn_on(
        "frame churn",
        churn_frames,
        0,
        ferrix_sched::NICE_0_WEIGHT,
        there,
        ferrix_sched::CpuSet::of(there),
    )
    .inspect_err(|_| CHURNING.store(false, Ordering::Release))?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let mut started = true;
    while CHURNED.load(Ordering::Relaxed) == 0 {
        if crate::timer::now_nanos() >= deadline {
            started = false;
            break;
        }
        crate::sched::sleep_for(1_000_000);
    }
    for _ in 0..32 {
        let _window = mm::FrameWindow::open();
    }
    CHURNING.store(false, Ordering::Release);
    while !churner.is_dead() {
        if crate::timer::now_nanos() >= deadline {
            return Err("the frame churner never stopped");
        }
        crate::sched::sleep_for(1_000_000);
    }
    crate::sched::wait_until_gone(&churner, crate::sched::REAPER_PATIENCE_NANOS)?;
    if started {
        Ok(())
    } else {
        Err("the frame churner never ran")
    }
}

/// The kernel arena refuses a length of nothing.
///
/// Verifies: L.mm.47
fn check_kernel_arena_refusals() -> Result<u32, &'static str> {
    match vmap::allocate(0, ferrix_paging::MapFlags::KERNEL_DATA) {
        Err(VmapError::BadLength(0)) => Ok(1),
        _ => Err("the kernel arena took a length of nothing"),
    }
}
