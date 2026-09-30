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
//!    the ring, rings the driver when the driver asked to be rung, takes
//!    completions off the ring, copies a write's bytes in and a read's out of
//!    the data VMO, and wakes the caller;
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
use core::sync::atomic::{AtomicUsize, Ordering};

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
use ferrix_native_abi::types::{CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, PACKET_SIGNAL};
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
    let depth = u32::try_from(side.capacity()).unwrap_or(u32::MAX).max(1);
    let limits = Limits::new(
        device.block_size(),
        device.capacity(),
        device.max_sectors(),
        MAX_PARTS,
        depth,
    )
    .map_err(|_| Refusal::Device)?;
    // The wire protocol has no refusal for memory; the device's is nearest.
    let kernel_port = Port::new().map_err(|_| Refusal::Device)?;
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
        park(start.location, name, disk, registration);
        return Err(Refusal::Malformed);
    }
    let region_bytes = u64::from(device.max_sectors()) * u64::from(device.block_size());
    let regions = u32::try_from(side.capacity()).unwrap_or(u32::MAX);
    Ok(Serving {
        side,
        disk,
        control: Arc::clone(&start.control),
        kernel_port,
        driver_port: accepted.driver_port,
        data: Pages::over(&data_held, accepted.data.len_bytes()),
        _ring_held: ring_held,
        _data_held: data_held,
        registration,
        location: start.location,
        name,
        region_bytes,
        free: (0..regions).collect(),
        flying: BTreeMap::new(),
        watching: false,
    })
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

    /// Copy `out.len()` bytes from `offset` into `out`; `false`, with `out`
    /// partly written, if the range runs past the end.
    fn copy_out(&self, offset: u64, out: &mut [u8]) -> bool {
        for (at, byte) in (offset..).zip(out.iter_mut()) {
            let Some(address) = self.address(at) else {
                return false;
            };
            // SAFETY: `address` is inside a frame the ring's `Held` keeps
            // for as long as this `Pages` is used, reached through the direct
            // map. The driver may write the byte at any moment, so it is
            // read volatilely and whatever it holds is taken as data.
            *byte = unsafe { core::ptr::read_volatile(address) };
        }
        true
    }

    /// Copy `bytes` into the region at `offset`; `false`, with the region
    /// partly written, if the range runs past the end.
    fn copy_in(&self, offset: u64, bytes: &[u8]) -> bool {
        for (at, &byte) in (offset..).zip(bytes.iter()) {
            let Some(address) = self.address(at) else {
                return false;
            };
            // SAFETY: `address` is inside a frame the ring's `Held` keeps for
            // as long as this `Pages` is used, reached through the direct
            // map. The driver may read the byte at any moment, so it is
            // written volatilely; nothing else writes this region while a
            // command is being built in it, because the region is one of the
            // free ones this task owns until it submits.
            unsafe { core::ptr::write_volatile(address, byte) };
        }
        true
    }
}

impl RingMemory for Pages {
    fn read_u8(&self, offset: usize) -> u8 {
        let Some(address) = self.address(offset as u64) else {
            return 0;
        };
        // SAFETY: as in `Pages::copy_out`.
        unsafe { core::ptr::read_volatile(address) }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        let Some(address) = self.address(offset as u64) else {
            return;
        };
        // SAFETY: `address` is inside a held frame, as in `Pages::copy_out`;
        // the driver's own mapping of it is the only other writer, and the
        // protocol gives each byte one writer.
        unsafe { core::ptr::write_volatile(address, value) };
    }

    fn barrier(&self) {
        // The want-bell handshake (BLOCK-RING.md section 5.1) needs every
        // store before this ordered against every load after it, on the
        // processor the driver reads from: a full fence, which is Rust's
        // `fence` and needs no assembly of its own.
        core::sync::atomic::fence(Ordering::SeqCst);
    }
}

/// A request a caller is waiting on.
#[derive(Debug)]
struct Pending {
    /// A write's bytes, until the ring's task copies them into the data
    /// region it dispatches the command through.
    payload: Option<Vec<u8>>,
    /// What came of it, once it has: a read's bytes, or nothing for a write
    /// or a flush.
    result: Option<Result<Vec<u8>, Errno>>,
    /// Whether its caller stopped waiting; the answer is then thrown away.
    abandoned: bool,
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
}

