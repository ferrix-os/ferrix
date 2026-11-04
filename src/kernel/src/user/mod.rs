//! User mode: address spaces, memory objects, and the processes built on them.
//!
//! Stage 6 of `docs/ROADMAP.md`. Everything above this point runs in one
//! address space at the kernel's own privilege level; from here the kernel
//! builds address spaces it does not itself run in, and hands a processor to
//! code it does not trust.

pub mod address_wait;
pub mod cache;
#[cfg(not(ferrix_load_built = "lift"))]
pub mod check;
#[cfg(not(ferrix_load_built = "lift"))]
pub mod dirty_check;
#[cfg(not(ferrix_load_built = "lift"))]
pub(crate) mod edge_check;
pub mod memory_type;
#[cfg(not(ferrix_load_built = "lift"))]
pub mod rehome_cache_check;
#[cfg(not(ferrix_load_built = "lift"))]
pub mod rehome_check;
pub mod space;
pub mod vmo;
