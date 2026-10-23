//! The Pixel 7's USB device driver process: a ring-3 program that presents a
//! USB serial port (CDC-ACM) on the phone's USB-C port, through its DWC3
//! (`docs/vendor/google/pixel7/USB-HANDOVER.md`).
//!
//! Everything that knows anything about USB is `ferrix-dwc3` and
//! `ferrix-usb-device`, tested on the host against a model of the
//! controller. This program is the handles they were written to be wrapped
//! in, as `usbhid` is for the DK board's host:
//!
//! 1. START, as a gadget's: only the device, whose common window is the
//!    controller's registers. devmgr does not wait for it.
//! 2. The controller's memory is one VMO pinned with `PIN_COHERENT` before
//!    it is mapped: the controller does not snoop the caches, so the kernel
//!    maps it past them. What the library's `clean` and `invalidate` still
//!    need on such memory is a barrier, which orders the processor's writes
//!    before the register write that hands them over, and a register read
//!    before the memory reads it announced.
//! 3. One port carries the controller's interrupt and the log channel's
//!    signals. The wait on it ends at the next tick at the latest, when
//!    the port's state is looked at again: a host opening or closing it
//!    changes DTR without anything for the loop to wait on.
//! 4. The port carries the kernel's log: every byte the console has sent,
//!    the boot's lines and pid 1's output alike, read over a log control
//!    channel (`ferrix-logctl`) that only this controller's node may open.
//!    It is read only while a program on the host has the port open (DTR),
//!    and the next READ goes only once the last DATA is all in the
//!    controller's ring, so a host that is slow or absent holds the log
//!    back in the kernel's ring rather than losing it here. A host that
//!    opens the port late still gets the boot, as far as that ring reaches
//!    back; what it had already dropped is said in a line of its own.
//!    What the host sends is read and dropped, but for one line: [`REBOOT`],
//!    which restarts the machine -- on the phone, the watchdog reset every
//!    run ends with, which brings Android back. It is the monitor's "Boot
//!    Android" button, until adb gives the host a proper way in
//!    (`docs/ADB.md`).
//!
//! What it does is said on standard error, which is the console.
//!
//! The exit status names the step that failed ([`Step`]).

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_blkring::control::{Message as StartMessage, START_BYTES, Start};
use ferrix_dwc3::layout::AREA_BYTES;
use ferrix_dwc3::usb_device::acm::{ADB_OUT, DATA_IN, DATA_OUT, SerialPort};
use ferrix_dwc3::{Clock, Controller, Dma, Error as UsbError, Parts, Registers};
use ferrix_logctl::message::{MAX_BYTES, MAX_DATA, Message as LogMessage};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{IoMappingSpec, PACKET_INTERRUPT, PACKET_SIGNAL, TREE_GS201_DWC3};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::device::{Device, Interrupt, IoMapping};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};

mod adb;

ferrix_rt::entry!(main);

/// A page, on every architecture this runs on.
const PAGE: usize = 4096;
/// The controller's memory, in pages.
const AREA_PAGES: usize = AREA_BYTES.div_ceil(PAGE);

/// The port keys: the interrupt, and the log channel's signals.
const KEY_INTERRUPT: u64 = 1;
const KEY_LOG: u64 = 2;

/// How often the loop wakes when nothing happens, to see whether a host
/// opened the port.
const TICK_NANOS: u64 = 100_000_000;

/// The tick while adbd is connected: two milliseconds.
const ADB_TICK_NANOS: u64 = 2_000_000;

/// The line that asks for a restart, as the monitor's button sends it. Only
/// a whole line equal to it counts, so bytes typed by accident do nothing.
const REBOOT: &[u8] = b"ferrix-usbdev: reboot";

/// The longest line of the host's input kept while waiting for a newline.
const TYPED_BYTES: usize = 64;

