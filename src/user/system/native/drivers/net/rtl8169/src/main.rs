//! The RTL8111H driver process: a ring-3 program that carries one
//! interface's frames between the kernel and a Realtek 8168-family PCIe
//! gigabit adapter -- nazuna's onboard port among them.
//!
//! It is `drivers/net/yt6801`'s program with another chip under it.
//! `ferrix-rtl8169` drives the chip (its indirect buses, its bring-up, its
//! PHY, its two descriptor rings), `ferrix-netring` speaks the ring and
//! `ferrix-netserve` joins them; all three are tested on the host. This
//! program is the handles they were written to be wrapped in:
//!
//! 1. START (`docs/NET-RING.md` §7) brings the device handle and the
//!    driver's end of the ring's control channel.
//! 2. The register window is BAR 2's first 256 bytes, mapped through the
//!    aperture `device_aperture` describes. The chip's MSI-X table is in a
//!    BAR of its own (BAR 4), so no page the driver needs is withheld.
//! 3. Every vector the function offers is bound to one port: its MSI-X
//!    entries, or its MSI vector, or none on a machine that mints none.
//! 4. The loop polls as well as waits: every millisecond until a first
//!    interrupt is seen (so a chip whose interrupts never arrive still
//!    works, slowly), backing off to 8 ms while looks find nothing, then
//!    every 250 ms, which is how often the link is read. A link change the
//!    chip announces is read at once. A change goes to the kernel as LINK.
//!    Two 250 ms looks in a row that find frames no interrupt announced go
//!    back to polling for the rest of the run, as the yt6801's loop does.
//! 5. The sleep is the serve library's (`serve.before_sleep`, then
//!    `serve.woke`), which rings the kernel's doorbell flag: a driver that
//!    slept without it would receive and never send.
//! 6. Every [`STATS_NANOS`] with traffic it says how many interrupts came
//!    against how many frames, and which way it is waiting.
//! 7. STOP stops and resets the chip, and STOPPED goes back only if the
//!    reset was seen to finish; otherwise the pins are kept and the process
//!    exits without it.
//!
//! The exit status is the step that failed ([`Step`]); 0 is a clean STOP.

#![no_std]
#![no_main]

use core::ptr;
use core::sync::atomic::{Ordering, fence};

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{ApertureInfo, IoMappingSpec};
use ferrix_netring::control::{HELLO_RIGHTS, Hello, Interface, InterfaceFlags, MAX_MESSAGE};
use ferrix_netring::driver::DriverSide;
use ferrix_netring::layout::VERSION;
use ferrix_netring::{Message, RingMemory, Wait};
use ferrix_netserve::{Nic, Serve};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::device::{Device, Interrupt, IoMapping};
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_rtl8169::{
    Clock, DEVICE, Driver, Event as ChipEvent, Frame as ChipFrame, Link, MIN_FRAME, Memory,
    Options, Pages, Parts, REGISTER_BAR, REGISTER_BYTES, Registers, SubmitError as ChipSubmitError,
    Teardown, VENDOR,
};
use ferrix_virtio_net::{DeviceError, Event, Frame, SubmitError};

ferrix_rt::entry!(main);

/// A page, on every architecture this runs on.
const PAGE: usize = 4096;

/// Entries in each of the net ring's two arrays, and slots in its data VMO.
const ENTRIES: u32 = 128;

/// Bytes one net ring slot holds: a frame at the default MTU.
const SLOT_BYTES: u32 = 2048;

/// Bytes in the ring VMO: the header and 128 entries of each kind.
const RING_BYTES: usize = 2 * PAGE;

/// Pages in the net ring's data VMO.
const DATA_PAGES: usize = ENTRIES as usize * SLOT_BYTES as usize / PAGE;

/// Pages of transmit data: one slot per ring entry, so a ring slot maps to
/// a device offset by its index alone.
const TRANSMIT_PAGES: usize = DATA_PAGES;

/// The largest region any pin covers.
const MAX_PAGES: usize = 64;

/// The most vectors bound: the RTL8111H offers four MSI-X entries, of
/// which the chip uses the first.
const MAX_VECTORS: usize = 4;

/// Port keys of this process's own: the ring's are 1 and 2.
const KEY_INTERRUPT: u64 = 3;
/// The control channel becoming readable.
const KEY_CONTROL: u64 = 4;

/// How often the link is read.
const LINK_POLL_NANOS: u64 = 250_000_000;

/// How long without an interrupt before frames a look finds count as ones
/// no interrupt announced.
const QUIET_NANOS: u64 = 50_000_000;

/// Looks in a row that must find unannounced frames before the driver
/// stops trusting its interrupts.
const MISSES_TO_POLL: u32 = 2;

/// How often the interrupts and frames are counted out on the console.
const STATS_NANOS: u64 = 10_000_000_000;

/// The name the interface takes in the net core.
const INTERFACE_NAME: &[u8] = b"eth0";

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device that is not an 8168.
    Identity = 2,
    /// The register window could not be mapped.
    Registers = 3,
    /// A VMO could not be made, pinned or mapped.
    Memory = 4,
    /// The chip would not come up.
    Device = 5,
    /// The ring could not be made, or HELLO was refused.
    Ring = 6,
    /// The serve loop stopped on a fault.
    Serve = 7,
    /// The chip would not reset.
    Reset = 8,
}

/// The program.
fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(()) => 0,
        Err(step) => step as i32,
    }
}

// ---------------------------------------------------------------------------
// Memory this process shares with the device or the kernel
// ---------------------------------------------------------------------------

/// A VMO mapped into this process: a byte range volatile accesses go to.
#[derive(Clone, Copy)]
struct Mapped {
    /// Where the mapping starts.
    base: usize,
    /// How long it is.
    len: usize,
}

