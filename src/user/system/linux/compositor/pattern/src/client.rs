//! The Wayland client: connect, bind, make a window, draw.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use compositor_protocol::core::{
    self, wl_compositor, wl_display, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use compositor_protocol::layer_shell::{self, zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use compositor_protocol::xdg_decoration::{
    self, zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use compositor_protocol::xdg_shell::{
    self, xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use compositor_render::Pattern;
use compositor_socket::{Connection, RecvError, socket_path};
use compositor_wire::{Arg, ArgType, Fd, Interface, ObjectId, Reader, Writer};

use compositor_shm::Shared;

use crate::av1::{Decoder, Frame, demux_ivf};

/// The objects this client makes, at fixed ids. A client may name its own
/// objects however it likes as long as it does not reuse one, and fixed
/// numbers make the code and its log readable.
mod id {
    use compositor_wire::ObjectId;

    pub(super) const DISPLAY: ObjectId = ObjectId(1);
    pub(super) const REGISTRY: ObjectId = ObjectId(2);
    pub(super) const SYNC: ObjectId = ObjectId(3);
    pub(super) const COMPOSITOR: ObjectId = ObjectId(4);
    pub(super) const SHM: ObjectId = ObjectId(5);
    pub(super) const SHELL: ObjectId = ObjectId(6);
    pub(super) const SURFACE: ObjectId = ObjectId(7);
    pub(super) const XDG_SURFACE: ObjectId = ObjectId(8);
    pub(super) const TOPLEVEL: ObjectId = ObjectId(9);
    pub(super) const POOL: ObjectId = ObjectId(10);
    pub(super) const BUFFER: ObjectId = ObjectId(11);
    pub(super) const LAYER_SHELL: ObjectId = ObjectId(15);
    pub(super) const LAYER_SURFACE: ObjectId = ObjectId(16);
    pub(super) const SEAT: ObjectId = ObjectId(12);
    pub(super) const OUTPUT: ObjectId = ObjectId(17);
    pub(super) const KEYBOARD: ObjectId = ObjectId(13);
    pub(super) const POINTER: ObjectId = ObjectId(14);
    pub(super) const DECORATIONS: ObjectId = ObjectId(18);
    pub(super) const DECORATION: ObjectId = ObjectId(19);
    pub(super) const POSITIONER: ObjectId = ObjectId(20);
    pub(super) const POPUP_SURFACE: ObjectId = ObjectId(21);
    pub(super) const POPUP_XDG: ObjectId = ObjectId(22);
    pub(super) const POPUP: ObjectId = ObjectId(23);
    pub(super) const POPUP_POOL: ObjectId = ObjectId(24);
    pub(super) const POPUP_BUFFER: ObjectId = ObjectId(25);
    /// The second buffer, which only a wallpaper that moves keeps: a client
    /// that draws again before the compositor has let go of the last frame
    /// needs somewhere else to draw it.
    pub(super) const BUFFER_TWO: ObjectId = ObjectId(26);
    /// The frame callback a wallpaper that moves asks for. One id, asked for
    /// again only once its `done` has come: a `wl_callback` is destroyed by
    /// the event, so the number is the client's again by then.
    pub(super) const FRAME: ObjectId = ObjectId(27);
    /// The extra window a [`Shape::Twin`] client opens and then destroys: a
    /// second `xdg_toplevel` on the same connection, with memory of its own
    /// because its size is not the first window's.
    pub(super) const TWIN_SURFACE: ObjectId = ObjectId(28);
    pub(super) const TWIN_XDG: ObjectId = ObjectId(29);
    pub(super) const TWIN_TOPLEVEL: ObjectId = ObjectId(30);
    pub(super) const TWIN_POOL: ObjectId = ObjectId(31);
    pub(super) const TWIN_BUFFER: ObjectId = ObjectId(32);
}

/// The buffers a client may keep, in the order it fills them.
const BUFFERS: [ObjectId; 2] = [id::BUFFER, id::BUFFER_TWO];

/// How many bands of damage are worth sending before the screen is cheaper.
///
/// Each is five words on the wire and a region the compositor intersects,
/// clips and blurs on its own; a frame that changed in sixty-four places
/// changed, and saying so as one rectangle is less work for both ends.
const MOST_BANDS: usize = 64;

/// How long a wallpaper that moves waits for the compositor to say it drew
/// the last frame before drawing the next anyway.
///
/// Long enough that a wallpaper behind a full-screen window really does stop
/// -- that is the point of waiting at all -- and short enough that a
/// compositor which answers no frame callbacks is a slow wallpaper rather
/// than a stopped one.
const UNPACED: Duration = Duration::from_secs(5);

/// How long to run before giving up, so a test can never hang.
///
/// A real client runs until it is closed; this one runs until the compositor
/// goes away, which closes the socket, or until this passes. Long enough that
/// it is never what ends a test: it is a window under a compositor's control,
/// and a window that vanished on its own would look like a compositor that
/// closed it.
const DEADLINE: Duration = Duration::from_secs(600);

/// How long [`Shape::Twin`]'s extra window stays up before the client
/// destroys it.
///
/// It has to be up long enough to have been *tiled*: closing a window the
/// compositor has drawn once is a different claim from closing one it never
/// laid out, and only the first is the bug this shape is written for.
/// Long enough, too, for a test to see both windows in a taskbar's list
/// before one of them goes.
const TWIN_LINGER: Duration = Duration::from_millis(2000);

/// What this client asks the compositor to make of its surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Shape {
    /// An `xdg_toplevel`: a window the layout tiles.
    #[default]
    Window,
    /// A `zwlr_layer_surface_v1` on the `top` layer, anchored across the top
    /// edge, this many pixels tall and reserving all of them: what a bar
    /// asks for.
    Bar(u32),
    /// A window with an `xdg_popup` on it, this many pixels square, hanging
    /// off the window's top-left corner: what a menu is.
    Menu(u32),
    /// A [`Shape::Bar`] this many pixels tall with an `xdg_popup` the
    /// second number square hanging off its top-left corner, taken with
    /// `zwlr_layer_surface_v1.get_popup`: what a bar's tooltip or a tray
    /// icon's menu is.
    BarMenu(u32, u32),
    /// Two windows on one connection: the pattern's own, and a second
    /// `xdg_toplevel` opened once the first has drawn and destroyed once it
    /// has drawn in its turn -- a document window and a dialog that is
    /// dismissed.
    ///
    /// The connection stays open throughout, so the only thing that can take
    /// the closed window out of the layout is the `xdg_toplevel.destroy`
    /// itself. That is what this shape is for: until 2026-09-18 only a whole
    /// connection going did, and a client that closed one of its two windows
    /// left the compositor tiling a window that was not there.
    Twin,
    /// A `zwlr_layer_surface_v1` on the `background` layer, anchored to all
    /// four edges and reserving nothing: what a wallpaper asks for. It draws
    /// the [`Picture`] it was given rather than a pattern.
    Wallpaper,
}

/// A picture a wallpaper shows: `XRGB8888` rows with no padding.
///
/// Read from a file that is those rows behind a twelve-byte header -- the
/// eight bytes [`Picture::MAGIC`], then the width and the height as
/// little-endian `u32`s. Not a format anybody else has: decoding a JPEG is
/// a library this client has no other use for, and whoever puts the file
/// on the image (`cargo xtask run-compositor`) has a whole machine to
/// convert one with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Picture {
    /// What a picture's file begins with.
    pub const MAGIC: &'static [u8; 8] = b"FXWALL1\n";

    /// A picture from its file's bytes.
    ///
    /// # Errors
    ///
    /// A sentence saying what is wrong with them.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let (magic, rest) = bytes
            .split_at_checked(Self::MAGIC.len())
            .ok_or("a picture shorter than its header")?;
        if magic != Self::MAGIC {
            return Err("not a picture: the file does not begin FXWALL1".to_owned());
        }
        let number = |at: usize| -> Option<u32> {
            let field = rest.get(at..at.checked_add(4)?)?;
            Some(u32::from_le_bytes(field.try_into().ok()?))
        };
        let (width, height) = number(0)
            .zip(number(4))
            .ok_or("a picture shorter than its header")?;
        let len = usize::try_from(u64::from(width) * u64::from(height) * 4)
            .map_err(|_| "a picture too large to hold".to_owned())?;
        let pixels = rest
            .get(8..)
            .filter(|pixels| width > 0 && height > 0 && pixels.len() == len)
            .ok_or_else(|| {
                format!("a {width}x{height} picture whose pixels are not {len} bytes")
            })?;
        Ok(Self {
            width,
            height,
            pixels: pixels.to_vec(),
        })
    }

    /// The picture as a `width` by `height` buffer: scaled until it covers
    /// the whole of it, the same amount each way, and cut evenly on the two
    /// sides that overflow. What `swaybg -m fill` and `hyprpaper`'s `cover`
    /// do, with the nearest pixel rather than a filter: a picture made for
    /// the screen it is shown on is copied, and that is the case this is
    /// for.
    #[must_use]
    pub fn cover(&self, width: u32, height: u32) -> Vec<u8> {
        if (width, height) == (self.width, self.height) {
            return self.pixels.clone();
        }
        let (to_w, to_h) = (u64::from(width.max(1)), u64::from(height.max(1)));
        let mut out = vec![0; usize::try_from(to_w * to_h * 4).unwrap_or(0)];
        self.cover_rows_into(
            width,
            height,
            &[(0, i32::try_from(to_h).unwrap_or(i32::MAX))],
            &mut out,
        );
        out
    }

    /// Scale the rows `bands` names -- each a `(top, height)` of the buffer
    /// -- of a `width` by `height` cover of this picture into `out`, which
    /// holds such a cover's rows tightly; the rest of `out` is left alone.
    ///
    /// The pixel each row takes is the one [`Picture::cover`] takes, so
    /// covering every row this way is the same bytes as `cover`. What
    /// differs is the cost: which source pixel a column takes is the same
    /// for every row and is worked out once rather than as a division a
    /// pixel, and a row the scale takes from the same source row as the
    /// row above it is a copy of that row. A wallpaper that moves covers
    /// only the rows a frame changed, which is what makes a frame of it
    /// cheaper than the screen.
    pub fn cover_rows_into(&self, width: u32, height: u32, bands: &[(i32, i32)], out: &mut [u8]) {
        let from_w = u64::from(self.width);
        let (to_w, to_h) = (u64::from(width.max(1)), u64::from(height.max(1)));
        let stride = usize::try_from(to_w * 4).unwrap_or(usize::MAX);
        let source_stride = usize::try_from(from_w * 4).unwrap_or(usize::MAX);
        let cut = self.cut(width, height);
        // Which byte of a source row each pixel of a row begins at. Empty
        // when the buffer is the picture's own size, where a row is a copy.
        let columns: Vec<usize> = if (width, height) == (self.width, self.height) {
            Vec::new()
        } else {
            (0..to_w)
                .map(|x| {
                    usize::try_from((cut.left + x * cut.part_w / to_w) * 4).unwrap_or(usize::MAX)
                })
                .collect()
        };
        for &(top, tall) in bands {
            let first = u64::try_from(top.max(0)).unwrap_or(0).min(to_h);
            let end = u64::try_from(top.max(0).saturating_add(tall.max(0)))
                .unwrap_or(0)
                .min(to_h);
            // The source row the row above took, and where that row is.
            let mut last: Option<(u64, usize)> = None;
            for y in first..end {
                let source_row = cut.row(y);
                let out_at = usize::try_from(y)
                    .unwrap_or(usize::MAX)
                    .saturating_mul(stride);
                let Some(out_end) = out_at.checked_add(stride).filter(|end| *end <= out.len())
                else {
                    break;
                };
                if let Some((row, previous)) = last
                    && row == source_row
                    && previous.saturating_add(stride) <= out_at
                {
                    out.copy_within(previous..previous.saturating_add(stride), out_at);
                    last = Some((source_row, out_at));
                    continue;
                }
                let base = usize::try_from(source_row * from_w * 4).unwrap_or(usize::MAX);
                let source = base
                    .checked_add(source_stride)
                    .and_then(|end| self.pixels.get(base..end))
                    .unwrap_or(&[]);
                let Some(target) = out.get_mut(out_at..out_end) else {
                    break;
                };
                scale_row(target, source, &columns);
                last = Some((source_row, out_at));
            }
        }
    }

    /// Which part of the picture a `width` by `height` buffer shows, and
    /// where in the picture it starts.
    ///
    /// The arithmetic [`Picture::cover`] scales by, in one place because
    /// [`Movie`]'s damage has to agree with it exactly: a row said to have
    /// changed that the scale took its pixels from somewhere else is a row
    /// the compositor will not redraw.
    fn cut(&self, width: u32, height: u32) -> Cut {
        let (from_w, from_h) = (u64::from(self.width), u64::from(self.height));
        let (to_w, to_h) = (u64::from(width.max(1)), u64::from(height.max(1)));
        // The part of the picture that has the buffer's shape: all of one
        // direction, and the middle of the other.
        let (part_w, part_h) = if from_w * to_h > from_h * to_w {
            ((from_h * to_w / to_h).max(1), from_h)
        } else {
            (from_w, (from_w * to_h / to_w).max(1))
        };
        Cut {
            part_w,
            part_h,
            left: (from_w - part_w) / 2,
            top: (from_h - part_h) / 2,
            to_h,
        }
    }

    /// The buffer rows a `width` by `height` cover takes from the picture
    /// rows `rows` marks, as `(top, height)` spans of the buffer.
    ///
    /// A picture smaller than the buffer has each of its rows stretched over
    /// several, so one changed row is a band; a picture larger has rows the
    /// scale skips, which are in no span at all.
    #[must_use]
    fn cover_rows(&self, width: u32, height: u32, rows: &[bool]) -> Vec<(i32, i32)> {
        if (width, height) == (self.width, self.height) {
            return spans(rows.iter().copied(), rows.len());
        }
        let cut = self.cut(width, height);
        let taken = (0..cut.to_h).map(|y| {
            usize::try_from(cut.row(y))
                .ok()
                .and_then(|row| rows.get(row).copied())
                .unwrap_or(true)
        });
        spans(taken, usize::try_from(cut.to_h).unwrap_or(0))
    }
}

