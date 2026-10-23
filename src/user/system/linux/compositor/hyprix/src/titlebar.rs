//! Title bars the compositor draws (`docs/DESKTOP.md` §2.2, step 2a).
//!
//! A window whose client asked for the compositor's frame -- foot, an X
//! program, anything that binds `xdg-decoration` or KDE's server
//! decoration -- draws no title bar of its own. With `hyprix:titlebars = 1`
//! a floating one gets a header bar above its rectangle in the style of
//! the theme: the title centred, minimize, maximize and close buttons on
//! the right drawn as shapes in code, rounded top corners. A press on the
//! bar moves the window, a double click maximizes it, and the buttons do
//! what they say.
//!
//! The bar goes into the frame as one more window with no client
//! ([`Bars::decorate`]), just under its own, its pixels drawn here into a
//! buffer the renderer composites like any client's ([`Bars::rendered`]).
//! It reaches `bar` pixels below the window's top edge, under the window,
//! so that its bottom corners are hidden and only its top ones show
//! rounded. Its id carries a digest of what it shows, so a new title or a
//! change of focus is a different window to the damage and is redrawn.

use std::collections::{BTreeMap, BTreeSet};

use compositor_layout::{MonitorLayout, Placed, Rect, Snap, State, WindowId};
use compositor_text::{FontDescription, Fonts, Rgba, Size, Weight};

/// How high a bar is, in logical pixels.
pub const HEIGHT: i64 = 38;

/// How far a bar reaches under its window, hidden: its corners' radius.
const UNDER: i64 = 12;

/// The bit every bar's id has, and no client's window.
const BAR: u64 = 1 << 62;

/// A button's diameter and the space between buttons, logical pixels.
const BUTTON: f32 = 24.0;
const SPACING: f32 = 9.0;

/// Ferrix light's header: active and inactive backgrounds, text, and a
/// button's disc. Our own values.
const ACTIVE: Rgba = Rgba::rgb(0xeb, 0xeb, 0xed);
const INACTIVE: Rgba = Rgba::rgb(0xf6, 0xf6, 0xf7);
const TEXT: Rgba = Rgba::rgb(0x2b, 0x2b, 0x30);
const TEXT_INACTIVE: Rgba = Rgba::rgb(0x8c, 0x8c, 0x92);
const DISC: Rgba = Rgba {
    r: 0,
    g: 0,
    b: 0,
    a: 0x14,
};

/// What a press on a bar hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The close button.
    Close,
    /// The maximize button.
    Maximize,
    /// The minimize button.
    Minimize,
    /// Anywhere else on the bar: a move.
    Bar,
}

/// One bar's pixels: premultiplied `ARGB8888`, rows packed.
#[derive(Clone, Debug)]
pub struct Rendered {
    /// The bytes.
    pub bytes: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// The bars drawn this frame, and the fonts their titles are set in.
#[derive(Default)]
pub struct Bars {
    /// Loaded the first time a title is drawn.
    fonts: Option<Fonts>,
    /// Every bar drawn, by its id.
    rendered: BTreeMap<WindowId, Rendered>,
    /// Each titled window's bar rectangle on the last frame, in layout
    /// space, for the pointer.
    placed: Vec<(WindowId, Rect)>,
    /// The last press on a bar, for a double click.
    last_press: Option<(WindowId, std::time::Instant)>,
    /// Whether the left button's next release ends a press on a bar's
    /// button, which no client saw and so none is sent.
    pub swallow_release: bool,
}

impl core::fmt::Debug for Bars {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bars")
            .field("rendered", &self.rendered.len())
            .field("placed", &self.placed)
            .finish_non_exhaustive()
    }
}

/// What one window's bar shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Wanted {
    /// The window.
    pub window: WindowId,
    /// Its title.
    pub title: String,
}

impl Bars {
    /// The bars drawn, by id, for the renderer.
    #[must_use]
    pub const fn rendered(&self) -> &BTreeMap<WindowId, Rendered> {
        &self.rendered
    }

