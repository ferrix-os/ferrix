//! Stage 10: the kernel's end of a block ring.
//!
//! `docs/BLOCK-RING.md` is the protocol, and `ferrix-blkring` checks every
//! word of it. This is the glue that crate leaves to the kernel (its §8). A
//! process holding a device with `MANAGE` asks for a ring with
//! `BLOCK_RING_CREATE` and hands the channel end it gets to a block driver.
//! One kernel task per ring then:
//!
//! 1. waits for the driver's HELLO and checks it, answering REFUSED with the
//!    first failure in §6.2's order;
//! 2. holds the ring and data VMOs, attaches [`KernelSide`] over the ring, and
//!    publishes the disk as a block device before answering READY with its
//!    completion port;
//! 3. serves the disk's requests: it moves them from `src/lib/fs/block`'s queue onto
//!    the ring, copying a write's bytes into the data VMO, rings the driver
//!    when the driver asked to be rung, takes completions off the ring and
//!    wakes each finished request's caller, who copies a read's bytes out of
//!    its data region into its own buffer and gives the region back. A
//!    caller nudges the task only when the task went to sleep with nothing
//!    to do (`TaskIs`);
//! 4. ends when the driver's control channel closes, the driver says STOPPED
//!    or the ring is corrupt.
//!
//! # A disk outlives a driver that dies
//!
//! STOPPED ends a disk: every outstanding request fails with EIO and the
//! node is unpublished. A driver that dies -- its channel closed without
//! STOPPED, or its ring corrupt -- does not. Its disk is *parked*: the
//! commands the driver held are put back on the queue, in their epochs so
//! barrier order holds, its node stays published, and readers and writers
//! keep waiting. The next ring made for the same PCI location whose HELLO
//! describes the same disk under the same name takes the parked disk up and
//! dispatches what waited to its own driver, so a filesystem mounted on the
//! disk sees a slow request rather than an error when `devmgr` starts a dead
//! driver again (`docs/DEVMGR.md` §4). A request is idempotent at the block
//! layer, so one the dead driver had already done is only done twice. A disk
//! parked longer than [`PARK_PATIENCE_NANOS`] answers new requests with EIO
//! at once, and each waiting request has its own patience as before; a HELLO
//! describing another disk on that location ends the parked one first.
//!
//! # The kernel never serves a disk
//!
//! It issues requests and copies payloads. Whatever answers a request is the
//! process at the other end of the ring.
//!
//! # Pinned frames on a driver's death
//!
//! §6.3's rule that pinned pages outlive the driver until its device is reset
//! is kept by the driver's own pin, not here. Closing a pin on a translated
//! domain unmaps its pages before they can go, and on an untranslated domain
//! the pin keeps them. The ring's holds on the VMOs are only what the kernel
//! reads and writes through.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ferrix_blkring::kernel::Ending;
use ferrix_blkring::{
    Block, Completed, Device, DeviceFlags, DiskName, KernelSide, Location, Message, Refusal,
    RingMemory, Slot, Start as StartMessage, Status, Submission, SubmitError, Wait,
};
use ferrix_block::{Config, Limits, Op, Queue, Request, Token};
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, PACKET_SIGNAL, PACKET_USER,
};
use ferrix_vfs::Errno;

use crate::device::{self, DeviceNode};
use crate::object::channel::{Endpoint, ReadError};
use crate::object::port::{Observer, Port};
use crate::object::{self, Object, Transfer};
use crate::sched::{Task, WaitQueue};
use crate::sync::SpinLock;
use crate::user::vmo::{Held, Vmo};
use crate::{mm, sched, timer};

use crate::claim::StillServed;
use crate::fs::block::BlockDevice;
use crate::fs::devfs::{BlockRefused, BlockRegistration, Origin, register_block_from};
use crate::hooks::Full;
use crate::object::process::Host;
use crate::syscall::native;

pub(crate) mod check;
pub(crate) mod driver_check;
pub(crate) mod hop_check;
pub(crate) mod reread_check;
pub(crate) mod trip_check;

/// The block major every ring's disk is published under. Linux allocates
/// virtio-blk's major dynamically, usually 253 or 254; nothing keys on it.
pub(crate) const VIRTIO_BLK_MAJOR: u32 = 254;

/// The rights the driver's end of the control channel carries: it sends and
/// receives on it, waits on it and is handed it, and nobody copies it. The
/// crate's definition, so START's check and this agree by construction.
pub(crate) const CONTROL_RIGHTS: Rights = ferrix_blkring::control::CONTROL_RIGHTS;

/// How long a ring waits for its driver's HELLO.
const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How long a read waits for its driver before answering EIO.
const READ_PATIENCE_NANOS: u64 = 30_000_000_000;

/// How long a parked disk waits for a driver to take it up again before a
/// new request is answered EIO at once rather than left to wait.
const PARK_PATIENCE_NANOS: u64 = READ_PATIENCE_NANOS;

/// How long the ring's task sleeps before looking again of its own accord.
const RECHECK_NANOS: u64 = 50_000_000;

/// The completion port's key for the control channel's signals. Keys 1 and 2
/// are the ring's own bells.
const CONTROL_KEY: u64 = 3;

/// The completion port's key for a reader's nudge that a read was queued.
const SUBMIT_KEY: u64 = 4;

/// The most reads `src/lib/fs/block` merges into one ring submission.
const MAX_PARTS: u32 = 16;

/// The most submissions a ring holds outstanding, whatever its data VMO.
const MAX_OUTSTANDING: u64 = 4096;

/// Why a ring could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CreateError {
    /// The device already has a ring, live or ended.
    InUse,
    /// The device is not a PCI function, which HELLO's location names.
    NotPci,
    /// No memory for the channel, or no stack for the ring's task.
    NoMemory,
}

/// A device with a ring.
#[derive(Debug)]
struct Claim {
    /// The ring's number, which its task is started with.
    id: usize,
    /// The device.
    device: Arc<DeviceNode>,
    /// The location its accepted driver serves and the kernel's end of the
    /// control channel it serves through, while one does. The end says
    /// whether the driver still holds its own: `PEER_CLOSED` once it is gone.
    served: Option<(Location, Arc<Endpoint>)>,
}

/// What a ring's task starts from.
#[derive(Debug)]
struct Start {
    /// The ring's number.
    id: usize,
    /// The kernel's end of the control channel.
    control: Arc<Endpoint>,
    /// The PCI location of the device the ring was made for.
    location: Location,
}

/// Every device with a ring. A device keeps its claim once a driver has been
/// accepted and has died, since nothing then says its device was reset; a
/// driver that answered STOPPED has reset it (BLOCK-RING.md section 6.3), and
/// the device is free for the next ring.
static CLAIMS: SpinLock<Vec<Claim>> = SpinLock::new(Vec::new());

/// Rings made and not yet taken up by their task.
static STARTING: SpinLock<Vec<Start>> = SpinLock::new(Vec::new());

/// Woken whenever a ring stops serving its device, for a quiesce waiting on
/// the ring's task to notice the driver is gone.
static SERVED: WaitQueue = WaitQueue::new();

/// How long a quiesce waits for a dead driver's ring to end. The ring's task
/// wakes for the closed channel at once, or at its 50 ms recheck.
const QUIESCE_PATIENCE_NANOS: u64 = 5_000_000_000;

/// Every ring's task, so that a check counting frames can wait for the ones
/// that have ended to have stopped; the dead are pruned as the living join.
static TASKS: SpinLock<Vec<Arc<Task>>> = SpinLock::new(Vec::new());

/// The next ring's number.
static NEXT_RING: AtomicUsize = AtomicUsize::new(1);

/// A disk whose driver died, waiting for the next one.
#[derive(Debug)]
struct Parked {
    /// The PCI location its driver served it from.
    location: Location,
    /// Its name, which the next driver's HELLO must give again.
    name: DiskName,
    /// The disk, its queue and its waiting requests.
    disk: Arc<RingDisk>,
    /// Its node, still published.
    registration: BlockRegistration,
}

/// Disks whose driver died (the module's comment says what follows).
static PARKED: SpinLock<Vec<Parked>> = SpinLock::new(Vec::new());

/// Park `disk`: its node stays published and its requests wait for the next
/// ring made for `location`. With no memory to record it, it is ended as a
/// stopped driver's disk is.
fn park(location: Location, name: DiskName, disk: Arc<RingDisk>, registration: BlockRegistration) {
    disk.state.lock().parked_at = Some(timer::now_nanos());
    let parked = Parked {
        location,
        name,
        disk,
        registration,
    };
    let mut list = PARKED.lock();
    if list.try_reserve(1).is_ok() {
        list.push(parked);
    } else {
        drop(list);
        parked.disk.end();
    }
}

/// The disk parked at `location`, if HELLO's `device` and `name` describe
/// it. One that does not match is ended and its node unpublished, and `None`
/// answered: the location serves another disk now.
fn take_parked(location: Location, device: &Device, name: &DiskName) -> Option<Parked> {
    let parked = {
        let mut list = PARKED.lock();
        let at = list.iter().position(|parked| parked.location == location)?;
        list.swap_remove(at)
    };
    if parked.disk.device == *device && parked.name == *name {
        return Some(parked);
    }
    parked.disk.end();
    None
}