impl Mapped {
    /// The byte at `offset`, or zero outside.
    fn read_u8(&self, offset: usize) -> u8 {
        if offset >= self.len {
            return 0;
        }
        // SAFETY: `base..base + len` is a mapping the kernel made for this
        // process and never takes away while the VMO handle lives; the offset
        // was checked. Volatile, because the other side is a device or the
        // kernel.
        unsafe { ptr::read_volatile((self.base + offset) as *const u8) }
    }

    /// Write the byte at `offset`, or nothing outside.
    fn write_u8(&mut self, offset: usize, value: u8) {
        if offset >= self.len {
            return;
        }
        // SAFETY: as for `read_u8`, and the mapping is writable.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u8, value) }
    }

    /// The little-endian `u32` at `offset`, in one access: the core may be
    /// writing the descriptor now.
    fn read_u32(&self, offset: usize) -> u32 {
        if offset.checked_add(4).is_none_or(|end| end > self.len) {
            return 0;
        }
        let address = self.base + offset;
        if !address.is_multiple_of(4) {
            return u32::from_le_bytes([
                self.read_u8(offset),
                self.read_u8(offset + 1),
                self.read_u8(offset + 2),
                self.read_u8(offset + 3),
            ]);
        }
        // SAFETY: as for `read_u8`, and the four bytes are one aligned `u32`.
        u32::from_le(unsafe { ptr::read_volatile(address as *const u32) })
    }

    /// Write the little-endian `u32` at `offset`, in one access: the core
    /// must never see a descriptor word half written.
    fn write_u32(&mut self, offset: usize, value: u32) {
        if offset.checked_add(4).is_none_or(|end| end > self.len) {
            return;
        }
        let address = self.base + offset;
        if !address.is_multiple_of(4) {
            for (at, byte) in value.to_le_bytes().into_iter().enumerate() {
                self.write_u8(offset + at, byte);
            }
            return;
        }
        // SAFETY: as for `write_u8`, and the four bytes are one aligned `u32`.
        unsafe { ptr::write_volatile(address as *mut u32, value.to_le()) }
    }

    /// The little-endian `u16` at `offset`: one access where it is aligned,
    /// bytes where it is not.
    fn read_u16(&self, offset: usize) -> u16 {
        if offset.checked_add(2).is_none_or(|end| end > self.len) {
            return 0;
        }
        let address = self.base + offset;
        if !address.is_multiple_of(2) {
            return u16::from_le_bytes([self.read_u8(offset), self.read_u8(offset + 1)]);
        }
        // SAFETY: as for `read_u8`, and the two bytes are one aligned `u16`.
        u16::from_le(unsafe { ptr::read_volatile(address as *const u16) })
    }

    /// Write the little-endian `u16` at `offset`, as [`Mapped::read_u16`]
    /// reads it.
    fn write_u16(&mut self, offset: usize, value: u16) {
        if offset.checked_add(2).is_none_or(|end| end > self.len) {
            return;
        }
        let address = self.base + offset;
        if !address.is_multiple_of(2) {
            let [low, high] = value.to_le_bytes();
            self.write_u8(offset, low);
            self.write_u8(offset + 1, high);
            return;
        }
        // SAFETY: as for `write_u8`, and the two bytes are one aligned `u16`.
        unsafe { ptr::write_volatile(address as *mut u16, value.to_le()) }
    }
}

/// A VMO this process made, pinned for the device and mapped for itself.
struct Pinned {
    /// Kept for its life: dropping it unpins, which must never happen while
    /// the device may still write.
    _pin: Pin<Kernel>,
    /// Kept so the mapping stays.
    _vmo: Vmo<Kernel>,
    /// This process's view.
    mapped: Mapped,
    /// The device's view, page by page.
    addresses: [u64; MAX_PAGES],
    /// How many of those are in use.
    pages: usize,
}

impl Pinned {
    /// `pages` pages, pinned into `device`'s domain and mapped read-write.
    fn new(device: &Device<Kernel>, pages: usize) -> Result<Pinned, Step> {
        if pages > MAX_PAGES {
            return Err(Step::Memory);
        }
        let bytes = pages * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        let pin = device
            .pin(&vmo, 0, bytes, PinAccess::ReadWrite)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; MAX_PAGES];
        let asked = raw.get_mut(..pages).ok_or(Step::Memory)?;
        let got = pin.addresses(asked).map_err(|_| Step::Memory)?;
        if !got.is_complete() || got.pages != pages {
            return Err(Step::Memory);
        }
        let mut addresses = [0_u64; MAX_PAGES];
        for (slot, bytes) in addresses.iter_mut().zip(raw.iter().take(pages)) {
            *slot = device_address(*bytes);
        }
        let base = vmo
            .map(None, bytes, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        Ok(Pinned {
            _pin: pin,
            _vmo: vmo,
            mapped: Mapped { base, len: bytes },
            addresses,
            pages,
        })
    }
}

impl Pages for Pinned {
    fn device_pages(&self) -> &[u64] {
        self.addresses.get(..self.pages).unwrap_or_default()
    }
}

impl Memory for Pinned {
    fn read_u32(&self, offset: usize) -> u32 {
        self.mapped.read_u32(offset)
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.mapped.write_u32(offset, value);
    }

    fn barrier(&self) {
        fence(Ordering::SeqCst);
    }
}

/// The ring VMO as the ring library reads it: mapped here, shared with the
/// kernel, never pinned.
struct Ring {
    /// Kept so the mapping stays.
    vmo: Vmo<Kernel>,
    /// This process's view.
    mapped: Mapped,
}

