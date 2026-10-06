//! Speculation domains: which switches skip the predictor barrier
//! (`docs/OPAQUE-KERNEL.md` §9.2, §9.3, §9.3a).
//!
//! The rule is decided where the barrier is, in `arch::speculation`'s
//! `entered_space`, so the check drives that path itself: it installs the
//! address spaces of processes in and out of domains on this processor, with
//! interrupts masked, and counts the switches the processor decided needed
//! the barrier (`arch::barrier_decisions_on`). The count is of decisions,
//! not of barriers issued, because a processor the reference configuration
//! runs without one -- QEMU's Cortex-A72 -- issues none, and the rule must
//! still be seen to hold there.
//!
//! The cases, each a switch from one space to another and back:
//! 1. two processes born in one marked job: no barrier;
//! 2. one of them and a process of an unmarked job, and one of them and a
//!    process of another marked job: one each way;
//! 3. a member moved out of the job, with one left in it: one each way;
//! 4. `job_create` marking without MANAGE on the parent, or with an unknown
//!    option bit: refused, and no `DOMAIN` record; marking with it: one;
//! 5. a member that lost dumpability by a change of credentials, and one by
//!    `PR_SET_DUMPABLE`'s path, with a member: one each way;
//! 6. a member whose space a process of another domain shares: one each way;
//! 7. a marked job made inside a marked job: a domain of its own, so a switch
//!    between the two jobs' members is one each way.
//! 8. a member that leaves: every processor whose last space was in its
//!    domain, this one and one running its thread, issues the barrier before
//!    the leave returns;
//! 9. a process that left before its birth stays out of its job's domain;
//! 10. a child job of a marked job, a fork, a process moved in and its
//!     fork, and a forgotten root;
//! 11. a processor that records the leaver's domain after the leave's scan
//!     has passed it, as a processor that read the space's domain before the
//!     leave could (the certification finding F-60), still issues the
//!     barrier before the leave returns;
//!
//! And inside a domain, no invalidation and, on x86-64, the refill.

use alloc::sync::Arc;

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::JOB_SPECULATION_DOMAIN;

use ferrix_native_abi::handle::Handle;
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::IrqControl;

use super::check::Side;
use super::job::{self, Job};
use super::{Object, process::Host as _};
use crate::arch;
use crate::audit;
use crate::syscall::attributes;
use crate::syscall::process::{self, Process};
use crate::user::space::AddressSpace;

/// What the check found, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Switches between members of one domain, none of which needed the
    /// barrier.
    pub(crate) skipped: u64,
    /// Switches out of a domain, each of which did.
    pub(crate) kept: u64,
    /// Whether the processor issues a barrier at all: the count above is of
    /// decisions either way.
    pub(crate) hardened: bool,
    /// Whether case 8 saw the barrier issued on a second processor, which a
    /// machine with one cannot show.
    pub(crate) elsewhere: bool,
    /// Whether case 11 ran, which it needs a second processor for.
    pub(crate) late: bool,
}

