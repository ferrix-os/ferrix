//! The keeper's half of the link to its worker ([`KeeperLink`]): object
//! memory pinned into 00:02.0's domain, the worker's process in a child of
//! the keeper's job, the channel to it, batched, and the MSI forwarded.
//!
//! What the worker is given (the consultant's condition 4, ledger 3156):
//! its bootstrap channel, the HELLO record on it, and for each bind a
//! read-only duplicate of the bound memory's address VMO, which it closes
//! once bound. Never a client's memory VMO, the device, the interrupt or a
//! control channel.

use core::cell::OnceCell;
use core::fmt;

use ferrix_intel_xelp::remote::wire::{
    BATCH_HANDLES, BATCH_RECORDS, Batch, Call, Event, RECORD_BYTES, records,
};
use ferrix_intel_xelp::remote::{GiveUp, KeeperLink};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::signals::Signals;
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::{self, Channel, ReadError};
use ferrix_rt::native::device::{Device, Interrupt};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::job::Job;
use ferrix_rt::native::pending::{self, Process, Protection};
use ferrix_rt::native::pin::{DeviceAddress, Pin, PinAccess};
use ferrix_rt::native::port::Port;
use ferrix_rt::native::vmo::{self, Vmo};

use crate::serve::{AsVmo, KEY_BACKEND, KEY_INTERRUPT, say};

/// Bytes in a page.
const PAGE: usize = 4096;
/// How long a dead worker's job may take to empty after its kill before
/// the keeper refuses to start another beside it.
const EMPTY_NS: u64 = 5_000_000_000;
/// The worker's name, in the initramfs and in `/proc/<pid>/comm`.
const WORKER: &str = "intel-gt";

/// A read-only duplicate's rights: what a worker needs to map the
/// addresses and nothing more.
const ADDRESSES_OUT: Rights = Rights(Rights::READ.0 | Rights::MAP.0 | Rights::TRANSFER.0);

/// What the worker is told of the device in its HELLO.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Identity {
    /// The PCI location.
    pub(crate) location: u32,
    /// The PCI device id.
    pub(crate) device_id: u16,
    /// The PCI revision.
    pub(crate) revision: u8,
    /// The machine's memory.
    pub(crate) system_memory_bytes: u64,
}

/// An object's memory: a VMO this process owns, pinned into 00:02.0's
/// domain at its first bind, its pages' device addresses in a VMO of their
/// own that a worker gets read-only.
#[derive(Debug)]
pub(crate) struct Memory {
    vmo: Vmo<Kernel>,
    pages: usize,
    pinned: OnceCell<Pinned>,
}

#[derive(Debug)]
struct Pinned {
    _pin: Pin<Kernel>,
    addresses: Vmo<Kernel>,
}

impl AsVmo for Memory {
    fn vmo(&self) -> &Vmo<Kernel> {
        &self.vmo
    }
}

/// A pin's refusal toward the program: the GPU's pin budget is `EDQUOT`,
/// everything else no memory (as `platform.rs` answers).
fn pin_errno(error: Error) -> Errno {
    match error {
        Error::LimitReached | Error::AccessDenied => Errno::EDQUOT,
        _ => Errno::ENOMEM,
    }
}

/// Hand each event in `bytes` to `sink`; `false` at a record no worker
/// writes, which ends that worker.
fn deliver(bytes: &[u8], sink: &mut dyn FnMut(Event)) -> bool {
    for record in records(bytes) {
        match Event::decode(record) {
            Ok(event) => sink(event),
            Err(error) => {
                say(format_args!(
                    "intel: the GT's worker sent a record nobody wrote ({error:?})"
                ));
                return false;
            }
        }
    }
    true
}

/// A worker, running.
#[derive(Debug)]
struct Running {
    job: Job<Kernel>,
    _process: Process<Kernel>,
    channel: Channel<Kernel>,
    /// Whether a wait on its channel is armed on the port and has not
    /// fired: a read that took a message means it fired.
    armed: bool,
}

/// The keeper's link.
pub(crate) struct Link<'a> {
    device: &'a Device<Kernel>,
    job: Job<Kernel>,
    image: Vmo<Kernel>,
    port: &'a Port<Kernel>,
    identity: Identity,
    interrupt: Option<Interrupt<Kernel>>,
    worker: Option<Running>,
    batch: Batch,
    handles: [Option<OwnedHandle<Kernel>>; BATCH_HANDLES],
}

