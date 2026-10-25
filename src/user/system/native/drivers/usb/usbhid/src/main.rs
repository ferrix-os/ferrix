//! The USB host driver process: a ring-3 program that finds the keyboards
//! and mice behind a USB host controller and serves each to the kernel's
//! input core (`docs/INPUT.md` §7). The controller is an STM32MP15 board's
//! EHCI, which the kernel publishes as a device tree node, or a PC's xHCI,
//! a PCI function of class 0C:03:30 (`docs/XHCI.md`).
//!
//! Everything that knows anything about USB is `ferrix-usb-host`, tested on
//! the host against a model of each controller and a bus behind it. This
//! program is the handles it was written to be wrapped in:
//!
//! 1. START, as a bus host's: only the device. An EHCI node's common window
//!    is its registers; an xHCI function's are the apertures of its first
//!    BAR, which the kernel cuts around the MSI-X table. devmgr does not
//!    wait for it.
//! 2. The controller's memory is one VMO pinned with `PIN_COHERENT` before
//!    it is mapped: where a controller does not snoop the caches, the
//!    kernel maps it past them, and it is seen alike from both sides. An
//!    xHCI controller says how much it needs, and takes 64-bit addresses.
//! 3. One port carries every event: the controller's interrupt, and each
//!    input control channel becoming readable. The wait on it ends at the
//!    next port poll, which is when keyboards and mice are found and lost.
//! 4. Each keyboard or mouse found gets an input control channel of its own
//!    (`device.input_control()`, which a USB host's node allows up to eight
//!    times), a HELLO, and its reports as EVENTS; one unplugged has its
//!    channel closed, which the core hears as the device going.
//!
//! What it finds is said on standard error, which is the console.
//!
//! With the debug switch `ferrix.usbhid.reports` on the kernel command line,
//! or the file `/run/usbhid-reports` there, every interrupt report of the
//! functions it names is said raw at debug, with the time it was taken and
//! the events it made (`ferrix_usb_host::watch`, `docs/XHCI.md` §13).
//!
//! The exit status names the step that failed ([`Step`]).

#![no_std]
#![no_main]

use core::fmt;
use core::ptr;

use ferrix_blkring::control::{Message as StartMessage, START_BYTES, Start};
use ferrix_inputctl::message::{Events, MAX_BYTES, Message, PORT_RIGHTS};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{
    APERTURE_WHOLE_PAGES, DEVICE_NOT_PCI, IoMappingSpec, PACKET_INTERRUPT, PACKET_SIGNAL,
    PCI_CLASS_XHCI, TREE_STM32_USBH,
};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::device::{Device, Interrupt, IoMapping};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_usb_host::bus::{
    Bus, DiskChange, MAX_FUNCTIONS, Note, Output, SERVICE_POLL, SERVICE_POLL_IDLE_MAX, Watched,
    next_service_poll,
};
use ferrix_usb_host::hid::Kind;
use ferrix_usb_host::host::{self, Host as Controller};
use ferrix_usb_host::watch::{self, Watch};
use ferrix_usb_host::{Clock, Dma, MILLISECOND, Parts, Registers, ehci, xhci};

ferrix_rt::entry!(main);

/// A page, on every architecture this runs on.
const PAGE: usize = 4096;
/// The most pages a controller's memory is: what an xHCI controller that
/// wants every scratchpad page asks for.
const MAX_AREA_PAGES: usize = xhci::MAX_AREA_PAGES;
/// The most apertures a controller's registers lie in: a BAR, cut in two
/// around the MSI-X table and again around its pending bits.
const MAX_WINDOWS: usize = 4;
/// How long an xHCI controller's ports are given to see what is plugged in
/// before the first poll: USB 2.0's debounce interval.
const XHCI_SETTLE: u64 = 100 * MILLISECOND;

/// Port keys: the interrupt, then one per function's control channel.
const KEY_INTERRUPT: u64 = 1;
const KEY_FIRST_CONTROL: u64 = 16;

/// What the bootstrap channel is told once the first enumeration settled:
/// devmgr reads only that something came.
const SETTLED: &[u8] = b"settled";

