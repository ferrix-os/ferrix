//! A virtio-net driver, as logic over a transport and DMA memory it is handed.
//!
//! This is `ferrix-virtio-blk` for the network device, over the same four
//! traits and with the same two invariants, so what follows says only what is
//! different. A driver process holds an `IoMapping` of the device's register
//! blocks, an `Interrupt` and pinned DMA memory whose device addresses came
//! from an IOMMU domain, none of which can be had in a unit test; so the
//! driver is written against [`Transport`], [`DevicePages`], [`RequestArea`]
//! and [`ferrix_virtio::QueueMemory`], and this crate is the order of the
//! status protocol, which features to take, how a frame becomes a chain and a
//! completion becomes an event, and what to do when the device lies.
//!
//! # Two queues, and what each is for
//!
//! virtio 1.2 §5.1.2 gives the device a receive queue (0) and a transmit queue
//! (1), and a control queue (2) this driver does not negotiate. The two are
//! not symmetrical and the asymmetry is the shape of the whole crate:
//!
//! * **Receive** buffers are the *driver's*. It carves
//!   [`Info::receive_buffers`] of them out of the receive data region, posts
//!   every one at bring-up, and posts them again as they come back, because a
//!   receive queue with no buffers in it drops every frame that arrives and
//!   says nothing. A frame that arrives is answered as [`Event::Received`] —
//!   an offset and a length in that region — and the buffer stays out of the
//!   ring until the caller has read it and called [`Driver::release`].
//! * **Transmit** buffers are the *caller's*. It writes a frame into the
//!   transmit data region, names it with an id, and gets that id back in
//!   [`Event::Sent`] when the device is done reading, which is the only
//!   moment the bytes may be overwritten.
//!
//! Each frame is one chain: a header descriptor pointing into the request
//! area, then one descriptor per run of device-contiguous pages the frame
//! covers. Because [`ferrix_virtio::net::FEATURE_MRG_RXBUF`] is declined, a
//! received frame is one buffer and never a reassembly; see that module for
//! the argument.
//!
//! # DMA memory is freed only after a reset
//!
//! As in `ferrix-virtio-blk`: a device holds addresses into the rings, the
//! headers and the frame regions for as long as it is not reset, so the driver
//! keeps them in [`ManuallyDrop`], dropping a [`Driver`] leaks rather than
//! frees, and only [`Driver::shutdown`] gives them back — as
//! [`Teardown::Released`] if the reset finished and [`Teardown::Wedged`] if it
//! did not.
//!
//! # Every id is answered exactly once
//!
//! Every id [`Driver::submit`] accepted — that is, every one it returned
//! `Ok` for — is answered exactly once: by an [`Event::Sent`] from
//! [`Driver::on_interrupt`], or — once the device has broken the protocol and
//! the driver has stopped trusting it — by [`Released::abandoned`] after the
//! reset. An id `submit` refused is not tracked at all, whether it was refused
//! for a reason of the caller's or because the device broke the protocol while
//! the frame was being published: the caller was told synchronously, which is
//! the whole of the answer it gets.
//!
//! # Trust
//!
//! Everything the device says is checked before it is acted on: the used rings
//! by [`SplitQueue`], a completion's length and a received header by
//! [`net::parse_receipt`] and [`net::parse_sent`], the configuration by
//! [`net::Config::read`] and [`net::Limits::new`], read again if
//! `config_generation` moves. A device that fails a check is marked failed, is
//! sent nothing more, and waits for [`Driver::shutdown`].

#![no_std]
// Forbidden in the shipped library. The tests implement
// `ferrix_virtio::QueueMemory`, an unsafe trait, for their fake memory, and a
// forbid cannot be relaxed for one item; there it is denied, and the one
// implementation is exempted and argued at its site.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

use core::fmt;
use core::mem::ManuallyDrop;

use ferrix_virtio::net::{self, Chain, Config, Data, Header, Limits, NetError, Segments};
use ferrix_virtio::pci::{
    self, CONFIG_GENERATION, CommonConfig, DEVICE_STATUS, QueueAddresses,
    STATUS_DEVICE_NEEDS_RESET, STATUS_FAILED, TransportError,
};
use ferrix_virtio::{
    DeviceConfig, Layout, MAX_QUEUE_SIZE, PAGE_SIZE, QueueError, QueueMemory, SplitQueue,
};

// At the root, so every test module can name `std`.
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

/// The receive queue: virtio-net's queue 0.
pub const RECEIVE_QUEUE: u16 = 0;

/// The transmit queue: virtio-net's queue 1.
pub const TRANSMIT_QUEUE: u16 = 1;

/// ISR status bit: a queue has something for the driver (virtio 1.2 §4.1.4.5).
pub const ISR_QUEUE: u8 = 1;

/// ISR status bit: the device configuration changed — a link that came up or
/// went down.
pub const ISR_CONFIG: u8 = 2;

/// The smallest queue worth running: two chains of a header and a frame, with
/// room for another beside them.
const MIN_QUEUE_SIZE: u16 = 4;

/// Bytes of request area one header slot takes.
///
/// A header is [`net::HEADER_LEN`] bytes and the slots are sixteen apart
/// anyway, because sixteen divides a page and twelve does not. A header that
/// straddled two pages whose device addresses do not follow on could not be
/// one descriptor, and every header here is exactly one.
const AREA_PER_HEADER: u64 = 16;

/// The device's registers, as the process that drives it reaches them.
///
/// The [`CommonConfig`] accessors are the common configuration block, exactly
/// as the kernel implements them over its mapping; the [`DeviceConfig`]
/// accessors are the device-specific block. Both take offsets within their
/// block.
pub trait Transport: CommonConfig + DeviceConfig {
    /// Ring the doorbell for `queue`, whose `queue_notify_off` the device
    /// reported as `notify_off`: write `queue` as a `u16` at
    /// `notify_off × notify_off_multiplier` in the notification block
    /// ([`pci::notify_offset`]).
    fn notify(&mut self, queue: u16, notify_off: u16);

    /// The MSI-X table entry `queue` should interrupt through, or
    /// [`pci::NO_VECTOR`] for a line interrupt. A device with one vector for
    /// both queues answers the same number twice, which is allowed: the
    /// handler drains both either way.
    fn queue_vector(&self, queue: u16) -> u16;

    /// Acknowledge the interrupt that woke the driver and say why it came:
    /// the ISR status byte, whose read clears it, for a line interrupt; or
    /// [`ISR_QUEUE`] for MSI-X, whose vector already says.
    fn acknowledge_interrupt(&mut self) -> u8;
}

