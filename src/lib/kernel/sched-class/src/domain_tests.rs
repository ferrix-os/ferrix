//! Tests for the domains, load tracking, placement and balancing.

extern crate std;

use std::vec::Vec;

use super::*;

// ---------------------------------------------------------------------------
// Domains and modes
// ---------------------------------------------------------------------------

#[test]
fn throughput_is_fair_then_idle_and_steals() {
    assert_eq!(
        Mode::Throughput.classes(),
        Ok(&[Class::Fair, Class::Idle][..]),
        "the Throughput stack"
    );
    assert!(Mode::Throughput.steals_work(), "Throughput steals");
}

#[test]
fn soft_real_time_is_fixed_priorities_over_fair_and_does_not_steal() {
    assert_eq!(
        Mode::SoftRt.classes(),
        Ok(&[Class::Realtime, Class::Fair, Class::Idle][..]),
        "the SoftRt stack"
    );
    assert!(!Mode::SoftRt.steals_work(), "SoftRt does not steal");
    let cpus = CpuSet::first(2).unwrap();
    assert_eq!(
        Domain::new(cpus, Mode::SoftRt).map(|domain| domain.mode()),
        Ok(Mode::SoftRt),
        "and a domain can be built in it"
    );
    assert_eq!(
        Domain::new(CpuSet::empty(), Mode::SoftRt),
        Err(DomainError::EmptyDomain),
        "of at least one CPU"
    );
}

#[test]
fn hard_real_time_is_named_and_refused() {
    let mode = Mode::HardRt;
    assert_eq!(
        mode.classes(),
        Err(DomainError::ModeUnavailable(mode)),
        "HardRt is stage 14's"
    );
    assert!(!mode.steals_work(), "HardRt does not steal");
    assert_eq!(
        Domain::new(CpuSet::first(1).unwrap(), mode),
        Err(DomainError::ModeUnavailable(mode)),
        "and no domain can be built in it"
    );
}

#[test]
fn a_cpu_set_holds_what_it_is_given_and_nothing_else() {
    let mut set = CpuSet::empty();
    assert!(set.is_empty(), "starts empty");
    for cpu in [0, 1, 63, 64, 200, MAX_CPUS - 1] {
        set.insert(cpu).unwrap();
    }
    assert_eq!(set.len(), 6, "six in");
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        [0, 1, 63, 64, 200, MAX_CPUS - 1],
        "lowest first"
    );
    assert!(!set.contains(2), "not given");
    assert_eq!(
        set.insert(MAX_CPUS),
        Err(DomainError::NoSuchCpu(MAX_CPUS)),
        "past the end"
    );
    assert!(!set.contains(MAX_CPUS), "and not there");
}

#[test]
fn an_empty_domain_is_refused() {
    assert_eq!(
        Domain::new(CpuSet::empty(), Mode::Throughput),
        Err(DomainError::EmptyDomain),
        "no CPUs"
    );
}

#[test]
fn a_partition_must_hold_every_cpu_exactly_once() {
    let everything = Domain::new(CpuSet::first(4).unwrap(), Mode::Throughput).unwrap();
    assert_eq!(
        check_partition(&[everything], 4),
        Ok(()),
        "one domain, all of them"
    );

    let low = Domain::new(CpuSet::first(2).unwrap(), Mode::Throughput).unwrap();
    assert_eq!(
        check_partition(&[low], 4),
        Err(DomainError::Uncovered(2)),
        "two CPUs in no domain"
    );
    assert_eq!(
        check_partition(&[low, everything], 4),
        Err(DomainError::Overlap(0)),
        "two domains claiming CPU 0"
    );
    assert_eq!(
        check_partition(&[everything], 3),
        Err(DomainError::NoSuchCpu(3)),
        "a domain naming a CPU the machine lacks"
    );

    let mut high = CpuSet::empty();
    high.insert(2).unwrap();
    high.insert(3).unwrap();
    let high = Domain::new(high, Mode::Throughput).unwrap();
    assert_eq!(check_partition(&[low, high], 4), Ok(()), "two halves");
}

