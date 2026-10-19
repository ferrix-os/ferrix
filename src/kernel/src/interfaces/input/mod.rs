//! The input core: the kernel's end of an input driver's control channel, and
//! the `/dev/input/event<N>` nodes it publishes.
//!
//! `docs/INPUT.md` §3.1 and §3.2 are the design and `ferrix-inputctl` is
//! every word of the protocol and of the queues. A process holding a device
//! with `MANAGE` asks for a control channel with `INPUT_CONTROL_CREATE` and
//! hands its end to an input driver. One kernel task per device then:
//!
//! 1. waits for HELLO and checks it through [`Session::accept`] and the
//!    device's location, answering REFUSED with the first failure;
//! 2. publishes `/dev/input/event<N>` and answers READY with the node's
//!    number and a port;
//! 3. serves: every EVENTS the driver sends goes through the session, which
//!    refuses an event the device did not declare, keeps the device's state
//!    as Linux's input core does, and hands back whole reports; each report
//!    is appended to every open's queue, or to the grabbing open's alone;
//! 4. ends when the driver closes its end, says STOPPED, or breaks the
//!    protocol, taking the node away and revoking every open.
//!
//! A device's claim is `crate::claim`'s, so a quiesce waits for a dead
//! driver's tasks to end before the device goes to the next driver, and
//! event numbers are the lowest free, so a keyboard whose driver `devmgr`
//! starts again comes back as the `event<N>` it was.
//!
//! # The kernel invents no events
//!
//! Not even a repeat: `docs/INPUT.md` §3.1 is a written deviation from Linux,
//! which repeats a held key for a device declaring `EV_REP`. Compositors
//! repeat keys themselves, so the core stores the delay and period for
//! `EVIOCGREP` and makes nothing of them.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicUsize, Ordering};

use ferrix_blkring::identity::Location;
use ferrix_inputctl::message::{
    Events, Hello, MAX_BYTES, MAX_EVENTS, Message, RawEvent, Ready, Refusal,
};
use ferrix_inputctl::queue::{Clock, Clocks, Queue, Stamped};
use ferrix_inputctl::session::{OpenId, Received, Session};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{CHANNEL_MAX_HANDLES, DEVICE_NOT_PCI};

use crate::claim::{Claims, Numbers, StillServed};
use crate::device::DeviceNode;
use crate::hooks::Full;
use crate::object::channel::{ChannelMessage, Endpoint, ReadError, Sender};
use crate::object::port::Port;
use crate::object::process::Host;
use crate::object::{Object, Transfer};
use crate::sched;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::native;
use crate::timer;

pub(crate) mod evdev;

/// Linux's major number for `/dev/input/event*`, from `INPUT_MAJOR`.
pub(crate) const INPUT_MAJOR: u32 = 13;

/// The minor `event0` takes, from Linux's `EVDEV_MINOR_BASE`.
pub(crate) const EVDEV_MINOR_BASE: u32 = 64;

/// How long the core waits for its driver's HELLO.
const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How long the task sleeps before looking at the channel of its own accord.
const RECHECK_NANOS: u64 = 50_000_000;

/// The word a device tree node's HELLO carries for its location, which is
/// what `device_info` says for it, as the display core's `TREE_LOCATION`.
const TREE_LOCATION: Location = Location(DEVICE_NOT_PCI);

/// Why a control channel could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CreateError {
    /// The device already has as many as it has input functions.
    InUse,
    /// No memory for the channel, or no stack for the task.
    NoMemory,
}

/// A control channel waiting for its task.
#[derive(Debug)]
struct Start {
    id: usize,
    control: Arc<Endpoint>,
    device: Arc<DeviceNode>,
    location: Option<Location>,
    /// Whether devmgr waits for this device's PUBLISHED. Not for a tree
    /// node's: every tree node shares [`TREE_LOCATION`], so one's PUBLISHED
    /// could end devmgr's wait for another. And not for a bus host's, tree
    /// node or PCI function (a node with more than one input function): its
    /// devices come and go long after devmgr stopped waiting for anything.
    announce: bool,
}

