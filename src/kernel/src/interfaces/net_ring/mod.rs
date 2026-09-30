//! The kernel's end of a net ring.
//!
//! `docs/NET-RING.md` is the protocol and `ferrix-netring` checks every word
//! of it. This is the glue that crate leaves to the kernel (its §8). A process
//! holding a device with `MANAGE` asks for a ring with `NET_RING_CREATE` and
//! hands the channel end it gets to a network driver. One kernel task per ring
//! then:
//!
//! 1. waits for the driver's HELLO and checks it, answering REFUSED with the
//!    first failure in §7's order;
//! 2. holds the ring and data VMOs, attaches [`KernelSide`] over the ring,
//!    adds the interface to the net core, and answers READY with its
//!    completion port;
//! 3. serves it: it posts every free slot for the driver to fill, takes frames
//!    the net core wants sent and puts them in slots, drains completions,
//!    hands what arrived up to the stack, and rings the driver when the driver
//!    asked to be rung;
//! 4. ends when the control channel closes, the driver says STOPPED or the
//!    ring is corrupt, taking the interface's carrier down.
//!
//! # An interface outlives its driver
//!
//! A driver that dies leaves its interface in the net core, parked: no
//! carrier, nothing queued for it, but its index, name, addresses and routes
//! kept. The next ring made for the same device and naming the same interface
//! takes it up again, so a driver `devmgr` starts again brings back `eth0` as
//! it was, and a socket bound to its address, a route through it and whatever
//! configured it see only the carrier go and come back, as when a cable is
//! pulled. A device whose driver never comes back leaves its interface down
//! in the same way. A ring's claim of its device is `crate::claim`'s, so a
//! quiesce waits for a dead driver's ring to end before the device is handed
//! to the next one.
//!
//! # The kernel never drives a device
//!
//! It puts bytes in slots and takes bytes out of them. Whatever puts a frame
//! on a wire is the process at the other end of the ring.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicUsize, Ordering};

use ferrix_blkring::identity::Location;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, PACKET_SIGNAL};
use ferrix_net::iface::{IFF_BROADCAST, IFF_MULTICAST, IFF_UP, Interface as NetInterface};
use ferrix_netring::control::{HELLO_RIGHTS, MAX_MESSAGE, READY_RIGHTS};
use ferrix_netring::kernel::{Completed, KernelSide, SubmitError};
use ferrix_netring::{Hello, Message, Op, Refusal, RingMemory, Status, Wait};

use crate::claim::{Claims, StillServed};
use crate::device::DeviceNode;
use crate::hooks::Full;
use crate::mm;
use crate::net;
use crate::object::channel::{ChannelMessage, Endpoint, ReadError};
use crate::object::port::{Observer, Port};
use crate::object::process::Host;
use crate::object::{Object, Transfer};
use crate::sched::{self, Task};
use crate::sync::SpinLock;
use crate::syscall::native;
use crate::timer;
use crate::user::vmo::{Held, Vmo};

pub(crate) mod check;

/// How long a ring waits for its driver's HELLO.
const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How long the ring's task sleeps before looking again of its own accord.
const RECHECK_NANOS: u64 = 20_000_000;

/// The completion port's key for the control channel's signals. Keys 1 and 2
/// are the ring's own bells, and 4 is the net core saying frames are waiting
/// ([`net::TRANSMIT_KEY`]).
const CONTROL_KEY: u64 = 3;

/// How many completions one drain takes at a time.
const DRAIN_AT_ONCE: usize = 32;

/// Why a ring could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CreateError {
    /// The device already has a ring.
    InUse,
    /// No memory for the channel, or no stack for the ring's task.
    NoMemory,
}

/// A ring waiting for its task to pick it up.
#[derive(Debug)]
struct Start {
    /// The ring's number.
    id: usize,
    /// The kernel's end of the control channel.
    control: Arc<Endpoint>,
    /// The device, held so it is not given away while a ring serves it.
    device: Arc<DeviceNode>,
    /// Its PCI location, when it has one, for the PUBLISHED that tells
    /// `devmgr` the driver did its job. A device that is not a PCI function
    /// -- the boot check's -- has none, and `devmgr` never started it.
    location: Option<Location>,
}

/// Rings whose task has not started yet.
static STARTING: SpinLock<Vec<Start>> = SpinLock::new(Vec::new());

