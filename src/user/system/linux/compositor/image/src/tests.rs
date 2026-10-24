//! Host tests. The SVGs are written here in the shapes the user's waybar
//! icons have -- a stretched cap, a gradient-stroked ring, a stroke whose
//! gradient is in user space -- rather than copied from their files.

use crate::{Fit, Kind, decode, jpeg, png, sniff, svg, svg_size, to_argb8888};

/// A chip's end cap: a triangle in a 12×32 box, stretched by its own
/// `preserveAspectRatio="none"`.
const CAP: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 12 32" width="12" height="32" preserveAspectRatio="none">
  <!-- a comment inside the root, as the user's caps keep theirs -->
  <path d="M12 0 L12 32 L0 32 Z" fill="#38a3ec"/>
</svg>"##;

/// A ring stroked with a gradient in the default (bounding-box) units.
const RING: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="24" height="24">
  <defs>
    <linearGradient id="g" x1="0.15" y1="0" x2="0.75" y2="1">
      <stop offset="0" stop-color="#8ed3f8"/>
      <stop offset="1" stop-color="#1e7fc4"/>
    </linearGradient>
  </defs>
  <circle cx="12" cy="12" r="8.6" fill="none" stroke="url(#g)" stroke-width="2.6"/>
</svg>"##;

/// A vertical stroke whose gradient is in user space: in bounding-box
/// units its box would be zero wide and the stroke would vanish.
const STEM: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="24" height="24">
  <defs>
    <linearGradient id="p" gradientUnits="userSpaceOnUse" x1="4" y1="2.5" x2="18" y2="21">
      <stop offset="0" stop-color="#ffc2d4"/>
      <stop offset="1" stop-color="#e8577f"/>
    </linearGradient>
  </defs>
  <path d="M5.94 8.76 A 7.4 7.4 0 1 0 18.06 8.76" fill="none" stroke="url(#p)" stroke-width="2.6" stroke-linecap="round"/>
  <path d="M12 3.4 L12 11.2" fill="none" stroke="url(#p)" stroke-width="2.6" stroke-linecap="round"/>
</svg>"##;

fn alpha(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> u8 {
    pixmap.pixel(x, y).map_or(0, |pixel| pixel.alpha())
}

#[test]
fn files_are_told_apart_by_their_bytes() {
    assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some(Kind::Png));
    assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Kind::Jpeg));
    assert_eq!(sniff(CAP.as_bytes()), Some(Kind::Svg));
    let leading = format!("<?xml version=\"1.0\"?>\n<!-- a long comment first -->\n{CAP}");
    assert_eq!(sniff(leading.as_bytes()), Some(Kind::Svg));
    assert_eq!(sniff(b"<html></html>"), None);
    assert_eq!(sniff(b"GIF89a"), None);
}

