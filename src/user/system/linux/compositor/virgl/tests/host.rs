//! The streams, run: on the host's GL, by virglrenderer's own test server.
//!
//! `src/tests.rs` says the words are the ones the header asks for. These say
//! the words *do* something -- that virglrenderer parses the shaders, links
//! them, and puts the pixels where the compositor means them. A host with no
//! `virgl_test_server` skips them, and prints that it did.

use compositor_virgl::vtest::Vtest;
use compositor_virgl::{
    Blend, Rasterizer, Region, Sampler, Stream, VertexBuffer, VertexElement, View, pipe, shaders,
};

const SURFACE: u32 = 1;
const BLEND: u32 = 2;
const DSA: u32 = 3;
const RASTERIZER: u32 = 4;
const ELEMENTS: u32 = 5;
const VERTEX: u32 = 6;
const FRAGMENT: u32 = 7;
const VIEW: u32 = 8;
const SAMPLER: u32 = 9;

/// A server, or `None` with a line saying the test was skipped.
#[expect(
    clippy::print_stderr,
    reason = "a test that did not run says so where a person reading the run will see it"
)]
fn server(name: &str) -> Option<Vtest> {
    match Vtest::start(name) {
        Ok(Some(server)) => Some(server),
        Ok(None) => {
            eprintln!("{name}: no virgl_test_server on this host; skipped");
            None
        }
        Err(error) => {
            eprintln!("{name}: virgl_test_server would not start ({error}); skipped");
            None
        }
    }
}

/// The state every draw here shares: a target to draw into, no depth, and
/// vertices of a place and a texture coordinate.
fn begin(stream: &mut Stream, target: u32, side: u32, blend: Blend) {
    stream.create_surface(SURFACE, target, pipe::FORMAT_B8G8R8A8_UNORM);
    stream.set_framebuffer(SURFACE);
    stream.create_blend(BLEND, blend);
    stream.bind_object(pipe::object::BLEND, BLEND);
    stream.create_dsa(DSA);
    stream.bind_object(pipe::object::DSA, DSA);
    stream.create_rasterizer(RASTERIZER, Rasterizer { scissor: false });
    stream.bind_object(pipe::object::RASTERIZER, RASTERIZER);
    stream.create_vertex_elements(
        ELEMENTS,
        &[
            VertexElement {
                offset: 0,
                buffer: 0,
                format: pipe::FORMAT_R32G32_FLOAT,
            },
            VertexElement {
                offset: 8,
                buffer: 0,
                format: pipe::FORMAT_R32G32_FLOAT,
            },
        ],
    );
    stream.bind_object(pipe::object::VERTEX_ELEMENTS, ELEMENTS);
    assert!(stream.create_shader(
        VERTEX,
        pipe::SHADER_VERTEX,
        shaders::VERTEX,
        shaders::TOKENS
    ));
    stream.bind_shader(VERTEX, pipe::SHADER_VERTEX);
    let side = side as f32;
    stream.set_constants(pipe::SHADER_VERTEX, &[2.0 / side, 2.0 / side, -1.0, -1.0]);
    stream.set_viewport(side as u32, side as u32);
}

/// A rectangle as a strip: each corner its place in pixels and its texture
/// coordinate.
fn quad(stream: &mut Stream, buffer: u32, rect: [f32; 4]) {
    let [x0, y0, x1, y1] = rect;
    let mut data = Vec::new();
    for (x, y, u, v) in [
        (x0, y0, 0.0, 0.0),
        (x1, y0, 1.0, 0.0),
        (x0, y1, 0.0, 1.0),
        (x1, y1, 1.0, 1.0),
    ] {
        for value in [x, y, u, v] {
            data.extend_from_slice(&f32::to_le_bytes(value));
        }
    }
    assert!(stream.write_buffer(buffer, 0, &data));
    stream.set_vertex_buffers(&[VertexBuffer {
        stride: 16,
        offset: 0,
        resource: buffer,
    }]);
    stream.draw(pipe::PRIM_TRIANGLE_STRIP, 0, 4);
}

/// The pixel at (`x`, `y`) of a `side`-wide readback, as `0xAARRGGBB`.
fn pixel(data: &[u8], side: u32, x: u32, y: u32) -> u32 {
    let at = ((y * side + x) * 4) as usize;
    match data.get(at..at + 4) {
        Some([blue, green, red, alpha]) => u32::from_le_bytes([*blue, *green, *red, *alpha]),
        _ => 0xdead_beef,
    }
}