/// How long the core has to answer a HELLO.
const READY_PATIENCE: u64 = 2_000_000_000;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device that is not a USB host.
    Identity = 2,
    /// The registers could not be mapped.
    Registers = 3,
    /// Memory could not be made, pinned or mapped.
    Memory = 4,
    /// The controller would not come up.
    Controller = 5,
    /// The port, the interrupt or the waits could not be arranged.
    Events = 7,
    /// The controller halted with a system error.
    Faulted = 8,
    /// The controller would not halt: its memory is kept.
    Wedged = 9,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(()) => 0,
        Err(step) => {
            ferrix_rt::err!("usbhid: stopped at {step:?}");
            step as i32
        }
    }
}

// ---------------------------------------------------------------------------
// The console
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Registers, memory, time
// ---------------------------------------------------------------------------

/// One mapped run of the controller's registers.
struct Part {
    _mapping: IoMapping<Kernel>,
    base: usize,
    /// Where it starts among the registers, and how long it is.
    offset: usize,
    len: usize,
}

/// The controller's registers: one page for EHCI, and for xHCI the
/// apertures of its BAR, whose gaps -- the MSI-X table, which is the
/// kernel's -- read as ones and take no write.
struct Window {
    parts: [Option<Part>; MAX_WINDOWS],
}

impl Window {
    /// Map `len` bytes at `phys` as the registers from `offset` on.
    fn add(&mut self, device: &Device<Kernel>, phys: u64, offset: usize, len: usize) -> bool {
        let Some(slot) = self.parts.iter_mut().find(|part| part.is_none()) else {
            return false;
        };
        let Ok(mapping) = device.io_mapping(IoMappingSpec {
            phys,
            len: len as u64,
        }) else {
            return false;
        };
        let Ok(base) = mapping.map(None) else {
            return false;
        };
        *slot = Some(Part {
            _mapping: mapping,
            base,
            offset,
            len,
        });
        true
    }

    fn address(&self, offset: u32) -> Option<usize> {
        let offset = offset as usize;
        if !offset.is_multiple_of(4) {
            return None;
        }
        self.parts.iter().flatten().find_map(|part| {
            let within = offset.checked_sub(part.offset)?;
            (within + 4 <= part.len).then_some(part.base + within)
        })
    }
}

impl Registers for Window {
    fn read32(&self, offset: u32) -> u32 {
        let Some(address) = self.address(offset) else {
            return u32::MAX;
        };
        // SAFETY: mapped device memory the kernel gave this process, the
        // offset inside one mapping and aligned, read volatile.
        unsafe { ptr::read_volatile(address as *const u32) }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        let Some(address) = self.address(offset) else {
            return;
        };
        // SAFETY: as for `read32`, and the mapping is writable.
        unsafe { ptr::write_volatile(address as *mut u32, value) }
    }
}

/// The controller's memory: pinned coherent, then mapped.
struct Area {
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
    base: usize,
    bytes: usize,
    addresses: [u64; MAX_AREA_PAGES],
    /// Whether an access outside the area was said.
    strayed: core::cell::Cell<bool>,
}

impl Area {
    fn new(device: &Device<Kernel>, bytes: usize) -> Result<Area, Step> {
        let pages = bytes / PAGE;
        if bytes == 0 || !bytes.is_multiple_of(PAGE) || pages > MAX_AREA_PAGES {
            return Err(Step::Memory);
        }
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        // Pinned before it is mapped: a coherent pin refuses a VMO anything
        // maps, since its mappings are the ones that bypass the caches.
        let pin = device
            .pin(&vmo, 0, bytes, PinAccess::Coherent)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; MAX_AREA_PAGES];
        let got = pin.addresses(&mut raw).map_err(|_| Step::Memory)?;
        if !got.is_complete() || got.pages != pages {
            return Err(Step::Memory);
        }
        let mut addresses = [0_u64; MAX_AREA_PAGES];
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
            bytes,
            addresses,
            strayed: core::cell::Cell::new(false),
        })
    }

    /// Where `size` bytes at `offset` are mapped; `None` outside the area
    /// or unaligned, which is said once and then read as zero and not
    /// written, rather than ending the driver.
    fn at(&self, offset: usize, size: usize) -> Option<usize> {
        let inside = offset
            .checked_add(size)
            .is_some_and(|end| end <= self.bytes)
            && offset.is_multiple_of(size);
        if inside {
            Some(self.base + offset)
        } else {
            if !self.strayed.replace(true) {
                ferrix_rt::warn!(
                    "usbhid: an access of {size} bytes at {offset:#x} is outside the controller's memory; ignored"
                );
            }
            None
        }
    }
}