/// One row of a cover: `target` takes the pixel of `source` each of
/// `columns` begins at, or the whole of `source` when there are none, which
/// is a buffer the picture's own width.
fn scale_row(target: &mut [u8], source: &[u8], columns: &[usize]) {
    if columns.is_empty() {
        if source.len() == target.len() {
            target.copy_from_slice(source);
        }
        return;
    }
    for (pixel, &column) in target.chunks_exact_mut(4).zip(columns) {
        let taken = column
            .checked_add(4)
            .and_then(|end| source.get(column..end))
            .unwrap_or(&[0, 0, 0, 0xFF]);
        pixel.copy_from_slice(taken);
    }
}

/// Where a buffer's pixels come from in the picture it covers.
#[derive(Clone, Copy, Debug)]
struct Cut {
    part_w: u64,
    part_h: u64,
    left: u64,
    top: u64,
    to_h: u64,
}

impl Cut {
    /// The picture row buffer row `y` takes its pixels from.
    fn row(&self, y: u64) -> u64 {
        self.top + y * self.part_h / self.to_h.max(1)
    }
}

/// Mark the rows `bands` names -- each a `(top, height)` -- stale.
fn mark_stale(stale: &mut [bool], bands: &[(i32, i32)]) {
    for &(top, tall) in bands {
        let first = usize::try_from(top.max(0)).unwrap_or(usize::MAX);
        let end = usize::try_from(top.max(0).saturating_add(tall.max(0))).unwrap_or(usize::MAX);
        for flag in stale.iter_mut().take(end).skip(first) {
            *flag = true;
        }
    }
}

/// Copy every stale row of `scaled`, `row_bytes` each, into `room`, and mark
/// them fresh.
fn copy_stale(scaled: &[u8], stale: &mut [bool], room: &mut [u8], row_bytes: usize) {
    for (top, tall) in spans(stale.iter().copied(), stale.len()) {
        let from = usize::try_from(top).unwrap_or(0).saturating_mul(row_bytes);
        let to = from.saturating_add(usize::try_from(tall).unwrap_or(0).saturating_mul(row_bytes));
        if let (Some(source), Some(target)) = (scaled.get(from..to), room.get_mut(from..to)) {
            target.copy_from_slice(source);
        }
    }
    stale.fill(false);
}

/// The runs of `true` in `marked`, as `(start, length)`.
fn spans(marked: impl Iterator<Item = bool>, len: usize) -> Vec<(i32, i32)> {
    let mut spans: Vec<(i32, i32)> = Vec::new();
    let mut start: Option<usize> = None;
    let end = len.checked_add(1).unwrap_or(len);
    for (at, is) in marked.chain(std::iter::once(false)).enumerate().take(end) {
        match (is, start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                let height = at.saturating_sub(from);
                spans.push((
                    i32::try_from(from).unwrap_or(0),
                    i32::try_from(height).unwrap_or(0),
                ));
                start = None;
            }
            _ => {}
        }
    }
    spans
}

/// A wallpaper that moves: its compressed AV1 temporal units, a live decoder,
/// and the picture it most recently produced.
///
/// The file is IVF: its `DKIF` header says AV1 and the time base, followed by
/// length-prefixed temporal units. Keeping those units rather than raw pixels
/// is what makes a useful clip fit in the initramfs. A frame is decoded only
/// when it is due, so the guest holds one decoded frame and the decoder's
/// reference pictures rather than every frame of the clip.
pub struct Movie {
    /// Every AV1 temporal unit, still compressed.
    packets: Vec<Vec<u8>>,
    /// The decoder, which has seen packets through `at`.
    decoder: Decoder,
    /// How long one frame is shown.
    period: Duration,
    /// Which packet made `shown`.
    at: usize,
    /// The frame last decoded, which the next one is a difference against.
    shown: Picture,
    /// Which rows differ from the preceding decoded frame. This is the
    /// damage: the compositor redraws the blur under those rows alone.
    changed: Vec<bool>,
}

impl std::fmt::Debug for Movie {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Movie")
            .field("packets", &self.packets.len())
            .field("period", &self.period)
            .field("at", &self.at)
            .field("shown", &self.shown)
            .field("changed", &self.changed)
            .finish_non_exhaustive()
    }
}

impl Movie {
    /// A video from its file's bytes, with its first frame shown.
    ///
    /// The stream is decoded once before the desktop starts, so malformed AV1
    /// is reported at startup rather than leaving a wallpaper stuck half way
    /// through its loop. Those decoded pixels are then discarded; playback
    /// opens a fresh decoder and keeps only its current picture.
    ///
    /// # Errors
    ///
    /// A sentence saying what is wrong with them.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let ivf = demux_ivf(bytes)?;
        let period = Duration::from_millis(1_000 / u64::from(ivf.rate));

        // Decode every temporal unit now.  A packet can depend on the ones
        // before it, so one decoder verifies the stream in playback order.
        let mut checked = Decoder::new()?;
        for packet in &ivf.packets {
            let frame = decode(&mut checked, packet)?;
            if (frame.width, frame.height) != (ivf.width, ivf.height) {
                return Err("a frame whose size differs from its IVF header".to_owned());
            }
        }

        let mut decoder = Decoder::new()?;
        let first = decode(
            &mut decoder,
            ivf.packets.first().ok_or("a video with no frames")?,
        )?;
        let rows = usize::try_from(first.height).map_err(|_| "a video too tall to hold")?;
        Ok(Self {
            packets: ivf.packets,
            decoder,
            period,
            at: 0,
            shown: picture(first),
            changed: vec![true; rows],
        })
    }

    /// How long one frame is shown.
    #[must_use]
    pub fn period(&self) -> Duration {
        self.period
    }

    /// How many frames there are.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.packets.len()
    }

    /// The frame being shown, which is a [`Picture`] and scales like one.
    #[must_use]
    pub fn shown(&self) -> &Picture {
        &self.shown
    }

    /// The `(top, height)` bands of a `width` by `height` buffer that this
    /// frame changed, for `wl_surface.damage_buffer`.
    ///
    /// This is the whole point of the `0x02` row. A compositor told that a
    /// wallpaper changed everywhere owes the blur of everything behind every
    /// translucent window on the screen; told that forty rows changed, it
    /// owes the blur of forty rows.
    #[must_use]
    pub fn damage(&self, width: u32, height: u32) -> Vec<(i32, i32)> {
        self.shown.cover_rows(width, height, &self.changed)
    }

    /// Show the next frame, beginning again after the last.
    ///
    /// Nothing here can fail: [`Movie::parse`] decoded every packet once.
    pub fn advance(&mut self) {
        let next = match self.at.checked_add(1) {
            Some(next) if next < self.packets.len() => next,
            _ => 0,
        };
        // AV1 reference pictures belong to one pass through the stream. A
        // loop starts a fresh decoder, then its first keyframe, exactly as a
        // player reopening an IVF file would.
        if next == 0 {
            let Ok(decoder) = Decoder::new() else {
                return;
            };
            self.decoder = decoder;
        }
        let Some(packet) = self.packets.get(next) else {
            return;
        };
        let Ok(frame) = decode(&mut self.decoder, packet) else {
            return;
        };
        self.play(next, frame);
    }

    /// Replace the picture and mark every source row whose decoded bytes
    /// differ. AV1's motion compensation often leaves large regions exactly
    /// unchanged; preserving that fact keeps the compositor's blur bounded.
    fn play(&mut self, index: usize, frame: Frame) {
        let next = picture(frame);
        let stride = usize::try_from(u64::from(next.width) * 4).unwrap_or(0);
        self.changed = next
            .pixels
            .chunks(stride.max(1))
            .zip(self.shown.pixels.chunks(stride.max(1)))
            .map(|(next, previous)| next != previous)
            .collect();
        self.shown = next;
        self.at = index;
    }
}

/// Feed one temporal unit and take the picture it makes. The decoder uses a
/// one-frame delay, so the configured single-threaded stream produces one
/// picture per unit; a different stream is rejected before playback starts.
fn decode(decoder: &mut Decoder, packet: &[u8]) -> Result<Frame, String> {
    decoder.feed(packet)?;
    decoder
        .take()
        .ok_or("a video packet that made no picture".to_owned())
}

/// An AV1 frame as the picture the Wayland client draws.
fn picture(frame: Frame) -> Picture {
    Picture {
        width: frame.width,
        height: frame.height,
        pixels: frame.pixels,
    }
}

/// Where this process says its window is on the screen: a file made once
/// the compositor has drawn a frame with the window in it, which is when it
/// has been placed. Set by the program, never by a client run in a thread,
/// so a compositor's own tests leave nothing behind.
static DRAWN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Say, by making `path` once the window's first frame has been drawn, that
/// the window is placed: what `pattern --after` waits for.
pub fn announce_when_drawn(path: std::path::PathBuf) {
    let _ = DRAWN.set(path);
}