/// Pinned memory, as the device addresses of its pages.
///
/// The addresses are the ones the pin's address query (native call 0x1026)
/// wrote after `VMO_PIN` (0x1025) pinned the range into the device's domain.
/// The driver must not assume any relation between them and physical
/// addresses: under a translating IOMMU the device reaches only what its
/// domain maps, and where the kernel maps a page is the kernel's choice.
pub trait DevicePages {
    /// The device address of page `i` of the pinned range, for every page in
    /// order: [`PAGE_SIZE`] bytes each, and not necessarily consecutive.
    fn device_pages(&self) -> &[u64];
}

/// The memory a driver keeps virtio headers in.
///
/// Offsets are bytes from the start of the region; the driver passes only
/// offsets below `device_pages().len() × PAGE_SIZE`.
pub trait RequestArea: DevicePages {
    /// Read the byte at `offset`.
    fn read_u8(&self, offset: usize) -> u8;
    /// Write the byte at `offset`.
    fn write_u8(&mut self, offset: usize, value: u8);
}

/// One frame to send, as it sits in the transmit data region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame {
    /// The caller's name for it, handed back when the device is done with it.
    pub id: u64,
    /// Where the frame starts, in bytes from the start of the transmit data
    /// region.
    pub offset: u64,
    /// How many bytes: the whole Ethernet frame, with no virtio header and no
    /// frame check sequence.
    pub len: u32,
}

/// Something the device did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// A frame arrived. The bytes are `len` from `offset` of the receive data
    /// region, and stay there until [`Driver::release`] gives the buffer back.
    Received {
        /// The buffer it came in, for [`Driver::release`].
        buffer: u16,
        /// Where the frame starts in the receive data region.
        offset: u64,
        /// How many bytes of frame, without the virtio header.
        len: u32,
    },
    /// A frame was sent: its bytes in the transmit data region are the
    /// caller's again.
    Sent {
        /// The id [`Driver::submit`] was given.
        id: u64,
    },
}

/// What [`Driver::on_interrupt`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Drained {
    /// Events written to the start of the caller's slice.
    pub events: usize,
    /// The device says its configuration changed; see
    /// [`Driver::refresh_config`].
    pub config_changed: bool,
    /// The device has completed more than the slice had room for.
    pub more: bool,
    /// Receive buffers posted again after the drain.
    pub refilled: u16,
}

/// How [`Driver::init`] should go about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    /// Reads of `device_status` a reset may take.
    pub reset_polls: u32,
    /// The largest queue wanted; the device, the memory and the slots may make
    /// it smaller.
    pub max_queue_size: u16,
    /// Reads of the configuration before giving up on one that keeps changing
    /// under the driver.
    pub config_attempts: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            reset_polls: 100_000,
            max_queue_size: 256,
            config_attempts: 8,
        }
    }
}

/// What the driver agreed with the device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    /// The features negotiated.
    pub features: u64,
    /// The device's configuration, as last read.
    pub config: Config,
    /// What a frame may be.
    pub limits: Limits,
    /// The address the device was given, which [`net::REQUIRED_FEATURES`]
    /// makes sure there is one of.
    pub mac: [u8; net::MAC_LEN],
    /// Whether the link was up when the configuration was last read.
    pub link_up: bool,
    /// Entries in each queue; both are the same size.
    pub queue_size: u16,
    /// Receive buffers the driver carved out of the receive data region.
    ///
    /// Half the queue, because with [`Info::receive_stride`] rounded up as it
    /// is a receive chain is a header descriptor and one frame descriptor:
    /// posting more buffers than that could never fill, and posting fewer
    /// would leave descriptors idle.
    pub receive_buffers: u16,
    /// Bytes from one receive buffer to the next.
    ///
    /// The frame capacity rounded up to a power of two, which is what keeps a
    /// buffer inside one page when it fits in one and on a page boundary when
    /// it does not. A buffer straddling two pages whose device addresses do
    /// not follow on would need two descriptors, and the whole queue is sized
    /// on each needing one.
    pub receive_stride: u32,
    /// The MSI-X vector the receive queue interrupts through, or `NO_VECTOR`.
    pub receive_vector: u16,
    /// The MSI-X vector the transmit queue interrupts through.
    pub transmit_vector: u16,
}

/// Bookkeeping for one queue entry.
///
/// The caller provides at least twice as many as the queue has entries — see
/// [`Options::max_queue_size`] — so the driver allocates nothing. The first
/// half belong to the receive queue and the second half to the transmit
/// queue, each indexed by the head descriptor of the chain in flight, or by
/// the header slot in use.
/// The two fields are indexed differently, which is the one thing to hold on
/// to about this type: `chain` belongs to the chain whose *head descriptor* is
/// this index, and `busy`/`held` to the *header slot* of this index. They are
/// unrelated, and a receive buffer's chain very often ends up recorded in some
/// other buffer's slot.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    /// The chain whose head descriptor is this slot's index within its bank.
    chain: Option<ChainRecord>,
    /// Whether this slot's header — and, in the receive bank, its frame
    /// buffer — is in use, either by the device or by the caller.
    busy: bool,
    /// Whether the caller holds this slot's receive buffer, having been handed
    /// the frame in it and not yet given it back.
    held: bool,
}

impl Slot {
    /// A slot with nothing in it.
    pub const EMPTY: Slot = Slot {
        chain: None,
        busy: false,
        held: false,
    };
}

impl Default for Slot {
    fn default() -> Self {
        Slot::EMPTY
    }
}

/// One published chain.
#[derive(Clone, Copy, Debug)]
struct ChainRecord {
    /// The header slot it uses, which in the receive bank is also its frame
    /// buffer.
    index: u16,
    /// The caller's id, for a frame being sent; `None` for a receive buffer,
    /// which nobody is waiting on.
    id: Option<u64>,
    /// What was published.
    chain: Chain,
}

/// Why a device could not be brought up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// The status protocol failed: a reset that did not finish, a device
    /// without `VERSION_1` or without a MAC, refused features, a missing
    /// queue, or a device that needs a reset.
    Transport(TransportError),
    /// The configuration is truncated or advises an unusable MTU.
    Config(NetError),
    /// `config_generation` changed on every read.
    ConfigUnstable,
    /// No queue of at least four entries fits the device, the rings' pages,
    /// the header area, the receive frames and the slots together.
    NoRoom,
    /// The device would not give a queue the MSI-X vector asked for.
    VectorRefused {
        /// The queue.
        queue: u16,
        /// The vector asked for.
        asked: u16,
        /// What the device kept.
        kept: u16,
    },
    /// The device broke the protocol while the receive queue was being filled.
    Device(DeviceError),
}

