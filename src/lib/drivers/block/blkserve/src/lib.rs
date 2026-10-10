//! A ring-3 block driver's serve loop.
//!
//! Two libraries already know everything about their side of a block driver:
//! `ferrix-blkring`'s [`DriverSide`] consumes the kernel's submissions and
//! produces its completions, and `ferrix-virtio-blk`'s [`Driver`] turns a
//! request into descriptor chains and a used entry into a completion. What is
//! left between them is a loop — take a submission, hand it to the device,
//! take what the device finished, hand it back, ring the bell, sleep — and
//! the loop is where the two sides' rules meet: a submission the device
//! refuses is completed on the ring with the status that says why; a device
//! queue that is full holds the submissions that did not fit, in order, until
//! an interrupt makes room; nothing is consumed that cannot be held; and a
//! side that reports corruption or a broken device stops the loop so the glue
//! can reset and leave.
//!
//! That loop is this crate, written against the two libraries' types and a
//! [`Disk`] trait the real driver implements, so that it runs and is tested on
//! the host with a fake device on one side and the ring's own kernel side on
//! the other. The process that runs it (`src/user/system/native/drivers/block/virtio-blk`) adds only the handles: the
//! mappings the two sides' memory lives in, the port both bells and the
//! interrupt arrive on, and the control channel STOP comes over.
//!
//! # Units
//!
//! The ring counts sectors in the disk's logical block size, announced in
//! HELLO; virtio counts them in 512-byte sectors whatever the block size. A
//! submission's `sector` and `count` are scaled by `block_size / 512` on the
//! way in, and a completion's `bytes` come back as they are.
//!
//! # Holding what the queue cannot take
//!
//! [`DriverSide::consume`] advances the ring's head, so a submission consumed
//! is one the driver holds until it completes it; there is no putting it
//! back. When the device's queue has no room for the chains a request needs,
//! the request waits here, first in first out, and the loop stops consuming:
//! the rest stays on the ring, where the kernel bounds it by the ring's
//! entries. So the pending store never needs more than `entries` places, and
//! while it holds anything the loop sleeps on the interrupt only, never on the
//! ring's bell, since a bell could not help.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use core::fmt;

use ferrix_blkring::bell::{Doorbell, Wait};
use ferrix_blkring::driver::{CompleteError, Consumed, DriverSide};
use ferrix_blkring::layout::{Op as RingOp, Status as RingStatus, Submission};
use ferrix_blkring::{Corruption, RingMemory};
use ferrix_virtio::QueueMemory;
use ferrix_virtio_blk::{
    Accepted, Completion, DeviceError, DevicePages, Drained, Driver, Op, Request, RequestArea,
    Slot, Status, SubmitError, Transport,
};

/// A virtio sector is this many bytes, whatever the disk's block size.
const VIRTIO_SECTOR: u64 = 512;

/// The device, as the loop needs it: [`Driver`] behind a trait so a test can
/// stand a fake in its place.
pub trait Disk {
    /// Submit a request; see [`Driver::submit`].
    ///
    /// # Errors
    ///
    /// [`SubmitError`], with [`SubmitError::QueueFull`] the one the loop
    /// retries after an interrupt.
    fn submit(&mut self, request: &Request) -> Result<Accepted, SubmitError>;

    /// Take what the device has completed; see [`Driver::on_interrupt`].
    ///
    /// # Errors
    ///
    /// [`DeviceError`] once the device broke the protocol.
    fn drain(&mut self, out: &mut [Completion]) -> Result<Drained, DeviceError>;

    /// Submit a write that must be durable when it completes: the ring's
    /// force unit access, which only a device that announced
    /// `DeviceFlags::FUA` is sent. One that did not is never asked, so the
    /// default, a plain [`Disk::submit`], is never reached with a promise
    /// to keep.
    ///
    /// # Errors
    ///
    /// As [`Disk::submit`].
    fn submit_fua(&mut self, request: &Request) -> Result<Accepted, SubmitError> {
        self.submit(request)
    }

    /// Whether the device holds requests whose completion is waited for;
    /// see [`Serve::device_busy`].
    fn busy(&self) -> bool {
        false
    }

    /// Whether [`Disk::drain`] has completions no interrupt will announce:
    /// ones [`Disk::check`] found waiting, or made by failing or finishing
    /// requests itself. The glue then drains after its watch without
    /// waiting for an interrupt; see [`Serve::wants_drain`].
    fn wants_drain(&self) -> bool {
        false
    }

    /// Ask the device whether it needs a reset, whatever the last interrupt
    /// looked like; see [`Driver::check_needs_reset`].
    ///
    /// # Errors
    ///
    /// [`DeviceError`] once the device broke the protocol or needs a reset.
    fn check(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }
}

