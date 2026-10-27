//! The scheduling-class interface (`docs/SCHED-CLASSES.md` §4).
//!
//! Every scheduler Ferrix has -- the fair class (EEVDF), and from stage 14
//! the fixed-priority, deadline and time-window classes -- is a crate of its
//! own implementing [`SchedClass`]. Which of them a kernel contains is a
//! build flag of its profile; the kernel names the chosen stack as one type
//! and calls it directly, so the choice costs nothing at run time and a
//! class a profile leaves out is never linked.
//!
//! A class keeps one CPU's worth of state in its [`SchedClass::Queue`], and
//! one task's in its [`SchedClass::Entity`], which the task carries while it
//! is off every queue. Tasks are named by a `u64` and carried as a payload
//! `T` the class never looks inside (the kernel's `Arc<Task>`). Nothing here
//! allocates: what a queue needs it is handed, as the fair class is handed a
//! tree node in its entity.
//!
//! [`Over`] stacks two classes, the upper one first, and is itself a class,
//! so a stack of any depth is one type, monomorphised and inlined where the
//! kernel uses it. [`Empty`] is the class with nothing in it: a tier a
//! profile leaves out.

#![no_std]
#![forbid(unsafe_code)]

use core::convert::Infallible;
use core::fmt;
use core::marker::PhantomData;

mod balance;
mod domain;

pub use balance::{
    BALANCE_THRESHOLD, Balance, CpuLoad, FLIP_DECAY_NS, LOAD_PERIOD_NS, LOAD_SCALE, Load,
    Placement, busiest, flip, imbalance, place, pull_fits, quietest, utilisation, wake_wide,
};
pub use domain::{Class, CpuSet, Domain, MAX_CPUS, Mode, check_partition};

/// Why a domain or a CPU set was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DomainError {
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

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainError::ModeUnavailable(mode) => {
                write!(f, "the {} mode is not written yet", mode.name())
            }
            DomainError::EmptyDomain => f.write_str("a scheduling domain has no CPUs"),
            DomainError::Overlap(cpu) => write!(f, "CPU {cpu} is in two scheduling domains"),
            DomainError::Uncovered(cpu) => write!(f, "CPU {cpu} is in no scheduling domain"),
            DomainError::NoSuchCpu(cpu) => write!(f, "there is no CPU {cpu}"),
        }
    }
}

/// An entity a queue would not take, handed back whole with the reason: in
/// the kernel the payload is the last reference to a task, and losing it
/// would lose the task.
#[derive(Debug)]
pub struct Back<T, E> {
    /// What the caller tried to queue.
    pub payload: T,
    /// What it carried.
    pub entity: E,
    /// Why it was refused.
    pub reason: Refusal,
}

/// Why a class refused something.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The parameters are not ones this class takes (a zero weight, a
    /// priority out of range, a budget over its period).
    Invalid,
    /// Something with this identifier is already on the queue.
    Duplicate(u64),
    /// Admitting it would break what the queue has already promised (a
    /// deadline class's utilisation test).
    Unschedulable,
    /// The entity is of a tier the stack does not have.
    NoSuchTier,
}

/// A running entity taken off its queue: its identifier, its payload and
/// what it carries away.
pub type Left<T, E> = (u64, T, E);

/// What [`SchedClass::hand_over`] answers: the entity that left, if one
/// was running, or the arriving one handed back.
pub type Handed<T, E> = Result<Option<Left<T, E>>, Back<T, E>>;

/// One scheduling class: what it keeps, and the operations the kernel's
/// scheduler calls on it, every one under the CPU's run-queue lock.
///
/// Associated functions, not methods: the queue is passed in, so a stack of
/// classes is a tuple of queues and a type, with nothing to construct.
pub trait SchedClass<T> {
    /// What the boot line and the audit record call it.
    const NAME: &'static str;

    /// What one CPU keeps.
    type Queue;
    /// What one task carries while it is off every queue of this class.
    type Entity;
    /// What a queue is made with (the fair class's slice).
    type Config: Copy;

    /// A CPU's empty queue.
    ///
    /// # Errors
    ///
    /// [`Refusal::Invalid`] for a configuration the class cannot use.
    fn new_queue(config: Self::Config) -> Result<Self::Queue, Refusal>;

    /// How many entities the queue holds, the running one included.
    fn len(queue: &Self::Queue) -> usize;

    /// Put an entity on the queue.
    ///
    /// # Errors
    ///
    /// The entity handed back, with the reason, and nothing changed.
    fn enqueue(
        queue: &mut Self::Queue,
        id: u64,
        payload: T,
        entity: Self::Entity,
    ) -> Result<(), Back<T, Self::Entity>>;

    /// Take a queued or running entity off the queue.
    fn remove(queue: &mut Self::Queue, id: u64) -> Option<(T, Self::Entity)>;

    /// Take the running entity off the queue.
    fn remove_current(queue: &mut Self::Queue) -> Option<Left<T, Self::Entity>>;

    /// Choose what runs next and make it the running entity: the running
    /// one competes again. `None` only when the queue is empty.
    fn pick(queue: &mut Self::Queue) -> Option<&T>;

