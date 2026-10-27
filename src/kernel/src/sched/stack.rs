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
//! **Still the fair class's own.** A few calls the scheduler makes are
//! EEVDF's alone and are made on the queue directly, because no second class
//! exists yet to say what they mean for it: a task's weight
//! (`RunQueue::set_weight`), the picture `/proc` and the checks read
//! (`for_each`, `avg_vruntime`, `current_id`, `queued`), the slice
//! (`set_slice_ns`, `config`, `should_preempt`), the processor's load
//! (`load_weight`), the stealing choice (`latest_where`), `yield_curr` and
//! `level`. They become operations of `SchedClass` with the second class
//! (step 4), each in its own review; until then a `Stack` other than the
//! fair class does not compile, which is the point.

use alloc::sync::Arc;

use super::task::Task;

/// The class stack, as the build flags compose it.
pub(crate) use ferrix_kernel_sched_select::Stack;

/// One CPU's queue of [`Stack`].
pub(crate) type StackQueue = <Stack as ferrix_sched_class::SchedClass<Arc<Task>>>::Queue;

/// What a task carries for [`Stack`] while it is off every queue: for the
/// fair class alone, its record and its run slot (`Task::entity_with`,
/// `Task::keep_entity`).
pub(crate) type StackEntity = <Stack as ferrix_sched_class::SchedClass<Arc<Task>>>::Entity;

/// The stack's entity is the fair class's (the code review of the split,
/// N2): `Task` keeps it as its record's atomics and its run slot's cell
/// (L.sched.63), and assembles it in `Task::entity_with`. A stack whose
/// entity is anything else stops the build here, where `Task`'s keeping of
/// it has to change with it.
const _: fn(StackEntity) -> ferrix_sched_fair::FairEntity<Arc<Task>> = |entity| entity;

/// What the boot line calls the class stack (SCHED-CLASSES.md S1): its
/// classes' names, top tier first.
pub(crate) const NAME: &str = <Stack as ferrix_sched_class::SchedClass<Arc<Task>>>::NAME;