/// Devices that have a ring, each claimed through its ring's control channel,
/// which a quiesce waits out.
static CLAIMS: Claims = Claims::new();

/// Which device node each device a ring added is served from, by the device's
/// key (`net::device`): what sysfs shows the interface inside, and which
/// parked interface a ring made for that device again takes up. The net core
/// knows interfaces and not devices, and this is where the two meet. The key,
/// and not the interface's index, because the interface may have moved to
/// another network namespace, where it has another index
/// (`docs/NETNS.md` section 3.2).
static PLACED: SpinLock<Vec<(u32, usize)>> = SpinLock::new(Vec::new());

/// The device node the interface with index `interface` in the reader's
/// network namespace is served from, by its index in `device::devices()`;
/// `None` for one no ring added, the loopback interface among them.
pub(crate) fn node_of(interface: u32) -> Option<usize> {
    let key = net::acting()
        .core()
        .look(|stack| match stack.interface(interface)?.backing {
            ferrix_net::iface::Backing::Device(key) => Some(key),
            _ => None,
        })?;
    PLACED
        .lock()
        .iter()
        .find(|(placed, _)| *placed == key)
        .map(|(_, node)| *node)
}

/// Take the device `key`'s interface out of the net core, wherever it is, and
/// forget where it was.
fn forget(key: u32) {
    PLACED.lock().retain(|(placed, _)| *placed != key);
    net::device::remove(key);
}

/// The device a ring for device `node` added before, parked or not.
fn placed_on(node: usize) -> Option<u32> {
    PLACED
        .lock()
        .iter()
        .find(|(_, placed)| *placed == node)
        .map(|(key, _)| *key)
}

/// Take the interface a dead ring left on `node` out of the net core: for a
/// check that made a ring on a device nobody will serve again.
pub(crate) fn forget_device(node: &Arc<DeviceNode>) {
    if let Some(key) = placed_on(node.index()) {
        forget(key);
    }
}

/// Every ring's task by its ring's number, so a check that ended one ring
/// can wait for that ring's task and no other: a machine with a real network
/// adapter has a driver's ring running for the life of the machine.
static TASKS: SpinLock<Vec<(usize, Arc<Task>)>> = SpinLock::new(Vec::new());

/// The next ring's number.
static NEXT_RING: AtomicUsize = AtomicUsize::new(1);

/// Answer `net_ring_create` with this ring.
///
/// Called once from `main.rs`'s `register_load`: the native ABI is the item's
/// and names no subsystem above it, so this registers into it.
///
/// # Errors
///
/// [`Full`] when the item has no room for the registration.
pub(crate) fn install() -> Result<(), Full> {
    native::serve(NativeCall::NetRingCreate, control_create)?;
    native::register_server(&SERVER)
}

/// What a quiesce waits out for the net ring.
static SERVER: native::Server = native::Server {
    wait_until_unserved,
    release: None,
};

/// Wait until no ring's channel claims `node`, for a quiesce
/// (`crate::claim`).
///
/// # Errors
///
/// [`StillServed`].
fn wait_until_unserved(
    node: &Arc<DeviceNode>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), StillServed> {
    CLAIMS.wait_until_released(node, cancelled)
}

/// `net_ring_create`.
///
/// The same shape as `block_ring_create` and for the same reason: a ring is
/// made for a device the caller holds with `MANAGE`, and the driver's end of
/// its control channel comes back as a handle. The device handle and its `MANAGE` right are the item's to
/// check (`native::control_channel`).
fn control_create(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let device = registers.first().copied().unwrap_or(0);
    native::control_channel(
        caller,
        device,
        ferrix_netring::control::CONTROL_RIGHTS,
        |node| {
            create(node)
                .map(|(_, driver_end)| driver_end)
                .map_err(|why| match why {
                    CreateError::InUse => status::ALREADY_BOUND,
                    CreateError::NoMemory => status::NO_MEMORY,
                })
        },
    )
}

