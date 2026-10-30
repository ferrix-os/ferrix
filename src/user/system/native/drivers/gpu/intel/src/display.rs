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
//! memory the plane already scans out of. Where the render node is served
//! beside it, the copy into the lit plane is the GT's blitter's instead
//! ([`blit`], H5 of `docs/GAME-MODE.md` §5.7), and the processor copies
//! only what the blitter cannot take. Flips are a later step.
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
use ferrix_intel_xelp::frontend::Completion;
use ferrix_intel_xelp::io::Mmio;

use crate::m2::ScreenPlane;

pub(crate) use blit::{Gpu, NoGpu};

mod blit;
mod hotplug;
mod modeset;
mod regs;
mod surface;
mod vblank;

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
/// How long after a GPU's write to a shared object copies evict their
/// source: longer than any frame takes to land.
const EVICT_WINDOW_NS: u64 = 500_000_000;
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
    /// Where its range starts in its VMO.
    offset: u64,
    base: usize,
    length: usize,
    stride: usize,
    width: usize,
    height: usize,
}

/// The scanouts a card can have here: pipe A's, and pipe B's.
const SCANOUTS: usize = 2;

/// The display card.
pub(crate) struct Display<'d> {
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
    /// The device and its aperture count, for a second surface made at a
    /// plug.
    device: Option<Device<Kernel>>,
    apertures: u32,
    /// `ferrix.intelgpu.modeset=on`, where the command line says it.
    written: Option<modeset::Written>,
    /// Pipe A's beam, where `FLUSH`es of the first scanout wait for its
    /// blank ([`vblank`]); `None` copies at once.
    beam: Option<vblank::Beam>,
    /// The FLUSH waiting for the blank, and when the loop comes back for it.
    pending: Option<vblank::Pending>,
    flip_at: u64,
    /// When the hotplug poll is due, in milliseconds; `u64::MAX` for never.
    hotplug_at: u64,
    tally: vblank::Tally,
    /// The count of GPU writes to shared objects the last copy saw
    /// (`ferrix_intel_xelp::frontend::shared_writes`).
    shared_writes: u64,
    /// Copies evict their source until then: a GPU write submitted before
    /// one copy may land after it, and the lines that copy read would
    /// otherwise be read again.
    evict_until: u64,
    /// Each buffer slot's VMO, kept for the blitter to pin or find it.
    vmos: [Option<Vmo<Kernel>>; MAX_BUFFERS],
    /// The first scanout's copy on BCS0.
    blit: blit::State<'d>,
}

/// What the second scanout starts with.
struct Second {
    /// The command line's word, which a later surface is made under.
    written: Option<modeset::Written>,
    surface: Option<surface::Surface>,
    /// Its mode where it is lit now.
    size: Option<(u32, u32)>,
    /// DRM's connector type for it.
    connector: u32,
    watch: Option<hotplug::Watch>,
}