/// Why a frame was not accepted.
///
/// A refused frame is never tracked: whichever of these came back, the id is
/// not in flight, no [`Event::Sent`] will name it and [`Released::abandoned`]
/// will not either. The error is the answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// The device has already failed; shut the driver down.
    Broken,
    /// A frame of no bytes.
    Empty,
    /// Longer than [`Limits::frame_capacity`], which is the MTU and an
    /// Ethernet header.
    TooLong,
    /// The frame is past the end of the transmit data region.
    OutsideData,
    /// A page's device address overflows.
    BadAddress,
    /// The frame is scattered over more pages than one chain can name.
    TooManySegments,
    /// No free header slot or not enough free descriptors now; take some
    /// completions and try again.
    QueueFull,
    /// The device broke the protocol while the frame was being published, so
    /// the driver has failed and will send nothing more. The frame did not go
    /// out.
    Device(DeviceError),
}

/// Why a receive buffer could not be given back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReleaseError {
    /// There is no such buffer.
    OutOfRange(u16),
    /// That buffer is not one the caller was handed: it is in the ring, or it
    /// was released already.
    NotHeld(u16),
}

/// How the device broke the protocol. After any of these the driver has set
/// `FAILED` and sends nothing more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceError {
    /// The driver has already failed on an earlier error.
    Broken,
    /// The device set `DEVICE_NEEDS_RESET`.
    NeedsReset,
    /// The rings say something impossible.
    Queue(QueueError),
    /// A completion's length, or a received header, is impossible.
    Protocol(NetError),
    /// A completion names a chain the driver has no record of.
    UnknownChain {
        /// The queue it came back on.
        queue: u16,
        /// The head descriptor named.
        head: u16,
    },
    /// The configuration changed on every read.
    ConfigUnstable,
    /// The driver's own records disagree with the queue — which only memory
    /// the device can write can have caused.
    Bookkeeping,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            InitError::Transport(error) => write!(f, "{error}"),
            InitError::Config(error) => write!(f, "{error}"),
            InitError::ConfigUnstable => f.write_str("the configuration kept changing"),
            InitError::NoRoom => f.write_str("no queue fits the memory given"),
            InitError::VectorRefused { queue, asked, kept } => {
                write!(f, "queue {queue} kept vector {kept:#x} for {asked:#x}")
            }
            InitError::Device(error) => write!(f, "{error}"),
        }
    }
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            SubmitError::Broken => "the device has failed",
            SubmitError::Empty => "the frame has no bytes",
            SubmitError::TooLong => "the frame is longer than the MTU allows",
            SubmitError::OutsideData => "the frame is past the end of its region",
            SubmitError::BadAddress => "a page's address overflows",
            SubmitError::TooManySegments => "the frame is scattered over too many pages",
            SubmitError::QueueFull => "the transmit queue is full",
            SubmitError::Device(error) => return write!(f, "{error}"),
        })
    }
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ReleaseError::OutOfRange(buffer) => write!(f, "there is no buffer {buffer}"),
            ReleaseError::NotHeld(buffer) => write!(f, "buffer {buffer} is not held"),
        }
    }
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DeviceError::Broken => f.write_str("the device has failed"),
            DeviceError::NeedsReset => f.write_str("the device needs a reset"),
            DeviceError::Queue(error) => write!(f, "the rings are corrupt: {error:?}"),
            DeviceError::Protocol(error) => write!(f, "{error}"),
            DeviceError::UnknownChain { queue, head } => {
                write!(f, "chain {head} is not in flight on queue {queue}")
            }
            DeviceError::ConfigUnstable => f.write_str("the configuration kept changing"),
            DeviceError::Bookkeeping => f.write_str("the driver's records disagree with the queue"),
        }
    }
}

/// Everything a driver is built from.
pub struct Parts<T, R, A, D, S> {
    /// The device's registers.
    pub transport: T,
    /// The receive queue's rings: at least one page.
    pub receive_rings: R,
    /// The transmit queue's rings, likewise.
    pub transmit_rings: R,
    /// Virtio headers: sixteen bytes per queue entry, for both queues.
    pub area: A,
    /// Where received frames are put.
    pub receive_data: D,
    /// Where frames to send are written by the caller.
    pub transmit_data: D,
    /// Bookkeeping, two slots an entry.
    pub slots: S,
}

/// A queue's memory as it comes back: never given to a queue, or inside the
/// queue it was built into.
pub enum Rings<R> {
    /// Initialisation stopped before the queue was built.
    Unused(R),
    /// The queue, with the memory inside it.
    Queue(SplitQueue<R>),
}

/// A driver's parts, handed back.
pub struct Released<T, R, A, D, S> {
    /// The device's registers.
    pub transport: T,
    /// The receive queue's memory.
    pub receive_rings: Rings<R>,
    /// The transmit queue's memory.
    pub transmit_rings: Rings<R>,
    /// The header area.
    pub area: A,
    /// The receive data region.
    pub receive_data: D,
    /// The transmit data region.
    pub transmit_data: D,
    /// The bookkeeping.
    pub slots: S,
}

impl<T, R, A, D, S: AsRef<[Slot]>> Released<T, R, A, D, S> {
    /// The ids of frames accepted and never answered, which the caller must
    /// now treat as lost.
    pub fn abandoned(&self) -> impl Iterator<Item = u64> + '_ {
        self.slots
            .as_ref()
            .iter()
            .filter_map(|slot| slot.chain.and_then(|record| record.id))
    }
}

/// How a driver ended.
pub enum Teardown<T, R, A, D, S> {
    /// The device reset: it holds no address into this memory, which may be
    /// unpinned.
    Released(Released<T, R, A, D, S>),
    /// The device did not reset and may still write to this memory, so it is
    /// never dropped. Take it out of the `ManuallyDrop` only to leak it
    /// somewhere else.
    Wedged(ManuallyDrop<Released<T, R, A, D, S>>),
}

/// A failed [`Driver::init`]: why, and the parts.
pub struct InitFailure<T, R, A, D, S> {
    /// What went wrong.
    pub error: InitError,
    /// The parts, released only if the reset after the failure finished.
    pub teardown: Teardown<T, R, A, D, S>,
}

impl<R> fmt::Debug for Rings<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Rings::Unused(_) => "Rings::Unused",
            Rings::Queue(_) => "Rings::Queue",
        })
    }
}

