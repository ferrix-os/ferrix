//! The boot console, drawn on the framebuffer firmware left behind.
//!
//! A machine whose serial port is out of reach shows nothing between its
//! loader and a panic. The Pixel 7 is one: its console is a RAM record read
//! back after the reset that ends a run, and until then the screen is the only
//! thing a person holding it can see. So the kernel's own console lines -- the
//! same bytes [`super::recent`] keeps, and so the same ones the log and a
//! later panic screen show -- are drawn here as they are printed. A program's
//! output is not: this is the boot console, not a terminal.
//!
//! It is asked for with `ferrix.fbcon` on the command line and is off
//! otherwise. On a machine with a cable a second copy of the log only costs
//! time, and the QEMU heads are compared against screenshots that expect them
//! as firmware left them.
//!
//! Nothing is read back and nothing scrolls, since a scroll reads the
//! framebuffer, which is a device mapping. Text runs down the screen and
//! starts again at the top, and the row after the newest line is kept blank,
//! so the newest line is the one above the gap.
//!
//! It stops for good when a panic draws its own screen over it, and when a
//! display driver takes the screen (`display::`). `docs/ARCHITECTURE.md` §1
//! names the framebuffer as one of the kernel's two output devices; this is
//! the second use of it, write-only as the first.

use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::BootView;
use ferrix_fbtext::{GLYPH_HEIGHT, GLYPH_WIDTH, Rgb, Surface};
use ferrix_sync::SpinLock;

/// The command-line flag that asks for it.
const FLAG: &str = "ferrix.fbcon";

/// The screen behind the text: the panic screen's, so a panic drawn over the
/// console changes the banner and the text, not the whole screen.
const BACKGROUND: Rgb = Rgb::new(0x12, 0x12, 0x1A);
/// The text.
const TEXT: Rgb = Rgb::new(0xC8, 0xC8, 0xD2);
/// Space at the sides and the bottom, in pixels.
const MARGIN: usize = 16;
/// The share of the height kept clear at the top: a phone's camera cut-out
/// and a monitor's bezel both take the top edge.
const TOP_INSET_DIVISOR: usize = 20;
/// Milestone stripes across the top of the screen, one row each
/// ([`milestone`]), above the text.
const STRIPES: usize = 12;
/// The height of one stripe, in pixels.
const STRIPE_HEIGHT: usize = 20;
/// A stripe's colour, by milestone: neighbours differ, so a photo shows which
/// rows are lit even when the text is unreadable.
const STRIPE_COLOURS: [Rgb; 6] = [
    Rgb::new(0xE0, 0x30, 0x30),
    Rgb::new(0x30, 0xC0, 0x40),
    Rgb::new(0x30, 0x60, 0xE0),
    Rgb::new(0xE0, 0xC0, 0x20),
    Rgb::new(0xC0, 0x40, 0xC0),
    Rgb::new(0x30, 0xC0, 0xC0),
];
/// The fewest columns a larger glyph is worth. The largest scale that still
/// leaves this many is used: a phone's 1080 pixels get double-size glyphs and
/// 65 columns rather than 131 columns nobody can read.
const MIN_COLUMNS: usize = 60;
/// The largest scale tried.
const MAX_SCALE: usize = 4;
/// Attempts [`stop`] makes at the lock before it gives up waiting for a draw
/// in flight, which it does only when that draw is on its own processor.
const STOP_SPINS: u32 = 1_000_000;

/// Whether bytes are drawn. Set once by [`start`], cleared by [`stop`].
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Whether [`start`] has run: it starts the board once, never again after a
/// [`stop`].
static STARTED: AtomicBool = AtomicBool::new(false);

/// The screen and where the next glyph goes, while there is one.
static BOARD: SpinLock<Option<Board>> = SpinLock::new(None);

