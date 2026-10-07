//! The frame being drawn: a tiny-skia pixmap in the canvas byte order the
//! crate docs describe, and the drawing operations clipped to damage.

use std::collections::BTreeMap;

use tiny_skia::{BlendMode, FilterQuality, Paint, Pattern, Pixmap, PixmapRef, Shader, SpreadMode};

use crate::blur::{Block, Blur};
use crate::damage::{bounding, intersect, is_empty};
use crate::gradient::Axis;
use crate::{Color, Damage, Error, Format, Gradient, Rect, Surface, Target};

/// The largest width or height a canvas, surface or target may have: past
/// a 16K output, and small enough that every coordinate is exact in the
/// `f32` tiny-skia draws with.
pub const MAX_SIZE: u32 = 16384;

/// A frame being drawn, the size of the output.
///
/// It starts opaque black and stays opaque: clearing ignores alpha, and
/// every other operation blends over what is there. Each operation draws
/// only inside the [`Damage`] it is given and adds what it wrote to the
/// canvas's own record, which [`Canvas::take_damage`] hands over.
#[derive(Debug, Clone)]
pub struct Canvas {
    pixmap: Pixmap,
    damage: Damage,
}

/// tiny-skia's colour for `color`, with red and blue swapped into canvas
/// order.
fn skia_color(color: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color.blue(), color.green(), color.red(), color.alpha())
}

