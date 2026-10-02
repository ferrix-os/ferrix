//! Turning what the clients have committed into one frame.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use compositor_layout::Rect;
use compositor_layout::{MonitorLayout, WindowId};
use compositor_render::timing::{Phase, Timer, timed};
use compositor_render::{
    Backdrop, Canvas, Damage, Format, LayerFrame, Painter, Style, Surface, Target, Transform,
    render_onto,
};
use compositor_wire::ObjectId;

use crate::backend::Backend;
use crate::state::Slot;

/// Where a frame is drawn: the canvas, the screen it goes to, where the
/// monitor is in the space every window's rectangle is in, and how a window
/// is drawn.
#[derive(Debug)]
pub struct Output<'a> {
    /// The canvas the frame is composed on.
    pub canvas: &'a mut Canvas,
    /// What is behind the windows and the blur of it, kept from frame to
    /// frame so that a tiled window's blur is a copy: `crate::damage` says
    /// what that does to a frame's damage.
    pub backdrop: &'a mut Backdrop,
    /// The GPU's canvas and backdrop, when the frame is drawn there rather
    /// than on the two above.
    pub gpu: Option<&'a mut Gpu>,
    /// The screen it is shown on.
    pub backend: &'a mut dyn Backend,
    /// Where the monitor is in the global space.
    pub origin: (i64, i64),
    /// The colours a window is drawn with, and what a `windowrule` changed
    /// for one of them.
    pub style: &'a Style,
    /// What each window is drawn with where a rule said something else.
    pub styles: &'a BTreeMap<WindowId, compositor_render::WindowStyle>,
    /// How many buffer pixels one logical pixel is: `monitor = ..., 2`.
    /// Everything above the renderer works in logical pixels, and this is
    /// where they become the screen's own.
    pub scale: f64,
    /// How the monitor is turned: `monitor = ..., transform, 1`.
    ///
    /// The frame is composed upright on a canvas the size of the monitor as
    /// it is read, and turned only here, as [`shown`] puts it in the
    /// screen's buffer; `present` is in the canvas's pixels and becomes the
    /// buffer's there too.
    pub transform: Transform,
    /// Where the pointer is and what it looks like, or `None` for a screen
    /// with no pointer on it.
    pub cursor: Option<Cursor>,
    /// The ramps a night-light set on this screen, if one did.
    pub gamma: Option<Gamma>,
    /// The surface a drag is carrying, drawn at the pointer.
    ///
    /// Its rectangle's position is where the pointer is; the size is the
    /// surface's own, because a drag icon is whatever the client drew and
    /// not something the compositor sizes.
    pub drag_icon: Option<Placed>,
    /// What to copy from the canvas to the screen, which is not always what
    /// was drawn on the canvas: a backend with two buffers is drawing into
    /// the one that holds the frame before last, so it is owed that frame's
    /// damage as well as this one's. `crate::damage` says the rest.
    pub present: Damage,
    /// `debug:overlay`'s counter, on the one screen it is drawn on.
    pub overlay: Option<&'a crate::overlay::Picture>,
    /// How long drawing the counter took, which the drawing fills in: what
    /// the counter's "Rendertime (No Overlay)" takes away.
    pub overlay_took: Duration,
    /// What the screen's buffer was given, in its own pixels, for the
    /// flip, which the drawing fills in and leaves to the caller
    /// ([`flip`]): the clients are told their frame is done before it, as
    /// the flip waits for the host to show the frame and nothing a client
    /// draws next depends on that.
    pub flip: Option<Damage>,
}

/// A frame drawn on a GPU: the canvas, and what is kept behind the windows.
///
/// The device is whichever there was -- the render node in a guest, the
/// test server on a host -- and nothing here knows which.
#[derive(Debug)]
pub struct Gpu {
    /// The canvas the frame is composed on.
    pub canvas: compositor_render::gpu::Canvas<Box<dyn compositor_virgl::Device>>,
    /// What is behind the windows, and the blur of it.
    pub backdrop: compositor_render::gpu::Backdrop,
}

/// What a frame that could not be drawn on the GPU says first, so that the
/// loop can tell a GPU that has gone from a screen that has: the first is
/// answered by drawing in software from then on.
pub const GPU_FAILED: &str = "the GPU: ";