// ---------------------------------------------------------------------------
// Load tracking, placement and balancing
// ---------------------------------------------------------------------------

use crate::{
    BALANCE_THRESHOLD, CpuLoad, LOAD_PERIOD_NS, LOAD_SCALE, Load, busiest, imbalance, place,
    quietest,
};

/// A set naming every CPU below `count`.
fn all_cpus(count: usize) -> CpuSet {
    CpuSet::first(count).expect("a set of that many CPUs")
}

/// A set naming exactly the CPUs listed.
fn only(cpus: &[usize]) -> CpuSet {
    let mut set = CpuSet::default();
    for cpu in cpus {
        set.insert(*cpu).expect("a CPU inside the set");
    }
    set
}

/// A busy processor with a given average.
fn busy(average: u64, queued: usize) -> CpuLoad {
    CpuLoad {
        queued,
        average,
        idle: false,
    }
}

#[test]
fn a_load_that_is_always_busy_converges_on_full() {
    let mut load = Load::new();
    for _ in 0..512 {
        load.accumulate(LOAD_PERIOD_NS, LOAD_SCALE);
    }
    // Within a percent of full: the series converges to LOAD_SCALE and this
    // many periods is many half-lives.
    assert!(
        load.average() > LOAD_SCALE * 99 / 100,
        "a permanently busy load read {} of {LOAD_SCALE}",
        load.average()
    );
    assert!(load.average() <= LOAD_SCALE, "a load exceeded full");
}

#[test]
fn a_load_that_is_never_busy_stays_at_zero() {
    let mut load = Load::new();
    for _ in 0..64 {
        load.accumulate(LOAD_PERIOD_NS, 0);
    }
    assert_eq!(load.average(), 0, "an idle load drifted upwards");
}

#[test]
fn a_half_busy_load_converges_on_half() {
    // Half the time at full demand, half at none — a duty cycle, expressed
    // as the level it actually is rather than as an amount.
    let mut load = Load::new();
    for _ in 0..512 {
        load.accumulate(LOAD_PERIOD_NS / 2, LOAD_SCALE);
        load.accumulate(LOAD_PERIOD_NS / 2, 0);
    }
    let half = LOAD_SCALE / 2;
    assert!(
        load.average().abs_diff(half) < LOAD_SCALE / 50,
        "a half-busy load read {} rather than about {half}",
        load.average()
    );
}

#[test]
fn how_finely_time_is_chopped_does_not_change_the_answer() {
    // The property that lets the kernel call this from a tick, a context
    // switch or an idle transition without the answer depending on which.
    let mut coarse = Load::new();
    let mut fine = Load::new();
    for _ in 0..64 {
        coarse.accumulate(LOAD_PERIOD_NS, LOAD_SCALE / 4);
        for _ in 0..8 {
            fine.accumulate(LOAD_PERIOD_NS / 8, LOAD_SCALE / 4);
        }
    }
    assert_eq!(
        coarse.average(),
        fine.average(),
        "the same history reported differently depending on how it was split"
    );
}

#[test]
fn a_load_decays_towards_zero_when_idle() {
    let mut load = Load::new();
    for _ in 0..256 {
        load.accumulate(LOAD_PERIOD_NS, LOAD_SCALE);
    }
    let busy_average = load.average();
    // Thirty-two periods is the half-life.
    load.decay(LOAD_PERIOD_NS * 32);
    assert!(
        load.average() < busy_average * 55 / 100 && load.average() > busy_average * 45 / 100,
        "after one half-life a load of {busy_average} read {}",
        load.average()
    );
}

#[test]
fn decaying_for_a_very_long_time_reaches_zero_without_looping_forever() {
    let mut load = Load::new();
    load.accumulate(LOAD_PERIOD_NS, LOAD_SCALE);
    load.decay(LOAD_PERIOD_NS * 1_000_000);
    assert_eq!(load.average(), 0, "a long idle period left load behind");
}

