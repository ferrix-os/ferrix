//! The two screens the splash draws: the mark with its line of text, and the
//! boot log under a small mark.
//!
//! Both are drawn whole into the buffer the card scans out. They change a
//! few times in a boot and once a second while a note counts, so nothing is
//! saved by painting less.

use ferrix_fbtext::{GLYPH_HEIGHT, GLYPH_WIDTH, PixelOrder, Rgb, Surface};
use ferrix_splash::{Canvas, INK, Mark};

use crate::boot::Note;

/// The brand's text colour, as `0xRRGGBB`.
const TEXT: u32 = 0xEE_F1_F5;
/// Its muted text colour.
const MUTED: u32 = 0x9A_A4_B2;
/// Bytes in a pixel of an `XRGB8888` buffer.
const PIXEL: usize = 4;
/// The fewest columns the log is drawn with: the largest glyphs that still
/// leave this many are used.
const LOG_COLUMNS: usize = 100;

/// A buffer the card shows: `XRGB8888`, blue first.
#[derive(Debug)]
pub struct Frame<'a> {
    /// The buffer's bytes.
    pixels: &'a mut [u8],
    /// Its width in pixels.
    width: u32,
    /// Its height.
    height: u32,
    /// Bytes from one row to the next.
    pitch: usize,
}

impl<'a> Frame<'a> {
    /// A frame over `pixels`, or `None` when they are fewer than `height`
    /// rows of `pitch` bytes holding `width` pixels each.
    #[must_use]
    pub fn new(pixels: &'a mut [u8], width: u32, height: u32, pitch: u32) -> Option<Self> {
        let pitch = usize::try_from(pitch).ok()?;
        let row = usize::try_from(width).ok()?.checked_mul(PIXEL)?;
        let size = usize::try_from(height).ok()?.checked_mul(pitch)?;
        (row <= pitch && size <= pixels.len()).then_some(Self {
            pixels,
            width,
            height,
            pitch,
        })
    }

    /// The frame as the text crate draws on one.
    fn surface(&mut self) -> Option<Surface<'_>> {
        Surface::new(
            self.pixels,
            usize::try_from(self.width).ok()?,
            usize::try_from(self.height).ok()?,
            self.pitch / PIXEL,
            PixelOrder::Bgrx,
        )
    }

    /// The shorter of its sides.
    const fn shorter(&self) -> u32 {
        if self.width < self.height {
            self.width
        } else {
            self.height
        }
    }
}

impl Canvas for Frame<'_> {
    fn span(&mut self, y: u32, from: u32, to: u32, rgb: u32) {
        if y >= self.height {
            return;
        }
        let at = |x: u32| {
            usize::try_from(y)
                .ok()
                .and_then(|y| y.checked_mul(self.pitch))
                .zip(usize::try_from(x.min(self.width)).ok())
                .map(|(row, x)| row + x * PIXEL)
        };
        let Some(row) = at(from)
            .zip(at(to))
            .and_then(|(from, to)| self.pixels.get_mut(from..to))
        else {
            return;
        };
        let [_, red, green, blue] = rgb.to_be_bytes();
        for pixel in row.as_chunks_mut::<PIXEL>().0 {
            *pixel = [blue, green, red, 0];
        }
    }
}

/// `rgb` as the text crate takes a colour.
const fn colour(rgb: u32) -> Rgb {
    let [_, red, green, blue] = rgb.to_be_bytes();
    Rgb::new(red, green, blue)
}

/// Draw the splash: ink, the mark with its slash at `level`, and `note`
/// under it.
pub fn splash(frame: &mut Frame<'_>, level: u32, note: Option<&Note>) {
    let (width, height, shorter) = (frame.width, frame.height, frame.shorter());
    if let Some(mut surface) = frame.surface() {
        surface.fill(colour(INK));
    }
    let Some(mark) = Mark::centred(width, height) else {
        return;
    };
    mark.draw(frame, level);
    let (Some(note), Some(mut surface)) = (note, frame.surface()) else {
        return;
    };
    // A line of text about a fortieth of the shorter side high: 32 pixels on
    // a 1080p screen, 16 on a small panel.
    let scale = usize::try_from((shorter * 3 / 100 + 8) / 16)
        .unwrap_or(1)
        .max(1);
    let line = GLYPH_HEIGHT * scale;
    let top = usize::try_from(mark.bottom() + shorter * 55 / 1000).unwrap_or(usize::MAX);
    let ink = if note.strong { TEXT } else { MUTED };
    centred(&mut surface, top, &note.text, scale, ink);
    centred(
        &mut surface,
        top.saturating_add(line * 17 / 10),
        note.hint,
        scale.saturating_sub(1).max(1),
        MUTED,
    );
}

