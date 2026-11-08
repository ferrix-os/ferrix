//! `dormant` and the stack's shortcut on it (ledger 8057, SD.1 and SD.3):
//! `FifoRr` says it is dormant exactly when it holds nothing, and then
//! every answer `Over` leaves unasked is the one it would have had; and an
//! `Over` with `FifoRr` above answers everything as the same stack whose
//! upper tier never says it is dormant, over the states a walk reaches.

extern crate std;

use std::collections::VecDeque;
use std::vec::Vec;

use ferrix_sched_class::{Back, Handed, LOAD_UNIT, Left, Over, Refusal, SchedClass, Tier};

use super::{FifoQueue, FifoRr, FifoSlot, PERIOD_NS, Params, Policy, RR_QUANTUM_NS, RUNTIME_NS};

/// `FifoRr` with `dormant` forced false: the stack then asks it everything,
/// as it did before the shortcut.
struct Undormant;

impl SchedClass<u64> for Undormant {
    const NAME: &'static str = "undormant";
    type Queue = FifoQueue<u64>;
    type Entity = (Params, FifoSlot<u64>);
    type Config = ();
    fn new_queue((): ()) -> Result<FifoQueue<u64>, Refusal> {
        FifoRr::new_queue(())
    }
    fn len(q: &FifoQueue<u64>) -> usize {
        FifoRr::len(q)
    }
    fn enqueue(
        q: &mut FifoQueue<u64>,
        id: u64,
        payload: u64,
        entity: Self::Entity,
    ) -> Result<(), Back<u64, Self::Entity>> {
        FifoRr::enqueue(q, id, payload, entity)
    }
    fn remove(q: &mut FifoQueue<u64>, id: u64) -> Option<(u64, Self::Entity)> {
        FifoRr::remove(q, id)
    }
    fn remove_current(q: &mut FifoQueue<u64>) -> Option<Left<u64, Self::Entity>> {
        FifoRr::remove_current(q)
    }
    fn pick(q: &mut FifoQueue<u64>) -> Option<&u64> {
        FifoRr::pick(q)
    }
    fn put_back(q: &mut FifoQueue<u64>) {
        FifoRr::put_back(q);
    }
    fn current(q: &FifoQueue<u64>) -> Option<&u64> {
        FifoRr::current(q)
    }
    fn hand_over(
        q: &mut FifoQueue<u64>,
        id: u64,
        payload: u64,
        entity: Self::Entity,
        slice_after: u64,
    ) -> Handed<u64, Self::Entity> {
        FifoRr::hand_over(q, id, payload, entity, slice_after)
    }
    fn charge(q: &mut FifoQueue<u64>, ran_ns: u64) -> bool {
        FifoRr::charge(q, ran_ns)
    }
    fn remaining_ns(q: &FifoQueue<u64>) -> Option<u64> {
        FifoRr::remaining_ns(q)
    }
    fn should_preempt(q: &FifoQueue<u64>) -> bool {
        FifoRr::should_preempt(q)
    }
    fn decision_in_ns(q: &FifoQueue<u64>) -> Option<u64> {
        FifoRr::decision_in_ns(q)
    }
    fn load_weight(q: &FifoQueue<u64>) -> u64 {
        FifoRr::load_weight(q)
    }
    fn yield_current(q: &mut FifoQueue<u64>) {
        FifoRr::yield_current(q);
    }
    fn ready(q: &FifoQueue<u64>, below_waiting: bool) -> bool {
        FifoRr::ready(q, below_waiting)
    }
    fn elapse(q: &mut FifoQueue<u64>, ns: u64) {
        FifoRr::elapse(q, ns);
    }
    fn steals_work(q: &FifoQueue<u64>) -> bool {
        FifoRr::steals_work(q)
    }
    fn dormant(_: &FifoQueue<u64>) -> bool {
        false
    }
    fn check(q: &FifoQueue<u64>) -> Result<(), &'static str> {
        FifoRr::check(q)
    }
}

