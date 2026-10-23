//! `intel`'s keeper (`docs/INTEL-GPU.md` §12.3): the process devmgr starts
//! for the Intel GPU when its initramfs carries the GT's worker,
//! `intel-gt`, and the only one the kernel sees as the driver. It holds the
//! device handle, the render node's control, the MSI and every pin, and
//! serves `ferrix-intel-xelp`'s front end -- every program's handles,
//! objects, VMs, contexts, syncobjs, sync files and dmabufs -- over a back
//! end that is a journal ([`ferrix_intel_xelp::remote::keeper`]). It
//! touches no GT register.
//!
//! The GT is driven by a worker it starts in a child of its own job, from
//! the image devmgr handed it at START, and starts again when that one
//! dies or stops answering: the programs on the render node see a stall,
//! never a lost device. Only the keeper's own death is §11: devmgr waits
//! out its job tree, worker included, and starts another keeper, and every
//! program's device is lost.
//!
//! What a run says, in order: the device, the first worker's facts, the
//! render node's publication; then, for each worker replaced, why, and
//! the new one up with what it was given again.

#![no_std]
#![no_main]

mod link;
/// The serving loop, shared with `intel` (`../serve.rs`).
#[path = "../serve.rs"]
mod serve;
/// The kernel command line's options, for the machine's memory size only.
#[path = "../steps.rs"]
#[allow(
    dead_code,
    reason = "the keeper reads only the machine's memory size from it"
)]
mod steps;

use ferrix_blkring::control::{Message as StartMessage, START_BYTES};
use ferrix_intel_xelp::device_info;
use ferrix_intel_xelp::frontend::{Backend, Frontend};
use ferrix_intel_xelp::remote::{Journal, RemoteBackend};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::job::Job;
use ferrix_rt::native::port;
use ferrix_rt::native::vmo::Vmo;
use ferrix_rt::{Bootstrap, Kernel};

pub(crate) use crate::serve::say;
// The display card is `intel`'s, not the keeper's, until card0 moves into
// the keeper (docs/INTEL-GPU.md §12.8, the second step).
use ferrix_display_modes as _;
use ferrix_displayctl as _;

ferrix_rt::entry!(main);

/// The keeper's journal, about a megabyte: in the program's data, since
/// the front end's tables already take most of its 8 MiB stack.
static mut JOURNAL: Journal<Handle> = Journal::new();

/// System memory to report when `/proc/meminfo` cannot be read: the box's.
const SYSTEM_MEMORY_FALLBACK: u64 = 16 << 30;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
pub(crate) enum Step {
    /// No bootstrap channel, or the first message was not START with the
    /// device, the keeper's job and the worker's image.
    Start = 1,
    /// The device is not one this driver drives.
    Identity = 2,
    /// The port could not be made.
    Port = 3,
    /// Serving stopped (said).
    Serve = 4,
    /// No worker came up, or the keeper gave up on its workers (said):
    /// §11 takes over.
    Worker = 15,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(never) => match never {},
        Err(step) => {
            say(format_args!("intel: the keeper stopped at {step:?}"));
            step as i32
        }
    }
}

/// What START hands the keeper.
type Given = (Device<Kernel>, Job<Kernel>, Vmo<Kernel>);

/// START with three handles: the device, the keeper's own job (devmgr's
/// job for this driver, in which the worker is started so devmgr's kill
/// and wait reach it), and the worker's image.
fn started(boot: &Channel<Kernel>) -> Result<Given, Step> {
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Start)?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 3];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Start)?;
    let [device, job, image] = handles.map(|handle| OwnedHandle::from_raw(Kernel, handle));
    if received.handles != 3 {
        say(format_args!(
            "intel: the keeper's START carried {} handles, not the device, a job and the worker's image",
            received.handles
        ));
        return Err(Step::Start);
    }
    match StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(StartMessage::Start(_)) => Ok((
            Device::from_owned(device),
            Job::from_owned(job),
            Vmo::from_owned(image),
        )),
        _ => Err(Step::Start),
    }
}

fn run(boot: &Channel<Kernel>) -> Result<core::convert::Infallible, Step> {
    let (device, job, image) = started(boot)?;
    let info = device.info().map_err(|_| Step::Identity)?;
    if device_info::lookup(info.vendor_id, info.device_id).is_none() {
        say(format_args!(
            "intel: {:04x}:{:04x} is not a device this driver drives",
            info.vendor_id, info.device_id
        ));
        return Err(Step::Identity);
    }
    let revision = device.config_read(0x08, 1).map_or(0, |value| value as u8);
    say(format_args!(
        "intel: {:#010x} {:04x}:{:04x} rev {revision}: the keeper; the GT is driven by worker processes it starts and replaces (docs/INTEL-GPU.md §12)",
        info.location, info.vendor_id, info.device_id
    ));
    let port = port::create(Kernel).map_err(|_| Step::Port)?;
    let link = link::Link::new(
        &device,
        job,
        image,
        &port,
        link::Identity {
            location: info.location,
            device_id: info.device_id,
            revision,
            system_memory_bytes: steps::system_memory_bytes().unwrap_or(SYSTEM_MEMORY_FALLBACK),
        },
    );
    let journal = &raw mut JOURNAL;
    // SAFETY: the only reference to `JOURNAL` ever made: `run` is called
    // once, from `main`, and the process has one thread.
    let journal = unsafe { &mut *journal };
    let backend = RemoteBackend::start(link, journal).map_err(|_| Step::Worker)?;
    let facts = *backend.facts();
    say(format_args!(
        "intel: the GT's worker 1 is up: {} engines, {} EUs, CS {} Hz",
        facts.engines.len(),
        facts.sseu.eu_total,
        facts.cs_timestamp_hz
    ));
    let mut frontend = Frontend::new(backend);
    let mut host = serve::publish(&device, info.location).map_err(|_| Step::Serve)?;
    match serve::serve(&mut host, &port, &mut frontend, &mut |_, _| {}) {
        Ok(never) => match never {},
        Err(stopped) => {
            say(format_args!("intel: serving stopped: {stopped:?}"));
            Err(Step::Serve)
        }
    }
}
