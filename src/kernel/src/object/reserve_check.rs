//! A job's memory reservation, proved at boot (`docs/microkernel/K12.md` §3,
//! steps 5 to 9; the floors themselves are stage 2's, `mm::check`).
//!
//! In a job tree of the check's own: a reserved job and a QM sibling. The
//! reservation's rules refuse what they must, the reserved job's frames are
//! drawn from its reservation through the real route while an interrupt's
//! never are, a reclaim for anyone else spares a drawn frame, and when the
//! tree is gone its reservation promises nothing and every slot is given
//! back.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;

use crate::mm;
use crate::object::job::Job;
use crate::object::quota::{self, Mark, ReserveError, Resource};

/// An object of every size class, made as a task of `job` and freed, so that its
/// heap keeps a page of each.
fn keep_a_page_of_every_class(job: &Arc<Job>) -> Result<(), &'static str> {
    let own = crate::sched::running_group();
    crate::sched::set_current_group(job.quota_index());
    let mut bytes = 16;
    while bytes <= 2048 {
        drop(crate::fallible::try_with_capacity::<u8>(bytes));
        bytes *= 2;
    }
    crate::sched::set_current_group(own);
    Ok(())
}

/// A new job's reservation on an entry a gone job's heap kept pages of is whole:
/// the pages went back at the bind, so one reserved fewer frames than were kept
/// draws every one of them (ledger 6260 B1).
fn a_rebound_reservation_is_whole(entry: mm::Reservation) -> Result<(), &'static str> {
    const SMALL: u64 = 4;
    if mm::reservation_usage(entry).1 <= SMALL {
        return Err("the rebinding case's heap kept too few pages to show anything");
    }
    let tree = Job::new_root().map_err(|_| "no memory for a job")?;
    let job = tree.new_child().map_err(|_| "no memory for a job")?;
    job.set_reservation(SMALL * PAGE_SIZE)
        .map_err(|_| "a small reservation was refused")?;
    if job.draws_from() != entry {
        return Err("a reservation did not take the entry a gone job's heap left");
    }
    if mm::reservation_usage(entry).1 != 0 {
        return Err("a rebound reservation started with frames drawn");
    }
    let own = crate::sched::running_group();
    crate::sched::set_current_group(job.quota_index());
    let mut held = alloc::vec::Vec::new();
    for _ in 0..SMALL {
        if let Some(frame) = mm::allocate_user_frame() {
            held.push(frame);
        }
    }
    crate::sched::set_current_group(own);
    let drawn = mm::reservation_usage(entry).1;
    for frame in held {
        let _ = mm::release_frame(frame);
    }
    if drawn != SMALL {
        return Err("a rebound reservation did not give its job every frame");
    }
    Ok(())
}

/// Pages the check reserves.
const RESERVED: u64 = 64;

/// What the check saw, for the boot line.
#[derive(Debug, Default)]
pub struct Report {
    /// Pages reserved, and drawn by the reserved job's frames.
    pub reserved: u64,
    /// Rules refused as they must be.
    pub rules: u32,
    /// The promise before the check, which it must leave as it found it.
    pub promise: u64,
    /// Interrupts dispatched since boot, every one marked as interrupt
    /// context (ledger 2783 (2)).
    pub interrupts: u64,
    /// Heap pages the reserved job's objects were drawn on.
    pub heap_pages: u64,
}

