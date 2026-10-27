//! `SCHED_FIFO` and `SCHED_RR`: fixed priorities above the fair class
//! (docs/SCHED-CLASSES.md §9e; the consultant's design review, ledger line
//! 29, R1-R15).
//!
//! **Priorities 1 to 99**, higher first, as Linux numbers them; within a
//! priority, arrival order. A `SCHED_FIFO` entity runs until it blocks,
//! yields or is preempted by a higher priority, and a preempted one keeps
//! the head of its priority. A `SCHED_RR` entity also has a quantum,
//! [`RR_QUANTUM_NS`], at whose end it goes to the tail of its priority if
//! another waits there; its quantum restarts after a yield and after it was
//! preempted, as Linux's does. A yield goes to the tail.
//!
//! **The floor.** As Linux's `sched_rt_runtime_us` does by default, the
//! class spends at most [`RUNTIME_NS`] of every [`PERIOD_NS`] of real time
//! on its entities while something below it waits, so the fair class keeps
//! at least 50 ms of every second on each processor (H.SCHED.20). While
//! throttled it holds its entities (`len` is honest) but is not
//! [`SchedClass::ready`], so a stack picks below it, preempts nothing and
//! arms the period's end, not a timer at once (R3, R4). With nothing below
//! waiting the floor does not bind. The period's clock is the class's own,
//! advanced by every charge on the processor, the lower tier's through
//! [`SchedClass::elapse`]. A `SCHED_FIFO` entity at priority 99 starves the
//! kernel's fair worker tasks (the reaper, the network's) for up to 950 ms
//! of every second (C2).
//!
//! Both constants are the crate's, never a run-time setting (R13).
//!
//! **The queue** is a balanced tree of the entities' own nodes, keyed by
//! (priority descending, an arrival sequence): picking is the first node,
//! O(log n). Removal by id walks the tree, O(n), as the fair class's does;
//! it serves `sched_setscheduler`, a move and an exit, never the switch
//! path. Each entity's node is its [`FifoSlot`], allocated fallibly when
//! its task first asks for a real-time policy (R1), so no scheduler path
//! allocates.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use core::cmp::Ordering;

use ferrix_sched_class::{Back, Handed, LOAD_UNIT, Left, RealtimeClass, Refusal, SchedClass};

/// The lowest real-time priority.
pub const MIN_PRIORITY: u8 = Params::MIN_PRIORITY;
/// The highest real-time priority.
pub const MAX_PRIORITY: u8 = Params::MAX_PRIORITY;
/// A `SCHED_RR` entity's quantum: Linux's default `sched_rr_timeslice_ms`.
pub const RR_QUANTUM_NS: u64 = 100_000_000;
/// The throttle's period: Linux's default `sched_rt_period_us`.
pub const PERIOD_NS: u64 = 1_000_000_000;
/// The most of each period the class runs while something below waits:
/// Linux's default `sched_rt_runtime_us`.
pub const RUNTIME_NS: u64 = 950_000_000;

/// A real-time policy and the parameters a task asks for: the interface's,
/// which any real-time class keeps the same way.
pub use ferrix_sched_class::{RtParams as Params, RtPolicy as Policy};

/// Where a node sits: higher priority first, then arrival.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Key {
    /// `MAX_PRIORITY - priority`, so that the higher priority orders first.
    rank: u8,
    /// The arrival sequence.
    seq: u64,
}

impl Key {
    fn order(self, other: Key) -> Ordering {
        self.rank.cmp(&other.rank).then(self.seq.cmp(&other.seq))
    }
}

type Link<T> = Option<Box<Node<T>>>;

/// One entity's node in the tree.
#[derive(Debug)]
pub struct Node<T> {
    id: u64,
    params: Params,
    key: Key,
    payload: Option<T>,
    left: Link<T>,
    right: Link<T>,
    height: u8,
}

/// A task's node, allocated once, fallibly, before it is ever queued (R1).
#[derive(Debug)]
pub struct FifoSlot<T>(Box<Node<T>>);

