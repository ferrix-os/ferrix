//! The virtio-gpu driver process: a ring-3 program that serves one card to
//! the kernel's display core.
//!
//! Everything that knows anything is a library. `ferrix-virtio-gpu` drives
//! the device and turns the core's requests into device commands, and
//! `ferrix-displayctl` is the conversation with the core; both are tested on
//! the host. This program is the handles those libraries were written to be
//! wrapped in (`docs/DISPLAY.md` §2.2):
//!
//! 1. The bootstrap channel carries START, as for the block driver: the
//!    device, the driver's end of the display control channel, and where the
//!    device's virtio register blocks are.
//! 2. Each register block is an `IoMapping`; the rings and the command area
//!    are VMOs this process creates, pins read-write and maps.
//! 3. `GET_DISPLAY_INFO` gives each scanout's preferred mode, which HELLO
//!    carries with a port. READY brings the card VMO back, `READ | TRANSFER`
//!    only: this process pins its ranges read-only as buffers' backing and
//!    can neither write nor map the pixels.
//! 4. One port carries every event: the device's interrupt and the control
//!    channel becoming readable. Requests queue in the pipeline, which says
//!    what to do next: pin, submit a command, unpin, or reply.
//! 5. STOP, or the core closing its end, ends it: the device is reset and
//!    the pins released only if the reset finished.
//!
//! The exit status names the step that failed ([`Step`]), 0 a clean STOP.

#![no_std]
#![no_main]

use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use ferrix_blkring::control::{Block as StartBlock, Message as StartMessage, START_BYTES, Start};
use ferrix_displayctl::message::{
    Hello, MAX_BUFFER_PAGES as MAX_PAGES, MAX_BYTES, MAX_DIMENSION, MAX_SCANOUTS, Message,
    PORT_RIGHTS, ScanoutMode, Timings, VERSION,
};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{
    IoMappingSpec, PACKET_INTERRUPT, PACKET_SIGNAL, PACKET_USER, PortPacket,
};
use ferrix_renderctl::message::{MAX_BYTES as RENDER_MAX_BYTES, Message as RenderMessage};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::device::{Device, Interrupt, IoMapping};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_virtio::QueueMemory;
use ferrix_virtio::gpu::{self, Command, DeviceConfig, DeviceError as Refusal, MemEntry, Response};
use ferrix_virtio::pci::{CommonConfig, NO_VECTOR};
use ferrix_virtio_gpu::pipeline::{Pipeline, Request, Step as Next, TransferOffset};
use ferrix_virtio_gpu::{
    CAPSET_ROOM, CONTROL_SLOTS, CommandArea, DevicePages, Driver, ISR_CONFIG, ISR_QUEUE, Options,
    Parts, SLOT_AREA_BYTES, SubmitError, Teardown, Transport, WATCH_NANOS,
};

ferrix_rt::entry!(main);

/// A page, on every architecture this runs on.
const PAGE: usize = 4096;

/// Pages of queue memory: the control queue's rings fit in one.
const QUEUE_PAGES: usize = 2;

/// The most pages one buffer may have, as the protocol says.
const MAX_BUFFER_PAGES: usize = MAX_PAGES as usize;

/// Bytes of `RESOURCE_ATTACH_BACKING` before its entries.
const ATTACH_BACKING_HEADER: usize = 32;

/// Bytes of one backing entry on the wire.
const ENTRY_BYTES: usize = 16;

/// Pages of command area: the slots, then the large place, whose largest
/// request is a backing list of one entry per page, with the response after
/// it. Sized for the worst case, pages no two of which the device sees side
/// by side, so no ATTACH the protocol allows is too large to submit.
const AREA_PAGES: usize = (SLOT_AREA_BYTES
    + ATTACH_BACKING_HEADER
    + MAX_BUFFER_PAGES * ENTRY_BYTES
    + ferrix_virtio_gpu::RESPONSE_BYTES)
    .div_ceil(PAGE);

/// Buffers pinned at once.
const MAX_PINS: usize = 32;

/// virtio-gpu's modern PCI device id.
const VIRTIO_GPU_ID: u16 = 0x1050;

/// Port keys.
const KEY_INTERRUPT: u64 = 1;
const KEY_CONTROL: u64 = 2;
/// The render core's channel, the card's other conversation.
const KEY_RENDER: u64 = 3;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device that is not virtio-gpu.
    Identity = 2,
    /// A register block could not be mapped.
    Registers = 3,
    /// Memory could not be made, pinned or mapped.
    Memory = 4,
    /// The device would not come up, or would not say its displays.
    Device = 5,
    /// HELLO could not be sent, or READY did not come.
    Hello = 6,
    /// The port, the interrupt or the waits could not be arranged.
    Events = 7,
    /// The device broke the protocol, or the pipeline was out of step.
    Faulted = 8,
    /// The device would not reset: its memory is kept.
    Wedged = 9,
    /// The control channel failed, or the core refused this driver.
    Control = 10,
}

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
// Memory
// ---------------------------------------------------------------------------

/// A VMO mapped into this process.
struct Mapped {
    base: usize,
    len: usize,
}

impl Mapped {
    fn read_u8(&self, offset: usize) -> u8 {
        assert!(offset < self.len, "a read inside the mapping");
        // SAFETY: a mapping the kernel made for this process that lives as
        // long as its VMO handle; the offset was checked; volatile, since
        // the other side is a device.
        unsafe { ptr::read_volatile((self.base + offset) as *const u8) }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        assert!(offset < self.len, "a write inside the mapping");
        // SAFETY: as for `read_u8`, and the mapping is writable.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u8, value) }
    }

    fn read_bytes(&self, offset: usize, out: &mut [u8]) {
        assert!(
            offset
                .checked_add(out.len())
                .is_some_and(|end| end <= self.len),
            "a read inside the mapping"
        );
        // SAFETY: as for `read_u8`, the whole range checked. A response is
        // read only once its completion has been taken, after which the
        // device writes none of it, so a plain copy sees what it wrote; the
        // queue's barrier orders the copy after the completion was read.
        unsafe {
            ptr::copy_nonoverlapping(
                (self.base + offset) as *const u8,
                out.as_mut_ptr(),
                out.len(),
            );
        }
    }

    fn write_bytes(&mut self, offset: usize, bytes: &[u8]) {
        assert!(
            offset
                .checked_add(bytes.len())
                .is_some_and(|end| end <= self.len),
            "a write inside the mapping"
        );
        // SAFETY: as for `write_u8`, the whole range checked. A request is
        // written before its chain is published, and the queue's barrier, a
        // full fence, orders the copy before the index the device reads.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), (self.base + offset) as *mut u8, bytes.len());
        }
    }

    /// The little-endian `u16` at `offset`, in one access: the other side may
    /// be writing it now, and two byte reads could take one byte from before
    /// its store and one from after.
    fn read_u16(&self, offset: usize) -> u16 {
        assert!(
            offset.checked_add(2).is_some_and(|end| end <= self.len),
            "a u16 inside the mapping"
        );
        let address = self.base + offset;
        assert!(address.is_multiple_of(2), "a u16 on its own alignment");
        // SAFETY: as for `read_u8`, and the two bytes are one aligned `u16`.
        u16::from_le(unsafe { ptr::read_volatile(address as *const u16) })
    }

    /// Write the little-endian `u16` at `offset`, in one access: the other
    /// side may read it at any moment, and must never see one byte changed.
    fn write_u16(&mut self, offset: usize, value: u16) {
        assert!(
            offset.checked_add(2).is_some_and(|end| end <= self.len),
            "a u16 inside the mapping"
        );
        let address = self.base + offset;
        assert!(address.is_multiple_of(2), "a u16 on its own alignment");
        // SAFETY: as for `write_u8`, and the two bytes are one aligned `u16`.
        unsafe { ptr::write_volatile(address as *mut u16, value.to_le()) }
    }
}

/// A VMO this process made, pinned read-write for the device and mapped.
struct Pinned {
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
    mapped: Mapped,
    addresses: [u64; AREA_PAGES],
    pages: usize,
}