// CHECKS-ONLY: stages_check.rs
/// Run every check.
///
/// # Errors
///
/// Which property failed.
///
/// Verifies: H.QUOTA.17
/// Verifies: L.object.222
/// Verifies: L.object.223
/// Verifies: L.object.225
/// Verifies: L.user.145
/// Verifies: L.mm.81
#[doc(hidden)]
pub fn run() -> Result<Report, &'static str> {
    let slots = quota::live_slots();
    let promise = mm::outstanding_promise();
    let mut report = Report {
        reserved: RESERVED,
        rules: 0,
        promise,
        interrupts: 0,
        heap_pages: 0,
    };
    report.interrupts = every_interrupt_marked()?;
    let kept_entry;
    {
        let tree = Job::new_root().map_err(|_| "no memory for a job")?;
        let reserved = tree.new_child().map_err(|_| "no memory for a job")?;
        let sibling = tree.new_child().map_err(|_| "no memory for a job")?;
        check_the_rules(&tree, &reserved, &sibling, &mut report)?;
        check_the_draws(&reserved)?;
        if mm::outstanding_promise() != promise + RESERVED {
            return Err("the reserved job's frames, all freed, did not make its promise whole");
        }
        report.heap_pages = check_the_heap(&reserved, &sibling)?;
        keep_a_page_of_every_class(&reserved)?;
        kept_entry = reserved.draws_from();
    }
    a_rebound_reservation_is_whole(kept_entry)?;
    check_the_heap_scope()?;
    if mm::outstanding_promise() != promise {
        return Err("a gone job's reservation still promised frames");
    }
    if quota::live_slots() != slots {
        return Err("the checks' jobs are gone and their quota slots are not");
    }
    Ok(report)
}

/// Every interrupt dispatched since boot ran marked as interrupt context,
/// through the real entry of this architecture (ledger 2783 (2)): at least
/// one timer tick is waited for, and none ran unmarked. How many there were.
fn every_interrupt_marked() -> Result<u64, &'static str> {
    let ticks = crate::timer::ticks();
    let give_up = crate::timer::now_nanos().saturating_add(1_000_000_000);
    while crate::timer::ticks() == ticks && crate::timer::now_nanos() < give_up {
        core::hint::spin_loop();
    }
    if crate::timer::ticks() == ticks {
        return Err("no timer interrupt arrived to show its entry marked");
    }
    if crate::irq::unmarked() != 0 {
        return Err("an interrupt ran its handler unmarked as interrupt context");
    }
    Ok(crate::irq::delivered())
}

