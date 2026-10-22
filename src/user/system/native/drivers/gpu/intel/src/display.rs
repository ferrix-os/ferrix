//! `card0` over the lit plane: the Intel driver as a copying display card
//! (`docs/INTEL-GPU.md` §4.3, M6's first step; `docs/DISPLAY.md` §8.1).
//!
//! Under `ferrix.efifb` and `ferrix.intelgpu`, K4 lends the firmware's
//! framebuffer to 00:02.0's own node as one more aperture, the only one that
//! is no BAR (`APERTURE_NOT_BAR`), and the display core hands this node a
//! copying card by its identity alone (`L.device.48`). So, as `efifb` does
//! for the same pixels when the GPU has no driver
//! (`drivers/display/efifb`), this driver maps that window write-combining,
//! says HELLO for one scanout at the lit plane's size, and copies each
//! flushed rectangle of a compositor's buffer into the window, the processor
//! doing the copy. Nothing of the display engine is touched: the plane, its
//! GGTT entries and the mode stay the firmware's, and the window is the
//! memory the plane already scans out of. A blit by BCS0 and flips are
//! later steps.
//!
//! The protocol's handling follows efifb's (`efifb/src/main.rs`): ATTACH maps
//! a buffer's VMO read-only, SCANOUT says which buffer is shown and copies
//! nothing (the kernel follows it with a FLUSH of the whole buffer, and a
//! copy at both put every page flip on the screen twice), FLUSH copies the
//! changed rectangle of the buffer on screen -- all of it at the first FLUSH
//! after a SCANOUT -- and answers FLIPPED at once (there is no blanking to
//! wait for), DETACH forgets it. A buffer may be a client's own, which
//! hyprix shows directly (`docs/DISPLAY.md` §8.2): the copy is then the
//! frame's only one. Every copy ends
//! with the device barrier that drains the write-combining buffers.

use core::ptr;

use ferrix_displayctl::message::{
    Attach, Hello, MAX_BYTES, MAX_DIMENSION, MAX_SCANOUTS, Message, PORT_RIGHTS, Rect, ScanoutMode,
    Status, Timing, Timings, VERSION,
};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{APERTURE_NOT_BAR, IoMappingSpec};
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::device::{Device, IoMapping};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::port::Port;
use ferrix_rt::native::vmo::Vmo;

use crate::m2::ScreenPlane;
use crate::say;

/// The port key the display control's signals arrive under, beside
/// `serve::KEY_CONTROL` (1) and `serve::KEY_INTERRUPT` (2).
pub(crate) const KEY_DISPLAY: u64 = 3;
/// Bytes a pixel.
const PIXEL: usize = 4;
/// A page.
const PAGE: usize = 4096;
/// Buffers known at once: a compositor's two, and a client's swap chain
/// shown directly.
const MAX_BUFFERS: usize = 16;
/// The refresh the one timing is listed at.
const REFRESH_HZ: u64 = 60;

/// The lent window, mapped, and the plane's mode.
struct Screen {
    _mapping: IoMapping<Kernel>,
    base: usize,
    len: usize,
    width: usize,
    height: usize,
    pitch: usize,
}

/// A buffer the screen may show.
#[derive(Clone, Copy)]
struct Buffer {
    id: u32,
    base: usize,
    length: usize,
    stride: usize,
    width: usize,
    height: usize,
}

/// The display card.
pub(crate) struct Display {
    screen: Screen,
    control: Channel<Kernel>,
    buffers: [Option<Buffer>; MAX_BUFFERS],
    shown: Option<(u32, Rect)>,
    /// Whether a SCANOUT changed the buffer shown and no FLUSH has copied
    /// it since: the next FLUSH of it copies all of it.
    owed_whole: bool,
    /// Frames copied, for the first lines.
    flushes: u64,
}

impl Display {
    /// Publish `card0`: the lent window found and mapped, the plane checked
    /// as one a copy can fill (linear XRGB8888, the window large enough),
    /// HELLO said and READY taken. `None`, said, when any of it is missing:
    /// the render node is served alone then, as before.
    pub(crate) fn publish(
        device: &Device<Kernel>,
        apertures: u32,
        location: u32,
        plane: Option<ScreenPlane>,
        port: &Port<Kernel>,
    ) -> Option<Display> {
        let Some(plane) = plane else {
            say(format_args!("intel: no card0: pipe A's plane is not lit"));
            return None;
        };
        let (screen, phys) = map_screen(device, apertures, plane)?;
        let (width, height, pitch) = (screen.width, screen.height, screen.pitch);
        let control = device
            .display_control()
            .inspect_err(|error| {
                say(format_args!(
                    "intel: no card0: no display control ({error:?})"
                ));
            })
            .ok()?;
        if let Err(why) = introduce(&control, port, location, &screen) {
            say(format_args!("intel: no card0: {why}"));
            return None;
        }
        if control
            .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_DISPLAY)
            .is_err()
        {
            say(format_args!(
                "intel: no card0: the display control could not be waited on"
            ));
            return None;
        }
        say(format_args!(
            "intel: card0 published: copies frames into the lit {width}x{height} plane at {phys:#x} ({pitch} bytes a line, write-combining)"
        ));
        Some(Display {
            screen,
            control,
            buffers: [None; MAX_BUFFERS],
            shown: None,
            owed_whole: false,
            flushes: 0,
        })
    }
}

