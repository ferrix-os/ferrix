//! Job quotas: what a job may hold at once, and what it holds now.
//!
//! The Security Target's `FRU_RSA.1`: the TSF enforces maximum quotas of
//! physical memory, kernel objects and processor time that a job can use
//! simultaneously (`docs/certification/IMPLEMENTATION.md`, W-13, is the
//! design and its argument). A job's quota is a **slot** here, not a field
//! of the job, and everything charged names its slot by a `u32`.
//!
//! # Why a table of slots
//!
//! A frame is freed under whatever lock its last holder had -- a VMO's
//! pages, an address space's, a page-table walk -- and has to find its
//! charge there. An `Arc<Job>` cannot be dropped there, since a drop frees
//! memory and may be a job's last, and a raw pointer would need `unsafe`. An
//! index into a table that never shrinks needs neither, and fits in the
//! frame record's link field, which an allocated frame leaves unused
//! (`ferrix_frame::Frames::set_owner`). The table grows by [`CHUNK`] slots at
//! a time, from process context, and a chunk once made is never taken away,
//! so an index read anywhere stays a slot.
//!
//! # Charging is hierarchical and exact
//!
//! A charge of `n` walks from the job's slot to the top of its tree. At each
//! level it adds `n` only if the level's use stays within its limit -- a
//! compare-and-swap, so two charges racing for the last unit cannot both
//! win -- and a refusal at any level takes back what the levels below took
//! and counts a refusal there. So a job's use is in every ancestor's, a limit
//! anywhere above refuses, and no use is ever over its limit, even briefly.
//!
//! The tree's root has no slot, and nothing is charged there: a program in
//! the root job pays one load of a word, and no line is written by every
//! processor's faults. A limit only ever sits below the root, as on Linux.
//!
//! # A slot outlives its job while anything is charged to it
//!
//! A slot counts **holds**: its job, each frame tagged with it, each object
//! token, each child slot, each task that names it for the scheduler. When
//! the last goes it is free, and lets go of its parent. A frame charged to a
//! job that has since gone still uncharges exactly the levels it charged,
//! because the chain of parents stays with the slots; nothing is moved to a
//! parent, and a gone job leaves nothing behind but counters.
//!
//! # Processor weight
//!
//! A job's `load` is the weight of its runnable tasks and of its busy
//! children, and a task's effective weight is its own times, at each level
//! from its job up, that job's weight over that job's load ([`effective`]).
//! A task in the root job keeps its weight exactly. Every function here is
//! atomics only: the scheduler calls them under its run-queue locks and from
//! interrupt handlers.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use ferrix_sched_fair::NICE_0_WEIGHT;
use ferrix_sync::Once;

use crate::fallible::{self, AllocError};
use crate::sync::SpinLock;

/// No slot: the tree's root, a kernel thread, a frame nobody is charged for.
pub const NONE: u32 = u32::MAX;

/// A limit that is no limit.
pub const UNLIMITED: u64 = u64::MAX;

/// What a job is charged for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resource {
    /// Memory, in bytes: the frames of its programs' memory and page tables,
    /// a page each, and the kernel heap the Linux personality holds for them
    /// ([`Resource::Kernel`], F-37). `memory.max` limits the sum, as Linux's
    /// does with kernel memory folded in.
    Memory,
    /// Kernel objects a program made: VMOs, channel ends, ports, jobs, pins.
    Objects,
    /// Tasks: a process, and each thread beside its first.
    Tasks,
    /// The part of [`Resource::Memory`] that is kernel heap, in bytes:
    /// `memory.stat`'s `kernel`. Charged only with memory, by
    /// [`charge_kernel`], and never limited on its own.
    Kernel,
    /// The part of [`Resource::Memory`] that is its programs' page tables,
    /// in bytes: `memory.stat`'s `pagetables`. Charged only with memory, by
    /// [`charge_table`], and never limited on its own.
    Tables,
}

impl Resource {
    /// Every resource, in the order the slot keeps them.
    pub const ALL: [Resource; RESOURCES] = [
        Resource::Memory,
        Resource::Objects,
        Resource::Tasks,
        Resource::Kernel,
        Resource::Tables,
    ];

    /// Where the slot keeps it.
    const fn at(self) -> usize {
        match self {
            Resource::Memory => 0,
            Resource::Objects => 1,
            Resource::Tasks => 2,
            Resource::Kernel => 3,
            Resource::Tables => 4,
        }
    }
}

/// How many resources a slot counts.
const RESOURCES: usize = 5;

/// What a slot counts that no limit governs: events, for `memory.events` and
/// `memory.stat`. Each is counted in the job it happened in and in every job
/// above it, as cgroup v2's hierarchical files report them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Counter {
    /// Times its use went over `memory.high` and was reclaimed.
    High,
    /// Pages reclaim looked at (`pgscan`).
    Scanned,
    /// Pages reclaim gave back (`pgsteal`).
    Stolen,
    /// Page faults its programs took (`pgfault`).
    Faults,
    /// Faults that read a file from its disk (`pgmajfault`).
    MajorFaults,
    /// Periods that ran under its `cpu.max` (`nr_periods`). Its own, not
    /// its descendants'.
    Periods,
    /// Periods in which its `cpu.max` ran out (`nr_throttled`). Its own.
    Throttled,
    /// Nanoseconds it spent throttled (`throttled_usec`). Its own.
    ThrottledNs,
    /// Periods in which it used more than its `cpu.max` quota, out of what
    /// `cpu.max.burst` carried (`nr_bursts`). Its own.
    Bursts,
    /// Nanoseconds it used beyond its quota in those periods, up to what was
    /// carried (`burst_usec`). Its own.
    BurstNs,
}

impl Counter {
    /// Where the slot keeps it.
    const fn at(self) -> usize {
        match self {
            Counter::High => 0,
            Counter::Scanned => 1,
            Counter::Stolen => 2,
            Counter::Faults => 3,
            Counter::MajorFaults => 4,
            Counter::Periods => 5,
            Counter::Throttled => 6,
            Counter::ThrottledNs => 7,
            Counter::Bursts => 8,
            Counter::BurstNs => 9,
        }
    }
}

/// How many counters a slot has.
const COUNTERS: usize = 10;

/// `cpu.max`'s default period, 100 ms, in nanoseconds.
pub(crate) const DEFAULT_PERIOD_NS: u64 = 100_000_000;

/// The marks of `memory.high`, `memory.low` and `memory.min`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    /// `memory.high`: above it, the job's pages are reclaimed. [`UNLIMITED`]
    /// for `max`.
    High,
    /// `memory.low`: a job using no more than this is spared by the reclaim
    /// of a job above it while anything else can give ([`protected`]).
    Low,
    /// `memory.min`: a job using no more than this is spared by the reclaim
    /// of a job above it, whatever else is asked of that reclaim.
    Min,
}

impl Mark {
    /// Where the slot keeps it.
    const fn at(self) -> usize {
        match self {
            Mark::High => 0,
            Mark::Low => 1,
            Mark::Min => 2,
        }
    }
}

/// What a frame is charged as: a page of memory.
const FRAME_BYTES: u64 = ferrix_bootinfo::PAGE_SIZE;

/// A charge a limit refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exceeded;

/// `cpu.weight`'s default, and Linux's.
pub const DEFAULT_WEIGHT: u32 = 100;
/// The least `cpu.weight` takes.
pub const MIN_WEIGHT: u32 = 1;
/// The most `cpu.weight` takes.
pub const MAX_WEIGHT: u32 = 10_000;

/// The entity weight of a job with `cpu.idle` set, in task units: Linux's
/// `WEIGHT_IDLEPRIO`, against 1024 for a task at nice 0.
pub(crate) const IDLE_ENTITY_WEIGHT: i64 = 3;

/// The least effective weight a task is given, however many share its job:
/// Linux's `MIN_SHARES`. Zero would be refused by the run queue.
const MIN_EFFECTIVE: u64 = 2;
/// The most, well inside a `u32` and the run queue's arithmetic.
const MAX_EFFECTIVE: u64 = 1 << 24;

