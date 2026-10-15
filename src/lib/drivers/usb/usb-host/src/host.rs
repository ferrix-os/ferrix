//! What the bus asks of a host controller, whichever kind it is.
//!
//! [`crate::bus::Bus`] enumerates devices, walks hubs and reads reports the
//! same way behind every controller; what differs is who schedules a
//! transfer and who gives a device its address. EHCI leaves both to
//! software: the bus sends `SET_ADDRESS` itself, and a transfer is a queue
//! head software links into a schedule. An xHCI controller does both itself:
//! it sends `SET_ADDRESS` when software issues an Address Device command, and
//! a transfer is a ring of requests it runs for an endpoint it was told
//! about. So the trait has a step for each place the two part ways, and each
//! controller does nothing in the steps the other needs:
//!
//! * [`Host::open`] and [`Host::assign`]: a device in the default state
//!   becomes one the bus can address. xHCI gives it a slot and an address in
//!   `open`, and `assign` only corrects the control endpoint's packet size;
//!   EHCI does nothing in `open`, and `assign` sends `SET_ADDRESS`.
//! * [`Host::set_hub`]: xHCI routes through a hub only once told it is one.
//! * [`Host::release`]: xHCI gives the slot back.
//!
//! The rest -- root ports, control transfers, interrupt IN pipes -- is what
//! [`crate::ehci::Controller`] always offered.

use crate::Clock;
use crate::usb::{Setup, Speed};

/// The largest control data stage.
pub const CONTROL_BYTES: usize = 1024;
/// The largest packet an interrupt pipe reads.
pub const PIPE_BYTES: usize = 64;

/// Why the controller would not come up, or a transfer failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The memory is too small, or a page of it has no address the
    /// controller can reach.
    Memory,
    /// The controller would not halt, reset or start, or stopped answering.
    Unresponsive,
    /// A schedule would not switch on or off.
    Schedule,
    /// The device stalled the request.
    Stall,
    /// A transaction failed three times: a device gone, or a bad cable.
    Transaction,
    /// The device sent more than the buffer held.
    Babble,
    /// The controller could not reach memory in time.
    Buffer,
    /// The transfer did not finish in time.
    Timeout,
    /// No pipe is free.
    NoPipe,
    /// The request was larger than the buffer.
    TooLong,
    /// The controller has no device slot left, or would not give one.
    NoSlot,
    /// The controller refused a command, with this completion code.
    Command(u8),
    /// The controller cannot do this at all.
    Unsupported,
}

/// Where a request goes: the device and how the controller reaches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Target {
    /// The device's address.
    pub address: u8,
    /// Its speed.
    pub speed: Speed,
    /// The largest packet its control endpoint takes.
    pub max_packet: u16,
    /// For a full- or low-speed device behind a high-speed hub, that hub --
    /// by [`Target::handle`] -- and the port on it the device's path goes
    /// through.
    pub translator: Option<(u8, u8)>,
    /// The device slot an xHCI controller gave it; zero behind EHCI.
    pub slot: u8,
    /// The hub ports between the root port and the device, four bits each,
    /// the nearest the root lowest: USB 3's route string.
    pub route: u32,
    /// The root port its path starts at, counting from zero.
    pub root_port: u32,
}

impl Target {
    /// What the controller knows the device by: its slot behind xHCI, its
    /// address behind EHCI.
    #[must_use]
    pub const fn handle(&self) -> u8 {
        if self.slot == 0 {
            self.address
        } else {
            self.slot
        }
    }
}

/// Where a device in the default state is, before it has an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Place {
    /// The root port its path starts at, counting from zero.
    pub root_port: u32,
    /// The hub ports below the root port, as [`Target::route`].
    pub route: u32,
    /// Its speed.
    pub speed: Speed,
    /// As [`Target::translator`].
    pub translator: Option<(u8, u8)>,
}

/// An endpoint a pipe is opened to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Endpoint {
    /// Its number, without the direction bit.
    pub number: u8,
    /// The largest packet it sends.
    pub max_packet: u16,
    /// `bInterval`, as its descriptor gives it.
    pub interval: u8,
}

/// A data stage.
#[derive(Debug)]
pub enum Data<'a> {
    /// None.
    None,
    /// Device to host, into the buffer.
    In(&'a mut [u8]),
    /// Host to device, from the buffer.
    Out(&'a [u8]),
}

impl Data<'_> {
    /// How many bytes the stage may move.
    #[must_use]
    pub const fn len(&self) -> usize {
        match self {
            Data::None => 0,
            Data::In(buffer) => buffer.len(),
            Data::Out(buffer) => buffer.len(),
        }
    }

    /// Whether there is no data stage, or an empty one.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A root port, as its status register says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RootPort {
    /// A device is there.
    pub connected: bool,
    /// The connection changed since this was last cleared.
    pub changed: bool,
    /// The port is enabled.
    pub enabled: bool,
}

/// What a root port reset found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootReset {
    /// A device of this speed, its port enabled.
    Ready(Speed),
    /// A full- or low-speed device, which EHCI hands to its companion
    /// controller: nothing this driver drives.
    Handed,
    /// Nothing is there any more.
    Gone,
    /// Something is there, and its port would not enable.
    Failed,
}

/// A packet an interrupt pipe received.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Packet {
    /// The pipe.
    pub pipe: usize,
    /// The bytes, of which `len` were received.
    pub bytes: [u8; PIPE_BYTES],
    /// How many.
    pub len: usize,
}

