//! A virtio-gpu 2D driver, as logic over a transport and DMA memory it is
//! handed.
//!
//! Iteration 1 of the display (`docs/DISPLAY.md`) runs this driver in a user
//! process under devmgr, as virtio-blk's runs. The process holds an
//! `IoMapping` of the device's register blocks, an `Interrupt`, pinned DMA
//! memory, the card VMO it pins buffers from, and the control channel to the
//! kernel's display core; none of those can be had in a unit test. So this
//! crate is written against [`Transport`], [`DevicePages`] and [`CommandArea`],
//! which the process implements over its handles, and holds everything else:
//!
//! * [`Driver`] brings the device up and runs control commands through the
//!   control queue, several in flight at once, checking every response;
//! * [`pipeline::Pipeline`] turns the display core's requests — ATTACH,
//!   SCANOUT, FLUSH, DETACH — into the device commands each takes, and the
//!   commands' outcomes into the replies the core waits for.
//!
//! # Commands in slots, as many in flight as there are slots
//!
//! The display waits for each FLIPPED, but a frame drawn on the GPU is an
//! upload or two, a command stream and a flush, and each used to be a round
//! trip of its own through two processes, the device and back (`docs/GPU.md`
//! §3.9). So the control queue is run the way seL4's device driver framework
//! runs a queue: the command area starts with [`CONTROL_SLOTS`] slots, each a
//! request and its response, [`Driver::post`] writes a command into a free
//! one and publishes it without ringing the doorbell, [`Driver::kick`] rings
//! it once for everything posted since, and [`Driver::take_done`] hands the
//! completions back in whatever order the device made them, each with the
//! tag it was posted with. A command too long for a slot -- a backing list
//! of many pages, a capability set -- takes the one large place after the
//! slots, the request and then a page of response, as every command did
//! when there was only one.
//!
//! A command stream need not be copied at all: [`Driver::post`] takes
//! buffers the device reads after the request, so a stream already in
//! pinned memory is handed over where it lies, with
//! [`Command::Submit3dHeader`] as the request.
//!
//! # The cursor queue is the other way round
//!
//! A pointer moves a hundred times a second and nobody waits on a move, so
//! the cursor queue is run the way seL4's device driver framework runs its
//! queues: commands go into slots of a page of their own
//! ([`CURSOR_SLOTS`]), everything owed is posted behind one doorbell, and
//! the device's completions raise no interrupt at all -- a slot is taken
//! back when the next command is posted, or when the control queue
//! interrupts anyway. When every slot is taken, what is owed waits, newest
//! first: a scanout owes at most one command, the latest place and, if the
//! image changed since, the image ([`Driver::update_cursor`],
//! [`Driver::move_cursor`]).
//!
//! # DMA memory is freed only after a reset
//!
//! As in `ferrix-virtio-blk`: the rings and the command area are held in
//! [`ManuallyDrop`], [`Driver::shutdown`] resets the device and hands them
//! back as [`Teardown::Released`] only if the reset finished.
//!
//! # Trust
//!
//! The used ring is checked by [`SplitQueue`], a response by
//! [`gpu::Response::parse_for`], the configuration by [`gpu::Config::read`].
//! A device that answers a command with an error response has refused that
//! command and is still trusted; a device that breaks the protocol is marked
//! failed and sent nothing more.

#![no_std]
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

use core::fmt;
use core::mem::ManuallyDrop;

use ferrix_virtio::gpu::{
    self, Command, Config, DeviceConfig, DeviceError as Refusal, GpuError, PAGE_SIZE, Response,
};
use ferrix_virtio::pci::{
    self, CommonConfig, DEVICE_STATUS, QueueAddresses, STATUS_DEVICE_NEEDS_RESET, STATUS_FAILED,
    TransportError,
};
use ferrix_virtio::{Buffer, Layout, QueueError, QueueMemory, SplitQueue};

pub mod pipeline;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

/// ISR status bit: a queue has something for the driver.
pub const ISR_QUEUE: u8 = 1;

/// How many interrupts may bring completions before [`Driver::on_interrupt`]
/// looks at the configuration anyway: a configuration change that arrived
/// together with completions is seen within this many.
pub const CONFIG_LOOK_EVERY: u32 = 64;

/// How long a driver with a command in flight waits for an interrupt before
/// it asks [`Driver::check_needs_reset`].
pub const WATCH_NANOS: u64 = 100_000_000;

/// ISR status bit: the device configuration changed, which for a GPU means a
/// display was attached or detached.
pub const ISR_CONFIG: u8 = 2;

/// Bytes at the end of the command area kept for the response.
///
/// One page. `GET_DISPLAY_INFO`'s 408 bytes were the longest response until
/// 3D, and 512 was that rounded up; a capability set is the longest now.
/// virglrenderer's `virgl_caps_v2` is about 1.4 KiB and a device names its
/// own size, so the driver either has room for what the device offers or
/// cannot fetch it -- and a page is comfortably more than any renderer's
/// today while staying one page of pinned memory per card.
///
/// [`CAPSET_ROOM`] is what that leaves for a capability set itself.
pub const RESPONSE_BYTES: usize = 4096;

/// Bytes of a capability set the response buffer has room for: everything
/// but the header a `GET_CAPSET` response starts with.
///
/// A driver asks for `min(what the device says, this)` and says so when the
/// device's is larger, rather than asking for a set that will not fit.
pub const CAPSET_ROOM: u32 = (RESPONSE_BYTES - gpu::HEADER_LEN) as u32;

/// The control queue's size: room for every slot's chain and the large
/// command's, whose request may cross many pages that are not side by side.
pub const QUEUE_SIZE: u16 = 128;