/// One job's counters.
#[derive(Debug)]
struct Slot {
    /// How many things keep it: zero when it is free.
    holds: AtomicU64,
    /// Whether the job it was the quota of has gone, though something (a task
    /// asleep in the kernel in it) still holds it: whoever keeps state of its
    /// own per slot must not start any for it (`blkio`'s entries).
    ended: AtomicBool,
    /// Its parent's slot, or [`NONE`].
    parent: AtomicU32,
    /// Use of each resource, its own and every descendant's.
    used: [AtomicU64; RESOURCES],
    /// Each resource's limit.
    limit: [AtomicU64; RESOURCES],
    /// How many charges of each resource a limit here refused.
    refused: [AtomicU64; RESOURCES],
    /// `memory.high`, `memory.low` and `memory.min`, in bytes.
    marks: [AtomicU64; 3],
    /// Its [`Counter`]s.
    counters: [AtomicU64; COUNTERS],
    /// Nanoseconds its tasks used the processor in user mode and in kernel
    /// mode, its descendants' included (`cpu.stat`), as the ticks saw them.
    cpu_time: [AtomicU64; 2],
    /// `cpu.max`: the runtime its subtree may use each period, or
    /// [`UNLIMITED`], in nanoseconds.
    bw_quota: AtomicU64,
    /// `cpu.max`'s period, in nanoseconds.
    bw_period: AtomicU64,
    /// What its subtree has used of this period's quota.
    bw_used: AtomicU64,
    /// When this period ends, by the counter's reckoning; zero before the
    /// first charge under a quota.
    bw_end: AtomicU64,
    /// When it ran out of this period's quota, or zero if it has not.
    bw_throttled_at: AtomicU64,
    /// `cpu.max.burst`: how much unused quota a period may carry to the
    /// next, in nanoseconds; zero for none.
    bw_burst: AtomicU64,
    /// What the last period carried into this one, beyond the quota: this
    /// period's runtime is the quota and this.
    bw_extra: AtomicU64,
    /// `cpu.weight`.
    weight: AtomicU32,
    /// `cpu.idle`: whether it runs at `IDLE_ENTITY_WEIGHT` whatever its
    /// `cpu.weight`.
    idle: AtomicBool,
    /// The weight of its runnable tasks and busy children, in task units.
    load: AtomicI64,
    /// What it adds to its parent's `load` while it is busy.
    contributed: AtomicI64,
    /// The job it is the quota of, and that job's audit budget: what a
    /// refused charge is recorded against (`audit`), read with no lock, as
    /// a refusal can come from inside the heap. 0 until the job is made.
    job: AtomicU64,
    budget: AtomicU64,
}

impl Slot {
    /// A free slot.
    const fn new() -> Slot {
        Slot {
            holds: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            parent: AtomicU32::new(NONE),
            used: [const { AtomicU64::new(0) }; RESOURCES],
            limit: [const { AtomicU64::new(UNLIMITED) }; RESOURCES],
            refused: [const { AtomicU64::new(0) }; RESOURCES],
            marks: [
                AtomicU64::new(UNLIMITED),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            counters: [const { AtomicU64::new(0) }; COUNTERS],
            cpu_time: [const { AtomicU64::new(0) }; 2],
            bw_quota: AtomicU64::new(UNLIMITED),
            bw_period: AtomicU64::new(DEFAULT_PERIOD_NS),
            bw_used: AtomicU64::new(0),
            bw_end: AtomicU64::new(0),
            bw_throttled_at: AtomicU64::new(0),
            bw_burst: AtomicU64::new(0),
            bw_extra: AtomicU64::new(0),
            weight: AtomicU32::new(DEFAULT_WEIGHT),
            idle: AtomicBool::new(false),
            load: AtomicI64::new(0),
            contributed: AtomicI64::new(0),
            job: AtomicU64::new(0),
            budget: AtomicU64::new(0),
        }
    }

    /// Its use of `resource`.
    fn used(&self, resource: Resource) -> Option<&AtomicU64> {
        self.used.get(resource.at())
    }

    /// Its limit on `resource`.
    fn limit(&self, resource: Resource) -> Option<&AtomicU64> {
        self.limit.get(resource.at())
    }

    /// Its refusals of `resource`.
    fn refused(&self, resource: Resource) -> Option<&AtomicU64> {
        self.refused.get(resource.at())
    }

    /// Its weight as the scheduler counts it: `cpu.weight` 100 is one task
    /// at nice 0, and a job with `cpu.idle` set is `IDLE_ENTITY_WEIGHT`.
    fn entity_weight(&self) -> i64 {
        if self.idle.load(Ordering::Relaxed) {
            return IDLE_ENTITY_WEIGHT;
        }
        i64::from(self.weight.load(Ordering::Relaxed)) * i64::from(NICE_0_WEIGHT)
            / i64::from(DEFAULT_WEIGHT)
    }

    /// Make what it adds to its parent's load its entity weight now, if it
    /// adds anything: a busy job's parent is told of a new weight at once,
    /// and an idle job's next turn busy reads it. Every change is a swap or a
    /// `fetch_update` that moves the parent's load by the exact difference,
    /// so the load stays the sum of what the children add in every
    /// interleaving. One window is stale, and was `set_weight`'s before
    /// `cpu.idle` shared this: a reweigh between [`adjust`]'s read of the
    /// entity weight as the job turns busy and its swap finds nothing added
    /// and does nothing, and the job adds its old weight until its next
    /// turn busy.
    fn reweigh(&self) {
        let fresh = self.entity_weight();
        let swapped = self
            .contributed
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old != 0).then_some(fresh)
            });
        if let Ok(old) = swapped {
            adjust(self.parent.load(Ordering::Acquire), fresh - old);
        }
    }
}

/// Slots a chunk of the table holds.
const CHUNK: usize = 256;
/// Chunks the table may grow to: 262,144 jobs at once.
const CHUNKS: usize = 1024;

/// The table: chunks made as they are needed, never freed.
static TABLE: [Once<Vec<Slot>>; CHUNKS] = [const { Once::new() }; CHUNKS];

/// How many chunks exist. Raised only under [`CLAIMING`].
static MADE: AtomicU32 = AtomicU32::new(0);

/// Where the next claim starts looking. A hint, read and written anywhere.
static HINT: AtomicU32 = AtomicU32::new(0);

/// Serialises claiming a slot and making a chunk: job creation, in process
/// context.
static CLAIMING: SpinLock<()> = SpinLock::new(());

/// Slots in use, for the boot check that a gone job gave its slot back.
static LIVE: AtomicU64 = AtomicU64::new(0);

/// The slot at `index`, if the table has it.
fn slot(index: u32) -> Option<&'static Slot> {
    let index = usize::try_from(index).ok()?;
    TABLE.get(index / CHUNK)?.get()?.get(index % CHUNK)
}

/// How many slots are in use.
pub fn live_slots() -> u64 {
    LIVE.load(Ordering::Relaxed)
}

/// Claim a free slot under `parent`, making a chunk if every one is taken.
///
/// # Errors
///
/// [`AllocError`] when there is no memory for a chunk, or the table is full.
fn claim(parent: u32) -> Result<u32, AllocError> {
    let _claiming = CLAIMING.lock();
    let index = match find_free() {
        Some(index) => index,
        None => grow()?,
    };
    let taken = slot(index).ok_or(AllocError)?;
    // Free, and nobody else claims under the lock, so nothing races this.
    for resource in Resource::ALL {
        if let (Some(used), Some(limit), Some(refused)) = (
            taken.used(resource),
            taken.limit(resource),
            taken.refused(resource),
        ) {
            used.store(0, Ordering::Relaxed);
            limit.store(UNLIMITED, Ordering::Relaxed);
            refused.store(0, Ordering::Relaxed);
        }
    }
    for (mark, fresh) in taken.marks.iter().zip([UNLIMITED, 0, 0]) {
        mark.store(fresh, Ordering::Relaxed);
    }
    for counter in taken.counters.iter().chain(&taken.cpu_time) {
        counter.store(0, Ordering::Relaxed);
    }
    taken.ended.store(false, Ordering::Relaxed);
    taken.bw_quota.store(UNLIMITED, Ordering::Relaxed);
    taken.bw_period.store(DEFAULT_PERIOD_NS, Ordering::Relaxed);
    taken.bw_used.store(0, Ordering::Relaxed);
    taken.bw_end.store(0, Ordering::Relaxed);
    taken.bw_throttled_at.store(0, Ordering::Relaxed);
    taken.bw_burst.store(0, Ordering::Relaxed);
    taken.bw_extra.store(0, Ordering::Relaxed);
    taken.weight.store(DEFAULT_WEIGHT, Ordering::Relaxed);
    taken.idle.store(false, Ordering::Relaxed);
    taken.load.store(0, Ordering::Relaxed);
    taken.job.store(0, Ordering::Relaxed);
    taken.budget.store(0, Ordering::Relaxed);
    taken.contributed.store(0, Ordering::Relaxed);
    taken.parent.store(parent, Ordering::Relaxed);
    if parent != NONE {
        hold(parent);
    }
    // Published last: a slot is findable by its holds.
    taken.holds.store(1, Ordering::Release);
    let _ = LIVE.fetch_add(1, Ordering::Relaxed);
    Ok(index)
}