impl Pinned {
    fn new(device: &Device<Kernel>, pages: usize) -> Result<Pinned, Step> {
        let bytes = pages * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        let pin = device
            .pin(&vmo, 0, bytes, PinAccess::ReadWrite)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; AREA_PAGES];
        let asked = raw.get_mut(..pages).ok_or(Step::Memory)?;
        let got = pin.addresses(asked).map_err(|_| Step::Memory)?;
        if !got.is_complete() || got.pages != pages {
            return Err(Step::Memory);
        }
        let mut addresses = [0_u64; AREA_PAGES];
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

impl CommandArea for Pinned {
    fn read_u8(&self, offset: usize) -> u8 {
        self.mapped.read_u8(offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.mapped.write_u8(offset, value);
    }

    fn read_bytes(&self, offset: usize, out: &mut [u8]) {
        self.mapped.read_bytes(offset, out);
    }

    fn write_bytes(&mut self, offset: usize, bytes: &[u8]) {
        self.mapped.write_bytes(offset, bytes);
    }
}

// SAFETY: one VMO, pinned so the device sees the pages `device_pages` names
// and mapped so this process sees `mapped`, both until the `Pinned` is
// dropped, which the driver's teardown rule forbids while the device may
// write. Every access is volatile and `barrier` is a full fence.
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

/// Scratch for pinning a buffer: its pages' addresses as the pin query writes
/// them, and the backing entries made from them. A mapped VMO, since both
/// are too large for a stack.
struct Scratch {
    _vmo: Vmo<Kernel>,
    raw: usize,
    entries: usize,
}

impl Scratch {
    fn new() -> Result<Scratch, Step> {
        let raw_bytes = MAX_BUFFER_PAGES * 8;
        let entry_bytes = MAX_BUFFER_PAGES * size_of::<MemEntry>();
        let bytes = (raw_bytes + entry_bytes).div_ceil(PAGE) * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        let base = vmo
            .map(None, bytes, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        let entries = (base + raw_bytes).next_multiple_of(align_of::<MemEntry>());
        if entries + entry_bytes > base + bytes {
            return Err(Step::Memory);
        }
        Ok(Scratch {
            _vmo: vmo,
            raw: base,
            entries,
        })
    }

    /// Room for `pages` addresses as the pin query writes them.
    fn raw(&mut self, pages: usize) -> &mut [[u8; 8]] {
        // SAFETY: `raw` starts a zero-filled private mapping of at least
        // `MAX_BUFFER_PAGES` eight-byte arrays, which have no alignment
        // requirement; `pages` is capped to that; the `&mut self` borrow
        // keeps the slice unique.
        unsafe {
            core::slice::from_raw_parts_mut(self.raw as *mut [u8; 8], pages.min(MAX_BUFFER_PAGES))
        }
    }

    /// The backing entries, all [`MAX_BUFFER_PAGES`] of them.
    fn entries(&self) -> &[MemEntry] {
        // SAFETY: `entries` is aligned for `MemEntry` inside a zero-filled
        // mapping holding `MAX_BUFFER_PAGES` of them, and all-zero is a valid
        // `MemEntry`; shared, as `&self` is.
        unsafe { core::slice::from_raw_parts(self.entries as *const MemEntry, MAX_BUFFER_PAGES) }
    }

    fn entries_mut(&mut self) -> &mut [MemEntry] {
        // SAFETY: as for `entries`, unique through `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.entries as *mut MemEntry, MAX_BUFFER_PAGES) }
    }
}

// ---------------------------------------------------------------------------
// The device's registers
// ---------------------------------------------------------------------------

/// One virtio register block.
struct Block {
    _mapping: IoMapping<Kernel>,
    base: usize,
    len: usize,
}

impl Block {
    fn map(device: &Device<Kernel>, block: &StartBlock) -> Result<Block, Step> {
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

    fn read<T: Copy>(&self, offset: u32) -> T {
        let offset = offset as usize;
        assert!(
            offset + size_of::<T>() <= self.len && offset.is_multiple_of(size_of::<T>()),
            "a register inside the block, aligned"
        );
        // SAFETY: mapped device memory the kernel gave this process, the
        // offset inside it and aligned for `T`, read volatile.
        unsafe { ptr::read_volatile((self.base + offset) as *const T) }
    }

    fn write<T: Copy>(&mut self, offset: u32, value: T) {
        let offset = offset as usize;
        assert!(
            offset + size_of::<T>() <= self.len && offset.is_multiple_of(size_of::<T>()),
            "a register inside the block, aligned"
        );
        // SAFETY: as for `read`, and the mapping is writable.
        unsafe { ptr::write_volatile((self.base + offset) as *mut T, value) }
    }
}

/// The device as `ferrix-virtio-gpu` drives it.
struct Registers {
    common: Block,
    notify: Block,
    isr: Block,
    device: Block,
    notify_off_multiplier: u32,
    msix: bool,
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

    fn queue_vector(&self) -> u16 {
        if self.msix { 0 } else { NO_VECTOR }
    }

    fn acknowledge_interrupt(&mut self) -> u8 {
        if self.msix {
            let _ = self.interrupt.ack();
            ISR_QUEUE
        } else {
            let status: u8 = self.isr.read(0);
            let _ = self.interrupt.ack();
            status
        }
    }

    fn config_write32(&mut self, offset: u32, value: u32) {
        self.device.write(offset, value);
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

type Gpu = Driver<Registers, Pinned, Pinned>;

/// What START handed over.
struct Started {
    start: Start,
    device: Device<Kernel>,
    control: Channel<Kernel>,
}

fn started(boot: &Channel<Kernel>) -> Result<Started, Step> {
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Start)?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 2];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Start)?;
    if received.handles != 2 {
        return Err(Step::Start);
    }
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));
    let control = Channel::from_owned(OwnedHandle::from_raw(Kernel, handles[1]));
    match StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(StartMessage::Start(start)) => Ok(Started {
            start,
            device,
            control,
        }),
        _ => Err(Step::Start),
    }
}

/// The render half of the card: `src/lib/proto/renderctl`'s HELLO, and then the one
/// context the core asks for.
///
/// A card has two conversations (`docs/GPU.md` §3.3) and this driver serves
/// both, as one Linux driver serves `card0` and `renderD128`. What is here
/// is the *adapter*: renderctl's messages in, virtio-gpu's 3D commands out.
/// A second driver for another GPU writes this function against its own
/// device and nothing above it changes.
///
/// This half is only the introduction -- HELLO, and READY's work VMO. The
/// requests that follow are served by [`Serving`], because the core keeps
/// the conversation open for as long as the render node exists.
fn render_introduce(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    device: &Device<Kernel>,
    location: u32,
) -> Result<Option<RenderSide>, Step> {
    use ferrix_renderctl::message::{
        self as rc, MAX_BYTES as RENDER_MAX_BYTES, Message as RenderMessage,
    };

    // A card with no 3D behind it has no render node to serve.
    if driver.info().features & gpu::FEATURE_VIRGL == 0 {
        return Ok(None);
    }
    let Ok(control) = device.render_control() else {
        return Ok(None);
    };
    let Some(name) = rc::Hello::named("virtio_gpu") else {
        return Ok(None);
    };
    // Which capability sets a context may be made for, asked for afresh:
    // the display's HELLO read them too, and a driver that kept one number in
    // two places would eventually disagree with itself. virgl's is the one
    // the compositor's streams are in; the device lists its sets oldest
    // first and a renderer wants the newest it knows, so `CAPSET_VIRGL2` is
    // taken over `CAPSET_VIRGL` when both are there. Venus's is offered only
    // where blobs and rings are: it keeps everything in host memory and
    // fences on rings (`docs/GPU.md` §6.1).
    let granted = driver.info().features;
    let blobs = granted & gpu::FEATURE_RESOURCE_BLOB != 0;
    let rings = granted & gpu::FEATURE_CONTEXT_INIT != 0;
    let mut capsets: [Option<gpu::CapsetInfo>; 2] = [None, None];
    for index in 0..driver.info().config.num_capsets.min(MAX_CAPSETS) {
        let Ok(Response::CapsetInfo(info)) =
            run_command(driver, port, &Command::GetCapsetInfo { index })?
        else {
            continue;
        };
        match info.id {
            gpu::CAPSET_VIRGL | gpu::CAPSET_VIRGL2
                if capsets[0].is_none_or(|held| info.id > held.id) =>
            {
                capsets[0] = Some(info);
            }
            gpu::CAPSET_VENUS if blobs && rings => capsets[1] = Some(info),
            _ => {}
        }
    }
    let mut features = rc::features::SUBMIT;
    if blobs {
        features |= rc::features::BLOBS;
    }
    if rings {
        features |= rc::features::RINGS;
    }
    let stream = Stream::new()?;
    let hello = RenderMessage::Hello(rc::Hello {
        version: rc::VERSION,
        location,
        name,
        features,
        capsets: capsets.iter().flatten().fold(0, |mask, info| {
            mask | 1_u32.checked_shl(info.id).unwrap_or(0)
        }),
        object_max: MAX_OBJECT_BYTES,
    });
    let share = port
        .as_owned()
        .duplicate(Requested::Exactly(PORT_RIGHTS))
        .map_err(|_| Step::Hello)?;
    control
        .write_with(hello.encode().as_bytes(), [share])
        .map_err(|_| Step::Hello)?;

    // READY ends the setup, and the conversation is served from the driver's
    // own event loop after that. It cannot be served here: this function
    // would then hold the process while the core kept the channel open, and
    // the display -- the card's other conversation, on the same thread and
    // the same device -- would never run again.
    let mut bytes = [0_u8; RENDER_MAX_BYTES];
    let mut handles = [Handle::INVALID; 2];
    // READY's work VMO, kept for as long as the conversation lasts: an
    // object's description and a command buffer are ranges of it, and the
    // core writes them there rather than into a message. The core's port,
    // which comes beside it, is for fences and is closed until something
    // waits on one.
    let mut work: Option<Vmo<Kernel>> = None;
    let _ = control
        .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
        .map_err(|_| Step::Events)?;
    let Ok(received) = control.read(&mut bytes, &mut handles) else {
        return Ok(None);
    };
    let ready = match RenderMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Some(RenderMessage::Ready(ready)) => Some(ready),
        _ => None,
    };
    let taken = ready.is_some() && received.handles == 2;
    for (index, handle) in handles.iter_mut().enumerate().take(received.handles) {
        let owned = OwnedHandle::from_raw(Kernel, *handle);
        match (taken, index) {
            (true, 0) => work = Some(Vmo::from_owned(owned)),
            _ => drop(owned),
        }
        *handle = Handle::INVALID;
    }
    // Anything but READY with its two handles is a core this driver cannot
    // talk to, and the render node simply does not appear.
    if !taken {
        return Ok(None);
    }
    // The work VMO pinned for the device, so that a command buffer is run
    // where the core wrote it. A card whose pin fails copies instead.
    let window = work
        .as_ref()
        .zip(ready)
        .and_then(|(vmo, ready)| WorkWindow::pin(device, vmo, ready.work_bytes));
    Ok(Some(RenderSide {
        control,
        work,
        window,
        capsets,
        stream,
        backings: [const { None }; MAX_BACKINGS],
        mapped: [0; MAX_BACKINGS],
    }))
}