/// The serial number the port reports.
const SERIAL: &str = "ferrix-pixel7";

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device that is not the Pixel's USB controller.
    Identity = 2,
    /// The registers could not be mapped.
    Registers = 3,
    /// Memory could not be made, pinned or mapped.
    Memory = 4,
    /// The controller was not as ABL leaves it, or would not come up.
    Controller = 5,
    /// The port, the interrupt or the waits could not be arranged.
    Events = 7,
    /// The controller failed while running.
    Faulted = 8,
    /// The controller would not stop: its memory is kept.
    Wedged = 9,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(()) => 0,
        Err(step) => {
            ferrix_rt::err!("usbdev: stopped at {step:?}");
            step as i32
        }
    }
}

// ---------------------------------------------------------------------------
// The console
// ---------------------------------------------------------------------------

/// A line, formatted into a fixed buffer and cut at its end.
struct Line {
    bytes: [u8; 160],
    len: usize,
}

impl Default for Line {
    fn default() -> Self {
        Line {
            bytes: [0; 160],
            len: 0,
        }
    }
}

impl Line {
    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = byte;
                self.len += 1;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Registers, memory, time
// ---------------------------------------------------------------------------

/// The controller's register window.
///
/// What it writes is logged, and so is every read made before the first
/// write: the product owner's condition for writing this controller at
/// all (`docs/vendor/google/pixel7/USB-HANDOVER.md` §8) is that each run's record shows
/// the guard's reads and which registers were written. Until the
/// controller runs, every write is logged; after that, only the first
/// write to each register, and not at once: it is queued with its time
/// and said when the driver is next idle ([`say_deferred`]). Said at once,
/// the dozen writes of the host's `SET_CONFIGURATION` cost a console line
/// each while the boot's own checks were still printing, drawn on the
/// phone's screen too, and in one run the answer came after the host's
/// five seconds: it gave up ("can't set config #1, error -110") and there
/// was no port. Logging them all, as the first run did, fed
/// itself once the log went over USB: each line sent is a transfer, and
/// each transfer is five more writes to log. Reads after the first write
/// -- mostly `GEVNTCOUNT`, at each interrupt -- are not logged.
struct Window {
    _mapping: IoMapping<Kernel>,
    base: usize,
    len: usize,
    /// Whether anything has been written yet.
    wrote: bool,
    /// Which registers have been written, a bit per word of the window.
    written: [u64; WINDOW_WORDS / 64],
}

/// Whether the controller runs: from then on, only a register's first
/// write is logged. A flag of the process's, since the controller owns the
/// window once it has started.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// First writes waiting to be said, each its offset in the high half and
/// its value in the low, beside when it was made. The process has one
/// thread, so the order of these atomics is its own.
static DEFERRED: [AtomicU64; DEFERRED_WRITES] = [const { AtomicU64::new(0) }; DEFERRED_WRITES];
static DEFERRED_AT: [AtomicU64; DEFERRED_WRITES] = [const { AtomicU64::new(0) }; DEFERRED_WRITES];
static DEFERRED_LEN: AtomicUsize = AtomicUsize::new(0);

/// How many first writes can wait. The controller has fewer registers the
/// driver writes than this; past it, a write is said at once.
const DEFERRED_WRITES: usize = 64;

/// Queue a first write to be said when the driver is idle, or say it now if
/// the queue is full.
fn defer_write(offset: u32, value: u32) {
    let at = DEFERRED_LEN.load(Ordering::Relaxed);
    match (DEFERRED.get(at), DEFERRED_AT.get(at)) {
        (Some(write), Some(when)) => {
            write.store(
                u64::from(offset) << 32 | u64::from(value),
                Ordering::Relaxed,
            );
            when.store(
                ferrix_rt::linux::monotonic_nanos().unwrap_or(0),
                Ordering::Relaxed,
            );
            DEFERRED_LEN.store(at + 1, Ordering::Relaxed);
        }
        _ => ferrix_rt::debug!("usbdev: write {offset:#06x} = {value:#010x}"),
    }
}

/// Say the first writes the queue holds, with when each was made.
fn say_deferred() {
    let len = DEFERRED_LEN.swap(0, Ordering::Relaxed);
    for (write, when) in DEFERRED.iter().zip(&DEFERRED_AT).take(len) {
        let packed = write.load(Ordering::Relaxed);
        let (offset, value) = (packed >> 32, packed & 0xFFFF_FFFF);
        let ms = when.load(Ordering::Relaxed) / 1_000_000;
        ferrix_rt::debug!("usbdev: write {offset:#06x} = {value:#010x} (at {ms} ms)");
    }
}

/// Words in the controller's 64 KiB window.
const WINDOW_WORDS: usize = 0x1_0000 / 4;

impl Window {
    fn address(&self, offset: u32) -> usize {
        let offset = offset as usize;
        assert!(
            offset + 4 <= self.len && offset.is_multiple_of(4),
            "a register inside the window, aligned"
        );
        self.base + offset
    }
}

impl Registers for Window {
    fn read32(&self, offset: u32) -> u32 {
        // SAFETY: mapped device memory the kernel gave this process, the
        // offset inside it and aligned, read volatile.
        let value = unsafe { ptr::read_volatile(self.address(offset) as *const u32) };
        if !self.wrote {
            ferrix_rt::debug!("usbdev: read  {offset:#06x} = {value:#010x}");
        }
        value
    }