/// A free slot in the chunks made so far, from the hint on.
fn find_free() -> Option<u32> {
    let slots = MADE.load(Ordering::Acquire).checked_mul(CHUNK as u32)?;
    let start = HINT.load(Ordering::Relaxed);
    (0..slots)
        .map(|offset| start.wrapping_add(offset) % slots)
        .find(|&index| slot(index).is_some_and(|free| free.holds.load(Ordering::Acquire) == 0))
        .inspect(|&index| HINT.store(index.wrapping_add(1), Ordering::Relaxed))
}

/// Make the next chunk and answer its first slot. Under [`CLAIMING`].
fn grow() -> Result<u32, AllocError> {
    let made = MADE.load(Ordering::Acquire);
    let cell = TABLE.get(made as usize).ok_or(AllocError)?;
    let mut chunk = fallible::try_with_capacity(CHUNK)?;
    for _ in 0..CHUNK {
        // NOALLOC: the room for every slot was had just above.
        if fallible::push_within(&mut chunk, Slot::new()).is_err() {
            return Err(AllocError);
        }
    }
    let _ = cell.call_once(move || chunk);
    MADE.store(made + 1, Ordering::Release);
    Ok(made * CHUNK as u32)
}

/// Keep `index` from being freed.
fn hold(index: u32) {
    if let Some(slot) = slot(index) {
        let _ = slot.holds.fetch_add(1, Ordering::AcqRel);
    }
}

/// Let go of a hold on `index`, freeing it, and then its parent's hold, if it
/// was the last. A loop, not recursion: a chain of jobs is as deep as a
/// program made it.
fn release(index: u32) {
    let mut at = index;
    while let Some(slot) = slot(at) {
        // Read before the hold goes: once it is free, a claim may reuse it.
        let parent = slot.parent.load(Ordering::Acquire);
        if slot.holds.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let _ = LIVE.fetch_sub(1, Ordering::Relaxed);
        at = parent;
    }
}

/// Charge `amount` of `resource` to `index` and every slot above it, if every
/// limit on the way allows it.
///
/// # Errors
///
/// [`Exceeded`], with nothing charged anywhere, and a refusal counted where
/// the limit was.
pub fn charge(index: u32, resource: Resource, amount: u64) -> Result<(), Exceeded> {
    let mut at = index;
    while let Some(slot) = slot(at) {
        let (Some(used), Some(limit)) = (slot.used(resource), slot.limit(resource)) else {
            break;
        };
        let limit = limit.load(Ordering::Acquire);
        let fits = used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
            now.checked_add(amount).filter(|&then| then <= limit)
        });
        if fits.is_err() {
            if let Some(refused) = slot.refused(resource) {
                let _ = refused.fetch_add(1, Ordering::Relaxed);
            }
            uncharge_below(index, at, resource, amount);
            record_refused(index, at, resource, amount, limit);
            return Err(Exceeded);
        }
        at = slot.parent.load(Ordering::Acquire);
    }
    Ok(())
}

/// The audit record of a charge to `index` that the limit at `at` refused
/// (`audit::LIMIT`): against the job whose slot `index` is, by the id and
/// budget the slot keeps, since the running process is not to be found
/// without a lock from inside the heap.
fn record_refused(index: u32, at: u32, resource: Resource, amount: u64, limit: u64) {
    let (job, budget) = slot(index).map_or((0, 0), |slot| {
        (
            slot.job.load(Ordering::Acquire),
            slot.budget.load(Ordering::Acquire),
        )
    });
    crate::audit::limit_refused(job, budget, resource, amount, limit, at);
}

/// The nearest slot from `index` up that a charge of `amount` of `resource`
/// would take past its limit, if any: after a refused charge, the job whose
/// limit refused it, for the scoped OOM kill (`object::oom`). Read, not
/// charged, so a slot under its limit again by now is not named.
pub fn at_limit(index: u32, resource: Resource, amount: u64) -> Option<u32> {
    let mut at = index;
    while let Some(slot) = slot(at) {
        let (Some(used), Some(limit)) = (slot.used(resource), slot.limit(resource)) else {
            return None;
        };
        let then = used.load(Ordering::Acquire).checked_add(amount);
        if then.is_none_or(|then| then > limit.load(Ordering::Acquire)) {
            return Some(at);
        }
        at = slot.parent.load(Ordering::Acquire);
    }
    None
}

/// Whether slot `index` is `scope` or beneath it: whether what is charged to
/// `index` is charged to `scope` too.
pub(crate) fn within(index: u32, scope: u32) -> bool {
    let mut at = index;
    while let Some(slot) = slot(at) {
        if at == scope {
            return true;
        }
        at = slot.parent.load(Ordering::Acquire);
    }
    false
}

/// Slots holding a `cpu.max` quota: what the way back to user mode asks
/// before it looks at any slot, so that a machine with none pays a load.
static BANDWIDTHS: AtomicU32 = AtomicU32::new(0);

/// Nanoseconds every task has used the processor in user and kernel mode,
/// in any job: the root cgroup's `cpu.stat`, which has no slot of its own.
static MACHINE_CPU: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether any `cpu.max` is set.
pub fn bandwidth_in_use() -> bool {
    BANDWIDTHS.load(Ordering::Relaxed) != 0
}

/// Nanoseconds all tasks have used the processor in user mode and in kernel
/// mode, since boot: exact after `sched::charge_running`, which moves what
/// each processor kept back ([`Pending`]) here.
pub(crate) fn machine_cpu() -> (u64, u64) {
    (
        MACHINE_CPU[0].load(Ordering::Relaxed),
        MACHINE_CPU[1].load(Ordering::Relaxed),
    )
}