/// A year, in nanoseconds: far more load periods than any loop should walk.
const YEAR_NS: u64 = 365 * 24 * 3600 * 1_000_000_000;

#[test]
fn a_long_stretch_is_accounted_in_bounded_work() {
    // The kernel folds time in under its run queue lock, with interrupts
    // masked, and a processor that has been idle for an hour hands it an hour.
    // A loop over every elapsed period is three and a half million iterations
    // for that hour and thirty billion for this year, so the check is made on
    // a thread with a deadline: without the bound this does not fail, it hangs.
    let (sender, receiver) = std::sync::mpsc::channel();
    let _worker = std::thread::spawn(move || {
        let mut load = Load::new();
        load.accumulate(LOAD_PERIOD_NS * 64, LOAD_SCALE);
        load.accumulate(YEAR_NS, LOAD_SCALE * 3);
        let busy = load.average();
        load.accumulate(YEAR_NS, 0);
        let _ = sender.send((busy, load.average()));
    });
    let (busy, idle) = receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("accounting a year of load walked every period of it");
    // Within one unit, which is as close as the per-period loop itself settles
    // on a constant level: its truncating steps stop a unit short from below.
    assert!(
        busy.abs_diff(LOAD_SCALE * 3) <= 1,
        "a year at three nice-0 entities read {busy}"
    );
    assert_eq!(idle, 0, "a year idle left load behind");
}

#[test]
fn a_long_stretch_reads_as_the_same_stretch_in_short_pieces() {
    // The bound must not change the answer: one call covering many thousands
    // of periods reads as the same history handed over a few hundred periods
    // at a time, which never reaches the bound and so walks every period.
    let mut whole = Load::new();
    let mut pieces = Load::new();
    for load in [&mut whole, &mut pieces] {
        for _ in 0..64 {
            load.accumulate(LOAD_PERIOD_NS, LOAD_SCALE / 2);
        }
        load.accumulate(LOAD_PERIOD_NS / 3, LOAD_SCALE * 5);
    }
    whole.accumulate(LOAD_PERIOD_NS * 10_000, LOAD_SCALE * 2);
    for _ in 0..20 {
        pieces.accumulate(LOAD_PERIOD_NS * 500, LOAD_SCALE * 2);
    }
    assert!(
        whole.average().abs_diff(pieces.average()) <= 1,
        "one long call read {} where the same time in pieces read {}",
        whole.average(),
        pieces.average()
    );
}

#[test]
fn several_runnable_entities_read_as_more_than_one() {
    // The property the balancer needs and a busy/idle signal cannot give:
    // four runnable nice-0 entities are four times the demand of one, not
    // the same "busy".
    let mut one = Load::new();
    let mut four = Load::new();
    for _ in 0..512 {
        one.accumulate(LOAD_PERIOD_NS, LOAD_SCALE);
        four.accumulate(LOAD_PERIOD_NS, LOAD_SCALE * 4);
    }
    assert!(
        four.average() > one.average() * 7 / 2,
        "four runnable entities read {} against one entity's {}",
        four.average(),
        one.average()
    );
}

#[test]
fn placement_prefers_staying_put_when_the_preferred_cpu_is_idle() {
    let loads = [CpuLoad::idle(), CpuLoad::idle(), CpuLoad::idle()];
    assert_eq!(place(&loads, &all_cpus(3), 2), Some(2));
}

#[test]
fn placement_takes_an_idle_cpu_over_a_busy_preferred_one() {
    let loads = [busy(LOAD_SCALE, 4), CpuLoad::idle(), busy(LOAD_SCALE, 4)];
    assert_eq!(
        place(&loads, &all_cpus(3), 0),
        Some(1),
        "a task stayed on a busy processor while one sat idle"
    );
}

#[test]
fn placement_takes_the_least_loaded_when_none_is_idle() {
    let loads = [
        busy(LOAD_SCALE, 4),
        busy(LOAD_SCALE / 2, 2),
        busy(LOAD_SCALE / 4, 1),
    ];
    assert_eq!(place(&loads, &all_cpus(3), 0), Some(2));
}

