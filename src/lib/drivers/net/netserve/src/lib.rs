//! A ring-3 network driver's serve loop.
//!
//! Two libraries already know everything about their side. `ferrix-netring`'s
//! [`DriverSide`] takes the kernel's submissions and makes its completions;
//! `ferrix-virtio-net`'s driver turns a frame into descriptors and a used
//! entry into an event. What is left between them is a loop, and the loop is
//! where the two sides' rules meet.
//!
//! That loop is this crate, written against a [`Nic`] trait so it runs and is
//! tested on the host with a fake device on one side and the ring's own kernel
//! end on the other. The process that runs it (`src/user/system/native/drivers/net/virtio-net`) adds only the
//! handles.
//!
//! # The two directions are not symmetrical, and that is the whole design
//!
//! **Sending** is a copy and a submission: the kernel put the frame in a ring
//! slot, the loop copies it into the device's transmit region and hands it
//! over, and the slot is completed when the device says the frame went.
//!
//! **Receiving** cannot work that way. A frame arrives when it arrives, into a
//! buffer the *device* chose, and the kernel's ring slot is picked afterwards.
//! So the loop keeps the receive slots the kernel posted in a queue, and a
//! frame that arrives takes the oldest. The loop takes from the device only
//! as many arrived frames as it has slots for ([`Nic::drain`]'s
//! `receive_room`); the rest stay in the device's own receive ring, in order,
//! until the kernel posts more. The device's ring is then a second queue
//! behind the net ring's, rather than a buffer emptied into the floor.
//!
//! That was the other way round until 2026-10-22: a frame with no slot
//! waiting was taken, dropped and its buffer given back. With a sender
//! allowed more than the kernel's posted half ring in flight (any real TCP
//! sender once its window opens) about one frame in seventeen went that way
//! on `x86_64` under KVM, and a 32 MiB download fell from 43 MB/s to 6 to
//! 8 MB/s on the losses -- the N150 box's yt6801 download, 4.5 MB/s, was
//! the same shape.
//!
//! # What is never done
//!
//! * **A submission is never consumed that cannot be answered.**
//!   [`DriverSide::take`] advances the ring's head, so a submission taken is
//!   one the driver owes a completion for. The loop takes only as many as it
//!   has room to complete.
//! * **A device buffer is released as soon as its bytes are copied**, not when
//!   the kernel drains the completion. A receive buffer the driver holds is a
//!   frame the device cannot deliver, and the kernel's drain is not on the
//!   device's critical path.
//! * **Nothing is retried for ever.** A device that reports an error stops the
//!   loop so the process can reset it and leave; a ring that reports
//!   corruption does the same.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod tests;

use core::fmt;

use ferrix_netring::bell::Wait;
use ferrix_netring::driver::{Consumed, DriverError, DriverSide};
use ferrix_netring::layout::{MAX_ENTRIES, Op, Status, Submission};
use ferrix_netring::{Corruption, RingMemory};
use ferrix_virtio_net::{DeviceError, Event, Frame, SubmitError};

/// How many receive slots the loop remembers.
///
/// A ring cannot post more slots than it has entries, so this is
/// [`MAX_ENTRIES`] and not a number of its own: anything larger is an array
/// nothing can fill, on the stack of a process that has little else on it.
pub const MAX_POSTED: usize = MAX_ENTRIES as usize;

/// How many submissions or events one turn of the loop moves.
pub const BATCH: usize = 32;

/// The device, as the loop needs it: the driver behind a trait so a test can
/// stand a fake in its place.
pub trait Nic {
    /// Hand a frame to the transmit queue; see `Driver::submit`.
    ///
    /// # Errors
    ///
    /// [`SubmitError`], with [`SubmitError::QueueFull`] the one the loop
    /// waits out.
    fn submit(&mut self, frame: &Frame) -> Result<(), SubmitError>;

    /// Give a receive buffer back; see `Driver::release`.
    ///
    /// # Errors
    ///
    /// A buffer that was not held.
    fn release(&mut self, buffer: u16) -> Result<(), DeviceError>;

    /// Take what the device finished; see `Driver::on_interrupt`: every
    /// sent frame that fits in `out`, and at most `receive_room` arrived
    /// ones. Arrived frames past `receive_room` stay with the device, in
    /// order, for a later call: the loop has no slot to put them in, and a
    /// frame left in the device's ring is one a later slot still carries.
    ///
    /// # Errors
    ///
    /// [`DeviceError`], which means reset.
    fn drain(&mut self, out: &mut [Event], receive_room: usize) -> Result<usize, DeviceError>;

    /// Copy `frame` into the transmit region at `offset`.
    fn write_transmit(&mut self, offset: u64, frame: &[u8]);

