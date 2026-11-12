//! The boot splash's picture: the FX mark on the brand's ink, with its rust
//! slash lit from the foot up to a level (`docs/SPLASH.md`).
//!
//! Two programs draw it, one after the other, on the same screen: the UEFI
//! loader into the framebuffer firmware left, and `/bin/splash` on
//! `/dev/dri/card0` once a display driver has the screen. They agree on the
//! picture by drawing it with this crate, so the second one's first frame is
//! the first one's last, pixel for pixel, and nothing jumps.
//!
//! The picture is spans of one colour on a background of one colour. An edge
//! pixel is the two blended by how much of it the shape covers, which needs
//! no read of the screen: the loader's screen is a device mapping it must not
//! read, and the background is known. Nothing here allocates, divides by a
//! value it was given unchecked, or holds a float.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

/// The screen behind the mark, as `0xRRGGBB`: the brand's ink.
pub const INK: u32 = 0x0D_0F_12;
/// The mark's grey.
pub const MARK: u32 = 0xCB_CD_D1;
/// The slash where the boot has been: the brand's rust.
pub const RUST: u32 = 0xFF_7A_2B;
/// The slash where it has not: the brand's line colour.
pub const UNLIT: u32 = 0x26_2C_35;

/// The level of a slash lit from foot to tip.
pub const FULL: u32 = 1000;

/// The level the loader leaves the slash at: the kernel is loaded and about
/// to run.
pub const LOADED: u32 = 100;

/// Where a picture goes: a row of a screen, from one column to before
/// another, in one colour.
pub trait Canvas {
    /// Paint row `y` from column `from` to before `to` in `rgb`
    /// (`0xRRGGBB`). The mark never asks for a pixel outside the box it was
    /// placed in.
    fn span(&mut self, y: u32, from: u32, to: u32, rgb: u32);
}

/// A corner of a shape, in half units of the logo's drawing
/// (`docs/brand/src/mark.svgfrag`), which has one coordinate ending in .5.
type Point = (i64, i64);

/// The drawing's width, in half units.
const WIDTH: i64 = 1226;
/// Its height.
const HEIGHT: i64 = 996;
/// How far down the slash reaches.
const SLASH_FOOT: i64 = 492;

/// The F whose leg runs out into the X.
const BODY: [Point; 12] = [
    (0, 0),
    (920, 0),
    (740, 196),
    (206, 196),
    (206, 346),
    (600, 346),
    (1146, 996),
    (868, 996),
    (484, 542),
    (206, 542),
    (206, 996),
    (0, 996),
];
/// The chip under the middle bar.
const CHIP: [Point; 5] = [(255, 828), (476, 592), (590, 728), (344, 996), (255, 996)];
/// The X's other stroke.
const SLASH: [Point; 4] = [(984, 0), (1226, 0), (770, 492), (656, 354)];

/// The most edges of one shape a row crosses.
const CROSSINGS: usize = 8;
/// Parts of a pixel an edge's place is kept to.
const SUB: i64 = 256;
/// The smallest mark drawn, in rows: under it the slash is a few pixels and
/// says nothing.
const SMALLEST: u32 = 16;
/// The mark's height as a share of the screen's shorter side: one fifth.
const SHARE: u32 = 5;

/// The mark, placed on a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// Its left edge, in pixels.
    left: u32,
    /// Its top edge.
    top: u32,
    /// Its height.
    height: u32,
}

impl Mark {
    /// The mark in the middle of a `width` by `height` screen, a fifth of
    /// the shorter side tall; `None` on a screen too small to show it.
    #[must_use]
    pub const fn centred(width: u32, height: u32) -> Option<Self> {
        let shorter = if width < height { width } else { height };
        Self::at_height(width, height, shorter / SHARE)
    }

    /// The mark `tall` pixels high in the middle of a `width` by `height`
    /// screen; `None` when it does not fit.
    #[must_use]
    pub const fn at_height(width: u32, height: u32, tall: u32) -> Option<Self> {
        if tall < SMALLEST || tall > height {
            return None;
        }
        let mark = Self {
            left: 0,
            top: (height - tall) / 2,
            height: tall,
        };
        let wide = mark.width();
        if wide > width {
            return None;
        }
        Some(Self {
            left: (width - wide) / 2,
            ..mark
        })
    }

    /// The mark `tall` pixels high with its corner at (`left`, `top`);
    /// `None` when it is too small to draw. The caller keeps it on its
    /// screen.
    #[must_use]
    pub const fn at(left: u32, top: u32, tall: u32) -> Option<Self> {
        if tall < SMALLEST {
            return None;
        }
        Some(Self {
            left,
            top,
            height: tall,
        })
    }

    /// Its left edge, in pixels.
    #[must_use]
    pub const fn left(&self) -> u32 {
        self.left
    }

    /// Its top edge.
    #[must_use]
    pub const fn top(&self) -> u32 {
        self.top
    }

    /// Its height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Its width: the drawing's proportions, rounded up.
    #[must_use]
    pub const fn width(&self) -> u32 {
        let wide = (self.height as u64 * WIDTH as u64).div_ceil(HEIGHT as u64);
        if wide > u32::MAX as u64 {
            u32::MAX
        } else {
            wide as u32
        }
    }

    /// The first row under it.
    #[must_use]
    pub const fn bottom(&self) -> u32 {
        self.top.saturating_add(self.height)
    }

