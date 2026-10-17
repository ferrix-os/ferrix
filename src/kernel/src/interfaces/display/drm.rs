//! `/dev/dri/card<N>` as a program opens it: the DRM/KMS subset
//! `docs/DISPLAY.md` §2.3 answers.
//!
//! Up to [`super::MAX_OPENS`] opens at once, and at most one of them the
//! DRM master, the open that shows things (`ferrix_displayctl::card`'s rules,
//! `docs/DISPLAY.md` §2.3): each open holds the dumb buffers it created, the
//! framebuffers it added and the page-flip events it has not read, and only
//! the master sets a mode, flips a page, reports damage or places a cursor. The card has one
//! connector, one encoder, one CRTC and one primary plane with its immutable
//! `type` property *per scanout the driver reported*, each in a block of
//! four ids of its own below the framebuffers'. A compositor with two
//! monitors is two connectors here, each with a CRTC that shows a
//! framebuffer of its own. Each dumb buffer
//! is a VMO of its own, which the core hands the driver with its ATTACH and
//! takes back decommitted once the device has let go of it. A program maps
//! one through the open that made it, at the offset `MODE_MAP_DUMB` gives,
//! which names the buffer rather than a place: the open answers
//! [`Inode::mapping_at`] for its own buffers and nothing else, and the card's
//! node maps nothing (E-02, `docs/DISPLAY.md` §2.1).
//!
//! A handle is the open's name for a buffer; the core names it by an id no
//! other buffer on the card ever has. `DESTROY_DUMB` of a buffer a
//! framebuffer still refers to only takes the handle away, as on Linux: the
//! buffer is let go of with its last framebuffer.
//!
//! `SETCRTC` and `PAGE_FLIP` wait for the driver's reply, returning once the
//! device has the frame, and a page flip's event is queued then. Letting go
//! of a buffer does not wait.
//!
//! Each head has a cursor plane, set with the legacy `MODE_CURSOR` and
//! `MODE_CURSOR2`: a 64 × 64 dumb buffer as the image, which waits for its
//! pixels to reach the device as a flush does, and a place, which waits for
//! nothing. A pointer moving is then not a frame, and a host that shows the
//! screen to a viewer can hand the viewer the image to draw where its own
//! mouse is (`docs/GPU.md` §3.9).
//!
//! # Offsets
//!
//! The open card is a stream: events, no position. Linux's `drm_read`
//! never looks at the offset it is handed and the DRM core never marks a
//! card a stream, so `pread64` on a card is a `read`, whatever the offset
//! -- measured on a Linux 7.0 host, `pread` of 4096 at 0 and at 1000 and
//! `preadv` at 5 on `card1` (amdgpu) and `renderD128` answered `EAGAIN`
//! under `O_NONBLOCK`, as `read` did, and `pwrite` and a zero-length
//! `write` `EINVAL`. The card here does the same:
//! [`Inode::ignores_position`] is yes. `lseek`, though, is the driver's
//! own on Linux: amdgpu's file operations have none, so it is `ESPIPE`;
//! the drivers built on `DEFINE_DRM_GEM_FOPS`, virtio-gpu's among them,
//! and nvidia's answer 0 (`card2` on the same host). No program seeks a
//! DRM descriptor, so the card keeps the `ESPIPE` it had, amdgpu's answer:
//! [`Inode::seek_is_noop`] is no.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_displayctl::message::{CURSOR_SIZE, MAX_DIMENSION, MAX_SCANOUTS, Rect, Status, Timing};
use ferrix_linux_abi::drm::{
    self, CardRes, ClipRect, CreateDumb, Crtc, CrtcPageFlip, DestroyDumb, Event, EventVblank,
    FbCmd, FbCmd2, FbDirtyCmd, Field, GetBlob, GetCap, GetConnector, GetEncoder, GetPlane,
    GetPlaneRes, GetProperty, Layout, MapDumb, ModeCursor, ModeCursor2, ModeInfo, ObjGetProperties,
    PrimeHandle, PropertyEnum, SetClientCap, Version,
};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::socket::Width;
use ferrix_vfs::{Inode, Metadata, Readiness, Result as VfsResult};

use super::{Card, CardError};
use ferrix_displayctl::card::{self as rules, Became, Class, OpenId};

use crate::user::vmo::Vmo;

use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};
use crate::syscall::uaccess;
use crate::timer;

// Object ids. Linux numbers every mode object of a device, whatever its type,
// from one idr, so an id names one object and `OBJ_GETPROPERTIES` can look one
// up with `DRM_MODE_OBJECT_ANY`. The card keeps that: the fixed objects take
// ids below `FIRST_FRAMEBUFFER_ID`, and framebuffers are numbered from there
// up, never reused within an open, as `add_framebuffer` counts.

/// How far up a `MODE_MAP_DUMB` offset the handle is: the low bits are a
/// place in the buffer, which no buffer is too big for, and the high bits
/// are which buffer of the open. An offset is a name, as the render node's
/// are (`interfaces::render::node`); 25 rather than its 32 so that an
/// `ARMv7` `mmap2`, which counts the offset in pages in 32 bits, reaches handles up
/// to 2^19.
const MAP_SHIFT: u32 = 25;

const _: () = assert!(
    1u64 << MAP_SHIFT >= ferrix_displayctl::message::MAX_BUFFER_PAGES * PAGE_SIZE,
    "a buffer is larger than a map offset's place bits reach"
);

/// How many objects one head has: a CRTC, an encoder, a connector and a
/// primary plane, in a block of ids of its own.
const PER_HEAD: u32 = 4;

/// Which of a head's four an id is.
const CRTC: u32 = 0;
/// The encoder's place in the block.
const ENCODER: u32 = 1;
/// The connector's.
const CONNECTOR: u32 = 2;
/// The primary plane's.
const PLANE: u32 = 3;

/// The object id of `head`'s `kind`, the blocks starting at 1.
const fn object_id(head: usize, kind: u32) -> u32 {
    1 + PER_HEAD * head as u32 + kind
}

/// The head an id names, if it is of `kind`.
///
/// The card answers for a head the driver reported; an id in a block past
/// them is `ENOENT`, which is what Linux gives an id its idr does not hold.
fn head_of(card: &Card, id: u32, kind: u32) -> Option<usize> {
    let index = id.checked_sub(1)?;
    if index % PER_HEAD != kind {
        return None;
    }
    let head = (index / PER_HEAD) as usize;
    (head < heads(card)).then_some(head)
}

/// How many heads the card has: one per scanout the driver reported, which
/// is one connector each, connected or not.
fn heads(card: &Card) -> usize {
    card.modes().len().min(MAX_SCANOUTS)
}

/// The plane `type` property's id, above every head's block.
const TYPE_PROPERTY_ID: u32 = 1 + PER_HEAD * MAX_SCANOUTS as u32;
/// The connector `EDID` property's id, after it.
const EDID_PROPERTY_ID: u32 = TYPE_PROPERTY_ID + 1;
/// The id of head 0's EDID blob; each head's is its own, in order.
const FIRST_EDID_BLOB_ID: u32 = EDID_PROPERTY_ID + 1;
/// The first framebuffer id; everything below is a fixed object's.
const FIRST_FRAMEBUFFER_ID: u32 = 128;

const _: () = assert!(
    FIRST_EDID_BLOB_ID + (MAX_SCANOUTS as u32) <= FIRST_FRAMEBUFFER_ID,
    "every fixed object's id is below the framebuffers'"
);

/// The id of `head`'s EDID blob.
const fn edid_blob_id(head: usize) -> u32 {
    FIRST_EDID_BLOB_ID + head as u32
}

/// The head whose EDID blob `id` names, if it names one the card holds: a
/// connected connector's that was given an EDID.
fn head_of_blob(card: &Card, id: u32) -> Option<usize> {
    let head = usize::try_from(id.checked_sub(FIRST_EDID_BLOB_ID)?).ok()?;
    (head < heads(card) && card.edid(head).is_some()).then_some(head)
}

/// The one format the primary plane takes.
const PLANE_FORMATS: [u32; 1] = [drm::FORMAT_XRGB8888];

/// The `type` property's named values, in `drm_plane_type_enum_list`'s order
/// (`drivers/gpu/drm/drm_mode_config.c`).
const PLANE_TYPES: [(u64, &[u8]); 3] = [
    (drm::PLANE_TYPE_OVERLAY, b"Overlay"),
    (drm::PLANE_TYPE_PRIMARY, b"Primary"),
    (drm::PLANE_TYPE_CURSOR, b"Cursor"),
];

/// This build's pointer width, which `DRM_IOCTL_VERSION`'s layout follows.
const NATIVE: Width = if size_of::<usize>() == 8 {
    Width::Bits64
} else {
    Width::Bits32
};