impl<T> FifoSlot<T> {
    /// A fresh slot.
    ///
    /// # Errors
    ///
    /// No memory for the node.
    pub fn new() -> Result<FifoSlot<T>, ferrix_fallible::AllocError> {
        ferrix_fallible::try_box(Node {
            id: 0,
            params: Params {
                policy: Policy::Fifo,
                priority: MIN_PRIORITY,
            },
            key: Key { rank: 0, seq: 0 },
            payload: None,
            left: None,
            right: None,
            height: 1,
        })
        .map(FifoSlot)
    }
}

/// What a task carries for the class while off its queue: its parameters
/// and its node.
pub type FifoEntity<T> = (Params, FifoSlot<T>);

/// The class.
#[derive(Debug)]
pub struct FifoRr;

/// One processor's queue.
#[derive(Debug)]
pub struct FifoQueue<T> {
    tree: Link<T>,
    len_waiting: usize,
    running: Option<Box<Node<T>>>,
    /// The next arrival sequence.
    seq: u64,
    /// The running `SCHED_RR` entity's use of its quantum.
    quantum_used: u64,
    /// Real time into the current period.
    period_at: u64,
    /// The class's running time in the current period.
    spent: u64,
}

impl<T> FifoQueue<T> {
    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        seq
    }

    /// Whether the class has spent its runtime in this period.
    const fn throttled(&self) -> bool {
        self.spent >= RUNTIME_NS
    }

    /// Advance the period clock by `ns`, starting a new period, with its
    /// budget refilled, at each period's end.
    fn advance(&mut self, ns: u64) {
        let at = self.period_at.saturating_add(ns);
        if at >= PERIOD_NS {
            self.period_at = at % PERIOD_NS;
            self.spent = 0;
        } else {
            self.period_at = at;
        }
    }

    fn insert(&mut self, mut node: Box<Node<T>>, seq: u64) {
        node.key = Key {
            rank: MAX_PRIORITY - node.params.priority,
            seq,
        };
        node.left = None;
        node.right = None;
        node.height = 1;
        self.tree = Some(insert(self.tree.take(), node));
        self.len_waiting += 1;
    }

    fn take_first(&mut self) -> Option<Box<Node<T>>> {
        let first = first_key(&self.tree)?;
        self.take_key(first)
    }

    fn take_key(&mut self, key: Key) -> Option<Box<Node<T>>> {
        let (rest, taken) = remove(self.tree.take(), key);
        self.tree = rest;
        if taken.is_some() {
            self.len_waiting -= 1;
        }
        taken
    }

    /// Whether a waiting entity has the running one's priority.
    fn same_priority_waits(&self) -> bool {
        match (&self.running, first_key(&self.tree)) {
            (Some(running), Some(first)) => first.rank == running.key.rank,
            _ => false,
        }
    }

    /// Whether the first waiting entity outranks the running one.
    fn higher_waits(&self) -> bool {
        match (&self.running, first_key(&self.tree)) {
            (Some(running), Some(first)) => first.rank < running.key.rank,
            (None, Some(_)) => true,
            _ => false,
        }
    }
}

fn height<T>(link: &Link<T>) -> u8 {
    link.as_ref().map_or(0, |node| node.height)
}

fn fix<T>(node: &mut Node<T>) {
    node.height = 1 + height(&node.left).max(height(&node.right));
}

fn rotate_right<T>(mut node: Box<Node<T>>) -> Box<Node<T>> {
    let Some(mut left) = node.left.take() else {
        return node;
    };
    node.left = left.right.take();
    fix(&mut node);
    left.right = Some(node);
    fix(&mut left);
    left
}

fn rotate_left<T>(mut node: Box<Node<T>>) -> Box<Node<T>> {
    let Some(mut right) = node.right.take() else {
        return node;
    };
    node.right = right.left.take();
    fix(&mut node);
    right.left = Some(node);
    fix(&mut right);
    right
}