impl<T, R, A, D, S> fmt::Debug for Parts<T, R, A, D, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parts").finish_non_exhaustive()
    }
}

impl<T, R, A, D, S> fmt::Debug for Released<T, R, A, D, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Released")
            .field("receive_rings", &self.receive_rings)
            .field("transmit_rings", &self.transmit_rings)
            .finish_non_exhaustive()
    }
}

impl<T, R, A, D, S> fmt::Debug for Teardown<T, R, A, D, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Teardown::Released(released) => f.debug_tuple("Released").field(released).finish(),
            Teardown::Wedged(released) => f.debug_tuple("Wedged").field(&**released).finish(),
        }
    }
}

impl<T, R, A, D, S> fmt::Debug for InitFailure<T, R, A, D, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitFailure")
            .field("error", &self.error)
            .field("teardown", &self.teardown)
            .finish()
    }
}

/// Everything the driver's memory is, before a queue has been built from any
/// of it.
struct Memory<R, A, D, S> {
    /// The receive queue's rings.
    receive_rings: R,
    /// The transmit queue's rings.
    transmit_rings: R,
    /// The header area.
    area: A,
    /// The receive data region.
    receive_data: D,
    /// The transmit data region.
    transmit_data: D,
    /// The bookkeeping.
    slots: S,
}

/// Reset the device, and hand the parts back as that went.
fn teardown<T: CommonConfig, R, A, D, S>(
    mut transport: T,
    receive_rings: Rings<R>,
    transmit_rings: Rings<R>,
    rest: (A, D, D, S),
    polls: u32,
) -> Teardown<T, R, A, D, S> {
    let reset = pci::reset(&mut transport, polls);
    let (area, receive_data, transmit_data, slots) = rest;
    let released = Released {
        transport,
        receive_rings,
        transmit_rings,
        area,
        receive_data,
        transmit_data,
        slots,
    };
    match reset {
        Ok(()) => Teardown::Released(released),
        Err(_) => Teardown::Wedged(ManuallyDrop::new(released)),
    }
}

/// Set `FAILED`, keeping the rest of the status.
fn set_failed<T: CommonConfig + ?Sized>(transport: &mut T) {
    let status = transport.read8(DEVICE_STATUS);
    transport.write8(DEVICE_STATUS, status | STATUS_FAILED);
}

/// Read the configuration, again while `config_generation` moves under it.
fn read_config<T: Transport>(
    transport: &T,
    features: u64,
    attempts: u32,
) -> Result<Config, InitError> {
    for _ in 0..attempts.max(1) {
        let before = transport.read8(CONFIG_GENERATION);
        let config = Config::read(transport, features);
        if transport.read8(CONFIG_GENERATION) == before {
            return config.map_err(InitError::Config);
        }
    }
    Err(InitError::ConfigUnstable)
}

/// The device address of `len` bytes at `start` in a region of `pages`, if
/// they are device-contiguous.
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
    let address = base.checked_add(u64::try_from(start % page).ok()?)?;
    let _ = address.checked_add(u64::try_from(len).ok()?)?;
    Some(address)
}