/// Whether a disk is parked at `location`, waiting for its next driver.
pub(crate) fn is_parked(location: Location) -> bool {
    PARKED
        .lock()
        .iter()
        .any(|parked| parked.location == location)
}

/// End the disk parked at `location`, if there is one, and unpublish it: for
/// a check that made a ring on a device nobody will serve again.
pub(crate) fn forget_parked(location: Location) {
    let parked = {
        let mut list = PARKED.lock();
        let Some(at) = list.iter().position(|parked| parked.location == location) else {
            return;
        };
        list.swap_remove(at)
    };
    parked.disk.end();
}

/// Make a ring for `node` and start its task; answer the driver's end of its
/// control channel.
///
/// # Errors
///
/// [`CreateError`].
pub(crate) fn create(node: &Arc<DeviceNode>) -> Result<Arc<Endpoint>, CreateError> {
    let location = location_of(node).ok_or(CreateError::NotPci)?;
    let (kernel_end, driver_end) = Endpoint::pair().map_err(|_| CreateError::NoMemory)?;
    let id = NEXT_RING.fetch_add(1, Ordering::Relaxed);
    {
        let mut claims = CLAIMS.lock();
        if claims.iter().any(|claim| Arc::ptr_eq(&claim.device, node)) {
            return Err(CreateError::InUse);
        }
        claims.push(Claim {
            id,
            device: Arc::clone(node),
            served: None,
        });
    }
    STARTING.lock().push(Start {
        id,
        control: kernel_end,
        location,
    });
    match sched::spawn("block ring", run, id, ferrix_sched::NICE_0_WEIGHT) {
        Ok(task) => {
            let mut tasks = TASKS.lock();
            tasks.retain(|task| !task.is_dead());
            // Room for more rings than any check makes at once, taken on the
            // first ring so the list never grows inside a frame-count window.
            if tasks.capacity() == 0 {
                tasks.reserve(16);
            }
            tasks.push(task);
        }
        Err(_) => {
            let _ = take_start(id);
            unclaim(id);
            return Err(CreateError::NoMemory);
        }
    }
    Ok(driver_end)
}

/// Wait until every ring's task has stopped, or `deadline` passes: for a
/// check counting frames, whose window must not hold a stack the reaper is
/// about to free.
///
/// # Errors
///
/// A ring's task still running at the deadline.
pub(crate) fn wait_until_tasks_stopped(deadline: u64) -> Result<(), &'static str> {
    loop {
        // Sleep before the first look as well as between looks, so a caller
        // measuring frames pays for the timer's entry on every call alike,
        // rather than only on the call whose tasks were slow to stop.
        sched::sleep_for(1_000_000);
        let mut tasks = TASKS.lock();
        if tasks.iter().all(|task| task.is_dead()) {
            tasks.clear();
            return Ok(());
        }
        drop(tasks);
        if timer::now_nanos() >= deadline {
            return Err("a ring's task kept running after its ring ended");
        }
    }
}

/// START for `node`, the disk to be named `name`: what `devmgr` sends the
/// driver it starts on the device, or what the boot check sends in its
/// stead. `None` for a node that is not a virtio PCI function.
pub(crate) fn start_for(node: &DeviceNode, name: DiskName) -> Option<StartMessage> {
    let location = location_of(node)?;
    let function = node.pci_function()?;
    let virtio = function.virtio?;
    let block = |block: device::RegisterBlock| Block {
        phys: block.phys,
        offset: block.offset,
        length: block.length,
    };
    Some(StartMessage {
        common: block(virtio.common),
        notify: block(virtio.notify),
        isr: block(virtio.isr),
        device: virtio.device.map(block).unwrap_or_default(),
        notify_off_multiplier: virtio.notify_multiplier,
        msix_table_size: function.msix_table_size,
        pci_device_id: function.device,
        location: location.raw(),
        name: *name.as_bytes(),
        pci_subsystem_vendor_id: function.subsystem_vendor,
        pci_subsystem_id: function.subsystem,
    })
}

/// Wait until no driver serves `node`'s disk through a ring, for a quiesce.
///
/// A driver's death fires its watchers' `TERMINATED` when its handles close,
/// which queues `PEER_CLOSED` for the ring's task but does not wait for that
/// task to take it: `devmgr`'s quiesce can arrive first. So a device served
/// through a control channel whose driver's end has closed is waited for,
/// bounded, until the ring ends and lets the device go; only a driver that
/// still holds its end refuses the quiesce. `cancelled` ends the wait early
/// for a caller that is being terminated.
///
/// # Errors
///
/// [`StillServed`].
pub(crate) fn wait_until_unserved(
    node: &Arc<DeviceNode>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), StillServed> {
    let deadline = timer::now_nanos().saturating_add(QUIESCE_PATIENCE_NANOS);
    let served_by = |node: &Arc<DeviceNode>| {
        CLAIMS
            .lock()
            .iter()
            .find(|claim| Arc::ptr_eq(&claim.device, node))
            .and_then(|claim| {
                claim
                    .served
                    .as_ref()
                    .map(|(_, control)| Arc::clone(control))
            })
    };
    loop {
        let Some(control) = served_by(node) else {
            return Ok(());
        };
        if !control.signals().intersects(Signals::PEER_CLOSED) {
            return Err(StillServed::ByADriver);
        }
        let ended = SERVED.wait_until_deadline(
            || {
                served_by(node)
                    .is_none_or(|control| !control.signals().intersects(Signals::PEER_CLOSED))
                    || cancelled()
            },
            deadline,
        );
        if !ended || cancelled() {
            return Err(StillServed::Waiting);
        }
    }
}

/// Release `node`'s ring claim: `devmgr` has quiesced the device, so the next
/// driver may have it. A ring still waiting for its HELLO keeps running and
/// finds nothing to release when it ends.
pub(crate) fn release_claim(node: &Arc<DeviceNode>) {
    CLAIMS
        .lock()
        .retain(|claim| !Arc::ptr_eq(&claim.device, node));
}

/// What a quiesce waits out for the block ring, and lets go of after.
static SERVER: native::Server = native::Server {
    wait_until_unserved,
    release: Some(release_claim),
};

/// Answer `block_ring_create` with this ring, and have a quiesce wait it out.
///
/// Called once from `main.rs`'s `register_load`: the native ABI is the item's
/// and names no subsystem above it, so the ring registers into it.
///
/// # Errors
///
/// [`Full`] when the item has no room for either registration.
pub(crate) fn install() -> Result<(), Full> {
    native::serve(NativeCall::BlockRingCreate, control_create)?;
    native::register_server(&SERVER)
}

/// `block_ring_create`.
///
/// `ALREADY_BOUND` for a device that has a ring, live or ended: nothing yet
/// says its device was reset. `INVALID_ARGS` for a device that is not a PCI
/// function, since HELLO names the disk by its PCI location. The device handle
/// and its `MANAGE` right are the item's to check (`native::control_channel`).
fn control_create(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let device = registers.first().copied().unwrap_or(0);
    native::control_channel(caller, device, CONTROL_RIGHTS, |node| {
        create(node).map_err(|why| match why {
            CreateError::InUse => status::ALREADY_BOUND,
            CreateError::NotPci => status::INVALID_ARGS,
            CreateError::NoMemory => status::NO_MEMORY,
        })
    })
}

/// The PCI location HELLO must name for `node`, if it is a PCI function:
/// `devmgr`'s, since its messages name devices the same way.
pub(crate) use crate::discovery::devmgr::location_of;

/// Take ring `id`'s start off the list.
fn take_start(id: usize) -> Option<Start> {
    let mut starting = STARTING.lock();
    let at = starting.iter().position(|start| start.id == id)?;
    Some(starting.swap_remove(at))
}

/// Give ring `id`'s device back, for a ring whose driver was never accepted.
fn unclaim(id: usize) {
    CLAIMS.lock().retain(|claim| claim.id != id);
}

/// Note that ring `id`'s driver serves `location` through `control`, or no
/// longer serves anything; a quiesce waiting for the latter is woken.
fn set_served(id: usize, served: Option<(Location, Arc<Endpoint>)>) {
    if let Some(claim) = CLAIMS.lock().iter_mut().find(|claim| claim.id == id) {
        claim.served = served;
    }
    SERVED.wake_all();
}

/// Whether a ring other than `id` has an accepted driver serving `location`.
fn served_elsewhere(id: usize, location: Location) -> bool {
    CLAIMS.lock().iter().any(|claim| {
        claim.id != id
            && claim
                .served
                .as_ref()
                .is_some_and(|(served, _)| *served == location)
    })
}

