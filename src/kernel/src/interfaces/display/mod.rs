//! The display core: the kernel's end of a display driver's control channel,
//! and the cards it publishes.
//!
//! `docs/DISPLAY.md` §2 is the design and `ferrix-displayctl` checks every
//! word of the protocol. A process holding a device with `MANAGE` asks for a
//! control channel with `DISPLAY_CONTROL_CREATE` and hands its end to a
//! display driver. One kernel task per card then:
//!
//! 1. waits for HELLO and checks it through [`Session::accept`], the device's
//!    location, and the firmware framebuffer's rule (§2.4), answering REFUSED
//!    with the first failure;
//! 2. makes the card VMO, whose ranges are the dumb buffers, publishes the
//!    card, and answers READY with the VMO (`READ | TRANSFER`) and a port;
//! 3. serves: every message the driver sends goes through the session, which
//!    accepts only the replies it is waiting for; the replies it accepts are
//!    queued as [`Event`]s for whoever made the request to collect;
//! 4. ends when the driver closes its end, says STOPPED, or breaks the
//!    protocol, taking the card away.
//!
//! # The kernel never draws
//!
//! The card VMO's pages are filled by the program that maps them. The core
//! only says which range is which buffer, which is shown, and what changed.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_blkring::identity::Location;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_displayctl::message::{
    Attach, AttachObject, FORMAT, Hello, MAX_BUFFER_PAGES, MAX_BYTES, MAX_SCANOUTS, Message, Ready,
    Rect, Refusal, ScanoutMode, Status, Timing, Timings,
};
use ferrix_displayctl::session::{Event, RequestError, Session};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;

use ferrix_native_abi::types::DEVICE_NOT_PCI;

use crate::claim::StillServed;
use crate::claim::{Claims, Numbers};
use crate::device::{self, DeviceNode, DmaShape};
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

pub(crate) mod drm;
pub(crate) mod edid;

/// Linux's major number for DRM devices.
pub(crate) const DRM_MAJOR: u32 = 226;

/// The most replies kept that no request is waiting for.
const MAX_EVENTS: usize = 64;

/// The most memory one card's buffers may take, which is the card VMO's size
/// and so also the most an opener of the card can make it commit.
///
/// A 4K mode is 3840 × 2160 × 4 bytes, about 33 MiB a buffer, so a
/// double-buffered 4K screen is 66 MiB; 256 MiB leaves room for a cursor and
/// a third buffer (`docs/DISPLAY.md` §2.1).
pub(crate) const CARD_BYTES: u64 = 256 * 1024 * 1024;

// A device's default pin budget counts a driver's pins as at most the card
// (`object::pin::DEFAULT_PIN_BUDGET_PAGES`).
const _: () = assert!(
    CARD_BYTES / PAGE_SIZE <= crate::object::pin::LARGEST_DRIVER_PIN_PAGES as u64,
    "the card is larger than a device's default pin budget allows a driver to pin"
);

/// How long the core waits for its driver's HELLO.
const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;

/// How long the task sleeps before looking at the channel of its own accord.
const RECHECK_NANOS: u64 = 50_000_000;

/// How long a request waits for the driver's reply.
const REPLY_PATIENCE_NANOS: u64 = 5_000_000_000;

/// Whether the firmware framebuffer is in memory the frame allocator owns,
/// as the loader said in `BootInfo.framebuffer.reclaimable`.
static RECLAIMABLE_FRAMEBUFFER: AtomicBool = AtomicBool::new(false);

/// Record what the loader said about the firmware framebuffer, once at boot.
pub(crate) fn note_boot_framebuffer(present: bool, reclaimable: bool) {
    RECLAIMABLE_FRAMEBUFFER.store(present && reclaimable, Ordering::Relaxed);
}

/// Why a control channel could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CreateError {
    /// The device already has one.
    InUse,
    /// No memory for the channel, or no stack for the task.
    NoMemory,
}

/// Why a request to a card did not get its answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CardError {
    /// The request is not one the protocol has a state for now.
    Request(RequestError),
    /// The card has no range, or no buffer id, left for the buffer.
    NoRoom,
    /// The driver's channel is full: it is behind, and the request can be
    /// made again once it has read.
    Busy,
    /// The driver is gone or broke the protocol.
    Gone,
    /// The driver did not answer in time.
    TimedOut,
}

/// A control channel waiting for its task.
#[derive(Debug)]
struct Start {
    id: usize,
    control: Arc<Endpoint>,
    device: Arc<DeviceNode>,
    location: Option<Location>,
}

/// The word a device tree node's HELLO and PUBLISHED carry for its location,
/// which is what `device_info` says for it: there is one display of the
/// kind on a board, so the word names it well enough.
const TREE_LOCATION: Location = Location(DEVICE_NOT_PCI);

static STARTING: SpinLock<Vec<Start>> = SpinLock::new(Vec::new());
static CLAIMS: Claims = Claims::new();
static CARDS: SpinLock<Vec<Arc<Card>>> = SpinLock::new(Vec::new());
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
/// `card<N>`: a driver started again after its card went publishes as the
/// number the card had, so `/dev/dri/card0` is found again (`crate::claim`).
static NUMBERS: Numbers = Numbers::new(0);

