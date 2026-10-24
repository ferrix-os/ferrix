//! Verify-only LMS/HSS and the signed manifest of Ferrix's product signing
//! (`docs/SIGNING.md`).
//!
//! [`lms`] verifies LM-OTS, LMS and HSS signatures (RFC 8554) over
//! SHA-256/192 (RFC 9858) for the parameter sets SIGNING.md §2.1 allows.
//! [`manifest`] parses the 4 KiB signed manifest and checks what its
//! signed fields bind. The UEFI loader, init's configuration verifier and
//! `devmgr` are its users; the kernel never links it (SIGNING.md §5.2).
//!
//! The crate verifies and nothing else: it holds no key generation, no
//! signing and no private-key state, so no image that links it can sign.
//! It is `no_std`, has no `unsafe` and allocates nothing; every buffer is
//! one its caller lends.

#![no_std]
#![forbid(unsafe_code)]
#![deny(clippy::arithmetic_side_effects)]

pub mod lms;
pub mod manifest;

pub use manifest::{Anchor, Expect, Field, Kind, Refusal, Verified, verify};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vectors;