impl RingMemory for Ring {
    fn read_u8(&self, offset: usize) -> u8 {
        self.mapped.read_u8(offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.mapped.write_u8(offset, value);
    }

    fn barrier(&self) {
        fence(Ordering::SeqCst);
    }
    // The indices the kernel moves while this side reads them are one
    // access each where aligned (`ferrix-netring`'s `RingMemory`): composed
    // from bytes, a tail stepping from 0x00ff to 0x0100 could read 0x01ff.

    fn read_u16(&self, offset: usize) -> u16 {
        self.mapped.read_u16(offset)
    }

    fn read_u32(&self, offset: usize) -> u32 {
        self.mapped.read_u32(offset)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        self.mapped.write_u16(offset, value);
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.mapped.write_u32(offset, value);
    }
}

/// The ring's data VMO, which the kernel also maps.
struct Data {
    /// This process's view.
    mapped: Mapped,
}

impl RingMemory for Data {
    fn read_u8(&self, offset: usize) -> u8 {
        self.mapped.read_u8(offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.mapped.write_u8(offset, value);
    }

    fn barrier(&self) {
        fence(Ordering::SeqCst);
    }
    // The indices the kernel moves while this side reads them are one
    // access each where aligned (`ferrix-netring`'s `RingMemory`): composed
    // from bytes, a tail stepping from 0x00ff to 0x0100 could read 0x01ff.

    fn read_u16(&self, offset: usize) -> u16 {
        self.mapped.read_u16(offset)
    }

    fn read_u32(&self, offset: usize) -> u32 {
        self.mapped.read_u32(offset)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        self.mapped.write_u16(offset, value);
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.mapped.write_u32(offset, value);
    }
}

// ---------------------------------------------------------------------------
// The register window
// ---------------------------------------------------------------------------

/// BAR 2's first [`REGISTER_BYTES`], mapped.
struct Window {
    /// Kept so the mapping stays.
    _mapping: IoMapping<Kernel>,
    /// Where it is mapped.
    base: usize,
}

impl Window {
    /// Map the aperture of [`REGISTER_BAR`] that starts at its offset 0.
    fn map(device: &Device<Kernel>, apertures: u32) -> Result<Window, Step> {
        for index in 0..apertures as usize {
            let Ok(aperture) = device.aperture(index) else {
                continue;
            };
            let ApertureInfo {
                phys,
                len,
                bar,
                offset,
                ..
            } = aperture;
            if bar != REGISTER_BAR || offset != 0 || len < REGISTER_BYTES {
                continue;
            }
            // The whole page: an aperture is granted in whole pages, and the
            // registers are its first 256 bytes.
            let len = len.min(PAGE as u64);
            let mapping = device
                .io_mapping(IoMappingSpec { phys, len })
                .map_err(|_| Step::Registers)?;
            let base = mapping.map(None).map_err(|_| Step::Registers)?;
            return Ok(Window {
                _mapping: mapping,
                base,
            });
        }
        Err(Step::Registers)
    }

    /// Where `offset` is mapped, for an access of `width` bytes: inside the
    /// register window and aligned to the width, else nothing.
    fn at(&self, offset: u32, width: u32) -> Option<usize> {
        let inside = u64::from(offset) + u64::from(width) <= REGISTER_BYTES;
        (inside && offset.is_multiple_of(width)).then(|| self.base + offset as usize)
    }
}

impl Registers for Window {
    fn read8(&self, offset: u32) -> u8 {
        let Some(address) = self.at(offset, 1) else {
            return u8::MAX;
        };
        // SAFETY: `address` is inside the register window the kernel mapped
        // for this process, kept for the `Window`'s life.
        unsafe { read8(address as *const u8) }
    }

    fn read16(&self, offset: u32) -> u16 {
        let Some(address) = self.at(offset, 2) else {
            return u16::MAX;
        };
        // SAFETY: as for `read8`, and aligned for a `u16`.
        unsafe { read16(address as *const u16) }
    }

    fn read32(&self, offset: u32) -> u32 {
        let Some(address) = self.at(offset, 4) else {
            return u32::MAX;
        };
        // SAFETY: as for `read8`, and aligned for a `u32`.
        unsafe { read32(address as *const u32) }
    }

    fn write8(&mut self, offset: u32, value: u8) {
        let Some(address) = self.at(offset, 1) else {
            return;
        };
        // SAFETY: as for `read8`, and the mapping is writable.
        unsafe { write8(address as *mut u8, value) }
    }

    fn write16(&mut self, offset: u32, value: u16) {
        let Some(address) = self.at(offset, 2) else {
            return;
        };
        // SAFETY: as for `read16`, and the mapping is writable.
        unsafe { write16(address as *mut u16, value) }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        let Some(address) = self.at(offset, 4) else {
            return;
        };
        // SAFETY: as for `read32`, and the mapping is writable.
        unsafe { write32(address as *mut u32, value) }
    }
}

// Each register access is one plain load or store, never inlined: inlined
// into a loop the address step can fold into the access as a writeback,
// which KVM's MMIO emulation refuses (`ferrix-driver`'s `mmio.rs`). The
// chip's registers are little-endian, as every processor this runs on is.

/// Read a byte register.
///
/// # Safety
///
/// `at` is mapped device memory.
#[inline(never)]
unsafe fn read8(at: *const u8) -> u8 {
    // SAFETY: as the caller promises.
    unsafe { ptr::read_volatile(at) }
}

/// Read a 16-bit register.
///
/// # Safety
///
/// `at` is mapped device memory, aligned.
#[inline(never)]
unsafe fn read16(at: *const u16) -> u16 {
    // SAFETY: as the caller promises.
    unsafe { ptr::read_volatile(at) }
}

/// Read a 32-bit register.
///
/// # Safety
///
/// `at` is mapped device memory, aligned.
#[inline(never)]
unsafe fn read32(at: *const u32) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { ptr::read_volatile(at) }
}

/// Write a byte register.
///
/// # Safety
///
/// `at` is mapped, writable device memory.
#[inline(never)]
unsafe fn write8(at: *mut u8, value: u8) {
    // SAFETY: as the caller promises.
    unsafe { ptr::write_volatile(at, value) }
}

/// Write a 16-bit register.
///
/// # Safety
///
/// `at` is mapped, writable device memory, aligned.
#[inline(never)]
unsafe fn write16(at: *mut u16, value: u16) {
    // SAFETY: as the caller promises.
    unsafe { ptr::write_volatile(at, value) }
}

/// Write a 32-bit register.
///
/// # Safety
///
/// `at` is mapped, writable device memory, aligned.
#[inline(never)]
unsafe fn write32(at: *mut u32, value: u32) {
    // SAFETY: as the caller promises.
    unsafe { ptr::write_volatile(at, value) }
}

/// The monotonic clock, and a port of the driver's own to sleep on.
struct Monotonic {
    port: Port<Kernel>,
}

impl Clock for Monotonic {
    fn now_nanos(&self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
    }