/// A published card.
pub(crate) struct Card {
    /// `card<index>`.
    pub(crate) index: u32,
    /// The pages the dumb buffers are ranges of.
    pub(crate) vmo: Arc<Vmo>,
    /// Each scanout's preferred mode: HELLO's, then each MODES's.
    modes: SpinLock<[ScanoutMode; MAX_SCANOUTS]>,
    /// How many times MODES changed `modes`, which an open card reads to
    /// tell its program the connectors changed.
    pub(crate) modes_changed: AtomicU64,
    scanouts: usize,
    control: Arc<Endpoint>,
    state: SpinLock<State>,
    changed: Arc<WaitQueue>,
    /// Whether an open holds the card: one at a time.
    pub(crate) opened: AtomicBool,
    /// How the device reads the buffers: one run each, and whether the
    /// caches have to be cleaned for it.
    dma: DmaShape,
    /// Whether the card is a board's HDMI output, which runs the modes it
    /// can make a pixel clock for, rather than a virtual one that shows any
    /// size it is handed.
    pub(crate) hdmi: bool,
    /// The device node the card is served from, by its index in
    /// `device::devices()`: where sysfs shows it.
    pub(crate) node: usize,
    /// The modes scanout 0 runs, the running one first, for a card that
    /// runs only some (HELLO's timings); empty for one that shows any size.
    timings: Timings,
    /// Each head's EDID, where `drm.edid_firmware` named one that could be
    /// read ([`edid::load`]); `None` for a head without.
    edids: Vec<Option<Vec<u8>>>,
}

/// The range of the card VMO a buffer id was given.
#[derive(Clone, Copy, Debug)]
struct Range {
    buffer: u32,
    offset: u64,
    bytes: u64,
    /// Bytes a row, for cleaning the rows a flush names.
    stride: u32,
}

/// A reply whose request stopped waiting for it, which the card's task drops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unwanted {
    Flipped(u64),
    Detached(u32),
}

struct State {
    session: Session,
    /// Accepted replies, for the requests waiting on them.
    events: Vec<Event>,
    gone: bool,
    /// The next buffer id. Ids are never reused on a card, so no reply can
    /// be taken for a later buffer's, and the driver never sees an id again
    /// that the device may still hold pages under.
    next_buffer: u32,
    /// Where never-used space in the card VMO starts.
    next_offset: u64,
    /// Ranges given back, decommitted, sorted by offset, neighbours merged.
    free: Vec<(u64, u64)>,
    /// The range of every buffer id the session may still track.
    ranges: Vec<Range>,
    /// Buffers nobody holds any more that are not detached yet: attaches
    /// that timed out, and buffers busy when they were let go. Each is
    /// detached as soon as the session allows it.
    orphans: Vec<u32>,
    unwanted: Vec<Unwanted>,
    /// A closed open's scanout-off that found the channel full, sent as soon
    /// as there is room, unless a later SCANOUT supersedes it.
    scanout_off: bool,
    /// Each scanout's cursor place that found the channel full: only the
    /// newest, sent as soon as there is room. A pointer's places in between
    /// are not worth a wait, and a MOVE is the one request that must never
    /// make its caller wait.
    moves: [Option<(i32, i32)>; MAX_SCANOUTS],
}

impl State {
    /// A page-aligned range of `bytes`, the first that fits.
    fn allocate(&mut self, bytes: u64) -> Option<u64> {
        if let Some(at) = self.free.iter().position(|&(_, len)| len >= bytes) {
            let (offset, len) = self.free.remove(at);
            if len > bytes {
                self.free.insert(at, (offset + bytes, len - bytes));
            }
            return Some(offset);
        }
        let end = self
            .next_offset
            .checked_add(bytes)
            .filter(|&end| end <= CARD_BYTES)?;
        let offset = self.next_offset;
        self.next_offset = end;
        Some(offset)
    }