/// The render conversation, once the core has answered READY.
///
/// Served from [`Serving::serve`] rather than by a loop of its own, so that
/// the display's requests and the GPU's go to the device in the order they
/// were asked for.
struct RenderSide {
    control: Channel<Kernel>,
    /// READY's work VMO: an object's description and a command buffer are
    /// ranges of it. The driver reads those ranges and never writes them.
    work: Option<Vmo<Kernel>>,
    /// The work VMO pinned read-only for the device, which then reads a
    /// command buffer where it lies.
    window: Option<WorkWindow>,
    /// The capability sets HELLO named, which `GET_CAPS` fetches: virgl's
    /// and Venus's.
    capsets: [Option<gpu::CapsetInfo>; 2],
    /// Where a command buffer is copied to on its way from the work VMO,
    /// which this process may read and may not map, to the command area.
    stream: Stream,
    /// The backing of every mappable object the device holds: the core's
    /// VMO and this driver's pin of it. One stays here for good if the
    /// device would not let its object go, which is the rule that pages a
    /// device may still hold are never unpinned.
    backings: [Option<Backing>; MAX_BACKINGS],
    /// The blobs mapped into the device's window, by object; 0 is none. One
    /// is taken out of the window before it goes.
    mapped: [u32; MAX_BACKINGS],
}

/// One mappable object's backing, pinned for the device.
struct Backing {
    object: u32,
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
}

/// The most mappable objects pinned at once: as many as the core's session
/// tracks, so a pin slot is never what refuses an object it would take.
const MAX_BACKINGS: usize = ferrix_renderctl::session::MAX_OBJECTS;

/// The most capability sets asked about. virtio-gpu defines a handful.
const MAX_CAPSETS: u32 = 8;

/// The longest command buffer one `SUBMIT` may name, which is the core's
/// slot and fits the command area beside its header and the response.
const STREAM_BYTES: usize = 64 * 1024;

/// Pages of the largest work VMO pinned for the device: the render core's
/// is a megabyte.
const WORK_PAGES: usize = 256;

/// The most runs of device-consecutive pages one command buffer covers: a
/// page each, and one more for a buffer that starts part way into one.
const MAX_STREAM_RUNS: usize = STREAM_BYTES / PAGE + 1;

/// The work VMO, pinned read-only for the device for as long as the render
/// conversation lasts: a command buffer is handed over where the core wrote
/// it, as the address of each page it covers after `SUBMIT_3D`'s header,
/// rather than copied through this process and into the command area --
/// Genode's "run the buffer at this offset", which `docs/GPU.md` §3.9 names.
///
/// The core holds a buffer's slot until its `SUBMITTED`, which is sent only
/// when the device has finished the command, so a slot is never rewritten
/// under the device. The device is virtio's, which is cache-coherent, so the
/// core's writes need no cleaning before it reads them.
struct WorkWindow {
    _pin: Pin<Kernel>,
    pages: [u64; WORK_PAGES],
    bytes: usize,
}

impl WorkWindow {
    fn pin(device: &Device<Kernel>, vmo: &Vmo<Kernel>, bytes: u64) -> Option<WorkWindow> {
        let bytes = usize::try_from(bytes).ok()?;
        let count = bytes / PAGE;
        if count == 0 || count > WORK_PAGES || !bytes.is_multiple_of(PAGE) {
            return None;
        }
        let pin = device.pin(vmo, 0, bytes, PinAccess::ReadOnly).ok()?;
        let mut raw = [[0_u8; 8]; WORK_PAGES];
        let got = pin.addresses(raw.get_mut(..count)?).ok()?;
        if !got.is_complete() || got.pages != count {
            return None;
        }
        let mut pages = [0_u64; WORK_PAGES];
        for (slot, address) in pages.iter_mut().zip(raw.iter().take(count)) {
            *slot = device_address(*address);
        }
        Some(WorkWindow {
            _pin: pin,
            pages,
            bytes,
        })
    }

    /// The device's view of `len` bytes from `at`, as runs of consecutive
    /// device addresses written into `out`: how many, or `None` for a range
    /// outside the window or one `out` has no room for.
    fn runs(&self, at: usize, len: usize, out: &mut [(u64, u32)]) -> Option<usize> {
        let end = at.checked_add(len).filter(|&end| end <= self.bytes)?;
        if len == 0 {
            return None;
        }
        let mut count = 0usize;
        let mut offset = at;
        while offset < end {
            let chunk = (PAGE - offset % PAGE).min(end - offset);
            let address = self.pages.get(offset / PAGE)? + (offset % PAGE) as u64;
            let chunk = u32::try_from(chunk).ok()?;
            let joined = count
                .checked_sub(1)
                .and_then(|last| out.get_mut(last))
                .filter(|(start, run)| start + u64::from(*run) == address);
            if let Some((_, run)) = joined {
                *run += chunk;
            } else {
                *out.get_mut(count)? = (address, chunk);
                count += 1;
            }
            offset += chunk as usize;
        }
        Some(count)
    }
}

/// A private buffer a command buffer is read into, for a card whose work
/// VMO could not be pinned.
struct Stream {
    _vmo: Vmo<Kernel>,
    base: usize,
}

impl Stream {
    fn new() -> Result<Stream, Step> {
        let vmo = vmo::create(Kernel, STREAM_BYTES).map_err(|_| Step::Memory)?;
        let base = vmo
            .map(None, STREAM_BYTES, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        Ok(Stream { _vmo: vmo, base })
    }

    fn bytes(&mut self, len: usize) -> &mut [u8] {
        // SAFETY: `base` starts a private, zero-filled, writable mapping of
        // `STREAM_BYTES` that lives as long as this process; `len` is capped
        // to it; the `&mut self` borrow keeps the slice unique.
        unsafe { core::slice::from_raw_parts_mut(self.base as *mut u8, len.min(STREAM_BYTES)) }
    }
}

/// Make one rendering context on the device.
///
/// virtio's own shape: the id rides in the header, and `context_init`
/// carries the capability set.
fn make_context(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    context: u32,
    capset: u32,
) -> Result<ferrix_renderctl::message::Message, Step> {
    let made = run_command_in(
        driver,
        port,
        gpu::Context {
            id: context,
            ..gpu::Context::NONE
        },
        &Command::CtxCreate {
            capset: u8::try_from(capset).unwrap_or(0),
            name: "ferrix",
        },
    )?;
    Ok(ferrix_renderctl::message::Message::ContextMade {
        context,
        status: status_of(made),
    })
}

/// Make one object nameable in a context that did not make it: the one a
/// program imported it into as a dmabuf (`docs/GPU.md` §3.13).
///
/// The same command the object's maker was given in its own context; the
/// device takes it again for a context that has it already, as Linux's
/// virtio-gpu sends it for every handle a context opens. Nothing is pinned
/// or unpinned: the backing stays the maker's.
fn attach_object(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    object: u32,
    context: u32,
) -> Result<ferrix_renderctl::message::Message, Step> {
    let attached = run_command_in(
        driver,
        port,
        gpu::Context {
            id: context,
            ..gpu::Context::NONE
        },
        &Command::CtxAttachResource {
            resource_id: object,
        },
    )?;
    Ok(ferrix_renderctl::message::Message::ObjectAttached {
        object,
        status: status_of(attached),
    })
}

/// Destroy one rendering context.
fn drop_context(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    context: u32,
) -> Result<ferrix_renderctl::message::Message, Step> {
    let gone = run_command_in(
        driver,
        port,
        gpu::Context {
            id: context,
            ..gpu::Context::NONE
        },
        &Command::CtxDestroy,
    )?;
    Ok(ferrix_renderctl::message::Message::ContextGone {
        context,
        status: status_of(gone),
    })
}

/// Give one object back to the device, and its backing back to the core.
///
/// The pin goes only when the device has said the resource is gone. One it
/// would not let go of keeps its pages pinned for as long as this process
/// lives, because the device may still read or write them.
fn drop_object(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    side: &mut RenderSide,
    object: u32,
) -> Result<ferrix_renderctl::message::Message, Step> {
    // A mapped blob leaves the window first. One the device would not take
    // out stays, and so does its place: the core gives a place back only for
    // an object that went.
    if let Some(slot) = side.mapped.iter().position(|held| *held == object) {
        let unmapped = run_command(
            driver,
            port,
            &Command::ResourceUnmapBlob {
                resource_id: object,
            },
        )?;
        if unmapped.is_err() {
            return Ok(ferrix_renderctl::message::Message::ObjectGone {
                object,
                status: status_of(unmapped),
            });
        }
        if let Some(held) = side.mapped.get_mut(slot) {
            *held = 0;
        }
    }
    let gone = run_command(
        driver,
        port,
        &Command::ResourceUnref {
            resource_id: object,
        },
    )?;
    if gone.is_ok()
        && let Some(slot) = side
            .backings
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|held| held.object == object))
    {
        *slot = None;
    }
    Ok(ferrix_renderctl::message::Message::ObjectGone {
        object,
        status: status_of(gone),
    })
}