/// A ring's task.
fn run(id: usize) {
    let Some(start) = take_start(id) else {
        return;
    };
    let Some(hello) = receive_hello(&start.control) else {
        unclaim(id);
        return;
    };
    let accepted = match check_hello(&start, hello) {
        Ok(accepted) => accepted,
        Err(refusal) => {
            refuse(&start.control, refusal);
            unclaim(id);
            return;
        }
    };
    let mut storage = vec![Slot::EMPTY; accepted.regions];
    let Some(mut ring) = attach(&start, accepted, &mut storage) else {
        unclaim(id);
        return;
    };
    let ending = ring.serve();
    // Parked (or ended) before a quiesce can see the ring unserved: devmgr
    // starts the next driver the moment the quiesce answers, and its HELLO
    // must find the disk parked, not still registered by this ring.
    ring.finish(ending);
    set_served(id, None);
    if ending == Ending::Stopped {
        unclaim(id);
    }
}

/// The first message on the control channel, or `None` if the driver closed
/// it or said nothing in time.
///
/// Runs in the ring's own kernel thread, never in a process, so the wait
/// below need not watch for a terminated process.
fn receive_hello(control: &Endpoint) -> Option<object::channel::ChannelMessage> {
    let deadline = timer::now_nanos().saturating_add(HELLO_PATIENCE_NANOS);
    loop {
        match control.read(CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => return Some(message),
            Err(ReadError::Empty) => {}
            // A HELLO carries no channel, and nothing but a HELLO is
            // expected: the ring is over before it began.
            Err(ReadError::PeerClosed | ReadError::NeedsTopology | ReadError::TooSmall { .. }) => {
                return None;
            }
        }
        let ready = control.waiters().wait_until_deadline(
            || {
                control
                    .signals()
                    .intersects(Signals::READABLE | Signals::PEER_CLOSED)
            },
            deadline,
        );
        if !ready {
            return None;
        }
    }
}

/// An accepted HELLO, with the objects it carried.
#[derive(Debug)]
struct Accepted {
    /// The device, as HELLO described it.
    device: Device,
    /// Its node name.
    name: DiskName,
    /// The serial number the driver read off the device, for sysfs.
    serial: [u8; 20],
    /// The ring VMO.
    ring: Arc<Vmo>,
    /// The data VMO.
    data: Arc<Vmo>,
    /// The driver's port.
    driver_port: Arc<Port>,
    /// How many data regions of `max_sectors` the data VMO holds, capped.
    regions: usize,
}

/// Every check on HELLO that needs no registry, in §6.2's order.
fn check_hello(
    start: &Start,
    message: object::channel::ChannelMessage,
) -> Result<Accepted, Refusal> {
    let accepted = decode_hello(start, &message);
    object::dispose(message.handles.into_iter().map(|(object, _)| object));
    accepted
}

/// [`check_hello`], before the message's handles are let go.
fn decode_hello(
    start: &Start,
    message: &object::channel::ChannelMessage,
) -> Result<Accepted, Refusal> {
    let hello = match Message::decode(&message.bytes) {
        Ok(Message::Hello(hello)) => hello,
        Ok(_) => return Err(Refusal::Malformed),
        Err(error) => return Err(error.refusal()),
    };
    let rights: Vec<Rights> = message.handles.iter().map(|(_, rights)| *rights).collect();
    let accepted = hello.validate(&rights)?;
    let (
        Some((Object::Vmo(ring), _)),
        Some((Object::Vmo(data), _)),
        Some((Object::Port(driver_port), _)),
    ) = (
        message.handles.first(),
        message.handles.get(1),
        message.handles.get(2),
    )
    else {
        return Err(Refusal::Rights);
    };
    if accepted.identity.location != start.location {
        return Err(Refusal::WrongLocation);
    }
    let device = accepted.device;
    if data.len_bytes() < device.data_vmo_size() {
        return Err(Refusal::Device);
    }
    let region_bytes = u64::from(device.max_sectors()) * u64::from(device.block_size());
    let regions = (device.data_vmo_size() / region_bytes).min(MAX_OUTSTANDING);
    Ok(Accepted {
        device,
        name: accepted.identity.name,
        serial: accepted.identity.serial,
        ring: Arc::clone(ring),
        data: Arc::clone(data),
        driver_port: Arc::clone(driver_port),
        regions: usize::try_from(regions).unwrap_or(1),
    })
}

/// Send REFUSED with `refusal`. A driver that has gone hears nothing.
fn refuse(control: &Endpoint, refusal: Refusal) {
    let bytes = Message::Refused(refusal.raw()).encode().as_bytes().to_vec();
    let _ = control.write(bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
}

/// Take up the ring an accepted HELLO describes, publish its disk and answer
/// READY; or refuse, and answer `None`.
fn attach<'s>(start: &Start, accepted: Accepted, storage: &'s mut [Slot]) -> Option<Serving<'s>> {
    match take_up(start, accepted, storage) {
        Ok(ring) => Some(ring),
        Err(refusal) => {
            refuse(&start.control, refusal);
            None
        }
    }
}

/// [`attach`]'s body: every step that can refuse, then READY.
fn take_up<'s>(
    start: &Start,
    accepted: Accepted,
    storage: &'s mut [Slot],
) -> Result<Serving<'s>, Refusal> {
    let device = accepted.device;
    let ring_held = accepted
        .ring
        .hold(0, accepted.ring.len_pages())
        .map_err(|_| Refusal::Header)?;
    let data_held = accepted
        .data
        .hold(0, accepted.data.len_pages())
        .map_err(|_| Refusal::Device)?;
    let memory = Pages::over(&ring_held, accepted.ring.len_bytes());
    let side = KernelSide::attach(memory, accepted.ring.len_bytes(), device, storage)
        .map_err(|_| Refusal::Header)?;
    let limits = Limits::new(
        device.block_size(),
        device.capacity(),
        device.max_sectors(),
        MAX_PARTS,
        u32::try_from(side.capacity()).unwrap_or(u32::MAX).max(1),
    )
    .map_err(|_| Refusal::Device)?;
    // The wire protocol has no refusal for memory; the device's is nearest.
    let kernel_port = Port::new().map_err(|_| Refusal::Device)?;
    let fresh = Regions::over(
        data_held,
        accepted.data.len_bytes(),
        &device,
        side.capacity(),
    )
    .ok_or(Refusal::Device)?;
    let regions = Arc::clone(&fresh.data);
    let name = accepted.name;
    let (disk, registration) = match take_parked(start.location, &device, &name) {
        // A dead driver's disk, taken up with what waited on it.
        Some(parked) => {
            parked.disk.take_up(Arc::clone(&kernel_port));
            (parked.disk, parked.registration)
        }
        None => {
            let disk = Arc::new(RingDisk::new(device, limits, Arc::clone(&kernel_port)));
            // The device node the ring was made for, which sysfs shows the
            // disk in.
            let node = CLAIMS
                .lock()
                .iter()
                .find(|claim| claim.id == start.id)
                .map(|claim| claim.device.index());
            let registration = register_block_from(
                name.as_str().as_bytes(),
                VIRTIO_BLK_MAJOR,
                name.minor(),
                // Its reads and writes are charged to the job that makes
                // them (`fs::blkio`, the `io` controller).
                crate::fs::blkio::account_disk(
                    Arc::clone(&disk) as Arc<dyn BlockDevice>,
                    VIRTIO_BLK_MAJOR,
                    name.minor(),
                ),
                Origin {
                    node,
                    serial: accepted.serial,
                },
            )
            .map_err(|refused| match refused {
                BlockRefused::InvalidName => Refusal::Name,
                BlockRefused::NameInUse | BlockRefused::NumberInUse => Refusal::NameInUse,
            })?;
            (disk, registration)
        }
    };
    if served_elsewhere(start.id, start.location) {
        // Refused: a disk taken up waits on, parked, for a ring that is not.
        park(start.location, name, disk, registration);
        return Err(Refusal::LocationInUse);
    }
    // The driver reset the device before it sent HELLO, so what a dead one's
    // pins kept from the allocator can go back (`object::pin`'s quarantine).
    let node = CLAIMS
        .lock()
        .iter()
        .find(|claim| claim.id == start.id)
        .map(|claim| Arc::clone(&claim.device));
    if let Some(node) = node {
        object::pin::quarantine_release(&node);
    }
    // Where a read's bytes will wait, before anything goes on the ring.
    disk.serve_from(fresh);
    // Served from before READY goes out, not after: the driver may act on
    // READY, and devmgr may ask after the device, the instant it is sent.
    set_served(start.id, Some((start.location, Arc::clone(&start.control))));
    crate::discovery::devmgr::published(start.location);
    let ready = Message::Ready.encode().as_bytes().to_vec();
    let handed = (Object::Port(Arc::clone(&kernel_port)), Rights::WRITE);
    if start
        .control
        .write(ready, 1, || Ok::<Vec<Transfer>, Infallible>(vec![handed]))
        .is_err()
    {
        set_served(start.id, None);
        disk.unserved();
        park(start.location, name, disk, registration);
        return Err(Refusal::Malformed);
    }
    Ok(Serving {
        side,
        disk,
        control: Arc::clone(&start.control),
        kernel_port,
        driver_port: accepted.driver_port,
        regions,
        _ring_held: ring_held,
        registration,
        location: start.location,
        name,
        flying: BTreeMap::new(),
        watching: false,
    })
}

