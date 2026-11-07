//! An AV1 video, demuxed from IVF and decoded by rav1d.
//!
//! A moving wallpaper was run-length frames in a format of this crate's own
//! ([`super::client::Movie`]'s old `FXVID`), which a long clip runs to
//! gigabytes of -- an initramfs is built into the kernel here, so the file
//! is the image. A real codec is the answer, and the one the desktop's
//! `mpvpaper` line already implies: AV1, which `ffmpeg` on the machine that
//! makes the wallpaper writes small, and which rav1d -- dav1d ported to
//! Rust, so no C and, with its `asm` feature off, no nasm -- decodes in the
//! guest (`docs/GPU.md` §3.6).
//!
//! Two small things live here: an IVF demuxer, because IVF is a 32-byte
//! header and then a length and a timestamp before each frame, and a thin
//! wrapper over rav1d's dav1d C API, because that API is a handful of
//! `unsafe extern "C"` calls and a few structs that are opaque enough that
//! they must be the crate's own rather than redeclared here.
//!
//! The decoder is driven one temporal unit at a time and single-threaded:
//! the wallpaper is small and slow (ten frames a second of a quarter-screen
//! picture), so the frame budget is the compositor's, not the decoder's, and
//! one thread keeps the picture order simple -- a frame comes out for each
//! that goes in.

use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_default_settings, dav1d_get_picture, dav1d_open,
    dav1d_picture_unref, dav1d_send_data,
};

/// What `dav1d` returns when it has taken all it can and has no picture yet:
/// feed it more, or drain what it has. `Dav1dResult` is `0` for success and
/// `-errno` otherwise.
const EAGAIN: i32 = -libc::EAGAIN;

/// dav1d's code for a 4:2:0 planar picture, the one layout a video wallpaper
/// is (`Dav1dPixelLayout`, `I420 = 1`).
const LAYOUT_I420: u32 = 1;

/// A decoded frame, converted to the `XRGB8888` rows the rest of the client
/// draws: four bytes a pixel, blue first, no padding between rows.
pub(crate) struct Frame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

/// An IVF file's frames, still AV1, with the shape its header gives.
pub(crate) struct Ivf {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Frames a second, from the header's timebase (denominator over
    /// numerator), which is what a still period cannot carry.
    pub(crate) rate: u32,
    /// Each temporal unit, in order.
    pub(crate) packets: Vec<Vec<u8>>,
}

/// Read an IVF file: `DKIF`, a 32-byte header, then for each frame a
/// little-endian `u32` length, an eight-byte timestamp, and that many bytes.
pub(crate) fn demux_ivf(bytes: &[u8]) -> Result<Ivf, String> {
    let header = bytes
        .get(..32)
        .ok_or("a video shorter than its IVF header")?;
    if header.get(..4) != Some(b"DKIF") {
        return Err("not a video: the file does not begin DKIF".to_owned());
    }
    if header.get(8..12) != Some(b"AV01") {
        return Err("a video that is not AV1".to_owned());
    }
    let width = u32::from(u16_at(header, 12).ok_or("an IVF header without a width")?);
    let height = u32::from(u16_at(header, 14).ok_or("an IVF header without a height")?);
    let den = u32_at(header, 16).ok_or("an IVF header without a timebase")?;
    let num = u32_at(header, 20).ok_or("an IVF header without a timebase")?;
    if width == 0 || height == 0 {
        return Err(format!("a video {width} by {height}"));
    }
    // The timebase is seconds per tick as num/den, and a frame is one tick,
    // so frames a second is den/num. A stream that does not say rounds to a
    // sane wallpaper rate rather than dividing by zero.
    let rate = den.checked_div(num).unwrap_or(0);
    let rate = rate.clamp(1, 60);

    let mut packets = Vec::new();
    let mut at = 32;
    while at < bytes.len() {
        let frame_end = at.checked_add(12).ok_or("a video too large to read")?;
        let frame = bytes
            .get(at..frame_end)
            .ok_or("a video that ends inside a frame header")?;
        let len = usize::try_from(u32_at(frame, 0).ok_or("a video frame without a length")?)
            .map_err(|_| "a video too large to read")?;
        at = frame_end;
        let payload = bytes
            .get(at..at.checked_add(len).ok_or("a video too large to read")?)
            .ok_or("a video that ends inside a frame")?;
        packets.push(payload.to_vec());
        at += len;
    }
    if packets.is_empty() {
        return Err("a video with no frames".to_owned());
    }
    Ok(Ivf {
        width,
        height,
        rate,
        packets,
    })
}

/// A little-endian `u16` at `at`, if `bytes` holds one.
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at.checked_add(2)?)?
        .try_into()
        .ok()
        .map(u16::from_le_bytes)
}

/// A little-endian `u32` at `at`, if `bytes` holds one.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at.checked_add(4)?)?
        .try_into()
        .ok()
        .map(u32::from_le_bytes)
}