/// A block device served by a ring-3 driver through a ring.
pub(crate) struct RingDisk {
    /// The device, as HELLO described it.
    device: Device,
    /// Reads, shared with the ring's task.
    state: SpinLock<DiskState>,
    /// Woken when a read finishes or the ring ends.
    done: WaitQueue,
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
            }),
            done: WaitQueue::new(),
            port: SpinLock::new(port),
        }
    }

    /// Be served by a new ring, which `port` is the completion port of.
    fn take_up(&self, port: Arc<Port>) {
        *self.port.lock() = port;
        self.state.lock().parked_at = None;
    }

    /// The disk is over: fail every outstanding request and wake every
    /// reader.
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
        self.done.wake_all();
    }

    /// Read `count` sectors from `sector` into `out`, which is exactly that
    /// long, as one request.
    fn read_request(&self, sector: u64, count: u32, out: &mut [u8]) -> Result<(), Errno> {
        let bytes = self.request(Request::read(0, sector, count), None)?;
        if bytes.len() != out.len() {
            return Err(Errno::EIO);
        }
        out.copy_from_slice(&bytes);
        Ok(())
    }

    /// Write `payload` — `count` whole sectors — at `sector`, as one request.
    fn write_request(&self, sector: u64, count: u32, payload: Vec<u8>) -> Result<(), Errno> {
        let _ = self.request(Request::write(0, sector, count).sync(), Some(payload))?;
        Ok(())
    }

    /// Make everything written durable: one flush, which the queue treats as
    /// a barrier no request crosses.
    fn flush_request(&self) -> Result<(), Errno> {
        let _ = self.request(Request::flush(0), None)?;
        Ok(())
    }

    /// Submit one request, wait for it, and hand back what a read brought.
    ///
    /// `payload` is a write's bytes, which the ring's task copies into the
    /// data region when it dispatches the command. The id in `request` is
    /// replaced by the disk's next one.
    fn request(&self, request: Request, payload: Option<Vec<u8>>) -> Result<Vec<u8>, Errno> {
        let id = {
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
                },
            );
            id
        };
        // A full port already holds a nudge the task has not taken.
        let port = Arc::clone(&self.port.lock());
        let _ = port.queue_user(SUBMIT_KEY, [0; 2]);
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
        let _ = self
            .done
            .wait_until_deadline(|| self.finished(id) || terminated(), deadline);
        let mut state = self.state.lock();
        match state.reads.remove(&id) {
            Some(Pending {
                result: Some(result),
                ..
            }) => result,
            Some(Pending { result: None, .. }) => {
                // The wait gave up. Leave a marker so the answer, when it
                // comes, is thrown away rather than kept for an id that will
                // be used again.
                let _ = state.reads.insert(
                    id,
                    Pending {
                        payload: None,
                        result: None,
                        abandoned: true,
                    },
                );
                Err(Errno::EIO)
            }
            None => Err(Errno::EIO),
        }
    }

    /// Whether read `id` has an answer, or the ring is over.
    fn finished(&self, id: u64) -> bool {
        let state = self.state.lock();
        state.ended
            || state
                .reads
                .get(&id)
                .is_none_or(|pending| pending.result.is_some())
    }
}