/// Commands the control queue may have in slots at once, beside the large
/// one.
pub const CONTROL_SLOTS: usize = 8;

/// Bytes of one slot: its request and then its response. A quarter of a
/// page, so that no slot crosses one.
const SLOT_BYTES: usize = 1024;

/// Bytes of a slot's request: every command but a long backing list or an
/// inline stream fits, and `CTX_CREATE`, the longest, is 96.
const SLOT_REQUEST_BYTES: usize = 512;

/// Bytes of a slot's response: `GET_DISPLAY_INFO`'s 408 is the longest a
/// slot is asked to hold.
const SLOT_RESPONSE_BYTES: usize = SLOT_BYTES - SLOT_REQUEST_BYTES;

/// Bytes at the start of the command area the slots take.
pub const SLOT_AREA_BYTES: usize = CONTROL_SLOTS * SLOT_BYTES;

/// Where the large command is in the table of what is in flight.
const LARGE: usize = CONTROL_SLOTS;

/// The most buffers one chain is made of.
const MAX_CHAIN: usize = 64;

/// Cursor commands the cursor queue may hold at once: its size, and the
/// slots of the cursor area.
pub const CURSOR_SLOTS: usize = 16;

/// Bytes of one slot of the cursor area: a command, rounded up so that no
/// slot crosses a page.
const CURSOR_SLOT_BYTES: usize = 64;

/// The device's registers, as the process that drives it reaches them. The
/// same shape as `ferrix-virtio-blk`'s.
pub trait Transport: CommonConfig + DeviceConfig {
    /// Ring the doorbell for `queue`, whose `queue_notify_off` is
    /// `notify_off`.
    fn notify(&mut self, queue: u16, notify_off: u16);

    /// The MSI-X table entry the control queue should interrupt through, or
    /// [`pci::NO_VECTOR`].
    fn queue_vector(&self) -> u16;

    /// Acknowledge the interrupt and say why it came: the ISR status byte for
    /// a line interrupt, [`ISR_QUEUE`] for MSI-X.
    fn acknowledge_interrupt(&mut self) -> u8;

    /// Write the little-endian `u32` at `offset` of the device-specific
    /// configuration: virtio-gpu's one writable field is `events_clear`.
    fn config_write32(&mut self, offset: u32, value: u32);
}

/// Pinned memory, as the device addresses of its pages, in order.
pub trait DevicePages {
    /// One device address per page, [`PAGE_SIZE`] bytes each, not necessarily
    /// consecutive.
    fn device_pages(&self) -> &[u64];
}

/// The memory a command's request and response live in.
pub trait CommandArea: DevicePages {
    /// Read the byte at `offset`.
    fn read_u8(&self, offset: usize) -> u8;
    /// Write the byte at `offset`.
    fn write_u8(&mut self, offset: usize, value: u8);

    /// Read `out.len()` bytes from `offset`. A byte at a time unless the
    /// memory knows better, which mapped memory does.
    fn read_bytes(&self, offset: usize, out: &mut [u8]) {
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = self.read_u8(offset + index);
        }
    }

    /// Write `bytes` from `offset`, the same way.
    fn write_bytes(&mut self, offset: usize, bytes: &[u8]) {
        for (index, &byte) in bytes.iter().enumerate() {
            self.write_u8(offset + index, byte);
        }
    }
}

/// Everything a driver is built from.
pub struct Parts<T, R, A> {
    /// The device's registers.
    pub transport: T,
    /// The control queue's rings: pages holding a queue of [`QUEUE_SIZE`].
    pub rings: R,
    /// Requests and responses: the slots, then the large command's request
    /// and a page for its response, so more than [`SLOT_AREA_BYTES`] and a
    /// page.
    pub area: A,
    /// The cursor queue's rings: pages holding a queue of [`CURSOR_SLOTS`].
    pub cursor_rings: R,
    /// Where cursor commands wait for the device: a page.
    pub cursor_area: A,
}

/// How [`Driver::init`] goes about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    /// Reads of `device_status` a reset may take.
    pub reset_polls: u32,
    /// Whether to accept the features 3D needs, which is what makes a
    /// `virtio-gpu-gl` device bring its renderer up: `gpu::DRIVER_FEATURES_3D`
    /// rather than `gpu::DRIVER_FEATURES`.
    ///
    /// Off by default, because accepting a feature is not free and a
    /// driver that will never send a 3D command has no business asking a
    /// host to start a renderer. A driver that wants to *know* what the
    /// card is turns it on: a 2D device cannot offer the feature, so asking
    /// costs that device nothing, and on a 3D one the answer is the point.
    pub want_3d: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            reset_polls: 100_000,
            want_3d: false,
        }
    }
}

/// What the driver agreed with the device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    /// The features negotiated.
    pub features: u64,
    /// The configuration as read at bring-up.
    pub config: Config,
    /// The MSI-X vector the control queue interrupts through, or `NO_VECTOR`.
    pub vector: u16,
}

/// Why a device could not be brought up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// The status protocol failed.
    Transport(TransportError),
    /// The configuration is unusable.
    Config(GpuError),
    /// The rings' pages cannot hold the queue, or the command area has no
    /// room for a request beside the response.
    NoRoom,
    /// The device would not give the queue the vector asked for.
    VectorRefused {
        /// Asked for.
        asked: u16,
        /// Kept.
        kept: u16,
    },
}

/// Why a command was not submitted. The command is not in flight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// The device has failed; shut the driver down.
    Broken,
    /// Every place the command could go is in flight, or the queue has no
    /// descriptors left for it until some come back: post it again after a
    /// completion.
    Busy,
    /// The request does not fit the command area or the queue.
    TooLarge,
    /// The command itself is malformed.
    Command(GpuError),
    /// The device broke the queue while the command was published.
    Device(DeviceError),
    /// A cursor for a scanout the device does not have.
    NoSuchScanout,
}