/// Require `refused` to be the rule's refusal, and count it.
fn rule(refused: bool, what: &'static str, report: &mut Report) -> Result<(), &'static str> {
    if !refused {
        return Err(what);
    }
    report.rules += 1;
    Ok(())
}

/// The rules of `docs/microkernel/K12.md` §2.6, each refused with nothing
/// changed, around one reservation admitted.
fn check_the_rules(
    tree: &Arc<Job>,
    reserved: &Arc<Job>,
    sibling: &Arc<Job>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let bytes = RESERVED * PAGE_SIZE;
    // Never above its own limit.
    if !reserved.set_limit(Resource::Memory, bytes - PAGE_SIZE) {
        return Err("a memory limit was refused on a job with no reservation");
    }
    rule(
        reserved.set_reservation(bytes) == Err(ReserveError::Rule),
        "a reservation above the job's own limit was admitted",
        report,
    )?;
    if !reserved.set_limit(Resource::Memory, quota::UNLIMITED) {
        return Err("a memory limit could not be lifted");
    }
    // Not under a limit or a high mark.
    if !tree.set_limit(Resource::Memory, 1 << 30) {
        return Err("a memory limit was refused on a tree with no reservation");
    }
    rule(
        reserved.set_reservation(bytes) == Err(ReserveError::Rule),
        "a reservation under a memory limit was admitted",
        report,
    )?;
    if !tree.set_limit(Resource::Memory, quota::UNLIMITED) || !tree.set_mark(Mark::High, 1 << 30) {
        return Err("a limit or mark could not be set on a tree with no reservation");
    }
    rule(
        reserved.set_reservation(bytes) == Err(ReserveError::Rule),
        "a reservation under a memory.high mark was admitted",
        report,
    )?;
    if !tree.set_mark(Mark::High, quota::UNLIMITED) {
        return Err("a memory.high mark could not be lifted");
    }
    // Admitted on an empty job.
    if reserved.draws_from() != mm::NO_RESERVATION {
        return Err("a job drew from a reservation before it had one");
    }
    reserved
        .set_reservation(bytes)
        .map_err(|_| "a reservation the free frames cover was refused")?;
    let entry = reserved.draws_from();
    if entry == mm::NO_RESERVATION {
        return Err("an admitted reservation is drawn from by nobody");
    }
    if reserved
        .reservation()
        .map(|usage| (usage.limit, usage.used))
        != Some((bytes, 0))
    {
        return Err("a reservation did not read back as admitted");
    }
    // A sub-job draws from it, and holds none of its own.
    let beneath = reserved.new_child().map_err(|_| "no memory for a job")?;
    if beneath.draws_from() != entry {
        return Err("a job made beneath a reserved one drew from another reservation");
    }
    rule(
        beneath.set_reservation(bytes) == Err(ReserveError::Rule),
        "a reservation beneath a reserved job was admitted",
        report,
    )?;
    // Nothing above it takes a limit or a mark, and it takes none below it.
    rule(
        !tree.set_limit(Resource::Memory, 1 << 30),
        "a memory limit was set above a reserved job",
        report,
    )?;
    rule(
        !tree.set_mark(Mark::High, 1 << 30),
        "a memory.high mark was set above a reserved job",
        report,
    )?;
    rule(
        !reserved.set_limit(Resource::Memory, bytes - PAGE_SIZE),
        "a memory limit was set below a job's reservation",
        report,
    )?;
    if !reserved.set_limit(Resource::Memory, bytes)
        || !reserved.set_limit(Resource::Memory, quota::UNLIMITED)
    {
        return Err("a memory limit at a job's reservation was refused");
    }
    // A job with a child is not reserved for the first time.
    let parent = sibling.new_child().map_err(|_| "no memory for a job")?;
    let _child = parent.new_child().map_err(|_| "no memory for a job")?;
    rule(
        parent.set_reservation(bytes) == Err(ReserveError::Rule),
        "a job with a child was reserved for the first time",
        report,
    )?;
    // Work from outside the reserved subtree is not admitted; from inside, it is.
    rule(
        !reserved.admits_work_from(sibling),
        "a reserved job admitted work from outside it",
        report,
    )?;
    if !beneath.admits_work_from(reserved) || !sibling.admits_work_from(reserved) {
        return Err("work from inside a reserved subtree, or into an unreserved job, was refused");
    }
    Ok(())
}

/// The reserved job's frames through the real route: each drawn from its
/// reservation and tagged with it, the next past it given untagged, one
/// taken while an interrupt is serviced never drawn (ledger 2550 (1)), and a
/// reclaim for anyone else sparing a drawn frame (`L.user.145`).
fn check_the_draws(reserved: &Arc<Job>) -> Result<(), &'static str> {
    let entry = reserved.draws_from();
    // Room for every frame held, made as the checking task: a heap page
    // drawn from the reservation here would take one of its frames.
    let mut held = alloc::vec::Vec::new();
    held.try_reserve(RESERVED as usize + 2)
        .map_err(|_| "no memory to hold the check's frames")?;
    let own = crate::sched::running_group();
    crate::sched::set_current_group(reserved.quota_index());
    let outcome = draws(entry, &mut held);
    crate::sched::set_current_group(own);
    for frame in held {
        let _ = mm::release_frame(frame);
    }
    outcome?;
    if reserved.reservation().map(|usage| usage.used) != Some(0) {
        return Err("a reservation still counted frames that were freed");
    }
    Ok(())
}

/// [`check_the_draws`]'s steps, as a task of the reserved job, every frame
/// taken pushed to `held` for the caller to give back.
fn draws(entry: mm::Reservation, held: &mut alloc::vec::Vec<u64>) -> Result<(), &'static str> {
    for _ in 0..RESERVED {
        let frame = mm::allocate_user_frame().ok_or("a reserved job's frame was refused")?;
        held.push(frame);
        if mm::drawn_from(frame) != entry {
            return Err("a reserved job's frame was not drawn from its reservation");
        }
    }
    let past = mm::allocate_user_frame().ok_or("a frame past the reservation was refused")?;
    held.push(past);
    if mm::drawn_from(past) != mm::NO_RESERVATION {
        return Err("a frame past the reservation's size was drawn from it");
    }
    let back = held.swap_remove(0);
    let _ = mm::release_frame(back);
    let interrupted = crate::irq::serviced(mm::allocate_user_frame)
        .ok_or("a frame in interrupt context was refused")?;
    held.push(interrupted);
    if mm::drawn_from(interrupted) != mm::NO_RESERVATION {
        return Err("an interrupt drew from a reservation");
    }
    let Some(&drawn) = held.iter().find(|&&frame| mm::drawn_from(frame) == entry) else {
        return Err("no drawn frame was left to show a reclaim");
    };
    if crate::user::cache::spares(drawn, mm::NO_RESERVATION)
        || !crate::user::cache::spares(drawn, entry)
        || !crate::user::cache::spares(interrupted, mm::NO_RESERVATION)
    {
        return Err("a machine-wide reclaim took a reserved page");
    }
    Ok(())
}

