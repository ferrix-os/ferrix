//! ARMv7-A's address space identifiers: the machine's allocator, and the
//! install's two paths to a number (`docs/OPAQUE-KERNEL.md` §9.13).
//!
//! The arithmetic is `ferrix_paging::asid`'s [`Numbers`], kept here under one
//! spin lock. What it cannot hold is each processor's *active* tag, the tag
//! it last installed, which the fast path swaps without the lock:
//!
//! * **Fast path.** The space's tag is of the current generation, and
//!   swapping it into this processor's active tag answers non-zero. The
//!   number is the tag's.
//! * **Slow path**, under the lock: the space gets a number of the current
//!   generation (rolling over if none is free); this processor's flush is
//!   made if a rollover left it pending; the tag becomes its active tag.
//!
//! A rollover swaps every processor's active tag with 0 and reserves what
//! it held. A fast path that read the old generation therefore either swapped
//! before the rollover's swap of the same word -- and its number is reserved,
//! so it still means the same space -- or after, and read 0, and took the
//! slow path, which the lock orders after the rollover. Read-modify-writes of
//! one location are totally ordered, so nothing else is needed: every access
//! here is `Relaxed`, and the lock orders the rest (L.armv7a.19, held by the
//! `asid` model in `src/tests/loom`). Every word is written either by its own
//! processor with interrupts masked (`AddressSpace::install`'s contract), or
//! under the lock.
//!
//! On ARMv7-A an `AtomicU64` load or store is an `ldrexd`/`strexd` pair and a
//! swap an `ldrexd`/`strexd` loop, none with a barrier at `Relaxed`: the fast
//! path is two loads and one swap of this processor's own word.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use ferrix_paging::asid::{self, AsidError, FlushPlan, Numbers};
use ferrix_sched::MAX_CPUS;

use super::cpu;
use crate::sync::SpinLock;

/// An address space's ASID tag: `generation << 8 | number`, 0 until it is
/// first installed. One per space, the same on every processor (DDI 0406C.d
/// B3.9.1), and dropped with the space: a new space on a reused root frame
/// starts at 0, so it never inherits a number.
#[derive(Debug)]
pub(crate) struct SpaceTag(AtomicU64);

impl SpaceTag {
    /// No number yet.
    pub(crate) const fn new() -> SpaceTag {
        SpaceTag(AtomicU64::new(0))
    }