/// Make one object on the device: the resource itself, its backing, and the
/// context's right to name it.
///
/// This is the device-specific half of the adapter, and the whole of what a
/// second GPU rewrites (`docs/GPU.md` §3.3). The core said how many bytes it
/// wants and handed over where they live; what a resource *is* on this
/// device is known here and nowhere above.
fn make_object(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    device: &Device<Kernel>,
    scratch: &mut Scratch,
    side: &mut RenderSide,
    make: &ferrix_renderctl::message::MakeObject,
    backing: Option<Vmo<Kernel>>,
) -> Result<ferrix_renderctl::message::Message, Step> {
    use ferrix_renderctl::message::{Message as RenderMessage, Status, flags};

    let answer = |status| {
        Ok(RenderMessage::ObjectMade {
            object: make.object,
            status,
        })
    };
    // A mappable object is one that came with its backing, and the other
    // way about: either without the other is a core this cannot serve.
    if (make.flags & flags::MAPPABLE != 0) != backing.is_some() {
        return answer(Status::Invalid);
    }
    let Some(slot) = side.backings.iter().position(Option::is_none) else {
        return answer(Status::OutOfMemory);
    };
    let shape = described(side.work.as_ref(), make);
    let made = run_command_in(
        driver,
        port,
        gpu::Context {
            id: make.context,
            ..gpu::Context::NONE
        },
        &Command::ResourceCreate3d {
            resource_id: make.object,
            target: shape.target,
            format: shape.format,
            bind: shape.bind,
            size: gpu::Box3d {
                x: 0,
                y: 0,
                z: 0,
                width: shape.width,
                height: shape.height,
                depth: shape.depth,
            },
            array_size: shape.array_size,
            last_level: shape.last_level,
            samples: shape.samples,
            flags: shape.flags,
        },
    )?;
    if made.is_err() {
        return answer(status_of(made));
    }
    // The backing, pinned for as long as the device has the resource. The
    // device writes it on a transfer from itself, so an object made to be
    // read back is pinned writable and any other is not.
    let mut status = Status::Ok;
    if let Some(vmo) = backing {
        let access = if make.flags & flags::FROM_DEVICE != 0 {
            PinAccess::ReadWrite
        } else {
            PinAccess::ReadOnly
        };
        let pages = usize::try_from(make.bytes)
            .unwrap_or(usize::MAX)
            .div_ceil(PAGE);
        match pin_entries(device, scratch, &vmo, 0, pages.saturating_mul(PAGE), access) {
            Ok((pin, count)) => {
                let attached = run_command(
                    driver,
                    port,
                    &Command::ResourceAttachBacking {
                        resource_id: make.object,
                        entries: scratch.entries().get(..count).unwrap_or_default(),
                    },
                )?;
                status = status_of(attached);
                if let Some(held) = side.backings.get_mut(slot) {
                    // Kept even when the attach was refused: the unref below
                    // decides whether the device still holds anything.
                    *held = Some(Backing {
                        object: make.object,
                        _pin: pin,
                        _vmo: vmo,
                    });
                }
            }
            Err(()) => status = Status::PinFailed,
        }
    }
    // A resource a context may name has to be given to it: a 3D command
    // naming one that was not is refused by the device.
    if status == Status::Ok && make.context != 0 {
        let attached = run_command_in(
            driver,
            port,
            gpu::Context {
                id: make.context,
                ..gpu::Context::NONE
            },
            &Command::CtxAttachResource {
                resource_id: make.object,
            },
        )?;
        status = status_of(attached);
    }
    // Half-made is not made: the core frees the id on anything but `Ok`, so
    // the resource must not outlive the refusal.
    if status != Status::Ok {
        let _ = drop_object(driver, port, side, make.object)?;
    }
    answer(status)
}

/// Make one blob on the device, and map it into the window where the core
/// said: Venus's host memory (`docs/GPU.md` §6.1).
///
/// Made in the context that named its `blob_id`, which is where the device
/// asks for the memory; mapped as Linux maps one, straight after, so the
/// core can hand a program its pages as soon as it is answered. A blob that
/// could not be mapped is given back rather than left half-made: the core
/// frees the id on anything but `Ok`.
fn make_blob(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    side: &mut RenderSide,
    make: &ferrix_renderctl::message::MakeBlob,
) -> Result<ferrix_renderctl::message::Message, Step> {
    use ferrix_renderctl::message::{Message as RenderMessage, NO_WINDOW, Status};

    let answer = |status, map_info| {
        Ok(RenderMessage::BlobMade {
            object: make.object,
            status,
            map_info,
        })
    };
    let slot = if make.window == NO_WINDOW {
        None
    } else {
        let Some(slot) = side.mapped.iter().position(|held| *held == 0) else {
            return answer(Status::OutOfMemory, 0);
        };
        Some(slot)
    };
    let made = run_command_in(
        driver,
        port,
        in_context(make.context),
        &Command::ResourceCreateBlob {
            resource_id: make.object,
            blob_mem: make.memory,
            blob_flags: make.flags,
            blob_id: make.blob_id,
            size: make.bytes,
            entries: &[],
        },
    )?;
    if made.is_err() {
        return answer(status_of(made), 0);
    }
    let Some(slot) = slot else {
        return answer(Status::Ok, 0);
    };
    let mapped = run_command(
        driver,
        port,
        &Command::ResourceMapBlob {
            resource_id: make.object,
            offset: make.window,
        },
    )?;
    if let Ok(Response::MapInfo { map_info }) = mapped {
        if let Some(held) = side.mapped.get_mut(slot) {
            *held = make.object;
        }
        return answer(Status::Ok, map_info);
    }
    let status = if mapped.is_ok() {
        Status::Invalid
    } else {
        status_of(mapped)
    };
    let _ = run_command(
        driver,
        port,
        &Command::ResourceUnref {
            resource_id: make.object,
        },
    )?;
    answer(status, 0)
}

/// A context's header fields: its id, and no fence.
const fn in_context(id: u32) -> gpu::Context {
    gpu::Context {
        id,
        ..gpu::Context::NONE
    }
}

/// The command that moves bytes between an object's backing and the
/// device's copy.
fn transfer_command(transfer: &ferrix_renderctl::message::Transfer) -> Command<'static> {
    use ferrix_renderctl::message::Direction;

    let region = gpu::Box3d {
        x: transfer.region.x,
        y: transfer.region.y,
        z: transfer.region.z,
        width: transfer.region.width,
        height: transfer.region.height,
        depth: transfer.region.depth,
    };
    match transfer.direction {
        Direction::ToDevice => Command::TransferToHost3d {
            region,
            offset: transfer.offset,
            resource_id: transfer.object,
            level: transfer.level,
            stride: transfer.stride,
            layer_stride: transfer.layer_stride,
        },
        Direction::FromDevice => Command::TransferFromHost3d {
            region,
            offset: transfer.offset,
            resource_id: transfer.object,
            level: transfer.level,
            stride: transfer.stride,
            layer_stride: transfer.layer_stride,
        },
    }
}

/// Fetch a capability set and hand its bytes over in a VMO of this
/// process's making. `None` is a reply that could not be sent at all.
fn get_caps(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    side: &RenderSide,
    capset: u32,
    version: u32,
) -> Result<Option<()>, Step> {
    use ferrix_renderctl::message::{Message as RenderMessage, Status};

    let refuse = |status| {
        side.control
            .write(
                RenderMessage::Caps {
                    capset,
                    status,
                    len: 0,
                }
                .encode()
                .as_bytes(),
            )
            .map_err(|_| Step::Control)
            .map(Some)
    };
    let Some(info) = side
        .capsets
        .iter()
        .flatten()
        .find(|info| info.id == capset)
        .copied()
    else {
        return refuse(Status::Invalid);
    };
    if info.max_size > CAPSET_ROOM {
        return refuse(Status::Invalid);
    }
    let fetched = run_command(
        driver,
        port,
        &Command::GetCapset {
            capset_id: info.id,
            // Version 0 is the core asking for the newest there is.
            capset_version: if version == 0 {
                info.max_version
            } else {
                version
            },
            max_size: info.max_size,
        },
    )?;
    let Ok(Response::Capset { len }) = fetched else {
        return refuse(status_of(fetched));
    };
    let mut bytes = [0_u8; CAPSET_ROOM as usize];
    let set = bytes.get_mut(..len).ok_or(Step::Device)?;
    driver.read_response(0, set);
    let Ok(vmo) = vmo::create(Kernel, len.div_ceil(PAGE).max(1) * PAGE) else {
        return refuse(Status::OutOfMemory);
    };
    if vmo.write(set, 0).is_err() {
        return refuse(Status::OutOfMemory);
    }
    let caps = RenderMessage::Caps {
        capset,
        status: Status::Ok,
        len: u32::try_from(len).unwrap_or(0),
    };
    side.control
        .write_with(caps.encode().as_bytes(), [vmo.into_owned()])
        .map_err(|_| Step::Control)?;
    Ok(Some(()))
}

/// The largest object this driver will make: what one virtio-gpu resource
/// may reasonably be, and far inside what the protocol allows.
const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// virgl's `PIPE_BUFFER`, from Mesa's `p_defines.h`: a resource with no
/// shape, which is what bytes on their way to a shader are.
const PIPE_BUFFER: u32 = 0;

/// virgl's `VIRGL_FORMAT_R8_UNORM`, from virglrenderer's `virgl_hw.h`: one
/// byte a pixel, which is how a buffer's bytes are counted.
const VIRGL_FORMAT_R8_UNORM: u32 = 64;

/// virgl's `VIRGL_BIND_VERTEX_BUFFER`, from the same header. virgl numbers
/// some of its bind bits differently from Mesa's `PIPE_BIND_*`, so they are
/// taken from virgl's header and not from gallium's.
const VIRGL_BIND_VERTEX_BUFFER: u32 = 1 << 4;

/// A resource's shape, in virgl's words.
struct Shape {
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    samples: u32,
    flags: u32,
}

