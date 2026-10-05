//! The block ring: the memory the kernel shares with a ring-3 block driver,
//! and the rules each side keeps over it.
//!
//! `docs/ARCHITECTURE.md` §7 runs drivers in user processes and says the data
//! path to them *"is not per-request IPC. Driver and kernel share a descriptor
//! ring in a VMO and ring a doorbell; requests batch."* This crate is that ring
//! for block devices. It is written ahead of both of its users — the kernel's
//! ring glue in front of `src/lib/fs/block`'s queue, and the virtio-blk driver
//! process — so that both link one implementation of the layout and of the
//! validation, and so that `cargo test`, Miri and a fuzzer can reach it.
//!
//! Nothing here maps a VMO, queues a packet or waits. The memory is behind
//! [`RingMemory`]; a doorbell is a value, [`Doorbell`], that the caller rings;
//! a control message is bytes the caller sends. Which system calls do that is
//! the glue's business.
//!
//! # Objects
//!
//! | Object | Created by | The other side holds it with | Purpose |
//! |---|---|---|---|
//! | Control channel | devmgr or the kernel | one endpoint each | setup, geometry, shutdown |
//! | Ring VMO | driver | kernel: exactly `READ \| WRITE \| MAP \| TRANSFER` | header and both entry arrays |
//! | Data VMO | driver, pinned | kernel: exactly `READ \| WRITE \| MAP \| TRANSFER` | payload, copied through |
//! | Driver port | driver | kernel: exactly `WRITE \| TRANSFER` | submission doorbell |
//! | Kernel completion port | kernel | driver: exactly `WRITE` | completion doorbell |
//!
//! Rights at handoff are exact. The kernel refuses a HELLO whose handles carry
//! more rights than these — a `DUPLICATE` on a VMO would let the kernel's
//! handle be copied — or fewer. A handle a process sends carries `TRANSFER`,
//! because only a transferable handle can be sent and a transfer keeps its
//! rights; the completion port the kernel sends back does not, since the
//! kernel places it in the driver's table itself.
//! [`control::Hello::validate`] decides it from the rights the glue read off
//! the handles.
//!
//! The kernel allocates regions of the data VMO: it picks each submission's
//! `data_offset`, copies write payloads in before publishing, copies read
//! payloads out once at completion, and never reuses a region before that
//! region's completion. The driver only checks that a region lies inside it.
//!
//! # The ring VMO
//!
//! Little-endian on every architecture, no padding, offsets in [`layout`] and
//! asserted by the tests:
//!
//! ```text
//! 0   magic "FXBR"     4   version 1        6   flags, 0 in v1
//! 8   entries          12  sub_offset       16  comp_offset
//! 20  sub_tail         written by the kernel
//! 24  sub_head         written by the driver
//! 28  comp_tail        written by the driver
//! 32  comp_head        written by the kernel
//! 36  sub_want_bell    written by the driver
//! 40  comp_want_bell   written by the kernel
//! 44  reserved, zero, to 64
//!
//! at sub_offset:  entries x 32-byte submissions
//!                 id 0, sector 8, data_offset 16, count 24, op 28, flags 29
//! at comp_offset: entries x 24-byte completions
//!                 id 0, bytes_done 8, status 16
//! ```
//!
//! `entries` is a power of two in `2..=4096` and the same for both rings. The
//! driver writes the first six fields before HELLO; the kernel reads them once,
//! in [`KernelSide::attach`], and keeps its own copies. Every other field has
//! the one writer marked. Indices are free-running `u32`s, and the slot for an
//! index is `index & (entries - 1)`.
//!
//! # Trust
//!
//! Neither side trusts the other's writes, and every rule follows from that.
//!
//! * **Private indices.** Each side keeps its own head, tail and want-bell flag
//!   in its own memory and only ever *writes* them to the ring. It never reads
//!   them back, so a peer scribbling on them moves nothing. The tests count
//!   such reads, and the fuzz target fails on one.
//! * **Every read of a peer's index is checked.** A tail more than `entries`
//!   ahead of this side's private head, or a peer index behind the last value
//!   this side accepted, is [`Corruption`].
//! * **Every entry is copied once, then checked.** The driver refuses a
//!   submission with an unknown op, set reserved or unknown flag bits, FUA
//!   where it was not announced or on anything but a write, a bad count, a
//!   range past the device or a payload outside the data VMO, by completing it
//!   `REFUSED`, so the device never sees it. The kernel treats a completion that
//!   names an id not outstanding (never submitted, or already completed), an
//!   unknown status, or more bytes than its submission carried as
//!   [`Corruption`]. A write or flush that reports OK with fewer bytes than it
//!   carried is an I/O error.
//! * **Corruption is terminal.** The side that sees it latches, and every later
//!   call reports it. The kernel fails everything outstanding with EIO, which
//!   [`KernelSide::end`] enumerates, and treats the driver as dead. A driver
//!   resets its device and stops.
//!
//! One rule the protocol needs and does not otherwise state is kept here,
//! because a wake-up depends on it: **the kernel never has more than `entries`
//! submissions outstanding.** Every completion waiting in the ring and every
//! request the driver holds is outstanding, so under that rule the completion
//! ring always has room for what the driver holds. The driver therefore never
//! waits for room, and there is no "room again" doorbell for it to wait on.
//! [`KernelSide`] holds itself to the rule, and [`DriverSide`] reports a
//! completion ring without room for what it holds as
//! [`Corruption::Overcommitted`].
//!
//! # Doorbells
//!
//! A doorbell is a `PACKET_USER` port packet: key [`BELL_SUBMIT`] on the
//! driver's port, key [`BELL_COMPLETE`] on the kernel's completion port, and
//! the tail at ring time in its first data word — a hint, since the receiver
//! always reads the ring. Bells are coalesced by a want-bell flag that only the
//! ring's consumer writes.
//!
//! * The consumer, before sleeping, writes `want_bell = 1` and then re-reads
//!   the producer's tail. If anything is pending it writes `want_bell = 0` and
//!   processes instead of sleeping ([`KernelSide::prepare_to_sleep`],
//!   [`DriverSide::prepare_to_sleep`]). On waking it writes `want_bell = 0`,
//!   then drains.
//! * The producer, after writing a new tail, reads `want_bell` and rings if it
//!   is 1. [`KernelSide::publish`] and [`DriverSide::publish`] return the
//!   [`Doorbell`] to ring, if any, and do no I/O of their own.
//!
//! Both sides write before they read, so one of them always sees the other.
//! [`RingMemory::barrier`] is what makes "before" hold across processors.
//!
//! Ports are bounded, so a bell can be refused. A `port_queue` that answers
//! Full has *rung* — a bell is already waiting — and is not an error;
//! [`bell::port_queue_rung`] and [`bell::rung`] say so for either side. A
//! hostile producer can queue as many bells as it likes, and since any number
//! of bells counts as one, that only delays its own work.
//!
//! # Control and shutdown
//!
//! On the control channel ([`control`]), the driver sends HELLO with the
//! device's geometry and its three handles. The kernel answers READY with its
//! completion port, or REFUSED with a [`Refusal`]: a version mismatch,
//! `queues != 1`, a ring header [`KernelSide::attach`] rejects, or handles with
//! the wrong rights.
//!
//! HELLO also says which disk it is: its PCI location, its `GET_ID` serial, and
//! the node name devmgr chose, `vd` and one to three lowercase letters. The
//! kernel refuses a malformed name itself ([`Refusal::Name`]), and its glue
//! refuses a name already published or a location already served
//! ([`Refusal::NameInUse`], [`Refusal::LocationInUse`]), which only it can
//! know. The kernel, not the driver, chooses the device numbers — a driver that
//! picked its own minor could collide with another's — so the minor is
//! [`identity::disk_index`] × 16, as Linux numbers virtio disks ([`identity`]),
//! which needs minors of at least 19 bits; the kernel's are 20.
//!
//! The kernel checks a HELLO in a fixed order and sends the first refusal:
//! version ([`Refusal::Version`], 1), queues (2), rights (4), the device
//! description (6), the name (7), and last the registry — a name already
//! published (8) or a location already served (9). A malformed message is 5,
//! and a ring header [`KernelSide::attach`] rejects is 3.
//!
//! STOP asks the driver to stop taking submissions, finish or fail what it
//! holds, reset its device and answer STOPPED. Only then does the kernel unmap
//! the VMOs and fail whatever is still outstanding. A driver that dies is an
//! implicit STOPPED *without* the promise that its device was reset.
//!
//! However a ring ends, every outstanding request fails with EIO at once, and
//! the data VMO's pinned pages are **held until devmgr confirms the device
//! reset**. On an untranslated domain, which is every domain today, a device
//! that was not reset can still write to frames after they are unpinned, and
//! not even an orderly STOPPED proves the reset — it is the untrusted driver's
//! word. So when a driver ends or its job is killed, devmgr resets the device
//! before any pinned frame is freed. Until it confirms, the pages stay held,
//! leaked by design, and the kernel logs it; if devmgr cannot reset the device
//! they are never freed. [`KernelSide`] models this as a state:
//! [`KernelSide::end`] fails the outstanding requests and leaves the data VMO
//! [`kernel::DataVmo::HeldUntilReset`], and only [`KernelSide::confirm_reset`]
//! makes it [`kernel::DataVmo::Releasable`].
//!
//! # What is not here
//!
//! Ports, mappings, the data VMO's allocator and the rule about pinned pages
//! belong to the kernel glue, and the device belongs to the driver process. The
//! arithmetic the driver needs to announce `max_sectors` is here
//! ([`geometry::max_sectors`], [`geometry::descriptors_for`]), so the driver
//! and `src/lib/drivers/block/virtio-blk` compute it one way.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

