//! The CPU renderer: tiny-skia for shapes and `compositor-text`'s glyph
//! masks for text, into a premultiplied RGBA pixmap. Always available.
//!
//! Its shapes are the lode viewer's (`draw.rs`): a rounded rectangle is
//! four cubic quarter circles, a fill is antialiased only when rounded,
//! and an outline is stroked half its width inside the edge, so a port
//! onto the framework draws the pixels the hand-written viewer drew.

use compositor_text::{Fonts, Rgba, Run};
use tiny_skia::{
    FillRule, Mask, Paint, PathBuilder, Pixmap, PixmapMut, PixmapPaint, Stroke, Transform,
};

use crate::render::{Rect, Renderer};
use crate::value::Color;

/// A cut in force: its rectangle in buffer pixels and, once a shape
/// needed one, its mask.
#[derive(Debug)]
struct Clip {
    /// Columns `.0..2`, rows `.1..3`.
    bounds: (i64, i64, i64, i64),
    /// The rectangle, in logical pixels.
    rect: Rect,
    /// The mask, made the first time a shape crosses the cut.
    mask: Option<Mask>,
}

/// Draws a scene into a pixmap at a scale.
pub struct CpuRenderer<'a, 'p> {
    /// The frame.
    base: &'a mut PixmapMut<'p>,
    /// The glyphs.
    fonts: &'a mut Fonts,
    /// Buffer pixels to a logical one.
    scale: f32,
    /// The cuts in force, innermost last.
    clips: Vec<Clip>,
    /// The opacity groups being drawn, innermost last.
    layers: Vec<(Pixmap, f32)>,
}

impl std::fmt::Debug for CpuRenderer<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpuRenderer")
            .field("scale", &self.scale)
            .field("clips", &self.clips.len())
            .field("layers", &self.layers.len())
            .finish_non_exhaustive()
    }
}

impl<'a, 'p> CpuRenderer<'a, 'p> {
    /// A renderer into `base` at `scale`, with `fonts`' glyphs.
    pub fn new(base: &'a mut PixmapMut<'p>, fonts: &'a mut Fonts, scale: f32) -> Self {
        CpuRenderer {
            base,
            fonts,
            scale,
            clips: Vec::new(),
            layers: Vec::new(),
        }
    }

    /// Make sure the innermost cut has a mask when a shape covering
    /// `rect` crosses it. Answers whether the shape shows at all, and
    /// whether it needs the mask.
    fn prepare_mask(&mut self, rect: Rect) -> Option<bool> {
        let (width, height) = (self.base.width(), self.base.height());
        let Some(clip) = self.clips.last_mut() else {
            return Some(false);
        };
        let inside = clip.rect.intersect(&rect);
        if inside.w <= 0.0 || inside.h <= 0.0 {
            return None;
        }
        if inside == rect {
            return Some(false);
        }
        if clip.mask.is_none() {
            let mut mask = Mask::new(width, height)?;
            let (x0, y0, x1, y1) = clip.bounds;
            let area = tiny_skia::Rect::from_ltrb(x0 as f32, y0 as f32, x1 as f32, y1 as f32)?;
            mask.fill_path(
                &PathBuilder::from_rect(area),
                FillRule::Winding,
                false,
                Transform::identity(),
            );
            clip.mask = Some(mask);
        }
        Some(true)
    }

    /// Fill or stroke `path` into the target, through the innermost cut's
    /// mask when `masked`.
    fn draw_path(
        &mut self,
        path: &tiny_skia::Path,
        paint: &Paint<'_>,
        stroke: Option<&Stroke>,
        masked: bool,
    ) {
        let mask = if masked {
            self.clips.last().and_then(|clip| clip.mask.as_ref())
        } else {
            None
        };
        let Some(mut target) = target(self.base, &mut self.layers) else {
            return;
        };
        match stroke {
            Some(stroke) => target.stroke_path(path, paint, stroke, Transform::identity(), mask),
            None => target.fill_path(path, paint, FillRule::Winding, Transform::identity(), mask),
        }
    }

