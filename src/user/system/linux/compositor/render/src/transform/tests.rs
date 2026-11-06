//! A turned monitor's pixels, each one where it belongs.

use super::{Transform, copy, damage, point, rect};
use crate::{Canvas, Color, Damage, Error, Rect, Target};

/// A 3x2 frame whose six pixels are told apart by value:
///
/// ```text
/// a b c
/// d e f
/// ```
const A: u32 = 0xFF00_000A;
const B: u32 = 0xFF00_000B;
const C: u32 = 0xFF00_000C;
const D: u32 = 0xFF00_000D;
const E: u32 = 0xFF00_000E;
const F: u32 = 0xFF00_000F;

/// The frame above as bytes.
fn frame() -> Vec<u8> {
    [A, B, C, D, E, F]
        .iter()
        .flat_map(|pixel| pixel.to_le_bytes())
        .collect()
}

/// `bytes` as the pixel values they hold.
fn values(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| u32::from_le_bytes(*pixel))
        .collect()
}

/// The frame turned into a buffer of the size `transform` makes it, whole.
fn turned(transform: Transform) -> (u32, u32, Vec<u32>) {
    let (width, height) = transform.size((3u32, 2u32));
    let mut bytes = vec![0; width as usize * height as usize * 4];
    let mut target = Target::new(&mut bytes, width, height, width * 4).unwrap();
    copy(
        transform,
        (3, 2),
        &frame(),
        12,
        (0, 0),
        Rect::new(0, 0, 3, 2),
        &mut target,
        (0, 0),
    );
    (width, height, values(&bytes))
}

/// Every transform's buffer for the frame, pixel for pixel, written out by
/// hand rather than worked out by the code under test.
///
/// Hyprland's matrix for transform 1 sends the picture's `(x, y)` to the
/// buffer's `(y, W - 1 - x)` (the module's own documentation derives it):
/// the picture turned 90 degrees counter-clockwise, which is the
/// protocol's word for 1, with `a`, the picture's top left, at the buffer's
/// bottom left. The flipped four are mirrored left to right first.
#[test]
fn every_transform_puts_every_pixel_where_hyprland_does() {
    let cases: [(Transform, (u32, u32), [u32; 6]); 8] = [
        (Transform::Normal, (3, 2), [A, B, C, D, E, F]),
        // Counter-clockwise: the right column is the top row.
        (Transform::Rotated90, (2, 3), [C, F, B, E, A, D]),
        (Transform::Rotated180, (3, 2), [F, E, D, C, B, A]),
        // Clockwise: the left column, read from the bottom, is the top row.
        (Transform::Rotated270, (2, 3), [D, A, E, B, F, C]),
        (Transform::Flipped, (3, 2), [C, B, A, F, E, D]),
        // Mirrored and turned: the picture's columns are the buffer's rows.
        (Transform::Flipped90, (2, 3), [A, D, B, E, C, F]),
        (Transform::Flipped180, (3, 2), [D, E, F, A, B, C]),
        (Transform::Flipped270, (2, 3), [F, C, E, B, D, A]),
    ];
    for (transform, size, want) in cases {
        let (width, height, got) = turned(transform);
        assert_eq!((width, height), size, "{transform:?}'s buffer size");
        assert_eq!(got, want, "{transform:?}");
    }
}

/// The same eight, checked another way: the picture, put through what the
/// protocol's words for each transform say -- mirrored left to right for
/// the flipped four, then turned counter-clockwise a quarter at a time, as
/// many quarters as the transform's number says -- is the buffer. An oracle
/// that shares no code with [`point`].
#[test]
fn each_transform_is_what_the_protocol_words_it_as() {
    /// A `width`-wide image mirrored left to right.
    fn mirrored(width: usize, pixels: &[u32]) -> Vec<u32> {
        pixels
            .chunks_exact(width)
            .flat_map(|row| row.iter().rev().copied())
            .collect()
    }
    /// A `width` by `height` image turned 90 degrees counter-clockwise,
    /// which makes it `height` wide.
    fn counter_clockwise(width: usize, height: usize, pixels: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(pixels.len());
        for row in 0..width {
            for column in 0..height {
                out.push(pixels[column * width + (width - 1 - row)]);
            }
        }
        out
    }
    let picture = [A, B, C, D, E, F];
    for (value, transform) in Transform::ALL.iter().enumerate() {
        let (mut width, mut height) = (3usize, 2usize);
        let mut worded = if value >= 4 {
            mirrored(width, &picture)
        } else {
            picture.to_vec()
        };
        for _ in 0..value % 4 {
            worded = counter_clockwise(width, height, &worded);
            (width, height) = (height, width);
        }
        let (across, down, buffer) = turned(*transform);
        assert_eq!(
            (across as usize, down as usize),
            (width, height),
            "{transform:?}"
        );
        assert_eq!(buffer, worded, "{transform:?}");
    }
}