impl fmt::Debug for Link<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("identity", &self.identity)
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl<'a> Link<'a> {
    /// The link: workers made in a child of `job` from `image`, their
    /// channels and the MSI waited on through `port`.
    pub(crate) fn new(
        device: &'a Device<Kernel>,
        job: Job<Kernel>,
        image: Vmo<Kernel>,
        port: &'a Port<Kernel>,
        identity: Identity,
    ) -> Self {
        let interrupt = device
            .interrupt(0)
            .ok()
            .filter(|interrupt| interrupt.bind(port, KEY_INTERRUPT).is_ok());
        Link {
            device,
            job,
            image,
            port,
            identity,
            interrupt,
            worker: None,
            batch: Batch::default(),
            handles: [const { None }; BATCH_HANDLES],
        }
    }

    /// End the running worker and wait, up to [`EMPTY_NS`], until nothing
    /// of its job is left: no two workers ever drive the GT at once.
    fn end_worker(&mut self) -> bool {
        let Some(old) = self.worker.take() else {
            return true;
        };
        let _ = old.job.kill();
        let until = self.now_ns().saturating_add(EMPTY_NS);
        let emptied = old.job.wait_one(Signals::EMPTY, Deadline::At(until));
        if emptied.is_err() {
            say(format_args!(
                "intel: a dead worker's job did not empty in {} s; no worker is started beside it",
                EMPTY_NS / 1_000_000_000
            ));
            return false;
        }
        true
    }

    /// Pin `memory` and fill its address VMO, at its first bind.
    fn pin(&self, memory: &Memory) -> Result<(), Errno> {
        let bytes = memory.pages * PAGE;
        let pin = self
            .device
            .pin(&memory.vmo, 0, bytes, PinAccess::ReadWrite)
            .map_err(pin_errno)?;
        let len = (memory.pages * 8).next_multiple_of(PAGE);
        let addresses = vmo::create(Kernel, len).map_err(|_| Errno::ENOMEM)?;
        let map = addresses
            .map(None, len, Protection::ReadWrite, 0)
            .map_err(|_| Errno::ENOMEM)?;
        // SAFETY: `map` is this process's own read-write mapping of `len`
        // bytes, unmapped below before anything else can reach it; eight-
        // byte entries, `memory.pages` of them, fit in it.
        let out =
            unsafe { core::slice::from_raw_parts_mut(map as *mut DeviceAddress, memory.pages) };
        let got = pin.addresses(out);
        let _ = ferrix_rt::linux::munmap(map, len);
        if !got.map_err(|_| Errno::ENOMEM)?.is_complete() {
            return Err(Errno::ENOMEM);
        }
        let _ = memory.pinned.set(Pinned {
            _pin: pin,
            addresses,
        });
        Ok(())
    }

    /// Send the batch once, waiting up to the TDR for room in the worker's
    /// queue.
    fn write_batch(&mut self) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        for _ in 0..2 {
            match worker
                .channel
                .write_slots(self.batch.as_bytes(), &mut self.handles)
            {
                Ok(()) => {
                    self.batch.clear();
                    return true;
                }
                Err(Error::ShouldWait) => {
                    let until = ferrix_rt::linux::monotonic_nanos()
                        .unwrap_or(0)
                        .saturating_add(ferrix_intel_xelp::remote::keeper::TDR_NS);
                    let _ = worker.channel.wait_one(
                        Signals::WRITABLE | Signals::PEER_CLOSED,
                        Deadline::At(until),
                    );
                }
                Err(_) => return false,
            }
        }
        false
    }
}

