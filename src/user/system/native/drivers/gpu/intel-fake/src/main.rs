//! `intel-fake`: the Intel driver's render node over a fake back end, for
//! `cargo xtask test-intel-uapi` alone (`docs/INTEL-GPU.md` §6).
//!
//! The same front end and the same serving loop as `intel`, with the GT
//! replaced by a back end that runs nothing and completes every batch the
//! moment it is submitted. Nothing is bound into any page table and no
//! register is touched: the device is a QEMU stand-in that answers as
//! Alder Lake-N (8086:46d4, class 030000, at 00:02.0) and has no GPU behind
//! it. Mesa's anv and iris see a device that takes every object, context,
//! VM, syncobj, sync file and dmabuf they ask for and finishes every batch
//! at once, with garbage in the pixels.
//!
//! Only `test-intel-uapi`'s image carries this program, under the name devmgr
//! starts for ADL-N, `intel`; no product image does. It exercises the
//! chardev core's K3, K5 and K6 paths, never the GPU's DMA, so no K2 claim
//! may cite it (the consultant's C2, ledger 2352).
//!
//! What a run says, in order: the device, `intel-fake: facts …`, the
//! render node's publication or the kernel's refusal of it, and then one
//! line for each call the front end does not serve.

#![no_std]
#![no_main]

mod fake;
/// The serving loop, shared with `intel` (`../../intel/src/serve.rs`).
#[path = "../../intel/src/serve.rs"]
mod serve;

use ferrix_blkring::control::{Message as StartMessage, START_BYTES};
use ferrix_intel_xelp::device_info;
use ferrix_intel_xelp::frontend::Frontend;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::port;
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};

use crate::serve::AsVmo;

ferrix_rt::entry!(main);

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// The device is not ADL-N.
    Identity = 2,
    /// The port could not be made.
    Port = 3,
    /// Serving stopped (said).
    Serve = 4,
}

/// The system memory the fake reports: Mesa sizes its heaps from it. The
/// gate's guest has 4 GiB.
const SYSTEM_MEMORY: u64 = 4 << 30;

/// An object's memory: a plain anonymous VMO.
struct Memory {
    vmo: Vmo<Kernel>,
}

impl AsVmo for Memory {
    fn vmo(&self) -> &Vmo<Kernel> {
        &self.vmo
    }
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(never) => match never {},
        Err(step) => {
            ferrix_rt::err!("intel-fake: stopped at {step:?}");
            step as i32
        }
    }
}

fn started(boot: &Channel<Kernel>) -> Result<Device<Kernel>, Step> {
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
        Ok(StartMessage::Start(_)) => Ok(device),
        _ => Err(Step::Start),
    }
}

fn run(boot: &Channel<Kernel>) -> Result<core::convert::Infallible, Step> {
    let device = started(boot)?;
    let info = device.info().map_err(|_| Step::Identity)?;
    if device_info::lookup(info.vendor_id, info.device_id).is_none() {
        ferrix_rt::warn!(
            "intel-fake: {:04x}:{:04x} is not ADL-N",
            info.vendor_id,
            info.device_id
        );
        return Err(Step::Identity);
    }
    let revision = device.config_read(0x08, 1).map_or(0, |value| value as u8);
    let facts = fake::facts(info.device_id, revision, SYSTEM_MEMORY);
    ferrix_rt::info!(
        "intel-fake: {:#010x} {:04x}:{:04x} rev {revision}: facts {} EUs, DSS {:#x}, CS {} Hz; {}",
        info.location,
        info.vendor_id,
        info.device_id,
        facts.sseu.eu_total,
        facts.sseu.subslice_mask,
        facts.cs_timestamp_hz,
        fake::MARKER
    );
    let port = port::create(Kernel).map_err(|_| Step::Port)?;
    let mut frontend = Frontend::new(fake::Fake::new(facts, |bytes| {
        let bytes = usize::try_from(bytes).map_err(|_| Errno::ENOMEM)?;
        vmo::create(Kernel, bytes)
            .map(|vmo| Memory { vmo })
            .map_err(|_| Errno::ENOMEM)
    }));
    let mut host = serve::publish(&device, info.location).map_err(|_| Step::Serve)?;
    match serve::serve(&mut host, &port, &mut frontend, &mut |_, _| {}) {
        Ok(never) => match never {},
        Err(stopped) => {
            ferrix_rt::err!("intel-fake: serving stopped: {stopped:?}");
            Err(Step::Serve)
        }
    }
}
