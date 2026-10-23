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
//!
//! Under `ferrix.intelgpu.modeset=on` the card has a second scanout
//! (`docs/DESKTOP.md` §3.5): pipe B on the other combo port, showing a
//! surface of its own ([`surface`]) -- lit by the driver
//! ([`modeset::light_port`]) on whichever combo port the firmware left
//! dark, or, where the firmware cloned its surface
//! onto both pipes, the running pipe B pointed at the new one. Both ports
//! are watched for hotplug, and a monitor plugged into the second port
//! later is lit then. Each scanout is copied into on its own, so a
//! compositor places two screens side by side.

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

use ferrix_intel_xelp::display::hotplug::Port as PortOf;
use ferrix_intel_xelp::display::light;
use ferrix_intel_xelp::io::Mmio;

use crate::m2::ScreenPlane;
use crate::say;

mod hotplug;
mod modeset;
mod regs;
mod surface;

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

/// What one scanout shows, and where its pixels are written: the lent
/// window for the first, the second surface for the second.
struct Output {
    base: usize,
    len: usize,
    width: usize,
    height: usize,
    pitch: usize,
    shown: Option<(u32, Rect)>,
    /// Whether a SCANOUT changed the buffer shown and no FLUSH has copied
    /// it since: the next FLUSH of it copies all of it.
    owed_whole: bool,
    /// How a mode smaller than the plane is shown on it, since `SET_MODE`
    /// chose one; `None` at the plane's own.
    scaler: Option<Scaler>,
    /// Whether what is written is flushed from the caches after it: the
    /// second surface, mapped cached, which the display engine reads past
    /// them. The lent window is write-combining and needs only the barrier.
    flush: bool,
}

impl Output {
    const fn new(base: usize, len: usize, (width, height): (usize, usize), pitch: usize) -> Self {
        Self {
            base,
            len,
            width,
            height,
            pitch,
            shown: None,
            owed_whole: false,
            scaler: None,
            flush: false,
        }
    }

    /// The second surface lit at `size`.
    fn surface(surface: &surface::Surface, size: (u32, u32)) -> Self {
        Self {
            flush: true,
            ..Self::new(
                surface.base,
                surface.len,
                (size.0 as usize, size.1 as usize),
                light::SURFACE_PITCH as usize,
            )
        }
    }

    /// Flush `[at, at + len)` of the plane's memory where it needs it.
    fn written(&self, at: usize, len: usize) {
        if self.flush {
            crate::arch::flush_range(self.base + at, len);
        }
    }