/// A transfer event on an interrupt pipe that was not a plain success.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Trouble {
    /// The device's slot.
    pub slot: u8,
    /// The endpoint's context index.
    pub endpoint: u8,
    /// The completion code.
    pub code: u8,
    /// Whether the event's TRB pointer was one of the pipe's requests.
    pub known: bool,
}

/// A host controller, running.
pub trait Host {
    /// The clock it waits on.
    type Clock: Clock;
    /// What it was built from, and gives back when it stops.
    type Parts;

    /// The clock the controller waits on.
    fn clock(&mut self) -> &mut Self::Clock;

    /// Stop the controller and take its parts back.
    ///
    /// # Errors
    ///
    /// The parts, when the controller would not halt: it may still write
    /// the memory, which must then be kept.
    fn shutdown(self) -> Result<Self::Parts, Self::Parts>
    where
        Self: Sized;

    /// How many root ports there are.
    fn ports(&self) -> u32;

    /// Root port `port`, counting from zero.
    fn root_port(&self, port: u32) -> RootPort;

    /// Clear a root port's change bits.
    fn clear_root_changes(&mut self, port: u32);

    /// Reset root port `port` and say what is there.
    fn reset_root_port(&mut self, port: u32) -> RootReset;

    /// Whether a root port's status changed since this was last asked: a
    /// controller that says so itself spares the wait for the next poll.
    fn ports_changed(&mut self) -> bool {
        false
    }

    /// The oldest transfer event on an interrupt pipe that was not a plain
    /// success, not yet taken: for the console, since real controllers say
    /// things a model does not.
    fn take_trouble(&mut self) -> Option<Trouble> {
        None
    }

    /// Make the device in the default state at `place` one that control
    /// transfers reach: its target, with the packet size every device of its
    /// speed takes.
    ///
    /// # Errors
    ///
    /// A controller with no slot left, or a device that would not take its
    /// address.
    fn open(&mut self, place: &Place) -> Result<Target, Error>;

    /// Finish what [`Host::open`] began, once the device's descriptor has
    /// said its control endpoint's `max_packet`: the device answers at an
    /// address of its own -- `address`, unless the controller chose one --
    /// and `target` says how to reach it from now on.
    ///
    /// # Errors
    ///
    /// A device that would not take the address, or a controller that would
    /// not take the packet size.
    fn assign(&mut self, target: &mut Target, address: u8, max_packet: u16) -> Result<(), Error>;

    /// Tell the controller the device is a hub of `ports` ports, whose
    /// transaction translator needs `think_time` (the hub descriptor's two
    /// bits) between transactions.
    ///
    /// # Errors
    ///
    /// A controller that refused.
    fn set_hub(&mut self, target: &Target, ports: u8, think_time: u8) -> Result<(), Error> {
        let _ = (target, ports, think_time);
        Ok(())
    }

    /// Let go of a device [`Host::open`] made, which is gone or could not be
    /// set up.
    fn release(&mut self, target: &Target) {
        let _ = target;
    }

    /// Run one control transfer to completion: how many bytes the data stage
    /// moved.
    ///
    /// # Errors
    ///
    /// What the transfer ended in, or [`Error::TooLong`] for a data stage
    /// past [`CONTROL_BYTES`].
    fn control(&mut self, target: &Target, setup: &Setup, data: Data<'_>) -> Result<usize, Error>;

    /// Open an interrupt IN pipe to `endpoint` of `target`: its index.
    ///
    /// # Errors
    ///
    /// [`Error::NoPipe`] when every pipe is open, [`Error::TooLong`] for a
    /// packet past [`PIPE_BYTES`].
    fn open_pipe(&mut self, target: &Target, endpoint: &Endpoint) -> Result<usize, Error>;

    /// Close pipe `pipe`, and wait until the controller can no longer be
    /// reading it.
    fn close_pipe(&mut self, pipe: usize);

    /// Whether pipe `pipe` has halted for good.
    fn pipe_stopped(&self, pipe: usize) -> bool;

    /// Acknowledge the controller's interrupt and collect every packet its
    /// pipes finished, in order, handing each to `receive`.
    ///
    /// # Errors
    ///
    /// [`Error::Unresponsive`] when the controller reported an error of its
    /// own and stopped.
    fn service(&mut self, receive: impl FnMut(Packet)) -> Result<(), Error>;

    /// Open the bulk endpoints `inward` and `outward` of `target`, for
    /// [`Host::bulk`]: the pair's index.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] from a controller that moves no bulk data.
    fn open_bulk(
        &mut self,
        target: &Target,
        inward: &Endpoint,
        outward: &Endpoint,
    ) -> Result<usize, Error> {
        let _ = (target, inward, outward);
        Err(Error::Unsupported)
    }

    /// Move `data` over bulk pair `pair` -- IN from its inward endpoint, OUT
    /// to its outward one -- and wait for it: how many bytes moved.
    ///
    /// # Errors
    ///
    /// What the transfer ended in. After [`Error::Stall`] the endpoint is
    /// ready again, its halt cleared on the device too.
    fn bulk(&mut self, pair: usize, data: Data<'_>) -> Result<usize, Error> {
        let _ = (pair, data);
        Err(Error::Unsupported)
    }

    /// Close bulk pair `pair`.
    fn close_bulk(&mut self, pair: usize) {
        let _ = pair;
    }
}