static STARTING: SpinLock<Vec<Start>> = SpinLock::new(Vec::new());
/// The devices a driver's channel claims, as many per device as it has input
/// functions, which a quiesce waits out (`crate::claim`).
static CLAIMS: Claims = Claims::new();
static DEVICES: SpinLock<Vec<Arc<InputDevice>>> = SpinLock::new(Vec::new());
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
/// The `event<N>` numbers: a device whose driver came back is `event0`
/// again, not `event1` after a hole.
static NUMBERS: Numbers = Numbers::new(0);
static NEXT_OPEN: AtomicUsize = AtomicUsize::new(1);

/// One open of an `event<N>` node: its queue, and who it is for the grab.
pub(crate) struct Open {
    /// Which open, for `EVIOCGRAB`.
    pub(crate) id: OpenId,
    /// Its events. A `Vec` of the size `queue_size` gave: Linux's rule for
    /// this device's declaration, with more packets than Linux keeps
    /// (`ferrix_inputctl::queue::BUFFER_PACKETS` says why).
    pub(crate) queue: SpinLock<Queue<Vec<Stamped>>>,
}

/// A published input device.
pub(crate) struct InputDevice {
    /// `event<index>`.
    pub(crate) index: u32,
    /// The device node the device is served from, by its index in
    /// `device::devices()`: where sysfs shows it. A USB host serves several.
    pub(crate) node: usize,
    control: Arc<Endpoint>,
    /// The session: the device's declaration and its state, which every
    /// `EVIOC*` reads.
    ///
    /// Boxed. A `Session` holds every bitmap the device declared and a whole
    /// report's worth of events -- several kilobytes -- and a kernel task
    /// has four pages of stack (`vmap::STACK_PAGES`). Built on the stack
    /// and moved into an `Arc` it overflowed that stack, and a kernel stack
    /// that overflows is a double fault.
    pub(crate) session: SpinLock<Box<Session>>,
    /// Every open of the node.
    pub(crate) opens: SpinLock<Vec<Arc<Open>>>,
    /// Woken whenever a report is queued or the device goes.
    pub(crate) changed: Arc<WaitQueue>,
    /// Set when the driver is gone: every read is then `ENODEV`.
    pub(crate) gone: SpinLock<bool>,
    /// How many wake-ups the wait queue has had, which `epoll`'s
    /// edge-triggered mode counts.
    pub(crate) wakes: AtomicUsize,
}

impl InputDevice {
    /// The clocks a read converts a report's stamp with.
    ///
    /// A report is stamped once from the monotonic clock, and a read shifts
    /// it into whichever clock the open chose. `CLOCK_BOOTTIME` and
    /// `CLOCK_MONOTONIC` are the same here: Ferrix does not suspend, so no
    /// time passes that the monotonic clock did not count.
    pub(crate) fn clocks() -> Clocks {
        Clocks {
            realtime_offset: crate::syscall::time::realtime_offset(),
            boottime_offset: 0,
        }
    }

    /// Whether the driver has gone.
    pub(crate) fn is_gone(&self) -> bool {
        *self.gone.lock()
    }

    /// Wake everything waiting on this device.
    fn wake(&self) {
        let _ = self.wakes.fetch_add(1, Ordering::Relaxed);
        self.changed.wake_all();
    }

    /// Add an open, with a queue of the size this device's declaration gives.
    pub(crate) fn open(self: &Arc<Self>) -> Option<Arc<Open>> {
        let size = self.session.lock().capabilities().queue_size();
        let mut queue = Queue::new(vec![Stamped::default(); size]).ok()?;
        queue.set_clock(Clock::Realtime, timer::now_nanos());
        let open = Arc::new(Open {
            id: OpenId(NEXT_OPEN.fetch_add(1, Ordering::Relaxed) as u64),
            queue: SpinLock::new(queue),
        });
        self.opens.lock().push(Arc::clone(&open));
        Some(open)
    }

