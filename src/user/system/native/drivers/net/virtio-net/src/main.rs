//! The virtio-net driver process: a ring-3 program that carries one
//! interface's frames between the kernel and a virtio-net device.
//!
//! Everything that knows anything is a library. `ferrix-virtio-net` drives the
//! device, `ferrix-netring` speaks the ring, and `ferrix-netserve` joins the
//! two; all three are tested on the host. This program is the handles those
//! libraries were written to be wrapped in, and nothing else:
//!
//! 1. The bootstrap channel carries START (`docs/NET-RING.md` §7): the device
//!    handle and the driver's end of the ring's control channel. A driver that
//!    gets anything else first exits.
//! 2. `device_info` says where the device's virtio register blocks are, and
//!    each becomes an `IoMapping` of the pages holding it. The [`Transport`]
//!    the device library asks for is volatile reads and writes into those.
//! 3. The device's memory — both queues' rings, the virtio headers, and the
//!    two data regions — is VMOs this process creates, pins into the device's
//!    IOMMU domain with `VMO_PIN`, and maps into itself. Device addresses come
//!    only from the pin's address query.
//! 4. The net ring is one more VMO, mapped here and sent to the kernel in
//!    HELLO with the data VMO and a port the kernel rings. READY brings the
//!    kernel's completion port back.
//! 5. One port carries every event: the kernel's bell, the device's interrupt,
//!    and the control channel becoming readable.
//! 6. STOP ends it: the device is reset, the interface goes with the ring, and
//!    STOPPED goes back. A device that will not reset keeps its memory for
//!    ever — the pins are never released — and the process exits without
//!    STOPPED, which tells the kernel to reset through its own means.
//!
//! The exit status is the diagnosis: 0 is a clean STOP, and every other number
//! names the step that failed (see [`Step`]), so a boot check that started the
//! process can say what went wrong without a console.

#![no_std]
#![no_main]

use core::ptr;
use core::sync::atomic::{Ordering, fence};

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{DeviceBlock, DeviceInfo, IoMappingSpec};
use ferrix_netring::control::{HELLO_RIGHTS, Hello, Interface, InterfaceFlags, MAX_MESSAGE};
use ferrix_netring::driver::DriverSide;
use ferrix_netring::layout::VERSION;
use ferrix_netring::{Message, RingMemory, Wait};
use ferrix_netserve::{Nic, Serve};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::device::{Device, Interrupt, IoMapping};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_virtio::DeviceConfig;
use ferrix_virtio::QueueMemory;
use ferrix_virtio::pci::CommonConfig;
use ferrix_virtio_net::{
    DeviceError, DevicePages, Driver, Event, Frame, Options, Parts, RequestArea, Slot, SubmitError,
    Teardown, Transport,
};

ferrix_rt::entry!(main);

/// A page, on every architecture this runs on.
const PAGE: usize = 4096;

/// Entries in each of the net ring's two arrays, and slots in its data VMO.
const ENTRIES: u32 = 128;

/// Bytes one net ring slot holds: a frame at the default MTU and its header,
/// rounded to the alignment the ring asks for.
const SLOT_BYTES: u32 = 2048;

/// Bytes in the ring VMO: the header and 128 entries of each kind of 16 bytes
/// take 4160 bytes, so two pages.
const RING_BYTES: usize = 2 * PAGE;

/// Pages in the net ring's data VMO: [`ENTRIES`] slots of [`SLOT_BYTES`].
const DATA_PAGES: usize = ENTRIES as usize * SLOT_BYTES as usize / PAGE;

/// The most descriptors each device queue is asked for.
const QUEUE_SIZE: u16 = 128;

/// Pages of queue memory, per queue.
const QUEUE_PAGES: usize = 2;

/// Pages of header area: sixteen bytes per entry for both queues.
const AREA_PAGES: usize = 1;

/// Pages of receive data: half the queue at the receive stride.
const RECEIVE_PAGES: usize = 32;

