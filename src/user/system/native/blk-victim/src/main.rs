//! A block driver over no device, for `cargo xtask test-btrfsd-restart`
//! (`docs/microkernel/BTRFSD.md` §5): the ring's driver side, whose every sector
//! reads as its own number, and which misbehaves by the name it was given.
//!
//! * `vda` ends itself every [`AFTER`] requests, as a crashed driver does.
//! * `vdb` stops answering after [`AFTER`] requests, and stays alive: a hang.
//! * any other serves.
//!
//! It never touches the device it is given. Only that gate's image carries it,
//! under the name `blk`.

#![no_std]
#![no_main]

use ferrix_blkclient::Shared;
use ferrix_blkring::bell::BELL_SUBMIT;
use ferrix_blkring::control::{MAX_BYTES, Message, START_BYTES, VMO_RIGHTS};
use ferrix_blkring::driver::{Consumed, DriverSide};
use ferrix_blkring::geometry::{Device as Geometry, DeviceFlags};
use ferrix_blkring::identity::{DiskName, Identity, Location};
use ferrix_blkring::layout::{Op, RingLayout, Status};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::PACKET_USER;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object as _, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::PinAccess;
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// `ferrix-blkclient` links `alloc` for its other side; `Shared` takes nothing from it.
struct NoHeap;

// SAFETY: it never hands out a block, so no block's rules can be broken.
unsafe impl core::alloc::GlobalAlloc for NoHeap {
    unsafe fn alloc(&self, _: core::alloc::Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _: *mut u8, _: core::alloc::Layout) {}
}

#[global_allocator]
static HEAP: NoHeap = NoHeap;

/// Requests before it misbehaves.
const AFTER: u32 = 300;
const SECTOR: u32 = 512;
const SECTORS: u64 = 65_536;
const MOST_SECTORS: u32 = 256;
const DATA_BYTES: usize = 1024 * 1024;
const RING_BYTES: usize = 4096;
const ENTRIES: u32 = 16;

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return 1;
    };
    serve(&boot).unwrap_or(2)
}

/// The sector's bytes: its number, little-endian, over and over.
fn fill(data: &Shared, offset: usize, sector: u64) {
    let word = sector.to_le_bytes();
    for at in (0..SECTOR as usize).step_by(8) {
        let _ = data.copy_in(offset + at, &word);
    }
}

/// `vmo_hold`'s refusals, by the real call (L.device.82). A driver has no line to
/// say it on under this shape, so the answer is whether it goes on: the first
/// refusal not as specified is this driver's exit status, devmgr's line says it,
/// and the disk the row reads never comes up.
fn hold_kinds() -> Option<i32> {
    let plain: Vmo<Kernel> = vmo::create(Kernel, 4096).ok()?;
    let bare = plain
        .as_owned()
        .duplicate(Requested::Exactly(Rights::READ))
        .ok()?;
    let bare: Vmo<Kernel> = Vmo::from_owned(bare);
    if bare.hold_whole().err() != Some(Error::AccessDenied) {
        return Some(70);
    }
    let huge: Vmo<Kernel> = vmo::create(Kernel, 64 * 1024 * 1024 + 4096).ok()?;
    if huge.hold_whole().err() != Some(Error::InvalidArgs) {
        return Some(71);
    }
    let sealed = plain.sealed_executable().ok()?;
    if sealed.hold_whole().err() != Some(Error::BadState) {
        return Some(72);
    }
    // A driver's job is measured (BA.4): a plain VMO becomes no image here.
    let image: Vmo<Kernel> = vmo::create(Kernel, 4096).ok()?;
    if image.executable().is_ok() {
        return Some(73);
    }
    None
}

