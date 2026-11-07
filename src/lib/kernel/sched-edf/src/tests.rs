//! The class's rules, on the host (ledger 7088, 7763 DG.4, DG.7): alone, and stacked over a toy
//! class that stands in for the fair class below it.

extern crate alloc;
extern crate std;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use ferrix_sched_class::deadline::DeadlineClass;
use ferrix_sched_class::{Back, Handed, Left, Over, Refusal as ClassRefusal, SchedClass, Tier};

use super::*;

const MS: u64 = 1_000_000;

/// Below: every task in arrival order, one at a time, never preempting.
struct Below;

#[derive(Default)]
struct BelowQueue {
    waiting: VecDeque<u64>,
    running: Option<u64>,
}

impl SchedClass<u64> for Below {
    const NAME: &'static str = "below";
    type Queue = BelowQueue;
    type Entity = ();
    type Config = ();
    fn new_queue((): ()) -> Result<BelowQueue, ClassRefusal> {
        Ok(BelowQueue::default())
    }
    fn len(queue: &BelowQueue) -> usize {
        queue.waiting.len() + usize::from(queue.running.is_some())
    }
    fn enqueue(queue: &mut BelowQueue, id: u64, _: u64, (): ()) -> Result<(), Back<u64, ()>> {
        queue.waiting.push_back(id);
        Ok(())
    }
    fn remove(queue: &mut BelowQueue, id: u64) -> Option<(u64, ())> {
        if queue.running == Some(id) {
            queue.running = None;
            return Some((id, ()));
        }
        let at = queue.waiting.iter().position(|&each| each == id)?;
        queue.waiting.remove(at).map(|id| (id, ()))
    }
    fn remove_current(queue: &mut BelowQueue) -> Option<Left<u64, ()>> {
        queue.running.take().map(|id| (id, id, ()))
    }
    fn pick(queue: &mut BelowQueue) -> Option<&u64> {
        if queue.running.is_none() {
            queue.running = queue.waiting.pop_front();
        }
        queue.running.as_ref()
    }
    fn put_back(queue: &mut BelowQueue) {
        if let Some(id) = queue.running.take() {
            queue.waiting.push_front(id);
        }
    }
    fn current(queue: &BelowQueue) -> Option<&u64> {
        queue.running.as_ref()
    }
    fn hand_over(queue: &mut BelowQueue, id: u64, payload: u64, (): (), _: u64) -> Handed<u64, ()> {
        Self::enqueue(queue, id, payload, ())?;
        let left = Self::remove_current(queue);
        let _ = Self::pick(queue);
        Ok(left)
    }
    fn charge(_: &mut BelowQueue, _: u64) -> bool {
        false
    }
    fn remaining_ns(_: &BelowQueue) -> Option<u64> {
        None
    }
    fn should_preempt(_: &BelowQueue) -> bool {
        false
    }
    fn decision_in_ns(_: &BelowQueue) -> Option<u64> {
        None
    }
    fn load_weight(queue: &BelowQueue) -> u64 {
        Self::len(queue) as u64
    }
    fn yield_current(_: &mut BelowQueue) {}
    fn ready(queue: &BelowQueue, _: bool) -> bool {
        Self::len(queue) > 0
    }
    fn elapse(_: &mut BelowQueue, _: u64) {}
    fn steals_work(_: &BelowQueue) -> bool {
        true
    }
    fn check(_: &BelowQueue) -> Result<(), &'static str> {
        Ok(())
    }
}

type Stack = Over<Edf, Below>;
type StackQueue = <Stack as SchedClass<u64>>::Queue;

fn alone() -> EdfQueue<u64> {
    <Edf as SchedClass<u64>>::new_queue(()).unwrap()
}

fn stack() -> StackQueue {
    Stack::new_queue(((), ())).unwrap()
}

fn plain(runtime_ms: u64, period_ms: u64) -> Reservation {
    Reservation {
        runtime_ns: runtime_ms * MS,
        deadline_ns: period_ms * MS,
        period_ns: period_ms * MS,
    }
}

/// Queue `id` in the stack: on the deadline tier if it is admitted there, else below.
fn put(q: &mut StackQueue, id: u64) {
    let entity = if Edf::is_reserved(&q.upper, id) {
        Tier::Upper(Admitted)
    } else {
        Tier::Lower(())
    };
    Stack::enqueue(q, id, id, entity).unwrap();
}

