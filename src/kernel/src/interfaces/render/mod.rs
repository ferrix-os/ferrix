//! The render core: the kernel's half of `src/lib/proto/renderctl`, and what will
//! sit under `/dev/dri/renderD<N>`.
//!
//! `docs/GPU.md` §3.3 is the specification and the reason this is a core of
//! its own rather than part of the display's. The two are different
//! conversations about the same card -- one about what is on the screen and
//! one about what the GPU computes -- and only this one has a second
//! implementation coming (§4's NVIDIA driver). So what is here knows a
//! renderer's *conversation* and nothing about virtio: which contexts and
//! objects exist, which fences are outstanding, and what the driver is
//! called. A command buffer's bytes go through a VMO neither side reads on
//! the way past.
//!
//! The shape is `crate::interfaces::display`'s, because the problems are the same: one
//! control channel per device made by a native call, a task per device, a
//! HELLO within its patience or the driver is refused, and `src/lib/proto/renderctl`'s
//! [`Session`] judging every reply so that a driver which answers a question
//! nobody asked is caught before the kernel acts on it.
//!
//! # The renderer outlives its proof
//!
//! A [`Renderer`] is published once its driver has answered, and its task
//! then *serves* it: every reply is taken from the channel, judged by the
//! session and left where the request waiting for it will find it. That is
//! what lets a request be made from a caller's own task -- the node's
//! ioctls, when they are written -- rather than only from this one. The
//! same division as the display's: [`Renderer::request`] sends with the
//! state locked, [`Renderer::collect`] sleeps on the wait queue, and no spin
//! lock is ever held across the sleep.
//!
//! # An upload or a stream is answered when it is sent
//!
//! `VIRTGPU_TRANSFER_TO_HOST` and `VIRTGPU_EXECBUFFER` return on Linux once
//! the work is queued, and a program that is about to write a backing again
//! calls `VIRTGPU_WAIT` first. So they do here: [`Renderer::transfer`] to the
//! device and [`Renderer::submit`] send their request and return, and the
//! reply is taken by [`serve`], which is what gives a command slot back and
//! what [`Renderer::settle`] -- `WAIT` -- waits for. A frame that was an
//! upload or two, a stream and a flush, each a round trip through the driver
//! and the device (`docs/GPU.md` §3.9), is one round trip, the flush's,
//! with the rest on the device behind it. What such a reply says goes
//! unheard, as it does on Linux: the device refusing a stream is a picture
//! that is wrong, and the core says so once on the console.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicUsize, Ordering};

use ferrix_blkring::identity::Location;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;
use ferrix_renderctl::message::{
    BACKING_RIGHTS, Direction, MAX_BYTES, MakeBlob, Message, NO_WINDOW, Ready, Refusal, Status,
    Transfer as Move, VERSION, WORK_VMO_RIGHTS, Work, features, flags,
};
use ferrix_renderctl::session::{Event, RequestError, Session};

use crate::claim::StillServed;
use crate::claim::{Claims, Numbers};
use crate::device::DeviceNode;
use crate::hooks::Full;
use crate::object::channel::{ChannelMessage, Endpoint, ReadError};
use crate::object::port::Port;
use crate::object::process::Host;
use crate::object::{Object, Transfer};
use crate::sched;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::native;
use crate::timer;
use crate::user::vmo::Vmo;

pub(crate) mod fence;
pub(crate) mod node;

/// How much work VMO one renderer gets: command buffers and object
/// descriptions on their way to the device.
///
/// A frame's worth of virgl is tens of kilobytes; a megabyte is room for
/// many in flight without being memory a guest notices.
pub(crate) const WORK_BYTES: u64 = 1024 * 1024;

/// How many words an object's description is: the ten `src/user/system/native/drivers/display/virtio-gpu` reads back
/// -- target, format, bind, width, height, depth, array size, last level,
/// samples and flags, which is a virtio-gpu resource's whole shape.
///
/// What they *mean* is the driver's language and the core never looks at
/// them; it only says where they are (`docs/GPU.md` §3.3). Another driver
/// would be handed other words by another node, in the same slots.
pub(crate) const DESCRIBE_WORDS: usize = 10;

/// How many bytes of the work VMO one description's slot is: the words, and
/// room to the next power of two so that a slot's place is a shift.
pub(crate) const DESCRIBE_BYTES: u64 = 64;

/// How many bytes one command buffer's slot is, and so the longest command
/// buffer one submission carries.
///
/// A frame of virgl is state and draws -- pixels go by `TRANSFER`, not
/// inline -- so tens of kilobytes is a great many windows. The driver has the
/// device read it where it lies, and its command area, which a submission is
/// copied into whole when the work VMO could not be pinned, has room for it.
pub(crate) const COMMAND_BYTES: u64 = 64 * 1024;

/// How many command buffers can be on their way at once: what is left of
/// the work VMO after the descriptions, in slots.
const COMMAND_SLOTS: usize = ((WORK_BYTES - DESCRIBE_REGION) / COMMAND_BYTES) as usize;

/// Where object ids start.
///
/// A card has two conversations and one device, and the device names a
/// display buffer and a render object from the same set of numbers: the
/// display core counts its buffers from 1, so this core counts from far
/// above anything that will reach. The number is also the `res_handle` a
/// program writes into its command streams, so it cannot be translated on
/// the way down -- it has to be apart from the start.
const FIRST_OBJECT: u32 = 1 << 30;

/// How many descriptions the work VMO holds at once: one per object the
/// session will track, so a description slot is never what refuses a request
/// the session itself would have taken.
const DESCRIBE_SLOTS: usize = ferrix_renderctl::session::MAX_OBJECTS;

/// The bytes at the base of the work VMO that the description slots own.
/// Command buffers come after them.
const DESCRIBE_REGION: u64 = DESCRIBE_BYTES * DESCRIBE_SLOTS as u64;

/// How long the core waits for its driver's HELLO.
const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How long a request waits for the driver's reply.
const REPLY_PATIENCE_NANOS: u64 = 5_000_000_000;

/// How long the task sleeps before looking at the channel of its own accord,
/// so that a renderer whose driver went quiet still notices it is gone.
const RECHECK_NANOS: u64 = 1_000_000_000;

/// Why a control channel could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CreateError {
    /// The device already has one.
    InUse,
    /// No memory for the channel, or no stack for the task.
    NoMemory,
}

/// Why a request on a published renderer could not be answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RenderError {
    /// The request is not one the protocol has a state for now.
    Request(RequestError),
    /// The device was asked and said no. Nothing is tracked either way, and
    /// the driver is still there: this is the device's answer, not a fault.
    Refused(Status),
    /// The driver's channel is full: it is behind, and the request can be
    /// made again once it has read.
    Busy,
    /// The driver is gone or broke the protocol.
    Gone,
    /// The driver did not answer in time.
    TimedOut,
}

/// A device whose render channel has been made, waiting for its task.
struct Start {
    id: usize,
    control: Arc<Endpoint>,
    device: Arc<DeviceNode>,
    location: Option<Location>,
}

/// A reply nobody is waiting for any more: an open that closed without
/// waiting, or a request that timed out. [`serve`] drops it when it comes
/// instead of leaving it in `events` for ever.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Abandoned {
    /// An object's `OBJ_MADE` or `OBJ_GONE`.
    Object(u32),
    /// A context's `CTX_MADE` or `CTX_GONE`.
    Context(u32),
    /// An object's `TRANSFERRED`, from the device.
    Transfer(u32),
    /// An object's `OBJ_ATTACHED`.
    Attach(u32),
}

