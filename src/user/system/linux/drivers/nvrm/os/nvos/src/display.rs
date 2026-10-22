//! The display bridge: nvrm's end of the kernel's display core
//! (`docs/NVIDIA.md` §4.5, N6; `docs/DISPLAY.md`).
//!
//! nvrm is a display driver as virtio-gpu and the STM32 LTDC are, except
//! that the 3060's display engine reads only its own video memory: a frame
//! is shown by copying it there with the processor. So its HELLO says it
//! copies, and each ATTACH hands it the buffer's own VMO with `MAP`, which
//! [`nvos_display_next`] maps read-only before C hears of the buffer.
//! NVKMS, through `os/glue/kms.c`, does the rest: the C side takes each
//! message from [`nvos_display_next`] and answers through
//! [`nvos_display_attached`], [`nvos_display_flipped`] and
//! [`nvos_display_detached`]. The bridge keeps each buffer's mapping and
//! takes it away itself, at a refused ATTACH or at DETACH, so C only reads
//! the pixels it is pointed at.
//!
//! ATTACH is checked here against its VMO's size before C sees it, so a
//! buffer C is told of lies inside its mapping.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use ferrix_displayctl::message::{
    EDID_BYTES, Edid, Hello, Listed, MAX_BYTES, MAX_LISTED_MODES, MAX_SCANOUTS, MODES_A_PAGE,
    Message, ModePage, PORT_RIGHTS, Rect, ScanoutMode, Status, Timing, Timings, VERSION,
};
use ferrix_linux_abi::drm;
use ferrix_native::channel::{Channel, ReadError};
use ferrix_native::handle::{Object, OwnedHandle};
use ferrix_native::pending::Protection;
use ferrix_native::port;
use ferrix_native::vmo::Vmo;
use ferrix_native::{Deadline, Error, Handle, Requested};
use ferrix_native_abi::nr;
use ferrix_native_abi::signals::Signals;

use crate::device;
use crate::futex::{self, Kernel};
use crate::log::say;
use crate::status::{self, NvStatus};

/// The control channel's handle, 0 before [`nvos_display_start`].
static CONTROL: AtomicU32 = AtomicU32::new(0);
/// The driver's port, which HELLO hands the core a share of; kept open.
static PORT: AtomicU32 = AtomicU32::new(0);

/// How many buffers' mappings are kept: `kms.c`'s `KMS_BUFFERS`.
const MAPPED: usize = 64;

/// One buffer's read-only mapping: its id (0 for a free slot), where, and
/// how long. Only C's one serving thread touches these, through the calls
/// below; atomics only so that a static may hold them.
struct Mapped {
    buffer: AtomicU32,
    at: AtomicUsize,
    length: AtomicUsize,
}

static MAPPINGS: [Mapped; MAPPED] = [const {
    Mapped {
        buffer: AtomicU32::new(0),
        at: AtomicUsize::new(0),
        length: AtomicUsize::new(0),
    }
}; MAPPED];

/// Remember `buffer`'s mapping. `false` when every slot is taken.
fn remember(buffer: u32, at: usize, length: usize) -> bool {
    let Some(slot) = MAPPINGS
        .iter()
        .find(|slot| slot.buffer.load(Ordering::Acquire) == 0)
    else {
        return false;
    };
    slot.at.store(at, Ordering::Relaxed);
    slot.length.store(length, Ordering::Relaxed);
    slot.buffer.store(buffer, Ordering::Release);
    true
}

/// Take `buffer`'s mapping away, if there is one. C reads nothing of it
/// after: it has refused the buffer or let go of it.
fn unmap(buffer: u32) {
    if buffer == 0 {
        return;
    }
    let Some(slot) = MAPPINGS
        .iter()
        .find(|slot| slot.buffer.load(Ordering::Acquire) == buffer)
    else {
        return;
    };
    let (at, length) = (
        slot.at.load(Ordering::Relaxed),
        slot.length.load(Ordering::Relaxed),
    );
    slot.buffer.store(0, Ordering::Release);
    // SAFETY: one buffer's whole mapping, made by `map_buffer`, which
    // nothing reads after (above).
    if unsafe { crate::libc::munmap(core::ptr::without_provenance_mut(at), length) } != 0 {
        say!("display: buffer {buffer}'s mapping at {at:#x} was not taken away; it stays");
    }
}

