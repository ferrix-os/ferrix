//! The large path, proved at boot (`docs/ROBUSTNESS.md` §2.6 (i),
//! `L.mm.141`-`L.mm.145`).
//!
//! The buddy is made to refuse the checking task's runs of frames alone
//! ([`super::refuse_runs_for`]), which is what a machine whose free memory is
//! in single frames answers it: no frame is held, so nothing else on the
//! machine meets a buddy short of runs. Then a heap scope's 32 KiB request is
//! served from frames drawn from its reservation, freed with interrupts
//! masked with no purge made, and purged; a 128 KiB request is served; a
//! request made and freed while `TABLES` is held is served; a free past the
//! lazy threshold that cannot block hands its purge to the reaper; so does
//! a reservation's unbind while frames are kept; and frees past the
//! threshold made where they may block are purged by the next allocation.

use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::{MapFlags, VirtAddr};
use ferrix_sync::IrqControl;

use super::large;

/// Frames the scoped case reserves.
const SCOPE_FRAMES: u64 = 64;

/// Rounds of each measured case.
const ROUNDS: u64 = 1000;

/// The scoped request: 32 KiB, eight pages.
const SMALL: usize = 32 * 1024;

/// RD.6 (g)'s request: 128 KiB, an order-5 run on the buddy.
const MEDIUM: usize = 128 * 1024;

/// R6.5's request: the largest the heap serves, 4 MiB.
const LARGEST: usize = (PAGE_SIZE as usize) << super::MAX_ORDER;

// CHECKS-ONLY: stages_check.rs
/// What the check measured, for its line.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LargeReport {
    /// The area's bytes.
    pub bytes: u64,
    /// A 32 KiB allocation and free through the buddy, in nanoseconds a round.
    pub buddy_nanos: u64,
    /// A 32 KiB allocation and free by the large path itself.
    pub large_nanos: u64,
    /// The 4 MiB allocation with interrupts masked, in nanoseconds.
    pub largest_allocate_nanos: u64,
    /// Its free, masked.
    pub largest_free_nanos: u64,
    /// The purge after it.
    pub largest_purge_nanos: u64,
    /// How long the reaper took to purge for a masked free, in nanoseconds.
    pub reaper_nanos: u64,
    /// The most frames ever held for a shootdown at once.
    pub most_kept: u64,
}

/// Puts the buddy back to serving this task's runs, on every exit.
struct RunsRefused;

impl Drop for RunsRefused {
    fn drop(&mut self) {
        super::refuse_runs_for(0);
    }
}

/// `body` with interrupts masked.
fn masked<T>(body: impl FnOnce() -> T) -> T {
    let saved = <crate::arch::Irq as IrqControl>::disable();
    let answer = body();
    <crate::arch::Irq as IrqControl>::restore(saved);
    answer
}

/// Whether `address` lies in the area.
fn in_area(address: u64) -> bool {
    let (base, end) = large::span();
    (base..end).contains(&address)
}

/// `bytes` from the heap, fallibly.
fn take(bytes: usize) -> Option<Vec<u8>> {
    crate::fallible::try_with_capacity::<u8>(bytes).ok()
}