/// An upload or a command stream whose caller was answered when it was
/// sent, and whose reply is still to come.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Flying {
    /// What the reply will name.
    flight: Flight,
    /// The context it was sent for, whose `WAIT` waits for it.
    context: u32,
    /// Its place among everything sent, so that a `WAIT` waits for what came
    /// before it and nothing after.
    sequence: u64,
}

/// The places of the device's host-visible window: where blobs are mapped
/// for programs to reach (`docs/GPU.md` §6.1).
///
/// The core hands them out as it hands out ranges of the work VMO, and gives
/// one back only when the device has said the blob in it is gone: a place
/// given out again while the device still had a blob there would have two
/// blobs in it. A blob the device would not let go of keeps its place for
/// good, the rule `docs/DISPLAY.md` §2.2 states for pages.
struct Places {
    /// Where the window's first page is.
    phys: u64,
    /// How many bytes it has.
    len: u64,
    /// What is free, as `(offset, len)` runs in order, none touching.
    free: Vec<(u64, u64)>,
    /// What each blob holds, as `(object, offset, len)`.
    held: Vec<(u32, u64, u64)>,
}

impl Places {
    fn new(phys: u64, len: u64) -> Self {
        Self {
            phys,
            len,
            free: vec![(0, len)],
            held: Vec::new(),
        }
    }

    /// Hold `len` bytes for `object`, the first run that has room: where.
    fn take(&mut self, object: u32, len: u64) -> Option<u64> {
        let at = self.free.iter().position(|&(_, run)| run >= len)?;
        let (offset, run) = *self.free.get(at)?;
        if run == len {
            let _ = self.free.remove(at);
        } else if let Some(slot) = self.free.get_mut(at) {
            *slot = (offset + len, run - len);
        }
        self.held.push((object, offset, len));
        Some(offset)
    }

    /// The device has let `object` go, or would not: its place comes back
    /// only in the first case.
    fn settle(&mut self, object: u32, gone: bool) {
        let Some(at) = self.held.iter().position(|&(held, _, _)| held == object) else {
            return;
        };
        let (_, offset, len) = self.held.remove(at);
        if !gone {
            return;
        }
        let at = self.free.partition_point(|&(start, _)| start < offset);
        self.free.insert(at, (offset, len));
        // Join it to the runs either side, which keeps the window from
        // fragmenting into places nothing fits.
        if let Some(&(next, next_len)) = self.free.get(at + 1)
            && offset + len == next
        {
            if let Some(slot) = self.free.get_mut(at) {
                slot.1 += next_len;
            }
            let _ = self.free.remove(at + 1);
        }
        if at > 0
            && let (Some(&(before, before_len)), Some(&(_, len))) =
                (self.free.get(at - 1), self.free.get(at))
            && before + before_len == offset
        {
            if let Some(slot) = self.free.get_mut(at - 1) {
                slot.1 = before_len + len;
            }
            let _ = self.free.remove(at);
        }
    }
}

/// Where a mapped blob's pages are, and how the device said to cache them:
/// what `mmap` of the blob maps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Placed {
    /// Physical address of its first page.
    pub(crate) phys: u64,
    /// How many bytes.
    pub(crate) len: u64,
    /// The driver's word on caching, from `BLOB_MADE`.
    pub(crate) map_info: u32,
}

/// What a flying request's reply will name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flight {
    /// A submission's `SUBMITTED`, and the command slot the driver may still
    /// be reading, which is given back only then.
    Submit {
        /// Its fence.
        fence: u64,
        /// Its slot.
        slot: usize,
    },
    /// An object's `TRANSFERRED`.
    Transfer(u32),
}

/// What the core knows about one renderer's conversation.
struct State {
    /// Boxed because it is kilobytes of fixed tables, and a `State` is moved
    /// into place on this task's stack.
    session: Box<Session>,
    /// Accepted replies, for the requests waiting on them.
    events: Vec<Event>,
    /// Which description slots are spoken for. A slot is held only while the
    /// driver could still be reading it -- from before the `MAKE_OBJ` is sent
    /// until its reply has been taken.
    describing: [bool; DESCRIBE_SLOTS],
    /// Where the search for the next object id starts.
    next_object: u32,
    /// Which command slots are spoken for, held the same way.
    commanding: [bool; COMMAND_SLOTS],
    /// The next context id and the next fence to hand out.
    next_context: u32,
    next_fence: u64,
    /// Replies nobody is waiting for any more.
    abandoned: Vec<Abandoned>,
    /// Uploads and streams sent and not answered yet.
    flying: Vec<Flying>,
    /// How many have been sent, which numbers the next.
    sent: u64,
    /// How many replies the driver has sent: a request waiting for room --
    /// a command slot, the session's, the channel's, an object that is
    /// moving -- waits for this to change.
    replies: u64,
    /// Whether a refused upload or stream has been reported, which is done
    /// once.
    refusal_said: bool,
    /// Objects a closed open left, not yet asked to go because the driver's
    /// channel was full, and contexts that go once their objects have.
    leaving: Vec<u32>,
    closing: Vec<u32>,
    /// Every capability set a context may be made for, as the device gave
    /// them: fetched once, before the renderer is published, so that
    /// `VIRTGPU_GET_CAPS` wakes nobody.
    caps: Vec<(u32, Vec<u8>)>,
    /// The device's host-visible window, for a driver that makes blobs and a
    /// device that has one.
    window: Option<Places>,
    gone: bool,
}

impl State {
    /// Take a description slot, or `None` when every one is spoken for.
    fn take_describe(&mut self) -> Option<u64> {
        let at = self.describing.iter().position(|held| !held)?;
        *self.describing.get_mut(at)? = true;
        Some(at as u64 * DESCRIBE_BYTES)
    }

    /// Give one back.
    fn give_describe(&mut self, at: u64) {
        if let Some(held) = self.describing.get_mut((at / DESCRIBE_BYTES) as usize) {
            *held = false;
        }
    }

    /// Choose an object id no live object has.
    ///
    /// Ids are handed out in turn rather than reused at once, so that a
    /// driver's late reply about an object names one that is gone rather than
    /// one that has just been made. An id the device refused to let go of
    /// stays tracked, and so is stepped over here for good.
    fn take_object_id(&mut self) -> Option<u32> {
        // One candidate per slot the session has, plus one for the id 0 a
        // wrap steps over: among that many consecutive ids at least one is
        // free whenever the session has room at all.
        for _ in 0..=DESCRIBE_SLOTS + 1 {
            let object = self.next_object;
            self.next_object = self.next_object.checked_add(1).unwrap_or(FIRST_OBJECT);
            if object != 0 && !self.session.holds_object(object) {
                return Some(object);
            }
        }
        None
    }

    /// Take a command slot, or `None` when every one is spoken for.
    fn take_command(&mut self) -> Option<usize> {
        let at = self.commanding.iter().position(|held| !held)?;
        *self.commanding.get_mut(at)? = true;
        Some(at)
    }

    /// Give one back.
    fn give_command(&mut self, at: usize) {
        if let Some(held) = self.commanding.get_mut(at) {
            *held = false;
        }
    }

    /// Record `flight`, sent for `context`, as on its way.
    fn fly(&mut self, flight: Flight, context: u32) {
        let sequence = self.sent;
        self.sent = self.sent.wrapping_add(1);
        self.flying.push(Flying {
            flight,
            context,
            sequence,
        });
    }

    /// Whether anything `context` sent before the `upto`th request is still
    /// on its way.
    fn unsettled(&self, context: u32, upto: u64) -> bool {
        self.flying
            .iter()
            .any(|flying| flying.context == context && flying.sequence < upto)
    }