#[test]
fn a_solid_rectangle_lands_where_its_pixels_say() {
    let Some(mut server) = server("solid") else {
        return;
    };
    let side = 64;
    let target = server
        .create(
            pipe::TEXTURE_2D,
            pipe::FORMAT_B8G8R8A8_UNORM,
            pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW,
            side,
            side,
        )
        .expect("a target");
    let vertices = server
        .create(
            pipe::BUFFER,
            pipe::FORMAT_R8_UNORM,
            pipe::BIND_VERTEX_BUFFER,
            4096,
            1,
        )
        .expect("a vertex buffer");

    let mut stream = Stream::new();
    begin(&mut stream, target, side, Blend::REPLACE);
    stream.clear([1.0, 0.0, 0.0, 1.0]);
    assert!(stream.create_shader(
        FRAGMENT,
        pipe::SHADER_FRAGMENT,
        shaders::SOLID,
        shaders::TOKENS
    ));
    stream.bind_shader(FRAGMENT, pipe::SHADER_FRAGMENT);
    stream.set_constants(
        pipe::SHADER_FRAGMENT,
        &[
            0.0, 1.0, 0.0, 1.0, // green
            32.0, 0.0, 32.0, 32.0, // the rectangle
            0.0, 2.0, 0.0, 0.0, // square corners
        ],
    );
    // The top right quarter.
    quad(&mut stream, vertices, [32.0, 0.0, 64.0, 32.0]);
    server.submit(stream.words()).expect("submitted");

    let data = server
        .get(
            target,
            Region {
                x: 0,
                y: 0,
                width: side,
                height: side,
            },
        )
        .expect("read back");
    assert_eq!(pixel(&data, side, 8, 8), 0xffff_0000, "the clear");
    assert_eq!(pixel(&data, side, 56, 8), 0xff00_ff00, "the rectangle");
    assert_eq!(pixel(&data, side, 56, 56), 0xffff_0000, "below it");
    // Its edges are where its pixels say, to the pixel.
    assert_eq!(pixel(&data, side, 31, 0), 0xffff_0000);
    assert_eq!(pixel(&data, side, 32, 0), 0xff00_ff00);
    assert_eq!(pixel(&data, side, 63, 31), 0xff00_ff00);
    assert_eq!(pixel(&data, side, 63, 32), 0xffff_0000);
}

