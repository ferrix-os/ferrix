//! Every allocation the memory layer's operations make, failed in turn
//! (findings F-10 and F-23).
//!
//! `object/alloc_check.rs` fails every `n`th allocation of a round of native
//! calls, for a few `n`: a sample, which lands where the round's order puts
//! it. This is the other half. It takes the address space's and the objects'
//! operations -- a fork of a space holding every kind of region, an `mremap`
//! that moves a private region and one that grows and shrinks a shared one,
//! an unmap that splits, a VMO shared, written, held and forked, kernel stacks
//! freed together, an object mapped as shared code -- and runs each once per
//! allocation it makes, failing that
//! allocation alone: the first run fails the first, the second the second,
//! until a run in which nothing failed. So every allocation site the scenario
//! reaches is made to fail once, whatever order it comes in.
//!
//! Every run must end in success or in running out of memory -- never another
//! refusal, never a panic -- and the run in which nothing failed must succeed
//! whole. Each scenario builds what it needs from nothing and drops all of it,
//! so a failure part-way that kept a frame shows as a frame the sweep did not
//! give back, counted over every run of every scenario.

use alloc::sync::Arc;
use core::any::Any;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vma::VmaFlags;

use crate::fallible::{self, AllocError};
use crate::mm;
use crate::sched::TaskId;
use crate::user::space::{
    Access, AddressSpace, Destination, FileMapping, FilePlace, SpaceError, WindowPages,
};
use crate::user::vmo::{Vmo, VmoError};
use crate::vmap::{self, VmapError};

/// Where the scenarios map: low in the user half, clear of anything a
/// fresh space holds.
const LOW: u64 = 0x1000_0000;

/// A page.
const P: u64 = PAGE_SIZE;

/// More allocations than any scenario makes: a sweep that reaches it is one
/// whose scenario grew, or one that never stops allocating.
const MOST_RUNS: u32 = 400;

/// How a scenario ended when it did not succeed.
#[derive(Debug, Clone, Copy)]
enum Refusal {
    /// Out of memory, in whichever words the step that ran out uses.
    NoMemory,
    /// Anything else: what the step was.
    Wrong(&'static str),
}

impl From<AllocError> for Refusal {
    fn from(_: AllocError) -> Refusal {
        Refusal::NoMemory
    }
}

/// `result`, with running out of memory told apart from the rest.
fn space<T>(result: Result<T, SpaceError>, what: &'static str) -> Result<T, Refusal> {
    result.map_err(|error| match error {
        SpaceError::OutOfMemory | SpaceError::Backing(VmoError::OutOfMemory) => Refusal::NoMemory,
        _ => {
            crate::console::println!("  sweep    {what}: {error:?}");
            Refusal::Wrong(what)
        }
    })
}

/// As [`space`], for an object's own refusals.
fn object<T>(result: Result<T, VmoError>, what: &'static str) -> Result<T, Refusal> {
    result.map_err(|error| match error {
        VmoError::OutOfMemory => Refusal::NoMemory,
        VmoError::OutOfRange { .. } => Refusal::Wrong(what),
    })
}

/// As [`space`], for the kernel arena's: a page table the arena could not
/// have for the mapping is running out of memory too.
fn arena<T>(result: Result<T, VmapError>, what: &'static str) -> Result<T, Refusal> {
    result.map_err(|error| match error {
        VmapError::OutOfMemory | VmapError::MapFailed(ferrix_paging::MapError::OutOfMemory) => {
            Refusal::NoMemory
        }
        _ => {
            crate::console::println!("  sweep    {what}: {error:?}");
            Refusal::Wrong(what)
        }
    })
}

/// What the sweeps came to, for the boot line.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Scenarios swept.
    pub(crate) scenarios: u32,
    /// Runs in which an allocation was failed: one per allocation site met.
    pub(crate) failed: u32,
    /// Of those, runs that succeeded all the same, the failure absorbed.
    pub(crate) absorbed: u32,
}

