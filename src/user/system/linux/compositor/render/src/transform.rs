//! A turned monitor: the frame, drawn upright, put on the screen turned.
//!
//! `monitor = ..., transform, N` says how the monitor stands. Everything
//! above the renderer -- the layout, the decorations, the layer surfaces,
//! the pointer -- works in the monitor as a person reads it, which for a
//! monitor stood on its edge is taller than it is wide. The connector does
//! not know it has been turned and scans out a buffer of its own mode. So
//! somewhere between the two the picture has to be turned, and this is
//! where.
//!
//! # Where Hyprland turns it, and where this does
//!
//! Hyprland draws on a GPU and folds the turn into its projection:
//! `CMonitor::updateMatrix` puts the monitor's transform into
//! `m_projMatrix`, every box drawn is projected through it, and the frame
//! lands in the buffer already turned. Nothing is drawn twice and nothing
//! is copied.
//!
//! A CPU renderer has no projection to fold it into. Drawing every
//! rectangle, border and client buffer turned would mean giving up what
//! makes the software path fast -- rows copied whole, a blur that walks
//! rows, a backdrop kept upright from frame to frame -- for a monitor most
//! people never turn. So the frame is drawn exactly as on an upright
//! monitor, on a [`Canvas`] the size of the monitor as it is read, and the
//! turn happens once, on the way out: [`Canvas::present_transformed`]
//! writes each damaged pixel to the place the transform sends it in the
//! screen's buffer, and hands back the damage in the buffer's own pixels
//! for the card and the night-light. An upright monitor never comes here;
//! its frame is copied by [`Canvas::present`], row by row, exactly as it
//! was before this existed.
//!
//! The price is that a turned monitor's pixels are moved one at a time
//! rather than a row at a time, and only the damaged ones; nothing else of
//! the frame costs more.
//!
//! # The direction
//!
//! Each pixel goes where Hyprland's matrix sends it. `CMonitor::updateMatrix`
//! (`src/output/Monitor.cpp`) builds
//!
//! ```text
//! m_projMatrix = translate(m_pixelSize / 2) * transform(m_transform) * translate(-m_transformedSize / 2)
//! ```
//!
//! with hyprutils' `Mat3x3::transform` table, which is wlroots'. For
//! transform 1 that table's matrix sends `(x, y)` to `(y, -x)`, so a point
//! of a `W` x `H` logical frame goes to `(y - H/2, W/2 - x)` about the
//! centre and to `(y, W - x)` in a buffer `H` wide and `W` tall: the frame's
//! top left is the buffer's bottom left. Hyprland reaches the same place by
//! a second road for the damage and the scissor --
//! `CBox::transform(invertTransform(m_transform), ...)` -- and wlroots by a
//! third, `wlr_box_transform` with `wlr_output_transform_invert`; all three
//! agree, and so does [`point`], in whole pixels (`W - 1 - x`).
//!
//! In words: the buffer holds the picture turned the way the protocol names
//! -- transform 1 is "90 degrees counter-clockwise" -- with the flipped four
//! mirrored left to right before they are turned. To read a picture the
//! connector scans out turned counter-clockwise, a person turns the monitor
//! clockwise, onto its right-hand edge: that is transform 1, and a monitor
//! turned the other way, onto its left-hand edge, is transform 3.

use crate::{Canvas, Damage, Error, Rect, Target};

pub use compositor_config::Transform;

#[cfg(test)]
mod tests;