/// Connect, make a window, draw `pattern` in it, and keep drawing until the
/// compositor goes away.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run(pattern: Pattern, title: &str) -> Result<String, String> {
    let display =
        std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set".to_owned())?;
    let path = socket_path(&display).map_err(|error| format!("the socket: {error}"))?;
    run_on(&path, pattern, title)
}

/// The same, as a bar rather than a window.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run_shaped(pattern: Pattern, title: &str, shape: Shape) -> Result<String, String> {
    let display =
        std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set".to_owned())?;
    let path = socket_path(&display).map_err(|error| format!("the socket: {error}"))?;
    run_shaped_on(&path, pattern, title, shape)
}

/// The same, on a socket named directly rather than through the environment.
///
/// The environment is one per process, so two clients running in one
/// process -- which is how the compositor's own test runs them -- cannot each
/// set `WAYLAND_DISPLAY`.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run_on(path: &std::path::Path, pattern: Pattern, title: &str) -> Result<String, String> {
    run_shaped_on(path, pattern, title, Shape::Window)
}

/// The same, with the surface given the role `shape` names.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run_shaped_on(
    path: &std::path::Path,
    pattern: Pattern,
    title: &str,
    shape: Shape,
) -> Result<String, String> {
    run_with(path, pattern, title, shape, Shown::Pattern, None)
}

/// The same, making `drawn` once the compositor has drawn a frame with the
/// window in it, as [`announce_when_drawn`] does for a whole program.
///
/// Several clients in one process each say it for themselves: a test that
/// starts them in turn waits on one's file before starting the next, so they
/// are placed in the order it wants however slow the host is.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run_announced_on(
    path: &std::path::Path,
    pattern: Pattern,
    title: &str,
    shape: Shape,
    drawn: std::path::PathBuf,
) -> Result<String, String> {
    run_with(path, pattern, title, shape, Shown::Pattern, Some(drawn))
}

/// Connect to the compositor the environment names and be its wallpaper:
/// `picture`, behind everything, until the compositor goes away.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run_wallpaper(picture: Picture) -> Result<String, String> {
    be_wallpaper(Shown::Still(picture))
}

/// The same, with a wallpaper that moves: `movie`'s frames in turn, beginning
/// again after the last, until the compositor goes away.
///
/// What `mpvpaper` is for on a Linux desktop, and the same surface it asks
/// for -- the difference is where the decoding happened, which for a guest
/// with no decoder was `cargo xtask wallpapers`, on a machine that has one.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run_video(movie: Movie) -> Result<String, String> {
    be_wallpaper(Shown::Moving(movie))
}

/// Be the wallpaper the compositor the environment names has.
fn be_wallpaper(shown: Shown) -> Result<String, String> {
    let display =
        std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set".to_owned())?;
    let path = socket_path(&display).map_err(|error| format!("the socket: {error}"))?;
    run_with(
        &path,
        Pattern::Checkerboard,
        "wallpaper",
        Shape::Wallpaper,
        shown,
        None,
    )
}

/// What this client draws.
#[derive(Default)]
enum Shown {
    /// The pattern it was given, which is what every entry point but a
    /// wallpaper's asks for.
    #[default]
    Pattern,
    /// One picture, behind everything, which never changes.
    Still(Picture),
    /// A video's frames, behind everything, in turn and then again.
    Moving(Movie),
}

impl Shown {
    /// Whether this is a wallpaper, which stays for as long as there is a
    /// desktop to be behind rather than until a test's deadline.
    fn stays(&self) -> bool {
        !matches!(self, Self::Pattern)
    }

    /// The picture to draw at this moment, where there is one.
    fn picture(&self) -> Option<&Picture> {
        match self {
            Self::Pattern => None,
            Self::Still(picture) => Some(picture),
            Self::Moving(movie) => Some(movie.shown()),
        }
    }
}