/// One timing as C hands it (`struct nvos_display_timing`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CTiming {
    /// Pixel clock, kHz.
    pub clock_khz: u32,
    /// DRM's spans.
    pub hdisplay: u16,
    /// .
    pub hsync_start: u16,
    /// .
    pub hsync_end: u16,
    /// .
    pub htotal: u16,
    /// .
    pub vdisplay: u16,
    /// .
    pub vsync_start: u16,
    /// .
    pub vsync_end: u16,
    /// .
    pub vtotal: u16,
    /// Bit 0 hsync positive, bit 1 vsync positive.
    pub flags: u32,
}

/// What C says of its one scanout (`struct nvos_display_hello`): the mode
/// it runs and every mode it can.
#[repr(C)]
#[derive(Debug)]
pub struct CHello {
    /// The running mode's size.
    pub width: u32,
    /// .
    pub height: u32,
    /// Its refresh, millihertz.
    pub refresh_mhz: u32,
    /// How many of `timings` are set, the running one first.
    pub count: u32,
    /// Every mode NVKMS validated for the display (displayctl v11's
    /// `MODE_LIST`, `docs/DISPLAY.md` §9.4), the running one first.
    pub timings: [CTiming; MAX_LISTED_MODES],
    /// Which of `timings` NVKMS called preferred.
    pub preferred: u32,
    /// The lit display's connector, as NVKMS names its type
    /// (`NvKmsConnectorType`).
    pub connector: u32,
    /// How many of `edid` are the display's EDID; 0 for none.
    pub edid_bytes: u32,
    /// The display's own EDID, as NVKMS read it (`NVKMS_KAPI_EDID_BUFFER_SIZE`).
    pub edid: [u8; NVKMS_EDID_BYTES],
}

/// The most EDID NVKMS reads, and C hands over: `NVKMS_KAPI_EDID_BUFFER_SIZE`.
pub const NVKMS_EDID_BYTES: usize = 2048;

/// DRM's connector type for NVKMS's `NvKmsConnectorType` `nv`, as Linux's
/// nvidia-drm maps it (`nvidia-drm-connector.c`), or 0 for one it calls
/// unknown, which leaves the core its own choice.
const fn drm_connector(nv: u32) -> u32 {
    match nv {
        // DP, and the two kinds that carry it: USB-C and a serializer.
        0 | 7 | 9 => drm::CONNECTOR_DISPLAYPORT,
        1 => drm::CONNECTOR_VGA,
        2 => drm::CONNECTOR_DVII,
        3 => drm::CONNECTOR_DVID,
        5 => drm::CONNECTOR_LVDS,
        6 => drm::CONNECTOR_HDMIA,
        8 => drm::CONNECTOR_DSI,
        // ADC and UNKNOWN.
        _ => 0,
    }
}

/// One message for C (`struct nvos_display_event`).
#[repr(C)]
#[derive(Debug, Default)]
pub struct Event {
    /// One of the `EVENT_*` kinds.
    pub kind: u32,
    /// The buffer it names.
    pub buffer: u32,
    /// ATTACH: where the buffer is mapped, read-only, and its length.
    pub pixels: u64,
    /// .
    pub length: u64,
    /// ATTACH: its size and stride.
    pub width: u32,
    /// .
    pub height: u32,
    /// .
    pub stride: u32,
    /// Unused.
    pub reserved: u32,
    /// FLUSH and CURSOR: the number FLIPPED answers.
    pub sequence: u64,
    /// SCANOUT and FLUSH: the rectangle; MOVE and CURSOR: x and y.
    pub x: i32,
    /// .
    pub y: i32,
    /// .
    pub w: u32,
    /// .
    pub h: u32,
    /// SET_MODE: the mode, as C listed it.
    pub timing: CTiming,
}

/// ATTACH, checked.
pub const EVENT_ATTACH: u32 = 1;
/// SCANOUT.
pub const EVENT_SCANOUT: u32 = 2;
/// FLUSH.
pub const EVENT_FLUSH: u32 = 3;
/// DETACH.
pub const EVENT_DETACH: u32 = 4;
/// CURSOR, which a card without a cursor plane is never sent.
pub const EVENT_CURSOR: u32 = 5;
/// MOVE, likewise.
pub const EVENT_MOVE: u32 = 6;
/// STOP: answer STOPPED and stop showing.
pub const EVENT_STOP: u32 = 7;
/// The core closed its end.
pub const EVENT_CLOSED: u32 = 8;
/// SET_MODE: run `timing`, one of the list `nvos_display_start` sent.
pub const EVENT_MODE: u32 = 9;