/// A paint with no anti-aliasing, on the `f32` pipeline.
fn paint(shader: Shader<'_>, blend_mode: BlendMode) -> Paint<'_> {
    Paint {
        shader,
        blend_mode,
        anti_alias: false,
        force_hq_pipeline: true,
        ..Paint::default()
    }
}

/// tiny-skia's rectangle for `rect`, which lies inside a canvas, so every
/// edge is exact.
fn skia_rect(rect: Rect) -> Option<tiny_skia::Rect> {
    tiny_skia::Rect::from_xywh(
        rect.x as f32,
        rect.y as f32,
        rect.width as f32,
        rect.height as f32,
    )
}

/// `value`, known to be inside a canvas, as an index.
fn index(value: i64) -> usize {
    usize::try_from(value).unwrap_or(0)
}

impl Canvas {
    /// An opaque black `width` × `height` canvas.
    ///
    /// # Errors
    ///
    /// [`Error::Size`] for a size of zero or over [`MAX_SIZE`].
    pub fn new(width: u32, height: u32) -> Result<Self, Error> {
        let size = Error::Size { width, height };
        if width == 0 || height == 0 || width > MAX_SIZE || height > MAX_SIZE {
            return Err(size);
        }
        let mut pixmap = Pixmap::new(width, height).ok_or(size)?;
        pixmap.fill(tiny_skia::Color::BLACK);
        Ok(Self {
            pixmap,
            damage: Damage::new(),
        })
    }

    /// The width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    /// The height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    /// The canvas as a rectangle at the origin.
    #[must_use]
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, i64::from(self.width()), i64::from(self.height()))
    }

    /// The frame as `XRGB8888` bytes, four a pixel with no padding: blue,
    /// green, red and an X byte of `0xFF`.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.pixmap.data()
    }

    /// The same bytes, to write: what [`crate::Backdrop`] keeps up to date a
    /// row at a time.
    pub(crate) fn data_mut(&mut self) -> &mut [u8] {
        self.pixmap.data_mut()
    }

    /// Pixel (`x`, `y`) as an `XRGB8888` value, if it is on the canvas.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<u32> {
        if x >= self.width() || y >= self.height() {
            return None;
        }
        let start = usize::try_from(u64::from(y) * u64::from(self.width()) + u64::from(x)).ok()?;
        let bytes = self.data().get(start.checked_mul(4)?..)?.first_chunk()?;
        Some(u32::from_le_bytes(*bytes))
    }

    /// What the operations since the last call wrote.
    #[must_use]
    pub const fn damage(&self) -> &Damage {
        &self.damage
    }

    /// What the operations since the last call wrote, leaving the record
    /// empty.
    pub fn take_damage(&mut self) -> Damage {
        core::mem::take(&mut self.damage)
    }

    /// The parts of `rect` inside both `damage` and the canvas, disjoint.
    fn clips(&self, rect: Rect, damage: &Damage) -> Vec<Rect> {
        let bounds = self.bounds();
        match intersect(rect, bounds) {
            Some(rect) => damage
                .rects()
                .iter()
                .filter_map(|&damaged| intersect(rect, damaged))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Draw into `clips` a band of rows at a time, the bands spread over the
    /// machine's cores ([`crate::cores`]).
    ///
    /// `draw` is given a band's bytes, the canvas row the band begins at,
    /// and the parts of `clips` inside it with that row taken off, so that
    /// it draws into the band as into a canvas of its own. A few pixels are
    /// one band, which is the whole canvas, and no thread is started.
    fn in_bands(&mut self, clips: &[Rect], draw: impl Fn(&mut [u8], i64, &[Rect]) + Sync) {
        let area: usize = clips
            .iter()
            .map(|clip| index(clip.width) * index(clip.height))
            .sum();
        let threads = crate::cores::threads_for(area);
        let Some(covered) = bounding(clips).filter(|_| threads > 1) else {
            draw(self.pixmap.data_mut(), 0, clips);
            return;
        };
        let stride = index(i64::from(self.width())) * 4;
        let tall = index(covered.height).div_ceil(threads).max(1);
        let Some(rows) = self
            .pixmap
            .data_mut()
            .get_mut(index(covered.y) * stride..index(covered.bottom()) * stride)
        else {
            return;
        };
        compositor_fan::fan_out(|fan| {
            for (at, band) in rows.chunks_mut(tall * stride).enumerate() {
                let top = covered.y + i64::try_from(at * tall).unwrap_or(0);
                let inside = Rect::new(
                    covered.x,
                    top,
                    covered.width,
                    i64::try_from(band.len() / stride.max(1)).unwrap_or(0),
                );
                let local: Vec<Rect> = clips
                    .iter()
                    .filter_map(|&clip| intersect(clip, inside))
                    .map(|clip| clip.translate(0, top.saturating_neg()))
                    .collect();
                let draw = &draw;
                fan.spawn(move || draw(band, top, &local));
            }
        });
    }

    /// Fill each of `clips` with `paint` and record it.
    fn fill_clips(&mut self, clips: &[Rect], paint: &Paint<'_>) {
        for &clip in clips {
            if let Some(rect) = skia_rect(clip) {
                self.pixmap
                    .fill_rect(rect, paint, tiny_skia::Transform::identity(), None);
                self.damage.add(clip);
            }
        }
    }

    /// Set every pixel in `damage` to `color`, whose alpha is ignored: the
    /// output has none.
    pub fn clear(&mut self, color: Color, damage: &Damage) {
        let opaque = Color(color.0 | 0xFF00_0000);
        let clips = self.clips(self.bounds(), damage);
        let paint = paint(Shader::SolidColor(skia_color(opaque)), BlendMode::Source);
        self.fill_clips(&clips, &paint);
    }

    /// Draw `color`, which is not premultiplied (`0xAARRGGBB` as Hyprland
    /// writes colours), over `rect` within `damage`. A colour with no alpha
    /// draws nothing and damages nothing.
    pub fn fill(&mut self, rect: Rect, color: Color, damage: &Damage) {
        if color.alpha() == 0 || is_empty(rect) {
            return;
        }
        let clips = self.clips(rect, damage);
        let paint = paint(Shader::SolidColor(skia_color(color)), BlendMode::SourceOver);
        self.fill_clips(&clips, &paint);
    }

    /// Draw a border `width` pixels wide in `color` around `rect`, outside
    /// it, within `damage`: Hyprland's border with no rounding, whose inner
    /// edge is the client area's edge. The four strips do not overlap, so a
    /// translucent border is blended once at the corners too.
    pub fn border(&mut self, rect: Rect, width: i64, color: Color, damage: &Damage) {
        if width <= 0 || is_empty(rect) {
            return;
        }
        let outer_x = rect.x.saturating_sub(width);
        let outer_width = rect.width.saturating_add(width.saturating_mul(2));
        let strips = [
            Rect::new(outer_x, rect.y.saturating_sub(width), outer_width, width),
            Rect::new(outer_x, rect.bottom(), outer_width, width),
            Rect::new(outer_x, rect.y, width, rect.height),
            Rect::new(rect.right(), rect.y, width, rect.height),
        ];
        for strip in strips {
            self.fill(strip, color, damage);
        }
    }

    /// Draw a border `width` pixels wide around `rect`, outside it, in
    /// `gradient`: Hyprland's gradient border with no rounding.
    ///
    /// The gradient runs across the whole border box -- `rect` grown by
    /// `width` on every side, which is the box `renderBorder` hands the
    /// shader -- so the four strips are four windows onto one gradient
    /// rather than four gradients of their own. A gradient of one colour is
    /// [`Canvas::border`] exactly, including its blend.
    pub fn border_gradient(
        &mut self,
        rect: Rect,
        width: i64,
        gradient: &Gradient,
        damage: &Damage,
    ) {
        if width <= 0 || is_empty(rect) {
            return;
        }
        if gradient.is_solid() {
            self.border(rect, width, gradient.first(), damage);
            return;
        }
        let outer_x = rect.x.saturating_sub(width);
        let outer_y = rect.y.saturating_sub(width);
        let outer_width = rect.width.saturating_add(width.saturating_mul(2));
        let outer_height = rect.height.saturating_add(width.saturating_mul(2));
        let box_rect = Rect::new(outer_x, outer_y, outer_width, outer_height);
        let strips = [
            Rect::new(outer_x, outer_y, outer_width, width),
            Rect::new(outer_x, rect.bottom(), outer_width, width),
            Rect::new(outer_x, rect.y, width, rect.height),
            Rect::new(rect.right(), rect.y, width, rect.height),
        ];
        let ramp = gradient.ramp();
        for strip in strips {
            let clips = self.clips(strip, damage);
            self.fill_gradient_clips(box_rect, &clips, gradient, &ramp);
        }
    }

    /// Fill `rect` with `gradient` and its corners cut to `radius`: the
    /// shape a rounded window's border is drawn as, before its surface goes
    /// inside it.
    ///
    /// The gradient runs across `rect`, which for a window's border is the
    /// border box, so the colour at the corner is the colour the square
    /// border has at the same corner.
    pub fn fill_rounded_gradient(
        &mut self,
        rect: Rect,
        rounding: Rounding,
        gradient: &Gradient,
        damage: &Damage,
    ) {
        if is_empty(rect) {
            return;
        }
        if gradient.is_solid() {
            self.fill_rounded(rect, rounding, gradient.first(), damage);
            return;
        }
        let clips = self.rounded_clips(rect, rounding, damage);
        let ramp = gradient.ramp();
        self.fill_gradient_clips(rect, &clips, gradient, &ramp);
    }

    /// Draw `gradient` over `clips`, taking each pixel's colour from where
    /// it is inside `box_rect`.
    ///
    /// By hand rather than through a tiny-skia shader, for the reason
    /// [`Canvas::shadow`] is: every pixel has a colour and an alpha of its
    /// own, and tiny-skia's linear gradient is not Hyprland's -- it
    /// interpolates in sRGB, along a true rotation, with anti-aliased
    /// stops. The blend is the same source-over tiny-skia's `f32` pipeline
    /// does, so a gradient of one colour and a fill of that colour agree to
    /// the byte.
    fn fill_gradient_clips(
        &mut self,
        box_rect: Rect,
        clips: &[Rect],
        gradient: &Gradient,
        ramp: &[Color],
    ) {
        let axis = gradient.axis();
        for &clip in clips {
            for y in clip.y..clip.bottom() {
                self.gradient_row(clip, y, box_rect, &axis, ramp);
            }
            self.damage.add(clip);
        }
    }

    /// One row of a gradient, which is where its pixels are written.
    fn gradient_row(&mut self, clip: Rect, y: i64, box_rect: Rect, axis: &Axis, ramp: &[Color]) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window's size in pixels, far inside f32's exact range"
        )]
        let (wide, tall) = (box_rect.width.max(1) as f32, box_rect.height.max(1) as f32);
        #[expect(clippy::cast_precision_loss, reason = "as above")]
        let ny = ((y - box_rect.y) as f32 + 0.5) / tall;
        let last = ramp.len().saturating_sub(1);
        let width = index(i64::from(self.width()));
        let row = index(y) * width;
        for x in clip.x..clip.right() {
            #[expect(clippy::cast_precision_loss, reason = "as above")]
            let nx = ((x - box_rect.x) as f32 + 0.5) / wide;
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a progress between zero and one over an index of a few thousand"
            )]
            let at = (axis.at(nx, ny) * last as f32).round() as usize;
            let Some(color) = ramp.get(at.min(last)) else {
                continue;
            };
            // The canvas holds blue in tiny-skia's red byte; the module
            // comment says why.
            let colour = [color.blue(), color.green(), color.red()];
            let alpha = f32::from(color.alpha()) / 255.0;
            if alpha > 0.0
                && let Some(pixel) = self
                    .pixmap
                    .data_mut()
                    .get_mut((row + index(x)) * 4..(row + index(x)) * 4 + 4)
            {
                over(pixel, colour, alpha);
            }
        }
    }

    /// Draw `surface` with its top-left pixel at `rect`'s, pixel for pixel,
    /// cropped to `rect` and to `damage`: source-over for
    /// [`Format::Argb8888`], a copy that ignores the X byte for
    /// [`Format::Xrgb8888`]. Where the surface is smaller than `rect`,
    /// nothing is drawn.
    pub fn composite(&mut self, surface: &Surface<'_>, rect: Rect, damage: &Damage) {
        self.composite_with(surface, rect, Rounding::none(), 1.0, damage);
    }

    /// The same, with Hyprland's two window decorations applied.
    ///
    /// `rounding` is `decoration:rounding`: the corners are cut to that
    /// radius, and `opacity` is `decoration:active_opacity` and its
    /// relatives, which multiply the surface's alpha as it is drawn. An
    /// opacity of 1 and a rounding of 0 is [`Canvas::composite`] exactly,
    /// including its copy for an opaque surface.
    pub fn composite_with(
        &mut self,
        surface: &Surface<'_>,
        rect: Rect,
        rounding: Rounding,
        opacity: f32,
        damage: &Damage,
    ) {
        let area = Rect::new(
            rect.x,
            rect.y,
            rect.width.min(i64::from(surface.width())),
            rect.height.min(i64::from(surface.height())),
        );
        if is_empty(area) {
            return;
        }
        let opacity = opacity.clamp(0.0, 1.0);
        if opacity == 0.0 {
            return;
        }
        let clips = if !rounding.is_square() {
            self.rounded_clips(area, rounding, damage)
        } else {
            self.clips(area, damage)
        };
        if clips.is_empty() {
            return;
        }
        match surface.format() {
            // The copy is the fast path and the exact one, and it is only
            // exact at full opacity: anything else has to be blended.
            Format::Xrgb8888 if opacity >= 1.0 => self.copy(surface, rect, &clips),
            _ => self.blend(surface, rect, opacity, &clips),
        }
    }

    /// Draw `surface` stretched to fill `rect`, with the same rounding and
    /// opacity [`Canvas::composite_with`] takes.
    ///
    /// For a window part-way through an animation, where the rectangle is
    /// between the size the client drew at and the size it is going to.
    /// Hyprland scales the window's texture for the same reason; the client
    /// is configured at the goal and draws once, not once a frame.
    ///
    /// The sampling is bilinear unless `nearest` says otherwise, which is
    /// the one place in this crate where a pixel is not a pixel. It is also the only place it can be: a
    /// stretched surface has no whole-pixel mapping to stretch along. A
    /// window that is not being animated goes through
    /// [`Canvas::composite_with`] and is exact, which is why every expected
    /// image in this tree still holds.
    pub fn composite_scaled(
        &mut self,
        surface: &Surface<'_>,
        rect: Rect,
        rounding: Rounding,
        opacity: f32,
        nearest: bool,
        damage: &Damage,
    ) {
        if is_empty(rect) || surface.width() == 0 || surface.height() == 0 {
            return;
        }
        let opacity = opacity.clamp(0.0, 1.0);
        if opacity == 0.0 {
            return;
        }
        // The same size is the exact path: an animation that has arrived
        // must draw what a still window draws, to the byte.
        if rect.width == i64::from(surface.width()) && rect.height == i64::from(surface.height()) {
            self.composite_with(surface, rect, rounding, opacity, damage);
            return;
        }
        let clips = if !rounding.is_square() {
            self.rounded_clips(rect, rounding, damage)
        } else {
            self.clips(rect, damage)
        };
        if clips.is_empty() {
            return;
        }
        // An opaque surface at full opacity covers what is under it: every
        // pixel is the surface's, sampled, with nothing to blend. That is
        // every tiled window part-way through a move, and it is done here in
        // whole numbers, a band of rows a core, rather than through the
        // shader's floating-point pipeline -- which on a Cortex-A7 took over
        // a second for a window's frame.
        if surface.format() == Format::Xrgb8888 && opacity >= 1.0 {
            self.stretch_opaque(surface, rect, nearest, &clips);
            return;
        }
        let gathered = opaque_rows(surface);
        let Some(pixmap) = PixmapRef::from_bytes(&gathered, surface.width(), surface.height())
        else {
            return;
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window's size in pixels; the loss is far below one"
        )]
        let (scale_x, scale_y) = (
            rect.width as f32 / f32::from(u16::try_from(surface.width()).unwrap_or(u16::MAX)),
            rect.height as f32 / f32::from(u16::try_from(surface.height()).unwrap_or(u16::MAX)),
        );
        #[expect(
            clippy::cast_precision_loss,
            reason = "as above: a position on a screen"
        )]
        let transform = tiny_skia::Transform::from_translate(rect.x as f32, rect.y as f32)
            .pre_scale(scale_x, scale_y);
        // `windowrule = nearest_neighbor`: the sampling is nearest rather
        // than bilinear, which is what a person writes for pixel art or a
        // retro game -- a stretched sprite must stay a sprite and not
        // become a smear.
        let shader = Pattern::new(
            pixmap,
            SpreadMode::Pad,
            if nearest {
                FilterQuality::Nearest
            } else {
                FilterQuality::Bilinear
            },
            opacity,
            transform,
        );
        let paint = paint(shader, BlendMode::SourceOver);
        self.fill_clips(&clips, &paint);
    }

    /// [`Canvas::composite_scaled`] for an opaque surface at full opacity:
    /// each canvas pixel of `clips` is the surface sampled where `rect`
    /// stretches it to, bilinearly unless `nearest`.
    ///
    /// The sampling is the shader's -- a pixel's centre mapped into the
    /// surface, the four texels round it weighed by how near it falls, the
    /// edges clamped -- in sixteenths of a sixteenth of a pixel rather than
    /// in floating point. Where each column and each row of the rectangle
    /// samples is worked out once, not once a pixel.
    fn stretch_opaque(&mut self, surface: &Surface<'_>, rect: Rect, nearest: bool, clips: &[Rect]) {
        let Some(covered) = bounding(clips) else {
            return;
        };
        let columns = samples(
            covered.x.saturating_sub(rect.x),
            covered.width,
            rect.width,
            surface.width(),
            nearest,
        );
        let rows = samples(
            covered.y.saturating_sub(rect.y),
            covered.height,
            rect.height,
            surface.height(),
            nearest,
        );
        let stretch = Stretch {
            surface,
            columns,
            rows,
            covered,
            width: index(i64::from(self.width())),
        };
        self.in_bands(clips, |band, top, local| {
            for &clip in local {
                stretch.clip(band, top, clip);
            }
        });
        for &clip in clips {
            self.damage.add(clip);
        }
    }

    /// The parts of `rect` inside `damage` and the canvas, with its corners
    /// cut to `radius`.
    ///
    /// A row at a time: the two rows of corners each become one span, and
    /// everything between them is a single rectangle. Anti-aliasing is off
    /// here as everywhere else in this crate, so a pixel is in or out, and a
    /// row's inset is the circle's at that row's centre, rounded to the
    /// nearest pixel.
    pub(crate) fn rounded_clips(
        &self,
        rect: Rect,
        rounding: Rounding,
        damage: &Damage,
    ) -> Vec<Rect> {
        rounded_spans(rect, rounding)
            .into_iter()
            .flat_map(|span| self.clips(span, damage))
            .collect()
    }

    /// Draw a window's drop shadow: `rect` grown by `range` on every side,
    /// with `color` fading out over that distance.
    ///
    /// A port of Hyprland's `shadow.glsl` (`getShadow` and
    /// `pixAlphaRoundedDistance`), which is the only description of the
    /// shape there is. Inside the box, each pixel's alpha is scaled by:
    ///
    /// * **In a corner** -- past the rounded corner's centre on both axes --
    ///   by `((radius - d) / range)^power`, with `d` the distance to that
    ///   centre and `radius` the range plus the window's rounding; nothing
    ///   further out than `radius`.
    /// * **Along an edge**, by `(smallest / range)^power`, with `smallest`
    ///   the distance to the nearest edge of the shadow's own box.
    /// * **Anywhere else**, not at all.
    ///
    /// The window is drawn over it afterwards, as Hyprland draws it: this
    /// does not cut the window's own shape out, because nothing shows
    /// through an opaque window and a translucent one shows its shadow in
    /// Hyprland too.
    ///
    /// This is the one place in the crate that blends a pixel by hand. It
    /// has to be: every pixel has an alpha of its own, and tiny-skia's
    /// shaders take one colour for a whole rectangle. The arithmetic is the
    /// same source-over its `f32` pipeline does -- `s + d × (255 − a) / 255`,
    /// rounded to the nearest byte -- so a shadow and a fill of the same
    /// colour agree.
    pub fn shadow(&mut self, rect: Rect, shadow: &Shadow, damage: &Damage) {
        if shadow.range <= 0 || shadow.color.alpha() == 0 || is_empty(rect) {
            return;
        }
        let full = Rect::new(
            rect.x
                .saturating_sub(shadow.range)
                .saturating_add(shadow.offset.0),
            rect.y
                .saturating_sub(shadow.range)
                .saturating_add(shadow.offset.1),
            rect.width.saturating_add(shadow.range.saturating_mul(2)),
            rect.height.saturating_add(shadow.range.saturating_mul(2)),
        );
        let clips = self.clips(full, damage);
        if clips.is_empty() {
            return;
        }
        let shape = Falloff::new(full, shadow.rounding.radius, shadow.range, shadow.power);
        let alpha = f32::from(shadow.color.alpha()) / 255.0;
        // A shadow's alpha is a function of where a pixel is in its box, and
        // most of the box is the same function of fewer things. At least
        // `inset` inside -- past the fade and clear of the corners -- it is
        // the full alpha. In the bands along the top and bottom edges it is
        // the row's, and along the sides the column's: the fade there is the
        // distance to that one edge. Only the four corners are each pixel's
        // own. So each of those alphas' blend of every byte a pixel can hold
        // is worked out once, by the arithmetic every pixel would have gone
        // through, and the pixels are looked up in it: the same bytes,
        // without three multiplications and a rounding in software for every
        // pixel. On the DK1 a focus change's damage is mostly those bands.
        let inset = shape.inset();
        let colour = [
            shadow.color.blue(),
            shadow.color.green(),
            shadow.color.red(),
        ];
        if full.width < inset.saturating_mul(2) || full.height < inset.saturating_mul(2) {
            for clip in clips {
                for y in clip.y..clip.bottom() {
                    self.shadow_row(clip, y, full, &shape, shadow.color, alpha);
                }
                self.damage.add(clip);
            }
            return;
        }
        let mut shade = Shade {
            full,
            shape,
            alpha,
            color: shadow.color,
            colour,
            inset,
            inside: Blended::of(colour, alpha),
            rows: BTreeMap::new(),
            columns: BTreeMap::new(),
        };
        for clip in clips {
            for y in clip.y..clip.bottom() {
                shade.row(self, clip, y);
            }
            self.damage.add(clip);
        }
    }

    /// One row of a shadow, which is where its pixels are actually written.
    fn shadow_row(
        &mut self,
        clip: Rect,
        y: i64,
        full: Rect,
        shape: &Falloff,
        color: Color,
        alpha: f32,
    ) {
        // The canvas holds blue in tiny-skia's red byte; the module comment
        // says why.
        let colour = [color.blue(), color.green(), color.red()];
        let width = index(i64::from(self.width()));
        let row = index(y) * width;
        for x in clip.x..clip.right() {
            let factor = shape.at(x - full.x, y - full.y) * alpha;
            if factor > 0.0
                && let Some(pixel) = self
                    .pixmap
                    .data_mut()
                    .get_mut((row + index(x)) * 4..(row + index(x)) * 4 + 4)
            {
                over(pixel, colour, factor);
            }
        }
    }

    /// Blur what has already been drawn inside `rect`, in place.
    ///
    /// Hyprland blurs what is *behind* a translucent window: the frame so
    /// far is taken, blurred, and put back before the window is drawn over
    /// it. So this reads the canvas's own pixels and writes them back, and
    /// it has to be called between the things behind and the thing in front.
    ///
    /// The region read is what will be *written* -- the damaged part of
    /// `rect` -- grown by the blur's reach on every side, because a blur
    /// that only read what it writes would pull the frame's own edge
    /// inwards and leave a bright rim; only `rect`'s rounded shape is
    /// written back. Nothing outside the canvas is read: the edges are
    /// clamped, as `GL_CLAMP_TO_EDGE` clamps them.
    ///
    /// Reading the damage rather than the whole rectangle is what makes a
    /// blurred desktop usable. A blinking cursor in a terminal damages a
    /// few hundred pixels; blurring the whole window behind it costs a
    /// tenth of a second on a 1920x1080 screen and blurring what changed
    /// costs a thousandth. The answer is the same either way: a pixel's
    /// blurred value depends on nothing further than the reach away, which
    /// is exactly what the region is grown by.
    ///
    /// The grading in `blur` ([`Blur::contrast`] and its four neighbours)
    /// runs over that read region rather than over the whole monitor as
    /// Hyprland's two extra passes do. Each of them is a function of one
    /// pixel, so the only place the two could differ is where the kernel
    /// clamps at the region's edge -- and the region already reaches a
    /// whole blur past what is written.
    pub fn blur(&mut self, rect: Rect, rounding: Rounding, blur: &Blur, damage: &Damage) {
        self.blur_from(None, rect, rounding, blur, damage);
    }

    /// The same, reading `backdrop` rather than this canvas: the blur of
    /// one canvas written onto another.
    ///
    /// Blurring a strip of a canvas in place is only right while everything
    /// the kernel reads has been drawn by this frame. Outside the damage a
    /// canvas still holds the *last* frame, and the last frame has the
    /// translucent surface drawn over the blur -- so the blur would be a
    /// blur of itself, a smear that grows with every frame. A caller that
    /// blurs in place therefore has to redraw the whole of each blurred
    /// surface its damage touches.
    ///
    /// A `backdrop` nothing is ever drawn over has no such trouble: a strip
    /// of it holds the same pixels a whole frame would have put there, and
    /// a blur of that strip is exact. [`crate::Backdrop`] is what keeps
    /// one, and what calls this.
    pub fn blur_from(
        &mut self,
        backdrop: Option<&Self>,
        rect: Rect,
        rounding: Rounding,
        blur: &Blur,
        damage: &Damage,
    ) {
        if blur.size <= 0 || blur.passes == 0 || is_empty(rect) {
            return;
        }
        // A backdrop of another size is not this screen's, and reading it
        // would draw one monitor's pixels onto another.
        let backdrop =
            backdrop.filter(|from| (from.width(), from.height()) == (self.width(), self.height()));
        let clips = if !rounding.is_square() {
            self.rounded_clips(rect, rounding, damage)
        } else {
            self.clips(rect, damage)
        };
        let Some(written) = bounding(&clips) else {
            return;
        };
        // How far a blurred pixel can read from, which is what the region
        // has to be grown by on every side.
        //
        // Each level of the pyramid is half the one above it, so a tap at
        // `size` on level *k* is `size * 2^k` source pixels. Going down
        // that sums to `size * (2^passes - 1)`, and coming back up sums to
        // the same, so the whole kernel reaches `2 * size * (2^passes - 1)`
        // -- which is inside `2 * size * 2^passes`, and that is this.
        let reach = blur
            .size
            .saturating_mul(1_i64 << blur.passes.min(6))
            .saturating_mul(2);
        // And the lattice the pyramid halves on. The region read is snapped
        // out to it, in the canvas's own coordinates, so that a blur over a
        // damaged strip lands on the same source pixels at every level as a
        // blur over the whole window: the downsample's taps are counted
        // from the region's first pixel, so two regions that begin at
        // different offsets would sample different pixels and differ by a
        // step of a channel where they meet.
        let lattice = 1_i64 << blur.passes.min(6);
        let snapped = |value: i64, up: bool| -> i64 {
            let rounded = value.div_euclid(lattice).saturating_mul(lattice);
            if up && rounded < value {
                rounded.saturating_add(lattice)
            } else {
                rounded
            }
        };
        let (left, top) = (
            snapped(written.x.saturating_sub(reach), false),
            snapped(written.y.saturating_sub(reach), false),
        );
        let (right, bottom) = (
            snapped(written.right().saturating_add(reach), true),
            snapped(written.bottom().saturating_add(reach), true),
        );
        let Some(read) = intersect(
            Rect::new(
                left,
                top,
                right.saturating_sub(left),
                bottom.saturating_sub(top),
            ),
            self.bounds(),
        ) else {
            return;
        };

        let (wide, tall) = (index(read.width), index(read.height));
        let stride = index(i64::from(self.width())) * 4;
        // Every row of it is copied in below before the blur reads any, so
        // it is taken from the kept buffers with whatever it last held.
        let mut block = crate::scratch::bytes(wide * tall * 4);
        let source = backdrop.map_or_else(|| self.pixmap.data(), |from| from.pixmap.data());
        for row in 0..tall {
            let from = (index(read.y) + row) * stride + index(read.x) * 4;
            let to = row * wide * 4;
            if let (Some(source), Some(target)) = (
                source.get(from..from + wide * 4),
                block.get_mut(to..to + wide * 4),
            ) {
                target.copy_from_slice(source);
            }
        }
        crate::blur::blur(
            &mut block,
            &Block {
                width: wide,
                height: tall,
                origin: (read.x, read.y),
                screen: (self.width(), self.height()),
            },
            blur,
        );

        for clip in clips {
            for y in clip.y..clip.bottom() {
                let from = (index(y - read.y) * wide + index(clip.x - read.x)) * 4;
                let to = index(y) * stride + index(clip.x) * 4;
                let len = index(clip.width) * 4;
                if let (Some(source), Some(target)) = (
                    block.get(from..from + len),
                    self.pixmap.data_mut().get_mut(to..to + len),
                ) {
                    target.copy_from_slice(source);
                }
            }
            self.damage.add(clip);
        }
        crate::scratch::bytes_back(block);
    }

    /// Fill `rect` with `color` and its corners cut to `radius`: the shape a
    /// rounded window's border is drawn as, before its surface is put inside
    /// it.
    pub fn fill_rounded(&mut self, rect: Rect, rounding: Rounding, color: Color, damage: &Damage) {
        if color.alpha() == 0 || is_empty(rect) {
            return;
        }
        let clips = self.rounded_clips(rect, rounding, damage);
        let paint = paint(Shader::SolidColor(skia_color(color)), BlendMode::SourceOver);
        self.fill_clips(&clips, &paint);
    }

    /// Copy an `XRGB8888` surface drawn at `rect` into `clips`, making each
    /// pixel opaque.
    fn copy(&mut self, surface: &Surface<'_>, rect: Rect, clips: &[Rect]) {
        let width = index(i64::from(self.width()));
        self.in_bands(clips, |band, top, local| {
            for &clip in local {
                copy_rows(
                    band,
                    width,
                    surface,
                    rect.translate(0, top.saturating_neg()),
                    clip,
                );
            }
        });
        for &clip in clips {
            self.damage.add(clip);
        }
    }

    /// Blend a premultiplied `ARGB8888` surface drawn at `rect` into
    /// `clips`, each canvas pixel over exactly one surface pixel.
    ///
    /// This was tiny-skia's pattern shader with nearest sampling at a
    /// whole-pixel offset, and is now that shader's arithmetic written out
    /// ([`over_row`]): the same bytes, without a floating-point pipeline
    /// run for every pixel of a window. On a Cortex-A7, which has no SIMD
    /// the shader's portable code can use, that pipeline took a quarter of
    /// a second for a translucent window's frame; and `foot`, the terminal
    /// people run, always hands over `ARGB8888`.
    ///
    /// The surface's rows are read where they are -- a padded buffer needs
    /// no gathering into tight rows first -- a band of rows a core. An
    /// `XRGB8888` surface drawn at less than full opacity is blended as
    /// opaque, its X byte read as a full alpha, as it always was.
    fn blend(&mut self, surface: &Surface<'_>, rect: Rect, opacity: f32, clips: &[Rect]) {
        let blend = Blend {
            surface,
            rect,
            opaque: surface.format() == Format::Xrgb8888,
            opacity,
            width: index(i64::from(self.width())),
        };
        self.in_bands(clips, |band, top, local| {
            for &clip in local {
                blend.clip(band, top, clip);
            }
        });
        for &clip in clips {
            self.damage.add(clip);
        }
    }

    /// Copy each of `clips` out of `other`, which must be this canvas's
    /// size, and record it.
    ///
    /// How a window gets the blur of what is behind it from a
    /// [`crate::Backdrop`]: the blur is there already, and the window's
    /// shape is the clips.
    pub(crate) fn copy_from(&mut self, other: &Self, clips: &[Rect]) {
        if (other.width(), other.height()) != (self.width(), self.height()) {
            return;
        }
        let width = index(i64::from(self.width()));
        for &clip in clips {
            let Some(clip) = intersect(clip, self.bounds()) else {
                continue;
            };
            let len = index(clip.width) * 4;
            for y in clip.y..clip.bottom() {
                let start = (index(y) * width + index(clip.x)) * 4;
                if let (Some(source), Some(target)) = (
                    other.pixmap.data().get(start..start + len),
                    self.pixmap.data_mut().get_mut(start..start + len),
                ) {
                    target.copy_from_slice(source);
                }
            }
            self.damage.add(clip);
        }
    }

    /// Copy the pixels in `damage` into `target`, which must be the canvas's
    /// size. Nothing outside `damage`, and none of a row's padding past its
    /// last pixel, is written.
    ///
    /// # Errors
    ///
    /// [`Error::Mismatch`] when the target is another size.
    pub fn present(&self, target: &mut Target<'_>, damage: &Damage) -> Result<(), Error> {
        if (target.width(), target.height()) != (self.width(), self.height()) {
            return Err(Error::Mismatch {
                canvas: (self.width(), self.height()),
                target: (target.width(), target.height()),
            });
        }
        let width = index(i64::from(self.width()));
        for clip in damage.clipped(self.bounds()).rects() {
            let len = index(clip.width) * 4;
            for y in clip.y..clip.bottom() {
                let start = (index(y) * width + index(clip.x)) * 4;
                let (Some(src), Some(dst)) = (
                    self.pixmap.data().get(start..start + len),
                    u32::try_from(clip.x)
                        .ok()
                        .zip(u32::try_from(y).ok())
                        .and_then(|(x, y)| target.span_mut(x, y, len)),
                ) else {
                    continue;
                };
                dst.copy_from_slice(src);
            }
        }
        Ok(())
    }
}