    fn write32(&mut self, offset: u32, value: u32) {
        let word = offset as usize / 4;
        let first = self.written.get_mut(word / 64).is_some_and(|bits| {
            let was = *bits & (1 << (word % 64)) != 0;
            *bits |= 1 << (word % 64);
            !was
        });
        if !RUNNING.load(Ordering::Relaxed) {
            ferrix_rt::debug!("usbdev: write {offset:#06x} = {value:#010x}");
        } else if first {
            defer_write(offset, value);
        }
        self.wrote = true;
        // SAFETY: as for `read32`, and the mapping is writable.
        unsafe { ptr::write_volatile(self.address(offset) as *mut u32, value) }
    }
}

/// The controller's memory: pinned coherent, then mapped.
struct Area {
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
    base: usize,
    addresses: [u64; AREA_PAGES],
}

impl Area {
    fn new(device: &Device<Kernel>) -> Result<Area, Step> {
        let bytes = AREA_PAGES * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        // Pinned before it is mapped: a coherent pin refuses a VMO anything
        // maps, since its mappings are the ones that bypass the caches.
        let pin = device
            .pin(&vmo, 0, bytes, PinAccess::Coherent)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; AREA_PAGES];
        let got = pin.addresses(&mut raw).map_err(|_| Step::Memory)?;
        if !got.is_complete() || got.pages != AREA_PAGES {
            return Err(Step::Memory);
        }
        let mut addresses = [0_u64; AREA_PAGES];
        for (slot, bytes) in addresses.iter_mut().zip(raw.iter()) {
            *slot = device_address(*bytes);
        }
        let base = vmo
            .map(None, bytes, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        Ok(Area {
            _pin: pin,
            _vmo: vmo,
            base,
            addresses,
        })
    }

    fn at(&self, offset: usize, size: usize) -> usize {
        assert!(
            offset + size <= AREA_PAGES * PAGE && offset.is_multiple_of(size),
            "inside the area, aligned"
        );
        self.base + offset
    }
}

impl Dma for Area {
    fn len(&self) -> usize {
        AREA_PAGES * PAGE
    }

    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: a mapping of this process's own pinned VMO that lives as
        // long as `self`; the offset was checked; volatile, since the other
        // side is a device.
        unsafe { ptr::read_volatile(self.at(offset, 4) as *const u32) }
    }

    fn write32(&mut self, offset: usize, value: u32) {
        // SAFETY: as for `read32`, and the mapping is writable.
        unsafe { ptr::write_volatile(self.at(offset, 4) as *mut u32, value) }
    }

    fn read8(&self, offset: usize) -> u8 {
        // SAFETY: as for `read32`.
        unsafe { ptr::read_volatile(self.at(offset, 1) as *const u8) }
    }

    fn write8(&mut self, offset: usize, value: u8) {
        // SAFETY: as for `write32`.
        unsafe { ptr::write_volatile(self.at(offset, 1) as *mut u8, value) }
    }

    fn clean(&mut self, _offset: usize, _len: usize) {
        // Uncached: only the order is owed. The controller is outside the
        // processors' shareability domain, which `dmb ish` orders only
        // within.
        ferrix_rt::device_barrier();
    }

    fn invalidate(&mut self, _offset: usize, _len: usize) {
        ferrix_rt::device_barrier();
    }