/// A live rav1d decoder and the frames it has produced but not yet handed
/// back.
pub(crate) struct Decoder {
    context: Dav1dContext,
    ready: VecDeque<Frame>,
}

impl Decoder {
    /// Open a single-threaded decoder.
    pub(crate) fn new() -> Result<Self, String> {
        // Zeroed first so every reserved byte and unset pointer is null, then
        // filled with dav1d's own defaults; a zeroed `Dav1dSettings` is a
        // valid one, as its fields are integers, `Option` pointers and
        // `c_uint` codes.
        let mut settings = MaybeUninit::<Dav1dSettings>::zeroed();
        let settings_ptr =
            NonNull::new(settings.as_mut_ptr()).ok_or("no room for decoder settings")?;
        // SAFETY: the pointer is to our own stack slot, which is writable.
        unsafe { dav1d_default_settings(settings_ptr) };
        // SAFETY: dav1d_default_settings wrote every field.
        let mut settings = unsafe { settings.assume_init() };
        // One thread: a picture out for each in, no reordering to drain.
        settings.n_threads = 1;
        settings.max_frame_delay = 1;

        let mut context: Option<Dav1dContext> = None;
        // SAFETY: both pointers are to our own writable stack slots, and the
        // settings are fully initialised.
        let result = unsafe { dav1d_open(NonNull::new(&mut context), NonNull::new(&mut settings)) };
        if result.0 != 0 {
            return Err(format!(
                "a decoder that would not open: dav1d error {}",
                -result.0
            ));
        }
        let context = context.ok_or("a decoder that opened to nothing")?;
        Ok(Self {
            context,
            ready: VecDeque::new(),
        })
    }

    /// Feed one temporal unit, draining every picture it makes ready into
    /// [`Self::ready`].
    pub(crate) fn feed(&mut self, unit: &[u8]) -> Result<(), String> {
        let len = unit.len();
        let mut data = MaybeUninit::<Dav1dData>::zeroed();
        // SAFETY: the pointer is to our own writable stack slot.
        let buffer = unsafe { dav1d_data_create(NonNull::new(data.as_mut_ptr()), len) };
        if buffer.is_null() {
            return Err("no room for a frame's bytes".to_owned());
        }
        // SAFETY: dav1d_data_create returned a buffer of `len` bytes and, on
        // success, initialised `data`.
        unsafe {
            std::ptr::copy_nonoverlapping(unit.as_ptr(), buffer, len);
        }
        // SAFETY: dav1d_data_create initialised `data` when it returned its
        // non-null backing buffer above.
        let mut data = unsafe { data.assume_init() };

        // Send, draining ready pictures whenever dav1d says it is full, until
        // the whole unit has been taken.
        loop {
            // SAFETY: the context is live and `data` points to our slot.
            let result = unsafe { dav1d_send_data(Some(self.context), NonNull::new(&mut data)) };
            if result.0 != 0 && result.0 != EAGAIN {
                return Err(format!("a frame dav1d would not take: error {}", -result.0));
            }
            self.drain()?;
            if data.sz == 0 {
                break;
            }
        }
        Ok(())
    }

    /// Take a frame dav1d has finished, if there is one.
    pub(crate) fn take(&mut self) -> Option<Frame> {
        self.ready.pop_front()
    }