/// Draw `text` in the middle of row `top`, cut to what the screen holds.
fn centred(surface: &mut Surface<'_>, top: usize, text: &str, scale: usize, rgb: u32) {
    let cell = GLYPH_WIDTH * scale;
    let fits = surface.width() / cell;
    let count = text.chars().count().min(fits);
    let left = (surface.width() - count * cell) / 2;
    line(surface, (left, top), text, scale, (rgb, count));
}

/// Draw the first `count` characters of `text` from (`left`, `top`).
fn line(
    surface: &mut Surface<'_>,
    (left, top): (usize, usize),
    text: &str,
    scale: usize,
    (rgb, count): (u32, usize),
) {
    for (column, ch) in text.chars().take(count).enumerate() {
        let x = left + column * GLYPH_WIDTH * scale;
        surface.draw_char(x, top, ch, scale, colour(rgb), None);
    }
}

/// Rows and columns of log text a `width` by `height` screen shows.
#[must_use]
pub fn log_size(width: u32, height: u32) -> (usize, usize) {
    Log::of(width, height).map_or((0, 0), |log| (log.rows, log.columns))
}

/// Where the log's text goes.
struct Log {
    /// The small mark in the corner.
    mark: Mark,
    /// Each font pixel is this many pixels square.
    scale: usize,
    /// Left edge of the text.
    left: usize,
    /// Top edge of the text.
    top: usize,
    /// Lines that fit.
    rows: usize,
    /// Characters that fit in one.
    columns: usize,
}

impl Log {
    /// The layout for a `width` by `height` screen, or `None` on one too
    /// small for the mark.
    fn of(width: u32, height: u32) -> Option<Self> {
        let shorter = width.min(height);
        let margin = shorter / 20;
        let mark = Mark::at(margin, margin, (shorter * 45 / 1000).max(16))?;
        let wide = usize::try_from(width.saturating_sub(margin * 2)).ok()?;
        let top = usize::try_from(mark.bottom() + margin / 2).ok()?;
        let tall = usize::try_from(height.saturating_sub(margin))
            .ok()?
            .saturating_sub(top);
        let scale = (wide / (GLYPH_WIDTH * LOG_COLUMNS)).max(1);
        let rows = tall / (GLYPH_HEIGHT * scale);
        let columns = wide / (GLYPH_WIDTH * scale);
        // A screen with no room for a line under the mark has no log view.
        (rows > 0 && columns > 0 && margin + mark.width() <= width).then_some(Self {
            mark,
            scale,
            left: usize::try_from(margin).ok()?,
            top,
            rows,
            columns,
        })
    }
}