    const fn size(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }
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

/// The scanouts a card can have here: pipe A's, and pipe B's.
const SCANOUTS: usize = 2;

/// The display card.
pub(crate) struct Display {
    _window: IoMapping<Kernel>,
    /// The second scanout's memory, kept while the card runs.
    surface: Option<surface::Surface>,
    outputs: [Option<Output>; SCANOUTS],
    /// How many scanouts HELLO said.
    scanouts: usize,
    /// Each scanout's mode as last told the core.
    modes: [ScanoutMode; MAX_SCANOUTS],
    control: Channel<Kernel>,
    buffers: [Option<Buffer>; MAX_BUFFERS],
    /// Frames copied, for the first lines.
    flushes: u64,
    /// The display registers, for hotplug and lighting.
    regs: Option<regs::Regs>,
    /// Each scanout's port, where it can be followed.
    watches: [Option<hotplug::Watch>; SCANOUTS],
}

/// What the second scanout starts with.
struct Second {
    surface: surface::Surface,
    /// Its mode where it is lit now.
    size: Option<(u32, u32)>,
    /// DRM's connector type for it.
    connector: u32,
    watch: Option<hotplug::Watch>,
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
        let (window, first, phys) = map_screen(device, apertures, plane)?;
        let control = device
            .display_control()
            .inspect_err(|error| {
                say(format_args!(
                    "intel: no card0: no display control ({error:?})"
                ));
            })
            .ok()?;
        let mut regs = regs::Regs::map(device, apertures)
            .inspect_err(|why| say(format_args!("intel: card0: no hotplug: {why}")))
            .ok();
        // `ferrix.intelgpu.modeset=dry` prints the steps for the second port;
        // `=on` gives the card a second scanout (`docs/DESKTOP.md` §3.5).
        let mut second = None;
        if let Some(regs) = regs.as_mut() {
            match modeset::asked() {
                modeset::Asked::Dry => {
                    if let Some(port) = dark_port(regs) {
                        modeset::dry(regs, port);
                    }
                }
                modeset::Asked::On => {
                    let firmware_end = u64::from(plane.ggtt) + first.len as u64;
                    second = prepare_second(device, apertures, firmware_end, regs);
                }
                modeset::Asked::Nothing => {}
            }
        }
        let first_watch = regs.as_mut().and_then(hotplug::Watch::start);
        let (scanouts, modes, connectors) = hello_parts(&first, plane.connector, second.as_ref());
        let running = timing(first.width as u16, first.height as u16);
        if let Err(why) = introduce(
            &control, port, location, scanouts, modes, connectors, running,
        ) {
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
            "intel: card0 published: copies frames into the lit {}x{} plane at {phys:#x} ({} bytes a line, write-combining){}",
            first.width,
            first.height,
            first.pitch,
            if scanouts == 2 {
                ", and a second scanout"
            } else {
                ""
            }
        ));
        let mut card = Display {
            _window: window,
            surface: None,
            outputs: [Some(first), None],
            scanouts,
            modes,
            control,
            buffers: [None; MAX_BUFFERS],
            flushes: 0,
            regs,
            watches: [first_watch, None],
        };
        if let Some(second) = second {
            if let Some(size) = second.size {
                card.outputs[1] = Some(Output::surface(&second.surface, size));
            }
            card.surface = Some(second.surface);
            card.watches[1] = second.watch;
        }
        if let Err(why) = card.announce() {
            say(format_args!("intel: no card0: {why}"));
            return None;
        }
        if card.watches.iter().any(Option::is_some) {
            schedule(now_ms());
        }
        Some(card)
    }

    /// Each lit scanout's modes, and the monitors' own EDIDs, which name
    /// them to the compositor (`docs/DESKTOP.md` §3.4, step 3g).
    fn announce(&mut self) -> Result<(), &'static str> {
        for scanout in 0..self.scanouts {
            let lit = self
                .outputs
                .get(scanout)
                .and_then(Option::as_ref)
                .map(Output::size);
            if let Some(size) = lit
                && self.modes.get(scanout).is_some_and(|mode| mode.enabled)
            {
                send_list(&self.control, scanout as u32, size)?;
                self.send_edid(scanout);
            }
        }
        Ok(())
    }

    /// The EDID of scanout `scanout`'s monitor to the core.
    fn send_edid(&mut self, scanout: usize) {
        let (Some(regs), Some(Some(watch))) = (self.regs.as_mut(), self.watches.get(scanout))
        else {
            return;
        };
        send_edid(&self.control, watch, regs, scanout as u32);
    }
}

/// HELLO's scanout count, modes and connectors: pipe A's plane, and the
/// second scanout's where there is one -- enabled where it is lit now, at
/// its size, else at 1920x1080 until a monitor is plugged in.
fn hello_parts(
    first: &Output,
    connector: u32,
    second: Option<&Second>,
) -> (usize, [ScanoutMode; MAX_SCANOUTS], [u32; MAX_SCANOUTS]) {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    let mut connectors = [0; MAX_SCANOUTS];
    if let (Some(mode), Some(slot)) = (modes.first_mut(), connectors.first_mut()) {
        *mode = ScanoutMode {
            width: first.width as u32,
            height: first.height as u32,
            enabled: true,
            refresh_mhz: 0,
        };
        *slot = connector;
    }
    let Some(second) = second else {
        return (1, modes, connectors);
    };
    if let (Some(mode), Some(slot)) = (modes.get_mut(1), connectors.get_mut(1)) {
        let (width, height) = second.size.unwrap_or((1920, 1080));
        *mode = ScanoutMode {
            width,
            height,
            enabled: second.size.is_some(),
            refresh_mhz: 0,
        };
        *slot = second.connector;
    }
    (2, modes, connectors)
}

/// The combo port the firmware left dark: B where pipe A drives A, A where
/// it drives B. `None`, said, where pipe A drives neither.
fn dark_port(regs: &mut regs::Regs) -> Option<PortOf> {
    let lit = PortOf::of_ddi_func_ctl(regs.read32(ferrix_intel_xelp::plane::TRANS_DDI_FUNC_CTL_A));
    match lit {
        Some(PortOf::Combo(0)) => Some(PortOf::Combo(1)),
        Some(PortOf::Combo(1)) => Some(PortOf::Combo(0)),
        _ => {
            say(format_args!(
                "intel: card0: no second port: pipe A drives no combo port"
            ));
            None
        }
    }
}

