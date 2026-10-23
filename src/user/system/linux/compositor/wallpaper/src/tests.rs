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

use compositor_config::WallpaperFit;

use super::{Span, contain, covered, pictures_in, render_fitted, shuffle};

/// A 4x2 picture, left half red and right half blue.
fn halves() -> Pixmap {
    let mut picture = Pixmap::new(4, 2).expect("a picture");
    for (index, pixel) in picture.pixels_mut().iter_mut().enumerate() {
        *pixel = if index % 4 < 2 {
            Color::from_rgba8(0xff, 0, 0, 0xff)
                .premultiply()
                .to_color_u8()
        } else {
            Color::from_rgba8(0, 0, 0xff, 0xff)
                .premultiply()
                .to_color_u8()
        };
    }
    picture
}

/// The `(red, green, blue)` of pixel `(x, y)`.
fn at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8) {
    let pixel = pixmap.pixel(x, y).expect("in the pixmap");
    (pixel.red(), pixel.green(), pixel.blue())
}

/// `fit`, `centre` and `tile` leave the colour where the picture is not,
/// and `stretch` and `fill` leave none.
#[test]
fn each_fit_puts_the_picture_where_it_says() {
    let picture = halves();
    let background = 0xff10_2030;
    let draw =
        |fit| render_fitted(Some(picture.as_ref()), (8, 8), fit, background, None).expect("drawn");
    // Fit: 8 wide, 4 high, centred: the colour above and below.
    let fitted = draw(WallpaperFit::Fit);
    assert_eq!(at(&fitted, 4, 0), (0x10, 0x20, 0x30));
    assert_eq!(at(&fitted, 1, 4), (0xff, 0, 0));
    assert_eq!(at(&fitted, 6, 4), (0, 0, 0xff));
    // Centre: its own size in the middle, 2..6 by 3..5.
    let centred = draw(WallpaperFit::Centre);
    assert_eq!(at(&centred, 0, 0), (0x10, 0x20, 0x30));
    assert_eq!(at(&centred, 2, 3), (0xff, 0, 0));
    assert_eq!(at(&centred, 5, 4), (0, 0, 0xff));
    // Tile: repeated from the top left, so the right half of each copy is
    // blue at x 2, 3, 6, 7.
    let tiled = draw(WallpaperFit::Tile);
    assert_eq!(at(&tiled, 0, 7), (0xff, 0, 0));
    assert_eq!(at(&tiled, 7, 7), (0, 0, 0xff));
    assert_eq!(at(&tiled, 4, 5), (0xff, 0, 0));
    // Stretch and fill: no colour anywhere.
    for fit in [WallpaperFit::Stretch, WallpaperFit::Fill] {
        let drawn = draw(fit);
        assert!(
            (0..8).all(|x| (0..8).all(|y| at(&drawn, x, y) != (0x10, 0x20, 0x30))),
            "{fit:?} left the colour showing"
        );
    }
    // No picture at all: the colour alone.
    let plain = render_fitted(None, (2, 2), WallpaperFit::Fill, background, None).expect("drawn");
    assert_eq!(at(&plain, 1, 1), (0x10, 0x20, 0x30));
}

/// `span`: one picture across two screens side by side, the left screen
/// showing its left half and the right screen its right half.
#[test]
fn a_spanned_picture_is_cut_between_the_screens() {
    let picture = halves();
    let whole = (0, 0, 8, 2);
    let left = Span {
        whole,
        screen: (0, 0, 4, 2),
    };
    let right = Span {
        whole,
        screen: (4, 0, 4, 2),
    };
    let draw = |span| {
        render_fitted(
            Some(picture.as_ref()),
            (4, 2),
            WallpaperFit::Fill,
            0,
            Some(span),
        )
        .expect("drawn")
    };
    assert_eq!(at(&draw(left), 1, 1), (0xff, 0, 0));
    assert_eq!(at(&draw(right), 2, 1), (0, 0, 0xff));
    let (_, _, scale) = contain((4, 2), (8, 8));
    assert!((scale - 2.0).abs() < 1e-6);
}

/// A slideshow takes the pictures in a directory, by name; a shuffle is
/// another order with the same pictures, the same for the same seed.
#[test]
fn a_slideshow_takes_pictures_and_shuffles_them() {
    let names = ["b.JPG", "notes.txt", "a.png", "c.svg", "d.jpeg"]
        .map(str::to_owned)
        .to_vec();
    let kept = pictures_in(names);
    assert_eq!(kept, ["a.png", "b.JPG", "c.svg", "d.jpeg"]);
    let mut once: Vec<u32> = (0..20).collect();
    let mut twice = once.clone();
    shuffle(&mut once, 7);
    shuffle(&mut twice, 7);
    assert_eq!(once, twice);
    assert_ne!(once, (0..20).collect::<Vec<_>>());
    once.sort_unstable();
    assert_eq!(once, (0..20).collect::<Vec<_>>());
}

/// `pause_covered` reads `hyprctl monitors` and `workspaces`.
#[test]
fn a_screen_with_a_fullscreen_window_is_covered() {
    let monitors = "Monitor DP-1 (ID 0):\n\t1920x1080@60.00000 at 0x0\n\tactive workspace: 2 (2)\n\n\
                    Monitor HDMI-A-1 (ID 1):\n\t1920x1080@60.00000 at 1920x0\n\tactive workspace: 3 (3)\n\n";
    let workspaces = "workspace ID 2 (2) on monitor DP-1:\n\twindows: 1\n\thasfullscreen: 1\n\n\
                      workspace ID 3 (3) on monitor HDMI-A-1:\n\twindows: 2\n\thasfullscreen: 0\n\n";
    assert!(covered(monitors, workspaces, "DP-1"));
    assert!(!covered(monitors, workspaces, "HDMI-A-1"));
    assert!(!covered(monitors, workspaces, "eDP-1"));
}