// CHECKS-ONLY: stages_check.rs
/// Run the check.
///
/// # Errors
///
/// The first property that does not hold.
///
/// Verifies: L.mm.142, L.mm.144
#[doc(hidden)]
pub fn run() -> Result<LargeReport, &'static str> {
    let (base, end) = large::span();
    if base == 0 {
        return Err("the large-request area is off");
    }
    if base % (2 << 20) != 0 || end % (2 << 20) != 0 {
        return Err("the large-request area is not aligned to 2 MiB");
    }
    let me = crate::sched::current_id().ok_or("the large path's check runs outside a task")?;
    let _ = large::purge();
    let _ = super::drain_caches();
    let free_before = super::free_frames();
    let out_before = large::counts().out_of_area;
    let mut report = LargeReport {
        bytes: end - base,
        ..LargeReport::default()
    };
    let outcome = cases(me, &mut report);
    super::refuse_runs_for(0);
    let _ = large::purge();
    let _ = super::drain_caches();
    outcome?;
    let counts = large::counts();
    // The two frees `refused_frees` makes are the only ones counted.
    if counts.out_of_area != out_before + REFUSED_FREES
        || counts.two_owners != 0
        || counts.bad_links != 0
    {
        return Err("the large path counted a free outside its area, two owners or a bad link");
    }
    report.most_kept = counts.most_kept;
    if counts.kept != 0 || counts.lazy_units != 0 {
        return Err("a purge left frames kept or blocks on the lazy list");
    }
    // Every page-level table of the area still linked: a walk that may make
    // none finds one over every 2 MiB (R6F.3).
    if large::mapper()
        .populate(
            &mut large::AreaTables,
            VirtAddr(base),
            end - base,
            MapFlags::KERNEL_DATA,
        )
        .is_err()
    {
        return Err("a page-level table of the large-request area was unlinked");
    }
    // Fewer free than before is a leak; more is something else on the
    // machine giving frames back meanwhile, which `print_frame_delta` reports
    // as other checks' do. A short pause and a second look, a few times,
    // before a shortfall is called a leak: another task may be holding a
    // frame for a moment.
    let mut leaked = 0;
    for _ in 0..5 {
        leaked = i64::try_from(free_before).unwrap_or(i64::MAX)
            - i64::try_from(super::free_frames()).unwrap_or(i64::MAX);
        if leaked <= 0 {
            break;
        }
        crate::sched::sleep_for(10_000_000);
        let _ = super::drain_caches();
    }
    super::print_frame_delta("large", leaked);
    if leaked > 0 {
        return Err("the large path's check did not give back every frame it took");
    }
    Ok(report)
}

/// The cases, in order, with the buddy refusing this task's runs from step 3.
///
/// Verifies: L.mm.141, L.mm.145
fn cases(me: u64, report: &mut LargeReport) -> Result<(), &'static str> {
    // First, so that its own line is what fails without the allocation's
    // purge, which `measure`'s masked 4 MiB request needs as well.
    an_allocation_purges_when_asked()?;
    measure(me, report)?;
    scoped(me)?;
    let _refused = RunsRefused;
    super::refuse_runs_for(me);
    // RD.6 (g): 128 KiB outside any scope, from the kernel's frames.
    let medium =
        take(MEDIUM).ok_or("the 128 KiB request was not served while the buddy refused")?;
    if !in_area(medium.as_ptr() as u64) {
        return Err("the 128 KiB request was not served from the area");
    }
    drop(medium);
    refused_frees()?;
    // R6.3: made and freed while this processor holds `TABLES`.
    let under_tables = super::with_tables(|_| {
        let object = take(SMALL);
        let served = object
            .as_ref()
            .is_some_and(|object| in_area(object.as_ptr() as u64));
        drop(object);
        served
    });
    if !under_tables {
        return Err("a request made while TABLES was held was not served from the area");
    }
    let _ = large::purge();
    reaper_purges(report)?;
    an_unbind_asks()
}

/// The frees [`refused_frees`] makes, each counted.
const REFUSED_FREES: u64 = 2;