    /// Copy the received frame at `offset` into `out`.
    fn read_receive(&self, offset: u64, out: &mut [u8]);

    /// Where the next frame to send may go, and how long it may be.
    fn transmit_slot(&self, index: u32) -> (u64, u32);
}

/// Why the loop stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The ring said something no honest kernel writes.
    Ring(Corruption),
    /// The device said something no honest device writes.
    Device(DeviceError),
    /// The completion ring had no room, which means more submissions were
    /// outstanding than it holds.
    Overcommitted,
}

impl fmt::Display for Fault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Ring(corruption) => write!(formatter, "the ring: {corruption}"),
            Fault::Device(_) => formatter.write_str("the device asked to be reset"),
            Fault::Overcommitted => {
                formatter.write_str("the completion ring had no room for what was owed")
            }
        }
    }
}

/// What one turn of the loop did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Turn {
    /// Frames handed to the device to send.
    pub sent: u32,
    /// Frames taken from the device and put in ring slots.
    pub received: u32,
    /// Frames the device delivered with no ring slot waiting.
    pub dropped: u32,
    /// Whether anything at all happened.
    pub busy: bool,
}

/// What the loop has done since it began.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counters {
    /// Frames sent.
    pub sent: u64,
    /// Frames received.
    pub received: u64,
    /// Frames dropped for want of a ring slot: none from a device that
    /// keeps `receive_room`, which [`Nic::drain`] asks of it.
    pub dropped: u64,
    /// Frames the device refused because its queue was full.
    pub deferred: u64,
}

/// The serve loop's state.
#[derive(Debug)]
pub struct Serve {
    /// Ring slots the kernel posted for the device to fill, oldest first.
    posted: [u32; MAX_POSTED],
    /// How many of those are in use.
    posted_len: usize,
    /// Frames taken off the ring that the device has not taken yet, oldest
    /// first.
    ///
    /// A queue rather than one frame, and the difference is a lost packet:
    /// [`DriverSide::take`] advances the ring's head for a whole batch at
    /// once, so every frame in a batch after the one the device refused is
    /// already owed a completion. Keeping one would drop the rest.
    pending: [(u32, u32); BATCH],
    /// How many of those there are.
    pending_len: usize,
    /// What has happened.
    counters: Counters,
}

impl Default for Serve {
    fn default() -> Serve {
        Serve::new()
    }
}

impl Serve {
    /// A loop that has done nothing.
    #[must_use]
    pub const fn new() -> Serve {
        Serve {
            posted: [0; MAX_POSTED],
            posted_len: 0,
            pending: [(0, 0); BATCH],
            pending_len: 0,
            counters: Counters {
                sent: 0,
                received: 0,
                dropped: 0,
                deferred: 0,
            },
        }
    }

    /// What has happened.
    #[must_use]
    pub const fn counters(&self) -> Counters {
        self.counters
    }

    /// How many receive slots are waiting for a frame.
    #[must_use]
    pub const fn posted(&self) -> usize {
        self.posted_len
    }

    /// One turn: take what the kernel submitted, take what the device
    /// finished, and answer both.
    ///
    /// # Errors
    ///
    /// [`Fault`], which is terminal: the caller resets the device and leaves.
    pub fn turn<R: RingMemory, D: RingMemory, N: Nic>(
        &mut self,
        ring: &mut R,
        data: &mut D,
        side: &mut DriverSide,
        nic: &mut N,
    ) -> Result<Turn, Fault> {
        let mut turn = Turn::default();
        self.retry_deferred(ring, data, side, nic, &mut turn)?;
        self.take_submissions(ring, data, side, nic, &mut turn)?;
        self.take_events(ring, data, side, nic, &mut turn)?;
        Ok(turn)
    }

    /// Whether a frame is waiting for room in the device's transmit queue.
    #[must_use]
    pub fn throttled(&self) -> bool {
        self.pending_len > 0
    }

    /// Before the caller sleeps on its port: whether it may, or must turn
    /// again first.
    ///
    /// This is in the library and not in the caller because the ring rings
    /// only a side that has asked to be rung, and a driver that sleeps
    /// without asking is never woken by the kernel at all. It still wakes on
    /// its device's interrupt, so it receives frames and answers none of
    /// them, which reads from outside as a working interface that transmits
    /// nothing. Leaving that to be remembered once per driver is leaving it
    /// to be forgotten.
    ///
    /// While a frame waits for the device's queue the answer is always to
    /// sleep, whatever the ring holds: the loop will not take another
    /// submission until that frame has gone, so only the device's interrupt
    /// can make progress, and asking the ring would answer `Pending` for ever
    /// and turn the wait into a spin.
    ///
    /// # Errors
    ///
    /// The [`Fault`] that stops the loop.
    pub fn before_sleep<R: RingMemory>(
        &mut self,
        ring: &mut R,
        side: &mut DriverSide,
    ) -> Result<Wait, Fault> {
        if self.throttled() {
            return Ok(Wait::Sleep);
        }
        side.prepare_to_sleep(ring).map_err(Fault::Ring)
    }

