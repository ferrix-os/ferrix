//! Scheduling logic: the EEVDF fair class, and the shape of the class stack.
//!
//! Stage 5 of `docs/ROADMAP.md`, and the half of the scheduler that is
//! arithmetic. The kernel's half — tasks, stacks, the context switch, the
//! per-CPU locks and the timer — lives in `src/kernel/src/sched/`. Everything
//! here is a pure function of the numbers it is handed, so `cargo test` can
//! drive a run queue through hundreds of thousands of decisions and check
//! every one, which a kernel that reboots on a mistake cannot.
//!
//! # EEVDF
//!
//! Earliest Eligible Virtual Deadline First (Stoica and Abdel-Wahab, 1995),
//! which Linux adopted in 6.6 in place of CFS. Each entity has a weight and a
//! *virtual runtime*, which advances as it runs at a rate inversely
//! proportional to its weight. The queue's *virtual time* is the
//! weight-average of its entities' virtual runtimes: where every one of them
//! would be had the CPU been shared out exactly in proportion to weight.
//!
//! * An entity whose virtual runtime is at or behind the queue's virtual time
//!   has had no more than its share. It is **eligible**.
//! * Each entity asks for a *slice* at a time, and its **virtual deadline** is
//!   its virtual runtime plus that slice scaled down by its weight.
//! * The scheduler runs the eligible entity with the earliest virtual
//!   deadline.
//!
//! What that buys over CFS is a bound. The difference between the service an
//! entity has been owed and the service it has had — its *lag* — stays within
//! the largest slice anybody asked for, where on real hardware "asked for" is
//! the slice plus however late the timer cut it. CFS promises fairness in the
//! limit; EEVDF promises it at every instant, to within one request. That is
//! the promise stage 5's exit test measures on a running machine, and the one
//! this crate's tests prove in simulation first.
//!
//! # Virtual time wraps, and is kept relative
//!
//! Virtual runtimes are `u64` nanoseconds and are allowed to wrap: two are
//! compared by the sign of their difference, which is right for any pair
//! closer than half the counter's range. The queue's virtual time is never
//! stored as an absolute number either. What is kept is a base, and the
//! weighted sum of every entity's offset from it — so the average is one
//! division, and eligibility is a comparison that needs none: an entity is
//! eligible exactly when the weighted sum of everybody's offset from it is not
//! negative. The base is moved to the average after every change, which keeps
//! the sum as small as the entities' spread rather than as large as the time
//! the queue has existed.
//!
//! # The running entity
//!
//! As in Linux, the entity that is running is held beside the tree rather
//! than in it — its virtual runtime changes on every update, and a key that
//! changes under a tree is a corrupt tree — but it is counted in the virtual
//! time throughout, because it is competing for the CPU as much as anything
//! queued.
//!
//! ```
//! use ferrix_sched::{Config, EntityState, NICE_0_WEIGHT, RunQueue, Slot};
//!
//! let mut queue: RunQueue<&str> = RunQueue::new(Config { slice_ns: 3_000_000 }).unwrap();
//! let state = EntityState::new(NICE_0_WEIGHT);
//! queue.enqueue(1, "one", state, Slot::new().unwrap()).unwrap();
//! queue.enqueue(2, "two", state, Slot::new().unwrap()).unwrap();
//!
//! // Equal weights, equal deadlines: the lower identifier runs first...
//! assert_eq!(queue.pick_next(), Some(&"one"));
//! // ...until its slice is spent, and then the other has the earlier deadline.
//! assert!(queue.update_curr(3_000_000));
//! assert_eq!(queue.pick_next(), Some(&"two"));
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod balance;
mod domain;
mod timeline;
mod tree;

#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use core::fmt;

pub use balance::{
    BALANCE_THRESHOLD, Balance, CpuLoad, LOAD_PERIOD_NS, LOAD_SCALE, Load, Placement, busiest,
    imbalance, place, quietest, slice_for,
};
pub use domain::{Class, CpuSet, Domain, MAX_CPUS, Mode, check_partition};
pub use timeline::Timeline;
use tree::{Key, Tree, before};
pub use tree::{Node, Slot};

/// The weight of a task at nice 0, and the unit every other weight is
/// measured in: an entity of this weight advances its virtual runtime at
/// exactly the rate it runs.
pub const NICE_0_WEIGHT: u32 = 1024;

/// Weights for nice -20 through 19: Linux's `sched_prio_to_weight`.
///
/// Each step is about 1.25 times the next, which is what makes "one nice level
/// is about ten per cent of the CPU" true between any two neighbours, whatever
/// else is running. The same table as Linux, so that a program that renices
/// itself gets the share it would get there.
const WEIGHTS: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291, // -20
    29154, 23254, 18705, 14949, 11916, // -15
    9548, 7620, 6100, 4904, 3906, // -10
    3121, 2501, 1991, 1586, 1277, // -5
    1024, 820, 655, 526, 423, // 0
    335, 272, 215, 172, 137, // 5
    110, 87, 70, 56, 45, // 10
    36, 29, 23, 18, 15, // 15
];

/// The weight for a nice value, or `None` outside -20..=19.
#[must_use]
pub fn weight_of_nice(nice: i32) -> Option<u32> {
    let index = usize::try_from(nice.checked_add(20)?).ok()?;
    WEIGHTS.get(index).copied()
}