/// A free of what the area did not hand out changes nothing, and is counted
/// (ledger 8583 B1): a dynamic-space address outside the area, whose mapping
/// still translates to its frame after, and an address inside a live block
/// that is not its base, whose block still translates whole.
///
/// Verifies: L.mm.142
fn refused_frees() -> Result<(), &'static str> {
    let unit = ferrix_frame::area::UNIT_PAGES * PAGE_SIZE;
    let counted = large::counts().out_of_area;
    let mapping = crate::vmap::allocate(2, MapFlags::KERNEL_DATA)
        .map_err(|_| "no vmap pages for the out-of-area free")?;
    let page = if mapping.base.is_multiple_of(unit) {
        mapping.base
    } else {
        mapping.base + PAGE_SIZE
    };
    let frame = super::translate(page);
    large::free(page, SMALL);
    let untouched = frame.is_some() && super::translate(page) == frame;
    let _ = crate::vmap::free(mapping.base);
    if !untouched {
        return Err("a free outside the area cleared a mapping it did not own");
    }
    if large::counts().out_of_area != counted + 1 {
        return Err("a free outside the area was not counted");
    }
    let object = take(SMALL).ok_or("the 32 KiB request was not served while the buddy refused")?;
    let base = object.as_ptr() as u64;
    let pages = (SMALL as u64) / PAGE_SIZE;
    let before: Vec<Option<u64>> = (0..pages)
        .map(|page| super::translate(base + page * PAGE_SIZE))
        .collect();
    large::free(base + unit, SMALL);
    let whole = (0..pages).all(|page| {
        let now = super::translate(base + page * PAGE_SIZE);
        now.is_some() && before.get(page as usize) == Some(&now)
    });
    drop(object);
    if !whole {
        return Err("a free inside a live block, not at its base, cleared part of it");
    }
    if large::counts().out_of_area != counted + REFUSED_FREES {
        return Err("a free inside a live block, not at its base, was not counted");
    }
    Ok(())
}

/// Step 1: a 32 KiB round through the buddy and through the large path, and
/// R6.5's 4 MiB case with interrupts masked.
///
/// Verifies: L.mm.141, L.mm.145
fn measure(me: u64, report: &mut LargeReport) -> Result<(), &'static str> {
    let rounds = |report_into: &mut u64| -> Result<(), &'static str> {
        let start = crate::timer::now_nanos();
        for _ in 0..ROUNDS {
            drop(take(SMALL).ok_or("a measured 32 KiB request was refused")?);
        }
        *report_into = crate::timer::now_nanos().saturating_sub(start) / ROUNDS;
        Ok(())
    };
    rounds(&mut report.buddy_nanos)?;
    // The large path's own cost, without the drain of every processor's
    // cache that comes before it in the heap (`heap_or_drain`).
    let request = ferrix_heap::Request::new(SMALL, 8);
    let start = crate::timer::now_nanos();
    for _ in 0..ROUNDS {
        let address = large::allocate(request, super::reservation::Floor::Kernel)
            .ok_or("a measured 32 KiB request was refused by the large path")?;
        large::free(address, SMALL);
    }
    report.large_nanos = crate::timer::now_nanos().saturating_sub(start) / ROUNDS;
    let _refused = RunsRefused;
    super::refuse_runs_for(me);
    let served = large::counts().served;
    drop(take(SMALL).ok_or("the 32 KiB request was not served while the buddy refused")?);
    if large::counts().served <= served {
        return Err("the 32 KiB request was not served while the buddy refused");
    }
    let start = crate::timer::now_nanos();
    let largest = masked(|| take(LARGEST));
    report.largest_allocate_nanos = crate::timer::now_nanos().saturating_sub(start);
    let largest = largest.ok_or("the 4 MiB request was refused while the buddy refused")?;
    if !in_area(largest.as_ptr() as u64) {
        return Err("the 4 MiB request was not served from the area");
    }
    let start = crate::timer::now_nanos();
    masked(|| drop(largest));
    report.largest_free_nanos = crate::timer::now_nanos().saturating_sub(start);
    let start = crate::timer::now_nanos();
    let _ = large::purge();
    report.largest_purge_nanos = crate::timer::now_nanos().saturating_sub(start);
    Ok(())
}

