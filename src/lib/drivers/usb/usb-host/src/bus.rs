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
//!
//! # Serial adapters
//!
//! An FTDI UART or a USB-Blaster ([`crate::ftdi::identify`]) behind a
//! controller that moves bulk data becomes a serial port: its OUT endpoint
//! a bulk pair of its own, its IN endpoint a stream ([`Host::open_stream`])
//! whose chunks [`Bus::serial_read`] hands on with the chip's status bytes
//! taken off. [`Bus::next_serial_change`] says when one comes and goes,
//! the way [`Bus::next_disk_change`] does for disks.

use ferrix_inputctl::message::{Hello, RawEvent, Text};

use crate::blaster;
use crate::ehci;
use crate::ftdi::{self, Chip, Line, Role};
use crate::hid::{BOOT_KEYBOARD, BOOT_MOUSE, EventBuf, Identity, Interpreter, Kind};
use crate::host::{
    self, CONTROL_BYTES, Data, Endpoint, Host, PIPE_BYTES, Place, RootReset, STREAM_BYTES, Target,
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
use crate::watch::Watch;
use crate::{Clock, Dma, MILLISECOND, Parts, Registers};

/// The largest packet a `SuperSpeed` control endpoint takes, which its device
/// descriptor gives as the exponent 9.
const SUPER_MAX_PACKET: u16 = 512;

/// How often the ports are polled.
pub const POLL_INTERVAL: u64 = 250 * MILLISECOND;

/// How often a driver whose controller interrupt has not been seen takes
/// the controller's events, at first and again as soon as any moved: a
/// low-speed keyboard's 8 ms interval, so no report waits.
pub const SERVICE_POLL: u64 = 8 * MILLISECOND;

/// The longest that service waits while it finds nothing: 32 ms, at most
/// four of a keyboard's 8 ms reports, short of the eight interrupt
/// requests a function keeps queued (backlog p2-138).
pub const SERVICE_POLL_IDLE_MAX: u64 = 32 * MILLISECOND;

/// The service's next wait, after one of `prev` that `moved` something or
/// not: back to [`SERVICE_POLL`] when it did, else twice as long up to
/// [`SERVICE_POLL_IDLE_MAX`]. The shape of the intel driver's
/// `next_poll_ns`.
#[must_use]
pub const fn next_service_poll(prev: u64, moved: bool) -> u64 {
    if moved {
        return SERVICE_POLL;
    }
    let doubled = prev.saturating_mul(2);
    if doubled < SERVICE_POLL {
        SERVICE_POLL
    } else if doubled > SERVICE_POLL_IDLE_MAX {
        SERVICE_POLL_IDLE_MAX
    } else {
        doubled
    }
}
/// The most devices, hubs included.
pub const MAX_DEVICES: usize = 16;
/// The most keyboards and mice at once: the kernel's
/// `USB_INPUT_FUNCTIONS`, the input channels one host may hold.
pub const MAX_FUNCTIONS: usize = 8;

/// The most disks at once.
pub const MAX_DISKS: usize = 2;

/// The most serial adapters at once.
pub const MAX_SERIALS: usize = 2;

/// How long a UART's chip holds a part-filled packet before it sends it:
/// FTDI's default, and Linux's.
const UART_LATENCY_MS: u8 = 16;
/// The same for a USB-Blaster: `OpenOCD`'s two, since every scan waits for
/// its answer.
const BLASTER_LATENCY_MS: u8 = 2;
/// How long [`Bus::serial_read_exact`] looks again while it waits.
const SERIAL_STEP: u64 = MILLISECOND / 2;

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
/// Watched reports held at once; older ones are dropped, and counted.
const WATCHED: usize = 32;

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
    /// An interrupt pipe's transfer ended other than in plain success: a
    /// short packet, an error the pipe is recovered from, or an event for a
    /// request it did not queue.
    Transfer(host::Trouble),
    /// A Port Status Change event: the root port, from zero, and its
    /// status register as the event found it.
    PortEvent {
        /// The port.
        port: u32,
        /// `PORTSC`.
        status: u32,
    },
    /// A root port's device is let go: its status register, and whether a
    /// connect status change (rather than a port with nothing connected)
    /// was the reason.
    RootGone {
        /// The port.
        port: u32,
        /// `PORTSC` when it was decided.
        status: u32,
        /// Whether the change bit was set.
        changed: bool,
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
    /// An FTDI chip would not be set up as a serial port.
    SerialFailed {
        /// What it was taken for.
        role: Role,
        /// Why.
        error: Error,
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
        /// When the controller's events were taken, on the clock's time.
        at: u64,
    },
    Detached(usize),
}

/// A watched function's report as it came off its pipe ([`Bus::watch`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Watched {
    /// The function.
    pub function: usize,
    /// Its device's `idVendor` and `idProduct`.
    pub vendor: u16,
    /// See `vendor`.
    pub product: u16,
    /// When the driver took it from the controller, on the clock's time:
    /// every report one service took has the same.
    pub at: u64,
    /// The report, of which `len` bytes came, the ID byte first where the
    /// descriptor numbers reports.
    pub bytes: [u8; PIPE_BYTES],
    /// See `bytes`.
    pub len: usize,
    /// Reports of every function dropped since the last one handed on,
    /// because the driver had not read them before 64 others waited.
    pub lost: u32,
    /// Watched reports dropped since the last one handed on, because the
    /// driver had not taken them before 32 others waited.
    pub unsaid: u32,
}