impl Dma for Area {
    fn len(&self) -> usize {
        self.bytes
    }

    fn read32(&self, offset: usize) -> u32 {
        let Some(at) = self.at(offset, 4) else {
            return 0;
        };
        // SAFETY: a mapping of this process's own pinned VMO that lives as
        // long as `self`; the offset was checked; volatile, since the other
        // side is a device.
        unsafe { ptr::read_volatile(at as *const u32) }
    }

    fn write32(&mut self, offset: usize, value: u32) {
        if let Some(at) = self.at(offset, 4) {
            // SAFETY: as for `read32`, and the mapping is writable.
            unsafe { ptr::write_volatile(at as *mut u32, value) }
        }
    }

    fn read8(&self, offset: usize) -> u8 {
        let Some(at) = self.at(offset, 1) else {
            return 0;
        };
        // SAFETY: as for `read32`.
        unsafe { ptr::read_volatile(at as *const u8) }
    }

    fn write8(&mut self, offset: usize, value: u8) {
        if let Some(at) = self.at(offset, 1) {
            // SAFETY: as for `write32`.
            unsafe { ptr::write_volatile(at as *mut u8, value) }
        }
    }

    fn device_address(&self, offset: usize) -> Option<u32> {
        u32::try_from(self.wide_address(offset)?).ok()
    }

    fn wide_address(&self, offset: usize) -> Option<u64> {
        if offset >= self.bytes {
            return None;
        }
        let page = self.addresses.get(offset / PAGE)?;
        Some(page + (offset % PAGE) as u64)
    }

    fn barrier(&self) {
        // The controller is outside the processors' shareability domain,
        // which `dmb ish` orders only within.
        ferrix_rt::device_barrier();
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

type Ehci = ehci::Controller<Window, Area, Time>;
type Xhci = xhci::Controller<Window, Area, Time>;

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

/// An EHCI node's registers: START's common window, whole pages from its
/// first byte.
fn tree_window(start: &Start, device: &Device<Kernel>) -> Result<Window, Step> {
    let len = start.common.length as usize;
    if start.common.offset != 0 || len == 0 || !len.is_multiple_of(PAGE) {
        return Err(Step::Registers);
    }
    let mut window = Window {
        parts: [const { None }; MAX_WINDOWS],
    };
    if window.add(device, start.common.phys, 0, len) {
        Ok(window)
    } else {
        Err(Step::Registers)
    }
}

/// An xHCI function's registers: every aperture of its first BAR, each at
/// its place in the BAR. The first must start the BAR, since that is where
/// the capability registers are.
fn bar_window(device: &Device<Kernel>) -> Result<Window, Step> {
    let info = device.info().map_err(|_| Step::Identity)?;
    if info.class != PCI_CLASS_XHCI {
        return Err(Step::Identity);
    }
    let mut window = Window {
        parts: [const { None }; MAX_WINDOWS],
    };
    let mut starts = false;
    for index in 0..info.apertures as usize {
        let Ok(aperture) = device.aperture(index) else {
            continue;
        };
        if aperture.bar != 0 || aperture.flags & APERTURE_WHOLE_PAGES == 0 {
            continue;
        }
        let (Ok(offset), Ok(len)) = (
            usize::try_from(aperture.offset),
            usize::try_from(aperture.len),
        ) else {
            continue;
        };
        if window.add(device, aperture.phys, offset, len) {
            starts |= offset == 0;
        }
    }
    if starts {
        Ok(window)
    } else {
        Err(Step::Registers)
    }
}

/// The port, the interrupt bound to it, and the clock: what every
/// controller's driver waits on.
fn events(device: &Device<Kernel>) -> Result<(Port<Kernel>, Interrupt<Kernel>, Time), Step> {
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    let interrupt = device.interrupt(0).map_err(|_| Step::Registers)?;
    interrupt
        .bind(&port, KEY_INTERRUPT)
        .map_err(|_| Step::Events)?;
    let clock = Time {
        idle: port::create(Kernel).map_err(|_| Step::Events)?,
    };
    Ok((port, interrupt, clock))
}

fn run(boot: &Channel<Kernel>) -> Result<(), Step> {
    let (start, device) = started(boot)?;
    if start.location == DEVICE_NOT_PCI {
        if start.pci_device_id != TREE_STM32_USBH {
            return Err(Step::Identity);
        }
        let registers = tree_window(&start, &device)?;
        let memory = Area::new(&device, ehci::AREA_BYTES)?;
        let (port, interrupt, clock) = events(&device)?;
        let controller = Ehci::start(Parts {
            registers,
            memory,
            clock,
        });
        serve_on(controller, "EHCI", 0, boot, device, port, interrupt, &start)
    } else {
        let registers = bar_window(&device)?;
        let bytes = xhci::area_bytes(&registers).ok_or(Step::Memory)?;
        let memory = Area::new(&device, bytes)?;
        let (port, interrupt, clock) = events(&device)?;
        let controller = Xhci::start(Parts {
            registers,
            memory,
            clock,
        });
        serve_on(
            controller,
            "xHCI",
            XHCI_SETTLE,
            boot,
            device,
            port,
            interrupt,
            &start,
        )
    }
}

/// Serve the bus behind a controller that started, until it stops; then
/// shut it down, keeping its memory for good if it will not halt.
#[expect(
    clippy::too_many_arguments,
    reason = "the handles a driver is made of, each used once"
)]
fn serve_on<H>(
    controller: Result<H, (host::Error, Parts<Window, Area, Time>)>,
    name: &str,
    settle: u64,
    boot: &Channel<Kernel>,
    device: Device<Kernel>,
    port: Port<Kernel>,
    interrupt: Interrupt<Kernel>,
    start: &Start,
) -> Result<(), Step>
where
    H: Controller<Clock = Time, Parts = Parts<Window, Area, Time>>,
{
    let controller = match controller {
        Ok(controller) => controller,
        Err((error, parts)) => {
            ferrix_rt::err!("usbhid: the {name} controller would not start: {error:?}");
            // Unresponsive may mean still running: its memory stays.
            if matches!(error, host::Error::Unresponsive) {
                let _kept_for_good = core::mem::ManuallyDrop::new(parts);
                return Err(Step::Wedged);
            }
            return Err(Step::Controller);
        }
    };
    ferrix_rt::info!("usbhid: {name} running");
    let booted = boot_watch();
    let mut bus = Bus::with(controller);
    bus.watch(booted);
    let mut driver = Driver {
        bus,
        booted,
        last_report: [0; MAX_FUNCTIONS],
        device,
        port,
        interrupt,
        location: start.location,
        channels: [const { None }; MAX_FUNCTIONS],
    };
    let ended = driver.serve(boot, settle);
    let Driver { bus, .. } = driver;
    match bus.shutdown() {
        Ok(parts) => {
            drop(parts);
            ended
        }
        Err(parts) => {
            let _kept_for_good = core::mem::ManuallyDrop::new(parts);
            Err(Step::Wedged)
        }
    }
}