/// Steps 2 to 6: a 32 KiB request by a task of a job with a reservation, drawn
/// from it (a heap scope sends a request above a page as outside it, L.mm.140),
/// freed masked with no purge, and purged.
///
/// Verifies: L.mm.141, L.mm.142, L.mm.143, L.mm.81
fn scoped(me: u64) -> Result<(), &'static str> {
    let tree = crate::object::job::Job::new_root().map_err(|_| "no memory for a job")?;
    let job = tree.new_child().map_err(|_| "no memory for a job")?;
    job.set_reservation(SCOPE_FRAMES * PAGE_SIZE)
        .map_err(|_| "the large path's reservation was refused")?;
    let entry = job.draws_from();
    if entry == super::NO_RESERVATION {
        return Err("a job with a reservation drew from none");
    }
    let own = crate::sched::running_group();
    crate::sched::set_current_group(job.quota_index());
    let outcome = (|| {
        let (_, drawn_before) = super::reservation_usage(entry);
        let _refused = RunsRefused;
        super::refuse_runs_for(me);
        if super::allocate_frames(1).is_some()
            || super::take_frames(1, super::reservation::Floor::Drawn(entry)).is_some()
        {
            return Err("the buddy did not refuse the checking task's runs");
        }
        let object =
            take(SMALL).ok_or("the 32 KiB request was not served while the buddy refused")?;
        let base = object.as_ptr() as u64;
        if !in_area(base) {
            return Err("the 32 KiB request was not served from the area");
        }
        let pages = (SMALL as u64) / PAGE_SIZE;
        for page in 0..pages {
            let frame = super::translate(base + page * PAGE_SIZE)
                .ok_or("a page of a large block did not translate")?
                / PAGE_SIZE;
            if super::drawn_from(frame) != entry {
                return Err("a large block's frame was not drawn from its job's reservation");
            }
        }
        if super::translate(base + pages * PAGE_SIZE).is_some() {
            return Err("the guard page above a large block translated");
        }
        if super::reservation_usage(entry).1 != drawn_before + pages {
            return Err("a large block did not draw its pages from its job's reservation");
        }
        let purges = large::counts().purges;
        masked(|| drop(object));
        let after = large::counts();
        if after.purges != purges {
            return Err("a masked free began a shootdown");
        }
        if after.lazy_units == 0 {
            return Err("a freed large block did not wait on the lazy list");
        }
        let kept = !crate::arch::TLB_FLUSH_IS_BROADCAST;
        let expected = if kept {
            drawn_before + pages
        } else {
            drawn_before
        };
        if super::reservation_usage(entry).1 != expected {
            return Err(if kept {
                "a masked free gave its frames back before their shootdown"
            } else {
                "a masked free did not give its frames back to its job's reservation"
            });
        }
        if !large::purge() || large::counts().lazy_units != 0 {
            return Err("a purge in task context did not empty the lazy list");
        }
        if super::reservation_usage(entry).1 != drawn_before {
            return Err(
                "a purge did not give a large block's frames back to its job's reservation",
            );
        }
        Ok(())
    })();
    crate::sched::set_current_group(own);
    drop((job, tree));
    outcome
}

/// Step 9: frees past the lazy threshold made with interrupts masked begin no
/// purge, nor does a request made masked after them, which finds the purge
/// wanted; and the reaper purges once interrupts are on again, within a
/// bounded wait.
///
/// Verifies: L.mm.143
fn reaper_purges(report: &mut LargeReport) -> Result<(), &'static str> {
    const LOW: u64 = 16;
    const PATIENCE_NANOS: u64 = 2_000_000_000;
    let threshold = large::set_lazy_max_for_check(LOW);
    let purges = large::counts().purges;
    // Then, still masked, one more request: it finds the purge wanted where
    // it may not block, and is served or refused with none begun.
    let (made, wanted, unpurged) = masked(|| {
        for _ in 0..3 {
            let Some(object) = take(SMALL) else {
                return (false, false, false);
            };
            drop(object);
        }
        let wanted = large::purge_wanted();
        drop(take(SMALL));
        (true, wanted, large::counts().purges == purges)
    });
    let start = crate::timer::now_nanos();
    let mut purged = false;
    while crate::timer::now_nanos().saturating_sub(start) < PATIENCE_NANOS {
        if large::counts().lazy_units == 0 && large::counts().purges > purges {
            purged = true;
            break;
        }
        crate::sched::sleep_for(1_000_000);
    }
    report.reaper_nanos = crate::timer::now_nanos().saturating_sub(start);
    let _ = large::set_lazy_max_for_check(threshold);
    if !made {
        return Err("a masked 32 KiB request was refused while the buddy refused");
    }
    if !wanted {
        return Err("a masked free past the lazy threshold did not hand its purge to the reaper");
    }
    if !unpurged {
        return Err("a masked free or a masked request that found a purge wanted began one");
    }
    if !purged {
        return Err("the reaper did not purge within two seconds of a masked free's asking");
    }
    Ok(())
}