/// Run `step` ns of whatever the stack picks at `now`, as the kernel does: the time told, the
/// charge, the next decision. What ran.
fn step(q: &mut StackQueue, now: &mut u64, step: u64) -> Option<u64> {
    Edf::note_time(&mut q.upper, *now);
    let ran = Stack::pick(q).copied();
    *now += step;
    Edf::note_time(&mut q.upper, *now);
    if ran.is_some() {
        let _ = Stack::charge(q, step);
    }
    ran
}

#[test]
fn a_malformed_reservation_is_refused_by_name() {
    let mut q = alone();
    for asked in [
        Reservation {
            runtime_ns: LEAST_RUNTIME_NS - 1,
            deadline_ns: 10 * MS,
            period_ns: 10 * MS,
        },
        plain(11, 10),
        Reservation {
            runtime_ns: MS,
            deadline_ns: 5 * MS,
            period_ns: 10 * MS,
        },
        Reservation {
            runtime_ns: LEAST_RUNTIME_NS,
            deadline_ns: LEAST_PERIOD_NS - 1,
            period_ns: LEAST_PERIOD_NS - 1,
        },
        Reservation {
            runtime_ns: MS,
            deadline_ns: MOST_PERIOD_NS + 1,
            period_ns: MOST_PERIOD_NS + 1,
        },
    ] {
        assert_eq!(
            Edf::admit(&mut q, 1, asked),
            Err(Refusal::Malformed),
            "{asked:?}"
        );
    }
    assert_eq!(Edf::state(&q).0, 0);
}

#[test]
fn an_unschedulable_set_is_refused_and_changes_nothing() {
    let mut q = alone();
    Edf::admit(&mut q, 1, plain(5, 10)).unwrap();
    Edf::admit(&mut q, 2, plain(4, 10)).unwrap();
    assert_eq!(
        Edf::admit(&mut q, 3, plain(1, 10)),
        Err(Refusal::Unschedulable)
    );
    assert_eq!(Edf::state(&q).0, 900_000);
    assert_eq!(Edf::admit(&mut q, 1, plain(1, 100)), Err(Refusal::Already));
    <Edf as SchedClass<u64>>::check(&q).unwrap();
}

#[test]
fn past_the_most_a_processor_refuses_by_name() {
    let mut q = alone();
    for id in 0..MOST as u64 {
        Edf::admit(&mut q, id, plain(1, 1000)).unwrap();
    }
    assert_eq!(Edf::admit(&mut q, 99, plain(1, 1000)), Err(Refusal::Full));
}

#[test]
fn a_task_with_no_admission_here_is_refused_no_such_tier() {
    let mut q = alone();
    let back = <Edf as SchedClass<u64>>::enqueue(&mut q, 7, 7, Admitted).unwrap_err();
    assert_eq!((back.payload, back.reason), (7, ClassRefusal::NoSuchTier));
}

#[test]
fn the_earliest_deadline_runs_and_the_tier_below_after_it() {
    let mut q = stack();
    Edf::admit(&mut q.upper, 1, plain(2, 20)).unwrap();
    Edf::admit(&mut q.upper, 2, plain(2, 10)).unwrap();
    let mut now = 1_000 * MS;
    Edf::note_time(&mut q.upper, now);
    for id in [100, 1, 2] {
        put(&mut q, id);
    }
    let order: Vec<_> = (0..6).filter_map(|_| step(&mut q, &mut now, MS)).collect();
    assert_eq!(order, [2, 2, 1, 1, 100, 100]);
}

#[test]
fn over_its_budget_a_reservation_is_throttled_until_its_period_ends() {
    let mut q = stack();
    Edf::admit(&mut q.upper, 1, plain(2, 10)).unwrap();
    let mut now = 1_000 * MS;
    Edf::note_time(&mut q.upper, now);
    put(&mut q, 1);
    put(&mut q, 100);
    let ran: Vec<_> = (0..12).filter_map(|_| step(&mut q, &mut now, MS)).collect();
    assert_eq!(ran, [1, 1, 100, 100, 100, 100, 100, 100, 100, 100, 1, 1]);
    let (_, throttles, refills) = Edf::state(&q.upper);
    assert_eq!((throttles, refills), (2, 1));
}