struct Driver<H> {
    bus: Bus<H>,
    device: Device<Kernel>,
    port: Port<Kernel>,
    interrupt: Interrupt<Kernel>,
    /// START's location word, which each HELLO repeats.
    location: u32,
    /// Each function's input control channel, once the core took it.
    channels: [Option<Channel<Kernel>>; MAX_FUNCTIONS],
    /// What the boot word asked to be watched, which holds while
    /// `/run/usbhid-reports` is not there.
    booted: Watch,
    /// When each function's last watched report was taken.
    last_report: [u64; MAX_FUNCTIONS],
}

impl<H: Controller<Clock = Time>> Driver<H> {
    /// Poll, forward, and wait, for as long as the controller runs.
    ///
    /// Once what was plugged in at boot is found and introduced, the
    /// bootstrap channel is told: devmgr waits for that before its REPORT,
    /// which the kernel starts init at, so a compositor that reads
    /// `/dev/input` once finds the keyboard (`docs/INPUT.md` §7.3).
    fn serve(&mut self, boot: &Channel<Kernel>, settle: u64) -> Result<(), Step> {
        if settle > 0 {
            let until = ferrix_rt::linux::monotonic_nanos()
                .unwrap_or(0)
                .saturating_add(settle);
            let _ = self.port.wait(Deadline::At(until));
        }
        self.poll();
        let _ = self.forward();
        let _ = boot.write(SETTLED);
        // Until the controller's interrupt has been seen to arrive, its
        // events are also taken every SERVICE_POLL: on the N150 box no MSI
        // came, the eight queued interrupt requests filled, and the keyboard
        // reset itself after ten reports it could not deliver.
        // The service backs off from 8 ms to 32 ms while it finds nothing,
        // and is back at 8 ms as soon as anything moves (p2-138).
        let mut interrupted = false;
        let mut said = false;
        let mut service_every = SERVICE_POLL;
        loop {
            if self.forward() {
                service_every = SERVICE_POLL;
            }
            let now = ferrix_rt::linux::monotonic_nanos().unwrap_or(0);
            let next_poll = self.bus.next_poll();
            let deadline = if interrupted {
                next_poll
            } else {
                next_poll.min(now.saturating_add(service_every))
            };
            match self.port.wait(Deadline::At(deadline)) {
                Ok(packet) if packet.kind == PACKET_INTERRUPT && packet.key == KEY_INTERRUPT => {
                    interrupted = true;
                    let serviced = self.bus.on_interrupt();
                    let _ = self.interrupt.ack();
                    if serviced.is_err() {
                        return Err(Step::Faulted);
                    }
                }
                Err(Error::TimedOut) if !interrupted => {
                    if !said {
                        said = true;
                        ferrix_rt::notice!(
                            "usbhid: no interrupt from the controller yet: its events are taken every {} to {} ms until one comes",
                            SERVICE_POLL / 1_000_000,
                            SERVICE_POLL_IDLE_MAX / 1_000_000
                        );
                    }
                    if self.bus.on_interrupt().is_err() {
                        return Err(Step::Faulted);
                    }
                    // Doubled here; the next forward puts it back at 8 ms
                    // if this service found anything.
                    service_every = next_service_poll(service_every, false);
                    if ferrix_rt::linux::monotonic_nanos().unwrap_or(0) >= next_poll {
                        self.poll();
                    }
                }
                Ok(packet) if packet.kind == PACKET_SIGNAL && packet.key >= KEY_FIRST_CONTROL => {
                    let function =
                        usize::try_from(packet.key - KEY_FIRST_CONTROL).unwrap_or(usize::MAX);
                    self.take_control(function);
                }
                Ok(_) => {}
                Err(Error::TimedOut) => self.poll(),
                Err(_) => return Err(Step::Events),
            }
        }
    }

