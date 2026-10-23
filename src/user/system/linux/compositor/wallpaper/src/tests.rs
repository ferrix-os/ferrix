//! Covering: a wider screen scales by its width, a taller one by its
//! height, and every pixel is drawn.

use compositor_image::tiny_skia::{Color, Pixmap};

use super::{cover, render};

#[test]
fn a_picture_covers_screens_of_every_shape() {
    // 16:9 on 16:10: scaled by the height, the sides cut.
    let (x, y, scale) = cover((1920, 1080), (1280, 800));
    assert!((scale - 800.0 / 1080.0).abs() < 1e-6);
    assert!(x < 0.0 && y.abs() < 1e-3);
    // 16:9 on a turned screen: scaled by the height, much cut.
    let (x, _, scale) = cover((1920, 1080), (1080, 1920));
    assert!((scale - 1920.0 / 1080.0).abs() < 1e-6);
    assert!(x < -1000.0);
    // The same shape: exactly scaled, nothing cut.
    assert_eq!(cover((1920, 1080), (3840, 2160)), (0.0, 0.0, 2.0));
}

/// Every pixel of a screen of another shape is the picture's, none left
/// transparent at the edges.
#[test]
fn no_edge_is_left_bare() {
    let mut picture = Pixmap::new(192, 108).expect("a picture");
    picture.fill(Color::from_rgba8(0x20, 0x30, 0x50, 0xff));
    let out = render(picture.as_ref(), (100, 160)).expect("drawn");
    assert!(
        out.pixels().iter().all(|pixel| pixel.alpha() == 0xff),
        "an edge left transparent"
    );
}
