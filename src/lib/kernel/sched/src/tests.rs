//! Tests for the scheduling logic.
//!
//! Three kinds, and the last two are the ones worth having. The small tests
//! pin one rule each. The randomised test drives the queue through tens of
//! thousands of mixed operations, checks every invariant after each, and
//! checks every pick against a brute-force search over the same entities —
//! which is what would catch a tree that picked a plausible entity rather than
//! the right one. And the simulations run a CPU for simulated minutes and
//! require EEVDF's own bound at every decision: no entity's lag, measured in
//! real service against its weighted share, may exceed the largest request
//! anyone was given. That bound is what stage 5's boot test measures on a
//! running machine; proving it here first is what makes a failure there a bug
//! in the kernel's half rather than in this one.

extern crate std;

use alloc::vec::Vec;

use super::*;

/// Three milliseconds, the slice the kernel uses.
const SLICE: u64 = 3_000_000;

/// A slot for one entity.
fn slot<T>() -> Slot<T> {
    Slot::new().unwrap()
}

fn queue() -> RunQueue<u64> {
    RunQueue::new(Config { slice_ns: SLICE }).unwrap()
}

#[track_caller]
fn check<T>(queue: &RunQueue<T>) {
    assert_eq!(
        queue.check_invariants(),
        Ok(()),
        "the queue broke its own invariant"
    );
}

/// A small deterministic generator: xorshift64*, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

// ---------------------------------------------------------------------------
// Weights
// ---------------------------------------------------------------------------

#[test]
fn nice_zero_is_the_unit_weight() {
    assert_eq!(weight_of_nice(0), Some(NICE_0_WEIGHT), "nice 0 is the unit");
}

#[test]
fn nice_outside_the_table_has_no_weight() {
    assert_eq!(weight_of_nice(-21), None, "below -20");
    assert_eq!(weight_of_nice(20), None, "above 19");
    assert_eq!(weight_of_nice(i32::MIN), None, "the far end");
}

#[test]
fn each_nice_level_is_about_a_quarter_heavier_than_the_next() {
    for nice in -20..19 {
        let this = f64::from(weight_of_nice(nice).unwrap());
        let next = f64::from(weight_of_nice(nice + 1).unwrap());
        let ratio = this / next;
        assert!(
            (1.20..1.30).contains(&ratio),
            "nice {nice} is {ratio} times nice {}",
            nice + 1
        );
    }
}

// ---------------------------------------------------------------------------
// One rule each
// ---------------------------------------------------------------------------

#[test]
fn a_zero_slice_is_refused() {
    assert_eq!(
        RunQueue::<u64>::new(Config { slice_ns: 0 }).map(|_| ()),
        Err(SchedError::ZeroSlice),
        "a zero slice"
    );
}

#[test]
fn an_empty_queue_picks_nothing() {
    let mut queue = queue();
    assert_eq!(queue.pick_next(), None, "nothing to pick");
    assert!(queue.is_empty(), "and nothing there");
    assert!(!queue.should_preempt(), "nothing to preempt for");
    check(&queue);
}

#[test]
fn a_zero_weight_is_refused_and_the_payload_handed_back() {
    let mut queue = queue();
    let refused = queue
        .enqueue(1, 77, EntityState::new(0), slot())
        .unwrap_err();
    assert_eq!(refused.reason, SchedError::ZeroWeight, "why");
    assert_eq!(refused.payload, 77, "the payload comes back");
    assert!(queue.is_empty(), "nothing was queued");
}

#[test]
fn an_identifier_is_refused_twice_whether_queued_or_running() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    let queued = queue
        .enqueue(1, 2, EntityState::new(1024), slot())
        .unwrap_err();
    assert_eq!(queued.reason, SchedError::Duplicate(1), "while queued");

    assert_eq!(queue.pick_next(), Some(&1), "now running");
    let running = queue
        .enqueue(1, 3, EntityState::new(1024), slot())
        .unwrap_err();
    assert_eq!(running.reason, SchedError::Duplicate(1), "while running");
    check(&queue);
}

#[test]
fn the_only_entity_runs_and_keeps_running() {
    let mut queue = queue();
    queue
        .enqueue(7, 70, EntityState::new(1024), slot())
        .unwrap();
    for _ in 0..10 {
        assert_eq!(queue.pick_next(), Some(&70), "the only candidate");
        let _ = queue.update_curr(SLICE);
        check(&queue);
    }
    assert!(!queue.should_preempt(), "nothing to give way to");
}

#[test]
fn equal_entities_take_turns_a_slice_at_a_time() {
    let mut queue = queue();
    for id in 1..=3 {
        queue
            .enqueue(id, id, EntityState::new(1024), slot())
            .unwrap();
    }
    let mut order = Vec::new();
    for _ in 0..9 {
        order.push(*queue.pick_next().unwrap());
        assert!(queue.update_curr(SLICE), "a whole slice is spent");
        check(&queue);
    }
    assert_eq!(order, [1, 2, 3, 1, 2, 3, 1, 2, 3], "round robin, in effect");
}

#[test]
fn running_charges_virtual_time_in_inverse_proportion_to_weight() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(2048), slot()).unwrap();
    let _ = queue.pick_next();
    let before = queue.get(1).unwrap().vruntime;
    let _ = queue.update_curr(1_000_000);
    let view = queue.get(1).unwrap();
    assert_eq!(
        view.vruntime - before,
        500_000,
        "twice the weight, half the rate"
    );
    assert_eq!(view.sum_exec, 1_000_000, "real time is real time");
}

#[test]
fn a_spent_slice_asks_for_a_new_deadline_from_where_the_entity_is() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    let _ = queue.pick_next();
    assert!(!queue.update_curr(SLICE - 1), "one nanosecond short");
    assert!(queue.update_curr(1_000), "and past it");
    let view = queue.get(1).unwrap();
    assert_eq!(
        view.deadline,
        view.vruntime + SLICE,
        "the next request runs from here, overrun forgiven"
    );
}

#[test]
fn the_remaining_slice_is_real_time_whatever_the_weight() {
    for weight in [15, 1024, 2048, 88761] {
        // A slice is stored as virtual time, and converting back rounds down
        // once per virtual nanosecond lost — so the error a heavy entity sees
        // is its weight in units of the unit weight, and no more. Eighty-seven
        // nanoseconds at nice -20, against a three-millisecond slice.
        let tolerance = u64::from(weight) / u64::from(NICE_0_WEIGHT) + 1;
        let mut queue = queue();
        queue
            .enqueue(1, 1, EntityState::new(weight), slot())
            .unwrap();
        let _ = queue.pick_next();
        let remaining = queue.remaining_ns().unwrap();
        assert!(
            remaining.abs_diff(SLICE) <= tolerance,
            "weight {weight} has {remaining} ns left of a {SLICE} ns slice"
        );
        let _ = queue.update_curr(SLICE / 3);
        let remaining = queue.remaining_ns().unwrap();
        assert!(
            remaining.abs_diff(SLICE - SLICE / 3) <= tolerance,
            "weight {weight} has {remaining} ns left after a third"
        );
    }
}