/// Start the next period of a slot's quota if the one it is in has ended at
/// `now`: its use goes to zero, and the time it spent throttled is counted.
/// Only one of any callers racing on the same end does it.
fn refresh_period(slot: &Slot, now: u64) {
    let period = slot.bw_period.load(Ordering::Relaxed).max(1);
    let end = slot.bw_end.load(Ordering::Acquire);
    if end == 0 {
        // The first charge starts the first period.
        if slot
            .bw_end
            .compare_exchange(
                0,
                now.saturating_add(period),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            bump(slot, Counter::Periods, 1);
        }
        return;
    }
    if now < end {
        return;
    }
    // Every whole period that passed with nothing running is one the job was
    // not busy in: the next starts now.
    let next = now.saturating_add(period);
    if slot
        .bw_end
        .compare_exchange(end, next, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let quota = slot.bw_quota.load(Ordering::Acquire);
    let ended = carry_bound(slot, quota);
    let used = slot.bw_used.swap(0, Ordering::AcqRel);
    carry(slot, used, ended);
    let throttled_at = slot.bw_throttled_at.swap(0, Ordering::AcqRel);
    // Counted as it starts, as Linux's period timer does, so that
    // `nr_periods` is never behind `nr_throttled`.
    bump(slot, Counter::Periods, 1);
    if throttled_at != 0 {
        bump(slot, Counter::ThrottledNs, end.saturating_sub(throttled_at));
    }
}

/// Set slot `index`'s `cpu.max.burst` and drop what was carried, as Linux's
/// `tg_set_cfs_bandwidth` starts the runtime again at the quota.
fn set_burst_at(index: u32, burst: u64) {
    if let Some(slot) = slot(index) {
        slot.bw_burst.store(burst, Ordering::Release);
        slot.bw_extra.store(0, Ordering::Release);
    }
}

/// The most a carry may grant under `cpu.max.burst`: the burst, but never
/// more than the quota, so that a burst left over a quota it no longer fits
/// (two writers of `cpu.max` and `cpu.max.burst` racing past each other's
/// check) grants no more than a burst that fits could.
fn carry_cap(slot: &Slot, quota: u64) -> u64 {
    let burst = slot.bw_burst.load(Ordering::Acquire);
    if quota == UNLIMITED {
        0
    } else {
        burst.min(quota)
    }
}

/// Before a new period's use is cleared: the most it can be carried, so that
/// until [`carry`] stores what it is, a charger compares the new period's
/// use with a runtime no smaller than its own. The window between the two
/// stores may let one charge through that should have been throttled, as
/// the last charge of any period overshoots; it never throttles one that
/// should not be. Answers the runtime the period that ended had.
fn carry_bound(slot: &Slot, quota: u64) -> u64 {
    let ended = allowance(slot, quota);
    let bound = ended.min(carry_cap(slot, quota));
    slot.bw_extra.store(bound, Ordering::Release);
    ended
}

/// What a period that used `used` of its runtime `ended` carries into the
/// next under `cpu.max.burst`, as Linux's `__refill_cfs_bandwidth_runtime`
/// refills: the runtime it left unused, up to the burst (and the quota,
/// [`carry_cap`]), the next period's runtime being the quota and that. A
/// period that used more than its quota under a burst used one, counted in
/// `nr_bursts` and `burst_usec` up to its runtime only: what the last charge
/// overshot by was no burst's, as Linux's pool never goes below zero. However
/// many periods passed with nothing running, one refill is made, as Linux
/// makes one when its period timer starts again. By the one caller that
/// starts the period, after [`carry_bound`].
fn carry(slot: &Slot, used: u64, ended: u64) {
    let quota = slot.bw_quota.load(Ordering::Acquire);
    let cap = carry_cap(slot, quota);
    if cap == 0 {
        slot.bw_extra.store(0, Ordering::Release);
        return;
    }
    let beyond = used.min(ended).saturating_sub(quota);
    if beyond > 0 {
        bump(slot, Counter::Bursts, 1);
        bump(slot, Counter::BurstNs, beyond);
    }
    slot.bw_extra
        .store(ended.saturating_sub(used).min(cap), Ordering::Release);
}

/// A slot's runtime in this period under a `cpu.max` quota of `quota`: the
/// quota, and what `cpu.max.burst` carried from the last period.
fn allowance(slot: &Slot, quota: u64) -> u64 {
    quota.saturating_add(slot.bw_extra.load(Ordering::Acquire))
}

/// Count `amount` of `counter` in `slot` alone.
fn bump(slot: &Slot, counter: Counter, amount: u64) {
    if let Some(held) = slot.counters.get(counter.at()) {
        let _ = held.fetch_add(amount, Ordering::Relaxed);
    }
}

/// Charge `delta` nanoseconds of processor time, used at `now` in user mode
/// (`user`) or in kernel mode, to slot `index` and every slot above it, and
/// to the machine, through `pending`, the charging processor's own, under its
/// run queue's lock. A slot under a quota whose use reaches it is throttled
/// until its period ends. Atomics only.
///
/// The `cpu.stat` part goes to `pending` ([`Pending`]), which a flush moves
/// into the slots: when a charge is for another job, when the processor
/// switches to a task of another job ([`switched`]), when the running task
/// leaves its job ([`leaving`]), and before `cpu.stat` is read ([`settle`]).
/// The `cpu.max` part is made at once, as a quota must be, and only while
/// some `cpu.max` is set: a machine with none pays a load.
pub(crate) fn charge_cpu(pending: &mut Pending, index: u32, now: u64, delta: u64, user: bool) {
    pending.charge(&MACHINE_CPU, index, usize::from(!user), delta);
    if bandwidth_in_use() {
        pending.walks = pending.walks.wrapping_add(1);
        charge_bandwidth(index, now, delta);
    }
}

/// The `cpu.max` part of [`charge_cpu`]: `delta` to the quota of every slot
/// from `index` up that has one.
fn charge_bandwidth(index: u32, now: u64, delta: u64) {
    let mut at = index;
    while let Some(slot) = slot(at) {
        let quota = slot.bw_quota.load(Ordering::Acquire);
        if quota != UNLIMITED {
            refresh_period(slot, now);
            let used = slot
                .bw_used
                .fetch_add(delta, Ordering::AcqRel)
                .saturating_add(delta);
            if used >= allowance(slot, quota)
                && slot
                    .bw_throttled_at
                    .compare_exchange(0, now.max(1), Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                bump(slot, Counter::Throttled, 1);
            }
        }
        at = slot.parent.load(Ordering::Acquire);
    }
}

/// How much longer a task of slot `index` may run at `now` before a
/// `cpu.max` at its slot or above runs out: the least remaining quota on the
/// way up. `None` when none of them has a quota. What arms the timer for a
/// task alone on its processor, which would otherwise get no tick to be
/// throttled at.
pub(crate) fn runtime_left(index: u32, now: u64) -> Option<u64> {
    let mut left: Option<u64> = None;
    let mut at = index;
    while let Some(slot) = slot(at) {
        let quota = slot.bw_quota.load(Ordering::Acquire);
        if quota != UNLIMITED {
            refresh_period(slot, now);
            let remaining =
                allowance(slot, quota).saturating_sub(slot.bw_used.load(Ordering::Acquire));
            left = Some(left.map_or(remaining, |least| least.min(remaining)));
        }
        at = slot.parent.load(Ordering::Acquire);
    }
    left
}

/// When the tasks of slot `index` may run again at `now`: the latest end of
/// a period in which it, or a slot above it, has used its `cpu.max` quota.
/// `None` if they may run.
pub(crate) fn throttled_until(index: u32, now: u64) -> Option<u64> {
    let mut until = None;
    let mut at = index;
    while let Some(slot) = slot(at) {
        let quota = slot.bw_quota.load(Ordering::Acquire);
        if quota != UNLIMITED {
            refresh_period(slot, now);
            if slot.bw_used.load(Ordering::Acquire) >= allowance(slot, quota) {
                let end = slot.bw_end.load(Ordering::Acquire);
                until = until.max(Some(end));
            }
        }
        at = slot.parent.load(Ordering::Acquire);
    }
    until.filter(|&end| end > now)
}

/// Whether a page charged to slot `owner` is spared when `scope` is
/// reclaimed from: some job from `owner` up to, not including, `scope` uses
/// no more than its `memory.min`, or, when `low` is set, its `memory.low`
/// (Linux's `mem_cgroup_protection`, without its proportional share: a job
/// is protected whole or not at all). A job's own reclaim, at its own limit,
/// is never spared by its own marks.
pub(crate) fn protected(owner: u32, scope: u32, low: bool) -> bool {
    let mut at = owner;
    while at != scope {
        let Some(slot) = slot(at) else {
            return false;
        };
        let used = slot
            .used(Resource::Memory)
            .map_or(u64::MAX, |used| used.load(Ordering::Acquire));
        let mark = |which: Mark| {
            slot.marks
                .get(which.at())
                .map_or(0, |held| held.load(Ordering::Relaxed))
        };
        if used <= mark(Mark::Min) || (low && used <= mark(Mark::Low)) {
            return true;
        }
        at = slot.parent.load(Ordering::Acquire);
    }
    false
}

/// Count `amount` of `counter` in `index` and every slot above it.
pub fn count(index: u32, counter: Counter, amount: u64) {
    let mut at = index;
    while let Some(slot) = slot(at) {
        if let Some(held) = slot.counters.get(counter.at()) {
            let _ = held.fetch_add(amount, Ordering::Relaxed);
        }
        at = slot.parent.load(Ordering::Acquire);
    }
}

/// The nearest slot from `index` up whose use of memory is over its
/// `memory.high`, and by how many bytes: where a program that just took a
/// page is made to reclaim (`memory.high`'s throttle, Linux's
/// `mem_cgroup_handle_over_high`). Atomics only; one load per level for a
/// job with no mark set.
pub(crate) fn over_high(index: u32) -> Option<(u32, u64)> {
    let mut at = index;
    while let Some(slot) = slot(at) {
        let high = slot.marks.get(Mark::High.at())?.load(Ordering::Relaxed);
        if high != UNLIMITED
            && let Some(used) = slot.used(Resource::Memory)
        {
            let used = used.load(Ordering::Acquire);
            if used > high {
                return Some((at, used - high));
            }
        }
        at = slot.parent.load(Ordering::Acquire);
    }
    None
}

/// Charge `amount` of `resource` to `index` and every slot above it whatever
/// the limits say: a process moved into a job brings its tasks, as Linux's
/// `pids_can_attach` does.
pub(crate) fn charge_regardless(index: u32, resource: Resource, amount: u64) {
    let mut at = index;
    while let Some(slot) = slot(at) {
        if let Some(used) = slot.used(resource) {
            let _ = used.fetch_add(amount, Ordering::AcqRel);
        }
        at = slot.parent.load(Ordering::Acquire);
    }
}

/// Take back `amount` of `resource` from `index` and every slot above it.
///
/// Never below zero: a count that would go there is a bug the boot check
/// looks for, not a reason to wrap.
pub fn uncharge(index: u32, resource: Resource, amount: u64) {
    uncharge_below(index, NONE, resource, amount);
}

/// [`uncharge`] from `index` up to, not including, `stop`.
fn uncharge_below(index: u32, stop: u32, resource: Resource, amount: u64) {
    let mut at = index;
    while at != stop {
        let Some(slot) = slot(at) else {
            return;
        };
        if let Some(used) = slot.used(resource) {
            let _ = used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
                Some(now.saturating_sub(amount))
            });
        }
        at = slot.parent.load(Ordering::Acquire);
    }
}