/// The control channel, borrowed for one call.
fn control() -> Option<core::mem::ManuallyDrop<Channel<Kernel>>> {
    let handle = CONTROL.load(Ordering::Acquire);
    (handle != 0).then(|| {
        core::mem::ManuallyDrop::new(Channel::from_owned(OwnedHandle::from_raw(
            Kernel,
            Handle(handle),
        )))
    })
}

/// C's timing, in the protocol's terms.
fn timing(c: &CTiming) -> Timing {
    Timing {
        clock_khz: c.clock_khz,
        hdisplay: c.hdisplay,
        hsync_start: c.hsync_start,
        hsync_end: c.hsync_end,
        htotal: c.htotal,
        vdisplay: c.vdisplay,
        vsync_start: c.vsync_start,
        vsync_end: c.vsync_end,
        vtotal: c.vtotal,
        hsync_high: c.flags & 1 != 0,
        vsync_high: c.flags & 2 != 0,
    }
}

/// The HELLO C's description makes.
fn hello(c: &CHello) -> Hello {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    if let Some(first) = modes.first_mut() {
        *first = ScanoutMode {
            width: c.width,
            height: c.height,
            enabled: true,
            refresh_mhz: c.refresh_mhz,
        };
    }
    // HELLO lists only the running mode; the whole list goes as
    // MODE_LIST after READY (`mode_pages`).
    let mut timings = Timings::NONE;
    if c.count > 0
        && let (Some(slot), Some(first)) = (timings.list.first_mut(), c.timings.first())
    {
        *slot = timing(first);
        timings.count = 1;
    }
    Hello {
        version: VERSION,
        scanouts: 1,
        location: device::location(),
        modes,
        virgl: false,
        capsets: 0,
        capset: 0,
        capset_bytes: 0,
        // The compositor draws its own pointer for now.
        cursor: false,
        timings,
        copies: true,
        connectors: {
            let mut connectors = [0; MAX_SCANOUTS];
            connectors[0] = drm_connector(c.connector);
            connectors
        },
    }
}

/// The `MODE_LIST` pages for C's list: every mode NVKMS validated, at most
/// [`MAX_LISTED_MODES`], the one NVKMS prefers marked. A list without a
/// valid preferred index marks the running one.
fn mode_pages(c: &CHello) -> impl Iterator<Item = Message> + '_ {
    let count = usize::try_from(c.count).unwrap_or(0).min(MAX_LISTED_MODES);
    let preferred = usize::try_from(c.preferred)
        .ok()
        .filter(|&at| at < count)
        .unwrap_or(0);
    let total = u32::try_from(count).unwrap_or(0);
    c.timings
        .get(..count)
        .unwrap_or(&[])
        .chunks(MODES_A_PAGE)
        .enumerate()
        .map(move |(page, chunk)| {
            let mut entries = [Listed::default(); MODES_A_PAGE];
            for (index, (into, from)) in entries.iter_mut().zip(chunk).enumerate() {
                *into = Listed {
                    timing: timing(from),
                    preferred: page * MODES_A_PAGE + index == preferred,
                };
            }
            Message::ModeList(ModePage {
                scanout: 0,
                total,
                first: u32::try_from(page * MODES_A_PAGE).unwrap_or(u32::MAX),
                count: u32::try_from(chunk.len()).unwrap_or(0),
                entries,
            })
        })
}

/// The EDID message for C's display: what NVKMS read, or none for one the
/// message cannot carry, which is said.
fn edid(c: &CHello) -> Edid {
    let read = usize::try_from(c.edid_bytes).unwrap_or(usize::MAX);
    let length = if read <= EDID_BYTES {
        read
    } else {
        say!(
            "display: the monitor's EDID is {read} bytes, more than displayctl's {EDID_BYTES}; none sent"
        );
        0
    };
    let mut bytes = [0; EDID_BYTES];
    if let (Some(into), Some(from)) = (bytes.get_mut(..length), c.edid.get(..length)) {
        into.copy_from_slice(from);
    }
    Edid {
        scanout: 0,
        length: u32::try_from(length).unwrap_or(0),
        bytes,
    }
}