/// Run every case.
///
/// Verifies: H.TRAP.17, L.object.113, L.object.114, L.object.115, L.object.116
/// Verifies: `L.x86_64.126`, L.aarch64.52, L.armv7a.3
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report {
        hardened: arch::HARDENED,
        ..Report::default()
    };
    // A build without the defences decides nothing at a switch.
    if !arch::HARDENED {
        return Ok(report);
    }
    let tree = Job::new_root().map_err(|_| "no memory for the domain check's jobs")?;
    let marked = tree
        .new_child_domain()
        .map_err(|_| "a job refused a marked child")?;
    let other = tree
        .new_child_domain()
        .map_err(|_| "a job refused a second marked child")?;
    let plain = tree.new_child().map_err(|_| "a job refused a child")?;
    if marked.domain() == 0 || other.domain() == 0 || marked.domain() == other.domain() {
        return Err("two marked jobs are not two speculation domains");
    }
    if plain.domain() != 0 {
        return Err("a job made unmarked is a speculation domain");
    }

    let first = born_in(&marked)?;
    let second = born_in(&marked)?;
    let stranger = born_in(&plain)?;
    let neighbour = born_in(&other)?;
    let mut made = alloc::vec![
        Arc::clone(&first),
        Arc::clone(&second),
        Arc::clone(&stranger),
        Arc::clone(&neighbour)
    ];

    // 1. Inside one domain.
    expect(
        &first,
        &second,
        0,
        "case 1: a switch between two members of one domain issued the barrier",
        &mut report,
    )?;
    // 2. Out of it, to no domain and to another.
    expect(
        &first,
        &stranger,
        2,
        "case 2: a switch between a member and a process in no domain skipped the barrier",
        &mut report,
    )?;
    expect(
        &first,
        &neighbour,
        2,
        "case 2: a switch between members of two domains skipped the barrier",
        &mut report,
    )?;

    // 3. A member that moves out leaves, wherever it went.
    let mover = born_in(&marked)?;
    made.push(Arc::clone(&mover));
    expect(
        &first,
        &mover,
        0,
        "case 3: a switch between two members of one domain issued the barrier",
        &mut report,
    )?;
    mover
        .core()
        .move_to(&marked)
        .map_err(|_| "a member could not stay in its job")?;
    mover
        .core()
        .move_to(&plain)
        .map_err(|_| "a member could not move out of its job")?;
    expect(
        &first,
        &mover,
        2,
        "case 3: a member that moved out of its domain still skipped the barrier",
        &mut report,
    )?;

    // 4. Marking: MANAGE on the parent, and no other option.
    check_marking(&tree)?;

    check_leavers(&first, &marked, &mut made, &mut report)?;
    check_in_domain_path(&first, &second)?;
    check_routes(&first, &marked, &mut made, &mut report)?;
    report.elsewhere = check_leaving_at_once(&first, &marked, &mut made)?;
    report.late = check_recorded_after_the_scan(&first, &marked, &mut made)?;

    for process in made {
        process.kill(job::KILLED_STATUS);
    }
    Ok(report)
}

/// Cases 5 to 7: members that rose in privilege or share their space with
/// another domain are out of it, and a marked job inside a marked job is a
/// domain of its own. Each switched with `first`, a member of `marked`.
fn check_leavers(
    first: &Arc<Process>,
    marked: &Arc<Job>,
    made: &mut alloc::vec::Vec<Arc<Process>>,
    report: &mut Report,
) -> Result<(), &'static str> {
    // 5. A rise in privilege leaves the domain (A1).
    let promoted = born_in(marked)?;
    let hidden = born_in(marked)?;
    made.push(Arc::clone(&promoted));
    made.push(Arc::clone(&hidden));
    attributes::credentials_changed(&promoted);
    attributes::update(&hidden, |held| held.dumpable = false);
    expect(
        first,
        &promoted,
        2,
        "case 5: a member whose credentials changed still skipped the barrier",
        report,
    )?;
    expect(
        first,
        &hidden,
        2,
        "case 5: a member that is no longer dumpable still skipped the barrier",
        report,
    )?;

    // 6. A space shared with a process of another domain is in neither.
    let sharer = born_in(marked)?;
    made.push(Arc::clone(&sharer));
    let guest = Process::new(Arc::clone(sharer.core().space()))
        .map_err(|_| "no memory for a process sharing a space")?;
    let guest = crate::syscall::registry::register(guest);
    made.push(Arc::clone(&guest));
    expect(
        first,
        &sharer,
        2,
        "case 6: a member whose space a process of no domain shares skipped the barrier",
        report,
    )?;

    // 7. A marked job inside a marked job is its own domain.
    let nested = marked
        .new_child_domain()
        .map_err(|_| "a marked job refused a marked child")?;
    if nested.domain() == marked.domain() || nested.domain() == 0 {
        return Err("case 7: a marked job inside a marked job is not a domain of its own");
    }
    let inner = born_in(&nested)?;
    made.push(Arc::clone(&inner));
    expect(
        first,
        &inner,
        2,
        "case 7: a switch between members of a domain and of one inside it skipped the barrier",
        report,
    )
}