#[test]
fn an_earlier_deadline_that_is_not_eligible_waits() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    // A heavy entity arriving a little ahead of its share. Its slice is a
    // sliver in virtual time — a heavy entity reaches the same real slice in
    // far less of it — so its deadline lands before the light entity's even
    // though it starts ahead of the average, and it is not eligible.
    //
    // Two microseconds of lag rather than sixty: placement scales lag by the
    // new total weight over the old, which for a weight of 88761 arriving
    // beside one of 1024 is a factor of eighty-seven.
    let ahead = EntityState {
        weight: 88761,
        vlag: -2_000,
        sum_exec: 0,
    };
    queue.enqueue(2, 2, ahead, slot()).unwrap();
    check(&queue);

    let light = queue.get(1).unwrap();
    let heavy = queue.get(2).unwrap();
    assert!(
        before(heavy.deadline, light.deadline),
        "the heavy one sorts first"
    );
    assert!(heavy.lag < 0, "and has had more than its share");
    assert_eq!(queue.pick_next(), Some(&1), "so the eligible one runs");
}

#[test]
fn a_new_entity_arrives_owed_nothing() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    let _ = queue.pick_next();
    let _ = queue.update_curr(10 * SLICE);
    queue.enqueue(2, 2, EntityState::new(3121), slot()).unwrap();
    let lag = queue.lag(2).unwrap();
    assert!(lag.abs() <= 1, "a newcomer has lag {lag}");
    check(&queue);
}

#[test]
fn lag_survives_leaving_and_coming_back() {
    let mut queue = queue();
    for id in 1..=4 {
        queue
            .enqueue(
                id,
                id,
                EntityState::new(weight_of_nice(id as i32 - 2).unwrap()),
                slot(),
            )
            .unwrap();
    }
    // Run a few slices so the lags are not all zero.
    for _ in 0..5 {
        let _ = queue.pick_next();
        let _ = queue.update_curr(SLICE);
    }
    let _ = queue.pick_next();
    let running = queue.current_id().unwrap();
    let sleeper = (1..=4).find(|id| *id != running).unwrap();
    let lag_before = queue.lag(sleeper).unwrap();

    let (payload, state, _) = queue.remove(sleeper).unwrap();
    assert_eq!(state.vlag, lag_before, "it leaves with the lag it had");
    check(&queue);

    queue.enqueue(sleeper, payload, state, slot()).unwrap();
    let lag_after = queue.lag(sleeper).unwrap();
    assert!(
        lag_after.abs_diff(lag_before) <= 2,
        "it left with {lag_before} and came back with {lag_after}"
    );
    check(&queue);
}

#[test]
fn lag_carried_back_is_clamped_to_two_slices() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    let owed = EntityState {
        weight: 1024,
        vlag: i64::MAX / 2,
        sum_exec: 0,
    };
    queue.enqueue(2, 2, owed, slot()).unwrap();
    let lag = queue.lag(2).unwrap();
    assert!(
        lag.abs_diff(2 * SLICE as i64) <= 2,
        "a long sleep collects {lag}, not a lifetime"
    );
}

#[test]
fn virtual_time_is_the_weighted_average_of_runtimes() {
    let mut rng = Rng(0x5EED);
    let mut queue = queue();
    for id in 0..20 {
        let weight = weight_of_nice(rng.below(40) as i32 - 20).unwrap();
        queue
            .enqueue(id, id, EntityState::new(weight), slot())
            .unwrap();
        let _ = queue.pick_next();
        let _ = queue.update_curr(rng.below(SLICE));
    }
    let mut weighted = 0i128;
    let mut total = 0i128;
    queue.for_each(|view| {
        weighted += i128::from(view.weight) * i128::from(view.vruntime);
        total += i128::from(view.weight);
    });
    let exact = weighted.div_euclid(total) as u64;
    assert_eq!(
        queue.avg_vruntime(),
        exact,
        "the queue's average is the average"
    );
}

#[test]
fn yielding_lets_the_next_deadline_go_first() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    queue.enqueue(2, 2, EntityState::new(1024), slot()).unwrap();
    assert_eq!(queue.pick_next(), Some(&1), "one first");
    let _ = queue.update_curr(SLICE / 10);
    queue.yield_curr();
    assert_eq!(
        queue.pick_next(),
        Some(&2),
        "then two, having been yielded to"
    );
}

/// Six hundred yields with nothing else queued leave the running entity's
/// request where it was. Each used to push its deadline a slice further out,
/// and 1.8 seconds of request was what the next arrival waited behind
/// (FX-0502).
///
/// Verifies: L.sched.2
#[test]
fn yielding_alone_leaves_the_request_as_it_was() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    assert_eq!(queue.pick_next(), Some(&1));
    let _ = queue.update_curr(SLICE / 10);
    let left = queue.remaining_ns();
    for _ in 0..600 {
        queue.yield_curr();
        assert_eq!(queue.pick_next(), Some(&1), "alone, it runs on");
    }
    assert_eq!(
        queue.remaining_ns(),
        left,
        "yielding to nobody asked for more of the processor"
    );
    check(&queue);
}

/// With an entity waiting that is not eligible, the running one yielding
/// again and again, and so holding a deadline hundreds of slices out, the
/// queue still asks for a decision within one slice. Armed for the whole
/// request instead, the waiting entity sat out seconds (FX-0502).
///
/// Verifies: L.sched.1
#[test]
fn something_waiting_is_decided_on_within_a_slice() {
    let mut queue = queue();
    assert_eq!(queue.decision_in_ns(), None, "an empty queue needs none");
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    assert_eq!(queue.pick_next(), Some(&1));
    assert_eq!(queue.decision_in_ns(), None, "nor one running alone");

    // Ahead of its share, as a woken or moved entity can arrive.
    let ahead = EntityState {
        weight: 1024,
        vlag: -2_000_000,
        sum_exec: 0,
    };
    queue.enqueue(2, 2, ahead, slot()).unwrap();
    for _ in 0..600 {
        queue.yield_curr();
        assert_eq!(
            queue.pick_next(),
            Some(&1),
            "the waiting one is not eligible"
        );
    }
    assert!(
        queue.remaining_ns().unwrap() > 100 * SLICE,
        "yielding past an ineligible entity pushed the request far out"
    );
    let decision = queue.decision_in_ns().unwrap();
    assert!(
        decision <= SLICE,
        "the next decision is {decision} ns away with something waiting"
    );
    check(&queue);
}

