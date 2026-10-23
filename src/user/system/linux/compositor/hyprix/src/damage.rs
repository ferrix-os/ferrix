//! What a frame has to redraw, and nothing else.
//!
//! Hyprland keeps a damage region for each monitor and adds to it as things
//! happen: `IHyprRenderer::damageSurface` for the pixels a client says it
//! drew, `damageWindow` for a window that moved -- the whole of its
//! bounding box, where it was and where it is -- and `damageMonitor` for
//! everything else (`src/render/Renderer.cpp`). This is the same region,
//! gathered in two halves that between them cover every pixel a frame can
//! change.
//!
//! The first half is what is *inside* a surface, which only its client can
//! change and only by committing: `wl_surface.damage` and `damage_buffer`
//! say which part, and those arrive here as [`Painted`] while the
//! connections are read.
//!
//! The second half is everything the compositor itself decides: where each
//! window is, which one has the focus, what a `windowrule` gave it, where
//! the bars and the menus are, where the pointer is, whether the session is
//! locked. There are some forty places in the loop that change one of
//! those, and a damage region built by instrumenting each of them is one
//! stale pixel away from a bug nobody can explain. So the *whole
//! description* a frame is drawn from is kept as a [`Plan`] and compared
//! with the last frame's, and what differs is damaged in both places --
//! which is what `damageWindow` does for a window that moved.
//!
//! Anything neither half can place damages the whole screen: a needless
//! redraw is only slow, and [`Told::everything`] is this module's
//! `damageMonitor`.
//!
//! # The blur reads more than it writes
//!
//! Then the region is grown once more, for the one thing a frame draws that
//! reads further than it writes: the blur behind a translucent surface.
//! There are two kinds ([`Blurred::live`]) and each is owed something else.
//!
//! A tiled window takes its blur from `compositor_render::Backdrop`, which
//! keeps what is behind the windows and the blur of it from frame to frame.
//! Nothing is drawn over that, so a strip of such a window can be redrawn
//! alone -- which is what makes a pointer crossing a translucent terminal,
//! or a letter typed into one, cost what it changes and not a blur of the
//! whole window. It is owed more only where what is *behind* the windows
//! changed, because a changed pixel shows in the blur a kernel's reach
//! away: [`Plan::behind_reaches`].
//!
//! Everything else blurs the frame as it stands, and is owed a whole
//! redraw whenever the damage touches it: [`Plan::blurs_whole`] says why
//! there is no cheaper way to be exact about that.
//!
//! # Two buffers, two frames
//!
//! The canvas keeps the whole picture from frame to frame -- that is what
//! makes redrawing a strip of it legal -- but the screen may not. The card
//! backend draws into two buffers in turn, so the one being drawn into
//! holds the frame from *two* frames ago, and copying only this frame's
//! damage into it would leave the frame before's changes off the screen.
//! [`Frame::screen`] is therefore this frame's damage and the last one's,
//! which is Hyprland's damage ring at a buffer age of two
//! (`CMonitor::addDamage`).
//!
//! A screen with one buffer is owed only this frame's: `Backend::age` says
//! which a screen is, and a virtio-gpu, which is sent what changed rather
//! than flipped to, is the one that has one.

use std::collections::BTreeMap;

use compositor_layout::{MonitorLayout, Rect, WindowId};
use compositor_render::{Damage, Style, WindowStyle, outer};
use compositor_wire::ObjectId;

use crate::frame::{Cursor, Placed};

/// How many commits are kept before a pass gives up and damages everything.
///
/// A frame is drawn on the pass a commit arrives, so this is only reached
/// when something is holding frames back; past it the region costs more to
/// work out than the screen costs to redraw.
const MOST: usize = 64;

/// What one client said about its own pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Painted {
    /// Which connection.
    pub(crate) client: usize,
    /// The `wl_surface` it committed.
    pub(crate) surface: ObjectId,
    /// The buffer's size in its own pixels, or `None` where the surface
    /// shows none.
    pub(crate) buffer: Option<(i64, i64)>,
    /// What it damaged, in the buffer's own pixels. Empty is the whole
    /// surface: a commit that named no damage at all has changed something
    /// the compositor cannot place inside it.
    pub(crate) rects: Vec<Rect>,
}