    /// Give a decommitted range back.
    fn give_back(&mut self, offset: u64, bytes: u64) {
        let at = self.free.partition_point(|&(start, _)| start < offset);
        self.free.insert(at, (offset, bytes));
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.free.len());
        for (start, len) in self.free.drain(..) {
            match merged.last_mut() {
                Some(last) if last.0 + last.1 == start => last.1 += len,
                _ => merged.push((start, len)),
            }
        }
        if let Some(&(start, len)) = merged.last()
            && start + len == self.next_offset
        {
            self.next_offset = start;
            let _ = merged.pop();
        }
        self.free = merged;
    }

    fn take_range(&mut self, buffer: u32) -> Option<Range> {
        let at = self
            .ranges
            .iter()
            .position(|range| range.buffer == buffer)?;
        Some(self.ranges.remove(at))
    }

    /// Detach `buffer`, which nobody holds, now if the session allows it
    /// and as soon as it does otherwise.
    fn let_go(&mut self, control: &Endpoint, buffer: u32) {
        if !control.peer_has_room() {
            if !self.orphans.contains(&buffer) {
                self.orphans.push(buffer);
            }
            return;
        }
        match self.session.detach(buffer) {
            Ok(message) => {
                self.unwanted.push(Unwanted::Detached(buffer));
                let _ = write(control, self, &message);
            }
            Err(RequestError::Busy) => self.orphans.push(buffer),
            Err(_) if self.session.is_attaching(buffer) => self.orphans.push(buffer),
            // Never attached, or already lost: nothing to detach.
            Err(_) => {}
        }
    }

    /// Settle an accepted reply: account for its range, drop it if nobody
    /// waits for it, and detach whatever orphans it frees. Answers a range
    /// to decommit and give back.
    fn settle(&mut self, control: &Endpoint, event: Event) -> Option<Range> {
        let mut freed = None;
        let wanted = match event {
            Event::Attached { buffer, status } => {
                if status != Status::Ok {
                    // The driver unpins what it pinned before it says so.
                    freed = self.take_range(buffer);
                }
                !self.orphans.contains(&buffer)
            }
            Event::Flipped { sequence, .. } => !self.forget(Unwanted::Flipped(sequence)),
            Event::Detached { buffer, status } => {
                let range = self.take_range(buffer);
                // A refused detach leaves the pages the device's: its range
                // is never handed out again (`docs/DISPLAY.md` §2.2).
                if status == Status::Ok {
                    freed = range;
                }
                !self.forget(Unwanted::Detached(buffer))
            }
            // Nobody waits for either: [`serve`] ends at STOPPED and takes
            // MODES's modes itself.
            Event::Stopped | Event::Modes => false,
        };
        if wanted {
            if self.events.len() >= MAX_EVENTS {
                let _ = self.events.remove(0);
            }
            self.events.push(event);
        }
        self.retry(control);
        freed
    }

    /// Send what waited for the session or for room in the channel: a
    /// closed open's scanout-off, the cursors' newest places, then the
    /// orphans' detaches.
    fn retry(&mut self, control: &Endpoint) {
        if self.scanout_off
            && control.peer_has_room()
            && let Ok(message) = self.session.scanout(0, 0, Rect::default())
        {
            self.scanout_off = false;
            if write(control, self, &message).is_err() {
                return;
            }
        }
        if self.send_moves(control).is_err() {
            return;
        }
        for buffer in core::mem::take(&mut self.orphans) {
            self.let_go(control, buffer);
        }
    }

    /// Send every scanout's waiting cursor place the channel has room for.
    fn send_moves(&mut self, control: &Endpoint) -> Result<(), CardError> {
        for scanout in 0..MAX_SCANOUTS {
            if !control.peer_has_room() {
                break;
            }
            let Some(at) = self.moves.get_mut(scanout).and_then(Option::take) else {
                continue;
            };
            // The session refuses only a scanout the driver has not got and
            // a conversation that is over; either way the place is dropped.
            if let Ok(message) = self.session.move_cursor(scanout as u32, at) {
                write(control, self, &message)?;
            }
        }
        Ok(())
    }

    fn forget(&mut self, reply: Unwanted) -> bool {
        let before = self.unwanted.len();
        self.unwanted.retain(|&held| held != reply);
        self.unwanted.len() != before
    }
}

/// Send `message` to the driver. Called with the card's state locked, so
/// messages reach the driver in the order the session made them, and only
/// after [`Endpoint::peer_has_room`] said yes: the kernel is the channel's
/// only writer, so the write fails only when the driver is gone, and the
/// card goes with it.
fn write(control: &Endpoint, state: &mut State, message: &Message) -> Result<(), CardError> {
    let bytes = message.encode().as_bytes().to_vec();
    if control
        .write(bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()))
        .is_err()
    {
        state.gone = true;
        return Err(CardError::Gone);
    }
    Ok(())
}

impl core::fmt::Debug for Card {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Card")
            .field("index", &self.index)
            .field("scanouts", &self.scanouts)
            .finish_non_exhaustive()
    }
}

impl Card {
    /// Each scanout's preferred mode, as the driver last reported it.
    pub(crate) fn modes(&self) -> Vec<ScanoutMode> {
        let modes = *self.modes.lock();
        modes.get(..self.scanouts).unwrap_or(&[]).to_vec()
    }

    /// Take the modes a MODES gave the session, and say so on the console.
    fn modes_from(&self, modes: [ScanoutMode; MAX_SCANOUTS]) {
        let mut held = self.modes.lock();
        if *held == modes {
            return;
        }
        *held = modes;
        drop(held);
        let _ = self.modes_changed.fetch_add(1, Ordering::AcqRel);
        for (scanout, mode) in modes.iter().enumerate().take(self.scanouts) {
            crate::console::println!(
                "  display  card{} scanout {scanout} is now {}x{}{}",
                self.index,
                mode.width,
                mode.height,
                shown_at(mode)
            );
        }
    }

    /// The modes scanout 0 runs, the one running at HELLO first; empty for
    /// a card that shows any size.
    pub(crate) fn timings(&self) -> &[Timing] {
        self.timings.as_slice()
    }

    /// Whether `head`'s connector was given an EDID, connected or not: the
    /// connectors that have the `EDID` property.
    pub(crate) fn carries_edid(&self, head: usize) -> bool {
        self.edids.get(head).is_some_and(Option::is_some)
    }

    /// `head`'s EDID while a display is there, as Linux's property holds the
    /// blob only while the connector is connected, and none otherwise.
    pub(crate) fn edid(&self, head: usize) -> Option<&[u8]> {
        let connected = self
            .modes
            .lock()
            .get(head)
            .is_some_and(|mode| mode.enabled && head < self.scanouts);
        if !connected {
            return None;
        }
        self.edids.get(head)?.as_deref()
    }

    /// Whether the driver is gone.
    pub(crate) fn is_gone(&self) -> bool {
        self.state.lock().gone
    }