/// Make the display control channel, say HELLO for the scanout C runs,
/// and wait for READY. `NV_OK`, or why not,
/// said.
///
/// # Safety
///
/// `described` points at a `struct nvos_display_hello`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_display_start(described: *const CHello) -> NvStatus {
    if described.is_null() {
        return status::INVALID_ARGUMENT;
    }
    // SAFETY: the caller vouches for it.
    let described = unsafe { &*described };
    let hello = hello(described);
    let device = device::handle();
    if device == 0 {
        return status::INVALID_STATE;
    }
    // SAFETY: the call takes the device's handle and nothing else.
    let made =
        unsafe { futex::call6(nr::DISPLAY_CONTROL_CREATE, [device as usize, 0, 0, 0, 0, 0]) };
    if (-4095..0).contains(&(made as isize)) {
        say!("display_control_create refused: errno {}", -(made as isize));
        return status::NOT_SUPPORTED;
    }
    let Ok(handle) = u32::try_from(made) else {
        return status::INVALID_STATE;
    };
    CONTROL.store(handle, Ordering::Release);
    let Some(channel) = control() else {
        return status::INVALID_STATE;
    };
    let Ok(driver_port) = port::create(Kernel) else {
        return status::NO_MEMORY;
    };
    let Ok(share) = driver_port
        .as_owned()
        .duplicate(Requested::Exactly(PORT_RIGHTS))
    else {
        return status::INVALID_STATE;
    };
    PORT.store(driver_port.as_owned().raw().0, Ordering::Release);
    core::mem::forget(driver_port);
    if let Err((why, _)) = channel.write_with(Message::Hello(hello).encode().as_bytes(), [share]) {
        say!("display HELLO not written: {why:?}");
        return status::INVALID_STATE;
    }
    if let Err(why) = channel.wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never) {
        say!("display wait for READY: {why:?}");
        return status::INVALID_STATE;
    }
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle(0); 1];
    let received = match channel.read(&mut bytes, &mut handles) {
        Ok(received) => received,
        Err(why) => {
            say!("display READY not read: {why:?}");
            return status::INVALID_STATE;
        }
    };
    // The core's port is for a driver that queues to it; this one does not.
    for handle in handles.iter().take(received.handles) {
        drop(OwnedHandle::from_raw(Kernel, *handle));
    }
    let ready = match Message::decode(bytes.get(..received.bytes).unwrap_or(&[])) {
        Ok(Message::Ready(ready)) if received.handles == 1 => ready,
        Ok(Message::Refused(refusal)) => {
            say!("display: refused: {refusal}");
            return status::INVALID_STATE;
        }
        _ => {
            say!("display: no READY");
            return status::INVALID_STATE;
        }
    };
    // The display's own EDID, which only the driver can read; the core
    // keeps a file drm.edid_firmware names over it.
    if let Err(why) = channel.write(Message::Edid(edid(described)).encode().as_bytes()) {
        say!("display: the EDID not written: {why:?}");
    }
    // Every mode NVKMS validated, which the core lists and lets a program
    // set (displayctl v11).
    for page in mode_pages(described) {
        if let Err(why) = channel.write(page.encode().as_bytes()) {
            say!("display: the mode list not written: {why:?}");
            break;
        }
    }
    say!(
        "display: card{} ready, its buffers up to {} MiB, each mapped read-only at ATTACH",
        ready.card,
        ready.card_bytes >> 20
    );
    status::OK
}

fn rect_into(event: &mut Event, rect: Rect) {
    event.x = i32::try_from(rect.x).unwrap_or(i32::MAX);
    event.y = i32::try_from(rect.y).unwrap_or(i32::MAX);
    event.w = rect.width;
    event.h = rect.height;
}

/// Map an ATTACH's buffer VMO read-only: where, or `None` for a buffer that
/// does not fit its VMO or will not map.
fn map_buffer(attach: &ferrix_displayctl::message::Attach, vmo: &Vmo<Kernel>) -> Option<usize> {
    let bytes = vmo.size().ok()?;
    attach.validate(bytes).ok()?;
    let length = usize::try_from(attach.length).ok()?;
    match vmo.map(None, length, Protection::Read, attach.offset) {
        Ok(at) if remember(attach.buffer, at, length) => Some(at),
        Ok(at) => {
            say!(
                "display: no room to keep buffer {}'s mapping",
                attach.buffer
            );
            // SAFETY: just made, and nothing has seen it.
            let _ = unsafe { crate::libc::munmap(core::ptr::without_provenance_mut(at), length) };
            None
        }
        Err(why) => {
            say!(
                "display: buffer {} did not map read-only: {why:?}",
                attach.buffer
            );
            None
        }
    }
}

