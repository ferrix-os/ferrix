//! What a blur must do: spread a colour, keep the total, leave a flat block
//! flat -- and then what the grading on top of it must do to the colours.

use super::{Block, Blur, blur};

/// A block of one colour, as premultiplied `RGBA` bytes.
fn flat(width: usize, height: usize, colour: [u8; 4]) -> Vec<u8> {
    colour
        .into_iter()
        .cycle()
        .take(width * height * 4)
        .collect()
}

/// A block at the canvas's top-left corner, which is where every test but
/// the one about the noise's position puts one.
fn block(width: usize, height: usize) -> Block {
    Block {
        width,
        height,
        origin: (0, 0),
        screen: (width as u32, height as u32),
        doubled: false,
    }
}

/// The two kernels and nothing else, which is what the tests about the
/// shape of a blur are about.
fn shape(size: i64, passes: u32) -> Blur {
    Blur::ungraded(size, passes)
}

fn pixel(pixels: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let at = (y * width + x) * 4;
    [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
}

/// A blur of one colour is that colour. Every tap reads the same value and
/// the weights sum to one, so anything else is an arithmetic mistake.
#[test]
fn a_flat_block_survives_every_pass() {
    for passes in 1..=3 {
        let mut pixels = flat(64, 64, [40, 80, 120, 255]);
        blur(&mut pixels, &block(64, 64), &shape(8, passes));
        for (x, y) in [(0, 0), (32, 32), (63, 63), (1, 62)] {
            assert_eq!(
                pixel(&pixels, 64, x, y),
                [40, 80, 120, 255],
                "{passes} passes changed ({x}, {y})"
            );
        }
    }
}

/// A bright block spreads past its own edge, and its middle stays bright.
/// That is the whole of what a blur is.
///
/// A block rather than a single pixel: one pixel's worth of light spread
/// over a blur's reach is less than a byte, so a test that asked for it
/// would be asking the arithmetic to be wrong.
#[test]
fn a_block_spreads_past_its_own_edge() {
    let mut pixels = flat(64, 64, [0, 0, 0, 255]);
    for y in 24..40 {
        for x in 24..40 {
            let at = (y * 64 + x) * 4;
            pixels[at] = 255;
            pixels[at + 1] = 255;
            pixels[at + 2] = 255;
        }
    }
    blur(&mut pixels, &block(64, 64), &shape(8, 2));

    // The middle is still the brightest thing there is, and light has gone
    // where there was none. How much is the radius's business, and a
    // radius of 8 over two passes reaches most of a 64-pixel square.
    let middle = pixel(&pixels, 64, 32, 32)[0];
    let past = pixel(&pixels, 64, 42, 32)[0];
    let far = pixel(&pixels, 64, 1, 1)[0];
    assert!(
        middle > past,
        "the middle is not the brightest: {middle} vs {past}"
    );
    assert!(past > far, "nothing spread past the edge: {past} vs {far}");
    // And its edge is no longer an edge.
    let inside = pixel(&pixels, 64, 38, 32)[0];
    let outside = pixel(&pixels, 64, 41, 32)[0];
    assert!(inside > outside, "{inside} is not brighter than {outside}");
}

/// An edge between two colours becomes a gradient, and the gradient runs the
/// right way.
#[test]
fn an_edge_becomes_a_gradient() {
    let (width, height) = (64, 16);
    let mut pixels = Vec::with_capacity(width * height * 4);
    for _ in 0..height {
        for x in 0..width {
            let value = if x < width / 2 { 0 } else { 255 };
            pixels.extend_from_slice(&[value, value, value, 255]);
        }
    }
    blur(&mut pixels, &block(width, height), &shape(8, 2));

    let row: Vec<u8> = (0..width).map(|x| pixel(&pixels, width, x, 8)[0]).collect();
    assert!(
        row.windows(2).all(|pair| pair[1] >= pair[0]),
        "the gradient is not monotonic: {row:?}"
    );
    assert!(row[0] < 128 && row[width - 1] > 128, "{row:?}");
    // The middle is somewhere between, which a sharp edge is not.
    let middle = row[width / 2];
    assert!(
        (16..240).contains(&middle),
        "the edge stayed sharp: {middle}"
    );
}

/// Zero passes or zero size is Hyprland's way of turning the blur off, and
/// it must leave the pixels exactly as they were -- the grading included,
/// since Hyprland skips the whole chain rather than running it with no
/// radius.
#[test]
fn nothing_is_blurred_with_no_passes_or_no_size() {
    let original = flat(8, 8, [1, 2, 3, 255]);
    for (size, passes) in [(0, 1), (8, 0), (0, 0)] {
        let mut pixels = original.clone();
        blur(&mut pixels, &block(8, 8), &Blur::new(size, passes));
        assert_eq!(pixels, original, "size {size}, {passes} passes");
    }
}

/// A block smaller than the pass chain still comes back the size it went in,
/// rather than the empty vector a division that reached zero would give.
#[test]
fn a_tiny_block_survives_more_passes_than_it_has_pixels() {
    let mut pixels = flat(3, 2, [9, 9, 9, 255]);
    blur(&mut pixels, &block(3, 2), &shape(8, 5));
    assert_eq!(pixels.len(), 3 * 2 * 4);
    assert_eq!(pixel(&pixels, 3, 1, 1), [9, 9, 9, 255]);
}

// -- The grading -------------------------------------------------------------

/// One flat block blurred with `settings`, as its one colour.
fn graded_flat(colour: [u8; 4], settings: &Blur) -> [u8; 4] {
    let mut pixels = flat(32, 32, colour);
    blur(&mut pixels, &block(32, 32), settings);
    pixel(&pixels, 32, 16, 16)
}

/// `blur:contrast` is `gain.glsl`'s curve about the middle grey: above one
/// it pushes a dark colour darker and a light one lighter, below one it
/// pulls both towards the middle. The alpha is not touched.
#[test]
fn contrast_moves_a_colour_away_from_the_middle() {
    let dark = [64, 64, 64, 255];
    let light = [192, 192, 192, 255];
    let with = |contrast: f32, colour: [u8; 4]| {
        graded_flat(
            colour,
            &Blur {
                contrast,
                ..Blur::ungraded(8, 1)
            },
        )
    };
    assert_eq!(
        with(1.0, dark),
        dark,
        "a contrast of one changed the colour"
    );
    assert!(
        with(2.0, dark)[0] < dark[0],
        "contrast above one did not darken the dark colour: {:?}",
        with(2.0, dark)
    );
    assert!(
        with(2.0, light)[0] > light[0],
        "contrast above one did not lighten the light colour: {:?}",
        with(2.0, light)
    );
    assert!(
        with(0.5, dark)[0] > dark[0] && with(0.5, light)[0] < light[0],
        "contrast below one did not pull towards the middle"
    );
    assert_eq!(with(2.0, dark)[3], 255, "the contrast changed the alpha");
}

/// `blur:brightness` is applied twice, `max(1, b)` before the passes and
/// `min(1, b)` after them, so one of the two is always the identity: below
/// one it darkens, above one it brightens, and one leaves the block alone.
#[test]
fn brightness_darkens_below_one_and_brightens_above_it() {
    let colour = [100, 100, 100, 255];
    let with = |brightness: f32| {
        graded_flat(
            colour,
            &Blur {
                brightness,
                ..Blur::ungraded(8, 1)
            },
        )
    };
    assert_eq!(with(1.0), colour);
    assert_eq!(with(0.5)[0], 50, "half brightness is not half the colour");
    assert_eq!(with(1.5)[0], 150, "half again is not half again");
}

/// `blur:noise` dithers, and the dither is a hash: the same frame twice is
/// the same bytes, which is what lets an expected image hold a blur at all.
#[test]
fn the_noise_dithers_the_same_way_every_time() {
    let colour = [128, 128, 128, 255];
    let settings = Blur {
        noise: 0.2,
        ..Blur::ungraded(8, 1)
    };
    let mut once = flat(32, 32, colour);
    let mut again = flat(32, 32, colour);
    blur(&mut once, &block(32, 32), &settings);
    blur(&mut again, &block(32, 32), &settings);
    assert_eq!(once, again, "the noise is not a function of the position");

    // And it is noise: the flat block is no longer flat.
    let values: Vec<u8> = (0..32).map(|x| pixel(&once, 32, x, 16)[0]).collect();
    assert!(
        values.iter().any(|&value| value != colour[0]),
        "the noise changed nothing: {values:?}"
    );
    // Around the colour rather than away from it: the dither is the hash
    // less a half, so it is as often below as above.
    assert!(
        values.iter().any(|&value| value < colour[0])
            && values.iter().any(|&value| value > colour[0]),
        "the noise only went one way: {values:?}"
    );
}

/// The noise is fixed to the screen, not to the block: `blurFinish.glsl`
/// hashes the coordinate of a quad that covers the monitor, so a window
/// that moves moves through the pattern rather than carrying it along.
#[test]
fn the_noise_is_fixed_to_the_screen() {
    let colour = [128, 128, 128, 255];
    let settings = Blur {
        noise: 0.2,
        ..Blur::ungraded(8, 1)
    };
    let at = |origin: (i64, i64)| {
        let mut pixels = flat(32, 32, colour);
        blur(
            &mut pixels,
            &Block {
                width: 32,
                height: 32,
                origin,
                screen: (256, 256),
                doubled: false,
            },
            &settings,
        );
        pixels
    };
    assert_eq!(at((0, 0)), at((0, 0)), "the same place, a different dither");
    assert_ne!(
        at((0, 0)),
        at((64, 32)),
        "the dither followed the block instead of staying on the screen"
    );
}

/// `blur:vibrancy` raises the saturation of what it blurs, and
/// `vibrancy_darkness` says how much of that a dark colour gets. A grey has
/// no saturation to raise, so it is left alone whatever the vibrancy.
#[test]
fn vibrancy_saturates_a_colour_and_leaves_a_grey_alone() {
    let spread = |colour: [u8; 4]| -> i32 {
        let channels = [
            i32::from(colour[0]),
            i32::from(colour[1]),
            i32::from(colour[2]),
        ];
        let (smallest, largest) = (
            channels.iter().min().copied().unwrap_or(0),
            channels.iter().max().copied().unwrap_or(0),
        );
        largest - smallest
    };
    let with = |vibrancy: f32, colour: [u8; 4]| {
        graded_flat(
            colour,
            &Blur {
                vibrancy,
                ..Blur::ungraded(8, 1)
            },
        )
    };
    // A colour with a hue: the blue of a window's own gradient pattern.
    let blue = [200, 120, 60, 255];
    assert!(
        spread(with(1.0, blue)) > spread(with(0.0, blue)),
        "vibrancy did not saturate {blue:?}: {:?} against {:?}",
        with(1.0, blue),
        with(0.0, blue)
    );
    let grey = [128, 128, 128, 255];
    assert_eq!(
        with(1.0, grey),
        grey,
        "vibrancy saturated a colour with no saturation"
    );
}

/// Hyprland's own defaults grade: a blur drawn with them is not the blur
/// drawn without them, which is why the defaults here are Hyprland's rather
/// than the values that do nothing.
#[test]
fn hyprlands_defaults_are_not_the_ungraded_ones() {
    let colour = [90, 140, 200, 255];
    let default = graded_flat(colour, &Blur::new(8, 2));
    let ungraded = graded_flat(colour, &Blur::ungraded(8, 2));
    assert_eq!(ungraded, colour, "the ungraded blur changed a flat block");
    assert_ne!(
        default, ungraded,
        "Hyprland's default grading changed nothing"
    );
}