/// The numbers are `wl_output.transform`'s, and only 0 to 7 are one.
#[test]
fn a_transform_is_its_protocol_number() {
    for value in 0..8 {
        assert_eq!(Transform::from_value(value).unwrap().value(), value);
    }
    assert_eq!(Transform::from_value(8), None);
    assert_eq!(Transform::from_value(u32::MAX), None);
    assert_eq!(Transform::default(), Transform::Normal);
    // A quarter turn exchanges a monitor's width and height, and nothing
    // else does.
    for transform in Transform::ALL {
        let swapped = matches!(transform.value(), 1 | 3 | 5 | 7);
        assert_eq!(transform.swaps(), swapped, "{transform:?}");
        assert_eq!(
            transform.size((1920, 1080)),
            if swapped { (1080, 1920) } else { (1920, 1080) }
        );
    }
}

/// A rectangle of damage is the rectangle its pixels land in, for every
/// transform: each of its pixels, put through [`point`], is inside it, and
/// it is no larger than they are.
#[test]
fn damage_lands_where_its_pixels_do() {
    let size = (7, 5);
    let damaged = Rect::new(1, 2, 4, 2);
    for transform in Transform::ALL {
        let moved = rect(transform, size, damaged);
        assert_eq!(
            moved.width * moved.height,
            damaged.width * damaged.height,
            "{transform:?} changed the damage's size"
        );
        for y in damaged.y..damaged.bottom() {
            for x in damaged.x..damaged.right() {
                let (bx, by) = point(transform, size, (x, y));
                assert!(
                    (moved.x..moved.right()).contains(&bx)
                        && (moved.y..moved.bottom()).contains(&by),
                    "{transform:?}: ({x}, {y}) went to ({bx}, {by}), outside {moved:?}"
                );
            }
        }
    }

    // Spelled out for the two a monitor on its edge uses, on a 768x1024
    // monitor whose buffer is 1024x768: a strip along the top of the
    // picture is a strip down one side of the buffer -- the left for 1,
    // the right for 3.
    let top = Rect::new(0, 0, 768, 30);
    assert_eq!(
        rect(Transform::Rotated90, (768, 1024), top),
        Rect::new(0, 0, 30, 768)
    );
    assert_eq!(
        rect(Transform::Rotated270, (768, 1024), top),
        Rect::new(994, 0, 30, 768)
    );

    // A region keeps its rectangles apart and its area.
    let region: Damage = [Rect::new(0, 0, 3, 1), Rect::new(0, 1, 1, 1)]
        .into_iter()
        .collect();
    for transform in Transform::ALL {
        let moved = damage(transform, (3, 2), &region);
        assert_eq!(moved.area(), region.area(), "{transform:?}");
        assert_eq!(moved.rects().len(), region.rects().len(), "{transform:?}");
    }
}