/// Charge one frame to `index`, which the frame then holds: what the frame
/// allocator does for a program's memory. Nothing for [`NONE`].
///
/// # Errors
///
/// [`Exceeded`].
pub(crate) fn charge_frame(index: u32) -> Result<(), Exceeded> {
    if index == NONE {
        return Ok(());
    }
    charge(index, Resource::Memory, FRAME_BYTES)?;
    hold(index);
    Ok(())
}

/// Take back a frame [`charge_frame`] charged, as it is freed.
pub(crate) fn uncharge_frame(index: u32) {
    if index == NONE {
        return;
    }
    uncharge(index, Resource::Memory, FRAME_BYTES);
    release(index);
}

/// Charge one frame of a program's page table to `index`, which the frame
/// then holds: memory, against the memory limit, and counted again as
/// [`Resource::Tables`] so `memory.stat` can say how much of it is page
/// tables. Nothing for [`NONE`].
///
/// # Errors
///
/// [`Exceeded`], with nothing charged anywhere.
pub(crate) fn charge_table(index: u32) -> Result<(), Exceeded> {
    if index == NONE {
        return Ok(());
    }
    charge(index, Resource::Memory, FRAME_BYTES)?;
    charge_regardless(index, Resource::Tables, FRAME_BYTES);
    hold(index);
    Ok(())
}

/// Take back a frame [`charge_table`] charged, as it is freed.
pub(crate) fn uncharge_table(index: u32) {
    if index == NONE {
        return;
    }
    uncharge(index, Resource::Tables, FRAME_BYTES);
    uncharge(index, Resource::Memory, FRAME_BYTES);
    release(index);
}

/// The most slots there can be: every slot's index is below it.
pub(crate) const SLOT_LIMIT: u64 = (CHUNK * CHUNKS) as u64;

/// Charge `bytes` of kernel heap to `index` and every slot above it: memory,
/// against the memory limit, and counted again as [`Resource::Kernel`] so
/// `memory.stat` can say how much of it is heap.
///
/// # Errors
///
/// [`Exceeded`], with nothing charged anywhere.
pub(crate) fn charge_kernel(index: u32, bytes: u64) -> Result<(), Exceeded> {
    charge(index, Resource::Memory, bytes)?;
    charge_regardless(index, Resource::Kernel, bytes);
    Ok(())
}

/// Take back what [`charge_kernel`] charged.
pub(crate) fn uncharge_kernel(index: u32, bytes: u64) {
    uncharge(index, Resource::Kernel, bytes);
    uncharge(index, Resource::Memory, bytes);
}

/// The account `ferrix_kmem` charges through: kernel heap a program's calls
/// hold, charged to the running task's job ([`charge_kernel`]), and holding
/// that job's slot for as long as the charge lives.
#[derive(Debug)]
struct KernelHeap;

impl ferrix_kmem::Account for KernelHeap {
    fn current(&self) -> u32 {
        crate::sched::running_group()
    }

    fn charge(&self, owner: u32, bytes: u64) -> Result<(), ferrix_kmem::Refused> {
        charge_kernel(owner, bytes).map_err(|Exceeded| ferrix_kmem::Refused)
    }

    fn uncharge(&self, owner: u32, bytes: u64) {
        uncharge_kernel(owner, bytes);
    }

    fn hold(&self, owner: u32) {
        hold_group(owner);
    }

    fn release(&self, owner: u32) {
        release_group(owner);
    }
}

/// Run `work` charging nothing to anyone: for what the kernel makes for a
/// process whatever its job, and cannot report the failure of -- a new
/// process's first descriptors. Each such use is a constant a process,
/// which the task limit bounds.
pub fn charging_nobody<R>(work: impl FnOnce() -> R) -> R {
    let own = crate::sched::running_group();
    if own == NONE {
        return work();
    }
    crate::sched::set_current_group(NONE);
    let done = work();
    crate::sched::set_current_group(own);
    done
}

/// The one [`KernelHeap`].
static KERNEL_HEAP: KernelHeap = KernelHeap;

/// Charge the kernel heap a program's calls hold to its job from now on.
/// Called as the first job below the root is made: until one is, there is
/// no slot a charge could go to, and the root's programs are charged to
/// nobody anyway.
fn install_kernel_heap() {
    ferrix_kmem::install(&KERNEL_HEAP);
}

/// A charge held by whatever it was made for, and taken back as it is dropped:
/// an object's, for as long as the object exists.
#[derive(Debug)]
pub(crate) struct Charge {
    /// The slot charged, or [`NONE`] for nothing.
    index: u32,
    /// What was charged.
    resource: Resource,
    /// How much.
    amount: u64,
}

impl Charge {
    /// Charge `amount` of `resource` to the job the running task is in.
    ///
    /// # Errors
    ///
    /// [`Exceeded`].
    pub(crate) fn running(resource: Resource, amount: u64) -> Result<Charge, Exceeded> {
        Charge::to(crate::sched::running_group(), resource, amount)
    }

    /// Charge `amount` of `resource` to `index`.
    ///
    /// # Errors
    ///
    /// [`Exceeded`].
    pub(crate) fn to(index: u32, resource: Resource, amount: u64) -> Result<Charge, Exceeded> {
        if index != NONE {
            charge(index, resource, amount)?;
            hold(index);
        }
        Ok(Charge {
            index,
            resource,
            amount,
        })
    }

    /// Nothing charged.
    pub(crate) const fn none(resource: Resource) -> Charge {
        Charge {
            index: NONE,
            resource,
            amount: 0,
        }
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if self.index != NONE {
            uncharge(self.index, self.resource, self.amount);
            release(self.index);
        }
    }
}

/// One job's claim on its slot: made with the job, and let go as it drops.
#[derive(Debug)]
pub struct Quota {
    /// The slot.
    index: u32,
}

/// What a job holds of one resource, and may.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Usage {
    /// Its use, its descendants' included.
    pub used: u64,
    /// Its limit, or [`UNLIMITED`].
    pub limit: u64,
    /// How many charges its limit refused.
    pub refused: u64,
}