/// Where the logical frame's pixel `(x, y)` goes in the buffer, for a
/// frame `size` pixels in its own (logical) orientation.
///
/// `size` is the canvas's, which for a quarter turn is the buffer's with
/// its width and height exchanged.
#[must_use]
pub fn point(transform: Transform, size: (i64, i64), (x, y): (i64, i64)) -> (i64, i64) {
    let (width, height) = size;
    let (right, bottom) = (width.saturating_sub(1), height.saturating_sub(1));
    match transform {
        Transform::Normal => (x, y),
        // The picture turned counter-clockwise into the buffer: the frame's
        // top left is the buffer's bottom left, and its top right the
        // buffer's top left.
        Transform::Rotated90 => (y, right.saturating_sub(x)),
        Transform::Rotated180 => (right.saturating_sub(x), bottom.saturating_sub(y)),
        // Clockwise: the frame's top left is the buffer's top right.
        Transform::Rotated270 => (bottom.saturating_sub(y), x),
        Transform::Flipped => (right.saturating_sub(x), y),
        // Mirrored, then turned as 1 is: the diagonal from the top left, so
        // the frame's rows are the buffer's columns.
        Transform::Flipped90 => (y, x),
        Transform::Flipped180 => (x, bottom.saturating_sub(y)),
        // Mirrored, then turned as 3 is: the other diagonal.
        Transform::Flipped270 => (bottom.saturating_sub(y), right.saturating_sub(x)),
    }
}

/// `rect` of the logical frame as the rectangle of the buffer its pixels
/// go to: the same pixels, since every transform sends a rectangle to a
/// rectangle.
#[must_use]
pub fn rect(transform: Transform, size: (i64, i64), rect: Rect) -> Rect {
    if rect.width <= 0 || rect.height <= 0 {
        return Rect::new(0, 0, 0, 0);
    }
    let first = point(transform, size, (rect.x, rect.y));
    let last = point(
        transform,
        size,
        (
            rect.right().saturating_sub(1),
            rect.bottom().saturating_sub(1),
        ),
    );
    let (left, top) = (first.0.min(last.0), first.1.min(last.1));
    Rect::new(
        left,
        top,
        first.0.max(last.0).saturating_sub(left).saturating_add(1),
        first.1.max(last.1).saturating_sub(top).saturating_add(1),
    )
}

/// A frame's damage in the buffer's pixels: each rectangle moved as
/// [`rect`] moves it.
///
/// Disjoint rectangles stay disjoint, since a transform moves every pixel
/// somewhere of its own, so the region is the same size and has as many
/// rectangles.
#[must_use]
pub fn damage(transform: Transform, size: (i64, i64), damage: &Damage) -> Damage {
    damage
        .rects()
        .iter()
        .map(|&held| rect(transform, size, held))
        .collect()
}