#[test]
fn a_throttled_reservation_with_nothing_else_leaves_the_processor_idle_and_a_timer_set() {
    let mut q = stack();
    Edf::admit(&mut q.upper, 1, plain(2, 10)).unwrap();
    let mut now = 1_000 * MS;
    Edf::note_time(&mut q.upper, now);
    put(&mut q, 1);
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    // Nothing waits below, and still it does not run: the budget is hard.
    assert_eq!(Stack::pick(&mut q).copied(), None);
    assert!(!Stack::ready(&q, false));
    assert_eq!(Stack::decision_in_ns(&q), Some(8 * MS));
}

#[test]
fn a_wake_with_too_little_time_left_starts_a_fresh_period() {
    let mut q = alone();
    Edf::admit(&mut q, 1, plain(4, 10)).unwrap();
    Edf::note_time(&mut q, 1_000 * MS);
    <Edf as SchedClass<u64>>::enqueue(&mut q, 1, 1, Admitted).unwrap();
    let _ = Edf::pick(&mut q);
    let _ = <Edf as SchedClass<u64>>::charge(&mut q, MS);
    let _ = <Edf as SchedClass<u64>>::remove_current(&mut q);
    // 3 ms left, 1 ms to its deadline: it would ask for more than its bandwidth.
    Edf::note_time(&mut q, 1_009 * MS);
    <Edf as SchedClass<u64>>::enqueue(&mut q, 1, 1, Admitted).unwrap();
    let _ = Edf::pick(&mut q);
    assert_eq!(<Edf as SchedClass<u64>>::remaining_ns(&q), Some(4 * MS));
}

#[test]
fn a_wake_with_enough_time_left_keeps_its_budget_and_deadline() {
    let mut q = alone();
    Edf::admit(&mut q, 1, plain(4, 10)).unwrap();
    Edf::note_time(&mut q, 1_000 * MS);
    <Edf as SchedClass<u64>>::enqueue(&mut q, 1, 1, Admitted).unwrap();
    let _ = Edf::pick(&mut q);
    let _ = <Edf as SchedClass<u64>>::charge(&mut q, MS);
    let _ = <Edf as SchedClass<u64>>::remove_current(&mut q);
    Edf::note_time(&mut q, 1_002 * MS);
    <Edf as SchedClass<u64>>::enqueue(&mut q, 1, 1, Admitted).unwrap();
    let _ = Edf::pick(&mut q);
    assert_eq!(<Edf as SchedClass<u64>>::remaining_ns(&q), Some(3 * MS));
}

#[test]
fn a_yield_gives_up_the_budget_until_the_next_period() {
    let mut q = stack();
    Edf::admit(&mut q.upper, 1, plain(4, 10)).unwrap();
    let mut now = 1_000 * MS;
    Edf::note_time(&mut q.upper, now);
    put(&mut q, 1);
    put(&mut q, 100);
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    Stack::yield_current(&mut q);
    assert_eq!(step(&mut q, &mut now, MS), Some(100));
}

#[test]
fn an_earlier_deadline_waking_preempts() {
    let mut q = stack();
    Edf::admit(&mut q.upper, 1, plain(5, 100)).unwrap();
    Edf::admit(&mut q.upper, 2, plain(1, 10)).unwrap();
    let mut now = 1_000 * MS;
    Edf::note_time(&mut q.upper, now);
    put(&mut q, 1);
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    assert!(!Stack::should_preempt(&q));
    put(&mut q, 2);
    assert!(Stack::should_preempt(&q));
}

#[test]
fn a_withdrawn_reservation_returns_its_share_at_once_and_goes_once_off_the_queue() {
    let mut q = stack();
    Edf::admit(&mut q.upper, 1, plain(4, 10)).unwrap();
    let mut now = 1_000 * MS;
    Edf::note_time(&mut q.upper, now);
    put(&mut q, 1);
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    assert!(Edf::withdraw(&mut q.upper, 1));
    assert_eq!(Edf::state(&q.upper).0, 0);
    // Still running here until its processor's next decision moves it, which takes it off.
    assert!(Edf::is_reserved(&q.upper, 1));
    assert!(!Edf::withdraw(&mut q.upper, 1));
    let (payload, entity) = Stack::remove(&mut q, 1).unwrap();
    assert!(matches!(entity, Tier::Upper(Admitted)));
    assert!(!Edf::is_reserved(&q.upper, 1));
    Stack::enqueue(&mut q, 1, payload, Tier::Lower(())).unwrap();
    assert_eq!(step(&mut q, &mut now, MS), Some(1));
    Stack::check(&q).unwrap();
}