/// A process of the check's own, born in `job` as `process_create` makes
/// one: made in the root job and moved, unstarted, into its own.
fn born_in(job: &Arc<Job>) -> Result<Arc<Process>, &'static str> {
    let process = process::new_for_check().map_err(|_| "no process for the domain check")?;
    process
        .core()
        .move_new_to(job)
        .map_err(|_| "a job refused a new process")?;
    if process.core().speculation_domain() != job.domain() {
        return Err("a process made in a job is not in its job's speculation domain");
    }
    Ok(process)
}

/// Switch this processor from `from`'s space to `to`'s and back, and fail
/// with `why` unless exactly `wanted` of the two switches needed the barrier.
fn expect(
    from: &Process,
    to: &Process,
    wanted: u64,
    why: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    let decided = switch_and_back(from.core().space(), to.core().space()).decided;
    if decided != wanted {
        crate::console::println!(
            "  domain   {why}: {decided} of 2 switches needed the barrier, {wanted} wanted (domains {} and {})",
            from.core().space().domain(),
            to.core().space().domain(),
        );
        return Err(why);
    }
    if wanted == 0 {
        report.skipped += 2;
    } else {
        report.kept += 2;
    }
    Ok(())
}

/// What a processor did over two switches: see [`switch_and_back`].
#[derive(Debug, Clone, Copy)]
struct Counts {
    /// Switches it decided needed the barrier.
    decided: u64,
    /// Invalidations it issued.
    issued: u64,
    /// Return-stack refills it made at a switch inside a domain.
    refilled: u64,
}

/// This processor's three counts, read together: see [`Counts`].
fn counts(cpu: usize) -> Counts {
    Counts {
        decided: arch::barrier_decisions_on(cpu),
        issued: arch::switch_barriers_on(cpu),
        refilled: arch::refills_in_domain_on(cpu),
    }
}

/// Install `from`, then `to` over it and `from` again over that, as two
/// switches between programs would, and leave the processor with no user
/// space, as the kernel thread running this had it. Answers what the
/// processor did at the two switches.
fn switch_and_back(from: &Arc<AddressSpace>, to: &Arc<AddressSpace>) -> Counts {
    let saved = <arch::Irq as IrqControl>::disable();
    let cpu = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    // SAFETY: (TRANSLATE) both spaces are held by the caller for the whole of this
    // function, past the uninstall that ends it; interrupts are masked, so
    // nothing switches this processor meanwhile, and the kernel thread
    // running this touches no user address.
    unsafe { from.install(None) };
    let before = counts(cpu);
    // SAFETY: (TRANSLATE) as above.
    unsafe { to.install(Some(from)) };
    // SAFETY: (TRANSLATE) as above.
    unsafe { from.install(Some(to)) };
    let after = counts(cpu);
    // SAFETY: (TRANSLATE) as above: back to no user space.
    unsafe { from.uninstall() };
    <arch::Irq as IrqControl>::restore(saved);
    Counts {
        decided: after.decided.saturating_sub(before.decided),
        issued: after.issued.saturating_sub(before.issued),
        refilled: after.refilled.saturating_sub(before.refilled),
    }
}