/// Objects the heap case makes as each job.
const OBJECTS: usize = 24;

/// Each object's bytes: two to a page, so that the objects span more pages than
/// the heap keeps once they are freed, and a free that missed its heap would show.
const OBJECT_BYTES: usize = 2048;

/// The reserved job's kernel heap (`L.mm.81`, `docs/microkernel/K12.md`
/// §2.4): its objects, made in task context, lie on pages drawn from its
/// reservation, the sibling's of the same size class on no reservation's,
/// and one made while an interrupt is serviced on none; freed, each goes back
/// to its own heap, and what stays drawn is at most the empty page a class
/// keeps. How many pages the reserved job's objects were drawn on.
fn check_the_heap(reserved: &Arc<Job>, sibling: &Arc<Job>) -> Result<u64, &'static str> {
    let entry = reserved.draws_from();
    let before = mm::reservation_usage(entry).1;
    let mut mine: [Option<alloc::vec::Vec<u8>>; OBJECTS] = [const { None }; OBJECTS];
    let mut theirs: [Option<alloc::vec::Vec<u8>>; OBJECTS] = [const { None }; OBJECTS];
    let own = crate::sched::running_group();
    crate::sched::set_current_group(reserved.quota_index());
    for object in &mut mine {
        *object = crate::fallible::try_with_capacity(OBJECT_BYTES).ok();
    }
    let interrupted: Option<alloc::vec::Vec<u8>> =
        crate::irq::serviced(|| crate::fallible::try_with_capacity(OBJECT_BYTES).ok());
    crate::sched::set_current_group(sibling.quota_index());
    for object in &mut theirs {
        *object = crate::fallible::try_with_capacity(OBJECT_BYTES).ok();
    }
    crate::sched::set_current_group(own);
    let drawn = mm::reservation_usage(entry).1.saturating_sub(before);
    let on = |object: &Option<alloc::vec::Vec<u8>>| {
        object
            .as_ref()
            .map(|object| mm::heap_drawn_from(object.as_ptr() as u64))
    };
    let outcome = if mine.iter().any(|object| on(object) != Some(entry)) {
        Err("a reserved job's object is in a shared page")
    } else if theirs
        .iter()
        .any(|object| on(object) != Some(mm::NO_RESERVATION))
    {
        Err("another job's object is on a reserved page")
    } else if on(&interrupted) != Some(mm::NO_RESERVATION) {
        Err("an interrupt's object was drawn from a reservation")
    } else {
        Ok(drawn)
    };
    drop(mine);
    drop(theirs);
    drop(interrupted);
    if outcome.is_ok() && mm::reserved_heap_bytes(entry) != 0 {
        return Err("a reserved job's object freed did not go back to its heap");
    }
    let kept = mm::reservation_usage(entry).1.saturating_sub(before);
    if outcome.is_ok() && kept > ferrix_heap::CLASSES as u64 {
        return Err("a reserved heap kept more than a page a class once its objects were freed");
    }
    outcome
}

/// Frames the heap scope's check promises its reservation.
const SCOPE_FRAMES: u64 = 16;