#[test]
fn placement_never_leaves_the_affinity_mask() {
    let loads = [CpuLoad::idle(), CpuLoad::idle(), busy(LOAD_SCALE, 8)];
    // Only CPU 2 is allowed, and it is the worst choice by every other
    // measure. Affinity is not a preference.
    assert_eq!(place(&loads, &only(&[2]), 0), Some(2));
}

#[test]
fn placement_answers_nothing_when_the_mask_names_no_usable_cpu() {
    let loads = [CpuLoad::idle(), CpuLoad::idle()];
    assert_eq!(place(&loads, &only(&[7]), 0), None);
    assert_eq!(place(&loads, &CpuSet::default(), 0), None);
}

#[test]
fn placement_is_deterministic_on_a_tie() {
    let loads = [busy(500, 2), busy(500, 2), busy(500, 2)];
    // The lowest-numbered processor wins a tie, unless the tie includes the
    // one the task would rather have.
    assert_eq!(place(&loads, &all_cpus(3), 9), Some(0));
    assert_eq!(place(&loads, &all_cpus(3), 2), Some(2));
}

#[test]
fn a_processor_with_only_a_running_task_is_not_worth_taking_from() {
    // The whole of stage 5's stealing rule, kept: taking the one task a
    // processor is running is not balancing.
    let from = busy(LOAD_SCALE, 1);
    let to = CpuLoad::idle();
    assert_eq!(imbalance(&from, &to), None);
}

#[test]
fn a_small_difference_is_not_worth_a_migration() {
    let from = busy(LOAD_SCALE * 4 + BALANCE_THRESHOLD / 2, 6);
    let to = busy(LOAD_SCALE * 4, 3);
    assert_eq!(
        imbalance(&from, &to),
        None,
        "a difference below the threshold was judged worth moving"
    );
}

#[test]
fn a_large_difference_moves_half_of_it() {
    let from = busy(LOAD_SCALE * 8, 9);
    let to = busy(0, 1);
    assert_eq!(
        imbalance(&from, &to),
        Some(LOAD_SCALE * 4),
        "balancing moved something other than half the difference"
    );
}

#[test]
fn balancing_cannot_oscillate() {
    // The property the threshold exists for: after moving half the
    // difference, neither side wants to move it back.
    let from = busy(LOAD_SCALE * 8, 9);
    let to = busy(0, 1);
    let moved = imbalance(&from, &to).expect("an imbalance worth moving");
    let settled_from = busy(from.average - moved, 3);
    let settled_to = busy(to.average + moved, 2);
    assert_eq!(imbalance(&settled_from, &settled_to), None);
    assert_eq!(imbalance(&settled_to, &settled_from), None);
}

#[test]
fn the_busiest_processor_is_the_one_chosen() {
    let loads = [
        busy(0, 1),
        busy(LOAD_SCALE * 2, 3),
        busy(LOAD_SCALE * 8, 9),
        busy(LOAD_SCALE, 2),
    ];
    assert_eq!(busiest(&loads, &all_cpus(4), 0), Some(2));
}

#[test]
fn there_is_no_busiest_processor_when_everything_is_level() {
    let loads = [busy(500, 2), busy(500, 2), busy(500, 2)];
    assert_eq!(busiest(&loads, &all_cpus(3), 0), None);
}

#[test]
fn an_overloaded_processor_finds_somewhere_to_push_to() {
    // The case a tickless kernel depends on: processors 1..3 each run one
    // task and are never interrupted, so processor 0 is the only one awake to
    // notice the imbalance, and pushing is the only thing that can fix it.
    let loads = [
        busy(LOAD_SCALE * 9, 10),
        busy(LOAD_SCALE, 1),
        busy(LOAD_SCALE, 1),
        busy(LOAD_SCALE, 1),
    ];
    assert_eq!(busiest(&loads, &all_cpus(4), 0), None);
    assert_eq!(quietest(&loads, &all_cpus(4), 0), Some(1));
}

