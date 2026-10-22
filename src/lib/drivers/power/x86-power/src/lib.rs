//! The x86-64 power driver's policy (`docs/POWERCTL.md` §8), apart from the
//! process so that host tests hold it.
//!
//! The kernel holds what makes a power-limit write safe whoever asks: the
//! model's ceilings, the lease, the backstop, the restores. Everything here
//! is what the driver decides inside those: the plan of steps the boot words
//! ask for ([`options`], [`plan`]), backing off before the kernel's backstop
//! has to ([`policy`]), and what the raw readings mean ([`decode`]). A
//! mistake here is bounded by the kernel; it never reaches a register the
//! kernel would refuse.

#![no_std]
#![forbid(unsafe_code)]

pub mod decode;
pub mod options;
pub mod plan;
pub mod policy;

#[cfg(test)]
mod tests;
