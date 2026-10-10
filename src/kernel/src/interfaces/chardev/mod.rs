//! The chardev core: device files a ring-3 driver serves, forwarded to it
//! undecoded (`docs/NVIDIA.md` §4.4, N1e; the consultant's design verdict,
//! ledger 294).
//!
//! The other cores decode every ioctl in the kernel and send typed messages.
//! NVIDIA's resource manager cannot be served that way: its ioctls carry
//! pointers RM follows itself. So this core passes open, ioctl and release
//! through as they are, and lets the driver reach the waiting program's
//! memory for exactly as long as the program waits.
//!
//! A driver (`nvrm`, started by devmgr with a device whose interrupts are
//! isolated) asks for a control channel with `chardev_control_create`. Its
//! end carries no `TRANSFER` and no `DUPLICATE`, so it stays the driver's.
//! The core's task for the control waits for HELLO, which lists the minors
//! of major 195 the driver serves; `ferrix_chardevctl::session` judges it,
//! and the kernel, not the driver, names each node (`ferrix_chardevctl::node`):
//! `nvidia<N>`, `nvidia-modeset`, `nvidiactl`, mode 0666, root's. The nodes
//! are published in `/dev` and READY goes back.
//!
//! From then on a program's open, ioctl and release on a node become a
//! REQUEST the task writes on the channel. The program's thread sleeps,
//! killably, until the driver answers with `chardev_reply`. Meanwhile the
//! driver may copy from and to the program's memory with `chardev_copy_in`
//! and `chardev_copy_out`, naming the request; once the request is answered
//! or abandoned, its id names nothing, and ids are never reused.
//!
//! # What holds
//!
//! * A copy reaches a program only while that program waits in a request
//!   to this control: abandoning a request marks it dead under its lock and
//!   then waits for any copy in flight, which re-checks between chunks of
//!   4 KiB, so the program waits out at most one chunk (N4).
//! * No copy waits on the driver: no page fault in this kernel is served
//!   by a ring-3 driver (the fault windows of `docs/NVIDIA.md` §11.4 are
//!   not built), so a copy's fault is the kernel's to resolve (N5). When
//!   windows land, these copies take the mode that refuses them.
//! * A REQUEST that finds the channel full waits, killably, for room; a
//!   RELEASE's room is reserved when its file is opened, so it is never
//!   lost and reaches the driver after every request of its file (N8).
//! * When the driver goes, its nodes go: every open file answers `ENODEV`
//!   for good, and every waiter wakes with `ENODEV` (N9).
//!
//! # Taken, not written (`docs/NVIDIA.md` §4.7)
//!
//! A driver that made its control with `CHARDEV_DIRECT` takes its requests
//! with `chardev_next` instead of reading them off the channel, and may
//! answer one in the same call. A driver thread with nothing to take waits
//! in a seat; the program's own thread, admitting a request, hands it to
//! the thread that sat down last and wakes it onto its own processor, as it
//! is about to sleep there. Nothing passes through the control's task,
//! which on such a control only watches the channel for the driver's end.
//! The request carries the first bytes of an ioctl's argument, which the
//! program's thread read from its own memory, and the answer may carry
//! bytes back, which the program's thread writes there before its call
//! returns: neither is a copy into another program's memory, and neither
//! needs a call of its own. What holds above holds unchanged: the table
//! and the queue are the same ones, a seat only takes the place of the
//! queue's head.
//!
//! An mmap is forwarded as a request too (N2; the consultant's ledger 300):
//! the driver answers with a VMO of its own or a range of its device's own
//! memory apertures, and the kernel maps that into the program once the
//! answer is in, never before ([`MapReply`]). An aperture mapping holds the
//! control's claim on the device, so no new driver serves it while a dead
//! one's mapping lives. Poll is never ready; read and write answer `EINVAL`,
//! as NVIDIA's nodes do (N11).

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ferrix_chardevctl::message::{self, INLINE_MAX, Message, Op, Request as Wire};
use ferrix_chardevctl::node::{MAJOR, RENDER_MINOR};
use ferrix_chardevctl::session::{self, Publication, Refusal};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;

use crate::claim::{Claims, StillServed};
use crate::device::DeviceNode;
use crate::hooks::Full;
use crate::object::Object;
use crate::object::channel::{ChannelMessage, Endpoint, ReadError};
use crate::object::process::Host;
use crate::sched;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::native;
use crate::syscall::process::Process;
use crate::syscall::uaccess;
use crate::timer;
use crate::user::vmo::Vmo;

pub(crate) mod check;
pub(crate) mod dmabuf;
pub(crate) mod file;
pub(crate) mod sync;

/// The most requests one control has outstanding; the next is `EBUSY`.
pub(crate) const MAX_OUTSTANDING: usize = 256;

/// The most driver threads waiting in `chardev_next` on one control; the
/// next is `LIMIT_REACHED`.
pub(crate) const MAX_SEATS: usize = 64;

/// A copy's chunk: between chunks it checks the request is still alive.
const CHUNK: usize = 4096;

/// The most one copy call moves.
pub(crate) const MAX_COPY: usize = 256 * CHUNK;

/// How long the core waits for its driver's HELLO.
const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How long the task sleeps before looking at the channel of its own accord.
const RECHECK_NANOS: u64 = 50_000_000;

/// The driver end's rights: no `TRANSFER`, no `DUPLICATE` (N2).
pub(crate) const DRIVER_RIGHTS: Rights = Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::WAIT.0);

/// The highest errno a reply may name.
const MAX_ERRNO: i64 = 4095;

/// The devices a driver's control claims, which a quiesce waits out.
static CLAIMS: Claims = Claims::new();
/// Controls between creation and their task taking them.
static STARTING: SpinLock<Vec<Arc<Control>>> = SpinLock::new(Vec::new());
/// Whether `device`'s driver serves published device files through this
/// core: what lets the display core hand the same driver a card it can map
/// (displayctl's `copies`, `docs/DISPLAY.md` §2.1).
pub(crate) fn publishes_for(device: &Arc<DeviceNode>) -> bool {
    PUBLISHED
        .lock()
        .iter()
        .any(|(_, control)| Arc::ptr_eq(&control.device, device))
}

/// Every live control, for the native calls to find by their endpoint.
static CONTROLS: SpinLock<Vec<Arc<Control>>> = SpinLock::new(Vec::new());
/// The published nodes: each minor of major 195 and the control serving it.
static PUBLISHED: SpinLock<Vec<(u16, Arc<Control>)>> = SpinLock::new(Vec::new());
/// The published render nodes: each `renderD<N>`'s number, which the render
/// core lent, and the control serving it (N3b; the consultant's B8).
static RENDERS: SpinLock<Vec<(u32, Arc<Control>)>> = SpinLock::new(Vec::new());
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