/// A scenario: a name for the console, and the thing itself.
type Scenario = (&'static str, fn(u64) -> Result<(), Refusal>);

/// Sweep every scenario, and require every frame back at the end.
///
/// `frame` is a frame of RAM the caller owns, for the scenario that maps a
/// window over one.
///
/// Verifies: H.MEM.11, L.mm.48, L.user.20, L.user.100, L.user.106
pub(crate) fn run(frame: u64) -> Result<Report, &'static str> {
    let scenarios: [Scenario; 9] = [
        ("fork", fork_every_kind_of_region),
        ("move", remap_a_private_region),
        ("shared", remap_a_shared_region),
        ("split", unmap_from_the_middle),
        ("cut", cut_a_file_under_its_mappings),
        ("object", share_write_and_hold_an_object),
        ("stacks", free_stacks_together),
        ("code", map_shared_code),
        ("page", map_code_page),
    ];
    let task = crate::sched::current_id().ok_or("the sweep runs outside a task")?;
    // Once each before the window, for the size classes and the tables their
    // first use grows, which stay.
    for (name, scenario) in scenarios {
        if let Err(refusal) = scenario(frame) {
            return Err(unexpected(name, refusal));
        }
    }
    let mut report = Report::default();
    for (name, scenario) in scenarios {
        crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
        let window = mm::FrameWindow::open();
        sweep(task, name, || scenario(frame), &mut report)?;
        crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
        let kept = window.kept();
        if kept != 0 {
            mm::print_frame_delta(name, kept);
            window.report(name);
            return Err("failing each allocation of a memory operation in turn kept frames");
        }
        report.scenarios += 1;
    }
    Ok(report)
}

/// Say which scenario refused, and how, and answer the error.
fn unexpected(name: &str, refusal: Refusal) -> &'static str {
    crate::console::println!("  sweep    {name}: {refusal:?}");
    match refusal {
        Refusal::NoMemory => "a memory operation ran out of memory with nothing failing",
        Refusal::Wrong(what) => what,
    }
}

/// Run `scenario` once per allocation it makes, the `n`th run failing the
/// `n`th allocation alone, until a run in which none was failed.
fn sweep(
    task: TaskId,
    name: &str,
    mut scenario: impl FnMut() -> Result<(), Refusal>,
    report: &mut Report,
) -> Result<(), &'static str> {
    for nth in 1..=MOST_RUNS {
        // Every run from the same heap, whatever the processors' caches held
        // after the last (docs/HEAP-CACHES.md, condition 2).
        let _ = mm::drain_caches();
        fallible::inject_once(task, nth, true);
        let outcome = scenario();
        let failed = fallible::stop_injecting() > 0;
        match outcome {
            Ok(()) if failed => report.absorbed += 1,
            Ok(()) => return Ok(()),
            Err(Refusal::NoMemory) if failed => {}
            Err(refusal) => return Err(unexpected(name, refusal)),
        }
        report.failed += 1;
    }
    Err("a memory operation made more allocations than the sweep allows")
}

/// A space holding a private region, a shared one, a native object, a
/// private file mapping written through and a window, forked; the child
/// and the parent each then write the private page, and the child the file
/// page.
fn fork_every_kind_of_region(frame: u64) -> Result<(), Refusal> {
    let parent = space(AddressSpace::new(), "a fresh space was refused")?;
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    let _ = space(
        parent.map_anonymous(LOW, 2 * P, VmaFlags::READ_WRITE),
        "a private region was refused",
    )?;
    let _ = space(
        parent.map_anonymous(LOW + 4 * P, P, shared),
        "a shared region was refused",
    )?;
    let native = Vmo::new_anonymous(1)?;
    let _ = space(
        parent.map_object(Some(LOW + 8 * P), P, native, 0, VmaFlags::READ_WRITE),
        "an object's mapping was refused",
    )?;
    let file = Vmo::new_anonymous(2)?;
    let _ = object(file.commit(0), "a file's page was refused")?;
    let opened: Arc<dyn Any + Send + Sync> = fallible::try_arc(0_u8)?;
    let _ = space(
        parent.map_file(
            FilePlace::Fixed(LOW + 12 * P),
            2 * P,
            VmaFlags::READ_WRITE,
            file,
            0,
            FileMapping {
                file: opened,
                may_write: false,
            },
        ),
        "a private file mapping was refused",
    )?;
    let keeper: Arc<dyn Any + Send + Sync> = fallible::try_arc(1_u8)?;
    let _ = space(
        parent.map_window(
            FilePlace::Anywhere(None),
            P,
            WindowPages::all(frame * PAGE_SIZE, P),
            VmaFlags::READ_WRITE,
            true,
            keeper,
        ),
        "a window was refused",
    )?;
    for at in [LOW, LOW + 4 * P, LOW + 8 * P, LOW + 12 * P] {
        space(
            parent.fault(at, Access::WRITE),
            "a write before the fork faulted",
        )?;
    }
    let child = space(parent.fork(), "a fork was refused")?;
    space(child.fault(LOW, Access::WRITE), "a child's write faulted")?;
    space(
        child.fault(LOW + 12 * P, Access::WRITE),
        "a child's file write faulted",
    )?;
    space(
        parent.fault(LOW, Access::WRITE),
        "a parent's write after the fork faulted",
    )
}