impl Painted {
    /// The parts of `into` this commit changed, in the screen's own pixels.
    ///
    /// The whole of it for a commit that named no damage and for a surface
    /// whose buffer's size is not known: a client that attached a buffer and
    /// said nothing about what it drew in it has changed something the
    /// compositor cannot place inside the surface.
    pub(crate) fn parts(&self, into: Rect) -> Vec<Rect> {
        let Some(buffer) = self
            .buffer
            .filter(|(width, height)| *width > 0 && *height > 0)
        else {
            return vec![into];
        };
        if self.rects.is_empty() {
            return vec![into];
        }
        self.rects
            .iter()
            .map(|rect| inside(*rect, buffer, into))
            .collect()
    }
}

/// Where a commit's pixels went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landed {
    /// Nowhere on this screen: a window on another monitor or another
    /// workspace, which this screen does not draw.
    Nowhere,
    /// Into this rectangle of the screen, which the surface's buffer is
    /// drawn into whole.
    In(Rect),
    /// The same, for a layer surface under the windows: what it draws is
    /// part of what a window's blur is a blur of.
    Behind(Rect),
    /// Somewhere this screen cannot place: a subsurface, a cursor surface
    /// that is not the one being drawn, a window that has not been mapped
    /// yet. The whole screen, then, which is the rule the module comment
    /// gives.
    Anywhere,
}

/// What the clients said since the last frame was drawn.
///
/// Held across passes rather than a pass, because a pass that changes
/// something does not always draw: what a client said has to survive until
/// the frame that shows it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Told {
    /// Whether something happened that cannot be placed on a screen at all,
    /// and every pixel of every screen has to be drawn again.
    everything: bool,
    /// The commits, in the order they arrived.
    painted: Vec<Painted>,
}

impl Told {
    /// Everything: a pool was remapped or a buffer destroyed under a
    /// surface that is still shown, and which pixels that moved cannot be
    /// worked out from what arrived.
    pub(crate) fn everything(&mut self) {
        self.everything = true;
        self.painted.clear();
    }

    /// One commit.
    pub(crate) fn painted(&mut self, painted: Painted) {
        if self.everything {
            return;
        }
        if self.painted.len() >= MOST {
            self.everything();
            return;
        }
        self.painted.push(painted);
    }

    /// Forget it all, which is what drawing a frame from it does.
    pub(crate) fn taken(&mut self) {
        self.everything = false;
        self.painted.clear();
    }

    /// Where all of it lands on one screen, and whether any of it is beyond
    /// placing -- which is the whole of that screen.
    pub(crate) fn on(
        &self,
        plan: &Plan,
        sources: &BTreeMap<WindowId, crate::frame::Source>,
    ) -> Heard {
        let mut region = Damage::new();
        let mut behind = Damage::new();
        let mut everything = self.everything;
        for painted in &self.painted {
            match plan.landed(painted, sources) {
                Landed::Nowhere => {}
                Landed::Anywhere => everything = true,
                Landed::In(rect) => {
                    for part in painted.parts(rect) {
                        region.add(part);
                    }
                }
                Landed::Behind(rect) => {
                    for part in painted.parts(rect) {
                        region.add(part);
                        behind.add(part);
                    }
                }
            }
        }
        Heard {
            region,
            behind,
            everything,
        }
    }
}

/// What the clients said, on one screen and in its own pixels.
#[derive(Clone, Debug, Default)]
pub(crate) struct Heard {
    /// Every part of the screen a commit changed.
    pub(crate) region: Damage,
    /// The parts of that which are *behind* the windows -- a wallpaper's
    /// commit and not a terminal's -- which is what a blur taken from the
    /// backdrop has to be told about.
    pub(crate) behind: Damage,
    /// Whether any of it is beyond placing, which is the whole screen.
    pub(crate) everything: bool,
}