    /// A program's LED events for the device (`evdev`'s `write`): each that
    /// changes the device's state goes to its driver, in STATUS messages,
    /// for it to light the LED. The rest change nothing.
    #[inline(never)]
    pub(crate) fn write_leds(&self, events: &[RawEvent]) {
        let changed: Vec<RawEvent> = {
            let mut session = self.session.lock();
            events
                .iter()
                .filter_map(|event| session.write_led(*event))
                .collect()
        };
        for batch in changed.chunks(MAX_EVENTS) {
            let Some(status) = Events::new(batch) else {
                continue;
            };
            let bytes = Message::Status(status).encode().as_bytes().to_vec();
            let _ = self
                .control
                .write(Sender::KERNEL, bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
        }
    }

    /// Drop an open, releasing its grab if it held one.
    pub(crate) fn close(&self, open: &Arc<Open>) {
        self.session.lock().release(open.id);
        self.opens.lock().retain(|held| !Arc::ptr_eq(held, open));
    }
}

/// The device published as `event<index>`, if one is.
pub(crate) fn device(index: u32) -> Option<Arc<InputDevice>> {
    DEVICES
        .lock()
        .iter()
        .find(|device| device.index == index)
        .map(Arc::clone)
}

/// Every published device's number, in order.
pub(crate) fn device_indices() -> Vec<u32> {
    let mut indices: Vec<u32> = DEVICES.lock().iter().map(|device| device.index).collect();
    indices.sort_unstable();
    indices
}

/// Answer `input_control_create` with input.
///
/// Called once from `main.rs`'s `register_load`: the native ABI is the item's
/// and names no subsystem above it, so this registers into it.
///
/// # Errors
///
/// [`Full`] when the item has no room for the registration.
pub(crate) fn install() -> Result<(), Full> {
    native::serve(NativeCall::InputControlCreate, control_create)?;
    native::register_server(&SERVER)
}

/// What a quiesce waits out for input.
static SERVER: native::Server = native::Server {
    wait_until_unserved,
    release: None,
};

/// Wait until no input driver's channel claims `node`, for a quiesce
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

/// `input_control_create`.
///
/// As the display's: the device's own channel, one per device, and the
/// driver's end of it back. The device handle and its `MANAGE` right are the item's to
/// check (`native::control_channel`).
fn control_create(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let device = registers.first().copied().unwrap_or(0);
    native::control_channel(
        caller,
        device,
        ferrix_blkring::control::CONTROL_RIGHTS,
        |node| {
            create(node).map_err(|why| match why {
                CreateError::InUse => status::ALREADY_BOUND,
                CreateError::NoMemory => status::NO_MEMORY,
            })
        },
    )
}

/// Make a control channel for `node` and start its task.
///
/// # Errors
///
/// [`CreateError::InUse`] when the device already has as many as it has
/// input functions -- one, but for a USB host (an STM32 board's node or a PCI
/// xHCI function), whose driver asks for one per keyboard or mouse it finds
/// -- and [`CreateError::NoMemory`] when the
/// channel or the task could not be made.
pub(crate) fn create(node: &Arc<DeviceNode>) -> Result<Arc<Endpoint>, CreateError> {
    let (kernel_end, driver_end) = Endpoint::pair().map_err(|_| CreateError::NoMemory)?;
    if !CLAIMS.claim_up_to(node, &kernel_end, node.input_functions()) {
        return Err(CreateError::InUse);
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    // A bus host's devices are not announced, wherever the host is: devmgr
    // waits for no PUBLISHED from one, and they come and go for as long as
    // the machine runs.
    let (location, announce) = match node.location() {
        crate::device::Location::Tree(_) => (Some(TREE_LOCATION), false),
        _ => (
            crate::interfaces::block_ring::location_of(node),
            node.input_functions() == 1,
        ),
    };
    STARTING.lock().push(Start {
        id,
        control: Arc::clone(&kernel_end),
        device: Arc::clone(node),
        location,
        announce,
    });
    if sched::spawn("input", run, id, ferrix_sched::NICE_0_WEIGHT).is_err() {
        let _ = take_start(id);
        CLAIMS.release_one(node, &kernel_end);
        return Err(CreateError::NoMemory);
    }
    Ok(driver_end)
}

fn take_start(id: usize) -> Option<Start> {
    let mut starting = STARTING.lock();
    let at = starting.iter().position(|start| start.id == id)?;
    Some(starting.remove(at))
}

/// One device's task.
fn run(id: usize) {
    let Some(start) = take_start(id) else {
        return;
    };
    if let Some(device) = take_up(&start) {
        serve(&device);
        DEVICES.lock().retain(|held| !Arc::ptr_eq(held, &device));
        // Every open now reads `ENODEV`, as Linux's evdev does once its
        // device is gone, and every wait ends.
        for open in device.opens.lock().iter() {
            open.queue.lock().revoke();
        }
        *device.gone.lock() = true;
        device.wake();
        // A program still holding the old device's opens keeps its
        // `InputDevice`, and a lookup of the number finds the next one.
        NUMBERS.give_back(device.index);
        crate::console::println!("  input    event{} is gone", device.index);
    }
    // One of the node's claims: a USB host holds one per device, and one
    // device going leaves the others theirs.
    CLAIMS.release_one(&start.device, &start.control);
}

fn receive(control: &Endpoint, deadline: u64) -> Option<ChannelMessage> {
    loop {
        match control.read(MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
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

#[inline(never)]
fn refuse(control: &Endpoint, refusal: Refusal) {
    let bytes = Message::Refused(refusal).encode().as_bytes().to_vec();
    let _ = control.write(Sender::KERNEL, bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
}

/// Wait for HELLO and publish the device, or refuse it.
#[inline(never)]
fn take_up(start: &Start) -> Option<Arc<InputDevice>> {
    let deadline = timer::now_nanos().saturating_add(HELLO_PATIENCE_NANOS);
    let message = receive(&start.control, deadline)?;
    match accept(start, &message) {
        Ok(device) => Some(device),
        Err(refusal) => {
            crate::console::println!("  input    a driver was refused: {refusal:?}");
            refuse(&start.control, refusal);
            None
        }
    }
}

#[inline(never)]
fn accept(start: &Start, message: &ChannelMessage) -> Result<Arc<InputDevice>, Refusal> {
    let session = judge(start, message)?;
    // The driver reset the device before it sent HELLO, so what a dead one's
    // pins kept from the allocator can go back (`object::pin`'s quarantine),
    // and the kernel's configuration is read back (`DeviceNode::verify_config`).
    start.device.hello_accepted();

    // The protocol has no refusal for memory; a malformed start is the
    // nearest it has, as the net ring's.
    let index = NUMBERS.take().ok_or(Refusal::Malformed)?;
    let left_out = session.capabilities().left_out;
    let name = describe(&session);
    let device = Arc::new(InputDevice {
        index,
        node: start.device.index(),
        control: Arc::clone(&start.control),
        session: SpinLock::new(session),
        opens: SpinLock::new(Vec::new()),
        changed: Arc::new(WaitQueue::new()),
        gone: SpinLock::new(false),
        wakes: AtomicUsize::new(0),
    });

    // Published before READY goes out, as the display does: devmgr kills a
    // driver that has not published by the time it reports.
    DEVICES.lock().push(Arc::clone(&device));
    if start.announce
        && let Some(location) = start.location
    {
        crate::discovery::devmgr::published(location);
    }
    if let Err(refusal) = send_ready(&start.control, index) {
        // Not left published, nor its number held, for a driver that never
        // heard it was.
        DEVICES.lock().retain(|held| !Arc::ptr_eq(held, &device));
        NUMBERS.give_back(index);
        return Err(refusal);
    }

    // The boot line says what the device is and what was left out, which is
    // `docs/INPUT.md` L5's exit.
    if left_out.is_empty() {
        crate::console::println!("  input    event{index} {name}");
    } else {
        crate::console::println!(
            "  input    event{index} {name}, without {}",
            left_out_names(&left_out)
        );
    }
    Ok(device)
}

/// Tell the driver its device is published, and hand it the core's port.
///
/// Its own frame: encoding a message costs a buffer of [`MAX_BYTES`], and
/// see [`judge`] for what a task's stack has room for.
#[inline(never)]
fn send_ready(control: &Endpoint, index: u32) -> Result<(), Refusal> {
    let ready = Message::Ready(Ready { node: index })
        .encode()
        .as_bytes()
        .to_vec();
    // The wire protocol has no refusal for memory; a malformed start is the
    // nearest it has.
    let port = Port::new().map_err(|_| Refusal::Malformed)?;
    let handed = vec![(Object::Port(port), Rights::WRITE)];
    control
        .write(Sender::KERNEL, ready, 1, || Ok::<Vec<Transfer>, Infallible>(handed))
        .map_err(|_| Refusal::Malformed)
}

/// Judge a driver's HELLO, giving the session it earned.
///
/// # Why this is three functions
///
/// A kernel task has four pages of stack: sixteen kilobytes, and a double
/// fault if it runs out. The two values this path handles are large -- a
/// decoded [`Message`] is 1680 bytes, because its `Hello` carries every
/// bitmap a device can declare, and a [`Session`] is 4272, because it
/// carries those bitmaps *and* the state they describe. Written as one
/// function the compiler inlines the lot into the task's entry and adds
/// every temporary up: the message, the session the library returns by
/// value, and the copy of it that goes into the box. That frame is bigger
/// than the stack, and the first push into the guard page is the fault.
///
/// So each large value gets a frame of its own that is gone before the next
/// one starts, and `#[inline(never)]` is what holds them apart. The peak is
/// then one session, not a message and two sessions.
#[inline(never)]
fn judge(start: &Start, message: &ChannelMessage) -> Result<Box<Session>, Refusal> {
    let rights: Vec<Rights> = message.handles.iter().map(|(_, rights)| *rights).collect();
    if !matches!(message.handles.first(), Some((Object::Port(_), _))) {
        return Err(Refusal::Rights);
    }
    let hello = hello_of(&message.bytes)?;
    if start.location.map(Location::raw) != Some(hello.location) {
        return Err(Refusal::WrongLocation);
    }
    session_of(&hello, &rights)
}

/// Decode a HELLO onto the heap. See [`judge`] for why it stands alone.
///
/// The hello is decoded through [`Hello::decode_into`] rather than
/// [`Message::decode`], so that the only whole hello in existence is the one
/// in the box: `decode` would build one, wrap it in a `Message`, and return
/// that, three times this frame's worth on a stack that has four pages.
#[inline(never)]
fn hello_of(bytes: &[u8]) -> Result<Box<Hello>, Refusal> {
    let mut hello = Box::new(Hello::EMPTY);
    Hello::decode_into(bytes, &mut hello).map_err(|_| Refusal::Malformed)?;
    Ok(hello)
}

/// Build a session onto the heap. See [`judge`] for why it stands alone.
#[inline(never)]
fn session_of(hello: &Hello, rights: &[Rights]) -> Result<Box<Session>, Refusal> {
    Session::accept(hello, rights).map(Box::new)
}

/// What a device is, for the boot line: its name and the types it publishes.
fn describe(session: &Session) -> alloc::string::String {
    use alloc::string::String;
    use core::fmt::Write;

    let mut text = String::new();
    let name = session.name();
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        let _ = write!(text, "an unnamed device");
    } else {
        for &byte in bytes {
            // A device's name is the device's word; anything that is not
            // printable ASCII would make a mess of a boot line.
            text.push(if byte.is_ascii_graphic() || byte == b' ' {
                char::from(byte)
            } else {
                '.'
            });
        }
    }
    let caps = session.capabilities();
    let mut first = true;
    for (kind, label) in TYPE_NAMES {
        if !caps.bits.has_type(kind) {
            continue;
        }
        let _ = write!(text, "{} {label}", if first { ":" } else { "," });
        first = false;
    }
    text
}

/// The event types the core publishes, with the names the boot line uses.
const TYPE_NAMES: [(u16, &str); 7] = [
    (ferrix_linux_abi::input::EV_KEY, "keys"),
    (ferrix_linux_abi::input::EV_REL, "relative axes"),
    (ferrix_linux_abi::input::EV_ABS, "absolute axes"),
    (ferrix_linux_abi::input::EV_MSC, "misc"),
    (ferrix_linux_abi::input::EV_SW, "switches"),
    (ferrix_linux_abi::input::EV_LED, "LEDs"),
    (ferrix_linux_abi::input::EV_REP, "repeat"),
];

/// What the core left out of a device's declaration, for the boot line.
fn left_out_names(left_out: &ferrix_inputctl::session::LeftOut) -> alloc::string::String {
    use alloc::string::String;
    use core::fmt::Write;

    let mut text = String::new();
    let mut first = true;
    for kind in 0..u16::try_from(left_out.types.len() * 8).unwrap_or(0) {
        if !ferrix_inputctl::message::bit(&left_out.types, kind) {
            continue;
        }
        let _ = write!(text, "{}EV_{kind:#04x}", if first { "" } else { ", " });
        first = false;
    }
    if left_out.mt_axes {
        let _ = write!(text, "{}multi-touch axes", if first { "" } else { ", " });
    }
    text
}

/// Serve one device until its driver goes.
#[inline(never)]
fn serve(device: &InputDevice) {
    loop {
        if device.is_gone() {
            break;
        }
        let deadline = timer::now_nanos().saturating_add(RECHECK_NANOS);
        let message = match device.control.read(MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => message,
            Err(ReadError::Empty) => {
                if device.control.signals().intersects(Signals::PEER_CLOSED) {
                    break;
                }
                let _ = device.control.waiters().wait_until_deadline(
                    || {
                        device
                            .control
                            .signals()
                            .intersects(Signals::READABLE | Signals::PEER_CLOSED)
                    },
                    deadline,
                );
                continue;
            }
            Err(_) => break,
        };
        crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
        let Ok(decoded) = Message::decode(&message.bytes) else {
            refuse(&device.control, Refusal::Protocol);
            break;
        };
        // The reports a message finished, gathered while the session is
        // locked and delivered after: a queue is another lock, and `deliver`
        // must not be called with the session held.
        let now = timer::now_nanos();
        let mut reports = Vec::new();
        let accepted = device
            .session
            .lock()
            .receive(&decoded, now, |report| reports.push(*report));
        match accepted {
            Ok(Received::Stopped) => break,
            Ok(Received::Events { .. }) => {
                if !reports.is_empty() {
                    deliver(device, &reports);
                }
            }
            Err(refusal) => {
                crate::console::println!("  input    event{}: {refusal:?}", device.index);
                refuse(&device.control, refusal);
                break;
            }
        }
    }
    *device.gone.lock() = true;
    device.wake();
}

/// Append every report to the opens that are to have it.
///
/// A report under a grab goes to the grabbing open alone, which is what
/// `EVIOCGRAB` is for: a compositor grabs a device so a key it takes does not
/// also reach whatever else has the node open.
fn deliver(device: &InputDevice, reports: &[ferrix_inputctl::session::Report]) {
    let opens: Vec<Arc<Open>> = device.opens.lock().clone();
    for report in reports {
        for open in &opens {
            if !report.is_for(open.id) {
                continue;
            }
            open.queue.lock().deliver(report);
        }
    }
    device.wake();
}