    /// What `stat` says of the card: a character device whose size is the
    /// card VMO's, so a mapping's bounds check holds (§2.1).
    pub(crate) fn metadata(&self) -> ferrix_vfs::Metadata {
        use ferrix_vfs::{FileType, Metadata, Timespec};
        Metadata {
            ino: (1u64 << 40) + u64::from(self.index),
            kind: FileType::CharDevice,
            // Owner and group only, as Linux's `video` group has it: whoever
            // opens the card can map every range of it.
            permissions: 0o660,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: CARD_BYTES,
            rdev: ferrix_vfs::initramfs::makedev(DRM_MAJOR, self.index),
            blocks: 0,
            block_size: 4096,
            atime: Timespec::default(),
            mtime: Timespec::default(),
            ctime: Timespec::default(),
        }
    }

    /// Run `make` on the session and send the message it makes, with the
    /// state locked throughout.
    fn send<T>(
        &self,
        make: impl FnOnce(&mut State) -> Result<(Message, T), CardError>,
    ) -> Result<T, CardError> {
        let mut state = self.state.lock();
        if state.gone || self.control.peer_closed() {
            return Err(CardError::Gone);
        }
        // Before the session commits to the request, which it cannot take
        // back: a driver that is behind makes the caller try again, not the
        // card go (os-02's review).
        if !self.control.peer_has_room() {
            return Err(CardError::Busy);
        }
        let (message, made) = make(&mut state)?;
        write(&self.control, &mut state, &message)?;
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
    ) -> Result<Event, CardError> {
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
            return Err(CardError::Gone);
        }
        abandon(&mut state);
        Err(CardError::TimedOut)
    }

    /// Give a new buffer of `width` × `height` rows of `stride` bytes a
    /// range and an id, ATTACH it and wait for ATTACHED: the id, the range's
    /// offset and how the attach went.
    pub(crate) fn attach(
        &self,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<(u32, u64, Status), CardError> {
        let pixels = u64::from(stride) * u64::from(height);
        let bytes = pixels
            .checked_next_multiple_of(PAGE_SIZE)
            .filter(|&bytes| bytes > 0 && bytes <= MAX_BUFFER_PAGES * PAGE_SIZE)
            .ok_or(CardError::NoRoom)?;
        // A device that reads one run of addresses gets its range filled
        // with one block of frames before the driver hears of it, outside
        // the state lock: zeroing megabytes is not something to do with
        // preemption off.
        let reserved = if self.dma.contiguous {
            let offset = self.state.lock().allocate(bytes).ok_or(CardError::NoRoom)?;
            if !commit_contiguous(&self.vmo, offset, bytes) {
                self.forget_range(offset, bytes);
                return Err(CardError::NoRoom);
            }
            Some(offset)
        } else {
            None
        };
        let sent = self.send(|state| {
            let buffer = state.next_buffer;
            let next = buffer.checked_add(1).ok_or(CardError::NoRoom)?;
            let offset = match reserved {
                Some(offset) => offset,
                None => state.allocate(bytes).ok_or(CardError::NoRoom)?,
            };
            let attach = Attach {
                buffer,
                format: FORMAT,
                offset,
                length: bytes,
                width,
                height,
                stride,
            };
            match state.session.attach(attach) {
                Ok(message) => {
                    state.next_buffer = next;
                    state.ranges.push(Range {
                        buffer,
                        offset,
                        bytes,
                        stride,
                    });
                    Ok((message, (buffer, offset)))
                }
                Err(error) => {
                    // Never sent, so never pinned, and nothing wrote to it.
                    // A reserved range holds frames, and goes back below.
                    if reserved.is_none() {
                        state.give_back(offset, bytes);
                    }
                    Err(CardError::Request(error))
                }
            }
        });
        let (buffer, offset) = match (sent, reserved) {
            (Ok(sent), _) => sent,
            (Err(error), Some(offset)) => {
                // Unless a buffer was made of it after all, which only a
                // write to a driver that has just gone can leave behind.
                let taken = self.state.lock().ranges.iter().any(|r| r.offset == offset);
                if !taken {
                    self.forget_range(offset, bytes);
                }
                return Err(error);
            }
            (Err(error), None) => return Err(error),
        };
        let event = self.collect(
            |event| matches!(event, Event::Attached { buffer: b, .. } if *b == buffer),
            |state| state.orphans.push(buffer),
        )?;
        match event {
            Event::Attached { status, .. } => Ok((buffer, offset, status)),
            _ => Err(CardError::Gone),
        }
    }

    /// Give an object the render core made a buffer id, `ATTACH_OBJ` it and
    /// wait for ATTACHED: the id and how the attach went.
    ///
    /// The other way to make a buffer, and the difference is where the
    /// pixels are. An ATTACH's are a range of the card VMO, which the driver
    /// pins for the device; these are the device's own already, so there is
    /// no range to allocate, nothing to pin, and nothing to send when the
    /// frame changes -- which is the whole of why it exists. What the driver
    /// is handed is the renderer's object id, which is the device's name for
    /// the resource and no business of this core's beyond passing it on.
    ///
    /// # Errors
    ///
    /// [`CardError`], including the driver's own refusal.
    pub(crate) fn attach_object(
        &self,
        object: u32,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<(u32, Status), CardError> {
        let buffer = self.send(|state| {
            let buffer = state.next_buffer;
            let next = buffer.checked_add(1).ok_or(CardError::NoRoom)?;
            let attach = AttachObject {
                buffer,
                object,
                format: FORMAT,
                width,
                height,
                stride,
            };
            let message = state
                .session
                .attach_object(attach)
                .map_err(CardError::Request)?;
            state.next_buffer = next;
            Ok((message, buffer))
        })?;
        let event = self.collect(
            |event| matches!(event, Event::Attached { buffer: b, .. } if *b == buffer),
            |state| state.orphans.push(buffer),
        )?;
        match event {
            Event::Attached { status, .. } => Ok((buffer, status)),
            _ => Err(CardError::Gone),
        }
    }

    /// Show `rect` of `buffer` on `scanout`, or turn it off with buffer 0.
    pub(crate) fn scanout(&self, scanout: u32, buffer: u32, rect: Rect) -> Result<(), CardError> {
        // A device that does not snoop starts reading the whole buffer at
        // the next frame, before any flush of it.
        if buffer != 0 {
            self.clean_rows(buffer, 0, u32::MAX);
        }
        self.send(|state| {
            state.scanout_off = false;
            let message = state
                .session
                .scanout(scanout, buffer, rect)
                .map_err(CardError::Request)?;
            Ok((message, ()))
        })
    }

    /// FLUSH `rect` of `buffer` and wait for FLIPPED.
    pub(crate) fn flush(&self, buffer: u32, rect: Rect) -> Result<Status, CardError> {
        self.clean_rows(buffer, rect.y, rect.height);
        let sequence = self.send(|state| {
            let message = state
                .session
                .flush(buffer, rect)
                .map_err(CardError::Request)?;
            let Message::Flush { sequence, .. } = message else {
                return Err(CardError::Gone);
            };
            Ok((message, sequence))
        })?;
        let event = self.collect(
            |event| matches!(event, Event::Flipped { sequence: s, .. } if *s == sequence),
            |state| state.unwanted.push(Unwanted::Flipped(sequence)),
        )?;
        match event {
            Event::Flipped { status, .. } => Ok(status),
            _ => Err(CardError::Gone),
        }
    }

    /// Write `rows` rows of `buffer` from row `first` back from the caches
    /// to memory, for a device that reads memory rather than the caches.
    /// Nothing to do for a coherent one, or for a buffer that is not a range
    /// of the card VMO.
    ///
    /// Runs without the state lock: the range's pages are pinned by the
    /// driver for as long as the buffer is attached, and a clean of a page
    /// that is being let go of at the same moment writes back nothing that
    /// matters.
    fn clean_rows(&self, buffer: u32, first: u32, rows: u32) {
        if self.dma.coherent {
            return;
        }
        let Some(range) = self
            .state
            .lock()
            .ranges
            .iter()
            .find(|range| range.buffer == buffer)
            .copied()
        else {
            return;
        };
        let stride = u64::from(range.stride);
        let start = (u64::from(first) * stride).min(range.bytes);
        let end = u64::from(first)
            .saturating_add(u64::from(rows))
            .saturating_mul(stride)
            .min(range.bytes);
        let mut at = range.offset + start;
        let stop = range.offset + end;
        while at < stop {
            let page_end = (at / PAGE_SIZE + 1) * PAGE_SIZE;
            let len = page_end.min(stop) - at;
            if let Some(frame) = self.vmo.page(at / PAGE_SIZE) {
                let virt = crate::mm::direct_map(frame * PAGE_SIZE) + at % PAGE_SIZE;
                crate::arch::clean_for_device(virt, len);
            }
            at += len;
        }
    }

    /// Give back a range no buffer was made of, frames and all.
    fn forget_range(&self, offset: u64, bytes: u64) {
        let _ = self
            .vmo
            .decommit_range(offset / PAGE_SIZE, bytes / PAGE_SIZE);
        self.state.lock().give_back(offset, bytes);
    }

    /// CURSOR: show `buffer` -- [`ferrix_displayctl::message::CURSOR_SIZE`]
    /// square, or 0 for none -- as
    /// `scanout`'s cursor with its hotspot at `hot` and its top-left corner
    /// at `at`, and wait for FLIPPED, which says the image is on the device.
    pub(crate) fn cursor(
        &self,
        scanout: u32,
        buffer: u32,
        hot: (u32, u32),
        at: (i32, i32),
    ) -> Result<Status, CardError> {
        // The image is read by the device, as a flushed frame is.
        if buffer != 0 {
            self.clean_rows(buffer, 0, u32::MAX);
        }
        let sequence = self.send(|state| {
            let message = state
                .session
                .cursor(scanout, buffer, hot, at)
                .map_err(CardError::Request)?;
            let Message::Cursor { sequence, .. } = message else {
                return Err(CardError::Gone);
            };
            // A place still waiting for room is older than this one.
            if let Some(waiting) = state.moves.get_mut(scanout as usize) {
                *waiting = None;
            }
            Ok((message, sequence))
        })?;
        let event = self.collect(
            |event| matches!(event, Event::Flipped { sequence: s, .. } if *s == sequence),
            |state| state.unwanted.push(Unwanted::Flipped(sequence)),
        )?;
        match event {
            Event::Flipped { status, .. } => Ok(status),
            _ => Err(CardError::Gone),
        }
    }

    /// Whether the card has a cursor plane, as its driver's HELLO said.
    pub(crate) fn has_cursor(&self) -> bool {
        self.state.lock().session.has_cursor()
    }

    /// MOVE `scanout`'s cursor's top-left corner to `at`, without waiting
    /// for anything: now if the channel has room, and otherwise as soon as
    /// it does, unless a later place comes first.
    pub(crate) fn move_cursor(&self, scanout: u32, at: (i32, i32)) -> Result<(), CardError> {
        let mut state = self.state.lock();
        if state.gone || self.control.peer_closed() {
            return Err(CardError::Gone);
        }
        if scanout as usize >= self.scanouts {
            return Err(CardError::Request(RequestError::NoSuchScanout));
        }
        if let Some(waiting) = state.moves.get_mut(scanout as usize) {
            *waiting = Some(at);
        }
        state.send_moves(&self.control)
    }

    /// Let go of `buffers` without waiting: detached now, or as soon as the
    /// session allows. Their ranges come back once the device gives them up.
    pub(crate) fn let_go(&self, buffers: &[u32]) {
        let mut state = self.state.lock();
        if state.gone {
            return;
        }
        for &buffer in buffers {
            state.let_go(&self.control, buffer);
        }
    }

    /// Take the screen off and let go of `buffers`: what closing an open
    /// does.
    pub(crate) fn release(&self, buffers: &[u32]) {
        let mut state = self.state.lock();
        if state.gone {
            return;
        }
        if !self.control.peer_has_room() {
            state.scanout_off = true;
        } else if let Ok(message) = state.session.scanout(0, 0, Rect::default())
            && write(&self.control, &mut state, &message).is_err()
        {
            return;
        }
        for &buffer in buffers {
            state.let_go(&self.control, buffer);
        }
    }
}

/// Fill `bytes` of `vmo` from `offset`, a range holding nothing, with zeroed
/// frames that are one run of physical memory: one block from the allocator,
/// or for a range longer than the largest block (4 MiB; a 1920x1080 buffer is
/// 8.3 MB) that many largest blocks back to back, split so the VMO owns and
/// gives back each page as it would any other, the pages past the range
/// given back at once. Whether it was done; a range the allocator has no run
/// for, or one somebody filled meanwhile, is left as it was.
fn commit_contiguous(vmo: &Vmo, offset: u64, bytes: u64) -> bool {
    let pages = bytes / PAGE_SIZE;
    let largest = 1u64 << ferrix_frame::MAX_ORDER;
    let (block, (order, blocks)) =
        match (0..=ferrix_frame::MAX_ORDER).find(|&order| 1u64 << order >= pages) {
            Some(order) => (crate::mm::allocate_frames(order), (order, 1)),
            None => {
                let blocks = pages.div_ceil(largest);
                (
                    crate::mm::allocate_frame_run(blocks),
                    (ferrix_frame::MAX_ORDER, blocks),
                )
            }
        };
    let Some(block) = block else {
        return false;
    };
    let each = 1u64 << order;
    // Split cannot refuse a block just allocated at its own order; if it
    // somehow did, the blocks split so far go back page by page and the
    // rest whole.
    if let Some(refused) = (0..blocks).find(|&k| !crate::mm::split_frames(block + k * each, order))
    {
        for frame in block..block + refused * each {
            let _ = crate::mm::release_frame(frame);
        }
        for k in refused..blocks {
            crate::mm::deallocate_frames(block + k * each, order);
        }
        return false;
    }
    let first = offset / PAGE_SIZE;
    let mut inserted = 0;
    for page in 0..blocks * each {
        let frame = block + page;
        if page < pages {
            crate::mm::zero_frame(frame);
            if vmo.insert_absent(first + page, frame) {
                inserted += 1;
                continue;
            }
        }
        let _ = crate::mm::release_frame(frame);
    }
    if inserted == pages {
        return true;
    }
    let _ = vmo.decommit_range(first, pages);
    false
}

/// The numbers of every published card, lowest first.
pub(crate) fn card_indices() -> Vec<u32> {
    let mut indices: Vec<u32> = CARDS.lock().iter().map(|card| card.index).collect();
    indices.sort_unstable();
    indices
}

/// The card published as `card<index>`, if its driver is serving it.
pub(crate) fn card(index: u32) -> Option<Arc<Card>> {
    CARDS
        .lock()
        .iter()
        .find(|card| card.index == index)
        .map(Arc::clone)
}

/// What a quiesce waits out for the display.
static SERVER: native::Server = native::Server {
    wait_until_unserved,
    release: None,
};

/// Answer `display_control_create` with the display, and have a quiesce wait it out.
///
/// Called once from `main.rs`'s `register_load`: the native ABI is the item's
/// and names no subsystem above it, so this registers into it.
///
/// # Errors
///
/// [`Full`] when the item has no room for the registration.
pub(crate) fn install() -> Result<(), Full> {
    native::serve(NativeCall::DisplayControlCreate, control_create)?;
    native::register_server(&SERVER)
}

/// `display_control_create`.
///
/// The same shape as the rings': made for a device the caller holds with
/// `MANAGE`, the driver's end of the control channel coming back as a handle. The device handle and its `MANAGE` right are the item's to
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
    let location = match node.location() {
        device::Location::Tree(_) => Some(TREE_LOCATION),
        _ => crate::interfaces::block_ring::location_of(node),
    };
    STARTING.lock().push(Start {
        id,
        control: kernel_end,
        device: Arc::clone(node),
        location,
    });
    if sched::spawn("display", run, id, ferrix_sched::NICE_0_WEIGHT).is_err() {
        let _ = take_start(id);
        CLAIMS.release(node);
        return Err(CreateError::NoMemory);
    }
    Ok(driver_end)
}