/// Where a damaged rectangle of a buffer lands on the screen.
///
/// The renderer draws the whole of a surface's buffer into the rectangle
/// the layout gave it, so a part of the buffer lands on the same part of
/// that rectangle. The edges are rounded outwards and then grown by a pixel,
/// because a rectangle that is half a pixel short is a row of stale pixels.
#[must_use]
fn inside(rect: Rect, buffer: (i64, i64), into: Rect) -> Rect {
    let (width, height) = buffer;
    if width <= 0 || height <= 0 {
        return into;
    }
    // The part of the buffer that is really there: a client may damage
    // whatever rectangle it likes, and `INT32_MAX` wide is what a toolkit
    // sends for "all of it".
    let left = rect.x.clamp(0, width);
    let top = rect.y.clamp(0, height);
    let right = rect.right().clamp(left, width);
    let bottom = rect.bottom().clamp(top, height);
    let along = |value: i64, of: i64, from: i64, span: i64| -> i64 {
        from.saturating_add(value.saturating_mul(span).div_euclid(of.max(1)))
    };
    let x = along(left, width, into.x, into.width);
    let y = along(top, height, into.y, into.height);
    let to_x = along(right, width, into.x, into.width);
    let to_y = along(bottom, height, into.y, into.height);
    grown(
        Rect::new(
            x,
            y,
            to_x.saturating_sub(x).max(0),
            to_y.saturating_sub(y).max(0),
        ),
        (1, 1),
    )
}

/// A surface's surface-coordinate `rect` -- `wl_surface.damage` -- in its
/// buffer's own pixels, which is where [`Painted::rects`] are.
///
/// Without a viewport the buffer is the surface times its scale. With one,
/// the surface is the viewport's destination, and what is stretched over it
/// is the source: part of the buffer, in the buffer's pixels over its scale,
/// or the whole buffer when no source is set. Chromium draws a buffer twice
/// its surface and scales it down this way, and its damage multiplied by
/// the scale alone (1) covered the buffer's top-left quarter: only that
/// quarter of the window was ever redrawn.
///
/// The result is rounded outward, so a fraction of a pixel still counts.
#[must_use]
pub(crate) fn from_surface(
    rect: Rect,
    scale: i64,
    buffer: Option<(i64, i64)>,
    source: Option<(f64, f64, f64, f64)>,
    destination: Option<(i32, i32)>,
) -> Rect {
    let scale = scale.max(1);
    let multiplied = || {
        Rect::new(
            rect.x.saturating_mul(scale),
            rect.y.saturating_mul(scale),
            rect.width.saturating_mul(scale),
            rect.height.saturating_mul(scale),
        )
    };
    if source.is_none() && destination.is_none() {
        return multiplied();
    }
    let scaled = scale as f64;
    let source = match (source, buffer) {
        (Some(source), _) => source,
        (None, Some((width, height))) => (0.0, 0.0, width as f64 / scaled, height as f64 / scaled),
        (None, None) => return multiplied(),
    };
    // A source with no destination makes the surface the source's size.
    let (width, height) = destination.map_or((source.2, source.3), |(width, height)| {
        (f64::from(width), f64::from(height))
    });
    if width <= 0.0 || height <= 0.0 || source.2 <= 0.0 || source.3 <= 0.0 {
        return multiplied();
    }
    let x = |value: i64| (source.0 + value as f64 * source.2 / width) * scaled;
    let y = |value: i64| (source.1 + value as f64 * source.3 / height) * scaled;
    let left = x(rect.x).floor();
    let top = y(rect.y).floor();
    let right = x(rect.right()).ceil();
    let bottom = y(rect.bottom()).ceil();
    // `as` saturates: `INT32_MAX` wide stays a very large rectangle, which
    // `inside` clamps to the buffer.
    Rect::new(
        left as i64,
        top as i64,
        (right - left).max(0.0) as i64,
        (bottom - top).max(0.0) as i64,
    )
}

/// `rect` with `margin` pixels added on every side.
#[must_use]
fn grown(rect: Rect, margin: (i64, i64)) -> Rect {
    Rect::new(
        rect.x.saturating_sub(margin.0),
        rect.y.saturating_sub(margin.1),
        rect.width.saturating_add(margin.0.saturating_mul(2)),
        rect.height.saturating_add(margin.1.saturating_mul(2)),
    )
}