    fn device_address(&self, offset: usize) -> Option<u64> {
        let page = self.addresses.get(offset / PAGE)?;
        page.checked_add((offset % PAGE) as u64)
    }
}

/// Time: the monotonic clock, and sleeps on a port nothing is bound to.
struct Time {
    idle: Port<Kernel>,
}

impl Clock for Time {
    fn now_nanos(&self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
    }

    fn sleep_nanos(&mut self, nanos: u64) {
        let until = self.now_nanos().saturating_add(nanos);
        let _ = self.idle.wait(Deadline::At(until));
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

type Usb = Controller<Window, Area, Time>;

fn started(boot: &Channel<Kernel>) -> Result<(Start, Device<Kernel>), Step> {
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Start)?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Start)?;
    if received.handles != 1 {
        return Err(Step::Start);
    }
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));
    match StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(StartMessage::Start(start)) => Ok((start, device)),
        _ => Err(Step::Start),
    }
}

/// Map the controller's window START names.
fn window(start: &Start, device: &Device<Kernel>) -> Result<Window, Step> {
    let len = start.common.length as usize;
    if start.common.offset != 0 || len == 0 || !len.is_multiple_of(PAGE) {
        return Err(Step::Registers);
    }
    let mapping = device
        .io_mapping(IoMappingSpec {
            phys: start.common.phys,
            len: len as u64,
        })
        .map_err(|_| Step::Registers)?;
    let base = mapping.map(None).map_err(|_| Step::Registers)?;
    Ok(Window {
        _mapping: mapping,
        base,
        len,
        wrote: false,
        written: [0; WINDOW_WORDS / 64],
    })
}

fn run(boot: &Channel<Kernel>) -> Result<(), Step> {
    let (start, device) = started(boot)?;
    if start.pci_device_id != TREE_GS201_DWC3 {
        return Err(Step::Identity);
    }
    let registers = window(&start, &device)?;
    let memory = Area::new(&device)?;
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    let interrupt = device.interrupt(0).map_err(|_| Step::Registers)?;
    interrupt
        .bind(&port, KEY_INTERRUPT)
        .map_err(|_| Step::Events)?;
    let clock = Time {
        idle: port::create(Kernel).map_err(|_| Step::Events)?,
    };

    let mut usb = match Controller::start(Parts {
        registers,
        memory,
        clock,
    }) {
        Ok(usb) => {
            RUNNING.store(true, Ordering::Relaxed);
            usb
        }
        Err((error, parts)) => return Err(refused(error, parts)),
    };
    // adb's host reads a payload of exactly the length its header said, and
    // an empty packet after one of whole packets would arrive where it reads
    // the next header (run adbusb3's pull broke on it). The serial port keeps
    // them: its host reads what comes, and an empty packet ends a transfer.
    usb.set_zero_length_packets(ferrix_dwc3::usb_device::acm::ADB_IN, false);
    ferrix_rt::info!("usbdev: DWC3 running, waiting for a host");
    let log = open_log(&device, &port);
    let mut driver = Driver {
        usb,
        serial: SerialPort::new(SERIAL),
        port,
        interrupt,
        log,
        reading: false,
        backlog: [0; MAX_DATA],
        taken: 0,
        filled: 0,
        typed: [0; TYPED_BYTES],
        typed_len: 0,
        adb: adb::Bridge::new(),
    };
    let ended = driver.serve();
    let Driver { mut usb, .. } = driver;
    if usb.stop().is_err() {
        let _kept_for_good = core::mem::ManuallyDrop::new(usb);
        return Err(Step::Wedged);
    }
    drop(usb.into_parts());
    ended
}

/// Say why the controller would not start, and keep its memory if it may
/// still be writing it.
fn refused(error: UsbError, parts: Parts<Window, Area, Time>) -> Step {
    ferrix_rt::err!("usbdev: the controller would not start: {error:?}");
    match error {
        // Nothing was written: the controller is as it was.
        UsbError::Memory | UsbError::NotDwc3(_) | UsbError::Refused { .. } => {
            drop(parts);
            Step::Controller
        }
        // It may be running, and may write its memory.
        _ => {
            let _kept_for_good = core::mem::ManuallyDrop::new(parts);
            Step::Wedged
        }
    }
}