/// One driver's control: its channel, its requests and its nodes.
pub(crate) struct Control {
    id: usize,
    /// The kernel's end, which the task writes requests on.
    kernel_end: Arc<Endpoint>,
    /// The driver's end, by identity only: a reply or copy must name it.
    driver_end: Weak<Endpoint>,
    device: Arc<DeviceNode>,
    /// The device's location word, which HELLO must name.
    location: u32,
    state: SpinLock<State>,
    /// The task's: woken when there is something to send.
    work: WaitQueue,
    /// Whether the driver takes its requests with `chardev_next`
    /// (`CHARDEV_DIRECT`), and the task writes none on the channel.
    direct: bool,
    /// Set once the driver is gone.
    gone: AtomicBool,
    /// Aperture mappings that hold the device's claim, and whether the
    /// claim has gone back: it goes back when the driver is gone and the
    /// last such mapping with it.
    apertures: SpinLock<Apertures>,
    /// Its live dmabufs, by cookie (N3b, [`dmabuf`]).
    dmabufs: SpinLock<dmabuf::Table>,
    /// Its unsignalled fences, by cookie (N3b sync, [`sync`]).
    syncs: SpinLock<sync::Table>,
}

/// [`Control::apertures`].
#[derive(Debug, Default)]
struct Apertures {
    mapped: usize,
    released: bool,
}

/// What a request is answered with.
#[derive(Clone, Debug)]
pub(crate) enum Answer {
    /// An open's, a release's, or an ioctl's value.
    Value(usize),
    /// An mmap's: what to map, checked against the device and the rights.
    Map(MapReply),
}

/// What a driver answered an mmap with, checked (M1–M3).
#[derive(Clone, Debug)]
pub(crate) enum MapReply {
    /// Pages of a VMO the driver holds, from byte `offset`.
    Vmo { vmo: Arc<Vmo>, offset: u64 },
    /// A range wholly inside one of the device's memory apertures.
    Aperture { phys: u64, combining: bool },
}

/// What the control's lock guards.
struct State {
    /// The next request's id. Never reused.
    next_request: u64,
    /// The next file's identity. Never reused.
    next_file: u64,
    /// Outstanding requests, by ascending id.
    requests: Vec<Arc<Request>>,
    /// What the task has yet to write, in order.
    outgoing: VecDeque<Outgoing>,
    /// How many of `outgoing` are requests: at most [`MAX_OUTSTANDING`],
    /// whatever the table holds (F-63). A request leaves the table when it
    /// is answered or abandoned, which may be before the task has written
    /// it, so the table's count alone does not bound the queue.
    queued: usize,
    /// Files open, each holding one slot of `outgoing` for its release.
    holding: usize,
    /// Driver threads waiting in `chardev_next`, the last to sit down last:
    /// none while `outgoing` holds anything, since whoever queues looks
    /// here first and whoever sits down looks there first, both under this
    /// lock. At most [`MAX_SEATS`].
    seats: Vec<Arc<Seat>>,
    /// Seats nobody sits in, kept so that sitting down allocates nothing.
    spare: Vec<Arc<Seat>>,
}

/// Where one driver thread waits in `chardev_next`.
struct Seat {
    /// What it was handed, by whoever took the seat off the control's list.
    given: SpinLock<Option<Outgoing>>,
    /// Woken when something is handed over, and when the driver is gone.
    queue: WaitQueue,
}

/// One message the task has yet to write.
enum Outgoing {
    Request(Arc<Request>),
    Release {
        file: u64,
        minor: u16,
    },
    /// A dmabuf's last reference went ([`dmabuf`]).
    DmabufRelease {
        cookie: u64,
    },
}

/// One program's request, outstanding until answered or abandoned.
pub(crate) struct Request {
    wire: Wire,
    /// The first bytes of an ioctl's argument, read by the program's own
    /// thread as it made the call; empty unless the control is direct.
    head: Inline,
    /// The waiting program, whose memory the copies reach.
    client: Arc<Process>,
    inner: SpinLock<Inner>,
    waiters: WaitQueue,
}

/// Up to [`INLINE_MAX`] bytes that go with a request or come back with its
/// answer.
#[derive(Clone, Copy, Debug)]
struct Inline {
    len: usize,
    bytes: [u8; INLINE_MAX],
}

