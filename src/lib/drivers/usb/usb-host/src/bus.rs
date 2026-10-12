//! The bus: the devices on it, found by polling the root ports and every
//! hub's ports, set up as chapter 9 says, and each keyboard or mouse boot
//! interface turned into an input function the driver introduces to the
//! kernel's input core.
//!
//! # Hot plugging by polling
//!
//! Every [`POLL_INTERVAL`] the driver calls [`Bus::poll`], which reads each
//! root port's `PORTSC` and asks each hub for each port's status. A port
//! whose connection changed loses what was behind it; a connected port with
//! nothing behind it is debounced, reset and enumerated. A hub's own status
//! change endpoint would say the same thing sooner, at the price of a pipe
//! per hub; a keyboard found a quarter of a second after it is plugged in is
//! found soon enough.
//!
//! A port whose device could not be set up is left alone until it is
//! unplugged, rather than reset four times a second.
//!
//! # What the driver reads
//!
//! [`Bus::next_output`] hands over, in order, each function as it appears,
//! each report as the events it makes, and each function as it goes. A
//! function's index is the driver's to key its input channel by; it is not
//! reused until its [`Output::Detached`] has been read.

use ferrix_inputctl::message::{Hello, RawEvent, Text};

use crate::ehci;
use crate::hid::{BOOT_KEYBOARD, BOOT_MOUSE, EventBuf, Identity, Interpreter, Kind};
use crate::host::{
    self, CONTROL_BYTES, Data, Endpoint, Host, PIPE_BYTES, Place, RootReset, Target,
};
use crate::hub::{
    self, CHANGE_CONNECTION, CHANGE_RESET, HubDescriptor, PORT_POWER, PORT_RESET, PortStatus,
    STATUS_ENABLE,
};
use crate::report::Descriptor;
use crate::storage::{self, Disk};
use crate::usb::{
    CLASS_HID, CLASS_HUB, CONFIGURATION, Configuration, DEVICE, DeviceDescriptor, Interface,
    LANGUAGE_US, PROTOCOL_KEYBOARD, PROTOCOL_MOUSE, STRING, SUBCLASS_BOOT, Setup, Speed,
    first_language, is_control_packet, string_ascii,
};
use crate::{Clock, Dma, MILLISECOND, Parts, Registers};

/// The largest packet a `SuperSpeed` control endpoint takes, which its device
/// descriptor gives as the exponent 9.
const SUPER_MAX_PACKET: u16 = 512;

/// How often the ports are polled.
pub const POLL_INTERVAL: u64 = 250 * MILLISECOND;
/// The most devices, hubs included.
pub const MAX_DEVICES: usize = 16;
/// The most keyboards and mice at once: the kernel's
/// `USB_INPUT_FUNCTIONS`, the input channels one host may hold.
pub const MAX_FUNCTIONS: usize = 8;

/// The most disks at once.
pub const MAX_DISKS: usize = 2;

/// How long a connection must stay before it is reset: USB 2.0's
/// `TATTDB`.
const DEBOUNCE: u64 = 100 * MILLISECOND;
/// How long a hub may take to finish a port reset.
const HUB_RESET_PATIENCE: u64 = 500 * MILLISECOND;
/// How often a hub port reset is looked at.
const HUB_RESET_STEP: u64 = 10 * MILLISECOND;
/// The rest a device gets after its port reset: `TRSTRCY`.
const RESET_RECOVERY: u64 = 10 * MILLISECOND;
/// The configuration descriptor bytes read.
const CONFIGURATION_BYTES: usize = 256;
/// The longest string descriptor.
const STRING_BYTES: usize = 255;
/// The longest string kept, in ASCII.
const NAME_BYTES: usize = 64;
/// Pending outputs held at once; a report past them is dropped.
const PENDING: usize = 64;
/// Notes held at once; older ones are dropped.
const NOTES: usize = 16;

/// Why a device could not be set up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A transfer failed.
    Transfer(host::Error),
    /// A descriptor made no sense.
    Descriptor,
    /// Every address, device slot or function slot is taken.
    Full,
    /// A hub would not reset or enable the port.
    PortReset,
    /// A `SuperSpeed` hub, whose ports this driver does not walk.
    Unsupported,
}

impl From<host::Error> for Error {
    fn from(error: host::Error) -> Self {
        Error::Transfer(error)
    }
}

/// Where a device hangs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parent {
    /// A root port, counting from zero.
    Root(u32),
    /// A port of a hub, counting from one, as hubs do.
    Hub {
        /// The hub's device slot.
        device: usize,
        /// The port.
        port: u8,
    },
}