    /// A rounded rectangle's path, in buffer pixels.
    fn rounded(&self, place: Rect, radius: f32) -> Option<tiny_skia::Path> {
        let s = self.scale;
        let (x, y, w, h) = (place.x * s, place.y * s, place.w * s, place.h * s);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let r = (radius * s).min(w / 2.0).min(h / 2.0);
        if r <= 0.0 {
            return Some(PathBuilder::from_rect(tiny_skia::Rect::from_xywh(
                x, y, w, h,
            )?));
        }
        // A quarter circle's control points are this far along its
        // tangents.
        let k = r * 0.552_284_8;
        let mut path = PathBuilder::new();
        path.move_to(x + r, y);
        path.line_to(x + w - r, y);
        path.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
        path.line_to(x + w, y + h - r);
        path.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
        path.line_to(x + r, y + h);
        path.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
        path.line_to(x, y + r);
        path.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
        path.close();
        path.finish()
    }
}

/// The pixmap drawn into now: the innermost group's, or the frame.
fn target<'t>(
    base: &'t mut PixmapMut<'_>,
    layers: &'t mut [(Pixmap, f32)],
) -> Option<PixmapMut<'t>> {
    match layers.last_mut() {
        Some((layer, _)) => Some(layer.as_mut()),
        None => {
            let (width, height) = (base.width(), base.height());
            PixmapMut::from_bytes(base.data_mut(), width, height)
        }
    }
}

/// tiny-skia's colour.
fn skia(color: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.a)
}

/// A paint of one colour.
fn paint(color: Color, anti_alias: bool) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(skia(color));
    paint.anti_alias = anti_alias;
    paint
}

impl Renderer for CpuRenderer<'_, '_> {
    fn fill(&mut self, rect: Rect, radius: f32, color: Color) {
        if color.is_transparent() {
            return;
        }
        let Some(path) = self.rounded(rect, radius) else {
            return;
        };
        let Some(masked) = self.prepare_mask(rect) else {
            return;
        };
        self.draw_path(&path, &paint(color, radius > 0.0), None, masked);
    }

    fn stroke(&mut self, rect: Rect, radius: f32, width: f32, color: Color) {
        let half = width / 2.0;
        let inner = Rect::new(rect.x + half, rect.y + half, rect.w - width, rect.h - width);
        let Some(path) = self.rounded(inner, (radius - half).max(0.0)) else {
            return;
        };
        let Some(masked) = self.prepare_mask(rect) else {
            return;
        };
        let stroke = Stroke {
            width: width * self.scale,
            ..Stroke::default()
        };
        self.draw_path(&path, &paint(color, true), Some(&stroke), masked);
    }

    fn text(&mut self, run: &Run, x: f32, baseline: f32, color: Color) {
        let clip = self
            .clips
            .last()
            .map_or((i64::MIN, i64::MIN, i64::MAX, i64::MAX), |clip| clip.bounds);
        let rgba = Rgba {
            r: color.r,
            g: color.g,
            b: color.b,
            a: color.a,
        };
        if let Some(mut pixmap) = target(self.base, &mut self.layers) {
            self.fonts
                .draw_run_clipped(run, &mut pixmap, (x, baseline), rgba, clip);
        }
    }

    fn push_clip(&mut self, rect: Rect) {
        let rect = match self.clips.last() {
            Some(outer) => outer.rect.intersect(&rect),
            None => rect,
        };
        let s = self.scale;
        let bounds = (
            (rect.x * s).round() as i64,
            (rect.y * s).round() as i64,
            (rect.right() * s).round() as i64,
            (rect.bottom() * s).round() as i64,
        );
        self.clips.push(Clip {
            bounds,
            rect,
            mask: None,
        });
    }

    fn pop_clip(&mut self) {
        let _ = self.clips.pop();
    }

    fn push_opacity(&mut self, opacity: f32) {
        // A group too large to make is drawn straight through, opaque.
        if let Some(layer) = Pixmap::new(self.base.width(), self.base.height()) {
            self.layers.push((layer, opacity));
        }
    }

    fn pop_opacity(&mut self) {
        let Some((layer, opacity)) = self.layers.pop() else {
            return;
        };
        let paint = PixmapPaint {
            opacity,
            ..PixmapPaint::default()
        };
        if let Some(mut target) = target(self.base, &mut self.layers) {
            target.draw_pixmap(0, 0, layer.as_ref(), &paint, Transform::identity(), None);
        }
    }
}