impl Quota {
    /// A slot under `parent`'s, or at the top of a tree for `None`.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub fn new(parent: Option<&Quota>) -> Result<Quota, AllocError> {
        install_kernel_heap();
        let index = claim(parent.map_or(NONE, |parent| parent.index))?;
        Ok(Quota { index })
    }

    /// Its slot's index.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Say which job it is the quota of, and that job's audit budget, for
    /// the record of a charge it refuses.
    pub(crate) fn identify(&self, job: u64, budget: u64) {
        if let Some(slot) = slot(self.index) {
            slot.job.store(job, Ordering::Release);
            slot.budget.store(budget, Ordering::Release);
        }
    }

    /// Set its limit on `resource`. A limit below its use refuses every
    /// charge until the use falls under it, and takes nothing away.
    pub fn set_limit(&self, resource: Resource, limit: u64) {
        if let Some(held) = slot(self.index).and_then(|slot| slot.limit(resource)) {
            held.store(limit, Ordering::Release);
        }
    }

    /// Its use of `resource`, its limit and its refusals.
    pub fn usage(&self, resource: Resource) -> Usage {
        let read = |field: Option<&AtomicU64>, otherwise| {
            field.map_or(otherwise, |field| field.load(Ordering::Acquire))
        };
        let slot = slot(self.index);
        Usage {
            used: read(slot.and_then(|slot| slot.used(resource)), 0),
            limit: read(slot.and_then(|slot| slot.limit(resource)), UNLIMITED),
            refused: read(slot.and_then(|slot| slot.refused(resource)), 0),
        }
    }

    /// Its `memory.high`, `memory.low` or `memory.min`: [`UNLIMITED`] for a
    /// `max`.
    pub(crate) fn mark(&self, mark: Mark) -> u64 {
        slot(self.index)
            .and_then(|slot| slot.marks.get(mark.at()))
            .map_or(0, |held| held.load(Ordering::Acquire))
    }

    /// Set a mark.
    pub(crate) fn set_mark(&self, mark: Mark, value: u64) {
        if let Some(held) = slot(self.index).and_then(|slot| slot.marks.get(mark.at())) {
            held.store(value, Ordering::Release);
        }
    }

    /// How many of `counter` were counted in it and beneath it.
    pub fn counted(&self, counter: Counter) -> u64 {
        slot(self.index)
            .and_then(|slot| slot.counters.get(counter.at()))
            .map_or(0, |held| held.load(Ordering::Acquire))
    }

    /// Its `cpu.max`: the quota in nanoseconds, or [`UNLIMITED`], and the
    /// period.
    pub(crate) fn bandwidth(&self) -> (u64, u64) {
        slot(self.index).map_or((UNLIMITED, DEFAULT_PERIOD_NS), |slot| {
            (
                slot.bw_quota.load(Ordering::Acquire),
                slot.bw_period.load(Ordering::Acquire),
            )
        })
    }

    /// Set its `cpu.max`, starting a period of it from the next charge.
    pub(crate) fn set_bandwidth(&self, quota: u64, period: u64) {
        let Some(slot) = slot(self.index) else {
            return;
        };
        let before = slot.bw_quota.swap(UNLIMITED, Ordering::AcqRel);
        slot.bw_period.store(period, Ordering::Release);
        slot.bw_used.store(0, Ordering::Release);
        slot.bw_end.store(0, Ordering::Release);
        slot.bw_throttled_at.store(0, Ordering::Release);
        slot.bw_extra.store(0, Ordering::Release);
        slot.bw_quota.store(quota, Ordering::Release);
        match (before == UNLIMITED, quota == UNLIMITED) {
            (true, false) => {
                let _ = BANDWIDTHS.fetch_add(1, Ordering::AcqRel);
            }
            (false, true) => {
                let _ = BANDWIDTHS.fetch_sub(1, Ordering::AcqRel);
            }
            _ => {}
        }
    }

    /// Its `cpu.max.burst`, in nanoseconds.
    pub(crate) fn burst(&self) -> u64 {
        slot(self.index).map_or(0, |slot| slot.bw_burst.load(Ordering::Acquire))
    }

    /// Set its `cpu.max.burst`. What was carried is dropped, as Linux's
    /// `tg_set_cfs_bandwidth` starts the runtime again at the quota.
    pub(crate) fn set_burst(&self, burst: u64) {
        set_burst_at(self.index, burst);
    }

    /// Nanoseconds its tasks used the processor in user mode and in kernel
    /// mode, its descendants' included: exact after `sched::charge_running`,
    /// which moves what each processor kept back ([`Pending`]) here.
    pub(crate) fn cpu_times(&self) -> (u64, u64) {
        slot(self.index).map_or((0, 0), |slot| {
            (
                slot.cpu_time[0].load(Ordering::Relaxed),
                slot.cpu_time[1].load(Ordering::Relaxed),
            )
        })
    }

    /// Its `cpu.weight`.
    pub(crate) fn weight(&self) -> u32 {
        slot(self.index).map_or(DEFAULT_WEIGHT, |slot| slot.weight.load(Ordering::Relaxed))
    }

    /// Set its `cpu.weight`, clamped to what the file takes. A busy job's
    /// parent is told at once.
    pub(crate) fn set_weight(&self, weight: u32) {
        let Some(slot) = slot(self.index) else {
            return;
        };
        slot.weight
            .store(weight.clamp(MIN_WEIGHT, MAX_WEIGHT), Ordering::Relaxed);
        slot.reweigh();
    }

    /// Its `cpu.idle`.
    pub(crate) fn idle(&self) -> bool {
        slot(self.index).is_some_and(|slot| slot.idle.load(Ordering::Relaxed))
    }

    /// Set its `cpu.idle`.
    pub(crate) fn set_idle(&self, idle: bool) {
        set_idle_at(self.index, idle);
    }
}

/// What a job's going tells whoever keeps state of its own per job slot
/// (`fs::blkio`): the core cannot name it, so it is told through this.
static RELEASE_HOOK: Once<fn(u32)> = Once::new();

/// Have `hook` called with a job's slot as the job goes, before the slot
/// lets go of its parent. Once; a second call is ignored.
pub fn on_job_release(hook: fn(u32)) {
    let _ = RELEASE_HOOK.call_once(|| hook);
}

/// The slot above `index`, or [`NONE`] at the top of a tree.
pub fn parent_of(index: u32) -> u32 {
    slot(index).map_or(NONE, |slot| slot.parent.load(Ordering::Acquire))
}

/// Whether the job `index` was the quota of has gone. Read under the lock
/// whoever keeps state per slot takes in its release hook, so that a task still
/// holding the slot cannot start state the hook has already cleared.
pub fn job_ended(index: u32) -> bool {
    slot(index).is_some_and(|slot| slot.ended.load(Ordering::Acquire))
}

impl Drop for Quota {
    fn drop(&mut self) {
        // Before the hook, which takes the lock `job_ended` is read under.
        if let Some(slot) = slot(self.index) {
            slot.ended.store(true, Ordering::Release);
        }
        if let Some(hook) = RELEASE_HOOK.get() {
            hook(self.index);
        }
        // A quota set on a job that goes leaves no mark behind.
        self.set_bandwidth(UNLIMITED, DEFAULT_PERIOD_NS);
        release(self.index);
    }
}

/// Keep `index` for a task that names it for the scheduler.
pub(crate) fn hold_group(index: u32) {
    if index != NONE {
        hold(index);
    }
}

/// Let go of what [`hold_group`] kept.
pub(crate) fn release_group(index: u32) {
    if index != NONE {
        release(index);
    }
}

/// A task of `index` of weight `weight` became runnable, or, negative,
/// stopped being: its job's load changes, and every job above whose busy or
/// idle state that flipped.
pub(crate) fn adjust(index: u32, delta: i64) {
    let mut at = index;
    let mut delta = delta;
    while delta != 0 {
        let Some(slot) = slot(at) else {
            return;
        };
        let before = slot.load.fetch_add(delta, Ordering::AcqRel);
        let after = before.saturating_add(delta);
        if (before > 0) == (after > 0) {
            return;
        }
        // It turned busy or idle: what it adds to its parent changes with
        // it, by exactly what it last added, so the parent's load is always
        // the sum of what its children say.
        let fresh = if after > 0 { slot.entity_weight() } else { 0 };
        delta = fresh - slot.contributed.swap(fresh, Ordering::AcqRel);
        at = slot.parent.load(Ordering::Acquire);
    }
}

/// The weight a task of weight `base` in `index` runs at: `base` times, at
/// each level from `index` up, that job's weight over its load.
///
/// The arithmetic is `ferrix_sched_fair::carried_weight`'s, in 64 bits, which is
/// the 128-bit formula's exactly while every entity weight is below 2^32
/// (the assertion below): a task's base is a `u32`, and a job's is
/// `cpu.weight` clamped to [`MAX_WEIGHT`] and scaled to task units.
pub fn effective(index: u32, base: u32) -> u32 {
    effective_with(index, base, 0)
}