#[test]
fn a_stall_past_a_period_is_not_paid_back() {
    let mut q = alone();
    Edf::admit(&mut q, 1, plain(2, 10)).unwrap();
    Edf::note_time(&mut q, 1_000 * MS);
    <Edf as SchedClass<u64>>::enqueue(&mut q, 1, 1, Admitted).unwrap();
    let _ = Edf::pick(&mut q);
    let _ = <Edf as SchedClass<u64>>::charge(&mut q, 2 * MS);
    Edf::note_time(&mut q, 1_100 * MS);
    let _ = Edf::pick(&mut q);
    assert_eq!(<Edf as SchedClass<u64>>::remaining_ns(&q), Some(2 * MS));
    assert_eq!(Edf::state(&q).2, 1);
}

#[test]
fn nothing_steals_a_reservation_and_the_direct_switch_never_fuses() {
    let mut q = alone();
    Edf::admit(&mut q, 1, plain(2, 10)).unwrap();
    Edf::admit(&mut q, 2, plain(2, 20)).unwrap();
    Edf::note_time(&mut q, 1_000 * MS);
    <Edf as SchedClass<u64>>::enqueue(&mut q, 2, 2, Admitted).unwrap();
    let _ = Edf::pick(&mut q);
    assert!(!Edf::steals_work(&q));
    // The arrival does not simply replace the running one: it is the earliest deadline's turn.
    let left = Edf::hand_over(&mut q, 1, 1, Admitted, 0).unwrap();
    assert_eq!(left.map(|(id, _, _)| id), Some(2));
    assert_eq!(Edf::current(&q).copied(), Some(1));
}

#[test]
fn a_given_back_entry_counts_toward_the_most_until_it_goes() {
    let mut q = alone();
    for id in 0..MOST as u64 {
        Edf::admit(&mut q, id, plain(1, 1000)).unwrap();
    }
    Edf::note_time(&mut q, 1_000 * MS);
    <Edf as SchedClass<u64>>::enqueue(&mut q, 0, 0, Admitted).unwrap();
    assert!(Edf::withdraw(&mut q, 0));
    assert_eq!(Edf::admit(&mut q, 99, plain(1, 1000)), Err(Refusal::Full));
    let _ = <Edf as SchedClass<u64>>::remove(&mut q, 0);
    Edf::admit(&mut q, 99, plain(1, 1000)).unwrap();
}

/// Ledger 7088 S.6 as a property (7763 DG.7, ED.3): every admitted set of implicit-deadline
/// reservations gets its whole budget in every period, and the tier below has the rest.
#[test]
fn every_admitted_set_meets_every_deadline_and_the_tier_below_has_the_rest() {
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
        let mut q = stack();
        let begun = 1_000 * MS;
        let mut now = begun;
        Edf::note_time(&mut q.upper, now);
        let mut admitted = Vec::new();
        for id in 1..=6_u64 {
            let period = (1 + random(5)) * 4 * MS;
            let runtime = (2 + random(period / STEP / 2)) * STEP;
            let asked = Reservation {
                runtime_ns: runtime.min(period),
                deadline_ns: period,
                period_ns: period,
            };
            if Edf::admit(&mut q.upper, id, asked).is_ok() {
                put(&mut q, id);
                admitted.push((id, asked));
            }
        }
        if Edf::state(&q.upper).0 > 800_000 {
            tight += 1;
        }
        put(&mut q, 100);
        // Every period divides 240 ms, the multiples of 4 to 20 having that as their lcm.
        let total = 240 * MS;
        let mut in_period = alloc::collections::BTreeMap::<u64, u64>::new();
        let mut below = 0_u64;
        let mut reserved = 0_u64;
        let end = begun + total;
        while now < end {
            match step(&mut q, &mut now, STEP) {
                Some(100) => below += STEP,
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
            reserved + below,
            total,
            "the processor was never left idle with work"
        );
        Stack::check(&q).unwrap();
    }
    assert!(
        cfg!(miri) || tight > 10,
        "the sets are too light to show anything: {tight}"
    );
}