    /// On waking: stop asking to be rung, before anything is drained.
    pub fn woke<R: RingMemory>(&mut self, ring: &mut R, side: &mut DriverSide) {
        side.woke(ring);
    }

    /// Send the frames the device would not take last time, if it will now.
    fn retry_deferred<R: RingMemory, D: RingMemory, N: Nic>(
        &mut self,
        ring: &mut R,
        data: &mut D,
        side: &mut DriverSide,
        nic: &mut N,
        turn: &mut Turn,
    ) -> Result<(), Fault> {
        while self.pending_len > 0 {
            let (slot, length) = *self.pending.first().ok_or(Fault::Overcommitted)?;
            match self.send(ring, data, side, nic, slot, length) {
                Sent::Gone => {
                    self.pending.copy_within(1..self.pending_len, 0);
                    self.pending_len -= 1;
                    turn.sent += 1;
                    turn.busy = true;
                }
                Sent::Waiting => return Ok(()),
                Sent::Failed(fault) => return Err(fault),
            }
        }
        Ok(())
    }

    /// Remember a frame the device would not take.
    fn defer(&mut self, slot: u32, length: u32) {
        if let Some(place) = self.pending.get_mut(self.pending_len) {
            *place = (slot, length);
            self.pending_len += 1;
            self.counters.deferred += 1;
        }
    }

    /// Take submissions the kernel made.
    fn take_submissions<R: RingMemory, D: RingMemory, N: Nic>(
        &mut self,
        ring: &mut R,
        data: &mut D,
        side: &mut DriverSide,
        nic: &mut N,
        turn: &mut Turn,
    ) -> Result<(), Fault> {
        // Nothing is taken while a frame is waiting for the device: the next
        // submission could be another transmission, and taking it would mean
        // owing a completion for a frame with nowhere to put it.
        if self.pending_len > 0 {
            return Ok(());
        }
        let mut taken = [Submission {
            slot: 0,
            length: 0,
            op: Op::Receive,
        }; BATCH];
        let room = BATCH.min(MAX_POSTED - self.posted_len);
        if room == 0 {
            return Ok(());
        }
        let consumed: Consumed = side
            .take(ring, taken.get_mut(..room).unwrap_or_default())
            .map_err(Fault::Ring)?;
        for entry in taken.iter().take(consumed.taken) {
            turn.busy = true;
            match entry.op {
                Op::Receive => self.post(entry.slot),
                // Once one frame is waiting, every later one in this batch
                // waits behind it rather than being tried out of order: the
                // kernel's frames leave in the order it asked for them.
                Op::Transmit if self.pending_len > 0 => self.defer(entry.slot, entry.length),
                Op::Transmit => match self.send(ring, data, side, nic, entry.slot, entry.length) {
                    Sent::Gone => turn.sent += 1,
                    Sent::Waiting => self.defer(entry.slot, entry.length),
                    Sent::Failed(fault) => return Err(fault),
                },
            }
        }
        Ok(())
    }

    /// Remember a slot the device may fill.
    fn post(&mut self, slot: u32) {
        if let Some(place) = self.posted.get_mut(self.posted_len) {
            *place = slot;
            self.posted_len += 1;
        }
    }

    /// Take the oldest slot the device may fill.
    fn take_posted(&mut self) -> Option<u32> {
        if self.posted_len == 0 {
            return None;
        }
        let slot = *self.posted.first()?;
        self.posted.copy_within(1..self.posted_len, 0);
        self.posted_len -= 1;
        Some(slot)
    }

