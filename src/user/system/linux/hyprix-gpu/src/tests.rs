//! The helper's frames against the software painter's, on lavapipe.
//!
//! The software painter (`compositor_render::Canvas`) is the reference, as
//! it is for the virgl painter (`compositor_render::gpu`'s tests): the same
//! calls, drawn once on the CPU and once by Vulkan, are one picture when
//! few pixels are more than a step apart. The calls go through the
//! renderer directly, and then whole through the socket as hyprix sends
//! them, into an output dmabuf that is mapped and read here as the card
//! reads it.
//!
//! A host with no lavapipe (`libvulkan_lvp.so` and its ICD) runs none of
//! this, and says so.

use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};

use compositor_gpu_protocol::{self as protocol, Message, Op, OpsWriter};
use compositor_render::gpu::Renderer;
use compositor_render::{
    Blur, Color, Damage, Format, Gradient, Painter, Rect, Rounding, Shadow, Surface,
};

use crate::server::Server;
use crate::vulkan::Vulkan;
use crate::wire::Wire;

const SIZE: (u32, u32) = (256, 192);

/// How far apart a channel may be; the virgl painter's own step.
const STEP: u8 = 3;

/// lavapipe -- or the device whose name holds `HYPRIX_GPU_TEST_DEVICE`,
/// which is how these tests run on a real GPU (anv on the N150) -- or
/// `None` with a line saying the test was skipped.
#[expect(
    clippy::print_stderr,
    reason = "a test that did not run says so where a person reading the run will see it"
)]
fn lavapipe(name: &str) -> Option<Vulkan> {
    let wanted = std::env::var("HYPRIX_GPU_TEST_DEVICE").unwrap_or_else(|_| "llvmpipe".to_owned());
    match tole::gpu::Core::new(false, Some(wanted), None) {
        Ok(core) => Some(Vulkan::new(core).expect("the backend on lavapipe")),
        Err(why) => {
            eprintln!("{name}: no lavapipe ({why}); skipped");
            None
        }
    }
}

/// How many pixels have a channel more than `step` apart, and the
/// furthest any is.
fn difference(one: &[u8], other: &[u8], step: u8) -> (usize, u8) {
    assert_eq!(one.len(), other.len());
    let mut count = 0;
    let mut furthest = 0;
    for (a, b) in one.chunks_exact(4).zip(other.chunks_exact(4)) {
        let apart = a
            .iter()
            .zip(b)
            .take(3)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        furthest = furthest.max(apart);
        if apart > step {
            count += 1;
        }
    }
    (count, furthest)
}

/// A client's buffer, premultiplied: a diagonal ramp, half of it
/// translucent.
fn pixels(width: u32, height: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let alpha: u32 = if x < width / 2 { 255 } else { 160 };
            let scale = |v: u32| u8::try_from(v * alpha / 255).unwrap_or(255);
            data.extend_from_slice(&[
                scale(x * 255 / width),
                scale(y * 255 / height),
                scale(128),
                u8::try_from(alpha).unwrap_or(255),
            ]);
        }
    }
    data
}

/// The scene: every operation hyprix draws a desktop with.
fn scene<P: Painter>(painter: &mut P, surface: &Surface<'_>, backdrop: &mut P::Backdrop) {
    let whole = Damage::full(SIZE.0, SIZE.1);
    painter.clear(Color(0xff20_3040), &whole);
    painter.fill(Rect::new(10, 10, 100, 60), Color(0xffc0_4020), &whole);
    painter.fill_rounded(
        Rect::new(60, 40, 120, 90),
        Rounding::circle(16),
        Color(0x8020_c060),
        &whole,
    );
    painter.keep_backdrop(backdrop, &whole);
    painter.shadow(
        Rect::new(40, 100, 90, 60),
        &Shadow {
            rounding: Rounding::circle(8),
            range: 12,
            power: 3,
            color: Color(0xc000_0000),
            offset: (2, 3),
        },
        &whole,
    );
    painter.blur_backdrop(
        backdrop,
        Rect::new(40, 100, 90, 60),
        Rounding::circle(8),
        &Blur::new(4, 2),
        &whole,
    );
    painter.border_gradient(
        Rect::new(150, 20, 80, 50),
        3,
        &Gradient::new(&[Color(0xff33_ccff), Color(0xff00_ff99)], 45),
        &whole,
    );
    painter.composite(surface, Rect::new(140, 110, 64, 48), &whole);
    painter.composite_scaled(
        surface,
        Rect::new(200, 100, 48, 80),
        Rounding::circle(6),
        0.75,
        false,
        &whole,
    );
}