/// Write the pixels of `clip` from `from` into `target`, each where
/// `transform` sends it.
///
/// `from` holds logical pixels in rows of `stride` bytes whose first byte
/// is the logical pixel `origin`: the whole canvas, with an origin of
/// `(0, 0)`, or a rectangle a GPU gave back, with its own corner. `size`
/// is the whole logical frame, which is what the transform turns about.
/// `clip` has to lie inside what `from` holds; a pixel outside it, or one
/// that would land outside `target`, is skipped rather than written
/// somewhere else.
///
/// `into` is the buffer pixel `target` begins at: `(0, 0)` for the screen's
/// whole buffer, and a rectangle's corner for a target that is only that
/// rectangle of it, which is what a screenshot of part of a turned monitor
/// is.
#[expect(
    clippy::too_many_arguments,
    reason = "where the pixels are, how they are laid out, which of them, and where they go: \
              a struct for them would be built at each of three calls and read at one"
)]
pub fn copy(
    transform: Transform,
    size: (i64, i64),
    from: &[u8],
    stride: usize,
    origin: (i64, i64),
    clip: Rect,
    target: &mut Target<'_>,
    into: (i64, i64),
) {
    if clip.width <= 0 || clip.height <= 0 {
        return;
    }
    // Where one step right and one step down in the frame go in the buffer:
    // the transform is linear once its corner is known, so each pixel's
    // place is the last one's plus a step rather than worked out afresh.
    let corner = point(transform, size, (clip.x, clip.y));
    let across = point(transform, size, (clip.x.saturating_add(1), clip.y));
    let down = point(transform, size, (clip.x, clip.y.saturating_add(1)));
    let step = |to: (i64, i64)| (to.0.saturating_sub(corner.0), to.1.saturating_sub(corner.1));
    let (right, below) = (step(across), step(down));
    let corner = (
        corner.0.saturating_sub(into.0),
        corner.1.saturating_sub(into.1),
    );
    let (width, height) = (i64::from(target.width()), i64::from(target.height()));
    let pitch = i64::from(target.stride());

    // Which of the clip's columns and rows are written: those whose pixels
    // are inside what `from` holds and land inside the target. Each step
    // moves along one axis of the buffer only, so where a column lands
    // across the buffer depends on the column alone and where a row lands
    // on the row alone: the part of the clip that is written is a rectangle
    // of it, found here once rather than asked of every pixel.
    let along = |step: (i64, i64)| {
        if step.0 == 0 {
            (step.1, corner.1, height)
        } else {
            (step.0, corner.0, width)
        }
    };
    let first = (
        clip.x.saturating_sub(origin.0),
        clip.y.saturating_sub(origin.1),
    );
    let held = (
        i64::try_from(stride / 4).unwrap_or(0),
        i64::try_from(from.len() / stride.max(1)).unwrap_or(0),
    );
    let columns = within(along(right))
        .and_then(|span| cut(span, (0, clip.width)))
        .and_then(|span| {
            cut(
                span,
                (first.0.saturating_neg(), held.0.saturating_sub(first.0)),
            )
        });
    let rows = within(along(below))
        .and_then(|span| cut(span, (0, clip.height)))
        .and_then(|span| {
            cut(
                span,
                (first.1.saturating_neg(), held.1.saturating_sub(first.1)),
            )
        });
    let (Some(columns), Some(rows)) = (columns, rows) else {
        return;
    };

    // In bytes from here: where the clip's pixel at `(column, row)` is read
    // from, and where it goes in the buffer.
    let source = |column: i64, row: i64| -> Option<usize> {
        let line = usize::try_from(first.1.saturating_add(row)).ok()?;
        let at = usize::try_from(first.0.saturating_add(column)).ok()?;
        Some(
            line.saturating_mul(stride)
                .saturating_add(at.saturating_mul(4)),
        )
    };
    let place = |column: i64, row: i64| -> Option<usize> {
        let x = corner
            .0
            .saturating_add(right.0.saturating_mul(column))
            .saturating_add(below.0.saturating_mul(row));
        let y = corner
            .1
            .saturating_add(right.1.saturating_mul(column))
            .saturating_add(below.1.saturating_mul(row));
        usize::try_from(y.saturating_mul(pitch).saturating_add(x.saturating_mul(4))).ok()
    };
    let length = |span: (i64, i64)| usize::try_from(span.1.saturating_sub(span.0)).unwrap_or(0);
    let data = target.data_mut();

    if right.1 == 0 {
        // A row of the frame is a run along one of the buffer's rows:
        // upright, turned half way, or mirrored. Copied a run at a time,
        // forwards or backwards.
        let run = length(columns).saturating_mul(4);
        let last = columns.1.saturating_sub(1);
        for row in rows.0..rows.1 {
            let (Some(read), Some(start), Some(end)) = (
                source(columns.0, row),
                place(columns.0, row),
                place(last, row),
            ) else {
                continue;
            };
            let (Some(pixels), Some(into)) = (
                from.get(read..read.saturating_add(run)),
                data.get_mut(start.min(end)..start.max(end).saturating_add(4)),
            ) else {
                continue;
            };
            if right.0 > 0 {
                into.copy_from_slice(pixels);
            } else {
                for (into, pixel) in into
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .rev()
                    .zip(pixels.as_chunks::<4>().0)
                {
                    *into = *pixel;
                }
            }
        }
        return;
    }

    // A quarter turn: a *column* of the frame is a run along one of the
    // buffer's rows. Walking the frame's rows wrote each pixel a whole
    // buffer row away from the last -- a cache line fetched from memory for
    // four bytes -- which on a Cortex-A7 was most of a turned monitor's
    // frame. So the frame is turned a band of rows at a time: the band's
    // lines stay in the cache while each of its columns is written as one
    // run along a buffer row.
    let mut band = rows.0;
    while band < rows.1 {
        let rows = (band, band.saturating_add(TILE).min(rows.1));
        let reach = length(rows)
            .saturating_sub(1)
            .saturating_mul(stride)
            .saturating_add(4);
        let last = rows.1.saturating_sub(1);
        for column in columns.0..columns.1 {
            let (Some(read), Some(start), Some(end)) = (
                source(column, rows.0),
                place(column, rows.0),
                place(column, last),
            ) else {
                continue;
            };
            let (Some(pixels), Some(into)) = (
                from.get(read..read.saturating_add(reach)),
                data.get_mut(start.min(end)..start.max(end).saturating_add(4)),
            ) else {
                continue;
            };
            let read = pixels
                .chunks(stride.max(1))
                .filter_map(|line| line.get(..4));
            if start <= end {
                for (into, pixel) in into.as_chunks_mut::<4>().0.iter_mut().zip(read) {
                    into.copy_from_slice(pixel);
                }
            } else {
                for (into, pixel) in into.as_chunks_mut::<4>().0.iter_mut().rev().zip(read) {
                    into.copy_from_slice(pixel);
                }
            }
        }
        band = rows.1;
    }
}

