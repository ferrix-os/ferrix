//! A glyph as coverage: what [`crate::Fonts::glyph`] hands back, how it is
//! made from an outline, and how it is blended into a pixmap.

use crate::markup::Rgba;

/// One glyph rasterised: 8-bit antialiased coverage, one byte a pixel.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Mask {
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// Where its left column is from the pen, in whole pixels.
    pub left: i32,
    /// Where its top row is above the baseline, in whole pixels: the mask's
    /// first row lands at `baseline - top`.
    pub top: i32,
    /// `width × height` coverage values, rows top first.
    pub coverage: Vec<u8>,
}

/// Where the outline builder puts a glyph: font units onto pixels, y
/// down, slanted and moved right by the pen's fraction of a pixel.
struct Sink {
    /// The path so far.
    builder: tiny_skia::PathBuilder,
    /// Pixels per font unit.
    scale: f32,
    /// The slant, x moved right per unit of y (0 upright).
    skew: f32,
    /// The pen's fraction of a pixel.
    shift: f32,
}

impl Sink {
    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (x + self.skew * y) * self.scale + self.shift,
            -y * self.scale,
        )
    }
}

impl ttf_parser::OutlineBuilder for Sink {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        self.builder.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.point(x, y);
        self.builder.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = self.point(x1, y1);
        let (x, y) = self.point(x, y);
        self.builder.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = self.point(x1, y1);
        let (x2, y2) = self.point(x2, y2);
        let (x, y) = self.point(x, y);
        self.builder.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

/// The largest glyph drawn, in pixels each way: a bound on what a corrupt
/// outline or an absurd size can allocate.
const LARGEST: f32 = 4096.0;

/// Glyph `id` of `face` at `px` filled with antialiasing, its pen `shift`
/// of a pixel right of a whole one. `bold` strokes the outline as well, a
/// twenty-fourth of the size wide as `FreeType`'s `FT_GlyphSlot_Embolden`
/// thickens it; `skew` slants it. `None` for a glyph with no outline.
pub(crate) fn rasterise(
    face: &ttf_parser::Face<'_>,
    id: u16,
    px: f32,
    shift: f32,
    bold: bool,
    skew: Option<f32>,
) -> Option<Mask> {
    if px.is_nan() || px <= 0.0 {
        return None;
    }
    let mut sink = Sink {
        builder: tiny_skia::PathBuilder::new(),
        scale: px / f32::from(face.units_per_em().max(1)),
        skew: skew.unwrap_or(0.0),
        shift,
    };
    let _ = face.outline_glyph(ttf_parser::GlyphId(id), &mut sink)?;
    let outline = sink.builder.finish()?;
    let mut paths = vec![outline];
    if bold {
        let stroke = tiny_skia::Stroke {
            width: px / 24.0,
            line_join: tiny_skia::LineJoin::Round,
            ..tiny_skia::Stroke::default()
        };
        if let Some(stroked) = paths
            .first()
            .and_then(|outline| outline.stroke(&stroke, 1.0))
        {
            paths.push(stroked);
        }
    }
    let (mut left, mut top, mut right, mut bottom) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for path in &paths {
        let bounds = path.bounds();
        left = left.min(bounds.left());
        top = top.min(bounds.top());
        right = right.max(bounds.right());
        bottom = bottom.max(bounds.bottom());
    }
    // Far from the origin or far across, it is not a glyph to draw, and
    // the integer arithmetic below stays well inside `i32`.
    let far = |value: f32| !value.is_finite() || value.abs() > 1_000_000.0;
    if [left, top, right, bottom].into_iter().any(far)
        || right - left > LARGEST
        || bottom - top > LARGEST
    {
        return None;
    }
    // A pixel of margin each way, so that antialiasing is never cut.
    let column = left.floor() as i32 - 1;
    let row = top.floor() as i32 - 1;
    let width = u32::try_from(right.ceil() as i32 + 1 - column).ok()?;
    let height = u32::try_from(bottom.ceil() as i32 + 1 - row).ok()?;
    let mut mask = tiny_skia::Mask::new(width, height)?;
    let place = tiny_skia::Transform::from_translate(-column as f32, -row as f32);
    for path in &paths {
        mask.fill_path(path, tiny_skia::FillRule::Winding, true, place);
    }
    Some(Mask {
        width,
        height,
        left: column,
        top: -row,
        coverage: mask.take(),
    })
}

/// `value × by / 255`, rounded.
fn scale8(value: u8, by: u8) -> u8 {
    let product = u32::from(value) * u32::from(by) + 128;
    u8::try_from((product + (product >> 8)) >> 8).unwrap_or(u8::MAX)
}

/// One pixel of premultiplied RGBA with `color` at coverage `coverage`
/// laid over it, source-over.
fn blend(pixel: &mut [u8], color: Rgba, coverage: u8) {
    let alpha = scale8(color.a, coverage);
    if alpha == 0 {
        return;
    }
    let source = [
        scale8(color.r, alpha),
        scale8(color.g, alpha),
        scale8(color.b, alpha),
        alpha,
    ];
    let keep = 255 - alpha;
    for (channel, source) in pixel.iter_mut().zip(source) {
        *channel = source.saturating_add(scale8(*channel, keep));
    }
}

/// `mask` blended in `color` into `pixmap` with its top-left at
/// (`column`, `row`), whatever of it falls outside the pixmap or outside
/// `clip` -- columns `clip.0..clip.2`, rows `clip.1..clip.3` -- cut off.
pub(crate) fn blend_mask(
    pixmap: &mut tiny_skia::PixmapMut<'_>,
    mask: &Mask,
    (column, row): (i64, i64),
    color: Rgba,
    clip: (i64, i64, i64, i64),
) {
    let pixmap_width = i64::from(pixmap.width());
    let (left, top) = (clip.0.max(0), clip.1.max(0));
    let (right, bottom) = (
        clip.2.min(pixmap_width),
        clip.3.min(i64::from(pixmap.height())),
    );
    let mask_width = mask.width as usize;
    let data = pixmap.data_mut();
    for (y, coverage_row) in mask.coverage.chunks_exact(mask_width.max(1)).enumerate() {
        let target_row = row + y as i64;
        if !(top..bottom).contains(&target_row) {
            continue;
        }
        let first = column.max(left);
        let last = (column + mask_width as i64).min(right);
        if first >= last {
            continue;
        }
        let start = ((target_row * pixmap_width + first) * 4) as usize;
        let end = ((target_row * pixmap_width + last) * 4) as usize;
        let Some(pixels) = data.get_mut(start..end) else {
            continue;
        };
        let skip = (first - column) as usize;
        for (pixel, &coverage) in pixels
            .chunks_exact_mut(4)
            .zip(coverage_row.iter().skip(skip))
        {
            if coverage != 0 {
                blend(pixel, color, coverage);
            }
        }
    }
}

/// The rectangle from `from` to `to`, its edges rounded to whole pixels,
/// filled with `color` source-over.
pub(crate) fn fill_rect(
    pixmap: &mut tiny_skia::PixmapMut<'_>,
    from: (f32, f32),
    to: (f32, f32),
    color: Rgba,
) {
    let clamp = |value: f32, most: u32| value.round().max(0.0).min(most as f32) as usize;
    let (width, height) = (pixmap.width(), pixmap.height());
    let (left, right) = (clamp(from.0, width), clamp(to.0, width));
    let (top, bottom) = (clamp(from.1, height), clamp(to.1, height));
    if left >= right || top >= bottom {
        return;
    }
    let stride = width as usize * 4;
    let data = pixmap.data_mut();
    for y in top..bottom {
        let Some(pixels) = data.get_mut(y * stride + left * 4..y * stride + right * 4) else {
            continue;
        };
        for pixel in pixels.chunks_exact_mut(4) {
            blend(pixel, color, 255);
        }
    }
}