/// The heap scope (`L.mm.140`, `docs/BTRFS-PRESSURE.md` W1-a): a reservation
/// bound and promised [`SCOPE_FRAMES`], a scope entered on it, and the
/// shared heap made to refuse this task's scoped allocations
/// (`mm::refuse_shared_heap_for`), so that only the reservation serves:
///
/// * one object of every size class, each on a page drawn from the
///   reservation, then objects until one is refused -- no more frames drawn
///   than promised, and the refusal counted as an overrun;
/// * an allocation in interrupt context, during the scope, on a shared page;
/// * one with the scope suspended on a shared page, and one after the
///   suspension on a reserved page again;
/// * once the scope's guard is gone, the next allocation on a shared page;
/// * every object freed, nothing of the reservation's heap held.
///
/// Verifies: L.mm.140
fn check_the_heap_scope() -> Result<(), &'static str> {
    use alloc::vec::Vec;
    const CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
    let me = crate::sched::current_id().ok_or("the heap scope's check runs outside a task")?;
    let entry = mm::reservation_bind().ok_or("no reservation was free for the heap scope")?;
    let on = |object: &Vec<u8>| mm::heap_drawn_from(object.as_ptr() as u64);
    let outcome = (|| {
        mm::reservation_resize(entry, SCOPE_FRAMES)
            .map_err(|_| "the heap scope's reservation was refused")?;
        let overruns = mm::heap_scope_overruns(entry);
        let mut held: Vec<Vec<u8>> = Vec::new();
        held.try_reserve(256)
            .map_err(|_| "no room to hold the heap scope's objects")?;
        let scope =
            mm::heap_scope(entry).ok_or("a heap scope on a bound reservation was refused")?;
        mm::refuse_shared_heap_for(me);
        let mut served = true;
        for size in CLASSES {
            match crate::fallible::try_with_capacity::<u8>(size) {
                Ok(object) => held.push(object),
                Err(_) => served = false,
            }
        }
        let classes_drawn = held.iter().all(|object| on(object) == entry);
        let mut refused = false;
        while held.len() < held.capacity() {
            match crate::fallible::try_with_capacity::<u8>(2048) {
                Ok(object) => held.push(object),
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        mm::refuse_shared_heap_for(0);
        let interrupted: Option<Vec<u8>> =
            crate::irq::serviced(|| crate::fallible::try_with_capacity(64).ok());
        let suspended = {
            let _paused = mm::heap_scope_suspended().ok_or("the heap scope was not suspended")?;
            crate::fallible::try_with_capacity::<u8>(64).ok()
        };
        let resumed = crate::fallible::try_with_capacity::<u8>(64).ok();
        drop(scope);
        let after = crate::fallible::try_with_capacity::<u8>(64).ok();
        let (_, drawn) = mm::reservation_usage(entry);
        let counted = mm::heap_scope_overruns(entry) > overruns;
        let shared = |object: &Option<Vec<u8>>| object.as_ref().map(on) == Some(mm::NO_RESERVATION);
        let result = if !served || !classes_drawn {
            Err("a scoped allocation of a size class was not served from its reservation")
        } else if held.iter().any(|object| on(object) != entry) {
            Err("a scoped allocation was served from a shared page")
        } else if !refused || drawn > SCOPE_FRAMES {
            Err("a scoped allocation was served past its reservation's room")
        } else if !counted {
            Err("a scoped allocation past its reservation was not counted")
        } else if !shared(&interrupted) {
            Err("an interrupt's allocation during a heap scope was drawn from its reservation")
        } else if !shared(&suspended) {
            Err("an allocation with the heap scope suspended was drawn from its reservation")
        } else if resumed.as_ref().map(on) != Some(entry) {
            Err("an allocation after the scope's suspension was not drawn from its reservation")
        } else if !shared(&after) {
            Err("an allocation after the heap scope's end was drawn from its reservation")
        } else {
            Ok(())
        };
        drop((held, interrupted, suspended, resumed, after));
        if result.is_ok() && mm::reserved_heap_bytes(entry) != 0 {
            return Err(
                "the heap scope's objects, freed, left the reservation's heap holding some",
            );
        }
        result
    })();
    mm::refuse_shared_heap_for(0);
    mm::reservation_unbind(entry);
    outcome
}