/// Draw the log view: ink, the mark small in the corner with its slash at
/// `level`, and `lines` under it, oldest first, the last one in the text
/// colour. The caller gives no more lines than [`log_size`] has rows.
pub fn log<'l>(frame: &mut Frame<'_>, level: u32, lines: impl Iterator<Item = &'l str>) {
    let layout = Log::of(frame.width, frame.height);
    if let Some(mut surface) = frame.surface() {
        surface.fill(colour(INK));
    }
    let Some(layout) = layout else {
        return;
    };
    layout.mark.draw(frame, level);
    let Some(mut surface) = frame.surface() else {
        return;
    };
    let mut lines = lines.take(layout.rows).enumerate().peekable();
    while let Some((row, text)) = lines.next() {
        let rgb = if lines.peek().is_some() { MUTED } else { TEXT };
        let top = layout.top + row * GLYPH_HEIGHT * layout.scale;
        line(
            &mut surface,
            (layout.left, top),
            text,
            layout.scale,
            (rgb, layout.columns),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, INK, MUTED, TEXT, log, log_size, splash};
    use crate::boot::Note;
    use ferrix_splash::{MARK, RUST, UNLIT};

    /// A buffer with a pitch wider than its rows, as a card may give one.
    struct Buffer {
        /// Its bytes.
        bytes: Vec<u8>,
        /// Its size in pixels.
        size: (u32, u32),
    }

    impl Buffer {
        /// Bytes from one row to the next.
        const fn pitch(&self) -> u32 {
            self.size.0 * 4 + 16
        }

        /// A buffer of bytes no picture holds.
        fn new(width: u32, height: u32) -> Self {
            let mut buffer = Self {
                bytes: Vec::new(),
                size: (width, height),
            };
            buffer.bytes = vec![0x55; (buffer.pitch() * height) as usize];
            buffer
        }

        /// The frame over it.
        fn frame(&mut self) -> Frame<'_> {
            let pitch = self.pitch();
            Frame::new(&mut self.bytes, self.size.0, self.size.1, pitch).expect("large enough")
        }

        /// The pixel at (`x`, `y`), as `0xRRGGBB`.
        fn at(&self, x: u32, y: u32) -> u32 {
            let at = (y * self.pitch() + x * 4) as usize;
            match self.bytes.get(at..at + 4) {
                Some(&[blue, green, red, _]) => u32::from_be_bytes([0, red, green, blue]),
                _ => 0,
            }
        }

        /// How many pixels of rows `rows` are `rgb`.
        fn count(&self, rows: std::ops::Range<u32>, rgb: u32) -> usize {
            rows.flat_map(|y| (0..self.size.0).map(move |x| (x, y)))
                .filter(|&(x, y)| self.at(x, y) == rgb)
                .count()
        }
    }

    #[test]
    fn a_frame_needs_its_rows() {
        let mut bytes = vec![0; 4 * 4 * 3];
        assert!(Frame::new(&mut bytes, 4, 3, 16).is_some());
        assert!(Frame::new(&mut bytes, 4, 4, 16).is_none(), "a row short");
        assert!(Frame::new(&mut bytes, 5, 3, 16).is_none(), "rows too wide");
    }

    #[test]
    fn the_splash_is_ink_with_the_mark_in_the_middle_and_no_text() {
        let mut buffer = Buffer::new(1920, 1080);
        splash(&mut buffer.frame(), 0, None);
        assert_eq!(buffer.at(0, 0), INK);
        assert_eq!(buffer.at(1919, 1079), INK);
        assert_eq!(buffer.at(960 - 100, 540 - 90), MARK, "the F's top bar");
        assert!(buffer.count(432..648, UNLIT) > 1000, "the slash, unlit");
        assert_eq!(buffer.count(0..1080, RUST), 0);
        assert_eq!(buffer.count(648..1080, MUTED), 0);
        // The bytes past each row's pixels are the card's, not the picture's.
        assert_eq!(buffer.bytes.get(1920 * 4), Some(&0x55));
        splash(&mut buffer.frame(), 1000, None);
        assert_eq!(buffer.count(0..1080, UNLIT), 0);
        assert!(buffer.count(432..648, RUST) > 1000);
    }

    #[test]
    fn a_note_is_under_the_mark_muted_and_a_stopped_boot_s_in_the_text_colour() {
        let quiet = Note {
            text: "starting services - 14 s".to_owned(),
            hint: "Esc shows the log",
            strong: false,
        };
        let mut buffer = Buffer::new(1920, 1080);
        splash(&mut buffer.frame(), 860, Some(&quiet));
        assert!(buffer.count(648..1080, MUTED) > 500);
        assert_eq!(buffer.count(0..1080, TEXT), 0);
        assert_eq!(
            buffer.count(0..648, MUTED),
            0,
            "nothing above the mark's foot"
        );

        let stopped = Note {
            strong: true,
            ..quiet
        };
        splash(&mut buffer.frame(), 860, Some(&stopped));
        assert!(buffer.count(648..1080, TEXT) > 500);
        assert!(buffer.count(648..1080, MUTED) > 100, "the hint stays muted");
    }

    #[test]
    fn a_note_longer_than_the_screen_is_cut_not_wrapped() {
        let long = Note {
            text: "x".repeat(400),
            hint: "",
            strong: true,
        };
        let mut buffer = Buffer::new(480, 800);
        splash(&mut buffer.frame(), 0, Some(&long));
        let rows: Vec<u32> = (0..800)
            .filter(|&y| buffer.count(y..y + 1, TEXT) > 0)
            .collect();
        assert!(!rows.is_empty());
        assert!(rows.len() <= 16, "one line of glyphs: {} rows", rows.len());
    }

    #[test]
    fn the_log_has_a_small_mark_and_its_last_line_in_the_text_colour() {
        let (rows, columns) = log_size(1920, 1080);
        assert!(rows >= 20 && columns >= 100, "{rows} rows of {columns}");
        let mut buffer = Buffer::new(1920, 1080);
        log(
            &mut buffer.frame(),
            500,
            ["devmgr nvme0", "btrfs check"].into_iter(),
        );
        assert_eq!(buffer.at(1919, 1079), INK);
        assert!(buffer.count(54..103, MARK) > 200, "the mark in the corner");
        assert!(buffer.count(0..1080, MUTED) > 50);
        assert!(buffer.count(0..1080, TEXT) > 50);
        // More lines than rows: the rest are not drawn past the bottom.
        let many = vec!["line"; rows + 50];
        log(&mut buffer.frame(), 500, many.into_iter());
        assert_eq!(buffer.count(1080 - 54..1080, MUTED), 0);
    }

    #[test]
    fn a_tiny_screen_is_ink_and_nothing_else() {
        let mut buffer = Buffer::new(40, 30);
        let note = Note {
            text: "x".to_owned(),
            hint: "",
            strong: true,
        };
        splash(&mut buffer.frame(), 500, Some(&note));
        log(&mut buffer.frame(), 500, ["x"].into_iter());
        assert_eq!(buffer.count(0..30, INK), 40 * 30);
        assert_eq!(log_size(40, 30), (0, 0));
    }
}
