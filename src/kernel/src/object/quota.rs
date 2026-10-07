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
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use ferrix_sched::NICE_0_WEIGHT;
use ferrix_sync::Once;

use crate::fallible::{self, AllocError};
use crate::sync::SpinLock;

/// No slot: the tree's root, a kernel thread, a frame nobody is charged for.
pub(crate) const NONE: u32 = u32::MAX;

/// A limit that is no limit.
pub(crate) const UNLIMITED: u64 = u64::MAX;

/// What a job is charged for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Resource {
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
}

impl Resource {
    /// Every resource, in the order the slot keeps them.
    pub(crate) const ALL: [Resource; RESOURCES] = [
        Resource::Memory,
        Resource::Objects,
        Resource::Tasks,
        Resource::Kernel,
    ];

    /// Where the slot keeps it.
    const fn at(self) -> usize {
        match self {
            Resource::Memory => 0,
            Resource::Objects => 1,
            Resource::Tasks => 2,
            Resource::Kernel => 3,
        }
    }
}

/// How many resources a slot counts.
const RESOURCES: usize = 4;

/// What a frame is charged as: a page of memory.
const FRAME_BYTES: u64 = ferrix_bootinfo::PAGE_SIZE;

/// A charge a limit refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Exceeded;

/// `cpu.weight`'s default, and Linux's.
pub(crate) const DEFAULT_WEIGHT: u32 = 100;
/// The least `cpu.weight` takes.
pub(crate) const MIN_WEIGHT: u32 = 1;
/// The most `cpu.weight` takes.
pub(crate) const MAX_WEIGHT: u32 = 10_000;

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
    /// Its parent's slot, or [`NONE`].
    parent: AtomicU32,
    /// Use of each resource, its own and every descendant's.
    used: [AtomicU64; RESOURCES],
    /// Each resource's limit.
    limit: [AtomicU64; RESOURCES],
    /// How many charges of each resource a limit here refused.
    refused: [AtomicU64; RESOURCES],
    /// `cpu.weight`.
    weight: AtomicU32,
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
            parent: AtomicU32::new(NONE),
            used: [const { AtomicU64::new(0) }; RESOURCES],
            limit: [const { AtomicU64::new(UNLIMITED) }; RESOURCES],
            refused: [const { AtomicU64::new(0) }; RESOURCES],
            weight: AtomicU32::new(DEFAULT_WEIGHT),
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
    /// at nice 0.
    fn entity_weight(&self) -> i64 {
        i64::from(self.weight.load(Ordering::Relaxed)) * i64::from(NICE_0_WEIGHT)
            / i64::from(DEFAULT_WEIGHT)
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
pub(crate) fn live_slots() -> u64 {
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
    taken.weight.store(DEFAULT_WEIGHT, Ordering::Relaxed);
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
pub(crate) fn charge(index: u32, resource: Resource, amount: u64) -> Result<(), Exceeded> {
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
pub(crate) fn at_limit(index: u32, resource: Resource, amount: u64) -> Option<u32> {
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
pub(crate) fn uncharge(index: u32, resource: Resource, amount: u64) {
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
pub(crate) fn charging_nobody<R>(work: impl FnOnce() -> R) -> R {
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
pub(crate) struct Quota {
    /// The slot.
    index: u32,
}

/// What a job holds of one resource, and may.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Usage {
    /// Its use, its descendants' included.
    pub(crate) used: u64,
    /// Its limit, or [`UNLIMITED`].
    pub(crate) limit: u64,
    /// How many charges its limit refused.
    pub(crate) refused: u64,
}

impl Quota {
    /// A slot under `parent`'s, or at the top of a tree for `None`.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn new(parent: Option<&Quota>) -> Result<Quota, AllocError> {
        install_kernel_heap();
        let index = claim(parent.map_or(NONE, |parent| parent.index))?;
        Ok(Quota { index })
    }

    /// Its slot's index.
    pub(crate) fn index(&self) -> u32 {
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
    pub(crate) fn set_limit(&self, resource: Resource, limit: u64) {
        if let Some(held) = slot(self.index).and_then(|slot| slot.limit(resource)) {
            held.store(limit, Ordering::Release);
        }
    }

    /// Its use of `resource`, its limit and its refusals.
    pub(crate) fn usage(&self, resource: Resource) -> Usage {
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
        let fresh = slot.entity_weight();
        // Only while it contributes: an idle job's next turn busy reads the
        // new weight.
        let swapped = slot
            .contributed
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old != 0).then_some(fresh)
            });
        if let Ok(old) = swapped {
            adjust(slot.parent.load(Ordering::Acquire), fresh - old);
        }
    }
}

impl Drop for Quota {
    fn drop(&mut self) {
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
/// The arithmetic is `ferrix_sched::carried_weight`'s, in 64 bits, which is
/// the 128-bit formula's exactly while every entity weight is below 2^32
/// (the assertion below): a task's base is a `u32`, and a job's is
/// `cpu.weight` clamped to [`MAX_WEIGHT`] and scaled to task units.
pub(crate) fn effective(index: u32, base: u32) -> u32 {
    effective_with(index, base, 0)
}

/// [`effective`], with `extra` added to `index`'s own load and to no other
/// level's: the direct switch's weights, made while the peer's weight is
/// counted in its word and not yet in the load it shares with the caller
/// (`docs/OPAQUE-KERNEL.md` §9.11, J2; `ferrix_sched::carried_weight_with`).
/// One walk, each level read once, as `effective`'s.
pub(crate) fn effective_with(index: u32, base: u32, extra: i64) -> u32 {
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
    let clamped =
        ferrix_sched::carried_weight_with(base, levels, extra).clamp(MIN_EFFECTIVE, MAX_EFFECTIVE);
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
pub(crate) fn load(index: u32) -> i64 {
    slot(index).map_or(0, |slot| slot.load.load(Ordering::Acquire))
}
