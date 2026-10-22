//! Showing a buffer smaller than the screen, scaled to it: the arithmetic
//! of a *scaled* mode (`docs/DISPLAY.md` §9.2), for a copying card whose
//! display runs one mode -- Intel's lit plane, efifb.
//!
//! The picture keeps its aspect ratio and is centred, with black bars on
//! the two sides it does not fill, as Linux's panel fitter does by default
//! and as Windows' GPU scaling does. Each screen pixel takes the source
//! pixel under its centre, so an integer ratio (960x540 on 1920x1080) is
//! exact, and a change to the buffer maps to exactly the screen pixels
//! that read it. The copy itself, which writes the screen, is the
//! driver's; this is only which pixel goes where.

use ferrix_displayctl::message::Rect;

/// One buffer size shown on one screen size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Scaler {
    /// The buffer's size, as shown: the mode's.
    pub source: (u32, u32),
    /// Where the picture lies on the screen.
    pub area: Rect,
}

impl Scaler {
    /// `source` on a `screen`: the largest rectangle of `source`'s shape
    /// that fits, centred. `None` for an empty size on either side.
    #[must_use]
    pub fn new(source: (u32, u32), screen: (u32, u32)) -> Option<Self> {
        let (sw, sh) = (u64::from(source.0), u64::from(source.1));
        let (dw, dh) = (u64::from(screen.0), u64::from(screen.1));
        if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
            return None;
        }
        // As wide as the screen if that leaves it no taller, else as tall.
        let (width, height) = if dw * sh <= dh * sw {
            (dw, (dw * sh / sw).max(1))
        } else {
            ((dh * sw / sh).max(1), dh)
        };
        Some(Self {
            source,
            area: Rect {
                x: u32::try_from((dw - width) / 2).ok()?,
                y: u32::try_from((dh - height) / 2).ok()?,
                width: u32::try_from(width).ok()?,
                height: u32::try_from(height).ok()?,
            },
        })
    }

    /// Whether the picture is the screen, pixel for pixel: nothing to scale.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        (self.area.x, self.area.y, self.area.width, self.area.height)
            == (0, 0, self.source.0, self.source.1)
    }

    /// The source column screen column `x` of the picture (0 at its left
    /// edge) shows.
    #[must_use]
    pub fn source_x(&self, x: u32) -> u32 {
        under_centre(x, self.source.0, self.area.width)
    }

    /// The source row screen row `y` of the picture shows.
    #[must_use]
    pub fn source_y(&self, y: u32) -> u32 {
        under_centre(y, self.source.1, self.area.height)
    }

    /// The screen rectangle, in screen coordinates, whose pixels read any
    /// of `changed`, a rectangle of the buffer: `None` when none do.
    #[must_use]
    pub fn damaged(&self, changed: Rect) -> Option<Rect> {
        let x_end = changed.x.saturating_add(changed.width).min(self.source.0);
        let y_end = changed.y.saturating_add(changed.height).min(self.source.1);
        if changed.x >= x_end || changed.y >= y_end {
            return None;
        }
        let left = first_reading(changed.x, self.source.0, self.area.width);
        let right = first_reading(x_end, self.source.0, self.area.width);
        let top = first_reading(changed.y, self.source.1, self.area.height);
        let bottom = first_reading(y_end, self.source.1, self.area.height);
        (left < right && top < bottom).then(|| Rect {
            x: self.area.x + left,
            y: self.area.y + top,
            width: right - left,
            height: bottom - top,
        })
    }
}

/// The source index under the centre of destination index `at`, of
/// `source` over `shown`: `floor((2 at + 1) source / (2 shown))`, clamped.
fn under_centre(at: u32, source: u32, shown: u32) -> u32 {
    if shown == 0 || source == 0 {
        return 0;
    }
    let index = (2 * u64::from(at) + 1) * u64::from(source) / (2 * u64::from(shown));
    u32::try_from(index)
        .unwrap_or(u32::MAX)
        .min(source.saturating_sub(1))
}

/// The first destination index whose source index is `from` or more: the
/// least `d` with `(2 d + 1) source >= 2 from shown`, at most `shown`.
fn first_reading(from: u32, source: u32, shown: u32) -> u32 {
    if source == 0 {
        return 0;
    }
    let need = 2 * u64::from(from) * u64::from(shown);
    let source = u64::from(source);
    let index = if need <= source {
        0
    } else {
        (need - source).div_ceil(2 * source)
    };
    u32::try_from(index).unwrap_or(u32::MAX).min(shown)
}