/// [`effective`], with `extra` added to `index`'s own load and to no other
/// level's: the direct switch's weights, made while the peer's weight is
/// counted in its word and not yet in the load it shares with the caller
/// (`docs/OPAQUE-KERNEL.md` §9.11, J2; `ferrix_sched_fair::carried_weight_with`).
/// One walk, each level read once, as `effective`'s.
pub fn effective_with(index: u32, base: u32, extra: i64) -> u32 {
    let mut at = index;
    let levels = core::iter::from_fn(|| {
        let slot = slot(at)?;
        // What the level below adds to this one's load, which the load is
        // never taken to be less than, is applied in `carried_weight`: a
        // task or job not yet counted, or counted a moment late, must not be
        // scaled up by a load that leaves it out.
        let load = slot.load.load(Ordering::Acquire);
        let own = u32::try_from(slot.entity_weight()).unwrap_or(1);
        at = slot.parent.load(Ordering::Acquire);
        Some((own, load))
    });
    let clamped = ferrix_sched_fair::carried_weight_with(base, levels, extra)
        .clamp(MIN_EFFECTIVE, MAX_EFFECTIVE);
    u32::try_from(clamped).unwrap_or(NICE_0_WEIGHT)
}

/// Every entity weight a job can be given fits a `u32`: the largest
/// `cpu.weight`, [`MAX_WEIGHT`], in task units. What makes
/// [`effective`]'s 64-bit arithmetic exact; a task's own weight is a `u32`
/// already. Checked when the kernel is built.
const _: () = assert!(
    (MAX_WEIGHT as u64) * (NICE_0_WEIGHT as u64) / (DEFAULT_WEIGHT as u64) <= u32::MAX as u64,
    "a job's largest entity weight must fit a u32 for effective's 64-bit arithmetic"
);

/// The load of `index`, for a check.
pub fn load(index: u32) -> i64 {
    slot(index).map_or(0, |slot| slot.load.load(Ordering::Acquire))
}

/// Add `delta` nanoseconds in `mode` (0 user, 1 kernel) to `machine` and to
/// the `cpu.stat` time of slot `index` and every slot above it: what a flush
/// of a [`Pending`] charge does.
fn add_cpu_time(machine: &[AtomicU64; 2], index: u32, mode: usize, delta: u64) {
    if let Some(total) = machine.get(mode) {
        let _ = total.fetch_add(delta, Ordering::Relaxed);
    }
    let mut at = index;
    while let Some(slot) = slot(at) {
        if let Some(time) = slot.cpu_time.get(mode) {
            let _ = time.fetch_add(delta, Ordering::Relaxed);
        }
        at = slot.parent.load(Ordering::Acquire);
    }
}

/// The processor time one processor has charged to one job and not yet
/// added to the job's slots: [`charge_cpu`]'s `cpu.stat` part, made with an
/// add to a field where the walk up the slots costs a locked add a level.
///
/// A field of the processor's run queue (`sched::queue::CpuQueue`), so only
/// ever touched under that queue's lock: by the queue's charges, by the
/// switch ([`switched`]), by the running task's own move to another job
/// (`sched::set_task_group`), and by `sched::charge_running` ([`settle`]),
/// which the only reader of the slots' time, `cpu.stat`, calls first. So
/// nothing charged is counted twice or lost, and a read counts everything
/// charged before it, as when each charge walked the slots itself.
#[derive(Debug)]
pub(crate) struct Pending {
    /// The slot the time is charged to, held while it is (so that it is not
    /// freed, and its index claimed again, with time owed to it), or
    /// [`NONE`]: the machine's alone.
    group: u32,
    /// Nanoseconds in user mode and in kernel mode.
    time: [u64; 2],
    /// How many charges made the `cpu.max` walk ([`charge_bandwidth`]), for
    /// the check that none does while no `cpu.max` is set.
    walks: u64,
}

impl Pending {
    /// Nothing pending.
    pub(crate) const fn new() -> Pending {
        Pending {
            group: NONE,
            time: [0; 2],
            walks: 0,
        }
    }

    /// Charge `delta` nanoseconds in `mode` to slot `index`: flushed first
    /// into `machine` and the slots if what is pending is another job's.
    fn charge(&mut self, machine: &[AtomicU64; 2], index: u32, mode: usize, delta: u64) {
        if self.group != index {
            self.flush(machine, index);
        }
        if let Some(time) = self.time.get_mut(mode) {
            *time = time.wrapping_add(delta);
        }
    }

    /// Move what is pending into `machine` and its job's slots, and charge
    /// to `to` from now on, holding its slot instead of the one it held.
    fn flush(&mut self, machine: &[AtomicU64; 2], to: u32) {
        let from = self.group;
        for (mode, time) in self.time.iter_mut().enumerate() {
            if *time != 0 {
                add_cpu_time(machine, from, mode, *time);
                *time = 0;
            }
        }
        if to != from {
            // `to` is held by the task being charged to it, so it is live.
            hold_group(to);
            self.group = to;
            release_group(from);
        }
    }

    /// How many charges made the `cpu.max` walk.
    pub(crate) fn walks(&self) -> u64 {
        self.walks
    }

    /// The slot it keeps time back for, or [`NONE`].
    pub(crate) fn group(&self) -> u32 {
        self.group
    }
}

/// The processor whose pending charge `pending` is switched to a task charged
/// to `group`: what it has pending for another job goes to that job's slots
/// now, so that a processor gone idle owes nothing and holds no gone job's
/// slot. One compare when the job is the same.
pub(crate) fn switched(pending: &mut Pending, group: u32) {
    if pending.group != group {
        pending.flush(&MACHINE_CPU, group);
    }
}

/// The running task on the processor whose pending charge this is leaves the
/// job it was charged to: what is pending goes to that job's slots, and the
/// hold on it goes, before the task's group changes.
pub(crate) fn leaving(pending: &mut Pending) {
    pending.flush(&MACHINE_CPU, NONE);
}

/// Move everything `pending` holds into the slots, for a reader of
/// `cpu.stat`.
pub(crate) fn settle(pending: &mut Pending) {
    let keep = pending.group;
    pending.flush(&MACHINE_CPU, keep);
}

/// Stage 13's check of the [`Pending`] charge, on records and machine
/// counters of its own and two slots claimed for it, a job and its child: a
/// charge stays pending and holds its slot; a charge to another job moves
/// the first into the slots, once; a flush settles everything and lets go
/// of what it held.
///
/// # Errors
///
/// The first thing that was not so, by name.
pub fn check_pending_charge() -> Result<(), &'static str> {
    let parent = claim(NONE).map_err(|_| "pending charge check: no slot to claim")?;
    let outcome = match claim(parent) {
        Ok(child) => {
            let outcome = pending_cases(parent, child);
            release(child);
            outcome
        }
        Err(_) => Err("pending charge check: no slot to claim"),
    };
    release(parent);
    outcome
}

/// The cases of [`check_pending_charge`].
fn pending_cases(parent: u32, child: u32) -> Result<(), &'static str> {
    let machine = [const { AtomicU64::new(0) }; 2];
    let (mut one, mut two) = (Pending::new(), Pending::new());
    let pair = |held: &[AtomicU64; 2]| {
        [
            held[0].load(Ordering::Relaxed),
            held[1].load(Ordering::Relaxed),
        ]
    };
    let times = |index: u32| slot(index).map_or([u64::MAX; 2], |slot| pair(&slot.cpu_time));
    let holds = |index: u32| slot(index).map_or(0, |slot| slot.holds.load(Ordering::Acquire));
    let (child_holds, parent_holds) = (holds(child), holds(parent));

    one.charge(&machine, child, 0, 1_000);
    two.charge(&machine, child, 1, 20);
    if (times(child), pair(&machine)) != ([0, 0], [0, 0]) {
        return Err("a pending processor-time charge reached the slots before a flush");
    }
    if (one.time, two.time, one.group, two.group) != ([1_000, 0], [0, 20], child, child) {
        return Err("a processor-time charge was not kept pending for its job");
    }
    if holds(child) != child_holds + 2 {
        return Err("a pending processor-time charge does not hold its job's slot");
    }
    one.charge(&machine, parent, 1, 300);
    if (times(child), times(parent), pair(&machine)) != ([1_000, 0], [1_000, 0], [1_000, 0]) {
        return Err("a charge to another job did not move what was pending into the slots once");
    }
    one.flush(&machine, NONE);
    two.flush(&machine, NONE);
    if (times(child), times(parent), pair(&machine)) != ([1_000, 20], [1_000, 320], [1_000, 320]) {
        return Err("a flush did not settle every pending processor-time charge exactly once");
    }
    if (one.time, two.time) != ([0, 0], [0, 0]) {
        return Err("a flush left processor time pending");
    }
    if (holds(child), holds(parent)) != (child_holds, parent_holds) {
        return Err("a flush did not let go of the slot the pending charge held");
    }
    Ok(())
}