    fn pause(&mut self, nanos: u64) {
        let until = self.now_nanos().saturating_add(nanos);
        // Nothing is ever queued on this port: the wait ends at the
        // deadline, and any other answer only shortens the pause.
        let _ = self.port.wait(Deadline::At(until));
    }
}

// ---------------------------------------------------------------------------
// The device, as the serve loop sees it
// ---------------------------------------------------------------------------

/// A window onto a mapping the driver holds for as long as it lives.
#[derive(Clone, Copy)]
struct View(Mapped);

impl View {
    /// Copy `bytes` to `offset`, then zeroes up to `at_least` bytes, or
    /// nothing if it does not fit.
    fn write(&self, offset: u64, bytes: &[u8], at_least: usize) {
        let mut mapped = self.0;
        let Ok(at) = usize::try_from(offset) else {
            return;
        };
        let len = bytes.len().max(at_least);
        if at.saturating_add(len) > mapped.len {
            return;
        }
        for index in 0..len {
            mapped.write_u8(at + index, bytes.get(index).copied().unwrap_or(0));
        }
    }

    /// Fill `out` from `offset`, or with zeroes if it does not fit.
    fn read(&self, offset: u64, out: &mut [u8]) {
        let Ok(at) = usize::try_from(offset) else {
            return;
        };
        if at.saturating_add(out.len()) > self.0.len {
            return;
        }
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = self.0.read_u8(at + index);
        }
    }
}

/// The driver's own type, written once.
type Card = Driver<Window, Monotonic, Pinned, Pinned>;

/// The device as [`Nic`] wants it.
struct Adapter {
    /// The chip's driver.
    card: Card,
    /// Where frames to send are written.
    transmit: View,
    /// Where frames that arrived are read.
    receive: View,
    /// A drain saw the chip announce a link change.
    link_changed: bool,
}

impl Nic for Adapter {
    fn submit(&mut self, frame: &Frame) -> Result<(), SubmitError> {
        let chip = ChipFrame {
            id: frame.id,
            offset: frame.offset,
            len: frame.len,
        };
        self.card.submit(&chip).map_err(|error| match error {
            ChipSubmitError::QueueFull => SubmitError::QueueFull,
            ChipSubmitError::BadLength if frame.len == 0 => SubmitError::Empty,
            ChipSubmitError::BadLength => SubmitError::TooLong,
            ChipSubmitError::OutsideData => SubmitError::OutsideData,
            ChipSubmitError::Broken => SubmitError::Broken,
        })
    }

    fn release(&mut self, buffer: u16) -> Result<(), DeviceError> {
        self.card
            .release(buffer)
            .map_err(|_| DeviceError::Bookkeeping)
    }

    fn drain(&mut self, out: &mut [Event], receive_room: usize) -> Result<usize, DeviceError> {
        let mut chip = [ChipEvent::Sent { id: 0 }; 32];
        let room = out.len().min(chip.len());
        let drained = self
            .card
            .drain_receiving(chip.get_mut(..room).unwrap_or_default(), receive_room)
            .map_err(|_| DeviceError::NeedsReset)?;
        if drained.status & ferrix_rtl8169::regs::INT_LINK_CHANGE != 0 {
            self.link_changed = true;
        }
        for (slot, event) in out.iter_mut().zip(chip.iter().take(drained.events)) {
            *slot = match *event {
                ChipEvent::Received {
                    buffer,
                    offset,
                    len,
                } => Event::Received {
                    buffer,
                    offset,
                    len,
                },
                ChipEvent::Sent { id } => Event::Sent { id },
            };
        }
        Ok(drained.events)
    }

    /// The frame, then zeroes to the shortest frame the chip is handed
    /// ([`MIN_FRAME`]): a short frame is sent padded, and the padding must
    /// never be an earlier frame's bytes.
    fn write_transmit(&mut self, offset: u64, frame: &[u8]) {
        self.transmit.write(offset, frame, MIN_FRAME as usize);
    }

    fn read_receive(&self, offset: u64, out: &mut [u8]) {
        self.receive.read(offset, out);
    }

    fn transmit_slot(&self, index: u32) -> (u64, u32) {
        (u64::from(index) * u64::from(SLOT_BYTES), SLOT_BYTES)
    }
}

// ---------------------------------------------------------------------------
// Bringing it up
// ---------------------------------------------------------------------------

/// What START handed over.
struct Started {
    /// The device.
    device: Device<Kernel>,
    /// The driver's end of the ring's control channel.
    control: Channel<Kernel>,
}

