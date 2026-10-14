//! The net ring: the memory the kernel shares with a ring-3 network driver,
//! and the rules each side keeps over it.
//!
//! `docs/ARCHITECTURE.md` §7 runs drivers in user processes and says the data
//! path to them *"is not per-request IPC. Driver and kernel share a descriptor
//! ring in a VMO and ring a doorbell; requests batch."* `src/lib/proto/blkring` is that
//! ring for block devices; this is the one for network devices, and
//! `docs/NET-RING.md` is its specification.
//!
//! # Why it is not `src/lib/proto/blkring` with a different entry
//!
//! A block request carries anything from a sector to a megabyte, so the block
//! ring has an allocator: the kernel picks a `data_offset` for every
//! submission and must not reuse a region before its completion. A frame is
//! bounded by the interface's MTU, so this ring has none. The data VMO is
//! `entries` slots of `slot_bytes`, slot *n* lives at `n * slot_bytes`, and a
//! submission names its slot. That removes the whole region-allocation half of
//! the protocol and with it the class of bug where a region is reused early --
//! which on an untranslated IOMMU domain is a device writing into somebody
//! else's packet.
//!
//! The index discipline below -- private indices, checked reads of the peer's,
//! the want-bell handshake -- is the same discipline `src/lib/proto/blkring` keeps, and
//! this is a second implementation of it rather than a shared one. That is a
//! deliberate debt with a reason: extracting it would refactor a subsystem
//! that is shipped, fuzzed and on the boot path, in the same landing as a new
//! one. `docs/BACKLOG.md` carries the row.
//!
//! # Objects
//!
//! | Object | Created by | The other side holds it with | Purpose |
//! |---|---|---|---|
//! | Control channel | devmgr or the kernel | one endpoint each | setup, the device's description, shutdown |
//! | Ring VMO | driver | kernel: exactly `READ \| WRITE \| MAP \| TRANSFER` | header and both entry arrays |
//! | Data VMO | driver, pinned | kernel: exactly `READ \| WRITE \| MAP \| TRANSFER` | the slots, copied through |
//! | Driver port | driver | kernel: exactly `WRITE \| TRANSFER` | submission doorbell |
//! | Kernel completion port | kernel | driver: exactly `WRITE` | completion doorbell |
//!
//! Rights at handoff are exact, for `src/lib/proto/blkring`'s reason: a `DUPLICATE` on
//! a VMO would let the kernel's handle be copied.
//!
//! # The ring VMO
//!
//! Little-endian on every architecture, no padding, offsets in [`layout`] and
//! asserted by the tests:
//!
//! ```text
//! 0   magic "FXNR"     4   version 1        6   flags, 0 in v1
//! 8   entries          12  slot_bytes
//! 16  sub_offset       20  comp_offset
//! 24  sub_tail         written by the kernel
//! 28  sub_head         written by the driver
//! 32  comp_tail        written by the driver
//! 36  comp_head        written by the kernel
//! 40  sub_want_bell    written by the driver
//! 44  comp_want_bell   written by the kernel
//! 48  reserved, zero, to 64
//!
//! at sub_offset:  entries x 16-byte submissions
//!                 slot 0, length 4, op 8, flags 9, reserved 10
//! at comp_offset: entries x 16-byte completions
//!                 slot 0, length 4, status 8, reserved 12
//! ```
//!
//! `entries` is a power of two in `2..=4096`, and `slot_bytes` a multiple of
//! 64 in `64..=65536`. The driver writes the first six fields before HELLO;
//! the kernel reads them once, in [`KernelSide::attach`], and keeps its own
//! copies. Every other field has the one writer marked. Indices are
//! free-running `u32`s, and the slot for an index is `index & (entries - 1)`.
//!
//! # Trust
//!
//! Neither side trusts the other's writes.
//!
//! * **Private indices.** Each side keeps its own head, tail and want-bell
//!   flag in its own memory and only ever *writes* them to the ring. A peer
//!   scribbling on them moves nothing.
//! * **Every read of a peer's index is checked.** A tail more than `entries`
//!   ahead of this side's head, or an index behind the last value this side
//!   accepted, is [`Corruption`].
//! * **Every entry is checked when it is read.** A slot number past `entries`,
//!   a length past `slot_bytes`, an operation the protocol does not have, a
//!   completion for a slot nobody submitted: each is [`Corruption`], and
//!   corruption is terminal for the side that sees it.
//!
//! # What is not here
//!
//! Ports, mappings, the pinned pages and the device. Those are the kernel's
//! glue and the driver process's business; this crate is the bytes and the
//! arithmetic, so that `cargo test`, Miri and a fuzzer can reach all of it.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

pub mod bell;
pub mod control;
pub mod driver;
pub mod kernel;
pub mod layout;
mod ring;

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

pub use bell::{BELL_COMPLETE, BELL_SUBMIT, Doorbell, Rung, Wait};
pub use control::{Hello, Interface, InterfaceFlags, Message, MessageError, Refusal, Start};
pub use driver::{Consumed, DriverError, DriverSide};
pub use kernel::{AttachError, Completed, Drain, KernelSide, SubmitError};
pub use layout::{HeaderError, Op, RingLayout, Status, Submission};

