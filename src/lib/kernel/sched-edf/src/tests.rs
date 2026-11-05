extern crate std;

use alloc::vec::Vec;

use ferrix_sched::{Config, EntityState, NICE_0_WEIGHT, Slot};

use super::*;

const MS: u64 = 1_000_000;

fn queue() -> OverFair<u64> {
    OverFair::new(Config { slice_ns: 3 * MS }).unwrap()
}

fn plain(runtime_ms: u64, period_ms: u64) -> Reservation {
    Reservation {
        runtime_ns: runtime_ms * MS,
        deadline_ns: period_ms * MS,
        period_ns: period_ms * MS,
    }
}

fn put(q: &mut OverFair<u64>, id: u64) {
    q.enqueue(
        id,
        id,
        EntityState::new(NICE_0_WEIGHT),
        Slot::new().unwrap(),
    )
    .unwrap();
}

/// Run what is picked for `step`, the clock moving: what ran.
fn step(q: &mut OverFair<u64>, now: &mut u64, step: u64) -> Option<u64> {
    q.note_time(*now);
    let ran = q.pick_next().copied();
    *now += step;
    if ran.is_some() {
        let _ = q.update_curr(step);
    }
    q.note_time(*now);
    ran
}

#[test]
fn a_malformed_reservation_is_refused_by_name() {
    for (runtime, deadline, period) in [
        (0, 10 * MS, 10 * MS),
        (LEAST_RUNTIME_NS - 1, 10 * MS, 10 * MS),
        (11 * MS, 10 * MS, 10 * MS),
        (MS, 5 * MS, 10 * MS),
        (MS, 20 * MS, 10 * MS),
        (LEAST_RUNTIME_NS, LEAST_PERIOD_NS - 1, LEAST_PERIOD_NS - 1),
        (MS, MOST_PERIOD_NS + 1, MOST_PERIOD_NS + 1),
        (u64::MAX, u64::MAX, u64::MAX),
    ] {
        let asked = Reservation {
            runtime_ns: runtime,
            deadline_ns: deadline,
            period_ns: period,
        };
        assert_eq!(
            queue().admit(1, asked),
            Err(Refusal::Malformed),
            "{asked:?}"
        );
    }
}

#[test]
fn an_unschedulable_set_is_refused_and_changes_nothing() {
    let mut q = queue();
    q.admit(1, plain(5, 10)).unwrap();
    q.admit(2, plain(4, 10)).unwrap();
    assert_eq!(q.used_ppm(), 900_000);
    assert_eq!(q.admit(3, plain(1, 10)), Err(Refusal::Unschedulable));
    assert_eq!(q.used_ppm(), 900_000);
    assert!(!q.is_reserved(3));
    q.admit(
        3,
        Reservation {
            runtime_ns: MS / 2,
            deadline_ns: 10 * MS,
            period_ns: 10 * MS,
        },
    )
    .unwrap();
    assert_eq!(q.used_ppm(), LIMIT_PPM);
    assert_eq!(q.admit(1, plain(1, 10_000)), Err(Refusal::Already));
    q.check_invariants().unwrap();
}

#[test]
fn past_the_most_a_processor_refuses_by_name() {
    let mut q = queue();
    let tiny = Reservation {
        runtime_ns: LEAST_RUNTIME_NS,
        deadline_ns: 1_000 * MS,
        period_ns: 1_000 * MS,
    };
    for id in 0..MOST as u64 {
        q.admit(id, tiny).unwrap();
    }
    assert_eq!(q.admit(99, tiny), Err(Refusal::Full));
}

#[test]
fn a_discharge_gives_the_share_back_only_off_the_queue() {
    let mut q = queue();
    q.admit(1, plain(5, 10)).unwrap();
    put(&mut q, 1);
    assert!(!q.discharge(1), "it is on the queue");
    let _ = q.pick_next();
    let _ = q.remove_curr().unwrap();
    assert!(q.discharge(1));
    assert_eq!(q.used_ppm(), 0);
    assert!(!q.discharge(1));
}

#[test]
fn the_earliest_deadline_runs_and_the_fair_class_after_it() {
    let mut q = queue();
    let mut now = 1_000 * MS;
    q.note_time(now);
    q.admit(1, plain(2, 40)).unwrap();
    q.admit(2, plain(2, 10)).unwrap();
    put(&mut q, 7);
    put(&mut q, 1);
    put(&mut q, 2);
    assert!(q.should_preempt());
    assert_eq!(step(&mut q, &mut now, MS), Some(2));
    assert_eq!(step(&mut q, &mut now, MS), Some(2));
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    assert_eq!(step(&mut q, &mut now, MS), Some(7), "both out of budget");
}