/// The client itself: every entry point above, with what it was given.
fn run_with(
    path: &std::path::Path,
    pattern: Pattern,
    title: &str,
    shape: Shape,
    shown: Shown,
    announce: Option<std::path::PathBuf>,
) -> Result<String, String> {
    let stream = UnixStream::connect(path)
        .map_err(|error| format!("connecting to {}: {error}", path.display()))?;
    let mut connection =
        Connection::new(stream).map_err(|error| format!("the connection: {error}"))?;

    let mut out = Writer::new();
    // `wl_display.get_registry` is every client's first request.
    request(
        &mut out,
        id::DISPLAY,
        wl_display::request::GET_REGISTRY,
        &[ArgType::NewId],
        &[Arg::NewId(id::REGISTRY)],
    );
    request(
        &mut out,
        id::DISPLAY,
        wl_display::request::SYNC,
        &[ArgType::NewId],
        &[Arg::NewId(id::SYNC)],
    );
    flush(&mut connection, &mut out)?;

    // A wallpaper stays for as long as there is a desktop to be behind: the
    // deadline is a test client's, there so that one a test forgot does not
    // outlive it for ever.
    let stays = shown.stays();
    // How long a frame of a moving wallpaper is shown, before there is a
    // client to ask.
    let period = match &shown {
        Shown::Moving(movie) => Some(movie.period()),
        _ => None,
    };
    let mut state = Client {
        buffers: if period.is_some() { BUFFERS.len() } else { 1 },
        busy: [false; BUFFERS.len()],
        filled: [false; BUFFERS.len()],
        damaged: 0,
        awaiting: false,
        unpaced: 0,
        pattern,
        shown,
        shape,
        title: title.to_owned(),
        globals: BTreeMap::new(),
        bound: false,
        width: 0,
        height: 0,
        acked: false,
        drawn: 0,
        shared: None,
        buffer_size: (0, 0),
        buffer_format: None,
        released: 0,
        keys: 0,
        pointer_placed: false,
        seat: false,
        scale: 1,
        menu_asked: false,
        menu_size: (0, 0),
        menu: None,
        twin_asked: false,
        twin_size: (0, 0),
        twin: None,
        twin_drawn: None,
        announce: announce.or_else(|| DRAWN.get().cloned()),
        twin_closed: false,
        scaled: Vec::new(),
        stale: [Vec::new(), Vec::new()],
    };

    let started = Instant::now();
    let mut due = Instant::now();
    while stays || started.elapsed() < DEADLINE {
        match connection.receive() {
            Ok(_) => {}
            Err(RecvError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(RecvError::Closed) => break,
            Err(error) => return Err(format!("reading: {error:?}")),
        }
        let consumed = state.read(connection.bytes(), &connection.fds(), &mut out)?;
        if consumed > 0 {
            connection.consume(consumed, 0);
        }
        // A wallpaper that moves: the next frame once it is due, once the
        // compositor has given back a buffer to draw it in, and once it has
        // said it drew the last one. The third is what paces this to the
        // screen rather than to a clock this loop keeps, and it is what
        // stops a wallpaper nobody can see: a surface that is not drawn is
        // told nothing, so `awaiting` stays true and no frame is made.
        //
        // Unless it stays true for [`UNPACED`], which no compositor that
        // draws at all would do, and which would otherwise be a wallpaper
        // stopped for ever by a compositor that answers no callbacks.
        if let Some(period) = period
            && due.elapsed() >= period
            && state.has_a_free_buffer()
            && (!state.awaiting || due.elapsed() >= UNPACED)
        {
            if state.awaiting {
                state.unpaced = state.unpaced.saturating_add(1);
            }
            state.advance();
            state.draw(&mut out)?;
            // The video's own clock, not this loop's: a frame that took
            // longer to draw than it is shown for is made up by the next,
            // and a wallpaper that fell a whole frame behind starts again
            // from now rather than running to catch up.
            due = due.checked_add(period).unwrap_or_else(Instant::now);
            if due.elapsed() >= period {
                due = Instant::now();
            }
        }
        // The extra window goes once it has been up for a moment. This is
        // in the loop rather than in the handler that drew it because a
        // client that destroyed a window in the same breath as it committed
        // one would never have had two on the screen.
        if state.shape == Shape::Twin
            && state
                .twin_drawn
                .is_some_and(|at| at.elapsed() >= TWIN_LINGER)
        {
            state.close_twin(&mut out);
        }
        flush(&mut connection, &mut out)?;
    }

    if let Shown::Moving(movie) = &state.shown {
        return Ok(format!(
            "pattern: video {}x{} of {} frames, drew {}, last damage {} rows of {}, {} unpaced",
            state.width,
            state.height,
            movie.frames(),
            state.drawn,
            state.damaged,
            state.height,
            state.unpaced
        ));
    }
    Ok(format!(
        "pattern: {:?} {}x{} frames {} keys {} title {}",
        state.pattern, state.width, state.height, state.drawn, state.keys, state.title
    ))
}

/// What the client knows.
struct Client {
    pattern: Pattern,
    /// What is drawn instead of the pattern, for a wallpaper.
    shown: Shown,
    /// How many buffers this client keeps.
    ///
    /// One is enough for anything that redraws only when it is configured:
    /// the compositor releases a buffer when a later commit replaces it, so a
    /// client that commits once and waits for the release would wait for
    /// ever. A wallpaper that moves commits a frame every period and cannot
    /// wait for that, so it keeps two and fills whichever it has back.
    buffers: usize,
    /// Whether each buffer is with the compositor.
    busy: [bool; BUFFERS.len()],
    /// Whether each buffer has been drawn into since it was made. A buffer
    /// that has not holds nothing, so the frame that fills it is damaged
    /// whole however little of the video changed.
    filled: [bool; BUFFERS.len()],
    /// How many rows the last frame said had changed, for the line this
    /// client prints when it ends.
    damaged: i32,
    /// Whether a frame callback has been asked for and not yet answered.
    awaiting: bool,
    /// How many frames were drawn without waiting, because the compositor
    /// answered no callback for long enough that a stall was likelier than a
    /// wallpaper nobody can see.
    unpaced: u32,
    shape: Shape,
    title: String,
    /// The registry's names, by interface.
    globals: BTreeMap<String, (u32, u32)>,
    bound: bool,
    width: i32,
    height: i32,
    acked: bool,
    drawn: u32,
    shared: Option<Shared>,
    buffer_size: (i32, i32),
    /// The format the live buffer is in, since a pattern change changes it
    /// and a buffer may not be reinterpreted.
    buffer_format: Option<u32>,
    released: u32,
    /// The buffer scale to draw at: the largest any `wl_output` announced.
    ///
    /// A toolkit picks a surface's scale from the outputs it has entered;
    /// this client has one window and takes the largest scale offered,
    /// which on a machine whose screens all have the same scale is that
    /// scale, and on a mixed one is the sharper of the two.
    scale: i32,
    /// How many keys have been pressed, which also says which pattern is
    /// drawn: the client draws a different one after each key, so that a key
    /// arriving is visible on the screen and not only in a log line.
    keys: u32,
    /// Whether a motion has been reported since the pointer last entered.
    ///
    /// Only the first is: a USB mouse reports up to a thousand times a
    /// second, and on a DK1 a line per motion kept one of its two cores
    /// writing to the serial console for as long as the mouse moved. Nothing
    /// reads the positions -- `tools/common/xtask/src/seat.rs` waits for the enter and the
    /// buttons -- and the first one after an enter is enough to see where the
    /// pointer came in.
    pointer_placed: bool,
    /// Whether the seat has been bound and asked for its objects.
    seat: bool,
    /// Whether the menu has been asked for, so it is asked for once.
    menu_asked: bool,
    /// The size the compositor configured the menu to.
    menu_size: (i32, i32),
    /// The memory the menu is drawn into, once its size is known.
    menu: Option<Shared>,
    /// [`Shape::Twin`]'s extra window: whether it has been asked for, the
    /// size the compositor gave it, the memory it draws in, when it was
    /// first drawn, and whether it has been destroyed.
    twin_asked: bool,
    twin_size: (i32, i32),
    twin: Option<Shared>,
    twin_drawn: Option<Instant>,
    /// Where to say the window has been drawn: this client's own file, or
    /// the program's (`announce_when_drawn`), or nowhere.
    announce: Option<std::path::PathBuf>,
    twin_closed: bool,
    /// A moving wallpaper's frame, scaled to the buffer's size and kept
    /// between frames, so that a frame scales only the rows the video
    /// changed rather than the screen: the rest are what they were.
    scaled: Vec<u8>,
    /// Which rows of each buffer are not what `scaled` holds. A frame is
    /// copied into a buffer a stale row at a time, and marks the rows it
    /// changed stale in every other buffer; a buffer just made is stale
    /// throughout, since it holds nothing.
    stale: [Vec<bool>; BUFFERS.len()],
}

impl Client {
    /// Read whatever arrived, and answer it.
    fn read(&mut self, bytes: &[u8], fds: &[Fd], out: &mut Writer) -> Result<usize, String> {
        let mut reader = Reader::new(bytes, fds);
        while !reader.is_done() {
            let Ok(header) = reader.peek() else {
                break;
            };
            let Some(interface) = self.interface_of(header.sender) else {
                // An object this client did not make: the compositor is
                // speaking about something it invented, which it may not.
                return Err(format!("an event for object {}", header.sender.0));
            };
            let Some(method) = interface.event(header.opcode) else {
                return Err(format!("{} has no event {}", interface.name, header.opcode));
            };
            let (_, args) = match reader.read(method.signature) {
                Ok(read) => read,
                Err(compositor_wire::Error::Incomplete { .. }) => break,
                Err(error) => return Err(format!("{}.{}: {error:?}", interface.name, method.name)),
            };
            self.event(header.sender, header.opcode, &args, out)?;
        }
        Ok(reader.consumed())
    }

    /// Which interface an object of this client's speaks.
    fn interface_of(&self, id: ObjectId) -> Option<&'static Interface> {
        Some(match id {
            id::DISPLAY => &core::WL_DISPLAY,
            id::REGISTRY => &core::WL_REGISTRY,
            id::SYNC => &core::WL_CALLBACK,
            id::SHM => &core::WL_SHM,
            id::SURFACE => &core::WL_SURFACE,
            id::XDG_SURFACE => &xdg_shell::XDG_SURFACE,
            id::TOPLEVEL => &xdg_shell::XDG_TOPLEVEL,
            id::LAYER_SHELL => &layer_shell::ZWLR_LAYER_SHELL_V1,
            id::LAYER_SURFACE => &layer_shell::ZWLR_LAYER_SURFACE_V1,
            id::SEAT => &core::WL_SEAT,
            id::KEYBOARD => &core::WL_KEYBOARD,
            id::POINTER => &core::WL_POINTER,
            id::SHELL => &xdg_shell::XDG_WM_BASE,
            id::BUFFER | id::BUFFER_TWO => &core::WL_BUFFER,
            id::FRAME => &core::WL_CALLBACK,
            id::OUTPUT => &core::WL_OUTPUT,
            id::DECORATION => &xdg_decoration::ZXDG_TOPLEVEL_DECORATION_V1,
            id::POSITIONER => &xdg_shell::XDG_POSITIONER,
            id::POPUP_SURFACE => &core::WL_SURFACE,
            id::POPUP_XDG => &xdg_shell::XDG_SURFACE,
            id::POPUP => &xdg_shell::XDG_POPUP,
            id::POPUP_POOL => &core::WL_SHM_POOL,
            id::POPUP_BUFFER => &core::WL_BUFFER,
            id::TWIN_SURFACE => &core::WL_SURFACE,
            id::TWIN_XDG => &xdg_shell::XDG_SURFACE,
            id::TWIN_TOPLEVEL => &xdg_shell::XDG_TOPLEVEL,
            id::TWIN_POOL => &core::WL_SHM_POOL,
            id::TWIN_BUFFER => &core::WL_BUFFER,
            _ => return None,
        })
    }

    /// Answer one event.
    fn event(
        &mut self,
        sender: ObjectId,
        opcode: u16,
        args: &[Arg<'_>],
        out: &mut Writer,
    ) -> Result<(), String> {
        match sender {
            id::DISPLAY if opcode == wl_display::event::ERROR => {
                let text = args.get(2).and_then(Arg::as_str).unwrap_or("");
                return Err(format!("the compositor refused this client: {text}"));
            }
            id::REGISTRY if opcode == wl_registry::event::GLOBAL => {
                let (Some(name), Some(interface), Some(version)) = (
                    args.first().and_then(Arg::as_uint),
                    args.get(1).and_then(Arg::as_str),
                    args.get(2).and_then(Arg::as_uint),
                ) else {
                    return Ok(());
                };
                let _ = self.globals.insert(interface.to_owned(), (name, version));
            }
            id::SYNC if opcode == core::wl_callback::event::DONE => {
                // The registry has been announced whole: bind what is needed.
                self.bind(out)?;
            }
            // The compositor drew the last frame, so this one may draw the
            // next. A surface nobody can see is not drawn and is told
            // nothing, which is how a wallpaper behind a full-screen window
            // stops playing without being asked to -- what `mpvpaper-stop`
            // is for on a desktop that has to be told.
            id::FRAME if opcode == core::wl_callback::event::DONE => {
                self.awaiting = false;
                // The compositor drew a frame with this window in it: it is
                // placed. Said once, to whoever waits with `--after` or on
                // this client's own file.
                if let Some(path) = self.announce.as_ref()
                    && !path.exists()
                {
                    let _ = std::fs::write(path, b"");
                }
            }
            id::SHELL if opcode == xdg_wm_base::event::PING => {
                let serial = args.first().and_then(Arg::as_uint).unwrap_or(0);
                request(
                    out,
                    id::SHELL,
                    xdg_wm_base::request::PONG,
                    &[ArgType::Uint],
                    &[Arg::Uint(serial)],
                );
            }
            id::TOPLEVEL if opcode == xdg_toplevel::event::CONFIGURE => {
                let (width, height) = (
                    args.first().and_then(Arg::as_int).unwrap_or(0),
                    args.get(1).and_then(Arg::as_int).unwrap_or(0),
                );
                // A zero means "you choose", which every client answers with
                // the size it would like.
                let (before_width, before_height) = (self.width, self.height);
                self.width = if width > 0 { width } else { 640 };
                self.height = if height > 0 { height } else { 480 };
                if (self.width, self.height) != (before_width, before_height) {
                    // Said out loud because a window that was not told it
                    // had grown is drawn at the size it had, and the
                    // difference between that and a compositor bug is this
                    // line.
                    say(&format!(
                        "pattern: {} configured {}x{}",
                        self.title, self.width, self.height
                    ));
                }
            }
            id::XDG_SURFACE if opcode == xdg_surface::event::CONFIGURE => {
                let serial = args.first().and_then(Arg::as_uint).unwrap_or(0);
                request(
                    out,
                    id::XDG_SURFACE,
                    xdg_surface::request::ACK_CONFIGURE,
                    &[ArgType::Uint],
                    &[Arg::Uint(serial)],
                );
                self.acked = true;
                // Drawn again only for the first configure or a new size: a
                // configure that changes nothing -- every focus change, which
                // toggles `activated` -- is acknowledged and no more, as a
                // toolkit does. Repainting for each made every pass of the
                // pointer between two windows two whole windows to composite
                // (the DK1, 2026-09-24).
                let scale = self.scale.max(1);
                let wanted = (
                    self.width.saturating_mul(scale),
                    self.height.saturating_mul(scale),
                );
                if self.shared.is_none() || self.buffer_size != wanted {
                    self.draw(out)?;
                }
                // A menu is asked for after the window has something on it,
                // which is when a toolkit would: a popup on a window that
                // has never drawn is a menu with nothing under it.
                if let Shape::Menu(side) = self.shape {
                    self.open_menu(side, out);
                }
                // The second window, for the same reason a menu waits: a
                // client opens its dialog after its document window is up,
                // and a layout given two windows at once would not be
                // showing what a layout given a second one does.
                if self.shape == Shape::Twin {
                    self.open_twin(out);
                }
            }
            id::POPUP_XDG if opcode == xdg_surface::event::CONFIGURE => {
                let serial = args.first().and_then(Arg::as_uint).unwrap_or(0);
                request(
                    out,
                    id::POPUP_XDG,
                    xdg_surface::request::ACK_CONFIGURE,
                    &[ArgType::Uint],
                    &[Arg::Uint(serial)],
                );
                let (width, height) = self.menu_size;
                self.draw_menu(width, height, out)?;
            }
            id::POPUP if opcode == xdg_popup::event::CONFIGURE => {
                // Where the compositor put it, in the parent's own
                // coordinates, and how large it is allowed to be.
                let value = |at: usize| args.get(at).and_then(Arg::as_int).unwrap_or(0);
                self.menu_size = (value(2), value(3));
                say(&format!(
                    "pattern: {} menu at {},{} {}x{}",
                    self.title,
                    value(0),
                    value(1),
                    value(2),
                    value(3)
                ));
            }
            id::TWIN_TOPLEVEL if opcode == xdg_toplevel::event::CONFIGURE => {
                let (width, height) = (
                    args.first().and_then(Arg::as_int).unwrap_or(0),
                    args.get(1).and_then(Arg::as_int).unwrap_or(0),
                );
                // A zero is "you choose", answered as the first window
                // answers it.
                self.twin_size = (
                    if width > 0 { width } else { 640 },
                    if height > 0 { height } else { 480 },
                );
            }
            id::TWIN_XDG if opcode == xdg_surface::event::CONFIGURE => {
                let serial = args.first().and_then(Arg::as_uint).unwrap_or(0);
                request(
                    out,
                    id::TWIN_XDG,
                    xdg_surface::request::ACK_CONFIGURE,
                    &[ArgType::Uint],
                    &[Arg::Uint(serial)],
                );
                let (width, height) = self.twin_size;
                self.draw_twin(width, height, out)?;
            }
            id::TWIN_TOPLEVEL if opcode == xdg_toplevel::event::CLOSE => {
                // Nothing in this shape asks for that, and a compositor that
                // sent it would be closing a window the client is about to
                // close itself -- which is the one thing this client must
                // not be able to confuse with its own destroy.
                return Err(format!(
                    "the compositor asked the second window of {} to close",
                    self.title
                ));
            }
            id::POPUP if opcode == xdg_popup::event::POPUP_DONE => {
                say(&format!("pattern: {} menu dismissed", self.title));
            }
            id::DECORATION if opcode == zxdg_toplevel_decoration_v1::event::CONFIGURE => {
                // Which side draws this window's decorations. Said out loud
                // because it is the only way to see it: a compositor that
                // never answers and one that answers `server_side` look the
                // same on the screen until a toolkit draws its own title
                // bar.
                let mode = args.first().and_then(Arg::as_uint).unwrap_or(0);
                let side = if mode == zxdg_toplevel_decoration_v1::mode::SERVER_SIDE {
                    "server_side"
                } else {
                    "client_side"
                };
                say(&format!("pattern: {} decorations {side}", self.title));
            }
            id::TOPLEVEL if opcode == xdg_toplevel::event::CLOSE => {
                // Which window, because a test that asks for one to be
                // closed from outside it has to be able to say whether the
                // right one went.
                return Err(format!(
                    "the compositor asked the {:?} window called {} to close",
                    self.pattern, self.title
                ));
            }
            // A buffer the compositor has finished reading is this client's
            // to draw in again, which is what a wallpaper that moves waits
            // for before it draws the next frame.
            object if opcode == core::wl_buffer::event::RELEASE && BUFFERS.contains(&object) => {
                self.released = self.released.saturating_add(1);
                if let Some(with) = BUFFERS
                    .iter()
                    .position(|buffer| *buffer == object)
                    .and_then(|slot| self.busy.get_mut(slot))
                {
                    *with = false;
                }
            }
            id::OUTPUT if opcode == core::wl_output::event::SCALE => {
                let scale = args.first().and_then(Arg::as_int).unwrap_or(1);
                if scale > self.scale {
                    self.scale = scale;
                    say(&format!("pattern: output scale {scale}"));
                    // The buffer is the wrong size now, and the window's
                    // logical size has not changed: draw it again.
                    self.draw(out)?;
                }
            }
            id::SEAT if opcode == wl_seat::event::CAPABILITIES => {
                self.seat(args.first().and_then(Arg::as_uint).unwrap_or(0), out);
            }
            id::LAYER_SURFACE if opcode == zwlr_layer_surface_v1::event::CONFIGURE => {
                let serial = args.first().and_then(Arg::as_uint).unwrap_or(0);
                let (width, height) = (
                    args.get(1).and_then(Arg::as_uint).unwrap_or(0),
                    args.get(2).and_then(Arg::as_uint).unwrap_or(0),
                );
                request(
                    out,
                    id::LAYER_SURFACE,
                    zwlr_layer_surface_v1::request::ACK_CONFIGURE,
                    &[ArgType::Uint],
                    &[Arg::Uint(serial)],
                );
                self.width = i32::try_from(width).unwrap_or(0);
                self.height = i32::try_from(height).unwrap_or(0);
                self.acked = true;
                say(&format!("pattern: layer {width}x{height}"));
                self.draw(out)?;
                // The menu once the bar has something on it, as a window's
                // waits for the window.
                if let Shape::BarMenu(_, side) = self.shape {
                    self.open_menu(side, out);
                }
            }
            id::LAYER_SURFACE if opcode == zwlr_layer_surface_v1::event::CLOSED => {
                return Err("the compositor closed this layer surface".to_owned());
            }
            id::KEYBOARD => self.keyboard(opcode, args, out)?,
            id::POINTER => self.pointer(opcode, args),
            _ => {}
        }
        Ok(())
    }

    /// Ask the seat for the objects it says it has.
    ///
    /// A client may only ask for a capability the seat announced, so this
    /// waits for `wl_seat.capabilities` rather than asking on binding. A seat
    /// that announces nothing leaves the client with no keyboard, which is a
    /// machine with nothing plugged in.
    fn seat(&mut self, capabilities: u32, out: &mut Writer) {
        if self.seat {
            return;
        }
        self.seat = true;
        for (bit, id, opcode) in [
            (
                wl_seat::capability::KEYBOARD,
                id::KEYBOARD,
                wl_seat::request::GET_KEYBOARD,
            ),
            (
                wl_seat::capability::POINTER,
                id::POINTER,
                wl_seat::request::GET_POINTER,
            ),
        ] {
            if capabilities & bit == 0 {
                continue;
            }
            request(out, id::SEAT, opcode, &[ArgType::NewId], &[Arg::NewId(id)]);
        }
        say(&format!("pattern: seat capabilities {capabilities:#x}"));
    }

    /// What the keyboard said. Every event is printed, because this client is
    /// how `cargo xtask test-seat` sees what reached a window.
    fn keyboard(&mut self, opcode: u16, args: &[Arg<'_>], out: &mut Writer) -> Result<(), String> {
        match opcode {
            wl_keyboard::event::KEYMAP => {
                let format = args.first().and_then(Arg::as_uint).unwrap_or(0);
                let size = args.get(2).and_then(Arg::as_uint).unwrap_or(0);
                say(&format!("pattern: keymap format {format} size {size}"));
            }
            wl_keyboard::event::ENTER => {
                say("pattern: keyboard enter");
            }
            wl_keyboard::event::LEAVE => {
                say("pattern: keyboard leave");
            }
            wl_keyboard::event::KEY => {
                let code = args.get(2).and_then(Arg::as_uint).unwrap_or(0);
                let state = args.get(3).and_then(Arg::as_uint).unwrap_or(0);
                say(&format!("pattern: key {code} state {state}"));
                if state == wl_keyboard::key_state::PRESSED {
                    // Draw something else, so that a key arriving shows on
                    // the screen and not only in this line.
                    self.keys = self.keys.saturating_add(1);
                    self.pattern = if self.keys % 2 == 1 {
                        Pattern::Gradient
                    } else {
                        Pattern::Checkerboard
                    };
                    self.draw(out)?;
                }
            }
            wl_keyboard::event::MODIFIERS => {
                let depressed = args.get(1).and_then(Arg::as_uint).unwrap_or(0);
                let locked = args.get(3).and_then(Arg::as_uint).unwrap_or(0);
                say(&format!(
                    "pattern: modifiers depressed {depressed:#x} locked {locked:#x}"
                ));
            }
            wl_keyboard::event::REPEAT_INFO => {
                let rate = args.first().and_then(Arg::as_int).unwrap_or(0);
                let delay = args.get(1).and_then(Arg::as_int).unwrap_or(0);
                say(&format!("pattern: repeat {rate} after {delay}"));
            }
            _ => {}
        }
        Ok(())
    }

    /// What the pointer said.
    fn pointer(&mut self, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            wl_pointer::event::ENTER => {
                self.pointer_placed = false;
                say("pattern: pointer enter");
            }
            wl_pointer::event::LEAVE => say("pattern: pointer leave"),
            wl_pointer::event::MOTION if !self.pointer_placed => {
                self.pointer_placed = true;
                let (x, y) = (
                    args.get(1).and_then(Arg::as_fixed),
                    args.get(2).and_then(Arg::as_fixed),
                );
                if let (Some(x), Some(y)) = (x, y) {
                    say(&format!(
                        "pattern: pointer at {} {}",
                        x.to_int(),
                        y.to_int()
                    ));
                }
            }
            wl_pointer::event::BUTTON => {
                let button = args.get(2).and_then(Arg::as_uint).unwrap_or(0);
                let state = args.get(3).and_then(Arg::as_uint).unwrap_or(0);
                say(&format!("pattern: button {button} state {state}"));
            }
            wl_pointer::event::AXIS => {
                let axis = args.get(1).and_then(Arg::as_uint).unwrap_or(0);
                let value = args.get(2).and_then(Arg::as_fixed);
                if let Some(value) = value {
                    say(&format!("pattern: axis {axis} by {}", value.to_int()));
                }
            }
            _ => {}
        }
    }

    /// Bind what a window needs, and ask for one.
    fn bind(&mut self, out: &mut Writer) -> Result<(), String> {
        if self.bound {
            return Ok(());
        }
        // `wl_seat` is wanted and not required: a compositor with nothing
        // plugged in may offer none, and a client that refused to start over
        // it would be a client that only runs on a machine with a keyboard.
        let bar = matches!(
            self.shape,
            Shape::Bar(_) | Shape::BarMenu(..) | Shape::Wallpaper
        );
        // A bar with a menu needs the shell for the menu's `xdg_surface`,
        // and nothing else of it.
        let shell = !bar || matches!(self.shape, Shape::BarMenu(..));
        for (interface, id, want, required) in [
            ("wl_compositor", id::COMPOSITOR, 6u32, true),
            ("wl_shm", id::SHM, 1, true),
            ("xdg_wm_base", id::SHELL, shell as u32 * 6, shell),
            ("wl_seat", id::SEAT, 7, false),
            // `wl_output` for its scale: a client on a scaled monitor draws
            // a buffer that many times the size and says so, or the
            // compositor has to stretch what it sent.
            ("wl_output", id::OUTPUT, 4, false),
            ("zwlr_layer_shell_v1", id::LAYER_SHELL, 5, bar),
            // Who draws the title bar. Wanted rather than required, as
            // every toolkit treats it: a compositor that offers none is one
            // where the client draws its own -- which is exactly what this
            // asks in order to find out.
            (
                "zxdg_decoration_manager_v1",
                id::DECORATIONS,
                !bar as u32,
                false,
            ),
        ] {
            if want == 0 {
                continue;
            }
            let offer = self.globals.get(interface).copied();
            if offer.is_none() && !required {
                say(&format!("pattern: the compositor offers no {interface}"));
                continue;
            }
            let (name, offered) =
                offer.ok_or_else(|| format!("the compositor offers no {interface}"))?;
            let version = want.min(offered);
            out.write(
                id::REGISTRY,
                wl_registry::request::BIND,
                &[ArgType::Uint, ArgType::AnyNewId],
                &[
                    Arg::Uint(name),
                    Arg::AnyNewId {
                        interface,
                        version,
                        id,
                    },
                ],
            )
            .map_err(|error| format!("binding {interface}: {error:?}"))?;
        }
        self.bound = true;

        // The surface, then the role, in the order the protocol requires.
        request(
            out,
            id::COMPOSITOR,
            wl_compositor::request::CREATE_SURFACE,
            &[ArgType::NewId],
            &[Arg::NewId(id::SURFACE)],
        );
        if let Shape::Bar(height) | Shape::BarMenu(height, _) = self.shape {
            return self.become_bar(height, out);
        }
        if self.shape == Shape::Wallpaper {
            return self.become_wallpaper(out);
        }
        request(
            out,
            id::SHELL,
            xdg_wm_base::request::GET_XDG_SURFACE,
            &[ArgType::NewId, ArgType::Object { nullable: false }],
            &[Arg::NewId(id::XDG_SURFACE), Arg::Object(id::SURFACE)],
        );
        request(
            out,
            id::XDG_SURFACE,
            xdg_surface::request::GET_TOPLEVEL,
            &[ArgType::NewId],
            &[Arg::NewId(id::TOPLEVEL)],
        );
        let title = self.title.clone();
        request(
            out,
            id::TOPLEVEL,
            xdg_toplevel::request::SET_TITLE,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(&title))],
        );
        request(
            out,
            id::TOPLEVEL,
            xdg_toplevel::request::SET_APP_ID,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some("rocks.magical.pattern"))],
        );
        // Who draws this window's title bar, if the compositor has an
        // opinion. A toolkit asks before its first frame, because the answer
        // decides how much of its own surface it has to leave for a bar.
        if self.globals.contains_key("zxdg_decoration_manager_v1") {
            request(
                out,
                id::DECORATIONS,
                zxdg_decoration_manager_v1::request::GET_TOPLEVEL_DECORATION,
                &[ArgType::NewId, ArgType::Object { nullable: false }],
                &[Arg::NewId(id::DECORATION), Arg::Object(id::TOPLEVEL)],
            );
        }
        // The first commit carries no buffer: it asks to be configured.
        request(out, id::SURFACE, wl_surface::request::COMMIT, &[], &[]);
        Ok(())
    }

    /// Ask for a menu on this window: an `xdg_popup` hanging off the
    /// window's top-left corner.
    ///
    /// The order is every toolkit's: a positioner with a size and an anchor
    /// rectangle, a surface, an `xdg_surface` over it, `get_popup`, and a
    /// commit with no buffer that asks to be configured. Nothing may be
    /// drawn until the compositor has said where the popup is.
    fn open_menu(&mut self, side: u32, out: &mut Writer) {
        if self.menu_asked {
            return;
        }
        self.menu_asked = true;
        let on_layer = matches!(self.shape, Shape::BarMenu(..));
        let side = i32::try_from(side).unwrap_or(1).max(1);
        request(
            out,
            id::SHELL,
            xdg_wm_base::request::CREATE_POSITIONER,
            &[ArgType::NewId],
            &[Arg::NewId(id::POSITIONER)],
        );
        request(
            out,
            id::POSITIONER,
            xdg_positioner::request::SET_SIZE,
            &[ArgType::Int, ArgType::Int],
            &[Arg::Int(side), Arg::Int(side)],
        );
        // A one-pixel rectangle at the window's top-left, which is what a
        // menu opened at a point hangs off.
        request(
            out,
            id::POSITIONER,
            xdg_positioner::request::SET_ANCHOR_RECT,
            &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
            &[Arg::Int(0), Arg::Int(0), Arg::Int(1), Arg::Int(1)],
        );
        for (opcode, value) in [
            (
                xdg_positioner::request::SET_ANCHOR,
                xdg_positioner::anchor::BOTTOM_RIGHT,
            ),
            (
                xdg_positioner::request::SET_GRAVITY,
                xdg_positioner::gravity::BOTTOM_RIGHT,
            ),
            (
                xdg_positioner::request::SET_CONSTRAINT_ADJUSTMENT,
                xdg_positioner::constraint_adjustment::SLIDE_X
                    | xdg_positioner::constraint_adjustment::SLIDE_Y,
            ),
        ] {
            request(
                out,
                id::POSITIONER,
                opcode,
                &[ArgType::Uint],
                &[Arg::Uint(value)],
            );
        }
        request(
            out,
            id::COMPOSITOR,
            wl_compositor::request::CREATE_SURFACE,
            &[ArgType::NewId],
            &[Arg::NewId(id::POPUP_SURFACE)],
        );
        request(
            out,
            id::SHELL,
            xdg_wm_base::request::GET_XDG_SURFACE,
            &[ArgType::NewId, ArgType::Object { nullable: false }],
            &[Arg::NewId(id::POPUP_XDG), Arg::Object(id::POPUP_SURFACE)],
        );
        request(
            out,
            id::POPUP_XDG,
            xdg_surface::request::GET_POPUP,
            &[
                ArgType::NewId,
                ArgType::Object { nullable: true },
                ArgType::Object { nullable: false },
            ],
            &[
                Arg::NewId(id::POPUP),
                // A layer surface's popup is made with no parent and then
                // handed to the layer surface, which is the protocol's own
                // two steps: `xdg_surface.get_popup` has no way to name a
                // surface that is not an `xdg_surface`.
                Arg::Object(if on_layer {
                    ObjectId(0)
                } else {
                    id::XDG_SURFACE
                }),
                Arg::Object(id::POSITIONER),
            ],
        );
        if on_layer {
            request(
                out,
                id::LAYER_SURFACE,
                zwlr_layer_surface_v1::request::GET_POPUP,
                &[ArgType::Object { nullable: false }],
                &[Arg::Object(id::POPUP)],
            );
        }
        request(
            out,
            id::POPUP_SURFACE,
            wl_surface::request::COMMIT,
            &[],
            &[],
        );
    }

    /// Draw the menu at the size the compositor put it, and commit it.
    fn draw_menu(&mut self, width: i32, height: i32, out: &mut Writer) -> Result<(), String> {
        let (width, height) = (width.max(1), height.max(1));
        let stride = width.saturating_mul(4);
        let len = usize::try_from(stride.saturating_mul(height))
            .map_err(|_| "a menu too large to draw".to_owned())?;
        if self.menu.is_none() {
            let shared = Shared::new(len).map_err(|error| format!("shared memory: {error}"))?;
            let fd = shared.as_raw_fd();
            self.menu = Some(shared);
            request_with_fd(
                out,
                id::SHM,
                wl_shm::request::CREATE_POOL,
                &[ArgType::NewId, ArgType::Fd, ArgType::Int],
                &[
                    Arg::NewId(id::POPUP_POOL),
                    Arg::Fd(Fd(fd)),
                    Arg::Int(i32::try_from(len).unwrap_or(i32::MAX)),
                ],
            );
            request(
                out,
                id::POPUP_POOL,
                wl_shm_pool::request::CREATE_BUFFER,
                &[
                    ArgType::NewId,
                    ArgType::Int,
                    ArgType::Int,
                    ArgType::Int,
                    ArgType::Int,
                    ArgType::Uint,
                ],
                &[
                    Arg::NewId(id::POPUP_BUFFER),
                    Arg::Int(0),
                    Arg::Int(width),
                    Arg::Int(height),
                    Arg::Int(stride),
                    Arg::Uint(Pattern::Gradient.format().wl_shm()),
                ],
            );
        }
        // The gradient, so the menu is a picture a test can compare and is
        // plainly not the window under it.
        let pixels = Pattern::Gradient.draw(
            u32::try_from(width).unwrap_or(0),
            u32::try_from(height).unwrap_or(0),
        );
        if let Some(shared) = self.menu.as_mut() {
            let room = shared.bytes_mut();
            let take = pixels.len().min(room.len());
            if let (Some(to), Some(from)) = (room.get_mut(..take), pixels.get(..take)) {
                to.copy_from_slice(from);
            }
        }
        request(
            out,
            id::POPUP_SURFACE,
            wl_surface::request::ATTACH,
            &[
                ArgType::Object { nullable: true },
                ArgType::Int,
                ArgType::Int,
            ],
            &[Arg::Object(id::POPUP_BUFFER), Arg::Int(0), Arg::Int(0)],
        );
        request(
            out,
            id::POPUP_SURFACE,
            wl_surface::request::DAMAGE_BUFFER,
            &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
            &[Arg::Int(0), Arg::Int(0), Arg::Int(width), Arg::Int(height)],
        );
        request(
            out,
            id::POPUP_SURFACE,
            wl_surface::request::COMMIT,
            &[],
            &[],
        );
        Ok(())
    }

    /// Open the extra window: a second `xdg_toplevel` on this connection,
    /// made in the same order and with the same requests as the first.
    fn open_twin(&mut self, out: &mut Writer) {
        if self.twin_asked {
            return;
        }
        self.twin_asked = true;
        request(
            out,
            id::COMPOSITOR,
            wl_compositor::request::CREATE_SURFACE,
            &[ArgType::NewId],
            &[Arg::NewId(id::TWIN_SURFACE)],
        );
        request(
            out,
            id::SHELL,
            xdg_wm_base::request::GET_XDG_SURFACE,
            &[ArgType::NewId, ArgType::Object { nullable: false }],
            &[Arg::NewId(id::TWIN_XDG), Arg::Object(id::TWIN_SURFACE)],
        );
        request(
            out,
            id::TWIN_XDG,
            xdg_surface::request::GET_TOPLEVEL,
            &[ArgType::NewId],
            &[Arg::NewId(id::TWIN_TOPLEVEL)],
        );
        // A title of its own, so a taskbar's list and this test's log say
        // which of the client's two windows each line is about.
        let title = format!("{} second", self.title);
        request(
            out,
            id::TWIN_TOPLEVEL,
            xdg_toplevel::request::SET_TITLE,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some(&title))],
        );
        request(
            out,
            id::TWIN_TOPLEVEL,
            xdg_toplevel::request::SET_APP_ID,
            &[ArgType::Str { nullable: false }],
            &[Arg::Str(Some("rocks.magical.pattern"))],
        );
        // The first commit carries no buffer: it asks to be configured.
        request(out, id::TWIN_SURFACE, wl_surface::request::COMMIT, &[], &[]);
    }

    /// Draw the extra window at the size the compositor gave it.
    ///
    /// The other pattern, so that a picture taken while both are up shows
    /// two windows rather than one twice, and so that a picture taken after
    /// the close is plainly the window that was kept.
    fn draw_twin(&mut self, width: i32, height: i32, out: &mut Writer) -> Result<(), String> {
        if self.twin_closed {
            return Ok(());
        }
        let (width, height) = (width.max(1), height.max(1));
        let stride = width.saturating_mul(4);
        let len = usize::try_from(stride.saturating_mul(height))
            .map_err(|_| "a second window too large to draw".to_owned())?;
        if self.twin.is_none() {
            let shared = Shared::new(len).map_err(|error| format!("shared memory: {error}"))?;
            let fd = shared.as_raw_fd();
            self.twin = Some(shared);
            request_with_fd(
                out,
                id::SHM,
                wl_shm::request::CREATE_POOL,
                &[ArgType::NewId, ArgType::Fd, ArgType::Int],
                &[
                    Arg::NewId(id::TWIN_POOL),
                    Arg::Fd(Fd(fd)),
                    Arg::Int(i32::try_from(len).unwrap_or(i32::MAX)),
                ],
            );
            request(
                out,
                id::TWIN_POOL,
                wl_shm_pool::request::CREATE_BUFFER,
                &[
                    ArgType::NewId,
                    ArgType::Int,
                    ArgType::Int,
                    ArgType::Int,
                    ArgType::Int,
                    ArgType::Uint,
                ],
                &[
                    Arg::NewId(id::TWIN_BUFFER),
                    Arg::Int(0),
                    Arg::Int(width),
                    Arg::Int(height),
                    Arg::Int(stride),
                    Arg::Uint(self.twin_pattern().format().wl_shm()),
                ],
            );
        }
        let pixels = self.twin_pattern().draw(
            u32::try_from(width).unwrap_or(0),
            u32::try_from(height).unwrap_or(0),
        );
        if let Some(shared) = self.twin.as_mut() {
            let room = shared.bytes_mut();
            let take = pixels.len().min(room.len());
            if let (Some(to), Some(from)) = (room.get_mut(..take), pixels.get(..take)) {
                to.copy_from_slice(from);
            }
        }
        request(
            out,
            id::TWIN_SURFACE,
            wl_surface::request::ATTACH,
            &[
                ArgType::Object { nullable: true },
                ArgType::Int,
                ArgType::Int,
            ],
            &[Arg::Object(id::TWIN_BUFFER), Arg::Int(0), Arg::Int(0)],
        );
        request(
            out,
            id::TWIN_SURFACE,
            wl_surface::request::DAMAGE_BUFFER,
            &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
            &[Arg::Int(0), Arg::Int(0), Arg::Int(width), Arg::Int(height)],
        );
        request(out, id::TWIN_SURFACE, wl_surface::request::COMMIT, &[], &[]);
        if self.twin_drawn.is_none() {
            self.twin_drawn = Some(Instant::now());
            say(&format!(
                "pattern: {} second window {width}x{height}",
                self.title
            ));
        }
        Ok(())
    }

    /// Destroy the extra window and nothing else.
    ///
    /// The connection stays, the first window stays, and its `wl_surface`
    /// is never touched -- so a compositor that still tiles two windows
    /// after this is one that only notices a window going when its whole
    /// client does.
    fn close_twin(&mut self, out: &mut Writer) {
        if self.twin_closed || !self.twin_asked {
            return;
        }
        self.twin_closed = true;
        // A toolkit destroys the role before the surface under it, and the
        // buffer last, because a surface with a buffer attached is a surface
        // the compositor may still be reading.
        request(
            out,
            id::TWIN_TOPLEVEL,
            xdg_toplevel::request::DESTROY,
            &[],
            &[],
        );
        request(out, id::TWIN_XDG, xdg_surface::request::DESTROY, &[], &[]);
        request(
            out,
            id::TWIN_SURFACE,
            wl_surface::request::DESTROY,
            &[],
            &[],
        );
        if self.twin.is_some() {
            request(
                out,
                id::TWIN_BUFFER,
                core::wl_buffer::request::DESTROY,
                &[],
                &[],
            );
            request(out, id::TWIN_POOL, wl_shm_pool::request::DESTROY, &[], &[]);
        }
        self.twin = None;
        say(&format!(
            "pattern: {} destroyed its second window",
            self.title
        ));
    }

    /// The pattern the extra window draws: the one the first window does
    /// not.
    const fn twin_pattern(&self) -> Pattern {
        match self.pattern {
            Pattern::Checkerboard => Pattern::Gradient,
            Pattern::Gradient => Pattern::Checkerboard,
        }
    }

    /// Draw the pattern at the size the compositor gave, and commit it.
    /// Ask for a `zwlr_layer_surface_v1` across the top edge: what a bar
    /// asks for, in the order `waybar` asks for it.
    fn become_bar(&mut self, height: u32, out: &mut Writer) -> Result<(), String> {
        self.become_layer(
            out,
            (zwlr_layer_shell_v1::layer::TOP, "pattern-bar"),
            zwlr_layer_surface_v1::anchor::TOP
                | zwlr_layer_surface_v1::anchor::LEFT
                | zwlr_layer_surface_v1::anchor::RIGHT,
            // Zero across, because it is anchored to both side edges and the
            // compositor decides; the height is the bar's own, and all of it
            // is reserved.
            (height, i32::try_from(height).unwrap_or(0)),
        )
    }

    /// The `background` layer, every edge, no size of its own and `-1` for
    /// the zone: the whole screen, under the bars as well as the windows,
    /// which is what `swaybg`, `hyprpaper` and `mpvpaper` ask for.
    fn become_wallpaper(&mut self, out: &mut Writer) -> Result<(), String> {
        self.become_layer(
            out,
            (zwlr_layer_shell_v1::layer::BACKGROUND, "wallpaper"),
            zwlr_layer_surface_v1::anchor::TOP
                | zwlr_layer_surface_v1::anchor::BOTTOM
                | zwlr_layer_surface_v1::anchor::LEFT
                | zwlr_layer_surface_v1::anchor::RIGHT,
            (0, -1),
        )
    }

    /// A layer surface on `layer` under `namespace`, anchored to `anchor`,
    /// `height` tall where it is not anchored top and bottom, with `zone`
    /// for its exclusive zone.
    fn become_layer(
        &mut self,
        out: &mut Writer,
        (layer, namespace): (u32, &str),
        anchor: u32,
        (height, zone): (u32, i32),
    ) -> Result<(), String> {
        request(
            out,
            id::LAYER_SHELL,
            zwlr_layer_shell_v1::request::GET_LAYER_SURFACE,
            &[
                ArgType::NewId,
                ArgType::Object { nullable: false },
                ArgType::Object { nullable: true },
                ArgType::Uint,
                ArgType::Str { nullable: false },
            ],
            &[
                Arg::NewId(id::LAYER_SURFACE),
                Arg::Object(id::SURFACE),
                // A null output: the compositor chooses, which is what a bar
                // with no monitor configured asks for.
                Arg::Object(ObjectId(0)),
                Arg::Uint(layer),
                Arg::Str(Some(namespace)),
            ],
        );
        request(
            out,
            id::LAYER_SURFACE,
            zwlr_layer_surface_v1::request::SET_ANCHOR,
            &[ArgType::Uint],
            &[Arg::Uint(anchor)],
        );
        request(
            out,
            id::LAYER_SURFACE,
            zwlr_layer_surface_v1::request::SET_SIZE,
            &[ArgType::Uint, ArgType::Uint],
            // Zero where it is anchored to both edges, because the
            // compositor decides.
            &[Arg::Uint(0), Arg::Uint(height)],
        );
        request(
            out,
            id::LAYER_SURFACE,
            zwlr_layer_surface_v1::request::SET_EXCLUSIVE_ZONE,
            &[ArgType::Int],
            &[Arg::Int(zone)],
        );
        // A surface with a role and no buffer: the compositor answers with a
        // configure, and the first commit is what asks for it.
        request(out, id::SURFACE, wl_surface::request::COMMIT, &[], &[]);
        Ok(())
    }

    /// Whether there is a buffer the compositor is not reading to draw in.
    ///
    /// A frame drawn into a buffer the compositor still holds is a frame torn
    /// across the screen, so a wallpaper whose buffers are both out waits: the
    /// next frame is not due for a period anyway.
    fn has_a_free_buffer(&self) -> bool {
        self.busy.iter().take(self.buffers).any(|with| !with)
    }

    /// Show the next frame of a moving wallpaper, where that is what this is.
    fn advance(&mut self) {
        if let Shown::Moving(movie) = &mut self.shown {
            movie.advance();
        }
    }

    fn draw(&mut self, out: &mut Writer) -> Result<(), String> {
        if !self.acked || self.width <= 0 || self.height <= 0 {
            return Ok(());
        }
        // The window's size is in logical pixels; the buffer is in the
        // screen's own, which on a monitor at `scale = 2` is twice as many
        // each way. `set_buffer_scale` is what tells the compositor that the
        // larger buffer is the same window and not a larger one.
        let scale = self.scale.max(1);
        let (width, height) = (
            self.width.saturating_mul(scale),
            self.height.saturating_mul(scale),
        );
        let stride = width.saturating_mul(4);
        let len = usize::try_from(stride.saturating_mul(height))
            .map_err(|_| "a window too large to draw".to_owned())?;

        // A pool is made once and grown if the window does; the buffer is
        // made afresh each size, as a client that never keeps two.
        // A pattern change changes the format, and a `wl_buffer` cannot be
        // reinterpreted: it is made afresh for a new format as for a new
        // size.
        let format = match self.shown.picture() {
            Some(_) => compositor_render::Format::Xrgb8888.wl_shm(),
            None => self.pattern.format().wl_shm(),
        };
        let fresh = self.shared.is_none()
            || self.buffer_size != (width, height)
            || self.buffer_format != Some(format);
        // The pool holds every buffer this client keeps, one after another.
        let whole = len
            .checked_mul(self.buffers)
            .ok_or("a window too large to draw")?;
        if fresh {
            // The ids are fixed, so the old objects have to go before the
            // new ones can take their numbers. A server is right to refuse a
            // `new_id` that is already live, and this one does.
            if self.shared.is_some() {
                for buffer in BUFFERS.iter().take(self.buffers) {
                    request(out, *buffer, core::wl_buffer::request::DESTROY, &[], &[]);
                }
                request(out, id::POOL, wl_shm_pool::request::DESTROY, &[], &[]);
                // The mapping goes with them: a pool destroyed while the
                // compositor still reads a buffer from it keeps its memory
                // until that buffer is gone, which it now is.
                self.shared = None;
                self.busy = [false; BUFFERS.len()];
                self.filled = [false; BUFFERS.len()];
            }
            let shared = Shared::new(whole).map_err(|error| format!("shared memory: {error}"))?;
            let fd = shared.as_raw_fd();
            self.shared = Some(shared);
            request_with_fd(
                out,
                id::SHM,
                wl_shm::request::CREATE_POOL,
                &[ArgType::NewId, ArgType::Fd, ArgType::Int],
                &[
                    Arg::NewId(id::POOL),
                    Arg::Fd(Fd(fd)),
                    Arg::Int(i32::try_from(whole).unwrap_or(i32::MAX)),
                ],
            );
            for (slot, buffer) in BUFFERS.iter().enumerate().take(self.buffers) {
                let at = slot.checked_mul(len).ok_or("a window too large to draw")?;
                request(
                    out,
                    id::POOL,
                    wl_shm_pool::request::CREATE_BUFFER,
                    &[
                        ArgType::NewId,
                        ArgType::Int,
                        ArgType::Int,
                        ArgType::Int,
                        ArgType::Int,
                        ArgType::Uint,
                    ],
                    &[
                        Arg::NewId(*buffer),
                        Arg::Int(i32::try_from(at).unwrap_or(0)),
                        Arg::Int(width),
                        Arg::Int(height),
                        Arg::Int(stride),
                        Arg::Uint(format),
                    ],
                );
            }
            self.buffer_size = (width, height);
            self.buffer_format = Some(format);
        }

        // Whichever buffer the compositor is not reading. With one buffer
        // that is the one either way, which is right for a client that draws
        // when it is configured and not otherwise: nothing else is coming,
        // so there is nothing for a torn frame to be replaced by.
        let slot = self
            .busy
            .iter()
            .take(self.buffers)
            .position(|with| !with)
            .unwrap_or(0);
        let buffer = *BUFFERS.get(slot).unwrap_or(&id::BUFFER);

        // The pattern itself, drawn by `src/user/system/linux/compositor/render` so the client and
        // the expected image are made from one piece of code.
        let size = (
            u32::try_from(width).unwrap_or(0),
            u32::try_from(height).unwrap_or(0),
        );
        let at = slot.checked_mul(len).ok_or("a window too large to draw")?;
        if let Shown::Moving(movie) = &self.shown {
            // A frame of a wallpaper that moves is not scaled whole: the
            // rows the video changed are scaled into the kept frame, and
            // the buffer takes the rows it does not have -- those, and
            // whatever earlier frames changed while the other buffer was
            // being shown. Whole only when there is no kept frame of this
            // size, and into every buffer when the pool is new.
            let rows = usize::try_from(height).unwrap_or(0);
            let whole = self.scaled.len() != len;
            if whole {
                self.scaled.clear();
                self.scaled.resize(len, 0);
            }
            let changed: Vec<(i32, i32)> = if whole {
                vec![(0, height)]
            } else {
                movie.damage(size.0, size.1)
            };
            movie
                .shown()
                .cover_rows_into(size.0, size.1, &changed, &mut self.scaled);
            for stale in &mut self.stale {
                if whole || fresh || stale.len() != rows {
                    stale.clear();
                    stale.resize(rows, true);
                } else {
                    mark_stale(stale, &changed);
                }
            }
            if let (Some(shared), Some(stale)) = (self.shared.as_mut(), self.stale.get_mut(slot)) {
                let room = shared.bytes_mut().get_mut(at..).unwrap_or(&mut []);
                copy_stale(
                    &self.scaled,
                    stale,
                    room,
                    usize::try_from(stride).unwrap_or(0),
                );
            }
        } else {
            let pixels = match self.shown.picture() {
                Some(picture) => picture.cover(size.0, size.1),
                None => self.pattern.draw(size.0, size.1),
            };
            if let Some(shared) = self.shared.as_mut() {
                let room = shared.bytes_mut();
                let take = pixels.len().min(len);
                let upto = at.checked_add(take).ok_or("a window too large to draw")?;
                if let (Some(to), Some(from)) = (room.get_mut(at..upto), pixels.get(..take)) {
                    to.copy_from_slice(from);
                }
            }
        }
        if let Some(with) = self.busy.get_mut(slot) {
            *with = true;
        }

        request(
            out,
            id::SURFACE,
            wl_surface::request::SET_BUFFER_SCALE,
            &[ArgType::Int],
            &[Arg::Int(scale)],
        );
        request(
            out,
            id::SURFACE,
            wl_surface::request::ATTACH,
            &[
                ArgType::Object { nullable: true },
                ArgType::Int,
                ArgType::Int,
            ],
            &[Arg::Object(buffer), Arg::Int(0), Arg::Int(0)],
        );
        // What changed. A wallpaper that moves knows which of its rows the
        // last frame did not have, and saying so is the difference between
        // the compositor blurring what is behind every translucent window on
        // the screen and blurring a band. Everything else changed all of
        // itself: it drew because it was configured.
        //
        // A buffer this client has not drawn into since it was made holds
        // nothing, so the first frame in each is damaged whole whatever the
        // video says changed.
        let bands = match &self.shown {
            Shown::Moving(movie) if self.filled.get(slot).copied().unwrap_or(false) => {
                let bands = movie.damage(size.0, size.1);
                // Past a point a list of bands costs more to send and to
                // walk than the screen costs to redraw.
                if bands.len() > MOST_BANDS {
                    Vec::new()
                } else {
                    bands
                }
            }
            _ => Vec::new(),
        };
        if bands.is_empty() {
            request(
                out,
                id::SURFACE,
                wl_surface::request::DAMAGE_BUFFER,
                &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
                &[Arg::Int(0), Arg::Int(0), Arg::Int(width), Arg::Int(height)],
            );
        } else {
            self.damaged = bands.iter().map(|band| band.1.max(0)).sum();
            for (top, tall) in bands {
                request(
                    out,
                    id::SURFACE,
                    wl_surface::request::DAMAGE_BUFFER,
                    &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
                    &[Arg::Int(0), Arg::Int(top), Arg::Int(width), Arg::Int(tall)],
                );
            }
        }
        if let Some(filled) = self.filled.get_mut(slot) {
            *filled = true;
        }
        // Ask to be told when this frame reaches the screen, which is what
        // the next one waits for. A wallpaper that moves asks every frame;
        // anything else only for its first, and only when the program says
        // where to announce that it has been drawn (`announce_when_drawn`,
        // `run_announced_on`): otherwise the answer would never be read.
        let announcing = self.drawn == 0 && self.announce.is_some();
        if (matches!(self.shown, Shown::Moving(_)) || announcing) && !self.awaiting {
            request(
                out,
                id::SURFACE,
                wl_surface::request::FRAME,
                &[ArgType::NewId],
                &[Arg::NewId(id::FRAME)],
            );
            self.awaiting = true;
        }
        request(out, id::SURFACE, wl_surface::request::COMMIT, &[], &[]);
        self.drawn = self.drawn.saturating_add(1);
        Ok(())
    }
}

