//! `wallpaper`: a picture behind everything on every screen
//! (`docs/brand/BRAND.md`), what `swaybg -m fill` and hyprpaper do.
//!
//! The drawing is here, where a host test reaches it: the picture scaled
//! to cover a screen -- as large as it must be for no edge to show, centred,
//! the overhang cut off -- with a smooth filter. `main.rs` keeps one
//! background layer surface per screen.

use compositor_image::tiny_skia::{
    FilterQuality, Pixmap, PixmapMut, PixmapPaint, PixmapRef, Transform,
};

/// The scale and offset that make a `picture`-sized image cover `target`:
/// the larger of the two ratios, centred.
#[must_use]
pub fn cover(picture: (u32, u32), target: (u32, u32)) -> (f32, f32, f32) {
    let (pw, ph) = (picture.0.max(1) as f32, picture.1.max(1) as f32);
    let (tw, th) = (target.0 as f32, target.1 as f32);
    let scale = (tw / pw).max(th / ph);
    ((tw - pw * scale) / 2.0, (th - ph * scale) / 2.0, scale)
}

/// Draw `picture` over the whole of `target`, covering it.
pub fn draw(picture: PixmapRef<'_>, target: &mut PixmapMut<'_>) {
    let (x, y, scale) = cover(
        (picture.width(), picture.height()),
        (target.width(), target.height()),
    );
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..PixmapPaint::default()
    };
    target.draw_pixmap(
        0,
        0,
        picture,
        &paint,
        Transform::from_row(scale, 0.0, 0.0, scale, x, y),
        None,
    );
}

/// `picture` covering a fresh pixmap of `size`, for a test or a screenshot.
#[must_use]
pub fn render(picture: PixmapRef<'_>, size: (u32, u32)) -> Option<Pixmap> {
    let mut out = Pixmap::new(size.0, size.1)?;
    draw(picture, &mut out.as_mut());
    Some(out)
}

#[cfg(test)]
mod tests;
