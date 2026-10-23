//! `wallpaper`: what is behind everything on every screen
//! (`docs/brand/BRAND.md`), what `swaybg` and hyprpaper do: a picture, a
//! colour, a slideshow of a directory's pictures, or a video handed to
//! `pattern --video`.
//!
//! The drawing is here, where a host test reaches it: a picture fitted to a
//! screen as the `wallpaper` line's `fit` says -- covering it (`fill`),
//! inside it (`fit`), pulled to its shape (`stretch`), repeated (`tile`) or
//! at its own size in the middle (`centre`) -- over the line's colour, and,
//! for `span`, the part of one picture across every screen that falls on
//! this one. `main.rs` keeps one background layer surface per screen.

use compositor_config::WallpaperFit;
use compositor_image::tiny_skia::{
    Color, FilterQuality, Pixmap, PixmapMut, PixmapPaint, PixmapRef, Transform,
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

/// The same for `fit`: the smaller ratio, so all of it shows.
#[must_use]
pub fn contain(picture: (u32, u32), target: (u32, u32)) -> (f32, f32, f32) {
    let (pw, ph) = (picture.0.max(1) as f32, picture.1.max(1) as f32);
    let (tw, th) = (target.0 as f32, target.1 as f32);
    let scale = (tw / pw).min(th / ph);
    ((tw - pw * scale) / 2.0, (th - ph * scale) / 2.0, scale)
}

/// Where a screen is in a picture spread across every screen: the box all
/// the screens make together, and this screen's place in it, both in the
/// compositor's own units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    /// The box every screen makes together: left, top, width, height.
    pub whole: (i32, i32, u32, u32),
    /// This screen: left, top, width, height.
    pub screen: (i32, i32, u32, u32),
}

/// `colour`, `0xAARRGGBB`, as a colour to fill with.
#[must_use]
pub fn colour(argb: u32) -> Color {
    let [alpha, red, green, blue] = argb.to_be_bytes();
    Color::from_rgba8(red, green, blue, alpha)
}

/// Draw the wallpaper on `target`: `background` everywhere, and `picture`,
/// when there is one, fitted over it as `fit` says -- or, with `span`, the
/// part of it that falls on this screen when it covers them all.
pub fn draw_fitted(
    picture: Option<PixmapRef<'_>>,
    target: &mut PixmapMut<'_>,
    fit: WallpaperFit,
    background: u32,
    span: Option<Span>,
) {
    target.fill(colour(background));
    let Some(picture) = picture else {
        return;
    };
    let size = (picture.width(), picture.height());
    let (tw, th) = (target.width(), target.height());
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..PixmapPaint::default()
    };
    if let Some(span) = span {
        // The picture covers the whole box; this screen shows its part,
        // its own units turned into the target's pixels.
        let (_, _, whole_w, whole_h) = span.whole;
        let (x, y, scale) = cover(size, (whole_w, whole_h));
        let pixels = tw as f32 / span.screen.2.max(1) as f32;
        let left = (span.screen.0 - span.whole.0) as f32;
        let top = (span.screen.1 - span.whole.1) as f32;
        let transform = Transform::from_row(
            scale * pixels,
            0.0,
            0.0,
            scale * pixels,
            (x - left) * pixels,
            (y - top) * pixels,
        );
        target.draw_pixmap(0, 0, picture, &paint, transform, None);
        return;
    }
    match fit {
        WallpaperFit::Fill | WallpaperFit::Fit => {
            let (x, y, scale) = if fit == WallpaperFit::Fill {
                cover(size, (tw, th))
            } else {
                contain(size, (tw, th))
            };
            let transform = Transform::from_row(scale, 0.0, 0.0, scale, x, y);
            target.draw_pixmap(0, 0, picture, &paint, transform, None);
        }
        WallpaperFit::Stretch => {
            let sx = tw as f32 / size.0.max(1) as f32;
            let sy = th as f32 / size.1.max(1) as f32;
            let transform = Transform::from_row(sx, 0.0, 0.0, sy, 0.0, 0.0);
            target.draw_pixmap(0, 0, picture, &paint, transform, None);
        }
        WallpaperFit::Centre => {
            let x = (i64::from(tw) - i64::from(size.0)) / 2;
            let y = (i64::from(th) - i64::from(size.1)) / 2;
            target.draw_pixmap(
                i32::try_from(x).unwrap_or(0),
                i32::try_from(y).unwrap_or(0),
                picture,
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }
        WallpaperFit::Tile => {
            let (w, h) = (size.0.max(1), size.1.max(1));
            let mut y = 0;
            while y < th {
                let mut x = 0;
                while x < tw {
                    target.draw_pixmap(
                        i32::try_from(x).unwrap_or(i32::MAX),
                        i32::try_from(y).unwrap_or(i32::MAX),
                        picture,
                        &PixmapPaint::default(),
                        Transform::identity(),
                        None,
                    );
                    x += w;
                }
                y += h;
            }
        }
    }
}