    /// Put the running entity back among the queued ones without choosing:
    /// what a stack does to a lower class when an upper one takes the CPU.
    fn put_back(queue: &mut Self::Queue);

    /// The running entity, if there is one.
    fn current(queue: &Self::Queue) -> Option<&T>;

    /// The direct switch's step: `id` arrives and runs, and the running
    /// entity leaves, exactly as an enqueue, a removal of the running entity
    /// and a pick would leave the queue (the IPC round trip rests on it,
    /// `docs/SCHED-CLASSES.md` S5). `slice_after` is the slice the caller's
    /// queue rescales to, for a class that has one; a class without one
    /// ignores it.
    ///
    /// # Errors
    ///
    /// As [`SchedClass::enqueue`], with nothing changed.
    fn hand_over(
        queue: &mut Self::Queue,
        id: u64,
        payload: T,
        entity: Self::Entity,
        slice_after: u64,
    ) -> Handed<T, Self::Entity>;

    /// Charge the running entity `ran_ns` of real time. Whether a decision
    /// is due: its slice or its budget is spent.
    fn charge(queue: &mut Self::Queue, ran_ns: u64) -> bool;

    /// Nanoseconds until the running entity's slice or budget ends: what the
    /// CPU's one-shot timer is armed for. `None` with nothing running.
    fn remaining_ns(queue: &Self::Queue) -> Option<u64>;

    /// Whether an idle CPU may take this queue's tasks, now. False for every
    /// real-time class: partitioning is what makes admission mean anything.
    /// A function of the queue, not a constant, so that a stack answers by
    /// what it holds (S11).
    fn steals_work(queue: &Self::Queue) -> bool;

    /// The class's own invariants, for the boot checks and host tests.
    ///
    /// # Errors
    ///
    /// The first one that does not hold.
    fn check(queue: &Self::Queue) -> Result<(), &'static str>;
}

/// The class with nothing in it: a tier a profile leaves out. It takes no
/// entity (its entity type has no values), so nothing can be queued on it.
#[derive(Debug)]
pub struct Empty;

impl<T> SchedClass<T> for Empty {
    const NAME: &'static str = "empty";
    type Queue = ();
    type Entity = Infallible;
    type Config = ();

    fn new_queue((): ()) -> Result<(), Refusal> {
        Ok(())
    }
    fn len((): &()) -> usize {
        0
    }
    fn enqueue((): &mut (), _: u64, _: T, entity: Infallible) -> Result<(), Back<T, Infallible>> {
        match entity {}
    }
    fn remove((): &mut (), _: u64) -> Option<(T, Infallible)> {
        None
    }
    fn remove_current((): &mut ()) -> Option<Left<T, Infallible>> {
        None
    }
    fn pick((): &mut ()) -> Option<&T> {
        None
    }
    fn put_back((): &mut ()) {}
    fn current((): &()) -> Option<&T> {
        None
    }
    fn hand_over((): &mut (), _: u64, _: T, entity: Infallible, _: u64) -> Handed<T, Infallible> {
        match entity {}
    }
    fn charge((): &mut (), _: u64) -> bool {
        false
    }
    fn remaining_ns((): &()) -> Option<u64> {
        None
    }
    fn steals_work((): &()) -> bool {
        false
    }
    fn check((): &()) -> Result<(), &'static str> {
        Ok(())
    }
}

/// Which tier of an [`Over`] stack an entity belongs to, and what it
/// carries there.
#[derive(Debug)]
pub enum Tier<U, L> {
    /// The upper class's.
    Upper(U),
    /// The lower class's.
    Lower(L),
}

/// Two classes stacked: whatever the upper one has runs before anything of
/// the lower one. Itself a class, so `Over<A, Over<B, C>>` is a stack of
/// three.
#[derive(Debug)]
pub struct Over<U, L>(PhantomData<(U, L)>);

/// An [`Over`] stack's queue: its two classes' queues, and which of them
/// holds the running entity.
#[derive(Debug)]
pub struct Stacked<UQ, LQ> {
    /// The upper class's queue.
    pub upper: UQ,
    /// The lower class's queue.
    pub lower: LQ,
    /// Whether the running entity, if any, is the upper class's.
    running_upper: bool,
}

impl<T, U: SchedClass<T>, L: SchedClass<T>> SchedClass<T> for Over<U, L> {
    const NAME: &'static str = "stack";
    type Queue = Stacked<U::Queue, L::Queue>;
    type Entity = Tier<U::Entity, L::Entity>;
    type Config = (U::Config, L::Config);

    fn new_queue((upper, lower): Self::Config) -> Result<Self::Queue, Refusal> {
        Ok(Stacked {
            upper: U::new_queue(upper)?,
            lower: L::new_queue(lower)?,
            running_upper: false,
        })
    }

    fn len(queue: &Self::Queue) -> usize {
        U::len(&queue.upper) + L::len(&queue.lower)
    }