/// A private region with both pages written, moved by an `mremap` that
/// grows it past a neighbour, written at its new end, and shrunk again.
fn remap_a_private_region(_: u64) -> Result<(), Refusal> {
    let space_ = space(AddressSpace::new(), "a fresh space was refused")?;
    let _ = space(
        space_.map_anonymous(LOW, 2 * P, VmaFlags::READ_WRITE),
        "a private region was refused",
    )?;
    let _ = space(
        space_.map_anonymous(LOW + 2 * P, P, VmaFlags::READ_WRITE),
        "a neighbour was refused",
    )?;
    for at in [LOW, LOW + P] {
        space(
            space_.fault(at, Access::WRITE),
            "a write before the move faulted",
        )?;
    }
    let moved = space(
        space_.remap(LOW, 2 * P, 4 * P, Destination::Anywhere),
        "a private region's move was refused",
    )?;
    if moved == LOW {
        return Err(Refusal::Wrong("a region grew over its neighbour"));
    }
    space(
        space_.fault(moved + 3 * P, Access::WRITE),
        "a moved region's new page faulted",
    )?;
    let _ = space(
        space_.remap(moved, 4 * P, 2 * P, Destination::InPlace),
        "a moved region's shrink was refused",
    )?;
    Ok(())
}

/// A shared region with three pages written, its last page moved away on
/// its own, the moved page grown in place, and the rest shrunk.
fn remap_a_shared_region(_: u64) -> Result<(), Refusal> {
    let space_ = space(AddressSpace::new(), "a fresh space was refused")?;
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    let _ = space(
        space_.map_anonymous(LOW, 3 * P, shared),
        "a shared region was refused",
    )?;
    for at in [LOW, LOW + P, LOW + 2 * P] {
        space(space_.fault(at, Access::WRITE), "a shared write faulted")?;
    }
    let apart = space(
        space_.remap(LOW + 2 * P, P, P, Destination::Fixed(LOW + 8 * P)),
        "moving a shared page on its own was refused",
    )?;
    let _ = space(
        space_.remap(apart, P, 2 * P, Destination::InPlace),
        "growing a shared page in place was refused",
    )?;
    space(
        space_.fault(apart + P, Access::WRITE),
        "a shared region's new page faulted",
    )?;
    let _ = space(
        space_.remap(LOW, 2 * P, P, Destination::InPlace),
        "shrinking a shared region was refused",
    )?;
    Ok(())
}

/// Three written pages, the middle one unmapped -- a split -- and then the
/// rest; a region placed anywhere, and a file's private mapping written and
/// unmapped, which lets go of the file.
fn unmap_from_the_middle(_: u64) -> Result<(), Refusal> {
    let space_ = space(AddressSpace::new(), "a fresh space was refused")?;
    let _ = space(
        space_.map_anonymous(LOW, 3 * P, VmaFlags::READ_WRITE),
        "a region was refused",
    )?;
    for at in [LOW, LOW + P, LOW + 2 * P] {
        space(
            space_.fault(at, Access::WRITE),
            "a write before the split faulted",
        )?;
    }
    space(
        space_.unmap(LOW + P, P),
        "unmapping a region's middle was refused",
    )?;
    space(
        space_.unmap(LOW, 3 * P),
        "unmapping what was left was refused",
    )?;
    let anywhere = space(
        space_.map_anywhere(None, 2 * P, VmaFlags::READ_WRITE),
        "a region placed anywhere was refused",
    )?;
    space(
        space_.fault(anywhere, Access::WRITE),
        "a write to a placed region faulted",
    )?;
    let file = Vmo::new_anonymous(1)?;
    let at = map_file(&space_, &file, VmaFlags::READ_WRITE, false)?;
    space(
        space_.fault(at, Access::WRITE),
        "a write to a file's private page faulted",
    )?;
    space(
        space_.unmap(at, P),
        "unmapping a file's mapping was refused",
    )
}