/// The second scanout under `ferrix.intelgpu.modeset=on`: the other combo
/// port, its surface made past `firmware_end`, and pipe B lit on it now
/// where a monitor is there -- or, where the firmware runs pipe B already
/// (both ports cloned onto one surface), pipe B pointed at the new surface.
/// `None`, said, when there is no second port or no surface.
fn prepare_second(
    device: &Device<Kernel>,
    apertures: u32,
    firmware_end: u64,
    regs: &mut regs::Regs,
) -> Option<Second> {
    let first =
        PortOf::of_ddi_func_ctl(regs.read32(ferrix_intel_xelp::plane::TRANS_DDI_FUNC_CTL_A));
    let cloned = light::running(regs, 1);
    let port = match (cloned, first) {
        (Some(running), _) => running.port,
        (None, Some(PortOf::Combo(0))) => PortOf::Combo(1),
        (None, Some(PortOf::Combo(1))) => PortOf::Combo(0),
        _ => {
            say(format_args!(
                "intel: card0: no second scanout: pipe A drives no combo port"
            ));
            return None;
        }
    };
    let surface = surface::Surface::make(device, apertures, firmware_end)?;
    let mut size = None;
    if let Some(running) = cloned {
        say(format_args!(
            "intel: card0: the firmware runs pipe B on port {} at {}x{}: it gets a surface of its own",
            running.port.name(),
            running.size.0,
            running.size.1
        ));
        if modeset::repoint(regs, 1, surface.ggtt, running.size) {
            size = Some(running.size);
        }
    } else if port
        .live_bit()
        .is_some_and(|bit| regs.read32(ferrix_intel_xelp::display::hotplug::SDEISR) & bit != 0)
    {
        size = modeset::light_port(regs, port, surface.ggtt).map(|mode| (mode.h[0], mode.v[0]));
    }
    let function = regs.read32(ferrix_intel_xelp::plane::TRANS_DDI_FUNC_CTL_A + 0x1000);
    let connector = match (ferrix_intel_xelp::plane::Output {
        ddi_func_ctl: function,
        pp_status: 0,
    })
    .drm_connector()
    {
        0 => ferrix_intel_xelp::plane::DRM_CONNECTOR_HDMIA,
        named => named,
    };
    let watch = hotplug::Watch::on(regs, port, 1);
    Some(Second {
        surface,
        size,
        connector,
        watch,
    })
}