    /// Poll the ports, and take what `/run/usbhid-reports` says should be
    /// watched now.
    fn poll(&mut self) {
        self.bus.poll();
        let now = file_watch().unwrap_or(self.booted);
        if now != self.bus.watching() {
            ferrix_rt::notice!("usbhid: watching reports: {now}");
            self.bus.watch(now);
        }
    }

    /// Say a watched report: its bytes, the time it was taken and since its
    /// function's last, and what was lost before it.
    fn say_watched(&mut self, report: &Watched) {
        let Some(last) = self.last_report.get_mut(report.function) else {
            return;
        };
        let since = report.at.saturating_sub(*last);
        let first = *last == 0;
        *last = report.at;
        let mut hex = Hex::default();
        for byte in report.as_bytes() {
            hex.push(*byte);
        }
        let micros = report.at / 1_000;
        ferrix_rt::debug!(
            "usbhid: report {:04x}:{:04x} function {} at {}.{:06} s (+{} us){}: {}{}",
            report.vendor,
            report.product,
            report.function,
            micros / 1_000_000,
            micros % 1_000_000,
            if first { 0 } else { since / 1_000 },
            if first { ", its first" } else { "" },
            hex.as_str(),
            Lost(report.lost, report.unsaid),
        );
    }

    /// Say the events the watched report before them made.
    fn say_events(&self, function: usize) {
        let mut text = Text::default();
        for event in self.bus.events() {
            let _ = fmt::Write::write_fmt(
                &mut text,
                format_args!(" {}:{}={}", event.kind, event.code, event.value),
            );
        }
        ferrix_rt::debug!("usbhid: function {function} made{}", text.as_str());
    }