/// A request shorter than the slice is decided on when it ends, as it was:
/// the bound only ever shortens the interval.
#[test]
fn a_short_request_is_decided_on_when_it_ends() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    queue.enqueue(2, 2, EntityState::new(1024), slot()).unwrap();
    let _ = queue.pick_next();
    let _ = queue.update_curr(SLICE / 3);
    assert_eq!(queue.decision_in_ns(), queue.remaining_ns());
    assert!(queue.decision_in_ns().unwrap() < SLICE);
}

#[test]
fn a_wakeup_with_an_earlier_eligible_deadline_preempts() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    let _ = queue.pick_next();
    let _ = queue.update_curr(SLICE / 2);
    assert!(!queue.should_preempt(), "nothing to preempt for");

    // Owed service: behind the running entity, and with a deadline before
    // the one the running entity is working towards.
    let owed = EntityState {
        weight: 1024,
        vlag: 1_000_000,
        sum_exec: 0,
    };
    queue.enqueue(2, 2, owed, slot()).unwrap();
    assert!(queue.should_preempt(), "it is owed, and due sooner");
    assert_eq!(queue.pick_next(), Some(&2), "and runs");
}

#[test]
fn a_wakeup_that_is_ahead_does_not_preempt() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    let _ = queue.pick_next();
    let ahead = EntityState {
        weight: 1024,
        vlag: -1_000_000,
        sum_exec: 0,
    };
    queue.enqueue(2, 2, ahead, slot()).unwrap();
    assert!(!queue.should_preempt(), "it has had more than its share");
}

#[test]
fn the_steal_candidate_is_the_latest_deadline_that_may_move() {
    let mut queue = queue();
    for id in 1..=5 {
        queue
            .enqueue(id, id, EntityState::new(1024), slot())
            .unwrap();
        let _ = queue.pick_next();
        let _ = queue.update_curr(SLICE / 7);
    }
    let _ = queue.pick_next();
    let running = queue.current_id().unwrap();

    let mut latest = None;
    queue.for_each(|view| {
        if !view.running {
            latest = Some(view.id);
        }
    });
    assert_eq!(
        queue.latest_where(|_| true),
        latest,
        "the last in deadline order"
    );
    assert_ne!(
        queue.latest_where(|_| true),
        Some(running),
        "never the running one"
    );
    assert_eq!(queue.latest_where(|_| false), None, "nothing may move");
}

#[test]
fn removing_the_running_entity_by_name_works_like_remove_curr() {
    let mut queue = queue();
    queue
        .enqueue(1, 10, EntityState::new(1024), slot())
        .unwrap();
    queue
        .enqueue(2, 20, EntityState::new(1024), slot())
        .unwrap();
    let _ = queue.pick_next();
    let running = queue.current_id().unwrap();
    let (payload, _, _) = queue.remove(running).unwrap();
    assert_eq!(payload, running * 10, "the running entity's payload");
    assert_eq!(queue.current(), None, "nothing running now");
    assert_eq!(queue.len(), 1, "one left");
    check(&queue);
}

#[test]
fn levelling_forgives_every_lag_and_keeps_the_average() {
    let mut queue = queue();
    for id in 1..=3 {
        queue
            .enqueue(id, id, EntityState::new(NICE_0_WEIGHT), slot())
            .unwrap();
    }
    // Run one entity well past everybody: it is far ahead, the others owed.
    let _ = queue.pick_next();
    let _ = queue.update_curr(SLICE * 7);
    let avg_before = queue.avg_vruntime();
    assert!(
        queue.for_each_lag().iter().any(|lag| *lag != 0),
        "the setup should have created lag"
    );

    queue.level();

    check(&queue);
    assert_eq!(
        queue.avg_vruntime(),
        avg_before,
        "levelling keeps the virtual time"
    );
    assert!(
        queue.for_each_lag().iter().all(|lag| *lag == 0),
        "every lag is forgiven: {:?}",
        queue.for_each_lag()
    );
    assert_eq!(queue.len(), 3, "nothing was lost");
    // And the queue still schedules: three picks visit three entities.
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(*queue.pick_next().unwrap());
        let _ = queue.update_curr(SLICE);
    }
    seen.sort_unstable();
    assert_eq!(seen, alloc::vec![1, 2, 3], "everyone still gets a turn");
}

#[test]
fn virtual_time_that_wraps_still_orders() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    queue.enqueue(2, 2, EntityState::new(1024), slot()).unwrap();
    // Each quarter of the range is well under the half the comparison needs,
    // and eight of them wrap the counter twice.
    let quarter = u64::MAX / 4;
    let mut last = 0;
    for step in 0..8 {
        let running = *queue.pick_next().unwrap();
        assert_ne!(running, last, "step {step}: the two still alternate");
        last = running;
        assert!(
            queue.update_curr(quarter),
            "a quarter of the range is a slice"
        );
        check(&queue);
    }
}

// ---------------------------------------------------------------------------
// Everything at once
// ---------------------------------------------------------------------------

/// The pick a brute-force search over `queue`'s entities would make: the
/// earliest deadline, then the lowest identifier, among those whose lag is
/// not negative.
impl RunQueue<u64> {
    /// Every entity's lag, running one first.
    fn for_each_lag(&self) -> Vec<i64> {
        let mut lags = Vec::new();
        self.for_each(|view| lags.push(view.lag));
        lags
    }
}

fn brute_force_pick(queue: &RunQueue<u64>) -> Option<u64> {
    let mut best: Option<(u64, u64)> = None;
    queue.for_each(|view| {
        if view.lag < 0 {
            return;
        }
        let better = match best {
            None => true,
            Some((deadline, id)) => {
                before(view.deadline, deadline) || (view.deadline == deadline && view.id < id)
            }
        };
        if better {
            best = Some((view.deadline, view.id));
        }
    });
    best.map(|(_, id)| id)
}

