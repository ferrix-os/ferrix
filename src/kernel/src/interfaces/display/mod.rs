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
//! 2. publishes the card and answers READY with a port; every buffer is a
//!    VMO of its own, which its ATTACH hands the driver (`READ | TRANSFER`);
//! 3. serves: every message the driver sends goes through the session, which
//!    accepts only the replies it is waiting for; the replies it accepts are
//!    queued as [`Event`]s for whoever made the request to collect;
//! 4. ends when the driver closes its end, says STOPPED, or breaks the
//!    protocol, taking the card away.
//!
//! # The kernel never draws
//!
//! A buffer's pages are filled by the program that maps them. The core only
//! says which VMO is which buffer, which is shown, and what changed.
//!
//! # One VMO a buffer
//!
//! A buffer's VMO is the open's that made it and nobody else's: the open
//! maps it by the offset `MODE_MAP_DUMB` names it by, and nothing else on
//! the card. Before, every buffer was a range of one card VMO, and the card's
//! node mapped the whole of it, so an open could map another open's pixels,
//! and a mapping kept past its buffer saw whatever buffer was given its range
//! next (E-02, `docs/DISPLAY.md` §2.1). Now a mapping that outlives its buffer
//! sees only that buffer's own pages, given back, and the card's budget is
//! counted in bytes rather than kept as a range of one object.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_blkring::identity::Location;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_displayctl::message::{
    Attach, AttachObject, FORMAT, Hello, Listed, MAX_BUFFER_PAGES, MAX_BYTES, MAX_LISTED_MODES,
    MAX_SCANOUTS, Message, Ready, Rect, Refusal, ScanoutMode, Status, Timing, Timings,
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
use crate::sync::{SchedParker, SpinLock};
use crate::syscall::native;
use crate::timer;
use crate::user::vmo::Vmo;
use ferrix_displayctl::card::{self as master, Became, OpenId, Taken};
use ferrix_linux_abi::errno::Errno as CardErrno;
use ferrix_sync::SleepLock;

pub(crate) mod check;

pub(crate) mod drm;
pub(crate) mod edid;

/// Linux's major number for DRM devices.
pub(crate) const DRM_MAJOR: u32 = 226;

/// The most replies kept that no request is waiting for.
const MAX_EVENTS: usize = 64;

/// The most memory one card's buffers may take together, and so also the
/// most an opener of the card can make it commit through the card.
///
/// A mapping a program keeps past its buffer's `DESTROY_DUMB` can fault
/// zeroed pages back into that buffer's VMO, up to the buffer's size, after
/// the card has given its bytes back; those pages are charged to the
/// faulting job's memory (`object::quota`), not to this budget. They are its
/// own pages and nobody else's, so isolation holds; the budget bounds what
/// the card commits, the job's limit what such a mapping does.
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
    /// Not a buffer this card may be handed: a client's own buffer on a
    /// card that does not copy, or one smaller than its rows.
    Refused,
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
    /// Whether the driver said it copies, and so maps each buffer's VMO
    /// read-only rather than pinning it.
    copies: bool,
    /// Each scanout's preferred mode: HELLO's, then each MODES's.
    modes: SpinLock<[ScanoutMode; MAX_SCANOUTS]>,
    /// How many times MODES changed `modes`, which an open card reads to
    /// tell its program the connectors changed.
    pub(crate) modes_changed: AtomicU64,
    scanouts: usize,
    control: Arc<Endpoint>,
    state: SpinLock<State>,
    changed: Arc<WaitQueue>,
    /// Held across every call that changes what the card shows, and across
    /// the master check that allows it: `SETCRTC`, `PAGE_FLIP`, `DIRTYFB`, `CURSOR`,
    /// `RMFB` and `DROP_MASTER`, from check to the driver's reply. A sleep lock:
    /// the changes wait for the driver. The lock order is this one, then
    /// [`Card::state`] (`docs/DISPLAY.md` §2.3).
    pub(crate) modeset: SleepLock<()>,
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
    /// Each head's connector type, as its driver's HELLO said, or 0 for the
    /// core's own choice ([`Card::connector_type`]).
    connectors: [u32; MAX_SCANOUTS],
    /// Each head's EDID as its driver read it from the display (EDID,
    /// displayctl v10), checked; a file `drm.edid_firmware` names wins.
    driver_edids: SpinLock<Vec<Option<Vec<u8>>>>,
}