/// Copy the part of `surface`, drawn at `rect`, that `clip` covers into
/// `data`, rows `width` pixels wide whose first is the row `rect` and `clip`
/// count from.
fn copy_rows(data: &mut [u8], width: usize, surface: &Surface<'_>, rect: Rect, clip: Rect) {
    let len = index(clip.width) * 4;
    let from = index(clip.x.saturating_sub(rect.x)) * 4;
    for y in clip.y..clip.bottom() {
        let Some(row) = u32::try_from(y.saturating_sub(rect.y))
            .ok()
            .and_then(|row| surface.row(row))
        else {
            continue;
        };
        let start = (index(y) * width + index(clip.x)) * 4;
        if let (Some(src), Some(dst)) =
            (row.get(from..from + len), data.get_mut(start..start + len))
        {
            copy_opaque(dst, src);
        }
    }
}

/// Copy `src` over `dst` pixel for pixel, taking the three colour bytes and
/// making each pixel opaque: an `XRGB8888` client leaves its X byte zero, and
/// a canvas pixel with a zero alpha would draw nothing.
fn copy_opaque(dst: &mut [u8], src: &[u8]) {
    // A word a pixel, with the alpha byte set in it, rather than four byte
    // stores: the same bytes, in a loop the compiler can widen.
    for (dst, src) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        if let (Ok(dst), Ok(src)) = (<&mut [u8; 4]>::try_from(dst), <[u8; 4]>::try_from(src)) {
            *dst = (u32::from_le_bytes(src) | 0xFF00_0000).to_le_bytes();
        }
    }
}