/// Read START off the bootstrap channel.
fn started(boot: &Channel<Kernel>) -> Result<Started, Step> {
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Start)?;
    let mut bytes = [0_u8; MAX_MESSAGE];
    let mut handles = [Handle::INVALID; 2];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Start)?;
    if received.handles != 2 {
        return Err(Step::Start);
    }
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));
    let control = Channel::from_owned(OwnedHandle::from_raw(Kernel, handles[1]));
    match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(Message::Start(_)) => Ok(Started { device, control }),
        _ => Err(Step::Start),
    }
}

/// Claim every vector the function offers, up to [`MAX_VECTORS`], and bind
/// each to `port`. None at all is not a failure: the loop polls.
fn vectors(
    device: &Device<Kernel>,
    count: u32,
    port: &Port<Kernel>,
) -> [Option<Interrupt<Kernel>>; MAX_VECTORS] {
    let mut bound: [Option<Interrupt<Kernel>>; MAX_VECTORS] = Default::default();
    for (index, slot) in bound.iter_mut().enumerate().take(count as usize) {
        let Ok(interrupt) = device.interrupt(index) else {
            continue;
        };
        if interrupt.bind(port, KEY_INTERRUPT).is_ok() {
            *slot = Some(interrupt);
        }
    }
    let bound_count = bound.iter().flatten().count();
    if bound_count == 0 {
        ferrix_rt::warn!(
            "rtl8169  no interrupt vector could be bound ({count} offered): polling every 1 to 8 ms"
        );
    } else {
        ferrix_rt::info!("rtl8169  {bound_count} of {count} vectors bound");
    }
    bound
}

/// Make the chip's memory and bring the chip up.
fn bring_up(device: &Device<Kernel>, regs: Window, location: u32) -> Result<Adapter, Step> {
    let clock = Monotonic {
        port: port::create(Kernel).map_err(|_| Step::Memory)?,
    };
    let seed = location.to_le_bytes();
    let options = Options {
        transmit_entries: 64,
        receive_entries: 128,
        buffer_bytes: 2048,
        gigabit: true,
        seed: [seed[0], seed[1], seed[2]],
    };
    let rings = Pinned::new(device, 1)?;
    let receive_pages = usize::from(options.receive_entries) * options.buffer_bytes as usize / PAGE;
    let receive = Pinned::new(device, receive_pages)?;
    let transmit = Pinned::new(device, TRANSMIT_PAGES)?;
    let receive_view = View(receive.mapped);
    let transmit_view = View(transmit.mapped);
    let parts = Parts {
        regs,
        clock,
        rings,
        receive,
        transmit,
    };
    let card = match Driver::init(parts, options) {
        Ok(card) => card,
        Err(failed) => {
            ferrix_rt::err!(
                "rtl8169  the chip did not come up: {} (reset after: {})",
                failed.error,
                failed.reset
            );
            // A chip not seen reset may still write its memory: the
            // kernel's pin quarantine keeps the frames until the next
            // driver's HELLO (`object::pin`), whatever this process drops.
            return Err(Step::Device);
        }
    };
    let info = *card.info();
    ferrix_rt::info!(
        "rtl8169  {:?} XID {:#05x}, PHY {:#010x}, address {:02x?} from {:?}",
        info.chip,
        info.xid,
        info.phy_id,
        info.mac,
        info.mac_source
    );
    Ok(Adapter {
        card,
        transmit: transmit_view,
        receive: receive_view,
        link_changed: false,
    })
}

// ---------------------------------------------------------------------------
// The ring, and the loop over it
// ---------------------------------------------------------------------------

/// The ring's memory and the driver's end of it.
struct RingSide {
    /// The ring VMO.
    ring: Ring,
    /// The data VMO.
    data: Data,
    /// The driver's end.
    side: DriverSide,
    /// The port the kernel rings, and the device's vectors too.
    port: Port<Kernel>,
    /// The port the kernel gave back with READY.
    completion: Port<Kernel>,
}

/// Make the ring, send HELLO, and take READY.
fn open_ring(
    control: &Channel<Kernel>,
    adapter: &Adapter,
    port: Port<Kernel>,
) -> Result<RingSide, Step> {
    let ring_vmo = vmo::create(Kernel, RING_BYTES).map_err(|_| Step::Ring)?;
    let ring_base = ring_vmo
        .map(None, RING_BYTES, Protection::ReadWrite, 0)
        .map_err(|_| Step::Ring)?;
    let mut ring = Ring {
        vmo: ring_vmo,
        mapped: Mapped {
            base: ring_base,
            len: RING_BYTES,
        },
    };
    let data_bytes = DATA_PAGES * PAGE;
    let data_vmo = vmo::create(Kernel, data_bytes).map_err(|_| Step::Ring)?;
    let data_base = data_vmo
        .map(None, data_bytes, Protection::ReadWrite, 0)
        .map_err(|_| Step::Ring)?;
    let data = Data {
        mapped: Mapped {
            base: data_base,
            len: data_bytes,
        },
    };
    let side =
        DriverSide::create(&mut ring, RING_BYTES, ENTRIES, SLOT_BYTES).map_err(|_| Step::Ring)?;
    let hello = Message::Hello(Hello {
        version: VERSION,
        entries: ENTRIES,
        slot_bytes: SLOT_BYTES,
        interface: Interface::new(
            INTERFACE_NAME,
            adapter.card.info().mac,
            1500,
            InterfaceFlags {
                carrier: matches!(adapter.card.link(), Link::Up { .. }),
                broadcast: true,
                multicast: true,
            },
        ),
    });
    let mut bytes = [0_u8; MAX_MESSAGE];
    let written = hello.encode(&mut bytes).map_err(|_| Step::Ring)?;
    let [ring_rights, data_rights, port_rights] = HELLO_RIGHTS;
    let handles = [
        ring.vmo
            .as_owned()
            .duplicate(Requested::Exactly(ring_rights))
            .map_err(|_| Step::Ring)?,
        data_vmo
            .as_owned()
            .duplicate(Requested::Exactly(data_rights))
            .map_err(|_| Step::Ring)?,
        port.as_owned()
            .duplicate(Requested::Exactly(port_rights))
            .map_err(|_| Step::Ring)?,
    ];
    control
        .write_with(bytes.get(..written).unwrap_or_default(), handles)
        .map_err(|_| Step::Ring)?;
    let completion = take_ready(control)?;
    Ok(RingSide {
        ring,
        data,
        side,
        port,
        completion,
    })
}