/// A night-light's three ramps, one entry a level.
///
/// `zwlr_gamma_control_v1` hands the compositor a descriptor holding three
/// tables of sixteen-bit entries -- red, then green, then blue -- and every
/// level a pixel can have is looked up in its channel's table on the way to
/// the screen. That is what `gammastep` and `hyprsunset` do to make an
/// evening screen warmer.
///
/// A real compositor hands the table to the connector and the hardware does
/// the lookup. This one has no such hardware -- the screen is memory -- so
/// the lookup is done here, once a frame, over the pixels that were drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gamma {
    /// One byte a level, taken from the top half of the protocol's
    /// sixteen: the screen is eight bits a channel.
    ramps: [[u8; SIZE]; 3],
}

/// How many entries each ramp has, which is what the client was told.
const SIZE: usize = 256;

impl Gamma {
    /// Read the three ramps off a descriptor, or `None` if they are not
    /// there.
    ///
    /// The descriptor is this compositor's once the client has sent it, and
    /// is closed here whatever it held.
    #[must_use]
    pub fn read(fd: compositor_wire::Fd) -> Option<Self> {
        use std::io::Read as _;
        use std::os::fd::FromRawFd as _;
        #[expect(
            unsafe_code,
            reason = "AUDIT: the descriptor arrived on this compositor's own socket and is claimed; File takes it and closes it"
        )]
        // SAFETY: a descriptor this process received and owns, claimed from
        // the connection so nothing else will close it.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd.0) };
        let mut bytes = [0u8; SIZE * 3 * 2];
        file.read_exact(&mut bytes).ok()?;
        let mut ramps = [[0u8; SIZE]; 3];
        for (channel, ramp) in ramps.iter_mut().enumerate() {
            for (level, entry) in ramp.iter_mut().enumerate() {
                // Little-endian sixteen-bit, which is what a client writes
                // from a `uint16_t` array; the screen keeps the top byte.
                let at = (channel * SIZE + level) * 2 + 1;
                *entry = bytes.get(at).copied().unwrap_or(0);
            }
        }
        Some(Self { ramps })
    }

    /// Put one screen's pixels through the ramps, in place, over the part
    /// of it this frame copied from the canvas.
    ///
    /// Only that part: the ramps are applied to the screen's buffer rather
    /// than to the canvas, so a pixel the frame did not copy has been
    /// through them already and putting it through a second time would
    /// warm it twice.
    ///
    /// The buffer is `XRGB8888` or `ARGB8888`; either way the three colour
    /// bytes are the low three of each little-endian word and the fourth is
    /// left alone.
    pub fn apply(&self, buffer: &mut [u8], stride: u32, damage: &Damage) {
        let stride = stride as usize;
        for rect in damage.rects() {
            let (Ok(left), Ok(width)) = (usize::try_from(rect.x), usize::try_from(rect.width))
            else {
                continue;
            };
            for y in rect.y..rect.bottom() {
                let Ok(row) = usize::try_from(y) else {
                    continue;
                };
                let start = row
                    .saturating_mul(stride)
                    .saturating_add(left.saturating_mul(4));
                let end = start.saturating_add(width.saturating_mul(4));
                let Some(span) = buffer.get_mut(start..end) else {
                    continue;
                };
                self.rows(span);
            }
        }
    }

    /// One run of pixels through the ramps.
    fn rows(&self, span: &mut [u8]) {
        for pixel in span.chunks_exact_mut(4) {
            for (at, channel) in [(2usize, 0usize), (1, 1), (0, 2)] {
                let Some(ramp) = self.ramps.get(channel) else {
                    continue;
                };
                let Some(byte) = pixel.get_mut(at) else {
                    continue;
                };
                if let Some(mapped) = ramp.get(usize::from(*byte)) {
                    *byte = *mapped;
                }
            }
        }
    }
}

/// The pointer, as the frame draws it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    /// Where its hotspot is, in the space every window's rectangle is in.
    pub at: (i64, i64),
    /// The client's own cursor surface and the hotspot inside it, or `None`
    /// for the compositor's built-in arrow.
    pub surface: Option<(usize, ObjectId, (i32, i32))>,
    /// Whether it is drawn at all: a client may ask for no pointer.
    pub shown: bool,
}

