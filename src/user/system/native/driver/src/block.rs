//! The block subsystem: the block ring (`docs/BLOCK-RING.md`), served to the
//! kernel for any driver that implements [`Device`].
//!
//! The run, whatever the disk:
//!
//! 1. START on the bootstrap channel; the driver binds and probes.
//! 2. The ring is one more VMO, mapped here and sent to the kernel in HELLO
//!    with the driver's data region and a port the kernel rings. READY
//!    brings the kernel's completion port back.
//! 3. One port carries every event: the kernel's bell (`BELL_SUBMIT`), the
//!    device's interrupt, and the control channel becoming readable.
//!    `ferrix-blkserve` decides what each means.
//! 4. STOP ends it: the device is reset, whatever it abandoned is completed
//!    `IoError` on the ring, STOPPED goes back, and the process exits. A
//!    device that will not reset keeps its memory for good, and the process
//!    exits without STOPPED, which tells the kernel to reset through its own
//!    means.
//!
//! The exit status is a [`Step`], 0 a clean STOP; a serve loop stopped by a
//! fault says which, from 20 up ([`fault_status`]).

use core::ptr;
use core::sync::atomic::{AtomicI32, Ordering, fence};

use ferrix_blkring::RingMemory;
use ferrix_blkring::bell::{BELL_SUBMIT, Doorbell, Wait};
use ferrix_blkring::control::{MAX_BYTES, Message, PORT_RIGHTS, VMO_RIGHTS};
use ferrix_blkring::driver::DriverSide;
use ferrix_blkring::geometry::Device as Geometry;
use ferrix_blkring::identity::{DiskName, Identity, Location, SERIAL_BYTES};
use ferrix_blkring::layout::{RingLayout, Status as RingStatus};
use ferrix_blkserve::{Disk, Fault, Serve};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{PACKET_INTERRUPT, PACKET_SIGNAL, PACKET_USER};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_virtio::QueueError;
use ferrix_virtio_blk::{Completion, DeviceError, Status};

use crate::dma::PAGE;
use crate::start::{self, Bind, Started};
use crate::{Driver, Step, Stopped, Stuck};

/// Ring entries: 64 in a 4 KiB ring.
const ENTRIES: u32 = 64;

/// Bytes of the ring VMO.
const RING_BYTES: usize = PAGE;

/// How long the loop sleeps while the device holds requests before it asks
/// the device whether it needs a reset (`Serve::on_watch`): an interrupt
/// that brings completions does not ask, so a reset announced in one could
/// otherwise leave the ring's requests waiting for good.
const WATCH_NANOS: u64 = 100_000_000;

/// Port keys, beside the kernel's `BELL_SUBMIT`.
const KEY_INTERRUPT: u64 = 3;
const KEY_CONTROL: u64 = 4;

/// A disk, as the block ring needs it: the requests it serves are
/// `ferrix-blkserve`'s [`Disk`].
pub trait Device: Driver + Disk {
    /// Its geometry, as the ring announces it.
    fn geometry(&self) -> Geometry;

    /// The handle to its data region HELLO hands the kernel, with exactly
    /// `ferrix_blkring::control::VMO_RIGHTS`: made at probe, before the
    /// device logic took the memory, and given once.
    ///
    /// # Errors
    ///
    /// [`Step::Memory`] if it was given already.
    fn data(&mut self) -> Result<OwnedHandle<Kernel>, Step>;

    /// Reset the device and prove it, freeing its memory; hand each request
    /// the reset abandoned to `abandoned`. Or say it would not reset,
    /// keeping its memory pinned.
    ///
    /// # Errors
    ///
    /// [`Stuck`] when the device did not reset.
    fn stop(self, abandoned: &mut dyn FnMut(u64)) -> Result<Stopped, Stuck>;
}

/// The exit status of the fault that stopped the serve loop, or 0 while none
/// has: written once, just before the loop returns [`Step::Faulted`].
static FAULTED_BY: AtomicI32 = AtomicI32::new(0);