    /// Put each wanted window's bar into `outputs`, just under the window,
    /// drawn at `scale`, and its style into `styles`; forget bars no longer
    /// drawn.
    pub fn decorate(
        &mut self,
        outputs: &mut [MonitorLayout],
        styles: &mut BTreeMap<WindowId, compositor_render::WindowStyle>,
        wanted: &[Wanted],
        state: &State,
        scale: f64,
    ) {
        let mut used = BTreeSet::new();
        self.placed.clear();
        for output in outputs.iter_mut() {
            let mut at = 0;
            while at < output.windows.len() {
                let Some(placed) = output.windows.get(at).copied() else {
                    break;
                };
                let Some(want) = wanted.iter().find(|want| want.window == placed.window) else {
                    at += 1;
                    continue;
                };
                let maximized = state.snap_of(placed.window) == Some(Snap::Maximized);
                let rect = Rect::new(
                    placed.rect.x,
                    placed.rect.y.saturating_sub(HEIGHT),
                    placed.rect.width,
                    HEIGHT.saturating_add(UNDER),
                );
                let id = bar_id(want, placed.focused, rect.width, maximized, scale);
                if !self.rendered.contains_key(&id)
                    && let Some(drawn) = self.draw(want, placed.focused, rect.width, scale)
                {
                    let _ = self.rendered.insert(id, drawn);
                }
                let _ = used.insert(id);
                self.placed
                    .push((placed.window, Rect::new(rect.x, rect.y, rect.width, HEIGHT)));
                output.windows.insert(
                    at,
                    Placed {
                        window: id,
                        rect,
                        border: placed.border,
                        focused: placed.focused,
                        floating: true,
                        fullscreen: false,
                    },
                );
                let _ = styles.insert(
                    id,
                    compositor_render::WindowStyle {
                        rounding: Some(if state.snap_of(placed.window).is_some() {
                            0
                        } else {
                            12
                        }),
                        blur: false,
                        dim: false,
                        ..compositor_render::WindowStyle::default()
                    },
                );
                // The window under its bar keeps square top corners, which
                // the bar's rounded ones stand for.
                let style = styles.entry(placed.window).or_default();
                style.rounding = Some(0);
                at += 2;
            }
        }
        self.rendered.retain(|id, _| used.contains(id));
    }