/// What to make of a `MAKE_OBJ`'s description: the resource's shape.
///
/// An empty description is the core saying "so many bytes, and the rest is
/// yours", which is a plain buffer. A description of its own is ten
/// little-endian words, the ones `VIRTGPU_RESOURCE_CREATE` carries and in
/// its order, which the render node writes and never reads. Anything else
/// is treated as empty rather than half-read.
fn described(work: Option<&Vmo<Kernel>>, make: &ferrix_renderctl::message::MakeObject) -> Shape {
    const WORDS: usize = 10;
    let plain = Shape {
        target: PIPE_BUFFER,
        format: VIRGL_FORMAT_R8_UNORM,
        bind: VIRGL_BIND_VERTEX_BUFFER,
        width: u32::try_from(make.bytes).unwrap_or(u32::MAX),
        height: 1,
        depth: 1,
        array_size: 1,
        last_level: 0,
        samples: 0,
        flags: 0,
    };
    let mut bytes = [0_u8; WORDS * 4];
    if make.describe.len as usize != bytes.len() {
        return plain;
    }
    let Some(work) = work else {
        return plain;
    };
    if work.read(&mut bytes, u64::from(make.describe.at)).is_err() {
        return plain;
    }
    let mut words = bytes
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap_or_default()));
    let mut next = || words.next().unwrap_or(0);
    Shape {
        target: next(),
        format: next(),
        bind: next(),
        width: next(),
        height: next(),
        depth: next(),
        array_size: next(),
        last_level: next(),
        samples: next(),
        flags: next(),
    }
}

/// A device's answer, as the render protocol says it.
fn status_of(answer: Result<Response, Refusal>) -> ferrix_renderctl::message::Status {
    use ferrix_renderctl::message::Status;
    match answer {
        Ok(_) => Status::Ok,
        Err(Refusal::OutOfMemory) => Status::OutOfMemory,
        Err(Refusal::InvalidParameter | Refusal::InvalidResourceId | Refusal::InvalidContextId) => {
            Status::Invalid
        }
        Err(_) => Status::DeviceRefused,
    }
}

/// [`run_command`] for a command that belongs to a context.
///
/// Only with nothing else in flight: bring-up, and the render requests that
/// wait for an idle device. Any completion but its own is a driver out of
/// step with itself.
fn run_command_in(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    context: gpu::Context,
    command: &Command<'_>,
) -> Result<Result<Response, Refusal>, Step> {
    driver
        .submit_in(context, command)
        .map_err(|_| Step::Device)?;
    let mut deferred = Deferred::new();
    let result = loop {
        let packet = match port.wait(watch(driver)) {
            Ok(packet) => packet,
            Err(Error::TimedOut) => {
                driver.check_needs_reset().map_err(|_| Step::Device)?;
                continue;
            }
            Err(_) => return Err(Step::Events),
        };
        if (packet.kind, packet.key) != (PACKET_INTERRUPT, KEY_INTERRUPT) {
            deferred.keep(&packet);
            continue;
        }
        let _ = driver.on_interrupt().map_err(|_| Step::Device)?;
        let mut mine = None;
        while let Some(done) = driver.take_done().map_err(|_| Step::Device)? {
            if done.tag != TAG_BLOCKING {
                return Err(Step::Faulted);
            }
            mine = Some(done.result);
        }
        if let Some(result) = mine {
            break result;
        }
    };
    deferred.give_back(port);
    Ok(result)
}

/// Submit a command and wait for its outcome.
fn run_command(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    command: &Command<'_>,
) -> Result<Result<Response, Refusal>, Step> {
    run_command_in(driver, port, gpu::Context::NONE, command)
}

/// How many packets one wait may keep: the display's channel, the render
/// core's, and room to spare.
const MAX_DEFERRED: usize = 4;

/// Packets taken off the port while waiting for a command's interrupt.
///
/// They belong to whoever armed them and cannot be answered here, in the
/// middle of a device command -- but they must not be dropped either:
/// `wait_async` delivers once, so a packet thrown away is a channel nobody
/// is ever woken for again, which is a driver that stops serving the
/// display. They are given back to the port when the command is done, as
/// user packets standing for the signal they were, and the serve loop reads
/// both forms alike.
///
/// Giving one back before then would only fetch it straight out of the port
/// again, so they are kept until the wait is over.
struct Deferred {
    kept: [(u64, u32); MAX_DEFERRED],
    count: usize,
}

impl Deferred {
    const fn new() -> Self {
        Self {
            kept: [(0, 0); MAX_DEFERRED],
            count: 0,
        }
    }

    /// Keep `packet`, unless its key is kept already: a second signal for
    /// one wait says no more than the first, and the room is small.
    fn keep(&mut self, packet: &PortPacket) {
        if self
            .kept
            .iter()
            .take(self.count)
            .any(|(key, _)| *key == packet.key)
        {
            return;
        }
        if let Some(slot) = self.kept.get_mut(self.count) {
            *slot = (packet.key, packet.signals);
            self.count += 1;
        }
    }

    /// Give them back, in the order they arrived.
    fn give_back(&self, port: &Port<Kernel>) {
        for (key, signals) in self.kept.iter().take(self.count) {
            let _ = port.queue(*key, [u64::from(*signals), 0]);
        }
    }
}

/// Each scanout's preferred mode, from `GET_DISPLAY_INFO`, and its refresh,
/// from `GET_EDID` where the device granted `VIRTIO_GPU_F_EDID`: what HELLO
/// and MODES carry.
///
/// The refresh is asked again with the size, since QEMU makes the EDID when
/// it is asked, at the size and refresh its window is at now. A refusal or
/// an EDID that does not say is a refresh of 0, which the kernel lists as
/// 60 Hz: the card is driven whatever it says.
fn display_modes(
    driver: &mut Gpu,
    port: &Port<Kernel>,
) -> Result<[ScanoutMode; MAX_SCANOUTS], Step> {
    let Ok(Response::DisplayInfo(scanouts)) = run_command(driver, port, &Command::GetDisplayInfo)?
    else {
        return Err(Step::Device);
    };
    let count = (driver.info().config.num_scanouts as usize).min(MAX_SCANOUTS);
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    for (mode, scanout) in modes.iter_mut().zip(scanouts.iter()).take(count) {
        let fits = scanout.rect.width <= MAX_DIMENSION && scanout.rect.height <= MAX_DIMENSION;
        if scanout.enabled && fits {
            *mode = ScanoutMode {
                width: scanout.rect.width,
                height: scanout.rect.height,
                enabled: true,
                refresh_mhz: 0,
            };
        }
    }
    if driver.info().features & gpu::FEATURE_EDID != 0 {
        for (index, mode) in modes.iter_mut().enumerate().take(count) {
            if !mode.enabled {
                continue;
            }
            let scanout = u32::try_from(index).map_err(|_| Step::Device)?;
            let answer = run_command(driver, port, &Command::GetEdid { scanout })?;
            mode.refresh_mhz = driver.edid_refresh_mhz(&answer);
        }
    }
    Ok(modes)
}

/// HELLO from what `GET_DISPLAY_INFO` said.
fn hello(driver: &mut Gpu, port: &Port<Kernel>, location: u32) -> Result<Hello, Step> {
    let modes = display_modes(driver, port)?;
    let count = (driver.info().config.num_scanouts as usize).min(MAX_SCANOUTS);
    // What the card can do in 3D, which is the device's answer and not the
    // driver's wish: `VIRTIO_GPU_F_VIRGL` is granted or it is not, and the
    // capability sets are only worth walking when it was. The first set's
    // id is what a renderer would go on -- `CAPSET_VIRGL2` on anything
    // recent -- and one is enough to say which renderer is behind the card.
    let virgl = driver.info().features & gpu::FEATURE_VIRGL != 0;
    let capsets = if virgl {
        u16::try_from(driver.info().config.num_capsets).unwrap_or(u16::MAX)
    } else {
        0
    };
    let first = match capsets {
        0 => None,
        _ => match run_command(driver, port, &Command::GetCapsetInfo { index: 0 })? {
            Ok(Response::CapsetInfo(info)) => Some(info),
            // A device that will not say what its first set is has one this
            // driver cannot use; the card is still a scanout.
            _ => None,
        },
    };
    // And the set itself, which is the blob a renderer reads to find out
    // what the host can do. Asked for at the size the device named, and
    // only when that fits the response buffer: a set larger than the buffer
    // is one this driver cannot fetch, and asking for it anyway would have
    // the device write past the end of it.
    let mut capset_bytes = 0;
    if let Some(info) = first
        && info.max_size <= CAPSET_ROOM
        && let Ok(Response::Capset { len }) = run_command(
            driver,
            port,
            &Command::GetCapset {
                capset_id: info.id,
                capset_version: info.max_version,
                max_size: info.max_size,
            },
        )?
    {
        capset_bytes = u32::try_from(len).unwrap_or(0);
    }
    let capset = first.map_or(0, |info| info.id);
    Ok(Hello {
        version: VERSION,
        scanouts: u16::try_from(count).map_err(|_| Step::Device)?,
        location,
        modes,
        virgl,
        capsets: if capset == 0 { 0 } else { capsets },
        capset,
        capset_bytes,
        // Every virtio-gpu has its cursor queue, and this driver runs it.
        cursor: true,
        // And shows any size it is handed a scanout of: no list.
        timings: Timings::NONE,
        // The device reads the pinned pages itself.
        copies: false,
    })
}

/// Wait for READY and take the card VMO from it.
fn ready(control: &Channel<Kernel>) -> Result<Vmo<Kernel>, Step> {
    let _ = control
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Hello)?;
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle::INVALID; 2];
    let received = control
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Hello)?;
    match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(Message::Ready(_)) if received.handles == 2 => {
            // The core's port is for later; it is closed here.
            drop(OwnedHandle::from_raw(Kernel, handles[1]));
            Ok(Vmo::from_owned(OwnedHandle::from_raw(Kernel, handles[0])))
        }
        _ => Err(Step::Control),
    }
}