impl KeeperLink for Link<'_> {
    type Memory = Memory;
    type Token = Handle;

    fn create_memory(&mut self, bytes: u64) -> Result<Memory, Errno> {
        let bytes = usize::try_from(bytes).map_err(|_| Errno::ENOMEM)?;
        if bytes == 0 || !bytes.is_multiple_of(PAGE) {
            return Err(Errno::EINVAL);
        }
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Errno::ENOMEM)?;
        Ok(Memory {
            vmo,
            pages: bytes / PAGE,
            pinned: OnceCell::new(),
        })
    }

    fn token(&mut self, memory: &Memory) -> Result<(Handle, u64), Errno> {
        if memory.pinned.get().is_none() {
            self.pin(memory)?;
        }
        let pinned = memory.pinned.get().ok_or(Errno::ENOMEM)?;
        Ok((pinned.addresses.handle(), memory.pages as u64))
    }

    fn send(&mut self, call: &Call, token: Option<Handle>) -> bool {
        let handle = match token {
            Some(raw) if call.carries_handle() => {
                // The memory owns its address VMO; a duplicate travels.
                let borrowed = OwnedHandle::from_raw(Kernel, raw);
                let duplicate = borrowed.duplicate(Requested::Exactly(ADDRESSES_OUT));
                let _ = borrowed.into_raw();
                match duplicate {
                    Ok(duplicate) => Some(duplicate),
                    Err(_) => return false,
                }
            }
            _ => None,
        };
        if !self.batch.fits(handle.is_some()) && !self.flush() {
            return false;
        }
        if let Some(handle) = handle
            && let Some(slot) = self.handles.iter_mut().find(|slot| slot.is_none())
        {
            *slot = Some(handle);
        }
        let _ = self.batch.push(&call.encode(), call.carries_handle());
        if *call == Call::Interrupt
            && let Some(interrupt) = &self.interrupt
        {
            let _ = interrupt.ack();
        }
        true
    }

    fn flush(&mut self) -> bool {
        self.batch.is_empty() || self.write_batch()
    }

    fn receive(&mut self, sink: &mut dyn FnMut(Event)) -> bool {
        let port = self.port;
        let Some(worker) = self.worker.as_mut() else {
            return false;
        };
        let mut bytes = [0_u8; RECORD_BYTES * BATCH_RECORDS];
        let mut handles = [Handle::INVALID; 4];
        loop {
            match worker.channel.read(&mut bytes, &mut handles) {
                Ok(received) => {
                    worker.armed = false;
                    for handle in handles.iter().take(received.handles) {
                        drop(OwnedHandle::from_raw(Kernel, *handle));
                    }
                    if !deliver(bytes.get(..received.bytes).unwrap_or_default(), sink) {
                        return false;
                    }
                }
                Err(ReadError::Failed(Error::ShouldWait)) => {
                    if !worker.armed {
                        worker.armed = worker
                            .channel
                            .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_BACKEND)
                            .is_ok();
                    }
                    return true;
                }
                Err(_) => return false,
            }
        }
    }

    fn start_worker(&mut self, generation: u32) -> bool {
        if !self.end_worker() {
            return false;
        }
        self.batch.clear();
        self.handles = [const { None }; BATCH_HANDLES];
        let Ok(job) = self.job.create_child() else {
            return false;
        };
        let Ok(process) = pending::create_process(&job, &self.image, WORKER) else {
            return false;
        };
        let Ok((near, far)) = channel::create(Kernel) else {
            return false;
        };
        let hello = Call::Hello {
            generation,
            location: self.identity.location,
            device_id: self.identity.device_id,
            revision: self.identity.revision,
            system_memory_bytes: self.identity.system_memory_bytes,
        };
        if near.write(&hello.encode()).is_err() || process.start(far.into_owned()).is_err() {
            let _ = job.kill();
            return false;
        }
        // Armed at once, so its READY wakes the serving loop.
        let armed = near
            .wait_async(
                self.port,
                Signals::READABLE | Signals::PEER_CLOSED,
                KEY_BACKEND,
            )
            .is_ok();
        self.worker = Some(Running {
            job,
            _process: process,
            channel: near,
            armed,
        });
        true
    }

    fn wait(&mut self, until_ns: u64) {
        if let Some(worker) = self.worker.as_ref() {
            let _ = worker.channel.wait_one(
                Signals::READABLE | Signals::PEER_CLOSED,
                Deadline::At(until_ns),
            );
        }
    }

    fn now_ns(&mut self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
    }

    fn say(&mut self, line: fmt::Arguments<'_>) {
        say(line);
    }

    fn gave_up(&mut self, why: GiveUp) {
        say(format_args!(
            "intel: the keeper gives up on its workers ({why:?}): it exits, and every program's device is lost as when the driver dies (docs/INTEL-GPU.md §11)"
        ));
        let _ = self.end_worker();
        ferrix_rt::exit(crate::Step::Worker as i32);
    }
}