/// The event C acts on for one message, or `None` for one answered here.
/// `vmo` is the handle the message came with: an ATTACH's buffer, and
/// nothing for any other.
fn event_of(message: Message, vmo: Option<Vmo<Kernel>>) -> Option<Event> {
    let mut event = Event::default();
    if vmo.is_some() && !matches!(message, Message::Attach(_)) {
        say!("display: a message that carries no handle came with one");
        return None;
    }
    match message {
        Message::Attach(attach) => {
            // The mapping outlives the handle, which closes here.
            let Some(at) = vmo.and_then(|vmo| map_buffer(&attach, &vmo)) else {
                reply(Message::Attached {
                    buffer: attach.buffer,
                    status: Status::Invalid,
                });
                return None;
            };
            event.kind = EVENT_ATTACH;
            event.buffer = attach.buffer;
            event.pixels = at as u64;
            event.length = attach.length;
            event.width = attach.width;
            event.height = attach.height;
            event.stride = attach.stride;
        }
        // Pixels on a GPU this card does not share with the core.
        Message::AttachObject(attach) => {
            reply(Message::Attached {
                buffer: attach.buffer,
                status: Status::Invalid,
            });
            return None;
        }
        Message::Scanout { buffer, rect, .. } => {
            event.kind = EVENT_SCANOUT;
            event.buffer = buffer;
            rect_into(&mut event, rect);
        }
        Message::Flush {
            buffer,
            sequence,
            rect,
        } => {
            event.kind = EVENT_FLUSH;
            event.buffer = buffer;
            event.sequence = sequence;
            rect_into(&mut event, rect);
        }
        Message::Detach { buffer } => {
            event.kind = EVENT_DETACH;
            event.buffer = buffer;
        }
        Message::Cursor {
            buffer,
            sequence,
            x,
            y,
            ..
        } => {
            event.kind = EVENT_CURSOR;
            event.buffer = buffer;
            event.sequence = sequence;
            event.x = x;
            event.y = y;
        }
        Message::Move { x, y, .. } => {
            event.kind = EVENT_MOVE;
            event.x = x;
            event.y = y;
        }
        Message::Stop => event.kind = EVENT_STOP,
        Message::SetMode { timing, .. } => {
            event.kind = EVENT_MODE;
            event.timing = CTiming {
                clock_khz: timing.clock_khz,
                hdisplay: timing.hdisplay,
                hsync_start: timing.hsync_start,
                hsync_end: timing.hsync_end,
                htotal: timing.htotal,
                vdisplay: timing.vdisplay,
                vsync_start: timing.vsync_start,
                vsync_end: timing.vsync_end,
                vtotal: timing.vtotal,
                flags: u32::from(timing.hsync_high) | u32::from(timing.vsync_high) << 1,
            };
        }
        other => {
            say!("display: a message a driver is not sent: {other:?}");
            return None;
        }
    }
    Some(event)
}

/// Wait for the next message C acts on and write it to `out`. `NV_OK`; an
/// `EVENT_CLOSED` once the core has gone.
///
/// # Safety
///
/// `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_display_next(out: *mut Event) -> NvStatus {
    let Some(channel) = control() else {
        return status::INVALID_STATE;
    };
    let mut bytes = [0_u8; MAX_BYTES];
    loop {
        let mut handles = [Handle(0); ferrix_displayctl::message::Attach::HANDLES];
        let event = match channel.read(&mut bytes, &mut handles) {
            Ok(received) => match (
                Message::decode(bytes.get(..received.bytes).unwrap_or(&[])),
                handles
                    .iter()
                    .take(received.handles)
                    .map(|handle| Vmo::from_owned(OwnedHandle::from_raw(Kernel, *handle)))
                    .next(),
            ) {
                (Ok(message), vmo) => event_of(message, vmo),
                (Err(why), _) => {
                    say!("display: a malformed message: {why:?}");
                    None
                }
            },
            Err(ReadError::Failed(Error::ShouldWait)) => {
                if channel
                    .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
                    .is_err()
                {
                    Some(Event {
                        kind: EVENT_CLOSED,
                        ..Event::default()
                    })
                } else {
                    None
                }
            }
            Err(_) => Some(Event {
                kind: EVENT_CLOSED,
                ..Event::default()
            }),
        };
        if let Some(event) = event {
            // SAFETY: the caller vouches for `out`.
            unsafe { out.write(event) };
            return status::OK;
        }
    }
}

/// Write one message to the core; a channel gone is the next read's news.
fn reply(message: Message) {
    if let Some(channel) = control()
        && let Err(why) = channel.write(message.encode().as_bytes())
    {
        say!("display: a reply not written: {why:?}");
    }
}