/// Where each of `count` pixels, from `first` into a span `span` pixels
/// long, samples a surface `size` texels long: the two texels either side of
/// the pixel's centre and how far towards the second it is, out of 256.
///
/// The centre of pixel `i` falls at texel `(i + 0.5) * size / span - 0.5`,
/// clamped to the surface's edges as the shader's `Pad` clamps it; nearest
/// sampling takes the texel that centre is in and no second.
fn samples(first: i64, count: i64, span: i64, size: u32, nearest: bool) -> Vec<(u32, u32, u32)> {
    let span = span.max(1);
    let last = i64::from(size).saturating_sub(1).max(0);
    (0..count.max(0))
        .map(|at| {
            let pixel = first.saturating_add(at);
            // In 1/65536ths of a texel.
            let centre = pixel
                .saturating_mul(2)
                .saturating_add(1)
                .saturating_mul(i64::from(size))
                .saturating_mul(1 << 16)
                / span.saturating_mul(2);
            let texel = |value: i64| u32::try_from(value.clamp(0, last)).unwrap_or(0);
            if nearest {
                let at = texel(centre >> 16);
                return (at, at, 0);
            }
            let position = centre.saturating_sub(1 << 15);
            if position <= 0 {
                return (0, 0, 0);
            }
            let whole = position >> 16;
            let part = u32::try_from((position >> 8) & 0xFF).unwrap_or(0);
            (texel(whole), texel(whole.saturating_add(1)), part)
        })
        .collect()
}