#[test]
fn a_texture_is_drawn_the_way_up_it_was_written() {
    let Some(mut server) = server("textured") else {
        return;
    };
    let side = 64;
    let target = server
        .create(
            pipe::TEXTURE_2D,
            pipe::FORMAT_B8G8R8A8_UNORM,
            pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW,
            side,
            side,
        )
        .expect("a target");
    let source = server
        .create(
            pipe::TEXTURE_2D,
            pipe::FORMAT_B8G8R8A8_UNORM,
            pipe::BIND_SAMPLER_VIEW,
            2,
            2,
        )
        .expect("a source");
    let vertices = server
        .create(
            pipe::BUFFER,
            pipe::FORMAT_R8_UNORM,
            pipe::BIND_VERTEX_BUFFER,
            4096,
            1,
        )
        .expect("a vertex buffer");
    // Four pixels, B G R A: blue and green above, red and white below.
    let texels: [u8; 16] = [
        255, 0, 0, 255, 0, 255, 0, 255, //
        0, 0, 255, 255, 255, 255, 255, 255,
    ];
    server
        .put(
            source,
            Region {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            8,
            &texels,
        )
        .expect("written");

    let mut stream = Stream::new();
    begin(&mut stream, target, side, Blend::PREMULTIPLIED_OVER);
    stream.clear([0.0, 0.0, 0.0, 1.0]);
    assert!(stream.create_shader(
        FRAGMENT,
        pipe::SHADER_FRAGMENT,
        shaders::SURFACE,
        shaders::TOKENS
    ));
    stream.bind_shader(FRAGMENT, pipe::SHADER_FRAGMENT);
    stream.create_sampler_view(
        VIEW,
        View {
            resource: source,
            format: pipe::FORMAT_B8G8R8A8_UNORM,
            opaque: false,
        },
    );
    stream.set_sampler_views(pipe::SHADER_FRAGMENT, &[VIEW]);
    stream.create_sampler_state(
        SAMPLER,
        Sampler {
            filter: pipe::TEX_FILTER_NEAREST,
        },
    );
    stream.bind_sampler_states(pipe::SHADER_FRAGMENT, &[SAMPLER]);
    // Half opacity, over black: every channel halves.
    stream.set_constants(
        pipe::SHADER_FRAGMENT,
        &[
            0.5, 0.0, 0.0, 0.0, //
            0.0, 0.0, 64.0, 64.0, //
            0.0, 2.0, 0.0, 0.0,
        ],
    );
    quad(&mut stream, vertices, [0.0, 0.0, 64.0, 64.0]);
    server.submit(stream.words()).expect("submitted");

    let data = server
        .get(
            target,
            Region {
                x: 0,
                y: 0,
                width: side,
                height: side,
            },
        )
        .expect("read back");
    let close = |found: u32, wanted: u32| {
        found
            .to_le_bytes()
            .iter()
            .zip(wanted.to_le_bytes())
            .all(|(found, wanted)| found.abs_diff(wanted) <= 1)
    };
    let found = [
        pixel(&data, side, 16, 16),
        pixel(&data, side, 48, 16),
        pixel(&data, side, 16, 48),
        pixel(&data, side, 48, 48),
    ];
    let wanted = [0xff00_0080, 0xff00_8000, 0xff80_0000, 0xff80_8080];
    for (found, wanted) in found.iter().zip(wanted) {
        assert!(close(*found, wanted), "{found:08x} is not {wanted:08x}");
    }
}

/// A rounded rectangle is cut where `src/user/system/linux/compositor/render` cuts one: a circle
/// of radius 16 leaves the corner's own pixel out and the pixel on the
/// diagonal at the curve in.
#[test]
fn a_corner_is_cut_by_the_superellipse() {
    let Some(mut server) = server("rounded") else {
        return;
    };
    let side = 64;
    let target = server
        .create(
            pipe::TEXTURE_2D,
            pipe::FORMAT_B8G8R8A8_UNORM,
            pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW,
            side,
            side,
        )
        .expect("a target");
    let vertices = server
        .create(
            pipe::BUFFER,
            pipe::FORMAT_R8_UNORM,
            pipe::BIND_VERTEX_BUFFER,
            4096,
            1,
        )
        .expect("a vertex buffer");
    let mut stream = Stream::new();
    begin(&mut stream, target, side, Blend::PREMULTIPLIED_OVER);
    stream.clear([0.0, 0.0, 0.0, 1.0]);
    assert!(stream.create_shader(
        FRAGMENT,
        pipe::SHADER_FRAGMENT,
        shaders::SOLID,
        shaders::TOKENS
    ));
    stream.bind_shader(FRAGMENT, pipe::SHADER_FRAGMENT);
    stream.set_constants(
        pipe::SHADER_FRAGMENT,
        &[
            1.0, 1.0, 1.0, 1.0, //
            0.0, 0.0, 64.0, 64.0, //
            16.0, 2.0, 0.0, 0.0,
        ],
    );
    quad(&mut stream, vertices, [0.0, 0.0, 64.0, 64.0]);
    server.submit(stream.words()).expect("submitted");
    let data = server
        .get(
            target,
            Region {
                x: 0,
                y: 0,
                width: side,
                height: side,
            },
        )
        .expect("read back");
    // Row 0's centre is 15.5 from the corner's centre line, so the circle
    // reaches in to sqrt(16^2 - 15.5^2) = 3.97 of it: columns 0 to 11 are
    // out, from 12 -- whose centre is 3.5 away -- in.
    assert_eq!(pixel(&data, side, 0, 0), 0xff00_0000);
    assert_eq!(pixel(&data, side, 11, 0), 0xff00_0000);
    assert_eq!(pixel(&data, side, 12, 0), 0xffff_ffff);
    // The same cut at every corner.
    assert_eq!(pixel(&data, side, 63, 63), 0xff00_0000);
    assert_eq!(pixel(&data, side, 52, 63), 0xff00_0000);
    assert_eq!(pixel(&data, side, 51, 63), 0xffff_ffff);
    assert_eq!(pixel(&data, side, 32, 32), 0xffff_ffff);
}

/// virglrenderer takes every shader here: each is made, bound and drawn
/// with, and the context is still one that draws afterwards. A shader it
/// could not parse puts the context in error, and nothing after it is run.
#[test]
fn every_shader_is_one_virglrenderer_takes() {
    for (name, text) in shaders::FRAGMENTS {
        // A socket of its own: the tests run side by side, and one of them
        // is named for a shader too.
        let Some(mut server) = server(&format!("every-{name}")) else {
            return;
        };
        let side = 64;
        let target = server
            .create(
                pipe::TEXTURE_2D,
                pipe::FORMAT_B8G8R8A8_UNORM,
                pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW,
                side,
                side,
            )
            .expect("a target");
        let source = server
            .create(
                pipe::TEXTURE_2D,
                pipe::FORMAT_B8G8R8A8_UNORM,
                pipe::BIND_SAMPLER_VIEW,
                side,
                side,
            )
            .expect("a source");
        let vertices = server
            .create(
                pipe::BUFFER,
                pipe::FORMAT_R8_UNORM,
                pipe::BIND_VERTEX_BUFFER,
                4096,
                1,
            )
            .expect("a vertex buffer");
        let mut stream = Stream::new();
        begin(&mut stream, target, side, Blend::REPLACE);
        stream.clear([0.0, 0.0, 0.0, 1.0]);
        stream.create_sampler_view(
            VIEW,
            View {
                resource: source,
                format: pipe::FORMAT_B8G8R8A8_UNORM,
                opaque: false,
            },
        );
        stream.set_sampler_views(pipe::SHADER_FRAGMENT, &[VIEW]);
        stream.create_sampler_state(
            SAMPLER,
            Sampler {
                filter: pipe::TEX_FILTER_LINEAR,
            },
        );
        stream.bind_sampler_states(pipe::SHADER_FRAGMENT, &[SAMPLER]);
        assert!(stream.create_shader(FRAGMENT, pipe::SHADER_FRAGMENT, text, shaders::TOKENS));
        stream.bind_shader(FRAGMENT, pipe::SHADER_FRAGMENT);
        stream.set_constants(pipe::SHADER_FRAGMENT, &[1.0; 16]);
        quad(&mut stream, vertices, [0.0, 0.0, 32.0, 32.0]);
        // And then one whose answer is known, in the other half.
        assert!(stream.create_shader(
            FRAGMENT + 100,
            pipe::SHADER_FRAGMENT,
            shaders::SOLID,
            shaders::TOKENS
        ));
        stream.bind_shader(FRAGMENT + 100, pipe::SHADER_FRAGMENT);
        stream.set_constants(
            pipe::SHADER_FRAGMENT,
            &[
                0.0, 0.0, 1.0, 1.0, //
                32.0, 32.0, 32.0, 32.0, //
                0.0, 2.0, 0.0, 0.0,
            ],
        );
        quad(&mut stream, vertices, [32.0, 32.0, 64.0, 64.0]);
        server.submit(stream.words()).expect("submitted");
        let data = server
            .get(
                target,
                Region {
                    x: 0,
                    y: 0,
                    width: side,
                    height: side,
                },
            )
            .expect("read back");
        assert_eq!(
            pixel(&data, side, 48, 48),
            0xff00_00ff,
            "{name}: the context stopped drawing"
        );
    }
}

/// What one protocol makes of a draw and of uploads: the whole target, a
/// rectangle of it read into a wider buffer, and a source texture written
/// twice in one place and read back.
#[expect(
    clippy::expect_used,
    clippy::print_stderr,
    reason = "a test's helper: a refusal is the test failing, and a skip says so"
)]
fn round_trip(version: u32) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    use compositor_virgl::{Device, Texture};
    let mut server = match Vtest::start_with(&format!("shm{version}"), version) {
        Ok(Some(server)) => server,
        _ => {
            eprintln!("shm{version}: no virgl_test_server on this host; skipped");
            return None;
        }
    };
    assert_eq!(server.version(), version, "the protocol agreed");
    let side = 64;
    let texture = |bind| Texture {
        width: side,
        height: side,
        format: pipe::FORMAT_B8G8R8A8_UNORM,
        bind,
        moved: true,
        scanout: false,
    };
    let target = server
        .texture(texture(pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW))
        .expect("a target");
    let source = server
        .texture(texture(pipe::BIND_SAMPLER_VIEW))
        .expect("a source");
    let vertices = server.buffer(4096).expect("a vertex buffer");
    let shared = if version >= 2 { 2 } else { 0 };
    assert_eq!(server.shared_textures(), shared, "textures with memory");

    let mut stream = Stream::new();
    begin(&mut stream, target, side, Blend::REPLACE);
    stream.clear([1.0, 0.0, 0.0, 1.0]);
    assert!(stream.create_shader(
        FRAGMENT,
        pipe::SHADER_FRAGMENT,
        shaders::SOLID,
        shaders::TOKENS
    ));
    stream.bind_shader(FRAGMENT, pipe::SHADER_FRAGMENT);
    stream.set_constants(
        pipe::SHADER_FRAGMENT,
        &[
            0.0, 1.0, 0.0, 1.0, 32.0, 0.0, 32.0, 32.0, 0.0, 2.0, 0.0, 0.0,
        ],
    );
    quad(&mut stream, vertices, [32.0, 0.0, 64.0, 32.0]);
    Device::submit(&mut server, stream.words()).expect("submitted");
    let whole = Region {
        x: 0,
        y: 0,
        width: side,
        height: side,
    };
    let frame = server.read(target, whole).expect("read back");
    // A rectangle across the green's edge, into a buffer wider than it,
    // at the buffer's start.
    let stride = 40 * 4;
    let mut wide = vec![0x55_u8; stride * 8];
    server
        .read_into(
            target,
            Region {
                x: 28,
                y: 28,
                width: 8,
                height: 8,
            },
            &mut wide,
            stride,
        )
        .expect("read into");

    // The same place written twice before anything reads it: the second
    // write may not land in memory the server is still reading the first
    // from, and it is the second that is read back.
    let place = Region {
        x: 3,
        y: 5,
        width: 10,
        height: 6,
    };
    let upload_stride = 12 * 4;
    for value in [0x11_u8, 0x22] {
        let pixels: Vec<u8> = (0..upload_stride * 6)
            .map(|index| value.wrapping_add((index % 251) as u8))
            .collect();
        server
            .upload(source, place, upload_stride as u32, &pixels)
            .expect("uploaded");
    }
    let uploaded = server.read(source, place).expect("read the upload");
    server.release(source).expect("released");
    server.release(target).expect("released");
    Some((frame, wide, uploaded))
}