#[test]
fn a_processor_with_nothing_spare_pushes_nowhere() {
    let loads = [busy(LOAD_SCALE, 2), busy(LOAD_SCALE, 2)];
    assert_eq!(quietest(&loads, &all_cpus(2), 0), None);
}

#[test]
fn pushing_goes_to_the_quietest_not_merely_a_quiet_one() {
    let loads = [
        busy(LOAD_SCALE * 9, 10),
        busy(LOAD_SCALE * 3, 3),
        busy(0, 0),
        busy(LOAD_SCALE * 2, 2),
    ];
    assert_eq!(quietest(&loads, &all_cpus(4), 0), Some(2));
}

#[test]
fn pushing_respects_the_mask() {
    let loads = [busy(LOAD_SCALE * 9, 10), busy(0, 0), busy(0, 0)];
    // Only processor 2 is allowed, so that is where it goes even though 1 is
    // just as quiet and comes first.
    assert_eq!(quietest(&loads, &only(&[0, 2]), 0), Some(2));
}

#[test]
fn pushing_and_pulling_never_both_apply() {
    // If they did, two processors could each decide to move a task to the
    // other at the same moment.
    let loads = [busy(LOAD_SCALE * 9, 10), busy(LOAD_SCALE, 1)];
    for me in 0..2 {
        let pull = busiest(&loads, &all_cpus(2), me);
        let push = quietest(&loads, &all_cpus(2), me);
        assert!(
            pull.is_none() || push.is_none(),
            "processor {me} would both pull and push"
        );
    }
}

#[test]
fn a_queue_only_one_longer_is_not_worth_rebalancing() {
    // The brake: five against four is already as level as moving one task can
    // make it, whatever the averages say.
    let from = busy(LOAD_SCALE * 9, 5);
    let to = busy(0, 4);
    assert_eq!(imbalance(&from, &to), None);
}

#[test]
fn balancing_settles_rather_than_oscillating_on_counts() {
    // Walk the actual loop: move one task at a time from a queue of ten to a
    // queue of one, with the load average deliberately frozen — which is what
    // it effectively is over the milliseconds a burst of moves takes. Without
    // the count test this never terminates.
    let mut from = busy(LOAD_SCALE * 9, 10);
    let mut to = busy(0, 1);
    let mut moves = 0;
    while imbalance(&from, &to).is_some() {
        from.queued -= 1;
        to.queued += 1;
        moves += 1;
        assert!(moves <= 16, "balancing did not settle after {moves} moves");
    }
    assert!(
        from.queued.abs_diff(to.queued) <= 1,
        "balancing settled at {} against {}",
        from.queued,
        to.queued
    );
}

#[test]
fn a_burst_of_placements_does_not_all_land_on_one_processor() {
    // The load average has a 33-millisecond half-life and a burst of spawns
    // takes microseconds, so within one burst every average is stale. Ranking
    // on it first sends the whole burst to whichever processor has been idle
    // longest. Only the queue count moves fast enough to see the last
    // decision, so it has to be ranked on first.
    //
    // Four processors, all equally and long idle by the average's reckoning,
    // and four tasks placed one after another with only the counts updating.
    let mut loads = [busy(0, 0), busy(0, 0), busy(0, 0), busy(0, 0)];
    let mut landed = [0usize; 4];
    for _ in 0..4 {
        let cpu = place(&loads, &all_cpus(4), 0).expect("somewhere to put it");
        landed[cpu] += 1;
        loads[cpu].queued += 1;
    }
    assert_eq!(
        landed,
        [1, 1, 1, 1],
        "a burst of four went {landed:?} across four processors"
    );
}

#[test]
fn placement_still_prefers_a_lighter_processor_when_counts_are_equal() {
    // The count is the first key, not the only one: with the same number of
    // tasks each, the less loaded processor still wins.
    let loads = [busy(LOAD_SCALE * 4, 2), busy(LOAD_SCALE, 2)];
    assert_eq!(place(&loads, &all_cpus(2), 0), Some(1));
}

// ---------------------------------------------------------------------------
// Wake-affine placement and the creation charge
// ---------------------------------------------------------------------------

