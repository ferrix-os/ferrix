//! A client's GPU buffer, sampled where it lies: the GPU painter on
//! Ferrix's patched `virgl_test_server` (tools/common/fetch/fetch-virgl-server.sh),
//! given a dmabuf and no pixels, as it is given a buffer in video memory.
//!
//! The dmabuf is the host's own (`/dev/udmabuf` over a memfd), so what is
//! in it is known and the frame can be judged. A host without the patched
//! server, without `/dev/udmabuf`, or whose EGL will not import one runs
//! none of this, and says so.

use std::os::fd::{AsFd, OwnedFd};

use compositor_render::gpu::Canvas;
use compositor_render::{Color, Damage, Format, Painter, Rect, Rounding, Surface};
use compositor_virgl::vtest::Vtest;

/// The buffer's side in pixels, and its stride in bytes.
const SIDE: u32 = 64;
const STRIDE: u32 = 256;

/// Ferrix's patched server: `FERRIX_VIRGL_SERVER_BIN`, or where the fetch
/// script puts it.
fn patched_server() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("FERRIX_VIRGL_SERVER_BIN")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                std::path::PathBuf::from(home)
                    .join(".local/share/ferrix/virgl-server/out/usr/bin/virgl_test_server")
            })
        })?;
    path.is_file().then_some(path)
}

/// What the buffer holds at (`x`, `y`): blue its column, green its row.
fn texel(x: u32, y: u32) -> [u8; 4] {
    #[expect(clippy::cast_possible_truncation, reason = "a side of 64, times three")]
    let (blue, green) = ((x * 3) as u8, (y * 3) as u8);
    [blue, green, 0x40, 0xff]
}

/// A dmabuf of system memory holding [`texel`]'s picture, from the host's
/// `/dev/udmabuf` over a sealed memfd: `None` where the host has none to
/// give.
fn udmabuf() -> Option<OwnedFd> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    let len = u64::from(STRIDE * SIDE).next_multiple_of(4096);
    // SAFETY: a fresh descriptor from a constant name.
    let raw = unsafe {
        libc::memfd_create(
            c"udmabuf".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return None;
    }
    // SAFETY: just made, owned by nothing else.
    let memory = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: a descriptor this test holds.
    if unsafe { libc::ftruncate(raw, i64::try_from(len).ok()?) } != 0 {
        return None;
    }
    let pixels: Vec<u8> = (0..SIDE)
        .flat_map(|y| (0..SIDE).flat_map(move |x| texel(x, y)))
        .collect();
    let mut file = std::fs::File::from(memory.try_clone().ok()?);
    file.write_all(&pixels).ok()?;
    // SAFETY: as above; only the seals change.
    if unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) } != 0 {
        return None;
    }
    let device = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/udmabuf")
        .ok()?;
    // `struct udmabuf_create`: memfd, flags, offset, size.
    let mut create = [0_u8; 24];
    create
        .get_mut(..4)?
        .copy_from_slice(&u32::try_from(memory.as_raw_fd()).ok()?.to_le_bytes());
    create.get_mut(4..8)?.copy_from_slice(&1_u32.to_le_bytes()); // UDMABUF_FLAGS_CLOEXEC
    create.get_mut(16..24)?.copy_from_slice(&len.to_le_bytes());
    /// `UDMABUF_CREATE`: `_IOW('u', 0x42, struct udmabuf_create)`.
    const UDMABUF_CREATE: libc::c_ulong = 0x4018_7542;
    // SAFETY: the request's own structure, live for the call.
    let made = unsafe { libc::ioctl(device.as_raw_fd(), UDMABUF_CREATE, create.as_ptr()) };
    if made < 0 {
        return None;
    }
    // SAFETY: the descriptor the ioctl just made, owned by nothing else.
    Some(unsafe { OwnedFd::from_raw_fd(made) })
}

/// A canvas on the patched server, and a dmabuf to show on it.
#[expect(clippy::print_stderr, reason = "a test that did not run says so")]
fn canvas_and_buffer(name: &str, size: (u32, u32)) -> Option<(Canvas<Vtest>, OwnedFd)> {
    let Some(program) = patched_server() else {
        eprintln!("{name}: no patched virgl_test_server built; skipped");
        return None;
    };
    let Ok(Some(mut server)) = Vtest::start_program(&program, name, 2) else {
        eprintln!("{name}: the patched server would not start; skipped");
        return None;
    };
    if !server.can_import().unwrap_or(false) {
        eprintln!("{name}: the server takes no dmabufs; skipped");
        return None;
    }
    let Some(dmabuf) = udmabuf() else {
        eprintln!("{name}: no /dev/udmabuf to make a dmabuf with; skipped");
        return None;
    };
    let canvas = Canvas::new(server, size.0, size.1).ok()?;
    Some((canvas, dmabuf))
}

/// The pixel at (`x`, `y`) of a readback `wide` pixels wide.
fn pixel(data: &[u8], wide: u32, x: u32, y: u32) -> Option<&[u8]> {
    let at = ((y * wide + x) * 4) as usize;
    data.get(at..at + 3)
}

