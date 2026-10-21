//! The Ferrix native system call ABI: the half of the interface that is ours.
//!
//! `docs/ARCHITECTURE.md` §2 fixes two ABIs side by side. The Linux one is a
//! compatibility obligation and `src/lib/proto/linux-abi` writes it down. This crate is
//! the other one — capability handles to typed kernel objects, numbered from
//! `0x1000` — and it is where the design opinions live, so each of them is
//! argued where it is decided rather than in a document that can drift.
//!
//! Like `src/lib/proto/linux-abi`, nothing here executes. It is numbers, flag words and
//! `repr(C)` layouts, shared by the kernel's dispatcher and by anything that
//! calls it, so it is `no_std`, forbids `unsafe` and cannot panic.
//!
//! # One table, not three
//!
//! Linux numbers the same call differently on each architecture, and the
//! kernel folds three tables onto one enum. The native ABI has no history to
//! inherit, so it has one table, the same on every architecture: a driver's
//! source says `CHANNEL_WRITE` and the number is the same everywhere it is
//! built. The range `0x1000..=0x1FFF` is clear of every Linux number on all
//! three targets — the highest is ARMv7-A's private `0x0F0000` block, above
//! it, and the generic tables end below `0x200` — and the tests hold that
//! against `src/lib/proto/linux-abi` rather than against this comment.
//!
//! # No argument wider than a register
//!
//! ARMv7-A's argument registers are 32 bits. Linux answered that by splitting
//! 64-bit values across two registers, differently per call, which is why that
//! architecture has sixteen calls twice. The native ABI never passes a value
//! that might not fit: counts, sizes and addresses are pointer-width because
//! they describe this process's own memory, and anything that must be 64 bits
//! everywhere — a port key, a deadline, a VMO offset — travels through a
//! pointer to a `u64`. One convention, and no call that means something
//! different on a 32-bit machine.
//!
//! # Errors are `errno`
//!
//! A native call returns in the same register, with the same `-4095..=-1`
//! convention, as a Linux one. [`status`] names the native failures and maps
//! each to a distinct `errno`. The alternative, a second error space, would
//! make every musl program that also speaks this ABI translate at
//! every call site, and would make musl's own `syscall()` wrapper report a
//! native failure as a nonsense `errno`.

#![no_std]
#![forbid(unsafe_code)]

pub mod bootstrap;
pub mod directory;
pub mod exception;
pub mod handle;
pub mod nr;
pub mod rights;
pub mod signals;
pub mod status;
pub mod types;

#[cfg(test)]
mod tests;