    /// Ask for what closed opens left behind to go, as far as the driver's
    /// channel has room: objects first, then each context whose objects have
    /// all gone. Whatever is left waits for the next reply to make room.
    fn let_go(&mut self, control: &Endpoint) {
        let mut at = 0;
        while let Some(&object) = self.leaving.get(at) {
            if !control.peer_has_room() {
                return;
            }
            // Only an id the session took the request for is abandoned: one
            // it refused was never asked about, so no reply is coming and
            // recording it would leave an entry nothing ever clears. One
            // whose bytes are still on their way is asked about again after
            // the next reply, which may be the one that says they arrived.
            let message = match self.session.drop_object(object) {
                Ok(message) => message,
                Err(RequestError::Busy) => {
                    at += 1;
                    continue;
                }
                Err(_) => {
                    let _ = self.leaving.remove(at);
                    continue;
                }
            };
            let _ = self.leaving.remove(at);
            self.abandoned.push(Abandoned::Object(object));
            if !send(control, &message) {
                return;
            }
        }
        let mut at = 0;
        while let Some(&context) = self.closing.get(at) {
            if !control.peer_has_room() {
                return;
            }
            match self.session.drop_context(context) {
                Ok(message) => {
                    let _ = self.closing.remove(at);
                    self.abandoned.push(Abandoned::Context(context));
                    if !send(control, &message) {
                        return;
                    }
                }
                // Its objects are still on their way out.
                Err(RequestError::Busy) => at += 1,
                Err(_) => {
                    let _ = self.closing.remove(at);
                }
            }
        }
    }
}