/// A data VMO's pages as the ring hands them out: a region per command, each
/// `region_bytes` long.
#[derive(Debug)]
struct Regions {
    /// The pages, read and written through the direct map.
    pages: Pages,
    /// The hold that keeps them. Readers still to copy a finished read out
    /// share it with the ring, so it outlives a ring that ends first.
    _held: Held,
    /// Bytes in one region.
    region_bytes: u64,
}

/// A new ring's data regions, and the lists its disk keeps of them.
#[derive(Debug)]
struct Fresh {
    /// The regions.
    data: Arc<Regions>,
    /// Every region, free.
    free: Vec<u32>,
    /// No reader holds any.
    leases: Vec<u32>,
}

impl Regions {
    /// `held`'s first `bytes` as `count` regions, each the largest request
    /// `device` takes, with their lists allocated up front, so that nothing
    /// handing regions out or back allocates under the disk's lock. `None`
    /// without the memory.
    fn over(held: Held, bytes: u64, device: &Device, count: usize) -> Option<Fresh> {
        let region_bytes = u64::from(device.max_sectors()) * u64::from(device.block_size());
        let count = u32::try_from(count).ok()?;
        let mut free = Vec::new();
        free.try_reserve_exact(count as usize).ok()?;
        free.extend((0..count).rev());
        let mut leases = Vec::new();
        leases.try_reserve_exact(count as usize).ok()?;
        leases.resize(count as usize, 0);
        let data = crate::fallible::try_arc(Regions {
            pages: Pages::over(&held, bytes),
            _held: held,
            region_bytes,
        })
        .ok()?;
        Some(Fresh { data, free, leases })
    }
}

/// Held frames of a VMO, read and written through the direct map.
#[derive(Debug)]
struct Pages {
    /// Each page's direct-map address, in page order.
    virts: Vec<u64>,
    /// Bytes that may be reached.
    bytes: u64,
}

impl Pages {
    /// The pages `held` holds, `bytes` of them reachable.
    fn over(held: &Held, bytes: u64) -> Pages {
        Pages {
            virts: held
                .frames()
                .iter()
                .map(|&frame| mm::direct_map(frame * PAGE_SIZE))
                .collect(),
            bytes,
        }
    }

    /// Where byte `offset` is, if it is inside.
    fn address(&self, offset: u64) -> Option<*mut u8> {
        if offset >= self.bytes {
            return None;
        }
        let page = usize::try_from(offset / PAGE_SIZE).ok()?;
        let virt = self.virts.get(page)?;
        Some((virt + offset % PAGE_SIZE) as *mut u8)
    }

    /// Whether `len` bytes from `offset` are all inside.
    fn holds(&self, offset: u64, len: usize) -> bool {
        u64::try_from(len)
            .ok()
            .and_then(|len| offset.checked_add(len))
            .is_some_and(|end| end <= self.bytes)
    }

    /// Copy `out.len()` bytes from `offset` into `out`, once each; `false`,
    /// with nothing copied, if the range runs past the end.
    ///
    /// The driver can write these pages at any moment, so they are read the
    /// way the ring's words are: volatile loads, never a plain slice copy,
    /// which Rust would be entitled to assume nothing else is writing. Whole
    /// machine words where the source is aligned, bytes at the ragged ends,
    /// page by page; whatever each load finds is taken as data. `out` is the
    /// kernel's own memory, and what the caller checks afterwards -- btrfs's
    /// checksums -- is checked there, so a driver changing the pages after
    /// this returns changes nothing the kernel acts on.
    fn copy_out(&self, offset: u64, out: &mut [u8]) -> bool {
        if !self.holds(offset, out.len()) {
            return false;
        }
        let mut done = 0;
        while done < out.len() {
            let at = offset + done as u64;
            let Some(address) = self.address(at) else {
                return false;
            };
            let room = usize::try_from(PAGE_SIZE - at % PAGE_SIZE).unwrap_or(usize::MAX);
            let chunk = room.min(out.len() - done);
            let Some(into) = out.get_mut(done..done + chunk) else {
                return false;
            };
            // SAFETY: `address` and the `chunk` bytes after it lie in one page
            // (`chunk` stops at the page's end), a frame the `Held` behind
            // this `Pages` keeps for as long as the `Pages` lives, reached
            // through the direct map, which maps all of RAM readable. Nothing
            // here holds a Rust reference to those bytes.
            unsafe { load_shared(address.addr(), into) };
            done += chunk;
        }
        true
    }

    /// Copy `bytes` into the region at `offset`; `false`, with nothing
    /// written, if the range runs past the end.
    fn copy_in(&self, offset: u64, bytes: &[u8]) -> bool {
        if !self.holds(offset, bytes.len()) {
            return false;
        }
        let mut done = 0;
        while done < bytes.len() {
            let at = offset + done as u64;
            let Some(address) = self.address(at) else {
                return false;
            };
            let room = usize::try_from(PAGE_SIZE - at % PAGE_SIZE).unwrap_or(usize::MAX);
            let chunk = room.min(bytes.len() - done);
            let Some(from) = bytes.get(done..done + chunk) else {
                return false;
            };
            // SAFETY: as in `copy_out`, the destination is `chunk` bytes of
            // one held page through the direct map. The driver may read them
            // at any moment, so they are stored volatilely; nothing else
            // writes this region while a command is built in it, since it is
            // a free region the ring hands out only once, until it completes.
            unsafe { store_shared(address.addr(), from) };
            done += chunk;
        }
        true
    }

    /// The address of a naturally aligned `T` at `offset`, if all of it is
    /// inside. Aligned, it cannot cross a page, since a page is a multiple
    /// of every width asked for.
    fn word<T>(&self, offset: usize) -> Option<*mut T> {
        let size = size_of::<T>();
        if !offset.is_multiple_of(size) || !self.holds(offset as u64, size) {
            return None;
        }
        let offset = offset as u64;
        let virt = self.virts.get(usize::try_from(offset / PAGE_SIZE).ok()?)?;
        Some((virt + offset % PAGE_SIZE) as *mut T)
    }
}

/// One volatile load from the pages the driver shares: every load this file
/// makes from them is this one, and every store [`store`].
///
/// # Safety
///
/// `at` must be aligned for a `T` and readable for one, for the whole call.
/// The driver may write it meanwhile; whatever it held is what is loaded.
unsafe fn load<T: Copy>(at: *const T) -> T {
    // SAFETY: the caller's.
    unsafe { core::ptr::read_volatile(at) }
}

/// One volatile store to the pages the driver shares.
///
/// # Safety
///
/// `at` must be aligned for a `T` and writable for one, for the whole call,
/// with no Rust reference to it anywhere.
unsafe fn store<T: Copy>(at: *mut T, value: T) {
    // SAFETY: the caller's.
    unsafe { core::ptr::write_volatile(at, value) }
}

/// Bytes in the words [`load_shared`] and [`store_shared`] move.
const WORD: usize = size_of::<usize>();

/// Copy `into.len()` bytes from the address `from` with volatile loads:
/// whole machine words where the address is aligned to one and a whole word
/// is left, single bytes elsewhere.
///
/// # Safety
///
/// `from` and the `into.len()` bytes after it must be readable for the whole
/// call. Anything may write them meanwhile: each byte is loaded once, and
/// whatever it held then is what is copied.
unsafe fn load_shared(from: usize, into: &mut [u8]) {
    let mut at = 0;
    while at < into.len() {
        let address = from + at;
        if address.is_multiple_of(WORD) && into.len() - at >= WORD {
            // SAFETY: an aligned word inside the range the caller vouches for.
            let word = unsafe { load(address as *const usize) };
            if let Some(slot) = into.get_mut(at..at + WORD) {
                slot.copy_from_slice(&word.to_ne_bytes());
            }
            at += WORD;
        } else {
            // SAFETY: a byte inside the range the caller vouches for.
            let byte = unsafe { load(address as *const u8) };
            if let Some(slot) = into.get_mut(at) {
                *slot = byte;
            }
            at += 1;
        }
    }
}

/// Copy `from` to the address `into` with volatile stores, as
/// [`load_shared`] loads.
///
/// # Safety
///
/// `into` and the `from.len()` bytes after it must be writable for the whole
/// call, and nothing Rust knows of may hold a reference to them.
unsafe fn store_shared(into: usize, from: &[u8]) {
    let mut at = 0;
    while at < from.len() {
        let address = into + at;
        if address.is_multiple_of(WORD)
            && let Some(bytes) = from.get(at..at + WORD)
        {
            let mut word = [0_u8; WORD];
            word.copy_from_slice(bytes);
            // SAFETY: an aligned word inside the range the caller vouches for.
            unsafe { store(address as *mut usize, usize::from_ne_bytes(word)) };
            at += WORD;
        } else {
            let byte = from.get(at).copied().unwrap_or(0);
            // SAFETY: a byte inside the range the caller vouches for.
            unsafe { store(address as *mut u8, byte) };
            at += 1;
        }
    }
}