/// Step 9a: a reservation's unbind while frames are kept asks the reaper for
/// a purge and begins none (F-104). With interrupts masked, so that no purge
/// comes between: a 32 KiB block made and freed, which keeps its frames
/// where the invalidation is local and none where it is broadcast, and then
/// an entry bound and unbound. The flag is set where frames are kept and
/// stays clear where none are.
///
/// Verifies: L.mm.143
fn an_unbind_asks() -> Result<(), &'static str> {
    const PATIENCE_NANOS: u64 = 2_000_000_000;
    let kept = !crate::arch::TLB_FLUSH_IS_BROADCAST;
    let _ = large::purge();
    let purges = large::counts().purges;
    let seen = masked(|| {
        drop(take(SMALL)?);
        let before = large::purge_wanted();
        let entry = super::reservation_bind()?;
        super::reservation_unbind(entry);
        Some((before, large::purge_wanted(), large::counts().purges))
    });
    // The block's addresses go back by the reaper where it was asked, and by
    // this task where it was not.
    let start = crate::timer::now_nanos();
    while large::counts().lazy_units != 0
        && crate::timer::now_nanos().saturating_sub(start) < PATIENCE_NANOS
    {
        if !kept {
            let _ = large::purge();
        }
        crate::sched::sleep_for(1_000_000);
    }
    let Some((before, after, purges_after)) = seen else {
        return Err("a masked 32 KiB request or a reservation was refused before an unbind");
    };
    if before {
        return Err("a masked free under the lazy threshold asked for a purge");
    }
    if purges_after != purges {
        return Err("a reservation's unbind began a shootdown");
    }
    if after != kept {
        return Err(if kept {
            "a reservation's unbind while frames were kept did not ask the reaper for a purge"
        } else {
            "a reservation's unbind with no frame kept asked for a purge"
        });
    }
    if large::counts().lazy_units != 0 {
        return Err("the reaper did not purge within two seconds of an unbind's asking");
    }
    Ok(())
}

/// Before step 1: a free past the lazy threshold made where it may block makes no
/// purge itself, and the next allocation that may block makes it. Over
/// rounds of a 32 KiB block made and freed by the large path in task
/// context, with the threshold lowered, the lazy list never holds more than
/// the threshold and one block: a list left to the reaper's turn alone
/// would hold three blocks after the third round.
///
/// Verifies: L.mm.143
fn an_allocation_purges_when_asked() -> Result<(), &'static str> {
    const LOW: u64 = 16;
    const BOUNDED_ROUNDS: u64 = 64;
    let request = ferrix_heap::Request::new(SMALL, 8);
    let round = || {
        let address = large::allocate(request, super::reservation::Floor::Kernel)?;
        large::free(address, SMALL);
        Some(large::counts().lazy_units)
    };
    let _ = large::purge();
    // What one block puts on the list.
    let one = round();
    let _ = large::purge();
    let threshold = large::set_lazy_max_for_check(LOW);
    let mut most = Some(0);
    for _ in 0..BOUNDED_ROUNDS {
        most = most.zip(round()).map(|(most, now)| most.max(now));
        if most.is_none() {
            break;
        }
    }
    let _ = large::set_lazy_max_for_check(threshold);
    let _ = large::purge();
    let (Some(one), Some(most)) = (one, most) else {
        return Err("a 32 KiB request in task context was refused by the large path");
    };
    if one == 0 {
        return Err("a freed large block did not wait on the lazy list");
    }
    if most > LOW + one {
        return Err(
            "frees past the lazy threshold in task context were not purged by the next allocation",
        );
    }
    Ok(())
}