/// How the device broke the protocol. The driver has set `FAILED`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceError {
    /// The device set `DEVICE_NEEDS_RESET`.
    NeedsReset,
    /// The rings say something impossible.
    Queue(QueueError),
    /// A response is impossible.
    Protocol(GpuError),
    /// A completion for a chain the driver did not publish.
    UnknownChain(u16),
}

/// A command's outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Done {
    /// What [`Driver::post`] was given, so the caller knows whose it is.
    pub tag: u64,
    /// The command's type code.
    pub command: u32,
    /// The response, or the device's refusal of the command.
    pub result: Result<Response, Refusal>,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Transport(error) => write!(f, "{error}"),
            Self::Config(error) => write!(f, "{error}"),
            Self::NoRoom => f.write_str("the queue or the command area does not fit"),
            Self::VectorRefused { asked, kept } => {
                write!(f, "the device kept vector {kept:#x} for {asked:#x}")
            }
        }
    }
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::NeedsReset => f.write_str("the device needs a reset"),
            Self::Queue(error) => write!(f, "the rings are corrupt: {error:?}"),
            Self::Protocol(error) => write!(f, "{error}"),
            Self::UnknownChain(head) => write!(f, "chain {head} is not in flight"),
        }
    }
}

/// A driver's parts, handed back.
pub struct Released<T, R, A> {
    /// The device's registers.
    pub transport: T,
    /// The rings' memory, inside the queue if one was built.
    pub rings: Rings<R>,
    /// The command area.
    pub area: A,
    /// The cursor queue's rings, inside the queue if one was built.
    pub cursor_rings: Rings<R>,
    /// The cursor area.
    pub cursor_area: A,
}

/// The rings' memory as it comes back.
pub enum Rings<R> {
    /// Bring-up stopped before the queue was built.
    Unused(R),
    /// The queue, with the memory inside it.
    Queue(SplitQueue<R>),
}

/// How a driver ended.
pub enum Teardown<T, R, A> {
    /// The device reset; the memory may be unpinned.
    Released(Released<T, R, A>),
    /// The device did not reset and may still write to the memory, so it is
    /// never dropped.
    Wedged(ManuallyDrop<Released<T, R, A>>),
}

/// A failed [`Driver::init`].
pub struct InitFailure<T, R, A> {
    /// Why.
    pub error: InitError,
    /// The parts, released only if the reset finished.
    pub teardown: Teardown<T, R, A>,
}

impl<T, R, A> fmt::Debug for Released<T, R, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Released")
            .field("rings", &self.rings)
            .finish_non_exhaustive()
    }
}

impl<R> fmt::Debug for Rings<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unused(_) => "Rings::Unused",
            Self::Queue(_) => "Rings::Queue",
        })
    }
}

impl<T, R, A> fmt::Debug for Teardown<T, R, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Released(_) => "Teardown::Released",
            Self::Wedged(_) => "Teardown::Wedged",
        })
    }
}

impl<T, R, A> fmt::Debug for InitFailure<T, R, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitFailure")
            .field("error", &self.error)
            .field("teardown", &self.teardown)
            .finish()
    }
}

impl<T, R, A> fmt::Debug for Parts<T, R, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parts").finish_non_exhaustive()
    }
}

/// A command in flight, in the table at the index of its place: a slot, or
/// [`LARGE`].
#[derive(Clone, Copy, Debug)]
struct InFlight {
    head: u16,
    command: u32,
    tag: u64,
}

/// A virtio-gpu device, brought up and driven.
pub struct Driver<T, R, A> {
    transport: T,
    queue: ManuallyDrop<SplitQueue<R>>,
    area: ManuallyDrop<A>,
    info: Info,
    notify_off: u16,
    reset_polls: u32,
    /// Interrupts taken, for [`CONFIG_LOOK_EVERY`].
    interrupts: u32,
    fault: Option<DeviceError>,
    /// What each slot, and then the large place, holds while the device has
    /// it.
    in_flight: [Option<InFlight>; CONTROL_SLOTS + 1],
    /// Whether something was posted since the doorbell last rang.
    posted: bool,
    /// The place of the command [`Driver::take_done`] last handed back,
    /// whose response [`Driver::read_response`] reads.
    last_taken: Option<usize>,
    cursor_queue: ManuallyDrop<SplitQueue<R>>,
    cursor_area: ManuallyDrop<A>,
    cursor_notify_off: u16,
    /// The chain each slot of the cursor area is in, while the device has it.
    cursor_slots: [Option<u16>; CURSOR_SLOTS],
    /// Each scanout's cursor as the device will next be told it.
    cursors: [gpu::Cursor; gpu::MAX_SCANOUTS],
    /// What each scanout's cursor is owed and has not been sent for want of
    /// a slot: `Some(true)` its image, `Some(false)` only its place.
    cursor_owed: [Option<bool>; gpu::MAX_SCANOUTS],
}

impl<T, R, A> fmt::Debug for Driver<T, R, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("info", &self.info)
            .field("fault", &self.fault)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

/// The device address of `len` bytes at `start` of `pages`, if they are
/// device-contiguous.
fn contiguous(pages: &[u64], start: usize, len: usize) -> Option<u64> {
    let page = usize::try_from(PAGE_SIZE).ok()?;
    let first = start / page;
    let last = start.checked_add(len)?.checked_sub(1)? / page;
    let base = *pages.get(first)?;
    let mut expected = base;
    for index in first + 1..=last {
        expected = expected.checked_add(PAGE_SIZE)?;
        if *pages.get(index)? != expected {
            return None;
        }
    }
    base.checked_add(u64::try_from(start % page).ok()?)
}