/// Where one layer surface is, and whose it is.
///
/// The compositor works the rectangle out from the protocol's anchor rules
/// each time the layer surfaces change; this is what the drawing needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placed {
    /// Which connection.
    pub client: usize,
    /// The `wl_surface` under the layer surface.
    pub surface: ObjectId,
    /// Where it goes, in the global space.
    pub rect: Rect,
    /// Whether it is drawn above the windows.
    pub above: bool,
    /// What its `layerrule` lines gave it.
    pub rules: LayerRules,
}

/// What a surface's `layerrule` lines came to, as the frame reads them.
///
/// A copy of the fields the drawing uses, rather than the whole of
/// `compositor_config::Layered`: a `Placed` is compared with the last pass's
/// and a `String` in it would allocate on every frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LayerRules {
    /// `blur`: what is behind it is blurred.
    pub blur: bool,
    /// `xray`: that blur is of the wallpaper -- everything behind the
    /// windows -- rather than of what is directly behind it.
    pub xray: bool,
    /// `dim_around`: everything behind it is darkened while it is up,
    /// which is what a launcher does to the desktop.
    pub dim_around: bool,
    /// `abovelock`: it is drawn over the session lock.
    pub above_lock: bool,
    /// `noscreenshare`: a screenshot leaves it out.
    pub no_screen_share: bool,
    /// `order`: where it goes among its own layer's surfaces, a higher
    /// number nearer the top.
    pub order: i64,
}

/// Which client and which surface a window's pixels come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Source {
    /// Which connection.
    pub client: usize,
    /// The `wl_surface`.
    pub surface: ObjectId,
}

/// Draw every window of `output` into `backend`, taking each one's pixels
/// from the buffer its client committed.
///
/// A window whose client has committed nothing, or whose buffer is in a pool
/// that is not mapped, is drawn as its border and background alone -- which is
/// what a window that has not painted yet looks like, and is better than
/// refusing to draw the frame.
pub fn draw(
    target: &mut Output<'_>,
    output: &MonitorLayout,
    clients: &[Slot],
    sources: &BTreeMap<WindowId, Source>,
    layers: &[Placed],
    damage: &Damage,
) -> Result<(), String> {
    draw_windows(target, output, clients, sources, layers, damage)
}

/// Draw the screen while the session is locked: the lock's own surface and
/// nothing else.
///
/// `locked` is where the lock surface's pixels are, or `None` for a screen
/// the lock has not covered yet, and `over` the layer surfaces a
/// `layerrule = abovelock` asked to be drawn on top of it. A screen with no lock surface is drawn as
/// the background alone -- black, since the style's background is what the
/// compositor clears to -- because what it must *not* show is what was on
/// it before. That is the whole point of the protocol: the compositor stops
/// drawing the windows the moment the lock is taken, before the client has
/// drawn anything at all.
pub fn draw_locked(
    target: &mut Output<'_>,
    output: &MonitorLayout,
    clients: &[Slot],
    locked: Option<Placed>,
    over: &[Placed],
    damage: &Damage,
) -> Result<(), String> {
    let empty = MonitorLayout {
        windows: Vec::new(),
        ..output.clone()
    };
    let sources = BTreeMap::new();
    // The lock's own surface first, and then whatever a `layerrule =
    // abovelock` asked to be drawn over it -- an on-screen keyboard, which
    // is the whole reason that rule exists.
    let layers: Vec<Placed> = locked.into_iter().chain(over.iter().copied()).collect();
    draw_windows(target, &empty, clients, &sources, &layers, damage)
}

/// Draw a screen that `dpms off` has turned off: black, and nothing else.
///
/// Hyprland turns the connector itself off, which a virtual screen has no
/// equivalent of; what a person sees is the same, and what the compositor
/// must *not* show -- the windows that were there -- is gone either way.
pub fn draw_dark(target: &mut Output<'_>, damage: &Damage) -> Result<(), String> {
    let black = compositor_render::Color(0xff00_0000);
    match target.gpu.as_deref_mut() {
        Some(gpu) => Painter::clear(&mut gpu.canvas, black, damage),
        None => target.canvas.clear(black, damage),
    }
    shown(target)
}