/// Case 4: `job_create` marks only under MANAGE on the parent and with no
/// other option bit, and records each marking it makes, and only those.
fn check_marking(tree: &Arc<Job>) -> Result<(), &'static str> {
    let side = Side::new()?;
    let watcher = side
        .process
        .with_handles(|table| table.insert(Object::Job(Arc::clone(tree)), Rights::WAIT))
        .map_err(|_| "no room for a job handle")?;
    let manager = side
        .process
        .with_handles(|table| table.insert(Object::Job(Arc::clone(tree)), Rights::JOB))
        .map_err(|_| "no room for a job handle")?;
    let before = domain_records();
    if side.call(
        nr::JOB_CREATE,
        &[u64::from(watcher.0), JOB_SPECULATION_DOMAIN],
    ) != Err(status::ACCESS_DENIED)
    {
        return Err("case 4: job_create marked a job without MANAGE on its parent");
    }
    if side.call(
        nr::JOB_CREATE,
        &[u64::from(manager.0), JOB_SPECULATION_DOMAIN << 1],
    ) != Err(status::INVALID_ARGS)
    {
        return Err("case 4: job_create took an option it does not know");
    }
    if domain_records() != before {
        return Err("case 4: a refused job_create wrote a DOMAIN audit record");
    }
    let made = side
        .call(
            nr::JOB_CREATE,
            &[u64::from(manager.0), JOB_SPECULATION_DOMAIN],
        )
        .map_err(|_| "case 4: job_create refused to mark a job under MANAGE on its parent")?;
    if domain_records() != before + 1 {
        return Err("case 4: a job_create that marked a job wrote no DOMAIN audit record");
    }
    // And the record names what was made: the new job, its parent and its
    // domain.
    let handle = Handle(u32::try_from(made).map_err(|_| "case 4: job_create answered no handle")?);
    let child = side
        .process
        .with_handles(|table| match table.get(handle) {
            Ok((Object::Job(job), _)) => Some(Arc::clone(job)),
            _ => None,
        })
        .ok_or("case 4: job_create's handle names no job")?;
    let record = last_domain_record().ok_or("case 4: no DOMAIN record to read back")?;
    let parent = tree.id();
    let named = [child.id() as u32, (child.id() >> 32) as u32];
    if record.target_kind != audit::target::JOB
        || record.target_id != named
        || record.detail != [parent as u32, (parent >> 32) as u32, child.domain() as u32]
        || child.domain() == 0
    {
        return Err("case 4: the DOMAIN record does not name the job, its parent and its domain");
    }
    drop(child);
    side.process.kill(job::KILLED_STATUS);
    Ok(())
}

/// The newest `DOMAIN` record the high-value ring holds.
fn last_domain_record() -> Option<audit::Record> {
    let mut records = [audit::Record::EMPTY; 16];
    let mut from = 0;
    let mut last = None;
    loop {
        let read = audit::read(audit::Which::High, from, &mut records);
        if read.copied == 0 {
            return last;
        }
        if let Some(found) = records
            .iter()
            .take(read.copied)
            .rev()
            .find(|record| record.is(audit::DOMAIN))
        {
            last = Some(*found);
        }
        from = read.next;
    }
}

/// How many `DOMAIN` records the high-value ring holds.
fn domain_records() -> u64 {
    let mut records = [audit::Record::EMPTY; 16];
    let mut from = 0;
    let mut count = 0;
    loop {
        let read = audit::read(audit::Which::High, from, &mut records);
        if read.copied == 0 {
            return count;
        }
        count += records
            .iter()
            .take(read.copied)
            .filter(|record| record.is(audit::DOMAIN))
            .count() as u64;
        from = read.next;
    }
}

/// Case 1's other half, the consultant's F3: a switch inside a domain issues
/// no invalidation, and on an architecture whose barrier has more to it --
/// x86-64's return-stack refill -- still does the rest.
fn check_in_domain_path(first: &Process, second: &Process) -> Result<(), &'static str> {
    let done = switch_and_back(first.core().space(), second.core().space());
    if done.issued != 0 {
        return Err("case 1: a switch inside one domain issued the predictor invalidation");
    }
    // None where the processor's own `CR3` write empties the return stack
    // (x86-64's ERAPS), which the check asks the processor about itself.
    let wanted = if arch::refill_wanted_in_domain() {
        2
    } else {
        0
    };
    if done.refilled != wanted {
        crate::console::println!(
            "  domain   case 1: {} of 2 switches inside one domain refilled the return stack, {wanted} wanted",
            done.refilled
        );
        return Err(
            "case 1: switches inside one domain refilled the return stack other than CPUID asks",
        );
    }
    Ok(())
}