/// A task's weight carried up its jobs: `base` times, at each level, that
/// job's own weight over its load, the load taken no smaller than what the
/// level below adds to it (`base` at the first level, then the level below's
/// own weight), nor than one. `levels` gives each level's own weight and
/// load, innermost first. Not clamped: the caller bounds the answer.
///
/// **In 64 bits, and exact.** The weight carried never exceeds the own
/// weight of the level below: true of `base` at the start, and kept by every
/// level, since `weight <= below <= load` makes `weight * own / load <= own`.
/// So each product is at most two entity weights multiplied, and with every
/// entity weight below 2^32 -- a `u32`, which `own` is -- it fits in a `u64`,
/// and the 64-bit quotient is the 128-bit formula's, bit for bit (the host
/// test holds it to that at depths 1 to 8). It was 128-bit arithmetic, a
/// software division on every architecture, at every wake
/// (OPAQUE-KERNEL.md §9.8, 2f).
#[must_use]
pub fn carried_weight(base: u32, levels: impl IntoIterator<Item = (u32, i64)>) -> u64 {
    let mut weight = u64::from(base);
    let mut below = u64::from(base);
    for (own, load) in levels {
        let load = u64::try_from(load).unwrap_or(0).max(below).max(1);
        let own = u64::from(own);
        weight = weight.saturating_mul(own) / load;
        below = own;
    }
    weight
}

/// Why something was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedError {
    /// A run queue was configured with a slice of zero, which would give every
    /// entity a deadline equal to its virtual runtime and make the pick a
    /// coin toss.
    ZeroSlice,
    /// An entity has a weight of zero: it could never be owed anything, and
    /// its virtual runtime would advance infinitely fast.
    ZeroWeight,
    /// An entity with this identifier is already on the queue.
    Duplicate(u64),
    /// A scheduling mode that is named, and not written yet.
    ModeUnavailable(Mode),
    /// A domain with no CPUs in it.
    EmptyDomain,
    /// A CPU claimed by two domains.
    Overlap(usize),
    /// A CPU claimed by no domain.
    Uncovered(usize),
    /// A CPU number past the end of a [`CpuSet`], or a domain naming a CPU
    /// the machine does not have.
    NoSuchCpu(usize),
}

impl fmt::Display for SchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedError::ZeroSlice => f.write_str("a run queue needs a slice longer than zero"),
            SchedError::ZeroWeight => f.write_str("an entity needs a weight above zero"),
            SchedError::Duplicate(id) => write!(f, "entity {id} is already queued"),
            SchedError::ModeUnavailable(mode) => {
                write!(f, "the {} mode is not written yet", mode.name())
            }
            SchedError::EmptyDomain => f.write_str("a scheduling domain has no CPUs"),
            SchedError::Overlap(cpu) => write!(f, "CPU {cpu} is in two scheduling domains"),
            SchedError::Uncovered(cpu) => write!(f, "CPU {cpu} is in no scheduling domain"),
            SchedError::NoSuchCpu(cpu) => write!(f, "there is no CPU {cpu}"),
        }
    }
}

/// How a run queue shares out its CPU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// How much CPU time an entity asks for at a time, in real nanoseconds.
    ///
    /// The unit of the fairness bound: no entity's lag strays further than
    /// one of these, plus the lateness of the timer that ends it.
    pub slice_ns: u64,
}

/// What an entity carries while it is off every run queue — asleep, or
/// between one CPU's queue and another's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntityState {
    /// Its share of the CPU, relative to [`NICE_0_WEIGHT`].
    pub weight: u32,
    /// Its lag when it left, in virtual nanoseconds: positive if it was owed
    /// service, negative if it had had more than its share.
    ///
    /// Kept so that sleeping cannot be used to cheat. An entity that leaves
    /// ahead of its share comes back still ahead, rather than with a clean
    /// slate it could collect by blocking for a moment every slice.
    pub vlag: i64,
    /// Real nanoseconds it has run for, ever. Carried rather than reset, so it
    /// is a lifetime total wherever the entity has been.
    pub sum_exec: u64,
}

impl EntityState {
    /// A fresh entity: owed nothing, and never run.
    #[must_use]
    pub const fn new(weight: u32) -> EntityState {
        EntityState {
            weight,
            vlag: 0,
            sum_exec: 0,
        }
    }
}

/// An entity the queue would not take, handed back with the reason.
///
/// The payload comes back rather than being dropped: in the kernel it is the
/// last reference to a task, and losing it would lose the task. So does the
/// slot it came in.
#[derive(Debug)]
pub struct Refused<T> {
    /// What the caller tried to queue.
    pub payload: T,
    /// The slot it came in.
    pub slot: Slot<T>,
    /// Why it was refused.
    pub reason: SchedError,
}

/// A running entity taken off its queue: its identifier, its data, what it
/// carries away and its slot, as [`RunQueue::remove_curr`] answers it.
pub type Left<T> = (u64, T, EntityState, Slot<T>);

/// One entity, while it is on a queue.
#[derive(Debug)]
pub(crate) struct Entity<T> {
    /// The caller's name for it.
    pub(crate) id: u64,
    /// Its share of the CPU.
    pub(crate) weight: u32,
    /// Virtual nanoseconds of service, advancing at `NICE_0_WEIGHT / weight`
    /// times the rate it runs.
    pub(crate) vruntime: u64,
    /// Where its current request ends, in virtual time.
    pub(crate) deadline: u64,
    /// Real nanoseconds it has run for.
    pub(crate) sum_exec: u64,
    /// The caller's data: always there while the entity is on a queue, and
    /// taken out as it leaves, so that the slot it came in goes back empty.
    pub(crate) payload: Option<T>,
}