/// The lent window found, checked against the lit plane, and mapped
/// write-combining: the screen, and the window's physical address.
fn map_screen(
    device: &Device<Kernel>,
    apertures: u32,
    plane: ScreenPlane,
) -> Option<(IoMapping<Kernel>, Output, u64)> {
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
        mapping,
        Output::new(base, len, (width, height), pitch),
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

    /// Read each watched port's hot-plug state, tell the core of a
    /// settled change, and set the next poll. A monitor plugged into the
    /// second port before it was ever lit is lit now. `false` when the core
    /// cannot be told.
    fn poll_hotplug(&mut self) -> bool {
        let Some(regs) = self.regs.as_mut() else {
            return true;
        };
        let now = now_ms();
        let mut next = u64::MAX;
        let mut changes = [None; SCANOUTS];
        for (watch, change) in self.watches.iter_mut().zip(changes.iter_mut()) {
            if let Some(watch) = watch {
                let (changed, at) = watch.poll(regs, now);
                next = next.min(at);
                *change = changed;
            }
        }
        schedule(next);
        for (scanout, change) in changes.into_iter().enumerate() {
            if let Some(connected) = change
                && let Err(why) = self.changed(scanout, connected)
            {
                say(format_args!("intel: card0 stopped: {why}"));
                return false;
            }
        }
        true
    }

    /// A settled change on `scanout`'s port told to the core: on a plug,
    /// the port lit first if it never was, its EDID, then `MODES` and the
    /// mode list.
    fn changed(&mut self, scanout: usize, connected: bool) -> Result<(), &'static str> {
        if connected && !self.shows(scanout) {
            return Ok(());
        }
        if connected {
            self.send_edid(scanout);
        }
        let size = self
            .outputs
            .get(scanout)
            .and_then(Option::as_ref)
            .map(Output::size);
        if let Some(mode) = self.modes.get_mut(scanout) {
            if let Some((width, height)) = size {
                mode.width = width;
                mode.height = height;
            }
            mode.enabled = connected;
        }
        tell_connected(&self.control, &self.modes, scanout, size, connected)
    }

    /// Whether `scanout`'s monitor, plugged in, is shown: its pipe runs
    /// still, or the second port, never lit, is lit now.
    fn shows(&mut self, scanout: usize) -> bool {
        let (Some(regs), Some(Some(watch))) = (self.regs.as_mut(), self.watches.get(scanout))
        else {
            return false;
        };
        if self.outputs.get(scanout).is_some_and(Option::is_some) {
            return watch.shown_again(regs);
        }
        let Some(surface) = self.surface.as_ref() else {
            return false;
        };
        let Some(mode) = modeset::light_port(regs, watch.port(), surface.ggtt) else {
            return false;
        };
        let lit = Output::surface(surface, (mode.h[0], mode.v[0]));
        if let Some(slot) = self.outputs.get_mut(scanout) {
            *slot = Some(lit);
        }
        true
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
            Message::Scanout {
                scanout,
                buffer,
                rect,
            } => {
                let known = buffer != 0 && self.buffer(buffer).is_some();
                if let Some(Some(output)) = self.outputs.get_mut(scanout as usize) {
                    output.shown = known.then_some((buffer, rect));
                    output.owed_whole = known;
                }
                Ok(())
            }
            Message::Flush {
                buffer,
                sequence,
                rect,
            } => {
                self.flush(buffer, rect);
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
            Message::SetMode { scanout, timing } => {
                let Some(Some(output)) = self.outputs.get_mut(scanout as usize) else {
                    return Err("SET_MODE for a scanout that is not lit");
                };
                let size = (u32::from(timing.hdisplay), u32::from(timing.vdisplay));
                let plane = output.size();
                if size.0 > plane.0 || size.1 > plane.1 || !timing.is_mode() {
                    return Err("SET_MODE for a mode larger than the plane");
                }
                output.scaler = Scaler::new(size, plane).filter(|scaler| !scaler.is_identity());
                clear(output);
                say(format_args!(
                    "intel: card0: scanout {scanout} mode {}x{}{}",
                    size.0,
                    size.1,
                    if output.scaler.is_some() {
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

    /// `changed` of `buffer` copied to every scanout that shows it -- all of
    /// it to one that has not since its SCANOUT.
    fn flush(&mut self, buffer: u32, rect: Rect) {
        let Some(source) = self.buffer(buffer) else {
            return;
        };
        for output in self.outputs.iter_mut().flatten() {
            let Some((id, whole)) = output.shown else {
                continue;
            };
            if id != buffer {
                continue;
            }
            let changed = if core::mem::take(&mut output.owed_whole) {
                whole
            } else {
                rect
            };
            copy(output, &source, changed);
            self.flushes += 1;
            if self.flushes == 1 {
                say(format_args!(
                    "intel: card0: the first frame is on the screen"
                ));
            }
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
        for output in self.outputs.iter_mut().flatten() {
            if output.shown.is_some_and(|(shown, _)| shown == id) {
                output.shown = None;
            }
        }
        if ferrix_rt::linux::munmap(buffer.base, buffer.length).is_err() {
            return Status::DeviceRefused;
        }
        Status::Ok
    }
}

/// Black on every pixel of a plane, then the barrier: the bars a scaled
/// mode leaves, and nothing of the mode before.
fn clear(screen: &Output) {
    for row in 0..screen.height {
        let target = (screen.base + row * screen.pitch) as *mut u32;
        for column in 0..screen.width {
            // SAFETY: `target` starts a line of the plane's memory mapped
            // for this process, which holds `width` four-byte pixels from
            // there (`map_screen` and `surface` checked pitch times height
            // against it), and is four-byte aligned.
            unsafe { ptr::write_volatile(target.wrapping_add(column), 0) };
        }
        screen.written(row * screen.pitch, screen.width * PIXEL);
    }
    ferrix_rt::device_barrier();
}

/// The scaled copy: each plane pixel whose nearest buffer pixel lies in
/// `changed` is read from there (efifb's `copy_scaled`).
fn copy_scaled(screen: &Output, scaler: &Scaler, buffer: &Buffer, shown: Rect, changed: Rect) {
    let in_shown = Rect {
        x: changed.x.saturating_sub(shown.x),
        y: changed.y.saturating_sub(shown.y),
        ..changed
    };
    let Some(area) = scaler.damaged(in_shown) else {
        return;
    };
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
            // mapping, which lives until its DETACH takes it from the
            // card, and `to` one inside the plane's memory; both
            // bounds-checked just above and four-byte aligned.
            let pixel = unsafe { ptr::read_volatile((buffer.base + from) as *const u32) };
            // SAFETY: as above.
            unsafe { ptr::write_volatile((screen.base + to) as *mut u32, pixel) };
        }
        screen.written(
            line + (area.x as usize) * PIXEL,
            (area.width as usize) * PIXEL,
        );
    }
}

/// `changed` of `buffer`, clipped as efifb clips it, copied into the
/// plane's memory, then the barrier that makes it shown.
fn copy(screen: &Output, buffer: &Buffer, changed: Rect) {
    let Some((_, shown)) = screen.shown else {
        return;
    };
    if let Some(scaler) = screen.scaler {
        copy_scaled(screen, &scaler, buffer, shown, changed);
    } else if let Some(area) = area(buffer, shown, changed, screen) {
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
            // the plane's memory is write-combining, which takes whole
            // lines best, and a pixel at a time held a 1920x1200 frame to
            // about a gigabyte a second (9.5 ms of hyprix's frame on the
            // N150). Linux's `memcpy_toio` is a plain `memcpy` on x86 for
            // the same reason. The writes are not elided: the memory is
            // outside anything the compiler can see, and the barrier below
            // drains them to it.
            //
            // SAFETY: `source` is inside the buffer's read-only mapping,
            // which lives until its DETACH takes it from the card, and
            // `target` inside the plane's memory mapped for this process;
            // both bounds-checked for the whole row above, and the two
            // mappings are different memory, so they do not overlap.
            unsafe { ptr::copy_nonoverlapping(source, target, bytes) };
            screen.written(to, bytes);
        }
    }
    ferrix_rt::device_barrier();
}

/// Tell the core a scanout's monitor is there or not, as virtio-gpu does
/// when its host window comes or goes: `MODES` with every scanout as it is
/// now, and on a plug the scanout's mode list again, which the core counts
/// as the connector's change.
fn tell_connected(
    control: &Channel<Kernel>,
    modes: &[ScanoutMode; MAX_SCANOUTS],
    scanout: usize,
    size: Option<(u32, u32)>,
    connected: bool,
) -> Result<(), &'static str> {
    control
        .write(Message::Modes { modes: *modes }.encode().as_bytes())
        .map_err(|_| "MODES could not be written")?;
    if connected && let Some(size) = size {
        send_list(control, scanout as u32, size)?;
    }
    Ok(())
}

/// A monitor's EDID to the core, for `scanout`: what its connector's `EDID`
/// property and the compositor's monitor description are made of.
fn send_edid(
    control: &Channel<Kernel>,
    watch: &hotplug::Watch,
    regs: &mut regs::Regs,
    scanout: u32,
) {
    let Some((bytes, length)) = watch.read_edid(regs) else {
        return;
    };
    let mut edid = Edid {
        scanout,
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

/// A scanout's mode list: its plane's mode and every smaller one shown
/// scaled.
fn send_list(
    control: &Channel<Kernel>,
    scanout: u32,
    (width, height): (u32, u32),
) -> Result<(), &'static str> {
    let mut list = ModeList::new();
    add_scaled(&mut list, timing(width as u16, height as u16));
    list.sort();
    for page in list.pages(scanout) {
        control
            .write(page.encode().as_bytes())
            .map_err(|_| "the mode list could not be written")?;
    }
    say(format_args!(
        "intel: card0: scanout {scanout}: {} modes listed, the smaller ones shown scaled",
        list.len()
    ));
    Ok(())
}

/// HELLO for `scanouts` scanouts, READY taken.
fn introduce(
    control: &Channel<Kernel>,
    port: &Port<Kernel>,
    location: u32,
    scanouts: usize,
    modes: [ScanoutMode; MAX_SCANOUTS],
    connectors: [u32; MAX_SCANOUTS],
    running: Timing,
) -> Result<(), &'static str> {
    let mut timings = Timings::NONE;
    if let Some(slot) = timings.list.first_mut() {
        *slot = running;
        timings.count = 1;
    }
    let hello = Hello {
        version: VERSION,
        scanouts: scanouts as u16,
        location,
        modes,
        virgl: false,
        capsets: 0,
        capset: 0,
        capset_bytes: 0,
        cursor: false,
        timings,
        copies: true,
        connectors,
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

fn area(buffer: &Buffer, shown: Rect, changed: Rect, screen: &Output) -> Option<Area> {
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