impl Inline {
    const EMPTY: Inline = Inline {
        len: 0,
        bytes: [0; INLINE_MAX],
    };

    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// A request's state.
#[derive(Clone, Debug)]
struct Inner {
    /// Cleared when the program abandons the request.
    alive: bool,
    /// The answer, once given: an errno or what was answered.
    answer: Option<Result<Answer, Errno>>,
    /// Copies in flight.
    copying: u32,
    /// What the answer brought for the program's thread to write at the
    /// ioctl's argument before its call returns.
    tail: Inline,
}

impl core::fmt::Debug for Control {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Control")
            .field("id", &self.id)
            .field("gone", &self.gone.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Control {
    /// Whether the driver is gone.
    pub(crate) fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }
}

/// Answer the chardev native calls.
///
/// # Errors
///
/// [`Full`] when the item has no room for a registration.
pub(crate) fn install() -> Result<(), Full> {
    native::serve(NativeCall::ChardevControlCreate, control_create)?;
    native::serve(NativeCall::ChardevReply, reply)?;
    native::serve(NativeCall::ChardevCopyIn, copy_in)?;
    native::serve(NativeCall::ChardevCopyOut, copy_out)?;
    native::serve(NativeCall::ChardevFile, file_of)?;
    native::serve(NativeCall::ChardevDmabufInstall, dmabuf::install)?;
    native::serve(NativeCall::ChardevDmabufResolve, dmabuf::resolve)?;
    native::serve(NativeCall::ChardevSyncInstall, sync::install)?;
    native::serve(NativeCall::ChardevSyncSignal, sync::signal)?;
    native::serve(NativeCall::ChardevSyncResolve, sync::resolve)?;
    native::serve(NativeCall::ChardevNext, next)?;
    native::register_server(&SERVER)
}

/// What a quiesce waits out for this core.
static SERVER: native::Server = native::Server {
    wait_until_unserved,
    release: None,
};

fn wait_until_unserved(
    node: &Arc<DeviceNode>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), StillServed> {
    CLAIMS.wait_until_released(node, cancelled)
}

// ---------------------------------------------------------------------------
// The control and its task
// ---------------------------------------------------------------------------

/// `chardev_control_create`: the device's control channel, one per device,
/// for a device whose interrupts are isolated.
fn control_create(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [device, flags, ..] = *registers;
    if flags & !ferrix_native_abi::types::CHARDEV_DIRECT != 0 {
        return Err(status::INVALID_ARGS);
    }
    let direct = flags & ferrix_native_abi::types::CHARDEV_DIRECT != 0;
    native::control_channel(caller, device, DRIVER_RIGHTS, |node| create(node, direct))
}

fn create(node: &Arc<DeviceNode>, direct: bool) -> Result<Arc<Endpoint>, Errno> {
    if !node.may_take_vectors_and_pins() {
        return Err(status::ACCESS_DENIED);
    }
    let (kernel_end, driver_end) = Endpoint::pair().map_err(|_| status::NO_MEMORY)?;
    if !CLAIMS.claim(node, &kernel_end) {
        return Err(status::ALREADY_BOUND);
    }
    let made = make_control(node, &kernel_end, &driver_end, direct);
    let Some(control) = made else {
        CLAIMS.release(node);
        return Err(status::NO_MEMORY);
    };
    let id = control.id;
    {
        // One guard for the room and the push, so no other push takes the
        // room between them.
        let mut starting = STARTING.lock();
        if starting.try_reserve(1).is_err() {
            drop(starting);
            CLAIMS.release(node);
            return Err(status::NO_MEMORY);
        }
        starting.push(control);
    }
    if sched::spawn("chardev", run, id, ferrix_sched::NICE_0_WEIGHT).is_err() {
        let _ = take_start(id);
        CLAIMS.release(node);
        return Err(status::NO_MEMORY);
    }
    Ok(driver_end)
}

/// A new control over `node`, its room reserved: the request table and the
/// outgoing queue hold [`MAX_OUTSTANDING`] without growing.
fn make_control(
    node: &Arc<DeviceNode>,
    kernel_end: &Arc<Endpoint>,
    driver_end: &Arc<Endpoint>,
    direct: bool,
) -> Option<Arc<Control>> {
    let mut requests = Vec::new();
    requests.try_reserve_exact(MAX_OUTSTANDING).ok()?;
    let mut seats = Vec::new();
    let mut spare = Vec::new();
    if direct {
        seats.try_reserve_exact(MAX_SEATS).ok()?;
        spare.try_reserve_exact(MAX_SEATS).ok()?;
    }
    let mut outgoing = VecDeque::new();
    outgoing.try_reserve_exact(MAX_OUTSTANDING).ok()?;
    crate::fallible::try_arc(Control {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kernel_end: Arc::clone(kernel_end),
        driver_end: Arc::downgrade(driver_end),
        device: Arc::clone(node),
        location: node.describe().location,
        state: SpinLock::new(State {
            next_request: 1,
            next_file: 1,
            requests,
            outgoing,
            queued: 0,
            holding: 0,
            seats,
            spare,
        }),
        work: WaitQueue::new(),
        direct,
        gone: AtomicBool::new(false),
        apertures: SpinLock::new(Apertures::default()),
        dmabufs: SpinLock::new(Vec::new()),
        syncs: SpinLock::new(Vec::new()),
    })
    .ok()
}

fn take_start(id: usize) -> Option<Arc<Control>> {
    let mut starting = STARTING.lock();
    let at = starting.iter().position(|control| control.id == id)?;
    Some(starting.remove(at))
}

/// One control's task: HELLO, then the requests, until the driver goes.
fn run(id: usize) {
    let Some(control) = take_start(id) else {
        return;
    };
    let listed = {
        let mut controls = CONTROLS.lock();
        let room = controls.try_reserve(1).is_ok();
        if room {
            controls.push(Arc::clone(&control));
        }
        room
    };
    if listed && let Some(publication) = take_up(&control) {
        serve(&control);
        unpublish(&control, &publication);
    }
    finish(&control);
    CONTROLS.lock().retain(|held| !Arc::ptr_eq(held, &control));
    release_claim_if_unmapped(&control);
}

/// Give the device's claim back, once, if no aperture mapping holds it.
fn release_claim_if_unmapped(control: &Control) {
    let release = {
        let mut apertures = control.apertures.lock();
        let release = apertures.mapped == 0 && !apertures.released;
        if release {
            apertures.released = true;
        }
        release
    };
    if release {
        CLAIMS.release(&control.device);
    }
}

/// What an aperture mapping keeps: the control, whose claim on the device
/// stays held while it lives (the consultant's ruling, ledger 300).
pub(crate) struct ApertureKeeper {
    control: Arc<Control>,
}

impl Drop for ApertureKeeper {
    fn drop(&mut self) {
        {
            let mut apertures = self.control.apertures.lock();
            apertures.mapped = apertures.mapped.saturating_sub(1);
        }
        if self.control.is_gone() {
            release_claim_if_unmapped(&self.control);
        }
    }
}

/// A keeper for one more aperture mapping of `control`, or `ENODEV` once
/// its claim has gone back.
pub(crate) fn aperture_keeper(control: &Arc<Control>) -> Result<ApertureKeeper, Errno> {
    let mut apertures = control.apertures.lock();
    if apertures.released || control.is_gone() {
        return Err(Errno::ENODEV);
    }
    apertures.mapped = apertures.mapped.saturating_add(1);
    Ok(ApertureKeeper {
        control: Arc::clone(control),
    })
}

/// The first message, or `None` at the deadline or a closed channel.
fn receive(control: &Endpoint, deadline: u64) -> Option<ChannelMessage> {
    loop {
        match control.read(message::MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
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

fn send(control: &Endpoint, message: &Message) -> bool {
    let bytes = message.encode().as_bytes().to_vec();
    control
        .write(bytes, 0, || Ok::<Vec<_>, Infallible>(Vec::new()))
        .is_ok()
}

/// Wait for HELLO and publish its nodes, or refuse it.
fn take_up(control: &Arc<Control>) -> Option<Publication> {
    let deadline = timer::now_nanos().saturating_add(HELLO_PATIENCE_NANOS);
    let message = receive(&control.kernel_end, deadline)?;
    let handed = !message.handles.is_empty();
    crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
    let judged = match Message::decode(&message.bytes) {
        Ok(Message::Hello(hello)) if !handed => session::judge(&hello, control.location),
        _ => Err(Refusal::Protocol),
    };
    let published =
        judged.and_then(|publication| publish(control, &publication).map(|()| publication));
    match published {
        Ok(publication) => {
            let count = u8::try_from(publication.count).unwrap_or(0);
            if !send(&control.kernel_end, &Message::Ready(count)) {
                unpublish(control, &publication);
                return None;
            }
            control.device.hello_accepted();
            crate::console::println!(
                "  chardev  {} node(s) of major {MAJOR} published for its driver",
                publication.count
            );
            Some(publication)
        }
        Err(refusal) => {
            crate::console::println!("  chardev  a driver was refused: {refusal}");
            let _ = send(&control.kernel_end, &Message::Refused(refusal));
            None
        }
    }
}

/// Publish `publication`'s minors for `control`: all or none. The render
/// node, if listed, is numbered by the render core, never by the driver
/// (the consultant's B8, ledger 316), and is not a minor of major 195.
fn publish(control: &Arc<Control>, publication: &Publication) -> Result<(), Refusal> {
    let render = publication.minors().contains(&RENDER_MINOR);
    let majors = publication
        .minors()
        .iter()
        .filter(|minor| **minor != RENDER_MINOR);
    let mut published = PUBLISHED.lock();
    if majors
        .clone()
        .any(|minor| published.iter().any(|(held, _)| held == minor))
    {
        return Err(Refusal::Taken);
    }
    published
        .try_reserve(publication.count)
        .map_err(|_| Refusal::NoMemory)?;
    if render {
        let mut renders = RENDERS.lock();
        renders.try_reserve(1).map_err(|_| Refusal::NoMemory)?;
        let index = crate::interfaces::render::lend_number(control.device.index())
            .ok_or(Refusal::NoMemory)?;
        // NOALLOC: reserved above.
        renders.push((index, Arc::clone(control)));
    }
    for minor in majors {
        // NOALLOC: reserved above.
        published.push((*minor, Arc::clone(control)));
    }
    Ok(())
}

fn unpublish(control: &Arc<Control>, publication: &Publication) {
    let _ = publication;
    PUBLISHED
        .lock()
        .retain(|(_, held)| !Arc::ptr_eq(held, control));
    let mut lent = Vec::new();
    RENDERS.lock().retain(|(index, held)| {
        let mine = Arc::ptr_eq(held, control);
        if mine && lent.try_reserve(1).is_ok() {
            lent.push(*index);
        }
        !mine
    });
    for index in lent {
        crate::interfaces::render::return_number(index);
    }
}

/// Write what is queued, and wait for more, until the driver goes.
fn serve(control: &Arc<Control>) {
    loop {
        let end = &control.kernel_end;
        if end.signals().intersects(Signals::PEER_CLOSED) {
            return;
        }
        // The driver says nothing after HELLO: anything it writes is a lie.
        match end.read(message::MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => {
                crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
                crate::console::println!("  chardev  its driver wrote after HELLO; closing");
                return;
            }
            Err(ReadError::Empty) => {}
            Err(_) => return,
        }
        // A direct control's driver takes what is queued itself
        // (`chardev_next`); this task only watches for its end.
        while !control.direct && end.peer_has_room() {
            let next = {
                let mut state = control.state.lock();
                take_queued(&mut state)
            };
            let Some(next) = next else {
                break;
            };
            // A request answered or abandoned before its turn is not
            // written: nobody waits for it (F-63).
            if settled(&next) {
                continue;
            }
            if !send(end, &Message::Request(wire_of(&next))) {
                return;
            }
        }
        let deadline = timer::now_nanos().saturating_add(RECHECK_NANOS);
        let _ = control.work.wait_until_deadline(
            || {
                end.signals()
                    .intersects(Signals::READABLE | Signals::PEER_CLOSED)
                    || (!control.direct
                        && end.peer_has_room()
                        && !control.state.lock().outgoing.is_empty())
            },
            deadline,
        );
    }
}

/// The head of the queue to the driver, taken off it and off its counts: a
/// request is no longer queued, and a release has used the slot its file
/// held. Under the control's lock.
fn take_queued(state: &mut State) -> Option<Outgoing> {
    let next = state.outgoing.pop_front()?;
    match next {
        Outgoing::Request(_) => state.queued = state.queued.saturating_sub(1),
        Outgoing::Release { .. } | Outgoing::DmabufRelease { .. } => {
            state.holding = state.holding.saturating_sub(1);
        }
    }
    Some(next)
}

/// Whether `item` is a request answered or abandoned already, which is not
/// given to the driver: nobody waits for it (F-63).
fn settled(item: &Outgoing) -> bool {
    match item {
        Outgoing::Request(request) => {
            let inner = request.inner.lock();
            !inner.alive || inner.answer.is_some()
        }
        Outgoing::Release { .. } | Outgoing::DmabufRelease { .. } => false,
    }
}

/// `item` as the driver is told it.
fn wire_of(item: &Outgoing) -> Wire {
    match item {
        Outgoing::Request(request) => request.wire,
        Outgoing::Release { file, minor } => Wire {
            id: 0,
            file: *file,
            op: Op::Release,
            minor: *minor,
            pid: 0,
            euid: 0,
            egid: 0,
            cmd: 0,
            arg: 0,
            pages: 0,
        },
        Outgoing::DmabufRelease { cookie } => Wire {
            id: 0,
            file: 0,
            op: Op::DmabufRelease,
            minor: 0,
            pid: 0,
            euid: 0,
            egid: 0,
            cmd: 0,
            arg: *cookie,
            pages: 0,
        },
    }
}

/// Queue `item` for the driver, or, on a direct control with a driver
/// thread waiting, hand it to the one that sat down last: its seat, for the
/// caller to wake once the lock is let go. Under the control's lock, with
/// the queue's room reserved as each caller says.
fn offer(control: &Control, state: &mut State, item: Outgoing) -> Option<Arc<Seat>> {
    if control.direct
        && let Some(seat) = state.seats.pop()
    {
        // Handed over, it has left the queue it never entered: a release's
        // slot is used, as when the task takes one off the queue.
        if let Outgoing::Release { .. } | Outgoing::DmabufRelease { .. } = item {
            state.holding = state.holding.saturating_sub(1);
        }
        *seat.given.lock() = Some(item);
        return Some(seat);
    }
    if let Outgoing::Request(_) = item {
        state.queued += 1;
    }
    // NOALLOC: every caller's room is reserved (`admit`, `hold_release`).
    state.outgoing.push_back(item);
    None
}

/// After [`offer`], the lock let go: wake whoever is to take what was
/// offered. `how` is for a seat's thread: on the caller's processor for a
/// caller about to wait for the answer, where it is for one that is not.
fn roused(control: &Control, seat: Option<Arc<Seat>>, how: sched::Wake) {
    match seat {
        Some(seat) => seat.queue.wake_all_with(how),
        None if control.direct => {}
        None => control.work.wake_all(),
    }
}

/// The driver is gone: every waiter wakes with `ENODEV`, the table empties,
/// and every program's process is let go of here, in the task.
fn finish(control: &Arc<Control>) {
    control.gone.store(true, Ordering::Release);
    let (requests, outgoing, seats) = {
        let mut state = control.state.lock();
        state.queued = 0;
        (
            core::mem::take(&mut state.requests),
            core::mem::take(&mut state.outgoing),
            core::mem::take(&mut state.seats),
        )
    };
    for request in &requests {
        answer(request, Err(Errno::ENODEV));
    }
    // A thread still waiting in `chardev_next` -- the driver closed its end
    // and lives on -- finds the control gone.
    for seat in &seats {
        seat.queue.wake_all();
    }
    drop(seats);
    drop(outgoing);
    drop(requests);
    // Every fence it made, signalled ENODEV: no waiter hangs on a dead
    // driver (S4).
    sync::control_gone(control);
}

/// Answer `request`, once, and wake its program.
fn answer(request: &Request, outcome: Result<Answer, Errno>) {
    answer_with(request, outcome, &Inline::EMPTY, sched::Wake::Home);
}

/// Answer `request`, once, with `tail` for its program's thread to write at
/// the ioctl's argument, and wake the program as `how` says.
fn answer_with(request: &Request, outcome: Result<Answer, Errno>, tail: &Inline, how: sched::Wake) {
    {
        let mut inner = request.inner.lock();
        if inner.answer.is_none() {
            inner.answer = Some(outcome);
            inner.tail = *tail;
        }
    }
    request.waiters.wake_all_with(how);
}

// ---------------------------------------------------------------------------
// The program's side
// ---------------------------------------------------------------------------

/// The control serving minor `minor` of major 195, if one is published.
pub(crate) fn published(minor: u16) -> Option<Arc<Control>> {
    PUBLISHED
        .lock()
        .iter()
        .find(|(held, _)| *held == minor)
        .map(|(_, control)| Arc::clone(control))
}

/// Every published minor, ascending.
pub(crate) fn published_minors() -> Vec<u16> {
    let published = PUBLISHED.lock();
    let mut minors = Vec::new();
    if minors.try_reserve(published.len()).is_ok() {
        minors.extend(published.iter().map(|(minor, _)| *minor));
    }
    drop(published);
    minors.sort_unstable();
    minors
}

/// The control serving `renderD<index>`, if a chardev driver serves it.
pub(crate) fn render_control(index: u32) -> Option<Arc<Control>> {
    RENDERS
        .lock()
        .iter()
        .find(|(held, _)| *held == index)
        .map(|(_, control)| Arc::clone(control))
}

/// Take one slot of `control`'s outgoing queue for a file's release.
pub(crate) fn hold_release(control: &Control) -> Result<(), Errno> {
    let mut state = control.state.lock();
    let holding = state.holding.saturating_add(1);
    let needed = (MAX_OUTSTANDING + holding).saturating_sub(state.outgoing.len());
    state
        .outgoing
        .try_reserve(needed)
        .map_err(|_| Errno::ENOMEM)?;
    state.holding = holding;
    Ok(())
}

/// Queue the release of `file`, whose slot [`hold_release`] took.
pub(crate) fn queue_release(control: &Control, file: u64, minor: u16) {
    let seat = {
        // Gone is read under the guard: `finish` sets it before it takes
        // the lock to empty the queue, so a release pushed here is never
        // pushed into the emptied queue, which has no room left.
        let mut state = control.state.lock();
        if control.is_gone() {
            state.holding = state.holding.saturating_sub(1);
            return;
        }
        // NOALLOC: the slot `hold_release` reserved.
        offer(control, &mut state, Outgoing::Release { file, minor })
    };
    roused(control, seat, sched::Wake::Home);
}

/// Give back a slot [`hold_release`] took that no release will use.
pub(crate) fn unhold_release(control: &Control) {
    let mut state = control.state.lock();
    state.holding = state.holding.saturating_sub(1);
}

/// Queue the release of the dmabuf `cookie` names, in the slot
/// [`hold_release`] took when it was made; nothing once the driver is gone
/// (the consultant's B4: the closer never waits on the driver).
pub(crate) fn queue_dmabuf_release(control: &Control, cookie: u64) {
    let seat = {
        let mut state = control.state.lock();
        if control.is_gone() {
            state.holding = state.holding.saturating_sub(1);
            return;
        }
        // NOALLOC: the slot `hold_release` reserved.
        offer(control, &mut state, Outgoing::DmabufRelease { cookie })
    };
    roused(control, seat, sched::Wake::Home);
}

/// A new file identity on `control`.
pub(crate) fn next_file(control: &Control) -> u64 {
    let mut state = control.state.lock();
    let file = state.next_file;
    state.next_file = file.saturating_add(1);
    file
}

/// What one request asks of a file: the REQUEST's own fields.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ask {
    /// The operation.
    pub(crate) op: Op,
    /// The file's identity.
    pub(crate) file: u64,
    /// Its node's minor.
    pub(crate) minor: u16,
    /// An ioctl's command; an mmap's protection and flags.
    pub(crate) cmd: u32,
    /// An ioctl's argument; an mmap's offset.
    pub(crate) arg: u64,
    /// An mmap's length in pages.
    pub(crate) pages: u32,
}

/// Send `ask` on `control` for `client` and wait for the answer.
///
/// # Errors
///
/// `ENODEV` once the driver is gone, `EBUSY` with [`MAX_OUTSTANDING`]
/// requests outstanding, `EINTR` when a signal abandons the request, or the
/// errno the driver answered.
pub(crate) fn call(
    control: &Arc<Control>,
    client: &Arc<Process>,
    ask: Ask,
) -> Result<Answer, Errno> {
    let request = admit(control, client, ask)?;
    await_answer(control, &request, client)
}

/// Take a request in: in the table and queued for the task, or `EBUSY`
/// with [`MAX_OUTSTANDING`] outstanding or queued (F-63).
fn admit(control: &Arc<Control>, client: &Arc<Process>, ask: Ask) -> Result<Arc<Request>, Errno> {
    let Ask {
        op,
        file,
        minor,
        cmd,
        arg,
        pages,
    } = ask;
    let (euid, egid) = client
        .with_credentials(|credentials| (credentials.user.effective, credentials.group.effective));
    // The first bytes of an ioctl's argument go with it to a driver that
    // takes its requests: read here, by the program's own thread from its
    // own memory, before any lock. An argument that cannot be read goes
    // without; the driver's own copy then finds the fault, as before.
    let mut head = Inline::EMPTY;
    if control.direct && op == Op::Ioctl {
        let len = message::inline_len(cmd);
        if let Some(room) = head.bytes.get_mut(..len)
            && uaccess::copy_from_user(client.space(), arg, room).is_ok()
        {
            head.len = len;
        }
    }
    let (request, seat) = {
        let mut state = control.state.lock();
        if control.is_gone() {
            return Err(Errno::ENODEV);
        }
        if state.requests.len() >= MAX_OUTSTANDING || state.queued >= MAX_OUTSTANDING {
            return Err(Errno::EBUSY);
        }
        let id = state.next_request;
        state.next_request = id.saturating_add(1);
        let request = crate::fallible::try_arc(Request {
            wire: Wire {
                id,
                file,
                op,
                minor,
                pid: client.pid(),
                euid,
                egid,
                cmd,
                arg,
                pages,
            },
            head,
            client: Arc::clone(client),
            inner: SpinLock::new(Inner {
                alive: true,
                answer: None,
                copying: 0,
                tail: Inline::EMPTY,
            }),
            waiters: WaitQueue::new(),
        })
        .map_err(|_| Errno::ENOMEM)?;
        // NOALLOC: both reserved when the control was made: the table for
        // MAX_OUTSTANDING, which it holds fewer than, and the queue for
        // MAX_OUTSTANDING requests and a release per open file, of which
        // `queued` counts the requests (F-63).
        state.requests.push(Arc::clone(&request));
        let seat = offer(control, &mut state, Outgoing::Request(Arc::clone(&request)));
        (request, seat)
    };
    // The program sleeps for the answer next: a driver thread that waited
    // for this request runs where the program ran.
    roused(control, seat, sched::Wake::Sync);
    Ok(request)
}

/// Wait for `request`'s answer, or abandon it on a signal.
fn await_answer(
    control: &Control,
    request: &Arc<Request>,
    client: &Process,
) -> Result<Answer, Errno> {
    let _ = request.waiters.wait_until_deadline(
        || request.inner.lock().answer.is_some() || client.signal_pending(),
        u64::MAX,
    );
    let answered = {
        let inner = request.inner.lock();
        inner.answer.clone().map(|answer| (answer, inner.tail))
    };
    if let Some((answer, tail)) = answered {
        // A reply from one driver thread may come while another is still
        // copying for the request: the program's call returns only once no
        // copy can touch its memory (N4; the consultant's L1, ledger 297).
        drain(request);
        // What the answer brought for the ioctl's argument, written by the
        // program's own thread: an argument that cannot be written fails
        // the call, as a driver's failed copy-out does.
        if answer.is_ok()
            && tail.len != 0
            && uaccess::copy_to_user(client.space(), request.wire.arg, tail.as_bytes()).is_err()
        {
            return Err(Errno::EFAULT);
        }
        return answer;
    }
    abandon(control, request);
    Err(Errno::EINTR)
}

/// Wait until no copy for `request` is in flight. Each copy re-checks the
/// request between chunks of 4 KiB, so this waits out one chunk at most.
fn drain(request: &Request) {
    let _ = request
        .waiters
        .wait_until_deadline(|| request.inner.lock().copying == 0, u64::MAX);
}

/// The program gave up on `request`: dead from here, out of the table, and
/// no copy for it still running when this returns (N4).
fn abandon(control: &Control, request: &Arc<Request>) {
    request.inner.lock().alive = false;
    {
        let mut state = control.state.lock();
        if let Ok(at) = state
            .requests
            .binary_search_by_key(&request.wire.id, |held| held.wire.id)
        {
            let _ = state.requests.remove(at);
        }
        // Not yet written, it never will be: out of the queue, so a driver
        // that stops reading cannot make the queue outgrow its room (F-63).
        let queued = state
            .outgoing
            .iter()
            .position(|held| matches!(held, Outgoing::Request(held) if Arc::ptr_eq(held, request)));
        if let Some(at) = queued {
            let _ = state.outgoing.remove(at);
            state.queued = state.queued.saturating_sub(1);
        }
    }
    drain(request); // and any copy in flight for it (N4)
}

// ---------------------------------------------------------------------------
// The driver's calls
// ---------------------------------------------------------------------------

/// The control whose driver end `handle` names in `caller`'s table, if any.
fn control_of(caller: &dyn Host, handle: u64) -> Result<Arc<Control>, Errno> {
    let endpoint = caller.core().with_handles(|table| {
        let (object, _) = table
            .get(Handle::from_register(handle))
            .map_err(|_| status::BAD_HANDLE)?;
        match object {
            Object::Channel(endpoint) => Ok(Arc::clone(endpoint)),
            _ => Err(status::WRONG_TYPE),
        }
    })?;
    CONTROLS
        .lock()
        .iter()
        .find(|control| core::ptr::eq(control.driver_end.as_ptr(), Arc::as_ptr(&endpoint)))
        .cloned()
        .ok_or(status::WRONG_TYPE)
}

/// The outstanding request `id` of `control`.
fn outstanding(control: &Control, id: u64) -> Result<Arc<Request>, Errno> {
    let state = control.state.lock();
    let at = state
        .requests
        .binary_search_by_key(&id, |held| held.wire.id)
        .map_err(|_| status::BAD_STATE)?;
    state.requests.get(at).cloned().ok_or(status::BAD_STATE)
}

/// `chardev_reply(control, request, status, value)` (N7).
fn reply(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, id, status_word, value, a4, a5] = *registers;
    let control = control_of(caller, handle)?;
    conclude(
        caller,
        &control,
        [id, status_word, value, a4, a5],
        &Inline::EMPTY,
        sched::Wake::Home,
    )
}

/// Answer request `id` of `control` as `chardev_reply`'s registers say,
/// with `tail` for the program to write, and wake the program as `how`
/// says.
fn conclude(
    caller: &dyn Host,
    control: &Arc<Control>,
    registers: [u64; 5],
    tail: &Inline,
    how: sched::Wake,
) -> Result<usize, Errno> {
    let [id, status_word, value, a4, a5] = registers;
    let request = {
        let mut state = control.state.lock();
        let at = state
            .requests
            .binary_search_by_key(&id, |held| held.wire.id)
            .map_err(|_| status::BAD_STATE)?;
        state.requests.remove(at)
    };
    let status = status_word as i64;
    let value = value as i64;
    let well_formed = status == 0 || (-MAX_ERRNO..=-1).contains(&status);
    let outcome = if !well_formed {
        Err(Errno::EIO)
    } else if status < 0 {
        Err(Errno(u16::try_from(-status).unwrap_or(5)))
    } else {
        match request.wire.op {
            // The kernel makes the descriptor; the driver's value is ignored.
            Op::Open | Op::Release => Ok(Answer::Value(0)),
            Op::Ioctl => usize::try_from(value)
                .ok()
                .filter(|value| i32::try_from(*value).is_ok())
                .map(Answer::Value)
                .ok_or(Errno::EIO),
            Op::Mmap => map_reply(caller, control, &request, value as u64, a4, a5)
                .map(Answer::Map)
                .ok_or(Errno::EIO),
            // Never a request anyone waits in.
            Op::DmabufRelease => Err(Errno::EIO),
        }
    };
    let accepted = outcome.is_ok() || status < 0;
    // Bytes for the argument go only with an ioctl's value.
    let tail = if matches!(outcome, Ok(Answer::Value(_))) && request.wire.op == Op::Ioctl {
        tail
    } else {
        &Inline::EMPTY
    };
    answer_with(&request, outcome, tail, how);
    if well_formed && accepted {
        Ok(0)
    } else {
        Err(status::INVALID_ARGS)
    }
}

/// `chardev_next(control, out, request, result, tail, tail length)`: answer
/// one request, unless `request` is zero, and take the next
/// (`docs/NVIDIA.md` §4.7).
fn next(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, out, id, result, tail_at, tail_len] = *registers;
    let control = control_of(caller, handle)?;
    if !control.direct {
        return Err(status::BAD_STATE);
    }
    let tail_len = usize::try_from(tail_len).map_err(|_| status::INVALID_ARGS)?;
    if tail_len > INLINE_MAX {
        return Err(status::INVALID_ARGS);
    }
    let driver = caller.core();
    // Everything of the caller's is read before anything is answered: a
    // tail that cannot be read leaves the request outstanding.
    let mut tail = Inline::EMPTY;
    if id != 0 && tail_len != 0 {
        let room = tail.bytes.get_mut(..tail_len).ok_or(status::INVALID_ARGS)?;
        uaccess::copy_from_user(driver.space(), tail_at, room).map_err(|_| status::FAULT)?;
        tail.len = tail_len;
    }
    let mut replied = 0_i32;
    if id != 0 {
        // The program is woken onto this processor when this thread is
        // about to wait here, and where it sleeps when more is queued.
        let how = if control.state.lock().outgoing.is_empty() {
            sched::Wake::Sync
        } else {
            sched::Wake::Home
        };
        let signed = result as i64;
        let (status_word, value) = if signed < 0 { (result, 0) } else { (0, result) };
        if let Err(errno) = conclude(caller, &control, [id, status_word, value, 0, 0], &tail, how) {
            replied = -i32::from(errno.0);
        }
    }
    loop {
        let item = match sit(&control)? {
            Sat::Taken(item) => item,
            Sat::Seat(seat) => {
                let _ = seat.queue.wait_until_deadline(
                    || {
                        seat.given.lock().is_some()
                            || control.is_gone()
                            || caller.wait_interrupted()
                    },
                    u64::MAX,
                );
                match rise(&control, seat) {
                    Some(item) => item,
                    None if control.is_gone() => return Err(status::PEER_CLOSED),
                    None => return Err(Errno::EINTR),
                }
            }
        };
        if settled(&item) {
            continue;
        }
        let mut bytes = [0_u8; message::NEXT_BYTES];
        let head = match &item {
            Outgoing::Request(request) => request.head,
            Outgoing::Release { .. } | Outgoing::DmabufRelease { .. } => Inline::EMPTY,
        };
        let encoded = Message::Request(wire_of(&item)).encode();
        let length = message::NEXT_HEAD + head.len;
        let fits = lay(&mut bytes, 0, encoded.as_bytes())
            && lay(&mut bytes, message::REQUEST_BYTES, &replied.to_le_bytes())
            && lay(
                &mut bytes,
                message::REQUEST_BYTES + 4,
                &u32::try_from(head.len).unwrap_or(0).to_le_bytes(),
            )
            && lay(&mut bytes, message::NEXT_HEAD, head.as_bytes());
        let written = fits
            && bytes
                .get(..length)
                .is_some_and(|whole| uaccess::copy_to_user(driver.space(), out, whole).is_ok());
        if written {
            return Ok(0);
        }
        // Taken off the queue and not delivered: its program does not wait
        // for a driver that never heard of it.
        if let Outgoing::Request(request) = &item {
            {
                let mut state = control.state.lock();
                if let Ok(at) = state
                    .requests
                    .binary_search_by_key(&request.wire.id, |held| held.wire.id)
                {
                    let _ = state.requests.remove(at);
                }
            }
            answer(request, Err(Errno::EIO));
        }
        return Err(status::FAULT);
    }
}

/// Put `source` in `bytes` at `at`; whether it fitted.
fn lay(bytes: &mut [u8], at: usize, source: &[u8]) -> bool {
    match bytes.get_mut(at..at.saturating_add(source.len())) {
        Some(slot) => {
            slot.copy_from_slice(source);
            true
        }
        None => false,
    }
}

/// What a driver thread found asking for the next request.
enum Sat {
    /// Something was queued.
    Taken(Outgoing),
    /// Nothing was: it is on the control's list in this seat.
    Seat(Arc<Seat>),
}

/// Take the head of `control`'s queue, or sit down to wait for one.
fn sit(control: &Control) -> Result<Sat, Errno> {
    let mut state = control.state.lock();
    if control.is_gone() {
        return Err(status::PEER_CLOSED);
    }
    if let Some(item) = take_queued(&mut state) {
        return Ok(Sat::Taken(item));
    }
    if state.seats.len() >= MAX_SEATS {
        return Err(status::LIMIT_REACHED);
    }
    let seat = match state.spare.pop() {
        Some(seat) => seat,
        None => crate::fallible::try_arc(Seat {
            given: SpinLock::new(None),
            queue: WaitQueue::new(),
        })
        .map_err(|_| status::NO_MEMORY)?,
    };
    // NOALLOC: reserved for MAX_SEATS when the control was made.
    state.seats.push(Arc::clone(&seat));
    Ok(Sat::Seat(seat))
}

/// Leave `seat` after a wait: what it was handed, if anything. Off the
/// control's list if it was still on it, and kept for the next to sit.
fn rise(control: &Control, seat: Arc<Seat>) -> Option<Outgoing> {
    let mut state = control.state.lock();
    if let Some(at) = state.seats.iter().position(|held| Arc::ptr_eq(held, &seat)) {
        let _ = state.seats.remove(at);
    }
    // Whoever hands a seat something takes it off the list first, under
    // this lock: off the list here, it is handed nothing more.
    let given = seat.given.lock().take();
    if state.spare.len() < MAX_SEATS && state.spare.capacity() > state.spare.len() {
        // NOALLOC: within the capacity reserved when the control was made.
        state.spare.push(seat);
    }
    given
}

/// Check an mmap's answer: `value` a VMO handle with `a4` its byte offset,
/// or a physical address, by `a5`'s kind (M1–M3).
fn map_reply(
    caller: &dyn Host,
    control: &Control,
    request: &Request,
    value: u64,
    a4: u64,
    a5: u64,
) -> Option<MapReply> {
    use ferrix_chardevctl::message::{MAP_APERTURE, MAP_KIND, MAP_VMO, MAP_WRITE_COMBINING};
    const PAGE: u64 = 4096;
    const PROT_WRITE: u32 = 2;
    let len = u64::from(request.wire.pages).checked_mul(PAGE)?;
    let write = request.wire.cmd & PROT_WRITE != 0;
    let combining = a5 & MAP_WRITE_COMBINING != 0;
    if a5 & !(MAP_KIND | MAP_WRITE_COMBINING) != 0 {
        return None;
    }
    match a5 & MAP_KIND {
        MAP_VMO if !combining => {
            if !a4.is_multiple_of(PAGE) {
                return None;
            }
            let vmo = caller.core().with_handles(|table| {
                let (object, rights) = table.get(Handle::from_register(value)).ok()?;
                let Object::Vmo(vmo) = object else {
                    return None;
                };
                let needed = if write {
                    Rights::READ.0 | Rights::WRITE.0
                } else {
                    Rights::READ.0
                };
                rights.contains(Rights(needed)).then(|| Arc::clone(vmo))
            })?;
            (a4.checked_add(len)? <= vmo.len_bytes()).then_some(MapReply::Vmo { vmo, offset: a4 })
        }
        MAP_APERTURE => {
            let end = value.checked_add(len)?;
            if !value.is_multiple_of(PAGE) || len == 0 {
                return None;
            }
            let inside = (0..control.device.apertures().len()).find_map(|index| {
                let info = control.device.aperture_info(index)?;
                let aperture_end = info.phys.checked_add(info.len)?;
                (info.phys <= value && end <= aperture_end).then_some(info)
            })?;
            let prefetchable = inside.flags & ferrix_native_abi::types::APERTURE_PREFETCHABLE != 0;
            (!combining || prefetchable).then_some(MapReply::Aperture {
                phys: value,
                combining,
            })
        }
        _ => None,
    }
}

/// `chardev_file(control, request, descriptor)`: the identity of the file
/// the waiting program's descriptor names, if it is one of this control's.
fn file_of(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, id, descriptor, ..] = *registers;
    let control = control_of(caller, handle)?;
    let request = outstanding(&control, id)?;
    {
        let inner = request.inner.lock();
        if !inner.alive || inner.answer.is_some() {
            return Err(status::BAD_STATE);
        }
    }
    let file = crate::syscall::fd::file(&request.client, crate::syscall::fd::arg(descriptor))
        .map_err(|_| status::BAD_HANDLE)?;
    let opened = file::of(file.io()).ok_or(status::BAD_HANDLE)?;
    if !Arc::ptr_eq(opened.control(), &control) {
        return Err(status::BAD_HANDLE);
    }
    usize::try_from(opened.identity()).map_err(|_| status::BAD_HANDLE)
}

/// Which way a copy goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Way {
    /// From the program to the driver.
    In,
    /// From the driver to the program.
    Out,
}