pub mod bell;
pub mod control;
pub mod driver;
pub mod geometry;
pub mod identity;
pub mod kernel;
pub mod layout;
mod ring;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

pub use bell::{BELL_COMPLETE, BELL_SUBMIT, Doorbell, Rung, Wait};
pub use control::{Accepted, Block, Hello, Message, MessageError, Refusal, Start, StartError};
pub use driver::{CompleteError, Consumed, DriverSide};
pub use geometry::{Device, DeviceError, DeviceFlags, InvalidSubmission};
pub use identity::{DiskName, Identity, Location, disk_index};
pub use kernel::{AttachError, Completed, Drain, KernelSide, Slot, SubmitError};
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
/// is checked as if the peer chose it -- but it ends a healthy ring: these
/// used to be provided from the byte accessors, `ferrix-driver`'s block ring
/// inherited them, and a disk's requests failed after 30 s about once in
/// 2,300 boots under load (`docs/BACKLOG.md`, the seam's deeper run). Only
/// [`RingMemory::read_u64`] and [`RingMemory::write_u64`] are provided, since
/// no 64-bit field is changed by one side while the other reads it. Memory
/// only one side touches at a time, as a test's is, may compose them from
/// bytes.
///
/// An implementation that gives only the byte accessors does not compile:
///
/// ```compile_fail,E0046
/// struct Bytes;
/// impl ferrix_blkring::RingMemory for Bytes {
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
/// `src/lib/drivers/virtio`'s `QueueMemory` is an `unsafe trait` because a virtqueue's
/// memory is handed to a device by address, so an implementation that lied
/// about where the memory is would let the device write anywhere. Nothing here
/// hands an address to anyone. The ring is reached only through these methods,
/// and an implementation that reads garbage or drops writes is
/// indistinguishable from a hostile peer, which both sides already survive. No
/// breach of this trait's contract is undefined behaviour, so the crate stays
/// `forbid(unsafe_code)`.
///
/// Every offset this crate passes lies below the ring size the side was built
/// with, which [`RingLayout`] checked. An implementation over a raw mapping
/// should bound offsets anyway; reading zero and discarding writes outside the
/// ring is the model to follow.
///
/// # The barrier
///
/// [`RingMemory::barrier`] carries the one obligation that matters for
/// correctness. The want-bell handshake is a store on one processor followed by
/// a load, racing a store and a load on another, and it only avoids a lost
/// wake-up if each side's load sees the other side's earlier store. A store
/// buffer can hide that store from the other processor for a while, so the
/// barrier must be a real ordering fence: an `mfence` on x86-64, a `dsb sy`
/// where one is needed, whatever the platform requires. An empty barrier is
/// correct only where both sides are stepped one after the other, as in the
/// tests.
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

    /// Read a little-endian `u64` at `offset`.
    fn read_u64(&self, offset: usize) -> u64 {
        u64::from(self.read_u32(offset)) | (u64::from(self.read_u32(offset.wrapping_add(4))) << 32)
    }

    /// Write the little-endian `u16` at `offset`, in one access where it is
    /// aligned.
    fn write_u16(&mut self, offset: usize, value: u16);

    /// Write the little-endian `u32` at `offset`, in one access where it is
    /// aligned.
    fn write_u32(&mut self, offset: usize, value: u32);

    /// Write a little-endian `u64` at `offset`.
    fn write_u64(&mut self, offset: usize, value: u64) {
        self.write_u32(offset, value as u32);
        self.write_u32(offset.wrapping_add(4), (value >> 32) as u32);
    }
}

