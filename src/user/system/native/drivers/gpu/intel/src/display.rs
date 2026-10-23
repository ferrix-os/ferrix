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
//!
//! Its modes (displayctl v11, `docs/DISPLAY.md` §9.4): the plane's own,
//! preferred, and every smaller standard size, shown scaled -- the pipe is
//! the firmware's and nothing here retimes it, so a smaller mode is the
//! buffer scaled into the plane by the copy, aspect kept, centred, black
//! bars, as Linux's panel fitter shows one. The connector is named by what
//! drives pipe A (`ferrix_intel_xelp::plane::Output`), so the box's
//! monitor is `HDMI-A-1` and not `Virtual-1`.

use core::ptr;
use core::sync::atomic::Ordering;

use ferrix_display_modes::scale::Scaler;
use ferrix_display_modes::{ModeList, add_scaled};
use ferrix_displayctl::message::{
    Attach, EDID_BYTES, Edid, Hello, MAX_BYTES, MAX_DIMENSION, MAX_SCANOUTS, Message, PORT_RIGHTS,
    Rect, ScanoutMode, Status, Timing, Timings, VERSION,
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

mod hotplug;
mod modeset;
mod regs;

/// The port key the display control's signals arrive under, beside
/// `serve::KEY_CONTROL` (1) and `serve::KEY_INTERRUPT` (2).
pub(crate) const KEY_DISPLAY: u64 = 3;

/// The port key the hotplug poll arrives under: the serving loop's wake-up
/// (`crate::serve::WAKE_AT`), which the card sets (`docs/DESKTOP.md` §3.4).
pub(crate) const KEY_HOTPLUG: u64 = crate::serve::KEY_WAKE;

/// Whether `key` is one of the card's: its control channel's, or the
/// hotplug poll's.
pub(crate) const fn ours(key: u64) -> bool {
    key == KEY_DISPLAY || key == KEY_HOTPLUG
}
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
    /// How a mode smaller than the plane is shown on it, since `SET_MODE`
    /// chose one; `None` at the plane's own.
    scaler: Option<Scaler>,
    /// Frames copied, for the first lines.
    flushes: u64,
    /// The lit port's hot-plug state, where it can be followed.
    hotplug: Option<hotplug::Watch>,
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
        if let Err(why) = introduce(&control, port, location, &screen, plane.connector) {
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
        let mut hotplug = match regs::Regs::map(device, apertures) {
            Ok(mut regs) => {
                // `ferrix.intelgpu.modeset=dry`: the plan for the second
                // port, printed and not written (`docs/DESKTOP.md` §3.5).
                if modeset::asked() {
                    modeset::print_plan(&mut regs);
                }
                hotplug::Watch::start(regs)
            }
            Err(why) => {
                say(format_args!("intel: card0: no hotplug: {why}"));
                None
            }
        };
        // The monitor's own EDID, which names it to the compositor
        // (`docs/DESKTOP.md` §3.4, step 3g).
        if let Some(watch) = hotplug.as_mut() {
            send_edid(&control, watch);
        }
        if hotplug.is_some() {
            schedule(now_ms());
        }
        Some(Display {
            screen,
            control,
            buffers: [None; MAX_BUFFERS],
            shown: None,
            owed_whole: false,
            scaler: None,
            flushes: 0,
            hotplug,
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

/// Milliseconds of `CLOCK_MONOTONIC`.
fn now_ms() -> u64 {
    ferrix_rt::linux::monotonic_nanos().unwrap_or(0) / 1_000_000
}

/// The hotplug poll due at `at_ms`.
fn schedule(at_ms: u64) {
    crate::serve::WAKE_AT.store(at_ms.saturating_mul(1_000_000).max(1), Ordering::Relaxed);
}

impl Display {
    /// A packet under one of the card's keys ([`ours`]): the hotplug poll,
    /// or the control channel's. `false` when the card is gone.
    pub(crate) fn on_key(&mut self, key: u64, port: &Port<Kernel>) -> bool {
        if key == KEY_HOTPLUG {
            return self.poll_hotplug();
        }
        self.on_packet(port)
    }

    /// Read the lit port's hot-plug state, tell the core of a settled
    /// change, and set the next poll. `false` when the core cannot be told.
    fn poll_hotplug(&mut self) -> bool {
        let Some(watch) = self.hotplug.as_mut() else {
            return true;
        };
        let (changed, next) = watch.poll(now_ms());
        schedule(next);
        let Some(connected) = changed else {
            return true;
        };
        // A monitor plugged in may be another one: its EDID first.
        if connected {
            send_edid(&self.control, watch);
        }
        match tell_connected(&self.control, &self.screen, connected) {
            Ok(()) => true,
            Err(why) => {
                say(format_args!("intel: card0 stopped: {why}"));
                false
            }
        }
    }

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
            // A mode of the list: the plane's own, or a smaller one shown
            // scaled. Nothing answers it.
            Message::SetMode { timing, .. } => {
                let size = (u32::from(timing.hdisplay), u32::from(timing.vdisplay));
                let plane = (self.screen.width as u32, self.screen.height as u32);
                if size.0 > plane.0 || size.1 > plane.1 || !timing.is_mode() {
                    return Err("SET_MODE for a mode larger than the plane");
                }
                self.scaler = Scaler::new(size, plane).filter(|scaler| !scaler.is_identity());
                self.clear();
                say(format_args!(
                    "intel: card0: mode {}x{}{}",
                    size.0,
                    size.1,
                    if self.scaler.is_some() {
                        ", scaled to the plane"
                    } else {
                        ""
                    }
                ));
                Ok(())
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

    /// Black on every pixel of the plane, then the barrier: the bars a
    /// scaled mode leaves, and nothing of the mode before.
    fn clear(&self) {
        let screen = &self.screen;
        for row in 0..screen.height {
            let target = (screen.base + row * screen.pitch) as *mut u32;
            for column in 0..screen.width {
                // SAFETY: `target` starts a line of the lent window mapped
                // for this process, which holds `width` four-byte pixels
                // from there (`map_screen` checked pitch times height
                // against the window), and is four-byte aligned.
                unsafe { ptr::write_volatile(target.wrapping_add(column), 0) };
            }
        }
        ferrix_rt::device_barrier();
    }

    /// The scaled copy: each plane pixel whose nearest buffer pixel lies in
    /// `changed` is read from there (efifb's `copy_scaled`).
    fn copy_scaled(&self, scaler: &Scaler, buffer: &Buffer, shown: Rect, changed: Rect) {
        let in_shown = Rect {
            x: changed.x.saturating_sub(shown.x),
            y: changed.y.saturating_sub(shown.y),
            ..changed
        };
        let Some(area) = scaler.damaged(in_shown) else {
            return;
        };
        let screen = &self.screen;
        for y in area.y..area.y + area.height {
            let source_y = (scaler.source_y(y - scaler.area.y) + shown.y) as usize;
            let line = (y as usize) * screen.pitch;
            for x in area.x..area.x + area.width {
                let source_x = (scaler.source_x(x - scaler.area.x) + shown.x) as usize;
                let from = source_y * buffer.stride + source_x * PIXEL;
                let to = line + (x as usize) * PIXEL;
                if source_x >= buffer.width
                    || source_y >= buffer.height
                    || from + PIXEL > buffer.length
                    || to + PIXEL > screen.len
                {
                    continue;
                }
                // SAFETY: `from` is a pixel inside the buffer's read-only
                // mapping, which lives until its DETACH takes it from
                // `self`, and `to` one inside the lent window; both
                // bounds-checked just above and four-byte aligned.
                let pixel = unsafe { ptr::read_volatile((buffer.base + from) as *const u32) };
                // SAFETY: as above.
                unsafe { ptr::write_volatile((screen.base + to) as *mut u32, pixel) };
            }
        }
    }

    /// `changed` of buffer `id`, clipped as efifb clips it, copied into the
    /// window, then the barrier that makes it shown.
    fn copy(&mut self, id: u32, changed: Rect) {
        let (Some(buffer), Some((_, shown))) = (self.buffer(id), self.shown) else {
            return;
        };
        if let Some(scaler) = self.scaler {
            self.copy_scaled(&scaler, &buffer, shown, changed);
        } else if let Some(area) = area(&buffer, shown, changed, &self.screen) {
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

/// Tell the core the monitor on the lit port is there or not, as
/// virtio-gpu does when its host window comes or goes: `MODES` with the
/// scanout enabled or not, and on a plug the mode list again, which the
/// core counts as the connector's change.
fn tell_connected(
    control: &Channel<Kernel>,
    screen: &Screen,
    connected: bool,
) -> Result<(), &'static str> {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    if let Some(first) = modes.first_mut() {
        *first = ScanoutMode {
            width: screen.width as u32,
            height: screen.height as u32,
            enabled: connected,
            refresh_mhz: 0,
        };
    }
    control
        .write(Message::Modes { modes }.encode().as_bytes())
        .map_err(|_| "MODES could not be written")?;
    if connected {
        send_list(control, screen)?;
    }
    Ok(())
}

/// The monitor's EDID to the core, for scanout 0: what its connector's
/// `EDID` property and the compositor's monitor description are made of.
fn send_edid(control: &Channel<Kernel>, watch: &mut hotplug::Watch) {
    let Some((bytes, length)) = watch.read_edid() else {
        return;
    };
    let mut edid = Edid {
        scanout: 0,
        length: 0,
        bytes: [0; EDID_BYTES],
    };
    let length = length.min(EDID_BYTES);
    if let (Some(to), Some(from)) = (edid.bytes.get_mut(..length), bytes.get(..length)) {
        to.copy_from_slice(from);
        edid.length = length as u32;
    }
    if control
        .write(Message::Edid(edid).encode().as_bytes())
        .is_err()
    {
        say(format_args!("intel: card0: the EDID could not be written"));
    }
}

/// The scanout's mode list: the plane's mode and every smaller one shown
/// scaled.
fn send_list(control: &Channel<Kernel>, screen: &Screen) -> Result<(), &'static str> {
    let mut list = ModeList::new();
    add_scaled(&mut list, timing(screen.width as u16, screen.height as u16));
    list.sort();
    for page in list.pages(0) {
        control
            .write(page.encode().as_bytes())
            .map_err(|_| "the mode list could not be written")?;
    }
    say(format_args!(
        "intel: card0: {} modes listed, the smaller ones shown scaled",
        list.len()
    ));
    Ok(())
}

/// HELLO for the one scanout at the plane's size, READY taken, and the
/// scanout's mode list sent.
fn introduce(
    control: &Channel<Kernel>,
    port: &Port<Kernel>,
    location: u32,
    screen: &Screen,
    connector: u32,
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
        connectors: {
            let mut connectors = [0; MAX_SCANOUTS];
            if let Some(first) = connectors.first_mut() {
                *first = connector;
            }
            connectors
        },
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
        Ok(Message::Ready(_)) if received.handles == 1 => {}
        Ok(Message::Refused(_)) => return Err("the display core refused the card"),
        _ => return Err("the display core did not answer READY"),
    }
    send_list(control, screen)
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