/// What one screen's frame is drawn from, beside the clients' own pixels.
///
/// Everything the renderer reads that is not inside a surface, so that two
/// frames with equal plans differ only where a client has drawn. The
/// rectangles are the screen's own pixels: the layout as
/// `compositor_render::scaled` gives it, and the pointer and the drag icon
/// where the frame puts them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    /// The screen's size in its own pixels.
    pub(crate) size: (u32, u32),
    /// Where the monitor is in the space every window's rectangle is in.
    pub(crate) origin: (i64, i64),
    /// How many of its pixels one logical pixel is.
    pub(crate) scale: f64,
    /// The colours and decorations, already at the screen's scale.
    pub(crate) style: Style,
    /// Whether `dpms off` has turned the screen off.
    pub(crate) dark: bool,
    /// Whether the session is locked.
    pub(crate) locked: bool,
    /// The ramps a night-light set on it.
    pub(crate) gamma: Option<crate::frame::Gamma>,
    /// The windows, scaled into the screen's own pixels.
    pub(crate) layout: MonitorLayout,
    /// What a `windowrule` gave each of them.
    pub(crate) styles: BTreeMap<WindowId, WindowStyle>,
    /// The bars, the menus and the lock's own surfaces, in drawing order.
    pub(crate) layers: Vec<Placed>,
    /// The pointer as the frame draws it, and where its pixels go.
    pub(crate) cursor: Option<(Cursor, Rect)>,
    /// The surface a drag is carrying, the same way.
    pub(crate) drag_icon: Option<(Placed, Rect)>,
    /// The client's surface the screen's cursor plane shows, which no frame
    /// draws: what it paints goes to the plane and damages nothing.
    pub(crate) plane: Option<(usize, ObjectId)>,
    /// The surfaces this frame draws a blur behind. Worked out where the
    /// clients' buffers are, because whether a surface can be seen through
    /// is half of the renderer's condition for blurring behind it.
    pub(crate) blurred: Vec<Blurred>,
    /// Where `debug:overlay`'s counter is and which rewrite of its numbers
    /// it shows, on the screen that draws it.
    pub(crate) overlay: Option<crate::overlay::Stamp>,
}

/// One surface a frame draws a blur behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Blurred {
    /// The rectangle the blur is written into, in the screen's own pixels.
    pub(crate) rect: Rect,
    /// Whether it is a blur of the frame as it stands when the surface is
    /// drawn, rather than one taken from the kept backdrop:
    /// `compositor_render::reads_backdrop` is the rule, and the module
    /// comment says what each is owed.
    pub(crate) live: bool,
}

impl Plan {
    /// One rectangle of the layout's global space in this screen's own
    /// pixels.
    fn local(&self, rect: Rect) -> Rect {
        crate::frame::local(rect, self.origin, self.scale)
    }

    /// Where a commit's pixels are on this screen.
    ///
    /// Which is to say: what this frame draws that surface's buffer into.
    /// A surface this frame draws nothing from is [`Landed::Anywhere`] --
    /// the compositor knows a client painted and not where -- unless it is
    /// a window of a workspace this screen does not show, which is the one
    /// case where nothing of it can be on this screen at all.
    pub(crate) fn landed(
        &self,
        painted: &Painted,
        sources: &BTreeMap<WindowId, crate::frame::Source>,
    ) -> Landed {
        let named = |client: usize, surface: ObjectId| {
            client == painted.client && surface == painted.surface
        };
        if let Some(placed) = self
            .layers
            .iter()
            .find(|placed| named(placed.client, placed.surface))
        {
            let at = self.local(placed.rect);
            return if placed.above {
                Landed::In(at)
            } else {
                Landed::Behind(at)
            };
        }
        if let Some((_, at)) = self
            .drag_icon
            .filter(|(icon, _)| named(icon.client, icon.surface))
        {
            return Landed::In(at);
        }
        if self
            .plane
            .is_some_and(|(client, surface)| named(client, surface))
        {
            return Landed::Nowhere;
        }
        if let Some((_, at)) = self.cursor.filter(|(cursor, _)| {
            cursor
                .surface
                .is_some_and(|(client, surface, _)| named(client, surface))
        }) {
            return Landed::In(at);
        }
        let Some((window, _)) = sources
            .iter()
            .find(|(_, source)| named(source.client, source.surface))
        else {
            return Landed::Anywhere;
        };
        // The layout is already in the screen's own pixels, so only the
        // monitor's corner is left to take off. A window this screen does
        // not show is on another monitor or another workspace.
        self.layout
            .windows
            .iter()
            .find(|placed| placed.window == *window)
            .map_or(Landed::Nowhere, |placed| {
                Landed::In(placed.rect.translate(
                    self.origin.0.saturating_neg(),
                    self.origin.1.saturating_neg(),
                ))
            })
    }