impl<T, R, A, D, S> Disk for Driver<T, R, A, D, S>
where
    T: Transport,
    R: QueueMemory + DevicePages,
    A: RequestArea,
    D: DevicePages,
    S: AsRef<[Slot]> + AsMut<[Slot]>,
{
    fn submit(&mut self, request: &Request) -> Result<Accepted, SubmitError> {
        Driver::submit(self, request)
    }

    fn drain(&mut self, out: &mut [Completion]) -> Result<Drained, DeviceError> {
        Driver::on_interrupt(self, out)
    }

    fn busy(&self) -> bool {
        Driver::requests_in_flight(self) > 0
    }

    fn check(&mut self) -> Result<(), DeviceError> {
        Driver::check_needs_reset(self)
    }
}

/// Why the loop stopped. After any of these the glue resets the device,
/// completes what the reset abandoned, and exits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The ring is corrupt, as [`DriverSide`] found it.
    Ring(Corruption),
    /// The device broke the protocol, as [`Driver`] found it.
    Device(DeviceError),
    /// The ring's side says it holds no request for a completion the device
    /// produced, or the pending store is full: a disagreement between the
    /// two sides that only a bug in this loop explains.
    Protocol,
}

impl fmt::Display for Fault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Ring(corruption) => write!(formatter, "ring corrupt: {corruption}"),
            Fault::Device(error) => write!(formatter, "device broke the protocol: {error:?}"),
            Fault::Protocol => formatter.write_str("the two sides disagree about what is held"),
        }
    }
}