    /// Move every ready picture out of dav1d and into [`Self::ready`].
    fn drain(&mut self) -> Result<(), String> {
        loop {
            let mut picture = MaybeUninit::<Dav1dPicture>::zeroed();
            // SAFETY: the context is live and the pointer is to our slot.
            let result = unsafe {
                dav1d_get_picture(Some(self.context), NonNull::new(picture.as_mut_ptr()))
            };
            if result.0 == EAGAIN {
                return Ok(());
            }
            if result.0 != 0 {
                return Err(format!(
                    "a picture dav1d would not give: error {}",
                    -result.0
                ));
            }
            // SAFETY: get_picture returned success, so `picture` is a valid
            // Dav1dPicture that we own until we unref it.
            let mut picture = unsafe { picture.assume_init() };
            let frame = convert(&picture);
            // SAFETY: `picture` is the one just returned; unref hands its
            // planes back to dav1d.
            unsafe { dav1d_picture_unref(NonNull::new(&mut picture)) };
            self.ready.push_back(frame?);
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let mut context = Some(self.context);
        // SAFETY: the context is live and has not been closed before; close
        // consumes it, and this runs once.
        unsafe { dav1d_close(NonNull::new(&mut context)) };
    }
}

/// Turn a 4:2:0 8-bit `Dav1dPicture` into `XRGB8888` rows.
///
/// BT.709 limited-range coefficients in fixed point: a wallpaper from a
/// modern clip is BT.709, and it is behind a blur and a translucent desktop,
/// so the last bit of colour accuracy is not what is on the screen.
fn convert(picture: &Dav1dPicture) -> Result<Frame, String> {
    if picture.p.layout != LAYOUT_I420 || picture.p.bpc != 8 {
        return Err("a video that is not 8-bit 4:2:0".to_owned());
    }
    let width = usize::try_from(picture.p.w).map_err(|_| "a frame of no width")?;
    let height = usize::try_from(picture.p.h).map_err(|_| "a frame of no height")?;
    let (Some(y_plane), Some(u_plane), Some(v_plane)) =
        (picture.data[0], picture.data[1], picture.data[2])
    else {
        return Err("a frame missing a plane".to_owned());
    };
    let y_stride = picture.stride[0];
    let c_stride = picture.stride[1];
    let y_plane = y_plane.as_ptr().cast::<u8>();
    let u_plane = u_plane.as_ptr().cast::<u8>();
    let v_plane = v_plane.as_ptr().cast::<u8>();

    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("a frame too large to hold")?;
    let mut pixels = vec![0u8; bytes];
    for row in 0..height {
        let y_row = (row as isize) * y_stride;
        let c_row = ((row / 2) as isize) * c_stride;
        let Some(pixels_row) = pixels.get_mut(row.saturating_mul(width * 4)..) else {
            return Err("a frame too large to hold".to_owned());
        };
        for (column, pixel) in pixels_row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .take(width)
            .enumerate()
        {
            // SAFETY: every offset is inside the plane dav1d says it wrote:
            // luma is `height` rows of `y_stride`, chroma half that each way.
            let y = unsafe { *y_plane.wrapping_offset(y_row + column as isize) };
            // SAFETY: every offset is inside the chroma plane dav1d wrote.
            let u = unsafe { *u_plane.wrapping_offset(c_row + (column / 2) as isize) };
            // SAFETY: every offset is inside the chroma plane dav1d wrote.
            let v = unsafe { *v_plane.wrapping_offset(c_row + (column / 2) as isize) };
            let (b, g, r) = yuv_to_bgr(y, u, v);
            pixel.copy_from_slice(&[b, g, r, 0xFF]);
        }
    }
    Ok(Frame {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

/// One pixel, BT.709 limited range, `Y`, `Cb`, `Cr` to blue, green, red.
fn yuv_to_bgr(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = i32::from(y) - 16;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let clamp = |value: i32| value.clamp(0, 255) as u8;
    let r = clamp((298 * c + 459 * e + 128) >> 8);
    let g = clamp((298 * c - 55 * d - 136 * e + 128) >> 8);
    let b = clamp((298 * c + 541 * d + 128) >> 8);
    (b, g, r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny AV1 clip made on the build host: four frames of a 64x64
    /// `testsrc` pattern, `ffmpeg -c:v libaom-av1`.
    const TINY: &[u8] = include_bytes!("../tests/fixtures/tiny.ivf");

    #[test]
    fn the_ivf_header_gives_the_clip_its_shape() {
        let ivf = demux_ivf(TINY).expect("a valid IVF");
        assert_eq!((ivf.width, ivf.height), (64, 64));
        assert_eq!(ivf.rate, 10);
        assert_eq!(ivf.packets.len(), 4);
    }

    #[test]
    fn a_file_that_does_not_begin_dkif_is_refused() {
        assert!(demux_ivf(b"not an ivf file at all, but long enough..").is_err());
    }

    #[test]
    fn every_frame_of_the_clip_decodes_to_its_size() {
        let ivf = demux_ivf(TINY).expect("a valid IVF");
        let mut decoder = Decoder::new().expect("a decoder");
        let mut decoded = 0;
        // Feed the whole clip; a single-threaded decoder gives a frame back
        // for each unit, so all four come out.
        for unit in &ivf.packets {
            decoder.feed(unit).expect("a frame decodes");
            while let Some(frame) = decoder.take() {
                assert_eq!((frame.width, frame.height), (64, 64));
                assert_eq!(frame.pixels.len(), 64 * 64 * 4);
                decoded += 1;
            }
        }
        assert_eq!(decoded, 4, "every frame came back");
    }

    #[test]
    fn the_clip_is_not_one_flat_colour() {
        // testsrc is a colour-bar pattern, so a decoded frame must hold more
        // than one pixel value -- proof the planes were read, not zeroed.
        let ivf = demux_ivf(TINY).expect("a valid IVF");
        let mut decoder = Decoder::new().expect("a decoder");
        decoder.feed(&ivf.packets[0]).expect("a frame decodes");
        let frame = loop {
            if let Some(frame) = decoder.take() {
                break frame;
            }
            // A reordering decoder might need a second unit; this one should
            // not, but keep the test honest if the fixture changes.
            decoder.feed(&ivf.packets[1]).expect("a frame decodes");
        };
        let first = &frame.pixels[0..4];
        assert!(
            frame
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel != first),
            "a decoded frame was one flat colour"
        );
    }
}