#[test]
fn random_operations_keep_every_invariant_and_pick_what_brute_force_picks() {
    let mut rng = Rng(0xFE44_1C5E_ED00_0001);
    let mut queue = queue();
    let mut away: Vec<(u64, EntityState)> = Vec::new();
    let mut next_id = 0u64;
    let mut picks = 0u32;

    for _ in 0..40_000 {
        match rng.below(10) {
            0 | 1 if queue.len() < 64 => {
                let weight = weight_of_nice(rng.below(40) as i32 - 20).unwrap();
                queue
                    .enqueue(next_id, next_id, EntityState::new(weight), slot())
                    .unwrap();
                next_id += 1;
            }
            2 if !away.is_empty() => {
                let (id, state) = away.swap_remove(rng.below(away.len() as u64) as usize);
                queue.enqueue(id, id, state, slot()).unwrap();
            }
            3 if queue.queued() > 0 => {
                let target = queue.latest_where(|_| true).unwrap();
                let (_, state, _) = queue.remove(target).unwrap();
                away.push((target, state));
            }
            4 => {
                if let Some((id, _, state, _)) = queue.remove_curr() {
                    away.push((id, state));
                }
            }
            5 => queue.yield_curr(),
            6..=8 => {
                let _ = queue.update_curr(rng.below(2 * SLICE));
            }
            _ => {
                // The running entity competes too, so the brute force sees
                // everything the pick will.
                let expected = brute_force_pick(&queue);
                let picked = queue.pick_next().copied();
                assert_eq!(
                    picked, expected,
                    "the tree picked differently from brute force"
                );
                picks += 1;
            }
        }
        check(&queue);
    }
    assert!(picks > 1000, "the test made only {picks} picks");
}

// ---------------------------------------------------------------------------
// The fairness bound, in simulation
// ---------------------------------------------------------------------------

/// Run a simulated CPU over `weights`, a slice at a time plus up to
/// `lateness` of timer overrun, for `decisions` scheduling decisions. At every
/// decision, measure each entity's lag in real service — its weighted share
/// of everything run so far, less what it has had — and return the largest
/// seen.
///
/// Measured from `sum_exec`, the real nanoseconds the queue was charged, not
/// from the queue's own lag field: the point is to check the virtual-time
/// arithmetic against what it is supposed to achieve, not against itself.
fn worst_real_lag(weights: &[u32], lateness: u64, decisions: u32, seed: u64) -> u64 {
    let mut rng = Rng(seed);
    let mut queue = queue();
    for (id, weight) in weights.iter().enumerate() {
        queue
            .enqueue(id as u64, id as u64, EntityState::new(*weight), slot())
            .unwrap();
    }
    let total_weight: u128 = weights.iter().map(|weight| u128::from(*weight)).sum();

    let mut worst = 0u64;
    for _ in 0..decisions {
        let _ = queue.pick_next();
        let remaining = queue.remaining_ns().unwrap();
        let late = if lateness == 0 {
            0
        } else {
            rng.below(lateness + 1)
        };
        let _ = queue.update_curr(remaining + late);

        let mut service = Vec::new();
        queue.for_each(|view| service.push((view.weight, view.sum_exec)));
        let total: u128 = service.iter().map(|(_, ran)| u128::from(*ran)).sum();
        for (weight, ran) in service {
            let share = total * u128::from(weight) / total_weight;
            worst = worst.max(share.abs_diff(u128::from(ran)) as u64);
        }
    }
    check(&queue);
    worst
}

#[test]
fn equal_weights_stay_within_one_slice_of_their_share() {
    let worst = worst_real_lag(&[1024; 8], 0, 50_000, 1);
    assert!(
        worst <= SLICE,
        "an entity strayed {worst} ns from its share"
    );
}

#[test]
fn mixed_weights_stay_within_one_slice_of_their_share() {
    // Powers of two, so every conversion between real and virtual time is
    // exact and the bound can be checked to the nanosecond.
    let worst = worst_real_lag(&[256, 512, 1024, 1024, 2048, 4096, 8192], 0, 50_000, 2);
    assert!(
        worst <= SLICE,
        "an entity strayed {worst} ns from its share"
    );
}

#[test]
fn a_late_timer_widens_the_bound_by_exactly_its_lateness() {
    // The bound is the largest request actually served, and a request cut
    // late by up to half a millisecond is up to half a millisecond longer.
    let lateness = 500_000;
    let worst = worst_real_lag(&[512, 1024, 1024, 2048, 4096], lateness, 50_000, 3);
    assert!(
        worst <= SLICE + lateness,
        "an entity strayed {worst} ns from its share, past {} ns",
        SLICE + lateness
    );
}

#[test]
fn nice_weights_stay_within_one_slice_of_their_share() {
    // Weights that do not divide evenly, so every update rounds. The rounding
    // is at most a nanosecond of virtual time per update, which over this
    // many decisions is still far inside the tolerance allowed for it.
    let weights: Vec<u32> = [-5, -3, 0, 0, 2, 5, 10]
        .iter()
        .map(|nice| weight_of_nice(*nice).unwrap())
        .collect();
    let worst = worst_real_lag(&weights, 250_000, 50_000, 4);
    assert!(
        worst <= SLICE + 250_000 + 100_000,
        "an entity strayed {worst} ns from its share"
    );
}

#[test]
fn shares_come_out_in_proportion_to_weight() {
    let weights = [1024u32, 2048, 4096];
    let mut queue = queue();
    for (id, weight) in weights.iter().enumerate() {
        queue
            .enqueue(id as u64, id as u64, EntityState::new(*weight), slot())
            .unwrap();
    }
    for _ in 0..30_000 {
        let _ = queue.pick_next();
        let remaining = queue.remaining_ns().unwrap();
        let _ = queue.update_curr(remaining);
    }
    let mut ran = [0u64; 3];
    queue.for_each(|view| ran[view.id as usize] = view.sum_exec);
    let total: u64 = ran.iter().sum();
    for (index, weight) in weights.iter().enumerate() {
        let expected = total / 7 * u64::from(*weight) / 1024;
        assert!(
            ran[index].abs_diff(expected) <= SLICE,
            "weight {weight} ran {} ns, owed {expected} ns",
            ran[index]
        );
    }
}

/// Run `queue` for `picks` decisions, each entity taking the whole of what it
/// is given, and answer what each has run for in all.
fn run_for(queue: &mut RunQueue<u64>, picks: usize, entities: usize) -> Vec<u64> {
    for _ in 0..picks {
        let _ = queue.pick_next();
        let remaining = queue.remaining_ns().unwrap();
        let _ = queue.update_curr(remaining);
    }
    let mut ran = alloc::vec![0u64; entities];
    queue.for_each(|view| ran[view.id as usize] = view.sum_exec);
    ran
}