#[test]
fn shared_memory_moves_the_pixels_the_socket_does() {
    let Some(socket) = round_trip(0) else {
        return;
    };
    let Some(shared) = round_trip(2) else {
        return;
    };
    let (frame, wide, uploaded) = &shared;
    assert_eq!(pixel(frame, 64, 8, 8), 0xffff_0000, "the clear");
    assert_eq!(pixel(frame, 64, 56, 8), 0xff00_ff00, "the rectangle");
    // The rectangle read into the wider buffer: its own rows only.
    // (28, 28) is its first pixel; the green ends at x 32 and y 32.
    assert_eq!(pixel(wide, 40, 4, 3), 0xff00_ff00, "inside the green");
    assert_eq!(pixel(wide, 40, 3, 3), 0xffff_0000, "left of it");
    assert_eq!(pixel(wide, 40, 4, 4), 0xffff_0000, "below it");
    assert_eq!(
        pixel(wide, 40, 8, 0),
        0x5555_5555,
        "past the row, untouched"
    );
    // The second upload's bytes, rows packed.
    let expected: Vec<u8> = (0..6)
        .flat_map(|row| {
            (0..40).map(move |byte| 0x22_u8.wrapping_add(((row * 48 + byte) % 251) as u8))
        })
        .collect();
    assert_eq!(uploaded, &expected, "the second upload");
    assert_eq!(socket, shared, "version 0 and version 2 agree");
}