/// The kernel's log, over the channel this controller's node may open, its
/// signals waited for on `port`. `None` when it cannot be had: the port
/// still comes up, and carries nothing.
fn open_log(device: &Device<Kernel>, port: &Port<Kernel>) -> Option<Channel<Kernel>> {
    let log = match device.log_control() {
        Ok(log) => log,
        Err(error) => {
            ferrix_rt::warn!("usbdev: no kernel log to send: {error:?}");
            return None;
        }
    };
    log.wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_LOG)
        .ok()?;
    Some(log)
}

struct Driver {
    usb: Usb,
    serial: SerialPort,
    port: Port<Kernel>,
    interrupt: Interrupt<Kernel>,
    /// The kernel's log, while it can be read.
    log: Option<Channel<Kernel>>,
    /// Whether a READ is out and its DATA not yet come.
    reading: bool,
    /// The last DATA's bytes: `taken..filled` are still to go to the host.
    backlog: [u8; MAX_DATA],
    taken: usize,
    filled: usize,
    /// The line the host is typing, up to [`TYPED_BYTES`] of it.
    typed: [u8; TYPED_BYTES],
    typed_len: usize,
    /// adb's interface, bridged to adbd.
    adb: adb::Bridge,
}

impl Driver {
    /// Serve interrupts and the log for as long as the controller runs.
    fn serve(&mut self) -> Result<(), Step> {
        loop {
            self.pump()?;
            self.adb.pump(&mut self.usb)?;
            // adb waits for an OKAY after every message, and the socket
            // has no way to wake the port, so while adbd is here the loop
            // looks at it often.
            let tick = if self.adb.connected() {
                ADB_TICK_NANOS
            } else {
                TICK_NANOS
            };
            let now = self.usb.clock().now_nanos();
            match self.port.wait(Deadline::At(now.saturating_add(tick))) {
                Ok(packet) if packet.kind == PACKET_INTERRUPT && packet.key == KEY_INTERRUPT => {
                    let serviced = self.usb.on_interrupt(&mut self.serial);
                    let _ = self.interrupt.ack();
                    match serviced {
                        Ok(notice) => self.act(notice)?,
                        Err(error) => {
                            ferrix_rt::err!("usbdev: {error:?}");
                            return Err(Step::Faulted);
                        }
                    }
                }
                Ok(packet) if packet.kind == PACKET_SIGNAL && packet.key == KEY_LOG => {
                    self.take_log();
                }
                // Idle: what was written while the host waited can be said.
                Err(Error::TimedOut) => say_deferred(),
                Ok(_) => {}
                Err(_) => return Err(Step::Events),
            }
        }
    }

    /// Act on what an interrupt found.
    fn act(&mut self, notice: ferrix_dwc3::Notice) -> Result<(), Step> {
        if let Some(speed) = notice.connected {
            ferrix_rt::info!("usbdev: connected at {speed:?} speed");
        }
        if notice.configuration {
            ferrix_rt::info!(
                "usbdev: {}",
                if self.serial.is_configured() {
                    "configured: the host sees a serial port"
                } else {
                    "unconfigured"
                }
            );
        }
        if notice.disconnected {
            ferrix_rt::info!("usbdev: the host went away");
        }
        if notice.reset || notice.disconnected {
            self.adb.reset();
        }
        if notice.received {
            let mut packet = [0_u8; 512];
            let count = self
                .usb
                .read(ADB_OUT, &mut packet)
                .map_err(|_| Step::Faulted)?;
            if let Some(bytes) = packet.get(..count)
                && !bytes.is_empty()
            {
                self.adb.take_from_host(bytes);
            }
            let mut bytes = [0_u8; 512];
            let count = self
                .usb
                .read(DATA_OUT, &mut bytes)
                .map_err(|_| Step::Faulted)?;
            if self.typed(bytes.get(..count).unwrap_or_default()) {
                ferrix_rt::notice!("usbdev: the host asked for a restart");
                // Returns only if the kernel refused: the port stays up.
                if let Err(error) = ferrix_rt::linux::reboot_restart() {
                    ferrix_rt::err!("usbdev: the restart was refused: {error:?}");
                }
            }
        }
        Ok(())
    }