    /// Hand on everything the bus has, and say what it found. Answers
    /// whether there was anything.
    fn forward(&mut self) -> bool {
        let mut moved = false;
        while let Some(note) = self.bus.next_note() {
            moved = true;
            tell(note);
        }
        while let Some(change) = self.bus.next_disk_change() {
            moved = true;
            self.tell_disk(change);
        }
        while let Some(output) = self.bus.next_output() {
            moved = true;
            let mut said_report = None;
            while let Some(report) = self.bus.next_watched() {
                said_report = Some(report.function);
                self.say_watched(&report);
            }
            if let Output::Events(function) = output
                && said_report == Some(function)
            {
                self.say_events(function);
            }
            match output {
                Output::Attached(function) => self.introduce(function),
                Output::Events(function) => {
                    let channel = self.channels.get(function).and_then(Option::as_ref);
                    if let (Some(channel), Some(events)) = (channel, Events::new(self.bus.events()))
                    {
                        let _ = channel.write(Message::Events(events).encode().as_bytes());
                    }
                }
                Output::Detached(function) => {
                    // Closing the channel is how the core hears it went.
                    if let Some(slot) = self.channels.get_mut(function) {
                        *slot = None;
                    }
                    ferrix_rt::info!("usbhid: function {function} unplugged");
                }
            }
        }
        // Reports that made no events come out after the last output.
        while let Some(report) = self.bus.next_watched() {
            self.say_watched(&report);
        }
        moved
    }

    /// Say what a disk that came is, with its first block read as proof the
    /// bulk pipes carry data; a disk is not served to the block core yet
    /// (`docs/XHCI.md` §6).
    fn tell_disk(&mut self, change: DiskChange) {
        let disk = match change {
            DiskChange::Detached(disk) => {
                ferrix_rt::info!("usbhid: disk {disk} unplugged");
                return;
            }
            DiskChange::Attached(disk) => disk,
        };
        let Some(found) = self.bus.disk(disk).copied() else {
            return;
        };
        let vendor = core::str::from_utf8(&found.vendor)
            .unwrap_or("?")
            .trim_end();
        let product = core::str::from_utf8(&found.product)
            .unwrap_or("?")
            .trim_end();
        let mut block = [0_u8; 4096];
        let Some(first) = block.get_mut(..found.block_bytes as usize) else {
            return;
        };
        match self.bus.disk_read(disk, 0, first) {
            Ok(()) => {
                // FNV-1a over the block: a line a gate can hold against the
                // image it attached.
                let sum = first.iter().fold(0xCBF2_9CE4_8422_2325_u64, |sum, &byte| {
                    (sum ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
                });
                ferrix_rt::info!(
                    "usbhid: disk {disk} {vendor} {product}: {} blocks of {} bytes, block 0 fnv {sum:016x}",
                    found.blocks,
                    found.block_bytes
                );
            }
            Err(error) => {
                ferrix_rt::err!("usbhid: disk {disk} {vendor} {product} would not read: {error:?}");
            }
        }
    }

    /// Give `function` an input control channel and introduce it; a function
    /// the core will not take is left without one, and its reports go
    /// nowhere.
    fn introduce(&mut self, function: usize) {
        let Some(hello) = self.bus.hello(function, self.location) else {
            return;
        };
        let kind = self.bus.kind(function).map_or("input", Kind::word);
        let name = core::str::from_utf8(hello.name.as_bytes()).unwrap_or("?");
        let Ok(control) = self.device.input_control() else {
            ferrix_rt::warn!("usbhid: no input channel for the {kind} {name}");
            return;
        };
        let Ok(share) = self
            .port
            .as_owned()
            .duplicate(Requested::Exactly(PORT_RIGHTS))
        else {
            return;
        };
        if control
            .write_with(Message::Hello(hello).encode().as_bytes(), [share])
            .is_err()
        {
            return;
        }
        match ready(&control) {
            Some(node) => {
                ferrix_rt::info!("usbhid: {kind} {name} is event{node}");
                let key = KEY_FIRST_CONTROL + function as u64;
                if control
                    .wait_async(&self.port, Signals::READABLE | Signals::PEER_CLOSED, key)
                    .is_ok()
                    && let Some(slot) = self.channels.get_mut(function)
                {
                    *slot = Some(control);
                }
            }
            None => ferrix_rt::warn!("usbhid: the core refused the {kind} {name}"),
        }
    }

    /// Read what the core said on `function`'s channel: STATUS lights the
    /// device's LEDs, STOP is answered and the channel closed, and the core
    /// going away closes it too.
    fn take_control(&mut self, function: usize) {
        let Some(Some(channel)) = self.channels.get(function) else {
            return;
        };
        let mut bytes = [0_u8; MAX_BYTES];
        let mut handles = [Handle::INVALID; 1];
        let keep = match channel.read(&mut bytes, &mut handles) {
            Ok(received) => {
                for handle in handles.iter().take(received.handles) {
                    drop(OwnedHandle::from_raw(Kernel, *handle));
                }
                match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
                    Ok(Message::Stop) => {
                        let _ = channel.write(Message::Stopped.encode().as_bytes());
                        false
                    }
                    Ok(Message::Status(leds)) => {
                        if let Err(error) = self.bus.set_leds(function, leds.as_slice()) {
                            ferrix_rt::warn!(
                                "usbhid: function {function}'s LEDs were not set: {error:?}"
                            );
                        }
                        true
                    }
                    _ => true,
                }
            }
            Err(ReadError::Failed(Error::ShouldWait)) => true,
            Err(_) => false,
        };
        let key = KEY_FIRST_CONTROL + function as u64;
        let armed = keep
            && channel
                .wait_async(&self.port, Signals::READABLE | Signals::PEER_CLOSED, key)
                .is_ok();
        if !armed && let Some(slot) = self.channels.get_mut(function) {
            *slot = None;
        }
    }
}