/// The scene through the renderer on lavapipe is the software painter's
/// picture.
#[test]
fn the_scene_on_lavapipe_is_the_software_picture() {
    let Some(vulkan) = lavapipe("the_scene_on_lavapipe_is_the_software_picture") else {
        return;
    };
    let data = pixels(64, 48);
    let surface = Surface::new(&data, 64, 48, 64 * 4, Format::Argb8888)
        .expect("a surface")
        .named(7);
    let mut software = compositor_render::Canvas::new(SIZE.0, SIZE.1).expect("a canvas");
    let mut kept = compositor_render::Backdrop::new(SIZE.0, SIZE.1).expect("a backdrop");
    scene(&mut software, &surface, &mut kept);
    let mut gpu = Renderer::with_backend(vulkan, SIZE.0, SIZE.1).expect("a renderer");
    let mut backdrop = compositor_render::gpu::Backdrop::new(SIZE.0, SIZE.1);
    scene(&mut gpu, &surface, &mut backdrop);
    gpu.finish().expect("drawn");
    let drawn = gpu
        .read(Rect::new(0, 0, i64::from(SIZE.0), i64::from(SIZE.1)))
        .expect("read back");
    let (count, furthest) = difference(&drawn, software.data(), STEP);
    assert!(
        count < 64,
        "{count} pixels more than {STEP} apart, the furthest {furthest}"
    );
}

/// A socket pair of the kind hyprix makes.
fn pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors.
    let made = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    assert_eq!(made, 0, "socketpair");
    // SAFETY: new and ours alone.
    let one = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: as above.
    let other = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    (one, other)
}