/// A lower tier for the stack: arrival order, a fixed quantum, stealing.
struct Below;

#[derive(Default)]
struct BelowQueue {
    waiting: VecDeque<u64>,
    running: Option<(u64, u64)>,
    elapsed: u64,
}

const BELOW_QUANTUM: u64 = 3_000_000;

impl SchedClass<u64> for Below {
    const NAME: &'static str = "below";
    type Queue = BelowQueue;
    type Entity = ();
    type Config = ();
    fn new_queue((): ()) -> Result<BelowQueue, Refusal> {
        Ok(BelowQueue::default())
    }
    fn len(q: &BelowQueue) -> usize {
        q.waiting.len() + usize::from(q.running.is_some())
    }
    fn enqueue(q: &mut BelowQueue, id: u64, payload: u64, (): ()) -> Result<(), Back<u64, ()>> {
        if q.waiting.contains(&id) || q.running.is_some_and(|(other, _)| other == id) {
            return Err(Back {
                payload,
                entity: (),
                reason: Refusal::Duplicate(id),
            });
        }
        q.waiting.push_back(id);
        Ok(())
    }
    fn remove(q: &mut BelowQueue, id: u64) -> Option<(u64, ())> {
        if q.running.is_some_and(|(other, _)| other == id) {
            return q.running.take().map(|(id, _)| (id, ()));
        }
        let at = q.waiting.iter().position(|&other| other == id)?;
        q.waiting.remove(at).map(|id| (id, ()))
    }
    fn remove_current(q: &mut BelowQueue) -> Option<Left<u64, ()>> {
        q.running.take().map(|(id, _)| (id, id, ()))
    }
    fn pick(q: &mut BelowQueue) -> Option<&u64> {
        if q.running.is_none() {
            q.running = q.waiting.pop_front().map(|id| (id, 0));
        }
        q.running.as_ref().map(|(id, _)| id)
    }
    fn put_back(q: &mut BelowQueue) {
        if let Some((id, _)) = q.running.take() {
            q.waiting.push_front(id);
        }
    }
    fn current(q: &BelowQueue) -> Option<&u64> {
        q.running.as_ref().map(|(id, _)| id)
    }
    fn hand_over(q: &mut BelowQueue, id: u64, payload: u64, (): (), _: u64) -> Handed<u64, ()> {
        Self::enqueue(q, id, payload, ())?;
        let left = Self::remove_current(q);
        let _ = Self::pick(q);
        Ok(left)
    }
    fn charge(q: &mut BelowQueue, ran_ns: u64) -> bool {
        q.running.as_mut().is_some_and(|(_, used)| {
            *used += ran_ns;
            *used >= BELOW_QUANTUM
        })
    }
    fn remaining_ns(q: &BelowQueue) -> Option<u64> {
        q.running
            .map(|(_, used)| BELOW_QUANTUM.saturating_sub(used))
    }
    fn should_preempt(q: &BelowQueue) -> bool {
        !q.waiting.is_empty() && q.running.is_none_or(|(_, used)| used >= BELOW_QUANTUM)
    }
    fn decision_in_ns(q: &BelowQueue) -> Option<u64> {
        if q.waiting.is_empty() {
            return None;
        }
        Some(
            q.running
                .map_or(0, |(_, used)| BELOW_QUANTUM.saturating_sub(used)),
        )
    }
    fn load_weight(q: &BelowQueue) -> u64 {
        Below::len(q) as u64 * LOAD_UNIT
    }
    fn yield_current(q: &mut BelowQueue) {
        if let Some((id, _)) = q.running.take() {
            q.waiting.push_back(id);
        }
    }
    fn ready(q: &BelowQueue, _: bool) -> bool {
        Below::len(q) > 0
    }
    fn elapse(q: &mut BelowQueue, ns: u64) {
        q.elapsed += ns;
    }
    fn steals_work(_: &BelowQueue) -> bool {
        true
    }
    fn dormant(_: &BelowQueue) -> bool {
        false
    }
    fn check(_: &BelowQueue) -> Result<(), &'static str> {
        Ok(())
    }
}

