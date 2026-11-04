//! The virtio-blk driver process: a ring-3 program that serves one disk to
//! the kernel over the block ring.
//!
//! Everything that knows anything is a library. `ferrix-virtio-blk` drives
//! the device, `ferrix-blkring` speaks the ring, and `ferrix-blkserve` joins
//! the two; all three are tested on the host. `ferrix-driver` is the rest:
//! START, the virtio transport, pinned memory, and the ring's conversation
//! with the kernel (`docs/BLOCK-RING.md`). What is here is the one thing only
//! this driver knows: how the logic crate comes up on a virtio-blk device,
//! the geometry that makes, and how it goes down.
//!
//! The exit status is a `ferrix_driver::Step`, 0 a clean STOP, and from 20 up
//! the fault that stopped the serve loop (`ferrix_driver::block::fault_status`).

#![no_std]
#![no_main]

use ferrix_blkring::control::VMO_RIGHTS;
use ferrix_blkring::geometry::{Device as Geometry, DeviceFlags};
use ferrix_blkserve::Disk;
use ferrix_driver::dma::{Dma, PAGE};
use ferrix_driver::{Probe, Step, Stopped, Stuck, block, virtio};
use ferrix_rt::native::handle::OwnedHandle;
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_virtio_blk::{
    Accepted, Completion, DeviceError, Drained, Options, Parts, Request, Rings, Slot, SubmitError,
    Teardown,
};

ferrix_rt::entry!(main);

/// Descriptors in the queue: two requests' worth per ring entry, which
/// QEMU's 256 and every device's power of two allow.
const QUEUE_SIZE: u16 = 128;

/// Pages of queue memory: a 128-entry split queue's rings with room for the
/// alignment the layout asks for.
const QUEUE_PAGES: usize = 2;

/// Pages of request headers and status bytes: one per queue entry fits.
const AREA_PAGES: usize = 1;

/// Pages of data region: what one ring's worth of requests reads and writes.
const DATA_PAGES: usize = 128;

/// The most sectors one request may carry, whatever the device allows.
const MAX_SECTORS_CAP: u32 = 256;

fn main(bootstrap: Bootstrap) -> i32 {
    block::run::<VirtioBlk>(bootstrap)
}

type Device = virtio::Device<virtio::Block>;

/// The device as the loop drives it.
type Logic = ferrix_virtio_blk::Driver<Device, Dma, Dma, Dma, [Slot; QUEUE_SIZE as usize]>;

/// The device logic on the virtio transport and pinned memory, and the
/// geometry the ring announces.
struct VirtioBlk {
    logic: Logic,
    geometry: Geometry,
    /// The data region's handle for HELLO, until HELLO takes it.
    data_share: Option<OwnedHandle<Kernel>>,
}

/// The most sectors a request may carry on this device with this data
/// region: what fits the device's segment limit, page by page, with one
/// page of slack for a region that does not start on a page.
fn max_sectors(max_segments: u32, block_size: u32) -> u32 {
    let pages = max_segments.saturating_sub(1).clamp(1, DATA_PAGES as u32);
    let bytes = pages.saturating_mul(PAGE as u32);
    (bytes / block_size.max(1)).clamp(1, MAX_SECTORS_CAP)
}

impl Probe for VirtioBlk {
    type Device = Device;

    fn probe(device: Device) -> Result<Self, Step> {
        let rings = device.dma(QUEUE_PAGES)?;
        let area = device.dma(AREA_PAGES)?;
        let data = device.dma(DATA_PAGES)?;
        // HELLO's handle to the data region, taken before the logic owns it.
        let data_share = data.share(VMO_RIGHTS)?;
        let options = Options {
            max_queue_size: QUEUE_SIZE,
            ..Options::default()
        };
        let parts = Parts {
            transport: device,
            rings,
            area,
            data,
            slots: [Slot::EMPTY; QUEUE_SIZE as usize],
        };
        let logic = match Logic::init(parts, options) {
            Ok(logic) => logic,
            Err(failure) => {
                // The parts come back as a stop would give them: freed only
                // after a reset.
                let _ = release(failure.teardown, &mut |_| {});
                return Err(Step::Device);
            }
        };
        let info = *logic.info();
        let block_size = info.limits.block_size;
        let sectors = max_sectors(info.limits.max_segments, block_size);
        let capacity = info.limits.capacity / u64::from(block_size / 512).max(1);
        let mut flags = DeviceFlags::default();
        if info.read_only {
            flags = flags.union(DeviceFlags::READ_ONLY);
        }
        if info.flush {
            flags = flags.union(DeviceFlags::FLUSH);
        }
        let Ok(geometry) = Geometry::new(
            block_size,
            capacity,
            sectors,
            flags,
            (DATA_PAGES * PAGE) as u64,
        ) else {
            let _ = release(logic.shutdown(), &mut |_| {});
            return Err(Step::Hello);
        };
        Ok(VirtioBlk {
            logic,
            geometry,
            data_share: Some(data_share),
        })
    }
}

impl Disk for VirtioBlk {
    fn submit(&mut self, request: &Request) -> Result<Accepted, SubmitError> {
        self.logic.submit(request)
    }

    fn drain(&mut self, out: &mut [Completion]) -> Result<Drained, DeviceError> {
        self.logic.on_interrupt(out)
    }

    fn busy(&self) -> bool {
        self.logic.requests_in_flight() > 0
    }

    fn check(&mut self) -> Result<(), DeviceError> {
        self.logic.check_needs_reset()
    }
}

impl block::Device for VirtioBlk {
    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn data(&mut self) -> Result<OwnedHandle<Kernel>, Step> {
        self.data_share.take().ok_or(Step::Memory)
    }

    fn stop(self, abandoned: &mut dyn FnMut(u64)) -> Result<Stopped, Stuck> {
        release(self.logic.shutdown(), abandoned)
    }
}

/// Free what a teardown gave back, if the transport saw the device reset,
/// handing `abandoned` every request the reset dropped.
///
/// A device that did not reset may still write into its memory, so it
/// stays pinned: the logic crate hands it back wedged, and a `Dma` is only
/// freed with the proof of a reset.
fn release(
    teardown: Teardown<Device, Dma, Dma, Dma, [Slot; QUEUE_SIZE as usize]>,
    abandoned: &mut dyn FnMut(u64),
) -> Result<Stopped, Stuck> {
    let Teardown::Released(released) = teardown else {
        return Err(Stuck);
    };
    let Some(stopped) = released.transport.stopped() else {
        return Err(Stuck);
    };
    for id in released.abandoned() {
        abandoned(id);
    }
    let rings = match released.rings {
        Rings::Unused(memory) => memory,
        Rings::Queue(queue) => queue.into_memory(),
    };
    rings.free(&stopped);
    released.area.free(&stopped);
    released.data.free(&stopped);
    Ok(stopped)
}