/// Ferrix's patched server, built by tools/common/fetch/fetch-virgl-server.sh:
/// `FERRIX_VIRGL_SERVER_BIN`, or where that script puts it.
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

/// One texture drawn and read back, to show the connection still works.
#[expect(clippy::expect_used, reason = "a test's helper")]
fn still_draws(server: &mut Vtest) {
    use compositor_virgl::{Device, Texture};
    let target = server
        .texture(Texture {
            width: 8,
            height: 8,
            format: pipe::FORMAT_B8G8R8A8_UNORM,
            bind: pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW,
            moved: true,
            scanout: false,
        })
        .expect("a texture after the import");
    let mut stream = Stream::new();
    stream.create_surface(SURFACE, target, pipe::FORMAT_B8G8R8A8_UNORM);
    stream.set_framebuffer(SURFACE);
    stream.clear([0.0, 0.0, 1.0, 1.0]);
    Device::submit(server, stream.words()).expect("submitted");
    let pixels = server
        .read(
            target,
            Region {
                x: 0,
                y: 0,
                width: 8,
                height: 8,
            },
        )
        .expect("read back");
    assert_eq!(
        pixel(&pixels, 8, 3, 3),
        0xff00_00ff,
        "the clear, after the import"
    );
}

/// A memfd standing in for a dmabuf: `lseek` gives its size, as a dmabuf's,
/// and no EGL takes it as one.
#[expect(clippy::expect_used, reason = "a test's helper")]
fn not_a_dmabuf(len: i64) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    // SAFETY: a fresh descriptor from a constant name.
    let raw = unsafe { libc::memfd_create(c"not-a-dmabuf".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just made, owned by nothing else.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
    // SAFETY: a descriptor this test holds.
    assert_eq!(unsafe { libc::ftruncate(raw, len) }, 0);
    let _ = fd.try_clone().expect("cloneable");
    fd
}

#[test]
#[expect(clippy::print_stderr, reason = "a test that did not run says so")]
fn a_stock_server_is_never_sent_the_import() {
    use std::os::fd::AsFd;
    let Some(mut server) = server("stock-import") else {
        return;
    };
    if server.can_import().expect("asked") {
        eprintln!("stock-import: the server on PATH is a patched one; skipped");
        return;
    }
    let buffer = not_a_dmabuf(64 * 64 * 4);
    let error = server
        .import(
            buffer.as_fd(),
            pipe::FORMAT_B8G8R8X8_UNORM,
            pipe::BIND_RENDER_TARGET,
            (64, 64),
            256,
            0,
            0,
        )
        .expect_err("a stock server takes no import");
    assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP));
    still_draws(&mut server);
}