/// The VMO a buffer id was given, and how much of the card's budget it takes.
#[derive(Clone, Debug)]
struct Range {
    buffer: u32,
    vmo: Arc<Vmo>,
    bytes: u64,
    /// The open that made it, which decides whose share of the budget it
    /// is in: the master's, or the other opens' (`card::may_take`).
    owner: OpenId,
    /// Bytes a row, for cleaning the rows a flush names.
    stride: u32,
    /// The dmabuf whose VMO this is, for a client's own buffer a program
    /// shows directly (`docs/DISPLAY.md` §8.2): `None` for a buffer the core
    /// made. Such a range is *shared*: the core allocated none of it, so it
    /// takes none of [`CARD_BYTES`] and is never decommitted
    /// ([`reclaim_plan`]); the dmabuf is held here until the range is
    /// reclaimed after its DETACHED, so its driver cannot hear its release
    /// and recycle the VMO while the display driver still maps it (the
    /// consultant's condition 4, ledger line 3108).
    shared: Option<Arc<crate::interfaces::chardev::dmabuf::Dmabuf>>,
}

impl Range {
    /// Whether the core allocated none of it.
    fn is_shared(&self) -> bool {
        self.shared.is_some()
    }
}

/// The bytes of [`CARD_BYTES`] a range of `bytes` takes: all of them for a
/// buffer the core made, none for a shared one, whose memory is its
/// dmabuf's (the consultant's condition 2).
pub(crate) const fn budget_bytes(shared: bool, bytes: u64) -> u64 {
    if shared { 0 } else { bytes }
}

/// What the budget holds: `taken` bytes in all, and of `ranges` -- each an
/// owner, its bytes and whether it is shared -- what the opens that are not
/// `master` hold. A shared range counts as one of their buffers and none of
/// their bytes (the consultant's condition 2).
pub(crate) fn tally(
    taken: u64,
    master: Option<OpenId>,
    ranges: impl Iterator<Item = (OpenId, u64, bool)>,
) -> Taken {
    let mut tally = Taken {
        bytes: taken,
        ..Taken::default()
    };
    for (owner, bytes, shared) in ranges {
        if Some(owner) != master {
            tally.others = tally.others.saturating_add(budget_bytes(shared, bytes));
            tally.others_buffers += 1;
        }
    }
    tally
}

/// What taking a range back after its DETACHED does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ReclaimPlan {
    /// Decommit its pages, and keep it for good if any is still pinned.
    pub(crate) decommit: bool,
    /// Give its bytes back to the card's budget.
    pub(crate) give_back: bool,
}

/// How a range is reclaimed: a buffer the core made is decommitted and its
/// bytes given back; a shared one is neither -- its VMO is a dmabuf's,
/// which other programs map and its GPU driver may legitimately pin, so
/// decommitting it would let a program holding a read-only descriptor have
/// the card zero memory it shares with others (the consultant's condition
/// 3). It is only let go of.
pub(crate) const fn reclaim_plan(shared: bool) -> ReclaimPlan {
    ReclaimPlan {
        decommit: !shared,
        give_back: !shared,
    }
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
    /// Bytes of [`CARD_BYTES`] the buffers take: every buffer the session
    /// may still track, and every one whose pages its device kept.
    taken: u64,
    /// The VMO of every buffer id the session may still track.
    ranges: Vec<Range>,
    /// Buffers whose pages the device may still hold: a refused detach's,
    /// and one still pinned after its driver let go. Kept for the card's
    /// life, counted in [`State::taken`], so its pages are never freed under
    /// the device and never dropped with a spin lock held.
    kept: Vec<Range>,
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
    /// The open that shows things, if any (`card::set_master`).
    master: Option<OpenId>,
    /// How many opens the card has, at most [`MAX_OPENS`].
    opens: usize,
    /// The number the next open is given.
    next_open: OpenId,
    /// Each head's last finished mode list (`MODE_LIST`, `docs/DISPLAY.md`
    /// §9.3), empty for a head whose driver sent none. Swapped in whole by
    /// the card's task, which built it outside the lock, and copied out
    /// into a vector its reader reserved first: nothing is allocated or
    /// freed under the lock. At most [`MAX_LISTED_MODES`] entries of 28
    /// bytes a head.
    lists: Vec<Vec<Listed>>,
}

/// The most opens one card may have at once.
pub(crate) const MAX_OPENS: usize = 8;