/// Everything a frame is drawn from, whichever painter draws it.
struct Scene<'a> {
    output: &'a MonitorLayout,
    clients: &'a [Slot],
    sources: &'a BTreeMap<WindowId, Source>,
    layers: &'a [Placed],
    damage: &'a Damage,
    origin: (i64, i64),
    scale: f64,
    style: &'a Style,
    styles: &'a BTreeMap<WindowId, compositor_render::WindowStyle>,
    cursor: Option<Cursor>,
    drag_icon: Option<Placed>,
    overlay: Option<&'a crate::overlay::Picture>,
}

/// The two above, which differ only in what they are given to draw.
fn draw_windows(
    target: &mut Output<'_>,
    output: &MonitorLayout,
    clients: &[Slot],
    sources: &BTreeMap<WindowId, Source>,
    layers: &[Placed],
    damage: &Damage,
) -> Result<(), String> {
    let scene = Scene {
        output,
        clients,
        sources,
        layers,
        damage,
        origin: target.origin,
        scale: target.scale,
        style: target.style,
        styles: target.styles,
        cursor: target.cursor,
        drag_icon: target.drag_icon,
        overlay: target.overlay,
    };
    // The same frame on whichever painter there is: what differs is where
    // its pixels are when it is done, which is `shown`'s business.
    target.overlay_took = match target.gpu.as_deref_mut() {
        Some(gpu) => composed(&mut gpu.canvas, &mut gpu.backdrop, &scene),
        None => composed(&mut *target.canvas, &mut *target.backdrop, &scene),
    };
    shown(target)
}

/// Draw `scene` with `painter`: the windows and the layer surfaces, then
/// what a drag carries, then `debug:overlay`'s counter, then the pointer.
/// How long the counter took.
fn composed<P: Painter>(
    painter: &mut P,
    backdrop: &mut P::Backdrop,
    scene: &Scene<'_>,
) -> Duration {
    let (origin, scale, clients, damage) = (scene.origin, scene.scale, scene.clients, scene.damage);
    // The layout, the decorations and the layer surfaces are all in logical
    // pixels; a scaled monitor draws each of them as `scale` buffer pixels.
    let output = &compositor_render::scaled(scene.output, origin, scale);
    let style = &scene.style.at_scale(scale);
    // Gather every window's pixels first: `render` takes them all at once, so
    // each borrow of a mapping has to live as long as the call.
    let mut surfaces: BTreeMap<WindowId, Surface<'_>> = BTreeMap::new();
    for placed in &output.windows {
        let Some(source) = scene.sources.get(&placed.window) else {
            continue;
        };
        let Some(slot) = clients.get(source.client) else {
            continue;
        };
        // The window, without the shadows its client drew around it:
        // what is stretched to the rectangle is the part the client said
        // is the window, so a window whose tile is its own size is drawn
        // pixel for pixel.
        let window = pixels(slot, source.surface).map(|surface| {
            window_crop(slot.client(), source.surface)
                .and_then(|crop| crop.of(&surface))
                .unwrap_or(surface)
        });
        if let Some(surface) = window {
            let _ = surfaces.insert(placed.window, surface);
        }
    }

    // The bars and wallpapers, in the order they were made, which is the
    // order they were placed in.
    let drawn: Vec<LayerFrame<'_>> = scene
        .layers
        .iter()
        .map(|placed| LayerFrame {
            rect: scale_rect(placed.rect, origin, scale),
            above: placed.above,
            // A popup, without the shadow its client drew around it, as a
            // window is; a layer surface has no xdg geometry and is whole.
            surface: clients.get(placed.client).and_then(|slot| {
                let surface = pixels(slot, placed.surface)?;
                Some(
                    window_crop(slot.client(), placed.surface)
                        .and_then(|crop| crop.of(&surface))
                        .unwrap_or(surface),
                )
            }),
            dim_around: placed.rules.dim_around,
            blur: placed.rules.blur,
            xray: placed.rules.xray,
        })
        .collect();

    // Every window's rectangle is in the global space all monitors share;
    // the canvas is this monitor's, so the origin is where the monitor is.
    let styles = compositor_render::Styles {
        base: style,
        windows: scene.styles,
    };
    let _ = render_onto(
        painter,
        Some(backdrop),
        output,
        origin,
        &styles,
        &surfaces,
        &drawn,
        damage,
    );

    let _over = Timer::start(Phase::Over);
    // The drag icon under the pointer and over everything else: what a
    // drag looks like is a thing following the pointer, and a compositor
    // that drew it under a window would have a drag nobody can see.
    if let Some(icon) = scene.drag_icon
        && let Some(at) = drag_rect(clients, &icon, origin, scale)
        && let Some(slot) = clients.get(icon.client)
        && let Some(surface) = pixels(slot, icon.surface)
    {
        painter.composite(&surface, at, damage);
    }

    // The counter over everything the frame shows, as Hyprland draws it
    // after the windows, the layers and its own notifications. Timed, so
    // that the counter can say what a frame costs without it.
    let mut took = Duration::ZERO;
    if let Some(picture) = scene.overlay {
        let began = Instant::now();
        picture.paint(painter, style.blur.as_ref(), damage);
        took = began.elapsed();
    }

    // The pointer last, over everything: it is not a window, not a layer
    // surface and not part of the layout, and a compositor that drew it
    // under a menu would have a pointer nobody can follow.
    //
    // The arrow is kept here rather than made once and held, because it is
    // 24x24 and a frame that has to allocate it is a frame that has already
    // blurred a window.
    let arrow;
    if let Some(cursor) = scene.cursor.filter(|cursor| cursor.shown)
        && let Some(at) = cursor_rect(clients, &cursor, origin, scale)
    {
        let own = cursor.surface.and_then(|(client, surface, _)| {
            let slot = clients.get(client)?;
            pixels(slot, surface)
        });
        let surface = match own {
            Some(surface) => Some(surface),
            None => {
                arrow = compositor_render::cursor::arrow();
                compositor_render::cursor::surface(&arrow).ok()
            }
        };
        if let Some(surface) = surface {
            painter.composite(&surface, at, damage);
        }
    }
    took
}