/// The debug switch's boot word (`ferrix_usb_host::watch`).
const WATCH_WORD: &[u8] = b"ferrix.usbhid.reports";

/// What the boot word asks to be watched: off when `/proc/cmdline` cannot
/// be read, which is said.
fn boot_watch() -> Watch {
    let mut line = [0_u8; 2048];
    match read_file(b"/proc/cmdline\0", &mut line) {
        Ok(len) => {
            let watch = Watch::from_command_line(line.get(..len).unwrap_or_default(), WATCH_WORD);
            if watch.is_on() {
                ferrix_rt::notice!("usbhid: ferrix.usbhid.reports: watching reports: {watch}");
            }
            watch
        }
        Err(errno) => {
            ferrix_rt::warn!(
                "usbhid: /proc/cmdline unreadable ({errno:?}): ferrix.usbhid.reports is off"
            );
            Watch::OFF
        }
    }
}

/// What `/run/usbhid-reports` asks to be watched, while it is there; a
/// value it cannot read watches nothing.
fn file_watch() -> Option<Watch> {
    let mut bytes = [0_u8; 64];
    let len = read_file(watch::FILE, &mut bytes).ok()?;
    Some(Watch::parse(bytes.get(..len).unwrap_or_default()).unwrap_or(Watch::OFF))
}

/// A small file from its start into `buf` (`path` NUL-terminated): its
/// length.
fn read_file(path: &[u8], buf: &mut [u8]) -> Result<usize, ferrix_linux_abi::errno::Errno> {
    use ferrix_rt::linux::{self, numbers};
    const AT_FDCWD: usize = (-100_isize).cast_unsigned();
    const O_RDONLY: usize = 0;
    const O_CLOEXEC: usize = 0o2_000_000;
    if path.last() != Some(&0) {
        return Err(ferrix_linux_abi::errno::Errno::EINVAL);
    }
    // SAFETY: `path` is NUL-terminated and borrowed for the call.
    let fd = unsafe {
        linux::call(
            numbers::OPENAT,
            [
                AT_FDCWD,
                path.as_ptr().addr(),
                O_RDONLY | O_CLOEXEC,
                0,
                0,
                0,
            ],
        )
    }?;
    let mut len = 0;
    while let Some(rest) = buf.get_mut(len..) {
        if rest.is_empty() {
            break;
        }
        match linux::read(fd, rest) {
            Ok(0) => break,
            Ok(got) => len += got,
            Err(errno) => {
                let _ = linux::close(fd);
                return Err(errno);
            }
        }
    }
    let _ = linux::close(fd);
    Ok(len)
}

/// A report's bytes in hex, a space between each.
struct Hex {
    bytes: [u8; 3 * host::PIPE_BYTES],
    len: usize,
}

impl Default for Hex {
    fn default() -> Self {
        Hex {
            bytes: [0; 3 * host::PIPE_BYTES],
            len: 0,
        }
    }
}

impl Hex {
    fn push(&mut self, byte: u8) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let high = DIGITS.get(usize::from(byte >> 4)).copied().unwrap_or(b'?');
        let low = DIGITS.get(usize::from(byte & 0xF)).copied().unwrap_or(b'?');
        let space = (self.len > 0).then_some(b' ');
        for digit in space.into_iter().chain([high, low]) {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = digit;
                self.len += 1;
            }
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(self.bytes.get(..self.len).unwrap_or_default()).unwrap_or("?")
    }
}

/// Up to a line of text, cut at its end.
struct Text {
    bytes: [u8; 400],
    len: usize,
}