/// Wait for READY and take the completion port it carries.
fn take_ready(control: &Channel<Kernel>) -> Result<Port<Kernel>, Step> {
    let mut bytes = [0_u8; MAX_MESSAGE];
    let mut handles = [Handle::INVALID; 1];
    loop {
        match control.read(&mut bytes, &mut handles) {
            Ok(received) => {
                let message = Message::decode(bytes.get(..received.bytes).unwrap_or_default())
                    .map_err(|_| Step::Ring)?;
                if !matches!(message, Message::Ready) || received.handles != 1 {
                    return Err(Step::Ring);
                }
                return Ok(Port::from_owned(OwnedHandle::from_raw(Kernel, handles[0])));
            }
            Err(ReadError::Failed(_)) => {}
            Err(_) => return Err(Step::Ring),
        }
        let _ = control
            .wait_one(Signals::READABLE, Deadline::Never)
            .map_err(|_| Step::Ring)?;
    }
}

/// Tell the kernel the link changed.
fn say_link(control: &Channel<Kernel>, up: bool) {
    let mut bytes = [0_u8; MAX_MESSAGE];
    if let Ok(written) = Message::Link(up).encode(&mut bytes) {
        let _ = control.write(bytes.get(..written).unwrap_or_default());
    }
}

/// Ask the PHY about the link and tell the kernel of a change.
///
/// A bus that did not answer this time is asked again at the next poll; the
/// link is as it was. The driver once left on one slow MDIO wait on a
/// loaded box (2026-10-18), and a driver started again on such a box can
/// fail its bring-up too, which leaves the machine off the network.
fn poll_link(
    adapter: &mut Adapter,
    control: &Channel<Kernel>,
    misses: &mut u64,
) -> Result<(), Step> {
    match adapter.card.poll_link() {
        Ok(Some(link)) => {
            let up = matches!(link, Link::Up { .. });
            ferrix_rt::info!("rtl8169  link {link:?}");
            ferrix_rt::info!("rtl8169  {:x?}", adapter.card.snapshot());
            say_link(control, up);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(ferrix_rtl8169::Error::Timeout(_)) => {
            *misses += 1;
            ferrix_rt::warn!(
                "rtl8169  the PHY did not answer a link poll ({misses} so far); asking again"
            );
            Ok(())
        }
        Err(error) => {
            ferrix_rt::err!("rtl8169  the link poll failed: {error}");
            Err(Step::Serve)
        }
    }
}

/// Carry frames until the ring ends.
fn serve(
    ring: &mut RingSide,
    adapter: &mut Adapter,
    vectors: &[Option<Interrupt<Kernel>>; MAX_VECTORS],
    control: &Channel<Kernel>,
) -> Result<(), Step> {
    let mut serve = Serve::new();
    let now = || ferrix_rt::linux::monotonic_nanos().unwrap_or(0);
    let mut interrupted = false;
    // Whether this turn follows a wait that ended at its deadline.
    let mut polled = false;
    // Set once looks found frames no interrupt announced: from then on an
    // interrupt no longer lengthens the wait.
    let mut unreliable = false;
    // Looks in a row that found frames no interrupt announced.
    let mut misses = 0;
    let mut last_interrupt = 0;
    let mut stats = Stats::new(now(), adapter.seen(&serve));
    let mut next_link = 0;
    // Link polls the PHY did not answer in time.
    let mut link_misses: u64 = 0;
    let mut poll_every = ferrix_rtl8169::POLL_NANOS;
    control
        .wait_async(
            &ring.port,
            Signals::READABLE | Signals::PEER_CLOSED,
            KEY_CONTROL,
        )
        .map_err(|_| Step::Ring)?;
    loop {
        // A link change the chip announced is read at once, not at the next
        // 250 ms look.
        if now() >= next_link || core::mem::take(&mut adapter.link_changed) {
            next_link = now().saturating_add(LINK_POLL_NANOS);
            poll_link(adapter, control, &mut link_misses)?;
        }
        let turn = serve
            .turn(&mut ring.ring, &mut ring.data, &mut ring.side, adapter)
            .map_err(|fault| {
                ferrix_rt::err!("rtl8169  the serve loop stopped: {fault} ({fault:?})");
                Step::Serve
            })?;
        stats.turn(&turn, polled);
        poll_every = next_look(poll_every, &turn, polled);
        // Frames a deadline found that no interrupt announced, with none
        // for long before: the interrupts are not arriving after all, and
        // every frame would wait for the 250 ms look or for a send to ring
        // the bell. Back to looking every millisecond (backing off to 8 ms while
        // looks find nothing), for good: interrupts
        // that come only now and then (the N150 box's yt6801, before its
        // glue's latch was cleared: 209 in five minutes of downloading)
        // would each put the wait back to 250 ms until a look found frames
        // again.
        if polled && interrupted {
            let unannounced =
                turn.received > 0 && now().saturating_sub(last_interrupt) > QUIET_NANOS;
            misses = if unannounced { misses + 1 } else { 0 };
            if misses >= MISSES_TO_POLL {
                interrupted = false;
                unreliable = true;
                ferrix_rt::notice!(
                    "rtl8169  frames no interrupt announced: polling every 1 to 8 ms from now on"
                );
            }
        }
        polled = false;
        let at = now();
        if at.saturating_sub(stats.since) >= STATS_NANOS {
            stats.say(at, adapter.seen(&serve), interrupted);
            ferrix_rt::debug!("rtl8169  {:x?}", adapter.card.snapshot());
        }
        if let Some(bell) = ring.side.publish(&mut ring.ring) {
            let _ = ring
                .completion
                .queue(bell.key(), [u64::from(bell.tail()), 0]);
        }
        if stopped(control)? {
            return Ok(());
        }
        if turn.busy {
            continue;
        }
        let wait = serve
            .before_sleep(&mut ring.ring, &mut ring.side)
            .map_err(|fault| {
                ferrix_rt::err!("rtl8169  the ring refused a sleep: {fault}");
                Step::Serve
            })?;
        if wait != Wait::Sleep {
            continue;
        }
        let deadline = if interrupted {
            next_link
        } else {
            now().saturating_add(poll_every).min(next_link)
        };
        let packet = ring.port.wait(Deadline::At(deadline));
        serve.woke(&mut ring.ring, &mut ring.side);
        match packet {
            Ok(packet) if packet.key == KEY_CONTROL => {
                let _ = control.wait_async(
                    &ring.port,
                    Signals::READABLE | Signals::PEER_CLOSED,
                    KEY_CONTROL,
                );
            }
            Ok(packet) if packet.key == KEY_INTERRUPT => {
                last_interrupt = now();
                stats.interrupts += 1;
                if !interrupted && !unreliable {
                    interrupted = true;
                    ferrix_rt::info!("rtl8169  first interrupt: polling every 250 ms now");
                }
                // The drain in the next turn takes what it raised; every
                // vector is re-armed, whichever rang.
                for interrupt in vectors.iter().flatten() {
                    let _ = interrupt.ack();
                }
            }
            // The deadline: the next turn looks, and says whether it found
            // what an interrupt should have announced.
            Err(_) => polled = true,
            // The kernel's bell: the next turn looks.
            _ => {}
        }
    }
}

/// The look's next wait while interrupts are not trusted (p2-138): backed
/// off only after a look that found nothing to do, and back to the fast look
/// on anything moved, whatever woke the turn.
fn next_look(every: u64, turn: &ferrix_netserve::Turn, polled: bool) -> u64 {
    let moved = turn.sent > 0 || turn.received > 0 || turn.dropped > 0;
    if moved || polled {
        ferrix_rtl8169::next_poll_nanos(every, moved)
    } else {
        every
    }
}

/// What the loop counts between two lines of [`STATS_NANOS`].
struct Stats {
    /// When the count began.
    since: u64,
    /// Interrupt packets taken.
    interrupts: u64,
    /// Frames received.
    received: u64,
    /// Frames handed to the core to send.
    sent: u64,
    /// Turns after a wait that ended at its deadline.
    looks: u64,
    /// Of those, the ones that found frames.
    found: u64,
    /// The adapter's and the loop's counts when the count began.
    seen: Seen,
}

/// Counts that run for the driver's life, of which a line says the change.
#[derive(Clone, Copy, Default)]
struct Seen {
    /// Times the transmit doorbell was rung again for frames still out.
    rekicks: u64,
    /// Frames the loop dropped for want of a ring slot.
    dropped: u64,
    /// Drains that found the core had run out of receive descriptors:
    /// frames then wait in its FIFO, and past that are lost.
    unavailable: u64,
    /// Descriptors the core gave back with an error, lent again unread.
    bad: u64,
}

impl Adapter {
    /// The counts as they stand.
    fn seen(&self, serve: &Serve) -> Seen {
        let core = self.card.counters();
        Seen {
            rekicks: core.rekicks,
            dropped: serve.counters().dropped,
            unavailable: core.receive_unavailable,
            bad: core.receive_dropped,
        }
    }
}

impl Stats {
    /// A count beginning at `since`.
    const fn new(since: u64, seen: Seen) -> Stats {
        Stats {
            since,
            interrupts: 0,
            received: 0,
            sent: 0,
            looks: 0,
            found: 0,
            seen,
        }
    }

    /// Count `turn`, which followed a deadline if `polled`.
    fn turn(&mut self, turn: &ferrix_netserve::Turn, polled: bool) {
        self.received += u64::from(turn.received);
        self.sent += u64::from(turn.sent);
        if polled {
            self.looks += 1;
            if turn.received > 0 {
                self.found += 1;
            }
        }
    }

    /// Say the count, if anything moved, and begin the next at `at`.
    ///
    /// `interrupts/s` against `frames in/s` is the line to read: with the
    /// interrupts carrying the traffic there are a few frames per
    /// interrupt and few of the looks find any; with them silent the looks
    /// (a thousand a second) find the frames.
    fn say(&mut self, at: u64, seen: Seen, interrupted: bool) {
        let elapsed = at.saturating_sub(self.since).max(1);
        if self.interrupts + self.received + self.sent > 0 {
            let per_second = |count: u64| count.saturating_mul(1_000_000_000) / elapsed;
            let per_interrupt_tenths = self.received.saturating_mul(10) / self.interrupts.max(1);
            ferrix_rt::debug!(
                "rtl8169  {} ms: {} interrupts ({}/s), {} frames in ({}/s), {} out, {}.{} in per \
                 interrupt; {} of {} looks found frames; doorbell rung again {} times; out of \
                 receive descriptors {} times, {} dropped with no slot, {} bad; {}",
                elapsed / 1_000_000,
                self.interrupts,
                per_second(self.interrupts),
                self.received,
                per_second(self.received),
                self.sent,
                per_interrupt_tenths / 10,
                per_interrupt_tenths % 10,
                self.found,
                self.looks,
                seen.rekicks.saturating_sub(self.seen.rekicks),
                seen.unavailable.saturating_sub(self.seen.unavailable),
                seen.dropped.saturating_sub(self.seen.dropped),
                seen.bad.saturating_sub(self.seen.bad),
                if interrupted {
                    "waking on interrupts"
                } else {
                    "polling every 1 to 8 ms"
                }
            );
        }
        *self = Stats::new(at, seen);
    }
}

/// Whether the kernel has asked the driver to stop.
fn stopped(control: &Channel<Kernel>) -> Result<bool, Step> {
    let mut bytes = [0_u8; MAX_MESSAGE];
    let mut handles = [Handle::INVALID; 1];
    match control.read(&mut bytes, &mut handles) {
        Ok(received) => match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
            Ok(Message::Stop) => Ok(true),
            Ok(_) | Err(_) => Ok(false),
        },
        Err(ReadError::Failed(_)) => Ok(false),
        Err(_) => Ok(true),
    }
}