/// Wait until no display driver's channel claims `node`, for a quiesce
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

/// One card's task.
fn run(id: usize) {
    let Some(start) = take_start(id) else {
        return;
    };
    if let Some(card) = take_up(&start) {
        serve(&card);
        CARDS.lock().retain(|held| !Arc::ptr_eq(held, &card));
        NUMBERS.give_back(card.index);
        crate::fs::devfs::announce(alloc::format!("dri/card{}", card.index).as_bytes(), false);
        crate::console::println!("  display  card{} is gone", card.index);
    }
    CLAIMS.release(&start.device);
}

/// The next message on the control channel, waiting up to `deadline`.
/// `None` when the channel closed or nothing came in time.
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

fn refuse(control: &Endpoint, refusal: Refusal) {
    let bytes = Message::Refused(refusal).encode().as_bytes().to_vec();
    let _ = control.write(bytes, 0, || Ok::<Vec<Transfer>, Infallible>(Vec::new()));
}

/// Take a HELLO, refusing it or answering READY, and publish the card.
fn take_up(start: &Start) -> Option<Arc<Card>> {
    let deadline = timer::now_nanos().saturating_add(HELLO_PATIENCE_NANOS);
    let message = receive(&start.control, deadline)?;
    match accept(start, &message) {
        Ok(card) => Some(card),
        Err(refusal) => {
            crate::console::println!("  display  a driver was refused: {refusal}");
            refuse(&start.control, refusal);
            None
        }
    }
}