    // An entity is in one tier's queue at most: the kernel queues a task
    // through its one run slot, which a queue keeps while the task is on it
    // (the split's review, C2), so the stack does not look for the same
    // identifier in the other tier.
    fn enqueue(
        queue: &mut Self::Queue,
        id: u64,
        payload: T,
        entity: Self::Entity,
    ) -> Result<(), Back<T, Self::Entity>> {
        match entity {
            Tier::Upper(entity) => {
                U::enqueue(&mut queue.upper, id, payload, entity).map_err(|back| Back {
                    payload: back.payload,
                    entity: Tier::Upper(back.entity),
                    reason: back.reason,
                })
            }
            Tier::Lower(entity) => {
                L::enqueue(&mut queue.lower, id, payload, entity).map_err(|back| Back {
                    payload: back.payload,
                    entity: Tier::Lower(back.entity),
                    reason: back.reason,
                })
            }
        }
    }

    fn remove(queue: &mut Self::Queue, id: u64) -> Option<(T, Self::Entity)> {
        if let Some((payload, entity)) = U::remove(&mut queue.upper, id) {
            return Some((payload, Tier::Upper(entity)));
        }
        L::remove(&mut queue.lower, id).map(|(payload, entity)| (payload, Tier::Lower(entity)))
    }

    fn remove_current(queue: &mut Self::Queue) -> Option<Left<T, Self::Entity>> {
        if queue.running_upper {
            U::remove_current(&mut queue.upper)
                .map(|(id, payload, entity)| (id, payload, Tier::Upper(entity)))
        } else {
            L::remove_current(&mut queue.lower)
                .map(|(id, payload, entity)| (id, payload, Tier::Lower(entity)))
        }
    }

    fn pick(queue: &mut Self::Queue) -> Option<&T> {
        if U::len(&queue.upper) > 0 {
            // The lower class's running entity, if any, waits again.
            if !queue.running_upper {
                L::put_back(&mut queue.lower);
            }
            queue.running_upper = true;
            return U::pick(&mut queue.upper);
        }
        queue.running_upper = false;
        L::pick(&mut queue.lower)
    }

    fn put_back(queue: &mut Self::Queue) {
        if queue.running_upper {
            U::put_back(&mut queue.upper);
        } else {
            L::put_back(&mut queue.lower);
        }
    }

    fn current(queue: &Self::Queue) -> Option<&T> {
        if queue.running_upper {
            U::current(&queue.upper)
        } else {
            L::current(&queue.lower)
        }
    }

    fn hand_over(
        queue: &mut Self::Queue,
        id: u64,
        payload: T,
        entity: Self::Entity,
        slice_after: u64,
    ) -> Handed<T, Self::Entity> {
        // The fused step only within one tier, and only when nothing of the
        // other tier could be chosen instead; otherwise the general sequence.
        match entity {
            Tier::Upper(entity) if queue.running_upper => {
                U::hand_over(&mut queue.upper, id, payload, entity, slice_after)
                    .map(|left| {
                        left.map(|(id, payload, entity)| (id, payload, Tier::Upper(entity)))
                    })
                    .map_err(|back| Back {
                        payload: back.payload,
                        entity: Tier::Upper(back.entity),
                        reason: back.reason,
                    })
            }
            Tier::Lower(entity) if !queue.running_upper && U::len(&queue.upper) == 0 => {
                L::hand_over(&mut queue.lower, id, payload, entity, slice_after)
                    .map(|left| {
                        left.map(|(id, payload, entity)| (id, payload, Tier::Lower(entity)))
                    })
                    .map_err(|back| Back {
                        payload: back.payload,
                        entity: Tier::Lower(back.entity),
                        reason: back.reason,
                    })
            }
            entity => {
                Self::enqueue(queue, id, payload, entity)?;
                let left = Self::remove_current(queue);
                let _ = Self::pick(queue);
                Ok(left)
            }
        }
    }

    fn charge(queue: &mut Self::Queue, ran_ns: u64) -> bool {
        if queue.running_upper {
            U::charge(&mut queue.upper, ran_ns)
        } else {
            // Something of the upper class waiting is a decision due too.
            L::charge(&mut queue.lower, ran_ns) || U::len(&queue.upper) > 0
        }
    }

    fn remaining_ns(queue: &Self::Queue) -> Option<u64> {
        if queue.running_upper {
            U::remaining_ns(&queue.upper)
        } else {
            L::remaining_ns(&queue.lower)
        }
    }

    fn steals_work(queue: &Self::Queue) -> bool {
        if U::len(&queue.upper) > 0 {
            U::steals_work(&queue.upper)
        } else {
            L::steals_work(&queue.lower)
        }
    }

    fn check(queue: &Self::Queue) -> Result<(), &'static str> {
        U::check(&queue.upper)?;
        L::check(&queue.lower)?;
        if queue.running_upper && L::current(&queue.lower).is_some() {
            return Err("a stack's lower class has a running entity while its upper class runs");
        }
        if !queue.running_upper && U::current(&queue.upper).is_some() {
            return Err("a stack's upper class has a running entity while its lower class runs");
        }
        Ok(())
    }
}

#[cfg(test)]
mod domain_tests;
#[cfg(test)]
mod tests;