/// Make a ring for `node` and start its task; answer the driver's end of its
/// control channel.
///
/// # Errors
///
/// [`CreateError`].
pub(crate) fn create(node: &Arc<DeviceNode>) -> Result<(usize, Arc<Endpoint>), CreateError> {
    let (kernel_end, driver_end) = Endpoint::pair().map_err(|_| CreateError::NoMemory)?;
    let id = NEXT_RING.fetch_add(1, Ordering::Relaxed);
    if !CLAIMS.claim(node, &kernel_end) {
        return Err(CreateError::InUse);
    }
    STARTING.lock().push(Start {
        id,
        control: kernel_end,
        device: Arc::clone(node),
        location: crate::interfaces::block_ring::location_of(node),
    });
    match sched::spawn("net ring", run, id, ferrix_sched::NICE_0_WEIGHT) {
        Ok(task) => {
            let mut tasks = TASKS.lock();
            tasks.retain(|(_, task)| !task.is_dead());
            if tasks.capacity() == 0 {
                tasks.reserve(8);
            }
            tasks.push((id, task));
        }
        Err(_) => {
            let _ = take_start(id);
            CLAIMS.release(node);
            return Err(CreateError::NoMemory);
        }
    }
    Ok((id, driver_end))
}

/// Take the start a task was spawned for.
fn take_start(id: usize) -> Option<Start> {
    let mut starting = STARTING.lock();
    let at = starting.iter().position(|start| start.id == id)?;
    Some(starting.remove(at))
}

/// Wait until ring `id`'s task has stopped, or `deadline` passes. A ring
/// whose task is already forgotten counts as stopped.
///
/// # Errors
///
/// That ring's task still running at the deadline.
pub(crate) fn wait_until_task_stopped(id: usize, deadline: u64) -> Result<(), &'static str> {
    loop {
        sched::sleep_for(1_000_000);
        let mut tasks = TASKS.lock();
        let stopped = tasks
            .iter()
            .find(|(ring, _)| *ring == id)
            .is_none_or(|(_, task)| task.is_dead());
        if stopped {
            tasks.retain(|(ring, task)| *ring != id && !task.is_dead());
            return Ok(());
        }
        drop(tasks);
        if timer::now_nanos() >= deadline {
            return Err("a net ring's task kept running after its ring ended");
        }
    }
}

/// One ring's task.
fn run(id: usize) {
    let Some(start) = take_start(id) else {
        return;
    };
    // Served, refused or never begun, the device is free for another ring,
    // and a quiesce waiting for it is woken.
    let _ = serve_ring(&start);
    CLAIMS.release(&start.device);
}

/// Wait for HELLO, take the ring up, serve it, and end.
///
/// `None` when the ring never began.
fn serve_ring(start: &Start) -> Option<()> {
    let message = receive_hello(&start.control)?;
    let mut serving = match take_up(start, &message) {
        Ok(serving) => serving,
        Err(refusal) => {
            refuse(&start.control, refusal);
            return None;
        }
    };
    serving.serve();
    serving.finish();
    Some(())
}

