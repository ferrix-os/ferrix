//! Tests for drawing, against a framebuffer that is a `Vec`.
//!
//! Every buffer starts full of a sentinel byte that no colour used here
//! produces, so "this byte was not written" is checkable everywhere — padding
//! past the stride, pixels outside a clip, clear pixels of a transparent glyph.
//! Clipping is checked against a per-pixel model rather than hand-picked
//! expectations, over edge values up to `usize::MAX`.

extern crate std;

use core::fmt::Write;
use std::format;
use std::vec;
use std::vec::Vec;

use super::*;

const SENTINEL: u8 = 0xEE;
/// Distinct channel values, so a swapped order is visible.
const ORANGE: Rgb = Rgb::new(0x11, 0x22, 0x33);
const BLUE: Rgb = Rgb::new(0x44, 0x55, 0x66);
const UNTOUCHED: [u8; 4] = [SENTINEL; 4];
/// `BLUE` as a `Bgrx` pixel.
const BG: [u8; 4] = [0x66, 0x55, 0x44, 0];
/// `ORANGE` as a `Bgrx` pixel.
const FG: [u8; 4] = [0x33, 0x22, 0x11, 0];

struct Fb {
    bytes: Vec<u8>,
    width: usize,
    height: usize,
    stride: usize,
}

impl Fb {
    fn new(width: usize, height: usize, stride: usize) -> Self {
        Fb {
            bytes: vec![SENTINEL; height * stride * 4],
            width,
            height,
            stride,
        }
    }

    fn surface(&mut self, order: PixelOrder) -> Surface<'_> {
        Surface::new(&mut self.bytes, self.width, self.height, self.stride, order)
            .expect("the fixture is sized for its own dimensions")
    }

    fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        let at = (y * self.stride + x) * 4;
        self.bytes[at..at + 4].try_into().unwrap()
    }

    /// Assert every pixel of every scanline, padding included.
    fn assert_pixels(&self, label: &str, expected: impl Fn(usize, usize) -> [u8; 4]) {
        for y in 0..self.height {
            for x in 0..self.stride {
                assert_eq!(
                    self.pixel(x, y),
                    expected(x, y),
                    "{label}: pixel ({x}, {y})"
                );
            }
        }
    }
}

/// Whether font pixel (`dx`, `dy`) of `ch` is set, at scale 1.
fn font_bit(ch: char, dx: usize, dy: usize) -> bool {
    glyph(ch)[dy] & (0x80 >> dx) != 0
}

/// Assert that every pixel of the cell at (`left`, `top`) is `ch` drawn at
/// `scale` in `fg`, with clear pixels `bg`.
fn assert_cell(fb: &Fb, left: usize, top: usize, ch: char, scale: usize, fg: [u8; 4], bg: [u8; 4]) {
    for dy in 0..GLYPH_HEIGHT * scale {
        for dx in 0..GLYPH_WIDTH * scale {
            let expected = if font_bit(ch, dx / scale, dy / scale) {
                fg
            } else {
                bg
            };
            assert_eq!(
                fb.pixel(left + dx, top + dy),
                expected,
                "{ch:?} at scale {scale}, cell pixel ({dx}, {dy})"
            );
        }
    }
}

