//! The panic report, drawn on the framebuffer firmware left behind.
//!
//! Serial is where a panic is reported first, and on a machine with a cable
//! attached nothing here matters. A machine with a screen and no cable — a
//! board on a desk — shows nothing at all without this, and that is the case
//! it exists for.
//!
//! What it draws is the console's recent output, the same bytes the serial
//! port carried, which by the time this runs end with the report itself. So
//! the screen and the log cannot disagree, and there is no second copy of the
//! report to keep in step. The report is picked out from the boot log above it
//! by colour, from its `FERRIX-PANIC` line on.
//!
//! Beside the text is a QR code of the report itself, as plain text: a photo
//! of a screen is how a report leaves a machine with no cable, and a photo of
//! a code carries every character where a photo of text loses some. It holds
//! as much of the report as a code with modules of at least
//! [`MIN_MODULE_PIXELS`] can on this screen, cut at a line, from the marker
//! line down, so what is lost to a small screen is the end of the explanation
//! and never the headline.
//!
//! This is the kernel's second output device after the serial port, and it is
//! the same kind of exception `docs/ARCHITECTURE.md` makes for that one:
//! written, never read, and configured no further than firmware left it. A
//! panic draws on it once; the boot console (`console::screen`) draws the
//! kernel's lines on it before that, only when the command line asks.

use core::cell::UnsafeCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use ferrix_bootinfo::{Framebuffer, PixelFormat};
use ferrix_fbtext::{
    BYTES_PER_PIXEL, GLYPH_HEIGHT, GLYPH_WIDTH, PixelOrder, Rect, Rgb, Surface, TextArea,
};
use ferrix_qr::{MAX_VERSION, MIN_MODULES_LEN, MIN_TMP_LEN, Symbol};

use crate::{arch, console, mm};

/// Where [`install`] recorded the framebuffer: its mapping, the memory behind
/// it, and how many bytes of it are mapped.
static VIRT: AtomicU64 = AtomicU64::new(0);
/// The physical address the mapping at [`VIRT`] must still resolve to.
static PHYS: AtomicU64 = AtomicU64::new(0);
/// Bytes mapped at [`VIRT`].
static LEN: AtomicU64 = AtomicU64::new(0);
/// Visible width in pixels.
static WIDTH: AtomicU32 = AtomicU32::new(0);
/// Lines that are both visible and mapped.
static HEIGHT: AtomicU32 = AtomicU32::new(0);
/// Pixels from one line to the next.
static STRIDE: AtomicU32 = AtomicU32::new(0);
/// The pixel format, as its discriminant. Stored last, so a reader that sees
/// a format it can draw in also sees everything above.
static FORMAT: AtomicU32 = AtomicU32::new(PixelFormat::Unknown as u32);

/// The screen behind everything.
const BACKGROUND: Rgb = Rgb::new(0x12, 0x12, 0x1A);
/// The banner across the top, which is what says "panic" from across a room.
const BANNER: Rgb = Rgb::new(0xA8, 0x1C, 0x1C);
/// The boot log above the report.
const LOG_TEXT: Rgb = Rgb::new(0x8C, 0x8C, 0x96);
/// The report.
const REPORT_TEXT: Rgb = Rgb::new(0xF2, 0xF2, 0xF2);
/// Space around the edges, in pixels.
const MARGIN: usize = 16;
/// What begins the report, as `panic.rs` prints it.
const MARKER: &[u8] = b"FERRIX-PANIC";

/// Columns the text keeps before the QR code gets any width: as wide as the
/// report's lines run, so none of them wraps.
const TEXT_COLUMNS: usize = 80;

/// Light modules a reader needs around a QR code, by the specification.
const QUIET_ZONE: usize = 4;
/// The smallest module worth drawing, in pixels. One pixel does not survive
/// being photographed off a screen; two usually does.
const MIN_MODULE_PIXELS: usize = 2;
/// What the code is, under it. Short, because it is only as wide as the code,
/// and on a small screen that is about sixteen columns.
const CAPTION: &str = "report as text";

/// How much recent console output is copied out to draw.
const TEXT_BYTES: usize = 4096;

/// Everything drawing needs to write to: the recent output copied out, and the
/// QR encoder's symbol and working space.
///
/// Static rather than on the stack, because a panic may be reporting that the
/// stack is nearly gone, and together these are over 11 KiB.
struct Scratch {
    text: [u8; TEXT_BYTES],
    modules: [u8; MIN_MODULES_LEN],
    work: [u8; MIN_TMP_LEN],
}