/// Cases 9 and 10: the ways into a domain and the ways not into one.
///
/// 9. A process that left before its birth -- made not dumpable in the root
///    job, as `inherit` of a parent that is not dumpable does before
///    `process_create` moves the child -- is out of the job's domain after
///    the move (the consultant's F2).
/// 10. A child job of a marked job is in no domain; a fork of a member is a
///     member; a process moved into a marked job is not, and nor is its
///     fork's child; and a forgotten root takes its domain with it, so the
///     next member installed after it decides the barrier.
fn check_routes(
    first: &Arc<Process>,
    marked: &Arc<Job>,
    made: &mut alloc::vec::Vec<Arc<Process>>,
    report: &mut Report,
) -> Result<(), &'static str> {
    // 9.
    let early = process::new_for_check().map_err(|_| "no process for the domain check")?;
    made.push(Arc::clone(&early));
    attributes::update(&early, |held| held.dumpable = false);
    early
        .core()
        .move_new_to(marked)
        .map_err(|_| "a job refused a new process")?;
    if early.core().speculation_domain() != 0 {
        return Err("case 9: a process that left before its birth joined its job's domain");
    }
    expect(
        first,
        &early,
        2,
        "case 9: a process that left before its birth skipped the barrier",
        report,
    )?;

    // 10.
    if marked
        .new_child()
        .map_err(|_| "a job refused a child")?
        .domain()
        != 0
    {
        return Err("case 10: a child job of a marked job is a speculation domain");
    }
    let child = fork_of(first)?;
    made.push(Arc::clone(&child));
    if child.core().speculation_domain() != marked.domain() {
        return Err("case 10: a member's fork is not in its parent's domain");
    }
    expect(
        first,
        &child,
        0,
        "case 10: a member and its fork issued the barrier",
        report,
    )?;

    let incomer = process::new_for_check().map_err(|_| "no process for the domain check")?;
    made.push(Arc::clone(&incomer));
    incomer
        .core()
        .move_to(marked)
        .map_err(|_| "a marked job refused a process moved in")?;
    let grandchild = fork_of(&incomer)?;
    made.push(Arc::clone(&grandchild));
    if incomer.core().speculation_domain() != 0 || grandchild.core().speculation_domain() != 0 {
        return Err("case 10: a process moved into a marked job, or its fork, joined its domain");
    }
    expect(
        first,
        &grandchild,
        2,
        "case 10: the fork of a process moved into a marked job skipped the barrier",
        report,
    )?;

    let again = born_in(marked)?;
    made.push(Arc::clone(&again));
    if after_a_forgotten_root(first, &again) != 1 {
        return Err(
            "case 10: a member installed after a forgotten member's root skipped the barrier",
        );
    }
    Ok(())
}

/// A fork of `parent`, as `fork` makes one, with no task.
fn fork_of(parent: &Arc<Process>) -> Result<Arc<Process>, &'static str> {
    let space = AddressSpace::new().map_err(|_| "no address space for a fork")?;
    let child = Process::forked(parent, space, false, false).map_err(|_| "no memory for a fork")?;
    Ok(crate::syscall::registry::register(child))
}

/// Install `forgotten`'s space and take it off again, forget its root as a
/// new space's would be, then install `next`'s: answers how many barriers
/// that install decided.
fn after_a_forgotten_root(forgotten: &Process, next: &Process) -> u64 {
    let saved = <arch::Irq as IrqControl>::disable();
    let cpu = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    // SAFETY: (TRANSLATE) as in `switch_and_back`: both held throughout,
    // interrupts masked, no user address touched, and back to no user space.
    unsafe { forgotten.core().space().install(None) };
    // SAFETY: (TRANSLATE) as above.
    unsafe { forgotten.core().space().uninstall() };
    arch::forget_root(forgotten.core().space().root_table());
    let before = arch::barrier_decisions_on(cpu);
    // SAFETY: (TRANSLATE) as above.
    unsafe { next.core().space().install(None) };
    let decided = arch::barrier_decisions_on(cpu).saturating_sub(before);
    // SAFETY: (TRANSLATE) as above.
    unsafe { next.core().space().uninstall() };
    <arch::Irq as IrqControl>::restore(saved);
    decided
}