    /// Which titled window's bar is at `point`, and what on it; only bars
    /// not covered by a window drawn over them count, which the caller
    /// checks with the layout.
    #[must_use]
    pub fn hit(&self, point: (f64, f64)) -> Option<(WindowId, Hit)> {
        let (x, y) = point;
        self.placed.iter().rev().find_map(|(window, rect)| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a screen's pixels are far inside f64's exact range"
            )]
            let (left, top, right, bottom) = (
                rect.x as f64,
                rect.y as f64,
                rect.right() as f64,
                rect.bottom() as f64,
            );
            if x < left || x >= right || y < top || y >= bottom {
                return None;
            }
            let from_right = right - x;
            let middle = top + f64::from(HEIGHT as f32) / 2.0;
            let radius = f64::from(BUTTON) / 2.0;
            for (index, hit) in [Hit::Close, Hit::Maximize, Hit::Minimize]
                .into_iter()
                .enumerate()
            {
                #[expect(clippy::cast_precision_loss, reason = "three buttons")]
                let centre =
                    f64::from(SPACING) + radius + index as f64 * f64::from(BUTTON + SPACING);
                if (from_right - centre).hypot(y - middle) <= radius + 2.0 {
                    return Some((*window, hit));
                }
            }
            Some((*window, Hit::Bar))
        })
    }

    /// The bar under `point`, unless a window drawn over that bar's window
    /// covers the point there: `layout` is the frame's, bottom to top.
    #[must_use]
    pub fn uncovered_hit(
        &self,
        point: (f64, f64),
        layout: &[MonitorLayout],
    ) -> Option<(WindowId, Hit)> {
        let (window, hit) = self.hit(point)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the pointer is held inside the screen, which is far inside i64"
        )]
        let (x, y) = (point.0 as i64, point.1 as i64);
        let covered = layout.iter().any(|output| {
            output
                .windows
                .iter()
                .skip_while(|placed| placed.window != window)
                .skip(1)
                .any(|placed| {
                    let rect = placed.rect;
                    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
                })
        });
        (!covered).then_some((window, hit))
    }

    /// Whether a press on `window`'s bar now is the second of a double
    /// click; remembers it either way.
    pub fn double_click(&mut self, window: WindowId) -> bool {
        let now = std::time::Instant::now();
        let double = self.last_press.is_some_and(|(last, when)| {
            last == window && now.duration_since(when) < std::time::Duration::from_millis(400)
        });
        self.last_press = (!double).then_some((window, now));
        double
    }

    /// Draw one bar: `width` logical pixels wide, at `scale`.
    fn draw(&mut self, want: &Wanted, focused: bool, width: i64, scale: f64) -> Option<Rendered> {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a bar is a screen's width and a few dozen pixels high"
        )]
        let (pixel_width, pixel_height) = (
            (width as f64 * scale).round().max(1.0) as u32,
            ((HEIGHT + UNDER) as f64 * scale).round().max(1.0) as u32,
        );
        let mut pixmap = tiny_skia::Pixmap::new(pixel_width, pixel_height)?;
        pixmap.fill(if focused { ACTIVE } else { INACTIVE }.to_skia());
        #[expect(clippy::cast_possible_truncation, reason = "a scale is a small number")]
        let s = scale as f32;
        let middle = HEIGHT as f32 * s / 2.0;
        // The buttons, right to left: close, maximize, minimize.
        let ink = if focused { TEXT } else { TEXT_INACTIVE };
        let radius = BUTTON * s / 2.0;
        let mut right_edge = pixel_width as f32;
        for (index, which) in [Hit::Close, Hit::Maximize, Hit::Minimize]
            .into_iter()
            .enumerate()
        {
            #[expect(clippy::cast_precision_loss, reason = "three buttons")]
            let cx =
                pixel_width as f32 - (SPACING * s + radius + index as f32 * (BUTTON + SPACING) * s);
            right_edge = cx - radius;
            button(&mut pixmap, (cx, middle), radius, which, ink, s);
        }
        // The title, centred on the bar and kept clear of the buttons.
        if !want.title.is_empty() {
            let fonts = self.fonts.get_or_insert_with(Fonts::system);
            let mut description = FontDescription::pango("sans-serif");
            description.weight = Weight::BOLD;
            description.size = Size::Pixels(14.0 * s);
            let font = fonts.resolve(&description);
            let room = (right_edge - 2.0 * SPACING * s).max(0.0)
                - (pixel_width as f32 - right_edge)
                - SPACING * s;
            let run = fonts.shape_ellipsized(
                &font,
                &want.title,
                room.max(BUTTON * s),
                compositor_text::Ellipsize::End,
            );
            let metrics = fonts.metrics(&font);
            let x = ((pixel_width as f32 - run.width) / 2.0).max(SPACING * s);
            let baseline = middle + (metrics.ascent - metrics.descent) / 2.0;
            fonts.draw_run(&run, &mut pixmap.as_mut(), x, baseline, ink);
        }
        Some(Rendered {
            bytes: argb(&pixmap),
            width: pixel_width,
            height: pixel_height,
        })
    }
}

/// A bar's id: the bar bit, the window, and a digest of what it shows.
fn bar_id(want: &Wanted, focused: bool, width: i64, maximized: bool, scale: f64) -> WindowId {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    want.title.hash(&mut hasher);
    focused.hash(&mut hasher);
    width.hash(&mut hasher);
    maximized.hash(&mut hasher);
    scale.to_bits().hash(&mut hasher);
    WindowId(BAR | ((want.window.0 & 0xff_ffff) << 32) | (hasher.finish() & 0xffff_ffff))
}

/// Whether `window` is a bar's id rather than a client's window.
#[must_use]
pub const fn is_bar(window: WindowId) -> bool {
    window.0 & BAR != 0
}