fn status_of(raw: u32) -> Status {
    Status::from_raw(raw).unwrap_or(Status::Invalid)
}

/// Answer ATTACH for `buffer`: `status` is a `Status` word. A buffer C
/// refused is unmapped here.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_display_attached(buffer: u32, status: u32) {
    if status_of(status) != Status::Ok {
        unmap(buffer);
    }
    reply(Message::Attached {
        buffer,
        status: status_of(status),
    });
}

/// Answer the flush or CURSOR numbered `sequence`.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_display_flipped(sequence: u64, status: u32) {
    reply(Message::Flipped {
        sequence,
        status: status_of(status),
    });
}

/// Answer DETACH for `buffer`, whose mapping goes first: C has let go.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_display_detached(buffer: u32, status: u32) {
    unmap(buffer);
    reply(Message::Detached {
        buffer,
        status: status_of(status),
    });
}

/// Answer STOP.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_display_stopped() {
    reply(Message::Stopped);
}

// The protocol's version this bridge was written against.
const _: () = assert!(VERSION == 11);

#[cfg(test)]
mod tests {
    use super::{
        CHello, CTiming, MAX_LISTED_MODES, Message, NVKMS_EDID_BYTES, drm, drm_connector, edid,
        hello, mode_pages,
    };

    fn described(connector: u32, edid_bytes: u32) -> CHello {
        let mut c = CHello {
            width: 1024,
            height: 600,
            refresh_mhz: 59_821,
            count: 0,
            timings: [CTiming::default(); MAX_LISTED_MODES],
            preferred: 0,
            connector,
            edid_bytes,
            edid: [0; NVKMS_EDID_BYTES],
        };
        for (index, byte) in c.edid.iter_mut().enumerate() {
            *byte = (index % 200) as u8 + 1;
        }
        c
    }

    #[test]
    fn nvkms_connectors_are_named_as_nvidia_drm_names_them() {
        assert_eq!(drm_connector(6), drm::CONNECTOR_HDMIA);
        assert_eq!(drm_connector(0), drm::CONNECTOR_DISPLAYPORT);
        assert_eq!(
            drm_connector(7),
            drm::CONNECTOR_DISPLAYPORT,
            "USB-C carries DP"
        );
        assert_eq!(drm_connector(3), drm::CONNECTOR_DVID);
        assert_eq!(drm_connector(10), 0, "unknown leaves the core its choice");
        let said = hello(&described(6, 0));
        assert_eq!(said.connectors[0], drm::CONNECTOR_HDMIA);
        assert!(said.connectors[1..].iter().all(|&kind| kind == 0));
    }

    #[test]
    fn the_edid_message_carries_what_nvkms_read_and_no_more() {
        let sent = edid(&described(6, 384));
        assert_eq!((sent.scanout, sent.length), (0, 384));
        assert_eq!(sent.as_bytes().len(), 384);
        assert!(sent.bytes[384..].iter().all(|&byte| byte == 0));
        // One the message cannot carry is none; none is none.
        assert_eq!(edid(&described(6, 1024)).length, 0);
        assert_eq!(edid(&described(6, 0)).length, 0);
    }

    #[test]
    fn every_mode_nvkms_validated_goes_as_mode_list_pages() {
        let mut c = described(6, 0);
        c.count = 30;
        c.preferred = 3;
        for (index, timing) in c.timings.iter_mut().take(30).enumerate() {
            let width = 640 + u16::try_from(index).unwrap_or(0) * 8;
            *timing = CTiming {
                clock_khz: 25_175,
                hdisplay: width,
                hsync_start: width + 16,
                hsync_end: width + 112,
                htotal: width + 160,
                vdisplay: 480,
                vsync_start: 490,
                vsync_end: 492,
                vtotal: 525,
                flags: 0,
            };
        }
        let pages: Vec<Message> = mode_pages(&c).collect();
        assert_eq!(pages.len(), 2);
        let mut preferred = 0;
        for message in &pages {
            let Message::ModeList(page) = message else {
                panic!("a page");
            };
            assert_eq!(page.total, 30);
            preferred += page.as_slice().iter().filter(|l| l.preferred).count();
            if page.first == 0 {
                assert!(page.entries[3].preferred);
            }
        }
        assert_eq!(preferred, 1);
        // HELLO lists the running mode alone.
        assert_eq!(hello(&c).timings.count, 1);
    }
}