impl RingMemory for Pages {
    fn read_u8(&self, offset: usize) -> u8 {
        let Some(address) = self.address(offset as u64) else {
            return 0;
        };
        // SAFETY: as in `Pages::copy_out`.
        unsafe { load(address) }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        let Some(address) = self.address(offset as u64) else {
            return;
        };
        // SAFETY: `address` is inside a held frame, as in `Pages::copy_out`;
        // the driver's own mapping of it is the only other writer, and the
        // protocol gives each byte one writer.
        unsafe { store(address, value) };
    }

    // The wider accessors are one access each, never the trait's byte-wise
    // defaults: the indices and want-bell flags are `u32`s the driver reads
    // and writes while the kernel does, and a torn `comp_tail` read across
    // the driver's store is a ring run backwards, corruption, a driver
    // declared dead (F-45's rule for the virtqueue, kept here too). Every
    // shared field is naturally aligned in a page-aligned VMO; an offset that
    // is not falls back to bytes, which only a hostile layout reaches.

    fn read_u16(&self, offset: usize) -> u16 {
        match self.word::<u16>(offset) {
            // SAFETY: an aligned `u16` inside a held frame, as in `read_u8`.
            Some(at) => u16::from_le(unsafe { load(at) }),
            None => {
                u16::from_le_bytes([self.read_u8(offset), self.read_u8(offset.wrapping_add(1))])
            }
        }
    }

    fn read_u32(&self, offset: usize) -> u32 {
        match self.word::<u32>(offset) {
            // SAFETY: an aligned `u32` inside a held frame, as in `read_u8`.
            Some(at) => u32::from_le(unsafe { load(at) }),
            None => {
                u32::from(self.read_u16(offset))
                    | (u32::from(self.read_u16(offset.wrapping_add(2))) << 16)
            }
        }
    }

    fn read_u64(&self, offset: usize) -> u64 {
        // Two `u32` loads, not one `u64`: no 64-bit field of the ring is
        // shared by both sides as an index, and ARMv7-A has no single-copy
        // atomic 64-bit load to promise.
        u64::from(self.read_u32(offset)) | (u64::from(self.read_u32(offset.wrapping_add(4))) << 32)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        match self.word::<u16>(offset) {
            // SAFETY: an aligned `u16` inside a held frame, as in `write_u8`.
            Some(at) => unsafe { store(at, value.to_le()) },
            None => {
                let [low, high] = value.to_le_bytes();
                self.write_u8(offset, low);
                self.write_u8(offset.wrapping_add(1), high);
            }
        }
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        match self.word::<u32>(offset) {
            // SAFETY: an aligned `u32` inside a held frame, as in `write_u8`.
            Some(at) => unsafe { store(at, value.to_le()) },
            None => {
                self.write_u16(offset, value as u16);
                self.write_u16(offset.wrapping_add(2), (value >> 16) as u16);
            }
        }
    }

    fn write_u64(&mut self, offset: usize, value: u64) {
        self.write_u32(offset, value as u32);
        self.write_u32(offset.wrapping_add(4), (value >> 32) as u32);
    }

    fn barrier(&self) {
        // The want-bell handshake (BLOCK-RING.md section 5.1) needs every
        // store before this ordered against every load after it, on the
        // processor the driver reads from: a full fence, which is Rust's
        // `fence` and needs no assembly of its own.
        core::sync::atomic::fence(Ordering::SeqCst);
    }
}

/// How many wait queues a disk spreads its callers over. A finished request
/// wakes only the queue its own caller sleeps on, so one completion wakes one
/// caller rather than every caller of the disk; two callers that share a
/// queue wake each other for nothing, which costs a look and no more. At most
/// 64, the bits of the mask [`answer`] returns.
const WAKE_SLOTS: usize = 64;
const _: () = assert!(WAKE_SLOTS <= 64, "a wake mask is one u64");

/// Requests that found the ring's task asleep and nudged it.
pub(crate) static NUDGES_SENT: AtomicU64 = AtomicU64::new(0);
/// Requests that found it awake, or nudged already, and did not.
pub(crate) static NUDGES_SPARED: AtomicU64 = AtomicU64::new(0);
/// Nudges the task found missing on waking: something it could have
/// dispatched was queued while it slept, and either nobody nudged it or the
/// nudge claimed never reached its port. The driver check holds this at zero.
pub(crate) static NUDGES_LOST: AtomicU64 = AtomicU64::new(0);

/// A finished read's bytes, waiting in a data region for their caller to
/// copy them out. The region is not handed out again until every caller it
/// holds bytes for has done so, so the next command cannot overwrite them.
#[derive(Debug)]
struct Landing {
    /// The ring's regions, kept by this even if the ring ends first.
    data: Arc<Regions>,
    /// The region.
    region: u32,
    /// Where this request's bytes start in the data VMO.
    offset: u64,
    /// How many there are.
    len: usize,
}

/// A request a caller is waiting on.
#[derive(Debug)]
struct Pending {
    /// A write's bytes, until the ring's task copies them into the data
    /// region it dispatches the command through.
    payload: Option<Vec<u8>>,
    /// What came of it, once it has: where a read's bytes wait, or nothing
    /// for a write or a flush.
    result: Option<Result<Option<Landing>, Errno>>,
    /// Whether its caller stopped waiting; the answer is then thrown away.
    abandoned: bool,
    /// Which of the disk's wait queues its caller sleeps on.
    slot: usize,
}

/// Where the ring's task is, as the readers that might nudge it see it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskIs {
    /// Running, or about to look at the queue again: nobody nudges it.
    Awake,
    /// Asleep with nothing it could dispatch: the next caller to make
    /// something dispatchable nudges it.
    Idle,
    /// Asleep, and a caller has queued its nudge.
    Nudged,
}

/// What a ring's disk shares between its readers and its task.
#[derive(Debug)]
struct DiskState {
    /// Reads not yet on the ring, and commands on it.
    queue: Queue,
    /// Every read submitted and not yet collected, by its request id.
    reads: BTreeMap<u64, Pending>,
    /// The next request id.
    next_id: u64,
    /// Whether the ring is over.
    ended: bool,
    /// When its driver died, while it waits parked for the next one.
    parked_at: Option<u64>,
    /// The serving ring's data regions, while a ring serves the disk.
    data: Option<Arc<Regions>>,
    /// Its regions not in use. Its capacity is every region, so giving one
    /// back never allocates.
    free: Vec<u32>,
    /// For each of its regions, how many callers still have to copy a
    /// read's bytes out of it.
    leases: Vec<u32>,
    /// Whether the queue holds back what it has queued until a command on
    /// the ring completes -- barrier order, or no room on the ring -- so
    /// that only the completion, whose bell wakes the task, can change it.
    held_back: bool,
    /// Where the ring's task is: see [`DiskState::claim_nudge`].
    task: TaskIs,
}

impl DiskState {
    /// Whether the task could put something on the ring now.
    fn dispatchable(&self) -> bool {
        !self.held_back && !self.free.is_empty() && self.queue.queued() > 0
    }

    /// Asked, under the disk's lock, by whoever just made something
    /// dispatchable -- a request queued, a region given back: `true` if the
    /// task went to sleep without it and this caller must nudge it, which
    /// it does before letting go of the lock.
    ///
    /// The task says it is going to sleep under the same lock, and only
    /// after finding nothing dispatchable ([`Serving::go_idle`]). Whichever
    /// takes the lock second sees what the other did: a caller that came
    /// first left something the task's look finds, and one that came second
    /// finds the task `Idle` and nudges it. Once nudged, it is not nudged
    /// again until it has woken, so a burst of requests costs one packet.
    fn claim_nudge(&mut self) -> bool {
        if self.task == TaskIs::Idle && self.dispatchable() {
            self.task = TaskIs::Nudged;
            return true;
        }
        false
    }

    /// Put `region` back on the free list.
    fn give_back(&mut self, region: u32) {
        debug_assert!(
            !self.free.contains(&region),
            "a data region was given back while it was free"
        );
        if self.free.len() < self.free.capacity() {
            // NOALLOC: below the capacity reserved for every region.
            self.free.push(region);
        }
    }

    /// A caller has copied its bytes out of `landing`'s region: the region
    /// goes back once nobody else is still to.
    fn return_lease(&mut self, landing: &Landing) {
        if !self
            .data
            .as_ref()
            .is_some_and(|data| Arc::ptr_eq(data, &landing.data))
        {
            // Its ring has ended, and its regions went with it.
            return;
        }
        let region = landing.region;
        let Some(count) = self.leases.get_mut(region as usize) else {
            return;
        };
        debug_assert!(*count > 0, "a data region's lease was returned twice");
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.give_back(region);
        }
    }
}

