//! A cursor drawn by a copying card: the arithmetic of CURSOR and MOVE for
//! a driver whose display has no cursor plane it programs (`docs/DISPLAY.md`
//! §8.2) -- Intel's lit plane, copied into.
//!
//! Such a card still says it has a cursor, so that a compositor leaves the
//! pointer out of its frames and can hand a fullscreen client's buffer to
//! the card as it is. The card then puts the pointer into the plane itself,
//! after each copy of a frame: what is under the image's last place is
//! copied again from the buffer shown, and the image is blended over the
//! buffer's pixels at its new place. Only those two small rectangles are
//! read and written, so a pointer costs a few thousand pixels a frame, and
//! nothing at all while it is hidden -- which is what a game shows.
//!
//! The image is premultiplied `ARGB8888`, as a Wayland client's cursor and
//! the compositor's arrow are; the plane is `XRGB8888`. In a scaled mode
//! ([`Scaler`]) the pointer is blended in the mode's pixels and scaled with
//! the picture, as a frame with the pointer drawn into it would be.

use ferrix_displayctl::message::{CURSOR_SIZE, Rect};

use crate::scale::Scaler;

/// `image`, a premultiplied `ARGB8888` pixel, over `under`, an `XRGB8888`
/// one: `image + under * (1 - alpha)` in each channel, rounded, the X byte
/// zero. A channel brighter than its alpha, which a premultiplied pixel
/// never is, is held at full.
#[must_use]
pub const fn over(image: u32, under: u32) -> u32 {
    let alpha = image >> 24;
    let keep = 255 - alpha;
    let mut out = 0;
    let mut shift = 0;
    while shift < 24 {
        let top = (image >> shift) & 0xff;
        let below = (under >> shift) & 0xff;
        // x / 255, rounded, for x up to 255 * 255.
        let scaled = below * keep + 128;
        let scaled = (scaled + (scaled >> 8)) >> 8;
        let sum = top + scaled;
        let channel = if sum > 255 { 255 } else { sum };
        out |= channel << shift;
        shift += 8;
    }
    out
}

/// The part of a [`CURSOR_SIZE`]-square image with its top-left corner at
/// `at` that lies on a mode `mode` big, in the mode's pixels: `None` when
/// none of it does.
#[must_use]
pub fn bounds(at: (i32, i32), mode: (u32, u32)) -> Option<Rect> {
    let span = |start: i32, length: u32| {
        let start = i64::from(start);
        let end = (start + i64::from(CURSOR_SIZE)).min(i64::from(length));
        let start = start.max(0);
        (start < end).then(|| (start as u32, (end - start) as u32))
    };
    let (x, width) = span(at.0, mode.0)?;
    let (y, height) = span(at.1, mode.1)?;
    Some(Rect {
        x,
        y,
        width,
        height,
    })
}

/// The plane's pixels that show any of `rect`, a rectangle of the mode,
/// on a plane `plane` big: `rect` itself, clipped, at the plane's own
/// mode, and what `scaler` maps it to in a scaled one. `None` when none
/// do.
#[must_use]
pub fn on_plane(scaler: Option<&Scaler>, rect: Rect, plane: (u32, u32)) -> Option<Rect> {
    if let Some(scaler) = scaler {
        return scaler.damaged(rect);
    }
    let right = rect.x.saturating_add(rect.width).min(plane.0);
    let bottom = rect.y.saturating_add(rect.height).min(plane.1);
    (rect.x < right && rect.y < bottom).then(|| Rect {
        x: rect.x,
        y: rect.y,
        width: right - rect.x,
        height: bottom - rect.y,
    })
}

/// The mode's pixel the plane's pixel (`x`, `y`) shows -- the plane's
/// pixel inside [`on_plane`]'s rectangle, or inside a scaled mode's
/// picture.
#[must_use]
pub fn shown_at(scaler: Option<&Scaler>, x: u32, y: u32) -> (u32, u32) {
    match scaler {
        Some(scaler) => (
            scaler.source_x(x.saturating_sub(scaler.area.x)),
            scaler.source_y(y.saturating_sub(scaler.area.y)),
        ),
        None => (x, y),
    }
}

/// The image's pixel that falls on the mode's pixel (`x`, `y`), the image
/// `size` big with its top-left corner at `at`: its column and row, or
/// `None` where the image does not reach.
#[must_use]
pub fn image_at(at: (i32, i32), size: (u32, u32), x: u32, y: u32) -> Option<(u32, u32)> {
    let column = i64::from(x) - i64::from(at.0);
    let row = i64::from(y) - i64::from(at.1);
    let inside = |value: i64, length: u32| {
        (0..i64::from(length.min(CURSOR_SIZE)))
            .contains(&value)
            .then_some(value as u32)
    };
    Some((inside(column, size.0)?, inside(row, size.1)?))
}