/// A surface being stretched by [`Canvas::stretch_opaque`], and where each
/// column and row of what it covers samples it.
struct Stretch<'a> {
    surface: &'a Surface<'a>,
    /// For each column of `covered`: the two texels and the weight.
    columns: Vec<(u32, u32, u32)>,
    /// For each row of `covered`: the two rows and the weight.
    rows: Vec<(u32, u32, u32)>,
    /// The part of the canvas the columns and rows begin at.
    covered: Rect,
    /// The canvas's width in pixels.
    width: usize,
}

impl Stretch<'_> {
    /// Draw `clip` of a band of canvas rows beginning at row `top`.
    fn clip(&self, band: &mut [u8], top: i64, clip: Rect) {
        let from = index(clip.x.saturating_sub(self.covered.x));
        let columns = self.columns.get(from..).unwrap_or(&[]);
        for y in clip.y..clip.bottom() {
            let at = index(y.saturating_add(top).saturating_sub(self.covered.y));
            let Some(&(first, second, down)) = self.rows.get(at) else {
                continue;
            };
            let start = (index(y) * self.width + index(clip.x)) * 4;
            let end = start + index(clip.width) * 4;
            if let (Some(upper), Some(lower), Some(out)) = (
                self.surface.row(first),
                self.surface.row(second),
                band.get_mut(start..end),
            ) {
                stretch_row(out, (upper, lower), down, columns);
            }
        }
    }
}