#[test]
#[expect(clippy::print_stderr, reason = "a test that did not run says so")]
fn the_patched_server_refuses_a_buffer_its_geometry_does_not_fit() {
    use std::os::fd::AsFd;
    let Some(program) = patched_server() else {
        eprintln!("patched-import: no patched virgl_test_server built; skipped");
        return;
    };
    let Ok(Some(mut server)) = Vtest::start_program(&program, "patched-import", 2) else {
        eprintln!("patched-import: the patched server would not start; skipped");
        return;
    };
    assert!(
        server.can_import().expect("asked"),
        "the patched server says it imports"
    );
    let refuse = |server: &mut Vtest, len: i64, size: (u32, u32), stride: u32, offset: u32| {
        let buffer = not_a_dmabuf(len);
        server
            .import(
                buffer.as_fd(),
                pipe::FORMAT_B8G8R8X8_UNORM,
                pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW,
                size,
                stride,
                offset,
                0,
            )
            .expect_err("refused")
            .raw_os_error()
    };
    // A stride shorter than a row, a buffer shorter than its rows, an offset
    // pushing the last row past the end, and no size: each EINVAL, before
    // EGL is asked anything.
    assert_eq!(
        refuse(&mut server, 64 * 64 * 4, (64, 64), 255, 0),
        Some(libc::EINVAL)
    );
    assert_eq!(
        refuse(&mut server, 64 * 64 * 4 - 1, (64, 64), 256, 0),
        Some(libc::EINVAL)
    );
    assert_eq!(
        refuse(&mut server, 64 * 64 * 4, (64, 64), 256, 4),
        Some(libc::EINVAL)
    );
    assert_eq!(
        refuse(&mut server, 4096, (0, 64), 256, 0),
        Some(libc::EINVAL)
    );
    assert_eq!(
        refuse(&mut server, 1 << 30, (20000, 4), 80000, 0),
        Some(libc::EINVAL)
    );
    // A geometry that fits, of a file that is no dmabuf: EGL refuses it, and
    // the server says so rather than ending the connection.
    let fits = refuse(&mut server, 64 * 64 * 4, (64, 64), 256, 0);
    assert!(
        fits.is_some_and(|errno| errno != 0),
        "refused by EGL: {fits:?}"
    );
    assert_eq!(server.imported_textures(), 0);
    still_draws(&mut server);
}