/// Set by the parked task once it runs in the leaver's space: see
/// [`check_leaving_at_once`].
static PARKED: AtomicBool = AtomicBool::new(false);
/// Set by the check to let the parked task end.
static RELEASED: AtomicBool = AtomicBool::new(false);

/// The task case 8 parks on another processor, in the leaver's space: it
/// runs, says so, and spins until released, so that its processor's last
/// space is the leaver's.
fn park(_: usize) {
    PARKED.store(true, Ordering::Release);
    while !RELEASED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

/// How long case 8 waits for the task it parks to run, or to go.
const PATIENCE_NANOS: u64 = 10_000_000_000;

/// Case 8, the consultant's F1: a member that leaves makes every processor
/// whose last space was in its domain issue the barrier before
/// `leave_speculation_domain` returns -- this one, and another running a
/// thread in the leaver's space -- not at their next switch. Answers whether
/// another processor took part.
fn check_leaving_at_once(
    first: &Arc<Process>,
    marked: &Arc<Job>,
    made: &mut alloc::vec::Vec<Arc<Process>>,
) -> Result<bool, &'static str> {
    let leaver = born_in(marked)?;
    made.push(Arc::clone(&leaver));
    let me = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    let parked = park_elsewhere(me, &leaver)?;

    // This processor's last space a member's, the leaver's.
    let saved = <arch::Irq as IrqControl>::disable();
    // SAFETY: (TRANSLATE) as in `switch_and_back`.
    unsafe { first.core().space().install(None) };
    // SAFETY: (TRANSLATE) as above.
    unsafe { leaver.core().space().install(Some(first.core().space())) };
    // SAFETY: (TRANSLATE) as above.
    unsafe { leaver.core().space().uninstall() };
    <arch::Irq as IrqControl>::restore(saved);
    let decided = |cpu: Option<usize>| cpu.map_or(0, arch::barrier_decisions_on);
    let other = parked.as_ref().map(|&(_, other)| other);
    let (here, there) = (decided(Some(me)), decided(other));

    leaver.core().leave_speculation_domain();

    let (here_now, there_now) = (decided(Some(me)), decided(other));
    RELEASED.store(true, Ordering::Release);
    if let Some((task, _)) = parked.as_ref() {
        crate::sched::wait_until_gone(task, PATIENCE_NANOS)?;
    }
    if here_now <= here {
        return Err(
            "case 8: a member that left its domain issued no barrier on this processor before it went on",
        );
    }
    if other.is_some() && there_now <= there {
        return Err(
            "case 8: a member that left its domain issued no barrier on the processor running its thread",
        );
    }
    Ok(other.is_some())
}

/// Start a kernel task in `leaver`'s space on a processor other than `me`,
/// and wait until it runs there: answers it and its processor, or `None` on
/// a machine with one.
fn park_elsewhere(
    me: usize,
    leaver: &Process,
) -> Result<Option<(Arc<crate::sched::Task>, usize)>, &'static str> {
    let Some(other) = crate::smp::topology().and_then(|topology| {
        topology
            .cpus()
            .iter()
            .find(|cpu| cpu.is_online() && cpu.logical != me)
            .map(|cpu| cpu.logical)
    }) else {
        return Ok(None);
    };
    PARKED.store(false, Ordering::Release);
    RELEASED.store(false, Ordering::Release);
    let mut only = CpuSet::empty();
    only.insert(other)
        .map_err(|_| "case 8 names a processor out of range")?;
    let task = crate::sched::spawn_on_in(
        "domain-park",
        park,
        0,
        NICE_0_WEIGHT,
        other,
        only,
        Some(Arc::clone(leaver.core().space())),
    )?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !PARKED.load(Ordering::Acquire) {
        if crate::timer::now_nanos() > deadline {
            RELEASED.store(true, Ordering::Release);
            return Err("case 8: the task parked in the leaver's space never ran");
        }
        crate::sched::yield_now();
    }
    Ok(Some((task, other)))
}