/// Map all of `file` into `space_` wherever it fits, as a file opened for
/// writing (`may_write`) or not.
fn map_file(
    space_: &AddressSpace,
    file: &Arc<Vmo>,
    flags: VmaFlags,
    may_write: bool,
) -> Result<u64, Refusal> {
    let opened: Arc<dyn Any + Send + Sync> = fallible::try_arc(0_u8)?;
    space(
        space_.map_file(
            FilePlace::Anywhere(None),
            file.len_bytes(),
            flags,
            Arc::clone(file),
            0,
            FileMapping {
                file: opened,
                may_write,
            },
        ),
        "a file mapping was refused",
    )
}

/// A file of two pages, both in the file, mapped privately in two spaces
/// that each wrote its last page, cut back to one page: the copies the cut
/// takes are the mappings' own.
fn cut_a_file_under_its_mappings(_: u64) -> Result<(), Refusal> {
    let file = Vmo::new_anonymous(2)?;
    for index in 0..2 {
        let _ = object(file.commit(index), "a file's page was refused")?;
    }
    let first = space(AddressSpace::new(), "a fresh space was refused")?;
    let second = space(AddressSpace::new(), "a fresh space was refused")?;
    for space_ in [&first, &second] {
        let at = map_file(space_, &file, VmaFlags::READ_WRITE, false)?;
        space(
            space_.fault(at + P, Access::WRITE),
            "a write to a file's page faulted",
        )?;
    }
    file.set_file_len(P);
    file.cut_mappings(1);
    Ok(())
}

/// An object whose pages a fork of it shares, written -- which copies the
/// page -- held, forked while held, mapped shared and written through, and
/// asked which pages were written; and an allocation too large for any size
/// class.
fn share_write_and_hold_an_object(_: u64) -> Result<(), Refusal> {
    let vmo = Vmo::new_anonymous(2)?;
    for index in 0..2 {
        let _ = object(vmo.commit(index), "committing a page was refused")?;
    }
    let copy = object(vmo.fork(), "forking an object was refused")?;
    object(
        vmo.write_page(0, 8, b"written"),
        "writing a shared page was refused",
    )?;
    let held = object(vmo.hold(0, 2), "holding an object's pages was refused")?;
    let _held_copy = object(vmo.fork(), "forking a held object was refused")?;
    drop(held);
    let space_ = space(AddressSpace::new(), "a fresh space was refused")?;
    let shared = VmaFlags {
        shared: true,
        ..VmaFlags::READ_WRITE
    };
    let at = map_file(&space_, &copy, shared, true)?;
    space(
        space_.fault(at, Access::WRITE),
        "a write through a shared file mapping faulted",
    )?;
    let _written = copy.take_mapped_writes()?;
    // Larger than any size class, so the heap asks for pages of its own:
    // with them refused, the section's reserve serves it.
    let large = fallible::try_arc([0x5A_u8; 3000])?;
    if large.iter().any(|&byte| byte != 0x5A) {
        return Err(Refusal::Wrong("a large allocation did not hold its value"));
    }
    Ok(())
}