/// The lent window found, checked against the lit plane, and mapped
/// write-combining: the screen, and the window's physical address.
fn map_screen(
    device: &Device<Kernel>,
    apertures: u32,
    plane: ScreenPlane,
) -> Option<(Screen, u64)> {
    if !plane.linear_xrgb8888() {
        say(format_args!(
            "intel: no card0: the lit plane is not linear XRGB8888 (PLANE_CTL {:#010x})",
            plane.ctl
        ));
        return None;
    }
    let find = |bar: u8| {
        (0..apertures as usize)
            .filter_map(|index| device.aperture(index).ok())
            .find(|aperture| aperture.bar == bar)
    };
    let Some(window) = find(APERTURE_NOT_BAR) else {
        say(format_args!(
            "intel: no card0: no lent framebuffer (K4 lends it under ferrix.efifb with ferrix.intelgpu)"
        ));
        return None;
    };
    // The window must be the pixels the plane scans: BAR 2 (the GGTT's
    // aperture) at the plane's GGTT offset.
    let gmadr = find(2);
    if gmadr.is_some_and(|bar2| bar2.phys + u64::from(plane.ggtt) != window.phys) {
        say(format_args!(
            "intel: no card0: the lent window at {:#x} is not the plane's surface (BAR 2 {:#x} + GGTT {:#x})",
            window.phys,
            gmadr.map_or(0, |b| b.phys),
            plane.ggtt
        ));
        return None;
    }
    let (width, height) = (plane.width as usize, plane.height as usize);
    let pitch = plane.pitch_bytes() as usize;
    let len = usize::try_from(window.len).unwrap_or(0);
    let largest = MAX_DIMENSION as usize;
    if width == 0
        || height == 0
        || width > largest
        || height > largest
        || pitch < width * PIXEL
        || pitch * height > len
        || !len.is_multiple_of(PAGE)
    {
        say(format_args!(
            "intel: no card0: the plane {width}x{height} (pitch {pitch}) does not fit the lent window ({len:#x} bytes)"
        ));
        return None;
    }
    let mapping = device
        .io_mapping(IoMappingSpec {
            phys: window.phys,
            len: window.len,
        })
        .inspect_err(|error| {
            say(format_args!(
                "intel: no card0: the lent window refused ({error:?})"
            ));
        })
        .ok()?;
    let base = mapping
        .map_combining(None)
        .inspect_err(|error| {
            say(format_args!(
                "intel: no card0: the window's write-combining mapping refused ({error:?})"
            ));
        })
        .ok()?;
    Some((
        Screen {
            _mapping: mapping,
            base,
            len,
            width,
            height,
            pitch,
        },
        window.phys,
    ))
}