/// Pages of transmit data: one slot per ring entry, so the serve loop can map
/// a ring slot to a device offset by its index alone.
const TRANSMIT_PAGES: usize = DATA_PAGES;

/// The largest region any pin covers, which sizes the address array.
const MAX_PAGES: usize = 64;

/// Bookkeeping slots the device library needs: twice each queue.
const SLOTS: usize = QUEUE_SIZE as usize * 2;

/// PCI device identifiers a virtio-net function has: modern and transitional.
const VIRTIO_NET_IDS: [u16; 2] = [0x1041, 0x1000];

/// Port keys of this process's own: the ring's are 1 and 2.
const KEY_INTERRUPT: u64 = 3;
/// The control channel becoming readable.
const KEY_CONTROL: u64 = 4;

/// The name the interface takes in the net core.
const INTERFACE_NAME: &[u8] = b"eth0";

/// A fault for `cargo xtask test-net` to ask for, compiled in only when the
/// build sets `FERRIX_NET_FAULT` (`native::arm_net_fault` in xtask), and
/// nothing at all otherwise: a frame anyone can send must not be able to take
/// a real machine's network down.
///
/// Armed, a frame the driver sends holding [`fault::MARKER`] makes it write an
/// impossible completion tail into the ring and publish nothing after it. The
/// kernel reads that as a corrupt ring, ends the ring and closes its end of
/// the control channel, which is exactly what a torn index looked like to it
/// on the night of 2026-10-15; the gate then requires this driver to leave
/// with [`Step::Ended`] and `devmgr` to start it again mid-transfer.
mod fault {
    use ferrix_netring::RingMemory;

    /// Whether this build carries the fault.
    pub(super) const ARMED: bool = match option_env!("FERRIX_NET_FAULT") {
        Some(value) => !value.is_empty(),
        None => false,
    };

    /// What a sent frame holds to ask for it (xtask's `net::FAULT_MARKER`).
    const MARKER: &[u8] = b"ferrix-netring-fault";

    /// Where the completion tail is in the ring's header (`docs/NET-RING.md`
    /// §3).
    const COMP_TAIL: usize = 32;

    /// Whether `frame` asks for the fault, in an armed build.
    pub(super) fn asked(frame: &[u8]) -> bool {
        ARMED && frame.windows(MARKER.len()).any(|window| window == MARKER)
    }

