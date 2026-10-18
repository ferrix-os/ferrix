//! Work every CPU runs once.
//!
//! Stage 5 replaces this with runqueues and a scheduler, and this module goes
//! away with it. What it exists for now is stage 4's exit criterion: something
//! that genuinely runs on every core at the same time, contending for one lock,
//! so that "the CPU came up" is demonstrated by the CPU doing arithmetic rather
//! than by it setting a flag and going back to sleep.
//!
//! The shape is deliberately the smallest thing that is not a lie. There is one
//! job at a time and it is described by an enum, for the same reason
//! [`crate::smp::Request`] is: a queue of closures needs an allocator, a lock
//! on the interrupt path, and a decision about what happens when it is full —
//! all of which stage 5 has to make properly and none of which should be made
//! twice.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ferrix_sync::IrqSpinLock;

use crate::arch;
use crate::percpu::{MAX_CPUS, Padded, PerCpu};

/// What every CPU is being asked to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Job {
    /// Times to take the lock and add one.
    increments: u64,
}

impl Job {
    /// Take the shared counter and increment it, `increments` times.
    ///
    /// The lock is taken and dropped per increment rather than once around the
    /// loop, which is the entire point: a lock held for the duration would make
    /// this a test of one CPU doing arithmetic while the others wait. Taking it
    /// `increments` times per CPU is what puts the ticket lock under the
    /// contention its fairness claim is about.
    pub(crate) fn run(self) {
        for _ in 0..self.increments {
            let mut counter = COUNTER.lock();
            *counter += 1;
        }
        let _ = CONTRIBUTED
            .mine()
            .fetch_add(self.increments, Ordering::Relaxed);
        let _ = FINISHED.fetch_add(1, Ordering::AcqRel);
    }
}

/// The contended counter itself.
///
/// An `IrqSpinLock` rather than a plain one: this is taken by a CPU that also
/// takes timer interrupts, and a handler that wanted the same lock would
/// deadlock against its own core. Nothing in an interrupt handler takes this
/// today — the discipline is cheaper to keep than to establish later.
static COUNTER: IrqSpinLock<u64, arch::Irq> = IrqSpinLock::new(0);

/// What each CPU says it added, so the total can be attributed rather than
/// merely checked. A total that is right because one CPU did everyone's share
/// is not the thing being tested.
static CONTRIBUTED: PerCpu<AtomicU64> = PerCpu::new([const { Padded(AtomicU64::new(0)) }; MAX_CPUS]);

/// Bumped once per posted job; a CPU runs the job when this differs from what
/// it last saw.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The posted job's parameter.
static INCREMENTS: AtomicU64 = AtomicU64::new(0);

/// The generation each CPU has already taken.
static SEEN: PerCpu<AtomicU64> = PerCpu::new([const { Padded(AtomicU64::new(0)) }; MAX_CPUS]);

/// CPUs that have finished the current job.
static FINISHED: AtomicUsize = AtomicUsize::new(0);

/// Post a job for every CPU, including this one.
pub(crate) fn post(increments: u64) {
    FINISHED.store(0, Ordering::Release);
    INCREMENTS.store(increments, Ordering::Release);
    let _ = GENERATION.fetch_add(1, Ordering::AcqRel);
    // Without this the other CPUs pick the job up on their next timer tick,
    // which works and takes a millisecond. The poke is what makes "every CPU
    // starts at about the same moment" true, and contention is the thing being
    // measured.
    let _ = crate::smp::broadcast(crate::smp::Request::Poke);
}

/// The job this CPU has not run yet, if there is one.
pub(crate) fn take() -> Option<Job> {
    let generation = GENERATION.load(Ordering::Acquire);
    let seen = SEEN.mine();
    if seen.load(Ordering::Acquire) == generation {
        return None;
    }
    seen.store(generation, Ordering::Release);
    Some(Job {
        increments: INCREMENTS.load(Ordering::Acquire),
    })
}

/// How many CPUs have finished the posted job.
pub(crate) fn finished() -> usize {
    FINISHED.load(Ordering::Acquire)
}

/// The counter's value.
pub(crate) fn total() -> u64 {
    *COUNTER.lock()
}

/// What one CPU added to it.
pub(crate) fn contributed(cpu: usize) -> Option<u64> {
    CONTRIBUTED.get(cpu).map(|slot| slot.load(Ordering::Relaxed))
}