/// What case 11's task on the other processor installs once the leave's scan
/// has passed: a member's space, so that its processor records the domain.
static LATE_MEMBER: crate::sync::SpinLock<Option<Arc<AddressSpace>>> =
    crate::sync::SpinLock::new(None);
/// A space in no domain, which case 11's task installs first, so that its
/// processor's last domain is none when the scan reads it.
static LATE_NONE: crate::sync::SpinLock<Option<Arc<AddressSpace>>> =
    crate::sync::SpinLock::new(None);
/// Set by case 11's task once its processor's last domain is none.
static LATE_READY: AtomicBool = AtomicBool::new(false);
/// Set by the hook, after the scan, to have the task record the domain.
static LATE_GO: AtomicBool = AtomicBool::new(false);
/// Set by the task once it has.
static LATE_RECORDED: AtomicBool = AtomicBool::new(false);
/// Set once, by the check, when the hook is to act: the hook does nothing
/// in any other leave, even one a missing disarm let it see.
static LATE_ARMED: AtomicBool = AtomicBool::new(false);
/// The barrier decisions case 11's processor had made once it recorded the
/// domain, its own install's among them.
static LATE_DECIDED: AtomicU64 = AtomicU64::new(0);
/// Set by the hook when the task did not record the domain in time.
static LATE_TIMED_OUT: AtomicBool = AtomicBool::new(false);
/// The processor case 11's task runs on.
static LATE_CPU: AtomicUsize = AtomicUsize::new(0);
/// That processor's last domain as the hook saw it once the task had
/// recorded: the leaver's, which the scan before it had not found there.
static LATE_SEEN: AtomicU64 = AtomicU64::new(0);

/// Case 11's hook, which the leave runs between its scan and its grace
/// period.
static LATE_HOOK: arch::CheckHook = arch::CheckHook {
    armed_by: "the speculation domain check's case 11",
    run: record_after_the_scan,
};

/// The hook: let the task on the other processor record the domain now,
/// after the scan, and wait until it has.
fn record_after_the_scan() {
    if !LATE_ARMED.swap(false, Ordering::SeqCst) {
        return;
    }
    LATE_GO.store(true, Ordering::SeqCst);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !LATE_RECORDED.load(Ordering::SeqCst) {
        if crate::timer::now_nanos() > deadline {
            LATE_TIMED_OUT.store(true, Ordering::SeqCst);
            return;
        }
        core::hint::spin_loop();
    }
    LATE_SEEN.store(
        arch::last_domain_on(LATE_CPU.load(Ordering::SeqCst)),
        Ordering::SeqCst,
    );
}

/// The task case 11 runs on the other processor: its last domain made none,
/// then, at the hook's word, a member's space installed and left, as a
/// processor that read the leaver's space before the leave would have
/// recorded it; then it spins with interrupts on, so that it answers the
/// leave's grace period, until released.
fn record_late(_: usize) {
    let none = LATE_NONE.lock().clone();
    let member = LATE_MEMBER.lock().clone();
    let (Some(none), Some(member)) = (none, member) else {
        LATE_READY.store(true, Ordering::SeqCst);
        return;
    };
    let cpu = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    let saved = <arch::Irq as IrqControl>::disable();
    // SAFETY: (TRANSLATE) as in `switch_and_back`: the check holds both
    // spaces past this task's end, interrupts are masked, no user address
    // is touched, and the processor is back to no user space after.
    unsafe { none.install(None) };
    // SAFETY: (TRANSLATE) as above.
    unsafe { none.uninstall() };
    <arch::Irq as IrqControl>::restore(saved);
    LATE_READY.store(true, Ordering::SeqCst);
    while !LATE_GO.load(Ordering::SeqCst) {
        if RELEASED.load(Ordering::SeqCst) {
            return;
        }
        core::hint::spin_loop();
    }
    let saved = <arch::Irq as IrqControl>::disable();
    // SAFETY: (TRANSLATE) as above.
    unsafe { member.install(None) };
    // SAFETY: (TRANSLATE) as above.
    unsafe { member.uninstall() };
    LATE_DECIDED.store(arch::barrier_decisions_on(cpu), Ordering::SeqCst);
    <arch::Irq as IrqControl>::restore(saved);
    LATE_RECORDED.store(true, Ordering::SeqCst);
    while !RELEASED.load(Ordering::SeqCst) {
        core::hint::spin_loop();
    }
}