/// `chardev_copy_in(control, request, client, buffer, length)`.
fn copy_in(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    copy(caller, registers, Way::In)
}

/// `chardev_copy_out(control, request, client, buffer, length)`.
fn copy_out(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    copy(caller, registers, Way::Out)
}

fn copy(caller: &dyn Host, registers: &[u64; 6], way: Way) -> Result<usize, Errno> {
    let [handle, id, client_at, buffer, length, _] = *registers;
    let length = usize::try_from(length).map_err(|_| status::INVALID_ARGS)?;
    if length > MAX_COPY {
        return Err(status::INVALID_ARGS);
    }
    let control = control_of(caller, handle)?;
    let request = outstanding(&control, id)?;
    begin_copy(&request)?;
    let moved = copy_chunks(caller, &request, way, client_at, buffer, length);
    copy_done(&request);
    moved.map(|()| 0)
}

/// Count one more copy for `request`, which must be alive and unanswered:
/// the program's call does not return while it runs (N4). A dmabuf's
/// install and resolve count as copies too, so a descriptor never lands in
/// a program that has already given up.
fn begin_copy(request: &Request) -> Result<(), Errno> {
    let mut inner = request.inner.lock();
    if !inner.alive || inner.answer.is_some() {
        return Err(status::BAD_STATE);
    }
    inner.copying = inner.copying.saturating_add(1);
    Ok(())
}