/// The whole run.
/// The whole run.
fn run(boot: &Channel<Kernel>) -> Result<(), Step> {
    let Started { device, control } = started(boot)?;
    let info = device.info().map_err(|_| Step::Identity)?;
    if info.vendor_id != VENDOR || info.device_id != DEVICE {
        return Err(Step::Identity);
    }
    ferrix_rt::info!(
        "rtl8169  {:#010x} {:04x}:{:04x} apertures {} vectors {} msix_table_size {}",
        info.location,
        info.vendor_id,
        info.device_id,
        info.apertures,
        info.vectors,
        info.msix_table_size
    );
    for index in 0..info.apertures as usize {
        if let Ok(aperture) = device.aperture(index) {
            ferrix_rt::info!(
                "rtl8169  aperture {index}: bar {} offset {:#x} len {:#x}",
                aperture.bar,
                aperture.offset,
                aperture.len
            );
        }
    }
    say_link_power(&device);
    let regs = Window::map(&device, info.apertures).inspect_err(|_| {
        ferrix_rt::err!("rtl8169  BAR 2's registers are not granted");
    })?;
    let port = port::create(Kernel).map_err(|_| Step::Ring)?;
    let vectors = vectors(&device, info.vectors, &port);
    let mut adapter = bring_up(&device, regs, info.location)?;
    let mut ring = open_ring(&control, &adapter, port)?;
    let outcome = serve(&mut ring, &mut adapter, &vectors, &control);
    let reset = finish(adapter, &control);
    outcome.and(reset)
}