#[test]
fn over_its_budget_a_reservation_is_throttled_until_its_period_ends() {
    let mut q = queue();
    let mut now = 1_000 * MS;
    q.note_time(now);
    q.admit(1, plain(2, 10)).unwrap();
    put(&mut q, 1);
    put(&mut q, 7);
    let mut ran = Vec::new();
    for _ in 0..30 {
        ran.push(step(&mut q, &mut now, MS).unwrap());
    }
    for period in ran.chunks(10) {
        assert_eq!(
            period.iter().filter(|id| **id == 1).count(),
            2,
            "{period:?}"
        );
        assert_eq!(
            period.iter().filter(|id| **id == 7).count(),
            8,
            "{period:?}"
        );
    }
    let (throttles, refills) = q.throttles_and_refills();
    assert_eq!(throttles, 3);
    assert_eq!(refills, 2 + 1);
}

#[test]
fn a_throttled_reservation_with_nothing_else_leaves_the_processor_idle_and_a_timer_set() {
    let mut q = queue();
    let mut now = 1_000 * MS;
    q.note_time(now);
    q.admit(1, plain(2, 10)).unwrap();
    put(&mut q, 1);
    assert_eq!(step(&mut q, &mut now, 2 * MS), Some(1));
    assert!(q.should_preempt(), "out of budget");
    assert_eq!(q.pick_next(), None);
    assert_eq!(q.decision_in_ns(), Some(8 * MS));
    assert!(!q.is_empty(), "it is still on the queue");
    now += 8 * MS;
    q.note_time(now);
    assert_eq!(q.decision_in_ns(), None);
    assert!(q.should_preempt(), "it came back on an idle processor");
    assert_eq!(q.pick_next(), Some(&1));
    assert_eq!(q.decision_in_ns(), Some(2 * MS));
}

#[test]
fn a_wake_with_too_little_time_left_starts_a_fresh_period() {
    let mut q = queue();
    let mut now = 1_000 * MS;
    q.note_time(now);
    q.admit(1, plain(2, 10)).unwrap();
    put(&mut q, 1);
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    // Asleep to within a millisecond of its deadline, with a millisecond of budget left:
    // that much bandwidth would overrun, so the budget and deadline start again.
    let _ = q.remove_curr().unwrap();
    now += 8 * MS;
    q.note_time(now);
    put(&mut q, 1);
    assert_eq!(q.pick_next(), Some(&1));
    assert_eq!(q.remaining_ns(), Some(2 * MS));
}

#[test]
fn a_wake_with_enough_time_left_keeps_its_budget_and_deadline() {
    let mut q = queue();
    let mut now = 1_000 * MS;
    q.note_time(now);
    q.admit(1, plain(2, 10)).unwrap();
    put(&mut q, 1);
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    let _ = q.remove_curr().unwrap();
    put(&mut q, 1);
    assert_eq!(q.pick_next(), Some(&1));
    assert_eq!(q.remaining_ns(), Some(MS));
}

#[test]
fn a_yield_gives_up_the_budget_until_the_next_period() {
    let mut q = queue();
    let mut now = 1_000 * MS;
    q.note_time(now);
    q.admit(1, plain(2, 10)).unwrap();
    put(&mut q, 1);
    put(&mut q, 7);
    assert_eq!(step(&mut q, &mut now, MS / 2), Some(1));
    q.yield_curr();
    assert_eq!(q.pick_next(), Some(&7));
    now += 10 * MS;
    q.note_time(now);
    assert!(q.should_preempt());
    assert_eq!(q.pick_next(), Some(&1));
}

#[test]
fn a_running_thread_admitted_leaves_the_fair_class_at_once() {
    let mut q = queue();
    let mut now = 1_000 * MS;
    q.note_time(now);
    put(&mut q, 7);
    put(&mut q, 8);
    assert_eq!(q.pick_next(), Some(&7));
    q.admit(7, plain(2, 10)).unwrap();
    q.adopt(7);
    assert_eq!(q.pick_next(), Some(&7));
    assert!(q.runs_reserved());
    assert_eq!(step(&mut q, &mut now, 2 * MS), Some(7));
    assert_eq!(q.throttles_and_refills().0, 1);
    assert_eq!(step(&mut q, &mut now, MS), Some(8));
    q.check_invariants().unwrap();
}

#[test]
fn a_withdrawn_reservation_runs_on_as_the_fair_class() {
    let mut q = queue();
    q.note_time(1_000 * MS);
    q.admit(7, plain(2, 10)).unwrap();
    put(&mut q, 7);
    assert_eq!(q.pick_next(), Some(&7));
    assert!(q.withdraw(7));
    assert_eq!(q.used_ppm(), 0);
    // Still running as it was, and no steal can find it: the fair class has nothing queued.
    assert!(q.runs_reserved());
    assert_eq!(q.latest_where(|id| *id == 7), None);
    assert_eq!(q.queued(), 0);
    assert!(!q.withdraw(7), "given back once");
    assert_eq!(q.pick_next(), Some(&7));
    assert!(!q.runs_reserved());
    assert!(!q.is_reserved(7));
    q.check_invariants().unwrap();
}