/// One row of [`Canvas::stretch_opaque`]: each pixel of `out` mixed from the
/// `rows` above and below its centre, `down` 256ths of the way to the lower,
/// at the texels `columns` names.
fn stretch_row(out: &mut [u8], rows: (&[u8], &[u8]), down: u32, columns: &[(u32, u32, u32)]) {
    let texel = |row: &[u8], at: u32| {
        let at = (at as usize).wrapping_mul(4);
        row.get(at..at.wrapping_add(4))
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map_or(0, u32::from_le_bytes)
    };
    let (upper, lower) = rows;
    for (pixel, &(left, right, across)) in out.chunks_exact_mut(4).zip(columns) {
        let above = mix(texel(upper, left), texel(upper, right), across);
        let below = mix(texel(lower, left), texel(lower, right), across);
        let value = mix(above, below, down) | 0xFF00_0000;
        pixel.copy_from_slice(&value.to_le_bytes());
    }
}

/// `a` and `b`, two pixels, mixed `towards` 256ths of the way to `b`, two
/// channels at a time: blue and red in one word, green and the fourth byte
/// in another, each channel sixteen bits apart so neither spills.
fn mix(a: u32, b: u32, towards: u32) -> u32 {
    if towards == 0 {
        return a;
    }
    // Each lane is at most 255 x 256, so nothing wraps; the arithmetic says
    // so rather than paying for an overflow check a pixel.
    let back = 256u32.wrapping_sub(towards);
    let lanes = |a: u32, b: u32| a.wrapping_mul(back).wrapping_add(b.wrapping_mul(towards));
    let even = (lanes(a & 0x00FF_00FF, b & 0x00FF_00FF) >> 8) & 0x00FF_00FF;
    let odd = lanes((a >> 8) & 0x00FF_00FF, (b >> 8) & 0x00FF_00FF) & 0xFF00_FF00;
    even | odd
}

/// How far a rounded corner's row is inset from the rectangle's edge.
///
/// `row` counts from the corner's own edge, so row 0 is the outermost and
/// `radius - 1` the innermost. The inset is the corner curve's at that
/// row's centre, rounded to the nearest pixel, because coverage here is
/// all or nothing.
///
/// The curve is `rounding.glsl`'s `distanceWithRounding`: a *superellipse*
/// `(|x|^p + |y|^p)^(1/p) = radius`, where `p` is
/// `decoration:rounding_power`. Two is a circle, which is what every other
/// compositor draws; above two the corner is squarer, which is the
/// "squircle" a person sets the option for, and below two it is pinched.
fn corner_inset(rounding: Rounding, row: i64) -> i64 {
    let radius = rounding.radius;
    #[expect(
        clippy::cast_precision_loss,
        reason = "a radius is a few dozen pixels; the loss is beyond any of them"
    )]
    let (radius_f, row_f) = (radius as f64, row as f64);
    let power = f64::from(rounding.power).clamp(1.0, 10.0);
    let dy = (radius_f - row_f - 0.5).max(0.0);
    // `x = (r^p - y^p)^(1/p)`, which is the circle's `sqrt(r² - y²)` at
    // `p = 2` and is computed the same way for every other power.
    let dx = (radius_f.powf(power) - dy.powf(power))
        .max(0.0)
        .powf(1.0 / power);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is between zero and the radius, which is an i64 already"
    )]
    let inset = (radius_f - dx).round() as i64;
    inset.clamp(0, radius)
}

/// How a corner is cut: how far, and by what curve.
///
/// `decoration:rounding` and `decoration:rounding_power`. They travel
/// together because every place that cuts a corner needs both, and a radius
/// without its power is a corner drawn as a circle whatever the
/// configuration said.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rounding {
    /// `decoration:rounding`: how far the corner is cut, in pixels. Zero is
    /// a square corner.
    pub radius: i64,
    /// `decoration:rounding_power`: the superellipse's exponent. Two is a
    /// circle; above two the corner is squarer.
    pub power: f32,
}

impl Rounding {
    /// Hyprland's `decoration:rounding_power` default, which is a circle.
    pub const POWER: f32 = 2.0;

    /// A square corner.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            radius: 0,
            power: Self::POWER,
        }
    }

    /// A corner of `radius` pixels cut as a circle.
    #[must_use]
    pub const fn circle(radius: i64) -> Self {
        Self {
            radius,
            power: Self::POWER,
        }
    }

    /// Whether the corner is cut at all.
    #[must_use]
    pub const fn is_square(self) -> bool {
        self.radius <= 0
    }
}

impl Default for Rounding {
    fn default() -> Self {
        Self::none()
    }
}