/// The first message on the control channel, or `None` if the driver closed it
/// or said nothing in time.
fn receive_hello(control: &Endpoint) -> Option<ChannelMessage> {
    let deadline = timer::now_nanos().saturating_add(HELLO_PATIENCE_NANOS);
    loop {
        match control.read(CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => return Some(message),
            Err(ReadError::Empty) => {}
            Err(_) => return None,
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

/// Tell the driver why not, and close this end.
fn refuse(control: &Endpoint, refusal: Refusal) {
    let mut bytes = [0_u8; MAX_MESSAGE];
    if let Ok(written) = Message::Refused(refusal).encode(&mut bytes) {
        let body = bytes.get(..written).unwrap_or_default().to_vec();
        let _ = control.write(body, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
    }
}

/// What a HELLO offered, once it has been believed.
#[derive(Debug)]
struct Accepted {
    /// What the driver says its interface is.
    hello: Hello,
    /// The ring VMO.
    ring: Arc<Vmo>,
    /// The data VMO.
    data: Arc<Vmo>,
    /// The port the kernel rings to say there are submissions.
    driver_port: Arc<Port>,
}

/// Read a HELLO and its handles, believing nothing.
fn decode_hello(message: &ChannelMessage) -> Result<Accepted, Refusal> {
    let hello = match Message::decode(&message.bytes) {
        Ok(Message::Hello(hello)) => hello,
        Ok(_) => return Err(Refusal::Malformed),
        Err(_) => return Err(Refusal::Malformed),
    };
    hello.validate()?;
    let (
        Some((Object::Vmo(ring), ring_rights)),
        Some((Object::Vmo(data), data_rights)),
        Some((Object::Port(driver_port), port_rights)),
    ) = (
        message.handles.first(),
        message.handles.get(1),
        message.handles.get(2),
    )
    else {
        return Err(Refusal::Handles);
    };
    // Exactly, not at least: a `DUPLICATE` on a VMO would let the kernel's
    // handle be copied, and a missing right would fail later and further away.
    let [ring_wanted, data_wanted, port_wanted] = HELLO_RIGHTS;
    if *ring_rights != ring_wanted || *data_rights != data_wanted {
        return Err(Refusal::Handles);
    }
    if *port_rights != port_wanted {
        return Err(Refusal::Handles);
    }
    Ok(Accepted {
        hello,
        ring: Arc::clone(ring),
        data: Arc::clone(data),
        driver_port: Arc::clone(driver_port),
    })
}

/// Take up the ring a HELLO describes, add its interface and answer READY.
fn take_up(start: &Start, message: &ChannelMessage) -> Result<Serving, Refusal> {
    let accepted = decode_hello(message);
    crate::object::dispose(
        message
            .handles
            .iter()
            .map(|(object, _)| object.clone())
            .skip(usize::MAX),
    );
    let accepted = accepted?;
    let ring_held = accepted
        .ring
        .hold(0, accepted.ring.len_pages())
        .map_err(|_| Refusal::RingTooSmall)?;
    let data_held = accepted
        .data
        .hold(0, accepted.data.len_pages())
        .map_err(|_| Refusal::DataTooSmall)?;
    let ring_memory = Pages::over(&ring_held, accepted.ring.len_bytes());
    let data_bytes = usize::try_from(accepted.data.len_bytes()).unwrap_or(0);
    let side =
        KernelSide::attach(&ring_memory, ring_bytes(&accepted), data_bytes).map_err(|error| {
            match error {
                ferrix_netring::AttachError::Header(_) => Refusal::RingTooSmall,
                ferrix_netring::AttachError::DataTooSmall => Refusal::DataTooSmall,
            }
        })?;

    let Ok(kernel_port) = Port::new() else {
        // The wire protocol has no refusal for memory; a malformed start is
        // the nearest it has.
        return Err(Refusal::Malformed);
    };
    let key = add_or_take_up(start, &accepted.hello)?;
    // The driver reset the device before it sent HELLO, so what a dead one's
    // pins kept from the allocator can go back (`object::pin`'s quarantine).
    crate::object::pin::quarantine_release(&start.device);
    net::device::wake_on_transmit(key, &kernel_port);
    net::device::set_carrier(key, accepted.hello.interface.flags.carrier);
    let ready = {
        let mut bytes = [0_u8; MAX_MESSAGE];
        let written = Message::Ready
            .encode(&mut bytes)
            .map_err(|_| Refusal::Malformed)?;
        bytes.get(..written).unwrap_or_default().to_vec()
    };
    // Published from before READY goes out, not after: `devmgr` kills a
    // driver that has not published by the time it reports, and the driver
    // may act on READY the instant it is sent.
    if let Some(location) = start.location {
        crate::discovery::devmgr::published(location);
    }
    let [ready_rights] = READY_RIGHTS;
    let handed = (Object::Port(Arc::clone(&kernel_port)), ready_rights);
    if start
        .control
        .write(ready, 1, || Ok::<Vec<Transfer>, Infallible>(vec![handed]))
        .is_err()
    {
        net::device::park(key);
        return Err(Refusal::Malformed);
    }
    Ok(Serving {
        side,
        ring: ring_memory,
        data: Pages::over(&data_held, accepted.data.len_bytes()),
        _ring_held: ring_held,
        _data_held: data_held,
        control: Arc::clone(&start.control),
        kernel_port,
        driver_port: accepted.driver_port,
        interface: key,
        mtu: accepted.hello.interface.mtu,
        watching: false,
        received: 0,
        sent: 0,
    })
}

/// The device a HELLO names, its interface added to the first network
/// namespace or, when a ring for this device left it parked with the same
/// name and hardware address, that one taken up again with its index,
/// addresses and routes, wherever it has been moved to. Answers the device's
/// key.
///
/// A parked interface the HELLO does not match -- another name, another
/// address -- is forgotten first: the device is serving something else now.
fn add_or_take_up(start: &Start, hello: &Hello) -> Result<u32, Refusal> {
    let core = net::core();
    let name = hello.interface.name();
    let mac = hello.interface.mac;
    if let Some(parked) = placed_on(start.device.index()) {
        let same = net::device::look(parked, |stack, index| {
            stack
                .interface(index)
                .is_some_and(|old| old.name.as_bytes() == name && old.hardware == mac)
        });
        if same == Some(true) {
            return Ok(parked);
        }
        forget(parked);
    }
    if core.look(|stack| stack.interface_by_name(name).is_some()) {
        return Err(Refusal::NameInUse);
    }
    let flags = hello.interface.flags;
    let mut interface = NetInterface::ethernet(0, name, mac, hello.interface.mtu);
    interface.flags = IFF_UP
        | if flags.broadcast { IFF_BROADCAST } else { 0 }
        | if flags.multicast { IFF_MULTICAST } else { 0 };
    let key = net::device::new_key();
    interface.backing = ferrix_net::iface::Backing::Device(key);
    let index = core.add_interface(interface);
    net::device::place(key, net::first(), index);
    PLACED.lock().push((key, start.device.index()));
    Ok(key)
}

/// How many bytes of the ring VMO the header may use.
fn ring_bytes(accepted: &Accepted) -> usize {
    usize::try_from(accepted.ring.len_bytes()).unwrap_or(0)
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
            // SAFETY: `address` is inside a frame the ring's `Held` keeps for
            // as long as this `Pages` is used, reached through the direct map.
            // The driver may write the byte at any moment, so it is read
            // volatilely and whatever it holds is taken as data.
            *byte = unsafe { core::ptr::read_volatile(address) };
        }
        true
    }

    /// Copy `data` to `offset`; `false`, partly written, if it runs past the
    /// end.
    fn copy_in(&mut self, offset: u64, data: &[u8]) -> bool {
        for (at, byte) in (offset..).zip(data.iter()) {
            let Some(address) = self.address(at) else {
                return false;
            };
            // SAFETY: as in `Pages::copy_out`; the driver's own mapping is the
            // only other writer, and the protocol gives each slot one writer
            // at a time -- whoever holds it.
            unsafe { core::ptr::write_volatile(address, *byte) };
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
        // SAFETY: as in `Pages::copy_in`.
        unsafe { core::ptr::write_volatile(address, value) };
    }

    fn barrier(&self) {
        // The want-bell handshake (NET-RING.md section 6) needs every store
        // before this ordered against every load after it, on the processor
        // the driver reads from: a full fence, which is Rust's `fence` and
        // needs no assembly of its own.
        core::sync::atomic::fence(Ordering::SeqCst);
    }
}

/// A ring being served.
#[derive(Debug)]
struct Serving {
    /// The kernel's end of the ring.
    side: KernelSide,
    /// The ring VMO.
    ring: Pages,
    /// The data VMO, where the slots are.
    data: Pages,
    /// Held so the frames stay while this serves.
    _ring_held: Held,
    /// Likewise.
    _data_held: Held,
    /// The control channel.
    control: Arc<Endpoint>,
    /// Where the driver rings to say there are completions.
    kernel_port: Arc<Port>,
    /// Where this rings to say there are submissions.
    driver_port: Arc<Port>,
    /// Which device of the net core this is: its key, which an interface
    /// keeps across a move to another network namespace.
    interface: u32,
    /// The largest frame it carries.
    mtu: u32,
    /// Whether the control channel's signals are being watched on the port.
    watching: bool,
    /// How many frames came in.
    received: u64,
    /// How many went out.
    sent: u64,
}

impl Serving {
    /// Serve until the ring ends.
    fn serve(&mut self) {
        loop {
            self.post_receives();
            self.take_transmits();
            if let Some(bell) = self.side.publish(&mut self.ring) {
                let _ = self
                    .driver_port
                    .queue_user(bell.key(), [u64::from(bell.tail()), 0]);
            }
            if !self.drain() {
                return;
            }
            if self.control_closed() {
                return;
            }
            self.rest();
        }
    }

    /// Give the driver slots to fill, so a frame that arrives has somewhere to
    /// land.
    ///
    /// A receive queue with no buffers drops every packet silently, which is
    /// the classic way a network driver looks alive and carries nothing. But
    /// posting *every* free slot is the other half of that mistake: it leaves
    /// none to put an outgoing frame in, and the interface receives for ever
    /// and never answers. Half the ring each, which the first end-to-end check
    /// of this path found the hard way.
    fn post_receives(&mut self) {
        let half = self.side.layout().entries() / 2;
        while self.side.outstanding() < half {
            let Some(slot) = self.side.free_slot() else {
                return;
            };
            match self.side.submit(&mut self.ring, slot, Op::Receive, 0) {
                Ok(()) => {}
                Err(SubmitError::Full) => return,
                Err(_) => return,
            }
        }
    }

    /// Take what the net core wants sent and put it in slots.
    fn take_transmits(&mut self) {
        // Half the ring at most, so a burst of transmissions cannot leave the
        // receive side with nothing posted.
        let room = (self.side.layout().entries() / 2) as usize;
        let frames = net::device::take_outgoing(self.interface, room);
        for frame in frames {
            if frame.len() > self.side.layout().slot_bytes() as usize {
                continue;
            }
            let Some(slot) = self.side.free_slot() else {
                return;
            };
            let Ok(offset) = self.side.layout().slot_offset(slot) else {
                return;
            };
            if !self.data.copy_in(offset as u64, &frame) {
                return;
            }
            let length = u32::try_from(frame.len()).unwrap_or(0);
            if self
                .side
                .submit(&mut self.ring, slot, Op::Transmit, length)
                .is_err()
            {
                return;
            }
            self.sent += 1;
        }
    }

    /// Take what the driver answered. `false` when the ring is over.
    fn drain(&mut self) -> bool {
        let mut done = [Completed {
            slot: 0,
            op: Op::Transmit,
            length: 0,
            status: Status::Ok,
        }; DRAIN_AT_ONCE];
        loop {
            let Ok(drained) = self.side.drain(&mut self.ring, &mut done) else {
                return false;
            };
            for entry in done.iter().take(drained.taken) {
                self.deliver(entry);
            }
            if !drained.more {
                return true;
            }
        }
    }

    /// Hand one completed slot to the net core, or let it go.
    fn deliver(&mut self, entry: &Completed) {
        if entry.op != Op::Receive || entry.status != Status::Ok || entry.length == 0 {
            return;
        }
        let Ok(offset) = self.side.layout().slot_offset(entry.slot) else {
            return;
        };
        let length = (entry.length as usize).min(self.mtu as usize + 18);
        let mut frame = vec![0_u8; length];
        if !self.data.copy_out(offset as u64, &mut frame) {
            return;
        }
        self.received += 1;
        net::device::receive(self.interface, &frame);
    }

    /// Whether the driver has closed its end.
    fn control_closed(&mut self) -> bool {
        if self.control.signals().intersects(Signals::PEER_CLOSED) {
            return true;
        }
        match self
            .control
            .read(CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, false)
        {
            Ok(message) => {
                crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
                match Message::decode(&message.bytes) {
                    Ok(Message::Stopped) => true,
                    Ok(Message::Link(up)) => {
                        net::device::set_carrier(self.interface, up);
                        false
                    }
                    _ => false,
                }
            }
            Err(ReadError::Empty) => false,
            Err(_) => true,
        }
    }

    /// Sleep until the driver rings, the control channel says something, or
    /// the recheck passes.
    fn rest(&mut self) {
        if !self.watching {
            let observer = Observer::new(
                &self.kernel_port,
                CONTROL_KEY,
                Signals::READABLE | Signals::PEER_CLOSED,
            );
            self.watching = observer.is_ok_and(|observer| self.control.observe(observer).is_ok());
        }
        if !matches!(self.side.prepare_to_sleep(&mut self.ring), Ok(Wait::Sleep)) {
            return;
        }
        let deadline = timer::now_nanos().saturating_add(RECHECK_NANOS);
        let _ = self
            .kernel_port
            .waiters()
            .wait_until_deadline(|| !self.kernel_port.is_empty(), deadline);
        while self.kernel_port.take().is_some() {}
        self.side.woke(&mut self.ring);
    }

    /// End the ring: park the interface for the next driver and give up
    /// every slot.
    fn finish(&mut self) {
        let abandoned = self.side.abandon();
        net::device::park(self.interface);
        let _ = abandoned;
        let _ = PACKET_SIGNAL;
    }
}