/// A block device served by a ring-3 driver through a ring.
pub(crate) struct RingDisk {
    /// The device, as HELLO described it.
    device: Device,
    /// Reads, shared with the ring's task.
    state: SpinLock<DiskState>,
    /// Woken when a request finishes, each its own caller's queue, and all
    /// of them when the ring ends.
    done: [WaitQueue; WAKE_SLOTS],
    /// The queue the next caller sleeps on.
    next_slot: AtomicUsize,
    /// The serving ring's completion port, nudged when a read is queued:
    /// the next ring's once a parked disk is taken up again.
    port: SpinLock<Arc<Port>>,
}

impl fmt::Debug for RingDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingDisk")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

impl RingDisk {
    /// A disk for `device`, whose queue takes `limits`, nudging `port`.
    fn new(device: Device, limits: Limits, port: Arc<Port>) -> RingDisk {
        RingDisk {
            device,
            state: SpinLock::new(DiskState {
                queue: Queue::new(limits, Config::fast_device()),
                reads: BTreeMap::new(),
                next_id: 0,
                ended: false,
                parked_at: None,
                data: None,
                free: Vec::new(),
                leases: Vec::new(),
                held_back: false,
                task: TaskIs::Awake,
            }),
            done: [const { WaitQueue::new() }; WAKE_SLOTS],
            next_slot: AtomicUsize::new(0),
            port: SpinLock::new(port),
        }
    }

    /// Be served by a new ring, which `port` is the completion port of.
    fn take_up(&self, port: Arc<Port>) {
        *self.port.lock() = port;
        self.state.lock().parked_at = None;
    }

    /// Hand out `fresh`'s regions from now on: the serving ring's, whose
    /// task is awake and about to look at the queue.
    fn serve_from(&self, fresh: Fresh) {
        let Fresh {
            data,
            mut free,
            mut leases,
        } = fresh;
        let mut data = Some(data);
        {
            let mut state = self.state.lock();
            core::mem::swap(&mut state.data, &mut data);
            core::mem::swap(&mut state.free, &mut free);
            core::mem::swap(&mut state.leases, &mut leases);
            state.held_back = false;
            state.task = TaskIs::Awake;
        }
        // Whatever an earlier ring left, let go of outside the lock.
        drop((data, free, leases));
    }

    /// No ring serves the disk any more. Its regions go once the last
    /// caller still copying out of one has let go of it.
    fn unserved(&self) {
        let left = {
            let mut state = self.state.lock();
            state.held_back = false;
            state.task = TaskIs::Awake;
            (
                state.data.take(),
                core::mem::take(&mut state.free),
                core::mem::take(&mut state.leases),
            )
        };
        drop(left);
    }

    /// The disk is over: fail every outstanding request and wake every
    /// caller.
    fn end(&self) {
        {
            let mut state = self.state.lock();
            state.ended = true;
            // A reader that gave up is not coming back for its answer.
            state.reads.retain(|_, pending| !pending.abandoned);
            for pending in state.reads.values_mut() {
                if pending.result.is_none() {
                    pending.result = Some(Err(Errno::EIO));
                }
            }
        }
        for queue in &self.done {
            queue.wake_all();
        }
    }

    /// Wake the queues `mask` names, one bit each.
    fn wake(&self, mask: u64) {
        for (index, queue) in self.done.iter().enumerate() {
            if mask & (1 << index) != 0 {
                queue.wake_all();
            }
        }
    }

    /// Run `stamp` on each of the wait queues `mask` names: the trip trace's
    /// points (`sched::trip`), which name a traced read by its caller's queue.
    fn each(&self, mut mask: u64, mut stamp: impl FnMut(&WaitQueue)) {
        while mask != 0 {
            if let Some(queue) = self.done.get(mask.trailing_zeros() as usize) {
                stamp(queue);
            }
            mask &= mask - 1;
        }
    }

    /// The wait queue the next caller sleeps on.
    fn slot(&self) -> usize {
        self.next_slot.fetch_add(1, Ordering::Relaxed) % WAKE_SLOTS
    }

    /// Nudge the task: called holding the disk's lock, having claimed the
    /// nudge, so that the packet is on the port before anyone else can look
    /// at the task's state -- which is what lets [`Serving::wake_up`] tell a
    /// nudge that is on its way from one that was lost.
    fn nudge(&self) {
        // A full port already holds a packet the task has not taken.
        let _ = self.port.lock().queue_user(SUBMIT_KEY, [0; 2]);
        let _ = NUDGES_SENT.fetch_add(1, Ordering::Relaxed);
    }

    /// Queue one request for a caller that will sleep on queue `slot`, and
    /// answer its id; the id in `request` is replaced by the disk's next one.
    /// `payload` is a write's bytes, which the ring's task copies into the
    /// data region when it dispatches the command.
    fn submit(
        &self,
        request: Request,
        payload: Option<Vec<u8>>,
        slot: usize,
    ) -> Result<u64, Errno> {
        let mut state = self.state.lock();
        let given_up = state
            .parked_at
            .is_some_and(|at| timer::now_nanos().saturating_sub(at) > PARK_PATIENCE_NANOS);
        if state.ended || given_up {
            return Err(Errno::EIO);
        }
        let id = state.next_id;
        state.next_id = id.wrapping_add(1);
        let request = Request {
            id: ferrix_block::RequestId(id),
            ..request
        };
        state
            .queue
            .submit(ticks(), request)
            .map_err(|_| Errno::EIO)?;
        let _ = state.reads.insert(
            id,
            Pending {
                payload,
                result: None,
                abandoned: false,
                slot,
            },
        );
        if let Some(queue) = self.done.get(slot) {
            sched::trip::queued(queue);
        }
        if state.claim_nudge() {
            self.nudge();
        } else {
            let _ = NUDGES_SPARED.fetch_add(1, Ordering::Relaxed);
        }
        Ok(id)
    }

    /// Sleep on queue `slot` until every request `ids` names has an answer,
    /// the ring is over, or the patience runs out.
    fn wait(&self, ids: &[u64], slot: usize) {
        let Some(queue) = self.done.get(slot) else {
            return;
        };
        // Not interruptible by signals, as a disk read on Linux is not; the
        // ring's end, or the patience, is what ends it.
        let deadline = timer::now_nanos().saturating_add(READ_PATIENCE_NANOS);
        // The reader is whoever reads through the registered disk, a user
        // thread inside a read as often as not, so its process being
        // terminated ends the wait too: a killed process lets go of what it
        // holds only once its last thread is out of the kernel.
        let process = crate::syscall::process::current();
        let terminated = || {
            process
                .as_ref()
                .is_some_and(|process| process.is_terminated())
        };
        let _ = queue.wait_until_deadline(|| self.finished(ids) || terminated(), deadline);
        sched::trip::reader_running(queue);
    }

    /// Whether every request `ids` names has an answer, or the ring is over.
    fn finished(&self, ids: &[u64]) -> bool {
        let state = self.state.lock();
        state.ended
            || ids.iter().all(|id| {
                state
                    .reads
                    .get(id)
                    .is_none_or(|pending| pending.result.is_some())
            })
    }

    /// Take request `id`'s answer, copying a read's bytes into `out`, which
    /// must be exactly as long; a write or a flush brings nothing, and takes
    /// an empty `out`. A request with no answer yet is given up: its answer
    /// is thrown away when it comes.
    ///
    /// The one copy a read's bytes take, from the driver's data region into
    /// the caller's buffer, is made here, by the caller, holding no lock:
    /// the region is the caller's until it lets go of it below.
    fn collect(&self, id: u64, out: &mut [u8]) -> Result<(), Errno> {
        let result = {
            let mut state = self.state.lock();
            match state.reads.get_mut(&id) {
                Some(pending) if pending.result.is_none() => {
                    // The wait gave up. Marked, so the answer, when it comes,
                    // is thrown away rather than kept for an id that will be
                    // used again.
                    pending.abandoned = true;
                    return Err(Errno::EIO);
                }
                Some(_) => state.reads.remove(&id).and_then(|pending| pending.result),
                None => None,
            }
        };
        match result {
            None => Err(Errno::EIO),
            Some(Err(errno)) => Err(errno),
            Some(Ok(None)) if out.is_empty() => Ok(()),
            Some(Ok(None)) => Err(Errno::EIO),
            Some(Ok(Some(landing))) => {
                let copied =
                    landing.len == out.len() && landing.data.pages.copy_out(landing.offset, out);
                self.let_go(landing);
                if copied { Ok(()) } else { Err(Errno::EIO) }
            }
        }
    }

    /// The caller has its bytes: `landing`'s region may be handed out again
    /// once nobody else is still to copy out of it.
    fn let_go(&self, landing: Landing) {
        {
            let mut state = self.state.lock();
            state.return_lease(&landing);
            if state.claim_nudge() {
                self.nudge();
            }
        }
        // The regions, if their ring has ended and this was the last hold on
        // them, are let go of outside the lock.
        drop(landing);
    }

    /// Submit one request, wait for it, and take its answer into `out`.
    fn request(
        &self,
        request: Request,
        payload: Option<Vec<u8>>,
        out: &mut [u8],
    ) -> Result<(), Errno> {
        let slot = self.slot();
        let id = self.submit(request, payload, slot)?;
        self.wait(&[id], slot);
        self.collect(id, out)
    }