#[test]
fn a_reweighed_entity_takes_the_share_its_new_weight_asks_for() {
    // Three equals, and then one of them is given four times the weight: what
    // it runs for from there is four times what each of the others does. The
    // entity being reweighed is a queued one, not the running one, because
    // `pick_next` leaves no entity running between decisions.
    let mut queue = queue();
    for id in 0..3u64 {
        queue
            .enqueue(id, id, EntityState::new(1024), slot())
            .unwrap();
    }
    let before = run_for(&mut queue, 3_000, 3);
    assert_eq!(queue.set_weight(2, 4096), Ok(true));
    check(&queue);
    let after = run_for(&mut queue, 30_000, 3);

    let ran: Vec<u64> = after
        .iter()
        .zip(&before)
        .map(|(after, before)| after - before)
        .collect();
    let total: u64 = ran.iter().sum();
    for (id, weight) in [1024u64, 1024, 4096].iter().enumerate() {
        let owed = total * weight / (1024 + 1024 + 4096);
        assert!(
            ran[id].abs_diff(owed) <= SLICE,
            "the entity of weight {weight} ran {} ns where {owed} ns was owed",
            ran[id]
        );
    }
}

#[test]
fn a_reweighed_running_entity_keeps_what_it_is_owed() {
    // The lag an entity carries is a statement in virtual time, and a weight
    // is the rate virtual time runs at: the change must not hand it a turn or
    // take one away. So its lag either side of the change is compared, and the
    // queue's own invariants are what say the sums followed.
    let mut queue = queue();
    for id in 0..3u64 {
        queue
            .enqueue(id, id, EntityState::new(1024), slot())
            .unwrap();
    }
    let _ = queue.pick_next();
    let running = queue.current_id().unwrap();
    let _ = queue.update_curr(SLICE / 2);
    let before = real_lag(&queue, running);

    assert_eq!(queue.set_weight(running, 2048), Ok(true));
    check(&queue);

    assert_eq!(queue.current_id(), Some(running), "it stopped running");
    assert_eq!(queue.get(running).unwrap().weight, 2048);
    let after = real_lag(&queue, running);
    assert!(
        after.abs_diff(before) <= 1_000,
        "it was owed {before} ns of the CPU and is now owed {after} ns"
    );
}

/// What entity `id` is owed in real nanoseconds: its virtual lag runs at the
/// rate its weight sets, so the two are only the same thing at nice 0.
fn real_lag(queue: &RunQueue<u64>, id: u64) -> i64 {
    let view = queue.get(id).unwrap();
    (i128::from(view.lag) * i128::from(view.weight) / i128::from(NICE_0_WEIGHT)) as i64
}

#[test]
fn reweighing_the_same_weight_does_not_extend_a_turn() {
    // A program may set the nice value it already has as often as it likes;
    // that must not be a way to ask for a fresh slice each time.
    let mut queue = queue();
    for id in 0..2u64 {
        queue
            .enqueue(id, id, EntityState::new(1024), slot())
            .unwrap();
    }
    let _ = queue.pick_next();
    let running = queue.current_id().unwrap();
    let _ = queue.update_curr(SLICE / 2);
    let left = queue.remaining_ns().unwrap();
    for _ in 0..10 {
        assert_eq!(queue.set_weight(running, 1024), Ok(true));
    }
    assert_eq!(
        queue.remaining_ns(),
        Some(left),
        "asking for the weight it had gave it more of the CPU"
    );
    check(&queue);
}

#[test]
fn a_weight_is_refused_for_nobody_and_for_zero() {
    let mut queue = queue();
    queue.enqueue(1, 1, EntityState::new(1024), slot()).unwrap();
    assert_eq!(queue.set_weight(7, 1024), Ok(false), "seven is not there");
    assert_eq!(queue.set_weight(1, 0), Err(SchedError::ZeroWeight));
    assert_eq!(queue.get(1).unwrap().weight, 1024, "the refusal changed it");
    check(&queue);
}

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
fn the_real_time_modes_are_named_and_refused() {
    for mode in [Mode::SoftRt, Mode::HardRt] {
        assert_eq!(
            mode.classes(),
            Err(SchedError::ModeUnavailable(mode)),
            "{} is stage 14's",
            mode.name()
        );
        assert!(!mode.steals_work(), "{} does not steal", mode.name());
        assert_eq!(
            Domain::new(CpuSet::first(1).unwrap(), mode),
            Err(SchedError::ModeUnavailable(mode)),
            "and no domain can be built in it"
        );
    }
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
        Err(SchedError::NoSuchCpu(MAX_CPUS)),
        "past the end"
    );
    assert!(!set.contains(MAX_CPUS), "and not there");
}