impl<T> Entity<T> {
    /// Where it sorts in the tree.
    pub(crate) const fn key(&self) -> Key {
        Key {
            deadline: self.deadline,
            id: self.id,
        }
    }
}

/// An entity as the caller sees it.
#[derive(Debug)]
pub struct EntityView<'a, T> {
    /// The caller's name for it.
    pub id: u64,
    /// Its share of the CPU.
    pub weight: u32,
    /// Its virtual runtime.
    pub vruntime: u64,
    /// Its virtual deadline.
    pub deadline: u64,
    /// Real nanoseconds it has run for.
    pub sum_exec: u64,
    /// The queue's virtual time less its virtual runtime: what it is owed, in
    /// virtual nanoseconds. Eligible exactly when this is not negative.
    pub lag: i64,
    /// Whether it is the running entity rather than a queued one.
    pub running: bool,
    /// The caller's data.
    pub payload: &'a T,
}

/// One CPU's fair class: the entities competing for it, and the one running.
#[derive(Debug)]
pub struct RunQueue<T> {
    /// The slice every entity asks for.
    config: Config,
    /// Queued entities, in deadline order.
    tree: Tree<T>,
    /// The running entity, in its node, if any. Counted in `sum` and `load`;
    /// not in `tree`.
    curr: Option<Box<Node<T>>>,
    /// The base virtual runtimes are measured from.
    zero: u64,
    /// The weighted sum of every entity's virtual runtime less `zero`.
    sum: i128,
    /// The sum of every entity's weight.
    load: u64,
}