/// A published renderer: `/dev/dri/renderD<index>`.
pub(crate) struct Renderer {
    /// `renderD<index>`, which is 128 upwards as Linux numbers render nodes.
    pub(crate) index: u32,
    /// The work VMO, whose ranges carry object descriptions and command
    /// buffers. The core hands out ranges of it and never reads what is
    /// written there (`docs/GPU.md` §3.3).
    pub(crate) work: Arc<Vmo>,
    /// The device node the renderer is served from, by its index in
    /// `device::devices()`: where sysfs shows it.
    pub(crate) node: usize,
    control: Arc<Endpoint>,
    state: SpinLock<State>,
    changed: Arc<WaitQueue>,
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
/// `renderD<N>`, from 128 as Linux numbers them, and the number a renderer
/// had is the one its driver gets when started again (`crate::claim`).
static NUMBERS: Numbers = Numbers::new(128);
static STARTING: SpinLock<Vec<Start>> = SpinLock::new(Vec::new());
static CLAIMS: Claims = Claims::new();
static RENDERERS: SpinLock<Vec<Arc<Renderer>>> = SpinLock::new(Vec::new());

/// What a quiesce waits out for the renderer.
static SERVER: native::Server = native::Server {
    wait_until_unserved,
    release: None,
};

/// Answer `render_control_create` with the renderer, and have a quiesce wait it out.
///
/// Called once from `main.rs`'s `register_load`: the native ABI is the item's
/// and names no subsystem above it, so this registers into it.
///
/// # Errors
///
/// [`Full`] when the item has no room for the registration.
pub(crate) fn install() -> Result<(), Full> {
    native::serve(NativeCall::RenderControlCreate, control_create)?;
    native::register_server(&SERVER)
}

/// `render_control_create`.
///
/// As the display's, for the other of a card's two conversations: the
/// device's own channel, one per device, and the driver's end of it back. The device handle and its `MANAGE` right are the item's to
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

/// Make the control channel for `node` and start its task; answer the
/// driver's end.
pub(crate) fn create(node: &Arc<DeviceNode>) -> Result<Arc<Endpoint>, CreateError> {
    let (kernel_end, driver_end) = Endpoint::pair().map_err(|_| CreateError::NoMemory)?;
    if !CLAIMS.claim(node, &kernel_end) {
        return Err(CreateError::InUse);
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    STARTING.lock().push(Start {
        id,
        control: kernel_end,
        device: Arc::clone(node),
        location: crate::interfaces::block_ring::location_of(node),
    });
    if sched::spawn("render", run, id, ferrix_sched::NICE_0_WEIGHT).is_err() {
        let _ = take_start(id);
        CLAIMS.release(node);
        return Err(CreateError::NoMemory);
    }
    Ok(driver_end)
}

/// Wait until no render driver's channel claims `node`, for a quiesce
/// (`crate::claim`).
///
/// # Errors
///
/// [`StillServed`].
pub(crate) fn wait_until_unserved(
    node: &Arc<DeviceNode>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), StillServed> {
    CLAIMS.wait_until_released(node, cancelled)
}

fn take_start(id: usize) -> Option<Start> {
    let mut starting = STARTING.lock();
    let at = starting.iter().position(|start| start.id == id)?;
    Some(starting.remove(at))
}

/// The numbers of every published renderer, lowest first.
pub(crate) fn renderer_indices() -> Vec<u32> {
    let mut indices: Vec<u32> = RENDERERS
        .lock()
        .iter()
        .map(|renderer| renderer.index)
        .collect();
    indices.sort_unstable();
    indices
}

/// The renderer published as `renderD<index>`, if its driver is serving it.
pub(crate) fn renderer(index: u32) -> Option<Arc<Renderer>> {
    RENDERERS
        .lock()
        .iter()
        .find(|renderer| renderer.index == index)
        .map(Arc::clone)
}

/// Render node numbers lent to the chardev core, whose driver decodes its
/// node's ioctls itself (`docs/NVIDIA.md` §4.4, N3b), each with the device
/// node it is served from, by its index in `device::devices()`.
static LENT: SpinLock<Vec<(u32, usize)>> = SpinLock::new(Vec::new());

/// A render node number for a node another core serves for the device at
/// `device`, taken from the same numbers as this core's renderers, so two
/// `renderD<N>` never collide whichever driver started first (the
/// consultant's B8, ledger 316). `None` when there was no memory to hold it.
pub(crate) fn lend_number(device: usize) -> Option<u32> {
    let mut lent = LENT.lock();
    lent.try_reserve(1).ok()?;
    let index = NUMBERS.take()?;
    // NOALLOC: reserved above.
    lent.push((index, device));
    Some(index)
}

/// Give back a number [`lend_number`] lent.
pub(crate) fn return_number(index: u32) {
    LENT.lock().retain(|(held, _)| *held != index);
    NUMBERS.give_back(index);
}

/// The numbers of every render node, this core's and the lent, lowest
/// first: what `/dev/dri` and sysfs list.
pub(crate) fn node_indices() -> Vec<u32> {
    let mut indices = renderer_indices();
    let lent = LENT.lock();
    if indices.try_reserve(lent.len()).is_ok() {
        indices.extend(lent.iter().map(|(index, _)| *index));
    }
    drop(lent);
    indices.sort_unstable();
    indices
}

/// The device node `renderD<index>` is served from, by its index in
/// `device::devices()`, whichever core serves it.
pub(crate) fn node_device(index: u32) -> Option<usize> {
    if let Some(renderer) = renderer(index) {
        return Some(renderer.node);
    }
    LENT.lock()
        .iter()
        .find(|(held, _)| *held == index)
        .map(|(_, device)| *device)
}

/// One renderer's task.
///
/// The proof runs before the renderer is published, so nothing can open the
/// node and take the session while the core is still asking its own
/// questions; then the renderer is served until its driver goes.
fn run(id: usize) {
    let Some(start) = take_start(id) else {
        return;
    };
    if let Some(renderer) = take_up(&start) {
        prove(&renderer);
        RENDERERS.lock().push(Arc::clone(&renderer));
        serve(&renderer);
        RENDERERS
            .lock()
            .retain(|held| !Arc::ptr_eq(held, &renderer));
        NUMBERS.give_back(renderer.index);
        crate::console::println!("  render   renderD{} is gone", renderer.index);
    }
    CLAIMS.release(&start.device);
}

/// The next message on the control channel, waiting up to `deadline`.
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

fn send(control: &Endpoint, message: &Message) -> bool {
    send_with(control, message, Vec::new())
}

/// [`send`] for a message that hands something over.
fn send_with(control: &Endpoint, message: &Message, handed: Vec<Transfer>) -> bool {
    let bytes = message.encode().as_bytes().to_vec();
    control
        .write(bytes, handed.len(), || {
            Ok::<Vec<Transfer>, Infallible>(handed)
        })
        .is_ok()
}

fn refuse(control: &Endpoint, refusal: Refusal) {
    let _ = send(control, &Message::Refused(refusal));
}

/// Take a HELLO, refusing it or answering READY.
fn take_up(start: &Start) -> Option<Arc<Renderer>> {
    let deadline = timer::now_nanos().saturating_add(HELLO_PATIENCE_NANOS);
    let message = receive(&start.control, deadline)?;
    match accept(start, &message) {
        Ok(renderer) => Some(renderer),
        Err(refusal) => {
            crate::console::println!("  render   a driver was refused: {refusal}");
            refuse(&start.control, refusal);
            None
        }
    }
}

fn accept(start: &Start, message: &ChannelMessage) -> Result<Arc<Renderer>, Refusal> {
    let Some(Message::Hello(hello)) = Message::decode(&message.bytes) else {
        return Err(Refusal::Malformed);
    };
    let rights: Vec<Rights> = message.handles.iter().map(|(_, rights)| *rights).collect();
    let session = Box::new(Session::accept(&hello, &rights, WORK_BYTES)?);
    // The window is the kernel's, found when the device was enumerated; a
    // driver that makes no blobs has no use for it.
    let window = start
        .device
        .host_visible()
        .filter(|_| hello.features & features::BLOBS != 0)
        .map(|window| Places::new(window.phys, window.len));
    let Some((Object::Port(_driver_port), _)) = message.handles.first() else {
        return Err(Refusal::Rights);
    };
    if start.location.map(Location::raw) != Some(hello.location) {
        return Err(Refusal::WrongLocation);
    }
    let index = NUMBERS.take().ok_or(Refusal::Malformed)?;
    // READY carries the two handles `Ready::HANDLE_RIGHTS` fixes: the work
    // VMO, which is where an object's description and a command buffer live
    // on their way to the device, and the core's port, which is how a driver
    // will say a fence has passed. The VMO is the core's: it hands out ranges
    // of it and never reads what is written there (`docs/GPU.md` §3.3), and
    // it keeps its own reference because the node writes those ranges.
    // The wire protocol has no refusal for memory; a malformed start is the
    // nearest it has.
    let work = Vmo::new_anonymous(WORK_BYTES / PAGE_SIZE).map_err(|_| Refusal::Malformed)?;
    let core_port = Port::new().map_err(|_| Refusal::Malformed)?;
    let renderer = Arc::new(Renderer {
        index,
        node: start.device.index(),
        work: Arc::clone(&work),
        control: Arc::clone(&start.control),
        state: SpinLock::new(State {
            session,
            events: Vec::new(),
            describing: [false; DESCRIBE_SLOTS],
            next_object: FIRST_OBJECT,
            commanding: [false; COMMAND_SLOTS],
            next_context: 1,
            next_fence: 1,
            abandoned: Vec::new(),
            flying: Vec::new(),
            sent: 0,
            replies: 0,
            refusal_said: false,
            leaving: Vec::new(),
            closing: Vec::new(),
            caps: Vec::new(),
            window,
            gone: false,
        }),
        changed: Arc::new(WaitQueue::new()),
    });
    let ready = Message::Ready(Ready {
        renderer: index,
        work_bytes: WORK_BYTES,
    })
    .encode()
    .as_bytes()
    .to_vec();
    let handed = vec![
        (Object::Vmo(work), WORK_VMO_RIGHTS),
        (Object::Port(core_port), Rights::WRITE),
    ];
    if start
        .control
        .write(ready, 2, || Ok::<Vec<Transfer>, Infallible>(handed))
        .is_err()
    {
        NUMBERS.give_back(index);
        return Err(Refusal::Malformed);
    }
    let state = renderer.state.lock();
    crate::console::println!(
        "  render   renderD{index} is `{}`, version {VERSION}, capsets {:#x}, objects to {} MiB, \
         window {} MiB",
        state.session.name(),
        state.session.capsets(),
        state.session.object_limit() / (1024 * 1024),
        state
            .window
            .as_ref()
            .map_or(0, |window| window.len / (1024 * 1024)),
    );
    drop(state);
    Ok(renderer)
}

impl Renderer {
    /// Whether the driver is gone.
    pub(crate) fn is_gone(&self) -> bool {
        self.state.lock().gone
    }

    /// What the driver calls itself: `virtio_gpu` here, something else for
    /// the card §4 describes.
    ///
    /// Copied out rather than borrowed, because the name lives under the
    /// lock and `DRM_IOCTL_VERSION` copies it to a program afterwards.
    pub(crate) fn name(&self) -> alloc::string::String {
        alloc::string::String::from(self.state.lock().session.name())
    }

    /// Which capability sets a context may be made for, a bit per set, as
    /// the driver's HELLO gave them.
    pub(crate) fn capsets(&self) -> u32 {
        self.state.lock().session.capsets()
    }

    /// The capability set a context is made for when its program names
    /// none: the lowest the driver offers, which is the one its streams have
    /// always been in -- virgl's here -- and 0 for a device with none.
    pub(crate) fn default_capset(&self) -> u32 {
        let capsets = self.capsets();
        if capsets == 0 {
            0
        } else {
            capsets.trailing_zeros()
        }
    }

    /// Whether the device has a window for blobs to be mapped through.
    pub(crate) fn has_window(&self) -> bool {
        self.state.lock().window.is_some()
    }

    /// Whether the driver fences a submission on a ring.
    pub(crate) fn has_rings(&self) -> bool {
        self.state.lock().session.features() & features::RINGS != 0
    }

    /// What `stat` says of the render node: a character device of major 226
    /// whose minor is its number, as Linux numbers `renderD128` 226:128.
    ///
    /// Its size is zero: unlike a card, whose one VMO is what
    /// `MODE_MAP_DUMB`'s offsets are into, each object here has a backing of
    /// its own and the node's offsets name an object, not a place in a file.
    pub(crate) fn metadata(&self) -> ferrix_vfs::Metadata {
        use ferrix_vfs::{FileType, Metadata, Timespec};
        Metadata {
            ino: (1u64 << 41) + u64::from(self.index),
            kind: FileType::CharDevice,
            // Owner and group only, as Linux's `render` group has it.
            permissions: 0o660,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: ferrix_vfs::initramfs::makedev(crate::interfaces::display::DRM_MAJOR, self.index),
            blocks: 0,
            block_size: 4096,
            atime: Timespec::default(),
            mtime: Timespec::default(),
            ctime: Timespec::default(),
        }
    }

    /// Run `make` on the session and send the message it makes, with the
    /// state locked throughout.
    fn request<T>(
        &self,
        make: impl FnOnce(&mut State) -> Result<(Message, T), RenderError>,
    ) -> Result<T, RenderError> {
        self.request_with(Vec::new(), make)
    }

    /// [`Renderer::request`] for a message that hands `handed` over with it.
    fn request_with<T>(
        &self,
        handed: Vec<Transfer>,
        make: impl FnOnce(&mut State) -> Result<(Message, T), RenderError>,
    ) -> Result<T, RenderError> {
        let mut state = self.state.lock();
        if state.gone || self.control.peer_closed() {
            return Err(RenderError::Gone);
        }
        // Before the session commits to the request, which it cannot take
        // back: a driver that is behind makes the caller try again, rather
        // than leaving the session waiting for a reply to a message that was
        // never written.
        if !self.control.peer_has_room() {
            return Err(RenderError::Busy);
        }
        let (message, made) = make(&mut state)?;
        if !send_with(&self.control, &message, handed) {
            return Err(RenderError::Gone);
        }
        drop(state);
        Ok(made)
    }

    /// Wait for the event `wanted` picks out, and take it. On a timeout,
    /// `abandon` runs with the state locked, so the reply is dealt with
    /// whenever it comes.
    fn collect(
        &self,
        wanted: impl Fn(&Event) -> bool,
        abandon: impl FnOnce(&mut State),
    ) -> Result<Event, RenderError> {
        let deadline = timer::now_nanos().saturating_add(REPLY_PATIENCE_NANOS);
        let _ = self.changed.wait_until_deadline(
            || {
                let state = self.state.lock();
                state.gone || state.events.iter().any(&wanted)
            },
            deadline,
        );
        let mut state = self.state.lock();
        if let Some(at) = state.events.iter().position(&wanted) {
            return Ok(state.events.remove(at));
        }
        if state.gone {
            return Err(RenderError::Gone);
        }
        abandon(&mut state);
        Err(RenderError::TimedOut)
    }

    /// Make a context on the device for `capset`, and answer the id it was
    /// given.
    ///
    /// A context is an open's own: what one program draws, and the objects
    /// it may name, are apart from every other's. The capability set is the
    /// one its program asked for, or the default.
    ///
    /// # Errors
    ///
    /// [`RenderError`].
    pub(crate) fn make_context(&self, capset: u32) -> Result<u32, RenderError> {
        let context = self.request(|state| {
            // The session refuses an id it holds, so the next one it does
            // not is found by asking: there are few contexts and the ids
            // are dense.
            for _ in 0..=ferrix_renderctl::session::MAX_CONTEXTS {
                let context = state.next_context;
                state.next_context = state.next_context.checked_add(1).unwrap_or(1);
                match state.session.make_context(context, capset) {
                    Ok(message) => return Ok((message, context)),
                    Err(RequestError::InUse | RequestError::ZeroId) => {}
                    Err(other) => return Err(RenderError::Request(other)),
                }
            }
            Err(RenderError::Request(RequestError::Full))
        })?;
        let event = self.collect(
            |event| matches!(event, Event::ContextMade { context: made, .. } if *made == context),
            |state| state.abandoned.push(Abandoned::Context(context)),
        )?;
        match event {
            Event::ContextMade {
                status: Status::Ok, ..
            } => Ok(context),
            Event::ContextMade { status, .. } => Err(RenderError::Refused(status)),
            _ => Err(RenderError::Gone),
        }
    }

    /// Capability set `capset`'s bytes, as the device gave them, if a
    /// context may be made for it.
    pub(crate) fn caps(&self, capset: u32) -> Option<Vec<u8>> {
        let state = self.state.lock();
        state
            .caps
            .iter()
            .find(|(held, _)| *held == capset)
            .map(|(_, bytes)| bytes.clone())
    }

    /// Make a blob of `bytes` in `context`, and answer the id it was given
    /// and, for a `mappable` one, where it is in the window.
    ///
    /// `memory`, `blob_flags` and `blob_id` are the program's words and the
    /// driver's business (`docs/GPU.md` §3.3). A mappable blob is placed in
    /// the window before it is asked for, so the driver can have the device
    /// map it as it makes it, as Linux does; the place is given back if the
    /// device makes nothing.
    ///
    /// # Errors
    ///
    /// [`RenderError`]; [`RequestError::Unsupported`] for a mappable blob on
    /// a device with no window, and [`RequestError::Full`] for one the
    /// window has no room for.
    pub(crate) fn make_blob(
        &self,
        context: u32,
        memory: u32,
        blob_flags: u32,
        blob_id: u64,
        bytes: u64,
        mappable: bool,
    ) -> Result<(u32, Option<Placed>), RenderError> {
        let (object, place) = {
            let mut state = self.state.lock();
            if state.gone {
                return Err(RenderError::Gone);
            }
            let object = state
                .take_object_id()
                .ok_or(RenderError::Request(RequestError::Full))?;
            let place = if mappable {
                let window = state
                    .window
                    .as_mut()
                    .ok_or(RenderError::Request(RequestError::Unsupported))?;
                let offset = window
                    .take(object, bytes)
                    .ok_or(RenderError::Request(RequestError::Full))?;
                Some((window.phys + offset, offset))
            } else {
                None
            };
            (object, place)
        };
        let give_back = |state: &mut State| {
            if let Some(window) = state.window.as_mut() {
                window.settle(object, true);
            }
        };
        let asked = self.request(|state| {
            let message = state
                .session
                .make_blob(MakeBlob {
                    object,
                    context,
                    memory,
                    flags: blob_flags,
                    blob_id,
                    bytes,
                    window: place.map_or(NO_WINDOW, |(_, offset)| offset),
                })
                .map_err(RenderError::Request)?;
            Ok((message, ()))
        });
        if let Err(error) = asked {
            give_back(&mut self.state.lock());
            return Err(error);
        }
        // A blob the driver answers late keeps its place for good: nothing
        // here knows whether the device mapped it.
        let event = self.collect(
            |event| matches!(event, Event::BlobMade { object: made, .. } if *made == object),
            |state| state.abandoned.push(Abandoned::Object(object)),
        )?;
        match event {
            Event::BlobMade {
                status: Status::Ok,
                map_info,
                ..
            } => Ok((
                object,
                place.map(|(phys, _)| Placed {
                    phys,
                    len: bytes,
                    map_info,
                }),
            )),
            Event::BlobMade { status, .. } => {
                give_back(&mut self.state.lock());
                Err(RenderError::Refused(status))
            }
            _ => Err(RenderError::Gone),
        }
    }

    /// Whether the submission `fence` names has been answered: for one on a
    /// ring, whether its work has finished. What a fence descriptor polls.
    pub(crate) fn fence_done(&self, fence: u64) -> bool {
        let state = self.state.lock();
        state.gone
            || !state.flying.iter().any(
                |held| matches!(held.flight, Flight::Submit { fence: kept, .. } if kept == fence),
            )
    }

    /// The queue woken whenever the driver answers anything, which is what a
    /// fence descriptor's waiters sleep on.
    pub(crate) fn changed(&self) -> &Arc<WaitQueue> {
        &self.changed
    }

    /// Make an object of `bytes` on the device, described by `words`, and
    /// answer the id it was given and the backing it was made with.
    ///
    /// A [`flags::MAPPABLE`] object gets a VMO of whole pages, which is the
    /// core's: it is what a program maps through the node, and the driver is
    /// handed it only to pin for the device. It lives as long as either side
    /// holds it, so an object the device would not let go of keeps its pages
    /// through the driver's pin and nothing here has to remember that.
    ///
    /// `words` is the shape the caller asked for, which
    /// goes into the work VMO untouched: this side chooses how many bytes and
    /// where the description is, and the driver is the only side that knows
    /// what the words say (`docs/GPU.md` §3.3). A core that read them would
    /// be a core §4's driver could not reuse.
    ///
    /// # Errors
    ///
    /// [`RenderError`], including the device's own refusal as
    /// [`RenderError::Request`] is not: a device that says no leaves nothing
    /// tracked.
    pub(crate) fn make_object(
        &self,
        context: u32,
        bytes: u64,
        object_flags: u32,
        words: [u32; DESCRIBE_WORDS],
    ) -> Result<(u32, Option<Arc<Vmo>>), RenderError> {
        // The id and the slot are taken together, under the lock; the write
        // that fills the slot happens after it, because `Vmo::write_page` may
        // wait for a shootdown and no spin lock is ever held across that.
        let (object, at) = {
            let mut state = self.state.lock();
            if state.gone {
                return Err(RenderError::Gone);
            }
            let object = state
                .take_object_id()
                .ok_or(RenderError::Request(RequestError::Full))?;
            let Some(at) = state.take_describe() else {
                return Err(RenderError::Request(RequestError::Full));
            };
            (object, at)
        };
        let Ok(backing) = (object_flags & flags::MAPPABLE != 0)
            .then(|| Vmo::new_anonymous(bytes.div_ceil(PAGE_SIZE)))
            .transpose()
        else {
            // The session has no error for memory; a full table is the
            // nearest, and the slot goes back as it would on any refusal.
            self.state.lock().give_describe(at);
            return Err(RenderError::Request(RequestError::Full));
        };
        let made = self.describe_and_make(
            object,
            at,
            context,
            bytes,
            object_flags,
            words,
            backing.as_ref(),
        );
        self.state.lock().give_describe(at);
        made.map(|object| (object, backing))
    }

    /// [`Renderer::make_object`] once its id and slot are in hand, so that the
    /// slot is given back by one line whichever way this goes.
    #[expect(
        clippy::too_many_arguments,
        reason = "one request's fields, passed once"
    )]
    fn describe_and_make(
        &self,
        object: u32,
        at: u64,
        context: u32,
        bytes: u64,
        object_flags: u32,
        words: [u32; DESCRIBE_WORDS],
        backing: Option<&Arc<Vmo>>,
    ) -> Result<u32, RenderError> {
        let mut description = [0_u8; DESCRIBE_BYTES as usize];
        for (word, into) in words.iter().zip(description.chunks_exact_mut(4)) {
            into.copy_from_slice(&word.to_le_bytes());
        }
        self.work
            .write_page(at / PAGE_SIZE, (at % PAGE_SIZE) as usize, &description)
            .map_err(|_| RenderError::Request(RequestError::Work))?;
        let describe = Work {
            at: u32::try_from(at).map_err(|_| RenderError::Request(RequestError::Work))?,
            len: (DESCRIBE_WORDS * 4) as u32,
        };
        let handed = backing
            .map(|vmo| (Object::Vmo(Arc::clone(vmo)), BACKING_RIGHTS))
            .into_iter()
            .collect();
        self.request_with(handed, |state| {
            let message = state
                .session
                .make_object(object, context, bytes, object_flags, describe)
                .map_err(RenderError::Request)?;
            Ok((message, ()))
        })?;
        let event = self.collect(
            |event| matches!(event, Event::ObjectMade { object: made, .. } if *made == object),
            // The driver may still answer. Nothing waits for that reply, so
            // it is dropped rather than left in `events` for ever. The
            // description slot is given back even so: a driver reads a
            // description while it is making the object, which is before the
            // reply this gave up on, so a late answer is never a late read.
            |state| state.abandoned.push(Abandoned::Object(object)),
        )?;
        match event {
            Event::ObjectMade {
                status: Status::Ok, ..
            } => Ok(object),
            // A refusal is the device's answer and leaves the id free, which
            // is not the same as a driver that has gone.
            Event::ObjectMade { status, .. } => Err(RenderError::Refused(status)),
            _ => Err(RenderError::Gone),
        }
    }