/// The most page-flip events kept unread.
const MAX_EVENTS: usize = 64;

/// The most framebuffers one open may have, as the session caps buffers.
const MAX_FRAMEBUFFERS: usize = 64;

/// A buffer object of this open: a dumb buffer, or one imported from the
/// render node.
#[derive(Clone, Copy, Debug)]
struct Dumb {
    handle: u32,
    /// The core's id for it.
    buffer: u32,
    /// Whether its pixels are a VMO of this open's, in
    /// [`OpenState::pages`]: a dumb buffer's are.
    ///
    /// Not an imported one's, whose pixels are on the device: `MODE_MAP_DUMB`
    /// of such a handle answers nothing.
    mapped: bool,
    width: u32,
    height: u32,
    pitch: u32,
    /// `DESTROY_DUMB` took the handle while a framebuffer still refers to it.
    destroyed: bool,
}

/// A framebuffer: a dumb buffer as `ADDFB` named it.
#[derive(Clone, Copy, Debug)]
struct Framebuffer {
    id: u32,
    handle: u32,
    buffer: u32,
    width: u32,
    height: u32,
}

#[derive(Debug, Default)]
struct OpenState {
    dumbs: Vec<Dumb>,
    /// The VMO of each dumb buffer this open made, by its handle: what a
    /// mapping of the open reaches, and all it reaches. An entry goes with
    /// its dumb buffer.
    pages: Vec<(u32, Arc<Vmo>)>,
    /// The objects imported from the render node, by the handle each was
    /// given. Holding one keeps the renderer's object alive for as long as
    /// this open can show it, whatever the program does with the handle it
    /// drew through.
    imports: Vec<(u32, Arc<crate::interfaces::render::node::Object>)>,
    framebuffers: Vec<Framebuffer>,
    next_handle: u32,
    next_framebuffer: u32,
    /// The framebuffer on each head's CRTC, and the mode it was set with.
    shown: BTreeMap<usize, (u32, ModeInfo)>,
    events: VecDeque<[u8; 32]>,
    sequence: u32,
    /// Where each head's cursor was last put, which a call that sets only
    /// the image shows it at.
    cursors: BTreeMap<usize, (i32, i32)>,
}

/// One open of a card.
pub(crate) struct CardFile {
    card: Arc<Card>,
    /// This open's number on the card, which the master is named by.
    id: OpenId,
    /// Whether this open was ever master, which lets it take master back
    /// without root after it dropped it (`card::set_master`).
    was_master: AtomicBool,
    state: SpinLock<OpenState>,
    readable: Arc<WaitQueue>,
    /// Whether the open set `DRM_CLIENT_CAP_UNIVERSAL_PLANES`, without which
    /// `GETPLANERESOURCES` lists only overlay planes, and so none here.
    universal_planes: AtomicBool,
    /// The card's `modes_changed` this open last told its program of with
    /// [`drm::EVENT_FERRIX_CONNECTORS`].
    modes_told: AtomicU64,
}

impl core::fmt::Debug for CardFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CardFile")
            .field("card", &self.card)
            .finish_non_exhaustive()
    }
}

impl CardFile {
    /// Open `card`: the master if there is none and the opener is root.
    pub(crate) fn open(card: Arc<Card>) -> Result<Arc<CardFile>, Errno> {
        if card.is_gone() {
            return Err(Errno::ENXIO);
        }
        let root = process::current().is_some_and(|opener| is_root(&opener));
        let (id, is_master) = card.open(root)?;
        Ok(Arc::new(CardFile {
            id,
            was_master: AtomicBool::new(is_master),
            state: SpinLock::new(OpenState {
                next_handle: 1,
                next_framebuffer: FIRST_FRAMEBUFFER_ID,
                ..OpenState::default()
            }),
            readable: Arc::new(WaitQueue::new()),
            universal_planes: AtomicBool::new(false),
            modes_told: AtomicU64::new(card.modes_changed.load(Ordering::Acquire)),
            card,
        }))
    }
}

impl Drop for CardFile {
    /// Take the screen off if this was the master, and let go of the buffers
    /// without waiting, allocating nothing (the consultant's condition 1): the
    /// replies go to the card's task, which drops them. A buffer whose
    /// detach the device refuses keeps its VMO and its share of the card's
    /// budget for good.
    fn drop(&mut self) {
        let state = self.state.get_mut();
        self.card
            .release(self.id, state.dumbs.iter().map(|dumb| dumb.buffer));
    }
}