    /// Put a completion tail more than a ring ahead of anything published.
    pub(super) fn corrupt<M: RingMemory>(ring: &mut M, entries: u32) {
        let tail = ring.read_u32(COMP_TAIL);
        ring.write_u32(COMP_TAIL, tail.wrapping_add(entries.saturating_mul(2)));
        ring.barrier();
    }
}

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device that is not virtio-net.
    Identity = 2,
    /// A register block could not be mapped, or the interrupt claimed.
    Registers = 3,
    /// A VMO could not be made, pinned or mapped.
    Memory = 4,
    /// The device would not come up.
    Device = 5,
    /// The ring could not be made, or HELLO was refused.
    Ring = 6,
    /// The serve loop stopped on a fault.
    Serve = 7,
    /// The device would not reset.
    Reset = 8,
    /// The kernel ended the ring without asking the driver to stop: it closed
    /// its end of the control channel, as it does when it finds the ring
    /// corrupt. The interface is parked by then, and only a driver started
    /// again takes it up, so this is a failure for whoever started this one
    /// to answer -- never a loop that waits on a channel nobody will write.
    Ended = 9,
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

    /// The little-endian `u16` at `offset`, in one access: the other side may
    /// be writing it now, and two byte reads could take one byte from before
    /// its store and one from after.
    fn read_u16(&self, offset: usize) -> u16 {
        if offset.checked_add(2).is_none_or(|end| end > self.len) {
            return 0;
        }
        let address = self.base + offset;
        assert!(address.is_multiple_of(2), "a u16 on its own alignment");
        // SAFETY: as for `read_u8`, and the two bytes are one aligned `u16`.
        u16::from_le(unsafe { ptr::read_volatile(address as *const u16) })
    }

    /// Write the little-endian `u16` at `offset`, in one access: the other
    /// side may read it at any moment, and must never see one byte changed.
    fn write_u16(&mut self, offset: usize, value: u16) {
        if offset.checked_add(2).is_none_or(|end| end > self.len) {
            return;
        }
        let address = self.base + offset;
        assert!(address.is_multiple_of(2), "a u16 on its own alignment");
        // SAFETY: as for `write_u8`, and the two bytes are one aligned `u16`.
        unsafe { ptr::write_volatile(address as *mut u16, value.to_le()) }
    }

    /// The little-endian `u32` at `offset`, in one access where it is aligned
    /// (`ferrix-netring`'s `RingMemory` says why), composed from bytes where
    /// it is not, which only a hostile layout reaches.
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

    /// Write the little-endian `u32` at `offset`, in one access where it is
    /// aligned, as [`Mapped::read_u32`] reads it.
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

    /// The little-endian `u16` at `offset` for the net ring: one access where
    /// it is aligned, bytes where it is not. [`Mapped::read_u16`] is the
    /// virtqueue's, which only ever asks for aligned ones.
    fn ring_read_u16(&self, offset: usize) -> u16 {
        if (self.base + offset).is_multiple_of(2) {
            self.read_u16(offset)
        } else {
            u16::from_le_bytes([self.read_u8(offset), self.read_u8(offset + 1)])
        }
    }

    /// Write the little-endian `u16` at `offset` for the net ring, as
    /// [`Mapped::ring_read_u16`] reads it.
    fn ring_write_u16(&mut self, offset: usize, value: u16) {
        if (self.base + offset).is_multiple_of(2) {
            self.write_u16(offset, value);
        } else {
            let [low, high] = value.to_le_bytes();
            self.write_u8(offset, low);
            self.write_u8(offset + 1, high);
        }
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

impl DevicePages for Pinned {
    fn device_pages(&self) -> &[u64] {
        self.addresses.get(..self.pages).unwrap_or_default()
    }
}

impl RequestArea for Pinned {
    fn read_u8(&self, offset: usize) -> u8 {
        self.mapped.read_u8(offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.mapped.write_u8(offset, value);
    }
}

// SAFETY: the memory is one VMO, pinned so the device's view of it is the
// pages `device_pages` names, mapped so this process's view is `mapped`, and
// both stay put until the `Pinned` is dropped, which the driver's teardown
// rule forbids while the device may write. Every access is volatile and
// `barrier` is a full fence, so what the device sees is what was written.
// Each `u16` is one access, as the trait requires of the ring's indices.
unsafe impl QueueMemory for Pinned {
    fn read_u8(&self, offset: usize) -> u8 {
        self.mapped.read_u8(offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.mapped.write_u8(offset, value);
    }

    fn read_u16(&self, offset: usize) -> u16 {
        self.mapped.read_u16(offset)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        self.mapped.write_u16(offset, value);
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

    // The indices the kernel moves while this side reads them are `u32`s, and
    // each is one access here: composed from bytes, a tail stepping from
    // 0x00ff to 0x0100 reads 0x01ff on the other side, which is "the peer's
    // tail ran more than a ring ahead" and ends the ring (`ferrix-netring`'s
    // `RingMemory`).

    fn read_u16(&self, offset: usize) -> u16 {
        self.mapped.ring_read_u16(offset)
    }

    fn read_u32(&self, offset: usize) -> u32 {
        self.mapped.read_u32(offset)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        self.mapped.ring_write_u16(offset, value);
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

    // The indices the kernel moves while this side reads them are `u32`s, and
    // each is one access here: composed from bytes, a tail stepping from
    // 0x00ff to 0x0100 reads 0x01ff on the other side, which is "the peer's
    // tail ran more than a ring ahead" and ends the ring (`ferrix-netring`'s
    // `RingMemory`).

    fn read_u16(&self, offset: usize) -> u16 {
        self.mapped.ring_read_u16(offset)
    }

    fn read_u32(&self, offset: usize) -> u32 {
        self.mapped.read_u32(offset)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        self.mapped.ring_write_u16(offset, value);
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.mapped.write_u32(offset, value);
    }
}

// ---------------------------------------------------------------------------
// The device's registers
// ---------------------------------------------------------------------------

/// One virtio register block: the mapping of the pages holding it, and where
/// in them it starts.
struct Block {
    /// Kept so the mapping stays.
    _mapping: IoMapping<Kernel>,
    /// Where the block itself begins.
    base: usize,
    /// How long it is.
    len: usize,
}

impl Block {
    /// Map the pages `device_info` says hold the block.
    fn map(device: &Device<Kernel>, block: &DeviceBlock) -> Result<Block, Step> {
        let offset = block.offset as usize;
        let len = block.length as usize;
        let end = offset.checked_add(len).ok_or(Step::Registers)?;
        let pages = end.div_ceil(PAGE).max(1) * PAGE;
        let mapping = device
            .io_mapping(IoMappingSpec {
                phys: block.phys,
                len: pages as u64,
            })
            .map_err(|_| Step::Registers)?;
        let base = mapping.map(None).map_err(|_| Step::Registers)?;
        Ok(Block {
            _mapping: mapping,
            base: base + offset,
            len,
        })
    }

    /// A register, read volatilely.
    fn read<T: Copy>(&self, offset: u32) -> T {
        let offset = offset as usize;
        assert!(
            offset + size_of::<T>() <= self.len && offset.is_multiple_of(size_of::<T>()),
            "a register inside the block, aligned"
        );
        // SAFETY: the block is mapped device memory the kernel gave this
        // process, the offset is inside it and aligned for `T`, and the access
        // is volatile, as a register read must be.
        unsafe { ptr::read_volatile((self.base + offset) as *const T) }
    }

    /// A register, written volatilely.
    fn write<T: Copy>(&mut self, offset: u32, value: T) {
        let offset = offset as usize;
        assert!(
            offset + size_of::<T>() <= self.len && offset.is_multiple_of(size_of::<T>()),
            "a register inside the block, aligned"
        );
        // SAFETY: as for `read`, and the block is writable.
        unsafe { ptr::write_volatile((self.base + offset) as *mut T, value) }
    }
}

/// The four blocks, the notification arithmetic and the interrupt.
struct Registers {
    /// virtio's common configuration.
    common: Block,
    /// The notification area.
    notify: Block,
    /// The ISR status byte.
    isr: Block,
    /// The device-specific configuration.
    device: Block,
    /// What a `queue_notify_off` is multiplied by.
    notify_off_multiplier: u32,
    /// Whether the device has MSI-X entries.
    msix: bool,
    /// The interrupt both queues arrive on.
    interrupt: Interrupt<Kernel>,
}

impl CommonConfig for Registers {
    fn read8(&self, offset: u32) -> u8 {
        self.common.read(offset)
    }

    fn read16(&self, offset: u32) -> u16 {
        self.common.read(offset)
    }

    fn read32(&self, offset: u32) -> u32 {
        self.common.read(offset)
    }

    fn write8(&mut self, offset: u32, value: u8) {
        self.common.write(offset, value);
    }

    fn write16(&mut self, offset: u32, value: u16) {
        self.common.write(offset, value);
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.common.write(offset, value);
    }
}

impl DeviceConfig for Registers {
    fn config_len(&self) -> u32 {
        self.device.len as u32
    }

    fn config_read8(&self, offset: u32) -> u8 {
        self.device.read(offset)
    }

    fn config_read16(&self, offset: u32) -> u16 {
        self.device.read(offset)
    }

    fn config_read32(&self, offset: u32) -> u32 {
        self.device.read(offset)
    }
}

impl Transport for Registers {
    fn notify(&mut self, queue: u16, notify_off: u16) {
        let at = u32::from(notify_off).saturating_mul(self.notify_off_multiplier);
        self.notify.write(at, queue);
    }

    fn queue_vector(&self, _queue: u16) -> u16 {
        // One entry for both queues: the handler drains both whichever rang,
        // and a device with one vector is what QEMU gives without `vectors=`.
        if self.msix {
            0
        } else {
            ferrix_virtio::pci::NO_VECTOR
        }
    }

    fn acknowledge_interrupt(&mut self) -> u8 {
        if self.msix {
            // Re-arm the entry; a failure here leaves the next interrupt
            // undelivered, which the serve loop's next wait shows as a hang,
            // and nothing better can be done from inside the driver.
            let _ = self.interrupt.ack();
            ferrix_virtio_net::ISR_QUEUE
        } else {
            let status: u8 = self.isr.read(0);
            let _ = self.interrupt.ack();
            status
        }
    }
}

// ---------------------------------------------------------------------------
// The device, as the serve loop sees it
// ---------------------------------------------------------------------------

/// A window onto a mapping somebody else owns.
///
/// The two data regions are moved into the driver, which holds their VMOs and
/// so their mappings, but gives no way to read or write their bytes: it builds
/// descriptors and leaves the payload to whoever asked for it. This is that
/// way -- the address and length of a mapping whose lifetime the driver
/// guarantees for as long as it lives.
#[derive(Clone, Copy)]
struct Window {
    /// Where the mapping starts.
    base: usize,
    /// How long it is.
    len: usize,
}

impl Window {
    /// Copy `bytes` to `offset`, or nothing if it does not fit.
    fn write(&self, offset: u64, bytes: &[u8]) {
        let Ok(at) = usize::try_from(offset) else {
            return;
        };
        if at.saturating_add(bytes.len()) > self.len {
            return;
        }
        for (index, byte) in bytes.iter().enumerate() {
            // SAFETY: the address is inside a mapping the driver holds for as
            // long as this `Window` is used, and the range was checked.
            // Volatile, because the device reads the same bytes.
            unsafe { ptr::write_volatile((self.base + at + index) as *mut u8, *byte) };
        }
    }

    /// Fill `out` from `offset`, or with zeroes if it does not fit.
    fn read(&self, offset: u64, out: &mut [u8]) {
        let Ok(at) = usize::try_from(offset) else {
            return;
        };
        if at.saturating_add(out.len()) > self.len {
            return;
        }
        for (index, byte) in out.iter_mut().enumerate() {
            // SAFETY: as for `write`; the device may be writing the byte at
            // any moment, so it is read volatilely and taken as data.
            *byte = unsafe { ptr::read_volatile((self.base + at + index) as *const u8) };
        }
    }
}

/// The driver's own type, written once.
type Card = Driver<Registers, Pinned, Pinned, Pinned, [Slot; SLOTS]>;

/// The device as [`Nic`] wants it: the driver, and the two windows it does
/// not hand out.
struct Adapter {
    /// The driver.
    card: Card,
    /// Where frames to send are written.
    transmit: Window,
    /// Where frames that arrived are read.
    receive: Window,
    /// Whether a frame sent asked for [`fault`], in an armed build.
    fault_asked: bool,
}

impl Nic for Adapter {
    fn submit(&mut self, frame: &Frame) -> Result<(), SubmitError> {
        self.card.submit(frame)
    }

    fn release(&mut self, buffer: u16) -> Result<(), DeviceError> {
        self.card
            .release(buffer)
            .map_err(|_| DeviceError::NeedsReset)
    }

    fn drain(&mut self, out: &mut [Event], receive_room: usize) -> Result<usize, DeviceError> {
        self.card
            .on_interrupt_receiving(out, receive_room)
            .map(|drained| drained.events)
    }

    fn write_transmit(&mut self, offset: u64, frame: &[u8]) {
        if fault::asked(frame) {
            self.fault_asked = true;
        }
        self.transmit.write(offset, frame);
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

/// Map the registers `device_info` describes and claim the interrupt.
fn registers(device: &Device<Kernel>, info: &DeviceInfo) -> Result<Registers, Step> {
    let interrupt = device.interrupt(0).map_err(|_| Step::Registers)?;
    Ok(Registers {
        common: Block::map(device, &info.common)?,
        notify: Block::map(device, &info.notify)?,
        isr: Block::map(device, &info.isr)?,
        device: Block::map(device, &info.device)?,
        notify_off_multiplier: info.notify_off_multiplier,
        msix: info.msix_table_size > 0,
        interrupt,
    })
}

/// Make the device's memory and bring the device up.
///
/// The interrupt is bound to `port` here, before the registers are moved into
/// the driver: after that the driver owns them, and the interrupt with them.
fn bring_up(
    device: &Device<Kernel>,
    registers: Registers,
    port: &Port<Kernel>,
) -> Result<Adapter, Step> {
    registers
        .interrupt
        .bind(port, KEY_INTERRUPT)
        .map_err(|_| Step::Registers)?;
    let receive_rings = Pinned::new(device, QUEUE_PAGES)?;
    let transmit_rings = Pinned::new(device, QUEUE_PAGES)?;
    let area = Pinned::new(device, AREA_PAGES)?;
    let receive_data = Pinned::new(device, RECEIVE_PAGES)?;
    let transmit_data = Pinned::new(device, TRANSMIT_PAGES)?;
    let transmit = Window {
        base: transmit_data.mapped.base,
        len: transmit_data.mapped.len,
    };
    let receive = Window {
        base: receive_data.mapped.base,
        len: receive_data.mapped.len,
    };
    let parts = Parts {
        transport: registers,
        receive_rings,
        transmit_rings,
        area,
        receive_data,
        transmit_data,
        slots: [Slot::EMPTY; SLOTS],
    };
    let card = Driver::init(
        parts,
        Options {
            max_queue_size: QUEUE_SIZE,
            ..Options::default()
        },
    )
    .map_err(|_| Step::Device)?;
    Ok(Adapter {
        card,
        transmit,
        receive,
        fault_asked: false,
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
    /// The port the kernel rings.
    port: Port<Kernel>,
    /// The port the kernel gave back with READY.
    completion: Port<Kernel>,
}

/// Make the ring, send HELLO, and take READY.
fn open_ring(
    device: &Device<Kernel>,
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

    let info = *adapter.card.info();
    let hello = Message::Hello(Hello {
        version: VERSION,
        entries: ENTRIES,
        slot_bytes: SLOT_BYTES,
        interface: Interface::new(
            INTERFACE_NAME,
            info.mac,
            u32::from(info.limits.mtu),
            InterfaceFlags {
                carrier: info.link_up,
                broadcast: true,
                multicast: true,
            },
        ),
    });
    let mut bytes = [0_u8; MAX_MESSAGE];
    let written = hello.encode(&mut bytes).map_err(|_| Step::Ring)?;
    // Exactly the rights `HELLO_RIGHTS` names, in its order: the kernel
    // refuses a HELLO whose handles carry anything else.
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
    let _ = device;

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

/// Carry frames until the ring ends.
fn serve(
    ring: &mut RingSide,
    adapter: &mut Adapter,
    control: &Channel<Kernel>,
) -> Result<(), Step> {
    let mut serve = Serve::new();
    // The interrupt was bound to this port before the registers went into the
    // driver, and the kernel's bell arrives on it too, so one wait covers
    // everything the loop can be woken by.
    control
        .wait_async(
            &ring.port,
            Signals::READABLE | Signals::PEER_CLOSED,
            KEY_CONTROL,
        )
        .map_err(|_| Step::Ring)?;
    // Set once an armed build has corrupted its ring: nothing is published
    // after it, so the kernel cannot miss it.
    let mut corrupted = false;
    loop {
        let turn = serve
            .turn(&mut ring.ring, &mut ring.data, &mut ring.side, adapter)
            .map_err(|_| Step::Serve)?;
        if fault::ARMED && adapter.fault_asked && !corrupted {
            fault::corrupt(&mut ring.ring, ENTRIES);
            corrupted = true;
        }
        if !corrupted && let Some(bell) = ring.side.publish(&mut ring.ring) {
            let _ = ring
                .completion
                .queue(bell.key(), [u64::from(bell.tail()), 0]);
        }
        match control_state(control) {
            Control::Running => {}
            Control::Stop => return Ok(()),
            Control::Ended => return Err(Step::Ended),
        }
        if turn.busy {
            continue;
        }
        // The handshake is `ferrix-netserve`'s, not this program's: the
        // kernel rings only a driver that has asked to be rung, and the rule
        // about a frame waiting for the device's queue belongs with the queue.
        let wait = serve
            .before_sleep(&mut ring.ring, &mut ring.side)
            .map_err(|_| Step::Serve)?;
        if wait != Wait::Sleep {
            continue;
        }
        let packet = ring.port.wait(Deadline::Never).map_err(|_| Step::Serve)?;
        serve.woke(&mut ring.ring, &mut ring.side);
        match packet.key {
            KEY_CONTROL => {
                let _ = control.wait_async(
                    &ring.port,
                    Signals::READABLE | Signals::PEER_CLOSED,
                    KEY_CONTROL,
                );
            }
            _ => {
                let _ = packet;
            }
        }
    }
}

/// What the control channel says about the ring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Control {
    /// Nothing that ends it.
    Running,
    /// STOP: finish, reset the device and say STOPPED.
    Stop,
    /// The kernel's end is gone, so the ring is over without a STOP.
    Ended,
}

/// Whether the kernel has asked the driver to stop, or ended the ring.
///
/// A closed channel is not "nothing to read". The loop waits on the channel's
/// `PEER_CLOSED` as well as `READABLE`, so once the kernel has closed its end
/// every wait returns at once; taking that for "keep going" turned a ring the
/// kernel ended into a driver spinning on one processor for ever, with its
/// interface parked and nothing restarting it (the night of 2026-10-15).
fn control_state(control: &Channel<Kernel>) -> Control {
    let mut bytes = [0_u8; MAX_MESSAGE];
    let mut handles = [Handle::INVALID; 1];
    match control.read(&mut bytes, &mut handles) {
        Ok(received) => match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
            Ok(Message::Stop) => Control::Stop,
            Ok(_) | Err(_) => Control::Running,
        },
        Err(ReadError::Failed(Error::ShouldWait)) => Control::Running,
        // The peer closed, or the channel cannot be read at all: either way
        // no STOP can come through it any more.
        Err(ReadError::Failed(_)) => Control::Ended,
        Err(_) => Control::Stop,
    }
}

/// The whole run.
fn run(boot: &Channel<Kernel>) -> Result<(), Step> {
    let Started { device, control } = started(boot)?;
    let info = device.info().map_err(|_| Step::Identity)?;
    if !VIRTIO_NET_IDS.contains(&info.device_id) {
        return Err(Step::Identity);
    }
    let registers = registers(&device, &info)?;
    let port = port::create(Kernel).map_err(|_| Step::Ring)?;
    let mut adapter = bring_up(&device, registers, &port)?;
    let mut ring = open_ring(&device, &control, &adapter, port)?;
    let outcome = serve(&mut ring, &mut adapter, &control);
    let reset = finish(adapter, &control);
    outcome.and(reset)
}

/// Reset the device and say STOPPED, or leave without saying it.
fn finish(adapter: Adapter, control: &Channel<Kernel>) -> Result<(), Step> {
    match adapter.card.shutdown() {
        Teardown::Released(_) => {
            let mut bytes = [0_u8; MAX_MESSAGE];
            if let Ok(written) = Message::Stopped.encode(&mut bytes) {
                let _ = control.write(bytes.get(..written).unwrap_or_default());
            }
            Ok(())
        }
        // The device did not reset, so its memory is never given back and the
        // kernel is told nothing: it resets through its own means, and the
        // pins keep the frames until it has. The exit status is what says so.
        Teardown::Wedged(_) => Err(Step::Reset),
    }
}