/// The cell [`SCRATCH`] lives in.
struct ScratchCell(UnsafeCell<Scratch>);

// SAFETY: (SHARED) only `draw` touches it, and only the first panic reaches `draw`:
// `panic.rs` lets exactly one caller past its `REPORTING` swap, and every
// later report halts before drawing. There is never a second accessor.
unsafe impl Sync for ScratchCell {}

/// The one set of scratch buffers.
static SCRATCH: ScratchCell = ScratchCell(UnsafeCell::new(Scratch {
    text: [0; TEXT_BYTES],
    modules: [0; MIN_MODULES_LEN],
    work: [0; MIN_TMP_LEN],
}));

/// Record the framebuffer boot mapped, for a panic to draw on.
///
/// Only for a mapping the caller has just made and checked resolves to
/// `framebuffer.phys`: before `mm::init`, [`surface`] trusts it unchecked.
/// `mapped` bytes of `framebuffer` are mapped at `virt`. A format this kernel
/// cannot draw in records nothing, and so does a mapping too small for a
/// single line.
pub fn install(framebuffer: &Framebuffer, virt: u64, mapped: u64) {
    if framebuffer.format == PixelFormat::Unknown {
        return;
    }
    let line = u64::from(framebuffer.stride).saturating_mul(BYTES_PER_PIXEL as u64);
    let lines = mapped.checked_div(line).unwrap_or(0);
    let height = u32::try_from(lines)
        .unwrap_or(u32::MAX)
        .min(framebuffer.height);
    if height == 0 {
        return;
    }
    VIRT.store(virt, Ordering::Relaxed);
    PHYS.store(framebuffer.phys, Ordering::Relaxed);
    LEN.store(mapped, Ordering::Relaxed);
    WIDTH.store(framebuffer.width, Ordering::Relaxed);
    HEIGHT.store(height, Ordering::Relaxed);
    STRIDE.store(framebuffer.stride, Ordering::Relaxed);
    FORMAT.store(framebuffer.format as u32, Ordering::Release);
}

/// Draw the panic screen, if there is a framebuffer to draw it on.
///
/// For the one panic that got past `REPORTING`, after it has printed its
/// report: what is drawn is the console's recent output, which by then ends
/// with that report.
pub(crate) fn draw() {
    // The boot console may be part way through a line on this screen; it
    // draws nothing from here on.
    console::screen::stop();
    // SAFETY: (SHARED) nothing else writes the framebuffer during a panic: the other
    // processors have been asked to stop, the boot console has just stopped,
    // and this is the only report that draws. A ring-3 driver the framebuffer
    // is lent to (`efifb`, under `ferrix.efifb`) writes it only from a
    // processor, and every other processor has been stopped; a store of its
    // in flight on one that never stops can only leave pixels of its own
    // frame on the screen, never touch what the panic draws with.
    let Some(mut surface) = (unsafe { surface() }) else {
        return;
    };
    // SAFETY: (SHARED) the one accessor, as the `Sync` impl for `ScratchCell` argues.
    let scratch = unsafe { &mut *SCRATCH.0.get() };
    let count = console::recent(&mut scratch.text);
    let text = scratch.text.get(..count).unwrap_or_default();
    paint(&mut surface, text, &mut scratch.modules, &mut scratch.work);
    // The window is write-combining on x86-64 from its first map
    // (`arch::framebuffer_flags`), lent or not: the picture's last stores
    // leave the fill buffers before this processor halts.
    arch::drain_write_combining();
}