/// The ring VMO, as bytes.
///
/// **The `u16` and `u32` accessors are not provided, and over memory the
/// peer shares each must be one access.** The ring's indices -- both tails,
/// both heads, the want-bell words -- are `u32`s one side writes while the
/// other reads them. Composed from bytes, a tail stepping from `0x00ff` to
/// `0x0100` is read as `0x01ff`, or written through `0x0000`, and the other
/// side reports [`Corruption::TailOverrun`] or [`Corruption::TailBackwards`]
/// and ends the ring. A torn read is not a safety problem -- every value read
/// is checked as if the peer chose it -- but it ends a healthy ring. These
/// used to be provided from the byte accessors, and both the virtio-net
/// driver's ring and the kernel's inherited them: under a long download the
/// driver stopped on a "corrupt" ring about every 35 minutes, and when the
/// kernel was the side that saw it, the interface was parked under a driver
/// that never noticed (`docs/BACKLOG.md`; `ferrix-blkring` had the same bug
/// first). Memory only one side touches at a time, as a test's is, may
/// compose them from bytes.
///
/// An implementation that gives only the byte accessors does not compile:
///
/// ```compile_fail,E0046
/// struct Bytes;
/// impl ferrix_netring::RingMemory for Bytes {
///     fn read_u8(&self, _offset: usize) -> u8 {
///         0
///     }
///     fn write_u8(&mut self, _offset: usize, _value: u8) {}
///     fn barrier(&self) {}
/// }
/// ```
///
/// # Why this trait is safe
///
/// Nothing here hands an address to anyone: the ring is reached only through
/// these methods, and an implementation that reads garbage or drops writes is
/// indistinguishable from a hostile peer, which both sides already survive. So
/// the crate stays `forbid(unsafe_code)`.
///
/// # The barrier
///
/// [`RingMemory::barrier`] carries the one obligation that matters for
/// correctness. The want-bell handshake is a store on one processor followed
/// by a load, racing a store and a load on another, and it avoids a lost
/// wake-up only if each side's load sees the other side's earlier store. The
/// barrier must be a real ordering fence. An empty barrier is correct only
/// where both sides are stepped one after the other, as in the tests.
pub trait RingMemory {
    /// Read one byte at `offset`.
    fn read_u8(&self, offset: usize) -> u8;

    /// Write one byte at `offset`.
    fn write_u8(&mut self, offset: usize, value: u8);

    /// Order every access issued before this call against every access issued
    /// after it, as seen from the other side.
    fn barrier(&self);

    /// Read the little-endian `u16` at `offset`, in one access where it is
    /// aligned.
    fn read_u16(&self, offset: usize) -> u16;

    /// Read the little-endian `u32` at `offset`, in one access where it is
    /// aligned.
    fn read_u32(&self, offset: usize) -> u32;

    /// Write the little-endian `u16` at `offset`, in one access where it is
    /// aligned.
    fn write_u16(&mut self, offset: usize, value: u16);

    /// Write the little-endian `u32` at `offset`, in one access where it is
    /// aligned.
    fn write_u32(&mut self, offset: usize, value: u32);
}

/// Shared memory said something no honest peer writes.
///
/// Terminal for the side that sees it: it latches and reports the same
/// corruption from every later call. The kernel treats the driver as dead and
/// takes its interface down; a driver resets its device and stops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Corruption {
    /// The producer's tail is more than `entries` ahead of this side's head.
    TailOverrun,
    /// The producer's tail is behind the value this side last accepted.
    TailBackwards,
    /// The consumer's head is behind the value this side last accepted, or
    /// ahead of the tail this side published.
    HeadOutOfRange,
    /// An entry named a slot at or past `entries`.
    SlotOutOfRange,
    /// An entry's length is larger than a slot holds.
    LengthTooLarge,
    /// A submission's operation is not one the protocol has.
    UnknownOp,
    /// A completion's status is not one the protocol defines.
    UnknownStatus,
    /// A completion named a slot that is not outstanding: never submitted, or
    /// already completed.
    UnknownSlot,
    /// The completion ring has no room for what the driver holds, which means
    /// more than `entries` submissions were outstanding.
    Overcommitted,
}

impl fmt::Display for Corruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Corruption::TailOverrun => "the peer's tail ran more than a ring ahead",
            Corruption::TailBackwards => "the peer's tail moved backwards",
            Corruption::HeadOutOfRange => "the peer's head moved backwards or past the tail",
            Corruption::SlotOutOfRange => "an entry named a slot the ring does not have",
            Corruption::LengthTooLarge => "an entry's length is larger than a slot holds",
            Corruption::UnknownOp => "a submission's operation is unknown",
            Corruption::UnknownStatus => "a completion's status is unknown",
            Corruption::UnknownSlot => "a completion named a slot that is not outstanding",
            Corruption::Overcommitted => "more submissions were outstanding than the ring holds",
        })
    }
}