impl Default for Text {
    fn default() -> Self {
        Text {
            bytes: [0; 400],
            len: 0,
        }
    }
}

impl Text {
    fn as_str(&self) -> &str {
        core::str::from_utf8(self.bytes.get(..self.len).unwrap_or_default()).unwrap_or("?")
    }
}

impl fmt::Write for Text {
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

/// What was dropped before a watched report, if anything.
struct Lost(u32, u32);

impl fmt::Display for Lost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 > 0 {
            write!(f, " ({} reports of any function dropped before it)", self.0)?;
        }
        if self.1 > 0 {
            write!(f, " ({} watched reports not said)", self.1)?;
        }
        Ok(())
    }
}

/// Wait for the core's answer to HELLO: the node's number on READY.
fn ready(control: &Channel<Kernel>) -> Option<u32> {
    let until = ferrix_rt::linux::monotonic_nanos()
        .ok()?
        .saturating_add(READY_PATIENCE);
    let _ = control
        .wait_one(Signals::READABLE, Deadline::At(until))
        .ok()?;
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = control.read(&mut bytes, &mut handles).ok()?;
    for handle in handles.iter().take(received.handles) {
        // The core's port is for the LED row, later.
        drop(OwnedHandle::from_raw(Kernel, *handle));
    }
    match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(Message::Ready(ready)) => Some(ready.node),
        _ => None,
    }
}

/// An xHCI PORTSC decoded: connect, enabled, reset, link state, power,
/// speed, and the change bits.
struct Portsc(u32);

impl fmt::Display for Portsc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        let bit = |n: u32| u8::from(v & (1 << n) != 0);
        write!(
            f,
            "CCS {} PED {} PR {} PLS {} PP {} speed {} CSC {} PEC {} WRC {} OCC {} PRC {} PLC {} CEC {}",
            bit(0),
            bit(1),
            bit(4),
            (v >> 5) & 0xF,
            bit(9),
            (v >> 10) & 0xF,
            bit(17),
            bit(18),
            bit(19),
            bit(20),
            bit(21),
            bit(22),
            bit(23),
        )
    }
}

/// Say a note on the console.
fn tell(note: Note) {
    match note {
        Note::Device {
            address,
            speed,
            vendor,
            product,
            hub_ports,
            functions,
        } => match hub_ports {
            Some(ports) => ferrix_rt::info!(
                "usbhid: device {address} {vendor:04x}:{product:04x} {speed:?} speed, a hub of {ports} ports"
            ),
            None => ferrix_rt::info!(
                "usbhid: device {address} {vendor:04x}:{product:04x} {speed:?} speed, {functions} input functions"
            ),
        },
        Note::Gone { address } => ferrix_rt::info!("usbhid: device {address} gone"),
        Note::PortEvent { port, status } => ferrix_rt::debug!(
            "usbhid: port status change event, root port {port}: PORTSC {status:08x} {}",
            Portsc(status)
        ),
        Note::RootGone {
            port,
            status,
            changed,
        } => ferrix_rt::warn!(
            "usbhid: root port {port}'s device let go ({}): PORTSC {status:08x} {}",
            if changed {
                "connect status change"
            } else {
                "nothing connected"
            },
            Portsc(status)
        ),
        // A short packet is how most reports end: not worth a line.
        Note::Transfer(trouble) if trouble.code == 13 && trouble.known => {}
        Note::Transfer(trouble) => ferrix_rt::warn!(
            "usbhid: slot {} endpoint {} transfer event code {}{}",
            trouble.slot,
            trouble.endpoint,
            trouble.code,
            if trouble.known {
                ""
            } else {
                ", for a TRB it did not queue"
            }
        ),
        Note::Stopped { function } => ferrix_rt::warn!(
            "usbhid: function {function}'s pipe kept failing and is stopped until it is plugged in again"
        ),
        Note::Failed { parent, error } => {
            ferrix_rt::err!("usbhid: a device at {parent:?} could not be set up: {error:?}");
        }
        Note::Disk { .. } => {}
        Note::DiskFailed { error } => {
            ferrix_rt::err!("usbhid: a mass-storage interface would not open as a disk: {error:?}");
        }
        Note::Handed { port } => ferrix_rt::info!(
            "usbhid: root port {port} has a full- or low-speed device, which EHCI hands to its companion"
        ),
    }
}