impl Watched {
    /// The bytes that came.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// A serial adapter coming or going, which [`Bus::next_serial_change`]
/// hands on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SerialChange {
    /// One appeared: [`Bus::serial`] says what it is.
    Attached(usize),
    /// One went: its index is free again.
    Detached(usize),
}

/// What a serial adapter is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Serial {
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
    /// The chip family.
    pub chip: Chip,
    /// What it is used as.
    pub role: Role,
    /// The interface, counting from zero.
    pub interface: u8,
    /// Its bulk IN endpoint's packet size: where the status bytes recur.
    pub max_packet: u16,
    /// The line it was last set to.
    pub line: Line,
    /// The modem lines, as the last packet's status gave them.
    pub modem: u8,
    /// Every line-status error bit seen since the last
    /// [`Bus::take_serial_errors`].
    pub errors: u8,
}

/// A serial adapter and the device it is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Port {
    device: usize,
    serial: Serial,
    /// The `wIndex` port byte its requests carry.
    port: u16,
    /// Its OUT pair and IN stream.
    out: usize,
    stream: usize,
    /// Unplugged, with its [`SerialChange::Detached`] still to be read.
    gone: bool,
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
    serials: [Option<Port>; MAX_SERIALS],
    serial_changes: Ring<SerialChange, { 2 * MAX_SERIALS }>,
    pending: Ring<Pending, PENDING>,
    /// The events of the last [`Output::Events`].
    events: EventBuf,
    notes: Ring<Note, NOTES>,
    /// Root ports whose device failed or was handed on, bit by port.
    root_failed: u32,
    next_poll: u64,
    /// Whose reports are handed on raw, and those not yet taken.
    watch: Watch,
    watched: Ring<Watched, WATCHED>,
    /// Reports dropped for want of room in `pending`, and watched ones for
    /// want of it in `watched`, since the last watched report was taken.
    lost: u32,
    unsaid: u32,
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
            serials: [None; MAX_SERIALS],
            serial_changes: Ring::new(),
            pending: Ring::new(),
            events: EventBuf::default(),
            notes: Ring::new(),
            root_failed: 0,
            next_poll: 0,
            watch: Watch::OFF,
            watched: Ring::new(),
            lost: 0,
            unsaid: 0,
        }
    }

    /// Hand on the raw reports of the functions `watch` covers, from now on,
    /// for [`Bus::next_watched`].
    pub fn watch(&mut self, watch: Watch) {
        self.watch = watch;
    }

    /// What is watched.
    #[must_use]
    pub const fn watching(&self) -> Watch {
        self.watch
    }

    /// Keep a watched report for the driver, with what was dropped before
    /// it.
    fn keep_watched(&mut self, mut report: Watched) {
        if self.watched.len == WATCHED {
            self.unsaid = self.unsaid.saturating_add(1);
        }
        report.lost = core::mem::take(&mut self.lost);
        report.unsaid = core::mem::take(&mut self.unsaid);
        self.watched.push_over(report);
    }

    /// The next watched report, in the order the reports were read.
    pub fn next_watched(&mut self) -> Option<Watched> {
        self.watched.pop()
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
            lost,
            ..
        } = self;
        let at = hc.clock().now_nanos();
        let serviced = hc.service(|packet| {
            let owner = functions
                .iter()
                .position(|function| function.is_some_and(|f| f.pipe == packet.pipe && !f.gone));
            if let Some(function) = owner {
                let kept = pending.push(Pending::Report {
                    function,
                    bytes: packet.bytes,
                    len: packet.len,
                    at,
                });
                if !kept {
                    *lost = lost.saturating_add(1);
                }
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
        while let Some(trouble) = self.hc.take_trouble() {
            self.notes.push_over(Note::Transfer(trouble));
        }
        while let Some((port, status)) = self.hc.take_port_event() {
            self.notes.push_over(Note::PortEvent { port, status });
        }
        // A controller that says a port changed is polled at once.
        if self.hc.ports_changed() {
            self.next_poll = self.hc.clock().now_nanos();
        }
        serviced
    }

    // -----------------------------------------------------------------------
    // Serial adapters
    // -----------------------------------------------------------------------

    /// The next serial adapter to have come or gone, if any.
    pub fn next_serial_change(&mut self) -> Option<SerialChange> {
        let change = self.serial_changes.pop()?;
        if let SerialChange::Detached(serial) = change
            && let Some(slot) = self.serials.get_mut(serial)
        {
            *slot = None;
        }
        Some(change)
    }

    /// Serial adapter `serial`, if it is there.
    #[must_use]
    pub fn serial(&self, serial: usize) -> Option<&Serial> {
        self.serials
            .get(serial)?
            .as_ref()
            .filter(|port| !port.gone)
            .map(|port| &port.serial)
    }

    fn port(&self, serial: usize) -> Result<Port, Error> {
        self.serials
            .get(serial)
            .copied()
            .flatten()
            .filter(|port| !port.gone)
            .ok_or(Error::Transfer(host::Error::NoPipe))
    }

    fn port_target(&self, port: &Port) -> Result<Target, Error> {
        self.devices
            .get(port.device)
            .copied()
            .flatten()
            .map(|device| device.target)
            .ok_or(Error::Transfer(host::Error::NoPipe))
    }

    /// Set serial adapter `serial`'s line to `line`: its rate, framing and
    /// flow control.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a rate the chip cannot have, which leaves
    /// the line as it was; a request's failure.
    pub fn serial_configure(&mut self, serial: usize, line: &Line) -> Result<(), Error> {
        let port = self.port(serial)?;
        let target = self.port_target(&port)?;
        let setups =
            ftdi::configure(port.serial.chip, port.port, line).ok_or(Error::Unsupported)?;
        for setup in &setups {
            let _ = self.hc.control(&target, setup, Data::None)?;
        }
        if let Some(Some(entry)) = self.serials.get_mut(serial) {
            entry.serial.line = *line;
        }
        Ok(())
    }

    /// Set serial adapter `serial`'s DTR and RTS.
    ///
    /// # Errors
    ///
    /// The request's failure.
    pub fn serial_modem(&mut self, serial: usize, dtr: bool, rts: bool) -> Result<(), Error> {
        let port = self.port(serial)?;
        let target = self.port_target(&port)?;
        let _ = self
            .hc
            .control(&target, &ftdi::modem(port.port, dtr, rts), Data::None)?;
        Ok(())
    }

    /// Queue `bytes` to go out of serial adapter `serial`, without waiting
    /// for the chip to take them: how many were queued, which is fewer
    /// than all once the OUT stream is full. A chip that holds its OUT
    /// endpoint off -- CTS low under RTS/CTS -- holds back only these.
    ///
    /// # Errors
    ///
    /// An adapter that is not there.
    pub fn serial_write(&mut self, serial: usize, bytes: &[u8]) -> Result<usize, Error> {
        let port = self.port(serial)?;
        Ok(self.hc.send(port.out, bytes))
    }

    /// How many bytes [`Bus::serial_write`] would queue now.
    #[must_use]
    pub fn serial_room(&self, serial: usize) -> usize {
        self.port(serial)
            .map_or(0, |port| self.hc.out_room(port.out))
    }

    /// Bytes queued to serial adapter `serial` that the chip has not taken.
    #[must_use]
    pub fn serial_in_flight(&self, serial: usize) -> usize {
        self.port(serial)
            .map_or(0, |port| self.hc.in_flight(port.out))
    }

    /// Bytes serial adapter `serial` lost when its OUT endpoint stalled,
    /// since this was last asked.
    pub fn take_serial_lost(&mut self, serial: usize) -> usize {
        match self.port(serial) {
            Ok(port) => self.hc.take_lost(port.out),
            Err(_) => 0,
        }
    }

    /// What serial adapter `serial` received and nobody read, into `into`,
    /// without waiting: how many bytes. A chunk is taken only while `into`
    /// has room for a whole one, so nothing is dropped: what is left waits
    /// in the controller, which holds the chip back once its queue is full.
    pub fn serial_read(&mut self, serial: usize, into: &mut [u8]) -> usize {
        let Ok(port) = self.port(serial) else {
            return 0;
        };
        let max_packet = usize::from(port.serial.max_packet);
        let mut got = 0;
        let mut modem = port.serial.modem;
        let mut errors = 0;
        while into.len() - got >= STREAM_BYTES {
            let Some(chunk) = self.hc.take_chunk(port.stream) else {
                break;
            };
            ftdi::payload(chunk.as_slice(), max_packet, |status, data| {
                modem = status.modem;
                if !data.is_empty() {
                    errors |= status.line
                        & (ftdi::LINE_OVERRUN
                            | ftdi::LINE_PARITY
                            | ftdi::LINE_FRAMING
                            | ftdi::LINE_BREAK);
                }
                if let Some(slot) = into.get_mut(got..got + data.len()) {
                    slot.copy_from_slice(data);
                    got += data.len();
                }
            });
        }
        if let Some(Some(entry)) = self.serials.get_mut(serial) {
            entry.serial.modem = modem;
            entry.serial.errors |= errors;
        }
        got
    }

    /// The line errors serial adapter `serial` reported since this was last
    /// asked, cleared.
    pub fn take_serial_errors(&mut self, serial: usize) -> u8 {
        self.serials
            .get_mut(serial)
            .and_then(Option::as_mut)
            .map_or(0, |port| core::mem::take(&mut port.serial.errors))
    }

    /// Fill `into` from serial adapter `serial`, waiting at most `patience`
    /// for it: whether it filled. The controller's events are taken while
    /// it waits, as [`Bus::on_interrupt`] takes them, so a report that comes
    /// meanwhile waits for the next output as any other does.
    ///
    /// # Errors
    ///
    /// [`Error::Transfer`] with [`host::Error::Timeout`] when too little
    /// came in time; the controller's own failure.
    pub fn serial_read_exact(
        &mut self,
        serial: usize,
        into: &mut [u8],
        patience: u64,
    ) -> Result<(), Error> {
        let until = self.hc.clock().now_nanos().saturating_add(patience);
        let mut got = 0;
        let mut chunk = [0_u8; STREAM_BYTES];
        let mut held = 0;
        let mut at = 0;
        loop {
            // What a chunk read beyond `into` would be lost, so the chunks
            // go through a buffer of their own size.
            while got < into.len() && at < held {
                let take = (held - at).min(into.len() - got);
                if let (Some(to), Some(from)) =
                    (into.get_mut(got..got + take), chunk.get(at..at + take))
                {
                    to.copy_from_slice(from);
                }
                got += take;
                at += take;
            }
            if got == into.len() {
                if at < held {
                    // A cable answers what it is asked for and no more; more
                    // is a protocol error the caller will see in its data.
                    self.notes.push_over(Note::SerialFailed {
                        role: self.port(serial)?.serial.role,
                        error: Error::Descriptor,
                    });
                }
                return Ok(());
            }
            held = self.serial_read(serial, &mut chunk);
            at = 0;
            if held > 0 {
                continue;
            }
            self.on_interrupt()?;
            held = self.serial_read(serial, &mut chunk);
            if held > 0 {
                continue;
            }
            if self.hc.clock().now_nanos() >= until {
                return Err(Error::Transfer(host::Error::Timeout));
            }
            self.hc.clock().sleep_nanos(SERIAL_STEP);
        }
    }

    /// Serial adapter `serial` as a JTAG cable, if it is a USB-Blaster.
    #[must_use]
    pub fn blaster(&mut self, serial: usize) -> Option<Cable<'_, H>> {
        let port = self.port(serial).ok()?;
        (port.serial.role == Role::Blaster).then_some(Cable { bus: self, serial })
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
                    at,
                } => {
                    let Some(Some(state)) = self.functions.get_mut(function) else {
                        continue;
                    };
                    let (identity, kind) = (state.identity, state.kind);
                    if self.watch.covers(identity.vendor, identity.product, kind) {
                        self.keep_watched(Watched {
                            function,
                            vendor: identity.vendor,
                            product: identity.product,
                            at,
                            bytes,
                            len,
                            lost: 0,
                            unsaid: 0,
                        });
                    }
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
            let status = self.hc.root_port_raw(port);
            self.hc.clear_root_changes(port);
            if let Some(child) = self.child(Parent::Root(port)) {
                self.notes.push_over(Note::RootGone {
                    port,
                    status,
                    changed: true,
                });
                self.detach(child);
            }
            self.root_failed &= !bit;
        }
        let child = self.child(Parent::Root(port));
        match (state.connected, child) {
            (true, None) if self.root_failed & bit == 0 => self.attach_root(port),
            (false, Some(child)) => {
                self.notes.push_over(Note::RootGone {
                    port,
                    status: self.hc.root_port_raw(port),
                    changed: false,
                });
                self.detach(child);
            }
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
            self.set_up_disks(slot, &target, &configuration);
            self.set_up_serials(slot, &target, &descriptor, &configuration);
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
    fn set_up_disks(&mut self, slot: usize, target: &Target, configuration: &Configuration) {
        for interface in configuration.interfaces() {
            if !storage::is_disk(interface) {
                continue;
            }
            let Some(index) = self.disks.iter().position(Option::is_none) else {
                break;
            };
            match Disk::open(&mut self.hc, target, interface) {
                Ok(disk) => {
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

    /// Open the device in `slot` as a serial adapter, if it is an FTDI
    /// chip this drives, behind a controller that moves bulk data: its
    /// first vendor interface with a bulk endpoint each way, reset, purged,
    /// DTR and RTS raised, and a UART set to [`Line::DEFAULT`].
    fn set_up_serials(
        &mut self,
        slot: usize,
        target: &Target,
        descriptor: &DeviceDescriptor,
        configuration: &Configuration,
    ) {
        let Some((chip, role)) =
            ftdi::identify(descriptor.vendor, descriptor.product, descriptor.release)
        else {
            return;
        };
        let Some(interface) = configuration
            .interfaces()
            .iter()
            .find(|interface| interface.bulk_in.is_some() && interface.bulk_out.is_some())
        else {
            return;
        };
        let Some(index) = self.serials.iter().position(Option::is_none) else {
            return;
        };
        match self.open_serial(slot, target, chip, role, interface, descriptor) {
            Ok(port) => {
                if let Some(entry) = self.serials.get_mut(index) {
                    *entry = Some(port);
                }
                let _ = self.serial_changes.push(SerialChange::Attached(index));
            }
            // A controller with no bulk pipes has no serial ports, silently.
            Err(Error::Transfer(host::Error::Unsupported)) => {}
            Err(error) => self.notes.push_over(Note::SerialFailed { role, error }),
        }
    }

    fn open_serial(
        &mut self,
        slot: usize,
        target: &Target,
        chip: Chip,
        role: Role,
        interface: &Interface,
        descriptor: &DeviceDescriptor,
    ) -> Result<Port, Error> {
        let ((number_in, packet_in), (number_out, packet_out)) = interface
            .bulk_in
            .zip(interface.bulk_out)
            .ok_or(Error::Descriptor)?;
        let port = chip.port(interface.number);
        let hc = &mut self.hc;
        let _ = hc.control(target, &ftdi::reset(port), Data::None)?;
        for setup in &ftdi::purge(port) {
            let _ = hc.control(target, setup, Data::None)?;
        }
        let latency = match role {
            Role::Uart => UART_LATENCY_MS,
            Role::Blaster => BLASTER_LATENCY_MS,
        };
        let _ = hc.control(target, &ftdi::latency(port, latency), Data::None)?;
        let _ = hc.control(target, &ftdi::modem(port, true, true), Data::None)?;
        let line = Line::DEFAULT;
        if role == Role::Uart {
            let setups = ftdi::configure(chip, port, &line).ok_or(Error::Unsupported)?;
            for setup in &setups {
                let _ = hc.control(target, setup, Data::None)?;
            }
        }
        let out = hc.open_out_stream(
            target,
            &Endpoint {
                number: number_out,
                max_packet: packet_out,
                interval: 0,
            },
        )?;
        let stream = match hc.open_stream(
            target,
            &Endpoint {
                number: number_in,
                max_packet: packet_in,
                interval: 0,
            },
        ) {
            Ok(stream) => stream,
            Err(error) => {
                hc.close_stream(out);
                return Err(error.into());
            }
        };
        Ok(Port {
            device: slot,
            serial: Serial {
                vendor: descriptor.vendor,
                product: descriptor.product,
                chip,
                role,
                interface: interface.number,
                max_packet: packet_in,
                line,
                modem: 0,
                errors: 0,
            },
            port,
            out,
            stream,
            gone: false,
        })
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
        for index in 0..MAX_SERIALS {
            let Some(Some(port)) = self.serials.get(index).copied() else {
                continue;
            };
            if port.device != slot || port.gone {
                continue;
            }
            self.hc.close_stream(port.stream);
            self.hc.close_stream(port.out);
            if let Some(Some(entry)) = self.serials.get_mut(index) {
                entry.gone = true;
            }
            let _ = self.serial_changes.push(SerialChange::Detached(index));
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

/// How long a USB-Blaster has to answer a read: a scan of a few hundred
/// bits answers within a few of its latency timer's two milliseconds.
const CABLE_PATIENCE: u64 = 500 * MILLISECOND;

/// A USB-Blaster on the bus, as [`blaster::Link`] reaches it.
#[derive(Debug)]
pub struct Cable<'a, H> {
    bus: &'a mut Bus<H>,
    serial: usize,
}

impl<H: Host> blaster::Link for Cable<'_, H> {
    type Error = Error;

    /// All of `bytes` queued, the controller served while the stream is
    /// full: a scan waits for its cable anyway.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let until = self
            .bus
            .hc
            .clock()
            .now_nanos()
            .saturating_add(CABLE_PATIENCE);
        let mut sent = 0;
        while sent < bytes.len() {
            sent += self
                .bus
                .serial_write(self.serial, bytes.get(sent..).unwrap_or_default())?;
            if sent == bytes.len() {
                break;
            }
            self.bus.on_interrupt()?;
            if self.bus.hc.clock().now_nanos() >= until {
                return Err(Error::Transfer(host::Error::Timeout));
            }
            self.bus.hc.clock().sleep_nanos(SERIAL_STEP);
        }
        Ok(())
    }

    fn read(&mut self, into: &mut [u8]) -> Result<(), Error> {
        self.bus
            .serial_read_exact(self.serial, into, CABLE_PATIENCE)
    }
}