#[test]
fn an_empty_domain_is_refused() {
    assert_eq!(
        Domain::new(CpuSet::empty(), Mode::Throughput),
        Err(SchedError::EmptyDomain),
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
        Err(SchedError::Uncovered(2)),
        "two CPUs in no domain"
    );
    assert_eq!(
        check_partition(&[low, everything], 4),
        Err(SchedError::Overlap(0)),
        "two domains claiming CPU 0"
    );
    assert_eq!(
        check_partition(&[everything], 3),
        Err(SchedError::NoSuchCpu(3)),
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
    quietest, slice_for,
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
fn a_slice_is_a_share_of_the_target_latency() {
    let target = 24_000_000;
    let minimum = 1_000_000;
    assert_eq!(slice_for(target, minimum, 1), target);
    assert_eq!(slice_for(target, minimum, 4), target / 4);
    assert_eq!(slice_for(target, minimum, 8), target / 8);
}

#[test]
fn a_slice_never_falls_below_the_floor() {
    let target = 24_000_000;
    let minimum = 1_000_000;
    // A thousand runnable would ask for 24 microseconds, which is a machine
    // that does nothing but switch.
    assert_eq!(slice_for(target, minimum, 1000), minimum);
    assert_eq!(slice_for(target, minimum, usize::MAX), minimum);
}

#[test]
fn a_slice_is_defined_for_an_empty_queue() {
    assert_eq!(slice_for(24_000_000, 1_000_000, 0), 24_000_000);
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

/// The 128-bit formula `carried_weight` replaced, as the kernel's
/// `quota::effective` computed it before 2f.
fn carried_weight_128(base: u32, levels: &[(u32, i64)]) -> u128 {
    let mut weight = u128::from(base);
    let mut below = i64::from(base);
    for &(own, load) in levels {
        let load = load.max(below).max(1);
        let own = i64::from(own);
        weight = weight.saturating_mul(u128::try_from(own).unwrap_or(1))
            / u128::try_from(load).unwrap_or(1);
        below = own;
    }
    weight
}

/// A small generator, so the sweep is the same every run.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A weight, often at a boundary.
    fn weight(&mut self) -> u32 {
        const EDGES: [u32; 8] = [0, 1, 2, 15, 1024, 88_761, u32::MAX - 1, u32::MAX];
        let pick = self.next();
        if pick.is_multiple_of(4) {
            EDGES[(pick / 4 % 8) as usize]
        } else {
            (self.next() >> (self.next() % 33)) as u32
        }
    }

    /// A load, often at a boundary, sometimes below what it must be at least.
    fn load(&mut self) -> i64 {
        const EDGES: [i64; 7] = [i64::MIN, -1, 0, 1, 2, u32::MAX as i64, i64::MAX];
        let pick = self.next();
        if pick.is_multiple_of(4) {
            EDGES[(pick / 4 % 7) as usize]
        } else {
            (self.next() >> (self.next() % 64)) as i64
        }
    }
}

/// `carried_weight` in 64 bits equals the 128-bit formula, bit for bit, over
/// random and boundary weights and loads at job depths 1 to 8: every entity
/// weight below 2^32, as a `u32` is.
///
/// Verifies: L.object.160
#[test]
fn the_carried_weight_is_the_wide_formula() {
    let mut random = Xorshift(0x9E37_79B9_7F4A_7C15);
    for depth in 1..=8 {
        for _ in 0..50_000 {
            let base = random.weight();
            let levels: Vec<(u32, i64)> = (0..depth)
                .map(|_| (random.weight(), random.load()))
                .collect();
            let wide = carried_weight_128(base, &levels);
            let narrow = carried_weight(base, levels.iter().copied());
            assert_eq!(
                u128::from(narrow),
                wide,
                "base {base}, levels {levels:?}: 64 bits gave {narrow}, 128 gave {wide}"
            );
        }
    }
    // The largest products the bound allows, at every depth.
    for depth in 1..=8 {
        let levels: Vec<(u32, i64)> = (0..depth).map(|_| (u32::MAX, 0_i64)).collect();
        assert_eq!(
            u128::from(carried_weight(u32::MAX, levels.iter().copied())),
            carried_weight_128(u32::MAX, &levels),
            "at depth {depth} with every weight u32::MAX"
        );
    }
}

// ---------------------------------------------------------------------------
// The direct switch's hand-over (docs/OPAQUE-KERNEL.md §9.7, part 1)
// ---------------------------------------------------------------------------

/// A queue running one entity, `11`, made from the same draws each time it
/// is called with a copy of `rng`: base, weight, lag brought, time run.
fn running_one(rng: &mut Rng) -> RunQueue<u64> {
    let mut queue = queue();
    // Virtual time near the counter's wrap, a third of the time.
    queue.zero = match rng.below(3) {
        0 => u64::MAX - rng.below(SLICE * 4),
        1 => rng.below(SLICE * 4),
        _ => rng.next(),
    };
    let weight = drawn_weight(rng);
    let vlag = drawn_lag(rng, weight);
    let sum_exec = rng.next() >> 8;
    queue
        .enqueue(
            11,
            11,
            EntityState {
                weight,
                vlag,
                sum_exec,
            },
            slot(),
        )
        .unwrap();
    let _ = queue.pick_next();
    let _ = queue.update_curr(rng.below(SLICE * 3));
    let _ = queue.set_slice_ns(SLICE / (1 + rng.below(8)));
    queue
}

/// A weight: the boundaries a third of the time, else any nice level's or
/// any value.
fn drawn_weight(rng: &mut Rng) -> u32 {
    match rng.below(4) {
        0 => [1, 15, NICE_0_WEIGHT, 88761, u32::MAX >> 8][rng.below(5) as usize],
        1 => weight_of_nice(rng.below(40) as i32 - 20).unwrap(),
        _ => 1 + (rng.next() % 200_000) as u32,
    }
}

/// A lag brought: zero, at the clamp, past it either way, or anything.
fn drawn_lag(rng: &mut Rng, weight: u32) -> i64 {
    let limit = i64::try_from(to_virtual(SLICE, weight).saturating_mul(2)).unwrap_or(i64::MAX);
    match rng.below(5) {
        0 => 0,
        1 => limit,
        2 => -limit.saturating_mul(3),
        3 => limit.saturating_add(1 + rng.below(1000) as i64),
        _ => (rng.next() as i64) >> (rng.below(40) + 20),
    }
}

/// Every quantity two queues hold, compared.
#[track_caller]
fn same_queue(a: &RunQueue<u64>, b: &RunQueue<u64>, case: u64) {
    assert_eq!(a.zero, b.zero, "case {case}: the base");
    assert_eq!(a.sum, b.sum, "case {case}: the weighted sum");
    assert_eq!(a.load, b.load, "case {case}: the load");
    assert_eq!(a.config, b.config, "case {case}: the slice");
    assert_eq!(a.tree.len(), b.tree.len(), "case {case}: the waiting");
    let (ca, cb) = (a.curr.as_ref().unwrap(), b.curr.as_ref().unwrap());
    let (ea, eb) = (&ca.entity, &cb.entity);
    assert_eq!(
        (
            ea.id,
            ea.weight,
            ea.vruntime,
            ea.deadline,
            ea.sum_exec,
            ea.payload
        ),
        (
            eb.id,
            eb.weight,
            eb.vruntime,
            eb.deadline,
            eb.sum_exec,
            eb.payload
        ),
        "case {case}: the running entity"
    );
}

/// `hand_over` is the general sequence -- `enqueue`, the rescale,
/// `remove_curr`, `pick_next` -- on a queue with one entity running and
/// nothing waiting: random states, weights and lags at and past the clamp,
/// virtual times either side of the wrap, every field compared.
///
/// Verifies: `L.sched.55`, `H.SCHED.13`
#[test]
fn hand_over_is_the_general_sequence() {
    let mut rng = Rng(0x00D1_EC75_17C4);
    for case in 0..200_000 {
        let seed = rng.next() | 1;
        let mut general = running_one(&mut Rng(seed));
        let mut direct = running_one(&mut Rng(seed));
        same_queue(&general, &direct, case);
        let weight = drawn_weight(&mut rng);
        let arriving = EntityState {
            weight,
            vlag: drawn_lag(&mut rng, weight),
            sum_exec: rng.next() >> 8,
        };
        let slice_after = slice_for(SLICE, SLICE / 8, 2 + rng.below(2) as usize);

        general.enqueue(22, 22, arriving, slot()).unwrap();
        general.set_slice_ns(slice_after).unwrap();
        let left_general = general.remove_curr().unwrap();
        assert_eq!(general.pick_next(), Some(&22));

        let left_direct = direct
            .hand_over(22, 22, arriving, slot(), slice_after)
            .unwrap()
            .unwrap();

        same_queue(&general, &direct, case);
        assert_eq!(
            (left_general.0, left_general.1, left_general.2),
            (left_direct.0, left_direct.1, left_direct.2),
            "case {case}: the leaving entity"
        );
        check(&direct);
    }
}

/// Refused as `enqueue` refuses, with nothing changed: a weight of zero, and
/// the running entity's own name.
#[test]
fn hand_over_refuses_as_enqueue_does() {
    let mut queue = running_one(&mut Rng(7));
    let refused = queue
        .hand_over(22, 22, EntityState::new(0), slot(), SLICE)
        .unwrap_err();
    assert_eq!(refused.reason, SchedError::ZeroWeight);
    let refused = queue
        .hand_over(11, 11, EntityState::new(NICE_0_WEIGHT), slot(), SLICE)
        .unwrap_err();
    assert_eq!(refused.reason, SchedError::Duplicate(11));
    assert_eq!(queue.current_id(), Some(11));
    check(&queue);
}

/// The 64-bit shortcuts of the wide arithmetic answer what the 128-bit
/// formulas answer, at random and at every boundary they switch at.
#[test]
fn the_narrow_arithmetic_is_the_wide_arithmetic() {
    let wide_virtual = |real: u64, weight: u32| {
        u64::try_from(u128::from(real) * u128::from(NICE_0_WEIGHT) / u128::from(weight.max(1)))
            .unwrap_or(u64::MAX)
    };
    let wide_real = |virt: u64, weight: u32| {
        u64::try_from(u128::from(virt) * u128::from(weight) / u128::from(NICE_0_WEIGHT))
            .unwrap_or(u64::MAX)
    };
    let mut rng = Rng(0xA110_F7E5);
    let edges = [
        0,
        1,
        1023,
        1024,
        (1 << 54) - 1,
        1 << 54,
        (1 << 54) + 1,
        u64::MAX / 2,
        u64::MAX,
    ];
    for case in 0..1_000_000_u64 {
        let value = if case % 4 == 0 {
            edges[(case / 4 % edges.len() as u64) as usize]
        } else {
            rng.next() >> rng.below(64)
        };
        let weight = match case % 3 {
            0 => weight_of_nice(rng.below(40) as i32 - 20).unwrap(),
            1 => [0, 1, NICE_0_WEIGHT, u32::MAX][rng.below(4) as usize],
            _ => rng.next() as u32,
        };
        assert_eq!(
            to_virtual(value, weight),
            wide_virtual(value, weight),
            "{value} {weight}"
        );
        assert_eq!(
            to_real(value, weight),
            wide_real(value, weight),
            "{value} {weight}"
        );
        let numerator = i128::from(rng.next() as i64) * if case % 5 == 0 { 1 << 60 } else { 1 };
        let numerator = if case % 7 == 0 {
            i128::from(i64::MIN)
        } else {
            numerator
        };
        let denominator = 1 + i128::from(rng.next() >> rng.below(64));
        let denominator = if case % 11 == 0 {
            i128::from(i64::MAX) + 1
        } else {
            denominator
        };
        assert_eq!(
            floor_div(numerator, denominator),
            numerator.div_euclid(denominator)
        );
        assert_eq!(
            truncating_div(numerator, denominator),
            numerator / denominator
        );
    }
}

/// A slot taken apart into its node and put together again
/// (`Slot::into_box`, `Slot::from_box`, the kernel's `SlotCell`) is the same
/// node and serves a queue as one never taken apart: enqueued, picked and
/// handed back, through a round trip at each step.
///
/// Verifies: L.sched.63
#[test]
fn a_slot_taken_apart_and_put_together_serves_a_queue() {
    let config = Config {
        slice_ns: 1_000_000,
    };
    let mut queue: RunQueue<u32> = RunQueue::new(config).expect("a queue");
    let slot = Slot::<u32>::new().expect("a slot");
    let node = slot.into_box();
    let address = core::ptr::from_ref(&*node);
    let slot = Slot::from_box(node);
    queue
        .enqueue(1, 10, EntityState::new(NICE_0_WEIGHT), slot)
        .map_err(|_| ())
        .expect("enqueued");
    assert_eq!(queue.pick_next().copied(), Some(10));
    let (id, payload, _, slot) = queue.remove_curr().expect("it ran");
    assert_eq!((id, payload), (1, 10));
    let node = slot.into_box();
    assert!(
        core::ptr::eq(address, &raw const *node),
        "the node a queue gave back was not the one it was lent"
    );
    let slot = Slot::from_box(node);
    queue
        .enqueue(1, 11, EntityState::new(NICE_0_WEIGHT), slot)
        .map_err(|_| ())
        .expect("enqueued again");
    assert_eq!(queue.pick_next().copied(), Some(11));
}

// ---------------------------------------------------------------------------
// The direct switch's job loads (docs/OPAQUE-KERNEL.md §9.11, J1 and J2)
// ---------------------------------------------------------------------------

/// `carried_weight_with` adds its extra to the first level's load and to no
/// other: it equals `carried_weight` of the same levels with the first
/// load raised (saturating), at depths 1 to 8, over random and boundary
/// weights, loads and extras. The equality with the 128-bit formula is
/// `the_carried_weight_is_the_wide_formula`'s.
#[test]
fn the_carried_weight_with_an_extra_raises_the_first_load_alone() {
    let mut random = Xorshift(0x2545_F491_4F6C_DD1D);
    for depth in 1..=8 {
        for _ in 0..20_000 {
            let base = random.weight();
            let levels: Vec<(u32, i64)> = (0..depth)
                .map(|_| (random.weight(), random.load()))
                .collect();
            let extra = match random.next() % 4 {
                0 => 0,
                1 => i64::from(random.weight()),
                2 => random.load(),
                _ => -i64::from(random.weight()),
            };
            let mut raised = levels.clone();
            if let Some(first) = raised.first_mut() {
                first.1 = first.1.saturating_add(extra);
            }
            assert_eq!(
                carried_weight_with(base, levels.iter().copied(), extra),
                carried_weight(base, raised.iter().copied()),
                "base {base}, levels {levels:?}, extra {extra}"
            );
        }
    }
}

/// A job of the model: its parent, its own weight, its load and what it
/// adds to its parent's.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelJob {
    parent: Option<u32>,
    weight: u32,
    load: i64,
    contributed: i64,
}

/// The index the model's "no job" is, as the kernel's `quota::NONE`.
const MODEL_NONE: u32 = u32::MAX;

/// `quota::adjust` (src/kernel/src/object/quota.rs) on the model: the load
/// changes, and every job above whose busy or idle state that flipped. A
/// model of the kernel's code, which this crate cannot call: a drift between
/// the two is caught only by review (as the slot model's is, ledger line
/// 420).
fn model_adjust(jobs: &mut [ModelJob], index: u32, delta: i64) {
    let mut at = Some(index);
    let mut delta = delta;
    while delta != 0 {
        let Some(job) = at.and_then(|at| jobs.get_mut(at as usize)) else {
            return;
        };
        let before = job.load;
        job.load = before.saturating_add(delta);
        let after = job.load;
        if (before > 0) == (after > 0) {
            return;
        }
        let fresh = if after > 0 { i64::from(job.weight) } else { 0 };
        delta = fresh - job.contributed;
        job.contributed = fresh;
        at = job.parent;
    }
}

/// `quota::effective_with`'s levels on the model, from `index` up.
fn model_weight(jobs: &[ModelJob], index: u32, base: u32, extra: i64) -> u64 {
    let mut at = Some(index);
    let levels = core::iter::from_fn(|| {
        let job = jobs.get(at? as usize)?;
        at = job.parent;
        Some((job.weight, job.load))
    });
    carried_weight_with(base, levels, extra)
}

/// The direct switch's plan and settlement, the kernel's own functions,
/// give what the general sequence gives: the peer's join (`adjust` by its
/// weight), the caller's charge and the peer's weight read with it counted,
/// then the caller's leave -- the same load and contribution at every job
/// and the same two weights -- over random job trees of depth 1 to 8 with
/// siblings and nested jobs, the caller and the peer in one job, in
/// siblings, in a job and its child either way, and in jobs apart; bases
/// equal and unequal both ways; other tasks counted or none (the job the
/// caller alone keeps busy). A fold is planned exactly when both run in one
/// job.
///
/// Verifies: L.sched.66
#[test]
fn the_direct_switch_folds_its_job_loads_as_the_general_sequence_leaves_them() {
    let mut random = Xorshift(0xD1B5_4A32_D192_ED03);
    let mut folds = 0_u32;
    let mut apart = 0_u32;
    for round in 0..40_000_u32 {
        // A forest: each job's parent an earlier job, or none, so depths
        // run from 1 to as many jobs as there are.
        let count = 1 + (random.next() % 8) as u32;
        let mut jobs: Vec<ModelJob> = (0..count)
            .map(|index| ModelJob {
                parent: (index > 0 && random.next() % 5 != 0)
                    .then(|| (random.next() % u64::from(index)) as u32),
                weight: 1 + (random.next() % 200_000) as u32,
                load: 0,
                contributed: 0,
            })
            .collect();
        // Other tasks, counted where they run; none at all a quarter of the
        // time.
        if round % 4 != 0 {
            for _ in 0..(random.next() % 6) {
                let at = (random.next() % u64::from(count)) as u32;
                model_adjust(&mut jobs, at, 1 + (random.next() % 90_000) as i64);
            }
        }
        let weight = |random: &mut Xorshift| 1 + (random.next() % 90_000) as u32;
        let caller_job = (random.next() % u64::from(count)) as u32;
        let peer_job = if random.next() % 2 == 0 {
            caller_job
        } else {
            (random.next() % u64::from(count)) as u32
        };
        let caller_base = weight(&mut random);
        let peer_base = if random.next() % 2 == 0 {
            caller_base
        } else {
            weight(&mut random)
        };
        // The caller runs, counted.
        model_adjust(&mut jobs, caller_job, i64::from(caller_base));

        // The general sequence.
        let mut general = jobs.clone();
        model_adjust(&mut general, peer_job, i64::from(peer_base));
        let caller_due = model_weight(&general, caller_job, caller_base, 0);
        let peer_due = model_weight(&general, peer_job, peer_base, 0);
        model_adjust(&mut general, caller_job, -i64::from(caller_base));

        // The direct switch's.
        let mut folded = jobs.clone();
        let plan = plan_job_fold(
            Some((peer_job, peer_base)),
            (caller_job, caller_base),
            MODEL_NONE,
        );
        let extra = match plan {
            JobPlan::Fold { job, joined } => {
                folds += 1;
                Some((job, i64::from(joined)))
            }
            JobPlan::Separate => {
                apart += 1;
                model_adjust(&mut folded, peer_job, i64::from(peer_base));
                None
            }
        };
        let extra_at = |at: u32| extra.filter(|&(job, _)| job == at).map_or(0, |(_, e)| e);
        let caller_folded = model_weight(&folded, caller_job, caller_base, extra_at(caller_job));
        let peer_folded = model_weight(&folded, peer_job, peer_base, extra_at(peer_job));
        match plan {
            JobPlan::Fold { job, joined } => {
                let (changes, planned) =
                    settle_job_fold(job, joined, Some((caller_job, caller_base)));
                assert!(planned, "round {round}: a fold's leave answered as planned was not");
                for (at, delta) in changes.into_iter().flatten() {
                    model_adjust(&mut folded, at, delta);
                }
            }
            JobPlan::Separate => {
                model_adjust(&mut folded, caller_job, -i64::from(caller_base));
            }
        }

        let what = || {
            std::format!(
                "round {round}: caller in {caller_job} at {caller_base}, peer in {peer_job} at \
                 {peer_base}, plan {plan:?}, jobs {jobs:?}"
            )
        };
        assert_eq!(
            plan == JobPlan::Separate,
            caller_job != peer_job,
            "a fold planned at an unshared level, or none at a shared one: {}",
            what()
        );
        assert_eq!(folded, general, "the job loads differ: {}", what());
        assert_eq!(caller_folded, caller_due, "the caller's weight differs: {}", what());
        assert_eq!(peer_folded, peer_due, "the peer's weight differs: {}", what());
    }
    assert!(folds > 1_000 && apart > 1_000, "folds {folds}, apart {apart}");
}

/// A fold's leave that answers other than planned makes the peer's join and
/// then the caller's leave, each in its own job, as the general path makes
/// them, and says it was not planned; one as planned makes one change by the
/// net, or none at zero.
#[test]
fn a_fold_settles_by_the_net_or_in_the_general_order() {
    assert_eq!(settle_job_fold(3, 700, Some((3, 700))), ([None, None], true));
    assert_eq!(settle_job_fold(3, 700, Some((3, 500))), ([Some((3, 200)), None], true));
    assert_eq!(settle_job_fold(3, 500, Some((3, 700))), ([Some((3, -200)), None], true));
    assert_eq!(
        settle_job_fold(3, 700, Some((4, 500))),
        ([Some((3, 700)), Some((4, -500))], false)
    );
    assert_eq!(settle_job_fold(3, 700, None), ([Some((3, 700)), None], false));
    assert_eq!(plan_job_fold(None, (3, 700), MODEL_NONE), JobPlan::Separate);
    assert_eq!(plan_job_fold(Some((3, 700)), (3, 0), MODEL_NONE), JobPlan::Separate);
    assert_eq!(
        plan_job_fold(Some((MODEL_NONE, 700)), (MODEL_NONE, 700), MODEL_NONE),
        JobPlan::Separate
    );
    assert_eq!(
        plan_job_fold(Some((3, 700)), (3, 500), MODEL_NONE),
        JobPlan::Fold { job: 3, joined: 700 }
    );
}