/// Something the driver may want to say on the console.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Note {
    /// A device was set up.
    Device {
        /// Its address.
        address: u8,
        /// Its speed.
        speed: Speed,
        /// `idVendor`.
        vendor: u16,
        /// `idProduct`.
        product: u16,
        /// Whether it is a hub, and how many ports it has.
        hub_ports: Option<u8>,
        /// How many keyboards and mice it has.
        functions: usize,
    },
    /// A device went.
    Gone {
        /// Its address.
        address: u8,
    },
    /// A device could not be set up, and its port is left alone until it
    /// is unplugged.
    Failed {
        /// Where it is.
        parent: Parent,
        /// Why.
        error: Error,
    },
    /// A function's pipe halted too often in a row and was left stopped: its
    /// device sends nothing more until it is plugged in again.
    Stopped {
        /// The function.
        function: usize,
    },
    /// A disk was found.
    Disk {
        /// Its index.
        disk: usize,
        /// How many blocks it has.
        blocks: u64,
        /// How many bytes a block is.
        block_bytes: u32,
    },
    /// A mass-storage interface would not open as a disk.
    DiskFailed {
        /// Why.
        error: storage::Error,
    },
    /// A full- or low-speed device on a root port went to the companion
    /// controller, which nothing drives.
    Handed {
        /// The root port.
        port: u32,
    },
}

/// What the driver hands on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Output {
    /// A keyboard or mouse appeared: introduce it with [`Bus::hello`].
    Attached(usize),
    /// A function's report made events, which [`Bus::events`] holds until
    /// the next output is read: never more than one EVENTS message holds,
    /// ending in `SYN_REPORT`.
    Events(usize),
    /// A function went: its index is free again.
    Detached(usize),
}