#[test]
fn a_cap_is_stretched_to_the_height_it_is_drawn_at() {
    let cap = svg(CAP.as_bytes(), Fit::Exactly(12, 40)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!((cap.width(), cap.height()), (12, 40));
    // The triangle's right edge runs the whole height; its left corner is
    // at the bottom. Stretched, not letterboxed.
    assert_eq!(alpha(&cap, 11, 10), 255);
    assert_eq!(alpha(&cap, 11, 38), 255);
    assert_eq!(alpha(&cap, 1, 38), 255);
    assert_eq!(alpha(&cap, 1, 1), 0);
    let pixel = cap
        .pixel(11, 20)
        .map(|pixel| (pixel.red(), pixel.green(), pixel.blue()));
    assert_eq!(pixel, Some((0x38, 0xa3, 0xec)));
}

#[test]
fn a_drawing_that_keeps_its_shape_is_centred_in_a_wider_box() {
    let ring = svg(RING.as_bytes(), Fit::Exactly(48, 24)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!((ring.width(), ring.height()), (48, 24));
    // xMidYMid meet: the 24×24 ring sits in the middle 24 columns.
    assert_eq!(alpha(&ring, 4, 12), 0, "nothing in the left margin");
    assert_eq!(alpha(&ring, 12 + 3, 12), 255, "the ring's left side");
    assert_eq!(alpha(&ring, 24, 12), 0, "the ring is hollow");
}

#[test]
fn a_gradient_in_user_space_keeps_a_vertical_stroke() {
    let icon = svg(STEM.as_bytes(), Fit::Within(48, 48)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!((icon.width(), icon.height()), (48, 48));
    // The stem at x 12, y 4..11 in user units: twice that here.
    assert!(alpha(&icon, 24, 14) > 200, "the stem is drawn");
    assert!(alpha(&icon, 9, 26) > 200, "the arc is drawn");
    assert_eq!(alpha(&icon, 24, 30), 0, "and the middle is empty");
    assert_eq!(svg_size(STEM.as_bytes()).ok(), Some((24.0, 24.0)));
}

#[test]
fn a_png_and_a_jpeg_decode_to_their_pixels() {
    let mut original = tiny_skia::Pixmap::new(3, 2).unwrap_or_else(|| panic!("a pixmap"));
    original.fill(tiny_skia::Color::from_rgba8(10, 200, 30, 255));
    let encoded = original
        .encode_png()
        .unwrap_or_else(|error| panic!("{error}"));
    let decoded = png(&encoded).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(decoded.data(), original.data());
    let via = decode(&encoded, Fit::Natural).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!((via.width(), via.height()), (3, 2));

    let red =
        jpeg(include_bytes!("../tests/data/red-4x2.jpg")).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!((red.width(), red.height()), (4, 2));
    let pixel = red
        .pixel(1, 1)
        .map(|pixel| (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()));
    let Some((r, g, b, a)) = pixel else {
        panic!("no pixel");
    };
    assert_eq!(a, 255);
    assert!(
        r.abs_diff(200) < 6 && g.abs_diff(30) < 6 && b.abs_diff(40) < 6,
        "{r} {g} {b}"
    );
}

#[test]
fn a_pixmap_goes_into_wl_shm_order_with_a_stride() {
    let mut pixmap = tiny_skia::Pixmap::new(2, 2).unwrap_or_else(|| panic!("a pixmap"));
    pixmap.fill(tiny_skia::Color::from_rgba8(1, 2, 3, 255));
    let mut out = vec![0u8; 12 * 2];
    to_argb8888(&pixmap.as_ref(), &mut out, 12);
    assert_eq!(out.get(..8), Some(&[3, 2, 1, 255, 3, 2, 1, 255][..]));
    assert_eq!(
        out.get(8..12),
        Some(&[0, 0, 0, 0][..]),
        "the stride's padding is left"
    );
    assert_eq!(out.get(12..16), Some(&[3, 2, 1, 255][..]));
}

#[test]
fn what_is_not_a_picture_is_said_so() {
    assert!(decode(b"not a picture", Fit::Natural).is_err());
    assert!(svg(b"<svg", Fit::Natural).is_err());
}

/// A probe of this machine's own waybar icons, run by hand
/// (`cargo test -p compositor-image -- --ignored`): every one renders, and
/// is not blank.
#[test]
#[ignore = "reads ~/.config/waybar/icons on the machine it runs on"]
fn the_users_icons_render() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home).join(".config/waybar/icons");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut seen = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|kind| kind != "svg") {
            continue;
        }
        let drawn = crate::load(&path, Fit::Exactly(12, 40))
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let covered = drawn
            .pixels()
            .iter()
            .filter(|pixel| pixel.alpha() > 0)
            .count();
        assert!(covered > 0, "{} drew nothing", path.display());
        seen += 1;
    }
    assert!(seen > 0);
}

/// Yoru's wallpaper (`docs/brand/yoru/yoru-nocturne.svg`, docs/DESKTOP.md
/// §6) renders as the design's Nocturne gradient: the ground at the foot
/// on the right, the section glow's blue up right, a whole 1920x1080
/// screen opaque; and quickly enough to start a desktop with.
#[test]
fn the_yoru_wallpaper_is_nocturne() {
    let bytes = include_bytes!("../../../../../../../docs/brand/yoru/yoru-nocturne.svg");
    let started = std::time::Instant::now();
    let drawn = crate::svg(bytes, Fit::Exactly(1920, 1080)).expect("renders");
    let took = started.elapsed();
    assert_eq!((drawn.width(), drawn.height()), (1920, 1080));
    let at = |x: u32, y: u32| {
        let pixel = drawn.pixel(x, y).expect("inside");
        (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
    };
    assert!(
        drawn.pixels().iter().all(|pixel| pixel.alpha() == 255),
        "opaque"
    );
    let (r, g, b, _) = at(1900, 1060);
    assert!(
        r < 0x24 && g < 0x24 && b < 0x34,
        "the ground at the foot: {r} {g} {b}"
    );
    let (r, g, b, _) = at(1440, 216);
    assert!(
        b > 0x5c && b > r + 0x28 && b > g + 0x20,
        "the glow's blue: {r} {g} {b}"
    );
    assert!(took.as_secs() < 5, "{took:?}");
}