/// Say HELLO, take READY's card VMO, and start listening to the core.
fn introduce(
    driver: &mut Gpu,
    port: &Port<Kernel>,
    control: &Channel<Kernel>,
    location: u32,
) -> Result<(Scratch, Vmo<Kernel>), Step> {
    let scratch = Scratch::new()?;
    let hello = hello(driver, port, location)?;
    let port_share = port
        .as_owned()
        .duplicate(Requested::Exactly(PORT_RIGHTS))
        .map_err(|_| Step::Hello)?;
    control
        .write_with(Message::Hello(hello).encode().as_bytes(), [port_share])
        .map_err(|_| Step::Hello)?;
    let card = ready(control)?;
    // The wait is armed by `run`, once every conversation's blocking setup
    // commands are done. A command waiting on the device takes packets off
    // the port and drops the ones that are not its interrupt, and an armed
    // wait is delivered once: arming before then can lose the core's first
    // message and leave nothing listening for another.
    Ok((scratch, card))
}

/// Arm the waits for both of the card's conversations.
///
/// Only once every conversation's blocking setup commands are done. A
/// command waiting on the device takes packets off the port, and an armed
/// wait is delivered once: arming earlier can spend a channel's wakeup on a
/// wait nobody is in a position to answer.
fn arm_waits(
    port: &Port<Kernel>,
    control: &Channel<Kernel>,
    render: Option<&RenderSide>,
) -> Result<(), Step> {
    control
        .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_CONTROL)
        .map_err(|_| Step::Events)?;
    if let Some(side) = render {
        side.control
            .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_RENDER)
            .map_err(|_| Step::Events)?;
    }
    Ok(())
}

fn run(boot: &Channel<Kernel>) -> Result<(), Step> {
    let Started {
        start,
        device,
        control,
    } = started(boot)?;
    if start.pci_device_id != VIRTIO_GPU_ID {
        return Err(Step::Identity);
    }

    let interrupt = device.interrupt(0).map_err(|_| Step::Registers)?;
    let registers = Registers {
        common: Block::map(&device, &start.common)?,
        notify: Block::map(&device, &start.notify)?,
        isr: Block::map(&device, &start.isr)?,
        device: Block::map(&device, &start.device)?,
        notify_off_multiplier: start.notify_off_multiplier,
        msix: start.msix_table_size > 0,
        interrupt,
    };
    let rings = Pinned::new(&device, QUEUE_PAGES)?;
    let area = Pinned::new(&device, AREA_PAGES)?;
    // The cursor queue's: a page of rings and a page of commands, which is
    // sixteen of each and more than a pointer ever has waiting.
    let cursor_rings = Pinned::new(&device, 1)?;
    let cursor_area = Pinned::new(&device, 1)?;
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    registers
        .interrupt
        .bind(&port, KEY_INTERRUPT)
        .map_err(|_| Step::Events)?;
    let mut driver = match Driver::init(
        Parts {
            transport: registers,
            rings,
            area,
            cursor_rings,
            cursor_area,
        },
        Options {
            // Ask the card whether there is a GPU behind it. A 2D card says
            // no and is driven as it always was; a `virtio-gpu-gl` says yes
            // and brings virglrenderer up on the host.
            want_3d: true,
            ..Options::default()
        },
    ) {
        Ok(driver) => driver,
        Err(failure) => {
            drop(failure);
            return Err(Step::Device);
        }
    };

    let introduced = introduce(&mut driver, &port, &control, start.location);
    let (scratch, card) = match introduced {
        Ok(introduced) => introduced,
        Err(step) => {
            // Nothing is pinned for a buffer yet; the device goes back to
            // reset, unless it will not, when its memory stays.
            return match driver.shutdown() {
                Teardown::Released(released) => {
                    drop(released);
                    Err(step)
                }
                Teardown::Wedged(kept) => {
                    let _kept_for_good = kept;
                    Err(Step::Wedged)
                }
            };
        }
    };

    // The card's other conversation, which is where the GPU is. A card with
    // no 3D behind it says nothing and serves the display as before.
    //
    // Its introduction runs device commands of its own, so it happens before
    // either wait is armed: an armed wait is delivered once, and a command
    // waiting on the device takes packets off the port.
    let render = render_introduce(&mut driver, &port, &device, start.location)?;

    arm_waits(&port, &control, render.as_ref())?;

    let mut serving = Serving {
        render_armed: render.is_some(),
        render,
        pipeline: Pipeline::for_device(TransferOffset::for_device(
            start.pci_device_id,
            start.pci_subsystem_vendor_id,
            start.pci_subsystem_id,
        )),
        ..Serving::new(driver, scratch, card, device, control, port)
    };
    let ended = serving.serve();
    let Serving {
        driver,
        pins,
        control,
        render,
        ..
    } = serving;
    match driver.shutdown() {
        Teardown::Released(released) => {
            drop(released);
            drop(pins);
            drop(render);
            if matches!(ended, Ok(true)) {
                let _ = control.write(Message::Stopped.encode().as_bytes());
            }
            ended.map(drop)
        }
        Teardown::Wedged(kept) => {
            // The device may still read the buffers' pages, the objects'
            // backings and the work VMO: the pins stay.
            let _kept_for_good = (kept, ManuallyDrop::new(pins), ManuallyDrop::new(render));
            Err(Step::Wedged)
        }
    }
}

/// The serve loop's state.
struct Serving {
    driver: Gpu,
    pipeline: Pipeline,
    pins: [Option<(u32, Pin<Kernel>)>; MAX_PINS],
    scratch: Scratch,
    card: Vmo<Kernel>,
    device: Device<Kernel>,
    control: Channel<Kernel>,
    port: Port<Kernel>,
    /// Whether the display core's channel may hold messages not read yet.
    /// Its wait is armed again only once a read finds it empty.
    display_readable: bool,
    /// A display request read and not given to the pipeline yet, because
    /// render requests read before it have still to go to the device.
    held: Option<Message>,
    /// The render conversation, while the core keeps it open. `None` for a
    /// card with no GPU behind it, and once the core has gone.
    render: Option<RenderSide>,
    /// Whether the render core's channel may hold messages not read yet.
    render_readable: bool,
    /// Whether a wait on the render channel is armed and not delivered: a
    /// read that finds the channel empty arms one only if not, since a
    /// display request has the channel read without being woken for it.
    render_armed: bool,
    /// Render requests read and not yet on the device, in order.
    backlog: Backlog,
    /// What the completion of each render command on the device is to be
    /// answered with, by its tag less [`TAG_RENDER`].
    owed: [Option<Owed>; CONTROL_SLOTS + 1],
    /// Whether the device said its displays changed and the core has not
    /// been sent MODES for it yet.
    modes_stale: bool,
}

/// The tag of a command a blocking helper waits for, which is
/// [`Driver::submit`]'s.
const TAG_BLOCKING: u64 = 0;
/// The tag of the display pipeline's commands.
const TAG_DISPLAY: u64 = 1;
/// The first tag of a render command's: the rest is its place in
/// [`Serving::owed`].
const TAG_RENDER: u64 = 2;

/// Render requests read ahead of the device.
const BACKLOG: usize = 16;

/// What a render command's completion is answered with.
#[derive(Clone, Copy, Debug)]
enum Owed {
    Submitted { fence: u64 },
    Transferred { object: u32 },
}

impl Owed {
    const fn reply(self, status: ferrix_renderctl::message::Status) -> RenderMessage {
        match self {
            Self::Submitted { fence } => RenderMessage::Submitted { fence, status },
            Self::Transferred { object } => RenderMessage::Transferred { object, status },
        }
    }
}

/// A render request waiting for the device, or the end of the conversation,
/// which waits too: the device may still be reading the work VMO the
/// conversation's pin holds.
enum Pending {
    Request(RenderMessage, Option<Vmo<Kernel>>),
    End,
}

/// [`Pending`]s in the order they were read.
struct Backlog {
    items: [Option<Pending>; BACKLOG],
    head: usize,
    count: usize,
}

impl Backlog {
    const fn new() -> Self {
        Self {
            items: [const { None }; BACKLOG],
            head: 0,
            count: 0,
        }
    }

    const fn is_empty(&self) -> bool {
        self.count == 0
    }

    const fn is_full(&self) -> bool {
        self.count == BACKLOG
    }

    fn push(&mut self, pending: Pending) {
        if let Some(slot) = self.items.get_mut((self.head + self.count) % BACKLOG) {
            *slot = Some(pending);
            self.count += 1;
        }
    }

    fn front(&self) -> Option<&Pending> {
        if self.count == 0 {
            return None;
        }
        self.items.get(self.head).and_then(Option::as_ref)
    }

    fn pop(&mut self) -> Option<Pending> {
        if self.count == 0 {
            return None;
        }
        let pending = self.items.get_mut(self.head).and_then(Option::take);
        self.head = (self.head + 1) % BACKLOG;
        self.count -= 1;
        pending
    }
}

/// What became of posting a render command.
enum Posting {
    /// On the device; its completion is owed this.
    Posted(Owed),
    /// Answered already, without the device.
    Answered(RenderMessage),
    /// The device has no room for it yet.
    Wait,
}

impl Serving {
    /// A card's serving, with no render conversation yet: both waits armed
    /// and nothing read.
    fn new(
        driver: Gpu,
        scratch: Scratch,
        card: Vmo<Kernel>,
        device: Device<Kernel>,
        control: Channel<Kernel>,
        port: Port<Kernel>,
    ) -> Self {
        Self {
            driver,
            pipeline: Pipeline::new(),
            pins: [const { None }; MAX_PINS],
            scratch,
            card,
            device,
            control,
            port,
            display_readable: false,
            held: None,
            render: None,
            render_readable: false,
            render_armed: false,
            backlog: Backlog::new(),
            owed: [None; CONTROL_SLOTS + 1],
            modes_stale: false,
        }
    }