/// A surface's pixels as tight rows the shader can read.
///
/// An opaque surface's fourth byte is the X byte, which a client leaves at
/// whatever it likes; read as alpha it would make the window blotchy, so it
/// is forced to `0xFF`. A premultiplied one is copied as it is.
fn opaque_rows(surface: &Surface<'_>) -> Vec<u8> {
    let opaque = surface.format() == Format::Xrgb8888;
    (0..surface.height())
        .filter_map(|y| surface.row(y))
        .flat_map(|row| row.chunks(4))
        .flat_map(|pixel| {
            [
                pixel.first().copied().unwrap_or(0),
                pixel.get(1).copied().unwrap_or(0),
                pixel.get(2).copied().unwrap_or(0),
                if opaque {
                    0xFF
                } else {
                    pixel.get(3).copied().unwrap_or(0)
                },
            ]
        })
        .collect()
}

/// What a window's drop shadow is: Hyprland's `decoration:shadow:*`.
///
/// Not `Eq`: its rounding carries a power, which is a float, as Hyprland's
/// is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    /// The window's own corner rounding, which the shadow follows.
    pub rounding: Rounding,
    /// `shadow:range`: how far it reaches past the window, in pixels.
    pub range: i64,
    /// `shadow:render_power`: how fast it fades, 1 to 4.
    pub power: u32,
    /// `shadow:color`, whose alpha is the shadow's own.
    pub color: Color,
    /// `shadow:offset`: how far the whole shadow is moved.
    pub offset: (i64, i64),
}

/// A shadow's alpha at each point of its box, as `shadow.glsl` computes it.
#[derive(Clone, Copy, Debug)]
struct Falloff {
    size: (f32, f32),
    /// The rounded corner's centre, inset from the box by `range + rounding`
    /// on both axes: `TOPLEFT` in `renderRoundedShadow`.
    inset: f32,
    range: f32,
    power: i32,
}

impl Falloff {
    fn new(full: Rect, rounding: i64, range: i64, power: u32) -> Self {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window's size and a shadow's range, in pixels"
        )]
        Self {
            size: (full.width as f32, full.height as f32),
            inset: (range.saturating_add(rounding.max(0))) as f32,
            range: range as f32,
            // Hyprland clamps the power to 1..=4 before it reaches the
            // shader.
            power: power.clamp(1, 4) as i32,
        }
    }

    /// How far inside the box every point is past the fade and clear of
    /// the corners, where [`Falloff::at`] is exactly one.
    ///
    /// A pixel's centre is half a pixel into it, so a column `inset` in is
    /// past a corner's centre (which is `inset` in) and at least `range`
    /// from every side; `inset` is `range` and the rounding, never less
    /// than `range`.
    fn inset(&self) -> i64 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a shadow's range and a rounding in pixels, made from integers"
        )]
        let inset = self.inset as i64;
        inset
    }

    /// The alpha at `(x, y)` inside the box, 0 to 1.
    fn at(&self, x: i64, y: i64) -> f32 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a position inside a shadow's box"
        )]
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let (right, bottom) = (self.size.0 - self.inset, self.size.1 - self.inset);
        let radius = self.inset;

        // The four corners, which are the only places both axes are past
        // the rounding's centre.
        let corner = match (px < self.inset, px > right, py < self.inset, py > bottom) {
            (true, _, true, _) => Some((self.inset, self.inset)),
            (true, _, _, true) => Some((self.inset, bottom)),
            (_, true, true, _) => Some((right, self.inset)),
            (_, true, _, true) => Some((right, bottom)),
            _ => None,
        };
        if let Some((cx, cy)) = corner {
            let distance = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            return rounded_distance(distance, radius, self.range, self.power);
        }

        // An edge: the distance to the nearest side of the shadow's own box.
        let smallest = py.min(self.size.1 - py).min(px).min(self.size.0 - px);
        if smallest < self.range {
            return (smallest / self.range).max(0.0).powi(self.power);
        }
        1.0
    }
}

/// `pixAlphaRoundedDistance` from `shadow.glsl`.
fn rounded_distance(distance: f32, radius: f32, range: f32, power: i32) -> f32 {
    if distance > radius {
        return 0.0;
    }
    if distance > radius - range {
        return ((radius - distance) / range).clamp(0.0, 1.0).powi(power);
    }
    1.0
}

/// A shadow being drawn by [`Canvas::shadow`]: its box and falloff, and
/// the tables its bands are blended through, made as each is first met.
struct Shade {
    full: Rect,
    shape: Falloff,
    alpha: f32,
    color: Color,
    /// `color`'s three channels in the canvas's order.
    colour: [u8; 3],
    /// How far in the corners and the fade end.
    inset: i64,
    /// The middle's table, at the full alpha.
    inside: Option<Blended>,
    /// The top and bottom bands' tables, by canvas row.
    rows: BTreeMap<i64, Option<Blended>>,
    /// The side bands' tables, by canvas column.
    columns: BTreeMap<i64, Option<Blended>>,
}

impl Shade {
    /// Row `y` of `clip`: its corner, band and middle pieces, each the way
    /// [`Canvas::shadow`] says.
    fn row(&mut self, canvas: &mut Canvas, clip: Rect, y: i64) {
        let (left, right) = (
            self.full.x.saturating_add(self.inset),
            self.full.right().saturating_sub(self.inset),
        );
        let band = y < self.full.y.saturating_add(self.inset)
            || y >= self.full.bottom().saturating_sub(self.inset);
        let pieces = [
            (clip.x, clip.right().min(left)),
            (clip.x.max(left), clip.right().min(right)),
            (clip.x.max(right), clip.right()),
        ];
        let width = index(i64::from(canvas.width()));
        for (at, (from, to)) in pieces.into_iter().enumerate() {
            if from >= to {
                continue;
            }
            let start = (index(y) * width + index(from)) * 4;
            let end = start + index(to.saturating_sub(from)) * 4;
            match (band, at) {
                // A corner: each pixel its own.
                (true, 0 | 2) => {
                    let span = Rect::new(from, y, to.saturating_sub(from), 1);
                    canvas.shadow_row(span, y, self.full, &self.shape, self.color, self.alpha);
                }
                // A band along the top or the bottom: the row's.
                (true, _) => {
                    let (shape, full, inset, alpha, colour) =
                        (self.shape, self.full, self.inset, self.alpha, self.colour);
                    let table = self.rows.entry(y).or_insert_with(|| {
                        Blended::of(colour, shape.at(inset, y.saturating_sub(full.y)) * alpha)
                    });
                    through(table.as_ref(), canvas.pixmap.data_mut().get_mut(start..end));
                }
                // The middle: the full alpha.
                (false, 1) => {
                    through(
                        self.inside.as_ref(),
                        canvas.pixmap.data_mut().get_mut(start..end),
                    );
                }
                // A band along a side: each column's.
                (false, _) => {
                    let (shape, full, inset, alpha, colour) =
                        (self.shape, self.full, self.inset, self.alpha, self.colour);
                    let make = |x: i64| {
                        Blended::of(colour, shape.at(x.saturating_sub(full.x), inset) * alpha)
                    };
                    let pixels = canvas.pixmap.data_mut().get_mut(start..end);
                    by_column(pixels, from, &mut self.columns, make);
                }
            }
        }
    }
}

/// Blend `pixels` through `table`, when there are both.
fn through(table: Option<&Blended>, pixels: Option<&mut [u8]>) {
    if let (Some(table), Some(pixels)) = (table, pixels) {
        table.apply(pixels);
    }
}

/// Blend `pixels`, a run of a row beginning at column `from`, each through
/// its own column's table, made by `make` the first time a column is met.
fn by_column(
    pixels: Option<&mut [u8]>,
    from: i64,
    tables: &mut BTreeMap<i64, Option<Blended>>,
    make: impl Fn(i64) -> Option<Blended>,
) {
    let Some(pixels) = pixels else {
        return;
    };
    for (x, pixel) in (from..).zip(pixels.chunks_exact_mut(4)) {
        if let Some(table) = tables.entry(x).or_insert_with(|| make(x)).as_ref() {
            table.apply(pixel);
        }
    }
}