/// A memfd of `len` bytes holding `bytes` at its start.
fn arena(len: usize, bytes: &[u8]) -> OwnedFd {
    // SAFETY: a name and flags.
    let raw = unsafe { libc::memfd_create(c"hyprix-gpu-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0, "memfd_create");
    // SAFETY: new and ours.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: a size for our own memfd.
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), len as i64) }, 0);
    // SAFETY: written from our buffer to our file.
    let wrote = unsafe { libc::pwrite(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
    assert_eq!(wrote, bytes.len() as isize);
    fd
}

/// `len` bytes of `fd`, read through a mapping, as the card reads them.
fn mapped(fd: &OwnedFd, len: usize) -> Vec<u8> {
    // SAFETY: a fresh read-only shared mapping of the dmabuf.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    assert_ne!(ptr, libc::MAP_FAILED, "mmap of the output");
    // SAFETY: mapped for `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec();
    // SAFETY: the mapping made above.
    let _ = unsafe { libc::munmap(ptr, len) };
    bytes
}

/// hyprix's side: send `message` with `fds`.
fn send(wire: &Wire, message: &Message, fds: &[&OwnedFd]) {
    let record = message.encode(1).expect("encoded");
    let fds: Vec<_> = fds.iter().map(|fd| fd.as_fd()).collect();
    wire.send(&record, &fds).expect("sent");
}

/// hyprix's side: the next message and its descriptors.
fn receive(wire: &Wire) -> (Message, Vec<OwnedFd>) {
    let (record, fds) = wire.receive().expect("received").expect("not closed");
    let (_, message) = Message::decode(&record).expect("decoded");
    (message, fds)
}

/// A frame of `ops` into `output`, and what the helper answered.
fn frame(wire: &Wire, number: u64, output: u32, ops: &[Op]) -> Message {
    send(
        wire,
        &Message::FrameBegin {
            canvas: 1,
            frame: number,
            output,
        },
        &[],
    );
    let mut writer = OpsWriter::new();
    for op in ops {
        writer.push(op).expect("an op");
    }
    for body in writer.finish() {
        send(wire, &Message::Ops(body), &[]);
    }
    send(
        wire,
        &Message::FrameEnd {
            frame: number,
            damage: vec![protocol::Rect::new(0, 0, SIZE.0, SIZE.1)],
        },
        &[],
    );
    receive(wire).0
}

/// A frame sent as hyprix sends it -- an shm texture through the arena,
/// shapes, a composite -- lands in the output dmabuf as the software
/// painter draws it; an output not given back is never drawn into.
#[test]
fn a_frame_through_the_socket_lands_in_the_output() {
    if lavapipe("a_frame_through_the_socket_lands_in_the_output").is_none() {
        return;
    }
    let (ours, theirs) = pair();
    // The helper on a thread of its own, its device made there.
    let helper = std::thread::spawn(move || {
        let vulkan = lavapipe("the helper's thread").expect("lavapipe, as a moment ago");
        let mut server = Server::new(vulkan, Wire::new(theirs), "lavapipe".to_owned());
        let _ = server.run();
    });
    let wire = Wire::new(ours);
    send(&wire, &Message::Hello { version: 1 }, &[]);
    assert!(matches!(receive(&wire).0, Message::Ready { .. }));
    assert!(matches!(receive(&wire).0, Message::Modifiers { .. }));
    assert!(matches!(receive(&wire).0, Message::Modifiers { .. }));
    send(
        &wire,
        &Message::Configure {
            canvas: 1,
            width: SIZE.0,
            height: SIZE.1,
            outputs: 2,
        },
        &[],
    );
    let (outputs, fds) = receive(&wire);
    let Message::Outputs {
        modifier, planes, ..
    } = outputs
    else {
        panic!("not Outputs: {outputs:?}");
    };
    // Linear dmabufs, or memfds where the device exports none.
    assert!(
        modifier == crate::vulkan::LINEAR || modifier == crate::vulkan::MEMFD,
        "an output of modifier {modifier:#x}"
    );
    assert_eq!(fds.len(), 2);
    let data = pixels(64, 48);
    let arena_fd = arena(1 << 20, &data);
    send(
        &wire,
        &Message::Arena {
            arena: 1,
            size: 1 << 20,
        },
        &[&arena_fd],
    );
    send(
        &wire,
        &Message::CreateTexture {
            texture: 7,
            width: 64,
            height: 48,
            fourcc: protocol::ARGB8888,
        },
        &[],
    );
    let whole = protocol::Rect::new(0, 0, SIZE.0, SIZE.1);
    let ops = [
        Op::Damage(vec![whole]),
        Op::Upload {
            texture: 7,
            rect: protocol::Rect::new(0, 0, 64, 48),
            arena: 1,
            offset: 0,
            stride: 64 * 4,
        },
        Op::Clear(0xff20_3040),
        Op::Fill(protocol::Rect::new(10, 10, 100, 60), 0xffc0_4020),
        Op::FillRounded(
            protocol::Rect::new(60, 40, 120, 90),
            protocol::Rounding {
                radius: 16,
                power: 2.0,
            },
            0x8020_c060,
        ),
        Op::Composite {
            texture: 7,
            src: protocol::Rect::new(0, 0, 64, 48),
            rect: protocol::Rect::new(140, 110, 64, 48),
        },
    ];
    let done = frame(&wire, 1, 0, &ops);
    assert!(
        matches!(done, Message::Done { output: 0, .. }),
        "not Done: {done:?}"
    );

    // The software painter's picture of the same calls.
    let surface = Surface::new(&data, 64, 48, 64 * 4, Format::Argb8888).expect("a surface");
    let mut software = compositor_render::Canvas::new(SIZE.0, SIZE.1).expect("a canvas");
    let all = Damage::full(SIZE.0, SIZE.1);
    Painter::clear(&mut software, Color(0xff20_3040), &all);
    Painter::fill(
        &mut software,
        Rect::new(10, 10, 100, 60),
        Color(0xffc0_4020),
        &all,
    );
    Painter::fill_rounded(
        &mut software,
        Rect::new(60, 40, 120, 90),
        Rounding::circle(16),
        Color(0x8020_c060),
        &all,
    );
    Painter::composite(&mut software, &surface, Rect::new(140, 110, 64, 48), &all);

    let plane = planes[0];
    let stride = plane.stride as usize;
    let row = SIZE.0 as usize * 4;
    let bytes = mapped(&fds[0], plane.offset as usize + stride * SIZE.1 as usize);
    let mut drawn = Vec::with_capacity(row * SIZE.1 as usize);
    for y in 0..SIZE.1 as usize {
        let start = plane.offset as usize + y * stride;
        drawn.extend_from_slice(&bytes[start..start + row]);
    }
    let (count, furthest) = difference(&drawn, software.data(), STEP);
    assert!(
        count < 32,
        "{count} pixels more than {STEP} apart, the furthest {furthest}"
    );

    // Output 0 was not given back: drawing into it again is refused.
    let refused = frame(&wire, 2, 0, &[Op::Clear(0xff00_0000)]);
    assert!(
        matches!(refused, Message::Failed { .. }),
        "an unreleased output was drawn into: {refused:?}"
    );
    // Output 1 was never handed out, and output 0 once it is back. Where
    // the outputs are dmabufs, output 0's -- the picture above -- is a
    // client's dmabuf drawn whole into output 1: an import, sampled in
    // place, after its writes are waited for.
    let dmabufs = modifier == crate::vulkan::LINEAR;
    let ops = if dmabufs {
        send(
            &wire,
            &Message::ImportDmabuf {
                texture: 8,
                width: SIZE.0,
                height: SIZE.1,
                fourcc: protocol::XRGB8888,
                modifier,
                planes: vec![plane],
            },
            &[&fds[0]],
        );
        vec![
            Op::Damage(vec![whole]),
            Op::Composite {
                texture: 8,
                src: whole,
                rect: whole,
            },
        ]
    } else {
        vec![Op::Clear(0xff00_0000)]
    };
    assert!(matches!(
        frame(&wire, 3, 1, &ops),
        Message::Done { output: 1, .. }
    ));
    if dmabufs {
        let plane = planes[1];
        let bytes = mapped(
            &fds[1],
            plane.offset as usize + plane.stride as usize * SIZE.1 as usize,
        );
        let mut copied = Vec::with_capacity(row * SIZE.1 as usize);
        for y in 0..SIZE.1 as usize {
            let start = plane.offset as usize + y * plane.stride as usize;
            copied.extend_from_slice(&bytes[start..start + row]);
        }
        let (count, furthest) = difference(&copied, software.data(), STEP);
        assert!(
            count < 32,
            "the imported dmabuf drawn: {count} pixels more than {STEP} apart, the furthest {furthest}"
        );
    }
    send(
        &wire,
        &Message::Release {
            canvas: 1,
            output: 0,
        },
        &[],
    );
    assert!(matches!(
        frame(&wire, 4, 0, &[Op::Clear(0xff00_0000)]),
        Message::Done { output: 0, .. }
    ));
    // A texture that is not there fails the frame: hyprix then draws it in
    // software, rather than show nothing where the window is.
    send(
        &wire,
        &Message::Release {
            canvas: 1,
            output: 1,
        },
        &[],
    );
    let missing = frame(
        &wire,
        5,
        1,
        &[Op::Composite {
            texture: 99,
            src: protocol::Rect::new(0, 0, 8, 8),
            rect: protocol::Rect::new(0, 0, 8, 8),
        }],
    );
    assert!(
        matches!(missing, Message::Failed { code: 1, .. }),
        "a missing texture was drawn as nothing: {missing:?}"
    );
    drop(wire);
    helper.join().expect("the helper ended");
}

/// What a desktop frame costs at 1920x1080: a wallpaper-sized texture,
/// a blurred translucent window and a shadow, drawn whole, twenty times.
/// Run by hand (`--ignored`), on lavapipe or `HYPRIX_GPU_TEST_DEVICE`'s.
#[test]
#[ignore = "a measurement, not a check"]
#[expect(
    clippy::print_stderr,
    reason = "a measurement says what it measured where the person running it reads"
)]
fn a_desktop_frame_at_1080p() {
    let Some(vulkan) = lavapipe("a_desktop_frame_at_1080p") else {
        return;
    };
    let (w, h) = (1920_u32, 1080_u32);
    let wall = pixels(w, h);
    let window = pixels(1280, 800);
    let wallpaper = Surface::new(&wall, w, h, w * 4, Format::Xrgb8888)
        .expect("a surface")
        .named(1);
    let terminal = Surface::new(&window, 1280, 800, 1280 * 4, Format::Argb8888)
        .expect("a surface")
        .named(2);
    let mut gpu = Renderer::with_backend(vulkan, w, h).expect("a renderer");
    let mut backdrop = compositor_render::gpu::Backdrop::new(w, h);
    let whole = Damage::full(w, h);
    let at = Rect::new(320, 140, 1280, 800);
    let mut times = Vec::new();
    for _ in 0..20 {
        let start = std::time::Instant::now();
        gpu.composite(
            &wallpaper,
            Rect::new(0, 0, i64::from(w), i64::from(h)),
            &whole,
        );
        gpu.keep_backdrop(&mut backdrop, &whole);
        gpu.shadow(
            at,
            &Shadow {
                rounding: Rounding::circle(10),
                range: 20,
                power: 3,
                color: Color(0xa000_0000),
                offset: (0, 4),
            },
            &whole,
        );
        gpu.blur_backdrop(
            &mut backdrop,
            at,
            Rounding::circle(10),
            &Blur::new(8, 3),
            &whole,
        );
        gpu.composite_scaled(&terminal, at, Rounding::circle(10), 0.9, false, &whole);
        gpu.finish().expect("drawn");
        times.push(start.elapsed());
    }
    times.sort();
    eprintln!(
        "1080p desktop frame: median {:?}, fastest {:?}, slowest {:?}",
        times[times.len() / 2],
        times[0],
        times[times.len() - 1]
    );
}