fn balance<T>(mut node: Box<Node<T>>) -> Box<Node<T>> {
    fix(&mut node);
    let lean = i16::from(height(&node.left)) - i16::from(height(&node.right));
    if lean > 1 {
        if let Some(left) = node.left.take() {
            node.left = Some(if height(&left.left) < height(&left.right) {
                rotate_left(left)
            } else {
                left
            });
        }
        return rotate_right(node);
    }
    if lean < -1 {
        if let Some(right) = node.right.take() {
            node.right = Some(if height(&right.right) < height(&right.left) {
                rotate_right(right)
            } else {
                right
            });
        }
        return rotate_left(node);
    }
    node
}

fn insert<T>(link: Link<T>, new: Box<Node<T>>) -> Box<Node<T>> {
    let Some(mut node) = link else {
        return new;
    };
    if new.key.order(node.key) == Ordering::Less {
        node.left = Some(insert(node.left.take(), new));
    } else {
        node.right = Some(insert(node.right.take(), new));
    }
    balance(node)
}

fn first_key<T>(link: &Link<T>) -> Option<Key> {
    let mut node = link.as_ref()?;
    while let Some(left) = node.left.as_ref() {
        node = left;
    }
    Some(node.key)
}

/// Take the first node out of the subtree rooted at `node`: (what is left,
/// the first node).
fn pop_first<T>(mut node: Box<Node<T>>) -> (Link<T>, Box<Node<T>>) {
    match node.left.take() {
        None => (node.right.take(), node),
        Some(left) => {
            let (rest, first) = pop_first(left);
            node.left = rest;
            (Some(balance(node)), first)
        }
    }
}

/// Take the node with `key` out of the subtree: (what is left, the node).
fn remove<T>(link: Link<T>, key: Key) -> (Link<T>, Link<T>) {
    let Some(mut node) = link else {
        return (None, None);
    };
    match key.order(node.key) {
        Ordering::Less => {
            let (rest, taken) = remove(node.left.take(), key);
            node.left = rest;
            (Some(balance(node)), taken)
        }
        Ordering::Greater => {
            let (rest, taken) = remove(node.right.take(), key);
            node.right = rest;
            (Some(balance(node)), taken)
        }
        Ordering::Equal => {
            let left = node.left.take();
            let rest = match node.right.take() {
                None => left,
                Some(right) => {
                    let (right_rest, mut successor) = pop_first(right);
                    successor.left = left;
                    successor.right = right_rest;
                    Some(balance(successor))
                }
            };
            (rest, Some(node))
        }
    }
}

/// The key of the node for `id` in the subtree, by a walk.
fn find<T>(link: &Link<T>, id: u64) -> Option<Key> {
    let node = link.as_ref()?;
    if node.id == id {
        return Some(node.key);
    }
    find(&node.left, id).or_else(|| find(&node.right, id))
}

/// The subtree's size, whether it is ordered and balanced, for `check`.
fn audit<T>(link: &Link<T>, low: Option<Key>, high: Option<Key>) -> Result<usize, &'static str> {
    let Some(node) = link.as_ref() else {
        return Ok(0);
    };
    if low.is_some_and(|low| node.key.order(low) != Ordering::Greater)
        || high.is_some_and(|high| node.key.order(high) != Ordering::Less)
    {
        return Err("a fifo queue's tree is out of order");
    }
    let lean = i16::from(height(&node.left)) - i16::from(height(&node.right));
    if !(-1..=1).contains(&lean) || node.height != 1 + height(&node.left).max(height(&node.right)) {
        return Err("a fifo queue's tree is out of balance");
    }
    if node.payload.is_none() {
        return Err("a fifo queue's node holds no payload");
    }
    let left = audit(&node.left, low, Some(node.key))?;
    let right = audit(&node.right, Some(node.key), high)?;
    Ok(left + right + 1)
}

fn back<T>(payload: T, entity: FifoEntity<T>, reason: Refusal) -> Back<T, FifoEntity<T>> {
    Back {
        payload,
        entity,
        reason,
    }
}

