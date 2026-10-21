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

/// Pages the check reserves.
const RESERVED: u64 = 16;

/// What the check saw, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Pages reserved, and drawn by the reserved job's frames.
    pub(crate) reserved: u64,
    /// Rules refused as they must be.
    pub(crate) rules: u32,
    /// The promise before the check, which it must leave as it found it.
    pub(crate) promise: u64,
    /// Interrupts dispatched since boot, every one marked as interrupt
    /// context (ledger 2783 (2)).
    pub(crate) interrupts: u64,
}

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
pub(crate) fn run() -> Result<Report, &'static str> {
    let slots = quota::live_slots();
    let promise = mm::outstanding_promise();
    let mut report = Report {
        reserved: RESERVED,
        rules: 0,
        promise,
        interrupts: 0,
    };
    report.interrupts = every_interrupt_marked()?;
    {
        let tree = Job::new_root().map_err(|_| "no memory for a job")?;
        let reserved = tree.new_child().map_err(|_| "no memory for a job")?;
        let sibling = tree.new_child().map_err(|_| "no memory for a job")?;
        check_the_rules(&tree, &reserved, &sibling, &mut report)?;
        check_the_draws(&reserved)?;
        if mm::outstanding_promise() != promise + RESERVED {
            return Err("the reserved job's frames, all freed, did not make its promise whole");
        }
    }
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
    let own = crate::sched::running_group();
    crate::sched::set_current_group(reserved.quota_index());
    let mut held = alloc::vec::Vec::new();
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
    held.try_reserve(RESERVED as usize + 2)
        .map_err(|_| "no memory to hold the check's frames")?;
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
