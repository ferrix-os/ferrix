//! User mode: address spaces, memory objects, and the processes built on them.
//!
//! Stage 6 of `docs/ROADMAP.md`. Everything above this point runs in one
//! address space at the kernel's own privilege level; from here the kernel
//! builds address spaces it does not itself run in, and hands a processor to
//! code it does not trust.

pub(crate) mod alloc_check;
pub(crate) mod cache;
pub(crate) mod check;
pub(crate) mod edge_check;
pub(crate) mod madvise_check;
pub(crate) mod rmap_check;
pub(crate) mod space;
pub(crate) mod vmo;
