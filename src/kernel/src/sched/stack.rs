//! The scheduling-class stack this kernel is built with
//! (`docs/SCHED-CLASSES.md` §5).
//!
//! Every operation the scheduler makes on a CPU's classes goes through
//! [`Stack`], named here once as a type, so the compiler monomorphises and
//! inlines it: the choice costs nothing at run time. The selection crate
//! below the core chooses it by the per-tier build flags
//! (`ferrix_kernel_sched_select`, S2, §9a); this file reads none. Every
//! buildable profile has the fair tier alone, so `Stack` is
//! [`ferrix_sched_fair::Eevdf`] and its queue is EEVDF's `RunQueue`.
//!
//! **The fair tier's own calls.** The scheduler's fair-only calls --
//! weights, the picture `/proc` and the checks read, the slice, the
//! levelling, the steal choice -- reach the fair tier through
//! `ferrix_sched_fair::FairTier`, read-only but for its named forwarders;
//! every change of membership or of the running entity goes through the
//! stack. docs/SCHED-CLASSES.md §9d maps each call (F2, F16), and
//! check-item-boundary refuses a `&mut RunQueue` taken anywhere in the
//! kernel (F1).

use alloc::sync::Arc;

use ferrix_sched_fair::StackTiers;

use super::task::Task;

/// The class stack, as the build flags compose it.
pub(crate) use ferrix_kernel_sched_select::Stack;

/// One CPU's queue of [`Stack`].
pub(crate) type StackQueue = <Stack as ferrix_sched_class::SchedClass<Arc<Task>>>::Queue;

/// What a task is queued with in [`Stack`]: for the fair class alone, its
/// record and its run slot; with a tier above fair, either that, as the
/// lower tier's entity, or its policy and its node of the upper tier
/// (`Task::entity_with`, `Task::keep_entity`).
pub(crate) type StackEntity = <Stack as ferrix_sched_class::SchedClass<Arc<Task>>>::Entity;

/// A task's node in the tier above fair (docs/SCHED-CLASSES.md §9e, R1);
/// a type with no values when the stack has no such tier.
pub(crate) type UpperSlot = <Stack as StackTiers<Arc<Task>>>::UpperSlot;

/// Where a task keeps that node while off every queue (F6, L.sched.107).
pub(crate) type UpperCellOf = <Stack as StackTiers<Arc<Task>>>::Cell;

/// Whether the stack has a tier above fair: whether a task may ask for a
/// real-time policy (`sched_setscheduler`, L.sched.116).
pub(crate) const REALTIME: bool = <Stack as StackTiers<Arc<Task>>>::REALTIME;

/// The round-robin quantum of that tier, zero with none.
pub(crate) const RR_QUANTUM_NS: u64 = <Stack as StackTiers<Arc<Task>>>::RR_QUANTUM_NS;

/// A fresh node of the tier above fair for a task, `None` with no such tier
/// or no memory for one (R1): allocated outside every scheduler path, by the
/// policy calls and by a fork of a real-time task.
pub(crate) fn new_upper_node() -> Option<UpperSlot> {
    <Stack as StackTiers<Arc<Task>>>::new_upper_slot()
}

/// The stack's innermost entity is the fair class's (the code review of the
/// split, N2, restated for step 4b, F7): a fair task's record and run slot
/// go in as [`ferrix_sched_fair::FairEntity`] whatever tiers are above, and
/// come back out as it. `Task` keeps that entity as its record's atomics and
/// its run slot's cell (L.sched.63). A stack whose fair tier is not the
/// innermost stops the build here, where `Task`'s keeping of it has to
/// change with it.
const _: fn(ferrix_sched_fair::FairEntity<Arc<Task>>) -> StackEntity =
    <Stack as StackTiers<Arc<Task>>>::fair_entity;
const _: fn(StackEntity) -> ferrix_sched_fair::Split<UpperSlot, Arc<Task>> =
    <Stack as StackTiers<Arc<Task>>>::split;

/// With no tier above fair, the stack's entity is the fair class's alone,
/// as it was before step 4b, and a task's upper cell costs nothing.
const _: () = assert!(
    REALTIME
        || (size_of::<StackEntity>() == size_of::<ferrix_sched_fair::FairEntity<Arc<Task>>>()
            && size_of::<UpperCellOf>() == 0),
    "a fair-only stack's entity is the fair entity, and its upper cell is zero-sized"
);

/// What the boot line calls the class stack (SCHED-CLASSES.md S1): its
/// classes' names, top tier first.
pub(crate) const NAME: &str = <Stack as ferrix_sched_class::SchedClass<Arc<Task>>>::NAME;