/// Put the frame that was composed on the screen.
///
/// The software canvas copies what `present` names into the screen's
/// buffer. The GPU's frame is on the GPU, and until the screen can be
/// pointed at it there (`docs/GPU.md` §3.5, piece 6) it is fetched: the
/// same rectangles, read back and written where the software canvas would
/// have written them, so everything after this -- the night-light's ramps,
/// the flip, a screenshot -- sees the frame it always saw.
///
/// A turned monitor's frame is turned here and nowhere else: each pixel of
/// `present` goes where the transform sends it in the screen's buffer, and
/// the ramps and the card are given the damage in the buffer's pixels. An
/// upright one takes the first branch of each `match` below, which is the
/// whole of what it did before monitors could be turned.
fn shown(target: &mut Output<'_>) -> Result<(), String> {
    let Output {
        canvas,
        backend,
        gpu,
        gamma,
        present,
        transform,
        flip,
        ..
    } = target;
    let transform = *transform;
    let (width, height) = backend.size();
    let stride = backend.stride();
    // The frame's own size, which a quarter turn makes the buffer's
    // exchanged.
    let (across, down) = transform.size((width, height));
    let logical = (i64::from(across), i64::from(down));
    match gpu.as_deref_mut() {
        Some(gpu) => {
            gpu.canvas
                .finish()
                .map_err(|error| format!("{GPU_FAILED}{error}"))?;
            // A screen shown the texture the frame was drawn into needs
            // nothing fetched at all, which is the whole of what adopting it
            // saves. The night-light is the exception: its ramps are applied
            // to pixels on their way out, and the only pixels this
            // compositor can reach are the ones it fetches.
            if !backend.adopted() || gamma.is_some() {
                let bounds = Rect::new(0, 0, logical.0, logical.1);
                for &rect in present.clipped(bounds).rects() {
                    let pixels = gpu
                        .canvas
                        .read(rect)
                        .map_err(|error| format!("{GPU_FAILED}{error}"))?;
                    match transform {
                        Transform::Normal => fetched(backend.buffer(), stride, rect, &pixels),
                        turned => fetched_turned(
                            Target::new(backend.buffer(), width, height, stride),
                            turned,
                            logical,
                            rect,
                            &pixels,
                        )?,
                    }
                }
            }
        }
        None => {
            let _present = Timer::start(Phase::Present);
            let mut screen = Target::new(backend.buffer(), width, height, stride)
                .map_err(|error| format!("the screen's buffer is not one: {error:?}"))?;
            match transform {
                Transform::Normal => canvas.present(&mut screen, present),
                turned => canvas
                    .present_transformed(&mut screen, present, turned)
                    .map(|_| ()),
            }
            .map_err(|error| format!("the frame does not fit the screen: {error:?}"))?;
        }
    }
    // What the buffer was given, in its own pixels: the frame's damage for
    // an upright monitor, and that damage turned for one that is not.
    let turned_damage;
    let written: &Damage = match transform {
        Transform::Normal => present,
        turned => {
            let inside = present.clipped(Rect::new(0, 0, logical.0, logical.1));
            turned_damage = compositor_render::transform::damage(turned, logical, &inside);
            &turned_damage
        }
    };
    // The night-light's ramps, over what was drawn: on hardware the
    // connector does this, and here the compositor does -- the same picture
    // by a slower road.
    if let Some(gamma) = gamma {
        gamma.apply(backend.buffer(), stride, written);
    }
    *flip = Some(written.clone());
    Ok(())
}