#[expect(
    clippy::too_many_lines,
    reason = "a test driver's whole life, in order, read in one place"
)]
fn serve(boot: &Channel<Kernel>) -> Option<i32> {
    let _ = boot.wait_one(Signals::READABLE, Deadline::Never).ok()?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 2];
    let got = boot.read(&mut bytes, &mut handles).ok()?;
    let Ok(Message::Start(start)) = Message::decode(bytes.get(..got.bytes)?) else {
        return None;
    };
    // Nothing here drives the device, but its data region is pinned to it as a real
    // driver's is: what its death leaves in the quarantine is the row's to read.
    let device: Device<Kernel> = Device::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));
    let control: Channel<Kernel> = Channel::from_owned(OwnedHandle::from_raw(Kernel, handles[1]));
    let name = DiskName::new(start.name)?;
    let shown = name.as_str().as_bytes();
    let (dies, hangs) = (shown == b"vda", shown == b"vdb");

    let ring_vmo: Vmo<Kernel> = vmo::create(Kernel, RING_BYTES).ok()?;
    let data_vmo: Vmo<Kernel> = vmo::create(Kernel, DATA_BYTES).ok()?;
    let ring_at = ring_vmo
        .map(None, RING_BYTES, Protection::ReadWrite, 0)
        .ok()?;
    let data_at = data_vmo
        .map(None, DATA_BYTES, Protection::ReadWrite, 0)
        .ok()?;
    // Only the dying disk's: the others leave their data region sparse, so that
    // what commits it when the server holds it shows who paid (H.1).
    if dies {
        // Held for this process's life: its death is what puts it in the quarantine.
        let pin = device
            .pin(&data_vmo, 0, DATA_BYTES, PinAccess::ReadWrite)
            .ok()?;
        let _ = pin.into_owned().into_raw();
    } else {
        let _ = device.into_owned().into_raw();
    }
    if shown == b"vdc"
        && let Some(status) = hold_kinds()
    {
        return Some(status);
    }
    // SAFETY: mapped above for the process's life, and this the only name here.
    let ring = unsafe { Shared::from_raw(ring_at, RING_BYTES) };
    // SAFETY: as `ring`.
    let data = unsafe { Shared::from_raw(data_at, DATA_BYTES) };
    let geometry = Geometry::new(
        SECTOR,
        SECTORS,
        MOST_SECTORS,
        DeviceFlags::FLUSH,
        DATA_BYTES as u64,
    )
    .ok()?;
    let layout = RingLayout::standard(ENTRIES).ok()?;
    let mut side = DriverSide::new(ring, RING_BYTES as u64, layout, geometry).ok()?;
    let port_here: Port<Kernel> = port::create(Kernel).ok()?;
    let identity = Identity {
        location: Location(start.location),
        serial: [0; 20],
        name,
    };
    let hello = Message::Hello(side.hello(&identity)).encode();
    let share = |vmo: &Vmo<Kernel>| {
        vmo.as_owned()
            .duplicate(Requested::Exactly(VMO_RIGHTS))
            .ok()
    };
    let port_share = port_here
        .as_owned()
        .duplicate(Requested::Exactly(ferrix_blkring::control::PORT_RIGHTS))
        .ok()?;
    control
        .write_with(
            hello.as_bytes(),
            [share(&ring_vmo)?, share(&data_vmo)?, port_share],
        )
        .ok()?;
    let _ = control.wait_one(Signals::READABLE, Deadline::Never).ok()?;
    let mut message = [0_u8; MAX_BYTES];
    let mut ready = [Handle::INVALID; 1];
    let got = control.read(&mut message, &mut ready).ok()?;
    if !matches!(
        Message::decode(message.get(..got.bytes)?),
        Ok(Message::Ready)
    ) {
        return None;
    }
    let their_port: Port<Kernel> = Port::from_owned(OwnedHandle::from_raw(Kernel, ready[0]));

    let mut served = 0_u32;
    loop {
        while let Some(consumed) = side.consume().ok()? {
            let Consumed::Request(request) = consumed else {
                continue;
            };
            served = served.saturating_add(1);
            if served > AFTER && dies {
                return Some(99);
            }
            if served > AFTER && hangs {
                // Alive, answering nothing, for good.
                loop {
                    let _ = port_here.wait(Deadline::Never);
                }
            }
            let mut status = Status::Ok;
            let mut bytes_done = 0;
            if request.op == Op::Read {
                for index in 0..u64::from(request.count) {
                    let offset = request.data_offset as usize + index as usize * SECTOR as usize;
                    fill(&data, offset, request.sector + index);
                }
                bytes_done = u64::from(request.count) * u64::from(SECTOR);
            } else if request.op == Op::Write {
                status = Status::ReadOnly;
            }
            side.complete(request.id, status, bytes_done).ok()?;
        }
        if let Some(bell) = side.publish() {
            let _ = their_port.queue(bell.key(), bell.packet().data);
        }
        match side.prepare_to_sleep().ok()? {
            ferrix_blkring::bell::Wait::Pending(_) => continue,
            ferrix_blkring::bell::Wait::Sleep => {}
        }
        let packet = port_here.wait(Deadline::Never).ok()?;
        side.woke();
        let _ = (packet.kind == PACKET_USER, packet.key == BELL_SUBMIT);
    }
}