/// The exit status that names `fault`: 20 a corrupt ring, 21 the two sides
/// disagreeing, 22 to 27 what the device did, and 30 up which of the
/// virtqueue's checks it failed. Below 128, where a status would read as a
/// signal.
///
/// A device that broke the protocol is the one failure a boot cannot say
/// more about from here, and whether it was the driver's queue, the device
/// or the IOMMU between them is decided by exactly this: `NeedsReset` is the
/// device refusing something it was given, a queue error is the used ring
/// saying something impossible.
#[must_use]
pub fn fault_status(fault: &Fault) -> i32 {
    match fault {
        Fault::Ring(_) => 20,
        Fault::Protocol => 21,
        Fault::Device(DeviceError::Broken) => 22,
        Fault::Device(DeviceError::NeedsReset) => 23,
        Fault::Device(DeviceError::UnknownChain(_)) => 24,
        Fault::Device(DeviceError::ConfigUnstable) => 25,
        Fault::Device(DeviceError::Bookkeeping) => 26,
        Fault::Device(DeviceError::Protocol(_)) => 27,
        Fault::Device(DeviceError::Queue(error)) => {
            30 + match error {
                QueueError::EmptyChain => 1,
                QueueError::ChainTooLong => 2,
                QueueError::OutOfDescriptors => 3,
                QueueError::DescriptorOutOfRange => 4,
                QueueError::ChainCycle => 5,
                QueueError::NotAChainHead => 6,
                QueueError::UsedIndexJumped => 7,
                QueueError::AvailableIndexJumped => 8,
                _ => 9,
            }
        }
    }
}

/// Run block driver `D` on the device START names; the exit status.
pub fn run<D: Device>(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start.status();
    };
    match serve_disk::<D>(&boot) {
        Ok(()) => 0,
        Err(Step::Faulted) => match FAULTED_BY.load(Ordering::Relaxed) {
            0 => Step::Faulted.status(),
            status => status,
        },
        Err(step) => step.status(),
    }
}

/// The disk's identity as HELLO carries it: START's location and name, and
/// the serial the device reported, if it did.
fn identity(started: &Started, serial: [u8; SERIAL_BYTES]) -> Result<Identity, Step> {
    let name = DiskName::new(started.start.name).ok_or(Step::Identity)?;
    Ok(Identity {
        location: Location(started.start.location),
        serial,
        name,
    })
}

fn serve_disk<D: Device>(boot: &Channel<Kernel>) -> Result<(), Step> {
    let started = start::read(boot)?;
    let identity = identity(&started, [0; SERIAL_BYTES])?;
    let Started {
        start,
        device,
        control,
    } = started;
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    let bound = D::Device::bind(device, &start, &port, KEY_INTERRUPT)?;
    let mut disk = D::probe(bound)?;

    // The ring, and HELLO with the ring VMO, the data region and the port.
    let geometry = disk.geometry();
    let data_share = disk.data()?;
    let ring_vmo = vmo::create(Kernel, RING_BYTES).map_err(|_| Step::Hello)?;
    let ring_share = ring_vmo
        .as_owned()
        .duplicate(Requested::Exactly(VMO_RIGHTS))
        .map_err(|_| Step::Hello)?;
    let port_share = port
        .as_owned()
        .duplicate(Requested::Exactly(PORT_RIGHTS))
        .map_err(|_| Step::Hello)?;
    let base = ring_vmo
        .map(None, RING_BYTES, Protection::ReadWrite, 0)
        .map_err(|_| Step::Hello)?;
    let ring = Ring {
        _vmo: ring_vmo,
        base,
        len: RING_BYTES,
    };
    let layout = RingLayout::standard(ENTRIES).map_err(|_| Step::Hello)?;
    let side =
        DriverSide::new(ring, RING_BYTES as u64, layout, geometry).map_err(|_| Step::Hello)?;
    let hello = Message::Hello(side.hello(&identity)).encode();
    control
        .write_with(hello.as_bytes(), [ring_share, data_share, port_share])
        .map_err(|_| Step::Hello)?;
    let kernel_port = ready(&control)?;

    // The control channel's readiness is the last event source.
    control
        .wait_async(&port, Signals::READABLE, KEY_CONTROL)
        .map_err(|_| Step::Events)?;

    // Serve until STOP, then take the device down. Each request's stay in
    // the device is timed where the processor's counter can be read, for
    // the seam's measurement (docs/OPAQUE-KERNEL.md, S0).
    let mut serve: Serve<Ring, D, { ENTRIES as usize }> = Serve::new(side, disk);
    if ferrix_rt::counter().is_some() {
        serve = serve.timed(ticks);
    }
    let ended = serve_until(&mut serve, &port, &kernel_port, &control);
    let stopped = matches!(ended, Ok(Ended::Stop));
    teardown(serve, &control, &kernel_port, stopped)?;
    ended.map(|_| ())
}