/// Hand the screen's buffer to the card, `damage` of it changed: the flip
/// [`shown`] left for after the clients' frame callbacks.
pub(crate) fn flip(backend: &mut dyn Backend, damage: &Damage) -> Result<(), String> {
    timed(Phase::Flip, || backend.present(damage)).map_err(|error| error.to_string())
}

/// Write `pixels`, the rows of `rect` of a turned monitor's frame as the GPU
/// gave them back, into the screen's buffer, each where `transform` sends
/// it. `logical` is the whole frame's size, which the turn is about.
fn fetched_turned(
    screen: Result<Target<'_>, compositor_render::Error>,
    transform: Transform,
    logical: (i64, i64),
    rect: Rect,
    pixels: &[u8],
) -> Result<(), String> {
    let mut screen =
        screen.map_err(|error| format!("the screen's buffer is not one: {error:?}"))?;
    let row = usize::try_from(rect.width).unwrap_or(0).saturating_mul(4);
    compositor_render::transform::copy(
        transform,
        logical,
        pixels,
        row,
        (rect.x, rect.y),
        rect,
        &mut screen,
        (0, 0),
    );
    Ok(())
}

/// Write `pixels`, `rect`'s rows packed, into a screen's buffer at `rect`.
pub(crate) fn fetched(buffer: &mut [u8], stride: u32, rect: Rect, pixels: &[u8]) {
    let index = |value: i64| usize::try_from(value.max(0)).unwrap_or(0);
    let row_bytes = index(rect.width) * 4;
    if row_bytes == 0 {
        return;
    }
    let first = index(rect.y) * stride as usize + index(rect.x) * 4;
    let rows = buffer
        .get_mut(first..)
        .unwrap_or(&mut [])
        .chunks_mut(stride.max(1) as usize);
    for (into, from) in rows.zip(pixels.chunks_exact(row_bytes)) {
        if let Some(into) = into.get_mut(..row_bytes) {
            into.copy_from_slice(from);
        }
    }
}

/// One rectangle in the buffer pixels of a monitor at `scale`, grown away
/// from the monitor's own corner, as `compositor_render::scaled` grows a
/// window's.
fn scale_rect(rect: Rect, origin: (i64, i64), scale: f64) -> Rect {
    if (scale - 1.0).abs() < f64::EPSILON {
        return rect;
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a screen's pixels are far inside f64's exact range, and a scaled length is \
                  rounded to the pixel it lands on"
    )]
    let grow = |value: i64| (value as f64 * scale).round() as i64;
    let at = |value: i64, from: i64| from.saturating_add(grow(value.saturating_sub(from)));
    Rect::new(
        at(rect.x, origin.0),
        at(rect.y, origin.1),
        grow(rect.width),
        grow(rect.height),
    )
}