    /// Make `object` nameable in `context`, which imported it as a dmabuf
    /// (`docs/GPU.md` §3.13), and wait until the driver says it is.
    ///
    /// One attach an object at a time, as the session keeps them; a second
    /// waits for the first's answer. Attaching to a context that has the
    /// object already is answered as the first was: the device takes it.
    ///
    /// # Errors
    ///
    /// [`RenderError`]; a refusal is the device's, and the import fails.
    pub(crate) fn attach(&self, object: u32, context: u32) -> Result<(), RenderError> {
        let deadline = timer::now_nanos().saturating_add(REPLY_PATIENCE_NANOS);
        loop {
            let seen = self.replies();
            let sent = self.request(|state| {
                let message = state
                    .session
                    .attach_object(object, context)
                    .map_err(RenderError::Request)?;
                Ok((message, ()))
            });
            match sent {
                Err(
                    RenderError::Busy
                    | RenderError::Request(RequestError::Full | RequestError::InUse),
                ) => self.wait_for_reply(seen, deadline)?,
                Err(error) => return Err(error),
                Ok(()) => break,
            }
        }
        let event = self.collect(
            |event| matches!(event, Event::ObjectAttached { object: given, .. } if *given == object),
            |state| state.abandoned.push(Abandoned::Attach(object)),
        )?;
        match event {
            Event::ObjectAttached {
                status: Status::Ok, ..
            } => Ok(()),
            Event::ObjectAttached { status, .. } => Err(RenderError::Refused(status)),
            _ => Err(RenderError::Gone),
        }
    }