#[test]
fn a_stall_past_a_period_is_not_paid_back() {
    let mut q = queue();
    let mut now = 1_000 * MS;
    q.note_time(now);
    q.admit(1, plain(2, 10)).unwrap();
    put(&mut q, 1);
    assert_eq!(step(&mut q, &mut now, 2 * MS), Some(1));
    now += 100 * MS;
    q.note_time(now);
    assert_eq!(q.pick_next(), Some(&1));
    assert_eq!(q.remaining_ns(), Some(2 * MS));
    assert_eq!(step(&mut q, &mut now, 2 * MS), Some(1));
    assert_eq!(q.pick_next(), None, "one budget, not ten");
    assert_eq!(q.decision_in_ns(), Some(8 * MS));
}

#[test]
fn nothing_moves_a_reservation() {
    let mut q = queue();
    q.admit(1, plain(2, 10)).unwrap();
    put(&mut q, 1);
    put(&mut q, 7);
    let _ = q.pick_next();
    assert_eq!(q.latest_where(|id| *id == 1), None);
}

#[test]
fn the_long_way_is_taken_when_a_reservation_is_here_and_not_otherwise() {
    let mut q = queue();
    put(&mut q, 7);
    assert_eq!(q.pick_next(), Some(&7));
    let left = q
        .hand_over(
            8,
            8,
            EntityState::new(NICE_0_WEIGHT),
            Slot::new().unwrap(),
            3 * MS,
        )
        .unwrap();
    assert_eq!(left.map(|left| left.0), Some(7));
    assert_eq!(q.current(), Some(&8));
    q.admit(1, plain(2, 10)).unwrap();
    put(&mut q, 1);
    let left = q
        .hand_over(
            9,
            9,
            EntityState::new(NICE_0_WEIGHT),
            Slot::new().unwrap(),
            3 * MS,
        )
        .unwrap();
    assert_eq!(left.map(|left| left.0), Some(8));
    assert_eq!(q.current(), Some(&1), "the reservation, never the peer");
    q.check_invariants().unwrap();
}

/// Anything admitted gets its whole budget by each of its periods' deadlines, whatever else
/// is admitted, and the fair class keeps what is left: EDF's bound at the limit's sum. Held
/// period by period: a budget delivered late, as a pick by anything but deadline would, fails.
#[test]
fn every_admitted_set_meets_every_deadline_and_the_fair_class_has_the_rest() {
    let mut seed = 0x1234_5678_9abc_def1_u64;
    let mut random = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    const STEP: u64 = 100_000;
    let mut tight = 0;
    // Interpreted, a few sets are what Miri has time for.
    let sets = if cfg!(miri) { 4 } else { 200 };
    for _ in 0..sets {
        let mut q = queue();
        let begun = 1_000 * MS;
        let mut now = begun;
        q.note_time(now);
        let mut admitted = Vec::new();
        for id in 1..=6_u64 {
            let period = (1 + random(5)) * 4 * MS;
            let runtime = (2 + random(period / STEP / 2)) * STEP;
            let asked = Reservation {
                runtime_ns: runtime.min(period),
                deadline_ns: period,
                period_ns: period,
            };
            if q.admit(id, asked).is_ok() {
                put(&mut q, id);
                admitted.push((id, asked));
            }
        }
        if q.used_ppm() > 800_000 {
            tight += 1;
        }
        put(&mut q, 100);
        // Every period divides 240 ms, the multiples of 4 to 20 having that as their lcm.
        let total = 240 * MS;
        let mut in_period = alloc::collections::BTreeMap::<u64, u64>::new();
        let mut fair = 0_u64;
        let mut reserved = 0_u64;
        let end = begun + total;
        while now < end {
            let ran = step(&mut q, &mut now, STEP);
            match ran {
                Some(100) => fair += STEP,
                Some(id) => {
                    reserved += STEP;
                    *in_period.entry(id).or_default() += STEP;
                }
                None => {}
            }
            for (id, asked) in &admitted {
                if (now - begun).is_multiple_of(asked.period_ns) {
                    let had = in_period.remove(id).unwrap_or(0);
                    assert_eq!(
                        had,
                        asked.runtime_ns,
                        "{id}: a period ending at {} ms had {had} of {}",
                        (now - begun) / MS,
                        asked.runtime_ns
                    );
                }
            }
        }
        assert_eq!(
            reserved + fair,
            total,
            "the processor was never left idle with work"
        );
        q.check_invariants().unwrap();
    }
    assert!(
        cfg!(miri) || tight > 10,
        "the sets are too light to show anything: {tight}"
    );
}