fn accept(start: &Start, message: &ChannelMessage) -> Result<Arc<Card>, Refusal> {
    let Ok(Message::Hello(hello)) = Message::decode(&message.bytes) else {
        return Err(Refusal::Malformed);
    };
    let rights: Vec<Rights> = message.handles.iter().map(|(_, rights)| *rights).collect();
    let session = Session::accept(&hello, &rights, CARD_BYTES)?;
    let Some((Object::Port(_driver_port), _)) = message.handles.first() else {
        return Err(Refusal::Rights);
    };
    if start.location.map(Location::raw) != Some(hello.location) {
        return Err(Refusal::WrongLocation);
    }
    if RECLAIMABLE_FRAMEBUFFER.load(Ordering::Relaxed) {
        return Err(Refusal::Framebuffer);
    }
    // A card the driver can map is the kernel's to give, not the driver's to
    // claim: only one whose device files the chardev core already serves for
    // this device, which is NVIDIA's (`docs/DISPLAY.md` §2.1).
    if hello.copies && !crate::interfaces::chardev::publishes_for(&start.device) {
        return Err(Refusal::Copies);
    }
    // The driver reset the device before it sent HELLO, so what a dead one's
    // pins kept from the allocator can go back (`object::pin`'s quarantine),
    // and the kernel's configuration is read back (`DeviceNode::verify_config`).
    start.device.hello_accepted();

    let vmo = Vmo::new_anonymous(CARD_BYTES / PAGE_SIZE).map_err(|_| Refusal::Malformed)?;
    // The wire protocol has no refusal for memory; a malformed start is the
    // nearest it has.
    let core_port = Port::new().map_err(|_| Refusal::Malformed)?;
    let index = NUMBERS.take().ok_or(Refusal::Malformed)?;
    let hdmi = matches!(start.device.location(), device::Location::Tree(_));
    // Read before READY, as Linux reads an override at the connector's first
    // probe: from the initramfs, which takes no time a driver would notice.
    let edids = edid::load(
        index,
        drm::connector_type(hdmi),
        usize::from(hello.scanouts).min(MAX_SCANOUTS),
    );
    let card = Arc::new(Card {
        index,
        node: start.device.index(),
        vmo: Arc::clone(&vmo),
        modes: SpinLock::new(hello.modes),
        modes_changed: AtomicU64::new(0),
        scanouts: usize::from(hello.scanouts),
        control: Arc::clone(&start.control),
        state: SpinLock::new(State {
            session,
            events: Vec::new(),
            gone: false,
            next_buffer: 1,
            next_offset: 0,
            free: Vec::new(),
            ranges: Vec::new(),
            orphans: Vec::new(),
            unwanted: Vec::new(),
            scanout_off: false,
            moves: [None; MAX_SCANOUTS],
        }),
        changed: Arc::new(WaitQueue::new()),
        opened: AtomicBool::new(false),
        dma: start.device.dma_shape(),
        hdmi,
        timings: hello.timings,
        edids,
    });

    // Announced and listed before devmgr is told. devmgr answers a bind
    // asked through sysfs once it hears the card was published, and the
    // writer then expects the boot line and `/dev/dri/card0`, as Linux's bind
    // returns once the driver's probe has registered the device. Told first,
    // devmgr's answer could overtake this thread: test-sysfs under the
    // coverage plugin saw "bind ... done" with no card0 announced.
    //
    // Listed under the card's own lock, held until READY has gone: every
    // message a client's call sends the driver is written under that lock,
    // so none can reach the driver before READY does.
    announce(&card, &hello);
    let mut state = card.state.lock();
    CARDS.lock().push(Arc::clone(&card));

    // Published before READY goes out, as the rings do: devmgr kills a driver
    // that has not published by the time it reports. A READY that cannot be
    // written means the driver is gone, and devmgr hears of its death and
    // quiesces the device; the card listed above goes with it.
    if let Some(location) = start.location {
        crate::discovery::devmgr::published(location);
    }
    let ready = Message::Ready(Ready {
        card: index,
        card_bytes: CARD_BYTES,
    })
    .encode()
    .as_bytes()
    .to_vec();
    let handed = vec![
        // A driver that copies with its processor maps the card read-only;
        // every other only pins it (`docs/DISPLAY.md` §2.1). Never WRITE.
        (Object::Vmo(vmo), Ready::card_rights(hello.copies)),
        (Object::Port(core_port), Rights::WRITE),
    ];
    if start
        .control
        .write(ready, 2, || Ok::<Vec<Transfer>, Infallible>(handed))
        .is_err()
    {
        state.gone = true;
        drop(state);
        card.changed.wake_all();
        CARDS.lock().retain(|held| !Arc::ptr_eq(held, &card));
        NUMBERS.give_back(index);
        crate::console::println!("  display  card{index} is gone");
        return Err(Refusal::Malformed);
    }
    drop(state);
    crate::fs::devfs::announce(alloc::format!("dri/card{index}").as_bytes(), true);

    // The driver owns what is on the screen now, and the boot console, if it
    // was drawing on the firmware's framebuffer, stops.
    crate::console::screen::stop();
    Ok(card)
}

