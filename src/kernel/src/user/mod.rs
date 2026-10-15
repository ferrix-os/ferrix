//! User mode: address spaces, memory objects, and the processes built on them.
//!
//! Stage 6 of `docs/ROADMAP.md`. Everything above this point runs in one
//! address space at the kernel's own privilege level; from here the kernel
//! builds address spaces it does not itself run in, and hands a processor to
//! code it does not trust.

pub mod alloc_check;
pub mod cache;
pub mod check;
pub(crate) mod edge_check;
pub mod memory_type;
pub mod space;
pub mod vmo;