impl Display {
    /// A packet under [`KEY_DISPLAY`]: every message taken and answered, the
    /// wait armed again. `false` when the core closed its end or sent what
    /// this card cannot act on; the card is then gone, the render node not.
    pub(crate) fn on_packet(&mut self, port: &Port<Kernel>) -> bool {
        match self.take_messages() {
            Ok(true) => self
                .control
                .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_DISPLAY)
                .is_ok(),
            Ok(false) => {
                say(format_args!(
                    "intel: card0: the display core closed its end"
                ));
                false
            }
            Err(why) => {
                say(format_args!("intel: card0 stopped: {why}"));
                false
            }
        }
    }

    fn reply(&self, message: Message) -> Result<(), &'static str> {
        self.control
            .write(message.encode().as_bytes())
            .map_err(|_| "a reply could not be written")
    }

    /// `Ok(true)` while the card goes on.
    fn take_messages(&mut self) -> Result<bool, &'static str> {
        loop {
            let mut bytes = [0_u8; MAX_BYTES];
            let mut handles = [Handle::INVALID; Attach::HANDLES];
            match self.control.read(&mut bytes, &mut handles) {
                Ok(received) => {
                    let vmo = handles
                        .iter()
                        .take(received.handles)
                        .map(|handle| Vmo::from_owned(OwnedHandle::from_raw(Kernel, *handle)))
                        .next();
                    let message = Message::decode(bytes.get(..received.bytes).unwrap_or_default())
                        .map_err(|_| "a message that does not decode")?;
                    match message {
                        Message::Stop if vmo.is_none() => {
                            let _ = self.reply(Message::Stopped);
                            return Ok(false);
                        }
                        Message::Attach(attach) => {
                            let vmo = vmo.ok_or("an ATTACH without its VMO")?;
                            let status = self.attach(&attach, &vmo);
                            self.reply(Message::Attached {
                                buffer: attach.buffer,
                                status,
                            })?;
                        }
                        _ if vmo.is_some() => {
                            return Err("a handle on a message that carries none");
                        }
                        other => self.act(other)?,
                    }
                }
                Err(ReadError::Failed(Error::PeerClosed)) => return Ok(false),
                Err(ReadError::Failed(Error::ShouldWait)) => return Ok(true),
                Err(_) => return Err("the display control failed"),
            }
        }
    }

    fn act(&mut self, message: Message) -> Result<(), &'static str> {
        match message {
            // A GPU object as a scanout buffer is M6's flip; not yet.
            Message::AttachObject(attach) => self.reply(Message::Attached {
                buffer: attach.buffer,
                status: Status::Invalid,
            }),
            Message::Scanout { buffer, rect, .. } => {
                if buffer == 0 || self.buffer(buffer).is_none() {
                    self.shown = None;
                    self.owed_whole = false;
                } else {
                    self.shown = Some((buffer, rect));
                    self.owed_whole = true;
                }
                Ok(())
            }
            Message::Flush {
                buffer,
                sequence,
                rect,
            } => {
                if let Some((id, whole)) = self.shown
                    && id == buffer
                {
                    let changed = if core::mem::take(&mut self.owed_whole) {
                        whole
                    } else {
                        rect
                    };
                    self.copy(buffer, changed);
                }
                self.reply(Message::Flipped {
                    sequence,
                    status: Status::Ok,
                })
            }
            Message::Detach { buffer } => {
                let status = self.detach(buffer);
                self.reply(Message::Detached { buffer, status })
            }
            _ => Err("a message a copying card without a cursor takes no part in"),
        }
    }

    fn attach(&mut self, attach: &Attach, vmo: &Vmo<Kernel>) -> Status {
        let Ok(vmo_bytes) = vmo.size() else {
            return Status::Invalid;
        };
        if attach.validate(vmo_bytes).is_err() || self.buffer(attach.buffer).is_some() {
            return Status::Invalid;
        }
        let Some(slot) = self.buffers.iter_mut().find(|slot| slot.is_none()) else {
            return Status::OutOfMemory;
        };
        let Ok(length) = usize::try_from(attach.length) else {
            return Status::Invalid;
        };
        let Ok(base) = vmo.map(None, length, Protection::Read, attach.offset) else {
            return Status::OutOfMemory;
        };
        *slot = Some(Buffer {
            id: attach.buffer,
            base,
            length,
            stride: attach.stride as usize,
            width: attach.width as usize,
            height: attach.height as usize,
        });
        Status::Ok
    }

    fn buffer(&self, id: u32) -> Option<Buffer> {
        self.buffers.iter().flatten().find(|b| b.id == id).copied()
    }

    fn detach(&mut self, id: u32) -> Status {
        let Some(slot) = self
            .buffers
            .iter_mut()
            .find(|slot| slot.is_some_and(|b| b.id == id))
        else {
            return Status::Invalid;
        };
        let Some(buffer) = slot.take() else {
            return Status::Invalid;
        };
        if self.shown.is_some_and(|(shown, _)| shown == id) {
            self.shown = None;
        }
        if ferrix_rt::linux::munmap(buffer.base, buffer.length).is_err() {
            return Status::DeviceRefused;
        }
        Status::Ok
    }

    /// `changed` of buffer `id`, clipped as efifb clips it, copied into the
    /// window, then the barrier that makes it shown.
    fn copy(&mut self, id: u32, changed: Rect) {
        let (Some(buffer), Some((_, shown))) = (self.buffer(id), self.shown) else {
            return;
        };
        if let Some(area) = area(&buffer, shown, changed, &self.screen) {
            let screen = &self.screen;
            for row in 0..area.rows {
                let from = (area.source_y + row) * buffer.stride + area.source_x * PIXEL;
                let to = (area.screen_y + row) * screen.pitch + area.screen_x * PIXEL;
                let bytes = area.columns * PIXEL;
                if from + bytes > buffer.length || to + bytes > screen.len {
                    break;
                }
                let source = (buffer.base + from) as *const u8;
                let target = (screen.base + to) as *mut u8;
                // One row as one copy, not a volatile load and store a pixel:
                // the window is write-combining memory, which takes whole
                // lines best, and a pixel at a time held a 1920x1200 frame
                // to about a gigabyte a second (9.5 ms of hyprix's frame on
                // the N150). Linux's `memcpy_toio` is a plain `memcpy` on
                // x86 for the same reason. The writes are not elided: the
                // window is memory outside anything the compiler can see,
                // and the barrier below drains them to it.
                //
                // SAFETY: `source` is inside the buffer's read-only mapping,
                // which lives until its DETACH takes it from `self`, and
                // `target` inside the lent window mapped for this process;
                // both bounds-checked for the whole row above, and the two
                // mappings are different memory, so they do not overlap.
                unsafe { ptr::copy_nonoverlapping(source, target, bytes) };
            }
        }
        ferrix_rt::device_barrier();
        self.flushes += 1;
        if self.flushes == 1 {
            say(format_args!(
                "intel: card0: the first frame is on the screen"
            ));
        }
    }
}

