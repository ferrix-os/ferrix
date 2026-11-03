//! The audio core: sound cards served by ring-3 drivers, and the streams
//! programs play through them.
//!
//! `docs/AUDIO.md` §3 is the design. A driver (`src/user/system/native/drivers/sound/virtio-snd`, started by devmgr)
//! asks for a control channel with `sound_control_create`; the core's task for
//! the card waits for its HELLO, judges it with `ferrix_sndctl::session`,
//! allocates the published stream's buffer, publishes `/dev/snd/controlC<N>`
//! and `pcmC<N>D0p`, and answers READY with the buffer, which the driver pins
//! read-only so the device reads samples from the core's pages. From then on
//! the task relays the driver's completions to the stream, and a program's
//! requests on the nodes ([`pcm`]) move it the other way.
//!
//! Everything the stream decides is `ferrix_sndctl::pcm`'s, host-tested
//! against Linux's rules; this module is the locks, the copies and the
//! messages around it.
//!
//! # Order on the channel
//!
//! A SUBMIT comes from two places: a program's write, and the task taking a
//! completion that frees room. The driver posts them in the order they
//! arrive and the stream takes completions only oldest first, so they must
//! arrive in sequence order. Every message the stream asks for is therefore
//! written while the stream's lock is held ([`Card::with_stream`]): the
//! sequence number and the write are one step.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use ferrix_blkring::identity::Location;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::socket::Width;
use ferrix_linux_abi::sound::STATE_XRUN;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;
use ferrix_sndctl::message::{BUFFER_RIGHTS, MAX_BYTES, Message, Refusal, Submit};
use ferrix_sndctl::pcm::{Effects, Stream};
use ferrix_sndctl::session::{self, Publication, Received, Session};

use crate::claim::{Claims, Numbers, StillServed};
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

pub(crate) mod pcm;

/// Linux's static ALSA major, `CONFIG_SND_MAJOR` (`docs/AUDIO.md` §3.4).
pub(crate) const SOUND_MAJOR: u32 = 116;

/// The minor of `controlC<N>`: Linux's static layout gives each card 32.
pub(crate) const fn control_minor(card: u32) -> u32 {
    card * 32
}

/// The minor of `pcmC<N>D0p`: the first playback device, 16 into the card's.
pub(crate) const fn playback_minor(card: u32) -> u32 {
    card * 32 + 16
}

/// How long the core waits for its driver's HELLO.
const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How long the task sleeps before looking at the channel of its own accord.
const RECHECK_NANOS: u64 = 50_000_000;

/// A page.
const PAGE_BYTES: usize = ferrix_bootinfo::PAGE_SIZE as usize;

/// The width of every program: the kernel's own. Ferrix has no compat mode.
pub(crate) const fn width() -> Width {
    if usize::BITS == 32 {
        Width::Bits32
    } else {
        Width::Bits64
    }
}

/// Why a control channel could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CreateError {
    /// The device already has one.
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
}

static STARTING: SpinLock<Vec<Start>> = SpinLock::new(Vec::new());
/// The devices a driver's channel claims, which a quiesce waits out, so
/// that devmgr can start a dead card's driver again (`crate::claim`).
static CLAIMS: Claims = Claims::new();
static CARDS: SpinLock<Vec<Arc<Card>>> = SpinLock::new(Vec::new());
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
/// The cards' numbers: a card whose driver came back is `C0` again.
static NUMBERS: Numbers = Numbers::new(0);

/// A published sound card, with its one playback stream.
pub(crate) struct Card {
    /// `C` in `controlC<C>`.
    pub(crate) index: u32,
    /// The device's stream number the published stream is.
    stream_id: u32,
    /// Where the device is, for `CARD_INFO`'s long name.
    pub(crate) location: u32,
    control: Arc<Endpoint>,
    /// The stream, which every PCM request and every completion moves.
    pub(crate) stream: SpinLock<Stream>,
    /// The published stream's buffer, which the driver pinned for its
    /// device to read.
    pub(crate) buffer: Arc<Vmo>,
    /// Whether the device sees what the caches hold: if not, what a program
    /// wrote is cleaned out of them before the device is told of it.
    coherent: bool,
    /// Whether a program has the playback node open: one opener at a time.
    pub(crate) opened: AtomicBool,
    /// Woken whenever the stream moves or the card goes.
    pub(crate) changed: Arc<WaitQueue>,
    /// Set when the driver is gone.
    gone: AtomicBool,
    /// How many times the stream has underrun, for the console.
    underruns: AtomicU32,
}

impl core::fmt::Debug for Card {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Card")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl Card {
    /// Whether the driver is gone.
    pub(crate) fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// Run `step` on the stream with the lock held, then send what it asks
    /// the driver for, still holding it, and wake the stream's waiters if it
    /// says to. See the module comment for why the send is under the lock.
    pub(crate) fn with_stream<R>(&self, step: impl FnOnce(&mut Stream, &mut Effects) -> R) -> R {
        let mut effects = Effects::default();
        let (result, underran) = {
            let mut stream = self.stream.lock();
            let before = stream.state();
            let result = step(&mut stream, &mut effects);
            self.send(&effects);
            (result, before != STATE_XRUN && stream.state() == STATE_XRUN)
        };
        if effects.wake {
            self.changed.wake_all();
        }
        if underran {
            self.say_underrun();
        }
        result
    }