/// The console's screen: the surface and the cursor on it.
struct Board {
    /// The framebuffer, from `panic::screen`'s record of it.
    surface: Surface<'static>,
    /// Each font pixel is this many pixels square.
    scale: usize,
    /// Left edge of the text, in pixels.
    left: usize,
    /// Top edge of the text, in pixels.
    top: usize,
    /// Columns of text.
    columns: usize,
    /// Rows of text.
    rows: usize,
    /// The column the next glyph goes in.
    column: usize,
    /// The row it goes in.
    row: usize,
}

impl Board {
    /// A board over `surface` with the largest scale that leaves
    /// [`MIN_COLUMNS`], or `None` if not even two rows fit.
    fn new(surface: Surface<'static>) -> Option<Board> {
        let width = surface.width().saturating_sub(MARGIN.saturating_mul(2));
        let top = MARGIN
            .max(surface.height() / TOP_INSET_DIVISOR)
            .saturating_add(STRIPES.saturating_mul(STRIPE_HEIGHT));
        let height = surface.height().saturating_sub(top.saturating_add(MARGIN));
        let scale = (1..=MAX_SCALE)
            .rev()
            .find(|&scale| width / GLYPH_WIDTH.saturating_mul(scale) >= MIN_COLUMNS)
            .unwrap_or(1);
        let columns = width / GLYPH_WIDTH.saturating_mul(scale);
        let rows = height / GLYPH_HEIGHT.saturating_mul(scale);
        if columns == 0 || rows < 2 {
            return None;
        }
        Some(Board {
            surface,
            scale,
            left: MARGIN,
            top,
            columns,
            rows,
            column: 0,
            row: 0,
        })
    }

    /// Draw `byte` at the cursor and move it on.
    fn put(&mut self, byte: u8) {
        match byte {
            b'\r' => {}
            b'\n' => self.newline(),
            b'\t' => {
                for _ in 0..(8 - self.column % 8) {
                    self.glyph(' ');
                }
            }
            0x20..=0x7E => self.glyph(char::from(byte)),
            _ => self.glyph(char::REPLACEMENT_CHARACTER),
        }
    }

    /// Draw `ch` at the cursor, wrapping first if the row is full.
    fn glyph(&mut self, ch: char) {
        if self.column >= self.columns {
            self.newline();
        }
        let cell_width = GLYPH_WIDTH.saturating_mul(self.scale);
        let x = self
            .left
            .saturating_add(self.column.saturating_mul(cell_width));
        let y = self.row_top(self.row);
        self.surface
            .draw_char(x, y, ch, self.scale, TEXT, Some(BACKGROUND));
        self.column = self.column.saturating_add(1);
    }

    /// Start the next row, from the top once the bottom is reached, and blank
    /// the one after it: the gap that marks where the newest line is.
    fn newline(&mut self) {
        self.column = 0;
        self.row = (self.row + 1) % self.rows;
        self.clear_row(self.row);
        self.clear_row((self.row + 1) % self.rows);
    }

    /// Paint milestone stripe `row` and its number.
    fn stripe(&mut self, row: usize) {
        let colour = STRIPE_COLOURS
            .get(row % STRIPE_COLOURS.len())
            .copied()
            .unwrap_or(TEXT);
        let digit = |value: usize| {
            u32::try_from(value % 10)
                .ok()
                .and_then(|value| char::from_digit(value, 10))
                .unwrap_or('?')
        };
        let y = MARGIN.saturating_add(row.saturating_mul(STRIPE_HEIGHT));
        let width = self.surface.width();
        self.surface
            .fill_rect(0, y, width, STRIPE_HEIGHT.saturating_sub(2), colour);
        let tens = digit(row / 10);
        let ones = digit(row);
        self.surface
            .draw_char(MARGIN, y, tens, 1, Rgb::new(0, 0, 0), Some(colour));
        self.surface.draw_char(
            MARGIN.saturating_add(GLYPH_WIDTH),
            y,
            ones,
            1,
            Rgb::new(0, 0, 0),
            Some(colour),
        );
    }