    /// Write `payload` — `count` whole sectors — at `sector`, as one request.
    fn write_request(&self, sector: u64, count: u32, payload: Vec<u8>) -> Result<(), Errno> {
        self.request(
            Request::write(0, sector, count).sync(),
            Some(payload),
            &mut [],
        )
    }

    /// Make everything written durable: one flush, which the queue treats as
    /// a barrier no request crosses.
    fn flush_request(&self) -> Result<(), Errno> {
        self.request(Request::flush(0), None, &mut [])
    }
}

impl BlockDevice for RingDisk {
    /// Read whole sectors, split into what the device takes at once. Every
    /// piece is queued before any is waited for, so a read longer than one
    /// request is one wait, not one per piece.
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let size = usize::try_from(self.device.block_size()).map_err(|_| Errno::EIO)?;
        if size == 0 || buf.is_empty() || !buf.len().is_multiple_of(size) {
            return Err(Errno::EINVAL);
        }
        let most = usize::try_from(self.device.max_sectors())
            .unwrap_or(usize::MAX)
            .saturating_mul(size)
            .max(size);
        let mut ids = Vec::new();
        ids.try_reserve_exact(buf.len().div_ceil(most))
            .map_err(|_| Errno::ENOMEM)?;
        let slot = self.slot();
        let mut outcome = Ok(());
        let mut at = sector;
        for chunk in buf.chunks(most) {
            let Ok(count) = u32::try_from(chunk.len() / size) else {
                outcome = Err(Errno::EIO);
                break;
            };
            let next = at.checked_add(u64::from(count));
            match self.submit(Request::read(0, at, count), None, slot) {
                // NOALLOC: one id per chunk, reserved above.
                Ok(id) => ids.push(id),
                Err(errno) => {
                    outcome = Err(errno);
                    break;
                }
            }
            let Some(next) = next else {
                outcome = Err(Errno::EIO);
                break;
            };
            at = next;
        }
        // What was queued is waited for and collected even after a piece
        // failed to queue, so that no answer is left behind.
        self.wait(&ids, slot);
        for (&id, chunk) in ids.iter().zip(buf.chunks_mut(most)) {
            let collected = self.collect(id, chunk);
            if outcome.is_ok() {
                outcome = collected;
            }
        }
        outcome
    }

    fn sectors(&self) -> u64 {
        self.device.capacity()
    }

    fn sector_size(&self) -> u32 {
        self.device.block_size()
    }

    fn read_only(&self) -> bool {
        self.device.flags().contains(DeviceFlags::READ_ONLY)
    }

    /// Write whole sectors, split like a read into what the device takes at
    /// once. A disk the driver called read-only is refused here rather than
    /// at the far end, so a mount learns why.
    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), Errno> {
        if self.read_only() {
            return Err(Errno::EROFS);
        }
        let size = usize::try_from(self.device.block_size()).map_err(|_| Errno::EIO)?;
        if size == 0 || buf.is_empty() || !buf.len().is_multiple_of(size) {
            return Err(Errno::EINVAL);
        }
        let most = usize::try_from(self.device.max_sectors())
            .unwrap_or(usize::MAX)
            .saturating_mul(size);
        let mut at = sector;
        for chunk in buf.chunks(most.max(size)) {
            let count = u32::try_from(chunk.len() / size).map_err(|_| Errno::EIO)?;
            self.write_request(at, count, chunk.to_vec())?;
            at = at.checked_add(u64::from(count)).ok_or(Errno::EIO)?;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), Errno> {
        if self.read_only() {
            return Ok(());
        }
        self.flush_request()
    }
}

/// The time `src/lib/fs/block`'s queue reckons in: milliseconds.
fn ticks() -> u64 {
    timer::now_nanos() / 1_000_000
}

/// A ring being served, owned by its task.
struct Serving<'s> {
    /// The kernel's side of the ring.
    side: KernelSide<'s, Pages>,
    /// The disk it serves.
    disk: Arc<RingDisk>,
    /// The kernel's end of the control channel.
    control: Arc<Endpoint>,
    /// The completion port: the driver's bells, the control channel's
    /// signals and readers' nudges.
    kernel_port: Arc<Port>,
    /// The driver's port, rung when the driver asks.
    driver_port: Arc<Port>,
    /// The data VMO's regions, which the disk hands out and takes back.
    regions: Arc<Regions>,
    /// The ring VMO's pages, held while the ring is served.
    _ring_held: Held,
    /// The disk's node, unpublished when a stopped ring is over and kept
    /// with the disk when its driver died.
    registration: BlockRegistration,
    /// The PCI location the ring serves, which a parked disk waits at.
    location: Location,
    /// The disk's name, which the next driver must give again.
    name: DiskName,
    /// The region each submission on the ring uses, by its token.
    flying: BTreeMap<u64, u32>,
    /// Whether a registration on the control channel is waiting to fire.
    watching: bool,
}

impl fmt::Debug for Serving<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Serving")
            .field("side", &self.side)
            .field("flying", &self.flying.len())
            .finish_non_exhaustive()
    }
}

