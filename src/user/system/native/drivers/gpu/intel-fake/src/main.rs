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

/// The serving loop, shared with `intel` (`../../intel/src/serve.rs`).
#[path = "../../intel/src/serve.rs"]
mod serve;

use ferrix_blkring::control::{Message as StartMessage, START_BYTES};
use ferrix_intel_xelp::device_info;
use ferrix_intel_xelp::frontend::{
    Backend, Completion, ContextParams, Engine, Facts, Frontend, HwContext, RequestId, SubmitError,
    VmId,
};
use ferrix_intel_xelp::sseu::{Fuses, gen12_sseu_info_init};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::port;
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};

use crate::serve::{AsVmo, say};

ferrix_rt::entry!(main);

/// The engines the fake answers for: the render and copy engines, those
/// the GT back end drives (M2).
static ENGINES: [Engine; 2] = [
    Engine {
        class: 0,
        instance: 0,
    },
    Engine {
        class: 1,
        instance: 0,
    },
];

/// The system memory the fake reports: Mesa sizes its heaps from it. The
/// gate's guest has 4 GiB.
const SYSTEM_MEMORY: u64 = 4 << 30;

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

/// An object's memory: a plain anonymous VMO.
struct Memory {
    vmo: Vmo<Kernel>,
}

impl AsVmo for Memory {
    fn vmo(&self) -> &Vmo<Kernel> {
        &self.vmo
    }
}

/// The fake GT: every submission done at once.
struct Fake {
    facts: Facts,
    next: u32,
    done: [RequestId; 64],
    pending: usize,
}

impl Backend for Fake {
    type Memory = Memory;

    fn facts(&self) -> &Facts {
        &self.facts
    }

    fn create_memory(&mut self, bytes: u64) -> Result<Memory, Errno> {
        let bytes = usize::try_from(bytes).map_err(|_| Errno::ENOMEM)?;
        vmo::create(Kernel, bytes)
            .map(|vmo| Memory { vmo })
            .map_err(|_| Errno::ENOMEM)
    }

    fn vm_create(&mut self) -> Result<VmId, Errno> {
        self.next += 1;
        Ok(VmId(self.next))
    }

    fn vm_destroy(&mut self, _vm: VmId) {}

    fn bind(
        &mut self,
        _vm: VmId,
        _address: u64,
        _memory: &Memory,
        _bytes: u64,
        pat_index: u8,
    ) -> Result<(), Errno> {
        if pat_index > self.facts.max_pat_index {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    fn unbind(&mut self, _vm: VmId, _address: u64, _bytes: u64) {}

    fn context_create(
        &mut self,
        _vm: VmId,
        _engine: Engine,
        params: ContextParams,
    ) -> Result<HwContext, Errno> {
        let ring = params.ring_bytes;
        if !ring.is_power_of_two() || !(4 << 10..=2 << 20).contains(&ring) {
            return Err(Errno::EINVAL);
        }
        self.next += 1;
        Ok(HwContext(self.next))
    }

    fn context_destroy(&mut self, _context: HwContext) {}

    fn submit(
        &mut self,
        _context: HwContext,
        _batch: u64,
        _length: u32,
        request: RequestId,
    ) -> Result<(), SubmitError> {
        let slot = self.done.get_mut(self.pending).ok_or(SubmitError::Full)?;
        *slot = request;
        self.pending += 1;
        Ok(())
    }

    fn service(&mut self, _now_ns: u64, _interrupt: bool) {}

    fn next_deadline(&self) -> Option<u64> {
        None
    }

    fn completed(&mut self, sink: &mut dyn FnMut(RequestId, Completion)) {
        for id in self.done.iter().take(self.pending) {
            sink(*id, Completion::Ok);
        }
        self.pending = 0;
    }

    fn timestamp(&mut self) -> u64 {
        // The command streamer's clock at 19.2 MHz, from the monotonic one.
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0) / 1000 * 192 / 10
    }
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(never) => match never {},
        Err(step) => {
            say(format_args!("intel-fake: stopped at {step:?}"));
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
        say(format_args!(
            "intel-fake: {:04x}:{:04x} is not ADL-N",
            info.vendor_id, info.device_id
        ));
        return Err(Step::Identity);
    }
    let revision = device.config_read(0x08, 1).map_or(0, |value| value as u8);
    // M0's ADL-N (`docs/INTEL-GPU.md` §10): one slice, two DSS of twelve
    // EUs (pairs 3 and 7 fused off), the CS timestamp at 19.2 MHz.
    let facts = Facts {
        device_id: info.device_id,
        revision,
        sseu: gen12_sseu_info_init(Fuses {
            slice_enable: 1,
            geometry_dss_enable: 0x3,
            eu_disable: 0x88,
        }),
        cs_timestamp_hz: 19_200_000,
        engines: &ENGINES,
        ggtt_bytes: 4 << 30,
        ppgtt_bits: 48,
        system_memory_bytes: SYSTEM_MEMORY,
        max_pat_index: 3,
    };
    say(format_args!(
        "intel-fake: {:#010x} {:04x}:{:04x} rev {revision}: facts {} EUs, DSS {:#x}, CS {} Hz; every batch completes at once, nothing runs",
        info.location,
        info.vendor_id,
        info.device_id,
        facts.sseu.eu_total,
        facts.sseu.subslice_mask,
        facts.cs_timestamp_hz
    ));
    let port = port::create(Kernel).map_err(|_| Step::Port)?;
    let mut frontend = Frontend::new(Fake {
        facts,
        next: 0,
        done: [0; 64],
        pending: 0,
    });
    let mut host = serve::publish(&device, info.location).map_err(|_| Step::Serve)?;
    match serve::serve(&mut host, &port, &mut frontend) {
        Ok(never) => match never {},
        Err(stopped) => {
            say(format_args!("intel-fake: serving stopped: {stopped:?}"));
            Err(Step::Serve)
        }
    }
}