/// Draw `picture` over the whole of `target`, covering it.
pub fn draw(picture: PixmapRef<'_>, target: &mut PixmapMut<'_>) {
    draw_fitted(Some(picture), target, WallpaperFit::Fill, 0xff00_0000, None);
}

/// `picture` fitted to a fresh pixmap of `size`, for a test or a
/// screenshot.
#[must_use]
pub fn render_fitted(
    picture: Option<PixmapRef<'_>>,
    size: (u32, u32),
    fit: WallpaperFit,
    background: u32,
    span: Option<Span>,
) -> Option<Pixmap> {
    let mut out = Pixmap::new(size.0, size.1)?;
    draw_fitted(picture, &mut out.as_mut(), fit, background, span);
    Some(out)
}

/// `picture` covering a fresh pixmap of `size`.
#[must_use]
pub fn render(picture: PixmapRef<'_>, size: (u32, u32)) -> Option<Pixmap> {
    render_fitted(Some(picture), size, WallpaperFit::Fill, 0xff00_0000, None)
}

/// The pictures a slideshow shows from the files in a directory: those
/// whose names end as a PNG, JPEG or SVG's do, sorted by name.
#[must_use]
pub fn pictures_in(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut kept: Vec<String> = names
        .into_iter()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            [".png", ".jpg", ".jpeg", ".svg"]
                .iter()
                .any(|ending| lower.ends_with(ending))
        })
        .collect();
    kept.sort();
    kept
}

/// Whether `output` shows a fullscreen window, from `hyprctl monitors` and
/// `hyprctl workspaces` as they print: the workspace the monitor shows,
/// and whether that one has a fullscreen window. What a slideshow's
/// `pause_covered` asks.
#[must_use]
pub fn covered(monitors: &str, workspaces: &str, output: &str) -> bool {
    let header = format!("Monitor {output} (");
    let Some(block) = monitors
        .split("\nMonitor ")
        .map(|block| {
            if block.starts_with("Monitor ") {
                block.to_owned()
            } else {
                format!("Monitor {block}")
            }
        })
        .find(|block| block.starts_with(&header))
    else {
        return false;
    };
    let Some(active) = block
        .lines()
        .find_map(|line| line.trim().strip_prefix("active workspace: "))
        .and_then(|rest| rest.split_whitespace().next())
    else {
        return false;
    };
    let wanted = format!("workspace ID {active} (");
    workspaces
        .split("workspace ID ")
        .map(|block| format!("workspace ID {block}"))
        .find(|block| block.starts_with(&wanted))
        .is_some_and(|block| block.lines().any(|line| line.trim() == "hasfullscreen: 1"))
}

/// `order` put in another order by `seed`, the same seed the same order:
/// a slideshow's shuffle, with no random-number crate for it.
pub fn shuffle<T>(order: &mut [T], mut seed: u64) {
    for at in (1..order.len()).rev() {
        // xorshift64*
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        let value = seed.wrapping_mul(0x2545_f491_4f6c_dd1d);
        let pick = usize::try_from(value % (at as u64 + 1)).unwrap_or(0);
        order.swap(at, pick);
    }
}

#[cfg(test)]
mod tests;