/// What a scanout's boot line says after its size: the refresh its display
/// reported, to the millihertz, or that it reported none -- which the card
/// lists as 60 Hz -- or that nothing is attached.
fn shown_at(mode: &ScanoutMode) -> alloc::string::String {
    if !mode.enabled {
        return alloc::string::String::from(", nothing attached");
    }
    match mode.refresh_mhz {
        0 => alloc::string::String::from(", refresh not reported"),
        rate => alloc::format!(" at {}.{:03} Hz", rate / 1000, rate % 1000),
    }
}

/// The boot lines for a card just published.
fn announce(card: &Card, hello: &Hello) {
    let index = card.index;
    // What the card is, before what it shows: a line a person reading a
    // boot log can tell a GPU from a framebuffer by, and the one thing
    // `docs/GPU.md`'s Path A can be checked against from outside the guest.
    crate::console::println!(
        "  display  card{index} {}",
        if hello.virgl {
            alloc::format!(
                "is a 3D card: virgl, {} capability set{}, the first #{} of {} bytes",
                hello.capsets,
                if hello.capsets == 1 { "" } else { "s" },
                hello.capset,
                hello.capset_bytes
            )
        } else {
            alloc::string::String::from("is a scanout: no 3D")
        }
    );
    if hello.copies {
        // The one driver kind that may map the card, said where a reader
        // of the boot log looks for what a card is.
        crate::console::println!(
            "  display  card{index} copies frames: its driver maps the card read-only"
        );
    }
    for (scanout, mode) in card.modes().iter().enumerate() {
        crate::console::println!(
            "  display  card{index} scanout {scanout}: {}x{}{}",
            mode.width,
            mode.height,
            shown_at(mode)
        );
    }
    if let Some(running) = card.timings().first() {
        crate::console::println!(
            "  display  card{index} runs {}x{} at {} Hz, {} mode{} listed",
            running.hdisplay,
            running.vdisplay,
            running.refresh_hz(),
            card.timings().len(),
            if card.timings().len() == 1 { "" } else { "s" }
        );
    }
}