    /// How far outside a window's border box its decorations reach.
    ///
    /// The shadow is drawn around the border box, grown by its range and
    /// moved by its offset, so a window that went leaves its shadow behind
    /// unless the damage covers that too. A `windowrule = bordersize` is
    /// added on top of it: the renderer draws that border instead of the
    /// layout's, and the layout's is what a rectangle is measured from.
    fn margin(&self) -> (i64, i64) {
        let border = self
            .styles
            .values()
            .filter_map(|style| style.border)
            .fold(0, i64::max);
        let (wide, tall) = self.style.shadow.map_or((0, 0), |shadow| {
            (
                shadow.range.saturating_add(shadow.offset.0.abs()),
                shadow.range.saturating_add(shadow.offset.1.abs()),
            )
        });
        (
            wide.saturating_add(border).max(0),
            tall.saturating_add(border).max(0),
        )
    }

    /// The part of the screen this plan differs from `old` in.
    ///
    /// Whole-screen for anything that changes every pixel -- the screen's
    /// size, where it is, its scale, the style, a night-light's ramps,
    /// `dpms`, the lock -- because each of those is drawn into every pixel
    /// and none of them happens often enough to be worth a region.
    ///
    /// What of it is behind the windows is added to `behind`.
    fn since(&self, old: &Self, behind: &mut Damage) -> Damage {
        let (width, height) = self.size;
        if self.size != old.size
            || self.origin != old.origin
            || (self.scale - old.scale).abs() >= f64::EPSILON
            || self.style != old.style
            || self.dark != old.dark
            || self.locked != old.locked
            || self.gamma != old.gamma
        {
            return Damage::full(width, height);
        }
        let mut region = Damage::new();
        self.windows_since(old, &mut region);
        self.layers_since(old, &mut region, behind);
        self.pointer_since(old, &mut region);
        self.overlay_since(old, &mut region);
        self.blurs_since(old, &mut region);
        region
    }

    /// The counter where it was and where it is, when it moved, grew or
    /// was rewritten: Hyprland damages the old box and the new one the
    /// same way (`COverlay::draw`).
    fn overlay_since(&self, old: &Self, region: &mut Damage) {
        if old.overlay != self.overlay {
            for stamp in [old.overlay, self.overlay].into_iter().flatten() {
                region.add(stamp.rect);
            }
        }
    }

    /// A surface whose blur is another kind than it was, or that has one
    /// and had none: a window a floating one was dragged off, which blurred
    /// that window and now takes the backdrop's. The two kinds differ by
    /// what was under the surface, all over it, so all of it is drawn again.
    fn blurs_since(&self, old: &Self, region: &mut Damage) {
        if old.blurred == self.blurred {
            return;
        }
        for (one, other) in [(old, self), (self, old)] {
            for blurred in &one.blurred {
                if !other.blurred.contains(blurred) {
                    region.add(blurred.rect);
                }
            }
        }
    }