/// Add `len` readable bytes at `address` to the chain being built in
/// `buffers`, joined to the last buffer where they follow it.
fn append(
    buffers: &mut [Buffer],
    count: &mut usize,
    address: u64,
    len: usize,
) -> Result<(), SubmitError> {
    let len = u32::try_from(len).map_err(|_| SubmitError::TooLarge)?;
    let joined = count
        .checked_sub(1)
        .and_then(|last| buffers.get_mut(last))
        .filter(|last| last.address.checked_add(u64::from(last.len)) == Some(address))
        .filter(|last| last.len.checked_add(len).is_some());
    if let Some(last) = joined {
        last.len += len;
        return Ok(());
    }
    // The last place is the response's.
    if *count + 1 >= buffers.len() {
        return Err(SubmitError::TooLarge);
    }
    let slot = buffers.get_mut(*count).ok_or(SubmitError::TooLarge)?;
    *slot = Buffer::readable(address, len);
    *count += 1;
    Ok(())
}

/// Where the rings of `layout` are in `pages`.
fn ring_addresses(layout: &Layout, pages: &[u64]) -> Option<QueueAddresses> {
    Some(QueueAddresses {
        descriptors: contiguous(
            pages,
            layout.descriptor_table,
            layout.available_ring - layout.descriptor_table,
        )?,
        driver: contiguous(
            pages,
            layout.available_ring,
            layout.used_ring - layout.available_ring,
        )?,
        device: contiguous(
            pages,
            layout.used_ring,
            layout.total_size - layout.used_ring,
        )?,
    })
}

/// Where queue `index` goes in `pages`: its layout, at most `wanted` long,
/// and the three parts' device addresses. `room` is whether the areas the
/// commands live in are large enough, which fails the plan the same way.
fn plan_queue<T: CommonConfig>(
    transport: &mut T,
    index: u16,
    wanted: u16,
    pages: &[u64],
    room: bool,
) -> Result<(Layout, QueueAddresses), InitError> {
    let max = pci::queue_max_size(transport, index).map_err(InitError::Transport)?;
    let layout = Layout::for_size(wanted.min(max)).map_err(|_| InitError::NoRoom)?;
    let addresses = ring_addresses(&layout, pages).ok_or(InitError::NoRoom)?;
    if room {
        Ok((layout, addresses))
    } else {
        Err(InitError::NoRoom)
    }
}

/// Enable the control queue on the vector the transport names and the
/// cursor queue on none, ask for configuration changes -- a display the host
/// resized -- on the control queue's vector, then `DRIVER_OK`.
fn start_queues<T: Transport>(
    transport: &mut T,
    control: (Layout, QueueAddresses),
    cursor: (Layout, QueueAddresses),
) -> Result<(pci::ActiveQueue, pci::ActiveQueue), InitError> {
    let asked = transport.queue_vector();
    let active = pci::activate_queue(
        transport,
        gpu::CONTROL_QUEUE,
        control.0.queue_size,
        control.1,
        asked,
    )
    .map_err(InitError::Transport)?;
    if active.vector != asked {
        return Err(InitError::VectorRefused {
            asked,
            kept: active.vector,
        });
    }
    let cursor = pci::activate_queue(
        transport,
        gpu::CURSOR_QUEUE,
        cursor.0.queue_size,
        cursor.1,
        pci::NO_VECTOR,
    )
    .map_err(InitError::Transport)?;
    // A device with no room for it keeps none, and a display change is then
    // noticed at the next completion instead: [`Driver::display_changed`]
    // reads the event register itself.
    let _kept = pci::set_config_vector(transport, asked);
    pci::driver_ok(transport).map_err(InitError::Transport)?;
    Ok((active, cursor))
}

/// A bring-up that failed at `error`: the device marked `FAILED` and reset,
/// and its parts handed back.
fn failed<T: CommonConfig, R, A>(
    mut transport: T,
    rings: Rings<R>,
    area: A,
    cursor: CursorParts<R, A>,
    error: InitError,
    polls: u32,
) -> InitFailure<T, R, A> {
    set_failed(&mut transport);
    InitFailure {
        error,
        teardown: teardown(transport, rings, area, cursor, polls),
    }
}

/// The cursor queue's half of [`Released`].
struct CursorParts<R, A> {
    rings: Rings<R>,
    area: A,
}

fn teardown<T: CommonConfig, R, A>(
    mut transport: T,
    rings: Rings<R>,
    area: A,
    cursor: CursorParts<R, A>,
    polls: u32,
) -> Teardown<T, R, A> {
    let reset = pci::reset(&mut transport, polls);
    let released = Released {
        transport,
        rings,
        area,
        cursor_rings: cursor.rings,
        cursor_area: cursor.area,
    };
    match reset {
        Ok(()) => Teardown::Released(released),
        Err(_) => Teardown::Wedged(ManuallyDrop::new(released)),
    }
}

fn set_failed<T: CommonConfig + ?Sized>(transport: &mut T) {
    let status = transport.read8(DEVICE_STATUS);
    transport.write8(DEVICE_STATUS, status | STATUS_FAILED);
}