/// The processor's counter, for [`Serve::timed`]: zero where it cannot be
/// read, which the loop is never given.
fn ticks() -> u64 {
    ferrix_rt::counter().unwrap_or(0)
}

/// Why the loop ended.
enum Ended {
    /// The kernel said STOP.
    Stop,
    /// The kernel's end of the control channel went away.
    ControlGone,
}

/// Wait for READY, and take the kernel's completion port from it.
fn ready(control: &Channel<Kernel>) -> Result<Port<Kernel>, Step> {
    let _ = control
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Hello)?;
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = control
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Hello)?;
    match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(Message::Ready) if received.handles == 1 => {
            Ok(Port::from_owned(OwnedHandle::from_raw(Kernel, handles[0])))
        }
        _ => Err(Step::Hello),
    }
}

/// Ring the kernel's completion port, if the loop says to. A port that is
/// full has a bell queued already, and a bell is only a hint, so a refusal
/// changes nothing.
fn ring_bell(kernel_port: &Port<Kernel>, bell: Option<Doorbell>) {
    if let Some(bell) = bell {
        let _ = kernel_port.queue(bell.key(), bell.packet().data);
    }
}

/// How long to sleep: for good with nothing in the device, and otherwise
/// [`WATCH_NANOS`].
fn watch(busy: bool) -> Deadline {
    if !busy {
        return Deadline::Never;
    }
    match ferrix_rt::linux::monotonic_nanos() {
        Ok(now) => Deadline::At(now.saturating_add(WATCH_NANOS)),
        Err(_) => Deadline::Never,
    }
}