    /// The windows: what `compositor_render::damage_between` says moved,
    /// appeared, went, took the focus or was restacked, and any window a
    /// rule now draws differently.
    fn windows_since(&self, old: &Self, region: &mut Damage) {
        let margin = self.margin();
        // What the plan draws each window with, so that a window whose focus
        // is all that changed is owed its border and not all of it.
        let styles = compositor_render::Styles {
            base: &self.style,
            windows: &self.styles,
        };
        let (between, rings) = compositor_render::damage_between_parts(
            &old.layout,
            &self.layout,
            self.origin,
            &styles,
        );
        for rect in between.rects() {
            region.add(grown(*rect, margin));
        }
        // A focus change's rings as they are: the shadows did not move.
        region.extend(&rings);
        // A rule changed between the two frames -- `hyprctl keyword
        // windowrule`, or a window that has just been renamed into a rule's
        // reach -- so what the window is drawn with is not what it was,
        // wherever it is.
        let ours = self.styles.keys().chain(old.styles.keys());
        for window in ours.copied().collect::<std::collections::BTreeSet<_>>() {
            if self.styles.get(&window) == old.styles.get(&window) {
                continue;
            }
            for layout in [&old.layout, &self.layout] {
                if let Some(placed) = layout.windows.iter().find(|placed| placed.window == window) {
                    region.add(grown(
                        outer(placed).translate(
                            self.origin.0.saturating_neg(),
                            self.origin.1.saturating_neg(),
                        ),
                        margin,
                    ));
                }
            }
        }
    }

    /// The bars, the menus and the lock's own surfaces.
    fn layers_since(&self, old: &Self, region: &mut Damage, behind: &mut Damage) {
        if old.layers == self.layers {
            return;
        }
        // One at a time, and each where it was and where it is: a bar that
        // changed nothing does not have to be drawn again because a dock
        // beside it opened. They are compared by their place in the list,
        // which is what decides which of them covers the other, so a list
        // whose order changed damages every surface that moved in it.
        for at in 0..old.layers.len().max(self.layers.len()) {
            let (was, now) = (old.layers.get(at), self.layers.get(at));
            if was == now {
                continue;
            }
            for placed in was.into_iter().chain(now) {
                region.add(self.local(placed.rect));
                if !placed.above {
                    behind.add(self.local(placed.rect));
                }
            }
        }
    }

    /// The blur the style asks for, if it asks for one that does anything.
    fn blur(&self) -> Option<compositor_render::Blur> {
        self.style
            .blur
            .filter(|blur| blur.size > 0 && blur.passes > 0)
    }

    /// Grow `region` by what a change `behind` the windows does to the
    /// blurs taken from the backdrop.
    ///
    /// A pixel of a blur is made from everything within
    /// `compositor_render::Blur::reach` of it, so a wallpaper that repainted
    /// a square has changed the blur that far outside the square -- under a
    /// window the commit never touched. Only under a window that takes its
    /// blur from the backdrop: one that blurs the frame as it stands is
    /// redrawn whole, which [`Plan::blurs_whole`] sees to.
    ///
    /// Nothing else is owed. A pointer over such a window, a letter typed
    /// into it, its neighbour closing: none of them changes what is behind
    /// the windows, so the blur they uncover is the one already kept.
    fn behind_reaches(&self, behind: &Damage, region: &mut Damage) {
        let Some(blur) = self.blur() else {
            return;
        };
        let reach = blur.reach();
        for blurred in self.blurred.iter().filter(|blurred| !blurred.live) {
            for changed in behind.rects() {
                for part in Damage::from(grown(*changed, (reach, reach)))
                    .clipped(blurred.rect)
                    .rects()
                {
                    region.add(*part);
                }
            }
        }
    }

