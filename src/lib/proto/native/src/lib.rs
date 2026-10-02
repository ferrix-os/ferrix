//! Typed, safe wrappers over every Ferrix native system call.
//!
//! `src/lib/proto/native-abi` writes the native ABI down as numbers and layouts; this
//! crate is how a program *speaks* it. Each call is a function or a method on
//! the handle type it acts on, taking slices and typed flags rather than
//! pointers and words, and returning [`Error`] rather than `-errno`:
//!
//! ```text
//! let (left, right) = channel::create(sys)?;
//! left.write(b"ping")?;
//! let got = right.read(&mut buffer, &mut [])?;
//! ```
//!
//! Handles are owned: [`OwnedHandle`] and every object type built on it close
//! the handle when dropped, and a handle sent through a channel or given to a
//! new process is given up only when the kernel has taken it.
//!
//! # Written against a trait, not an instruction
//!
//! Nothing here traps. Every wrapper builds a [`Raw`] — number, six argument
//! registers, and the memory its pointer arguments name — and passes it to a
//! [`Syscall`]. `src/user/system/native/rt`, the runtime a native program links, implements that
//! trait with the architecture's trap instruction. The tests implement it with
//! a recorder that plays the kernel's part, so every wrapper's number, argument
//! order, pointer layout and error decoding is checked on the host and under
//! Miri. That split is also why this crate can sit in `src/lib/`: it is the part
//! of the runtime that is a pure function of its arguments.
//!
//! No `unsafe`: pointer arguments are addresses of borrowed slices, and the one
//! claim a real trap has to rest on — that a [`Raw`] names only memory it
//! borrows — is made by [`call`], where a `Raw` is built.
//!
//! # Not yet on main
//!
//! [`pending`] wraps process creation and `vmo_map`, whose handlers are not
//! written; they answer [`Error::Unsupported`] until they are.

#![no_std]
#![forbid(unsafe_code)]

pub mod call;
pub mod channel;
pub mod device;
pub mod error;
pub mod handle;
pub mod job;
pub mod pending;
pub mod pin;
pub mod port;
pub mod vmo;
pub mod window;

#[cfg(test)]
mod tests;

pub use call::{Raw, Syscall};
pub use error::Error;
pub use ferrix_native_abi::handle::Handle;
pub use ferrix_native_abi::rights::{Requested, Rights};
pub use ferrix_native_abi::signals::Signals;
pub use ferrix_native_abi::types::{IoMappingSpec, PortPacket};
pub use handle::{Deadline, Object, OwnedHandle};