    /// Say that the stream underran: the first few times, then every 100th,
    /// so a program that underruns all the time does not flood the console.
    fn say_underrun(&self) {
        let count = self.underruns.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 3 || count.is_multiple_of(100) {
            crate::console::println!(
                "  audio    card{}: underrun {count}: the program did not keep the device fed",
                self.index
            );
        }
    }

    /// Write the SUBMITs and the HALT `effects` asks for. A driver that went
    /// away takes nothing, and the task hears of it.
    fn send(&self, effects: &Effects) {
        let messages = effects
            .submits()
            .map(|submit| {
                Message::Submit(Submit {
                    stream: self.stream_id,
                    sequence: submit.sequence,
                    offset: submit.offset,
                    bytes: submit.bytes,
                })
            })
            .chain(effects.halt.then_some(Message::Halt {
                stream: self.stream_id,
            }));
        for message in messages {
            let bytes = message.encode().as_bytes().to_vec();
            let _ = self
                .control
                .write(bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
        }
    }

    /// Copy `data` into the buffer at `offset`, a page at a time, and clean
    /// it out of the caches for a device that does not see them. Called with
    /// no lock held: `Vmo::write_page` must not be.
    pub(crate) fn fill(&self, offset: usize, data: &[u8]) -> Result<(), Errno> {
        let mut at = offset;
        for chunk in data.chunks(PAGE_BYTES) {
            let mut rest = chunk;
            while !rest.is_empty() {
                let within = at % PAGE_BYTES;
                let len = rest.len().min(PAGE_BYTES - within);
                let (here, next) = rest.split_at(len);
                let page = (at / PAGE_BYTES) as u64;
                self.buffer
                    .write_page(page, within, here)
                    .map_err(|_| Errno::EFAULT)?;
                if !self.coherent
                    && let Some(frame) = self.buffer.page(page)
                {
                    let virt =
                        crate::mm::direct_map(frame * ferrix_bootinfo::PAGE_SIZE) + within as u64;
                    crate::arch::clean_for_device(virt, len as u64);
                }
                at += len;
                rest = next;
            }
        }
        Ok(())
    }
}

/// The card published as `index`, if one is.
pub(crate) fn card(index: u32) -> Option<Arc<Card>> {
    CARDS
        .lock()
        .iter()
        .find(|card| card.index == index)
        .map(Arc::clone)
}

/// Every published card's number, in order.
pub(crate) fn card_indices() -> Vec<u32> {
    let mut indices: Vec<u32> = CARDS.lock().iter().map(|card| card.index).collect();
    indices.sort_unstable();
    indices
}

/// Answer `sound_control_create` with audio.
///
/// # Errors
///
/// [`Full`] when the item has no room for the registration.
pub fn install() -> Result<(), Full> {
    native::serve(NativeCall::SoundControlCreate, control_create)?;
    native::register_server(&SERVER)
}

/// What a quiesce waits out for audio.
static SERVER: native::Server = native::Server {
    wait_until_unserved,
    release: None,
};

/// Wait until no sound driver's channel claims `node`, for a quiesce
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

/// `sound_control_create`: the device's own channel, one per device, and the
/// driver's end of it back, as input's.
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

fn create(node: &Arc<DeviceNode>) -> Result<Arc<Endpoint>, CreateError> {
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
    if sched::spawn("audio", run, id, ferrix_sched_fair::NICE_0_WEIGHT).is_err() {
        let _ = take_start(id);
        CLAIMS.release(node);
        return Err(CreateError::NoMemory);
    }
    Ok(driver_end)
}

fn take_start(id: usize) -> Option<Start> {
    let mut starting = STARTING.lock();
    let at = starting.iter().position(|start| start.id == id)?;
    Some(starting.remove(at))
}

/// One card's task.
fn run(id: usize) {
    let Some(start) = take_start(id) else {
        return;
    };
    if let Some((card, session)) = take_up(&start) {
        serve(&card, session);
        CARDS.lock().retain(|held| !Arc::ptr_eq(held, &card));
        card.gone.store(true, Ordering::Release);
        // Every request now answers `EBADFD`, as a disconnected card's do.
        card.stream.lock().disconnect(&mut Effects::default());
        card.changed.wake_all();
        // A program still holding the old card's nodes keeps its `Card`,
        // and a lookup of the number finds the next one.
        NUMBERS.give_back(card.index);
        crate::console::println!("  audio    card{} is gone", card.index);
    }
    CLAIMS.release(&start.device);
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
    let _ = control.write(bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
}

/// Wait for HELLO and publish the card, or refuse it.
#[inline(never)]
fn take_up(start: &Start) -> Option<(Arc<Card>, Session)> {
    let deadline = timer::now_nanos().saturating_add(HELLO_PATIENCE_NANOS);
    let message = receive(&start.control, deadline)?;
    match accept(start, &message) {
        Ok(taken) => Some(taken),
        Err(refusal) => {
            crate::console::println!("  audio    a driver was refused: {refusal}");
            refuse(&start.control, refusal);
            None
        }
    }
}

/// Judge a HELLO, giving what the card publishes.
#[inline(never)]
fn judge(start: &Start, message: &ChannelMessage) -> Result<Publication, Refusal> {
    if !matches!(message.handles.first(), Some((Object::Port(_), _))) {
        return Err(Refusal::Hello);
    }
    let rights: Vec<Rights> = message.handles.iter().map(|(_, rights)| *rights).collect();
    let Ok(Message::Hello(hello)) = Message::decode(&message.bytes) else {
        return Err(Refusal::Protocol);
    };
    if start.location.map(Location::raw) != Some(hello.location) {
        return Err(Refusal::Hello);
    }
    session::judge(&hello, &rights)
}

#[inline(never)]
fn accept(start: &Start, message: &ChannelMessage) -> Result<(Arc<Card>, Session), Refusal> {
    let publication = judge(start, message)?;
    // The driver reset the device, and released what its streams held,
    // before it sent HELLO: what a dead one's pins kept from the allocator
    // can go back (`object::pin`'s quarantine, `docs/AUDIO.md` §3.3), and the
    // kernel's configuration is read back (`DeviceNode::verify_config`).
    start.device.hello_accepted();
    let config = publication.config;
    let pages = (config.buffer_bytes() as usize).div_ceil(PAGE_BYTES) as u64;
    let buffer = Vmo::new_anonymous(pages).map_err(|_| Refusal::Hello)?;
    // Written through the direct map for its life: never write-combining
    // (`L.user.133`). A new object is never refused.
    let _ = buffer.take_kernel_view();
    let index = NUMBERS.take().ok_or(Refusal::Hello)?;
    let location = start.location.map_or(0, Location::raw);
    let card = Arc::new(Card {
        index,
        stream_id: publication.stream,
        location,
        control: Arc::clone(&start.control),
        stream: SpinLock::new(Stream::new(config)),
        buffer: Arc::clone(&buffer),
        coherent: start.device.dma_shape().coherent,
        opened: AtomicBool::new(false),
        changed: Arc::new(WaitQueue::new()),
        gone: AtomicBool::new(false),
        underruns: AtomicU32::new(0),
    });
    // Published before READY goes out, as input does: devmgr kills a driver
    // that has not published by the time it reports.
    CARDS.lock().push(Arc::clone(&card));
    if let Some(location) = start.location {
        crate::discovery::devmgr::published(location);
    }
    if let Err(refusal) = send_ready(&start.control, &publication, index, buffer) {
        CARDS.lock().retain(|held| !Arc::ptr_eq(held, &card));
        NUMBERS.give_back(index);
        return Err(refusal);
    }
    crate::console::println!(
        "  audio    card{index} virtio-snd: playback {} Hz, {} channels, S16_LE{}",
        config.rate,
        config.channels,
        match publication.left_out {
            0 => "",
            1 => ", 1 stream left out",
            _ => ", streams left out",
        }
    );
    Ok((card, Session::new(publication)))
}

/// Tell the driver its card is published: the core's port, then the
/// stream's buffer, which it pins read-only.
#[inline(never)]
fn send_ready(
    control: &Endpoint,
    publication: &Publication,
    index: u32,
    buffer: Arc<Vmo>,
) -> Result<(), Refusal> {
    let ready = Message::Ready(publication.ready(index))
        .encode()
        .as_bytes()
        .to_vec();
    let port = Port::new().map_err(|_| Refusal::Hello)?;
    let handed = vec![
        (Object::Port(port), Rights::WRITE),
        (Object::Vmo(buffer), BUFFER_RIGHTS),
    ];
    control
        .write(ready, 2, || Ok::<Vec<Transfer>, Infallible>(handed))
        .map_err(|_| Refusal::Hello)
}

/// Serve one card until its driver goes or lies.
#[inline(never)]
fn serve(card: &Card, mut session: Session) {
    loop {
        let deadline = timer::now_nanos().saturating_add(RECHECK_NANOS);
        let message = match card.control.read(MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => message,
            Err(ReadError::Empty) => {
                if card.control.signals().intersects(Signals::PEER_CLOSED) {
                    break;
                }
                let _ = card.control.waiters().wait_until_deadline(
                    || {
                        card.control
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
            refuse(&card.control, Refusal::Protocol);
            break;
        };
        let now = timer::now_nanos();
        let received =
            card.with_stream(|stream, effects| session.receive(&decoded, stream, now, effects));
        match received {
            Ok(Received::Stream) => {}
            Ok(Received::Stopped) => break,
            Err(refusal) => {
                crate::console::println!("  audio    card{}: {refusal}", card.index);
                refuse(&card.control, refusal);
                break;
            }
        }
    }
}