/// One button: a faint disc and its glyph in `ink`.
fn button(
    pixmap: &mut tiny_skia::Pixmap,
    (cx, cy): (f32, f32),
    radius: f32,
    which: Hit,
    ink: Rgba,
    s: f32,
) {
    use tiny_skia::{FillRule, LineCap, Paint, PathBuilder, Stroke, Transform};
    let mut disc = Paint::default();
    disc.set_color(DISC.to_skia());
    disc.anti_alias = true;
    if let Some(circle) = PathBuilder::from_circle(cx, cy, radius) {
        pixmap.fill_path(
            &circle,
            &disc,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let mut pen = Paint::default();
    pen.set_color(ink.to_skia());
    pen.anti_alias = true;
    let stroke = Stroke {
        width: 1.6 * s,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    let half = 4.5 * s;
    let mut path = PathBuilder::new();
    match which {
        Hit::Close => {
            path.move_to(cx - half, cy - half);
            path.line_to(cx + half, cy + half);
            path.move_to(cx + half, cy - half);
            path.line_to(cx - half, cy + half);
        }
        Hit::Maximize => {
            if let Some(rect) =
                tiny_skia::Rect::from_ltrb(cx - half, cy - half, cx + half, cy + half)
            {
                path.push_rect(rect);
            }
        }
        Hit::Minimize => {
            path.move_to(cx - half, cy + half * 0.6);
            path.line_to(cx + half, cy + half * 0.6);
        }
        Hit::Bar => {}
    }
    if let Some(path) = path.finish() {
        pixmap.stroke_path(&path, &pen, &stroke, Transform::identity(), None);
    }
}

/// A tiny-skia pixmap (premultiplied RGBA bytes) as premultiplied
/// `ARGB8888` in memory order, which is B, G, R, A.
fn argb(pixmap: &tiny_skia::Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.data().chunks_exact(4) {
        if let [r, g, b, a] = *pixel {
            out.extend([b, g, r, a]);
        }
    }
    out
}

pub use compositor_text::tiny_skia;

#[cfg(test)]
mod tests {
    use compositor_layout::{Rect, WindowId};

    use super::{Bars, Hit, is_bar};

    /// A bar is hit by its buttons from the right, and anywhere else is a
    /// move.
    #[test]
    fn a_press_finds_the_button_under_it() {
        let mut bars = Bars::default();
        bars.placed
            .push((WindowId(7), Rect::new(100, 100, 400, super::HEIGHT)));
        let middle = 100.0 + 19.0;
        assert_eq!(
            bars.hit((500.0 - 21.0, middle)),
            Some((WindowId(7), Hit::Close))
        );
        assert_eq!(
            bars.hit((500.0 - 54.0, middle)),
            Some((WindowId(7), Hit::Maximize))
        );
        assert_eq!(
            bars.hit((500.0 - 87.0, middle)),
            Some((WindowId(7), Hit::Minimize))
        );
        assert_eq!(bars.hit((200.0, middle)), Some((WindowId(7), Hit::Bar)));
        assert_eq!(bars.hit((200.0, 99.0)), None);
        assert_eq!(bars.hit((200.0, 138.0)), None, "the window, not its bar");
    }

    /// A bar's id is never a client's window's.
    #[test]
    fn bar_ids_are_apart_from_windows() {
        let want = super::Wanted {
            window: WindowId(3),
            title: "foot".to_owned(),
        };
        let id = super::bar_id(&want, true, 640, false, 1.0);
        assert!(is_bar(id));
        assert!(!is_bar(WindowId(3)));
        assert_ne!(
            id,
            super::bar_id(&want, false, 640, false, 1.0),
            "focus redraws it"
        );
    }

    /// A drawn bar is the size asked, opaque, in the active colour at its
    /// left edge.
    #[test]
    fn a_bar_is_drawn_at_its_size() {
        let mut bars = Bars {
            fonts: Some(compositor_text::Fonts::new()),
            ..Bars::default()
        };
        let want = super::Wanted {
            window: WindowId(3),
            title: String::new(),
        };
        let drawn = bars.draw(&want, true, 300, 1.0).expect("drawn");
        assert_eq!((drawn.width, drawn.height), (300, 50));
        assert_eq!(drawn.bytes.len(), 300 * 50 * 4);
        assert_eq!(
            drawn.bytes.get(..4),
            Some([0xed, 0xeb, 0xeb, 0xff].as_slice())
        );
    }
}