impl<'d> Display<'d> {
    /// Publish `card0`: the lent window found and mapped, the plane checked
    /// as one a copy can fill (linear XRGB8888, the window large enough),
    /// HELLO said and READY taken. `None`, said, when any of it is missing:
    /// the render node is served alone then, as before.
    pub(crate) fn publish(
        device: &'d Device<Kernel>,
        apertures: u32,
        location: u32,
        plane: Option<ScreenPlane>,
        port: &Port<Kernel>,
    ) -> Option<Display<'d>> {
        let Some(plane) = plane else {
            ferrix_rt::info!("intel: no card0: pipe A's plane is not lit");
            return None;
        };
        let (window, first, phys) = map_screen(device, apertures, plane)?;
        let control = device
            .display_control()
            .inspect_err(|error| {
                ferrix_rt::err!("intel: no card0: no display control ({error:?})");
            })
            .ok()?;
        let mut regs = regs::Regs::map(device, apertures)
            .inspect_err(|why| ferrix_rt::warn!("intel: card0: no hotplug: {why}"))
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
                modeset::Asked::On(written) => {
                    second = prepare_second(written, device, apertures, regs);
                }
                modeset::Asked::Nothing => {}
            }
        }
        let first_watch = regs.as_mut().and_then(hotplug::Watch::start);
        let beam = beam(regs.as_mut(), first.height as u32);
        let (scanouts, modes, connectors) = hello_parts(&first, plane.connector, second.as_ref());
        let running = timing(first.width as u16, first.height as u16);
        if let Err(why) = introduce(
            &control, port, location, scanouts, modes, connectors, running,
        ) {
            ferrix_rt::warn!("intel: no card0: {why}");
            return None;
        }
        if control
            .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_DISPLAY)
            .is_err()
        {
            ferrix_rt::err!("intel: no card0: the display control could not be waited on");
            return None;
        }
        ferrix_rt::info!(
            "intel: card0 published: copies frames into the lit {}x{} plane at {phys:#x} ({} bytes a line, write-combining){}",
            first.width,
            first.height,
            first.pitch,
            if scanouts == 2 {
                ", and a second scanout"
            } else {
                ""
            }
        );
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
            device: None,
            apertures: 0,
            written: None,
            beam,
            pending: None,
            flip_at: u64::MAX,
            hotplug_at: u64::MAX,
            tally: vblank::Tally::default(),
            shared_writes: 0,
            evict_until: 0,
            vmos: [const { None }; MAX_BUFFERS],
            blit: blit::State::new(plane, device),
        };
        if let Some(second) = second {
            if let (Some(size), Some(surface)) = (second.size, second.surface.as_ref()) {
                card.outputs[1] = Some(Output::surface(surface, size));
            }
            card.surface = second.surface;
            card.watches[1] = second.watch;
            // Kept to make the surface at a later plug where it could not be
            // made now.
            card.device = device.duplicate(Requested::Same).ok();
            card.apertures = apertures;
            card.written = second.written;
        }
        if let Err(why) = card.announce() {
            ferrix_rt::warn!("intel: no card0: {why}");
            return None;
        }
        if card.watches.iter().any(Option::is_some) {
            card.hotplug_at = now_ms();
            card.rearm();
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
            ferrix_rt::info!("intel: card0: no second port: pipe A drives no combo port");
            None
        }
    }
}