    /// Grow `region` until every blur of the frame as it stands can be
    /// drawn exactly.
    ///
    /// Such a blur is the one thing a frame draws that *reads* the canvas
    /// beyond what it writes: `Canvas::blur` takes the pixels behind a
    /// surface from the canvas, and what it reads reaches a whole kernel
    /// outside the part of the surface being redrawn. Inside the damage the
    /// canvas holds what this frame has drawn under the surface; outside
    /// it, it still holds the *last* frame -- including the surface's own
    /// pixels, drawn over that blur. A blur that read those would be a blur
    /// of itself, which is the ghosting a person sees around a translucent
    /// window and cannot explain.
    ///
    /// So such a surface the damage touches is redrawn whole, with a
    /// kernel's reach of the frame around it: then everything the blur
    /// reads has been drawn by this frame, and the pixels are the ones a
    /// whole frame would have produced. Hyprland grows its damage for the
    /// same reason and by a kernel or two of the same reach
    /// (`CRenderPass::begin` in `src/render/pass/Pass.cpp`, whose comment
    /// -- "moving a window over blur shows the edges being wonk" -- is
    /// this artifact); it stops at the reach and keeps the edges, because
    /// on a GPU the whole window is cheap. Here the whole surface is the
    /// price of being exact, and being exact is what the blessed frames
    /// ask for.
    ///
    /// It is a price only a floating window, a scratchpad's and a blurred
    /// layer surface pay: a tiled window's blur comes from the backdrop.
    ///
    /// Growing one surface may reach another, which is why this runs until
    /// nothing more is added.
    fn blurs_whole(&self, region: &mut Damage) {
        let Some(blur) = self.blur() else {
            return;
        };
        let reach = blur.reach();
        let mut left: Vec<Rect> = self
            .blurred
            .iter()
            .filter(|blurred| blurred.live)
            .map(|blurred| blurred.rect)
            .collect();
        while !left.is_empty() {
            let touched: Vec<Rect> = left
                .iter()
                .copied()
                .filter(|rect| !region.clipped(*rect).is_empty())
                .collect();
            if touched.is_empty() {
                return;
            }
            left.retain(|rect| !touched.contains(rect));
            for rect in touched {
                region.add(grown(rect, (reach, reach)));
            }
        }
    }

    /// The pointer and the drag icon, each where it was and where it is.
    fn pointer_since(&self, old: &Self, region: &mut Damage) {
        if old.cursor != self.cursor {
            for at in [old.cursor, self.cursor].into_iter().flatten() {
                region.add(at.1);
            }
        }
        if old.drag_icon != self.drag_icon {
            for at in [old.drag_icon, self.drag_icon].into_iter().flatten() {
                region.add(at.1);
            }
        }
    }
}

/// What one frame draws and what it copies to the screen.
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    /// The part of the canvas this frame redraws.
    pub(crate) canvas: Damage,
    /// The part of the screen it copies into, which is this frame's damage
    /// and the frame before's: the module comment says why.
    pub(crate) screen: Damage,
    /// Where the damage came from, in pixels: the plan's change since the
    /// last frame, the clients' commits, and what the two blur rules added.
    /// For the compositor's report, which says it of its slowest frame.
    pub(crate) sources: [i64; 4],
}

/// One screen's memory of what it last drew.
#[derive(Clone, Debug, Default)]
pub(crate) struct Watch {
    /// The plan the last frame was drawn from, or `None` before there was
    /// one -- which is why a screen's first frame is a whole one.
    plan: Option<Plan>,
    /// What that frame redrew.
    previous: Damage,
}

impl Watch {
    /// The damage for the frame `plan` describes, given what the clients
    /// said they drew.
    ///
    /// `heard` is already in this screen's own pixels: only the caller knows
    /// where a surface's rectangle is. `age` is how many frames old the
    /// buffer this frame is copied into is, `Backend::age`: past one, it is
    /// owed the frame before's damage as well as this one's.
    pub(crate) fn frame(&mut self, plan: Plan, heard: &Heard, age: u32) -> Frame {
        let (width, height) = plan.size;
        let mut behind = heard.behind.clone();
        let mut region = match self.plan.as_ref() {
            Some(old) if !heard.everything => plan.since(old, &mut behind),
            _ => Damage::full(width, height),
        };
        let planned = region.area();
        region.extend(&heard.region);
        let told = region.area();
        // And whatever the blurs are owed to come out the same as a whole
        // frame's would, which is the last thing added: it grows around
        // everything else. The backdrop's first, since what that adds may
        // touch a surface the second has to redraw whole.
        plan.behind_reaches(&behind, &mut region);
        let reached = region.area();
        plan.blurs_whole(&mut region);
        let sources = [
            planned,
            heard.region.area(),
            reached.saturating_sub(told),
            region.area().saturating_sub(reached),
        ];
        let region = region.clipped(Rect::new(0, 0, i64::from(width), i64::from(height)));
        let mut screen = region.clone();
        if age > 1 {
            screen.extend(&self.previous);
        }
        self.previous = region.clone();
        self.plan = Some(plan);
        Frame {
            canvas: region,
            screen,
            sources,
        }
    }
}

#[cfg(test)]
mod tests;