/// The loop: consume, serve, sleep, until STOP or a fault.
fn serve_until<D: Device>(
    serve: &mut Serve<Ring, D, { ENTRIES as usize }>,
    port: &Port<Kernel>,
    kernel_port: &Port<Kernel>,
    control: &Channel<Kernel>,
) -> Result<Ended, Step> {
    let mut out = [Completion {
        id: 0,
        status: Status::Ok,
        bytes: 0,
    }; 32];
    let faulted = |fault: Fault| {
        FAULTED_BY.store(fault_status(&fault), Ordering::Relaxed);
        Step::Faulted
    };
    ring_bell(kernel_port, serve.on_bell().map_err(faulted)?);
    loop {
        match serve.before_sleep().map_err(faulted)? {
            Wait::Pending(_) => {
                ring_bell(kernel_port, serve.on_bell().map_err(faulted)?);
                continue;
            }
            Wait::Sleep => {}
        }
        let packet = match port.wait(watch(serve.device_busy())) {
            Ok(packet) => packet,
            Err(Error::TimedOut) => {
                serve.on_watch().map_err(faulted)?;
                continue;
            }
            Err(_) => return Err(Step::Events),
        };
        match (packet.kind, packet.key) {
            (PACKET_USER, BELL_SUBMIT) => {
                ring_bell(kernel_port, serve.on_bell().map_err(faulted)?);
            }
            (PACKET_INTERRUPT, KEY_INTERRUPT) => {
                ring_bell(kernel_port, serve.on_interrupt(&mut out).map_err(faulted)?);
            }
            (PACKET_SIGNAL, KEY_CONTROL) => {
                let mut bytes = [0_u8; MAX_BYTES];
                match control.read(&mut bytes, &mut []) {
                    Ok(received) => {
                        if let Ok(Message::Stop) =
                            Message::decode(bytes.get(..received.bytes).unwrap_or_default())
                        {
                            return Ok(Ended::Stop);
                        }
                        // Anything else on the control channel after READY
                        // is not the kernel's; the registration is one-shot,
                        // so arm it again.
                        control
                            .wait_async(port, Signals::READABLE, KEY_CONTROL)
                            .map_err(|_| Step::Events)?;
                    }
                    Err(ReadError::Failed(Error::PeerClosed)) => return Ok(Ended::ControlGone),
                    Err(_) => {
                        control
                            .wait_async(port, Signals::READABLE, KEY_CONTROL)
                            .map_err(|_| Step::Events)?;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Take the device down: reset it, complete on the ring whatever the reset
/// abandoned, and say STOPPED if STOP was asked. A device that will not
/// reset keeps every page pinned for good and the answer is the exit status.
fn teardown<D: Device>(
    serve: Serve<Ring, D, { ENTRIES as usize }>,
    control: &Channel<Kernel>,
    kernel_port: &Port<Kernel>,
    stopped: bool,
) -> Result<(), Step> {
    let (mut side, disk) = serve.into_parts();
    let reset = disk.stop(&mut |id| {
        let _ = side.complete(id, RingStatus::IoError, 0);
    });
    match reset {
        Ok(_) => {
            ring_bell(kernel_port, side.publish());
            if stopped {
                control
                    .write(Message::Stopped.encode().as_bytes())
                    .map_err(|_| Step::Control)?;
            }
            Ok(())
        }
        Err(Stuck) => Err(Step::Wedged),
    }
}

/// The ring VMO as the ring library reads it: mapped here, shared with the
/// kernel, never pinned.
struct Ring {
    _vmo: Vmo<Kernel>,
    base: usize,
    len: usize,
}

impl RingMemory for Ring {
    fn read_u8(&self, offset: usize) -> u8 {
        assert!(offset < self.len, "a read inside the ring");
        // SAFETY: a mapping the kernel made for this process that lives as
        // long as `_vmo`; the offset was checked; volatile, since the kernel
        // writes the other side.
        unsafe { ptr::read_volatile((self.base + offset) as *const u8) }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        assert!(offset < self.len, "a write inside the ring");
        // SAFETY: as for `read_u8`, and the mapping is writable.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u8, value) }
    }

    fn barrier(&self) {
        fence(Ordering::SeqCst);
    }

    // The indices the kernel moves while this side reads them are `u32`s, and
    // each is one access here: composed from bytes, a tail stepping from
    // 0x00ff to 0x0100 reads 0x01ff on the other side, which is "the peer's
    // tail ran more than a ring ahead" and ends the ring (`ferrix-blkring`'s
    // `RingMemory`). The ring VMO is mapped page-aligned and the layout puts
    // every index at an aligned offset; anything unaligned is composed.

    fn read_u16(&self, offset: usize) -> u16 {
        if !(self.base + offset).is_multiple_of(2) {
            return u16::from_le_bytes([self.read_u8(offset), self.read_u8(offset + 1)]);
        }
        assert!(offset + 2 <= self.len, "a read inside the ring");
        // SAFETY: as for `read_u8`, and aligned to two within the mapping.
        u16::from_le(unsafe { ptr::read_volatile((self.base + offset) as *const u16) })
    }

    fn read_u32(&self, offset: usize) -> u32 {
        if !(self.base + offset).is_multiple_of(4) {
            return u32::from(self.read_u16(offset)) | (u32::from(self.read_u16(offset + 2)) << 16);
        }
        assert!(offset + 4 <= self.len, "a read inside the ring");
        // SAFETY: as for `read_u8`, and aligned to four within the mapping.
        u32::from_le(unsafe { ptr::read_volatile((self.base + offset) as *const u32) })
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        if !(self.base + offset).is_multiple_of(2) {
            let [low, high] = value.to_le_bytes();
            self.write_u8(offset, low);
            self.write_u8(offset + 1, high);
            return;
        }
        assert!(offset + 2 <= self.len, "a write inside the ring");
        // SAFETY: as for `write_u8`, and aligned to two within the mapping.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u16, value.to_le()) }
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        if !(self.base + offset).is_multiple_of(4) {
            self.write_u16(offset, value as u16);
            self.write_u16(offset + 2, (value >> 16) as u16);
            return;
        }
        assert!(offset + 4 <= self.len, "a write inside the ring");
        // SAFETY: as for `write_u8`, and aligned to four within the mapping.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u32, value.to_le()) }
    }
}