type Short = Over<FifoRr, Below>;
type Long = Over<Undormant, Below>;

fn entity(policy: Policy, priority: u8) -> (Params, FifoSlot<u64>) {
    (Params { policy, priority }, FifoSlot::new().unwrap())
}

/// Every answer `Over` gives, of each stack: they must agree.
#[derive(Debug, PartialEq, Eq)]
struct Answers {
    current: Option<u64>,
    remaining_ns: Option<u64>,
    decision_in_ns: Option<u64>,
    should_preempt: bool,
    ready: [bool; 2],
    load_weight: u64,
    len: usize,
}

fn answers<S: SchedClass<u64>>(q: &S::Queue) -> Answers {
    Answers {
        current: S::current(q).copied(),
        remaining_ns: S::remaining_ns(q),
        decision_in_ns: S::decision_in_ns(q),
        should_preempt: S::should_preempt(q),
        ready: [S::ready(q, false), S::ready(q, true)],
        load_weight: S::load_weight(q),
        len: S::len(q),
    }
}

/// SD.1: `FifoRr`'s `dormant` is `len() == 0`, and every answer it lets a
/// stack leave unasked is then the empty one.
fn check_dormant(q: &FifoQueue<u64>) {
    let dormant = <FifoRr as SchedClass<u64>>::dormant(q);
    assert_eq!(dormant, FifoRr::len(q) == 0);
    if dormant {
        assert!(!FifoRr::ready(q, false) && !FifoRr::ready(q, true));
        assert!(FifoRr::current(q).is_none());
        assert_eq!(FifoRr::remaining_ns(q), None);
        assert_eq!(FifoRr::decision_in_ns(q), None);
        assert!(!FifoRr::should_preempt(q));
        assert_eq!(FifoRr::load_weight(q), 0);
    }
}

/// A small deterministic generator, so that the walk is the same each run.
struct Walk(u64);

impl Walk {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

/// Verifies: L.sched.186
#[test]
fn fifo_rr_is_dormant_exactly_when_it_holds_nothing() {
    let mut q = <FifoRr as SchedClass<u64>>::new_queue(()).unwrap();
    // Empty, its period clock at rest.
    check_dormant(&q);
    // Waiting only.
    FifoRr::enqueue(&mut q, 1, 1, entity(Policy::Fifo, 5)).unwrap();
    check_dormant(&q);
    // Running only.
    let _ = FifoRr::pick(&mut q);
    check_dormant(&q);
    // Running and waiting, throttled.
    FifoRr::enqueue(&mut q, 2, 2, entity(Policy::Rr, 5)).unwrap();
    let _ = FifoRr::charge(&mut q, RUNTIME_NS);
    check_dormant(&q);
    // Empty again but throttled, its period clock running: dormant, and
    // nothing due though the period has not ended.
    let _ = FifoRr::remove_current(&mut q);
    let _ = FifoRr::remove(&mut q, 2);
    assert!(q.throttled());
    check_dormant(&q);
    FifoRr::elapse(&mut q, PERIOD_NS / 3);
    check_dormant(&q);
}

/// The two stacks, walked together.
struct Pair {
    short: <Short as SchedClass<u64>>::Queue,
    long: <Long as SchedClass<u64>>::Queue,
    walk: Walk,
    next_id: u64,
    ids: Vec<u64>,
}

impl Pair {
    fn new(seed: u64) -> Pair {
        Pair {
            short: Short::new_queue(((), ())).unwrap(),
            long: Long::new_queue(((), ())).unwrap(),
            walk: Walk(seed),
            next_id: 1,
            ids: Vec::new(),
        }
    }