/// How many of a frame's rows a quarter turn moves at a time.
///
/// Thirty-two lines of the frame are what a band reads, a few kilobytes
/// held in a first-level cache of thirty-two while every column of the
/// band is written; and each column's run along a buffer row is then two
/// whole cache lines.
const TILE: i64 = 32;

/// The `k` for which `start + step * k` lies in `0..limit`, as a half-open
/// range, for a step of one either way: `(step, start, limit)`. `None` if
/// there are none.
fn within((step, start, limit): (i64, i64, i64)) -> Option<(i64, i64)> {
    let (low, high) = if step > 0 {
        (start.saturating_neg(), limit.saturating_sub(start))
    } else {
        (
            start.saturating_sub(limit).saturating_add(1),
            start.saturating_add(1),
        )
    };
    (low < high).then_some((low, high))
}

/// The part of the half-open range `span` inside `bounds`, or `None`.
fn cut(span: (i64, i64), bounds: (i64, i64)) -> Option<(i64, i64)> {
    let (low, high) = (span.0.max(bounds.0), span.1.min(bounds.1));
    (low < high).then_some((low, high))
}

impl Canvas {
    /// [`Canvas::present`] for a turned monitor: copy the pixels in
    /// `damage` into `target`, each where `transform` sends it, and give
    /// back what was written in the target's own pixels.
    ///
    /// The target is the screen's buffer, which is the canvas's size turned:
    /// a quarter turn exchanges the width and the height. What comes back
    /// is what the card is told changed and what the night-light's ramps
    /// are applied over, both of which are the buffer's business.
    ///
    /// # Errors
    ///
    /// [`Error::Mismatch`] when the target is not the canvas's size turned.
    pub fn present_transformed(
        &self,
        target: &mut Target<'_>,
        damage: &Damage,
        transform: Transform,
    ) -> Result<Damage, Error> {
        let (width, height) = (self.width(), self.height());
        let turned = transform.size((width, height));
        if (target.width(), target.height()) != turned {
            return Err(Error::Mismatch {
                canvas: turned,
                target: (target.width(), target.height()),
            });
        }
        let size = (i64::from(width), i64::from(height));
        let clipped = damage.clipped(self.bounds());
        let stride = usize::try_from(width).unwrap_or(0).saturating_mul(4);
        for &clip in clipped.rects() {
            copy(
                transform,
                size,
                self.data(),
                stride,
                (0, 0),
                clip,
                target,
                (0, 0),
            );
        }
        Ok(self::damage(transform, size, &clipped))
    }
}