    /// Serve until STOP (`true`) or the core closing its end (`false`).
    fn serve(&mut self) -> Result<bool, Step> {
        loop {
            if let Some(stopped) = self.advance()? {
                return Ok(stopped);
            }
            let packet = match self.port.wait(watch(&self.driver)) {
                Ok(packet) => packet,
                Err(Error::TimedOut) => {
                    self.driver.check_needs_reset().map_err(|_| Step::Faulted)?;
                    continue;
                }
                Err(_) => return Err(Step::Events),
            };
            // A packet given back by a command's wait is a user packet
            // standing for the signal it was, so both forms are read alike.
            match (packet.kind, packet.key) {
                (PACKET_INTERRUPT, KEY_INTERRUPT) => self.completions()?,
                (PACKET_SIGNAL | PACKET_USER, KEY_CONTROL) => self.display_readable = true,
                (PACKET_SIGNAL | PACKET_USER, KEY_RENDER) => {
                    self.render_readable = true;
                    self.render_armed = false;
                }
                _ => {}
            }
        }
    }

    /// Move everything that can move: requests read from both cores, put
    /// on the device in the order they were asked for, until nothing more
    /// can go before a completion; then ring the doorbell once for all of
    /// it. `Some` when the run is over.
    fn advance(&mut self) -> Result<Option<bool>, Step> {
        self.send_modes()?;
        loop {
            let mut moved = self.read_render()?;
            moved |= self.start_render()?;
            if let Some(stopped) = self.read_display()? {
                self.driver.kick();
                return Ok(Some(stopped));
            }
            moved |= self.pump()?;
            if !moved {
                break;
            }
        }
        self.driver.kick();
        Ok(None)
    }

    /// Tell the core what the scanouts' modes are now, once the device has
    /// said they changed and has nothing in flight: `GET_DISPLAY_INFO` runs
    /// to completion here, as the rare render requests do.
    fn send_modes(&mut self) -> Result<(), Step> {
        if !self.modes_stale || self.driver.is_busy() {
            return Ok(());
        }
        self.modes_stale = false;
        let modes = display_modes(&mut self.driver, &self.port)?;
        self.control
            .write(Message::Modes { modes }.encode().as_bytes())
            .map_err(|_| Step::Control)
    }

    /// Take the device's completions and hand each to whoever posted it,
    /// and note a change of the device's displays.
    fn completions(&mut self) -> Result<(), Step> {
        let isr = self.driver.on_interrupt().map_err(|_| Step::Faulted)?;
        if isr & ISR_CONFIG != 0 && self.driver.display_changed() {
            self.modes_stale = true;
        }
        while let Some(done) = self.driver.take_done().map_err(|_| Step::Faulted)? {
            if done.tag == TAG_DISPLAY {
                self.pipeline.done(done.result).map_err(|_| Step::Faulted)?;
                continue;
            }
            // A blocking helper takes its own and nothing else is posted
            // while one runs, so any other tag is a render command's.
            let owed = done
                .tag
                .checked_sub(TAG_RENDER)
                .and_then(|at| self.owed.get_mut(usize::try_from(at).ok()?))
                .and_then(Option::take)
                .ok_or(Step::Faulted)?;
            self.answer_render(owed.reply(status_of(done.result)))?;
        }
        Ok(())
    }