    fn fresh_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The same entity for each stack: of the upper tier with `upper`.
    fn entities(
        &mut self,
        upper: bool,
    ) -> (
        <Short as SchedClass<u64>>::Entity,
        <Long as SchedClass<u64>>::Entity,
    ) {
        if !upper {
            return (Tier::Lower(()), Tier::Lower(()));
        }
        let policy = [Policy::Fifo, Policy::Rr][usize::try_from(self.walk.next(2)).unwrap()];
        let priority = u8::try_from(1 + self.walk.next(3)).unwrap();
        (
            Tier::Upper(entity(policy, priority)),
            Tier::Upper(entity(policy, priority)),
        )
    }

    fn arrive(&mut self) {
        let upper = self.walk.next(2) == 0;
        let id = self.fresh_id();
        let (a, b) = self.entities(upper);
        assert_eq!(
            Short::enqueue(&mut self.short, id, id, a).is_ok(),
            Long::enqueue(&mut self.long, id, id, b).is_ok()
        );
        self.ids.push(id);
    }

    fn charge(&mut self) {
        let ran = [
            0,
            1_000,
            BELOW_QUANTUM,
            RR_QUANTUM_NS,
            RUNTIME_NS / 2,
            PERIOD_NS,
        ][usize::try_from(self.walk.next(6)).unwrap()];
        assert_eq!(
            Short::charge(&mut self.short, ran),
            Long::charge(&mut self.long, ran)
        );
    }

    fn elapse(&mut self) {
        let ns = [1_000, RR_QUANTUM_NS, PERIOD_NS / 2][usize::try_from(self.walk.next(3)).unwrap()];
        Short::elapse(&mut self.short, ns);
        Long::elapse(&mut self.long, ns);
    }

    /// The direct switch's step, of either tier.
    fn hand_over(&mut self) {
        let upper = self.walk.next(3) == 0;
        let id = self.fresh_id();
        let (a, b) = self.entities(upper);
        let left_short = Short::hand_over(&mut self.short, id, id, a, 0)
            .ok()
            .map(|left| left.map(|(id, ..)| id));
        let left_long = Long::hand_over(&mut self.long, id, id, b, 0)
            .ok()
            .map(|left| left.map(|(id, ..)| id));
        assert_eq!(left_short, left_long);
        self.ids.push(id);
    }

    fn remove_current(&mut self) {
        let a = Short::remove_current(&mut self.short).map(|(id, ..)| id);
        let b = Long::remove_current(&mut self.long).map(|(id, ..)| id);
        assert_eq!(a, b);
    }

    fn remove(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        let at = usize::try_from(self.walk.next(self.ids.len() as u64)).unwrap();
        let id = self.ids.swap_remove(at);
        assert_eq!(
            Short::remove(&mut self.short, id).map(|(p, _)| p),
            Long::remove(&mut self.long, id).map(|(p, _)| p)
        );
    }

    fn step(&mut self) {
        match self.walk.next(9) {
            0 | 1 => self.arrive(),
            2 => assert_eq!(
                Short::pick(&mut self.short).copied(),
                Long::pick(&mut self.long).copied()
            ),
            3 => self.charge(),
            4 => self.elapse(),
            5 => self.hand_over(),
            6 => self.remove_current(),
            7 => {
                Short::yield_current(&mut self.short);
                Long::yield_current(&mut self.long);
            }
            _ => self.remove(),
        }
    }
}

/// Verifies: L.sched.186, L.sched.125, L.sched.126, L.sched.127
#[test]
fn a_stack_over_fifo_rr_answers_as_one_that_asks_everything() {
    for seed in 0..64 {
        let mut pair = Pair::new(seed);
        for _ in 0..400 {
            assert_eq!(
                answers::<Short>(&pair.short),
                answers::<Long>(&pair.long),
                "seed {seed}"
            );
            check_dormant(&pair.short.upper);
            pair.step();
            assert!(Short::check(&pair.short).is_ok() && Long::check(&pair.long).is_ok());
            assert_eq!(
                answers::<Short>(&pair.short),
                answers::<Long>(&pair.long),
                "seed {seed}"
            );
            // A stack is dormant only when both its tiers are; `Below` never is.
            assert!(!<Short as SchedClass<u64>>::dormant(&pair.short));
        }
    }
}
