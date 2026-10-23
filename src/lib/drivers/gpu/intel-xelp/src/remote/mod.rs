//! The opaque crash mode's protocol (`docs/INTEL-GPU.md` §12): the GT's
//! driver split into a keeper, which holds everything a program holds and
//! is the only process the kernel sees as the driver, and a worker, which
//! drives the GT and may die without any program noticing.
//!
//! * [`wire`]: the records between them, several to a channel message;
//! * [`keeper`]: the front end's back end in the keeper, a journal of
//!   every live object, sent on to the worker and replayed into a new one;
//! * [`worker`]: the keeper's calls run on the GT's back end in the worker.
//!
//! Host-tested whole, the worker over the engine model (`tests`): a worker
//! killed with requests in flight, a new one given the journal, and the
//! GPU's view -- every binding's pages, every request's result -- the same
//! as a run in which nothing died.

pub mod keeper;
pub mod wire;
pub mod worker;

#[cfg(test)]
mod tests;

pub use keeper::{GiveUp, Journal, KeeperLink, RemoteBackend};
pub use wire::{Call, Event};
pub use worker::Worker;