/// One rectangle of the global space in the screen's own pixels: scaled,
/// and moved into the canvas's own corner.
///
/// Everything above the renderer is in logical pixels in the space all the
/// monitors share; a canvas begins at its monitor's corner and counts in
/// the screen's own pixels, and this is the one conversion between them.
pub(crate) fn local(rect: Rect, origin: (i64, i64), scale: f64) -> Rect {
    let at = scale_rect(rect, origin, scale);
    Rect::new(
        at.x.saturating_sub(origin.0),
        at.y.saturating_sub(origin.1),
        at.width,
        at.height,
    )
}

/// The part of a toplevel's or a popup's buffer that is its window, as the
/// client said with `xdg_surface.set_window_geometry`, in the buffer's own
/// pixels.
///
/// Chrome's surface is its window and 10 pixels of shadow all round. Drawn
/// whole into its tile, the window came out a little smaller than it is and
/// its text resampled, and the pointer had to be scaled back through the
/// squeeze; Hyprland draws the window geometry into the tile, and so does
/// this, for a menu's shadow as for a window's. `None` -- the whole buffer is
/// the window -- for a surface that
/// never said, one scaled or turned by a viewport or a transform, where the
/// geometry and the buffer's pixels are not one scale apart, and a geometry
/// that is the whole buffer or does not fit inside it.
pub(crate) fn window_crop(client: &compositor_server::Client, surface: ObjectId) -> Option<Crop> {
    let (x, y, width, height) = client.window_geometry(surface)?;
    let state = &client.surface(surface)?.current;
    if state.viewport_size.is_some() || state.viewport_source.is_some() || state.transform != 0 {
        return None;
    }
    let buffer = client.buffer(state.buffer?)?;
    let scale = state.scale.max(1);
    let crop = Crop {
        x: u32::try_from(x.checked_mul(scale)?).ok()?,
        y: u32::try_from(y.checked_mul(scale)?).ok()?,
        width: u32::try_from(width.checked_mul(scale)?).ok()?,
        height: u32::try_from(height.checked_mul(scale)?).ok()?,
        scale: u32::try_from(scale).ok()?,
    };
    let (buffer_width, buffer_height) = (
        u32::try_from(buffer.width).ok()?,
        u32::try_from(buffer.height).ok()?,
    );
    let inside = crop.width > 0
        && crop.height > 0
        && crop.x.checked_add(crop.width)? <= buffer_width
        && crop.y.checked_add(crop.height)? <= buffer_height;
    let whole = (crop.x, crop.y, crop.width, crop.height) == (0, 0, buffer_width, buffer_height);
    (inside && !whole).then_some(crop)
}

/// A window's part of its buffer: [`window_crop`]'s answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Crop {
    /// Where the window starts in the buffer, in its pixels.
    pub x: u32,
    /// See [`Crop::x`].
    pub y: u32,
    /// How big the window is, in the buffer's pixels.
    pub width: u32,
    /// See [`Crop::width`].
    pub height: u32,
    /// The buffer's pixels per surface pixel.
    pub scale: u32,
}

impl Crop {
    /// The window's pixels out of the whole buffer's.
    pub(crate) fn of<'a>(&self, surface: &Surface<'a>) -> Option<Surface<'a>> {
        surface.cropped(self.x, self.y, self.width, self.height)
    }

    /// The window's part in surface coordinates: where it starts and how
    /// big it is.
    pub(crate) fn in_surface(&self) -> (f64, f64, f64, f64) {
        let scale = f64::from(self.scale);
        (
            f64::from(self.x) / scale,
            f64::from(self.y) / scale,
            f64::from(self.width) / scale,
            f64::from(self.height) / scale,
        )
    }
}