    /// Move bytes between an object's backing and the device's copy of it.
    ///
    /// To the device, this returns once the request is sent, and the caller
    /// waits with [`Renderer::settle`] before it writes the backing again;
    /// from the device, it waits until the bytes are there, since a caller
    /// asks in order to read them. Either waits first for room, and for the
    /// object's last transfer to be answered: one at a time an object.
    ///
    /// # Errors
    ///
    /// [`RenderError`]; the session refuses an object with no backing.
    pub(crate) fn transfer(&self, transfer: Move) -> Result<(), RenderError> {
        let object = transfer.object;
        let flying = transfer.direction == Direction::ToDevice;
        let deadline = timer::now_nanos().saturating_add(REPLY_PATIENCE_NANOS);
        loop {
            let seen = self.replies();
            let sent = self.request(|state| {
                let message = state
                    .session
                    .transfer(transfer)
                    .map_err(RenderError::Request)?;
                if flying {
                    state.fly(Flight::Transfer(object), transfer.context);
                }
                Ok((message, ()))
            });
            match sent {
                Err(
                    RenderError::Busy
                    | RenderError::Request(RequestError::Full | RequestError::InUse),
                ) => self.wait_for_reply(seen, deadline)?,
                Err(error) => return Err(error),
                Ok(()) if flying => return Ok(()),
                Ok(()) => break,
            }
        }
        let event = self.collect(
            |event| matches!(event, Event::Transferred { object: moved, .. } if *moved == object),
            |state| state.abandoned.push(Abandoned::Transfer(object)),
        )?;
        match event {
            Event::Transferred {
                status: Status::Ok, ..
            } => Ok(()),
            Event::Transferred { status, .. } => Err(RenderError::Refused(status)),
            _ => Err(RenderError::Gone),
        }
    }

    /// Run `commands` in `context`: send them, and return the fence the
    /// driver will answer. On a `ring` other than
    /// [`ferrix_renderctl::message::NO_RING`], the answer comes when the work
    /// has finished, which is what [`Renderer::fence_done`] then says.
    ///
    /// The bytes are the renderer's own language and go into a slot of the
    /// work VMO untouched (`docs/GPU.md` §3.3), where the driver has the
    /// device read them. The slot is held until the driver has answered,
    /// because until then the device may still be reading it; with every
    /// slot held, this waits for an answer.
    ///
    /// # Errors
    ///
    /// [`RenderError`]; [`RequestError::Work`] for a command buffer of no
    /// bytes or more than [`COMMAND_BYTES`].
    pub(crate) fn submit(
        &self,
        context: u32,
        ring: u32,
        commands: &[u8],
    ) -> Result<u64, RenderError> {
        if commands.is_empty() || commands.len() as u64 > COMMAND_BYTES {
            return Err(RenderError::Request(RequestError::Work));
        }
        let deadline = timer::now_nanos().saturating_add(REPLY_PATIENCE_NANOS);
        let slot = loop {
            let seen = {
                let mut state = self.state.lock();
                if state.gone {
                    return Err(RenderError::Gone);
                }
                if let Some(slot) = state.take_command() {
                    break slot;
                }
                state.replies
            };
            self.wait_for_reply(seen, deadline)?;
        };
        let sent = self.write_and_send(context, ring, commands, slot, deadline);
        // A sent submission's slot is given back with its reply.
        if sent.is_err() {
            self.state.lock().give_command(slot);
        }
        sent
    }