/// A two-page object mapped as shared code, as the vDSO is: its first page
/// read-only and shared, its second read and execute, both then unmapped.
/// x86-64's vDSO is the only caller outside this check, so on the Arm
/// architectures this is what reaches [`AddressSpace::map_shared_code`] at
/// all. Before it, a one-page object is refused as a bad range and leaves the
/// space as it was: that refusal comes before any allocation, so it is the
/// same on every run of the sweep.
fn map_shared_code(_: u64) -> Result<(), Refusal> {
    let space_ = space(AddressSpace::new(), "a fresh space was refused")?;
    let before = space_.region_count();
    let small = Vmo::new_anonymous(1)?;
    if !matches!(
        space_.map_shared_code(small, crate::syscall::exec::VDSO_END),
        Err(SpaceError::BadRange)
    ) {
        return Err(Refusal::Wrong(
            "a one-page object was not refused as shared code",
        ));
    }
    if space_.region_count() != before {
        return Err(Refusal::Wrong(
            "a refused shared-code mapping left a region",
        ));
    }
    let vmo = Vmo::new_anonymous(2)?;
    let code = space(
        space_.map_shared_code(vmo, crate::syscall::exec::VDSO_END),
        "mapping shared code was refused",
    )?;
    let regions = space_.regions()?;
    let data = regions.iter().find(|region| region.end == code);
    let text = regions.iter().find(|region| region.start == code);
    let (Some(data), Some(text)) = (data, text) else {
        return Err(Refusal::Wrong("shared code was not mapped as two regions"));
    };
    let data_right = data.start + P == code && data.flags.read && data.flags.shared;
    let data_closed = !data.flags.write && !data.flags.execute;
    let text_right = text.end == code + P && text.flags.execute && !text.flags.write;
    if !(data_right && data_closed && text_right) {
        return Err(Refusal::Wrong(
            "shared code was not a read-only data page and a read-execute code page",
        ));
    }
    space(
        space_.unmap(code - P, 2 * P),
        "unmapping shared code was refused",
    )?;
    if space_.region_count() != before {
        return Err(Refusal::Wrong("unmapping shared code left a region"));
    }
    Ok(())
}

/// One page of code, ARMv7-A's signal return page's mapping, on every
/// architecture: only ARMv7-A's exec reaches [`AddressSpace::map_code_page`]
/// otherwise. A two-page object is refused first, before any allocation;
/// then one page maps as a single region to be read and run, shared and never
/// written, and unmapping it leaves the space as it was.
fn map_code_page(_: u64) -> Result<(), Refusal> {
    let space_ = space(AddressSpace::new(), "a fresh space was refused")?;
    let before = space_.region_count();
    let large = Vmo::new_anonymous(2)?;
    if !matches!(space_.map_code_page(large), Err(SpaceError::BadRange)) {
        return Err(Refusal::Wrong(
            "a two-page object was not refused as a code page",
        ));
    }
    if space_.region_count() != before {
        return Err(Refusal::Wrong("a refused code page left a region"));
    }
    let vmo = Vmo::new_anonymous(1)?;
    let at = space(space_.map_code_page(vmo), "mapping a code page was refused")?;
    let regions = space_.regions()?;
    let Some(page) = regions.iter().find(|region| region.start == at) else {
        return Err(Refusal::Wrong("a code page was not where it was mapped"));
    };
    let flags = page.flags;
    if page.end != at + P || !(flags.read && flags.execute && flags.shared) || flags.write {
        return Err(Refusal::Wrong(
            "a code page was not one read-execute, unwritable, shared page",
        ));
    }
    space(space_.unmap(at, P), "unmapping a code page was refused")?;
    if space_.region_count() != before {
        return Err(Refusal::Wrong("unmapping a code page left a region"));
    }
    Ok(())
}

/// Three kernel stacks, the middle one freed on its own -- which splits the
/// arena's record of the three -- and then the other two under one
/// shootdown.
fn free_stacks_together(_: u64) -> Result<(), Refusal> {
    let mut stacks = [None; 3];
    let mut made = Ok(());
    for slot in &mut stacks {
        match vmap::allocate_stack() {
            Ok(stack) => *slot = Some(stack),
            Err(error) => {
                made = arena(Err(error), "a kernel stack was refused");
                break;
            }
        }
    }
    let [first, middle, last] = stacks;
    match (made, first, middle, last) {
        (Ok(()), Some(first), Some(middle), Some(last)) => {
            // SAFETY: (KMEM) made just above, and nothing has run on any of them.
            let alone = unsafe { vmap::free_stacks(&[middle]) };
            // SAFETY: (KMEM) as above.
            let rest = unsafe { vmap::free_stacks(&[first, last]) };
            arena(alone.and(rest), "freeing kernel stacks was refused")
        }
        (made, ..) => {
            for stack in stacks.into_iter().flatten() {
                // SAFETY: (KMEM) made just above, and nothing has run on it.
                let _ = unsafe { vmap::free_stack(stack) };
            }
            made
        }
    }
}