impl State {
    /// What the budget holds now, each buffer counted by whether its open
    /// is master *now*: walked over at most the session's 64 buffers, under
    /// the state lock (the consultant's condition 4, ledger 2327).
    fn taken_now(&self) -> Taken {
        tally(
            self.taken,
            self.master,
            self.ranges
                .iter()
                .map(|range| (range.owner, range.bytes, range.is_shared())),
        )
    }

    /// Take `bytes` of the card's budget for open `owner`, if its share has
    /// them left (`card::may_take`).
    fn reserve(&mut self, bytes: u64, owner: OpenId) -> bool {
        let is_master = self.master == Some(owner);
        if !master::may_take(self.taken_now(), bytes, is_master, CARD_BYTES) {
            return false;
        }
        self.taken = self.taken.saturating_add(bytes);
        true
    }

    /// Turn every scanout off, and hide the cursors, now if the channel has
    /// room and as soon as it does otherwise: what losing the master does,
    /// so that nothing of its frame stays on the screen. With the state
    /// locked, before the master is cleared (the consultant's condition 2).
    fn blank(&mut self, control: &Endpoint, scanouts: usize) {
        for scanout in 0..scanouts {
            if !control.peer_has_room() {
                self.scanout_off = true;
                return;
            }
            let Ok(message) = self.session.scanout(scanout as u32, 0, Rect::default()) else {
                continue;
            };
            if write(control, self, &message).is_err() {
                return;
            }
            // The cursor's image is a plane of its own and stays up through
            // a scanout's off; none, unwaited for.
            if self.session.has_cursor()
                && control.peer_has_room()
                && let Ok(message) = self.session.cursor(scanout as u32, 0, (0, 0), (0, 0))
                && let Message::Cursor { sequence, .. } = message
            {
                self.unwanted.push(Unwanted::Flipped(sequence));
                if write(control, self, &message).is_err() {
                    return;
                }
            }
        }
    }

