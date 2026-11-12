//! The picture, drawn on a screen in memory.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::{Canvas, FULL, INK, MARK, Mark, RUST, UNLIT};

/// A screen in memory that counts what is asked of it.
struct Screen {
    /// Its width.
    width: u32,
    /// Its height.
    height: u32,
    /// Its pixels, row by row.
    pixels: Vec<u32>,
    /// Pixels painted since it was made.
    painted: usize,
}

impl Screen {
    /// A screen of ink.
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![INK; (width * height) as usize],
            painted: 0,
        }
    }

    /// The pixel at (`x`, `y`).
    fn at(&self, x: u32, y: u32) -> u32 {
        self.pixels
            .get((y * self.width + x) as usize)
            .copied()
            .unwrap_or(0)
    }

    /// The pixel at a place of the drawing, given in its own 613 by 498
    /// units, under `mark`.
    fn under(&self, mark: &Mark, (x, y): (u32, u32)) -> u32 {
        self.at(
            mark.left() + x * mark.height() / 498,
            mark.top() + y * mark.height() / 498,
        )
    }
}

impl Canvas for Screen {
    fn span(&mut self, y: u32, from: u32, to: u32, rgb: u32) {
        assert!(y < self.height, "row {y} is off the screen");
        assert!(
            from < to && to <= self.width,
            "span {from}..{to} on row {y}"
        );
        for x in from..to {
            if let Some(pixel) = self.pixels.get_mut((y * self.width + x) as usize) {
                *pixel = rgb;
            }
            self.painted += 1;
        }
    }
}

/// The mark on a 1920 by 1080 screen at `level`.
fn drawn(level: u32) -> (Screen, Mark) {
    let mut screen = Screen::new(1920, 1080);
    let mark = Mark::centred(1920, 1080).expect("the mark fits a 1080p screen");
    mark.draw(&mut screen, level);
    (screen, mark)
}

#[test]
fn the_mark_is_a_fifth_of_the_shorter_side_and_centred() {
    let mark = Mark::centred(1920, 1080).expect("fits");
    assert_eq!(mark.height(), 216);
    assert_eq!(mark.width(), 266);
    assert_eq!(mark.top(), (1080 - 216) / 2);
    assert_eq!(mark.left(), (1920 - 266) / 2);
    let upright = Mark::centred(1080, 2400).expect("fits");
    assert_eq!(upright.height(), 216);
    assert_eq!(upright.top(), (2400 - 216) / 2);
}

#[test]
fn a_screen_too_small_has_no_mark() {
    assert_eq!(Mark::centred(64, 64), None);
    assert_eq!(
        Mark::at_height(100, 400, 200),
        None,
        "wider than the screen"
    );
    assert_eq!(Mark::at(0, 0, 8), None);
}

#[test]
fn each_part_is_where_the_drawing_has_it() {
    let (screen, mark) = drawn(0);
    // The F's top bar, its stem, the leg, the chip.
    for place in [(200, 50), (50, 300), (500, 450), (200, 450)] {
        assert_eq!(screen.under(&mark, place), MARK, "{place:?}");
    }
    // The notch between the bars, the gap between leg and slash, the gap
    // between stem and chip, outside the mark.
    for place in [(250, 135), (340, 120), (115, 400), (600, 400)] {
        assert_eq!(screen.under(&mark, place), INK, "{place:?}");
    }
    assert_eq!(screen.under(&mark, (500, 60)), UNLIT, "the slash, unlit");
}

#[test]
fn the_slash_lights_from_its_foot() {
    let tip = (540, 20);
    let foot = (390, 215);
    let (none, mark) = drawn(0);
    assert_eq!(none.under(&mark, tip), UNLIT);
    assert_eq!(none.under(&mark, foot), UNLIT);
    let (half, _) = drawn(FULL / 2);
    assert_eq!(half.under(&mark, tip), UNLIT);
    assert_eq!(half.under(&mark, foot), RUST);
    let (whole, _) = drawn(FULL);
    assert_eq!(whole.under(&mark, tip), RUST);
    assert_eq!(whole.under(&mark, foot), RUST);
    let (past, _) = drawn(FULL * 3);
    assert_eq!(past.pixels, whole.pixels, "a level past full is full");
}

#[test]
fn lighting_a_drawn_slash_gives_the_picture_drawn_at_that_level() {
    for (from, to) in [(0, 100), (100, 700), (700, FULL), (FULL, 300), (250, 250)] {
        let (mut screen, mark) = drawn(from);
        let before = screen.painted;
        mark.light(&mut screen, from, to);
        let (whole, _) = drawn(to);
        assert_eq!(screen.pixels, whole.pixels, "{from} to {to}");
        assert!(
            screen.painted - before < before / 4,
            "{from} to {to} painted {} of {before} pixels",
            screen.painted - before
        );
    }
}

#[test]
fn nothing_is_painted_outside_the_mark_at_any_size() {
    // Screen's span refuses a pixel off the screen; the box is checked here.
    for tall in [16, 17, 31, 96, 160, 216, 431, 1000] {
        let wide = Mark::at(0, 0, tall).expect("tall enough").width();
        let mut screen = Screen::new(wide + 8, tall + 8);
        let mark = Mark::at(4, 4, tall).expect("tall enough");
        mark.draw(&mut screen, 400);
        for y in 0..screen.height {
            for x in 0..screen.width {
                let inside = (4..4 + wide).contains(&x) && (4..4 + tall).contains(&y);
                assert!(
                    inside || screen.at(x, y) == INK,
                    "({x}, {y}) at {tall} rows"
                );
            }
        }
    }
}

#[test]
fn an_edge_pixel_is_between_the_shape_and_the_ink() {
    let (screen, mark) = drawn(FULL);
    // The slash's slanted sides cut pixels: some pixel on a row through it
    // is neither rust nor ink.
    let y = mark.top() + mark.height() / 4;
    let mixed = (mark.left()..mark.left() + mark.width())
        .map(|x| screen.at(x, y))
        .filter(|&pixel| ![INK, MARK, RUST].contains(&pixel))
        .count();
    assert!(
        (1..=6).contains(&mixed),
        "{mixed} blended pixels on row {y}"
    );
}