/// Stage 13's check of `cpu.max.burst`'s carry, on a slot claimed for it
/// and charged at instants chosen from `now` on, so that what each period
/// leaves is known exactly. Under a quota of 20 ms a 100 ms period and a
/// burst of 20 ms: a period that used 5 ms leaves the next 35 ms, in which
/// 30 ms of use is not throttled and 36 ms is, counted as one burst of 15 ms
/// (what the period had beyond its quota, not the last charge's overshoot);
/// the next period has its quota alone, and 21 ms of use there counts no
/// burst; and with the burst at 0, a period that used 5 ms carries
/// nothing.
///
/// # Errors
///
/// The first thing that was not so, by name.
// CHECKS-ONLY: fs/cgroupfs/cpu_check.rs
pub fn check_burst_carry(now: u64) -> Result<(), &'static str> {
    let index = claim(NONE).map_err(|_| "burst check: no slot to claim")?;
    let outcome = burst_cases(index, now);
    release(index);
    outcome
}

/// The cases of [`check_burst_carry`].
fn burst_cases(index: u32, start: u64) -> Result<(), &'static str> {
    const MS: u64 = 1_000_000;
    let held = slot(index).ok_or("burst check: a claimed slot is not there")?;
    let period = 100 * MS;
    held.bw_period.store(period, Ordering::Release);
    held.bw_quota.store(20 * MS, Ordering::Release);
    set_burst_at(index, 20 * MS);
    let counted = |counter: Counter| {
        held.counters
            .get(counter.at())
            .map_or(u64::MAX, |value| value.load(Ordering::Acquire))
    };

    charge_bandwidth(index, start, 5 * MS);
    if runtime_left(index, start + 1) != Some(15 * MS) {
        return Err("a period under cpu.max did not leave its quota less what it used");
    }
    let next = start + period;
    if runtime_left(index, next) != Some(35 * MS) {
        return Err("cpu.max.burst did not carry a period's unused quota into the next");
    }
    // Written again, the burst drops what was carried; then the period is
    // started over with its carry, as before.
    set_burst_at(index, 20 * MS);
    if runtime_left(index, next) != Some(20 * MS) {
        return Err("a cpu.max.burst written again kept what was carried");
    }
    held.bw_extra.store(15 * MS, Ordering::Release);
    charge_bandwidth(index, next + 1, 30 * MS);
    if counted(Counter::Throttled) != 0 {
        return Err("a charge within what cpu.max.burst carried throttled its period");
    }
    if throttled_until(index, next + 2).is_some() {
        return Err("a period running on what cpu.max.burst carried was throttled at its quota");
    }
    charge_bandwidth(index, next + 3, 6 * MS);
    if throttled_until(index, next + 4) != Some(next + period) || counted(Counter::Throttled) != 1 {
        return Err("a period past its quota and its carried burst was not throttled");
    }
    let after = next + period;
    if runtime_left(index, after) != Some(20 * MS) {
        return Err("a period that used its burst carried something into the next");
    }
    if (counted(Counter::Bursts), counted(Counter::BurstNs)) != (1, 15 * MS) {
        return Err(
            "cpu.stat's nr_bursts and burst_usec did not count the burst a period used, up to \
             what was carried",
        );
    }
    // Nothing was carried into this period: 21 ms overshoots its quota by
    // the last charge, and that is no burst.
    charge_bandwidth(index, after + 1, 21 * MS);
    if runtime_left(index, after + period) != Some(20 * MS)
        || (counted(Counter::Bursts), counted(Counter::BurstNs)) != (1, 15 * MS)
    {
        return Err("a period that overshot its quota with nothing carried counted a burst");
    }
    let after = after + period;
    set_burst_at(index, 0);
    charge_bandwidth(index, after + 1, 5 * MS);
    if runtime_left(index, after + period) != Some(20 * MS) {
        return Err("a period with cpu.max.burst 0 carried unused quota into the next");
    }
    held.bw_quota.store(UNLIMITED, Ordering::Release);
    Ok(())
}

/// Set slot `index`'s `cpu.idle`, as Linux's `sched_group_set_idle` does
/// with weights: an idle job's entity weight is `IDLE_ENTITY_WEIGHT`, and
/// a busy job's parent is told at once.
fn set_idle_at(index: u32, idle: bool) {
    if let Some(slot) = slot(index) {
        slot.idle.store(idle, Ordering::Relaxed);
        slot.reweigh();
    }
}

/// Stage 13's check of `cpu.idle`, on a job and two children claimed for it,
/// each child holding one runnable task of weight 1024 so that both are busy
/// and share the parent evenly, 512 a task. With one child at `cpu.weight`
/// 10000 and then set idle, the parent's load is 1024 and 3 at once, the idle
/// child's task runs at no more than 3 and its sibling's at more than 1000;
/// set back, the parent's load and both tasks' weights are what they were at
/// 10000.
///
/// # Errors
///
/// The first thing that was not so, by name.
// CHECKS-ONLY: fs/cgroupfs/cpu_check.rs
pub fn check_idle_weight() -> Result<(), &'static str> {
    let parent = claim(NONE).map_err(|_| "idle check: no slot to claim")?;
    let outcome = match (claim(parent), claim(parent)) {
        (Ok(idle), Ok(busy)) => {
            let outcome = idle_cases(parent, idle, busy);
            release(idle);
            release(busy);
            outcome
        }
        (Ok(one), Err(_)) | (Err(_), Ok(one)) => {
            release(one);
            Err("idle check: no slot to claim")
        }
        (Err(_), Err(_)) => Err("idle check: no slot to claim"),
    };
    release(parent);
    outcome
}

/// The cases of [`check_idle_weight`].
fn idle_cases(parent: u32, idle: u32, busy: u32) -> Result<(), &'static str> {
    let nice_0 = i64::from(NICE_0_WEIGHT);
    let load = |index: u32| slot(index).map_or(i64::MIN, |slot| slot.load.load(Ordering::Acquire));
    let even = NICE_0_WEIGHT / 2;
    adjust(idle, nice_0);
    adjust(busy, nice_0);
    let outcome = (|| {
        if (
            effective(idle, NICE_0_WEIGHT),
            effective(busy, NICE_0_WEIGHT),
        ) != (even, even)
        {
            return Err("idle check: two busy jobs' tasks did not share their parent evenly");
        }
        // At the most weight, so that the idle weight is shown to be the
        // idle weight whatever the job's, and letting it off to restore its
        // own.
        let heavy = slot(idle).ok_or("idle check: a claimed slot is not there")?;
        heavy.weight.store(MAX_WEIGHT, Ordering::Relaxed);
        heavy.reweigh();
        let own = heavy.entity_weight();
        let before = (
            load(parent),
            effective(idle, NICE_0_WEIGHT),
            effective(busy, NICE_0_WEIGHT),
        );
        if before.0 != own + nice_0 {
            return Err("idle check: a busy job's weight did not reach its parent's load");
        }
        set_idle_at(idle, true);
        if load(parent) != nice_0 + IDLE_ENTITY_WEIGHT {
            return Err("a busy job set to cpu.idle did not give its parent the idle weight");
        }
        if effective(idle, NICE_0_WEIGHT) > 3 || effective(busy, NICE_0_WEIGHT) <= 1000 {
            return Err(
                "a task of a job with cpu.idle set did not run at the idle weight beside a busy \
                 sibling",
            );
        }
        set_idle_at(idle, false);
        let after = (
            load(parent),
            effective(idle, NICE_0_WEIGHT),
            effective(busy, NICE_0_WEIGHT),
        );
        if after != before {
            return Err("a job let off cpu.idle did not run at its cpu.weight again");
        }
        Ok(())
    })();
    adjust(idle, -nice_0);
    adjust(busy, -nice_0);
    outcome
}

/// What `index` adds to its parent's load: its own weight while it is busy,
/// nothing while idle. For a check.
pub fn contributed(index: u32) -> i64 {
    slot(index).map_or(0, |slot| slot.contributed.load(Ordering::Acquire))
}