    /// Give `bytes` of the budget back.
    fn give_back(&mut self, bytes: u64) {
        self.taken = self.taken.saturating_sub(bytes);
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
                // A refused detach leaves the pages the device's: its VMO is
                // kept, and its bytes stay taken (`docs/DISPLAY.md` §2.2).
                if status == Status::Ok {
                    freed = range;
                } else if let Some(range) = range {
                    self.kept.push(range);
                }
                !self.forget(Unwanted::Detached(buffer))
            }
            // Nobody waits for either: [`serve`] ends at STOPPED and takes
            // MODES's modes itself.
            Event::Stopped | Event::Modes | Event::Edid { .. } | Event::ModeList { .. } => false,
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
        if self.scanout_off {
            let scanouts = self.session.modes().len();
            self.scanout_off = false;
            self.blank(control, scanouts);
            if self.scanout_off {
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
    write_handing(control, state, message, Vec::new())
}

/// [`write`], handing the driver `handed` with the message.
fn write_handing(
    control: &Endpoint,
    state: &mut State,
    message: &Message,
    handed: Vec<Transfer>,
) -> Result<(), CardError> {
    let bytes = message.encode().as_bytes().to_vec();
    let count = handed.len();
    if control
        .write(bytes, count, || Ok::<Vec<Transfer>, Infallible>(handed))
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

    /// Whether `head` has a mode list from its driver (`MODE_LIST`), which
    /// its connector then lists instead of anything the core makes up.
    pub(crate) fn has_list(&self, head: usize) -> bool {
        u32::try_from(head).is_ok_and(|scanout| self.state.lock().session.has_list(scanout))
    }

    /// Copy `head`'s mode list into `into`, cleared first. `into` must
    /// hold [`MAX_LISTED_MODES`] already (`reserve_list`), so that the copy
    /// under the state lock allocates nothing; a vector short of that is
    /// left empty.
    pub(crate) fn list_into(&self, head: usize, into: &mut Vec<Listed>) {
        into.clear();
        let state = self.state.lock();
        if let Some(list) = state.lists.get(head)
            && list.len() <= into.capacity()
        {
            into.extend_from_slice(list);
        }
    }

    /// Tell `head`'s driver to run `timing`, one of `list` -- the caller's
    /// copy of the head's list, which the session holds it to -- unless it
    /// was told already. Whether a `SET_MODE` went.
    pub(crate) fn set_mode(
        &self,
        head: usize,
        timing: Timing,
        list: &[Listed],
    ) -> Result<bool, CardError> {
        let scanout = u32::try_from(head).map_err(|_| CardError::Request(RequestError::Mode))?;
        let mut state = self.state.lock();
        if state.gone || self.control.peer_closed() {
            return Err(CardError::Gone);
        }
        if !self.control.peer_has_room() {
            return Err(CardError::Busy);
        }
        let Some(message) = state
            .session
            .set_mode(scanout, timing, list)
            .map_err(CardError::Request)?
        else {
            return Ok(false);
        };
        write(&self.control, &mut state, &message)?;
        Ok(true)
    }

    /// The driver refused the flush after a `SET_MODE` to `head`: it runs
    /// what it ran before, and the next `SETCRTC` tells it again.
    pub(crate) fn mode_refused(&self, head: usize) {
        if let Ok(scanout) = u32::try_from(head) {
            self.state.lock().session.mode_refused(scanout);
        }
    }

    /// Take a finished list for `head` that the card's task built in
    /// `built`, which gets the list it replaces back, to be cleared and
    /// built into again outside the lock.
    fn list_from_driver(&self, head: usize, built: &mut Vec<Listed>) {
        {
            let mut state = self.state.lock();
            if let Some(list) = state.lists.get_mut(head) {
                core::mem::swap(list, built);
            }
        }
        let _ = self.modes_changed.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether `head`'s connector was given an EDID, connected or not: the
    /// connectors that have the `EDID` property.
    pub(crate) fn carries_edid(&self, head: usize) -> bool {
        self.edids.get(head).is_some_and(Option::is_some)
            || self
                .driver_edids
                .lock()
                .get(head)
                .is_some_and(Option::is_some)
    }

    /// `head`'s EDID while a display is there, as Linux's property holds the
    /// blob only while the connector is connected, and none otherwise: the
    /// file `drm.edid_firmware` names for it, or else its driver's.
    pub(crate) fn edid(&self, head: usize) -> Option<Vec<u8>> {
        let connected = self
            .modes
            .lock()
            .get(head)
            .is_some_and(|mode| mode.enabled && head < self.scanouts);
        if !connected {
            return None;
        }
        if let Some(file) = self.edids.get(head).and_then(Option::as_ref) {
            return Some(file.clone());
        }
        self.driver_edids.lock().get(head)?.clone()
    }

    /// `head`'s connector type: what its driver's HELLO said, or else a
    /// board's HDMI output's for a board's display controller and a virtual
    /// card's for any other.
    pub(crate) fn connector_type(&self, head: usize) -> u32 {
        match self.connectors.get(head).copied().unwrap_or(0) {
            0 => drm::connector_type(self.hdmi),
            kind => kind,
        }
    }

    /// Take `head`'s EDID from its driver, checked, in the card's task: the
    /// connectors are told they changed, so a program reads it again.
    fn edid_from_driver(&self, head: usize, bytes: &[u8]) {
        let overridden = self.edids.get(head).is_some_and(Option::is_some);
        let connector = edid::connector_name(self.connector_type(head), head);
        let taken = edid::from_driver(self.index, &connector, bytes, overridden);
        if let Some(slot) = self.driver_edids.lock().get_mut(head) {
            *slot = taken;
        }
        let _ = self.modes_changed.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether the driver is gone.
    pub(crate) fn is_gone(&self) -> bool {
        self.state.lock().gone
    }

    /// What `stat` says of the card: a character device whose size is the
    /// most its buffers may take together (§2.1).
    pub(crate) fn metadata(&self) -> ferrix_vfs::Metadata {
        use ferrix_vfs::{FileType, Metadata, Timespec};
        Metadata {
            ino: (1u64 << 40) + u64::from(self.index),
            kind: FileType::CharDevice,
            // Owner and group only, as Linux's `video` group has it: whoever
            // opens the card holds the screen. An open maps only the buffers
            // it made (E-02).
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

    /// Give a new buffer of `width` × `height` rows of `stride` bytes a VMO
    /// of its own and an id, ATTACH it with the VMO and wait for ATTACHED:
    /// the id, the VMO, which the open that asked maps and nobody else, and
    /// how the attach went.
    pub(crate) fn attach(
        &self,
        owner: OpenId,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<(u32, Arc<Vmo>, Status), CardError> {
        let pixels = u64::from(stride) * u64::from(height);
        let bytes = pixels
            .checked_next_multiple_of(PAGE_SIZE)
            .filter(|&bytes| bytes > 0 && bytes <= MAX_BUFFER_PAGES * PAGE_SIZE)
            .ok_or(CardError::NoRoom)?;
        // A first look, so that no VMO is made for a buffer there is no room
        // for; the bytes are taken with the buffer's listing, in
        // `send_attach`, so two opens at once cannot both pass it.
        {
            let state = self.state.lock();
            let is_master = state.master == Some(owner);
            if !master::may_take(state.taken_now(), bytes, is_master, CARD_BYTES) {
                return Err(CardError::NoRoom);
            }
        }
        let Ok(vmo) = Vmo::new_anonymous(bytes / PAGE_SIZE) else {
            return Err(CardError::NoRoom);
        };
        // Cleaned through the direct map for its life: never
        // write-combining (`L.user.133`). A new object is never refused.
        let _ = vmo.take_kernel_view();
        // A device that reads one run of addresses gets its VMO filled with
        // one block of frames before the driver hears of it, outside the
        // state lock: zeroing megabytes is not something to do with
        // preemption off.
        if self.dma.contiguous && !commit_contiguous(&vmo, 0, bytes) {
            return Err(CardError::NoRoom);
        }
        // An error leaves nothing taken unless the buffer was listed, which
        // only a write to a driver that has just gone can leave behind; its
        // bytes then come back with the card.
        let buffer = self.send_attach(owner, &vmo, bytes, width, height, stride, None)?;
        let event = self.collect(
            |event| matches!(event, Event::Attached { buffer: b, .. } if *b == buffer),
            |state| state.orphans.push(buffer),
        )?;
        match event {
            Event::Attached { status, .. } => Ok((buffer, vmo, status)),
            _ => Err(CardError::Gone),
        }
    }

    /// Show a client's own buffer: give the VMO of `dmabuf`, `bytes` of it
    /// from its start, a buffer id, ATTACH it and wait for ATTACHED -- the id
    /// and how the attach went (`docs/DISPLAY.md` §8.2).
    ///
    /// Only on a card whose driver copies, which is handed the VMO with
    /// `READ | MAP` and never `WRITE`, and only for a dmabuf over a plain
    /// anonymous VMO at least `bytes` long; anything else is
    /// [`CardError::Refused`] with nothing sent (the consultant's
    /// conditions 1 and 6). The range takes none of the card's bytes but
    /// counts as one of an open's buffers.
    pub(crate) fn attach_shared(
        &self,
        owner: OpenId,
        dmabuf: &Arc<crate::interfaces::chardev::dmabuf::Dmabuf>,
        bytes: u64,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<(u32, Status), CardError> {
        let vmo = dmabuf.vmo().ok_or(CardError::Refused)?;
        if !self.copies
            || !vmo.is_anonymous()
            || bytes == 0
            || !bytes.is_multiple_of(PAGE_SIZE)
            || bytes > vmo.len_bytes()
        {
            return Err(CardError::Refused);
        }
        let vmo = Arc::clone(vmo);
        let buffer = self.send_attach(
            owner,
            &vmo,
            bytes,
            width,
            height,
            stride,
            Some(Arc::clone(dmabuf)),
        )?;
        let event = self.collect(
            |event| matches!(event, Event::Attached { buffer: b, .. } if *b == buffer),
            |state| state.orphans.push(buffer),
        )?;
        match event {
            Event::Attached { status, .. } => Ok((buffer, status)),
            _ => Err(CardError::Gone),
        }
    }

    /// Give `vmo` a buffer id and send its ATTACH, handing the driver the VMO:
    /// the id. With the state locked throughout, as [`Card::send`] has it.
    /// `shared` is the dmabuf a client's own buffer is, whose range takes
    /// none of the card's bytes.
    #[expect(
        clippy::too_many_arguments,
        reason = "one ATTACH's fields and the range's owner and dmabuf"
    )]
    fn send_attach(
        &self,
        owner: OpenId,
        vmo: &Arc<Vmo>,
        bytes: u64,
        width: u32,
        height: u32,
        stride: u32,
        shared: Option<Arc<crate::interfaces::chardev::dmabuf::Dmabuf>>,
    ) -> Result<u32, CardError> {
        let mut state = self.state.lock();
        if state.gone || self.control.peer_closed() {
            return Err(CardError::Gone);
        }
        // Before the session commits to the request, as `send` has it: a
        // driver that is behind makes the caller try again.
        if !self.control.peer_has_room() {
            return Err(CardError::Busy);
        }
        let buffer = state.next_buffer;
        let next = buffer.checked_add(1).ok_or(CardError::NoRoom)?;
        let attach = Attach {
            buffer,
            format: FORMAT,
            offset: 0,
            length: bytes,
            width,
            height,
            stride,
        };
        // The bytes are taken here, with the listing below and under the
        // same hold of the lock: the share is the open's as it is now. A
        // shared range takes none, but still counts as a buffer.
        let charged = budget_bytes(shared.is_some(), bytes);
        if !state.reserve(charged, owner) {
            return Err(CardError::NoRoom);
        }
        let message = match state.session.attach(attach) {
            Ok(message) => message,
            Err(error) => {
                state.give_back(charged);
                return Err(CardError::Request(error));
            }
        };
        state.next_buffer = next;
        state.ranges.push(Range {
            buffer,
            vmo: Arc::clone(vmo),
            bytes,
            owner,
            stride,
            shared,
        });
        let handed = vec![(
            Object::Vmo(Arc::clone(vmo)),
            Ready::card_rights(self.copies),
        )];
        write_handing(&self.control, &mut state, &message, handed)?;
        Ok(buffer)
    }

    /// Give an object the render core made a buffer id, `ATTACH_OBJ` it and
    /// wait for ATTACHED: the id and how the attach went.
    ///
    /// The other way to make a buffer, and the difference is where the
    /// pixels are. An ATTACH's are a VMO of their own, which the driver
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
            // A pending blank goes before any later scanout, so the order the
            // driver hears them in is the order they were asked in (the
            // consultant's condition 2); one still waiting for room makes
            // the caller try again.
            if state.scanout_off {
                state.retry(&self.control);
                if state.scanout_off || !self.control.peer_has_room() {
                    return Err(CardError::Busy);
                }
            }
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
    /// Nothing to do for a coherent one, or for a buffer with no VMO here.
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
            .cloned()
        else {
            return;
        };
        let stride = u64::from(range.stride);
        let start = (u64::from(first) * stride).min(range.bytes);
        let end = u64::from(first)
            .saturating_add(u64::from(rows))
            .saturating_mul(stride)
            .min(range.bytes);
        let mut at = start;
        let stop = end;
        while at < stop {
            let page_end = (at / PAGE_SIZE + 1) * PAGE_SIZE;
            let len = page_end.min(stop) - at;
            if let Some(frame) = range.vmo.page(at / PAGE_SIZE) {
                let virt = crate::mm::direct_map(frame * PAGE_SIZE) + at % PAGE_SIZE;
                crate::arch::clean_for_device(virt, len);
            }
            at += len;
        }
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

    /// Whether the card's driver copies each frame with its processor, as
    /// its HELLO said and the core allowed (§2.1).
    pub(crate) fn copies(&self) -> bool {
        self.copies
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

    /// Close open `owner`: the screen goes off if it was the master, and
    /// `buffers` are let go of, none waited for. Allocates nothing, so a
    /// file's Drop may call it (the consultant's condition 1, ledger 2327).
    pub(crate) fn release(&self, owner: OpenId, buffers: impl Iterator<Item = u32>) {
        let mut state = self.state.lock();
        state.opens = state.opens.saturating_sub(1);
        if state.gone {
            if state.master == Some(owner) {
                state.master = None;
            }
            return;
        }
        if state.master == Some(owner) {
            state.blank(&self.control, self.scanouts);
            state.master = None;
        }
        for buffer in buffers {
            state.let_go(&self.control, buffer);
        }
    }

    /// Count a new open: its number, and whether it is the master (the
    /// first open while there is none, made by root).
    ///
    /// # Errors
    ///
    /// `EMFILE` past [`MAX_OPENS`].
    pub(crate) fn open(&self, root: bool) -> Result<(OpenId, bool), CardErrno> {
        let mut state = self.state.lock();
        if state.opens >= MAX_OPENS {
            return Err(CardErrno::EMFILE);
        }
        let id = state.next_open;
        state.next_open = id.wrapping_add(1);
        state.opens += 1;
        let is_master = master::first_open_is_master(state.master, root);
        if is_master {
            state.master = Some(id);
        }
        Ok((id, is_master))
    }

    /// Whether open `id` is the master now.
    pub(crate) fn is_master(&self, id: OpenId) -> bool {
        self.state.lock().master == Some(id)
    }

    /// `SET_MASTER` from open `me` (`card::set_master`).
    ///
    /// # Errors
    ///
    /// `EBUSY`, `EACCES`.
    pub(crate) fn set_master(
        &self,
        me: OpenId,
        was_master: bool,
        root: bool,
    ) -> Result<Became, CardErrno> {
        let mut state = self.state.lock();
        let became = master::set_master(state.master, me, was_master, root)?;
        state.master = Some(me);
        Ok(became)
    }

    /// `DROP_MASTER` from open `me` (`card::drop_master`): the screen goes
    /// off before the master is cleared, under one hold of the state lock,
    /// so nothing of its frame is shown to whoever is master next. The caller
    /// holds [`Card::modeset`].
    ///
    /// # Errors
    ///
    /// `EINVAL` for an open that is not master.
    pub(crate) fn drop_master(&self, me: OpenId) -> Result<(), CardErrno> {
        let mut state = self.state.lock();
        master::drop_master(state.master, me)?;
        if !state.gone {
            state.blank(&self.control, self.scanouts);
        }
        state.master = None;
        Ok(())
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

/// Whether a driver of `device` may say it copies, and so be handed buffer
/// VMOs it can map. A card the driver can map is the kernel's to give, not the
/// driver's to claim: only one whose device files the chardev core already
/// serves for this device, which is NVIDIA's (`docs/DISPLAY.md` §2.1), and
/// the firmware framebuffer's node, which is a screen of memory a driver can
/// only copy into (§8). `check::run` holds it.
pub(crate) fn may_copy(device: &Arc<DeviceNode>) -> bool {
    device.framebuffer_mode().is_some()
        || crate::interfaces::chardev::publishes_for(device)
        // The Intel GPU lent the framebuffer under `ferrix.intelgpu`, by its
        // identity alone (K4, `L.device.48`).
        || device.may_copy_lent()
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
    if hello.copies && !may_copy(&start.device) {
        return Err(Refusal::Copies);
    }
    let framebuffer = start.device.framebuffer_mode().is_some();
    // The driver reset the device before it sent HELLO, so what a dead one's
    // pins kept from the allocator can go back (`object::pin`'s quarantine),
    // and the kernel's configuration is read back (`DeviceNode::verify_config`).
    start.device.hello_accepted();

    // The wire protocol has no refusal for memory; a malformed start is the
    // nearest it has.
    let core_port = Port::new().map_err(|_| Refusal::Malformed)?;
    let index = NUMBERS.take().ok_or(Refusal::Malformed)?;
    // A board's display controller is an HDMI output; the firmware's
    // framebuffer is whatever firmware lit, which DRM calls virtual.
    let hdmi = matches!(start.device.location(), device::Location::Tree(_)) && !framebuffer;
    // Read before READY, as Linux reads an override at the connector's first
    // probe: from the initramfs, which takes no time a driver would notice.
    let heads = usize::from(hello.scanouts).min(MAX_SCANOUTS);
    let kinds: Vec<u32> = hello
        .connectors
        .iter()
        .take(heads)
        .map(|&kind| {
            if kind == 0 {
                drm::connector_type(hdmi)
            } else {
                kind
            }
        })
        .collect();
    let edids = edid::load(index, &kinds);
    let card = Arc::new(Card {
        index,
        node: start.device.index(),
        copies: hello.copies,
        modes: SpinLock::new(hello.modes),
        modes_changed: AtomicU64::new(0),
        scanouts: usize::from(hello.scanouts),
        control: Arc::clone(&start.control),
        state: SpinLock::new(State {
            session,
            events: Vec::new(),
            gone: false,
            next_buffer: 1,
            taken: 0,
            ranges: Vec::new(),
            kept: Vec::new(),
            orphans: Vec::new(),
            unwanted: Vec::new(),
            scanout_off: false,
            moves: [None; MAX_SCANOUTS],
            master: None,
            opens: 0,
            next_open: 1,
            lists: vec![Vec::new(); heads],
        }),
        changed: Arc::new(WaitQueue::new()),
        modeset: SleepLock::new((), &SchedParker),
        dma: start.device.dma_shape(),
        hdmi,
        timings: hello.timings,
        edids,
        connectors: hello.connectors,
        driver_edids: SpinLock::new(vec![None; heads]),
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
    // No VMO: each buffer's comes with its ATTACH, which a driver that
    // copies with its processor maps read-only and every other only pins
    // (`docs/DISPLAY.md` §2.1). Never WRITE.
    let handed = vec![(Object::Port(core_port), Rights::WRITE)];
    if start
        .control
        .write(ready, 1, || Ok::<Vec<Transfer>, Infallible>(handed))
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

/// Take a buffer back that the driver let go of. Its pages are decommitted,
/// so that a mapping its program kept past it -- which only ever reaches its
/// own VMO -- holds nothing of the card's budget. A page still pinned is
/// skipped by the decommit: the driver replied before it unpinned, which a
/// correct one never does, and the buffer is kept and its bytes stay taken
/// for good, as a refused detach's are (os-02's review).
///
/// Runs in the card's task with no lock held, which a decommit needs.
fn reclaim(card: &Card, range: Range) {
    let plan = reclaim_plan(range.is_shared());
    if !plan.decommit {
        // A client's own buffer: let go of, here with no lock held, which
        // its dmabuf's release needs (the consultant's condition 4).
        drop(range);
        return;
    }
    let pages = range.bytes / PAGE_SIZE;
    let _ = range.vmo.decommit_range(0, pages);
    if range.vmo.holds_any(0, pages) {
        crate::console::println!(
            "  display  card{}: buffer {} is still pinned after its driver let go; \
             {} pages kept out of use",
            card.index,
            range.buffer,
            pages
        );
        card.state.lock().kept.push(range);
        return;
    }
    if plan.give_back {
        card.state.lock().give_back(range.bytes);
    }
}

/// Reserve room for a whole mode list in `list`, so that nothing is
/// allocated while a lock is held over it. `false` when there is no memory.
pub(crate) fn reserve_list(list: &mut Vec<Listed>) -> bool {
    list.try_reserve(MAX_LISTED_MODES.saturating_sub(list.len()))
        .is_ok()
}

/// A page of `scanout`'s mode list, which the session accepted: collected
/// in the card task's own `pending`, outside every lock, and taken by the
/// card when it finishes the list. `false` when no memory could be
/// reserved for it, which stops the card rather than panic (the
/// consultant's condition 2, ledger 3117).
fn take_page(
    card: &Card,
    pending: &mut [Vec<Listed>],
    message: &Message,
    event: (u32, bool, bool),
) -> bool {
    let (scanout, starts, finishes) = event;
    let (Message::ModeList(page), Some(built)) = (message, pending.get_mut(scanout as usize))
    else {
        return true;
    };
    if starts {
        built.clear();
    }
    if !reserve_list(built) {
        return false;
    }
    let room = MAX_LISTED_MODES.saturating_sub(built.len());
    built.extend(page.as_slice().iter().take(room).copied());
    if finishes {
        card.list_from_driver(scanout as usize, built);
        built.clear();
        announce_list(card, scanout as usize);
    }
    true
}

/// The boot line for a head's new list: how many modes, and the preferred.
fn announce_list(card: &Card, head: usize) {
    let mut list = Vec::new();
    if !reserve_list(&mut list) {
        return;
    }
    card.list_into(head, &mut list);
    if let Some(preferred) = list.iter().find(|listed| listed.preferred) {
        let mhz = preferred.timing.refresh_mhz();
        crate::console::println!(
            "  display  card{} scanout {head}: {} modes listed, {}x{} at {}.{:03} Hz preferred",
            card.index,
            list.len(),
            preferred.timing.hdisplay,
            preferred.timing.vdisplay,
            mhz / 1000,
            mhz % 1000
        );
    }
}

/// Serve the card until its driver goes.
fn serve(card: &Card) {
    // Each head's mode list being sent, page by page: the card task's own,
    // under no lock (`docs/DISPLAY.md` §9.3).
    let mut pending: Vec<Vec<Listed>> = Vec::new();
    if pending.try_reserve(card.scanouts).is_err() {
        refuse(&card.control, Refusal::Protocol);
        card.state.lock().gone = true;
        card.changed.wake_all();
        return;
    }
    pending.resize_with(card.scanouts, Vec::new);
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
            Ok(Event::Edid { scanout }) => {
                if let Message::Edid(edid) = &decoded {
                    card.edid_from_driver(scanout as usize, edid.as_bytes());
                }
                card.changed.wake_all();
            }
            Ok(Event::ModeList {
                scanout,
                starts,
                finishes,
            }) => {
                if !take_page(card, &mut pending, &decoded, (scanout, starts, finishes)) {
                    crate::console::println!(
                        "  display  card{}: no memory for a mode list; the card stops",
                        card.index
                    );
                    refuse(&card.control, Refusal::Protocol);
                    break;
                }
                card.changed.wake_all();
            }
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