/// Take a range back from a buffer the driver let go of. Nothing the next
/// buffer's program maps may show what the last one drew, so its pages are
/// decommitted. A page still pinned is skipped by the decommit: the driver
/// replied before it unpinned, which a correct one never does, and the range
/// is kept out of use for good, as a refused detach's is (os-02's review).
fn reclaim(card: &Card, range: Range) {
    let (first, pages) = (range.offset / PAGE_SIZE, range.bytes / PAGE_SIZE);
    let _ = card.vmo.decommit_range(first, pages);
    if card.vmo.holds_any(first, pages) {
        crate::console::println!(
            "  display  card{}: buffer {} is still pinned after its driver let go; \
             {} pages kept out of use",
            card.index,
            range.buffer,
            pages
        );
        return;
    }
    card.state.lock().give_back(range.offset, range.bytes);
}

/// Serve the card until its driver goes.
fn serve(card: &Card) {
    loop {
        if card.state.lock().gone {
            break;
        }
        let deadline = timer::now_nanos().saturating_add(RECHECK_NANOS);
        let message = match card.control.read(MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => message,
            Err(ReadError::Empty) => {
                if card.control.signals().intersects(Signals::PEER_CLOSED) {
                    break;
                }
                card.state.lock().retry(&card.control);
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
        let accepted = card.state.lock().session.receive(&decoded);
        match accepted {
            Ok(Event::Stopped) => break,
            Ok(Event::Modes) => {
                let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
                for (into, mode) in modes.iter_mut().zip(card.state.lock().session.modes()) {
                    *into = *mode;
                }
                card.modes_from(modes);
                card.changed.wake_all();
            }
            Ok(event) => {
                let freed = card.state.lock().settle(&card.control, event);
                if let Some(range) = freed {
                    reclaim(card, range);
                }
                card.changed.wake_all();
            }
            Err(refusal) => {
                crate::console::println!("  display  card{}: {refusal}", card.index);
                refuse(&card.control, refusal);
                break;
            }
        }
    }
    card.state.lock().gone = true;
    card.changed.wake_all();
}