/// A dmabuf of system memory, from the host's `/dev/udmabuf` over a sealed
/// memfd, and the memfd: `None` where the host has none to give.
fn udmabuf(len: u64) -> Option<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::{AsRawFd, FromRawFd};
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
    let memory = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
    // SAFETY: a descriptor this test holds.
    if unsafe { libc::ftruncate(raw, i64::try_from(len).ok()?) } != 0 {
        return None;
    }
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
    create[..4].copy_from_slice(&(memory.as_raw_fd() as u32).to_le_bytes());
    create[4..8].copy_from_slice(&1_u32.to_le_bytes()); // UDMABUF_FLAGS_CLOEXEC
    create[16..24].copy_from_slice(&len.to_le_bytes());
    // SAFETY: UDMABUF_CREATE reads the 24 bytes it is given.
    let made = unsafe { libc::ioctl(device.as_raw_fd(), 0x4018_7542, create.as_mut_ptr()) };
    if made < 0 {
        return None;
    }
    // SAFETY: the descriptor the ioctl just made, owned by nothing else.
    Some((unsafe { std::os::fd::OwnedFd::from_raw_fd(made) }, memory))
}

#[test]
#[expect(clippy::print_stderr, reason = "a test that did not run says so")]
fn a_frame_is_drawn_into_an_imported_buffer() {
    use compositor_virgl::Device;
    use std::os::fd::AsFd;
    let Some(program) = patched_server() else {
        eprintln!("import-draw: no patched virgl_test_server built; skipped");
        return;
    };
    let Ok(Some(mut server)) = Vtest::start_program(&program, "import-draw", 2) else {
        eprintln!("import-draw: the patched server would not start; skipped");
        return;
    };
    let side = 64_u32;
    let stride = 256_u32;
    let Some((dmabuf, memory)) = udmabuf(u64::from(stride * side).next_multiple_of(4096)) else {
        eprintln!("import-draw: no /dev/udmabuf to make a dmabuf with; skipped");
        return;
    };
    // Blue in the memory first, as a client's frame would be.
    {
        use std::io::Write;
        let mut file = std::fs::File::from(memory.try_clone().expect("cloned"));
        let blue: Vec<u8> = (0..stride * side / 4)
            .flat_map(|_| [0xff_u8, 0, 0, 0xff])
            .collect();
        file.write_all(&blue).expect("written");
    }
    let target = match server.import(
        dmabuf.as_fd(),
        pipe::FORMAT_B8G8R8X8_UNORM,
        pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW,
        (side, side),
        stride,
        0,
        0,
    ) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("import-draw: this host's EGL would not import a udmabuf ({error}); skipped");
            return;
        }
    };
    let before = server
        .read(
            target,
            Region {
                x: 0,
                y: 0,
                width: side,
                height: side,
            },
        )
        .expect("read back before");
    eprintln!(
        "import-draw: before drawing the texture reads {:08x}",
        pixel(&before, side, 5, 5)
    );
    let mut stream = Stream::new();
    stream.create_surface(SURFACE, target, pipe::FORMAT_B8G8R8X8_UNORM);
    stream.set_framebuffer(SURFACE);
    stream.clear([0.0, 1.0, 0.0, 1.0]);
    Device::submit(&mut server, stream.words()).expect("submitted");
    // Read back through the server: the GL texture is the buffer.
    let pixels = server
        .read(
            target,
            Region {
                x: 0,
                y: 0,
                width: side,
                height: side,
            },
        )
        .expect("read back");
    assert_eq!(
        pixel(&pixels, side, 5, 5) & 0x00ff_ffff,
        0x0000_ff00,
        "the clear, read back"
    );
    // And in the buffer's own memory, with no transfer: the frame is where
    // a display engine would scan it out.
    // (At its own offset: the descriptor's position is shared with the
    // clone the blue was written through.)
    let file = std::fs::File::from(memory);
    let mut bytes = [0_u8; 4];
    std::os::unix::fs::FileExt::read_exact_at(&file, &mut bytes, u64::from(5 * stride + 5 * 4))
        .expect("the memory");
    // Whether the drawing lands in the memory is the importing driver's:
    // NVIDIA's EGL on a host takes a foreign (udmabuf) buffer's pixels in
    // and draws into a copy of its own (measured 2026-10-10: blue stays),
    // where a buffer nvidia-drm made is its own memory. Said, not judged;
    // the card run judges it.
    eprintln!(
        "import-draw: the dmabuf's memory holds {:02x?} after the clear ({})",
        &bytes[..3],
        if bytes[..3] == [0, 0xff, 0] {
            "the drawing is in the buffer"
        } else {
            "this driver drew into a copy"
        }
    );
}