impl Inode for CardFile {
    fn metadata(&self) -> Metadata {
        self.card.metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    /// The VMO of the dumb buffer a `MODE_MAP_DUMB` offset names, if this
    /// open made it and still holds its handle, and the byte of it the
    /// offset is. Nothing else: no other open's buffer, no buffer given
    /// back, and nothing past a buffer's end, which the mapping's bounds
    /// check against this VMO refuses (E-02).
    fn mapping_at(&self, offset: u64) -> Option<(Arc<dyn Any + Send + Sync>, u64)> {
        let handle = u32::try_from(offset >> MAP_SHIFT).ok()?;
        let within = offset & ((1 << MAP_SHIFT) - 1);
        let state = self.state.lock();
        if !state
            .dumbs
            .iter()
            .any(|dumb| dumb.handle == handle && dumb.mapped && !dumb.destroyed)
        {
            return None;
        }
        let (_, vmo) = state.pages.iter().find(|(held, _)| *held == handle)?;
        let vmo: Arc<dyn Any + Send + Sync> = Arc::clone(vmo) as Arc<dyn Any + Send + Sync>;
        Some((vmo, within))
    }

    /// Linux's DRM files have no `splice_read`: `sendfile` and `splice`
    /// from one are `EINVAL`, into a pipe as into anything.
    fn splices_out(&self) -> bool {
        false
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// `pread64` reads events as `read` does, the offset ignored: see the
    /// module's documentation on offsets.
    fn ignores_position(&self) -> bool {
        true
    }

    /// And `lseek` stays `ESPIPE`: see the module's documentation.
    fn seek_is_noop(&self) -> bool {
        false
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.read_stream(buf, false)
    }

    /// Readable with a page-flip event to read, or a connector change not
    /// told yet, or once the driver is gone, when a read returns 0; never
    /// writable, as on Linux.
    fn poll(&self) -> Readiness {
        let waiting = !self.state.lock().events.is_empty() || self.modes_untold();
        Readiness {
            readable: waiting || self.card.is_gone(),
            writable: false,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    /// The events queue's wakes, and the card's, which a driver that goes
    /// wakes.
    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(crate::fs::wake::shared(&self.readable));
        visit(crate::fs::wake::shared(&self.card.changed));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(
            self.readable
                .wakes()
                .wrapping_add(self.card.changed.wakes()),
        )
    }

    /// Put back events a read took but could not deliver, as Linux's
    /// `drm_read` does, so a bad buffer loses no flip. A connector change,
    /// which a read puts first and alone is eight bytes, is owed again.
    fn unread_stream(&self, bytes: &[u8]) {
        let flips = if bytes.len() % EventVblank::SIZE == Event::SIZE {
            self.modes_told.store(
                self.card
                    .modes_changed
                    .load(Ordering::Acquire)
                    .wrapping_sub(1),
                Ordering::Release,
            );
            bytes.get(Event::SIZE..).unwrap_or_default()
        } else {
            bytes
        };
        let mut state = self.state.lock();
        for chunk in flips.chunks_exact(EventVblank::SIZE).rev() {
            let mut event = [0u8; 32];
            event.copy_from_slice(chunk);
            state.events.push_front(event);
        }
        drop(state);
        self.readable.wake_all();
    }

    /// Page-flip events, whole records only, as Linux's `drm_read` reads
    /// them: as many as fit; with one queued that does not fit, 0, the event
    /// left for a larger read; with none, `EAGAIN` under `O_NONBLOCK` and
    /// otherwise a wait, whatever the count. A blocking read ends with a
    /// restart code when the reader has a signal to take.
    ///
    /// Measured on a 7.0 host, `card1` (amdgpu) and `card2` (nvidia) with
    /// nothing queued answer a read of 8 and of 0 with `EAGAIN`; the case of
    /// an event that does not fit could not be made there, neither card
    /// giving an unprivileged opener an event, and is `drm_read`'s own code,
    /// `if (length > count - ret)` putting the event back and answering
    /// `ret`. This used to answer `EINVAL` for any read smaller than an
    /// event.
    ///
    /// A connector change comes first, as Ferrix's own eight-byte
    /// [`drm::EVENT_FERRIX_CONNECTORS`], once however many changes there were
    /// since the last: what it asks of the program is to read the
    /// connectors again, which reads them as they are now.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> VfsResult<usize> {
        let mut written = 0;
        let mut take = || {
            let changed = self.card.modes_changed.load(Ordering::Acquire);
            let untold = self.modes_told.load(Ordering::Acquire) != changed;
            if untold && let Some(slot) = buf.get_mut(..Event::SIZE) {
                let event = Event {
                    r#type: drm::EVENT_FERRIX_CONNECTORS,
                    length: Event::SIZE as u32,
                };
                if event.write(slot).is_some() {
                    written = Event::SIZE;
                    self.modes_told.store(changed, Ordering::Release);
                }
            }
            let mut state = self.state.lock();
            while buf.len() - written >= EventVblank::SIZE {
                let Some(event) = state.events.pop_front() else {
                    break;
                };
                if let Some(slot) = buf.get_mut(written..written + EventVblank::SIZE) {
                    slot.copy_from_slice(&event);
                }
                written += EventVblank::SIZE;
            }
            // An event still queued is one that did not fit: the read ends,
            // with what it took, 0 if nothing.
            written > 0 || !state.events.is_empty() || self.modes_untold() || self.card.is_gone()
        };
        if nonblock {
            if take() {
                return Ok(written);
            }
            return Err(Errno::EAGAIN);
        }
        let caller = process::current();
        let killed = || {
            caller
                .as_ref()
                .is_some_and(|process| process.signal_pending())
        };
        // A connector change wakes the card's queue, not this open's, so the
        // wait looks again every so often rather than miss one.
        while !self
            .readable
            .wait_until_deadline(|| take() || killed(), timer::now_nanos() + RECHECK_NANOS)
        {
        }
        if written == 0 && !self.card.is_gone() && killed() {
            return Err(Errno::ERESTARTSYS);
        }
        Ok(written)
    }
}

impl CardFile {
    /// Whether the card's connectors changed since this open last said so.
    fn modes_untold(&self) -> bool {
        self.modes_told.load(Ordering::Acquire) != self.card.modes_changed.load(Ordering::Acquire)
    }
}

/// How often a blocking read of the card looks for a connector change.
const RECHECK_NANOS: u64 = 50_000_000;

/// The open card `file` reads and writes go to, if it is one.
pub(crate) fn of(io: &Arc<dyn Inode>) -> Option<Arc<CardFile>> {
    Arc::clone(io).into_any().downcast::<CardFile>().ok()
}

fn card_error(error: CardError) -> Errno {
    match error {
        CardError::Request(_) => Errno::EINVAL,
        CardError::NoRoom => Errno::ENOMEM,
        CardError::Busy => Errno::EBUSY,
        CardError::Gone => Errno::ENODEV,
        CardError::TimedOut => Errno::ETIMEDOUT,
    }
}

fn status_error(status: Status) -> Result<(), Errno> {
    match status {
        Status::Ok => Ok(()),
        Status::OutOfMemory | Status::PinFailed => Err(Errno::ENOMEM),
        Status::DeviceRefused | Status::Invalid => Err(Errno::EIO),
    }
}

fn read_arg<L: Layout>(process: &Process, arg: u64) -> Result<L, Errno> {
    let mut bytes = vec![0u8; L::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    L::read(&bytes).ok_or(Errno::EFAULT)
}

fn write_arg<L: Layout>(process: &Process, arg: u64, value: &L) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; L::SIZE];
    value.write(&mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// Copy `ids` to the user array at `at` if it has room for `capacity`.
fn write_ids(process: &Process, at: u64, capacity: u32, ids: &[u32]) -> Result<(), Errno> {
    if at == 0 || (capacity as usize) < ids.len() {
        return Ok(());
    }
    let bytes: Vec<u8> = ids.iter().flat_map(|id| id.to_le_bytes()).collect();
    uaccess::copy_to_user(process.space(), at, &bytes).map_err(|_| Errno::EFAULT)
}

/// The sizes a connected connector offers beside the one the device prefers.
///
/// A virtio-gpu shows whatever size it is handed a scanout of, so the
/// device's own mode is a preference and not a limit -- and under a window
/// the preference is the *window's* size, which for a window QEMU has just
/// opened is 640x480 whatever `xres` and `yres` said. Linux's `virtio_gpu`
/// lists the standard sizes beside the preferred one for the same reason
/// (`drm_add_modes_noedid` in `virtio_gpu_conn_get_modes`), and a
/// `monitor = , 1920x1080, ...` line is how somebody picks one.
const STANDARD_SIZES: [(u32, u32); 10] = [
    (3840, 2160),
    (2560, 1440),
    (1920, 1200),
    (1920, 1080),
    (1680, 1050),
    (1600, 900),
    (1280, 1024),
    (1280, 720),
    (1024, 768),
    (800, 600),
];

/// The refreshes a connector lists its preferred size at beside the one its
/// display reports, in hertz: the 60 every monitor has, and the two rates
/// fast monitors are sold at, so that a `monitor = , WxH@120, ...` line can
/// ask for a desktop faster than its host's window shows. A virtio-gpu
/// takes a flush whenever it is sent one, so nothing about the card stops a
/// compositor pacing to any of them.
const OFFERED_REFRESHES_HZ: [u32; 3] = [60, 120, 144];

/// The refresh a display that reported none is listed at, in millihertz:
/// the 60 Hz every virtual card was listed at before its driver could say.
const UNKNOWN_REFRESH_MHZ: u32 = 60_000;

/// What a connector whose device prefers `width` by `height` at
/// `refresh_mhz` (0 when it does not know) lists: that size first, at that
/// refresh and marked preferred; then the same size at each of
/// [`OFFERED_REFRESHES_HZ`] it is not listed at already; then every standard
/// size that is not it, at 60 Hz.
fn listed_modes(width: u32, height: u32, refresh_mhz: u32) -> Vec<ModeInfo> {
    let reported = if refresh_mhz == 0 {
        UNKNOWN_REFRESH_MHZ
    } else {
        refresh_mhz
    };
    let mut modes = vec![mode_at(width, height, reported)];
    for hertz in OFFERED_REFRESHES_HZ {
        if modes.iter().all(|mode| mode.vrefresh != hertz) {
            let mut mode = mode_at(width, height, hertz * 1000);
            mode.r#type = drm::MODE_TYPE_DRIVER;
            modes.push(mode);
        }
    }
    let standard = STANDARD_SIZES
        .iter()
        .filter(|&&size| size != (width, height))
        .map(|&(wide, tall)| {
            let mut mode = mode_for(wide, tall);
            mode.r#type = drm::MODE_TYPE_DRIVER;
            mode
        });
    modes.extend(standard);
    modes
}

/// A mode a card listed, as DRM has it: its own timing, `preferred` for the
/// one the card runs from the start.
fn mode_of(timing: &Timing, preferred: bool) -> ModeInfo {
    let mut mode = mode_for(u32::from(timing.hdisplay), u32::from(timing.vdisplay));
    mode.clock = timing.clock_khz;
    mode.hsync_start = timing.hsync_start;
    mode.hsync_end = timing.hsync_end;
    mode.htotal = timing.htotal;
    mode.vsync_start = timing.vsync_start;
    mode.vsync_end = timing.vsync_end;
    mode.vtotal = timing.vtotal;
    mode.vrefresh = timing.refresh_hz();
    mode.flags = if timing.hsync_high {
        drm::MODE_FLAG_PHSYNC
    } else {
        drm::MODE_FLAG_NHSYNC
    } | if timing.vsync_high {
        drm::MODE_FLAG_PVSYNC
    } else {
        drm::MODE_FLAG_NVSYNC
    };
    mode.r#type = if preferred {
        drm::MODE_TYPE_PREFERRED | drm::MODE_TYPE_DRIVER
    } else {
        drm::MODE_TYPE_DRIVER
    };
    mode
}

/// [`mode_at`] 60 Hz.
fn mode_for(width: u32, height: u32) -> ModeInfo {
    mode_at(width, height, UNKNOWN_REFRESH_MHZ)
}

/// The mode a size makes at `refresh_mhz`: sync pulses where a monitor
/// would put them, the pixel clock the refresh takes over those totals, in
/// whole kHz rounded down, and `vrefresh` what that clock gives, rounded --
/// so that the two agree as Linux's `drm_mode_vrefresh` has them agree.
/// Marked preferred, and named `WxH`, which Linux's names do not add the
/// refresh to.
fn mode_at(width: u32, height: u32, refresh_mhz: u32) -> ModeInfo {
    let mut mode = ModeInfo::ZERO;
    let clamp = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
    mode.hdisplay = clamp(width);
    mode.hsync_start = clamp(width + 16);
    mode.hsync_end = clamp(width + 48);
    mode.htotal = clamp(width + 160);
    mode.vdisplay = clamp(height);
    mode.vsync_start = clamp(height + 3);
    mode.vsync_end = clamp(height + 9);
    mode.vtotal = clamp(height + 23);
    let total = u64::from(mode.htotal) * u64::from(mode.vtotal);
    let clock = total * u64::from(refresh_mhz) / 1_000_000;
    mode.clock = u32::try_from(clock).unwrap_or(u32::MAX);
    mode.vrefresh = u32::try_from((clock * 1000 + total / 2) / total.max(1)).unwrap_or(0);
    mode.r#type = drm::MODE_TYPE_PREFERRED | drm::MODE_TYPE_DRIVER;
    let mut name = [0u8; drm::DISPLAY_MODE_LEN];
    let mut cursor = 0;
    let mut push = |bytes: &[u8]| {
        for &byte in bytes {
            if let Some(slot) = name.get_mut(cursor) {
                *slot = byte;
                cursor += 1;
            }
        }
    };
    push(&decimal(width));
    push(b"x");
    push(&decimal(height));
    mode.name = name;
    mode
}

fn decimal(mut value: u32) -> Vec<u8> {
    let mut digits = Vec::new();
    loop {
        digits.push(b'0' + (value % 10) as u8);
        value /= 10;
        if value == 0 {
            break;
        }
    }
    digits.reverse();
    digits
}

/// Answer `request` on the open card `file`, or `ENOTTY` for one this
/// subset does not have.
pub(crate) fn ioctl(
    process: &Process,
    file: &CardFile,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    // One table decides, and a call in no class is refused
    // (`card::class_of`, the consultant's condition 5).
    let class = rules::class_of(request, drm::ioctl_version(NATIVE)).ok_or(Errno::ENOTTY)?;
    // A call that changes what is shown, or who shows it, holds the card's
    // modeset lock from the master check to the driver's reply, so a
    // DROP_MASTER cannot pass between them (condition 3).
    let _modeset = (class != Class::Any).then(|| file.card.modeset.lock());
    rules::permit(class, file.card.is_master(file.id))?;
    match request {
        drm::IOCTL_SET_MASTER => set_master(process, file),
        drm::IOCTL_DROP_MASTER => drop_master(file),
        request if request == drm::ioctl_version(NATIVE) => version(process, arg),
        drm::IOCTL_GET_CAP => get_cap(process, &file.card, arg),
        drm::IOCTL_SET_CLIENT_CAP => set_client_cap(process, file, arg),
        drm::IOCTL_MODE_GETRESOURCES => get_resources(process, file, arg),
        drm::IOCTL_MODE_GETCONNECTOR => connector(process, &file.card, arg),
        drm::IOCTL_MODE_GETENCODER => get_encoder(process, &file.card, arg),
        drm::IOCTL_MODE_GETCRTC => get_crtc(process, file, arg),
        drm::IOCTL_MODE_CREATE_DUMB => create_dumb(process, file, arg),
        drm::IOCTL_MODE_MAP_DUMB => map_dumb(process, file, arg),
        drm::IOCTL_PRIME_FD_TO_HANDLE => prime_fd_to_handle(process, file, arg),
        drm::IOCTL_MODE_DESTROY_DUMB => {
            let destroy: DestroyDumb = read_arg(process, arg)?;
            destroy_dumb(file, destroy.handle).map(|()| 0)
        }
        drm::IOCTL_MODE_ADDFB => add_fb(process, file, arg),
        drm::IOCTL_MODE_ADDFB2 => add_fb2(process, file, arg),
        drm::IOCTL_MODE_RMFB => {
            let mut bytes = [0u8; 4];
            uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
            // An Any call, so the lock is taken here: a framebuffer this open
            // shows goes off the screen with it.
            let _modeset = file.card.modeset.lock();
            remove_framebuffer(file, u32::from_le_bytes(bytes)).map(|()| 0)
        }
        drm::IOCTL_MODE_SETCRTC => set_crtc(process, file, arg),
        drm::IOCTL_MODE_PAGE_FLIP => page_flip(process, file, arg),
        drm::IOCTL_MODE_DIRTYFB => dirty_fb(process, file, arg),
        drm::IOCTL_MODE_CURSOR => {
            let legacy: ModeCursor = read_arg(process, arg)?;
            cursor(
                file,
                &ModeCursor2 {
                    flags: legacy.flags,
                    crtc_id: legacy.crtc_id,
                    x: legacy.x,
                    y: legacy.y,
                    width: legacy.width,
                    height: legacy.height,
                    handle: legacy.handle,
                    hot_x: 0,
                    hot_y: 0,
                },
            )
        }
        drm::IOCTL_MODE_CURSOR2 => cursor(file, &read_arg(process, arg)?),
        drm::IOCTL_MODE_GETPLANERESOURCES => get_plane_resources(process, file, arg),
        drm::IOCTL_MODE_GETPLANE => get_plane(process, file, arg),
        drm::IOCTL_MODE_OBJ_GETPROPERTIES => obj_get_properties(process, file, arg),
        drm::IOCTL_MODE_GETPROPERTY => get_property(process, arg),
        drm::IOCTL_MODE_GETPROPBLOB => get_blob(process, &file.card, arg),
        _ => Err(Errno::ENOTTY),
    }
}

/// Whether `process` acts as root: its effective uid, the kernel's own id.
fn is_root(process: &Process) -> bool {
    process.with_credentials(|credentials| credentials.user.effective == 0)
}

/// `SET_MASTER`, as `card::set_master` decides: root, or an open that was
/// master before, takes it while nobody has it.
fn set_master(process: &Process, file: &CardFile) -> Result<usize, Errno> {
    let became = file.card.set_master(
        file.id,
        file.was_master.load(Ordering::Acquire),
        is_root(process),
    )?;
    if became == Became::Master {
        file.was_master.store(true, Ordering::Release);
    }
    Ok(0)
}

/// `DROP_MASTER`: the screen goes off and nothing this open showed is shown
/// any more, so its `GETCRTC` says so too. Its buffers and framebuffers stay
/// its own, as on Linux, for when it takes master back.
fn drop_master(file: &CardFile) -> Result<usize, Errno> {
    file.card.drop_master(file.id)?;
    file.state.lock().shown.clear();
    Ok(0)
}

/// `SET_CLIENT_CAP` as Linux's `drm_setclientcap` answers a driver without
/// atomic: universal planes is 0 or 1 and only changes what
/// `GETPLANERESOURCES` lists; atomic is refused, so clients stay on the
/// legacy calls (`docs/DISPLAY.md` §5).
fn set_client_cap(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let cap: SetClientCap = read_arg(process, arg)?;
    match cap.capability {
        drm::CLIENT_CAP_UNIVERSAL_PLANES => {
            if cap.value > 1 {
                return Err(Errno::EINVAL);
            }
            file.universal_planes
                .store(cap.value == 1, Ordering::Relaxed);
            Ok(0)
        }
        drm::CLIENT_CAP_ATOMIC => Err(Errno::EOPNOTSUPP),
        _ => Err(Errno::EINVAL),
    }
}

/// Copy the first `capacity` of `items` to the user array at `at`, as
/// Linux's per-element `put_user` loops do: a short array gets what fits, and
/// the count written back says how many there are.
fn write_prefix<T: Copy>(
    process: &Process,
    at: u64,
    capacity: u32,
    items: &[T],
    bytes_of: impl Fn(T) -> Vec<u8>,
) -> Result<(), Errno> {
    let fits = items.len().min(capacity as usize);
    if fits == 0 {
        return Ok(());
    }
    let bytes: Vec<u8> = items
        .iter()
        .take(fits)
        .flat_map(|&item| bytes_of(item))
        .collect();
    uaccess::copy_to_user(process.space(), at, &bytes).map_err(|_| Errno::EFAULT)
}

/// `GETPLANERESOURCES`: the primary plane to an open that set universal
/// planes, and no plane to one that did not, since Linux's
/// `drm_mode_getplane_res` lists only overlay planes then.
fn get_plane_resources(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut resources: GetPlaneRes = read_arg(process, arg)?;
    let planes: Vec<u32> = if file.universal_planes.load(Ordering::Relaxed) {
        (0..heads(&file.card))
            .map(|head| object_id(head, PLANE))
            .collect()
    } else {
        Vec::new()
    };
    write_prefix(
        process,
        resources.plane_id_ptr,
        resources.count_planes,
        &planes,
        |id| id.to_le_bytes().to_vec(),
    )?;
    resources.count_planes = u32::try_from(planes.len()).unwrap_or(u32::MAX);
    write_arg(process, arg, &resources)
}

/// `GETPLANE`, as `drm_mode_getplane` answers it for a plane without atomic
/// state: the CRTC and framebuffer are the ones `SETCRTC` and `PAGE_FLIP`
/// last showed, and the formats are copied only into an array with room for
/// all of them.
fn get_plane(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut plane: GetPlane = read_arg(process, arg)?;
    let head = head_of(&file.card, plane.plane_id, PLANE).ok_or(Errno::ENOENT)?;
    let shown = file.state.lock().shown.get(&head).map(|(id, _)| *id);
    plane.crtc_id = if shown.is_some() {
        object_id(head, CRTC)
    } else {
        0
    };
    plane.fb_id = shown.unwrap_or(0);
    plane.possible_crtcs = crtc_mask(head);
    plane.gamma_size = 0;
    if plane.count_format_types as usize >= PLANE_FORMATS.len() {
        let bytes: Vec<u8> = PLANE_FORMATS
            .iter()
            .flat_map(|format| format.to_le_bytes())
            .collect();
        uaccess::copy_to_user(process.space(), plane.format_type_ptr, &bytes)
            .map_err(|_| Errno::EFAULT)?;
    }
    plane.count_format_types = PLANE_FORMATS.len() as u32;
    write_arg(process, arg, &plane)
}

/// What an object id names, with its `DRM_MODE_OBJECT_*` type.
fn object_type(file: &CardFile, id: u32) -> Option<u32> {
    if id == TYPE_PROPERTY_ID || id == EDID_PROPERTY_ID {
        return Some(drm::MODE_OBJECT_PROPERTY);
    }
    let card = &file.card;
    if head_of_blob(card, id).is_some() {
        return Some(drm::MODE_OBJECT_BLOB);
    }
    for (kind, object) in [
        (CRTC, drm::MODE_OBJECT_CRTC),
        (ENCODER, drm::MODE_OBJECT_ENCODER),
        (CONNECTOR, drm::MODE_OBJECT_CONNECTOR),
        (PLANE, drm::MODE_OBJECT_PLANE),
    ] {
        if head_of(card, id, kind).is_some() {
            return Some(object);
        }
    }
    file.state
        .lock()
        .framebuffers
        .iter()
        .any(|fb| fb.id == id)
        .then_some(drm::MODE_OBJECT_FB)
}

/// `OBJ_GETPROPERTIES`, as `drm_mode_obj_get_properties_ioctl` answers a
/// client without atomic. An id of another type than the one asked for is
/// `ENOENT`, as `__drm_mode_object_find` has it. The plane has its `type`;
/// the CRTC has none, since Linux attaches CRTC properties only to atomic
/// drivers. The connector has `EDID` when `drm.edid_firmware` gave it one
/// (`docs/DISPLAY.md` §7), whose value is its blob's id while a display is
/// there and 0 while none is, and nothing else, where Linux has `DPMS`,
/// `link-status`, `non-desktop` and `TILE` too (§2.3). Encoders,
/// framebuffers, properties and blobs carry no property list, which is
/// `EINVAL`.
fn obj_get_properties(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut request: ObjGetProperties = read_arg(process, arg)?;
    let found = object_type(file, request.obj_id)
        .filter(|&kind| request.obj_type == drm::MODE_OBJECT_ANY || request.obj_type == kind)
        .ok_or(Errno::ENOENT)?;
    let edid;
    let properties: &[(u32, u64)] = match found {
        drm::MODE_OBJECT_PLANE => &[(TYPE_PROPERTY_ID, drm::PLANE_TYPE_PRIMARY)],
        drm::MODE_OBJECT_CONNECTOR => {
            edid = head_of(&file.card, request.obj_id, CONNECTOR)
                .and_then(|head| edid_property(&file.card, head));
            edid.as_slice()
        }
        drm::MODE_OBJECT_CRTC => &[],
        _ => return Err(Errno::EINVAL),
    };
    write_prefix(
        process,
        request.props_ptr,
        request.count_props,
        properties,
        |(id, _)| id.to_le_bytes().to_vec(),
    )?;
    write_prefix(
        process,
        request.prop_values_ptr,
        request.count_props,
        properties,
        |(_, value)| value.to_le_bytes().to_vec(),
    )?;
    request.count_props = u32::try_from(properties.len()).unwrap_or(u32::MAX);
    write_arg(process, arg, &request)
}

/// `head`'s connector's `EDID` property and its value, if the connector has
/// one: the blob's id while a display is there, 0 while none is.
fn edid_property(card: &Card, head: usize) -> Option<(u32, u64)> {
    if !card.carries_edid(head) {
        return None;
    }
    let blob = card.edid(head).map_or(0, |_| u64::from(edid_blob_id(head)));
    Some((EDID_PROPERTY_ID, blob))
}

/// `GETPROPERTY`, as `drm_mode_getproperty_ioctl` answers it. The plane
/// `type` property is an immutable enum whose values are the three plane
/// types, each named, copied as far as the caller's arrays have room; the
/// connector `EDID` property is an immutable blob, which has neither values
/// nor enum records (`ferrix_displayctl::edid::describe_property`).
fn get_property(process: &Process, arg: u64) -> Result<usize, Errno> {
    let mut property: GetProperty = read_arg(process, arg)?;
    if property.prop_id == EDID_PROPERTY_ID {
        ferrix_displayctl::edid::describe_property(&mut property);
        return write_arg(process, arg, &property);
    }
    if property.prop_id != TYPE_PROPERTY_ID {
        return Err(Errno::ENOENT);
    }
    let mut name = [0u8; drm::PROP_NAME_LEN];
    if let Some(slot) = name.get_mut(..4) {
        slot.copy_from_slice(b"type");
    }
    property.name = name;
    property.flags = drm::MODE_PROP_ENUM | drm::MODE_PROP_IMMUTABLE;
    write_prefix(
        process,
        property.values_ptr,
        property.count_values,
        &PLANE_TYPES,
        |(value, _)| value.to_le_bytes().to_vec(),
    )?;
    write_prefix(
        process,
        property.enum_blob_ptr,
        property.count_enum_blobs,
        &PLANE_TYPES,
        |(value, label)| {
            let mut record = PropertyEnum {
                value,
                name: [0; drm::PROP_NAME_LEN],
            };
            if let Some(slot) = record.name.get_mut(..label.len()) {
                slot.copy_from_slice(label);
            }
            let mut bytes = vec![0u8; PropertyEnum::SIZE];
            let _ = record.write(&mut bytes);
            bytes
        },
    )?;
    property.count_values = PLANE_TYPES.len() as u32;
    property.count_enum_blobs = PLANE_TYPES.len() as u32;
    write_arg(process, arg, &property)
}

/// `GETPROPBLOB`, as `drm_mode_getblob_ioctl` answers it: an id that names
/// no blob is `ENOENT`; the bytes are copied only into room of exactly the
/// blob's length, and the length is written back either way, so a caller
/// asks once with none to learn it (`ferrix_displayctl::edid::answer_blob`).
fn get_blob(process: &Process, card: &Card, arg: u64) -> Result<usize, Errno> {
    let mut request: GetBlob = read_arg(process, arg)?;
    let head = head_of_blob(card, request.blob_id).ok_or(Errno::ENOENT)?;
    let bytes = card.edid(head).ok_or(Errno::ENOENT)?;
    if ferrix_displayctl::edid::answer_blob(&mut request, bytes.len()) {
        uaccess::copy_to_user(process.space(), request.data, &bytes).map_err(|_| Errno::EFAULT)?;
    }
    write_arg(process, arg, &request)
}

fn get_cap(process: &Process, card: &Card, arg: u64) -> Result<usize, Errno> {
    let mut cap: GetCap = read_arg(process, arg)?;
    cap.value = match cap.capability {
        // Only a card whose driver said it has a cursor plane has a size
        // for one: a program that is told none draws its pointer itself.
        drm::CAP_CURSOR_WIDTH | drm::CAP_CURSOR_HEIGHT if card.has_cursor() => {
            u64::from(CURSOR_SIZE)
        }
        drm::CAP_DUMB_BUFFER | drm::CAP_TIMESTAMP_MONOTONIC | drm::CAP_CRTC_IN_VBLANK_EVENT => 1,
        drm::CAP_DUMB_PREFERRED_DEPTH => 24,
        drm::CAP_DUMB_PREFER_SHADOW => 0,
        _ => return Err(Errno::EINVAL),
    };
    write_arg(process, arg, &cap)
}

fn get_resources(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut resources: CardRes = read_arg(process, arg)?;
    let framebuffers: Vec<u32> = file
        .state
        .lock()
        .framebuffers
        .iter()
        .map(|fb| fb.id)
        .collect();
    write_ids(
        process,
        resources.fb_id_ptr,
        resources.count_fbs,
        &framebuffers,
    )?;
    let heads = heads(&file.card);
    let ids = |kind: u32| -> Vec<u32> { (0..heads).map(|head| object_id(head, kind)).collect() };
    write_ids(
        process,
        resources.crtc_id_ptr,
        resources.count_crtcs,
        &ids(CRTC),
    )?;
    write_ids(
        process,
        resources.connector_id_ptr,
        resources.count_connectors,
        &ids(CONNECTOR),
    )?;
    write_ids(
        process,
        resources.encoder_id_ptr,
        resources.count_encoders,
        &ids(ENCODER),
    )?;
    let count = u32::try_from(heads).unwrap_or(u32::MAX);
    resources.count_fbs = u32::try_from(framebuffers.len()).unwrap_or(u32::MAX);
    resources.count_crtcs = count;
    resources.count_connectors = count;
    resources.count_encoders = count;
    resources.min_width = 1;
    resources.min_height = 1;
    resources.max_width = MAX_DIMENSION;
    resources.max_height = MAX_DIMENSION;
    write_arg(process, arg, &resources)
}

fn get_encoder(process: &Process, card: &Card, arg: u64) -> Result<usize, Errno> {
    let mut encoder: GetEncoder = read_arg(process, arg)?;
    let head = head_of(card, encoder.encoder_id, ENCODER).ok_or(Errno::ENOENT)?;
    encoder.encoder_type = drm::ENCODER_VIRTUAL;
    encoder.crtc_id = object_id(head, CRTC);
    // One CRTC drives one connector here, so the mask has the one bit: a
    // head's CRTC is at its own index in `GETRESOURCES`' list.
    encoder.possible_crtcs = crtc_mask(head);
    encoder.possible_clones = 0;
    write_arg(process, arg, &encoder)
}

/// The `possible_crtcs` bit of `head`'s CRTC, which counts in the order
/// `GETRESOURCES` lists them.
fn crtc_mask(head: usize) -> u32 {
    u32::try_from(head)
        .ok()
        .and_then(|bit| 1u32.checked_shl(bit))
        .unwrap_or(0)
}

fn get_crtc(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut crtc: Crtc = read_arg(process, arg)?;
    let head = head_of(&file.card, crtc.crtc_id, CRTC).ok_or(Errno::ENOENT)?;
    let shown = file.state.lock().shown.get(&head).copied();
    crtc.fb_id = shown.map_or(0, |(id, _)| id);
    crtc.mode_valid = u32::from(shown.is_some());
    crtc.mode = shown.map_or(ModeInfo::ZERO, |(_, mode)| mode);
    crtc.x = 0;
    crtc.y = 0;
    crtc.gamma_size = 0;
    write_arg(process, arg, &crtc)
}

fn map_dumb(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut map: MapDumb = read_arg(process, arg)?;
    map.offset = file
        .state
        .lock()
        .dumbs
        .iter()
        .find(|dumb| dumb.handle == map.handle && !dumb.destroyed && dumb.mapped)
        .map(|dumb| u64::from(dumb.handle) << MAP_SHIFT)
        .ok_or(Errno::ENOENT)?;
    write_arg(process, arg, &map)
}

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`: take a buffer object the render node
/// exported, and give it a handle on this card.
///
/// This is where a frame drawn on the GPU meets the screen. The descriptor
/// names a resource the device holds already, so what the card is given is
/// a buffer with no range: `ATTACH_OBJ` rather than ATTACH, nothing pinned,
/// and a flush that sends no pixels (`docs/GPU.md` §3.5 piece 6). The rest
/// of the card -- `ADDFB2`, `SETCRTC`, a page flip, `DIRTYFB` -- knows
/// nothing about it and works unchanged.
///
/// The handle holds the object alive, so a program may close the render
/// node's handle, or the render node itself, while the screen shows what it
/// drew.
///
/// A descriptor that is not an exported object is `EINVAL`, as Linux
/// answers one that is not a dmabuf.
fn prime_fd_to_handle(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut prime: PrimeHandle = read_arg(process, arg)?;
    let descriptor = crate::syscall::fd::file(process, crate::syscall::fd::arg(prime.fd as u64))
        .map_err(|_| Errno::EBADF)?;
    let exported =
        crate::interfaces::render::node::exported(descriptor.io()).ok_or(Errno::EINVAL)?;
    let object = exported.object();
    let (id, width, height, stride) = object.shown();
    // One handle an object, as Linux gives one: a second import of the same
    // buffer answers the handle the first was given, and the object is not
    // attached twice.
    if let Some((handle, _)) = file
        .state
        .lock()
        .imports
        .iter()
        .find(|(_, held)| Arc::ptr_eq(held, &object))
    {
        prime.handle = *handle;
        return write_arg(process, arg, &prime);
    }
    let (buffer, status) = file
        .card
        .attach_object(id, width, height, stride)
        .map_err(card_error)?;
    status_error(status)?;
    let handle = {
        let mut state = file.state.lock();
        let handle = state.next_handle;
        match handle.checked_add(1) {
            Some(next) => {
                state.next_handle = next;
                state.dumbs.push(Dumb {
                    handle,
                    buffer,
                    mapped: false,
                    width,
                    height,
                    pitch: stride,
                    destroyed: false,
                });
                state.imports.push((handle, object));
                Some(handle)
            }
            None => None,
        }
    };
    let Some(handle) = handle else {
        file.card.let_go(&[buffer]);
        return Err(Errno::ENOMEM);
    };
    prime.handle = handle;
    write_arg(process, arg, &prime)
}

fn add_fb(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut command: FbCmd = read_arg(process, arg)?;
    if command.bpp != 32 || command.depth != 24 {
        return Err(Errno::EINVAL);
    }
    command.fb_id = add_framebuffer(
        file,
        command.handle,
        command.width,
        command.height,
        command.pitch,
    )?;
    write_arg(process, arg, &command)
}

fn add_fb2(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut command: FbCmd2 = read_arg(process, arg)?;
    let [handle, extra_handles @ ..] = command.handles;
    if command.pixel_format != drm::FORMAT_XRGB8888
        || command.flags != 0
        || command.offsets != [0; 4]
        || extra_handles != [0; 3]
    {
        return Err(Errno::EINVAL);
    }
    let [pitch, ..] = command.pitches;
    command.fb_id = add_framebuffer(file, handle, command.width, command.height, pitch)?;
    write_arg(process, arg, &command)
}

fn page_flip(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let flip: CrtcPageFlip = read_arg(process, arg)?;
    if flip.reserved != 0 || flip.flags & !drm::PAGE_FLIP_EVENT != 0 {
        return Err(Errno::EINVAL);
    }
    let head = head_of(&file.card, flip.crtc_id, CRTC).ok_or(Errno::EINVAL)?;
    let mode = file
        .state
        .lock()
        .shown
        .get(&head)
        .map(|(_, mode)| *mode)
        .ok_or(Errno::EINVAL)?;
    show(file, head, flip.fb_id, mode, false)?;
    if flip.flags & drm::PAGE_FLIP_EVENT != 0 {
        queue_event(file, head, flip.user_data);
    }
    Ok(0)
}

fn dirty_fb(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let dirty: FbDirtyCmd = read_arg(process, arg)?;
    let framebuffer = find_framebuffer(file, dirty.fb_id)?;
    let rect = dirty_rect(process, &dirty, &framebuffer)?;
    file.card
        .flush(framebuffer.buffer, rect)
        .map_err(card_error)
        .and_then(status_error)
        .map(|()| 0)
}

/// `MODE_CURSOR2`, and `MODE_CURSOR` made one with no hotspot, as Linux's
/// `drm_mode_cursor_ioctl` makes it: the head's cursor plane.
///
/// `MOVE` puts the image's top-left corner at (`x`, `y`) and waits for
/// nothing. `BO` shows the dumb buffer `handle` names -- 64 × 64, which is
/// the one size the host shows, or none for handle 0 -- with its hotspot,
/// and returns once the device shows it. virtio-gpu's host has copied the
/// pixels by then and the buffer may be drawn into again; the DK1's LTDC
/// shows its second layer straight from the buffer, so a program draws the
/// next image into another one, as Linux's compositors do. A call with both
/// sets the place first and shows the image there, as
/// `drm_mode_cursor_universal` does.
fn cursor(file: &CardFile, request: &ModeCursor2) -> Result<usize, Errno> {
    if request.flags == 0 || request.flags & !drm::MODE_CURSOR_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    // What Linux answers for a CRTC with no cursor.
    if !file.card.has_cursor() {
        return Err(Errno::ENXIO);
    }
    let head = head_of(&file.card, request.crtc_id, CRTC).ok_or(Errno::ENOENT)?;
    let scanout = u32::try_from(head).map_err(|_| Errno::EINVAL)?;
    let at = {
        let mut state = file.state.lock();
        if request.flags & drm::MODE_CURSOR_MOVE != 0 {
            let _ = state.cursors.insert(head, (request.x, request.y));
        }
        state.cursors.get(&head).copied().unwrap_or_default()
    };
    if request.flags & drm::MODE_CURSOR_BO == 0 {
        return file
            .card
            .move_cursor(scanout, at)
            .map_err(card_error)
            .map(|()| 0);
    }
    let buffer = match request.handle {
        0 => 0,
        handle => {
            let dumb = file
                .state
                .lock()
                .dumbs
                .iter()
                .find(|dumb| dumb.handle == handle && !dumb.destroyed)
                .copied()
                .ok_or(Errno::ENOENT)?;
            let size = (CURSOR_SIZE, CURSOR_SIZE);
            if (dumb.width, dumb.height) != size || (request.width, request.height) != size {
                return Err(Errno::EINVAL);
            }
            dumb.buffer
        }
    };
    let hot = (
        u32::try_from(request.hot_x).map_err(|_| Errno::EINVAL)?,
        u32::try_from(request.hot_y).map_err(|_| Errno::EINVAL)?,
    );
    file.card
        .cursor(scanout, buffer, hot, at)
        .map_err(card_error)
        .and_then(status_error)
        .map(|()| 0)
}

fn version(process: &Process, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; Version::size(NATIVE)];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut version = Version::read(NATIVE, &bytes).ok_or(Errno::EFAULT)?;
    let give = |at: u64, capacity: u64, text: &[u8]| -> Result<u64, Errno> {
        let len = text.len() as u64;
        if at != 0 && capacity > 0 {
            let copied = text.get(..capacity.min(len) as usize).unwrap_or(text);
            uaccess::copy_to_user(process.space(), at, copied).map_err(|_| Errno::EFAULT)?;
        }
        Ok(len)
    };
    version.version_major = 0;
    version.version_minor = 1;
    version.version_patchlevel = 0;
    version.name_len = give(version.name, version.name_len, b"virtio_gpu")?;
    version.date_len = give(version.date, version.date_len, b"0")?;
    version.desc_len = give(version.desc, version.desc_len, b"virtio GPU")?;
    version.write(NATIVE, &mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// The type a connector has when its driver names none: a board's HDMI
/// output's, or a virtual card's.
pub(crate) const fn connector_type(hdmi: bool) -> u32 {
    if hdmi {
        drm::CONNECTOR_HDMIA
    } else {
        drm::CONNECTOR_VIRTUAL
    }
}

/// The modes head `head` of `card` lists: none while nothing is connected;
/// on a board's HDMI output the modes its driver can make a pixel clock for,
/// the one it chose first, or the one it runs where it said no more; the
/// preferred size at its display's refresh and at the offered ones, and the
/// standard sizes, on a virtual card, which shows any.
fn modes_of(card: &Card, head: usize) -> Vec<ModeInfo> {
    card.modes()
        .get(head)
        .copied()
        .filter(|mode| mode.enabled)
        .map(|mode| {
            if !card.timings().is_empty() && head == 0 {
                card.timings()
                    .iter()
                    .enumerate()
                    .map(|(index, timing)| mode_of(timing, index == 0))
                    .collect()
            } else if card.hdmi {
                vec![mode_for(mode.width, mode.height)]
            } else {
                listed_modes(mode.width, mode.height, mode.refresh_mhz)
            }
        })
        .unwrap_or_default()
}

/// One connector, as sysfs shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectorView {
    /// `DRM_MODE_CONNECTOR_*`.
    pub(crate) kind: u32,
    /// Its number among the card's connectors of that type, from one.
    pub(crate) kind_index: u32,
    /// Whether a display is there.
    pub(crate) connected: bool,
    /// What `modes` lists, preferred first, as `WxH`.
    pub(crate) modes: Vec<(u32, u32)>,
}

/// Every connector of `card`, as the connector ioctl reports each: the same
/// heads, types, numbers and modes, from the same functions.
pub(crate) fn connectors(card: &Card) -> Vec<ConnectorView> {
    (0..heads(card))
        .map(|head| {
            let modes = modes_of(card, head);
            ConnectorView {
                kind: card.connector_type(head),
                kind_index: u32::try_from(head).unwrap_or(0).saturating_add(1),
                connected: !modes.is_empty(),
                modes: modes
                    .iter()
                    .map(|mode| (u32::from(mode.hdisplay), u32::from(mode.vdisplay)))
                    .collect(),
            }
        })
        .collect()
}

fn connector(process: &Process, card: &Card, arg: u64) -> Result<usize, Errno> {
    let mut connector: GetConnector = read_arg(process, arg)?;
    let head = head_of(card, connector.connector_id, CONNECTOR).ok_or(Errno::ENOENT)?;
    let preferred = card.modes().get(head).copied().filter(|mode| mode.enabled);
    let modes = modes_of(card, head);
    if connector.modes_ptr != 0 && connector.count_modes as usize >= modes.len() {
        let mut bytes = vec![0u8; modes.len() * ModeInfo::SIZE];
        for (index, mode) in modes.iter().enumerate() {
            let slot = bytes
                .get_mut(index * ModeInfo::SIZE..)
                .ok_or(Errno::EFAULT)?;
            mode.write(slot).ok_or(Errno::EFAULT)?;
        }
        uaccess::copy_to_user(process.space(), connector.modes_ptr, &bytes)
            .map_err(|_| Errno::EFAULT)?;
    }
    write_ids(
        process,
        connector.encoders_ptr,
        connector.count_encoders,
        &[object_id(head, ENCODER)],
    )?;
    // The properties as `OBJ_GETPROPERTIES` lists them, copied while the
    // caller's count has room, as `drm_mode_getconnector` copies them.
    let properties = edid_property(card, head);
    let properties = properties.as_slice();
    write_prefix(
        process,
        connector.props_ptr,
        connector.count_props,
        properties,
        |(id, _)| id.to_le_bytes().to_vec(),
    )?;
    write_prefix(
        process,
        connector.prop_values_ptr,
        connector.count_props,
        properties,
        |(_, value)| value.to_le_bytes().to_vec(),
    )?;
    connector.count_modes = u32::try_from(modes.len()).unwrap_or(0);
    connector.count_props = u32::try_from(properties.len()).unwrap_or(0);
    connector.count_encoders = 1;
    connector.encoder_id = object_id(head, ENCODER);
    connector.connector_type = card.connector_type(head);
    // Linux numbers connectors of one type from one upwards, and a program
    // prints the name as `Virtual-1`, `Virtual-2`.
    connector.connector_type_id = u32::try_from(head).unwrap_or(0).saturating_add(1);
    connector.connection = if preferred.is_some() {
        drm::CONNECTION_CONNECTED
    } else {
        drm::CONNECTION_DISCONNECTED
    };
    connector.mm_width = 0;
    connector.mm_height = 0;
    connector.subpixel = drm::SUBPIXEL_UNKNOWN;
    connector.pad = 0;
    write_arg(process, arg, &connector)
}

fn create_dumb(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let mut create: CreateDumb = read_arg(process, arg)?;
    if create.bpp != 32
        || create.flags != 0
        || !(1..=MAX_DIMENSION).contains(&create.width)
        || !(1..=MAX_DIMENSION).contains(&create.height)
    {
        return Err(Errno::EINVAL);
    }
    let pitch = create.width * 4;
    let (buffer, pages, status) = file
        .card
        .attach(file.id, create.width, create.height, pitch)
        .map_err(card_error)?;
    status_error(status)?;
    let size = (u64::from(pitch) * u64::from(create.height)).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let handle = {
        let mut state = file.state.lock();
        let handle = state.next_handle;
        match handle.checked_add(1) {
            Some(next) => {
                state.next_handle = next;
                state.pages.push((handle, pages));
                state.dumbs.push(Dumb {
                    handle,
                    buffer,
                    mapped: true,
                    width: create.width,
                    height: create.height,
                    pitch,
                    destroyed: false,
                });
                Some(handle)
            }
            None => None,
        }
    };
    let Some(handle) = handle else {
        file.card.let_go(&[buffer]);
        return Err(Errno::ENOMEM);
    };
    create.handle = handle;
    create.pitch = pitch;
    create.size = size;
    write_arg(process, arg, &create)
}

/// Take the dumb buffer at `at` of the open's list away, and its VMO with
/// it: the core's id, for the caller to let go of. A mapping of it keeps the
/// VMO, and only it, alive.
fn forget(state: &mut OpenState, at: usize) -> u32 {
    let dumb = state.dumbs.remove(at);
    state.pages.retain(|(held, _)| *held != dumb.handle);
    dumb.buffer
}

/// Take `handle` away. The buffer goes now, or with the last framebuffer
/// that refers to it.
fn destroy_dumb(file: &CardFile, handle: u32) -> Result<(), Errno> {
    let gone = {
        let mut state = file.state.lock();
        let referred = state.framebuffers.iter().any(|fb| fb.handle == handle);
        let at = state
            .dumbs
            .iter()
            .position(|dumb| dumb.handle == handle && !dumb.destroyed)
            .ok_or(Errno::ENOENT)?;
        if referred {
            if let Some(dumb) = state.dumbs.get_mut(at) {
                dumb.destroyed = true;
            }
            None
        } else {
            Some(forget(&mut state, at))
        }
    };
    if let Some(buffer) = gone {
        // The object an import held goes with the handle, unless the
        // program that drew it still has one of its own.
        file.state
            .lock()
            .imports
            .retain(|(held, _)| *held != handle);
        file.card.let_go(&[buffer]);
    }
    Ok(())
}

fn add_framebuffer(
    file: &CardFile,
    handle: u32,
    width: u32,
    height: u32,
    pitch: u32,
) -> Result<u32, Errno> {
    let mut state = file.state.lock();
    let dumb = state
        .dumbs
        .iter()
        .find(|dumb| dumb.handle == handle && !dumb.destroyed)
        .copied()
        .ok_or(Errno::ENOENT)?;
    if width == 0
        || height == 0
        || width > dumb.width
        || height > dumb.height
        || pitch != dumb.pitch
    {
        return Err(Errno::EINVAL);
    }
    if state.framebuffers.len() >= MAX_FRAMEBUFFERS {
        return Err(Errno::ENOSPC);
    }
    let id = state.next_framebuffer;
    state.next_framebuffer = id.checked_add(1).ok_or(Errno::ENOMEM)?;
    state.framebuffers.push(Framebuffer {
        id,
        handle,
        buffer: dumb.buffer,
        width,
        height,
    });
    Ok(id)
}

fn find_framebuffer(file: &CardFile, id: u32) -> Result<Framebuffer, Errno> {
    file.state
        .lock()
        .framebuffers
        .iter()
        .find(|fb| fb.id == id)
        .copied()
        .ok_or(Errno::ENOENT)
}

fn remove_framebuffer(file: &CardFile, id: u32) -> Result<(), Errno> {
    let framebuffer = find_framebuffer(file, id)?;
    // A framebuffer may be on more than one head; every head showing it
    // goes off, as Linux's `drm_framebuffer_remove` turns off every CRTC
    // and plane that refers to one.
    let showing: Vec<usize> = file
        .state
        .lock()
        .shown
        .iter()
        .filter(|(_, (shown, _))| *shown == id)
        .map(|(head, _)| *head)
        .collect();
    for head in showing {
        file.card
            .scanout(scanout_of(head), 0, Rect::default())
            .map_err(card_error)?;
        let _ = file.state.lock().shown.remove(&head);
    }
    let orphaned = {
        let mut state = file.state.lock();
        state.framebuffers.retain(|fb| fb.id != framebuffer.id);
        let still = state
            .framebuffers
            .iter()
            .any(|fb| fb.handle == framebuffer.handle);
        let at = state
            .dumbs
            .iter()
            .position(|dumb| dumb.handle == framebuffer.handle && dumb.destroyed)
            .filter(|_| !still);
        at.map(|at| forget(&mut state, at))
    };
    if let Some(buffer) = orphaned {
        file.card.let_go(&[buffer]);
    }
    Ok(())
}

/// Put framebuffer `id` on the CRTC at `mode`, setting the scanout again
/// even if it is shown when `reset`, and wait for the device to have it.
fn show(file: &CardFile, head: usize, id: u32, mode: ModeInfo, reset: bool) -> Result<(), Errno> {
    let framebuffer = find_framebuffer(file, id)?;
    let (width, height) = (u32::from(mode.hdisplay), u32::from(mode.vdisplay));
    if width == 0 || height == 0 || width > framebuffer.width || height > framebuffer.height {
        return Err(Errno::ENOSPC);
    }
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let already = file
        .state
        .lock()
        .shown
        .get(&head)
        .is_some_and(|(shown, _)| *shown == id);
    if reset || !already {
        file.card
            .scanout(scanout_of(head), framebuffer.buffer, rect)
            .map_err(card_error)?;
        // The device shows it from here, whether or not the flush goes.
        let _ = file.state.lock().shown.insert(head, (id, mode));
    }
    let status = file
        .card
        .flush(framebuffer.buffer, rect)
        .map_err(card_error)?;
    status_error(status)
}

/// `SETCRTC` as Linux answers it: no mode and no connectors turns the CRTC
/// off; a mode takes a framebuffer, `-1` meaning the one shown, and the one
/// connector.
fn set_crtc(process: &Process, file: &CardFile, arg: u64) -> Result<usize, Errno> {
    let crtc: Crtc = read_arg(process, arg)?;
    let head = head_of(&file.card, crtc.crtc_id, CRTC).ok_or(Errno::ENOENT)?;
    if crtc.mode_valid == 0 {
        if crtc.count_connectors != 0 {
            return Err(Errno::EINVAL);
        }
        file.card
            .scanout(scanout_of(head), 0, Rect::default())
            .map_err(card_error)?;
        let _ = file.state.lock().shown.remove(&head);
        return Ok(0);
    }
    if crtc.x != 0 || crtc.y != 0 || crtc.count_connectors != 1 {
        return Err(Errno::EINVAL);
    }
    let mut connector = [0u8; 4];
    uaccess::copy_from_user(process.space(), crtc.set_connectors_ptr, &mut connector)
        .map_err(|_| Errno::EFAULT)?;
    // The connector has to be the one this CRTC drives: nothing here can
    // route a CRTC to another head's connector.
    if u32::from_le_bytes(connector) != object_id(head, CONNECTOR) {
        return Err(Errno::EINVAL);
    }
    // A card that runs only the modes it listed is not asked for a size it
    // did not list: its driver would leave the screen dark, which Linux's
    // `mode_valid` refuses in the same place.
    let timings = file.card.timings();
    let size = (crtc.mode.hdisplay, crtc.mode.vdisplay);
    if head == 0
        && !timings.is_empty()
        && !timings
            .iter()
            .any(|timing| (timing.hdisplay, timing.vdisplay) == size)
    {
        return Err(Errno::EINVAL);
    }
    let fb = if crtc.fb_id == u32::MAX {
        file.state
            .lock()
            .shown
            .get(&head)
            .map(|(id, _)| *id)
            .ok_or(Errno::EINVAL)?
    } else {
        crtc.fb_id
    };
    // A new mode on the framebuffer already shown is set again.
    show(file, head, fb, crtc.mode, true)?;
    Ok(0)
}

/// The scanout number of a head, which is its index: the card's connectors
/// are its scanouts in the order the driver reported them.
fn scanout_of(head: usize) -> u32 {
    u32::try_from(head).unwrap_or(0)
}

fn dirty_rect(
    process: &Process,
    dirty: &FbDirtyCmd,
    framebuffer: &Framebuffer,
) -> Result<Rect, Errno> {
    let whole = Rect {
        x: 0,
        y: 0,
        width: framebuffer.width,
        height: framebuffer.height,
    };
    if dirty.num_clips == 0 || dirty.clips_ptr == 0 {
        return Ok(whole);
    }
    if dirty.num_clips > drm::FB_DIRTY_MAX_CLIPS {
        return Err(Errno::EINVAL);
    }
    let mut bytes = vec![0u8; dirty.num_clips as usize * ClipRect::SIZE];
    uaccess::copy_from_user(process.space(), dirty.clips_ptr, &mut bytes)
        .map_err(|_| Errno::EFAULT)?;
    let (mut left, mut top, mut right, mut bottom) = (u32::MAX, u32::MAX, 0, 0);
    for clip in bytes
        .chunks_exact(ClipRect::SIZE)
        .filter_map(ClipRect::read)
    {
        left = left.min(u32::from(clip.x1));
        top = top.min(u32::from(clip.y1));
        right = right.max(u32::from(clip.x2)).min(framebuffer.width);
        bottom = bottom.max(u32::from(clip.y2)).min(framebuffer.height);
    }
    if left >= right || top >= bottom {
        return Ok(whole);
    }
    Ok(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn queue_event(file: &CardFile, head: usize, user_data: u64) {
    let nanos = timer::now_nanos();
    let mut state = file.state.lock();
    state.sequence = state.sequence.wrapping_add(1);
    let event = EventVblank {
        base: Event {
            r#type: drm::EVENT_FLIP_COMPLETE,
            length: EventVblank::SIZE as u32,
        },
        user_data,
        tv_sec: u32::try_from(nanos / 1_000_000_000).unwrap_or(u32::MAX),
        tv_usec: u32::try_from(nanos % 1_000_000_000 / 1000).unwrap_or(0),
        sequence: state.sequence,
        crtc_id: object_id(head, CRTC),
    };
    let mut bytes = [0u8; 32];
    if event.write(&mut bytes).is_some() && state.events.len() < MAX_EVENTS {
        state.events.push_back(bytes);
    }
    drop(state);
    file.readable.wake_all();
}