impl<T> SchedClass<T> for FifoRr {
    const NAME: &'static str = "fifo_rr";
    type Queue = FifoQueue<T>;
    type Entity = FifoEntity<T>;
    type Config = ();

    fn new_queue((): ()) -> Result<FifoQueue<T>, Refusal> {
        Ok(FifoQueue {
            tree: None,
            len_waiting: 0,
            running: None,
            seq: 0,
            quantum_used: 0,
            period_at: 0,
            spent: 0,
        })
    }

    fn len(queue: &FifoQueue<T>) -> usize {
        queue.len_waiting + usize::from(queue.running.is_some())
    }

    fn enqueue(
        queue: &mut FifoQueue<T>,
        id: u64,
        payload: T,
        (params, FifoSlot(mut node)): FifoEntity<T>,
    ) -> Result<(), Back<T, FifoEntity<T>>> {
        if !params.valid() {
            return Err(back(payload, (params, FifoSlot(node)), Refusal::Invalid));
        }
        if queue
            .running
            .as_ref()
            .is_some_and(|running| running.id == id)
            || find(&queue.tree, id).is_some()
        {
            return Err(back(
                payload,
                (params, FifoSlot(node)),
                Refusal::Duplicate(id),
            ));
        }
        node.id = id;
        node.params = params;
        node.payload = Some(payload);
        let seq = queue.next_seq();
        queue.insert(node, seq);
        Ok(())
    }

    fn remove(queue: &mut FifoQueue<T>, id: u64) -> Option<(T, FifoEntity<T>)> {
        if queue
            .running
            .as_ref()
            .is_some_and(|running| running.id == id)
        {
            return Self::remove_current(queue).map(|(_, payload, entity)| (payload, entity));
        }
        let key = find(&queue.tree, id)?;
        let mut node = queue.take_key(key)?;
        let payload = node.payload.take()?;
        Some((payload, (node.params, FifoSlot(node))))
    }

    fn remove_current(queue: &mut FifoQueue<T>) -> Option<Left<T, FifoEntity<T>>> {
        let mut node = queue.running.take()?;
        queue.quantum_used = 0;
        let payload = node.payload.take()?;
        Some((node.id, payload, (node.params, FifoSlot(node))))
    }

    fn pick(queue: &mut FifoQueue<T>) -> Option<&T> {
        if let Some(running) = queue.running.take() {
            let rotate = running.params.policy == Policy::Rr && queue.quantum_used >= RR_QUANTUM_NS;
            // A preempted entity keeps the head of its priority (its key);
            // an RR entity whose quantum is spent goes to the tail.
            let seq = if rotate {
                queue.next_seq()
            } else {
                running.key.seq
            };
            if rotate {
                queue.quantum_used = 0;
            }
            queue.insert(running, seq);
        }
        let next = queue.take_first()?;
        if queue.running.is_none() {
            queue.quantum_used = 0;
        }
        queue.running = Some(next);
        queue
            .running
            .as_ref()
            .and_then(|node| node.payload.as_ref())
    }

    fn put_back(queue: &mut FifoQueue<T>) {
        if let Some(running) = queue.running.take() {
            // Preempted: the head of its priority, its quantum restarting.
            let seq = running.key.seq;
            queue.quantum_used = 0;
            queue.insert(running, seq);
        }
    }

    fn current(queue: &FifoQueue<T>) -> Option<&T> {
        queue
            .running
            .as_ref()
            .and_then(|node| node.payload.as_ref())
    }

    fn hand_over(
        queue: &mut FifoQueue<T>,
        id: u64,
        payload: T,
        entity: FifoEntity<T>,
        _: u64,
    ) -> Handed<T, FifoEntity<T>> {
        Self::enqueue(queue, id, payload, entity)?;
        let left = Self::remove_current(queue);
        let _ = Self::pick(queue);
        Ok(left)
    }