impl BlockDevice for RingDisk {
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let size = usize::try_from(self.device.block_size()).map_err(|_| Errno::EIO)?;
        if size == 0 || buf.is_empty() || !buf.len().is_multiple_of(size) {
            return Err(Errno::EINVAL);
        }
        let most = usize::try_from(self.device.max_sectors())
            .unwrap_or(usize::MAX)
            .saturating_mul(size);
        let mut at = sector;
        for chunk in buf.chunks_mut(most.max(size)) {
            let count = u32::try_from(chunk.len() / size).map_err(|_| Errno::EIO)?;
            self.read_request(at, count, chunk)?;
            at = at.checked_add(u64::from(count)).ok_or(Errno::EIO)?;
        }
        Ok(())
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
    /// The data VMO's pages.
    data: Pages,
    /// The ring VMO's pages, held while the ring is served.
    _ring_held: Held,
    /// The data VMO's pages, held while the ring is served.
    _data_held: Held,
    /// The disk's node, unpublished when a stopped ring is over and kept
    /// with the disk when its driver died.
    registration: BlockRegistration,
    /// The PCI location the ring serves, which a parked disk waits at.
    location: Location,
    /// The disk's name, which the next driver must give again.
    name: DiskName,
    /// Bytes in one data region.
    region_bytes: u64,
    /// Data regions not in use.
    free: Vec<u32>,
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
            self.dispatch();
            if let Some(bell) = self.side.publish() {
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
            if self.dispatchable() {
                self.side.woke();
                continue;
            }
            self.sleep();
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

    /// Whether a read is queued and a region is free for it.
    fn dispatchable(&self) -> bool {
        !self.free.is_empty() && self.disk.state.lock().queue.queued() > 0
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
            bytes.is_some_and(|bytes| self.data.copy_in(offset + (at - sector) * block, bytes))
        })
    }

    /// Move queued requests onto the ring while regions and ring slots last.
    fn dispatch(&mut self) {
        let mut failed = Vec::new();
        {
            let mut state = self.disk.state.lock();
            let block = u64::from(state.queue.limits().logical_block_size());
            while let Some(&region) = self.free.last() {
                let Some(command) = state.queue.dispatch(ticks()) else {
                    break;
                };
                let (token, op, sector, count) =
                    (command.token, command.op, command.sector, command.count);
                // Copied out so the queue is no longer borrowed while the
                // payloads, which live beside it, are read.
                let parts: Vec<(u64, u64)> = command
                    .parts
                    .iter()
                    .map(|part| (part.id.0, part.sector))
                    .collect();
                let offset = u64::from(region) * self.region_bytes;
                let submission = match op {
                    Op::Flush => Submission::flush(token.raw()),
                    Op::Write if !self.fill(&state, &parts, sector, offset, block) => {
                        failed.push(token);
                        continue;
                    }
                    Op::Write => Submission::write(token.raw(), sector, count, offset),
                    _ => Submission::read(token.raw(), sector, count, offset),
                };
                match self.side.submit(submission) {
                    Ok(()) => {
                        let _ = self.free.pop();
                        let _ = self.flying.insert(token.raw(), region);
                        crate::fs::seam::submitted();
                    }
                    Err(SubmitError::Full) => {
                        let _ = state.queue.requeue(token);
                        break;
                    }
                    Err(_) => failed.push(token),
                }
            }
            for token in failed {
                answer(&mut state, token, Err(Errno::EIO), &Pages::empty(), 0);
            }
        }
        self.disk.done.wake_all();
    }

    /// Take every completion off the ring and answer its reads.
    fn complete(&mut self) -> Result<(), ferrix_blkring::Corruption> {
        let mut answered = false;
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
            let offset = u64::from(region) * self.region_bytes;
            answer(
                &mut self.disk.state.lock(),
                Token::from_raw(token),
                result,
                &self.data,
                offset,
            );
            self.free.push(region);
            answered = true;
        }
        if answered {
            self.disk.done.wake_all();
        }
        Ok(())
    }

    /// Sleep on the completion port until something arrives, or a while.
    ///
    /// Runs in the ring's own kernel thread, never in a process, so the wait
    /// need not watch for a terminated process.
    fn sleep(&mut self) {
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
        while let Some(packet) = port.take() {
            if packet.key == CONTROL_KEY && packet.kind == PACKET_SIGNAL {
                self.watching = false;
            }
        }
    }

    /// The ring is over. STOPPED ends the disk: every outstanding request
    /// fails and the node goes. A dead driver's disk is parked instead, with
    /// the commands it held put back on the queue for the next ring.
    fn finish(mut self, ending: Ending) {
        let _outstanding = self.side.end(ending).count();
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

impl Pages {
    /// Pages holding nothing.
    const fn empty() -> Pages {
        Pages {
            virts: Vec::new(),
            bytes: 0,
        }
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

/// Finish the command `token` names with `result`, copying each read's bytes
/// out of `data` from `offset` when it succeeded.
fn answer(
    state: &mut DiskState,
    token: Token,
    result: Result<(), Errno>,
    data: &Pages,
    offset: u64,
) {
    let Ok(completion) = state.queue.complete(token, ()) else {
        return;
    };
    let size = u64::from(completion.count.max(1)).max(1);
    let block = completion_block(state, size);
    for part in completion.parts() {
        let Some(pending) = state.reads.get_mut(&part.id.0) else {
            continue;
        };
        let answer = result.and_then(|()| {
            if completion.op != Op::Read {
                // A write or a flush brings nothing back.
                return Ok(Vec::new());
            }
            let start = offset + (part.sector - completion.sector) * block;
            let len = usize::try_from(u64::from(part.count) * block).map_err(|_| Errno::EIO)?;
            let mut bytes = vec![0; len];
            if data.copy_out(start, &mut bytes) {
                Ok(bytes)
            } else {
                Err(Errno::EIO)
            }
        });
        if pending.abandoned {
            let _ = state.reads.remove(&part.id.0);
        } else {
            pending.result = Some(answer);
        }
    }
}

/// Bytes in one of the queue's sectors.
fn completion_block(state: &DiskState, _count: u64) -> u64 {
    u64::from(state.queue.limits().logical_block_size())
}