/// Shared memory said something no honest peer writes.
///
/// Detected by either side, and terminal for it: the side latches and reports
/// the same corruption from every later call. What to do next depends on who
/// saw it. The kernel fails every outstanding request with EIO
/// ([`KernelSide::end`]) and treats the driver as dead; a driver resets its
/// device and stops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Corruption {
    /// The producer's tail is more than `entries` ahead of this side's head.
    TailOverrun,
    /// The producer's tail is behind the value this side last accepted.
    TailBackwards,
    /// The consumer's head is behind the value this side last accepted, or
    /// ahead of the tail this side published.
    HeadOutOfRange,
    /// A completion named an id that is not outstanding: never submitted, or
    /// already completed.
    UnknownId,
    /// A completion's status is not one the protocol defines.
    UnknownStatus,
    /// A completion reported more bytes than its submission carried.
    BytesDoneTooLarge,
    /// The completion ring has no room for what the driver holds, which means
    /// the kernel had more than `entries` submissions outstanding.
    Overcommitted,
}

impl fmt::Display for Corruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Corruption::TailOverrun => "the peer's tail ran more than a ring ahead",
            Corruption::TailBackwards => "the peer's tail moved backwards",
            Corruption::HeadOutOfRange => "the peer's head moved backwards or past the tail",
            Corruption::UnknownId => "a completion named an id that is not outstanding",
            Corruption::UnknownStatus => "a completion's status is unknown",
            Corruption::BytesDoneTooLarge => "a completion reported more bytes than it carried",
            Corruption::Overcommitted => "the kernel had more submissions outstanding than entries",
        })
    }
}