/// The framebuffer [`install`] recorded, as a surface to draw on: `None` if
/// there is none, it is in a format this kernel cannot draw in, or it is no
/// longer mapped where boot left it.
///
/// # Safety
///
/// (SHARED) Nothing else may write the framebuffer while the surface lives. There are
/// two callers: [`draw`], during the one panic that draws, and the boot
/// console (`console::screen`), which [`draw`] stops first. The framebuffer
/// lent to a ring-3 driver under `ferrix.efifb` is written by that driver
/// too: the console is stopped before it can be (`discovery::firmware`), and
/// [`draw`] runs with the other processors stopped.
pub(crate) unsafe fn surface() -> Option<Surface<'static>> {
    let order = match FORMAT.load(Ordering::Acquire) {
        format if format == PixelFormat::Bgrx8888 as u32 => PixelOrder::Bgrx,
        format if format == PixelFormat::Rgbx8888 as u32 => PixelOrder::Rgbx,
        _ => return None,
    };
    let virt = VIRT.load(Ordering::Relaxed);
    // Still mapped where boot left it, and to the same memory. Nothing since
    // has had a reason to change that, and a panic is not the moment to
    // assume it.
    //
    // Before `mm::init` the root table is not known to `mm`, and neither is
    // the direct map a walk reads tables through, so a walk would fault with
    // nowhere to go. That early the mapping is trusted as `install` recorded
    // it: its one caller (`check_early_mapper`) records only a mapping it has
    // just made and translated back to the framebuffer through the live
    // tables, on the only processor running, and nothing between that and
    // `mm::init` unmaps or remaps the framebuffer window. This is what lets a
    // machine with no serial port show a hang or panic in early boot.
    let root = mm::root_table();
    if root != 0 && mm::translate_in(root, virt) != Some(PHYS.load(Ordering::Relaxed)) {
        return None;
    }
    let (Ok(address), Ok(len), Ok(width), Ok(height), Ok(stride)) = (
        usize::try_from(virt),
        usize::try_from(LEN.load(Ordering::Relaxed)),
        usize::try_from(WIDTH.load(Ordering::Relaxed)),
        usize::try_from(HEIGHT.load(Ordering::Relaxed)),
        usize::try_from(STRIDE.load(Ordering::Relaxed)),
    ) else {
        return None;
    };

    // SAFETY: (DEVICE) `install` recorded `len` bytes mapped at `address`, and the
    // check above shows the mapping still stands; the caller is the only
    // writer.
    let pixels = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, len) };
    Surface::new(pixels, width, height, stride, order)
}

/// Paint the banner, the QR code of the report at the right, and as much of
/// `text` as fits beside it, newest last.
fn paint(surface: &mut Surface<'_>, text: &[u8], modules: &mut [u8], work: &mut [u8]) {
    surface.fill(BACKGROUND);
    let width = surface.width();
    let height = surface.height();
    let inner = width.saturating_sub(MARGIN.saturating_mul(2));
    let banner = GLYPH_HEIGHT.saturating_mul(2).saturating_add(MARGIN);
    surface.fill_rect(0, 0, width, banner, BANNER);

    let title = Rect {
        x: MARGIN,
        y: MARGIN / 2,
        width: inner,
        height: GLYPH_HEIGHT.saturating_mul(2),
    };
    let _ = TextArea::new(surface, title, 2, Rgb::WHITE, None)
        .write_str("Ferrix kernel panic - this processor has stopped");

    let top = banner.saturating_add(MARGIN);
    let tall = height.saturating_sub(banner.saturating_add(MARGIN.saturating_mul(2)));

    // The code gets what is left once the text has its columns, and never more
    // than half the width: a small screen gets a small code rather than a
    // report whose every line wraps.
    let text_wanted = TEXT_COLUMNS
        .saturating_mul(GLYPH_WIDTH)
        .saturating_add(MARGIN);
    let side = tall.min(inner.saturating_sub(text_wanted)).min(inner / 2);
    let report = report_start(text).and_then(|start| text.get(start..));
    let code = report.and_then(|report| {
        let right = MARGIN.saturating_add(inner);
        draw_code(surface, right, top, side, report, modules, work)
    });
    let beside = code.map_or(0, |drawn| drawn.saturating_add(MARGIN));

    let body = Rect {
        x: MARGIN,
        y: top,
        width: inner.saturating_sub(beside),
        height: tall,
    };
    let mut area = TextArea::new(surface, body, 1, LOG_TEXT, None);
    let shown = visible(text, area.rows(), area.columns());
    let report = report_start(shown);
    for (offset, &byte) in shown.iter().enumerate() {
        if Some(offset) == report {
            area.set_color(REPORT_TEXT);
        }
        match byte {
            b'\r' => {}
            b'\n' => area.newline(),
            0x20..=0x7E => area.put_char(char::from(byte)),
            _ => area.put_char(char::REPLACEMENT_CHARACTER),
        }
    }
}