impl Serving<'_> {
    /// Serve until the ring is over, and say how it ended.
    fn serve(&mut self) -> Ending {
        loop {
            if let Some(ending) = self.take_control() {
                return ending;
            }
            let dispatched = self.dispatch();
            if let Some(bell) = self.side.publish() {
                self.disk.each(dispatched, |reader| {
                    sched::trip::bell(reader, self.driver_port.waiters());
                });
                // A full port already holds a bell the driver has not taken.
                let _ = self.driver_port.queue_user(bell.key(), bell.packet().data);
            }
            if self.complete().is_err() {
                return Ending::DriverDied;
            }
            match self.side.prepare_to_sleep() {
                Ok(Wait::Pending(_)) => continue,
                Ok(Wait::Sleep) => {}
                Err(_) => return Ending::DriverDied,
            }
            if !self.go_idle() {
                self.side.woke();
                continue;
            }
            let nudged = self.sleep();
            self.wake_up(nudged);
            self.side.woke();
        }
    }

    /// Read what the driver said on the control channel: `Some` if the ring
    /// is over.
    fn take_control(&mut self) -> Option<Ending> {
        loop {
            match self
                .control
                .read(CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, false)
            {
                Ok(message) => {
                    let stopped = matches!(Message::decode(&message.bytes), Ok(Message::Stopped));
                    object::dispose(message.handles.into_iter().map(|(object, _)| object));
                    if stopped {
                        return Some(Ending::Stopped);
                    }
                }
                Err(ReadError::Empty) => return None,
                Err(
                    ReadError::PeerClosed | ReadError::NeedsTopology | ReadError::TooSmall { .. },
                ) => {
                    return Some(Ending::DriverDied);
                }
            }
        }
    }

    /// About to sleep: say so, unless something could be dispatched now,
    /// and answer whether to sleep. See [`DiskState::claim_nudge`] for why
    /// the look and the flag are one step under the disk's lock.
    fn go_idle(&self) -> bool {
        let mut state = self.disk.state.lock();
        if state.dispatchable() {
            return false;
        }
        state.task = TaskIs::Idle;
        true
    }

    /// Awake again, for whatever reason, having taken a nudge's packet if
    /// `nudged`. Something dispatchable found here while the task is still
    /// `Idle` was queued with nobody nudging it; one found `Nudged` with no
    /// packet taken and none on the port is a nudge claimed and never sent.
    /// Either is a nudge lost, which cost a caller the wait for this task's
    /// recheck, and is counted for the driver check.
    fn wake_up(&self, nudged: bool) {
        let mut state = self.disk.state.lock();
        let lost = match state.task {
            TaskIs::Idle => state.dispatchable(),
            TaskIs::Nudged => !nudged && self.kernel_port.is_empty(),
            TaskIs::Awake => false,
        };
        if lost {
            let _ = NUDGES_LOST.fetch_add(1, Ordering::Relaxed);
        }
        state.task = TaskIs::Awake;
    }

    /// Put a write command's bytes into the region it is dispatched through,
    /// before the driver is told about it.
    ///
    /// A merged command's parts tile its range, so each part's bytes go where
    /// its own sectors are. `false` if a part's payload is missing or does
    /// not fit, and the command is then failed rather than sent with
    /// whatever the region held.
    fn fill(
        &self,
        state: &DiskState,
        parts: &[(u64, u64)],
        sector: u64,
        offset: u64,
        block: u64,
    ) -> bool {
        parts.iter().all(|&(id, at)| {
            let bytes = state
                .reads
                .get(&id)
                .and_then(|pending| pending.payload.as_deref());
            bytes.is_some_and(|bytes| {
                self.regions.pages.copy_in(
                    offset + at.saturating_sub(sector).saturating_mul(block),
                    bytes,
                )
            })
        })
    }

    /// Move queued requests onto the ring while regions and ring slots last,
    /// and answer the wait queues of the callers whose requests went on.
    fn dispatch(&mut self) -> u64 {
        let mut wake = 0;
        let mut dispatched = 0;
        {
            let mut state = self.disk.state.lock();
            state.held_back = false;
            let block = u64::from(state.queue.limits().logical_block_size());
            while let Some(&region) = state.free.last() {
                let Some(command) = state.queue.dispatch(ticks()) else {
                    // What is still queued waits for a command on the ring
                    // to complete, and the completion's bell wakes the task.
                    state.held_back = state.queue.queued() > 0;
                    break;
                };
                let (token, op, sector, count) =
                    (command.token, command.op, command.sector, command.count);
                // Copied out, into room on the stack rather than the heap, so
                // the queue is no longer borrowed while the payloads, which
                // live beside it, are read.
                let (room, len) = parts_of(command.parts);
                let whole = len == command.parts.len();
                let parts = room.get(..len).unwrap_or_default();
                let offset = u64::from(region) * self.regions.region_bytes;
                let submission = match op {
                    // More parts than the queue was told it may merge.
                    _ if !whole => None,
                    Op::Flush => Some(Submission::flush(token.raw())),
                    Op::Write if !self.fill(&state, parts, sector, offset, block) => None,
                    Op::Write => Some(Submission::write(token.raw(), sector, count, offset)),
                    _ => Some(Submission::read(token.raw(), sector, count, offset)),
                };
                let Some(submission) = submission else {
                    wake |= answer(&mut state, token, Err(Errno::EIO), None);
                    continue;
                };
                match self.side.submit(submission) {
                    Ok(()) => {
                        let _ = state.free.pop();
                        let _ = self.flying.insert(token.raw(), region);
                        crate::fs::seam::submitted();
                        let slots = slots_of(&state.reads, parts);
                        self.stamp_on_ring(slots);
                        dispatched |= slots;
                    }
                    Err(SubmitError::Full) => {
                        let _ = state.queue.requeue(token);
                        state.held_back = true;
                        break;
                    }
                    Err(_) => wake |= answer(&mut state, token, Err(Errno::EIO), None),
                }
            }
        }
        // Only the callers something happened to: the ones whose requests
        // failed here. The rest are on the ring, and wait for completions.
        if wake != 0 {
            self.disk.wake(wake);
        }
        dispatched
    }

    /// The trip trace's `OnRing` for the callers `slots` names.
    fn stamp_on_ring(&self, slots: u64) {
        let port = self.kernel_port.waiters();
        self.disk
            .each(slots, |reader| sched::trip::on_ring(reader, port));
    }

    /// Take every completion off the ring and answer its requests.
    fn complete(&mut self) -> Result<(), ferrix_blkring::Corruption> {
        let mut wake = 0;
        while let Some(completed) = self.side.poll()? {
            let token = completed.submission.id;
            let Some(region) = self.flying.remove(&token) else {
                continue;
            };
            crate::fs::seam::completed();
            if completed.device_ticks != 0 {
                let _ = hop_check::DEVICE_TICKS
                    .fetch_add(u64::from(completed.device_ticks), Ordering::Relaxed);
                let _ = hop_check::DEVICE_TIMED.fetch_add(1, Ordering::Relaxed);
            }
            let result = outcome(&completed);
            let offset = u64::from(region) * self.regions.region_bytes;
            let mut state = self.disk.state.lock();
            let answered = answer(
                &mut state,
                Token::from_raw(token),
                result,
                Some((&self.regions, region, offset)),
            );
            // Stamped before the lock goes, which a reader already awake
            // takes the answer at.
            self.disk.each(answered, sched::trip::answered);
            wake |= answered;
            // A read's region waits for its callers to copy their bytes out;
            // anything else's is free again now.
            if state
                .leases
                .get(region as usize)
                .is_none_or(|&count| count == 0)
            {
                state.give_back(region);
            }
            state.held_back = false;
        }
        if wake != 0 {
            self.disk.wake(wake);
        }
        Ok(())
    }

    /// Sleep on the completion port until something arrives, or a while,
    /// and answer whether a caller's nudge was among what arrived.
    ///
    /// Runs in the ring's own kernel thread, never in a process, so the wait
    /// need not watch for a terminated process.
    fn sleep(&mut self) -> bool {
        if !self.watching {
            let observer = Observer::new(
                &self.kernel_port,
                CONTROL_KEY,
                Signals::READABLE | Signals::PEER_CLOSED,
            );
            self.watching = observer.is_ok_and(|observer| self.control.observe(observer).is_ok());
        }
        let deadline = timer::now_nanos().saturating_add(RECHECK_NANOS);
        let port = &self.kernel_port;
        let _ = port
            .waiters()
            .wait_until_deadline(|| !port.is_empty(), deadline);
        // Whichever reader is traced: the task serves them all.
        self.disk.each(u64::MAX, sched::trip::ring_running);
        let mut nudged = false;
        while let Some(packet) = port.take() {
            if packet.key == CONTROL_KEY && packet.kind == PACKET_SIGNAL {
                self.watching = false;
            }
            nudged |= packet.key == SUBMIT_KEY && packet.kind == PACKET_USER;
        }
        nudged
    }

    /// The ring is over. STOPPED ends the disk: every outstanding request
    /// fails and the node goes. A dead driver's disk is parked instead, with
    /// the commands it held put back on the queue for the next ring.
    fn finish(mut self, ending: Ending) {
        let _outstanding = self.side.end(ending).count();
        self.disk.unserved();
        if ending != Ending::DriverDied {
            self.disk.end();
            return;
        }
        {
            let mut state = self.disk.state.lock();
            for &token in self.flying.keys() {
                let _ = state.queue.requeue(Token::from_raw(token));
            }
        }
        let Serving {
            disk,
            registration,
            location,
            name,
            ..
        } = self;
        park(location, name, disk, registration);
    }
}

/// What a completion means for its reads.
fn outcome(completed: &Completed) -> Result<(), Errno> {
    if completed.status == Status::Ok {
        Ok(())
    } else {
        Err(Errno::EIO)
    }
}

/// Finish the command `token` names with `result`, and answer the mask of
/// the wait queues whose callers it answered.
///
/// A read that succeeded brought its bytes into a region: `at` is the
/// regions, the region and the command's offset in the data VMO. Each of its
/// requests is answered with where its own bytes are, and the region counts
/// one lease more for each, which its caller returns once it has copied
/// them. Nothing is copied here, and nothing allocated: this runs under the
/// disk's lock.
fn answer(
    state: &mut DiskState,
    token: Token,
    result: Result<(), Errno>,
    at: Option<(&Arc<Regions>, u32, u64)>,
) -> u64 {
    let Ok(completion) = state.queue.complete(token, ()) else {
        return 0;
    };
    let block = u64::from(state.queue.limits().logical_block_size());
    let mut wake = 0;
    let mut leases = 0_u32;
    for part in completion.parts() {
        let Some(pending) = state.reads.get_mut(&part.id.0) else {
            continue;
        };
        if pending.abandoned {
            let _ = state.reads.remove(&part.id.0);
            continue;
        }
        let answered = result.and_then(|()| {
            if completion.op != Op::Read {
                // A write or a flush brings nothing back.
                return Ok(None);
            }
            let (data, region, offset) = at.ok_or(Errno::EIO)?;
            let start = part
                .sector
                .saturating_sub(completion.sector)
                .saturating_mul(block)
                .saturating_add(offset);
            let len = usize::try_from(u64::from(part.count) * block).map_err(|_| Errno::EIO)?;
            if !data.pages.holds(start, len) {
                return Err(Errno::EIO);
            }
            leases = leases.saturating_add(1);
            Ok(Some(Landing {
                data: Arc::clone(data),
                region,
                offset: start,
                len,
            }))
        });
        pending.result = Some(answered);
        wake |= 1_u64 << (pending.slot % WAKE_SLOTS);
    }
    if leases > 0
        && let Some((_, region, _)) = at
        && let Some(count) = state.leases.get_mut(region as usize)
    {
        *count = count.saturating_add(leases);
    }
    wake
}

/// Each of `parts`' request id and first sector, in room on the stack: as
/// many as [`MAX_PARTS`] and how many there were room for, which is fewer
/// than `parts` only if the queue merged past the limit it was given.
fn parts_of(parts: &[ferrix_block::Part]) -> ([(u64, u64); MAX_PARTS as usize], usize) {
    let mut room = [(0_u64, 0_u64); MAX_PARTS as usize];
    for (slot, part) in room.iter_mut().zip(parts) {
        *slot = (part.id.0, part.sector);
    }
    (room, parts.len().min(MAX_PARTS as usize))
}

/// The wait queues of the callers of `parts`, a bit each.
fn slots_of(reads: &BTreeMap<u64, Pending>, parts: &[(u64, u64)]) -> u64 {
    parts
        .iter()
        .filter_map(|(id, _)| reads.get(id))
        .fold(0, |mask, pending| {
            mask | 1_u64 << (pending.slot % WAKE_SLOTS)
        })
}