/// One copy for `request` is over: wake the program if it was waiting for
/// the last one, on either way out of its call, its request answered or
/// abandoned (N4).
fn copy_done(request: &Request) {
    let drained = {
        let mut inner = request.inner.lock();
        inner.copying = inner.copying.saturating_sub(1);
        inner.copying == 0 && (!inner.alive || inner.answer.is_some())
    };
    if drained {
        request.waiters.wake_all();
    }
}

/// Move `length` bytes chunk by chunk, re-checking between chunks that the
/// request is alive and unanswered.
fn copy_chunks(
    caller: &dyn Host,
    request: &Request,
    way: Way,
    client_at: u64,
    buffer: u64,
    length: usize,
) -> Result<(), Errno> {
    let mut bounce = Vec::new();
    bounce
        .try_reserve_exact(CHUNK.min(length))
        .map_err(|_| status::NO_MEMORY)?;
    bounce.resize(CHUNK.min(length), 0);
    let driver = caller.core().space();
    let client = request.client.space();
    let mut done = 0;
    while done < length {
        {
            let inner = request.inner.lock();
            if !inner.alive || inner.answer.is_some() {
                return Err(status::BAD_STATE);
            }
        }
        let chunk = CHUNK.min(length - done);
        let offset = u64::try_from(done).map_err(|_| status::INVALID_ARGS)?;
        let from_client = client_at.checked_add(offset).ok_or(status::INVALID_ARGS)?;
        let in_driver = buffer.checked_add(offset).ok_or(status::INVALID_ARGS)?;
        let slice = bounce.get_mut(..chunk).ok_or(status::INVALID_ARGS)?;
        match way {
            Way::In => {
                uaccess::copy_from_user(client, from_client, slice).map_err(|_| status::FAULT)?;
                uaccess::copy_to_user(driver, in_driver, slice).map_err(|_| status::FAULT)?;
            }
            Way::Out => {
                uaccess::copy_from_user(driver, in_driver, slice).map_err(|_| status::FAULT)?;
                uaccess::copy_to_user(client, from_client, slice).map_err(|_| status::FAULT)?;
            }
        }
        done += chunk;
    }
    Ok(())
}