/// Where the device finds each of `layout`'s three areas in `pages`, if each
/// is device-contiguous.
fn ring_addresses(layout: &Layout, pages: &[u64]) -> Option<QueueAddresses> {
    let region = u64::try_from(pages.len()).ok()?.checked_mul(PAGE_SIZE)?;
    if u64::try_from(layout.total_size).ok()? > region {
        return None;
    }
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

/// Bytes from one receive buffer to the next: the frame capacity rounded up
/// to a power of two. See [`Info::receive_stride`].
fn receive_stride(limits: &Limits) -> u32 {
    limits.frame_capacity.next_power_of_two()
}

/// Bytes a region of `pages` pages holds.
fn region_bytes(pages: &[u64]) -> u64 {
    u64::try_from(pages.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(PAGE_SIZE)
}

/// The device addresses of every region, for sizing the queues.
struct Regions<'a> {
    /// The receive queue's rings.
    receive_rings: &'a [u64],
    /// The transmit queue's rings.
    transmit_rings: &'a [u64],
    /// The header area.
    area: &'a [u64],
    /// The receive data region.
    receive_data: &'a [u64],
}

/// A queue size that fits everything, and where each queue's rings are.
struct Sizing {
    /// The layout both queues share.
    layout: Layout,
    /// The receive queue's ring addresses.
    receive: QueueAddresses,
    /// The transmit queue's ring addresses.
    transmit: QueueAddresses,
}

/// The largest queue that fits everything, and where the rings are.
///
/// A smaller queue is tried when an area straddles pages whose device
/// addresses do not follow on, since a smaller table may fit inside one page.
fn choose_queue(
    device_max: u16,
    wanted: u16,
    slots: usize,
    regions: &Regions<'_>,
    limits: &Limits,
) -> Result<Sizing, InitError> {
    let cap = device_max
        .min(wanted)
        .min(MAX_QUEUE_SIZE)
        // Two banks of bookkeeping, one for each queue.
        .min(u16::try_from(slots / 2).unwrap_or(u16::MAX));
    if cap < MIN_QUEUE_SIZE {
        return Err(InitError::NoRoom);
    }
    let area = region_bytes(regions.area);
    let frames = region_bytes(regions.receive_data);
    // The largest power of two no larger than `cap`, which is at least four.
    let mut size: u16 = 1 << (15 - cap.leading_zeros());
    while size >= MIN_QUEUE_SIZE {
        if u64::from(size) * 2 * AREA_PER_HEADER <= area
            && u64::from(size / 2) * u64::from(receive_stride(limits)) <= frames
            && let Ok(layout) = Layout::for_size(size)
            && let Some(receive) = ring_addresses(&layout, regions.receive_rings)
            && let Some(transmit) = ring_addresses(&layout, regions.transmit_rings)
        {
            return Ok(Sizing {
                layout,
                receive,
                transmit,
            });
        }
        size /= 2;
    }
    Err(InitError::NoRoom)
}

/// What negotiation settled, before either queue is built.
struct Plan {
    /// The features agreed.
    features: u64,
    /// The configuration.
    config: Config,
    /// What a frame may be.
    limits: Limits,
    /// The queues' size and addresses.
    sizing: Sizing,
}

/// Reset, agree on features, read the configuration and size the queues.
fn negotiate<T: Transport>(
    transport: &mut T,
    regions: &Regions<'_>,
    slots: usize,
    options: &Options,
) -> Result<Plan, InitError> {
    let features = pci::negotiate(
        transport,
        net::DRIVER_FEATURES,
        net::REQUIRED_FEATURES,
        options.reset_polls,
    )
    .map_err(InitError::Transport)?;
    let config = read_config(transport, features, options.config_attempts)?;
    let limits = Limits::new(&config, features).map_err(InitError::Config)?;
    let receive_max =
        pci::queue_max_size(transport, RECEIVE_QUEUE).map_err(InitError::Transport)?;
    let transmit_max =
        pci::queue_max_size(transport, TRANSMIT_QUEUE).map_err(InitError::Transport)?;
    let sizing = choose_queue(
        receive_max.min(transmit_max),
        options.max_queue_size,
        slots,
        regions,
        &limits,
    )?;
    Ok(Plan {
        features,
        config,
        limits,
        sizing,
    })
}

/// A virtio-net device, brought up and driven.
pub struct Driver<T, R, A, D, S> {
    /// The device's registers.
    transport: T,
    /// The receive queue, never dropped but by [`Driver::shutdown`].
    receive: ManuallyDrop<SplitQueue<R>>,
    /// The transmit queue, likewise.
    transmit: ManuallyDrop<SplitQueue<R>>,
    /// The headers, likewise.
    area: ManuallyDrop<A>,
    /// Where received frames go, likewise.
    receive_data: ManuallyDrop<D>,
    /// Where frames to send come from, likewise.
    transmit_data: ManuallyDrop<D>,
    /// Bookkeeping.
    slots: S,
    /// What was agreed.
    info: Info,
    /// The receive queue's `queue_notify_off`.
    receive_notify_off: u16,
    /// The transmit queue's `queue_notify_off`.
    transmit_notify_off: u16,
    /// Reads a reset may take.
    reset_polls: u32,
    /// Reads of the configuration before giving up.
    config_attempts: u32,
    /// The error the device broke the protocol with, once it has.
    fault: Option<DeviceError>,
    /// Frames accepted and not yet answered.
    sending: u16,
    /// Receive buffers the caller holds.
    held: u16,
}

impl<T, R, A, D, S> fmt::Debug for Driver<T, R, A, D, S> {
    /// Leaves out the memory, whose reads have side effects for the device.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("info", &self.info)
            .field("fault", &self.fault)
            .field("sending", &self.sending)
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

impl<T, R, A, D, S> Driver<T, R, A, D, S>
where
    T: Transport,
    R: QueueMemory + DevicePages,
    A: RequestArea,
    D: DevicePages,
    S: AsRef<[Slot]> + AsMut<[Slot]>,
{
    /// Bring the device up: reset, `ACKNOWLEDGE`, `DRIVER`, features,
    /// `FEATURES_OK` confirmed, the configuration read, both queues built and
    /// enabled, `DRIVER_OK`, and then the receive queue filled.
    ///
    /// The order is the protocol's, and the last step is this device's own:
    /// virtio 1.2 §5.1.6.3 has a driver post receive buffers before it expects
    /// a frame, and a receive queue left empty drops what arrives without a
    /// word.
    ///
    /// # Errors
    ///
    /// On any failure the device is marked `FAILED` and reset, and the parts
    /// come back in an [`InitFailure`].
    pub fn init(
        parts: Parts<T, R, A, D, S>,
        options: Options,
    ) -> Result<Self, InitFailure<T, R, A, D, S>> {
        let Parts {
            mut transport,
            receive_rings,
            transmit_rings,
            area,
            receive_data,
            transmit_data,
            mut slots,
        } = parts;
        slots.as_mut().fill(Slot::EMPTY);

        let memory = Memory {
            receive_rings,
            transmit_rings,
            area,
            receive_data,
            transmit_data,
            slots,
        };
        let plan = match Self::plan(&mut transport, &memory, &options) {
            Ok(plan) => plan,
            Err(error) => return Err(Self::give_up_early(transport, memory, error, &options)),
        };
        let driver = Self::start(transport, memory, &plan, &options)?;
        Self::fill(driver)
    }

    /// Negotiate and size the queues against the memory in hand.
    fn plan(
        transport: &mut T,
        memory: &Memory<R, A, D, S>,
        options: &Options,
    ) -> Result<Plan, InitError> {
        let regions = Regions {
            receive_rings: memory.receive_rings.device_pages(),
            transmit_rings: memory.transmit_rings.device_pages(),
            area: memory.area.device_pages(),
            receive_data: memory.receive_data.device_pages(),
        };
        negotiate(transport, &regions, memory.slots.as_ref().len(), options)
    }

    /// Fail before either queue was built.
    fn give_up_early(
        mut transport: T,
        memory: Memory<R, A, D, S>,
        error: InitError,
        options: &Options,
    ) -> InitFailure<T, R, A, D, S> {
        set_failed(&mut transport);
        let teardown = teardown(
            transport,
            Rings::Unused(memory.receive_rings),
            Rings::Unused(memory.transmit_rings),
            (
                memory.area,
                memory.receive_data,
                memory.transmit_data,
                memory.slots,
            ),
            options.reset_polls,
        );
        InitFailure { error, teardown }
    }

    /// Build both queues, activate them and say the driver is ready.
    fn start(
        mut transport: T,
        memory: Memory<R, A, D, S>,
        plan: &Plan,
        options: &Options,
    ) -> Result<Self, InitFailure<T, R, A, D, S>> {
        let Memory {
            receive_rings,
            transmit_rings,
            area,
            receive_data,
            transmit_data,
            slots,
        } = memory;
        // Both queues exist in memory before the device is told where either
        // is: `SplitQueue::new` zeroes the rings, and a device already looking
        // at one would see it wiped underneath it.
        let receive = SplitQueue::new(plan.sizing.layout, receive_rings);
        let transmit = SplitQueue::new(plan.sizing.layout, transmit_rings);

        let size = plan.sizing.layout.queue_size;
        let started = Self::activate(&mut transport, RECEIVE_QUEUE, size, plan.sizing.receive)
            .and_then(|receive| {
                let transmit =
                    Self::activate(&mut transport, TRANSMIT_QUEUE, size, plan.sizing.transmit)?;
                pci::driver_ok(&mut transport).map_err(InitError::Transport)?;
                Ok((receive, transmit))
            });
        let (receive_active, transmit_active) = match started {
            Ok(both) => both,
            Err(error) => {
                set_failed(&mut transport);
                let teardown = teardown(
                    transport,
                    Rings::Queue(receive),
                    Rings::Queue(transmit),
                    (area, receive_data, transmit_data, slots),
                    options.reset_polls,
                );
                return Err(InitFailure { error, teardown });
            }
        };

        Ok(Driver {
            transport,
            receive: ManuallyDrop::new(receive),
            transmit: ManuallyDrop::new(transmit),
            area: ManuallyDrop::new(area),
            receive_data: ManuallyDrop::new(receive_data),
            transmit_data: ManuallyDrop::new(transmit_data),
            slots,
            info: Info {
                features: plan.features,
                config: plan.config,
                limits: plan.limits,
                mac: plan.config.mac.unwrap_or([0; net::MAC_LEN]),
                link_up: plan.config.link_up(),
                queue_size: size,
                receive_buffers: size / 2,
                receive_stride: receive_stride(&plan.limits),
                receive_vector: receive_active.vector,
                transmit_vector: transmit_active.vector,
            },
            receive_notify_off: receive_active.notify_off,
            transmit_notify_off: transmit_active.notify_off,
            reset_polls: options.reset_polls,
            config_attempts: options.config_attempts,
            fault: None,
            sending: 0,
            held: 0,
        })
    }

    /// Describe one queue to the device and check it kept the vector asked
    /// for.
    fn activate(
        transport: &mut T,
        queue: u16,
        size: u16,
        addresses: QueueAddresses,
    ) -> Result<pci::ActiveQueue, InitError> {
        let asked = transport.queue_vector(queue);
        let active = pci::activate_queue(transport, queue, size, addresses, asked)
            .map_err(InitError::Transport)?;
        if active.vector != asked {
            return Err(InitError::VectorRefused {
                queue,
                asked,
                kept: active.vector,
            });
        }
        Ok(active)
    }

    /// Post every receive buffer, or give the whole driver back.
    fn fill(mut driver: Self) -> Result<Self, InitFailure<T, R, A, D, S>> {
        match driver.refill() {
            Ok(_) => Ok(driver),
            Err(error) => Err(InitFailure {
                error: InitError::Device(error),
                teardown: driver.shutdown(),
            }),
        }
    }

    /// What was agreed with the device.
    #[must_use]
    pub const fn info(&self) -> &Info {
        &self.info
    }

    /// The error the device broke the protocol with, if it has.
    #[must_use]
    pub const fn fault(&self) -> Option<DeviceError> {
        self.fault
    }

    /// Frames accepted and not yet answered.
    #[must_use]
    pub const fn frames_in_flight(&self) -> u16 {
        self.sending
    }

    /// Receive buffers the caller has been handed and not given back.
    #[must_use]
    pub const fn buffers_held(&self) -> u16 {
        self.held
    }

    /// Receive buffers sitting in the ring, waiting for a frame.
    #[must_use]
    pub fn buffers_posted(&self) -> u16 {
        let size = usize::from(self.info.queue_size);
        let posted = self
            .slots
            .as_ref()
            .iter()
            .take(size)
            .filter(|slot| slot.chain.is_some())
            .count();
        u16::try_from(posted).unwrap_or(u16::MAX)
    }

    /// The device's registers.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// Send a frame the caller has already written into the transmit data
    /// region.
    ///
    /// The frame is one chain — a header the driver writes, then one
    /// descriptor per run of device-contiguous pages — and the device is
    /// notified unless it asked not to be.
    ///
    /// # Errors
    ///
    /// [`SubmitError`], none of which leaves the id tracked.
    pub fn submit(&mut self, frame: &Frame) -> Result<(), SubmitError> {
        if self.fault.is_some() {
            return Err(SubmitError::Broken);
        }
        if frame.len == 0 {
            return Err(SubmitError::Empty);
        }
        if frame.len > self.info.limits.frame_capacity {
            return Err(SubmitError::TooLong);
        }
        let segments = net::plan(
            &Data {
                pages: self.transmit_data.device_pages(),
                offset: frame.offset,
                len: frame.len,
            },
            false,
        )
        .map_err(|error| match error {
            NetError::OutsideRegion => SubmitError::OutsideData,
            NetError::TooManySegments => SubmitError::TooManySegments,
            NetError::EmptyFrame => SubmitError::Empty,
            _ => SubmitError::BadAddress,
        })?;
        if usize::from(self.transmit.free_descriptors()) < segments.count() + 1 {
            return Err(SubmitError::QueueFull);
        }
        let index = self
            .take_header(TRANSMIT_QUEUE)
            .ok_or(SubmitError::QueueFull)?;

        match self.publish(TRANSMIT_QUEUE, index, &segments, Some(frame.id)) {
            Ok(()) => {
                self.sending += 1;
                if self.transmit.device_wants_notification() {
                    self.transport
                        .notify(TRANSMIT_QUEUE, self.transmit_notify_off);
                }
                Ok(())
            }
            Err(DeviceError::Queue(QueueError::OutOfDescriptors)) => {
                self.release_header(TRANSMIT_QUEUE, index);
                Err(SubmitError::QueueFull)
            }
            Err(error) => {
                self.release_header(TRANSMIT_QUEUE, index);
                self.break_down(error);
                Err(SubmitError::Device(error))
            }
        }
    }

    /// Post every receive buffer that is free, and notify the device if any
    /// went in.
    ///
    /// Called at bring-up and at the end of every [`Driver::on_interrupt`], so
    /// a caller that releases its buffers promptly never has to think about
    /// this; a caller that wants the ring topped up sooner may call it.
    ///
    /// # Errors
    ///
    /// [`DeviceError`], after which the driver is failed. A queue that has no
    /// room left for another chain is not an error: this posts what fits and
    /// says how many that was.
    pub fn refill(&mut self) -> Result<u16, DeviceError> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        let mut posted = 0;
        for index in 0..self.info.receive_buffers {
            if self.slot(RECEIVE_QUEUE, index).is_none_or(|slot| slot.busy) {
                continue;
            }
            let segments = match self.receive_segments(index) {
                Ok(segments) => segments,
                Err(error) => {
                    self.break_down(error);
                    return Err(error);
                }
            };
            if usize::from(self.receive.free_descriptors()) < segments.count() + 1 {
                break;
            }
            if let Some(slot) = self.slot_mut(RECEIVE_QUEUE, index) {
                slot.busy = true;
            }
            match self.publish(RECEIVE_QUEUE, index, &segments, None) {
                Ok(()) => posted += 1,
                Err(DeviceError::Queue(QueueError::OutOfDescriptors)) => {
                    self.release_header(RECEIVE_QUEUE, index);
                    break;
                }
                Err(error) => {
                    self.release_header(RECEIVE_QUEUE, index);
                    self.break_down(error);
                    return Err(error);
                }
            }
        }
        if posted > 0 && self.receive.device_wants_notification() {
            self.transport
                .notify(RECEIVE_QUEUE, self.receive_notify_off);
        }
        Ok(posted)
    }

    /// Give a receive buffer back, once its frame has been read.
    ///
    /// Until this is called the bytes stay where [`Event::Received`] said they
    /// were, and the buffer is not in the ring. A caller that never releases
    /// stops receiving, which is the failure mode worth having: the
    /// alternative is a device writing a new frame over one being read.
    ///
    /// # Errors
    ///
    /// [`ReleaseError`] for a buffer that does not exist or was not held.
    pub fn release(&mut self, buffer: u16) -> Result<(), ReleaseError> {
        if buffer >= self.info.receive_buffers {
            return Err(ReleaseError::OutOfRange(buffer));
        }
        if !self
            .slot(RECEIVE_QUEUE, buffer)
            .is_some_and(|slot| slot.held)
        {
            return Err(ReleaseError::NotHeld(buffer));
        }
        if let Some(slot) = self.slot_mut(RECEIVE_QUEUE, buffer) {
            slot.held = false;
        }
        self.release_header(RECEIVE_QUEUE, buffer);
        self.held = self.held.saturating_sub(1);
        Ok(())
    }

    /// Take what the device has done, after an interrupt.
    ///
    /// The interrupt is acknowledged first, so a completion that lands while
    /// this runs raises another rather than being lost behind the
    /// acknowledgement. Both queues are drained — a line interrupt does not
    /// say which, and draining the other is free — and the receive queue is
    /// refilled at the end.
    ///
    /// # Errors
    ///
    /// A [`DeviceError`] once the device breaks the protocol. Events taken
    /// before the fault in the same call are returned first, and the fault on
    /// the next call, so none is lost.
    pub fn on_interrupt(&mut self, out: &mut [Event]) -> Result<Drained, DeviceError> {
        self.on_interrupt_receiving(out, usize::MAX)
    }

    /// [`Driver::on_interrupt`], taking at most `receive_room` arrived
    /// frames: the rest stay in the receive queue's used ring, in order,
    /// for a later call, and the device holds further frames back rather
    /// than the caller dropping them for want of somewhere to put them.
    ///
    /// # Errors
    ///
    /// As [`Driver::on_interrupt`].
    pub fn on_interrupt_receiving(
        &mut self,
        out: &mut [Event],
        receive_room: usize,
    ) -> Result<Drained, DeviceError> {
        let isr = self.transport.acknowledge_interrupt();
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        if self.transport.read8(DEVICE_STATUS) & STATUS_DEVICE_NEEDS_RESET != 0 {
            self.break_down(DeviceError::NeedsReset);
            return Err(DeviceError::NeedsReset);
        }

        let room = receive_room.min(out.len());
        let (received, receive_fault) =
            self.drain(RECEIVE_QUEUE, out.get_mut(..room).unwrap_or_default(), 0);
        let (sent, transmit_fault) = match receive_fault {
            Some(_) => (0, None),
            None => self.drain(TRANSMIT_QUEUE, out, received),
        };
        let events = received + sent;
        if let Some(error) = receive_fault.or(transmit_fault) {
            self.break_down(error);
            if events == 0 {
                return Err(error);
            }
        }
        // A refill that fails has already recorded the fault, which the next
        // call returns; returning it here would drop the events just taken.
        let refilled = self.refill().unwrap_or(0);
        Ok(Drained {
            events,
            config_changed: isr & ISR_CONFIG != 0,
            more: self.fault.is_none() && (self.receive.has_used() || self.transmit.has_used()),
            refilled,
        })
    }

    /// Read the configuration again, after [`Drained::config_changed`], and
    /// take the link state.
    ///
    /// # Errors
    ///
    /// [`DeviceError::ConfigUnstable`] or [`DeviceError::Protocol`]; neither
    /// fails the driver, which keeps what it had.
    pub fn refresh_config(&mut self) -> Result<bool, DeviceError> {
        let config = read_config(&self.transport, self.info.features, self.config_attempts)
            .map_err(|error| match error {
                InitError::Config(error) => DeviceError::Protocol(error),
                _ => DeviceError::ConfigUnstable,
            })?;
        self.info.config = config;
        self.info.link_up = config.link_up();
        Ok(self.info.link_up)
    }

    /// Reset the device and hand everything back — the memory only if the
    /// reset finished. Frames still in flight are [`Released::abandoned`].
    pub fn shutdown(self) -> Teardown<T, R, A, D, S> {
        let Driver {
            transport,
            receive,
            transmit,
            area,
            receive_data,
            transmit_data,
            slots,
            reset_polls,
            ..
        } = self;
        teardown(
            transport,
            Rings::Queue(ManuallyDrop::into_inner(receive)),
            Rings::Queue(ManuallyDrop::into_inner(transmit)),
            (
                ManuallyDrop::into_inner(area),
                ManuallyDrop::into_inner(receive_data),
                ManuallyDrop::into_inner(transmit_data),
                slots,
            ),
            reset_polls,
        )
    }

    /// Take completions from `queue` into `out` from `at`: how many were
    /// written, and the fault that stopped the drain.
    fn drain(&mut self, queue: u16, out: &mut [Event], at: usize) -> (usize, Option<DeviceError>) {
        let mut written = 0;
        // Each completion frees a chain the driver published, so there are
        // never more to take than the queue holds.
        for _ in 0..=self.info.queue_size {
            if out.get(at + written).is_none() {
                break;
            }
            let used = match self.take_used(queue) {
                Ok(Some(used)) => used,
                Ok(None) => break,
                Err(error) => return (written, Some(DeviceError::Queue(error))),
            };
            match self.finish(queue, used) {
                Ok(event) => {
                    if let Some(slot) = out.get_mut(at + written) {
                        *slot = event;
                        written += 1;
                    }
                }
                Err(error) => return (written, Some(error)),
            }
        }
        (written, None)
    }

    /// The next completion on `queue`.
    fn take_used(&mut self, queue: u16) -> Result<Option<ferrix_virtio::Completion>, QueueError> {
        if queue == RECEIVE_QUEUE {
            self.receive.take_used()
        } else {
            self.transmit.take_used()
        }
    }

    /// Account for one completed chain.
    fn finish(
        &mut self,
        queue: u16,
        used: ferrix_virtio::Completion,
    ) -> Result<Event, DeviceError> {
        let record = self
            .slot(queue, used.head)
            .and_then(|slot| slot.chain)
            .ok_or(DeviceError::UnknownChain {
                queue,
                head: used.head,
            })?;
        // The record is only cleared once the completion has been believed, so
        // a frame whose completion is refused is still in the driver's books
        // and is still reported by `Released::abandoned`.
        let event = if queue == RECEIVE_QUEUE {
            self.received(&record, used.written)?
        } else {
            net::parse_sent(&record.chain, used.written).map_err(DeviceError::Protocol)?;
            self.release_header(TRANSMIT_QUEUE, record.index);
            self.sending = self.sending.saturating_sub(1);
            Event::Sent {
                id: record.id.unwrap_or_default(),
            }
        };
        self.clear_chain(queue, used.head);
        Ok(event)
    }

    /// Read the header the device wrote and turn a receive completion into an
    /// event. The buffer stays busy: the caller holds it until
    /// [`Driver::release`].
    fn received(&mut self, record: &ChainRecord, written: u32) -> Result<Event, DeviceError> {
        let mut bytes = [0_u8; net::HEADER_LEN as usize];
        let base = self.header_offset(RECEIVE_QUEUE, record.index);
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = self.area.read_u8(base + index);
        }
        let receipt = net::parse_receipt(&record.chain, written, &bytes, self.info.features)
            .map_err(DeviceError::Protocol)?;
        if let Some(slot) = self.slot_mut(RECEIVE_QUEUE, record.index) {
            slot.held = true;
        }
        self.held += 1;
        Ok(Event::Received {
            buffer: record.index,
            offset: self.buffer_offset(record.index),
            len: receipt.len,
        })
    }

    /// The descriptors of receive buffer `index`.
    fn receive_segments(&self, index: u16) -> Result<Segments, DeviceError> {
        net::plan(
            &Data {
                pages: self.receive_data.device_pages(),
                offset: self.buffer_offset(index),
                len: self.info.limits.frame_capacity,
            },
            true,
        )
        .map_err(DeviceError::Protocol)
    }

    /// Write the header for header slot `index` of `queue` and publish its
    /// chain.
    fn publish(
        &mut self,
        queue: u16,
        index: u16,
        segments: &Segments,
        id: Option<u64>,
    ) -> Result<(), DeviceError> {
        let address = self
            .header_address(queue, index)
            .ok_or(DeviceError::Bookkeeping)?;
        let receive = queue == RECEIVE_QUEUE;
        let base = self.header_offset(queue, index);
        let bytes = Header::plain().encode();
        for (at, byte) in bytes.iter().enumerate().take(self.header_len()) {
            self.area.write_u8(base + at, *byte);
        }

        let header_len = self.info.limits.header_len;
        let queue_memory = if receive {
            &mut *self.receive
        } else {
            &mut *self.transmit
        };
        let chain = net::publish(queue_memory, address, header_len, segments, receive).map_err(
            |error| match error {
                NetError::Queue(error) => DeviceError::Queue(error),
                error => DeviceError::Protocol(error),
            },
        )?;
        let slot = self
            .slot_mut(queue, chain.head)
            .filter(|slot| slot.chain.is_none())
            .ok_or(DeviceError::Bookkeeping)?;
        slot.chain = Some(ChainRecord { index, id, chain });
        Ok(())
    }

    /// Bytes of virtio header on the wire, as a `usize`.
    fn header_len(&self) -> usize {
        usize::try_from(self.info.limits.header_len).unwrap_or(net::HEADER_LEN as usize)
    }

    /// Where a buffer's frame bytes start in the receive data region.
    fn buffer_offset(&self, buffer: u16) -> u64 {
        u64::from(buffer) * u64::from(self.info.receive_stride)
    }

    /// Where a queue's bookkeeping bank starts.
    const fn bank(&self, queue: u16) -> usize {
        if queue == RECEIVE_QUEUE {
            0
        } else {
            self.info.queue_size as usize
        }
    }

    /// Slot `index` of `queue`'s bank.
    fn slot(&self, queue: u16, index: u16) -> Option<&Slot> {
        self.slots
            .as_ref()
            .get(self.bank(queue) + usize::from(index))
    }

    /// Slot `index` of `queue`'s bank, to be changed.
    fn slot_mut(&mut self, queue: u16, index: u16) -> Option<&mut Slot> {
        let at = self.bank(queue) + usize::from(index);
        self.slots.as_mut().get_mut(at)
    }

    /// Forget the chain at head `head` of `queue`.
    fn clear_chain(&mut self, queue: u16, head: u16) {
        if let Some(slot) = self.slot_mut(queue, head) {
            slot.chain = None;
        }
    }

    /// Take a free header slot of `queue`, or `None` when every one is in use.
    ///
    /// On the receive queue the header slots are also the frame buffers, so
    /// only [`Info::receive_buffers`] of them are ever taken.
    fn take_header(&mut self, queue: u16) -> Option<u16> {
        let most = if queue == RECEIVE_QUEUE {
            usize::from(self.info.receive_buffers)
        } else {
            usize::from(self.info.queue_size)
        };
        let bank = self.bank(queue);
        let slots = self.slots.as_mut().get_mut(bank..)?;
        let found = slots
            .iter_mut()
            .take(most)
            .enumerate()
            .find(|(_, slot)| !slot.busy)?;
        found.1.busy = true;
        u16::try_from(found.0).ok()
    }

    /// Give a header slot of `queue` back. Receive slots are given back by
    /// [`Driver::release`], transmit slots by a completion.
    fn release_header(&mut self, queue: u16, index: u16) {
        if let Some(slot) = self.slot_mut(queue, index) {
            slot.busy = false;
        }
    }

    /// Where header slot `index` of `queue` is in the area.
    fn header_offset(&self, queue: u16, index: u16) -> usize {
        let slot = self.bank(queue) + usize::from(index);
        slot * AREA_PER_HEADER as usize
    }

    /// The device address of header slot `index` of `queue`.
    ///
    /// A header starts at a multiple of [`AREA_PER_HEADER`], which divides a
    /// page, so it never straddles one.
    fn header_address(&self, queue: u16, index: u16) -> Option<u64> {
        let offset = u64::try_from(self.header_offset(queue, index)).ok()?;
        let page = usize::try_from(offset / PAGE_SIZE).ok()?;
        self.area
            .device_pages()
            .get(page)?
            .checked_add(offset % PAGE_SIZE)
    }

    /// Stop trusting the device: record why, and tell it `FAILED`.
    fn break_down(&mut self, error: DeviceError) {
        if self.fault.is_none() {
            self.fault = Some(error);
        }
        set_failed(&mut self.transport);
    }
}