/// A frame copied into a memfd output -- what a device without dmabuf
/// export draws into -- is there when the memfd is read.
#[test]
fn a_frame_lands_in_a_memfd_output() {
    use compositor_render::gpu::{Backend as _, Region};
    let Some(mut vulkan) = lavapipe("a_frame_lands_in_a_memfd_output") else {
        return;
    };
    let output = match vulkan.host_output(SIZE) {
        Ok(output) => output,
        Err(why) => {
            assert!(
                vulkan.core().host_import().is_none(),
                "a device with host import made no memfd output: {why}"
            );
            return;
        }
    };
    assert_eq!(output.modifier, crate::vulkan::MEMFD);
    let mut gpu = Renderer::with_backend(vulkan, SIZE.0, SIZE.1).expect("a renderer");
    let whole = Damage::full(SIZE.0, SIZE.1);
    gpu.clear(Color(0xff10_2030), &whole);
    gpu.fill(Rect::new(8, 8, 40, 20), Color(0xffff_8000), &whole);
    let target = gpu.target();
    gpu.backend()
        .copy(
            target.id,
            output.id,
            &[Region {
                x: 0,
                y: 0,
                width: SIZE.0,
                height: SIZE.1,
            }],
        )
        .expect("copied");
    gpu.finish().expect("drawn");
    let bytes = mapped(&output.fd, (output.stride * SIZE.1) as usize);
    let at = |x: usize, y: usize| {
        let start = y * output.stride as usize + x * 4;
        [bytes[start], bytes[start + 1], bytes[start + 2]]
    };
    assert_eq!(
        at(0, 0),
        [0x30, 0x20, 0x10],
        "the cleared ground, blue first"
    );
    assert_eq!(at(10, 10), [0x00, 0x80, 0xff], "the fill");
    let _ = gpu.backend().release(output.id);
}