/// Say what the function's configuration space holds of its link's power
/// states and its MSI-X: read only, nothing written.
///
/// Linux turns the link's ASPM L1 off on this family unless the board
/// vendor flagged it safe, because with it on these chips have stopped
/// moving frames; a Ferrix driver may not write Link Control, so the chip's
/// own ASPM is turned off instead (`ferrix-rtl8169`'s `chip`). The line
/// shows what firmware left, so a run that stalls can be told apart.
fn say_link_power(device: &Device<Kernel>) {
    /// The capability list's start, and the IDs looked for in it.
    const CAPABILITIES: u16 = 0x34;
    const EXPRESS: u32 = 0x10;
    const MSIX: u32 = 0x11;
    let read = |offset: u16, width: u8| device.config_read(offset, width).ok();
    let mut express = None;
    let mut msix = None;
    let mut at = read(CAPABILITIES, 1).map_or(0, |pointer| pointer as u16 & 0xFC);
    // At most 48 capabilities fit in the space: a list that loops ends.
    for _ in 0..48 {
        if at < 0x40 {
            break;
        }
        let Some(header) = read(at, 2) else {
            break;
        };
        match header & 0xFF {
            EXPRESS => express = express.or(Some(at)),
            MSIX => msix = msix.or(Some(at)),
            _ => {}
        }
        at = (header >> 8) as u16 & 0xFC;
    }
    let link = express.and_then(|cap| read(cap + 0x10, 2));
    let control = msix.and_then(|cap| read(cap + 0x02, 2));
    match link {
        Some(link) => {
            ferrix_rt::info!(
                "rtl8169  link control {link:#06x}: ASPM L0s {} L1 {}, CLKREQ {}; MSI-X control {}",
                if link & 0b01 != 0 { "on" } else { "off" },
                if link & 0b10 != 0 { "on" } else { "off" },
                if link & 0x100 != 0 { "on" } else { "off" },
                control.map_or(-1, i64::from),
            );
        }
        None => {
            ferrix_rt::warn!("rtl8169  link control unreadable");
        }
    }
}

/// Stop and reset the chip and say STOPPED, or leave without saying it.
fn finish(adapter: Adapter, control: &Channel<Kernel>) -> Result<(), Step> {
    match adapter.card.shutdown() {
        Teardown::Released(_) => {
            let mut bytes = [0_u8; MAX_MESSAGE];
            if let Ok(written) = Message::Stopped.encode(&mut bytes) {
                let _ = control.write(bytes.get(..written).unwrap_or_default());
            }
            Ok(())
        }
        // The chip did not reset: the kernel is told nothing, and its pin
        // quarantine keeps the frames the chip may still write.
        Teardown::Wedged(_) => Err(Step::Reset),
    }
}
