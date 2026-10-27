//! The scheduling-class stack this kernel is built with
//! (`docs/SCHED-CLASSES.md` §5).
//!
//! Every operation the scheduler makes on a CPU's classes goes through
//! [`Stack`], named here once as a type, so the compiler monomorphises and
//! inlines it: the choice costs nothing at run time. Today's kernel has one
//! class, the fair one, so `Stack` is [`ferrix_sched_fair::Eevdf`] and its
//! queue is EEVDF's `RunQueue`. When the per-tier build flags arrive (step
//! 3), this choice moves into the selection crate below the core, chosen by
//! them (S2); until then there is no flag to choose by, and nothing here
//! reads one.
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

/// The class stack: the fair class alone.
pub(crate) type Stack = ferrix_sched_fair::Eevdf;

/// One CPU's queue of [`Stack`].
pub(crate) type StackQueue = <Stack as ferrix_sched_class::SchedClass<Arc<Task>>>::Queue;