/// What [`over`] makes of every byte a canvas pixel can hold, for one
/// colour at one alpha: a table a channel.
///
/// Built by calling [`over`] itself, so a pixel looked up here is the byte
/// that blending it would have written.
struct Blended([[u8; 256]; 3]);

impl Blended {
    /// The table for `colour` at `alpha`, or `None` where the alpha is
    /// nothing, which [`over`] is never called for.
    fn of(colour: [u8; 3], alpha: f32) -> Option<Self> {
        (alpha > 0.0).then(|| Self::new(colour, alpha))
    }

    fn new(colour: [u8; 3], alpha: f32) -> Self {
        let mut table = [[0u8; 256]; 3];
        for level in 0..=255u8 {
            let mut pixel = [level, level, level, 0];
            over(&mut pixel, colour, alpha);
            for (channel, byte) in table.iter_mut().zip(pixel) {
                if let Some(slot) = channel.get_mut(usize::from(level)) {
                    *slot = byte;
                }
            }
        }
        Self(table)
    }

    /// Blend every pixel of `pixels`, canvas bytes, through the table.
    fn apply(&self, pixels: &mut [u8]) {
        let [blue, green, red] = &self.0;
        for pixel in pixels.chunks_exact_mut(4) {
            if let [b, g, r, _] = pixel {
                let look = |table: &[u8; 256], byte: u8| {
                    table.get(usize::from(byte)).copied().unwrap_or(byte)
                };
                (*b, *g, *r) = (look(blue, *b), look(green, *g), look(red, *r));
            }
        }
    }
}

/// A surface being blended by [`Canvas::blend`].
struct Blend<'a> {
    surface: &'a Surface<'a>,
    /// Where it is drawn.
    rect: Rect,
    /// Whether it is `XRGB8888`, whose fourth byte is read as opaque.
    opaque: bool,
    opacity: f32,
    /// The canvas's width in pixels.
    width: usize,
}

impl Blend<'_> {
    /// Blend `clip` of a band of canvas rows beginning at row `top`.
    fn clip(&self, band: &mut [u8], top: i64, clip: Rect) {
        let from = index(clip.x.saturating_sub(self.rect.x)) * 4;
        let len = index(clip.width) * 4;
        for y in clip.y..clip.bottom() {
            let Some(row) = u32::try_from(y.saturating_add(top).saturating_sub(self.rect.y))
                .ok()
                .and_then(|row| self.surface.row(row))
            else {
                continue;
            };
            let start = (index(y) * self.width + index(clip.x)) * 4;
            if let (Some(pixels), Some(into)) =
                (row.get(from..from + len), band.get_mut(start..start + len))
            {
                over_row(into, pixels, self.opaque, self.opacity);
            }
        }
    }
}

/// A premultiplied row over a canvas row, `SourceOver`, byte for byte as
/// tiny-skia's high-precision pipeline blends a pattern at a whole-pixel
/// offset -- the path [`Canvas::blend`] took through it before, which the
/// expected images were drawn by.
///
/// That pipeline loads each byte as `byte * (1 / 255)`, scales the source
/// by the pattern's opacity when it is not one (`scale_1_float`), blends
/// as `dst * (1 - src_alpha) + src` (`source_over_rgba`, a multiply and an
/// add, never fused), clamps to `0..=1`, multiplies by 255 and rounds half
/// to even (`unnorm`, `round_int`). The same `f32` operations in the same
/// order give the same bytes.
///
/// Two kinds of pixel need none of it at full opacity, and are most of any
/// window: an opaque one, whose blend is the source whatever is under it,
/// and an empty one, whose blend is what is under it. Both are exact: the
/// pipeline's result for them rounds to exactly those bytes, as the test
/// against tiny-skia over every alpha, colour and background shows.
fn over_row(into: &mut [u8], pixels: &[u8], opaque: bool, opacity: f32) {
    const FACTOR: f32 = 1.0 / 255.0;
    let whole = opacity >= 1.0;
    for (into, pixel) in into.chunks_exact_mut(4).zip(pixels.chunks_exact(4)) {
        let (Ok(into), Ok(mut source)) =
            (<&mut [u8; 4]>::try_from(into), <[u8; 4]>::try_from(pixel))
        else {
            continue;
        };
        if opaque {
            source[3] = 0xFF;
        }
        if whole {
            if source[3] == 0xFF {
                *into = source;
                continue;
            }
            if source == [0; 4] {
                continue;
            }
        }
        let load = |byte: u8| {
            let value = f32::from(byte) * FACTOR;
            if whole { value } else { value * opacity }
        };
        let keep = 1.0 - load(source[3]);
        for (out, from) in into.iter_mut().zip(source) {
            *out = unnorm((f32::from(*out) * FACTOR) * keep + load(from));
        }
    }
}

/// A channel from `0..=1` to a byte, as tiny-skia's `unnorm` stores it:
/// clamped, times 255, rounded half to even. The rounding is the sum of
/// the value and `2^23` taken away again, which in `f32` is exactly that
/// rounding for any value under `2^23`, and what tiny-skia's portable
/// `round` does; x86's conversion instruction rounds the same way.
fn unnorm(value: f32) -> u8 {
    const WHOLE: f32 = 8_388_608.0;
    let scaled = value.clamp(0.0, 1.0) * 255.0;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a whole number from 0 to 255"
    )]
    let byte = ((scaled + WHOLE) - WHOLE) as u8;
    byte
}

/// Blend one premultiplied colour over one canvas pixel.
///
/// `colour` is not premultiplied; `alpha` is how much of it shows. The
/// arithmetic is tiny-skia's source-over, rounded the way its `f32` pipeline
/// rounds, so a shadow and a fill of the same colour agree to the byte.
fn over(pixel: &mut [u8], colour: [u8; 3], alpha: f32) {
    let alpha = alpha.clamp(0.0, 1.0);
    let keep = 1.0 - alpha;
    for (at, channel) in colour.iter().enumerate() {
        let Some(slot) = pixel.get_mut(at) else {
            continue;
        };
        let source = f32::from(*channel) * alpha;
        let blended = source + f32::from(*slot) * keep;
        *slot = blended.round().clamp(0.0, 255.0) as u8;
    }
}

/// `rect` with its corners cut by `rounding`, as rectangles: a row for each
/// line of the corners and one block for the straight middle. The shape a
/// rounded window's border is filled as, and the box a screenshot blacks
/// out over a window that asked not to be shared.
#[must_use]
pub fn rounded_spans(rect: Rect, rounding: Rounding) -> Vec<Rect> {
    if is_empty(rect) {
        return Vec::new();
    }
    let radius = rounding
        .radius
        .min(rect.width / 2)
        .min(rect.height / 2)
        .max(0);
    let rounding = Rounding { radius, ..rounding };
    if radius == 0 {
        return vec![rect];
    }
    let mut spans = Vec::with_capacity((radius * 2 + 1) as usize);
    for row in 0..radius {
        let inset = corner_inset(rounding, row);
        let width = rect.width.saturating_sub(inset.saturating_mul(2));
        if width <= 0 {
            continue;
        }
        spans.push(Rect::new(rect.x + inset, rect.y + row, width, 1));
        spans.push(Rect::new(rect.x + inset, rect.bottom() - row - 1, width, 1));
    }
    let middle = rect.height.saturating_sub(radius.saturating_mul(2));
    if middle > 0 {
        spans.push(Rect::new(rect.x, rect.y + radius, rect.width, middle));
    }
    spans
}