    fn charge(queue: &mut FifoQueue<T>, ran_ns: u64) -> bool {
        queue.advance(ran_ns);
        if queue.running.is_none() {
            return false;
        }
        queue.spent = queue.spent.saturating_add(ran_ns);
        queue.quantum_used = queue.quantum_used.saturating_add(ran_ns);
        let rr_due = queue
            .running
            .as_ref()
            .is_some_and(|node| node.params.policy == Policy::Rr)
            && queue.quantum_used >= RR_QUANTUM_NS
            && queue.same_priority_waits();
        // A spent budget is a decision too: the stack picks below if
        // anything waits there (`ready`).
        rr_due || queue.higher_waits() || queue.throttled()
    }

    fn remaining_ns(queue: &FifoQueue<T>) -> Option<u64> {
        let running = queue.running.as_ref()?;
        let budget = RUNTIME_NS.saturating_sub(queue.spent);
        Some(match running.params.policy {
            Policy::Rr => budget.min(RR_QUANTUM_NS.saturating_sub(queue.quantum_used)),
            Policy::Fifo => budget,
        })
    }

    fn should_preempt(queue: &FifoQueue<T>) -> bool {
        queue.higher_waits()
            || (queue
                .running
                .as_ref()
                .is_some_and(|node| node.params.policy == Policy::Rr)
                && queue.quantum_used >= RR_QUANTUM_NS
                && queue.same_priority_waits())
    }

    fn decision_in_ns(queue: &FifoQueue<T>) -> Option<u64> {
        if queue.throttled() && Self::len(queue) > 0 {
            // Throttled: the period's end is the next decision (R4).
            return Some(PERIOD_NS.saturating_sub(queue.period_at));
        }
        let running = queue.running.as_ref()?;
        if queue.len_waiting == 0 {
            return Some(RUNTIME_NS.saturating_sub(queue.spent));
        }
        match running.params.policy {
            Policy::Rr if queue.same_priority_waits() => Self::remaining_ns(queue),
            _ => Some(RUNTIME_NS.saturating_sub(queue.spent)),
        }
    }

    fn load_weight(queue: &FifoQueue<T>) -> u64 {
        (Self::len(queue) as u64).saturating_mul(LOAD_UNIT)
    }

    fn yield_current(queue: &mut FifoQueue<T>) {
        if let Some(running) = queue.running.take() {
            let seq = queue.next_seq();
            queue.quantum_used = 0;
            queue.insert(running, seq);
            if let Some(next) = queue.take_first() {
                queue.running = Some(next);
            }
        }
    }

    fn ready(queue: &FifoQueue<T>, below_waiting: bool) -> bool {
        Self::len(queue) > 0 && !(queue.throttled() && below_waiting)
    }

    fn elapse(queue: &mut FifoQueue<T>, ns: u64) {
        queue.advance(ns);
    }

    fn steals_work(_: &FifoQueue<T>) -> bool {
        false
    }

    #[inline(never)]
    fn check(queue: &FifoQueue<T>) -> Result<(), &'static str> {
        if audit(&queue.tree, None, None)? != queue.len_waiting {
            return Err("a fifo queue's count of waiting entities is wrong");
        }
        if queue
            .running
            .as_ref()
            .is_some_and(|node| node.payload.is_none() || !node.params.valid())
        {
            return Err("a fifo queue's running entity is not one");
        }
        if queue.period_at >= PERIOD_NS {
            return Err("a fifo queue's period clock is past its period");
        }
        Ok(())
    }
}

impl<T> RealtimeClass<T> for FifoRr {
    type Slot = FifoSlot<T>;

    fn new_slot() -> Option<FifoSlot<T>> {
        FifoSlot::new().ok()
    }

    fn entity(params: Params, slot: FifoSlot<T>) -> FifoEntity<T> {
        (params, slot)
    }

    fn into_slot((_, slot): FifoEntity<T>) -> FifoSlot<T> {
        slot
    }
}

#[cfg(test)]
mod tests;