    /// [`Renderer::submit`] once its slot is in hand.
    fn write_and_send(
        &self,
        context: u32,
        ring: u32,
        commands: &[u8],
        slot: usize,
        deadline: u64,
    ) -> Result<u64, RenderError> {
        let at = DESCRIBE_REGION + slot as u64 * COMMAND_BYTES;
        for (index, chunk) in commands.chunks(PAGE_SIZE as usize).enumerate() {
            // A slot starts on a page boundary, so a chunk is a page's.
            self.work
                .write_page(at / PAGE_SIZE + index as u64, 0, chunk)
                .map_err(|_| RenderError::Request(RequestError::Work))?;
        }
        let range = Work {
            at: u32::try_from(at).map_err(|_| RenderError::Request(RequestError::Work))?,
            len: commands.len() as u32,
        };
        loop {
            let seen = self.replies();
            let sent = self.request(|state| {
                let fence = state.next_fence;
                let message = state
                    .session
                    .submit(context, ring, fence, range)
                    .map_err(RenderError::Request)?;
                state.next_fence = fence.wrapping_add(1).max(1);
                state.fly(Flight::Submit { fence, slot }, context);
                Ok((message, fence))
            });
            match sent {
                Err(RenderError::Busy | RenderError::Request(RequestError::Full)) => {
                    self.wait_for_reply(seen, deadline)?;
                }
                other => return other,
            }
        }
    }

    /// Wait until every upload and stream `context` sent before this has
    /// been answered: `VIRTGPU_WAIT`, which a program calls before it writes
    /// a backing the device may still be reading.
    ///
    /// # Errors
    ///
    /// [`RenderError::TimedOut`], and [`RenderError::Gone`] for a driver
    /// that went with them unanswered.
    pub(crate) fn settle(&self, context: u32) -> Result<(), RenderError> {
        let deadline = timer::now_nanos().saturating_add(REPLY_PATIENCE_NANOS);
        let upto = self.state.lock().sent;
        let _ = self.changed.wait_until_deadline(
            || {
                let state = self.state.lock();
                state.gone || !state.unsettled(context, upto)
            },
            deadline,
        );
        let state = self.state.lock();
        if !state.unsettled(context, upto) {
            Ok(())
        } else if state.gone {
            Err(RenderError::Gone)
        } else {
            Err(RenderError::TimedOut)
        }
    }

    /// Whether anything `context` has sent is still on its way.
    pub(crate) fn is_settled(&self, context: u32) -> bool {
        !self.state.lock().unsettled(context, u64::MAX)
    }

    /// How many replies the driver has sent so far.
    fn replies(&self) -> u64 {
        self.state.lock().replies
    }

    /// Sleep until the driver has sent a reply since it had sent `seen`,
    /// which is what makes room for a request that found none.
    fn wait_for_reply(&self, seen: u64, deadline: u64) -> Result<(), RenderError> {
        let _ = self.changed.wait_until_deadline(
            || {
                let state = self.state.lock();
                state.gone || state.replies != seen
            },
            deadline,
        );
        let state = self.state.lock();
        if state.gone {
            Err(RenderError::Gone)
        } else if state.replies != seen {
            Ok(())
        } else {
            Err(RenderError::TimedOut)
        }
    }

    /// Let go of `objects`, and then of `context`, without waiting for the
    /// device to answer.
    ///
    /// The same bargain [`crate::interfaces::display::Card::release`] makes when a card's
    /// open closes: a close does not wait on a device, so the replies go to
    /// [`serve`], which drops them. An object the device would not let go of
    /// stays tracked and its id is never handed out again. What the driver's
    /// channel has no room for now is asked for as its replies make room.
    pub(crate) fn release(&self, objects: &[u32], context: Option<u32>) {
        let mut state = self.state.lock();
        if state.gone {
            return;
        }
        state.leaving.extend_from_slice(objects);
        state.closing.extend(context);
        state.let_go(&self.control);
    }
}