    /// Take what the host typed, a line at a time, and say whether a line
    /// was [`REBOOT`]. Everything else goes nowhere.
    fn typed(&mut self, bytes: &[u8]) -> bool {
        let mut asked = false;
        for &byte in bytes {
            if byte == b'\n' {
                let line = self.typed.get(..self.typed_len).unwrap_or_default();
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                asked |= line == REBOOT;
                self.typed_len = 0;
            } else if let Some(slot) = self.typed.get_mut(self.typed_len) {
                *slot = byte;
                self.typed_len += 1;
            }
            // Past TYPED_BYTES the rest of the line is dropped: a line that
            // long cannot be the command.
        }
        asked
    }

    /// Move the log towards the host: what is left of the last DATA into
    /// the controller's ring, and once that is empty, the next READ. Only
    /// while a program on the host has the port open.
    fn pump(&mut self) -> Result<(), Step> {
        if !self.serial.is_configured() || !self.serial.dtr() {
            return Ok(());
        }
        if let Some(rest) = self.backlog.get(self.taken..self.filled)
            && !rest.is_empty()
        {
            let taken = self.usb.write(DATA_IN, rest).map_err(|_| Step::Faulted)?;
            self.taken += taken;
        }
        if self.taken < self.filled || self.reading {
            return Ok(());
        }
        let Some(log) = &self.log else {
            return Ok(());
        };
        let mut bytes = [0_u8; 16];
        let read = LogMessage::Read {
            max: MAX_DATA as u32,
        };
        let sent = read
            .encode_into(&mut bytes)
            .ok()
            .and_then(|len| bytes.get(..len))
            .is_some_and(|message| log.write(message).is_ok());
        if sent {
            self.reading = true;
        } else {
            self.lose_log("the log channel refused a READ");
        }
        Ok(())
    }

    /// Take what the log channel says: DATA into the backlog, anything
    /// else, or its closing, the end of the log.
    fn take_log(&mut self) {
        let Some(log) = &self.log else {
            return;
        };
        let mut bytes = [0_u8; MAX_BYTES];
        let mut handles = [Handle::INVALID; 1];
        let received = match log.read(&mut bytes, &mut handles) {
            Ok(received) => received,
            Err(ReadError::Failed(Error::ShouldWait)) => {
                self.rearm();
                return;
            }
            Err(_) => {
                self.lose_log("the kernel closed the log channel");
                return;
            }
        };
        match LogMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
            Ok(LogMessage::Data { lost, bytes: data }) => {
                let count = data.len().min(self.backlog.len());
                if let (Some(into), Some(from)) = (self.backlog.get_mut(..count), data.get(..count))
                {
                    into.copy_from_slice(from);
                }
                self.taken = 0;
                self.filled = count;
                self.reading = false;
                if lost > 0 {
                    self.tell_lost(lost);
                }
                self.rearm();
            }
            _ => self.lose_log("the kernel refused the log's reader"),
        }
    }

    /// Say on the port how much of the log went before it could be sent.
    fn tell_lost(&mut self, lost: u64) {
        let mut line = Line::default();
        let _ = writeln!(
            line,
            "usbdev: {lost} bytes of the log were lost before this"
        );
        // Best effort: if the ring has no room, the gap goes unsaid here,
        // and the console record still has the lines.
        let _taken = self.usb.write(DATA_IN, line.as_bytes());
    }

    /// Wait for the log channel's next signal.
    fn rearm(&mut self) {
        let armed = self.log.as_ref().is_some_and(|log| {
            log.wait_async(
                &self.port,
                Signals::READABLE | Signals::PEER_CLOSED,
                KEY_LOG,
            )
            .is_ok()
        });
        if !armed {
            self.lose_log("the log channel could not be waited on");
        }
    }

    /// Stop reading the log, saying why; the port stays up.
    fn lose_log(&mut self, why: &str) {
        if self.log.take().is_some() {
            ferrix_rt::warn!("usbdev: {why}; the port carries no more of the log");
        }
        self.reading = false;
    }
}
