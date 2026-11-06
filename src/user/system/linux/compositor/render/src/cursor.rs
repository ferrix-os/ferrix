//! The pointer, drawn.
//!
//! A compositor with a mouse and no arrow on the screen is one a person
//! cannot use, and the arrow has to come from somewhere. Hyprland loads an
//! `XCursor` or a `hyprcursor` theme; Ferrix has no theme files and no
//! library to read them with, so the one this draws when no client has said
//! otherwise is here, in code, as a shape rather than as a file.
//!
//! It is the shape every desktop's default cursor is: a left-pointing arrow
//! with a black outline and a white fill, its tip at the pointer. A client
//! that calls `wl_pointer.set_cursor` replaces it with its own surface,
//! which is how a text field shows an I-beam and a link shows a hand.

use crate::{Format, Surface};

/// The arrow's size in pixels, each way.
pub const SIDE: u32 = 24;

/// Where the pointer *is* inside the arrow: its tip, at the top-left.
pub const HOTSPOT: (i32, i32) = (0, 0);

/// The arrow, a row of characters each: `#` is the outline, `.` the fill,
/// and a space is not drawn.
///
/// Written out rather than computed because that is what the shape is --
/// every pixel of a 24×24 cursor was chosen by somebody, and an arrow
/// generated from two lines and a fill rule looks like an arrow generated
/// from two lines and a fill rule.
const SHAPE: [&str; SIDE as usize] = [
    "#                       ",
    "##                      ",
    "#.#                     ",
    "#..#                    ",
    "#...#                   ",
    "#....#                  ",
    "#.....#                 ",
    "#......#                ",
    "#.......#               ",
    "#........#              ",
    "#.........#             ",
    "#..........#            ",
    "#...........#           ",
    "#............#          ",
    "#.............#         ",
    "#......#######          ",
    "#...#..#                ",
    "#..# #..#               ",
    "#.#  #..#               ",
    "##    #..#              ",
    "#     #..#              ",
    "       #..#             ",
    "       #..#             ",
    "        ##              ",
];

/// The arrow's pixels, premultiplied `ARGB8888` in the canvas's byte order.
///
/// Every pixel is either opaque or fully transparent -- an arrow with soft
/// edges would need a theme, and a compositor that has none should draw a
/// hard one rather than a blurry guess.
#[must_use]
pub fn arrow() -> Vec<u8> {
    let mut pixels = vec![0u8; (SIDE * SIDE * 4) as usize];
    for (y, row) in SHAPE.iter().enumerate() {
        for (x, glyph) in row.chars().enumerate() {
            let shade = match glyph {
                '#' => 0x00,
                '.' => 0xFF,
                _ => continue,
            };
            let at = (y * SIDE as usize + x) * 4;
            // `ARGB8888` is little-endian, so the bytes are blue, green,
            // red and alpha -- and premultiplied, which for an opaque
            // pixel is the colour itself.
            if let Some(pixel) = pixels.get_mut(at..at + 4) {
                pixel.copy_from_slice(&[shade, shade, shade, 0xFF]);
            }
        }
    }
    pixels
}

/// The arrow as a [`Surface`], ready to composite.
///
/// # Errors
///
/// Only a size the renderer refuses, which [`SIDE`] is not.
pub fn surface(pixels: &[u8]) -> Result<Surface<'_>, crate::Error> {
    Surface::new(pixels, SIDE, SIDE, SIDE * 4, Format::Argb8888)
}

#[cfg(test)]
mod tests {
    use super::{HOTSPOT, SHAPE, SIDE, arrow, surface};

    /// The shape is square and every character in it is one of the three.
    #[test]
    fn the_arrow_is_a_square_of_three_characters() {
        assert_eq!(SHAPE.len(), SIDE as usize);
        for row in SHAPE {
            assert_eq!(row.chars().count(), SIDE as usize, "{row:?}");
            assert!(row.chars().all(|glyph| matches!(glyph, '#' | '.' | ' ')));
        }
    }

    /// The tip is where the pointer is, and it is drawn: a cursor whose
    /// hotspot is on a transparent pixel is one that points at nothing.
    #[test]
    fn the_tip_is_drawn_and_is_the_hotspot() {
        let pixels = arrow();
        let at = ((HOTSPOT.1 as usize) * SIDE as usize + HOTSPOT.0 as usize) * 4;
        assert_eq!(pixels[at + 3], 0xFF, "the hotspot is transparent");
        assert_eq!(&pixels[at..at + 3], &[0, 0, 0], "the tip is the outline");
    }

    /// Every pixel is opaque or clear, and the fill is white.
    #[test]
    fn every_pixel_is_opaque_or_clear() {
        let pixels = arrow();
        assert_eq!(pixels.len(), (SIDE * SIDE * 4) as usize);
        let mut white = 0;
        for pixel in pixels.as_chunks::<4>().0 {
            assert!(pixel[3] == 0 || pixel[3] == 0xFF, "{pixel:?}");
            if *pixel == [0xFF, 0xFF, 0xFF, 0xFF] {
                white += 1;
            }
        }
        assert!(white > 100, "the arrow has {white} filled pixels");
        assert!(surface(&pixels).is_ok());
    }
}