/// Presenting a turned frame writes only what was damaged, where the
/// transform sends it, into a buffer with a stride wider than its pixels,
/// and says what it wrote in the buffer's own pixels.
#[test]
fn a_turned_frame_is_presented_through_its_damage_only() {
    // A 3x2 canvas, `a` to `f` as above, drawn one pixel at a time.
    let mut canvas = Canvas::new(3, 2).unwrap();
    for (at, colour) in [A, B, C, D, E, F].into_iter().enumerate() {
        let (x, y) = (at as i64 % 3, at as i64 / 3);
        canvas.clear(Color(colour), &Damage::from(Rect::new(x, y, 1, 1)));
    }
    // Two pixels of padding a row, which a card's pitch often has.
    let stride = 4 * 4;
    let mut bytes = vec![0; stride * 3];
    let mut target = Target::new(&mut bytes, 2, 3, stride as u32).unwrap();
    // Only the top row of the picture changed.
    let written = canvas
        .present_transformed(
            &mut target,
            &Damage::from(Rect::new(0, 0, 3, 1)),
            Transform::Rotated90,
        )
        .unwrap();
    // Which is the buffer's left column, read from the bottom.
    assert_eq!(written, Damage::from(Rect::new(0, 0, 1, 3)));
    let rows: Vec<Vec<u32>> = bytes.chunks_exact(stride).map(values).collect();
    assert_eq!(
        rows,
        [vec![C, 0, 0, 0], vec![B, 0, 0, 0], vec![A, 0, 0, 0]],
        "only the damaged pixels, in the left column, and no padding"
    );

    // A buffer the canvas's size, rather than its size turned, is not one.
    let mut flat = vec![0; 3 * 2 * 4];
    let mut wrong = Target::new(&mut flat, 3, 2, 12).unwrap();
    assert_eq!(
        canvas.present_transformed(&mut wrong, &Damage::full(3, 2), Transform::Rotated270),
        Err(Error::Mismatch {
            canvas: (2, 3),
            target: (3, 2)
        })
    );
    // An upright transform presents what `present` does, byte for byte.
    let mut upright = vec![0; 3 * 2 * 4];
    let mut target = Target::new(&mut upright, 3, 2, 12).unwrap();
    let written = canvas
        .present_transformed(&mut target, &Damage::full(3, 2), Transform::Normal)
        .unwrap();
    assert_eq!(written, Damage::full(3, 2));
    assert_eq!(upright, canvas.data());
}

/// What a GPU gives back is a rectangle's rows and nothing else; turned
/// from there, each pixel lands where the whole canvas's would have.
#[test]
fn a_rectangle_read_back_is_turned_as_the_whole_frame_is() {
    let whole = frame();
    for transform in Transform::ALL {
        let (width, height) = transform.size((3u32, 2u32));
        let mut from_whole = vec![0; width as usize * height as usize * 4];
        let mut from_part = from_whole.clone();
        // The frame's right two columns, as a GPU would read them back:
        // packed, two pixels a row.
        let part = Rect::new(1, 0, 2, 2);
        let packed: Vec<u8> = [B, C, E, F]
            .iter()
            .flat_map(|pixel| pixel.to_le_bytes())
            .collect();
        {
            let mut target = Target::new(&mut from_whole, width, height, width * 4).unwrap();
            copy(
                transform,
                (3, 2),
                &whole,
                12,
                (0, 0),
                part,
                &mut target,
                (0, 0),
            );
        }
        {
            let mut target = Target::new(&mut from_part, width, height, width * 4).unwrap();
            copy(
                transform,
                (3, 2),
                &packed,
                8,
                (1, 0),
                part,
                &mut target,
                (0, 0),
            );
        }
        assert_eq!(from_part, from_whole, "{transform:?}");
        assert_eq!(
            values(&from_part)
                .iter()
                .filter(|pixel| **pixel != 0)
                .count(),
            4,
            "{transform:?}"
        );
    }
}

/// A target that is only part of the buffer -- a screenshot of part of a
/// turned monitor -- is given the pixels of that part and no others.
#[test]
fn a_target_that_is_part_of_the_buffer_gets_that_part() {
    for transform in Transform::ALL {
        let (width, _, whole) = turned(transform);
        // The picture's middle column, wherever it lands in the buffer.
        let middle = Rect::new(1, 0, 1, 2);
        let landed = rect(transform, (3, 2), middle);
        let (across, down) = (landed.width as u32, landed.height as u32);
        let mut part = vec![0; across as usize * down as usize * 4];
        let mut target = Target::new(&mut part, across, down, across * 4).unwrap();
        copy(
            transform,
            (3, 2),
            &frame(),
            12,
            (0, 0),
            middle,
            &mut target,
            (landed.x, landed.y),
        );
        let want: Vec<u32> = (landed.y..landed.bottom())
            .flat_map(|y| (landed.x..landed.right()).map(move |x| (x, y)))
            .map(|(x, y)| whole[(y * i64::from(width) + x) as usize])
            .collect();
        assert_eq!(values(&part), want, "{transform:?}");
        assert!(want.contains(&B) && want.contains(&E), "{transform:?}");
    }
}