/// A wake is wide only when the smaller flip count reaches the factor and
/// the larger is the factor times it, either way round.
/// Verifies: L.sched.91
#[test]
fn a_wake_is_wide_only_between_many_partners() {
    use crate::wake_wide;
    // A pair that wakes each other: no flips, never wide.
    assert!(!wake_wide(0, 0, 4));
    assert!(!wake_wide(1, 1, 4));
    // A server of many and a client that flips at least `factor` times.
    assert!(wake_wide(16, 4, 4));
    assert!(wake_wide(4, 16, 4), "symmetric in waker and wakee");
    // One short of either bound.
    assert!(!wake_wide(15, 4, 4));
    assert!(!wake_wide(100, 3, 4));
    // The factor is the domain's processor count: two processors make the
    // same counts wide that four do not.
    assert!(wake_wide(6, 3, 2));
    assert!(!wake_wide(6, 3, 4));
    assert!(
        wake_wide(u32::MAX, u32::MAX / 2, 2),
        "no overflow at the top"
    );
}

/// The flip count is halved once when more than a second has passed since
/// the last halving, and a change of wakee adds one.
/// Verifies: L.sched.91
#[test]
fn flips_halve_after_a_second_and_count_changes() {
    use crate::{FLIP_DECAY_NS, flip};
    assert_eq!(flip(0, 0, 10, true), (1, 0));
    assert_eq!(flip(5, 0, 10, false), (5, 0), "the same wakee adds nothing");
    assert_eq!(
        flip(8, 0, FLIP_DECAY_NS, false),
        (8, 0),
        "a second exactly is not more"
    );
    assert_eq!(flip(8, 0, FLIP_DECAY_NS + 1, false), (4, FLIP_DECAY_NS + 1));
    // Once however long the gap.
    assert_eq!(
        flip(8, 0, 100 * FLIP_DECAY_NS, true),
        (5, 100 * FLIP_DECAY_NS)
    );
    assert_eq!(flip(u32::MAX, 0, 1, true), (u32::MAX, 0), "saturates");
}

/// The pull is allowed while the waker's side, the wakee's load added,
/// reads below 108% of the home's, the wakee's load taken away; a wakee
/// busy more than about 8% of the time stays home between two spinners.
/// Verifies: L.sched.90
#[test]
fn the_pull_weighs_the_home_at_108_percent() {
    use crate::pull_fits;
    assert!(
        pull_fits(1024, 1024, 0),
        "equal loads: the waker's side wins"
    );
    assert!(pull_fits(1105, 1024, 0));
    assert!(!pull_fits(1106, 1024, 0), "110,600 is not below 110,592");
    assert!(!pull_fits(0, 0, 0), "two idle averages: no reason to move");
    assert!(pull_fits(0, 1, 0));
    // A spinner each side and the wakee's own load on top.
    assert!(pull_fits(1024, 1024 + 30, 30), "3% busy: pulled");
    assert!(!pull_fits(1024, 1024 + 92, 92), "9% busy: stays");
    assert!(!pull_fits(1024, 1024 + 600, 600), "a wakee busy 60% stays");
    assert!(
        !pull_fits(0, 10, 20),
        "more than home carries: the home side is zero"
    );
    assert!(
        !pull_fits(u64::MAX, u64::MAX / 2, u64::MAX),
        "no overflow at the top"
    );
}

/// A task's busy share is its runtime over the window, capped at one
/// processor, with no answer for an empty window.
/// Verifies: L.sched.90
#[test]
fn utilisation_is_runtime_over_the_window() {
    use crate::utilisation;
    assert_eq!(utilisation(0, 0), None);
    assert_eq!(utilisation(1_000, 1_000), Some(LOAD_SCALE));
    assert_eq!(utilisation(30_000, 1_000_000), Some(30));
    assert_eq!(
        utilisation(5_000_000, 1_000_000),
        Some(LOAD_SCALE),
        "capped"
    );
    assert_eq!(utilisation(u64::MAX, 1), Some(LOAD_SCALE));
}
