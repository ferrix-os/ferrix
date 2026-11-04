//! The virtio-input driver process: a ring-3 program that serves one input
//! device to the kernel's input core.
//!
//! Everything that knows anything is a library. `ferrix-virtio-input` drives
//! the device and turns its events into the messages the core reads, and
//! `ferrix-driver` is the rest: START, the virtio transport, pinned memory,
//! and `inputctl` with the core (`docs/INPUT.md` §3.2). What is here is the
//! one thing only this driver knows: how the logic crate comes up on a
//! virtio-input device, and how its answers map to the input subsystem's.
//!
//! The exit status is the `ferrix_driver::Step` that failed, 0 a clean STOP.

#![no_std]
#![no_main]

use ferrix_driver::dma::{Dma, PAGE};
use ferrix_driver::input::{self, Answer, Fault};
use ferrix_driver::virtio;
use ferrix_driver::{Probe, Step, Stopped, Stuck};
use ferrix_inputctl::message::{Events, Hello, Message};
use ferrix_rt::Bootstrap;
use ferrix_virtio_input::{AREA_BYTES, Control, Options, Parts, Rings, Teardown};

ferrix_rt::entry!(main);

/// Pages of queue memory: one 64-entry split queue's rings fit in one page,
/// and two leaves room for the alignment the layout asks for.
const QUEUE_PAGES: usize = 2;

/// Pages of event buffers: 64 buffers of eight bytes.
const AREA_PAGES: usize = AREA_BYTES.div_ceil(PAGE);

fn main(bootstrap: Bootstrap) -> i32 {
    input::run::<VirtioInput>(bootstrap)
}

type Device = virtio::Device<virtio::Input>;

/// The device logic, on the virtio transport and pinned memory.
struct VirtioInput(ferrix_virtio_input::Driver<Device, Dma, Dma>);

impl Probe for VirtioInput {
    type Device = Device;

    fn probe(device: Device) -> Result<Self, Step> {
        let rings = device.dma(QUEUE_PAGES)?;
        let area = device.dma(AREA_PAGES)?;
        let parts = Parts {
            transport: device,
            rings,
            area,
        };
        match ferrix_virtio_input::Driver::init(parts, Options::default()) {
            Ok(driver) => Ok(VirtioInput(driver)),
            Err(failure) => {
                // The parts come back as a stop would give them: freed only
                // after a reset, and the reason is the bring-up that failed.
                let _ = release(failure.teardown);
                Err(Step::Device)
            }
        }
    }
}

impl input::Device for VirtioInput {
    fn hello(&mut self, location: u32) -> Hello {
        self.0.hello(location)
    }

    fn control(&mut self, message: &Message) -> Result<Answer, Fault> {
        // Answering READY is what sets DRIVER_OK and posts the buffers, so
        // the device delivers nothing until the core has judged it.
        match self.0.on_control(message) {
            Ok(Control::Started { node }) => Ok(Answer::Started { node }),
            Ok(Control::Stop) => Ok(Answer::Stop),
            Ok(Control::Refused(_)) => Ok(Answer::Refused),
            Ok(Control::Status) => Ok(Answer::Nothing),
            Err(_) => Err(Fault),
        }
    }

    fn interrupt(&mut self) -> Result<bool, Fault> {
        self.0
            .on_interrupt()
            .map(|drained| drained.more)
            .map_err(|_| Fault)
    }

    fn events(&mut self) -> Option<Events> {
        self.0.pop_events()
    }

    fn stop(self) -> Result<Stopped, Stuck> {
        release(self.0.shutdown())
    }
}

/// Free what a teardown gave back, if the transport saw the device reset.
///
/// A device that did not reset may still write into the event buffers, so
/// they stay pinned: the logic crate hands them back wedged, and a `Dma` is
/// only freed with the proof of a reset.
fn release(teardown: Teardown<Device, Dma, Dma>) -> Result<Stopped, Stuck> {
    let Teardown::Released(released) = teardown else {
        return Err(Stuck);
    };
    let Some(stopped) = released.transport.stopped() else {
        return Err(Stuck);
    };
    let rings = match released.rings {
        Rings::Unused(memory) => memory,
        Rings::Queue(queue) => queue.into_memory(),
    };
    rings.free(&stopped);
    released.area.free(&stopped);
    Ok(stopped)
}