/// A real duration as the virtual time an entity of `weight` accrues in it.
///
/// In 64 bits when the product fits, and not at all for the unit weight: the
/// quotient is the 128-bit one either way (`docs/OPAQUE-KERNEL.md` §9.7,
/// part 7; the host tests hold every caller to it). A 128-bit division is a
/// call into the compiler's runtime, and a switch made several.
fn to_virtual(real_ns: u64, weight: u32) -> u64 {
    if weight == NICE_0_WEIGHT {
        return real_ns;
    }
    let weight = u64::from(weight.max(1));
    if let Some(product) = real_ns.checked_mul(u64::from(NICE_0_WEIGHT)) {
        return product / weight;
    }
    let scaled = u128::from(real_ns) * u128::from(NICE_0_WEIGHT) / u128::from(weight);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// A virtual duration as the real time it takes an entity of `weight` to
/// accrue it. In 64 bits when the product fits, as [`to_virtual`].
fn to_real(virtual_ns: u64, weight: u32) -> u64 {
    if weight == NICE_0_WEIGHT {
        return virtual_ns;
    }
    if let Some(product) = virtual_ns.checked_mul(u64::from(weight)) {
        return product / u64::from(NICE_0_WEIGHT);
    }
    let scaled = u128::from(virtual_ns) * u128::from(weight) / u128::from(NICE_0_WEIGHT);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// `numerator.div_euclid(denominator)` for a positive denominator, in 64
/// bits when both fit: the same quotient (see [`to_virtual`]).
fn floor_div(numerator: i128, denominator: i128) -> i128 {
    if let (Ok(numerator), Ok(denominator)) = (i64::try_from(numerator), i64::try_from(denominator))
        && denominator > 0
    {
        return i128::from(numerator.div_euclid(denominator));
    }
    numerator.div_euclid(denominator)
}

/// `numerator / denominator`, truncating, for a positive denominator, in 64
/// bits when both fit: the same quotient (see [`to_virtual`]).
fn truncating_div(numerator: i128, denominator: i128) -> i128 {
    if let (Ok(numerator), Ok(denominator)) = (i64::try_from(numerator), i64::try_from(denominator))
        && denominator > 0
    {
        return i128::from(numerator / denominator);
    }
    numerator / denominator
}

impl<T> RunQueue<T> {
    /// An empty queue.
    ///
    /// # Errors
    ///
    /// [`SchedError::ZeroSlice`] for a slice of zero.
    pub fn new(config: Config) -> Result<RunQueue<T>, SchedError> {
        if config.slice_ns == 0 {
            return Err(SchedError::ZeroSlice);
        }
        Ok(RunQueue {
            config,
            tree: Tree::new(),
            curr: None,
            zero: 0,
            sum: 0,
            load: 0,
        })
    }

    /// How this queue shares out its CPU.
    #[must_use]
    pub const fn config(&self) -> Config {
        self.config
    }

    /// The total weight of everything runnable here, the running entity
    /// included.
    ///
    /// This is the demand a balancer compares between processors: it counts
    /// entities, weighted, rather than asking whether the processor is busy —
    /// a distinction that does not exist on an idle machine and is the only
    /// one that matters on a loaded one.
    #[must_use]
    pub const fn load_weight(&self) -> u64 {
        self.load
    }

    /// Change the slice every entity asks for from now on.
    ///
    /// Deadlines already handed out are left alone: an entity part-way through
    /// a request keeps the request it was given, and the new slice applies to
    /// the next one. Rewriting live deadlines would be the other choice and it
    /// is the wrong one — it would move entities relative to each other for a
    /// reason that has nothing to do with what they have run.
    ///
    /// # Errors
    ///
    /// [`SchedError::ZeroSlice`], for the same reason [`new`](Self::new) does.
    pub const fn set_slice_ns(&mut self, slice_ns: u64) -> Result<(), SchedError> {
        if slice_ns == 0 {
            return Err(SchedError::ZeroSlice);
        }
        self.config.slice_ns = slice_ns;
        Ok(())
    }

    /// Entities on the queue, the running one included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tree.len() + usize::from(self.curr.is_some())
    }

    /// Whether there is nothing on the queue at all, running or waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entities waiting, not counting the running one.
    #[must_use]
    pub const fn queued(&self) -> usize {
        self.tree.len()
    }

    /// The sum of every entity's weight.
    #[must_use]
    pub const fn load(&self) -> u64 {
        self.load
    }

    /// Whether `id` is on the queue, running or waiting.
    #[must_use]
    ///
    /// A walk of the queue: the tree is ordered by deadline, not by name, and
    /// an index by name would be one more thing to allocate as entities come
    /// and go.
    pub fn contains(&self, id: u64) -> bool {
        self.curr.as_ref().is_some_and(|curr| curr.entity.id == id) || self.tree.find(id).is_some()
    }

    /// The queue's virtual time: the weight-average of every entity's virtual
    /// runtime, rounded down. The base itself when the queue is empty, which
    /// is where the queue's time stood when it last had anything on it.
    #[must_use]
    pub fn avg_vruntime(&self) -> u64 {
        if self.load == 0 {
            return self.zero;
        }
        let offset = floor_div(self.sum, i128::from(self.load));
        self.zero.wrapping_add(offset as i64 as u64)
    }

    /// `vruntime` measured from the base, as a signed offset.
    const fn relative(&self, vruntime: u64) -> i128 {
        vruntime.wrapping_sub(self.zero) as i64 as i128
    }

    /// Whether an entity at `vruntime` is eligible: at or behind the queue's
    /// virtual time.
    ///
    /// Asked without dividing, because the division rounds and this must not:
    /// the weighted sum of everybody's offset from `vruntime` is not negative
    /// exactly when the average is not below it.
    fn is_eligible(&self, vruntime: u64) -> bool {
        self.sum >= self.relative(vruntime) * i128::from(self.load)
    }

    /// The lag of an entity at `vruntime`, rounded down: virtual time less
    /// virtual runtime.
    fn lag_at(&self, vruntime: u64) -> i64 {
        if self.load == 0 {
            return 0;
        }
        let owed = self.sum - self.relative(vruntime) * i128::from(self.load);
        floor_div(owed, i128::from(self.load)) as i64
    }

    /// The virtual length of one slice, for an entity of `weight`.
    fn vslice(&self, weight: u32) -> u64 {
        to_virtual(self.config.slice_ns, weight)
    }

    /// The most lag an entity of `weight` may carry away or bring back: two
    /// slices, as Linux allows. Enough that an entity preempted a moment
    /// before its turn is still owed that turn when it comes back; not so much
    /// that one which slept through a minute collects the minute.
    fn lag_limit(&self, weight: u32) -> i64 {
        i64::try_from(self.vslice(weight).saturating_mul(2)).unwrap_or(i64::MAX)
    }

    /// Count an entity at `vruntime` of `weight` into the virtual time.
    fn add_load(&mut self, vruntime: u64, weight: u32) {
        self.sum += i128::from(weight) * self.relative(vruntime);
        self.load += u64::from(weight);
    }

    /// Take one out again.
    fn sub_load(&mut self, vruntime: u64, weight: u32) {
        self.sum -= i128::from(weight) * self.relative(vruntime);
        self.load -= u64::from(weight);
    }

    /// Move the base to the virtual time, so the sum stays as small as the
    /// entities' spread around it.
    fn normalize(&mut self) {
        if self.load == 0 {
            self.sum = 0;
            return;
        }
        // One entity, running, and nothing waiting: the virtual time is its
        // virtual runtime, which is what the division below comes to, written
        // down (`docs/OPAQUE-KERNEL.md` §9.7, part 7: every charge of a task
        // alone on its queue, and every direct switch, makes this one).
        if self.tree.is_empty()
            && let Some(curr) = self.curr.as_ref().map(|node| &node.entity)
            && self.load == u64::from(curr.weight)
        {
            self.zero = curr.vruntime;
            self.sum = 0;
            return;
        }
        let target = self.avg_vruntime();
        let shift = self.relative(target);
        self.sum -= shift * i128::from(self.load);
        self.zero = target;
    }

    /// Put an entity on the queue, carrying the lag it left its last queue
    /// with.
    ///
    /// The lag is scaled before it is applied, as Linux scales it: adding an
    /// entity behind the virtual time pulls the virtual time back towards it,
    /// by its share of the new total weight, and placing it naively would
    /// leave it owed less than it arrived with. Scaling by the new total over
    /// the old makes the lag it has once it is counted exactly the lag it
    /// brought.
    ///
    /// It arrives in `slot`, which the queue keeps while the entity is on it
    /// and hands back as it leaves: nothing here allocates.
    ///
    /// # Errors
    ///
    /// [`SchedError::ZeroWeight`] or [`SchedError::Duplicate`], with the
    /// payload and the slot handed back.
    pub fn enqueue(
        &mut self,
        id: u64,
        payload: T,
        state: EntityState,
        slot: Slot<T>,
    ) -> Result<(), Refused<T>> {
        if state.weight == 0 {
            return Err(Refused {
                payload,
                slot,
                reason: SchedError::ZeroWeight,
            });
        }
        if self.contains(id) {
            return Err(Refused {
                payload,
                slot,
                reason: SchedError::Duplicate(id),
            });
        }

        let lag = self.placement_lag(state);
        let vruntime = self.avg_vruntime().wrapping_sub(lag as u64);
        let deadline = vruntime.wrapping_add(self.vslice(state.weight));
        self.add_load(vruntime, state.weight);

        let Slot(mut node) = slot;
        node.entity = Entity {
            id,
            weight: state.weight,
            vruntime,
            deadline,
            sum_exec: state.sum_exec,
            payload: Some(payload),
        };
        self.tree.insert(node);
        self.normalize();
        Ok(())
    }

    /// The lag to place an arriving entity with, clamped and scaled.
    fn placement_lag(&self, state: EntityState) -> i64 {
        let limit = self.lag_limit(state.weight);
        let lag = state.vlag.clamp(-limit, limit);
        if self.load == 0 {
            return lag;
        }
        let total = i128::from(self.load) + i128::from(state.weight);
        truncating_div(i128::from(lag) * total, i128::from(self.load)) as i64
    }

    /// Choose what runs next, and make it the running entity.
    ///
    /// The running entity, if there is one, goes back into the tree first and
    /// competes like anything else: this is the whole EEVDF decision, made
    /// afresh. `None` only when the queue is empty.
    pub fn pick_next(&mut self) -> Option<&T> {
        self.put_curr_back();
        let key = self.choose()?;
        let node = self.tree.remove(key)?;
        let chosen = self.curr.insert(node);
        chosen.entity.payload.as_ref()
    }

    /// The direct switch's step (`docs/OPAQUE-KERNEL.md` §9.7, part 1): `id`
    /// arrives and runs, and the running entity leaves, on a queue where
    /// nothing waits. Exactly [`RunQueue::enqueue`] of `id`, then
    /// [`RunQueue::set_slice_ns`] to `slice_after` (the slice the caller's
    /// queue rescales to once `id` is on it), then [`RunQueue::remove_curr`]
    /// and [`RunQueue::pick_next`] -- every quantity of the queue, of the
    /// arriving entity and of the leaving one bit for bit as those calls
    /// leave them, which the host test holds it to -- without the tree they
    /// go through: with nothing waiting the pick can only be `id`. Answers
    /// the leaving entity's parts, as [`RunQueue::remove_curr`] does.
    ///
    /// On a queue with something waiting, or nothing running, it is those
    /// calls themselves.
    ///
    /// # Errors
    ///
    /// As [`RunQueue::enqueue`], with nothing changed.
    pub fn hand_over(
        &mut self,
        id: u64,
        payload: T,
        state: EntityState,
        slot: Slot<T>,
        slice_after: u64,
    ) -> Result<Option<Left<T>>, Refused<T>> {
        if !self.tree.is_empty() || self.curr.is_none() || slice_after == 0 {
            self.enqueue(id, payload, state, slot)?;
            let _ = self.set_slice_ns(slice_after);
            let left = self.remove_curr();
            let _ = self.pick_next();
            return Ok(left);
        }
        if state.weight == 0 {
            return Err(Refused {
                payload,
                slot,
                reason: SchedError::ZeroWeight,
            });
        }
        if self.curr.as_ref().is_some_and(|curr| curr.entity.id == id) {
            return Err(Refused {
                payload,
                slot,
                reason: SchedError::Duplicate(id),
            });
        }
        // `enqueue`: placed by the lag it brings, counted, and the base moved
        // to the virtual time.
        let lag = self.placement_lag(state);
        let vruntime = self.avg_vruntime().wrapping_sub(lag as u64);
        let deadline = vruntime.wrapping_add(self.vslice(state.weight));
        self.add_load(vruntime, state.weight);
        self.normalize();
        // `set_slice_ns`.
        self.config.slice_ns = slice_after;
        // `remove_curr`: the leaving entity's lag, measured while it is still
        // counted, then uncounted. Only the arrival is left, so `normalize`
        // puts the base at its virtual runtime and the sum at nothing, which
        // is written down rather than divided out.
        let Some(mut leaving) = self.curr.take() else {
            return Ok(None);
        };
        let entity = &leaving.entity;
        let limit = self.lag_limit(entity.weight);
        let vlag = self.lag_at(entity.vruntime).clamp(-limit, limit);
        let (left_id, weight, sum_exec) = (entity.id, entity.weight, entity.sum_exec);
        self.load -= u64::from(weight);
        self.zero = vruntime;
        self.sum = 0;
        // `pick_next`: the arrival, the only entity, runs.
        let Slot(mut node) = slot;
        node.entity = Entity {
            id,
            weight: state.weight,
            vruntime,
            deadline,
            sum_exec: state.sum_exec,
            payload: Some(payload),
        };
        self.curr = Some(node);
        let Some(payload) = leaving.entity.payload.take() else {
            return Ok(None);
        };
        Ok(Some((
            left_id,
            payload,
            EntityState {
                weight,
                vlag,
                sum_exec,
            },
            Slot(leaving),
        )))
    }

    /// The eligible entity with the earliest deadline.
    ///
    /// The fallback to the earliest deadline outright cannot be taken while
    /// the sums are right — the entity furthest behind is at or behind any
    /// average that includes it — and is there so that a queue whose
    /// bookkeeping had gone wrong would still run something rather than idle
    /// a CPU with work on it. `check_invariants` is what would notice.
    fn choose(&self) -> Option<Key> {
        self.tree
            .pick(|vruntime| self.is_eligible(vruntime))
            .or_else(|| self.tree.first())
            .map(Entity::key)
    }

    /// Return the running entity to the tree, still counted.
    fn put_curr_back(&mut self) {
        if let Some(node) = self.curr.take() {
            self.tree.insert(node);
        }
    }

    /// Charge the running entity for `delta_ns` real nanoseconds of CPU.
    ///
    /// Returns whether that finished its slice, in which case it has already
    /// been given its next deadline and the caller should pick again. The
    /// next deadline runs from where the entity *is*, not from where the last
    /// one was: time a late timer let it overrun is not charged to its next
    /// request. That is the overrun the fairness bound has to allow for.
    pub fn update_curr(&mut self, delta_ns: u64) -> bool {
        let slice_ns = self.config.slice_ns;
        let Some(curr) = self.curr.as_mut().map(|node| &mut node.entity) else {
            return false;
        };
        let delta = to_virtual(delta_ns, curr.weight);
        curr.sum_exec = curr.sum_exec.saturating_add(delta_ns);
        curr.vruntime = curr.vruntime.wrapping_add(delta);
        let exhausted = !before(curr.vruntime, curr.deadline);
        if exhausted {
            curr.deadline = curr
                .vruntime
                .wrapping_add(to_virtual(slice_ns, curr.weight));
        }
        let weight = curr.weight;

        self.sum += i128::from(weight) * i128::from(delta);
        self.normalize();
        exhausted
    }

    /// The running entity gives up the rest of its request: its deadline moves
    /// a slice further out, as Linux's `sched_yield` moves it, so anything
    /// eligible with a nearer deadline goes first.
    ///
    /// **Nothing at all while nothing else is queued**, as Linux's
    /// `yield_task_fair` returns at once when its queue runs one task. There
    /// is nobody to go first, the entity is picked again, and each yield
    /// would push its deadline another slice out: six hundred yields in a
    /// loop asked for 1.8 seconds, which is what the next entity to arrive
    /// was then made to wait behind (FX-0502).
    pub fn yield_curr(&mut self) {
        if self.tree.is_empty() {
            return;
        }
        let slice_ns = self.config.slice_ns;
        if let Some(curr) = self.curr.as_mut().map(|node| &mut node.entity) {
            curr.deadline = curr
                .deadline
                .wrapping_add(to_virtual(slice_ns, curr.weight));
        }
    }

    /// Real nanoseconds until the queue needs a decision: the running entity's
    /// remaining slice, but never more than one configured slice while
    /// anything waits, and `None` while nothing does.
    ///
    /// **The bound is not an optimisation.** An entity that arrives behind
    /// the running one without being eligible -- woken, or moved from another
    /// queue, ahead of its share -- does not preempt it, and waits for the
    /// next decision. Armed for the whole remaining request, that decision is
    /// as far away as the running entity's deadline, and a deadline can be
    /// many slices out: yielding moves it a slice per yield. The waiting
    /// entity then sat out seconds of an unrelated timer (FX-0502). Linux
    /// keeps its tick while more than one task is runnable for the same
    /// reason. The pick itself is unchanged: an entity that is still not
    /// eligible at the decision waits for the next one.
    #[must_use]
    pub fn decision_in_ns(&self) -> Option<u64> {
        if self.tree.is_empty() {
            return None;
        }
        self.remaining_ns()
            .map(|left| left.min(self.config.slice_ns))
    }

    /// Whether something queued should run instead of the running entity, now.
    ///
    /// What a wake-up asks: an entity that has just arrived with an earlier
    /// deadline than the running one, while eligible, preempts it — which is
    /// how a task that sleeps a lot gets the latency its small lag is owed,
    /// rather than waiting out whatever slice is in progress.
    #[must_use]
    pub fn should_preempt(&self) -> bool {
        let Some(curr) = self.curr.as_ref().map(|node| &node.entity) else {
            return !self.tree.is_empty();
        };
        let Some(best) = self.tree.pick(|vruntime| self.is_eligible(vruntime)) else {
            return false;
        };
        !self.is_eligible(curr.vruntime) || before(best.deadline, curr.deadline)
    }

    /// Real nanoseconds until the running entity's slice is spent: what the
    /// timer should be armed for. Zero if it is already spent.
    #[must_use]
    pub fn remaining_ns(&self) -> Option<u64> {
        let curr = &self.curr.as_ref()?.entity;
        let left = curr.deadline.wrapping_sub(curr.vruntime) as i64;
        Some(if left <= 0 {
            0
        } else {
            to_real(left as u64, curr.weight)
        })
    }

    /// Forgive every lag: put every entity, running or waiting, at the queue's
    /// virtual time with a fresh deadline a slice away, so that from here on
    /// nobody is owed anything and nobody is ahead.
    ///
    /// For measuring the scheduler, not for scheduling with. A window that
    /// measures each entity's service against its share assumes every entity
    /// starts the window even; it does not, because everything that happened
    /// before is still in the lags — and on an emulator the host can stall a
    /// processor for tens of milliseconds while one entity is charged for
    /// them as if it had run. The queue then spends the window repaying a debt
    /// that was never service, and the measurement reads the repayment as
    /// unfairness. Levelling first makes the window measure only what the
    /// queue decides inside it.
    ///
    /// The virtual time itself does not move: every entity goes to the
    /// average, which leaves the average where it was.
    pub fn level(&mut self) {
        let avg = self.avg_vruntime();
        let slice_ns = self.config.slice_ns;
        if let Some(curr) = self.curr.as_mut().map(|node| &mut node.entity) {
            curr.vruntime = avg;
            curr.deadline = avg.wrapping_add(to_virtual(slice_ns, curr.weight));
        }
        // Moved node by node from the old tree to a fresh one, every key
        // changing on the way: no list of them, so nothing is allocated.
        let mut old = core::mem::replace(&mut self.tree, Tree::new());
        while let Some(mut node) = old.pop_first() {
            node.entity.vruntime = avg;
            node.entity.deadline = avg.wrapping_add(to_virtual(slice_ns, node.entity.weight));
            self.tree.insert(node);
        }
        // Everything sits at the average, so the weighted sum about it is
        // nothing, and the base may as well be the average itself.
        self.zero = avg;
        self.sum = 0;
    }

    /// Take the running entity off the queue — it is blocking, exiting or
    /// moving — and hand back what it needs to come back with.
    ///
    /// The slot it came in comes back with it.
    pub fn remove_curr(&mut self) -> Option<(u64, T, EntityState, Slot<T>)> {
        let node = self.curr.take()?;
        self.detach(node)
    }

    /// Take entity `id` off the queue, running or waiting, with its slot.
    ///
    /// A waiting entity is found by a walk: see [`RunQueue::contains`].
    pub fn remove(&mut self, id: u64) -> Option<(T, EntityState, Slot<T>)> {
        if self.curr.as_ref().is_some_and(|curr| curr.entity.id == id) {
            return self
                .remove_curr()
                .map(|(_, payload, state, slot)| (payload, state, slot));
        }
        let key = self.tree.find(id)?;
        let node = self.tree.remove(key)?;
        let (_, payload, state, slot) = self.detach(node)?;
        Some((payload, state, slot))
    }

    /// Give entity `id` the weight `weight`, running or waiting, and say
    /// whether it was there to be given one.
    ///
    /// Its virtual runtime is kept, so what the entity is owed survives the
    /// change; what moves is the rate virtual time accrues at, which is the
    /// whole of what a weight is. The deadline is measured again from where
    /// the entity stands, because a request is one slice at the weight it is
    /// made with — and only when the weight really changes, so that setting
    /// the weight an entity already has cannot extend a running entity's turn
    /// however often it is asked for.
    ///
    /// # Errors
    ///
    /// [`SchedError::ZeroWeight`], for the reason [`RunQueue::enqueue`] gives.
    pub fn set_weight(&mut self, id: u64, weight: u32) -> Result<bool, SchedError> {
        if weight == 0 {
            return Err(SchedError::ZeroWeight);
        }
        if let Some(curr) = self
            .curr
            .as_ref()
            .map(|node| &node.entity)
            .filter(|curr| curr.id == id)
        {
            let (vruntime, was) = (curr.vruntime, curr.weight);
            if was != weight {
                let (at, deadline) = self.reweigh(vruntime, was, weight);
                if let Some(curr) = self.curr.as_mut().map(|node| &mut node.entity) {
                    curr.weight = weight;
                    curr.vruntime = at;
                    curr.deadline = deadline;
                }
                self.normalize();
            }
            return Ok(true);
        }
        // Out of the tree and back into it, because the deadline it is keyed
        // by is one of the things the new weight changes.
        let Some(key) = self.tree.find(id) else {
            return Ok(false);
        };
        let Some(mut node) = self.tree.remove(key) else {
            return Ok(false);
        };
        let entity = &mut node.entity;
        if entity.weight != weight {
            let (at, deadline) = self.reweigh(entity.vruntime, entity.weight, weight);
            let entity = &mut node.entity;
            entity.weight = weight;
            entity.vruntime = at;
            entity.deadline = deadline;
        }
        self.tree.insert(node);
        self.normalize();
        Ok(true)
    }

    /// Count an entity at `vruntime` out of the virtual time at `was` and back
    /// into it at `weight`, and answer where it now stands and when its next
    /// request ends.
    ///
    /// **What is kept is the real time it is owed, not the virtual.** Virtual
    /// time runs at a rate the weight sets, so the same lag means a different
    /// number of nanoseconds of CPU either side of the change; an entity owed
    /// a millisecond before is owed a millisecond after, and the virtual lag
    /// is scaled by the old weight over the new to say so. Linux's
    /// `reweight_entity` moves `vruntime` by exactly this, and takes the
    /// virtual time while the entity is still counted at the weight it had,
    /// as [`RunQueue::detach`] takes a leaving entity's lag.
    ///
    /// The entity's own `weight` field is the caller's to set: the sums are
    /// what this keeps right between the two.
    fn reweigh(&mut self, vruntime: u64, was: u32, weight: u32) -> (u64, u64) {
        let virtual_time = self.avg_vruntime();
        let owed = truncating_div(
            i128::from(self.lag_at(vruntime)) * i128::from(was),
            i128::from(weight),
        );
        let owed = i64::try_from(owed).unwrap_or(if owed < 0 { i64::MIN } else { i64::MAX });
        let at = virtual_time.wrapping_sub(owed as u64);
        self.sub_load(vruntime, was);
        self.add_load(at, weight);
        (at, at.wrapping_add(self.vslice(weight)))
    }

    /// Record an entity's lag, uncount it, and hand its parts back.
    ///
    /// The lag is taken while the entity is still counted, because it is a
    /// statement about the queue it is leaving.
    ///
    /// `None` only for a node with no payload, which a queue never holds.
    fn detach(&mut self, mut node: Box<Node<T>>) -> Option<(u64, T, EntityState, Slot<T>)> {
        let entity = &node.entity;
        let limit = self.lag_limit(entity.weight);
        let vlag = self.lag_at(entity.vruntime).clamp(-limit, limit);
        let (id, weight, vruntime, sum_exec) =
            (entity.id, entity.weight, entity.vruntime, entity.sum_exec);
        self.sub_load(vruntime, weight);
        self.normalize();
        let payload = node.entity.payload.take()?;
        Some((
            id,
            payload,
            EntityState {
                weight,
                vlag,
                sum_exec,
            },
            Slot(node),
        ))
    }

    /// The running entity's data.
    #[must_use]
    pub fn current(&self) -> Option<&T> {
        self.curr.as_ref()?.entity.payload.as_ref()
    }

    /// The running entity's data, mutably.
    pub fn current_mut(&mut self) -> Option<&mut T> {
        self.curr.as_mut()?.entity.payload.as_mut()
    }

    /// The running entity's identifier.
    #[must_use]
    pub fn current_id(&self) -> Option<u64> {
        self.curr.as_ref().map(|curr| curr.entity.id)
    }

    /// Entity `id`, running or waiting.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<EntityView<'_, T>> {
        if let Some(curr) = self
            .curr
            .as_ref()
            .map(|node| &node.entity)
            .filter(|curr| curr.id == id)
        {
            return self.view(curr, true);
        }
        let key = self.tree.find(id)?;
        let entity = self.tree.get(key)?;
        self.view(entity, false)
    }

    /// Entity `id`'s lag, in virtual nanoseconds.
    #[must_use]
    pub fn lag(&self, id: u64) -> Option<i64> {
        self.get(id).map(|view| view.lag)
    }

    /// Visit every entity: the running one first, then the queued ones in
    /// deadline order.
    pub fn for_each(&self, mut visit: impl FnMut(EntityView<'_, T>)) {
        if let Some(view) = self
            .curr
            .as_ref()
            .and_then(|node| self.view(&node.entity, true))
        {
            visit(view);
        }
        self.tree.for_each(|entity| {
            if let Some(view) = self.view(entity, false) {
                visit(view);
            }
        });
    }

    /// The queued entity with the latest deadline whose data satisfies
    /// `movable`: what an idle CPU should take from this one.
    ///
    /// The latest deadline because it is the one this queue would run last,
    /// so moving it costs this queue's other entities nothing — and the
    /// running entity never, because it is running.
    #[must_use]
    pub fn latest_where(&self, movable: impl Fn(&T) -> bool) -> Option<u64> {
        self.tree
            .last_where(|entity| entity.payload.as_ref().is_some_and(&movable))
            .map(|key| key.id)
    }

    /// An entity as the caller sees it: `None` for a node with no payload,
    /// which a queue never holds.
    fn view<'a>(&self, entity: &'a Entity<T>, running: bool) -> Option<EntityView<'a, T>> {
        Some(EntityView {
            id: entity.id,
            weight: entity.weight,
            vruntime: entity.vruntime,
            deadline: entity.deadline,
            sum_exec: entity.sum_exec,
            lag: self.lag_at(entity.vruntime),
            running,
            payload: entity.payload.as_ref()?,
        })
    }

    /// Check every claim the queue's bookkeeping rests on.
    ///
    /// The tree is ordered, balanced and its minima are current; every entity
    /// on the queue carries its payload; the running entity is not also in
    /// the tree; and the weighted sum and the load, recomputed from scratch,
    /// are the ones the queue has been maintaining by increments. The last is
    /// the one that matters most — a sum that has drifted makes every
    /// eligibility decision subtly wrong, and nothing else would say so.
    ///
    /// # Errors
    ///
    /// The first claim that does not hold, as a sentence.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        let counted = self.tree.check()?;
        if counted != self.tree.len() {
            return Err("the tree's count of what is queued is wrong");
        }
        let mut missing = 0;
        self.tree.for_each(|entity| {
            if entity.payload.is_none() {
                missing += 1;
            }
        });
        if missing != 0
            || self
                .curr
                .as_ref()
                .is_some_and(|curr| curr.entity.payload.is_none())
        {
            return Err("an entity on the queue has lost its payload");
        }
        if let Some(curr) = self.curr.as_ref()
            && self.tree.find(curr.entity.id).is_some()
        {
            return Err("the running entity is also queued");
        }
        self.check_sums()
    }

    /// Recompute the load and the weighted sum, and require them to match.
    fn check_sums(&self) -> Result<(), &'static str> {
        let mut load = 0u64;
        let mut sum = 0i128;
        self.for_each(|view| {
            load += u64::from(view.weight);
            sum += i128::from(view.weight) * self.relative(view.vruntime);
        });
        if load != self.load {
            return Err("the queue's load is not the sum of its entities' weights");
        }
        if sum != self.sum {
            return Err("the queue's weighted sum has drifted from its entities");
        }
        if self.load == 0 && self.sum != 0 {
            return Err("an empty queue has a weighted sum");
        }
        if self.load != 0 && !(0..i128::from(self.load)).contains(&self.sum) {
            return Err("the queue's base is not at its virtual time");
        }
        Ok(())
    }
}