impl<T, R, A> Driver<T, R, A>
where
    T: Transport,
    R: QueueMemory + DevicePages,
    A: CommandArea,
{
    /// Bring the device up: reset, features, `FEATURES_OK`, the configuration
    /// read, the control queue and the cursor queue built and enabled,
    /// `DRIVER_OK`.
    ///
    /// The cursor queue interrupts through no vector and asks for no
    /// interrupt: see the module's note on why nobody waits for it.
    #[cfg_attr(
        target_pointer_width = "64",
        expect(
            clippy::result_large_err,
            reason = "the parts are DMA memory handed back once, and there is no allocator to box                       them into; two queues of them pass the lint's size only where a word is                       eight bytes"
        )
    )]
    pub fn init(parts: Parts<T, R, A>, options: Options) -> Result<Self, InitFailure<T, R, A>> {
        let Parts {
            mut transport,
            rings,
            area,
            cursor_rings,
            cursor_area,
        } = parts;
        let polls = options.reset_polls;
        let wanted = if options.want_3d {
            gpu::DRIVER_FEATURES_3D
        } else {
            gpu::DRIVER_FEATURES
        };
        let agreed = pci::negotiate(&mut transport, wanted, gpu::REQUIRED_FEATURES, polls)
            .map_err(InitError::Transport)
            .and_then(|features| {
                let config = Config::read(&transport).map_err(InitError::Config)?;
                let room = area.device_pages().len() * PAGE_SIZE as usize
                    > SLOT_AREA_BYTES + RESPONSE_BYTES
                    && cursor_area.device_pages().len() * PAGE_SIZE as usize
                        >= CURSOR_SLOTS * CURSOR_SLOT_BYTES;
                let control = plan_queue(
                    &mut transport,
                    gpu::CONTROL_QUEUE,
                    QUEUE_SIZE,
                    rings.device_pages(),
                    room,
                )?;
                let cursor = plan_queue(
                    &mut transport,
                    gpu::CURSOR_QUEUE,
                    CURSOR_SLOTS as u16,
                    cursor_rings.device_pages(),
                    room,
                )?;
                Ok((features, config, control, cursor))
            });
        let (features, config, control, cursor) = match agreed {
            Ok(agreed) => agreed,
            Err(error) => {
                let cursor = CursorParts {
                    rings: Rings::Unused(cursor_rings),
                    area: cursor_area,
                };
                return Err(failed(
                    transport,
                    Rings::Unused(rings),
                    area,
                    cursor,
                    error,
                    polls,
                ));
            }
        };

        let queue = SplitQueue::new(control.0, rings);
        let mut cursor_queue = SplitQueue::new(cursor.0, cursor_rings);
        cursor_queue.set_interrupts_suppressed(true);
        let (active, cursor_active) = match start_queues(&mut transport, control, cursor) {
            Ok(started) => started,
            Err(error) => {
                let cursor = CursorParts {
                    rings: Rings::Queue(cursor_queue),
                    area: cursor_area,
                };
                return Err(failed(
                    transport,
                    Rings::Queue(queue),
                    area,
                    cursor,
                    error,
                    polls,
                ));
            }
        };

        Ok(Self {
            transport,
            queue: ManuallyDrop::new(queue),
            area: ManuallyDrop::new(area),
            info: Info {
                features,
                config,
                vector: active.vector,
            },
            notify_off: active.notify_off,
            reset_polls: polls,
            interrupts: 0,
            fault: None,
            in_flight: [None; CONTROL_SLOTS + 1],
            posted: false,
            last_taken: None,
            cursor_queue: ManuallyDrop::new(cursor_queue),
            cursor_area: ManuallyDrop::new(cursor_area),
            cursor_notify_off: cursor_active.notify_off,
            cursor_slots: [None; CURSOR_SLOTS],
            cursors: core::array::from_fn(|scanout| gpu::Cursor {
                scanout_id: scanout as u32,
                ..gpu::Cursor::default()
            }),
            cursor_owed: [None; gpu::MAX_SCANOUTS],
        })
    }

    /// What was agreed.
    #[must_use]
    pub const fn info(&self) -> &Info {
        &self.info
    }

    /// The error the device broke the protocol with, if it has.
    #[must_use]
    pub const fn fault(&self) -> Option<DeviceError> {
        self.fault
    }

    /// Read the device status for `DEVICE_NEEDS_RESET` now, whatever the
    /// last interrupt looked like: for a driver that has waited
    /// [`WATCH_NANOS`] for a command in flight and heard nothing, in case the
    /// configuration change that announced a reset came together with a
    /// completion and [`Driver::on_interrupt`] did not look.
    ///
    /// # Errors
    ///
    /// [`DeviceError::NeedsReset`], or the fault the driver already has.
    pub fn check_needs_reset(&mut self) -> Result<(), DeviceError> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        if self.transport.read8(DEVICE_STATUS) & STATUS_DEVICE_NEEDS_RESET != 0 {
            self.break_down(DeviceError::NeedsReset);
            return Err(DeviceError::NeedsReset);
        }
        Ok(())
    }

    /// Whether any command is in flight.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.in_flight.iter().any(Option::is_some)
    }

    /// How many slots are free: commands short enough for one that could be
    /// posted now.
    #[must_use]
    pub fn free_slots(&self) -> usize {
        self.in_flight
            .iter()
            .take(CONTROL_SLOTS)
            .filter(|slot| slot.is_none())
            .count()
    }

    /// The device's registers.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    fn break_down(&mut self, error: DeviceError) {
        if self.fault.is_none() {
            self.fault = Some(error);
            set_failed(&mut self.transport);
        }
    }

    /// Bytes of the command area.
    fn area_bytes(&self) -> usize {
        self.area.device_pages().len() * PAGE_SIZE as usize
    }

    /// Where the request of the command at `place` starts, and how long it
    /// may be.
    fn request_of(&self, place: usize) -> (usize, usize) {
        if place < CONTROL_SLOTS {
            (place * SLOT_BYTES, SLOT_REQUEST_BYTES)
        } else {
            (
                SLOT_AREA_BYTES,
                self.area_bytes()
                    .saturating_sub(SLOT_AREA_BYTES + RESPONSE_BYTES),
            )
        }
    }

    /// Where the response of the command at `place` starts, and its room.
    fn response_of(&self, place: usize) -> (usize, usize) {
        if place < CONTROL_SLOTS {
            (place * SLOT_BYTES + SLOT_REQUEST_BYTES, SLOT_RESPONSE_BYTES)
        } else {
            (
                self.area_bytes().saturating_sub(RESPONSE_BYTES),
                RESPONSE_BYTES,
            )
        }
    }

    /// Where a command of `len` bytes whose response is `response` bytes
    /// goes: a free slot if it fits one, else the large place.
    fn place_for(&self, len: usize, response: usize) -> Result<usize, SubmitError> {
        let free = |place: usize| self.in_flight.get(place).is_some_and(Option::is_none);
        if len <= SLOT_REQUEST_BYTES
            && response <= SLOT_RESPONSE_BYTES
            && let Some(slot) = (0..CONTROL_SLOTS).find(|&slot| free(slot))
        {
            return Ok(slot);
        }
        if len > self.request_of(LARGE).1 || response > RESPONSE_BYTES {
            return Err(SubmitError::TooLarge);
        }
        if free(LARGE) {
            Ok(LARGE)
        } else {
            Err(SubmitError::Busy)
        }
    }

    /// Post `command` and ring the doorbell: [`Driver::post`] and
    /// [`Driver::kick`], tagged 0, for a caller with one command at a time.
    ///
    /// # Errors
    ///
    /// As [`Driver::post`].
    pub fn submit(&mut self, command: &Command<'_>) -> Result<(), SubmitError> {
        self.submit_in(gpu::Context::NONE, command)
    }

    /// The same for a command that belongs to a context, or asks for a
    /// fence, or both: the header's fields rather than zeros.
    ///
    /// # Errors
    ///
    /// As [`Driver::post`].
    pub fn submit_in(
        &mut self,
        context: gpu::Context,
        command: &Command<'_>,
    ) -> Result<(), SubmitError> {
        self.post(0, context, command, &[])?;
        self.kick();
        Ok(())
    }

    /// Write `command` into a free place of the command area and publish it,
    /// with a response buffer, without ringing the doorbell: that is
    /// [`Driver::kick`]'s, once for everything posted.
    ///
    /// `following` is memory the device reads after the request, as
    /// `(device address, bytes)`: the stream of a
    /// [`Command::Submit3dHeader`], where its writer left it. It must stay
    /// as it is until the command is done.
    ///
    /// `tag` comes back in the command's [`Done`].
    ///
    /// # Errors
    ///
    /// [`SubmitError::Busy`] to try again after a completion;
    /// [`SubmitError::TooLarge`] for a command that would not fit even with
    /// nothing in flight.
    pub fn post(
        &mut self,
        tag: u64,
        context: gpu::Context,
        command: &Command<'_>,
        following: &[(u64, u32)],
    ) -> Result<(), SubmitError> {
        if self.fault.is_some() {
            return Err(SubmitError::Broken);
        }
        let len = command.len();
        let place = self.place_for(len, command.response_len())?;
        let (base, _) = self.request_of(place);
        let (response_at, response_room) = self.response_of(place);

        // One readable descriptor per run of device-consecutive pages the
        // request covers, then what follows it, then the response buffer.
        let mut buffers = [Buffer::readable(0, 0); MAX_CHAIN];
        let mut count = 0usize;
        let pages = self.area.device_pages();
        let page = PAGE_SIZE as usize;
        let mut offset = 0usize;
        while offset < len {
            let at = base + offset;
            let chunk = (page - at % page).min(len - offset);
            let address = contiguous(pages, at, chunk).ok_or(SubmitError::TooLarge)?;
            append(&mut buffers, &mut count, address, chunk)?;
            offset += chunk;
        }
        for &(address, bytes) in following {
            append(&mut buffers, &mut count, address, bytes as usize)?;
        }
        let response =
            contiguous(pages, response_at, response_room).ok_or(SubmitError::TooLarge)?;
        let slot = buffers.get_mut(count).ok_or(SubmitError::TooLarge)?;
        *slot = Buffer::writable(response, response_room as u32);
        count += 1;
        let chain = buffers.get(..count).ok_or(SubmitError::TooLarge)?;

        let area = &mut *self.area;
        let _ = command
            .write_with_in(context, |at, bytes| area.write_bytes(base + at, bytes))
            .map_err(SubmitError::Command)?;
        // The response buffer starts unwritten, so a device that completes
        // the chain without writing a response is caught.
        self.area.write_bytes(response_at, &[0; 4]);

        let head = match self.queue.add_chain(chain) {
            Ok(head) => head,
            Err(QueueError::ChainTooLong | QueueError::OutOfDescriptors) => {
                // Descriptors come back with completions; a chain that does
                // not fit an empty queue never will.
                return Err(if self.is_busy() {
                    SubmitError::Busy
                } else {
                    SubmitError::TooLarge
                });
            }
            Err(error) => {
                self.break_down(DeviceError::Queue(error));
                return Err(SubmitError::Device(DeviceError::Queue(error)));
            }
        };
        if let Some(held) = self.in_flight.get_mut(place) {
            *held = Some(InFlight {
                head,
                command: command.code(),
                tag,
            });
        }
        self.posted = true;
        Ok(())
    }

    /// Ring the control queue's doorbell once for everything posted since it
    /// last rang, unless the device said it is looking already.
    pub fn kick(&mut self) {
        if core::mem::take(&mut self.posted)
            && self.fault.is_none()
            && self.queue.device_wants_notification()
        {
            self.transport.notify(gpu::CONTROL_QUEUE, self.notify_off);
        }
    }

    /// Acknowledge the interrupt and see to the cursor queue. Returns the ISR
    /// bits, so a display change is not lost; the control queue's
    /// completions are [`Driver::take_done`]'s.
    ///
    /// [`ISR_CONFIG`] in the answer says the configuration may have changed
    /// and [`Driver::display_changed`] is worth asking: the ISR said so, or
    /// -- under MSI-X, which has no ISR byte and shares the control queue's
    /// vector with configuration changes -- the interrupt brought no
    /// completion on either queue, which is what a configuration change's
    /// looks like, or it is the [`CONFIG_LOOK_EVERY`]th. Only then is the
    /// device status read for `DEVICE_NEEDS_RESET`, which the specification
    /// has a device announce with a configuration change (virtio 1.2
    /// §2.1.2). Both are register reads that leave the guest; under KVM
    /// each waits for QEMU's lock, which QEMU holds while it shows a frame,
    /// and read on every interrupt they were nine tenths of this driver's
    /// time with a video playing.
    ///
    /// # Errors
    ///
    /// How the device broke the protocol.
    pub fn on_interrupt(&mut self) -> Result<u8, DeviceError> {
        let isr = self.transport.acknowledge_interrupt();
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        self.interrupts = self.interrupts.wrapping_add(1);
        let news = self.queue.has_used() || self.cursor_queue.has_used();
        let config =
            isr & ISR_CONFIG != 0 || !news || self.interrupts.is_multiple_of(CONFIG_LOOK_EVERY);
        let isr = if config {
            isr | ISR_CONFIG
        } else {
            isr & !ISR_CONFIG
        };
        if config && self.transport.read8(DEVICE_STATUS) & STATUS_DEVICE_NEEDS_RESET != 0 {
            self.break_down(DeviceError::NeedsReset);
            return Err(DeviceError::NeedsReset);
        }
        // The cursor queue raises no interrupt of its own, so whatever woke
        // the driver is when its slots come back and what waited for one
        // goes out.
        if let Err(SubmitError::Device(error)) = self.pump_cursor() {
            return Err(error);
        }
        Ok(isr)
    }

    /// Take one command the device has finished, if there is one: its tag
    /// and its outcome. Ask until `None` after every interrupt, since one
    /// interrupt may stand for many completions.
    ///
    /// # Errors
    ///
    /// How the device broke the protocol.
    pub fn take_done(&mut self) -> Result<Option<Done>, DeviceError> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        let used = match self.queue.take_used() {
            Ok(Some(used)) => used,
            Ok(None) => return Ok(None),
            Err(error) => {
                self.break_down(DeviceError::Queue(error));
                return Err(DeviceError::Queue(error));
            }
        };
        let Some((place, flight)) = self
            .in_flight
            .iter_mut()
            .enumerate()
            .find(|(_, held)| held.is_some_and(|flight| flight.head == used.head))
            .and_then(|(place, held)| Some((place, held.take()?)))
        else {
            let error = DeviceError::UnknownChain(used.head);
            self.break_down(error);
            return Err(error);
        };
        let (at, room) = self.response_of(place);
        let mut response = [0u8; RESPONSE_BYTES];
        let written = (used.written as usize).min(room);
        if let Some(bytes) = response.get_mut(..written) {
            self.area.read_bytes(at, bytes);
        }
        let result = match Response::parse_for(
            flight.command,
            response.get(..room).unwrap_or_default(),
            used.written,
        ) {
            Ok(response) => Ok(response),
            Err(GpuError::Device(refusal)) => Err(refusal),
            Err(error) => {
                self.break_down(DeviceError::Protocol(error));
                return Err(DeviceError::Protocol(error));
            }
        };
        self.last_taken = Some(place);
        Ok(Some(Done {
            tag: flight.tag,
            command: flight.command,
            result,
        }))
    }

    /// Copy out what followed the header of the response
    /// [`Driver::take_done`] last handed back, from `offset` bytes into it: a
    /// capability set's bytes, which [`Response::Capset`] gives only the
    /// length of.
    ///
    /// The response stays where the device wrote it until another command is
    /// posted, so this is asked between the two. Bytes past the response
    /// buffer's end are left as they were.
    pub fn read_response(&self, offset: usize, out: &mut [u8]) {
        let Some(place) = self.last_taken else {
            return;
        };
        let (at, room) = self.response_of(place);
        let room = room.saturating_sub(gpu::HEADER_LEN);
        let len = room.saturating_sub(offset).min(out.len());
        if let Some(out) = out.get_mut(..len) {
            self.area.read_bytes(at + gpu::HEADER_LEN + offset, out);
        }
    }

    /// Show `resource_id` -- a resource [`gpu::CURSOR_SIZE`] pixels square,
    /// its pixels already on the device -- as `scanout`'s cursor with its
    /// hotspot at (`hot_x`, `hot_y`), where the cursor last was: an
    /// `UPDATE_CURSOR`. Resource 0 is Linux's way of saying none, and QEMU's
    /// of keeping the last image and hiding it from a window.
    ///
    /// # Errors
    ///
    /// A scanout the device does not have, or the device's.
    pub fn update_cursor(
        &mut self,
        scanout: u32,
        resource_id: u32,
        hot_x: u32,
        hot_y: u32,
    ) -> Result<(), SubmitError> {
        let cursor = self.cursor(scanout)?;
        cursor.resource_id = resource_id;
        cursor.hot_x = hot_x;
        cursor.hot_y = hot_y;
        self.owe_cursor(scanout, true)
    }

    /// Put `scanout`'s cursor's top-left corner at (`x`, `y`): a
    /// `MOVE_CURSOR`, or nothing while it shows no image, whose next
    /// [`Driver::update_cursor`] carries the place.
    ///
    /// # Errors
    ///
    /// A scanout the device does not have, or the device's.
    pub fn move_cursor(&mut self, scanout: u32, x: i32, y: i32) -> Result<(), SubmitError> {
        let cursor = self.cursor(scanout)?;
        cursor.x = x;
        cursor.y = y;
        if cursor.resource_id == 0 {
            return Ok(());
        }
        self.owe_cursor(scanout, false)
    }

    /// Where `scanout`'s cursor is, without telling the device: the place
    /// the next update or move carries.
    ///
    /// # Errors
    ///
    /// A scanout the device does not have.
    pub fn place_cursor(&mut self, scanout: u32, x: i32, y: i32) -> Result<(), SubmitError> {
        let cursor = self.cursor(scanout)?;
        cursor.x = x;
        cursor.y = y;
        Ok(())
    }

    fn cursor(&mut self, scanout: u32) -> Result<&mut gpu::Cursor, SubmitError> {
        if scanout >= self.info.config.num_scanouts {
            return Err(SubmitError::NoSuchScanout);
        }
        self.cursors
            .get_mut(scanout as usize)
            .ok_or(SubmitError::NoSuchScanout)
    }

    fn owe_cursor(&mut self, scanout: u32, update: bool) -> Result<(), SubmitError> {
        if let Some(owed) = self.cursor_owed.get_mut(scanout as usize) {
            *owed = Some(owed.unwrap_or(false) || update);
        }
        self.pump_cursor()
    }

    /// Take back the cursor area's slots the device has finished with, post
    /// what each scanout's cursor is owed into as many as are free, and ring
    /// the doorbell once for all of them.
    ///
    /// # Errors
    ///
    /// [`SubmitError::Broken`] for a failed device, and the device's when it
    /// broke the queue.
    pub fn pump_cursor(&mut self) -> Result<(), SubmitError> {
        if self.fault.is_some() {
            return Err(SubmitError::Broken);
        }
        loop {
            let used = match self.cursor_queue.take_used() {
                Ok(Some(used)) => used,
                Ok(None) => break,
                Err(error) => {
                    self.break_down(DeviceError::Queue(error));
                    return Err(SubmitError::Device(DeviceError::Queue(error)));
                }
            };
            let Some(slot) = self
                .cursor_slots
                .iter_mut()
                .find(|slot| **slot == Some(used.head))
            else {
                let error = DeviceError::UnknownChain(used.head);
                self.break_down(error);
                return Err(SubmitError::Device(error));
            };
            *slot = None;
        }
        let mut posted = false;
        for scanout in 0..gpu::MAX_SCANOUTS {
            let Some(Some(update)) = self.cursor_owed.get(scanout).copied() else {
                continue;
            };
            let Some(slot) = self.cursor_slots.iter().position(Option::is_none) else {
                break;
            };
            let Some(cursor) = self.cursors.get(scanout).copied() else {
                continue;
            };
            let at = slot * CURSOR_SLOT_BYTES;
            let address = contiguous(self.cursor_area.device_pages(), at, gpu::CURSOR_LEN)
                .ok_or(SubmitError::TooLarge)?;
            for (index, &byte) in cursor.encode(update).iter().enumerate() {
                self.cursor_area.write_u8(at + index, byte);
            }
            let head = match self
                .cursor_queue
                .add_chain(&[Buffer::readable(address, gpu::CURSOR_LEN as u32)])
            {
                Ok(head) => head,
                Err(QueueError::ChainTooLong | QueueError::OutOfDescriptors) => break,
                Err(error) => {
                    self.break_down(DeviceError::Queue(error));
                    return Err(SubmitError::Device(DeviceError::Queue(error)));
                }
            };
            if let Some(held) = self.cursor_slots.get_mut(slot) {
                *held = Some(head);
            }
            if let Some(owed) = self.cursor_owed.get_mut(scanout) {
                *owed = None;
            }
            posted = true;
        }
        if posted && self.cursor_queue.device_wants_notification() {
            self.transport
                .notify(gpu::CURSOR_QUEUE, self.cursor_notify_off);
        }
        Ok(())
    }

    /// Whether the device's displays changed since this was last asked --
    /// `VIRTIO_GPU_EVENT_DISPLAY`, which QEMU raises when the window a
    /// scanout is shown in is resized -- acknowledging it. `GET_DISPLAY_INFO`
    /// says what they are now.
    ///
    /// The event register is read rather than the ISR's configuration bit,
    /// because under MSI-X there is no ISR byte and the configuration's
    /// vector is the control queue's: after any interrupt, this is the
    /// question to ask.
    pub fn display_changed(&mut self) -> bool {
        self.take_events() & gpu::EVENT_DISPLAY != 0
    }

    /// Read the configuration's pending events and acknowledge them.
    pub fn take_events(&mut self) -> u32 {
        let events = self.transport.config_read32(gpu::CONFIG_EVENTS_READ);
        if events != 0 {
            self.transport
                .config_write32(gpu::CONFIG_EVENTS_CLEAR, events);
        }
        events
    }

    /// Reset the device and hand everything back, the memory only if the
    /// reset finished.
    pub fn shutdown(self) -> Teardown<T, R, A> {
        let Self {
            transport,
            queue,
            area,
            reset_polls,
            cursor_queue,
            cursor_area,
            ..
        } = self;
        teardown(
            transport,
            Rings::Queue(ManuallyDrop::into_inner(queue)),
            ManuallyDrop::into_inner(area),
            CursorParts {
                rings: Rings::Queue(ManuallyDrop::into_inner(cursor_queue)),
                area: ManuallyDrop::into_inner(cursor_area),
            },
            reset_polls,
        )
    }
}