    /// The tag as it stands: for the boot check.
    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Clear the tag, so that the space's next install spends a number.
    ///
    /// Check code only (`tools/common/check/check-armv7a-asid.py` refuses any
    /// other caller): it lets one space stand for the many a rollover needs.
    /// Safe on a live space, installed or not, because it only spends a
    /// number: the old one stays given until the next rollover, and a
    /// processor running it keeps it reserved through its active tag.
    pub(crate) fn forget(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

/// The machine's numbers.
static NUMBERS: SpinLock<Numbers<MAX_CPUS>> = SpinLock::new(Numbers::new());

/// The current generation, as the slow path last stored it: what the fast
/// path compares a space's tag with.
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Each processor's active tag.
static ACTIVE: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Each processor's flushes after a rollover, counted where the flush is made.
static FLUSHES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Rollovers so far.
static ROLLOVERS: AtomicU64 = AtomicU64::new(0);

/// Each processor's [`FlushPlan`], from its own identification registers:
/// bit 0 `ICIALLU` in the flush, bit 1 `BPIALL` at every install, bit 7
/// "decided".
static PLANS: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];

/// [`PLANS`]'s bits.
const PLAN_ICACHE: u8 = 1 << 0;
const PLAN_PREDICTOR: u8 = 1 << 1;
const PLAN_DECIDED: u8 = 1 << 7;

/// Decide processor `cpu`'s flush plan from its own `CTR` and `ID_MMFR1`
/// (L.armv7a.20). Every processor runs this before it can install a space:
/// the boot processor as it decides its speculation defences, each secondary
/// as it starts.
pub(crate) fn init_this_cpu(cpu: usize) {
    let plan = FlushPlan::for_core(cpu::read_ctr(), cpu::read_id_mmfr1());
    let bits = PLAN_DECIDED
        | if plan.instruction_cache { PLAN_ICACHE } else { 0 }
        | if plan.predictor_every_install { PLAN_PREDICTOR } else { 0 };
    if let Some(slot) = PLANS.get(cpu) {
        slot.store(bits, Ordering::Relaxed);
    }
}

/// Processor `cpu`'s plan.
fn plan(cpu: usize) -> u8 {
    PLANS.get(cpu).map_or(0, |plan| plan.load(Ordering::Relaxed))
}

/// Whether processor `cpu` runs `BPIALL` at every install.
pub(crate) fn predictor_every_install(cpu: usize) -> bool {
    plan(cpu) & PLAN_PREDICTOR != 0
}

/// The number processor `cpu` installs `tag`'s space with, after any flush
/// it owes: the fast path, or the slow one.
///
/// Called with interrupts masked, on processor `cpu`, by `install_user_root`
/// only.
pub(crate) fn number_for(tag: &SpaceTag, cpu: usize) -> u8 {
    let held = tag.0.load(Ordering::Relaxed);
    if asid::generation_of(held) == GENERATION.load(Ordering::Relaxed)
        && asid::number_of(held) != asid::NO_SPACE
        && ACTIVE
            .get(cpu)
            .is_some_and(|active| active.swap(held, Ordering::Relaxed) != 0)
    {
        return asid::number_of(held);
    }
    slow_path(tag, cpu)
}

/// The slow path: see the module.
fn slow_path(tag: &SpaceTag, cpu: usize) -> u8 {
    let mut numbers = NUMBERS.lock();
    let held = tag.0.load(Ordering::Relaxed);
    let assigned = match numbers.assign(held, |other| {
        ACTIVE
            .get(other)
            .map_or(0, |active| active.swap(0, Ordering::Relaxed))
    }) {
        Ok(assigned) => assigned,
        Err(why) => crate::panic::fatal!(
            crate::panic::catalog::ASID_EXHAUSTED,
            "no ASID could be given: {}",
            match why {
                AsidError::GenerationExhausted => "the generation is at its limit",
                AsidError::NoneFree => "every number is reserved after a rollover",
            }
        ),
    };
    if assigned.rolled_over {
        GENERATION.store(numbers.generation(), Ordering::Relaxed);
        count(&ROLLOVERS);
    }
    tag.0.store(assigned.tag, Ordering::Relaxed);
    if numbers.take_pending(cpu) {
        // SAFETY: (TRANSLATE) called from `install_user_root`, which installs a root right
        // after; nothing on this processor needs a user address in between.
        unsafe { cpu::flush_for_new_generation(plan(cpu) & PLAN_ICACHE != 0) };
        if let Some(flushes) = FLUSHES.get(cpu) {
            count(flushes);
        }
    }
    if let Some(active) = ACTIVE.get(cpu) {
        active.store(assigned.tag, Ordering::Relaxed);
    }
    asid::number_of(assigned.tag)
}

/// Add one to a counter: a load and a store, since each is written by one
/// processor at a time (its own, or the lock's holder).
fn count(counter: &AtomicU64) {
    counter.store(
        counter.load(Ordering::Relaxed).wrapping_add(1),
        Ordering::Relaxed,
    );
}

/// What the boot check reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// The current generation.
    pub(crate) generation: u64,
    /// Rollovers so far.
    pub(crate) rollovers: u64,
    /// Processor `cpu`'s flushes so far.
    pub(crate) flushes: u64,
    /// Whether processor `cpu`'s flush is pending.
    pub(crate) pending: bool,
    /// The number the next space with none to keep would be given.
    pub(crate) next_free: Option<u8>,
}

/// The counts as seen for processor `cpu`.
pub(crate) fn counts(cpu: usize) -> Counts {
    let numbers = NUMBERS.lock();
    Counts {
        generation: numbers.generation(),
        rollovers: ROLLOVERS.load(Ordering::Relaxed),
        flushes: FLUSHES
            .get(cpu)
            .map_or(0, |flushes| flushes.load(Ordering::Relaxed)),
        pending: numbers.is_pending(cpu),
        next_free: numbers.next_free(),
    }
}

pub(crate) mod check;