/// Every pair from `a` and `b`.
fn pairs(a: &[usize], b: &[usize]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for &first in a {
        for &second in b {
            out.push((first, second));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Surface::new
// ---------------------------------------------------------------------------

#[test]
fn new_accepts_an_exact_or_longer_slice() {
    let mut exact = vec![0; 3 * 2 * 4];
    assert!(Surface::new(&mut exact, 3, 2, 3, PixelOrder::Bgrx).is_some());

    let mut longer = vec![SENTINEL; 3 * 2 * 4 + 5];
    let mut surface = Surface::new(&mut longer, 3, 2, 3, PixelOrder::Bgrx).unwrap();
    assert_eq!((surface.width(), surface.height()), (3, 2));
    assert_eq!(surface.order(), PixelOrder::Bgrx);
    surface.fill(ORANGE);
    assert_eq!(
        &longer[24..],
        &[SENTINEL; 5],
        "the tail past the surface is not ours"
    );
}

#[test]
fn new_rejects_a_short_slice() {
    let mut short = vec![0; 3 * 2 * 4 - 1];
    assert!(Surface::new(&mut short, 3, 2, 3, PixelOrder::Bgrx).is_none());
    // The last scanline's padding is still part of the surface.
    let mut no_padding = vec![0; 4 * 2 * 4 - 4];
    assert!(Surface::new(&mut no_padding, 3, 2, 4, PixelOrder::Bgrx).is_none());
}

#[test]
fn new_rejects_stride_narrower_than_width() {
    let mut bytes = vec![0; 64];
    assert!(Surface::new(&mut bytes, 4, 1, 3, PixelOrder::Rgbx).is_none());
}

#[test]
fn new_rejects_overflowing_dimensions() {
    let mut bytes = vec![0; 64];
    for (width, height, stride) in [
        (1, usize::MAX, 2),
        (1, 2, usize::MAX),
        (1, 1, usize::MAX / 2),
        (usize::MAX, usize::MAX, usize::MAX),
        (1, usize::MAX / 4 + 1, 1),
    ] {
        assert!(
            Surface::new(&mut bytes, width, height, stride, PixelOrder::Bgrx).is_none(),
            "{width}x{height} stride {stride}"
        );
    }
}

#[test]
fn an_empty_surface_draws_nothing_and_does_not_panic() {
    let mut bytes = Vec::<u8>::new();
    let mut surface = Surface::new(&mut bytes, 0, 0, 0, PixelOrder::Bgrx).unwrap();
    surface.fill(ORANGE);
    surface.fill_rect(0, 0, usize::MAX, usize::MAX, ORANGE);
    surface.draw_char(0, 0, 'A', 1, ORANGE, Some(BLUE));
    let mut text = TextArea::new(&mut surface, Rect::default(), 1, ORANGE, None);
    assert!(text.is_full());
    write!(text, "anything").unwrap();
}

// ---------------------------------------------------------------------------
// Pixel order and stride
// ---------------------------------------------------------------------------

#[test]
fn bgrx_writes_blue_first() {
    let mut fb = Fb::new(3, 2, 3);
    fb.surface(PixelOrder::Bgrx).fill_rect(1, 1, 1, 1, ORANGE);
    assert_eq!(&fb.bytes[16..20], &[0x33, 0x22, 0x11, 0]);
    fb.assert_pixels("bgrx", |x, y| if (x, y) == (1, 1) { FG } else { UNTOUCHED });
}

#[test]
fn rgbx_writes_red_first() {
    let mut fb = Fb::new(3, 2, 3);
    fb.surface(PixelOrder::Rgbx).fill_rect(2, 0, 1, 1, ORANGE);
    assert_eq!(&fb.bytes[8..12], &[0x11, 0x22, 0x33, 0]);
    let red_first = [0x11, 0x22, 0x33, 0];
    fb.assert_pixels("rgbx", |x, y| {
        if (x, y) == (2, 0) {
            red_first
        } else {
            UNTOUCHED
        }
    });
}

#[test]
fn fill_leaves_the_stride_padding_alone() {
    let (width, height, stride) = (3, 4, 5);
    let mut fb = Fb::new(width, height, stride);
    fb.surface(PixelOrder::Bgrx).fill(BLUE);
    fb.assert_pixels("padding", |x, _| if x < width { BG } else { UNTOUCHED });
}

#[test]
fn fill_covers_a_full_hd_screen() {
    let mut fb = Fb::new(1920, 1080, 1920);
    fb.surface(PixelOrder::Rgbx).fill(ORANGE);
    for pixel in fb.bytes.as_chunks::<4>().0 {
        assert_eq!(pixel, &[0x11, 0x22, 0x33, 0]);
    }
}

// ---------------------------------------------------------------------------
// fill_rect clipping
// ---------------------------------------------------------------------------

#[test]
fn fill_rect_clips_at_every_edge() {
    let (width, height, stride) = (7, 5, 9);
    let origins = [0, 1, 4, 5, 6, 7, 8, 100, usize::MAX - 1, usize::MAX];
    let sizes = [0, 1, 2, 3, 5, 6, 7, 8, usize::MAX - 1, usize::MAX];
    let sizes = pairs(&sizes, &sizes);
    for (x, y) in pairs(&origins, &origins) {
        for &(w, h) in &sizes {
            let mut fb = Fb::new(width, height, stride);
            fb.surface(PixelOrder::Bgrx).fill_rect(x, y, w, h, ORANGE);
            let inside =
                |px: usize, py: usize| px < width && px >= x && px - x < w && py >= y && py - y < h;
            let label = format!("rect ({x}, {y}) {w}x{h}");
            fb.assert_pixels(&label, |px, py| if inside(px, py) { FG } else { UNTOUCHED });
        }
    }
}

#[test]
fn fill_rect_fills_spans_of_every_length() {
    // The doubling copy has a partial last step for any width that is not a
    // power of two.
    for width in 1..=67 {
        let mut fb = Fb::new(70, 1, 70);
        fb.surface(PixelOrder::Bgrx).fill_rect(2, 0, width, 1, BLUE);
        let label = format!("span {width}");
        fb.assert_pixels(&label, |x, _| {
            if (2..2 + width).contains(&x) {
                BG
            } else {
                UNTOUCHED
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------

#[test]
fn the_font_is_the_spleen_rows() {
    // Spot checks against the BDF, so that a generator that shifted or
    // mirrored every row would not pass by drawing consistently wrong.
    assert_eq!(glyph('A')[2], 0b0111_1100);
    assert_eq!(glyph('A')[3], 0b1100_0110);
    assert_eq!(glyph('A')[6], 0b1111_1110);
    assert_eq!(glyph(' '), &[0; GLYPH_HEIGHT]);
}

#[test]
fn every_printable_character_has_its_own_glyph() {
    let replacement = glyph('\u{80}');
    for code in 0x20..=0x7E_u8 {
        let ch = char::from(code);
        assert!(!core::ptr::eq(glyph(ch), replacement), "{ch:?}");
    }
}

#[test]
fn everything_else_is_the_replacement_box() {
    let replacement = glyph('\u{FFFD}');
    assert_eq!(replacement[4], 0b0111_1100, "the box's top edge");
    assert_eq!(replacement[5], 0b0100_0100, "the box's sides");
    for ch in ['\0', '\u{1f}', '\u{7f}', '\n', 'é', '€', '\u{10FFFF}'] {
        assert!(core::ptr::eq(glyph(ch), replacement), "{ch:?}");
    }
}

#[test]
fn draw_char_renders_a_glyph_pixel_exactly() {
    let mut fb = Fb::new(20, 30, 24);
    fb.surface(PixelOrder::Bgrx)
        .draw_char(3, 5, 'A', 1, ORANGE, Some(BLUE));
    assert_cell(&fb, 3, 5, 'A', 1, FG, BG);

    // Named pixels, independently of the checker: row 2 of 'A' is .#####..
    assert_eq!(fb.pixel(3, 7), BG);
    assert_eq!(fb.pixel(4, 7), FG);
    assert_eq!(fb.pixel(8, 7), FG);
    assert_eq!(fb.pixel(9, 7), BG);

    // Nothing outside the cell.
    let inside = |x: usize, y: usize| (3..11).contains(&x) && (5..21).contains(&y);
    fb.assert_pixels("outside 'A'", |x, y| {
        if inside(x, y) {
            fb.pixel(x, y)
        } else {
            UNTOUCHED
        }
    });
}

#[test]
fn draw_char_without_background_leaves_clear_pixels() {
    let mut fb = Fb::new(8, 16, 8);
    fb.surface(PixelOrder::Bgrx)
        .draw_char(0, 0, 'A', 1, ORANGE, None);
    assert_cell(&fb, 0, 0, 'A', 1, FG, UNTOUCHED);
}

#[test]
fn draw_char_at_scale_two_doubles_every_pixel() {
    let mut fb = Fb::new(40, 40, 40);
    fb.surface(PixelOrder::Bgrx)
        .draw_char(1, 2, 'g', 2, ORANGE, Some(BLUE));
    assert_cell(&fb, 1, 2, 'g', 2, FG, BG);
    assert_eq!(fb.pixel(0, 2), UNTOUCHED);
    assert_eq!(fb.pixel(17, 2), UNTOUCHED);
    assert_eq!(fb.pixel(1, 34), UNTOUCHED);
}

#[test]
fn draw_char_at_scale_zero_draws_nothing() {
    let mut fb = Fb::new(8, 16, 8);
    fb.surface(PixelOrder::Bgrx)
        .draw_char(0, 0, 'A', 0, ORANGE, Some(BLUE));
    assert!(fb.bytes.iter().all(|&byte| byte == SENTINEL));
}

#[test]
fn draw_char_draws_the_replacement_for_non_ascii() {
    let mut fb = Fb::new(8, 16, 8);
    fb.surface(PixelOrder::Bgrx)
        .draw_char(0, 0, 'ß', 1, ORANGE, Some(BLUE));
    assert_cell(&fb, 0, 0, '\u{FFFD}', 1, FG, BG);
}

#[test]
fn draw_char_is_clipped_at_the_edges() {
    let (width, height) = (10, 20);
    for (left, top) in [(6, 12), (9, 19), (10, 0), (0, 20)] {
        let mut fb = Fb::new(width, height, width);
        fb.surface(PixelOrder::Bgrx)
            .draw_char(left, top, 'M', 1, ORANGE, Some(BLUE));
        let model = |x: usize, y: usize| {
            let inside = (left..left + 8).contains(&x) && (top..top + 16).contains(&y);
            match inside {
                false => UNTOUCHED,
                true if font_bit('M', x - left, y - top) => FG,
                true => BG,
            }
        };
        fb.assert_pixels(&format!("'M' at ({left}, {top})"), model);
    }
}

#[test]
fn draw_char_survives_absurd_arguments() {
    let mut fb = Fb::new(10, 20, 10);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    for (x, y, scale) in [
        (usize::MAX, usize::MAX, 1),
        (usize::MAX, 0, usize::MAX),
        (usize::MAX - 3, 0, 2),
        (0, usize::MAX - 20, 3),
        (5, 5, usize::MAX / 2),
        (0, 0, usize::MAX),
    ] {
        surface.draw_char(x, y, 'W', scale, ORANGE, Some(BLUE));
    }
    // Scale usize::MAX at the origin covers the whole surface with the top-left
    // font pixel of 'W', which is clear.
    assert!(!font_bit('W', 0, 0));
    fb.assert_pixels("one huge font pixel", |_, _| BG);
}

// ---------------------------------------------------------------------------
// TextArea
// ---------------------------------------------------------------------------

/// A region of four columns and three rows at (4, 6), with a few pixels over
/// in each direction that must stay untouched.
const SMALL: Rect = Rect {
    x: 4,
    y: 6,
    width: 4 * 8 + 3,
    height: 3 * 16 + 2,
};

/// Assert that row `row` of a `SMALL` area holds `text` in `ORANGE` on
/// `BLUE`, and nothing after it.
fn assert_line(fb: &Fb, row: usize, text: &str) {
    for (column, ch) in text.chars().enumerate() {
        assert_cell(fb, SMALL.x + column * 8, SMALL.y + row * 16, ch, 1, FG, BG);
    }
    let drawn_to = SMALL.x + text.chars().count() * 8;
    for x in drawn_to..SMALL.x + SMALL.width {
        assert_eq!(
            fb.pixel(x, SMALL.y + row * 16),
            UNTOUCHED,
            "row {row}, x {x}"
        );
    }
}

#[test]
fn text_area_counts_whole_cells() {
    let mut fb = Fb::new(60, 70, 60);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let text = TextArea::new(&mut surface, SMALL, 1, ORANGE, Some(BLUE));
    assert_eq!((text.columns(), text.rows()), (4, 3));
    assert!(!text.is_full());

    let region = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 70,
    };
    let doubled = TextArea::new(&mut surface, region, 2, ORANGE, None);
    assert_eq!((doubled.columns(), doubled.rows()), (3, 2));

    let zero = TextArea::new(&mut surface, region, 0, ORANGE, None);
    assert_eq!((zero.columns(), zero.rows()), (7, 4), "scale 0 is scale 1");

    let huge = TextArea::new(&mut surface, region, usize::MAX, ORANGE, None);
    assert_eq!((huge.columns(), huge.rows()), (0, 0));
    assert!(huge.is_full());
}

#[test]
fn text_area_region_is_clipped_to_the_surface() {
    let mut fb = Fb::new(20, 20, 20);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let region = Rect {
        x: 4,
        y: 2,
        width: usize::MAX,
        height: usize::MAX,
    };
    let text = TextArea::new(&mut surface, region, 1, ORANGE, None);
    assert_eq!((text.columns(), text.rows()), (2, 1));

    let outside = Rect {
        x: usize::MAX,
        y: usize::MAX,
        width: usize::MAX,
        height: 5,
    };
    let mut text = TextArea::new(&mut surface, outside, 1, ORANGE, None);
    assert!(text.is_full());
    write!(text, "dropped").unwrap();
    assert!(fb.bytes.iter().all(|&byte| byte == SENTINEL));
}

#[test]
fn text_area_wraps_at_the_right_edge_and_honours_newline() {
    let mut fb = Fb::new(60, 70, 64);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let mut text = TextArea::new(&mut surface, SMALL, 1, ORANGE, Some(BLUE));
    write!(text, "abcdefg\nhi").unwrap();
    assert!(!text.is_full());
    assert_line(&fb, 0, "abcd");
    assert_line(&fb, 1, "efg");
    assert_line(&fb, 2, "hi");
}

#[test]
fn a_line_exactly_as_wide_as_the_area_does_not_leave_a_blank_line() {
    let mut fb = Fb::new(60, 70, 60);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let mut text = TextArea::new(&mut surface, SMALL, 1, ORANGE, Some(BLUE));
    writeln!(text, "abcd").unwrap();
    write!(text, "ef").unwrap();
    assert_line(&fb, 0, "abcd");
    assert_line(&fb, 1, "ef");
    assert_line(&fb, 2, "");
}

#[test]
fn text_area_stops_drawing_when_full_but_keeps_accepting() {
    let mut fb = Fb::new(60, 70, 60);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let mut text = TextArea::new(&mut surface, SMALL, 1, ORANGE, Some(BLUE));
    write!(text, "abcdefghijkl").unwrap();
    // The last cell is filled but the wrap is deferred until more arrives.
    assert!(!text.is_full());
    let snapshot = fb.bytes.clone();

    let mut surface = fb.surface(PixelOrder::Bgrx);
    let mut text = TextArea::new(&mut surface, SMALL, 1, ORANGE, Some(BLUE));
    write!(text, "abcdefghijkl").unwrap();
    assert_eq!(write!(text, "m"), Ok(()));
    assert!(text.is_full());
    for _ in 0..100 {
        assert_eq!(write!(text, "more text {}\n\t\r", 12345), Ok(()));
        text.newline();
    }
    assert!(text.is_full());
    assert_eq!(fb.bytes, snapshot, "nothing is drawn once the area is full");

    assert_line(&fb, 0, "abcd");
    assert_line(&fb, 1, "efgh");
    assert_line(&fb, 2, "ijkl");
    let inside = |x: usize, y: usize| {
        (SMALL.x..SMALL.x + 32).contains(&x) && (SMALL.y..SMALL.y + 48).contains(&y)
    };
    fb.assert_pixels("outside the area", |x, y| {
        if inside(x, y) {
            fb.pixel(x, y)
        } else {
            UNTOUCHED
        }
    });
}

#[test]
fn newline_on_the_last_row_fills_the_area() {
    let mut fb = Fb::new(60, 70, 60);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let mut text = TextArea::new(&mut surface, SMALL, 1, ORANGE, None);
    text.newline();
    text.newline();
    assert!(!text.is_full());
    text.newline();
    assert!(text.is_full());
}

#[test]
fn set_color_applies_to_what_follows() {
    let mut fb = Fb::new(60, 70, 60);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let mut text = TextArea::new(&mut surface, SMALL, 1, ORANGE, Some(BLUE));
    write!(text, "a").unwrap();
    text.set_color(Rgb::WHITE);
    write!(text, "b").unwrap();
    assert_cell(&fb, SMALL.x, SMALL.y, 'a', 1, FG, BG);
    assert_cell(&fb, SMALL.x + 8, SMALL.y, 'b', 1, [0xFF, 0xFF, 0xFF, 0], BG);
}

#[test]
fn tab_advances_to_the_next_stop_and_carriage_return_rewinds() {
    let mut fb = Fb::new(200, 16, 200);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let region = Rect {
        x: 0,
        y: 0,
        width: 200,
        height: 16,
    };
    let mut text = TextArea::new(&mut surface, region, 1, ORANGE, Some(BLUE));
    write!(text, "ab\tc\rZ").unwrap();
    assert_cell(&fb, 0, 0, 'Z', 1, FG, BG);
    assert_cell(&fb, 8, 0, 'b', 1, FG, BG);
    assert_cell(&fb, 16, 0, ' ', 1, FG, BG);
    assert_cell(&fb, 56, 0, ' ', 1, FG, BG);
    assert_cell(&fb, 64, 0, 'c', 1, FG, BG);
    assert_eq!(fb.pixel(72, 0), UNTOUCHED);
}

#[test]
fn text_area_at_scale_two() {
    let mut fb = Fb::new(40, 70, 40);
    let mut surface = fb.surface(PixelOrder::Bgrx);
    let region = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 70,
    };
    let mut text = TextArea::new(&mut surface, region, 2, ORANGE, Some(BLUE));
    assert_eq!((text.columns(), text.rows()), (2, 2));
    write!(text, "xyz").unwrap();
    assert_cell(&fb, 0, 0, 'x', 2, FG, BG);
    assert_cell(&fb, 16, 0, 'y', 2, FG, BG);
    assert_cell(&fb, 0, 32, 'z', 2, FG, BG);
}