    /// Read the render core's messages into the backlog, while there is
    /// room. Whether anything was read.
    fn read_render(&mut self) -> Result<bool, Step> {
        let mut moved = false;
        while self.render_readable && !self.backlog.is_full() {
            let Some(side) = self.render.as_ref() else {
                self.render_readable = false;
                break;
            };
            let mut bytes = [0_u8; RENDER_MAX_BYTES];
            // One handle at most: a mappable object's backing.
            let mut handles = [Handle::INVALID; 1];
            let pending = match side.control.read(&mut bytes, &mut handles) {
                Ok(received) => {
                    // Owned before anything else, so that a handle is closed
                    // whichever way this goes.
                    let handed = (received.handles == 1)
                        .then(|| Vmo::from_owned(OwnedHandle::from_raw(Kernel, handles[0])));
                    match RenderMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
                        Some(message) => Pending::Request(message, handed),
                        None => Pending::End,
                    }
                }
                Err(ReadError::Failed(Error::ShouldWait)) => {
                    // Nothing more until the core speaks again.
                    self.render_readable = false;
                    if !self.render_armed {
                        side.control
                            .wait_async(
                                &self.port,
                                Signals::READABLE | Signals::PEER_CLOSED,
                                KEY_RENDER,
                            )
                            .map_err(|_| Step::Events)?;
                        self.render_armed = true;
                    }
                    break;
                }
                Err(_) => Pending::End,
            };
            if matches!(pending, Pending::End) {
                // Nothing after the end is read, and nothing arms the wait.
                self.render_readable = false;
            }
            self.backlog.push(pending);
            moved = true;
        }
        Ok(moved)
    }

    /// Put the backlog on the device, in order, as far as it will go.
    ///
    /// An upload or a command stream is posted and answered when it
    /// completes, and the next goes straight after it; one slot is always
    /// left for the display, whose flush is what a person is waiting on.
    /// Anything else -- a context, an object, a capability set, the end --
    /// waits until nothing is in flight and then runs to completion here, as
    /// every request once did: they are rare, and several are more than one
    /// command whose later steps depend on the earlier.
    fn start_render(&mut self) -> Result<bool, Step> {
        let mut moved = false;
        while let Some(front) = self.backlog.front() {
            let hot = match front {
                Pending::Request(
                    message @ (RenderMessage::Submit(_) | RenderMessage::Transfer(_)),
                    _,
                ) => Some(*message),
                _ => None,
            };
            if let Some(message) = hot {
                if !self.start_hot(message)? {
                    break;
                }
                let _ = self.backlog.pop();
            } else {
                if self.driver.is_busy() {
                    break;
                }
                if let Some(pending) = self.backlog.pop() {
                    self.run_render(pending)?;
                }
            }
            moved = true;
        }
        Ok(moved)
    }

    /// Put one upload or command stream on the device, or answer it at
    /// once. `false` when it has to wait for a completion.
    fn start_hot(&mut self, message: RenderMessage) -> Result<bool, Step> {
        // One slot stays the display's.
        if self.driver.free_slots() < 2 {
            return Ok(false);
        }
        let Some(at) = self.owed.iter().position(Option::is_none) else {
            return Ok(false);
        };
        match self.post_render(message, at)? {
            Posting::Posted(owed) => {
                if let Some(slot) = self.owed.get_mut(at) {
                    *slot = Some(owed);
                }
            }
            Posting::Answered(reply) => self.answer_render(reply)?,
            Posting::Wait => return Ok(false),
        }
        Ok(true)
    }

    /// Post one upload or command stream, tagged with its place in `owed`.
    fn post_render(&mut self, message: RenderMessage, at: usize) -> Result<Posting, Step> {
        use ferrix_renderctl::message::Status;

        let Some(side) = self.render.as_mut() else {
            return Ok(Posting::Wait);
        };
        let tag = TAG_RENDER + at as u64;
        let (owed, posted) = match message {
            RenderMessage::Transfer(moving) => (
                Owed::Transferred {
                    object: moving.object,
                },
                self.driver.post(
                    tag,
                    in_context(moving.context),
                    &transfer_command(&moving),
                    &[],
                ),
            ),
            RenderMessage::Submit(submit) => {
                let owed = Owed::Submitted {
                    fence: submit.fence,
                };
                let at = submit.commands.at as usize;
                let len = submit.commands.len as usize;
                let mut runs = [(0_u64, 0_u32); MAX_STREAM_RUNS];
                // On a ring, the stream is fenced there, and the device
                // answers it only when the host has finished it: that answer
                // is what a program's fence descriptor waits for.
                let context = if submit.ring == ferrix_renderctl::message::NO_RING {
                    in_context(submit.context)
                } else {
                    gpu::Context {
                        id: submit.context,
                        fence: Some(submit.fence),
                        ring: u8::try_from(submit.ring).ok(),
                    }
                };
                // Where the core wrote it, if the work VMO is pinned; copied
                // through this process otherwise.
                let posted = if let Some(count) = side
                    .window
                    .as_ref()
                    .and_then(|window| window.runs(at, len, &mut runs))
                {
                    self.driver.post(
                        tag,
                        context,
                        &Command::Submit3dHeader { size: len as u32 },
                        runs.get(..count).unwrap_or_default(),
                    )
                } else if len <= STREAM_BYTES
                    && side
                        .work
                        .as_ref()
                        .is_some_and(|work| work.read(side.stream.bytes(len), at as u64).is_ok())
                {
                    self.driver.post(
                        tag,
                        context,
                        &Command::Submit3d {
                            commands: side.stream.bytes(len),
                        },
                        &[],
                    )
                } else {
                    return Ok(Posting::Answered(owed.reply(Status::Invalid)));
                };
                (owed, posted)
            }
            _ => return Ok(Posting::Wait),
        };
        match posted {
            Ok(()) => Ok(Posting::Posted(owed)),
            Err(SubmitError::Busy) => Ok(Posting::Wait),
            Err(SubmitError::Broken | SubmitError::Device(_)) => Err(Step::Faulted),
            // A command the device could never take is the core's mistake,
            // and answered as one.
            Err(_) => Ok(Posting::Answered(owed.reply(Status::Invalid))),
        }
    }

    /// Answer the render core, if it is still there to answer.
    fn answer_render(&self, reply: RenderMessage) -> Result<(), Step> {
        let Some(side) = self.render.as_ref() else {
            return Ok(());
        };
        side.control
            .write(reply.encode().as_bytes())
            .map_err(|_| Step::Control)
    }

    /// Run one render request that waits for an idle device, and answer it.
    fn run_render(&mut self, pending: Pending) -> Result<(), Step> {
        let Pending::Request(message, handed) = pending else {
            self.end_render();
            return Ok(());
        };
        let Some(side) = self.render.as_mut() else {
            return Ok(());
        };
        let answer = match message {
            RenderMessage::MakeContext { context, capset } => {
                make_context(&mut self.driver, &self.port, context, capset)?
            }
            RenderMessage::DropContext { context } => {
                drop_context(&mut self.driver, &self.port, context)?
            }
            RenderMessage::MakeObject(make) => make_object(
                &mut self.driver,
                &self.port,
                &self.device,
                &mut self.scratch,
                side,
                &make,
                handed,
            )?,
            RenderMessage::DropObject { object } => {
                drop_object(&mut self.driver, &self.port, side, object)?
            }
            RenderMessage::MakeBlob(make) => make_blob(&mut self.driver, &self.port, side, &make)?,
            RenderMessage::AttachObject { object, context } => {
                attach_object(&mut self.driver, &self.port, object, context)?
            }
            RenderMessage::GetCaps { capset, version } => {
                // Its reply carries a handle, so it is written there.
                let _ = get_caps(&mut self.driver, &self.port, side, capset, version)?;
                return Ok(());
            }
            RenderMessage::Stop => {
                self.answer_render(RenderMessage::Stopped)?;
                self.end_render();
                return Ok(());
            }
            _ => {
                // A core this driver cannot answer: the render node goes and
                // the display carries on without it.
                self.end_render();
                return Ok(());
            }
        };
        self.answer_render(answer)
    }

    /// Let the render conversation go: STOP, or a core that went or said
    /// something this driver cannot answer. Only with nothing in flight,
    /// because its pins are memory the device may be reading.
    fn end_render(&mut self) {
        self.render = None;
        self.render_readable = false;
        while self.backlog.pop().is_some() {}
    }

    /// Read what the display core has sent, until none is left, the
    /// pipeline has no room, or a request has to wait for render requests
    /// read before it. `Some` when the run is over.
    ///
    /// A render request is answered only when the device has run it, but
    /// its caller need not wait for that: a program may submit a frame's
    /// drawing and flush the screen straight after. So a flush has to reach
    /// the device after the drawing, and that is why the render channel is
    /// read again after each display request and before it goes further: a
    /// render request written before a display request is in its channel by
    /// the time the display request can be read.
    fn read_display(&mut self) -> Result<Option<bool>, Step> {
        loop {
            if let Some(message) = self.held {
                if !self.backlog.is_empty() {
                    return Ok(None);
                }
                self.held = None;
                self.take(&message)?;
                continue;
            }
            if !self.display_readable || self.pipeline.is_full() {
                // The rest wait in the channel, which the core bounds.
                return Ok(None);
            }
            let mut bytes = [0_u8; MAX_BYTES];
            let message = match self.control.read(&mut bytes, &mut []) {
                Ok(received) => {
                    match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
                        Ok(Message::Stop) => return Ok(Some(true)),
                        Ok(Message::Refused(_)) | Err(_) => return Err(Step::Control),
                        Ok(message) => message,
                    }
                }
                Err(ReadError::Failed(Error::PeerClosed)) => return Ok(Some(false)),
                Err(ReadError::Failed(Error::ShouldWait)) => {
                    self.display_readable = false;
                    self.control
                        .wait_async(
                            &self.port,
                            Signals::READABLE | Signals::PEER_CLOSED,
                            KEY_CONTROL,
                        )
                        .map_err(|_| Step::Events)?;
                    return Ok(None);
                }
                Err(_) => return Err(Step::Control),
            };
            // A pointer moving waits for nothing, least of all the GPU.
            if let Message::Move { scanout, x, y } = message {
                self.driver
                    .move_cursor(scanout, x, y)
                    .map_err(|_| Step::Faulted)?;
                continue;
            }
            self.render_readable |= self.render.is_some();
            let _ = self.read_render()?;
            let _ = self.start_render()?;
            self.held = Some(message);
        }
    }

    /// Act on one request from the core.
    ///
    /// Where a new cursor is goes to the driver as it is taken: an image
    /// shown later is shown where the pointer is then. Everything else goes
    /// down the pipeline in order.
    fn take(&mut self, message: &Message) -> Result<(), Step> {
        if let Message::Cursor { scanout, x, y, .. } = *message {
            self.driver
                .place_cursor(scanout, x, y)
                .map_err(|_| Step::Faulted)?;
        }
        let request = Request::from_message(message).ok_or(Step::Control)?;
        self.pipeline.push(request).map_err(|_| Step::Control)
    }

    /// Do what the pipeline says until it waits on the device or has
    /// nothing. Whether it did anything.
    fn pump(&mut self) -> Result<bool, Step> {
        let mut moved = false;
        loop {
            match self.pipeline.next(self.scratch.entries()) {
                Next::Pin {
                    buffer,
                    offset,
                    length,
                } => {
                    let result = self.pin(buffer, offset, length);
                    self.pipeline.pinned(result).map_err(|_| Step::Faulted)?;
                }
                Next::Submit(command) => {
                    match self
                        .driver
                        .post(TAG_DISPLAY, gpu::Context::NONE, &command, &[])
                    {
                        Ok(()) => return Ok(true),
                        Err(SubmitError::Busy) => {
                            self.pipeline.unsent().map_err(|_| Step::Faulted)?;
                            return Ok(moved);
                        }
                        Err(_) => return Err(Step::Faulted),
                    }
                }
                Next::Unpin { buffer } => self.unpin(buffer),
                Next::Cursor {
                    scanout,
                    resource,
                    hot_x,
                    hot_y,
                } => {
                    self.driver
                        .update_cursor(scanout, resource, hot_x, hot_y)
                        .map_err(|_| Step::Faulted)?;
                }
                Next::Reply(message) => {
                    self.control
                        .write(message.encode().as_bytes())
                        .map_err(|_| Step::Control)?;
                }
                Next::Wait | Next::Idle => return Ok(moved),
            }
            moved = true;
        }
    }

    /// Close `buffer`'s pin: the device no longer holds its pages.
    fn unpin(&mut self, buffer: u32) {
        if let Some(slot) = self
            .pins
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|(held, _)| *held == buffer))
        {
            *slot = None;
        }
    }

    /// Pin `length` bytes of the card from `offset` read-only and make the
    /// backing entries: how many, or `Err` when it cannot be done.
    fn pin(&mut self, buffer: u32, offset: u64, length: u64) -> Result<usize, ()> {
        let offset = usize::try_from(offset).map_err(drop)?;
        let length = usize::try_from(length).map_err(drop)?;
        // An id the device may still hold pages under is never pinned twice:
        // the core never reuses one, so this is a broken core.
        if self.pins.iter().flatten().any(|(held, _)| *held == buffer) {
            return Err(());
        }
        let slot = self.pins.iter().position(Option::is_none).ok_or(())?;
        let (pin, count) = pin_entries(
            &self.device,
            &mut self.scratch,
            &self.card,
            offset,
            length,
            PinAccess::ReadOnly,
        )?;
        if let Some(held) = self.pins.get_mut(slot) {
            *held = Some((buffer, pin));
        }
        Ok(count)
    }
}

/// Pin `length` bytes of `vmo` from `offset` for the device and make the
/// backing entries in `scratch`: the pin and how many entries, or `Err` when
/// it cannot be done.
fn pin_entries(
    device: &Device<Kernel>,
    scratch: &mut Scratch,
    vmo: &Vmo<Kernel>,
    offset: usize,
    length: usize,
    access: PinAccess,
) -> Result<(Pin<Kernel>, usize), ()> {
    let pages = length / PAGE;
    if pages == 0 || pages > MAX_BUFFER_PAGES {
        return Err(());
    }
    let pin = device.pin(vmo, offset, length, access).map_err(drop)?;
    let got = pin.addresses(scratch.raw(pages)).map_err(drop)?;
    if !got.is_complete() || got.pages != pages {
        return Err(());
    }
    // One entry per run of device-consecutive pages, as
    // `backing_entries` makes them, without a second array of addresses.
    let page = PAGE as u64;
    let mut count = 0usize;
    for index in 0..pages {
        let bytes = scratch.raw(pages).get(index).copied().ok_or(())?;
        let address = device_address(bytes);
        if !address.is_multiple_of(page) {
            return Err(());
        }
        let entries = scratch.entries_mut();
        let joined = count
            .checked_sub(1)
            .and_then(|last| entries.get_mut(last))
            .filter(|entry| {
                entry.addr.checked_add(u64::from(entry.length)) == Some(address)
                    && entry.length.checked_add(PAGE as u32).is_some()
            });
        if let Some(entry) = joined {
            entry.length += PAGE as u32;
        } else {
            let fresh = entries.get_mut(count).ok_or(())?;
            *fresh = MemEntry {
                addr: address,
                length: PAGE as u32,
            };
            count += 1;
        }
    }
    Ok((pin, count))
}

/// How long to wait for the device: for good with nothing in flight, and
/// otherwise [`WATCH_NANOS`], after which the driver looks at the device
/// status itself -- an interrupt that brought completions does not
/// (`Driver::on_interrupt`), so a reset announced in one could otherwise
/// leave a command waited for that never comes.
fn watch(driver: &Gpu) -> Deadline {
    if !driver.is_busy() {
        return Deadline::Never;
    }
    match ferrix_rt::linux::monotonic_nanos() {
        Ok(now) => Deadline::At(now.saturating_add(WATCH_NANOS)),
        Err(_) => Deadline::Never,
    }
}