    /// Paint `row` the background colour.
    fn clear_row(&mut self, row: usize) {
        let y = self.row_top(row);
        let width = self.surface.width();
        let height = GLYPH_HEIGHT.saturating_mul(self.scale);
        self.surface.fill_rect(0, y, width, height, BACKGROUND);
    }

    /// Where `row` begins, in pixels from the top.
    fn row_top(&self, row: usize) -> usize {
        self.top
            .saturating_add(row.saturating_mul(GLYPH_HEIGHT.saturating_mul(self.scale)))
    }
}

/// Start drawing the console, if the command line asks for it and there is a
/// framebuffer to draw on: clear the screen, and draw what the console has
/// kept of the lines printed before this.
///
/// Once, on the boot processor, as soon as `kmain` has mapped the framebuffer
/// (`map_boot_framebuffer`), before its first line and before `mm::init`: a machine with no serial port
/// shows nothing at all of a hang before the console is on the screen. The
/// lines printed before this -- the very first one on -- are drawn from what
/// the console kept. A second call does nothing.
pub(crate) fn start(view: &BootView<'_>) {
    if !view.flag(FLAG) || STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    // SAFETY: (SHARED) nothing else writes the framebuffer while the board holds it:
    // the panic screen stops the board before it draws, and the one ring-3
    // writer there can be -- `efifb`, under `ferrix.efifb` -- can map the
    // framebuffer only through the node `discovery::firmware` lends it to,
    // which stops the board before the node exists (`device::check`'s
    // `check_framebuffer` requires it stopped).
    let Some(mut surface) = (unsafe { crate::panic::screen::surface() }) else {
        return;
    };
    surface.fill(BACKGROUND);
    let Some(mut board) = Board::new(surface) else {
        return;
    };
    let mut earlier = [0_u8; 2048];
    let count = super::recent(&mut earlier);
    // From the first whole line kept.
    let kept = earlier.get(..count).unwrap_or_default();
    let from = match kept.iter().position(|&byte| byte == b'\n') {
        Some(newline) if count == earlier.len() => newline + 1,
        _ => 0,
    };
    for &byte in kept.get(from..).unwrap_or_default() {
        board.put(byte);
    }
    *BOARD.lock() = Some(board);
    ENABLED.store(true, Ordering::Release);
}

/// Light the stripe for boot milestone `row`, counted from 0 at the top, if
/// the console is on the screen.
///
/// What a photo of a machine with no serial port reads the last milestone
/// from when no text is legible or none was drawn: each stripe is a bar the
/// width of the screen in its own colour, with its number at the left. The
/// rows are fixed by the caller (`kmain`); a row past [`STRIPES`] is dropped.
pub(crate) fn milestone(row: usize) {
    if !ENABLED.load(Ordering::Acquire) || row >= STRIPES {
        return;
    }
    if let Some(mut board) = BOARD.try_lock()
        && let Some(board) = board.as_mut()
    {
        board.stripe(row);
    }
}

/// Draw `byte`, if the console is on the screen.
///
/// Called with the console's port lock held, so bytes arrive in the order the
/// log has them. A byte that finds the board busy -- only possible while
/// [`stop`] is taking it away -- is not drawn.
pub(super) fn put(byte: u8) {
    if !ENABLED.load(Ordering::Acquire) {
        return;
    }
    if let Some(mut board) = BOARD.try_lock()
        && let Some(board) = board.as_mut()
    {
        board.put(byte);
    }
}

/// Whether the console is drawing on the framebuffer: started, and not yet
/// stopped.
pub(crate) fn drawing() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Stop drawing, for good, and wait for a draw in flight to finish.
///
/// For the panic screen, which is about to draw over the whole framebuffer,
/// and for a display driver taking the screen. The wait is bounded: a draw
/// that never finishes is one this processor was part way through when it
/// panicked, and it will not be resumed.
pub(crate) fn stop() {
    if !ENABLED.swap(false, Ordering::AcqRel) {
        return;
    }
    for _ in 0..STOP_SPINS {
        if let Some(mut board) = BOARD.try_lock() {
            *board = None;
            return;
        }
        spin_loop();
    }
}