    /// Draw the whole mark with the slash lit to `level` of [`FULL`]. The
    /// background is the caller's: [`INK`], already painted.
    pub fn draw(&self, canvas: &mut impl Canvas, level: u32) {
        for row in 0..self.height {
            self.shape(canvas, row, &BODY, MARK);
            self.shape(canvas, row, &CHIP, MARK);
            self.shape(canvas, row, &SLASH, self.slash_colour(row, level));
        }
    }

    /// Bring a slash drawn at level `from` to level `to`, painting only the
    /// rows whose colour changes.
    pub fn light(&self, canvas: &mut impl Canvas, from: u32, to: u32) {
        for row in 0..self.height {
            let colour = self.slash_colour(row, to);
            if colour != self.slash_colour(row, from) {
                self.shape(canvas, row, &SLASH, colour);
            }
        }
    }

    /// The slash's colour on `row` at `level`: lit below the level's line,
    /// which rises from the slash's foot to its tip.
    fn slash_colour(&self, row: u32, level: u32) -> u32 {
        let (centre, rows) = self.centre(row);
        let unlit = i64::from(FULL.saturating_sub(level.min(FULL)));
        // The row's centre is at centre/rows half units; the line is at
        // SLASH_FOOT * unlit / FULL.
        if centre * i64::from(FULL) >= SLASH_FOOT * unlit * rows {
            RUST
        } else {
            UNLIT
        }
    }

    /// Row `row`'s centre as a fraction of the drawing's height in half
    /// units: numerator and denominator.
    fn centre(&self, row: u32) -> (i64, i64) {
        (
            (2 * i64::from(row) + 1) * HEIGHT,
            2 * i64::from(self.height),
        )
    }

    /// Paint what of `shape` lies on `row` in `rgb`.
    fn shape(&self, canvas: &mut impl Canvas, row: u32, shape: &[Point], rgb: u32) {
        let mut places = [0_i64; CROSSINGS];
        let count = self.crossings(row, shape, &mut places);
        let Some(found) = places.get_mut(..count) else {
            return;
        };
        found.sort_unstable();
        let y = self.top.saturating_add(row);
        for &[enters, leaves] in found.as_chunks::<2>().0 {
            self.stroke(canvas, y, (enters, leaves), rgb);
        }
    }

    /// Where `shape`'s edges cross `row`'s centre line, in [`SUB`]ths of a
    /// pixel from the mark's left edge; the number found.
    fn crossings(&self, row: u32, shape: &[Point], places: &mut [i64; CROSSINGS]) -> usize {
        let (centre, rows) = self.centre(row);
        let mut count = 0;
        let ends = shape.iter().cycle().skip(1);
        for (&(x0, y0), &(x1, y1)) in shape.iter().zip(ends) {
            let (top, bottom) = if y0 < y1 { (y0, y1) } else { (y1, y0) };
            // A level edge crosses nothing; one that ends on the line counts
            // at its top end only, so a corner is one crossing.
            if centre < top * rows || centre >= bottom * rows {
                continue;
            }
            // x0 + (x1 - x0) * (centre/rows - y0) / (y1 - y0), in half
            // units, then in SUBths of a pixel: a half unit is
            // height / HEIGHT pixels.
            let over = (y1 - y0) * rows;
            let half_units = x0 * over + (x1 - x0) * (centre - y0 * rows);
            let place = half_units * i64::from(self.height) * SUB / (over * HEIGHT);
            if let Some(slot) = places.get_mut(count) {
                *slot = place;
                count += 1;
            }
        }
        // A closed shape is crossed an even number of times.
        count & !1
    }

    /// Paint row `y` between two crossings: whole pixels in `rgb`, and each
    /// end's pixel blended with [`INK`] by how much of it is inside.
    fn stroke(&self, canvas: &mut impl Canvas, y: u32, (enters, leaves): (i64, i64), rgb: u32) {
        let limit = i64::from(self.width()) * SUB;
        let enters = enters.clamp(0, limit);
        let leaves = leaves.clamp(0, limit);
        if leaves <= enters {
            return;
        }
        let (first, last) = (enters / SUB, leaves / SUB);
        let column = |pixel: i64| self.left.saturating_add(u32::try_from(pixel).unwrap_or(0));
        let mut dot = |pixel: i64, inside: i64| {
            if inside > 0 {
                let at = column(pixel);
                canvas.span(y, at, at.saturating_add(1), blend(rgb, inside));
            }
        };
        if first == last {
            dot(first, leaves - enters);
            return;
        }
        let lead = enters % SUB;
        let whole_from = if lead == 0 { first } else { first + 1 };
        if lead != 0 {
            dot(first, SUB - lead);
        }
        dot(last, leaves % SUB);
        if whole_from < last {
            canvas.span(y, column(whole_from), column(last), rgb);
        }
    }
}

/// `rgb` over [`INK`] where the shape covers `inside` [`SUB`]ths of the pixel.
fn blend(rgb: u32, inside: i64) -> u32 {
    let inside = u32::try_from(inside.clamp(0, SUB)).unwrap_or(0);
    let outside = 256 - inside;
    let channel = |shift: u32| {
        let over = (rgb >> shift) & 0xFF;
        let under = (INK >> shift) & 0xFF;
        ((over * inside + under * outside + 128) / 256) << shift
    };
    channel(16) | channel(8) | channel(0)
}