/// Draw a QR code of as much of `report` as fits in a square of `side` pixels
/// whose top right corner is at `right`, `top`, with its caption below, and
/// return the side of the square drawn: `None` if no code worth scanning fits.
fn draw_code(
    surface: &mut Surface<'_>,
    right: usize,
    top: usize,
    side: usize,
    report: &[u8],
    modules: &mut [u8],
    work: &mut [u8],
) -> Option<usize> {
    let symbol = encode(report, side, modules, work)?;
    let span = symbol.width().saturating_add(QUIET_ZONE.saturating_mul(2));
    let scale = side.checked_div(span)?;
    if scale < MIN_MODULE_PIXELS {
        return None;
    }
    let drawn = span.saturating_mul(scale);
    let left = right.checked_sub(drawn)?;

    surface.fill_rect(left, top, drawn, drawn, Rgb::WHITE);
    let origin = |module: usize| module.saturating_add(QUIET_ZONE).saturating_mul(scale);
    for y in 0..symbol.width() {
        for x in 0..symbol.width() {
            if symbol.is_dark(x, y) {
                let at_x = left.saturating_add(origin(x));
                let at_y = top.saturating_add(origin(y));
                surface.fill_rect(at_x, at_y, scale, scale, Rgb::BLACK);
            }
        }
    }

    let caption = Rect {
        x: left,
        y: top.saturating_add(drawn).saturating_add(GLYPH_HEIGHT / 2),
        width: drawn,
        height: GLYPH_HEIGHT,
    };
    let _ = TextArea::new(surface, caption, 1, LOG_TEXT, None).write_str(CAPTION);
    Some(drawn)
}

/// Encode as much of `report` as a code no wider than `side` pixels can carry
/// with modules of [`MIN_MODULE_PIXELS`], cut at the end of a line.
fn encode<'m>(
    report: &[u8],
    side: usize,
    modules: &'m mut [u8],
    work: &mut [u8],
) -> Option<Symbol<'m>> {
    let across = (side / MIN_MODULE_PIXELS).checked_sub(QUIET_ZONE.saturating_mul(2))?;
    // A symbol is 17 + 4 * version modules across.
    let version = u8::try_from(across.checked_sub(17)? / 4)
        .unwrap_or(MAX_VERSION)
        .min(MAX_VERSION);
    if version == 0 {
        return None;
    }
    let room = ferrix_qr::max_data_size(version, 0);
    let payload = if report.len() <= room {
        report
    } else {
        let fits = report.get(..room)?;
        let line = fits
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(room, |end| end + 1);
        fits.get(..line)?
    };
    ferrix_qr::generate(None, payload, modules, work).ok()
}

/// What of `text` to show in `rows` rows of `columns` columns.
///
/// The end of it, so the report sits under as much of the boot log as fits,
/// unless the report alone does not fit. Then it is the report from its first
/// line, cut at the bottom: the headline, the location and the trace matter more
/// than the last lines of the explanation, which the serial log and the QR code
/// still carry.
fn visible(text: &[u8], rows: usize, columns: usize) -> &[u8] {
    let shown = tail(text, rows, columns);
    match report_start(text) {
        Some(start) if report_start(shown).is_none() => text.get(start..).unwrap_or(shown),
        _ => shown,
    }
}

/// The end of `text` that fits in `rows` rows of `columns` columns, starting
/// at the beginning of a line.
///
/// Counted from the end, because the newest lines are the report and they are
/// the ones that must be on the screen; the boot log above fills whatever is
/// left.
fn tail(text: &[u8], rows: usize, columns: usize) -> &[u8] {
    let columns = columns.max(1);
    // Output normally ends with a newline, which begins no line of its own.
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    let mut used = 0_usize;
    let mut end = body.len();
    let mut from = text.len();
    for line in body.rsplit(|&byte| byte == b'\n') {
        let visible = line.iter().filter(|&&byte| byte != b'\r').count();
        let needs = visible.div_ceil(columns).max(1);
        if used.saturating_add(needs) > rows {
            break;
        }
        used = used.saturating_add(needs);
        from = end.saturating_sub(line.len());
        end = from.saturating_sub(1);
    }
    text.get(from..).unwrap_or_default()
}

/// Where the line holding the last report marker in `text` begins.
fn report_start(text: &[u8]) -> Option<usize> {
    let marker = text
        .windows(MARKER.len())
        .rposition(|window| window == MARKER)?;
    let before = text.get(..marker)?;
    Some(
        before
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1),
    )
}

/// The kernel's window on the framebuffer, as [`install`] recorded it: its
/// address and the bytes mapped, or `None` when there is none. What
/// `discovery::firmware` reads back write-combining when it lends the
/// framebuffer, and stage 9 on every x86-64 boot.
pub fn window() -> Option<(u64, u64)> {
    let virt = VIRT.load(Ordering::Acquire);
    let len = LEN.load(Ordering::Acquire);
    (virt != 0 && len != 0).then_some((virt, len))
}