/// HELLO for the one scanout at the plane's size, and READY taken.
fn introduce(
    control: &Channel<Kernel>,
    port: &Port<Kernel>,
    location: u32,
    screen: &Screen,
) -> Result<(), &'static str> {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    if let Some(first) = modes.first_mut() {
        *first = ScanoutMode {
            width: screen.width as u32,
            height: screen.height as u32,
            enabled: true,
            refresh_mhz: 0,
        };
    }
    let mut timings = Timings::NONE;
    if let Some(slot) = timings.list.first_mut() {
        *slot = timing(screen.width as u16, screen.height as u16);
        timings.count = 1;
    }
    let hello = Hello {
        version: VERSION,
        scanouts: 1,
        location,
        modes,
        virgl: false,
        capsets: 0,
        capset: 0,
        capset_bytes: 0,
        cursor: false,
        timings,
        copies: true,
        connectors: [0; MAX_SCANOUTS],
    };
    let share = port
        .as_owned()
        .duplicate(Requested::Exactly(PORT_RIGHTS))
        .map_err(|_| "the port could not be shared")?;
    control
        .write_with(Message::Hello(hello).encode().as_bytes(), [share])
        .map_err(|_| "HELLO could not be written")?;
    let _ = control
        .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
        .map_err(|_| "no answer to HELLO")?;
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = control
        .read(&mut bytes, &mut handles)
        .map_err(|_| "the answer to HELLO could not be read")?;
    for handle in handles.iter().take(received.handles) {
        drop(OwnedHandle::from_raw(Kernel, *handle));
    }
    match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(Message::Ready(_)) if received.handles == 1 => Ok(()),
        Ok(Message::Refused(_)) => Err("the display core refused the card"),
        _ => Err("the display core did not answer READY"),
    }
}

/// The one timing listed: the plane's size at 60 Hz with reduced blanking,
/// as efifb lists it; nothing is ever set.
fn timing(width: u16, height: u16) -> Timing {
    let htotal = width.saturating_add(160);
    let vtotal = height.saturating_add(30);
    let clock_khz = u64::from(htotal) * u64::from(vtotal) * REFRESH_HZ / 1000;
    Timing {
        clock_khz: u32::try_from(clock_khz).unwrap_or(u32::MAX),
        hdisplay: width,
        hsync_start: width.saturating_add(48),
        hsync_end: width.saturating_add(80),
        htotal,
        vdisplay: height,
        vsync_start: height.saturating_add(3),
        vsync_end: height.saturating_add(9),
        vtotal,
        hsync_high: true,
        vsync_high: false,
    }
}

/// The part of a change that reaches the screen (efifb's `Area::of`).
struct Area {
    source_x: usize,
    source_y: usize,
    screen_x: usize,
    screen_y: usize,
    columns: usize,
    rows: usize,
}

fn area(buffer: &Buffer, shown: Rect, changed: Rect, screen: &Screen) -> Option<Area> {
    let clip = |start: u32, length: u32, low: usize, high: usize| {
        let end = (start as usize).saturating_add(length as usize).min(high);
        let start = (start as usize).max(low);
        (start < end).then_some((start, end))
    };
    let (shown_x, shown_y) = (shown.x as usize, shown.y as usize);
    let right = buffer
        .width
        .min(shown_x.saturating_add(shown.width as usize))
        .min(shown_x.saturating_add(screen.width));
    let bottom = buffer
        .height
        .min(shown_y.saturating_add(shown.height as usize))
        .min(shown_y.saturating_add(screen.height));
    let (left, right) = clip(changed.x, changed.width, shown_x, right)?;
    let (top, bottom) = clip(changed.y, changed.height, shown_y, bottom)?;
    Some(Area {
        source_x: left,
        source_y: top,
        screen_x: left - shown_x,
        screen_y: top - shown_y,
        columns: right - left,
        rows: bottom - top,
    })
}