/// The buffer, whole and in part, pixel for pixel and stretched, is the
/// picture its memory holds -- with no pixel of it ever handed to the
/// painter.
#[test]
#[expect(clippy::print_stderr, reason = "a test that did not run says so")]
fn a_buffer_without_pixels_is_drawn_from_its_import() {
    let (wide, tall) = (160_u32, 96_u32);
    let Some((mut gpu, dmabuf)) = canvas_and_buffer("canvas-import", (wide, tall)) else {
        return;
    };
    let whole = Damage::full(wide, tall);
    let surface = Surface::without_pixels(SIDE, SIDE, STRIDE, Format::Xrgb8888)
        .expect("a surface")
        .named(11)
        .on_device(dmabuf.as_fd(), 42)
        .laid_out(0, 0);
    if !gpu.imports(&surface) {
        eprintln!("canvas-import: this host's EGL would not import a udmabuf; skipped");
        return;
    }
    Painter::clear(&mut gpu, Color(0xff00_0000), &whole);
    // The whole buffer at (0, 0).
    Painter::composite(&mut gpu, &surface, Rect::new(0, 0, 64, 64), &whole);
    // Its 32x24 part from (16, 8) at (72, 4), pixel for pixel.
    let part = surface.cropped(16, 8, 32, 24).expect("a part");
    Painter::composite(&mut gpu, &part, Rect::new(72, 4, 32, 24), &whole);
    // The same part at twice the size at (72, 40), nearest.
    Painter::composite_scaled(
        &mut gpu,
        &part,
        Rect::new(72, 40, 64, 48),
        Rounding::none(),
        1.0,
        true,
        &whole,
    );
    gpu.finish().expect("the frame was drawn");
    let drawn = gpu
        .read(Rect::new(0, 0, wide.into(), tall.into()))
        .expect("read back");
    let is = |x: u32, y: u32, wanted: [u8; 4], what: &str| {
        assert_eq!(
            pixel(&drawn, wide, x, y),
            wanted.get(..3),
            "{what} at ({x}, {y})"
        );
    };
    for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63), (20, 41)] {
        is(x, y, texel(x, y), "the whole buffer");
    }
    for (x, y) in [(0, 0), (31, 0), (0, 23), (31, 23), (7, 13)] {
        is(72 + x, 4 + y, texel(16 + x, 8 + y), "the part");
    }
    for (x, y) in [(0, 0), (31, 23), (5, 9)] {
        is(
            72 + 2 * x,
            40 + 2 * y,
            texel(16 + x, 8 + y),
            "the part, doubled",
        );
        is(
            73 + 2 * x,
            41 + 2 * y,
            texel(16 + x, 8 + y),
            "the part, doubled",
        );
    }
    // And nothing of the buffer outside the part's rectangle.
    is(71, 4, [0, 0, 0, 0xff], "left of the part");
    is(104, 4, [0, 0, 0, 0xff], "right of the part");
    is(72, 3, [0, 0, 0, 0xff], "above the part");
    is(72, 28, [0, 0, 0, 0xff], "below the part");
}

/// A descriptor the server's EGL will not import, said to be a buffer
/// without pixels: refused, nothing drawn, and the canvas goes on drawing
/// -- the screen is not given up for a client's bad buffer.
#[test]
#[expect(clippy::print_stderr, reason = "a test that did not run says so")]
fn a_buffer_the_device_refuses_draws_nothing_and_fails_nothing() {
    let (wide, tall) = (64_u32, 32_u32);
    let Some((mut gpu, dmabuf)) = canvas_and_buffer("canvas-refused", (wide, tall)) else {
        return;
    };
    let whole = Damage::full(wide, tall);
    // A file of the right size that is no dmabuf.
    let file = std::fs::File::open("/proc/self/exe").expect("a descriptor");
    let refused = Surface::without_pixels(16, 16, 64, Format::Argb8888)
        .expect("a surface")
        .named(3)
        .on_device(file.as_fd(), 5);
    assert!(!gpu.imports(&refused));
    Painter::clear(&mut gpu, Color(0xff20_4060), &whole);
    for _ in 0..3 {
        Painter::composite(&mut gpu, &refused, Rect::new(0, 0, 16, 16), &whole);
    }
    gpu.finish().expect("nothing failed");
    let drawn = gpu
        .read(Rect::new(0, 0, wide.into(), 1))
        .expect("read back");
    for found in drawn.chunks_exact(4) {
        assert_eq!(found.get(..3), Some(&[0x60, 0x40, 0x20][..]), "the clear");
    }
    // A buffer it does take is still taken afterwards, and drawn.
    let good = Surface::without_pixels(SIDE, SIDE, STRIDE, Format::Xrgb8888)
        .expect("a surface")
        .named(4)
        .on_device(dmabuf.as_fd(), 6);
    if !gpu.imports(&good) {
        eprintln!("canvas-refused: this host's EGL would not import a udmabuf; the rest skipped");
        return;
    }
    Painter::composite(&mut gpu, &good, Rect::new(0, 0, 64, 64), &whole);
    gpu.finish().expect("the frame was drawn");
    let drawn = gpu
        .read(Rect::new(0, 0, wide.into(), 1))
        .expect("read back");
    assert_eq!(pixel(&drawn, wide, 9, 0), texel(9, 0).get(..3));
}