/// Whether a surface can be seen through, which is half of what decides
/// whether the renderer draws a blur behind it.
///
/// A format with alpha may be translucent anywhere; one without it never
/// is. A surface with nothing committed shows nothing, and nothing is
/// blurred behind it.
pub(crate) fn translucent(clients: &[Slot], client: usize, surface: ObjectId) -> bool {
    clients
        .get(client)
        .and_then(|slot| pixels(slot, surface))
        .is_some_and(|pixels| pixels.format() == Format::Argb8888)
}

/// Where the surface a drag is carrying is drawn, in the screen's own
/// pixels, or `None` when the client has drawn nothing to carry.
///
/// The size is the surface's own: a drag icon is whatever the client drew
/// and not something the compositor sizes.
pub(crate) fn drag_rect(
    clients: &[Slot],
    icon: &Placed,
    origin: (i64, i64),
    scale: f64,
) -> Option<Rect> {
    let slot = clients.get(icon.client)?;
    let surface = pixels(slot, icon.surface)?;
    Some(local(
        Rect::new(
            icon.rect.x,
            icon.rect.y,
            i64::from(surface.width()),
            i64::from(surface.height()),
        ),
        origin,
        scale,
    ))
}

/// Where the pointer's pixels go, in the screen's own pixels, or `None` for
/// a pointer that is not drawn at all.
///
/// The client's own cursor surface where it set one, and the built-in arrow
/// where it did not; the hotspot is what the position names, so the
/// rectangle begins that far above and left of it.
pub(crate) fn cursor_rect(
    clients: &[Slot],
    cursor: &Cursor,
    origin: (i64, i64),
    scale: f64,
) -> Option<Rect> {
    if !cursor.shown {
        return None;
    }
    let own = cursor.surface.and_then(|(client, surface, hotspot)| {
        let slot = clients.get(client)?;
        let pixels = pixels(slot, surface)?;
        Some(((pixels.width(), pixels.height()), hotspot))
    });
    let ((width, height), hotspot) = own.unwrap_or((
        (
            compositor_render::cursor::SIDE,
            compositor_render::cursor::SIDE,
        ),
        compositor_render::cursor::HOTSPOT,
    ));
    Some(local(
        Rect::new(
            cursor.at.0.saturating_sub(i64::from(hotspot.0)),
            cursor.at.1.saturating_sub(i64::from(hotspot.1)),
            i64::from(width),
            i64::from(height),
        ),
        origin,
        scale,
    ))
}

/// The pixels a surface is showing, if it is showing any.
pub(crate) fn pixels(slot: &Slot, surface: ObjectId) -> Option<Surface<'_>> {
    let (client, pools) = (slot.client(), slot.pools());
    // What the surface is called from frame to frame: the connection, which
    // a slot's place is not, and the `wl_surface` on it. A renderer that
    // keeps a surface's pixels somewhere of its own keeps them by this.
    let name = (slot.serial() << 32) | u64::from(surface.0);
    let state = client.surface(surface)?;
    let buffer = client.buffer(state.current.buffer?)?;
    // A `wp_single_pixel_buffer_v1` is in no pool: the colour is the
    // buffer, four bytes held on the buffer itself. A window drawn from one
    // is scaled to its rectangle like any other, so one pixel fills it.
    let bytes = match buffer.solid.as_ref() {
        Some(colour) => colour.as_slice(),
        None => {
            let mapping = pools.get(&buffer.pool)?;
            let (start, end) = buffer.range()?;
            // The client may have shrunk nothing -- a pool only grows --
            // but a mapping made before a resize is smaller than the pool
            // is now, so the range is checked against what is mapped rather
            // than against the pool.
            mapping.bytes().get(start..end)?
        }
    };
    let format = match buffer.format {
        compositor_server::Format::Argb8888 => Format::Argb8888,
        compositor_server::Format::Xrgb8888 => Format::Xrgb8888,
    };
    Surface::new(
        bytes,
        u32::try_from(buffer.width).ok()?,
        u32::try_from(buffer.height).ok()?,
        u32::try_from(buffer.stride).ok()?,
        format,
    )
    .ok()
    // A single-pixel buffer's four bytes are the buffer's own and may be
    // any colour next frame at the same size, under damage that is the
    // whole window: it is moved whole like anything else without a name.
    .map(|made| match buffer.solid {
        Some(_) => made,
        None => made.named(name),
    })
}