    /// Copy a frame out of its ring slot and hand it to the device.
    fn send<R: RingMemory, D: RingMemory, N: Nic>(
        &mut self,
        ring: &mut R,
        data: &mut D,
        side: &mut DriverSide,
        nic: &mut N,
        slot: u32,
        length: u32,
    ) -> Sent {
        let Ok(offset) = side.layout().slot_offset(slot) else {
            return Sent::Failed(Fault::Ring(Corruption::SlotOutOfRange));
        };
        let (device_offset, capacity) = nic.transmit_slot(slot);
        if length > capacity {
            // Longer than the device will carry: answer it rather than hold
            // it, so the kernel learns the frame did not go.
            return match side.complete(ring, slot, 0, Status::Failed) {
                Ok(()) => Sent::Gone,
                Err(error) => Sent::Failed(fault_of(error)),
            };
        }
        let mut frame = [0_u8; MAX_FRAME];
        let len = (length as usize).min(MAX_FRAME);
        for (at, byte) in frame.iter_mut().take(len).enumerate() {
            *byte = data.read_u8(offset + at);
        }
        nic.write_transmit(device_offset, frame.get(..len).unwrap_or_default());
        match nic.submit(&Frame {
            id: u64::from(slot),
            offset: device_offset,
            len: length,
        }) {
            Ok(()) => Sent::Gone,
            Err(SubmitError::QueueFull) => Sent::Waiting,
            Err(_) => Sent::Failed(Fault::Device(DeviceError::NeedsReset)),
        }
    }

    /// Take what the device finished.
    fn take_events<R: RingMemory, D: RingMemory, N: Nic>(
        &mut self,
        ring: &mut R,
        data: &mut D,
        side: &mut DriverSide,
        nic: &mut N,
        turn: &mut Turn,
    ) -> Result<(), Fault> {
        let mut events = [Event::Sent { id: 0 }; BATCH];
        // No more arrived frames than there are slots to put them in: the
        // rest wait in the device's ring for the kernel's next post.
        let count = nic
            .drain(&mut events, self.posted_len)
            .map_err(Fault::Device)?;
        for event in events.iter().take(count) {
            turn.busy = true;
            match *event {
                Event::Sent { id } => {
                    let slot = u32::try_from(id).unwrap_or(u32::MAX);
                    side.complete(ring, slot, 0, Status::Ok).map_err(fault_of)?;
                    self.counters.sent += 1;
                }
                Event::Received {
                    buffer,
                    offset,
                    len,
                } => {
                    self.deliver(
                        ring,
                        data,
                        side,
                        nic,
                        Arrived {
                            buffer,
                            offset,
                            len,
                        },
                        turn,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Put a frame the device delivered into a ring slot, and give the
    /// device's buffer back either way.
    fn deliver<R: RingMemory, D: RingMemory, N: Nic>(
        &mut self,
        ring: &mut R,
        data: &mut D,
        side: &mut DriverSide,
        nic: &mut N,
        arrived: Arrived,
        turn: &mut Turn,
    ) -> Result<(), Fault> {
        let Arrived {
            buffer,
            offset,
            len,
        } = arrived;
        let slot = self.take_posted();
        if let Some(slot) = slot {
            let mut frame = [0_u8; MAX_FRAME];
            let length = (len as usize).min(MAX_FRAME);
            nic.read_receive(offset, frame.get_mut(..length).unwrap_or_default());
            let Ok(at) = side.layout().slot_offset(slot) else {
                return Err(Fault::Ring(Corruption::SlotOutOfRange));
            };
            for (index, byte) in frame.iter().take(length).enumerate() {
                data.write_u8(at + index, *byte);
            }
            side.complete(ring, slot, u32::try_from(length).unwrap_or(0), Status::Ok)
                .map_err(fault_of)?;
            self.counters.received += 1;
            turn.received += 1;
        } else {
            self.counters.dropped += 1;
            turn.dropped += 1;
        }
        // The buffer goes back whether or not the frame found a slot: a
        // receive buffer the driver holds is a frame the device cannot
        // deliver.
        nic.release(buffer).map_err(Fault::Device)
    }
}

/// The largest frame the loop copies through its stack buffer.
///
/// A jumbo frame and then some; a slot larger than this is a slot no
/// interface's MTU reaches, because HELLO refuses an MTU a slot cannot hold.
pub const MAX_FRAME: usize = 2048;

/// A frame the device delivered, as the loop passes it along.
#[derive(Clone, Copy, Debug)]
struct Arrived {
    /// The device buffer it came in.
    buffer: u16,
    /// Where it starts in the receive region.
    offset: u64,
    /// How long it is.
    len: u32,
}

/// What a send did.
enum Sent {
    /// The device took it.
    Gone,
    /// The device's queue is full; try again after an interrupt.
    Waiting,
    /// Something is broken.
    Failed(Fault),
}

/// Which fault a completion's refusal is.
fn fault_of(error: DriverError) -> Fault {
    match error {
        DriverError::Full => Fault::Overcommitted,
        DriverError::Corrupt(corruption) => Fault::Ring(corruption),
        DriverError::SlotOutOfRange => Fault::Ring(Corruption::SlotOutOfRange),
        DriverError::LengthTooLarge => Fault::Ring(Corruption::LengthTooLarge),
    }
}