/// The second scanout under `ferrix.intelgpu.modeset=on`: the combo port
/// pipe A does not drive -- or, where the firmware runs pipe B already
/// (both ports cloned onto one surface), pipe B's -- its own surface, and
/// pipe B lit on it now where a monitor is there, or pointed at it where
/// the firmware runs it. A surface that cannot be made leaves the scanout
/// there and dark, its port watched, to be tried again at a plug. `None`,
/// said, only where pipe A drives no combo port.
fn prepare_second(
    written: modeset::Written,
    device: &Device<Kernel>,
    apertures: u32,
    regs: &mut regs::Regs,
) -> Option<Second> {
    let cloned = light::running(regs, 1);
    let port = match cloned {
        Some(running) => running.port,
        None => dark_port(regs)?,
    };
    let surface = surface::Surface::make(written, device, apertures, regs);
    let mut size = None;
    if let (Some(running), Some(surface)) = (cloned, surface.as_ref()) {
        ferrix_rt::info!(
            "intel: card0: the firmware runs pipe B on port {} at {}x{}: it gets a surface of its own",
            running.port.name(),
            running.size.0,
            running.size.1
        );
        if modeset::repoint(regs, 1, surface.ggtt, running.size) {
            size = Some(running.size);
        }
    } else if let Some(surface) = surface.as_ref()
        && port
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
        written: Some(written),
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
        ferrix_rt::warn!(
            "intel: no card0: the lit plane is not linear XRGB8888 (PLANE_CTL {:#010x})",
            plane.ctl
        );
        return None;
    }
    let find = |bar: u8| {
        (0..apertures as usize)
            .filter_map(|index| device.aperture(index).ok())
            .find(|aperture| aperture.bar == bar)
    };
    let Some(window) = find(APERTURE_NOT_BAR) else {
        ferrix_rt::warn!(
            "intel: no card0: no lent framebuffer (K4 lends it under ferrix.efifb with ferrix.intelgpu)"
        );
        return None;
    };
    // The window must be the pixels the plane scans: BAR 2 (the GGTT's
    // aperture) at the plane's GGTT offset.
    let gmadr = find(2);
    if gmadr.is_some_and(|bar2| bar2.phys + u64::from(plane.ggtt) != window.phys) {
        ferrix_rt::warn!(
            "intel: no card0: the lent window at {:#x} is not the plane's surface (BAR 2 {:#x} + GGTT {:#x})",
            window.phys,
            gmadr.map_or(0, |b| b.phys),
            plane.ggtt
        );
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
        ferrix_rt::warn!(
            "intel: no card0: the plane {width}x{height} (pitch {pitch}) does not fit the lent window ({len:#x} bytes)"
        );
        return None;
    }
    let mapping = device
        .io_mapping(IoMappingSpec {
            phys: window.phys,
            len: window.len,
        })
        .inspect_err(|error| {
            ferrix_rt::err!("intel: no card0: the lent window refused ({error:?})");
        })
        .ok()?;
    let base = mapping
        .map_combining(None)
        .inspect_err(|error| {
            ferrix_rt::err!(
                "intel: no card0: the window's write-combining mapping refused ({error:?})"
            );
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

/// Nanoseconds of `CLOCK_MONOTONIC`.
fn now_ns() -> u64 {
    ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
}

/// The loop's wake-up at `at_ns`.
fn schedule(at_ns: u64) {
    crate::serve::WAKE_AT.store(at_ns.max(1), Ordering::Relaxed);
}

/// Pipe A's beam, said, unless `ferrix.intelgpu.vblank=off` asks to copy
/// each frame at once or the transcoder's numbers make no sense.
fn beam(regs: Option<&mut regs::Regs>, height: u32) -> Option<vblank::Beam> {
    if vblank_off() {
        ferrix_rt::info!("intel: card0: copies at once (ferrix.intelgpu.vblank=off)");
        return None;
    }
    let beam = regs.and_then(|regs| vblank::Beam::read(regs, height, REFRESH_HZ, now_ns()));
    match beam.map(|beam| beam.timing()) {
        Some((vactive, vtotal, frame_ns)) => ferrix_rt::info!(
            "intel: card0: flips wait for pipe A's blank (lines {vactive} of {vtotal}, {frame_ns} ns a refresh)"
        ),
        None => ferrix_rt::info!("intel: card0: no beam to wait for; frames are copied at once"),
    }
    beam
}

/// `ferrix.intelgpu.vblank=off`: copy each frame the moment it comes, as
/// before frames waited for the blank.
fn vblank_off() -> bool {
    let mut line = [0_u8; 4096];
    crate::steps::read_cmdline(&mut line).is_some_and(|length| {
        line.get(..length)
            .unwrap_or_default()
            .split(u8::is_ascii_whitespace)
            .any(|word| word == b"ferrix.intelgpu.vblank=off")
    })
}

impl Display<'_> {
    /// A packet under one of the card's keys ([`ours`]): the hotplug poll,
    /// or the control channel's. `false` when the card is gone.
    pub(crate) fn on_key(&mut self, key: u64, port: &Port<Kernel>, gpu: &mut dyn Gpu) -> bool {
        if key == KEY_HOTPLUG {
            if self.pending.is_some() && self.flip_due(gpu).is_err() {
                return false;
            }
            let alive = now_ms() < self.hotplug_at || self.poll_hotplug();
            self.rearm();
            return alive;
        }
        self.on_packet(port, gpu)
    }

    /// After the render node's loop serviced the back end: the blit that
    /// finished answered. `false` when the card is gone.
    pub(crate) fn on_service(&mut self, gpu: &mut dyn Gpu) -> bool {
        match self.settle_blit(gpu, false) {
            Ok(()) => true,
            Err(why) => {
                ferrix_rt::err!("intel: card0 stopped: {why}");
                false
            }
        }
    }

    /// Every copy on the processor from now on: nothing services the back
    /// end, so no blit would be seen to finish.
    pub(crate) fn copy_on_processor(&mut self) {
        self.blit = blit::State::Off;
    }

    /// The running blit, if any, answered: at once if `wait`, servicing
    /// the back end until it ends, else only if it has. A blit that did
    /// not end well is copied by the processor and the blitter turned off.
    fn settle_blit(&mut self, gpu: &mut dyn Gpu, wait: bool) -> Result<(), &'static str> {
        let Some(blitter) = self.blit.made() else {
            return Ok(());
        };
        let Some(inflight) = blitter.inflight else {
            return Ok(());
        };
        let done = if wait {
            blitter.wait(gpu)
        } else {
            gpu.completion(inflight.request).map(|how| {
                blitter.inflight = None;
                (inflight, how)
            })
        };
        let (inflight, how) = match done {
            Some(done) => done,
            None if wait => (inflight, Completion::Hung),
            None => return Ok(()),
        };
        let now = now_ns();
        if how == Completion::Ok {
            self.tally.blitted(now.saturating_sub(inflight.started));
        } else {
            ferrix_rt::warn!(
                "intel: card0: a blit ended {how:?}; frames are copied by the processor from now on"
            );
            self.blit = blit::State::Off;
            if let Some(Some(buffer)) = self.buffers.get(inflight.slot).copied() {
                if let Some(Some(output)) = self.outputs.first_mut() {
                    output.owed_whole = true;
                }
                self.flush(buffer.id, Rect::default());
            }
        }
        self.tally
            .flip(now, now.saturating_sub(inflight.at), inflight.torn);
        if let Some(done) = self.tally.report(now) {
            done.say();
        }
        self.reply(Message::Flipped {
            sequence: inflight.sequence,
            status: Status::Ok,
        })
    }

    /// The FLUSH copied by the blitter, if it can take it: `true` when its
    /// blit was submitted, and FLIPPED is said once it ends.
    fn blit_flush(&mut self, pending: vblank::Pending, torn: bool, gpu: &mut dyn Gpu) -> bool {
        let Some(slot) = self
            .buffers
            .iter()
            .position(|b| b.is_some_and(|b| b.id == pending.buffer))
        else {
            return false;
        };
        let Some(Some(buffer)) = self.buffers.get(slot).copied() else {
            return false;
        };
        let [Some(first), second] = &mut self.outputs else {
            return false;
        };
        if second
            .as_ref()
            .is_some_and(|o| o.shown.is_some_and(|(id, _)| id == pending.buffer))
        {
            return false;
        }
        let Some((id, whole)) = first.shown else {
            return false;
        };
        if id != pending.buffer || first.scaler.is_some() {
            return false;
        }
        let changed = if first.owed_whole {
            whole
        } else {
            pending.rect
        };
        let Some(area) = area(&buffer, whole, changed, first) else {
            return false;
        };
        let Some(vmo) = self.vmos.get(slot).and_then(Option::as_ref) else {
            return false;
        };
        let Some(blitter) = self.blit.get(gpu) else {
            return false;
        };
        if !blitter.prepare(
            slot,
            vmo,
            buffer.offset,
            buffer.length as u64,
            buffer.stride,
            gpu,
        ) {
            return false;
        }
        if blitter.written_by_processor(slot) {
            // `clflush` writes a dirty line back as it evicts it.
            write_back(&buffer, &area);
        }
        let Ok(request) = blitter.submit(slot, buffer.stride, &area, gpu) else {
            return false;
        };
        blitter.inflight = Some(blit::Inflight {
            request,
            slot,
            sequence: pending.sequence,
            at: pending.at,
            started: now_ns(),
            torn,
        });
        first.owed_whole = false;
        self.flushes += 1;
        if self.flushes == 1 {
            ferrix_rt::info!("intel: card0: the first frame is on the screen");
        }
        true
    }

    /// The loop's wake-up: the earlier of the hotplug poll and the blank a
    /// FLUSH waits for, a spin's length before it.
    fn rearm(&self) {
        let hotplug = self.hotplug_at.saturating_mul(1_000_000);
        let flip = self.pending.map_or(u64::MAX, |_| self.flip_at);
        let at = hotplug.min(flip);
        if at != u64::MAX {
            schedule(at);
        }
    }

    /// The FLUSH waiting for the blank, copied and answered if the blank
    /// is near enough to spin for; otherwise the wake-up set for it.
    fn flip_due(&mut self, gpu: &mut dyn Gpu) -> Result<(), &'static str> {
        let (Some(_), Some(beam), Some(regs)) =
            (self.pending, self.beam.as_mut(), self.regs.as_mut())
        else {
            return self.finish_pending(false, gpu);
        };
        let mut left = beam.until_blank(regs, now_ns());
        if left > vblank::SPIN_NS {
            self.flip_at = now_ns() + left - vblank::SPIN_NS;
            return Ok(());
        }
        // Spin the last stretch: a wake a little late would miss a blank
        // of under a millisecond.
        let give_up = now_ns() + vblank::SPIN_NS + 500_000;
        while left > 0 && now_ns() < give_up {
            core::hint::spin_loop();
            left = beam.until_blank(regs, now_ns());
        }
        self.finish_pending(left > 0, gpu)
    }

    /// Copy the FLUSH waiting for the blank and answer it; `torn` when it
    /// is copied outside the blank. A blit still running is answered
    /// first; a FLUSH the blitter takes is answered when its blit ends.
    fn finish_pending(&mut self, torn: bool, gpu: &mut dyn Gpu) -> Result<(), &'static str> {
        if self.pending.is_none() {
            return Ok(());
        }
        self.settle_blit(gpu, true)?;
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        if self.blit_flush(pending, torn, gpu) {
            return Ok(());
        }
        self.flush(pending.buffer, pending.rect);
        let now = now_ns();
        self.tally.flip(now, now.saturating_sub(pending.at), torn);
        if let Some(done) = self.tally.report(now) {
            done.say();
        }
        self.reply(Message::Flipped {
            sequence: pending.sequence,
            status: Status::Ok,
        })
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
        self.hotplug_at = next;
        for (scanout, change) in changes.into_iter().enumerate() {
            if let Some(connected) = change
                && let Err(why) = self.changed(scanout, connected)
            {
                ferrix_rt::err!("intel: card0 stopped: {why}");
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
        if self.surface.is_none()
            && let (Some(device), Some(written)) = (self.device.as_ref(), self.written)
        {
            self.surface = surface::Surface::make(written, device, self.apertures, regs);
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
    pub(crate) fn on_packet(&mut self, port: &Port<Kernel>, gpu: &mut dyn Gpu) -> bool {
        match self.take_messages(gpu) {
            Ok(true) => self
                .control
                .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_DISPLAY)
                .is_ok(),
            Ok(false) => {
                ferrix_rt::warn!("intel: card0: the display core closed its end");
                false
            }
            Err(why) => {
                ferrix_rt::err!("intel: card0 stopped: {why}");
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
    fn take_messages(&mut self, gpu: &mut dyn Gpu) -> Result<bool, &'static str> {
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
                    // Whatever comes next is acted on after the FLUSH that
                    // waits for the blank, as it was sent after it: that
                    // one is copied now.
                    if self.pending.is_some() {
                        self.finish_pending(true, gpu)?;
                    }
                    // What changes the plane or lets a buffer go waits for
                    // the blit that may write or read it.
                    if matches!(
                        message,
                        Message::Stop | Message::Detach { .. } | Message::SetMode { .. }
                    ) {
                        self.settle_blit(gpu, true)?;
                    }
                    match message {
                        Message::Stop if vmo.is_none() => {
                            let _ = self.reply(Message::Stopped);
                            return Ok(false);
                        }
                        Message::Attach(attach) => {
                            let vmo = vmo.ok_or("an ATTACH without its VMO")?;
                            let status = self.attach(&attach, vmo);
                            self.reply(Message::Attached {
                                buffer: attach.buffer,
                                status,
                            })?;
                        }
                        _ if vmo.is_some() => {
                            return Err("a handle on a message that carries none");
                        }
                        other => self.act(other, gpu)?,
                    }
                }
                Err(ReadError::Failed(Error::PeerClosed)) => return Ok(false),
                Err(ReadError::Failed(Error::ShouldWait)) => return Ok(true),
                Err(_) => return Err("the display control failed"),
            }
        }
    }

    fn act(&mut self, message: Message, gpu: &mut dyn Gpu) -> Result<(), &'static str> {
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
                // The first scanout's frame waits for pipe A's blank
                // ([`vblank`]); another scanout's is copied at once.
                let shown_first = self
                    .outputs
                    .first()
                    .and_then(|output| output.as_ref())
                    .and_then(|output| output.shown)
                    .is_some_and(|(id, _)| id == buffer);
                self.pending = Some(vblank::Pending {
                    buffer,
                    sequence,
                    rect,
                    at: now_ns(),
                });
                if shown_first && self.beam.is_some() {
                    self.flip_due(gpu)?;
                    self.rearm();
                    Ok(())
                } else {
                    self.finish_pending(false, gpu)
                }
            }
            Message::Detach { buffer } => {
                let status = self.detach(buffer, gpu);
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
                ferrix_rt::info!(
                    "intel: card0: scanout {scanout} mode {}x{}{}",
                    size.0,
                    size.1,
                    if output.scaler.is_some() {
                        ", scaled to the plane"
                    } else {
                        ""
                    }
                );
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
        let evicting = self.evicting();
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
            let started = now_ns();
            let evicted = copy(output, &source, changed, evicting);
            self.tally.copied(now_ns().saturating_sub(started), evicted);
            self.flushes += 1;
            if self.flushes == 1 {
                ferrix_rt::info!("intel: card0: the first frame is on the screen");
            }
        }
    }

    /// Whether a copy now evicts its source first: a GPU wrote an object
    /// shared with other programs within [`EVICT_WINDOW_NS`]. A desktop
    /// drawn in software with no GPU client copies as before, at no cost.
    fn evicting(&mut self) -> bool {
        let writes = ferrix_intel_xelp::frontend::shared_writes();
        let now = now_ns();
        if writes != self.shared_writes {
            self.shared_writes = writes;
            self.evict_until = now.saturating_add(EVICT_WINDOW_NS);
        }
        now < self.evict_until
    }

    fn attach(&mut self, attach: &Attach, vmo: Vmo<Kernel>) -> Status {
        let Ok(vmo_bytes) = vmo.size() else {
            return Status::Invalid;
        };
        if attach.validate(vmo_bytes).is_err() || self.buffer(attach.buffer).is_some() {
            return Status::Invalid;
        }
        let Some(index) = self.buffers.iter().position(Option::is_none) else {
            return Status::OutOfMemory;
        };
        let Ok(length) = usize::try_from(attach.length) else {
            return Status::Invalid;
        };
        let Ok(base) = vmo.map(None, length, Protection::Read, attach.offset) else {
            return Status::OutOfMemory;
        };
        if let Some(kept) = self.vmos.get_mut(index) {
            *kept = Some(vmo);
        }
        let Some(slot) = self.buffers.get_mut(index) else {
            return Status::OutOfMemory;
        };
        *slot = Some(Buffer {
            id: attach.buffer,
            offset: attach.offset,
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

    fn detach(&mut self, id: u32, gpu: &mut dyn Gpu) -> Status {
        let Some(index) = self
            .buffers
            .iter()
            .position(|slot| slot.is_some_and(|b| b.id == id))
        else {
            return Status::Invalid;
        };
        let Some(buffer) = self.buffers.get_mut(index).and_then(Option::take) else {
            return Status::Invalid;
        };
        // No blit runs (the caller settled it): the mapping and pin go
        // before the core may give the pages back.
        if let Some(blitter) = self.blit.made() {
            blitter.forget(index, gpu);
        }
        if let Some(kept) = self.vmos.get_mut(index) {
            *kept = None;
        }
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

/// Evict `changed` of `buffer` from the processor's caches before a copy
/// reads it, while a GPU writes shared objects (`Display::evicting`).
///
/// The buffer may be a GPU's frame: hyprix drawing on iris or a Vulkan
/// helper on anv, or a game's buffer shown directly. Mesa writes an object
/// it imported or exported with uncached MOCS, past the LLC, so a line this
/// process read from the buffer for an earlier frame stays in its cache and
/// is read again, stale: on 2026-10-28 hyprix's frames drawn on iris showed
/// on the N150 as the previous frames' pixels, with the pointer leaving a
/// trail. Evicting the lines first sends the copy's reads to memory. A line
/// the processor wrote (a compositor's dumb buffer) is written back by the
/// same `clflush`, so a frame drawn in software reads the same pixels.
fn evict(buffer: &Buffer, changed: Rect) {
    let Some((left, right, top, bottom)) = source_span(buffer, changed) else {
        return;
    };
    let bytes = (right - left) * PIXEL;
    for row in top..bottom {
        let from = row * buffer.stride + left * PIXEL;
        if from + bytes > buffer.length {
            break;
        }
        crate::arch::evict_range(buffer.base + from, bytes);
    }
    crate::arch::fence();
}

/// Write `area` of `buffer` back from the processor's caches before the
/// blitter reads it past them.
fn write_back(buffer: &Buffer, area: &Area) {
    let bytes = area.columns * PIXEL;
    for row in area.source_y..area.source_y + area.rows {
        let from = row * buffer.stride + area.source_x * PIXEL;
        if from + bytes > buffer.length {
            break;
        }
        crate::arch::evict_range(buffer.base + from, bytes);
    }
    crate::arch::fence();
}

/// The columns and rows of `buffer` inside `changed`, ends exclusive, or
/// `None` when that is nothing.
fn source_span(buffer: &Buffer, changed: Rect) -> Option<(usize, usize, usize, usize)> {
    let left = (changed.x as usize).min(buffer.width);
    let right = (changed.x as usize)
        .saturating_add(changed.width as usize)
        .min(buffer.width);
    let top = (changed.y as usize).min(buffer.height);
    let bottom = (changed.y as usize)
        .saturating_add(changed.height as usize)
        .min(buffer.height);
    (left < right && top < bottom).then_some((left, right, top, bottom))
}

/// `changed` of `buffer`, clipped as efifb clips it, copied into the
/// plane's memory, then the barrier that makes it shown.
fn copy(screen: &Output, buffer: &Buffer, changed: Rect, evicting: bool) -> u64 {
    let Some((_, shown)) = screen.shown else {
        return 0;
    };
    let started = now_ns();
    if evicting {
        evict(buffer, changed);
    }
    let evicted = now_ns().saturating_sub(started);
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
    evicted
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
        ferrix_rt::err!("intel: card0: the EDID could not be written");
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
    ferrix_rt::info!(
        "intel: card0: scanout {scanout}: {} modes listed, the smaller ones shown scaled",
        list.len()
    );
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