/// Case 11, the certification finding F-60: a processor that records the
/// leaver's domain after the leave's scan has passed it -- one that read the
/// leaver's space before the leave stored `OUT`, as the store-buffer pattern
/// allows on x86-64 and ARMv7-A -- still issues the barrier before
/// `leave_speculation_domain` returns, by its own compare as it answers the
/// grace period. Here the late record is made to happen by a hook between
/// the scan and the grace period, and the processor's last domain is a
/// member's, `first`'s, as it would be after a switch from the leaver to a
/// member. Answers whether it ran, which needs a second processor.
fn check_recorded_after_the_scan(
    first: &Arc<Process>,
    marked: &Arc<Job>,
    made: &mut alloc::vec::Vec<Arc<Process>>,
) -> Result<bool, &'static str> {
    let me = crate::smp::this_cpu().map_or(0, |cpu| cpu.logical);
    let Some(other) = crate::smp::topology().and_then(|topology| {
        topology
            .cpus()
            .iter()
            .find(|cpu| cpu.is_online() && cpu.logical != me)
            .map(|cpu| cpu.logical)
    }) else {
        return Ok(false);
    };
    let leaver = born_in(marked)?;
    made.push(Arc::clone(&leaver));
    let none = AddressSpace::new().map_err(|_| "no address space for case 11")?;
    *LATE_NONE.lock() = Some(none);
    *LATE_MEMBER.lock() = Some(Arc::clone(first.core().space()));
    for flag in [
        &LATE_READY,
        &LATE_GO,
        &LATE_RECORDED,
        &LATE_TIMED_OUT,
        &RELEASED,
    ] {
        flag.store(false, Ordering::SeqCst);
    }
    LATE_CPU.store(other, Ordering::SeqCst);
    LATE_SEEN.store(0, Ordering::SeqCst);
    let mut only = CpuSet::empty();
    only.insert(other)
        .map_err(|_| "case 11 names a processor out of range")?;
    let task = crate::sched::spawn_on_in(
        "domain-late",
        record_late,
        0,
        NICE_0_WEIGHT,
        other,
        only,
        None,
    )?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !LATE_READY.load(Ordering::SeqCst) {
        if crate::timer::now_nanos() > deadline {
            RELEASED.store(true, Ordering::SeqCst);
            return Err("case 11: the task on the other processor never ran");
        }
        crate::sched::yield_now();
    }
    if arch::last_domain_on(other) != 0 {
        RELEASED.store(true, Ordering::SeqCst);
        return Err("case 11: the other processor's last domain was not none before the leave");
    }

    LATE_ARMED.store(true, Ordering::SeqCst);
    arch::arm_leave_hook(&LATE_HOOK);
    leaver.core().leave_speculation_domain();
    arch::disarm_leave_hook();
    LATE_ARMED.store(false, Ordering::SeqCst);

    let recorded = LATE_RECORDED.load(Ordering::SeqCst);
    let before = LATE_DECIDED.load(Ordering::SeqCst);
    let after = arch::barrier_decisions_on(other);
    RELEASED.store(true, Ordering::SeqCst);
    crate::sched::wait_until_gone(&task, PATIENCE_NANOS)?;
    *LATE_NONE.lock() = None;
    *LATE_MEMBER.lock() = None;
    if LATE_TIMED_OUT.load(Ordering::SeqCst)
        || !recorded
        || LATE_SEEN.load(Ordering::SeqCst) != marked.domain()
    {
        return Err("case 11: the other processor did not record the domain after the scan");
    }
    if after <= before {
        return Err(
            "case 11: a processor that recorded the domain after the leave's scan issued no barrier before the leave returned",
        );
    }
    Ok(true)
}
