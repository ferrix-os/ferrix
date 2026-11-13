//! Packages: the format an app is built into and a root is installed from
//! (`docs/APPS.md` §6).
//!
//! An app's folder holds an `app.toml`; building it makes a package, a newc
//! archive of its files and its record; an image is packages installed into
//! a root. This crate is everything in that chain that is a function of
//! text: the manifest, the record, and the plan that says whether a set of
//! packages may go into one root, and in which order. xtask builds images
//! with it on the host, and the package manager will be the same code on
//! Ferrix, so the two cannot disagree about what a package is.
//!
//! # What is here
//!
//! * [`toml`] -- the subset of TOML a manifest and a record are written in.
//! * [`manifest`] -- an `app.toml`: the package, how it is built, how it is
//!   judged.
//! * [`record`] -- what an installed package leaves in
//!   `lib/ferrix/packages/`.
//! * [`plan`] -- whether a set of packages installs, and in what order.
//! * [`index`] -- a repository's index: every version of every package it
//!   has, and which of them are security fixes (`docs/UPDATES.md`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod index;
pub mod manifest;
pub mod plan;
pub mod record;
pub mod toml;

#[cfg(test)]
mod tests;