/// A disk coming or going, which [`Bus::next_disk_change`] hands on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiskChange {
    /// A disk appeared: [`Bus::disk`] says what it is.
    Attached(usize),
    /// A disk went: its index is free again.
    Detached(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pending {
    Attached(usize),
    Report {
        function: usize,
        bytes: [u8; PIPE_BYTES],
        len: usize,
    },
    Detached(usize),
}

/// A disk and the device it is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Held {
    device: usize,
    disk: Disk,
    /// Unplugged, with its [`Output::DiskDetached`] still to be read.
    gone: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Device {
    target: Target,
    parent: Parent,
    hub: Option<Hub>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Hub {
    ports: u8,
    /// Ports whose device failed, bit by port number.
    failed: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Function {
    device: usize,
    pipe: usize,
    /// The interface, for `SET_REPORT`.
    interface: u8,
    kind: Kind,
    identity: Identity,
    reader: Interpreter,
    /// Unplugged, with its [`Output::Detached`] still to be read.
    gone: bool,
    /// Its pipe stopped, and a note said so.
    stopped: bool,
}

/// A fixed ring of `N` items.
#[derive(Clone, Copy, Debug)]
struct Ring<T, const N: usize> {
    items: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Ring<T, N> {
    const fn new() -> Self {
        Ring {
            items: [None; N],
            head: 0,
            len: 0,
        }
    }

    /// Add `item` at the back: whether there was room.
    fn push(&mut self, item: T) -> bool {
        if self.len == N {
            return false;
        }
        if let Some(slot) = self.items.get_mut((self.head + self.len) % N) {
            *slot = Some(item);
            self.len += 1;
            return true;
        }
        false
    }

    /// Add `item`, dropping the oldest to make room.
    fn push_over(&mut self, item: T) {
        if self.len == N {
            let _ = self.pop();
        }
        let _ = self.push(item);
    }

    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let item = self.items.get_mut(self.head)?.take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        item
    }
}

/// The bus, its controller, and everything found on it.
#[derive(Debug)]
pub struct Bus<H> {
    hc: H,
    devices: [Option<Device>; MAX_DEVICES],
    functions: [Option<Function>; MAX_FUNCTIONS],
    disks: [Option<Held>; MAX_DISKS],
    disk_changes: Ring<DiskChange, { 2 * MAX_DISKS }>,
    pending: Ring<Pending, PENDING>,
    /// The events of the last [`Output::Events`].
    events: EventBuf,
    notes: Ring<Note, NOTES>,
    /// Root ports whose device failed or was handed on, bit by port.
    root_failed: u32,
    next_poll: u64,
}

impl<R: Registers, D: Dma, C: Clock> Bus<ehci::Controller<R, D, C>> {
    /// Start an EHCI controller with nothing found yet; the first
    /// [`Bus::poll`] finds what is plugged in.
    ///
    /// # Errors
    ///
    /// As [`ehci::Controller::start`].
    pub fn start(parts: Parts<R, D, C>) -> Result<Self, (ehci::Error, Parts<R, D, C>)> {
        Ok(Bus::with(ehci::Controller::start(parts)?))
    }
}

impl<H: Host> Bus<H> {
    /// The bus behind a controller already running, with nothing found yet;
    /// the first [`Bus::poll`] finds what is plugged in.
    pub fn with(hc: H) -> Self {
        Bus {
            hc,
            devices: [None; MAX_DEVICES],
            functions: [None; MAX_FUNCTIONS],
            disks: [None; MAX_DISKS],
            disk_changes: Ring::new(),
            pending: Ring::new(),
            events: EventBuf::default(),
            notes: Ring::new(),
            root_failed: 0,
            next_poll: 0,
        }
    }

    /// Stop the controller.
    ///
    /// # Errors
    ///
    /// As [`Host::shutdown`]: the parts, whose memory must be kept.
    pub fn shutdown(self) -> Result<H::Parts, H::Parts> {
        self.hc.shutdown()
    }

    /// When the ports should next be polled, on the clock's time.
    #[must_use]
    pub const fn next_poll(&self) -> u64 {
        self.next_poll
    }

    /// Poll every port, setting up what was plugged in and letting go of
    /// what was unplugged.
    pub fn poll(&mut self) {
        // The failed-port marks are a bit a port.
        for port in 0..self.hc.ports().min(u32::BITS) {
            self.poll_root(port);
        }
        // A hub found in this pass is polled in it too, whatever its slot.
        for device in 0..MAX_DEVICES {
            if self.hub(device).is_some() {
                self.poll_hub(device);
            }
        }
        self.next_poll = self.hc.clock().now_nanos().saturating_add(POLL_INTERVAL);
    }

    /// Take what the controller's interrupt says: every packet the pipes
    /// received becomes a pending report.
    ///
    /// # Errors
    ///
    /// As [`Host::service`]: the controller halted.
    pub fn on_interrupt(&mut self) -> Result<(), host::Error> {
        let Bus {
            hc,
            functions,
            pending,
            ..
        } = self;
        let serviced = hc.service(|packet| {
            let owner = functions
                .iter()
                .position(|function| function.is_some_and(|f| f.pipe == packet.pipe && !f.gone));
            if let Some(function) = owner {
                let _ = pending.push(Pending::Report {
                    function,
                    bytes: packet.bytes,
                    len: packet.len,
                });
            }
        });
        for (index, slot) in self.functions.iter_mut().enumerate() {
            if let Some(function) = slot
                && !function.stopped
                && !function.gone
                && self.hc.pipe_stopped(function.pipe)
            {
                function.stopped = true;
                self.notes.push_over(Note::Stopped { function: index });
            }
        }
        // A controller that says a port changed is polled at once.
        if self.hc.ports_changed() {
            self.next_poll = self.hc.clock().now_nanos();
        }
        serviced
    }

    /// The next thing to hand on, if any.
    pub fn next_output(&mut self) -> Option<Output> {
        loop {
            match self.pending.pop()? {
                Pending::Attached(function) => return Some(Output::Attached(function)),
                Pending::Detached(function) => {
                    if let Some(slot) = self.functions.get_mut(function) {
                        *slot = None;
                    }
                    return Some(Output::Detached(function));
                }

                Pending::Report {
                    function,
                    bytes,
                    len,
                } => {
                    let Some(Some(state)) = self.functions.get_mut(function) else {
                        continue;
                    };
                    state
                        .reader
                        .report(bytes.get(..len).unwrap_or(&[]), &mut self.events);
                    if !self.events.as_slice().is_empty() {
                        return Some(Output::Events(function));
                    }
                }
            }
        }
    }

    /// The next note, if any.
    pub fn next_note(&mut self) -> Option<Note> {
        self.notes.pop()
    }

    /// The events of the last [`Output::Events`].
    #[must_use]
    pub fn events(&self) -> &[RawEvent] {
        self.events.as_slice()
    }

    /// The HELLO introducing `function`, at `location`.
    #[must_use]
    pub fn hello(&self, function: usize, location: u32) -> Option<Hello> {
        let function = self.functions.get(function)?.as_ref()?;
        Some(crate::hid::hello(
            &function.reader,
            &function.identity,
            location,
        ))
    }

    /// Light `function`'s LEDs as the core's LED events say, with an output
    /// report for each report holding an LED: whether any was sent. Events
    /// for other types, codes the function has no LED for, and a function
    /// gone, change nothing.
    ///
    /// # Errors
    ///
    /// The first `SET_REPORT` that failed; the rest are still sent.
    pub fn set_leds(&mut self, function: usize, events: &[RawEvent]) -> Result<bool, Error> {
        let Some(Some(entry)) = self.functions.get_mut(function) else {
            return Ok(false);
        };
        if entry.gone || !entry.reader.set_leds(events) {
            return Ok(false);
        }
        let interface = entry.interface;
        let reader = entry.reader;
        let Some(target) = self
            .devices
            .get(entry.device)
            .copied()
            .flatten()
            .map(|device| device.target)
        else {
            return Ok(false);
        };
        let mut first_error = None;
        let mut sent = false;
        let hc = &mut self.hc;
        reader.led_reports(|id, bytes| {
            let length = u16::try_from(bytes.len()).unwrap_or(0);
            let setup = Setup::set_output_report(interface, id, length);
            match hc.control(&target, &setup, Data::Out(bytes)) {
                Ok(_) => sent = true,
                Err(error) => {
                    let _ = first_error.get_or_insert(Error::Transfer(error));
                }
            }
        });
        first_error.map_or(Ok(sent), Err)
    }

    /// The next disk to have come or gone, if any.
    pub fn next_disk_change(&mut self) -> Option<DiskChange> {
        let change = self.disk_changes.pop()?;
        if let DiskChange::Detached(disk) = change
            && let Some(slot) = self.disks.get_mut(disk)
        {
            *slot = None;
        }
        Some(change)
    }

    /// Disk `disk`, if it is there.
    #[must_use]
    pub fn disk(&self, disk: usize) -> Option<&Disk> {
        self.disks
            .get(disk)?
            .as_ref()
            .filter(|held| !held.gone)
            .map(|held| &held.disk)
    }

    /// Read whole blocks of disk `disk` from `lba`.
    ///
    /// # Errors
    ///
    /// As [`Disk::read`]; [`storage::Error::Unsupported`] for a disk that is
    /// not there.
    pub fn disk_read(
        &mut self,
        disk: usize,
        lba: u64,
        buffer: &mut [u8],
    ) -> Result<(), storage::Error> {
        let Bus { hc, disks, .. } = self;
        Self::held(disks, disk)?.read(hc, lba, buffer)
    }

    /// Write whole blocks of disk `disk` at `lba`.
    ///
    /// # Errors
    ///
    /// As [`Bus::disk_read`].
    pub fn disk_write(
        &mut self,
        disk: usize,
        lba: u64,
        buffer: &[u8],
    ) -> Result<(), storage::Error> {
        let Bus { hc, disks, .. } = self;
        Self::held(disks, disk)?.write(hc, lba, buffer)
    }

    /// Have disk `disk` write its cache out.
    ///
    /// # Errors
    ///
    /// As [`Bus::disk_read`].
    pub fn disk_flush(&mut self, disk: usize) -> Result<(), storage::Error> {
        let Bus { hc, disks, .. } = self;
        Self::held(disks, disk)?.flush(hc)
    }

    fn held(
        disks: &mut [Option<Held>; MAX_DISKS],
        disk: usize,
    ) -> Result<&mut Disk, storage::Error> {
        disks
            .get_mut(disk)
            .and_then(Option::as_mut)
            .filter(|held| !held.gone)
            .map(|held| &mut held.disk)
            .ok_or(storage::Error::Unsupported)
    }

    /// What `function` is.
    #[must_use]
    pub fn kind(&self, function: usize) -> Option<Kind> {
        Some(self.functions.get(function).copied().flatten()?.kind)
    }

    // -----------------------------------------------------------------------
    // Ports
    // -----------------------------------------------------------------------

    fn poll_root(&mut self, port: u32) {
        let bit = 1_u32 << port;
        let state = self.hc.root_port(port);
        if state.changed {
            self.hc.clear_root_changes(port);
            if let Some(child) = self.child(Parent::Root(port)) {
                self.detach(child);
            }
            self.root_failed &= !bit;
        }
        let child = self.child(Parent::Root(port));
        match (state.connected, child) {
            (true, None) if self.root_failed & bit == 0 => self.attach_root(port),
            (false, Some(child)) => self.detach(child),
            _ => {}
        }
    }

    fn attach_root(&mut self, port: u32) {
        let bit = 1_u32 << port;
        self.hc.clock().sleep_nanos(DEBOUNCE);
        if !self.hc.root_port(port).connected {
            return;
        }
        match self.hc.reset_root_port(port) {
            RootReset::Ready(speed) => {
                self.hc.clear_root_changes(port);
                let place = Place {
                    root_port: port,
                    route: 0,
                    speed,
                    translator: None,
                };
                if let Err(error) = self.enumerate(Parent::Root(port), &place) {
                    self.root_failed |= bit;
                    self.notes.push_over(Note::Failed {
                        parent: Parent::Root(port),
                        error,
                    });
                }
            }
            RootReset::Handed => {
                self.root_failed |= bit;
                self.notes.push_over(Note::Handed { port });
            }
            RootReset::Failed => {
                self.root_failed |= bit;
                self.notes.push_over(Note::Failed {
                    parent: Parent::Root(port),
                    error: Error::PortReset,
                });
            }
            RootReset::Gone => {}
        }
    }

    fn poll_hub(&mut self, device: usize) {
        let Some((target, hub)) = self.hub(device) else {
            return;
        };
        for port in 1..=hub.ports {
            let Ok(status) = self.port_status(&target, port) else {
                continue;
            };
            for feature in status.changes() {
                let _ =
                    self.hc
                        .control(&target, &hub::clear_port_feature(port, feature), Data::None);
            }
            let parent = Parent::Hub { device, port };
            if status.change & CHANGE_CONNECTION != 0 {
                if let Some(child) = self.child(parent) {
                    self.detach(child);
                }
                self.set_hub_failed(device, port, false);
            }
            let failed = self
                .hub(device)
                .is_some_and(|(_, hub)| hub.failed & (1 << port) != 0);
            match (status.connected(), self.child(parent)) {
                (true, None) if !failed => {
                    if let Err(error) = self.attach_hub_port(device, port) {
                        self.set_hub_failed(device, port, true);
                        self.notes.push_over(Note::Failed { parent, error });
                    }
                }
                (false, Some(child)) => self.detach(child),
                _ => {}
            }
        }
    }

    fn attach_hub_port(&mut self, device: usize, port: u8) -> Result<(), Error> {
        let Some((target, _)) = self.hub(device) else {
            return Ok(());
        };
        self.hc.clock().sleep_nanos(DEBOUNCE);
        if !self.port_status(&target, port)?.connected() {
            return Ok(());
        }
        let _ = self.hc.control(
            &target,
            &hub::set_port_feature(port, PORT_RESET),
            Data::None,
        )?;
        let limit = HUB_RESET_PATIENCE / HUB_RESET_STEP;
        let mut status = PortStatus::default();
        for _ in 0..limit {
            self.hc.clock().sleep_nanos(HUB_RESET_STEP);
            status = self.port_status(&target, port)?;
            if status.change & CHANGE_RESET != 0 {
                break;
            }
        }
        let _ = self.hc.control(
            &target,
            &hub::clear_port_feature(port, hub::C_PORT_RESET),
            Data::None,
        );
        if status.change & CHANGE_RESET == 0 || status.status & STATUS_ENABLE == 0 {
            return Err(Error::PortReset);
        }
        self.hc.clock().sleep_nanos(RESET_RECOVERY);
        let speed = status.speed();
        // The transaction translator is the nearest high-speed hub's: this
        // one, if it is high speed, or the one it is itself behind.
        let translator = if speed == Speed::High {
            None
        } else if target.speed == Speed::High {
            Some((target.handle(), port))
        } else {
            target.translator
        };
        // The route gains this hub's port in the tier below the hub's own.
        let tier = (u32::BITS - target.route.leading_zeros()).div_ceil(4);
        let place = Place {
            root_port: target.root_port,
            route: target.route | (u32::from(port.min(15)) << (4 * tier.min(4))),
            speed,
            translator,
        };
        self.enumerate(Parent::Hub { device, port }, &place)
    }

    fn port_status(&mut self, target: &Target, port: u8) -> Result<PortStatus, Error> {
        let mut bytes = [0_u8; 4];
        let got = self
            .hc
            .control(target, &hub::get_port_status(port), Data::In(&mut bytes))?;
        if got < 4 {
            return Err(Error::Descriptor);
        }
        PortStatus::parse(&bytes).ok_or(Error::Descriptor)
    }

    fn set_hub_failed(&mut self, device: usize, port: u8, failed: bool) {
        if let Some(Some(Device { hub: Some(hub), .. })) = self.devices.get_mut(device) {
            if failed {
                hub.failed |= 1 << port;
            } else {
                hub.failed &= !(1 << port);
            }
        }
    }

    fn hub(&self, device: usize) -> Option<(Target, Hub)> {
        let device = self.devices.get(device).copied().flatten()?;
        Some((device.target, device.hub?))
    }

    fn child(&self, parent: Parent) -> Option<usize> {
        self.devices
            .iter()
            .position(|device| device.is_some_and(|device| device.parent == parent))
    }

    // -----------------------------------------------------------------------
    // Devices
    // -----------------------------------------------------------------------

    /// Give a device in its default state an address, read what it is, and
    /// configure it: a hub is powered and its ports left to the next poll,
    /// and every keyboard and mouse boot interface becomes a function.
    fn enumerate(&mut self, parent: Parent, place: &Place) -> Result<(), Error> {
        let slot = self
            .devices
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Full)?;
        let address = self.free_address().ok_or(Error::Full)?;
        let mut target = self.hc.open(place)?;
        let set_up = self.set_up(slot, parent, &mut target, address);
        if set_up.is_err() {
            if let Some(entry) = self.devices.get_mut(slot) {
                *entry = None;
            }
            self.hc.release(&target);
        }
        set_up
    }

    /// [`Bus::enumerate`], from the first transfer on.
    fn set_up(
        &mut self,
        slot: usize,
        parent: Parent,
        target: &mut Target,
        address: u8,
    ) -> Result<(), Error> {
        let speed = target.speed;
        let mut bytes = [0_u8; 18];
        let head = bytes.get_mut(..8).ok_or(Error::Descriptor)?;
        let got = self.hc.control(
            target,
            &Setup::get_descriptor(DEVICE, 0, 0, 8),
            Data::In(head),
        )?;
        let max_packet = bytes.get(7).copied().unwrap_or(0);
        let max_packet = if speed == Speed::Super {
            SUPER_MAX_PACKET
        } else if is_control_packet(max_packet) {
            u16::from(max_packet)
        } else {
            0
        };
        if got < 8 || max_packet == 0 {
            return Err(Error::Descriptor);
        }
        self.hc.assign(target, address, max_packet)?;
        let target = *target;
        let address = target.address;

        let got = self.hc.control(
            &target,
            &Setup::get_descriptor(DEVICE, 0, 0, 18),
            Data::In(&mut bytes),
        )?;
        let descriptor =
            DeviceDescriptor::parse(bytes.get(..got).unwrap_or(&[])).ok_or(Error::Descriptor)?;
        let configuration = self.configuration(&target)?;
        let is_hub = descriptor.class == CLASS_HUB
            || configuration
                .interfaces()
                .iter()
                .any(|interface| interface.class == CLASS_HUB);
        if is_hub && speed == Speed::Super {
            return Err(Error::Unsupported);
        }
        let _ = self.hc.control(
            &target,
            &Setup::set_configuration(configuration.value),
            Data::None,
        )?;

        if let Some(entry) = self.devices.get_mut(slot) {
            *entry = Some(Device {
                target,
                parent,
                hub: None,
            });
        }
        let (hub_ports, functions) = if is_hub {
            (Some(self.set_up_hub(slot, &target)?), 0)
        } else {
            self.set_up_disks(slot, &target, &descriptor, &configuration);
            (
                None,
                self.set_up_functions(slot, &target, &descriptor, &configuration),
            )
        };
        self.notes.push_over(Note::Device {
            address,
            speed,
            vendor: descriptor.vendor,
            product: descriptor.product,
            hub_ports,
            functions,
        });
        Ok(())
    }

    fn configuration(&mut self, target: &Target) -> Result<Configuration, Error> {
        let mut bytes = [0_u8; CONFIGURATION_BYTES];
        let header = bytes.get_mut(..9).ok_or(Error::Descriptor)?;
        let got = self.hc.control(
            target,
            &Setup::get_descriptor(CONFIGURATION, 0, 0, 9),
            Data::In(header),
        )?;
        let total = Configuration::total_length(bytes.get(..got).unwrap_or(&[]))
            .ok_or(Error::Descriptor)?;
        let wanted = usize::from(total).clamp(9, CONFIGURATION_BYTES);
        let whole = bytes.get_mut(..wanted).ok_or(Error::Descriptor)?;
        let length = u16::try_from(wanted).map_err(|_| Error::Descriptor)?;
        let got = self.hc.control(
            target,
            &Setup::get_descriptor(CONFIGURATION, 0, 0, length),
            Data::In(whole),
        )?;
        Configuration::parse(bytes.get(..got).unwrap_or(&[])).ok_or(Error::Descriptor)
    }

    /// Power every port of the hub in `slot`: how many it has.
    fn set_up_hub(&mut self, slot: usize, target: &Target) -> Result<u8, Error> {
        let mut bytes = [0_u8; 8];
        let got = self
            .hc
            .control(target, &hub::get_hub_descriptor(8), Data::In(&mut bytes))?;
        let descriptor =
            HubDescriptor::parse(bytes.get(..got).unwrap_or(&[])).ok_or(Error::Descriptor)?;
        self.hc
            .set_hub(target, descriptor.ports, descriptor.think_time)?;
        for port in 1..=descriptor.ports {
            let _ =
                self.hc
                    .control(target, &hub::set_port_feature(port, PORT_POWER), Data::None)?;
        }
        // Power good, then the debounce every new connection is given.
        let settle = u64::from(descriptor.power_good_ms) * MILLISECOND + DEBOUNCE;
        self.hc.clock().sleep_nanos(settle);
        if let Some(Some(device)) = self.devices.get_mut(slot) {
            device.hub = Some(Hub {
                ports: descriptor.ports,
                failed: 0,
            });
        }
        Ok(descriptor.ports)
    }

    /// Open every bulk-only mass-storage interface of the device in `slot`
    /// as a disk, behind a controller that moves bulk data.
    fn set_up_disks(
        &mut self,
        slot: usize,
        target: &Target,
        descriptor: &DeviceDescriptor,
        configuration: &Configuration,
    ) {
        let mut serial = None;
        for interface in configuration.interfaces() {
            if !storage::is_disk(interface) {
                continue;
            }
            let Some(index) = self.disks.iter().position(Option::is_none) else {
                break;
            };
            match Disk::open(&mut self.hc, target, interface) {
                Ok(mut disk) => {
                    disk.serial =
                        *serial.get_or_insert_with(|| self.serial(target, descriptor.serial));
                    if let Some(entry) = self.disks.get_mut(index) {
                        *entry = Some(Held {
                            device: slot,
                            disk,
                            gone: false,
                        });
                    }
                    let _ = self.disk_changes.push(DiskChange::Attached(index));
                    self.notes.push_over(Note::Disk {
                        disk: index,
                        blocks: disk.blocks,
                        block_bytes: disk.block_bytes,
                    });
                }
                // A controller with no bulk pipes has no disks, silently.
                Err(storage::Error::Transfer(host::Error::Unsupported)) => {}
                Err(error) => self.notes.push_over(Note::DiskFailed { error }),
            }
        }
    }

    /// Turn every HID interface of the device in `slot` whose reports map
    /// to a key, a button or an axis into a function: how many.
    fn set_up_functions(
        &mut self,
        slot: usize,
        target: &Target,
        descriptor: &DeviceDescriptor,
        configuration: &Configuration,
    ) -> usize {
        let mut made = 0;
        let mut identity: Option<(Text, Text)> = None;
        for interface in configuration.interfaces() {
            if interface.class != CLASS_HID {
                continue;
            }
            let Some((endpoint, packet)) = interface.interrupt_in else {
                continue;
            };
            let Some(function) = self.functions.iter().position(Option::is_none) else {
                break;
            };
            let Some(reader) = self.reader(target, interface) else {
                continue;
            };
            let kind = reader.kind();
            if kind == Kind::Keyboard {
                let _ = self.hc.control(
                    target,
                    &Setup::set_idle_forever(interface.number),
                    Data::None,
                );
            }
            let packet = packet.min(PIPE_BYTES as u16);
            let endpoint = Endpoint {
                number: endpoint,
                max_packet: packet,
                interval: interface.interval,
            };
            let Ok(pipe) = self.hc.open_pipe(target, &endpoint) else {
                break;
            };
            let names = *identity.get_or_insert_with(|| self.names(target, descriptor));
            let name = crate::hid::name(names.0.as_bytes(), names.1.as_bytes(), b"USB", kind);
            if let Some(entry) = self.functions.get_mut(function) {
                *entry = Some(Function {
                    device: slot,
                    pipe,
                    interface: interface.number,
                    kind,
                    identity: Identity {
                        vendor: descriptor.vendor,
                        product: descriptor.product,
                        release: descriptor.release,
                        name,
                    },
                    reader,
                    gone: false,
                    stopped: false,
                });
                let _ = self.pending.push(Pending::Attached(function));
                made += 1;
            }
        }
        made
    }

    /// How `interface`'s reports are read, and the interface put in the
    /// protocol that makes them so: its own report descriptor in the report
    /// protocol, if it has one that maps anything; otherwise, for a boot
    /// keyboard or mouse that takes the boot protocol, the boot layout.
    /// `None` for an interface that is neither.
    fn reader(&mut self, target: &Target, interface: &Interface) -> Option<Interpreter> {
        let boot = match (interface.subclass, interface.protocol) {
            (SUBCLASS_BOOT, PROTOCOL_KEYBOARD) => Some(&BOOT_KEYBOARD[..]),
            (SUBCLASS_BOOT, PROTOCOL_MOUSE) => Some(&BOOT_MOUSE[..]),
            _ => None,
        };
        if let Some(reader) = self.own_reader(target, interface) {
            // Devices start in the report protocol, but a boot interface
            // firmware used may have been left in the other.
            if boot.is_some() {
                let _ = self.hc.control(
                    target,
                    &Setup::set_report_protocol(interface.number),
                    Data::None,
                );
            }
            return Some(reader);
        }
        let layout = boot?;
        let _ = self
            .hc
            .control(
                target,
                &Setup::set_boot_protocol(interface.number),
                Data::None,
            )
            .ok()?;
        Descriptor::parse(layout).ok().map(Interpreter::new)
    }

    /// The interface's report descriptor, read, if it maps anything.
    fn own_reader(&mut self, target: &Target, interface: &Interface) -> Option<Interpreter> {
        let length = usize::from(interface.report_length);
        if length == 0 || length > CONTROL_BYTES {
            return None;
        }
        let mut bytes = [0_u8; CONTROL_BYTES];
        let buffer = bytes.get_mut(..length)?;
        let setup = Setup::get_report_descriptor(interface.number, interface.report_length);
        let got = self.hc.control(target, &setup, Data::In(buffer)).ok()?;
        let descriptor = Descriptor::parse(bytes.get(..got)?).ok()?;
        let reader = Interpreter::new(descriptor);
        reader.maps_anything().then_some(reader)
    }

    /// String `index` of the device in its first language, as a disk's
    /// serial ([`storage::serial_of`]): all zero where the device has none
    /// or will not say.
    fn serial(&mut self, target: &Target, index: u8) -> [u8; storage::SERIAL_BYTES] {
        let mut serial = [0_u8; storage::SERIAL_BYTES];
        if index == 0 {
            return serial;
        }
        let mut bytes = [0_u8; STRING_BYTES];
        let language = self
            .hc
            .control(
                target,
                &Setup::get_descriptor(STRING, 0, 0, STRING_BYTES as u16),
                Data::In(&mut bytes),
            )
            .ok()
            .and_then(|got| first_language(bytes.get(..got).unwrap_or(&[])))
            .unwrap_or(LANGUAGE_US);
        let mut raw = [0_u8; STRING_BYTES];
        if let Ok(got) = self.hc.control(
            target,
            &Setup::get_descriptor(STRING, index, language, STRING_BYTES as u16),
            Data::In(&mut raw),
        ) {
            // A string descriptor holds at most 126 UTF-16 units.
            let mut ascii = [0_u8; STRING_BYTES / 2];
            let len = string_ascii(raw.get(..got).unwrap_or(&[]), &mut ascii);
            serial = storage::serial_of(ascii.get(..len).unwrap_or(&[]));
        }
        serial
    }

    /// The manufacturer's and product's strings, empty where the device has
    /// none or will not say.
    fn names(&mut self, target: &Target, descriptor: &DeviceDescriptor) -> (Text, Text) {
        let mut bytes = [0_u8; STRING_BYTES];
        let language = self
            .hc
            .control(
                target,
                &Setup::get_descriptor(STRING, 0, 0, STRING_BYTES as u16),
                Data::In(&mut bytes),
            )
            .ok()
            .and_then(|got| first_language(bytes.get(..got).unwrap_or(&[])))
            .unwrap_or(LANGUAGE_US);
        let mut string = |index: u8| -> Text {
            if index == 0 {
                return Text::NONE;
            }
            let mut raw = [0_u8; STRING_BYTES];
            let Ok(got) = self.hc.control(
                target,
                &Setup::get_descriptor(STRING, index, language, STRING_BYTES as u16),
                Data::In(&mut raw),
            ) else {
                return Text::NONE;
            };
            let mut ascii = [0_u8; NAME_BYTES];
            let len = string_ascii(raw.get(..got).unwrap_or(&[]), &mut ascii);
            Text::new(ascii.get(..len).unwrap_or(&[])).unwrap_or(Text::NONE)
        };
        (
            string(descriptor.manufacturer),
            string(descriptor.product_name),
        )
    }

    /// Let go of the device in `slot` and everything behind it.
    fn detach(&mut self, slot: usize) {
        let Some(device) = self.devices.get(slot).copied().flatten() else {
            return;
        };
        if let Some(hub) = device.hub {
            for port in 1..=hub.ports {
                if let Some(child) = self.child(Parent::Hub { device: slot, port }) {
                    self.detach(child);
                }
            }
        }
        for index in 0..MAX_FUNCTIONS {
            let Some(Some(function)) = self.functions.get(index).copied() else {
                continue;
            };
            if function.device != slot || function.gone {
                continue;
            }
            self.hc.close_pipe(function.pipe);
            if let Some(Some(entry)) = self.functions.get_mut(index) {
                entry.gone = true;
            }
            let _ = self.pending.push(Pending::Detached(index));
        }
        for index in 0..MAX_DISKS {
            let Some(Some(held)) = self.disks.get(index).copied() else {
                continue;
            };
            if held.device != slot || held.gone {
                continue;
            }
            held.disk.close(&mut self.hc);
            if let Some(Some(entry)) = self.disks.get_mut(index) {
                entry.gone = true;
            }
            let _ = self.disk_changes.push(DiskChange::Detached(index));
        }
        if let Some(entry) = self.devices.get_mut(slot) {
            *entry = None;
        }
        self.hc.release(&device.target);
        self.notes.push_over(Note::Gone {
            address: device.target.address,
        });
    }

    fn free_address(&self) -> Option<u8> {
        (1..=127_u8).find(|&address| {
            !self
                .devices
                .iter()
                .flatten()
                .any(|device| device.target.address == address)
        })
    }
}
