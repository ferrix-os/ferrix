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