/// Take every reply the driver sends, judge it and leave it where the
/// request waiting for it will find it, until the driver goes.
fn serve(renderer: &Renderer) {
    loop {
        if renderer.state.lock().gone {
            break;
        }
        let deadline = timer::now_nanos().saturating_add(RECHECK_NANOS);
        let message = match renderer.control.read(MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => message,
            Err(ReadError::Empty) => {
                if renderer.control.signals().intersects(Signals::PEER_CLOSED) {
                    break;
                }
                renderer.state.lock().let_go(&renderer.control);
                let _ = renderer.control.waiters().wait_until_deadline(
                    || {
                        renderer
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
        // A reply carries no handles; anything sent with one is disposed of
        // rather than leaked, as the display's task does.
        crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
        let Some(decoded) = Message::decode(&message.bytes) else {
            refuse(&renderer.control, Refusal::Protocol);
            break;
        };
        let accepted = renderer.state.lock().session.receive(&decoded);
        match accepted {
            Ok(Event::Stopped) => break,
            Ok(event) => {
                let mut state = renderer.state.lock();
                state.replies = state.replies.wrapping_add(1);
                // A blob's place in the window is free once the device has
                // unmapped it, and never if it would not.
                if let Event::ObjectGone { object, status } = event
                    && let Some(window) = state.window.as_mut()
                {
                    window.settle(object, status == Status::Ok);
                }
                // A reply is room in the driver's channel, and an object
                // that has gone may be the last its context was waiting for.
                state.let_go(&renderer.control);
                // An upload or a stream whose caller was answered when it
                // was sent: its slot comes back, and a `WAIT` may be over.
                if let Some(at) = flying_at(&state, event) {
                    let flying = state.flying.remove(at);
                    if let Flight::Submit { slot, .. } = flying.flight {
                        state.give_command(slot);
                    }
                    let refused = matches!(
                        event,
                        Event::Transferred { status, .. } | Event::Submitted { status, .. }
                            if status != Status::Ok
                    );
                    let say = refused && !core::mem::replace(&mut state.refusal_said, true);
                    drop(state);
                    if say {
                        crate::console::println!(
                            "  render   renderD{}: the device refused an upload or a command stream \
                             nobody waits for; later ones go unsaid",
                            renderer.index
                        );
                    }
                    renderer.changed.wake_all();
                    continue;
                }
                // The session has settled the id either way; what is left is
                // whether anyone is still waiting to be told. An abandoned
                // one is dropped here rather than growing `events` for ever.
                if let Some(at) = abandoned_at(&state, event) {
                    let _ = state.abandoned.remove(at);
                    drop(state);
                    // Room, for whoever is waiting for some.
                    renderer.changed.wake_all();
                    continue;
                }
                state.events.push(event);
                drop(state);
                renderer.changed.wake_all();
            }
            Err(refusal) => {
                crate::console::println!("  render   renderD{}: {refusal}", renderer.index);
                refuse(&renderer.control, refusal);
                break;
            }
        }
    }
    renderer.state.lock().gone = true;
    renderer.changed.wake_all();
}

/// Make one context and take it away again, which is the whole of what the
/// core does with a renderer until the node above it exists.
///
/// It is not ceremony: it is the only thing that says the conversation
/// works end to end against a real device, and it is what the boot log
/// reports. `docs/GPU.md` §3.2 -- the commands under it were tested against
/// QEMU's header, and this is them against QEMU.
///
/// It runs before the renderer is published, so it has the session to
/// itself and can read the channel directly rather than through [`serve`].
fn prove(renderer: &Renderer) {
    let context = 1;
    let capset = renderer.default_capset();
    let Ok(ask) = renderer.state.lock().session.make_context(context, capset) else {
        return;
    };
    match exchange(renderer, &ask) {
        Some(Event::ContextMade {
            status: Status::Ok, ..
        }) => {
            crate::console::println!(
                "  render   renderD{} made context {context} on the device",
                renderer.index
            );
            prove_object(renderer, context);
            prove_caps(renderer);
            prove_context_goes(renderer, context);
        }
        Some(Event::ContextMade { status, .. }) => {
            crate::console::println!("  render   the device refused a context: {status:?}");
        }
        _ => {}
    }
}

/// How big the object the proof asks for is: one page, which is enough to be
/// a real resource on the device and small enough to cost nothing.
const PROOF_OBJECT_BYTES: u64 = PAGE_SIZE;

/// Make an object in `context` and take it away again.
///
/// This is `prove`'s second half and the rest of `docs/GPU.md` step 1:
/// `RESOURCE_CREATE_3D` against the device rather than against QEMU's
/// header. The description is **empty** -- `Work { at: 0, len: 0 }` -- and
/// that is the point of the seam: the core says how many bytes it wants and
/// nothing about what the resource is, and the driver, which is the only
/// side that knows virgl, chooses the target, format and bind words. A core
/// that wrote them would be a core an NVIDIA driver could not reuse.
fn prove_object(renderer: &Renderer, context: u32) {
    // The first id a program's object would get, and for its reason: the
    // device's numbers are the display's too.
    let object = FIRST_OBJECT;
    let Ok(ask) = renderer.state.lock().session.make_object(
        object,
        context,
        PROOF_OBJECT_BYTES,
        flags::TO_DEVICE,
        Work { at: 0, len: 0 },
    ) else {
        return;
    };
    match exchange(renderer, &ask) {
        Some(Event::ObjectMade {
            status: Status::Ok, ..
        }) => {
            crate::console::println!(
                "  render   renderD{} made object {object} of {PROOF_OBJECT_BYTES} bytes in context {context}",
                renderer.index
            );
        }
        Some(Event::ObjectMade { status, .. }) => {
            crate::console::println!("  render   the device refused an object: {status:?}");
            return;
        }
        _ => return,
    }
    let Ok(ask) = renderer.state.lock().session.drop_object(object) else {
        return;
    };
    match exchange(renderer, &ask) {
        Some(Event::ObjectGone {
            status: Status::Ok, ..
        }) => {
            crate::console::println!(
                "  render   renderD{} gave object {object} back",
                renderer.index
            );
        }
        Some(Event::ObjectGone { status, .. }) => {
            // The device may still hold the backing, so the core keeps the
            // memory rather than handing it out again: `docs/DISPLAY.md`
            // §2.2's rule, which belongs to the core and not to virtio.
            crate::console::println!("  render   the device kept an object: {status:?}");
        }
        _ => {}
    }
}

/// Fetch every capability set a context may be made for, and keep them.
///
/// Asked once, here, while the core still has the channel to itself: a set
/// is a property of the device and does not change, and a reply that brings
/// a handle is one [`serve`] has no business with. Version 0 asks for the
/// newest the device has.
fn prove_caps(renderer: &Renderer) {
    let capsets = renderer.capsets();
    for capset in (1..u32::BITS).filter(|capset| capsets & (1 << capset) != 0) {
        prove_capset(renderer, capset);
    }
}

/// Fetch one capability set, and keep it.
fn prove_capset(renderer: &Renderer, capset: u32) {
    let Ok(ask) = renderer.state.lock().session.get_caps(capset, 0) else {
        return;
    };
    let Some((event, handles)) = exchange_with(renderer, &ask) else {
        return;
    };
    let mut caps = Vec::new();
    if let (
        Event::Caps {
            status: Status::Ok,
            len,
            ..
        },
        Some((Object::Vmo(vmo), _)),
    ) = (event, handles.first())
    {
        caps = vec![0_u8; len as usize];
        for (index, chunk) in caps.chunks_mut(PAGE_SIZE as usize).enumerate() {
            if vmo.read_page(index as u64, 0, chunk).is_err() {
                caps = Vec::new();
                break;
            }
        }
    }
    crate::object::dispose(handles.into_iter().map(|(object, _)| object));
    crate::console::println!(
        "  render   renderD{} read capability set {capset}: {} bytes",
        renderer.index,
        caps.len()
    );
    renderer.state.lock().caps.push((capset, caps));
}

/// Take the proof's context away again, so that a published renderer starts
/// with nothing on the device that no open owns.
fn prove_context_goes(renderer: &Renderer, context: u32) {
    let Ok(ask) = renderer.state.lock().session.drop_context(context) else {
        return;
    };
    match exchange(renderer, &ask) {
        Some(Event::ContextGone {
            status: Status::Ok, ..
        }) => {
            crate::console::println!(
                "  render   renderD{} gave context {context} back",
                renderer.index
            );
        }
        Some(Event::ContextGone { status, .. }) => {
            crate::console::println!("  render   the device kept a context: {status:?}");
        }
        _ => {}
    }
}

/// Send `ask` and take the one reply the session is waiting for.
///
/// Only [`prove`] uses this, and only before the renderer is published: it
/// reads the channel itself, which nothing may do once [`serve`] is the one
/// taking replies off it.
fn exchange(renderer: &Renderer, ask: &Message) -> Option<Event> {
    let (event, handles) = exchange_with(renderer, ask)?;
    crate::object::dispose(handles.into_iter().map(|(object, _)| object));
    Some(event)
}

/// [`exchange`], answering what the reply brought with it as well.
fn exchange_with(renderer: &Renderer, ask: &Message) -> Option<(Event, Vec<Transfer>)> {
    if !send(&renderer.control, ask) {
        return None;
    }
    let deadline = timer::now_nanos().saturating_add(REPLY_PATIENCE_NANOS);
    let reply = receive(&renderer.control, deadline)?;
    let decoded = Message::decode(&reply.bytes);
    let accepted = decoded.map(|message| renderer.state.lock().session.receive(&message));
    match accepted {
        Some(Ok(event)) => Some((event, reply.handles)),
        other => {
            crate::object::dispose(reply.handles.into_iter().map(|(object, _)| object));
            if let Some(Err(refusal)) = other {
                refuse(&renderer.control, refusal);
            }
            None
        }
    }
}

/// Where a flying request is recorded, if `event` is its reply.
fn flying_at(state: &State, event: Event) -> Option<usize> {
    let flight = match event {
        Event::Transferred { object, .. } => Flight::Transfer(object),
        Event::Submitted { fence, .. } => {
            return state.flying.iter().position(
                |held| matches!(held.flight, Flight::Submit { fence: kept, .. } if kept == fence),
            );
        }
        _ => return None,
    };
    state.flying.iter().position(|held| held.flight == flight)
}

/// Where an abandoned reply is recorded, if `event` is one.
fn abandoned_at(state: &State, event: Event) -> Option<usize> {
    let which = match event {
        Event::ObjectMade { object, .. }
        | Event::BlobMade { object, .. }
        | Event::ObjectGone { object, .. } => Abandoned::Object(object),
        Event::ContextMade { context, .. } | Event::ContextGone { context, .. } => {
            Abandoned::Context(context)
        }
        Event::Transferred { object, .. } => Abandoned::Transfer(object),
        Event::ObjectAttached { object, .. } => Abandoned::Attach(object),
        _ => return None,
    };
    state.abandoned.iter().position(|held| *held == which)
}