/// Submissions consumed off the ring that the device's queue had no room
/// for, oldest first. `N` places, of which the loop never needs more than
/// the ring has entries.
struct Pending<const N: usize> {
    slots: [Option<Submission>; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Pending<N> {
    const fn new() -> Self {
        Self {
            slots: [None; N],
            head: 0,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn front(&self) -> Option<Submission> {
        if self.len == 0 {
            return None;
        }
        self.slots.get(self.head).copied().flatten()
    }

    fn pop(&mut self) {
        if self.len == 0 {
            return;
        }
        if let Some(slot) = self.slots.get_mut(self.head) {
            *slot = None;
        }
        self.head = (self.head + 1) % N.max(1);
        self.len -= 1;
    }

    fn push(&mut self, submission: Submission) -> Result<(), Fault> {
        if self.len >= N {
            return Err(Fault::Protocol);
        }
        let at = (self.head + self.len) % N.max(1);
        match self.slots.get_mut(at) {
            Some(slot) => *slot = Some(submission),
            None => return Err(Fault::Protocol),
        }
        self.len += 1;
        Ok(())
    }
}

/// What one submission became at the device.
enum Handed {
    /// Queued, or completed at once and written to the ring.
    Taken,
    /// The queue had no room; the submission waits.
    NoRoom,
}

/// The loop's state: the ring's driver side, the device, and what waits
/// between them.
pub struct Serve<M, K, const PENDING: usize> {
    ring: DriverSide<M>,
    disk: K,
    pending: Pending<PENDING>,
    /// Virtio sectors per ring block: the block size over 512.
    sectors_per_block: u64,
    fault: Option<Fault>,
    /// The processor's free-running counter, when the glue can read one:
    /// what times each request's stay in the device, for the seam's
    /// measurement (`docs/OPAQUE-KERNEL.md`, S0). `None` measures nothing.
    clock: Option<fn() -> u64>,
    /// When each request queued in the device was handed to it, by id.
    started: [Option<(u64, u64)>; PENDING],
}

impl<M, K, const PENDING: usize> fmt::Debug for Serve<M, K, PENDING> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Serve")
            .field("ring", &self.ring)
            .field("pending", &self.pending.len)
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

impl<M: RingMemory, K: Disk, const PENDING: usize> Serve<M, K, PENDING> {
    /// A loop over `ring`, whose device geometry says the block size, and
    /// `disk`. `PENDING` must be at least the ring's entries, which the glue
    /// chooses; the loop checks and reports [`Fault::Protocol`] otherwise.
    #[must_use]
    pub fn new(ring: DriverSide<M>, disk: K) -> Self {
        let block_size = u64::from(ring.device().block_size());
        let fault =
            (u64::from(ring.layout().entries()) > PENDING as u64).then_some(Fault::Protocol);
        Self {
            ring,
            disk,
            pending: Pending::new(),
            sectors_per_block: (block_size / VIRTIO_SECTOR).max(1),
            fault,
            clock: None,
            started: [None; PENDING],
        }
    }

    /// Time each request's stay in the device by `clock`, the processor's
    /// free-running counter, and say it in the completion's `device_ticks`.
    #[must_use]
    pub fn timed(mut self, clock: fn() -> u64) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Note that request `id` went to the device now.
    fn start(&mut self, id: u64) {
        let Some(clock) = self.clock else {
            return;
        };
        if let Some(slot) = self.started.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some((id, clock()));
        }
    }

    /// The ticks request `id` spent in the device, zero if it was not timed.
    fn took(&mut self, id: u64) -> u32 {
        let Some(clock) = self.clock else {
            return 0;
        };
        let Some(slot) = self
            .started
            .iter_mut()
            .find(|slot| slot.is_some_and(|(started, _)| started == id))
        else {
            return 0;
        };
        let Some((_, at)) = slot.take() else {
            return 0;
        };
        // At least one: zero means "not measured".
        u32::try_from(clock().saturating_sub(at))
            .unwrap_or(u32::MAX)
            .max(1)
    }

    /// The ring's side, for HELLO and for a look at what it holds.
    #[must_use]
    pub const fn ring(&self) -> &DriverSide<M> {
        &self.ring
    }

    /// The fault that stopped the loop, if one has.
    #[must_use]
    pub const fn fault(&self) -> Option<Fault> {
        self.fault
    }

    /// Whether a submission waits for room in the device's queue.
    #[must_use]
    pub fn throttled(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The kernel rang, or the loop is starting: consume everything pending
    /// on the ring that the device can take, and publish what completed at
    /// once. The doorbell, if any, is the glue's to ring.
    ///
    /// # Errors
    ///
    /// The [`Fault`] that stops the loop.
    pub fn on_bell(&mut self) -> Result<Option<Doorbell>, Fault> {
        self.checked()?;
        self.ring.woke();
        self.pump()?;
        Ok(self.ring.publish())
    }

    /// The device interrupted: take its completions onto the ring, hand it
    /// what waited for room, and publish. `out` is the glue's scratch for
    /// completions, at least one long.
    ///
    /// # Errors
    ///
    /// The [`Fault`] that stops the loop.
    pub fn on_interrupt(&mut self, out: &mut [Completion]) -> Result<Option<Doorbell>, Fault> {
        self.checked()?;
        loop {
            let drained = self.disk.drain(out).map_err(Fault::Device);
            let drained = self.fail(drained)?;
            for completion in out.iter().take(drained.completions) {
                let ticks = self.took(completion.id);
                self.complete_timed(
                    completion.id,
                    status(completion.status),
                    completion.bytes,
                    ticks,
                )?;
            }
            if !drained.more || drained.completions == 0 {
                break;
            }
        }
        self.pump()?;
        Ok(self.ring.publish())
    }

    /// Before the glue sleeps on its port: whether it may, or must consume
    /// first. While a submission waits for the device's queue, the answer is
    /// always to sleep, since only an interrupt can make room; the ring is
    /// not asked to ring, and the kernel's bells, if it does, are coalesced
    /// and harmless.
    ///
    /// # Errors
    ///
    /// The [`Fault`] that stops the loop.
    pub fn before_sleep(&mut self) -> Result<Wait, Fault> {
        self.checked()?;
        if self.throttled() {
            return Ok(Wait::Sleep);
        }
        let wait = self.ring.prepare_to_sleep().map_err(Fault::Ring);
        self.fail(wait)
    }

    /// Whether the device holds requests the ring waits for: a glue that
    /// sleeps then wakes after a while all the same and calls
    /// [`Serve::on_watch`], so a device that went quiet is not waited for
    /// for good.
    #[must_use]
    pub fn device_busy(&self) -> bool {
        self.disk.busy()
    }

    /// The glue waited for the device a while, with requests in flight, and
    /// heard nothing: ask it whether it needs a reset. An interrupt that
    /// brought completions does not ask (`Driver::on_interrupt`), so a reset
    /// announced in one would otherwise leave the ring's requests waiting
    /// for good.
    ///
    /// # Errors
    ///
    /// The [`Fault`] that stops the loop.
    pub fn on_watch(&mut self) -> Result<(), Fault> {
        self.checked()?;
        let checked = self.disk.check().map_err(Fault::Device);
        self.fail(checked)
    }

    /// Whether the device has completions to hand over that no interrupt
    /// will announce: the glue calls [`Serve::on_interrupt`] after a watch
    /// that left this set.
    #[must_use]
    pub fn wants_drain(&self) -> bool {
        self.disk.wants_drain()
    }

    /// Take the loop apart, for the glue to shut the device down and finish
    /// the ring: the requests the device abandons on reset are completed
    /// `IoError` on the ring side by the glue, one `complete` each.
    #[must_use]
    pub fn into_parts(self) -> (DriverSide<M>, K) {
        (self.ring, self.disk)
    }

    /// Hand the device everything held and everything consumable, in order:
    /// what waited first, then the ring, until the queue is full or the ring
    /// is empty.
    fn pump(&mut self) -> Result<(), Fault> {
        while let Some(waiting) = self.pending.front() {
            match self.hand(&waiting)? {
                Handed::Taken => self.pending.pop(),
                Handed::NoRoom => return Ok(()),
            }
        }
        loop {
            let consumed = self.ring.consume().map_err(Fault::Ring);
            let consumed = self.fail(consumed)?;
            match consumed {
                None => return Ok(()),
                Some(Consumed::Refused { .. }) => {}
                Some(Consumed::Request(submission)) => {
                    if let Handed::NoRoom = self.hand(&submission)? {
                        self.pending.push(submission)?;
                        return Ok(());
                    }
                }
            }
        }
    }

    /// One submission to the device. A request the device completes at once
    /// or refuses is completed on the ring here; a broken device stops the
    /// loop.
    fn hand(&mut self, submission: &Submission) -> Result<Handed, Fault> {
        let request = self.request(submission);
        let submitted = if submission.fua {
            self.disk.submit_fua(&request)
        } else {
            self.disk.submit(&request)
        };
        match submitted {
            Ok(Accepted::Queued { .. }) => {
                self.start(submission.id);
                Ok(Handed::Taken)
            }
            Ok(Accepted::Completed(done)) => {
                self.complete(done.id, status(done.status), done.bytes)?;
                Ok(Handed::Taken)
            }
            Err(SubmitError::QueueFull) => Ok(Handed::NoRoom),
            Err(SubmitError::ReadOnly) => {
                self.complete(submission.id, RingStatus::ReadOnly, 0)?;
                Ok(Handed::Taken)
            }
            Err(SubmitError::TooLarge | SubmitError::Unsplittable) => {
                self.complete(submission.id, RingStatus::Unsupported, 0)?;
                Ok(Handed::Taken)
            }
            Err(
                SubmitError::Empty
                | SubmitError::NotAligned
                | SubmitError::OutOfRange
                | SubmitError::OutsideData
                | SubmitError::BadAddress,
            ) => {
                self.complete(submission.id, RingStatus::Refused, 0)?;
                Ok(Handed::Taken)
            }
            Err(SubmitError::Broken) => Err(self.stop(Fault::Device(DeviceError::Broken))),
            Err(SubmitError::Device(error)) => Err(self.stop(Fault::Device(error))),
        }
    }

    /// A ring submission as the device takes it: sectors scaled to virtio's.
    fn request(&self, submission: &Submission) -> Request {
        let op = match submission.op {
            RingOp::Read => Op::Read,
            RingOp::Write => Op::Write,
            RingOp::Flush => Op::Flush,
        };
        let count = u64::from(submission.count).saturating_mul(self.sectors_per_block);
        Request {
            id: submission.id,
            op,
            sector: submission.sector.saturating_mul(self.sectors_per_block),
            count: u32::try_from(count).unwrap_or(u32::MAX),
            data_offset: submission.data_offset,
        }
    }

    fn complete(&mut self, id: u64, status: RingStatus, bytes: u64) -> Result<(), Fault> {
        self.complete_timed(id, status, bytes, 0)
    }

    fn complete_timed(
        &mut self,
        id: u64,
        status: RingStatus,
        bytes: u64,
        ticks: u32,
    ) -> Result<(), Fault> {
        match self.ring.complete_timed(id, status, bytes, ticks) {
            Ok(()) => Ok(()),
            Err(CompleteError::Corrupt(corruption)) => Err(self.stop(Fault::Ring(corruption))),
            Err(CompleteError::NotHeld) => Err(self.stop(Fault::Protocol)),
        }
    }

    fn checked(&self) -> Result<(), Fault> {
        self.fault.map_or(Ok(()), Err)
    }

    fn fail<T>(&mut self, result: Result<T, Fault>) -> Result<T, Fault> {
        if let Err(fault) = &result {
            let _ = self.fault.get_or_insert(*fault);
        }
        result
    }

    fn stop(&mut self, fault: Fault) -> Fault {
        *self.fault.get_or_insert(fault)
    }
}

/// A device's status as the ring reports it.
fn status(status: Status) -> RingStatus {
    match status {
        Status::Ok => RingStatus::Ok,
        Status::IoError => RingStatus::IoError,
        Status::Unsupported => RingStatus::Unsupported,
    }
}