/// Say a line on the standard output, flushed: this client runs as a child
/// of the compositor with the console for its output, and a line held in a
/// buffer is a line a test never sees.
fn say(line: &str) {
    use std::io::Write as _;

    let mut out = io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Queue one request.
fn request(
    out: &mut Writer,
    sender: ObjectId,
    opcode: u16,
    signature: &'static [ArgType],
    args: &[Arg<'_>],
) {
    // The only way this fails is a message longer than the format allows,
    // which none of this client's is.
    let _ = out.write(sender, opcode, signature, args);
}

/// Queue one request that carries a descriptor.
fn request_with_fd(
    out: &mut Writer,
    sender: ObjectId,
    opcode: u16,
    signature: &'static [ArgType],
    args: &[Arg<'_>],
) {
    request(out, sender, opcode, signature, args);
}

/// Send everything queued.
fn flush(connection: &mut Connection, out: &mut Writer) -> Result<(), String> {
    if out.is_empty() {
        // What a full socket left queued still has to go.
        return connection
            .flush()
            .map_err(|error| format!("writing: {error:?}"));
    }
    let (bytes, fds) = out.take();
    connection
        .send(&bytes, &fds)
        .map_err(|error| format!("writing: {error:?}"))
}

/// So the module compiles on a host without the descriptor plumbing.
const _: fn() -> io::Result<()> = || Ok(());

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Movie, Picture, spans};

    /// A picture's file: the header, and then `pixels`.
    fn file(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = Picture::MAGIC.to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(pixels);
        bytes
    }

    /// A 4x2 picture whose every pixel says where it is: its column in the
    /// first byte and its row in the second.
    fn numbered() -> Picture {
        let pixels: Vec<u8> = (0..2u8)
            .flat_map(|y| (0..4u8).flat_map(move |x| [x, y, 0, 0xFF]))
            .collect();
        Picture::parse(&file(4, 2, &pixels)).expect("a picture")
    }

    /// The column and row each pixel of a buffer came from.
    fn places(buffer: &[u8]) -> Vec<(u8, u8)> {
        buffer
            .chunks_exact(4)
            .map(|pixel| (pixel[0], pixel[1]))
            .collect()
    }

    #[test]
    fn a_picture_is_its_header_and_exactly_its_pixels() {
        assert!(Picture::parse(&file(2, 1, &[0; 8])).is_ok());
        for (what, bytes) in [
            ("no header", b"FXWALL".to_vec()),
            (
                "another file",
                file(2, 1, &[0; 8]).iter().map(|byte| byte ^ 1).collect(),
            ),
            ("a row short", file(2, 2, &[0; 8])),
            ("a row over", file(2, 1, &[0; 16])),
            ("no size", file(0, 0, &[])),
        ] {
            assert!(
                Picture::parse(&bytes).is_err(),
                "{what} was taken for a picture"
            );
        }
    }

    /// Made for the screen it is on, a picture is copied; on a narrower one
    /// it keeps its height and loses its sides evenly, and on a shorter one
    /// the other way about: it covers, and is never stretched.
    #[test]
    fn a_picture_covers_the_screen_and_is_cut_evenly() {
        let picture = numbered();
        assert_eq!(
            places(&picture.cover(4, 2)),
            [
                (0, 0),
                (1, 0),
                (2, 0),
                (3, 0),
                (0, 1),
                (1, 1),
                (2, 1),
                (3, 1)
            ]
        );
        // Two wide and two tall: the middle two columns of the four.
        assert_eq!(
            places(&picture.cover(2, 2)),
            [(1, 0), (2, 0), (1, 1), (2, 1)]
        );
        // Eight wide and two tall: every column twice, and the middle row of
        // what is now a picture twice as tall -- one of the two it has.
        let wide = places(&picture.cover(8, 2));
        assert_eq!(wide.len(), 16);
        assert!(
            wide.iter()
                .map(|place| place.0)
                .eq([0, 0, 1, 1, 2, 2, 3, 3, 0, 0, 1, 1, 2, 2, 3, 3])
        );
        // Twice the size each way is every pixel four times over.
        let doubled = places(&picture.cover(8, 4));
        assert_eq!(doubled.first(), Some(&(0, 0)));
        assert_eq!(doubled.last(), Some(&(3, 1)));
        assert_eq!(doubled.len(), 32);
    }

    /// Four 64x64 AV1 frames, made by ffmpeg's `testsrc` filter. This lives
    /// in the repository so both the host tests and the QEMU gate need no
    /// encoder installed.
    const VIDEO: &[u8] = include_bytes!("../tests/fixtures/tiny.ivf");

    /// Covering every row of a buffer a band at a time is the same bytes
    /// as covering it whole, and a band touches only its own rows: what
    /// lets a wallpaper that moves scale the rows a frame changed and no
    /// others.
    #[test]
    fn rows_covered_in_bands_are_the_rows_of_the_whole_cover() {
        let pixels: Vec<u8> = (0..5 * 3 * 4).map(|byte| byte as u8).collect();
        let picture = Picture::parse(&file(5, 3, &pixels)).expect("a picture");
        for (width, height) in [(13, 7), (5, 3), (2, 9)] {
            let whole = picture.cover(width, height);
            let stride = width as usize * 4;
            let mut banded = vec![0xAA; whole.len()];
            let top = height as i32 / 2;
            picture.cover_rows_into(width, height, &[(0, 1), (top, 2)], &mut banded);
            for row in 0..height as usize {
                let (from, to) = (row * stride, (row + 1) * stride);
                let covered = row == 0 || (row as i32 >= top && (row as i32) < top + 2);
                if covered {
                    assert_eq!(
                        banded[from..to],
                        whole[from..to],
                        "{width}x{height} row {row}"
                    );
                } else {
                    assert!(
                        banded[from..to].iter().all(|byte| *byte == 0xAA),
                        "{width}x{height} row {row} was touched"
                    );
                }
            }
            let mut all = vec![0; whole.len()];
            picture.cover_rows_into(width, height, &[(0, height as i32)], &mut all);
            assert_eq!(all, whole, "{width}x{height} covered whole");
        }
    }

    #[test]
    fn a_video_plays_its_frames_in_turn_and_begins_again() {
        let mut movie = Movie::parse(VIDEO).expect("a video");
        assert_eq!(movie.frames(), 4);
        assert_eq!(movie.period(), Duration::from_millis(100));
        let first = movie.shown().pixels.clone();
        assert!(first.windows(4).any(|pixels| pixels != &first[..4]));
        // The fixture is an animated test pattern, so each decoded packet
        // changes the shown picture. At the end a new decoder starts at the
        // first keyframe and reproduces the first frame exactly.
        movie.advance();
        assert_ne!(movie.shown().pixels, first);
        movie.advance();
        movie.advance();
        movie.advance();
        assert_eq!(movie.shown().pixels, first);
    }

    /// Damage comes from the rows that differ between decoded AV1 frames,
    /// then follows the same cover scale as the pixels.
    #[test]
    fn a_frames_damage_is_the_rows_it_changed_through_the_scale() {
        let mut movie = Movie::parse(VIDEO).expect("a video");
        assert_eq!(movie.damage(64, 64), [(0, 64)]);
        movie.advance();
        let source = movie.damage(64, 64);
        let scaled = movie.damage(128, 128);
        assert!(!source.is_empty());
        assert_eq!(
            scaled.iter().map(|(_, tall)| *tall).sum::<i32>(),
            source.iter().map(|(_, tall)| *tall * 2).sum::<i32>()
        );
    }

    /// Bands are the runs of changed rows, and a row the scale never reads is
    /// in none of them.
    #[test]
    fn damage_is_the_runs_of_the_rows_that_changed() {
        assert_eq!(spans([].into_iter(), 0), []);
        assert_eq!(spans([false, false].into_iter(), 2), []);
        assert_eq!(spans([true, true].into_iter(), 2), [(0, 2)]);
        assert_eq!(
            spans([true, false, true, true, false].into_iter(), 5),
            [(0, 1), (2, 2)]
        );
        // A run that reaches the last row is closed by the end of the rows.
        assert_eq!(spans([false, true].into_iter(), 2), [(1, 1)]);
    }

    #[test]
    fn a_video_is_ivf_and_all_of_its_packets_decode() {
        assert!(Movie::parse(VIDEO).is_ok());
        for (what, bytes) in [
            ("no header", b"DKI".as_slice()),
            ("wrong codec", b"DKIF\0\0\x20\0VP90".as_slice()),
            ("truncated packet", &VIDEO[..VIDEO.len() - 1]),
        ] {
            assert!(Movie::parse(bytes).is_err(), "{what} was taken for a video");
        }
    }
}
